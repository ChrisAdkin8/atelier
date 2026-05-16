//! §5 typed plan canvas.
//!
//! Spec §5 "Visible context / memory / plan":
//!   * "Plan canvas (editable tree; reorder, constraints, manual mark-done)"
//!   * "Planning is opt-in. The agent emits `plan_update` in the envelope
//!     when it wants to contribute to the plan canvas (§2.5)."
//!
//! Schema: `schemas/session/v1.json` `plan.steps[]` items
//! (`{id, text, status, constraints?}`). This module types the field so the
//! §5 plan canvas consumes `PlanStep` directly and so the envelope's
//! [`crate::protocol::PlanUpdate`] has a typed application path.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::protocol::{PlanOp, PlanOpKind, PlanUpdate};

/// Status of a plan step. Matches the schema's `status` enum exactly so the
/// typed shape and the on-disk JSON stay locked together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
    Skipped,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Skipped => "skipped",
        }
    }

    /// Whether this status counts as terminal for the plan canvas UI
    /// (renders with a strike-through / muted colour). `Done` and `Skipped`
    /// are terminal; `Pending` and `InProgress` are not.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }
}

/// A single plan step. Mirrors the schema's `plan.steps[]` shape. The `id`
/// is assigned by [`PlanCanvas::add`] and is stable for the lifetime of the
/// step — call sites referencing a step by id survive reorder + status
/// changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub text: String,
    pub status: PlanStatus,
    /// Per-step constraint pins. Absent in JSON when empty so existing
    /// minimal-session JSONs round-trip unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("plan step with id {0:?} already exists")]
    DuplicateId(String),

    #[error("plan step {0:?} not found")]
    NotFound(String),

    #[error(
        "reorder list does not match canvas membership (expected {expected} ids, got {got}; missing: {missing:?})"
    )]
    ReorderMismatch {
        expected: usize,
        got: usize,
        missing: Vec<String>,
    },
}

/// Insertion-ordered list of plan steps. The plan canvas renders in this
/// order; reorder operations rewrite it. Mirrors the storage pattern in
/// [`crate::context::ContextManager`] / [`crate::memory::MemoryStore`].
///
/// **Not internally `Send + Sync`** — owned by the §2.5 session actor.
/// Wrap in `Arc<Mutex<_>>` for concurrent access.
#[derive(Debug, Default, Clone)]
pub struct PlanCanvas {
    order: Vec<String>,
    by_id: BTreeMap<String, PlanStep>,
    next_serial: u64,
}

impl PlanCanvas {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a serialised list (e.g., loaded from
    /// `OnDiskSession.plan.steps`). Rejects duplicate ids.
    pub fn from_vec(steps: Vec<PlanStep>) -> Result<Self, PlanError> {
        let mut c = Self::default();
        for s in steps {
            c.insert(s)?;
        }
        Ok(c)
    }

    /// Snapshot back to the on-disk representation (canvas order).
    pub fn to_vec(&self) -> Vec<PlanStep> {
        self.order
            .iter()
            .map(|id| self.by_id.get(id).expect("order and by_id in sync").clone())
            .collect()
    }

    /// Add a fresh step with auto-assigned id (`step-N`). Returns the id.
    /// The harness uses this; the envelope's `PlanUpdate` is best-effort
    /// applied via [`Self::apply_envelope`] which routes through here.
    pub fn add(&mut self, text: impl Into<String>) -> String {
        let id = loop {
            let candidate = format!("step-{}", self.next_serial);
            self.next_serial += 1;
            if !self.by_id.contains_key(&candidate) {
                break candidate;
            }
        };
        let step = PlanStep {
            id: id.clone(),
            text: text.into(),
            status: PlanStatus::Pending,
            constraints: Vec::new(),
        };
        self.order.push(id.clone());
        self.by_id.insert(id.clone(), step);
        id
    }

    /// Insert a fully-formed step (used by [`Self::from_vec`] and for
    /// migrating from `Vec<serde_json::Value>` storage). Rejects duplicates.
    pub fn insert(&mut self, step: PlanStep) -> Result<(), PlanError> {
        if self.by_id.contains_key(&step.id) {
            return Err(PlanError::DuplicateId(step.id));
        }
        // Keep `next_serial` ahead of any auto-derivable id so a later
        // `add` doesn't collide with an inserted id of the form `step-N`.
        if let Some(n) = step
            .id
            .strip_prefix("step-")
            .and_then(|s| s.parse::<u64>().ok())
        {
            self.next_serial = self.next_serial.max(n + 1);
        }
        self.order.push(step.id.clone());
        self.by_id.insert(step.id.clone(), step);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<PlanStep, PlanError> {
        let step = self
            .by_id
            .remove(id)
            .ok_or_else(|| PlanError::NotFound(id.to_string()))?;
        self.order.retain(|e| e != id);
        Ok(step)
    }

    pub fn mark_status(&mut self, id: &str, status: PlanStatus) -> Result<(), PlanError> {
        self.with_mut(id, |s| s.status = status)
    }

    pub fn mark_done(&mut self, id: &str) -> Result<(), PlanError> {
        self.mark_status(id, PlanStatus::Done)
    }

    pub fn mark_skipped(&mut self, id: &str) -> Result<(), PlanError> {
        self.mark_status(id, PlanStatus::Skipped)
    }

    pub fn add_constraint(&mut self, id: &str, text: impl Into<String>) -> Result<(), PlanError> {
        let text = text.into();
        self.with_mut(id, |s| {
            if !s.constraints.iter().any(|c| *c == text) {
                s.constraints.push(text.clone());
            }
        })
    }

    /// Rewrite the canvas order. `new_order` must contain every existing id
    /// exactly once; otherwise [`PlanError::ReorderMismatch`] is returned
    /// and the canvas is unchanged.
    pub fn reorder(&mut self, new_order: Vec<String>) -> Result<(), PlanError> {
        if new_order.len() != self.order.len() {
            return Err(PlanError::ReorderMismatch {
                expected: self.order.len(),
                got: new_order.len(),
                missing: missing_ids(&self.order, &new_order),
            });
        }
        // O(N log N) — sort both slices to compare without mutating the
        // canvas before the all-clear.
        let mut have: Vec<&String> = self.order.iter().collect();
        let mut want: Vec<&String> = new_order.iter().collect();
        have.sort();
        want.sort();
        if have != want {
            return Err(PlanError::ReorderMismatch {
                expected: self.order.len(),
                got: new_order.len(),
                missing: missing_ids(&self.order, &new_order),
            });
        }
        self.order = new_order;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&PlanStep> {
        self.by_id.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PlanStep> {
        self.order
            .iter()
            .map(|id| self.by_id.get(id).expect("invariant"))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Apply the envelope's [`PlanUpdate`] (per spec §2.5: "the agent emits
    /// `plan_update` … the harness never gates on it"). Best-effort: model
    /// references steps by their text rather than id, so `remove` and
    /// `complete` match the first step whose `text` equals `step` exactly,
    /// while `add` creates a new step and `reorder` is ignored at this
    /// layer (the model can't author a valid id ordering on its own; user-
    /// driven reorder goes through [`Self::reorder`]). Returns an
    /// [`ApplyReport`] so the UI can show which ops applied vs. dropped.
    pub fn apply_envelope(&mut self, update: &PlanUpdate) -> ApplyReport {
        let mut report = ApplyReport::default();
        for op in &update.ops {
            match self.apply_envelope_op(op) {
                Ok(()) => report.applied += 1,
                Err(reason) => report.dropped.push((op.clone(), reason)),
            }
        }
        report
    }

    fn apply_envelope_op(&mut self, op: &PlanOp) -> Result<(), &'static str> {
        match op.op {
            PlanOpKind::Add => {
                self.add(op.step.clone());
                Ok(())
            }
            PlanOpKind::Remove => {
                let id = self.find_by_text(&op.step).ok_or("no step matches text")?;
                self.remove(&id).map_err(|_| "remove failed")?;
                Ok(())
            }
            PlanOpKind::Complete => {
                let id = self.find_by_text(&op.step).ok_or("no step matches text")?;
                self.mark_done(&id).map_err(|_| "mark_done failed")?;
                Ok(())
            }
            // Reorder over a text-keyed envelope op is ambiguous without
            // additional structure (which step goes where?). Drop with a
            // reason so the UI can surface "the model tried to reorder; do
            // it by hand if you need to."
            PlanOpKind::Reorder => Err("reorder must come from the user, not envelope"),
        }
    }

    fn find_by_text(&self, text: &str) -> Option<String> {
        self.order
            .iter()
            .find(|id| self.by_id.get(*id).map(|s| s.text == text).unwrap_or(false))
            .cloned()
    }

    fn with_mut<F: FnOnce(&mut PlanStep)>(&mut self, id: &str, f: F) -> Result<(), PlanError> {
        let step = self
            .by_id
            .get_mut(id)
            .ok_or_else(|| PlanError::NotFound(id.to_string()))?;
        f(step);
        Ok(())
    }
}

fn missing_ids(have: &[String], want: &[String]) -> Vec<String> {
    have.iter().filter(|h| !want.contains(h)).cloned().collect()
}

/// What `apply_envelope` did. The UI uses this to render a "the model wanted
/// to do X but it was ambiguous" badge for dropped ops.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: usize,
    pub dropped: Vec<(PlanOp, &'static str)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, text: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            id: id.into(),
            text: text.into(),
            status,
            constraints: Vec::new(),
        }
    }

    // ---------- add / insert / iter / serde round-trip ----------

    #[test]
    fn add_returns_unique_ids_and_appends() {
        let mut c = PlanCanvas::new();
        let a = c.add("first");
        let b = c.add("second");
        assert_ne!(a, b);
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["first", "second"]);
        for s in c.iter() {
            assert_eq!(s.status, PlanStatus::Pending);
            assert!(s.constraints.is_empty());
        }
    }

    #[test]
    fn insert_rejects_duplicate_id_without_mutating() {
        let mut c = PlanCanvas::new();
        c.insert(step("step-0", "a", PlanStatus::Pending)).unwrap();
        let err = c
            .insert(step("step-0", "different", PlanStatus::Done))
            .unwrap_err();
        assert!(matches!(err, PlanError::DuplicateId(_)));
        assert_eq!(c.len(), 1);
        assert_eq!(c.get("step-0").unwrap().text, "a");
    }

    #[test]
    fn insert_then_add_does_not_collide_with_existing_step_n_ids() {
        let mut c = PlanCanvas::new();
        c.insert(step("step-5", "imported", PlanStatus::Pending))
            .unwrap();
        let fresh = c.add("new");
        assert_eq!(fresh, "step-6");
    }

    #[test]
    fn from_vec_and_to_vec_round_trip_in_order() {
        let cards = vec![
            step("a", "first", PlanStatus::Done),
            step("b", "second", PlanStatus::InProgress),
            step("c", "third", PlanStatus::Pending),
        ];
        let c = PlanCanvas::from_vec(cards.clone()).unwrap();
        assert_eq!(c.to_vec(), cards);
    }

    #[test]
    fn serde_round_trips_with_optional_constraints() {
        let s = step("step-0", "build the thing", PlanStatus::Pending);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("constraints"));
        let back: PlanStep = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        let with_constraints = PlanStep {
            constraints: vec!["no new deps".into()],
            ..s
        };
        let json = serde_json::to_string(&with_constraints).unwrap();
        assert!(json.contains("constraints"));
        let back: PlanStep = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_constraints);
    }

    #[test]
    fn status_enum_matches_schema_literals() {
        for (lit, st) in [
            ("pending", PlanStatus::Pending),
            ("in_progress", PlanStatus::InProgress),
            ("done", PlanStatus::Done),
            ("skipped", PlanStatus::Skipped),
        ] {
            assert_eq!(serde_json::to_string(&st).unwrap(), format!("\"{lit}\""));
        }
    }

    #[test]
    fn terminal_predicate_matches_done_and_skipped() {
        assert!(PlanStatus::Done.is_terminal());
        assert!(PlanStatus::Skipped.is_terminal());
        assert!(!PlanStatus::Pending.is_terminal());
        assert!(!PlanStatus::InProgress.is_terminal());
    }

    // ---------- mutators ----------

    #[test]
    fn mark_done_and_mark_skipped_update_status() {
        let mut c = PlanCanvas::new();
        let a = c.add("a");
        let b = c.add("b");
        c.mark_done(&a).unwrap();
        c.mark_skipped(&b).unwrap();
        assert_eq!(c.get(&a).unwrap().status, PlanStatus::Done);
        assert_eq!(c.get(&b).unwrap().status, PlanStatus::Skipped);
    }

    #[test]
    fn remove_drops_step_and_keeps_others_in_order() {
        let mut c = PlanCanvas::new();
        let _a = c.add("a");
        let b = c.add("b");
        let _c = c.add("c");
        c.remove(&b).unwrap();
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["a", "c"]);
    }

    #[test]
    fn add_constraint_is_idempotent() {
        let mut c = PlanCanvas::new();
        let a = c.add("a");
        c.add_constraint(&a, "no new deps").unwrap();
        c.add_constraint(&a, "no new deps").unwrap();
        c.add_constraint(&a, "preserve api").unwrap();
        let constraints = &c.get(&a).unwrap().constraints;
        assert_eq!(
            constraints,
            &vec!["no new deps".to_string(), "preserve api".into()]
        );
    }

    #[test]
    fn mutator_on_missing_id_errors_without_mutating() {
        let mut c = PlanCanvas::new();
        let a = c.add("a");
        assert!(matches!(c.mark_done("nope"), Err(PlanError::NotFound(_))));
        assert!(matches!(
            c.add_constraint("nope", "x"),
            Err(PlanError::NotFound(_))
        ));
        assert!(matches!(c.remove("nope"), Err(PlanError::NotFound(_))));
        assert_eq!(c.get(&a).unwrap().status, PlanStatus::Pending);
        assert_eq!(c.len(), 1);
    }

    // ---------- reorder ----------

    #[test]
    fn reorder_rewrites_canvas_order() {
        let mut c = PlanCanvas::new();
        let a = c.add("a");
        let b = c.add("b");
        let cid = c.add("c");
        c.reorder(vec![cid.clone(), a.clone(), b.clone()]).unwrap();
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["c", "a", "b"]);
    }

    #[test]
    fn reorder_rejects_wrong_length_and_keeps_order() {
        let mut c = PlanCanvas::new();
        let a = c.add("a");
        let _b = c.add("b");
        let err = c.reorder(vec![a.clone()]).unwrap_err();
        assert!(matches!(
            err,
            PlanError::ReorderMismatch {
                expected: 2,
                got: 1,
                ..
            }
        ));
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["a", "b"]);
    }

    #[test]
    fn reorder_rejects_unknown_ids_and_keeps_order() {
        let mut c = PlanCanvas::new();
        let a = c.add("a");
        let _b = c.add("b");
        let err = c.reorder(vec![a.clone(), "step-99".into()]).unwrap_err();
        assert!(matches!(err, PlanError::ReorderMismatch { .. }));
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["a", "b"]);
    }

    // ---------- envelope plan_update consumption ----------

    fn op(kind: PlanOpKind, step: &str) -> PlanOp {
        PlanOp {
            op: kind,
            step: step.into(),
        }
    }

    #[test]
    fn apply_envelope_add_appends_new_steps() {
        let mut c = PlanCanvas::new();
        let report = c.apply_envelope(&PlanUpdate {
            ops: vec![
                op(PlanOpKind::Add, "draft API"),
                op(PlanOpKind::Add, "write tests"),
            ],
        });
        assert_eq!(report.applied, 2);
        assert!(report.dropped.is_empty());
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["draft API", "write tests"]);
    }

    #[test]
    fn apply_envelope_complete_marks_first_text_match_done() {
        let mut c = PlanCanvas::new();
        c.add("draft");
        c.add("write tests");
        let report = c.apply_envelope(&PlanUpdate {
            ops: vec![op(PlanOpKind::Complete, "draft")],
        });
        assert_eq!(report.applied, 1);
        let done: Vec<_> = c
            .iter()
            .filter(|s| s.status == PlanStatus::Done)
            .map(|s| s.text.clone())
            .collect();
        assert_eq!(done, vec!["draft"]);
    }

    #[test]
    fn apply_envelope_remove_drops_first_text_match() {
        let mut c = PlanCanvas::new();
        c.add("a");
        c.add("b");
        let report = c.apply_envelope(&PlanUpdate {
            ops: vec![op(PlanOpKind::Remove, "a")],
        });
        assert_eq!(report.applied, 1);
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["b"]);
    }

    #[test]
    fn apply_envelope_remove_with_no_match_is_dropped_with_reason() {
        let mut c = PlanCanvas::new();
        c.add("a");
        let report = c.apply_envelope(&PlanUpdate {
            ops: vec![op(PlanOpKind::Remove, "ghost")],
        });
        assert_eq!(report.applied, 0);
        assert_eq!(report.dropped.len(), 1);
        assert!(report.dropped[0].1.contains("no step matches text"));
    }

    #[test]
    fn apply_envelope_reorder_from_envelope_is_dropped_intentionally() {
        let mut c = PlanCanvas::new();
        c.add("a");
        c.add("b");
        let report = c.apply_envelope(&PlanUpdate {
            ops: vec![op(PlanOpKind::Reorder, "a")],
        });
        assert_eq!(report.applied, 0);
        assert_eq!(report.dropped.len(), 1);
        assert!(report.dropped[0]
            .1
            .contains("reorder must come from the user"));
        // Order unchanged.
        let texts: Vec<_> = c.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts, vec!["a", "b"]);
    }

    #[test]
    fn apply_envelope_handles_a_mix_of_add_complete_and_drop() {
        let mut c = PlanCanvas::new();
        c.add("existing");
        let report = c.apply_envelope(&PlanUpdate {
            ops: vec![
                op(PlanOpKind::Add, "new step"),
                op(PlanOpKind::Complete, "existing"),
                op(PlanOpKind::Remove, "ghost"),
                op(PlanOpKind::Reorder, "irrelevant"),
            ],
        });
        assert_eq!(report.applied, 2);
        assert_eq!(report.dropped.len(), 2);
        assert_eq!(c.len(), 2);
        let steps: Vec<_> = c.iter().collect();
        assert_eq!(steps[0].status, PlanStatus::Done);
        assert_eq!(steps[1].text, "new step");
    }
}
