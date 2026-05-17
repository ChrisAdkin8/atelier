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
///
/// **Wire-label discipline (v58, MED-smell-1 fix)**: the snake_case
/// label used everywhere — `ContextItemSummary::from_item`, the GUI
/// badge map, the TUI badge map — comes from
/// [`Provenance::wire_label`]. The `#[serde(rename_all = "snake_case")]`
/// projection produces the same strings; a unit test below pins the
/// agreement so a future variant rename can't drift.
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
    /// The assistant's own past turn. Not pinnable in the usual sense
    /// (you don't pin your own output), but counts toward the context
    /// window and so appears in the §5 panel for honest token attribution.
    AssistantTurn,
}

impl Provenance {
    /// v58 (MED-smell-1) — canonical snake_case label, matching the
    /// `#[serde(rename_all = "snake_case")]` projection. Single
    /// source of truth across `ContextItemSummary::from_item`, GUI
    /// `ContextPane.svelte` badge map, and TUI `provenance_badge`.
    pub fn wire_label(&self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::UserAttached { .. } => "user_attached",
            Self::ToolResult { .. } => "tool_result",
            Self::MemoryPromoted { .. } => "memory_promoted",
            Self::PinnedByUser { .. } => "pinned_by_user",
            Self::AssistantTurn => "assistant_turn",
        }
    }
}

impl Payload {
    /// v58 (MED-smell-1) — canonical snake_case kind label, matching
    /// the `#[serde(tag = "kind", rename_all = "snake_case")]`
    /// projection.
    pub fn wire_label(&self) -> &'static str {
        match self {
            Self::FileRef { .. } => "file_ref",
            Self::InlineText { .. } => "inline_text",
            Self::BlobRef { .. } => "blob_ref",
        }
    }
}

impl TokenSource {
    /// v58 (MED-smell-2) — canonical lowercase label, matching the
    /// `#[serde(rename_all = "lowercase")]` projection.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Approx => "approx",
            Self::Unavailable => "unavailable",
        }
    }
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

/// v53 — flat projection of a [`ContextItem`] for the §5 Context
/// panel. Built by [`ContextManager::summarise`]; broadcast on the
/// bus via `Event::ContextItems`; consumed by the GUI + TUI.
///
/// The shape is intentionally string-typed (kind / provenance /
/// token_source as `String`) so the JSON projection in
/// `atelier-gui/src/lib.rs::bridge_event` can ship straight through
/// to the webview without a second mapping layer. The `_detail`
/// field carries the variant-specific payload (tool_call_id, card_id,
/// note) for provenances that have one; `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItemSummary {
    /// UUID of the item, as a string. Lets the UI correlate
    /// successive snapshots — same id across re-emits = same item.
    pub id: String,
    /// Payload kind label: `"file_ref"`, `"inline_text"`, `"blob_ref"`.
    pub kind: String,
    /// Short human-readable label. File path for `FileRef`, a
    /// truncated first line for `InlineText`, a `sha256:abcd…` prefix
    /// for `BlobRef`.
    pub label: String,
    /// Provenance label: `"initial"`, `"user_attached"`,
    /// `"tool_result"`, `"memory_promoted"`, `"pinned_by_user"`.
    pub provenance: String,
    /// Optional provenance detail — tool-call id, memory-card id,
    /// or the user-supplied note. `None` for `Initial`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_detail: Option<String>,
    /// Token count this item contributes to the context window.
    pub tokens: u32,
    /// Source of the count: `"exact"` / `"approx"` / `"unavailable"`.
    pub token_source: String,
    /// `true` iff the user has explicitly pinned this item.
    pub pinned: bool,
}

impl ContextItemSummary {
    /// Build a summary from a `ContextItem`. Caps the inline-text
    /// label at 80 characters so long pastes don't dominate the
    /// pane; the full payload remains on the actor side.
    ///
    /// v58 (MED-smell-1+2) — all the string fields now route through
    /// `*::wire_label()` instead of hand-typed strings.
    pub fn from_item(item: &ContextItem) -> Self {
        let kind = item.payload.wire_label().to_string();
        let label = match &item.payload {
            Payload::FileRef { path, line_range } => match line_range {
                Some((s, e)) => format!("{path}:{s}-{e}"),
                None => path.clone(),
            },
            Payload::InlineText { text } => {
                let first_line = text.lines().next().unwrap_or("");
                let truncated: String = first_line.chars().take(80).collect();
                if truncated.chars().count() < first_line.chars().count() {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            }
            Payload::BlobRef {
                sha256_hex,
                mime_type,
            } => {
                let prefix: String = sha256_hex.chars().take(8).collect();
                match mime_type {
                    Some(m) => format!("sha256:{prefix}… ({m})"),
                    None => format!("sha256:{prefix}…"),
                }
            }
        };
        let provenance = item.provenance.wire_label().to_string();
        let provenance_detail = match &item.provenance {
            Provenance::Initial | Provenance::AssistantTurn => None,
            Provenance::UserAttached { note } | Provenance::PinnedByUser { note } => note.clone(),
            Provenance::ToolResult { tool_call_id } => Some(tool_call_id.clone()),
            Provenance::MemoryPromoted { card_id } => Some(card_id.clone()),
        };
        let token_source = item.tokens.source.wire_label().to_string();
        Self {
            id: item.id.0.to_string(),
            kind,
            label,
            provenance,
            provenance_detail,
            tokens: item.tokens.count,
            token_source,
            pinned: item.pinned,
        }
    }
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

    /// v57 (L cleanup) — the wire-format id wasn't a valid UUID.
    /// Pre-v57 the dispatcher's `parse_context_item_id` substituted
    /// a nil UUID and returned `NotFound`, which surfaced as
    /// "context item 00000000-0000-… not found" to the user — a
    /// misleading error for what is actually a typo / malformed
    /// input at the API boundary.
    #[error("malformed context item id {0:?}")]
    Malformed(String),

    /// v60.6 — `add_batch` refuses to insert an item whose id is
    /// already present in the manager (or appears twice in the same
    /// input batch). The first-write-wins / silently-overwrite
    /// semantics of the underlying `BTreeMap` would otherwise turn a
    /// duplicate id into a lost item; this error makes the collision
    /// visible to the caller (the §5 Expand path, which surfaces it
    /// as "this card was already expanded, or its ids collide with
    /// items added since").
    #[error("context item {0} is already present")]
    AlreadyPresent(ContextItemId),
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

    /// v60.5 — atomic batch evict for §5 non-destructive compaction.
    ///
    /// Behaviour:
    ///
    /// * Pass 1 verifies every id is present and unpinned. If any id is
    ///   missing or pinned, returns `Err(...)` with the offending id and
    ///   leaves state unchanged.
    /// * Pass 2 evicts in input order, returning the `CacheBustEvent`s
    ///   in the same order. Each event mirrors `evict`'s output so the
    ///   caller can ledger them individually if needed.
    /// * Empty input is an idempotent `Ok(vec![])` — callers don't have
    ///   to special-case the "user selected nothing" branch.
    ///
    /// Distinct from looping `evict` in two ways:
    ///
    /// 1. **Atomicity**: a pinned item in the middle of the list would
    ///    otherwise stop the loop with N − k items already gone.
    /// 2. **Duplicate detection**: a duplicated id in the input is
    ///    caught at Pass 1 (the second copy hits `NotFound` after the
    ///    first eviction).
    pub fn evict_batch(
        &mut self,
        ids: &[ContextItemId],
        evicted_at: impl Into<String>,
    ) -> Result<Vec<CacheBustEvent>, ContextError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Pass 1: validate every id exists and is unpinned. Also rejects
        // duplicates because the second copy still has to survive Pass 1
        // before Pass 2 runs.
        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            if !seen.insert(*id) {
                return Err(ContextError::NotFound(*id));
            }
            let order = self
                .by_id
                .get(id)
                .copied()
                .ok_or(ContextError::NotFound(*id))?;
            if self.items.get(&order).is_some_and(|i| i.pinned) {
                return Err(ContextError::EvictPinned(*id));
            }
        }
        // Pass 2: evict in input order, share the same evicted_at across
        // every event so they all carry the same timestamp.
        let evicted_at: String = evicted_at.into();
        let mut events = Vec::with_capacity(ids.len());
        for id in ids {
            let order = self.by_id.remove(id).expect("Pass 1 guarantees presence");
            let item = self.items.remove(&order).expect("by_id and items in sync");
            events.push(CacheBustEvent {
                item_id: *id,
                tokens_freed: item.tokens.count,
                provenance: item.provenance,
                evicted_at: evicted_at.clone(),
            });
        }
        Ok(events)
    }

    /// v60.6 — atomic batch insert for §5 Expand. Mirror of
    /// [`Self::evict_batch`]'s discipline:
    ///
    /// * Pass 1 verifies that every item's id is fresh (not already in
    ///   the manager) and that no two items in the input share an id.
    ///   If any check fails, returns `Err(...)` with the offending id
    ///   and leaves state unchanged.
    /// * Pass 2 inserts each item in input order, mirroring
    ///   [`Self::add`]'s `next_order`-incrementing behaviour so the
    ///   restored items appear at the tail of the manager (rather than
    ///   reclaiming their original insertion slots, which would
    ///   reshuffle the §5 panel relative to anything added since the
    ///   compaction).
    /// * Empty input is `Ok(())` — callers don't have to special-case
    ///   the "nothing to restore" branch.
    ///
    /// Used by `SessionDispatcher::expand_memory_card` to replay the
    /// blob's items back into context atomically: if even one id
    /// collides with a currently-present item, the whole expansion is
    /// refused (the caller surfaces the error to the user — typically
    /// "this card was already expanded, or another card with the same
    /// item ids was restored").
    pub fn add_batch(&mut self, items: Vec<ContextItem>) -> Result<(), ContextError> {
        if items.is_empty() {
            return Ok(());
        }
        // Pass 1: validate. A duplicate within the input batch trips
        // the same error as a collision with existing state — the
        // second occurrence of an id would (in Pass 2) overwrite the
        // first via `by_id.insert`, which is precisely the bug
        // `add_batch`'s atomicity is designed to prevent.
        let mut seen = std::collections::BTreeSet::new();
        for item in &items {
            if !seen.insert(item.id) {
                return Err(ContextError::AlreadyPresent(item.id));
            }
            if self.by_id.contains_key(&item.id) {
                return Err(ContextError::AlreadyPresent(item.id));
            }
        }
        // Pass 2: insert in input order.
        for item in items {
            let id = item.id;
            let order = self.next_order;
            self.next_order += 1;
            self.by_id.insert(id, order);
            self.items.insert(order, item);
        }
        Ok(())
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

    /// v53 — projection for the §5 Context panel bus event. Each
    /// item materialises into a [`ContextItemSummary`] with a short
    /// human label, a token count + source, and a why-here trace
    /// (provenance + optional detail). Insertion order preserved so
    /// the panel renders chronologically.
    ///
    /// Distinct from [`token_snapshot`](Self::token_snapshot): that
    /// gives the aggregate meter denominator; this gives the per-row
    /// data the panel actually shows. The two are emitted at the
    /// same turn boundary so the meter and the rows stay coherent.
    pub fn summarise(&self) -> Vec<ContextItemSummary> {
        self.items
            .values()
            .map(ContextItemSummary::from_item)
            .collect()
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
            Provenance::AssistantTurn,
        ] {
            let json = serde_json::to_string(&prov).unwrap();
            let back: Provenance = serde_json::from_str(&json).unwrap();
            assert_eq!(back, prov);
        }
    }

    #[test]
    fn provenance_wire_label_agrees_with_serde_tag() {
        // Regression for MED-smell-1 — `wire_label` is the single
        // source of truth for the snake_case projection. Pin
        // agreement with the `#[serde(tag = "source", rename_all =
        // "snake_case")]` derive so a future variant rename can't
        // drift between the hand match and serde.
        for prov in [
            Provenance::Initial,
            Provenance::UserAttached { note: None },
            Provenance::ToolResult {
                tool_call_id: "tc".into(),
            },
            Provenance::MemoryPromoted {
                card_id: "m".into(),
            },
            Provenance::PinnedByUser { note: None },
            Provenance::AssistantTurn,
        ] {
            let json = serde_json::to_value(&prov).unwrap();
            let source = json
                .get("source")
                .and_then(|v| v.as_str())
                .expect("Provenance serializes with a `source` tag");
            assert_eq!(
                source,
                prov.wire_label(),
                "Provenance::{prov:?}.wire_label() must equal serde source tag"
            );
        }
    }

    #[test]
    fn payload_wire_label_agrees_with_serde_tag() {
        for p in [
            Payload::FileRef {
                path: "a".into(),
                line_range: None,
            },
            Payload::InlineText { text: "x".into() },
            Payload::BlobRef {
                sha256_hex: "abc".into(),
                mime_type: None,
            },
        ] {
            let json = serde_json::to_value(&p).unwrap();
            let kind = json
                .get("kind")
                .and_then(|v| v.as_str())
                .expect("Payload serializes with a `kind` tag");
            assert_eq!(kind, p.wire_label());
        }
    }

    #[test]
    fn token_source_wire_label_agrees_with_serde() {
        for ts in [
            TokenSource::Exact,
            TokenSource::Approx,
            TokenSource::Unavailable,
        ] {
            let json = serde_json::to_value(ts).unwrap();
            let s = json.as_str().expect("TokenSource serializes as a string");
            assert_eq!(s, ts.wire_label());
        }
    }

    #[test]
    fn summary_assistant_turn_maps_to_string_label() {
        let i = item(
            Payload::InlineText {
                text: "ok I'll start by reading parser.rs".into(),
            },
            10,
            TokenSource::Approx,
            Provenance::AssistantTurn,
        );
        let s = ContextItemSummary::from_item(&i);
        assert_eq!(s.provenance, "assistant_turn");
        assert!(s.provenance_detail.is_none());
    }

    // ---------- v53: ContextItemSummary + summarise() ----------

    #[test]
    fn summary_file_ref_uses_path_as_label() {
        let i = item(
            Payload::FileRef {
                path: "src/lib.rs".into(),
                line_range: None,
            },
            42,
            TokenSource::Exact,
            Provenance::UserAttached { note: None },
        );
        let s = ContextItemSummary::from_item(&i);
        assert_eq!(s.kind, "file_ref");
        assert_eq!(s.label, "src/lib.rs");
        assert_eq!(s.provenance, "user_attached");
        assert!(s.provenance_detail.is_none());
        assert_eq!(s.tokens, 42);
        assert_eq!(s.token_source, "exact");
    }

    #[test]
    fn summary_file_ref_with_line_range_includes_it_in_label() {
        let i = item(
            Payload::FileRef {
                path: "src/lib.rs".into(),
                line_range: Some((10, 20)),
            },
            42,
            TokenSource::Exact,
            Provenance::Initial,
        );
        let s = ContextItemSummary::from_item(&i);
        assert_eq!(s.label, "src/lib.rs:10-20");
        assert_eq!(s.provenance, "initial");
    }

    #[test]
    fn summary_inline_text_truncates_long_first_line() {
        let long = "x".repeat(200);
        let i = item(
            Payload::InlineText { text: long },
            10,
            TokenSource::Approx,
            Provenance::ToolResult {
                tool_call_id: "tc-1".into(),
            },
        );
        let s = ContextItemSummary::from_item(&i);
        assert_eq!(s.kind, "inline_text");
        // 80 chars + ellipsis.
        assert_eq!(s.label.chars().count(), 81);
        assert!(s.label.ends_with('…'));
        assert_eq!(s.provenance, "tool_result");
        assert_eq!(s.provenance_detail.as_deref(), Some("tc-1"));
    }

    #[test]
    fn summary_inline_text_short_does_not_truncate() {
        let i = item(
            Payload::InlineText {
                text: "hello world".into(),
            },
            1,
            TokenSource::Approx,
            Provenance::PinnedByUser {
                note: Some("important".into()),
            },
        );
        let s = ContextItemSummary::from_item(&i);
        assert_eq!(s.label, "hello world");
        assert_eq!(s.provenance, "pinned_by_user");
        assert_eq!(s.provenance_detail.as_deref(), Some("important"));
    }

    #[test]
    fn summary_blob_ref_uses_sha_prefix() {
        let i = item(
            Payload::BlobRef {
                sha256_hex: "deadbeef1234".into(),
                mime_type: Some("application/json".into()),
            },
            5,
            TokenSource::Unavailable,
            Provenance::MemoryPromoted {
                card_id: "card-1".into(),
            },
        );
        let s = ContextItemSummary::from_item(&i);
        assert_eq!(s.kind, "blob_ref");
        assert!(s.label.starts_with("sha256:deadbeef"));
        assert!(s.label.contains("application/json"));
        assert_eq!(s.token_source, "unavailable");
        assert_eq!(s.provenance, "memory_promoted");
        assert_eq!(s.provenance_detail.as_deref(), Some("card-1"));
    }

    #[test]
    fn summarise_preserves_insertion_order() {
        let mut m = ContextManager::new();
        m.add(file_item("a.rs", 1));
        m.add(file_item("b.rs", 2));
        m.add(file_item("c.rs", 3));
        let v = m.summarise();
        let labels: Vec<&str> = v.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[test]
    fn summary_round_trips_through_serde() {
        let i = item(
            Payload::FileRef {
                path: "x".into(),
                line_range: None,
            },
            7,
            TokenSource::Exact,
            Provenance::Initial,
        );
        let s = ContextItemSummary::from_item(&i);
        let json = serde_json::to_string(&s).unwrap();
        let back: ContextItemSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
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

    // ---------- v60.5: evict_batch ----------

    #[test]
    fn evict_batch_evicts_in_input_order_and_returns_events() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        let b = m.add(file_item("b.rs", 20));
        let c = m.add(file_item("c.rs", 30));

        let evs = m
            .evict_batch(&[c, a], "2026-05-17T11:00:00Z")
            .expect("must succeed");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].item_id, c);
        assert_eq!(evs[0].tokens_freed, 30);
        assert_eq!(evs[1].item_id, a);
        assert_eq!(evs[1].tokens_freed, 10);
        assert_eq!(evs[0].evicted_at, "2026-05-17T11:00:00Z");

        // Remaining state: only b survives.
        assert_eq!(m.len(), 1);
        assert!(m.get(b).is_some());
    }

    #[test]
    fn evict_batch_pin_check_is_all_or_nothing() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        let b = m.add(file_item("b.rs", 20));
        let c = m.add(file_item("c.rs", 30));
        m.pin(b).unwrap();

        let err = m.evict_batch(&[a, b, c], "t").unwrap_err();
        assert_eq!(err, ContextError::EvictPinned(b));
        // Crucially: nothing was evicted because Pass 1 rejected.
        assert_eq!(m.len(), 3);
        assert!(m.get(a).is_some());
        assert!(m.get(c).is_some());
    }

    #[test]
    fn evict_batch_unknown_id_rejects_and_leaves_state_unchanged() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        let ghost = ContextItemId::new();

        let err = m.evict_batch(&[a, ghost], "t").unwrap_err();
        assert_eq!(err, ContextError::NotFound(ghost));
        assert_eq!(m.len(), 1);
        assert!(m.get(a).is_some());
    }

    #[test]
    fn evict_batch_empty_is_noop() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        let evs = m.evict_batch(&[], "t").unwrap();
        assert!(evs.is_empty());
        assert_eq!(m.len(), 1);
        assert!(m.get(a).is_some());
    }

    #[test]
    fn evict_batch_rejects_duplicate_ids() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        let err = m.evict_batch(&[a, a], "t").unwrap_err();
        // The second occurrence triggers NotFound (Pass-1 dup guard).
        assert_eq!(err, ContextError::NotFound(a));
        // Nothing was evicted because Pass 1 rejected.
        assert_eq!(m.len(), 1);
    }

    // ---------- v60.6: add_batch ----------

    #[test]
    fn add_batch_inserts_in_input_order_and_appends_at_tail() {
        let mut m = ContextManager::new();
        // Pre-existing item — restored items must appear AFTER it.
        let pre = m.add(file_item("pre.rs", 5));
        let restored = vec![file_item("a.rs", 10), file_item("b.rs", 20)];
        let ids: Vec<_> = restored.iter().map(|i| i.id).collect();
        m.add_batch(restored).expect("must succeed");
        assert_eq!(m.len(), 3);

        let order: Vec<String> = m
            .iter()
            .map(|i| match &i.payload {
                Payload::FileRef { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(order, vec!["pre.rs", "a.rs", "b.rs"]);
        // Lookup by id still works.
        for id in ids {
            assert!(m.get(id).is_some());
        }
        // Pre-existing item still present.
        assert!(m.get(pre).is_some());
    }

    #[test]
    fn add_batch_rejects_collision_with_existing_item_atomically() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        // Build an input that includes a fresh item AND a collision.
        let fresh = file_item("fresh.rs", 99);
        let fresh_id = fresh.id;
        let colliding = ContextItem {
            id: a, // same id as the existing item
            payload: Payload::FileRef {
                path: "colliding.rs".into(),
                line_range: None,
            },
            tokens: TokenCount {
                count: 1,
                source: TokenSource::Exact,
            },
            provenance: Provenance::UserAttached { note: None },
            pinned: false,
            added_at: "2026-05-17T11:00:00Z".into(),
            last_used: "2026-05-17T11:00:00Z".into(),
        };
        let err = m.add_batch(vec![fresh, colliding]).unwrap_err();
        assert_eq!(err, ContextError::AlreadyPresent(a));
        // Nothing was inserted — `fresh` must NOT survive Pass-1 rejection.
        assert_eq!(m.len(), 1);
        assert!(m.get(fresh_id).is_none());
    }

    #[test]
    fn add_batch_rejects_duplicate_within_input() {
        let mut m = ContextManager::new();
        let item_a = file_item("a.rs", 10);
        let item_a_clone = item_a.clone();
        let id_a = item_a.id;
        let err = m.add_batch(vec![item_a, item_a_clone]).unwrap_err();
        assert_eq!(err, ContextError::AlreadyPresent(id_a));
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn add_batch_empty_is_noop() {
        let mut m = ContextManager::new();
        let a = m.add(file_item("a.rs", 10));
        m.add_batch(vec![]).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m.get(a).is_some());
    }

    #[test]
    fn add_batch_preserves_original_token_and_provenance() {
        let mut m = ContextManager::new();
        let item = item(
            Payload::InlineText {
                text: "summarised content".into(),
            },
            128,
            TokenSource::Exact,
            Provenance::ToolResult {
                tool_call_id: "tc-1".into(),
            },
        );
        let id = item.id;
        m.add_batch(vec![item]).unwrap();
        let got = m.get(id).unwrap();
        assert_eq!(got.tokens.count, 128);
        assert!(matches!(
            got.provenance,
            Provenance::ToolResult { ref tool_call_id } if tool_call_id == "tc-1"
        ));
    }
}
