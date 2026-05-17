//! Tauri shell for Atelier.
//!
//! Spec §3. Boots a Tauri app, spawns an `atelier_core::session::Handle`, and
//! forwards the broadcast event bus onto the webview as `atelier://event`.
//! The first panel (`ui/src/App.svelte`) subscribes and counts `EditStaged`
//! events — the smallest end-to-end demonstration that the broadcast bus
//! reaches the UI.
//!
//! The bridge is **one-way for now** (Rust → webview). Webview → Rust
//! commands (start session, cancel, advance) will land alongside the
//! multi-pane workspace; until then the only exposed command is `ping`, used
//! by the integration test to confirm the IPC wiring round-trips.
//!
//! # Event payload shape
//!
//! [`Event`](atelier_core::session::Event) is `Debug + Clone` but not
//! `Serialize` — adding serde to the core enum would force every variant's
//! constituent types (e.g. `State`) to be `Serialize` too, which we don't
//! want to commit to yet. So we hand-roll a JSON projection here. The
//! frontend matches on `payload.kind`.

use std::sync::Arc;

use atelier_cli::runner::{DispatcherHandle, EventSink, MockResponse, ProviderChoice, Runner};
use atelier_core::adapter::ToolCallRequest;
use atelier_core::dispatcher::ApprovalPolicy;
use atelier_core::protocol::Envelope;
use atelier_core::protocol_strategy::HARNESS_META_NAME;
use atelier_core::session::Event as SessionEvent;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

/// Wrapper Tauri emits to the webview. `kind` is the variant tag; `payload`
/// is the variant's JSON body. The TypeScript side only depends on `kind`
/// — `payload` shape is per-variant and evolves with the spec.
#[derive(Serialize, Clone, Debug)]
pub struct BridgedEvent {
    pub kind: &'static str,
    pub payload: Value,
}

/// State the Tauri runtime manages for the lifetime of the shell.
///
/// v47: the GUI is now a driver, not a viewer. `dispatcher_handle` is
/// populated by `start_demo_run` once the runner builds its
/// `SessionDispatcher`; `submit_approval` reads from it to route
/// accept-sets to the live dispatcher. `workspace_root` is the disk
/// root the demo run writes against — each run gets a fresh UUID
/// subdirectory (v49) so concurrent runs can't see each other's
/// edits.
///
/// `run_in_flight` (v49) is the concurrent-run guard: `start_demo_run`
/// uses compare_exchange to refuse a second invocation while one is
/// still active. Cleared by the spawned task's `Drop`-style cleanup.
pub struct SessionState {
    pub dispatcher_handle: DispatcherHandle,
    pub workspace_root: std::path::PathBuf,
    pub run_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Entry point. Spawned by `main.rs`; lives in `lib.rs` so the integration
/// tests can pull in the same module and exercise the helpers.
pub fn run() {
    tracing_subscriber::fmt::try_init().ok();

    tauri::Builder::default()
        .setup(|app| {
            // v47: ephemeral workspace per process. Real "open project"
            // selection lands when the GUI grows a file-tree pane.
            let workspace_root =
                std::env::temp_dir().join(format!("atelier-gui-{}", std::process::id()));
            std::fs::create_dir_all(&workspace_root)?;

            app.manage(SessionState {
                dispatcher_handle: DispatcherHandle::new(),
                workspace_root,
                run_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping,
            submit_approval,
            start_demo_run
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Trivial round-trip command used by the integration test to confirm the
/// IPC channel is wired. Production commands (start session, cancel,
/// advance) land alongside the multi-pane workspace.
#[tauri::command]
fn ping() -> &'static str {
    "pong"
}

/// Spec §3 hunk accept/reject — frontend bridge. Routed to the
/// live `SessionDispatcher` via the `DispatcherHandle` in
/// `SessionState`. Returns `false` when there's no active run
/// (`start_demo_run` hasn't been called) or when `commit_id` doesn't
/// match an outstanding pending (already approved / dispatcher torn
/// down). `accepted` is the list of file paths (relative to the
/// workspace root) the user OK'd.
#[tauri::command]
fn submit_approval(
    state: tauri::State<'_, SessionState>,
    commit_id: String,
    accepted: Vec<String>,
) -> bool {
    let Ok(parsed_id) = uuid::Uuid::parse_str(&commit_id) else {
        tracing::warn!(commit_id, "submit_approval: malformed commit_id");
        return false;
    };
    let Some(sd) = state.dispatcher_handle.get() else {
        tracing::warn!(
            commit_id,
            "submit_approval: no active dispatcher (start_demo_run not running?)"
        );
        return false;
    };
    let accepted_paths: Vec<std::path::PathBuf> =
        accepted.into_iter().map(std::path::PathBuf::from).collect();
    sd.submit_approval(parsed_id, accepted_paths)
}

/// Start a mock-scripted run with `AwaitApproval` policy. v47 demo
/// driver: the GUI builds a `Runner` that emits a `write_file` tool
/// call against the ephemeral workspace, the dispatcher hits the
/// approval gate, the user clicks accept/reject in the DiffPane, the
/// resulting commit lands in the workspace.
///
/// Returns immediately after spawning the run on the tokio runtime;
/// the webview observes progress via the `atelier://event` stream.
/// Max prompt size accepted by `start_demo_run`. A multi-GB string
/// from a hostile or buggy webview would otherwise be copied into
/// `format!(content)`, `MockResponse`, the bus envelope, and the
/// adapter's message history — easy DoS surface.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

#[tauri::command]
fn start_demo_run(
    app: AppHandle,
    state: tauri::State<'_, SessionState>,
    prompt: String,
) -> Result<(), String> {
    if prompt.len() > MAX_PROMPT_BYTES {
        // `.len()` is bytes (memory cost is what we care about, not
        // character count). In a multi-byte locale a CJK or emoji
        // prompt may report e.g. 21k chars but 64k bytes — the
        // message clarifies this so the user doesn't read "bytes"
        // as "characters."
        return Err(format!(
            "prompt too long: {} bytes (max {} bytes / ~{} ASCII chars)",
            prompt.len(),
            MAX_PROMPT_BYTES,
            MAX_PROMPT_BYTES
        ));
    }

    // v49 concurrent-run guard. compare_exchange (Acquire/Relaxed) so
    // a second invocation while a run is in flight gets a typed error
    // the frontend can surface, rather than silently corrupting the
    // dispatcher slot.
    if state
        .run_in_flight
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::Acquire,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return Err("a run is already in progress — wait for it to finish".to_string());
    }

    // v49 per-run workspace: a fresh UUID-named subdir under the GUI's
    // ephemeral root. Two concurrent demos can't clobber each other's
    // files (the concurrent-run guard above also prevents this today,
    // but the directory isolation is defence in depth and survives a
    // future relaxation of the guard).
    let run_id = uuid::Uuid::new_v4();
    let workspace = state.workspace_root.join(run_id.to_string());
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        state
            .run_in_flight
            .store(false, std::sync::atomic::Ordering::Release);
        return Err(format!("workspace setup failed: {e}"));
    }

    let handle = state.dispatcher_handle.clone();
    let run_in_flight = state.run_in_flight.clone();

    // Build a scripted single-turn run:
    //   1. Assistant emits a write_file tool call + a harness_meta
    //      envelope carrying claimed_done.
    //   2. Dispatcher stages the write, hits AwaitApproval, emits
    //      StagingPendingApproval — the DiffPane renders the banner.
    //   3. The user clicks accept or reject; submit_approval routes
    //      back; the dispatcher commits (or drops) and the run ends.
    //
    // The file name is derived from the prompt's first word so the
    // user sees their input reflected without us having to parse
    // anything more sophisticated.
    let file_name = first_word_or_default(&prompt, "demo.txt");
    let content = format!("written by the GUI demo driver:\n{prompt}\n");
    let write_call = ToolCallRequest {
        id: "tc-demo-write".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": file_name,
            "content": content,
        }),
    };
    let envelope_done = Envelope {
        claimed_done: Some(true),
        ..Default::default()
    };
    let envelope_call = ToolCallRequest {
        id: "tc-demo-envelope".to_string(),
        name: HARNESS_META_NAME.to_string(),
        arguments: serde_json::to_value(&envelope_done).unwrap_or(Value::Null),
    };
    let responses = vec![MockResponse {
        assistant_text: format!("Acknowledging: {prompt}"),
        tool_calls: vec![write_call, envelope_call],
    }];

    // EventSink::Callback forwards every bus event to the webview as
    // `atelier://event`. Same JSON shape `bridge_event` produces in
    // v44, just driven by the runner's own bus instead of a separate
    // session actor.
    let app_clone = app.clone();
    let cb = Arc::new(move |evt: &SessionEvent| {
        emit_event(&app_clone, evt);
    });

    let runner = match Runner::new(
        workspace.clone(),
        ProviderChoice::Mock { responses },
        EventSink::Callback(cb),
    ) {
        Ok(r) => r,
        Err(e) => {
            // Release the guard before bailing — otherwise the next
            // start_demo_run is permanently rejected.
            run_in_flight.store(false, std::sync::atomic::Ordering::Release);
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(format!("Runner::new failed: {e}"));
        }
    };
    let runner = runner
        .with_approval_policy(ApprovalPolicy::AwaitApproval)
        .with_dispatcher_handle(handle)
        .with_max_turns(4);

    // The spawned task owns the per-run workspace + the in-flight
    // flag; both are cleaned up on every exit path via the
    // `RunCleanup` Drop guard below.
    tauri::async_runtime::spawn(async move {
        let _cleanup = RunCleanup {
            in_flight: run_in_flight,
            workspace_to_remove: Some(workspace.clone()),
        };
        if let Err(e) = runner.run(prompt).await {
            tracing::warn!(error = %e, "demo run failed");
        }
    });
    Ok(())
}

/// Drop-guard for `start_demo_run`'s spawned task. Clears the
/// `run_in_flight` flag and (best-effort) removes the per-run
/// workspace on every exit path — including a panic inside
/// `runner.run`. Mirrors the `DispatcherHandleGuard` pattern in
/// `atelier-cli/src/runner.rs`.
struct RunCleanup {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    workspace_to_remove: Option<std::path::PathBuf>,
}

impl Drop for RunCleanup {
    fn drop(&mut self) {
        self.in_flight
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(ws) = self.workspace_to_remove.take() {
            // `remove_dir_all` traverses symlinks on some platforms
            // (older glibc; pre-Rust-1.69 stdlib). If a model managed
            // to plant a symlink in the per-run workspace, this could
            // delete outside files. Two reasons we're OK here:
            //   1. `commit_selected` rejects `..` + absolute paths at
            //      the staging layer (spec §3), so a model can't
            //      write a symlink to outside via the tool path.
            //   2. The per-run workspace is under our own
            //      `temp_dir()/atelier-gui-{pid}/{run_uuid}` and is
            //      only ever written by atelier-core staging.
            // If a future change introduces a tool that writes
            // symlinks, audit this call and add a `symlink_metadata`
            // pre-check or switch to `tokio::fs::remove_dir_all`
            // (which is symlink-safe on every supported platform).
            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}

/// Pick the first whitespace-delimited word from `s`, sanitised down
/// to ASCII alphanumerics + `-`/`_`/`.`. Falls back to `default` when
/// no usable word is present. Used to build the demo file name.
fn first_word_or_default(s: &str, default: &str) -> String {
    let word: String = s
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect();
    if word.is_empty() {
        default.to_string()
    } else if word.contains('.') {
        word
    } else {
        format!("{word}.txt")
    }
}

fn emit_event(app: &AppHandle, evt: &SessionEvent) {
    let bridged = bridge_event(evt);
    if let Err(e) = app.emit("atelier://event", &bridged) {
        tracing::warn!("atelier-gui: emit failed: {e}");
    }
}

/// Project an [`atelier_core::session::Event`] onto the JSON shape the
/// webview consumes. Pure function — exercised by the unit tests below
/// without booting Tauri.
pub fn bridge_event(evt: &SessionEvent) -> BridgedEvent {
    match evt {
        SessionEvent::Transitioned { from, to } => BridgedEvent {
            kind: "Transitioned",
            payload: json!({
                "from": format!("{from:?}"),
                "to": format!("{to:?}"),
            }),
        },
        SessionEvent::IllegalTransitionAttempted { from, to } => BridgedEvent {
            kind: "IllegalTransitionAttempted",
            payload: json!({
                "from": format!("{from:?}"),
                "to": format!("{to:?}"),
            }),
        },
        SessionEvent::Cancelled => BridgedEvent {
            kind: "Cancelled",
            payload: Value::Null,
        },
        SessionEvent::EditStaged { path, hunks } => BridgedEvent {
            kind: "EditStaged",
            payload: json!({
                "path": path.to_string_lossy(),
                "hunks": serde_json::to_value(hunks).unwrap_or(Value::Null),
            }),
        },
        SessionEvent::MessageCommitted { role, text } => BridgedEvent {
            kind: "MessageCommitted",
            payload: json!({
                "role": format!("{role:?}").to_lowercase(),
                "text": text,
            }),
        },
        SessionEvent::PlanSnapshot { steps } => BridgedEvent {
            kind: "PlanSnapshot",
            payload: json!({
                "steps": serde_json::to_value(steps).unwrap_or(Value::Null),
            }),
        },
        SessionEvent::LedgerAppended { entry } => BridgedEvent {
            kind: "LedgerAppended",
            payload: json!({
                "entry": serde_json::to_value(entry).unwrap_or(Value::Null),
            }),
        },
        SessionEvent::ContextSnapshot {
            known_tokens,
            unknown_tokens,
        } => BridgedEvent {
            kind: "ContextSnapshot",
            payload: json!({
                "known_tokens": known_tokens,
                "unknown_tokens": unknown_tokens,
            }),
        },
        SessionEvent::StagingPendingApproval { commit_id, files } => BridgedEvent {
            kind: "StagingPendingApproval",
            payload: json!({
                "commit_id": commit_id.to_string(),
                "files": files
                    .iter()
                    .map(|f| json!({
                        "path": f.path.to_string_lossy(),
                        "hunks": serde_json::to_value(&f.hunks).unwrap_or(Value::Null),
                    }))
                    .collect::<Vec<_>>(),
            }),
        },
        SessionEvent::CommitDecision {
            commit_id,
            committed,
            dropped,
        } => BridgedEvent {
            kind: "CommitDecision",
            payload: json!({
                "commit_id": commit_id.to_string(),
                "committed": committed
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>(),
                "dropped": dropped
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>(),
            }),
        },
        SessionEvent::ModelProfileLoaded {
            model_id,
            base_url,
            strategy,
            outcome,
        } => BridgedEvent {
            kind: "ModelProfileLoaded",
            payload: json!({
                "model_id": model_id,
                "base_url": base_url,
                "strategy": strategy.as_str(),
                "outcome": format!("{outcome:?}").to_lowercase(),
            }),
        },
        SessionEvent::Shutdown => BridgedEvent {
            kind: "Shutdown",
            payload: Value::Null,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelier_core::diff::Hunks;
    use atelier_core::state::State;
    use std::path::PathBuf;

    #[test]
    fn bridge_transitioned_event() {
        let b = bridge_event(&SessionEvent::Transitioned {
            from: State::Idle,
            to: State::Streaming,
        });
        assert_eq!(b.kind, "Transitioned");
        assert_eq!(b.payload["from"], "Idle");
        assert_eq!(b.payload["to"], "Streaming");
    }

    #[test]
    fn bridge_illegal_transition_event() {
        let b = bridge_event(&SessionEvent::IllegalTransitionAttempted {
            from: State::Done,
            to: State::Streaming,
        });
        assert_eq!(b.kind, "IllegalTransitionAttempted");
        assert_eq!(b.payload["from"], "Done");
    }

    #[test]
    fn bridge_cancelled_has_null_payload() {
        let b = bridge_event(&SessionEvent::Cancelled);
        assert_eq!(b.kind, "Cancelled");
        assert!(b.payload.is_null());
    }

    #[test]
    fn bridge_edit_staged_event_carries_path_and_hunks() {
        let b = bridge_event(&SessionEvent::EditStaged {
            path: PathBuf::from("/tmp/foo.rs"),
            hunks: Hunks::Binary,
        });
        assert_eq!(b.kind, "EditStaged");
        assert_eq!(b.payload["path"], "/tmp/foo.rs");
        assert!(b.payload["hunks"].is_object() || b.payload["hunks"].is_string());
    }

    #[test]
    fn bridge_shutdown_event() {
        let b = bridge_event(&SessionEvent::Shutdown);
        assert_eq!(b.kind, "Shutdown");
        assert!(b.payload.is_null());
    }

    #[test]
    fn bridged_event_serializes_to_kind_and_payload_object() {
        let b = bridge_event(&SessionEvent::Cancelled);
        let v = serde_json::to_value(&b).unwrap();
        assert!(v.is_object());
        assert_eq!(v["kind"], "Cancelled");
        assert!(v.get("payload").is_some());
    }

    // ---------- PC-5: new bus variants ----------

    #[test]
    fn bridge_message_committed_carries_role_and_text() {
        let b = bridge_event(&SessionEvent::MessageCommitted {
            role: atelier_core::session::MessageRole::Assistant,
            text: "starting the rename".into(),
        });
        assert_eq!(b.kind, "MessageCommitted");
        assert_eq!(b.payload["role"], "assistant");
        assert_eq!(b.payload["text"], "starting the rename");
    }

    #[test]
    fn bridge_plan_snapshot_carries_steps_array() {
        use atelier_core::plan::{PlanStatus, PlanStep};
        let b = bridge_event(&SessionEvent::PlanSnapshot {
            steps: vec![PlanStep {
                id: "step-0".into(),
                text: "first".into(),
                status: PlanStatus::Pending,
                constraints: vec![],
            }],
        });
        assert_eq!(b.kind, "PlanSnapshot");
        assert!(b.payload["steps"].is_array());
        assert_eq!(b.payload["steps"][0]["text"], "first");
    }

    #[test]
    fn bridge_ledger_appended_carries_entry() {
        use atelier_core::ledger::LedgerEntry;
        let b = bridge_event(&SessionEvent::LedgerAppended {
            entry: LedgerEntry::tool_call("t", "shell", 1.0, Some(0.001), None),
        });
        assert_eq!(b.kind, "LedgerAppended");
        assert_eq!(b.payload["entry"]["kind"], "tool_call");
        assert_eq!(b.payload["entry"]["tool_name"], "shell");
    }

    #[test]
    fn bridge_context_snapshot_carries_known_and_unknown() {
        let b = bridge_event(&SessionEvent::ContextSnapshot {
            known_tokens: 3_200,
            unknown_tokens: 150,
        });
        assert_eq!(b.kind, "ContextSnapshot");
        assert_eq!(b.payload["known_tokens"], 3_200);
        assert_eq!(b.payload["unknown_tokens"], 150);
    }

    // ---------- HR-F: pending-approval bridge ----------

    #[test]
    fn bridge_staging_pending_approval_carries_commit_id_and_files() {
        use atelier_core::session::PendingFile;
        let cid = uuid::Uuid::new_v4();
        let b = bridge_event(&SessionEvent::StagingPendingApproval {
            commit_id: cid,
            files: vec![PendingFile {
                path: PathBuf::from("src/foo.rs"),
                hunks: Hunks::Binary,
            }],
        });
        assert_eq!(b.kind, "StagingPendingApproval");
        assert_eq!(b.payload["commit_id"], cid.to_string());
        assert!(b.payload["files"].is_array());
        assert_eq!(b.payload["files"][0]["path"], "src/foo.rs");
    }

    #[test]
    fn bridge_commit_decision_lists_committed_and_dropped_paths() {
        let cid = uuid::Uuid::new_v4();
        let b = bridge_event(&SessionEvent::CommitDecision {
            commit_id: cid,
            committed: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            dropped: vec![PathBuf::from("c.rs")],
        });
        assert_eq!(b.kind, "CommitDecision");
        assert_eq!(b.payload["commit_id"], cid.to_string());
        assert_eq!(b.payload["committed"][0], "a.rs");
        assert_eq!(b.payload["committed"][1], "b.rs");
        assert_eq!(b.payload["dropped"][0], "c.rs");
    }
}
