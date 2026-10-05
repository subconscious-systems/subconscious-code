//! Join-semilattice for a distributed radix replicator.
//!
//! The set of cached prefixes under union is idempotent, commutative, and
//! associative — a join-semilattice, which is precisely the CRDT convergence
//! condition. Gossip in any order, no coordination, guaranteed convergence to
//! the same fixed point.
//!
//! The catch: **eviction is not monotone** and breaks the lattice. A plain
//! grow-only set can't remove anything; a 2P-set (add-set + tombstone-set) can
//! remove, but once a prefix is tombstoned it can't be re-added under 2P
//! semantics. So [`PrefixSet`] uses **per-entry LWW with an epoch counter**:
//! each entry carries a monotone `epoch`, and a tombstone at epoch `e` wins over
//! an entry at epoch `e' < e`, but a fresh `add` at epoch `e'' > e` wins back.
//! That asymmetry — the reason 2P-sets don't suffice — is exactly what the
//! epoch counter buys.
//!
//! ## Scope
//!
//! `rc-proto` carries no local cache; the provider owns the prefix cache. This
//! module is the **convergence-grade data structure only**: there is no radix
//! trie here, and no gossip transport. The `fingerprint`/`context_key` fields
//! of [`PrefixEntry`] are populated from the producers in `rc-ctx`
//! ([`PrefixFingerprint`](crate::seqhash::PrefixFingerprint) and
//! [`ContextKey`](crate::multiset::ContextKey)), so a future replicator that
//! gossips [`PrefixSet`]s is a drop-in.

use crate::multiset::ContextKey;
use crate::traits::Semilattice;
use std::collections::BTreeMap;

/// A cached prefix: its sequence fingerprint (KV-state identity, the
/// "sequence downstairs" layer) and its multiset context key (the eviction-
/// equivalent set, the "set upstairs" layer), plus the LWW epoch.
///
/// `context_key` is stored as a `Vec<u8>` (always length 2048) rather than a
/// fixed array so the entry is (de)serializable for gossip without pulling a
/// byte-array serde helper; a future replicator is free to swap in a more
/// compact wire encoding.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrefixEntry {
    /// Non-commutative sequence fingerprint — identifies the KV-cache state.
    pub fingerprint: u64,
    /// Order-independent multiset key (the [`ContextKey`] bytes, length 2048).
    pub context_key: Vec<u8>,
    /// Monotone epoch for LWW resolution. Higher epoch wins.
    pub epoch: u64,
}

impl PrefixEntry {
    /// Build an entry from a [`ContextKey`] (the "set upstairs" identifier).
    pub fn from_context_key(fingerprint: u64, context_key: &ContextKey, epoch: u64) -> Self {
        Self {
            fingerprint,
            context_key: context_key.as_bytes().to_vec(),
            epoch,
        }
    }
}

/// A CRDT set of cached prefixes under union-with-LWW. Join is the semilattice
/// operation; it is idempotent, commutative, and associative, so any gossip
/// order converges.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrefixSet {
    /// `fingerprint → live entry`. Absent means "no live entry".
    entries: BTreeMap<u64, PrefixEntry>,
    /// `fingerprint → tombstone epoch`. A tombstone hides any entry with a
    /// strictly smaller epoch; a newer add (epoch > tombstone) resurrects it.
    tombstones: BTreeMap<u64, u64>,
}

impl PrefixSet {
    /// The empty set (bottom of the semilattice).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `entry` under LWW: wins if `entry.epoch` is `>` any tombstone and
    /// (epoch, context_key) lexicographically beats the current entry's. A
    /// tombstone at a strictly higher epoch suppresses the add.
    ///
    /// Ties on equal epoch are broken by a **total order on the context_key
    /// bytes**, not insert order: "last insert wins" would make `join` (which
    /// clones `self` then folds in `other`) non-commutative for equal
    /// (fingerprint, epoch) pairs with different payloads — `a.join(b) != b.join(a)`.
    /// Comparing (epoch, context_key) keeps new entries from replacing an
    /// entry that already sorts higher, so either fold order converges to the
    /// same representative.
    pub fn add(&mut self, entry: PrefixEntry) {
        let fp = entry.fingerprint;
        if let Some(&tomb_epoch) = self.tombstones.get(&fp) {
            if entry.epoch <= tomb_epoch {
                // Tombstone wins; entry stays hidden.
                return;
            }
        }
        match self.entries.get(&fp) {
            Some(existing)
                if (existing.epoch, &existing.context_key) > (entry.epoch, &entry.context_key) =>
            {
                // Older-than-current (or equal and lower-ordered); keep current.
            }
            _ => {
                self.entries.insert(fp, entry);
            }
        }
    }

    /// Evict `fingerprint` at `epoch`: records a tombstone that hides any
    /// entry with a strictly smaller epoch. Idempotent at a given epoch.
    pub fn evict(&mut self, fingerprint: u64, epoch: u64) {
        let tomb = self.tombstones.get(&fingerprint).copied().unwrap_or(0);
        if epoch >= tomb {
            self.tombstones.insert(fingerprint, epoch);
        }
        if let Some(e) = self.entries.get(&fingerprint) {
            if e.epoch <= epoch {
                self.entries.remove(&fingerprint);
            }
        }
    }

    /// Is `fingerprint` currently live (present and not tombstoned at a higher
    /// epoch)?
    pub fn contains(&self, fingerprint: u64) -> bool {
        if let Some(entry) = self.entries.get(&fingerprint) {
            !matches!(self.tombstones.get(&fingerprint), Some(tomb) if *tomb >= entry.epoch)
        } else {
            false
        }
    }

    /// The live entries (present, not suppressed by a tombstone).
    pub fn live(&self) -> Vec<&PrefixEntry> {
        self.entries
            .iter()
            .filter(|(fp, e)| !matches!(self.tombstones.get(fp), Some(tomb) if *tomb >= e.epoch))
            .map(|(_, e)| e)
            .collect()
    }

    /// The number of live entries.
    pub fn len(&self) -> usize {
        self.live().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Semilattice for PrefixSet {
    /// Join: union of live entries with per-fingerprint LWW (highest epoch
    /// wins), and union of tombstones (highest epoch wins). Idempotent,
    /// commutative, associative.
    fn join(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for entry in other.entries.values() {
            out.add(entry.clone());
        }
        for (fp, epoch) in &other.tombstones {
            out.evict(*fp, *epoch);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fp: u64, epoch: u64) -> PrefixEntry {
        PrefixEntry {
            fingerprint: fp,
            context_key: vec![0u8; 2048],
            epoch,
        }
    }

    #[test]
    fn join_is_commutative() {
        let mut a = PrefixSet::new();
        a.add(entry(1, 1));
        let mut b = PrefixSet::new();
        b.add(entry(2, 1));
        assert_eq!(a.join(&b), b.join(&a));
    }

    /// The review's failure case: equal (fingerprint, epoch) with different
    /// context keys. "Insert wins" made join non-commutative; the total-order
    /// tie-break on the context_key bytes restores it and picks the same
    /// representative from either side.
    #[test]
    fn join_is_commutative_on_equal_epoch_ties() {
        let key = |lead: u8| {
            let mut context_key = vec![0u8; 2048];
            context_key[0] = lead;
            context_key
        };
        let with_key = |fp: u64, epoch: u64, lead: u8| PrefixEntry {
            fingerprint: fp,
            context_key: key(lead),
            epoch,
        };
        let mut a = PrefixSet::new();
        a.add(with_key(7, 5, 0x11));
        let mut b = PrefixSet::new();
        b.add(with_key(7, 5, 0x22));
        let joined_ab = a.join(&b);
        let joined_ba = b.join(&a);
        assert_eq!(
            joined_ab, joined_ba,
            "join must be commutative on equal (fingerprint, epoch) ties"
        );
        // Either fold order keeps the same representative — the entry whose
        // context_key sorts higher, regardless of who folded into whom.
        let live_ab = joined_ab.live();
        assert_eq!(live_ab.len(), 1);
        assert_eq!(live_ab[0].context_key[0], 0x22);
        let live_ba = joined_ba.live();
        assert_eq!(live_ba[0].context_key[0], 0x22);
    }

    /// The tie-break must not weaken plain LWW: a strictly higher epoch still
    /// wins over a higher-sorted context key.
    #[test]
    fn higher_epoch_beats_context_key_order() {
        let mut key_low = vec![0u8; 2048];
        key_low[0] = 0xEE;
        let mut key_high = vec![0u8; 2048];
        key_high[0] = 0x01;
        let mut a = PrefixSet::new();
        a.add(PrefixEntry {
            fingerprint: 7,
            context_key: key_low,
            epoch: 9,
        });
        let mut b = PrefixSet::new();
        b.add(PrefixEntry {
            fingerprint: 7,
            context_key: key_high,
            epoch: 3,
        });
        let joined = a.join(&b);
        let live = joined.live();
        assert_eq!(live.len(), 1);
        assert_eq!(
            live[0].epoch, 9,
            "epoch dominates the context_key tie-break"
        );
    }

    #[test]
    fn join_is_associative() {
        let mut a = PrefixSet::new();
        a.add(entry(1, 1));
        let mut b = PrefixSet::new();
        b.add(entry(2, 1));
        let mut c = PrefixSet::new();
        c.add(entry(3, 1));
        let ab_c = a.join(&b).join(&c);
        let a_bc = a.join(&b.join(&c));
        assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn join_is_idempotent() {
        let mut a = PrefixSet::new();
        a.add(entry(1, 1));
        a.add(entry(2, 1));
        assert_eq!(a.join(&a), a);
    }

    #[test]
    fn evict_then_re_add_with_higher_epoch() {
        // The 2P-set failure: once tombstoned, can't re-add. LWW-epoch fixes it.
        let mut s = PrefixSet::new();
        s.add(entry(7, 1));
        assert!(s.contains(7));
        s.evict(7, 2);
        assert!(
            !s.contains(7),
            "tombstone at epoch 2 hides entry at epoch 1"
        );
        // Re-add at a strictly higher epoch resurrects it.
        s.add(entry(7, 3));
        assert!(s.contains(7), "a higher-epoch add must beat the tombstone");
    }

    #[test]
    fn evict_below_epoch_is_noop() {
        let mut s = PrefixSet::new();
        s.add(entry(7, 5));
        s.evict(7, 3); // tombstone below the entry epoch
        assert!(
            s.contains(7),
            "low-epoch tombstone must not evict a newer entry"
        );
    }

    #[test]
    fn lww_keeps_highest_epoch_on_join() {
        let mut a = PrefixSet::new();
        a.add(entry(7, 1));
        let mut b = PrefixSet::new();
        b.add(entry(7, 9));
        let joined = a.join(&b);
        let live = joined.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].epoch, 9);
    }

    #[test]
    fn convergence_under_any_gossip_order() {
        // Three nodes; each adds a distinct entry and evicts a shared one.
        // Any pairwise join order must reach the same fixed point.
        let mut n1 = PrefixSet::new();
        n1.add(entry(1, 1));
        n1.add(entry(10, 1));
        n1.evict(10, 2); // n1 also tombstones 10 at epoch 2

        let mut n2 = PrefixSet::new();
        n2.add(entry(2, 1));

        let mut n3 = PrefixSet::new();
        n3.add(entry(3, 1));

        let order_a = n1.join(&n2).join(&n3);
        let order_b = n2.join(&n3).join(&n1);
        let order_c = n3.join(&n1).join(&n2);
        assert_eq!(order_a, order_b);
        assert_eq!(order_b, order_c);
        // Entry 10 is tombstoned everywhere.
        assert!(!order_a.contains(10));
        // Entries 1,2,3 survive.
        assert!(order_a.contains(1) && order_a.contains(2) && order_a.contains(3));
    }
}
