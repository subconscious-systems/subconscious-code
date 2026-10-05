//! Connecting to MCP servers and tracking their health.
//!
//! Every server connects concurrently before the first model request, so the
//! tool set is fixed for the session and the request prefix stays cacheable. A
//! server that fails to start, times out, or later crashes is recorded with its
//! error; it never fails the session, and its tools answer with that error.

use crate::config::{ServerConfig, Transport};
use crate::tool::{tool_wire_name, McpTool};
use rc_core::tool::Tool;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::ServiceExt;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Lines of a stdio server's stderr kept for `/mcp` and error messages.
const STDERR_TAIL_LINES: usize = 20;

/// Where one server stands. `tools` lists the wire names it contributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Connected {
        tools: Vec<String>,
    },
    Failed {
        error: String,
    },
    /// Connected at startup, then the connection closed (the server exited).
    Closed {
        tools: Vec<String>,
    },
}

/// A snapshot for `/mcp` and `marathon doctor`.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub transport: String,
    pub state: ServerState,
    pub stderr_tail: Vec<String>,
}

type Service = RunningService<RoleClient, ClientConfig>;

/// One configured server. Shared by its tools so a call can see whether the
/// server is still alive and record when it is not.
pub(crate) struct ServerHandle {
    pub(crate) name: String,
    transport: String,
    pub(crate) tool_timeout: std::time::Duration,
    state: Mutex<ServerState>,
    /// Set once a call sees the transport close. rmcp reports the closed
    /// transport to the failing call before `is_closed` turns true, so the
    /// call records it here for every later call.
    closed: AtomicBool,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Kept alive for the session: dropping it closes the connection and, for
    /// stdio, kills the child process.
    service: Mutex<Option<Service>>,
}

impl ServerHandle {
    pub(crate) fn peer(&self) -> Option<Peer<RoleClient>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let service = self.service.lock().unwrap_or_else(|e| e.into_inner());
        service
            .as_ref()
            .filter(|s| !s.is_closed())
            .map(|s| s.peer().clone())
    }

    /// Record that the connection is gone, keeping the tool list for `/mcp`.
    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let ServerState::Connected { tools } = &*state {
            *state = ServerState::Closed {
                tools: tools.clone(),
            };
        }
    }

    /// Why calls to this server cannot run, with the last stderr line if any.
    pub(crate) fn unavailable_reason(&self) -> String {
        let tail = self.stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
        match tail.back() {
            Some(line) => format!(
                "MCP server `{}` is not running (last stderr: {line})",
                self.name
            ),
            None => format!("MCP server `{}` is not running", self.name),
        }
    }

    fn status(&self) -> ServerStatus {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let ServerState::Connected { tools } = &state {
            if self.peer().is_none() {
                state = ServerState::Closed {
                    tools: tools.clone(),
                };
            }
        }
        ServerStatus {
            name: self.name.clone(),
            transport: self.transport.clone(),
            state,
            stderr_tail: self
                .stderr_tail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned()
                .collect(),
        }
    }
}

/// Every configured MCP server for one session.
#[derive(Clone, Default)]
pub struct McpHub {
    servers: Vec<Arc<ServerHandle>>,
    tools: Vec<Arc<dyn Tool>>,
}

impl McpHub {
    /// Connect to every server concurrently and list their tools. `invalid`
    /// carries config entries that failed to parse, so they show in `/mcp`.
    pub async fn connect(servers: Vec<ServerConfig>, invalid: Vec<(String, String)>) -> Self {
        let mut tasks = tokio::task::JoinSet::new();
        for (index, config) in servers.into_iter().enumerate() {
            tasks.spawn(async move { (index, connect_one(config).await) });
        }
        let mut connected = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(pair) => connected.push(pair),
                Err(e) => tracing::warn!(target: "sc.mcp", "MCP connect task failed: {e}"),
            }
        }
        // Registration order is canonicalized by the tool registry; sorting
        // here only keeps `/mcp` and duplicate resolution deterministic.
        connected.sort_by_key(|(index, _)| *index);

        let mut hub = McpHub::default();
        let mut seen_tools = BTreeSet::new();
        for (_, (handle, listed)) in connected {
            let handle = Arc::new(handle);
            let mut kept = Vec::new();
            for tool in listed {
                let wire = tool_wire_name(&handle.name, &tool.name);
                if !seen_tools.insert(wire.clone()) {
                    tracing::warn!(target: "sc.mcp", server = %handle.name, tool = %tool.name,
                        "skipping MCP tool: its name collides with another after sanitizing");
                    continue;
                }
                kept.push(wire.clone());
                hub.tools
                    .push(Arc::new(McpTool::new(wire, tool, handle.clone())) as Arc<dyn Tool>);
            }
            if let ServerState::Connected { tools } =
                &mut *handle.state.lock().unwrap_or_else(|e| e.into_inner())
            {
                *tools = kept;
            }
            hub.servers.push(handle);
        }
        for (name, error) in invalid {
            hub.servers.push(Arc::new(ServerHandle {
                name,
                transport: "invalid config".into(),
                tool_timeout: crate::config::DEFAULT_TOOL_TIMEOUT,
                state: Mutex::new(ServerState::Failed { error }),
                closed: AtomicBool::new(true),
                stderr_tail: Arc::default(),
                service: Mutex::new(None),
            }));
        }
        hub.servers.sort_by(|a, b| a.name.cmp(&b.name));
        hub
    }

    /// The tools to add to the session's registry.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn status(&self) -> Vec<ServerStatus> {
        self.servers.iter().map(|s| s.status()).collect()
    }

    /// `/mcp` output: one block per server.
    pub fn report_lines(&self) -> Vec<String> {
        if self.servers.is_empty() {
            return vec![
                "  no MCP servers configured".into(),
                "  add an `mcpServers` block to .sc/settings.json or pass --mcp-config".into(),
            ];
        }
        let mut lines = Vec::new();
        for status in self.status() {
            let (label, tools, error) = match &status.state {
                ServerState::Connected { tools } => ("connected", tools.as_slice(), None),
                ServerState::Closed { tools } => ("exited", tools.as_slice(), None),
                ServerState::Failed { error } => ("failed", &[][..], Some(error.as_str())),
            };
            lines.push(format!(
                "  {}  {label}  ({})",
                status.name, status.transport
            ));
            if let Some(error) = error {
                lines.push(format!("      error: {error}"));
            }
            if !tools.is_empty() {
                lines.push(format!("      tools: {}", tools.join(", ")));
            }
            if label != "connected" {
                if let Some(line) = status.stderr_tail.last() {
                    lines.push(format!("      stderr: {line}"));
                }
            }
        }
        lines
    }

    /// One line per server that is not connected, for printing at startup.
    pub fn problems(&self) -> Vec<String> {
        self.status()
            .into_iter()
            .filter_map(|s| match s.state {
                ServerState::Failed { error } => {
                    Some(format!("MCP server `{}` failed: {error}", s.name))
                }
                ServerState::Closed { .. } => Some(format!("MCP server `{}` exited", s.name)),
                ServerState::Connected { .. } => None,
            })
            .collect()
    }
}

fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("marathon", env!("CARGO_PKG_VERSION")),
    )
}

async fn connect_one(config: ServerConfig) -> (ServerHandle, Vec<rmcp::model::Tool>) {
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let transport_label = config.transport.describe();
    let attempt = tokio::time::timeout(
        config.startup_timeout,
        start(&config.name, &config.transport, stderr_tail.clone()),
    )
    .await;
    let outcome = match attempt {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!(
            "did not start within {} ms",
            config.startup_timeout.as_millis()
        )),
    };
    let (state, service, tools) = match outcome {
        Ok((service, tools)) => (
            ServerState::Connected { tools: Vec::new() },
            Some(service),
            tools,
        ),
        Err(error) => {
            let error = match stderr_tail.lock().unwrap_or_else(|e| e.into_inner()).back() {
                Some(line) => format!("{error} (stderr: {line})"),
                None => error,
            };
            tracing::warn!(target: "sc.mcp", server = %config.name, "{error}");
            (ServerState::Failed { error }, None, Vec::new())
        }
    };
    let handle = ServerHandle {
        name: config.name,
        transport: transport_label,
        tool_timeout: config.tool_timeout,
        state: Mutex::new(state),
        closed: AtomicBool::new(false),
        stderr_tail,
        service: Mutex::new(service),
    };
    (handle, tools)
}

async fn start(
    name: &str,
    transport: &Transport,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
) -> Result<(Service, Vec<rmcp::model::Tool>), String> {
    let service = match transport {
        Transport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let program = resolve_program(command);
            let command = tokio::process::Command::new(program).configure(|cmd| {
                cmd.args(args).envs(env);
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
            });
            // stderr must never reach the terminal: it would corrupt the TUI.
            let (child, stderr) = TokioChildProcess::builder(command)
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("could not start `{}`: {e}", transport_command(transport)))?;
            if let Some(stderr) = stderr {
                drain_stderr(name.to_string(), stderr, stderr_tail);
            }
            client_config()
                .serve(child)
                .await
                .map_err(|e| format!("initialize failed: {e}"))?
        }
        Transport::Http { url, headers } => {
            let mut custom = std::collections::HashMap::new();
            for (key, value) in headers {
                let key = http::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| format!("bad header name {key:?}: {e}"))?;
                let value = http::HeaderValue::from_str(value)
                    .map_err(|e| format!("bad value for header {key}: {e}"))?;
                custom.insert(key, value);
            }
            let config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .custom_headers(custom);
            client_config()
                .serve(StreamableHttpClientTransport::from_config(config))
                .await
                .map_err(|e| format!("initialize failed: {e}"))?
        }
    };
    let tools = service
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| format!("listing tools failed: {e}"))?;
    Ok((service, tools))
}

fn transport_command(transport: &Transport) -> &str {
    match transport {
        Transport::Stdio { command, .. } => command,
        Transport::Http { url, .. } => url,
    }
}

/// Resolve a bare command on `PATH` the way a shell would. On Windows this is
/// what finds `npx.cmd` for `npx`; elsewhere it is a no-op for most commands.
fn resolve_program(command: &str) -> std::ffi::OsString {
    which::which(command)
        .map(|path| path.into_os_string())
        .unwrap_or_else(|_| command.into())
}

fn drain_stderr(
    name: String,
    stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "sc.mcp", server = %name, "stderr: {line}");
            let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });
}
