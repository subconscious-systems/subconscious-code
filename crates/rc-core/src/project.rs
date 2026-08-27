//! Projection: `Turn`s → wire messages, and the tool-answer invariant (§4.2).
//!
//! `Turn` is the source of truth; the wire form is computed fresh each request
//! via [`project`] — re-projection is needed after compaction, rewind, and
//! model switches (§4.1). Never store wire messages as state.

use crate::turn::{NoteKind, Turn};
use rc_proto::ToolCall as WireToolCall;
use rc_proto::{FunctionCall, UserContent, WireMessage};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// The largest a single message's `content` may be, in *escaped* bytes.
/// GLM-class gateways reject any one message whose serialized `content`
/// exceeds ~1 MB — `400 "messages[N].content must not exceed 1048576 serialized
/// bytes"` — a per-**message** limit, not a per-string or total-body one (see
/// `sc doctor --body-ladder`). A large user paste or tool result is therefore
/// split into multiple messages, each under this cap (1 MB is 1_048_576); the
/// gateway accepts consecutive user messages and multiple `role:tool` messages
/// sharing one `tool_call_id` (both verified against the real gateway), and the
/// model reads the chunks as one input. The total is still bounded by the
/// route's token context window, which is a model property — no client change
/// lifts it.
const MAX_STRING_ESCAPED: usize = 1_000_000;

/// Minimal system prompt for M1–M5. The full §4.6 system prompt (identity,
/// environment block, memory chain, skill index) lands in M6 — see
/// [`rc_ctx::ContextAssembler`], which builds the real system prompt and
/// calls [`project_with`] with it.
const SYSTEM_PROMPT: &str =
    "You are `sc` (Subconscious Code), an agent that helps with software engineering tasks in \
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
    let mut out = vec![WireMessage::System {
        content: Arc::from(system_prompt),
    }];
    // A compaction marker is a durable projection boundary. The session file
    // stays append-only for trace/history recovery, while requests carry only
    // the latest bounded summary and everything that happened after it.
    let active_start = messages
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
    // Goals are session metadata, not disposable conversational history. When
    // the active goal predates the newest compaction marker, re-inject it just
    // ahead of the summary. A later goal/clear marker remains in the active
    // slice and naturally supersedes the older value.
    if let Some((goal_index, goal)) =
        messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, turn)| match turn {
                Turn::SystemNote {
                    kind: NoteKind::Goal,
                    text,
                } => Some((index, text.as_str())),
                _ => None,
            })
    {
        if goal_index < active_start && !goal.trim().is_empty() {
            push_user(
                &mut out,
                &Arc::from(format!("<session-goal>{goal}</session-goal>")),
            );
        }
    }
    for turn in &messages[active_start..] {
        match turn {
            Turn::User { content, .. } => push_user(&mut out, content),
            Turn::Assistant {
                text,
                reasoning,
                calls,
                ..
            } => {
                let tool_calls: Vec<WireToolCall> = calls
                    .iter()
                    .map(|c| WireToolCall {
                        id: c.id.clone(),
                        kind: Default::default(),
                        function: FunctionCall {
                            name: c.name.clone(),
                            arguments: safe_tool_arguments(&c.arguments),
                        },
                    })
                    .collect();
                // Assistant messages with tool calls often carry null content; some
                // providers reject "" (§3.1 trap 1). Omit when there's no text.
                let reasoning_content = reasoning
                    .as_ref()
                    .filter(|reasoning| !reasoning.trim().is_empty())
                    .cloned();
                let content =
                    if text.is_empty() && (!tool_calls.is_empty() || reasoning_content.is_some()) {
                        None
                    } else {
                        Some(text.clone())
                    };
                // NOTE: a single assistant turn with >1 MB of text is not chunked
                // here — splitting one assistant message would duplicate its
                // tool_calls or reorder the turn. Such a turn is rare (it needs
                // ~250k+ output tokens in one go); if it ever bites, chunking it
                // needs a dedicated strategy, not the user/tool split below.
                out.push(WireMessage::Assistant {
                    content,
                    reasoning_content,
                    tool_calls,
                });
            }
            Turn::ToolResult {
                call_id, result, ..
            } => {
                // A tool result with a rendered body over the per-message limit
                // is split into multiple `role:tool` messages sharing one
                // `tool_call_id`. The gateway accepts this (verified against the
                // real gateway) and the model reads the chunks as one result;
                // the tool-answer invariant still holds because the run stays
                // contiguous and the id set matches.
                let body = result.render();
                if escaped_len(&body) <= MAX_STRING_ESCAPED {
                    out.push(WireMessage::Tool {
                        tool_call_id: call_id.clone(),
                        content: body,
                    });
                } else {
                    for chunk in chunk_by_escaped_len(&body, MAX_STRING_ESCAPED) {
                        out.push(WireMessage::Tool {
                            tool_call_id: call_id.clone(),
                            content: chunk,
                        });
                    }
                }
            }
            Turn::SystemNote { kind, text } => {
                // Never into the system prompt (already sent); a user-side block.
                let rendered: Arc<str> = match kind {
                    NoteKind::Compaction => format!("<session-summary>{text}</session-summary>"),
                    NoteKind::Goal => format!("<session-goal>{text}</session-goal>"),
                    NoteKind::Recovery => text.clone(),
                    NoteKind::ModeChange | NoteKind::Notice => format!("[note] {text}"),
                }
                .into();
                push_user(&mut out, &rendered);
            }
            // A failed or cancelled request leaves no wire message — there is no
            // assistant turn to re-send. They're recorded in the transcript for
            // honesty (the "lack of errors" fix) but are invisible to the model.
            Turn::Error { .. } | Turn::Cancelled { .. } => {}
        }
    }
    out
}

/// Providers require every replayed `function.arguments` value to be a JSON
/// object. A cut stream can leave a persisted parse-error call containing the
/// model's incomplete raw bytes; replaying those bytes poisons every later
/// request with HTTP 400. Preserve valid objects byte-for-byte for cache
/// stability, but replace malformed/scalar values with a small valid envelope.
/// The adjacent tool result still carries the actual parse/interruption error.
pub(crate) fn safe_tool_arguments(arguments: &Arc<str>) -> Arc<str> {
    if matches!(
        serde_json::from_str::<Value>(arguments),
        Ok(Value::Object(_))
    ) {
        return arguments.clone();
    }

    let preview: String = arguments.chars().take(2048).collect();
    Arc::from(
        serde_json::json!({
            "_sc_invalid_tool_arguments": true,
            "raw_preview": preview,
        })
        .to_string(),
    )
}

/// Push one or more `role:user` messages for `content`: a single message when
/// it fits the per-message limit (a refcount bump of the source `Arc`), or
/// multiple consecutive user messages — each under the limit — when it
/// doesn't. The gateway accepts consecutive user messages (verified against
/// the real gateway) and the model reads them as one input; concatenating the
/// chunks reconstructs the original. Only oversized content allocates; the
/// common path keeps the source allocation shared.
fn push_user(out: &mut Vec<WireMessage>, content: &Arc<str>) {
    if escaped_len(content) <= MAX_STRING_ESCAPED {
        out.push(WireMessage::User {
            content: UserContent::Text(content.clone()),
        });
    } else {
        for chunk in chunk_by_escaped_len(content, MAX_STRING_ESCAPED) {
            out.push(WireMessage::User {
                content: UserContent::Text(chunk),
            });
        }
    }
}

/// The byte length `s` occupies as a JSON string literal (escapes included),
/// not counting the surrounding quotes. This mirrors `serde_json`'s default
/// escaping exactly — `"` and `\` as 2 bytes, the short control forms
/// (`\b \t \n \f \r`) as 2, other control chars as `\u00XX` (6), and non-ASCII
/// as raw UTF-8 (serde_json does not escape non-ASCII). Pinned against
/// `serde_json::to_string` by the `escaped_len_matches_serde_json` test, so a
/// serializer change that breaks the assumption fails loudly.
fn escaped_len(s: &str) -> usize {
    // Begin with the UTF-8 byte length. JSON only adds bytes for ASCII escape
    // characters, so this avoids decoding every non-ASCII scalar value.
    s.as_bytes().iter().fold(s.len(), |len, &byte| {
        len + match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 1,
            0x00..=0x1f => 5,
            _ => 0,
        }
    })
}

fn escape_len_char(c: char) -> usize {
    match c {
        '"' | '\\' => 2,
        '\u{08}' | '\t' | '\n' | '\u{0c}' | '\r' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Split `s` into owned `Arc<str>` chunks, each with [`escaped_len`] ≤ `max`,
/// such that concatenating the chunks reconstructs `s` exactly. Splits only on
/// `char` boundaries (UTF-8 safe). Any single char fits (`max` is ~1 MB), so
/// the loop never deadlocks. Used to keep every JSON string under the
/// gateway's per-string limit while preserving the full content.
fn chunk_by_escaped_len(s: &str, max: usize) -> Vec<Arc<str>> {
    let mut chunks = Vec::new();
    let mut buf = String::new();
    let mut cur = 0usize;
    for c in s.chars() {
        let e = escape_len_char(c);
        if cur + e > max && !buf.is_empty() {
            chunks.push(Arc::from(buf.as_str()));
            buf.clear();
            cur = 0;
        }
        buf.push(c);
        cur += e;
    }
    if !buf.is_empty() {
        chunks.push(Arc::from(buf.as_str()));
    }
    chunks
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
        ToolCall {
            id: id.into(),
            name: "X".into(),
            arguments: "{}".into(),
        }
    }
    fn ok(content: &str) -> ToolResultBody {
        ToolResultBody::Ok {
            content: content.into(),
            truncated: false,
        }
    }
    fn toolresult(id: &str) -> Turn {
        Turn::ToolResult {
            call_id: id.into(),
            tool: "X".into(),
            result: ok("r"),
            duration: Default::default(),
        }
    }

    #[test]
    fn invariant_ok_for_matched_contiguous_answers() {
        let turns = vec![
            Turn::Assistant {
                text: "".into(),
                reasoning: None,
                calls: vec![call("c1")],
                usage: None,
                cost: None,
                trace: None,
            },
            toolresult("c1"),
        ];
        assert!(verify_invariant(&project(&turns)).is_ok());
    }

    #[test]
    fn invariant_detects_id_mismatch() {
        let turns = vec![
            Turn::Assistant {
                text: "".into(),
                reasoning: None,
                calls: vec![call("c1")],
                usage: None,
                cost: None,
                trace: None,
            },
            toolresult("c2"),
        ];
        assert!(verify_invariant(&project(&turns)).is_err());
    }

    #[test]
    fn invariant_detects_non_contiguous_answers() {
        // A user message between the assistant(tool_calls) and its tool result.
        let turns = vec![
            Turn::Assistant {
                text: "".into(),
                reasoning: None,
                calls: vec![call("c1")],
                usage: None,
                cost: None,
                trace: None,
            },
            Turn::User {
                content: "hi".into(),
                ts: SystemTime::now(),
            },
            toolresult("c1"),
        ];
        assert!(
            verify_invariant(&project(&turns)).is_err(),
            "contiguity must be enforced"
        );
    }

    #[test]
    fn invariant_detects_missing_answer() {
        let turns = vec![
            Turn::Assistant {
                text: "".into(),
                reasoning: None,
                calls: vec![call("c1"), call("c2")],
                usage: None,
                cost: None,
                trace: None,
            },
            toolresult("c1"),
        ];
        assert!(verify_invariant(&project(&turns)).is_err());
    }

    #[test]
    fn malformed_persisted_tool_arguments_are_safe_to_replay() {
        let turns = vec![
            Turn::Assistant {
                text: Arc::from(""),
                reasoning: None,
                calls: vec![ToolCall {
                    id: "cut-write".into(),
                    name: "Write".into(),
                    arguments: Arc::from("{\"file_path\":\"plan.md\",\"content\":\"cut"),
                }],
                usage: None,
                cost: None,
                trace: None,
            },
            Turn::ToolResult {
                call_id: "cut-write".into(),
                tool: "Write".into(),
                result: ToolResultBody::Interrupted,
                duration: Default::default(),
            },
        ];

        let wire = project(&turns);
        assert!(verify_invariant(&wire).is_ok());
        let WireMessage::Assistant { tool_calls, .. } = &wire[1] else {
            panic!("expected assistant tool call")
        };
        let arguments = &tool_calls[0].function.arguments;
        assert!(matches!(
            serde_json::from_str::<Value>(arguments),
            Ok(Value::Object(_))
        ));
        assert!(arguments.contains("_sc_invalid_tool_arguments"));
    }

    #[test]
    fn project_with_uses_the_supplied_system_prompt() {
        // A custom §4.6 system prompt must land as the leading message, and the
        // turn projection must be identical to the default path below it.
        let turns = vec![Turn::User {
            content: "hi".into(),
            ts: SystemTime::now(),
        }];
        let wire = project_with(&turns, "CUSTOM SYSTEM PROMPT");
        assert!(matches!(
            wire.first(),
            Some(WireMessage::System { content }) if content.as_ref() == "CUSTOM SYSTEM PROMPT"
        ));
        // The rest mirrors the default path's tail (same length, same user msg).
        let default = project(&turns);
        assert_eq!(wire.len(), default.len());
        assert!(matches!(
            &wire[1],
            WireMessage::User { content: UserContent::Text(t) } if t.as_ref() == "hi"
        ));
        assert!(matches!(
            &default[1],
            WireMessage::User { content: UserContent::Text(t) } if t.as_ref() == "hi"
        ));
    }

    #[test]
    fn latest_compaction_marker_replaces_older_projected_context() {
        let turns = vec![
            Turn::User {
                content: "old context must disappear".into(),
                ts: SystemTime::now(),
            },
            Turn::SystemNote {
                kind: NoteKind::Compaction,
                text: "bounded saved summary".into(),
            },
            Turn::User {
                content: "new work".into(),
                ts: SystemTime::now(),
            },
        ];
        let wire = project(&turns);
        let rendered = wire
            .iter()
            .map(|message| format!("{message:?}"))
            .collect::<String>();
        assert!(!rendered.contains("old context must disappear"));
        assert!(rendered.contains("bounded saved summary"));
        assert!(rendered.contains("new work"));
    }

    #[test]
    fn active_goal_survives_compaction_and_a_clear_marker_removes_it() {
        let mut turns = vec![
            Turn::SystemNote {
                kind: NoteKind::Goal,
                text: "finish the release".into(),
            },
            Turn::User {
                content: "old context".into(),
                ts: SystemTime::now(),
            },
            Turn::SystemNote {
                kind: NoteKind::Compaction,
                text: "saved summary".into(),
            },
        ];
        let rendered = project(&turns)
            .into_iter()
            .map(|message| format!("{message:?}"))
            .collect::<String>();
        assert!(rendered.contains("<session-goal>finish the release</session-goal>"));
        assert!(!rendered.contains("old context"));

        turns.push(Turn::SystemNote {
            kind: NoteKind::Goal,
            text: String::new(),
        });
        let cleared = project(&turns)
            .into_iter()
            .map(|message| format!("{message:?}"))
            .collect::<String>();
        assert!(!cleared.contains("finish the release"));
    }

    #[test]
    fn projection_shares_body_allocations_via_arc() {
        // The memory optimization: projecting a Turn into a WireMessage (which
        // happens every request) must be a refcount bump, not a deep copy of
        // the body. Pin that with `Arc::ptr_eq` across the assembly seam — if any
        // step drops to `.to_string()` / `.to_owned()`, the pointers diverge and
        // this fails. `Arc::ptr_eq` is reliable at any size (Arc has no
        // small-string optimization), so a modest body is enough.
        let big_body: Arc<str> = Arc::from("x".repeat(4096));
        let big_text: Arc<str> = Arc::from("y".repeat(4096));
        let turns = vec![
            Turn::User {
                content: big_text.clone(),
                ts: SystemTime::now(),
            },
            Turn::ToolResult {
                call_id: "c1".into(),
                tool: "X".into(),
                result: ToolResultBody::Ok {
                    content: big_body.clone(),
                    truncated: false,
                },
                duration: Default::default(),
            },
        ];
        let wire = project_with(&turns, "sys");
        // wire[0] = System, wire[1] = User, wire[2] = Tool.
        match &wire[1] {
            WireMessage::User {
                content: UserContent::Text(t),
            } => {
                assert!(
                    Arc::ptr_eq(t, &big_text),
                    "user content must share the turn's allocation"
                );
            }
            _ => panic!("expected user message at index 1"),
        }
        match &wire[2] {
            WireMessage::Tool { content, .. } => {
                assert!(
                    Arc::ptr_eq(content, &big_body),
                    "tool body must share the turn's allocation"
                );
            }
            _ => panic!("expected tool message at index 2"),
        }
        // A second request re-projects the same turns; that must not copy either.
        let wire2 = project_with(&turns, "sys");
        match (&wire[2], &wire2[2]) {
            (WireMessage::Tool { content: a, .. }, WireMessage::Tool { content: b, .. }) => {
                assert!(Arc::ptr_eq(a, b), "re-projection must not copy the body");
            }
            _ => panic!("expected tool messages in both projections"),
        }
    }

    /// `escaped_len` must mirror `serde_json`'s default escaping exactly — the
    /// chunker's "under the limit" guarantee is only as good as this match.
    #[test]
    fn escaped_len_matches_serde_json_exactly() {
        let cases = [
            "hello",
            "with \"quotes\" and \\backslashes",
            "tab\there\nnewline",
            "control\x01\x07\x1f chars",
            "unicode: é, 中, 😀, ☃",
            "",
            "\"\"\"\"\"\"",
            "\\\\\\\\",
        ];
        for s in cases {
            let serde_len = serde_json::to_string(s).unwrap().len();
            assert_eq!(escaped_len(s) + 2, serde_len, "escape mismatch for {s:?}");
        }
    }

    /// A string well over the limit — including high-escape chars — must split
    /// into chunks each under the limit (in *escaped* bytes) that concatenate
    /// back to the original exactly.
    #[test]
    fn chunks_stay_under_limit_and_concatenate_back() {
        let max = 1_000_000;
        let mut big = String::from("start \u{1} mid ");
        big.push_str(&"x".repeat(max * 2 + 12345));
        big.push_str("\"end\" \\\\");
        let chunks = chunk_by_escaped_len(&big, max);
        assert!(
            chunks.len() >= 3,
            "expected multiple chunks: {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                escaped_len(c) <= max,
                "chunk over limit: {}",
                escaped_len(c)
            );
        }
        let rejoined: String = chunks.iter().map(|c| c.as_ref()).collect();
        assert_eq!(rejoined, big, "chunks must reconstruct the original");
    }

    /// An oversized user message projects to multiple consecutive `role:user`
    /// messages — each under the per-message limit — whose concatenation
    /// preserves the full content. (An array of parts within one message does
    /// NOT help: the gateway's 1 MB limit is per *message*, so a parts array
    /// still exceeds it. Verified against the real gateway.)
    #[test]
    fn oversized_user_message_projects_to_multiple_user_messages() {
        let big: Arc<str> = Arc::from("x".repeat(MAX_STRING_ESCAPED + 500_000));
        let turns = vec![Turn::User {
            content: big.clone(),
            ts: SystemTime::now(),
        }];
        let wire = project(&turns);
        // wire[0] is the system message; the rest are the user chunks.
        let user_msgs: Vec<&WireMessage> = wire[1..]
            .iter()
            .filter(|m| matches!(m, WireMessage::User { .. }))
            .collect();
        assert!(
            user_msgs.len() >= 2,
            "expected >=2 user messages: {}",
            user_msgs.len()
        );
        for m in &user_msgs {
            let WireMessage::User {
                content: UserContent::Text(t),
            } = m
            else {
                panic!("chunked user content must be bare Text: {m:?}");
            };
            assert!(escaped_len(t) <= MAX_STRING_ESCAPED, "chunk over limit");
        }
        let rejoined: String = user_msgs
            .iter()
            .map(|m| match m {
                WireMessage::User {
                    content: UserContent::Text(t),
                } => t.as_ref(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            rejoined.as_str(),
            big.as_ref(),
            "chunks must reconstruct the original"
        );
    }

    /// Small user content stays a single bare `Text` message and shares the
    /// source `Arc` (the chunker only allocates for oversized content).
    #[test]
    fn small_user_message_stays_text_and_shares_allocation() {
        let s: Arc<str> = Arc::from("hello".to_string());
        let turns = vec![Turn::User {
            content: s.clone(),
            ts: SystemTime::now(),
        }];
        let wire = project(&turns);
        // Exactly one user message, and its content shares the source Arc.
        let user_msgs: Vec<&WireMessage> = wire[1..]
            .iter()
            .filter(|m| matches!(m, WireMessage::User { .. }))
            .collect();
        assert_eq!(
            user_msgs.len(),
            1,
            "small content is one message: {user_msgs:?}"
        );
        let WireMessage::User {
            content: UserContent::Text(t),
        } = user_msgs[0]
        else {
            panic!("small user content must stay Text");
        };
        assert!(
            Arc::ptr_eq(t, &s),
            "small content must share the source Arc"
        );
    }

    /// An oversized tool result projects to multiple `role:tool` messages
    /// sharing one `tool_call_id` (the gateway accepts this and the model
    /// concatenates); the tool-answer invariant still holds.
    #[test]
    fn oversized_tool_result_projects_to_multiple_same_id_tool_messages() {
        let big_body: Arc<str> = Arc::from("x".repeat(MAX_STRING_ESCAPED + 500_000));
        let turns = vec![
            Turn::Assistant {
                text: "".into(),
                reasoning: None,
                calls: vec![call("c1")],
                usage: None,
                cost: None,
                trace: None,
            },
            Turn::ToolResult {
                call_id: "c1".into(),
                tool: "X".into(),
                result: ToolResultBody::Ok {
                    content: big_body.clone(),
                    truncated: false,
                },
                duration: Default::default(),
            },
        ];
        let wire = project(&turns);
        let tool_msgs: Vec<&WireMessage> = wire[1..]
            .iter()
            .filter(|m| matches!(m, WireMessage::Tool { .. }))
            .collect();
        assert!(
            tool_msgs.len() >= 2,
            "expected >=2 tool messages: {}",
            tool_msgs.len()
        );
        for m in &tool_msgs {
            let WireMessage::Tool {
                tool_call_id,
                content,
            } = m
            else {
                unreachable!()
            };
            assert_eq!(tool_call_id, "c1", "all chunks share the call id");
            assert!(
                escaped_len(content) <= MAX_STRING_ESCAPED,
                "chunk over limit"
            );
        }
        let rejoined: String = tool_msgs
            .iter()
            .map(|m| match m {
                WireMessage::Tool { content, .. } => content.as_ref(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(rejoined, big_body.as_ref());
        assert!(
            verify_invariant(&wire).is_ok(),
            "tool-answer invariant must hold with a chunked tool result"
        );
    }
}
