//! §14 on-disk session + crash-recovery scaffold.
//!
//! Spec §14 "On-disk storage":
//!   * Per-repo: `<repo>/.atelier/sessions/<session-uuid>/session.json`
//!     plus `<repo>/.atelier/sessions/<session-uuid>/diffs/<sha256>.diff`.
//!   * Global registry: `~/.atelier/registry.json` — `{ uuid → repo_path,
//!     last_touched }`. Rebuilt opportunistically; safe to delete.
//!   * Diff blobs are content-addressed.
//!
//! Spec §14 "Mid-turn crash":
//!   On restart, harness resumes at the last completed tool call. **Partial
//!   output is preserved in a dedicated `recovery_log` slot, not in
//!   conversation history.**
//!
//! ## Scope of this scaffold
//!
//! This module covers the *data layer*:
//!
//!   * Typed [`OnDiskSession`] matching `schemas/session/v1.json`.
//!   * Atomic save (`tempfile` + rename) under
//!     `.atelier/sessions/<uuid>/session.json`.
//!   * Load with schema-shape validation (catches version skew up front).
//!   * `recovery_log` mutator with the four allowed reasons from the schema.
//!   * Global [`Registry`] read/write/touch helpers.
//!
//! Deferred to follow-on commits (each is its own §14 sub-item in
//! `tasks/todo.md`):
//!
//!   * File-watcher integration (`notify` crate) — needs the actor's
//!     read-set tracking, which lands with the tool dispatcher.
//!   * Concurrent-edit modal flow — UX surface; queues at tool-call
//!     boundary.
//!   * Diff-blob storage (`diffs/<sha256>.diff[.zst]` / `.full[.zst]`) —
//!     bundled with §4 checkpoint storage.
//!   * Resume-at-last-completed-tool-call traversal — needs typed
//!     conversation / tool-fixture entries, which arrive with the BYOM
//!     adapter.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ledger::LedgerEntry;
use crate::memory::MemoryCard;
use crate::plan::PlanStep;

/// Schema version of the on-disk session (`harness_session_version` in
/// `schemas/session/v1.json`). Bumps require a one-way migration tool per
/// `schemas/versions.md`.
pub const HARNESS_SESSION_VERSION: u32 = 1;

/// Filename within a session directory.
pub const SESSION_FILE: &str = "session.json";

/// Sub-directory under a session for diff blobs (§14 diff format). Created
/// lazily by the §4 checkpoint store; present here so the path layout is
/// declared in one place.
pub const DIFFS_SUBDIR: &str = "diffs";

/// Top-level on-disk session document. Field set mirrors
/// `schemas/session/v1.json` `required` keys plus the optional `subagents`
/// (added by §10.1 delegation). Nested types stay as `serde_json::Value` for
/// now; typed structs land as the producing subsystems (adapter, ledger,
/// checkpoint store) come online — keeping them untyped here avoids dragging
/// half-finished schemas into the persistence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnDiskSession {
    pub session_uuid: Uuid,
    pub harness_session_version: u32,
    pub atelier_version: String,
    pub created_at: String,

    pub conversation: Vec<serde_json::Value>,
    /// Typed in v31 (was `Vec<serde_json::Value>`). Round-trips the schema's
    /// `cost_ledger[]` shape via [`LedgerEntry`]; the schema's per-kind
    /// `allOf/if/then` required-field rules are enforced by the type itself,
    /// so a malformed entry can't be constructed in Rust.
    pub cost_ledger: Vec<LedgerEntry>,
    pub checkpoints: Checkpoints,
    pub tool_fixtures: BTreeMap<String, serde_json::Value>,
    /// Typed in v29 (was `Vec<serde_json::Value>`). Round-trips the schema's
    /// `memory[]` shape via [`MemoryCard`]; existing on-disk sessions
    /// deserialise unchanged because the schema and the type match exactly.
    pub memory: Vec<MemoryCard>,
    pub plan: Plan,
    pub constraints: Vec<serde_json::Value>,
    pub recovery_log: Vec<RecoveryEntry>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents: Option<serde_json::Value>,
}

/// Diff-based checkpoint tree (spec §4). Root id + map of nodes. Concrete
/// node typing lands with §4 / §14 diff-blob storage; for now nodes are
/// untyped JSON so an `OnDiskSession` instantiated today round-trips a
/// future-richer document without losing fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoints {
    pub root: String,
    pub nodes: BTreeMap<String, serde_json::Value>,
}

/// Plan canvas state (spec §5). Typed in v29 (was `Vec<serde_json::Value>`);
/// the on-disk shape is unchanged since [`PlanStep`] mirrors the schema's
/// `plan.steps[]` items exactly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
}

/// One entry in the `recovery_log` slot. Mirrors the schema's required
/// fields; `reason` is the closed enum the schema permits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEntry {
    pub turn_id: String,
    pub partial_content: String,
    pub captured_at: String,
    pub reason: RecoveryReason,
}

/// Closed enum matching the `reason` schema enum exactly. `serde` renames to
/// snake_case so the JSON values match (`crash`, `user_cancel`, `timeout`,
/// `concurrent_edit_pause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReason {
    Crash,
    UserCancel,
    Timeout,
    ConcurrentEditPause,
}

impl OnDiskSession {
    /// Build a fresh minimal session document. Validates against
    /// `schemas/session/v1.json` (exercised by the rig in `make check`).
    pub fn fresh(
        uuid: Uuid,
        atelier_version: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            session_uuid: uuid,
            harness_session_version: HARNESS_SESSION_VERSION,
            atelier_version: atelier_version.into(),
            created_at: created_at.into(),
            conversation: Vec::new(),
            cost_ledger: Vec::new(),
            checkpoints: Checkpoints {
                root: "root".into(),
                nodes: BTreeMap::new(),
            },
            tool_fixtures: BTreeMap::new(),
            memory: Vec::new(),
            plan: Plan::default(),
            constraints: Vec::new(),
            recovery_log: Vec::new(),
            subagents: None,
        }
    }

    /// Canonical on-disk directory for a session given a repo root.
    pub fn session_dir(repo_root: &Path, uuid: Uuid) -> PathBuf {
        repo_root
            .join(".atelier")
            .join("sessions")
            .join(uuid.to_string())
    }

    /// Append to `recovery_log`. Spec §14 — partial output captured here is
    /// **not** added to conversation history; the next turn's model never
    /// sees it as a "completed" action.
    pub fn append_recovery(&mut self, entry: RecoveryEntry) {
        self.recovery_log.push(entry);
    }

    /// Atomic write: serialize to a temp file in the same directory, then
    /// rename over `session.json`. Same-filesystem rename is atomic on POSIX
    /// and avoids the partial-write corruption mode that plagues naive
    /// persistence layers.
    pub fn save_to(&self, dir: &Path) -> Result<PathBuf, PersistenceError> {
        std::fs::create_dir_all(dir).map_err(|e| PersistenceError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| PersistenceError::Serialize(e.to_string()))?;
        let target = dir.join(SESSION_FILE);

        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| PersistenceError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        io::Write::write_all(tmp.as_file_mut(), &json).map_err(|e| PersistenceError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.as_file().sync_all().map_err(|e| PersistenceError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.persist(&target).map_err(|e| PersistenceError::Io {
            path: target.clone(),
            source: e.error,
        })?;
        // POSIX rename is atomic for content but the directory entry's
        // durability requires fsync of the parent directory. Without
        // this, a power loss after `persist` returns can leave the
        // directory in its pre-rename state on disk — fatal for §14
        // crash-recovery. Linux/macOS support fd-on-dir + sync_all;
        // Windows isn't a v1 target.
        fsync_dir(dir)?;
        Ok(target)
    }

    /// Load and deserialize. Rejects sessions whose
    /// `harness_session_version` differs from [`HARNESS_SESSION_VERSION`] —
    /// per spec §14 those need a one-way migration.
    pub fn load_from(dir: &Path) -> Result<Self, PersistenceError> {
        let path = dir.join(SESSION_FILE);
        let bytes = std::fs::read(&path).map_err(|e| PersistenceError::Io {
            path: path.clone(),
            source: e,
        })?;
        let session: Self =
            serde_json::from_slice(&bytes).map_err(|e| PersistenceError::Deserialize {
                path: path.clone(),
                error: e.to_string(),
            })?;
        if session.harness_session_version != HARNESS_SESSION_VERSION {
            return Err(PersistenceError::IncompatibleVersion {
                path,
                got: session.harness_session_version,
                expected: HARNESS_SESSION_VERSION,
            });
        }
        Ok(session)
    }
}

/// Persistence-layer errors. Mapped onto `ToolError` variants by callers
/// when surfacing to the agent loop.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("I/O failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("serialize failure: {0}")]
    Serialize(String),

    #[error("deserialize failure at {path}: {error}")]
    Deserialize { path: PathBuf, error: String },

    #[error(
        "session at {path} uses harness_session_version {got}, this build expects {expected}; run the migration tool"
    )]
    IncompatibleVersion {
        path: PathBuf,
        got: u32,
        expected: u32,
    },
}

/// Global session registry per spec §14 ("Global registry":
/// `~/.atelier/registry.json` — a small index mapping session UUID → repo
/// path + last-touched timestamp). Rebuildable; safe to delete.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub entries: BTreeMap<Uuid, RegistryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub repo_path: PathBuf,
    pub last_touched: String,
}

impl Registry {
    /// Default registry path under the user's home — `~/.atelier/registry.json`.
    /// Returns `None` when home cannot be resolved (very minimal CI envs).
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".atelier").join("registry.json"))
    }

    /// Load or return empty. Per spec §14: "Rebuilt opportunistically; safe
    /// to delete." A missing file is not an error.
    pub fn load(path: &Path) -> Result<Self, PersistenceError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| PersistenceError::Deserialize {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PersistenceError::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Atomic write.
    pub fn save(&self, path: &Path) -> Result<(), PersistenceError> {
        let parent = path.parent().ok_or_else(|| PersistenceError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "registry path has no parent"),
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
        // Durability: see `OnDiskSession::save_to` rationale.
        fsync_dir(parent)?;
        Ok(())
    }

    /// Record (or refresh) an entry. Use this from the session actor after
    /// any save so the registry index stays warm.
    pub fn touch(&mut self, uuid: Uuid, repo_path: PathBuf, last_touched: String) {
        self.entries.insert(
            uuid,
            RegistryEntry {
                repo_path,
                last_touched,
            },
        );
    }

    pub fn forget(&mut self, uuid: &Uuid) -> Option<RegistryEntry> {
        self.entries.remove(uuid)
    }
}

/// fsync the directory entry after an atomic rename so the rename is
/// durable across power loss. POSIX guarantees `rename` is atomic for
/// content, but the *directory entry's* update is buffered until the
/// next fs sync — without this call, a crash between `tmp.persist()`
/// and the next natural fsync can leave the directory in its
/// pre-rename state on stable storage. v1 targets unix only (Windows
/// not supported per spec §11), so we cfg-gate the impl.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<(), PersistenceError> {
    let f = std::fs::File::open(dir).map_err(|e| PersistenceError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    f.sync_all().map_err(|e| PersistenceError::Io {
        path: dir.to_path_buf(),
        source: e,
    })
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<(), PersistenceError> {
    // Windows + others: opening a directory as a file for fsync is not
    // a thing. v1 doesn't target them. Returning Ok here is honest —
    // we made no durability promise on these platforms.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn uuid_for(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = n;
        // Force v4 + variant 1 bits so it serialises as a valid UUID string.
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Uuid::from_bytes(b)
    }

    #[test]
    fn fresh_session_has_required_fields_and_correct_version() {
        let s = OnDiskSession::fresh(uuid_for(1), "0.0.0", "2026-05-16T10:00:00Z");
        assert_eq!(s.harness_session_version, HARNESS_SESSION_VERSION);
        assert_eq!(s.atelier_version, "0.0.0");
        assert_eq!(s.created_at, "2026-05-16T10:00:00Z");
        assert!(s.conversation.is_empty());
        assert!(s.recovery_log.is_empty());
        assert_eq!(s.checkpoints.root, "root");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let s = OnDiskSession::fresh(uuid_for(2), "0.0.0", "2026-05-16T10:00:00Z");
        let path = s.save_to(dir.path()).unwrap();
        assert_eq!(path, dir.path().join(SESSION_FILE));
        let loaded = OnDiskSession::load_from(dir.path()).unwrap();
        assert_eq!(loaded.session_uuid, s.session_uuid);
        assert_eq!(loaded.atelier_version, s.atelier_version);
        assert_eq!(loaded.created_at, s.created_at);
    }

    #[test]
    fn session_dir_layout_matches_spec() {
        let p = OnDiskSession::session_dir(Path::new("/repo"), uuid_for(7));
        assert!(p.starts_with("/repo/.atelier/sessions/"));
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            uuid_for(7).to_string()
        );
    }

    #[test]
    fn append_recovery_grows_log_and_round_trips() {
        let dir = TempDir::new().unwrap();
        let mut s = OnDiskSession::fresh(uuid_for(3), "0.0.0", "2026-05-16T10:00:00Z");
        s.append_recovery(RecoveryEntry {
            turn_id: "turn-7".into(),
            partial_content: "the model was streaming when the process died".into(),
            captured_at: "2026-05-16T10:00:42Z".into(),
            reason: RecoveryReason::Crash,
        });
        s.append_recovery(RecoveryEntry {
            turn_id: "turn-8".into(),
            partial_content: "user hit ctrl-c".into(),
            captured_at: "2026-05-16T10:01:00Z".into(),
            reason: RecoveryReason::UserCancel,
        });
        s.save_to(dir.path()).unwrap();
        let loaded = OnDiskSession::load_from(dir.path()).unwrap();
        assert_eq!(loaded.recovery_log.len(), 2);
        assert_eq!(loaded.recovery_log[0].reason, RecoveryReason::Crash);
        assert_eq!(loaded.recovery_log[1].reason, RecoveryReason::UserCancel);
    }

    #[test]
    fn recovery_reasons_serialise_as_schema_snake_case() {
        let json = serde_json::to_string(&RecoveryReason::ConcurrentEditPause).unwrap();
        assert_eq!(json, "\"concurrent_edit_pause\"");
        let back: RecoveryReason = serde_json::from_str("\"timeout\"").unwrap();
        assert_eq!(back, RecoveryReason::Timeout);
    }

    #[test]
    fn load_rejects_incompatible_session_version() {
        let dir = TempDir::new().unwrap();
        let mut raw = serde_json::json!({
            "session_uuid": uuid_for(4).to_string(),
            "harness_session_version": 999,
            "atelier_version": "0.0.0",
            "created_at": "2026-05-16T10:00:00Z",
            "conversation": [],
            "cost_ledger": [],
            "checkpoints": {"root": "root", "nodes": {}},
            "tool_fixtures": {},
            "memory": [],
            "plan": {"steps": []},
            "constraints": [],
            "recovery_log": []
        });
        raw["harness_session_version"] = serde_json::json!(999);
        std::fs::write(dir.path().join(SESSION_FILE), raw.to_string()).unwrap();
        let err = OnDiskSession::load_from(dir.path()).unwrap_err();
        match err {
            PersistenceError::IncompatibleVersion { got, expected, .. } => {
                assert_eq!(got, 999);
                assert_eq!(expected, HARNESS_SESSION_VERSION);
            }
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_file_is_an_io_error() {
        let dir = TempDir::new().unwrap();
        let err = OnDiskSession::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, PersistenceError::Io { .. }));
    }

    #[test]
    fn save_is_atomic_no_partial_files_on_failure_via_temp_rename() {
        // Saving twice must overwrite rather than leave a half-written file.
        let dir = TempDir::new().unwrap();
        let s1 = OnDiskSession::fresh(uuid_for(5), "0.0.0", "2026-05-16T10:00:00Z");
        s1.save_to(dir.path()).unwrap();
        let mut s2 = s1.clone();
        s2.atelier_version = "0.0.1".into();
        s2.save_to(dir.path()).unwrap();
        let loaded = OnDiskSession::load_from(dir.path()).unwrap();
        assert_eq!(loaded.atelier_version, "0.0.1");
        // No leftover NamedTempFile siblings (NamedTempFile cleans up on
        // successful persist + drop).
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != SESSION_FILE)
            .collect();
        assert!(leftovers.is_empty(), "stray files: {leftovers:?}");
    }

    // ---------- registry ----------

    #[test]
    fn registry_load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let reg = Registry::load(&dir.path().join("registry.json")).unwrap();
        assert!(reg.entries.is_empty());
    }

    #[test]
    fn registry_touch_save_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.json");

        let mut reg = Registry::default();
        reg.touch(
            uuid_for(10),
            PathBuf::from("/repo/one"),
            "2026-05-16T10:00:00Z".into(),
        );
        reg.touch(
            uuid_for(11),
            PathBuf::from("/repo/two"),
            "2026-05-16T11:00:00Z".into(),
        );
        reg.save(&path).unwrap();

        let loaded = Registry::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(
            loaded.entries.get(&uuid_for(10)).unwrap().repo_path,
            PathBuf::from("/repo/one")
        );
        assert_eq!(
            loaded.entries.get(&uuid_for(11)).unwrap().last_touched,
            "2026-05-16T11:00:00Z"
        );
    }

    #[test]
    fn registry_touch_replaces_existing_entry() {
        let mut reg = Registry::default();
        reg.touch(
            uuid_for(12),
            PathBuf::from("/old"),
            "2026-05-16T10:00:00Z".into(),
        );
        reg.touch(
            uuid_for(12),
            PathBuf::from("/new"),
            "2026-05-16T12:00:00Z".into(),
        );
        let entry = reg.entries.get(&uuid_for(12)).unwrap();
        assert_eq!(entry.repo_path, PathBuf::from("/new"));
        assert_eq!(entry.last_touched, "2026-05-16T12:00:00Z");
    }

    #[test]
    fn registry_forget_removes_entry_returns_it() {
        let mut reg = Registry::default();
        reg.touch(
            uuid_for(13),
            PathBuf::from("/x"),
            "2026-05-16T10:00:00Z".into(),
        );
        let removed = reg.forget(&uuid_for(13)).unwrap();
        assert_eq!(removed.repo_path, PathBuf::from("/x"));
        assert!(reg.entries.is_empty());
        assert!(reg.forget(&uuid_for(13)).is_none());
    }
}
