//! Receiver (DESIGN §2, §3.3, §6.4).
//!
//! Accumulates the per-session log in a content-addressed store and hands the
//! runtime a *pointer*, never a rebuild. Decodes APPEND deltas (decompress +
//! store + dedup), resolves Ref blocks locally, handles RESYNC (names the
//! missing set) and BULK (fountain-decode + store), and emits ACKs of the
//! session root.
//!
//! Importantly, reconstruction + store are cheap and continuous; the prune runs
//! on its own compute pool (§4), off the transfer hot path. The receiver never
//! blocks on the prune.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use rayon::prelude::*;

use dlr_coding::fountain::{FountainDecoder, FountainError};
use dlr_compress::Compressor;
use dlr_core::{
    from_canonical_owned, AckFrame, AppendFrame, Block, BlockId, BulkFrame, ContentStore, Frame,
    FrameBlock, MerkleRoot, MissingFrame, ResyncFrame,
};

#[derive(Debug, thiserror::Error)]
pub enum ReceiverError {
    #[error("fountain: {0}")]
    Fountain(String),
    #[error("compress: {0}")]
    Compress(String),
    #[error("frame: {0}")]
    Frame(String),
    #[error("referenced block id not in store")]
    MissingRef,
    #[error("session {0} not found")]
    NoSession(u128),
    #[error("session {0} cold start in progress; bulk transfer not yet complete")]
    ColdStartInProgress(u128),
    #[error("session {session_id} base root mismatch: client {client:?}, receiver {current:?}")]
    BaseRootMismatch {
        session_id: u128,
        client: MerkleRoot,
        current: MerkleRoot,
    },
}

/// A handle the runtime uses to access an assembled session log without
/// rebuilding it — a pointer (session id + current root).
#[derive(Debug, Clone, Copy)]
pub struct LogPointer {
    pub session_id: u128,
    pub root: MerkleRoot,
    pub len: usize,
}

/// Per-session receiver state.
///
/// The ordered id list and the running root are **not** duplicated here: the
/// store's session log is the single source of truth for both. A cold start
/// `seed_session`s the store's log in manifest (authoritative) order at RESYNC
/// and BULK fills in content without appending, so the store's
/// `session_ids`/`session_root` are always in manifest order — even after an
/// out-of-order cold start — and a steady-state APPEND extends them in arrival
/// order. `SessionState` holds only the cold-start bookkeeping the store can't
/// recover on its own: the pending set, the acked flag, and the in-flight
/// per-generation decoders.
struct SessionState {
    /// The set of manifest block ids the receiver has not yet recovered via
    /// BULK. Non-empty between RESYNC and cold-start completion; empty once the
    /// session is synced (or for a steady-state session that never cold-started).
    /// This is the cold-start completion oracle: `pending.is_empty()` means the
    /// receiver has every manifest block and may ACK so the shim advances
    /// `base_root` and resumes steady state.
    pending: HashSet<BlockId>,
    /// Whether the receiver has already emitted the cold-start completion ACK
    /// for this session. Guards against re-ACKing on late/duplicate BULK frames.
    acked: bool,
    /// In-flight fountain decoders by generation (cold start). Each is behind
    /// its own mutex so a generation's decode runs without the sessions lock.
    decoders: HashMap<u32, Arc<Mutex<FountainDecoder>>>,
}

/// The receiver: a content-addressed store plus per-session logs.
pub struct Receiver {
    store: ContentStore,
    compressor: Compressor,
    sessions: Mutex<HashMap<u128, SessionState>>,
    /// Serialize mutations within one session while allowing unrelated
    /// sessions to decompress, decode, and persist concurrently.
    mutation_locks: DashMap<u128, Arc<Mutex<()>>>,
}

impl Receiver {
    pub fn new(store: ContentStore, compressor: Compressor) -> Self {
        let sessions = Mutex::new(HashMap::new());
        // Recover session state from a WAL-replayed store. `ContentStore::with_wal`
        // replays the log before the store is handed here, so every session the
        // log restored is already in the store's session map — seeded in manifest
        // order (via a SEED record) with content filled by CONTENT records. For
        // each, rebuild `SessionState`: `pending` = manifest blocks whose content
        // is still missing (a mid-cold-start crash leaves some content un-stored,
        // so the session stays cold-starting and a fresh RESYNC+BULK finishes it).
        // A fully recovered session (`pending` empty) is `acked` and ready for
        // APPENDs — no cold re-transfer. The ordered id list and root are not
        // duplicated here; the store's session log (manifest order) is the source
        // of truth. This is what keeps "cold resume paid ONCE" true across
        // receiver restarts.
        {
            let mut g = sessions.lock();
            for sid in store.session_list() {
                let ids = store.session_ids(sid);
                let pending: HashSet<BlockId> = ids
                    .iter()
                    .copied()
                    .filter(|id| !store.contains(id))
                    .collect();
                let acked = pending.is_empty();
                g.insert(
                    sid,
                    SessionState {
                        pending,
                        acked,
                        decoders: HashMap::new(),
                    },
                );
            }
        }
        Self {
            store,
            compressor,
            sessions,
            mutation_locks: DashMap::new(),
        }
    }

    fn mutation_lock(&self, session_id: u128) -> Arc<Mutex<()>> {
        self.mutation_locks
            .entry(session_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn store(&self) -> &ContentStore {
        &self.store
    }
    pub fn compressor(&self) -> &Compressor {
        &self.compressor
    }

    /// Handle an APPEND frame: decompress + store inline blocks, resolve refs,
    /// advance the session root, and return an ACK of the new root.
    ///
    /// If the frame's `base_root` does not match the receiver's current root,
    /// the receiver signals a divergence (cold start needed) by returning an
    /// error — the shim will then send RESYNC + BULK.
    pub fn handle_append(&self, frame: AppendFrame) -> Result<AckFrame, ReceiverError> {
        let mutation_lock = self.mutation_lock(frame.session_id);
        let _mutation_guard = mutation_lock.lock();
        // Divergence is checked against the authoritative `SessionState.root`
        // (manifest order), in the locked critical section below. An earlier
        // lock-free pre-check read `store.session_root()` (insertion /
        // generation-decode order) as a proxy, but that proxy diverges from the
        // authoritative root after an *out-of-order* cold start — BULK frames
        // arriving in generation-decode order leave the store's rolling root
        // != `client_root` even though `SessionState.root == client_root`. That
        // made the pre-check spuriously reject the first post-cold-start APPEND
        // ("cold start required" livelock). The store's session log is no longer
        // trusted as the divergence oracle; `SessionState.root` is. Divergent
        // APPENDs (rare — only on the cold-start transition) now pay the resolve
        // before being rejected by the in-lock check, an acceptable trade for
        // correctness.
        //
        // Pre-resolve every block *before* taking the sessions mutex: zstd
        // decompression and the per-block id hash are CPU-bound, and holding
        // the single sessions lock across them would serialize every session's
        // append behind one another's decompression. The lock is only needed
        // for the ordered id/root append + the store insert, which we do in a
        // single short critical section below. Appends still land in frame
        // order because the whole resolved batch is appended under one lock.
        enum Resolved {
            Inline { id: BlockId, block: Block },
            Ref(BlockId),
        }
        let mut resolved: Vec<Resolved> = Vec::with_capacity(frame.blocks.len());
        for fb in &frame.blocks {
            match fb {
                FrameBlock::Inline(wire_block) => {
                    // wire_block.payload is the *compressed canonical* bytes.
                    let comp = &wire_block.payload;
                    let canon = self
                        .compressor
                        .decompress(comp)
                        .map_err(|e| ReceiverError::Compress(e.to_string()))?;
                    // `from_canonical_owned` hashes + parses in one pass and
                    // keeps the payload as a zero-copy view of the decompressed
                    // buffer — no payload copy, no second full read.
                    let (block, id) = from_canonical_owned(canon)
                        .map_err(|e: &str| ReceiverError::Frame(e.to_string()))?;
                    resolved.push(Resolved::Inline { id, block });
                }
                FrameBlock::Ref(id) => {
                    resolved.push(Resolved::Ref(*id));
                }
            }
        }
        // Validate every reference before mutating the store. A Ref may point
        // at content already present at the receiver or at an Inline that
        // appeared earlier in this same frame. Rejecting up front prevents a
        // malformed tail Ref from leaving a partially-applied APPEND behind.
        let mut available_inline = HashSet::new();
        for item in &resolved {
            match item {
                Resolved::Inline { id, .. } => {
                    available_inline.insert(*id);
                }
                Resolved::Ref(id) if !self.store.contains(id) && !available_inline.contains(id) => {
                    return Err(ReceiverError::MissingRef);
                }
                Resolved::Ref(_) => {}
            }
        }

        let mut g = self.sessions.lock();
        let st = g.entry(frame.session_id).or_insert_with(|| SessionState {
            pending: HashSet::new(),
            acked: false,
            decoders: HashMap::new(),
        });
        // divergence check: if the client's base_root does not match our root,
        // we are out of sync. The authoritative root is the store's session log
        // (manifest order): ROOT_ZERO for a brand-new session, `client_root` (+
        // appends) once synced.
        if !st.pending.is_empty() {
            // Cold start in progress: the receiver has the manifest + target root
            // from RESYNC but has not yet recovered every block via BULK. Reject
            // APPENDs until completion (the completion ACK tells the shim when to
            // resume). This is the defensive guard the old lock-free pre-check
            // accidentally supplied; without it, a mid-cold-start APPEND would
            // be accepted onto a partial log.
            return Err(ReceiverError::ColdStartInProgress(frame.session_id));
        }
        let cur_root = self.store.session_root(frame.session_id);
        // Compute the root this frame claims it will produce. If it already
        // equals our current root, this is the common lost-ACK replay: return
        // the same ACK without appending the blocks twice. This makes APPEND
        // safe to retry after a response is lost.
        let target_root = resolved.iter().fold(frame.base_root, |root, item| {
            let id = match item {
                Resolved::Inline { id, .. } | Resolved::Ref(id) => id,
            };
            dlr_core::append_root(&root, id)
        });
        if frame.base_root != cur_root {
            if target_root == cur_root {
                return Ok(AckFrame {
                    session_id: frame.session_id,
                    root: cur_root,
                });
            }
            return Err(ReceiverError::BaseRootMismatch {
                session_id: frame.session_id,
                client: frame.base_root,
                current: cur_root,
            });
        }
        for r in resolved {
            match r {
                Resolved::Inline { id, block } => {
                    // insert_with_id reuses the id we already derived from the
                    // canonical bytes, skipping a second hash of the block, and
                    // appends `id` to the store's (manifest-ordered) session log
                    // + advances its root.
                    self.store.insert_with_id(frame.session_id, block, id);
                }
                Resolved::Ref(id) => {
                    self.store
                        .reference(frame.session_id, id)
                        .map_err(|_| ReceiverError::MissingRef)?;
                }
            }
        }
        Ok(AckFrame {
            session_id: frame.session_id,
            root: self.store.session_root(frame.session_id),
        })
    }

    /// Handle a RESYNC frame: compute the missing block set (the ones we don't
    /// have) and return the missing ids. On a cold gateway this is all of them.
    pub fn handle_resync(&self, frame: &ResyncFrame) -> Vec<BlockId> {
        let mutation_lock = self.mutation_lock(frame.session_id);
        let _mutation_guard = mutation_lock.lock();
        let mut missing = Vec::new();
        for id in &frame.manifest {
            if !self.store.contains(id) {
                missing.push(*id);
            }
        }
        // Seed the store's session log with the full manifest + client root in
        // manifest (authoritative) order. BULK then fills in block content via
        // `store_content_with_id` *without* appending to the log, so the log
        // and root stay correct regardless of generation arrival order — and a
        // SEED record is shadowed to the WAL so a restart replays this order.
        self.store
            .seed_session(frame.session_id, frame.manifest.clone(), frame.client_root);
        // Stash the cold-start bookkeeping the store can't recover on its own.
        // The manifest order + target root live in the store (seeded above); we
        // only track the pending set (the completion oracle) here.
        let mut g = self.sessions.lock();
        let st = g.entry(frame.session_id).or_insert_with(|| SessionState {
            pending: HashSet::new(),
            acked: false,
            decoders: HashMap::new(),
        });
        // The cold-start completion oracle: the set of manifest blocks we still
        // need to recover via BULK. Empty iff the receiver already has every
        // manifest block (warm-but-synced), in which case `handle_frame` ACKs
        // immediately so the shim advances `base_root` without a bulk transfer.
        st.pending = missing.iter().copied().collect();
        // Warm-but-synced (missing empty): the session is already complete, so
        // mark it ACKed — `handle_frame` ACKs the client root immediately. Cold
        // start (missing non-empty): ACK fires once, on completion.
        st.acked = missing.is_empty();
        missing
    }

    /// Handle a BULK frame: fountain-decode the generation and store recovered
    /// blocks. Once a generation decodes, the recovered *compressed canonical*
    /// bytes are decompressed and stored. Returns `Some(root)` when this frame
    /// completed the session's cold start (every manifest block is now
    /// recovered) — the caller should ACK `root` so the shim advances
    /// `base_root` and resumes steady state. Returns `None` otherwise (more
    /// BULK needed, or the session was already ACKed).
    pub fn handle_bulk(&self, frame: &BulkFrame) -> Result<Option<MerkleRoot>, ReceiverError> {
        let mutation_lock = self.mutation_lock(frame.session_id);
        let _mutation_guard = mutation_lock.lock();
        // Phase 1 (sessions lock held only briefly): fetch or create the
        // per-generation decoder, then clone its `Arc` and release the sessions
        // lock. The full fountain decode is CPU-bound; holding the single
        // sessions mutex across it (the old shape) serialized every session's
        // cold-start decode behind every other's and blocked APPEND handling.
        let dec = {
            let mut g = self.sessions.lock();
            let st = g.entry(frame.session_id).or_insert_with(|| SessionState {
                pending: HashSet::new(),
                acked: false,
                decoders: HashMap::new(),
            });
            st.decoders
                .entry(frame.generation)
                .or_insert_with(|| {
                    Arc::new(Mutex::new(FountainDecoder::new(
                        frame.k as usize,
                        frame.symbol_size as usize,
                    )))
                })
                .clone()
        };
        // Phase 2 (per-generation lock, NOT the sessions lock): add symbols and
        // attempt decode. Same-generation bulk frames serialize here, which is
        // correct (a generation's symbols must be assembled in order); other
        // generations and sessions proceed concurrently, and APPEND handling
        // is no longer blocked by an in-flight decode.
        let decode_result = {
            let mut d = dec.lock();
            for s in &frame.symbols {
                d.add(s)
                    .map_err(|e| ReceiverError::Fountain(e.to_string()))?;
            }
            d.decode()
        };
        match decode_result {
            Ok(symbols) => {
                // concatenate the generation's source symbols into the flat stream
                let total: usize = symbols.iter().map(|s| s.len()).sum();
                let mut flat = Vec::with_capacity(total);
                for s in &symbols {
                    flat.extend_from_slice(s);
                }
                // Pass 1 (serial): parse the variable-length [len:u32][comp]
                // framing into slice descriptors. The flat stream is a sequence
                // of compressed canonical blocks; each carries the compressor's
                // marker, so the boundary isn't knowable without the framing
                // length the shim prepended. Stop at the zero-length padding
                // marker (the shim zero-pads the stream to a whole symbol) or a
                // truncated tail.
                let mut frames: Vec<(usize, usize)> = Vec::new();
                let mut off = 0usize;
                while off + 4 <= flat.len() {
                    let blen = u32::from_le_bytes(flat[off..off + 4].try_into().unwrap()) as usize;
                    off += 4;
                    if blen == 0 {
                        break;
                    } // zero-pad tail of the coded stream
                    if off + blen > flat.len() {
                        break;
                    } // truncated tail
                    frames.push((off, blen));
                    off += blen;
                }
                // Pass 2 (parallel): decompress + parse each recovered block
                // concurrently. zstd decompression is the CPU-heavy part of
                // cold-start recovery; the store is DashMap-sharded and the
                // compressor's zstd contexts are thread-local, so this is safe
                // under rayon. Recovered (block, id) pairs land in frame order.
                let session_id = frame.session_id;
                let recovered: Result<Vec<(Block, BlockId)>, ReceiverError> = frames
                    .par_iter()
                    .map(|&(off, blen)| {
                        let comp = &flat[off..off + blen];
                        let canon = self
                            .compressor
                            .decompress(comp)
                            .map_err(|e| ReceiverError::Compress(e.to_string()))?;
                        from_canonical_owned(canon)
                            .map_err(|e: &str| ReceiverError::Frame(e.to_string()))
                    })
                    .collect();
                let recovered = recovered?;
                // The ids we retire from the pending set; collected before the
                // store loop consumes `recovered` by value.
                let recovered_ids: Vec<BlockId> = recovered.iter().map(|(_, id)| *id).collect();
                // Pass 3 (serial): store each block's *content* only. The
                // session's authoritative order is `SessionState.ids` (the
                // manifest seeded at RESYNC), and the store's session log was
                // seeded in that same order — so content is stored WITHOUT
                // appending to the log. Arrival / decode order here does not
                // affect the ACK root or `reconstruct`, which is what makes
                // out-of-order BULK delivery (multipath / RDMA / multicast)
                // correct — and, because the log was seeded in manifest order,
                // a WAL replay rebuilds the same root regardless of arrival
                // order. `st.root` was fixed to `client_root` at RESYNC and is
                // not recomputed from insertion order.
                for (block, id) in recovered {
                    self.store.store_content_with_id(session_id, block, id);
                }
                // Cold-start completion: drop this generation's decoder and retire
                // its recovered ids from the pending set. When the pending set
                // empties for the first time, the cold start is complete — ACK the
                // session root so the shim advances `base_root` and resumes steady
                // state. `acked` guards against re-ACK on late/duplicate BULK.
                let mut completion: Option<MerkleRoot> = None;
                {
                    let mut g = self.sessions.lock();
                    if let Some(st) = g.get_mut(&frame.session_id) {
                        st.decoders.remove(&frame.generation);
                        for id in &recovered_ids {
                            st.pending.remove(id);
                        }
                        if st.pending.is_empty() && !st.acked {
                            st.acked = true;
                            // The store's session root is `client_root` (seeded at
                            // RESYNC, content-only BULK did not change it, and no
                            // APPEND can have landed — pending was non-empty until
                            // now). ACK it so the shim advances `base_root`.
                            completion = Some(self.store.session_root(frame.session_id));
                        }
                    }
                }
                Ok(completion)
            }
            Err(FountainError::Underdetermined { .. }) => Ok(None),
            Err(e) => Err(ReceiverError::Fountain(e.to_string())),
        }
    }

    /// Handle any frame, returning an optional frame to send back (ACK).
    pub fn handle_frame(&self, f: Frame) -> Result<Option<Frame>, ReceiverError> {
        match f {
            Frame::Append(a) => {
                let ack = self.handle_append(a)?;
                Ok(Some(Frame::Ack(ack)))
            }
            Frame::Resync(r) => {
                let missing = self.handle_resync(&r);
                if missing.is_empty() {
                    // Warm-but-synced: the receiver already has every manifest
                    // block. No BULK transfer is needed — ACK the client root so
                    // the shim advances `base_root` and continues steady state.
                    // The store was seeded with `client_root` above, so its
                    // session root is the value to ACK.
                    let root = self.store.session_root(r.session_id);
                    Ok(Some(Frame::Ack(AckFrame {
                        session_id: r.session_id,
                        root,
                    })))
                } else {
                    // Close the §3.3 handshake: name the missing set back to the
                    // sender so it codes *only* what we lack (sparse
                    // reconstruction).
                    Ok(Some(Frame::Missing(MissingFrame {
                        session_id: r.session_id,
                        missing,
                    })))
                }
            }
            Frame::Bulk(b) => {
                // A BULK frame that completes the cold start returns the session
                // root to ACK; the shim applies it to advance `base_root` and
                // resume steady state. Incomplete / duplicate frames return None.
                let sid = b.session_id;
                match self.handle_bulk(&b)? {
                    Some(root) => Ok(Some(Frame::Ack(AckFrame {
                        session_id: sid,
                        root,
                    }))),
                    None => Ok(None),
                }
            }
            Frame::Ack(_) => Ok(None),
            Frame::Missing(_) => Ok(None),
        }
    }

    /// Return a pointer to an assembled session log (for the runtime / prune).
    pub fn pointer(&self, session_id: u128) -> Option<LogPointer> {
        let g = self.sessions.lock();
        if g.contains_key(&session_id) {
            Some(LogPointer {
                session_id,
                root: self.store.session_root(session_id),
                len: self.store.session_len(session_id),
            })
        } else {
            None
        }
    }

    /// The authoritative session root (manifest order), or `None` if the
    /// receiver has no state for `session_id`. This is the root the receiver
    /// ACKs and the value the shim's `base_root` must match to append. The
    /// store's session log is seeded in manifest order at RESYNC and extended
    /// in arrival order by APPENDs, so `store.session_root` *is* the
    /// authoritative root (no separate receiver-maintained copy).
    pub fn session_root(&self, session_id: u128) -> Option<MerkleRoot> {
        let g = self.sessions.lock();
        g.contains_key(&session_id)
            .then(|| self.store.session_root(session_id))
    }

    /// Reconstruct the full ordered session log (for the prune / distillation
    /// sink). Cheap: clones refcounted `Bytes`.
    ///
    /// Delegates to the store, whose session log is the authoritative manifest
    /// order (seeded at RESYNC, extended by APPEND arrival order, content-only
    /// BULK never reorders it). Blocks not yet stored (a partial cold start) are
    /// dropped by the store, yielding the correct partial prefix.
    pub fn reconstruct(&self, session_id: u128) -> Vec<Block> {
        let g = self.sessions.lock();
        if !g.contains_key(&session_id) {
            return Vec::new();
        }
        drop(g);
        self.store.reconstruct(session_id)
    }

    /// Reconstruct a range (e.g. the last N blocks) for a candidate prune window.
    ///
    /// Same manifest order as `reconstruct` (the store's seeded session log).
    /// Blocks not yet stored (a partial cold start) are dropped.
    pub fn reconstruct_range(&self, session_id: u128, from: usize, to: usize) -> Vec<Block> {
        let g = self.sessions.lock();
        if !g.contains_key(&session_id) {
            return Vec::new();
        }
        drop(g);
        self.store.reconstruct_range(session_id, from, to)
    }
}
