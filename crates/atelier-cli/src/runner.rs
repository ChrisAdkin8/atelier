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
pub struct MockResponse {
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCallRequest>,
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
        let (adapter, probe_policy, probe_base_url): (Arc<dyn Adapter>, ProbePolicy, String) =
            match provider {
                ProviderChoice::Mock { responses } => (
                    Arc::new(build_mock_adapter(responses)),
                    ProbePolicy::Skip,
                    String::new(),
                ),
                ProviderChoice::Anthropic { model_id } => (
                    Arc::new(AnthropicAdapter::from_env(model_id).map_err(adapter_to_run_error)?),
                    ProbePolicy::Skip,
                    String::new(),
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
            concurrent_edit_policy: ConcurrentEditPolicy::Modal,
            resume_from: None,
            non_interactive: false,
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
        let _ = bus.send(Event::ModelProfileLoaded {
            model_id: profile.model_id.clone(),
            base_url: profile.base_url.clone(),
            strategy: profile.strategy,
            outcome,
        });
        // The profile itself is informational in v51 — the §1
        // conformance tracker still drives runtime strategy
        // selection at the adapter level. A v52 follow-on can
        // thread `profile.strategy` into the adapter's initial
        // strategy so the first turn skips the warm-up period.
        let _initial_strategy_hint = profile.strategy;

        // 5. Turn loop. v61 — when `resume_from` is set, replay the
        //    persisted conversation prefix first; the supplied prompt
        //    is then appended as a fresh user turn (or skipped when
        //    empty). On a fresh run we keep the pre-v61 single-prompt
        //    bootstrap.
        let session_id = session_handle.id();
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

            let response = self
                .adapter
                .chat(&messages, &tools_spec)
                .await
                .map_err(|e| RunError::Adapter(format!("{e}")))?;

            // 6. Parse envelope from response per the adapter's chosen
            //    strategy. Native tool calls (if any) take precedence:
            //    the dispatcher executes them and feeds results back as
            //    Role::Tool messages.
            let envelope = match response.strategy {
                Strategy::NativeTool => {
                    // The envelope rides as a `harness_meta` tool call;
                    // pull it out (if present) and dispatch the rest.
                    extract_native_envelope(&response.tool_calls).unwrap_or_default()
                }
                Strategy::JsonSentinel => parse_json_sentinel(&response.text)
                    .map(|parsed| parsed.envelope)
                    .unwrap_or_default(),
                Strategy::RegexProse => {
                    atelier_core::protocol_strategy::parse_regex_prose(&response.text)
                        .unwrap_or_default()
                }
            };

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

fn build_mock_adapter(responses: Vec<MockResponse>) -> MockAdapter {
    let m = MockAdapter::new("mock:run");
    for r in responses {
        use atelier_core::adapter::{ChatResponse, StreamChunk, Usage};
        use atelier_core::context::TokenSource;
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
}
