//! §1 typed cost ledger.
//!
//! Spec §1 "Cost ledger":
//!   Every call records: prompt / completion / cached tokens, latency,
//!   model ID, `$` cost. Local cost is latency-weighted:
//!   `wall_clock_seconds × local_rate`. **`local_rate` defaults to
//!   `$0.00028/sec`** (PROVISIONAL).
//!
//! Lessons captured during spec evolution:
//!   "Cache-bust cost is invisible unless ledgered."
//!
//! Schema: `schemas/session/v1.json` `cost_ledger[]` — three kinds
//! (`model_call`, `tool_call`, `cache_bust`) with per-kind required fields
//! enforced by the schema's `allOf / if / then`. This module mirrors the
//! shape as a Rust enum so callers cannot construct a `tool_call` without
//! `tool_name`, an `model_call` without `prompt_tokens`, etc.
//!
//! [`Ledger`] is append-only and lock-free for readers (via `RwLock`); the
//! adapter / dispatcher / context manager / verification gate each take a
//! `&Ledger` and append entries directly. Persistence to disk is the
//! responsibility of [`crate::persistence::OnDiskSession`] — the ledger
//! itself stays in-memory until that snapshot writes.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::context::TokenSource;

/// PROVISIONAL — spec §1. Default local cost per wall-clock second when the
/// adapter is local (Ollama / llama.cpp / MLX-LM). Derived as a cloud A100
/// hourly rate / 3600. User can override per-session.
pub const DEFAULT_LOCAL_RATE_USD_PER_SEC: f64 = 0.000_28;

/// Per-kind required fields enforced by the type system: a `ModelCall` can't
/// be constructed without `model_id`/`prompt_tokens`/etc., and a `ToolCall`
/// can't be constructed without `tool_name`/`latency_ms`. Matches the
/// schema's `allOf / if / then` exactly so on-disk JSON round-trips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerEntry {
    ModelCall {
        timestamp: String,
        model_id: String,
        prompt_tokens: u32,
        completion_tokens: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cached_tokens: Option<u32>,
        count_source: TokenSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latency_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    ToolCall {
        timestamp: String,
        tool_name: String,
        latency_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    CacheBust {
        timestamp: String,
        /// Required by the schema — the `note` is the explanation surfaced
        /// in the UI (`evicted context-item: <kind/provenance>`, or
        /// `ContextOverflowError -> <chosen action>`, etc.).
        note: String,
    },
}

impl LedgerEntry {
    pub fn timestamp(&self) -> &str {
        match self {
            Self::ModelCall { timestamp, .. }
            | Self::ToolCall { timestamp, .. }
            | Self::CacheBust { timestamp, .. } => timestamp,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::ModelCall { .. } => Kind::ModelCall,
            Self::ToolCall { .. } => Kind::ToolCall,
            Self::CacheBust { .. } => Kind::CacheBust,
        }
    }

    pub fn cost_usd(&self) -> Option<f64> {
        match self {
            Self::ModelCall { cost_usd, .. } | Self::ToolCall { cost_usd, .. } => *cost_usd,
            Self::CacheBust { .. } => None,
        }
    }

    /// Helper: build a `ToolCall` entry. Latency is the wall-clock cost of
    /// running the tool inside the §11 sandbox; `cost_usd` is the
    /// latency-weighted local cost the §1 rate produces. The dispatcher
    /// (when it lands) calls this after each tool invocation.
    pub fn tool_call(
        timestamp: impl Into<String>,
        tool_name: impl Into<String>,
        latency_ms: f64,
        cost_usd: Option<f64>,
        note: Option<String>,
    ) -> Self {
        Self::ToolCall {
            timestamp: timestamp.into(),
            tool_name: tool_name.into(),
            latency_ms,
            cost_usd,
            note,
        }
    }

    /// Helper: build a `CacheBust` entry from a context-manager
    /// [`crate::context::CacheBustEvent`]. The context manager itself
    /// doesn't import the ledger module (kept pure of I/O); the caller
    /// (dispatcher / actor) bridges via this helper.
    pub fn cache_bust_from(event: &crate::context::CacheBustEvent) -> Self {
        let prov_label = match &event.provenance {
            crate::context::Provenance::Initial => "initial",
            crate::context::Provenance::UserAttached { .. } => "user-attached",
            crate::context::Provenance::ToolResult { .. } => "tool-result",
            crate::context::Provenance::MemoryPromoted { .. } => "memory-promoted",
            crate::context::Provenance::PinnedByUser { .. } => "pinned-by-user",
            crate::context::Provenance::AssistantTurn => "assistant-turn",
        };
        Self::CacheBust {
            timestamp: event.evicted_at.clone(),
            note: format!(
                "evicted context-item {} ({}, {} tokens freed)",
                event.item_id, prov_label, event.tokens_freed
            ),
        }
    }
}

/// Discriminator type for queries / aggregations. Mirrors the schema's
/// `kind` enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    ModelCall,
    ToolCall,
    CacheBust,
}

/// Append-only typed cost ledger. **Share via `Arc<Ledger>`, not by
/// cloning** — the type doesn't `impl Clone`, and even if it did the
/// shape (a `RwLock<Vec<_>>`) makes "shallow copy" meaningless. Every
/// production consumer (`SessionDispatcher`, the §2.5 actor, the
/// in-the-future §1 adapters) holds an `Arc<Ledger>` and appends through
/// it concurrently. `parking_lot::RwLock` underneath so a panicking writer
/// can't poison the ledger and brick every later read.
#[derive(Debug, Default)]
pub struct Ledger {
    entries: RwLock<Vec<LedgerEntry>>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hydrate from a serialised list — used by
    /// [`crate::persistence::OnDiskSession::cost_ledger`] when reopening a
    /// session.
    pub fn from_vec(entries: Vec<LedgerEntry>) -> Self {
        Self {
            entries: RwLock::new(entries),
        }
    }

    /// Snapshot back to the on-disk representation (chronological insertion
    /// order; no sort).
    pub fn to_vec(&self) -> Vec<LedgerEntry> {
        self.entries.read().clone()
    }

    /// Append one entry. Cheap path; no allocation on the lock side.
    pub fn append(&self, entry: LedgerEntry) {
        self.entries.write().push(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// All entries of one kind. Returns owned clones — callers wanting a
    /// per-kind aggregate should use [`Self::total_cost_usd`] /
    /// [`Self::total_tokens`] which don't allocate.
    pub fn by_kind(&self, kind: Kind) -> Vec<LedgerEntry> {
        self.entries
            .read()
            .iter()
            .filter(|e| e.kind() == kind)
            .cloned()
            .collect()
    }

    /// Sum of recorded `cost_usd` across all kinds. Entries with no
    /// declared cost (most `CacheBust`s, model calls before pricing is
    /// known) contribute 0 — the UI surfaces "+X unknown costs" separately
    /// via [`Self::entries_without_cost`].
    pub fn total_cost_usd(&self) -> f64 {
        self.entries
            .read()
            .iter()
            .filter_map(|e| e.cost_usd())
            .sum()
    }

    /// Count of entries with no declared `cost_usd` (excluding `CacheBust`,
    /// which never has one). Lets the §3 cost meter render
    /// "$1.23 + N unknown" rather than understating the bill.
    pub fn entries_without_cost(&self) -> usize {
        self.entries
            .read()
            .iter()
            .filter(|e| !matches!(e, LedgerEntry::CacheBust { .. }))
            .filter(|e| e.cost_usd().is_none())
            .count()
    }

    /// Sum of `prompt_tokens` + `completion_tokens` across all model calls.
    /// `cached_tokens` is intentionally excluded — it's a discount line in
    /// the schema, not an additional bill.
    pub fn total_tokens(&self) -> u64 {
        self.entries
            .read()
            .iter()
            .map(|e| match e {
                LedgerEntry::ModelCall {
                    prompt_tokens,
                    completion_tokens,
                    ..
                } => (*prompt_tokens as u64) + (*completion_tokens as u64),
                _ => 0,
            })
            .sum()
    }
}

/// Latency-weighted local cost: `seconds × rate`. Returns dollars (USD).
/// Used by adapters that don't have a per-call provider cost (Ollama,
/// llama.cpp, MLX-LM); the dispatcher uses it for tool calls.
pub fn local_cost_usd(latency_ms: f64, rate_usd_per_sec: f64) -> f64 {
    (latency_ms / 1000.0) * rate_usd_per_sec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CacheBustEvent, ContextItemId, Provenance};

    fn model_call(ts: &str, prompt: u32, completion: u32, cost: Option<f64>) -> LedgerEntry {
        LedgerEntry::ModelCall {
            timestamp: ts.into(),
            model_id: "mock:test".into(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: None,
            count_source: TokenSource::Exact,
            cost_usd: cost,
            latency_ms: Some(100.0),
            note: None,
        }
    }

    fn tool_call(ts: &str, tool: &str, cost: Option<f64>) -> LedgerEntry {
        LedgerEntry::tool_call(ts, tool, 50.0, cost, None)
    }

    // ---------- entry shape ----------

    #[test]
    fn kind_discriminator_matches_schema_literals() {
        for (lit, k) in [
            ("model_call", Kind::ModelCall),
            ("tool_call", Kind::ToolCall),
            ("cache_bust", Kind::CacheBust),
        ] {
            assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{lit}\""));
        }
    }

    #[test]
    fn model_call_serializes_to_schema_shape() {
        let entry = model_call("2026-05-16T10:00:00Z", 100, 50, Some(0.012));
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["kind"], "model_call");
        assert_eq!(json["timestamp"], "2026-05-16T10:00:00Z");
        assert_eq!(json["model_id"], "mock:test");
        assert_eq!(json["prompt_tokens"], 100);
        assert_eq!(json["completion_tokens"], 50);
        assert_eq!(json["count_source"], "exact");
        assert_eq!(json["cost_usd"], 0.012);
        // Optional `cached_tokens` is omitted when None.
        assert!(json.get("cached_tokens").is_none());
    }

    #[test]
    fn tool_call_requires_latency_ms_and_tool_name() {
        let entry = tool_call("2026-05-16T10:00:01Z", "shell", None);
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["kind"], "tool_call");
        assert_eq!(json["tool_name"], "shell");
        assert_eq!(json["latency_ms"], 50.0);
    }

    #[test]
    fn cache_bust_requires_note() {
        let entry = LedgerEntry::CacheBust {
            timestamp: "2026-05-16T10:00:02Z".into(),
            note: "ContextOverflowError -> Compact".into(),
        };
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["kind"], "cache_bust");
        assert_eq!(json["note"], "ContextOverflowError -> Compact");
    }

    #[test]
    fn all_three_entry_kinds_round_trip_through_serde() {
        for entry in [
            model_call("t", 1, 2, Some(0.001)),
            tool_call("t", "read_file", Some(0.000_05)),
            LedgerEntry::CacheBust {
                timestamp: "t".into(),
                note: "x".into(),
            },
        ] {
            let json = serde_json::to_string(&entry).unwrap();
            let back: LedgerEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(back, entry);
        }
    }

    #[test]
    fn entry_helpers_expose_timestamp_kind_and_cost() {
        let m = model_call("t1", 1, 2, Some(0.5));
        assert_eq!(m.timestamp(), "t1");
        assert_eq!(m.kind(), Kind::ModelCall);
        assert_eq!(m.cost_usd(), Some(0.5));

        let cb = LedgerEntry::CacheBust {
            timestamp: "t2".into(),
            note: "n".into(),
        };
        assert_eq!(cb.kind(), Kind::CacheBust);
        assert_eq!(cb.cost_usd(), None);
    }

    #[test]
    fn cache_bust_from_context_event_renders_human_readable_note() {
        let id = ContextItemId::new();
        let ev = CacheBustEvent {
            item_id: id,
            tokens_freed: 250,
            provenance: Provenance::ToolResult {
                tool_call_id: "tc-7".into(),
            },
            evicted_at: "2026-05-16T10:00:00Z".into(),
        };
        let entry = LedgerEntry::cache_bust_from(&ev);
        match entry {
            LedgerEntry::CacheBust { timestamp, note } => {
                assert_eq!(timestamp, "2026-05-16T10:00:00Z");
                assert!(note.contains(&id.to_string()));
                assert!(note.contains("tool-result"));
                assert!(note.contains("250 tokens freed"));
            }
            other => panic!("expected CacheBust, got {other:?}"),
        }
    }

    // ---------- Ledger wrapper ----------

    #[test]
    fn append_then_iter_preserves_insertion_order() {
        let l = Ledger::new();
        l.append(model_call("t1", 1, 2, Some(0.01)));
        l.append(tool_call("t2", "read_file", Some(0.001)));
        l.append(LedgerEntry::CacheBust {
            timestamp: "t3".into(),
            note: "n".into(),
        });
        let v = l.to_vec();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].timestamp(), "t1");
        assert_eq!(v[1].timestamp(), "t2");
        assert_eq!(v[2].timestamp(), "t3");
    }

    #[test]
    fn from_vec_round_trips_to_vec() {
        let entries = vec![
            model_call("t1", 1, 2, Some(0.01)),
            tool_call("t2", "shell", None),
        ];
        let l = Ledger::from_vec(entries.clone());
        assert_eq!(l.to_vec(), entries);
    }

    #[test]
    fn by_kind_filters_correctly() {
        let l = Ledger::new();
        l.append(model_call("t1", 1, 2, Some(0.01)));
        l.append(model_call("t2", 3, 4, Some(0.02)));
        l.append(tool_call("t3", "shell", Some(0.001)));
        assert_eq!(l.by_kind(Kind::ModelCall).len(), 2);
        assert_eq!(l.by_kind(Kind::ToolCall).len(), 1);
        assert_eq!(l.by_kind(Kind::CacheBust).len(), 0);
    }

    #[test]
    fn total_cost_usd_sums_only_recorded_costs() {
        let l = Ledger::new();
        l.append(model_call("t1", 0, 0, Some(0.1)));
        l.append(model_call("t2", 0, 0, None));
        l.append(tool_call("t3", "x", Some(0.05)));
        let total: f64 = l.total_cost_usd();
        assert!((total - 0.15).abs() < 1e-9);
    }

    #[test]
    fn entries_without_cost_excludes_cache_bust() {
        let l = Ledger::new();
        l.append(model_call("t1", 0, 0, None));
        l.append(tool_call("t2", "x", None));
        l.append(LedgerEntry::CacheBust {
            timestamp: "t3".into(),
            note: "n".into(),
        });
        assert_eq!(l.entries_without_cost(), 2);
    }

    #[test]
    fn total_tokens_sums_prompt_and_completion_excluding_cached() {
        let l = Ledger::new();
        l.append(LedgerEntry::ModelCall {
            timestamp: "t1".into(),
            model_id: "m".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: Some(40),
            count_source: TokenSource::Exact,
            cost_usd: None,
            latency_ms: None,
            note: None,
        });
        l.append(tool_call("t2", "x", None));
        assert_eq!(l.total_tokens(), 150);
    }

    #[test]
    fn empty_ledger_has_zero_totals() {
        let l = Ledger::new();
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
        assert_eq!(l.total_cost_usd(), 0.0);
        assert_eq!(l.total_tokens(), 0);
        assert_eq!(l.entries_without_cost(), 0);
    }

    // ---------- local rate helper ----------

    #[test]
    fn local_cost_scales_linearly_with_latency() {
        assert_eq!(local_cost_usd(0.0, 1.0), 0.0);
        assert_eq!(local_cost_usd(1000.0, 1.0), 1.0);
        assert!(
            (local_cost_usd(1000.0, DEFAULT_LOCAL_RATE_USD_PER_SEC)
                - DEFAULT_LOCAL_RATE_USD_PER_SEC)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn default_local_rate_matches_spec_value() {
        assert!((DEFAULT_LOCAL_RATE_USD_PER_SEC - 0.000_28).abs() < 1e-9);
    }
}
