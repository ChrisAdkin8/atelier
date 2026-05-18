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
        tmp.persist(path).map_err(|e| PersistenceError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
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
