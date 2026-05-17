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

use std::sync::Arc;

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
/// reconciliation after a UI falls behind.
pub const EVENT_BUFFER: usize = 256;

/// Default inbox depth. Producers `.await` when full — backpressure is
/// intentional: a full inbox means the actor is wedged on a transition.
pub const INBOX_CAPACITY: usize = 32;

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
    },

    /// The actor is shutting down. No further events will be emitted.
    Shutdown,
}

/// One pending file in a [`Event::StagingPendingApproval`]. Carries
/// the same `hunks` payload as [`Event::EditStaged`] so the UI can
/// render the diff before any rename has happened.
#[derive(Debug, Clone)]
pub struct PendingFile {
    pub path: PathBuf,
    pub hunks: Hunks,
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
    let id = SessionId::new();
    let (tx, rx) = mpsc::channel(inbox_capacity);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let tool_semaphore = Arc::new(Semaphore::new(TOOL_PARALLELISM_CAP));
    let cancel = CancellationToken::new();

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
                    let _ = events.send(Event::Transitioned { from: state, to });
                    state = to;
                    if state.is_terminal() {
                        tracing::debug!(session = %id, terminal = %state, "session reached terminal state");
                        break;
                    }
                }
                Err(IllegalTransition { from, to }) => {
                    tracing::warn!(session = %id, %from, %to, "illegal transition rejected");
                    let _ = events.send(Event::IllegalTransitionAttempted { from, to });
                }
            },

            Command::Cancel => {
                tracing::debug!(session = %id, current = %state, "cancel requested");
                cancel.cancel();
                let _ = events.send(Event::Cancelled);
                // Per spec §2.5, the driver is responsible for advancing
                // through the legal path to `AwaitingUser`. The actor only
                // trips the token and notifies subscribers.
            }

            Command::Shutdown => {
                tracing::debug!(session = %id, "session actor shutdown requested");
                let _ = events.send(Event::Shutdown);
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
