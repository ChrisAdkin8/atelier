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
        ToolCallRequest, ToolSpec,
    },
    context::{
        ContextItem, ContextItemId, ContextManager, Payload, Provenance, TokenCount, TokenSource,
    },
    dispatcher::{
        ConcurrentEditPolicy, Dispatcher, SessionDispatcher, ShellHookExecutor, Tool, ToolContext,
        ToolRegistry,
    },
    dod::DodConfig,
    file_watcher,
    hooks::HookSet,
    ledger::Ledger,
    memory::MemoryStore,
    persistence::OnDiskSession,
    plan::PlanCanvas,
    protocol::Envelope,
    protocol_strategy::{parse_json_sentinel, parse_native_tool, NativeToolCall, Strategy},
    sandbox::SandboxPolicy,
    session::{self, Command as SessionCommand, ConcurrentEditOutcome, Event, MessageRole},
    state::NoopHook,
    tools::{
        ast_grep::AstGrep, edit_file::EditFile, grep::Grep, list_dir::ListDir, read_file::ReadFile,
        shell::Shell, write_file::WriteFile,
    },
    SessionHandle, SessionId, State, Transition,
};

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
            ProviderChoice::OpenAiCompat { model_id, base_url } => {
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
                let base = base_url.unwrap_or_else(|| {
                    std::env::var("OPENAI_BASE_URL")
                        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
                });
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
                (
                    Arc::new(
                        atelier_core::adapter::openai_compat::OpenAiCompatAdapter::new(
                            api_key,
                            model_id,
                            base.clone(),
                        ),
                    ),
                    ProbePolicy::Auto,
                    base,
                    cost,
                )
            }
        };
        Ok(Self {
            workspace,
            adapter,
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
            non_interactive: false,
            degradation_window: atelier_core::protocol_conformance::DEFAULT_DEGRADATION_WINDOW,
            degradation_threshold:
                atelier_core::protocol_conformance::DEFAULT_DEGRADATION_THRESHOLD,
            overflow_policy: ContextOverflowPolicy::Compact,
        })
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
    /// queueing twenty mock responses.
    #[allow(dead_code)]
    pub fn with_degradation_window(mut self, window: usize) -> Self {
        self.degradation_window = window;
        self
    }

    /// §1 BYOM — override the conformance-driven degradation threshold
    /// (failures-in-window count). The default is
    /// [`atelier_core::protocol_conformance::DEFAULT_DEGRADATION_THRESHOLD`]
    /// (PROVISIONAL 3). See [`Self::with_degradation_window`] for the
    /// companion knob.
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

    /// Drive the loop until `claims_done` or `max_turns`. Returns when:
    ///   * a turn carried `claims_done: true` (success path; runs DoD next),
    ///   * `max_turns` reached (timeout; `final_state = AwaitingUser`),
    ///   * the adapter errored irrecoverably (propagated).
    pub async fn run(&self, prompt: String) -> Result<RunReport, RunError> {
        let workspace = self.workspace.clone();

        // 1. Load config: hooks + DoD. Both are tolerant of missing files —
        //    a fresh repo with no .atelier/hooks/ or .atelier/dod.json
        //    just gets an empty HookSet + None DoD.
        let hooks = HookSet::load_dir(&workspace.join(".atelier/hooks"))
            .map_err(|e| RunError::Config(format!("hooks: {e}")))?;
        let dod = DodConfig::load(&workspace).map_err(|e| RunError::Config(format!("dod: {e}")))?;

        // 2. Sandbox + dispatcher + ledger.
        let sandbox = SandboxPolicy::restrictive(&workspace)
            .map_err(|e| RunError::Config(format!("sandbox: {e}")))?;
        let registry = built_in_registry()?;
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
        let session_handle = session::spawn(Arc::new(NoopHook), Arc::new(NoopHook));
        let bus = session_handle.events_sender();

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
        let (profile, outcome) = match self.probe_policy {
            ProbePolicy::Skip => {
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
        };
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
        let _ = bus.send(Event::ModelProfileLoaded {
            model_id: profile.model_id.clone(),
            base_url: profile.base_url.clone(),
            strategy: profile.strategy,
            outcome,
            capability_row: Some(capability_row),
        });
        // The profile recommends the starting §2 strategy; the
        // runtime conformance tracker downshifts it (one-way) if the
        // model emits malformed envelopes past the threshold. The
        // active strategy lives in `active_strategy`; degrade events
        // emit on every transition so UIs refresh the footer badge.
        let mut active_strategy = profile.strategy;
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
                let _ = bus.send(Event::MessageCommitted {
                    role: role_bus,
                    text: entry.content,
                });
                messages.push(msg);
            }
            // Surface every recovery_log entry to UIs as a system
            // message so the user knows what was preserved. The
            // entries themselves stay on the persisted recovery_log;
            // we re-write them to the next save so the audit trail is
            // never erased.
            for rec in &on_disk.recovery_log {
                let _ = bus.send(Event::MessageCommitted {
                    role: MessageRole::System,
                    text: format!(
                        "[recovery] turn={} reason={:?} captured_at={} partial={:?}",
                        rec.turn_id, rec.reason, rec.captured_at, rec.partial_content
                    ),
                });
            }
            resumed_session = Some(on_disk);
            if !prompt.trim().is_empty() {
                context_manager
                    .lock()
                    .add(context_item_for_user_prompt(&prompt, &prompt_now));
                let _ = bus.send(Event::MessageCommitted {
                    role: MessageRole::User,
                    text: prompt.clone(),
                });
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
            let _ = bus.send(Event::MessageCommitted {
                role: MessageRole::User,
                text: prompt.clone(),
            });
        }
        // v57 (M-bug-3 fix) — emit one ContextItems snapshot before
        // entering the turn loop so a UI subscriber that joins
        // immediately after `MessageCommitted{User}` doesn't see an
        // empty Context panel until turn 1 finishes (which never
        // happens for max_turns=0). The aggregate `ContextSnapshot`
        // still fires per-turn; this pre-loop emission is the
        // per-item snapshot only.
        let initial_items = context_manager.lock().summarise();
        let _ = bus.send(Event::ContextItems {
            items: initial_items,
        });
        let mut turns = 0;
        let mut final_state = State::Idle;
        let tools_spec = registry_to_tool_specs();

        for turn in 0..self.max_turns {
            advance(&session_handle, State::Idle, State::Streaming).await?;
            final_state = State::Streaming;

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
            let response = {
                let mut overflow_retries: usize = 0;
                loop {
                    match self.adapter.chat(&messages, &tools_spec).await {
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
                                        let _ = bus.send(Event::ContextOverflowResolved {
                                            resolution: "surfaced",
                                            freed_tokens: None,
                                            items_compacted: None,
                                        });
                                        return Err(RunError::ContextOverflow {
                                            needed_tokens,
                                            limit_tokens,
                                        });
                                    }
                                    let now = now_rfc3339();
                                    let sid_str = session_id.0.to_string();
                                    match crate::compaction::compact(
                                        self.adapter.as_ref(),
                                        &session_dispatcher,
                                        &workspace,
                                        &sid_str,
                                        picks.clone(),
                                        &now,
                                    )
                                    .await
                                    {
                                        Ok(out) => {
                                            let _ = bus.send(Event::ContextOverflowResolved {
                                                resolution: "compacted",
                                                freed_tokens: Some(out.freed_tokens),
                                                items_compacted: Some(picks.len()),
                                            });
                                            overflow_retries += 1;
                                            continue;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "context-overflow auto-compaction failed; surfacing the original overflow"
                                            );
                                            let _ = bus.send(Event::ContextOverflowResolved {
                                                resolution: "surfaced",
                                                freed_tokens: None,
                                                items_compacted: None,
                                            });
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
                                    let _ = bus.send(Event::ContextOverflowResolved {
                                        resolution: "rerouted",
                                        freed_tokens: None,
                                        items_compacted: None,
                                    });
                                    return Err(RunError::Config(
                                        "reroute not yet implemented".into(),
                                    ));
                                }
                                ContextOverflowPolicy::Surface => {
                                    let _ = bus.send(Event::ContextOverflowResolved {
                                        resolution: "surfaced",
                                        freed_tokens: None,
                                        items_compacted: None,
                                    });
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
                note: None,
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
                    let _ = bus.send(Event::StrategyDegraded {
                        from: previous,
                        to: next,
                        reason,
                    });
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
            let _ = bus.send(Event::MessageCommitted {
                role: MessageRole::Assistant,
                text: response.text.clone(),
            });
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
                let _ = bus.send(Event::PlanSnapshot { steps });
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
                let _ = bus.send(Event::ClaimedChanges { changes });
            }
            let real_tool_calls: Vec<_> = response
                .tool_calls
                .into_iter()
                .filter(|c| c.name != atelier_core::protocol_strategy::HARNESS_META_NAME)
                .collect();

            if !real_tool_calls.is_empty() {
                advance(&session_handle, State::Streaming, State::ToolDispatching).await?;
                advance(
                    &session_handle,
                    State::ToolDispatching,
                    State::ToolExecuting,
                )
                .await?;
                let ctx = ToolContext {
                    workspace_root: &workspace,
                    sandbox: &sandbox,
                    // tool_call_id is set per-call by Dispatcher::dispatch;
                    // the value here is ignored.
                    tool_call_id: None,
                    audit_log_path: Some(audit_log_path.as_path()),
                };
                for call in real_tool_calls {
                    let outcome = session_dispatcher.dispatch(&call, &ctx, now_rfc3339).await;
                    // Feed the tool result back into the next turn's
                    // messages so the adapter sees what happened.
                    // v25.2-F: failure path uses serde_json::json! so an
                    // error containing quotes/backslashes/newlines is
                    // properly escaped. Pre-fix `format!("{{\"error\":\"{e}\"}}")`
                    // produced invalid JSON when `e` contained `"`.
                    let result_str = match &outcome.result {
                        Ok(r) => serde_json::to_string(&r.output).unwrap_or_default(),
                        Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
                    };
                    let _ = bus.send(Event::MessageCommitted {
                        role: MessageRole::Tool,
                        text: result_str.clone(),
                    });
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
                let _ = bus.send(Event::ContextSnapshot {
                    known_tokens: token_count.count,
                    unknown_tokens: 0,
                });
            }
            let context_items = context_manager.lock().summarise();
            let _ = bus.send(Event::ContextItems {
                items: context_items,
            });

            // v55 — §5 Memory panel snapshot. The MemoryStore is now
            // mutable from the UI via SessionDispatcher's add /
            // delete / promote mutators (Step 3). The runner still
            // re-emits at each turn boundary so a late-joining
            // subscriber converges to the live state.
            let _ = bus.send(Event::MemoryCards {
                cards: memory_store.lock().summarise(),
            });

            // 8. If the envelope or scripted response says done, exit.
            if envelope.claimed_done == Some(true) {
                advance(&session_handle, State::Streaming, State::Verifying).await?;
                final_state = State::Verifying;
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

        // 10. Done — transition to terminal and persist.
        if final_state == State::Verifying {
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

        Ok(RunReport {
            session_id,
            turns,
            final_state,
            dod_passed,
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

fn built_in_registry() -> Result<ToolRegistry, RunError> {
    let mut r = ToolRegistry::new();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile),
        Arc::new(ListDir),
        Arc::new(Grep),
        Arc::new(WriteFile),
        Arc::new(EditFile),
        Arc::new(AstGrep),
        Arc::new(Shell),
    ];
    for t in tools {
        r.register(t)
            .map_err(|e| RunError::Config(format!("tool registry: {e}")))?;
    }
    Ok(r)
}

/// Empty `&[ToolSpec]` for v0 — adapters that need the tool list for
/// native tool-use mode get it from this. The real list (with each tool's
/// `input_schema`) lands when the dispatcher's input-schema work expands.
fn registry_to_tool_specs() -> Vec<ToolSpec> {
    Vec::new()
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
                        let _ = bus.send(Event::FilesChangedAcknowledged {
                            outcome: ConcurrentEditOutcome::AutoReload,
                        });
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
                                    let _ = bus.send(Event::FilesChangedAcknowledged {
                                        outcome: ConcurrentEditOutcome::PauseTimedOut,
                                    });
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
