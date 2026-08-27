//! Fibonacci backoff / congestion law (DESIGN §6.8 — honest caveat).
//!
//! You *can* grow retransmit intervals or the congestion window on the Fibonacci
//! sequence (1,1,2,3,5,8,...) as a middle ground between additive-increase and
//! multiplicative-increase — gentler than exponential, still bounded. Elegant and
//! harmless. **But:** congestion-control quality is dominated by the *signal*
//! (loss vs. delay vs. ECN vs. BBR-style bandwidth estimation), not the number
//! sequence of the increase law. Treat this as a tunable, not a headline feature.
//! Per DESIGN §0, congestion control belongs to the reused transport (QUIC);
//! this is only used on bespoke (RDMA) paths where we do not get QUIC's BBR for
//! free.

/// A Fibonacci-sequence congestion window / backoff controller.
#[derive(Debug, Clone)]
pub struct FibBackoff {
    window: u64,
    fib_a: u64,
    fib_b: u64,
    max: u64,
    /// Multiplicative-decrease factor on loss (gentler than halving).
    md_factor: f64,
}

impl FibBackoff {
    pub fn new(initial: u64, max: u64) -> Self {
        Self {
            window: initial,
            fib_a: 1,
            fib_b: 1,
            max,
            md_factor: 0.5,
        }
    }

    /// Current congestion window (bytes or packets, caller's unit).
    pub fn window(&self) -> u64 {
        self.window
    }

    /// On a successful ack: Fibonacci additive increase (window grows by the
    /// next Fibonacci increment, gentler than multiplicative).
    pub fn on_success(&mut self) {
        let inc = self.fib_b;
        self.window = self.window.saturating_add(inc).min(self.max);
        let next = self.fib_a.saturating_add(self.fib_b);
        self.fib_a = self.fib_b;
        self.fib_b = next;
    }

    /// On loss: multiplicative decrease. Resets the Fibonacci incrementer so
    /// growth restarts gently from 1,1.
    pub fn on_loss(&mut self) {
        self.window = ((self.window as f64) * self.md_factor) as u64;
        self.fib_a = 1;
        self.fib_b = 1;
    }

    /// Set the multiplicative-decrease factor (default 0.5).
    pub fn set_md(&mut self, f: f64) {
        self.md_factor = f.clamp(0.01, 1.0);
    }

    /// On a timeout: collapse the window to the initial MSS and restart Fibonacci.
    pub fn on_timeout(&mut self, initial: u64) {
        self.window = initial;
        self.fib_a = 1;
        self.fib_b = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gentler_than_exponential() {
        let mut b = FibBackoff::new(10, 1_000_000);
        for _ in 0..10 {
            b.on_success();
        }
        // window grew along Fibonacci increments, bounded
        assert!(b.window() < 500, "fib window bounded {}", b.window());
    }
}
