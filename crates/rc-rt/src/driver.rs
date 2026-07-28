//! The driver task: owns the `Session` and runs one turn per `DriverCmd::Run`,
//! emitting `Ready`/`Outcome`/`Error`/`Idle` boundaries. It never reads
//! `UserAction`s directly — the pump translates those into `DriverCmd`s and
//! owns the per-turn cancel token.

use rc_core::{AgentLoop, AgentMode, EventSink, Session};
use rc_session::SessionStore;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::event::AgentEvent;
use crate::prompter::RuntimePrompter;

/// Commands the pump sends to the driver.
pub(crate) enum DriverCmd {
    /// Run one turn for `prompt`; `cancel` is the per-turn token the pump also
    /// holds a handle to (so `Cancel` can fire it mid-turn).
    Run { prompt: String, cancel: CancellationToken },
    /// Update `session.mode` for rendering/persistence (enforcement already
    /// changed via `permission.set_mode` in the pump).
    SetMode(AgentMode),
}

pub(crate) async fn driver_task(
    agent: std::sync::Arc<AgentLoop>,
    mut session: Session,
    mut cmds: mpsc::UnboundedReceiver<DriverCmd>,
    sink: std::sync::Arc<dyn EventSink>,
    prompter: RuntimePrompter,
    events: broadcast::Sender<AgentEvent>,
    mut store: Option<SessionStore>,
) {
    // How many turns are already persisted — append only the new ones after
    // each `Run` (crash recovery: the file is a valid prefix up to here).
    let mut persisted = session.messages.len();
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            DriverCmd::Run { prompt, cancel } => {
                let _ = events.send(AgentEvent::Ready);
                let outcome = agent
                    .run(&mut session, prompt, sink.as_ref(), &prompter, cancel)
                    .await;
                match outcome {
                    Ok(o) => {
                        let _ = events.send(AgentEvent::Outcome(o));
                    }
                    Err(e) => {
                        let _ = events.send(AgentEvent::Error(e.to_string()));
                    }
                }
                // Persist any turns added by this run (User + Assistant + any
                // ToolResults). A failure to write is logged, not fatal — the
                // in-memory session is still correct for the rest of the run.
                if let Some(store) = store.as_mut() {
                    for turn in &session.messages[persisted..] {
                        if let Err(e) = store.append_turn(turn) {
                            tracing::warn!("session persist failed: {e}");
                            break;
                        }
                    }
                    persisted = session.messages.len();
                }
                let _ = events.send(AgentEvent::Idle);
            }
            DriverCmd::SetMode(mode) => {
                session.mode = mode;
            }
        }
    }
}
