//! Built-in `list_dir` tool. Manifest:
//! `crates/atelier-core/tools/list_dir.v1.json`.
//!
//! Args: `{ path: string, include_hidden?: bool }`. Returns
//! `{ entries: [{ name, kind, size? }] }` sorted by name.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;

pub const NAME: &str = "list_dir";

#[derive(Debug, Default)]
pub struct ListDir;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    #[serde(default)]
    include_hidden: bool,
}

#[derive(Debug, Serialize)]
struct Entry {
    name: String,
    kind: EntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[async_trait]
impl Tool for ListDir {
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
        let workspace_root = ctx.workspace_root.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<ToolResult, ToolError> {
            let abs = resolve_repo_path(&workspace_root, NAME, &parsed.path)?;
            // Symlink containment — `parsed.path` itself could be a
            // symlink pointing out of the workspace. The leaves we report
            // are still raw paths; downstream tools (read_file) re-check
            // via the same helper before reading.
            let canonical = ensure_inside_workspace_existing(&workspace_root, NAME, &abs)?;

            let dir_iter =
                std::fs::read_dir(&canonical).map_err(|e| ToolError::ExecutionFailed {
                    tool: NAME.into(),
                    exit_code: -1,
                    stderr: format!("read_dir {:?} failed: {e}", parsed.path),
                })?;

            let mut entries: Vec<Entry> = Vec::new();
            for dir_entry in dir_iter {
                let dir_entry = dir_entry.map_err(|e| ToolError::ExecutionFailed {
                    tool: NAME.into(),
                    exit_code: -1,
                    stderr: format!("dir iteration failed: {e}"),
                })?;
                let name = match dir_entry.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue, // non-UTF-8 name; skip
                };
                if !parsed.include_hidden && name.starts_with('.') {
                    continue;
                }
                let file_type = match dir_entry.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let kind = if file_type.is_file() {
                    EntryKind::File
                } else if file_type.is_dir() {
                    EntryKind::Dir
                } else if file_type.is_symlink() {
                    EntryKind::Symlink
                } else {
                    EntryKind::Other
                };
                let size = if matches!(kind, EntryKind::File) {
                    dir_entry.metadata().ok().map(|m| m.len())
                } else {
                    None
                };
                entries.push(Entry { name, kind, size });
            }

            entries.sort_by(|a, b| a.name.cmp(&b.name));

            Ok(ToolResult {
                output: serde_json::json!({ "entries": entries }),
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
            tool_call_id: None,
            audit_log_path: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
            subagent_depth: 0,
        }
    }

    #[tokio::test]
    async fn lists_visible_files_sorted_with_size_for_files_only() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("zebra.txt"), b"zz").unwrap();
        std::fs::write(dir.path().join("apple.txt"), b"a").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();

        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = ListDir
            .execute(serde_json::json!({"path": ""}), &ctx(dir.path(), &s))
            .await
            .unwrap();
        let entries = r.output["entries"].as_array().unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["apple.txt", "sub", "zebra.txt"]);
        // .hidden excluded by default
        assert!(!names.contains(&".hidden".to_string()));
        // file size present
        assert_eq!(entries[0]["size"], 1);
        // dir size absent
        assert!(entries[1].get("size").is_none() || entries[1]["size"].is_null());
    }

    #[tokio::test]
    async fn includes_hidden_when_requested() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), b"X=1").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = ListDir
            .execute(
                serde_json::json!({"path": "", "include_hidden": true}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        let names: Vec<_> = r.output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&".env".to_string()));
    }

    #[tokio::test]
    async fn rejects_path_escape() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = ListDir
            .execute(serde_json::json!({"path": "../"}), &ctx(dir.path(), &s))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[tokio::test]
    async fn missing_dir_is_execution_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = ListDir
            .execute(serde_json::json!({"path": "ghost"}), &ctx(dir.path(), &s))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }
}
