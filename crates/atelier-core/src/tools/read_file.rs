//! Built-in `read_file` tool. Manifest:
//! `crates/atelier-core/tools/read_file.v1.json`.
//!
//! Args: `{ path: string, offset?: integer, length?: integer }`.
//! Repo-relative path; optional byte range. Returns
//! `{ contents: string, byte_len: integer, truncated: bool }`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;

pub const NAME: &str = "read_file";

#[derive(Debug, Default)]
pub struct ReadFile;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Output {
    contents: String,
    byte_len: u64,
    truncated: bool,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        NAME
    }

    fn side_effect_class(&self) -> SideEffectClass {
        SideEffectClass::LocalSafe
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
        // All file I/O + path canonicalisation runs on the blocking pool so
        // a multi-MB read doesn't stall a tokio worker. The §2.5 actor and
        // the broadcast bus need their worker threads to stay live.
        let workspace_root = ctx.workspace_root.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<ToolResult, ToolError> {
            let abs = resolve_repo_path(&workspace_root, NAME, &parsed.path)?;
            // Symlink containment: catch the case where `parsed.path`
            // itself (or a path component) is a symlink pointing outside
            // the workspace. `resolve_repo_path` only rejects literal `..`.
            let canonical = ensure_inside_workspace_existing(&workspace_root, NAME, &abs)?;
            let bytes = std::fs::read(&canonical).map_err(|e| ToolError::ExecutionFailed {
                tool: NAME.into(),
                exit_code: -1,
                stderr: format!("read {:?} failed: {e}", parsed.path),
            })?;
            let total = bytes.len() as u64;

            let offset = parsed.offset.unwrap_or(0);
            if offset > total {
                return Err(ToolError::SchemaViolation {
                    tool: NAME.into(),
                    error: format!("offset {offset} exceeds file length {total}"),
                });
            }
            let remaining = total - offset;
            let take = parsed.length.unwrap_or(remaining).min(remaining);
            let slice = &bytes[offset as usize..(offset + take) as usize];
            let truncated = take < remaining;

            // UTF-8 best-effort. Binary files lose information; the tool
            // manifest's natural-language description tells the model to
            // use dedicated tools for binary, so this is acceptable for v1.
            let contents = String::from_utf8_lossy(slice).into_owned();

            Ok(ToolResult {
                output: serde_json::to_value(Output {
                    contents,
                    byte_len: total,
                    truncated,
                })
                .expect("Output serialises"),
                staged_writes: None,
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
        }
    }

    #[tokio::test]
    async fn reads_full_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = ReadFile
            .execute(serde_json::json!({"path": "a.txt"}), &ctx(dir.path(), &s))
            .await
            .unwrap();
        assert_eq!(r.output["contents"], "hello");
        assert_eq!(r.output["byte_len"], 5);
        assert_eq!(r.output["truncated"], false);
    }

    #[tokio::test]
    async fn reads_with_offset_and_length() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"abcdefghij").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = ReadFile
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 2, "length": 4}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["contents"], "cdef");
        assert_eq!(r.output["byte_len"], 10);
        assert_eq!(r.output["truncated"], true);
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = ReadFile
            .execute(
                serde_json::json!({"path": "/etc/passwd"}),
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
        let err = ReadFile
            .execute(
                serde_json::json!({"path": "../outside"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[tokio::test]
    async fn missing_file_returns_execution_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = ReadFile
            .execute(
                serde_json::json!({"path": "ghost.txt"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_pointing_outside_workspace() {
        // Sets up: workspace contains `looks_safe.txt` which is actually a
        // symlink to a file outside the workspace. read_file must refuse.
        let ws = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"sensitive").unwrap();
        std::os::unix::fs::symlink(&secret, ws.path().join("looks_safe.txt")).unwrap();

        let s = SandboxPolicy::restrictive(ws.path()).unwrap();
        let err = ReadFile
            .execute(
                serde_json::json!({"path": "looks_safe.txt"}),
                &ctx(ws.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[tokio::test]
    async fn offset_past_eof_is_schema_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"abc").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = ReadFile
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 99}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }
}
