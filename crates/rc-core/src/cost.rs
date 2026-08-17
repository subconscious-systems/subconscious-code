//! Integer micro-unit cost accounting — a genuine monoid homomorphism.
//!
//! Float addition is *not* associative, so a float cost accumulator is not a
//! monoid: a sharded or parallel reduction over it is order-dependent and
//! non-reproducible. Working in integer micro-USD restores `(ℕ, +)` and makes
//! cost aggregation a real monoid homomorphism — shard the reduction however
//! you like, merge the shards in any order, and you get the same number.
//!
//! Stating that as an invariant here (and implementing [`Monoid`]) is deliberate:
//! it's the kind of thing someone "optimizes" back into floats later, and once
//! they do, reproducibility of the accounting quietly breaks. The `as_usd()`
//! accessor is for *display only*; every accumulation stays in integers.

use rc_algebra::traits::Monoid;
use rc_proto::Usage;
use serde::{Deserialize, Serialize};

/// One million tokens — the unit prices are denominated in.
const PER_MILLION: u128 = 1_000_000;

/// A cost in integer micro-USD (10⁻⁶ USD). Aggregation is a true monoid:
/// `Cost::id()` is zero and `Cost::op` is saturating integer addition, which is
/// associative and commutative, so any sharded reduction converges to the same
/// total regardless of merge order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Cost(u64);

impl Cost {
    /// The identity element of the cost monoid.
    pub const ZERO: Self = Self(0);

    /// Build a cost from an integer micro-USD value.
    pub const fn from_micro_usd(micro_usd: u64) -> Self {
        Self(micro_usd)
    }

    /// The cost in micro-USD (the canonical integer unit).
    pub const fn as_micro_usd(&self) -> u64 {
        self.0
    }

    /// The cost in USD, for display only. Never used for aggregation.
    pub fn as_usd(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Accumulate `other` into `self` (saturating). This is the monoid `op`.
    pub fn add(&mut self, other: &Self) {
        self.0 = self.0.saturating_add(other.0);
    }
}

impl Monoid for Cost {
    fn id() -> Self {
        Self::ZERO
    }
    fn op(&mut self, other: &Self) {
        self.add(other);
    }
}

/// Token prices in integer micro-USD per *million* tokens. Integer per-million
/// pricing keeps `cost_of` a pure integer computation (u128 intermediate, then
/// truncated to micro-USD), so the running total is reproducible across shard
/// orderings.
///
/// `cached` is the cache-hit price for prompt tokens (typically a fraction of
/// `prompt`); the non-cached fraction of prompt tokens is billed at `prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Pricing {
    /// micro-USD per 1M non-cached prompt tokens.
    pub prompt: u64,
    /// micro-USD per 1M cached prompt tokens.
    pub cached: u64,
    /// micro-USD per 1M completion tokens.
    pub completion: u64,
}

impl Pricing {
    /// The no-op pricing: every cost computes to zero. The default for an
    /// unconfigured session, so cost accounting stays a zero-cost no-op until a
    /// real price sheet is supplied.
    pub const ZERO: Self = Self {
        prompt: 0,
        cached: 0,
        completion: 0,
    };

    /// The cost of a single response's [`Usage`], in micro-USD.
    ///
    /// `prompt_tokens` includes the cached subset; the cached fraction is
    /// billed at `cached` and the remainder at `prompt`, so cache hits reduce
    /// cost exactly as they reduce billed tokens. The computation is pure
    /// integer arithmetic (u128 intermediate) and is a homomorphism: summing
    /// `cost_of(u_i)` over a session equals `cost_of` of the summed usage, so
    /// the running total is independent of how/when the per-turn costs are
    /// combined.
    pub fn cost_of(&self, usage: &Usage) -> Cost {
        let cached = usage.cached_tokens().unwrap_or(0).min(usage.prompt_tokens);
        let prompt_billable = usage.prompt_tokens.saturating_sub(cached);

        let mut total: u128 = 0;
        total += prompt_billable as u128 * self.prompt as u128;
        total += cached as u128 * self.cached as u128;
        total += usage.completion_tokens as u128 * self.completion as u128;
        // Prices are per-million tokens; convert token·micro-USD into micro-USD.
        let micro_usd = (total / PER_MILLION).min(u64::MAX as u128) as u64;
        Cost::from_micro_usd(micro_usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::wire::PromptTokensDetails;

    fn usage(prompt: u64, completion: u64, cached: Option<u64>) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            prompt_tokens_details: cached.map(|c| PromptTokensDetails { cached_tokens: c }),
        }
    }

    #[test]
    fn zero_pricing_is_a_noop() {
        let p = Pricing::ZERO;
        assert_eq!(p.cost_of(&usage(1_000_000, 500_000, None)).as_micro_usd(), 0);
    }

    #[test]
    fn cost_of_matches_per_million_pricing() {
        // $3/M prompt, $0.30/M cached, $15/M completion.
        let p = Pricing {
            prompt: 3_000_000,
            cached: 300_000,
            completion: 15_000_000,
        };
        // 1M prompt (200k cached) + 100k completion.
        let u = usage(1_000_000, 100_000, Some(200_000));
        // $3/M prompt, $0.30/M cached, $15/M completion:
        //   billable prompt: 800k tokens × $3/M  = $2.40
        //   cached:          200k tokens × $0.30/M = $0.06
        //   completion:      100k tokens × $15/M  = $1.50
        //   total = $3.96 = 3,960,000 µUSD.
        assert_eq!(p.cost_of(&u).as_micro_usd(), 3_960_000);
        assert!((p.cost_of(&u).as_usd() - 3.96).abs() < 1e-9);
    }

    #[test]
    fn cost_is_a_monoid_homomorphism() {
        // The invariant: summing cost_of over the parts equals cost_of of the
        // summed usage (plus, not floats, so order-independent).
        let p = Pricing {
            prompt: 3_000_000,
            cached: 300_000,
            completion: 15_000_000,
        };
        let u1 = usage(500_000, 50_000, Some(100_000));
        let u2 = usage(500_000, 50_000, Some(100_000));

        let mut summed_usage = u1.clone();
        summed_usage.add(&u2);
        let whole = p.cost_of(&summed_usage);

        let mut parts = Cost::ZERO;
        parts.add(&p.cost_of(&u1));
        parts.add(&p.cost_of(&u2));
        assert_eq!(parts.as_micro_usd(), whole.as_micro_usd());
    }

    #[test]
    fn monoid_laws_hold() {
        let a = Cost::from_micro_usd(7);
        let b = Cost::from_micro_usd(11);
        let c = Cost::from_micro_usd(13);
        // identity
        let mut id_left = a;
        id_left.op(&Cost::id());
        assert_eq!(id_left, a);
        // associativity: (a + b) + c == a + (b + c)
        let mut lhs = a;
        lhs.op(&b);
        lhs.op(&c);
        let mut rhs = b;
        rhs.op(&c);
        let mut rhs2 = a;
        rhs2.op(&rhs);
        assert_eq!(lhs, rhs2);
        // commutativity (integer addition): a + b == b + a
        let mut ab = a;
        ab.op(&b);
        let mut ba = b;
        ba.op(&a);
        assert_eq!(ab, ba);
    }

    #[test]
    fn cost_saturates_instead_of_overflowing() {
        let mut big = Cost::from_micro_usd(u64::MAX);
        big.add(&Cost::from_micro_usd(1));
        assert_eq!(big.as_micro_usd(), u64::MAX);
    }
}
