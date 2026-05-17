//! v60.6 — §5 Expand orchestration. Symmetric counterpart to
//! [`crate::compaction`].
//!
//! Composes the three steps the GUI Tauri command + TUI `submit_expand`
//! helper both need:
//!
//! 1. Snapshot the summary `MemoryCard` via
//!    `SessionDispatcher::snapshot_memory_card`; refuse if it isn't a
//!    compaction-generated card.
//! 2. Read the on-disk blob via [`crate::compaction_blob::read`] using
//!    the path stored in the card's `compacted_from`.
//! 3. Call `SessionDispatcher::expand_memory_card` with the blob's
//!    items; the dispatcher owns the atomic state mutation (add_batch,
//!    drop summary card, ledger `Expansion`, broadcast events).
//!
//! Distinct from `compaction::compact` in three ways:
//!
//! * No adapter call — the items are restored verbatim from disk; no
//!   model in the loop.
//! * No size cap on the input — the blob writer enforces
//!   `MAX_COMPACTION_BLOB_BYTES` at write time; re-reading something
//!   that fit when written is safe by construction.
//! * Cost disclosure is descriptive rather than aspirational — the
//!   user already agreed in the confirm dialog; this module surfaces
//!   the exact value (sum of restored items' tokens) in the returned
//!   [`ExpansionResult`] so the toast can read "restored N items;
//!   paid ~M cache tokens."
//!
//! Like `compaction::compact`, the function is `async` even though
//! the body is sync — keeps the call-site symmetry with the
//! Compact path (both spawned from a tokio task in the TUI; both
//! `await`ed in the GUI Tauri command).

use std::path::Path;

use atelier_core::dispatcher::{ExpansionError, SessionDispatcher};

use crate::compaction_blob;

/// Outcome of a successful [`expand`]. The GUI / TUI render
/// `restored_item_count` and `cache_rewarm_tokens` in their toasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionResult {
    /// Number of `ContextItem`s restored.
    pub restored_item_count: usize,
    /// Id of the summary `MemoryCard` that was dropped.
    pub summary_card_id: String,
    /// Prompt-cache rewarm cost (sum of restored items' `tokens.count`).
    pub cache_rewarm_tokens: u32,
}

/// Error returned by [`expand`]. Wraps the layered failures (card
/// snapshot, blob read, dispatcher mutator) so the caller can render a
/// precise message.
#[derive(Debug, thiserror::Error)]
pub enum ExpansionRunError {
    #[error("expand: memory card {0:?} not found")]
    CardNotFound(String),

    #[error("expand: card {0:?} is not a compaction summary (missing compacted_from)")]
    NotACompactionCard(String),

    #[error("expand: blob read: {0}")]
    BlobRead(String),

    #[error(
        "expand: blob version {got} is not supported (expected {expected}); refusing to restore"
    )]
    BlobVersionMismatch { got: u32, expected: u32 },

    #[error("expand: dispatcher: {0}")]
    Dispatcher(#[from] ExpansionError),
}

/// v60.6 — run a §5 Expand end-to-end.
///
/// The caller (GUI Tauri command, TUI `submit_expand`, future
/// autopilot) supplies:
///
/// * `dispatcher` — owns the shared `ContextManager` / `MemoryStore` /
///   `Ledger` / event bus.
/// * `workspace_root` — repository root; the blob lives under
///   `<workspace_root>/.atelier/sessions/<session_id>/compactions/`.
/// * `card_id` — id of the summary memory card to expand.
/// * `now` — caller-supplied RFC 3339 timestamp; threaded onto the
///   `Expansion` ledger entry.
///
/// Order of operations is deliberate: snapshot card (cheap, no
/// state change), then blob read (medium, can fail), then dispatcher
/// mutator (state mutation, atomic). A failure at any step before
/// the dispatcher call leaves the session unchanged.
pub async fn expand(
    dispatcher: &SessionDispatcher,
    workspace_root: &Path,
    card_id: String,
    now: &str,
) -> Result<ExpansionResult, ExpansionRunError> {
    // ---- Step 1: snapshot the card + extract its compaction link. ----
    let card = dispatcher
        .snapshot_memory_card(&card_id)
        .ok_or_else(|| ExpansionRunError::CardNotFound(card_id.clone()))?;
    let compacted_from = card
        .compacted_from
        .as_ref()
        .ok_or_else(|| ExpansionRunError::NotACompactionCard(card_id.clone()))?;

    // ---- Step 2: read the blob. ----
    let blob = compaction_blob::read(workspace_root, &compacted_from.expansion_blob_path)
        .map_err(ExpansionRunError::BlobRead)?;
    if blob.version != compaction_blob::COMPACTION_BLOB_VERSION {
        return Err(ExpansionRunError::BlobVersionMismatch {
            got: blob.version,
            expected: compaction_blob::COMPACTION_BLOB_VERSION,
        });
    }

    // ---- Step 3: atomic mutation through the dispatcher. ----
    let out = dispatcher.expand_memory_card(card_id, blob.items, now)?;

    Ok(ExpansionResult {
        restored_item_count: out.restored_item_count,
        summary_card_id: out.summary_card_id,
        cache_rewarm_tokens: out.cache_rewarm_tokens,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use atelier_core::context::{
        ContextItem, ContextItemId, ContextManager, Payload, Provenance, TokenCount, TokenSource,
    };
    use atelier_core::dispatcher::{Dispatcher, SessionDispatcher, ToolRegistry};
    use atelier_core::hooks::HookSet;
    use atelier_core::ledger::{Kind as LedgerKind, Ledger};
    use atelier_core::memory::MemoryStore;
    use atelier_core::plan::PlanCanvas;
    use parking_lot::Mutex;
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use super::*;

    type TestHarness = (
        SessionDispatcher,
        Arc<Ledger>,
        Arc<Mutex<ContextManager>>,
        Arc<Mutex<MemoryStore>>,
        Vec<String>,
    );

    fn build_dispatcher_with_items(count: u32) -> TestHarness {
        let dispatcher = Dispatcher::new(ToolRegistry::new(), HookSet::empty());
        let ledger = Arc::new(Ledger::new());
        let (tx, _rx) = broadcast::channel(64);
        let cm = Arc::new(Mutex::new(ContextManager::new()));
        let ms = Arc::new(Mutex::new(MemoryStore::new()));
        let pc = Arc::new(Mutex::new(PlanCanvas::new()));
        let sd = SessionDispatcher::new(dispatcher, ledger.clone(), tx).with_shared_state(
            cm.clone(),
            ms.clone(),
            pc.clone(),
        );
        let mut ids = Vec::new();
        for i in 0..count {
            let item = ContextItem {
                id: ContextItemId::new(),
                payload: Payload::InlineText {
                    text: format!("inline-{i}"),
                },
                tokens: TokenCount {
                    count: 100 + i,
                    source: TokenSource::Exact,
                },
                provenance: Provenance::UserAttached { note: None },
                pinned: false,
                added_at: "2026-05-17T10:00:00Z".into(),
                last_used: "2026-05-17T10:00:00Z".into(),
            };
            let id = item.id;
            cm.lock().add(item);
            ids.push(id.to_string());
        }
        (sd, ledger, cm, ms, ids)
    }

    /// Snapshot the items, write the blob, run the compaction
    /// dispatcher mutator, return (summary_card_id, blob_relative_path).
    /// Mirrors what `compaction::compact` does end-to-end but skips
    /// the adapter call (no MockAdapter needed for the expand tests).
    fn compact_in_place(
        sd: &SessionDispatcher,
        cm: &Arc<Mutex<ContextManager>>,
        ws: &Path,
        ids: &[String],
    ) -> (String, String) {
        let items = sd.snapshot_context_items(ids).expect("snapshot");
        let sid = uuid::Uuid::new_v4().to_string();
        let written =
            compaction_blob::write(ws, &sid, "2026-05-17T11:00:00Z", &items).expect("blob write");
        let out = sd
            .compact_context_items(
                ids.to_vec(),
                "summary line".into(),
                written.relative_path.clone(),
                "2026-05-17T11:00:00Z",
            )
            .expect("compact must succeed");
        // Sanity: post-compaction, the items are gone and the card
        // is present.
        assert_eq!(cm.lock().len(), 0);
        (out.summary_card_id, written.relative_path)
    }

    #[tokio::test]
    async fn expand_happy_path_round_trips_items_and_drops_card() {
        let ws = TempDir::new().unwrap();
        let (sd, ledger, cm, ms, ids) = build_dispatcher_with_items(3);
        let (card_id, _path) = compact_in_place(&sd, &cm, ws.path(), &ids);
        assert_eq!(ms.lock().len(), 1, "summary card present pre-expand");

        let result = expand(&sd, ws.path(), card_id.clone(), "2026-05-17T12:00:00Z")
            .await
            .expect("expand must succeed");
        assert_eq!(result.restored_item_count, 3);
        assert_eq!(result.cache_rewarm_tokens, 100 + 101 + 102);
        assert_eq!(result.summary_card_id, card_id);

        // Context state: items are back.
        assert_eq!(cm.lock().len(), 3);
        // Memory state: card gone.
        assert_eq!(ms.lock().len(), 0);
        // Ledger has a tail Expansion entry.
        let entries = ledger.to_vec();
        assert_eq!(entries.last().unwrap().kind(), LedgerKind::Expansion);
    }

    #[tokio::test]
    async fn expand_unknown_card_returns_card_not_found() {
        let ws = TempDir::new().unwrap();
        let (sd, _ledger, _cm, _ms, _ids) = build_dispatcher_with_items(0);
        let err = expand(&sd, ws.path(), "mem-nope".into(), "t")
            .await
            .unwrap_err();
        assert!(matches!(err, ExpansionRunError::CardNotFound(_)));
    }

    #[tokio::test]
    async fn expand_plain_card_returns_not_a_compaction_card() {
        let ws = TempDir::new().unwrap();
        let (sd, _ledger, _cm, _ms, _ids) = build_dispatcher_with_items(0);
        let plain_id = sd
            .add_memory_card("ordinary card".into(), "2026-05-17T10:00:00Z")
            .unwrap();
        let err = expand(&sd, ws.path(), plain_id, "t").await.unwrap_err();
        assert!(matches!(err, ExpansionRunError::NotACompactionCard(_)));
    }

    #[tokio::test]
    async fn expand_missing_blob_surfaces_blob_read_error() {
        let ws = TempDir::new().unwrap();
        let (sd, _ledger, cm, _ms, ids) = build_dispatcher_with_items(2);
        let (card_id, blob_path) = compact_in_place(&sd, &cm, ws.path(), &ids);
        // Wipe the blob from disk.
        let abs = ws.path().join(&blob_path);
        std::fs::remove_file(&abs).expect("rm blob");

        let err = expand(&sd, ws.path(), card_id, "t").await.unwrap_err();
        assert!(matches!(err, ExpansionRunError::BlobRead(_)));
    }
}
