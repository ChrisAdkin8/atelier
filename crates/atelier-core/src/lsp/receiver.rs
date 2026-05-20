//! §7 verify Tier-1 LSP receiver (U07 — Phase B §7 LSP live receiver).
//!
//! Wraps `async-lsp 0.2` against `typescript-language-server --stdio`.
//! The public surface:
//!
//!   - [`LspSession`] — owns a running `typescript-language-server` child and
//!     the `async-lsp` `ServerSocket` for calling LSP methods on it.
//!   - [`LspSession::open_file`] — sends `textDocument/didOpen`.
//!   - [`LspSession::collect_diagnostics`] — waits up to `timeout`, returns
//!     `Vec<(String, DiagnosticInput)>` where the `String` is the absolute
//!     path from the `publishDiagnostics` URI (rebasing to workspace-relative
//!     is the caller's responsibility — the runner strips the workspace prefix
//!     before calling `map_diagnostic_to_discrepancy`).
//!   - [`LspSession::shutdown`] — sends `shutdown` + `exit`, joins the mainloop.
//!
//! **Conversion boundary:** `lsp_types::Diagnostic` → `DiagnosticInput` happens
//! inside the notification handler (inside the mainloop), so the pure-function
//! mapper in [`crate::lsp::typescript`] never sees `lsp_types` directly.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_lsp::router::Router;
use async_lsp::{LanguageServer, MainLoop, ServerSocket};
use lsp_types::notification::{PublishDiagnostics, ShowMessage};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    TextDocumentItem, Url,
};
use parking_lot::Mutex;
use tokio::time;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower::ServiceBuilder;

use crate::lsp::DiagnosticInput;

/// One collected diagnostic: the absolute path that the LSP server reported
/// (from the `publishDiagnostics` URI) plus the typed input ready for the
/// pure-function mapper.
pub type RawDiagnostic = (String, DiagnosticInput);

/// Internal accumulator type inside the mainloop.
type DiagBuf = Arc<Mutex<Vec<RawDiagnostic>>>;

/// State threaded through the `async-lsp` Router. Holds the shared
/// diagnostics accumulator so the `PublishDiagnostics` notification handler
/// can append and the outer task can drain.
struct ClientState {
    diagnostics: DiagBuf,
}

/// Loopback event that signals the mainloop router to exit after `shutdown`
/// + `exit` have been sent by the caller.
struct Stop;

/// A live LSP session against a running `typescript-language-server` child
/// process. Created by [`crate::lsp::launcher::LspLauncher::spawn`].
///
/// Call [`shutdown`][Self::shutdown] before dropping for a clean teardown;
/// the child is `kill_on_drop`-guarded so an unclean drop still reaps it.
pub struct LspSession {
    /// Socket to call LSP methods on the server.
    server: ServerSocket,
    /// Shared diagnostics buffer. Notification handler appends; caller drains
    /// via [`collect_diagnostics`][Self::collect_diagnostics].
    diagnostics: DiagBuf,
    /// Handle to the mainloop task. Joined on [`shutdown`][Self::shutdown].
    mainloop_handle: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for LspSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspSession").finish_non_exhaustive()
    }
}

impl LspSession {
    /// Spawn a child process (already configured with piped stdio by the
    /// caller — see `LspLauncher`) and complete the `initialize` /
    /// `initialized` handshake.
    ///
    /// `workspace_root` is used as the LSP `rootUri` / `workspaceFolders`
    /// entry (must be an absolute path).
    pub(crate) async fn connect(
        workspace_root: &Path,
        mut child_cmd: tokio::process::Command,
    ) -> Result<Self, String> {
        let diagnostics: DiagBuf = Arc::new(Mutex::new(Vec::new()));
        let diags_for_handler = Arc::clone(&diagnostics);

        let (mainloop, mut server) = MainLoop::new_client(move |_server: ServerSocket| {
            let mut router = Router::new(ClientState {
                diagnostics: Arc::clone(&diags_for_handler),
            });
            router
                .notification::<PublishDiagnostics>(|state, params| {
                    // Extract the file path from the URI.
                    let abs_path = params
                        .uri
                        .to_file_path()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let mut guard = state.diagnostics.lock();
                    for diag in &params.diagnostics {
                        guard.push((
                            abs_path.clone(),
                            DiagnosticInput {
                                line_zero_indexed: diag.range.start.line,
                                character_zero_indexed: diag.range.start.character,
                                message: diag.message.clone(),
                            },
                        ));
                    }
                    ControlFlow::Continue(())
                })
                // ShowMessage: informational only; ignore.
                .notification::<ShowMessage>(|_, _| ControlFlow::Continue(()))
                // Stop event: caller asked the mainloop to exit.
                .event(|_, _: Stop| ControlFlow::Break(Ok(())))
                // All other notifications ($/progress, $/logTrace, etc.): ignore silently.
                .unhandled_notification(|_, _| ControlFlow::Continue(()));

            ServiceBuilder::new().service(router)
        });

        let mut child = child_cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn typescript-language-server: {e}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "child has no stdout".to_string())?
            .compat();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child has no stdin".to_string())?
            .compat_write();

        let mainloop_handle = tokio::spawn(async move {
            // Drive the mainloop; errors after Stop are expected.
            let _ = mainloop.run_buffered(stdout, stdin).await;
            // Wait for the child so no zombie remains.
            let _ = child.wait().await;
        });

        // Complete the initialize handshake.
        let root_uri = Url::from_file_path(workspace_root)
            .map_err(|()| format!("workspace_root is not absolute: {workspace_root:?}"))?;

        server
            .initialize(InitializeParams {
                workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                    uri: root_uri,
                    name: "root".into(),
                }]),
                capabilities: ClientCapabilities::default(),
                ..InitializeParams::default()
            })
            .await
            .map_err(|e| format!("LSP initialize failed: {e}"))?;

        server
            .initialized(InitializedParams {})
            .map_err(|e| format!("LSP initialized notification failed: {e}"))?;

        Ok(Self {
            server,
            diagnostics,
            mainloop_handle: Some(mainloop_handle),
        })
    }

    /// Send `textDocument/didOpen` for `path` with `content`. Diagnostics for
    /// this file will arrive asynchronously as `publishDiagnostics` notifications.
    ///
    /// `path` must be absolute so the LSP URI round-trip is unambiguous.
    pub fn open_file(&mut self, path: &Path, content: &str) -> Result<(), String> {
        let uri =
            Url::from_file_path(path).map_err(|()| format!("path is not absolute: {path:?}"))?;
        self.server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "typescript".into(),
                    version: 1,
                    text: content.to_string(),
                },
            })
            .map_err(|e| format!("LSP didOpen failed: {e}"))
    }

    /// Wait up to `timeout` for diagnostics to arrive, then return all that
    /// landed as `Vec<(absolute_path, DiagnosticInput)>`. The caller (runner)
    /// rebases `absolute_path` to workspace-relative before passing to
    /// [`crate::lsp::map_diagnostic_to_discrepancy`].
    ///
    /// Poll strategy: check every 50 ms. TypeScript-language-server typically
    /// responds in 1–3 s on first open; 50 ms keeps the wait tight without
    /// burning CPU.
    pub async fn collect_diagnostics(&self, timeout: Duration) -> Vec<RawDiagnostic> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let guard = self.diagnostics.lock();
                if !guard.is_empty() {
                    return guard.clone();
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            time::sleep(Duration::from_millis(50)).await;
        }
        // Return whatever arrived (possibly empty on timeout).
        self.diagnostics.lock().clone()
    }

    /// Send `shutdown` + `exit`, emit the `Stop` event to break the router,
    /// then join the mainloop task (with a 5-second timeout so a slow server
    /// doesn't block the runner).
    pub async fn shutdown(mut self) {
        let _ = self.server.shutdown(()).await;
        let _ = self.server.exit(());
        let _ = self.server.emit(Stop);
        if let Some(handle) = self.mainloop_handle.take() {
            let _ = time::timeout(Duration::from_secs(5), handle).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::DiagnosticInput;

    /// Verify `RawDiagnostic` is the expected `(String, DiagnosticInput)` pair.
    /// This is a compile-time / shape test — ensures the module's type alias
    /// matches what the runner expects.
    #[test]
    fn raw_diagnostic_is_path_plus_input() {
        let raw: RawDiagnostic = (
            "/workspace/src/foo.ts".to_string(),
            DiagnosticInput {
                line_zero_indexed: 5,
                character_zero_indexed: 2,
                message: "Property 'x' does not exist on type 'Y'".into(),
            },
        );
        assert_eq!(raw.0, "/workspace/src/foo.ts");
        assert_eq!(raw.1.line_zero_indexed, 5);
    }
}
