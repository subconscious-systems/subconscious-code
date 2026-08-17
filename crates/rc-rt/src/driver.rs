//! The driver task: owns the `Session` and runs one turn per `DriverCmd::Run`,
//! emitting `Ready`/`Outcome`/`Error`/`Idle` boundaries. It never reads
//! `UserAction`s directly — the pump translates those into `DriverCmd`s and
//! owns the per-turn cancel token.

use rc_core::{AgentLoop, AgentMode, EventSink, NoteKind, Session, Turn};
use rc_session::SessionStore;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::event::AgentEvent;
use crate::prompter::RuntimePrompter;

/// Commands the pump sends to the driver.
pub(crate) enum DriverCmd {
    /// Run one turn for `prompt`; `cancel` is the per-turn token the pump also
    /// holds a handle to (so `Cancel` can fire it mid-turn).
    Run {
        prompt: String,
        cancel: CancellationToken,
    },
    /// Update `session.mode` for rendering/persistence (enforcement already
    /// changed via `permission.set_mode` in the pump).
    SetMode(AgentMode),
    /// `/rewind` — restore the last `steps` turns of agent file changes.
    Rewind { steps: usize },
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
                if session.mode != mode {
                    session.mode = mode;
                    // The header is append-only, so record later mode changes
                    // as metadata notes. rc-session replays the latest one on
                    // resume while rc-tui keeps it out of the transcript.
                    session.messages.push(Turn::SystemNote {
                        kind: NoteKind::ModeChange,
                        text: persisted_mode(mode).to_string(),
                    });
                    if let Some(store) = store.as_mut() {
                        if let Some(turn) = session.messages.last() {
                            if let Err(e) = store.append_turn(turn) {
                                tracing::warn!("session persist (mode change) failed: {e}");
                            }
                        }
                    }
                    persisted = session.messages.len();
                }
            }
            DriverCmd::Rewind { steps } => {
                match rc_session::rewind::rewind_session(&mut session, steps) {
                    Ok(report) => {
                        let text = format!(
                            "Rewound {} turn(s) of file changes; restored {} file(s).",
                            report.turns,
                            report.restored.len()
                        );
                        let _ = events.send(AgentEvent::Notice(text.clone()));
                        // Mark the rewind in the transcript so a resumed session
                        // and the model see it. The transcript is append-only,
                        // so the rewound turns stay in history; files are restored.
                        session.messages.push(Turn::SystemNote {
                            kind: NoteKind::Notice,
                            text,
                        });
                        if let Some(store) = store.as_mut() {
                            if let Some(turn) = session.messages.last() {
                                if let Err(e) = store.append_turn(turn) {
                                    tracing::warn!("session persist (rewind note) failed: {e}");
                                }
                            }
                            persisted = session.messages.len();
                        }
                    }
                    Err(e) => {
                        let _ = events.send(AgentEvent::Error(format!("rewind failed: {e}")));
                    }
                }
                let _ = events.send(AgentEvent::Idle);
            }
        }
    }

    // The driver loop has exited (the runtime is shutting down). Kill any
    // background shells so they don't outlive `rc` — std `Child` won't kill on
    // drop, so this must be explicit.
    if let Ok(mut s) = session.shell_state.lock() {
        s.shutdown();
    }
}

fn persisted_mode(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Default => "default",
        AgentMode::AcceptEdits => "accept_edits",
        AgentMode::Plan => "plan",
        AgentMode::Ask => "ask",
        AgentMode::Auto => "auto",
    }
}
