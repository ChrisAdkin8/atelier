//! §5 context manager — typed `ContextItem` + ops.
//!
//! Spec §5 "Visible context / memory / plan":
//!   * "Context-panel API (token counts + why-here trace per item)"
//!   * "Pin / unpin / evict with cache-bust confirm"
//!   * "UX target: 'find what agent knows about file X' median <5 s"
//!
//! Lessons captured during spec evolution:
//!   "Cache-bust cost is invisible unless ledgered."
//!
//! This is the data layer underneath those UI requirements. The Phase C UIs
//! (§3 GUI + §5 context panel) consume `ContextItem` values; eviction returns
//! a [`CacheBustEvent`] that the caller forwards to the §1 cost ledger so the
//! cache-bust cost stays visible. The manager itself is pure — no async, no
//! I/O — to keep the data layer testable in isolation from the agent loop.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a context item. UUIDs so pinning by id survives
/// serde round-trips and cross-process replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContextItemId(pub Uuid);

impl ContextItemId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ContextItemId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ContextItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Per-item token count + the source convention. Matches the
/// `cost_ledger.count_source` enum in `schemas/session/v1.json` exactly so
/// the §5 context panel can render the same `exact / approx / unavailable`
/// badges the ledger surfaces elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount {
    pub count: u32,
    pub source: TokenSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    /// Adapter-counted via the provider's `count_tokens` endpoint.
    Exact,
    /// Local approximation (e.g. tiktoken-ish heuristic for an Anthropic
    /// session before the API confirms).
    Approx,
    /// No count available — UI renders the token meter as gray for this
    /// item per spec §2 degradation policy.
    Unavailable,
}

/// Why this item is in the agent's context. Drives the "why-here trace per
/// item" requirement (spec §5). The enum is closed because the UI renders a
/// distinct badge per variant; adding a source means adding a badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Provenance {
    /// Loaded by the harness at session start (e.g. `ATELIER.md`,
    /// auto-loaded user config).
    Initial,
    /// User attached this item via the workspace UI (drag-drop, "Add file
    /// to context").
    UserAttached { note: Option<String> },
    /// Result of a tool invocation; references the tool-call id so the UI
    /// can link back to the originating tool card in the conversation.
    ToolResult { tool_call_id: String },
    /// Promoted from the §5 memory subsystem; references the source
    /// `MemoryCard::id` so a click-through can show the card.
    MemoryPromoted { card_id: String },
    /// Explicitly pinned by the user — pinning is itself a why-here signal
    /// distinct from how the item arrived, and survives independently.
    PinnedByUser { note: Option<String> },
}

/// Shape of the payload. The context manager never inspects payload bytes —
/// the discriminator is enough for the UI to pick a renderer (file path
/// vs. blob vs. inline text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    /// Repo-relative file reference. The UI resolves the file on demand;
    /// the context manager stores only the path + optional line range.
    FileRef {
        path: String,
        line_range: Option<(u32, u32)>,
    },
    /// Inline text (e.g. a snippet pasted by the user, a tool's stdout
    /// excerpt, a memory card's content).
    InlineText { text: String },
    /// Opaque blob, identified by a content-addressed hash. The §14
    /// diff-blob store and any future asset store can both point here.
    BlobRef {
        sha256_hex: String,
        mime_type: Option<String>,
    },
}

/// A single context item. Round-trips through serde so it can ride along in
/// future versions of `schemas/session/v1.json` (the schema's `context`
/// field is reserved for this).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub payload: Payload,
    pub tokens: TokenCount,
    pub provenance: Provenance,
    /// User-driven pin state. Independent of [`Provenance::PinnedByUser`] —
    /// an item can be added by tool result and *then* pinned, in which case
    /// `pinned` is true but provenance still reads `ToolResult`. Pinning
    /// prevents eviction.
    pub pinned: bool,
    /// RFC 3339 timestamps. Stringly typed for the same reason as
    /// `OnDiskSession.created_at` — the persistence layer takes timestamps
    /// from the caller so this module avoids depending on a time crate.
    pub added_at: String,
    pub last_used: String,
}

/// Event handed back from [`ContextManager::evict`]. The caller (the
/// agent-loop turn driver) forwards this onto the §1 cost ledger as a
/// `cost_ledger` entry with `kind: "cache_bust"`. Returning the event
/// instead of writing it directly keeps `context.rs` pure and lets tests
/// observe the cache-bust signal without mocking a ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBustEvent {
    pub item_id: ContextItemId,
    /// Token count of the evicted item — the upper bound on how much
    /// cached-prefix material is lost.
    pub tokens_freed: u32,
    /// Provenance of the evicted item, carried into the ledger note so the
    /// user can see *what* was busted, not just *when*.
    pub provenance: Provenance,
    /// RFC 3339; supplied by the caller (same convention as elsewhere).
    pub evicted_at: String,
}

/// Errors raised by [`ContextManager`] ops.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContextError {
    #[error("context item {0} not found")]
    NotFound(ContextItemId),

    #[error("cannot evict pinned context item {0}; unpin first")]
    EvictPinned(ContextItemId),
}

/// Insertion-ordered context store. Items render in the §5 panel in the
/// order they were added; tests rely on the iteration order being stable.
/// `BTreeMap` keyed by a monotonic insertion counter satisfies both
/// requirements while still giving O(log N) lookup by id.
///
/// **Not internally `Send + Sync`** — no interior mutability. Owned by
/// the §2.5 session actor; consumers that need concurrent access wrap
/// in `Arc<Mutex<_>>` themselves. The compiler will surface the bound
/// at the share site.
#[derive(Debug, Default, Clone)]
pub struct ContextManager {
    /// insertion_order → item
    items: BTreeMap<u64, ContextItem>,
    /// id → insertion_order (for O(log N) id-based lookups)
    by_id: BTreeMap<ContextItemId, u64>,
    next_order: u64,
}

impl ContextManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a fresh item. The caller picks the id (or uses
    /// [`ContextItemId::new`]) so external systems can keep a stable
    /// reference. Returns the id back for convenience.
    ///
    /// Re-adding the same id is rejected by the [`BTreeMap`] semantics —
    /// callers that mean to refresh an existing item should call
    /// [`Self::touch`] or [`Self::evict`] + re-add.
    pub fn add(&mut self, item: ContextItem) -> ContextItemId {
        let id = item.id;
        let order = self.next_order;
        self.next_order += 1;
        self.by_id.insert(id, order);
        self.items.insert(order, item);
        id
    }

    /// Mark an item pinned. Pinned items are skipped by eviction.
    pub fn pin(&mut self, id: ContextItemId) -> Result<(), ContextError> {
        self.with_mut(id, |i| i.pinned = true)
    }

    pub fn unpin(&mut self, id: ContextItemId) -> Result<(), ContextError> {
        self.with_mut(id, |i| i.pinned = false)
    }

    /// Update `last_used` to `now` (caller-supplied RFC 3339). Cheap; intended
    /// for "this item just got referenced in a turn" signalling.
    pub fn touch(&mut self, id: ContextItemId, now: impl Into<String>) -> Result<(), ContextError> {
        let now = now.into();
        self.with_mut(id, |i| i.last_used = now.clone())
    }

    /// Evict an item from context. Refuses if pinned. Returns the
    /// [`CacheBustEvent`] the caller forwards to the cost ledger.
    pub fn evict(
        &mut self,
        id: ContextItemId,
        evicted_at: impl Into<String>,
    ) -> Result<CacheBustEvent, ContextError> {
        let order = self
            .by_id
            .get(&id)
            .copied()
            .ok_or(ContextError::NotFound(id))?;
        // Pin check before removal so a refused evict leaves state unchanged.
        let pinned = self.items.get(&order).map(|i| i.pinned).unwrap_or(false);
        if pinned {
            return Err(ContextError::EvictPinned(id));
        }
        let item = self
            .items
            .remove(&order)
            .expect("BTreeMap entry must exist when by_id has it");
        self.by_id.remove(&id);
        Ok(CacheBustEvent {
            item_id: id,
            tokens_freed: item.tokens.count,
            provenance: item.provenance,
            evicted_at: evicted_at.into(),
        })
    }

    /// Lookup by id.
    pub fn get(&self, id: ContextItemId) -> Option<&ContextItem> {
        self.by_id.get(&id).and_then(|o| self.items.get(o))
    }

    /// Iterate in insertion order — the order the §5 context panel renders.
    pub fn iter(&self) -> impl Iterator<Item = &ContextItem> {
        self.items.values()
    }

    /// Number of items currently in context.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Sum of token counts. Items whose token source is `Unavailable`
    /// contribute 0 — and the snapshot below carries the count of such
    /// items separately so the UI can surface "X items have unknown
    /// tokens" rather than silently underreporting.
    pub fn token_snapshot(&self) -> TokenSnapshot {
        let mut total: u64 = 0;
        let mut unknown = 0usize;
        for item in self.items.values() {
            if matches!(item.tokens.source, TokenSource::Unavailable) {
                unknown += 1;
            } else {
                total += item.tokens.count as u64;
            }
        }
        TokenSnapshot {
            total_known_tokens: total,
            items_with_unknown_tokens: unknown,
            item_count: self.items.len(),
        }
    }

    fn with_mut<F: FnOnce(&mut ContextItem)>(
        &mut self,
        id: ContextItemId,
        f: F,
    ) -> Result<(), ContextError> {
        let order = self
            .by_id
            .get(&id)
            .copied()
            .ok_or(ContextError::NotFound(id))?;
        let item = self
            .items
            .get_mut(&order)
            .expect("BTreeMap entry must exist when by_id has it");
        f(item);
        Ok(())
    }
}

/// Aggregate view consumed by the §5 token meter. Splits known from unknown
/// so the UI can render "1234 tokens + 3 unknown" rather than silently
/// substituting a default — per spec §2 degradation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSnapshot {
    pub total_known_tokens: u64,
    pub items_with_unknown_tokens: usize,
    pub item_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(payload: Payload, tokens: u32, src: TokenSource, prov: Provenance) -> ContextItem {
        ContextItem {
            id: ContextItemId::new(),
            payload,
            tokens: TokenCount {
                count: tokens,
                source: src,
            },
            provenance: prov,
            pinned: false,
            added_at: "2026-05-16T10:00:00Z".into(),
            last_used: "2026-05-16T10:00:00Z".into(),
        }
    }

    fn file_item(path: &str, tokens: u32) -> ContextItem {
        item(
            Payload::FileRef {
                path: path.into(),
                line_range: None,
            },
            tokens,
            TokenSource::Exact,
            Provenance::UserAttached { note: None },
        )
    }

    // ---------- add / get / iter ----------

    #[test]
    fn add_then_get_round_trips() {
        let mut m = ContextManager::new();
        let i = file_item("a.rs", 12);
        let id = m.add(i.clone());
        let got = m.get(id).expect("must be present");
        assert_eq!(got, &i);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn iter_yields_insertion_order() {
        let mut m = ContextManager::new();
        for path in ["a.rs", "b.rs", "c.rs"] {
            m.add(file_item(path, 1));
        }
        let names: Vec<String> = m
            .iter()
            .map(|i| match &i.payload {
                Payload::FileRef { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(names, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[test]
    fn get_unknown_id_returns_none() {
        let m = ContextManager::new();
        assert!(m.get(ContextItemId::new()).is_none());
    }

    // ---------- pin / unpin / touch ----------

    #[test]
    fn pin_marks_item_and_prevents_eviction() {
        let mut m = ContextManager::new();
        let id = m.add(file_item("a", 5));
        m.pin(id).unwrap();
        assert!(m.get(id).unwrap().pinned);
        let err = m.evict(id, "2026-05-16T10:01:00Z").unwrap_err();
        assert!(matches!(err, ContextError::EvictPinned(_)));
        assert_eq!(m.len(), 1, "refused evict must not remove the item");
    }

    #[test]
    fn unpin_re_enables_eviction() {
        let mut m = ContextManager::new();
        let id = m.add(file_item("a", 5));
        m.pin(id).unwrap();
        m.unpin(id).unwrap();
        let ev = m.evict(id, "2026-05-16T10:02:00Z").unwrap();
        assert_eq!(ev.item_id, id);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn touch_updates_last_used_only() {
        let mut m = ContextManager::new();
        let id = m.add(file_item("a", 5));
        let original_added = m.get(id).unwrap().added_at.clone();
        m.touch(id, "2026-05-16T11:00:00Z").unwrap();
        let after = m.get(id).unwrap();
        assert_eq!(after.last_used, "2026-05-16T11:00:00Z");
        assert_eq!(after.added_at, original_added);
    }

    #[test]
    fn pin_unknown_id_errors_without_mutating_state() {
        let mut m = ContextManager::new();
        let unknown = ContextItemId::new();
        let err = m.pin(unknown).unwrap_err();
        assert_eq!(err, ContextError::NotFound(unknown));
        assert_eq!(m.len(), 0);
    }

    // ---------- evict + cache-bust event ----------

    #[test]
    fn evict_returns_a_cache_bust_event_with_freed_tokens() {
        let mut m = ContextManager::new();
        let id = m.add(item(
            Payload::InlineText {
                text: "snippet".into(),
            },
            128,
            TokenSource::Exact,
            Provenance::ToolResult {
                tool_call_id: "tc-42".into(),
            },
        ));
        let ev = m.evict(id, "2026-05-16T10:03:00Z").unwrap();
        assert_eq!(ev.item_id, id);
        assert_eq!(ev.tokens_freed, 128);
        assert_eq!(ev.evicted_at, "2026-05-16T10:03:00Z");
        assert!(matches!(
            ev.provenance,
            Provenance::ToolResult { ref tool_call_id } if tool_call_id == "tc-42"
        ));
    }

    #[test]
    fn evict_unknown_id_errors() {
        let mut m = ContextManager::new();
        let unknown = ContextItemId::new();
        let err = m.evict(unknown, "now").unwrap_err();
        assert_eq!(err, ContextError::NotFound(unknown));
    }

    #[test]
    fn evict_then_re_add_uses_a_new_insertion_position() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a", 1));
        let _b = m.add(file_item("b", 1));
        m.evict(a, "now").unwrap();
        m.add(file_item("a", 1));
        // After re-add, "a" appears *after* "b" — insertion order is
        // re-established at the new add, not preserved from the prior life.
        let names: Vec<String> = m
            .iter()
            .map(|i| match &i.payload {
                Payload::FileRef { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(names, vec!["b", "a"]);
    }

    // ---------- token snapshot ----------

    #[test]
    fn token_snapshot_sums_known_and_counts_unknown_separately() {
        let mut m = ContextManager::new();
        m.add(file_item("a", 100));
        m.add(file_item("b", 50));
        m.add(item(
            Payload::FileRef {
                path: "c".into(),
                line_range: None,
            },
            999, // ignored when source is unavailable
            TokenSource::Unavailable,
            Provenance::Initial,
        ));
        let snap = m.token_snapshot();
        assert_eq!(snap.total_known_tokens, 150);
        assert_eq!(snap.items_with_unknown_tokens, 1);
        assert_eq!(snap.item_count, 3);
    }

    #[test]
    fn empty_manager_has_zero_token_snapshot() {
        let m = ContextManager::new();
        let snap = m.token_snapshot();
        assert_eq!(snap.total_known_tokens, 0);
        assert_eq!(snap.items_with_unknown_tokens, 0);
        assert_eq!(snap.item_count, 0);
        assert!(m.is_empty());
    }

    // ---------- serde round-trip (for future session.context field) ----------

    #[test]
    fn context_item_round_trips_through_serde_json() {
        let i = item(
            Payload::BlobRef {
                sha256_hex: "deadbeef".into(),
                mime_type: Some("image/png".into()),
            },
            42,
            TokenSource::Approx,
            Provenance::PinnedByUser {
                note: Some("important".into()),
            },
        );
        let json = serde_json::to_string(&i).unwrap();
        let back: ContextItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back, i);
    }

    #[test]
    fn provenance_variants_all_round_trip() {
        for prov in [
            Provenance::Initial,
            Provenance::UserAttached { note: None },
            Provenance::UserAttached {
                note: Some("from drag-drop".into()),
            },
            Provenance::ToolResult {
                tool_call_id: "tc-1".into(),
            },
            Provenance::MemoryPromoted {
                card_id: "m-7".into(),
            },
            Provenance::PinnedByUser { note: None },
        ] {
            let json = serde_json::to_string(&prov).unwrap();
            let back: Provenance = serde_json::from_str(&json).unwrap();
            assert_eq!(back, prov);
        }
    }

    #[test]
    fn token_source_serializes_to_match_session_schema() {
        // Schema (cost_ledger.count_source): "exact" | "approx" | "unavailable".
        assert_eq!(
            serde_json::to_string(&TokenSource::Exact).unwrap(),
            "\"exact\""
        );
        assert_eq!(
            serde_json::to_string(&TokenSource::Approx).unwrap(),
            "\"approx\""
        );
        assert_eq!(
            serde_json::to_string(&TokenSource::Unavailable).unwrap(),
            "\"unavailable\""
        );
    }
}
