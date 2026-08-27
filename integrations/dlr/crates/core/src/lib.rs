//! dlr core: the append-only Merkle-DAG log replication primitives.
//!
//! DESIGN §3. The unit of replication is the **semantic content block**
//! (message / tool-call / tool-result). Each block has a content-addressed
//! `block_id = BLAKE3(canonical_bytes(block))`. The conversation is a Merkle DAG
//! with an incrementally-updatable root:
//!   root_0 = zero
//!   root_N = BLAKE3(root_{N-1} || block_id_N)
//! This is O(1) per append — the steady-state win does not pay any rehashing.

pub mod block;
pub mod canonical;
pub mod filter;
pub mod frame;
pub mod merkle;
pub mod mmr;
pub mod session;
pub mod store;
pub mod wal;

pub use block::{block_id_from_canonical, Block, BlockId, BlockKind};
pub use canonical::{
    canonical_bytes, canonical_bytes_and_id, from_canonical, from_canonical_owned,
};
pub use filter::CuckooFilter;
pub use frame::{
    decode_frame, decode_frame_bytes, encode_frame, AckFrame, AppendFrame, BulkFrame, Frame,
    FrameBlock, FrameError, MissingFrame, ResyncFrame,
};
pub use merkle::{append_root, MerkleRoot, ROOT_ZERO};
pub use mmr::{Mmr, MmrHash};
pub use session::SessionId;
pub use store::{ContentStore, StoreStats};
pub use wal::Wal;
