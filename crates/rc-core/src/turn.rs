//! `Turn` (the source of truth) and `Session` (§4.1).
//!
//! `Turn` is the internal representation; the wire form is a fresh projection
//! per request ([`crate::project`]), never stored as state.

use crate::state::{ChangeJournal, ReadRegistry, SharedChangeJournal, SharedReadRegistry, SharedShellState, ShellState};
use rc_proto::Usage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A tool call in the domain model. `arguments` is the model's (or repaired)
/// JSON argument *string*, preserved verbatim so the assistant message re-sent
/// next turn is byte-identical (§4.6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A conversation turn (§4.1). `Turn` is the source of truth; the wire form is
/// a fresh [`crate::project::project`] projection per request, never stored.
/// Serialized to JSONL for session persistence (M5) via the `type` tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Turn {
    User {
        content: String,
        #[serde(with = "epoch_millis", default = "epoch_millis::zero")]
        ts: SystemTime,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
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
    SystemNote {
        kind: NoteKind,
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultBody {
    Ok { content: String, truncated: bool },
    Error { message: String, retryable: bool },
    Denied { reason: String },
    Interrupted,
}

impl ToolResultBody {
    /// Render to the `role:tool` message content. Errors/denials are marked so
    /// the model can tell a result from a file's literal contents.
    pub fn render(&self) -> String {
        match self {
            ToolResultBody::Ok { content, truncated: true } => {
                format!("{content}\n[output truncated]")
            }
            ToolResultBody::Ok { content, .. } => content.clone(),
            ToolResultBody::Error { message, .. } => format!("[tool error: {message}]"),
            ToolResultBody::Denied { reason } => format!("[denied: {reason}]"),
            ToolResultBody::Interrupted => "[interrupted by user]".to_string(),
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
            ToolResultBody::Ok { content, truncated: _ } => {
                if content.len() <= cap {
                    return self.clone();
                }
                let head = floor_char_boundary(content, cap);
                let head = &content[..head];
                let elided = content.len() - head.len();
                ToolResultBody::Ok {
                    content: format!("{head}\n[… {elided} bytes truncated]"),
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
            ToolOutcome::Ok { content, truncated, .. } => {
                ToolResultBody::Ok { content, truncated }
            }
            ToolOutcome::Error { message, retryable } => ToolResultBody::Error { message, retryable },
            ToolOutcome::Denied { reason } => ToolResultBody::Denied { reason },
            ToolOutcome::Interrupted => ToolResultBody::Interrupted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Compaction,
    ModeChange,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
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
            AgentMode::BypassPermissions => crate::Mode::BypassPermissions,
        }
    }
}

impl From<crate::Mode> for AgentMode {
    fn from(m: crate::Mode) -> Self {
        match m {
            crate::Mode::Default => AgentMode::Default,
            crate::Mode::AcceptEdits => AgentMode::AcceptEdits,
            crate::Mode::Plan => AgentMode::Plan,
            crate::Mode::BypassPermissions => AgentMode::BypassPermissions,
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
            AgentMode::BypassPermissions,
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
        let body = ToolResultBody::Ok { content: "é".repeat(100), truncated: false };
        let cap = 50;
        let out = body.truncate_body(cap);
        let ToolResultBody::Ok { content, truncated } = out else { panic!() };
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
            Turn::User { content: "hello".into(), ts: UNIX_EPOCH + Duration::from_secs(1000) },
            Turn::Assistant {
                text: "hi".into(),
                reasoning: Some("thinking".into()),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: r#"{"file_path":"/a"}"#.into(),
                }],
                usage: Some(Usage::default()),
            },
            Turn::ToolResult {
                call_id: "c1".into(),
                tool: "Read".into(),
                result: ToolResultBody::Ok { content: "body".into(), truncated: true },
                duration: Duration::from_millis(250),
            },
            Turn::SystemNote { kind: NoteKind::Notice, text: "mode changed".into() },
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
        let ms = t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
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
/// `str::floor_char_boundary` in 1.80, but this workspace targets 1.75.)
fn floor_char_boundary(s: &str, cap: usize) -> usize {
    let cap = cap.min(s.len());
    let mut i = cap;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
