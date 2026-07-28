//! rc-tokenize: token counting / estimation (§4.7).
//!
//! We do not have the provider's tokenizer. M6 wires the real strategy:
//!   1. Authoritative `usage` from the last response (cost, display, compaction
//!      trigger) — fed back via [`Estimator::observe`].
//!   2. A char-based proxy estimate for the *pending* request, plus a per-model
//!      calibration factor (EWMA) learned at runtime from the authoritative
//!      `usage`. (`tiktoken-rs` `o200k_base` is the §4.7 ideal; it requires
//!      rustc 1.85 and materially slows the workspace build, so M6 ships the
//!      calibrated proxy and leaves the tiktoken swap as a drop-in behind this
//!      seam — see `Estimator::estimate`.)
//!   3. A 10% safety margin on the compaction threshold ([`MARGIN`]).
//!
//! M0 shipped only a naive byte proxy so call sites could compile; M6 keeps it
//! as a public free function for tests and cheap non-decision estimates.

use std::sync::Mutex;

/// The compaction safety margin (§4.7): trigger compaction at 90% of the model's
/// context window so we never overshoot mid-request. Applied by callers to a
/// threshold they derive from model metadata.
pub const MARGIN: f64 = 0.90;

/// Default chars-per-token prior (matches the M0 byte proxy: ~4 chars/token).
/// The EWMA converges away from this once authoritative `usage` arrives.
const PRIOR_CHARS_PER_TOKEN: f64 = 4.0;

/// EWMA smoothing: `alpha = 0.3` weights recent observations more heavily than
/// the prior, so a single calibration sample doesn't fully override the
/// default but a steady stream does. (§4.7 — a per-model factor learned online.)
const ALPHA: f64 = 0.3;

/// A token estimator with a per-model EWMA calibration factor.
///
/// Cheap to clone (the inner is a `Mutex<f64>` behind a `std::sync::Arc`);
/// share one `Estimator` across a session and feed it every authoritative
/// `usage` from the model. Thread-safe.
#[derive(Debug, Clone)]
pub struct Estimator {
    /// The learned chars-per-token factor. Guarded so `observe` and `estimate`
    /// can race safely from parallel tool tasks.
    factor: std::sync::Arc<Mutex<f64>>,
}

impl Default for Estimator {
    fn default() -> Self {
        Self::new()
    }
}

impl Estimator {
    /// A fresh estimator starting from the default prior (~4 chars/token).
    pub fn new() -> Self {
        Self { factor: std::sync::Arc::new(Mutex::new(PRIOR_CHARS_PER_TOKEN)) }
    }

    /// The current chars-per-token factor (the EWMA). Exposed for display and
    /// tests; not part of any decision.
    pub fn factor(&self) -> f64 {
        *self.factor.lock().unwrap()
    }

    /// Estimate the token count of `text` using the current calibration.
    ///
    /// This is the proxy for the *pending* request (§4.7 #2): it is never
    /// trusted for cost or display — only for the compaction trigger, and
    /// only against a margin-adjusted threshold. Authoritative `usage` from
    /// [`Self::observe`] is what's displayed and billed.
    pub fn estimate(&self, text: &str) -> usize {
        let factor = self.factor();
        if factor <= 0.0 {
            return text.len().div_ceil(4);
        }
        (text.chars().count() as f64 / factor).ceil() as usize
    }

    /// Estimate the total tokens across an iterator of string slices (the
    /// projected wire messages). Sums per-message estimates; callers pass the
    /// rendered form of each message.
    pub fn estimate_iter<'a, I>(&self, messages: I) -> usize
    where
        I: IntoIterator<Item = &'a str>,
    {
        messages.into_iter().map(|m| self.estimate(m)).sum()
    }

    /// Calibrate against an authoritative `usage` from the model (§4.7 #1).
    ///
    /// `prompt_tokens` is the model's reported prompt token count for the last
    /// request; `estimated_chars` is the *char length* of the request body that
    /// produced it (what [`Self::estimate`] measured for that same request).
    /// Updates the EWMA: `factor = alpha * sample + (1 - alpha) * factor`.
    ///
    /// No-ops on a non-positive `prompt_tokens` (some providers omit it) or a
    /// zero `estimated_chars` (a degenerate request that would divide by zero).
    pub fn observe(&self, prompt_tokens: u64, estimated_chars: usize) {
        if prompt_tokens == 0 || estimated_chars == 0 {
            return;
        }
        let sample = estimated_chars as f64 / prompt_tokens as f64;
        if !sample.is_finite() || sample <= 0.0 {
            return;
        }
        let mut f = self.factor.lock().unwrap();
        *f = ALPHA * sample + (1.0 - ALPHA) * *f;
    }
}

/// Rough token estimate — ~4 chars/token. The M0 placeholder; kept for cheap,
/// non-decision estimates and tests that don't need calibration.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny float-equality helper so the tests read close to intent.
    fn approx_eq(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn default_factor_matches_prior() {
        approx_eq(Estimator::new().factor(), PRIOR_CHARS_PER_TOKEN);
    }

    #[test]
    fn estimate_uses_the_current_factor() {
        let e = Estimator::new();
        // 12 chars / 4 chars-per-token = 3 tokens.
        assert_eq!(e.estimate("hello world!"), 3);
    }

    #[test]
    fn observe_pulls_the_factor_toward_the_sample() {
        let e = Estimator::new();
        // The model reports 1000 tokens for 5000 chars -> 5 chars/token.
        e.observe(1000, 5000);
        // EWMA: 0.3 * 5.0 + 0.7 * 4.0 = 1.5 + 2.8 = 4.3
        approx_eq(e.factor(), 4.3);
    }

    #[test]
    fn observe_converges_under_a_steady_stream() {
        let e = Estimator::new();
        for _ in 0..50 {
            e.observe(1000, 5000); // steady 5.0 chars/token
        }
        // The EWMA converges toward 5.0; assert it's within 1%.
        assert!((e.factor() - 5.0).abs() < 0.05, "converged to {}", e.factor());
    }

    #[test]
    fn observe_ignores_degenerate_inputs() {
        let e = Estimator::new();
        let before = e.factor();
        e.observe(0, 5000);
        e.observe(1000, 0);
        assert_eq!(e.factor(), before, "degenerate samples must not move the EWMA");
    }

    #[test]
    fn estimate_iter_sums_per_message() {
        let e = Estimator::new();
        // "ab" (1) + "abcd" (1) + "abcdefgh" (2) = 4 tokens at 4 chars/token.
        assert_eq!(e.estimate_iter(["ab", "abcd", "abcdefgh"]), 4);
    }

    #[test]
    fn margin_is_90_percent() {
        approx_eq(MARGIN, 0.90);
    }

    #[test]
    fn legacy_estimate_tokens_still_works() {
        assert_eq!(estimate_tokens("hello world!"), 3);
    }
}
