//! Staged pipeline (extra strategy).
//!
//! The append path is three stages of very different character:
//!   1. **compress** — CPU bound (zstd-dict)
//!   2. **code**      — CPU bound (fountain/RLNC, GF(256))
//!   3. **send**      — network bound (QUIC/RDMA)
//!
//! Running them sequentially serializes three different bottlenecks: the
//! network idles while the CPU compresses, the CPU idles while the network
//! sends. A **staged pipeline** runs each stage on its own thread connected by
//! bounded channels, so stage 1 compresses turn N+1 while stage 3 sends turn
//! N. Steady-state throughput rises to `min(stage throughput)` instead of
//! `sum(stage latency)` — a concurrency multiplier bounded by the slowest
//! stage, which on the append path is usually the network.
//!
//! Bounded `sync_channel` gives backpressure for free: if the network stalls,
//! the channel fills and the compress stage blocks instead of ballooning
//! memory. This is the application-layer pipeline the doc's "don't reinvent L4"
//! leaves room for — we own the staging, the transport substrate owns the wire.
//!
//! Worker threads are **persistent across `run` calls**: the per-stage threads
//! are spawned once (lazily, on the first `run`) and reused for every batch, so
//! a pipeline that fires once per turn doesn't pay `thread::spawn` + `join` for
//! every stage on every turn. Between batches the stage threads simply block
//! on `recv`, idle and ready. (#29)

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

/// The persistent stage threads + the channel ends a `run` feeds and drains.
/// Built once, reused per batch. Fields drop in declaration order — `head`
/// and `tail` drop first, closing the channels so the stage threads exit, then
/// `_handles` joins them.
struct Workers {
    head: SyncSender<Vec<u8>>,
    tail: Receiver<Vec<u8>>,
    _handles: Vec<JoinHandle<()>>,
}

/// A byte-buffer pipeline: each stage maps `Vec<u8>` -> `Vec<u8>` and runs on
/// its own thread. Concrete (compress/code all operate on byte buffers) rather
/// than fully generic, to keep the ownership simple and the dependency list
/// empty.
pub struct BytePipeline {
    #[allow(clippy::type_complexity)]
    stages: parking_lot::Mutex<Vec<Box<dyn Fn(Vec<u8>) -> Vec<u8> + Send + 'static>>>,
    cap: usize,
    workers: OnceLock<Workers>,
}

impl BytePipeline {
    /// `cap` is the per-stage channel bound (backpressure). 8–32 is typical.
    pub fn new(cap: usize) -> Self {
        Self {
            stages: parking_lot::Mutex::new(Vec::new()),
            cap,
            workers: OnceLock::new(),
        }
    }

    /// Append a stage. Stages run in insertion order.
    pub fn stage<F>(self, f: F) -> Self
    where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + 'static,
    {
        self.stages.lock().push(Box::new(f));
        self
    }

    /// Spawn the persistent stage threads and return the channel ends. Takes
    /// the stage closures out of `self.stages` (drained, so this only runs once).
    fn spawn(&self) -> Workers {
        let stages = std::mem::take(&mut *self.stages.lock());
        let (head, mut prev_rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = sync_channel(self.cap);
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(stages.len());

        for stage in stages {
            let (next_tx, next_rx) = sync_channel::<Vec<u8>>(self.cap);
            let rx = prev_rx;
            handles.push(thread::spawn(move || {
                while let Ok(item) = rx.recv() {
                    let out = stage(item);
                    if next_tx.send(out).is_err() {
                        break;
                    }
                }
            }));
            prev_rx = next_rx;
        }

        Workers {
            head,
            tail: prev_rx,
            _handles: handles,
        }
    }

    /// Feed `input` through all stages and collect the results in order. The
    /// feeder preserves order by sending sequentially; each stage preserves
    /// order because it has a single thread and a FIFO channel. Stage threads
    /// persist across calls, so repeated `run`s reuse the same workers.
    ///
    /// A short-lived feeder thread sends the batch concurrently with the
    /// collector draining the tail, so backpressure (a full head channel while
    /// we collect) can't deadlock the pipeline for batches larger than the
    /// channel capacity — the collector keeps draining and frees the stages.
    pub fn run(&self, input: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let n = input.len();
        if n == 0 {
            return Vec::new();
        }
        let workers = self.workers.get_or_init(|| self.spawn());

        let head = workers.head.clone();
        let feed = thread::spawn(move || {
            for item in input {
                if head.send(item).is_err() {
                    break;
                }
            }
        });

        // Stages are 1:1 and FIFO, so exactly `n` items emerge at the tail.
        let mut out = Vec::with_capacity(n);
        while let Ok(item) = workers.tail.recv() {
            out.push(item);
            if out.len() == n {
                break;
            }
        }
        let _ = feed.join();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_runs_stages_in_order() {
        let p = BytePipeline::new(4)
            .stage(|b| b.iter().map(|x| x + 1).collect())
            .stage(|b| b.iter().map(|x| x * 2).collect());
        let out = p.run(vec![vec![1, 2, 3], vec![10]]);
        // (x+1)*2
        assert_eq!(out, vec![vec![4, 6, 8], vec![22]]);
    }

    #[test]
    fn pipeline_reuses_workers_across_runs() {
        // Two batches through the same pipeline: order must be preserved and
        // the second batch must work without re-spawning the stage threads.
        let p = BytePipeline::new(2)
            .stage(|b| b.iter().map(|x| x + 1).collect())
            .stage(|b| b.iter().map(|x| x * 2).collect());
        let a = p.run(vec![vec![1, 2], vec![3]]);
        let b = p.run(vec![vec![5], vec![6, 7]]);
        assert_eq!(a, vec![vec![4, 6], vec![8]]);
        assert_eq!(b, vec![vec![12], vec![14, 16]]);
    }

    #[test]
    fn pipeline_handles_batch_larger_than_channel_cap() {
        // 32 items through a cap-2 pipeline with 3 stages would deadlock a
        // feed-then-collect design; the concurrent feeder/collector must not.
        let p = BytePipeline::new(2)
            .stage(|b| b.iter().map(|x| x.wrapping_add(1)).collect())
            .stage(|b| b.iter().map(|x| x.wrapping_mul(3)).collect())
            .stage(|b| b.iter().map(|x| x.wrapping_sub(2)).collect());
        let input: Vec<Vec<u8>> = (0u8..32).map(|i| vec![i]).collect();
        let out = p.run(input.clone());
        // (x+1)*3 - 2 = 3x+1
        assert_eq!(out, (0u8..32).map(|i| vec![i * 3 + 1]).collect::<Vec<_>>());
    }
}
