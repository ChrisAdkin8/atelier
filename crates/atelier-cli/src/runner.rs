//! Phase C unblock (1) — `atelier run` runtime.
//!
//! Wires the §2.5 actor + §15 dispatcher (with all 7 built-in tools) + §15
//! hook loader + §7 DoD config + §11 sandbox + §1 typed cost ledger into a
//! runnable agent loop. Reads a prompt, loops turns against an adapter
//! until `claimed_done: true`, transitions to `Verifying` for DoD checks,
//! and persists the session to `<repo>/.atelier/sessions/<uuid>/`.
//!
//! Pure-Rust API (`Runner::run`) so integration tests can drive it without
//! going through the binary. The `main.rs` `run` subcommand is a thin
//! wrapper that parses argv and prints events to stdout.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atelier_core::{
    adapter::{
        anthropic::AnthropicAdapter, Adapter, AdapterError, Message, MockAdapter, Role,
        ToolCallRequest,
    },
    context::{
        ContextItem, ContextItemId, ContextManager, Payload, Provenance, TokenCount, TokenSource,
    },
    dispatcher::{
        ConcurrentEditPolicy, Dispatcher, SessionDispatcher, ShellHookExecutor, ToolRegistry,
    },
    dod::DodConfig,
    file_watcher,
    hooks::HookSet,
    ledger::Ledger,
    memory::MemoryStore,
    persistence::{OnDiskSession, PersistedSubagent, PersistedSubagentCost},
    plan::PlanCanvas,
    protocol::Envelope,
    protocol_strategy::{parse_json_sentinel, parse_native_tool, NativeToolCall, Strategy},
    sandbox::SandboxPolicy,
    session::{
        self, try_emit, Command as SessionCommand, ConcurrentEditOutcome, Event, MessageRole,
    },
    state::NoopHook,
    subagents::{SubagentSpawner, SubagentStatus, SubagentTypeRegistry},
    tools::{register_builtins, BuiltinDeps},
    SessionHandle, SessionId, State, Transition,
};

use crate::subagent_spawner::RunnerSpawner;

/// How `atelier run` reports events. The binary uses `Stdout` (one line per
/// event); tests use `Capture` to assert on the recorded sequence without
/// stdout interception. `Null` is for tests that don't care about events.
///
/// The `#[allow(dead_code)]` is intentional: this module is consumed by
/// two different build targets (the `atelier` binary and the
/// `run_integration` integration tests), and each only uses a subset of
/// the variants. Without the allow, clippy in one target flags variants
/// the other target needs as dead.
#[allow(dead_code)]
pub enum EventSink {
    /// Print each `Event` to stdout as one JSON-ish line.
    Stdout,
    /// Collect events into a shared `Vec` for test assertions.
    Capture(Arc<parking_lot::Mutex<Vec<Event>>>),
    /// Discard.
    Null,
    /// Invoke a caller-supplied callback for each event. The GUI uses
    /// this to forward bus events into the Tauri webview as
    /// `atelier://event`. The callback must be `Send + Sync` and
    /// non-blocking; the drain task awaits broadcast::recv() between
    /// invocations, not the callback itself.
    Callback(Arc<dyn Fn(&Event) + Send + Sync + 'static>),
}

/// Provider selector — what `--provider` flips between. `Mock` is for tests
/// and dev-loop walkthroughs; `Anthropic` is the first real provider, gated
/// on `ANTHROPIC_API_KEY` being present in the environment.
///
/// `#[allow(dead_code)]`: same rationale as [`EventSink`] — the `Anthropic`
/// variant is only constructed by the `atelier` binary (live API path);
/// the integration tests stay on `Mock`. Without the allow, clippy in the
/// test target flags `Anthropic` as dead.
#[allow(dead_code)]
pub enum ProviderChoice {
    /// In-tree `MockAdapter`. Seeded chunk streams come from `mock_responses`.
    Mock { responses: Vec<MockResponse> },
    /// Anthropic Messages API. `model_id` is the `<provider>:<model>` form
    /// the cost ledger stores, e.g. `anthropic:claude-opus-4-7`. API key is
    /// read from `ANTHROPIC_API_KEY` at construction time.
    Anthropic { model_id: String },
    /// v50: OpenAI-compatible endpoint — works with LM Studio,
    /// llama.cpp server, vLLM, sglang, Ollama (compat layer), and
    /// OpenAI itself. `base_url` is the full URL ending in `/v1`
    /// (e.g. `http://localhost:11434/v1`); `None` uses the adapter's
    /// default (OpenAI). `api_key` is read from `OPENAI_API_KEY`
    /// (empty allowed for local servers that don't require auth).
    /// `model_id` is `<provider>:<model>` — typically `local:<tag>`
    /// for self-hosted, `openai:<model>` for OpenAI.
    OpenAiCompat {
        model_id: String,
        base_url: Option<String>,
        /// Enable llama.cpp / mlx-lm KV-cache prefix reuse on every
        /// request. Maps to `"cache_prompt": true` on the wire. See
        /// `OpenAiCompatAdapter::with_cache_prompt`.
        cache_prompt: bool,
    },
}

/// One pre-baked response the `MockAdapter` should return on the Nth
/// `chat()` call. Lets tests script a multi-turn flow without writing the
/// JSON-mode sentinel by hand.
///
/// The envelope rides in `tool_calls` via the `harness_meta` tool — tests
/// construct it with the `mock_envelope_tool_call` helper. The fields
/// below are exactly what the mock returns over the wire; the runner's
/// `extract_native_envelope` recovers the envelope from `tool_calls`.
#[derive(Default)]
pub struct MockResponse {
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCallRequest>,
    /// §1 BYOM context-overflow test seam. When `Some`, the mock
    /// emits a `StreamChunk::Error { error: ContextOverflow { … } }`
    /// instead of the usual `Complete` chunk — letting the
    /// integration test drive the overflow recovery path through the
    /// regular `ProviderChoice::Mock { responses }` channel. The
    /// `assistant_text` / `tool_calls` fields are ignored when this
    /// is set. Production callers always leave it `None`. Existing
    /// callers that build the struct via the `MockResponse { … }`
    /// literal can continue to do so by appending `..Default::default()`
    /// — but every pre-existing site already populates the other
    /// fields explicitly, so they only need to append `overflow: None`.
    #[doc(hidden)]
    pub overflow: Option<(u32, u32)>,
}

impl MockResponse {
    /// Convenience constructor for the happy-path used by every
    /// existing test. Keeps the call sites short and ensures
    /// `overflow` stays `None` unless a test explicitly asks for it.
    #[allow(dead_code)]
    pub fn new(assistant_text: impl Into<String>, tool_calls: Vec<ToolCallRequest>) -> Self {
        Self {
            assistant_text: assistant_text.into(),
            tool_calls,
            overflow: None,
        }
    }

    /// §1 BYOM test helper — drive the `ContextOverflow` arm.
    /// `needed` / `limit` populate the `AdapterError::ContextOverflow`
    /// fields the runner's auto-selector reads.
    #[allow(dead_code)]
    pub fn context_overflow(needed: u32, limit: u32) -> Self {
        Self {
            assistant_text: String::new(),
            tool_calls: Vec::new(),
            overflow: Some((needed, limit)),
        }
    }
}

/// What `Runner::run` returns. Caller (binary or test) decides whether a
/// non-success exit code is warranted.
pub struct RunReport {
    pub session_id: SessionId,
    pub turns: usize,
    pub final_state: State,
    /// `true` iff the DoD config (if present) reported all checks green.
    /// `None` when no DoD config was found — the harness doesn't fail
    /// closed in that case; it's a soft "no DoD configured" state.
    pub dod_passed: Option<bool>,
    /// Phase B Track A — final snapshot of the per-session envelope-parse
    /// conformance ring buffer. Lets the Phase B nightly gate test
    /// (`phase_b_live_anthropic_conformance`) aggregate per-strategy
    /// success rates without re-driving the loop. Empty when the run
    /// produced zero envelope-parse attempts (a degenerate failure mode;
    /// the runner emits at least one attempt per turn under normal
    /// operation).
    pub envelope_conformance: atelier_core::protocol_conformance::ConformanceSnapshot,
    /// §10 — final assistant message text. `None` if the run produced no
    /// assistant turns (degenerate; shouldn't happen under normal operation).
    /// Used by sub-agent callers to extract the result to return to the
    /// parent's tool-result slot.
    pub final_assistant_text: Option<String>,
    /// §10 — a copy of the ledger entries accumulated during this run.
    /// Used by sub-agent callers to roll cost up into the parent's ledger.
    /// Empty in the root-runner case where the caller doesn't need them.
    pub ledger_entries: Vec<atelier_core::ledger::LedgerEntry>,
    /// §10 — number of turns actually consumed (alias for `turns` for
    /// clarity in sub-agent callers).
    pub turns_used: usize,
}

/// Shared slot a caller registers via [`Runner::with_dispatcher_handle`]
/// to receive the `SessionDispatcher` Arc as soon as the runner builds
/// it. The GUI uses this to wire its `submit_approval` Tauri command
/// to the live dispatcher — spec §3 hunk accept/reject is otherwise
/// only reachable through direct Rust calls.
///
/// Typical setup:
///
/// ```ignore
/// let slot = DispatcherHandle::new();
/// let runner = Runner::new(...)?
///     .with_approval_policy(ApprovalPolicy::AwaitApproval)
///     .with_dispatcher_handle(slot.clone());
/// tokio::spawn(async move { runner.run(prompt).await });
/// // Elsewhere (e.g. a Tauri command handler):
/// if let Some(sd) = slot.get() {
///     sd.submit_approval(commit_id, accepted);
/// }
/// ```
#[derive(Clone, Default)]
pub struct DispatcherHandle {
    inner: Arc<parking_lot::Mutex<Option<Arc<SessionDispatcher>>>>,
}

impl DispatcherHandle {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self) -> Option<Arc<SessionDispatcher>> {
        self.inner.lock().clone()
    }
    fn set(&self, sd: Arc<SessionDispatcher>) {
        *self.inner.lock() = Some(sd);
    }
    fn clear(&self) {
        *self.inner.lock() = None;
    }
}

/// v60.5 — shared slot the runner publishes its active adapter into so
/// external callers (the GUI's `compact_context_items` Tauri command,
/// the TUI's `Mutation::Compact` arm) can issue the §5 compaction
/// summary call without re-implementing the per-provider construction
/// logic. Same pattern as [`DispatcherHandle`]: `get()` is cheap and
/// returns an `Arc` clone; the runner clears the slot via
/// `AdapterHandleGuard` so a torn-down run can't leak an
/// `Arc<dyn Adapter>` to a future caller.
#[derive(Clone, Default)]
pub struct AdapterHandle {
    inner: Arc<parking_lot::Mutex<Option<Arc<dyn Adapter>>>>,
}

impl AdapterHandle {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self) -> Option<Arc<dyn Adapter>> {
        self.inner.lock().clone()
    }
    fn set(&self, a: Arc<dyn Adapter>) {
        *self.inner.lock() = Some(a);
    }
    fn clear(&self) {
        *self.inner.lock() = None;
    }

    /// v60.10 §1 BYOM — external swap: replace the slot's adapter from
    /// outside the running `Runner`. Used by the GUI's `swap_adapter`
    /// Tauri command so a mid-session provider swap atomically updates
    /// the slot that `compact_context_items` reads from — the slot's
    /// Arc doesn't keep the pre-swap adapter alive past this call.
    ///
    /// `None` if the slot was empty (no active run). The caller decides
    /// whether to treat that as an error.
    pub fn swap(&self, new_adapter: Arc<dyn Adapter>) -> Option<Arc<dyn Adapter>> {
        let mut guard = self.inner.lock();
        let previous = guard.take();
        *guard = Some(new_adapter);
        previous
    }
}

/// Drop-guard that clears the caller's `DispatcherHandle` slot when
/// `Runner::run` exits via any path: success, `?`-propagated error,
/// or panic. Without this, an early-return between `handle.set(...)`
/// and the success-path `handle.clear()` would leave a stale
/// `Arc<SessionDispatcher>` reachable via `DispatcherHandle::get` —
/// `submit_approval` would route to a dispatcher that's mid-teardown.
struct DispatcherHandleGuard<'a> {
    handle: Option<&'a DispatcherHandle>,
}

impl Drop for DispatcherHandleGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle {
            handle.clear();
        }
    }
}

/// v60.5 — companion to [`DispatcherHandleGuard`] for the new
/// [`AdapterHandle`]. Same lifecycle: clear on every exit path so a
/// crashing run can't strand an `Arc<dyn Adapter>` in the slot.
struct AdapterHandleGuard<'a> {
    handle: Option<&'a AdapterHandle>,
}

impl Drop for AdapterHandleGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle {
            handle.clear();
        }
    }
}

/// v51 — Probe-on-first-use policy. The Runner uses this to decide
/// whether to call [`atelier_core::adapter::model_profile::ProfileStore::load_or_probe`]
/// before the first turn, or to short-circuit with a stub profile
/// (Mock and Anthropic are well-characterised; probing them would be
/// wasted round-trips).
///
/// `#[allow(dead_code)]`: same rationale as [`ProviderChoice`] —
/// `Force` is only constructed by the `atelier` binary's
/// `--force-probe` path; the integration tests stay on `Auto`/`Skip`
/// implicit-via-`ProviderChoice::Mock` defaults.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbePolicy {
    /// Cache-first: reuse a cached profile when present, probe on
    /// miss. Default for `openai-compat`.
    Auto,
    /// Never probe; build a stub profile from the adapter's
    /// [`atelier_core::adapter::Capabilities`]. Default for `mock` and
    /// `anthropic`; also the result of `--no-probe`.
    Skip,
    /// Always probe, even when cached. CLI `--force-probe`.
    Force,
}

pub struct Runner {
    workspace: PathBuf,
    adapter: Arc<dyn Adapter>,
    /// Optional fast/cheap adapter for tool-result turns (§1 per-task
    /// routing). When `Some`, turns whose last message has `Role::Tool`
    /// are sent to this adapter instead of `self.adapter`. `None` keeps
    /// the existing single-adapter behaviour.
    executor_adapter: Option<Arc<dyn Adapter>>,
    sink: EventSink,
    /// Max turns before bailing — defends against a model that never
    /// emits `claimed_done`. 32 matches a generous canonical-workload
    /// median (PROVISIONAL).
    max_turns: usize,
    /// Spec §3 hunk-accept-reject policy. Defaults to
    /// `AutoApproveAll` so every existing caller keeps its v45
    /// behaviour.
    approval_policy: atelier_core::dispatcher::ApprovalPolicy,
    /// Optional caller-supplied slot the runner writes into once the
    /// SessionDispatcher is constructed. Lets the GUI thread a
    /// `submit_approval` Tauri command to the live dispatcher.
    dispatcher_handle: Option<DispatcherHandle>,
    /// v60.5 — companion slot for the adapter. Populated as soon as
    /// the run starts; cleared via [`AdapterHandleGuard`] on every exit
    /// path. Both drivers (GUI + TUI) use this to surface
    /// `compact_context_items` without re-constructing the adapter.
    adapter_handle: Option<AdapterHandle>,
    /// v51 — probe policy chosen at construction time. The CLI
    /// flips this for `--no-probe` / `--force-probe`.
    probe_policy: ProbePolicy,
    /// v51 — base URL used as part of the probe cache key. Empty
    /// for adapters that don't speak HTTP (Mock, Anthropic).
    probe_base_url: String,
    /// Phase C close — pane-visibility instrumentation. `None`
    /// means the driver didn't supply one; the runner skips writing
    /// `pane_visibility.json` in that case so the file's presence
    /// is a positive signal ("this run was instrumented") rather
    /// than a default-noise artefact. The `driver` field on the
    /// record names the surface (GUI / TUI / headless).
    pane_visibility: Option<(crate::instrumentation::PaneVisibility, String)>,
    /// v60.7 §1 BYOM ledger discipline — how to attribute
    /// `cost_usd` to each `ModelCall` ledger entry. Local providers
    /// (Mock, OpenAI-compat against a self-hosted server) use the
    /// latency-weighted §1 default rate; cloud providers (Anthropic,
    /// OpenAI's hosted API) leave `cost_usd = None` until per-provider
    /// pricing tables land in a later bundle. Surfaces in the §3
    /// cost meter as the "+ N unknown" suffix.
    cost_policy: ModelCostPolicy,
    /// v61 — §14 concurrent-edit policy. `Modal` (the default) surfaces
    /// `Event::FilesChanged` and waits for a user decision; `AutoReload`
    /// auto-resolves to Reload for `--non-interactive` mode.
    concurrent_edit_policy: ConcurrentEditPolicy,
    /// v61 — §14 resume. When `Some(uuid)`, the runner loads the
    /// on-disk session and replays its conversation prefix instead of
    /// starting from the supplied prompt. The prompt is appended after
    /// the prefix as a fresh user turn so the model picks up where the
    /// crashed run left off.
    resume_from: Option<uuid::Uuid>,
    /// v60.20 — §5 mental-model panel. When `Some((text, enabled))` and
    /// `enabled == true && !text.trim().is_empty()`, the runner seeds the
    /// `SessionDispatcher`'s mental-model state at construction time and
    /// injects the text as a second System message on every per-turn
    /// `adapter.chat` call. `None` keeps the dispatcher's default (off,
    /// empty); the GUI/TUI can still flip it via the existing
    /// `set_mental_model` round-trip.
    initial_mental_model: Option<(String, bool)>,
    /// v61 — `--non-interactive`. Disables modals (approvals + concurrent
    /// edits) and never prompts the user. Drives the auto-resolve
    /// answers that headless runs need.
    ///
    /// `#[allow(dead_code)]` because integration tests pull this file
    /// in via `#[path]` and don't reference the field — but it's wired
    /// through `Runner::with_non_interactive` from `main.rs`. Removing
    /// the allow under a different cfg-feature topology is fine when
    /// the `Runner`'s field set is consumed via a method (today: only
    /// the builder sets it; the run loop reads `concurrent_edit_policy`
    /// / `approval_policy` which `with_non_interactive` mutates).
    #[allow(dead_code)]
    non_interactive: bool,
    /// §1 BYOM — rolling window size for conformance-driven strategy
    /// degradation. Defaults to
    /// [`atelier_core::protocol_conformance::DEFAULT_DEGRADATION_WINDOW`]
    /// (PROVISIONAL 20); tests dial it down via
    /// [`Self::with_degradation_window`].
    degradation_window: usize,
    /// §1 BYOM — failure threshold inside the rolling window. Defaults
    /// to [`atelier_core::protocol_conformance::DEFAULT_DEGRADATION_THRESHOLD`]
    /// (PROVISIONAL 3); tests dial it down via
    /// [`Self::with_degradation_threshold`].
    degradation_threshold: u32,
    /// §1 BYOM — context-window asymmetry policy. Defaults to
    /// [`ContextOverflowPolicy::Compact`]; the binary and the GUI/TUI
    /// drivers will surface a flag/setting to flip it in a follow-on
    /// bundle. See [`ContextOverflowPolicy`] for the three arms.
    overflow_policy: ContextOverflowPolicy,
    /// v60.9 §2 — per-session cache of the adapter's few-shot override
    /// (resolved against the starting `active_strategy`). Populated on
    /// first access in [`Self::run`]; `Some(empty)` means "the adapter
    /// returned `None` — fall back to the shared baseline (currently
    /// empty)". The cache is per-`Runner`, not per-process, so two
    /// sequential runs with the same adapter each pay one
    /// `few_shot_override` call.
    few_shot_cache: parking_lot::Mutex<Option<Vec<Message>>>,
    /// v60.10 §1 BYOM — pending mid-session adapter-swap announcement.
    /// Populated by [`Self::swap_adapter`]; consumed at the start of
    /// the next [`Self::run`] which emits an `Event::AdapterSwapped`
    /// (carrying `from_model_id` → `to_model_id` + the swap timestamp)
    /// alongside the regular `ModelProfileLoaded` for the new
    /// adapter. Cleared after the next run reads it so a third run
    /// without a fresh swap doesn't re-announce.
    pending_swap: parking_lot::Mutex<Option<PendingAdapterSwap>>,
    /// Phase B Track D — test-only override for the starting §2 strategy.
    /// When set, pins `active_strategy` regardless of the profile's
    /// recommendation. Lets the Phase B mechanical gate exercise
    /// `JsonSentinel` and `RegexProse` parse arms end-to-end against the
    /// `MockAdapter` (whose `Capabilities` always resolve to
    /// `NativeTool`). Production callers leave this `None`.
    starting_strategy_override: Option<Strategy>,
    /// Phase B Track C3 — test-only seam for the §7 Tier-1 LSP
    /// hallucinated-symbol gate. When set, the runner's verify-pass
    /// call site uses `dispatcher.verify_pass_with_tier1` (instead of
    /// the bare Tier-3 `verify_pass`) with these pre-mapped
    /// discrepancies. Lets the hallucinating-agent fixture exercise
    /// the merged-tier verify path before the live LSP receiver
    /// (`async-lsp` + `typescript-language-server`) is wired — once the
    /// spike at `experiments/lsp_spike/` resolves GO, the runner
    /// produces these from `lsp_types::Diagnostic` instead and this
    /// override stays unused.
    tier1_diagnostics_for_test: Vec<atelier_core::verify::Discrepancy>,
    /// v60.29 H10 — external cancellation token. The CLI `main`
    /// supplies one from its SIGINT/SIGTERM handler and trips it on
    /// signal; the session actor + the dispatcher's `tokio::select!`
    /// both observe it and unwind. `None` means the run is on its own
    /// (test and GUI/TUI driver entry points today).
    external_cancel: Option<tokio_util::sync::CancellationToken>,
    /// v60.51 §15 — skill registry consulted before the first user
    /// turn fires. When `None` the runner does no slash expansion; a
    /// `/foo` prompt reaches the model verbatim (matches pre-v60.51
    /// behaviour). The CLI wires this from
    /// `atelier_core::SkillRegistry::load(workspace, home)` so a fresh
    /// repo with no `.atelier/skills/` directory still gets the
    /// bundled set.
    skill_registry: Option<Arc<atelier_core::skills::SkillRegistry>>,
    /// §10 — recursion depth of this runner (0 = root; +1 per sub-agent
    /// level). Threaded into `ToolContext::subagent_depth` so that
    /// nested `spawn_subagent` calls can enforce the depth cap.
    subagent_depth: u8,
    /// §10 — pre-resolved `ModelProfile` injected by `RunnerSpawner`
    /// into child runners. When `Some`, the probe block is bypassed and
    /// this profile is used directly so children inherit the parent's
    /// observed emission strategy without an extra model round-trip.
    pinned_profile: Option<atelier_core::adapter::model_profile::ModelProfile>,
}

/// v60.10 §1 BYOM — record of a pending mid-session adapter swap that
/// the next [`Runner::run`] should announce on the bus. Kept on
/// [`Runner`] (not the bus directly) because `swap_adapter` may run
/// between two `run()` invocations — at that point no bus exists. The
/// next run's startup consults this field and emits the announcement
/// pair (`AdapterSwapped` + a fresh `ModelProfileLoaded`) before the
/// first turn so a UI subscribed via `EventSink::Capture` /
/// `EventSink::Callback` always sees both events.
#[derive(Debug, Clone)]
struct PendingAdapterSwap {
    from_model_id: String,
    to_model_id: String,
    swapped_at: String,
}

/// §1 BYOM — context-window asymmetry policy. Chosen at
/// [`Runner::new`] time (default = [`Self::Compact`]) and consulted
/// whenever `adapter.chat()` returns
/// [`AdapterError::ContextOverflow`].
///
/// Three arms per spec §1 ("BYOM context-window asymmetry: Compact /
/// Reroute / `ContextOverflowError`"):
///
///   * [`Self::Compact`] — auto-trigger a v60.5 non-destructive
///     compaction on the largest unpinned context items, then retry
///     the turn. Bounded by [`MAX_OVERFLOW_RETRIES`] consecutive
///     attempts; after the cap is hit the runner falls back to the
///     `Surface` behaviour so a wedged model can't drive a runaway
///     compaction loop.
///   * [`Self::Reroute`] — placeholder for the future
///     routing-dispatcher work. v60.9 has no router yet, so this arm
///     emits `RunError::Config("reroute not yet implemented")` and
///     a `ContextOverflowResolved { resolution: "rerouted" }` event
///     so subscribers can render "reroute requested but unconfigured"
///     for the time being.
///   * [`Self::Surface`] — propagate the overflow as a typed
///     `RunError::ContextOverflow` so the caller (binary, GUI, TUI)
///     decides what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextOverflowPolicy {
    /// Auto-compact the largest-token unpinned items then retry the
    /// turn. Default.
    Compact,
    /// Future routing-dispatcher hook. Returns
    /// `RunError::Config("reroute not yet implemented")` for now.
    Reroute,
    /// Propagate `AdapterError::ContextOverflow` as
    /// [`RunError::ContextOverflow`] so the caller decides.
    Surface,
}

/// Defence-in-depth cap on consecutive auto-compact retries within a
/// single turn. If a second `adapter.chat()` after a successful
/// compaction still overflows, the runner drops to
/// [`ContextOverflowPolicy::Surface`] behaviour rather than spinning
/// forever — a runaway compaction loop is worse than a clean error.
///
/// PROVISIONAL — `2` (one retry after the initial overflow) matches
/// the spec §1 "single recovery attempt" intent; raise only after a
/// calibration run shows real models routinely need more.
pub const MAX_OVERFLOW_RETRIES: usize = 2;

/// §1 BYOM — fraction of the freed-token target the auto-selector
/// padds with so a near-miss heuristic doesn't immediately re-overflow
/// after compaction. 25% over the strict `needed - (limit - current)`
/// gap. PROVISIONAL — set by inspection of the v60.5 compaction
/// summary-card token budget (≈120 words, ≈160 tokens worst case);
/// a real calibration run pending Q1 will tune.
const OVERFLOW_SAFETY_MARGIN_PCT: u32 = 25;

/// v60.7 — §1 latency-weighted local-cost selector. Determined once
/// at [`Runner::new`] time from the [`ProviderChoice`] + base URL;
/// reused for every `ModelCall` ledger entry emitted across the
/// run's turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCostPolicy {
    /// Apply [`atelier_core::ledger::local_cost_usd`] with the §1
    /// default `$0.00028/sec`. Used for the Mock adapter and for
    /// OpenAI-compatible adapters whose base URL is anything other
    /// than `api.openai.com` (i.e. local servers — LM Studio,
    /// llama-server, vLLM, Ollama, sglang).
    LatencyWeighted,
    /// Leave `cost_usd = None`; the §3 cost meter renders the entry
    /// as "+1 unknown". Used for cloud providers whose pricing must
    /// come from a per-provider table the harness doesn't yet ship
    /// (Anthropic Messages API, hosted OpenAI).
    UnknownPending,
}

impl Runner {
    /// Build a `Runner` for the given provider. Fallible because real
    /// providers (`Anthropic`) need credentials at construction time —
    /// `ANTHROPIC_API_KEY` missing → `RunError::Config`. The `Mock` branch
    /// is infallible.
    pub fn new(
        workspace: PathBuf,
        provider: ProviderChoice,
        sink: EventSink,
    ) -> Result<Self, RunError> {
        // v51: per-provider probe defaults. Mock + Anthropic are
        // well-characterised — no point spending two calibration
        // round-trips on them. OpenAI-compat is the unknown:
        // self-hosted models vary widely, so we cache-and-probe by
        // default. The CLI flips this for `--no-probe` / `--force-probe`.
        let (adapter, probe_policy, probe_base_url, cost_policy): (
            Arc<dyn Adapter>,
            ProbePolicy,
            String,
            ModelCostPolicy,
        ) = match provider {
            ProviderChoice::Mock { responses } => (
                Arc::new(build_mock_adapter(responses)),
                ProbePolicy::Skip,
                String::new(),
                // Mock is a local in-process actor — latency is
                // ~0ms so the cost ends up ~$0, but emitting it
                // exercises the same ledger path the real local
                // adapters take.
                ModelCostPolicy::LatencyWeighted,
            ),
            ProviderChoice::Anthropic { model_id } => (
                Arc::new(AnthropicAdapter::from_env(model_id).map_err(adapter_to_run_error)?),
                ProbePolicy::Skip,
                String::new(),
                // Anthropic's wire usage tells us the *tokens*;
                // the *dollars* require a per-model pricing
                // table we don't yet ship. Leave the row's
                // `cost_usd` empty so the meter doesn't lie.
                ModelCostPolicy::UnknownPending,
            ),
            ProviderChoice::OpenAiCompat {
                model_id,
                base_url,
                cache_prompt,
            } => {
                // Empty OPENAI_API_KEY is OK — most local servers
                // (LM Studio, llama-server, vLLM, Ollama-compat)
                // don't require auth. A 401 from a server that
                // *does* require it surfaces as AdapterError::Auth
                // at first call.
                //
                // base_url None → adapter default (OpenAI). Local
                // servers must be explicit via --base-url (or
                // OPENAI_BASE_URL via from_env), since pointing at
                // OpenAI by accident with a `local:` model id would
                // 404 in a confusing way.
                let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
                // v60.32 M01 — only consult `OPENAI_BASE_URL` when no
                // CLI flag and no profile entry set the base url.
                // Documented precedence is `CLI > profile > env >
                // default`; emit a one-shot `tracing::info!` recording
                // which layer won so an operator can diagnose surprise
                // origins.
                let (base, source) =
                    resolve_openai_base_url(base_url, std::env::var("OPENAI_BASE_URL").ok());
                tracing::info!(layer = source, base_url = %base, "openai-compat base url resolved");
                // §1 BYOM: "Mock + OpenAI-compat (local servers)"
                // get the latency-weighted local rate.  Hosted
                // OpenAI is the one openai-compat target that's a
                // *cloud* provider — pricing comes from a future
                // per-provider table, so leave `cost_usd = None`
                // for now. Any base_url other than the canonical
                // OpenAI host is treated as local.
                let cost = if is_openai_cloud_base_url(&base) {
                    ModelCostPolicy::UnknownPending
                } else {
                    ModelCostPolicy::LatencyWeighted
                };
                let mut oa = atelier_core::adapter::openai_compat::OpenAiCompatAdapter::new(
                    api_key,
                    model_id,
                    base.clone(),
                );
                if cache_prompt {
                    oa = oa.with_cache_prompt(true);
                }
                (Arc::new(oa), ProbePolicy::Auto, base, cost)
            }
        };
        Ok(Self {
            workspace,
            adapter,
            executor_adapter: None,
            sink,
            max_turns: 32,
            approval_policy: atelier_core::dispatcher::ApprovalPolicy::AutoApproveAll,
            dispatcher_handle: None,
            adapter_handle: None,
            probe_policy,
            probe_base_url,
            pane_visibility: None,
            cost_policy,
            concurrent_edit_policy: ConcurrentEditPolicy::Modal,
            resume_from: None,
            initial_mental_model: None,
            non_interactive: false,
            degradation_window: atelier_core::protocol_conformance::DEFAULT_DEGRADATION_WINDOW,
            degradation_threshold:
                atelier_core::protocol_conformance::DEFAULT_DEGRADATION_THRESHOLD,
            overflow_policy: ContextOverflowPolicy::Compact,
            few_shot_cache: parking_lot::Mutex::new(None),
            pending_swap: parking_lot::Mutex::new(None),
            starting_strategy_override: None,
            tier1_diagnostics_for_test: Vec::new(),
            external_cancel: None,
            skill_registry: None,
            subagent_depth: 0,
            pinned_profile: None,
        })
    }

    /// §1 per-task routing — install a fast/cheap executor adapter for
    /// tool-result turns. When set, turns whose last message has
    /// `Role::Tool` use this adapter instead of the primary one. Lets
    /// a small local model handle follow-through tool calls while the
    /// primary (larger) model handles planning / user-facing turns.
    #[allow(dead_code)] // used by main.rs and atelier-gui; not exercised in integration tests
    pub fn with_executor_adapter(mut self, adapter: Arc<dyn Adapter>) -> Self {
        self.executor_adapter = Some(adapter);
        self
    }

    /// v60.51 §15 — install a skill registry so the runner expands a
    /// leading `/<name>` prompt via its `prompt_template`. Skipping
    /// this leaves the slash literal in the prompt — useful for tests
    /// that want to drive the raw text through the loop.
    pub fn with_skill_registry(
        mut self,
        registry: Arc<atelier_core::skills::SkillRegistry>,
    ) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    /// v60.37 A3 — public accessor for the runner's [`ModelCostPolicy`]
    /// so the GUI/TUI compaction commands can pass the same policy into
    /// [`crate::compaction::compact`] that the runner's main loop uses.
    /// Without this, off-main-loop compactions silently dropped the
    /// `LatencyWeighted` attribution for local providers.
    ///
    /// `#[allow(dead_code)]`: today only the GUI/TUI driver paths would
    /// call this through a `Runner` handle, and they go via the helper
    /// at the call site rather than holding a Runner reference. The
    /// accessor is exposed for future driver wiring.
    #[allow(dead_code)]
    pub fn cost_policy(&self) -> ModelCostPolicy {
        self.cost_policy
    }

    /// v60.29 H10 — wire a caller-supplied cancellation token so an
    /// external SIGINT/SIGTERM handler can unwind the in-flight run.
    /// `Runner::run` passes it through to `session::spawn_with_cancel_token`;
    /// the dispatcher's outer `tokio::select!` observes it via
    /// `ToolContext::cancel` and surfaces `ToolError::Cancelled` mid-tool.
    ///
    /// `#[allow(dead_code)]`: only the `atelier` binary and the
    /// `sigint_resume` integration test reach this; the other integration
    /// tests pull `runner.rs` in via `#[path]` (separate compilation
    /// unit) and never call it, which the lint would otherwise flag.
    #[allow(dead_code)]
    pub fn with_external_cancel(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.external_cancel = Some(token);
        self
    }

    pub fn with_max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// Spec §3 hunk accept/reject: install the policy. With
    /// `AwaitApproval`, the dispatcher blocks at staging time and emits
    /// `Event::StagingPendingApproval`; the consumer feeds the accept
    /// set via `SessionDispatcher::submit_approval`.
    pub fn with_approval_policy(
        mut self,
        policy: atelier_core::dispatcher::ApprovalPolicy,
    ) -> Self {
        self.approval_policy = policy;
        self
    }

    /// Register a handle the runner writes into once the
    /// SessionDispatcher is built. See [`DispatcherHandle`] for the
    /// motivating use case.
    pub fn with_dispatcher_handle(mut self, handle: DispatcherHandle) -> Self {
        self.dispatcher_handle = Some(handle);
        self
    }

    /// v60.5 — register a slot the runner publishes its active adapter
    /// into. Enables `atelier_cli::compaction::compact` to be invoked
    /// from a UI thread without re-constructing the adapter.
    pub fn with_adapter_handle(mut self, handle: AdapterHandle) -> Self {
        self.adapter_handle = Some(handle);
        self
    }

    /// v60.9 — test-only adapter swap. Lets the integration suite drop in
    /// a custom `Adapter` impl (e.g. one that records the message history
    /// it received) without going through [`ProviderChoice`]. The
    /// production binary never calls this; production paths construct
    /// the adapter via [`Self::new`].
    ///
    /// `#[doc(hidden)]` so this doesn't appear in published docs.
    /// v60.32 M06 — gated under `#[cfg(any(test, feature =
    /// "test-seams"))]` so production builds can't pin stale
    /// strategies through this seam.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-seams"))]
    #[allow(dead_code)]
    pub fn with_adapter_for_test(mut self, adapter: Arc<dyn Adapter>) -> Self {
        self.adapter = adapter;
        // Clear the cache so the next `run` re-queries the new adapter's
        // `few_shot_override`. Without this, a Runner re-used across
        // adapters would surface stale overrides — pathological in
        // production, plausible in tests.
        *self.few_shot_cache.lock() = None;
        self
    }

    /// §10 Sub-agent spawning — replace the adapter with a pre-built one.
    /// Production callers (specifically `RunnerSpawner`) use this to give
    /// a child runner the parent's adapter without going through credential
    /// resolution again. Unlike `with_adapter_for_test` this is not gated
    /// behind `test-seams` because sub-agent spawning is a production path.
    pub fn with_adapter(mut self, adapter: Arc<dyn Adapter>) -> Self {
        self.adapter = adapter;
        *self.few_shot_cache.lock() = None;
        self
    }

    /// v60.10 §1 BYOM — swap the active adapter mid-session. Preserves
    /// conversation context, plan state, memory, and any in-flight
    /// approval pending. Resets the conformance window (new adapter,
    /// new behaviour signal) and re-emits `ModelProfileLoaded` on the
    /// next `run()` so the GUI/TUI footer refreshes the model badge +
    /// capability tooltip.
    ///
    /// The pre-swap adapter is dropped (returned `Arc`'s strong count
    /// falls to zero unless another reference is held — the
    /// `AdapterHandle` slot is updated to the new adapter in the same
    /// call so the slot doesn't keep the old one alive).
    ///
    /// This is a per-turn-boundary operation — the caller should
    /// invoke it between turns, not mid-stream. The pre-swap adapter's
    /// in-flight `chat()` future is NOT cancelled (drop-on-cancel
    /// applies via the existing CancellationToken; the caller decides
    /// whether to cancel first).
    ///
    /// State-preservation invariants:
    ///   * `ContextManager` — unchanged.
    ///   * `MemoryStore` — unchanged.
    ///   * `PlanCanvas` — unchanged.
    ///   * `Conversation` — unchanged (the next `run()` resumes via
    ///     `with_resume`, and the new adapter sees the same prefix on
    ///     its first chat call).
    ///   * `ConformanceWindow` — RESET. New adapter, no carryover.
    ///   * `Strategy` — re-resolved from the new adapter's
    ///     `ModelProfile` on the next run.
    ///   * `CapabilityMatrixRow` — refreshed via the new model's
    ///     matrix entry on the next run.
    ///   * `CostPolicy` — recomputed on `Runner::new` time; on a swap
    ///     we keep the existing policy because the swap goes through
    ///     a pre-built adapter (the caller decides policy).
    ///   * Pending approval — unchanged (lives on `SessionDispatcher`,
    ///     not on `Runner`; the user keeps accept/reject options).
    pub fn swap_adapter(
        &mut self,
        new_adapter: Arc<dyn Adapter>,
        now: &str,
    ) -> Result<(), RunError> {
        let from_model_id = self.adapter.model_id().to_string();
        let to_model_id = new_adapter.model_id().to_string();
        // Update the live adapter slot. The two-pass "swap then drop
        // the old Arc" pattern guarantees the slot doesn't hold both
        // adapters at once — important if a downstream consumer is
        // counting strong references to detect leaked adapters.
        self.adapter = new_adapter.clone();
        // Clear the per-session few-shot cache so the next `run()`
        // re-queries `few_shot_override` against the new adapter. A
        // stale cache would feed the new adapter's `chat()` an example
        // tailored for the previous adapter's quirks.
        *self.few_shot_cache.lock() = None;
        // Update the externally-visible `AdapterHandle` slot if one
        // was registered. Same Arc-replacement discipline: the slot
        // drops its previous adapter so the old Arc's strong count
        // can fall to zero. The slot is otherwise cleared at the end
        // of every `run()` via `AdapterHandleGuard` — so an external
        // call after `run()` exits is a no-op until the next `run()`
        // re-populates it.
        if let Some(handle) = &self.adapter_handle {
            let _ = handle.swap(new_adapter);
        }
        // Stash the announcement so the next `run()` emits
        // `Event::AdapterSwapped` + a fresh `Event::ModelProfileLoaded`
        // pair on the new bus. We can't emit here because between two
        // `run()` invocations no broadcast::Sender exists; queueing
        // also covers the rare case where two swaps happen
        // back-to-back without an intervening run (the second
        // overwrites the first — only the most-recent transition is
        // relevant to the UI).
        *self.pending_swap.lock() = Some(PendingAdapterSwap {
            from_model_id,
            to_model_id,
            swapped_at: now.to_string(),
        });
        Ok(())
    }

    /// Phase C close — record which UI panes the driver had visible
    /// for this run. The Runner writes a sibling
    /// `pane_visibility.json` next to `session.json` at end-of-run.
    /// `driver` is a free-form label ("gui", "tui", "headless").
    pub fn with_pane_visibility(
        mut self,
        panes: crate::instrumentation::PaneVisibility,
        driver: impl Into<String>,
    ) -> Self {
        self.pane_visibility = Some((panes, driver.into()));
        self
    }

    /// v51 — override the probe policy. Defaults are per-provider
    /// (set in [`Self::new`]); the CLI's `--no-probe` / `--force-probe`
    /// flags use this to overlay user intent on top of the provider
    /// default.
    ///
    /// `#[allow(dead_code)]`: only the binary's `--no-probe` /
    /// `--force-probe` paths reach this; tests rely on the per-provider
    /// default set in [`Self::new`].
    #[allow(dead_code)]
    pub fn with_probe_policy(mut self, policy: ProbePolicy) -> Self {
        self.probe_policy = policy;
        self
    }

    /// v61 — install the §14 concurrent-edit policy. `Modal` (default)
    /// surfaces `Event::FilesChanged` and awaits user choice;
    /// `AutoReload` is the headless answer used by `--non-interactive`.
    #[allow(dead_code)] // called from main.rs; integration tests pull this file via `#[path]`.
    pub fn with_concurrent_edit_policy(mut self, policy: ConcurrentEditPolicy) -> Self {
        self.concurrent_edit_policy = policy;
        self
    }

    /// v61 — `--non-interactive` mode. Composite flag: forces
    /// [`atelier_core::dispatcher::ApprovalPolicy::AutoApproveAll`] +
    /// [`ConcurrentEditPolicy::AutoReload`] so no run can block on a
    /// missing UI. Wins over any prior `with_approval_policy` /
    /// `with_concurrent_edit_policy` call (call this last).
    #[allow(dead_code)] // called from main.rs; integration tests pull this file via `#[path]`.
    pub fn with_non_interactive(mut self, on: bool) -> Self {
        self.non_interactive = on;
        if on {
            self.approval_policy = atelier_core::dispatcher::ApprovalPolicy::AutoApproveAll;
            self.concurrent_edit_policy = ConcurrentEditPolicy::AutoReload;
        }
        self
    }

    /// §1 BYOM — override the conformance-driven degradation window
    /// size. The default is
    /// [`atelier_core::protocol_conformance::DEFAULT_DEGRADATION_WINDOW`]
    /// (PROVISIONAL 20). Integration tests dial this down so a short
    /// scripted sequence can exercise the degradation path without
    /// queueing twenty mock responses. v60.32 M06 — gated under
    /// `test-seams`.
    #[cfg(any(test, feature = "test-seams"))]
    #[allow(dead_code)]
    pub fn with_degradation_window(mut self, window: usize) -> Self {
        self.degradation_window = window;
        self
    }

    /// §1 BYOM — override the conformance-driven degradation threshold
    /// (failures-in-window count). The default is
    /// [`atelier_core::protocol_conformance::DEFAULT_DEGRADATION_THRESHOLD`]
    /// (PROVISIONAL 3). See [`Self::with_degradation_window`] for the
    /// companion knob. v60.32 M06 — gated under `test-seams`.
    #[cfg(any(test, feature = "test-seams"))]
    #[allow(dead_code)]
    pub fn with_degradation_threshold(mut self, threshold: u32) -> Self {
        self.degradation_threshold = threshold;
        self
    }

    /// §1 BYOM — install the context-window asymmetry policy. Defaults
    /// to [`ContextOverflowPolicy::Compact`]; see the enum's docs for
    /// the three arms. The binary's `--overflow-policy` flag and a
    /// future GUI/TUI setting will flip this; today only the
    /// integration tests reach this seam.
    #[allow(dead_code)]
    pub fn with_overflow_policy(mut self, policy: ContextOverflowPolicy) -> Self {
        self.overflow_policy = policy;
        self
    }

    /// v61 — resume a previously-persisted session by UUID. The runner
    /// reads `<workspace>/.atelier/sessions/<uuid>/session.json`, replays
    /// the conversation prefix per
    /// [`atelier_core::OnDiskSession::resume_conversation_prefix`] (only
    /// turns through the last completed tool round-trip), and surfaces
    /// the on-disk `recovery_log` to UI consumers via
    /// `Event::MessageCommitted { role: System, … }`. The fresh prompt
    /// passed to [`Self::run`] is appended after the prefix as a new
    /// user turn — pass an empty string to resume without an additional
    /// prompt.
    pub fn with_resume(mut self, session_uuid: uuid::Uuid) -> Self {
        self.resume_from = Some(session_uuid);
        self
    }

    /// v60.20 §5 — seed the mental-model panel before the run starts.
    /// When `enabled == true && !text.trim().is_empty()` the text is
    /// injected as a second System message at the head of every
    /// per-turn `adapter.chat` call. Persists into the
    /// `SessionDispatcher`'s mental-model state so a subsequent GUI /
    /// TUI mutation lands on the same store.
    ///
    /// `None` (the default) keeps the dispatcher at off/empty; the UI
    /// can still flip it mid-run via the existing `set_mental_model`
    /// Tauri command / TUI keybinds.
    pub fn with_initial_mental_model(mut self, text: String, enabled: bool) -> Self {
        self.initial_mental_model = Some((text, enabled));
        self
    }

    /// Phase B Track D — pin the starting §2 strategy regardless of the
    /// model profile's recommendation. Lets the Phase B mechanical gate
    /// exercise the `JsonSentinel` and `RegexProse` parse arms end-to-end
    /// against the `MockAdapter` (whose declared capabilities always
    /// resolve to `NativeTool`). Production callers should not set this —
    /// the probe-on-first-use + conformance tracker pair owns strategy
    /// selection in real runs. v60.32 M06 — gated under `test-seams`.
    #[cfg(any(test, feature = "test-seams"))]
    #[allow(dead_code)]
    pub fn with_starting_strategy_override(mut self, strategy: Strategy) -> Self {
        self.starting_strategy_override = Some(strategy);
        self
    }

    /// Phase B Track C3 — pre-mapped Tier-1 LSP discrepancies for the
    /// hallucinating-agent gate test. Pure test seam; production
    /// callers leave the vec empty (the runner uses bare
    /// `verify_pass` in that case). Once `async-lsp` lands, the
    /// runner produces these from the LSP receiver and this builder
    /// stays unused. v60.32 M06 — gated under `test-seams`.
    #[cfg(any(test, feature = "test-seams"))]
    #[allow(dead_code)]
    pub fn with_tier1_diagnostics_for_test(
        mut self,
        discrepancies: Vec<atelier_core::verify::Discrepancy>,
    ) -> Self {
        self.tier1_diagnostics_for_test = discrepancies;
        self
    }

    /// §10 — set the recursion depth for sub-agent runners. Called by
    /// `RunnerSpawner` when constructing a child runner; root runners
    /// stay at 0 (the default). The depth is threaded into every
    /// `ToolContext` so nested `spawn_subagent` calls can enforce the cap.
    pub fn with_subagent_depth(mut self, depth: u8) -> Self {
        self.subagent_depth = depth;
        self
    }

    /// Pass a pre-resolved `ModelProfile` from the parent runner so the
    /// child runner skips the probe entirely and starts with the parent's
    /// observed emission strategy. Without this, `ProbePolicy::Skip`
    /// falls back to adapter capability defaults which may diverge from
    /// what the actual probe measured (e.g. a model that claims
    /// native-tool support but fails it).
    pub fn with_model_profile(
        mut self,
        profile: atelier_core::adapter::model_profile::ModelProfile,
    ) -> Self {
        self.pinned_profile = Some(profile);
        self
    }

    /// v60.51 §15 — pre-turn slash-command expansion.
    ///
    /// Returns `(expanded_prompt, pending_skill_note)`:
    ///
    /// * If `prompt` does not start with `/`, the input is returned
    ///   unchanged and the note is `None`.
    /// * If the runner has no [`SkillRegistry`] installed, slashes are
    ///   left in place (so tests can still drive a literal `/foo`
    ///   prompt through the model).
    /// * If the slash is `/help`, expansion is short-circuited: the
    ///   registry's `format_help()` output is returned in place of the
    ///   prompt with no note. (The CLI binary intercepts `/help` at
    ///   parse time so the model is never asked to digest the help
    ///   text; this branch matters for programmatic callers that drive
    ///   `Runner::run` directly.)
    /// * Otherwise: parse args, substitute, return the expansion plus
    ///   `Some("skill: <name>")` so the next `ModelCall` ledger entry
    ///   can carry the §15 attribution.
    fn expand_skill_prompt(&self, prompt: String) -> Result<(String, Option<String>), RunError> {
        let Some(rest) = prompt.strip_prefix('/') else {
            return Ok((prompt, None));
        };
        let Some(registry) = self.skill_registry.as_ref() else {
            return Ok((prompt, None));
        };
        // Strip the leading `/`, split into name + args.
        let (name, raw_args) = match rest.find(char::is_whitespace) {
            Some(i) => (&rest[..i], rest[i..].trim_start()),
            None => (rest, ""),
        };

        // `/help` is harness-intercepted per spec §15 line 785. The
        // returned string is rendered as the next user message; in
        // practice the CLI short-circuits earlier so the model never
        // sees this.
        if name == "help" {
            return Ok((registry.format_help(), None));
        }

        let Some(skill) = registry.get(name) else {
            let available: Vec<String> = registry.names().cloned().collect();
            return Err(RunError::SkillUnknown {
                name: name.to_string(),
                available,
            });
        };
        let args = atelier_core::skills::parse_args(skill, raw_args).map_err(|e| {
            RunError::SkillSubstitution {
                name: name.to_string(),
                source: e,
            }
        })?;
        let ctx = atelier_core::skills::SkillSubstitutionContext {
            repo_root: &self.workspace,
            args: &args,
            atelier_md: None,
        };
        let expanded = atelier_core::skills::substitute(skill, &ctx).map_err(|e| {
            RunError::SkillSubstitution {
                name: name.to_string(),
                source: e,
            }
        })?;
        Ok((expanded, Some(format!("skill: {name}"))))
    }

    /// Drive the loop until `claims_done` or `max_turns`. Returns when:
    ///   * a turn carried `claims_done: true` (success path; runs DoD next),
    ///   * `max_turns` reached (timeout; `final_state = AwaitingUser`),
    ///   * the adapter errored irrecoverably (propagated).
    pub async fn run(&self, prompt: String) -> Result<RunReport, RunError> {
        let workspace = self.workspace.clone();

        // v60.51 §15 — slash-command expansion. Runs *before* hooks /
        // DoD / sandbox so a `/foo` typo bails cleanly without leaving
        // half-loaded state behind. `pending_skill_note` is consumed
        // by the first `ModelCall` ledger append below.
        let (prompt, mut pending_skill_note) = self.expand_skill_prompt(prompt)?;

        // 1. Load config: hooks + DoD. Both are tolerant of missing files —
        //    a fresh repo with no .atelier/hooks/ or .atelier/dod.json
        //    just gets an empty HookSet + None DoD.
        let hooks = HookSet::load_dir(&workspace.join(".atelier/hooks"))
            .map_err(|e| RunError::Config(format!("hooks: {e}")))?;
        let dod = DodConfig::load(&workspace).map_err(|e| RunError::Config(format!("dod: {e}")))?;

        // 2. Sandbox + dispatcher + ledger.
        let sandbox = SandboxPolicy::restrictive(&workspace)
            .map_err(|e| RunError::Config(format!("sandbox: {e}")))?;

        // §10 — wire spawn_subagent. Load the type registry (tolerates missing
        // directory — falls back to bundled-only types). Build the spawner that
        // shares the parent's adapter; register it as the 8th built-in.
        let home_dir = std::env::var_os("HOME").map(PathBuf::from);
        let type_registry = Arc::new(
            SubagentTypeRegistry::load(&workspace, home_dir.as_deref())
                .map_err(|e| RunError::Config(format!("subagent type registry: {e}")))?,
        );
        let spawner = Arc::new(RunnerSpawner::new(
            self.adapter.clone(),
            workspace.clone(),
            type_registry.clone(),
        ));
        let registry = built_in_registry_with_deps(BuiltinDeps {
            spawner: spawner.clone(),
            type_registry,
        })?;
        // Snapshot the §15 tool surface for the adapter's native tool-use
        // channel *before* moving the registry into the dispatcher.
        // Without this the model only sees an empty `tools` array on the
        // wire and has nothing to invoke — the §2.5 loop then stalls per
        // the v60.15 stall guard.
        let tools_spec = registry.tool_specs();
        let dispatcher = Dispatcher::new(registry, hooks)
            .with_executor(Arc::new(ShellHookExecutor::new(sandbox.clone())));
        let ledger = Arc::new(Ledger::new());

        // v55 — §5 shared state. Owned here, cloned into the
        // SessionDispatcher so the UI's mutator commands (pin / evict /
        // add-card / mark-step-done) land on the same store the runner
        // re-emits from at each turn boundary.
        let context_manager = Arc::new(parking_lot::Mutex::new(ContextManager::new()));
        let memory_store = Arc::new(parking_lot::Mutex::new(MemoryStore::new()));
        let plan_canvas = Arc::new(parking_lot::Mutex::new(PlanCanvas::new()));

        // 3. Session actor + SessionDispatcher.
        //
        //    v60.29 H10 — if the caller supplied an external
        //    cancellation token (the CLI does this from its
        //    SIGINT/SIGTERM handler), thread it into the session so
        //    tripping the token unwinds the actor + every dispatched
        //    tool. The default unparameterised path still works for
        //    GUI/TUI driver runs that don't wire signal handling.
        let session_handle = match self.external_cancel.clone() {
            Some(token) => atelier_core::session::spawn_with_cancel_token(
                Arc::new(NoopHook),
                Arc::new(NoopHook),
                atelier_core::session::INBOX_CAPACITY,
                token,
            ),
            None => session::spawn(Arc::new(NoopHook), Arc::new(NoopHook)),
        };
        let bus = session_handle.events_sender();
        // Wire the parent bus into the spawner so SubagentSpawned /
        // SubagentCompleted events reach the GUI/TUI sub-agent panel.
        spawner.set_bus(bus.clone());

        // v61 — §14 per-session file watcher. The dispatcher feeds the
        // read-set after each `read_file` / `list_dir` / `grep` / `ast_grep`
        // call; external edits to any tracked path surface as
        // `Event::FilesChanged`. Init failure is non-fatal: we fall back
        // to a disabled handle so the run still progresses (concurrent
        // edits just go undetected — same as a pre-v61 build).
        let file_watcher_handle = match file_watcher::spawn(
            bus.clone(),
            file_watcher::FILE_WATCH_DEBOUNCE,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "file watcher init failed; concurrent-edit detection disabled");
                file_watcher::FileWatcherHandle::disabled()
            }
        };

        let session_dispatcher = Arc::new(
            SessionDispatcher::new(dispatcher, ledger.clone(), bus.clone())
                .with_approval_policy(self.approval_policy)
                .with_shared_state(
                    context_manager.clone(),
                    memory_store.clone(),
                    plan_canvas.clone(),
                )
                .with_file_watcher(file_watcher_handle.clone()),
        );

        // v60.20 §5 — seed the mental-model panel if the caller pre-set
        // it via `with_initial_mental_model`. Errors here surface as
        // `RunError::Config` because they only fire on text-safety
        // violations (Trojan-Source bytes, etc.) — a misuse, not a
        // runtime adapter issue.
        if let Some((text, enabled)) = self.initial_mental_model.clone() {
            session_dispatcher
                .set_mental_model(text, enabled, &now_rfc3339())
                .map_err(|e| RunError::Config(format!("initial mental_model: {e}")))?;
        }

        // v61 — §14 concurrent-edit policy. Under `AutoReload` we wire
        // a background task that auto-emits FilesChangedAcknowledged
        // every time the watcher reports FilesChanged. The Modal arm
        // leaves resolution to the UI consumer (GUI / TUI). Under
        // Modal we also start the 5-minute auto-pause timer per
        // spec §14; it's cancellable on the next FilesChangedAcknowledged.
        let auto_reload_task = spawn_concurrent_edit_resolver(
            bus.clone(),
            self.concurrent_edit_policy,
            std::time::Duration::from_secs(5 * 60),
        );
        // Publish the dispatcher to any caller-registered handle BEFORE
        // we start dispatching. The GUI's `submit_approval` Tauri
        // command reads this slot to route accept-sets back to the
        // dispatcher.
        //
        // v49 scope-guard: every exit path from `run()` — success,
        // `?`-propagated error, panic — must clear the handle so a
        // stale Arc isn't left pointing at a torn-down dispatcher. The
        // Drop impl below runs in LIFO so the handle is cleared
        // BEFORE the `session_dispatcher` Arc is dropped, which is the
        // ordering `submit_approval` checks against.
        if let Some(handle) = &self.dispatcher_handle {
            handle.set(session_dispatcher.clone());
        }
        let _handle_guard = DispatcherHandleGuard {
            handle: self.dispatcher_handle.as_ref(),
        };
        // v60.5 — companion publication of the active adapter so the
        // UI's `compact_context_items` command (and the TUI's
        // `Mutation::Compact` arm) can issue the §5 summary call
        // without re-constructing the adapter. Same guard discipline
        // as the dispatcher slot.
        if let Some(handle) = &self.adapter_handle {
            handle.set(self.adapter.clone());
        }
        let _adapter_handle_guard = AdapterHandleGuard {
            handle: self.adapter_handle.as_ref(),
        };

        // 4. Drain events into the sink. tokio task; exits when the
        //    broadcast channel closes (we hold `session_handle` so this is
        //    after we Shutdown below).
        let mut event_rx = session_handle.subscribe();
        let sink_handle = spawn_sink_drain(&self.sink, &mut event_rx);

        // 4b. v51 — resolve the model profile (cached or freshly
        //     probed) and broadcast it so UIs can render the active
        //     §2 strategy badge before the first turn lands. For
        //     `Skip` policy adapters (Mock, Anthropic) we build a
        //     stub from `Adapter::capabilities()`; for `Auto`/`Force`
        //     we call into `ProfileStore::load_or_probe`. A probe
        //     failure logs + falls back to a stub so the run still
        //     proceeds — better to lose a probe than to refuse to
        //     start.
        let profile_now = now_rfc3339();
        let caps = self.adapter.capabilities();
        let (profile, outcome) = if let Some(pinned) = self.pinned_profile.clone() {
            // Child runner: use parent's resolved profile so children
            // inherit the observed emission strategy without re-probing.
            (
                pinned,
                atelier_core::adapter::model_profile::ProbeLoadOutcome::CacheHit,
            )
        } else {
            match self.probe_policy {
                ProbePolicy::Skip => {
                    let strategy = if caps.native_tool_use.is_usable() {
                        atelier_core::protocol_strategy::Strategy::NativeTool
                    } else {
                        atelier_core::protocol_strategy::Strategy::JsonSentinel
                    };
                    let p =
                        atelier_core::adapter::model_profile::ModelProfile::skipped_for_well_known(
                            self.adapter.model_id(),
                            strategy,
                            caps.context_window_tokens,
                            atelier_core::adapter::model_profile::DEFAULT_PROFILE_MAX_TOKENS,
                            profile_now.clone(),
                        );
                    (
                        p,
                        atelier_core::adapter::model_profile::ProbeLoadOutcome::CacheHit,
                    )
                }
                ProbePolicy::Auto | ProbePolicy::Force => {
                    let force = matches!(self.probe_policy, ProbePolicy::Force);
                    let store = atelier_core::adapter::model_profile::ProfileStore::user_default();
                    match store
                        .load_or_probe(
                            self.adapter.as_ref(),
                            &self.probe_base_url,
                            force,
                            profile_now.clone(),
                        )
                        .await
                    {
                        Ok((p, o)) => (p, o),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "model probe failed; falling back to a default profile and \
                                 continuing with §1 conformance-tracker-driven strategy selection"
                            );
                            let strategy = if caps.native_tool_use.is_usable() {
                                atelier_core::protocol_strategy::Strategy::NativeTool
                            } else {
                                atelier_core::protocol_strategy::Strategy::JsonSentinel
                            };
                            let p = atelier_core::adapter::model_profile::ModelProfile::skipped_for_well_known(
                            self.adapter.model_id(),
                            strategy,
                            caps.context_window_tokens,
                            atelier_core::adapter::model_profile::DEFAULT_PROFILE_MAX_TOKENS,
                            profile_now,
                        );
                            (
                                p,
                                atelier_core::adapter::model_profile::ProbeLoadOutcome::NotCached,
                            )
                        }
                    }
                }
            }
        }; // closes else branch / let binding
           // v60.7 §1 BYOM — build the capability matrix row for this
           // model. The static lookup table covers the providers we
           // already ship; unknown models fall back to the adapter's
           // runtime `Capabilities` declaration. The probe cross-walk
           // flips columns to `ClaimedButBroken` if the probe observed
           // the model failing a capability the table claims.
        let capability_row = {
            let base_row = atelier_core::adapter::capability_matrix::matrix_row_for(
                self.adapter.model_id(),
                &caps,
            );
            atelier_core::adapter::capability_matrix::crosswalk_with_profile(base_row, &profile)
        };
        let _ = try_emit(
            &bus,
            Event::ModelProfileLoaded {
                model_id: profile.model_id.clone(),
                base_url: profile.base_url.clone(),
                strategy: profile.strategy,
                outcome,
                capability_row: Some(capability_row),
            },
        );
        // v60.10 §1 BYOM — consume the swap announcement queued by an
        // earlier `swap_adapter` call (if any). Emit
        // `Event::AdapterSwapped` AFTER the initial
        // `ModelProfileLoaded` so subscribers see "profile resolved,
        // swap announced" in temporal order — a UI rendering a toast
        // for the swap can then read the latest `currentModel` it
        // just folded in. The pair is followed by no further bus
        // events until the first turn proper.
        if let Some(swap) = self.pending_swap.lock().take() {
            let _ = try_emit(
                &bus,
                Event::AdapterSwapped {
                    from_model_id: swap.from_model_id,
                    to_model_id: swap.to_model_id,
                    swapped_at: swap.swapped_at,
                },
            );
        }
        // Feed the resolved profile to the spawner so child sub-agents
        // inherit the parent's observed emission strategy.
        spawner.set_profile(profile.clone());
        // The profile recommends the starting §2 strategy; the
        // runtime conformance tracker downshifts it (one-way) if the
        // model emits malformed envelopes past the threshold. The
        // active strategy lives in `active_strategy`; degrade events
        // emit on every transition so UIs refresh the footer badge.
        //
        // Phase B Track D — `starting_strategy_override` (test-only)
        // wins over `profile.strategy` so the mechanical gate can drive
        // the `JsonSentinel` / `RegexProse` parse arms end-to-end
        // through `MockAdapter`. Production callers leave it `None`.
        let mut active_strategy = self.starting_strategy_override.unwrap_or(profile.strategy);
        // Rolling envelope-parse window. Successes / failures recorded
        // by the parse arm of the turn loop drive
        // `should_degrade` — see protocol_conformance::ConformanceRingBuffer.
        let mut envelope_conformance =
            atelier_core::protocol_conformance::ConformanceRingBuffer::new();

        // 5. Turn loop. v61 — when `resume_from` is set, replay the
        //    persisted conversation prefix first; the supplied prompt
        //    is then appended as a fresh user turn (or skipped when
        //    empty). On a fresh run we keep the pre-v61 single-prompt
        //    bootstrap.
        let session_id = session_handle.id();
        // §11 / §12 — pre-compute the per-session audit-log path so
        // every dispatched tool call sees the same destination via
        // `ToolContext::audit_log_path`. The path lives alongside
        // `session.json` in the per-repo session dir; the directory
        // is created lazily on first append (see `audit::append_*`).
        // On a resume run we keep the prior session uuid so the audit
        // trail accumulates in one place across the resume boundary.
        let audit_session_uuid = self.resume_from.unwrap_or(session_id.0);
        let audit_log_path =
            OnDiskSession::session_dir(&workspace, audit_session_uuid).join("audit.log");
        let mut messages: Vec<Message> = Vec::new();

        // v60.17 §2 — atelier-flavoured system prompt. Without this, the
        // model gets the user task verbatim with no instruction that the
        // harness expects an envelope to signal completion. Surfaced by
        // the t01 live re-probe where Claude completed the task, ran
        // tests, then burned the turn budget describing the outcome with
        // no way to claim done. Only fired on fresh runs — a resumed
        // conversation already carries the original system message in
        // its on-disk prefix (re-hydrated below).
        if self.resume_from.is_none() {
            messages.push(Message::text(
                Role::System,
                build_atelier_system_prompt(&workspace, active_strategy),
            ));
        }

        // v60.9 §2 — consult the adapter's per-strategy few-shot override
        // once per session (the cache below ensures it's not recomputed
        // per turn). `None` means "use the shared baseline"; the runner's
        // current shared baseline is empty (the spec §2 baseline lives in
        // `prompts/protocol_fewshot/` as fixtures consumed by the rig),
        // so we only prepend when the adapter actively wants to teach the
        // model a provider-specific carrier shape. Today: Anthropic + the
        // OpenAI-compat adapters override for `JsonSentinel`; Mock keeps
        // the default `None`.
        //
        // The override is recorded on the per-session cache so a later
        // re-query (e.g. for an in-flight UI inspection) returns the same
        // messages without re-entering the adapter. We deliberately do
        // NOT re-fetch the override if `active_strategy` degrades during
        // the run — the conversation history already carries the
        // initial-strategy example and re-priming mid-run with a
        // different example would confuse the model. (If a future spec
        // revision asks for re-priming, the cache layout makes that a
        // one-line change.)
        let few_shot_prefix: Vec<Message> = {
            let mut cache = self.few_shot_cache.lock();
            if let Some(cached) = cache.as_ref() {
                cached.clone()
            } else {
                let computed = self
                    .adapter
                    .few_shot_override(active_strategy)
                    .unwrap_or_default();
                *cache = Some(computed.clone());
                computed
            }
        };
        for m in &few_shot_prefix {
            messages.push(m.clone());
        }

        let mut resumed_session: Option<OnDiskSession> = None;
        let prompt_now = now_rfc3339();

        if let Some(resume_uuid) = self.resume_from {
            let session_dir = OnDiskSession::session_dir(&workspace, resume_uuid);
            let on_disk = OnDiskSession::load_from(&session_dir).map_err(|e| {
                RunError::Config(format!("resume: cannot load session {resume_uuid}: {e}"))
            })?;
            // Re-hydrate the in-memory message list from the prefix.
            for entry in on_disk.resume_conversation_prefix() {
                let role = match entry.role.as_str() {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    "tool" => Role::Tool,
                    "system" => Role::System,
                    other => {
                        tracing::warn!(role = %other, "resume: skipping unknown role");
                        continue;
                    }
                };
                let tool_calls: Vec<ToolCallRequest> = entry
                    .tool_calls
                    .iter()
                    .filter_map(reconstruct_tool_call_request)
                    .collect();
                let msg = Message {
                    role,
                    content: entry.content.clone(),
                    tool_call_id: entry.tool_call_id.clone(),
                    tool_calls,
                };
                let role_bus = match role {
                    Role::User => MessageRole::User,
                    Role::Assistant => MessageRole::Assistant,
                    Role::Tool => MessageRole::Tool,
                    Role::System => MessageRole::System,
                };
                let _ = try_emit(
                    &bus,
                    Event::MessageCommitted {
                        role: role_bus,
                        text: entry.content,
                    },
                );
                messages.push(msg);
            }
            // Surface every recovery_log entry to UIs as a system
            // message so the user knows what was preserved. The
            // entries themselves stay on the persisted recovery_log;
            // we re-write them to the next save so the audit trail is
            // never erased.
            for rec in &on_disk.recovery_log {
                let _ = try_emit(
                    &bus,
                    Event::MessageCommitted {
                        role: MessageRole::System,
                        text: format!(
                            "[recovery] turn={} reason={:?} captured_at={} partial={:?}",
                            rec.turn_id, rec.reason, rec.captured_at, rec.partial_content
                        ),
                    },
                );
            }
            resumed_session = Some(on_disk);
            if !prompt.trim().is_empty() {
                context_manager
                    .lock()
                    .add(context_item_for_user_prompt(&prompt, &prompt_now));
                let _ = try_emit(
                    &bus,
                    Event::MessageCommitted {
                        role: MessageRole::User,
                        text: prompt.clone(),
                    },
                );
                messages.push(Message::text(Role::User, prompt.clone()));
            }
        } else {
            // Fresh run — pre-v61 behaviour.
            messages.push(Message::text(Role::User, prompt.clone()));
            context_manager
                .lock()
                .add(context_item_for_user_prompt(&prompt, &prompt_now));
            // Broadcast the initial user prompt so the conversation pane
            // catches up before the first turn. Best-effort send (no
            // subscribers is fine — see SessionDispatcher::dispatch).
            let _ = try_emit(
                &bus,
                Event::MessageCommitted {
                    role: MessageRole::User,
                    text: prompt.clone(),
                },
            );
        }
        // v57 (M-bug-3 fix) — emit one ContextItems snapshot before
        // entering the turn loop so a UI subscriber that joins
        // immediately after `MessageCommitted{User}` doesn't see an
        // empty Context panel until turn 1 finishes (which never
        // happens for max_turns=0). The aggregate `ContextSnapshot`
        // still fires per-turn; this pre-loop emission is the
        // per-item snapshot only.
        let initial_items = context_manager.lock().summarise();
        let _ = try_emit(
            &bus,
            Event::ContextItems {
                items: initial_items,
            },
        );
        let mut turns = 0;
        let mut final_state = State::Idle;

        // v60.8 A2 follow-on — accumulate the observed file changes
        // across turns so we can feed them into the §7 verify pass at
        // end-of-run. Each tool dispatch's `EditStaged` events carry
        // the path + a `Hunks` discriminator that maps cleanly onto
        // [`atelier_core::verify::ObservedKind`]; we keep the latest
        // observation per path so repeat-edits collapse to one entry.
        //
        // The latest envelope rides alongside so the verify pass has
        // its `claimed_changes` to compare against. `verify_pass` is
        // a no-op (badge stays `NotRun`) when both vectors are empty,
        // which is what `emit_verify_not_run` makes explicit on the bus.
        let mut observed_changes: Vec<atelier_core::verify::ObservedChange> = Vec::new();
        let mut last_envelope: Envelope = Envelope::default();
        // §10 — track the last assistant text for sub-agent result extraction.
        let mut last_assistant_text: Option<String> = None;

        for turn in 0..self.max_turns {
            // v60.15 (M-bug-state-desync) — only fire the `Idle → Streaming`
            // edge once at the start of the run. After turn 0 the state
            // is already `Streaming` (or `ToolExecuting → Streaming` after
            // a tool dispatch), and unconditionally re-advancing was
            // emitting an `IllegalTransitionAttempted{Streaming, Streaming}`
            // bus event on every turn beyond the first. The spec §2.5
            // table has no `Streaming → Idle` edge — multi-turn iteration
            // stays inside `Streaming` modulo the `Streaming ↔ Tool*`
            // sub-cycle.
            if final_state == State::Idle {
                advance(&session_handle, State::Idle, State::Streaming).await?;
                final_state = State::Streaming;
            }

            // §1 BYOM context-window asymmetry — wrap `adapter.chat()`
            // in a small retry loop that consults
            // [`ContextOverflowPolicy`] when the adapter raises
            // `AdapterError::ContextOverflow`. Compact / Reroute /
            // Surface arms each emit `ContextOverflowResolved` so the
            // bus carries one terminal marker per overflow, regardless
            // of which arm fired. The retry cap is
            // [`MAX_OVERFLOW_RETRIES`] consecutive attempts; after that
            // the runner drops to Surface behaviour rather than
            // looping forever.
            // v60.17 §2 — advertise the synthetic `harness_meta` tool to
            // the model when the active strategy is `NativeTool`. Without
            // this, the model has no way to signal `claimed_done` /
            // `claimed_changes` and the loop stalls after the task is
            // really done (surfaced by the t01 live re-probe where Claude
            // completed the task, ran tests, then burned the remaining
            // turn budget describing the result in prose). The list is
            // recomputed per turn because `active_strategy` can degrade
            // mid-run via the §1 conformance tracker.
            let turn_tools_spec: Vec<atelier_core::adapter::ToolSpec> = match active_strategy {
                atelier_core::protocol_strategy::Strategy::NativeTool => {
                    let mut v = Vec::with_capacity(tools_spec.len() + 1);
                    v.push(atelier_core::protocol_strategy::harness_meta_tool_spec());
                    v.extend(tools_spec.iter().cloned());
                    v
                }
                atelier_core::protocol_strategy::Strategy::JsonSentinel
                | atelier_core::protocol_strategy::Strategy::RegexProse => tools_spec.clone(),
            };

            // v60.20 §5 — mental-model injection. Snapshot the
            // SessionDispatcher's mental-model state once per turn
            // (cheap; the snapshot is a clone of a tiny struct) and,
            // when enabled + non-empty, prepend a second System
            // message to the per-turn message vec carrying the user's
            // text. The history `messages` is NOT mutated — the on-disk
            // conversation transcript stays free of the panel preamble
            // (which lives separately in `mental_model.json` and would
            // re-inject on resume anyway).
            // v60.32 M03 — `messages_for_call` is rebuilt at the head
            // of every retry iteration so a compaction that runs in
            // the `ContextOverflow → Compact` arm below feeds the
            // post-mutation history into the next chat call. Pre-fix,
            // the projection was captured once outside the loop and
            // the retry re-sent the pre-compaction snapshot, defeating
            // the compaction. The mental-model snapshot is also
            // re-read each iteration so a concurrent
            // `set_mental_model` mutation lands on the retry too.
            let project_messages_for_call = |history: &[Message]| -> Vec<Message> {
                let mm_snapshot = session_dispatcher.snapshot_mental_model();
                if mm_snapshot.enabled && !mm_snapshot.text.trim().is_empty() {
                    let mut v = Vec::with_capacity(history.len() + 1);
                    // Insert immediately after the atelier system prompt
                    // (which is history[0] on a fresh run) so both system
                    // messages land together at the head of the
                    // conversation; Anthropic concatenates multiple system
                    // entries cleanly, OpenAI-compat keeps them as separate
                    // `system`-role rows. On a resumed run the prepended
                    // entry sits ahead of the rehydrated prefix — both
                    // shapes are acceptable wire-wise.
                    let insert_pos =
                        if !history.is_empty() && matches!(history[0].role, Role::System) {
                            1
                        } else {
                            0
                        };
                    v.extend(history.iter().take(insert_pos).cloned());
                    v.push(Message::text(
                        Role::System,
                        format!(
                            "User-supplied mental model / working hypothesis. The user \
                         maintains this in the Atelier §5 mental-model panel; it is \
                         additional context layered on top of the §2 protocol \
                         instructions above. Treat it as guidance, not as ground \
                         truth: the user may be wrong, and you should still verify \
                         claims via tools.\n\n{}",
                            mm_snapshot.text.trim()
                        ),
                    ));
                    v.extend(history.iter().skip(insert_pos).cloned());
                    v
                } else {
                    history.to_vec()
                }
            };

            let response = {
                // §1 per-task routing: if the last message is a tool
                // result, use the executor adapter (fast/cheap) when
                // one is configured; otherwise fall back to the primary.
                let call_adapter: &dyn Adapter = {
                    let is_tool_turn = messages
                        .last()
                        .map(|m| matches!(m.role, Role::Tool))
                        .unwrap_or(false);
                    if is_tool_turn {
                        self.executor_adapter
                            .as_deref()
                            .unwrap_or(self.adapter.as_ref())
                    } else {
                        self.adapter.as_ref()
                    }
                };
                let mut overflow_retries: usize = 0;
                loop {
                    let messages_for_call = project_messages_for_call(&messages);
                    match call_adapter
                        .chat(&messages_for_call, &turn_tools_spec)
                        .await
                    {
                        Ok(r) => break r,
                        Err(AdapterError::ContextOverflow {
                            needed_tokens,
                            limit_tokens,
                        }) => {
                            // Choose the policy arm. After the retry
                            // cap we always drop to Surface, even when
                            // the policy is Compact, to defend against
                            // a wedged model.
                            let effective = if overflow_retries >= MAX_OVERFLOW_RETRIES {
                                ContextOverflowPolicy::Surface
                            } else {
                                self.overflow_policy
                            };
                            match effective {
                                ContextOverflowPolicy::Compact => {
                                    // Snapshot the live context, pick
                                    // items to free the gap (plus a
                                    // small safety margin), and run
                                    // the v60.5 compaction
                                    // orchestrator. A successful
                                    // compaction publishes the
                                    // resolution event then retries
                                    // the turn; a failure surfaces.
                                    let current_total: u32 = context_manager
                                        .lock()
                                        .summarise()
                                        .iter()
                                        .map(|s| s.tokens)
                                        .sum();
                                    let summaries = context_manager.lock().summarise();
                                    let picks = pick_overflow_compaction_targets(
                                        &summaries,
                                        needed_tokens,
                                        limit_tokens,
                                        current_total,
                                    );
                                    if picks.is_empty() {
                                        // No unpinned items to free —
                                        // there's nothing the
                                        // compaction arm can do.
                                        // Surface so the user can
                                        // intervene (unpin, edit, etc).
                                        let _ = try_emit(
                                            &bus,
                                            Event::ContextOverflowResolved {
                                                resolution: "surfaced",
                                                freed_tokens: None,
                                                items_compacted: None,
                                            },
                                        );
                                        return Err(RunError::ContextOverflow {
                                            needed_tokens,
                                            limit_tokens,
                                        });
                                    }
                                    // v60.32 M03 — snapshot the text
                                    // of each picked context item
                                    // before compaction so we can drop
                                    // the matching conversation
                                    // history entries after the
                                    // mutator runs. Without this the
                                    // retry chat call would re-send
                                    // every original message verbatim,
                                    // defeating the compaction.
                                    let picked_texts: Vec<String> = {
                                        let cm = context_manager.lock();
                                        picks
                                            .iter()
                                            .filter_map(|id| {
                                                cm.iter()
                                                    .find(|it| it.id.to_string() == *id)
                                                    .and_then(|it| match &it.payload {
                                                        atelier_core::context::Payload::InlineText { text } => {
                                                            Some(text.clone())
                                                        }
                                                        _ => None,
                                                    })
                                            })
                                            .collect()
                                    };
                                    let now = now_rfc3339();
                                    let sid_str = session_id.0.to_string();
                                    match crate::compaction::compact(
                                        self.adapter.as_ref(),
                                        &session_dispatcher,
                                        &workspace,
                                        &sid_str,
                                        picks.clone(),
                                        &now,
                                        // v60.37 A3 — propagate the same cost policy
                                        // the main loop uses so the compaction ModelCall
                                        // ledger entry is attributed correctly.
                                        self.cost_policy,
                                    )
                                    .await
                                    {
                                        Ok(out) => {
                                            let _ = try_emit(
                                                &bus,
                                                Event::ContextOverflowResolved {
                                                    resolution: "compacted",
                                                    freed_tokens: Some(out.freed_tokens),
                                                    items_compacted: Some(picks.len()),
                                                },
                                            );
                                            // v60.32 M03 — drop the history entries the
                                            // compaction consumed so the next
                                            // `project_messages_for_call` builds a smaller
                                            // payload. We only trim User / Assistant rows
                                            // (the rolling prose history); Tool rows stay
                                            // so a pending tool_use → tool_result pair
                                            // isn't orphaned. The atelier system prompt is
                                            // never a compaction target — the picker
                                            // filters pinned items and the system prompt
                                            // isn't a context item to begin with.
                                            if !picked_texts.is_empty() {
                                                messages.retain(|m| {
                                                    !(matches!(
                                                        m.role,
                                                        Role::User | Role::Assistant
                                                    ) && picked_texts
                                                        .iter()
                                                        .any(|t| t == &m.content))
                                                });
                                            }
                                            overflow_retries += 1;
                                            continue;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "context-overflow auto-compaction failed; surfacing the original overflow"
                                            );
                                            let _ = try_emit(
                                                &bus,
                                                Event::ContextOverflowResolved {
                                                    resolution: "surfaced",
                                                    freed_tokens: None,
                                                    items_compacted: None,
                                                },
                                            );
                                            return Err(RunError::ContextOverflow {
                                                needed_tokens,
                                                limit_tokens,
                                            });
                                        }
                                    }
                                }
                                ContextOverflowPolicy::Reroute => {
                                    // v60.9 stub — the routing-dispatcher
                                    // arm lands in a follow-on bundle.
                                    // Emit the resolution event so a
                                    // subscriber can render
                                    // "reroute requested but unconfigured"
                                    // and surface a typed config error
                                    // so the caller knows the policy
                                    // arm hasn't been wired yet.
                                    let _ = try_emit(
                                        &bus,
                                        Event::ContextOverflowResolved {
                                            resolution: "rerouted",
                                            freed_tokens: None,
                                            items_compacted: None,
                                        },
                                    );
                                    return Err(RunError::Config(
                                        "reroute not yet implemented".into(),
                                    ));
                                }
                                ContextOverflowPolicy::Surface => {
                                    let _ = try_emit(
                                        &bus,
                                        Event::ContextOverflowResolved {
                                            resolution: "surfaced",
                                            freed_tokens: None,
                                            items_compacted: None,
                                        },
                                    );
                                    return Err(RunError::ContextOverflow {
                                        needed_tokens,
                                        limit_tokens,
                                    });
                                }
                            }
                        }
                        Err(e) => return Err(RunError::Adapter(format!("{e}"))),
                    }
                }
            };

            // §1 BYOM (v60.7) — append one `ModelCall` ledger entry
            // per `adapter.chat()` call. `count_source` is the
            // adapter's own honest claim (Exact iff the provider
            // returned `usage`; Unavailable otherwise — see the
            // anthropic / openai_compat assemblers). `cost_usd` is
            // determined by the runner's [`ModelCostPolicy`]:
            // latency-weighted local rate for Mock + OpenAI-compat
            // against a self-hosted server; `None` for cloud
            // providers whose pricing comes from a per-provider
            // table that hasn't shipped yet. `latency_ms` is
            // whatever the adapter measured.
            let model_call_ts = now_rfc3339();
            let latency_f64 = response.usage.latency_ms.map(|ms| ms as f64);
            let cost_usd = match self.cost_policy {
                ModelCostPolicy::LatencyWeighted => latency_f64.map(|ms| {
                    atelier_core::ledger::local_cost_usd(
                        ms,
                        atelier_core::ledger::DEFAULT_LOCAL_RATE_USD_PER_SEC,
                    )
                }),
                ModelCostPolicy::UnknownPending => None,
            };
            session_dispatcher.append_ledger_entry(atelier_core::ledger::LedgerEntry::ModelCall {
                timestamp: model_call_ts,
                model_id: self.adapter.model_id().to_string(),
                prompt_tokens: response.usage.prompt_tokens,
                completion_tokens: response.usage.completion_tokens,
                cached_tokens: response.usage.cached_tokens,
                count_source: response.usage.count_source,
                cost_usd,
                latency_ms: latency_f64,
                // v60.51 §15 — the first `ModelCall` after a slash
                // invocation carries `note: Some("skill: <name>")`;
                // `take()` ensures only that first call is annotated.
                note: pending_skill_note.take(),
            });

            // 6. Parse envelope from response per the *active* strategy
            //    (the §1/§2 conformance tracker may have already
            //    downshifted from the adapter's reported one).
            //
            //    We track whether the parse cleanly produced an envelope
            //    so the cross-call rolling window can drive the §1
            //    degradation check. "Clean" means the strategy-specific
            //    parser returned `Ok`. An empty `tool_calls` payload on
            //    the native-tool path is treated as malformed for
            //    conformance — the carrier did not actually carry an
            //    envelope. Pre-degradation, the adapter's reported
            //    strategy and the active one agree; once degraded, we
            //    use the active value so the parse stays aligned with
            //    the UI badge.
            //
            //    Native tool calls (if any) still take precedence:
            //    the dispatcher executes them and feeds results back as
            //    Role::Tool messages.
            let parse_strategy = active_strategy;
            let (envelope, parse_ok) = match parse_strategy {
                Strategy::NativeTool => match extract_native_envelope(&response.tool_calls) {
                    Some(env) => (env, true),
                    None => (Envelope::default(), false),
                },
                Strategy::JsonSentinel => match parse_json_sentinel(&response.text) {
                    Ok(parsed) => (parsed.envelope, true),
                    Err(_) => (Envelope::default(), false),
                },
                Strategy::RegexProse => {
                    match atelier_core::protocol_strategy::parse_regex_prose(&response.text) {
                        Ok(env) => (env, true),
                        Err(_) => (Envelope::default(), false),
                    }
                }
            };
            // Record the outcome against the *active* strategy so the
            // per-strategy breakdown lines up with what the runner was
            // actually trying to use.
            if parse_ok {
                envelope_conformance.record_success(parse_strategy);
            } else {
                envelope_conformance.record_failure(parse_strategy);
            }
            // §1/§2 conformance-driven degradation. One-way: NativeTool →
            // JsonSentinel → RegexProse. When the rolling window crosses
            // the threshold we walk one step toward the more-tolerant
            // strategy and announce the transition on the bus so the
            // UI badge can refresh. If we are already at the lowest
            // strategy (`RegexProse`), `downshift()` returns `None` and
            // the check no-ops — the §2 escalate-to-user path lives in
            // `TurnConformance` and fires separately.
            if envelope_conformance
                .should_degrade_with(self.degradation_window, self.degradation_threshold)
            {
                if let Some(next) = active_strategy.downshift() {
                    let previous = active_strategy;
                    active_strategy = next;
                    // Clear the window so a freshly-degraded strategy
                    // gets its own evaluation window instead of carrying
                    // the failures from the prior strategy into the new
                    // one's accounting.
                    envelope_conformance =
                        atelier_core::protocol_conformance::ConformanceRingBuffer::new();
                    let reason = format!(
                        "{} of last {} envelope parses malformed",
                        self.degradation_threshold, self.degradation_window,
                    );
                    let _ = try_emit(
                        &bus,
                        Event::StrategyDegraded {
                            from: previous,
                            to: next,
                            reason,
                        },
                    );
                }
            }

            // P5: re-send assistant turn with its tool_calls so multi-turn
            // tool flows round-trip the tool_use ids correctly. Pre-P5 we
            // flattened to text-only, which broke any provider whose
            // protocol requires the prior `tool_use` block to reference
            // its matching `tool_result` (Anthropic, OpenAI, Bedrock,
            // Gemini all do).
            //
            // v25.2-F: keep ALL tool_calls — including the
            // `harness_meta` envelope-bearing call — on the assistant
            // message so the conversation history is complete. The
            // dispatcher only executes the non-envelope ones (filtered
            // below into `real_tool_calls`), but the envelope tool_use
            // id must still appear in history because the next turn
            // (or a future audit) may reference it.
            last_assistant_text = Some(response.text.clone());
            messages.push(Message {
                role: Role::Assistant,
                content: response.text.clone(),
                tool_call_id: None,
                tool_calls: response.tool_calls.clone(),
            });
            context_manager.lock().add(context_item_for_assistant_turn(
                &response.text,
                &now_rfc3339(),
            ));
            let _ = try_emit(
                &bus,
                Event::MessageCommitted {
                    role: MessageRole::Assistant,
                    text: response.text.clone(),
                },
            );
            // Apply the envelope's plan_update (if any) and broadcast a
            // fresh snapshot so the plan pane converges. `apply_envelope`
            // is idempotent — re-applying the same update produces the
            // same canvas — but we still broadcast on every turn so a
            // late-joining subscriber sees something promptly.
            if let Some(plan_update) = &envelope.plan_update {
                let steps = {
                    let mut canvas = plan_canvas.lock();
                    let _report = canvas.apply_envelope(plan_update);
                    canvas.to_vec()
                };
                let _ = try_emit(&bus, Event::PlanSnapshot { steps });
            }
            // v56 — surface the envelope's per-file rationale so the
            // §3 "Why this change?" UI can render it. The bus event
            // carries the same shape as the envelope, flattened to
            // string `kind` so consumers don't import the protocol enum.
            if let Some(claimed) = &envelope.claimed_changes {
                let changes = claimed
                    .iter()
                    .map(|c| atelier_core::session::ClaimedChangeSummary {
                        path: c.path.clone(),
                        // v59 (MED-smell-2 fix) — route through
                        // `ClaimedChangeKind::wire_label` so the
                        // projection stays in sync with the serde
                        // `rename_all = "lowercase"` derive.
                        kind: c.kind.wire_label().to_string(),
                        summary: c.summary.clone(),
                    })
                    .collect();
                let _ = try_emit(&bus, Event::ClaimedChanges { changes });
            }
            let real_tool_calls: Vec<_> = response
                .tool_calls
                .into_iter()
                .filter(|c| c.name != atelier_core::protocol_strategy::HARNESS_META_NAME)
                .collect();
            // v60.15 — capture before the `Vec` is consumed by the
            // dispatch loop below; needed for the end-of-turn stall
            // guard that detects "no tool calls AND no claimed_done".
            let made_tool_calls = !real_tool_calls.is_empty();

            if !real_tool_calls.is_empty() {
                advance(&session_handle, State::Streaming, State::ToolDispatching).await?;
                advance(
                    &session_handle,
                    State::ToolDispatching,
                    State::ToolExecuting,
                )
                .await?;
                // Dispatch all tool calls from this turn concurrently.
                // When the model emits multiple spawn_subagent calls, they
                // run in parallel rather than serially, giving N× throughput
                // on independent sub-agent workloads. Outcomes are processed
                // in the original call order so message history stays
                // deterministic. One ToolContext per call: the dispatcher
                // overrides tool_call_id internally so these are equivalent
                // to the single shared ctx used in the old sequential path.
                let ctxs: Vec<atelier_core::dispatcher::ToolContext<'_>> = real_tool_calls
                    .iter()
                    .map(|_| atelier_core::dispatcher::ToolContext {
                        workspace_root: workspace.as_path(),
                        sandbox: &sandbox,
                        tool_call_id: None,
                        audit_log_path: Some(audit_log_path.as_path()),
                        cancel: session_handle.cancel_token(),
                        deadline: atelier_core::dispatcher::DEFAULT_TOOL_DEADLINE,
                        subagent_depth: self.subagent_depth,
                    })
                    .collect();
                let outcomes = futures::future::join_all(
                    real_tool_calls
                        .iter()
                        .zip(ctxs.iter())
                        .map(|(call, ctx)| session_dispatcher.dispatch(call, ctx, now_rfc3339)),
                )
                .await;

                for (_call, outcome) in real_tool_calls.into_iter().zip(outcomes) {
                    // v60.8 A2 follow-on — harvest per-file EditStaged events.
                    for evt in &outcome.events {
                        if let Event::EditStaged { path, hunks } = evt {
                            let kind = match hunks {
                                atelier_core::diff::Hunks::Created { .. } => {
                                    atelier_core::verify::ObservedKind::Created
                                }
                                atelier_core::diff::Hunks::Deleted { .. } => {
                                    atelier_core::verify::ObservedKind::Deleted
                                }
                                _ => atelier_core::verify::ObservedKind::Modified,
                            };
                            let path_str = path.to_string_lossy().into_owned();
                            observed_changes.retain(|o| o.path != path_str);
                            observed_changes.push(atelier_core::verify::ObservedChange {
                                path: path_str,
                                kind,
                            });
                        }
                    }
                    let result_str = match &outcome.result {
                        Ok(r) => serde_json::to_string(&r.output).unwrap_or_default(),
                        Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
                    };
                    let _ = try_emit(
                        &bus,
                        Event::MessageCommitted {
                            role: MessageRole::Tool,
                            text: result_str.clone(),
                        },
                    );
                    context_manager.lock().add(context_item_for_tool_result(
                        &result_str,
                        &outcome.tool_call_id,
                        &now_rfc3339(),
                    ));
                    messages.push(Message {
                        role: Role::Tool,
                        content: result_str,
                        tool_call_id: Some(outcome.tool_call_id),
                        tool_calls: Vec::new(),
                    });
                }
                advance(&session_handle, State::ToolExecuting, State::Streaming).await?;
                final_state = State::Streaming;
            }

            turns = turn + 1;

            // Per-turn ContextSnapshot + ContextItems (v55).
            //
            // The aggregate `ContextSnapshot` drives the §5 token
            // meter; the per-item `ContextItems` stream feeds the
            // Context panel. They go out together at the same turn
            // boundary so the panel rows and the meter denominator
            // can never disagree.
            //
            // v55 — items now come from the live `ContextManager`
            // populated in parallel with the chat transcript above.
            // Per-item token attribution still uses the same char/4
            // approximation tagged `TokenSource::Approx`; the
            // adapter's `count_tokens` continues to drive the
            // aggregate meter. Pin / unpin / evict from the §5 panel
            // operate on this store; the dispatcher (Step 2) shares
            // the Arc to mutate it from UI handlers.
            if let Ok(token_count) = self.adapter.count_tokens(&messages).await {
                let _ = try_emit(
                    &bus,
                    Event::ContextSnapshot {
                        known_tokens: token_count.count,
                        unknown_tokens: 0,
                    },
                );
            }
            let context_items = context_manager.lock().summarise();
            let _ = try_emit(
                &bus,
                Event::ContextItems {
                    items: context_items,
                },
            );

            // v55 — §5 Memory panel snapshot. The MemoryStore is now
            // mutable from the UI via SessionDispatcher's add /
            // delete / promote mutators (Step 3). The runner still
            // re-emits at each turn boundary so a late-joining
            // subscriber converges to the live state.
            let _ = try_emit(
                &bus,
                Event::MemoryCards {
                    cards: memory_store.lock().summarise(),
                },
            );

            // v60.8 A2 follow-on — stash the most recent envelope for
            // the §7 verify pass below. The envelope is small (no
            // streaming payload, just claims + plan_update) so the
            // clone per turn is cheap; this stays the simplest seam.
            last_envelope = envelope.clone();

            // 8. If the envelope or scripted response says done, exit.
            if envelope.claimed_done == Some(true) {
                advance(&session_handle, State::Streaming, State::Verifying).await?;
                final_state = State::Verifying;
                break;
            }

            // v60.15 (M-bug-stall) — stall guard. A turn that produced
            // neither real tool calls nor `claimed_done=true` leaves
            // the `messages` array ending on an assistant turn. The
            // Anthropic API rejects "conversation ends with assistant"
            // with a 400 `invalid_request_error` on stricter models
            // (Sonnet, Opus); permissive providers (Haiku 4.5) return
            // near-empty completions in a wedge until the turn cap.
            //
            // v60.67 — distinguish two sub-cases:
            //   • Previous message was NOT a tool result: the model had
            //     a user turn to work on but chose neither to call tools
            //     nor claim done. That's a genuine §2 violation → stall.
            //   • Previous message WAS a tool result: the model just
            //     received tool outputs and wrote a prose conclusion
            //     without calling `harness_meta`. This is the common
            //     wrap-up pattern after sub-agent turns. Treat it as an
            //     implicit claimed_done so the conversation completes
            //     naturally rather than blocking with a stall banner.
            if !made_tool_calls {
                // v60.67 — distinguish wrap-up after sub-agents from a true
                // stall. Walk backwards past the Tool block preceding the
                // current Assistant turn; if every tool call in the preceding
                // Assistant turn was `spawn_subagent`, the current prose is
                // the natural task-completion summary — treat it as an
                // implicit claimed_done. Any other tool mix (read_file,
                // list_dir, …) means the model gathered info but didn't
                // follow through, which is a genuine §2 stall.
                let all_prev_were_subagent = {
                    let n = messages.len();
                    // messages[n-1] = current (just-pushed) Assistant.
                    // Walk backwards from n-2 to find the start of the
                    // contiguous Tool block.
                    let mut tool_block_start = n.saturating_sub(1);
                    while tool_block_start > 0 && messages[tool_block_start - 1].role == Role::Tool
                    {
                        tool_block_start -= 1;
                    }
                    // The Assistant turn that issued those tool calls sits at
                    // tool_block_start - 1 (if any Tool messages were found).
                    tool_block_start < n.saturating_sub(1)
                        && tool_block_start > 0
                        && messages[tool_block_start - 1].role == Role::Assistant
                        && messages[tool_block_start - 1].tool_calls.iter().all(|tc| {
                            tc.name == atelier_core::protocol_strategy::SPAWN_SUBAGENT_NAME
                        })
                };
                if all_prev_were_subagent {
                    // All prior tool calls were spawn_subagent: this prose is
                    // the natural task-completion summary after delegation.
                    advance(&session_handle, State::Streaming, State::Verifying).await?;
                    final_state = State::Verifying;
                } else {
                    // True stall: no tool calls and the prior tool block (if
                    // any) was not a sub-agent delegation.
                    let _ = try_emit(
                        &bus,
                        Event::AgentStalled {
                            turn: turn + 1,
                            reason: "assistant turn produced no tool calls and no \
                                     claimed_done=true; conversation cannot advance \
                                     without a §2 protocol violation"
                                .to_string(),
                        },
                    );
                    advance(&session_handle, State::Streaming, State::AwaitingUser).await?;
                    final_state = State::AwaitingUser;
                }
                break;
            }
        }

        // 9. DoD checks. The runner doesn't yet shell out to dod.checks
        //    — that's a follow-on. Until then we report `None`
        //    unconditionally rather than `Some(true)` regardless of dod
        //    presence: the latter is a lie that downstream readers (audit
        //    log, UI badge) would interpret as "all DoD checks passed".
        //    Spec §7 expects this field to mean what it says. Emit a
        //    one-shot warning when a DoD config IS present so the user
        //    knows their checks aren't being honoured.
        if let Some(cfg) = &dod {
            tracing::warn!(
                checks = cfg.checks.len(),
                "DoD config loaded but the runner's check executor is not yet wired; \
                 reporting dod_passed=None. See tasks/todo.md (P4 follow-on)."
            );
        }
        let dod_passed: Option<bool> = None;

        // §10 — flush any lingering in-flight sub-agents before the §7 gate.
        // Synchronous spawn() already awaits each child inline, so in practice
        // the map should be empty here. wait_all is a safety drain for the
        // rare cancel-race where a handle slipped past the normal await path.
        spawner
            .wait_all(&atelier_core::subagents::SubagentId::new())
            .await;

        // 10. Done — transition to terminal and persist.
        if final_state == State::Verifying {
            // v60.8 A2 follow-on — exercise the §7 verify pass. When the
            // run produced either claimed_changes or observed edits,
            // fire `verify_pass` so the bus carries the Tier 3 textual
            // outcome and the GUI/TUI verify-pass badge converges off
            // its `NotRun` default. When neither side has anything to
            // weigh, the explicit `emit_verify_not_run` keeps the badge
            // at `NotRun` rather than letting it drift to a prior
            // turn's tier — both arms emit exactly one
            // `Event::VerificationPassed` so consumers can rely on
            // the per-run terminal-marker contract.
            let has_claims = last_envelope
                .claimed_changes
                .as_ref()
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            if has_claims || !observed_changes.is_empty() {
                // U09 — §7 Tier-1 live LSP path.
                //
                // Priority (highest to lowest):
                //   1. Test seam (tier1_diagnostics_for_test) — bypasses the
                //      launcher entirely, used by the hallucinating-agent gate.
                //   2. Live LSP receiver — fires when `.ts` files are in the
                //      change set AND `LspApprovals` says "typescript" is approved.
                //   3. Tier-3 textual verify_pass — fallback.
                let tier1_discrepancies: Vec<atelier_core::verify::Discrepancy> = if !self
                    .tier1_diagnostics_for_test
                    .is_empty()
                {
                    // Test seam: pre-mapped discrepancies bypass the launcher.
                    self.tier1_diagnostics_for_test.clone()
                } else {
                    // Live path: detect TypeScript files in the change set.
                    let ts_paths: Vec<String> = observed_changes
                        .iter()
                        .filter(|o| o.path.ends_with(".ts") || o.path.ends_with(".tsx"))
                        .map(|o| o.path.clone())
                        .collect();

                    if ts_paths.is_empty() {
                        // No TypeScript files — skip LSP entirely.
                        Vec::new()
                    } else {
                        // Load approvals for this workspace.
                        let approvals_path = atelier_core::lsp::lsp_approvals_path(&workspace);
                        let approvals = atelier_core::lsp::LspApprovals::load(&approvals_path)
                            .unwrap_or_default();

                        if !approvals.is_approved("typescript") {
                            // Not approved yet — emit the install prompt so
                            // the GUI/TUI can ask the user. Fall through to
                            // Tier-3 textual for this run.
                            let _ = try_emit(
                                &bus,
                                Event::RequestLspInstall {
                                    language: "typescript".into(),
                                    candidate_packages: vec!["typescript-language-server".into()],
                                },
                            );
                            Vec::new()
                        } else {
                            // Approved — spawn the LSP server and gather
                            // diagnostics for all modified `.ts` files.
                            match atelier_core::lsp::LspLauncher::spawn(
                                &workspace, &sandbox, &approvals,
                            )
                            .await
                            {
                                Ok(mut session) => {
                                    // Open each TypeScript file.
                                    for rel_path in &ts_paths {
                                        let abs_path = workspace.join(rel_path);
                                        if let Ok(content) = std::fs::read_to_string(&abs_path) {
                                            let _ = session.open_file(&abs_path, &content);
                                        }
                                    }
                                    // Collect diagnostics (10-second budget).
                                    let raw = session
                                        .collect_diagnostics(std::time::Duration::from_secs(10))
                                        .await;
                                    session.shutdown().await;
                                    // Map raw diagnostics to discrepancies.
                                    raw.into_iter()
                                        .filter_map(|(abs_path, diag)| {
                                            // Rebase absolute path to
                                            // workspace-relative.
                                            let rel = abs_path
                                                .strip_prefix(workspace.to_string_lossy().as_ref())
                                                .unwrap_or(abs_path.as_str())
                                                .trim_start_matches('/')
                                                .to_string();
                                            atelier_core::lsp::map_diagnostic_to_discrepancy(
                                                &rel, &diag,
                                            )
                                        })
                                        .collect()
                                }
                                Err(atelier_core::lsp::LspLaunchError::NotApproved { .. }) => {
                                    // Should not happen (we checked above), but
                                    // belt-and-suspenders: emit the install
                                    // prompt and fall through.
                                    let _ = try_emit(
                                        &bus,
                                        Event::RequestLspInstall {
                                            language: "typescript".into(),
                                            candidate_packages: vec![
                                                "typescript-language-server".into()
                                            ],
                                        },
                                    );
                                    Vec::new()
                                }
                                Err(e) => {
                                    // Non-approval launch error: warn + fall
                                    // through to Tier 3.
                                    tracing::warn!(
                                        error = %e,
                                        "LSP launcher failed; falling through to Tier-3 verify"
                                    );
                                    Vec::new()
                                }
                            }
                        }
                    }
                };

                if tier1_discrepancies.is_empty() && self.tier1_diagnostics_for_test.is_empty() {
                    let _ = session_dispatcher.verify_pass(&last_envelope, &observed_changes);
                } else {
                    let _ = session_dispatcher.verify_pass_with_tier1(
                        &last_envelope,
                        &observed_changes,
                        tier1_discrepancies,
                    );
                }
            } else {
                session_dispatcher.emit_verify_not_run();
            }
            advance(&session_handle, State::Verifying, State::Done).await?;
            final_state = State::Done;
        }

        // 11. Persist session. Best-effort — failure here logs but doesn't
        //     fail the run, because the in-memory state is what the user
        //     cares about for `atelier run`. (The actor's checkpoint hook
        //     is where every-transition persistence would land; this is
        //     the end-of-run snapshot.)
        //
        //     v61 — write the full conversation back so `--resume` has
        //     something to reconstruct from. When resuming, we keep the
        //     prior `session_uuid` so the on-disk path is stable; fresh
        //     runs use the actor's session id.
        let persist_uuid = self.resume_from.unwrap_or(session_id.0);
        let session_dir = OnDiskSession::session_dir(&workspace, persist_uuid);
        let mut snapshot = OnDiskSession::fresh(
            persist_uuid,
            env!("CARGO_PKG_VERSION").to_string(),
            now_rfc3339(),
        );
        // Re-attach the resumed recovery_log so its audit trail
        // survives the round-trip. Fresh runs get an empty log.
        if let Some(resumed) = &resumed_session {
            snapshot.recovery_log = resumed.recovery_log.clone();
        }
        // Materialise the in-memory `messages` into the persisted
        // conversation field. Turn ids are session-local positional —
        // sufficient for the resume protocol's ordering invariants;
        // a future audit-grade format will carry stable per-call ids.
        for (idx, msg) in messages.iter().enumerate() {
            let turn_id = format!("turn-{idx}");
            let role_str = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
            };
            let tool_calls_json: Vec<serde_json::Value> = msg
                .tool_calls
                .iter()
                .map(|tc| {
                    serde_json::json!({
                        "tool_call_id": tc.id,
                        "tool_name": tc.name,
                        "args": tc.arguments,
                    })
                })
                .collect();
            snapshot.append_conversation_turn(
                turn_id,
                role_str,
                msg.content.clone(),
                msg.tool_call_id.clone(),
                tool_calls_json,
            );
        }
        // §10 — persist completed sub-agent records into session.json.
        for rec in spawner.drain_completed() {
            let status_str = match rec.result.status {
                SubagentStatus::Completed => "completed",
                SubagentStatus::Failed => "failed",
                SubagentStatus::TimedOut => "timed_out",
                SubagentStatus::Cancelled => "cancelled",
            };
            let cost_summary = if rec.result.cost.prompt_tokens > 0
                || rec.result.cost.completion_tokens > 0
                || rec.result.cost.cost_usd.is_some()
            {
                Some(PersistedSubagentCost {
                    prompt_tokens: rec.result.cost.prompt_tokens,
                    completion_tokens: rec.result.cost.completion_tokens,
                    cached_tokens: rec.result.cost.cached_tokens,
                    cost_usd: rec.result.cost.cost_usd,
                })
            } else {
                None
            };
            snapshot.subagents.insert(
                rec.result.id.to_string(),
                PersistedSubagent {
                    subagent_type: Some(rec.subagent_type_name),
                    description: Some(rec.description),
                    started_at: Some(rec.started_at),
                    finished_at: Some(rec.finished_at),
                    status: status_str.to_string(),
                    result: Some(rec.result.result),
                    max_turns: Some(rec.max_turns),
                    turns_used: Some(rec.result.turns_used),
                    cost_summary,
                },
            );
        }
        // R-1: carry forward completed sub-agents from the prior session so
        // the subagents map is additive across resumes.
        if let Some(resumed) = &resumed_session {
            for (id, rec) in &resumed.subagents {
                snapshot
                    .subagents
                    .entry(id.clone())
                    .or_insert_with(|| rec.clone());
            }
            // Mark any sub-agent that was `running` in the prior session as
            // cancelled — v1 does not resume in-flight sub-agent runs (§10).
            for rec in snapshot.subagents.values_mut() {
                if rec.status == "running" {
                    rec.status = "cancelled".to_string();
                    let _ = try_emit(
                        &bus,
                        Event::SubagentCancelled {
                            id: rec.description.clone().unwrap_or_default(),
                            reason: "resume_inflight".to_string(),
                        },
                    );
                }
            }
        }

        if let Err(e) = snapshot.save_to(&session_dir) {
            tracing::warn!(error = %e, "atelier run: session snapshot save failed");
        }

        // Phase C close — write the pane-visibility record next to
        // `session.json` if the driver supplied one. Best-effort: a
        // failure here logs but does not fail the run (the spec
        // measurement subsystem reads this file lazily and falls
        // back to "all visible" when it's absent).
        if let Some((panes, driver)) = &self.pane_visibility {
            let rec = crate::instrumentation::PaneVisibilityRecord::new(
                session_id.0.to_string(),
                now_rfc3339(),
                panes.clone(),
                driver.clone(),
            );
            if let Err(e) = rec.save_to(&session_dir) {
                tracing::warn!(error = %e, "pane_visibility.json write failed");
            }
        }

        // 12. Shutdown. The broadcast channel only closes when *every*
        //     Sender clone drops, and SessionDispatcher holds one
        //     (cloned from session_handle.events_sender()). Drop them in
        //     order: send Shutdown command, drop the dispatcher's
        //     Sender clone, drop the handle's Sender clone, then the
        //     sink's rx.recv() sees Closed and the drain task exits.
        let _ = session_handle.send(SessionCommand::Shutdown).await;
        // The DispatcherHandleGuard (created above) clears the slot
        // on every exit path; nothing extra to do here.
        drop(session_dispatcher);
        drop(file_watcher_handle);
        drop(session_handle);
        // v61 — the concurrent-edit resolver task exits when the
        // broadcast channel closes. Abort it for promptness so the
        // 5-min pause timer doesn't keep the runtime alive after a
        // crash.
        auto_reload_task.abort();
        // Safety belt: if a future regression keeps a Sender alive, the
        // drain task would block forever; bound the wait so the runner
        // doesn't hang the test or CLI process.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sink_handle).await;

        // Phase B Track A — snapshot the conformance ring buffer at
        // end-of-run so test callers (and the nightly gate) can fold
        // per-strategy summaries without reaching into the runner's
        // internals. Cheap: the snapshot allocates a small Vec.
        let envelope_conformance = envelope_conformance.snapshot();

        let ledger_snapshot = ledger.to_vec();
        Ok(RunReport {
            session_id,
            turns,
            turns_used: turns,
            final_state,
            dod_passed,
            envelope_conformance,
            final_assistant_text: last_assistant_text,
            ledger_entries: ledger_snapshot,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("config: {0}")]
    Config(String),
    #[error("adapter: {0}")]
    Adapter(String),
    #[error("session command failed: {0}")]
    Session(String),
    /// §1 BYOM — typed surface for the
    /// [`ContextOverflowPolicy::Surface`] arm (and the
    /// defence-in-depth Surface fallback after a Compact retry also
    /// overflowed). Carries the same `needed` / `limit` token counts
    /// the adapter extracted from the provider error body so the
    /// caller can show an actionable message rather than "adapter:
    /// context overflow: …".
    #[error("context overflow: needed {needed_tokens} tokens, model accepts {limit_tokens}")]
    ContextOverflow {
        needed_tokens: u32,
        limit_tokens: u32,
    },
    /// v60.51 §15 — the prompt began with `/` but no skill of that
    /// name is registered. Caller surfaces the available names so the
    /// user can fix the typo.
    #[error("unknown skill `/{name}`; available: {}", available.join(", "))]
    SkillUnknown {
        name: String,
        available: Vec<String>,
    },
    /// v60.51 §15 — the skill exists but expansion failed (a required
    /// arg was missing, or `prompt_template` referenced an unknown
    /// `${variable}`). The wrapped error names the specific cause.
    #[error("skill `/{name}`: {source}")]
    SkillSubstitution {
        name: String,
        #[source]
        source: atelier_core::skills::SubstitutionError,
    },
}

/// v60.17 §2 — atelier-flavoured system prompt. The model needs to know
/// two things the spec doesn't currently teach via tool-spec alone:
/// 1. The workspace root and how to express paths (repo-relative).
/// 2. That signalling completion happens through the `harness_meta` tool
///    (or, under degraded strategies, the matching carrier).
///
/// Strategy-aware wording: under [`Strategy::NativeTool`] the model is
/// pointed at the `harness_meta` tool by name; under sentinel/prose
/// strategies the carrier shape is described in plain English.
fn build_atelier_system_prompt(
    workspace: &Path,
    strategy: atelier_core::protocol_strategy::Strategy,
) -> String {
    use atelier_core::protocol_strategy::Strategy;
    let workspace_display = workspace.display();
    let completion_clause = match strategy {
        Strategy::NativeTool => {
            "When you finish the user's task, you MUST invoke the `harness_meta` \
             tool with `claimed_done: true` and a `claimed_changes` array listing \
             every file you created, edited, or deleted. The harness consumes \
             that envelope to recognise completion; without it the loop keeps \
             running and burns tokens. Invoke `harness_meta` on the same turn \
             you communicate completion in prose.\n\
             \n\
             If you believe the task is complete but couldn't fully verify (for \
             example, the sandbox blocked `pytest`, `getcwd` printed a \
             `shell-init` warning, or a check tool wasn't available), STILL emit \
             `harness_meta` with `claimed_done: true`. Add an `uncertainty` entry \
             describing what you couldn't verify. The harness's §7 verifier will \
             catch any inconsistency; do NOT keep iterating because you couldn't \
             reach pytest-green on your own."
        }
        Strategy::JsonSentinel => {
            "When you finish the user's task, append a §2 protocol envelope to \
             your final reply, bracketed exactly as `<<<harness_meta>>>{...}<<<end>>>`. \
             The envelope is a JSON object with `claimed_done: true` and a \
             `claimed_changes` array listing every file you created, edited, or \
             deleted. The harness consumes the envelope; the user sees only your \
             prose. If you couldn't verify (sandbox / missing tool), still emit \
             `claimed_done: true` and use an `uncertainty` entry to flag it — \
             do not silently iterate."
        }
        Strategy::RegexProse => {
            "When you finish the user's task, end your reply with tagged sections: \
             `DONE: yes` on its own line, followed by `CHANGED-FILES:` and a \
             newline-separated list of paths you created, edited, or deleted. \
             Use `UNCERTAINTY:` if you couldn't verify (sandbox / missing tool) \
             but still emit `DONE: yes`. The harness consumes those tags; the \
             user sees only your prose."
        }
    };
    format!(
        "You are an autonomous coding agent running inside the Atelier harness.\n\
         \n\
         Workspace root: {workspace_display}\n\
         All file paths you pass to tools (read_file, write_file, edit_file, …) \
         must be repo-relative (no leading `/`, no `..`). The shell tool runs \
         with the workspace as cwd.\n\
         \n\
         {completion_clause}\n\
         \n\
         Be concise. Use tools to make changes and verify them; do not ask the \
         user for confirmation between steps."
    )
}

/// v60.7 §1 — does this OpenAI-compat base URL point at the hosted
/// OpenAI service? Used to discriminate cloud OpenAI (pricing via a
/// future per-provider table; `cost_usd = None` for now) from local
/// OpenAI-compatible servers (LM Studio / llama-server / vLLM /
/// Ollama / sglang — latency-weighted local rate).
///
/// Match is permissive — both `https://api.openai.com/v1` and any
/// future `/v2` / regional variant under the same host counts as
/// cloud. A custom proxy in front of OpenAI must be self-declared
/// as local (by using a non-`api.openai.com` host).
fn is_openai_cloud_base_url(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    lower.starts_with("https://api.openai.com") || lower.starts_with("http://api.openai.com")
}

/// Lift an `AdapterError` raised at construction time (so far only
/// `Anthropic::from_env`) into a `RunError`. Missing credentials are
/// `Config` so the binary can hint at remediation; everything else is
/// `Adapter` so it surfaces as a transient adapter issue.
fn adapter_to_run_error(e: AdapterError) -> RunError {
    match e {
        AdapterError::NotConfigured(m) => RunError::Config(m),
        other => RunError::Adapter(other.to_string()),
    }
}

#[allow(dead_code)]
fn built_in_registry() -> Result<ToolRegistry, RunError> {
    let mut r = ToolRegistry::new();
    // deps=None: spawn_subagent is wired separately when a RunnerSpawner is
    // available; the root runner supplies deps through built_in_registry_with_deps().
    register_builtins(&mut r, None).map_err(|e| RunError::Config(format!("tool registry: {e}")))?;
    Ok(r)
}

pub(crate) fn built_in_registry_with_deps(
    deps: atelier_core::tools::BuiltinDeps,
) -> Result<ToolRegistry, RunError> {
    let mut r = ToolRegistry::new();
    register_builtins(&mut r, Some(deps))
        .map_err(|e| RunError::Config(format!("tool registry: {e}")))?;
    Ok(r)
}

/// Pull the §2 envelope out of a native-tool response. The `harness_meta`
/// tool-call's arguments ARE the envelope; everything else is a real tool
/// call to dispatch.
///
/// v57 (M-bug-1 fix) — parse failures used to be swallowed via `.ok()`,
/// which manifested as a model that "said it was done" but kept
/// looping until `max_turns` (no `claimed_done` reached the run loop).
/// Log via `tracing::warn` so the failure is visible in any harness
/// running with `RUST_LOG=warn` or above.
fn extract_native_envelope(calls: &[ToolCallRequest]) -> Option<Envelope> {
    for c in calls {
        if c.name == atelier_core::protocol_strategy::HARNESS_META_NAME {
            match parse_native_tool(&NativeToolCall {
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            }) {
                Ok(env) => return Some(env),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "envelope: malformed harness_meta payload — treating as no envelope (claimed_done / plan_update / claimed_changes silently dropped)"
                    );
                    return None;
                }
            }
        }
    }
    None
}

/// v61 — concurrent-edit resolver. Subscribes to the session bus and
/// either auto-acknowledges (`AutoReload` policy, no human in the loop)
/// or starts a 5-minute auto-pause timer per spec §14 (`Modal` policy,
/// fires `FilesChangedAcknowledged { outcome: PauseTimedOut }` if the
/// user doesn't intervene).
///
/// Returns a `JoinHandle` so the caller can drop the task at session
/// teardown. The task exits naturally when the broadcast channel closes
/// (`session_handle` dropped).
fn spawn_concurrent_edit_resolver(
    bus: tokio::sync::broadcast::Sender<Event>,
    policy: ConcurrentEditPolicy,
    pause_timeout: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            // Wait for a FilesChanged event (skip others).
            let next = rx.recv().await;
            match next {
                Ok(Event::FilesChanged { .. }) => match policy {
                    ConcurrentEditPolicy::AutoReload => {
                        // Headless: auto-resolve immediately.
                        let _ = try_emit(
                            &bus,
                            Event::FilesChangedAcknowledged {
                                outcome: ConcurrentEditOutcome::AutoReload,
                            },
                        );
                    }
                    ConcurrentEditPolicy::Modal => {
                        // Interactive: start the 5-minute auto-pause
                        // timer. Cancel it if a user-driven
                        // FilesChangedAcknowledged arrives first.
                        let mut local_rx = bus.subscribe();
                        let timer = tokio::time::sleep(pause_timeout);
                        tokio::pin!(timer);
                        loop {
                            tokio::select! {
                                _ = &mut timer => {
                                    let _ = try_emit(
                                        &bus,
                                        Event::FilesChangedAcknowledged {
                                            outcome: ConcurrentEditOutcome::PauseTimedOut,
                                        },
                                    );
                                    break;
                                }
                                ev = local_rx.recv() => match ev {
                                    Ok(Event::FilesChangedAcknowledged { .. }) => {
                                        // User (or auto-arm) resolved
                                        // the modal — stand down the
                                        // timer.
                                        break;
                                    }
                                    Ok(Event::Shutdown) => return,
                                    Err(_) => return,
                                    _ => continue,
                                },
                            }
                        }
                    }
                },
                Ok(Event::Shutdown) => return,
                Err(_) => return,
                _ => continue,
            }
        }
    })
}

/// v61 — reverse of the JSON shape `OnDiskSession::append_conversation_turn`
/// writes into `tool_calls[]`. Returns `None` for malformed entries; the
/// caller skips them.
fn reconstruct_tool_call_request(v: &serde_json::Value) -> Option<ToolCallRequest> {
    let id = v.get("tool_call_id")?.as_str()?.to_string();
    let name = v.get("tool_name")?.as_str()?.to_string();
    let arguments = v.get("args").cloned().unwrap_or(serde_json::Value::Null);
    Some(ToolCallRequest {
        id,
        name,
        arguments,
    })
}

async fn advance(handle: &SessionHandle, _from: State, to: State) -> Result<(), RunError> {
    // We don't strictly need `from` — the actor enforces legality against
    // its own current state via `Transition::new`. Carried as a doc hint.
    let _ = Transition::new(_from, to); // compile-time check the edge is legal
    handle
        .send(SessionCommand::Advance(to))
        .await
        .map_err(|e| RunError::Session(format!("send Advance({to}): {e}")))
}

/// v55 — char/4 approximation of message tokens, mirroring the
/// `count_tokens` adapter fallback so per-item counts stay coherent
/// with the aggregate meter the adapter reports.
fn approx_tokens(s: &str) -> u32 {
    let count = s.chars().count() / 4;
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn context_item_for_user_prompt(text: &str, now: &str) -> ContextItem {
    ContextItem {
        id: ContextItemId::new(),
        payload: Payload::InlineText {
            text: text.to_string(),
        },
        tokens: TokenCount {
            count: approx_tokens(text),
            source: TokenSource::Approx,
        },
        provenance: Provenance::UserAttached { note: None },
        pinned: false,
        added_at: now.to_string(),
        last_used: now.to_string(),
    }
}

fn context_item_for_assistant_turn(text: &str, now: &str) -> ContextItem {
    ContextItem {
        id: ContextItemId::new(),
        payload: Payload::InlineText {
            text: text.to_string(),
        },
        tokens: TokenCount {
            count: approx_tokens(text),
            source: TokenSource::Approx,
        },
        provenance: Provenance::AssistantTurn,
        pinned: false,
        added_at: now.to_string(),
        last_used: now.to_string(),
    }
}

fn context_item_for_tool_result(text: &str, tool_call_id: &str, now: &str) -> ContextItem {
    ContextItem {
        id: ContextItemId::new(),
        payload: Payload::InlineText {
            text: text.to_string(),
        },
        tokens: TokenCount {
            count: approx_tokens(text),
            source: TokenSource::Approx,
        },
        provenance: Provenance::ToolResult {
            tool_call_id: tool_call_id.to_string(),
        },
        pinned: false,
        added_at: now.to_string(),
        last_used: now.to_string(),
    }
}

// v57 (H6 fix): `now_rfc3339` lifted into `atelier_core::time::now_rfc3339`.
// Local re-export keeps the call sites in this module short.
use atelier_core::time::now_rfc3339;

/// §1 BYOM — auto-selector for [`ContextOverflowPolicy::Compact`].
///
/// Heuristic (deterministic, pure):
///   1. Compute the target number of tokens to free, per the spec
///      §1 sketch: `target = needed - (limit - current_total)`. With
///      saturating arithmetic this is the gap by which the next call
///      would overshoot the model's reported limit. A 0/negative gap
///      doesn't mean "nothing to do" — the *adapter* already said
///      we overflowed, so the harness must free *some* tokens
///      regardless of the local estimate. We therefore floor the
///      target at the smallest unpinned candidate's token count
///      whenever the saturating delta lands at zero.
///   2. Pad `target` by [`OVERFLOW_SAFETY_MARGIN_PCT`] so the
///      freshly-pinned summary card (the v60.5 compaction always
///      reads one back into the window) plus a small slop budget
///      don't put us right back over the line on the retry.
///   3. Filter the live `summarise()` projection to unpinned items
///      only. Pinned items are user-asserted load-bearing — touching
///      them silently would surprise the user. If everything is
///      pinned the selector returns empty; the caller then surfaces
///      the overflow.
///   4. Sort the candidates by token count **descending**. Largest
///      items free the most tokens per compaction blob entry — fewer
///      items round-tripped through the summary call means cheaper
///      recovery.
///   5. Greedily accumulate until the running sum covers the padded
///      target (or we run out of unpinned candidates).
///   6. Return the chosen ids. Empty vec means "nothing to compact"
///      (everything is pinned) and the caller surfaces the original
///      overflow.
///
/// Pure function: takes the snapshot + the three counters and returns
/// a `Vec<String>` of context-item ids. Unit-tested directly so the
/// heuristic stays inspectable without a live `Runner`.
fn pick_overflow_compaction_targets(
    summaries: &[atelier_core::context::ContextItemSummary],
    needed_tokens: u32,
    limit_tokens: u32,
    current_total: u32,
) -> Vec<String> {
    // Step 3 hoisted: unpinned candidates only. If none, the selector
    // can't act.
    let mut candidates: Vec<&atelier_core::context::ContextItemSummary> =
        summaries.iter().filter(|s| !s.pinned).collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    // Step 4: token-count-descending. Stable sort keeps insertion
    // order as the tiebreaker so the test fixture stays predictable.
    candidates.sort_by(|a, b| b.tokens.cmp(&a.tokens));

    // Step 1: spec gap. Saturating arithmetic guards against an
    // adapter that mis-reports `needed < limit + current_total`.
    let headroom = limit_tokens.saturating_sub(current_total);
    let raw_target = needed_tokens.saturating_sub(headroom);

    // Floor: the adapter raised overflow, so we must free at least
    // one item's worth of tokens even when the local estimate says
    // we had headroom. Pick the smallest unpinned candidate's count
    // as the floor so a tiny prompt doesn't force eviction of a
    // multi-thousand-token item.
    let min_unpinned = candidates
        .iter()
        .map(|c| c.tokens)
        .min()
        .unwrap_or(1)
        .max(1);
    let effective_target = raw_target.max(min_unpinned);

    // Step 2: pad by the safety margin. `+25%` makes a near-miss
    // unlikely to immediately re-overflow on the retry; saturating
    // to u32::MAX is fine because the loop terminates on the
    // candidates running out anyway.
    let padded_target = effective_target
        .saturating_add(effective_target.saturating_mul(OVERFLOW_SAFETY_MARGIN_PCT) / 100);

    // Step 5: greedy accumulate.
    let mut chosen = Vec::new();
    let mut freed: u32 = 0;
    for c in candidates {
        if freed >= padded_target {
            break;
        }
        chosen.push(c.id.clone());
        freed = freed.saturating_add(c.tokens);
    }
    chosen
}

/// v60.32 M01 — pure resolution of the OpenAI-compat base url.
///
/// Documented precedence: CLI flag > profile entry > `OPENAI_BASE_URL`
/// env > built-in default. The CLI and profile fold into the
/// `from_cli_or_profile` argument upstream (`resolve_provider_choice`
/// in `main.rs`). Returning the source label lets the caller log
/// which layer won.
fn resolve_openai_base_url(
    from_cli_or_profile: Option<String>,
    from_env: Option<String>,
) -> (String, &'static str) {
    if let Some(v) = from_cli_or_profile {
        (v, "cli_or_profile")
    } else if let Some(v) = from_env {
        (v, "env")
    } else {
        ("https://api.openai.com/v1".to_string(), "default")
    }
}

fn build_mock_adapter(responses: Vec<MockResponse>) -> MockAdapter {
    let m = MockAdapter::new("mock:run");
    for r in responses {
        use atelier_core::adapter::{ChatResponse, StreamChunk, Usage};
        use atelier_core::context::TokenSource;
        if let Some((needed, limit)) = r.overflow {
            // §1 BYOM — drive the ContextOverflow recovery path.
            // A single Error chunk short-circuits the stream
            // assembler in `Adapter::chat`'s default impl.
            m.queue_stream(vec![StreamChunk::Error {
                error: AdapterError::ContextOverflow {
                    needed_tokens: needed,
                    limit_tokens: limit,
                },
            }]);
            continue;
        }
        m.queue_stream(vec![
            StreamChunk::Text {
                delta: r.assistant_text.clone(),
            },
            StreamChunk::Complete {
                response: ChatResponse {
                    text: r.assistant_text,
                    stop_reason: Some(if !r.tool_calls.is_empty() {
                        atelier_core::adapter::StopReason::ToolUse
                    } else {
                        atelier_core::adapter::StopReason::EndTurn
                    }),
                    tool_calls: r.tool_calls,
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        cached_tokens: None,
                        count_source: TokenSource::Approx,
                        latency_ms: Some(0),
                    },
                    // MockResponse only exercises the native-tool path —
                    // the envelope rides in `tool_calls` and the runner's
                    // `extract_native_envelope` picks it up. Other
                    // strategies are exercised by the strategy-level unit
                    // tests in atelier-core, not by this end-to-end mock.
                    strategy: Strategy::NativeTool,
                },
            },
        ]);
    }
    m
}

fn spawn_sink_drain(
    sink: &EventSink,
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
) -> tokio::task::JoinHandle<()> {
    match sink {
        EventSink::Stdout => {
            let mut rx = rx.resubscribe();
            tokio::spawn(async move {
                // Lock stdout per-line, not across the await — StdoutLock
                // isn't Send and would refuse to compile in a tokio task.
                while let Ok(ev) = rx.recv().await {
                    let stdout = io::stdout();
                    let mut handle = stdout.lock();
                    let _ = writeln!(handle, "{ev:?}");
                    let _ = handle.flush();
                }
            })
        }
        EventSink::Capture(buf) => {
            let buf = buf.clone();
            let mut rx = rx.resubscribe();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    buf.lock().push(ev);
                }
            })
        }
        EventSink::Null => {
            let mut rx = rx.resubscribe();
            tokio::spawn(async move { while rx.recv().await.is_ok() {} })
        }
        EventSink::Callback(cb) => {
            let cb = cb.clone();
            let mut rx = rx.resubscribe();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    cb(&ev);
                }
            })
        }
    }
}

/// Convenience used by the binary to translate a `--prompt-file` path into
/// the prompt string. Centralised so the binary stays small.
/// `allow(dead_code)` because the integration-test build of this module
/// doesn't call it — only the binary does. See the EventSink comment for
/// the dual-build context.
// v49: binary-only helper. Stays `pub` because the [[bin]] target is
// a separate crate from the library and `pub(crate)` would hide it.
// The lib's doc warns consumers off `atelier_cli::runner::*`; this
// is the one item there that's strictly binary-internal.
#[allow(dead_code)]
pub fn read_prompt(path: Option<&Path>) -> io::Result<String> {
    match path {
        Some(p) => std::fs::read_to_string(p),
        None => {
            let mut s = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut s)?;
            Ok(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // v60.32 M01 — pin the documented `CLI > profile > env > default`
    // precedence for the OpenAI-compat base url. The CLI and profile
    // fold into the first argument upstream in `resolve_provider_choice`;
    // the per-test scenarios below name the layer for clarity.
    #[test]
    fn base_url_cli_or_profile_wins_over_env_and_default() {
        let (v, src) = resolve_openai_base_url(
            Some("http://localhost:11434/v1".to_string()),
            Some("http://env-override:9999/v1".to_string()),
        );
        assert_eq!(v, "http://localhost:11434/v1");
        assert_eq!(src, "cli_or_profile");
    }

    #[test]
    fn base_url_env_wins_over_default_when_cli_and_profile_absent() {
        let (v, src) = resolve_openai_base_url(None, Some("http://env-host:8080/v1".to_string()));
        assert_eq!(v, "http://env-host:8080/v1");
        assert_eq!(src, "env");
    }

    #[test]
    fn base_url_falls_back_to_default_when_all_layers_absent() {
        let (v, src) = resolve_openai_base_url(None, None);
        assert_eq!(v, "https://api.openai.com/v1");
        assert_eq!(src, "default");
    }

    #[test]
    fn user_prompt_item_uses_inline_text_and_user_attached_provenance() {
        let item = context_item_for_user_prompt("hello world", "2026-05-17T10:00:00Z");
        assert!(matches!(item.payload, Payload::InlineText { ref text } if text == "hello world"));
        assert!(matches!(
            item.provenance,
            Provenance::UserAttached { note: None }
        ));
        assert_eq!(item.tokens.source, TokenSource::Approx);
        // chars/4 floor: "hello world" (11 chars) -> 2.
        assert_eq!(item.tokens.count, 2);
        assert!(!item.pinned);
        assert_eq!(item.added_at, "2026-05-17T10:00:00Z");
    }

    #[test]
    fn assistant_turn_item_uses_assistant_turn_provenance() {
        let item = context_item_for_assistant_turn("ok I'll start", "2026-05-17T10:00:00Z");
        assert!(matches!(item.provenance, Provenance::AssistantTurn));
    }

    #[test]
    fn tool_result_item_carries_tool_call_id() {
        let item = context_item_for_tool_result("file contents…", "tc-1", "2026-05-17T10:00:00Z");
        match item.provenance {
            Provenance::ToolResult { tool_call_id } => assert_eq!(tool_call_id, "tc-1"),
            other => panic!("expected ToolResult provenance, got {other:?}"),
        }
    }

    #[test]
    fn approx_tokens_caps_floor_to_chars_div_four() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abc"), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("0123456789"), 2);
    }

    // ---- v60.7 §1 BYOM: cost-policy URL discriminator ----

    #[test]
    fn cloud_openai_base_urls_are_classified_unknown_pending() {
        // Canonical hosted OpenAI endpoint and case variants must
        // resolve to `UnknownPending` so the runner doesn't apply
        // the local latency-weighted rate to a cloud bill.
        for url in [
            "https://api.openai.com/v1",
            "https://API.openai.com/v1",
            "http://api.openai.com",
        ] {
            assert!(
                is_openai_cloud_base_url(url),
                "{url} should be classified as cloud OpenAI"
            );
        }
    }

    #[test]
    fn local_base_urls_are_classified_latency_weighted() {
        // Self-hosted OpenAI-compatible servers — Ollama, LM
        // Studio, llama-server, vLLM, sglang. None of these
        // should be classified as cloud OpenAI; the runner uses
        // the latency-weighted local rate for all of them.
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://localhost:8080",
            "https://my-vllm.internal:8000/v1",
        ] {
            assert!(
                !is_openai_cloud_base_url(url),
                "{url} should be classified as local"
            );
        }
    }

    // ---- §1 BYOM: ContextOverflowPolicy + auto-selector heuristic ----

    fn summary_fixture(
        id: &str,
        tokens: u32,
        pinned: bool,
    ) -> atelier_core::context::ContextItemSummary {
        atelier_core::context::ContextItemSummary {
            id: id.to_string(),
            kind: "inline_text".into(),
            label: format!("item-{id}"),
            provenance: "user_attached".into(),
            provenance_detail: None,
            tokens,
            token_source: "approx".into(),
            pinned,
        }
    }

    #[test]
    fn overflow_policy_default_is_compact() {
        // The default must stay `Compact` — that's the user-visible
        // contract; flipping it would silently change recovery
        // behaviour for every existing caller.
        let workspace = std::path::PathBuf::from("/tmp");
        let runner = Runner::new(
            workspace,
            ProviderChoice::Mock { responses: vec![] },
            EventSink::Null,
        )
        .expect("mock runner construction is infallible");
        assert_eq!(runner.overflow_policy, ContextOverflowPolicy::Compact);
    }

    #[test]
    fn with_overflow_policy_overrides_default() {
        // The builder method threads the policy through; the test
        // pins the surface so a future refactor can't silently lose
        // the override path. All three variants are covered.
        let workspace = std::path::PathBuf::from("/tmp");
        for policy in [
            ContextOverflowPolicy::Compact,
            ContextOverflowPolicy::Reroute,
            ContextOverflowPolicy::Surface,
        ] {
            let runner = Runner::new(
                workspace.clone(),
                ProviderChoice::Mock { responses: vec![] },
                EventSink::Null,
            )
            .expect("mock runner construction is infallible")
            .with_overflow_policy(policy);
            assert_eq!(runner.overflow_policy, policy);
        }
    }

    #[test]
    fn overflow_selector_picks_at_least_one_when_local_estimate_says_no_overshoot() {
        // The adapter raised overflow, but the local token estimate
        // says we had headroom. The selector must still pick at
        // least one item (the smallest unpinned) — the adapter's
        // claim wins over the local estimate; otherwise the runner
        // would spin retrying the same overflowed call.
        let summaries = vec![
            summary_fixture("small", 50, false),
            summary_fixture("medium", 100, false),
            summary_fixture("large", 200, false),
        ];
        let picks = pick_overflow_compaction_targets(&summaries, 50, 1000, 100);
        // Floor is min unpinned (50). After +25% the padded target
        // is 62. The selector greedily takes the largest (200), which
        // alone covers the padded floor, so picks = ["large"].
        assert_eq!(
            picks,
            vec!["large".to_string()],
            "expected exactly the largest item; got {picks:?}"
        );
    }

    #[test]
    fn overflow_selector_picks_largest_unpinned_first_with_margin() {
        // limit=1000, current=900, needed=1050 ⇒ headroom = 100,
        // raw_target = 1050 - 100 = 950. min_unpinned = 100 so the
        // effective_target stays at 950. Padded by 25% = 1187.
        // Unpinned items sorted desc: c(900), b(400), a(100). Greedy:
        // pick c (900), still < 1187 → pick b (1300 ≥ 1187 → stop).
        // Expected chosen list: ["c", "b"].
        let summaries = vec![
            summary_fixture("a", 100, false),
            summary_fixture("b", 400, false),
            summary_fixture("c", 900, false),
        ];
        let picks = pick_overflow_compaction_targets(&summaries, 1050, 1000, 900);
        assert_eq!(picks, vec!["c".to_string(), "b".to_string()]);
    }

    #[test]
    fn overflow_selector_skips_pinned_and_accumulates_when_needed() {
        // limit=1000, current=950, needed=1300 ⇒ raw_target = 250,
        // padded = 312. The largest item (c=400) is pinned, so the
        // selector must skip it. b=200 doesn't cover alone; a=150
        // accumulates to 350 ≥ 312. Order: largest unpinned first.
        let summaries = vec![
            summary_fixture("a", 150, false),
            summary_fixture("b", 200, false),
            summary_fixture("c", 400, true), // pinned ⇒ must skip
        ];
        let picks = pick_overflow_compaction_targets(&summaries, 1300, 1000, 950);
        // b is larger than a so it ends up first; a is appended to
        // make up the rest.
        assert_eq!(picks, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn overflow_selector_returns_empty_when_all_pinned() {
        // All candidates pinned ⇒ nothing the auto-selector can
        // touch. The runner's Compact arm surfaces in this case.
        let summaries = vec![
            summary_fixture("a", 100, true),
            summary_fixture("b", 200, true),
        ];
        let picks = pick_overflow_compaction_targets(&summaries, 5000, 1000, 0);
        assert!(picks.is_empty(), "all-pinned ⇒ no picks; got {picks:?}");
    }
}
