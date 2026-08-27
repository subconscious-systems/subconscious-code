//! Session identity (DESIGN §3.2, §6.1).
//!
//! A `SessionId` is a 128-bit opaque conversation id. It is the key under which
//! the receiver accumulates a per-session log and the Merkle root the receiver
//! ACKs. We expose it as a thin newtype (rather than a bare `u128`) so the
//! transport, shim, and receiver share one type and so session->shard mapping
//! (§6.1 Fibonacci multiplicative hashing) lives next to the identity.
//!
//! The hash constant is `floor(2^64 / phi) = 0x9E3779B97F4A7C15` (Knuth's
//! multiplicative hash, golden-ratio). Consecutive / monotonic session ids
//! spread maximally across the shard space with no clustering.

/// A 128-bit session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u128);

impl SessionId {
    pub fn new(v: u128) -> Self {
        Self(v)
    }
    pub fn raw(self) -> u128 {
        self.0
    }

    /// Map this session to one of `n` shards using Fibonacci multiplicative
    /// hashing (§6.1). `n` must be > 0. Uses the top `ceil(log2 n)` bits of the
    /// golden-ratio product of the low 64 bits, which gives excellent avalanche
    /// and no clustering on strided/monotonic inputs.
    pub fn shard(self, n: usize) -> usize {
        debug_assert!(n > 0);
        if n == 1 {
            return 0;
        }
        const PHI: u64 = 0x9E37_79B9_7F4A_7C15;
        let lo = (self.0 & 0xFFFF_FFFF_FFFF_FFFF) as u64;
        let prod = lo.wrapping_mul(PHI);
        // power-of-two fast path; else modulo on the full product bits.
        if n.is_power_of_two() {
            let b = n.trailing_zeros();
            return (prod >> (64 - b)) as usize;
        }
        (prod % (n as u64)) as usize
    }
}

impl From<u128> for SessionId {
    fn from(v: u128) -> Self {
        Self(v)
    }
}
impl From<u64> for SessionId {
    fn from(v: u64) -> Self {
        Self(v as u128)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session:{:032x}", self.0)
    }
}
