//! Built-in `grep` tool. Manifest:
//! `crates/atelier-core/tools/grep.v1.json`.
//!
//! Args: `{ pattern: string, path?: string, case_insensitive?: bool,
//! max_results?: integer }`. Searches text files under `path` for lines
//! matching the regex; returns `{ matches: [{path, line_number, line}],
//! truncated: bool }`. Skips binary files (NUL in first 8 KB), hidden
//! files / dotted dirs (`.git`, `.atelier`, etc.), and files larger
//! than 1 MB to keep search bounded.

use async_trait::async_trait;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;

pub const NAME: &str = "grep";
const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_FILE_BYTES: u64 = 1_048_576; // 1 MB

#[derive(Debug, Default)]
pub struct Grep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Match {
    path: String,
    line_number: u32,
    line: String,
}

#[async_trait]
impl Tool for Grep {
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

        let re = RegexBuilder::new(&parsed.pattern)
            .case_insensitive(parsed.case_insensitive)
            .build()
            .map_err(|e| ToolError::SchemaViolation {
                tool: NAME.into(),
                error: format!("invalid regex: {e}"),
            })?;

        let workspace_root = ctx.workspace_root.to_path_buf();
        // Walk + per-file read is `std::fs` + `walkdir` (both blocking) —
        // move onto the blocking pool so a deep walk doesn't stall the
        // tokio runtime.
        tokio::task::spawn_blocking(move || -> Result<ToolResult, ToolError> {
            let root =
                resolve_repo_path(&workspace_root, NAME, parsed.path.as_deref().unwrap_or(""))?;
            // Symlink containment on the walk root itself. Per-entry
            // symlinks inside the walk are skipped below — see the
            // `is_symlink` check.
            let root = ensure_inside_workspace_existing(&workspace_root, NAME, &root)?;

            let cap = parsed.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
            let mut matches: Vec<Match> = Vec::new();
            let mut truncated = false;

            for entry in walkdir::WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    // depth==0 is the search root itself; never filter it out
                    // (tempdir-prefixed roots like `.tmpXyZ` would otherwise be
                    // rejected by the dot-prefix rule below).
                    if e.depth() == 0 {
                        return true;
                    }
                    e.file_name()
                        .to_str()
                        .map(|n| !n.starts_with('.'))
                        .unwrap_or(false)
                })
            {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                // Skip symlinks at the leaf: `std::fs::read` follows them,
                // so a symlink in the repo pointing to /etc/passwd would
                // otherwise be grep'd. `WalkDir::follow_links(false)` only
                // controls *traversal*; we still need to filter symlinked
                // files explicitly.
                if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                    continue;
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
                let bytes = match std::fs::read(entry.path()) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Binary detection — same heuristic as the §3 / §14 diff layer.
                let head = &bytes[..bytes.len().min(8 * 1024)];
                if head.contains(&0u8) {
                    continue;
                }
                let text = match std::str::from_utf8(&bytes) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                // Use canonical root for the strip — `entry.path()` starts
                // with whatever `WalkDir::new(&root)` was given, and `root`
                // is already canonical above.
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                for (idx, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        if matches.len() >= cap {
                            truncated = true;
                            break;
                        }
                        matches.push(Match {
                            path: rel.clone(),
                            line_number: (idx as u32) + 1,
                            line: line.to_string(),
                        });
                    }
                }
                if truncated {
                    break;
                }
            }

            Ok(ToolResult {
                output: serde_json::json!({ "matches": matches, "truncated": truncated }),
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
        }
    }

    #[tokio::test]
    async fn finds_matches_with_line_numbers() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        )
        .unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Grep
            .execute(
                serde_json::json!({"pattern": "fn beta"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        let matches = r.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line_number"], 2);
        assert_eq!(matches[0]["line"], "fn beta() {}");
        assert_eq!(matches[0]["path"], "a.rs");
        assert_eq!(r.output["truncated"], false);
    }

    #[tokio::test]
    async fn case_insensitive_works() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "Hello\nWORLD\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Grep
            .execute(
                serde_json::json!({"pattern": "world", "case_insensitive": true}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["matches"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn max_results_truncates() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut content = String::new();
        for _ in 0..50 {
            content.push_str("match\n");
        }
        std::fs::write(dir.path().join("a.txt"), content).unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Grep
            .execute(
                serde_json::json!({"pattern": "match", "max_results": 10}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["matches"].as_array().unwrap().len(), 10);
        assert_eq!(r.output["truncated"], true);
    }

    #[tokio::test]
    async fn skips_hidden_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[match]\n").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "match\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Grep
            .execute(
                serde_json::json!({"pattern": "match"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        let paths: Vec<_> = r.output["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec!["visible.txt"]);
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("text.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"\x00needle\n").unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Grep
            .execute(
                serde_json::json!({"pattern": "needle"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        let paths: Vec<_> = r.output["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec!["text.txt"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_symlinked_files_pointing_outside_workspace() {
        let ws = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "needle\n").unwrap();
        std::os::unix::fs::symlink(&secret, ws.path().join("evil_link.txt")).unwrap();
        std::fs::write(ws.path().join("legit.txt"), "needle\n").unwrap();

        let s = SandboxPolicy::restrictive(ws.path()).unwrap();
        let r = Grep
            .execute(
                serde_json::json!({"pattern": "needle"}),
                &ctx(ws.path(), &s),
            )
            .await
            .unwrap();
        let paths: Vec<_> = r.output["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        // Only the legitimate (non-symlink) file is grep'd; the symlink
        // pointing outside the workspace is skipped silently.
        assert_eq!(paths, vec!["legit.txt"]);
    }

    #[tokio::test]
    async fn invalid_regex_is_schema_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = Grep
            .execute(
                serde_json::json!({"pattern": "[unclosed"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }
}
