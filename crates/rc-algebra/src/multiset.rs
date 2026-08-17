//! Abelian-group multiset hash for incremental context hashing.
//!
//! A request's context is modeled as a *multiset of blocks*. Each block is
//! hashed to an element of an abelian group and combined with the group
//! operation:
//!
//! ```text
//! H(S ∪ {x}) = H(S) · H(x)        // append
//! H(S \ {x}) = H(S) · H(x)⁻¹      // evict
//! ```
//!
//! This gives O(1) add/evict with no rehash, and commutativity gives
//! order-independence for free — two agents that assembled the same block set
//! in different orders produce the same key.
//!
//! ## Why additive LtHash, not XOR
//!
//! A naive XOR-fold over block hashes is a homomorphism into `(GF(2)^n, ⊕)` —
//! but in that group every element is its own inverse (`x ⊕ x = 0`), so adding
//! the same block twice cancels to zero: a *set* hash that can't tell `{x, x}`
//! from `{}`. That is exactly the failure mode to avoid.
//!
//! [`LtHash`] instead lives in `((ℤ/2¹⁶)^1024, +)`: elementwise addition modulo
//! 2¹⁶. The inverse is elementwise subtraction modulo 2¹⁶, and `2x mod 2¹⁶ ≠ 0`
//! for `x ≠ 0`, so duplicates do *not* cancel. This is a genuine abelian group
//! and a real multiset hash — the additive form of Facebook's LtHash. The
//! `add_same_block_twice_is_not_zero` test pins this property: it fails loudly
//! if anyone switches the operation back to XOR.
//!
//! The inverse is the point: a monoid gets you append; a group gets you evict.

use crate::traits::{Group, Monoid};
use sha2::{Digest, Sha256};

/// 1024 limbs of 16 bits = 16384-bit group element. The accumulator state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LtHash([u16; 1024]);

impl LtHash {
    /// The identity element (all-zero). Group identity for `((ℤ/2¹⁶)^1024, +)`.
    pub const ZERO: Self = Self([0; 1024]);

    /// Add `block`'s group element: `self += H(block)` (elementwise u16 add).
    pub fn add_block(&mut self, block: &BlockId) {
        let elem = expand_to_element(block);
        for (a, b) in self.0.iter_mut().zip(elem.iter()) {
            *a = a.wrapping_add(*b);
        }
    }

    /// Remove `block`'s group element: `self -= H(block)` — the group inverse
    /// of [`add_block`]. This is the eviction primitive.
    pub fn remove(&mut self, block: &BlockId) {
        let elem = expand_to_element(block);
        for (a, b) in self.0.iter_mut().zip(elem.iter()) {
            *a = a.wrapping_sub(*b);
        }
    }

    /// The 2048-byte canonical key (little-endian limbs). Two `ContextSet`s
    /// with the same multiset of blocks compare equal here regardless of
    /// insertion order.
    pub fn as_bytes(&self) -> [u8; 2048] {
        let mut out = [0u8; 2048];
        for (i, limb) in self.0.iter().enumerate() {
            let le = limb.to_le_bytes();
            out[i * 2] = le[0];
            out[i * 2 + 1] = le[1];
        }
        out
    }
}

impl Default for LtHash {
    fn default() -> Self {
        Self::ZERO
    }
}

impl Monoid for LtHash {
    fn id() -> Self {
        Self::ZERO
    }
    fn op(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a = a.wrapping_add(*b);
        }
    }
}

impl Group for LtHash {
    fn inv_op(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a = a.wrapping_sub(*b);
        }
    }
}

/// A content address: the SHA-256 of a block's canonical bytes.
///
/// "Canonical" is whatever the caller has already made byte-stable — for a
/// `Turn` that's `rc_proto::canonical::to_bytes`. The digest is the identity of
/// a block; the *group element* it maps to is derived separately by
/// [`expand_to_element`] so that the 32-byte digest fills all 2048 bytes of
/// accumulator state without collisions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId([u8; 32]);

impl BlockId {
    /// Hash `canonical_bytes` (already byte-stable) to a `BlockId`.
    pub fn from_bytes(canonical: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(canonical);
        Self(h.finalize().into())
    }

    /// Build a `BlockId` from an already-computed 32-byte digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Expand a 32-byte [`BlockId`] to a 2048-byte (1024 × u16) group element.
///
/// Runs SHA-256 in CTR mode seeded with the digest: block `k` hashes
/// `digest || k.to_le_bytes()`, yielding 32 bytes per call; 64 calls fill 2048
/// bytes, read as 1024 little-endian u16. This is a PRG-ish mapping — distinct
/// digests spread across the full group — which is what defeats the
/// duplicate-cancellation of an XOR-fold.
pub(crate) fn expand_to_element(block: &BlockId) -> [u16; 1024] {
    let mut out = [0u16; 1024];
    let mut buf = [0u8; 2048];
    let mut counter: u32 = 0;
    let mut pos = 0;
    while pos < buf.len() {
        let mut h = Sha256::new();
        h.update(block.as_bytes());
        h.update(counter.to_le_bytes());
        let digest = h.finalize();
        let take = (buf.len() - pos).min(digest.len());
        buf[pos..pos + take].copy_from_slice(&digest[..take]);
        pos += take;
        counter += 1;
    }
    for (i, chunk) in buf.chunks_exact(2).enumerate() {
        out[i] = u16::from_le_bytes([chunk[0], chunk[1]]);
    }
    out
}

/// The order-independent multiset key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContextKey(pub [u8; 2048]);

impl ContextKey {
    /// The 2048-byte key.
    pub fn as_bytes(&self) -> &[u8; 2048] {
        &self.0
    }
}

/// A context as a multiset of blocks: the accumulator plus the live block set.
///
/// `add` is append (`H(S ∪ {x})`); [`evict`](Self::evict) is the group inverse
/// (`H(S \ {x})`). Both are O(1) on the hash; `evict` also drops the id from the
/// live set. [`context_key`](Self::context_key) is the order-independent
/// identifier — the seam the future compaction/eviction milestone consumes.
pub struct ContextSet {
    hash: LtHash,
    blocks: Vec<BlockId>,
}

impl Default for ContextSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextSet {
    /// The empty context (identity).
    pub fn new() -> Self {
        Self {
            hash: LtHash::ZERO,
            blocks: Vec::new(),
        }
    }

    /// Hash `canonical_bytes` to a [`BlockId`], append it, and return the id.
    pub fn add(&mut self, canonical_bytes: &[u8]) -> BlockId {
        let id = BlockId::from_bytes(canonical_bytes);
        self.add_block(&id);
        id
    }

    /// Append an already-computed block id.
    pub fn add_block(&mut self, id: &BlockId) {
        self.hash.add_block(id);
        self.blocks.push(id.clone());
    }

    /// Evict `id` via the group inverse. No-op if `id` is not in the set.
    pub fn evict(&mut self, id: &BlockId) {
        if let Some(pos) = self.blocks.iter().rposition(|b| b == id) {
            self.blocks.swap_remove(pos);
            self.hash.remove(id);
        }
    }

    /// The live block ids, in insertion order.
    pub fn blocks(&self) -> &[BlockId] {
        &self.blocks
    }

    /// The order-independent multiset key (the [`LtHash`] bytes).
    pub fn context_key(&self) -> ContextKey {
        ContextKey(self.hash.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_is_commutative() {
        let a = BlockId::from_bytes(b"block-a");
        let b = BlockId::from_bytes(b"block-b");
        let mut s1 = LtHash::ZERO;
        s1.add_block(&a);
        s1.add_block(&b);
        let mut s2 = LtHash::ZERO;
        s2.add_block(&b);
        s2.add_block(&a);
        assert_eq!(s1, s2, "order of addition must not matter");
    }

    #[test]
    fn add_then_remove_returns_to_zero() {
        let a = BlockId::from_bytes(b"block-a");
        let mut s = LtHash::ZERO;
        s.add_block(&a);
        s.remove(&a);
        assert_eq!(s, LtHash::ZERO, "group inverse must undo the add");
    }

    /// The load-bearing test that distinguishes additive LtHash from an
    /// XOR-fold. With XOR, `x ⊕ x = 0` so duplicates cancel and `{x, x}` looks
    /// like `{}`. With additive mod 2¹⁶, `2x mod 2¹⁶ ≠ 0`. Fails loudly if anyone
    /// switches the operation to XOR.
    #[test]
    fn add_same_block_twice_is_not_zero() {
        let a = BlockId::from_bytes(b"block-a");
        let mut s = LtHash::ZERO;
        s.add_block(&a);
        s.add_block(&a);
        assert_ne!(s, LtHash::ZERO, "additive LtHash must not cancel duplicates");
    }

    #[test]
    fn evict_by_inverse_equals_never_added() {
        let a = BlockId::from_bytes(b"block-a");
        let b = BlockId::from_bytes(b"block-b");
        let mut with_a = LtHash::ZERO;
        with_a.add_block(&a);
        with_a.add_block(&b);
        with_a.remove(&a);

        let mut without_a = LtHash::ZERO;
        without_a.add_block(&b);

        assert_eq!(
            with_a, without_a,
            "evicting a via the group inverse equals never having added it"
        );
    }

    #[test]
    fn different_block_bytes_diverge() {
        let a = BlockId::from_bytes(b"block-a");
        let b = BlockId::from_bytes(b"block-b");
        let mut sa = LtHash::ZERO;
        sa.add_block(&a);
        let mut sb = LtHash::ZERO;
        sb.add_block(&b);
        assert_ne!(sa, sb);
    }

    #[test]
    fn context_key_is_2048_bytes() {
        let mut s = ContextSet::new();
        s.add(b"hello");
        assert_eq!(s.context_key().0.len(), 2048);
    }

    #[test]
    fn context_set_order_independence() {
        let mut s1 = ContextSet::new();
        s1.add(b"a");
        s1.add(b"b");
        s1.add(b"c");
        let mut s2 = ContextSet::new();
        s2.add(b"c");
        s2.add(b"a");
        s2.add(b"b");
        assert_eq!(s1.context_key(), s2.context_key());
    }

    #[test]
    fn context_set_evict_restores_key() {
        let mut s = ContextSet::new();
        let a = s.add(b"a");
        s.add(b"b");
        let key_with_a = s.context_key();
        s.evict(&a);
        let mut expected = ContextSet::new();
        expected.add(b"b");
        assert_eq!(s.context_key(), expected.context_key());
        assert_ne!(s.context_key(), key_with_a);
    }

    #[test]
    fn group_inverse_via_subtraction() {
        let a = BlockId::from_bytes(b"x");
        let mut g = LtHash::ZERO;
        g.op(&LtHash::ZERO); // monoid id
        g.add_block(&a);
        let mut h = g.clone();
        h.inv_op(&g);
        assert_eq!(h, LtHash::ZERO, "inv_op must apply the group inverse");
    }
}
