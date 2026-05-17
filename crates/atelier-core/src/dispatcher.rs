//! §15 tool dispatcher — central per-tool-call orchestrator.
//!
//! Spec §15 "Built-in tools" + "MCP-routed tools":
//!   > Built-in tools (file ops, shell, search) and MCP-routed tools share
//!   > the same `ToolDispatching → ToolExecuting` state transitions. The
//!   > loop does not branch on tool origin: same parallelism cap, same
//!   > checkpoint/ledger writes, same sandbox model (§11), same
//!   > cancellation semantics.
//!
//! The dispatcher is the place that uniformity lives. It looks up a
//! [`Tool`] from a [`ToolRegistry`], identifies the §15 hooks that apply
//! (pre-tool + post-tool), executes the tool, builds the
//! [`crate::ledger::LedgerEntry`] for the call, and translates any staged
//! writes into the [`crate::session::Event::EditStaged`] events the UI
//! consumes.
//!
//! ## Scope of this skeleton
//!
//! * **In place:** `Tool` trait, `ToolRegistry`, `Dispatcher::dispatch`
//!   end-to-end on the in-process side. Hook *identification* via
//!   [`crate::hooks::HookSet`]. Per-call latency + cost-ledger entry.
//!   `EditStaged` events derived from staged writes.
//! * **Deferred to follow-ons (each tracked in `tasks/todo.md`):**
//!   * Real **hook execution** — subprocess / HTTP per `HookImplementation`,
//!     with the §15 "warn-but-never-block" time-budget wrapper. The
//!     dispatcher today returns the *list of hooks that would have run* so
//!     downstream tests can assert the wiring without the runner being in
//!     place. A `HookExecutor` trait is sketched at the bottom of the file.
//!   * Real **built-in tool implementations** (`read_file`, `write_file`,
//!     `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`) — each gets
//!     its own module and lands across multiple commits.
//!   * **MCP-routed tools** — a `Tool` impl that proxies to `rmcp`; gated
//!     on the Q7 spike. Slots into the same registry transparently.
//!
//! The dispatcher returns a [`DispatchOutcome`] rather than performing the
//! ledger append + event broadcast directly. That keeps it **pure** —
//! testable without a running session actor or a real `Ledger` — and
//! mirrors the pattern used in `staging.rs` / `verify.rs` / `context.rs`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::adapter::ToolCallRequest;
use crate::error::ToolError;
use crate::hooks::{HookEvent, HookManifest, HookSet};
use crate::ledger::{local_cost_usd, LedgerEntry, DEFAULT_LOCAL_RATE_USD_PER_SEC};
use crate::sandbox::SandboxPolicy;
use crate::session::{edit_staged_events, Event};

/// Spec §8 trust-budget side-effect classification. Mirrors the
/// `tool_manifest.v1.json` enum exactly so a Rust `Tool` impl and its bundled
/// manifest cannot disagree. `Tool::side_effect_class()` carries the default;
/// per-call override goes through the trust-budget UI (not this layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffectClass {
    /// Read-only or contained-to-temp; no trust budget cost.
    LocalSafe,
    /// Writes inside the repo; costs 1 budget unit (PROVISIONAL, spec §8).
    LocalRisky,
    /// Affects shared state outside the workspace (e.g., a posted comment);
    /// costs 20 (always asks).
    SharedState,
    /// Irreversible side effect; costs 20 + double-confirm.
    Irreversible,
}

impl SideEffectClass {
    /// PROVISIONAL — spec §8 calibrated together.
    pub fn budget_cost(self) -> u32 {
        match self {
            Self::LocalSafe => 0,
            Self::LocalRisky => 1,
            Self::SharedState => 20,
            Self::Irreversible => 20,
        }
    }

    pub fn requires_double_confirm(self) -> bool {
        matches!(self, Self::Irreversible)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalSafe => "local-safe",
            Self::LocalRisky => "local-risky",
            Self::SharedState => "shared-state",
            Self::Irreversible => "irreversible",
        }
    }
}

/// What a `Tool` returns to the dispatcher. `output` flows into the next
/// model turn as a tool-result message. `staged_writes` (when present) means
/// the tool produced edits via §3 staging; the dispatcher publishes one
/// `EditStaged` event per file once the batch commits.
///
/// As of v46 the tool hands back a [`StagedBatch`] (validated + staged on
/// disk, NOT renamed). The dispatcher's [`ApprovalPolicy`] decides
/// whether to call `commit_all` immediately (the v45 behaviour, default)
/// or to emit `StagingPendingApproval` and wait for a user decision
/// (spec §3 "Hunk accept / reject"). Tools therefore call
/// `Staging::stage()` instead of `Staging::commit()`.
///
/// `Clone` was previously derived for `ToolResult` but no caller uses it
/// — the dispatcher consumes the result by value. Removing the derive
/// keeps the `StagedBatch` resource (owns a `TempDir`) inside a value
/// that can't be accidentally duplicated, which would leak temp trees.
#[derive(Debug)]
pub struct ToolResult {
    pub output: serde_json::Value,
    /// `None` for read-only tools, `Some(StagedBatch)` for tools that
    /// went through `Staging::stage` to prepare writes.
    pub staged_writes: Option<crate::staging::StagedBatch>,
}

/// Per-call environment passed to a `Tool`. Borrows the session-scoped
/// pieces the tool needs to execute correctly (workspace root, sandbox
/// profile). The lifetime is bound to the dispatch call.
pub struct ToolContext<'a> {
    pub workspace_root: &'a Path,
    pub sandbox: &'a SandboxPolicy,
}

/// The §15 dispatch contract. Async because tool execution may involve
/// subprocess / I/O (`shell`, `grep`) or remote calls (MCP-routed tools).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name. Must match the bundled manifest and the name the model
    /// emits in `ToolCallRequest::name`.
    fn name(&self) -> &str;

    /// Default trust-budget classification. Overridable per-call via the
    /// trust-budget UI (not this layer).
    fn side_effect_class(&self) -> SideEffectClass;

    /// Validate the model-emitted arguments before [`Self::execute`] runs.
    /// Spec §15: "the harness validates the model's tool-call arguments
    /// against this before dispatch; SchemaViolation (§2.5) on mismatch."
    /// `Err(msg)` is mapped onto `ToolError::SchemaViolation` by the
    /// dispatcher and short-circuits execute (no hooks fire either).
    ///
    /// **Built-in tools rely on the default `Ok(())`** because their
    /// `execute` impl deserialises `args` into a `#[serde(deny_unknown_fields)]`
    /// struct — that path produces `SchemaViolation` on shape errors and is
    /// equivalent to running the bundled manifest's `input_schema` through
    /// a JSONSchema validator for the constraints those manifests express
    /// (types, required fields, enum values, unknown-field rejection).
    /// MCP-routed tools (when they land) and any future tool whose
    /// bundled `input_schema` expresses constraints serde can't (regex
    /// patterns, length bounds, conditional schemas) should override with
    /// a real validator. This trait hook is the integration seam.
    fn validate_args(&self, _args: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }

    /// Run the tool. Errors here surface as `ToolError` variants; the §2.5
    /// state machine's `Recovery` routing decides how to react.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError>;
}

/// Read-only lookup of tools by name. Built from the union of built-in
/// tools (bundled in `atelier-core`) and any MCP-routed proxies the
/// session has registered.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegisterError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(RegisterError::DuplicateName(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    #[error("a tool named {0:?} is already registered")]
    DuplicateName(String),
}

/// Spec §3 hunk accept/reject contract.
///
/// Called by [`Dispatcher::dispatch`] between staging and commit when a
/// tool produced [`crate::staging::StagedBatch`]. The gate decides which
/// files commit — auto-approve all (the default [`AutoApprove`]
/// behaviour, identical to v45), or block on a user decision routed
/// through the broadcast bus (the production
/// [`PendingApprovalGate`] used by [`SessionDispatcher`] when its
/// policy is [`ApprovalPolicy::AwaitApproval`]).
///
/// The trait is async because real implementations wait on a `oneshot`
/// channel; the trivial impl returns instantly.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Decide which of `pending` to commit. Returns the accepted paths
    /// (subset of `pending[i].path`). An empty return = full reject.
    /// `commit_id` is the correlation token the implementation may use
    /// for round-tripping over the bus.
    async fn approve(
        &self,
        commit_id: uuid::Uuid,
        pending: &[crate::staging::FileOutcome],
    ) -> Vec<std::path::PathBuf>;

    // v49 — `notify_outcome` removed. `CommitDecision` emission moved
    // to `SessionDispatcher::dispatch` so the bus ordering
    // (EditStaged per file → LedgerAppended → CommitDecision) matches
    // the documented "user-visible side effects before bookkeeping"
    // intent. The dispatcher returns the commit summary on
    // `DispatchOutcome.approval_summary`; the SessionDispatcher
    // broadcasts the CommitDecision event from there.
}

/// Default [`ApprovalGate`] — commits every staged file unconditionally.
/// Pre-v46 behaviour; used by tests and headless runs (`atelier run`
/// without explicit `--require-approval`).
pub struct AutoApprove;

#[async_trait]
impl ApprovalGate for AutoApprove {
    async fn approve(
        &self,
        _commit_id: uuid::Uuid,
        pending: &[crate::staging::FileOutcome],
    ) -> Vec<std::path::PathBuf> {
        pending.iter().map(|f| f.path.clone()).collect()
    }
}

/// Stateful dispatcher composing a [`ToolRegistry`], a [`HookSet`], the
/// [`HookExecutor`] that runs hooks at the per-call lifecycle boundaries
/// (pre-tool + post-tool), and the [`ApprovalGate`] that decides which
/// staged writes commit (spec §3 hunk accept/reject). Defaults are
/// [`NoopHookExecutor`] + [`AutoApprove`]; production wires in
/// [`ShellHookExecutor`] + [`PendingApprovalGate`] via the builder
/// methods.
pub struct Dispatcher {
    registry: ToolRegistry,
    hooks: HookSet,
    executor: Arc<dyn HookExecutor>,
    approval_gate: Arc<dyn ApprovalGate>,
}

impl Dispatcher {
    pub fn new(registry: ToolRegistry, hooks: HookSet) -> Self {
        Self {
            registry,
            hooks,
            executor: Arc::new(NoopHookExecutor),
            approval_gate: Arc::new(AutoApprove),
        }
    }

    /// Replace the hook executor (default is [`NoopHookExecutor`]). Returns
    /// `Self` so the builder reads naturally:
    /// `Dispatcher::new(reg, hooks).with_executor(Arc::new(ShellHookExecutor::new(policy)))`.
    pub fn with_executor(mut self, executor: Arc<dyn HookExecutor>) -> Self {
        self.executor = executor;
        self
    }

    /// Replace the approval gate. Default is [`AutoApprove`].
    /// [`SessionDispatcher::with_approval_policy`] constructs a
    /// [`PendingApprovalGate`] and threads it in.
    pub fn with_approval_gate(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval_gate = gate;
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn hooks(&self) -> &HookSet {
        &self.hooks
    }

    /// Dispatch one tool call. Returns a [`DispatchOutcome`] regardless of
    /// success / failure; the outcome's `result` field carries the
    /// `Result<ToolResult, ToolError>`. `now` supplies the RFC 3339
    /// timestamp threaded onto the ledger entry — caller-supplied for the
    /// same reason as elsewhere in this crate (no implicit time dep).
    pub async fn dispatch(
        &self,
        call: &ToolCallRequest,
        ctx: &ToolContext<'_>,
        now: impl Fn() -> String,
    ) -> DispatchOutcome {
        let timestamp = now();
        let started = Instant::now();

        // 1. Look up the tool. Unknown name → ExecutionFailed (not
        //    SchemaViolation; the harness should refuse to dispatch
        //    unknown names before reaching the dispatcher, but if it
        //    does we fail closed).
        let tool = match self.registry.get(&call.name) {
            Some(t) => t,
            None => {
                let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
                return DispatchOutcome {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    result: Err(ToolError::ExecutionFailed {
                        tool: call.name.clone(),
                        exit_code: -1,
                        stderr: format!("unknown tool {:?}", call.name),
                    }),
                    ledger_entry: LedgerEntry::tool_call(
                        timestamp,
                        call.name.clone(),
                        latency_ms,
                        Some(local_cost_usd(latency_ms, DEFAULT_LOCAL_RATE_USD_PER_SEC)),
                        Some("unknown tool".into()),
                    ),
                    events: Vec::new(),
                    matched_hooks: HookPhases::default(),
                    approval_summary: None,
                };
            }
        };

        // 1b. Validate args against the tool's bundled input_schema
        //     (spec §15). On schema failure short-circuit with a
        //     SchemaViolation — no hooks fire, no execute attempted.
        if let Err(msg) = tool.validate_args(&call.arguments) {
            let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
            return DispatchOutcome {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                result: Err(ToolError::SchemaViolation {
                    tool: call.name.clone(),
                    error: msg.clone(),
                }),
                ledger_entry: LedgerEntry::tool_call(
                    timestamp,
                    call.name.clone(),
                    latency_ms,
                    Some(local_cost_usd(latency_ms, DEFAULT_LOCAL_RATE_USD_PER_SEC)),
                    Some(format!("SchemaViolation: {msg}")),
                ),
                events: Vec::new(),
                matched_hooks: HookPhases::default(),
                approval_summary: None,
            };
        }

        // 2. Identify + run pre-tool hooks. Per spec §15: warn and
        //    continue, never block — `HookExecutor::execute` returns ()
        //    and logs its own errors / over-budget warnings via `tracing`.
        //    `matched_hooks` records what fired (for UI / ledger).
        let pre_tool_hooks = self.hooks.for_tool_event(HookEvent::PreTool, &call.name);
        let pre_tool_names: Vec<String> = pre_tool_hooks.iter().map(|h| h.name.clone()).collect();
        let pre_payload = serde_json::json!({
            "event": "pre-tool",
            "tool_name": call.name,
            "tool_call_id": call.id,
            "arguments": call.arguments,
        });
        // Spec §15 hooks are warn-but-never-block; the executor handles
        // its own time budget + error logging per call. Run them
        // concurrently so N pre-tool hooks don't serialise an N-fold
        // fork/exec overhead onto the critical dispatch path.
        join_all(
            pre_tool_hooks
                .iter()
                .map(|m| self.executor.execute(m, &pre_payload)),
        )
        .await;

        // 3. Execute the tool.
        let raw_result = tool.execute(call.arguments.clone(), ctx).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        let cost_usd = Some(local_cost_usd(latency_ms, DEFAULT_LOCAL_RATE_USD_PER_SEC));

        // 4. Stage → approval gate → commit_selected → events.
        //
        //    The pure `Dispatcher` invokes its `approval_gate` between
        //    stage and commit. The default [`AutoApprove`] returns
        //    every pending path so behaviour is identical to pre-v46.
        //    `SessionDispatcher` installs a [`PendingApprovalGate`]
        //    when its [`ApprovalPolicy::AwaitApproval`] is set —
        //    that gate emits `StagingPendingApproval` on the bus and
        //    awaits the user's accept-set via
        //    `SessionDispatcher::submit_approval`.
        //
        //    Commit errors fold into the final `result` as a tool
        //    error so the ledger note + the next-turn model message
        //    both see the failure.
        let (result, events, approval_summary) = match raw_result {
            Ok(mut ok) => match ok.staged_writes.take() {
                Some(batch) => {
                    let commit_id = uuid::Uuid::new_v4();
                    let pending_paths: Vec<std::path::PathBuf> = batch
                        .pending_files()
                        .iter()
                        .map(|f| f.path.clone())
                        .collect();
                    let accepted_vec = self
                        .approval_gate
                        .approve(commit_id, batch.pending_files())
                        .await;
                    let accepted: std::collections::HashSet<std::path::PathBuf> =
                        accepted_vec.into_iter().collect();
                    match batch.commit_selected(&accepted) {
                        Ok(report) => {
                            let committed: Vec<std::path::PathBuf> =
                                report.files.iter().map(|f| f.path.clone()).collect();
                            let committed_set: std::collections::HashSet<_> =
                                committed.iter().cloned().collect();
                            let dropped: Vec<std::path::PathBuf> = pending_paths
                                .into_iter()
                                .filter(|p| !committed_set.contains(p))
                                .collect();
                            let events = edit_staged_events(&report);
                            let summary = ApprovalSummary {
                                commit_id,
                                committed,
                                dropped,
                            };
                            (Ok(ok), events, Some(summary))
                        }
                        Err(commit_err) => {
                            // Commit failed wholesale — everything we
                            // wanted to commit is "dropped".
                            let summary = ApprovalSummary {
                                commit_id,
                                committed: Vec::new(),
                                dropped: pending_paths,
                            };
                            let err = ToolError::ExecutionFailed {
                                tool: call.name.clone(),
                                exit_code: -1,
                                stderr: format!("staging commit failed: {commit_err}"),
                            };
                            (Err(err), Vec::new(), Some(summary))
                        }
                    }
                }
                None => (Ok(ok), Vec::new(), None),
            },
            Err(e) => (Err(e), Vec::new(), None),
        };
        let ledger_note = match &result {
            Ok(_) => None,
            Err(e) => Some(format!("{}: {}", e.kind(), e)),
        };

        let ledger_entry = LedgerEntry::tool_call(
            timestamp,
            call.name.clone(),
            latency_ms,
            cost_usd,
            ledger_note,
        );

        // 5. Identify + run post-tool hooks. Payload includes the
        //    success/failure shape so the hook can act on outcomes
        //    (e.g., a post-tool lint that runs only on successful writes).
        let post_tool_hooks = self.hooks.for_tool_event(HookEvent::PostTool, &call.name);
        let post_tool_names: Vec<String> = post_tool_hooks.iter().map(|h| h.name.clone()).collect();
        let post_payload = serde_json::json!({
            "event": "post-tool",
            "tool_name": call.name,
            "tool_call_id": call.id,
            "arguments": call.arguments,
            "ok": result.is_ok(),
            "error_kind": result.as_ref().err().map(|e| e.kind()),
        });
        join_all(
            post_tool_hooks
                .iter()
                .map(|m| self.executor.execute(m, &post_payload)),
        )
        .await;

        let matched_hooks = HookPhases {
            pre_tool: pre_tool_names,
            post_tool: post_tool_names,
        };

        DispatchOutcome {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result,
            ledger_entry,
            events,
            matched_hooks,
            approval_summary,
        }
    }
}

/// Result of one `Dispatcher::dispatch`. The caller — the agent-loop
/// turn driver — does the side-effecting part (ledger append + event
/// broadcast) so the dispatcher stays pure / testable.
pub struct DispatchOutcome {
    pub tool_call_id: String,
    pub tool_name: String,
    /// `Ok(ToolResult)` on success; `Err(ToolError)` routes via §2.5.
    pub result: Result<ToolResult, ToolError>,
    /// Always present — even on failure, the call counts against the cost
    /// ledger so the §3 cost meter doesn't underreport.
    pub ledger_entry: LedgerEntry,
    /// EditStaged events ready to publish on the session bus, in commit
    /// order (lexicographic by path).
    pub events: Vec<Event>,
    /// Names of hooks that match this tool call. Subprocess execution
    /// lands in a follow-on; returning names today lets tests + the UI
    /// surface "X hooks will fire" without the runner being in place.
    pub matched_hooks: HookPhases,
    /// Spec §3 hunk-accept-reject summary. `Some` when the tool
    /// produced staged writes (regardless of policy); `None` for
    /// read-only tools. Carried out so the SessionDispatcher can
    /// emit `Event::CommitDecision` *after* the per-file `EditStaged`
    /// events — the v49 fix to the ordering inversion the v48 audit
    /// surfaced.
    pub approval_summary: Option<ApprovalSummary>,
}

/// Snapshot of how a staged batch was resolved by the approval gate.
/// Lives on [`DispatchOutcome`]; emitted as `Event::CommitDecision`
/// by [`SessionDispatcher::dispatch`].
#[derive(Debug, Clone)]
pub struct ApprovalSummary {
    pub commit_id: uuid::Uuid,
    pub committed: Vec<std::path::PathBuf>,
    pub dropped: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookPhases {
    pub pre_tool: Vec<String>,
    pub post_tool: Vec<String>,
}

// ---------- SessionDispatcher (side-effecting wrapper) ----------

/// Hunk-accept-reject policy for [`SessionDispatcher`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Auto-approve every staged file — pre-v46 behaviour. Default for
    /// headless runs (`atelier run`) and tests.
    #[default]
    AutoApproveAll,
    /// Spec §3 "Hunk accept / reject" — dispatcher emits
    /// `Event::StagingPendingApproval` and blocks until the consumer
    /// calls [`SessionDispatcher::submit_approval`].
    AwaitApproval,
}

/// Production approval gate that emits the pending event on the bus
/// and waits for the user's decision. Lives on the `SessionDispatcher`
/// rather than the pure `Dispatcher` because the bus is a side-effect.
struct PendingApprovalGate {
    events: tokio::sync::broadcast::Sender<Event>,
    pending: Arc<
        parking_lot::Mutex<
            std::collections::HashMap<
                uuid::Uuid,
                tokio::sync::oneshot::Sender<Vec<std::path::PathBuf>>,
            >,
        >,
    >,
}

#[async_trait]
impl ApprovalGate for PendingApprovalGate {
    async fn approve(
        &self,
        commit_id: uuid::Uuid,
        pending: &[crate::staging::FileOutcome],
    ) -> Vec<std::path::PathBuf> {
        // Register a oneshot and emit the bus event. The consumer
        // calls SessionDispatcher::submit_approval(commit_id, accepted)
        // which fulfils the oneshot, unblocking this await.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(commit_id, tx);

        let pending_files: Vec<crate::session::PendingFile> = pending
            .iter()
            .map(|f| crate::session::PendingFile {
                path: f.path.clone(),
                hunks: f.hunks.clone(),
            })
            .collect();
        let _ = self.events.send(Event::StagingPendingApproval {
            commit_id,
            files: pending_files,
        });

        // If the sender is dropped (consumer crashed mid-decision)
        // the receiver errors. Treat that as full reject — safer than
        // committing without user consent.
        rx.await.unwrap_or_default()
    }
}

/// Thin runtime wrapper around the pure [`Dispatcher`]. Owns a shared
/// reference to the §1 [`crate::ledger::Ledger`] and a clone of the
/// session's `broadcast::Sender<Event>`, so a single
/// [`Self::dispatch`] call performs the two side effects the agent-loop
/// integration code would otherwise reimplement at every call site:
/// append the outcome's `LedgerEntry` to the ledger, and broadcast each
/// outcome event onto the bus.
///
/// The pure [`Dispatcher`] stays the unit-test surface (no `Arc`, no
/// `Sender`); `SessionDispatcher` is the production surface that the
/// `atelier run` CLI / agent-loop wiring builds at session start.
pub struct SessionDispatcher {
    dispatcher: Dispatcher,
    ledger: Arc<crate::ledger::Ledger>,
    events: tokio::sync::broadcast::Sender<Event>,
    /// Pending oneshot senders, keyed by `commit_id`. Used by the
    /// `AwaitApproval` policy's `PendingApprovalGate`. Always present
    /// (even under `AutoApproveAll`) so `submit_approval` is callable
    /// without first toggling the policy.
    pending: Arc<
        parking_lot::Mutex<
            std::collections::HashMap<
                uuid::Uuid,
                tokio::sync::oneshot::Sender<Vec<std::path::PathBuf>>,
            >,
        >,
    >,
}

impl SessionDispatcher {
    pub fn new(
        dispatcher: Dispatcher,
        ledger: Arc<crate::ledger::Ledger>,
        events: tokio::sync::broadcast::Sender<Event>,
    ) -> Self {
        Self {
            dispatcher,
            ledger,
            events,
            pending: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Install the spec §3 hunk-accept-reject policy. With
    /// `AwaitApproval`, [`Self::dispatch`] for tools that produce staged
    /// writes emits `Event::StagingPendingApproval` and blocks until
    /// the consumer calls [`Self::submit_approval`].
    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        match policy {
            ApprovalPolicy::AutoApproveAll => {
                // Default; nothing to do — the pure dispatcher's
                // default gate is AutoApprove.
            }
            ApprovalPolicy::AwaitApproval => {
                let gate = Arc::new(PendingApprovalGate {
                    events: self.events.clone(),
                    pending: self.pending.clone(),
                });
                // Swap the inner dispatcher to one with the new gate.
                // Builder consumes self; we re-construct in place.
                let old = std::mem::replace(
                    &mut self.dispatcher,
                    Dispatcher::new(ToolRegistry::new(), HookSet::empty()),
                );
                self.dispatcher = old.with_approval_gate(gate);
            }
        }
        self
    }

    /// Spec §3 follow-on to `Event::StagingPendingApproval`: deliver
    /// the user's accept set for a pending commit. Returns `false`
    /// when `commit_id` doesn't match an outstanding pending (e.g.
    /// already approved, or the dispatcher dropped its receiver
    /// because the consumer disconnected).
    pub fn submit_approval(
        &self,
        commit_id: uuid::Uuid,
        accepted: Vec<std::path::PathBuf>,
    ) -> bool {
        let sender = self.pending.lock().remove(&commit_id);
        match sender {
            Some(tx) => tx.send(accepted).is_ok(),
            None => false,
        }
    }

    /// Access the underlying pure dispatcher (useful when tests need to
    /// hit the bare path or when the caller wants to inspect the
    /// registry / hook-set state).
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// Dispatch one tool call and perform the side effects. Returns the
    /// `DispatchOutcome` unchanged so the caller can still react to the
    /// `result` field (success / error routing via §2.5's `Recovery`).
    pub async fn dispatch(
        &self,
        call: &ToolCallRequest,
        ctx: &ToolContext<'_>,
        now: impl Fn() -> String,
    ) -> DispatchOutcome {
        let outcome = self.dispatcher.dispatch(call, ctx, now).await;
        self.ledger.append(outcome.ledger_entry.clone());
        // Order matters here:
        //   1. EditStaged per file (user-visible diff)
        //   2. LedgerAppended (cost meter tick)
        //   3. CommitDecision (approval summary, if staged_writes ran)
        //
        // A subscriber rendering both a diff pane and a cost meter
        // should see the diff arrive first; the summary lands last so
        // any UI clearing pending state does so AFTER it sees the
        // committed-file events.
        //
        // `broadcast::Sender::send` errors when there are zero
        // subscribers; we ignore that — the on-disk session is the
        // recoverable source of truth (§14), the bus is for live UI.
        for event in &outcome.events {
            let _ = self.events.send(event.clone());
        }
        let _ = self.events.send(crate::session::Event::LedgerAppended {
            entry: outcome.ledger_entry.clone(),
        });
        if let Some(summary) = &outcome.approval_summary {
            let _ = self.events.send(crate::session::Event::CommitDecision {
                commit_id: summary.commit_id,
                committed: summary.committed.clone(),
                dropped: summary.dropped.clone(),
            });
        }
        outcome
    }
}

// ---------- HookExecutor trait (sketched; impl deferred) ----------

/// Subprocess / HTTP hook runner. Spec §15: "over-budget = warn and
/// continue, never block." A no-op impl ships today as the default
/// dispatcher executor; production wires in [`ShellHookExecutor`].
///
/// **Concurrency.** When a single tool call matches multiple hooks for
/// the same phase (pre-tool or post-tool), the dispatcher fires all of
/// them concurrently via `futures::future::join_all` — N hooks no longer
/// serialise an N-fold fork/exec overhead onto the dispatch path. Hook
/// implementations must therefore treat any shared external resource
/// (audit log file, network endpoint, lock file) as needing its own
/// synchronisation; interleaved writes from two concurrent invocations
/// are the implementation's problem, not the dispatcher's.
///
/// **Privacy.** The `payload` carries the tool's `arguments` verbatim
/// (`shell` command strings, file paths, write contents). An audit-style
/// hook persisting payloads to disk could expose secrets that ride on
/// tool args (API keys in command lines, `.env` values, etc.). Spec §12
/// mandates redaction for the egress audit log; hook payloads should
/// route through the same redaction layer when it lands. Until then,
/// callers wiring real hooks must treat payloads as sensitive.
#[async_trait]
pub trait HookExecutor: Send + Sync {
    async fn execute(&self, manifest: &HookManifest, payload: &serde_json::Value);
}

/// Default impl: does nothing. The dispatcher uses
/// [`HookSet::for_tool_event`] to identify hooks today; once a real
/// executor lands, `Dispatcher::dispatch` calls it between identify and
/// execute (pre-tool) and between execute and return (post-tool).
pub struct NoopHookExecutor;

#[async_trait]
impl HookExecutor for NoopHookExecutor {
    async fn execute(&self, _manifest: &HookManifest, _payload: &serde_json::Value) {}
}

/// Concrete [`HookExecutor`] for hooks with `implementation.kind = shell`.
/// Each invocation spawns the hook's `command` inside the supplied §11
/// sandbox via the shared [`crate::subprocess`] helper, with the manifest's
/// `time_budget_ms` as the wall-clock cap. Spec §15 — over-budget = warn
/// and continue, never block: the executor returns regardless of timeout /
/// exit code, surfacing the outcome through `tracing` rather than back into
/// the dispatcher's control flow.
///
/// The HTTP-impl variant lands when an HTTP-impl hook ships; for now the
/// executor refuses non-shell impls with a `tracing::warn!` and a no-op.
pub struct ShellHookExecutor {
    sandbox: crate::sandbox::SandboxPolicy,
}

impl ShellHookExecutor {
    pub fn new(sandbox: crate::sandbox::SandboxPolicy) -> Self {
        Self { sandbox }
    }
}

#[async_trait]
impl HookExecutor for ShellHookExecutor {
    async fn execute(&self, manifest: &HookManifest, payload: &serde_json::Value) {
        use crate::hooks::HookImplementation;
        let HookImplementation::Shell { command, env } = &manifest.implementation else {
            tracing::warn!(
                hook = %manifest.name,
                kind = "non-shell",
                "ShellHookExecutor skipping non-shell hook impl"
            );
            return;
        };

        // The hook's command runs through `sh -c` so the manifest can use
        // pipes / redirections / env var expansion without re-implementing
        // a shell here. Same convention the `shell` built-in tool uses.
        let user_argv = vec!["sh".to_string(), "-c".to_string(), command.clone()];

        // Sandboxed argv generation; on unsupported platforms we warn and
        // skip rather than crash the dispatch loop.
        let (program, wrapped) = match crate::subprocess::sandboxed_argv(&user_argv, &self.sandbox)
        {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(hook = %manifest.name, error = %e, "hook sandbox wrap failed");
                return;
            }
        };

        let mut spec = crate::subprocess::SubprocessSpec::with_budget_ms(manifest.time_budget_ms);
        spec.env = env.clone();
        // Payload is forwarded as ATELIER_HOOK_PAYLOAD env-var (compact
        // single-line JSON). Hooks that need richer transport can shell out
        // to `jq` / `python` on this var.
        spec.env.insert(
            "ATELIER_HOOK_PAYLOAD".into(),
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
        );

        match crate::subprocess::run(&program, &wrapped, &spec).await {
            Ok(outcome) => {
                if outcome.timed_out {
                    tracing::warn!(
                        hook = %manifest.name,
                        duration_ms = outcome.duration_ms,
                        budget_ms = manifest.time_budget_ms,
                        "hook exceeded time budget; killed (warn-but-never-block per §15)"
                    );
                } else if !outcome.is_success() {
                    tracing::warn!(
                        hook = %manifest.name,
                        exit = ?outcome.exit_code,
                        stderr = %outcome.stderr_str_lossy(),
                        "hook exited non-zero"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(hook = %manifest.name, error = %e, "hook spawn failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ToolCallRequest;
    use crate::sandbox::SandboxPolicy;
    use serde_json::json;

    // A minimal in-tree Tool impl for tests. Returns whatever args it was
    // given as the output (echo semantics), with an optional pre-built
    // staged-write report so the dispatcher's event derivation can be
    // exercised without going near the filesystem.
    struct EchoTool {
        name: String,
        side_effect: SideEffectClass,
        /// Relative paths the tool will stage on each execute() call.
        /// We rebuild a fresh `StagedBatch` per call against the
        /// `ctx.workspace_root` (a real tempdir in dispatcher tests),
        /// because `StagedBatch` owns a `TempDir` and can't be cloned.
        staged_paths: Option<Vec<String>>,
        err: Option<ToolError>,
    }

    impl EchoTool {
        fn new(name: &str) -> Self {
            Self {
                name: name.into(),
                side_effect: SideEffectClass::LocalSafe,
                staged_paths: None,
                err: None,
            }
        }

        fn with_staged(mut self, paths: Vec<&str>) -> Self {
            self.staged_paths = Some(paths.iter().map(|s| s.to_string()).collect());
            self
        }

        fn with_error(mut self, err: ToolError) -> Self {
            self.err = Some(err);
            self
        }
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn side_effect_class(&self) -> SideEffectClass {
            self.side_effect
        }

        async fn execute(
            &self,
            args: serde_json::Value,
            ctx: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            if let Some(e) = &self.err {
                return Err(match e {
                    ToolError::ExecutionFailed {
                        tool,
                        exit_code,
                        stderr,
                    } => ToolError::ExecutionFailed {
                        tool: tool.clone(),
                        exit_code: *exit_code,
                        stderr: stderr.clone(),
                    },
                    ToolError::SandboxViolation { tool, attempted } => {
                        ToolError::SandboxViolation {
                            tool: tool.clone(),
                            attempted: attempted.clone(),
                        }
                    }
                    other => {
                        return Err(ToolError::ExecutionFailed {
                            tool: self.name.clone(),
                            exit_code: -1,
                            stderr: format!("synth: {other:?}"),
                        })
                    }
                });
            }
            // Build a real StagedBatch lazily against the test's
            // workspace tempdir. Each test that uses `with_staged`
            // creates a workspace via `ctx_in_workspace`; this stages
            // a one-byte file per path, returns the batch un-committed
            // so the dispatcher can drive commit_all (AutoApprove) or
            // commit_selected (AwaitApproval).
            let staged_writes = if let Some(paths) = &self.staged_paths {
                let check = crate::staging::NoopSyntaxCheck;
                let mut s = crate::staging::Staging::new(ctx.workspace_root, &check);
                for p in paths {
                    s.add(crate::staging::StagedWrite::new(p.clone(), "x".to_string()))
                        .map_err(|e| ToolError::ExecutionFailed {
                            tool: self.name.clone(),
                            exit_code: -1,
                            stderr: format!("synth staging add failed: {e}"),
                        })?;
                }
                let batch = s.stage().map_err(|e| ToolError::ExecutionFailed {
                    tool: self.name.clone(),
                    exit_code: -1,
                    stderr: format!("synth staging stage failed: {e}"),
                })?;
                Some(batch)
            } else {
                None
            };
            Ok(ToolResult {
                output: args,
                staged_writes,
            })
        }
    }

    fn ctx(policy: &SandboxPolicy) -> ToolContext<'_> {
        ToolContext {
            workspace_root: Path::new("/repo"),
            sandbox: policy,
        }
    }

    /// Like `ctx`, but pins `workspace_root` to a real on-disk path —
    /// required by any test that exercises `EchoTool::with_staged` now
    /// that the staged path produces a real `StagedBatch` over the
    /// workspace's filesystem.
    fn ctx_in_workspace<'a>(workspace: &'a Path, policy: &'a SandboxPolicy) -> ToolContext<'a> {
        ToolContext {
            workspace_root: workspace,
            sandbox: policy,
        }
    }

    fn now() -> String {
        "2026-05-16T10:00:00Z".into()
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCallRequest {
        ToolCallRequest {
            id: format!("tc-{name}"),
            name: name.into(),
            arguments: args,
        }
    }

    // ---------- SideEffectClass ----------

    #[test]
    fn side_effect_budget_costs_match_spec() {
        assert_eq!(SideEffectClass::LocalSafe.budget_cost(), 0);
        assert_eq!(SideEffectClass::LocalRisky.budget_cost(), 1);
        assert_eq!(SideEffectClass::SharedState.budget_cost(), 20);
        assert_eq!(SideEffectClass::Irreversible.budget_cost(), 20);
        assert!(SideEffectClass::Irreversible.requires_double_confirm());
        assert!(!SideEffectClass::SharedState.requires_double_confirm());
    }

    #[test]
    fn side_effect_serialises_as_schema_kebab_case() {
        for (lit, c) in [
            ("local-safe", SideEffectClass::LocalSafe),
            ("local-risky", SideEffectClass::LocalRisky),
            ("shared-state", SideEffectClass::SharedState),
            ("irreversible", SideEffectClass::Irreversible),
        ] {
            assert_eq!(serde_json::to_string(&c).unwrap(), format!("\"{lit}\""));
        }
    }

    // ---------- ToolRegistry ----------

    #[test]
    fn register_then_get_round_trips() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("echo"))).unwrap();
        assert_eq!(r.len(), 1);
        let t = r.get("echo").unwrap();
        assert_eq!(t.name(), "echo");
    }

    #[test]
    fn register_rejects_duplicate_names() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("echo"))).unwrap();
        let err = r.register(Arc::new(EchoTool::new("echo"))).unwrap_err();
        assert!(matches!(err, RegisterError::DuplicateName(n) if n == "echo"));
    }

    #[test]
    fn registry_iter_is_sorted_by_name() {
        let mut r = ToolRegistry::new();
        for name in ["zeta", "alpha", "mu"] {
            r.register(Arc::new(EchoTool::new(name))).unwrap();
        }
        let names: Vec<_> = r.names().collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    // ---------- Dispatcher: happy path ----------

    #[tokio::test]
    async fn dispatch_known_tool_returns_ok_outcome_and_ledger_entry() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("echo"))).unwrap();
        let d = Dispatcher::new(r, HookSet::empty());

        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = d
            .dispatch(&call("echo", json!({"x": 1})), &ctx(&policy), now)
            .await;

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.tool_call_id, "tc-echo");
        assert_eq!(outcome.tool_name, "echo");
        assert_eq!(outcome.events.len(), 0);
        // Ledger entry is always present.
        match &outcome.ledger_entry {
            LedgerEntry::ToolCall {
                tool_name,
                latency_ms,
                note,
                cost_usd,
                ..
            } => {
                assert_eq!(tool_name, "echo");
                assert!(*latency_ms >= 0.0);
                assert!(note.is_none(), "no error → no note");
                assert!(cost_usd.is_some());
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_tool_with_staged_writes_emits_edit_staged_per_file() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(
            EchoTool::new("write_things").with_staged(vec!["a.rs", "src/b.rs"]),
        ))
        .unwrap();
        let d = Dispatcher::new(r, HookSet::empty());
        let workspace = tempfile::TempDir::new().unwrap();
        let policy = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let outcome = d
            .dispatch(
                &call("write_things", json!({})),
                &ctx_in_workspace(workspace.path(), &policy),
                now,
            )
            .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(outcome.events.len(), 2);
        let paths: Vec<_> = outcome
            .events
            .iter()
            .map(|e| match e {
                Event::EditStaged { path, .. } => path.to_string_lossy().into_owned(),
                other => panic!("expected EditStaged, got {other:?}"),
            })
            .collect();
        assert_eq!(paths, vec!["a.rs", "src/b.rs"]);
        // The auto-commit actually wrote the files to the workspace.
        assert!(workspace.path().join("a.rs").exists());
        assert!(workspace.path().join("src/b.rs").exists());
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_fails_closed_with_execution_failed() {
        let d = Dispatcher::new(ToolRegistry::new(), HookSet::empty());
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = d
            .dispatch(&call("ghost", json!({})), &ctx(&policy), now)
            .await;
        match outcome.result {
            Err(ToolError::ExecutionFailed { tool, .. }) => assert_eq!(tool, "ghost"),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
        // Ledger entry still recorded — failed dispatches count.
        assert!(matches!(outcome.ledger_entry, LedgerEntry::ToolCall { .. }));
        // No EditStaged events.
        assert!(outcome.events.is_empty());
        // No hooks (no tool to match).
        assert!(outcome.matched_hooks.pre_tool.is_empty());
    }

    #[tokio::test]
    async fn dispatch_failed_tool_records_error_in_ledger_note() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("flaky").with_error(
            ToolError::ExecutionFailed {
                tool: "flaky".into(),
                exit_code: 1,
                stderr: "boom".into(),
            },
        )))
        .unwrap();
        let d = Dispatcher::new(r, HookSet::empty());
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = d
            .dispatch(&call("flaky", json!({})), &ctx(&policy), now)
            .await;
        assert!(outcome.result.is_err());
        match outcome.ledger_entry {
            LedgerEntry::ToolCall { note, .. } => {
                let note = note.expect("note should be present on failure");
                assert!(note.contains("ExecutionFailed"));
                assert!(note.contains("boom"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    // ---------- Dispatcher: hook identification ----------

    fn pre_tool_hook(name: &str, filter: Option<Vec<String>>) -> HookManifest {
        let json = serde_json::json!({
            "version": 1,
            "name": name,
            "event": "pre-tool",
            "tool_filter": filter,
            "implementation": {"kind": "shell", "command": "echo"},
            "time_budget_ms": 50,
        });
        serde_json::from_value(json).unwrap()
    }

    fn post_tool_hook(name: &str) -> HookManifest {
        let json = serde_json::json!({
            "version": 1,
            "name": name,
            "event": "post-tool",
            "implementation": {"kind": "shell", "command": "echo"},
            "time_budget_ms": 50,
        });
        serde_json::from_value(json).unwrap()
    }

    fn hook_set(manifests: Vec<HookManifest>) -> HookSet {
        let tmp = tempfile::TempDir::new().unwrap();
        for m in manifests {
            let path = tmp.path().join(format!("{}.json", m.name));
            std::fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
        }
        HookSet::load_dir(tmp.path()).unwrap()
    }

    #[tokio::test]
    async fn dispatch_identifies_applicable_pre_and_post_hooks() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("echo"))).unwrap();
        let hs = hook_set(vec![
            pre_tool_hook("lint_pre", None),
            post_tool_hook("audit_post"),
        ]);
        let d = Dispatcher::new(r, hs);
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = d
            .dispatch(&call("echo", json!({})), &ctx(&policy), now)
            .await;
        assert_eq!(outcome.matched_hooks.pre_tool, vec!["lint_pre".to_string()]);
        assert_eq!(
            outcome.matched_hooks.post_tool,
            vec!["audit_post".to_string()]
        );
    }

    #[tokio::test]
    async fn dispatch_skips_hooks_with_non_matching_tool_filter() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("echo"))).unwrap();
        let hs = hook_set(vec![
            pre_tool_hook("lint_write", Some(vec!["write_*".into()])),
            pre_tool_hook("lint_all", None),
        ]);
        let d = Dispatcher::new(r, hs);
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = d
            .dispatch(&call("echo", json!({})), &ctx(&policy), now)
            .await;
        // Only `lint_all` matches — `lint_write` filters to write_*.
        assert_eq!(outcome.matched_hooks.pre_tool, vec!["lint_all".to_string()]);
    }

    // ---------- HookExecutor placeholder ----------

    // Recording executor: captures every call so a test can assert the
    // pre/post lifecycle actually ran the hooks (vs. just identifying them).
    #[derive(Default)]
    struct RecordingExecutor {
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl RecordingExecutor {
        fn snapshot(&self) -> Vec<(String, serde_json::Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HookExecutor for RecordingExecutor {
        async fn execute(&self, manifest: &HookManifest, payload: &serde_json::Value) {
            self.calls
                .lock()
                .unwrap()
                .push((manifest.name.clone(), payload.clone()));
        }
    }

    // A tool whose validate_args always rejects — proves the dispatcher
    // gate fires before execute / hooks. MCP-routed tools and any future
    // built-in whose constraints exceed serde's reach will use this seam.
    struct GatedTool;
    #[async_trait]
    impl Tool for GatedTool {
        fn name(&self) -> &str {
            "gated"
        }
        fn side_effect_class(&self) -> SideEffectClass {
            SideEffectClass::LocalSafe
        }
        fn validate_args(&self, _args: &serde_json::Value) -> Result<(), String> {
            Err("forbidden by tool-specific input schema".into())
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            panic!("execute must NOT run when validate_args returns Err");
        }
    }

    #[tokio::test]
    async fn dispatch_validate_args_failure_short_circuits_execute_and_hooks() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(GatedTool)).unwrap();
        let hs = hook_set(vec![
            pre_tool_hook("audit_pre", None),
            post_tool_hook("audit_post"),
        ]);
        let recorder = Arc::new(RecordingExecutor::default());
        let d = Dispatcher::new(r, hs).with_executor(recorder.clone() as Arc<dyn HookExecutor>);
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = d
            .dispatch(&call("gated", json!({"x": 1})), &ctx(&policy), now)
            .await;
        match outcome.result {
            Err(ToolError::SchemaViolation { tool, error }) => {
                assert_eq!(tool, "gated");
                assert!(error.contains("forbidden"));
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
        // Hooks did NOT fire — validation is the gate.
        assert!(recorder.snapshot().is_empty());
        // Ledger entry still recorded with the SchemaViolation note.
        match outcome.ledger_entry {
            LedgerEntry::ToolCall { note, .. } => {
                let n = note.unwrap();
                assert!(n.starts_with("SchemaViolation:"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_actually_invokes_pre_and_post_hooks_in_order() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("echo"))).unwrap();
        let hs = hook_set(vec![
            pre_tool_hook("audit_pre", None),
            post_tool_hook("audit_post"),
        ]);
        let recorder = Arc::new(RecordingExecutor::default());
        let d = Dispatcher::new(r, hs).with_executor(recorder.clone() as Arc<dyn HookExecutor>);

        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        d.dispatch(&call("echo", json!({"x": 1})), &ctx(&policy), now)
            .await;

        let snap = recorder.snapshot();
        assert_eq!(snap.len(), 2, "both pre and post should fire");
        assert_eq!(snap[0].0, "audit_pre");
        assert_eq!(snap[0].1["event"], "pre-tool");
        assert_eq!(snap[0].1["tool_name"], "echo");
        assert_eq!(snap[0].1["tool_call_id"], "tc-echo");
        assert_eq!(snap[0].1["arguments"], json!({"x": 1}));
        assert_eq!(snap[1].0, "audit_post");
        assert_eq!(snap[1].1["event"], "post-tool");
        assert_eq!(snap[1].1["ok"], true);
    }

    #[tokio::test]
    async fn dispatch_post_hook_payload_reflects_failure() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool::new("flaky").with_error(
            ToolError::ExecutionFailed {
                tool: "flaky".into(),
                exit_code: 2,
                stderr: "nope".into(),
            },
        )))
        .unwrap();
        let hs = hook_set(vec![post_tool_hook("audit_post")]);
        let recorder = Arc::new(RecordingExecutor::default());
        let d = Dispatcher::new(r, hs).with_executor(recorder.clone() as Arc<dyn HookExecutor>);

        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        d.dispatch(&call("flaky", json!({})), &ctx(&policy), now)
            .await;

        let snap = recorder.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].1["ok"], false);
        assert_eq!(snap[0].1["error_kind"], "ExecutionFailed");
    }

    #[tokio::test]
    async fn dispatch_skips_hooks_for_unknown_tool() {
        let hs = hook_set(vec![pre_tool_hook("audit_pre", None)]);
        let recorder = Arc::new(RecordingExecutor::default());
        let d = Dispatcher::new(ToolRegistry::new(), hs)
            .with_executor(recorder.clone() as Arc<dyn HookExecutor>);
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        d.dispatch(&call("ghost", json!({})), &ctx(&policy), now)
            .await;
        // Unknown tool short-circuits before the hook phase.
        assert!(recorder.snapshot().is_empty());
    }

    #[tokio::test]
    async fn noop_hook_executor_is_silent() {
        let exec = NoopHookExecutor;
        let m = pre_tool_hook("x", None);
        // Just exercises the trait object path.
        let dyn_exec: &dyn HookExecutor = &exec;
        dyn_exec.execute(&m, &json!({})).await;
    }

    // ---------- SessionDispatcher (wired path) ----------

    use crate::ledger::{Ledger, LedgerEntry};
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    fn build_session_dispatcher(
        tools: Vec<Arc<dyn Tool>>,
    ) -> (SessionDispatcher, Arc<Ledger>, broadcast::Receiver<Event>) {
        let mut registry = ToolRegistry::new();
        for t in tools {
            registry.register(t).unwrap();
        }
        let dispatcher = Dispatcher::new(registry, HookSet::empty());
        let ledger = Arc::new(Ledger::new());
        let (tx, rx) = broadcast::channel(64);
        let sd = SessionDispatcher::new(dispatcher, ledger.clone(), tx);
        (sd, ledger, rx)
    }

    async fn next_event(rx: &mut broadcast::Receiver<Event>) -> Event {
        timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("event within 1s")
            .expect("channel still open")
    }

    #[tokio::test]
    async fn session_dispatcher_appends_ledger_entry_on_success() {
        let (sd, ledger, _rx) = build_session_dispatcher(vec![Arc::new(EchoTool::new("echo"))]);
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = sd
            .dispatch(&call("echo", json!({"x": 1})), &ctx(&policy), now)
            .await;
        assert!(outcome.result.is_ok());
        // Ledger has one tool-call entry.
        assert_eq!(ledger.len(), 1);
        let entries = ledger.to_vec();
        assert!(matches!(
            entries[0],
            LedgerEntry::ToolCall { ref tool_name, .. } if tool_name == "echo"
        ));
    }

    #[tokio::test]
    async fn session_dispatcher_broadcasts_edit_staged_for_writes() {
        let tool = Arc::new(EchoTool::new("write_things").with_staged(vec!["a.rs", "b.rs"]));
        let (sd, _ledger, mut rx) = build_session_dispatcher(vec![tool]);
        let workspace = tempfile::TempDir::new().unwrap();
        let policy = SandboxPolicy::restrictive(workspace.path()).unwrap();
        sd.dispatch(
            &call("write_things", json!({})),
            &ctx_in_workspace(workspace.path(), &policy),
            now,
        )
        .await;

        // v49 ordering: EditStaged per file → LedgerAppended →
        // CommitDecision. A UI rendering both a diff pane and a cost
        // meter sees the diff arrive first; the summary lands last so
        // any pending-state clear happens after the file events.
        // This test locks the ordering against regression.
        match next_event(&mut rx).await {
            Event::EditStaged { path, .. } => assert_eq!(path.to_str(), Some("a.rs")),
            other => panic!("expected EditStaged, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::EditStaged { path, .. } => assert_eq!(path.to_str(), Some("b.rs")),
            other => panic!("expected EditStaged, got {other:?}"),
        }
        // Then the LedgerAppended for the tool call itself.
        match next_event(&mut rx).await {
            Event::LedgerAppended { .. } => {}
            other => panic!("expected LedgerAppended, got {other:?}"),
        }
        // Then the CommitDecision summary (v49). Under AutoApproveAll
        // (the default for this test), `committed` lists every file
        // and `dropped` is empty.
        match next_event(&mut rx).await {
            Event::CommitDecision {
                committed, dropped, ..
            } => {
                assert_eq!(
                    committed,
                    vec![
                        std::path::PathBuf::from("a.rs"),
                        std::path::PathBuf::from("b.rs"),
                    ]
                );
                assert!(dropped.is_empty(), "AutoApprove leaves nothing dropped");
            }
            other => panic!("expected CommitDecision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_dispatcher_ledgers_failed_calls_and_broadcasts_ledger_only() {
        let failing = Arc::new(
            EchoTool::new("flaky").with_error(ToolError::ExecutionFailed {
                tool: "flaky".into(),
                exit_code: 2,
                stderr: "nope".into(),
            }),
        );
        let (sd, ledger, mut rx) = build_session_dispatcher(vec![failing]);
        let policy = SandboxPolicy::restrictive("/repo").unwrap();
        let outcome = sd
            .dispatch(&call("flaky", json!({})), &ctx(&policy), now)
            .await;
        assert!(outcome.result.is_err());
        // Ledger entry recorded with the error note.
        assert_eq!(ledger.len(), 1);
        match &ledger.to_vec()[0] {
            LedgerEntry::ToolCall { note, .. } => {
                let note = note.as_deref().unwrap_or("");
                assert!(note.contains("ExecutionFailed"));
                assert!(note.contains("nope"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        // LedgerAppended IS emitted even for failures (cost meter must
        // count the failed call against the trust budget — spec §1
        // doesn't carve out a "free failure" path), but EditStaged is
        // NOT (the call produced no staged writes).
        match next_event(&mut rx).await {
            Event::LedgerAppended { entry } => match entry {
                LedgerEntry::ToolCall { note, .. } => {
                    assert!(note.as_deref().unwrap_or("").contains("ExecutionFailed"));
                }
                other => panic!("expected ToolCall ledger entry, got {other:?}"),
            },
            other => panic!("expected LedgerAppended, got {other:?}"),
        }
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "no further events after the LedgerAppended"
        );
    }

    #[tokio::test]
    async fn session_dispatcher_sends_without_subscribers_is_silent_not_fatal() {
        // broadcast::Sender::send returns Err when there are no
        // subscribers — SessionDispatcher must ignore that so a
        // headless run with no UI doesn't surface dispatcher errors.
        let tool = Arc::new(EchoTool::new("write_things").with_staged(vec!["a.rs"]));
        let (sd, ledger, rx) = build_session_dispatcher(vec![tool]);
        drop(rx); // remove the only subscriber
        let workspace = tempfile::TempDir::new().unwrap();
        let policy = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let outcome = sd
            .dispatch(
                &call("write_things", json!({})),
                &ctx_in_workspace(workspace.path(), &policy),
                now,
            )
            .await;
        assert!(
            outcome.result.is_ok(),
            "dispatch must succeed even with no subscribers"
        );
        assert_eq!(ledger.len(), 1);
    }

    #[tokio::test]
    async fn session_dispatcher_exposes_inner_dispatcher_for_introspection() {
        let (sd, _ledger, _rx) = build_session_dispatcher(vec![Arc::new(EchoTool::new("echo"))]);
        // Round-trip through the accessor.
        assert_eq!(sd.dispatcher().registry().len(), 1);
    }

    // ---------- HR-D: ApprovalPolicy round-trip ----------

    #[tokio::test]
    async fn await_approval_emits_pending_event_and_blocks_until_submit() {
        let tool =
            Arc::new(EchoTool::new("write_things").with_staged(vec!["accept.txt", "reject.txt"]));
        let (sd, _ledger, mut rx) = build_session_dispatcher(vec![tool]);
        let sd = Arc::new(sd.with_approval_policy(ApprovalPolicy::AwaitApproval));
        let workspace = tempfile::TempDir::new().unwrap();
        let policy = SandboxPolicy::restrictive(workspace.path()).unwrap();

        // Drive dispatch concurrently with the consumer that watches
        // the bus for StagingPendingApproval and submits an accept set.
        let sd_dispatch = sd.clone();
        let ws_path = workspace.path().to_path_buf();
        let dispatch_task = tokio::spawn(async move {
            let p = SandboxPolicy::restrictive(&ws_path).unwrap();
            sd_dispatch
                .dispatch(
                    &call("write_things", json!({})),
                    &ctx_in_workspace(&ws_path, &p),
                    now,
                )
                .await
        });

        // Consumer loop: pull events, accept only "accept.txt".
        let _ = policy; // unused locally — kept above for the sd_dispatch task
        let commit_id = loop {
            match next_event(&mut rx).await {
                Event::StagingPendingApproval {
                    commit_id: cid,
                    files,
                } => {
                    assert_eq!(files.len(), 2, "two pending files");
                    let accepted = vec![std::path::PathBuf::from("accept.txt")];
                    assert!(
                        sd.submit_approval(cid, accepted),
                        "submit_approval should hit a registered pending"
                    );
                    break cid;
                }
                _ => continue,
            }
        };

        let outcome = dispatch_task.await.unwrap();
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);

        // Verify EditStaged + CommitDecision were emitted.
        let mut got_edit_for_accept = false;
        let mut got_decision = false;
        loop {
            match rx.try_recv() {
                Ok(Event::EditStaged { path, .. }) => {
                    if path.to_str() == Some("accept.txt") {
                        got_edit_for_accept = true;
                    }
                }
                Ok(Event::CommitDecision {
                    commit_id: cid,
                    committed,
                    dropped,
                }) => {
                    assert_eq!(cid, commit_id);
                    assert_eq!(committed, vec![std::path::PathBuf::from("accept.txt")]);
                    assert_eq!(dropped, vec![std::path::PathBuf::from("reject.txt")]);
                    got_decision = true;
                }
                Ok(_) => continue, // LedgerAppended etc.
                Err(_) => break,
            }
        }
        assert!(got_edit_for_accept, "EditStaged for accept.txt missing");
        assert!(got_decision, "CommitDecision missing");

        // Filesystem: accept.txt landed, reject.txt did not.
        assert!(workspace.path().join("accept.txt").exists());
        assert!(!workspace.path().join("reject.txt").exists());
    }

    #[tokio::test]
    async fn await_approval_full_reject_drops_everything() {
        let tool = Arc::new(EchoTool::new("write_things").with_staged(vec!["a.txt", "b.txt"]));
        let (sd, _ledger, mut rx) = build_session_dispatcher(vec![tool]);
        let sd = Arc::new(sd.with_approval_policy(ApprovalPolicy::AwaitApproval));
        let workspace = tempfile::TempDir::new().unwrap();

        let sd_dispatch = sd.clone();
        let ws_path = workspace.path().to_path_buf();
        let dispatch_task = tokio::spawn(async move {
            let p = SandboxPolicy::restrictive(&ws_path).unwrap();
            sd_dispatch
                .dispatch(
                    &call("write_things", json!({})),
                    &ctx_in_workspace(&ws_path, &p),
                    now,
                )
                .await
        });

        loop {
            match next_event(&mut rx).await {
                Event::StagingPendingApproval { commit_id, .. } => {
                    // Empty accept set = full reject.
                    sd.submit_approval(commit_id, Vec::new());
                    break;
                }
                _ => continue,
            }
        }
        let outcome = dispatch_task.await.unwrap();
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert!(
            outcome.events.is_empty(),
            "no EditStaged for rejected files"
        );
        assert!(!workspace.path().join("a.txt").exists());
        assert!(!workspace.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn submit_approval_for_unknown_commit_id_returns_false() {
        let (sd, _ledger, _rx) = build_session_dispatcher(vec![Arc::new(EchoTool::new("echo"))]);
        let sd = sd.with_approval_policy(ApprovalPolicy::AwaitApproval);
        assert!(!sd.submit_approval(uuid::Uuid::new_v4(), Vec::new()));
    }
}
