//! rc-core: the agent loop, turn state, and orchestration (§4).
//!
//! `Turn` is the source of truth; the wire form is a fresh [`project::project`]
//! projection per request (§4.1) — never stored. The agent loop drives
//! streaming + tool calling with concurrency classes (§4.3) and enforces the
//! tool-answer invariant (§4.2 / §3.1 trap 3).
//!
//! Architecture note (deviation from the §1 diagram): the `Tool` trait lives
//! here (§16), and concrete tools in `rc-tools` implement it — so `rc-tools`
//! depends on `rc-core`, NOT the other way around. The diagram's
//! `rc-core → rc-tools` arrow would create a cycle and drag ripgrep/pty into
//! core; the testability requirement ("core runs from a unit test with no
//! terminal") wins, so the dependency is inverted here. The composition root
//! (rc-cli) wires concrete tools into a [`registry::ToolRegistry`].

pub mod agent;
pub mod context;
pub mod model;
pub mod project;
pub mod prompt;
pub mod registry;
pub mod state;
pub mod tool;
pub mod turn;

pub use agent::{AgentLoop, LoopError, LoopOutcome};
pub use context::{ContextAssembler, LegacyAssembler};
pub use model::{
    ChatModel, EventSink, FinalizedToolCall, Model, ModelError, ModelRequest, ModelResponse, NullSink,
};
pub use rc_proto::FinishReason;
pub use rc_proto::Usage;
pub use prompt::{AskResponse, NullPrompter, Prompter};
pub use project::{project, project_with, verify_invariant};
pub use rc_perm::{
    AllowAllChecker, BypassChecker, Decision, Mode, PermissionChecker, PermissionEngine,
    resolve_within, resolve_within_loose,
};
pub use registry::ToolRegistry;
pub use state::ReadRegistry;
pub use state::{BgShell, ChangeJournal, ChangeRecord, SharedChangeJournal, SharedShellState, ShellState};
pub use tool::{Artifact, Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
pub use turn::{AgentMode, NoteKind, Session, ToolCall, ToolResultBody, Turn};
