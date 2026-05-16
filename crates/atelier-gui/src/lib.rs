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

use atelier_core::{
    session::{self, Event as SessionEvent},
    state::NoopHook,
    SessionHandle,
};
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

/// State the Tauri runtime manages for the lifetime of the shell. Wraps the
/// session handle so future commands (advance, cancel, shutdown) can
/// re-acquire it via `tauri::State<SessionState>`.
pub struct SessionState {
    pub handle: SessionHandle,
}

/// Entry point. Spawned by `main.rs`; lives in `lib.rs` so the integration
/// tests can pull in the same module and exercise the helpers.
pub fn run() {
    tracing_subscriber::fmt::try_init().ok();

    tauri::Builder::default()
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Spawn the §2.5 session actor. Hooks are no-ops here — the
            // §15 hook loader runs inside the CLI runner; the GUI shell
            // is currently a viewer, not a driver.
            //
            // `session::spawn` calls `tokio::spawn` internally, which needs
            // a runtime context. Tauri's `setup` runs on the main thread
            // outside the runtime, so we enter it via `block_on`.
            let session_handle = tauri::async_runtime::block_on(async {
                session::spawn(Arc::new(NoopHook), Arc::new(NoopHook))
            });
            let mut rx = session_handle.subscribe();
            app.manage(SessionState {
                handle: session_handle,
            });

            // Spawn one task per shell that pumps the broadcast onto the
            // Tauri event bus. Closes when the channel closes.
            tauri::async_runtime::spawn(async move {
                while let Ok(evt) = rx.recv().await {
                    emit_event(&app_handle, &evt);
                }
                tracing::info!("atelier-gui: session event bridge ended");
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![ping])
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
}
