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
    AskResponse, BypassChecker, Mode, NullPrompter, PermissionChecker, PermissionEngine, Prompter,
};
use rc_proto::ChatClient;
use rc_tools::{Bash, Edit, Glob, Grep, Read, Write};
use std::io::{IsTerminal, Write as IoWrite};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
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

    let client = Arc::new(ChatClient::new(
        settings.base_url.clone(),
        api_key,
        settings.model.clone(),
    )?);
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
    let mut session = Session::new("session".into(), std::env::current_dir()?, settings.model.clone());
    session.extra_dirs = extra_dirs;

    match cli.print {
        // Headless one-shot: run one turn and print the answer (§5.8 U14).
        Some(prompt) if !prompt.is_empty() => {
            run_headless(model, tools, permission, session, prompt).await
        }
        // Otherwise: launch the interactive TUI (M4).
        _ => run_tui(model, tools, permission, session, settings.model).await,
    }
}

/// Headless `-p`: one turn, then print the final assistant text. An interactive
/// stdin prompter is used only on a TTY; non-interactive runs deny on Ask (fail
/// closed). `--dangerously-skip-permissions` uses `BypassChecker`, which never
/// asks, so the prompter is moot in bypass mode.
async fn run_headless(
    model: Arc<dyn Model>,
    tools: Arc<ToolRegistry>,
    permission: Arc<dyn PermissionChecker>,
    mut session: Session,
    prompt: String,
) -> Result<()> {
    let prompter: Box<dyn Prompter> = if std::io::stdin().is_terminal() {
        Box::new(StdinPrompter)
    } else {
        Box::new(NullPrompter)
    };
    let agent = AgentLoop::new(model, tools, permission);
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
async fn run_tui(
    model: Arc<dyn Model>,
    tools: Arc<ToolRegistry>,
    permission: Arc<dyn PermissionChecker>,
    session: Session,
    model_name: String,
) -> Result<()> {
    let agent = Arc::new(AgentLoop::new(model, tools, permission));
    let runtime = rc_rt::Runtime::new(agent, session);
    match tokio::task::spawn_blocking(move || rc_tui::run(runtime, model_name)).await {
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
        None if outcome == LoopOutcome::ItersExceeded => {
            eprintln!("warning: iteration budget reached before the model finished")
        }
        None => eprintln!("(the model produced no answer text)"),
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt::Subscriber::builder()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("rc=debug".parse().expect("valid directive")),
        )
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
