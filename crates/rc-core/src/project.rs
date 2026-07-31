//! Projection: `Turn`s → wire messages, and the tool-answer invariant (§4.2).
//!
//! `Turn` is the source of truth; the wire form is computed fresh each request
//! via [`project`] — re-projection is needed after compaction, rewind, and
//! model switches (§4.1). Never store wire messages as state.

use crate::turn::{NoteKind, Turn};
use rc_proto::{FunctionCall, WireMessage};
use rc_proto::ToolCall as WireToolCall;
use std::collections::HashSet;

/// Minimal system prompt for M1–M5. The full §4.6 system prompt (identity,
/// environment block, memory chain, skill index) lands in M6 — see
/// [`rc_ctx::ContextAssembler`], which builds the real system prompt and
/// calls [`project_with`] with it.
const SYSTEM_PROMPT: &str = "You are `sc` (Subconscious Code), an agent that helps with software engineering tasks in \
the user's repository. Use the provided tools to inspect and edit files. Be concise and direct. \
When you have enough information, answer in plain text.";

/// Project the session's turns to wire messages (§4.1) with the default system
/// prompt. The full §4.6 prompt (with an environment block and memory chain)
/// is assembled by `rc-ctx` and passed to [`project_with`].
pub fn project(messages: &[Turn]) -> Vec<WireMessage> {
    project_with(messages, SYSTEM_PROMPT)
}

/// Project the session's turns to wire messages (§4.1) with a caller-supplied
/// system prompt. A leading system message is always first; the conversation
/// order is rigid (§3.1 trap 3).
///
/// This is the seam the M6 context layer uses: it assembles the real §4.6
/// system prompt (identity + environment + memory chain + skill index) and
/// hands it here, then the turn projection stays identical to the legacy path.
pub fn project_with(messages: &[Turn], system_prompt: &str) -> Vec<WireMessage> {
    let mut out = vec![WireMessage::System { content: system_prompt.to_string() }];
    for turn in messages {
        match turn {
            Turn::User { content, .. } => {
                out.push(WireMessage::User { content: content.clone().into() })
            }
            Turn::Assistant { text, calls, .. } => {
                let tool_calls: Vec<WireToolCall> = calls
                    .iter()
                    .map(|c| WireToolCall {
                        id: c.id.clone(),
                        kind: Default::default(),
                        function: FunctionCall { name: c.name.clone(), arguments: c.arguments.clone() },
                    })
                    .collect();
                // Assistant messages with tool calls often carry null content; some
                // providers reject "" (§3.1 trap 1). Omit when there's no text.
                let content = if text.is_empty() && !tool_calls.is_empty() {
                    None
                } else {
                    Some(text.clone())
                };
                out.push(WireMessage::Assistant { content, tool_calls });
            }
            Turn::ToolResult { call_id, result, .. } => {
                out.push(WireMessage::Tool {
                    tool_call_id: call_id.clone(),
                    content: result.render(),
                });
            }
            Turn::SystemNote { kind, text } => {
                // Never into the system prompt (already sent); a user-side block.
                let rendered = match kind {
                    NoteKind::Compaction => format!("<session-summary>{text}</session-summary>"),
                    NoteKind::ModeChange | NoteKind::Notice => format!("[note] {text}"),
                };
                out.push(WireMessage::User { content: rendered.into() });
            }
        }
    }
    out
}

/// The tool-answer invariant (§4.2 / §3.1 trap 3): every assistant `tool_calls`
/// id must be answered by exactly one *contiguous* `role:tool` message before
/// the next non-tool message. Returns `Ok(())` or the first violation.
///
/// Used by the loop as a debug assertion; any early exit from a turn must still
/// synthesize tool results to keep this true.
pub fn verify_invariant(msgs: &[WireMessage]) -> Result<(), String> {
    let mut i = 0;
    while i < msgs.len() {
        if let WireMessage::Assistant { tool_calls, .. } = &msgs[i] {
            if !tool_calls.is_empty() {
                let expected: HashSet<&str> = tool_calls.iter().map(|t| t.id.as_str()).collect();
                let mut got: HashSet<&str> = HashSet::new();
                let mut j = i + 1;
                while j < msgs.len() {
                    if let WireMessage::Tool { tool_call_id, .. } = &msgs[j] {
                        got.insert(tool_call_id.as_str());
                        j += 1;
                    } else {
                        break; // non-tool message ends the contiguous run
                    }
                }
                if j == i + 1 {
                    return Err("assistant tool_calls have no contiguous tool answers".to_string());
                }
                if got != expected {
                    let missing: Vec<&str> = expected.difference(&got).copied().collect();
                    let extra: Vec<&str> = got.difference(&expected).copied().collect();
                    return Err(format!(
                        "tool answer id mismatch (missing={missing:?}, extra={extra:?})"
                    ));
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::{ToolCall, ToolResultBody};
    use rc_proto::wire::UserContent;
    use std::time::SystemTime;

    fn call(id: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "X".into(), arguments: "{}".into() }
    }
    fn ok(content: &str) -> ToolResultBody {
        ToolResultBody::Ok { content: content.into(), truncated: false }
    }
    fn toolresult(id: &str) -> Turn {
        Turn::ToolResult { call_id: id.into(), tool: "X".into(), result: ok("r"), duration: Default::default() }
    }

    #[test]
    fn invariant_ok_for_matched_contiguous_answers() {
        let turns = vec![
            Turn::Assistant { text: "".into(), reasoning: None, calls: vec![call("c1")], usage: None },
            toolresult("c1"),
        ];
        assert!(verify_invariant(&project(&turns)).is_ok());
    }

    #[test]
    fn invariant_detects_id_mismatch() {
        let turns = vec![
            Turn::Assistant { text: "".into(), reasoning: None, calls: vec![call("c1")], usage: None },
            toolresult("c2"),
        ];
        assert!(verify_invariant(&project(&turns)).is_err());
    }

    #[test]
    fn invariant_detects_non_contiguous_answers() {
        // A user message between the assistant(tool_calls) and its tool result.
        let turns = vec![
            Turn::Assistant { text: "".into(), reasoning: None, calls: vec![call("c1")], usage: None },
            Turn::User { content: "hi".into(), ts: SystemTime::now() },
            toolresult("c1"),
        ];
        assert!(verify_invariant(&project(&turns)).is_err(), "contiguity must be enforced");
    }

    #[test]
    fn invariant_detects_missing_answer() {
        let turns = vec![
            Turn::Assistant {
                text: "".into(),
                reasoning: None,
                calls: vec![call("c1"), call("c2")],
                usage: None,
            },
            toolresult("c1"),
        ];
        assert!(verify_invariant(&project(&turns)).is_err());
    }

    #[test]
    fn project_with_uses_the_supplied_system_prompt() {
        // A custom §4.6 system prompt must land as the leading message, and the
        // turn projection must be identical to the default path below it.
        let turns = vec![Turn::User { content: "hi".into(), ts: SystemTime::now() }];
        let wire = project_with(&turns, "CUSTOM SYSTEM PROMPT");
        assert!(matches!(
            wire.first(),
            Some(WireMessage::System { content }) if content == "CUSTOM SYSTEM PROMPT"
        ));
        // The rest mirrors the default path's tail (same length, same user msg).
        let default = project(&turns);
        assert_eq!(wire.len(), default.len());
        assert!(matches!(
            &wire[1],
            WireMessage::User { content: UserContent::Text(t) } if t == "hi"
        ));
        assert!(matches!(
            &default[1],
            WireMessage::User { content: UserContent::Text(t) } if t == "hi"
        ));
    }
}
