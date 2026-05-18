//! §7 verification gates — pure functions.
//!
//! Spec §7 "Did-it-do-what-it-said":
//!   The harness compares the model's [`Envelope::claimed_changes`] against
//!   the actual on-disk diff produced by §3 atomic staging. A mismatch
//!   (claimed edit but no diff, diff but no claim, claim says delete but
//!   file still present, claim says create but file pre-existed) is the
//!   "lying-agent" signal — surfaces as red in the UI and trips the §7
//!   mechanical gate.
//!
//! This module is **pure**: it takes a claimed-changes list + an observed
//! workspace-diff list and returns a list of [`Discrepancy`] items. It does
//! not look at the filesystem or shell out — that wiring belongs in the
//! `Verifying` state of the agent loop, which feeds the inputs here.
//!
//! The §7 hallucination detector (LSP shell-out, tiered language coverage)
//! lands in its own module once `tower-lsp` is wired and Q3 (LSP auto-install
//! UX) is resolved.
//!
//! # Verification tiers
//!
//! Spec §7 lays out three tiers of verification coverage. A given verify
//! pass picks the *highest* tier whose producer is available; the UI
//! surfaces the chosen tier so the user can see when a higher-tier
//! check fell back to a coarser one.
//!
//!   * **Tier 1 — LSP** (`Tier1Lsp`). Gated on Q3 (LSP auto-install UX);
//!     not yet wired. The variant exists so the producer-side wiring is
//!     a one-line change when the LSP shell-out lands.
//!   * **Tier 2 — tree-sitter** (`Tier2TreeSitter`). Syntactic checks
//!     run in [`crate::staging::SyntaxCheck`] (the real impl is
//!     `TreeSitterSyntaxCheck`). When that check ran for at least one
//!     file in this turn, the verify pass reports Tier 2.
//!   * **Tier 3 — textual** (`Tier3Textual`). The pure
//!     [`compare`] lying-agent detector below. Always available; this
//!     is the fallback when no higher tier ran.
//!   * **NotRun** — the harness did not run a verify pass this turn
//!     (e.g. the envelope didn't declare `claimed_done`). UIs render
//!     a "verify off" badge so absence is unambiguous.

use std::collections::BTreeMap;

use crate::protocol::{ClaimedChange, ClaimedChangeKind, Envelope};

/// Which tier of §7 verification actually ran this turn. The producer
/// picks the highest tier whose check executed; consumers surface it
/// so the user can see the active hallucination-coverage level.
///
/// Wire form is the snake-case variant name; serde and
/// [`Self::wire_label`] agree by construction (pinned by
/// `verification_tier_wire_labels_agree_with_serde`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationTier {
    /// Spec §7 Tier 1 — LSP-driven hallucination detector. Gated on
    /// Q3 (LSP auto-install UX); no producer is wired yet. The
    /// variant exists so a future producer can flip to this tier
    /// without a wire-format change.
    Tier1Lsp,
    /// Spec §7 Tier 2 — tree-sitter syntactic checks. Producer:
    /// [`crate::staging::TreeSitterSyntaxCheck`]. Reported when at
    /// least one staged file ran the syntax check this turn.
    Tier2TreeSitter,
    /// Spec §7 Tier 3 — textual lying-agent detector. Producer:
    /// [`compare`] in this module. Always available; the fallback
    /// when no higher tier ran for any file in the turn.
    Tier3Textual,
    /// No verify pass ran this turn. Emitted so the UI can render a
    /// "verify off" badge unambiguously rather than inferring absence.
    NotRun,
}

impl VerificationTier {
    /// Stable wire label used by the GUI bridge and TUI projection.
    /// Pinned by `verification_tier_wire_labels_agree_with_serde` so
    /// a future variant rename forces a deliberate edit.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Tier1Lsp => "tier1_lsp",
            Self::Tier2TreeSitter => "tier2_tree_sitter",
            Self::Tier3Textual => "tier3_textual",
            Self::NotRun => "not_run",
        }
    }
}

/// One §7 verify pass's summary. The `tier` says which producer ran;
/// `file_count` is the number of files the pass considered (the union
/// of claimed paths + observed paths); `claim_count` is the number of
/// envelope `claimed_changes` entries it weighed. `discrepancies` is
/// the [`compare`] output, retained so the dispatcher can ledger or
/// otherwise act on the lying-agent signal without re-running the
/// comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRun {
    pub tier: VerificationTier,
    pub file_count: usize,
    pub claim_count: usize,
    pub discrepancies: Vec<Discrepancy>,
}

impl VerificationRun {
    /// Run the §7 textual (Tier 3) pass and bundle the result with
    /// the tier label. Convenience for callers (the dispatcher's
    /// `verify_pass`) that don't need to thread `compare` and the
    /// tier badge through their own state. Tier 1 / Tier 2 producers
    /// aren't wired yet (see [`VerificationTier`]); when they land,
    /// add sibling constructors here.
    pub fn tier3_textual(envelope: &Envelope, observed: &[ObservedChange]) -> Self {
        let discrepancies = compare(envelope, observed);
        let claim_count = envelope
            .claimed_changes
            .as_ref()
            .map(|c| c.len())
            .unwrap_or(0);

        // file_count is the union of claimed paths + observed paths.
        // BTreeSet keeps it O(n log n) deterministic without pulling
        // in a HashSet allocation.
        let mut paths = std::collections::BTreeSet::new();
        if let Some(claims) = envelope.claimed_changes.as_ref() {
            for c in claims {
                paths.insert(c.path.as_str());
            }
        }
        for o in observed {
            paths.insert(o.path.as_str());
        }

        Self {
            tier: VerificationTier::Tier3Textual,
            file_count: paths.len(),
            claim_count,
            discrepancies,
        }
    }

    /// Sentinel for "the harness did not run a verify pass this turn".
    /// Distinct from a zero-discrepancy Tier 3 run — the UI renders a
    /// different badge so absence is unambiguous.
    pub fn not_run() -> Self {
        Self {
            tier: VerificationTier::NotRun,
            file_count: 0,
            claim_count: 0,
            discrepancies: Vec::new(),
        }
    }
}

/// Observed change to a single path produced by §3 staging. Built by
/// diffing the post-commit workspace against the pre-turn baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedChange {
    pub path: String,
    pub kind: ObservedKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedKind {
    /// File existed before and after; bytes differ.
    Modified,
    /// File did not exist before; now exists.
    Created,
    /// File existed before; no longer exists.
    Deleted,
}

impl ObservedKind {
    fn matches(self, claimed: ClaimedChangeKind) -> bool {
        matches!(
            (self, claimed),
            (Self::Modified, ClaimedChangeKind::Edit)
                | (Self::Created, ClaimedChangeKind::Create)
                | (Self::Deleted, ClaimedChangeKind::Delete)
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Created => "created",
            Self::Deleted => "deleted",
        }
    }
}

/// One mismatch between the envelope's claims and what actually happened.
/// Each variant carries enough context for the UI to render a red badge with
/// a precise reason, and for the cost ledger to log the mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discrepancy {
    /// Model claimed it changed `path` but the workspace diff is empty for
    /// that path. The lying-agent gate's primary signal.
    Claimed {
        path: String,
        claimed: ClaimedChangeKind,
    },

    /// Workspace diff shows a change at `path` but the envelope didn't
    /// mention it. The model edited something silently — also a trust
    /// failure.
    Unclaimed {
        path: String,
        observed: ObservedKind,
    },

    /// Both sides reference the same `path` but disagree on kind (e.g.,
    /// claimed `delete` but file was only modified).
    KindMismatch {
        path: String,
        claimed: ClaimedChangeKind,
        observed: ObservedKind,
    },

    /// Same path appears more than once in `claimed_changes`. Spec §2
    /// doesn't forbid it, but it confuses the diff comparison and is worth
    /// flagging at trust-budget time. **Orthogonal to the per-path
    /// `Claimed` / `Unclaimed` / `KindMismatch` discrepancies** — the
    /// duplicate flag conveys "the model claimed N times" (a model-quality
    /// signal) while the per-path comparison conveys "the workspace
    /// disagrees with the claim" (a verification signal). Both can fire
    /// for the same path; UIs that summarise per-path can group via
    /// [`Discrepancy::path`].
    DuplicateClaim { path: String, count: usize },
}

impl Discrepancy {
    /// Path the discrepancy is about. Useful when grouping for UI.
    pub fn path(&self) -> &str {
        match self {
            Self::Claimed { path, .. }
            | Self::Unclaimed { path, .. }
            | Self::KindMismatch { path, .. }
            | Self::DuplicateClaim { path, .. } => path,
        }
    }

    /// One-line human-readable summary for log lines and the ledger.
    pub fn summary(&self) -> String {
        match self {
            Self::Claimed { path, claimed } => format!(
                "{path}: claimed {} but workspace diff is empty",
                kind_label(*claimed)
            ),
            Self::Unclaimed { path, observed } => format!(
                "{path}: workspace diff shows {} but envelope did not claim it",
                observed.as_str()
            ),
            Self::KindMismatch {
                path,
                claimed,
                observed,
            } => format!(
                "{path}: claimed {} but observed {}",
                kind_label(*claimed),
                observed.as_str()
            ),
            Self::DuplicateClaim { path, count } => {
                format!("{path}: claimed {count} times in one envelope")
            }
        }
    }
}

// v59 (MED-smell-2 fix) — `kind_label` retired in favour of the
// canonical `ClaimedChangeKind::wire_label`. Inlined at call sites
// below.
#[inline]
fn kind_label(k: ClaimedChangeKind) -> &'static str {
    k.wire_label()
}

/// Run the §7 did-it-do-what-it-said comparison. Returns an empty vec on
/// agreement; otherwise the list of [`Discrepancy`] items the UI should
/// surface and the trust-budget should weight.
///
/// **Per-claim duplicate detection** runs first because a duplicate
/// poisons the per-path comparison — we report the duplicate, then fold
/// duplicates into a single entry for the comparison pass so the user sees
/// both the duplicate flag and the underlying claim/observation mismatch.
pub fn compare(envelope: &Envelope, observed: &[ObservedChange]) -> Vec<Discrepancy> {
    let claimed_list: &[ClaimedChange] = envelope.claimed_changes.as_deref().unwrap_or(&[]);

    let mut discrepancies = Vec::new();

    // 1. Duplicate-claim detection.
    let mut claim_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for c in claimed_list {
        *claim_counts.entry(c.path.as_str()).or_insert(0) += 1;
    }
    for (path, count) in &claim_counts {
        if *count > 1 {
            discrepancies.push(Discrepancy::DuplicateClaim {
                path: (*path).to_string(),
                count: *count,
            });
        }
    }

    // 2. Build a claim-by-path map (last claim wins on dup, but the dup is
    //    already flagged above so the comparison just needs *some* entry).
    let mut claims: BTreeMap<&str, &ClaimedChange> = BTreeMap::new();
    for c in claimed_list {
        claims.insert(c.path.as_str(), c);
    }

    let mut observed_map: BTreeMap<&str, &ObservedChange> = BTreeMap::new();
    for o in observed {
        observed_map.insert(o.path.as_str(), o);
    }

    // 3. Per-claimed-path: is it in the diff? Right kind?
    for (path, claim) in &claims {
        match observed_map.get(path) {
            None => discrepancies.push(Discrepancy::Claimed {
                path: (*path).to_string(),
                claimed: claim.kind,
            }),
            Some(o) if !o.kind.matches(claim.kind) => {
                discrepancies.push(Discrepancy::KindMismatch {
                    path: (*path).to_string(),
                    claimed: claim.kind,
                    observed: o.kind,
                });
            }
            Some(_) => {}
        }
    }

    // 4. Per-observed-path that has no claim — model edited silently.
    for (path, obs) in &observed_map {
        if !claims.contains_key(path) {
            discrepancies.push(Discrepancy::Unclaimed {
                path: (*path).to_string(),
                observed: obs.kind,
            });
        }
    }

    discrepancies
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClaimedChange;

    fn claim(path: &str, kind: ClaimedChangeKind) -> ClaimedChange {
        ClaimedChange {
            path: path.into(),
            kind,
            summary: format!("{kind:?} {path}"),
        }
    }

    fn envelope_with(changes: Vec<ClaimedChange>) -> Envelope {
        Envelope {
            claimed_changes: Some(changes),
            ..Default::default()
        }
    }

    fn observed(path: &str, kind: ObservedKind) -> ObservedChange {
        ObservedChange {
            path: path.into(),
            kind,
        }
    }

    #[test]
    fn agreement_yields_no_discrepancies() {
        let env = envelope_with(vec![
            claim("a.py", ClaimedChangeKind::Edit),
            claim("b.py", ClaimedChangeKind::Create),
        ]);
        let obs = vec![
            observed("a.py", ObservedKind::Modified),
            observed("b.py", ObservedKind::Created),
        ];
        assert!(compare(&env, &obs).is_empty());
    }

    #[test]
    fn claimed_but_not_observed_flags_lying_agent_signal() {
        let env = envelope_with(vec![claim("a.py", ClaimedChangeKind::Edit)]);
        let result = compare(&env, &[]);
        assert_eq!(
            result,
            vec![Discrepancy::Claimed {
                path: "a.py".into(),
                claimed: ClaimedChangeKind::Edit
            }]
        );
        // Summary mentions the path and the claimed kind.
        assert!(result[0].summary().contains("a.py"));
        assert!(result[0].summary().contains("edit"));
    }

    #[test]
    fn observed_but_not_claimed_flags_silent_edit() {
        let env = envelope_with(vec![]);
        let obs = vec![observed("secret.txt", ObservedKind::Modified)];
        assert_eq!(
            compare(&env, &obs),
            vec![Discrepancy::Unclaimed {
                path: "secret.txt".into(),
                observed: ObservedKind::Modified
            }]
        );
    }

    #[test]
    fn kind_mismatch_is_distinct_from_missing() {
        // Claimed delete, but workspace shows edit.
        let env = envelope_with(vec![claim("a.py", ClaimedChangeKind::Delete)]);
        let obs = vec![observed("a.py", ObservedKind::Modified)];
        assert_eq!(
            compare(&env, &obs),
            vec![Discrepancy::KindMismatch {
                path: "a.py".into(),
                claimed: ClaimedChangeKind::Delete,
                observed: ObservedKind::Modified,
            }]
        );
    }

    #[test]
    fn all_three_kind_match_pairs_are_clean() {
        let env = envelope_with(vec![
            claim("e.py", ClaimedChangeKind::Edit),
            claim("c.py", ClaimedChangeKind::Create),
            claim("d.py", ClaimedChangeKind::Delete),
        ]);
        let obs = vec![
            observed("e.py", ObservedKind::Modified),
            observed("c.py", ObservedKind::Created),
            observed("d.py", ObservedKind::Deleted),
        ];
        assert!(compare(&env, &obs).is_empty());
    }

    #[test]
    fn duplicate_claim_is_flagged_separately() {
        let env = envelope_with(vec![
            claim("a.py", ClaimedChangeKind::Edit),
            claim("a.py", ClaimedChangeKind::Edit),
        ]);
        let obs = vec![observed("a.py", ObservedKind::Modified)];
        let result = compare(&env, &obs);
        assert_eq!(
            result,
            vec![Discrepancy::DuplicateClaim {
                path: "a.py".into(),
                count: 2,
            }]
        );
    }

    #[test]
    fn duplicate_claim_plus_unobserved_yields_both_signals() {
        // Two claims for the same path, neither observed in workspace.
        let env = envelope_with(vec![
            claim("missing.py", ClaimedChangeKind::Edit),
            claim("missing.py", ClaimedChangeKind::Edit),
        ]);
        let result = compare(&env, &[]);
        assert_eq!(result.len(), 2);
        // DuplicateClaim reported first.
        assert!(matches!(result[0], Discrepancy::DuplicateClaim { .. }));
        assert!(matches!(result[1], Discrepancy::Claimed { .. }));
    }

    #[test]
    fn unrelated_paths_are_compared_independently() {
        let env = envelope_with(vec![
            claim("a.py", ClaimedChangeKind::Edit),
            claim("b.py", ClaimedChangeKind::Create),
        ]);
        let obs = vec![
            observed("a.py", ObservedKind::Modified), // matches a.py
            observed("c.py", ObservedKind::Modified), // unclaimed
        ];
        let result = compare(&env, &obs);
        assert_eq!(result.len(), 2);
        // b.py claimed but not observed:
        assert!(result.iter().any(|d| matches!(
            d,
            Discrepancy::Claimed { path, .. } if path == "b.py"
        )));
        // c.py observed but not claimed:
        assert!(result.iter().any(|d| matches!(
            d,
            Discrepancy::Unclaimed { path, .. } if path == "c.py"
        )));
    }

    #[test]
    fn empty_envelope_with_empty_diff_is_clean() {
        let env = Envelope::default();
        assert!(compare(&env, &[]).is_empty());
    }

    #[test]
    fn empty_envelope_with_diff_flags_every_observed_change() {
        let env = Envelope::default();
        let obs = vec![
            observed("a", ObservedKind::Modified),
            observed("b", ObservedKind::Created),
        ];
        let result = compare(&env, &obs);
        assert_eq!(result.len(), 2);
        for d in &result {
            assert!(matches!(d, Discrepancy::Unclaimed { .. }));
        }
    }

    #[test]
    fn discrepancy_path_helper_returns_the_underlying_path() {
        let d = Discrepancy::Claimed {
            path: "x.py".into(),
            claimed: ClaimedChangeKind::Edit,
        };
        assert_eq!(d.path(), "x.py");
        let d = Discrepancy::KindMismatch {
            path: "y.py".into(),
            claimed: ClaimedChangeKind::Delete,
            observed: ObservedKind::Modified,
        };
        assert_eq!(d.path(), "y.py");
    }

    #[test]
    fn summary_includes_both_kinds_for_kind_mismatch() {
        let d = Discrepancy::KindMismatch {
            path: "x".into(),
            claimed: ClaimedChangeKind::Delete,
            observed: ObservedKind::Modified,
        };
        let s = d.summary();
        assert!(s.contains("delete"));
        assert!(s.contains("modified"));
    }

    // ---------- §7 verification tier indicator ----------

    #[test]
    fn verification_tier_wire_labels_agree_with_serde() {
        // v62 (wire-vs-serde discipline) — `VerificationTier` is
        // bridged onto the GUI via serde and into the TUI footer via
        // `wire_label()`. Pin both surfaces so a future variant
        // rename forces a deliberate edit on each path. The serde
        // form is the snake_case variant name (from `rename_all`);
        // the helper returns the same string.
        for (variant, label) in [
            (VerificationTier::Tier1Lsp, "tier1_lsp"),
            (VerificationTier::Tier2TreeSitter, "tier2_tree_sitter"),
            (VerificationTier::Tier3Textual, "tier3_textual"),
            (VerificationTier::NotRun, "not_run"),
        ] {
            assert_eq!(variant.wire_label(), label);
            // Serde round-trip: serialise → string → deserialise → eq.
            let s = serde_json::to_string(&variant).unwrap();
            // Stringified JSON value is wrapped in quotes.
            assert_eq!(s, format!("\"{label}\""));
            let back: VerificationTier = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn verification_run_tier3_textual_carries_compare_output() {
        let env = envelope_with(vec![
            claim("a.py", ClaimedChangeKind::Edit),
            claim("b.py", ClaimedChangeKind::Create),
        ]);
        let obs = vec![
            observed("a.py", ObservedKind::Modified),
            // b.py claimed Create but workspace shows nothing — flags.
            observed("c.py", ObservedKind::Modified),
        ];
        let run = VerificationRun::tier3_textual(&env, &obs);
        assert_eq!(run.tier, VerificationTier::Tier3Textual);
        // claim_count is the envelope's claim list length (independent of
        // discrepancies).
        assert_eq!(run.claim_count, 2);
        // file_count is the union of claimed + observed paths: {a, b, c}.
        assert_eq!(run.file_count, 3);
        // compare() output is preserved verbatim.
        assert_eq!(run.discrepancies.len(), 2);
    }

    #[test]
    fn verification_run_tier3_textual_clean_run() {
        let env = envelope_with(vec![claim("a.py", ClaimedChangeKind::Edit)]);
        let obs = vec![observed("a.py", ObservedKind::Modified)];
        let run = VerificationRun::tier3_textual(&env, &obs);
        assert_eq!(run.tier, VerificationTier::Tier3Textual);
        assert_eq!(run.file_count, 1);
        assert_eq!(run.claim_count, 1);
        assert!(run.discrepancies.is_empty());
    }

    #[test]
    fn verification_run_not_run_sentinel_is_zeroed() {
        let run = VerificationRun::not_run();
        assert_eq!(run.tier, VerificationTier::NotRun);
        assert_eq!(run.file_count, 0);
        assert_eq!(run.claim_count, 0);
        assert!(run.discrepancies.is_empty());
    }
}
