//! Reed-Solomon systematic erasure coding over GF(256) (extra strategy).
//!
//! A fixed-rate alternative to the rateless fountain (§6.4) for the *non-lossy*
//! fabric cold-start path, where you don't need an unbounded repair stream and
//! a single shot of `n = k + m` symbols is cheaper to compute than a fountain's
//! robust-soliton degree sampling. RS is also the natural inner code for the
//! hierarchical fan-out (§`hierarchical`).
//!
//! Systematic: the first `k` symbols are the data verbatim, the next `m` are
//! parity. Built on a Vandermonde generator matrix (rows = powers of distinct
//! field elements), which is MDS — any `k` of `n` symbols reconstruct. Decode
//! solves the linear system over GF(256) via the shared Gaussian elimination.
//!
//! Compose: `fountain` for lossy/high-BDP WAN; `rs` for clean fabric / inner
//! layer of hierarchical fan-out.

use crate::gf256::{self, gf_gauss_eliminate, gf_mul};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, thiserror::Error)]
pub enum RsError {
    #[error("empty generation")]
    Empty,
    #[error("symbol size mismatch: {a} vs {b}")]
    SizeMismatch { a: usize, b: usize },
    #[error("need {need} symbols, have {have}")]
    Insufficient { have: usize, need: usize },
}

/// Build a systematic (n x k) Vandermonde generator matrix over GF(256):
/// row i, col j = alpha_i ^ j, with alpha_i = i+1 (distinct nonzero). The top
/// k x k block is then transformed to identity (systematic) by row-reducing,
/// which preserves the MDS property (any k rows are independent because the
/// original Vandermonde has that property and row operations don't change the
/// row span's independence structure across the chosen k-subsets... in practice
/// we keep the parity rows as the Vandermonde rows minus the identity head).
///
/// The generator depends only on `(k, m)`, so it is memoized in a process-wide
/// cache: the O(k³) Vandermonde inversion is paid once per shape, not once per
/// `RsEncoder::new` / `decode`. On a cold start with many same-shape generations
/// (or repeated decodes) this is the difference between O(g·k³) and O(k³).
fn generator(k: usize, m: usize) -> Vec<Vec<u8>> {
    static CACHE: OnceLock<Mutex<HashMap<(usize, usize), Vec<Vec<u8>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(g) = cache.lock().get(&(k, m)).cloned() {
        return g;
    }
    let g = build_generator(k, m);
    cache.lock().insert((k, m), g.clone());
    g
}

fn build_generator(k: usize, m: usize) -> Vec<Vec<u8>> {
    let n = k + m;
    // raw Vandermonde: v[i][j] = (i+1)^j  (i in 0..n, j in 0..k)
    let mut v = vec![vec![0u8; k]; n];
    for i in 0..n {
        let alpha = (i + 1) as u8;
        let mut pow = 1u8;
        for j in 0..k {
            v[i][j] = pow;
            pow = gf_mul(pow, alpha);
        }
    }
    // make the top k x k identity by gaussian-eliminating the first k rows to I,
    // and applying the same row ops to the parity rows. Simpler: invert the
    // top k x k Vandermonde and multiply the parity (bottom m) rows by it.
    let inv = invert_matrix(&v[0..k].to_vec(), k);
    // parity rows = v[k..n] * inv
    let mut g = vec![vec![0u8; k]; n];
    for j in 0..k {
        g[j][j] = 1;
    } // identity head
    for i in 0..m {
        for j in 0..k {
            let mut acc = 0u8;
            for t in 0..k {
                acc ^= gf_mul(v[k + i][t], inv[t][j]);
            }
            g[k + i][j] = acc;
        }
    }
    g
}

/// Invert a k x k GF(256) matrix via Gauss-Jordan. The inner scale/axpy loops
/// use the SIMD `pshufb`/`vqtbl1q` kernels, so the O(k³) work benefits from the
/// same vector speedup as the encode/decode paths.
fn invert_matrix(a: &Vec<Vec<u8>>, k: usize) -> Vec<Vec<u8>> {
    // augment [a | I]
    let mut rows: Vec<Vec<u8>> = a
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut row = vec![0u8; 2 * k];
            row[..k].copy_from_slice(r);
            row[k + i] = 1;
            row
        })
        .collect();
    // forward + back elimination to RREF
    let mut col = 0;
    for r in 0..k {
        // find pivot
        let mut piv = None;
        for rr in r..k {
            if rows[rr][col] != 0 {
                piv = Some(rr);
                break;
            }
        }
        let Some(p) = piv else {
            col += 1;
            continue;
        };
        rows.swap(r, p);
        let inv = gf256::inv(rows[r][col]);
        gf256::gf_scal_slice(inv, &mut rows[r]);
        // Move the pivot row out so the elimination loop can hold `&mut rows[rr]`
        // while reading the pivot immutably — otherwise `rows` is borrowed both
        // ways. Restoring the Vec preserves its allocation (no per-pivot alloc).
        let prow_vec = std::mem::take(&mut rows[r]);
        let prow = prow_vec.as_slice();
        for rr in 0..k {
            if rr != r && rows[rr][col] != 0 {
                let c = rows[rr][col];
                gf256::gf_axpy_slice(c, prow, &mut rows[rr]);
            }
        }
        rows[r] = prow_vec;
        col += 1;
    }
    rows.into_iter().map(|r| r[k..].to_vec()).collect()
}

/// Reed-Solomon encoder for one generation.
pub struct RsEncoder {
    g: Vec<Vec<u8>>,
    k: usize,
    n: usize,
    symbol_size: usize,
}

impl RsEncoder {
    pub fn new(k: usize, m: usize, symbol_size: usize) -> Result<Self, RsError> {
        if k == 0 {
            return Err(RsError::Empty);
        }
        Ok(Self {
            g: generator(k, m),
            k,
            n: k + m,
            symbol_size,
        })
    }
    pub fn k(&self) -> usize {
        self.k
    }
    pub fn n(&self) -> usize {
        self.n
    }

    /// Encode `data` (k symbols of `symbol_size` bytes) into `n` codeword
    /// symbols. The first k are the data verbatim (systematic).
    pub fn encode(&self, data: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, RsError> {
        if data.len() != self.k {
            return Err(RsError::SizeMismatch {
                a: data.len(),
                b: self.k,
            });
        }
        for d in data {
            if d.len() != self.symbol_size {
                return Err(RsError::SizeMismatch {
                    a: d.len(),
                    b: self.symbol_size,
                });
            }
        }
        let mut out = vec![vec![0u8; self.symbol_size]; self.n];
        for i in 0..self.k {
            out[i].copy_from_slice(&data[i]);
        }
        // parity rows. `axpy` fetches the multiply row once and runs a
        // branchless inner loop instead of a per-byte `gf_mul` (which
        // re-loads the static table each byte).
        for i in self.k..self.n {
            for j in 0..self.k {
                let c = self.g[i][j];
                if c == 0 {
                    continue;
                }
                gf256::axpy(&mut out[i], c, &data[j]);
            }
        }
        Ok(out)
    }
}

/// Decode from any `k` of the `n` symbols. `survived` is (index, symbol) pairs.
pub fn decode(
    k: usize,
    m: usize,
    symbol_size: usize,
    survived: &[(usize, Vec<u8>)],
) -> Result<Vec<Vec<u8>>, RsError> {
    if survived.len() < k {
        return Err(RsError::Insufficient {
            have: survived.len(),
            need: k,
        });
    }
    let g = generator(k, m);
    // pick first k survived rows
    let rows_in: Vec<(usize, Vec<u8>)> = survived.iter().take(k).cloned().collect();
    // build augmented [g[idx][..k] | symbol] as a flat row-major buffer: one
    // allocation for the whole matrix, solved in place by Gaussian elimination.
    let row_len = k + symbol_size;
    let mut rows: Vec<u8> = Vec::with_capacity(rows_in.len() * row_len);
    for (idx, sym) in &rows_in {
        let off = rows.len();
        rows.resize(off + row_len, 0);
        rows[off..off + k].copy_from_slice(&g[*idx][..k]);
        rows[off + k..off + row_len].copy_from_slice(sym);
    }
    let rank = gf_gauss_eliminate(&mut rows, row_len, k);
    if rank < k {
        return Err(RsError::Insufficient {
            have: rank,
            need: k,
        });
    }
    // extract: pivot column r -> data[r] = payload
    let mut out = vec![Vec::new(); k];
    for r in rows.chunks(row_len) {
        let mut piv = 0;
        while piv < k && r[piv] == 0 {
            piv += 1;
        }
        if piv < k {
            out[piv] = r[k..k + symbol_size].to_vec();
        }
    }
    if out.iter().any(|s| s.is_empty()) {
        return Err(RsError::Insufficient {
            have: rank,
            need: k,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn any_k_reconstruct() {
        let k = 4;
        let m = 2;
        let sz = 8;
        let data: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..sz).map(|j| ((i * 31 + j) & 0xFF) as u8).collect())
            .collect();
        let enc = RsEncoder::new(k, m, sz).unwrap();
        let cw = enc.encode(&data).unwrap();
        assert_eq!(cw.len(), k + m);
        // drop parity and one data symbol (index 1) -> still k symbols
        let survived: Vec<(usize, Vec<u8>)> =
            [0, 2, 3, 5].iter().map(|&i| (i, cw[i].clone())).collect();
        let rec = decode(k, m, sz, &survived).unwrap();
        assert_eq!(rec, data);
    }
}
