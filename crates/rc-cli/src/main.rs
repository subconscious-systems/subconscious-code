//! `rc` — a terminal agent harness speaking OpenAI-compatible chat completions.
//!
//! M1: headless agent loop. `rc -p "what's in <file>"` drives the streaming
//! loop with the `Read` tool registered, executes the tool call, and prints
//! the model's final answer. The TUI, more tools, and permissions arrive in
//! later milestones.

use anyhow::{Context, Result};
use clap::Parser;
use rc_config::Settings;
use rc_core::agent::{AgentLoop, LoopOutcome};
use rc_core::model::{ChatModel, Model, NullSink};
use rc_core::registry::ToolRegistry;
use rc_core::tool::Tool;
use rc_core::turn::{Session, Turn};
use rc_core::{
    AskResponse, BypassChecker, ContextAssembler, Mode, NullPrompter, PermissionChecker,
    PermissionEngine, Prompter,
};
use rc_ctx::{ContextAssembler as CtxAssembler, Environment};
use rc_proto::{ChatClient, RetryOpts};
use rc_tools::{Bash, Edit, Glob, Grep, Read, Write};
use std::io::{IsTerminal, Write as IoWrite};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    name = "rc",
    version,
    about = "A Claude Code–style agent harness (chat completions backend).",
    long_about = "M4: a headless one-shot (`rc -p \"<prompt>\"`) or the interactive \
                  TUI (just `rc`). Either way it speaks an OpenAI-compatible chat \
                  completions backend."
)]
struct Cli {
    /// One-shot headless mode: run the agent loop for PROMPT and print the answer (§5.8 U14).
    #[arg(short, long, value_name = "PROMPT")]
    print: Option<String>,

    /// Override the model for this invocation (§5.6 A9).
    #[arg(long, env = "RC_MODEL")]
    model: Option<String>,

    /// Override the base URL (§5.6 G3).
    #[arg(long, env = "RC_BASE_URL")]
    base_url: Option<String>,

    /// Dump request/response and emit tracing to stderr (§5.9 O5).
    #[arg(long)]
    debug: bool,

    /// Skip all permission prompts (still hard-denies catastrophic commands).
    /// Refused in CI without RC_DANGEROUS=1.
    #[arg(long = "dangerously-skip-permissions")]
    dangerously_skip_permissions: bool,

    /// Resume the most recent session (the newest `~/.rc/sessions/*.jsonl`).
    /// Replays its turns into the conversation and continues it.
    #[arg(long, conflicts_with = "resume")]
    continue_last: bool,

    /// Resume a specific session file (an absolute path to a `*.jsonl`).
    #[arg(long, value_name = "PATH")]
    resume: Option<PathBuf>,

    /// Per-response completion-token cap (overrides RC_MAX_TOKENS; 0 = provider default).
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Sampling temperature (overrides RC_TEMPERATURE; e.g. 0 for reproducible runs).
    #[arg(long, value_name = "T")]
    temperature: Option<f32>,

    /// M7: confine every approved `Bash` command at the kernel level (Linux:
    /// Landlock + seccomp). Denies writes outside the workspace roots and
    /// network. Off by default; also settable via RC_SANDBOX=1.
    #[arg(long)]
    sandbox: bool,

    /// M7: allow network under `--sandbox` (otherwise denied). Also via
    /// RC_SANDBOX_NET=1. Implies --sandbox.
    #[arg(long = "sandbox-net")]
    sandbox_net: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.debug {
        init_tracing();
    }

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let mut settings = Settings::load(&std::env::current_dir()?);
    if let Some(m) = cli.model {
        settings.model = m;
    }
    if let Some(u) = cli.base_url {
        settings.base_url = u;
    }

    tracing::debug!(model = %settings.model, base_url = %settings.base_url, "settings loaded");

    let api_key = settings
        .api_key
        .clone()
        .context("no API key: set $RC_API_KEY (or the var named by provider.api_key_env)")?;

    // T1: request timeout is configurable (was hardcoded 600s). Clamp 0 → default
    // to avoid a Duration::ZERO footgun. This is a *total* timeout; on the
    // streaming path it caps the whole stream, so a small value can cut a stream
    // mid-tool-call (the loop synthesizes Interrupted results — §4.2).
    let timeout = Duration::from_millis(if settings.timeout_ms == 0 {
        600_000
    } else {
        settings.timeout_ms
    });
    // T2: idle bound on the model stream (0 = off). A stall (no chunk for this
    // long) aborts with ProtoError::Idle instead of waiting out the total timeout.
    let idle = if settings.idle_timeout_ms == 0 {
        None
    } else {
        Some(Duration::from_millis(settings.idle_timeout_ms))
    };
    let retry = RetryOpts {
        max_retries: settings.max_retries,
        base_delay: Duration::from_millis(settings.retry_base_ms),
        max_delay: Duration::from_millis(settings.retry_max_ms),
    };
    // T3: wall-clock budget for a turn (0 = off). Checked at the top of each
    // loop iteration; on expiry the turn ends with LoopOutcome::TimeUp.
    let turn_timeout = if settings.turn_timeout_ms == 0 {
        None
    } else {
        Some(Duration::from_millis(settings.turn_timeout_ms))
    };
    let max_tokens = cli
        .max_tokens
        .or_else(|| (settings.max_tokens > 0).then_some(settings.max_tokens))
        .filter(|&n| n > 0);
    let temperature = cli.temperature.or(settings.temperature);
    // M7: opt-in kernel sandbox for Bash (§7.6). CLI flags force-on; env
    // (RC_SANDBOX / RC_SANDBOX_NET) and the settings file are the base layer.
    let mut sandbox_enabled = settings.sandbox.enabled;
    let mut sandbox_allow_net = settings.sandbox.allow_net;
    if cli.sandbox_net {
        sandbox_enabled = true;
        sandbox_allow_net = true;
    }
    if cli.sandbox {
        sandbox_enabled = true;
    }
    let sandbox = sandbox_enabled.then_some(rc_core::tool::SandboxPolicy {
        allow_net: sandbox_allow_net,
    });
    let client = Arc::new(
        ChatClient::new(settings.base_url.clone(), api_key, settings.model.clone(), timeout)?
            .with_retry(retry),
    );
    let model = Arc::new(ChatModel::new(client)) as Arc<dyn Model>;
    let tools = Arc::new(ToolRegistry::new(vec![
        Arc::new(Read::new()) as Arc<dyn Tool>,
        Arc::new(Write::new()) as Arc<dyn Tool>,
        Arc::new(Edit::new()) as Arc<dyn Tool>,
        Arc::new(Glob::new()) as Arc<dyn Tool>,
        Arc::new(Grep::new()) as Arc<dyn Tool>,
        Arc::new(Bash::new()) as Arc<dyn Tool>,
    ]));
    // Permission engine (§7): bypass, or the real engine from the settings block.
    let permission: Arc<dyn PermissionChecker> = if cli.dangerously_skip_permissions {
        if std::env::var("CI").as_deref() == Ok("true")
            && std::env::var("RC_DANGEROUS").as_deref() != Ok("1")
        {
            anyhow::bail!("refusing --dangerously-skip-permissions in CI (set RC_DANGEROUS=1 to override)");
        }
        Arc::new(BypassChecker)
    } else {
        let mode = Mode::parse(&settings.permissions.default_mode);
        Arc::new(PermissionEngine::new(
            mode,
            settings.permissions.deny.clone(),
            settings.permissions.allow.clone(),
            settings.permissions.ask.clone(),
        ))
    };

    let extra_dirs: Vec<PathBuf> = settings
        .permissions
        .additional_directories
        .iter()
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .collect();

    // Resume an existing session (--continue picks the newest; --resume takes a
    // path). Only the TUI path persists; a headless `-p` run is ephemeral.
    let sessions_dir = sessions_dir()?;
    let resumed = if let Some(path) = cli.resume.clone() {
        Some(rc_session::load(&path).context("--resume: could not load session")?)
    } else if cli.continue_last {
        match rc_session::latest(&sessions_dir) {
            Some(path) => Some(
                rc_session::load(&path)
                    .context("--continue: could not load the latest session")?,
            ),
            None => {
                anyhow::bail!("--continue: no prior session found in {}", sessions_dir.display());
            }
        }
    } else {
        None
    };
    let mut session = match resumed {
        Some(s) => s,
        None => {
            // A fresh id (timestamp + pid) so each run gets its own session file
            // and `--continue` can find the newest one unambiguously.
            let id = format!("session-{}-{}", chrono_like_ts(), std::process::id());
            let cwd = std::env::current_dir()?;
            Session::new(id, cwd, settings.model.clone())
        }
    };
    session.extra_dirs = extra_dirs;

    // M7: a per-session directory for background-shell logs (`run_in_background`
    // Bash), so `Read <log>` can find them. Background children are killed on
    // shutdown via `ShellState::shutdown` when the driver exits.
    {
        let bg_dir = sessions_dir.join("..").join("bg").join(&session.id);
        if let Ok(mut s) = session.shell_state.lock() {
            s.bg_dir = Some(bg_dir);
        }
    }

    let agent = AgentLoop::new(model, tools, permission)
        .with_assembler(build_assembler(&session))
        .with_idle_timeout(idle)
        .with_turn_timeout(turn_timeout)
        .with_max_tokens(max_tokens)
        .with_temperature(temperature)
        .with_sandbox(sandbox);
    match cli.print {
        // Headless one-shot: run one turn and print the answer (§5.8 U14).
        Some(prompt) if !prompt.is_empty() => {
            run_headless(agent, session, prompt).await
        }
        // Otherwise: launch the interactive TUI (M4) with persistence (M5).
        _ => run_tui(Arc::new(agent), session, settings.model, sessions_dir).await,
    }
}

/// Build the §4.6 context assembler for a session: the environment block (cwd,
/// platform, date, git branch) plus the hierarchical memory files rooted at the
/// session cwd. Wired into the `AgentLoop` so the model gets a real system
/// prompt and `@file` mention expansion (M6).
fn build_assembler(session: &Session) -> Arc<dyn ContextAssembler> {
    let env = Environment::from_cwd(&session.cwd);
    let assembler = CtxAssembler::new(env);
    Arc::new(assembler) as Arc<dyn ContextAssembler>
}

/// `~/.rc/sessions/` — where session JSONL files live. Created on first use.
fn sessions_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; cannot locate ~/.rc/sessions")?;
    Ok(PathBuf::from(home).join(".rc").join("sessions"))
}

/// A compact, sortable UTC timestamp (`YYYYmmddTHHMMSS`) for fresh session ids,
/// without pulling in chrono — `--continue` picks the newest file by mtime, so
/// monotonicity is what matters, not the format.
fn chrono_like_ts() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs = now % 60;
    let mins = (now / 60) % 60;
    let hours = (now / 3600) % 24;
    let days = now / 86400;
    // Days since 1970-01-01 -> a proleptic Gregorian Y-M-D (no leap-second fuss).
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}{m:02}{d:02}T{hours:02}{mins:02}{secs:02}")
}

/// Howard Hinnant's days-from-civil inverse: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// Headless `-p`: one turn, then print the final assistant text. An interactive
/// stdin prompter is used only on a TTY; non-interactive runs deny on Ask (fail
/// closed). `--dangerously-skip-permissions` uses `BypassChecker`, which never
/// asks, so the prompter is moot in bypass mode.
async fn run_headless(
    agent: AgentLoop,
    mut session: Session,
    prompt: String,
) -> Result<()> {
    let prompter: Box<dyn Prompter> = if std::io::stdin().is_terminal() {
        Box::new(StdinPrompter)
    } else {
        Box::new(NullPrompter)
    };
    let outcome = agent
        .run(&mut session, prompt, &NullSink, &*prompter, CancellationToken::new())
        .await
        .context("agent loop failed")?;
    print_result(&session, outcome);
    Ok(())
}

/// Interactive TUI: wire the agent loop into the rc-rt runtime and hand it to
/// rc-tui. The TUI is a blocking poll loop, so it runs on a `spawn_blocking`
/// thread — the rc-rt driver/pump keep running on this runtime's worker pool.
///
/// A `SessionStore` is created (or, for a resumed session, re-opened) under
/// `sessions_dir` so the conversation persists across restarts (§9, M5).
async fn run_tui(
    agent: Arc<AgentLoop>,
    session: Session,
    model_name: String,
    sessions_dir: PathBuf,
) -> Result<()> {
    let cwd = session.cwd.clone();

    // Persistence: a fresh session gets a new timestamped file; a resumed
    // session re-opens its existing file in append mode (the header and old
    // turns are already on disk — no rewrite, no risk of losing history).
    let session_id = session.id.clone();
    let path = sessions_dir.join(format!("{session_id}.jsonl"));
    let store = if session.messages.is_empty() {
        Some(rc_session::SessionStore::create(path, &session)?)
    } else {
        // Resumed: the file already holds the header + prior turns. Open it for
        // append so the driver's new turns land after the old ones.
        Some(rc_session::SessionStore::open_append(path)?)
    };

    let runtime = rc_rt::Runtime::new(agent, session, store);
    match tokio::task::spawn_blocking(move || rc_tui::run(runtime, model_name, cwd)).await {
        Ok(inner) => inner,
        Err(join_err) => Err(anyhow::anyhow!("TUI task failed: {join_err}")),
    }
}

/// Print the model's final assistant text. The last non-empty assistant turn is
/// the answer (a tool-calling turn leaves an earlier empty-content assistant).
fn print_result(session: &Session, outcome: LoopOutcome) {
    let text = session.messages.iter().rev().find_map(|t| match t {
        Turn::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
        _ => None,
    });
    match text {
        Some(text) => println!("{text}"),
        None => match outcome {
            LoopOutcome::ItersExceeded => {
                eprintln!("warning: iteration budget reached before the model finished")
            }
            LoopOutcome::TimeUp => {
                eprintln!("warning: turn timed out before the model finished")
            }
            _ => eprintln!("(the model produced no answer text)"),
        },
    }
    // Metering (M3): cumulative session usage to stderr (stdout holds the answer
    // for piping). total_tokens is an upper bound (each turn re-sends the
    // prefix); completion_tokens is the true cumulative output.
    let u = &session.total_usage;
    if u.total_tokens > 0 {
        eprintln!(
            "tokens: {} total · {} completion · {} cached",
            u.total_tokens,
            u.completion_tokens,
            u.cached_tokens().unwrap_or(0),
        );
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    // `rc=debug` covers the `rc` bin and `rc::…`; the `rc_*` crates are separate
    // top-level targets and need their own directives, so the --debug request/
    // response logs (rc_proto::client, rc_core::model) AND the previously-silent
    // rc_core::agent error/warn logs become visible. `RUST_LOG` still wins.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("rc=debug,rc_cli=debug,rc_proto=debug,rc_core=debug,rc_rt=debug")
    });
    let _ = fmt::Subscriber::builder()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
}

/// A crude stdin prompter (§7.4): `y`=once, `s`=session, `a`=always, `n`=deny.
/// The blocking `read_line` is fine inside the async fn: this prompter only runs
/// on a TTY under the multi-thread runtime, so one blocked worker is acceptable.
struct StdinPrompter;
#[async_trait::async_trait]
impl Prompter for StdinPrompter {
    async fn ask(&self, tool: &str, input: &Value, reason: &str) -> AskResponse {
        let suggested = suggested_rule(tool, input);
        eprintln!("━ {tool} requires permission: {reason}");
        eprintln!("  granting rule: {suggested}");
        eprint!("  [y]es once / [s]ession / [a]lways / [n]o: ");
        let _ = std::io::stderr().flush();  // IoWrite in scope
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return AskResponse::Deny("could not read a response".into());
        }
        match line.trim().chars().next() {
            Some('y') | Some('Y') => AskResponse::Once,
            Some('s') | Some('S') => AskResponse::Session(suggested),
            Some('a') | Some('A') => AskResponse::Always(suggested),
            _ => AskResponse::Deny("declined".into()),
        }
    }
}

/// A rough "don't ask again for this" rule: `Bash(<first-token>:*)` for Bash,
/// the bare tool name for everything else (grants the whole tool for the session).
fn suggested_rule(tool: &str, input: &Value) -> String {
    if tool == "Bash" {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            let first = cmd.split_whitespace().next().unwrap_or("");
            if !first.is_empty() {
                return format!("Bash({first}:*)");
            }
        }
    }
    tool.to_string()
}
