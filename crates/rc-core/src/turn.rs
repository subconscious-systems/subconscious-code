//! `Turn` (the source of truth) and `Session` (§4.1).
//!
//! `Turn` is the internal representation; the wire form is a fresh projection
//! per request ([`crate::project`]), never stored as state.

use crate::state::{ReadRegistry, SharedReadRegistry};
use rc_proto::Usage;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A tool call in the domain model. `arguments` is the model's (or repaired)
/// JSON argument *string*, preserved verbatim so the assistant message re-sent
/// next turn is byte-identical (§4.6).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub enum Turn {
    User {
        content: String,
        ts: SystemTime,
    },
    Assistant {
        text: String,
        reasoning: Option<String>,
        calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    ToolResult {
        call_id: String,
        tool: String,
        result: ToolResultBody,
        duration: Duration,
    },
    /// Compaction markers, mode changes, notices — never injected into the
    /// system prompt (it's already sent); rendered as a user-side block.
    SystemNote {
        kind: NoteKind,
        text: String,
    },
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub enum NoteKind {
    Compaction,
    ModeChange,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
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
}

impl Session {
    pub fn new(id: String, cwd: PathBuf, model: String) -> Self {
        Self {
            id,
            cwd,
            extra_dirs: Vec::new(),
            model,
            messages: Vec::new(),
            mode: AgentMode::Default,
            read_registry: Arc::new(std::sync::Mutex::new(ReadRegistry::new())),
        }
    }
}
