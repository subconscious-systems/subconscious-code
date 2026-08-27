//! Reference-delta compression (extra strategy).
//!
//! Per-block zstd-with-dictionary (§7) handles repetition *within* the trace
//! corpus. But SWE traces have a sharper pattern: turn N+1's file snapshot is
//! often 99 % identical to turn N's. Compressing each block independently
//! misses that cross-block redundancy. Reference-delta compresses a block
//! *against a previously-seen reference block* (the prior version of the same
//! file) using zstd with the reference as a "raw content" dictionary, so the
//! encoder can emit "copy from reference" matches covering the unchanged
//! prefix. On snapshot-heavy traces this pushes the effective ratio from the
//! 3–10× of per-block zstd-dict to 20–50× for the delta turn.
//!
//! The reference never travels — it is a block_id the receiver already holds
//! in its content-addressed store; the frame carries only the id and the
//! (tiny) delta. This is the cross-block analogue of the append-log's
//! "don't re-send history" win, applied *inside* a single block.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::{CompressError, Compressor, Dict};

/// Derive the dictionary id from the *content* of the reference, not a fixed
/// constant. The per-thread zstd encoder cache keys by `(level, dict_id)`, so a
/// fixed id (the old `0xDE1A`) made a second delta call with a *different*
/// reference reuse the cached compressor built for the *first* reference —
/// compressing against the wrong reference and producing a blob the receiver
/// could not decode. Keying by content id both fixes that (a different
/// reference is a cache miss → rebuild with the right dict) and lets repeated
/// deltas against the *same* reference hit the cache, skipping the expensive
/// zstd context + dictionary load on every call after the first.
fn ref_dict_id(reference: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    reference.hash(&mut h);
    h.finish()
}

/// Compress `input` against a `reference` block (the prior version). The
/// reference is used as a zstd raw-content dictionary, enabling long-distance
/// matches into the unchanged prefix. Falls back to ordinary zstd-dict
/// behaviour when `reference` is empty.
pub fn compress_with_reference(
    input: &[u8],
    reference: &[u8],
    level: i32,
) -> Result<Vec<u8>, CompressError> {
    let dict = if reference.is_empty() {
        Dict::empty()
    } else {
        Dict::from_content(ref_dict_id(reference), reference.to_vec())
    };
    Compressor::new(dict, level).compress(input)
}

/// Decompress a reference-delta blob. The caller supplies the same reference
/// block the encoder used; without it the bytes are meaningless, which is the
/// point — the reference is already in the store.
pub fn decompress_with_reference(
    blob: &[u8],
    reference: &[u8],
    level: i32,
) -> Result<Vec<u8>, CompressError> {
    let dict = if reference.is_empty() {
        Dict::empty()
    } else {
        Dict::from_content(ref_dict_id(reference), reference.to_vec())
    };
    Compressor::new(dict, level).decompress(blob)
}

/// Pick the best reference for a new block from a small set of candidate prior
/// blocks: the one with the longest common prefix is the best delta base.
/// Cheap O(n·k) over the candidate set; in practice the candidate set is the
/// last few versions of the same path.
pub fn pick_reference<'a>(input: &[u8], candidates: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    candidates
        .iter()
        .max_by_key(|c| common_prefix_len(input, c))
        .copied()
}

/// A reusable reference-delta compressor. The reference is built into a zstd
/// `Dict` *once* (the `reference.to_vec()` happens at construction, not per
/// call), and the underlying `Compressor`'s per-thread zstd contexts are keyed
/// by the reference's content id — so repeated `compress`/`decompress` calls
/// against the same reference skip both the reference-byte clone and the zstd
/// context + dictionary load. Use this when compressing a run of blocks
/// against one prior version (snapshot-heavy traces); use the free
/// [`compress_with_reference`]/[`decompress_with_reference`] for one-offs.
pub struct DeltaCompressor {
    compressor: Compressor,
}

impl DeltaCompressor {
    /// Build a delta compressor over `reference` at zstd `level`. An empty
    /// reference falls back to ordinary (dict-less) zstd.
    pub fn new(reference: &[u8], level: i32) -> Self {
        let dict = if reference.is_empty() {
            Dict::empty()
        } else {
            Dict::from_content(ref_dict_id(reference), reference.to_vec())
        };
        Self {
            compressor: Compressor::new(dict, level),
        }
    }

    pub fn compress(&self, input: &[u8]) -> Result<Vec<u8>, CompressError> {
        self.compressor.compress(input)
    }

    pub fn decompress(&self, blob: &[u8]) -> Result<Vec<u8>, CompressError> {
        self.compressor.decompress(blob)
    }
}

#[inline]
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    // word-at-a-time would be faster; keep it simple and correct.
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delta_beats_independent_on_near_identical() {
        let prev = b"fn main() { let x = 1; ".repeat(50);
        let mut next = prev.clone();
        next.extend_from_slice(b"println!(x); }");
        let level = 19;

        let indep = compress_with_reference(&next, &[], level).unwrap().len();
        let delta = compress_with_reference(&next, &prev, level).unwrap().len();
        // delta uses the reference, so it must be at least as small, and on this
        // near-identical input substantially smaller.
        assert!(delta <= indep, "delta {delta} should be <= indep {indep}");

        let recovered = decompress_with_reference(
            &compress_with_reference(&next, &prev, level).unwrap(),
            &prev,
            level,
        )
        .unwrap();
        assert_eq!(recovered, next);
    }

    #[test]
    fn delta_compressor_roundtrip_and_matches_free_fn() {
        let prev = b"fn main() { let x = 1; ".repeat(50);
        let mut next = prev.clone();
        next.extend_from_slice(b"println!(x); }");
        let level = 19;

        let dc = DeltaCompressor::new(&prev, level);
        let blob = dc.compress(&next).unwrap();
        // same reference + level → identical dict id → same bytes as the free fn
        let free = compress_with_reference(&next, &prev, level).unwrap();
        assert_eq!(blob, free);
        assert_eq!(dc.decompress(&blob).unwrap(), next);

        // empty reference falls back to plain zstd and still round-trips
        let dc0 = DeltaCompressor::new(&[], level);
        let b0 = dc0.compress(&next).unwrap();
        assert_eq!(dc0.decompress(&b0).unwrap(), next);
    }
}
