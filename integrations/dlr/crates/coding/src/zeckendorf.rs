//! Zeckendorf / Fibonacci-coded wire varints (DESIGN §6.3).
//!
//! By Zeckendorf's theorem every positive integer is a *unique* sum of
//! non-consecutive Fibonacci numbers; appending a terminating `1` yields a
//! codeword ending in `11` that appears nowhere internally (no two consecutive
//! 1s internally). Properties:
//!   - self-synchronizing / self-delimiting: after a corrupt run, the decoder
//!     re-locks at the next `11`. No length-prefix framing needed to resync.
//!   - size competitive with Elias-gamma/delta over the offset ranges seen.
//!
//! Matters most on the raw RDMA / coded paths where you don't get QUIC's framing
//! integrity for free. Load-bearing: nice robustness, low cost.
//!
//! Bits are emitted **LSB-first** within each byte: codeword bit `j` (the
//! coefficient of `fibs[j]`) lives at byte `j >> 3`, bit `j & 7`. A codeword is
//! `ceil((hi+1)/8)` bytes — one allocation, 8× denser than a byte-per-bit
//! `Vec<bool>`-style encoding. The terminating `11` sits at the very end of the
//! packed stream (bits `hi-1` and `hi`), so a decoder scanning for the first
//! consecutive `11` still re-locks cleanly.

/// Fibonacci numbers up to 2^64 (the largest Zeckendorf codeword for a 64-bit
/// integer uses ~88 Fibonacci digits). Built once and cached: every
/// `zeck_encode`/`zeck_decode` used to rebuild and allocate this ~90-entry
/// table on the wire hot path.
fn fib_table() -> &'static [u64] {
    use std::sync::OnceLock;
    static FIB: OnceLock<Vec<u64>> = OnceLock::new();
    FIB.get_or_init(|| {
        let mut f = vec![1u64, 2u64];
        while *f.last().unwrap() <= u64::MAX / 2 {
            let n = f.len();
            f.push(f[n - 1].wrapping_add(f[n - 2]));
        }
        f
    })
}

/// Read one bit (LSB-first indexing) at bit index `i`; out-of-range reads `0`.
#[inline]
fn bit_at(buf: &[u8], i: usize) -> u32 {
    if (i >> 3) >= buf.len() {
        0
    } else {
        ((buf[i >> 3] >> (i & 7)) & 1) as u32
    }
}

/// Encode `n` as a Zeckendorf codeword: a bit vector terminated by an extra
/// `1` (so the on-wire form ends in `11`). Returns the codeword, bit-packed.
///
/// Bits are emitted **LSB-first** across the packed bytes. The greedy
/// Zeckendorf decomposition always sets the largest Fibonacci it picks, so the
/// last data bit is always `1`; appending a single terminating `1` reliably
/// yields a trailing `11` — the self-sync sentinel that never occurs internally
/// (Zeckendorf uses non-consecutive Fibonacci numbers, so no two adjacent data
/// bits are both `1`).
///
/// `n` must be positive; `0` is handled by the [`ZeckStream`] layer via a +1
/// shift.
pub fn zeck_encode(n: u64) -> Vec<u8> {
    assert!(
        n > 0,
        "Zeckendorf encodes positive integers; 0 is handled by callers"
    );
    let fibs = fib_table();
    // find the count of Fibonacci numbers <= n; the largest used is fibs[hi-1].
    let mut hi = 0;
    while hi < fibs.len() && fibs[hi] <= n {
        hi += 1;
    }
    // Zeckendorf greedy from highest index down, writing straight into the
    // packed byte buffer (bits LSB-first: bit j <-> fibs[j]).
    let nbits = hi + 1; // hi data bits + 1 terminator
    let mut out = vec![0u8; nbits.div_ceil(8)];
    let mut rem = n;
    let mut idx = hi;
    while idx > 0 {
        idx -= 1;
        if fibs[idx] <= rem {
            out[idx >> 3] |= 1 << (idx & 7);
            rem -= fibs[idx];
        }
    }
    // terminator -> trailing `11` with the last data bit.
    out[hi >> 3] |= 1 << (hi & 7);
    out
}

/// Decode a single Zeckendorf codeword from the front of the packed `bits`,
/// returning the value and the number of **bits** consumed (including the
/// terminator). Returns None if no terminator (`11`) is found.
pub fn zeck_decode(bits: &[u8]) -> Option<(u64, usize)> {
    decode_at(bits, 0)
}

/// Core decoder: read one codeword whose first bit sits at `start`.
///
/// Returns `None` if no terminating `11` exists before the end of the buffer
/// (rather than looping forever on trailing zero padding). For a single
/// well-formed codeword produced by [`zeck_encode`] the terminator always
/// exists, so this never returns `None` on valid input.
fn decode_at(bits: &[u8], start: usize) -> Option<(u64, usize)> {
    // find the first bit pair `11` (the terminator); scanning bit-by-bit keeps
    // the self-sync property even across byte boundaries. Bound the scan by
    // the buffer length so trailing zero padding (after the last codeword in
    // a bit-continuous stream) cannot loop forever.
    let mut i = start;
    loop {
        if (i >> 3) >= bits.len() {
            return None; // ran off the end: no terminator here
        }
        if bit_at(bits, i) == 1 && bit_at(bits, i + 1) == 1 {
            let term = i + 1; // index of the second 1 of the terminating `11`
            let n_data = term - start; // data bits (excluding terminator)
            if n_data == 0 {
                return Some((0, term + 1 - start));
            }
            let fibs = fib_table();
            let mut val: u64 = 0;
            for j in start..term {
                if bit_at(bits, j) != 0 {
                    let fib_idx = j - start; // LSB-first: bit j <-> fibs[j-start]
                    if fib_idx < fibs.len() {
                        val = val.wrapping_add(fibs[fib_idx]);
                    }
                }
            }
            return Some((val, term + 1 - start));
        }
        i += 1;
    }
}

/// A bit-level Zeckendorf stream writer/reader for framing offsets and lengths
/// on the raw RDMA / coded paths.
///
/// Codewords are packed **bit-continuously**: a codeword may start mid-byte, so
/// the on-wire stream is a dense bit stream with no per-codeword byte padding.
/// `decode_all` advances by the decoded bit count (`used`), so the next
/// codeword picks up exactly where the previous one ended — the "codewords can
/// start mid-byte" self-sync property the design relies on. (An earlier version
/// appended each codeword as whole bytes, which byte-aligned the stream and
/// made the bit-advancing decoder misread every codeword after the first.)
pub struct ZeckStream {
    pub buf: Vec<u8>,
    /// Total bits written into `buf` (the bit length of the stream). The last
    /// byte may carry up to 7 bits of zero padding at its top.
    bits: usize,
}

/// Number of meaningful bits in a codeword produced by `zeck_encode` (the index
/// of the highest set bit + 1 — the terminator is always the highest set bit).
#[inline]
fn codeword_nbits(cw: &[u8]) -> usize {
    for (bi, &byte) in cw.iter().enumerate().rev() {
        if byte != 0 {
            return bi * 8 + 8 - byte.leading_zeros() as usize;
        }
    }
    0
}

impl ZeckStream {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            bits: 0,
        }
    }
    /// Push a non-negative integer. Zeckendorf encodes positives, so the stream
    /// shifts by 1: it stores `n + 1` (always positive) and [`decode_all`]
    /// subtracts 1 back. This gives `0` a distinct, self-delimiting codeword.
    ///
    /// The codeword's `nbits` meaningful bits are appended at the current bit
    /// offset `self.bits`, packing across byte boundaries — no per-codeword
    /// byte padding.
    ///
    /// [`decode_all`]: ZeckStream::decode_all
    pub fn push(&mut self, n: u64) {
        let cw = zeck_encode(n + 1);
        let nbits = codeword_nbits(&cw);
        let need = (self.bits + nbits).div_ceil(8);
        if self.buf.len() < need {
            self.buf.resize(need, 0);
        }
        // `cw` is packed LSB-first: codeword bit j is (cw[j>>3] >> (j&7)) & 1.
        // Append at bit offset `self.bits`.
        for j in 0..nbits {
            if (cw[j >> 3] >> (j & 7)) & 1 == 1 {
                let pos = self.bits + j;
                self.buf[pos >> 3] |= 1 << (pos & 7);
            }
        }
        self.bits += nbits;
    }
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// Decode all codewords in a packed buffer, undoing the +1 stream shift.
    /// `i` counts **bits**, so codewords can start mid-byte. Scanning stops
    /// cleanly when `decode_at` runs off the end (no terminator in the
    /// trailing zero padding).
    pub fn decode_all(buf: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut i = 0usize;
        loop {
            match decode_at(buf, i) {
                // decode_at of a stream codeword is always >= 1 (we stored n+1),
                // so the unshift is exact for well-formed streams.
                Some((v, used)) => {
                    out.push(v - 1);
                    i += used;
                }
                None => break,
            }
        }
        out
    }
}

impl Default for ZeckStream {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        for n in [1u64, 2, 3, 7, 10, 100, 1_000_000, u64::MAX / 2] {
            let cw = zeck_encode(n);
            // `nbits` is the *meaningful* bit count (hi+1: data bits + terminator),
            // not `cw.len() * 8` — the codeword is byte-padded with zero bits at
            // the top, so the trailing `11` sits at meaningful bits (nbits-2,
            // nbits-1), not at the padded byte boundary. `zeck_decode` consumes
            // exactly `nbits` bits, so `used == nbits` holds against this count.
            let nbits = codeword_nbits(&cw);
            // exactly one 1-run in the whole codeword, and it is the trailing
            // `11` (bits nbits-2, nbits-1) — no internal `11`.
            let mut run = 0usize;
            let mut runs: Vec<(usize, usize)> = Vec::new();
            for b in 0..=nbits {
                let one = if b < nbits {
                    (cw[b >> 3] >> (b & 7)) & 1
                } else {
                    0
                };
                if one == 1 {
                    run += 1;
                } else {
                    if run > 0 {
                        runs.push((b - run, run));
                    }
                    run = 0;
                }
            }
            // Invariant: Zeckendorf data bits are non-adjacent (no internal
            // `11`), so every run of 1s except the last has length 1, and the
            // last run is exactly the terminating `11` at (nbits-2, 2). (The
            // old assertion demanded a *single* `11` run, which only holds for
            // numbers whose Zeckendorf decomposition is one Fibonacci — it
            // falsely failed every multi-term codeword such as 7 = 5+2.)
            assert!(!runs.is_empty(), "codeword for {n} must have a terminator");
            assert_eq!(
                *runs.last().unwrap(),
                (nbits - 2, 2),
                "codeword for {n} should end in exactly one `11`"
            );
            for &(pos, len) in &runs[..runs.len() - 1] {
                assert_eq!(
                    len, 1,
                    "no internal `11` in codeword for {n}: run at {pos} len {len}"
                );
            }
            let (v, used) = zeck_decode(&cw).unwrap();
            assert_eq!(v, n);
            assert_eq!(used, nbits, "all packed bits consumed for {n}");
        }
    }
    #[test]
    fn stream_roundtrip() {
        let mut s = ZeckStream::new();
        for n in [0u64, 1, 5, 9, 42, 1000] {
            s.push(n);
        }
        let buf = s.finish();
        let got = ZeckStream::decode_all(&buf);
        assert_eq!(got, vec![0, 1, 5, 9, 42, 1000]);
    }
}
