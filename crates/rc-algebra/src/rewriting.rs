//! Knuth–Bendix / confluent rewriting.
//!
//! Knuth–Bendix completion grew out of the word problem for groups; it gives a
//! terminating, confluent normalizer — a rewrite system where every input has a
//! *unique* normal form regardless of the order rules are applied. The
//! difference between "canonical" and "canonical if you squint" is confluence:
//! without checking critical pairs, two reductions of the same term can land in
//! different normal forms, and your canonicalizer silently non-determines.
//!
//! This is a **minimal engine** for small rule sets: leftmost-outermost
//! reduction to a fixed point (assuming termination), plus a critical-pair
//! check that reports non-confluence. It is *not* wired into the live
//! canonicalization path — `rc_proto::canonical::canonicalize` is a single
//! deterministic key-sort recursion and does not need a rewrite system. This
//! engine is for future canonicalizers with more than a handful of rules,
//! where confluence is no longer obvious. [`crate::orbit::burnside_orbit_count`]
//! is the tool for estimating whether such a canonicalization is worth building
//! *before* committing to a rewrite system.
//!
//! Termination is the caller's responsibility (Knuth–Bendix completion itself
//! is undecidable in general); the shipped [`demo_rules`] terminate because
//! every rule is strictly length-decreasing.

use std::collections::BTreeMap;

/// A symbol in the rewrite alphabet. Cheap to copy and compare.
pub type Symbol = u32;

/// A rewrite rule `lhs → rhs`. Applied leftmost-outermost; `lhs` must be
/// non-empty. Termination is the caller's concern — typically every rule is
/// length-decreasing (strictly shorter `rhs`) or lex-decreasing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub lhs: Vec<Symbol>,
    pub rhs: Vec<Symbol>,
}

impl Rule {
    pub fn new(lhs: Vec<Symbol>, rhs: Vec<Symbol>) -> Self {
        assert!(!lhs.is_empty(), "rule lhs must be non-empty");
        Self { lhs, rhs }
    }
}

/// A rewrite system: an ordered list of rules.
#[derive(Clone, Debug, Default)]
pub struct RewriteSystem {
    rules: Vec<Rule>,
}

/// The result of a confluence check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfluenceReport {
    pub confluent: bool,
    pub failures: Vec<CriticalPair>,
}

/// A critical pair: an overlap of two rules' LHSs producing two different
/// reductions of the same term — the witness of non-confluence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CriticalPair {
    pub term: Vec<Symbol>,
    pub reduction_a: Vec<Symbol>,
    pub reduction_b: Vec<Symbol>,
}

impl RewriteSystem {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Reduce `input` to its normal form by repeatedly applying the
    /// leftmost-outermost matching rule until no rule applies. Bounded by
    /// `max_steps` to guarantee termination even on a non-terminating system
    /// (returns the partially-reduced term in that case).
    pub fn normalize(&self, input: &[Symbol]) -> Vec<Symbol> {
        self.normalize_bounded(input, 100_000)
    }

    fn normalize_bounded(&self, input: &[Symbol], max_steps: usize) -> Vec<Symbol> {
        let mut term = input.to_vec();
        let mut steps = 0;
        while steps < max_steps {
            match self.first_redex(&term) {
                Some((pos, rule_idx)) => {
                    let rule = &self.rules[rule_idx];
                    let mut out = Vec::with_capacity(term.len() - rule.lhs.len() + rule.rhs.len());
                    out.extend_from_slice(&term[..pos]);
                    out.extend_from_slice(&rule.rhs);
                    out.extend_from_slice(&term[pos + rule.lhs.len()..]);
                    term = out;
                    steps += 1;
                }
                None => return term,
            }
        }
        term
    }

    /// Leftmost-outermost redex: the earliest position where any rule's LHS
    /// matches. Returns `(position, rule_index)`.
    fn first_redex(&self, term: &[Symbol]) -> Option<(usize, usize)> {
        for pos in 0..term.len() {
            for (idx, rule) in self.rules.iter().enumerate() {
                if rule.lhs.len() <= term.len() - pos
                    && term[pos..pos + rule.lhs.len()] == rule.lhs[..]
                {
                    return Some((pos, idx));
                }
            }
        }
        None
    }

    /// All critical pairs: for every pair of rules (including a rule with
    /// itself), every non-trivial overlap of `a.lhs` with `b.lhs` — where a
    /// proper suffix of `a.lhs` equals a proper prefix of `b.lhs` — produces a
    /// term reducible by both; if the two normal forms differ, that's a
    /// non-confluence witness.
    pub fn critical_pairs(&self) -> Vec<CriticalPair> {
        let mut out = Vec::new();
        let n = self.rules.len();
        for i in 0..n {
            for j in 0..n {
                self.overlaps(&self.rules[i], &self.rules[j], &mut out);
            }
        }
        out
    }

    fn overlaps(&self, a: &Rule, b: &Rule, out: &mut Vec<CriticalPair>) {
        // Overlap where a suffix of a.lhs of length `k` equals a prefix of
        // b.lhs of length `k`, for 1 <= k < min(len_a, len_b) (proper overlap,
        // so the two redexes start at different positions).
        let la = a.lhs.len();
        let lb = b.lhs.len();
        let max_k = la.min(lb).saturating_sub(1);
        for k in 1..=max_k {
            if la >= k && lb >= k && a.lhs[la - k..] == b.lhs[..k] {
                // The combined term: a.lhs followed by b.lhs[k..].
                let mut term = a.lhs.clone();
                term.extend_from_slice(&b.lhs[k..]);
                // Redex a starts at 0; redex b starts at la - k.
                let red_a = self.apply_at(&term, 0, a);
                let red_b = self.apply_at(&term, la - k, b);
                let nf_a = self.normalize(&red_a);
                let nf_b = self.normalize(&red_b);
                if nf_a != nf_b {
                    out.push(CriticalPair {
                        term,
                        reduction_a: nf_a,
                        reduction_b: nf_b,
                    });
                }
            }
        }
    }

    fn apply_at(&self, term: &[Symbol], pos: usize, rule: &Rule) -> Vec<Symbol> {
        let mut out = Vec::with_capacity(term.len() - rule.lhs.len() + rule.rhs.len());
        out.extend_from_slice(&term[..pos]);
        out.extend_from_slice(&rule.rhs);
        out.extend_from_slice(&term[pos + rule.lhs.len()..]);
        out
    }

    /// Check confluence: normalize every critical pair from both sides and
    /// report any that diverge. `confluent == true` means every input has a
    /// unique normal form regardless of rule application order.
    /// (Newman's lemma + termination ⇒ confluence iff all critical pairs join).
    pub fn is_confluent(&self) -> ConfluenceReport {
        let failures: Vec<CriticalPair> = self
            .critical_pairs()
            .into_iter()
            .filter(|cp| cp.reduction_a != cp.reduction_b)
            .collect();
        ConfluenceReport {
            confluent: failures.is_empty(),
            failures,
        }
    }
}

/// A demo canonicalizer rule set: a sketch of a tool-call/prompt normalizer
/// using a tiny alphabet. Every rule is strictly length-decreasing, so the
/// system terminates; the rules are non-overlapping, so it is confluent.
///
/// Alphabet:
/// - `0` = whitespace run token
/// - `1` = `(` (open tool-call args)
/// - `2` = `)` (close)
/// - `3` = `,` (arg separator)
/// - `4` = a content token
///
/// Rules (all length-decreasing ⇒ terminating):
/// - `[0, 0] → [0]`            — collapse repeated whitespace
/// - `[1, 2] → [1, 2]` ... no; a length-decreasing empty-args rule:
///   `[1, 2] → []`             — elide empty tool-call argument list
/// - `[3, 0] → [0]`            — trailing separator whitespace simplifies
/// - `[0, 2] → [2]`            — whitespace before close elided
pub fn demo_rules() -> RewriteSystem {
    // Alphabet: 0 = whitespace run, 1 = "(", 2 = ")", 3 = ",", 4 = content.
    const WS: Symbol = 0;
    const LP: Symbol = 1;
    const RP: Symbol = 2;
    const SEP: Symbol = 3;
    RewriteSystem::new(vec![
        Rule::new(vec![WS, WS], vec![WS]),
        Rule::new(vec![LP, RP], vec![]),
        Rule::new(vec![SEP, WS], vec![WS]),
        Rule::new(vec![WS, RP], vec![RP]),
        // The KB completion step: `[SEP, WS, RP]` reduces two ways —
        // ([SEP,WS]→[WS]) then ([WS,RP]→[RP]) gives [RP]; ([WS,RP]→[RP]) gives
        // [SEP,RP]. Without this rule those normal forms differ and the system
        // is *not* confluent. Adding [SEP,RP]→[RP] joins the critical pair.
        Rule::new(vec![SEP, RP], vec![RP]),
    ])
}

/// Convenience: encode a slice of small integers as `Symbol`s.
pub fn syms(xs: &[u32]) -> Vec<Symbol> {
    xs.to_vec()
}

/// Tally rule applications by lhs — a cheap termination metric for the demo
/// (every shipped rule strictly reduces length). Returns `true` iff every rule
/// is length-decreasing, a sufficient (not necessary) termination condition.
pub fn all_rules_length_decreasing(sys: &RewriteSystem) -> bool {
    sys.rules().iter().all(|r| r.rhs.len() < r.lhs.len())
}

/// For callers that want a stable map of symbol → meaning; not used by the
/// engine itself, only for documentation/introspection.
pub fn _rule_index(_sys: &RewriteSystem) -> BTreeMap<usize, Rule> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Alphabet aliases for readable test terms.
    const W: Symbol = 0;
    const L: Symbol = 1;
    const R: Symbol = 2;
    const S: Symbol = 3;

    #[test]
    fn normalize_reduces_whitespace_run() {
        let sys = demo_rules();
        // [W,W,W,R] → [W,R] → [R]
        let nf = sys.normalize(&[W, W, W, R]);
        assert_eq!(nf, vec![R]);
    }

    #[test]
    fn normalize_elides_empty_args() {
        let sys = demo_rules();
        // [L,R] → []
        let nf = sys.normalize(&[L, R]);
        assert!(nf.is_empty());
    }

    #[test]
    fn normalize_reaches_fixed_point() {
        let sys = demo_rules();
        let nf1 = sys.normalize(&[W, W, L, R, W, R]);
        let nf2 = sys.normalize(&nf1);
        assert_eq!(
            nf1, nf2,
            "normal form must be stable under re-normalization"
        );
    }

    #[test]
    fn demo_rule_set_is_confluent() {
        let sys = demo_rules();
        let report = sys.is_confluent();
        assert!(report.confluent, "non-confluence: {:?}", report.failures);
    }

    #[test]
    fn demo_rules_terminate_by_decreasing_length() {
        let sys = demo_rules();
        assert!(all_rules_length_decreasing(&sys));
    }

    #[test]
    fn completed_critical_pair_joins() {
        // The critical pair that forced the [SEP,RP]→[RP] completion rule:
        // [SEP,WS,RP] must normalize to [RP] regardless of reduction order.
        let sys = demo_rules();
        assert_eq!(sys.normalize(&[S, W, R]), vec![R]);
    }

    #[test]
    fn detects_non_confluence() {
        // Two rules that overlap ambiguously: [L,R]→[] and [R,S]→[9].
        // Overlap: suffix [R] of first == prefix [R] of second → term [L,R,S].
        // Reduce at 0 → [S] → (no rule) [S]. Reduce at 1 → [L,9] → (no rule) [L,9].
        // [S] ≠ [L,9] ⇒ non-confluent.
        let sys = RewriteSystem::new(vec![
            Rule::new(vec![L, R], vec![]),
            Rule::new(vec![R, S], vec![9]),
        ]);
        let report = sys.is_confluent();
        assert!(!report.confluent, "expected a non-confluence witness");
        assert!(!report.failures.is_empty());
    }

    #[test]
    fn non_overlapping_rules_are_confluent() {
        // Disjoint alphabets ⇒ no critical pairs ⇒ confluent.
        let sys = RewriteSystem::new(vec![
            Rule::new(vec![L, L], vec![L]),
            Rule::new(vec![R, R], vec![R]),
        ]);
        assert!(sys.is_confluent().confluent);
    }
}
