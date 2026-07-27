//! The interactive-prompt seam (§7.4). The loop calls [`Prompter::ask`] when a
//! tool call escalates to Ask; M3's prompters are crude stdin / always-deny.
//! The TUI (M4) provides a richer one; `--dangerously-skip-permissions` avoids
//! asking entirely (via `BypassChecker`).

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

pub trait Prompter: Send + Sync {
    fn ask(&self, tool: &str, input: &Value, reason: &str) -> AskResponse;
}

/// Denies every Ask — for tests and non-interactive headless runs without
/// `--dangerously-skip-permissions` (so unattended `-p` fails closed on Ask).
#[derive(Default)]
pub struct NullPrompter;
impl Prompter for NullPrompter {
    fn ask(&self, _tool: &str, _input: &Value, _reason: &str) -> AskResponse {
        AskResponse::Deny("no prompter available — running non-interactively".into())
    }
}
