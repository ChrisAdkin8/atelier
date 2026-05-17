//! Built-in `shell` tool. Manifest:
//! `crates/atelier-core/tools/shell.v1.json`.
//!
//! Args: `{ command: string, cwd?: string, timeout_ms?: integer,
//! allow_net?: bool }`. Runs `sh -c <command>` inside the §11 sandbox
//! (`sandbox-exec` on macOS, `bwrap` on Linux). Captures stdout / stderr /
//! exit code via the shared [`crate::subprocess`] helper.
//!
//! `allow_net: true` flips the sandbox policy from default-deny to
//! allow-network-egress — agents must request this explicitly per call
//! (matching the manifest convention: opt-in, surfaces in the §8 trust
//! budget UI).

use async_trait::async_trait;
use serde::Deserialize;

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;
#[cfg(test)]
use crate::sandbox::SandboxPolicy;
use crate::subprocess::{run as run_subprocess, sandboxed_argv, SubprocessSpec};

pub const NAME: &str = "shell";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Default)]
pub struct Shell;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    allow_net: bool,
}

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        NAME
    }

    fn side_effect_class(&self) -> SideEffectClass {
        SideEffectClass::LocalRisky
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Args =
            serde_json::from_value(args).map_err(|e| ToolError::SchemaViolation {
                tool: NAME.into(),
                error: e.to_string(),
            })?;
        if parsed.command.is_empty() {
            return Err(ToolError::SchemaViolation {
                tool: NAME.into(),
                error: "command must not be empty".into(),
            });
        }

        // cwd, if provided, is repo-relative and path-validated.
        //
        // v57 (H8 fix) — the pre-v57 path called only `resolve_repo_path`,
        // which is syntax-only (rejects `..` + absolute paths but does
        // NOT follow symlinks). A model that wrote a symlink
        // `escape -> /Users/me` inside the workspace via `write_file`
        // could then call `shell` with `cwd: "escape"` and start the
        // child under attacker-controlled cwd. macOS sandbox-exec
        // still bounds the FS; Linux bwrap binds the original repo
        // path. This added containment is defence-in-depth.
        let cwd_abs = if let Some(rel) = parsed.cwd.as_deref() {
            let abs = resolve_repo_path(ctx.workspace_root, NAME, rel)?;
            Some(ensure_inside_workspace_existing(
                ctx.workspace_root,
                NAME,
                &abs,
            )?)
        } else {
            None
        };

        // Clone the session's sandbox policy so any per-session extras
        // (extra_read_paths, extra_write_paths) survive into the shell
        // call. Mutating the clone for `allow_net` doesn't affect the
        // session default. Prior versions rebuilt the policy from scratch
        // via `SandboxPolicy::restrictive(ctx.sandbox.repo_root())`,
        // which silently dropped any extras the session had granted.
        let mut policy = ctx.sandbox.clone();
        if parsed.allow_net || ctx.sandbox.allow_net_flag() {
            policy = policy.with_net();
        }

        let user_argv = vec!["sh".to_string(), "-c".to_string(), parsed.command.clone()];

        let (program, wrapped_args) =
            sandboxed_argv(&user_argv, &policy).map_err(|e| ToolError::SandboxViolation {
                tool: NAME.into(),
                attempted: format!("sandbox wrap failed: {e}"),
            })?;

        let mut spec =
            SubprocessSpec::with_budget_ms(parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        spec.working_dir = cwd_abs;

        let outcome = run_subprocess(&program, &wrapped_args, &spec)
            .await
            .map_err(|e| ToolError::ExecutionFailed {
                tool: NAME.into(),
                exit_code: -1,
                stderr: format!("subprocess spawn failed: {e}"),
            })?;

        // The contract: the agent receives the captured output regardless
        // of exit code. Non-zero exit + timed_out flow back as part of
        // `output` so the model can decide what to do; only a real
        // SandboxViolation (which sandbox-exec / bwrap surfaces via exit
        // code, not this layer) escalates to a typed ToolError. For v0
        // we leave that detection to the subprocess result — agents see
        // the exit code and stderr.
        Ok(ToolResult {
            output: serde_json::json!({
                "exit_code": outcome.exit_code,
                "stdout": outcome.stdout_str_lossy(),
                "stderr": outcome.stderr_str_lossy(),
                "stdout_truncated": outcome.stdout_truncated,
                "stderr_truncated": outcome.stderr_truncated,
                "duration_ms": outcome.duration_ms,
                "timed_out": outcome.timed_out,
            }),
            staged_writes: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(root: &'a Path, sandbox: &'a SandboxPolicy) -> ToolContext<'a> {
        ToolContext {
            workspace_root: root,
            sandbox,
        }
    }

    /// Tests gated on macOS because sandbox-exec is always present there.
    /// On Linux, bwrap may not be installed; these are integration tests
    /// rather than the dispatcher's unit-test surface. The
    /// dispatcher-level unit tests use the EchoTool mock instead.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shell_runs_simple_command_inside_sandbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Shell
            .execute(
                serde_json::json!({"command": "echo hello"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["exit_code"], 0);
        let stdout = r.output["stdout"].as_str().unwrap();
        assert!(stdout.contains("hello"), "stdout: {stdout:?}");
    }

    #[tokio::test]
    async fn empty_command_is_schema_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = Shell
            .execute(serde_json::json!({"command": ""}), &ctx(dir.path(), &s))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn cwd_escape_is_permission_denied() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = Shell
            .execute(
                serde_json::json!({"command": "true", "cwd": "../outside"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_through_symlink_escaping_workspace_is_permission_denied() {
        // Regression for H8 — `resolve_repo_path` was the only check
        // pre-v57, and it didn't follow symlinks. A symlink inside the
        // workspace pointing outside it should be rejected by
        // `ensure_inside_workspace_existing`.
        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let link = workspace.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let s = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let err = Shell
            .execute(
                serde_json::json!({"command": "true", "cwd": "escape"}),
                &ctx(workspace.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { .. }),
            "shell with cwd through a symlink-out must be PermissionDenied; got {err:?}"
        );
    }
}
