//! Local shim (DESIGN §2, §3).
//!
//! Sits between Claude Code and the expensive/limited hop. Receives the *full*
//! 50M array over loopback (free, downstream of all ceilings), does **framing +
//! dedup + coding only** — never prunes, holds no policy. Dedup is over opaque
//! hashes and reveals nothing.
//!
//! Steady state: each turn ingests the new blocks, computes block ids, dedups
//! against the local cache, advances the session root incrementally, and emits
//! an `APPEND` frame carrying only the delta since the receiver's last ACKed
//! `base_root`. Per-turn wire bytes = size of the new turn (the 500× win).
//!
//! Cold start: emits a `RESYNC` manifest (ordered block ids) then a coded,
//! resumable `BULK` stream (fountain-coded, §6.4) of any blocks the receiver is
//! missing.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use rayon::prelude::*;

use dlr_compress::Compressor;
use dlr_core::{
    append_root, canonical_bytes, canonical_bytes_and_id, AppendFrame, Block, BlockId, BulkFrame,
    ContentStore, CuckooFilter, FrameBlock, MerkleRoot, ResyncFrame, ROOT_ZERO,
};

#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error("coding error: {0}")]
    Coding(String),
    #[error("compress error: {0}")]
    Compress(String),
    #[error("session {0} unknown to receiver; cold start required")]
    ColdStartRequired(u128),
}

/// Per-session shim state. The shim holds one of these per active session.
pub struct SessionShim {
    pub session_id: u128,
    /// Local content-addressed cache (shared across sessions on this host).
    store: ContentStore,
    /// Ordered block ids the client has produced this session (for cold-start
    /// manifest and root computation).
    ids: Vec<BlockId>,
    /// Current client root.
    root: MerkleRoot,
    /// Last root the receiver ACKed (the base for the next APPEND delta).
    base_root: MerkleRoot,
    /// Index into `ids` corresponding to `base_root` (the delta starts here).
    base_len: usize,
    /// Block compressor for the cold-start bulk path (zstd level 19 — max ratio
    /// for the one-time ~200 MB transfer, where CPU is amortized across the
    /// whole log).
    compressor: Compressor,
    /// Block compressor for the per-turn append hot path (zstd level 3 — fast
    /// on KB-scale deltas, where ratio matters far less than latency). Shares
    /// the bulk compressor's dictionary; decompression is level-agnostic.
    append_compressor: Compressor,
    /// Cuckoo filter mirroring the store's ids (extra strategy): lock-free
    /// negative path so novel blocks skip the store read-lock entirely.
    filter: Arc<CuckooFilter>,
    /// Fountain coding parameters for cold-start bulk transfer.
    fountain_k: u32,
    fountain_symbol_size: u32,
}

impl SessionShim {
    /// zstd level for the per-turn append path. KB-scale deltas compress nearly
    /// as well at level 3 as at 19 but in a fraction of the CPU — the hot path is
    /// latency-bound, and the big-ratio win belongs to the cold-start bulk path.
    const APPEND_LEVEL: i32 = 3;

    pub fn new(session_id: u128, store: ContentStore, compressor: Compressor) -> Self {
        Self::with_filter(
            session_id,
            store,
            compressor,
            Arc::new(CuckooFilter::with_capacity(1 << 20)),
        )
    }

    pub fn with_filter(
        session_id: u128,
        store: ContentStore,
        compressor: Compressor,
        filter: Arc<CuckooFilter>,
    ) -> Self {
        Self {
            session_id,
            store,
            ids: Vec::new(),
            root: ROOT_ZERO,
            base_root: ROOT_ZERO,
            base_len: 0,
            append_compressor: compressor.with_level(Self::APPEND_LEVEL),
            compressor,
            filter,
            fountain_k: 64,
            fountain_symbol_size: 1024,
        }
    }

    pub fn root(&self) -> MerkleRoot {
        self.root
    }
    pub fn base_root(&self) -> MerkleRoot {
        self.base_root
    }
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Ingest a batch of new blocks from Claude Code (one turn's content blocks).
    /// Computes ids, dedups against the local cache, advances the root, and
    /// returns the `APPEND` frame carrying only the blocks the receiver does not
    /// yet have (delta since `base_root`).
    ///
    /// Blocks whose content already lives in the store are emitted as
    /// `FrameBlock::Ref` (no payload re-sent) — the dedup win for repeated
    /// content within a turn.
    pub fn ingest_turn(&mut self, blocks: Vec<Block>) -> AppendFrame {
        let mut frame_blocks: Vec<FrameBlock> = Vec::with_capacity(blocks.len());
        for b in blocks {
            // Materialize the canonical bytes *and* derive the block id in a
            // single pass: `canonical_bytes_and_id` builds the canonical buffer
            // while feeding the same header/payload bytes into one BLAKE3
            // hasher, so the id is computed without a second full scan of the
            // canonical bytes. The previous form did `canonical_bytes(&b)`
            // (alloc + full payload copy) then `block_id_from_canonical(&c)`
            // (a *second* full pass over `c` to hash it) — every inline block's
            // payload was traversed twice on the per-turn hot path.
            let (c, id) = canonical_bytes_and_id(&b);
            // advance client state
            self.ids.push(id);
            self.root = append_root(&self.root, &id);

            // dedup: if the store already has this payload (from this or another
            // session), reference it; otherwise insert inline. We also compress
            // inline payloads before placing them on the wire (§7).
            //
            // Cuckoo filter fast path (extra strategy): a "definitely absent"
            // answer skips the store read-lock entirely — the common case for
            // novel per-turn content. A "maybe present" answer falls back to the
            // authoritative store lookup.
            let known = if self.filter.contains(&id) {
                self.store.contains(&id)
            } else {
                false
            };
            if known {
                frame_blocks.push(FrameBlock::Ref(id));
                // still reference it in *this* session's log so the root matches.
                let _ = self.store.reference(self.session_id, id);
            } else {
                // compress the canonical payload for transport. The append hot
                // path uses the fast-level compressor (level 3); the cold-start
                // bulk path (`bulk_frames_for`) uses the max-ratio one (level 19).
                let z = self
                    .append_compressor
                    .compress(&c)
                    .unwrap_or_else(|_| c.clone());
                // store the *uncompressed* block (the runtime needs fidelity);
                // only the wire payload is compressed. We mark compression on the
                // wire via the compressor's own leading marker, carried in payload.
                // `c` is handed to the store so a durable WAL insert logs the
                // caller's canonical buffer instead of re-deriving it.
                let _ = self
                    .store
                    .insert_with_canonical(self.session_id, b.clone(), id, Some(&c));
                self.filter.insert(&id);
                // Replace the block's payload with the compressed canonical bytes
                // for the wire. The receiver decompresses before storing.
                let wire_block = Block {
                    kind: b.kind,
                    seq: b.seq,
                    payload: Bytes::from(z),
                };
                frame_blocks.push(FrameBlock::Inline(wire_block));
            }
        }

        AppendFrame {
            session_id: self.session_id,
            base_root: self.base_root,
            blocks: frame_blocks,
        }
    }

    /// Ingest several turns' worth of blocks and emit a single coalesced
    /// `APPEND` frame (extra strategy: batch coalescing). On high-turn-rate
    /// sessions, per-turn framing + base_root overhead and per-frame receiver
    /// lock acquisition dominate the small delta. Coalescing N turns into one
    /// frame amortizes that to 1/N, and the receiver ACKs once for the batch —
    /// so the shim advances `base_root` in one round trip instead of N.
    pub fn ingest_coalesced(&mut self, turns: Vec<Vec<Block>>) -> AppendFrame {
        let flat: Vec<Block> = turns.into_iter().flatten().collect();
        self.ingest_turn(flat)
    }

    /// Apply a receiver ACK: advance `base_root` / `base_len` to the ACKed root.
    /// Returns true if the ACK advanced the base (i.e. matched a known root).
    pub fn apply_ack(&mut self, acked: MerkleRoot) -> bool {
        // Fast path: the receiver almost always ACKs the *current* root (the
        // latest turn). Catching that here is O(1) and skips the O(N) BLAKE3
        // re-scan below — meaningful on a 50M-token log where the scan would
        // re-hash every block id per ACK.
        if acked == self.root {
            self.base_root = acked;
            self.base_len = self.ids.len();
            return true;
        }
        if acked == self.base_root {
            return true;
        }
        // find the prefix length whose root == acked
        let mut r = ROOT_ZERO;
        for (i, id) in self.ids.iter().enumerate() {
            r = append_root(&r, id);
            if r == acked {
                self.base_root = acked;
                self.base_len = i + 1;
                return true;
            }
        }
        false
    }

    /// Build a cold-start RESYNC frame: the full ordered manifest of block ids.
    /// ~100KB for a 50M-token log (§3.3).
    pub fn resync_frame(&self) -> ResyncFrame {
        ResyncFrame {
            session_id: self.session_id,
            client_root: self.root,
            manifest: self.ids.clone(),
        }
    }

    /// Build the cold-start BULK frames: fountain-coded, resumable transfer of
    /// the blocks the receiver is missing. Here "missing" defaults to all blocks
    /// since `base_len` (the receiver named the missing set; the shim assumes
    /// everything after the ACKed base is needed on a cold gateway).
    ///
    /// Splits the missing payload into generations of `fountain_k` symbols of
    /// `fountain_symbol_size` bytes and fountain-codes each generation, emitting
    /// K + a small repair margin so the receiver decodes from any K(1+ε).
    pub fn bulk_frames(&self, repair_margin_pct: u32) -> Result<Vec<BulkFrame>, ShimError> {
        let missing: Vec<BlockId> = self.ids[self.base_len..].to_vec();
        self.bulk_frames_for(&missing, repair_margin_pct)
    }

    /// Build BULK frames for an *arbitrary* receiver-named missing set (§3.3
    /// step 3: sparse reconstruction). After a RESYNC the receiver answers with
    /// a `MissingFrame` listing exactly the ids it lacks; the sender codes only
    /// those. On a warm-but-diverged receiver (a network blip that left partial
    /// state) this set can be far smaller than "everything after base", so the
    /// cold-start bulk scales with the *gap*, not the whole tail.
    pub fn bulk_frames_for(
        &self,
        missing: &[BlockId],
        repair_margin_pct: u32,
    ) -> Result<Vec<BulkFrame>, ShimError> {
        if missing.is_empty() {
            return Ok(Vec::new());
        }

        // Serialize + compress each missing block into a self-framed record
        // `[len:u32][compressed canonical bytes]`. We keep the records separate
        // so we can shard the stream at *block* boundaries (below): every
        // generation then contains only whole records, so the receiver's
        // per-generation parser can split its decoded bytes back into blocks
        // without a record ever straddling a generation boundary. (Splitting the
        // flat stream at fixed byte offsets instead — `k * sym` per generation —
        // would cut records mid-frame and leave every generation after the first
        // starting in the middle of a record, which the parser reads as garbage.)
        let mut records: Vec<Vec<u8>> = Vec::with_capacity(missing.len());
        for id in missing {
            if let Some(b) = self.store.get(id) {
                let c = canonical_bytes(&b);
                let z = self
                    .compressor
                    .compress(&c)
                    .map_err(|e| ShimError::Compress(e.to_string()))?;
                let mut rec = Vec::with_capacity(4 + z.len());
                rec.extend_from_slice(&(z.len() as u32).to_le_bytes());
                rec.extend_from_slice(&z);
                records.push(rec);
            }
        }

        let k = self.fountain_k as usize;
        let sym = self.fountain_symbol_size as usize;
        let full_gen_bytes = k * sym;
        let repair_fraction = (repair_margin_pct as f64) / 100.0;

        // Shard the records into generations at block boundaries: pack as many
        // whole records as fit in one `k * sym` generation, then start the next.
        // A record larger than a full generation becomes its own (oversized)
        // generation whose `gen_size` is sized to the record so the fountain
        // still has a uniform K.
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        for rec in &records {
            if !cur.is_empty() && cur.len() + rec.len() > full_gen_bytes {
                chunks.push(std::mem::take(&mut cur));
            }
            cur.extend_from_slice(rec);
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }

        // Encode each chunk as one independent fountain generation, in parallel
        // across chunks. `dlr_coding::bulk::encode` pads each chunk out to a
        // uniform K (= its symbol count) with zero symbols the receiver's parser
        // discards at the first `[len == 0]`. Each generation is independently
        // decodable, so out-of-order BULK delivery (multipath / multicast) is
        // correct and the receiver decodes whichever generations finish first.
        let sid = self.session_id;
        let frames: Result<Vec<BulkFrame>, ShimError> = chunks
            .par_iter()
            .enumerate()
            .map(|(gen, chunk)| {
                let gen_size = chunk.len().div_ceil(sym).max(1);
                let cfg = dlr_coding::bulk::BulkConfig {
                    gen_size,
                    symbol_size: sym,
                    repair_fraction,
                    generations: 0, // `encode` derives the count from the payload
                };
                let coded = dlr_coding::bulk::encode(chunk, &cfg)
                    .map_err(|e| ShimError::Coding(e.to_string()))?;
                // `encode` numbers its single generation 0; renumber to this
                // chunk's generation id so the receiver's per-generation
                // decoders stay distinct across chunks.
                let symbols = coded.into_iter().map(|(_, w)| Bytes::from(w)).collect();
                Ok(BulkFrame {
                    session_id: sid,
                    generation: gen as u32,
                    k: gen_size as u32,
                    symbol_size: sym as u32,
                    symbols,
                })
            })
            .collect();
        frames
    }
}

/// A multi-session shim coordinator. Holds sessions in a sharded registry; each
/// Claude Code session maps to one `SessionShim`. The registry is a `DashMap` so
/// per-session work (`ingest_turn` — compress + dedup + root advance) on one
/// session does not take a lock that blocks every other session: the old
/// `Mutex<HashMap>` form serialized all sessions' hot-path work behind one
/// mutex, defeating the multi-session fan-in the shim exists to serve.
pub struct Shim {
    store: ContentStore,
    compressor: Arc<Mutex<Compressor>>,
    filter: Arc<CuckooFilter>,
    sessions: DashMap<u128, SessionShim>,
}

impl Shim {
    pub fn new(store: ContentStore, compressor: Compressor) -> Self {
        Self::with_filter(
            store,
            compressor,
            Arc::new(CuckooFilter::with_capacity(1 << 20)),
        )
    }

    /// Build with a shared cuckoo filter (e.g. pre-warmmed from an existing store).
    pub fn with_filter(
        store: ContentStore,
        compressor: Compressor,
        filter: Arc<CuckooFilter>,
    ) -> Self {
        Self {
            store,
            compressor: Arc::new(Mutex::new(compressor)),
            filter,
            sessions: DashMap::new(),
        }
    }

    pub fn session(&self, session_id: u128) -> SessionShim {
        // Bind the shared fields the closure needs *before* the entry call so the
        // closure borrows only those fields, leaving `self.sessions` free to be
        // borrowed by `entry` (disjoint field borrows).
        let store = &self.store;
        let compressor = &self.compressor;
        let filter = &self.filter;
        self.sessions
            .entry(session_id)
            .or_insert_with(|| {
                SessionShim::with_filter(
                    session_id,
                    store.clone(),
                    compressor.lock().clone(),
                    filter.clone(),
                )
            })
            .clone_shallow()
    }

    /// Ingest a turn for a session, returning the APPEND frame.
    pub fn ingest(&self, session_id: u128, blocks: Vec<Block>) -> AppendFrame {
        let store = &self.store;
        let compressor = &self.compressor;
        let filter = &self.filter;
        let mut s = self.sessions.entry(session_id).or_insert_with(|| {
            SessionShim::with_filter(
                session_id,
                store.clone(),
                compressor.lock().clone(),
                filter.clone(),
            )
        });
        s.ingest_turn(blocks)
    }

    /// Coalesce many turns into one APPEND frame (extra strategy).
    pub fn ingest_batch(&self, session_id: u128, turns: Vec<Vec<Block>>) -> AppendFrame {
        let store = &self.store;
        let compressor = &self.compressor;
        let filter = &self.filter;
        let mut s = self.sessions.entry(session_id).or_insert_with(|| {
            SessionShim::with_filter(
                session_id,
                store.clone(),
                compressor.lock().clone(),
                filter.clone(),
            )
        });
        s.ingest_coalesced(turns)
    }

    pub fn apply_ack(&self, session_id: u128, root: MerkleRoot) -> bool {
        if let Some(mut s) = self.sessions.get_mut(&session_id) {
            s.apply_ack(root)
        } else {
            false
        }
    }

    /// Expose the shared content store (for the demo / observability).
    pub fn store(&self) -> &ContentStore {
        &self.store
    }

    /// Ordered block ids the client has produced for a session.
    pub fn store_session_ids(&self, session_id: u128) -> Vec<BlockId> {
        self.store.session_ids(session_id)
    }

    /// Current client root for a session.
    pub fn store_session_root(&self, session_id: u128) -> MerkleRoot {
        self.store.session_root(session_id)
    }
}

// helper: SessionShim is not Clone (contains Compressor which may be non-Clone
// depending on dict), so the coordinator stores owned sessions and never clones.
impl SessionShim {
    fn clone_shallow(&self) -> Self {
        // not used externally; provided so `Shim::session` could hand out a view.
        Self {
            session_id: self.session_id,
            store: self.store.clone(),
            ids: self.ids.clone(),
            root: self.root,
            base_root: self.base_root,
            base_len: self.base_len,
            compressor: self.compressor.clone(),
            append_compressor: self.append_compressor.clone(),
            filter: self.filter.clone(),
            fountain_k: self.fountain_k,
            fountain_symbol_size: self.fountain_symbol_size,
        }
    }
}

// ensure BlockKind import isn't flagged unused if API narrows later
#[allow(unused_imports)]
use dlr_core::Frame as _;
