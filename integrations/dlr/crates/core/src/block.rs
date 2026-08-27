//! Semantic content blocks — the unit of replication (DESIGN §3.1).
//!
//! Not byte-CDC chunks: we already have exact JSON boundaries. The unit is the
//! message / tool-call / tool-result block.

use bytes::Bytes;
use xxhash_rust::xxh3::xxh3_64;

/// 256-bit content address.
pub type BlockId = [u8; 32];

/// The kind of semantic block, mirroring Claude Code's content-block taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BlockKind {
    Message = 1,
    ToolCall = 2,
    ToolResult = 3,
    System = 4,
    Summary = 5,
}

impl BlockKind {
    #[inline]
    pub fn to_byte(self) -> u8 {
        self as u8
    }
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Message),
            2 => Some(Self::ToolCall),
            3 => Some(Self::ToolResult),
            4 => Some(Self::System),
            5 => Some(Self::Summary),
            _ => None,
        }
    }
}

/// A semantic content block. `payload` is the canonical byte content of the
/// block (already serialized deterministically by the caller, or built via
/// `canonical_bytes`).
///
/// `seq` is the monotonic append index within the session (1-based); it makes
/// the Merkle ordering explicit and supports the receiver's ordered log.
#[derive(Debug, Clone)]
pub struct Block {
    pub kind: BlockKind,
    pub seq: u64,
    /// Raw canonical content bytes. `Bytes` enables zero-copy slicing across the
    /// loopback boundary and clone-on-write fan-out to N agents (§6.5).
    pub payload: Bytes,
}

impl Block {
    pub fn new(kind: BlockKind, seq: u64, payload: impl Into<Bytes>) -> Self {
        Self {
            kind,
            seq,
            payload: payload.into(),
        }
    }

    /// Content address: BLAKE3(canonical_bytes(self)).
    pub fn block_id(&self) -> BlockId {
        let mut h = blake3::Hasher::new();
        self.canonical_into(&mut h);
        *h.finalize().as_bytes()
    }

    /// 64-bit content fingerprint (xxh3 over the 32-byte block_id). A cheap,
    /// non-cryptographic digest used as a pre-filter for dedup (cuckoo filter)
    /// and as a fast hash-table key before the authoritative BLAKE3 `block_id`
    /// is computed. xxh3 runs at ~30+ GB/s, so hashing 32 bytes is ~1 ns.
    pub fn fingerprint(&self) -> u64 {
        xxh3_64(&self.block_id())
    }

    /// Write the canonical encoding into `h`. Canonical form is deterministic and
    /// length-prefixed: kind || seq_le_8 || len_le_8 || payload. The 17-byte
    /// header is packed into one stack buffer so `block_id` (called on every
    /// block) issues two BLAKE3 updates instead of four, halving per-update
    /// overhead without allocating.
    pub(crate) fn canonical_into(&self, h: &mut blake3::Hasher) {
        let mut hdr = [0u8; 17];
        hdr[0] = self.kind.to_byte();
        hdr[1..9].copy_from_slice(&self.seq.to_le_bytes());
        hdr[9..17].copy_from_slice(&(self.payload.len() as u64).to_le_bytes());
        h.update(&hdr);
        h.update(&self.payload);
    }
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        // content equality by canonical bytes (kind + seq + payload)
        self.kind == other.kind && self.seq == other.seq && self.payload == other.payload
    }
}
impl Eq for Block {}

/// Content address from an already-materialized canonical byte buffer.
/// `blake3(canonical_bytes(block))` is identical to `block.block_id()`
/// (BLAKE3 hashes a single stream, so `update(a);update(b) == update(a||b)`),
/// but avoids re-deriving the canonical bytes when the caller already has
/// them (e.g. for compression). Lets the hot path canonicalize once instead
/// of twice.
pub fn block_id_from_canonical(canon: &[u8]) -> BlockId {
    let mut h = blake3::Hasher::new();
    h.update(canon);
    *h.finalize().as_bytes()
}

impl std::hash::Hash for Block {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.block_id().hash(state);
    }
}
