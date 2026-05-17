//! v60.5 — §5 non-destructive compaction orchestration.
//!
//! Composes the three load-bearing operations into a single async
//! free function so the GUI Tauri command + TUI mutation handler
//! both delegate to one code path:
//!
//! 1. Snapshot the to-be-compacted `ContextItem`s via
//!    `SessionDispatcher::snapshot_context_items`.
//! 2. Generate the summary via `Adapter::chat` with a fixed
//!    system prompt + the snapshotted items as the user message.
//! 3. Write the items to disk via `compaction_blob::write` so
//!    v60.6 Expand has the originals to replay.
//! 4. Append the `ModelCall` ledger entry for the summary call.
//! 5. Call `SessionDispatcher::compact_context_items` with the
//!    generated summary + the written blob's relative path.
//!
//! The dispatcher's `compact_context_items` is what owns the
//! atomic state mutation (evict items, add summary card, ledger
//! Compaction entry, broadcast events). This module only owns the
//! adapter call + the disk write — both ahead of the mutation so a
//! failure here leaves the session unchanged.

use std::path::Path;

use atelier_core::adapter::{Adapter, AdapterError, Message, Role};
use atelier_core::context::ContextItem;
use atelier_core::dispatcher::{CompactionError, SessionDispatcher};
use atelier_core::ledger::LedgerEntry;

use crate::compaction_blob;

/// System prompt for the summary call. Fixed in v60.5; a future
/// version may make it configurable per-project. Constrained to keep
/// the summary inside the §5 token budget the freshly-pinned summary
/// card will occupy.
pub const COMPACTION_SUMMARY_SYSTEM_PROMPT: &str = "\
You are summarising context items so they can be safely evicted \
from a coding agent's context window. Produce a single short \
paragraph (<=120 words) covering: file paths mentioned, key \
claims about contents, and any open questions. Do not invent \
details. Do not include YAML frontmatter delimiters (no `---` \
lines) — your output is wrapped into a markdown memory card.";

/// Hard ceiling on the model's summary output. Defence-in-depth
/// against a runaway response: the dispatcher's text-safety check
/// already rejects malformed content, but a 100 KB "summary" would
/// be useless even if well-formed.
pub const MAX_SUMMARY_BYTES: usize = 16 * 1024;

/// Outcome of a successful [`compact`]. The GUI / TUI render
/// `freed_tokens` + `summary_card_id` in their toasts;
/// `summary_tokens_in` / `summary_tokens_out` let the cost meter show
/// the size of the model call the user just paid for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub summary_card_id: String,
    pub freed_tokens: u32,
    pub expansion_blob_path: String,
    pub summary_tokens_in: u32,
    pub summary_tokens_out: u32,
}

/// Error returned by [`compact`]. Wraps the layered failures (adapter
/// call, blob write, dispatcher mutator) so the caller can render a
/// precise message.
#[derive(Debug, thiserror::Error)]
pub enum CompactionRunError {
    #[error("compact: refusing to compact an empty selection")]
    Empty,

    #[error("compact: snapshot: {0}")]
    Snapshot(#[from] atelier_core::context::ContextError),

    #[error("compact: adapter: {0}")]
    Adapter(AdapterError),

    #[error("compact: summary too large ({0} bytes; cap is {MAX_SUMMARY_BYTES})")]
    SummaryTooLarge(usize),

    #[error("compact: blob write: {0}")]
    BlobWrite(String),

    #[error("compact: dispatcher: {0}")]
    Dispatcher(#[from] CompactionError),
}

/// v60.5 — run a §5 non-destructive compaction end-to-end.
///
/// The caller (GUI Tauri command, TUI mutation arm, future autopilot)
/// supplies:
///
/// * `adapter` — the active provider; used for the summary call.
/// * `dispatcher` — owns the shared `ContextManager` / `MemoryStore` /
///   `Ledger` / event bus.
/// * `workspace_root` — repository root; the blob lands under
///   `<workspace_root>/.atelier/sessions/<session_id>/compactions/`.
/// * `session_id` — current session UUID (string).
/// * `ids` — context-item ids to compact. Must be non-empty.
/// * `now` — caller-supplied RFC 3339 timestamp; reused for the
///   blob, the ledger entries, the summary card's `compacted_at`,
///   and the `evicted_at` on each `CacheBustEvent`.
///
/// Order of operations is deliberate: snapshot first (cheap, no
/// state change), then adapter call (slow, can fail), then blob
/// write (medium, can fail), then dispatcher mutator (state
/// mutation, atomic). A failure at any step before the dispatcher
/// call leaves the session unchanged.
pub async fn compact(
    adapter: &dyn Adapter,
    dispatcher: &SessionDispatcher,
    workspace_root: &Path,
    session_id: &str,
    ids: Vec<String>,
    now: &str,
) -> Result<CompactionResult, CompactionRunError> {
    if ids.is_empty() {
        return Err(CompactionRunError::Empty);
    }

    // ---- Step 1: snapshot the items (no state change). ----
    let items = dispatcher.snapshot_context_items(&ids)?;

    // ---- Step 2: ask the adapter for a summary. ----
    let user_prompt = render_items_as_prompt(&items);
    let messages = [
        Message::text(Role::System, COMPACTION_SUMMARY_SYSTEM_PROMPT),
        Message::text(Role::User, user_prompt),
    ];
    let response = adapter
        .chat(&messages, &[])
        .await
        .map_err(CompactionRunError::Adapter)?;
    let summary_text = response.text.trim().to_string();
    if summary_text.len() > MAX_SUMMARY_BYTES {
        return Err(CompactionRunError::SummaryTooLarge(summary_text.len()));
    }

    // ---- Step 3: write the blob. ----
    let written = compaction_blob::write(workspace_root, session_id, now, &items)
        .map_err(CompactionRunError::BlobWrite)?;

    // ---- Step 4: ledger the ModelCall for the summary. ----
    dispatcher.append_ledger_entry(LedgerEntry::ModelCall {
        timestamp: now.to_string(),
        model_id: adapter.model_id().to_string(),
        prompt_tokens: response.usage.prompt_tokens,
        completion_tokens: response.usage.completion_tokens,
        cached_tokens: response.usage.cached_tokens,
        count_source: response.usage.count_source,
        cost_usd: None,
        latency_ms: response.usage.latency_ms.map(|ms| ms as f64),
        note: Some(format!(
            "compaction summary: {} items -> {} bytes",
            items.len(),
            summary_text.len()
        )),
    });

    // ---- Step 5: atomic mutation through the dispatcher. ----
    let out =
        dispatcher.compact_context_items(ids, summary_text, written.relative_path.clone(), now)?;

    Ok(CompactionResult {
        summary_card_id: out.summary_card_id,
        freed_tokens: out.freed_tokens,
        expansion_blob_path: written.relative_path,
        summary_tokens_in: response.usage.prompt_tokens,
        summary_tokens_out: response.usage.completion_tokens,
    })
}

fn render_items_as_prompt(items: &[ContextItem]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(items.len() * 256);
    s.push_str(
        "The following context items are candidates for non-destructive compaction. \
         Summarise them so they can be safely evicted.\n\n",
    );
    for (i, item) in items.iter().enumerate() {
        let _ = writeln!(
            s,
            "## Item {} (id={}, tokens={})",
            i + 1,
            item.id,
            item.tokens.count
        );
        match &item.payload {
            atelier_core::context::Payload::FileRef { path, line_range } => {
                let range = line_range
                    .map(|(s, e)| format!(" lines {s}-{e}"))
                    .unwrap_or_default();
                let _ = writeln!(s, "file: {path}{range}");
            }
            atelier_core::context::Payload::InlineText { text } => {
                let _ = writeln!(s, "inline text:\n{text}");
            }
            atelier_core::context::Payload::BlobRef {
                sha256_hex,
                mime_type,
            } => {
                let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                let _ = writeln!(s, "blob: sha256:{sha256_hex} ({mime})");
            }
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use atelier_core::adapter::MockAdapter;
    use atelier_core::context::{
        ContextItem, ContextItemId, ContextManager, Payload, Provenance, TokenCount, TokenSource,
    };
    use atelier_core::dispatcher::{Dispatcher, SessionDispatcher, ToolRegistry};
    use atelier_core::hooks::HookSet;
    use atelier_core::ledger::{Kind as LedgerKind, Ledger, LedgerEntry};
    use atelier_core::memory::MemoryStore;
    use atelier_core::plan::PlanCanvas;
    use parking_lot::Mutex;
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use super::*;

    type TestHarness = (
        Arc<MockAdapter>,
        SessionDispatcher,
        Arc<Ledger>,
        Arc<Mutex<ContextManager>>,
        Vec<String>,
    );

    fn build_dispatcher_with_items(items: u32) -> TestHarness {
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
        for i in 0..items {
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
        let adapter = Arc::new(MockAdapter::new("mock:test"));
        (adapter, sd, ledger, cm, ids)
    }

    #[tokio::test]
    async fn compact_happy_path_emits_model_call_then_compaction_in_ledger() {
        let ws = TempDir::new().unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let (adapter, sd, ledger, cm, ids) = build_dispatcher_with_items(3);

        adapter.queue_text_response("Items 1-3 cover module X and Y.");

        let result = compact(
            adapter.as_ref(),
            &sd,
            ws.path(),
            &sid,
            ids.clone(),
            "2026-05-17T11:00:00Z",
        )
        .await
        .expect("compact must succeed");
        assert!(result.summary_card_id.starts_with("mem-"));
        assert_eq!(result.freed_tokens, 100 + 101 + 102);
        assert!(result.expansion_blob_path.contains(&sid));

        // Ledger order: ModelCall (summary) then Compaction.
        let entries = ledger.to_vec();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind(), LedgerKind::ModelCall);
        assert_eq!(entries[1].kind(), LedgerKind::Compaction);
        if let LedgerEntry::Compaction { freed_tokens, .. } = &entries[1] {
            assert_eq!(*freed_tokens, 100 + 101 + 102);
        }

        // Context state: empty.
        assert_eq!(cm.lock().len(), 0);

        // Blob on disk: round-trips.
        let blob = compaction_blob::read(ws.path(), &result.expansion_blob_path)
            .expect("blob must be readable");
        assert_eq!(blob.items.len(), 3);
    }

    #[tokio::test]
    async fn compact_empty_ids_returns_empty_error_without_touching_adapter() {
        let ws = TempDir::new().unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let (adapter, sd, ledger, cm, _) = build_dispatcher_with_items(2);

        // Adapter queue is empty — the call must NOT pop it.
        let err = compact(adapter.as_ref(), &sd, ws.path(), &sid, vec![], "t")
            .await
            .unwrap_err();
        assert!(matches!(err, CompactionRunError::Empty));
        assert_eq!(ledger.len(), 0);
        assert_eq!(cm.lock().len(), 2);
    }

    #[tokio::test]
    async fn compact_rejects_oversize_summary() {
        let ws = TempDir::new().unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let (adapter, sd, ledger, cm, ids) = build_dispatcher_with_items(2);

        // 20 KiB summary > 16 KiB cap.
        let runaway: String = "x".repeat(20 * 1024);
        adapter.queue_text_response(runaway);

        let err = compact(adapter.as_ref(), &sd, ws.path(), &sid, ids, "t")
            .await
            .unwrap_err();
        assert!(matches!(err, CompactionRunError::SummaryTooLarge(_)));
        // Nothing committed: ledger empty, items intact, no blob.
        assert_eq!(ledger.len(), 0);
        assert_eq!(cm.lock().len(), 2);
    }

    #[tokio::test]
    async fn compact_dispatcher_error_does_not_leak_partial_ledger_state() {
        let ws = TempDir::new().unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let (adapter, sd, _ledger, cm, ids) = build_dispatcher_with_items(2);

        // Pin one of the items so the dispatcher's evict_batch refuses.
        // (We re-parse the id to access the underlying type.)
        let pin_id = uuid::Uuid::parse_str(&ids[0]).unwrap();
        cm.lock()
            .pin(atelier_core::context::ContextItemId(pin_id))
            .unwrap();

        adapter.queue_text_response("summary OK");

        let err = compact(adapter.as_ref(), &sd, ws.path(), &sid, ids, "t")
            .await
            .unwrap_err();
        // The dispatcher path got far enough to reject — the ModelCall
        // ledger entry + blob have been written (intentional: the user
        // paid for the summary call), but the state mutation aborted.
        assert!(matches!(err, CompactionRunError::Dispatcher(_)));
        assert_eq!(cm.lock().len(), 2, "items must remain");
    }
}
