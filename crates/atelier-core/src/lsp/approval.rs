//! `LspApprovals` — first-use approval store for the §7 Tier-1 LSP install
//! prompt. Bit-for-bit mirror of [`crate::mcp_config::McpApprovals`] (v60.8),
//! per **L-D-3** (reuse the tier/fallback approval shape, don't reinvent).
//!
//! Layout:
//!
//!   - JSON file on disk, written atomically via
//!     [`tempfile::NamedTempFile::persist`] (per **L-D-4** — every cross-call
//!     write routes through the atomic pattern).
//!   - Map of `language → approval_timestamp` (RFC 3339, stringly typed to
//!     match `OnDiskSession::created_at` and `McpApprovals`).
//!   - `is_approved` / `approve` / `revoke` are pure in-memory ops; `save`
//!     persists.
//!
//! Path convention (Q3 resolution v60.12): the store lives at
//! `<workspace>/.atelier/lsp/_approvals.json`. The `_` prefix matches the
//! §15 hooks + MCP approvals convention so a glob like `.atelier/**/manifest.json`
//! never picks up the approvals file.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::persistence::PersistenceError;

/// Directory under `<workspace>/.atelier/` that holds the per-workspace LSP
/// approval store. Distinct from `mcp_servers/` and `hooks/` so a misconfigured
/// glob can't cross-contaminate the trust surfaces.
pub const LSP_APPROVALS_DIR: &str = "lsp";

/// File name inside [`LSP_APPROVALS_DIR`]. The `_` prefix matches the §15
/// hooks + MCP approvals convention: every approvals-bearing file across
/// the harness uses `_approvals.json`.
pub const LSP_APPROVALS_FILE: &str = "_approvals.json";

/// First-use approval store. One approval per language identifier (the
/// `language` field on `Event::RequestLspInstall`). Granting trust to a
/// language grants it to that language's LSP server + any subsequent install
/// retries (e.g. `npm install -g typescript-language-server` fails network →
/// the user re-tries an hour later; the prompt does not re-fire).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LspApprovals {
    #[serde(default)]
    pub approved: BTreeMap<String, String>,
}

impl LspApprovals {
    /// Load the approvals file. Missing file → empty store (the common
    /// case on first run); malformed file → typed error.
    pub fn load(path: &Path) -> Result<Self, PersistenceError> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| PersistenceError::Deserialize {
                path: path.to_path_buf(),
                error: e.to_string(),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PersistenceError::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Atomically write the approvals file. Creates parent dirs as needed —
    /// the `<workspace>/.atelier/lsp/` directory does not exist in a fresh
    /// repo.
    pub fn save(&self, path: &Path) -> Result<(), PersistenceError> {
        let parent = path.parent().ok_or_else(|| PersistenceError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "approvals path has no parent"),
        })?;
        std::fs::create_dir_all(parent).map_err(|e| PersistenceError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| PersistenceError::Serialize(e.to_string()))?;
        let mut tmp =
            tempfile::NamedTempFile::new_in(parent).map_err(|e| PersistenceError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        io::Write::write_all(tmp.as_file_mut(), &json).map_err(|e| PersistenceError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        // v60.34 (M15) — align with the v60.29 H11 staging pattern:
        // sync the temp file's contents before persist, then fsync the
        // parent dir entry. Without these, a power loss between write
        // and the kernel's next natural flush can leave the approvals
        // file zero-length or absent, silently re-prompting the user.
        tmp.as_file().sync_all().map_err(|e| PersistenceError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.persist(path).map_err(|e| PersistenceError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
        fsync_dir_best_effort(parent);
        Ok(())
    }

    /// Mark `language` approved at `granted_at` (RFC 3339 string). Idempotent —
    /// re-approving overwrites the timestamp.
    pub fn approve(&mut self, language: impl Into<String>, granted_at: impl Into<String>) {
        self.approved.insert(language.into(), granted_at.into());
    }

    /// Drop an approval. Returns the previous timestamp if there was one.
    pub fn revoke(&mut self, language: &str) -> Option<String> {
        self.approved.remove(language)
    }

    pub fn is_approved(&self, language: &str) -> bool {
        self.approved.contains_key(language)
    }
}

/// Conventional location of the approvals store under a workspace root:
/// `<workspace>/.atelier/lsp/_approvals.json`.
pub fn lsp_approvals_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".atelier")
        .join(LSP_APPROVALS_DIR)
        .join(LSP_APPROVALS_FILE)
}

#[cfg(unix)]
fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(not(unix))]
fn fsync_dir_best_effort(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_store_round_trips_through_disk() {
        let td = TempDir::new().unwrap();
        let path = lsp_approvals_path(td.path());
        let store = LspApprovals::default();
        store.save(&path).expect("save");
        let back = LspApprovals::load(&path).expect("load");
        assert_eq!(store, back);
        assert!(path.ends_with(".atelier/lsp/_approvals.json"));
    }

    #[test]
    fn approve_then_round_trip_persists_the_timestamp() {
        let td = TempDir::new().unwrap();
        let path = lsp_approvals_path(td.path());
        let mut store = LspApprovals::default();
        store.approve("typescript", "2026-05-18T12:00:00Z");
        store.save(&path).expect("save");
        let loaded = LspApprovals::load(&path).expect("load");
        assert!(loaded.is_approved("typescript"));
        assert_eq!(
            loaded.approved.get("typescript"),
            Some(&"2026-05-18T12:00:00Z".to_string()),
        );
    }

    #[test]
    fn approve_is_idempotent_and_revoke_clears() {
        let mut store = LspApprovals::default();
        store.approve("typescript", "2026-05-18T12:00:00Z");
        // Idempotent — re-approving with a fresh timestamp overwrites.
        store.approve("typescript", "2026-05-19T08:30:00Z");
        assert_eq!(
            store.approved.get("typescript"),
            Some(&"2026-05-19T08:30:00Z".to_string()),
        );
        let prev = store.revoke("typescript");
        assert_eq!(prev, Some("2026-05-19T08:30:00Z".to_string()));
        assert!(!store.is_approved("typescript"));
    }

    #[test]
    fn load_missing_file_returns_empty_store_not_error() {
        let td = TempDir::new().unwrap();
        let path = lsp_approvals_path(td.path());
        // No save() before load — the .atelier/lsp dir doesn't even exist.
        let store = LspApprovals::load(&path).expect("missing file → empty");
        assert!(store.approved.is_empty());
    }

    #[test]
    fn load_malformed_json_returns_typed_error() {
        let td = TempDir::new().unwrap();
        let path = lsp_approvals_path(td.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not valid json").unwrap();
        let err = LspApprovals::load(&path).expect_err("malformed file → error");
        assert!(matches!(err, PersistenceError::Deserialize { .. }));
    }

    #[test]
    fn approval_path_is_under_workspace_dot_atelier_lsp() {
        let td = TempDir::new().unwrap();
        let p = lsp_approvals_path(td.path());
        let suffix = format!(
            "{}/{}/{}",
            ".atelier", LSP_APPROVALS_DIR, LSP_APPROVALS_FILE
        );
        assert!(p.ends_with(&suffix), "expected suffix {suffix:?} on {p:?}");
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;
    use tempfile::TempDir;

    // v60.34 (M15) — `save` must either leave no file or a fully written
    // one. The atomic-write pattern (write → sync_all → persist →
    // fsync_dir) means after `save` returns, the on-disk bytes parse
    // back to the same struct. There is no in-between "half-written"
    // state that a subsequent `load` would observe.
    #[test]
    fn save_is_atomic_or_absent() {
        let td = TempDir::new().unwrap();
        let path = lsp_approvals_path(td.path());

        // Pre-state: no file. `load` returns the empty default.
        let pre = LspApprovals::load(&path).expect("missing file → empty");
        assert!(pre.approved.is_empty());

        let mut store = LspApprovals::default();
        store.approve("typescript", "2026-05-19T10:00:00Z");
        store.save(&path).expect("save");

        // Post-state: file exists, parses, and round-trips. The fsync_dir
        // call means after `save` returns, the directory entry update is
        // durable; a power loss after this point cannot revert the file.
        let metadata = std::fs::metadata(&path).expect("file present");
        assert!(metadata.len() > 0, "approvals file is zero-length");
        let back = LspApprovals::load(&path).expect("load");
        assert_eq!(back, store);
    }

    #[test]
    fn save_overwrite_is_atomic_or_absent() {
        // A second save must replace the file atomically — the persist
        // step rebinds the directory entry, never producing a partial
        // file on the path.
        let td = TempDir::new().unwrap();
        let path = lsp_approvals_path(td.path());

        let mut store = LspApprovals::default();
        store.approve("typescript", "2026-05-19T10:00:00Z");
        store.save(&path).expect("save 1");

        let original_len = std::fs::metadata(&path).unwrap().len();
        store.approve("python", "2026-05-19T11:00:00Z");
        store.save(&path).expect("save 2");

        let new_len = std::fs::metadata(&path).unwrap().len();
        assert!(new_len > original_len, "file should have grown");
        let back = LspApprovals::load(&path).expect("load");
        assert_eq!(back, store);
        assert!(back.is_approved("typescript"));
        assert!(back.is_approved("python"));
    }
}
