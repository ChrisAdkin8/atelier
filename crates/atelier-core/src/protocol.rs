//! §2 Model Protocol — typed envelope.
//!
//! Spec §2 "Envelope": structured signals emitted by the model alongside its
//! natural-language output. All fields optional; `claimed_changes` and
//! `grounding` are required *when the turn made edits or factual claims*
//! (i.e., omit them on a clarification-only turn — see
//! `prompts/protocol_fewshot/with_uncertainty.md`). Schema:
//! `schemas/model_protocol/envelope.v1.json`.
//!
//! Emission strategies, parsers, and the conformance tracker land in the
//! sibling `protocol_strategy` and `protocol_conformance` modules.
//!
//! Spec §2 "Degradation policy": every UI consumer defines absent-field
//! rendering. Default: visible "unknown" state. **Never silently substitute
//! 'everything OK.'** That contract is upheld by treating the absent variant
//! as a distinct value (`Option::None`), not by inferring defaults at the
//! consumer's discretion.

use serde::{Deserialize, Serialize};

/// Atelier model-protocol envelope. Round-trips through
/// `schemas/model_protocol/envelope.v1.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_changes: Option<Vec<ClaimedChange>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_done: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty: Option<Vec<Uncertainty>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_update: Option<PlanUpdate>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grounding: Option<Vec<Grounding>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints_acknowledged: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimedChange {
    pub path: String,
    pub kind: ClaimedChangeKind,
    /// Schema enforces `maxLength: 500`; mirrored at validation time.
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimedChangeKind {
    Edit,
    Create,
    Delete,
}

impl ClaimedChangeKind {
    /// v59 (MED-smell-2) — canonical lowercase wire label. Matches the
    /// `#[serde(rename_all = "lowercase")]` projection. Single source
    /// of truth so the runner's projection into `ClaimedChangeSummary`
    /// and any UI string-match logic stay in sync with serde.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Create => "create",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Uncertainty {
    pub about: String,
    pub kind: UncertaintyKind,
    pub asks: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UncertaintyKind {
    MissingContext,
    AmbiguousSpec,
    UntestableClaim,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanUpdate {
    pub ops: Vec<PlanOp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanOp {
    pub op: PlanOpKind,
    pub step: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanOpKind {
    Add,
    Remove,
    Reorder,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grounding {
    pub text_span: String,
    pub source: GroundingSource,
}

/// `tool:read` / `tool:grep` / `context:file` / `guess`. JSON renders with a
/// colon, so a custom `serde` rename is needed (the default identifier rules
/// don't permit `:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GroundingSource {
    #[serde(rename = "tool:read")]
    ToolRead,
    #[serde(rename = "tool:grep")]
    ToolGrep,
    #[serde(rename = "context:file")]
    ContextFile,
    #[serde(rename = "guess")]
    Guess,
}

/// Validation errors over an already-deserialized envelope. Schema-shape
/// errors (unknown fields, wrong enum values) surface as `serde` errors
/// before we get here; this catches the conditional invariants the JSON
/// schema cannot express (e.g., `summary` length).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvelopeError {
    #[error("claimed_changes[{index}].summary length {len} exceeds 500-char schema cap")]
    SummaryTooLong { index: usize, len: usize },

    #[error("envelope parse error: {0}")]
    Parse(String),
}

impl Envelope {
    /// Round-trip through `serde_json::from_slice` plus the schema-shape
    /// invariants we can't express in pure JSON Schema.
    pub fn from_json(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let env: Self =
            serde_json::from_slice(bytes).map_err(|e| EnvelopeError::Parse(e.to_string()))?;
        env.validate()?;
        Ok(env)
    }

    /// As above but from a `serde_json::Value` (used by the JSON-mode
    /// sentinel strategy and tests).
    pub fn from_value(v: serde_json::Value) -> Result<Self, EnvelopeError> {
        let env: Self =
            serde_json::from_value(v).map_err(|e| EnvelopeError::Parse(e.to_string()))?;
        env.validate()?;
        Ok(env)
    }

    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if let Some(changes) = &self.claimed_changes {
            for (i, c) in changes.iter().enumerate() {
                if c.summary.len() > 500 {
                    return Err(EnvelopeError::SummaryTooLong {
                        index: i,
                        len: c.summary.len(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Convenience: was this turn making edits? Used by §7's "did-it-do-
    /// what-it-said" gate to decide whether to run the diff comparison.
    pub fn has_edits(&self) -> bool {
        self.claimed_changes
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Convenience: did the model raise uncertainty this turn?
    pub fn has_uncertainty(&self) -> bool {
        self.uncertainty
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_change_kind_wire_label_agrees_with_serde() {
        // Regression for v59 MED-smell-2 — pin `wire_label` to the
        // `#[serde(rename_all = "lowercase")]` projection. Single
        // source of truth across the runner's bus projection,
        // verify::kind_label, and any UI string-match logic.
        for k in [
            ClaimedChangeKind::Edit,
            ClaimedChangeKind::Create,
            ClaimedChangeKind::Delete,
        ] {
            let json = serde_json::to_value(k).unwrap();
            let serde_label = json
                .as_str()
                .expect("ClaimedChangeKind serializes as a string");
            assert_eq!(serde_label, k.wire_label());
        }
    }

    #[test]
    fn round_trips_minimal_edit_example_from_fewshot() {
        // Mirrors prompts/protocol_fewshot/minimal_edit.md
        let raw = serde_json::json!({
            "claimed_changes": [
                {"path": "utils.py", "kind": "edit", "summary": "Renamed foo to bar"}
            ],
            "grounding": [
                {"text_span": "one definition of foo, no callers in this file", "source": "tool:read"}
            ]
        });
        let env = Envelope::from_value(raw.clone()).unwrap();
        assert_eq!(env.claimed_changes.as_ref().unwrap().len(), 1);
        assert_eq!(
            env.claimed_changes.as_ref().unwrap()[0].kind,
            ClaimedChangeKind::Edit
        );
        assert_eq!(env.claimed_done, None);
        assert_eq!(
            env.grounding.as_ref().unwrap()[0].source,
            GroundingSource::ToolRead
        );
        let back = serde_json::to_value(&env).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn round_trips_completion_example_from_fewshot() {
        let raw = serde_json::json!({
            "claimed_changes": [
                {"path": "mycli.py", "kind": "edit", "summary": "Added --verbose argparse flag and verbose-prefix in main()"},
                {"path": "tests/test_mycli.py", "kind": "edit", "summary": "Added test_verbose_off and test_verbose_on"}
            ],
            "claimed_done": true,
            "grounding": [
                {"text_span": "build_parser already separated from main", "source": "tool:read"},
                {"text_span": "no callers of main() outside tests/", "source": "tool:grep"}
            ]
        });
        let env = Envelope::from_value(raw.clone()).unwrap();
        assert_eq!(env.claimed_done, Some(true));
        assert!(env.has_edits());
        assert_eq!(
            env.grounding.as_ref().unwrap()[1].source,
            GroundingSource::ToolGrep
        );
        let back = serde_json::to_value(&env).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn round_trips_uncertainty_example_from_fewshot() {
        let raw = serde_json::json!({
            "uncertainty": [
                {
                    "about": "cache invalidation policy",
                    "kind": "ambiguous-spec",
                    "asks": "Should cached entries expire on a TTL, drop on explicit invalidation, or be unbounded? The three callers have different needs."
                }
            ],
            "grounding": [
                {"text_span": "lookup is called from api.py, worker.py, cli.py", "source": "tool:grep"}
            ]
        });
        let env = Envelope::from_value(raw.clone()).unwrap();
        assert!(env.has_uncertainty());
        assert!(!env.has_edits());
        assert_eq!(
            env.uncertainty.as_ref().unwrap()[0].kind,
            UncertaintyKind::AmbiguousSpec
        );
        let back = serde_json::to_value(&env).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let raw = r#"{"claimed_changes": [], "made_up_field": true}"#;
        let err = Envelope::from_json(raw.as_bytes()).unwrap_err();
        assert!(matches!(err, EnvelopeError::Parse(_)));
    }

    #[test]
    fn rejects_unknown_grounding_source() {
        let raw = r#"{"grounding": [{"text_span": "x", "source": "not-a-source"}]}"#;
        let err = Envelope::from_json(raw.as_bytes()).unwrap_err();
        assert!(matches!(err, EnvelopeError::Parse(_)));
    }

    #[test]
    fn rejects_unknown_uncertainty_kind() {
        let raw = r#"{"uncertainty": [{"about": "x", "kind": "made-up", "asks": "y"}]}"#;
        let err = Envelope::from_json(raw.as_bytes()).unwrap_err();
        assert!(matches!(err, EnvelopeError::Parse(_)));
    }

    #[test]
    fn rejects_summary_exceeding_500_chars() {
        let long = "x".repeat(501);
        let raw = serde_json::json!({
            "claimed_changes": [{"path": "a", "kind": "edit", "summary": long}]
        });
        let err = Envelope::from_value(raw).unwrap_err();
        assert!(matches!(
            err,
            EnvelopeError::SummaryTooLong { index: 0, len: 501 }
        ));
    }

    #[test]
    fn accepts_summary_at_exactly_500_chars() {
        let edge = "x".repeat(500);
        let raw = serde_json::json!({
            "claimed_changes": [{"path": "a", "kind": "edit", "summary": edge}]
        });
        let env = Envelope::from_value(raw).unwrap();
        assert_eq!(env.claimed_changes.unwrap()[0].summary.len(), 500);
    }

    #[test]
    fn empty_envelope_serializes_as_empty_object() {
        let env = Envelope::default();
        assert_eq!(serde_json::to_value(&env).unwrap(), serde_json::json!({}));
        assert!(!env.has_edits());
        assert!(!env.has_uncertainty());
    }

    #[test]
    fn plan_update_round_trips() {
        let raw = serde_json::json!({
            "plan_update": {
                "ops": [
                    {"op": "add", "step": "write the parser"},
                    {"op": "complete", "step": "write the lexer"}
                ]
            }
        });
        let env = Envelope::from_value(raw.clone()).unwrap();
        assert_eq!(env.plan_update.as_ref().unwrap().ops.len(), 2);
        assert_eq!(env.plan_update.as_ref().unwrap().ops[0].op, PlanOpKind::Add);
        assert_eq!(serde_json::to_value(&env).unwrap(), raw);
    }

    #[test]
    fn claimed_change_kinds_all_round_trip() {
        for (lower, k) in [
            ("edit", ClaimedChangeKind::Edit),
            ("create", ClaimedChangeKind::Create),
            ("delete", ClaimedChangeKind::Delete),
        ] {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{lower}\""));
        }
    }

    #[test]
    fn grounding_sources_all_round_trip() {
        for (s, src) in [
            ("tool:read", GroundingSource::ToolRead),
            ("tool:grep", GroundingSource::ToolGrep),
            ("context:file", GroundingSource::ContextFile),
            ("guess", GroundingSource::Guess),
        ] {
            let json = serde_json::to_string(&src).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: GroundingSource = serde_json::from_str(&json).unwrap();
            assert_eq!(back, src);
        }
    }

    #[test]
    fn constraints_acknowledged_round_trips() {
        let raw = serde_json::json!({
            "constraints_acknowledged": ["no new deps", "preserve api"]
        });
        let env = Envelope::from_value(raw.clone()).unwrap();
        assert_eq!(
            env.constraints_acknowledged.as_ref().unwrap(),
            &vec!["no new deps".to_string(), "preserve api".to_string()]
        );
        assert_eq!(serde_json::to_value(&env).unwrap(), raw);
    }
}
