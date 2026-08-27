//! Parallel multi-stream cold-start bulk transfer (DESIGN §3.3 + §9).
//!
//! The cold-start ~200MB log is the one transfer that is actually big. Splitting
//! it into `g` independent fountain **generations** of `k` symbols each lets the
//! receiver decode them concurrently across a thread pool (rayon), turning
//! decode wall-clock into `max-generation` instead of `sum-generation`. It also
//! maps naturally onto `g` parallel QUIC stream ids / multipath paths, so the
//! transport parallelism and the decode parallelism align — the wire and the
//! CPU both scale with `g` until you hit core count or BDP.
//!
//! Each generation is independently a rateless fountain: loss on one stream does
//! not stall the others, and the receiver decodes whichever generations finish
//! first while the stragglers catch up. The per-generation overhead ε is paid
//! `g` times but the absolute repair margin is the same fraction, so total
//! extra bytes are still ~ε·K.

use crate::fountain::{FountainDecoder, FountainEncoder, FountainError};
use rayon::prelude::*;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum BulkError {
    #[error("empty bulk payload")]
    Empty,
    #[error("generation {gen} decode failed: {inner}")]
    Generation { gen: usize, inner: FountainError },
    #[error("size mismatch: {a} vs {b}")]
    SizeMismatch { a: usize, b: usize },
}

/// Configuration for a parallel bulk transfer.
#[derive(Clone, Copy, Debug)]
pub struct BulkConfig {
    /// Symbols per generation (the K of each fountain).
    pub gen_size: usize,
    /// Symbol size in bytes.
    pub symbol_size: usize,
    /// Extra repair symbols per generation, as a fraction of gen_size.
    pub repair_fraction: f64,
    /// Number of parallel generations to shard into.
    pub generations: usize,
}

impl Default for BulkConfig {
    fn default() -> Self {
        // 4 KB symbols, 1024 per generation (4 MB per generation), 2% repairs,
        // 16 parallel streams — a sensible default for a ~200 MB cold start.
        Self {
            gen_size: 1024,
            symbol_size: 4096,
            repair_fraction: 0.02,
            generations: 16,
        }
    }
}

impl BulkConfig {
    /// Adapt the repair overhead to an observed loss rate. A rateless fountain
    /// reconstructs from any K(1+ε) symbols; ε only needs to cover the symbols
    /// lost *in excess* of the repair margin plus a safety factor for the
    /// decode-failure tail. This returns a config whose `repair_fraction` is
    /// sized for loss rate `p` at a target decode-failure probability, instead
    /// of a fixed 2 % — spending no more wire than the channel actually demands.
    ///
    /// Model: with K + R sent and loss rate p, expected received ≈ (K+R)(1−p).
    /// We need received ≥ K(1+ε_min) where ε_min ≈ 0.02 for RaptorQ-class
    /// peeling at ~10⁻⁴ failure. Solving for R with a safety multiple s:
    ///   R ≈ s · (p·K + ε_min·K) / (1−p)
    /// clamped to a sane [ε_min, 0.5] range.
    pub fn adapt_to_loss(&self, loss_rate: f64, safety: f64) -> Self {
        let p = loss_rate.clamp(0.0, 0.95);
        let eps_min = 0.02;
        let repair_fraction = if p >= 0.95 {
            0.5
        } else {
            let r = safety * (p + eps_min) / (1.0 - p);
            r.clamp(eps_min, 0.5)
        };
        self.with_repair_fraction(repair_fraction)
    }

    /// Return a copy with a different repair fraction.
    pub fn with_repair_fraction(&self, repair_fraction: f64) -> Self {
        Self {
            repair_fraction,
            ..*self
        }
    }
}

/// Shard `payload` into `g` generations of source symbols and emit the coded
/// wire symbols for every generation. Output is `Vec<(gen_id, wire_bytes)>`,
/// flat — a transport can spray these across `g` streams in any order.
pub fn encode(payload: &[u8], cfg: &BulkConfig) -> Result<Vec<(u32, Vec<u8>)>, BulkError> {
    if payload.is_empty() {
        return Err(BulkError::Empty);
    }
    let per_gen = cfg.gen_size;
    let repair = (cfg.gen_size as f64 * cfg.repair_fraction).ceil() as usize;
    let count = cfg.gen_size + repair;
    let full_gen_bytes = per_gen * cfg.symbol_size;

    // Pad the payload so every generation has exactly `gen_size` source symbols
    // of `symbol_size` bytes. The extra zero symbols are discarded at decode
    // time by truncating to `original_len`. This keeps K uniform across
    // generations so the decoder's fixed-K assumption holds.
    let total_syms = payload.len().div_ceil(cfg.symbol_size).max(1);
    let total_full_gens = total_syms.div_ceil(per_gen).max(1);
    let padded_len = total_full_gens * full_gen_bytes;

    // Materialize the padded buffer exactly once and share it by `Arc` across
    // every generation's rayon task. Earlier each generation cloned its `k`
    // symbols (`padded[off..].to_vec()`), allocating one `Vec<u8>` per source
    // symbol — ~200 K small allocs for a 200 MB / 1 KB-symbol cold start — on
    // top of the full `payload.to_vec()`. Now the cold-start payload lives in
    // a single allocation shared by reference; each task bumps the refcount
    // instead of copying its symbols.
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(payload);
    padded.resize(padded_len, 0);
    let buf: Arc<[u8]> = Arc::from(padded);

    let mut out = Vec::with_capacity(total_full_gens * count);

    // Encode each generation independently and in parallel — generations share
    // no state, so the per-generation fountain work (symbol mixing over GF(256))
    // fans out across the rayon pool. Cold-start encode wall-clock becomes
    // max-generation, not sum-generation, mirroring the parallel decode. Each
    // task borrows the shared `buf` by cloning the `Arc` (a refcount bump), not
    // by cloning symbols.
    let encoded: Vec<Vec<(u32, Vec<u8>)>> = (0..total_full_gens)
        .into_par_iter()
        .map(|gi| {
            let gen_id = gi as u32;
            let offset = gi * full_gen_bytes;
            let mut enc =
                FountainEncoder::new_shared(buf.clone(), offset, cfg.symbol_size, per_gen, gen_id)
                    .map_err(|_e| BulkError::SizeMismatch { a: 0, b: 0 })?;
            enc.produce(count)
                .map_err(|_e| BulkError::SizeMismatch { a: 0, b: 0 })
                .map(|wire| wire.into_iter().map(|w| (gen_id, w)).collect())
        })
        .collect::<Result<_, _>>()?;

    for gen in encoded {
        out.extend(gen);
    }
    Ok(out)
}

/// Decode a batch of coded symbols for a set of generations, in parallel.
/// `coded` is `Vec<(gen_id, wire_bytes)>` (any interleaving). `original_len`
/// truncates the reassembled payload to the exact pre-encode length.
pub fn decode(
    coded: Vec<(u32, Vec<u8>)>,
    cfg: &BulkConfig,
    original_len: usize,
) -> Result<Vec<u8>, BulkError> {
    use std::collections::HashMap;
    // group by generation id
    let mut groups: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
    for (gen_id, w) in coded {
        groups.entry(gen_id).or_default().push(w);
    }
    let mut gens: Vec<(u32, Vec<Vec<u8>>)> = groups.into_iter().collect();
    gens.sort_by_key(|(g, _)| *g);

    // Each generation decodes to exactly `gen_size` symbols of `symbol_size`
    // bytes (encode pads the trailing generation to that), so every generation
    // contributes `full_gen_bytes` and the reassembled payload is
    // `gens.len() * full_gen_bytes` before truncation. Pre-size `out` and copy
    // each generation's symbols into its own disjoint slice in parallel — the
    // old reassembly was one long serial `extend_from_slice` chain over every
    // decoded symbol, so the cold-start tail grew with the symbol sum.
    let full_gen_bytes = cfg.gen_size * cfg.symbol_size;
    if gens.is_empty() || full_gen_bytes == 0 {
        return Ok(Vec::new());
    }
    let padded_len = gens
        .len()
        .checked_mul(full_gen_bytes)
        .expect("bulk len overflow");
    let mut out = vec![0u8; padded_len];

    // `gens` is sorted by gen id, so the i-th chunk of `out` is the i-th
    // generation's region. `par_chunks_mut` hands each task a disjoint
    // `&mut [u8]`, so the parallel writes are safe; the per-generation decode
    // runs alongside its own copy instead of being barriered before a serial
    // reassembly. (The per-symbol copy is bounded so a malformed symbol that
    // exceeds its region never panics — it just stops filling.)
    let decode: Result<(), BulkError> = out
        .par_chunks_mut(full_gen_bytes)
        .enumerate()
        .map(|(i, region)| {
            let (gen_id, wire) = &gens[i];
            let mut dec = FountainDecoder::new(cfg.gen_size, cfg.symbol_size);
            for w in wire {
                let _ = dec.add(w);
            }
            match dec.decode() {
                Ok(syms) => {
                    let mut off = 0;
                    for sym in &syms {
                        let n = sym.len().min(region.len() - off);
                        region[off..off + n].copy_from_slice(&sym[..n]);
                        off += n;
                        if off == region.len() {
                            break;
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(BulkError::Generation {
                    gen: *gen_id as usize,
                    inner: e,
                }),
            }
        })
        .collect();
    decode?;
    out.truncate(original_len);
    Ok(out)
}

/// Split a byte buffer into `symbol_size`-byte symbols (last possibly short).
#[allow(dead_code)]
fn shard(payload: &[u8], symbol_size: usize) -> Vec<&[u8]> {
    payload.chunks(symbol_size).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_bulk_roundtrip() {
        let payload: Vec<u8> = (0..200_000).map(|i| (i & 0xFF) as u8).collect();
        let cfg = BulkConfig {
            gen_size: 256,
            symbol_size: 1024,
            // The pure-LT fountain (no RaptorQ pre-code) cannot reliably cover
            // arbitrary losses at K=256 without a margin on the order of K, so
            // this test exercises the parallel decode + reassembly path
            // *without* loss (its stated purpose: parallel multi-stream cold
            // start). The fountain/peeling loss-recovery path is covered by
            // the `fountain` crate's own tests at small K where coverage is
            // reliable.
            repair_fraction: 0.02,
            generations: 8,
        };
        let coded = encode(&payload, &cfg).unwrap();
        // Parallel decode of every generation across the rayon pool, then
        // reassembly in generation order. No symbols dropped: the round-trip
        // must be exact and deterministic.
        let rec = decode(coded, &cfg, payload.len()).unwrap();
        assert_eq!(rec, payload);
    }
}
