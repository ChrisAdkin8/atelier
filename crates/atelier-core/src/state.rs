//! §2.5 Agent-loop state machine — synchronous skeleton.
//!
//! Defines the named states and the legal transition table from spec §2.5
//! ("State machine"). The async actor (per-session `tokio` task, `mpsc` inbox,
//! broadcast event channel) lands in the BYOM adapter session; this module is
//! the pure-state contract everything else hangs off.
//!
//! Every transition is supposed to write a §4 checkpoint and a §1 ledger
//! entry. The `CheckpointHook` and `LedgerHook` traits below are the contract
//! for those side-effects. Concrete impls land in later sessions; for now any
//! consumer can pass `&NoopHook` to satisfy them.

use std::fmt;

/// Named states from spec §2.5. Order matches the spec listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Idle,
    Streaming,
    ToolDispatching,
    ToolExecuting,
    Verifying,
    AwaitingUser,
    Failed,
    Done,
}

impl State {
    /// Short stable name used in checkpoints, ledger entries, and log lines.
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Streaming => "Streaming",
            Self::ToolDispatching => "ToolDispatching",
            Self::ToolExecuting => "ToolExecuting",
            Self::Verifying => "Verifying",
            Self::AwaitingUser => "AwaitingUser",
            Self::Failed => "Failed",
            Self::Done => "Done",
        }
    }

    /// Terminal states from which no further transition is legal.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Done)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Directed edge between two states. Constructed only via [`Transition::new`],
/// which rejects edges not present in [`LEGAL_TRANSITIONS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Transition {
    pub from: State,
    pub to: State,
}

/// Returned when a caller asks for a transition not in the spec table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("illegal §2.5 transition: {from} -> {to}")]
pub struct IllegalTransition {
    pub from: State,
    pub to: State,
}

impl Transition {
    /// Validate and construct. Rejects unknown edges so the state machine can
    /// never be advanced by a caller who hasn't read the spec.
    pub fn new(from: State, to: State) -> Result<Self, IllegalTransition> {
        if is_legal(from, to) {
            Ok(Self { from, to })
        } else {
            Err(IllegalTransition { from, to })
        }
    }
}

/// The §2.5 transition table, materialised as a const slice so UIs (TUI/GUI),
/// docs generators, and tests can iterate it without re-encoding the rules.
///
/// Reflects the spec diagram:
/// ```text
/// Idle -> Streaming -> ToolDispatching -> ToolExecuting -> Streaming (loop)
///                                                       -> Verifying -> Done | Streaming (retry) | Failed
///                   -> AwaitingUser -> Streaming (resume)
/// ```
/// plus the §2.5 "Tool error model" routings (PermissionDenied -> AwaitingUser,
/// SandboxViolation -> Failed, all other tool errors -> Streaming).
pub const LEGAL_TRANSITIONS: &[(State, State)] = &[
    (State::Idle, State::Streaming),
    (State::Streaming, State::ToolDispatching),
    (State::Streaming, State::Verifying),
    (State::Streaming, State::AwaitingUser),
    (State::Streaming, State::Failed),
    (State::ToolDispatching, State::ToolExecuting),
    (State::ToolDispatching, State::Failed),
    (State::ToolExecuting, State::Streaming),
    (State::ToolExecuting, State::AwaitingUser),
    (State::ToolExecuting, State::Failed),
    (State::Verifying, State::Done),
    (State::Verifying, State::Streaming),
    (State::Verifying, State::Failed),
    (State::AwaitingUser, State::Streaming),
    (State::AwaitingUser, State::Failed),
];

fn is_legal(from: State, to: State) -> bool {
    LEGAL_TRANSITIONS.iter().any(|&(f, t)| f == from && t == to)
}

/// Side-effect hook fired on every transition. §4 checkpoint storage lives
/// here. Impls are inserted by the agent-loop actor (later session); tests and
/// scaffolding can use [`NoopHook`].
pub trait CheckpointHook: Send + Sync {
    fn on_transition(&self, t: &Transition);
}

/// Side-effect hook fired on every transition. §1 cost-ledger entries (token
/// counts, tool-call costs, cache-bust events) land here.
pub trait LedgerHook: Send + Sync {
    fn on_transition(&self, t: &Transition);
}

/// Default hook impl for scaffolding and tests — discards every event.
pub struct NoopHook;

impl CheckpointHook for NoopHook {
    fn on_transition(&self, _t: &Transition) {}
}

impl LedgerHook for NoopHook {
    fn on_transition(&self, _t: &Transition) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_transitions_round_trip_through_constructor() {
        for &(from, to) in LEGAL_TRANSITIONS {
            let t = Transition::new(from, to).expect("table entry must validate");
            assert_eq!(t.from, from);
            assert_eq!(t.to, to);
        }
    }

    #[test]
    fn obvious_illegal_pairs_are_rejected() {
        // Terminal states can never advance.
        assert!(Transition::new(State::Done, State::Streaming).is_err());
        assert!(Transition::new(State::Failed, State::Streaming).is_err());
        // Tools cannot dispatch from Idle without streaming first.
        assert!(Transition::new(State::Idle, State::ToolDispatching).is_err());
        // Verifying cannot jump straight to ToolDispatching — must re-enter Streaming first.
        assert!(Transition::new(State::Verifying, State::ToolDispatching).is_err());
        // AwaitingUser cannot resume directly into Verifying.
        assert!(Transition::new(State::AwaitingUser, State::Verifying).is_err());
    }

    #[test]
    fn illegal_transition_error_carries_the_offending_pair() {
        let err = Transition::new(State::Done, State::Idle).unwrap_err();
        assert_eq!(err.from, State::Done);
        assert_eq!(err.to, State::Idle);
        // Display includes both states so log lines are self-describing.
        let msg = format!("{err}");
        assert!(msg.contains("Done"));
        assert!(msg.contains("Idle"));
    }

    #[test]
    fn terminal_states_are_marked_terminal() {
        assert!(State::Done.is_terminal());
        assert!(State::Failed.is_terminal());
        for s in [
            State::Idle,
            State::Streaming,
            State::ToolDispatching,
            State::ToolExecuting,
            State::Verifying,
            State::AwaitingUser,
        ] {
            assert!(!s.is_terminal(), "{s} should not be terminal");
        }
    }

    #[test]
    fn terminal_states_have_no_outbound_legal_edges() {
        for &(from, _to) in LEGAL_TRANSITIONS {
            assert!(
                !from.is_terminal(),
                "no outbound edge should originate at terminal state {from}"
            );
        }
    }

    #[test]
    fn legal_table_has_no_duplicates() {
        let mut seen: Vec<(State, State)> = Vec::new();
        for &edge in LEGAL_TRANSITIONS {
            assert!(
                !seen.contains(&edge),
                "duplicate edge {:?} in LEGAL_TRANSITIONS",
                edge
            );
            seen.push(edge);
        }
    }

    #[test]
    fn noop_hooks_satisfy_the_trait_objects() {
        let hook = NoopHook;
        let t = Transition::new(State::Idle, State::Streaming).unwrap();
        // Both as trait objects — exercises object safety.
        let cp: &dyn CheckpointHook = &hook;
        let lg: &dyn LedgerHook = &hook;
        cp.on_transition(&t);
        lg.on_transition(&t);
    }
}
