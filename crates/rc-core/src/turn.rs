//! `Turn` (the source of truth) and `Session` (§4.1).
//!
//! `Turn` is the internal representation; the wire form is a fresh projection
//! per request ([`crate::project`]), never stored as state.

use crate::cost::Cost;
use crate::state::{
    ChangeJournal, ReadRegistry, SharedChangeJournal, SharedReadRegistry, SharedShellState,
    ShellState,
};
use rc_proto::Usage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A tool call in the domain model. `arguments` is the model's (or repaired)
/// JSON argument *string*, preserved verbatim so the assistant message re-sent
/// next turn is byte-identical (§4.6).
/// `arguments` is `Arc<str>` so cloning a `ToolCall` (which happens for every
/// re-sent assistant message, §4.6) is a refcount bump, not a deep copy of what
/// can be a whole-file Write/Edit payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Arc<str>,
}

/// Persisted observability for one model request. Millisecond scalar fields
/// keep JSONL easy to inspect with jq and remain backward-compatible: older
/// assistant/error records simply deserialize with `trace: None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTrace {
    /// Wall-clock request boundaries (Unix epoch milliseconds).
    pub started_ms: u64,
    pub completed_ms: u64,
    /// End-to-end model call latency, including body encoding, retries, and the
    /// streamed response body.
    pub total_ms: u64,
    /// Time until HTTP response headers, when the wire client reached them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers_ms: Option<u64>,
    /// Time until the first visible text/reasoning/tool event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Time spent consuming the body after response headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_ms: Option<u64>,
    /// Canonical JSON size before compression and bytes actually uploaded.
    #[serde(default)]
    pub request_bytes: usize,
    #[serde(default)]
    pub wire_bytes: usize,
    /// Context size measured before request serialization.
    #[serde(default)]
    pub context_chars: usize,
    #[serde(default)]
    pub context_tokens_estimate: usize,
    #[serde(default)]
    pub retries: u32,
    /// What the provider reported versus what the loop acted on. They differ
    /// when an empty response at the completion ceiling is recovered as an
    /// implicit `length` finish.
    #[serde(default)]
    pub reported_finish_reason: String,
    #[serde(default)]
    pub effective_finish_reason: String,
    #[serde(default)]
    pub implicit_length: bool,
    /// Raw response-body and semantic model progress. These remain useful on a
    /// failed stream, where provider usage is unavailable and TTFT alone cannot
    /// distinguish a dead socket from a model that stopped producing deltas.
    #[serde(default)]
    pub transport_events: u64,
    #[serde(default)]
    pub semantic_events: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transport_activity_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_semantic_activity_ms: Option<u64>,
    #[serde(default)]
    pub partial_text_chars: usize,
    #[serde(default)]
    pub partial_reasoning_chars: usize,
    #[serde(default)]
    pub partial_tool_argument_chars: usize,
}

/// A bounded snapshot of a response that failed after streaming began. It is
/// persisted only in the private session JSONL and is never projected back to
/// the model or copied into benchmark trajectories.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialStreamResponse {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<PartialToolCall>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialToolCall {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments: String,
    #[serde(default)]
    pub truncated: bool,
}

/// A conversation turn (§4.1). `Turn` is the source of truth; the wire form is
/// a fresh [`crate::project::project`] projection per request, never stored.
/// Serialized to JSONL for session persistence (M5) via the `type` tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Turn {
    User {
        content: Arc<str>,
        #[serde(with = "epoch_millis", default = "epoch_millis::zero")]
        ts: SystemTime,
    },
    Assistant {
        text: Arc<str>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<Arc<str>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Integer micro-USD cost of this response (the accounting monoid,
        /// `rc_core::cost`). Persisted so a resumed session reconstructs the
        /// running total exactly, without needing the price sheet at load
        /// time. `#[serde(default)]` keeps old session files readable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<Cost>,
        /// Request timing/payload/finish metadata. This is never projected back
        /// to the model; it exists solely for trace diagnosis.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace: Option<ModelTrace>,
    },
    ToolResult {
        call_id: String,
        tool: String,
        result: ToolResultBody,
        #[serde(with = "duration_millis", default)]
        duration: Duration,
    },
    /// Compaction markers, mode changes, notices — never injected into the
    /// system prompt (it's already sent); rendered as a user-side block.
    SystemNote { kind: NoteKind, text: String },
    /// A model request *failed* (transport error, HTTP non-2xx after retries
    /// exhausted, context-length rejection, …). Persisted so the session
    /// record shows the failure — previously a failed request vanished from the
    /// JSONL with no trace (the "lack of errors" blind spot). Like `SystemNote`
    /// it is NOT injected into the next request's messages: a failed request
    /// leaves no assistant message to re-send, so the projection skips it.
    Error {
        message: Arc<str>,
        /// `Some(true)` for transient errors the loop could retry, `Some(false)`
        /// for permanent ones; `None` when unknown. Mirrors `ToolResultBody::Error`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
        /// How many wire-layer retries (429/5xx) happened before this failure.
        /// `None` when the failure wasn't retried / the count is unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retries: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace: Option<ModelTrace>,
        /// Bounded partial output from a failed stream. Kept out of model
        /// projection and ATIF export; diagnostic only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial: Option<PartialStreamResponse>,
        #[serde(with = "epoch_millis", default = "epoch_millis::zero")]
        ts: SystemTime,
    },
    /// The user cancelled the turn mid-flight (Esc). Persisted so a `--continue`
    /// resume shows the interruption in the scrollback and the loop knows no
    /// assistant turn completed. Not injected into the next request's messages.
    Cancelled {
        #[serde(with = "epoch_millis", default = "epoch_millis::zero")]
        ts: SystemTime,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultBody {
    Ok { content: Arc<str>, truncated: bool },
    Error { message: String, retryable: bool },
    Denied { reason: String },
    Interrupted,
}

impl ToolResultBody {
    /// Render to the `role:tool` message content. Errors/denials are marked so
    /// the model can tell a result from a file's literal contents.
    ///
    /// Returns `Arc<str>` so the common case (an uncapped `Ok` body) shares the
    /// body's allocation with the session turn and the prepared turn — projecting
    /// a `ToolResult` into a `WireMessage::Tool` is then a refcount bump, not a
    /// copy of (potentially) many megabytes.
    pub fn render(&self) -> Arc<str> {
        match self {
            ToolResultBody::Ok {
                content,
                truncated: true,
            } => Arc::from(format!("{content}\n[output truncated]")),
            ToolResultBody::Ok { content, .. } => content.clone(),
            ToolResultBody::Error { message, .. } => Arc::from(format!("[tool error: {message}]")),
            ToolResultBody::Denied { reason } => Arc::from(format!("[denied: {reason}]")),
            ToolResultBody::Interrupted => Arc::from("[tool call interrupted before execution]"),
        }
    }

    /// Return a copy with an oversized `Ok` body head-truncated to `cap` bytes
    /// (§8.5 microcompaction seam). A tail sentinel records the elision count
    /// and the `truncated` flag is set. Non-`Ok` variants are returned unchanged.
    /// This is a bounded per-result cap, not full summary-turn compaction.
    ///
    /// The cap is in *bytes* (the context-window unit), not characters: a body of
    /// multibyte UTF-8 must not slip `cap` chars (up to 4×cap bytes) through. The
    /// cut is floored to a char boundary so the head is always valid UTF-8.
    pub fn truncate_body(&self, cap: usize) -> ToolResultBody {
        match self {
            ToolResultBody::Ok {
                content,
                truncated: _,
            } => {
                if content.len() <= cap {
                    return self.clone();
                }
                let head = floor_char_boundary(content, cap);
                let head = &content[..head];
                let elided = content.len() - head.len();
                ToolResultBody::Ok {
                    content: Arc::from(format!("{head}\n[… {elided} bytes truncated]")),
                    truncated: true,
                }
            }
            other => other.clone(),
        }
    }
}

impl From<crate::tool::ToolOutcome> for ToolResultBody {
    fn from(o: crate::tool::ToolOutcome) -> Self {
        use crate::tool::ToolOutcome;
        match o {
            ToolOutcome::Ok {
                content, truncated, ..
            } => ToolResultBody::Ok {
                content: Arc::from(content),
                truncated,
            },
            ToolOutcome::Error { message, retryable } => {
                ToolResultBody::Error { message, retryable }
            }
            ToolOutcome::Denied { reason } => ToolResultBody::Denied { reason },
            ToolOutcome::Interrupted => ToolResultBody::Interrupted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Compaction,
    /// Persistent session objective set by `/goal`. An empty value is an
    /// explicit clear marker, so append-only session history stays honest.
    Goal,
    /// Harness-authored control guidance after a truncated model response.
    /// This is intentionally distinct from `Turn::User`: transcript and ATIF
    /// consumers must not attribute synthetic recovery text to the user.
    Recovery,
    ModeChange,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Default,
    AcceptEdits,
    Plan,
    /// Confirm *every* tool call, including reads — the cautious end of the
    /// dial, where `Default` only stops for mutating tools.
    Ask,
    /// Run without prompting. Catastrophic commands are still hard-denied.
    ///
    /// Serialized as `auto`. `bypass_permissions` was the pre-rename spelling
    /// and is accepted on read so session files written by older builds still
    /// load — dropping it would silently reset a saved session to `default`,
    /// which is exactly the "my mode didn't stick" bug this rename came with.
    #[serde(alias = "bypass_permissions")]
    Auto,
}

/// `AgentMode` (the TUI/session-facing enum) and `rc_perm::Mode` (the engine's)
/// have identical variants. These bridges let the runtime cycle the mode once
/// and apply it to both the engine (live enforcement) and the session (render).
impl From<AgentMode> for crate::Mode {
    fn from(m: AgentMode) -> Self {
        match m {
            AgentMode::Default => crate::Mode::Default,
            AgentMode::AcceptEdits => crate::Mode::AcceptEdits,
            AgentMode::Plan => crate::Mode::Plan,
            AgentMode::Ask => crate::Mode::Ask,
            AgentMode::Auto => crate::Mode::Auto,
        }
    }
}

impl From<crate::Mode> for AgentMode {
    fn from(m: crate::Mode) -> Self {
        match m {
            crate::Mode::Default => AgentMode::Default,
            crate::Mode::AcceptEdits => AgentMode::AcceptEdits,
            crate::Mode::Plan => AgentMode::Plan,
            crate::Mode::Ask => AgentMode::Ask,
            crate::Mode::Auto => AgentMode::Auto,
        }
    }
}

/// Minimal session for M1: identity + roots + the turn log + mode + the shared
/// read registry. Sessions, checkpoints, and persistence land in M5.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub extra_dirs: Vec<PathBuf>,
    pub model: String,
    pub messages: Vec<Turn>,
    pub mode: AgentMode,
    pub read_registry: SharedReadRegistry,
    /// Session-scoped permission grants ("Yes, and don't ask again for this"),
    /// added by the loop from [`crate::prompt::AskResponse::Session`].
    pub perm_grants: Vec<String>,
    /// M7: live shell state — persisted `cd` across Bash calls + background shells.
    /// `Session::cwd` is synced from `shell_state.cwd` at the top of each turn.
    pub shell_state: SharedShellState,
    /// M7: the `/rewind` change journal of pre-mutation file contents.
    pub change_journal: SharedChangeJournal,
    /// Cumulative token usage across all turns (metering, M3). Each turn's
    /// prompt re-sends the prefix, so `total_tokens` is an upper bound;
    /// `completion_tokens` is the true cumulative output.
    pub total_usage: Usage,
    /// Cumulative cost across all turns in integer micro-USD — the accounting
    /// monoid (`rc_core::cost`). Integer (not float) so a sharded/parallel
    /// reduction is order-independent and reproducible. Defaults to zero;
    /// reconstructed exactly on resume from the per-turn `cost` records.
    pub total_cost: Cost,
}

impl Session {
    pub fn new(id: String, cwd: PathBuf, model: String) -> Self {
        let shell_state = std::sync::Arc::new(std::sync::Mutex::new(ShellState::new(cwd.clone())));
        let change_journal = std::sync::Arc::new(std::sync::Mutex::new(ChangeJournal::new()));
        Self {
            id,
            cwd,
            extra_dirs: Vec::new(),
            model,
            messages: Vec::new(),
            mode: AgentMode::Default,
            read_registry: Arc::new(std::sync::Mutex::new(ReadRegistry::new())),
            perm_grants: Vec::new(),
            shell_state,
            change_journal,
            total_usage: Usage::default(),
            total_cost: Cost::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mode;
    use std::time::UNIX_EPOCH;

    #[test]
    fn agent_mode_round_trips_to_perm_mode() {
        // The TUI cycles AgentMode; the runtime converts to the engine's Mode and
        // back. All four variants must survive the round trip (identical enums).
        for m in [
            AgentMode::Default,
            AgentMode::AcceptEdits,
            AgentMode::Plan,
            AgentMode::Auto,
        ] {
            let perm: Mode = m.into();
            let back: AgentMode = perm.into();
            assert_eq!(back, m, "{m:?} should round-trip through Mode");
        }
    }

    #[test]
    fn truncate_body_respects_byte_cap_for_multibyte_utf8() {
        // The §8.5 cap is in *bytes*, not characters. A body of N multibyte
        // chars (2 bytes each) over a `cap` must produce a head of at most
        // `cap` bytes — `.chars().take(cap)` would wrongly keep `cap` chars
        // (i.e. 2*cap bytes), blowing the window it's meant to protect.
        let body = ToolResultBody::Ok {
            content: "é".repeat(100).into(),
            truncated: false,
        };
        let cap = 50;
        let out = body.truncate_body(cap);
        let ToolResultBody::Ok { content, truncated } = out else {
            panic!()
        };
        assert!(truncated, "must be flagged truncated");
        // head (≤ cap bytes) + sentinel must not exceed cap + a small sentinel budget.
        assert!(
            content.len() <= cap + 64,
            "byte cap violated: {} > {} — content={:?}",
            content.len(),
            cap + 64,
            content
        );
        assert!(content.contains("truncated"));
    }

    #[test]
    fn turn_serde_round_trips() {
        // A representative turn of each kind must survive a serialize→deserialize
        // cycle (the JSONL persistence path, M5).
        let turns = vec![
            Turn::User {
                content: "hello".into(),
                ts: UNIX_EPOCH + Duration::from_secs(1000),
            },
            Turn::Assistant {
                text: "hi".into(),
                reasoning: Some("thinking".into()),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: r#"{"file_path":"/a"}"#.into(),
                }],
                usage: Some(Usage::default()),
                cost: None,
                trace: Some(ModelTrace {
                    started_ms: 1,
                    completed_ms: 2,
                    total_ms: 1,
                    reported_finish_reason: "tool_calls".into(),
                    effective_finish_reason: "tool_calls".into(),
                    ..ModelTrace::default()
                }),
            },
            Turn::ToolResult {
                call_id: "c1".into(),
                tool: "Read".into(),
                result: ToolResultBody::Ok {
                    content: "body".into(),
                    truncated: true,
                },
                duration: Duration::from_millis(250),
            },
            Turn::SystemNote {
                kind: NoteKind::Notice,
                text: "mode changed".into(),
            },
        ];
        for t in &turns {
            let j = serde_json::to_string(t).unwrap();
            let back: Turn = serde_json::from_str(&j).unwrap();
            let j2 = serde_json::to_string(&back).unwrap();
            assert_eq!(j, j2, "round-trip not stable for {t:?}");
        }
    }
}

// ---- serde helpers for SystemTime / Duration -------------------------------
//
// Store as unsigned millisecond offsets from `UNIX_EPOCH` so JSONL is stable
// and portable (not the platform-specific `SystemTime` serde format). `None`
// from an epoch (pre-1970) degrades to zero — sessions don't predate 1970.

mod epoch_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let ms = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        (ms as u64).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_millis(ms))
    }

    pub fn zero() -> SystemTime {
        UNIX_EPOCH
    }
}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_millis().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

/// Largest index `≤ cap` that lands on a UTF-8 char boundary in `s`, so a
/// byte-based truncation yields valid UTF-8. Walks back at most 3 bytes (the
/// max lead-byte length). `cap` is clamped to `s.len()`. (Std gained
/// kept local so projection truncation retains its exact byte-boundary policy.)
fn floor_char_boundary(s: &str, cap: usize) -> usize {
    let cap = cap.min(s.len());
    let mut i = cap;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
