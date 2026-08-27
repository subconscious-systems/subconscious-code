//! `UserAction` — what the host pushes down to the runtime over an `mpsc`.
//! The runtime never exposes core handles to the host; these are the only
//! ways the host affects the loop.

use rc_core::{AgentMode, AskResponse};

#[derive(Debug, Clone)]
pub enum UserAction {
    /// Submit a user prompt; the driver runs one turn.
    Submit(String),
    /// Cancel the in-flight turn, including its model/tool cancellation budget,
    /// and deny any pending permission prompt so the turn can terminate.
    Cancel,
    /// Cycle the permission mode (applies to the engine immediately).
    SetMode(AgentMode),
    /// Answer a pending `AgentEvent::PermissionAsk`.
    PermissionAnswer { id: u64, response: AskResponse },
    /// `/rewind [steps]` — restore the last `steps` turns of agent file changes
    /// from the change journal (Write/Edit snapshots). Bash side-effects are
    /// not rolled back. `steps` defaults to 1.
    Rewind { steps: usize },
    /// `/compact` — append a bounded summary marker. Future model projection
    /// starts at that marker, so prior tool output and turns leave context while
    /// the durable session file remains append-only.
    Compact,
    /// `/goal <objective>` (or `/goal clear`) — persist the active session
    /// objective. `None` is an explicit clear marker.
    SetGoal(Option<String>),
    /// Bare `/goal` — report the currently active session objective.
    ShowGoal,
    /// Stop the runtime (also cancels any in-flight turn).
    Quit,
}
