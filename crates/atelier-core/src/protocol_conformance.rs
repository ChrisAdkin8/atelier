//! §2 conformance tracker — per-turn re-prompt counter + cross-call ring
//! buffer for the §1 `Adapter::conformance()` window.
//!
//! Spec §2 "Conformance enforcement":
//!   Malformed envelope → re-prompt with validation error inline. After **3
//!   consecutive failures** in a turn, **downshift strategy** and re-run the
//!   turn. Persistent failure surfaces a model-quality warning.
//!
//! Spec §1 "Conformance interface" (cross-referenced):
//!   Bounded **100-call ring buffer** of structured-output successes /
//!   failures so the adapter can answer `conformance()` without re-counting
//!   from scratch.
//!
//! The two scopes are distinct:
//!
//!   * [`TurnConformance`] — *per turn*. Resets when a new turn starts.
//!     Decides "should we re-prompt or downshift?".
//!   * [`ConformanceRingBuffer`] — *cross-call, 100-deep*. The adapter
//!     surfaces this to the BYOM trait so the trust-budget and routing
//!     subsystems can see "this model has been at 12% structured-output
//!     conformance for the last 100 calls."
//!
//! Both PROVISIONAL constants are exported so calibration (spec §1 Q1) can
//! land in one place.

use std::collections::VecDeque;

use crate::protocol_strategy::Strategy;

/// PROVISIONAL — spec §2. Consecutive in-turn failures before downshift.
pub const TURN_FAILURE_BUDGET: usize = 3;

/// PROVISIONAL — spec §1. Ring-buffer depth for `Adapter::conformance()`.
pub const CONFORMANCE_WINDOW: usize = 100;

/// PROVISIONAL — spec §1 / §2 cross-call degradation window. The runner
/// degrades the active strategy when at least
/// [`DEFAULT_DEGRADATION_THRESHOLD`] of the last
/// [`DEFAULT_DEGRADATION_WINDOW`] envelope-parse outcomes were
/// malformed. Tracked separately from the per-turn [`TURN_FAILURE_BUDGET`]
/// (which fires after consecutive failures *within one turn*); this
/// window covers the slower-burning case of a model that intermittently
/// produces malformed envelopes across many turns. The 3-of-20 default
/// is a placeholder pending the canonical-workload calibration row in
/// `tasks/todo.md`.
pub const DEFAULT_DEGRADATION_WINDOW: usize = 20;

/// Companion to [`DEFAULT_DEGRADATION_WINDOW`]. Number of malformed
/// envelope-parse outcomes in the window that triggers a one-way
/// downshift. See [`ConformanceRingBuffer::should_degrade`].
pub const DEFAULT_DEGRADATION_THRESHOLD: u32 = 3;

/// Per-turn tracker. One instance is created at the start of each turn and
/// driven by the BYOM adapter's envelope-parse loop:
///
///   * On a clean parse, call [`TurnConformance::on_success`]; the counter
///     resets.
///   * On a parse failure, call [`TurnConformance::on_failure`] with the
///     validation error message. The tracker either asks for a re-prompt
///     (returning the message verbatim for inclusion) or for a downshift
///     (returning the next [`Strategy`]). On no-lower-strategy, returns
///     [`TurnDecision::EscalateToUser`].
#[derive(Debug, Clone)]
pub struct TurnConformance {
    strategy: Strategy,
    consecutive_failures: usize,
    budget: usize,
}

impl TurnConformance {
    /// Start a turn at the given strategy. Budget defaults to spec §2's
    /// PROVISIONAL value; tests / calibration can override.
    pub fn new(initial: Strategy) -> Self {
        Self::with_budget(initial, TURN_FAILURE_BUDGET)
    }

    pub fn with_budget(initial: Strategy, budget: usize) -> Self {
        Self {
            strategy: initial,
            consecutive_failures: 0,
            budget,
        }
    }

    pub fn current_strategy(&self) -> Strategy {
        self.strategy
    }

    pub fn consecutive_failures(&self) -> usize {
        self.consecutive_failures
    }

    /// Envelope parsed cleanly — reset the in-turn failure counter.
    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Envelope failed to parse. Returns the harness's decision: re-prompt
    /// (with the validation error embedded), downshift to a weaker
    /// strategy, or escalate when even the weakest strategy can't get a
    /// clean envelope. `validation_error` is forwarded verbatim into the
    /// re-prompt payload.
    pub fn on_failure(&mut self, validation_error: String) -> TurnDecision {
        self.consecutive_failures += 1;
        if self.consecutive_failures < self.budget {
            return TurnDecision::Reprompt {
                attempt: self.consecutive_failures,
                budget: self.budget,
                validation_error,
            };
        }
        // Budget exhausted at the current strategy.
        match self.strategy.downshift() {
            Some(next) => {
                let previous = self.strategy;
                self.strategy = next;
                self.consecutive_failures = 0;
                TurnDecision::Downshift {
                    from: previous,
                    to: next,
                }
            }
            None => TurnDecision::EscalateToUser {
                at_strategy: self.strategy,
            },
        }
    }
}

/// Harness response after each model envelope attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnDecision {
    /// Within budget — re-prompt the model with the validation error.
    /// `attempt` is 1-based; `budget` is `TURN_FAILURE_BUDGET` (or the
    /// per-instance override).
    Reprompt {
        attempt: usize,
        budget: usize,
        validation_error: String,
    },
    /// Budget exhausted but a weaker strategy is still available. The turn
    /// re-runs from the start with the new strategy.
    Downshift { from: Strategy, to: Strategy },
    /// Already at the weakest strategy (`regex_prose`) and still failing —
    /// surface the model-quality warning to the user (spec §2 "Persistent
    /// failure surfaces a model-quality warning").
    EscalateToUser { at_strategy: Strategy },
}

/// Cross-call ring buffer of envelope-parse outcomes. The §1 `Adapter`
/// trait's `conformance()` returns a snapshot from this buffer — see
/// `Adapter::conformance() -> bounded 100-call ring buffer` in
/// `tasks/todo.md`.
#[derive(Debug, Clone)]
pub struct ConformanceRingBuffer {
    capacity: usize,
    samples: VecDeque<Sample>,
}

/// Per-call outcome stored in the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub strategy: Strategy,
    pub ok: bool,
}

/// Snapshot exposed to callers — `successes / total` and per-strategy
/// breakdown. The trust budget and routing subsystems read this; they never
/// mutate the buffer directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceSnapshot {
    pub total: usize,
    pub successes: usize,
    pub failures: usize,
    /// (strategy, total_at_strategy, successes_at_strategy)
    pub by_strategy: Vec<(Strategy, usize, usize)>,
}

impl ConformanceSnapshot {
    /// Success rate over the window, or `None` when the buffer is empty.
    ///
    /// The empty-buffer case is genuinely *no evidence* — neither "the
    /// adapter is healthy" nor "the adapter is broken". Returning `None`
    /// forces callers to choose explicitly between rubber-stamping
    /// (`unwrap_or(1.0) >= threshold`) and waiting for evidence
    /// (`map_or(false, |r| r >= threshold)`); the former is the
    /// fail-open trap the deep-scan flagged.
    ///
    /// `#[must_use]` because dropping the result on the floor is almost
    /// always a bug — `snapshot.rate();` does nothing useful, and a
    /// stray `unwrap_or(1.0)` after a refactor would silently rubber-stamp
    /// the threshold check.
    #[must_use]
    pub fn rate(&self) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        Some(self.successes as f32 / self.total as f32)
    }

    /// Whether the buffer has recorded any samples yet. Cheap predicate
    /// for the common "skip the threshold check until we have data" pattern.
    pub fn has_evidence(&self) -> bool {
        self.total > 0
    }
}

impl ConformanceRingBuffer {
    pub fn new() -> Self {
        Self::with_capacity(CONFORMANCE_WINDOW)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            samples: VecDeque::with_capacity(capacity),
        }
    }

    /// Record one envelope-parse outcome. Drops the oldest sample once at
    /// capacity (FIFO).
    pub fn record(&mut self, sample: Sample) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Convenience: record success.
    pub fn record_success(&mut self, strategy: Strategy) {
        self.record(Sample { strategy, ok: true });
    }

    /// Convenience: record failure.
    pub fn record_failure(&mut self, strategy: Strategy) {
        self.record(Sample {
            strategy,
            ok: false,
        });
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Has the rolling window crossed the §1/§2 degradation threshold?
    /// Returns `true` when the buffer holds **at least**
    /// [`DEFAULT_DEGRADATION_WINDOW`] samples *and* **at least**
    /// [`DEFAULT_DEGRADATION_THRESHOLD`] of them are failures. Empty or
    /// not-yet-full buffers return `false` so the runner doesn't degrade
    /// on a handful of early-session noise. Asymmetric on purpose: the
    /// "lots of evidence, lots of failures" precondition is the v1
    /// signal; calibration (`tasks/todo.md` PROVISIONAL row) can tune
    /// both knobs together.
    ///
    /// One-way: the runner calls this each turn and acts on a positive
    /// result by downshifting the active strategy. There is no
    /// auto-promote helper here — promotion would require evidence
    /// against the new strategy (which is what the buffer carries) but
    /// the spec calls for the degradation to be sticky for the session.
    pub fn should_degrade(&self) -> bool {
        self.should_degrade_with(DEFAULT_DEGRADATION_WINDOW, DEFAULT_DEGRADATION_THRESHOLD)
    }

    /// Custom-threshold variant of [`Self::should_degrade`]. Exposed for
    /// integration tests that want to dial the window down to a handful
    /// of samples without monkeypatching the global constant.
    pub fn should_degrade_with(&self, window: usize, threshold: u32) -> bool {
        if self.samples.len() < window {
            return false;
        }
        let recent = self
            .samples
            .iter()
            .rev()
            .take(window)
            .filter(|s| !s.ok)
            .count();
        recent as u32 >= threshold
    }

    /// Build an immutable snapshot. Constructing the per-strategy breakdown
    /// is O(N) over the buffer; callers typically take a snapshot once per
    /// turn end, not per sample.
    pub fn snapshot(&self) -> ConformanceSnapshot {
        let total = self.samples.len();
        let successes = self.samples.iter().filter(|s| s.ok).count();
        let failures = total - successes;

        let mut by_strategy: Vec<(Strategy, usize, usize)> = Vec::new();
        for strategy in [
            Strategy::NativeTool,
            Strategy::JsonSentinel,
            Strategy::RegexProse,
        ] {
            let at = self.samples.iter().filter(|s| s.strategy == strategy);
            let t = at.clone().count();
            let s = at.filter(|s| s.ok).count();
            if t > 0 {
                by_strategy.push((strategy, t, s));
            }
        }

        ConformanceSnapshot {
            total,
            successes,
            failures,
            by_strategy,
        }
    }
}

impl Default for ConformanceRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- TurnConformance ----------

    #[test]
    fn turn_starts_at_specified_strategy_with_zero_failures() {
        let t = TurnConformance::new(Strategy::NativeTool);
        assert_eq!(t.current_strategy(), Strategy::NativeTool);
        assert_eq!(t.consecutive_failures(), 0);
    }

    #[test]
    fn success_resets_the_failure_counter() {
        let mut t = TurnConformance::new(Strategy::NativeTool);
        t.on_failure("e1".into());
        t.on_failure("e2".into());
        assert_eq!(t.consecutive_failures(), 2);
        t.on_success();
        assert_eq!(t.consecutive_failures(), 0);
    }

    #[test]
    fn first_two_failures_reprompt_with_error_embedded() {
        let mut t = TurnConformance::new(Strategy::NativeTool);
        match t.on_failure("missing claimed_changes".into()) {
            TurnDecision::Reprompt {
                attempt,
                budget,
                validation_error,
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(budget, TURN_FAILURE_BUDGET);
                assert_eq!(validation_error, "missing claimed_changes");
            }
            other => panic!("expected Reprompt, got {other:?}"),
        }
        match t.on_failure("still missing".into()) {
            TurnDecision::Reprompt { attempt, .. } => assert_eq!(attempt, 2),
            other => panic!("expected Reprompt, got {other:?}"),
        }
    }

    #[test]
    fn third_failure_at_native_downshifts_to_sentinel_and_resets_count() {
        let mut t = TurnConformance::new(Strategy::NativeTool);
        t.on_failure("a".into());
        t.on_failure("b".into());
        match t.on_failure("c".into()) {
            TurnDecision::Downshift { from, to } => {
                assert_eq!(from, Strategy::NativeTool);
                assert_eq!(to, Strategy::JsonSentinel);
            }
            other => panic!("expected Downshift, got {other:?}"),
        }
        assert_eq!(t.current_strategy(), Strategy::JsonSentinel);
        assert_eq!(t.consecutive_failures(), 0);
    }

    #[test]
    fn full_downshift_chain_to_regex_prose() {
        let mut t = TurnConformance::new(Strategy::NativeTool);
        for _ in 0..3 {
            t.on_failure("x".into());
        }
        assert_eq!(t.current_strategy(), Strategy::JsonSentinel);
        for _ in 0..3 {
            t.on_failure("x".into());
        }
        assert_eq!(t.current_strategy(), Strategy::RegexProse);
    }

    #[test]
    fn failure_at_regex_prose_escalates_to_user() {
        let mut t = TurnConformance::new(Strategy::RegexProse);
        t.on_failure("a".into());
        t.on_failure("b".into());
        match t.on_failure("c".into()) {
            TurnDecision::EscalateToUser { at_strategy } => {
                assert_eq!(at_strategy, Strategy::RegexProse)
            }
            other => panic!("expected EscalateToUser, got {other:?}"),
        }
        // Strategy stays put — there's nothing weaker to downshift to.
        assert_eq!(t.current_strategy(), Strategy::RegexProse);
    }

    #[test]
    fn custom_budget_is_honored() {
        let mut t = TurnConformance::with_budget(Strategy::NativeTool, 2);
        // First failure: reprompt at attempt 1.
        assert!(matches!(
            t.on_failure("a".into()),
            TurnDecision::Reprompt { attempt: 1, .. }
        ));
        // Second failure: budget = 2, so this triggers downshift.
        assert!(matches!(
            t.on_failure("b".into()),
            TurnDecision::Downshift { .. }
        ));
    }

    #[test]
    fn intermittent_success_prevents_downshift() {
        let mut t = TurnConformance::new(Strategy::NativeTool);
        for _ in 0..50 {
            t.on_failure("err".into());
            t.on_success();
        }
        assert_eq!(t.current_strategy(), Strategy::NativeTool);
    }

    // ---------- ConformanceRingBuffer ----------

    #[test]
    fn empty_buffer_reports_no_evidence_not_perfect_rate() {
        let b = ConformanceRingBuffer::new();
        let snap = b.snapshot();
        assert_eq!(snap.total, 0);
        // P4 regression: an empty buffer is "no evidence", not "100%
        // healthy". Returning Some(1.0) would let any caller doing
        // `rate() >= threshold` rubber-stamp a brand-new adapter.
        assert_eq!(snap.rate(), None);
        assert!(!snap.has_evidence());
    }

    #[test]
    fn buffer_records_outcomes_and_computes_rate() {
        let mut b = ConformanceRingBuffer::with_capacity(10);
        for _ in 0..7 {
            b.record_success(Strategy::NativeTool);
        }
        for _ in 0..3 {
            b.record_failure(Strategy::NativeTool);
        }
        let s = b.snapshot();
        assert_eq!(s.total, 10);
        assert_eq!(s.successes, 7);
        assert_eq!(s.failures, 3);
        let r = s.rate().expect("non-empty buffer has a rate");
        assert!((r - 0.7).abs() < 1e-6);
        assert!(s.has_evidence());
    }

    #[test]
    fn buffer_evicts_oldest_when_at_capacity() {
        let mut b = ConformanceRingBuffer::with_capacity(3);
        b.record_success(Strategy::NativeTool);
        b.record_failure(Strategy::NativeTool);
        b.record_failure(Strategy::NativeTool);
        b.record_success(Strategy::NativeTool);
        // First success was evicted; remaining: F, F, S.
        let s = b.snapshot();
        assert_eq!(s.total, 3);
        assert_eq!(s.successes, 1);
        assert_eq!(s.failures, 2);
    }

    #[test]
    fn buffer_per_strategy_breakdown_excludes_strategies_with_no_samples() {
        let mut b = ConformanceRingBuffer::new();
        b.record_success(Strategy::NativeTool);
        b.record_failure(Strategy::JsonSentinel);
        let s = b.snapshot();
        let kinds: Vec<Strategy> = s.by_strategy.iter().map(|(k, _, _)| *k).collect();
        assert_eq!(kinds, vec![Strategy::NativeTool, Strategy::JsonSentinel]);
        // RegexProse not present — no samples.
        assert!(!kinds.contains(&Strategy::RegexProse));
    }

    #[test]
    fn buffer_default_capacity_is_the_spec_window_of_100() {
        let b = ConformanceRingBuffer::new();
        assert_eq!(b.capacity(), CONFORMANCE_WINDOW);
    }

    // ---------- should_degrade ----------

    #[test]
    fn should_degrade_returns_false_on_empty_buffer() {
        let b = ConformanceRingBuffer::new();
        assert!(!b.should_degrade());
    }

    #[test]
    fn should_degrade_returns_false_when_window_not_yet_full() {
        // 19 samples, all failures — still under the 20-sample window
        // threshold, so the runner should NOT degrade on partial
        // evidence.
        let mut b = ConformanceRingBuffer::new();
        for _ in 0..19 {
            b.record_failure(Strategy::NativeTool);
        }
        assert!(!b.should_degrade());
    }

    #[test]
    fn should_degrade_returns_true_at_three_failures_in_a_full_window() {
        // 20 samples, exactly 3 failures (the threshold) — degrade.
        let mut b = ConformanceRingBuffer::new();
        for _ in 0..3 {
            b.record_failure(Strategy::NativeTool);
        }
        for _ in 0..17 {
            b.record_success(Strategy::NativeTool);
        }
        assert!(b.should_degrade());
    }

    #[test]
    fn should_degrade_stays_false_with_two_failures_in_a_full_window() {
        // 20 samples, only 2 failures — just under the 3-failure
        // threshold, so no degradation.
        let mut b = ConformanceRingBuffer::new();
        for _ in 0..2 {
            b.record_failure(Strategy::NativeTool);
        }
        for _ in 0..18 {
            b.record_success(Strategy::NativeTool);
        }
        assert!(!b.should_degrade());
    }

    #[test]
    fn should_degrade_considers_only_the_most_recent_window() {
        // 30 samples: first 10 are failures, last 20 are successes.
        // The rolling window only sees the recent 20 (all success), so
        // `should_degrade` is false even though there were >3 failures
        // earlier in the buffer.
        let mut b = ConformanceRingBuffer::new();
        for _ in 0..10 {
            b.record_failure(Strategy::NativeTool);
        }
        for _ in 0..20 {
            b.record_success(Strategy::NativeTool);
        }
        assert!(!b.should_degrade());
    }

    #[test]
    fn should_degrade_custom_threshold_honours_arguments() {
        // Dial the window to 5 and the threshold to 2 — useful for
        // integration tests that script a short sequence.
        let mut b = ConformanceRingBuffer::new();
        for _ in 0..4 {
            b.record_success(Strategy::NativeTool);
        }
        b.record_failure(Strategy::NativeTool);
        // 5 samples, 1 failure → under threshold of 2.
        assert!(!b.should_degrade_with(5, 2));
        b.record_failure(Strategy::NativeTool);
        // Now 6 samples; the recent 5 = 4 success + 2 failure → at
        // threshold of 2 → degrade.
        assert!(b.should_degrade_with(5, 2));
    }

    #[test]
    fn should_degrade_threshold_default_is_the_spec_value() {
        // Pin the constants so a calibration row touching them is
        // forced to update this test.
        assert_eq!(DEFAULT_DEGRADATION_WINDOW, 20);
        assert_eq!(DEFAULT_DEGRADATION_THRESHOLD, 3);
    }
}
