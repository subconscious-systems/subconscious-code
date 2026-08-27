//! Canonical byte encoding (DESIGN §3.1).
//!
//! Deterministic, length-prefixed encoding so `block_id = BLAKE3(canonical_bytes)`
//! is stable across machines and builds. We avoid JSON canonicalization
//! pitfalls (key ordering, float formats) by using a fixed binary layout:
//!   kind:u8 | seq:u64_le | len:u64_le | payload:bytes

use bytes::Bytes;

use crate::block::{Block, BlockId, BlockKind};

/// Upper bound on a single block's payload length. The canonical header stores
/// `len` as a `u64`; a corrupted or hostile buffer could claim up to 2^64 and
/// drive an allocation of that size before the trailing-length check rejects
/// it. Capping here bounds the allocation to a sane ceiling (a single semantic
/// block is many orders of magnitude smaller than this) and the `17 + len`
/// addition can't overflow. The same constant bounds the wire frame's inline
/// payload (see `frame::MAX_INLINE_PAYLOAD_BYTES`).
pub const MAX_BLOCK_PAYLOAD_BYTES: u64 = 1 << 30; // 1 GiB

/// Canonical bytes of a block. Used for hashing and as the on-wire inline
/// representation.
pub fn canonical_bytes(block: &Block) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 + 8 + block.payload.len());
    out.push(block.kind.to_byte());
    out.extend_from_slice(&block.seq.to_le_bytes());
    out.extend_from_slice(&(block.payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&block.payload);
    out
}

/// Materialize the canonical bytes *and* the `block_id` in a single pass.
///
/// The shim's hot path needs both: the canonical bytes (to compress for the
/// wire) and the `block_id` (to dedup + advance the root). Doing it as
/// `canonical_bytes` then `block_id_from_canonical` reads the whole canonical
/// buffer twice (build, then re-hash). This helper hashes the header and
/// payload directly while building the buffer, so the id is derived without a
/// second full scan of the canonical bytes. The result is identical to
/// `(canonical_bytes(block), block.block_id())`.
pub fn canonical_bytes_and_id(block: &Block) -> (Vec<u8>, BlockId) {
    let mut out = Vec::with_capacity(1 + 8 + 8 + block.payload.len());
    let mut h = blake3::Hasher::new();
    let mut hdr = [0u8; 17];
    hdr[0] = block.kind.to_byte();
    hdr[1..9].copy_from_slice(&block.seq.to_le_bytes());
    hdr[9..17].copy_from_slice(&(block.payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&hdr);
    h.update(&hdr);
    out.extend_from_slice(&block.payload);
    h.update(&block.payload);
    let id = *h.finalize().as_bytes();
    (out, id)
}

/// Parse a block from canonical bytes (inverse of `canonical_bytes`).
pub fn from_canonical(buf: &[u8]) -> Result<Block, &'static str> {
    if buf.len() < 17 {
        return Err("buffer too short for canonical block");
    }
    let kind = BlockKind::from_byte(buf[0]).ok_or("unknown block kind")?;
    let seq = u64::from_le_bytes(buf[1..9].try_into().unwrap());
    let len = u64::from_le_bytes(buf[9..17].try_into().unwrap());
    if len > MAX_BLOCK_PAYLOAD_BYTES {
        return Err("payload length exceeds maximum");
    }
    let len = len as usize;
    let end = 17usize
        .checked_add(len)
        .ok_or("payload length exceeds maximum")?;
    if buf.len() < end {
        return Err("payload truncated");
    }
    let payload = Bytes::copy_from_slice(&buf[17..end]);
    Ok(Block { kind, seq, payload })
}

/// Parse a block from an **owned** canonical buffer, deriving its `block_id` in
/// the same pass and keeping the payload as a zero-copy view of `canon` —
/// the mirror of `canonical_bytes_and_id` on the decode side.
///
/// The receiver's hot path decompresses to an owned `Vec<u8>`, then the old
/// flow did `from_canonical(&canon)` (which `Bytes::copy_from_slice`s the whole
/// payload) *and* `block_id_from_canonical(&canon)` (a second read of the whole
/// buffer to hash it). Here the id is hashed once while parsing, the payload is
/// a refcounted window (`Bytes::from(vec)` + `.slice`, zero copy), and the two
/// stages collapse into one.
pub fn from_canonical_owned(canon: Vec<u8>) -> Result<(Block, BlockId), &'static str> {
    if canon.len() < 17 {
        return Err("buffer too short for canonical block");
    }
    let kind = BlockKind::from_byte(canon[0]).ok_or("unknown block kind")?;
    let seq = u64::from_le_bytes(canon[1..9].try_into().unwrap());
    let len = u64::from_le_bytes(canon[9..17].try_into().unwrap());
    if len > MAX_BLOCK_PAYLOAD_BYTES {
        return Err("payload length exceeds maximum");
    }
    let len = len as usize;
    let end = 17usize
        .checked_add(len)
        .ok_or("payload length exceeds maximum")?;
    if canon.len() < end {
        return Err("payload truncated");
    }
    // Hash header + payload in one pass (identical to `Block::block_id`).
    let mut h = blake3::Hasher::new();
    h.update(&canon[..17]);
    h.update(&canon[17..end]);
    let id = *h.finalize().as_bytes();
    // Take ownership of the whole buffer, then slice the payload out: the
    // slice keeps the original allocation alive via the refcount header, so no
    // payload bytes are copied.
    let buf = Bytes::from(canon);
    let payload = buf.slice(17..end);
    Ok((Block { kind, seq, payload }, id))
}
