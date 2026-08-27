//! Compression layer (DESIGN §7).
//!
//! Orthogonal multiplier on the novel blocks: **zstd with a dictionary trained
//! on your trace corpus.** SWE traces are brutally repetitive (repeated file
//! snapshots, boilerplate tool schemas, common prefixes), so a trained dict
//! beats generic zstd substantially.
//!
//! Compose order (§7): `canonicalize -> zstd(dict) -> {erasure|network}-code ->
//! transport`. The shim applies this per-block on the append path; the cold-start
//! path applies it to source symbols before fountain/RLNC coding.
//!
//! This module wraps zstd with:
//!   - a per-block compressor using a fixed (optionally trained) dictionary,
//!   - a "trained dictionary" carrier (the dict bytes travel once at session
//!     start, then are referenced by id),
//!   - a fast passthrough fallback for blocks that do not compress (incompressible
//!     payloads should not expand).

use std::cell::RefCell;
use std::sync::Arc;

pub mod delta;
pub use delta::{
    compress_with_reference, decompress_with_reference, pick_reference, DeltaCompressor,
};

#[derive(Debug, thiserror::Error)]
pub enum CompressError {
    #[error("zstd encode: {0}")]
    Encode(String),
    #[error("zstd decode: {0}")]
    Decode(String),
    #[error("dictionary id mismatch: expected {expected} got {got}")]
    DictId { expected: u64, got: u64 },
}

// Reusable zstd contexts, cached per-thread. `Compressor::with_dictionary` /
// `Decompressor::with_dictionary` return `'static` contexts — the dictionary is
// *copied* into the zstd context (`load_dictionary`), so the cached object owns
// its dict and no longer borrows the user's `Dict`. The contexts are keyed by
// `(level, dict_id)` / `dict_id`; a dict change (different id) rebuilds. This
// eliminates the per-call dictionary load + context allocation on the append
// hot path — the single most expensive part of per-block zstd when a trained
// dictionary is in use.
thread_local! {
    static ENC: RefCell<Option<(i32, u64, zstd::bulk::Compressor<'static>)>> =
        const { RefCell::new(None) };
    static DEC: RefCell<Option<(u64, zstd::bulk::Decompressor<'static>)>> =
        const { RefCell::new(None) };
}

/// A compression dictionary. The raw bytes are a zstd "raw content" dictionary;
/// `id` lets the wire reference it once instead of resending.
#[derive(Debug, Clone)]
pub struct Dict {
    pub id: u64,
    pub content: Arc<Vec<u8>>,
}

impl Dict {
    /// Build a zstd `CDict`/`DDict` from raw content. We keep the raw bytes and
    /// construct encoders/decoders on demand (zstd's dictionary objects are not
    /// Send/Sync-friendly across all versions, so we rebuild per thread).
    pub fn from_content(id: u64, content: Vec<u8>) -> Self {
        Self {
            id,
            content: Arc::new(content),
        }
    }

    /// A trivial empty dictionary (falls back to plain zstd).
    pub fn empty() -> Self {
        Self {
            id: 0,
            content: Arc::new(Vec::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// Per-block compressor with a dictionary. Cheap to clone (Arc-shared dict).
#[derive(Clone)]
pub struct Compressor {
    dict: Dict,
    level: i32,
    /// Threshold below which we don't even try (tiny blocks rarely compress).
    min_block: usize,
}

impl Compressor {
    pub fn new(dict: Dict, level: i32) -> Self {
        Self {
            dict,
            level,
            min_block: 32,
        }
    }

    /// Default compressor: level 19, empty dict (caller swaps in a trained one).
    #[allow(clippy::should_implement_trait)] // named factory fn, not a Default impl
    pub fn default() -> Self {
        Self::new(Dict::empty(), 19)
    }

    pub fn dict(&self) -> &Dict {
        &self.dict
    }
    pub fn set_dict(&mut self, dict: Dict) {
        self.dict = dict;
    }

    /// Return a compressor over the *same dictionary* at a different zstd level.
    /// The per-thread encoder cache is keyed by `(level, dict_id)`, so running two
    /// levels side by side costs only one extra cached context per level — the
    /// append hot path can use a fast level (low CPU, KB-scale deltas) while the
    /// cold-start bulk path keeps max ratio (level 19, 200 MB), sharing one
    /// dictionary. Decompression is level-agnostic, so the receiver's single
    /// `Decompressor` handles both.
    pub fn with_level(&self, level: i32) -> Self {
        Self {
            dict: self.dict.clone(),
            level,
            min_block: self.min_block,
        }
    }

    /// Compress a block. Returns the compressed bytes, or the original bytes if
    /// compression made it larger (passthrough — flagged by a leading 0x00 marker
    /// vs 0x01 for "zstd-compressed").
    pub fn compress(&self, input: &[u8]) -> Result<Vec<u8>, CompressError> {
        if input.len() < self.min_block {
            // passthrough, marked raw
            let mut out = Vec::with_capacity(input.len() + 1);
            out.push(0x00);
            out.extend_from_slice(input);
            return Ok(out);
        }
        let compressed = ENC.with(|cell| -> Result<Vec<u8>, CompressError> {
            let mut b = cell.borrow_mut();
            let need_rebuild = match &*b {
                Some((l, d, _)) => *l != self.level || *d != self.dict.id,
                None => true,
            };
            if need_rebuild {
                let enc = if self.dict.is_empty() {
                    zstd::bulk::Compressor::new(self.level)
                } else {
                    zstd::bulk::Compressor::with_dictionary(self.level, &self.dict.content[..])
                }
                .map_err(|e| CompressError::Encode(e.to_string()))?;
                *b = Some((self.level, self.dict.id, enc));
            }
            let (.., enc) = b.as_mut().unwrap();
            enc.compress(input)
                .map_err(|e| CompressError::Encode(e.to_string()))
        })?;
        if compressed.len() + 1 >= input.len() {
            // expansion — passthrough
            let mut out = Vec::with_capacity(input.len() + 1);
            out.push(0x00);
            out.extend_from_slice(input);
            Ok(out)
        } else {
            let mut out = Vec::with_capacity(compressed.len() + 1);
            out.push(0x01);
            out.extend_from_slice(&compressed);
            Ok(out)
        }
    }

    /// Decompress bytes produced by `compress`. The leading marker selects path.
    pub fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, CompressError> {
        if input.is_empty() {
            return Err(CompressError::Decode("empty input".into()));
        }
        match input[0] {
            0x00 => Ok(input[1..].to_vec()),
            0x01 => {
                let body = &input[1..];
                // Reuse a per-thread `Decompressor` keyed by dict id; the dict is
                // loaded once into the context, not per call. The output buffer
                // is sized from the frame's stored content size
                // (`zstd_safe::get_frame_content_size`, `experimental` feature)
                // so a small compressed block no longer reserves a ~1 MB Vec;
                // frames without a content size fall back to a heuristic cap.
                DEC.with(|cell| -> Result<Vec<u8>, CompressError> {
                    let mut b = cell.borrow_mut();
                    let need_rebuild = match &*b {
                        Some((d, _)) => *d != self.dict.id,
                        None => true,
                    };
                    if need_rebuild {
                        let dd = if self.dict.is_empty() {
                            zstd::bulk::Decompressor::new()
                        } else {
                            zstd::bulk::Decompressor::with_dictionary(&self.dict.content[..])
                        }
                        .map_err(|e| CompressError::Decode(e.to_string()))?;
                        *b = Some((self.dict.id, dd));
                    }
                    let (_, dd) = b.as_mut().unwrap();
                    // Size the output buffer from the frame's stored content
                    // size (`zstd_safe::get_frame_content_size`, exposed by the
                    // `experimental` zstd feature) so a 4 KB compressed block
                    // reserves exactly its output, not a ~1 MB Vec. Our
                    // `Compressor::compress` writes a single bulk frame that
                    // always carries the content size, so this is the common
                    // path; frames without one (streaming-compressed, edge
                    // cases) fall back to the heuristic cap.
                    let cap = match zstd::zstd_safe::get_frame_content_size(body) {
                        Ok(Some(s)) => s as usize,
                        Ok(None) | Err(_) => body.len().saturating_mul(64) + (1 << 20),
                    };
                    dd.decompress(body, cap)
                        .map_err(|e| CompressError::Decode(e.to_string()))
                })
            }
            other => Err(CompressError::Decode(format!(
                "unknown marker {:#x}",
                other
            ))),
        }
    }
}

/// Train a zstd dictionary from a corpus of sample blocks (DESIGN §7).
///
/// zstd accepts arbitrary bytes as a "raw content" dictionary, so we build one
/// by concatenating representative samples (most-recent first, to bias the dict
/// toward current-trace patterns) up to `target_size`. This is robust across zstd
/// versions and still gives the bulk of the win on repetitive SWE traces; a
/// full COVER/BetterCover trainer can be dropped in behind this signature when
/// the `zstd` training API is confirmed available.
pub fn train_dictionary(samples: &[&[u8]], target_size: usize) -> Result<Vec<u8>, CompressError> {
    let mut dict = Vec::with_capacity(target_size);
    // Sort samples by size ascending so small high-frequency prefixes land first
    // (zstd dictionaries benefit from common prefixes appearing early). Then
    // interleave samples to maximize variety within the budget.
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by_key(|&i| samples[i].len());
    let mut filled = 0usize;
    'outer: for pass in 0..3 {
        for &i in &order {
            let s = samples[i];
            if s.is_empty() {
                continue;
            }
            let take = s.len().min(target_size.saturating_sub(filled));
            if take == 0 {
                break 'outer;
            }
            // take a slice from the start (common prefixes) on even passes, end on odd
            let chunk = if pass % 2 == 0 {
                &s[..take]
            } else {
                &s[s.len() - take..]
            };
            dict.extend_from_slice(chunk);
            filled += take;
            if filled >= target_size {
                break 'outer;
            }
        }
    }
    Ok(dict)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_small() {
        let c = Compressor::default();
        let data = b"hello world ".repeat(200);
        let z = c.compress(&data).unwrap();
        let d = c.decompress(&z).unwrap();
        assert_eq!(d, data);
    }

    #[test]
    fn passthrough_incompressible() {
        let c = Compressor::default();
        // Build genuinely incompressible data: a byte sequence with no
        // repeating structure. `(i * 9973) as u8` is 256-periodic (9973 mod 256
        // = 245, coprime to 256), so zstd legitimately compresses it and the
        // old assertion (`z[0] == 0x00` passthrough) fails on zstd versions
        // that spot the period. A splitmix64 keystream has no such period.
        let data: Vec<u8> = (0..4096)
            .map(|i| {
                let mut z = i as u64;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
                (z ^ (z >> 31)) as u8
            })
            .collect();
        let z = c.compress(&data).unwrap();
        assert_eq!(z[0], 0x00, "expected passthrough for incompressible");
        let d = c.decompress(&z).unwrap();
        assert_eq!(d, data);
    }
}
