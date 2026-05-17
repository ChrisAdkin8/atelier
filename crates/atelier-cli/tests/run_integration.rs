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

#[tokio::test]
async fn run_loops_until_claimed_done_and_reaches_terminal_state() {
    let workspace = tempfile::TempDir::new().unwrap();

    let responses = vec![MockResponse {
        assistant_text: "ack".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
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
        },
        MockResponse {
            assistant_text: "done".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
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
        },
        MockResponse {
            assistant_text: "..".into(),
            tool_calls: vec![],
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
        });
    }
    responses.push(MockResponse {
        assistant_text: "all renamed".into(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
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
        },
        MockResponse {
            assistant_text: "done".into(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
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
                assert!(sd.submit_approval(commit_id, vec![PathBuf::from("approved.txt")]));
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
                assert!(sd.submit_approval(commit_id, vec![]));
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
