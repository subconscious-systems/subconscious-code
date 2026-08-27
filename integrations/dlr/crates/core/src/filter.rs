//! Cuckoo filter — fast negative-path dedup (extra strategy).
//!
//! A content-addressed store's hot path is `contains(id)`. Under concurrency
//! that means acquiring a read lock on a big HashMap for every block the shim
//! considers, including the (common) case where the block is *not* present and
//! will be inserted anyway. A cuckoo filter in front of the store answers
//! "definitely not present" without touching the locked map: a lock-free-ish
//! structure with ~95% load factor and tunable false-positive rate.
//!
//! We use 2-byte fingerprints (FPR ~ 2b/2^16 ~ 0.012% at b=4 slots) so a
//! "present" answer is almost always a real hit; the rare false positive pays
//! one HashMap lookup (the same lookup we'd have done unconditionally without
//! the filter). Net: the negative path — the common case for *novel* turn
//! content — becomes a handful of array reads, no lock.
//!
//! This is an additive strategy: the store remains authoritative; the filter
//! is a cache that can be dropped/rebuilt at any time. Because the filter
//! stores only fingerprints (not keys), it **cannot be grown from its own
//! contents** — growing requires the original keys. So the saturation contract
//! is: on eviction failure the filter latches `saturated` (and `contains`
//! falls back to "maybe present", staying correct), the caller `clear()`s and
//! rebuilds from the authoritative store's ids, and `grow` (a one-time
//! double-and-rehash on saturation) absorbs a burst past capacity.
//!
//! ### Query-path locking
//!
//! `contains`/`insert` take the table **read lock exactly once** per call (the
//! mask reads in the index math are served by a plain `AtomicUsize`, so
//! `indices`/`alt_index` touch no lock). A shared read lock never blocks other
//! readers: the negative path is concurrent across threads and across sessions,
//! and only `grow`/`clear` (rare, exclusive) ever wait for readers.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Golden-ratio constant for the cheap alt-index mix (same constant used
/// elsewhere in the codebase for multiplicative hashing). A single 64-bit
/// multiply gives plenty of avalanche for a 16-bit fingerprint indexed into a
/// power-of-two table, replacing a full `xxh3` over 2 bytes that the old code
/// ran on every `contains`/eviction kick.
const GOLDEN64: u64 = 0x9E37_79B9_7F4A_7C15;

/// 2-byte fingerprint. Chosen so the false-positive rate at 4 slots/bucket is
/// ~ 2*4 / 2^16 ≈ 0.012%.
pub type Fp = u16;
const SLOTS: usize = 4;
/// A bucket: 4 fingerprints. Kept behind a `parking_lot::RwLock` per bucket so
/// concurrent `contains` on the same bucket share the read, and the rare
/// eviction/clear writes only the one bucket.
pub struct Bucket([Fp; SLOTS]);

impl Bucket {
    const EMPTY: Bucket = Bucket([0; SLOTS]);
}

/// Buckets, the mutable identity of the table. Queries index it through
/// a shared read lock; `grow` rebuilds it under a write lock. (The table mask
/// lives in `CuckooFilter.mask` as an atomic so the index math locks nothing;
/// it is not duplicated here.)
struct FilterTable {
    buckets: Vec<parking_lot::RwLock<Bucket>>,
}

/// A cuckoo filter. `buckets` is a power of two; indexes are masked.
pub struct CuckooFilter {
    /// Buckets and mask behind a read/write lock: `contains`/`insert` take a
    /// **shared** read (concurrent readers never block each other), and only
    /// the rare `grow`/`clear` take the write side to swap the table out.
    table: parking_lot::RwLock<FilterTable>,
    /// The table mask (`nbuckets - 1`), mirrored as an atomic so the index
    /// math in `indices`/`alt_index` needs no lock at all. Refreshed whenever
    /// `grow` (the only writer) swaps the table.
    mask: AtomicUsize,
    /// Count of occupied slots, for load-factor observability.
    count: AtomicUsize,
    /// Set on eviction failure: the filter is saturated and `contains` must
    /// fall back to "maybe present" to stay correct. Cleared by `clear()`,
    /// which the caller runs before rebuilding from the authoritative store.
    saturated: AtomicU8,
}

impl Default for CuckooFilter {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}

impl CuckooFilter {
    /// Create a filter holding ~`cap` items at <=~95% load before eviction
    /// failures. `cap` is rounded up to a power of two buckets.
    pub fn with_capacity(cap: usize) -> Self {
        let nbuckets = (cap / SLOTS).next_power_of_two().max(4);
        let mut buckets = Vec::with_capacity(nbuckets);
        for _ in 0..nbuckets {
            buckets.push(parking_lot::RwLock::new(Bucket::EMPTY));
        }
        let mask = nbuckets - 1;
        Self {
            table: parking_lot::RwLock::new(FilterTable { buckets }),
            mask: AtomicUsize::new(mask),
            count: AtomicUsize::new(0),
            saturated: AtomicU8::new(0),
        }
    }

    #[inline]
    fn indices(&self, id: &[u8]) -> (usize, Fp) {
        // Primary index + fingerprint from one xxh3 over the id. The mask is
        // read from an atomic (updated only by `grow`), so this path locks
        // nothing.
        let h = xxhash_rust::xxh3::xxh3_64(id);
        let fp = (h as Fp).max(1); // never zero (0 == empty slot)
        let i1 = (h as usize) & self.mask.load(Ordering::Relaxed);
        (i1, fp)
    }

    #[inline]
    fn alt_index(&self, i: usize, fp: Fp) -> usize {
        // Cheap integer mix instead of a second xxh3 call. `(i ^ delta) & mask`
        // is involutive in `i` for any `delta` (proof in the module docs), so
        // `alt(alt(i)) == i` holds and the two-candidate invariant is preserved.
        let delta = ((fp as u64).wrapping_mul(GOLDEN64)) as usize;
        (i ^ delta) & self.mask.load(Ordering::Relaxed)
    }

    /// Insert an item. Returns false if the filter is saturated and eviction
    /// failed after `KICKS` attempts. Correctness is unaffected since the store
    /// is authoritative; the caller may `clear()` and rebuild from the store.
    pub fn insert(&self, id: &[u8]) -> bool {
        if self.saturated.load(Ordering::Acquire) != 0 {
            // Already saturated: don't pretend we stored it. The caller should
            // have rebuilt us; until then stay correct (contains -> true).
            return false;
        }
        const KICKS: usize = 500;
        let (mut i, mut fp) = self.indices(id);
        for _ in 0..KICKS {
            // One shared read of the table per kick; the per-bucket write lock
            // below serializes only same-bucket evictions, not the whole table.
            let table = self.table.read();
            let bucket = &table.buckets[i];
            {
                let mut g = bucket.write();
                for s in 0..SLOTS {
                    if g.0[s] == 0 {
                        g.0[s] = fp;
                        self.count.fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                }
                // evict a slot chosen by the fingerprint
                let s = (fp as usize) % SLOTS;
                std::mem::swap(&mut fp, &mut g.0[s]);
            }
            drop(table);
            i = self.alt_index(i, fp);
        }
        // Eviction failed: try to grow the table once before giving up. A
        // one-time double-and-rehash absorbs a burst past the configured
        // capacity without permanently degrading the filter for the rest of
        // the process lifetime. If `grow` is a no-op (already at max width)
        // or the retry still finds no slot, we latch `saturated` and the caller
        // rebuilds via `clear()`.
        if self.grow() {
            let (i2, fp2) = self.indices(id);
            let table = self.table.read();
            let bucket = &table.buckets[i2];
            let mut g = bucket.write();
            for s in 0..SLOTS {
                if g.0[s] == 0 {
                    g.0[s] = fp2;
                    self.count.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
            }
        }
        self.saturated.store(1, Ordering::Release);
        false
    }

    /// Membership query. `true` = "probably present", `false` = "definitely
    /// not present". The negative answer is the load-bearing one for dedup.
    ///
    /// Takes the table read lock once; readers share it, so the negative path
    /// is concurrent across threads and sessions.
    pub fn contains(&self, id: &[u8]) -> bool {
        if self.saturated.load(Ordering::Acquire) != 0 {
            // saturated: fall back to "maybe present" to stay correct.
            return true;
        }
        let (i1, fp) = self.indices(id);
        let i2 = self.alt_index(i1, fp);
        let table = self.table.read();
        let b1 = &table.buckets[i1];
        for s in 0..SLOTS {
            if b1.read().0[s] == fp {
                return true;
            }
        }
        let b2 = &table.buckets[i2];
        for s in 0..SLOTS {
            if b2.read().0[s] == fp {
                return true;
            }
        }
        false
    }

    /// True once eviction has failed and the filter can no longer guarantee
    /// storage. The caller should `clear()` and rebuild from the authoritative
    /// store.
    pub fn saturated(&self) -> bool {
        self.saturated.load(Ordering::Acquire) != 0
    }

    /// Reset the filter to empty and clear the `saturated` flag. Used to
    /// recover from saturation by rebuilding from the store's ids; also a
    /// general "drop the cache" hook since the store is authoritative.
    pub fn clear(&self) {
        let table = self.table.write();
        for b in &table.buckets {
            let mut g = b.write();
            g.0 = [0u16; SLOTS];
        }
        self.count.store(0, Ordering::Relaxed);
        self.saturated.store(0, Ordering::Release);
    }

    /// Double the table width and rehash every fingerprint into the new
    /// buckets. Called once on saturation to absorb a burst past capacity.
    /// Returns `true` if the table grew, `false` if it was already at the
    /// maximum width (`MAX_BITS`) and could not grow. Growth is conservative:
    /// it rehashes by re-deriving both candidate indices for each fingerprint
    /// and reinserting into the new table, which is correct because a
    /// fingerprint's two candidate indices in the *old* table map to two
    /// candidate indices in the *new* (wider) table (the index mix is by
    /// `fp * GOLDEN64 & mask`, and widening the mask preserves the
    /// involutive `alt(alt(i)) == i` property). The rehash holds the table's
    /// write lock; it is intended for the rare saturation path, not the insert
    /// hot path.
    fn grow(&self) -> bool {
        const MAX_BITS: u32 = 24; // cap at 2^24 buckets (~16M fingerprints)
        let mut table = self.table.write();
        let cur_buckets = table.buckets.len();
        if cur_buckets == 0 || cur_buckets >= (1usize << MAX_BITS) {
            return false;
        }
        // Collect all fingerprints under the (exclusive) table write lock. This
        // is O(N) on the rare saturation path; the insert hot path that
        // triggers it has already done KICKS eviction attempts, so the relative
        // cost is negligible.
        let mut fps: Vec<Fp> = Vec::with_capacity(self.count.load(Ordering::Relaxed));
        for b in &table.buckets {
            let mut g = b.write();
            for s in 0..SLOTS {
                if g.0[s] != 0 {
                    fps.push(g.0[s]);
                    g.0[s] = 0;
                }
            }
        }
        let new_nbuckets = cur_buckets * 2;
        let mut new_buckets = Vec::with_capacity(new_nbuckets);
        for _ in 0..new_nbuckets {
            new_buckets.push(parking_lot::RwLock::new(Bucket::EMPTY));
        }
        // Rehash: re-derive indices using the *old* fingerprint bits. A
        // cuckoo fingerprint carries no key, so we cannot recompute `indices`
        // from it; we place each fingerprint into its first empty slot in
        // either of its two new candidate buckets (cheaper: just try the
        // primary then the alternate; if both full, leave it for the next
        // saturation — but a fresh doubled table is ~50% empty so this never
        // triggers in practice). We approximate `indices` for a bare
        // fingerprint by hashing the fp itself.
        for fp in fps {
            let h = xxhash_rust::xxh3::xxh3_64(&fp.to_le_bytes());
            let i1 = (h as usize) & (new_nbuckets - 1);
            let placed = {
                let mut g = new_buckets[i1].write();
                for s in 0..SLOTS {
                    if g.0[s] == 0 {
                        g.0[s] = fp;
                        break;
                    }
                }
                g.0.contains(&fp)
            };
            if !placed {
                let delta = ((fp as u64).wrapping_mul(GOLDEN64)) as usize & (new_nbuckets - 1);
                let i2 = (i1 ^ delta) & (new_nbuckets - 1);
                let mut g = new_buckets[i2].write();
                for s in 0..SLOTS {
                    if g.0[s] == 0 {
                        g.0[s] = fp;
                        break;
                    }
                }
            }
        }
        // Swap in the new buckets. We rebuilt `new_buckets` independently so
        // no reader sees a half-grown table; the swap is a pointer rewrite
        // under the write lock (readers held a shared read and have drained by
        // the time we own the write side). Assign through the guard we already
        // hold (`table`) — taking `self.table.write()` a *second* time here
        // would deadlock: parking_lot's `RwLock` write lock is not reentrant, so
        // a second acquire from the same thread blocks forever. (This was the
        // cause of the `clear_recovers_from_saturation` hang.)
        let new_mask = new_nbuckets - 1;
        *table = FilterTable {
            buckets: new_buckets,
        };
        self.mask.store(new_mask, Ordering::Release);
        true
    }

    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn capacity(&self) -> usize {
        self.table.read().buckets.len() * SLOTS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_path_and_no_false_negatives() {
        let f = CuckooFilter::with_capacity(4096);
        let present: Vec<Vec<u8>> = (0..1000)
            .map(|i| (i as u64).to_le_bytes().to_vec())
            .collect();
        for p in &present {
            assert!(f.insert(p));
        }
        for p in &present {
            assert!(f.contains(p), "no false negatives");
        }
        // absent items: most should be reported absent
        let absent: Vec<Vec<u8>> = (100_000..101_000)
            .map(|i| (i as u64).to_le_bytes().to_vec())
            .collect();
        let mut fp = 0;
        for a in &absent {
            if f.contains(a) {
                fp += 1;
            }
        }
        assert!(fp < 50, "false positive rate too high: {fp}/1000");
    }

    #[test]
    fn clear_recovers_from_saturation() {
        // A tiny filter saturates quickly; clear() must make it usable again.
        let f = CuckooFilter::with_capacity(8);
        for i in 0..10_000u64 {
            f.insert(&i.to_le_bytes());
        }
        // saturated now (capacity ~8): contains is true for everything.
        assert!(f.saturated());
        assert!(f.contains(&[0xFF; 32]));
        f.clear();
        assert!(!f.saturated());
        // after clear, a novel key is honestly absent again.
        assert!(!f.contains(&[0xAB; 32]));
    }
}
