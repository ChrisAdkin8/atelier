//! §5 typed memory subsystem.
//!
//! Spec §5 "Visible context / memory / plan":
//!   * "Memory panel: editable cards + last-used + one-click promote"
//!   * "Pin / unpin / evict with cache-bust confirm" (cache-bust applies to
//!     the §5 context manager; memory pin/unpin governs whether the
//!     compaction step is allowed to drop a card)
//!
//! Schema: `schemas/session/v1.json` `memory[]` items
//! (`{id, content, created_at, last_used, pinned?}`). This module makes that
//! field typed so the §5 memory panel consumes `MemoryCard` directly and
//! so callers can't accidentally drift away from the schema's shape.
//!
//! "Promote to global" is intentionally **pure data** — it returns the bytes
//! a caller writes to `~/.atelier/memory/<id>.md` (or wherever the harness
//! mounts global memory). Keeping the I/O out of this module mirrors
//! [`crate::context`] and lets tests assert what gets written without
//! touching disk.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Single editable memory card. Mirrors the schema's `memory[]` item shape:
/// the `id` field is opaque (assigned by whoever creates the card — usually
/// the harness via a `mem-N` scheme), so we keep it as `String` rather than
/// inventing a typed wrapper that wouldn't round-trip existing on-disk
/// sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCard {
    pub id: String,
    pub content: String,
    /// RFC 3339; supplied by the caller (same convention as
    /// `OnDiskSession.created_at`).
    pub created_at: String,
    pub last_used: String,
    /// Schema marks `pinned` optional with no default; we serialise it
    /// `false` -> absent so existing minimal-session JSON keeps round-tripping.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// v59 (LOW-2 fix) / v60 (MED-A fix) — content-byte safety for
/// cards loaded via `from_vec`. Routes through the single
/// `crate::text_safety::validate_user_text` source of truth so the
/// rule set can't drift between this snapshot-load path and the
/// live add path (dispatcher).
fn validate_card_content(content: &str) -> Result<(), MemoryError> {
    crate::text_safety::validate_user_text(content, /* check_frontmatter */ true)
        .map_err(MemoryError::InvalidContent)
}

/// Insertion-ordered store of cards. Mirrors [`crate::context::ContextManager`]
/// — `BTreeMap` keyed on a monotonic counter so iteration is stable and
/// lookup by id is `O(log N)`.
///
/// **Not internally `Send + Sync`** — owned by the §2.5 session actor.
/// Wrap in `Arc<Mutex<_>>` if external readers need concurrent access.
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    items: BTreeMap<u64, MemoryCard>,
    by_id: BTreeMap<String, u64>,
    next_order: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryError {
    #[error("memory card with id {0:?} already exists")]
    DuplicateId(String),

    #[error("memory card {0:?} not found")]
    NotFound(String),

    /// v57 (M-sec-5) — content contained a byte we won't promote
    /// safely: NUL, ASCII control characters (other than `\n`/`\t`),
    /// or a YAML frontmatter delimiter that would forge frontmatter
    /// in the promoted markdown file.
    #[error("memory card content is invalid: {0}")]
    InvalidContent(String),
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a serialised list (e.g., loaded from `OnDiskSession.memory`).
    /// Rejects duplicate ids — the schema doesn't enforce uniqueness, but a
    /// runtime store keyed by id can't tolerate it.
    ///
    /// v59 (LOW-2 from v58 audit) — applies the same content-validity
    /// rules as `SessionDispatcher::add_memory_card` so an attacker-
    /// controlled session snapshot (hand-edited `session.json`, replay
    /// of an untrusted backup) can't reintroduce NUL bytes, C1
    /// controls, Trojan-Source bidi marks, or forged YAML frontmatter
    /// delimiters that v57 / v58 closed for the live add-card path.
    pub fn from_vec(cards: Vec<MemoryCard>) -> Result<Self, MemoryError> {
        let mut store = Self::default();
        for c in cards {
            validate_card_content(&c.content)?;
            store.add(c)?;
        }
        Ok(store)
    }

    /// Snapshot back to the on-disk representation (insertion order).
    pub fn to_vec(&self) -> Vec<MemoryCard> {
        self.items.values().cloned().collect()
    }

    pub fn add(&mut self, card: MemoryCard) -> Result<(), MemoryError> {
        if self.by_id.contains_key(&card.id) {
            return Err(MemoryError::DuplicateId(card.id));
        }
        let order = self.next_order;
        self.next_order += 1;
        self.by_id.insert(card.id.clone(), order);
        self.items.insert(order, card);
        Ok(())
    }

    /// Update `last_used`. UI calls this whenever the card is surfaced to
    /// the model so the memory panel can sort "least-recently-used" sensibly.
    pub fn touch(&mut self, id: &str, now: impl Into<String>) -> Result<(), MemoryError> {
        let now = now.into();
        self.with_mut(id, |c| c.last_used = now.clone())
    }

    pub fn pin(&mut self, id: &str) -> Result<(), MemoryError> {
        self.with_mut(id, |c| c.pinned = true)
    }

    pub fn unpin(&mut self, id: &str) -> Result<(), MemoryError> {
        self.with_mut(id, |c| c.pinned = false)
    }

    /// Drop the card. Caller is responsible for surfacing an "are you sure?"
    /// confirm in the UI for pinned cards (spec §5: pin/unpin/evict with
    /// cache-bust confirm). Unlike the §5 context manager, evicting a
    /// memory card does **not** bust the prompt cache — memory lives outside
    /// the per-turn prompt prefix until promoted into context.
    pub fn evict(&mut self, id: &str) -> Result<MemoryCard, MemoryError> {
        let order = self
            .by_id
            .remove(id)
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        Ok(self.items.remove(&order).expect("by_id and items in sync"))
    }

    pub fn get(&self, id: &str) -> Option<&MemoryCard> {
        self.by_id.get(id).and_then(|o| self.items.get(o))
    }

    /// One-click promote (spec §5) — returns the bytes + relative filename
    /// the caller writes under the global memory dir
    /// (`~/.atelier/memory/<filename>`). Format follows the markdown +
    /// frontmatter convention used elsewhere in the project memory system.
    /// The card itself is unchanged in the store — promotion is additive.
    pub fn promote_to_global(&self, id: &str) -> Result<PromoteOutput, MemoryError> {
        let card = self
            .get(id)
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        let body = format!(
            "---\nname: {id}\ndescription: promoted from session memory\nmetadata:\n  type: promoted\n  created_at: {created}\n---\n\n{content}\n",
            id = card.id,
            created = card.created_at,
            content = card.content,
        );
        Ok(PromoteOutput {
            relative_path: format!("{}.md", sanitize_filename(&card.id)),
            bytes: body.into_bytes(),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &MemoryCard> {
        self.items.values()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// v54 — projection for the §5 Memory panel bus event. Each
    /// card materialises into a [`MemoryCardSummary`] with a short
    /// title (first line of `content`), a truncated body preview,
    /// and the lifecycle fields the panel renders (`created_at`,
    /// `last_used`, `pinned`).
    ///
    /// Distinct from [`Self::to_vec`]: that gives the full cards
    /// for on-disk round-trip; this gives the UI-friendly
    /// projection for the per-row panel. Insertion order preserved
    /// so the panel renders chronologically.
    pub fn summarise(&self) -> Vec<MemoryCardSummary> {
        self.items
            .values()
            .map(MemoryCardSummary::from_card)
            .collect()
    }

    fn with_mut<F: FnOnce(&mut MemoryCard)>(&mut self, id: &str, f: F) -> Result<(), MemoryError> {
        let order = *self
            .by_id
            .get(id)
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        f(self.items.get_mut(&order).expect("by_id and items in sync"));
        Ok(())
    }
}

/// Output of [`MemoryStore::promote_to_global`]. The caller is responsible
/// for the actual write so the file-system policy lives in one place
/// (`atelier-cli` for now, or the host harness eventually).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteOutput {
    pub relative_path: String,
    pub bytes: Vec<u8>,
}

/// v54 — flat projection of a [`MemoryCard`] for the §5 Memory
/// panel. Built by [`MemoryStore::summarise`]; broadcast on the
/// bus via `Event::MemoryCards`; consumed by the GUI + TUI.
///
/// The shape mirrors [`crate::context::ContextItemSummary`] in
/// spirit: string-typed and self-describing so the wire format is
/// directly renderable. Title + body preview are derived from
/// `content`: the first line is the title (markdown convention),
/// the remainder (capped at 200 chars) is the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCardSummary {
    /// Card id, opaque to the panel — used as a stable React-style
    /// key for diffed re-renders.
    pub id: String,
    /// First non-empty line of `content`. Empty string when the
    /// card has no content at all (degenerate case; the schema
    /// allows it but the panel renders a placeholder).
    pub title: String,
    /// Remainder of `content` after the title, capped at
    /// [`MEMORY_BODY_PREVIEW_CHARS`] characters with a trailing
    /// ellipsis when truncated. Lets the panel show a one-glance
    /// hint of the card body without expanding the whole thing.
    pub body_preview: String,
    /// RFC 3339, from [`MemoryCard::created_at`].
    pub created_at: String,
    /// RFC 3339, from [`MemoryCard::last_used`]. Lets the panel
    /// sort or badge least-recently-used cards.
    pub last_used: String,
    /// `true` iff [`MemoryCard::pinned`].
    pub pinned: bool,
}

/// Cap on [`MemoryCardSummary::body_preview`] before truncation —
/// 200 chars is enough for one or two short paragraphs to be
/// visible without dominating the panel.
pub const MEMORY_BODY_PREVIEW_CHARS: usize = 200;

impl MemoryCardSummary {
    /// Build a summary from a `MemoryCard`. Splits `content` into a
    /// title (first non-empty line) and a preview (remaining text,
    /// capped at [`MEMORY_BODY_PREVIEW_CHARS`]).
    pub fn from_card(card: &MemoryCard) -> Self {
        let (title, body_preview) = split_title_and_preview(&card.content);
        Self {
            id: card.id.clone(),
            title,
            body_preview,
            created_at: card.created_at.clone(),
            last_used: card.last_used.clone(),
            pinned: card.pinned,
        }
    }
}

/// Pure helper for [`MemoryCardSummary::from_card`]. Walks
/// `content` once: first non-empty trimmed line becomes the title;
/// remaining text (with leading whitespace stripped) is the body,
/// truncated at the configured cap with a trailing ellipsis.
fn split_title_and_preview(content: &str) -> (String, String) {
    let mut lines = content.lines();
    let title = lines
        .by_ref()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    let remaining: String = lines.collect::<Vec<_>>().join("\n");
    let remaining = remaining.trim_start().to_string();
    let preview = if remaining.chars().count() > MEMORY_BODY_PREVIEW_CHARS {
        let truncated: String = remaining.chars().take(MEMORY_BODY_PREVIEW_CHARS).collect();
        format!("{truncated}…")
    } else {
        remaining
    };
    (title, preview)
}

/// Conservative filename derivation — keep ASCII-alphanumeric / `-` / `_`,
/// replace anything else with `_`. Covers the common case where memory ids
/// look like `mem-1` while still being defensive against ids that came from
/// user input (e.g., card titles).
fn sanitize_filename(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, content: &str) -> MemoryCard {
        MemoryCard {
            id: id.into(),
            content: content.into(),
            created_at: "2026-05-16T10:00:00Z".into(),
            last_used: "2026-05-16T10:00:00Z".into(),
            pinned: false,
        }
    }

    // ---------- add / get / iter ----------

    #[test]
    fn add_then_get_round_trips() {
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "alpha")).unwrap();
        let got = s.get("mem-1").unwrap();
        assert_eq!(got.content, "alpha");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "first")).unwrap();
        let err = s.add(card("mem-1", "second")).unwrap_err();
        assert!(matches!(err, MemoryError::DuplicateId(id) if id == "mem-1"));
        assert_eq!(s.get("mem-1").unwrap().content, "first");
    }

    #[test]
    fn iter_yields_insertion_order() {
        let mut s = MemoryStore::new();
        for (id, c) in [("a", "1"), ("b", "2"), ("c", "3")] {
            s.add(card(id, c)).unwrap();
        }
        let ids: Vec<_> = s.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn from_vec_preserves_order_and_rejects_duplicates() {
        let cards = vec![card("a", "1"), card("b", "2")];
        let s = MemoryStore::from_vec(cards.clone()).unwrap();
        assert_eq!(s.to_vec(), cards);

        let dups = vec![card("a", "1"), card("a", "1")];
        let err = MemoryStore::from_vec(dups).unwrap_err();
        assert!(matches!(err, MemoryError::DuplicateId(_)));
    }

    // ---------- touch / pin / unpin ----------

    #[test]
    fn touch_updates_last_used_only() {
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "x")).unwrap();
        let before = s.get("mem-1").unwrap().created_at.clone();
        s.touch("mem-1", "2026-05-16T11:00:00Z").unwrap();
        let after = s.get("mem-1").unwrap();
        assert_eq!(after.last_used, "2026-05-16T11:00:00Z");
        assert_eq!(after.created_at, before);
    }

    #[test]
    fn pin_unpin_toggle_persists() {
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "x")).unwrap();
        assert!(!s.get("mem-1").unwrap().pinned);
        s.pin("mem-1").unwrap();
        assert!(s.get("mem-1").unwrap().pinned);
        s.unpin("mem-1").unwrap();
        assert!(!s.get("mem-1").unwrap().pinned);
    }

    #[test]
    fn missing_id_ops_return_not_found_without_mutating() {
        let mut s = MemoryStore::new();
        assert!(matches!(s.pin("nope"), Err(MemoryError::NotFound(_))));
        assert!(matches!(
            s.touch("nope", "x"),
            Err(MemoryError::NotFound(_))
        ));
        assert!(matches!(s.evict("nope"), Err(MemoryError::NotFound(_))));
        assert!(s.is_empty());
    }

    // ---------- evict ----------

    #[test]
    fn evict_returns_the_card_and_removes_it() {
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "x")).unwrap();
        let removed = s.evict("mem-1").unwrap();
        assert_eq!(removed.content, "x");
        assert!(s.get("mem-1").is_none());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn evict_pinned_card_still_removes_it() {
        // Unlike context items, memory pinning prevents compaction, not
        // explicit user-driven eviction. The UI is responsible for the
        // "are you sure?" prompt; the store just removes.
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "x")).unwrap();
        s.pin("mem-1").unwrap();
        assert!(s.evict("mem-1").is_ok());
    }

    // ---------- promote_to_global ----------

    #[test]
    fn promote_includes_frontmatter_and_content() {
        let mut s = MemoryStore::new();
        s.add(card("mem-7", "User prefers small composable helpers."))
            .unwrap();
        let out = s.promote_to_global("mem-7").unwrap();
        assert_eq!(out.relative_path, "mem-7.md");
        let body = String::from_utf8(out.bytes).unwrap();
        assert!(body.starts_with("---\n"));
        assert!(body.contains("name: mem-7"));
        assert!(body.contains("type: promoted"));
        assert!(body.contains("created_at: 2026-05-16T10:00:00Z"));
        assert!(body.contains("User prefers small composable helpers."));
    }

    #[test]
    fn promote_sanitises_filename_for_funky_ids() {
        let mut s = MemoryStore::new();
        s.add(card("weird id/with::stuff", "x")).unwrap();
        let out = s.promote_to_global("weird id/with::stuff").unwrap();
        assert_eq!(out.relative_path, "weird_id_with__stuff.md");
    }

    #[test]
    fn promote_does_not_modify_the_store() {
        let mut s = MemoryStore::new();
        s.add(card("mem-1", "x")).unwrap();
        let before = s.get("mem-1").cloned().unwrap();
        s.promote_to_global("mem-1").unwrap();
        let after = s.get("mem-1").cloned().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn promote_unknown_id_errors() {
        let s = MemoryStore::new();
        assert!(matches!(
            s.promote_to_global("nope"),
            Err(MemoryError::NotFound(_))
        ));
    }

    // ---------- schema-shape round trip ----------

    #[test]
    fn round_trips_through_serde_with_optional_pinned() {
        let with_pinned = card("mem-1", "x");
        let json = serde_json::to_string(&with_pinned).unwrap();
        // pinned=false is omitted, matching the existing example session
        // shapes where most cards are unpinned and the key is absent.
        assert!(!json.contains("pinned"));
        let back: MemoryCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_pinned);

        let mut pinned = with_pinned.clone();
        pinned.pinned = true;
        let json = serde_json::to_string(&pinned).unwrap();
        assert!(json.contains("\"pinned\":true"));
        let back: MemoryCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pinned);
    }

    #[test]
    fn rejects_unknown_fields_at_card_level() {
        let raw = r#"{"id":"x","content":"x","created_at":"x","last_used":"x","extra":1}"#;
        assert!(serde_json::from_str::<MemoryCard>(raw).is_err());
    }

    // ---------- v54: MemoryCardSummary + summarise() ----------

    fn fixture_card(id: &str, content: &str) -> MemoryCard {
        MemoryCard {
            id: id.into(),
            content: content.into(),
            created_at: "2026-05-17T10:00:00Z".into(),
            last_used: "2026-05-17T12:00:00Z".into(),
            pinned: false,
        }
    }

    #[test]
    fn summary_title_is_first_non_empty_line() {
        let c = fixture_card(
            "mem-1",
            "User prefers tabs over spaces.\n\nDetails: chose this in turn 2.",
        );
        let s = MemoryCardSummary::from_card(&c);
        assert_eq!(s.id, "mem-1");
        assert_eq!(s.title, "User prefers tabs over spaces.");
        assert_eq!(s.body_preview, "Details: chose this in turn 2.");
    }

    #[test]
    fn summary_skips_leading_blank_lines_for_title() {
        let c = fixture_card("mem-1", "\n   \nFirst real line\nsecond line");
        let s = MemoryCardSummary::from_card(&c);
        assert_eq!(s.title, "First real line");
        assert_eq!(s.body_preview, "second line");
    }

    #[test]
    fn summary_empty_content_yields_empty_title_and_preview() {
        let c = fixture_card("mem-1", "");
        let s = MemoryCardSummary::from_card(&c);
        assert_eq!(s.title, "");
        assert_eq!(s.body_preview, "");
    }

    #[test]
    fn summary_single_line_has_empty_preview() {
        let c = fixture_card("mem-1", "just one line, no body");
        let s = MemoryCardSummary::from_card(&c);
        assert_eq!(s.title, "just one line, no body");
        assert_eq!(s.body_preview, "");
    }

    #[test]
    fn summary_body_preview_truncates_with_ellipsis_past_cap() {
        let body = "x".repeat(MEMORY_BODY_PREVIEW_CHARS + 50);
        let content = format!("title line\n{body}");
        let c = fixture_card("mem-1", &content);
        let s = MemoryCardSummary::from_card(&c);
        assert_eq!(s.title, "title line");
        // Cap chars + ellipsis.
        assert_eq!(
            s.body_preview.chars().count(),
            MEMORY_BODY_PREVIEW_CHARS + 1
        );
        assert!(s.body_preview.ends_with('…'));
    }

    #[test]
    fn summary_carries_pinned_and_timestamps() {
        let mut c = fixture_card("mem-1", "title\nbody");
        c.pinned = true;
        c.last_used = "2026-05-17T13:00:00Z".into();
        let s = MemoryCardSummary::from_card(&c);
        assert!(s.pinned);
        assert_eq!(s.last_used, "2026-05-17T13:00:00Z");
        assert_eq!(s.created_at, "2026-05-17T10:00:00Z");
    }

    #[test]
    fn summarise_preserves_insertion_order() {
        let mut m = MemoryStore::new();
        m.add(fixture_card("a", "first")).unwrap();
        m.add(fixture_card("b", "second")).unwrap();
        m.add(fixture_card("c", "third")).unwrap();
        let v = m.summarise();
        let ids: Vec<&str> = v.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn summary_round_trips_through_serde() {
        let c = fixture_card("mem-1", "title\nbody");
        let s = MemoryCardSummary::from_card(&c);
        let json = serde_json::to_string(&s).unwrap();
        let back: MemoryCardSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn matches_the_shape_in_the_bundled_example_session() {
        // Lifted from tests/sessions/examples/with_fork_and_recovery.json
        let raw = r#"{
            "id": "mem-1",
            "content": "User prefers small composable helpers over class-based shapes in this repo. (Recorded from turn-2 follow-up; verify on fork merge.)",
            "created_at": "2026-05-16T09:00:10Z",
            "last_used": "2026-05-16T09:02:30Z",
            "pinned": false
        }"#;
        let card: MemoryCard = serde_json::from_str(raw).unwrap();
        assert_eq!(card.id, "mem-1");
        assert!(!card.pinned);
    }
}
