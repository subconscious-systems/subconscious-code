//! `sc` — Subconscious Code: a terminal coding agent speaking OpenAI-compatible
//! chat completions.
//!
//! Two entry points: a headless one-shot (`sc -p "<prompt>"`) and the
//! interactive TUI (bare `sc`). Both drive the same agent loop over the same
//! tools, permission engine, and context assembler.
//!
//! Context behavior is configurable, with a provider-safe model-facing cap on
//! individual tool results so one runaway command cannot invalidate the next
//! request. The request body is serialized exactly once into refcounted bytes
//! or, above 8 MiB, an immutable disk spool, so a retry never rebuilds it; and
//! the total request timeout is off by default, so a large upload isn't
//! mistaken for a hung one.
//!
//! Known cost, measured: peak RSS runs ~6× the payload (12 MB body → 86.7 MB
//! peak against a 15.2 MB baseline). That multiple lives in the assembly
//! pipeline's `Turn`/`WireMessage` clones, not the transport. Budget memory
//! accordingly for a very large context until those clones are removed.

mod doctor;
mod resource_scope;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rc_config::{RequestTransport, Settings};
use rc_core::agent::{AgentLoop, LoopOutcome};
use rc_core::model::{ChatModel, Model};
use rc_core::registry::ToolRegistry;
use rc_core::tool::Tool;
use rc_core::turn::{AgentMode, ModelTrace, Session, ToolResultBody, Turn};
use rc_core::{
    AskResponse, BypassChecker, ContextAssembler, Mode, NullPrompter, PermissionChecker,
    PermissionEngine, Prompter,
};
use rc_ctx::{Caps as CtxCaps, ContextAssembler as CtxAssembler, Environment};
use rc_proto::{ChatClient, DlrMode, ProtoError, RetryOpts};
use rc_tools::{Append, Bash, Edit, Glob, Grep, GrepMany, List, Read, ReadMany, Write};
use serde_json::Value;
use std::io::{IsTerminal, Write as IoWrite};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    name = "sc",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("SC_BUILD_ID")),
    about = "Subconscious Code — a large-context terminal coding agent.",
    long_about = "Subconscious Code (`sc`): a headless one-shot (`sc -p \"<prompt>\"`) or \
                  the interactive TUI (just `sc`). Either way it speaks an \
                  OpenAI-compatible chat completions backend, with configurable \
                  context caps and provider-safe tool-result projection."
)]
struct Cli {
    /// One-shot headless mode: run the agent loop for PROMPT and print the answer (§5.8 U14).
    #[arg(short, long, value_name = "PROMPT", allow_hyphen_values = true)]
    print: Option<String>,

    /// Write a stable JSON performance report for a headless run.
    /// Intended for benchmark harnesses; requires `--print`.
    #[arg(long, value_name = "PATH", requires = "print")]
    benchmark_report: Option<PathBuf>,

    /// Write an ATIF v1.7 trajectory for a headless benchmark run.
    /// Includes user-visible messages and tool activity, but omits hidden reasoning.
    #[arg(long, value_name = "PATH", requires = "print")]
    benchmark_trajectory: Option<PathBuf>,

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

    /// Provider-native reasoning effort (for GLM use `high` or `max`; `off` omits it).
    #[arg(long, env = "SC_REASONING_EFFORT", value_name = "LEVEL")]
    reasoning_effort: Option<String>,

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

    if cli.command.is_none() {
        if let Some(status) = resource_scope::maybe_reexec() {
            return status;
        }
    }

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
    let benchmark_mode = cli.benchmark_report.is_some() || cli.benchmark_trajectory.is_some();
    let mut settings = Settings::load(&std::env::current_dir()?);
    let model_override = cli.model.clone();
    let base_url_override = cli.base_url.clone();
    if let Some(m) = model_override.clone() {
        settings.model = m;
    }
    if let Some(u) = base_url_override.clone() {
        settings.apply_base_url_override(u);
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

    // Mutable because `/menu` can save a new key mid-run: the reload path below
    // adopts it and rebuilds the client around it.
    let mut api_key = settings
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
    let reasoning_effort = cli
        .reasoning_effort
        .clone()
        .or_else(|| settings.reasoning_effort.clone())
        .and_then(|value| {
            let value = value.trim();
            (!value.is_empty()
                && !value.eq_ignore_ascii_case("off")
                && !value.eq_ignore_ascii_case("none"))
            .then(|| value.to_ascii_lowercase())
        });
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
    // The context caps (§8). Tool results ship with a provider-safe projection
    // cap; a settings file or SC_* env var can tune every limit. Each tool is
    // told its own cap so the schema it advertises matches what it enforces.
    let caps = settings.context;
    let tools = Arc::new(ToolRegistry::new(vec![
        Arc::new(Read::with_limits(
            caps.read_default_limit,
            caps.read_max_line_chars,
        )) as Arc<dyn Tool>,
        Arc::new(ReadMany::with_limits(
            caps.read_default_limit,
            caps.read_max_line_chars,
            caps.tool_result_cap,
        )) as Arc<dyn Tool>,
        Arc::new(Write::new()) as Arc<dyn Tool>,
        Arc::new(Append::new()) as Arc<dyn Tool>,
        Arc::new(Edit::new()) as Arc<dyn Tool>,
        // Recursive path inventories tokenize far worse than normal command
        // output, so List owns a tighter independent entry/byte budget. Do not
        // couple it to the general tool-result or Glob settings.
        Arc::new(List::new()) as Arc<dyn Tool>,
        Arc::new(Glob::with_cap(caps.glob_cap)) as Arc<dyn Tool>,
        Arc::new(Grep::with_cap(caps.grep_output_cap)) as Arc<dyn Tool>,
        Arc::new(GrepMany::with_cap(caps.grep_output_cap)) as Arc<dyn Tool>,
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
    let build_agent =
        |session: &Session, api_key: &str, settings: &Settings| -> Result<AgentLoop> {
            let client = ChatClient::new(
                settings.base_url.clone(),
                api_key.to_string(),
                session.model.clone(),
                timeout,
            )?
            .with_retry(retry)
            .with_request_gzip(settings.request_gzip);
            let client = Arc::new(configure_request_transport(client, settings)?);
            let model = Arc::new(ChatModel::new(client)) as Arc<dyn Model>;
            Ok(AgentLoop::new(model, tools.clone(), permission.clone())
                .with_assembler(build_assembler(session, &caps))
                .with_max_iters(caps.max_iters)
                .with_idle_timeout(idle)
                .with_turn_timeout(turn_timeout)
                .with_max_tokens(max_tokens)
                .with_temperature(temperature)
                .with_reasoning_effort(reasoning_effort.clone())
                .with_completion_review(benchmark_mode)
                .with_sandbox(sandbox))
        };

    // Headless one-shot: run one turn and print the answer (§5.8 U14).
    if let Some(prompt) = cli.print.filter(|p| !p.is_empty()) {
        return run_headless(
            build_agent(&session, &api_key, &settings)?,
            session,
            prompt,
            cli.benchmark_report,
            cli.benchmark_trajectory,
        )
        .await;
    }

    // Interactive TUI (M4) with persistence (M5), in a loop so `/menu` can
    // switch sessions without restarting the process. The TUI returns the
    // session it wants next; everything cwd-scoped is rebuilt around it here,
    // which is precisely why the switch can't happen inside rc-tui.
    loop {
        let model_name = session.model.clone();
        // Kept for the reload path, which needs them after `session` has moved
        // into the TUI.
        let session_id = session.id.clone();
        let next = run_tui(
            Arc::new(build_agent(&session, &api_key, &settings)?),
            session,
            model_name,
            sessions_dir.clone(),
            mode,
            session_path.clone(),
            settings.mouse,
        )
        .await?;
        let Some(next) = next else { return Ok(()) };
        // A reload re-enters the *same* session, so it restores that session's
        // own mode exactly as a resume does.
        let reload_settings = matches!(&next, rc_tui::Outcome::Reload);
        let switched_to_existing =
            matches!(&next, rc_tui::Outcome::Resume(_) | rc_tui::Outcome::Reload);
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
            // `/menu` saved an API key. Adopt it — the user typed it into this
            // process, so it wins here even where the env var would outrank it
            // at startup — and reopen the same session from its own file, which
            // already holds every turn (the store appends as they happen).
            rc_tui::Outcome::Reload => {
                if let Some(saved) = rc_config::saved_api_key() {
                    api_key = saved;
                }
                let path = session_path
                    .clone()
                    .unwrap_or_else(|| sessions_dir.join(format!("{session_id}.jsonl")));
                let reloaded = rc_session::load(&path).with_context(|| {
                    format!("/menu: could not reopen {} to reload", path.display())
                })?;
                (reloaded, Some(path))
            }
        };
        session = next_session;
        session_path = next_path;
        if reload_settings {
            // Settings that change the HTTP client (currently the DLR toggle)
            // take effect without abandoning the active conversation.
            settings = Settings::load(&session.cwd);
            if let Some(model) = model_override.clone() {
                settings.model = model;
            }
            if let Some(url) = base_url_override.clone() {
                settings.apply_base_url_override(url);
            }
        }
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

fn configure_request_transport(
    client: ChatClient,
    settings: &Settings,
) -> Result<ChatClient, ProtoError> {
    match settings.request_transport {
        RequestTransport::Json => Ok(client),
        RequestTransport::Dlr | RequestTransport::Auto => client.with_dlr(
            settings.dlr_url.clone(),
            settings.dlr_ingress_token.clone(),
            if settings.request_transport == RequestTransport::Dlr {
                DlrMode::Required
            } else {
                DlrMode::Prefer
            },
            settings.dlr_repair_margin_pct,
        ),
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
    // Session transcripts are append-only, and rewind state now is too: file
    // preimages live in a durable content-addressed store keyed by session id.
    // Resuming the JSONL transcript therefore also restores `/rewind` across a
    // process restart instead of pointing at a deleted TempDir.
    let rewind_root = sessions_dir.join("..").join("artifacts").join(&session.id);
    match rc_core::state::ChangeJournal::durable(rewind_root) {
        Ok(journal) => {
            session.change_journal = Arc::new(std::sync::Mutex::new(journal));
        }
        Err(error) => tracing::warn!("durable rewind journal unavailable: {error}"),
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
    Arc::new(LazyContextAssembler {
        cwd: session.cwd.clone(),
        caps: CtxCaps {
            inline_file: caps.inline_file_cap,
            tool_result: caps.tool_result_cap,
        },
        inner: OnceLock::new(),
    }) as Arc<dyn ContextAssembler>
}

/// Delay git/environment discovery and hierarchical memory reads until the
/// first prompt. The welcome screen does not consume the system prompt, so
/// doing this work before its first frame only makes launch feel slower.
struct LazyContextAssembler {
    cwd: PathBuf,
    caps: CtxCaps,
    inner: OnceLock<CtxAssembler>,
}

impl LazyContextAssembler {
    fn get(&self) -> &CtxAssembler {
        self.inner.get_or_init(|| {
            CtxAssembler::new(Environment::from_cwd(&self.cwd)).with_caps(self.caps)
        })
    }
}

impl ContextAssembler for LazyContextAssembler {
    fn assemble(&self, turns: &[Turn]) -> Vec<rc_proto::WireMessage> {
        self.get().assemble(turns)
    }

    fn assemble_for(&self, turns: &[Turn], cwd: &std::path::Path) -> Vec<rc_proto::WireMessage> {
        if cwd == self.cwd {
            return self.get().assemble(turns);
        }
        // Directory changes are rare and semantically important: refresh the
        // environment, git branch, hierarchical AGENTS.md chain, and @file
        // root instead of reusing the startup snapshot.
        CtxAssembler::new(Environment::from_cwd(cwd))
            .with_caps(self.caps)
            .assemble(turns)
    }

    fn system_prompt(&self) -> Option<&str> {
        Some(self.get().system_prompt())
    }

    fn context_key(&self, turns: &[Turn]) -> Option<rc_algebra::ContextKey> {
        ContextAssembler::context_key(self.get(), turns)
    }

    fn prefix_fingerprint(&self, turns: &[Turn]) -> Option<rc_algebra::PrefixFingerprint> {
        ContextAssembler::prefix_fingerprint(self.get(), turns)
    }
}

#[cfg(test)]
mod lazy_assembler_tests {
    use super::*;

    fn system_text(messages: &[rc_proto::WireMessage]) -> &str {
        match &messages[0] {
            rc_proto::WireMessage::System { content } => content.as_ref(),
            other => panic!("expected system message, got {other:?}"),
        }
    }

    #[test]
    fn context_discovery_waits_for_first_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "lazy startup memory").unwrap();
        let assembler = LazyContextAssembler {
            cwd: dir.path().to_path_buf(),
            caps: CtxCaps {
                inline_file: 0,
                tool_result: 0,
            },
            inner: OnceLock::new(),
        };

        assert!(assembler.inner.get().is_none(), "construction does no I/O");
        let prompt = ContextAssembler::system_prompt(&assembler).unwrap();
        assert!(prompt.contains("lazy startup memory"));
        assert!(
            assembler.inner.get().is_some(),
            "first use initializes once"
        );
    }

    #[test]
    fn changed_cwd_refreshes_environment_and_project_memory() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("AGENTS.md"), "FIRST MEMORY").unwrap();
        std::fs::write(second.join("AGENTS.md"), "SECOND MEMORY").unwrap();
        let assembler = LazyContextAssembler {
            cwd: first.clone(),
            caps: CtxCaps {
                inline_file: 0,
                tool_result: 0,
            },
            inner: OnceLock::new(),
        };

        let initial = assembler.assemble_for(&[], &first);
        let changed = assembler.assemble_for(&[], &second);
        assert!(system_text(&initial).contains("FIRST MEMORY"));
        assert!(system_text(&changed).contains("SECOND MEMORY"));
        assert!(!system_text(&changed).contains("FIRST MEMORY"));
        assert!(system_text(&changed).contains(&second.display().to_string()));
    }
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
    use super::{fresh_session_id, Cli};
    use clap::Parser;

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

    #[test]
    fn print_accepts_a_prompt_beginning_with_a_hyphen() {
        let cli = Cli::try_parse_from(["sc", "--print", "- Update the display grid"])
            .expect("dash-prefixed prompts are task content, not CLI flags");

        assert_eq!(cli.print.as_deref(), Some("- Update the display grid"));
    }
}

/// Headless `-p`: one turn, then print the final assistant text. An interactive
/// stdin prompter is used only on a TTY; non-interactive runs deny on Ask (fail
/// closed). `--dangerously-skip-permissions` uses `BypassChecker`, which never
/// asks, so the prompter is moot in bypass mode.
async fn run_headless(
    agent: AgentLoop,
    mut session: Session,
    prompt: String,
    benchmark_report: Option<PathBuf>,
    benchmark_trajectory: Option<PathBuf>,
) -> Result<()> {
    let started = Instant::now();
    let prompter: Box<dyn Prompter> = if std::io::stdin().is_terminal() {
        Box::new(StdinPrompter)
    } else {
        Box::new(NullPrompter)
    };
    // Records the largest request context during the run. Provider-returned
    // prompt tokens replace the preflight estimate whenever usage is present.
    let sink = HeadlessSink::new(
        &session,
        benchmark_report.clone(),
        benchmark_trajectory.clone(),
        started,
    );
    let cancel = CancellationToken::new();
    let pressure_cancel = cancel.clone();
    let _resource_monitor = resource_scope::ResourceMonitor::start(move |snapshot| {
        eprintln!(
            "resource pressure stayed at {}% ({} / {} MiB); cancelling cleanly before OOM",
            snapshot.percent,
            snapshot.current_bytes / (1024 * 1024),
            snapshot.max_bytes / (1024 * 1024)
        );
        pressure_cancel.cancel();
    });
    let run_result = agent
        .run(&mut session, prompt, &sink, &*prompter, cancel)
        .await;
    let outcome = match run_result {
        Ok(outcome) => outcome,
        Err(error) => {
            // Publish the last completed-turn prefix even when the model layer
            // fails. `on_turn` already checkpointed it; this final pass marks
            // the artifact terminal rather than leaving `outcome=incomplete`
            // from an earlier checkpoint with stale metrics.
            sink.finalize(&session, LoopOutcome::Incomplete)?;
            return Err(error).context("agent loop failed");
        }
    };
    print_result(&session, outcome);
    sink.print_context();
    sink.finalize(&session, outcome)?;
    Ok(())
}

/// Stable, machine-readable result emitted by `--benchmark-report`.
///
/// The report deliberately excludes prompts, reasoning, tool arguments, and
/// tool output. Benchmark infrastructure gets performance/accounting data
/// without turning its artifact store into a second copy of the transcript.
#[derive(serde::Serialize)]
struct BenchmarkReport<'a> {
    schema_version: u32,
    harness: &'static str,
    harness_version: &'static str,
    harness_build: &'static str,
    provenance: BenchmarkProvenance,
    model: &'a str,
    outcome: &'static str,
    answer_present: bool,
    wall_time_ms: u64,
    model_time_ms: u64,
    tool_time_ms: u64,
    request_count: usize,
    tool_call_count: usize,
    tool_error_count: usize,
    tool_denied_count: usize,
    retry_count: u64,
    usage: BenchmarkUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_micro_usd: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<BenchmarkResources>,
    requests: Vec<&'a ModelTrace>,
}

#[derive(serde::Serialize)]
struct BenchmarkProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
}

#[derive(serde::Serialize)]
struct BenchmarkResources {
    scope_unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_current_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_peak_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oom_kill_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    monitor_peak_bytes: Option<u64>,
    pressure_terminated: bool,
}

#[derive(serde::Serialize)]
struct BenchmarkUsage {
    input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_input_tokens: Option<u64>,
    output_tokens: u64,
    total_tokens: u64,
}

fn build_benchmark_report(
    session: &Session,
    outcome: LoopOutcome,
    elapsed: Duration,
) -> BenchmarkReport<'_> {
    let mut requests = Vec::new();
    let mut tool_call_count = 0usize;
    let mut tool_error_count = 0usize;
    let mut tool_denied_count = 0usize;
    let mut tool_time_ms = 0u64;
    let mut answer_present = false;

    for turn in &session.messages {
        match turn {
            Turn::Assistant {
                text, calls, trace, ..
            } => {
                answer_present |= !text.is_empty();
                tool_call_count = tool_call_count.saturating_add(calls.len());
                if let Some(trace) = trace {
                    requests.push(trace);
                }
            }
            Turn::ToolResult {
                result, duration, ..
            } => {
                tool_time_ms = tool_time_ms.saturating_add(duration_ms_u64(*duration));
                match result {
                    ToolResultBody::Error { .. } | ToolResultBody::Interrupted => {
                        tool_error_count = tool_error_count.saturating_add(1);
                    }
                    ToolResultBody::Denied { .. } => {
                        tool_denied_count = tool_denied_count.saturating_add(1);
                    }
                    ToolResultBody::Ok { .. } => {}
                }
            }
            Turn::Error {
                trace: Some(trace), ..
            } => requests.push(trace),
            _ => {}
        }
    }

    let model_time_ms = requests
        .iter()
        .fold(0u64, |total, trace| total.saturating_add(trace.total_ms));
    let retry_count = requests.iter().fold(0u64, |total, trace| {
        total.saturating_add(u64::from(trace.retries))
    });
    let request_count = requests.len();
    let cost_micro_usd =
        (session.total_cost.as_micro_usd() > 0).then_some(session.total_cost.as_micro_usd());
    let cached_input_tokens = session
        .total_usage
        .prompt_tokens_details
        .as_ref()
        .map(|details| details.cached_tokens);

    BenchmarkReport {
        schema_version: 1,
        harness: "subconscious-code",
        harness_version: env!("CARGO_PKG_VERSION"),
        harness_build: env!("SC_BUILD_ID"),
        provenance: benchmark_provenance(),
        model: &session.model,
        outcome: benchmark_outcome_label(outcome),
        answer_present,
        wall_time_ms: duration_ms_u64(elapsed),
        model_time_ms,
        tool_time_ms,
        request_count,
        tool_call_count,
        tool_error_count,
        tool_denied_count,
        retry_count,
        usage: BenchmarkUsage {
            input_tokens: session.total_usage.prompt_tokens,
            cached_input_tokens,
            output_tokens: session.total_usage.completion_tokens,
            total_tokens: session.total_usage.total_tokens,
        },
        cost_micro_usd,
        cost_usd: cost_micro_usd.map(|_| session.total_cost.as_usd()),
        resources: benchmark_resources(),
        requests,
    }
}

fn benchmark_provenance() -> BenchmarkProvenance {
    let clean_hash = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    BenchmarkProvenance {
        run_id: std::env::var("SC_HARBOR_RUN_ID")
            .ok()
            .filter(|id| !id.is_empty()),
        binary_sha256: clean_hash("SC_HARBOR_INSTALLED_BINARY_SHA256"),
        source_sha256: clean_hash("SC_HARBOR_SOURCE_SHA256"),
        artifact_sha256: clean_hash("SC_HARBOR_ARTIFACT_SHA256"),
        revision: std::env::var("SC_HARBOR_REVISION")
            .ok()
            .filter(|revision| !revision.is_empty()),
    }
}

fn benchmark_resources() -> Option<BenchmarkResources> {
    let scope_unit = std::env::var("SC_RESOURCE_SCOPE_UNIT").ok()?;
    let rlimit_fallback = scope_unit == "rlimit-as";
    let cgroup_path = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("0::"))
                .map(|path| {
                    std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))
                })
        });
    let read_u64 = |name: &str| -> Option<u64> {
        std::fs::read_to_string(cgroup_path.as_ref()?.join(name))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    let oom_kill_count = cgroup_path.as_ref().and_then(|path| {
        let events = std::fs::read_to_string(path.join("memory.events")).ok()?;
        events.lines().find_map(|line| {
            let value = line.strip_prefix("oom_kill ")?;
            value.trim().parse().ok()
        })
    });

    Some(BenchmarkResources {
        scope_unit,
        memory_current_bytes: read_u64("memory.current").or_else(|| {
            rlimit_fallback
                .then(|| proc_status_bytes("VmRSS:"))
                .flatten()
        }),
        memory_peak_bytes: read_u64("memory.peak").or_else(|| {
            rlimit_fallback
                .then(|| proc_status_bytes("VmHWM:"))
                .flatten()
        }),
        memory_max_bytes: std::env::var("SC_RESOURCE_MEMORY_MAX_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .or_else(|| read_u64("memory.max")),
        oom_kill_count,
        monitor_peak_bytes: resource_scope::observed_peak_bytes(),
        pressure_terminated: resource_scope::pressure_terminated(),
    })
}

fn proc_status_bytes(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = status.lines().find_map(|line| {
        line.strip_prefix(field)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })?;
    kib.checked_mul(1024)
}

fn benchmark_outcome_label(outcome: LoopOutcome) -> &'static str {
    match outcome {
        LoopOutcome::Stop => "stop",
        LoopOutcome::Length => "length",
        LoopOutcome::ItersExceeded => "iteration_limit",
        LoopOutcome::NoProgress => "no_progress",
        LoopOutcome::Incomplete => "incomplete",
        LoopOutcome::TimeUp => "time_limit",
        LoopOutcome::Cancelled => "cancelled",
    }
}

fn duration_ms_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn write_benchmark_report(
    path: &std::path::Path,
    session: &Session,
    outcome: LoopOutcome,
    elapsed: Duration,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create benchmark report directory {}", parent.display()))?;
    }
    let report = build_benchmark_report(session, outcome, elapsed);
    let bytes = serde_json::to_vec_pretty(&report).context("serialize benchmark report")?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("write benchmark report {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("publish benchmark report {}", path.display()))?;
    Ok(())
}

/// Build an ATIF v1.7 trajectory from the completed in-memory session.
///
/// Unlike the accounting-only benchmark report, an ATIF trajectory is an
/// explicit transcript artifact: it contains user/assistant messages, tool
/// arguments, and tool observations so Harbor can render and inspect the run.
/// Hidden model reasoning is deliberately omitted.
fn build_benchmark_trajectory(
    session: &Session,
    outcome: LoopOutcome,
    elapsed: Duration,
) -> serde_json::Value {
    use serde_json::{json, Map};

    let report = build_benchmark_report(session, outcome, elapsed);
    let mut steps = Vec::new();
    let mut message_index = 0usize;

    while message_index < session.messages.len() {
        let step_id = steps.len() + 1;
        match &session.messages[message_index] {
            Turn::User { content, .. } => {
                steps.push(json!({
                    "step_id": step_id,
                    "source": "user",
                    "message": content,
                }));
            }
            Turn::Assistant {
                text,
                calls,
                usage,
                cost,
                trace,
                ..
            } => {
                let tool_calls: Vec<_> = calls
                    .iter()
                    .map(|call| {
                        let parsed = serde_json::from_str::<serde_json::Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({"_raw": call.arguments}));
                        let arguments = if parsed.is_object() {
                            parsed
                        } else {
                            json!({"value": parsed})
                        };
                        json!({
                            "tool_call_id": call.id,
                            "function_name": call.name,
                            "arguments": arguments,
                        })
                    })
                    .collect();

                let mut results = Vec::new();
                let mut next_index = message_index + 1;
                while let Some(Turn::ToolResult {
                    call_id,
                    tool,
                    result,
                    duration,
                }) = session.messages.get(next_index)
                {
                    if !calls.iter().any(|call| call.id == *call_id) {
                        break;
                    }
                    let (content, status, mut extra) = match result {
                        ToolResultBody::Ok { content, truncated } => {
                            (content.to_string(), "ok", json!({"truncated": truncated}))
                        }
                        ToolResultBody::Error { message, retryable } => {
                            (message.clone(), "error", json!({"retryable": retryable}))
                        }
                        ToolResultBody::Denied { reason } => (reason.clone(), "denied", json!({})),
                        ToolResultBody::Interrupted => (
                            "Tool call interrupted before execution".to_string(),
                            "interrupted",
                            json!({}),
                        ),
                    };
                    let extra_object = extra.as_object_mut().expect("ATIF result extra object");
                    extra_object.insert("status".to_string(), json!(status));
                    extra_object.insert("tool_name".to_string(), json!(tool));
                    extra_object
                        .insert("duration_ms".to_string(), json!(duration_ms_u64(*duration)));
                    results.push(json!({
                        "source_call_id": call_id,
                        "content": content,
                        "extra": extra,
                    }));
                    next_index += 1;
                }

                let mut metrics = Map::new();
                if let Some(usage) = usage {
                    metrics.insert("prompt_tokens".to_string(), json!(usage.prompt_tokens));
                    metrics.insert(
                        "completion_tokens".to_string(),
                        json!(usage.completion_tokens),
                    );
                    if let Some(details) = &usage.prompt_tokens_details {
                        metrics.insert("cached_tokens".to_string(), json!(details.cached_tokens));
                    }
                }
                if let Some(cost) = cost {
                    metrics.insert("cost_usd".to_string(), json!(cost.as_usd()));
                }
                if let Some(trace) = trace {
                    metrics.insert(
                        "extra".to_string(),
                        json!({
                            "started_ms": trace.started_ms,
                            "completed_ms": trace.completed_ms,
                            "total_ms": trace.total_ms,
                            "response_headers_ms": trace.response_headers_ms,
                            "ttft_ms": trace.ttft_ms,
                            "stream_ms": trace.stream_ms,
                            "request_bytes": trace.request_bytes,
                            "wire_bytes": trace.wire_bytes,
                            "context_chars": trace.context_chars,
                            "context_tokens_estimate": trace.context_tokens_estimate,
                            "retries": trace.retries,
                            "reported_finish_reason": trace.reported_finish_reason,
                            "effective_finish_reason": trace.effective_finish_reason,
                            "implicit_length": trace.implicit_length,
                            "transport_events": trace.transport_events,
                            "semantic_events": trace.semantic_events,
                            "last_transport_activity_ms": trace.last_transport_activity_ms,
                            "last_semantic_activity_ms": trace.last_semantic_activity_ms,
                            "partial_text_chars": trace.partial_text_chars,
                            "partial_reasoning_chars": trace.partial_reasoning_chars,
                            "partial_tool_argument_chars": trace.partial_tool_argument_chars,
                        }),
                    );
                }

                let mut step = Map::new();
                step.insert("step_id".to_string(), json!(step_id));
                step.insert("source".to_string(), json!("agent"));
                step.insert("model_name".to_string(), json!(session.model));
                step.insert("message".to_string(), json!(text));
                step.insert("llm_call_count".to_string(), json!(1));
                if !tool_calls.is_empty() {
                    step.insert("tool_calls".to_string(), json!(tool_calls));
                }
                if !results.is_empty() {
                    step.insert("observation".to_string(), json!({"results": results}));
                }
                if !metrics.is_empty() {
                    step.insert("metrics".to_string(), serde_json::Value::Object(metrics));
                }
                steps.push(serde_json::Value::Object(step));
                message_index = next_index.saturating_sub(1);
            }
            Turn::ToolResult {
                call_id,
                tool,
                result,
                duration,
            } => {
                steps.push(json!({
                    "step_id": step_id,
                    "source": "system",
                    "message": format!("Orphaned tool result: {tool}"),
                    "observation": {
                        "results": [{
                            "content": result.render(),
                            "extra": {
                                "call_id": call_id,
                                "tool_name": tool,
                                "duration_ms": duration_ms_u64(*duration),
                            },
                        }],
                    },
                }));
            }
            Turn::SystemNote { kind, text } => {
                let synthetic = matches!(kind, rc_core::turn::NoteKind::Recovery);
                steps.push(json!({
                    "step_id": step_id,
                    "source": "system",
                    "message": text,
                    "extra": {
                        "note_kind": kind,
                        "synthetic": synthetic,
                        "event": synthetic.then_some("completion_recovery"),
                    },
                }));
            }
            Turn::Error {
                message,
                retryable,
                retries,
                trace,
                ..
            } => {
                steps.push(json!({
                    "step_id": step_id,
                    "source": "system",
                    "message": message,
                    "extra": {
                        "event": "model_error",
                        "retryable": retryable,
                        "retries": retries,
                        "trace": trace,
                    },
                }));
            }
            Turn::Cancelled { .. } => {
                steps.push(json!({
                    "step_id": step_id,
                    "source": "system",
                    "message": "Agent run cancelled",
                    "extra": {"event": "cancelled"},
                }));
            }
        }
        message_index += 1;
    }

    json!({
        "schema_version": "ATIF-v1.7",
        "session_id": session.id,
        "trajectory_id": session.id,
        "agent": {
            "name": "subconscious-code",
            "version": env!("CARGO_PKG_VERSION"),
            "model_name": session.model,
            "extra": {
                "reasoning_content": "omitted",
                "provenance": &report.provenance,
            },
        },
        "steps": steps,
        "notes": "Hidden model reasoning is omitted. User-visible messages, tool arguments, and tool observations are retained for trajectory inspection.",
        "final_metrics": {
            "total_prompt_tokens": report.usage.input_tokens,
            "total_completion_tokens": report.usage.output_tokens,
            "total_cached_tokens": report.usage.cached_input_tokens,
            "total_cost_usd": report.cost_usd,
            "total_steps": steps.len(),
            "extra": {
                "outcome": report.outcome,
                "wall_time_ms": report.wall_time_ms,
                "model_time_ms": report.model_time_ms,
                "tool_time_ms": report.tool_time_ms,
                "request_count": report.request_count,
                "tool_call_count": report.tool_call_count,
                "tool_error_count": report.tool_error_count,
                "tool_denied_count": report.tool_denied_count,
                "retry_count": report.retry_count,
            },
        },
    })
}

fn write_benchmark_trajectory(
    path: &std::path::Path,
    session: &Session,
    outcome: LoopOutcome,
    elapsed: Duration,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create benchmark trajectory directory {}", parent.display())
        })?;
    }
    let trajectory = build_benchmark_trajectory(session, outcome, elapsed);
    let bytes = serde_json::to_vec_pretty(&trajectory).context("serialize benchmark trajectory")?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("write benchmark trajectory {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("publish benchmark trajectory {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod benchmark_report_tests {
    use super::*;
    use rc_core::cost::Cost;
    use rc_proto::wire::PromptTokensDetails;
    use std::sync::Arc;

    fn measured_session() -> Session {
        let mut session = Session::new(
            "bench-session".to_string(),
            PathBuf::from("/workspace"),
            "subconscious/glm-5.2".to_string(),
        );
        session.total_usage = rc_proto::Usage {
            prompt_tokens: 120,
            completion_tokens: 30,
            total_tokens: 150,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 80 }),
        };
        session.total_cost = Cost::from_micro_usd(42);
        session.messages = vec![
            Turn::User {
                content: Arc::from("secret benchmark prompt"),
                ts: std::time::SystemTime::now(),
            },
            Turn::Assistant {
                text: Arc::from(""),
                reasoning: Some(Arc::from("private chain of thought")),
                calls: vec![rc_core::ToolCall {
                    id: "call-1".to_string(),
                    name: "Read".to_string(),
                    arguments: Arc::from(r#"{"file_path":"secret.rs"}"#),
                }],
                usage: Some(rc_proto::Usage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    total_tokens: 120,
                    prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 60 }),
                }),
                cost: Some(Cost::from_micro_usd(30)),
                trace: Some(ModelTrace {
                    total_ms: 75,
                    ttft_ms: Some(12),
                    retries: 1,
                    ..ModelTrace::default()
                }),
            },
            Turn::ToolResult {
                call_id: "call-1".to_string(),
                tool: "Read".to_string(),
                result: ToolResultBody::Ok {
                    content: Arc::from("secret tool output"),
                    truncated: false,
                },
                duration: Duration::from_millis(5),
            },
            Turn::Assistant {
                text: Arc::from("done"),
                reasoning: None,
                calls: Vec::new(),
                usage: None,
                cost: None,
                trace: Some(ModelTrace {
                    total_ms: 25,
                    ..ModelTrace::default()
                }),
            },
        ];
        session
    }

    #[test]
    fn report_has_harbor_metrics_without_transcript_content() {
        let session = measured_session();
        let value = serde_json::to_value(build_benchmark_report(
            &session,
            LoopOutcome::Stop,
            Duration::from_millis(150),
        ))
        .unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["outcome"], "stop");
        assert_eq!(value["wall_time_ms"], 150);
        assert_eq!(value["model_time_ms"], 100);
        assert_eq!(value["tool_time_ms"], 5);
        assert_eq!(value["request_count"], 2);
        assert_eq!(value["harness_build"], env!("SC_BUILD_ID"));
        assert_eq!(value["retry_count"], 1);
        assert_eq!(value["usage"]["input_tokens"], 120);
        assert_eq!(value["usage"]["cached_input_tokens"], 80);
        assert_eq!(value["usage"]["output_tokens"], 30);
        assert_eq!(value["cost_micro_usd"], 42);

        let json = value.to_string();
        for private in [
            "secret benchmark prompt",
            "private chain of thought",
            "secret.rs",
            "secret tool output",
        ] {
            assert!(!json.contains(private), "report leaked {private:?}");
        }
    }

    #[test]
    fn report_is_published_at_requested_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/report.json");
        write_benchmark_report(
            &path,
            &measured_session(),
            LoopOutcome::Length,
            Duration::from_millis(9),
        )
        .unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["outcome"], "length");
        assert_eq!(value["wall_time_ms"], 9);
    }

    #[test]
    fn headless_sink_publishes_a_partial_prefix_at_turn_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("partial-report.json");
        let trajectory_path = dir.path().join("partial-trajectory.json");
        let session = Session::new(
            "partial".into(),
            PathBuf::from("/workspace"),
            "subconscious/glm-5.2".into(),
        );
        let sink = HeadlessSink::new(
            &session,
            Some(report_path.clone()),
            Some(trajectory_path.clone()),
            Instant::now(),
        );

        rc_core::EventSink::on_turn(
            &sink,
            &Turn::User {
                content: "work in progress".into(),
                ts: std::time::SystemTime::now(),
            },
        );

        for _ in 0..100 {
            if report_path.is_file() && trajectory_path.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        let trajectory: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&trajectory_path).unwrap()).unwrap();
        assert_eq!(report["outcome"], "incomplete");
        assert_eq!(trajectory["steps"][0]["message"], "work in progress");
        assert!(trajectory_path.with_extension("turns.jsonl").is_file());
    }

    #[test]
    fn trajectory_is_atif_with_tool_activity_but_without_hidden_reasoning() {
        let value = build_benchmark_trajectory(
            &measured_session(),
            LoopOutcome::Stop,
            Duration::from_millis(150),
        );

        assert_eq!(value["schema_version"], "ATIF-v1.7");
        assert_eq!(value["agent"]["name"], "subconscious-code");
        assert_eq!(value["agent"]["model_name"], "subconscious/glm-5.2");
        assert_eq!(value["steps"].as_array().unwrap().len(), 3);
        assert_eq!(value["steps"][0]["source"], "user");
        assert_eq!(value["steps"][0]["message"], "secret benchmark prompt");
        assert_eq!(value["steps"][1]["source"], "agent");
        assert_eq!(value["steps"][1]["tool_calls"][0]["function_name"], "Read");
        assert_eq!(
            value["steps"][1]["tool_calls"][0]["arguments"]["file_path"],
            "secret.rs"
        );
        assert_eq!(
            value["steps"][1]["observation"]["results"][0]["content"],
            "secret tool output"
        );
        assert_eq!(value["steps"][1]["metrics"]["prompt_tokens"], 100);
        assert_eq!(value["steps"][1]["metrics"]["cached_tokens"], 60);
        assert_eq!(value["steps"][2]["message"], "done");
        assert_eq!(value["final_metrics"]["total_prompt_tokens"], 120);

        assert!(!value.to_string().contains("private chain of thought"));
        assert!(value["steps"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn trajectory_labels_harness_recovery_as_synthetic_system_activity() {
        let mut session = measured_session();
        session.messages.push(Turn::SystemNote {
            kind: rc_core::turn::NoteKind::Recovery,
            text: "take observable action now".to_string(),
        });

        let value = build_benchmark_trajectory(
            &session,
            LoopOutcome::NoProgress,
            Duration::from_millis(150),
        );
        let step = value["steps"].as_array().unwrap().last().unwrap();

        assert_eq!(step["source"], "system");
        assert_eq!(step["message"], "take observable action now");
        assert_eq!(step["extra"]["note_kind"], "recovery");
        assert_eq!(step["extra"]["synthetic"], true);
        assert_eq!(step["extra"]["event"], "completion_recovery");
        assert_eq!(value["final_metrics"]["extra"]["outcome"], "no_progress");
    }

    #[test]
    fn trajectory_is_published_at_requested_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/trajectory.json");
        write_benchmark_trajectory(
            &path,
            &measured_session(),
            LoopOutcome::Stop,
            Duration::from_millis(9),
        )
        .unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], "ATIF-v1.7");
        assert_eq!(value["steps"][1]["tool_calls"][0]["tool_call_id"], "call-1");
    }

    #[test]
    fn benchmark_report_requires_headless_print_mode() {
        let error = Cli::try_parse_from(["sc", "--benchmark-report", "report.json"])
            .expect_err("reporting without --print must be rejected");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn benchmark_trajectory_requires_headless_print_mode() {
        let error = Cli::try_parse_from(["sc", "--benchmark-trajectory", "trajectory.json"])
            .expect_err("trajectory without --print must be rejected");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }
}

/// A headless [`EventSink`] that keeps only the peak context token count. The
/// provider's returned `prompt_tokens` is authoritative; the calibrated
/// preflight estimate is retained only as a fallback when usage is absent.
struct HeadlessSink {
    peak_estimated_tokens: std::sync::atomic::AtomicUsize,
    peak_reported_tokens: std::sync::atomic::AtomicU64,
    checkpoint: Option<HeadlessCheckpoint>,
    progress: Arc<HeadlessProgress>,
    _heartbeat: Option<HeadlessHeartbeat>,
}

struct HeadlessProgress {
    started: Instant,
    phase: std::sync::Mutex<String>,
}

struct HeadlessHeartbeat {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct HeadlessCheckpoint {
    sender: std::sync::Mutex<Option<std::sync::mpsc::Sender<CheckpointCommand>>>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

enum CheckpointCommand {
    Turn(Turn),
    Final {
        session: Session,
        outcome: LoopOutcome,
        reply: std::sync::mpsc::Sender<std::result::Result<(), String>>,
    },
}

impl Default for HeadlessSink {
    fn default() -> Self {
        let progress = Arc::new(HeadlessProgress {
            started: Instant::now(),
            phase: std::sync::Mutex::new("starting".into()),
        });
        Self {
            peak_estimated_tokens: std::sync::atomic::AtomicUsize::new(0),
            peak_reported_tokens: std::sync::atomic::AtomicU64::new(0),
            checkpoint: None,
            progress,
            _heartbeat: None,
        }
    }
}

impl rc_core::model::EventSink for HeadlessSink {
    fn on_tool_start(&self, call: &rc_core::ToolCall) {
        self.progress.set_phase(format!("running {}", call.name));
    }

    fn on_tool_end(&self, _call_id: &str, _tool: &str, _result: &ToolResultBody) {
        self.progress.set_phase("preparing next model request");
    }

    fn on_iter(&self, count: u32, max: u32) {
        self.progress
            .set_phase(format!("model request {count}/{max}"));
    }

    fn on_retry(&self, retries: u32) {
        self.progress.set_phase(format!("provider retry {retries}"));
    }

    fn on_context(&self, _chars: usize, est_tokens: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.peak_estimated_tokens.fetch_max(est_tokens, Relaxed);
    }

    fn on_usage(&self, usage: &rc_core::Usage) {
        use std::sync::atomic::Ordering::Relaxed;
        self.peak_reported_tokens
            .fetch_max(usage.prompt_tokens, Relaxed);
    }

    fn on_turn(&self, turn: &Turn) {
        let Some(checkpoint) = &self.checkpoint else {
            return;
        };
        checkpoint.send(CheckpointCommand::Turn(turn.clone()));
    }
}

impl HeadlessSink {
    fn new(
        session: &Session,
        report_path: Option<PathBuf>,
        trajectory_path: Option<PathBuf>,
        started: Instant,
    ) -> Self {
        let checkpoint = (report_path.is_some() || trajectory_path.is_some()).then(|| {
            HeadlessCheckpoint::new(session.clone(), report_path, trajectory_path, started)
        });
        let progress = Arc::new(HeadlessProgress {
            started,
            phase: std::sync::Mutex::new("starting".into()),
        });
        let heartbeat = Some(HeadlessHeartbeat::new(progress.clone()));
        Self {
            checkpoint,
            progress,
            _heartbeat: heartbeat,
            ..Self::default()
        }
    }

    fn finalize(&self, session: &Session, outcome: LoopOutcome) -> Result<()> {
        let Some(checkpoint) = &self.checkpoint else {
            return Ok(());
        };
        checkpoint.finalize(session.clone(), outcome)
    }

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

impl HeadlessProgress {
    fn set_phase(&self, phase: impl Into<String>) {
        if let Ok(mut current) = self.phase.lock() {
            *current = phase.into();
        }
    }
}

impl HeadlessHeartbeat {
    fn new(progress: Arc<HeadlessProgress>) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || loop {
            std::thread::park_timeout(Duration::from_secs(10));
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            let phase = progress
                .phase
                .lock()
                .map(|phase| phase.clone())
                .unwrap_or_else(|_| "working".into());
            eprintln!(
                "harness heartbeat: {:.0}s · {phase}",
                progress.started.elapsed().as_secs_f64()
            );
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for HeadlessHeartbeat {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl HeadlessCheckpoint {
    fn new(
        session: Session,
        report_path: Option<PathBuf>,
        trajectory_path: Option<PathBuf>,
        started: Instant,
    ) -> Self {
        // The dispatcher owns checkpoint I/O. Enqueuing a completed turn must
        // never park a Tokio runtime worker on slow artifact storage.
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            checkpoint_worker(session, report_path, trajectory_path, started, receiver);
        });
        Self {
            sender: std::sync::Mutex::new(Some(sender)),
            thread: std::sync::Mutex::new(Some(thread)),
        }
    }

    fn send(&self, command: CheckpointCommand) -> bool {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let sent = sender
            .as_ref()
            .is_some_and(|sender| sender.send(command).is_ok());
        if !sent {
            tracing::warn!("headless checkpoint writer is unavailable");
        }
        sent
    }

    fn finalize(&self, session: Session, outcome: LoopOutcome) -> Result<()> {
        let (reply, result) = std::sync::mpsc::channel();
        if !self.send(CheckpointCommand::Final {
            session,
            outcome,
            reply,
        }) {
            return Err(anyhow::anyhow!("headless checkpoint writer is unavailable"));
        }
        let result = result
            .recv()
            .map_err(|_| anyhow::anyhow!("headless checkpoint writer stopped before finalizing"))?;
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        if let Ok(mut thread) = self.thread.lock() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
        result.map_err(anyhow::Error::msg)
    }
}

impl Drop for HeadlessCheckpoint {
    fn drop(&mut self) {
        if let Ok(sender) = self.sender.get_mut() {
            sender.take();
        }
        if let Ok(thread) = self.thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

fn checkpoint_worker(
    mut session: Session,
    report_path: Option<PathBuf>,
    trajectory_path: Option<PathBuf>,
    started: Instant,
    receiver: std::sync::mpsc::Receiver<CheckpointCommand>,
) {
    let journal_path = trajectory_path
        .as_ref()
        .or(report_path.as_ref())
        .map(|path| path.with_extension("turns.jsonl"));
    let mut journal = journal_path.and_then(|path| {
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return None;
            }
        }
        std::fs::File::create(path)
            .ok()
            .map(std::io::BufWriter::new)
    });
    let mut published = false;

    while let Ok(command) = receiver.recv() {
        match command {
            CheckpointCommand::Turn(turn) => {
                if let Some(journal) = &mut journal {
                    if serde_json::to_writer(&mut *journal, &turn).is_ok() {
                        let _ = journal.write_all(b"\n");
                        let _ = journal.flush();
                    }
                }
                if let Turn::Assistant { usage, cost, .. } = &turn {
                    if let Some(usage) = usage {
                        session.total_usage.add(usage);
                    }
                    if let Some(cost) = cost {
                        session.total_cost.add(cost);
                    }
                }
                session.messages.push(turn);
                // Publish one valid prefix, then use the append-only journal as
                // the incremental checkpoint. Rebuilding complete report and
                // ATIF files every second made long runs O(n²) in bytes.
                if !published {
                    if let Err(error) = publish_checkpoint(
                        report_path.as_deref(),
                        trajectory_path.as_deref(),
                        &session,
                        LoopOutcome::Incomplete,
                        started.elapsed(),
                    ) {
                        tracing::warn!("headless incremental checkpoint failed: {error:#}");
                    }
                    published = true;
                }
            }
            CheckpointCommand::Final {
                session,
                outcome,
                reply,
            } => {
                let result = publish_checkpoint(
                    report_path.as_deref(),
                    trajectory_path.as_deref(),
                    &session,
                    outcome,
                    started.elapsed(),
                )
                .map_err(|error| format!("{error:#}"));
                if let Some(journal) = &mut journal {
                    let _ = journal.flush();
                }
                let _ = reply.send(result);
                return;
            }
        }
    }
}

fn publish_checkpoint(
    report_path: Option<&std::path::Path>,
    trajectory_path: Option<&std::path::Path>,
    session: &Session,
    outcome: LoopOutcome,
    elapsed: Duration,
) -> Result<()> {
    if let Some(path) = report_path {
        write_benchmark_report(path, session, outcome, elapsed)?;
    }
    if let Some(path) = trajectory_path {
        write_benchmark_trajectory(path, session, outcome, elapsed)?;
    }
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
    initial_mode: AgentMode,
    resumed_path: Option<PathBuf>,
    mouse: bool,
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

    // Persistence: a fresh session prepares a lazy store whose file is created
    // with its first turn; a resumed session re-opens its existing file in
    // append mode (the header and old turns are already on disk — no rewrite,
    // no risk of losing history).
    //
    // Prepared *after* the terminal check on purpose. Fresh stores are also
    // lazy, so both failed startup and opening/closing without a turn leave no
    // header-only orphan for `--continue` to mistake for useful history.
    let is_resumed = resumed_path.is_some();
    let path = resumed_path.unwrap_or_else(|| sessions_dir.join(format!("{}.jsonl", session.id)));
    let store = if is_resumed {
        // Preserve the exact selected/`--resume` path. Reconstructing a path
        // from the id would append somewhere else for imported session files.
        Some(rc_session::SessionStore::open_append(path)?)
    } else {
        Some(rc_session::SessionStore::create_lazy(path, &session)?)
    };

    let runtime = rc_rt::Runtime::new(agent, session, store);
    let control = runtime.control();
    let _resource_monitor = resource_scope::ResourceMonitor::start(move |snapshot| {
        control.notice(format!(
            "memory pressure held at {}%; cancelling before the hard limit",
            snapshot.percent
        ));
        control.action(rc_rt::UserAction::Quit);
    });
    match tokio::task::spawn_blocking(move || {
        rc_tui::run(runtime, model_name, cwd, initial_mode, history, mouse)
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
            LoopOutcome::NoProgress => {
                eprintln!(
                    "warning: model reached the completion limit twice without making progress"
                )
            }
            LoopOutcome::Incomplete => {
                eprintln!("warning: model response ended without a clean completion marker")
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
