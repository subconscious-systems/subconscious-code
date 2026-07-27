//! `rc` — a terminal agent harness speaking OpenAI-compatible chat completions.
//!
//! M0: headless one-shot. `rc -p "say hi"` loads settings, issues a single
//! non-streaming `/v1/chat/completions` request, and prints the assistant
//! reply. The agentic loop, tools, TUI, and streaming arrive in M1+.

use anyhow::{Context, Result};
use clap::Parser;
use rc_config::Settings;
use rc_proto::{ChatClient, CompleteOpts, WireMessage};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "rc",
    version,
    about = "A Claude Code–style agent harness (chat completions backend).",
    long_about = "M0: headless one-shot. Run `rc -p \"<prompt>\"`. \
                  Streaming, tools, and the TUI arrive in later milestones."
)]
struct Cli {
    /// One-shot headless mode: print the response to PROMPT and exit (§5.8 U14).
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
            "M0 is headless-only. Use `rc -p \"<prompt>\"`. The TUI lands in M4."
        ),
    };

    let mut settings = Settings::load(&std::env::current_dir()?);
    if let Some(m) = cli.model { settings.model = m; }
    if let Some(u) = cli.base_url { settings.base_url = u; }

    tracing::debug!(model = %settings.model, base_url = %settings.base_url, "settings loaded");

    let api_key = settings
        .api_key
        .clone()
        .context("no API key: set $RC_API_KEY (or the var named by provider.api_key_env)")?;

    let client = ChatClient::new(settings.base_url, settings.model.clone(), api_key)?;
    let messages = vec![WireMessage::User { content: prompt.into() }];

    let resp = client.complete(&messages, &CompleteOpts::default()).await?;
    let text = resp
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();

    println!("{text}");
    Ok(())
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
