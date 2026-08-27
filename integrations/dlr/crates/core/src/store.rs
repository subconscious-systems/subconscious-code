//! Content-addressed store (DESIGN §2, §3.3).
//!
//! The receiver accumulates the per-session log in a content-addressed store and
//! hands the runtime a *pointer*, never a rebuild. Dedup is over opaque
//! bytes/hashes and reveals nothing about retention policy.
//!
//! The store maps `BlockId -> Block` (payload) and, per session, an ordered log
//! of block ids. It is the substrate for "kill the re-send": an APPEND may
//! reference an already-known block by id (`FrameBlock::Ref`) and the receiver
//! resolves it locally without re-reading the wire.
//!
//! Concurrency: the block map and the per-session logs are sharded
//! `DashMap`s so inserts/lookups from *different* sessions do not contend on a
//! single lock (the old design held one `RwLock` over the whole structure and
//! serialized every session's append). Stats are plain atomics, so the hot
//! path takes no lock at all for accounting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::{mapref::entry::Entry, DashMap};
use parking_lot::Mutex;

use crate::block::{Block, BlockId};
use crate::canonical::canonical_bytes;
use crate::merkle::{append_root, MerkleRoot, ROOT_ZERO};
use crate::wal::Wal;

/// Statistics for observability and the performance model.
#[derive(Debug, Default, Clone)]
pub struct StoreStats {
    pub blocks_stored: u64,
    pub bytes_stored: u64,
    pub blocks_deduped: u64, // blocks referenced but already present
    pub sessions: u64,
}

/// Lock-free counters backing [`StoreStats`] so accounting never touches a map
/// shard lock.
struct StatsAtomic {
    blocks_stored: AtomicU64,
    bytes_stored: AtomicU64,
    blocks_deduped: AtomicU64,
    sessions: AtomicU64,
}

impl StatsAtomic {
    fn snapshot(&self) -> StoreStats {
        StoreStats {
            blocks_stored: self.blocks_stored.load(Ordering::Relaxed),
            bytes_stored: self.bytes_stored.load(Ordering::Relaxed),
            blocks_deduped: self.blocks_deduped.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
        }
    }
}

/// A content-addressed block store, multi-session. Cheap to clone (Arc-shared).
#[derive(Clone)]
pub struct ContentStore {
    /// block_id -> Block payload. Sharded so cross-session dedup/insert is
    /// concurrent; same-key access is naturally serialized by the shard.
    blocks: Arc<DashMap<BlockId, Block>>,
    /// session_id -> ordered log of block ids + running root.
    sessions: Arc<DashMap<u128, SessionLog>>,
    /// Lock-free accounting.
    stats: Arc<StatsAtomic>,
    /// Optional durable append-only log, set once at construction. When set,
    /// every insert/reference is shadowed to disk so a restart replays the log
    /// instead of forcing a full cold-start re-transfer (DESIGN §3.3 / §4 —
    /// keep "cold resume paid ONCE" true across receiver restarts). Unset =
    /// in-memory only. `OnceLock` is set once via `&self` (at setup, before the
    /// store is shared across threads) and read cheaply on the hot path.
    wal: std::sync::OnceLock<Option<Arc<Wal>>>,
    /// First asynchronous WAL append failure. Store mutation APIs intentionally
    /// remain allocation-friendly and infallible, so an error is latched here
    /// and surfaced by `flush_wal` before a caller exposes an ACK.
    wal_error: Arc<Mutex<Option<(std::io::ErrorKind, String)>>>,
}

struct SessionLog {
    pub ids: Vec<BlockId>,
    pub root: MerkleRoot,
}

impl Default for ContentStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentStore {
    pub fn new() -> Self {
        Self {
            blocks: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            stats: Arc::new(StatsAtomic {
                blocks_stored: AtomicU64::new(0),
                bytes_stored: AtomicU64::new(0),
                blocks_deduped: AtomicU64::new(0),
                sessions: AtomicU64::new(0),
            }),
            wal: std::sync::OnceLock::new(),
            wal_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Open a durable store backed by an append-only WAL at `path`. Any
    /// existing log is replayed into the in-memory index before the handle is
    /// returned, so a restarted receiver recovers its session logs and Merkle
    /// roots without a cold-start re-transfer. The WAL is flushed manually via
    /// [`ContentStore::flush_wal`]; the hot path only appends to a buffer.
    pub fn with_wal<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<Self> {
        let store = Self::new();
        let wal = Wal::open(path, &store)?;
        // set_wal is sound here: the store is not yet shared across threads.
        store.set_wal(wal);
        Ok(store)
    }

    /// Attach a WAL to an existing store (e.g. after building it in-memory).
    /// Must be called before the store is shared across threads; `OnceLock`
    /// enforces the single-set property.
    pub fn set_wal(&self, wal: Wal) {
        let _ = self.wal.set(Some(Arc::new(wal)));
    }

    /// The WAL handle, if one was attached. Cheap on the hot path: an atomic
    /// load when unset, no lock contention with the store's maps.
    fn wal(&self) -> Option<Arc<Wal>> {
        self.wal.get().and_then(|opt| opt.as_ref().cloned())
    }

    /// Flush the WAL buffer. With `sync=true`, fsync so records survive a crash.
    /// No-op if no WAL is attached.
    pub fn flush_wal(&self, sync: bool) -> std::io::Result<()> {
        if let Some((kind, message)) = self.wal_error.lock().as_ref() {
            return Err(std::io::Error::new(*kind, message.clone()));
        }
        let result = match self.wal() {
            Some(w) => w.flush(sync),
            None => Ok(()),
        };
        if let Err(error) = result {
            self.latch_wal_error(&error);
            return Err(error);
        }
        Ok(())
    }

    fn record_wal_result(&self, result: std::io::Result<()>) {
        if let Err(error) = result {
            self.latch_wal_error(&error);
        }
    }

    fn latch_wal_error(&self, error: &std::io::Error) {
        let mut slot = self.wal_error.lock();
        if slot.is_none() {
            *slot = Some((error.kind(), error.to_string()));
        }
    }

    /// Insert a block by content address. Returns `true` if newly stored, `false`
    /// if it was already present (dedup). The session log is appended and the
    /// session root updated incrementally.
    pub fn insert(&self, session_id: u128, block: Block) -> bool {
        let id = block.block_id();
        self.insert_with_id(session_id, block, id)
    }

    /// Insert a block whose `block_id` the caller already computed. The shim
    /// hashes the canonical bytes once (for compression) and derives the id
    /// from the same buffer; this entry point avoids re-streaming the canonical
    /// bytes for the id on the hot path — the store trusts the caller's id.
    pub fn insert_with_id(&self, session_id: u128, block: Block, id: BlockId) -> bool {
        self.insert_with_canonical(session_id, block, id, None)
    }

    /// Insert a block whose `block_id` — and, optionally, whose already-
    /// materialized **canonical bytes** — the caller has computed. The shim
    /// builds the canonical form anyway (for compression), so handing it
    /// through here means a durable insert into the WAL logs the caller's
    /// buffer instead of re-deriving `canonical_bytes` (a full payload copy +
    /// header rebuild) a second time on the hot path. `canonical`, when `Some`,
    /// must equal `canonical_bytes(&block)`.
    pub fn insert_with_canonical(
        &self,
        session_id: u128,
        block: Block,
        id: BlockId,
        canonical: Option<&[u8]>,
    ) -> bool {
        let wal = self.wal();
        // Clone the block (cheap — `Bytes` is refcounted) only when a WAL is
        // attached, so the no-WAL fast path touches no extra allocation. The
        // canonical bytes are materialized below *only* on the newly-stored
        // path, so a dedup hit never pays for a payload-sized canonical alloc.
        let block_for_wal = if wal.is_some() {
            Some(block.clone())
        } else {
            None
        };
        let payload_len = block.payload.len() as u64;

        // One hash lookup via the entry API (was `contains_key` + `insert`).
        let newly = match self.blocks.entry(id) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(block);
                true
            }
        };
        if newly {
            self.stats.blocks_stored.fetch_add(1, Ordering::Relaxed);
            self.stats
                .bytes_stored
                .fetch_add(payload_len, Ordering::Relaxed);
        } else {
            self.stats.blocks_deduped.fetch_add(1, Ordering::Relaxed);
        }

        // Single session-map lookup (was `contains_key` + `entry`): the vacant
        // arm both creates the session and appends; the occupied arm mutates.
        let was_new_session = match self.sessions.entry(session_id) {
            Entry::Occupied(mut o) => {
                let log = o.get_mut();
                let nr = append_root(&log.root, &id);
                log.ids.push(id);
                log.root = nr;
                false
            }
            Entry::Vacant(v) => {
                v.insert(SessionLog {
                    ids: vec![id],
                    root: append_root(&ROOT_ZERO, &id),
                });
                true
            }
        };
        if was_new_session {
            self.stats.sessions.fetch_add(1, Ordering::Relaxed);
        }

        // Shadow to the WAL outside the store maps: a buffered sequential
        // write, no fsync (deferred to `flush_wal`). Log the full canonical
        // bytes only when newly stored; a dedup writes just the 32-byte id.
        // During `Wal::open` replay this `wal()` is still unset, so replay
        // does not re-append (no feedback loop).
        if let Some(wal) = wal {
            if newly {
                if let Some(b) = block_for_wal {
                    match canonical {
                        Some(c) => {
                            self.record_wal_result(wal.append_insert(session_id, c));
                        }
                        None => {
                            let c = canonical_bytes(&b);
                            self.record_wal_result(wal.append_insert(session_id, &c));
                        }
                    }
                }
            } else {
                self.record_wal_result(wal.append_reference(session_id, id));
            }
        }
        newly
    }

    /// Reference a block already in the store (dedup path: `FrameBlock::Ref`).
    /// Appends to the session log and updates the root without re-storing.
    pub fn reference(&self, session_id: u128, id: BlockId) -> Result<(), &'static str> {
        if !self.blocks.contains_key(&id) {
            return Err("referenced block not present in store");
        }
        match self.sessions.entry(session_id) {
            Entry::Occupied(mut o) => {
                let log = o.get_mut();
                let nr = append_root(&log.root, &id);
                log.ids.push(id);
                log.root = nr;
            }
            Entry::Vacant(v) => {
                v.insert(SessionLog {
                    ids: vec![id],
                    root: append_root(&ROOT_ZERO, &id),
                });
            }
        }
        self.stats.blocks_deduped.fetch_add(1, Ordering::Relaxed);
        let wal = self.wal();
        if let Some(wal) = wal {
            self.record_wal_result(wal.append_reference(session_id, id));
        }
        Ok(())
    }

    /// Seed a session log with the full manifest + target root at RESYNC, before
    /// any block content has arrived via BULK. The store's session log is the
    /// *authoritative* order (manifest order) from the outset; BULK then fills in
    /// block content via [`ContentStore::store_content_with_id`] *without* appending to the
    /// log, so out-of-order generation arrival leaves the log and root correct.
    /// Durably logs a SEED record so a restart replays the manifest order (not
    /// arrival order) and "cold resume paid ONCE" survives a receiver restart.
    /// Idempotent in shape: re-seeding overwrites the log (used by replay).
    pub fn seed_session(&self, session_id: u128, ids: Vec<BlockId>, root: MerkleRoot) {
        let wal = self.wal();
        // Clone for the WAL record before `ids` is moved into the session log.
        let wal_ids = if wal.is_some() {
            Some(ids.clone())
        } else {
            None
        };
        let was_new = match self.sessions.entry(session_id) {
            Entry::Occupied(mut o) => {
                let log = o.get_mut();
                log.ids = ids;
                log.root = root;
                false
            }
            Entry::Vacant(v) => {
                v.insert(SessionLog { ids, root });
                true
            }
        };
        if was_new {
            self.stats.sessions.fetch_add(1, Ordering::Relaxed);
        }
        if let (Some(wal), Some(ids)) = (wal, wal_ids) {
            self.record_wal_result(wal.append_seed(session_id, &ids, &root));
        }
    }

    /// Store cold-start block *content* (from BULK) without appending to the
    /// session log — the log was seeded at RESYNC by [`ContentStore::seed_session`]. This is
    /// the content-only counterpart of [`ContentStore::insert_with_id`]: it populates the
    /// content map (and dedup stats) and shadows a CONTENT record to the WAL,
    /// but does not touch the session log or root. Arrival/decode order is
    /// therefore irrelevant to the durable log. Returns `true` if newly stored.
    pub fn store_content_with_id(&self, session_id: u128, block: Block, id: BlockId) -> bool {
        let wal = self.wal();
        let block_for_wal = if wal.is_some() {
            Some(block.clone())
        } else {
            None
        };
        let payload_len = block.payload.len() as u64;
        let newly = match self.blocks.entry(id) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(block);
                true
            }
        };
        if newly {
            self.stats.blocks_stored.fetch_add(1, Ordering::Relaxed);
            self.stats
                .bytes_stored
                .fetch_add(payload_len, Ordering::Relaxed);
        } else {
            self.stats.blocks_deduped.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(wal) = wal {
            if newly {
                if let Some(b) = block_for_wal {
                    // Re-derive canonical only on the WAL path (no-WAL fast path
                    // touches no payload-sized alloc); same trade as `insert_with_id`.
                    let c = canonical_bytes(&b);
                    self.record_wal_result(wal.append_content(session_id, &c));
                }
            }
            // dedup: content is already durably present, log nothing.
        }
        let _ = session_id; // session log untouched; kept for API symmetry / WAL key
        newly
    }

    /// Convenience: store content, deriving the id from the block (replay path).
    pub fn store_content(&self, session_id: u128, block: Block) -> bool {
        let id = block.block_id();
        self.store_content_with_id(session_id, block, id)
    }

    pub fn get(&self, id: &BlockId) -> Option<Block> {
        self.blocks.get(id).map(|r| (*r).clone())
    }

    pub fn contains(&self, id: &BlockId) -> bool {
        self.blocks.contains_key(id)
    }

    pub fn session_root(&self, session_id: u128) -> MerkleRoot {
        self.sessions
            .get(&session_id)
            .map(|l| l.root)
            .unwrap_or(ROOT_ZERO)
    }

    pub fn session_len(&self, session_id: u128) -> usize {
        self.sessions
            .get(&session_id)
            .map(|l| l.ids.len())
            .unwrap_or(0)
    }

    /// Ordered block ids for a session (cold-start manifest material).
    pub fn session_ids(&self, session_id: u128) -> Vec<BlockId> {
        self.sessions
            .get(&session_id)
            .map(|l| l.ids.clone())
            .unwrap_or_default()
    }

    /// All known session ids. Used by the receiver to rebuild `SessionState`
    /// for every session a WAL replay restored into the store.
    pub fn session_list(&self) -> Vec<u128> {
        self.sessions.iter().map(|r| *r.key()).collect()
    }

    pub fn stats(&self) -> StoreStats {
        self.stats.snapshot()
    }

    /// Reconstruct the ordered block payloads for a session (for the prune /
    /// distillation sink). Clones `Bytes` (cheap, refcounted).
    pub fn reconstruct(&self, session_id: u128) -> Vec<Block> {
        let log = match self.sessions.get(&session_id) {
            Some(l) => l,
            None => return Vec::new(),
        };
        let ids: Vec<BlockId> = log.ids.clone();
        drop(log);
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(b) = self.blocks.get(id) {
                out.push((*b).clone());
            }
        }
        out
    }

    /// Reconstruct a *range* of the session log (e.g. the last N blocks) without
    /// materializing the whole history — used by the prune to build a candidate
    /// window directly.
    pub fn reconstruct_range(&self, session_id: u128, from: usize, to: usize) -> Vec<Block> {
        let log = match self.sessions.get(&session_id) {
            Some(l) => l,
            None => return Vec::new(),
        };
        let from = from.min(log.ids.len());
        let to = to.min(log.ids.len());
        let ids: Vec<BlockId> = log.ids[from..to].to_vec();
        drop(log);
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(b) = self.blocks.get(id) {
                out.push((*b).clone());
            }
        }
        out
    }
}

// Silence unused Bytes import (kept for API symmetry / future zero-copy block
// payloads stored as Bytes directly).
#[allow(unused_imports)]
use bytes::Bytes as _;
