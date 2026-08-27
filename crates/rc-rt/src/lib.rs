//! rc-rt: the event transport + driver/pump runtime (§12, M4a).
//!
//! The layer above rc-core that turns the headless `AgentLoop` into an
//! interactive runtime a UI (or a test harness) can drive asynchronously:
//!
//! ```text
//!   host (TUI/test) --UserAction-->  pump  --DriverCmd-->  driver --> AgentLoop
//!        ^                             |                    |          |
//!       AgentEvent (broadcast)   pending-asks         EventSink / Prompter
//! ```
//!
//! The host *observes* via a [`tokio::sync::broadcast::Receiver`] of
//! [`AgentEvent`] and *acts* via the sync [`Runtime::action`]; it never calls
//! into core synchronously. rc-core stays a plain library with no channel deps;
//! this crate owns the channels, the driver (one turn at a time), and the pump
//! (translates actions to commands, owns the per-turn cancel token, resolves
//! pending asks).
//!
//! At most one permission Ask is ever pending: rc-core's `execute_batch` runs
//! all permission checks serially before spawning any tool, so an Ask and a
//! tool run never overlap — the pump's `Cancel` can drain the (≥0) pending asks
//! without racing a tool.

mod action;
mod driver;
mod event;
mod prompter;
mod pump;
mod runtime;
mod sink;

pub use action::UserAction;
pub use event::AgentEvent;
pub use rc_session::SessionStore;
pub use runtime::{EventStream, Runtime, RuntimeControl};
