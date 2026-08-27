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
use rc_algebra::multiset::ContextKey;
use rc_algebra::seqhash::PrefixFingerprint;
use rc_proto::WireMessage;
use std::path::Path;

/// Assemble the wire messages for the next model request (§4.1 + §4.6).
///
/// Implementations build the §4.6 system prompt (identity + environment +
/// memory chain), expand `@file` mentions, and truncate oversized tool
/// results, then project the turn list. When `None` is supplied to the loop,
/// the legacy [`crate::project::project`] path is used (M1–M5 behavior).
pub trait ContextAssembler: Send + Sync {
    /// Project `turns` to the wire messages for the next request.
    fn assemble(&self, turns: &[Turn]) -> Vec<WireMessage>;

    /// Project using the live session working directory. Implementations that
    /// embed cwd/git/memory state may refresh it here; legacy/static assemblers
    /// keep their existing behavior through this default.
    fn assemble_for(&self, turns: &[Turn], _cwd: &Path) -> Vec<WireMessage> {
        self.assemble(turns)
    }

    /// The assembled §4.6 system prompt, if this assembler produces one.
    /// Exposed for display (the TUI status line) and debugging; the loop does
    /// not require it. `None` for assemblers that defer to the legacy path.
    fn system_prompt(&self) -> Option<&str> {
        None
    }

    /// The content-addressed key of the assembled context as a *multiset* of
    /// blocks — an abelian-group hash (LtHash in `(ℤ/2¹⁶)^1024`). This is the
    /// "sets upstairs" algebra: order-independent (two agents that assembled
    /// the same block set in different orders produce the same key) and
    /// O(1) add/evict via the group operation and its inverse.
    ///
    /// This is the seam for the content-addressed context protocol: a future
    /// cache/eviction layer keys on it. `None` for assemblers that don't
    /// content-address (the legacy path).
    fn context_key(&self, _turns: &[Turn]) -> Option<ContextKey> {
        None
    }

    /// The non-commutative fingerprint of the assembled message *sequence* —
    /// a polynomial hash in `ℤ/p` with positional weighting. This is the
    /// "sequences downstairs" algebra, deliberately opposite to
    /// [`Self::context_key`]: `[A, B]` and `[B, A]` are different KV-cache
    /// states, so the prefix hash must *not* commute. The seam for a future
    /// KV-cache-state cache key.
    fn prefix_fingerprint(&self, _turns: &[Turn]) -> Option<PrefixFingerprint> {
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
