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
use runner::{EventSink, MockResponse, ProviderChoice, Runner};

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

#[tokio::test]
async fn run_loops_until_claimed_done_and_reaches_terminal_state() {
    let workspace = tempfile::TempDir::new().unwrap();

    let responses = vec![MockResponse {
        assistant_text: "ack".into(),
        envelope: envelope_done(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        claims_done: true,
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
            envelope: Envelope::default(),
            tool_calls: vec![write_call],
            claims_done: false,
        },
        MockResponse {
            assistant_text: "done".into(),
            envelope: envelope_done(),
            tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
            claims_done: true,
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
            envelope: Envelope::default(),
            tool_calls: vec![],
            claims_done: false,
        },
        MockResponse {
            assistant_text: "..".into(),
            envelope: Envelope::default(),
            tool_calls: vec![],
            claims_done: false,
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
            envelope: Envelope::default(),
            tool_calls: vec![ToolCallRequest {
                id: format!("tc-{n}"),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": format!("file_{n}.txt"),
                    "content": format!("new contents {n}"),
                }),
            }],
            claims_done: false,
        });
    }
    responses.push(MockResponse {
        assistant_text: "all renamed".into(),
        envelope: envelope_done(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        claims_done: true,
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
        envelope: envelope_done(),
        tool_calls: vec![mock_envelope_tool_call(&envelope_done())],
        claims_done: true,
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
