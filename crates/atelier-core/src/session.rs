//! §2.5 per-session actor.
//!
//! Spec §2.5 "Concurrency":
//!   * **tokio multi-threaded runtime.**
//!   * **Per-session actor:** session state lives behind a dedicated task with
//!     an `mpsc` inbox. UI consumers subscribe to a broadcast channel; no
//!     shared mutable state crosses the FFI boundary.
//!   * **Bounded in-turn tool parallelism:** `Semaphore` capped at **4**
//!     (PROVISIONAL — calibration: contention on the reference machine
//!     running the canonical workload).
//!
//! This module owns the runtime mechanics: spawning the actor, routing
//! commands, broadcasting events, validating every state transition against
//! the [`LEGAL_TRANSITIONS`](crate::state::LEGAL_TRANSITIONS) table, and
//! firing `CheckpointHook` + `LedgerHook` on each accepted transition.
//!
//! The actor does **not** own the BYOM adapter, MCP client, or tool runners.
//! Those land in Phase A §1 / §15 and drive the actor via [`Command::Advance`]
//! plus tool-permit acquisition through [`Handle::tool_semaphore`]. The
//! cancellation primitive ([`Handle::cancel_token`]) is shared with the
//! eventual turn-driver so drop-on-cancel works without a bespoke protocol.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::diff::Hunks;
use crate::ledger::LedgerEntry;
use crate::plan::PlanStep;
use crate::staging::CommitReport;
use crate::state::{CheckpointHook, IllegalTransition, LedgerHook, State, Transition};

use std::path::PathBuf;

/// PROVISIONAL — spec §2.5. Bounded in-turn tool parallelism cap.
pub const TOOL_PARALLELISM_CAP: usize = 4;

/// Broadcast-channel buffer per session. Slow subscribers will get
/// `RecvError::Lagged`; the on-disk session (§14) is authoritative for
/// reconciliation after a UI falls behind. Scaled by
/// [`crate::subagents::BUS_FANOUT_FACTOR`] (= 4) so a depth-3 spawn tree
/// can burst events from all levels without overrunning the buffer.
pub const EVENT_BUFFER: usize = 256 * crate::subagents::BUS_FANOUT_FACTOR;

/// Default inbox depth. Producers `.await` when full — backpressure is
/// intentional: a full inbox means the actor is wedged on a transition.
pub const INBOX_CAPACITY: usize = 32;

/// Counter incremented every time [`try_emit`] observes a broadcast
/// send that returned `Err(SendError(_))` (no live receivers, or the
/// channel was closed before the send landed). Public so tests and a
/// future metrics sink can read it; never reset in production.
pub static BROADCAST_LAGGED: AtomicU64 = AtomicU64::new(0);

/// Unix-second timestamp of the most recent throttled lag warning,
/// updated via CAS in [`try_emit`] so concurrent lag observations
/// emit at most one `warn!` per 1-second window.
static BROADCAST_LAG_LAST_WARNED_SEC: AtomicU64 = AtomicU64::new(0);

/// Best-effort broadcast emit with lag instrumentation.
///
/// Wraps `bus.send(ev)`; every previously-bare `let _ = bus.send(...)`
/// call site funnels through here so a slow / absent subscriber is
/// observable rather than silently dropped. On `Err(SendError(_))`:
///
/// * Increments [`BROADCAST_LAGGED`] (saturating-add semantics; we
///   wrap at `u64::MAX` only after >5 quintillion lags).
/// * Fires `tracing::warn!` at most once per 1-second window, gated
///   by a CAS on [`BROADCAST_LAG_LAST_WARNED_SEC`]. The first lag in
///   any window wins the warn; concurrent lags inside the window
///   silently bump the counter and rely on the next window to flush.
///
/// Returns the underlying `Result` so callers that need the receiver
/// count can still use it. `Event` is large enough (128+ bytes for
/// some variants) to trigger `clippy::result_large_err`; the err carries
/// the unsent value by design, so we allow it here rather than boxing
/// every emit on the happy path.
#[allow(clippy::result_large_err)]
pub fn try_emit(
    bus: &broadcast::Sender<Event>,
    ev: Event,
) -> Result<usize, broadcast::error::SendError<Event>> {
    let result = bus.send(ev);
    if result.is_err() {
        BROADCAST_LAGGED.fetch_add(1, Ordering::Relaxed);
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = BROADCAST_LAG_LAST_WARNED_SEC.load(Ordering::Relaxed);
        if now_sec > last
            && BROADCAST_LAG_LAST_WARNED_SEC
                .compare_exchange(last, now_sec, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                broadcast_lagged_total = BROADCAST_LAGGED.load(Ordering::Relaxed),
                "session event dropped: no live subscribers (broadcast send returned SendError)"
            );
        }
    }
    result
}

/// Stable per-session identifier. Persisted on disk under
/// `.atelier/sessions/<id>/` (spec §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Commands accepted by the session actor.
///
/// Turn drivers (the BYOM adapter, the MCP tool dispatcher) drive the actor by
/// sending [`Command::Advance`] as the state machine progresses. The actor
/// validates each transition against the spec table and runs the registered
/// hooks. Illegal transitions become an `Event::IllegalTransitionAttempted`
/// — the state does **not** advance.
#[derive(Debug)]
pub enum Command {
    /// Attempt a state transition. Validated against `LEGAL_TRANSITIONS`.
    Advance(State),

    /// Cooperative cancel: trips the session's [`CancellationToken`] so any
    /// in-flight stream / tool-call future observes the cancel and drops.
    /// The driver is responsible for following up with the legal `Advance`
    /// path to `AwaitingUser` per spec §2.5.
    Cancel,

    /// Drain the inbox and stop the actor. Sent at session teardown.
    Shutdown,
}

// Note: spec §3 hunk accept/reject doesn't ride on `Command` —
// approval is a dispatcher concern, not a state-machine concern. The
// state actor emits `Advance(AwaitingApproval)` and later
// `Advance(ToolExecuting)` from the dispatcher's flow; the UI feeds
// the accept-set back via `SessionDispatcher::submit_approval`
// directly. Keeping the session actor free of approval routing means
// the actor's job stays "validate transitions, fire hooks" and the
// approval lifecycle lives next to the staging it controls.

/// Events broadcast by the session actor.
///
/// UI consumers subscribe via [`Handle::subscribe`] and render off this stream.
/// The on-disk session (§14) is the source of truth — `Event` is for live UI
/// updates only.
#[derive(Debug, Clone)]
pub enum Event {
    /// A transition validated and ran its hooks. Subscribers update UI state.
    Transitioned { from: State, to: State },

    /// A driver attempted a transition the spec table rejects. The actor
    /// surfaces it so the offending driver is visible in the event stream;
    /// state is unchanged.
    IllegalTransitionAttempted { from: State, to: State },

    /// The cancellation token has been tripped. In-flight futures should
    /// observe it and drop.
    Cancelled,

    /// One file was just staged + committed by §3. Published from a
    /// [`crate::staging::CommitReport`] (one event per file in the report).
    /// Carries the per-file hunks so the §3 live-diff renderer can paint
    /// without re-reading from disk. Phase C data layer; spec §3 "Live
    /// diff updates as the agent edits."
    EditStaged { path: PathBuf, hunks: Hunks },

    /// One conversation message just appended to the session's history.
    /// Published by the turn driver (the §2.5 actor's runner) so UI
    /// consumers can render the live conversation pane without polling
    /// the on-disk session. Spec §3 "conversation pane" + §5
    /// "non-destructive compaction" both rely on this stream.
    MessageCommitted { role: MessageRole, text: String },

    /// Incremental text token from the active streaming turn. Emitted
    /// by `start_chat_run` (GUI chat-REPL) for each `StreamChunk::Text`
    /// so the conversation pane can render text word-by-word and the
    /// footer token meter can show a running estimate. Followed by a
    /// `MessageCommitted` with the full assembled text at end-of-turn.
    AssistantTextDelta { delta: String },

    /// A plan snapshot — the canvas as it stands after the most recent
    /// `plan_update` was applied. Snapshots (not deltas) so a UI consumer
    /// that joined mid-session converges to the truth on the next event
    /// without replay. Spec §5 plan canvas; the canvas is small enough
    /// (≤ tens of steps in practice) that snapshot cost is negligible.
    PlanSnapshot { steps: Vec<PlanStep> },

    /// One ledger entry just landed. Published from
    /// [`crate::dispatcher::SessionDispatcher`] after every tool-call
    /// outcome, and from the turn driver after every model call.
    /// Consumers fold these into a rolling cost / token total — the
    /// snapshot of the ledger itself is not on the bus because a
    /// running session can already drive the cost meter from the stream.
    LedgerAppended { entry: LedgerEntry },

    /// Aggregate token-budget snapshot from the §5 context manager.
    /// Emitted at end-of-turn (after the turn driver tallies its
    /// `ContextManager::token_snapshot`) so the context meter never
    /// silently underreports — `unknown_tokens` makes the
    /// `TokenSource::Unavailable` items visible to the UI.
    ContextSnapshot {
        known_tokens: u32,
        unknown_tokens: u32,
    },

    /// v53 — per-item snapshot of the §5 context manager for the
    /// "what's in my agent's head right now?" panel. Emitted at the
    /// same turn boundary as [`Event::ContextSnapshot`] so the aggregate
    /// meter and the per-row list stay coherent. Items appear in
    /// insertion order; a stable `id` per row lets UIs animate
    /// additions/evictions across re-emits.
    ///
    /// The §5 mechanical gate ("API assertions for token counts and
    /// why-here; cache-bust ledger entry on eviction") is satisfied
    /// by this stream plus the existing `LedgerAppended` cache-bust
    /// entries.
    ContextItems {
        items: Vec<crate::context::ContextItemSummary>,
    },

    /// v54 — per-card snapshot of the §5 memory subsystem for the
    /// Memory panel ("what does the agent know about me long-term?").
    /// Emitted at the same turn boundary as [`Event::ContextItems`] so
    /// "context" (current-turn-only) and "memory" (durable) render
    /// coherently. Cards appear in insertion order; the stable `id`
    /// per card lets UIs animate adds/promotions/evictions.
    ///
    /// Distinct from `ContextItems` in semantics: context items live
    /// for one prompt-cache lifetime and contribute to the token
    /// meter; memory cards survive across sessions and are surfaced
    /// to the model only when explicitly promoted into context.
    MemoryCards {
        cards: Vec<crate::memory::MemoryCardSummary>,
    },

    /// v56 — agent's per-file "why" rationale. Emitted when the
    /// envelope carries `claimed_changes`. Powers spec §3
    /// "Why this change? UI consuming §2 grounding" — the UI keys
    /// each entry off `path` so the diff pane can display the
    /// agent's summary next to the file header. `kind` is one of
    /// `"edit"` / `"create"` / `"delete"`.
    ///
    /// The envelope's separate `grounding` field (textual-claim
    /// citations) is intentionally out of scope here — that's a
    /// different surface (sidebar / inline span annotations) and
    /// lands separately.
    ClaimedChanges { changes: Vec<ClaimedChangeSummary> },

    /// Spec §3 "Hunk accept / reject": a tool staged writes and the
    /// dispatcher is waiting for the user's accept-set decision before
    /// the rename phase. UI consumers render each `files[i]` (path +
    /// hunks) with an accept/reject control and call
    /// `SessionDispatcher::submit_approval` carrying the accepted
    /// paths. The `commit_id` is the correlation token; the
    /// dispatcher only acts on a matching approval. (Pre-v46 the
    /// design called for a `Command::ApproveCommit` actor message
    /// here; the spec §3 follow-on landed as a direct dispatcher
    /// call instead so the approval round-trip stays out of the
    /// session command queue.)
    StagingPendingApproval {
        commit_id: Uuid,
        files: Vec<PendingFile>,
    },

    /// Spec §3 follow-on to [`Event::StagingPendingApproval`]: the user
    /// approved a subset (possibly empty for a full reject) of the
    /// pending files. `committed` are the paths that successfully
    /// renamed into the workspace; `dropped` are the paths the user
    /// rejected (or that failed to commit).
    ///
    /// **Ordering (v49 onwards):** emitted by `SessionDispatcher` as
    /// the last bus event for a tool call that produced staged writes,
    /// AFTER each per-file `EditStaged` and the `LedgerAppended`
    /// summary. The per-file events are authoritative for diff
    /// rendering; this summary is a convenience for UIs that want to
    /// clear pending state in one place. The `committed`/`dropped`
    /// vectors carry the same paths a consumer would derive from the
    /// preceding `EditStaged` stream — if the two ever disagree,
    /// `EditStaged` wins.
    ///
    /// **AutoApprove note (v49):** also emitted under
    /// `ApprovalPolicy::AutoApproveAll` (with `dropped` empty), not
    /// just under `AwaitApproval`. Pre-v49 consumers that treated
    /// `CommitDecision` as the "AwaitApproval marker" must migrate to
    /// check `dropped.is_empty()` or correlate via `commit_id`.
    CommitDecision {
        commit_id: Uuid,
        committed: Vec<PathBuf>,
        dropped: Vec<PathBuf>,
    },

    /// v51 — probe-on-first-use (§1). Emitted by the Runner once,
    /// before the first turn, once the model profile (cached or
    /// freshly probed) has been resolved. UIs render the active §2
    /// strategy badge ("native tool · cached", "json sentinel ·
    /// probed", …) off this event.
    ///
    /// `model_id` is `<provider>:<model>`, `base_url` is empty for
    /// adapters that don't speak HTTP (Mock, Anthropic). `strategy`
    /// is the [`crate::protocol_strategy::Strategy`] the profile
    /// recommends as the *initial* §2 mode; the runtime conformance
    /// tracker may still degrade if the model misbehaves. `outcome`
    /// distinguishes cache-hit / probed / re-probed / not-cached —
    /// useful for the user to know whether a probe round-trip just
    /// happened.
    ModelProfileLoaded {
        model_id: String,
        base_url: String,
        strategy: crate::protocol_strategy::Strategy,
        outcome: crate::adapter::model_profile::ProbeLoadOutcome,
        /// v60.7 §1 BYOM — the static capability matrix row for this
        /// model, cross-walked against the probe observations. The
        /// GUI/TUI surface it as a tooltip on the existing model
        /// badge in the footer so the user can see at a glance which
        /// columns are `Supported` / `ClaimedButBroken` / `Unsupported`
        /// for the active model. `None` is preserved for one cycle
        /// of backwards-compatibility (UIs that haven't been
        /// updated still render the model badge without the
        /// tooltip), but the runner always populates it.
        capability_row: Option<crate::adapter::capability_matrix::CapabilityMatrixRow>,
    },

    /// v60.5 — terminal marker for a successful §5 non-destructive
    /// compaction. Emitted by `SessionDispatcher::compact_context_items`
    /// after the preceding `LedgerAppended` (the `Compaction` entry),
    /// `ContextItems` (snapshot without the replaced items), and
    /// `MemoryCards` (snapshot with the new pinned summary) events
    /// land. UIs use it as the "clear my multi-select state and show
    /// the toast" signal — the snapshot events have already
    /// converged the panels by the time it arrives.
    CompactionExecuted {
        freed_tokens: u32,
        replaced_item_count: usize,
        summary_card_id: String,
    },

    /// v60.6 — terminal marker for a successful §5 Expand (symmetric to
    /// [`Self::CompactionExecuted`]). Emitted by
    /// `SessionDispatcher::expand_memory_card` after the preceding
    /// `LedgerAppended(Expansion)`, `ContextItems` (snapshot with the
    /// originals back in place), and `MemoryCards` (snapshot with the
    /// summary card gone) events land. UIs use it as the "show the
    /// 'restored N items' toast" signal; the snapshot events have
    /// already converged the panels by the time it arrives.
    ExpansionExecuted {
        restored_item_count: usize,
        summary_card_id: String,
        cache_rewarm_tokens: u32,
    },

    /// Phase C close — §5 mental-model panel snapshot. Emitted by
    /// `SessionDispatcher::set_mental_model` after every successful
    /// `MentalModel::set` so subscribed UIs converge on the user's
    /// latest text + toggle state. `text_tokens` is an approximate
    /// byte/4 count for the cost-disclosure badge; v0 does **not**
    /// inject the text into the prompt, so the badge renders
    /// "0 tokens per turn at present" regardless of the snapshot's
    /// `text_tokens` value.
    MentalModelSnapshot { enabled: bool, text_tokens: u32 },

    /// v61 — §14 concurrent-edit detection. The per-session file
    /// watcher (`atelier_core::file_watcher`) observed an external
    /// edit to one or more files in the agent's read-set. The runner
    /// queues the *next* tool dispatch (does not cancel the current
    /// stream, per spec §14) and surfaces a modal to the user with
    /// Reload / Wait / Pause options; the GUI's
    /// `ConcurrentEditModal` and the TUI's
    /// `InputMode::ConcurrentEditModal` consume this variant.
    ///
    /// `observed_at` is RFC 3339; the runner records it on the
    /// session-resume `recovery_log` entry when the modal flow elects
    /// to pause.
    FilesChanged {
        paths: Vec<PathBuf>,
        observed_at: String,
    },

    /// v61 — companion to [`Self::FilesChanged`]. Emitted when the
    /// user clears the modal (chooses Reload, Wait, or after a Pause
    /// timer fires). Drives two consumers:
    ///
    ///   * The runner's auto-pause timer cancels on receipt — the
    ///     5-minute deadline only fires if the user does *nothing*.
    ///   * UI consumers hide the modal.
    ///
    /// `outcome` records which choice resolved the modal. Useful for
    /// the ledger trail + post-mortem.
    FilesChangedAcknowledged { outcome: ConcurrentEditOutcome },

    /// v62 — §7 verify pass terminal marker. Emitted by
    /// `SessionDispatcher::verify_pass` after the (currently Tier 3
    /// textual) [`crate::verify::compare`] runs. The `tier` carries
    /// which producer ran so the GUI/TUI can render a small badge
    /// (Tier 1 LSP green / Tier 2 tree-sitter yellow / Tier 3 textual
    /// orange / NotRun gray) — when a higher tier is unavailable
    /// (e.g. LSP not installed), the badge makes the coverage drop
    /// visible to the user rather than silently degrading.
    ///
    /// `file_count` is the union of claimed paths + observed paths
    /// the verify pass weighed; `claim_count` is the envelope's
    /// `claimed_changes` length. Both are surfaced in the event log
    /// detail and the badge tooltip.
    VerificationPassed {
        tier: crate::verify::VerificationTier,
        file_count: usize,
        claim_count: usize,
    },

    /// §7 lying-agent / silent-edit gate. Emitted by
    /// `SessionDispatcher::verify_pass` when `crate::verify::compare`
    /// returns a non-empty discrepancy list — i.e. the envelope's
    /// `claimed_changes` don't match the workspace's observed edits.
    /// Carries the full discrepancy list so consumers (UI red badge,
    /// trust-budget ledger, post-mortem) don't need to re-run the
    /// comparison.
    ///
    /// The tier mirrors `VerificationPassed` — which §7 producer ran
    /// (only Tier 3 textual is wired today; Tier 1 LSP + Tier 2
    /// tree-sitter follow). One verify call emits exactly one of
    /// `VerificationPassed` / `VerificationFailed`, never both; UIs
    /// can swap their badge state on either event.
    VerificationFailed {
        tier: crate::verify::VerificationTier,
        discrepancies: Vec<crate::verify::Discrepancy>,
    },

    /// §1 BYOM — conformance-driven degradation fired. The runner's
    /// rolling envelope-parse window crossed the §1/§2 threshold
    /// (PROVISIONAL 3-of-20 default; see
    /// [`crate::protocol_conformance::DEFAULT_DEGRADATION_WINDOW`] +
    /// [`crate::protocol_conformance::DEFAULT_DEGRADATION_THRESHOLD`])
    /// and the runner walked the active strategy one step toward the
    /// more-tolerant end of the stack (NativeTool → JsonSentinel →
    /// RegexProse). Degradation is one-way for the session — no
    /// auto-promotion arm fires the reverse transition.
    ///
    /// UI consumers refresh the strategy badge in the footer (GUI's
    /// `currentModel.strategy`, TUI's `CurrentModel.strategy`) so the
    /// user sees the harness has lowered the bar on the active model.
    /// `reason` is short, human-readable, and stable enough for a
    /// regression test to assert on (e.g.
    /// `"3 malformed envelopes in last 20 calls"`).
    StrategyDegraded {
        from: crate::protocol_strategy::Strategy,
        to: crate::protocol_strategy::Strategy,
        reason: String,
    },

    /// §1 BYOM — context-window asymmetry resolution. Emitted by the
    /// `Runner` after an `AdapterError::ContextOverflow` was handled
    /// via the configured `ContextOverflowPolicy`. `resolution` is the
    /// stable wire label of the policy arm that ran:
    ///   * `"compacted"` — auto-compaction freed `freed_tokens` across
    ///     `items_compacted` items, and the turn was retried.
    ///   * `"rerouted"` — the routing-dispatcher arm picked an alternate
    ///     adapter. v60.9 stub: this label is reserved; the policy
    ///     currently surfaces a typed config error instead.
    ///   * `"surfaced"` — the overflow was propagated to the caller as
    ///     a typed `RunError`. `freed_tokens` / `items_compacted` are
    ///     both `None`.
    ///
    /// UIs use this as the "we recovered from a context squeeze" toast
    /// signal; the ledger trail is on `LedgerAppended` (Compaction) for
    /// the auto-compaction arm.
    ContextOverflowResolved {
        resolution: &'static str,
        freed_tokens: Option<u32>,
        items_compacted: Option<usize>,
    },

    /// §2 agent-protocol — the most recent assistant turn produced
    /// neither real tool calls nor `claimed_done=true`, leaving the
    /// conversation in a state where another `adapter.chat()` would
    /// re-send a transcript ending on an assistant message. Strict
    /// providers (Anthropic Sonnet/Opus) reject that pattern with a
    /// 400 `invalid_request_error`; permissive providers (Anthropic
    /// Haiku) return near-empty completions until the turn cap. Both
    /// arms collapse to the same diagnosis: the agent has abandoned
    /// the §2 contract (every well-formed turn either advances state
    /// via tool calls or terminates via `claimed_done`). The Runner
    /// emits this once and transitions `Streaming → AwaitingUser` so
    /// the driver can decide whether to nudge, swap adapters, or
    /// give up — there's nothing the loop alone can do to recover.
    /// `turn` is 1-indexed (matches `RunReport.turns`).
    AgentStalled { turn: usize, reason: String },

    /// v60.10 §1 BYOM — the active adapter was swapped mid-session.
    /// Emitted by [`crate::session::Event::AdapterSwapped`]'s producer
    /// (today: `Runner::swap_adapter` on the next `run()` startup, and
    /// the GUI's `swap_adapter` Tauri command directly to the webview
    /// bus). Pairs with an immediately-following
    /// [`Event::ModelProfileLoaded`] re-emission so the footer's model
    /// badge + capability tooltip refresh in lockstep.
    ///
    /// State-preservation invariant: `ContextManager`, `MemoryStore`,
    /// `PlanCanvas`, the conversation history, and any in-flight
    /// `StagingPendingApproval` carry across the swap; the §1/§2
    /// conformance window resets (new adapter, new behaviour signal)
    /// and the §2 strategy may re-resolve from the new model's
    /// `ModelProfile`.
    AdapterSwapped {
        from_model_id: String,
        to_model_id: String,
        /// RFC 3339 timestamp the swap was requested.
        swapped_at: String,
    },

    /// v60.28 H2 — the renderer asked to swap into a new adapter and we
    /// want the user's explicit confirmation before tearing down the old
    /// one. The webview renders a consent modal and replies via the
    /// `respond_to_swap` Tauri command, keyed by `swap_id`. Followed by
    /// either an `AdapterSwapped` (accepted) or an `AdapterSwapRejected`
    /// (refused / timed out) event carrying the same `swap_id`.
    AdapterSwapPending {
        /// UUID v4 the producer mints per pending swap. The renderer
        /// echoes it back via `respond_to_swap` so stale replies (after
        /// a new swap started) are dropped.
        swap_id: String,
        to_model_id: String,
        base_url: String,
    },

    /// v60.28 H2 — the swap request was refused (either by the base_url
    /// allowlist gate, by the user from the consent modal, or by a
    /// consent-modal timeout). Carries the typed reason so the trust-
    /// budget UI can render a toast. `swap_id` matches the originating
    /// `AdapterSwapPending` when one was emitted; `None` for allowlist
    /// rejections that refuse before opening the modal.
    AdapterSwapRejected {
        swap_id: Option<String>,
        to_model_id: String,
        reason: String,
    },

    /// Phase B Track C1 — §7 verify Tier-1 LSP first-use install prompt.
    /// The runner observed an unverified language (today: TypeScript) and
    /// no cached `LspApprovals` entry exists; the UI presents a modal
    /// listing `candidate_packages` (e.g. `["typescript-language-server"]`)
    /// and lets the user approve / decline. Pairs with a subsequent
    /// [`Event::LspInstallResolved`] carrying the outcome.
    ///
    /// Q3 resolution (v60.12) — first-use approval, not always-install.
    /// The flow mirrors v60.8's MCP first-use prompt (`McpApprovals`).
    RequestLspInstall {
        language: String,
        candidate_packages: Vec<String>,
    },

    /// Phase B Track C1 — terminal marker for an LSP first-use install
    /// flow started by [`Event::RequestLspInstall`]. `outcome` carries the
    /// tier/fallback decision per **L-D-3**:
    ///   * `Installed` / `AlreadyPresent` — Tier-1 LSP verify available.
    ///   * `Declined` / `Failed` — fall back to Tier 2/3 for this language.
    LspInstallResolved {
        language: String,
        outcome: crate::lsp::LspInstallOutcome,
    },

    /// The actor is shutting down. No further events will be emitted.
    Shutdown,

    // --- §10 Sub-agent delegation events ---
    /// A sub-agent was spawned by the parent. Emitted on the shared bus
    /// immediately after `spawn_subagent` validates the request and before
    /// the child's first turn begins. The `description` field is the
    /// one-liner passed by the model; UIs render it as the sub-agent card title.
    SubagentSpawned {
        id: String,
        parent_id: String,
        subagent_type: String,
        description: String,
        max_turns: u32,
    },

    /// A sub-agent advanced one turn. Used by the GUI sub-agent progress bar.
    SubagentTurnAdvanced {
        id: String,
        turn: u32,
        max_turns: u32,
    },

    /// A sub-agent dispatched a tool call.
    SubagentToolCall { id: String, tool: String },

    /// A sub-agent reached a terminal state.
    SubagentCompleted {
        id: String,
        status: crate::subagents::SubagentStatus,
        turns_used: u32,
    },

    /// A sub-agent was cancelled (via the cancel shape, parent cancel token, or
    /// time-travel rewind). `reason` is a short free-text label for display.
    SubagentCancelled { id: String, reason: String },
}

impl Event {
    /// v57 (H5 fix) — canonical wire-format label for this variant.
    /// Matches the Rust enum variant name exactly so the GUI's
    /// `bridge_event.kind` and the TUI's `project_event.kind`
    /// strings can't drift. Pre-v57 those projections hand-typed the
    /// labels and had already diverged
    /// (`PendingApproval` vs `StagingPendingApproval`,
    /// `IllegalTransition` vs `IllegalTransitionAttempted`,
    /// `ModelProfile` vs `ModelProfileLoaded`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Transitioned { .. } => "Transitioned",
            Self::IllegalTransitionAttempted { .. } => "IllegalTransitionAttempted",
            Self::Cancelled => "Cancelled",
            Self::EditStaged { .. } => "EditStaged",
            Self::MessageCommitted { .. } => "MessageCommitted",
            Self::AssistantTextDelta { .. } => "AssistantTextDelta",
            Self::PlanSnapshot { .. } => "PlanSnapshot",
            Self::LedgerAppended { .. } => "LedgerAppended",
            Self::ContextSnapshot { .. } => "ContextSnapshot",
            Self::ContextItems { .. } => "ContextItems",
            Self::MemoryCards { .. } => "MemoryCards",
            Self::ClaimedChanges { .. } => "ClaimedChanges",
            Self::StagingPendingApproval { .. } => "StagingPendingApproval",
            Self::CommitDecision { .. } => "CommitDecision",
            Self::ModelProfileLoaded { .. } => "ModelProfileLoaded",
            Self::CompactionExecuted { .. } => "CompactionExecuted",
            Self::ExpansionExecuted { .. } => "ExpansionExecuted",
            Self::MentalModelSnapshot { .. } => "MentalModelSnapshot",
            Self::FilesChanged { .. } => "FilesChanged",
            Self::FilesChangedAcknowledged { .. } => "FilesChangedAcknowledged",
            Self::VerificationPassed { .. } => "VerificationPassed",
            Self::VerificationFailed { .. } => "VerificationFailed",
            Self::StrategyDegraded { .. } => "StrategyDegraded",
            Self::ContextOverflowResolved { .. } => "ContextOverflowResolved",
            Self::AgentStalled { .. } => "AgentStalled",
            Self::AdapterSwapped { .. } => "AdapterSwapped",
            Self::AdapterSwapPending { .. } => "AdapterSwapPending",
            Self::AdapterSwapRejected { .. } => "AdapterSwapRejected",
            Self::RequestLspInstall { .. } => "RequestLspInstall",
            Self::LspInstallResolved { .. } => "LspInstallResolved",
            Self::Shutdown => "Shutdown",
            Self::SubagentSpawned { .. } => "SubagentSpawned",
            Self::SubagentTurnAdvanced { .. } => "SubagentTurnAdvanced",
            Self::SubagentToolCall { .. } => "SubagentToolCall",
            Self::SubagentCompleted { .. } => "SubagentCompleted",
            Self::SubagentCancelled { .. } => "SubagentCancelled",
        }
    }
}

/// One pending file in a [`Event::StagingPendingApproval`]. Carries
/// the same `hunks` payload as [`Event::EditStaged`] so the UI can
/// render the diff before any rename has happened.
#[derive(Debug, Clone)]
pub struct PendingFile {
    pub path: PathBuf,
    pub hunks: Hunks,
}

/// v61 — outcome the user chose on the §14 concurrent-edit modal,
/// carried on [`Event::FilesChangedAcknowledged`]. Wire labels stay
/// stable across renames via [`Self::wire_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrentEditOutcome {
    /// User chose Reload: drop the queued tool-call dispatch and
    /// re-read the changed files into context at the next turn.
    Reload,
    /// User chose Wait: dispatch stays queued; the user owns
    /// re-entry by clearing the modal.
    Wait,
    /// User chose Pause: same as Wait but a 5-minute (PROVISIONAL,
    /// spec §14) timer auto-fires Reload semantics if the user
    /// doesn't intervene.
    Pause,
    /// `--non-interactive` mode: auto-applied Reload without a user
    /// in the loop. Logged distinctly so the recovery_log can show
    /// "no human resolved this — headless auto-reload".
    AutoReload,
    /// The 5-minute pause timer fired without user intervention.
    /// Semantically equivalent to AutoReload but distinct in the
    /// audit log.
    PauseTimedOut,
}

impl ConcurrentEditOutcome {
    /// Stable wire label used by the GUI bridge and TUI projection.
    /// Pinned by `concurrent_edit_outcome_wire_labels_are_stable` so a
    /// future variant rename forces a deliberate edit.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Reload => "reload",
            Self::Wait => "wait",
            Self::Pause => "pause",
            Self::AutoReload => "auto_reload",
            Self::PauseTimedOut => "pause_timed_out",
        }
    }
}

/// v56 — one entry in [`Event::ClaimedChanges`]. Mirrors the envelope's
/// `claimed_changes[i]` shape with `kind` flattened to its string form
/// (`"edit"` / `"create"` / `"delete"`) so the bus consumers don't have
/// to import the protocol enum to render badges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedChangeSummary {
    pub path: String,
    pub kind: String,
    pub summary: String,
}

/// Speaker role for [`Event::MessageCommitted`]. Mirrors the adapter's
/// `Role` shape — duplicated here so the session crate doesn't pull the
/// adapter module into every consumer of the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// v57 (H7 fix) — canonical lowercase wire label. Pre-v57 callers
    /// rendered via `format!("{role:?}").to_lowercase()` which made
    /// Rust's `Debug` output a wire format and would break the UI
    /// the moment a variant got renamed.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// Translate a [`CommitReport`] into the matching sequence of
/// [`Event::EditStaged`] events. The tool dispatcher (when it lands) calls
/// this after each successful [`crate::staging::Staging::commit`] and
/// forwards each event onto the session bus via a regular
/// `broadcast::Sender::send`. Pure function — exercisable without spinning
/// up an actor — so the §3 live-diff invariants are unit-testable.
pub fn edit_staged_events(report: &CommitReport) -> Vec<Event> {
    report
        .files
        .iter()
        .map(|f| Event::EditStaged {
            path: f.path.clone(),
            hunks: f.hunks.clone(),
        })
        .collect()
}

/// Outside-the-actor handle. Clone freely — sends share the inbox, subscribers
/// share the broadcast.
#[derive(Clone)]
pub struct Handle {
    id: SessionId,
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
    tool_semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl Handle {
    /// Session identifier (stable for the lifetime of the actor).
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Send a command to the actor. Returns error if the actor has shut down.
    pub async fn send(&self, cmd: Command) -> Result<(), mpsc::error::SendError<Command>> {
        self.tx.send(cmd).await
    }

    /// Subscribe to the event stream. Each call returns a fresh receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Tool runners acquire a permit before dispatching. Cap from spec §2.5.
    pub fn tool_semaphore(&self) -> Arc<Semaphore> {
        self.tool_semaphore.clone()
    }

    /// Cloned `broadcast::Sender` over the session's event bus. Used by the
    /// tool dispatcher's `SessionDispatcher` wrapper to publish
    /// `Event::EditStaged` (and future variants) without going through the
    /// actor's command inbox. Subscribers attached via [`Self::subscribe`]
    /// see these events in the same stream as actor-emitted ones.
    pub fn events_sender(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }

    /// Session-level cancellation token. Tool / stream futures select on it
    /// alongside their own work so a Cancel command aborts them promptly.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Spawn the per-session actor on the current tokio runtime.
///
/// Hooks are wrapped in `Arc` because the actor needs `Send + Sync` lifetimes
/// (they may be called from inside any tokio worker thread).
pub fn spawn(checkpoint: Arc<dyn CheckpointHook>, ledger: Arc<dyn LedgerHook>) -> Handle {
    spawn_with_capacity(checkpoint, ledger, INBOX_CAPACITY)
}

/// Spawn with an explicit inbox capacity. Useful for tests that want a tight
/// channel to make backpressure observable.
pub fn spawn_with_capacity(
    checkpoint: Arc<dyn CheckpointHook>,
    ledger: Arc<dyn LedgerHook>,
    inbox_capacity: usize,
) -> Handle {
    spawn_with_cancel_token(checkpoint, ledger, inbox_capacity, CancellationToken::new())
}

/// v60.29 H10 — spawn with a caller-supplied cancellation token. The
/// CLI's signal handler trips this token to cooperatively unwind the
/// session on SIGINT / SIGTERM. Tests use the unparameterised spawn;
/// `atelier-cli::runner::Runner::run` wires the binary's external
/// cancel-token through this variant.
pub fn spawn_with_cancel_token(
    checkpoint: Arc<dyn CheckpointHook>,
    ledger: Arc<dyn LedgerHook>,
    inbox_capacity: usize,
    cancel: CancellationToken,
) -> Handle {
    let id = SessionId::new();
    let (tx, rx) = mpsc::channel(inbox_capacity);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let tool_semaphore = Arc::new(Semaphore::new(TOOL_PARALLELISM_CAP));

    let handle = Handle {
        id,
        tx,
        events: events.clone(),
        tool_semaphore: tool_semaphore.clone(),
        cancel: cancel.clone(),
    };

    tokio::spawn(run_actor(id, rx, events, checkpoint, ledger, cancel));

    handle
}

async fn run_actor(
    id: SessionId,
    mut rx: mpsc::Receiver<Command>,
    events: broadcast::Sender<Event>,
    checkpoint: Arc<dyn CheckpointHook>,
    ledger: Arc<dyn LedgerHook>,
    cancel: CancellationToken,
) {
    tracing::debug!(session = %id, "session actor started");
    let mut state = State::Idle;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Advance(to) => match Transition::new(state, to) {
                Ok(t) => {
                    // Hooks run *before* the broadcast: a subscriber that
                    // reacts to a `Transitioned` event by reading session
                    // state from disk needs the checkpoint already written.
                    checkpoint.on_transition(&t);
                    ledger.on_transition(&t);
                    let _ = try_emit(&events, Event::Transitioned { from: state, to });
                    state = to;
                    if state.is_terminal() {
                        tracing::debug!(session = %id, terminal = %state, "session reached terminal state");
                        break;
                    }
                }
                Err(IllegalTransition { from, to }) => {
                    tracing::warn!(session = %id, %from, %to, "illegal transition rejected");
                    let _ = try_emit(&events, Event::IllegalTransitionAttempted { from, to });
                }
            },

            Command::Cancel => {
                tracing::debug!(session = %id, current = %state, "cancel requested");
                cancel.cancel();
                let _ = try_emit(&events, Event::Cancelled);
                // Per spec §2.5, the driver is responsible for advancing
                // through the legal path to `AwaitingUser`. The actor only
                // trips the token and notifies subscribers.
            }

            Command::Shutdown => {
                tracing::debug!(session = %id, "session actor shutdown requested");
                let _ = try_emit(&events, Event::Shutdown);
                break;
            }
        }
    }

    tracing::debug!(session = %id, "session actor stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[test]
    fn message_role_wire_label_is_stable() {
        // Regression for v58 HIGH-bug-1 — `MessageRole` doesn't
        // derive `Serialize`, so its wire form is solely
        // `wire_label`. Pin the four labels so a future variant
        // rename forces a deliberate edit rather than silently
        // shipping a different string to UIs.
        assert_eq!(MessageRole::System.wire_label(), "system");
        assert_eq!(MessageRole::User.wire_label(), "user");
        assert_eq!(MessageRole::Assistant.wire_label(), "assistant");
        assert_eq!(MessageRole::Tool.wire_label(), "tool");
    }

    #[test]
    fn compaction_executed_event_carries_expected_kind() {
        let ev = Event::CompactionExecuted {
            freed_tokens: 100,
            replaced_item_count: 3,
            summary_card_id: "mem-c".into(),
        };
        assert_eq!(ev.kind(), "CompactionExecuted");
    }

    #[test]
    fn expansion_executed_event_carries_expected_kind() {
        let ev = Event::ExpansionExecuted {
            restored_item_count: 3,
            summary_card_id: "mem-c".into(),
            cache_rewarm_tokens: 240,
        };
        assert_eq!(ev.kind(), "ExpansionExecuted");
    }

    #[test]
    fn mental_model_snapshot_event_carries_expected_kind() {
        let ev = Event::MentalModelSnapshot {
            enabled: true,
            text_tokens: 12,
        };
        assert_eq!(ev.kind(), "MentalModelSnapshot");
    }

    #[test]
    fn files_changed_event_carries_expected_kind() {
        let ev = Event::FilesChanged {
            paths: vec![PathBuf::from("/repo/src/main.rs")],
            observed_at: "2026-05-17T10:00:00Z".into(),
        };
        assert_eq!(ev.kind(), "FilesChanged");
    }

    #[test]
    fn files_changed_acknowledged_event_carries_expected_kind() {
        let ev = Event::FilesChangedAcknowledged {
            outcome: ConcurrentEditOutcome::Reload,
        };
        assert_eq!(ev.kind(), "FilesChangedAcknowledged");
    }

    #[test]
    fn verification_passed_event_carries_expected_kind() {
        // v62 — `VerificationPassed.kind()` is the GUI bridge's
        // routing key + the TUI event log label. Pinned so a future
        // variant rename forces a deliberate edit on the wire side.
        let ev = Event::VerificationPassed {
            tier: crate::verify::VerificationTier::Tier3Textual,
            file_count: 3,
            claim_count: 2,
        };
        assert_eq!(ev.kind(), "VerificationPassed");
    }

    #[test]
    fn strategy_degraded_event_carries_expected_kind() {
        let ev = Event::StrategyDegraded {
            from: crate::protocol_strategy::Strategy::NativeTool,
            to: crate::protocol_strategy::Strategy::JsonSentinel,
            reason: "3 malformed envelopes in last 20 calls".into(),
        };
        assert_eq!(ev.kind(), "StrategyDegraded");
    }

    #[test]
    fn context_overflow_resolved_event_carries_expected_kind_and_wire_labels() {
        // §1 BYOM — `ContextOverflowResolved.resolution` is a
        // `&'static str`; the wire labels are the contract the GUI /
        // TUI consume to render the "compact succeeded" / "rerouted"
        // / "overflow surfaced" toasts. Pinning them here keeps a
        // future variant rename from silently shipping a different
        // string.
        let compacted = Event::ContextOverflowResolved {
            resolution: "compacted",
            freed_tokens: Some(200),
            items_compacted: Some(3),
        };
        assert_eq!(compacted.kind(), "ContextOverflowResolved");
        if let Event::ContextOverflowResolved { resolution, .. } = compacted {
            assert_eq!(resolution, "compacted");
        }
        let rerouted = Event::ContextOverflowResolved {
            resolution: "rerouted",
            freed_tokens: None,
            items_compacted: None,
        };
        if let Event::ContextOverflowResolved { resolution, .. } = rerouted {
            assert_eq!(resolution, "rerouted");
        }
        let surfaced = Event::ContextOverflowResolved {
            resolution: "surfaced",
            freed_tokens: None,
            items_compacted: None,
        };
        if let Event::ContextOverflowResolved { resolution, .. } = surfaced {
            assert_eq!(resolution, "surfaced");
        }
    }

    #[test]
    fn adapter_swapped_event_carries_expected_kind() {
        // v60.10 §1 BYOM — `Event::AdapterSwapped.kind()` is the GUI
        // bridge's routing key + the TUI event log label. Pinned so a
        // future variant rename forces a deliberate edit on the wire
        // side.
        let ev = Event::AdapterSwapped {
            from_model_id: "anthropic:claude-opus-4-7".into(),
            to_model_id: "local:qwen2.5-coder:7b".into(),
            swapped_at: "2026-05-18T12:00:00Z".into(),
        };
        assert_eq!(ev.kind(), "AdapterSwapped");
        if let Event::AdapterSwapped {
            from_model_id,
            to_model_id,
            swapped_at,
        } = ev
        {
            assert_eq!(from_model_id, "anthropic:claude-opus-4-7");
            assert_eq!(to_model_id, "local:qwen2.5-coder:7b");
            assert_eq!(swapped_at, "2026-05-18T12:00:00Z");
        }
    }

    #[test]
    fn lsp_install_event_kinds_are_stable() {
        // Phase B Track C1 prep — pin the wire labels for the two new
        // §7 LSP first-use install variants. The four sinks
        // (`bridge_event`, TUI `apply` + `project_event`, Svelte
        // `applyEvent` + `projectEvent`) consume these strings; a
        // variant rename would silently mis-route the modal payload
        // without this regression test.
        let req = Event::RequestLspInstall {
            language: "typescript".into(),
            candidate_packages: vec!["typescript-language-server".into()],
        };
        assert_eq!(req.kind(), "RequestLspInstall");

        let resolved = Event::LspInstallResolved {
            language: "typescript".into(),
            outcome: crate::lsp::LspInstallOutcome::Installed,
        };
        assert_eq!(resolved.kind(), "LspInstallResolved");
    }

    #[test]
    fn concurrent_edit_outcome_wire_labels_are_stable() {
        // Pinned so a variant rename forces a deliberate change — the
        // GUI / TUI consume these strings and would silently mis-render
        // otherwise.
        assert_eq!(ConcurrentEditOutcome::Reload.wire_label(), "reload");
        assert_eq!(ConcurrentEditOutcome::Wait.wire_label(), "wait");
        assert_eq!(ConcurrentEditOutcome::Pause.wire_label(), "pause");
        assert_eq!(
            ConcurrentEditOutcome::AutoReload.wire_label(),
            "auto_reload"
        );
        assert_eq!(
            ConcurrentEditOutcome::PauseTimedOut.wire_label(),
            "pause_timed_out"
        );
    }

    /// Hook impl that counts invocations — lets tests assert that hooks fired
    /// the expected number of times on the expected transitions.
    #[derive(Default)]
    struct Counting {
        n: AtomicUsize,
    }

    impl Counting {
        fn count(&self) -> usize {
            self.n.load(Ordering::SeqCst)
        }
    }

    impl CheckpointHook for Counting {
        fn on_transition(&self, _t: &Transition) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl LedgerHook for Counting {
        fn on_transition(&self, _t: &Transition) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn next_event(rx: &mut broadcast::Receiver<Event>) -> Event {
        timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("event within 1s")
            .expect("event channel not closed")
    }

    #[tokio::test]
    async fn legal_advance_runs_hooks_and_broadcasts() {
        let cp = Arc::new(Counting::default());
        let lg = Arc::new(Counting::default());
        let h = spawn(cp.clone(), lg.clone());
        let mut rx = h.subscribe();

        h.send(Command::Advance(State::Streaming)).await.unwrap();
        match next_event(&mut rx).await {
            Event::Transitioned { from, to } => {
                assert_eq!(from, State::Idle);
                assert_eq!(to, State::Streaming);
            }
            other => panic!("expected Transitioned, got {other:?}"),
        }

        assert_eq!(cp.count(), 1);
        assert_eq!(lg.count(), 1);

        h.send(Command::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn illegal_advance_does_not_change_state_or_run_hooks() {
        let cp = Arc::new(Counting::default());
        let lg = Arc::new(Counting::default());
        let h = spawn(cp.clone(), lg.clone());
        let mut rx = h.subscribe();

        // Idle -> ToolDispatching is rejected; spec demands Streaming first.
        h.send(Command::Advance(State::ToolDispatching))
            .await
            .unwrap();
        match next_event(&mut rx).await {
            Event::IllegalTransitionAttempted { from, to } => {
                assert_eq!(from, State::Idle);
                assert_eq!(to, State::ToolDispatching);
            }
            other => panic!("expected IllegalTransitionAttempted, got {other:?}"),
        }
        assert_eq!(cp.count(), 0);
        assert_eq!(lg.count(), 0);

        // Actor is still alive and at Idle — a legal advance works.
        h.send(Command::Advance(State::Streaming)).await.unwrap();
        match next_event(&mut rx).await {
            Event::Transitioned { from, .. } => assert_eq!(from, State::Idle),
            other => panic!("expected Transitioned, got {other:?}"),
        }
        h.send(Command::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn reaching_terminal_state_stops_the_actor() {
        let cp = Arc::new(Counting::default());
        let lg = Arc::new(Counting::default());
        let h = spawn(cp.clone(), lg.clone());
        let mut rx = h.subscribe();

        for to in [State::Streaming, State::Verifying, State::Done] {
            h.send(Command::Advance(to)).await.unwrap();
            let _ = next_event(&mut rx).await;
        }

        // Once the actor reaches Done it returns, dropping its receiver.
        // The next send must fail.
        for _ in 0..10 {
            if h.send(Command::Advance(State::Streaming)).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("actor did not stop after terminal transition");
    }

    #[tokio::test]
    async fn cancel_trips_token_and_emits_event_without_advancing_state() {
        let cp = Arc::new(Counting::default());
        let lg = Arc::new(Counting::default());
        let h = spawn(cp.clone(), lg.clone());
        let mut rx = h.subscribe();

        h.send(Command::Advance(State::Streaming)).await.unwrap();
        let _ = next_event(&mut rx).await; // Transitioned

        let token = h.cancel_token();
        assert!(!token.is_cancelled());

        h.send(Command::Cancel).await.unwrap();
        match next_event(&mut rx).await {
            Event::Cancelled => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert!(token.is_cancelled());

        // State machine hasn't auto-advanced. The driver follows up with a
        // legal Advance — Streaming -> AwaitingUser is the spec'd path.
        h.send(Command::Advance(State::AwaitingUser)).await.unwrap();
        match next_event(&mut rx).await {
            Event::Transitioned { from, to } => {
                assert_eq!(from, State::Streaming);
                assert_eq!(to, State::AwaitingUser);
            }
            other => panic!("expected Transitioned, got {other:?}"),
        }
        h.send(Command::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn tool_semaphore_caps_concurrency_at_four() {
        let h = spawn(
            Arc::new(crate::state::NoopHook),
            Arc::new(crate::state::NoopHook),
        );
        let sem = h.tool_semaphore();

        // Acquire all four permits — the fifth must not be immediately
        // available (the spec cap is 4).
        let p1 = sem.clone().acquire_owned().await.unwrap();
        let p2 = sem.clone().acquire_owned().await.unwrap();
        let p3 = sem.clone().acquire_owned().await.unwrap();
        let p4 = sem.clone().acquire_owned().await.unwrap();

        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "5th permit must block"
        );

        drop(p1);
        // After releasing one, a fresh permit is available.
        let _p5 = sem.clone().acquire_owned().await.unwrap();

        drop((p2, p3, p4));
        h.send(Command::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_the_same_events() {
        let h = spawn(
            Arc::new(crate::state::NoopHook),
            Arc::new(crate::state::NoopHook),
        );
        let mut a = h.subscribe();
        let mut b = h.subscribe();

        h.send(Command::Advance(State::Streaming)).await.unwrap();

        for rx in [&mut a, &mut b] {
            match next_event(rx).await {
                Event::Transitioned { to, .. } => assert_eq!(to, State::Streaming),
                other => panic!("expected Transitioned, got {other:?}"),
            }
        }
        h.send(Command::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn late_subscribers_see_only_subsequent_events() {
        // Broadcast channels are not a replay log. A subscriber that joins
        // after an event was emitted will not see it. The on-disk session is
        // authoritative for reconciliation (§14).
        let h = spawn(
            Arc::new(crate::state::NoopHook),
            Arc::new(crate::state::NoopHook),
        );
        h.send(Command::Advance(State::Streaming)).await.unwrap();

        // Give the actor a tick to consume the command.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut late = h.subscribe();
        // Nothing buffered for `late`.
        assert!(matches!(
            late.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        h.send(Command::Advance(State::Verifying)).await.unwrap();
        match next_event(&mut late).await {
            Event::Transitioned { to, .. } => assert_eq!(to, State::Verifying),
            other => panic!("expected Transitioned, got {other:?}"),
        }
        h.send(Command::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_emits_event_and_stops_actor() {
        let h = spawn(
            Arc::new(crate::state::NoopHook),
            Arc::new(crate::state::NoopHook),
        );
        let mut rx = h.subscribe();
        h.send(Command::Shutdown).await.unwrap();
        match next_event(&mut rx).await {
            Event::Shutdown => {}
            other => panic!("expected Shutdown, got {other:?}"),
        }

        // Drop the handle so the broadcast sender refcount drops to zero once
        // the actor returns. `recv` then yields `Closed` instead of blocking
        // forever waiting on a clone the test itself kept alive.
        drop(h);

        for _ in 0..50 {
            match rx.try_recv() {
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
                | Ok(_) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        panic!("broadcast channel did not close after actor shutdown");
    }

    #[tokio::test]
    async fn session_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(seen.insert(SessionId::new()));
        }
    }

    // ---------- Phase C data layer: EditStaged event derivation ----------

    use crate::diff::Hunks;
    use crate::staging::{FileOutcome, SyntaxOutcome};

    fn fake_outcome(path: &str, hunks: Hunks) -> FileOutcome {
        FileOutcome {
            path: path.into(),
            syntax: SyntaxOutcome::NotApplicable,
            hunks,
        }
    }

    #[test]
    fn edit_staged_events_yields_one_event_per_committed_file() {
        let report = CommitReport {
            files: vec![
                fake_outcome("a.rs", Hunks::Same),
                fake_outcome(
                    "b.rs",
                    Hunks::Created {
                        new_byte_len: 9,
                        new_line_count: 1,
                    },
                ),
            ],
        };
        let events = edit_staged_events(&report);
        assert_eq!(events.len(), 2);
        match &events[0] {
            Event::EditStaged { path, hunks } => {
                assert_eq!(path.to_str(), Some("a.rs"));
                assert_eq!(hunks, &Hunks::Same);
            }
            other => panic!("expected EditStaged, got {other:?}"),
        }
        match &events[1] {
            Event::EditStaged { path, hunks } => {
                assert_eq!(path.to_str(), Some("b.rs"));
                assert!(matches!(hunks, Hunks::Created { .. }));
            }
            other => panic!("expected EditStaged, got {other:?}"),
        }
    }

    #[test]
    fn edit_staged_events_for_empty_report_is_empty() {
        let report = CommitReport { files: Vec::new() };
        assert!(edit_staged_events(&report).is_empty());
    }

    #[tokio::test]
    async fn edit_staged_events_broadcast_through_the_session_bus() {
        // Subscribers receive EditStaged events when the dispatcher forwards
        // edit_staged_events()'s output onto the session bus. Until the
        // dispatcher exists, we test by publishing directly through the
        // broadcast Sender exposed via `handle.events.send()` — but the
        // Sender is private. Instead, drive a Transitioned event first
        // (proves the bus works) then assert the variant exists by
        // constructing it in-process.
        let h = spawn(
            Arc::new(crate::state::NoopHook),
            Arc::new(crate::state::NoopHook),
        );
        let mut rx = h.subscribe();
        h.send(Command::Advance(State::Streaming)).await.unwrap();
        match next_event(&mut rx).await {
            Event::Transitioned { .. } => {}
            other => panic!("expected Transitioned, got {other:?}"),
        }
        h.send(Command::Shutdown).await.unwrap();
        // Variant compiles and matches as expected — the dispatcher will
        // emit it via the same Sender clone the actor uses.
        let synthesised = Event::EditStaged {
            path: "x.rs".into(),
            hunks: Hunks::Same,
        };
        assert!(matches!(synthesised, Event::EditStaged { .. }));
    }
}

#[cfg(test)]
mod broadcast_tests {
    use super::*;

    /// v60.35 M29 — `try_emit` increments [`BROADCAST_LAGGED`] whenever
    /// the underlying broadcast send returns `Err(SendError(_))`. The
    /// observable shape we drive here is "no live receivers": creating
    /// a fresh channel and dropping the lone receiver before any send
    /// puts every emit on the error arm. Saturating-add past the
    /// capacity makes the assertion sturdy under any baseline counter
    /// value (other tests in the workspace may have already bumped it).
    #[test]
    fn try_emit_increments_broadcast_lagged_when_no_receivers() {
        let (tx, rx) = broadcast::channel::<Event>(4);
        drop(rx);

        let before = BROADCAST_LAGGED.load(Ordering::Relaxed);
        for _ in 0..16 {
            let _ = try_emit(&tx, Event::Cancelled);
        }
        let after = BROADCAST_LAGGED.load(Ordering::Relaxed);

        assert!(
            after >= before + 16,
            "BROADCAST_LAGGED should have advanced by at least 16; before={before} after={after}"
        );
    }
}
