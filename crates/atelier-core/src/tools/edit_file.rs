//! Built-in `edit_file` tool. Manifest:
//! `crates/atelier-core/tools/edit_file.v1.json`.
//!
//! Args: `{ path: string, old_text: string, new_text: string,
//! expected_count?: integer (default 1) }`. Anchor-based patch:
//! replaces `old_text` with `new_text` in the file. Requires the number
//! of matches to equal `expected_count` exactly — guards against the
//! common LLM mistake of providing an ambiguous anchor that matches
//! more than once (or zero times). Routes through §3 `Staging::commit`
//! so the edit gets atomic-application + hunk-extraction for free.

use async_trait::async_trait;
use serde::Deserialize;

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;
use crate::staging::{sha256, NoopSyntaxCheck, StagedWrite, Staging};

pub const NAME: &str = "edit_file";

/// v58 (M-sec-1b fix) — per-call cap on `new_text` size. Same
/// rationale as `write_file`'s `MAX_WRITE_BYTES` — without a cap a
/// hostile model can OOM the host with a multi-GB `new_text`, and
/// the in-place `original_text.replace(&old_text, &new_text)`
/// allocates O(N×M) when `expected_count > 1`.
pub const MAX_EDIT_NEW_TEXT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct EditFile;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default = "default_expected_count")]
    expected_count: usize,
}

fn default_expected_count() -> usize {
    1
}

#[async_trait]
impl Tool for EditFile {
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

        if parsed.old_text.is_empty() {
            return Err(ToolError::SchemaViolation {
                tool: NAME.into(),
                error: "old_text must not be empty".into(),
            });
        }
        // v58 (M-sec-1b fix) — reject oversized new_text at the
        // boundary.
        if parsed.new_text.len() > MAX_EDIT_NEW_TEXT_BYTES {
            return Err(ToolError::SchemaViolation {
                tool: NAME.into(),
                error: format!(
                    "new_text too long: {} bytes (max {MAX_EDIT_NEW_TEXT_BYTES} bytes)",
                    parsed.new_text.len()
                ),
            });
        }

        let workspace_root = ctx.workspace_root.to_path_buf();
        // read + Staging::commit are both `std::fs` — onto the blocking
        // pool so a large file edit doesn't stall the runtime.
        tokio::task::spawn_blocking(move || -> Result<ToolResult, ToolError> {
            let abs = resolve_repo_path(&workspace_root, NAME, &parsed.path)?;
            // Symlink containment — `parsed.path` could be a symlink to
            // outside the workspace. Read + write both pass through the
            // canonical path (Staging::commit re-checks via its own
            // helper).
            let canonical = ensure_inside_workspace_existing(&workspace_root, NAME, &abs)?;
            let original_bytes =
                std::fs::read(&canonical).map_err(|e| ToolError::ExecutionFailed {
                    tool: NAME.into(),
                    exit_code: -1,
                    stderr: format!("read {:?} failed: {e}", parsed.path),
                })?;
            let original_text =
                std::str::from_utf8(&original_bytes).map_err(|_| ToolError::SchemaViolation {
                    tool: NAME.into(),
                    error: format!(
                        "{:?} is not UTF-8; edit_file rejects binary files",
                        parsed.path
                    ),
                })?;

            let actual_count = original_text.matches(&parsed.old_text).count();
            if actual_count != parsed.expected_count {
                return Err(ToolError::SchemaViolation {
                    tool: NAME.into(),
                    error: format!(
                        "expected_count {} but old_text matches {} times in {:?}",
                        parsed.expected_count, actual_count, parsed.path
                    ),
                });
            }

            let new_text = original_text.replace(&parsed.old_text, &parsed.new_text);

            let check = NoopSyntaxCheck;
            let mut staging = Staging::new(&workspace_root, &check);
            staging
                .add(
                    StagedWrite::new(&parsed.path, new_text.into_bytes())
                        .with_expected_hash(sha256(&original_bytes)),
                )
                .map_err(|e| ToolError::ExecutionFailed {
                    tool: NAME.into(),
                    exit_code: -1,
                    stderr: format!("staging add failed: {e}"),
                })?;
            // v46: stage instead of commit — dispatcher gates the
            // rename on ApprovalPolicy (spec §3 hunk accept/reject).
            let batch = staging.stage().map_err(|e| ToolError::ExecutionFailed {
                tool: NAME.into(),
                exit_code: -1,
                stderr: format!("staging stage failed: {e}"),
            })?;

            Ok(ToolResult {
                output: serde_json::json!({
                    "path": parsed.path,
                    "replacements": actual_count,
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
        }
    }

    #[tokio::test]
    async fn rejects_oversized_new_text_at_the_boundary() {
        // Regression for M-sec-1b — guard against multi-GB new_text
        // exhausting host memory through the `replace` allocation.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let huge = "x".repeat(MAX_EDIT_NEW_TEXT_BYTES + 1);
        let err = EditFile
            .execute(
                serde_json::json!({
                    "path": "a.txt",
                    "old_text": "alpha",
                    "new_text": huge,
                }),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn replaces_a_single_unique_anchor() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = EditFile
            .execute(
                serde_json::json!({
                    "path": "a.txt",
                    "old_text": "beta",
                    "new_text": "BETA"
                }),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["replacements"], 1);
        // v46: EditFile stages but does NOT commit. Commit explicitly
        // to verify the on-disk result; integration goes through the
        // dispatcher in production.
        assert!(r.staged_writes.is_some());
        r.staged_writes.unwrap().commit_all().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn rejects_anchor_that_matches_more_than_expected_count() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\nx\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = EditFile
            .execute(
                serde_json::json!({"path": "a.txt", "old_text": "x", "new_text": "y"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
        // Workspace untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x\nx\nx\n"
        );
    }

    #[tokio::test]
    async fn expected_count_lets_caller_request_multi_replace() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo\nfoo\nfoo\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = EditFile
            .execute(
                serde_json::json!({
                    "path": "a.txt",
                    "old_text": "foo",
                    "new_text": "bar",
                    "expected_count": 3
                }),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["replacements"], 3);
        // v46: commit the staged batch to materialise the edit on disk.
        r.staged_writes.unwrap().commit_all().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "bar\nbar\nbar\n"
        );
    }

    #[tokio::test]
    async fn rejects_anchor_not_present() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = EditFile
            .execute(
                serde_json::json!({
                    "path": "a.txt",
                    "old_text": "ghost",
                    "new_text": "x"
                }),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn rejects_empty_old_text() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = EditFile
            .execute(
                serde_json::json!({"path": "a.txt", "old_text": "", "new_text": "y"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn rejects_non_utf8_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.bin"), [0xFFu8, 0xFE, 0xFD]).unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = EditFile
            .execute(
                serde_json::json!({"path": "a.bin", "old_text": "x", "new_text": "y"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn rejects_path_escape() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = EditFile
            .execute(
                serde_json::json!({
                    "path": "../a.txt",
                    "old_text": "x",
                    "new_text": "y"
                }),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }
}
