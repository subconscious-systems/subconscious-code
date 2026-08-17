//! rc-algebra: the algebraic structures underpinning the context/cache and
//! accounting layer.
//!
//! The harness leans on two deliberately opposite algebras, both defined here:
//!
//! - **Sets upstairs** ([`multiset`]): a context is a multiset of blocks; add
//!   and evict are the operation and inverse of an abelian group, giving O(1)
//!   eviction with order-independence. This is the real group-theory win — it
//!   lands directly on the content-addressed context protocol.
//! - **Sequences downstairs** ([`seqhash`]): the token prefix is a sequence;
//!   its hash is a *non-commutative* polynomial monoid, because `[A, B]` and
//!   `[B, A]` are different KV-cache states.
//!
//! Same building blocks, deliberately opposite algebra.
//!
//! On top of these:
//!
//! - [`orbit`] — group actions and orbit canonicalization (collapse the `Sₙ`
//!   orbit of independent blocks onto one cache key), plus Burnside/Pólya
//!   orbit-count estimation.
//! - [`crdt`] — a join-semilattice for a future distributed radix replicator
//!   (CRDT convergence under any gossip order; LWW-epoch tombstones for
//!   eviction, since eviction is not monotone).
//! - [`rewriting`] — a minimal Knuth–Bendix confluent-rewriting engine for
//!   future canonicalizers with more than a handful of rules.
//!
//! The accounting monoid (`Cost`, integer micro-USD) lives in `rc-core`, not
//! here — it bridges the wire `Usage` to display and has no need for crypto.

pub mod crdt;
pub mod multiset;
pub mod orbit;
pub mod rewriting;
pub mod seqhash;
pub mod traits;

pub use multiset::{BlockId, ContextKey, ContextSet, LtHash};
pub use orbit::{
    burnside_orbit_count, canonical_representative, content_fingerprint, orbit_divergence,
    OrbitDivergence,
};
pub use rewriting::{ConfluenceReport, CriticalPair, RewriteSystem, Rule, Symbol};
pub use seqhash::{PrefixFingerprint, SeqHash};
pub use traits::{Group, GroupAction, Monoid, Semilattice};
