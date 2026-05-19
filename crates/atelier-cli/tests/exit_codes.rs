//! v60.32 M02 — pin the binary's exit-code contract.
//!
//! A run that ends in `AwaitingUser` (agent stalled, no `claimed_done`,
//! no tool call) must exit 6 so CI gates can distinguish "completed"
//! from "stalled". 0 is reserved for `Done`; 130/143 belong to the
//! v60.29 signal handlers.
//!
//! The `atelier` binary's Mock provider takes no scripted responses
//! over the CLI (empty `MockResponse` vec is the only shape via argv),
//! so we drive the runner directly and assert two pieces:
//!
//!   1. A Mock that produces an assistant turn with neither
//!      `claimed_done` nor a tool call lands in `State::AwaitingUser`
//!      after the loop's stall guard fires.
//!   2. The library helper `exit_code_for_final_state` (which the
//!      `atelier` binary's `run_run` path calls) maps `AwaitingUser`
//!      to 6 and every other terminal state to 0.

use std::sync::Arc;

use atelier_cli::{exit_code_for_final_state, EventSink, MockResponse, ProviderChoice, Runner};
use atelier_core::session::Event;
use atelier_core::State;

#[tokio::test]
async fn assistant_text_without_claimed_done_lands_in_awaiting_user() {
    let workspace = tempfile::TempDir::new().unwrap();
    let responses = vec![MockResponse::new(
        "I'd like to ask the user for guidance before continuing.",
        Vec::new(),
    )];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(2);
    let report = runner.run("kick off the task".into()).await.unwrap();
    assert_eq!(report.final_state, State::AwaitingUser);
    let captured = events.lock();
    assert!(
        captured
            .iter()
            .any(|e| matches!(e, Event::AgentStalled { .. })),
        "expected an AgentStalled event before AwaitingUser"
    );
    // The library helper the binary calls maps this stall to 6.
    assert_eq!(exit_code_for_final_state(report.final_state), 6);
}

#[test]
fn exit_code_helper_pins_terminal_state_mapping() {
    assert_eq!(exit_code_for_final_state(State::AwaitingUser), 6);
    assert_eq!(exit_code_for_final_state(State::Done), 0);
    assert_eq!(exit_code_for_final_state(State::Idle), 0);
    assert_eq!(exit_code_for_final_state(State::Failed), 0);
}
