//! The driver task: owns the `Session` and runs one turn per `DriverCmd::Run`,
//! emitting `Ready`/`Outcome`/`Error`/`Idle` boundaries. It never reads
//! `UserAction`s directly — the pump translates those into `DriverCmd`s and
//! owns the per-turn cancel token.

use rc_core::{AgentLoop, AgentMode, EventSink, NoteKind, Session, Turn};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, EventSender};
use crate::prompter::RuntimePrompter;
use crate::sink::SessionWriter;

/// Commands the pump sends to the driver.
pub(crate) enum DriverCmd {
    /// Run one turn for `prompt`; `cancel` is the per-turn token the pump also
    /// holds a handle to (so `Cancel` can fire it mid-turn).
    Run {
        turn_id: u64,
        prompt: String,
        cancel: CancellationToken,
    },
    /// Update `session.mode` for rendering/persistence (enforcement already
    /// changed via `permission.set_mode` in the pump).
    SetMode(AgentMode),
    /// `/rewind` — restore the last `steps` turns of agent file changes.
    Rewind { steps: usize },
    /// `/compact` — summarize the active context and start projection there.
    Compact,
    /// Persist or clear the active `/goal` objective.
    SetGoal(Option<String>),
    /// Report the active `/goal` objective without changing it.
    ShowGoal,
}

pub(crate) enum DriverFeedback {
    TurnFinished { turn_id: u64 },
}

pub(crate) struct DriverTask {
    pub(crate) agent: std::sync::Arc<AgentLoop>,
    pub(crate) session: Session,
    pub(crate) sink: std::sync::Arc<dyn EventSink>,
    pub(crate) prompter: RuntimePrompter,
    pub(crate) events: EventSender,
    pub(crate) store: Option<SessionWriter>,
    pub(crate) feedback: mpsc::Sender<DriverFeedback>,
}

pub(crate) async fn driver_task(task: DriverTask, mut cmds: mpsc::Receiver<DriverCmd>) {
    let DriverTask {
        agent,
        mut session,
        sink,
        prompter,
        events,
        store,
        feedback,
    } = task;
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            DriverCmd::Run {
                turn_id,
                prompt,
                cancel,
            } => {
                events.send(AgentEvent::Ready);
                let outcome = agent
                    .run(&mut session, prompt, sink.as_ref(), &prompter, cancel)
                    .await;
                match outcome {
                    Ok(o) => {
                        events.send(AgentEvent::Outcome(o));
                    }
                    Err(e) => {
                        events.send(AgentEvent::Error(e.to_string()));
                    }
                }
                // RuntimeSink queues each completed turn to its dedicated
                // writer. Persistence is incremental without putting disk
                // flush latency on this driver task.
                let _ = feedback
                    .send(DriverFeedback::TurnFinished { turn_id })
                    .await;
                events.send(AgentEvent::Idle);
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
                    if let Some(store) = &store {
                        if let Some(turn) = session.messages.last() {
                            append_shared(store, turn, "mode change");
                        }
                    }
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
                        events.send(AgentEvent::Notice(text.clone()));
                        // Mark the rewind in the transcript so a resumed session
                        // and the model see it. The transcript is append-only,
                        // so the rewound turns stay in history; files are restored.
                        session.messages.push(Turn::SystemNote {
                            kind: NoteKind::Notice,
                            text,
                        });
                        if let Some(store) = &store {
                            if let Some(turn) = session.messages.last() {
                                append_shared(store, turn, "rewind note");
                            }
                        }
                    }
                    Err(e) => {
                        events.send(AgentEvent::Error(format!("rewind failed: {e}")));
                    }
                }
                events.send(AgentEvent::Idle);
            }
            DriverCmd::Compact => {
                let summary = compaction_summary(&session.messages);
                let note = Turn::SystemNote {
                    kind: NoteKind::Compaction,
                    text: summary,
                };
                session.messages.push(note);
                if let Some(store) = &store {
                    if let Some(turn) = session.messages.last() {
                        append_shared(store, turn, "compaction");
                    }
                }
                events.send(AgentEvent::Notice(
                    "Context compacted; future requests start from the saved summary.".into(),
                ));
                events.send(AgentEvent::Idle);
            }
            DriverCmd::SetGoal(goal) => {
                let text = goal.unwrap_or_default();
                session.messages.push(Turn::SystemNote {
                    kind: NoteKind::Goal,
                    text: text.clone(),
                });
                if let Some(store) = &store {
                    if let Some(turn) = session.messages.last() {
                        append_shared(store, turn, "goal");
                    }
                }
                let notice = if text.is_empty() {
                    "Session goal cleared.".to_string()
                } else {
                    format!("Session goal set: {text}")
                };
                events.send(AgentEvent::Notice(notice));
                events.send(AgentEvent::Idle);
            }
            DriverCmd::ShowGoal => {
                let notice = active_goal(&session.messages)
                    .map(|goal| format!("Active goal: {goal}"))
                    .unwrap_or_else(|| {
                        "No active goal. Set one with /goal <objective>.".to_string()
                    });
                events.send(AgentEvent::Notice(notice));
                events.send(AgentEvent::Idle);
            }
        }
    }

    // The driver loop has exited (the runtime is shutting down). Kill any
    // background shells so they don't outlive `rc` — std `Child` won't kill on
    // drop, so this must be explicit.
    session
        .shell_state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .shutdown();
}

fn append_shared(store: &SessionWriter, turn: &Turn, _kind: &str) {
    store.append(turn);
}

fn active_goal(turns: &[Turn]) -> Option<&str> {
    turns
        .iter()
        .rev()
        .find_map(|turn| match turn {
            Turn::SystemNote {
                kind: NoteKind::Goal,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .filter(|goal| !goal.trim().is_empty())
}

/// Build a bounded, deterministic context checkpoint. Tool bodies and private
/// reasoning are deliberately omitted; recent user/assistant content is kept
/// verbatim because it is safer than inventing a lossy semantic summary in the
/// runtime. The newest entries win when the cap is reached.
fn compaction_summary(turns: &[Turn]) -> String {
    const CAP_CHARS: usize = 16_000;
    let active_start = turns
        .iter()
        .rposition(|turn| {
            matches!(
                turn,
                Turn::SystemNote {
                    kind: NoteKind::Compaction,
                    ..
                }
            )
        })
        .unwrap_or(0);
    let mut entries = Vec::new();
    for turn in &turns[active_start..] {
        let entry = match turn {
            Turn::User { content, .. } => Some(format!("User: {content}")),
            Turn::Assistant { text, calls, .. } => {
                let tools = calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                match (text.trim().is_empty(), tools.is_empty()) {
                    (false, true) => Some(format!("Assistant: {text}")),
                    (false, false) => Some(format!("Assistant: {text}\nTools used: {tools}")),
                    (true, false) => Some(format!("Tools used: {tools}")),
                    (true, true) => None,
                }
            }
            Turn::SystemNote {
                kind: NoteKind::Compaction,
                text,
            } => Some(format!("Previous summary: {text}")),
            Turn::SystemNote {
                kind: NoteKind::Notice | NoteKind::Recovery,
                text,
            } => Some(format!("Note: {text}")),
            Turn::SystemNote {
                kind: NoteKind::Goal | NoteKind::ModeChange,
                ..
            }
            | Turn::ToolResult { .. }
            | Turn::Error { .. }
            | Turn::Cancelled { .. } => None,
        };
        if let Some(entry) = entry {
            entries.push(entry);
        }
    }

    let mut kept = Vec::new();
    let mut used = 0usize;
    for entry in entries.into_iter().rev() {
        let chars = entry.chars().count();
        let remaining = CAP_CHARS.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        if chars <= remaining {
            used += chars;
            kept.push(entry);
        } else {
            let tail = entry
                .chars()
                .rev()
                .take(remaining)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            kept.push(format!("[…older content elided…]{tail}"));
            break;
        }
    }
    kept.reverse();
    if kept.is_empty() {
        "No conversational content preceded this compaction.".into()
    } else {
        kept.join("\n\n")
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
