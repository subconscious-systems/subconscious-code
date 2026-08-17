//! `sc` — Subconscious Code: a terminal coding agent speaking OpenAI-compatible
//! chat completions.
//!
//! Two entry points: a headless one-shot (`sc -p "<prompt>"`) and the
//! interactive TUI (bare `sc`). Both drive the same agent loop over the same
//! tools, permission engine, and context assembler.
//!
//! The defining choice: **no context-window limit and no request-size cap.**
//! Every per-item truncation cap is configurable and ships at `0` (unlimited);
//! the request body is serialized exactly once into refcounted `Bytes`, so a
//! retry costs a refcount rather than a re-copy; and the total request timeout
//! is off by default, so a large upload isn't mistaken for a hung one.
//!
//! Known cost, measured: peak RSS runs ~6× the payload (12 MB body → 86.7 MB
//! peak against a 15.2 MB baseline). That multiple lives in the assembly
//! pipeline's `Turn`/`WireMessage` clones, not the transport. Budget memory
//! accordingly for a very large context until those clones are removed.

mod doctor;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rc_config::Settings;
use rc_core::agent::{AgentLoop, LoopOutcome};
use rc_core::model::{ChatModel, Model};
use rc_core::registry::ToolRegistry;
use rc_core::tool::Tool;
use rc_core::turn::{AgentMode, Session, Turn};
use rc_core::{
    AskResponse, BypassChecker, ContextAssembler, Mode, NullPrompter, PermissionChecker,
    PermissionEngine, Prompter,
};
use rc_ctx::{Caps as CtxCaps, ContextAssembler as CtxAssembler, Environment};
use rc_proto::{ChatClient, RetryOpts};
use rc_tools::{Bash, Edit, Glob, Grep, Read, Write};
use serde_json::Value;
use std::io::{IsTerminal, Write as IoWrite};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    name = "sc",
    version,
    about = "Subconscious Code — a terminal coding agent with no context limit.",
    long_about = "Subconscious Code (`sc`): a headless one-shot (`sc -p \"<prompt>\"`) or \
                  the interactive TUI (just `sc`). Either way it speaks an \
                  OpenAI-compatible chat completions backend, with every context cap \
                  unlimited by default."
)]
struct Cli {
    /// One-shot headless mode: run the agent loop for PROMPT and print the answer (§5.8 U14).
    #[arg(short, long, value_name = "PROMPT")]
    print: Option<String>,

    /// Override the model for this invocation (§5.6 A9).
    #[arg(long, env = "SC_MODEL")]
    model: Option<String>,

    /// Override the base URL (§5.6 G3).
    #[arg(long, env = "SC_BASE_URL")]
    base_url: Option<String>,

    /// Dump request/response and emit tracing to stderr (§5.9 O5).
    #[arg(long)]
    debug: bool,

    /// Skip all permission prompts (still hard-denies catastrophic commands).
    /// Refused in CI without SC_DANGEROUS=1.
    #[arg(long = "dangerously-skip-permissions")]
    dangerously_skip_permissions: bool,

    /// Resume the most recent session (the newest `~/.sc/sessions/*.jsonl` that
    /// has actual history). Replays its turns and continues it.
    ///
    /// Spelled `--continue` (the field can't be, since `continue` is a Rust
    /// keyword); `--continue-last` stays as an alias.
    #[arg(long = "continue", alias = "continue-last", conflicts_with = "resume")]
    continue_last: bool,

    /// Resume a specific session file (an absolute path to a `*.jsonl`).
    #[arg(long, value_name = "PATH")]
    resume: Option<PathBuf>,

    /// Per-response completion-token cap (overrides SC_MAX_TOKENS; 0 = provider default).
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Sampling temperature (overrides SC_TEMPERATURE; e.g. 0 for reproducible runs).
    #[arg(long, value_name = "T")]
    temperature: Option<f32>,

    /// M7: confine every approved `Bash` command at the kernel level (Linux:
    /// Landlock + seccomp). Denies writes outside the workspace roots and
    /// network. Off by default; also settable via SC_SANDBOX=1.
    #[arg(long)]
    sandbox: bool,

    /// M7: allow network under `--sandbox` (otherwise denied). Also via
    /// SC_SANDBOX_NET=1. Implies --sandbox.
    #[arg(long = "sandbox-net")]
    sandbox_net: bool,

    /// Verify the endpoint before trusting it: config, non-streaming, streaming,
    /// and tool-call support. Exits non-zero if a check fails. Run as
    /// `sc doctor` (see [`Command::Doctor`).
    #[command(subcommand)]
    command: Option<Command>,
}

/// Optional subcommands. `None` (bare `sc`, or `sc -p "..."`) runs the agent.
#[derive(Subcommand, Debug)]
enum Command {
    /// Verify the endpoint before trusting it: config, non-streaming, streaming,
    /// and tool-call support. Exits non-zero if a check fails. Runs before the
    /// API-key requirement so it can *report* a missing key as a failed check.
    Doctor {
        /// Also measure the gateway's maximum request size by uploading
        /// 1/10/32/100/500 MB bodies until one is refused. That ceiling —
        /// not this client — is what bounds the context.
        #[arg(long = "body-ladder")]
        body_ladder: bool,
    },
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
    let model_override = cli.model.clone();
    if let Some(m) = model_override.clone() {
        settings.model = m;
    }
    if let Some(u) = cli.base_url {
        settings.base_url = u;
    }

    tracing::debug!(model = %settings.model, base_url = %settings.base_url, "settings loaded");

    // `sc doctor` runs before the API-key requirement so it can *report* a
    // missing key as a failed check instead of erroring out with no diagnostics.
    if let Some(Command::Doctor { body_ladder }) = cli.command {
        let ok = doctor::run(&settings, body_ladder).await?;
        if !ok {
            anyhow::bail!("doctor: one or more checks failed");
        }
        return Ok(());
    }

    let api_key = settings
        .api_key
        .clone()
        .context("no API key: set $SC_API_KEY (or the var named by provider.api_key_env)")?;

    // T1: the *total* request timeout. `0` means off, which is the default —
    // a total budget also covers the upload, so on a huge body it can expire
    // mid-upload and trigger a retry that re-uploads from scratch. Liveness is
    // enforced by the idle bound below instead, which distinguishes a stalled
    // stream from a merely large one. When set, a small value can cut a stream
    // mid-tool-call (the loop synthesizes Interrupted results — §4.2).
    let timeout = (settings.timeout_ms > 0).then(|| Duration::from_millis(settings.timeout_ms));
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
    // (SC_SANDBOX / SC_SANDBOX_NET) and the settings file are the base layer.
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
    // The context caps (§8). Every one ships at 0 = unlimited; a settings file or
    // SC_* env var dials them back in for a small-context model. Each tool is
    // told its own cap so the schema it advertises matches what it enforces.
    let caps = settings.context;
    let tools = Arc::new(ToolRegistry::new(vec![
        Arc::new(Read::with_limits(
            caps.read_default_limit,
            caps.read_max_line_chars,
        )) as Arc<dyn Tool>,
        Arc::new(Write::new()) as Arc<dyn Tool>,
        Arc::new(Edit::new()) as Arc<dyn Tool>,
        Arc::new(Glob::with_cap(caps.glob_cap)) as Arc<dyn Tool>,
        Arc::new(Grep::with_cap(caps.grep_output_cap)) as Arc<dyn Tool>,
        Arc::new(Bash::with_cap(caps.bash_output_cap)) as Arc<dyn Tool>,
    ]));
    // Permission engine (§7): bypass, or the real engine from the settings block.
    let permission: Arc<dyn PermissionChecker> = if cli.dangerously_skip_permissions {
        if std::env::var("CI").as_deref() == Ok("true")
            && std::env::var("SC_DANGEROUS").as_deref() != Ok("1")
        {
            anyhow::bail!(
                "refusing --dangerously-skip-permissions in CI (set SC_DANGEROUS=1 to override)"
            );
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
        let session = rc_session::load(&path).context("--resume: could not load session")?;
        Some((path, session))
    } else if cli.continue_last {
        match rc_session::latest(&sessions_dir) {
            Some(path) => {
                let session = rc_session::load(&path)
                    .context("--continue: could not load the latest session")?;
                Some((path, session))
            }
            None => {
                anyhow::bail!(
                    "--continue: no prior session found in {}",
                    sessions_dir.display()
                );
            }
        }
    } else {
        None
    };
    let was_resumed = resumed.is_some();
    let (mut session, mut session_path) = match resumed {
        Some((path, session)) => (session, Some(path)),
        None => {
            // A random id avoids collisions when `/menu` creates several
            // sessions in one process and becomes the gateway correlation key.
            let id = fresh_session_id();
            let cwd = std::env::current_dir()?;
            (Session::new(id, cwd, settings.model.clone()), None)
        }
    };
    // A saved session normally resumes on its original model. An explicit CLI
    // override is the exception: it is an intentional migration for this run.
    if model_override.is_some() {
        session.model = settings.model.clone();
    }
    prepare_session(&mut session, &sessions_dir, &extra_dirs);
    let mut mode = reconcile_mode(&mut session, &permission, &settings, was_resumed);

    // Build the agent for one session. The model, tools, and permission engine
    // are session-independent (they come from settings) and are shared across
    // rebuilds; the context assembler is not — its environment block and
    // memory files are rooted at the session's cwd — so it is rebuilt here.
    let build_agent = |session: &Session| -> Result<AgentLoop> {
        let client = Arc::new(
            ChatClient::new(
                settings.base_url.clone(),
                api_key.clone(),
                session.model.clone(),
                timeout,
            )?
            .with_retry(retry)
            .with_request_gzip(settings.request_gzip),
        );
        let model = Arc::new(ChatModel::new(client)) as Arc<dyn Model>;
        Ok(AgentLoop::new(model, tools.clone(), permission.clone())
            .with_assembler(build_assembler(session, &caps))
            .with_max_iters(caps.max_iters)
            .with_idle_timeout(idle)
            .with_turn_timeout(turn_timeout)
            .with_max_tokens(max_tokens)
            .with_temperature(temperature)
            .with_sandbox(sandbox))
    };

    // Headless one-shot: run one turn and print the answer (§5.8 U14).
    if let Some(prompt) = cli.print.filter(|p| !p.is_empty()) {
        return run_headless(build_agent(&session)?, session, prompt).await;
    }

    // Interactive TUI (M4) with persistence (M5), in a loop so `/menu` can
    // switch sessions without restarting the process. The TUI returns the
    // session it wants next; everything cwd-scoped is rebuilt around it here,
    // which is precisely why the switch can't happen inside rc-tui.
    loop {
        let model_name = session.model.clone();
        let next = run_tui(
            Arc::new(build_agent(&session)?),
            session,
            model_name,
            sessions_dir.clone(),
            mode,
            session_path.clone(),
        )
        .await?;
        let Some(next) = next else { return Ok(()) };
        let switched_to_existing = matches!(&next, rc_tui::Outcome::Resume(_));
        let (next_session, next_path) = match next {
            rc_tui::Outcome::Resume(path) => {
                let resumed = rc_session::load(&path)
                    .with_context(|| format!("/menu: could not load {}", path.display()))?;
                (resumed, Some(path))
            }
            rc_tui::Outcome::NewIn(dir) => (
                Session::new(fresh_session_id(), dir, settings.model.clone()),
                None,
            ),
        };
        session = next_session;
        session_path = next_path;
        if model_override.is_some() {
            session.model = settings.model.clone();
        }
        prepare_session(&mut session, &sessions_dir, &extra_dirs);
        // A session resumed through `/menu` restores its own saved mode, the
        // same as `--resume` does; a brand-new one starts from the configured
        // default. Without this the engine would keep the *previous* session's
        // mode after a switch.
        mode = reconcile_mode(&mut session, &permission, &settings, switched_to_existing);
    }
}

/// Per-session setup shared by the initial launch and by a `/menu` session
/// switch: the extra working directories from settings, and a background-shell
/// log directory keyed to *this* session's id (§M7). Re-applied on every
/// switch — a resumed session gets its own `bg/` directory, not the previous
/// session's.
/// Reconcile the permission mode across the three places it lives: the session
/// (persisted + displayed), the engine (actual enforcement), and the value
/// handed to the TUI for its status bar. Returns the agreed mode.
///
/// These used to disagree. The engine was built from
/// `permissions.default_mode` while `Session::new` and the TUI's initial state
/// both hardcoded `Default` and no startup event corrected them. Two visible
/// consequences: setting `default_mode` in `settings.json` enforced correctly
/// but showed "default" in the status bar, and — worse — resuming a session
/// saved in `auto` restored `auto` into the session and the display while the
/// engine silently reverted to the settings mode, so it kept asking for
/// confirmation. That is the "bypass isn't working" symptom.
///
/// A resumed session's own mode wins, since it is what the user last chose;
/// a fresh session takes the configured default.
fn reconcile_mode(
    session: &mut Session,
    permission: &Arc<dyn PermissionChecker>,
    settings: &rc_config::Settings,
    was_resumed: bool,
) -> AgentMode {
    let mode = if was_resumed {
        session.mode
    } else {
        AgentMode::from(Mode::parse(&settings.permissions.default_mode))
    };
    session.mode = mode;
    permission.set_mode(mode.into());
    mode
}

fn prepare_session(session: &mut Session, sessions_dir: &std::path::Path, extra_dirs: &[PathBuf]) {
    session.extra_dirs = extra_dirs.to_vec();
    let bg_dir = sessions_dir.join("..").join("bg").join(&session.id);
    if let Ok(mut s) = session.shell_state.lock() {
        s.bg_dir = Some(bg_dir);
    }
}

/// Build the §4.6 context assembler for a session: the environment block (cwd,
/// platform, date, git branch) plus the hierarchical memory files rooted at the
/// session cwd. Wired into the `AgentLoop` so the model gets a real system
/// prompt and `@file` mention expansion (M6).
fn build_assembler(
    session: &Session,
    caps: &rc_config::ContextConfig,
) -> Arc<dyn ContextAssembler> {
    let env = Environment::from_cwd(&session.cwd);
    let assembler = CtxAssembler::new(env).with_caps(CtxCaps {
        inline_file: caps.inline_file_cap,
        tool_result: caps.tool_result_cap,
    });
    Arc::new(assembler) as Arc<dyn ContextAssembler>
}

/// `~/.sc/sessions/` — where session JSONL files live. Created on first use.
fn sessions_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; cannot locate ~/.sc/sessions")?;
    Ok(PathBuf::from(home).join(".sc").join("sessions"))
}

/// Opaque, collision-resistant identity shared by local persistence and the
/// gateway correlation header. Resumed sessions retain the id in their JSONL
/// header; only genuinely new sessions call this function.
fn fresh_session_id() -> String {
    format!("session-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod session_id_tests {
    use super::fresh_session_id;

    #[test]
    fn fresh_session_ids_are_unique_header_safe_uuids() {
        let first = fresh_session_id();
        let second = fresh_session_id();

        assert_ne!(first, second);
        let uuid = first.strip_prefix("session-").expect("session prefix");
        uuid::Uuid::parse_str(uuid).expect("UUID suffix");
        assert!(first
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }
}

/// Headless `-p`: one turn, then print the final assistant text. An interactive
/// stdin prompter is used only on a TTY; non-interactive runs deny on Ask (fail
/// closed). `--dangerously-skip-permissions` uses `BypassChecker`, which never
/// asks, so the prompter is moot in bypass mode.
async fn run_headless(agent: AgentLoop, mut session: Session, prompt: String) -> Result<()> {
    let prompter: Box<dyn Prompter> = if std::io::stdin().is_terminal() {
        Box::new(StdinPrompter)
    } else {
        Box::new(NullPrompter)
    };
    // Records the largest request context during the run. Provider-returned
    // prompt tokens replace the preflight estimate whenever usage is present.
    let sink = HeadlessSink::default();
    let outcome = agent
        .run(
            &mut session,
            prompt,
            &sink,
            &*prompter,
            CancellationToken::new(),
        )
        .await
        .context("agent loop failed")?;
    print_result(&session, outcome);
    sink.print_context();
    Ok(())
}

/// A headless [`EventSink`] that keeps only the peak context token count. The
/// provider's returned `prompt_tokens` is authoritative; the calibrated
/// preflight estimate is retained only as a fallback when usage is absent.
#[derive(Default)]
struct HeadlessSink {
    peak_estimated_tokens: std::sync::atomic::AtomicUsize,
    peak_reported_tokens: std::sync::atomic::AtomicU64,
}

impl rc_core::model::EventSink for HeadlessSink {
    fn on_context(&self, _chars: usize, est_tokens: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.peak_estimated_tokens.fetch_max(est_tokens, Relaxed);
    }

    fn on_usage(&self, usage: &rc_core::Usage) {
        use std::sync::atomic::Ordering::Relaxed;
        self.peak_reported_tokens
            .fetch_max(usage.prompt_tokens, Relaxed);
    }
}

impl HeadlessSink {
    /// Report the peak context to stderr (stdout holds the answer for piping).
    fn print_context(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let reported = self.peak_reported_tokens.load(Relaxed);
        if reported > 0 {
            eprintln!("context: {reported} tokens at peak (provider reported)");
        } else {
            let estimated = self.peak_estimated_tokens.load(Relaxed);
            if estimated > 0 {
                eprintln!("context: ~{estimated} tokens at peak (preflight estimate)");
            }
        }
    }
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
    initial_mode: AgentMode,
    resumed_path: Option<PathBuf>,
) -> Result<Option<rc_tui::Outcome>> {
    let cwd = session.cwd.clone();
    let history = session.messages.clone();

    // Fail with a useful message instead of crossterm's raw-mode error. Without
    // this, a piped or non-tty invocation dies with "Device not configured
    // (os error 6)", which says nothing about what to do next.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "sc needs a terminal for the interactive TUI.\n\
             For non-interactive use, run a one-shot: sc -p \"<prompt>\""
        );
    }

    // Persistence: a fresh session gets a new timestamped file; a resumed
    // session re-opens its existing file in append mode (the header and old
    // turns are already on disk — no rewrite, no risk of losing history).
    //
    // Created *after* the terminal check on purpose: creating it first left a
    // header-only orphan file behind on every failed startup, and `--continue`
    // would then pick that orphan as the newest session and resume nothing.
    let is_resumed = resumed_path.is_some();
    let path = resumed_path.unwrap_or_else(|| sessions_dir.join(format!("{}.jsonl", session.id)));
    let store = if is_resumed {
        // Preserve the exact selected/`--resume` path. Reconstructing a path
        // from the id would append somewhere else for imported session files.
        Some(rc_session::SessionStore::open_append(path)?)
    } else {
        Some(rc_session::SessionStore::create(path, &session)?)
    };

    let runtime = rc_rt::Runtime::new(agent, session, store);
    match tokio::task::spawn_blocking(move || {
        rc_tui::run(runtime, model_name, cwd, initial_mode, history)
    })
    .await
    {
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
    // for piping). Cache efficiency is a rate over returned prompt tokens, not
    // another raw count competing with the context figure.
    let u = &session.total_usage;
    if u.total_tokens > 0 {
        let cache_rate = u
            .cache_hit_rate()
            .map(|rate| format!("{:.1}%", rate * 100.0))
            .unwrap_or_else(|| "not reported".to_string());
        eprintln!(
            "tokens: {} total · {} completion · {cache_rate} cache hit",
            u.total_tokens, u.completion_tokens,
        );
        // Integer micro-USD cost (the accounting monoid); shown only when a
        // pricing sheet was configured, so the default zero-cost case stays
        // silent. Displayed in USD with 6 decimals (micro-USD resolution).
        let c = session.total_cost.as_micro_usd();
        if c > 0 {
            eprintln!("cost: ${:.6} ({} µUSD)", session.total_cost.as_usd(), c);
        }
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
        let _ = std::io::stderr().flush(); // IoWrite in scope
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
