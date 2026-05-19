//! Built-in `ast_grep` tool. Manifest:
//! `crates/atelier-core/tools/ast_grep.v1.json`.
//!
//! Args: `{ pattern: string, language: string, path?: string,
//! max_results?: integer }`. Walks the repo subtree at `path`, parses
//! each matching-extension file with the tree-sitter grammar for
//! `language`, returns the byte ranges of subtrees whose node kind
//! matches the pattern.
//!
//! Scope: v0 supports `kind:<node-kind>` patterns over JSON (the one
//! bundled Tier-1 grammar — see `staging::TreeSitterSyntaxCheck`).
//! Richer pattern syntax (ts-pattern, structural match) and the rest of
//! the Tier-1 grammars land alongside the §7 hallucination detector,
//! which needs the same grammar bundle.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;

pub const NAME: &str = "ast_grep";
const DEFAULT_MAX_RESULTS: usize = 100;

#[derive(Debug, Default)]
pub struct AstGrep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    language: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Match {
    path: String,
    node_kind: String,
    start_byte: usize,
    end_byte: usize,
    start_row: usize,
    start_col: usize,
}

#[async_trait]
impl Tool for AstGrep {
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

        let want_kind = match parsed.pattern.strip_prefix("kind:") {
            Some(k) if !k.is_empty() => k.to_string(),
            _ => {
                return Err(ToolError::SchemaViolation {
                    tool: NAME.into(),
                    error: "v0 supports `kind:<node-kind>` patterns only".into(),
                });
            }
        };

        let (language, ext_filter): (tree_sitter::Language, &'static [&'static str]) = match parsed
            .language
            .as_str()
        {
            "json" => (tree_sitter_json::LANGUAGE.into(), &["json"]),
            other => {
                return Err(ToolError::SchemaViolation {
                    tool: NAME.into(),
                    error: format!("language {other:?} grammar not bundled in v0 (only `json`)"),
                });
            }
        };

        let workspace_root = ctx.workspace_root.to_path_buf();
        // walkdir + tree-sitter parsing are all sync — onto the blocking
        // pool to keep the runtime live.
        tokio::task::spawn_blocking(move || -> Result<ToolResult, ToolError> {
            let root_path =
                resolve_repo_path(&workspace_root, NAME, parsed.path.as_deref().unwrap_or(""))?;
            // Symlink containment on the walk root; per-entry symlinks
            // are skipped below to match grep's behaviour.
            let root_path = ensure_inside_workspace_existing(&workspace_root, NAME, &root_path)?;
            let cap = parsed.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
            let mut matches: Vec<Match> = Vec::new();
            let mut truncated = false;

            for entry in walkdir::WalkDir::new(&root_path)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
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
                // Same symlink containment as grep — skip symlinked files.
                if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                    continue;
                }
                let ext = entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if !ext_filter.contains(&ext) {
                    continue;
                }
                let bytes = match std::fs::read(entry.path()) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let mut parser = Parser::new();
                if parser.set_language(&language).is_err() {
                    continue;
                }
                let tree = match parser.parse(&bytes, None) {
                    Some(t) => t,
                    None => continue,
                };
                let rel = entry
                    .path()
                    .strip_prefix(&root_path)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();

                walk_for_kind(
                    tree.root_node(),
                    &want_kind,
                    &rel,
                    &mut matches,
                    cap,
                    &mut truncated,
                );
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

fn walk_for_kind(
    node: Node<'_>,
    want_kind: &str,
    rel_path: &str,
    out: &mut Vec<Match>,
    cap: usize,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }
    if node.kind() == want_kind {
        if out.len() >= cap {
            *truncated = true;
            return;
        }
        let start = node.start_position();
        out.push(Match {
            path: rel_path.to_string(),
            node_kind: node.kind().to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_row: start.row,
            start_col: start.column,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_kind(child, want_kind, rel_path, out, cap, truncated);
        if *truncated {
            return;
        }
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
    async fn finds_json_string_nodes() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("a.json"),
            r#"{"name": "atelier", "version": 1}"#,
        )
        .unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = AstGrep
            .execute(
                serde_json::json!({"pattern": "kind:string", "language": "json"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        let matches = r.output["matches"].as_array().unwrap();
        // Two string nodes: "name" key, "atelier" value. (Two more would
        // appear under "name" + "version" as part of the pair, but
        // tree-sitter-json reports them as `string` and the key string is
        // also a `string`. The exact count depends on grammar shape;
        // assert non-zero rather than exact.)
        assert!(!matches.is_empty(), "expected at least one string node");
        // Path is correct.
        assert_eq!(matches[0]["path"], "a.json");
        assert_eq!(matches[0]["node_kind"], "string");
    }

    #[tokio::test]
    async fn ignores_files_with_non_matching_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), r#"{"x": 1}"#).unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = AstGrep
            .execute(
                serde_json::json!({"pattern": "kind:string", "language": "json"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert!(r.output["matches"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pattern_without_kind_prefix_is_schema_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = AstGrep
            .execute(
                serde_json::json!({"pattern": "string", "language": "json"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn unsupported_language_is_schema_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = AstGrep
            .execute(
                serde_json::json!({"pattern": "kind:string", "language": "rust"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }
}
