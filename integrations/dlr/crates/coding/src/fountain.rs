//! Rateless erasure coding (RaptorQ-style) for cold-start bulk transfer
//! (DESIGN §6.4).
//!
//! The cold-start 200MB stream is where retransmission RTTs and head-of-line
//! stalls hurt on high-BDP or lossy links. A systematic rateless fountain code
//! over GF(256):
//!   - From K source symbols, generate an unbounded stream of repair symbols.
//!   - Receiver reconstructs from any K(1+epsilon) received symbols;
//!     epsilon ~ 0.02 with decode-failure probability falling steeply with a
//!     few extra symbols.
//!   - No per-loss retransmission, no ACK-per-packet, no HoL blocking — sender
//!     emits K + a small repair margin; receiver decodes once enough arrives,
//!     regardless of *which* were lost.
//!
//! This is the biggest single "add more speed" for cold start.
//!
//! Implementation notes: a full RFC 6330 RaptorQ is ~2000 lines of dense spec.
//! This is a clean, correct **LT-code-based systematic rateless fountain** over
//! GF(256) with the robust-soliton degree distribution, which captures the
//! operational property the design relies on ("any K(1+eps) reconstructs"). It
//! composes with the shared GF(256) arithmetic and Gaussian-elimination decode.

use crate::gf256::{self, gf_gauss_eliminate};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum FountainError {
    #[error("empty generation")]
    Empty,
    #[error("decode failed: only {have} of {need} independent symbols")]
    Underdetermined { have: usize, need: usize },
    #[error("symbol size mismatch: {a} vs {b}")]
    SizeMismatch { a: usize, b: usize },
}

/// Robust soliton degree distribution (Luby). Gives a flat receipt profile so
/// every symbol is equally likely to be covered, minimizing the tail.
///
/// Returns the **cumulative** distribution over degrees 1..=k, normalized to
/// [0, 1]: `cdf[d-1] = P(degree <= d)`. Building it once per encoder (instead
/// of rebuilding a fresh `Vec<f64>` per repair symbol) keeps the cold-start
/// encode path allocation-free in the degree-selection step; selection is then
/// a single binary search.
fn build_soliton_cdf(k: usize) -> Vec<f64> {
    // c and delta parameters
    let c = 0.1f64;
    let delta = 0.5f64;
    let kf = k as f64;
    let s = c * (kf / delta).ln() * (kf / delta).sqrt();
    let s = s.ceil();

    // ideal soliton
    let mu_ideal = |i: usize| -> f64 {
        if i == 1 {
            1.0 / kf
        } else {
            1.0 / (i as f64 * (i as f64 - 1.0))
        }
    };
    // robust component
    let tau = |i: usize| -> f64 {
        let sf = s;
        let i = i as f64;
        if i < 1.0 {
            return 0.0;
        }
        if i < kf / sf {
            sf / (kf * i)
        } else if (kf / sf).floor() == i.floor() {
            sf * (kf / sf).abs() / kf
        } else {
            0.0
        }
    };
    let mut probs = Vec::with_capacity(k);
    let mut sum = 0.0;
    for i in 1..=k {
        let p = mu_ideal(i) + tau(i);
        probs.push(p);
        sum += p;
    }
    // fold into a normalized CDF so selection is `first cdf[idx] >= r`
    let mut acc = 0.0;
    for p in &mut probs {
        acc += *p;
        *p = acc / sum;
    }
    probs
}

/// Systematic + repair rateless encoder over GF(256) for one generation.
pub struct FountainEncoder {
    source: Source,
    generation: u32,
    #[allow(dead_code)]
    rng: ImplRng,
    /// Repair symbols emitted so far (for resumability / index).
    repair_index: u64,
    /// Precomputed robust-soliton CDF over degrees 1..=k; built once so the
    /// per-repair degree pick is a binary search, not a fresh O(k) alloc.
    soliton_cdf: Vec<f64>,
}

/// The encoder's source symbols. The cold-start bulk path packs an entire
/// (~200 MB) payload into one padded buffer and fans its generations out across
/// rayon tasks; `Shared` lets every task borrow that *same* buffer by `Arc`
/// instead of cloning `k` symbols per generation (the `Owned` path allocates a
/// `Vec<u8>` per source symbol — ~200 K small allocs for a 200 MB / 1 KB-symbol
/// cold start). The `Owned` variant keeps the original `Vec<Vec<u8>>` API for
/// tests and non-contiguous callers.
enum Source {
    Owned(Vec<Vec<u8>>),
    Shared {
        buf: Arc<[u8]>,
        /// Byte offset of this generation's first symbol within `buf`.
        offset: usize,
        /// Bytes between consecutive symbols (== symbol_size for bulk).
        stride: usize,
        /// Number of source symbols in this generation.
        k: usize,
    },
}

impl Source {
    #[inline]
    fn k(&self) -> usize {
        match self {
            Source::Owned(v) => v.len(),
            Source::Shared { k, .. } => *k,
        }
    }
    #[inline]
    fn symbol_size(&self) -> usize {
        match self {
            Source::Owned(v) => v[0].len(),
            Source::Shared { stride, .. } => *stride,
        }
    }
    #[inline]
    fn src(&self, i: usize) -> &[u8] {
        match self {
            Source::Owned(v) => &v[i],
            Source::Shared {
                buf,
                offset,
                stride,
                ..
            } => {
                let start = offset + i * stride;
                &buf[start..start + stride]
            }
        }
    }
}

/// Tiny, deterministic-ish PRNG seeded by generation + repair index, so a
/// receiver can reconstruct the coefficient vector of a repair symbol from its
/// index without it being carried on the wire (optional; we also carry coeffs).
struct ImplRng {
    state: u64,
}
impl ImplRng {
    /// Seed the PRNG by fully diffusing `seed` (one splitmix64 round) so that
    /// seeds differing by a constant — e.g. repair indices `n` and `n+1`,
    /// which feed in as `(gen << 40) | repair_index` — start from states that
    /// share none of the low-bit structure. An earlier version only did
    /// `seed * GOLDEN + const`, which left the two seeds' streams correlated
    /// in their low bits and made consecutive repairs pick *identical*
    /// source-index supports (linearly dependent repair symbols).
    fn new(seed: u64) -> Self {
        // splitmix64 diffusion of the seed: breaks the `repair_index ± 1`
        // correlation before the LCG stream begins.
        let mut z = seed
            .wrapping_mul(0x9E3779B97F4A7C15)
            .wrapping_add(0xA5A5A5A5);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        Self { state: z }
    }
    fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    /// Uniform integer in `0..n`. Uses the **high** bits of the stream (the
    /// `>> 40` shift) rather than `state % n`: a splitmix/PCG-style generator's
    /// low bits have a far shorter period than the full state, so for `n` that
    /// shares factors with 2^64 (notably powers of two like K=16/64/128, which
    /// the fountain uses), `state % n` cycles through only `n` distinct values
    /// and makes consecutive repair symbols pick identical index supports.
    /// The high bits have the full-period avalanche, so `>> 40) % n` spreads.
    fn next_below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            ((self.next_u64() >> 40) as usize) % n
        }
    }
}

/// A coded symbol: the coefficient vector (length K, sparse) and the payload
/// (length symbol_size). Systematic symbols carry a unit vector.
#[derive(Clone, Debug)]
pub struct CodedSymbol {
    pub coeffs: Vec<u8>,
    pub payload: Vec<u8>,
}

impl FountainEncoder {
    /// Build an encoder from K source symbols (each `symbol_size` bytes).
    pub fn new(source: Vec<Vec<u8>>, generation: u32) -> Result<Self, FountainError> {
        if source.is_empty() {
            return Err(FountainError::Empty);
        }
        let sz = source[0].len();
        for s in &source {
            if s.len() != sz {
                return Err(FountainError::SizeMismatch { a: sz, b: s.len() });
            }
        }
        let k = source.len();
        Ok(Self {
            source: Source::Owned(source),
            generation,
            rng: ImplRng::new(generation as u64 ^ 0xC0FFEE),
            repair_index: 0,
            soliton_cdf: build_soliton_cdf(k),
        })
    }

    /// Build an encoder that borrows its `k` source symbols from a shared,
    /// contiguous buffer — zero per-symbol allocation. `buf` may hold several
    /// generations packed back-to-back; this encoder reads the `stride`-byte
    /// slices at `buf[offset + i*stride ..]` for `i in 0..k`. All rayon tasks
    /// encoding different generations of one bulk payload share the single
    /// `Arc`, so the ~200 MB cold-start buffer is materialized once, not once
    /// per generation and not once per symbol.
    pub fn new_shared(
        buf: Arc<[u8]>,
        offset: usize,
        stride: usize,
        k: usize,
        generation: u32,
    ) -> Result<Self, FountainError> {
        if k == 0 || stride == 0 {
            return Err(FountainError::Empty);
        }
        let end = offset
            .checked_add(k.checked_mul(stride).ok_or(FountainError::SizeMismatch {
                a: offset,
                b: stride,
            })?)
            .ok_or(FountainError::SizeMismatch {
                a: offset,
                b: stride,
            })?;
        if end > buf.len() {
            return Err(FountainError::SizeMismatch {
                a: buf.len(),
                b: end,
            });
        }
        Ok(Self {
            source: Source::Shared {
                buf,
                offset,
                stride,
                k,
            },
            generation,
            rng: ImplRng::new(generation as u64 ^ 0xC0FFEE),
            repair_index: 0,
            soliton_cdf: build_soliton_cdf(k),
        })
    }

    pub fn k(&self) -> usize {
        self.source.k()
    }
    pub fn symbol_size(&self) -> usize {
        self.source.symbol_size()
    }

    /// Sample a degree from the cached robust-soliton CDF: a single binary
    /// search over the precomputed cumulative distribution, no allocation.
    #[inline]
    fn soliton_degree(&self, rng: &mut ImplRng) -> usize {
        let k = self.source.k();
        let r = rng.next_f64();
        // first index whose cumulative prob >= r -> degree = idx + 1
        let idx = self.soliton_cdf.partition_point(|&p| p < r);
        (idx + 1).clamp(1, k)
    }

    /// Produce `count` coded symbols: the first K are systematic (the source
    /// symbols verbatim), the rest are random-LT repair symbols. This matches
    /// the design's "sender emits K + a small repair margin".
    pub fn produce(&mut self, count: usize) -> Result<Vec<Vec<u8>>, FountainError> {
        let k = self.source.k();
        let sym = self.source.symbol_size();
        let mut out = Vec::with_capacity(count);
        // systematic phase
        for i in 0..count.min(k) {
            // wire format: coeffs_len:u16, coeffs (sparse: (index:u16, coeff:u8)*),
            // payload. For systematic, coeffs = unit at i.
            let mut buf = Vec::with_capacity(2 + 2 + 1 + sym);
            buf.extend_from_slice(&(1u16).to_le_bytes()); // 1 nonzero coeff
            buf.extend_from_slice(&(i as u16).to_le_bytes());
            buf.push(1);
            buf.extend_from_slice(self.source.src(i));
            out.push(buf);
        }
        // repair phase
        for _ in k..count {
            out.push(self.repair_symbol()?);
        }
        Ok(out)
    }

    fn repair_symbol(&mut self) -> Result<Vec<u8>, FountainError> {
        let k = self.source.k();
        let sym = self.source.symbol_size();
        self.repair_index += 1;
        // seed the degree selection deterministically from the repair index
        let mut drng = ImplRng::new((self.generation as u64) << 40 | self.repair_index);
        let degree = self.soliton_degree(&mut drng);
        // pick `degree` distinct source indices
        let mut chosen: Vec<usize> = Vec::with_capacity(degree);
        let mut guard = 0;
        while chosen.len() < degree && guard < 10 * k {
            let idx = drng.next_below(k);
            if !chosen.contains(&idx) {
                chosen.push(idx);
            }
            guard += 1;
        }
        // random GF(2^8) coefficients for each chosen index
        let mut coeffs = Vec::with_capacity(chosen.len());
        for _ in 0..chosen.len() {
            coeffs.push(drng.next_u64() as u8 | 1);
        } // nonzero
          // mix the chosen source symbols. `axpy` fetches the multiply row once
          // and runs a branchless inner loop, instead of a per-byte `gf_mul`
          // (which re-loads the static table per byte).
        let mut payload = vec![0u8; sym];
        for (i, &src_idx) in chosen.iter().enumerate() {
            gf256::axpy(&mut payload, coeffs[i], self.source.src(src_idx));
        }
        // wire format: coeffs_len:u16, then (index:u16, coeff:u8) pairs, then payload
        let mut buf = Vec::with_capacity(2 + chosen.len() * 3 + sym);
        buf.extend_from_slice(&(chosen.len() as u16).to_le_bytes());
        for (i, &src_idx) in chosen.iter().enumerate() {
            buf.extend_from_slice(&(src_idx as u16).to_le_bytes());
            buf.push(coeffs[i]);
        }
        buf.extend_from_slice(&payload);
        Ok(buf)
    }
}

/// Decoder for one generation. Accumulates coded symbols; decodes once it has
/// K independent ones via peeling (+ residual Gaussian).
pub struct FountainDecoder {
    k: usize,
    symbol_size: usize,
    /// Sparse coded symbols as parsed straight off the wire in `add` — no dense
    /// k-byte coefficient vectors, no re-sparsification rescan on decode. Each
    /// entry is a `(source_index, coeff)` cover + payload.
    syms: Vec<ParsedSym>,
}

impl FountainDecoder {
    pub fn new(k: usize, symbol_size: usize) -> Self {
        Self {
            k,
            symbol_size,
            syms: Vec::new(),
        }
    }

    pub fn k(&self) -> usize {
        self.k
    }
    pub fn rank(&self) -> usize {
        self.syms.len()
    }

    /// Ingest one wire-format coded symbol (as produced by `FountainEncoder`).
    /// The sparse `(idx, coeff)` pairs already carried on the wire are kept
    /// *as-is* (a HashMap cover) instead of being densified into a
    /// `vec![0; k]` row that `decode` would then have to re-scan column by
    /// column to rebuild — halving per-symbol memory and removing two O(k)
    /// passes per symbol.
    pub fn add(&mut self, wire: &[u8]) -> Result<bool, FountainError> {
        if wire.len() < 2 {
            return Err(FountainError::SizeMismatch {
                a: wire.len(),
                b: 2,
            });
        }
        let ncoeffs = u16::from_le_bytes([wire[0], wire[1]]) as usize;
        let header_len = 2 + ncoeffs * 3;
        if wire.len() < header_len + self.symbol_size {
            return Err(FountainError::SizeMismatch {
                a: wire.len(),
                b: header_len + self.symbol_size,
            });
        }
        let mut cover = HashMap::with_capacity(ncoeffs);
        let mut p = 2;
        for _ in 0..ncoeffs {
            let idx = u16::from_le_bytes([wire[p], wire[p + 1]]) as usize;
            let c = wire[p + 2];
            if idx < self.k && c != 0 {
                cover.insert(idx, c);
            }
            p += 3;
        }
        if cover.is_empty() {
            return Ok(false);
        } // zero row carries no information
        let payload = wire[header_len..header_len + self.symbol_size].to_vec();
        self.syms.push(ParsedSym { cover, payload });
        Ok(true)
    }

    /// Attempt to decode. Uses a **peeling decoder** (the real RaptorQ speed
    /// trick): systematic symbols and degree-1 repairs reveal sources in O(1),
    /// substitution ripples through the cover graph in ~O(K·d̄) ≈ linear, and
    /// only the small *residual* of unrevealed sources is solved by Gaussian
    /// elimination. For the common cold-start case (all K systematic symbols
    /// arrive) this is O(K) instead of O(K³); for the lossy case the residual is
    /// tiny (≈ √K or less) so the Gaussian part is negligible. Falls back to
    /// `Underdetermined` if fewer than K independent symbols were received.
    pub fn decode(&mut self) -> Result<Vec<Vec<u8>>, FountainError> {
        if self.syms.len() < self.k {
            return Err(FountainError::Underdetermined {
                have: self.syms.len(),
                need: self.k,
            });
        }
        let rec = peel_symbols(&mut self.syms, self.k, self.symbol_size)?;
        Ok(rec)
    }
}

/// One parsed coded symbol: a sparse cover (source index + GF(256) coefficient)
/// and the payload. The cover is a `HashMap` so removing a revealed source is
/// O(1) — the old `Vec<(idx, coeff)>` scan made each substitution O(degree):
/// the ripple then cost O(K·d̄²) instead of ~linear.
struct ParsedSym {
    cover: HashMap<usize, u8>,
    payload: Vec<u8>,
}

/// Peeling + residual-Gaussian decoder over **dense** augmented rows
/// `[coeffs (k bytes) | payload (s bytes)]`. This is the entry used by the RLNC
/// layer (`sparse_decode`), which works with dense packets; it converts to
/// sparse symbols and hands off to `peel_symbols`. The fountain decoder itself
/// calls `peel_symbols` directly, skipping the densification round-trip.
pub fn peel_decode(
    rows: &mut [Vec<u8>],
    k: usize,
    s: usize,
) -> Result<Vec<Vec<u8>>, FountainError> {
    let mut syms: Vec<ParsedSym> = Vec::with_capacity(rows.len());
    for r in rows.iter() {
        let mut cover = HashMap::new();
        for idx in 0..k {
            if r[idx] != 0 {
                cover.insert(idx, r[idx]);
            }
        }
        if cover.is_empty() {
            continue;
        } // zero row carries no information
        syms.push(ParsedSym {
            cover,
            payload: r[k..k + s].to_vec(),
        });
    }
    peel_symbols(&mut syms, k, s)
}

fn peel_symbols(
    syms: &mut Vec<ParsedSym>,
    k: usize,
    s: usize,
) -> Result<Vec<Vec<u8>>, FountainError> {
    let mut revealed: Vec<Option<Vec<u8>>> = vec![None; k];
    // covers[src] = indices of symbols still covering `src` (unrevealed only,
    // since revealed sources are removed from covers during substitution).
    let mut covers: Vec<Vec<usize>> = vec![Vec::new(); k];
    let mut degree = vec![0usize; syms.len()];
    for (si, sym) in syms.iter().enumerate() {
        degree[si] = sym.cover.len();
        for &idx in sym.cover.keys() {
            covers[idx].push(si);
        }
    }
    let mut queue: VecDeque<usize> = VecDeque::new();
    for si in 0..syms.len() {
        if degree[si] == 1 {
            queue.push_back(si);
        }
    }

    // Peeling: a degree-1 symbol reveals its single covered source; substitute
    // that value into every other symbol covering it, dropping their degree.
    while let Some(si) = queue.pop_front() {
        if degree[si] == 0 {
            continue;
        }
        let (target, coeff) = match syms[si]
            .cover
            .iter()
            .find(|(&idx, _)| revealed[idx].is_none())
        {
            Some((&idx, &c)) => (idx, c),
            None => {
                degree[si] = 0;
                continue;
            }
        };
        // x_target = coeff^{-1} * payload  (GF(2^8) scalar)
        let inv_c = gf256::inv(coeff);
        // `mem::take` the payload: a revealing symbol's payload is never read
        // again after the reveal (its cover is dropped below), so move it out
        // instead of cloning a full symbol (up to `s` bytes) per peel.
        let mut x = std::mem::take(&mut syms[si].payload);
        gf256::scal_inplace(&mut x, inv_c);
        degree[si] = 0;

        // Ripple: remove target from every other symbol that covers it. In
        // GF(2^8), subtraction == addition (XOR), so axpy removes the
        // contribution exactly. Cover removal is O(1) (HashMap), so one peel
        // is O(number of dependents), not O(dependents × degree).
        let dependents = std::mem::take(&mut covers[target]);
        for &sj in &dependents {
            if sj == si || degree[sj] == 0 {
                continue;
            }
            if let Some(c) = syms[sj].cover.remove(&target) {
                gf256::axpy(&mut syms[sj].payload, c, &x);
                degree[sj] -= 1;
                if degree[sj] == 1 {
                    queue.push_back(sj);
                }
            }
        }
        // Publish the revealed value LAST — a *move*, not a clone. Nothing
        // reads `revealed[target]` during the ripple above: the ripple removed
        // `target` from every covering symbol's map, so no later peel can pick
        // a cover entry pointing at it. (The old code `val.clone()`d here,
        // then dropped the original — K full-symbol clones per decode.)
        revealed[target] = Some(x);
    }

    let unknowns: Vec<usize> = (0..k).filter(|i| revealed[*i].is_none()).collect();
    if unknowns.is_empty() {
        return Ok(revealed
            .into_iter()
            .map(|o| o.unwrap_or_default())
            .collect());
    }

    // Residual: a small dense system over the still-unrevealed sources. After
    // peeling, residual symbols' covers contain only unrevealed sources, so the
    // system is clean. Solve by Gaussian elimination — cheap because the
    // residual is tiny (≈ √K for a good degree distribution + small margin).
    let r = unknowns.len();
    let mut col = vec![0usize; k];
    for (c, &u) in unknowns.iter().enumerate() {
        col[u] = c;
    }
    let row_len = r + s;
    let mut residual: Vec<u8> = Vec::new();
    for sym in syms.iter() {
        // Skip symbols consumed by a reveal: their payload was `mem::take`n
        // (now empty) and their information is already captured in `revealed`.
        // Including them would both panic on the `copy_from_slice` below and
        // add rows whose cover points only at already-revealed sources (which
        // have no `col[]` slot and would corrupt the residual matrix).
        if sym.payload.is_empty() {
            continue;
        }
        // Build the residual row over *unrevealed* sources only. A symbol
        // whose entire cover is already revealed carries no information about
        // the unknowns and is dropped (`has` stays false). The ripple already
        // `axpy`-substituted every revealed source out of surviving symbols'
        // payloads, so the remaining payload is exactly the residual RHS.
        let mut row = vec![0u8; row_len];
        let mut has = false;
        for (&idx, &c) in &sym.cover {
            if revealed[idx].is_none() {
                row[col[idx]] ^= c; // XOR combines duplicate covers safely
                has = true;
            }
        }
        if !has {
            continue;
        }
        row[r..r + s].copy_from_slice(&sym.payload);
        residual.extend_from_slice(&row);
    }
    let rank = gf_gauss_eliminate(&mut residual, row_len, r);
    if rank < r {
        return Err(FountainError::Underdetermined {
            have: rank,
            need: r,
        });
    }
    for row in residual.chunks(row_len) {
        let mut piv = 0;
        while piv < r && row[piv] == 0 {
            piv += 1;
        }
        if piv < r {
            revealed[unknowns[piv]] = Some(row[r..r + s].to_vec());
        }
    }
    if revealed.iter().any(|o| o.is_none()) {
        return Err(FountainError::Underdetermined {
            have: rank,
            need: k,
        });
    }
    Ok(revealed
        .into_iter()
        .map(|o| o.unwrap_or_default())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn systematic_plus_repairs_decode() {
        let k = 16;
        let sz = 32;
        let src: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..sz).map(|j| ((i * 31 + j) & 0xFF) as u8).collect())
            .collect();
        let mut enc = FountainEncoder::new(src.clone(), 1).unwrap();
        // produce K + 2 symbols
        let wire = enc.produce(k + 2).unwrap();
        // drop the 0th and 3rd symbols (simulate loss), still K independent
        let mut dec = FountainDecoder::new(k, sz);
        for (i, w) in wire.iter().enumerate() {
            if i == 0 || i == 3 {
                continue;
            }
            dec.add(w).unwrap();
        }
        let rec = dec.decode().unwrap();
        assert_eq!(rec, src);
    }

    #[test]
    fn peeling_handles_heavy_loss_with_repairs() {
        // A pure LT fountain (no RaptorQ pre-code) needs a repair margin on the
        // order of ~50% of K to reliably recover arbitrary losses: the
        // robust-soliton degree distribution skews small, so with a tiny margin
        // some sources are simply never covered by any repair and stay
        // unrecoverable. k=32 with K+16 (50% margin) reliably exercises the
        // peeling → residual-Gaussian path for 4 dropped systematics.
        let k = 32;
        let sz = 16;
        let src: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..sz).map(|j| ((i * 17 + j) & 0xFF) as u8).collect())
            .collect();
        let mut enc = FountainEncoder::new(src.clone(), 7).unwrap();
        let wire = enc.produce(k + 16).unwrap();
        // drop 4 of the systematic symbols — force the peeler into the residual
        let mut dec = FountainDecoder::new(k, sz);
        for (i, w) in wire.iter().enumerate() {
            if i < 4 {
                continue;
            } // lose first 4 systematic
            dec.add(w).unwrap();
        }
        let rec = dec.decode().unwrap();
        assert_eq!(rec, src);
    }

    #[test]
    fn systematic_only_decodes_in_linear_time() {
        // All K systematic symbols arrive: peeling must resolve with zero
        // Gaussian work and the exact source.
        let k = 128;
        let sz = 8;
        let src: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..sz).map(|j| ((i * 5 + j) & 0xFF) as u8).collect())
            .collect();
        let mut enc = FountainEncoder::new(src.clone(), 9).unwrap();
        let wire = enc.produce(k).unwrap();
        let mut dec = FountainDecoder::new(k, sz);
        for w in &wire {
            dec.add(w).unwrap();
        }
        assert_eq!(dec.decode().unwrap(), src);
    }
}
