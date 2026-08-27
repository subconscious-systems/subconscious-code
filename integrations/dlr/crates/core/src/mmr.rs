//! Merkle Mountain Range — fork/range-proof friendly append log (extra strategy).
//!
//! The linear Merkle root (`root_N = H(root_{N-1} || id_N)`) is O(1) per append
//! and perfect for the steady-state ACK, but it cannot answer "prove block `i`
//! is in the log at root R" without O(N) re-hashing, and it does not support
//! forking sessions cheaply. The distillation sink and the trace-provenance
//! path want both.
//!
//! An MMR is an append-only forest of perfect binary trees. We store it with
//! each perfect subtree represented **once** in a shared, append-only arena:
//! a merge inside `append` links the two child subtrees as children of one new
//! arena node instead of copying either subtree's nodes (the old heap-concat
//! layout copied every subtree root-to-leaf per merge, O(N log N) total for a
//! full log). Appends are O(log N) hashing + O(log N) arena pushes; a bagged
//! peak gives a single root; inclusion proofs are O(log N).
//!
//! This composes with — does not replace — the linear root. The wire still
//! ACKs the linear `base_root`; the MMR is the structured index behind it for
//! provenance, forking, and range queries on the receiver/sink side.

use blake3::Hasher;

use crate::block::BlockId;

pub type MmrHash = [u8; 32];

#[inline]
fn parent(left: &MmrHash, right: &MmrHash) -> MmrHash {
    let mut h = Hasher::new();
    h.update(b"M");
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

#[inline]
fn leaf_h(id: &BlockId) -> MmrHash {
    let mut h = Hasher::new();
    h.update(b"L");
    h.update(id);
    *h.finalize().as_bytes()
}

/// One node in the tree arena. Internal nodes carry the combined hash plus the
/// arena indices of their children; leaves have no children. Nodes are
/// immutable once pushed, which is what lets merges *link* subtrees instead of
/// copying them.
#[derive(Clone, Copy)]
struct MmrNode {
    hash: MmrHash,
    left: Option<usize>,
    right: Option<usize>,
}

/// One perfect binary tree: its root node in the shared arena, plus the global
/// leaf range it covers. `height` = log2(leaf_count); `base` = the global leaf
/// index of its leftmost leaf.
struct Tree {
    /// Arena index of this tree's root `MmrNode`.
    root: usize,
    height: u32,
    base: u64, // first global leaf index covered
}

impl Tree {
    fn leaf_count(&self) -> u64 {
        1u64 << self.height
    }
    fn leaf_range(&self) -> std::ops::Range<u64> {
        self.base..self.base + self.leaf_count()
    }
}

/// Merkle Mountain Range.
pub struct Mmr {
    trees: Vec<Tree>,
    leaves: u64,
    /// Shared, append-only arena of tree nodes. Every perfect subtree is stored
    /// exactly once and linked into the merges above it, so total memory is the
    /// ~2N-node tree family itself (never a per-merge copy) and `append` costs
    /// O(log N) arena pushes.
    arena: Vec<MmrNode>,
}

impl Default for Mmr {
    fn default() -> Self {
        Self::new()
    }
}

impl Mmr {
    pub fn new() -> Self {
        Self {
            trees: Vec::new(),
            leaves: 0,
            arena: Vec::new(),
        }
    }
    pub fn leaf_count(&self) -> u64 {
        self.leaves
    }
    pub fn is_empty(&self) -> bool {
        self.leaves == 0
    }

    /// Append a leaf (block id). O(log N) hashing + O(log N) arena pushes:
    /// merge equal-height trees by linking their roots under one new node.
    pub fn append(&mut self, id: BlockId) {
        let leaf_node = self.arena.len();
        self.arena.push(MmrNode {
            hash: leaf_h(&id),
            left: None,
            right: None,
        });
        let mut tree = Tree {
            root: leaf_node,
            height: 0,
            base: self.leaves,
        };
        self.leaves += 1;
        while let Some(prev) = self.trees.last() {
            if prev.height == tree.height {
                // merge: new root = H(prev.root, tree.root); link both subtrees
                // as children of one new arena node — no node copies.
                let prev = self.trees.pop().unwrap();
                let new_root = self.arena.len();
                let root_hash = parent(&self.arena[prev.root].hash, &self.arena[tree.root].hash);
                self.arena.push(MmrNode {
                    hash: root_hash,
                    left: Some(prev.root),
                    right: Some(tree.root),
                });
                tree = Tree {
                    root: new_root,
                    height: prev.height + 1,
                    base: prev.base,
                };
            } else {
                break;
            }
        }
        self.trees.push(tree);
    }

    /// Bag all peaks into a single root. Iterates the trees directly rather
    /// than collecting into an intermediate `Vec`, so a root query allocates
    /// nothing.
    pub fn root(&self) -> MmrHash {
        let mut iter = self.trees.iter();
        let first = match iter.next() {
            Some(t) => self.arena[t.root].hash,
            None => return [0u8; 32],
        };
        let mut acc = first;
        for t in iter {
            acc = parent(&acc, &self.arena[t.root].hash);
        }
        acc
    }

    pub fn peaks(&self) -> Vec<MmrHash> {
        self.trees.iter().map(|t| self.arena[t.root].hash).collect()
    }

    /// Inclusion proof for leaf `i`: sibling hashes from leaf up to its peak.
    /// Each entry is `(sibling_hash, sibling_is_left)`.
    ///
    /// Walks the arena tree top-down from the peak, recording at each level the
    /// child we are *not* descending into, then reverses leaf-to-root for
    /// `peak_from_proof`.
    pub fn inclusion_proof(&self, i: u64) -> Vec<(MmrHash, bool)> {
        let tree = self.trees.iter().find(|t| t.leaf_range().contains(&i));
        let Some(tree) = tree else { return Vec::new() };
        let local = (i - tree.base) as usize;
        let mut steps: Vec<(usize, bool)> = Vec::new();
        let mut h = tree.height;
        let mut leaf = local;
        let mut node = tree.root;
        while h > 0 {
            let half = 1usize << (h - 1); // leaves in the left subtree
            let n = &self.arena[node];
            let left_root = n.left.expect("internal node has a left child");
            let right_root = n.right.expect("internal node has a right child");
            if leaf < half {
                // leaf is in the left subtree; sibling is the right subtree root
                steps.push((right_root, false));
                node = left_root;
            } else {
                // leaf is in the right subtree; sibling is the left subtree root
                steps.push((left_root, true));
                node = right_root;
                leaf -= half;
            }
            h -= 1;
        }
        steps.reverse(); // leaf-to-root
        steps
            .into_iter()
            .map(|(idx, sib_is_left)| (self.arena[idx].hash, sib_is_left))
            .collect()
    }

    /// Verify an inclusion proof. Reconstructs the leaf's peak; the caller
    /// checks it against the known peak set (`Mmr::peaks`).
    pub fn peak_from_proof(leaf_id: &BlockId, proof: &[(MmrHash, bool)]) -> MmrHash {
        let mut acc = leaf_h(leaf_id);
        for (sib, sib_is_left) in proof {
            acc = if *sib_is_left {
                parent(sib, &acc)
            } else {
                parent(&acc, sib)
            };
        }
        acc
    }

    /// Full inclusion check: leaf `i` is in this MMR iff its reconstructed peak
    /// equals the peak of the tree covering `i`.
    pub fn verify_inclusion(&self, i: u64, leaf_id: &BlockId) -> bool {
        let Some(tree) = self.trees.iter().find(|t| t.leaf_range().contains(&i)) else {
            return false;
        };
        let proof = self.inclusion_proof(i);
        Self::peak_from_proof(leaf_id, &proof) == self.arena[tree.root].hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(b: u8) -> BlockId {
        [b; 32]
    }

    #[test]
    fn append_root_and_proofs() {
        let mut m = Mmr::new();
        for i in 0..7u64 {
            m.append(id(i as u8));
        }
        assert_eq!(m.leaf_count(), 7);
        // 7 leaves -> trees of heights 2,1,0 (3 peaks)
        assert_eq!(m.peaks().len(), 3);
        // every leaf verifies
        for i in 0..7u64 {
            assert!(
                m.verify_inclusion(i, &id(i as u8)),
                "leaf {i} should verify"
            );
        }
        // a wrong leaf does not
        assert!(!m.verify_inclusion(3, &id(99)));
    }
}
