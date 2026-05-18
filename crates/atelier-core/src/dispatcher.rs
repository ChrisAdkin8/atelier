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
/// As of v46 the tool hands back a [`crate::staging::StagedBatch`]
/// (validated + staged on disk, NOT renamed). The dispatcher's
/// [`ApprovalPolicy`] decides
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
///
/// ## §11 / §12 audit log seam
///
/// `tool_call_id` is `None` when the caller built the `ToolContext`
/// directly (e.g. unit tests that bypass `Dispatcher::dispatch`).
/// `Dispatcher::dispatch` always overrides it with the live
/// `ToolCallRequest::id` before handing the context to `Tool::execute`,
/// so production tool impls can rely on it being `Some`. The shell tool
/// uses it to label the §11 egress audit row.
///
/// `audit_log_path`, when `Some`, points at the session's `audit.log`
/// (NDJSON, one event per line). Tools that detect a §11 enforcement
/// (egress block today; over-scope FS write tomorrow) append a row via
/// `crate::audit::append_subprocess_egress`. `None` disables the
/// producer — used by dispatcher-level unit tests that don't care about
/// audit side-effects, and by older tests that pre-date this seam.
pub struct ToolContext<'a> {
    pub workspace_root: &'a Path,
    pub sandbox: &'a SandboxPolicy,
    /// Live tool-call id (set by `Dispatcher::dispatch` per call).
    pub tool_call_id: Option<&'a str>,
    /// Per-session audit-log path. See type-level docs.
    pub audit_log_path: Option<&'a Path>,
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

/// v60.5 — error returned by `SessionDispatcher::compact_context_items`.
///
/// Compaction crosses the §5 Context and Memory subsystems, so a single
/// domain-specific error type wouldn't carry the right shape; this enum
/// keeps the underlying `ContextError` / `MemoryError` reachable for
/// callers that want to render a specific message and provides the
/// dispatcher-level cases on top.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompactionError {
    #[error("compact_context_items: refusing to compact an empty selection")]
    Empty,

    #[error("compact_context_items: summary text rejected: {0}")]
    InvalidSummary(String),

    #[error("compact_context_items: context: {0}")]
    Context(#[from] crate::context::ContextError),

    #[error("compact_context_items: memory: {0}")]
    Memory(#[from] crate::memory::MemoryError),
}

/// v60.5 — value returned by a successful
/// `SessionDispatcher::compact_context_items` call. The caller (typically
/// the `atelier_cli::compaction` orchestrator) uses these fields to
/// build the UI-side toast ("compacted 7 items, freed ~12.3k tokens").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOutput {
    /// `mem-<uuid>` of the freshly-created summary card. Always pinned
    /// by construction; carries a `CompactionSource` linking back to
    /// the replaced items + the expansion blob.
    pub summary_card_id: String,
    /// Sum of `tokens` across the evicted items (as reported by
    /// `CacheBustEvent::tokens_freed`).
    pub freed_tokens: u32,
}

/// v60.6 — error returned by `SessionDispatcher::expand_memory_card`.
/// Wraps the layered failures so the caller (the `atelier_cli::expansion`
/// orchestrator) can render a precise message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExpansionError {
    #[error("expand_memory_card: memory card {0:?} not found")]
    CardNotFound(String),

    #[error("expand_memory_card: card {0:?} is not a compaction summary (missing compacted_from)")]
    NotACompactionCard(String),

    #[error(
        "expand_memory_card: blob items do not match the card's compacted_from.item_ids \
         (expected {expected} ids, got {got})"
    )]
    ItemMismatch { expected: usize, got: usize },

    #[error(
        "expand_memory_card: blob item id {got:?} at position {position} does not match \
         compacted_from.item_ids[{position}] = {expected:?}"
    )]
    ItemIdMismatch {
        position: usize,
        expected: String,
        got: String,
    },

    #[error("expand_memory_card: context: {0}")]
    Context(#[from] crate::context::ContextError),

    #[error("expand_memory_card: memory: {0}")]
    Memory(#[from] crate::memory::MemoryError),
}

/// v60.6 — value returned by a successful
/// `SessionDispatcher::expand_memory_card` call. Drives the UI toast
/// ("restored 5 items, paid ~240 cache tokens"). Mirror of
/// [`CompactionOutput`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionOutput {
    /// Number of `ContextItem`s restored to the manager.
    pub restored_item_count: usize,
    /// Id of the summary `MemoryCard` that was dropped.
    pub summary_card_id: String,
    /// Prompt-cache rewarm cost the user paid (sum of `tokens.count`
    /// across the restored items). Matches the `cache_rewarm_tokens`
    /// stored in the now-gone `CompactionSource`.
    pub cache_rewarm_tokens: u32,
}

/// Spec §3 hunk accept/reject contract.
///
/// Called by [`Dispatcher::dispatch`] between staging and commit when a
/// tool produced [`crate::staging::StagedBatch`]. The gate decides which
/// files commit — auto-approve all (the default [`AutoApprove`]
/// behaviour, identical to v45), or block on a user decision routed
/// through the broadcast bus (the production
/// the module-private `PendingApprovalGate` used by [`SessionDispatcher`] when its
/// policy is [`ApprovalPolicy::AwaitApproval`]).
///
/// The trait is async because real implementations wait on a `oneshot`
/// channel; the trivial impl returns instantly.
///
/// v56: return type widened from `Vec<PathBuf>` to
/// [`crate::staging::HunkSelection`] so the gate can carry per-hunk
/// decisions through to `commit_selected_hunks`.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Decide which of `pending` to commit and at what granularity.
    /// Paths absent from the returned `HunkSelection` are fully
    /// rejected. `commit_id` is the correlation token the
    /// implementation may use for round-tripping over the bus.
    async fn approve(
        &self,
        commit_id: uuid::Uuid,
        pending: &[crate::staging::FileOutcome],
    ) -> crate::staging::HunkSelection;

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
    ) -> crate::staging::HunkSelection {
        pending
            .iter()
            .map(|f| (f.path.clone(), crate::staging::FileApproval::All))
            .collect()
    }
}

/// Stateful dispatcher composing a [`ToolRegistry`], a [`HookSet`], the
/// [`HookExecutor`] that runs hooks at the per-call lifecycle boundaries
/// (pre-tool + post-tool), and the [`ApprovalGate`] that decides which
/// staged writes commit (spec §3 hunk accept/reject). Defaults are
/// [`NoopHookExecutor`] + [`AutoApprove`]; production wires in
/// [`ShellHookExecutor`] + the module-private `PendingApprovalGate` via the builder
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
    /// the module-private `PendingApprovalGate` and threads it in.
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
        //
        //    Thread the live `ToolCallRequest::id` onto a per-call clone
        //    of the caller's `ToolContext` so the §11 egress audit
        //    producer (in `tools/shell.rs`) can label rows with the
        //    originating tool_call_id. The caller's `tool_call_id`
        //    field is ignored — `Dispatcher::dispatch` is the single
        //    source of truth for that id, since it's the layer that
        //    sees the `ToolCallRequest` directly.
        let per_call_ctx = ToolContext {
            workspace_root: ctx.workspace_root,
            sandbox: ctx.sandbox,
            tool_call_id: Some(call.id.as_str()),
            audit_log_path: ctx.audit_log_path,
        };
        let raw_result = tool.execute(call.arguments.clone(), &per_call_ctx).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        let cost_usd = Some(local_cost_usd(latency_ms, DEFAULT_LOCAL_RATE_USD_PER_SEC));

        // 4. Stage → approval gate → commit_selected → events.
        //
        //    The pure `Dispatcher` invokes its `approval_gate` between
        //    stage and commit. The default [`AutoApprove`] returns
        //    every pending path so behaviour is identical to pre-v46.
        //    `SessionDispatcher` installs a the module-private `PendingApprovalGate`
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
                    let selection = self
                        .approval_gate
                        .approve(commit_id, batch.pending_files())
                        .await;
                    match batch.commit_selected_hunks(&selection) {
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

/// v61 — §14 concurrent-edit policy. Distinct axis from [`ApprovalPolicy`]
/// (which gates *staging*); this one gates how the runner reacts when
/// the file-watcher reports an external edit mid-turn.
///
/// `Modal` is the interactive default: queue the next dispatch, surface
/// `Event::FilesChanged`, wait for a user decision (or the 5-min
/// auto-pause timer to fire). `AutoReload` is `--non-interactive`'s
/// answer — the runner immediately treats the change as a Reload
/// without surfacing a modal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConcurrentEditPolicy {
    /// Interactive. Surface `Event::FilesChanged`; await the user's
    /// choice (Reload / Wait / Pause). Default for GUI / TUI driver
    /// modes and `atelier run` without `--non-interactive`.
    #[default]
    Modal,
    /// Headless. Auto-resolve every file-watch event as Reload; emit
    /// `Event::FilesChangedAcknowledged { outcome: AutoReload }`
    /// straight away. Set by `--non-interactive`.
    AutoReload,
}

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
                tokio::sync::oneshot::Sender<crate::staging::HunkSelection>,
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
    ) -> crate::staging::HunkSelection {
        // Register a oneshot and emit the bus event. The consumer
        // calls SessionDispatcher::submit_approval(commit_id, selection)
        // which fulfils the oneshot, unblocking this await.
        //
        // v57 (H1 fix) — a `PendingEntryGuard` holds the
        // `pending`-map lifetime so a cancelled dispatch future
        // (caller times out, GUI tab closes, etc.) removes the
        // entry on drop. Without the guard, the `tx` lingered in
        // the HashMap forever and a long-running session with
        // frequent cancellations grew unboundedly.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(commit_id, tx);
        let _guard = PendingEntryGuard {
            pending: self.pending.clone(),
            commit_id,
        };

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
        // committing without user consent. The guard drops here too
        // — on the success path the entry was already removed by
        // `submit_approval`, so the second `remove` is a no-op.
        rx.await.unwrap_or_default()
    }
}

/// v57 — drop guard for the `PendingApprovalGate.pending` HashMap.
/// On any exit from `approve` (success, failure, cancellation,
/// panic) the entry is removed so a stale `oneshot::Sender` can't
/// linger. `submit_approval` on the success path removes the entry
/// first; the guard's drop becomes a no-op.
struct PendingEntryGuard {
    pending: Arc<
        parking_lot::Mutex<
            std::collections::HashMap<
                uuid::Uuid,
                tokio::sync::oneshot::Sender<crate::staging::HunkSelection>,
            >,
        >,
    >,
    commit_id: uuid::Uuid,
}

impl Drop for PendingEntryGuard {
    fn drop(&mut self) {
        let _ = self.pending.lock().remove(&self.commit_id);
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
/// v57 (M-sec-4) / v58 (L-sec-2) — substring keys that almost always
/// carry secrets. A value whose key contains any of these (ASCII
/// case-insensitive) is replaced with `"<redacted>"` before the
/// payload lands in the `ATELIER_HOOK_PAYLOAD` env var.
///
/// The list is deliberately broad — false-positive redactions cost a
/// hook a useful debug payload, false-negatives cost a user-pasted
/// API key.
///
/// v58 additions cover AWS access keys (matched via `access_key`),
/// GitHub PATs (`_pat`), private keys (`private_key`), bearer
/// tokens (`bearer`), and HTTP cookies / session cookies (`cookie`).
const SECRET_KEY_SUBSTRINGS: &[&str] = &[
    "api_key",
    "apikey",
    "access_key",
    "private_key",
    "secret",
    "password",
    "passwd",
    "token",
    "authorization",
    "auth_token",
    "bearer",
    "session_id",
    "credentials",
    "cookie",
    "_pat",
];

fn key_looks_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_SUBSTRINGS.iter().any(|s| lower.contains(s))
}

/// Walk `payload` and replace the value of any key whose name matches
/// [`SECRET_KEY_SUBSTRINGS`] with `"<redacted>"`. Non-object values are
/// returned unchanged. Arrays are recursed into so a nested
/// `{"headers":[{"name":"authorization","value":"Bearer X"}]}` still
/// gets its `value` masked when the matching name suggests a secret.
fn redact_secrets(payload: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match payload {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if key_looks_secret(k) {
                    out.insert(k.clone(), Value::String("<redacted>".into()));
                } else {
                    out.insert(k.clone(), redact_secrets(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_secrets).collect()),
        other => other.clone(),
    }
}

/// v60 (MED-A fix) — memory-card content validator. Thin wrapper
/// over `crate::text_safety::validate_user_text` (with frontmatter
/// check on) so the rule set lives in one place across the live
/// add path, `MemoryStore::from_vec`, the plan path, and any future
/// free-form-text consumer.
fn validate_memory_card_content(content: &str) -> Result<(), crate::memory::MemoryError> {
    crate::text_safety::validate_user_text(content, /* check_frontmatter */ true)
        .map_err(crate::memory::MemoryError::InvalidContent)
}

/// v59 (MED-sec-2) / v60 (MED-A fix) — plan-step text validator.
/// Skips the frontmatter check (plan steps aren't promoted to
/// markdown) but otherwise shares the rule set via
/// [`crate::text_safety::validate_user_text`].
pub(crate) fn validate_plan_text(text: &str) -> Result<(), crate::plan::PlanError> {
    crate::text_safety::validate_user_text(text, /* check_frontmatter */ false)
        .map_err(crate::plan::PlanError::InvalidContent)
}

/// v61 — extract the paths a read-only tool call touches so the §14
/// file watcher's read-set stays in sync with the model's actual
/// observations. Returns absolute paths (joined onto `workspace_root`
/// since the tools accept repo-relative inputs).
///
/// Only the built-in read tools are recognised: `read_file`,
/// `list_dir`, `grep`, `ast_grep`. Unknown tool names (MCP-routed or
/// not in the catalogue) return an empty vector; the watcher gracefully
/// degrades — we just don't track their reads.
fn extract_read_paths(
    tool_name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let resolve = |raw: &str| -> Option<PathBuf> {
        if raw.is_empty() {
            return None;
        }
        let p = std::path::Path::new(raw);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(workspace_root.join(p))
        }
    };
    match tool_name {
        "read_file" | "list_dir" | "ast_grep" => args
            .get("path")
            .and_then(|v| v.as_str())
            .and_then(resolve)
            .into_iter()
            .collect(),
        "grep" => {
            // `grep` accepts an optional `path` (default = workspace root)
            // plus the implicit recursion. Tracking just the root entry
            // is the right granularity — recursive watching would balloon
            // the read-set on a big repo. Spec §14 says the watcher
            // detects edits to files in the *read set*; for `grep`, the
            // read set is the directory the model targeted.
            args.get("path")
                .and_then(|v| v.as_str())
                .and_then(resolve)
                .or_else(|| Some(workspace_root.to_path_buf()))
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Parse a `ContextItemId` from its stringified UUID form
/// (`ContextItemSummary::id`).
///
/// v57 (L cleanup) — returns `ContextError::Malformed(id)` for
/// invalid UUIDs instead of `NotFound(nil-uuid)`. The pre-v57 nil
/// UUID rendered as "context item 00000000-0000-… not found" in the
/// UI for a simple typo — misleading.
fn parse_context_item_id(
    id: &str,
) -> Result<crate::context::ContextItemId, crate::context::ContextError> {
    uuid::Uuid::parse_str(id)
        .map(crate::context::ContextItemId)
        .map_err(|_| crate::context::ContextError::Malformed(id.to_string()))
}

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
                tokio::sync::oneshot::Sender<crate::staging::HunkSelection>,
            >,
        >,
    >,
    /// v55 — §5 shared state. The runner constructs these `Arc`s once
    /// and clones one set into the dispatcher via
    /// [`Self::with_shared_state`] so the UI's pin / unpin / evict /
    /// add-card / mark-step-done mutators land on the same store the
    /// runner reads at each turn boundary. `new` seeds each with a
    /// fresh empty instance so `SessionDispatcher` is constructible
    /// without the runner — unit tests still work.
    context_manager: Arc<parking_lot::Mutex<crate::context::ContextManager>>,
    memory_store: Arc<parking_lot::Mutex<crate::memory::MemoryStore>>,
    plan_canvas: Arc<parking_lot::Mutex<crate::plan::PlanCanvas>>,
    /// Phase C close — §5 mental-model state. Off by default; the
    /// runner does **not** inject this into the prompt in v0. UI
    /// mutators land here via [`Self::set_mental_model`]; the
    /// resulting [`Event::MentalModelSnapshot`] re-emits the
    /// enabled flag + approximate token count so subscribed UIs
    /// converge.
    mental_model: Arc<parking_lot::Mutex<crate::mental_model::MentalModel>>,
    /// v61 — §14 file watcher. Defaults to a no-op handle so callers
    /// (tests, GUI demo runs) that don't wire the watcher pay nothing.
    /// The Runner attaches a real handle via [`Self::with_file_watcher`].
    file_watcher: crate::file_watcher::FileWatcherHandle,
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
            context_manager: Arc::new(parking_lot::Mutex::new(
                crate::context::ContextManager::new(),
            )),
            memory_store: Arc::new(parking_lot::Mutex::new(crate::memory::MemoryStore::new())),
            plan_canvas: Arc::new(parking_lot::Mutex::new(crate::plan::PlanCanvas::new())),
            mental_model: Arc::new(parking_lot::Mutex::new(
                crate::mental_model::MentalModel::new(),
            )),
            file_watcher: crate::file_watcher::FileWatcherHandle::disabled(),
        }
    }

    /// v61 — attach a §14 file watcher. After each successful read-only
    /// tool dispatch (`read_file`, `list_dir`, `grep`, `ast_grep`), the
    /// dispatcher feeds the touched path into the watcher's read-set
    /// via [`crate::file_watcher::FileWatcherHandle::track`]. External
    /// edits to any tracked path surface as `Event::FilesChanged`.
    pub fn with_file_watcher(mut self, handle: crate::file_watcher::FileWatcherHandle) -> Self {
        self.file_watcher = handle;
        self
    }

    /// v55 — share the runner's §5 state with this dispatcher. The
    /// runner owns one set of `Arc`s and clones them into the
    /// dispatcher at construction; UI mutator methods on
    /// `SessionDispatcher` then mutate the same stores the runner
    /// re-emits from at each turn boundary.
    pub fn with_shared_state(
        mut self,
        context_manager: Arc<parking_lot::Mutex<crate::context::ContextManager>>,
        memory_store: Arc<parking_lot::Mutex<crate::memory::MemoryStore>>,
        plan_canvas: Arc<parking_lot::Mutex<crate::plan::PlanCanvas>>,
    ) -> Self {
        self.context_manager = context_manager;
        self.memory_store = memory_store;
        self.plan_canvas = plan_canvas;
        self
    }

    /// Install the spec §3 hunk-accept-reject policy. With
    /// `AwaitApproval`, [`Self::dispatch`] for tools that produce staged
    /// writes emits `Event::StagingPendingApproval` and blocks until
    /// the consumer calls [`Self::submit_approval`].
    ///
    /// v57 (M-bug-2 fix) — `AutoApproveAll` now actively re-installs
    /// the [`AutoApprove`] gate. The pre-v57 path treated this arm as
    /// a no-op, which silently broke any caller that toggled
    /// `AwaitApproval → AutoApproveAll` to "revert" — the inner
    /// gate stayed at `PendingApprovalGate` and dispatch kept parking
    /// on the pending channel.
    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        let gate: Arc<dyn ApprovalGate> = match policy {
            ApprovalPolicy::AutoApproveAll => Arc::new(AutoApprove),
            ApprovalPolicy::AwaitApproval => Arc::new(PendingApprovalGate {
                events: self.events.clone(),
                pending: self.pending.clone(),
            }),
        };
        // Swap the inner dispatcher to one with the new gate. Builder
        // consumes self; we re-construct in place.
        let old = std::mem::replace(
            &mut self.dispatcher,
            Dispatcher::new(ToolRegistry::new(), HookSet::empty()),
        );
        self.dispatcher = old.with_approval_gate(gate);
        self
    }

    /// Spec §3 follow-on to `Event::StagingPendingApproval`: deliver
    /// the user's [`crate::staging::HunkSelection`] for a pending
    /// commit. Returns `false` when `commit_id` doesn't match an
    /// outstanding pending (e.g. already approved, or the dispatcher
    /// dropped its receiver because the consumer disconnected).
    ///
    /// v56 widened from `Vec<PathBuf>` to `HunkSelection` so the
    /// consumer can carry per-hunk decisions. To preserve pre-v56
    /// call ergonomics, see [`Self::submit_approval_files`] which
    /// translates a path list into an `All` selection.
    pub fn submit_approval(
        &self,
        commit_id: uuid::Uuid,
        selection: crate::staging::HunkSelection,
    ) -> bool {
        let sender = self.pending.lock().remove(&commit_id);
        match sender {
            Some(tx) => tx.send(selection).is_ok(),
            None => false,
        }
    }

    /// Pre-v56-compat wrapper. Builds a `FileApproval::All` selection
    /// from `accepted` and delegates to [`Self::submit_approval`].
    /// Used by callers that still think in terms of file-level
    /// accept/reject — tests, the v47 GUI/TUI path before per-hunk
    /// selection lands.
    pub fn submit_approval_files(
        &self,
        commit_id: uuid::Uuid,
        accepted: Vec<std::path::PathBuf>,
    ) -> bool {
        let selection: crate::staging::HunkSelection = accepted
            .into_iter()
            .map(|p| (p, crate::staging::FileApproval::All))
            .collect();
        self.submit_approval(commit_id, selection)
    }

    /// Access the underlying pure dispatcher (useful when tests need to
    /// hit the bare path or when the caller wants to inspect the
    /// registry / hook-set state).
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    // ----- v55 §5 mutator surface -----
    //
    // Each method acquires the relevant lock, calls the pure data-layer
    // op, drops the lock, then re-broadcasts the matching Snapshot
    // event so subscribed UIs converge. The pure data layer stays I/O-
    // free; this layer owns the side effects (lock + emit + ledger).

    fn emit_context_items(&self) {
        let items = self.context_manager.lock().summarise();
        let _ = self.events.send(Event::ContextItems { items });
    }

    fn emit_memory_cards(&self) {
        let cards = self.memory_store.lock().summarise();
        let _ = self.events.send(Event::MemoryCards { cards });
    }

    fn emit_plan_snapshot(&self) {
        let steps = self.plan_canvas.lock().to_vec();
        let _ = self.events.send(Event::PlanSnapshot { steps });
    }

    /// v61 — surface a user's §14 concurrent-edit modal choice onto
    /// the bus as `Event::FilesChangedAcknowledged`. Best-effort send;
    /// no subscribers (post-shutdown) is acceptable, matching the
    /// other dispatcher-side emit helpers. Called by the GUI's
    /// `resolve_concurrent_edit` Tauri command and the TUI's
    /// `ConcurrentEditResolve` outcome.
    pub fn resolve_concurrent_edit(&self, outcome: crate::session::ConcurrentEditOutcome) {
        let _ = self
            .events
            .send(Event::FilesChangedAcknowledged { outcome });
    }

    /// Pin a context item. UI handler. Returns `Ok` on success, with
    /// the matching `ContextItems` snapshot already broadcast; UI
    /// state converges on receipt rather than from the return value.
    pub fn pin_context_item(&self, id: &str) -> Result<(), crate::context::ContextError> {
        let item_id = parse_context_item_id(id)?;
        self.context_manager.lock().pin(item_id)?;
        self.emit_context_items();
        Ok(())
    }

    pub fn unpin_context_item(&self, id: &str) -> Result<(), crate::context::ContextError> {
        let item_id = parse_context_item_id(id)?;
        self.context_manager.lock().unpin(item_id)?;
        self.emit_context_items();
        Ok(())
    }

    /// Evict a context item. Also appends a `CacheBust` ledger entry
    /// (spec §5 "cache-bust cost is invisible unless ledgered") and
    /// emits `LedgerAppended` so the cost meter ticks. Returns the
    /// `CacheBustEvent` so the caller can surface "freed N tokens" in
    /// a confirm-result toast.
    pub fn evict_context_item(
        &self,
        id: &str,
        evicted_at: &str,
    ) -> Result<crate::context::CacheBustEvent, crate::context::ContextError> {
        let item_id = parse_context_item_id(id)?;
        let event = self.context_manager.lock().evict(item_id, evicted_at)?;
        let entry = crate::ledger::LedgerEntry::cache_bust_from(&event);
        self.ledger.append(entry.clone());
        let _ = self.events.send(Event::LedgerAppended { entry });
        self.emit_context_items();
        Ok(event)
    }

    /// Add a memory card with a freshly minted id. The dispatcher
    /// owns id generation so the UI doesn't have to.
    ///
    /// v57 (M-sec-5 fix) — rejects content that:
    ///   * contains NUL or other ASCII control bytes (except `\n` and
    ///     `\t`), which would render as binary in grep/git and break
    ///     any downstream tooling that scans `~/.atelier/memory/`,
    ///   * contains a line starting with `---` (YAML frontmatter
    ///     delimiter), which would forge frontmatter in the promoted
    ///     markdown file and pollute `mempromote` / `memrecall`.
    pub fn add_memory_card(
        &self,
        content: String,
        now: &str,
    ) -> Result<String, crate::memory::MemoryError> {
        validate_memory_card_content(&content)?;
        let id = format!("mem-{}", uuid::Uuid::new_v4());
        let card = crate::memory::MemoryCard {
            id: id.clone(),
            content,
            created_at: now.to_string(),
            last_used: now.to_string(),
            pinned: false,
            compacted_from: None,
        };
        self.memory_store.lock().add(card)?;
        self.emit_memory_cards();
        Ok(id)
    }

    pub fn delete_memory_card(&self, id: &str) -> Result<(), crate::memory::MemoryError> {
        self.memory_store.lock().evict(id)?;
        self.emit_memory_cards();
        Ok(())
    }

    /// Promote a memory card. Returns the bytes the caller writes to
    /// `~/.atelier/memory/<filename>`; this method advances the card's
    /// `last_used` and re-emits so the UI shows the "just-promoted"
    /// timestamp tick.
    pub fn promote_memory_card(
        &self,
        id: &str,
        now: &str,
    ) -> Result<crate::memory::PromoteOutput, crate::memory::MemoryError> {
        let mut store = self.memory_store.lock();
        let output = store.promote_to_global(id)?;
        store.touch(id, now)?;
        drop(store);
        self.emit_memory_cards();
        Ok(output)
    }

    /// Add a plan step. Returns the auto-assigned `step-N` id.
    ///
    /// v59 (MED-sec-2 fix) — validates the text against Trojan Source
    /// / control-byte rejection so a hostile model can't make a plan
    /// step display reversed in the GUI/TUI footer. The pre-v59
    /// signature was infallible; the new `Result` returns the
    /// rejection reason. Callers that don't care can `.ok()` or
    /// `.expect()` — but the GUI / TUI surface the error to the
    /// user, which is the intent.
    pub fn add_plan_step(&self, text: String) -> Result<String, crate::plan::PlanError> {
        validate_plan_text(&text)?;
        let id = self.plan_canvas.lock().add(text);
        self.emit_plan_snapshot();
        Ok(id)
    }

    pub fn remove_plan_step(&self, id: &str) -> Result<(), crate::plan::PlanError> {
        self.plan_canvas.lock().remove(id)?;
        self.emit_plan_snapshot();
        Ok(())
    }

    /// Set a plan step's status. UI cycler maps button click → status
    /// → this method.
    pub fn mark_plan_step_status(
        &self,
        id: &str,
        status: crate::plan::PlanStatus,
    ) -> Result<(), crate::plan::PlanError> {
        self.plan_canvas.lock().mark_status(id, status)?;
        self.emit_plan_snapshot();
        Ok(())
    }

    pub fn add_plan_step_constraint(
        &self,
        id: &str,
        constraint: String,
    ) -> Result<(), crate::plan::PlanError> {
        // v59 (MED-sec-2 fix) — same Trojan-Source rejection as
        // `add_plan_step`. Constraint strings also render in the
        // GUI/TUI plan pane and persist to session.json.
        validate_plan_text(&constraint)?;
        self.plan_canvas.lock().add_constraint(id, constraint)?;
        self.emit_plan_snapshot();
        Ok(())
    }

    /// Rewrite the plan order. `new_order` must contain every existing
    /// id exactly once; otherwise the canvas is unchanged.
    pub fn reorder_plan_steps(&self, new_order: Vec<String>) -> Result<(), crate::plan::PlanError> {
        self.plan_canvas.lock().reorder(new_order)?;
        self.emit_plan_snapshot();
        Ok(())
    }

    /// Phase C close — §5 mental-model mutator. Updates the toggle +
    /// free-form text and broadcasts the snapshot. Off by default; v0
    /// does **not** inject the text into the prompt. Returns the
    /// projection so callers (CLI / Tauri / TUI) can render the
    /// cost-disclosure label inline without a follow-up read.
    pub fn set_mental_model(
        &self,
        text: String,
        enabled: bool,
        now: &str,
    ) -> Result<crate::mental_model::MentalModelSnapshot, crate::mental_model::MentalModelError>
    {
        let mut m = self.mental_model.lock();
        m.set(text, enabled, now)?;
        let snap = m.snapshot();
        drop(m);
        let _ = self.events.send(Event::MentalModelSnapshot {
            enabled: snap.enabled,
            text_tokens: snap.text_tokens,
        });
        Ok(snap)
    }

    /// Phase C close — read-only snapshot of the §5 mental-model.
    /// Lets a CLI subcommand or a Tauri command surface the current
    /// text without a re-emit (or, for the snapshot bus consumers,
    /// without waiting for an event).
    pub fn snapshot_mental_model(&self) -> crate::mental_model::MentalModelSnapshot {
        self.mental_model.lock().snapshot()
    }

    /// v62 — §7 verify pass. Runs [`crate::verify::compare`] against
    /// the envelope's `claimed_changes` and the workspace
    /// [`crate::verify::ObservedChange`] list, then broadcasts the
    /// [`Event::VerificationPassed`] terminal marker with the tier
    /// that actually ran so the GUI/TUI badge converges.
    ///
    /// Returns the [`crate::verify::VerificationRun`] so the caller
    /// (the Runner today, a future audit-grade DoD pass tomorrow)
    /// can ledger the discrepancies without re-running the
    /// comparison.
    ///
    /// **Tier selection.** v0 always runs Tier 3 (the pure textual
    /// `compare`). Tier 1 (LSP) is gated on Q3 and has no producer
    /// wired yet — the [`crate::verify::VerificationTier::Tier1Lsp`]
    /// variant exists so flipping the producer in is a one-line
    /// change here. Tier 2 (tree-sitter syntactic) already runs
    /// inside `staging::SyntaxCheck` at commit time; the verify pass
    /// will surface Tier 2 once we thread the per-file syntax
    /// outcomes back into this call (Phase D follow-on). Until
    /// then the explicit "Tier 3 ran" badge is more honest than
    /// silently degrading.
    pub fn verify_pass(
        &self,
        envelope: &crate::protocol::Envelope,
        observed: &[crate::verify::ObservedChange],
    ) -> crate::verify::VerificationRun {
        let run = crate::verify::VerificationRun::tier3_textual(envelope, observed);
        // §7 lying-agent / silent-edit gate. Empty discrepancies =>
        // workspace agrees with the envelope; non-empty => the §7
        // detector flags the turn. Each verify call emits exactly one
        // of the two terminal-marker events so consumers can rely on
        // the per-run contract.
        if run.discrepancies.is_empty() {
            let _ = self.events.send(Event::VerificationPassed {
                tier: run.tier,
                file_count: run.file_count,
                claim_count: run.claim_count,
            });
        } else {
            let _ = self.events.send(Event::VerificationFailed {
                tier: run.tier,
                discrepancies: run.discrepancies.clone(),
            });
        }
        run
    }

    /// v62 — explicit "no verify pass ran this turn" signal. Emitted
    /// when the harness skips the §7 gate (envelope didn't claim
    /// done, or the run aborted early). UIs render a "verify off"
    /// gray badge so absence is unambiguous rather than the user
    /// inferring it from a missing event.
    pub fn emit_verify_not_run(&self) {
        let _ = self.events.send(Event::VerificationPassed {
            tier: crate::verify::VerificationTier::NotRun,
            file_count: 0,
            claim_count: 0,
        });
    }

    /// v60.5 — append a single ledger entry and broadcast the matching
    /// `LedgerAppended` event. Public so callers outside the dispatcher
    /// (`atelier_cli::compaction`, which records the `ModelCall` for the
    /// summary generation) can ledger without holding their own
    /// `Arc<Ledger>` + `broadcast::Sender` clones. The dispatcher's
    /// internal call sites continue to inline `ledger.append + send`
    /// where they need other side effects in the same atomic step.
    pub fn append_ledger_entry(&self, entry: crate::ledger::LedgerEntry) {
        self.ledger.append(entry.clone());
        let _ = self.events.send(Event::LedgerAppended { entry });
    }

    /// v60.5 — snapshot a subset of `ContextManager` items without
    /// evicting them. Used by the `atelier_cli::compaction` orchestrator
    /// to serialise the items into the expansion blob *before* calling
    /// [`Self::compact_context_items`] (which then atomically evicts
    /// them). Returns clones (so the caller is free to drop the lock)
    /// in input order.
    pub fn snapshot_context_items(
        &self,
        ids: &[String],
    ) -> Result<Vec<crate::context::ContextItem>, crate::context::ContextError> {
        let parsed: Vec<_> = ids
            .iter()
            .map(|s| parse_context_item_id(s))
            .collect::<Result<_, _>>()?;
        let mgr = self.context_manager.lock();
        let mut out = Vec::with_capacity(parsed.len());
        for id in parsed {
            let item = mgr
                .get(id)
                .ok_or(crate::context::ContextError::NotFound(id))?
                .clone();
            out.push(item);
        }
        Ok(out)
    }

    /// v60.5 — atomic §5 non-destructive compaction. Replaces the
    /// `ids` items in `ContextManager` with one pinned summary
    /// `MemoryCard`, ledgers the operation as a `Compaction` entry, and
    /// emits the snapshot stream so subscribed UIs converge.
    ///
    /// The caller is responsible for:
    ///   1. Generating `summary_text` (typically via `adapter.chat()`).
    ///   2. Snapshotting the items via [`Self::snapshot_context_items`]
    ///      and writing them to disk via
    ///      `atelier_cli::compaction_blob::write` *before* this call.
    ///   3. Passing the resulting `expansion_blob_path` here so the
    ///      summary card's `compacted_from` link points at the
    ///      already-written blob (v60.6 Expand reads it back).
    ///
    /// Event broadcast order (matters for UI convergence):
    ///   * `LedgerAppended` with the `Compaction` entry
    ///   * `ContextItems` snapshot (with the replaced items gone)
    ///   * `MemoryCards` snapshot (with the new pinned summary)
    ///   * `CompactionExecuted` (terminal signal for UIs to clear
    ///     their multi-select / toast state)
    pub fn compact_context_items(
        &self,
        ids: Vec<String>,
        summary_text: String,
        expansion_blob_path: String,
        now: &str,
    ) -> Result<CompactionOutput, CompactionError> {
        if ids.is_empty() {
            return Err(CompactionError::Empty);
        }

        // Pre-mutation validation. Same predicates as the memory-card
        // add path so a summary that fails here would have failed there
        // (and a future `promote_to_global` of the summary card stays
        // safe).
        crate::text_safety::validate_user_text(&summary_text, /* check_frontmatter */ true)
            .map_err(CompactionError::InvalidSummary)?;

        let parsed_ids: Vec<_> = ids
            .iter()
            .map(|s| parse_context_item_id(s))
            .collect::<Result<_, _>>()?;

        // Evict atomically (pin/missing checks land in Pass 1).
        let cache_bust_events = self.context_manager.lock().evict_batch(&parsed_ids, now)?;
        let freed_tokens: u32 = cache_bust_events.iter().map(|e| e.tokens_freed).sum();

        // Mint the summary card. Always pinned — the compaction is
        // pointless if the user can drop the replacement.
        let summary_card_id = format!("mem-{}", uuid::Uuid::new_v4());
        let card = crate::memory::MemoryCard {
            id: summary_card_id.clone(),
            content: summary_text,
            created_at: now.to_string(),
            last_used: now.to_string(),
            pinned: true,
            compacted_from: Some(crate::memory::CompactionSource {
                item_ids: ids.clone(),
                expansion_blob_path: expansion_blob_path.clone(),
                compacted_at: now.to_string(),
                cache_rewarm_tokens: freed_tokens,
            }),
        };
        self.memory_store.lock().add(card)?;

        // Ledger entry + event broadcast.
        let entry = crate::ledger::LedgerEntry::Compaction {
            timestamp: now.to_string(),
            freed_tokens,
            replaced_items: ids,
            summary_card_id: summary_card_id.clone(),
            expansion_blob_path,
        };
        self.ledger.append(entry.clone());
        let _ = self.events.send(Event::LedgerAppended { entry });
        self.emit_context_items();
        self.emit_memory_cards();
        let _ = self.events.send(Event::CompactionExecuted {
            freed_tokens,
            replaced_item_count: cache_bust_events.len(),
            summary_card_id: summary_card_id.clone(),
        });

        Ok(CompactionOutput {
            summary_card_id,
            freed_tokens,
        })
    }

    /// v60.6 — read-only snapshot of a single memory card by id. Used
    /// by `atelier_cli::expansion::expand` to read the
    /// `compacted_from` link (and its `expansion_blob_path`) before
    /// reading the blob; symmetric to [`Self::snapshot_context_items`].
    /// Returns `None` when the card isn't in the store.
    pub fn snapshot_memory_card(&self, id: &str) -> Option<crate::memory::MemoryCard> {
        self.memory_store.lock().get(id).cloned()
    }

    /// v60.6 — atomic §5 Expand. Symmetric counterpart to
    /// [`Self::compact_context_items`]. Removes a compaction-summary
    /// `MemoryCard`, re-inserts the original `ContextItem`s into the
    /// `ContextManager`, ledgers the operation as an `Expansion`
    /// entry, and emits the snapshot stream so subscribed UIs
    /// converge.
    ///
    /// The caller is responsible for:
    ///
    ///   1. Reading the on-disk blob via
    ///      `atelier_cli::compaction_blob::read` (path comes from the
    ///      card's `compacted_from.expansion_blob_path`).
    ///   2. Passing the blob items in — this method validates them
    ///      against the card's `compacted_from.item_ids` (count + ids
    ///      in order) before mutating anything.
    ///
    /// Failure modes (all atomic — state stays unchanged on `Err`):
    ///
    ///   * `CardNotFound` if the card isn't in `MemoryStore`.
    ///   * `NotACompactionCard` if the card has no `compacted_from`.
    ///   * `ItemMismatch` / `ItemIdMismatch` if `items` doesn't
    ///     match the card's recorded ids (defends against a
    ///     blob/card desync — e.g., the blob was rewritten by hand).
    ///   * `Context(AlreadyPresent)` if any restored id collides
    ///     with an item already in the manager (e.g., the user
    ///     pinned a memory card after compaction and is trying to
    ///     re-expand on top of another compaction's tail).
    ///
    /// Event broadcast order (matches Compaction's discipline):
    ///   * `LedgerAppended` with the `Expansion` entry
    ///   * `ContextItems` snapshot (with the restored items present)
    ///   * `MemoryCards` snapshot (with the summary card gone)
    ///   * `ExpansionExecuted` (terminal signal for UIs)
    pub fn expand_memory_card(
        &self,
        card_id: String,
        items: Vec<crate::context::ContextItem>,
        now: &str,
    ) -> Result<ExpansionOutput, ExpansionError> {
        // ---- Pre-flight: snapshot card + validate it carries compacted_from. ----
        let card = self
            .memory_store
            .lock()
            .get(&card_id)
            .cloned()
            .ok_or_else(|| ExpansionError::CardNotFound(card_id.clone()))?;
        let compacted_from = card
            .compacted_from
            .as_ref()
            .ok_or_else(|| ExpansionError::NotACompactionCard(card_id.clone()))?;

        // ---- Validate items match the card's recorded ids. ----
        if items.len() != compacted_from.item_ids.len() {
            return Err(ExpansionError::ItemMismatch {
                expected: compacted_from.item_ids.len(),
                got: items.len(),
            });
        }
        for (position, (expected_id, item)) in
            compacted_from.item_ids.iter().zip(items.iter()).enumerate()
        {
            if item.id.to_string() != *expected_id {
                return Err(ExpansionError::ItemIdMismatch {
                    position,
                    expected: expected_id.clone(),
                    got: item.id.to_string(),
                });
            }
        }

        // Pre-compute the rewarm cost from the items we're about to
        // restore; this is the ledger entry's authoritative value.
        // Equal to the card's stored `cache_rewarm_tokens` for any
        // v60.6+ compaction; for v60.5-era compactions (where the
        // stored field defaulted to 0) we still report the real cost.
        let cache_rewarm_tokens: u32 = items.iter().map(|i| i.tokens.count).sum();
        let restored_item_ids: Vec<String> = items.iter().map(|i| i.id.to_string()).collect();
        let restored_item_count = items.len();

        // ---- Pass-1 atomic restore: refuse on any id collision. ----
        self.context_manager.lock().add_batch(items)?;

        // ---- Drop the summary card from MemoryStore. ----
        self.memory_store.lock().evict(&card_id)?;

        // ---- Ledger entry + event broadcast. ----
        let entry = crate::ledger::LedgerEntry::Expansion {
            timestamp: now.to_string(),
            restored_item_ids,
            summary_card_id: card_id.clone(),
            cache_rewarm_tokens,
        };
        self.ledger.append(entry.clone());
        let _ = self.events.send(Event::LedgerAppended { entry });
        self.emit_context_items();
        self.emit_memory_cards();
        let _ = self.events.send(Event::ExpansionExecuted {
            restored_item_count,
            summary_card_id: card_id.clone(),
            cache_rewarm_tokens,
        });

        Ok(ExpansionOutput {
            restored_item_count,
            summary_card_id: card_id,
            cache_rewarm_tokens,
        })
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
        // v61 — §14 read-set tracking. On any *successful* read-only
        // tool call, hand the path(s) the model just observed to the
        // file watcher. Cheap no-op when the watcher is disabled (the
        // default for tests / GUI demos).
        if outcome.result.is_ok() && !self.file_watcher.is_disabled() {
            for path in extract_read_paths(&call.name, &call.arguments, ctx.workspace_root) {
                self.file_watcher.track(&path);
            }
        }
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
        //
        // v57 (M-sec-4 fix) — redact obvious-secret keys before
        // landing the payload in env. An approved hook with
        // `allow_net: true` could otherwise exfiltrate API keys
        // pasted into tool arguments. The redaction is intentionally
        // conservative (substring-match on the key name): if the
        // model emits `{"api_key": "..."}`, `{"AuthorizationToken":
        // "..."}` etc. the value is replaced by `"<redacted>"`.
        let redacted = redact_secrets(payload);
        spec.env.insert(
            "ATELIER_HOOK_PAYLOAD".into(),
            serde_json::to_string(&redacted).unwrap_or_else(|_| "{}".into()),
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
        ///
        /// v56: each entry carries an optional explicit content; `None`
        /// keeps the pre-v56 default of "x" so existing tests are
        /// unchanged. Tests that need a meaningful diff (per-hunk
        /// accept/reject) use [`Self::with_staged_writes`].
        staged_paths: Option<Vec<(String, Option<String>)>>,
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
            self.staged_paths = Some(paths.iter().map(|s| (s.to_string(), None)).collect());
            self
        }

        fn with_staged_writes(mut self, writes: Vec<(&str, &str)>) -> Self {
            self.staged_paths = Some(
                writes
                    .into_iter()
                    .map(|(p, c)| (p.to_string(), Some(c.to_string())))
                    .collect(),
            );
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
                for (p, content) in paths {
                    let body = content.clone().unwrap_or_else(|| "x".to_string());
                    s.add(crate::staging::StagedWrite::new(p.clone(), body))
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
            tool_call_id: None,
            audit_log_path: None,
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
            tool_call_id: None,
            audit_log_path: None,
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

    #[test]
    fn side_effect_as_str_agrees_with_serde() {
        // Regression for v60 MED-B — `as_str()` and the
        // `#[serde(rename_all = "kebab-case")]` projection must
        // produce identical strings. Pre-v60 nothing tied them
        // together; a rename of `SideEffectClass::SharedState` →
        // `SideEffectClass::Shared` would leave the serde derive
        // happy with `"shared"` while `as_str()` kept returning
        // `"shared-state"` (caught at compile if exhaustive — but
        // the wire strings could still drift if both ends were
        // updated to different values).
        for c in [
            SideEffectClass::LocalSafe,
            SideEffectClass::LocalRisky,
            SideEffectClass::SharedState,
            SideEffectClass::Irreversible,
        ] {
            let serde_label = serde_json::to_value(c).unwrap();
            let serde_str = serde_label
                .as_str()
                .expect("SideEffectClass serializes as a string");
            assert_eq!(
                serde_str,
                c.as_str(),
                "as_str({c:?}) must match serde projection"
            );
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
                        sd.submit_approval_files(cid, accepted),
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
                    sd.submit_approval_files(commit_id, Vec::new());
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
        assert!(!sd.submit_approval_files(uuid::Uuid::new_v4(), Vec::new()));
    }

    #[tokio::test]
    async fn with_approval_policy_auto_after_await_reverts_to_autoapprove() {
        // Regression for M-bug-2 — pre-v57 the AutoApproveAll arm of
        // with_approval_policy was a no-op, so toggling
        // AwaitApproval → AutoApproveAll kept the inner gate at
        // PendingApprovalGate and dispatch kept parking. The fix
        // installs AutoApprove explicitly on both arms.
        let tool = Arc::new(EchoTool::new("write_thing").with_staged(vec!["a.rs"]));
        let (sd, _ledger, _rx) = build_session_dispatcher(vec![tool]);
        let sd = sd
            .with_approval_policy(ApprovalPolicy::AwaitApproval)
            .with_approval_policy(ApprovalPolicy::AutoApproveAll);
        let workspace = tempfile::TempDir::new().unwrap();
        let policy = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sd.dispatch(
                &call("write_thing", json!({})),
                &ctx_in_workspace(workspace.path(), &policy),
                now,
            ),
        )
        .await
        .expect("dispatch must NOT park on AutoApproveAll");
        assert!(outcome.result.is_ok());
        assert!(
            workspace.path().join("a.rs").exists(),
            "file should commit immediately under AutoApprove"
        );
    }

    #[tokio::test]
    async fn cancelled_dispatch_future_does_not_leak_pending_entry() {
        // Regression for H1 — a cancelled / aborted dispatch must
        // remove its entry from PendingApprovalGate.pending so the
        // HashMap doesn't grow unboundedly. Spawn a dispatch, wait
        // for the StagingPendingApproval bus event (= entry
        // registered), abort the task, give a moment for the guard
        // to drop, then peek the pending map.
        let tool = Arc::new(EchoTool::new("writer").with_staged(vec!["a.txt"]));
        let (sd, _ledger, mut rx) = build_session_dispatcher(vec![tool]);
        let sd = Arc::new(sd.with_approval_policy(ApprovalPolicy::AwaitApproval));
        let workspace = tempfile::TempDir::new().unwrap();
        let ws_path = workspace.path().to_path_buf();
        let sd_dispatch = sd.clone();
        let task = tokio::spawn(async move {
            let p = SandboxPolicy::restrictive(&ws_path).unwrap();
            sd_dispatch
                .dispatch(
                    &call("writer", json!({})),
                    &ctx_in_workspace(&ws_path, &p),
                    now,
                )
                .await
        });
        // Wait until we see the pending event — proves the entry
        // was registered.
        loop {
            match next_event(&mut rx).await {
                Event::StagingPendingApproval { .. } => break,
                _ => continue,
            }
        }
        // Cancel mid-await.
        task.abort();
        let _ = task.await; // resolves to JoinError(Cancelled)
                            // Let the abort propagate + the guard's Drop run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            sd.pending.lock().is_empty(),
            "pending map should be empty after a cancelled dispatch"
        );
    }

    // ---------- v56 hunk-level approval ----------

    #[tokio::test]
    async fn submit_approval_with_per_hunk_selection_routes_to_commit_selected_hunks() {
        // Pre-image with two non-adjacent changes → two hunks. Accept
        // only hunk 0; reject hunk 1. The file should land with hunk 0
        // applied and hunk 1 reverted to pre-image.
        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::write(
            workspace.path().join("a.txt"),
            b"one\ntwo\nthree\nfour\nfive\n",
        )
        .unwrap();
        let tool = Arc::new(
            EchoTool::new("rewriter")
                .with_staged_writes(vec![("a.txt", "ONE\ntwo\nthree\nfour\nFIVE\n")]),
        );
        let (sd, _ledger, mut rx) = build_session_dispatcher(vec![tool]);
        let sd = Arc::new(sd.with_approval_policy(ApprovalPolicy::AwaitApproval));
        let ws_path = workspace.path().to_path_buf();
        let sd_dispatch = sd.clone();
        let dispatch_task = tokio::spawn(async move {
            let p = SandboxPolicy::restrictive(&ws_path).unwrap();
            sd_dispatch
                .dispatch(
                    &call("rewriter", json!({})),
                    &ctx_in_workspace(&ws_path, &p),
                    now,
                )
                .await
        });

        loop {
            match next_event(&mut rx).await {
                Event::StagingPendingApproval { commit_id, .. } => {
                    let mut sel = crate::staging::HunkSelection::new();
                    sel.insert(
                        std::path::PathBuf::from("a.txt"),
                        crate::staging::FileApproval::Hunks(vec![0]),
                    );
                    assert!(sd.submit_approval(commit_id, sel));
                    break;
                }
                _ => continue,
            }
        }
        let outcome = dispatch_task.await.unwrap();
        assert!(outcome.result.is_ok());
        assert_eq!(
            std::fs::read(workspace.path().join("a.txt")).unwrap(),
            b"ONE\ntwo\nthree\nfour\nfive\n",
            "hunk 0 accepted; hunk 1 reverted to pre-image"
        );
    }

    // ---------- v55 §5 mutator round-trips ----------

    use crate::context::{
        ContextItem, ContextItemId, Payload as ContextPayload, Provenance, TokenCount, TokenSource,
    };
    use crate::memory::MemoryStore;
    use crate::plan::{PlanCanvas, PlanStatus};

    #[allow(clippy::type_complexity)]
    fn build_v55_dispatcher() -> (
        SessionDispatcher,
        Arc<parking_lot::Mutex<crate::context::ContextManager>>,
        Arc<parking_lot::Mutex<MemoryStore>>,
        Arc<parking_lot::Mutex<PlanCanvas>>,
        Arc<Ledger>,
        broadcast::Receiver<Event>,
    ) {
        let dispatcher = Dispatcher::new(ToolRegistry::new(), HookSet::empty());
        let ledger = Arc::new(Ledger::new());
        let (tx, rx) = broadcast::channel(64);
        let cm = Arc::new(parking_lot::Mutex::new(
            crate::context::ContextManager::new(),
        ));
        let ms = Arc::new(parking_lot::Mutex::new(MemoryStore::new()));
        let pc = Arc::new(parking_lot::Mutex::new(PlanCanvas::new()));
        let sd = SessionDispatcher::new(dispatcher, ledger.clone(), tx).with_shared_state(
            cm.clone(),
            ms.clone(),
            pc.clone(),
        );
        (sd, cm, ms, pc, ledger, rx)
    }

    fn seed_context_item(
        cm: &Arc<parking_lot::Mutex<crate::context::ContextManager>>,
        tokens: u32,
    ) -> ContextItemId {
        let item = ContextItem {
            id: ContextItemId::new(),
            payload: ContextPayload::InlineText { text: "x".into() },
            tokens: TokenCount {
                count: tokens,
                source: TokenSource::Approx,
            },
            provenance: Provenance::UserAttached { note: None },
            pinned: false,
            added_at: "2026-05-17T10:00:00Z".into(),
            last_used: "2026-05-17T10:00:00Z".into(),
        };
        let id = item.id;
        cm.lock().add(item);
        id
    }

    #[tokio::test]
    async fn pin_context_item_marks_pinned_and_emits_snapshot() {
        let (sd, cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = seed_context_item(&cm, 10);
        sd.pin_context_item(&id.to_string()).unwrap();
        assert!(cm.lock().get(id).unwrap().pinned);
        match next_event(&mut rx).await {
            Event::ContextItems { items } => {
                assert_eq!(items.len(), 1);
                assert!(items[0].pinned);
            }
            other => panic!("expected ContextItems, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unpin_context_item_clears_pinned() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let id = seed_context_item(&cm, 10);
        cm.lock().pin(id).unwrap();
        sd.unpin_context_item(&id.to_string()).unwrap();
        assert!(!cm.lock().get(id).unwrap().pinned);
    }

    #[tokio::test]
    async fn evict_context_item_appends_cache_bust_to_ledger_and_emits() {
        let (sd, cm, _ms, _pc, ledger, mut rx) = build_v55_dispatcher();
        let id = seed_context_item(&cm, 128);
        let ev = sd
            .evict_context_item(&id.to_string(), "2026-05-17T10:05:00Z")
            .unwrap();
        assert_eq!(ev.tokens_freed, 128);
        assert_eq!(cm.lock().len(), 0);
        // Ledger has a single CacheBust entry.
        let entries = ledger.to_vec();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], LedgerEntry::CacheBust { .. }));
        // Event order: LedgerAppended, then ContextItems.
        match next_event(&mut rx).await {
            Event::LedgerAppended { entry } => {
                assert!(matches!(entry, LedgerEntry::CacheBust { .. }))
            }
            other => panic!("expected LedgerAppended first, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::ContextItems { items } => assert!(items.is_empty()),
            other => panic!("expected ContextItems second, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn evict_pinned_context_item_returns_error_and_does_not_emit() {
        let (sd, cm, _ms, _pc, ledger, mut rx) = build_v55_dispatcher();
        let id = seed_context_item(&cm, 5);
        cm.lock().pin(id).unwrap();
        let err = sd
            .evict_context_item(&id.to_string(), "2026-05-17T10:05:00Z")
            .unwrap_err();
        assert!(matches!(err, crate::context::ContextError::EvictPinned(_)));
        assert_eq!(cm.lock().len(), 1, "pinned item must stay");
        assert_eq!(ledger.len(), 0, "no ledger entry on refused evict");
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "no event on refused evict"
        );
    }

    #[tokio::test]
    async fn pin_with_malformed_id_returns_malformed_error() {
        // v57 (L cleanup) — distinguish "garbage input" from "valid
        // UUID that just isn't in the store" so the error message in
        // the UI is precise.
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let err = sd.pin_context_item("not-a-uuid").unwrap_err();
        assert!(matches!(err, crate::context::ContextError::Malformed(s) if s == "not-a-uuid"));
    }

    #[tokio::test]
    async fn pin_with_valid_uuid_not_in_store_returns_not_found() {
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let fresh_id = uuid::Uuid::new_v4().to_string();
        let err = sd.pin_context_item(&fresh_id).unwrap_err();
        assert!(matches!(err, crate::context::ContextError::NotFound(_)));
    }

    #[tokio::test]
    async fn add_memory_card_inserts_and_emits_snapshot() {
        let (sd, _cm, ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd
            .add_memory_card("a note worth keeping".into(), "2026-05-17T10:00:00Z")
            .unwrap();
        assert!(id.starts_with("mem-"));
        assert_eq!(ms.lock().len(), 1);
        match next_event(&mut rx).await {
            Event::MemoryCards { cards } => {
                assert_eq!(cards.len(), 1);
                assert_eq!(cards[0].id, id);
            }
            other => panic!("expected MemoryCards, got {other:?}"),
        }
    }

    #[test]
    fn redact_secrets_masks_obvious_keys_at_any_depth() {
        // Regression for M-sec-4 — secret-looking keys in hook
        // payloads must be redacted before landing in env.
        let payload = serde_json::json!({
            "command": "shell",
            "arguments": {
                "api_key": "sk-deadbeef",
                "headers": [{"Authorization": "Bearer x"}],
                "harmless": "value"
            },
            "metadata": {
                "session_id": "abcd",
                "OPENAI_API_KEY": "sk-y",
                "nested": {"password": "p"}
            }
        });
        let out = redact_secrets(&payload);
        assert_eq!(out["arguments"]["api_key"], "<redacted>");
        assert_eq!(
            out["arguments"]["headers"][0]["Authorization"],
            "<redacted>"
        );
        assert_eq!(out["arguments"]["harmless"], "value");
        assert_eq!(out["metadata"]["session_id"], "<redacted>");
        assert_eq!(out["metadata"]["OPENAI_API_KEY"], "<redacted>");
        assert_eq!(out["metadata"]["nested"]["password"], "<redacted>");
        assert_eq!(out["command"], "shell");
    }

    #[test]
    fn redact_secrets_v58_covers_cloud_and_session_creds() {
        // Regression for L-sec-2 — the v57 list missed common cloud /
        // session creds. v58 expanded with `access_key`,
        // `private_key`, `bearer`, `cookie`, `_pat`.
        let payload = serde_json::json!({
            "aws_access_key_id": "AKIA...",
            "aws_secret_access_key": "...",
            "ssh_private_key": "----- BEGIN RSA -----",
            "Authorization": "Bearer X",
            "Set-Cookie": "session=abc; HttpOnly",
            "github_pat_xyz": "ghp_...",
            "harmless": "value"
        });
        let out = redact_secrets(&payload);
        assert_eq!(out["aws_access_key_id"], "<redacted>");
        assert_eq!(out["aws_secret_access_key"], "<redacted>");
        assert_eq!(out["ssh_private_key"], "<redacted>");
        assert_eq!(out["Authorization"], "<redacted>");
        assert_eq!(out["Set-Cookie"], "<redacted>");
        assert_eq!(out["github_pat_xyz"], "<redacted>");
        assert_eq!(out["harmless"], "value");
    }

    #[test]
    fn redact_secrets_leaves_non_object_payloads_alone() {
        let n = redact_secrets(&serde_json::json!(42));
        assert_eq!(n, serde_json::json!(42));
        let s = redact_secrets(&serde_json::json!("just a string"));
        assert_eq!(s, serde_json::json!("just a string"));
    }

    #[tokio::test]
    async fn add_memory_card_rejects_nul_and_other_control_bytes() {
        // Regression for M-sec-5 — NUL / control bytes break grep
        // and git tooling that scans `~/.atelier/memory/` markdown.
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let err = sd
            .add_memory_card("hello\0world".into(), "2026-05-17T10:00:00Z")
            .unwrap_err();
        assert!(matches!(err, crate::memory::MemoryError::InvalidContent(_)));
    }

    #[tokio::test]
    async fn add_memory_card_rejects_forged_yaml_frontmatter_delimiters() {
        // Regression for M-sec-5 — a `---` line would close (or
        // reopen) the YAML frontmatter that `MemoryStore::promote_to_global`
        // wraps the content in, polluting the promoted file's
        // metadata.
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let hostile = "harmless prefix\n---\nforged: malicious\n";
        let err = sd
            .add_memory_card(hostile.into(), "2026-05-17T10:00:00Z")
            .unwrap_err();
        assert!(matches!(err, crate::memory::MemoryError::InvalidContent(_)));
    }

    #[tokio::test]
    async fn add_memory_card_rejects_unicode_control_chars_v58() {
        // Regression for L-sec-3 — v57 only rejected ASCII controls;
        // v58 expanded to DEL, C1 controls (incl. NEL U+0085),
        // U+2028/U+2029 (line/paragraph separators), and bidi
        // marks/overrides (U+200E/F, U+202A-E — Trojan Source).
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let cases = [
            ('\u{007F}', "DEL"),
            ('\u{0085}', "NEL"),
            ('\u{0091}', "C1 PU1"),
            ('\u{2028}', "LINE SEP"),
            ('\u{2029}', "PARA SEP"),
            ('\u{200E}', "LRM"),
            ('\u{200F}', "RLM"),
            ('\u{202E}', "RLO (Trojan Source)"),
            // v59 (L-sec-3 extension) — bidi isolate variants.
            ('\u{2066}', "LRI"),
            ('\u{2067}', "RLI"),
            ('\u{2068}', "FSI"),
            ('\u{2069}', "PDI"),
        ];
        for (c, label) in cases {
            let content = format!("hi {c} world");
            let err = sd
                .add_memory_card(content, "2026-05-17T10:00:00Z")
                .unwrap_err();
            assert!(
                matches!(err, crate::memory::MemoryError::InvalidContent(_)),
                "{label} (U+{:04X}) should be rejected",
                c as u32
            );
        }
    }

    #[tokio::test]
    async fn add_memory_card_accepts_normal_unicode() {
        // Sanity — printable non-ASCII Unicode (CJK, emoji,
        // accented Latin) must be allowed.
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        sd.add_memory_card(
            "Café — 日本語 — 🦀 rusty crab".into(),
            "2026-05-17T10:00:00Z",
        )
        .unwrap();
        let _ = next_event(&mut rx).await;
    }

    #[tokio::test]
    async fn add_memory_card_accepts_tabs_and_newlines() {
        // Sanity — tabs + newlines are content, not control bytes.
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        sd.add_memory_card(
            "line1\n\tindented line2\nline3".into(),
            "2026-05-17T10:00:00Z",
        )
        .unwrap();
        let _ = next_event(&mut rx).await;
    }

    #[tokio::test]
    async fn delete_memory_card_removes_and_emits() {
        let (sd, _cm, ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd
            .add_memory_card("ephemeral".into(), "2026-05-17T10:00:00Z")
            .unwrap();
        // Drop the add's snapshot event.
        let _ = next_event(&mut rx).await;
        sd.delete_memory_card(&id).unwrap();
        assert_eq!(ms.lock().len(), 0);
        match next_event(&mut rx).await {
            Event::MemoryCards { cards } => assert!(cards.is_empty()),
            other => panic!("expected MemoryCards, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_memory_card_unknown_id_errors() {
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let err = sd.delete_memory_card("nope").unwrap_err();
        assert!(matches!(err, crate::memory::MemoryError::NotFound(_)));
    }

    #[tokio::test]
    async fn promote_memory_card_returns_bytes_and_touches_last_used() {
        let (sd, _cm, ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd
            .add_memory_card("important fact".into(), "2026-05-17T10:00:00Z")
            .unwrap();
        let _ = next_event(&mut rx).await; // initial MemoryCards from add
        let out = sd.promote_memory_card(&id, "2026-05-17T11:00:00Z").unwrap();
        assert!(out.relative_path.ends_with(".md"));
        assert!(!out.bytes.is_empty());
        assert_eq!(
            ms.lock().get(&id).unwrap().last_used,
            "2026-05-17T11:00:00Z"
        );
        // Re-emit fires after promote.
        match next_event(&mut rx).await {
            Event::MemoryCards { .. } => {}
            other => panic!("expected MemoryCards, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_plan_step_rejects_trojan_source_and_control_bytes() {
        // Regression for v59 MED-sec-2 — plan text rendered in the
        // GUI/TUI footer + plan pane + session.json must NOT accept
        // bidi overrides (U+202E) or other control characters.
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        for hostile in ["hi \u{202E} world", "step\0name", "two\u{2028}lines"] {
            let err = sd.add_plan_step(hostile.into()).unwrap_err();
            assert!(matches!(err, crate::plan::PlanError::InvalidContent(_)));
        }
    }

    #[tokio::test]
    async fn add_plan_step_constraint_rejects_trojan_source() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd.add_plan_step("step".into()).unwrap();
        let _ = next_event(&mut rx).await;
        let err = sd
            .add_plan_step_constraint(&id, "harmless prefix \u{202E} reversed".into())
            .unwrap_err();
        assert!(matches!(err, crate::plan::PlanError::InvalidContent(_)));
    }

    #[tokio::test]
    async fn add_plan_step_returns_id_and_emits_snapshot() {
        let (sd, _cm, _ms, pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd.add_plan_step("write the test".into()).unwrap();
        assert!(id.starts_with("step-"));
        assert_eq!(pc.lock().len(), 1);
        match next_event(&mut rx).await {
            Event::PlanSnapshot { steps } => {
                assert_eq!(steps.len(), 1);
                assert_eq!(steps[0].id, id);
            }
            other => panic!("expected PlanSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mark_plan_step_status_updates_and_emits() {
        let (sd, _cm, _ms, pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd.add_plan_step("do it".into()).unwrap();
        let _ = next_event(&mut rx).await; // initial PlanSnapshot
        sd.mark_plan_step_status(&id, PlanStatus::Done).unwrap();
        assert_eq!(pc.lock().get(&id).unwrap().status, PlanStatus::Done);
        match next_event(&mut rx).await {
            Event::PlanSnapshot { steps } => assert_eq!(steps[0].status, PlanStatus::Done),
            other => panic!("expected PlanSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_plan_step_constraint_appends_and_emits() {
        let (sd, _cm, _ms, pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd.add_plan_step("review".into()).unwrap();
        let _ = next_event(&mut rx).await;
        sd.add_plan_step_constraint(&id, "must not break api".into())
            .unwrap();
        assert_eq!(pc.lock().get(&id).unwrap().constraints.len(), 1);
        match next_event(&mut rx).await {
            Event::PlanSnapshot { steps } => {
                assert_eq!(steps[0].constraints, vec!["must not break api".to_string()]);
            }
            other => panic!("expected PlanSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reorder_plan_steps_rewrites_order_and_emits() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let a = sd.add_plan_step("a".into()).unwrap();
        let b = sd.add_plan_step("b".into()).unwrap();
        // drop two PlanSnapshots
        let _ = next_event(&mut rx).await;
        let _ = next_event(&mut rx).await;
        sd.reorder_plan_steps(vec![b.clone(), a.clone()]).unwrap();
        match next_event(&mut rx).await {
            Event::PlanSnapshot { steps } => {
                assert_eq!(
                    steps.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
                    vec![b, a]
                );
            }
            other => panic!("expected PlanSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_plan_step_drops_and_emits() {
        let (sd, _cm, _ms, pc, _ledger, mut rx) = build_v55_dispatcher();
        let id = sd.add_plan_step("temp".into()).unwrap();
        let _ = next_event(&mut rx).await;
        sd.remove_plan_step(&id).unwrap();
        assert!(pc.lock().is_empty());
        match next_event(&mut rx).await {
            Event::PlanSnapshot { steps } => assert!(steps.is_empty()),
            other => panic!("expected PlanSnapshot, got {other:?}"),
        }
    }

    // ---------- v60.5: compact_context_items ----------

    #[tokio::test]
    async fn compact_context_items_evicts_creates_summary_card_and_ledgers() {
        let (sd, cm, ms, _pc, ledger, mut rx) = build_v55_dispatcher();
        let a = seed_context_item(&cm, 100);
        let b = seed_context_item(&cm, 150);
        let _kept = seed_context_item(&cm, 50);

        let out = sd
            .compact_context_items(
                vec![a.to_string(), b.to_string()],
                "Summary: a + b discussed module X.".into(),
                ".atelier/sessions/sid/compactions/comp-test.json".into(),
                "2026-05-17T11:00:00Z",
            )
            .expect("compact must succeed");
        assert_eq!(out.freed_tokens, 250);
        assert!(out.summary_card_id.starts_with("mem-"));

        // Context state: only the kept item remains.
        assert_eq!(cm.lock().len(), 1);

        // Memory state: one pinned card with the compacted_from link.
        let cards = ms.lock().to_vec();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, out.summary_card_id);
        assert!(cards[0].pinned);
        let cs = cards[0].compacted_from.as_ref().expect("compacted_from");
        assert_eq!(cs.item_ids, vec![a.to_string(), b.to_string()]);
        assert_eq!(
            cs.expansion_blob_path,
            ".atelier/sessions/sid/compactions/comp-test.json"
        );
        assert_eq!(cs.compacted_at, "2026-05-17T11:00:00Z");

        // Ledger has a single Compaction entry.
        let entries = ledger.to_vec();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            LedgerEntry::Compaction {
                freed_tokens,
                replaced_items,
                summary_card_id,
                ..
            } => {
                assert_eq!(*freed_tokens, 250);
                assert_eq!(replaced_items.len(), 2);
                assert_eq!(summary_card_id, &out.summary_card_id);
            }
            other => panic!("expected Compaction, got {other:?}"),
        }

        // Event broadcast order: LedgerAppended → ContextItems → MemoryCards → CompactionExecuted.
        match next_event(&mut rx).await {
            Event::LedgerAppended { entry } => {
                assert!(matches!(entry, LedgerEntry::Compaction { .. }))
            }
            other => panic!("expected LedgerAppended, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::ContextItems { items } => assert_eq!(items.len(), 1),
            other => panic!("expected ContextItems, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::MemoryCards { cards } => assert_eq!(cards.len(), 1),
            other => panic!("expected MemoryCards, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::CompactionExecuted {
                freed_tokens,
                replaced_item_count,
                summary_card_id,
            } => {
                assert_eq!(freed_tokens, 250);
                assert_eq!(replaced_item_count, 2);
                assert_eq!(summary_card_id, out.summary_card_id);
            }
            other => panic!("expected CompactionExecuted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_context_items_empty_selection_returns_error() {
        let (sd, _cm, _ms, _pc, ledger, _rx) = build_v55_dispatcher();
        let err = sd
            .compact_context_items(vec![], "summary".into(), "blob.json".into(), "t")
            .unwrap_err();
        assert_eq!(err, CompactionError::Empty);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn compact_context_items_rejects_pinned_item_atomically() {
        let (sd, cm, ms, _pc, ledger, _rx) = build_v55_dispatcher();
        let a = seed_context_item(&cm, 100);
        let b = seed_context_item(&cm, 50);
        cm.lock().pin(b).unwrap();

        let err = sd
            .compact_context_items(
                vec![a.to_string(), b.to_string()],
                "summary".into(),
                "blob.json".into(),
                "t",
            )
            .unwrap_err();
        assert!(matches!(
            err,
            CompactionError::Context(crate::context::ContextError::EvictPinned(_))
        ));
        // Atomicity: a is still in the store (Pass-1 of evict_batch refused).
        assert_eq!(cm.lock().len(), 2);
        assert_eq!(ms.lock().to_vec().len(), 0);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn compact_context_items_rejects_unknown_item() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let _a = seed_context_item(&cm, 10);
        let ghost = uuid::Uuid::new_v4().to_string();

        let err = sd
            .compact_context_items(
                vec![ghost.clone()],
                "summary".into(),
                "blob.json".into(),
                "t",
            )
            .unwrap_err();
        assert!(matches!(
            err,
            CompactionError::Context(crate::context::ContextError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn compact_context_items_rejects_malformed_id() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let _a = seed_context_item(&cm, 10);

        let err = sd
            .compact_context_items(
                vec!["not-a-uuid".into()],
                "summary".into(),
                "blob.json".into(),
                "t",
            )
            .unwrap_err();
        assert!(matches!(
            err,
            CompactionError::Context(crate::context::ContextError::Malformed(s)) if s == "not-a-uuid"
        ));
    }

    #[tokio::test]
    async fn compact_context_items_rejects_summary_with_trojan_source() {
        let (sd, cm, ms, _pc, ledger, _rx) = build_v55_dispatcher();
        let a = seed_context_item(&cm, 10);

        let err = sd
            .compact_context_items(
                vec![a.to_string()],
                "harmless looking\u{202E}reversed".into(),
                "blob.json".into(),
                "t",
            )
            .unwrap_err();
        assert!(matches!(err, CompactionError::InvalidSummary(_)));
        // No state change.
        assert_eq!(cm.lock().len(), 1);
        assert_eq!(ms.lock().to_vec().len(), 0);
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn compact_context_items_rejects_summary_with_frontmatter_delimiter() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let a = seed_context_item(&cm, 10);

        let err = sd
            .compact_context_items(
                vec![a.to_string()],
                "alpha\n---\nforged frontmatter".into(),
                "blob.json".into(),
                "t",
            )
            .unwrap_err();
        assert!(matches!(err, CompactionError::InvalidSummary(_)));
    }

    #[tokio::test]
    async fn snapshot_context_items_returns_clones_in_input_order() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let a = seed_context_item(&cm, 1);
        let b = seed_context_item(&cm, 2);
        let c = seed_context_item(&cm, 3);

        // Request out of insertion order; result should follow the input.
        let items = sd
            .snapshot_context_items(&[c.to_string(), a.to_string(), b.to_string()])
            .expect("snapshot must succeed");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, c);
        assert_eq!(items[1].id, a);
        assert_eq!(items[2].id, b);
        // Items still in the manager (not evicted).
        assert_eq!(cm.lock().len(), 3);
    }

    #[tokio::test]
    async fn snapshot_context_items_rejects_unknown_id() {
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let ghost = uuid::Uuid::new_v4().to_string();
        let err = sd.snapshot_context_items(&[ghost]).unwrap_err();
        assert!(matches!(err, crate::context::ContextError::NotFound(_)));
    }

    // ---------- v60.6: expand_memory_card + snapshot_memory_card ----------

    /// Drive a compaction end-to-end through the dispatcher and return
    /// (a) the resulting summary `card_id`, (b) the items that were
    /// evicted (so the test can simulate "read the blob" by passing
    /// them straight back in). Keeps the v60.6 tests focused on the
    /// expansion path rather than re-asserting the compaction shape.
    fn compact_and_capture(
        sd: &SessionDispatcher,
        cm: &Arc<parking_lot::Mutex<crate::context::ContextManager>>,
        tokens: &[u32],
    ) -> (String, Vec<crate::context::ContextItem>) {
        let ids: Vec<crate::context::ContextItemId> =
            tokens.iter().map(|t| seed_context_item(cm, *t)).collect();
        let id_strings: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        // Snapshot BEFORE compaction so we have the items to pass back
        // into `expand_memory_card` (the orchestrator would normally
        // read these from the on-disk blob).
        let items = sd.snapshot_context_items(&id_strings).expect("snapshot");
        let out = sd
            .compact_context_items(
                id_strings,
                "summary line".into(),
                ".atelier/sessions/sid/compactions/comp-test.json".into(),
                "2026-05-17T11:00:00Z",
            )
            .expect("compact must succeed");
        (out.summary_card_id, items)
    }

    #[tokio::test]
    async fn snapshot_memory_card_returns_cloned_card() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let (card_id, _items) = compact_and_capture(&sd, &cm, &[10, 20]);
        let card = sd.snapshot_memory_card(&card_id).expect("must be present");
        assert_eq!(card.id, card_id);
        assert!(card.pinned);
        assert!(card.compacted_from.is_some());
        let cs = card.compacted_from.unwrap();
        assert_eq!(cs.item_ids.len(), 2);
        assert_eq!(cs.cache_rewarm_tokens, 30);
    }

    #[tokio::test]
    async fn snapshot_memory_card_returns_none_for_missing_id() {
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        assert!(sd.snapshot_memory_card("mem-nope").is_none());
    }

    #[tokio::test]
    async fn expand_memory_card_restores_items_drops_card_and_ledgers() {
        let (sd, cm, ms, _pc, ledger, mut rx) = build_v55_dispatcher();
        let (card_id, items) = compact_and_capture(&sd, &cm, &[10, 20]);
        // Drain the compaction-side events so the expansion events
        // arrive at the head of the receiver.
        while rx.try_recv().is_ok() {}
        assert_eq!(cm.lock().len(), 0, "items must be gone post-compaction");
        assert_eq!(ms.lock().len(), 1, "summary card must be present");

        let out = sd
            .expand_memory_card(card_id.clone(), items, "2026-05-17T12:00:00Z")
            .expect("expand must succeed");
        assert_eq!(out.restored_item_count, 2);
        assert_eq!(out.cache_rewarm_tokens, 30);
        assert_eq!(out.summary_card_id, card_id);

        // Context state: items are back.
        assert_eq!(cm.lock().len(), 2);
        // Memory state: summary card gone.
        assert_eq!(ms.lock().len(), 0);

        // Ledger: ModelCall(none here) + Compaction (from earlier) + Expansion.
        let entries = ledger.to_vec();
        let expansion = entries
            .iter()
            .rev()
            .find(|e| matches!(e, LedgerEntry::Expansion { .. }))
            .expect("must have one Expansion entry");
        match expansion {
            LedgerEntry::Expansion {
                cache_rewarm_tokens,
                summary_card_id,
                restored_item_ids,
                ..
            } => {
                assert_eq!(*cache_rewarm_tokens, 30);
                assert_eq!(summary_card_id, &card_id);
                assert_eq!(restored_item_ids.len(), 2);
            }
            _ => unreachable!(),
        }

        // Event order: LedgerAppended(Expansion) → ContextItems → MemoryCards → ExpansionExecuted.
        match next_event(&mut rx).await {
            Event::LedgerAppended { entry } => {
                assert!(matches!(entry, LedgerEntry::Expansion { .. }))
            }
            other => panic!("expected LedgerAppended, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::ContextItems { items } => assert_eq!(items.len(), 2),
            other => panic!("expected ContextItems, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::MemoryCards { cards } => assert!(cards.is_empty()),
            other => panic!("expected MemoryCards, got {other:?}"),
        }
        match next_event(&mut rx).await {
            Event::ExpansionExecuted {
                restored_item_count,
                summary_card_id,
                cache_rewarm_tokens,
            } => {
                assert_eq!(restored_item_count, 2);
                assert_eq!(summary_card_id, card_id);
                assert_eq!(cache_rewarm_tokens, 30);
            }
            other => panic!("expected ExpansionExecuted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expand_memory_card_unknown_card_returns_error() {
        let (sd, _cm, _ms, _pc, ledger, _rx) = build_v55_dispatcher();
        let err = sd
            .expand_memory_card("mem-nope".into(), vec![], "t")
            .unwrap_err();
        assert!(matches!(err, ExpansionError::CardNotFound(_)));
        assert_eq!(ledger.len(), 0);
    }

    #[tokio::test]
    async fn expand_memory_card_non_compaction_card_returns_error() {
        let (sd, _cm, _ms, _pc, ledger, _rx) = build_v55_dispatcher();
        let plain_id = sd
            .add_memory_card("ordinary card".into(), "2026-05-17T10:00:00Z")
            .unwrap();
        let err = sd.expand_memory_card(plain_id, vec![], "t").unwrap_err();
        assert!(matches!(err, ExpansionError::NotACompactionCard(_)));
        // No expansion ledger entry written.
        let n_expansions = ledger
            .to_vec()
            .iter()
            .filter(|e| matches!(e, LedgerEntry::Expansion { .. }))
            .count();
        assert_eq!(n_expansions, 0);
    }

    #[tokio::test]
    async fn expand_memory_card_item_count_mismatch_rejects_atomically() {
        let (sd, cm, ms, _pc, ledger, _rx) = build_v55_dispatcher();
        let (card_id, mut items) = compact_and_capture(&sd, &cm, &[10, 20]);
        items.pop(); // 2 ids on the card, only 1 item supplied

        let err = sd.expand_memory_card(card_id, items, "t").unwrap_err();
        assert!(matches!(
            err,
            ExpansionError::ItemMismatch {
                expected: 2,
                got: 1
            }
        ));
        // Atomicity: card still present, no Expansion entry written.
        assert_eq!(ms.lock().len(), 1);
        assert_eq!(cm.lock().len(), 0);
        let n_expansions = ledger
            .to_vec()
            .iter()
            .filter(|e| matches!(e, LedgerEntry::Expansion { .. }))
            .count();
        assert_eq!(n_expansions, 0);
    }

    #[tokio::test]
    async fn expand_memory_card_item_id_mismatch_rejects_atomically() {
        let (sd, cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let (card_id, items) = compact_and_capture(&sd, &cm, &[10, 20]);
        // Swap the two items' positions — count matches, ids don't.
        let swapped = vec![items[1].clone(), items[0].clone()];
        let err = sd.expand_memory_card(card_id, swapped, "t").unwrap_err();
        assert!(matches!(
            err,
            ExpansionError::ItemIdMismatch { position: 0, .. }
        ));
    }

    #[tokio::test]
    async fn expand_memory_card_id_collision_rolls_back_via_add_batch() {
        // If the user has somehow re-introduced an item with the same id
        // since compaction (e.g., via an alternate restore path),
        // expansion must refuse to overwrite it.
        let (sd, cm, ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let (card_id, items) = compact_and_capture(&sd, &cm, &[10, 20]);
        // Pre-insert one of the items back into the manager — re-using
        // its original id — before calling expand. add_batch's Pass 1
        // must reject the whole batch.
        cm.lock().add(items[0].clone());
        assert_eq!(cm.lock().len(), 1);

        let err = sd.expand_memory_card(card_id, items, "t").unwrap_err();
        assert!(matches!(
            err,
            ExpansionError::Context(crate::context::ContextError::AlreadyPresent(_))
        ));
        // Card still present (atomicity).
        assert_eq!(ms.lock().len(), 1);
    }

    // ---------- Phase C close: set_mental_model ----------

    #[tokio::test]
    async fn set_mental_model_round_trips_through_dispatcher() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let snap = sd
            .set_mental_model(
                "the user wants brevity".into(),
                true,
                "2026-05-17T12:00:00Z",
            )
            .unwrap();
        assert!(snap.enabled);
        assert_eq!(snap.text, "the user wants brevity");
        assert!(snap.text_tokens > 0);
        match next_event(&mut rx).await {
            Event::MentalModelSnapshot {
                enabled,
                text_tokens,
            } => {
                assert!(enabled);
                assert_eq!(text_tokens, snap.text_tokens);
            }
            other => panic!("expected MentalModelSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_mental_model_can_toggle_off_without_clearing_text() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        sd.set_mental_model("notes".into(), true, "t1").unwrap();
        let _ = next_event(&mut rx).await;
        let snap = sd.set_mental_model("notes".into(), false, "t2").unwrap();
        assert!(!snap.enabled);
        assert_eq!(snap.text, "notes");
        match next_event(&mut rx).await {
            Event::MentalModelSnapshot { enabled, .. } => assert!(!enabled),
            other => panic!("expected MentalModelSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_mental_model_rejects_invalid_text_atomically() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        // Seed a valid snapshot first so we can prove state stays put on the
        // rejected call below.
        sd.set_mental_model("baseline".into(), true, "t1").unwrap();
        let _ = next_event(&mut rx).await;
        let err = sd
            .set_mental_model("contains\0nul".into(), true, "t2")
            .unwrap_err();
        assert!(matches!(
            err,
            crate::mental_model::MentalModelError::InvalidText(_)
        ));
        // State unchanged on error.
        let snap = sd.snapshot_mental_model();
        assert_eq!(snap.text, "baseline");
        assert_eq!(snap.updated_at, "t1");
    }

    #[test]
    fn snapshot_mental_model_defaults_to_disabled_empty() {
        let (sd, _cm, _ms, _pc, _ledger, _rx) = build_v55_dispatcher();
        let snap = sd.snapshot_mental_model();
        assert!(!snap.enabled);
        assert!(snap.text.is_empty());
        assert_eq!(snap.text_tokens, 0);
    }

    // ---------- v61 — §14 concurrent-edit smoke tests ----------

    #[test]
    fn concurrent_edit_policy_default_is_modal() {
        // Pin the default so a future Default derive change is a
        // conscious migration — the GUI / TUI render an unsuppressed
        // modal under Modal, which is the user-visible safe choice.
        let policy = ConcurrentEditPolicy::default();
        assert_eq!(policy, ConcurrentEditPolicy::Modal);
    }

    #[test]
    fn extract_read_paths_resolves_relative_against_workspace() {
        let ws = std::path::Path::new("/tmp/repo");
        let paths =
            super::extract_read_paths("read_file", &serde_json::json!({"path": "src/main.rs"}), ws);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], std::path::Path::new("/tmp/repo/src/main.rs"));
    }

    #[test]
    fn extract_read_paths_keeps_absolute_paths_untouched() {
        let ws = std::path::Path::new("/tmp/repo");
        let paths = super::extract_read_paths(
            "list_dir",
            &serde_json::json!({"path": "/abs/elsewhere"}),
            ws,
        );
        assert_eq!(paths, vec![std::path::PathBuf::from("/abs/elsewhere")]);
    }

    #[test]
    fn extract_read_paths_for_grep_falls_back_to_workspace_root() {
        let ws = std::path::Path::new("/tmp/repo");
        let paths = super::extract_read_paths(
            "grep",
            &serde_json::json!({"pattern": "foo"}), // no `path` key
            ws,
        );
        assert_eq!(paths, vec![std::path::PathBuf::from("/tmp/repo")]);
    }

    #[test]
    fn extract_read_paths_for_unknown_tool_is_empty() {
        let ws = std::path::Path::new("/tmp/repo");
        let paths = super::extract_read_paths("write_file", &serde_json::json!({"path": "x"}), ws);
        assert!(paths.is_empty(), "write_file isn't a read tool: {paths:?}");
    }

    #[tokio::test]
    async fn resolve_concurrent_edit_emits_files_changed_acknowledged() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        sd.resolve_concurrent_edit(crate::session::ConcurrentEditOutcome::Reload);
        // Drain until we see the ack — other v55 setup events may
        // beat it to the queue.
        let mut saw = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(crate::session::Event::FilesChangedAcknowledged { outcome })) => {
                    assert_eq!(outcome, crate::session::ConcurrentEditOutcome::Reload);
                    saw = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw, "resolve_concurrent_edit must publish on the bus");
    }

    // ---------- v62: §7 verify-pass tier indicator ----------

    /// Discrepancy case (b.py claimed Create but never observed): the
    /// §7 lying-agent gate must emit `VerificationFailed`, NOT
    /// `VerificationPassed`. The Failed variant carries the
    /// discrepancy list verbatim for downstream consumers.
    #[tokio::test]
    async fn verify_pass_emits_failed_event_when_discrepancies_present() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let env = crate::protocol::Envelope {
            claimed_changes: Some(vec![
                crate::protocol::ClaimedChange {
                    path: "a.py".into(),
                    kind: crate::protocol::ClaimedChangeKind::Edit,
                    summary: "tweak fn foo".into(),
                },
                crate::protocol::ClaimedChange {
                    path: "b.py".into(),
                    kind: crate::protocol::ClaimedChangeKind::Create,
                    summary: "new helper".into(),
                },
            ]),
            ..Default::default()
        };
        let observed = vec![crate::verify::ObservedChange {
            path: "a.py".into(),
            kind: crate::verify::ObservedKind::Modified,
        }];

        let run = sd.verify_pass(&env, &observed);
        assert_eq!(run.tier, crate::verify::VerificationTier::Tier3Textual);
        assert_eq!(run.claim_count, 2);
        assert_eq!(run.file_count, 2); // a.py + b.py
        assert_eq!(run.discrepancies.len(), 1);

        match next_event(&mut rx).await {
            Event::VerificationFailed {
                tier,
                discrepancies,
            } => {
                assert_eq!(tier, crate::verify::VerificationTier::Tier3Textual);
                assert_eq!(discrepancies.len(), 1);
                assert!(
                    matches!(
                        &discrepancies[0],
                        crate::verify::Discrepancy::Claimed { path, .. } if path == "b.py"
                    ),
                    "expected Claimed{{b.py}}; got {:?}",
                    discrepancies[0],
                );
            }
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    /// No-discrepancy case (every claim observed, no silent edits):
    /// the §7 gate emits `VerificationPassed` with file + claim counts.
    /// Pairs with `verify_pass_emits_failed_event_when_discrepancies_present`
    /// to pin both arms of the `verify_pass` branch.
    #[tokio::test]
    async fn verify_pass_emits_passed_event_when_workspace_agrees() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        let env = crate::protocol::Envelope {
            claimed_changes: Some(vec![crate::protocol::ClaimedChange {
                path: "a.py".into(),
                kind: crate::protocol::ClaimedChangeKind::Edit,
                summary: "tweak fn foo".into(),
            }]),
            ..Default::default()
        };
        let observed = vec![crate::verify::ObservedChange {
            path: "a.py".into(),
            kind: crate::verify::ObservedKind::Modified,
        }];

        let run = sd.verify_pass(&env, &observed);
        assert_eq!(run.tier, crate::verify::VerificationTier::Tier3Textual);
        assert!(run.discrepancies.is_empty());

        match next_event(&mut rx).await {
            Event::VerificationPassed {
                tier,
                file_count,
                claim_count,
            } => {
                assert_eq!(tier, crate::verify::VerificationTier::Tier3Textual);
                assert_eq!(file_count, 1);
                assert_eq!(claim_count, 1);
            }
            other => panic!("expected VerificationPassed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_verify_not_run_publishes_not_run_tier() {
        let (sd, _cm, _ms, _pc, _ledger, mut rx) = build_v55_dispatcher();
        sd.emit_verify_not_run();
        match next_event(&mut rx).await {
            Event::VerificationPassed {
                tier,
                file_count,
                claim_count,
            } => {
                assert_eq!(tier, crate::verify::VerificationTier::NotRun);
                assert_eq!(file_count, 0);
                assert_eq!(claim_count, 0);
            }
            other => panic!("expected VerificationPassed(NotRun), got {other:?}"),
        }
    }
}
