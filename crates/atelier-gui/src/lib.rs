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
use atelier_core::adapter::{Message, Role, ToolCallRequest};
use atelier_core::dispatcher::ApprovalPolicy;
use atelier_core::protocol::Envelope;
use atelier_core::protocol_strategy::HARNESS_META_NAME;
use atelier_core::session::{Event as SessionEvent, MessageRole};
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
    /// v60.45 — user-selectable workspace override. When `Some`,
    /// `workspace_root()` returns this path instead of the ephemeral
    /// tempdir; persisted across launches via `~/.atelier/gui.toml`.
    /// `std::sync::Mutex` (not `parking_lot`) because the lock is
    /// only taken on `get_workspace` / `set_workspace` / startup —
    /// contention is functionally zero.
    pub workspace_override: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// v60.28 H2 follow-on — pending `swap_adapter` consent gates,
    /// keyed by `swap_id` (UUID v4). The renderer's `respond_to_swap`
    /// reply pops the sender and signals the decision; `swap_adapter`
    /// awaits the receiver with a bounded timeout.
    pub pending_swaps: tokio::sync::Mutex<
        std::collections::HashMap<uuid::Uuid, tokio::sync::oneshot::Sender<SwapDecision>>,
    >,
    /// Session UUID of the most-recently completed `start_agent_run`.
    /// The next call to `start_agent_run` resumes from this session so
    /// conversation history, plan, and memory carry over across Composer
    /// submits. Cleared when the workspace changes (new session context).
    /// Arc so the spawned async task can clone it without a raw-pointer hack.
    pub active_session_id: Arc<std::sync::Mutex<Option<uuid::Uuid>>>,
}

/// v60.28 H2 follow-on — wire-format decision the renderer's consent
/// modal sends back through `respond_to_swap`.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SwapDecision {
    Accepted,
    Rejected,
}

/// v60.28 H2 follow-on — how long `swap_adapter` waits for the
/// renderer's reply before treating the swap as rejected. 120s is
/// generous enough for the user to read the modal and decide; a
/// hung webview can't pin the credential-bearing path forever.
const SWAP_CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl SessionState {
    /// Per-process workspace root. v60.45 — if `workspace_override`
    /// is `Some`, returns that path (user picked a real repo via
    /// `set_workspace`); otherwise returns the ephemeral tempdir.
    /// Returns `PathBuf` (owned) because the override is behind a
    /// mutex — there's no way to hand out a `&Path` reference that
    /// outlives the lock guard safely.
    pub fn workspace_root(&self) -> std::path::PathBuf {
        match self.workspace_override.lock().ok().and_then(|g| g.clone()) {
            Some(p) => p,
            None => self.workspace_tempdir.path().to_path_buf(),
        }
    }
}

/// Entry point. Spawned by `main.rs`; lives in `lib.rs` so the integration
/// tests can pull in the same module and exercise the helpers.
pub fn run() {
    tracing_subscriber::fmt::try_init().ok();

    tauri::Builder::default()
        // v60.46 — native folder picker for the workspace selector.
        .plugin(tauri_plugin_dialog::init())
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

            // v60.45 — restore the user's last-chosen workspace (if any).
            // A missing / unreadable / non-existent path is logged + falls
            // back to the tempdir, so a stale gui.toml never blocks startup.
            let workspace_override = std::sync::Mutex::new(
                load_persisted_workspace().filter(|p| {
                    let ok = p.is_dir();
                    if !ok {
                        tracing::warn!(
                            path = %p.display(),
                            "persisted workspace no longer exists or isn't a directory; falling back to tempdir"
                        );
                    }
                    ok
                }),
            );
            app.manage(SessionState {
                dispatcher_handle: DispatcherHandle::new(),
                adapter_handle: atelier_cli::AdapterHandle::new(),
                run_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                workspace_tempdir,
                workspace_override,
                pending_swaps: tokio::sync::Mutex::new(std::collections::HashMap::new()),
                active_session_id: Arc::new(std::sync::Mutex::new(None)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping,
            start_demo_run,
            // v60.43 — pure chat path (no Runner, no tools, no §3 staging).
            start_chat_run,
            // §10 — Runner-backed agent path (tools + sub-agents).
            start_agent_run,
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
            // v60.10 §1 BYOM mid-session provider swap (B2 real impl + C3 dropdown UI).
            swap_adapter,
            // v60.28 H2 follow-on — renderer reply for the consent modal.
            respond_to_swap,
            // v60.37 B5 — hydrate the dropdown from .atelier/providers.toml.
            list_provider_profiles,
            // v60.45 — workspace selector (set + read live workspace path).
            get_workspace,
            set_workspace,
            // v60.52 §15 Skills surface.
            list_skills,
            invoke_skill,
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

/// v60.34 (M25) — pure model-drift check. Used by
/// `compact_context_items` to guard against the swap-adapter race:
/// when `expected` is `Some`, reject the call if `live` doesn't match.
fn check_model_drift(expected: Option<&str>, live: &str) -> Result<(), String> {
    if let Some(expected) = expected {
        if live != expected {
            return Err(format!(
                "ModelDrift: compaction expected model {expected:?} but live adapter is {live:?}"
            ));
        }
    }
    Ok(())
}

#[tauri::command]
async fn compact_context_items(
    state: tauri::State<'_, SessionState>,
    ids: Vec<String>,
    expected_model_id: Option<String>,
) -> Result<CompactionResult, String> {
    if ids.len() > MAX_COMPACTION_IDS {
        return Err(format!(
            "compact_context_items: too many ids: {} (max {MAX_COMPACTION_IDS})",
            ids.len()
        ));
    }
    let sd = require_dispatcher(&state)?;
    let adapter = require_adapter(&state)?;
    // v60.34 (M25) — guard the §5 compaction call against a stale
    // adapter. The renderer is told the swap is live via
    // `AdapterSwapped` before the Runner observes it; a compaction
    // issued in that window would call the OLD adapter. The renderer
    // stamps each compaction with the model id it expected; we reject
    // with a typed ModelDrift signal if the live adapter has drifted.
    check_model_drift(expected_model_id.as_deref(), adapter.model_id())?;
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
        // v60.37 A3 — GUI demo path uses `LatencyWeighted` so Mock and
        // local-OpenAI-compat compactions get their local-rate cost
        // attribution. Cloud-provider compactions in the GUI are not
        // the cost-accounting surface of record (the runner's main loop
        // is); this is a best-effort default.
        atelier_cli::runner::ModelCostPolicy::LatencyWeighted,
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

/// v60.10 §1 BYOM — wire-format provider selector the webview sends on
/// `swap_adapter`. Mirrors `ProviderChoice` but stays serde-friendly
/// (no `Mock` variant — the webview shouldn't be able to swap into a
/// mock at runtime; that's a test seam).
#[derive(serde::Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SwapProviderWire {
    /// In-process test adapter. Accepted at the wire boundary so a
    /// scripted scenario or a future integration test can drive a
    /// mock swap through the same surface; production webview code
    /// should not send this.
    Mock {
        model_id: String,
    },
    Anthropic {
        model_id: String,
    },
    OpenAiCompat {
        model_id: String,
        #[serde(default)]
        base_url: Option<String>,
    },
}

/// v60.10 §1 BYOM — wire-format result returned by the `swap_adapter`
/// command. Carries the model id pair so the webview can render a
/// toast ("swapped from X → Y") without round-tripping back through
/// `currentModel`.
#[derive(Serialize, Debug)]
pub struct SwapResult {
    pub from_model_id: String,
    pub to_model_id: String,
    pub swapped_at: String,
}

/// v60.28 H2 — built-in base_url allowlist for the `swap_adapter` Tauri
/// command. A future revision will fold in user-configured entries from
/// `providers.toml`; the wired-in set covers the two public providers
/// the binary supports plus loopback.
pub const SWAP_BASE_URL_ALLOWLIST: &[&str] = &[
    "api.anthropic.com",
    "api.openai.com",
    "localhost",
    "127.0.0.1",
    "::1",
];

/// v60.28 H2 — predicate for whether a `swap_adapter` base_url is
/// allowed. `None` base_url (e.g. anthropic uses no `base_url`) is
/// allowed; only an explicit value off the allowlist is refused.
pub fn is_base_url_allowed(base_url: Option<&str>) -> bool {
    let Some(url) = base_url else {
        return true;
    };
    let host = match host_of_url(url) {
        Some(h) => h,
        None => return false,
    };
    SWAP_BASE_URL_ALLOWLIST.iter().any(|h| *h == host)
}

/// Bare host extraction matching `atelier_core::mcp::mcp_tool::host_of_url`
/// (kept local to avoid pulling in the mcp module for this single helper).
///
/// v60.37 B1 — now requires an explicit `http://` or `https://` prefix.
/// Without this, `host_of_url("localhost")` returned `Some("localhost")`
/// which the allowlist happily accepted, and `host_of_url("gopher://api.anthropic.com/x")`
/// returned `Some("api.anthropic.com")` — both defence-in-depth thinness
/// that a copy-paste of this helper into a future adapter could exploit.
/// Scheme comparison is case-insensitive.
fn host_of_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme_lc = scheme.to_ascii_lowercase();
    if scheme_lc != "http" && scheme_lc != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split_once(']').map(|(h, _)| h).unwrap_or(stripped)
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// v60.37 B5 — wire shape returned by [`list_provider_profiles`]. One
/// row per resolved named profile in `<workspace>/.atelier/providers.toml`
/// (or `~/.atelier/providers.toml` per [`ProvidersConfig::load`]'s
/// discovery order). The webview hydrates its swap dropdown from this
/// list on mount; no profile match → built-in defaults.
///
/// `base_url` is propagated so OpenAiCompat profiles route their swap
/// through the configured endpoint (not the env fallback). Anthropic +
/// Mock profiles carry `None` (the consent-modal allowlist gate ignores
/// it for those kinds).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SwapOptionWire {
    /// Matches `SwapProviderWire`'s tag values (snake_case): `mock`,
    /// `anthropic`, `openai_compat`.
    pub kind: String,
    /// Model id as the wire would render it (`anthropic:claude-…`,
    /// `local:qwen2.5-coder:7b`, `mock:default`, …).
    pub model_id: String,
    /// Human label for the `<option>`. Includes the profile name so a
    /// user with two openai-compat profiles can tell them apart.
    pub label: String,
    /// Resolved base_url for OpenAiCompat profiles. `None` for
    /// Anthropic and Mock (the swap-allowlist gate rejects a `base_url`
    /// on those kinds anyway).
    pub base_url: Option<String>,
    /// v60.55 — `true` iff this row matches `providers.toml`'s
    /// `default = "<name>"` selector. The webview renders a `★` next
    /// to default rows so the user can tell at a glance which profile
    /// the CLI / lazy-default chat path would pick. For the built-in
    /// fallback set (no `providers.toml` at all), the `mock` row is
    /// marked default since that's what `Runner::new` would pick.
    pub is_default: bool,
}

/// v60.37 B5 — hydrate the GUI's swap dropdown from
/// `.atelier/providers.toml`. Called on mount by `App.svelte`.
///
/// Behaviour:
///
/// * Loads via [`ProvidersConfig::load`], which already consults
///   `<workspace>/.atelier/providers.toml` then `~/.atelier/providers.toml`
///   and is capped at 1 MiB per v60.37 A2.
/// * Skips profiles where `provider` or `model` is `None` (incomplete
///   profiles can't drive a swap without further CLI flags; surfacing
///   them in the dropdown would invite a swap that fails halfway).
/// * Falls back to a built-in default list when no profile is
///   loadable, so first-run UX matches the pre-B5 hardcoded array.
///
/// Never returns an error: a malformed `providers.toml` is logged via
/// `tracing::warn!` and we fall back to defaults. The dropdown is
/// "polish + convenience" — failing it shouldn't disable the GUI.
#[tauri::command]
fn list_provider_profiles(state: tauri::State<'_, SessionState>) -> Vec<SwapOptionWire> {
    list_provider_profiles_in(&state.workspace_root())
}

/// Test-visible inner helper for [`list_provider_profiles`]. Splitting
/// the `tauri::State` wrapper out makes the projection logic exercisable
/// without booting a Tauri runtime.
pub fn list_provider_profiles_in(repo_root: &std::path::Path) -> Vec<SwapOptionWire> {
    list_provider_profiles_with_home(repo_root, None)
}

/// Variant that accepts an explicit home-dir override. `None` disables
/// the user scope (only `<repo>/.atelier/providers.toml` is read);
/// `Some(path)` reads `<path>/.atelier/providers.toml` as the
/// home-scope fallback. Production paths go through the no-override
/// helper above; tests pin a tempdir so they don't depend on the
/// developer's `~/.atelier/providers.toml` state.
pub fn list_provider_profiles_with_home(
    repo_root: &std::path::Path,
    home_override: Option<&std::path::Path>,
) -> Vec<SwapOptionWire> {
    use atelier_core::config::{ProviderKind, ProvidersConfig};

    // When tests opt into the override path with `None`, we mean
    // "skip user scope entirely". The `None` arm of
    // `ProvidersConfig::load_with_home` already does that; we just
    // pass the override through.
    let loaded = ProvidersConfig::load_with_home(repo_root, home_override);
    let mut out = Vec::new();
    match loaded {
        Ok(Some(loaded)) => {
            let default_name = loaded.config.default.as_deref();
            for (name, prof) in &loaded.config.providers {
                let (Some(kind), Some(model)) = (prof.provider, prof.model.as_ref()) else {
                    // Incomplete profile — skip rather than ship a row
                    // that would fail downstream when the user clicks it.
                    continue;
                };
                let (kind_str, base_url) = match kind {
                    ProviderKind::Mock => ("mock", None),
                    ProviderKind::Anthropic => ("anthropic", None),
                    ProviderKind::OpenaiCompat => ("openai_compat", prof.base_url.clone()),
                };
                out.push(SwapOptionWire {
                    kind: kind_str.to_string(),
                    model_id: model.clone(),
                    label: format!("{name} · {model}"),
                    base_url,
                    is_default: default_name == Some(name.as_str()),
                });
            }
        }
        Ok(None) => {
            // No file on disk; fall through to defaults below.
        }
        Err(e) => {
            tracing::warn!(error = %e, "list_provider_profiles: providers.toml malformed; falling back to built-in defaults");
        }
    }
    if out.is_empty() {
        out.extend(builtin_swap_defaults());
    }
    out
}

/// v60.37 B5 — built-in fallback set used when `providers.toml` is
/// absent or unparseable. Matches the pre-B5 hardcoded list in
/// `App.svelte` so first-run UX is unchanged.
fn builtin_swap_defaults() -> Vec<SwapOptionWire> {
    vec![
        SwapOptionWire {
            kind: "mock".into(),
            model_id: "mock:default".into(),
            label: "mock".into(),
            base_url: None,
            // v60.55 — mock is what `Runner::new` falls back to in the
            // absence of `providers.toml`, so the dropdown's default
            // marker should match.
            is_default: true,
        },
        SwapOptionWire {
            kind: "anthropic".into(),
            model_id: "anthropic:claude-opus-4-7".into(),
            label: "anthropic · claude-opus-4-7".into(),
            base_url: None,
            is_default: false,
        },
        SwapOptionWire {
            kind: "anthropic".into(),
            model_id: "anthropic:claude-sonnet-4-6".into(),
            label: "anthropic · claude-sonnet-4-6".into(),
            base_url: None,
            is_default: false,
        },
        SwapOptionWire {
            kind: "openai_compat".into(),
            model_id: "local:qwen2.5-coder:7b".into(),
            label: "openai-compat · local qwen2.5-coder:7b".into(),
            base_url: None,
            is_default: false,
        },
    ]
}

// ---------- v60.52 §15 Skills (GUI) ----------

/// Wire shape for [`list_skills`]. One row per registered skill,
/// override-resolved per [`atelier_core::skills::SkillRegistry`]'s
/// layered semantics. `prompt_template` is intentionally not
/// projected — the model never sees it from the webview side
/// (expansion happens server-side in [`invoke_skill`]).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SkillWire {
    pub name: String,
    pub description: String,
    pub proactive: bool,
    /// One of `"bundled"`, `"home"`, `"repo"` — matches
    /// `SkillSource::as_str()`. Stable wire labels so future renames
    /// of the Rust enum don't break Svelte consumers.
    pub source: String,
    /// `args[*].name` projected so the Composer's autocomplete can
    /// show what fields a skill takes without round-tripping the
    /// full manifest.
    pub args: Vec<String>,
}

/// v60.52 S09 — hydrate the Composer's slash-autocomplete dropdown.
/// Loads bundled + `~/.atelier/skills/` + `<workspace>/.atelier/skills/`
/// via the same loader the CLI uses; tolerates missing layers; never
/// returns an error (a malformed user manifest logs `warn!` and is
/// skipped via the registry's tolerant-load semantics — same shape as
/// `list_provider_profiles`).
#[tauri::command]
fn list_skills(state: tauri::State<'_, SessionState>) -> Vec<SkillWire> {
    list_skills_in(&state.workspace_root())
}

/// Test-visible inner helper for [`list_skills`].
pub fn list_skills_in(repo_root: &std::path::Path) -> Vec<SkillWire> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let registry = match atelier_core::skills::SkillRegistry::load(repo_root, home.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_skills: registry load failed; returning empty");
            return Vec::new();
        }
    };
    registry
        .iter()
        .map(|s| SkillWire {
            name: s.name.clone(),
            description: s.description.clone(),
            proactive: s.is_proactive(),
            source: s.source.as_str().to_string(),
            args: s.args.iter().map(|a| a.name.clone()).collect(),
        })
        .collect()
}

/// v60.52 S10 — invoke a skill: expand its `prompt_template` from the
/// supplied args + repo context, then route the expansion through the
/// existing `start_chat_run` pipeline. The user sees the expanded
/// body in the conversation pane as the User message (the plan's
/// open question #2 — landing on "show the expansion" for v1; flip
/// to "show the literal slash" later if user feedback asks).
///
/// Errors map to a stringified message so the Composer can surface
/// them inline.
#[tauri::command]
fn invoke_skill(
    app: AppHandle,
    state: tauri::State<'_, SessionState>,
    name: String,
    args: std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let workspace = state.workspace_root();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let registry = atelier_core::skills::SkillRegistry::load(&workspace, home.as_deref())
        .map_err(|e| format!("skills: {e}"))?;
    let skill = registry
        .get(&name)
        .ok_or_else(|| format!("unknown skill `/{name}`"))?;
    let core_args: std::collections::BTreeMap<String, String> = args.into_iter().collect();
    let ctx = atelier_core::skills::SkillSubstitutionContext {
        repo_root: &workspace,
        args: &core_args,
        atelier_md: None,
    };
    let expanded = atelier_core::skills::substitute(skill, &ctx)
        .map_err(|e| format!("skill `/{name}`: {e}"))?;
    // Delegate to the Runner-backed path so sub-agents, tools, and §7
    // verification are available during skill execution.
    start_agent_run(app, state, expanded)
}

/// v60.10 §1 BYOM — build a fresh `Arc<dyn Adapter>` from a
/// `SwapProviderWire`. Mirrors the per-provider construction logic in
/// `Runner::new`; lifted here so the swap-from-webview path doesn't
/// have to go through the full `Runner` constructor. Reads
/// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` from the environment, same
/// as the binary path.
fn build_swap_adapter(
    provider: SwapProviderWire,
) -> Result<std::sync::Arc<dyn atelier_core::adapter::Adapter>, String> {
    use atelier_core::adapter::{
        anthropic::AnthropicAdapter, openai_compat::OpenAiCompatAdapter, MockAdapter,
    };
    match provider {
        SwapProviderWire::Mock { model_id } => Ok(std::sync::Arc::new(MockAdapter::new(model_id))),
        SwapProviderWire::Anthropic { model_id } => {
            let a = AnthropicAdapter::from_env(model_id).map_err(|e| e.to_string())?;
            Ok(std::sync::Arc::new(a))
        }
        SwapProviderWire::OpenAiCompat { model_id, base_url } => {
            let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            let base = base_url.unwrap_or_else(|| {
                std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
            });
            Ok(std::sync::Arc::new(OpenAiCompatAdapter::new(
                api_key, model_id, base,
            )))
        }
    }
}

/// v60.10 §1 BYOM — mid-session provider swap. Builds the new adapter
/// from the wire payload, swaps it into the live `AdapterHandle` slot
/// (so the §5 compaction Tauri command sees the new adapter on its
/// next call), and emits an `AdapterSwapped` event directly to the
/// webview alongside a fresh `ModelProfileLoaded` so the footer
/// refreshes the model badge + capability tooltip.
///
/// In-flight `chat()` futures held by the Runner's run loop are NOT
/// cancelled — the run loop reads `Runner::adapter` per turn, so a
/// fully running-loop swap requires the caller to cancel + relaunch
/// the run via `with_resume`. v60.10 lands the surface; the
/// run-loop-aware swap is a follow-on bundle.
#[tauri::command]
async fn swap_adapter(
    app: AppHandle,
    state: tauri::State<'_, SessionState>,
    provider: SwapProviderWire,
) -> Result<SwapResult, String> {
    // v60.28 H2 — base_url allowlist gate. Refuses the swap before any
    // credential build / event emission so a hostile webview message
    // can't peel `OPENAI_API_KEY` to an arbitrary host.
    //
    // v60.37 B2 — resolve the EFFECTIVE base_url (wire-format value OR
    // `OPENAI_BASE_URL` env fallback) BEFORE the allowlist check.
    // Without this, a malicious .envrc setting `OPENAI_BASE_URL=http://attacker.test/v1`
    // would let an OpenAiCompat swap with `base_url: None` route past the
    // allowlist (which saw None) and then exfiltrate OPENAI_API_KEY to the
    // attacker URL via build_swap_adapter's env-fallback.
    let pending_base_url = match &provider {
        SwapProviderWire::OpenAiCompat { base_url, .. } => base_url
            .clone()
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok()),
        SwapProviderWire::Anthropic { .. } | SwapProviderWire::Mock { .. } => None,
    };
    let pending_to_id = match &provider {
        SwapProviderWire::Mock { model_id }
        | SwapProviderWire::Anthropic { model_id }
        | SwapProviderWire::OpenAiCompat { model_id, .. } => model_id.clone(),
    };
    if !is_base_url_allowed(pending_base_url.as_deref()) {
        let reason = format!(
            "base_url {:?} not in swap_adapter allowlist",
            pending_base_url.as_deref().unwrap_or("<none>")
        );
        emit_event(
            &app,
            &SessionEvent::AdapterSwapRejected {
                swap_id: None,
                to_model_id: pending_to_id,
                reason: reason.clone(),
            },
        );
        return Err(reason);
    }
    // Open the consent modal. Mint a per-swap UUID so the renderer's
    // `respond_to_swap` reply can correlate (and a stale reply after a
    // new swap has started is silently dropped). Register the oneshot
    // sender BEFORE emitting `AdapterSwapPending` so a fast accept
    // round-trip can't race the listener.
    let swap_id = uuid::Uuid::new_v4();
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel::<SwapDecision>();
    state
        .pending_swaps
        .lock()
        .await
        .insert(swap_id, decision_tx);
    emit_event(
        &app,
        &SessionEvent::AdapterSwapPending {
            swap_id: swap_id.to_string(),
            to_model_id: pending_to_id.clone(),
            base_url: pending_base_url.clone().unwrap_or_default(),
        },
    );
    let decision = match tokio::time::timeout(SWAP_CONSENT_TIMEOUT, decision_rx).await {
        Ok(Ok(d)) => d,
        Ok(Err(_recv_err)) => {
            // Sender dropped without sending — treat as Rejected.
            let reason = "consent channel closed without reply".to_string();
            emit_event(
                &app,
                &SessionEvent::AdapterSwapRejected {
                    swap_id: Some(swap_id.to_string()),
                    to_model_id: pending_to_id.clone(),
                    reason: reason.clone(),
                },
            );
            return Err(reason);
        }
        Err(_elapsed) => {
            // Timed out waiting for the user. Drop the registry slot so
            // a late `respond_to_swap` is a no-op.
            state.pending_swaps.lock().await.remove(&swap_id);
            let reason = format!(
                "consent timed out after {}s",
                SWAP_CONSENT_TIMEOUT.as_secs()
            );
            emit_event(
                &app,
                &SessionEvent::AdapterSwapRejected {
                    swap_id: Some(swap_id.to_string()),
                    to_model_id: pending_to_id.clone(),
                    reason: reason.clone(),
                },
            );
            return Err(reason);
        }
    };
    if matches!(decision, SwapDecision::Rejected) {
        let reason = "user rejected the swap".to_string();
        emit_event(
            &app,
            &SessionEvent::AdapterSwapRejected {
                swap_id: Some(swap_id.to_string()),
                to_model_id: pending_to_id.clone(),
                reason: reason.clone(),
            },
        );
        return Err(reason);
    }
    // v60.41 — emit `AdapterSwapRejected` on build failure so the
    // consent modal closes when adapter construction fails (e.g.
    // `AnthropicAdapter::from_env` returns `NotConfigured` because
    // `ANTHROPIC_API_KEY` is unset). Before this fix, the user saw
    // their first Accept click succeed (`respond_to_swap` returns Ok)
    // then the modal sat orphaned because no terminal event ever
    // arrived to clear `pendingSwap` in the reducer. A second click
    // on Accept hit the now-empty registry and surfaced
    // "no pending swap with id <uuid>" — a confusing downstream
    // symptom of the missing rejection event.
    let new_adapter = match build_swap_adapter(provider) {
        Ok(a) => a,
        Err(e) => {
            emit_event(
                &app,
                &SessionEvent::AdapterSwapRejected {
                    swap_id: Some(swap_id.to_string()),
                    to_model_id: pending_to_id.clone(),
                    reason: e.clone(),
                },
            );
            return Err(e);
        }
    };
    let to_model_id = new_adapter.model_id().to_string();
    // Read the pre-swap model id off the live adapter slot. If
    // nothing is in flight (no active run), use "<none>" so the
    // event still has a stable from-field.
    let from_model_id = state
        .adapter_handle
        .get()
        .map(|a| a.model_id().to_string())
        .unwrap_or_else(|| "<none>".to_string());
    // Push the new adapter into the slot atomically. `swap` drops
    // the pre-swap Arc on the slot side — the run loop still
    // references it via `Runner::adapter` until the next `run()`
    // re-constructs from `Runner::new`, but the shared slot doesn't
    // hold both at once.
    state.adapter_handle.swap(new_adapter.clone());
    let now = now_rfc3339();
    // Emit `AdapterSwapped` directly to the webview. The Runner's
    // pending_swap queue isn't reachable from the Tauri command (the
    // Runner lives inside the spawned task); emit straight to the
    // webview bridge so the UI gets the signal without waiting for
    // the next `run()` startup.
    emit_event(
        &app,
        &SessionEvent::AdapterSwapped {
            from_model_id: from_model_id.clone(),
            to_model_id: to_model_id.clone(),
            swapped_at: now.clone(),
        },
    );
    // Re-emit `ModelProfileLoaded` so the footer's model badge +
    // capability tooltip refresh. We build a `Skip`-policy stub
    // profile from the adapter's declared capabilities — the full
    // probe round-trip is the responsibility of the next `run()`.
    let caps = new_adapter.capabilities();
    let strategy = if caps.native_tool_use.is_usable() {
        atelier_core::protocol_strategy::Strategy::NativeTool
    } else {
        atelier_core::protocol_strategy::Strategy::JsonSentinel
    };
    let profile = atelier_core::adapter::model_profile::ModelProfile::skipped_for_well_known(
        new_adapter.model_id(),
        strategy,
        caps.context_window_tokens,
        atelier_core::adapter::model_profile::DEFAULT_PROFILE_MAX_TOKENS,
        now.clone(),
    );
    let capability_row = {
        let base =
            atelier_core::adapter::capability_matrix::matrix_row_for(new_adapter.model_id(), &caps);
        atelier_core::adapter::capability_matrix::crosswalk_with_profile(base, &profile)
    };
    emit_event(
        &app,
        &SessionEvent::ModelProfileLoaded {
            model_id: profile.model_id.clone(),
            base_url: profile.base_url.clone(),
            strategy: profile.strategy,
            outcome: atelier_core::adapter::model_profile::ProbeLoadOutcome::CacheHit,
            capability_row: Some(capability_row),
        },
    );
    Ok(SwapResult {
        from_model_id,
        to_model_id,
        swapped_at: now,
    })
}

/// v60.28 H2 follow-on — renderer-side accept/reject reply for the
/// consent modal opened by `swap_adapter`. The renderer parses the
/// `swap_id` off the matching `AdapterSwapPending` event and echoes it
/// back here with its decision. A reply with a swap_id that isn't in
/// the pending registry (stale: the originating swap already timed out
/// or was answered) returns `Err` without touching adapter state.
#[tauri::command]
async fn respond_to_swap(
    state: tauri::State<'_, SessionState>,
    swap_id: String,
    decision: SwapDecision,
) -> Result<(), String> {
    let parsed = uuid::Uuid::parse_str(&swap_id).map_err(|e| format!("invalid swap_id: {e}"))?;
    let sender = {
        let mut map = state.pending_swaps.lock().await;
        map.remove(&parsed)
    };
    let Some(sender) = sender else {
        return Err(format!("no pending swap with id {swap_id}"));
    };
    sender
        .send(decision)
        .map_err(|_| "swap_adapter no longer awaiting reply".to_string())
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
        overflow: None,
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

// ────────────────────────────────────────────────────────────────────
// v60.47 — auto memory-card drafting on known-fixable adapter errors
// ────────────────────────────────────────────────────────────────────
//
// When `start_chat_run`'s `adapter.chat()` fails with a known-shape
// error, we draft a workspace-scope memory card describing the failure
// + a likely fix. The card lands at
// `<workspace>/.atelier/memory/auto_<slug>.md`. Slug is deterministic
// per error variant so repeat occurrences overwrite rather than
// accumulate.
//
// Cards are workspace-scope so they only fire when the user has
// pointed the GUI at a real repo via the workspace selector. If the
// active workspace is still the ephemeral tempdir, we skip — there's
// no point writing a card to a directory that'll be deleted on
// shutdown.

use atelier_core::adapter::AdapterError;

/// One auto-card draft: a slug (filename stem) and the markdown body
/// (including frontmatter) to write at
/// `<workspace>/.atelier/memory/auto_<slug>.md`.
struct AutoCard {
    slug: &'static str,
    body: String,
}

/// Pattern-match a chat error against the set of known-fixable
/// variants. Returns `None` for errors that don't map to a useful
/// note — we'd rather skip auto-drafting than fill the workspace
/// memory with vague placeholders.
fn auto_card_for_error(err: &AdapterError) -> Option<AutoCard> {
    let now = atelier_core::time::now_rfc3339();
    let (slug, description, body_md) = match err {
        AdapterError::Auth(msg) => (
            "auth_failure",
            "Adapter auth failed — credentials missing or expired.",
            format!(
                "The adapter rejected the request with an authentication error:\n\n\
                 ```\n{msg}\n```\n\n\
                 **Likely fix:**\n\
                 - For Anthropic: export `ANTHROPIC_API_KEY` (in `~/.envrc` for direnv users, or your shell rc).\n\
                 - For OpenAI / openai-compat against OpenAI itself: export `OPENAI_API_KEY`.\n\
                 - For a local server (mlx_lm.server, Ollama, vLLM): the server usually doesn't need a key; if it returned 401, check the server config rather than the env var.\n\n\
                 **How to verify:** rerun any prompt — a successful round-trip clears the issue.\n"
            ),
        ),
        AdapterError::NotConfigured(msg) => (
            "adapter_not_configured",
            "Adapter dependency missing — env var or config file not found.",
            format!(
                "The adapter refused to construct itself:\n\n\
                 ```\n{msg}\n```\n\n\
                 **Likely fix:** the named env var (e.g. `ANTHROPIC_API_KEY`) isn't set. Set it before relaunching the GUI; the adapter is built once at swap-time and won't re-read the environment until the next adapter swap.\n"
            ),
        ),
        AdapterError::Unreachable(msg) => (
            "provider_unreachable",
            "Provider's HTTP endpoint did not respond.",
            format!(
                "The adapter could not reach the configured `base_url`:\n\n\
                 ```\n{msg}\n```\n\n\
                 **Likely fix:**\n\
                 - For a local server: confirm it's running. On macOS, `lsof -i :8080 -sTCP:LISTEN` (or whatever port your `providers.toml` uses) should show a Python / llama-server process.\n\
                 - For mlx-lm specifically: `mlx_lm.server --model <id> --host 127.0.0.1 --port 8080 --chat-template-args '{{\"enable_thinking\": false}}'`.\n\
                 - For a cloud provider: check network / VPN / corporate proxy.\n"
            ),
        ),
        AdapterError::ContextOverflow {
            needed_tokens,
            limit_tokens,
        } => (
            "context_overflow",
            "Prompt exceeded the model's context window.",
            format!(
                "The conversation needed **{needed_tokens}** tokens but the active model only accepts **{limit_tokens}**.\n\n\
                 **Likely fix:**\n\
                 - Switch to a larger-context profile in `providers.toml` (Qwen3 8B = 32k, Qwen3-Coder-30B = 256k, Anthropic Claude = 200k).\n\
                 - Or use the Context panel's compact action to summarise old items into a memory card.\n\
                 - Or just start a fresh chat — recall picks up your promoted memory automatically.\n"
            ),
        ),
        AdapterError::RateLimited { retry_after_ms } => (
            "rate_limited",
            "Provider returned a rate-limit response.",
            format!(
                "The provider asked us to back off for **{retry_after_ms} ms**.\n\n\
                 **Likely fix:** wait the named window before retrying. If this repeats, check the provider's quota dashboard — you may need a higher tier or to throttle local request rate.\n"
            ),
        ),
        AdapterError::ResponseTooLarge { limit } => (
            "response_too_large",
            "Adapter response body exceeded its per-call cap.",
            format!(
                "The non-streaming HTTP response grew past **{limit} bytes** before reading completed.\n\n\
                 **Likely fix:** reduce `max_tokens` in `providers.toml`, or move to a streaming code path (the openai-compat adapter has `stream()`; chat mode uses `chat()` non-streaming today).\n"
            ),
        ),
        AdapterError::SseEventTooLarge { limit } => (
            "sse_event_too_large",
            "Streaming SSE event payload exceeded its per-event cap.",
            format!(
                "One SSE event's accumulated `data:` body grew past **{limit} bytes**.\n\n\
                 **Likely fix:** the provider is misbehaving (sending a single mega-event instead of per-token chunks). Switch providers or temporarily route to the non-streaming `chat()` path.\n"
            ),
        ),
        AdapterError::Provider { status, body: _ }
            if *status >= 500 =>
        {
            (
                "provider_5xx",
                "Provider returned a 5xx server error.",
                format!(
                    "The provider responded with status **{status}**. This is typically transient; retrying after a brief pause usually resolves it. If it persists for one provider but not another, check that provider's status page.\n"
                ),
            )
        }
        // Provider 4xx and Malformed are usually request-shape bugs in
        // the harness, not actionable user fixes — skip those.
        _ => return None,
    };
    let body = format!(
        "---\nname: auto-{slug}\ndescription: {description}\nmetadata:\n  type: feedback\n  auto: true\n  created_at: \"{now}\"\n---\n\n{body_md}"
    );
    Some(AutoCard { slug, body })
}

/// Write an auto-card to `<workspace>/.atelier/memory/auto_<slug>.md`.
/// Skips silently when the workspace is still the ephemeral tempdir
/// (no real project picked yet, indicated by `real_workspace == None`)
/// so we don't litter `/var/folders/`. Returns the resolved path on
/// success so the caller can surface it to the user.
///
/// Takes the workspace path by owned value rather than via
/// `&SessionState` so the spawned async task in `start_chat_run` can
/// call this without holding a state reference across `.await`.
fn write_workspace_auto_card(
    real_workspace: Option<&std::path::Path>,
    card: &AutoCard,
) -> std::io::Result<Option<std::path::PathBuf>> {
    let Some(workspace) = real_workspace else {
        // No real workspace picked yet; auto-cards would be lost.
        return Ok(None);
    };
    let dir = workspace.join(".atelier/memory");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("auto_{}.md", card.slug));
    // Atomic write via tempfile + persist; mirror the discipline used
    // by `atelier-core::memory::promote`.
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    use std::io::Write;
    tmp.write_all(card.body.as_bytes())?;
    tmp.flush()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(Some(path))
}

// ────────────────────────────────────────────────────────────────────
// v60.45 — workspace selector
// ────────────────────────────────────────────────────────────────────
//
// The GUI launches with an ephemeral tempdir as its workspace root.
// `set_workspace` lets the user point it at any directory (typically
// the repo they're driving from a coding session); the choice is
// persisted to `~/.atelier/gui.toml` so it survives relaunches.
//
// Validation is intentionally light: the path must exist and be a
// directory. We do NOT require it to be a git repo — atelier doesn't
// hard-depend on git, and many useful workspaces (a docs tree, a
// scratch dir) aren't repos.

/// Maximum length of a workspace path. 4 KiB is far above any sane
/// real path on every supported platform; the cap exists so a hostile
/// webview message can't ask us to canonicalise a 1 GB string.
const MAX_WORKSPACE_PATH_BYTES: usize = 4096;

#[tauri::command]
fn get_workspace(state: tauri::State<'_, SessionState>) -> String {
    state.workspace_root().to_string_lossy().into_owned()
}

/// Set the active workspace root. Validates that the path exists and
/// is a directory; canonicalises (resolves symlinks + relative
/// segments) so downstream consumers always see an absolute path;
/// persists the result so the next launch picks it up automatically.
///
/// Returns the canonicalised path on success so the frontend can
/// display the resolved form (e.g. `~` expanded, `..` collapsed).
#[tauri::command]
fn set_workspace(state: tauri::State<'_, SessionState>, path: String) -> Result<String, String> {
    if path.is_empty() {
        return Err("workspace path is empty".to_string());
    }
    if path.len() > MAX_WORKSPACE_PATH_BYTES {
        return Err(format!(
            "workspace path is {} bytes (max {} bytes)",
            path.len(),
            MAX_WORKSPACE_PATH_BYTES
        ));
    }
    // Expand a leading `~` to the user's home so the natural shell
    // shorthand works. We don't expand `$VAR`-style env refs — keep
    // the surface area narrow.
    let expanded = if let Some(rest) = path.strip_prefix("~") {
        let home = std::env::var_os("HOME").ok_or_else(|| "HOME not set".to_string())?;
        let mut p = std::path::PathBuf::from(home);
        // strip a leading '/' on `rest` so PathBuf::push doesn't reset.
        let rest = rest.trim_start_matches('/');
        if !rest.is_empty() {
            p.push(rest);
        }
        p
    } else {
        std::path::PathBuf::from(&path)
    };
    let canonical = std::fs::canonicalize(&expanded).map_err(|e| {
        format!(
            "could not resolve {:?}: {e} (does it exist?)",
            expanded.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!("{:?} is not a directory", canonical.display()));
    }
    // Swap under the mutex, then persist outside the lock so a slow
    // disk doesn't pin the read path.
    {
        let mut guard = state
            .workspace_override
            .lock()
            .map_err(|e| format!("workspace_override mutex poisoned: {e}"))?;
        *guard = Some(canonical.clone());
    }
    // New workspace → new conversation context; clear the resume pointer.
    if let Ok(mut guard) = state.active_session_id.lock() {
        *guard = None;
    }
    if let Err(e) = persist_workspace(&canonical) {
        // Persistence failure is non-fatal — the swap is already in
        // effect; just warn so the user sees it via tracing.
        tracing::warn!(error = %e, "failed to persist workspace selection to ~/.atelier/gui.toml");
    }
    Ok(canonical.to_string_lossy().into_owned())
}

/// `~/.atelier/gui.toml` schema:
///
/// ```toml
/// [workspace]
/// path = "/absolute/path/the/user/picked"
/// ```
fn gui_settings_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".atelier/gui.toml"))
}

fn load_persisted_workspace() -> Option<std::path::PathBuf> {
    let path = gui_settings_path()?;
    let body = std::fs::read_to_string(&path).ok()?;
    // Minimal hand-roll instead of pulling in serde_toml derive plumbing
    // for a 2-field file — find `[workspace]` then `path = "..."`.
    let mut in_workspace = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            in_workspace = trimmed == "[workspace]";
            continue;
        }
        if in_workspace {
            if let Some(rest) = trimmed.strip_prefix("path") {
                let rest = rest.trim().strip_prefix('=')?.trim();
                let unquoted = rest.trim_matches('"');
                if !unquoted.is_empty() {
                    return Some(std::path::PathBuf::from(unquoted));
                }
            }
        }
    }
    None
}

fn persist_workspace(path: &std::path::Path) -> std::io::Result<()> {
    let settings = gui_settings_path()
        .ok_or_else(|| std::io::Error::other("HOME not set; cannot resolve ~/.atelier/gui.toml"))?;
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "# atelier-gui settings — managed by the workspace selector.\n\
         # Edit by hand only if the GUI is closed; otherwise use the\n\
         # in-app picker so changes round-trip cleanly.\n\
         [workspace]\n\
         path = \"{}\"\n",
        path.to_string_lossy()
    );
    std::fs::write(&settings, body)
}

/// v60.44 — memory recall for chat mode. Reads every promoted card
/// from `~/.atelier/memory/*.md` (skipping the `MEMORY.md` index file)
/// and concatenates them into a single system-prompt string the chat
/// path prepends to the user's first message.
///
/// Files use a Jekyll-style frontmatter:
///
/// ```text
/// ---
/// name: <slug>
/// description: <one-liner>
/// metadata:
///   type: feedback
/// ---
///
/// <body>
/// ```
///
/// We strip the frontmatter (so the model sees only the rendered
/// content) but keep the `description:` line as a brief lead-in
/// because it's the part the user wrote as a TL;DR. Total recalled
/// memory is capped at `MEMORY_RECALL_BYTE_CAP` (16 KiB) — over the
/// cap, later files are dropped with a `tracing::warn`.
///
/// Returns `None` when there's nothing to recall (directory missing
/// or empty after exclusions) so the caller can skip the system
/// message entirely instead of sending an empty preamble.
const MEMORY_RECALL_BYTE_CAP: usize = 16 * 1024;

fn load_promoted_memory(workspace_root: Option<&std::path::Path>) -> Option<String> {
    // v60.47 — workspace-scope cards (auto-drafts from chat errors,
    // user-added notes scoped to a single repo) are read alongside
    // the global ones from `~/.atelier/memory/`. Workspace cards
    // sort after global so a project-specific note overrides a
    // global one when they describe the same topic (the chat is
    // free to read both — the order is just deterministic).
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".atelier/memory"));
    }
    if let Some(ws) = workspace_root {
        dirs.push(ws.join(".atelier/memory"));
    }
    let mut entries: Vec<std::path::PathBuf> = Vec::new();
    for dir in &dirs {
        let Ok(read) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read.flatten() {
            let p = entry.path();
            let keep = p.extension().map(|x| x == "md").unwrap_or(false)
                && p.file_name()
                    .and_then(|f| f.to_str())
                    .map(|n| n != "MEMORY.md" && n != "README.md")
                    .unwrap_or(false);
            if keep {
                entries.push(p);
            }
        }
    }
    if entries.is_empty() {
        return None;
    }
    // Stable order so the prompt is deterministic across launches.
    entries.sort();
    let mut out = String::from(
        "The following durable notes were promoted by the user across previous sessions. \
         Treat them as ground truth about the user's preferences and project context.\n\n",
    );
    let mut included = 0usize;
    for path in entries {
        let body = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rendered = strip_frontmatter(&body);
        let chunk = format!(
            "## {}\n\n{}\n\n",
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("memory"),
            rendered.trim()
        );
        if out.len() + chunk.len() > MEMORY_RECALL_BYTE_CAP {
            tracing::warn!(
                cap = MEMORY_RECALL_BYTE_CAP,
                "load_promoted_memory: byte cap reached; dropping {} and later",
                path.display()
            );
            break;
        }
        out.push_str(&chunk);
        included += 1;
    }
    if included == 0 {
        None
    } else {
        Some(out)
    }
}

/// Strip a `---\n...\n---\n` frontmatter block from the head of `s`.
/// Returns `s` unchanged if the frontmatter delimiters aren't present.
fn strip_frontmatter(s: &str) -> &str {
    let trimmed = s.trim_start();
    if !trimmed.starts_with("---") {
        return s;
    }
    // Skip the opening `---` line.
    let after_open = match trimmed.find('\n') {
        Some(i) => &trimmed[i + 1..],
        None => return s,
    };
    // Find the closing `---` on its own line.
    let mut search = after_open;
    let mut offset_from_start = trimmed.len() - after_open.len();
    while let Some(idx) = search.find("\n---") {
        offset_from_start += idx + 4;
        let after_close = &search[idx + 4..];
        // The closing fence should be followed by a newline (or EOF) to
        // qualify — otherwise it's just text that happens to contain `---`.
        if after_close.starts_with('\n') || after_close.is_empty() {
            let abs = (s.len() - trimmed.len()) + offset_from_start;
            return s[abs..].trim_start_matches('\n');
        }
        // Not a closing fence; continue searching past this position.
        search = &search[idx + 4..];
    }
    s
}

/// v60.43 — load the default profile from providers.toml and build
/// its adapter. Used by `start_chat_run` when the `adapter_handle`
/// slot is empty (no explicit dropdown swap has happened yet).
///
/// Why not require the user to click the dropdown? The dropdown
/// shows the providers.toml default as pre-selected on mount, but a
/// `<select>` doesn't fire `onchange` for the default-selected value
/// — only when the user actively picks a different option. So a
/// fresh-launch user who types a prompt against the visibly-selected
/// model would otherwise get "no adapter selected" with no way out
/// short of toggling the dropdown twice. This helper closes that
/// gap by activating the default exactly as the CLI does.
fn resolve_default_adapter(
    repo_root: &std::path::Path,
) -> Result<std::sync::Arc<dyn atelier_core::adapter::Adapter>, String> {
    use atelier_core::config::{ProviderKind, ProvidersConfig};
    let loaded = ProvidersConfig::load(repo_root)
        .map_err(|e| format!("providers.toml load failed: {e}"))?
        .ok_or_else(|| {
            "no providers.toml found at <workspace>/.atelier/ or ~/.atelier/ — \
             create one or pick a profile from the dropdown"
                .to_string()
        })?;
    let cfg = &loaded.config;
    // Pick the named default; if no `default = ...` is set, take the
    // first profile in iteration order (HashMap iteration is unstable
    // but the singleton case is what matters here).
    let (name, prof) = match cfg.default.as_deref() {
        Some(name) => cfg
            .providers
            .get(name)
            .map(|p| (name.to_string(), p))
            .ok_or_else(|| {
                format!(
                    "providers.toml `default = {name:?}` but no `[providers.{name}]` table defined"
                )
            })?,
        None => cfg
            .providers
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v))
            .ok_or_else(|| "providers.toml has no profiles defined".to_string())?,
    };
    let kind = prof
        .provider
        .ok_or_else(|| format!("profile {name:?} is missing `provider = ...`"))?;
    let model = prof
        .model
        .clone()
        .ok_or_else(|| format!("profile {name:?} is missing `model = ...`"))?;
    let wire = match kind {
        ProviderKind::Mock => SwapProviderWire::Mock { model_id: model },
        ProviderKind::Anthropic => SwapProviderWire::Anthropic { model_id: model },
        ProviderKind::OpenaiCompat => SwapProviderWire::OpenAiCompat {
            model_id: model,
            base_url: prof.base_url.clone(),
        },
    };
    build_swap_adapter(wire)
}

/// v60.43 — pure chat path. Bypasses the Runner / Mock / §3 staging
/// pipeline entirely. Takes the prompt, asks the live adapter for a
/// completion with NO tools advertised, emits the assistant reply
/// straight onto the bus. Use this when the user wants a chat REPL
/// rather than a scripted demo.
///
/// Why not just reuse `start_demo_run`?
///
/// `start_demo_run` wraps a Mock adapter with a scripted
/// `write_file` + `harness_meta` tool-call sequence designed to
/// exercise the §3 atomic-staging + AwaitApproval flow. Even when the
/// `adapter_handle` carries a live OpenaiCompat adapter (post-swap),
/// the Runner builds with `ProviderChoice::Mock` and registers the
/// full built-in tool surface — so a real adapter would receive a
/// ~2.5k-token prompt full of tool definitions and try to tool-call
/// its way through. On a 4B-class local model that costs minutes,
/// blows the HTTP timeout, and surfaces nothing.
///
/// `start_chat_run` is the inverse: zero tools, single round-trip,
/// the reply lands as `MessageCommitted { role: Assistant, .. }`.
/// Concurrent-run guard reuses `run_in_flight` so the Composer
/// disables correctly while a turn is in flight.
#[tauri::command]
fn start_chat_run(
    app: AppHandle,
    state: tauri::State<'_, SessionState>,
    prompt: String,
) -> Result<(), String> {
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "prompt too long: {} bytes (max {} bytes)",
            prompt.len(),
            MAX_PROMPT_BYTES
        ));
    }
    // Prefer whatever's in the handle (a prior explicit swap wins). Fall
    // back to lazily building the default profile from providers.toml so a
    // user who just opened the app and typed a prompt — without picking
    // anything in the dropdown — still gets a working adapter. The
    // dropdown's default-selected option visually matches providers.toml's
    // `default = ...`, so this matches user expectation.
    // Note `default_was_built` so we can emit a one-shot
    // `ModelProfileLoaded` after the adapter is in the slot — that gives
    // the footer's context-usage meter a `context_window_tokens` value
    // to divide against. Without it the meter renders as `N / 0` until
    // an explicit dropdown swap.
    let (adapter, default_was_built) = match state.adapter_handle.get() {
        Some(a) => (a, false),
        None => match resolve_default_adapter(&state.workspace_root()) {
            Ok(a) => {
                state.adapter_handle.swap(a.clone());
                (a, true)
            }
            Err(e) => return Err(e),
        },
    };
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
    // v60.47 — capture the real workspace path (the override, if set)
    // before spawning so the async task can write workspace-scope
    // auto-cards without re-locking `state.workspace_override`.
    let workspace_override = state.workspace_override.lock().ok().and_then(|g| g.clone());
    let app_clone = app.clone();
    let run_in_flight = state.run_in_flight.clone();
    tauri::async_runtime::spawn(async move {
        let _cleanup = ChatRunCleanup {
            in_flight: run_in_flight,
        };
        // First chat through the default-built adapter: synthesise a
        // `ModelProfileLoaded` so the footer's context-usage meter has a
        // window-size denominator. (The swap_adapter path emits this
        // already; the lazy-default path didn't until now.)
        if default_was_built {
            let caps = adapter.capabilities();
            let strategy = if caps.native_tool_use.is_usable() {
                atelier_core::protocol_strategy::Strategy::NativeTool
            } else {
                atelier_core::protocol_strategy::Strategy::JsonSentinel
            };
            let now = now_rfc3339();
            let profile =
                atelier_core::adapter::model_profile::ModelProfile::skipped_for_well_known(
                    adapter.model_id(),
                    strategy,
                    caps.context_window_tokens,
                    atelier_core::adapter::model_profile::DEFAULT_PROFILE_MAX_TOKENS,
                    now.clone(),
                );
            let capability_row = {
                let base = atelier_core::adapter::capability_matrix::matrix_row_for(
                    adapter.model_id(),
                    &caps,
                );
                atelier_core::adapter::capability_matrix::crosswalk_with_profile(base, &profile)
            };
            emit_event(
                &app_clone,
                &SessionEvent::ModelProfileLoaded {
                    model_id: profile.model_id.clone(),
                    base_url: profile.base_url.clone(),
                    strategy: profile.strategy,
                    outcome: atelier_core::adapter::model_profile::ProbeLoadOutcome::CacheHit,
                    capability_row: Some(capability_row),
                },
            );
        }
        // Echo the user message onto the bus first so the ConversationPane
        // renders it immediately, then the model reply lands as a second
        // MessageCommitted once the adapter returns.
        emit_event(
            &app_clone,
            &SessionEvent::MessageCommitted {
                role: MessageRole::User,
                text: prompt.clone(),
            },
        );
        // v60.44 — memory recall. Prepend a system message built from
        // every promoted card in `~/.atelier/memory/` so the model
        // starts every chat with the user's durable cross-session
        // context. No write-back: chat mode doesn't have a tool surface
        // for the model to create cards. Cards still get added/promoted
        // via the MemoryPane.
        let mut messages = Vec::with_capacity(2);
        if let Some(sys) = load_promoted_memory(workspace_override.as_deref()) {
            messages.push(Message::text(Role::System, sys));
        }
        messages.push(Message::text(Role::User, prompt));
        // Stream the response so the conversation pane renders text
        // word-by-word and the footer token meter updates continuously.
        let mut stream = match adapter.stream(&messages, &[]).await {
            Ok(s) => s,
            Err(e) => {
                emit_event(
                    &app_clone,
                    &SessionEvent::MessageCommitted {
                        role: MessageRole::System,
                        text: format!("[chat error] {e}"),
                    },
                );
                tracing::warn!(error = %e, "start_chat_run: adapter.stream failed");
                if let Some(card) = auto_card_for_error(&e) {
                    match write_workspace_auto_card(workspace_override.as_deref(), &card) {
                        Ok(Some(path)) => {
                            tracing::info!(
                                path = %path.display(),
                                slug = card.slug,
                                "start_chat_run: auto-drafted workspace memory card"
                            );
                            emit_event(
                                &app_clone,
                                &SessionEvent::MessageCommitted {
                                    role: MessageRole::System,
                                    text: format!(
                                        "[auto-memory] drafted workspace card at {} — review in MemoryPane or edit by hand",
                                        path.display()
                                    ),
                                },
                            );
                        }
                        Ok(None) => {
                            tracing::debug!(
                                "auto-card skipped: no workspace selected (still on tempdir)"
                            );
                        }
                        Err(io_err) => {
                            tracing::warn!(
                                error = %io_err,
                                "start_chat_run: failed to write auto memory card"
                            );
                        }
                    }
                }
                return;
            }
        };
        let mut final_resp: Option<atelier_core::adapter::ChatResponse> = None;
        let mut chunk_count: u32 = 0;
        loop {
            match stream.next().await {
                Some(atelier_core::adapter::StreamChunk::Text { delta }) => {
                    chunk_count += 1;
                    tracing::debug!(chunk = chunk_count, len = delta.len(), "stream text chunk");
                    emit_event(&app_clone, &SessionEvent::AssistantTextDelta { delta });
                }
                Some(atelier_core::adapter::StreamChunk::Complete { response }) => {
                    tracing::debug!(chunks = chunk_count, "stream complete");
                    final_resp = Some(response);
                    break;
                }
                Some(atelier_core::adapter::StreamChunk::Error { error }) => {
                    emit_event(
                        &app_clone,
                        &SessionEvent::MessageCommitted {
                            role: MessageRole::System,
                            text: format!("[chat error] {error}"),
                        },
                    );
                    tracing::warn!(error = %error, "start_chat_run: mid-stream error");
                    return;
                }
                Some(_) => {}  // ToolCall variants — not used in zero-tool chat mode
                None => break, // stream ended unexpectedly without Complete
            }
        }
        if let Some(resp) = final_resp {
            let used = resp.usage.prompt_tokens + resp.usage.completion_tokens;
            // Commit the full assembled text to conversation history;
            // the reducer clears streamingAssistant when this lands.
            emit_event(
                &app_clone,
                &SessionEvent::MessageCommitted {
                    role: MessageRole::Assistant,
                    text: resp.text.clone(),
                },
            );
            emit_event(
                &app_clone,
                &SessionEvent::LedgerAppended {
                    entry: atelier_core::ledger::LedgerEntry::ModelCall {
                        timestamp: atelier_core::time::now_rfc3339(),
                        model_id: adapter.model_id().to_string(),
                        prompt_tokens: resp.usage.prompt_tokens,
                        completion_tokens: resp.usage.completion_tokens,
                        cached_tokens: resp.usage.cached_tokens,
                        count_source: atelier_core::context::TokenSource::Exact,
                        latency_ms: None,
                        cost_usd: None,
                        note: None,
                    },
                },
            );
            emit_event(
                &app_clone,
                &SessionEvent::ContextSnapshot {
                    known_tokens: used,
                    unknown_tokens: 0,
                },
            );
            // Emit synthetic ContextItems so the Context pane shows the
            // conversation in chat mode (no ContextManager in this path).
            let mut items: Vec<atelier_core::context::ContextItemSummary> =
                Vec::with_capacity(messages.len() + 1);
            for (idx, msg) in messages.iter().enumerate() {
                let (provenance, token_source, tokens) = match msg.role {
                    Role::System => ("initial", "approx", (msg.content.len() as u32 + 3) / 4),
                    Role::User => (
                        "user_attached",
                        "approx",
                        resp.usage
                            .prompt_tokens
                            .saturating_sub((msg.content.len() as u32 + 3) / 4),
                    ),
                    _ => ("initial", "approx", (msg.content.len() as u32 + 3) / 4),
                };
                // For the user message use the full prompt_tokens count
                // (it includes any prepended system context).
                let tokens = if msg.role == Role::User {
                    resp.usage.prompt_tokens
                } else {
                    tokens
                };
                let label: String = msg
                    .content
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();
                items.push(atelier_core::context::ContextItemSummary {
                    id: format!("chat-{idx}"),
                    kind: "inline_text".to_string(),
                    label,
                    provenance: provenance.to_string(),
                    provenance_detail: None,
                    tokens,
                    token_source: token_source.to_string(),
                    pinned: false,
                });
            }
            // Assistant response as the final item.
            let asst_label: String = resp
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            items.push(atelier_core::context::ContextItemSummary {
                id: format!("chat-{}", messages.len()),
                kind: "inline_text".to_string(),
                label: asst_label,
                provenance: "assistant_turn".to_string(),
                provenance_detail: None,
                tokens: resp.usage.completion_tokens,
                token_source: "exact".to_string(),
                pinned: false,
            });
            emit_event(&app_clone, &SessionEvent::ContextItems { items });
        }
    });
    Ok(())
}

/// Runner-backed agent path. Unlike `start_chat_run` (which calls
/// `adapter.chat()` directly with no tools), this wires the prompt
/// through the full Runner so sub-agent spawning, tool calls, §7
/// verification, and all bus events propagate to the webview. Sub-agent
/// events in particular feed the `SubagentPane` in the GUI.
///
/// Approval policy defaults to `AutoApproveAll` (the Runner default) so
/// the run doesn't block awaiting a DiffPane that no longer exists in the
/// chat-REPL GUI.
#[tauri::command]
fn start_agent_run(
    app: AppHandle,
    state: tauri::State<'_, SessionState>,
    prompt: String,
) -> Result<(), String> {
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "prompt too long: {} bytes (max {} bytes)",
            prompt.len(),
            MAX_PROMPT_BYTES
        ));
    }
    let (adapter, _) = match state.adapter_handle.get() {
        Some(a) => (a, false),
        None => match resolve_default_adapter(&state.workspace_root()) {
            Ok(a) => {
                state.adapter_handle.swap(a.clone());
                (a, true)
            }
            Err(e) => return Err(e),
        },
    };
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
    // Use the user's chosen workspace if set; fall back to a per-run
    // temp subdir so the Runner always has a writable directory for
    // session.json.
    let workspace_override = state.workspace_override.lock().ok().and_then(|g| g.clone());
    let (workspace, cleanup_workspace) = if let Some(ref ws) = workspace_override {
        (ws.clone(), None)
    } else {
        let run_id = uuid::Uuid::new_v4();
        let ws = state.workspace_root().join(run_id.to_string());
        if let Err(e) = std::fs::create_dir_all(&ws) {
            state
                .run_in_flight
                .store(false, std::sync::atomic::Ordering::Release);
            return Err(format!("workspace setup failed: {e}"));
        }
        let ws_clone = ws.clone();
        (ws, Some(ws_clone))
    };

    let handle = state.dispatcher_handle.clone();
    let adapter_handle = state.adapter_handle.clone();
    let run_in_flight = state.run_in_flight.clone();
    // Clone the Arc so the spawned task can write the new session UUID back
    // without needing to hold a reference to `state` across an await point.
    let session_id_arc = Arc::clone(&state.active_session_id);
    let prior_session_id = session_id_arc.lock().ok().and_then(|g| *g);

    let app_clone = app.clone();
    let cb = Arc::new(move |evt: &SessionEvent| {
        emit_event(&app_clone, evt);
    });

    let runner = match Runner::new(
        workspace,
        ProviderChoice::Mock { responses: vec![] },
        EventSink::Callback(cb),
    ) {
        Ok(r) => r,
        Err(e) => {
            run_in_flight.store(false, std::sync::atomic::Ordering::Release);
            if let Some(ref ws) = cleanup_workspace {
                let _ = std::fs::remove_dir_all(ws);
            }
            return Err(format!("Runner::new failed: {e}"));
        }
    };
    // Resume from the prior session so conversation history, plan, and
    // memory carry over across Composer submits.
    let runner = if let Some(uuid) = prior_session_id {
        runner.with_resume(uuid)
    } else {
        runner
    };
    let runner = runner
        .with_adapter(adapter)
        .with_dispatcher_handle(handle)
        .with_adapter_handle(adapter_handle);

    // Emit immediately so the frontend shows a spinner before the first
    // real event (model probe can take 10-30s on a local model).
    let _ = app.emit(
        "atelier://event",
        BridgedEvent {
            kind: "RunStarted",
            payload: serde_json::Value::Null,
        },
    );

    let app_for_finish = app.clone();
    tauri::async_runtime::spawn(async move {
        let _cleanup = RunCleanup {
            in_flight: run_in_flight,
            workspace_to_remove: cleanup_workspace,
        };
        match runner.run(prompt).await {
            Ok(report) => {
                if let Ok(mut guard) = session_id_arc.lock() {
                    *guard = Some(report.session_id.0);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "agent run failed");
            }
        }
        let _ = app_for_finish.emit(
            "atelier://event",
            BridgedEvent {
                kind: "RunFinished",
                payload: serde_json::Value::Null,
            },
        );
    });
    Ok(())
}

/// v60.43 — minimal drop-guard for `start_chat_run`'s spawned task.
/// Only clears `run_in_flight`; there's no per-run workspace to
/// remove because the chat path doesn't construct a Runner.
struct ChatRunCleanup {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for ChatRunCleanup {
    fn drop(&mut self) {
        self.in_flight
            .store(false, std::sync::atomic::Ordering::Release);
    }
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
        SessionEvent::AssistantTextDelta { delta } => json!({
            "delta": delta,
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
        // v62 — §7 verify-pass terminal marker. The tier rides as its
        // canonical wire label (`tier1_lsp` / `tier2_tree_sitter` /
        // `tier3_textual` / `not_run`) so the Svelte reducer in
        // `state.ts` can switch on it directly without re-importing
        // the Rust enum.
        SessionEvent::VerificationPassed {
            tier,
            file_count,
            claim_count,
        } => json!({
            "tier": tier.wire_label(),
            "file_count": file_count,
            "claim_count": claim_count,
        }),
        // Phase A close — §7 lying-agent / silent-edit gate. Discrepancy
        // list rides as JSON objects keyed by `kind` (`claimed` /
        // `unclaimed` / `kind_mismatch` / `duplicate_claim`) + payload
        // fields the Svelte reducer in `state.ts` can switch on. The
        // red-failed UI badge lands in Phase C; this bridge arm pins
        // the wire shape so the Svelte side can subscribe today.
        SessionEvent::VerificationFailed {
            tier,
            discrepancies,
        } => json!({
            "tier": tier.wire_label(),
            "discrepancy_count": discrepancies.len(),
            "discrepancies": discrepancies
                .iter()
                .map(|d| match d {
                    atelier_core::verify::Discrepancy::Claimed { path, claimed } => json!({
                        "kind": "claimed",
                        "path": path,
                        "claimed": claimed.wire_label(),
                    }),
                    atelier_core::verify::Discrepancy::Unclaimed { path, observed } => json!({
                        "kind": "unclaimed",
                        "path": path,
                        "observed": observed.wire_label(),
                    }),
                    atelier_core::verify::Discrepancy::KindMismatch { path, claimed, observed } => json!({
                        "kind": "kind_mismatch",
                        "path": path,
                        "claimed": claimed.wire_label(),
                        "observed": observed.wire_label(),
                    }),
                    atelier_core::verify::Discrepancy::DuplicateClaim { path, count } => json!({
                        "kind": "duplicate_claim",
                        "path": path,
                        "count": count,
                    }),
                    // Phase B Track C2 — §7 Tier-1 LSP signal. Carries
                    // the LSP diagnostic location + the hallucinated
                    // symbol + the verbatim `lsp_message` so the red
                    // badge can quote the language server directly.
                    atelier_core::verify::Discrepancy::HallucinatedSymbol {
                        path, line, column, symbol, lsp_message,
                    } => json!({
                        "kind": "hallucinated_symbol",
                        "path": path,
                        "line": line,
                        "column": column,
                        "symbol": symbol,
                        "lsp_message": lsp_message,
                    }),
                })
                .collect::<Vec<_>>(),
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
        // §1 BYOM (v60.9) — context-window asymmetry resolution. The
        // `resolution` field is already a stable wire label
        // (`"compacted"` / `"rerouted"` / `"surfaced"`). The toast
        // surface in the Svelte reducer is a follow-on bundle; this
        // arm just plumbs the payload through so the webview's event
        // log shows the resolution alongside the existing
        // `CompactionExecuted` / `LedgerAppended` rows.
        SessionEvent::ContextOverflowResolved {
            resolution,
            freed_tokens,
            items_compacted,
        } => json!({
            "resolution": resolution,
            "freed_tokens": freed_tokens,
            "items_compacted": items_compacted,
        }),
        // v60.10 §1 BYOM — mid-session adapter swap. Pairs with the
        // immediately-following `ModelProfileLoaded` re-emission;
        // subscribers fold the swap into a toast / event-log entry and
        // refresh the model badge off the next `ModelProfileLoaded`.
        SessionEvent::AdapterSwapped {
            from_model_id,
            to_model_id,
            swapped_at,
        } => json!({
            "from_model_id": from_model_id,
            "to_model_id": to_model_id,
            "swapped_at": swapped_at,
        }),
        // v60.28 H2 — consent-modal lifecycle. `Pending` opens the modal
        // in the webview; the renderer echoes `swap_id` back through the
        // `respond_to_swap` Tauri command, which signals the swap_adapter
        // future and emits `AdapterSwapped` (accepted) or
        // `AdapterSwapRejected` (refused / timed out).
        SessionEvent::AdapterSwapPending {
            swap_id,
            to_model_id,
            base_url,
        } => json!({
            "swap_id": swap_id,
            "to_model_id": to_model_id,
            "base_url": base_url,
        }),
        SessionEvent::AdapterSwapRejected {
            swap_id,
            to_model_id,
            reason,
        } => json!({
            "swap_id": swap_id,
            "to_model_id": to_model_id,
            "reason": reason,
        }),
        // §2 — agent abandoned the turn-protocol contract (no tool
        // calls and no claimed_done). Runner has already transitioned
        // Streaming → AwaitingUser; the toast surface in the Svelte
        // reducer prompts the user to nudge / swap / abort.
        SessionEvent::AgentStalled { turn, reason } => json!({
            "turn": turn,
            "reason": reason,
        }),
        // Phase B Track C1 prep — §7 Tier-1 LSP first-use install prompt.
        // The webview renders a modal listing `candidate_packages`; the
        // approval round-trip lands in C1. Wire shape pinned here so the
        // four parallel bundles don't collide on this file (per **L-D-2**).
        SessionEvent::RequestLspInstall {
            language,
            candidate_packages,
        } => json!({
            "language": language,
            "candidate_packages": candidate_packages,
        }),
        SessionEvent::LspInstallResolved { language, outcome } => json!({
            "language": language,
            "outcome": outcome.wire_label(),
        }),
        // §10 sub-agent events — GUI card pane wired in WU-10
        SessionEvent::SubagentSpawned {
            id,
            parent_id,
            subagent_type,
            description,
            max_turns,
        } => json!({
            "id": id,
            "parent_id": parent_id,
            "subagent_type": subagent_type,
            "description": description,
            "max_turns": max_turns,
        }),
        SessionEvent::SubagentTurnAdvanced {
            id,
            turn,
            max_turns,
        } => json!({
            "id": id,
            "turn": turn,
            "max_turns": max_turns,
        }),
        SessionEvent::SubagentToolCall { id, tool } => json!({
            "id": id,
            "tool": tool,
        }),
        SessionEvent::SubagentCompleted {
            id,
            status,
            turns_used,
        } => json!({
            "id": id,
            "status": status.to_string(),
            "turns_used": turns_used,
        }),
        SessionEvent::SubagentCancelled { id, reason } => json!({
            "id": id,
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

    // ---------- v62: §7 verify-pass tier indicator ----------

    #[test]
    fn bridge_verification_passed_carries_tier_wire_label_and_counts() {
        // v62 — the Svelte reducer switches on the wire label directly
        // (`tier1_lsp` / `tier2_tree_sitter` / `tier3_textual` /
        // `not_run`), so the bridge must serialise the tier as that
        // exact label. Pin the canonical labels here so a future
        // variant rename forces a deliberate edit on the GUI side.
        use atelier_core::verify::VerificationTier;
        for (tier, label) in [
            (VerificationTier::Tier1Lsp, "tier1_lsp"),
            (VerificationTier::Tier2TreeSitter, "tier2_tree_sitter"),
            (VerificationTier::Tier3Textual, "tier3_textual"),
            (VerificationTier::NotRun, "not_run"),
        ] {
            let b = bridge_event(&SessionEvent::VerificationPassed {
                tier,
                file_count: 3,
                claim_count: 2,
            });
            assert_eq!(b.kind, "VerificationPassed");
            assert_eq!(b.payload["tier"], label);
            assert_eq!(b.payload["file_count"], 3);
            assert_eq!(b.payload["claim_count"], 2);
        }
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

    #[test]
    fn bridge_adapter_swapped_carries_model_id_pair() {
        // v60.10 §1 BYOM — `AdapterSwapped` ships the from/to model
        // id pair + timestamp. Pin the wire format so a future enum
        // rename can't silently drift what the webview consumes.
        let b = bridge_event(&SessionEvent::AdapterSwapped {
            from_model_id: "anthropic:claude-opus-4-7".into(),
            to_model_id: "local:qwen2.5-coder:7b".into(),
            swapped_at: "2026-05-18T12:00:00Z".into(),
        });
        assert_eq!(b.kind, "AdapterSwapped");
        assert_eq!(b.payload["from_model_id"], "anthropic:claude-opus-4-7");
        assert_eq!(b.payload["to_model_id"], "local:qwen2.5-coder:7b");
        assert_eq!(b.payload["swapped_at"], "2026-05-18T12:00:00Z");
    }

    // ---------- v60.28 H2 swap_adapter allowlist + consent ----------

    #[test]
    fn swap_allowlist_refuses_unknown_host() {
        assert!(!is_base_url_allowed(Some("https://evil.example/v1")));
        assert!(!is_base_url_allowed(Some("http://attacker.test/v1")));
    }

    #[test]
    fn swap_allowlist_accepts_known_hosts_and_loopback() {
        assert!(is_base_url_allowed(Some("https://api.anthropic.com/v1")));
        assert!(is_base_url_allowed(Some("https://api.openai.com/v1")));
        assert!(is_base_url_allowed(Some("http://localhost:11434/v1")));
        assert!(is_base_url_allowed(Some("http://127.0.0.1:8080/v1")));
        assert!(is_base_url_allowed(None));
    }

    // ---------- v60.37 B5 list_provider_profiles ----------

    #[test]
    fn list_provider_profiles_returns_defaults_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        // v60.55 — pin `home_override = None` so the test doesn't pick
        // up the developer's `~/.atelier/providers.toml`. Pre-v60.55
        // this assertion was latently broken whenever the developer
        // had a configured home-scope profile.
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        // Built-in fallback list is non-empty and starts with mock.
        assert!(!out.is_empty(), "expected built-in defaults");
        assert_eq!(out[0].kind, "mock");
        assert!(out.iter().any(|o| o.kind == "anthropic"));
        assert!(out.iter().any(|o| o.kind == "openai_compat"));
    }

    #[test]
    fn list_provider_profiles_hydrates_from_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_atelier = tmp.path().join(".atelier");
        std::fs::create_dir_all(&dot_atelier).unwrap();
        std::fs::write(
            dot_atelier.join("providers.toml"),
            r#"
[providers.local-codestral]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model = "local:codestral:22b"

[providers.cloud]
provider = "anthropic"
model = "anthropic:claude-opus-4-7"
"#,
        )
        .unwrap();
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        // Two named profiles → two rows, alphabetical by name (BTreeMap).
        assert_eq!(out.len(), 2, "got {out:?}");
        assert_eq!(out[0].kind, "anthropic");
        assert!(out[0].label.contains("cloud"));
        assert_eq!(out[0].base_url, None);
        assert_eq!(out[1].kind, "openai_compat");
        assert!(out[1].label.contains("local-codestral"));
        assert_eq!(
            out[1].base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn list_provider_profiles_skips_incomplete_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_atelier = tmp.path().join(".atelier");
        std::fs::create_dir_all(&dot_atelier).unwrap();
        std::fs::write(
            dot_atelier.join("providers.toml"),
            r#"
[providers.complete]
provider = "anthropic"
model = "anthropic:claude-opus-4-7"

[providers.no-model]
provider = "anthropic"

[providers.no-provider]
model = "anthropic:claude-opus-4-7"
"#,
        )
        .unwrap();
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        // Only the complete row survives the skip-incomplete filter.
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].label.contains("complete"));
    }

    #[test]
    fn list_provider_profiles_marks_default_from_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_atelier = tmp.path().join(".atelier");
        std::fs::create_dir_all(&dot_atelier).unwrap();
        std::fs::write(
            dot_atelier.join("providers.toml"),
            r#"
default = "cloud"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model = "local:qwen2.5-coder:7b"

[providers.cloud]
provider = "anthropic"
model = "anthropic:claude-opus-4-7"
"#,
        )
        .unwrap();
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        // Exactly one row must be flagged default; it must be the
        // one whose profile name matches `default = "cloud"`.
        let defaults: Vec<_> = out.iter().filter(|o| o.is_default).collect();
        assert_eq!(defaults.len(), 1, "got {out:?}");
        assert!(defaults[0].label.contains("cloud"), "got {defaults:?}");
    }

    #[test]
    fn list_provider_profiles_no_default_when_field_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_atelier = tmp.path().join(".atelier");
        std::fs::create_dir_all(&dot_atelier).unwrap();
        std::fs::write(
            dot_atelier.join("providers.toml"),
            r#"
[providers.cloud]
provider = "anthropic"
model = "anthropic:claude-opus-4-7"
"#,
        )
        .unwrap();
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        // No `default = ...` field → no row is starred.
        assert!(out.iter().all(|o| !o.is_default), "got {out:?}");
    }

    #[test]
    fn list_provider_profiles_marks_mock_default_in_fallback() {
        // No `providers.toml` → built-in fallback. `Runner::new` would
        // construct a Mock adapter; the dropdown's default marker must
        // match so the user knows where chat would land.
        let tmp = tempfile::tempdir().unwrap();
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        let defaults: Vec<_> = out.iter().filter(|o| o.is_default).collect();
        assert_eq!(defaults.len(), 1, "got {out:?}");
        assert_eq!(defaults[0].kind, "mock");
    }

    #[test]
    fn list_provider_profiles_falls_back_on_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_atelier = tmp.path().join(".atelier");
        std::fs::create_dir_all(&dot_atelier).unwrap();
        std::fs::write(
            dot_atelier.join("providers.toml"),
            "this = is = not = valid = toml\n",
        )
        .unwrap();
        let out = super::list_provider_profiles_with_home(tmp.path(), None);
        // Malformed file logs a warn but doesn't kill the dropdown —
        // user gets the built-in fallback.
        assert!(
            !out.is_empty(),
            "malformed providers.toml must still surface the default list"
        );
        assert_eq!(out[0].kind, "mock");
    }

    #[test]
    fn swap_allowlist_refuses_non_http_scheme() {
        // v60.37 B1 — `host_of_url` now requires an explicit `http://`
        // or `https://` prefix. Without this, `localhost` (no scheme)
        // was accepted, and `gopher://api.anthropic.com/x` was accepted
        // because the parsed host matched the allowlist. Both are
        // defence-in-depth thinness — reqwest fails on non-http schemes
        // today, but a copy-paste of this helper would inherit the bug.
        assert!(!is_base_url_allowed(Some("localhost")));
        assert!(!is_base_url_allowed(Some("127.0.0.1")));
        assert!(!is_base_url_allowed(Some("gopher://api.anthropic.com/x")));
        assert!(!is_base_url_allowed(Some("file:///etc/passwd")));
        assert!(!is_base_url_allowed(Some("ftp://api.openai.com/v1")));
        // Mixed-case schemes still validated (lowered before compare).
        assert!(is_base_url_allowed(Some("HTTPS://api.anthropic.com/v1")));
    }

    #[test]
    fn bridge_adapter_swap_pending_carries_swap_id_to_id_and_base_url() {
        let b = bridge_event(&SessionEvent::AdapterSwapPending {
            swap_id: "9b3c8d52-8c9b-4e8e-a3b6-2c8e1c2a8f55".into(),
            to_model_id: "openai-compat:gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
        });
        assert_eq!(b.kind, "AdapterSwapPending");
        assert_eq!(b.payload["swap_id"], "9b3c8d52-8c9b-4e8e-a3b6-2c8e1c2a8f55");
        assert_eq!(b.payload["to_model_id"], "openai-compat:gpt-4o");
        assert_eq!(b.payload["base_url"], "https://api.openai.com/v1");
    }

    #[test]
    fn bridge_adapter_swap_rejected_carries_reason_and_swap_id_when_known() {
        let b = bridge_event(&SessionEvent::AdapterSwapRejected {
            swap_id: Some("9b3c8d52-8c9b-4e8e-a3b6-2c8e1c2a8f55".into()),
            to_model_id: "openai-compat:gpt-4o".into(),
            reason: "user rejected the swap".into(),
        });
        assert_eq!(b.kind, "AdapterSwapRejected");
        assert_eq!(b.payload["swap_id"], "9b3c8d52-8c9b-4e8e-a3b6-2c8e1c2a8f55");
        assert!(b.payload["reason"].as_str().unwrap().contains("rejected"));

        // Allowlist refusal happens before the modal opens; swap_id is
        // null on the wire in that case so the renderer can tell apart
        // "modal refusal" from "we never opened a modal".
        let b = bridge_event(&SessionEvent::AdapterSwapRejected {
            swap_id: None,
            to_model_id: "openai-compat:gpt-4o".into(),
            reason: "base_url \"https://evil.example/v1\" not in swap_adapter allowlist".into(),
        });
        assert!(b.payload["swap_id"].is_null());
        assert!(b.payload["reason"]
            .as_str()
            .unwrap()
            .contains("evil.example"));
    }

    // ---------- v60.28 H2 follow-on: consent gate semantics ----------

    #[test]
    fn swap_decision_deserialises_from_lowercase_wire() {
        // The Svelte modal calls `invoke('respond_to_swap', { swap_id,
        // decision: 'accepted' | 'rejected' })`. Pin the wire labels so
        // a future enum rename can't silently break the renderer reply.
        let accepted: SwapDecision = serde_json::from_str("\"accepted\"").unwrap();
        let rejected: SwapDecision = serde_json::from_str("\"rejected\"").unwrap();
        assert_eq!(accepted, SwapDecision::Accepted);
        assert_eq!(rejected, SwapDecision::Rejected);
        // Anything else is a renderer bug; defensively reject it at the
        // boundary so a typo doesn't get silently coerced.
        assert!(serde_json::from_str::<SwapDecision>("\"yes\"").is_err());
        assert!(serde_json::from_str::<SwapDecision>("\"ACCEPTED\"").is_err());
    }

    /// The pending-swaps registry the consent gate uses internally.
    /// We exercise it through the same `tokio::sync::oneshot` shape
    /// `swap_adapter` builds so the test asserts the actual signalling
    /// path, not a stub.
    fn empty_pending_swaps() -> tokio::sync::Mutex<
        std::collections::HashMap<uuid::Uuid, tokio::sync::oneshot::Sender<SwapDecision>>,
    > {
        tokio::sync::Mutex::new(std::collections::HashMap::new())
    }

    #[tokio::test]
    async fn consent_accepted_reply_signals_swap_adapter() {
        // `swap_adapter` mints a swap_id, registers a oneshot sender,
        // emits `AdapterSwapPending`, awaits the receiver. The renderer
        // calls `respond_to_swap` which pops the sender and signals
        // `Accepted`. Pin: the receiver sees the decision.
        let registry = empty_pending_swaps();
        let swap_id = uuid::Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel::<SwapDecision>();
        registry.lock().await.insert(swap_id, tx);

        // `respond_to_swap`'s body, inline.
        let sender = registry.lock().await.remove(&swap_id).unwrap();
        sender.send(SwapDecision::Accepted).unwrap();

        let decision = tokio::time::timeout(std::time::Duration::from_millis(50), rx)
            .await
            .expect("receiver should resolve within 50ms")
            .expect("sender should not have been dropped");
        assert_eq!(decision, SwapDecision::Accepted);
    }

    #[tokio::test]
    async fn consent_rejected_reply_routes_through_oneshot() {
        let registry = empty_pending_swaps();
        let swap_id = uuid::Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel::<SwapDecision>();
        registry.lock().await.insert(swap_id, tx);

        let sender = registry.lock().await.remove(&swap_id).unwrap();
        sender.send(SwapDecision::Rejected).unwrap();

        let decision = tokio::time::timeout(std::time::Duration::from_millis(50), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decision, SwapDecision::Rejected);
    }

    #[tokio::test]
    async fn consent_unknown_swap_id_returns_none() {
        // `respond_to_swap` with a stale or invented swap_id pops `None`
        // from the registry and returns Err to the renderer. Pin: a
        // stale reply doesn't accidentally signal some other in-flight
        // swap.
        let registry = empty_pending_swaps();
        let real_id = uuid::Uuid::new_v4();
        let bogus_id = uuid::Uuid::new_v4();
        let (tx, _rx) = tokio::sync::oneshot::channel::<SwapDecision>();
        registry.lock().await.insert(real_id, tx);

        let bogus = registry.lock().await.remove(&bogus_id);
        assert!(bogus.is_none(), "stale swap_id must not pop any sender");
        // The real entry is still there waiting for a reply.
        assert!(registry.lock().await.contains_key(&real_id));
    }

    // ---------- v60.52 §15 Skills ----------

    #[test]
    fn list_skills_returns_bundled_set_in_a_clean_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let skills = super::list_skills_in(dir.path());
        // 19 bundled skills land via SkillRegistry::load (3 original +
        // 11 from v60.50.5 + 5 from v60.55).
        assert!(
            skills.len() >= 19,
            "expected ≥19 skills, got {}",
            skills.len()
        );
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        for required in ["review", "security-review", "test"] {
            assert!(
                names.contains(&required),
                "bundled skill `{required}` missing from list_skills_in"
            );
        }
        // The proactive flag is honoured on `security-review`.
        let sr = skills.iter().find(|s| s.name == "security-review").unwrap();
        assert!(sr.proactive);
        // Source is the kebab-case wire label.
        assert!(skills.iter().all(|s| s.source == "bundled"));
    }

    #[tokio::test]
    async fn consent_timeout_yields_rejected_via_dropped_sender() {
        // If `swap_adapter`'s wait times out, the registry slot is
        // removed so a late `respond_to_swap` is a no-op. The dropped
        // sender on the still-living receiver path resolves to an
        // `Err(RecvError)` which `swap_adapter` already treats as a
        // refusal. Pin: dropping the sender wakes the receiver.
        let registry = empty_pending_swaps();
        let swap_id = uuid::Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel::<SwapDecision>();
        registry.lock().await.insert(swap_id, tx);

        // Simulate `swap_adapter`'s timeout-path cleanup: pop and drop.
        let sender = registry.lock().await.remove(&swap_id).unwrap();
        drop(sender);

        let recv = tokio::time::timeout(std::time::Duration::from_millis(50), rx)
            .await
            .expect("receiver should resolve within 50ms once sender is dropped");
        assert!(recv.is_err(), "dropped sender must surface as RecvError");
    }
}

#[cfg(test)]
mod adapter_swap_tests {
    use super::check_model_drift;

    // v60.34 (M25) — when the renderer stamps the expected model id and
    // it matches the live adapter, the compaction proceeds. When the
    // live adapter has drifted (a swap raced ahead of the renderer),
    // the call is rejected with a typed ModelDrift signal instead of
    // silently invoking the wrong adapter.
    #[test]
    fn no_expected_id_is_always_ok() {
        check_model_drift(None, "anthropic:claude-opus-4-7").expect("no expectation, no drift");
    }

    #[test]
    fn matching_expected_id_is_ok() {
        check_model_drift(
            Some("anthropic:claude-opus-4-7"),
            "anthropic:claude-opus-4-7",
        )
        .expect("match → ok");
    }

    #[test]
    fn mismatched_expected_id_surfaces_model_drift_error() {
        let err = check_model_drift(Some("anthropic:claude-opus-4-7"), "local:qwen2.5-coder:7b")
            .expect_err("mismatch → ModelDrift");
        assert!(err.starts_with("ModelDrift:"), "wrong error shape: {err}");
        assert!(err.contains("anthropic:claude-opus-4-7"));
        assert!(err.contains("local:qwen2.5-coder:7b"));
    }
}
