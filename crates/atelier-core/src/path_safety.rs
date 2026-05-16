//! Repo-relative path validation + symlink containment.
//!
//! Spec §11 "Policy":
//!   > Default: repo-scoped FS, no network egress, no writes to `/etc` or
//!   > `/usr/local`. Out-of-repo reads require approval.
//!
//! The harness's own file I/O (read_file / list_dir / grep / edit_file /
//! write_file / ast_grep, and §3 staging) does **not** flow through the §11
//! sandbox profile generator — that profile only wraps shelled-out
//! subprocesses. So the symlink-escape attack ("repo contains `link.txt`
//! pointing to /etc/passwd; tool happily reads through the link") has to
//! be blocked in-process by every consumer.
//!
//! This module is that blocker. Three helpers:
//!
//!   * [`resolve_repo_path`] — convert a model-emitted repo-relative path
//!     into the absolute path inside `workspace_root`. Rejects absolute
//!     paths and `..` components at the syntax level.
//!   * [`ensure_inside_workspace_existing`] — canonicalize a path that
//!     already exists on disk and assert it falls inside the canonicalized
//!     workspace root. Catches the symlink escape.
//!   * [`ensure_inside_workspace_creatable`] — same, for a path that
//!     doesn't exist yet (write creating). Canonicalizes the parent
//!     directory and joins the basename.

use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

/// Convert a model-emitted repo-relative path into the absolute path inside
/// `workspace_root`. **Syntax-only** check — rejects absolute paths and
/// `..` components. For containment against symlinks, follow up with
/// [`ensure_inside_workspace_existing`] / [`ensure_inside_workspace_creatable`].
pub fn resolve_repo_path(
    workspace_root: &Path,
    tool: &str,
    rel: &str,
) -> Result<PathBuf, ToolError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ToolError::SchemaViolation {
            tool: tool.to_string(),
            error: format!("`path` must be repo-relative; got absolute {rel:?}"),
        });
    }
    if rel_path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(ToolError::PermissionDenied {
            tool: tool.to_string(),
            reason: format!("`path` {rel:?} contains `..` and would escape the workspace"),
        });
    }
    // Empty path means "the workspace root" — list_dir uses this.
    Ok(workspace_root.join(rel_path))
}

/// Canonicalize `abs_path` (which must exist) and assert it falls inside
/// the canonicalized `workspace_root`. Defense against symlink escape:
/// `resolve_repo_path` only rejects literal `..`; a repo-relative path
/// that hits a symlink-to-outside passes that check but fails this one.
///
/// Returns the canonical path on success.
pub fn ensure_inside_workspace_existing(
    workspace_root: &Path,
    tool: &str,
    abs_path: &Path,
) -> Result<PathBuf, ToolError> {
    let canonical_root = canonicalize_root(workspace_root, tool)?;
    let canonical_target =
        std::fs::canonicalize(abs_path).map_err(|e| ToolError::ExecutionFailed {
            tool: tool.to_string(),
            exit_code: -1,
            stderr: format!("canonicalize {abs_path:?} failed: {e}"),
        })?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(ToolError::PermissionDenied {
            tool: tool.to_string(),
            reason: format!(
                "{abs_path:?} resolves to {canonical_target:?} which is outside the workspace {canonical_root:?} (symlink escape?)"
            ),
        });
    }
    Ok(canonical_target)
}

/// Like [`ensure_inside_workspace_existing`] but for a path that doesn't
/// exist yet (e.g., a new file being created). Canonicalizes the **parent**
/// directory (which must exist) and joins the basename. Used by
/// `write_file` and `Staging::commit`'s create path.
pub fn ensure_inside_workspace_creatable(
    workspace_root: &Path,
    tool: &str,
    abs_path: &Path,
) -> Result<PathBuf, ToolError> {
    let parent = abs_path
        .parent()
        .ok_or_else(|| ToolError::SchemaViolation {
            tool: tool.to_string(),
            error: format!("{abs_path:?} has no parent directory"),
        })?;
    let basename = abs_path
        .file_name()
        .ok_or_else(|| ToolError::SchemaViolation {
            tool: tool.to_string(),
            error: format!("{abs_path:?} has no file name"),
        })?;
    // The parent must exist (caller has either mkdir'd it or it was already
    // there). If it doesn't, the write itself would fail with a confusing
    // I/O error; we'd rather surface that here.
    let canonical_parent = ensure_inside_workspace_existing(workspace_root, tool, parent)?;
    Ok(canonical_parent.join(basename))
}

fn canonicalize_root(workspace_root: &Path, tool: &str) -> Result<PathBuf, ToolError> {
    std::fs::canonicalize(workspace_root).map_err(|e| ToolError::ExecutionFailed {
        tool: tool.to_string(),
        exit_code: -1,
        stderr: format!("canonicalize workspace root {workspace_root:?} failed: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- resolve_repo_path (syntax-level) ----

    #[test]
    fn resolve_accepts_repo_relative_path() {
        let p = resolve_repo_path(Path::new("/repo"), "t", "src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/repo/src/main.rs"));
    }

    #[test]
    fn resolve_accepts_empty_path_as_workspace_root() {
        let p = resolve_repo_path(Path::new("/repo"), "t", "").unwrap();
        assert_eq!(p, PathBuf::from("/repo"));
    }

    #[test]
    fn resolve_rejects_absolute_path() {
        let err = resolve_repo_path(Path::new("/repo"), "t", "/etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[test]
    fn resolve_rejects_parent_dir_escape() {
        let err = resolve_repo_path(Path::new("/repo"), "t", "../outside").unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[test]
    fn resolve_rejects_embedded_parent_dir() {
        let err = resolve_repo_path(Path::new("/repo"), "t", "src/../../etc").unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    // ---- ensure_inside_workspace_existing (symlink containment) ----

    #[test]
    fn existing_accepts_real_file_inside_workspace() {
        let ws = tempfile::TempDir::new().unwrap();
        let target = ws.path().join("a.txt");
        std::fs::write(&target, b"x").unwrap();
        let canonical = ensure_inside_workspace_existing(ws.path(), "t", &target).unwrap();
        // Both sides canonicalize (macOS /tmp → /private/tmp) so the
        // prefix check must still hold.
        let canonical_ws = std::fs::canonicalize(ws.path()).unwrap();
        assert!(canonical.starts_with(&canonical_ws));
    }

    #[cfg(unix)]
    #[test]
    fn existing_rejects_symlink_pointing_outside_workspace() {
        let ws = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, b"sensitive").unwrap();

        let link = ws.path().join("looks_inside.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let err = ensure_inside_workspace_existing(ws.path(), "t", &link).unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { ref reason, .. } if reason.contains("symlink escape"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_accepts_symlink_pointing_inside_workspace() {
        let ws = tempfile::TempDir::new().unwrap();
        let real = ws.path().join("real.txt");
        std::fs::write(&real, b"x").unwrap();
        let link = ws.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        ensure_inside_workspace_existing(ws.path(), "t", &link).unwrap();
    }

    #[test]
    fn existing_rejects_missing_file() {
        let ws = tempfile::TempDir::new().unwrap();
        let err =
            ensure_inside_workspace_existing(ws.path(), "t", &ws.path().join("ghost")).unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }

    // ---- ensure_inside_workspace_creatable ----

    #[test]
    fn creatable_accepts_new_file_with_existing_parent_inside_workspace() {
        let ws = tempfile::TempDir::new().unwrap();
        let path = ws.path().join("brand-new.txt");
        let result = ensure_inside_workspace_creatable(ws.path(), "t", &path).unwrap();
        let canonical_ws = std::fs::canonicalize(ws.path()).unwrap();
        assert!(result.starts_with(&canonical_ws));
        assert_eq!(result.file_name().unwrap(), "brand-new.txt");
    }

    #[cfg(unix)]
    #[test]
    fn creatable_rejects_new_file_under_symlinked_parent() {
        let ws = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let link_dir = ws.path().join("escape_via_dir");
        std::os::unix::fs::symlink(outside.path(), &link_dir).unwrap();

        let target = link_dir.join("new.txt");
        let err = ensure_inside_workspace_creatable(ws.path(), "t", &target).unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    #[test]
    fn creatable_rejects_when_parent_missing() {
        let ws = tempfile::TempDir::new().unwrap();
        let path = ws.path().join("ghost-dir").join("file.txt");
        let err = ensure_inside_workspace_creatable(ws.path(), "t", &path).unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }
}
