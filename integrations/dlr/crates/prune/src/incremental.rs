//! Incremental prune (extra strategy).
//!
//! The doc's prune is "heavy, accurate, async, off the hot path." But even off
//! the path, a *re-scan* prune that re-scores the entire log every turn is O(N)
//! in the growing history — fine for a while, then not. An **incremental**
//! pruner treats the window as a maintained top-K structure: each new block is
//! scored and inserted in O(log K); when the budget overflows the lowest-scored
//! survivor is evicted in O(log K). Re-producing the window is O(K) (it is
//! already maintained). Prune cost becomes independent of N.
//!
//! The *real* accurate prune (the secret one) lives in the compiled runtime.
//! This is its incremental *skeleton*: the score function is the swappable
//! policy hook (`new_with`). We ship a recency×size score that is a reasonable
//! stand-in and keeps the data structures exercised; the runtime swaps in its
//! own scorer without touching the maintenance code.
//!
//! Two budget shapes are supported: a block-count top-K (`Budget::Blocks`,
//! the original skeleton) and a token/size budget (`Budget::Tokens`, what the
//! ~100k-token window actually wants). `IncrementalPruneScheduler` wires the
//! token-budgeted pruner to a `Receiver` so the per-turn prune is O(K), not
//! O(N log N).

use std::collections::BTreeMap;

use ahash::AHashMap;

/// The retention budget.
#[derive(Clone, Copy, Debug)]
pub enum Budget {
    /// Keep at most N blocks (top-K by score).
    Blocks(usize),
    /// Keep blocks until their total size exceeds the budget, evicting the
    /// lowest-scored survivor(s) to stay under it. A single block larger than
    /// the budget is still retained (the window never goes empty from one
    /// oversized block).
    Tokens(usize),
}

/// A scored block in the prune window.
#[derive(Clone, Copy)]
pub struct ScoredBlock {
    pub id: [u8; 32],
    pub score: u64,
    pub seq: u64,
    pub size: usize,
}

/// Incremental top-K pruner. Maintains the K highest-scoring blocks; inserts
/// and evictions are O(log K); window reproduction is O(K).
pub struct IncrementalPruner {
    budget: Budget,
    /// ordered by (score, seq) so the min is the eviction candidate.
    ordered: BTreeMap<(u64, u64), [u8; 32]>,
    /// id -> (score, seq, size) for O(log K) removal-on-update and log-order
    /// window reproduction.
    index: AHashMap<[u8; 32], (u64, u64, usize)>,
    /// total size of current survivors (for `Budget::Tokens`).
    total_size: usize,
    /// monotonic clock for tie-breaking so equal scores don't collide.
    clock: u64,
    /// pluggable score function: (seq, size) -> score. `seq` is the insertion
    /// order (recency); the runtime swaps in its own scorer here.
    scorer: Box<dyn Fn(u64, usize) -> u64 + Send + Sync>,
}

/// Default score: recency dominates (so the live turn's blocks win), size
/// breaks ties toward larger context blocks. `seq << 20` keeps recency in the
/// high bits; the low 20 bits carry a size proxy (capped to 1 MiB).
fn default_score(seq: u64, size: usize) -> u64 {
    (seq << 20) | ((size as u64).min((1 << 20) - 1))
}

impl IncrementalPruner {
    /// Block-count top-K (the original skeleton shape).
    pub fn new(budget: usize) -> Self {
        Self::new_with(Budget::Blocks(budget), Box::new(default_score))
    }

    /// Token/size budget — the shape the ~100k-token window actually wants.
    pub fn new_token_budget(token_budget: usize) -> Self {
        Self::new_with(Budget::Tokens(token_budget), Box::new(default_score))
    }

    /// Full control: choose the budget and the scoring policy.
    pub fn new_with(budget: Budget, scorer: Box<dyn Fn(u64, usize) -> u64 + Send + Sync>) -> Self {
        Self {
            budget,
            ordered: BTreeMap::new(),
            index: AHashMap::new(),
            total_size: 0,
            clock: 0,
            scorer,
        }
    }

    /// Insert a block; evicts the lowest-scored survivor(s) if over budget.
    /// Returns the *first* evicted id, if any (so the caller can drop it from
    /// the store). Re-inserting an existing id updates its score/size.
    pub fn insert(&mut self, id: [u8; 32], size: usize) -> Option<[u8; 32]> {
        // remove old entry if present (re-insert / score update).
        if let Some(&(_, old_seq, old_size)) = self.index.get(&id) {
            self.ordered.remove(&(self.index[&id].0, old_seq));
            self.total_size -= old_size;
        }

        self.clock += 1;
        let s = (self.scorer)(self.clock, size);
        self.ordered.insert((s, self.clock), id);
        self.index.insert(id, (s, self.clock, size));
        self.total_size += size;

        // Evict to satisfy the budget. `Tokens` may need several evictions;
        // `Blocks` at most one. We never evict the last survivor to satisfy a
        // token budget — a single block larger than the budget is retained
        // rather than producing an empty window.
        let mut evicted: Option<[u8; 32]> = None;
        loop {
            let over = match self.budget {
                Budget::Blocks(b) => self.ordered.len() > b,
                Budget::Tokens(b) => self.total_size > b && self.ordered.len() > 1,
            };
            if !over {
                break;
            }
            let ((_s, _seq), ev_id) = self.ordered.pop_first().unwrap();
            let (_, _, ev_size) = self.index.remove(&ev_id).unwrap();
            self.total_size -= ev_size;
            if evicted.is_none() {
                evicted = Some(ev_id);
            }
            if matches!(self.budget, Budget::Blocks(_)) {
                break;
            }
        }
        evicted
    }

    /// Current window: the surviving block ids, highest score first.
    pub fn window(&self) -> Vec<[u8; 32]> {
        self.ordered.iter().rev().map(|(_, id)| *id).collect()
    }

    /// Survivors in original insertion (log) order — the order the inference
    /// backend expects. O(K log K), independent of the log length N.
    pub fn window_ordered(&self) -> Vec<[u8; 32]> {
        let mut v: Vec<([u8; 32], u64)> = self
            .index
            .iter()
            .map(|(id, &(_, seq, _))| (*id, seq))
            .collect();
        v.sort_unstable_by_key(|(_, seq)| *seq);
        v.into_iter().map(|(id, _)| id).collect()
    }

    pub fn len(&self) -> usize {
        self.ordered.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }
    /// Total size of current survivors (for `Budget::Tokens`).
    pub fn total_size(&self) -> usize {
        self.total_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintains_top_k_and_evicts_lowest() {
        let mut p = IncrementalPruner::new(3);
        p.insert([1; 32], 100);
        p.insert([2; 32], 100);
        p.insert([3; 32], 100);
        // over budget -> [1] (oldest, lowest recency) evicted
        let evicted = p.insert([4; 32], 100);
        assert_eq!(evicted, Some([1u8; 32]));
        let w = p.window();
        assert_eq!(w.len(), 3);
        // newest is first
        assert_eq!(w[0], [4u8; 32]);
    }

    #[test]
    fn token_budget_evicts_until_under_budget() {
        // budget 300 bytes; blocks are 100 each -> keep 3, evict on the 4th.
        let mut p = IncrementalPruner::new_token_budget(300);
        p.insert([1; 32], 100);
        p.insert([2; 32], 100);
        p.insert([3; 32], 100);
        assert_eq!(p.total_size(), 300);
        // 4th pushes total to 400 > 300 -> evict lowest (oldest) until <= 300.
        let evicted = p.insert([4; 32], 100);
        assert_eq!(evicted, Some([1u8; 32]));
        assert_eq!(p.total_size(), 300);
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn token_budget_keeps_one_oversized_block() {
        // a single block larger than the budget is retained, not evicted to
        // produce an empty window.
        let mut p = IncrementalPruner::new_token_budget(100);
        let ev = p.insert([9; 32], 10_000);
        assert_eq!(ev, None);
        assert_eq!(p.len(), 1);
        assert_eq!(p.total_size(), 10_000);
    }

    #[test]
    fn window_ordered_is_log_order() {
        let mut p = IncrementalPruner::new(10);
        p.insert([3; 32], 1);
        p.insert([1; 32], 1);
        p.insert([2; 32], 1);
        // insertion (log) order, not score order
        assert_eq!(p.window_ordered(), vec![[3u8; 32], [1u8; 32], [2u8; 32]]);
    }

    #[test]
    fn custom_scorer_overrides_default() {
        // a scorer that ignores recency and ranks only by size: the smallest
        // block is evicted first regardless of age.
        let mut p =
            IncrementalPruner::new_with(Budget::Blocks(2), Box::new(|_seq, size| size as u64));
        p.insert([1; 32], 10);
        p.insert([2; 32], 50);
        // over budget (3 > 2): lowest score is size 10 ([1]) -> evicted.
        let evicted = p.insert([3; 32], 30);
        assert_eq!(evicted, Some([1u8; 32]));
    }

    #[test]
    fn reinsert_updates_size_and_score() {
        let mut p = IncrementalPruner::new_token_budget(1000);
        p.insert([1; 32], 100);
        p.insert([1; 32], 500); // update same id
        assert_eq!(p.len(), 1);
        assert_eq!(p.total_size(), 500);
    }
}
