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
        add_assign(&mut self.0, &elem);
    }

    /// Remove `block`'s group element: `self -= H(block)` — the group inverse
    /// of [`add_block`]. This is the eviction primitive.
    pub fn remove(&mut self, block: &BlockId) {
        let elem = expand_to_element(block);
        sub_assign(&mut self.0, &elem);
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
        add_assign(&mut self.0, &other.0);
    }
}

impl Group for LtHash {
    fn inv_op(&mut self, other: &Self) {
        sub_assign(&mut self.0, &other.0);
    }
}

#[inline]
#[cfg(not(target_arch = "aarch64"))]
fn add_assign_scalar(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    for (a, b) in lhs.iter_mut().zip(rhs) {
        *a = a.wrapping_add(*b);
    }
}

#[inline]
#[cfg(not(target_arch = "aarch64"))]
fn sub_assign_scalar(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    for (a, b) in lhs.iter_mut().zip(rhs) {
        *a = a.wrapping_sub(*b);
    }
}

// AArch64 guarantees NEON, so there is no runtime dispatch on that target.
#[cfg(target_arch = "aarch64")]
#[inline]
fn add_assign(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    // SAFETY: AArch64 guarantees Advanced SIMD and both operands are fixed at
    // 2048 bytes, exactly the range consumed by the assembly loop.
    unsafe { add_assign_neon_asm(lhs, rhs) }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn sub_assign(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    // SAFETY: same fixed-size and ISA guarantees as `add_assign_neon_asm`.
    unsafe { sub_assign_neon_asm(lhs, rhs) }
}

/// Four-vector AArch64/NEON wrapping-add kernel. Keeping the complete loop in
/// one assembly block fixes the unroll factor and removes bounds, iterator,
/// and per-vector loop bookkeeping from this 2048-byte primitive.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn add_assign_neon_asm(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    use std::arch::asm;

    let lhs_ptr = lhs.as_mut_ptr();
    let rhs_ptr = rhs.as_ptr();
    let end = lhs_ptr.add(1024);

    asm!(
        "2:",
        "ldp q0, q1, [{lhs}]",
        "ldp q2, q3, [{lhs}, #32]",
        "ldp q4, q5, [{rhs}]",
        "ldp q6, q7, [{rhs}, #32]",
        "add v0.8h, v0.8h, v4.8h",
        "add v1.8h, v1.8h, v5.8h",
        "add v2.8h, v2.8h, v6.8h",
        "add v3.8h, v3.8h, v7.8h",
        "stp q0, q1, [{lhs}]",
        "stp q2, q3, [{lhs}, #32]",
        "add {lhs}, {lhs}, #64",
        "add {rhs}, {rhs}, #64",
        "cmp {lhs}, {end}",
        "b.lo 2b",
        lhs = inout(reg) lhs_ptr => _,
        rhs = inout(reg) rhs_ptr => _,
        end = in(reg) end,
        out("v0") _,
        out("v1") _,
        out("v2") _,
        out("v3") _,
        out("v4") _,
        out("v5") _,
        out("v6") _,
        out("v7") _,
        options(nostack),
    );
}

/// Four-vector AArch64/NEON wrapping-subtract twin of
/// [`add_assign_neon_asm`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn sub_assign_neon_asm(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    use std::arch::asm;

    let lhs_ptr = lhs.as_mut_ptr();
    let rhs_ptr = rhs.as_ptr();
    let end = lhs_ptr.add(1024);

    asm!(
        "2:",
        "ldp q0, q1, [{lhs}]",
        "ldp q2, q3, [{lhs}, #32]",
        "ldp q4, q5, [{rhs}]",
        "ldp q6, q7, [{rhs}, #32]",
        "sub v0.8h, v0.8h, v4.8h",
        "sub v1.8h, v1.8h, v5.8h",
        "sub v2.8h, v2.8h, v6.8h",
        "sub v3.8h, v3.8h, v7.8h",
        "stp q0, q1, [{lhs}]",
        "stp q2, q3, [{lhs}, #32]",
        "add {lhs}, {lhs}, #64",
        "add {rhs}, {rhs}, #64",
        "cmp {lhs}, {end}",
        "b.lo 2b",
        lhs = inout(reg) lhs_ptr => _,
        rhs = inout(reg) rhs_ptr => _,
        end = in(reg) end,
        out("v0") _,
        out("v1") _,
        out("v2") _,
        out("v3") _,
        out("v4") _,
        out("v5") _,
        out("v6") _,
        out("v7") _,
        options(nostack),
    );
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn add_assign(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected and the kernel stays inside both arrays.
        unsafe { add_assign_avx2(lhs, rhs) }
    } else {
        add_assign_scalar(lhs, rhs)
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn sub_assign(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected and the kernel stays inside both arrays.
        unsafe { sub_assign_avx2(lhs, rhs) }
    } else {
        sub_assign_scalar(lhs, rhs)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn add_assign_avx2(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    use std::arch::x86_64::{__m256i, _mm256_add_epi16, _mm256_loadu_si256, _mm256_storeu_si256};

    for i in (0..1024).step_by(16) {
        let a = _mm256_loadu_si256(lhs.as_ptr().add(i).cast::<__m256i>());
        let b = _mm256_loadu_si256(rhs.as_ptr().add(i).cast::<__m256i>());
        _mm256_storeu_si256(
            lhs.as_mut_ptr().add(i).cast::<__m256i>(),
            _mm256_add_epi16(a, b),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sub_assign_avx2(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    use std::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256, _mm256_sub_epi16};

    for i in (0..1024).step_by(16) {
        let a = _mm256_loadu_si256(lhs.as_ptr().add(i).cast::<__m256i>());
        let b = _mm256_loadu_si256(rhs.as_ptr().add(i).cast::<__m256i>());
        _mm256_storeu_si256(
            lhs.as_mut_ptr().add(i).cast::<__m256i>(),
            _mm256_sub_epi16(a, b),
        );
    }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline]
fn add_assign(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    add_assign_scalar(lhs, rhs)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline]
fn sub_assign(lhs: &mut [u16; 1024], rhs: &[u16; 1024]) {
    sub_assign_scalar(lhs, rhs)
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
/// Uses BLAKE3's domain-separated XOF to fill 2048 bytes in one streaming
/// operation, then reads those bytes as 1024 little-endian u16s. BLAKE3
/// dispatches to its optimized SIMD/assembly implementation at runtime.
pub(crate) fn expand_to_element(block: &BlockId) -> [u16; 1024] {
    let mut out = [0u16; 1024];
    let mut buf = [0u8; 2048];
    let mut h = blake3::Hasher::new_derive_key("subconscious-code LtHash element v1");
    h.update(block.as_bytes());
    h.finalize_xof().fill(&mut buf);
    for (i, chunk) in buf.as_chunks::<2>().0.iter().enumerate() {
        out[i] = u16::from_le_bytes(*chunk);
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
    use std::hint::black_box;
    use std::time::Instant;

    /// Manual release-mode microbenchmark for the two LtHash kernels. Kept
    /// ignored so ordinary test runs stay deterministic and fast:
    /// `cargo test -p rc-algebra --release bench_lthash_kernels -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_lthash_kernels() {
        const EXPAND_ITERS: usize = 20_000;
        const FOLD_ITERS: usize = 1_000_000;
        let id = BlockId::from_bytes(b"representative context block");

        let mut expanded = LtHash::ZERO;
        let start = Instant::now();
        for _ in 0..EXPAND_ITERS {
            expanded.add_block(black_box(&id));
        }
        let expand_elapsed = start.elapsed();

        let rhs = expanded.clone();
        let mut folded = LtHash::ZERO;
        let start = Instant::now();
        for _ in 0..FOLD_ITERS {
            folded.op(black_box(&rhs));
        }
        let fold_elapsed = start.elapsed();

        black_box((expanded, folded));
        eprintln!(
            "LtHash add_block: {:.1} ns/op; fold: {:.1} ns/op",
            expand_elapsed.as_nanos() as f64 / EXPAND_ITERS as f64,
            fold_elapsed.as_nanos() as f64 / FOLD_ITERS as f64,
        );
    }

    #[test]
    fn vector_kernels_match_wrapping_scalar_arithmetic() {
        let mut source = [0u16; 1024];
        let mut rhs = [0u16; 1024];
        for i in 0..1024 {
            source[i] = (i as u16).wrapping_mul(251).wrapping_add(65_000);
            rhs[i] = (i as u16).wrapping_mul(509).wrapping_add(1_000);
        }

        let expected_add = std::array::from_fn(|i| source[i].wrapping_add(rhs[i]));
        let expected_sub = std::array::from_fn(|i| source[i].wrapping_sub(rhs[i]));

        let mut actual = source;
        add_assign(&mut actual, &rhs);
        assert_eq!(actual, expected_add);

        actual = source;
        sub_assign(&mut actual, &rhs);
        assert_eq!(actual, expected_sub);
    }

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
        assert_ne!(
            s,
            LtHash::ZERO,
            "additive LtHash must not cancel duplicates"
        );
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
