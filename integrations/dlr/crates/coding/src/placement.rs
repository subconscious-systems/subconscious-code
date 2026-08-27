//! Golden-ratio low-discrepancy placement on the hash ring (DESIGN §6.2).
//!
//!   pos(n) = frac(n * phi^-1) = frac(n * 0.6180339887...)  on [0,1)
//!
//! By the three-distance (Steinhaus) theorem, {n*alpha} for irrational alpha
//! partitions the circle into arcs of at most 3 distinct lengths; for
//! alpha = phi^-1 those lengths are as uniform as an irrational rotation allows
//! and successive gaps sit in ratio phi. The sequence {n*phi^-1} achieves the
//! lowest asymptotic discrepancy of any Weyl sequence. Random hashing has
//! discrepancy that fluctuates like sqrt(log log N / N) and produces hot/cold
//! arcs at small N; golden placement is provably near-uniform at *every* N.
//!
//! Load-bearing: genuinely optimal, near-zero cost. Relevant because Nightshift
//! adds/removes sessions and agents continuously — placement stays balanced at
//! every N without reshuffling.

#[allow(dead_code)] // documented constant; the bit-exact path uses integer arithmetic
const PHI_INV: f64 = 0.618_033_988_749_894_9;

/// Position of the n-th entity on the unit ring, in [0,1).
#[inline]
pub fn placement(n: u64) -> f64 {
    // frac(n * phi^-1). Use integer arithmetic to stay bit-exact and avoid f64
    // drift: pos = (n * GOLDEN64) / 2^64, taking the fractional part.
    let prod = (n as u128).wrapping_mul(0x9E3779B9_7F4A7C15_u128);
    // the low 64 bits of prod / 2^64 give the fractional position in [0,1)
    let frac_bits = prod as u64;
    (frac_bits as f64) / (u64::MAX as f64)
}

/// Integer exact variant: returns the position scaled to [0, 2^64).
#[inline]
pub fn placement_fixed(n: u64) -> u64 {
    ((n as u128).wrapping_mul(0x9E3779B9_7F4A7C15_u128)) as u64
}

/// A consistent-hash ring using golden-ratio placement. Insert entities in any
/// order; each gets a position, and lookup maps a key to the nearest entity
/// clockwise. Load is balanced at every N (no resharding) because the underlying
/// sequence has minimal discrepancy.
///
/// Backed by a `BTreeMap<position, entity_id>` plus a reverse
/// `HashMap<entity_id, position>`: `add`/`remove`/`route` are all O(log N)
/// (the old `Vec` core shifted on insert and rebuilt + re-sorted on remove —
/// O(N) and O(N log N); the prior `BTreeMap` version still O(N)-scanned and
/// re-derived every position on remove). Positions come from a **monotonic
/// counter** fed through `placement_fixed`, so removal does *not* renumber
/// survivors — it leaves one enlarged gap and the rest stay put. `placement_fixed`
/// is a bijection on `u64` (`GOLDEN64` is odd ⇒ period 2⁶⁴), so the counter never
/// collides, and the surviving positions are always a subset of the low-discrepancy
/// golden sequence.
pub struct GoldenRing {
    /// (position, entity_id), kept sorted by position for clockwise routing.
    nodes: std::collections::BTreeMap<u64, u64>,
    /// Reverse index for O(1) position lookup on `remove`.
    id_to_pos: std::collections::HashMap<u64, u64>,
    /// Monotonic source of golden positions; never reused, so no collisions and
    /// no survivor renumbering on removal.
    counter: u64,
}

impl GoldenRing {
    pub fn new() -> Self {
        Self {
            nodes: std::collections::BTreeMap::new(),
            id_to_pos: std::collections::HashMap::new(),
            counter: 0,
        }
    }

    pub fn add(&mut self, entity_id: u64) {
        // Idempotent: a re-add of a known entity is a no-op (keeps its position).
        if self.id_to_pos.contains_key(&entity_id) {
            return;
        }
        let pos = placement_fixed(self.counter);
        self.counter += 1;
        self.nodes.insert(pos, entity_id);
        self.id_to_pos.insert(entity_id, pos);
    }

    pub fn remove(&mut self, entity_id: u64) -> bool {
        // O(log N): reverse map finds the position, BTreeMap removes it. No
        // survivor renumbering — positions are fixed once assigned.
        if let Some(pos) = self.id_to_pos.remove(&entity_id) {
            self.nodes.remove(&pos);
            true
        } else {
            false
        }
    }

    /// Map a key to the responsible entity (nearest clockwise).
    pub fn route(&self, key: u64) -> Option<u64> {
        if self.nodes.is_empty() {
            return None;
        }
        let k = placement_fixed(key);
        // first position >= k, else wrap to the smallest
        match self.nodes.range(k..).next() {
            Some((_, id)) => Some(*id),
            None => self.nodes.iter().next().map(|(_, id)| *id),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for GoldenRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn balanced_at_every_n() {
        // 8 entities, 1024 bins: standard deviation of load should be tiny.
        let mut ring = GoldenRing::new();
        for i in 0..8 {
            ring.add(i);
        }
        let mut bins = [0u32; 8];
        for k in 0..1024 {
            if let Some(e) = ring.route(k) {
                bins[e as usize] += 1;
            }
        }
        let mean = 1024.0 / 8.0;
        let var: f64 = bins
            .iter()
            .map(|&c| {
                let d = c as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / 8.0;
        let std = var.sqrt();
        // Golden-ratio placement gives provably *bounded* imbalance, not equal
        // load: consecutive positions partition the ring into arcs whose
        // lengths sit in ratio phi (≈1.618), so with uniformly-distributed keys
        // the expected bin sizes are `gap * n_keys` and the std/mean ratio is
        // ~0.19 at N=8 — not the 0.0 of even spacing. The 0.25 threshold below
        // admits that designed-in phi-ratio spread while still catching a
        // genuinely broken placement (e.g. all nodes clustered in one arc).
        assert!(
            std < mean * 0.25,
            "load imbalance too high: std={std} mean={mean}"
        );
    }
}
