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
use rc_proto::ChatClient;
use rc_tools::{Bash, Edit, Glob, Grep, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    name = "rc",
    version,
    about = "A Claude Code–style agent harness (chat completions backend).",
    long_about = "M1: headless agent loop with the Read tool. Run \
                  `rc -p \"<prompt>\"`. The TUI and more tools arrive in later milestones."
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
    let prompt = match cli.print {
        Some(p) if !p.is_empty() => p,
        _ => anyhow::bail!(
            "M1 is headless-only. Use `rc -p \"<prompt>\"`. The TUI lands in M4."
        ),
    };

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
    let agent = AgentLoop::new(model, tools);

    let mut session = Session::new(
        "session".into(),
        std::env::current_dir()?,
        settings.model.clone(),
    );
    let outcome = agent
        .run(&mut session, prompt, &NullSink, CancellationToken::new())
        .await
        .context("agent loop failed")?;

    print_result(&session, outcome);
    Ok(())
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
