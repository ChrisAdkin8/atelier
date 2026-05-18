//! Tauri shell for Atelier.
//!
//! Spec §3. Boots a Tauri app, spawns an `atelier_core::session::Handle`, and
//! forwards the broadcast event bus onto the webview as `atelier://event`.
//! The first panel (`ui/src/App.svelte`) subscribes and counts `EditStaged`
//! events — the smallest end-to-end demonstration that the broadcast bus
//! reaches the UI.
//!
//! The bridge is **one-way for now** (Rust → webview). Webview → Rust
//! commands (start session, cancel, advance) will land alongside the
//! multi-pane workspace; until then the only exposed command is `ping`, used
//! by the integration test to confirm the IPC wiring round-trips.
//!
//! # Event payload shape
//!
//! [`Event`](atelier_core::session::Event) is `Debug + Clone` but not
//! `Serialize` — adding serde to the core enum would force every variant's
//! constituent types (e.g. `State`) to be `Serialize` too, which we don't
//! want to commit to yet. So we hand-roll a JSON projection here. The
//! frontend matches on `payload.kind`.

use std::sync::Arc;

use atelier_cli::runner::{DispatcherHandle, EventSink, MockResponse, ProviderChoice, Runner};
use atelier_core::adapter::ToolCallRequest;
use atelier_core::dispatcher::ApprovalPolicy;
use atelier_core::protocol::Envelope;
use atelier_core::protocol_strategy::HARNESS_META_NAME;
use atelier_core::session::Event as SessionEvent;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

/// Wrapper Tauri emits to the webview. `kind` is the variant tag; `payload`
/// is the variant's JSON body. The TypeScript side only depends on `kind`
/// — `payload` shape is per-variant and evolves with the spec.
#[derive(Serialize, Clone, Debug)]
pub struct BridgedEvent {
    pub kind: &'static str,
    pub payload: Value,
}

/// State the Tauri runtime manages for the lifetime of the shell.
///
/// v47: the GUI is now a driver, not a viewer. `dispatcher_handle` is
/// populated by `start_demo_run` once the runner builds its
/// `SessionDispatcher`; `submit_approval` reads from it to route
/// accept-sets to the live dispatcher. `workspace_root` is the disk
/// root the demo run writes against — each run gets a fresh UUID
/// subdirectory (v49) so concurrent runs can't see each other's
/// edits.
///
/// `run_in_flight` (v49) is the concurrent-run guard: `start_demo_run`
/// uses compare_exchange to refuse a second invocation while one is
/// still active. Cleared by the spawned task's `Drop`-style cleanup.
pub struct SessionState {
    pub dispatcher_handle: DispatcherHandle,
    /// v60.5 — companion slot for the active `Adapter`. Populated by
    /// `start_demo_run` alongside `dispatcher_handle`; cleared by the
    /// runner's `AdapterHandleGuard` on every exit path. Read by
    /// `compact_context_items` to issue the §5 summary call.
    pub adapter_handle: atelier_cli::AdapterHandle,
    pub run_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// v58 (M-sec-2 regression fix) — own the per-process tempdir
    /// handle so RAII removes the directory on app shutdown. The
    /// pre-v58 path called `tempfile::TempDir::keep()` which leaked
    /// `/tmp/atelier-gui-{pid}-XXXX/` forever; each launch left a
    /// fresh empty directory in `/tmp`.
    ///
    /// v59 (audit LOW-7 fix) — single source of truth for the
    /// per-process workspace root. Pre-v59 `workspace_root` was
    /// stored as a separate `PathBuf` alongside this handle; a
    /// future edit that mutated one and not the other would
    /// silently desync. Callers read `workspace_root()` instead.
    pub workspace_tempdir: tempfile::TempDir,
}

impl SessionState {
    /// Per-process workspace root (the parent of every per-run UUID
    /// subdir created by `start_demo_run`). Always points inside the
    /// owned `workspace_tempdir` so RAII cleanup covers any descendant
    /// left behind by `RunCleanup`.
    pub fn workspace_root(&self) -> &std::path::Path {
        self.workspace_tempdir.path()
    }
}

/// Entry point. Spawned by `main.rs`; lives in `lib.rs` so the integration
/// tests can pull in the same module and exercise the helpers.
pub fn run() {
    tracing_subscriber::fmt::try_init().ok();

    tauri::Builder::default()
        .setup(|app| {
            // v47: ephemeral workspace per process. Real "open project"
            // selection lands when the GUI grows a file-tree pane.
            //
            // v57 (L cleanup) — use `tempfile::TempDir` for the
            // per-process root so the directory inherits 0700 perms.
            // The pre-v57 path was `std::env::temp_dir().join(pid)`
            // with the umask default (typically 0755), which on
            // multi-user Linux let any local user read staged files.
            //
            // v58 (M-sec-2 regression fix) — hold the `TempDir`
            // handle in `SessionState` so RAII removes the dir on
            // app shutdown. v57 called `.keep()` which leaked the
            // directory forever.
            let workspace_tempdir = tempfile::Builder::new()
                .prefix(&format!("atelier-gui-{}-", std::process::id()))
                .tempdir()
                .map_err(|e| std::io::Error::other(format!("workspace tempdir: {e}")))?;

            app.manage(SessionState {
                dispatcher_handle: DispatcherHandle::new(),
                adapter_handle: atelier_cli::AdapterHandle::new(),
                run_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                workspace_tempdir,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping,
            submit_approval,
            start_demo_run,
            // v55 §5 mutator commands.
            pin_context_item,
            unpin_context_item,
            evict_context_item,
            add_memory_card,
            delete_memory_card,
            promote_memory_card,
            add_plan_step,
            remove_plan_step,
            mark_plan_step_status,
            add_plan_step_constraint,
            reorder_plan_steps,
            // v60.5 §5 non-destructive compaction.
            compact_context_items,
            // v60.6 §5 Expand.
            expand_memory_card,
            // Phase C close — §5 mental-model panel.
            set_mental_model,
            snapshot_mental_model,
            // v61 §14 concurrent-edit modal resolver.
            resolve_concurrent_edit,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Trivial round-trip command used by the integration test to confirm the
/// IPC channel is wired. Production commands (start session, cancel,
/// advance) land alongside the multi-pane workspace.
#[tauri::command]
fn ping() -> &'static str {
    "pong"
}

/// v56 — wire-format file decision the webview sends on
/// `submit_approval`. Mirrors `atelier_core::staging::FileApproval`.
#[derive(serde::Deserialize, Debug)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum FileApprovalWire {
    /// Commit every staged byte for this file.
    All,
    /// Commit only the listed hunk indices. Empty list = drop.
    Hunks { indices: Vec<u32> },
}

impl FileApprovalWire {
    fn into_core(self) -> atelier_core::staging::FileApproval {
        match self {
            Self::All => atelier_core::staging::FileApproval::All,
            Self::Hunks { indices } => atelier_core::staging::FileApproval::Hunks(
                indices.into_iter().map(|i| i as usize).collect(),
            ),
        }
    }
}

/// Spec §3 hunk accept/reject — frontend bridge. Routed to the
/// live `SessionDispatcher` via the `DispatcherHandle` in
/// `SessionState`. Returns `false` when there's no active run
/// (`start_demo_run` hasn't been called) or when `commit_id` doesn't
/// match an outstanding pending (already approved / dispatcher torn
/// down).
///
/// v56: `selection` carries per-path decisions (and per-hunk indices
/// for `Hunks::Lines` files); a path absent from the map is fully
/// rejected.
/// v57 (L cleanup) — defence-in-depth on the Tauri boundary. Pre-v57
/// the path keys flowed straight to `PathBuf::from` and the staging
/// layer rejected absolute / `..` paths later. Rejecting at the
/// boundary makes the failure mode clearer in the IPC layer's logs.
fn is_safe_repo_relative(p: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|c| !matches!(c, std::path::Component::ParentDir))
}

#[tauri::command]
fn submit_approval(
    state: tauri::State<'_, SessionState>,
    commit_id: String,
    selection: std::collections::HashMap<String, FileApprovalWire>,
) -> bool {
    let Ok(parsed_id) = uuid::Uuid::parse_str(&commit_id) else {
        tracing::warn!(commit_id, "submit_approval: malformed commit_id");
        return false;
    };
    // v57 (L cleanup) — reject absolute / `..`-containing path keys
    // at the IPC boundary. The staging layer rejects them later
    // anyway; doing it here means the log line names the actual
    // problem and dispatch never sees a hostile selection map.
    for k in selection.keys() {
        if !is_safe_repo_relative(k) {
            tracing::warn!(path = %k, "submit_approval: rejecting unsafe path key");
            return false;
        }
    }
    let Some(sd) = state.dispatcher_handle.get() else {
        tracing::warn!(
            commit_id,
            "submit_approval: no active dispatcher (start_demo_run not running?)"
        );
        return false;
    };
    let core_selection: atelier_core::staging::HunkSelection = selection
        .into_iter()
        .map(|(p, fa)| (std::path::PathBuf::from(p), fa.into_core()))
        .collect();
    sd.submit_approval(parsed_id, core_selection)
}

/// v61 — §14 concurrent-edit modal resolver. Surfaced from the
/// webview's `ConcurrentEditModal.svelte`. `choice` is one of
/// `"reload"` / `"wait"` / `"pause"`; anything else is rejected so a
/// future variant rename forces a deliberate edit. Returns `false`
/// when there's no active dispatcher (run already torn down).
#[tauri::command]
fn resolve_concurrent_edit(state: tauri::State<'_, SessionState>, choice: String) -> bool {
    let outcome = match choice.as_str() {
        "reload" => atelier_core::ConcurrentEditOutcome::Reload,
        "wait" => atelier_core::ConcurrentEditOutcome::Wait,
        "pause" => atelier_core::ConcurrentEditOutcome::Pause,
        other => {
            tracing::warn!(choice = %other, "resolve_concurrent_edit: unknown choice");
            return false;
        }
    };
    let Some(sd) = state.dispatcher_handle.get() else {
        tracing::warn!("resolve_concurrent_edit: no active dispatcher");
        return false;
    };
    sd.resolve_concurrent_edit(outcome);
    true
}

// v57 (H6 fix): `now_rfc3339` lifted into `atelier_core::time`. The
// pre-v57 path had three byte-for-byte copies (this file, the runner,
// the TUI).
use atelier_core::time::now_rfc3339;

/// What `evict_context_item` returns to the frontend so the confirm
/// dialog can show "evicted — freed N tokens" without a follow-up
/// round-trip.
#[derive(Serialize, Debug)]
pub struct EvictResult {
    pub tokens_freed: u32,
}

/// What `promote_memory_card` returns. `path` is the absolute path
/// the bytes were written to (under `~/.atelier/memory/`).
#[derive(Serialize, Debug)]
pub struct PromoteResult {
    pub path: String,
    pub bytes: usize,
}

fn require_dispatcher(
    state: &tauri::State<'_, SessionState>,
) -> Result<std::sync::Arc<atelier_core::dispatcher::SessionDispatcher>, String> {
    state
        .dispatcher_handle
        .get()
        .ok_or_else(|| "no active dispatcher (start a run first)".to_string())
}

#[tauri::command]
fn pin_context_item(state: tauri::State<'_, SessionState>, id: String) -> Result<(), String> {
    let sd = require_dispatcher(&state)?;
    sd.pin_context_item(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn unpin_context_item(state: tauri::State<'_, SessionState>, id: String) -> Result<(), String> {
    let sd = require_dispatcher(&state)?;
    sd.unpin_context_item(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn evict_context_item(
    state: tauri::State<'_, SessionState>,
    id: String,
) -> Result<EvictResult, String> {
    let sd = require_dispatcher(&state)?;
    let now = now_rfc3339();
    sd.evict_context_item(&id, &now)
        .map(|ev| EvictResult {
            tokens_freed: ev.tokens_freed,
        })
        .map_err(|e| e.to_string())
}

/// v57 (M-sec-1) / v59 framework-limit note — per-Tauri-command size
/// caps. Pre-v57 the v55 mutator commands accepted unbounded
/// `String`s from the webview, each cloned through the dispatcher,
/// the memory/plan store, and echoed over the bus to every
/// subscriber. A hostile or buggy webview could land multi-GB
/// strings; in a future browser-bound mode this is a realistic
/// DoS path.
///
/// **Framework limitation (acknowledged in v59 audit MED-sec-1)**:
/// Tauri 2.x deserialises the IPC payload into the handler's
/// parameter types *before* the handler runs, so a multi-GB
/// `String` is already allocated by the time `check_bytes` rejects
/// it. The cap stops the value from escaping into the dispatcher /
/// bus / disk, but the initial allocation is unavoidable without
/// Tauri-side support for a per-window IPC body limit (no such
/// option exists in `tauri.conf.json` as of Tauri 2.0.x). When the
/// upstream API adds one, configure it via `app.security` in
/// `tauri.conf.json` and these caps become defence-in-depth rather
/// than the primary boundary.
const MAX_MEMORY_CARD_BYTES: usize = 32 * 1024;
const MAX_PLAN_STEP_BYTES: usize = 4 * 1024;
const MAX_PLAN_CONSTRAINT_BYTES: usize = 1024;
const MAX_PLAN_STEPS: usize = 256;

fn check_bytes(label: &str, s: &str, max: usize) -> Result<(), String> {
    if s.len() > max {
        return Err(format!(
            "{label} too long: {} bytes (max {max} bytes)",
            s.len()
        ));
    }
    Ok(())
}

#[tauri::command]
fn add_memory_card(
    state: tauri::State<'_, SessionState>,
    content: String,
) -> Result<String, String> {
    check_bytes("memory card content", &content, MAX_MEMORY_CARD_BYTES)?;
    let sd = require_dispatcher(&state)?;
    let now = now_rfc3339();
    sd.add_memory_card(content, &now).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_memory_card(state: tauri::State<'_, SessionState>, id: String) -> Result<(), String> {
    let sd = require_dispatcher(&state)?;
    sd.delete_memory_card(&id).map_err(|e| e.to_string())
}

/// Promote a card to `~/.atelier/memory/`. The dispatcher returns the
/// bytes (pure); this command does the disk write via the shared
/// [`atelier_cli::memory_promote::write_promoted_card`] helper so
/// the GUI and TUI go through the same hardened path.
///
/// v60 (security M-1 fix) — pre-v60 the GUI's `promote_memory_card`
/// and the TUI's `Mutation::PromoteMemory` carried independent
/// copies of the HOME validation / canonical-root containment /
/// atomic-write logic. The TUI copy was *not* updated for v58 / v59
/// hardening, leaving the TUI driver as a bypass. v60 consolidates
/// the hardening in `atelier-cli::memory_promote` and both drivers
/// delegate.
#[tauri::command]
fn promote_memory_card(
    state: tauri::State<'_, SessionState>,
    id: String,
) -> Result<PromoteResult, String> {
    let sd = require_dispatcher(&state)?;
    let now = now_rfc3339();
    let output = sd
        .promote_memory_card(&id, &now)
        .map_err(|e| e.to_string())?;
    let written = atelier_cli::memory_promote::write_promoted_card(&output)?;
    Ok(PromoteResult {
        path: written.path.to_string_lossy().to_string(),
        bytes: written.bytes,
    })
}

#[tauri::command]
fn add_plan_step(state: tauri::State<'_, SessionState>, text: String) -> Result<String, String> {
    check_bytes("plan step text", &text, MAX_PLAN_STEP_BYTES)?;
    let sd = require_dispatcher(&state)?;
    sd.add_plan_step(text).map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_plan_step(state: tauri::State<'_, SessionState>, id: String) -> Result<(), String> {
    let sd = require_dispatcher(&state)?;
    sd.remove_plan_step(&id).map_err(|e| e.to_string())
}

/// Map a wire-format status string onto [`atelier_core::plan::PlanStatus`].
/// Rejects unknown labels rather than coercing silently.
///
/// v58 (MED-smell-2 fix) — routes through
/// `PlanStatus::from_wire_label`, the single source of truth shared
/// with the serde `rename_all = "snake_case"` projection.
fn parse_plan_status(s: &str) -> Result<atelier_core::plan::PlanStatus, String> {
    atelier_core::plan::PlanStatus::from_wire_label(s)
        .ok_or_else(|| format!("unknown plan status {s:?}"))
}

#[tauri::command]
fn mark_plan_step_status(
    state: tauri::State<'_, SessionState>,
    id: String,
    status: String,
) -> Result<(), String> {
    let sd = require_dispatcher(&state)?;
    let parsed = parse_plan_status(&status)?;
    sd.mark_plan_step_status(&id, parsed)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_plan_step_constraint(
    state: tauri::State<'_, SessionState>,
    id: String,
    constraint: String,
) -> Result<(), String> {
    check_bytes(
        "plan step constraint",
        &constraint,
        MAX_PLAN_CONSTRAINT_BYTES,
    )?;
    let sd = require_dispatcher(&state)?;
    sd.add_plan_step_constraint(&id, constraint)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reorder_plan_steps(
    state: tauri::State<'_, SessionState>,
    ordering: Vec<String>,
) -> Result<(), String> {
    if ordering.len() > MAX_PLAN_STEPS {
        return Err(format!(
            "reorder list too long: {} items (max {MAX_PLAN_STEPS})",
            ordering.len()
        ));
    }
    let sd = require_dispatcher(&state)?;
    sd.reorder_plan_steps(ordering).map_err(|e| e.to_string())
}

/// v60.5 — wire shape returned by the Compact toast in the §5 Context
/// pane. Carries enough to populate "Compacted N items, freed ~Mk
/// tokens; summary card mem-…" without a follow-up query.
#[derive(Serialize, Debug)]
pub struct CompactionResult {
    pub tokens_freed: u32,
    pub summary_card_id: String,
    pub expansion_blob_path: String,
    pub summary_tokens_in: u32,
    pub summary_tokens_out: u32,
}

/// v60.5 — cap on the number of items a single compaction call may
/// touch. Matches the `MAX_PLAN_STEPS` discipline on the v55 mutators:
/// a hostile or buggy webview shouldn't be able to push a 1M-id list
/// through the IPC boundary.
const MAX_COMPACTION_IDS: usize = 256;

fn require_adapter(
    state: &tauri::State<'_, SessionState>,
) -> Result<std::sync::Arc<dyn atelier_core::adapter::Adapter>, String> {
    state
        .adapter_handle
        .get()
        .ok_or_else(|| "no active adapter (start a run first)".to_string())
}

#[tauri::command]
async fn compact_context_items(
    state: tauri::State<'_, SessionState>,
    ids: Vec<String>,
) -> Result<CompactionResult, String> {
    if ids.len() > MAX_COMPACTION_IDS {
        return Err(format!(
            "compact_context_items: too many ids: {} (max {MAX_COMPACTION_IDS})",
            ids.len()
        ));
    }
    let sd = require_dispatcher(&state)?;
    let adapter = require_adapter(&state)?;
    let workspace = state.workspace_root().to_path_buf();
    // v49 per-run workspace lives under the per-process root; for v60.5
    // we use the process-wide root as the session id since the GUI
    // demo run doesn't expose its run_id externally. Real session-id
    // routing lands once the GUI grows a session picker.
    let session_id = uuid::Uuid::new_v4().to_string();
    let now = now_rfc3339();
    atelier_cli::compaction::compact(
        adapter.as_ref(),
        sd.as_ref(),
        &workspace,
        &session_id,
        ids,
        &now,
    )
    .await
    .map(|r| CompactionResult {
        tokens_freed: r.freed_tokens,
        summary_card_id: r.summary_card_id,
        expansion_blob_path: r.expansion_blob_path,
        summary_tokens_in: r.summary_tokens_in,
        summary_tokens_out: r.summary_tokens_out,
    })
    .map_err(|e| e.to_string())
}

/// v60.6 — wire shape returned by the Expand toast in the §5 Memory
/// pane. Carries enough to render "Restored N items; ~M cache tokens
/// re-warmed" without a follow-up query.
#[derive(Serialize, Debug)]
pub struct ExpansionResult {
    pub restored_item_count: usize,
    pub summary_card_id: String,
    pub cache_rewarm_tokens: u32,
}

/// Phase C close — §5 mental-model wire shape returned by both
/// `set_mental_model` and `snapshot_mental_model`. Mirrors
/// [`atelier_core::mental_model::MentalModelSnapshot`].
#[derive(Serialize, Debug)]
pub struct MentalModelWire {
    pub enabled: bool,
    pub text: String,
    pub text_tokens: u32,
    pub updated_at: String,
}

impl From<atelier_core::mental_model::MentalModelSnapshot> for MentalModelWire {
    fn from(s: atelier_core::mental_model::MentalModelSnapshot) -> Self {
        Self {
            enabled: s.enabled,
            text: s.text,
            text_tokens: s.text_tokens,
            updated_at: s.updated_at,
        }
    }
}

/// Phase C close — cap on the mental-model text size. Same discipline
/// as `MAX_MEMORY_CARD_BYTES` — a hostile webview shouldn't be able to
/// push a multi-GB string through the IPC boundary into the bus.
const MAX_MENTAL_MODEL_BYTES: usize = 32 * 1024;

#[tauri::command]
fn set_mental_model(
    state: tauri::State<'_, SessionState>,
    text: String,
    enabled: bool,
) -> Result<MentalModelWire, String> {
    check_bytes("mental model text", &text, MAX_MENTAL_MODEL_BYTES)?;
    let sd = require_dispatcher(&state)?;
    let now = now_rfc3339();
    sd.set_mental_model(text, enabled, &now)
        .map(MentalModelWire::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn snapshot_mental_model(state: tauri::State<'_, SessionState>) -> Result<MentalModelWire, String> {
    let sd = require_dispatcher(&state)?;
    Ok(sd.snapshot_mental_model().into())
}

#[tauri::command]
async fn expand_memory_card(
    state: tauri::State<'_, SessionState>,
    id: String,
) -> Result<ExpansionResult, String> {
    let sd = require_dispatcher(&state)?;
    let workspace = state.workspace_root().to_path_buf();
    let now = now_rfc3339();
    atelier_cli::expansion::expand(sd.as_ref(), &workspace, id, &now)
        .await
        .map(|r| ExpansionResult {
            restored_item_count: r.restored_item_count,
            summary_card_id: r.summary_card_id,
            cache_rewarm_tokens: r.cache_rewarm_tokens,
        })
        .map_err(|e| e.to_string())
}

/// Start a mock-scripted run with `AwaitApproval` policy. v47 demo
/// driver: the GUI builds a `Runner` that emits a `write_file` tool
/// call against the ephemeral workspace, the dispatcher hits the
/// approval gate, the user clicks accept/reject in the DiffPane, the
/// resulting commit lands in the workspace.
///
/// Returns immediately after spawning the run on the tokio runtime;
/// the webview observes progress via the `atelier://event` stream.
/// Max prompt size accepted by `start_demo_run`. A multi-GB string
/// from a hostile or buggy webview would otherwise be copied into
/// `format!(content)`, `MockResponse`, the bus envelope, and the
/// adapter's message history — easy DoS surface.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

#[tauri::command]
fn start_demo_run(
    app: AppHandle,
    state: tauri::State<'_, SessionState>,
    prompt: String,
) -> Result<(), String> {
    if prompt.len() > MAX_PROMPT_BYTES {
        // `.len()` is bytes (memory cost is what we care about, not
        // character count). In a multi-byte locale a CJK or emoji
        // prompt may report e.g. 21k chars but 64k bytes — the
        // message clarifies this so the user doesn't read "bytes"
        // as "characters."
        return Err(format!(
            "prompt too long: {} bytes (max {} bytes / ~{} ASCII chars)",
            prompt.len(),
            MAX_PROMPT_BYTES,
            MAX_PROMPT_BYTES
        ));
    }

    // v49 concurrent-run guard. compare_exchange (Acquire/Relaxed) so
    // a second invocation while a run is in flight gets a typed error
    // the frontend can surface, rather than silently corrupting the
    // dispatcher slot.
    if state
        .run_in_flight
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::Acquire,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return Err("a run is already in progress — wait for it to finish".to_string());
    }

    // v49 per-run workspace: a fresh UUID-named subdir under the GUI's
    // ephemeral root. Two concurrent demos can't clobber each other's
    // files (the concurrent-run guard above also prevents this today,
    // but the directory isolation is defence in depth and survives a
    // future relaxation of the guard).
    let run_id = uuid::Uuid::new_v4();
    let workspace = state.workspace_root().join(run_id.to_string());
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        state
            .run_in_flight
            .store(false, std::sync::atomic::Ordering::Release);
        return Err(format!("workspace setup failed: {e}"));
    }

    let handle = state.dispatcher_handle.clone();
    let adapter_handle = state.adapter_handle.clone();
    let run_in_flight = state.run_in_flight.clone();

    // Build a scripted single-turn run:
    //   1. Assistant emits a write_file tool call + a harness_meta
    //      envelope carrying claimed_done.
    //   2. Dispatcher stages the write, hits AwaitApproval, emits
    //      StagingPendingApproval — the DiffPane renders the banner.
    //   3. The user clicks accept or reject; submit_approval routes
    //      back; the dispatcher commits (or drops) and the run ends.
    //
    // The file name is derived from the prompt's first word so the
    // user sees their input reflected without us having to parse
    // anything more sophisticated.
    let file_name = first_word_or_default(&prompt, "demo.txt");
    let content = format!("written by the GUI demo driver:\n{prompt}\n");
    let write_call = ToolCallRequest {
        id: "tc-demo-write".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": file_name,
            "content": content,
        }),
    };
    let envelope_done = Envelope {
        claimed_done: Some(true),
        ..Default::default()
    };
    let envelope_call = ToolCallRequest {
        id: "tc-demo-envelope".to_string(),
        name: HARNESS_META_NAME.to_string(),
        arguments: serde_json::to_value(&envelope_done).unwrap_or(Value::Null),
    };
    let responses = vec![MockResponse {
        assistant_text: format!("Acknowledging: {prompt}"),
        tool_calls: vec![write_call, envelope_call],
    }];

    // EventSink::Callback forwards every bus event to the webview as
    // `atelier://event`. Same JSON shape `bridge_event` produces in
    // v44, just driven by the runner's own bus instead of a separate
    // session actor.
    let app_clone = app.clone();
    let cb = Arc::new(move |evt: &SessionEvent| {
        emit_event(&app_clone, evt);
    });

    let runner = match Runner::new(
        workspace.clone(),
        ProviderChoice::Mock { responses },
        EventSink::Callback(cb),
    ) {
        Ok(r) => r,
        Err(e) => {
            // Release the guard before bailing — otherwise the next
            // start_demo_run is permanently rejected.
            run_in_flight.store(false, std::sync::atomic::Ordering::Release);
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(format!("Runner::new failed: {e}"));
        }
    };
    let runner = runner
        .with_approval_policy(ApprovalPolicy::AwaitApproval)
        .with_dispatcher_handle(handle)
        .with_adapter_handle(adapter_handle)
        .with_max_turns(4);

    // The spawned task owns the per-run workspace + the in-flight
    // flag; both are cleaned up on every exit path via the
    // `RunCleanup` Drop guard below.
    tauri::async_runtime::spawn(async move {
        let _cleanup = RunCleanup {
            in_flight: run_in_flight,
            workspace_to_remove: Some(workspace.clone()),
        };
        if let Err(e) = runner.run(prompt).await {
            tracing::warn!(error = %e, "demo run failed");
        }
    });
    Ok(())
}

/// Drop-guard for `start_demo_run`'s spawned task. Clears the
/// `run_in_flight` flag and (best-effort) removes the per-run
/// workspace on every exit path — including a panic inside
/// `runner.run`. Mirrors the `DispatcherHandleGuard` pattern in
/// `atelier-cli/src/runner.rs`.
struct RunCleanup {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    workspace_to_remove: Option<std::path::PathBuf>,
}

impl Drop for RunCleanup {
    fn drop(&mut self) {
        self.in_flight
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(ws) = self.workspace_to_remove.take() {
            // `remove_dir_all` traverses symlinks on some platforms
            // (older glibc; pre-Rust-1.69 stdlib). If a model managed
            // to plant a symlink in the per-run workspace, this could
            // delete outside files. Two reasons we're OK here:
            //   1. `commit_selected` rejects `..` + absolute paths at
            //      the staging layer (spec §3), so a model can't
            //      write a symlink to outside via the tool path.
            //   2. The per-run workspace is under our own
            //      `temp_dir()/atelier-gui-{pid}/{run_uuid}` and is
            //      only ever written by atelier-core staging.
            // If a future change introduces a tool that writes
            // symlinks, audit this call and add a `symlink_metadata`
            // pre-check or switch to `tokio::fs::remove_dir_all`
            // (which is symlink-safe on every supported platform).
            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}

/// Pick the first whitespace-delimited word from `s`, sanitised down
/// to ASCII alphanumerics + `-`/`_`/`.`. Falls back to `default` when
/// no usable word is present. Used to build the demo file name.
fn first_word_or_default(s: &str, default: &str) -> String {
    let word: String = s
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect();
    if word.is_empty() {
        default.to_string()
    } else if word.contains('.') {
        word
    } else {
        format!("{word}.txt")
    }
}

fn emit_event(app: &AppHandle, evt: &SessionEvent) {
    let bridged = bridge_event(evt);
    if let Err(e) = app.emit("atelier://event", &bridged) {
        tracing::warn!("atelier-gui: emit failed: {e}");
    }
}

/// Project an [`atelier_core::session::Event`] onto the JSON shape the
/// webview consumes. Pure function — exercised by the unit tests below
/// without booting Tauri.
///
/// v57 (H5 fix) — the `kind` label is sourced from
/// [`SessionEvent::kind`] so the GUI and TUI projections can't drift
/// from the Rust enum variant names again. Adding a new variant
/// Rust-side is a one-line change in `Event::kind()`; this projection
/// just adds a new `match` arm for the payload shape.
pub fn bridge_event(evt: &SessionEvent) -> BridgedEvent {
    let kind = evt.kind();
    let payload = match evt {
        // v57 (H7 fix) — `State::name()` / `MessageRole::wire_label()`
        // are canonical labels owned by atelier-core; pre-v57 we
        // shipped `format!("{from:?}")` which made Rust's Debug a
        // wire format and would silently break the UI if a variant
        // got renamed.
        SessionEvent::Transitioned { from, to } => json!({
            "from": from.name(),
            "to": to.name(),
        }),
        SessionEvent::IllegalTransitionAttempted { from, to } => json!({
            "from": from.name(),
            "to": to.name(),
        }),
        SessionEvent::Cancelled => Value::Null,
        SessionEvent::EditStaged { path, hunks } => json!({
            "path": path.to_string_lossy(),
            "hunks": serde_json::to_value(hunks).unwrap_or(Value::Null),
        }),
        SessionEvent::MessageCommitted { role, text } => json!({
            "role": role.wire_label(),
            "text": text,
        }),
        SessionEvent::PlanSnapshot { steps } => json!({
            "steps": serde_json::to_value(steps).unwrap_or(Value::Null),
        }),
        SessionEvent::LedgerAppended { entry } => json!({
            "entry": serde_json::to_value(entry).unwrap_or(Value::Null),
        }),
        SessionEvent::ContextSnapshot {
            known_tokens,
            unknown_tokens,
        } => json!({
            "known_tokens": known_tokens,
            "unknown_tokens": unknown_tokens,
        }),
        SessionEvent::StagingPendingApproval { commit_id, files } => json!({
            "commit_id": commit_id.to_string(),
            "files": files
                .iter()
                .map(|f| json!({
                    "path": f.path.to_string_lossy(),
                    "hunks": serde_json::to_value(&f.hunks).unwrap_or(Value::Null),
                }))
                .collect::<Vec<_>>(),
        }),
        SessionEvent::CommitDecision {
            commit_id,
            committed,
            dropped,
        } => json!({
            "commit_id": commit_id.to_string(),
            "committed": committed
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
            "dropped": dropped
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
        }),
        SessionEvent::ModelProfileLoaded {
            model_id,
            base_url,
            strategy,
            outcome,
            capability_row,
        } => json!({
            "model_id": model_id,
            "base_url": base_url,
            "strategy": strategy.as_str(),
            // `ProbeLoadOutcome` derives `Serialize` with
            // `rename_all = "snake_case"`, so `cache_hit` /
            // `probed` / `reprobed` / `not_cached` land on the
            // wire as legible labels suitable for direct UI use.
            "outcome": serde_json::to_value(outcome).unwrap_or(Value::Null),
            // v60.7 §1 BYOM — capability matrix row. The Svelte
            // footer renders this as a tooltip on the model badge
            // so the user can spot a `ClaimedButBroken` cell
            // without opening a separate panel.
            // `CapabilityMatrixRow` already derives Serialize with
            // serde-rename-all=snake_case so pass through verbatim.
            "capability_row": serde_json::to_value(capability_row).unwrap_or(Value::Null),
        }),
        SessionEvent::ContextItems { items } => json!({
            // `ContextItemSummary` already derives Serialize with
            // snake_case fields — pass through verbatim.
            "items": serde_json::to_value(items).unwrap_or(Value::Null),
        }),
        SessionEvent::MemoryCards { cards } => json!({
            // `MemoryCardSummary` derives Serialize verbatim.
            "cards": serde_json::to_value(cards).unwrap_or(Value::Null),
        }),
        SessionEvent::ClaimedChanges { changes } => json!({
            "changes": changes
                .iter()
                .map(|c| json!({
                    "path": c.path,
                    "kind": c.kind,
                    "summary": c.summary,
                }))
                .collect::<Vec<_>>(),
        }),
        SessionEvent::Shutdown => Value::Null,
        SessionEvent::CompactionExecuted {
            freed_tokens,
            replaced_item_count,
            summary_card_id,
        } => json!({
            "freed_tokens": freed_tokens,
            "replaced_item_count": replaced_item_count,
            "summary_card_id": summary_card_id,
        }),
        SessionEvent::ExpansionExecuted {
            restored_item_count,
            summary_card_id,
            cache_rewarm_tokens,
        } => json!({
            "restored_item_count": restored_item_count,
            "summary_card_id": summary_card_id,
            "cache_rewarm_tokens": cache_rewarm_tokens,
        }),
        SessionEvent::MentalModelSnapshot {
            enabled,
            text_tokens,
        } => json!({
            "enabled": enabled,
            "text_tokens": text_tokens,
        }),
        // v61 — §14 concurrent-edit signals. Paths are stringified
        // verbatim for the webview to render; the wire label of the
        // resolution outcome ("reload" / "wait" / "pause" /
        // "auto_reload" / "pause_timed_out") is the canonical form
        // owned by atelier-core.
        SessionEvent::FilesChanged { paths, observed_at } => json!({
            "paths": paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "observed_at": observed_at,
        }),
        SessionEvent::FilesChangedAcknowledged { outcome } => json!({
            "outcome": outcome.wire_label(),
        }),
        SessionEvent::StrategyDegraded { from, to, reason } => json!({
            // Use the stable `as_str` wire labels (`native_tool` /
            // `json_sentinel` / `regex_prose`) so the Svelte reducer
            // can compare directly against `currentModel.strategy`
            // without re-deriving labels.
            "from": from.as_str(),
            "to": to.as_str(),
            "reason": reason,
        }),
    };
    BridgedEvent { kind, payload }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelier_core::diff::Hunks;
    use atelier_core::state::State;
    use std::path::PathBuf;

    #[test]
    fn bridge_transitioned_event() {
        let b = bridge_event(&SessionEvent::Transitioned {
            from: State::Idle,
            to: State::Streaming,
        });
        assert_eq!(b.kind, "Transitioned");
        assert_eq!(b.payload["from"], "Idle");
        assert_eq!(b.payload["to"], "Streaming");
    }

    #[test]
    fn bridge_illegal_transition_event() {
        let b = bridge_event(&SessionEvent::IllegalTransitionAttempted {
            from: State::Done,
            to: State::Streaming,
        });
        assert_eq!(b.kind, "IllegalTransitionAttempted");
        assert_eq!(b.payload["from"], "Done");
    }

    #[test]
    fn bridge_cancelled_has_null_payload() {
        let b = bridge_event(&SessionEvent::Cancelled);
        assert_eq!(b.kind, "Cancelled");
        assert!(b.payload.is_null());
    }

    #[test]
    fn bridge_edit_staged_event_carries_path_and_hunks() {
        let b = bridge_event(&SessionEvent::EditStaged {
            path: PathBuf::from("/tmp/foo.rs"),
            hunks: Hunks::Binary,
        });
        assert_eq!(b.kind, "EditStaged");
        assert_eq!(b.payload["path"], "/tmp/foo.rs");
        assert!(b.payload["hunks"].is_object() || b.payload["hunks"].is_string());
    }

    #[test]
    fn bridge_shutdown_event() {
        let b = bridge_event(&SessionEvent::Shutdown);
        assert_eq!(b.kind, "Shutdown");
        assert!(b.payload.is_null());
    }

    #[test]
    fn bridged_event_serializes_to_kind_and_payload_object() {
        let b = bridge_event(&SessionEvent::Cancelled);
        let v = serde_json::to_value(&b).unwrap();
        assert!(v.is_object());
        assert_eq!(v["kind"], "Cancelled");
        assert!(v.get("payload").is_some());
    }

    // ---------- PC-5: new bus variants ----------

    #[test]
    fn bridge_message_committed_carries_role_and_text() {
        let b = bridge_event(&SessionEvent::MessageCommitted {
            role: atelier_core::session::MessageRole::Assistant,
            text: "starting the rename".into(),
        });
        assert_eq!(b.kind, "MessageCommitted");
        assert_eq!(b.payload["role"], "assistant");
        assert_eq!(b.payload["text"], "starting the rename");
    }

    #[test]
    fn bridge_plan_snapshot_carries_steps_array() {
        use atelier_core::plan::{PlanStatus, PlanStep};
        let b = bridge_event(&SessionEvent::PlanSnapshot {
            steps: vec![PlanStep {
                id: "step-0".into(),
                text: "first".into(),
                status: PlanStatus::Pending,
                constraints: vec![],
            }],
        });
        assert_eq!(b.kind, "PlanSnapshot");
        assert!(b.payload["steps"].is_array());
        assert_eq!(b.payload["steps"][0]["text"], "first");
    }

    #[test]
    fn bridge_ledger_appended_carries_entry() {
        use atelier_core::ledger::LedgerEntry;
        let b = bridge_event(&SessionEvent::LedgerAppended {
            entry: LedgerEntry::tool_call("t", "shell", 1.0, Some(0.001), None),
        });
        assert_eq!(b.kind, "LedgerAppended");
        assert_eq!(b.payload["entry"]["kind"], "tool_call");
        assert_eq!(b.payload["entry"]["tool_name"], "shell");
    }

    #[test]
    fn bridge_context_snapshot_carries_known_and_unknown() {
        let b = bridge_event(&SessionEvent::ContextSnapshot {
            known_tokens: 3_200,
            unknown_tokens: 150,
        });
        assert_eq!(b.kind, "ContextSnapshot");
        assert_eq!(b.payload["known_tokens"], 3_200);
        assert_eq!(b.payload["unknown_tokens"], 150);
    }

    // ---------- HR-F: pending-approval bridge ----------

    #[test]
    fn bridge_staging_pending_approval_carries_commit_id_and_files() {
        use atelier_core::session::PendingFile;
        let cid = uuid::Uuid::new_v4();
        let b = bridge_event(&SessionEvent::StagingPendingApproval {
            commit_id: cid,
            files: vec![PendingFile {
                path: PathBuf::from("src/foo.rs"),
                hunks: Hunks::Binary,
            }],
        });
        assert_eq!(b.kind, "StagingPendingApproval");
        assert_eq!(b.payload["commit_id"], cid.to_string());
        assert!(b.payload["files"].is_array());
        assert_eq!(b.payload["files"][0]["path"], "src/foo.rs");
    }

    #[test]
    fn bridge_commit_decision_lists_committed_and_dropped_paths() {
        let cid = uuid::Uuid::new_v4();
        let b = bridge_event(&SessionEvent::CommitDecision {
            commit_id: cid,
            committed: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            dropped: vec![PathBuf::from("c.rs")],
        });
        assert_eq!(b.kind, "CommitDecision");
        assert_eq!(b.payload["commit_id"], cid.to_string());
        assert_eq!(b.payload["committed"][0], "a.rs");
        assert_eq!(b.payload["committed"][1], "b.rs");
        assert_eq!(b.payload["dropped"][0], "c.rs");
    }

    #[test]
    fn bridge_memory_cards_passes_cards_through_verbatim() {
        use atelier_core::memory::MemoryCardSummary;

        let cards = vec![MemoryCardSummary {
            id: "mem-1".into(),
            title: "user prefers tabs".into(),
            body_preview: "chose this in turn 2".into(),
            created_at: "2026-05-17T10:00:00Z".into(),
            last_used: "2026-05-17T12:00:00Z".into(),
            pinned: true,
            compacted_from: None,
            cache_rewarm_tokens: None,
        }];
        let b = bridge_event(&SessionEvent::MemoryCards {
            cards: cards.clone(),
        });
        assert_eq!(b.kind, "MemoryCards");
        let wire = b.payload["cards"].as_array().expect("cards array");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["id"], "mem-1");
        assert_eq!(wire[0]["title"], "user prefers tabs");
        assert_eq!(wire[0]["pinned"], true);
        assert_eq!(wire[0]["body_preview"], "chose this in turn 2");
    }

    #[test]
    fn bridge_context_items_passes_items_through_verbatim() {
        use atelier_core::context::ContextItemSummary;

        let items = vec![ContextItemSummary {
            id: "msg-0001-user_message".into(),
            kind: "user_message".into(),
            label: "fix the failing test".into(),
            provenance: "user_attached".into(),
            provenance_detail: None,
            tokens: 5,
            token_source: "approx".into(),
            pinned: false,
        }];
        let b = bridge_event(&SessionEvent::ContextItems {
            items: items.clone(),
        });
        assert_eq!(b.kind, "ContextItems");
        let wire = b.payload["items"].as_array().expect("items array");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["kind"], "user_message");
        assert_eq!(wire[0]["label"], "fix the failing test");
        assert_eq!(wire[0]["token_source"], "approx");
        assert_eq!(wire[0]["pinned"], false);
    }

    #[test]
    fn bridge_claimed_changes_passes_per_file_summary() {
        use atelier_core::session::ClaimedChangeSummary;
        let b = bridge_event(&SessionEvent::ClaimedChanges {
            changes: vec![
                ClaimedChangeSummary {
                    path: "src/lib.rs".into(),
                    kind: "edit".into(),
                    summary: "tighten error handling around the parser".into(),
                },
                ClaimedChangeSummary {
                    path: "tests/parser.rs".into(),
                    kind: "create".into(),
                    summary: "regression for issue #42".into(),
                },
            ],
        });
        assert_eq!(b.kind, "ClaimedChanges");
        let arr = b.payload["changes"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["path"], "src/lib.rs");
        assert_eq!(arr[0]["kind"], "edit");
        assert_eq!(
            arr[0]["summary"],
            "tighten error handling around the parser"
        );
        assert_eq!(arr[1]["kind"], "create");
    }

    #[test]
    fn parse_plan_status_accepts_all_four_labels() {
        use atelier_core::plan::PlanStatus;
        assert_eq!(parse_plan_status("pending").unwrap(), PlanStatus::Pending);
        assert_eq!(
            parse_plan_status("in_progress").unwrap(),
            PlanStatus::InProgress
        );
        assert_eq!(parse_plan_status("done").unwrap(), PlanStatus::Done);
        assert_eq!(parse_plan_status("skipped").unwrap(), PlanStatus::Skipped);
    }

    #[test]
    fn parse_plan_status_rejects_unknown_label() {
        let err = parse_plan_status("blocked").unwrap_err();
        assert!(err.contains("blocked"));
    }

    #[test]
    fn check_bytes_rejects_oversize_input() {
        // Regression for M-sec-1 — Tauri command sizes are bounded so
        // the webview can't ship arbitrarily large strings through the
        // bus to every subscriber.
        let s = "x".repeat(MAX_MEMORY_CARD_BYTES + 1);
        let err = check_bytes("memory card content", &s, MAX_MEMORY_CARD_BYTES).unwrap_err();
        assert!(err.contains("too long"));
    }

    #[test]
    fn is_safe_repo_relative_accepts_normal_paths_rejects_escapes() {
        assert!(is_safe_repo_relative("src/lib.rs"));
        assert!(is_safe_repo_relative("a.txt"));
        assert!(is_safe_repo_relative("nested/dir/file.go"));
        assert!(!is_safe_repo_relative(""));
        assert!(!is_safe_repo_relative("/etc/passwd"));
        assert!(!is_safe_repo_relative("../escape"));
        assert!(!is_safe_repo_relative("src/../../../etc/passwd"));
    }

    #[test]
    fn check_bytes_accepts_at_boundary() {
        let s = "x".repeat(MAX_MEMORY_CARD_BYTES);
        assert!(check_bytes("memory card content", &s, MAX_MEMORY_CARD_BYTES).is_ok());
    }

    #[test]
    fn bridge_model_profile_loaded_carries_id_strategy_and_outcome() {
        use atelier_core::adapter::model_profile::ProbeLoadOutcome;
        use atelier_core::protocol_strategy::Strategy;

        let b = bridge_event(&SessionEvent::ModelProfileLoaded {
            model_id: "local:qwen2.5-coder:7b".into(),
            base_url: "http://localhost:11434/v1".into(),
            strategy: Strategy::JsonSentinel,
            outcome: ProbeLoadOutcome::CacheHit,
            capability_row: None,
        });
        assert_eq!(b.kind, "ModelProfileLoaded");
        assert_eq!(b.payload["model_id"], "local:qwen2.5-coder:7b");
        assert_eq!(b.payload["base_url"], "http://localhost:11434/v1");
        assert_eq!(b.payload["strategy"], "json_sentinel");
        // Outcome is serialised through serde's snake_case rename,
        // which is what the footer renders directly.
        assert_eq!(b.payload["outcome"], "cache_hit");
        // v60.7 — capability_row rides on the same bridge.
        assert!(b.payload["capability_row"].is_null());
    }

    #[test]
    fn bridge_model_profile_loaded_includes_capability_row_when_set() {
        use atelier_core::adapter::capability_matrix;
        use atelier_core::adapter::model_profile::ProbeLoadOutcome;
        use atelier_core::adapter::Capabilities;
        use atelier_core::adapter::CapabilityClaim;
        use atelier_core::protocol_strategy::Strategy;

        let caps = Capabilities {
            native_tool_use: CapabilityClaim::Supported,
            streaming: CapabilityClaim::Supported,
            vision: CapabilityClaim::Supported,
            prompt_cache: CapabilityClaim::Supported,
            structured_output: CapabilityClaim::Supported,
            long_context: CapabilityClaim::Supported,
            context_window_tokens: 200_000,
        };
        let row = capability_matrix::matrix_row_for("anthropic:claude-opus-4-7", &caps);
        let b = bridge_event(&SessionEvent::ModelProfileLoaded {
            model_id: "anthropic:claude-opus-4-7".into(),
            base_url: String::new(),
            strategy: Strategy::NativeTool,
            outcome: ProbeLoadOutcome::CacheHit,
            capability_row: Some(row),
        });
        assert_eq!(
            b.payload["capability_row"]["model_id"],
            "anthropic:claude-opus-4-7"
        );
        assert_eq!(b.payload["capability_row"]["source"], "static");
        assert_eq!(b.payload["capability_row"]["native_tool_use"], "supported");
    }

    // ---------- v60.5: compaction wiring ----------

    #[test]
    fn bridge_compaction_executed_carries_freed_tokens_and_card_id() {
        let b = bridge_event(&SessionEvent::CompactionExecuted {
            freed_tokens: 12_345,
            replaced_item_count: 7,
            summary_card_id: "mem-abc".into(),
        });
        assert_eq!(b.kind, "CompactionExecuted");
        assert_eq!(b.payload["freed_tokens"], 12_345);
        assert_eq!(b.payload["replaced_item_count"], 7);
        assert_eq!(b.payload["summary_card_id"], "mem-abc");
    }

    #[test]
    fn bridge_memory_cards_passes_compacted_from_when_set() {
        use atelier_core::memory::MemoryCardSummary;

        let cards = vec![MemoryCardSummary {
            id: "mem-c".into(),
            title: "summary of …".into(),
            body_preview: "compacted from 7 items".into(),
            created_at: "2026-05-17T11:00:00Z".into(),
            last_used: "2026-05-17T11:00:00Z".into(),
            pinned: true,
            compacted_from: Some(7),
            cache_rewarm_tokens: Some(1234),
        }];
        let b = bridge_event(&SessionEvent::MemoryCards { cards });
        let wire = b.payload["cards"].as_array().expect("cards array");
        assert_eq!(wire[0]["compacted_from"], 7);
        assert_eq!(wire[0]["cache_rewarm_tokens"], 1234);
    }

    // ---------- v60.6: Expand wiring ----------

    #[test]
    fn bridge_expansion_executed_carries_count_card_and_cost() {
        let b = bridge_event(&SessionEvent::ExpansionExecuted {
            restored_item_count: 5,
            summary_card_id: "mem-abc".into(),
            cache_rewarm_tokens: 240,
        });
        assert_eq!(b.kind, "ExpansionExecuted");
        assert_eq!(b.payload["restored_item_count"], 5);
        assert_eq!(b.payload["summary_card_id"], "mem-abc");
        assert_eq!(b.payload["cache_rewarm_tokens"], 240);
    }

    // ---------- Phase C close: mental-model wiring ----------

    #[test]
    fn bridge_mental_model_snapshot_carries_enabled_and_text_tokens() {
        let b = bridge_event(&SessionEvent::MentalModelSnapshot {
            enabled: true,
            text_tokens: 42,
        });
        assert_eq!(b.kind, "MentalModelSnapshot");
        assert_eq!(b.payload["enabled"], true);
        assert_eq!(b.payload["text_tokens"], 42);
    }

    // ---------- §1 BYOM: conformance-driven degradation ----------

    #[test]
    fn bridge_strategy_degraded_uses_stable_wire_labels() {
        let b = bridge_event(&SessionEvent::StrategyDegraded {
            from: atelier_core::protocol_strategy::Strategy::NativeTool,
            to: atelier_core::protocol_strategy::Strategy::JsonSentinel,
            reason: "3 malformed envelopes in last 20 calls".into(),
        });
        assert_eq!(b.kind, "StrategyDegraded");
        // The labels must exactly match what `currentModel.strategy`
        // carries on the wire — same `as_str` source. Pin them so a
        // rename of the enum can't silently drift the projection.
        assert_eq!(b.payload["from"], "native_tool");
        assert_eq!(b.payload["to"], "json_sentinel");
        assert_eq!(
            b.payload["reason"],
            "3 malformed envelopes in last 20 calls"
        );
    }
}
