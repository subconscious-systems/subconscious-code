//! `UserAction` — what the host pushes down to the runtime over an `mpsc`.
//! The runtime never exposes core handles to the host; these are the only
//! ways the host affects the loop.

use rc_core::{AgentMode, AskResponse};

#[derive(Debug, Clone)]
pub enum UserAction {
    /// Submit a user prompt; the driver runs one turn.
    Submit(String),
    /// Cancel the in-flight turn (denies any pending ask; cancels in-flight
    /// tools). The current model stream still runs to completion — interrupting
    /// it is a later milestone (see the M4a plan's known limitations).
    Cancel,
    /// Cycle the permission mode (applies to the engine immediately).
    SetMode(AgentMode),
    /// Answer a pending `AgentEvent::PermissionAsk`.
    PermissionAnswer { id: u64, response: AskResponse },
    /// `/rewind [steps]` — restore the last `steps` turns of agent file changes
    /// from the change journal (Write/Edit snapshots). Bash side-effects are
    /// not rolled back. `steps` defaults to 1.
    Rewind { steps: usize },
    /// Stop the runtime (also cancels any in-flight turn).
    Quit,
}
