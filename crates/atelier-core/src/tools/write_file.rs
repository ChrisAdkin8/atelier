//! Built-in `write_file` tool. Manifest:
//! `crates/atelier-core/tools/write_file.v1.json`.
//!
//! Args: `{ path: string, content: string, create_dirs?: bool }`. Routes
//! through §3 `Staging::commit` so the workspace gets the all-or-nothing
//! atomic-application guarantee even for single-file writes — and the
//! resulting `CommitReport` flows out as the dispatcher's
//! `Event::EditStaged` event (Phase C live diff).

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    ensure_inside_workspace_creatable, ensure_inside_workspace_existing, resolve_repo_path,
};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;
use crate::staging::{NoopSyntaxCheck, StagedWrite, Staging};

pub const NAME: &str = "write_file";

/// v58 (M-sec-1b fix) — per-call write cap. The model can ask to
/// write any byte count it likes; without a cap a hostile or buggy
/// emission of multi-GB content allocates through json deserialise →
/// `into_bytes` → `write_with_sync`, plus a per-line hunk walk in
/// `Staging::stage`. 16 MiB is large enough for any realistic source
/// file and matches the order of magnitude of `read_file`'s cap.
pub const MAX_WRITE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct WriteFile;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    content: String,
    #[serde(default)]
    create_dirs: bool,
}

#[async_trait]
impl Tool for WriteFile {
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
        // v58 (M-sec-1b fix) — reject oversized writes at the boundary
        // rather than allocating through stage().
        if parsed.content.len() > MAX_WRITE_BYTES {
            return Err(ToolError::SchemaViolation {
                tool: NAME.into(),
                error: format!(
                    "content too long: {} bytes (max {MAX_WRITE_BYTES} bytes)",
                    parsed.content.len()
                ),
            });
        }

        let workspace_root = ctx.workspace_root.to_path_buf();
        // `Staging::commit` is all-synchronous I/O. Move it to the
        // blocking pool so the runtime keeps draining the broadcast bus
        // and the actor inbox.
        tokio::task::spawn_blocking(move || -> Result<ToolResult, ToolError> {
            // Path-escape validation: syntax (no `..`, no absolute) here,
            // plus symlink containment below after any required
            // `create_dirs`. Both `Staging::add` and `Staging::commit`
            // re-check on the staging side; doubling up keeps the error
            // surface attributed to this tool when something's off.
            let abs = resolve_repo_path(&workspace_root, NAME, &parsed.path)?;

            // `Staging` requires the parent directory of every staged
            // write to already exist (or be creatable inside the staging
            // tempdir). For committing into the workspace, the dispatcher
            // relies on the OS semantics of
            // `rename(temp_dir/foo/bar.rs, workspace/foo/bar.rs)` — the
            // parent must exist. `create_dirs: true` ensures it does.
            if parsed.create_dirs {
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| ToolError::ExecutionFailed {
                        tool: NAME.into(),
                        exit_code: -1,
                        stderr: format!("create_dir_all {parent:?} failed: {e}"),
                    })?;
                }
            }

            // Symlink containment: if the target already exists, treat it
            // as existing (so a symlink-to-outside is caught); otherwise
            // check the parent so a `create_dirs` chain through a
            // symlinked dir is caught.
            let _canonical = if abs.exists() {
                ensure_inside_workspace_existing(&workspace_root, NAME, &abs)?
            } else {
                ensure_inside_workspace_creatable(&workspace_root, NAME, &abs)?
            };

            let check = NoopSyntaxCheck;
            let mut staging = Staging::new(&workspace_root, &check);
            // v57 (H2 fix): capture length before consuming `content`
            // so `bytes_written` reflects the agent's content length
            // regardless of the resulting `Hunks` variant.
            let content_bytes = parsed.content.into_bytes();
            let bytes_written = content_bytes.len();
            staging
                .add(StagedWrite::new(&parsed.path, content_bytes))
                .map_err(|e| ToolError::ExecutionFailed {
                    tool: NAME.into(),
                    exit_code: -1,
                    stderr: format!("staging add failed: {e}"),
                })?;

            // v46: stage instead of commit. The dispatcher's
            // ApprovalPolicy decides whether to auto-commit
            // (current behaviour, the default) or to emit
            // StagingPendingApproval and wait for the user's accept
            // set (spec §3 hunk accept/reject).
            let batch = staging.stage().map_err(|e| ToolError::ExecutionFailed {
                tool: NAME.into(),
                exit_code: -1,
                stderr: format!("staging stage failed: {e}"),
            })?;
            // v57 (H2 fix) — `bytes_written` was captured above before
            // the content was consumed.

            Ok(ToolResult {
                output: serde_json::json!({
                    "path": parsed.path,
                    "bytes_written": bytes_written,
                }),
                staged_writes: Some(batch),
            })
        })
        .await
        .map_err(|join_err| super::join_error_to_tool_error(NAME, join_err))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxPolicy;
    use std::path::Path;

    fn ctx<'a>(root: &'a Path, sandbox: &'a SandboxPolicy) -> ToolContext<'a> {
        ToolContext {
            workspace_root: root,
            sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
        }
    }

    #[tokio::test]
    async fn writes_a_new_file_and_returns_staged_report() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = WriteFile
            .execute(
                serde_json::json!({"path": "a.txt", "content": "hello"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        // v46: WriteFile stages but does NOT commit — the dispatcher's
        // ApprovalPolicy gates the rename. Without going through the
        // dispatcher this test commits the batch directly.
        assert!(r.staged_writes.is_some(), "should produce staged writes");
        // v57 (H2): bytes_written is the content length, regardless of
        // whether the diff is Create / Lines / Same.
        assert_eq!(r.output["bytes_written"], 5);
        let report = r.staged_writes.unwrap().commit_all().unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, std::path::PathBuf::from("a.txt"));
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn rejects_oversized_content_at_the_boundary() {
        // Regression for M-sec-1b — a model emitting multi-GB content
        // would OOM the host through stage()'s allocations. Reject
        // before staging touches the bytes.
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let huge = "x".repeat(MAX_WRITE_BYTES + 1);
        let err = WriteFile
            .execute(
                serde_json::json!({"path": "a.txt", "content": huge}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn overwriting_an_existing_file_reports_content_len_not_zero() {
        // Regression for H2 — pre-v57 the bytes_written field was
        // derived from `Hunks::Created` and reported `0` for any
        // overwrite (Lines / Same).
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"old contents").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = WriteFile
            .execute(
                serde_json::json!({"path": "a.txt", "content": "new content here"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(
            r.output["bytes_written"], 16,
            "bytes_written must equal new content length, not 0"
        );
        let _ = r.staged_writes.unwrap().commit_all().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("a.txt")).unwrap(),
            b"new content here"
        );
    }

    #[tokio::test]
    async fn creates_nested_dirs_when_requested() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = WriteFile
            .execute(
                serde_json::json!({
                    "path": "src/lib/inner.rs",
                    "content": "// hi",
                    "create_dirs": true
                }),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        // v46: commit explicitly outside the dispatcher.
        r.staged_writes.unwrap().commit_all().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("src/lib/inner.rs")).unwrap(),
            b"// hi"
        );
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = WriteFile
            .execute(
                serde_json::json!({"path": "/etc/passwd", "content": "x"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn rejects_parent_dir_escape() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = WriteFile
            .execute(
                serde_json::json!({"path": "../outside.txt", "content": "x"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }
}
