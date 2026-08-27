//! Group-structured interconnect: Cayley / circulant overlays (DESIGN §6.7).
//!
//! For the multi-node case — gateway replicas / DPUs gossiping session state and
//! reconstructing across the fabric — lay the replication overlay on a circulant
//! graph C_n(s_1..s_k), the Cayley graph of Z_n with connection set {+/-s_i}.
//! Choosing the jumps Fibonacci-spaced (or as generators minimizing diameter)
//! yields a vertex-transitive, low-diameter, high-bisection overlay: every node
//! routes symmetrically, no hot central relay, diameter ~O(log n) with the right
//! generators. Matters at >= several replicas; irrelevant for a single receiver.

use std::collections::VecDeque;

#[derive(Debug, thiserror::Error)]
pub enum CayleyError {
    #[error("node id {id} out of range for n={n}")]
    OutOfRange { id: usize, n: usize },
    #[error("no path found {from}->{to}")]
    NoPath { from: usize, to: usize },
}

/// A circulant graph C_n(s_1, ..., s_k): node i connects to (i +/- s_j) mod n.
/// This is the Cayley graph of the cyclic group Z_n with connection set
/// {+/-s_j}. Vertex-transitive by construction (translation in the group is a
/// graph automorphism), so every node is structurally identical — no hot relay.
pub struct CayleyGraph {
    pub n: usize,
    /// Sorted, positive, distinct jumps in 1..n/2.
    pub jumps: Vec<usize>,
}

impl CayleyGraph {
    /// Build C_n(jumps). Jumps are normalized to the minimal representative in
    /// [1, n/2] (since +/-s are both edges). Fibonacci-spaced jumps give a good
    /// diameter/degree trade-off; see `fibonacci_jumps`.
    pub fn new(n: usize, mut jumps: Vec<usize>) -> Self {
        let half = n / 2;
        for s in jumps.iter_mut() {
            *s %= n;
            if *s == 0 {
                *s = 1;
            }
            if *s > half {
                *s = n - *s;
            }
        }
        jumps.sort_unstable();
        jumps.dedup();
        Self { n, jumps }
    }

    /// Fibonacci-spaced jumps for degree 2k, minimizing diameter heuristically.
    /// Returns k jumps drawn from the Fibonacci sequence mod n.
    pub fn fibonacci_jumps(n: usize, degree: usize) -> Self {
        let mut fibs = vec![1usize, 2];
        while *fibs.last().unwrap() < n / 2 {
            let next = fibs[fibs.len() - 1] + fibs[fibs.len() - 2];
            fibs.push(next);
        }
        // take `degree` evenly spaced fibonacci jumps up to n/2
        let half = n / 2;
        let candidates: Vec<usize> = fibs.into_iter().filter(|&f| f > 0 && f <= half).collect();
        let mut chosen = Vec::new();
        if candidates.is_empty() {
            chosen.push(1);
        } else {
            let step = (candidates.len() / degree).max(1);
            for i in (0..candidates.len()).step_by(step) {
                chosen.push(candidates[i]);
                if chosen.len() == degree {
                    break;
                }
            }
        }
        Self::new(n, chosen)
    }

    /// Neighbors of node `u` (+/- each jump mod n).
    pub fn neighbors(&self, u: usize) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.jumps.len() * 2);
        for &s in &self.jumps {
            out.push((u + s) % self.n);
            out.push((u + self.n - s) % self.n);
        }
        out
    }

    /// BFS shortest path from `from` to `to`. Vertex-transitivity keeps average
    /// path length identical regardless of source.
    pub fn shortest_path(&self, from: usize, to: usize) -> Result<Vec<usize>, CayleyError> {
        if from >= self.n || to >= self.n {
            return Err(CayleyError::OutOfRange {
                id: from.max(to),
                n: self.n,
            });
        }
        if from == to {
            return Ok(vec![from]);
        }
        let mut prev = vec![None; self.n];
        let mut visited = vec![false; self.n];
        let mut q = VecDeque::new();
        visited[from] = true;
        q.push_back(from);
        while let Some(u) = q.pop_front() {
            for v in self.neighbors(u) {
                if !visited[v] {
                    visited[v] = true;
                    prev[v] = Some(u);
                    if v == to {
                        // reconstruct
                        let mut path = vec![to];
                        let mut cur = to;
                        while let Some(p) = prev[cur] {
                            path.push(p);
                            cur = p;
                        }
                        path.reverse();
                        return Ok(path);
                    }
                    q.push_back(v);
                }
            }
        }
        Err(CayleyError::NoPath { from, to })
    }

    /// Diameter (max shortest path over all pairs). O(n^2) — use for small n.
    pub fn diameter(&self) -> usize {
        (0..self.n)
            .map(|s| {
                let mut dist = vec![usize::MAX; self.n];
                let mut q = VecDeque::new();
                dist[s] = 0;
                q.push_back(s);
                while let Some(u) = q.pop_front() {
                    for v in self.neighbors(u) {
                        if dist[v] == usize::MAX {
                            dist[v] = dist[u] + 1;
                            q.push_back(v);
                        }
                    }
                }
                *dist.iter().max().unwrap_or(&0)
            })
            .max()
            .unwrap_or(0)
    }

    /// Greedy routing by group distance: hop to the neighbor that minimizes the
    /// cyclic distance to the target. O(diameter * degree), no global state.
    pub fn greedy_route(&self, from: usize, to: usize) -> Result<Vec<usize>, CayleyError> {
        if from >= self.n || to >= self.n {
            return Err(CayleyError::OutOfRange {
                id: from.max(to),
                n: self.n,
            });
        }
        let mut path = vec![from];
        let mut cur = from;
        let mut steps = 0;
        let cap = self.n + 1;
        while cur != to && steps < cap {
            let best = self
                .neighbors(cur)
                .into_iter()
                .min_by_key(|&v| cyclic_dist(v, to, self.n))
                .unwrap();
            path.push(best);
            cur = best;
            steps += 1;
        }
        if cur == to {
            Ok(path)
        } else {
            Err(CayleyError::NoPath { from, to })
        }
    }
}

#[inline]
fn cyclic_dist(a: usize, b: usize, n: usize) -> usize {
    let d = (a + n - b) % n;
    d.min(n - d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connected_and_routes() {
        let g = CayleyGraph::fibonacci_jumps(128, 3);
        let p = g.shortest_path(0, 100).unwrap();
        assert_eq!(*p.last().unwrap(), 100);
        let d = g.diameter();
        assert!(d <= 8, "diameter {d} too large for n=128 degree 3");
    }
}
