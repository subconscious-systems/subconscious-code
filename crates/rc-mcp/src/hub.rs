//! Connecting to MCP servers, tracking their health, and stopping them.
//!
//! Every server connects concurrently before the first model request, so the
//! tool set is fixed for the session and the request prefix stays cacheable. A
//! server that fails to start, times out, or later crashes is recorded with its
//! error; it never fails the session, and its tools answer with that error.
//! Messages shown to the user use the config as written and are scrubbed of
//! every `${VAR}` value, so a substituted credential is never displayed.

use crate::config::{redact, ServerConfig, Transport};
use crate::process::{self, ProcessTree};
use crate::tool::{tool_wire_name, McpTool};
use rc_core::tool::Tool;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Lines of a stdio server's stderr kept for `/mcp` and error messages.
const STDERR_TAIL_LINES: usize = 20;
/// After a failed start, how long to wait for the server's last stderr lines.
/// A server that exits at once usually explains why on stderr, and the
/// closed pipe can reach the client before those lines are read.
const STDERR_SETTLE: Duration = Duration::from_secs(1);
/// How long shutdown waits for a server to close cleanly before killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
    /// Configured but deliberately not started, for example an untrusted
    /// project server.
    Skipped {
        reason: String,
    },
}

/// A snapshot for `/mcp`.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub transport: String,
    pub state: ServerState,
    pub warnings: Vec<String>,
    pub stderr_tail: Vec<String>,
}

type Service = RunningService<RoleClient, ClientConfig>;

/// One configured server. Shared by its tools so a call can see whether the
/// server is still alive and record when it is not.
pub(crate) struct ServerHandle {
    pub(crate) name: String,
    transport: String,
    pub(crate) tool_timeout: Duration,
    secrets: Vec<String>,
    state: Mutex<ServerState>,
    warnings: Mutex<Vec<String>>,
    /// Set once a call sees the transport close. rmcp reports the closed
    /// transport to the failing call before `is_closed` turns true, so the
    /// call records it here for every later call.
    closed: AtomicBool,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    service: tokio::sync::Mutex<Option<Service>>,
    /// The stdio server's process tree. Dropping it kills the tree, so the
    /// server is gone even if the session ends without calling `shutdown`.
    process: Mutex<Option<(tokio::process::Child, ProcessTree)>>,
    /// A clone of the live peer, readable without awaiting the service lock.
    peer: Mutex<Option<Peer<RoleClient>>>,
}

impl ServerHandle {
    fn new(name: String, transport: String, tool_timeout: Duration, state: ServerState) -> Self {
        Self {
            name,
            transport,
            tool_timeout,
            secrets: Vec::new(),
            state: Mutex::new(state),
            warnings: Mutex::default(),
            closed: AtomicBool::new(false),
            stderr_tail: Arc::default(),
            service: tokio::sync::Mutex::new(None),
            process: Mutex::new(None),
            peer: Mutex::new(None),
        }
    }

    pub(crate) fn peer(&self) -> Option<Peer<RoleClient>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        lock(&self.peer).clone()
    }

    /// Record that the connection is gone, keeping the tool list for `/mcp`.
    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        let mut state = lock(&self.state);
        if let ServerState::Connected { tools } = &*state {
            *state = ServerState::Closed {
                tools: tools.clone(),
            };
        }
    }

    /// Scrub every substituted `${VAR}` value from a message.
    pub(crate) fn scrub(&self, text: &str) -> String {
        redact(text, &self.secrets)
    }

    /// Why calls to this server cannot run, with the last stderr line if any.
    pub(crate) fn unavailable_reason(&self) -> String {
        match lock(&self.stderr_tail).back() {
            Some(line) => format!(
                "MCP server `{}` is not running (last stderr: {line})",
                self.name
            ),
            None => format!("MCP server `{}` is not running", self.name),
        }
    }

    fn status(&self) -> ServerStatus {
        let mut state = lock(&self.state).clone();
        if let ServerState::Connected { tools } = &state {
            if self.closed.load(Ordering::Acquire) {
                state = ServerState::Closed {
                    tools: tools.clone(),
                };
            }
        }
        ServerStatus {
            name: self.name.clone(),
            transport: self.transport.clone(),
            state,
            warnings: lock(&self.warnings).clone(),
            stderr_tail: lock(&self.stderr_tail).iter().cloned().collect(),
        }
    }

    /// Close the connection, then kill the process tree and reap the child.
    async fn shutdown(&self) {
        *lock(&self.peer) = None;
        self.closed.store(true, Ordering::Release);
        if let Some(service) = self.service.lock().await.take() {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, service.cancel()).await;
        }
        let process = lock(&self.process).take();
        if let Some((mut child, mut tree)) = process {
            if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
                .await
                .is_err()
            {
                let _ = child.start_kill();
            }
            // The direct child may have exited while its own children (an
            // `npx` launcher's node process) live on in the group or job.
            tree.kill();
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every configured MCP server for one session.
#[derive(Clone, Default)]
pub struct McpHub {
    servers: Vec<Arc<ServerHandle>>,
    tools: Vec<Arc<dyn Tool>>,
}

/// Servers that were configured but not started, with the reason.
pub type Skipped = Vec<(String, String)>;

impl McpHub {
    /// Connect to every server concurrently and list their tools. `invalid`
    /// carries entries that failed to parse and `skipped` entries that were
    /// not started on purpose, so both show in `/mcp`.
    pub async fn connect(
        servers: Vec<ServerConfig>,
        invalid: Vec<(String, String)>,
        skipped: Skipped,
    ) -> Self {
        let mut tasks = tokio::task::JoinSet::new();
        let mut pending = std::collections::HashMap::new();
        for (index, config) in servers.into_iter().enumerate() {
            let label = (
                config.name.clone(),
                config.display.clone(),
                config.tool_timeout,
            );
            let id = tasks
                .spawn(async move { (index, connect_one(config).await) })
                .id();
            pending.insert(id, (index, label));
        }
        let mut connected = Vec::new();
        while let Some(joined) = tasks.join_next_with_id().await {
            match joined {
                Ok((_, pair)) => connected.push(pair),
                // A panic inside a transport must still leave the server
                // visible in `/mcp`, not silently drop it.
                Err(error) => {
                    if let Some((index, (name, display, tool_timeout))) =
                        pending.remove(&error.id())
                    {
                        tracing::warn!(target: "sc.mcp", server = %name, "connect task failed: {error}");
                        let handle = ServerHandle::new(
                            name,
                            display,
                            tool_timeout,
                            ServerState::Failed {
                                error: "connecting failed unexpectedly (internal error)".into(),
                            },
                        );
                        connected.push((index, (handle, Vec::new())));
                    }
                }
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
                    lock(&handle.warnings).push(format!(
                        "skipped tool `{}`: its name collides with another tool after sanitizing",
                        tool.name
                    ));
                    continue;
                }
                if wire != format!("mcp__{}__{}", handle.name, tool.name) {
                    lock(&handle.warnings)
                        .push(format!("tool `{}` is offered as `{wire}`", tool.name));
                }
                kept.push(wire.clone());
                hub.tools
                    .push(Arc::new(McpTool::new(wire, tool, handle.clone())) as Arc<dyn Tool>);
            }
            if let ServerState::Connected { tools } = &mut *lock(&handle.state) {
                *tools = kept;
            }
            hub.servers.push(handle);
        }
        for (name, error) in invalid {
            hub.servers.push(Arc::new(ServerHandle::new(
                name,
                "invalid config".into(),
                crate::config::DEFAULT_TOOL_TIMEOUT,
                ServerState::Failed { error },
            )));
        }
        for (name, reason) in skipped {
            hub.servers.push(Arc::new(ServerHandle::new(
                name,
                "not started".into(),
                crate::config::DEFAULT_TOOL_TIMEOUT,
                ServerState::Skipped { reason },
            )));
        }
        hub.servers.sort_by(|a, b| a.name.cmp(&b.name));
        hub
    }

    /// Stop every server: close each connection, then kill its process tree.
    /// Call it before the session exits; dropping the hub also kills the
    /// trees, but without the graceful close.
    pub async fn shutdown(&self) {
        let mut tasks = tokio::task::JoinSet::new();
        for server in &self.servers {
            let server = server.clone();
            tasks.spawn(async move { server.shutdown().await });
        }
        while tasks.join_next().await.is_some() {}
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
                "  add an `mcpServers` block to ~/.sc/settings.json or pass --mcp-config".into(),
            ];
        }
        let mut lines = Vec::new();
        for status in self.status() {
            let (label, tools, detail) = match &status.state {
                ServerState::Connected { tools } => ("connected", tools.as_slice(), None),
                ServerState::Closed { tools } => ("exited", tools.as_slice(), None),
                ServerState::Failed { error } => ("failed", &[][..], Some(error.as_str())),
                ServerState::Skipped { reason } => ("not started", &[][..], Some(reason.as_str())),
            };
            lines.push(format!(
                "  {}  {label}  ({})",
                status.name, status.transport
            ));
            if let Some(detail) = detail {
                lines.push(format!("      {detail}"));
            }
            if !tools.is_empty() {
                lines.push(format!("      tools: {}", tools.join(", ")));
            }
            for warning in &status.warnings {
                lines.push(format!("      warning: {warning}"));
            }
            if label != "connected" {
                if let Some(line) = status.stderr_tail.last() {
                    lines.push(format!("      stderr: {line}"));
                }
            }
        }
        lines
    }

    /// One line per server needing attention, for printing at startup.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for status in self.status() {
            match status.state {
                ServerState::Failed { error } => {
                    problems.push(format!("MCP server `{}` failed: {error}", status.name))
                }
                ServerState::Closed { .. } => {
                    problems.push(format!("MCP server `{}` exited", status.name))
                }
                ServerState::Skipped { reason } => problems.push(format!(
                    "MCP server `{}` not started: {reason}",
                    status.name
                )),
                ServerState::Connected { .. } => {}
            }
            for warning in status.warnings {
                problems.push(format!("MCP server `{}`: {warning}", status.name));
            }
        }
        problems
    }
}

fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("marathon", env!("CARGO_PKG_VERSION")),
    )
}

async fn connect_one(config: ServerConfig) -> (ServerHandle, Vec<rmcp::model::Tool>) {
    let mut handle = ServerHandle::new(
        config.name.clone(),
        config.display.clone(),
        config.tool_timeout,
        ServerState::Connected { tools: Vec::new() },
    );
    handle.secrets = config.secrets.clone();
    let stderr_reader: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
    let attempt = tokio::time::timeout(
        config.startup_timeout,
        start(&config, &handle, &stderr_reader),
    )
    .await;
    let outcome = match attempt {
        Ok(result) => result,
        Err(_) => Err(format!(
            "did not start within {} ms",
            config.startup_timeout.as_millis()
        )),
    };
    match outcome {
        Ok((service, tools)) => {
            *lock(&handle.peer) = Some(service.peer().clone());
            *handle.service.get_mut() = Some(service);
            (handle, tools)
        }
        Err(error) => {
            let reader = lock(&stderr_reader).take();
            if let Some(reader) = reader {
                let _ = tokio::time::timeout(STDERR_SETTLE, reader).await;
            }
            let error = match lock(&handle.stderr_tail).back() {
                Some(line) => format!("{error} (stderr: {line})"),
                None => error,
            };
            let error = handle.scrub(&error);
            tracing::warn!(target: "sc.mcp", server = %config.name, "{error}");
            *lock(&handle.state) = ServerState::Failed { error };
            handle.shutdown().await;
            (handle, Vec::new())
        }
    }
}

async fn start(
    config: &ServerConfig,
    handle: &ServerHandle,
    stderr_reader: &Mutex<Option<tokio::task::JoinHandle<()>>>,
) -> Result<(Service, Vec<rmcp::model::Tool>), String> {
    let service = match &config.transport {
        Transport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let env = process::server_env(std::env::vars_os(), env);
            let program = resolve_program(command, env.get(std::ffi::OsStr::new("PATH")));
            let spawned = process::spawn(program, args, env, cwd.as_deref())
                .map_err(|e| format!("could not start `{}`: {e}", config.display))?;
            if let Some(stderr) = spawned.stderr {
                let reader = drain_stderr(
                    config.name.clone(),
                    stderr,
                    handle.stderr_tail.clone(),
                    config.secrets.clone(),
                );
                *lock(stderr_reader) = Some(reader);
            }
            *lock(&handle.process) = Some((spawned.child, spawned.tree));
            client_config()
                .serve((spawned.stdout, spawned.stdin))
                .await
                .map_err(|e| format!("initialize failed: {e}"))?
        }
        Transport::Http { url, headers } => {
            install_crypto_provider();
            let mut custom = std::collections::HashMap::new();
            for (key, value) in headers {
                let key = http::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| format!("bad header name {key:?}: {e}"))?;
                let value = http::HeaderValue::from_str(value)
                    .map_err(|_| format!("bad value for header {key}"))?;
                custom.insert(key, value);
            }
            let transport_config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.as_str(),
                )
                .custom_headers(custom);
            client_config()
                .serve(StreamableHttpClientTransport::from_config(transport_config))
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

/// reqwest panics when it builds a TLS client with no process-wide rustls
/// provider. Another part of the process may already have installed one,
/// which is fine.
fn install_crypto_provider() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Resolve a bare command on the server's `PATH` the way a shell would. On
/// Windows this is what finds `npx.cmd` for `npx`.
fn resolve_program(command: &str, path: Option<&std::ffi::OsString>) -> std::ffi::OsString {
    let cwd = std::env::current_dir().unwrap_or_default();
    which::which_in(command, path, cwd)
        .map(|path| path.into_os_string())
        .unwrap_or_else(|_| command.into())
}

fn drain_stderr(
    name: String,
    stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
    secrets: Vec<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = redact(&line, &secrets);
            tracing::debug!(target: "sc.mcp", server = %name, "stderr: {line}");
            let mut tail = lock(&tail);
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    })
}
