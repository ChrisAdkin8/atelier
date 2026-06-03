//! Per-turn agent loop state — mutable state, read-only context, and the
//! `TurnControl` signal that drives the outer loop.
//!
//! Kept out of `runner.rs` so the turn loop can be expressed as a clean
//! `run_turn` method rather than one 1400-line function. Mirrors the
//! `runner/concurrent_edit.rs` pattern.

use atelier_core::{adapter::Message, dispatcher::SessionDispatcher};
use atelier_core::{
    adapter::{TokenCount as AdapterTokenCount, ToolSpec},
    context::{ContextItemSummary, ContextManager},
    memory::{MemoryCardSummary, MemoryStore},
    plan::{PlanCanvas, PlanStep},
    protocol::Envelope,
    protocol_conformance::ConformanceRingBuffer,
    protocol_strategy::Strategy,
    sandbox::SandboxPolicy,
    session::{Event, SessionId},
    verify::ObservedChange,
    SessionHandle, State,
};

/// Mutable state carried across turns of the agent loop.
///
/// Every local that `run()` wrote across turns is moved here so the loop body
/// can be extracted into `Runner::run_turn` as a 4-parameter method rather
/// than a 24-parameter monster or a 1400-line function body.
pub(super) struct TurnState {
    pub(super) active_strategy: Strategy,
    pub(super) envelope_conformance: ConformanceRingBuffer,
    pub(super) messages: Vec<Message>,
    pub(super) observed_changes: Vec<ObservedChange>,
    pub(super) last_envelope: Envelope,
    pub(super) last_assistant_text: Option<String>,
    pub(super) token_count_cache: Option<(u64, AdapterTokenCount)>,
    pub(super) last_context_items: Option<Vec<ContextItemSummary>>,
    pub(super) last_context_meter: Option<(u32, u32)>,
    pub(super) last_memory_cards: Option<Vec<MemoryCardSummary>>,
    pub(super) last_plan_steps: Option<Vec<PlanStep>>,
    pub(super) turns: usize,
    pub(super) final_state: State,
    /// Consumed on the first `ModelCall` ledger entry after a `/skill` invocation.
    pub(super) pending_skill_note: Option<String>,
}

/// Read-only references the turn loop needs from the `run()` setup region.
///
/// All fields borrow from locals that outlive the loop — no cloning of Arcs.
/// Field inits are coercion sites, so `&Arc<T>` deref-coerces to `&T`
/// without `&*`.
pub(super) struct TurnContext<'a> {
    pub(super) workspace: &'a std::path::Path,
    pub(super) sandbox: &'a SandboxPolicy,
    pub(super) session_handle: &'a SessionHandle,
    pub(super) bus: &'a tokio::sync::broadcast::Sender<Event>,
    pub(super) session_dispatcher: &'a SessionDispatcher,
    pub(super) context_manager: &'a parking_lot::Mutex<ContextManager>,
    pub(super) memory_store: &'a parking_lot::Mutex<MemoryStore>,
    pub(super) plan_canvas: &'a parking_lot::Mutex<PlanCanvas>,
    pub(super) tools_spec: &'a [ToolSpec],
    pub(super) audit_log_path: &'a std::path::Path,
    pub(super) session_id: SessionId,
}

/// What `Runner::run_turn` tells the outer loop to do next.
pub(super) enum TurnControl {
    Continue,
    Break,
}
