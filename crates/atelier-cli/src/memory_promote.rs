//! v60 — shared "promote a memory card to disk" writer.
//!
//! `MemoryStore::promote_to_global` (in atelier-core) is pure: it
//! returns `PromoteOutput { relative_path, bytes }` and lets the
//! caller do the I/O. Pre-v60 the GUI Tauri command did this
//! correctly (HOME validation, canonical-root containment, size cap,
//! atomic `NamedTempFile::persist` write) while the TUI driver had a
//! copy-paste of the *unvalidated* version — the v59 audit's
//! security M-1 finding. v60 consolidates the disk-side hardening
//! here so both drivers go through one code path.
//!
//! Doesn't live in atelier-core because the staging-layer rule is
//! "data layer is I/O-free"; `atelier-cli` is the right place for
//! the disk write.

use std::io::Write;
use std::path::{Path, PathBuf};

use atelier_core::memory::PromoteOutput;

/// Per-call cap on the promoted bytes a `PromoteOutput` may persist.
/// Belt-and-braces with `MemoryStore::add`'s `MAX_MEMORY_CARD_BYTES`:
/// `promote_to_global` wraps content in YAML frontmatter so the on-
/// disk size is larger than the source by a known fixed overhead, but
/// we apply a hard cap here too. 256 KiB matches the GUI's v58/v59
/// constant; the TUI path now shares it.
pub const MAX_PROMOTE_BYTES: usize = 256 * 1024;

/// Outcome of a successful promote-and-write. `path` is the absolute
/// target the bytes were persisted at; `bytes` is the count actually
/// written (always equal to `output.bytes.len()`, exposed for the
/// caller's "promoted → /path/to/file.md (N bytes)" UX toast).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedWrite {
    pub path: PathBuf,
    pub bytes: usize,
}

/// Persist a memory-card promote payload to `~/.atelier/memory/`.
/// Hardens against the failure modes the v58 / v59 / v60 audits
/// surfaced:
///
/// * **HOME hygiene** — non-empty, absolute, canonical path different
///   from `/`. v58 caught `HOME=""`; v59 caught the exact-string `/`
///   case; v60 also rejects multi-slash / relative / non-canonical
///   roots via the canonicalize check.
/// * **`relative_path` hygiene** — non-empty, no path separators, no
///   `..`, no leading-dot, no control characters.
/// * **Size cap** — rejects payloads > [`MAX_PROMOTE_BYTES`].
/// * **Canonical-root containment** — the target's canonical parent
///   must equal the canonical memory root, defending against any
///   future regression in `MemoryStore::promote_to_global`'s filename
///   sanitiser.
/// * **Atomic write** — bytes go through
///   [`tempfile::NamedTempFile::persist`] so a crash mid-write can't
///   leave a partial file under `~/.atelier/memory/`.
pub fn write_promoted_card(output: &PromoteOutput) -> Result<PromotedWrite, String> {
    // ---- relative_path hygiene ----
    let rel = output.relative_path.as_str();
    if rel.is_empty()
        || rel == "."
        || rel == ".."
        || rel.starts_with('.')
        || rel.contains('/')
        || rel.contains('\\')
        || rel.chars().any(|c| (c as u32) < 0x20)
    {
        return Err(format!(
            "promote_memory_card: invalid relative_path {rel:?}"
        ));
    }

    // ---- size cap ----
    if output.bytes.len() > MAX_PROMOTE_BYTES {
        return Err(format!(
            "promote_memory_card: {} bytes exceeds {MAX_PROMOTE_BYTES}",
            output.bytes.len()
        ));
    }

    // ---- HOME hygiene ----
    let home = std::env::var("HOME").map_err(|_| "HOME env var unset".to_string())?;
    if home.is_empty() {
        return Err("promote_memory_card: refusing empty HOME".into());
    }
    let home_path = PathBuf::from(&home);
    if !home_path.is_absolute() {
        return Err(format!(
            "promote_memory_card: refusing relative HOME={home:?}"
        ));
    }
    let canonical_home = std::fs::canonicalize(&home_path)
        .map_err(|e| format!("canonicalize HOME ({home:?}): {e}"))?;
    if canonical_home == Path::new("/") {
        return Err(format!(
            "promote_memory_card: refusing root HOME (canonicalised {canonical_home:?})"
        ));
    }

    // ---- canonical-root containment ----
    let memory_root = canonical_home.join(".atelier").join("memory");
    std::fs::create_dir_all(&memory_root).map_err(|e| format!("mkdir {memory_root:?}: {e}"))?;
    let canonical_root = std::fs::canonicalize(&memory_root)
        .map_err(|e| format!("canonicalize {memory_root:?}: {e}"))?;
    let target = canonical_root.join(rel);
    let canonical_target_parent = std::fs::canonicalize(
        target
            .parent()
            .ok_or_else(|| format!("target {target:?} has no parent"))?,
    )
    .map_err(|e| format!("canonicalize parent of {target:?}: {e}"))?;
    if !canonical_target_parent.starts_with(&canonical_root) {
        return Err(format!(
            "promote_memory_card: {target:?} parent {canonical_target_parent:?} escapes {canonical_root:?}"
        ));
    }

    // ---- atomic write ----
    // v60.37 A1 — full discipline: data + metadata fsync, then atomic
    // rename, then parent-dir fsync. Earlier versions stopped at persist,
    // leaving a window where a power loss between persist() and the next
    // natural fs sync could leave the directory in its pre-rename state.
    let mut tmp = tempfile::NamedTempFile::new_in(&canonical_root)
        .map_err(|e| format!("create temp in {canonical_root:?}: {e}"))?;
    tmp.write_all(&output.bytes)
        .map_err(|e| format!("write temp: {e}"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| format!("sync_all temp: {e}"))?;
    tmp.persist(&target)
        .map_err(|e| format!("persist {target:?}: {e}"))?;
    atelier_core::path_safety::fsync_dir(&canonical_root)
        .map_err(|e| format!("fsync_dir {canonical_root:?}: {e}"))?;
    let index_path = atelier_core::memory_index::user_memory_index_path(&canonical_home);
    if let Err(e) = atelier_core::memory_index::upsert_memory_card_file(
        &target,
        &index_path,
        atelier_core::memory_index::MemoryScope::User,
    ) {
        tracing::warn!(
            error = %e,
            path = %target.display(),
            index = %index_path.display(),
            "promote_memory_card: promoted card persisted but memory index update failed"
        );
    }
    Ok(PromotedWrite {
        path: target,
        bytes: output.bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_output(rel: &str, bytes: &[u8]) -> PromoteOutput {
        PromoteOutput {
            relative_path: rel.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn rejects_empty_relative_path() {
        let o = make_output("", b"x");
        assert!(write_promoted_card(&o).is_err());
    }

    #[test]
    fn rejects_dotdot_relative_path() {
        let o = make_output("..", b"x");
        assert!(write_promoted_card(&o).is_err());
    }

    #[test]
    fn rejects_path_with_separator() {
        let o = make_output("a/b.md", b"x");
        assert!(write_promoted_card(&o).is_err());
    }

    #[test]
    fn rejects_oversize_bytes() {
        let big = vec![b'x'; MAX_PROMOTE_BYTES + 1];
        let o = make_output("ok.md", &big);
        assert!(write_promoted_card(&o).is_err());
    }
}
