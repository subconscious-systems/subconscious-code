//! Homomorphic hash — securing coded blocks (DESIGN §6.6).
//!
//! RLNC has a pollution-attack surface: one bad coded packet corrupts everything
//! downstream that mixes it. Defend with a homomorphic hash H where a coded
//! packet's hash is computable from the source hashes and the coding
//! coefficients, so a receiver verifies a coded block *without decoding it*.
//!
//! ## Construction
//!
//! A multiplicative hash `H(c) = ∏_i g_i^{c_i} mod P` with integer exponents is
//! homomorphic over **integer** linear combinations, but *not* over GF(2^8):
//! GF(2^8) addition is XOR and GF(2^8) multiplication is carry-free, neither of
//! which equals integer addition/multiplication, so lifting a GF(2^8) linear
//! combination to integer exponents does not preserve the hash. (The original
//! implementation claimed this homomorphism over GF(2^8); it does not hold.)
//!
//! The textbook-correct fix is to hash over a **prime field** `GF(q)`. We use:
//!   - `P = 2^61 − 1` (the Mersenne prime M61) as the hash modulus, so the group
//!     `Z_P*` has order `P − 1`;
//!   - a prime `q` dividing `P − 1` (`q = 1321`) as the coding field order;
//!   - a generator `g` of the unique order-`q` subgroup of `Z_P*`, and
//!     per-coordinate generators `g_i = g^{i+1}` (each of order `q`).
//!
//! Then for coefficient vectors over `GF(q)`:
//! ```text
//!   H(c) = ∏_i g_i^{c_i}  mod P,        c_i ∈ GF(q) lifted to {0..q-1}
//!   H(Σ_j a_j · c_j) = ∏_j H(c_j)^{a_j}      (homomorphism over GF(q))
//! ```
//! because every `g_i` has order `q`, so `g_i^x = g_i^{x mod q}`, and scalar
//! multiplication / addition in `GF(q)` are exactly integer mul/add followed
//! by reduction mod `q` — which is what the exponents reduce by.
//!
//! ## Scope note
//!
//! This module is homomorphic over `GF(q)` (`q = 1321`). The `rlnc` crate codes
//! over `GF(2^8)`; wiring this hash to that layer requires either moving that
//! RLNC to a prime field (`GF(q)`) or adopting a per-symbol large-prime scheme.
//! That integration is left as a follow-up; this module is correct and
//! self-contained in the meantime.
//!
//! `P` is a 64-bit-class prime for speed on the testing/verification path;
//! production would use a 256-bit safe-prime group with a larger `q`.

/// Hash modulus: `P = 2^61 − 1` (Mersenne prime M61). `Z_P*` has order `P − 1`.
pub const P: u128 = (1u128 << 61) - 1;

/// Coding field order: a prime `q` with `q | (P − 1)`, so `Z_P*` contains a
/// unique subgroup of order `q`. `1321` divides `2^60 − 1`, hence `P − 1`.
pub const Q: u32 = 1321;

#[derive(Debug, thiserror::Error)]
pub enum HomoHashError {
    #[error("coefficient vector length mismatch: {a} vs {b}")]
    Length { a: usize, b: usize },
}

/// Reduce `x mod P` for `P = 2^61 − 1` (Mersenne reduction).
#[inline]
fn m_reduce(x: u128) -> u128 {
    // P = 2^61 - 1. Split x = hi * 2^61 + lo.
    let lo = x & P;
    let hi = x >> 61;
    let s = lo + hi; // s = lo + hi, with hi <= x/2^61
    if s >= P {
        s - P
    } else {
        s
    }
}

/// Multiply mod P without overflow: a,b < P ~ 2^61 so a*b fits in u128; reduce.
#[inline]
fn mulmod(a: u128, b: u128) -> u128 {
    m_reduce(a * b)
}

#[inline]
fn powmod(mut base: u128, mut exp: u128, p: u128) -> u128 {
    let mut result: u128 = 1;
    base %= p;
    while exp > 0 {
        if exp & 1 == 1 {
            result = mulmod(result, base);
        }
        base = mulmod(base, base);
        exp >>= 1;
    }
    result
}

/// A generator of the order-`Q` subgroup of `Z_P*`: `g = BASE^((P-1)/Q) mod P`.
/// Since `Q | (P-1)` and `Q` is prime, any such `g ≠ 1` has order exactly `Q`.
fn subgroup_gen() -> u128 {
    debug_assert_eq!((P - 1) % Q as u128, 0, "Q must divide P-1");
    let exp = (P - 1) / Q as u128;
    // Try a few small bases; the first that yields g != 1 has order Q.
    for &base in [2u128, 3, 5, 7, 11, 13, 37, 41, 61].iter() {
        let g = powmod(base, exp, P);
        if g != 1 {
            debug_assert_eq!(powmod(g, Q as u128, P), 1, "g must have order Q");
            return g;
        }
    }
    // Q is prime so an order-Q element exists; search the rest.
    let mut base: u128 = 67;
    loop {
        let g = powmod(base, exp, P);
        if g != 1 {
            return g;
        }
        base += 1;
    }
}

/// A homomorphic hash over `GF(Q)`.
///
/// Precomputes the per-coordinate generators `g_i = g^{i+1}` (each order `Q`)
/// for a fixed generation size `k`, plus the **full power table**
/// `pow_table[i][e] = g_i^e mod P` for `e in 0..Q`. With that, `hash` is
/// `k - nz` `mulmod`s and **no modular exponentiations** — `pow_mod` (an
/// O(log Q) mulmod chain) is replaced by a single table lookup per coordinate.
/// The generators are built incrementally (`g_{i+1} = g_i * g mod P`) instead
/// of O(k) full exponentiations.
pub struct HomomorphicHash {
    /// Per-coordinate generators, each of order `Q`.
    gens: Vec<u128>,
    /// `table[i * Q + e] = g_i^e mod P` for `e in 0..Q` — flattened so `hash`
    /// is a single index + `mulmod` chain with no per-coordinate `powmod`.
    table: Vec<u128>,
    /// Unit hash (`g_i`) → coordinate index, so `combine_many` (used by
    /// `verify_coded`, whose `hashes` are source unit hashes) can also take the
    /// table fast path instead of a per-element `powmod`.
    unit_index: std::collections::HashMap<u128, usize>,
}

impl HomomorphicHash {
    pub fn new(k: usize) -> Self {
        let g = subgroup_gen();
        // g_i = g^{i+1}; order Q (Q prime, i+1 not a multiple of Q for i << Q).
        // Built incrementally: g_{i+1} = g_i * g mod P, one mulmod per step
        // (was one full modular exponentiation per coordinate).
        let mut gens = Vec::with_capacity(k);
        let mut acc: u128 = 1;
        for _ in 0..k {
            acc = mulmod(acc, g);
            gens.push(acc);
        }
        // Full power table: pow_table[i][e] = g_i^e for e in 0..Q.
        let q = Q as usize;
        let mut table = vec![0u128; k * q];
        let mut unit_index = std::collections::HashMap::with_capacity(k);
        for (i, &gi) in gens.iter().enumerate() {
            let row = &mut table[i * q..(i + 1) * q];
            let mut p = 1u128;
            for slot in row.iter_mut() {
                *slot = p;
                p = mulmod(p, gi);
            }
            unit_index.insert(gi, i);
        }
        Self {
            gens,
            table,
            unit_index,
        }
    }

    /// `H(c) = ∏_i g_i^{c_i} mod P`. Coefficients are `GF(Q)` elements
    /// (`0..Q`); values are reduced mod `Q` defensively. `O(K)` `mulmod`s —
    /// each exponentiation is a table lookup.
    pub fn hash(&self, c: &[u32]) -> Result<u128, HomoHashError> {
        if c.len() != self.gens.len() {
            return Err(HomoHashError::Length {
                a: c.len(),
                b: self.gens.len(),
            });
        }
        let q = Q as usize;
        let mut acc: u128 = 1;
        for (i, &ci) in c.iter().enumerate() {
            let e = (ci as usize) % q; // reduce mod Q (g_i has order Q)
            if e != 0 {
                acc = mulmod(acc, self.table[i * q + e]);
            }
        }
        Ok(acc)
    }

    /// Homomorphic combine of two hashes:
    /// `H(a·c1 + b·c2) = H(c1)^a · H(c2)^b mod P`, over `GF(Q)`.
    /// `a`, `b` are `GF(Q)` scalars (`0..Q`); used as integer exponents, which
    /// is correct because `H(c1), H(c2)` lie in the order-`Q` subgroup.
    pub fn combine(a: u32, h1: u128, b: u32, h2: u128) -> u128 {
        let lhs = powmod(h1, (a as u128) % (Q as u128), P);
        let rhs = powmod(h2, (b as u128) % (Q as u128), P);
        mulmod(lhs, rhs)
    }

    /// Combine a full linear combination: `H(Σ_j a_j · c_j) = ∏_j H(c_j)^{a_j}`.
    /// When `hashes` are source unit hashes (the `verify_coded` path), each
    /// exponentiation resolves through the power table via `unit_index`; any
    /// non-unit hash falls back to a `powmod`.
    pub fn combine_many(&self, coeffs: &[u32], hashes: &[u128]) -> u128 {
        let q = Q as usize;
        let mut acc: u128 = 1;
        for (a, h) in coeffs.iter().zip(hashes.iter()) {
            let e = (*a as usize) % q;
            if e != 0 {
                acc = match self.unit_index.get(h) {
                    Some(&i) => mulmod(acc, self.table[i * q + e]),
                    None => mulmod(acc, powmod(*h, e as u128, P)),
                };
            }
        }
        acc
    }
}

/// Verify a coded packet's coefficient vector against the source unit hashes.
/// `source_hashes` are `H(e_i)` for each source symbol `i` (the unit-vector
/// hashes, i.e. `g_i`); `coded_coeffs` is the coded packet's coefficient row
/// over `GF(Q)`; `expected` is the claimed `H(coded_coeffs)`. Returns true iff
/// they match.
pub fn verify_coded(
    hh: &HomomorphicHash,
    source_hashes: &[u128],
    coded_coeffs: &[u32],
    expected: u128,
) -> Result<bool, HomoHashError> {
    if coded_coeffs.len() != source_hashes.len() {
        return Err(HomoHashError::Length {
            a: coded_coeffs.len(),
            b: source_hashes.len(),
        });
    }
    // H(Σ a_i e_i) = ∏ H(e_i)^{a_i}
    let computed = hh.combine_many(coded_coeffs, source_hashes);
    Ok(computed == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(x + y) mod Q` — addition in GF(Q).
    #[inline]
    fn q_add(x: u32, y: u32) -> u32 {
        let s = (x as u64 + y as u64) % Q as u64;
        s as u32
    }

    /// `(x * y) mod Q` — multiplication in GF(Q).
    #[inline]
    fn q_mul(x: u32, y: u32) -> u32 {
        let p = (x as u64 * y as u64) % Q as u64;
        p as u32
    }

    #[test]
    fn homomorphism_holds() {
        let k = 16;
        let hh = HomomorphicHash::new(k);
        let c1: Vec<u32> = (0..k).map(|i| q_mul((i as u32) + 1, 7)).collect();
        let c2: Vec<u32> = (0..k)
            .map(|i| q_add(q_mul((i as u32) + 1, 13), 5))
            .collect();
        let h1 = hh.hash(&c1).unwrap();
        let h2 = hh.hash(&c2).unwrap();

        // c_comb = a·c1 + b·c2 over GF(Q).
        let (a, b) = (3u32, 5u32);
        let c_comb: Vec<u32> = (0..k)
            .map(|i| q_add(q_mul(a, c1[i]), q_mul(b, c2[i])))
            .collect();
        let h_comb = hh.hash(&c_comb).unwrap();
        let h_hom = HomomorphicHash::combine(a, h1, b, h2);
        assert_eq!(h_comb, h_hom, "homomorphism must hold over GF(Q)");
    }

    #[test]
    fn combine_many_matches_direct_hash() {
        // A coded packet that is just a scalar multiple of one source: a·c1.
        let k = 8;
        let hh = HomomorphicHash::new(k);
        let c1: Vec<u32> = (0..k).map(|i| q_mul((i as u32) + 2, 11)).collect();
        let h1 = hh.hash(&c1).unwrap();
        let a = 9u32;
        let scaled: Vec<u32> = (0..k).map(|i| q_mul(a, c1[i])).collect();
        let h_scaled = hh.hash(&scaled).unwrap();
        let h_hom = hh.combine_many(&[a], &[h1]);
        assert_eq!(h_scaled, h_hom);
    }

    #[test]
    fn verify_coded_works() {
        let k = 8;
        let hh = HomomorphicHash::new(k);
        // Source unit hashes: H(e_i) = g_i.
        let unit_hashes: Vec<u128> = (0..k)
            .map(|i| {
                let mut e = vec![0u32; k];
                e[i] = 1;
                hh.hash(&e).unwrap()
            })
            .collect();
        let coded: Vec<u32> = vec![2, 0, 7, 0, 0, 1, 0, 3];
        let claimed = hh.hash(&coded).unwrap();
        assert!(verify_coded(&hh, &unit_hashes, &coded, claimed).unwrap());
        // Tamper: changing one coefficient must break verification.
        let bad: Vec<u32> = vec![2, 0, 7, 0, 0, 1, 0, 4];
        assert!(!verify_coded(&hh, &unit_hashes, &bad, claimed).unwrap());
    }
}
