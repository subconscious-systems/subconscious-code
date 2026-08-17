//! Cyclic-group traversal for benchmark variance reduction.
//!
//! Evaluating N tasks in one fixed order samples a single element of S_N; when
//! the benchmarked subject shares state between tasks (warm caches, JIT, OS
//! page cache, an agent that carries context forward), order effects are real
//! and a single fixed ordering biases the measurement.
//!
//! Random shuffling is the usual fix, but iid shuffling covers order-space
//! thinly per unit of compute. ℤ/n rotations systematically cover the orbit
//! instead: across n rotations every task occupies every position exactly
//! once, so position effects cancel in the per-task mean — antithetic-variates
//! flavour, deterministic, and reproducible (no RNG seed to pin).
//!
//! The Latin square is the same idea stated as a design: an n×n array in which
//! each symbol appears once per row and once per column. The addition table of
//! ℤ/n *is* a Latin square, so `cyclic_rotations(n)` is the canonical instance;
//! `is_latin_square` checks the invariant for any candidate design.

use std::process::Command;
use std::time::{Duration, Instant};

/// Cyclic group ℤ_n acting on `n` indices by rotation. The k-th rotation
/// (k = 0..n) is the shift `[k, k+1, …, n-1, 0, …, k-1]`.
///
/// Across the n rotations each task occupies each position exactly once: task
/// `i` lands at position `p` for the unique `k = (i - p) mod n`. That is the
/// Latin-square (addition-table) property, and it is what makes position
/// effects cancel in the per-task mean.
pub fn cyclic_rotations(n: usize) -> Vec<Vec<usize>> {
    (0..n)
        .map(|k| (0..n).map(|j| (k + j) % n).collect())
        .collect()
}

/// Take `k` of the `n` cyclic rotations, evenly stepped, when running all n
/// orderings is too expensive. With `k == n` this is `cyclic_rotations(n)`;
/// with `k == 1` it is the single fixed-order baseline to beat. Stepping
/// preserves approximate position balance; exact balance needs `k == n`.
pub fn rotations_subset(n: usize, k: usize) -> Vec<Vec<usize>> {
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let all = cyclic_rotations(n);
    let k = k.min(n);
    (0..k)
        .map(|r| all[r * n / k].clone())
        .collect()
}

/// Verify the Latin-square invariant: an n×n array in which each of `n`
/// symbols appears exactly once per row and once per column. `cyclic_rotations`
/// always satisfies this; the check is for custom designs.
pub fn is_latin_square(square: &[Vec<usize>]) -> bool {
    let n = square.len();
    if n == 0 || square.iter().any(|row| row.len() != n) {
        return false;
    }
    let mut seen = vec![false; n];
    // Rows.
    for row in square {
        seen.iter_mut().for_each(|s| *s = false);
        for &v in row {
            if v >= n || std::mem::replace(&mut seen[v], true) {
                return false;
            }
        }
    }
    // Columns.
    for col in 0..n {
        seen.iter_mut().for_each(|s| *s = false);
        for row in square {
            let v = row[col];
            if std::mem::replace(&mut seen[v], true) {
                return false;
            }
        }
    }
    true
}

/// A benchmark task: a human-readable label and a shell command to time.
pub struct BenchTask {
    pub label: String,
    pub command: String,
}

/// Per-task timing statistics across all evaluated orderings.
#[derive(Debug, Clone)]
pub struct TaskStats {
    pub label: String,
    /// Duration in each ordering, in evaluation order (one per rotation).
    pub samples: Vec<Duration>,
}

impl TaskStats {
    pub fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let sum: Duration = self.samples.iter().sum();
        sum / self.samples.len() as u32
    }

    /// Population standard deviation across orderings — the order-effect
    /// signal. A fixed single-order run has no variance to estimate; the whole
    /// point of rotating is to surface this number rather than hide it.
    pub fn stddev(&self) -> Duration {
        if self.samples.len() < 2 {
            return Duration::ZERO;
        }
        let mean = self.mean();
        let var: f64 = self
            .samples
            .iter()
            .map(|d| {
                let delta = d.as_nanos() as f64 - mean.as_nanos() as f64;
                delta * delta
            })
            .sum::<f64>()
            / self.samples.len() as f64;
        Duration::from_nanos(var.sqrt() as u64)
    }
}

/// Run `tasks` under each of `orderings` (each a permutation of task indices),
/// timing every task via `sh -c`. Returns one [`TaskStats`] per task, with a
/// sample per ordering. Orderings are validated to be permutations of
/// `0..tasks.len()`; an invalid design panics (it is a programmer error, not a
/// runtime one).
pub fn run_bench(tasks: &[BenchTask], orderings: &[Vec<usize>]) -> Vec<TaskStats> {
    let n = tasks.len();
    for order in orderings {
        assert!(
            order.len() == n && (0..n).all(|i| order.contains(&i)),
            "ordering is not a permutation of 0..{n}: {order:?}"
        );
    }
    let mut stats: Vec<TaskStats> = tasks
        .iter()
        .map(|t| TaskStats {
            label: t.label.clone(),
            samples: Vec::with_capacity(orderings.len()),
        })
        .collect();
    for order in orderings {
        for &idx in order {
            let start = Instant::now();
            let _ = Command::new("sh").arg("-c").arg(&tasks[idx].command).status();
            stats[idx].samples.push(start.elapsed());
        }
    }
    stats
}

/// Format a [`TaskStats`] table to a string: per-task mean, stddev, and the
/// fixed-order (rotation 0) sample for comparison.
pub fn format_report(stats: &[TaskStats]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<24} {:>10} {:>12} {:>12} {:>14}",
        "label", "rotations", "mean", "stddev", "fixed-order"
    );
    let _ = writeln!(out, "{}", "-".repeat(76));
    for s in stats {
        let fixed = s.samples.first().copied().unwrap_or(Duration::ZERO);
        let _ = writeln!(
            out,
            "{:<24} {:>10} {:>12.3?} {:>12.3?} {:>14.3?}",
            s.label,
            s.samples.len(),
            s.mean(),
            s.stddev(),
            fixed
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyclic_rotations_are_permutations() {
        let rots = cyclic_rotations(5);
        assert_eq!(rots.len(), 5);
        for r in &rots {
            assert_eq!(r.len(), 5);
            let mut sorted = r.clone();
            sorted.sort();
            assert_eq!(sorted, (0..5).collect::<Vec<_>>());
        }
    }

    #[test]
    fn cyclic_rotations_are_a_latin_square() {
        // The addition table of ℤ/n: each (task, position) pair appears once.
        let rots = cyclic_rotations(7);
        assert!(is_latin_square(&rots));
    }

    #[test]
    fn every_task_occupies_every_position_once() {
        let n = 6;
        let rots = cyclic_rotations(n);
        let mut seen = [[false; 6]; 6]; // seen[task][position]
        for (pos, order) in rots.iter().enumerate() {
            for (position_in_order, &task) in order.iter().enumerate() {
                assert!(
                    !std::mem::replace(&mut seen[task][position_in_order], true),
                    "task {task} at position {position_in_order} twice"
                );
            }
            let _ = pos;
        }
        // Every cell filled.
        assert!(seen.iter().flatten().all(|c| *c));
    }

    #[test]
    fn rotations_subset_n_equals_full() {
        let full = cyclic_rotations(4);
        let sub = rotations_subset(4, 4);
        assert_eq!(sub, full);
    }

    #[test]
    fn rotations_subset_one_is_first_rotation() {
        let sub = rotations_subset(5, 1);
        assert_eq!(sub, vec![vec![0, 1, 2, 3, 4]]);
    }

    #[test]
    fn rotations_subset_cannot_exceed_n() {
        let sub = rotations_subset(3, 10);
        assert_eq!(sub.len(), 3);
    }

    #[test]
    fn is_latin_square_rejects_non_square() {
        assert!(!is_latin_square(&[vec![0, 1], vec![0]]));
        assert!(!is_latin_square(&[vec![0, 1], vec![1, 1]])); // dup in row
        assert!(!is_latin_square(&[vec![0, 1], vec![0, 1]])); // dup in column
    }

    #[test]
    fn empty_rotations_handle_gracefully() {
        assert!(cyclic_rotations(0).is_empty());
        assert!(rotations_subset(0, 4).is_empty());
        assert!(rotations_subset(4, 0).is_empty());
    }
}
