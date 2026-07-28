//! The context-assembly seam (§4.6 / §8). `rc-core` defines the trait; the
//! `rc-ctx` crate provides the real implementation and the composition root
//! (rc-cli) wires it into the [`crate::agent::AgentLoop`].
//!
//! The dependency direction is deliberate: `rc-ctx → rc-core`, never the
//! reverse. Keeping the trait here lets the loop stay in `rc-core` (the testable
//! core) while the §4.6 system prompt + `@file` expansion + tool-output
//! truncation live in a separate crate that can pull in more deps without
//! weighing down the core.

use crate::turn::Turn;
use rc_proto::WireMessage;

/// Assemble the wire messages for the next model request (§4.1 + §4.6).
///
/// Implementations build the §4.6 system prompt (identity + environment +
/// memory chain), expand `@file` mentions, and truncate oversized tool
/// results, then project the turn list. When `None` is supplied to the loop,
/// the legacy [`crate::project::project`] path is used (M1–M5 behavior).
pub trait ContextAssembler: Send + Sync {
    /// Project `turns` to the wire messages for the next request.
    fn assemble(&self, turns: &[Turn]) -> Vec<WireMessage>;

    /// The assembled §4.6 system prompt, if this assembler produces one.
    /// Exposed for display (the TUI status line) and debugging; the loop does
    /// not require it. `None` for assemblers that defer to the legacy path.
    fn system_prompt(&self) -> Option<&str> {
        None
    }
}

/// A trivial assembler that forwards to the legacy [`crate::project::project`]
/// — the M1–M5 behavior. Used when no `rc-ctx` assembler is wired in.
#[derive(Default)]
pub struct LegacyAssembler;

impl ContextAssembler for LegacyAssembler {
    fn assemble(&self, turns: &[Turn]) -> Vec<WireMessage> {
        crate::project::project(turns)
    }
}
