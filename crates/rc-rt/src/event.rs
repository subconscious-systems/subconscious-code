//! `AgentEvent` — the observation stream a host (TUI or test harness) reads
//! from the runtime over a `broadcast` channel.
//!
//! Streaming deltas and tool lifecycle are forwarded from the loop by
//! [`crate::sink::RuntimeSink`]; permission ask/decision come from the async
//! prompter ([`crate::prompter`]); turn boundaries (`Ready`/`Idle`/`Outcome`/
//! `Error`) and `ModeChanged` come from the driver/pump. Nothing here is
//! emitted by rc-core directly — the runtime is the only producer.

use rc_core::{AgentMode, AskResponse, LoopOutcome, ToolCall, ToolResultBody, Usage};
use serde_json::Value;

/// One observable agent event. `Clone` is required by `tokio::sync::broadcast`.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Assistant text delta (streaming).
    Text(String),
    /// Reasoning delta (streaming).
    Reasoning(String),
    /// A tool call is about to run (announced by the model stream).
    ToolStart { call: ToolCall },
    /// A tool call finished — one per batch item, including denied /
    /// parse-error / unknown-tool results, so the host always observes a
    /// terminal state for every announced call.
    ToolEnd { call_id: String, tool: String, result: ToolResultBody },
    /// A tool call escalated to Ask; the async prompter is awaiting an answer
    /// (a `UserAction::PermissionAnswer` with this `id`).
    PermissionAsk { id: u64, tool: String, input: Value, reason: String },
    /// The prompter got an answer (or the ask was cancelled → `Deny`).
    PermissionDecision { id: u64, response: AskResponse },
    /// Top of a loop iteration.
    Iter { count: u32, max: u32 },
    /// Token usage from the model response.
    Usage(Usage),
    /// M8: size of the context about to be sent — char length and the
    /// calibrated token estimate. Informational: there is no window to exceed,
    /// but the operator wants to watch it grow.
    Context { chars: usize, est_tokens: usize },
    /// A turn completed normally.
    Outcome(LoopOutcome),
    /// A turn failed (model error).
    Error(String),
    /// The active mode changed (Shift+Tab). Emitted as soon as the engine's
    /// mode is swapped — the next permission check sees it.
    ModeChanged(AgentMode),
    /// A turn is starting (driver).
    Ready,
    /// A turn finished (driver) — the runtime is ready for the next submit.
    Idle,
    /// A host-side notice from the driver (e.g. `/rewind` outcome). Rendered as
    /// a system line, never injected into the model's prompt.
    Notice(String),
}
