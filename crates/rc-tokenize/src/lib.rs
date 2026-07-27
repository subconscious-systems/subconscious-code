//! rc-tokenize: token counting / estimation (§4.7).
//!
//! We do not have the provider's tokenizer. M6 wires the real strategy:
//!   1. Authoritative `usage` from the last response (cost, display, compaction
//!      trigger).
//!   2. `tiktoken-rs` (`o200k_base`) as a proxy estimate for the *pending*
//!      request, plus a per-model calibration factor (EWMA) learned at runtime.
//!   3. A 10% safety margin on the compaction threshold.
//!
//! M0 ships only a naive byte proxy so call sites can compile; it is not
//! trusted for any decision yet.

/// Rough token estimate — ~4 chars/token. Placeholder until tiktoken lands.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}
