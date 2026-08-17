//! Algebraic-structure traits the rest of `rc-algebra` builds on.
//!
//! These are deliberately tiny marker-plus-operation traits. They exist so the
//! load-bearing algebraic properties of the context/cache/accounting layer can
//! be *named* in the type system and asserted in tests, not to model category
//! theory — a `Group` here is "a monoid where every element has an inverse",
//! because that inverse is exactly what turns a multiset-hash accumulator from
//! append-only into one that supports eviction.
//!
//! The two opposite algebras the harness relies on both live behind these
//! traits:
//!
//! - **Sets upstairs** — a context is a *multiset of blocks*; add/remove is the
//!   group operation/inverse of an abelian group, giving O(1) eviction with
//!   order-independence (see [`crate::multiset`]).
//! - **Sequences downstairs** — the token prefix is a *sequence*; its hash is a
//!   non-commutative (polynomial, positionally weighted) monoid, because
//!   `[A, B]` and `[B, A]` are different KV-cache states (see [`crate::seqhash`]).
//!
//! Same building blocks, deliberately opposite algebra.

/// A monoid: an associative binary operation with an identity.
///
/// `op` must be associative: `op(op(a, b), c) == op(a, op(b, c))`, and `id()` is
/// a left and right identity. The point of naming this: float addition is *not*
/// associative, so a float cost accumulator is not a monoid and sharded/parallel
/// reduction over it is order-dependent. Integer micro-units restore the
/// monoid — see `rc_core::cost::Cost`.
pub trait Monoid {
    /// The identity element.
    fn id() -> Self;
    /// The associative binary operation. Mutates `self` in place for
    /// ergonomics in the accumulator use case (`self.add(other)`).
    fn op(&mut self, other: &Self);
}

/// A group: a monoid where every element has an inverse.
///
/// The inverse is the whole reason a multiset hash can evict: a monoid gets you
/// append (`H(S ∪ {x}) = H(S) · H(x)`); a group gets you remove
/// (`H(S \ {x}) = H(S) · H(x)⁻¹`). `inv_op` applies the inverse of `other`.
pub trait Group: Monoid {
    /// Apply the inverse of `other`: `self · other⁻¹`.
    fn inv_op(&mut self, other: &Self);
}

/// A join-semilattice: a join (`∨`) that is idempotent, commutative, and
/// associative. This is precisely the CRDT convergence condition — gossip in
/// any order, no coordination, guaranteed convergence to the same fixed point.
/// See [`crate::crdt::PrefixSet`].
pub trait Semilattice {
    /// The join. Must satisfy idempotence (`a ∨ a == a`), commutativity
    /// (`a ∨ b == b ∨ a`), and associativity (`(a ∨ b) ∨ c == a ∨ (b ∨ c)`).
    fn join(&self, other: &Self) -> Self;
}

/// A group action of group `G` on a set `X`: `g · x`. The motivating case is
/// `Sₙ` (permutations) acting on a sequence of independent blocks — each
/// permutation is a distinct point in the orbit, and picking a canonical
/// representative collapses the whole orbit onto one cache key. See
/// [`crate::orbit`].
pub trait GroupAction {
    type Group;
    type Set;
    /// Apply `g` to `x`.
    fn act(g: &Self::Group, x: &Self::Set) -> Self::Set;
}
