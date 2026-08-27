//! Runtime prune (DESIGN §4, §8).
//!
//! The heavy, accurate prune is **decoupled from transport by construction**.
//! Transport + reconstruction are cheap and continuous; the receiver hands the
//! runtime a stable pointer to the assembled log; the prune runs on its own
//! compute pool, pipelined, producing ~100k windows asynchronously. A slow
//! prune backpressures *inference scheduling*, never the *wire*.
//!
//! Security (§8): the prune never touches a wire. It runs post-reconstruction in
//! the runtime binary. No client, no shim frame, no coded packet encodes *which*
//! blocks survive. Observing the entire wire reveals content and nothing about
//! retention.
//!
//! This module provides:
//!   - `PrunePolicy`: a pluggable retention strategy trait (the *secret*
//!     algorithm lives behind this). A default importance-scored policy is
//!     included; a real deployment swaps in its own compiled implementation.
//!   - `PruneScheduler`: runs prune jobs on a rayon thread pool, off the transfer
//!     hot path, producing windows asynchronously with priority + work-stealing.

use std::sync::Arc;

pub mod incremental;
pub use incremental::{Budget, IncrementalPruner};

use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::HashMap;

use dlr_core::{Block, BlockId, BlockKind};
use dlr_receiver::Receiver;

/// A pruned window: the ~100k-token slice handed to the inference backend.
#[derive(Debug, Clone)]
pub struct PruneWindow {
    pub session_id: u128,
    pub blocks: Vec<Block>,
    pub approx_tokens: usize,
}

/// Retention policy — the secret sauce. Implementations decide which blocks of
/// an assembled log survive into a ~100k-token window. The trait is the only
/// seam; concrete scoring lives in the compiled binary and is never serialized.
pub trait PrunePolicy: Send + Sync {
    /// Produce a window of at most `budget_tokens` from the ordered log.
    fn prune(&self, session_id: u128, log: &[Block], budget_tokens: usize) -> PruneWindow;
}

/// Default policy: importance-scored retention.
///
/// Scores every block with cheap, content-agnostic heuristics and keeps the
/// top-scoring blocks within a token budget, preserving original order. This is
/// *not* the "accurate, computationally heavy" prune — it is a fast baseline.
/// The design's point is that the *real* (slow) prune runs here, off the wire;
/// this default stands in so the system is end-to-end functional. Swap it for
/// your own `PrunePolicy` in production.
pub struct ImportancePolicy {
    /// Weight for recent blocks (recency decay exponent).
    pub recency_decay: f32,
    /// Weight for tool results vs messages.
    pub tool_result_weight: f32,
    /// Weight for system/summary blocks (always retained).
    pub system_weight: f32,
}

impl Default for ImportancePolicy {
    fn default() -> Self {
        Self {
            recency_decay: 1.5,
            tool_result_weight: 1.3,
            system_weight: 2.0,
        }
    }
}

fn approx_tokens(b: &Block) -> usize {
    // ~4 chars per token, lower bound 1
    (b.payload.len() / 4).max(1)
}

impl PrunePolicy for ImportancePolicy {
    fn prune(&self, session_id: u128, log: &[Block], budget_tokens: usize) -> PruneWindow {
        let n = log.len();
        if n == 0 {
            return PruneWindow {
                session_id,
                blocks: Vec::new(),
                approx_tokens: 0,
            };
        }
        // Score each block. System/summary blocks always survive (weight inf).
        let mut scored: Vec<(usize, f32)> = log
            .par_iter()
            .enumerate()
            .map(|(i, b)| {
                let base = match b.kind {
                    BlockKind::System | BlockKind::Summary => f32::INFINITY,
                    BlockKind::ToolResult => self.tool_result_weight,
                    _ => 1.0,
                };
                let recency = ((n - i) as f32 / n.max(1) as f32).powf(self.recency_decay);
                let length_factor = 1.0 + (b.payload.len() as f32 / 4096.0).min(2.0);
                (i, base * recency * length_factor)
            })
            .collect();

        // Sort by score descending; for infinity (system) they all tie -> keep
        // earliest first by index to preserve order. Unstable sort is safe here
        // — items are unique by index `a.0` — and faster (no aux allocation).
        scored.sort_unstable_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        // Greedily fill the budget, then re-sort by index to preserve order.
        let mut kept: Vec<usize> = Vec::new();
        let mut tokens = 0usize;
        for (idx, _score) in &scored {
            let t = approx_tokens(&log[*idx]);
            if tokens + t > budget_tokens && !kept.is_empty() {
                // system blocks always included even if over budget: handled by
                // the inf score landing first; for non-system, stop when full.
                if log[*idx].kind != BlockKind::System && log[*idx].kind != BlockKind::Summary {
                    continue;
                }
            }
            tokens += t;
            kept.push(*idx);
            if tokens >= budget_tokens {
                break;
            }
        }
        kept.sort_unstable();
        let blocks: Vec<Block> = kept.iter().map(|&i| log[i].clone()).collect();
        PruneWindow {
            session_id,
            blocks,
            approx_tokens: tokens,
        }
    }
}

/// A scheduled prune job.
struct PruneJob {
    session_id: u128,
    budget_tokens: usize,
    // The log is *not* snapshotted at schedule time. `schedule` runs on the
    // transport hot path and must return immediately; reconstructing the log
    // is O(N) and belongs off the wire. Instead the log is reconstructed at
    // run time (a consistent snapshot — `reconstruct` returns an owned `Vec`),
    // which also drops the full-`Vec<Block>`-per-job memory cost. (#32)
}

/// Async prune scheduler running on a rayon pool, off the transfer hot path.
/// Jobs are FIFO with a priority boost for the most-recently-scheduled session
/// (so the live turn's window lands first).
pub struct PruneScheduler {
    policy: Arc<dyn PrunePolicy>,
    receiver: Arc<Receiver>,
    /// Pending jobs. A real deployment would use a priority queue + a background
    /// worker; here we expose a synchronous `run_one` plus a `run_all` that
    /// fans out across the rayon pool, which is the off-hot-path contract.
    pending: Mutex<Vec<PruneJob>>,
}

impl PruneScheduler {
    pub fn new(receiver: Arc<Receiver>, policy: Arc<dyn PrunePolicy>) -> Self {
        Self {
            policy,
            receiver,
            pending: Mutex::new(Vec::new()),
        }
    }

    pub fn with_default(receiver: Arc<Receiver>) -> Self {
        Self::new(receiver, Arc::new(ImportancePolicy::default()))
    }

    /// Schedule a prune for `session_id` at `budget_tokens`. This is O(1) and
    /// safe to call on the transport hot path: it only records the intent
    /// (session id + budget). The log is reconstructed at run time, off the
    /// wire, so ongoing appends between schedule and run simply yield a more
    /// current window (still a consistent snapshot — `reconstruct` is atomic).
    pub fn schedule(&self, session_id: u128, budget_tokens: usize) {
        self.pending.lock().push(PruneJob {
            session_id,
            budget_tokens,
        });
    }

    /// Run one pending job (synchronously). Returns the window if one ran.
    pub fn run_one(&self) -> Option<PruneWindow> {
        let job = self.pending.lock().pop()?;
        let log = self.receiver.reconstruct(job.session_id);
        Some(self.policy.prune(job.session_id, &log, job.budget_tokens))
    }

    /// Run all pending jobs in parallel across the rayon pool. This is the
    /// "prune runs on its own compute pool" contract: the wire never waits.
    /// Each job reconstructs its own log snapshot here (off the hot path).
    pub fn run_all(&self) -> Vec<PruneWindow> {
        let jobs: Vec<PruneJob> = self.pending.lock().drain(..).collect();
        let policy = self.policy.clone();
        let receiver = self.receiver.clone();
        jobs.into_par_iter()
            .map(|j| {
                let log = receiver.reconstruct(j.session_id);
                policy.prune(j.session_id, &log, j.budget_tokens)
            })
            .collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().len()
    }
}

/// A priority prune scheduler (extra strategy): a binary heap orders jobs so
/// the most recently-scheduled (live) session's window lands first, and older
/// sessions degrade gracefully. Same off-hot-path contract as `PruneScheduler`
/// — the wire never waits — but under backlog the live turn wins.
pub struct PriorityPruneScheduler {
    policy: Arc<dyn PrunePolicy>,
    receiver: Arc<Receiver>,
    pending: Mutex<std::collections::BinaryHeap<PriorityJob>>,
    seq: Mutex<u64>,
}

struct PriorityJob {
    seq: u64, // higher = scheduled later = higher priority
    session_id: u128,
    budget_tokens: usize,
    // No `log` snapshot: reconstructed at run time (see `PruneJob`). (#32)
}

impl PartialEq for PriorityJob {
    fn eq(&self, o: &Self) -> bool {
        self.seq == o.seq
    }
}
impl Eq for PriorityJob {}
impl PartialOrd for PriorityJob {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for PriorityJob {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; we want highest seq first.
        self.seq.cmp(&o.seq)
    }
}

impl PriorityPruneScheduler {
    pub fn new(receiver: Arc<Receiver>, policy: Arc<dyn PrunePolicy>) -> Self {
        Self {
            policy,
            receiver,
            pending: Mutex::new(std::collections::BinaryHeap::new()),
            seq: Mutex::new(0),
        }
    }
    pub fn with_default(receiver: Arc<Receiver>) -> Self {
        Self::new(receiver, Arc::new(ImportancePolicy::default()))
    }

    pub fn schedule(&self, session_id: u128, budget_tokens: usize) {
        // O(1) on the hot path: record intent only, reconstruct at run time.
        let s = {
            let mut g = self.seq.lock();
            *g += 1;
            *g
        };
        self.pending.lock().push(PriorityJob {
            seq: s,
            session_id,
            budget_tokens,
        });
    }

    pub fn run_all(&self) -> Vec<PruneWindow> {
        let jobs: Vec<PriorityJob> = {
            let mut g = self.pending.lock();
            let mut v = Vec::new();
            while let Some(j) = g.pop() {
                v.push(j);
            }
            v
        };
        let policy = self.policy.clone();
        let receiver = self.receiver.clone();
        jobs.into_par_iter()
            .map(|j| {
                let log = receiver.reconstruct(j.session_id);
                policy.prune(j.session_id, &log, j.budget_tokens)
            })
            .collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().len()
    }
}

/// Incremental prune scheduler: maintains a token-budgeted top-K *per session*,
/// fed block-by-block as blocks arrive, so producing a window is O(K log K) +
/// O(K) store fetches — **independent of the log length N**. This is the doc's
/// "prune cost independent of N" win: the re-scan `ImportancePolicy::prune` (and
/// the snapshotting `PruneScheduler`/`PriorityPruneScheduler`) are O(N) in the
/// growing history every turn; this is O(log K) per new block and O(K) per
/// window.
///
/// Cold start is `ingest_session` (O(N log K) once); steady state is
/// `ingest_block` per APPEND (O(log K)) and `window` on demand (O(K)). The
/// window is materialized from the content store by id, in original log order.
///
/// The score function is the swappable policy hook (`IncrementalPruner::new_with`);
/// the default is recency×size. The real (secret) prune swaps in its own scorer
/// without touching the maintenance code.
pub struct IncrementalPruneScheduler {
    receiver: Arc<Receiver>,
    token_budget: usize,
    pruners: Mutex<HashMap<u128, IncrementalPruner>>,
}

impl IncrementalPruneScheduler {
    pub fn new(receiver: Arc<Receiver>, token_budget: usize) -> Self {
        Self {
            receiver,
            token_budget,
            pruners: Mutex::new(HashMap::new()),
        }
    }

    /// Cold-ingest an already-assembled session: insert every stored block by
    /// id + size. O(N log K) once; afterwards feed new blocks via `ingest_block`.
    pub fn ingest_session(&self, session_id: u128) {
        let store = self.receiver.store();
        let ids = store.session_ids(session_id);
        let mut p = IncrementalPruner::new_token_budget(self.token_budget);
        for id in &ids {
            let size = store.get(id).map(|b| b.payload.len()).unwrap_or(0);
            p.insert(*id, size);
        }
        self.pruners.lock().insert(session_id, p);
    }

    /// Ingest a newly-arrived block (call from the append path). O(log K).
    pub fn ingest_block(&self, session_id: u128, id: BlockId, size: usize) {
        let mut g = self.pruners.lock();
        let p = g
            .entry(session_id)
            .or_insert_with(|| IncrementalPruner::new_token_budget(self.token_budget));
        p.insert(id, size);
    }

    /// Produce the pruned window for a session: survivors in log order,
    /// materialized from the content store. O(K log K) + O(K) store fetches.
    /// Returns `None` if the session has never been ingested.
    pub fn window(&self, session_id: u128) -> Option<PruneWindow> {
        let ids = {
            let g = self.pruners.lock();
            g.get(&session_id)?.window_ordered()
        };
        let store = self.receiver.store();
        let blocks: Vec<Block> = ids.iter().filter_map(|id| store.get(id)).collect();
        let approx_tokens = blocks.iter().map(approx_tokens).sum();
        Some(PruneWindow {
            session_id,
            blocks,
            approx_tokens,
        })
    }

    /// Number of sessions currently tracked.
    pub fn session_count(&self) -> usize {
        self.pruners.lock().len()
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use dlr_compress::Compressor;
    use dlr_core::{Block, BlockKind, ContentStore};

    #[test]
    fn incremental_scheduler_window_in_log_order_and_under_budget() {
        let receiver = Arc::new(Receiver::new(ContentStore::new(), Compressor::default()));
        let session: u128 = 0x7072_756E;
        // store 1000 blocks of varying (small) size
        for i in 0..1000u64 {
            let payload = vec![0u8; ((i % 10) as usize + 1) * 10];
            let b = Block::new(BlockKind::Message, i, payload);
            receiver.store().insert(session, b);
        }
        // 1000-byte token (size) budget -> keeps a small top-K, independent of N=1000.
        let sched = IncrementalPruneScheduler::new(receiver.clone(), 1000);
        sched.ingest_session(session);
        let w = sched
            .window(session)
            .expect("ingested session has a window");
        assert!(!w.blocks.is_empty());
        // survivors are returned in original log (seq) order
        let seqs: Vec<u64> = w.blocks.iter().map(|b| b.seq).collect();
        assert!(
            seqs.windows(2).all(|s| s[0] < s[1]),
            "window not in log order: {seqs:?}"
        );
        // the kept blocks' total size respects the byte budget (allowing a
        // single oversized survivor to exceed it)
        let total_bytes: usize = w.blocks.iter().map(|b| b.payload.len()).sum();
        assert!(
            total_bytes <= 1000 || w.blocks.len() == 1,
            "over budget: {total_bytes}"
        );
    }

    #[test]
    fn incremental_scheduler_ingest_block_stays_under_budget() {
        let receiver = Arc::new(Receiver::new(ContentStore::new(), Compressor::default()));
        let session: u128 = 1;
        let sched = IncrementalPruneScheduler::new(receiver.clone(), 300);
        // feed 4 blocks of 100 bytes each; budget is 300 -> keep 3.
        for i in 0..4u64 {
            let payload = vec![0u8; 100];
            let b = Block::new(BlockKind::Message, i, payload);
            let id = b.block_id();
            receiver.store().insert(session, b);
            sched.ingest_block(session, id, 100);
        }
        let w = sched.window(session).expect("window");
        let total_bytes: usize = w.blocks.iter().map(|b| b.payload.len()).sum();
        assert!(total_bytes <= 300, "over budget: {total_bytes}");
        assert_eq!(w.blocks.len(), 3);
    }

    /// The incremental prune's window is **independent of the log length N**: its
    /// size is bounded by the token budget (O(K)), not the growing history. This
    /// is the structural property the stateless `ImportancePolicy` re-scan lacks
    /// (it is O(N log N) in N). Grow the log 100× (1k -> 100k blocks) under a
    /// fixed budget and assert the survivor count and total size stay ~constant
    /// — i.e. producing the window did not grow with N. (Plan #19.)
    #[test]
    fn incremental_window_size_is_independent_of_log_length() {
        let session: u128 = 0x0005_CA1E;
        let budget = 5000;
        let block_bytes = 10;
        let window_for = |n: usize| {
            let receiver = Arc::new(Receiver::new(ContentStore::new(), Compressor::default()));
            for i in 0..n as u64 {
                let b = Block::new(BlockKind::Message, i, vec![0u8; block_bytes]);
                receiver.store().insert(session, b);
            }
            let sched = IncrementalPruneScheduler::new(receiver.clone(), budget);
            sched.ingest_session(session);
            let w = sched
                .window(session)
                .expect("ingested session has a window");
            // survivors in log order
            let seqs: Vec<u64> = w.blocks.iter().map(|b| b.seq).collect();
            assert!(
                seqs.windows(2).all(|s| s[0] < s[1]),
                "window not in log order at N={n}: {seqs:?}"
            );
            let total: usize = w.blocks.iter().map(|b| b.payload.len()).sum();
            assert!(
                total <= budget || w.blocks.len() == 1,
                "over budget at N={n}: {total}"
            );
            (w.blocks.len(), total)
        };
        let (n1, t1) = window_for(1_000);
        let (n2, t2) = window_for(100_000);
        // 100x more history, same survivor count and total size (both bounded by
        // the budget, not N). The window keeps the most recent `budget /
        // block_bytes` blocks in both cases.
        assert_eq!(n1, n2, "survivor count grew with N: {n1} -> {n2}");
        assert_eq!(t1, t2, "total size grew with N: {t1} -> {t2}");
    }
}
