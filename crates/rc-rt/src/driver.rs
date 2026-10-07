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
    const ENTRY_CHARS: usize = 2_000;
    let latest_request = turns.iter().enumerate().rev().find_map(|(index, turn)| {
        if let Turn::User { content, .. } = turn {
            Some((index, content))
        } else {
            None
        }
    });
    // Keep task constraints outside the recency queue: a large answer or log
    // must not spend the whole summary budget and erase the user's request.
    let prefix = latest_request.map_or_else(String::new, |(_, content)| {
        format!(
            "Latest user request:\n{}\n\nRecent activity:\n",
            summary_excerpt(content, 4_000)
        )
    });
    let activity_budget = CAP_CHARS.saturating_sub(prefix.chars().count());
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
    // A rolling queue bounds temporary summary allocations as well as output.
    // Excerpt source text before formatting; never clone a whole large log.
    let mut entries = std::collections::VecDeque::<(String, usize)>::new();
    let mut used = 0usize;
    for (index, turn) in turns.iter().enumerate().skip(active_start) {
        let entry = match turn {
            Turn::User { .. } if latest_request.is_some_and(|(latest, _)| latest == index) => None,
            Turn::User { content, .. } => {
                Some(format!("User: {}", summary_excerpt(content, ENTRY_CHARS)))
            }
            Turn::Assistant { text, calls, .. } => {
                let tools = calls
                    .iter()
                    .take(8)
                    .map(|call| {
                        format!(
                            "{} {}",
                            summary_excerpt(&call.name, 64),
                            summary_excerpt(&call.arguments, 256)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let tools = if calls.len() > 8 {
                    format!("{tools} [… {} additional calls]", calls.len() - 8)
                } else {
                    tools
                };
                let text = summary_excerpt(text, ENTRY_CHARS);
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
            } => Some(format!(
                "Previous summary: {}",
                summary_excerpt(text, activity_budget.saturating_sub(32))
            )),
            Turn::SystemNote {
                kind: NoteKind::Notice | NoteKind::Recovery,
                text,
            } => Some(format!("Note: {}", summary_excerpt(text, ENTRY_CHARS))),
            Turn::ToolResult { tool, result, .. } => {
                use rc_core::ToolResultBody;
                let (status, body) = match result {
                    ToolResultBody::Ok { content, truncated } => (
                        if *truncated {
                            "returned output, truncated"
                        } else {
                            "returned output"
                        },
                        content.as_ref(),
                    ),
                    ToolResultBody::Error { message, .. } => ("error", message.as_str()),
                    ToolResultBody::Denied { reason } => ("denied", reason.as_str()),
                    ToolResultBody::Interrupted => ("interrupted", "not completed"),
                };
                Some(format!(
                    "Tool {} [{status}]: {}",
                    summary_excerpt(tool, 64),
                    summary_excerpt(body, 1_000)
                ))
            }
            Turn::SystemNote {
                kind: NoteKind::Goal | NoteKind::ModeChange,
                ..
            }
            | Turn::Error { .. }
            | Turn::Cancelled { .. } => None,
        };
        if let Some(entry) = entry {
            let entry = summary_excerpt(&entry, activity_budget.saturating_sub(2));
            let chars = entry.chars().count();
            used += chars + 2; // includes the inter-entry separator
            entries.push_back((entry, chars));
            while used > activity_budget {
                let excess = used - activity_budget;
                let (oldest, chars) = entries.front_mut().expect("just added an entry");
                if *chars <= excess {
                    used -= *chars + 2;
                    entries.pop_front();
                } else {
                    let clipped = summary_excerpt(oldest, *chars - excess);
                    used -= *chars;
                    *chars = clipped.chars().count();
                    used += *chars;
                    *oldest = clipped;
                }
            }
        }
    }
    if entries.is_empty() {
        return if prefix.is_empty() {
            "No conversational content preceded this compaction.".into()
        } else {
            prefix
        };
    }
    let body = entries
        .into_iter()
        .map(|(entry, _)| entry)
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{prefix}{body}")
}

/// Keep both endpoints so file identifiers, diagnostics, and command exit
/// footers survive. Work and allocation depend on the cap, not the log size.
fn summary_excerpt(text: &str, cap: usize) -> String {
    if text.char_indices().nth(cap).is_none() {
        return text.to_string();
    }
    const MARKER: &str = "\n[… middle omitted …]\n";
    let marker_chars = MARKER.chars().count();
    if cap <= marker_chars {
        return text.chars().take(cap).collect();
    }
    let remaining = cap - marker_chars;
    let head = remaining / 2;
    let tail = remaining - head;
    let head_end = text.char_indices().nth(head).map_or(0, |(index, _)| index);
    let tail_start = text
        .char_indices()
        .rev()
        .nth(tail - 1)
        .map_or(text.len(), |(index, _)| index);
    format!("{}{MARKER}{}", &text[..head_end], &text[tail_start..])
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

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use rc_core::{ToolCall, ToolResultBody};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn user(text: &str) -> Turn {
        Turn::User {
            content: text.into(),
            ts: SystemTime::UNIX_EPOCH,
        }
    }

    fn assistant(text: String, calls: Vec<ToolCall>) -> Turn {
        Turn::Assistant {
            text: text.into(),
            reasoning: Some("hidden-reasoning-marker".into()),
            calls,
            usage: None,
            cost: None,
            trace: None,
        }
    }

    #[test]
    fn latest_request_survives_a_large_assistant_response() {
        let turns = vec![
            user("Fix parser.rs; preserve the public API and test Unicode inputs."),
            assistant("long answer ".repeat(10_000), vec![]),
        ];
        let summary = compaction_summary(&turns);
        assert!(
            summary.contains("preserve the public API"),
            "task constraints disappeared"
        );
        assert!(summary.contains("test Unicode inputs"));
        assert!(summary.chars().count() <= 16_000);
        assert!(!summary.contains("hidden-reasoning-marker"));
    }

    #[test]
    fn tool_command_and_failure_footer_survive_compaction() {
        let turns = vec![
            user("Fix the failing parser tests."),
            assistant(
                String::new(),
                vec![ToolCall {
                    id: "verify".into(),
                    name: "Bash".into(),
                    arguments: Arc::from(r#"{"command":"cargo test parser"}"#),
                }],
            ),
            Turn::ToolResult {
                call_id: "verify".into(),
                tool: "Bash".into(),
                result: ToolResultBody::Ok {
                    content: format!(
                        "parser diagnostic\n{}\nFAILED unicode_parser\nexit: 1",
                        "log output ".repeat(20_000)
                    )
                    .into(),
                    truncated: false,
                },
                duration: Duration::ZERO,
            },
        ];
        let summary = compaction_summary(&turns);
        assert!(summary.contains("cargo test parser"));
        assert!(summary.contains("parser diagnostic"));
        assert!(summary.contains("FAILED unicode_parser"));
        assert!(summary.contains("exit: 1"));
        assert!(summary.chars().count() <= 16_000);
    }

    #[test]
    fn denied_failed_and_interrupted_tools_are_distinguished() {
        let mut turns = vec![user("Update the file safely.")];
        for (id, result) in [
            (
                "failed",
                ToolResultBody::Error {
                    message: "file changed since read".into(),
                    retryable: false,
                },
            ),
            (
                "denied",
                ToolResultBody::Denied {
                    reason: "user refused edit".into(),
                },
            ),
            ("interrupted", ToolResultBody::Interrupted),
        ] {
            turns.push(assistant(
                String::new(),
                vec![ToolCall {
                    id: id.into(),
                    name: "Edit".into(),
                    arguments: Arc::from(r#"{"file_path":"parser.rs"}"#),
                }],
            ));
            turns.push(Turn::ToolResult {
                call_id: id.into(),
                tool: "Edit".into(),
                result,
                duration: Duration::ZERO,
            });
        }
        let summary = compaction_summary(&turns);
        assert!(summary.contains("file changed since read"));
        assert!(summary.contains("user refused edit"));
        assert!(summary.contains("interrupted"));
        assert!(summary.contains("parser.rs"));
    }

    #[test]
    fn repeated_compaction_remains_bounded_and_retains_the_latest_request() {
        let mut turns = vec![user(
            "Keep Unicode input valid and preserve the public API.",
        )];
        for round in 0..12 {
            turns.push(assistant("🦀é 中 ".repeat(8_000), vec![]));
            turns.push(Turn::ToolResult {
                call_id: format!("verify-{round}"),
                tool: "Bash".into(),
                result: ToolResultBody::Ok {
                    content: format!("latest verification {round}: 8 tests passed\nexit: 0").into(),
                    truncated: false,
                },
                duration: Duration::ZERO,
            });
            let summary = compaction_summary(&turns);
            assert!(summary.contains("preserve the public API"));
            assert!(summary.contains(&format!("latest verification {round}: 8 tests passed")));
            assert!(summary.chars().count() <= 16_000);
            turns.push(Turn::SystemNote {
                kind: NoteKind::Compaction,
                text: summary,
            });
        }
    }

    #[test]
    fn latest_request_after_a_compaction_replaces_the_pinned_request() {
        let turns = vec![
            user("Old request marker"),
            Turn::SystemNote {
                kind: NoteKind::Compaction,
                text: "Previous task summary".into(),
            },
            user("New request marker: leave permissions unchanged."),
            assistant("new activity ".repeat(5_000), vec![]),
        ];
        let summary = compaction_summary(&turns);
        assert!(summary.starts_with("Latest user request:\nNew request marker"));
        assert!(summary.contains("leave permissions unchanged"));
        assert!(!summary.contains("Old request marker"));
    }
}
