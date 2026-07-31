//! The interactive-prompt seam (§7.4). The loop `await`s [`Prompter::ask`] when a
//! tool call escalates to Ask; M3's prompters are crude stdin / always-deny.
//! The TUI (M4) provides a richer one whose `ask` emits an event and awaits a
//! keypress; `--dangerously-skip-permissions` avoids asking entirely (via
//! `BypassChecker`). The trait is async so a TUI prompter can render and block
//! on input without stalling a sync call site — the loop's `&mut grants` borrow
//! is unaffected: `ask(...).await` yields an owned [`AskResponse`] before the
//! grant-push arm runs.

use async_trait::async_trait;
use serde_json::Value;

/// A prompter's response to an Ask.
#[derive(Debug, Clone)]
pub enum AskResponse {
    /// Run this one call only.
    Once,
    /// Grant a rule for the rest of the session (e.g. `Bash(cargo test:*)`).
    Session(String),
    /// Persist the grant to project settings. M3 treats this as [`AskResponse::Session`]
    /// — writing `.rc/settings.local.json` is P3 polish (§7.4).
    Always(String),
    /// Deny, feeding `reason` back to the model as the tool result.
    Deny(String),
}

#[async_trait]
pub trait Prompter: Send + Sync {
    async fn ask(&self, tool: &str, input: &Value, reason: &str) -> AskResponse;
}

/// Denies every Ask — for tests and non-interactive headless runs without
/// `--dangerously-skip-permissions` (so unattended `-p` fails closed on Ask).
#[derive(Default)]
pub struct NullPrompter;
#[async_trait]
impl Prompter for NullPrompter {
    async fn ask(&self, _tool: &str, _input: &Value, _reason: &str) -> AskResponse {
        AskResponse::Deny("no prompter available — running non-interactively".into())
    }
}
