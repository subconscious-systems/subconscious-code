//! Non-commutative sequence hash for the layer *below* the multiset hash.
//!
//! Where [`crate::multiset`] treats the context as a *set* (commutative,
//! eviction via the group inverse), a token prefix is a *sequence*: `[A, B]`
//! and `[B, A]` are different KV-cache states and must hash differently. So the
//! sequence hash lives in the deliberately opposite algebra — a polynomial hash
//! in `ℤ/p` with positional weighting, which is a *non-commutative* monoid.
//!
//! ```text
//! H(seq) = Σ_i  coeff(i) · w(block_i)   mod p
//! ```
//!
//! where `coeff(i) = BASE^i mod p`. Appending a block at position `i` is
//! `h = h + coeff(i) · w(block)`, an O(1) incremental update. Swapping two
//! distinct blocks changes the hash with overwhelming probability (~1/p).
//!
//! Same building blocks as the multiset hash, deliberately opposite algebra:
//! sets upstairs, sequences downstairs.

use crate::multiset::BlockId;
use crate::traits::Monoid;

/// Mersenne prime `p = 2⁶¹ − 1`. All arithmetic is modulo `p`.
const P: u64 = (1u64 << 61) - 1;
/// Base for positional weighting. An odd constant well below `p`; primitive
/// enough that `BASE^i` cycles through a large fraction of `ℤ/p`.
const BASE: u64 = 0x9E3779B97F4A7C15;

/// Multiply mod `p`. Uses `u128` to avoid overflow; `p < 2⁶¹` so the product of
/// two field elements fits in 128 bits.
fn mul_mod(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) % P as u128) as u64
}

fn add_mod(a: u64, b: u64) -> u64 {
    let s = a.wrapping_add(b);
    if s >= P {
        s - P
    } else {
        s
    }
}

/// `BASE^i mod p` by square-and-multiply.
fn coeff(i: usize) -> u64 {
    let mut result = 1u64;
    let mut base = BASE;
    let mut e = i;
    while e > 0 {
        if e & 1 == 1 {
            result = mul_mod(result, base);
        }
        base = mul_mod(base, base);
        e >>= 1;
    }
    result
}

/// The first 8 bytes of a [`BlockId`] read little-endian as a field element.
fn block_word(b: &BlockId) -> u64 {
    let bytes = b.as_bytes();
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf) % P
}

/// A polynomial hash accumulator in `ℤ/p` — the non-commutative monoid for
/// sequences. Append-only; the position is the current length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeqHash {
    acc: u64,
    len: usize,
}

impl SeqHash {
    /// The empty sequence (identity).
    pub const EMPTY: Self = Self { acc: 0, len: 0 };

    /// Append `block` at the current position.
    pub fn append(&mut self, block: &BlockId) {
        let term = mul_mod(coeff(self.len), block_word(block));
        self.acc = add_mod(self.acc, term);
        self.len += 1;
    }

    /// Append many blocks, incrementing the position each time.
    pub fn extend(&mut self, blocks: &[BlockId]) {
        for b in blocks {
            self.append(b);
        }
    }

    /// The sequence fingerprint.
    pub fn as_u64(&self) -> u64 {
        self.acc
    }

    /// Number of blocks hashed.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no blocks have been hashed.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for SeqHash {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Monoid for SeqHash {
    fn id() -> Self {
        Self::EMPTY
    }
    /// Concatenation: `self`'s blocks followed by `other`'s blocks, reweighted
    /// so `other`'s contribution shifts by `coeff(self.len)`.
    fn op(&mut self, other: &Self) {
        let shift = coeff(self.len);
        let mut carried = other.acc;
        // carried is Σ_i coeff_other(i)·w_i in [0, p). Shifting positions by
        // self.len multiplies each coeff by BASE^self.len, i.e. scales the whole
        // sum by `shift` mod p.
        carried = mul_mod(carried, shift);
        self.acc = add_mod(self.acc, carried);
        self.len += other.len;
    }
}

/// A fingerprint over an ordered sequence of blocks — the KV-cache-state
/// identity. Two sequences with the same `PrefixFingerprint` share a prefix
/// boundary; a divergence marks where the provider's prefix cache must miss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixFingerprint {
    seq: SeqHash,
}

impl PrefixFingerprint {
    pub fn new() -> Self {
        Self {
            seq: SeqHash::EMPTY,
        }
    }

    /// Build the fingerprint over a whole block sequence in one pass.
    pub fn from_blocks(blocks: &[BlockId]) -> Self {
        let mut s = SeqHash::EMPTY;
        s.extend(blocks);
        Self { seq: s }
    }

    /// Incrementally append a block.
    pub fn extend(&mut self, block: &BlockId) {
        self.seq.append(block);
    }

    /// The fingerprint value.
    pub fn fingerprint(&self) -> u64 {
        self.seq.as_u64()
    }

    /// Number of blocks hashed.
    pub fn len(&self) -> usize {
        self.seq.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seq.is_empty()
    }
}

impl Default for PrefixFingerprint {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bid(b: &[u8]) -> BlockId {
        BlockId::from_bytes(b)
    }

    #[test]
    fn append_is_non_commutative() {
        let a = bid(b"aaaa");
        let b = bid(b"bbbb");
        let mut ab = SeqHash::EMPTY;
        ab.append(&a);
        ab.append(&b);
        let mut ba = SeqHash::EMPTY;
        ba.append(&b);
        ba.append(&a);
        assert_ne!(ab, ba, "[A,B] and [B,A] must differ — KV states differ");
    }

    #[test]
    fn extend_incremental_equals_from_scratch() {
        let blocks = [bid(b"a"), bid(b"b"), bid(b"c"), bid(b"d")];
        let mut inc = SeqHash::EMPTY;
        inc.extend(&blocks);
        let one = SeqHash::EMPTY;
        let one_shot = {
            let mut s = one;
            s.extend(&blocks);
            s
        };
        assert_eq!(inc, one_shot);
    }

    #[test]
    fn swapped_blocks_diverge() {
        let a = bid(b"block-a");
        let b = bid(b"block-b");
        let ab = PrefixFingerprint::from_blocks(&[a.clone(), b.clone()]);
        let ba = PrefixFingerprint::from_blocks(&[b, a]);
        assert_ne!(ab.fingerprint(), ba.fingerprint());
    }

    #[test]
    fn prefix_fingerprint_stable_across_rebuilds() {
        let blocks = [bid(b"x"), bid(b"y"), bid(b"z")];
        let f1 = PrefixFingerprint::from_blocks(&blocks);
        let f2 = PrefixFingerprint::from_blocks(&blocks);
        assert_eq!(f1.fingerprint(), f2.fingerprint());
    }

    #[test]
    fn monoid_concatenation_matches_sequential_append() {
        let lhs = [bid(b"a"), bid(b"b")];
        let rhs = [bid(b"c"), bid(b"d")];
        let mut concat = SeqHash::EMPTY;
        concat.extend(&lhs);
        let mut rhs_seq = SeqHash::EMPTY;
        rhs_seq.extend(&rhs);
        concat.op(&rhs_seq);

        let mut all = SeqHash::EMPTY;
        all.extend(&lhs);
        all.extend(&rhs);
        assert_eq!(concat, all, "monoid op must equal sequential append");
    }
}
