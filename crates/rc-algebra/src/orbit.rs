//! Group actions and orbit canonicalization for cache hit rate.
//!
//! A bunch of things in an agentic harness differ only by a symmetry that a
//! prefix-cache router can't see through: tool/MCP definition ordering in the
//! request's `tools` array, file ordering in a context bundle, JSON key
//! ordering, whitespace. Each permutation is a distinct point in the orbit of
//! `Sₙ` acting on independent blocks, and each one is a distinct trie path → a
//! prefix miss from the first byte.
//!
//! Picking a *canonical orbit representative* — stable sort the blocks by
//! content hash — collapses the whole orbit onto one cache key. At high cache
//! hit rates you're chasing the tail, and non-deterministic iteration order in
//! a client SDK (or nondeterministic MCP connect order) is a classic source of
//! that tail. [`orbit_divergence`] logs the canonical key alongside the raw key
//! so you can measure how often they would have diverged.
//!
//! This module deliberately applies only to *order-independent* blocks — e.g.
//! the `tools` array, where two sessions registering the same tool set in
//! different orders are semantically identical but byte-distinct. It must not
//! be applied to semantically-ordered sequences (conversation turns, the
//! memory precedence chain, user-authored `@file` mention order) — those live in
//! the non-commutative world of [`crate::seqhash`].

use crate::multiset::BlockId;
use sha2::{Digest, Sha256};

/// Reorder `blocks` in place into the canonical orbit representative: stable
/// sort by content hash. Idempotent — calling it twice is the same as calling
/// it once. `key` extracts the order-determining content from each block; for a
/// `ToolDefinition` that's its canonical serialized bytes.
pub fn canonical_representative<T>(blocks: &mut [T], key: impl Fn(&T) -> BlockId) {
    blocks.sort_by_key(key);
}

/// Instrumentation: compute both the raw-order key and the canonical-order key
/// and report whether they diverged. The `raw`/`canonical` fields are short
/// 64-bit fingerprints of the two orderings, for cheap comparison in logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrbitDivergence {
    /// `true` if the raw insertion order was already canonical; `false` means a
    /// reorder would have changed the bytes (a would-be prefix miss).
    pub already_canonical: bool,
    pub raw: u64,
    pub canonical: u64,
}

fn fingerprint_order<T>(blocks: &[T], key: impl Fn(&T) -> BlockId) -> u64 {
    // FNV-1a over the concatenated content hashes, in the given order. This is
    // *only* a comparison token for the divergence log — never a cache key.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in blocks {
        let id = key(b);
        for &byte in id.as_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// Compute the orbit divergence for `blocks` without mutating them.
pub fn orbit_divergence<T>(blocks: &[T], key: impl Fn(&T) -> BlockId) -> OrbitDivergence
where
    T: Clone,
{
    let raw = fingerprint_order(blocks, &key);
    let mut sorted = blocks.to_vec();
    canonical_representative(&mut sorted, &key);
    let canonical = fingerprint_order(&sorted, &key);
    OrbitDivergence {
        already_canonical: raw == canonical,
        raw,
        canonical,
    }
}

/// Burnside's lemma: the number of orbits of a finite group `G` acting on a set
/// is `|G|⁻¹ Σ_{g∈G} |Fix(g)|`. For estimating the dedup gain of a
/// canonicalization *before* building it, you pass the cycle structure of each
/// group element (the lengths of the disjoint cycles it permutes); the fixed
/// points are `n - (moved by the cycle)` ... but the practically useful,
/// ignorance-free form is: given the cycle structures present in the acting
/// group, return the orbit count.
///
/// `cycle_structures` is one entry per group element; each entry is the list of
/// cycle lengths in that element's permutation (including 1-cycles for fixed
/// points). The number of fixed points of a permutation with cycles
/// `[c1, c2, …]` acting on `n` labelled points is the number of 1-cycles. The
/// orbit count is the average number of fixed points across the group.
pub fn burnside_orbit_count(cycle_structures: &[Vec<usize>]) -> u64 {
    if cycle_structures.is_empty() {
        return 0;
    }
    let total_fixed: u64 = cycle_structures
        .iter()
        .map(|cycles| cycles.iter().copied().filter(|&c| c == 1).count() as u64)
        .sum();
    total_fixed / cycle_structures.len() as u64
}

/// A short 64-bit content fingerprint for a block's canonical bytes — a
/// convenience for callers that want a cache-key fragment without keeping the
/// full 32-byte digest around.
pub fn content_fingerprint(canonical_bytes: &[u8]) -> u64 {
    let mut h = Sha256::new();
    h.update(canonical_bytes);
    let digest = h.finalize();
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<BlockId> {
        items
            .iter()
            .map(|s| BlockId::from_bytes(s.as_bytes()))
            .collect()
    }

    #[test]
    fn canonical_representative_is_idempotent() {
        let mut a = ids(&["c", "a", "b"]);
        canonical_representative(&mut a, |b| b.clone());
        let once = a.clone();
        canonical_representative(&mut a, |b| b.clone());
        assert_eq!(a, once);
    }

    #[test]
    fn two_orders_collapse_to_same_representative() {
        let mut abc = ids(&["a", "b", "c"]);
        let mut cab = ids(&["c", "a", "b"]);
        canonical_representative(&mut abc, |b| b.clone());
        canonical_representative(&mut cab, |b| b.clone());
        assert_eq!(abc, cab, "different insertion orders must collapse");
    }

    #[test]
    fn orbit_divergence_detects_reorder() {
        let blocks = ids(&["c", "a", "b"]);
        let div = orbit_divergence(&blocks, |b| b.clone());
        assert!(!div.already_canonical);
        assert_ne!(div.raw, div.canonical);
    }

    #[test]
    fn orbit_divergence_canonical_input_is_noop() {
        let mut blocks = ids(&["a", "b", "c"]);
        canonical_representative(&mut blocks, |b| b.clone());
        let div = orbit_divergence(&blocks, |b| b.clone());
        assert!(div.already_canonical);
        assert_eq!(div.raw, div.canonical);
    }

    #[test]
    fn burnside_identity_action_has_n_orbits() {
        // The trivial group (just the identity) acting on 3 points: the
        // identity fixes all 3, so there are 3 orbits (no collapse).
        let cycles = vec![vec![1, 1, 1]];
        assert_eq!(burnside_orbit_count(&cycles), 3);
    }

    #[test]
    fn burnside_full_symmetric_group_one_orbit() {
        // S_3 acting on 3 points: identity fixes 3, the three transpositions
        // each fix 1, the two 3-cycles fix 0. Orbits = (3 + 3*1 + 2*0)/6 = 1.
        let cycles = vec![
            vec![1, 1, 1],
            vec![2, 1],
            vec![2, 1],
            vec![2, 1],
            vec![3],
            vec![3],
        ];
        assert_eq!(burnside_orbit_count(&cycles), 1);
    }
}
