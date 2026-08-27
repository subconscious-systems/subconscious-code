//! GF(2^8) field arithmetic (DESIGN §6.4, §6.5, §6.6).
//!
//! Shared by the fountain (RaptorQ), RLNC, and homomorphic-hash layers. We use
//! the AES field GF(2^8) with reducing polynomial x^8 + x^4 + x^3 + x + 1
//! (0x11B).
//!
//! Hot-path strategy: a full **256×256 multiply table (64 KB)** is built once
//! and held in a static. Every multiply is then a single branchless table
//! lookup — no log/exp dance, no per-byte zero branch — and the vector
//! primitives (`axpy`, `scal_inplace`, Gaussian inner loops) become tight
//! `dst[i] ^= MUL[a][src[i]]` loops. The 64 KB table is L1-resident, which is
//! faster than log/exp+branch for the large symbols (4 KB) the cold-start bulk
//! moves. exp/log tables are kept only for `inv`/`div`, which act on scalars.

use std::sync::OnceLock;

pub const GF_SIZE: usize = 256;

/// Precomputed tables: full multiply table + log/exp for scalar inverse/div.
struct GfTables {
    /// mul[a][b] = a * b in GF(2^8). 64 KB. Index `[a as usize][b as usize]`.
    mul: [[u8; 256]; 256],
    exp: [u8; 512], // doubled to avoid mod in a few scalar paths
    log: [u8; 256],
}

static TABLES: OnceLock<GfTables> = OnceLock::new();

#[inline]
fn tables() -> &'static GfTables {
    TABLES.get_or_init(build_tables)
}

/// Build the full table set: 256x256 multiply table + log/exp for scalars.
fn build_tables() -> GfTables {
    // Build exp/log by walking the field with generator 3.
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut g: u16 = 1;
    for i in 0..255 {
        exp[i as usize] = g as u8;
        log[g as usize] = i as u8;
        let g2 = (g << 1) ^ (if g & 0x80 != 0 { 0x11B } else { 0 });
        g = (g2 ^ g) & 0xFF;
    }
    for i in 0..256 {
        exp[255 + i] = exp[i];
    }
    log[0] = 0;

    // Build the full 256x256 multiply table from log/exp.
    let mut mul = [[0u8; 256]; 256];
    for a in 1..256u16 {
        let la = log[a as usize] as usize;
        let row = &mut mul[a as usize];
        for b in 1..256u16 {
            row[b as usize] = exp[la + log[b as usize] as usize];
        }
        // row[0] stays 0 (a * 0 == 0); mul[0][*] stays 0 by default.
    }

    GfTables { mul, exp, log }
}

/// O(1) multiply in GF(2^8) — single branchless table lookup.
#[inline]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    tables().mul[a as usize][b as usize]
}

/// Add in GF(2^8) is XOR.
#[inline]
pub fn gf_add(a: u8, b: u8) -> u8 {
    a ^ b
}

/// Build the two 16-byte `pshufb`/`vqtbl1q` lookup tables for multiplying by
/// `a`: `lo[i] = a * i`, `hi[i] = a * (i << 4)` (products reduced in GF(2^8)).
/// By distributivity of GF(2^8) multiplication over XOR-addition,
/// `a * x = lo[x & 0x0f] ^ hi[x >> 4]` for every byte `x` — two parallel 16-entry
/// table lookups instead of a 256-entry gather. This is the ISA-L `pshufb`
/// GF-multiply: a 16-byte operand splits into low/high nibbles, each indexes
/// one table, and the results XOR. Building the 32 bytes is 32 `gf_mul`s once
/// per coefficient — negligible against a multi-KB symbol.
#[inline]
fn mul_nibble_tables(a: u8) -> [u8; 32] {
    let row = &tables().mul[a as usize];
    let mut t = [0u8; 32];
    let mut i = 0;
    while i < 16 {
        t[i] = row[i];
        t[16 + i] = row[i << 4];
        i += 1;
    }
    t
}

/// `dst[i] ^= a * src[i]` over GF(2^8) — the axpy building block, accelerated
/// with `pshufb` (x86 SSSE3/AVX2) or `vqtbl1q` (aarch64 NEON), with a scalar
/// fallback. The SIMD path processes 32-byte (AVX2) / 16-byte (SSSE3/NEON)
/// chunks; the scalar tail handles the remainder. This is the CPU ceiling of
/// the cold-start fountain/RLNC/Bulk encode+decode path — typically 5-10× over
/// the byte-at-a-time gather+XOR.
#[inline]
#[allow(unused_assignments)]
pub fn gf_axpy_slice(a: u8, src: &[u8], dst: &mut [u8]) {
    let n = src.len().min(dst.len());
    if a == 0 || n == 0 {
        return;
    }
    if a == 1 {
        for i in 0..n {
            dst[i] ^= src[i];
        }
        return;
    }
    let tabs = mul_nibble_tables(a);
    let mut i = 0;
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                axpy_avx2(&tabs, src, dst, n);
            }
            i = n - (n % 32);
        } else if is_x86_feature_detected!("ssse3") {
            unsafe {
                axpy_ssse3(&tabs, src, dst, n);
            }
            i = n - (n % 16);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            axpy_neon(&tabs, src, dst, n);
        }
        i = n - (n % 16);
    }
    let row = &tables().mul[a as usize];
    for j in i..n {
        dst[j] ^= row[src[j] as usize];
    }
}

/// `dst[i] = a * dst[i]` over GF(2^8) — scale in place, SIMD-accelerated.
#[inline]
#[allow(unused_assignments)]
pub fn gf_scal_slice(a: u8, dst: &mut [u8]) {
    let n = dst.len();
    if n == 0 {
        return;
    }
    if a == 0 {
        for d in dst.iter_mut() {
            *d = 0;
        }
        return;
    }
    if a == 1 {
        return;
    }
    let tabs = mul_nibble_tables(a);
    let mut i = 0;
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                scal_avx2(&tabs, dst, n);
            }
            i = n - (n % 32);
        } else if is_x86_feature_detected!("ssse3") {
            unsafe {
                scal_ssse3(&tabs, dst, n);
            }
            i = n - (n % 16);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            scal_neon(&tabs, dst, n);
        }
        i = n - (n % 16);
    }
    let row = &tables().mul[a as usize];
    for j in i..n {
        dst[j] = row[dst[j] as usize];
    }
}

// ---- x86_64 SIMD kernels ----
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn axpy_avx2(tabs: &[u8; 32], src: &[u8], dst: &mut [u8], n: usize) {
    use std::arch::x86_64::*;
    let lo = _mm256_broadcastsi128_si256(_mm_loadu_si128(tabs.as_ptr() as *const __m128i));
    let hi = _mm256_broadcastsi128_si256(_mm_loadu_si128(tabs[16..].as_ptr() as *const __m128i));
    let mask = _mm256_set1_epi8(0x0f);
    let mut i = 0;
    while i + 32 <= n {
        let x = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
        let xl = _mm256_and_si256(x, mask);
        // srli_epi16 shifts 16-bit lanes; the &mask after clears the
        // inter-byte contamination, leaving the per-byte high nibble.
        let xh = _mm256_and_si256(_mm256_srli_epi16(x, 4), mask);
        let y = _mm256_xor_si256(_mm256_shuffle_epi8(lo, xl), _mm256_shuffle_epi8(hi, xh));
        let d = _mm256_loadu_si256(dst.as_ptr().add(i) as *const __m256i);
        _mm256_storeu_si256(dst.as_ptr().add(i) as *mut __m256i, _mm256_xor_si256(d, y));
        i += 32;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn axpy_ssse3(tabs: &[u8; 32], src: &[u8], dst: &mut [u8], n: usize) {
    use std::arch::x86_64::*;
    let lo = _mm_loadu_si128(tabs.as_ptr() as *const __m128i);
    let hi = _mm_loadu_si128(tabs[16..].as_ptr() as *const __m128i);
    let mask = _mm_set1_epi8(0x0f);
    let mut i = 0;
    while i + 16 <= n {
        let x = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let xl = _mm_and_si128(x, mask);
        let xh = _mm_and_si128(_mm_srli_epi16(x, 4), mask);
        let y = _mm_xor_si128(_mm_shuffle_epi8(lo, xl), _mm_shuffle_epi8(hi, xh));
        let d = _mm_loadu_si128(dst.as_ptr().add(i) as *const __m128i);
        _mm_storeu_si128(dst.as_ptr().add(i) as *mut __m128i, _mm_xor_si128(d, y));
        i += 16;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn scal_avx2(tabs: &[u8; 32], dst: &mut [u8], n: usize) {
    use std::arch::x86_64::*;
    let lo = _mm256_broadcastsi128_si256(_mm_loadu_si128(tabs.as_ptr() as *const __m128i));
    let hi = _mm256_broadcastsi128_si256(_mm_loadu_si128(tabs[16..].as_ptr() as *const __m128i));
    let mask = _mm256_set1_epi8(0x0f);
    let mut i = 0;
    while i + 32 <= n {
        let x = _mm256_loadu_si256(dst.as_ptr().add(i) as *const __m256i);
        let xl = _mm256_and_si256(x, mask);
        let xh = _mm256_and_si256(_mm256_srli_epi16(x, 4), mask);
        let y = _mm256_xor_si256(_mm256_shuffle_epi8(lo, xl), _mm256_shuffle_epi8(hi, xh));
        _mm256_storeu_si256(dst.as_ptr().add(i) as *mut __m256i, y);
        i += 32;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn scal_ssse3(tabs: &[u8; 32], dst: &mut [u8], n: usize) {
    use std::arch::x86_64::*;
    let lo = _mm_loadu_si128(tabs.as_ptr() as *const __m128i);
    let hi = _mm_loadu_si128(tabs[16..].as_ptr() as *const __m128i);
    let mask = _mm_set1_epi8(0x0f);
    let mut i = 0;
    while i + 16 <= n {
        let x = _mm_loadu_si128(dst.as_ptr().add(i) as *const __m128i);
        let xl = _mm_and_si128(x, mask);
        let xh = _mm_and_si128(_mm_srli_epi16(x, 4), mask);
        let y = _mm_xor_si128(_mm_shuffle_epi8(lo, xl), _mm_shuffle_epi8(hi, xh));
        _mm_storeu_si128(dst.as_ptr().add(i) as *mut __m128i, y);
        i += 16;
    }
}

// ---- aarch64 NEON kernels ----
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn axpy_neon(tabs: &[u8; 32], src: &[u8], dst: &mut [u8], n: usize) {
    use std::arch::aarch64::*;
    let lo = vld1q_u8(tabs.as_ptr());
    let hi = vld1q_u8(tabs[16..].as_ptr());
    let mask = vdupq_n_u8(0x0f);
    let mut i = 0;
    while i + 16 <= n {
        let x = vld1q_u8(src.as_ptr().add(i));
        let xl = vandq_u8(x, mask);
        let xh = vandq_u8(vshrq_n_u8(x, 4), mask);
        let y = veorq_u8(vqtbl1q_u8(lo, xl), vqtbl1q_u8(hi, xh));
        let d = vld1q_u8(dst.as_ptr().add(i));
        vst1q_u8(dst.as_mut_ptr().add(i), veorq_u8(d, y));
        i += 16;
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn scal_neon(tabs: &[u8; 32], dst: &mut [u8], n: usize) {
    use std::arch::aarch64::*;
    let lo = vld1q_u8(tabs.as_ptr());
    let hi = vld1q_u8(tabs[16..].as_ptr());
    let mask = vdupq_n_u8(0x0f);
    let mut i = 0;
    while i + 16 <= n {
        let x = vld1q_u8(dst.as_ptr().add(i));
        let xl = vandq_u8(x, mask);
        let xh = vandq_u8(vshrq_n_u8(x, 4), mask);
        let y = veorq_u8(vqtbl1q_u8(lo, xl), vqtbl1q_u8(hi, xh));
        vst1q_u8(dst.as_mut_ptr().add(i), y);
        i += 16;
    }
}

/// `axpy(dst, a, src)`: `dst[i] ^= gf_mul(a, src[i])` in place — the building
/// block of Gaussian elimination and random linear combinations. SIMD-accelerated
/// via [`gf_axpy_slice`]; the zero and identity coefficients are fast-pathed
/// (`MUL[a][0] == 0` covers the zero case, identity is a plain XOR).
#[inline]
pub fn axpy(dst: &mut [u8], a: u8, src: &[u8]) {
    gf_axpy_slice(a, src, dst);
}

/// Scale a vector in place by a field element: `dst[i] = gf_mul(a, dst[i])`.
/// SIMD-accelerated via [`gf_scal_slice`].
#[inline]
pub fn scal_inplace(dst: &mut [u8], a: u8) {
    gf_scal_slice(a, dst);
}

/// Return a reference to the 256-entry multiply row for `a` — for callers that
/// want the table-lookup inner loop without re-fetching the static per byte.
#[inline]
pub fn mul_row(a: u8) -> &'static [u8; 256] {
    &tables().mul[a as usize]
}

/// Inverse alias matching the name RLNC uses.
#[inline]
pub fn inv(a: u8) -> u8 {
    gf_inv(a)
}

/// Field addition (alias).
#[inline]
pub fn add(a: u8, b: u8) -> u8 {
    a ^ b
}

/// Field multiplication (alias).
#[inline]
pub fn mul(a: u8, b: u8) -> u8 {
    gf_mul(a, b)
}

/// Inverse in GF(2^8) via `exp[-log[a]] = exp[255 - log[a]]`.
#[inline]
pub fn gf_inv(a: u8) -> u8 {
    if a == 0 {
        return 0;
    } // undefined; caller must guard
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// Divide in GF(2^8).
#[inline]
pub fn gf_div(a: u8, b: u8) -> u8 {
    if b == 0 {
        return 0;
    }
    if a == 0 {
        return 0;
    }
    let t = tables();
    let l = (t.log[a as usize] as i32 - t.log[b as usize] as i32).rem_euclid(255) as usize;
    t.exp[l]
}

/// A linear combination: `out = sum coeffs[i] * vecs[i][row]` over GF(2^8).
/// Each term is an SIMD-accelerated axpy into `out`.
pub fn gf_dot(coeffs: &[u8], rows: &[&[u8]]) -> Vec<u8> {
    if rows.is_empty() {
        return Vec::new();
    }
    let len = rows[0].len();
    let mut out = vec![0u8; len];
    for (c, r) in coeffs.iter().zip(rows.iter()) {
        if *c == 0 {
            continue;
        }
        gf_axpy_slice(*c, r, &mut out);
    }
    out
}

/// In-place Gaussian elimination over GF(2^8) on a flat row-major augmented
/// matrix `[A | b]`. `mat` is `n` contiguous rows of `row_len` bytes each
/// (`n = mat.len() / row_len`); the leading `k` bytes of each row are the `A`
/// columns and the remaining `row_len - k` bytes are the augmented RHS. Returns
/// the rank and, if rank == k, leaves the reduced matrix with the solution in
/// the augmented part. The inner stride loops use the precomputed multiply
/// rows so the elimination is a tight `row[j] ^= mul[f][pivot[j]]` loop with
/// no per-byte branch.
///
/// Flat (one allocation for the whole matrix) instead of `Vec<Vec<u8>>` (one
/// allocation per row): the decode hot path builds and solves a residual system
/// per generation, and a single contiguous buffer is kinder to the cache and
/// to the allocator. The normalized pivot row is copied into a single reused
/// buffer so the elimination loop can mutably touch any other row without
/// aliasing the pivot — the `mem::take`/restore dance the row-of-`Vec` version
/// needed doesn't apply to a flat buffer, and the one-row copy per pivot is
/// negligible next to the O(k·n·row_len) elimination work.
pub fn gf_gauss_eliminate(mat: &mut [u8], row_len: usize, k: usize) -> usize {
    if row_len == 0 {
        return 0;
    }
    let n = mat.len() / row_len;
    let t = tables();
    let mut rank = 0;
    // Reusable buffer holding the normalized pivot row for this column.
    let mut prow = vec![0u8; row_len];
    for col in 0..k {
        // find a pivot row at or below `rank` with a nonzero entry in `col`
        let mut piv = None;
        for r in rank..n {
            if mat[r * row_len + col] != 0 {
                piv = Some(r);
                break;
            }
        }
        let Some(p) = piv else {
            continue;
        };
        // swap rows `rank` and `p` (exchange the two contiguous rows in place)
        if rank != p {
            let (lo, hi) = mat.split_at_mut(p * row_len);
            lo[rank * row_len..rank * row_len + row_len].swap_with_slice(&mut hi[..row_len]);
        }
        // normalize the pivot row so its `col` entry == 1 (SIMD-accelerated)
        let prow_off = rank * row_len;
        let pv = mat[prow_off + col];
        let inv = t.exp[255 - t.log[pv as usize] as usize]; // 1/pv
        gf_scal_slice(inv, &mut mat[prow_off..prow_off + row_len]);
        // snapshot the normalized pivot row; the elimination loop reads it
        // immutably while writing every other row.
        prow.copy_from_slice(&mat[prow_off..prow_off + row_len]);
        // eliminate `col` from every other row: rr ^= f * prow (SIMD-accelerated
        // — the gfmul-by-table gather becomes a pshufb/TBL nibble-pair lookup).
        for r in 0..n {
            if r == rank {
                continue;
            }
            let roff = r * row_len;
            let f = mat[roff + col];
            if f == 0 {
                continue;
            }
            gf_axpy_slice(f, &prow, &mut mat[roff..roff + row_len]);
        }
        rank += 1;
        if rank == n {
            break;
        }
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn field_laws() {
        // a * inv(a) == 1 for a != 0
        for a in 1..=255u8 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1);
        }
        // distributivity
        for a in 0..=255 {
            for b in 0..=255 {
                for c in 0..=255 {
                    let lhs = gf_mul(a, gf_add(b, c));
                    let rhs = gf_add(gf_mul(a, b), gf_mul(a, c));
                    assert_eq!(lhs, rhs);
                }
            }
        }
    }

    /// The SIMD axpy/scal paths must match the scalar table-lookup path byte
    /// for byte across every coefficient, including lengths that exercise the
    /// scalar tail (non-multiple of the SIMD chunk width) and the a==1/a==0
    /// fast paths.
    #[test]
    fn simd_matches_scalar() {
        // deterministic source bytes (no Math.random in cfg(test) builds is
        // fine here — this is a normal test, but keep it deterministic anyway)
        let src: Vec<u8> = (0..1000)
            .map(|i| (i as u32).wrapping_mul(2654435761) as u8)
            .collect();
        let lengths = [
            0usize, 1, 15, 16, 17, 31, 32, 33, 63, 64, 100, 127, 128, 999,
        ];
        for &a in &[0u8, 1, 2, 3, 7, 17, 128, 200, 255] {
            for &len in &lengths {
                let s = &src[..len.min(src.len())];
                // axpy
                let mut scalar = vec![0u8; len];
                let mut simd = vec![0u8; len];
                for _ in 0..3 {
                    // run a few times to also exercise the xor-accumulate path
                    let mut tmp = s.to_vec();
                    for (d, &v) in scalar.iter_mut().zip(tmp.iter()) {
                        *d ^= gf_mul(a, v);
                    }
                    tmp.copy_from_slice(s);
                    gf_axpy_slice(a, &tmp, &mut simd);
                }
                assert_eq!(scalar, simd, "axpy mismatch a={a} len={len}");

                // scal (dst = a*dst), seed dst with nonzero
                let mut scalar = vec![];
                let mut simd = vec![];
                for i in 0..len {
                    let v = (i as u32).wrapping_mul(2246822519) as u8 | 1;
                    scalar.push(v);
                    simd.push(v);
                }
                let row = &tables().mul[a as usize];
                for d in scalar.iter_mut() {
                    *d = row[*d as usize];
                }
                gf_scal_slice(a, &mut simd);
                assert_eq!(scalar, simd, "scal mismatch a={a} len={len}");
            }
        }
    }
}
