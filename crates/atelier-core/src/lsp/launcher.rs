//! §7 Tier-1 LSP launcher (U08 — Phase B §7 LSP live receiver).
//!
//! Provides [`LspLauncher::spawn`], the single entry point for starting
//! `typescript-language-server --stdio` and completing the LSP handshake.
//!
//! **Approval gate:** the launcher checks [`LspApprovals::is_approved`] before
//! spawning anything; callers receive [`LspLaunchError::NotApproved`] and can
//! emit `Event::RequestLspInstall` to prompt the user.
//!
//! **Sandbox:** the child argv is passed through
//! [`crate::subprocess::sandboxed_argv`] so the process inherits the same
//! macOS `sandbox-exec` / Linux `bwrap` restrictions that every shell-out in
//! the harness uses.

use std::path::Path;

use thiserror::Error;
use tokio::process::Command;

use crate::lsp::receiver::LspSession;
use crate::lsp::{lsp_approvals_path, LspApprovals};
use crate::sandbox::SandboxPolicy;
use crate::subprocess::sandboxed_argv;

/// Errors that can arise when launching an LSP server.
#[derive(Debug, Error)]
pub enum LspLaunchError {
    /// The user has not yet approved this language's LSP server. The runner
    /// emits `Event::RequestLspInstall` and falls back to Tier 2/3.
    #[error("LSP server for '{language}' is not approved — approval needed")]
    NotApproved { language: String },

    /// `sandboxed_argv` returned an error (unsupported platform, policy
    /// construction failure). Non-fatal for the run; the runner logs and
    /// falls through to Tier 2/3.
    #[error("sandbox argv construction failed: {0}")]
    SandboxArgv(#[from] crate::subprocess::SubprocessError),

    /// The LSP child process failed to spawn (not found on PATH, permission
    /// denied, etc.). Non-fatal; runner logs and falls through.
    #[error("failed to connect to LSP server: {0}")]
    Connect(String),
}

/// Spawns and handshakes an LSP server for a given language.
pub struct LspLauncher;

impl LspLauncher {
    /// Spawn `typescript-language-server --stdio` for `workspace_root`.
    ///
    /// Before spawning:
    /// 1. Loads `LspApprovals` from `<workspace_root>/.atelier/lsp/_approvals.json`.
    /// 2. Returns [`LspLaunchError::NotApproved`] when `"typescript"` is not in
    ///    the approved set.
    /// 3. Builds the sandbox-wrapped argv via [`sandboxed_argv`].
    /// 4. Spawns the child and completes `initialize` / `initialized` via
    ///    [`LspSession::connect`].
    ///
    /// # Errors
    ///
    /// See [`LspLaunchError`] variants.
    pub async fn spawn(
        workspace_root: &Path,
        sandbox: &SandboxPolicy,
        approvals: &LspApprovals,
    ) -> Result<LspSession, LspLaunchError> {
        if !approvals.is_approved("typescript") {
            return Err(LspLaunchError::NotApproved {
                language: "typescript".into(),
            });
        }

        // Build the sandboxed argv.
        let raw_argv: Vec<String> = vec!["typescript-language-server".into(), "--stdio".into()];
        let (program, args) = sandboxed_argv(&raw_argv, sandbox)?;

        let mut cmd = Command::new(&program);
        cmd.args(&args).current_dir(workspace_root);

        LspSession::connect(workspace_root, cmd)
            .await
            .map_err(LspLaunchError::Connect)
    }

    /// Load `LspApprovals` from the conventional path under `workspace_root`
    /// and delegate to [`Self::spawn`]. Convenience wrapper so callers don't
    /// have to manage the file path themselves.
    pub async fn spawn_loading_approvals(
        workspace_root: &Path,
        sandbox: &SandboxPolicy,
    ) -> Result<LspSession, LspLaunchError> {
        let approvals_path = lsp_approvals_path(workspace_root);
        let approvals = LspApprovals::load(&approvals_path).unwrap_or_default();
        Self::spawn(workspace_root, sandbox, &approvals).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn restrictive_policy(workspace: &Path) -> SandboxPolicy {
        SandboxPolicy::restrictive(workspace).expect("restrictive policy")
    }

    /// Unit test: `LspLauncher::spawn` returns `NotApproved` immediately when
    /// the `LspApprovals` store has no entry for `"typescript"`. No subprocess
    /// is spawned — the approval gate fires before any `Command::spawn`.
    #[tokio::test]
    async fn lsp_launcher_returns_not_approved_when_no_approval() {
        let td = TempDir::new().unwrap();
        let workspace = td.path();
        let approvals = LspApprovals::default(); // empty — no approvals
        let sandbox = restrictive_policy(workspace);

        let result = LspLauncher::spawn(workspace, &sandbox, &approvals).await;

        assert!(
            matches!(result, Err(LspLaunchError::NotApproved { ref language }) if language == "typescript"),
            "expected NotApproved for typescript, got: {result:?}"
        );
    }

    /// Integration test: requires `typescript-language-server` on PATH.
    /// Run with `cargo test -p atelier-core -- --ignored lsp_launcher_spawns`.
    #[tokio::test]
    #[ignore = "requires typescript-language-server on PATH"]
    async fn lsp_launcher_spawns_and_handshakes() {
        let td = TempDir::new().unwrap();
        let workspace = td.path();
        let mut approvals = LspApprovals::default();
        approvals.approve("typescript", "2026-05-20T00:00:00Z");
        let sandbox = restrictive_policy(workspace);

        let session = LspLauncher::spawn(workspace, &sandbox, &approvals)
            .await
            .expect("spawn should succeed when typescript-language-server is on PATH");

        session.shutdown().await;
    }
}
