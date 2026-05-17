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

use std::collections::BTreeMap;

use crate::protocol::{ClaimedChange, ClaimedChangeKind, Envelope};

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
}
