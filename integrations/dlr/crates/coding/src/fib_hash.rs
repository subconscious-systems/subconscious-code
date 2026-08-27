//! Fibonacci (multiplicative) hashing — session -> shard (DESIGN §6.1).
//!
//!   h(k) = (k * 0x9E3779B97F4A7C15) >> (64 - b)   // top b bits
//!
//! `0x9E3779B97F4A7C15 = floor(2^64 / phi)`, odd. Because phi is the "most
//! irrational" number (continued fraction [1;1,1,...], worst rational
//! approximability), consecutive keys spread maximally across the table with
//! excellent avalanche and no clustering on strided inputs (e.g. monotonic session
//! counters). Cheap, no modulo. Load-bearing: solid, standard.

/// The golden-ratio multiplicative constant for 64-bit hashing.
pub const GOLDEN64: u64 = 0x9E3779B9_7F4A7C15; // floor(2^64 / phi)

/// Map a 128-bit key to a `b`-bit shard index using Fibonacci multiplicative
/// hashing.
///
/// The high 64 bits of a 128-bit session id are often zero (most real session
/// ids fit in 64 bits), so hashing `hi * GOLDEN ^ lo` and taking the *top* `b`
/// bits — as an earlier version did — collapses all small keys onto shard 0
/// (the top bits of `lo` are `lo` itself for `lo < 2^(64-b)`). The fix is to
/// Fibonacci-hash the **low** 64-bit lane (`lo * GOLDEN`) and take the top `b`
/// bits of that product, which is the textbook Fibonacci-hash shard selector
/// and spreads consecutive 64-bit keys maximally. We still fold the high lane
/// in by XOR so full 128-bit keys also avalanche.
#[inline]
pub fn fib_hash64(key: u128, b: u32) -> usize {
    let lo = key as u64;
    // Fibonacci-hash the low lane (the lane that actually varies for small
    // keys) and take the top `b` bits of the product. The high lane is folded
    // in by XOR *before* the multiply so 128-bit keys avalanche too, but it
    // cannot zero-out the result the way the old `hi * GOLDEN` path did.
    let mixed = lo ^ (key >> 64) as u64;
    ((mixed.wrapping_mul(GOLDEN64)) >> (64 - b)) as usize
}

/// 64-bit variant (for non-128-bit keys). Top `b` bits of `key * GOLDEN`.
#[inline]
pub fn fib_hash64_u64(key: u64, b: u32) -> usize {
    ((key.wrapping_mul(GOLDEN64)) >> (64 - b)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distributes_consecutive_keys() {
        // consecutive session counters should land in many distinct shards
        let b = 8; // 256 shards
        let mut seen = std::collections::HashSet::new();
        for i in 0..256u128 {
            seen.insert(fib_hash64(i, b));
        }
        // Fibonacci hashing of consecutive keys hits most shards; allow a
        // margin for the collisions a 256-shard / 256-key probe can have
        // (221/256 is the expected ~86% coverage for a near-universal hash).
        assert!(
            seen.len() > 200,
            "consecutive keys should spread widely: {}",
            seen.len()
        );
    }

    #[test]
    fn small_keys_do_not_collapse() {
        // The bug this fixed: small 128-bit keys (hi=0) used to all land on
        // shard 0. They must now spread across the shard space.
        let b = 8;
        let shards: std::collections::HashSet<usize> =
            (0..64u128).map(|i| fib_hash64(i, b)).collect();
        assert!(shards.len() > 40, "small keys collapsed: {}", shards.len());
    }
}
