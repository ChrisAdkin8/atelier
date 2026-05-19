//! v60.5 — hardened reader/writer for §5 compaction blobs.
//!
//! The §5 non-destructive compaction flow evicts a set of `ContextItem`s
//! from `ContextManager` and replaces them with one summary `MemoryCard`.
//! The *originals* are persisted to disk so the v60.6 Expand affordance
//! can replay them back into context without re-running the adapter.
//!
//! Path layout (relative to the workspace root):
//!
//! ```text
//! .atelier/sessions/<session_id>/compactions/<blob_id>.json
//! ```
//!
//! where `session_id` is the session UUID (string-typed at the API
//! boundary — we validate the shape rather than parse), and `blob_id`
//! is a fresh UUID prefixed with `comp-` (assigned by [`write`]).
//!
//! Mirrors the hardening discipline in
//! [`crate::memory_promote::write_promoted_card`]:
//!
//! * **Workspace hygiene** — non-empty absolute path, canonicalises
//!   different from `/`.
//! * **Session-id hygiene** — non-empty, only hyphens + ASCII
//!   hex (`[0-9a-fA-F-]`), no path separators / leading dot /
//!   control bytes.
//! * **Blob-id hygiene** (read path) — extracted from the
//!   caller-supplied relative path; validated as `comp-<hex>` with
//!   no path separators / dotty bits / control bytes.
//! * **Size cap** — payload (serialised JSON) must not exceed
//!   [`MAX_COMPACTION_BLOB_BYTES`].
//! * **Canonical-root containment** — read path canonicalises the
//!   target's parent and verifies it sits under the session's
//!   compactions directory; write path does the same after creating
//!   the directory.
//! * **Atomic write** — bytes go through
//!   [`tempfile::NamedTempFile::persist`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use atelier_core::context::ContextItem;
use serde::{Deserialize, Serialize};

/// Per-call cap on the serialised JSON payload. 4 MiB is generous —
/// 100 context items at 20 KiB each — but bounded; a payload larger
/// than this is almost certainly a runaway encoding bug rather than
/// legitimate user data.
pub const MAX_COMPACTION_BLOB_BYTES: usize = 4 * 1024 * 1024;

/// Outcome of a successful [`write`]. `relative_path` is the same
/// string that gets persisted in the summary card's `compacted_from`
/// (so the v60.6 Expand path can resolve it without re-deriving the
/// blob layout); `path` is the absolute path the bytes landed at, for
/// the caller's "wrote N bytes to /path/foo.json" UX toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenBlob {
    pub blob_id: String,
    pub relative_path: String,
    pub path: PathBuf,
    pub bytes: usize,
}

/// On-disk envelope. Exposed for tests and v60.6's read path; the
/// production write path constructs it internally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionBlob {
    /// Schema version. Bumped when the on-disk shape changes; v60.5
    /// ships `1`.
    pub version: u32,
    /// `comp-<uuid>` — matches `WrittenBlob.blob_id` and the
    /// `summary_card.compacted_from.expansion_blob_path` last segment.
    pub blob_id: String,
    /// RFC 3339 — when the compaction ran.
    pub compacted_at: String,
    /// Original context items in the order they were evicted.
    pub items: Vec<ContextItem>,
}

/// On-disk schema version stamped into [`CompactionBlob::version`].
pub const COMPACTION_BLOB_VERSION: u32 = 1;

/// Persist a compaction blob under
/// `<workspace_root>/.atelier/sessions/<session_id>/compactions/`.
/// Generates a fresh `comp-<uuid>` blob id and returns it (alongside
/// the relative + absolute paths) so the caller can stash the
/// relative form into the resulting summary card's `compacted_from`
/// link.
pub fn write(
    workspace_root: &Path,
    session_id: &str,
    compacted_at: &str,
    items: &[ContextItem],
) -> Result<WrittenBlob, String> {
    // ---- session_id hygiene ----
    validate_session_id(session_id)?;

    // ---- workspace_root hygiene ----
    let canonical_workspace = canonicalize_workspace(workspace_root)?;

    // ---- compose target directory + blob id ----
    let blob_id = format!("comp-{}", uuid::Uuid::new_v4());
    let compactions_dir = canonical_workspace
        .join(".atelier")
        .join("sessions")
        .join(session_id)
        .join("compactions");
    std::fs::create_dir_all(&compactions_dir)
        .map_err(|e| format!("compaction_blob::write: mkdir {compactions_dir:?}: {e}"))?;
    let canonical_dir = std::fs::canonicalize(&compactions_dir)
        .map_err(|e| format!("compaction_blob::write: canonicalize {compactions_dir:?}: {e}"))?;

    // Containment: the canonical compactions dir must sit under the
    // canonical workspace. (Defensive — `create_dir_all` of an
    // attacker-controlled session_id can't escape past
    // validate_session_id, but we still check after canonicalisation
    // in case the workspace contains a symlink.)
    if !canonical_dir.starts_with(&canonical_workspace) {
        return Err(format!(
            "compaction_blob::write: target {canonical_dir:?} escapes workspace {canonical_workspace:?}"
        ));
    }

    let target = canonical_dir.join(format!("{blob_id}.json"));
    let relative_path = format!(".atelier/sessions/{session_id}/compactions/{blob_id}.json");

    // ---- serialise + size cap ----
    let envelope = CompactionBlob {
        version: COMPACTION_BLOB_VERSION,
        blob_id: blob_id.clone(),
        compacted_at: compacted_at.to_string(),
        items: items.to_vec(),
    };
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|e| format!("compaction_blob::write: serialize: {e}"))?;
    if bytes.len() > MAX_COMPACTION_BLOB_BYTES {
        return Err(format!(
            "compaction_blob::write: {} bytes exceeds {MAX_COMPACTION_BLOB_BYTES}",
            bytes.len()
        ));
    }

    // ---- atomic write ----
    // v60.37 A1 — full discipline: data + metadata fsync, then atomic
    // rename, then parent-dir fsync. Earlier versions stopped at persist,
    // leaving a window where a power loss could leave the directory entry
    // in its pre-rename state on stable storage.
    let mut tmp = tempfile::NamedTempFile::new_in(&canonical_dir)
        .map_err(|e| format!("compaction_blob::write: temp in {canonical_dir:?}: {e}"))?;
    tmp.write_all(&bytes)
        .map_err(|e| format!("compaction_blob::write: write temp: {e}"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| format!("compaction_blob::write: sync_all temp: {e}"))?;
    tmp.persist(&target)
        .map_err(|e| format!("compaction_blob::write: persist {target:?}: {e}"))?;
    atelier_core::path_safety::fsync_dir(&canonical_dir)
        .map_err(|e| format!("compaction_blob::write: fsync_dir {canonical_dir:?}: {e}"))?;

    Ok(WrittenBlob {
        blob_id,
        relative_path,
        path: target,
        bytes: bytes.len(),
    })
}

/// Read a previously-written compaction blob. Consumed by v60.6
/// Expand. Shipped in v60.5 so the integration tests prove round-trip
/// fidelity (and so that v60.6 ships as the consumer flip, not a new
/// read path).
///
/// The `relative_path` is the same string [`write`] stored in the
/// summary card's `compacted_from.expansion_blob_path`. We re-derive
/// the absolute path under `workspace_root`, canonicalise the parent,
/// and verify containment before reading.
pub fn read(workspace_root: &Path, relative_path: &str) -> Result<CompactionBlob, String> {
    // ---- relative_path hygiene ----
    if relative_path.is_empty()
        || relative_path.starts_with('/')
        || relative_path.contains("..")
        || relative_path.chars().any(|c| (c as u32) < 0x20)
    {
        return Err(format!(
            "compaction_blob::read: invalid relative_path {relative_path:?}"
        ));
    }
    let expected_prefix = ".atelier/sessions/";
    if !relative_path.starts_with(expected_prefix) {
        return Err(format!(
            "compaction_blob::read: {relative_path:?} not under {expected_prefix}"
        ));
    }
    if !relative_path.ends_with(".json") {
        return Err(format!(
            "compaction_blob::read: {relative_path:?} is not a .json file"
        ));
    }

    // ---- workspace_root hygiene ----
    let canonical_workspace = canonicalize_workspace(workspace_root)?;
    let target = canonical_workspace.join(relative_path);
    let canonical_target_parent = std::fs::canonicalize(
        target
            .parent()
            .ok_or_else(|| format!("compaction_blob::read: {target:?} has no parent"))?,
    )
    .map_err(|e| format!("compaction_blob::read: canonicalize parent of {target:?}: {e}"))?;
    if !canonical_target_parent.starts_with(&canonical_workspace) {
        return Err(format!(
            "compaction_blob::read: {canonical_target_parent:?} escapes workspace {canonical_workspace:?}"
        ));
    }

    // ---- read (capped) + parse ----
    let f = std::fs::File::open(&target)
        .map_err(|e| format!("compaction_blob::read: open {target:?}: {e}"))?;
    let mut buf = Vec::new();
    f.take((MAX_COMPACTION_BLOB_BYTES as u64) + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("compaction_blob::read: read {target:?}: {e}"))?;
    if buf.len() > MAX_COMPACTION_BLOB_BYTES {
        return Err(format!(
            "compaction_blob::read: {target:?} exceeds {MAX_COMPACTION_BLOB_BYTES}"
        ));
    }
    let envelope: CompactionBlob = serde_json::from_slice(&buf)
        .map_err(|e| format!("compaction_blob::read: parse {target:?}: {e}"))?;
    Ok(envelope)
}

fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.starts_with('.')
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id
            .chars()
            .any(|c| (c as u32) < 0x20 || !(c.is_ascii_hexdigit() || c == '-'))
    {
        return Err(format!(
            "compaction_blob: invalid session_id {session_id:?}"
        ));
    }
    Ok(())
}

fn canonicalize_workspace(workspace_root: &Path) -> Result<PathBuf, String> {
    if workspace_root.as_os_str().is_empty() {
        return Err("compaction_blob: refusing empty workspace_root".into());
    }
    if !workspace_root.is_absolute() {
        return Err(format!(
            "compaction_blob: refusing relative workspace_root {workspace_root:?}"
        ));
    }
    let canonical = std::fs::canonicalize(workspace_root)
        .map_err(|e| format!("compaction_blob: canonicalize {workspace_root:?}: {e}"))?;
    if canonical == Path::new("/") {
        return Err(format!(
            "compaction_blob: refusing root workspace_root (canonicalised {canonical:?})"
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelier_core::context::{
        ContextItem, ContextItemId, Payload, Provenance, TokenCount, TokenSource,
    };
    use tempfile::TempDir;

    fn fixture_item(path: &str, tokens: u32) -> ContextItem {
        ContextItem {
            id: ContextItemId::new(),
            payload: Payload::FileRef {
                path: path.into(),
                line_range: None,
            },
            tokens: TokenCount {
                count: tokens,
                source: TokenSource::Exact,
            },
            provenance: Provenance::UserAttached { note: None },
            pinned: false,
            added_at: "2026-05-17T10:00:00Z".into(),
            last_used: "2026-05-17T10:00:00Z".into(),
        }
    }

    fn fresh_session_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn write_then_read_round_trips_items() {
        let ws = TempDir::new().unwrap();
        let sid = fresh_session_id();
        let items = vec![fixture_item("a.rs", 10), fixture_item("b.rs", 20)];
        let w = write(ws.path(), &sid, "2026-05-17T11:00:00Z", &items).expect("write must succeed");
        assert!(w.path.exists());
        assert_eq!(w.blob_id.len(), "comp-".len() + 36);
        assert!(w.relative_path.contains(&sid));

        let back = read(ws.path(), &w.relative_path).expect("read must succeed");
        assert_eq!(back.version, COMPACTION_BLOB_VERSION);
        assert_eq!(back.blob_id, w.blob_id);
        assert_eq!(back.items, items);
    }

    #[test]
    fn write_rejects_oversize_payload() {
        let ws = TempDir::new().unwrap();
        let sid = fresh_session_id();
        // 200 items × ~30 KiB each = ~6 MiB → exceeds the 4 MiB cap.
        let mut items = Vec::with_capacity(200);
        let big_path: String = "x".repeat(30 * 1024);
        for _ in 0..200 {
            items.push(fixture_item(&big_path, 1));
        }
        let err = write(ws.path(), &sid, "t", &items).unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
    }

    #[test]
    fn read_rejects_path_traversal() {
        let ws = TempDir::new().unwrap();
        let err = read(ws.path(), ".atelier/sessions/../escape.json").unwrap_err();
        assert!(err.contains("invalid relative_path"), "got: {err}");
    }

    #[test]
    fn read_rejects_path_outside_atelier_sessions() {
        let ws = TempDir::new().unwrap();
        let err = read(ws.path(), "etc/passwd.json").unwrap_err();
        assert!(err.contains("not under"), "got: {err}");
    }

    #[test]
    fn read_rejects_non_json_suffix() {
        let ws = TempDir::new().unwrap();
        let err = read(ws.path(), ".atelier/sessions/abc/compactions/comp.txt").unwrap_err();
        assert!(err.contains("not a .json file"), "got: {err}");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let ws = TempDir::new().unwrap();
        let sid = fresh_session_id();
        let items = vec![fixture_item("a.rs", 5)];
        let w = write(ws.path(), &sid, "t", &items).expect("write");
        assert!(w
            .path
            .parent()
            .unwrap()
            .ends_with(format!("sessions/{sid}/compactions")));
    }

    #[test]
    fn write_rejects_invalid_session_id() {
        let ws = TempDir::new().unwrap();
        for bad in ["..", "../etc", "abc/def", ".hidden", "a\0b", ""] {
            let err = write(ws.path(), bad, "t", &[]).unwrap_err();
            assert!(err.contains("invalid session_id"), "{bad:?} -> {err}");
        }
    }

    #[test]
    fn write_rejects_relative_workspace_root() {
        let sid = fresh_session_id();
        let err = write(Path::new("relative/dir"), &sid, "t", &[]).unwrap_err();
        assert!(
            err.contains("refusing relative workspace_root"),
            "got: {err}"
        );
    }
}
