//! Merkle DAG root (DESIGN §3.1, §3.2).
//!
//!   root_0 = ROOT_ZERO
//!   root_N = BLAKE3(root_{N-1} || block_id_N)
//!
//! O(1) per append — the steady-state win pays no rehashing. The single
//! `base_root` the receiver ACKs replaces any per-block manifest on the hot
//! path: "we already agree up to here."

use blake3::Hasher;

use crate::block::BlockId;

/// A 32-byte Merkle root committing to "everything up to here".
pub type MerkleRoot = [u8; 32];

/// The empty-log root (all zeros). Distinct from any BLAKE3 output.
pub const ROOT_ZERO: MerkleRoot = [0u8; 32];

/// Compute the next root given the previous root and the new block id.
/// `root_N = BLAKE3(root_{N-1} || block_id_N)`.
#[inline]
pub fn append_root(prev: &MerkleRoot, block_id: &BlockId) -> MerkleRoot {
    // Pack both 32-byte inputs into one 64-byte stack buffer and hash in a
    // single update. BLAKE3 processes a single stream, so this is identical to
    // two `update` calls but with one update call overhead per append —
    // meaningful on the per-block hot path.
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(prev);
    buf[32..].copy_from_slice(block_id);
    let mut h = Hasher::new();
    h.update(&buf);
    *h.finalize().as_bytes()
}

/// Compute the root over an ordered sequence of block ids from a starting root.
/// Used on cold start to verify the receiver's reconstruction against the
/// client's manifest.
pub fn root_over(start: &MerkleRoot, ids: &[BlockId]) -> MerkleRoot {
    let mut cur = *start;
    for id in ids {
        cur = append_root(&cur, id);
    }
    cur
}
