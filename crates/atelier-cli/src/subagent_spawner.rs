//! `RunnerSpawner` — the `atelier-cli`-side implementation of
//! [`atelier_core::subagents::SubagentSpawner`].
//!
//! When `spawn_subagent` (in `atelier-core`) wants to materialise a child
//! §2.5 state machine, it calls through the [`SubagentSpawner`] trait seam.
//! `RunnerSpawner` fulfils the contract by constructing a minimal child
//! `Runner` via the existing builder API with:
//!
//!   - A child `CancellationToken` derived from the parent's token so
//!     cancellation cascades automatically.
//!   - The parent's `Arc<dyn Adapter>` (sub-agents share the model).
//!   - The parent's resolved `ModelProfile` (inherited via `set_profile` so
//!     child runners skip the probe and use the same emission strategy).
//!   - An `EventSink::Callback` that forwards `SubagentTurnAdvanced` and
//!     `SubagentToolCall` events to the parent bus in real time.
//!   - The sub-agent type's `system_prompt_addendum` injected via a
//!     synthetic first system turn in the prompt.
//!   - `max_turns` capped per the effective-max-turns of the spawn request.
//!   - `tool_allowlist` enforcement (via the dispatcher's allow-list filter
//!     once WU-4 allowlist filtering lands; today we carry it through
//!     `SpawnRequest` for correctness).
//!
//! The spawner maintains a registry of in-flight runs keyed by `SubagentId`.
//! `cancel` immediately trips the child token; `wait_all` awaits every
//! child's `JoinHandle`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use atelier_core::adapter::model_profile::ModelProfile;
use atelier_core::session::{try_emit, Event as SessionEvent};
use atelier_core::state::State as ActorState;
use atelier_core::subagents::{
    CancelError, SpawnError, SpawnRequest, SubagentCost, SubagentId, SubagentResult,
    SubagentSpawner, SubagentStatus, SubagentTypeRegistry,
};
use atelier_core::time::now_rfc3339;
use atelier_core::Adapter;

use crate::runner::{EventSink, ProviderChoice, RunReport, Runner};

// ---------- in-flight entry ----------

struct InFlight {
    cancel: CancellationToken,
    /// Abort handle for the spawned task. Kept separate from the JoinHandle
    /// so the JoinHandle can live locally in `spawn()` and be awaited there
    /// while this entry remains in the map — making `cancel()` functional
    /// for the entire duration of the child's run.
    abort: tokio::task::AbortHandle,
}

// ---------- RunnerSpawner ----------

/// Metadata retained alongside the SubagentResult so the parent session
/// can write a richer [`atelier_core::persistence::PersistedSubagent`].
pub struct CompletedSubagentRecord {
    pub description: String,
    pub subagent_type_name: String,
    pub started_at: String,
    pub finished_at: String,
    pub max_turns: u32,
    pub result: SubagentResult,
}

/// Production impl of [`SubagentSpawner`] backed by the existing `Runner`.
pub struct RunnerSpawner {
    /// Shared adapter — child runners clone the Arc.
    adapter: Arc<dyn Adapter>,
    /// Workspace root forwarded to every child runner.
    workspace: std::path::PathBuf,
    /// Sub-agent type registry (for future proactive-trigger hooks).
    _type_registry: Arc<SubagentTypeRegistry>,
    /// In-flight runs keyed by [`SubagentId`].
    in_flight: Mutex<HashMap<SubagentId, InFlight>>,
    /// Completed sub-agent records keyed by sub-agent ID string. Drained by
    /// the parent runner after each `run()` to persist into `session.json`.
    completed: Mutex<Vec<CompletedSubagentRecord>>,
    /// Parent event bus. Set after the session is spawned via `set_bus`.
    /// Used to emit SubagentSpawned / SubagentCompleted on the parent's bus
    /// so GUIs and TUIs can update their sub-agent panels in real time.
    bus_slot: Mutex<Option<tokio::sync::broadcast::Sender<SessionEvent>>>,
    /// Resolved model profile from the parent's probe. Set by `Runner::run`
    /// immediately after the probe completes so child runners can inherit
    /// the parent's observed emission strategy without re-probing.
    profile_slot: Mutex<Option<ModelProfile>>,
}

impl RunnerSpawner {
    pub fn new(
        adapter: Arc<dyn Adapter>,
        workspace: std::path::PathBuf,
        type_registry: Arc<SubagentTypeRegistry>,
    ) -> Self {
        Self {
            adapter,
            workspace,
            _type_registry: type_registry,
            in_flight: Mutex::new(HashMap::new()),
            completed: Mutex::new(Vec::new()),
            bus_slot: Mutex::new(None),
            profile_slot: Mutex::new(None),
        }
    }

    /// Wire the parent session's broadcast sender so lifecycle events
    /// (SubagentSpawned, SubagentCompleted, SubagentCancelled) are visible
    /// on the parent's bus. Called by `Runner::run` immediately after the
    /// session actor is spawned.
    pub fn set_bus(&self, bus: tokio::sync::broadcast::Sender<SessionEvent>) {
        *self.bus_slot.lock() = Some(bus);
    }

    /// Store the parent's resolved `ModelProfile` so child runners can
    /// inherit it. Called by `Runner::run` right after the probe completes.
    pub fn set_profile(&self, profile: ModelProfile) {
        *self.profile_slot.lock() = Some(profile);
    }

    fn emit_on_parent(&self, ev: SessionEvent) {
        if let Some(bus) = self.bus_slot.lock().as_ref() {
            let _ = try_emit(bus, ev);
        }
    }
}

#[async_trait]
impl SubagentSpawner for RunnerSpawner {
    async fn spawn(&self, req: SpawnRequest) -> Result<SubagentResult, SpawnError> {
        use atelier_core::subagents::RECURSION_DEPTH_CAP;
        if req.parent_depth >= RECURSION_DEPTH_CAP {
            return Err(SpawnError::DepthCapExceeded {
                cap: RECURSION_DEPTH_CAP,
            });
        }

        let child_cancel = req.parent_cancel.child_token();
        let subagent_id = req.id.clone();
        let depth = req.parent_depth + 1;

        // Capture metadata before req is partially moved.
        let description = req.description.clone();
        let type_name = req.subagent_type.name.clone();
        let started_at = now_rfc3339();

        // Build the prompt injecting the system_prompt_addendum first.
        let addendum = req.subagent_type.system_prompt_addendum.clone();
        let user_prompt = req.prompt.clone();
        let max_turns = req.effective_max_turns() as usize;

        // Build an EventSink for the child runner that forwards turn-progress
        // events (SubagentTurnAdvanced, SubagentToolCall) to the parent bus
        // so GUIs/TUIs can show what the sub-agent is doing in real time.
        let child_sink = {
            let id_str = subagent_id.0.to_string();
            let mt = max_turns as u32;
            match self.bus_slot.lock().clone() {
                Some(parent_bus) => {
                    let turn_ctr = Arc::new(std::sync::atomic::AtomicU32::new(0));
                    EventSink::Callback(Arc::new(move |ev: &SessionEvent| match ev {
                        SessionEvent::Transitioned { to, .. } if *to == ActorState::Streaming => {
                            let t = turn_ctr.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            let _ = try_emit(
                                &parent_bus,
                                SessionEvent::SubagentTurnAdvanced {
                                    id: id_str.clone(),
                                    turn: t,
                                    max_turns: mt,
                                },
                            );
                        }
                        SessionEvent::Transitioned { to, .. }
                            if *to == ActorState::ToolDispatching =>
                        {
                            let _ = try_emit(
                                &parent_bus,
                                SessionEvent::SubagentToolCall {
                                    id: id_str.clone(),
                                    tool: String::from("dispatching"),
                                },
                            );
                        }
                        _ => {}
                    }))
                }
                None => EventSink::Null,
            }
        };

        // Build the child runner. We use `with_adapter` (not the provider-
        // choice path) because the parent has already selected and
        // initialised the adapter — the sub-agent shares it. We also
        // inherit the parent's resolved ModelProfile so the child starts
        // with the correct emission strategy without an extra probe call.
        let pinned_profile = self.profile_slot.lock().clone();
        let child_runner = {
            let dummy = Runner::new(
                self.workspace.clone(),
                ProviderChoice::Mock { responses: vec![] },
                child_sink,
            )
            .map_err(|e| SpawnError::Internal(e.to_string()))?;

            let r = dummy
                .with_adapter(self.adapter.clone())
                .with_probe_policy(crate::runner::ProbePolicy::Skip)
                .with_max_turns(max_turns)
                .with_external_cancel(child_cancel.clone())
                .with_subagent_depth(depth);

            if let Some(profile) = pinned_profile {
                r.with_model_profile(profile)
            } else {
                r
            }
        };

        // Prepend the system_prompt_addendum as a prefix to the user prompt
        // so the sub-agent's first context message carries the type's
        // instructions. A future revision can inject this as a dedicated
        // System role turn instead.
        let full_prompt = if addendum.is_empty() {
            user_prompt
        } else {
            format!("[System instruction]\n{addendum}\n\n{user_prompt}")
        };

        // Emit on the parent bus so GUIs/TUIs update their sub-agent panel.
        self.emit_on_parent(SessionEvent::SubagentSpawned {
            id: subagent_id.0.to_string(),
            parent_id: String::new(), // root runner has no persisted parent id
            subagent_type: type_name.clone(),
            description: description.clone(),
            max_turns: max_turns as u32,
        });

        let id = subagent_id.clone();
        let handle: JoinHandle<SubagentResult> = tokio::spawn(async move {
            match child_runner.run(full_prompt).await {
                Ok(report) => run_report_to_subagent_result(&id, report, depth),
                Err(e) => SubagentResult {
                    id: id.clone(),
                    result: format!("sub-agent failed: {e}"),
                    status: SubagentStatus::Failed,
                    turns_used: 0,
                    cost: SubagentCost::default(),
                },
            }
        });

        // Register in-flight BEFORE awaiting the handle. The entry stays in
        // the map for the entire duration of the child's run so `cancel()` can
        // find and trip it at any point. (Previously the entry was removed
        // immediately after insertion, making `cancel()` always return
        // NotFound during the await.)
        self.in_flight.lock().insert(
            subagent_id.clone(),
            InFlight {
                cancel: child_cancel,
                abort: handle.abort_handle(),
            },
        );

        // Await completion inline (spec: tool returns only once sub-agent done).
        let join_result = handle.await;

        // Remove entry now that the task is finished (or was already removed by
        // a concurrent cancel(), in which case remove() is a no-op).
        self.in_flight.lock().remove(&subagent_id);

        let result = match join_result {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => SubagentResult {
                id: subagent_id,
                result: String::new(),
                status: SubagentStatus::Cancelled,
                turns_used: 0,
                cost: SubagentCost::default(),
            },
            Err(e) => return Err(SpawnError::Internal(e.to_string())),
        };

        let finished_at = now_rfc3339();
        self.emit_on_parent(SessionEvent::SubagentCompleted {
            id: result.id.0.to_string(),
            status: result.status.clone(),
            turns_used: result.turns_used,
        });
        // Record for the parent runner to persist in session.json.
        self.completed.lock().push(CompletedSubagentRecord {
            description,
            subagent_type_name: type_name,
            started_at,
            finished_at,
            max_turns: max_turns as u32,
            result: result.clone(),
        });

        Ok(result)
    }

    async fn cancel(&self, id: &SubagentId) -> Result<(), CancelError> {
        let entry = self.in_flight.lock().remove(id);
        match entry {
            Some(entry) => {
                entry.cancel.cancel();
                entry.abort.abort();
                self.emit_on_parent(SessionEvent::SubagentCancelled {
                    id: id.0.to_string(),
                    reason: "cancelled by parent".to_string(),
                });
                Ok(())
            }
            None => Err(CancelError::NotFound(id.clone())),
        }
    }

    async fn wait_all(&self, _parent_id: &SubagentId) {
        // Spec correctness: drain any in-flight sub-agents not yet awaited.
        // In the current inline-await flow (spawn() awaits the child handle
        // before returning to the caller and then removes its entry), the map
        // is already empty by the time the parent runner calls this before its
        // §7 gate. Drain and abort any entries that remain — this is a safety
        // net for future fire-and-forget paths; the JoinHandles are local to
        // each spawn() call so we can only abort here.
        let entries: Vec<InFlight> = self.in_flight.lock().drain().map(|(_, v)| v).collect();
        for entry in entries {
            entry.abort.abort();
        }
    }
}

impl RunnerSpawner {
    /// Drain and return all completed sub-agent records since the last drain.
    /// Called by the parent runner after `run()` to persist into session.json.
    pub fn drain_completed(&self) -> Vec<CompletedSubagentRecord> {
        std::mem::take(&mut *self.completed.lock())
    }
}

fn run_report_to_subagent_result(id: &SubagentId, report: RunReport, _depth: u8) -> SubagentResult {
    // Extract the final assistant message from the report's conversation.
    // RunReport doesn't directly carry the last assistant text today —
    // we use the run status as a proxy and leave result empty until the
    // RunReport grows a `final_assistant_message` field.
    use atelier_core::state::State;
    let status = match report.final_state {
        State::Done => SubagentStatus::Completed,
        State::AwaitingUser => SubagentStatus::TimedOut,
        _ => SubagentStatus::Failed,
    };

    // Accumulate cost from the ledger entries.
    let cost = summarise_cost(&report);

    SubagentResult {
        id: id.clone(),
        result: report
            .final_assistant_text
            .unwrap_or_else(|| format!("sub-agent produced no output (status: {status:?})")),
        status,
        turns_used: report.turns_used as u32,
        cost,
    }
}

fn summarise_cost(report: &RunReport) -> SubagentCost {
    use atelier_core::ledger::LedgerEntry;
    let mut cost = SubagentCost::default();
    for entry in &report.ledger_entries {
        if let LedgerEntry::ModelCall {
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cost_usd,
            ..
        } = entry
        {
            cost.prompt_tokens += prompt_tokens;
            cost.completion_tokens += completion_tokens;
            if let Some(ct) = cached_tokens {
                cost.cached_tokens += ct;
            }
            if let Some(usd) = cost_usd {
                *cost.cost_usd.get_or_insert(0.0) += usd;
            }
        }
    }
    cost
}
