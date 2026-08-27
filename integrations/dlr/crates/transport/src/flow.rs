//! Credit-based flow control for the append-log stream (extra strategy).
//!
//! DESIGN §0: do not reinvent L4 — QUIC already provides congestion control and
//! a stream-level flow-credit window. This module is the *application-layer*
//! complement for the bespoke/RDMA paths where you don't get a stream credit
//! window for free, and for the shim→receiver hop where backpressure must
//! express *application* buffers (the content-addressed store + the async
//! prune), not just transport buffers.
//!
//! Model: the receiver grants the sender a window of credits (in bytes). The
//! sender consumes one credit per byte shipped; when the window is exhausted
//! it blocks (backpressure) until the receiver grants more. This bounds the
//! in-flight bytes to ≤ the window, so a slow prune or a burst of large turns
//! cannot grow unbounded buffers and collapse latency — the sender is simply
//! held. The window is sized to the **BDP** (bandwidth × RTprop) so the pipe
//! stays full without overfilling, exactly as a well-tuned credit window
//! should. Composes with [`crate::BbrModel`]: when the BBR estimate moves, the
//! credit window is re-aimed at the new BDP.
//!
//! All operations are lock-free atomics; `take` does not spin — it reports
//! remaining credit and the caller decides to block/retry/yield.

use std::sync::atomic::{AtomicU64, Ordering};

/// A lock-free credit window. `available` is the bytes the sender may still
/// ship before it must wait for a grant. `capacity` is the current window size
/// (aimed at the BDP); the receiver may grow it.
pub struct CreditFlow {
    available: AtomicU64,
    capacity: AtomicU64,
}

impl CreditFlow {
    /// New flow controller with an initial window (typically the estimated
    /// BDP = bandwidth × RTprop).
    pub fn new(window_bytes: u64) -> Self {
        Self {
            available: AtomicU64::new(window_bytes),
            capacity: AtomicU64::new(window_bytes),
        }
    }

    /// Receiver grants `n` more bytes of credit (e.g. after it has drained its
    /// store/prune backlog and can accept more). Saturating at `capacity` is
    /// intentional: a window larger than the BDP buys nothing and risks
    /// overfilling the receiver.
    pub fn grant(&self, n: u64) {
        let cap = self.capacity.load(Ordering::Relaxed);
        let mut a = self.available.load(Ordering::Relaxed);
        loop {
            let nxt = a.saturating_add(n).min(cap);
            match self
                .available
                .compare_exchange_weak(a, nxt, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(cur) => a = cur,
            }
        }
    }

    /// Sender attempts to consume `n` bytes of credit. Returns the number of
    /// bytes actually consumed (≤ `n`): if `available >= n`, consumes `n`;
    /// otherwise consumes whatever remains (possibly 0) and the caller blocks.
    /// Never oversells: the CAS only succeeds if it can decrement by the
    /// claimed amount without underflow.
    pub fn take(&self, n: u64) -> u64 {
        let mut a = self.available.load(Ordering::Relaxed);
        loop {
            if a == 0 {
                return 0;
            }
            let give = n.min(a);
            match self.available.compare_exchange_weak(
                a,
                a - give,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return give,
                Err(cur) => a = cur,
            }
        }
    }

    /// Current available credit.
    pub fn available(&self) -> u64 {
        self.available.load(Ordering::Relaxed)
    }

    /// Current window capacity.
    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    /// Re-aim the window at a new BDP estimate (e.g. when the BBR model
    /// revises bandwidth or RTprop). Growing takes effect immediately; the
    /// extra credit is added to `available`. Shrinking only caps future
    /// grants — in-flight credit already extended is honored (no recall),
    /// so the window ratchets down gracefully as outstanding bytes drain.
    pub fn reaim(&self, new_bdp: u64) {
        let old = self.capacity.swap(new_bdp, Ordering::Relaxed);
        if new_bdp > old {
            // extend credit by the growth
            self.grant(new_bdp - old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_and_grant() {
        let f = CreditFlow::new(1000);
        assert_eq!(f.take(400), 400);
        assert_eq!(f.take(700), 600); // only 600 remain
        assert_eq!(f.available(), 0);
        f.grant(500);
        assert_eq!(f.available(), 500);
        assert_eq!(f.take(500), 500);
    }

    #[test]
    fn reaim_grows_credit() {
        let f = CreditFlow::new(1000);
        assert_eq!(f.take(1000), 1000);
        f.reaim(2000);
        assert_eq!(f.available(), 1000); // grew by 1000
    }
}
