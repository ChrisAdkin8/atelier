//! Project bootstrap — `atelier init` (spec §11).
//!
//! Idempotent. Re-running on an initialised repo reports "no changes".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Seed template written as `<repo>/ATELIER.md` when the repo has none.
/// Source: `crates/atelier-core/templates/ATELIER.md`.
pub const ATELIER_MD_TEMPLATE: &str = include_str!("../templates/ATELIER.md");

/// Per-step record of what `init` did. The CLI renders this as the one-line
/// summary required by spec §11 step 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSummary {
    pub repo_root: PathBuf,
    pub atelier_dir_created: bool,
    pub sessions_dir_created: bool,
    pub tools_dir_created: bool,
    pub hooks_dir_created: bool,
    pub atelier_md_written: bool,
    pub gitignore_updated: bool,
    pub gitignore_present: bool,
}

impl InitSummary {
    /// True if every step was a no-op — the repo was already initialised.
    pub fn no_changes(&self) -> bool {
        !(self.atelier_dir_created
            || self.sessions_dir_created
            || self.tools_dir_created
            || self.hooks_dir_created
            || self.atelier_md_written
            || self.gitignore_updated)
    }
}

impl std::fmt::Display for InitSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.no_changes() {
            return write!(f, "atelier init: no changes (repo already initialised)");
        }
        let mut parts: Vec<&str> = Vec::new();
        if self.atelier_dir_created {
            parts.push("created .atelier/");
        }
        let mut subs: Vec<&str> = Vec::new();
        if self.sessions_dir_created {
            subs.push("sessions");
        }
        if self.tools_dir_created {
            subs.push("tools");
        }
        if self.hooks_dir_created {
            subs.push("hooks");
        }
        let subs_joined;
        if !subs.is_empty() {
            subs_joined = format!("subdirs: {}", subs.join(", "));
            parts.push(&subs_joined);
        }
        if self.atelier_md_written {
            parts.push("wrote ATELIER.md");
        }
        if self.gitignore_updated {
            parts.push("appended .atelier/ to .gitignore");
        } else if !self.gitignore_present {
            parts.push("no .gitignore present (skipped)");
        }
        write!(f, "atelier init: {}", parts.join("; "))
    }
}

/// Run the bootstrap against `repo_root`. See spec §11 for the contract.
pub fn init(repo_root: &Path) -> io::Result<InitSummary> {
    if !repo_root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "repo root does not exist or is not a directory: {}",
                repo_root.display()
            ),
        ));
    }

    let atelier_dir = repo_root.join(".atelier");
    let atelier_dir_created = !atelier_dir.exists();
    fs::create_dir_all(&atelier_dir)?;

    let mut created = [false; 3];
    for (i, sub) in ["sessions", "tools", "hooks"].iter().enumerate() {
        let p = atelier_dir.join(sub);
        created[i] = !p.exists();
        fs::create_dir_all(&p)?;
    }

    let atelier_md = repo_root.join("ATELIER.md");
    let atelier_md_written = if atelier_md.exists() {
        false
    } else {
        // Atomic write: tempfile+persist so a crash mid-write doesn't
        // leave a half-written ATELIER.md that the next `init` will skip
        // (because `exists()` returns true on the truncated remnant).
        atomic_write(repo_root, &atelier_md, ATELIER_MD_TEMPLATE.as_bytes())?;
        true
    };

    let gitignore = repo_root.join(".gitignore");
    let (gitignore_present, gitignore_updated) = if gitignore.is_file() {
        let existing = fs::read_to_string(&gitignore)?;
        if gitignore_has_atelier_entry(&existing) {
            (true, false)
        } else {
            atomic_append_atelier_entry(repo_root, &gitignore, &existing)?;
            (true, true)
        }
    } else {
        (false, false)
    };

    Ok(InitSummary {
        repo_root: repo_root.to_path_buf(),
        atelier_dir_created,
        sessions_dir_created: created[0],
        tools_dir_created: created[1],
        hooks_dir_created: created[2],
        atelier_md_written,
        gitignore_updated,
        gitignore_present,
    })
}

/// True if any non-comment line matches `.atelier` or `.atelier/` exactly
/// (after trimming surrounding whitespace and any single leading `/`).
fn gitignore_has_atelier_entry(contents: &str) -> bool {
    contents.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return false;
        }
        let stripped = trimmed.trim_start_matches('/').trim_end_matches('/');
        stripped == ".atelier"
    })
}

/// Atomic write via tempfile+rename in the same directory. Used for
/// ATELIER.md so a crash mid-write doesn't leave a truncated file that
/// the next `init` would skip recreating.
fn atomic_write(dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    // The temp file lives next to the target so the rename is
    // same-filesystem (cross-fs rename returns EXDEV and silently falls
    // back to copy+delete, defeating atomicity).
    let parent = target.parent().unwrap_or(dir);
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    io::Write::write_all(tmp.as_file_mut(), bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| e.error)?;
    // POSIX rename is atomic for content but the directory entry's
    // update is buffered until the next natural fsync — without this
    // call, a power loss after `persist` returns can leave the directory
    // in its pre-rename state on stable storage and the next `init`
    // would re-create the file as if nothing happened. Same fix the
    // staging and persistence layers apply (see staging.rs
    // `fsync_dir_best_effort` and persistence.rs `fsync_dir`).
    fsync_dir_best_effort(parent);
    Ok(())
}

/// Best-effort fsync of a directory entry. Mirrors the helper in
/// staging.rs; we re-implement rather than share to keep this module
/// dependency-light (init runs before the rest of the crate).
#[cfg(unix)]
fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(not(unix))]
fn fsync_dir_best_effort(_dir: &Path) {}

/// Atomic read-modify-write append of `.atelier/\n` to `.gitignore`.
/// A concurrent `git status` or another `atelier init` running on the
/// same `.gitignore` would otherwise interleave bytes if we used
/// `OpenOptions::append`.
fn atomic_append_atelier_entry(dir: &Path, path: &Path, existing: &str) -> io::Result<()> {
    let mut new_contents = String::with_capacity(existing.len() + 12);
    new_contents.push_str(existing);
    if !existing.is_empty() && !existing.ends_with('\n') {
        new_contents.push('\n');
    }
    new_contents.push_str(".atelier/\n");
    atomic_write(dir, path, new_contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Minimal scoped tempdir so we don't take a `tempfile` dep just for tests.
    /// `Drop` removes the directory tree.
    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "atelier-init-test-{}-{}-{}",
                label,
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn fresh_repo_creates_all_dirs_and_writes_atelier_md() {
        let r = TempRepo::new("fresh");
        let s = init(&r.path).unwrap();

        assert!(s.atelier_dir_created);
        assert!(s.sessions_dir_created);
        assert!(s.tools_dir_created);
        assert!(s.hooks_dir_created);
        assert!(s.atelier_md_written);
        assert!(!s.gitignore_present);
        assert!(!s.gitignore_updated);

        assert!(r.path.join(".atelier/sessions").is_dir());
        assert!(r.path.join(".atelier/tools").is_dir());
        assert!(r.path.join(".atelier/hooks").is_dir());

        let written = fs::read_to_string(r.path.join("ATELIER.md")).unwrap();
        assert_eq!(written, ATELIER_MD_TEMPLATE);
    }

    #[test]
    fn rerun_is_idempotent() {
        let r = TempRepo::new("rerun");
        let _ = init(&r.path).unwrap();
        let second = init(&r.path).unwrap();

        assert!(!second.atelier_dir_created);
        assert!(!second.sessions_dir_created);
        assert!(!second.tools_dir_created);
        assert!(!second.hooks_dir_created);
        assert!(!second.atelier_md_written);
        assert!(!second.gitignore_updated);
        assert!(second.no_changes());
    }

    #[test]
    fn existing_atelier_md_is_never_overwritten() {
        let r = TempRepo::new("preserve-md");
        fs::write(r.path.join("ATELIER.md"), "# my project\n\nhand-written\n").unwrap();

        let s = init(&r.path).unwrap();

        assert!(!s.atelier_md_written);
        let preserved = fs::read_to_string(r.path.join("ATELIER.md")).unwrap();
        assert_eq!(preserved, "# my project\n\nhand-written\n");
    }

    #[test]
    fn existing_gitignore_gets_atelier_appended_with_newline_safety() {
        let r = TempRepo::new("gitignore-no-trailing-nl");
        fs::write(r.path.join(".gitignore"), "target/\n*.log").unwrap(); // no trailing \n

        let s = init(&r.path).unwrap();

        assert!(s.gitignore_present);
        assert!(s.gitignore_updated);
        let after = fs::read_to_string(r.path.join(".gitignore")).unwrap();
        assert_eq!(after, "target/\n*.log\n.atelier/\n");
    }

    #[test]
    fn gitignore_with_existing_entry_is_not_modified() {
        let r = TempRepo::new("gitignore-already");
        let body = "target/\n.atelier/\nnode_modules/\n";
        fs::write(r.path.join(".gitignore"), body).unwrap();

        let s = init(&r.path).unwrap();

        assert!(s.gitignore_present);
        assert!(!s.gitignore_updated);
        let after = fs::read_to_string(r.path.join(".gitignore")).unwrap();
        assert_eq!(after, body);
    }

    #[test]
    fn gitignore_entry_match_ignores_leading_slash_and_trailing_slash() {
        // `.atelier`, `.atelier/`, `/.atelier`, `/.atelier/` should all count.
        for variant in [".atelier", ".atelier/", "/.atelier", "/.atelier/"] {
            let r = TempRepo::new("gitignore-variants");
            fs::write(r.path.join(".gitignore"), format!("target/\n{variant}\n")).unwrap();
            let s = init(&r.path).unwrap();
            assert!(
                !s.gitignore_updated,
                "variant {variant:?} should be recognised"
            );
        }
    }

    #[test]
    fn gitignore_commented_atelier_does_not_count_as_present() {
        let r = TempRepo::new("gitignore-commented");
        fs::write(r.path.join(".gitignore"), "# .atelier/\ntarget/\n").unwrap();
        let s = init(&r.path).unwrap();
        assert!(s.gitignore_updated);
        let after = fs::read_to_string(r.path.join(".gitignore")).unwrap();
        assert!(after.ends_with(".atelier/\n"));
    }

    #[test]
    fn missing_repo_root_errors() {
        let missing = std::env::temp_dir().join("atelier-init-does-not-exist-xyz");
        let _ = fs::remove_dir_all(&missing);
        let err = init(&missing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn summary_display_describes_fresh_init() {
        let r = TempRepo::new("display-fresh");
        fs::write(r.path.join(".gitignore"), "target/\n").unwrap();
        let s = init(&r.path).unwrap();
        let line = format!("{s}");
        assert!(line.contains("created .atelier/"));
        assert!(line.contains("subdirs: sessions, tools, hooks"));
        assert!(line.contains("wrote ATELIER.md"));
        assert!(line.contains("appended .atelier/ to .gitignore"));
    }

    #[test]
    fn summary_display_describes_noop() {
        let r = TempRepo::new("display-noop");
        let _ = init(&r.path).unwrap();
        let second = init(&r.path).unwrap();
        assert_eq!(
            format!("{second}"),
            "atelier init: no changes (repo already initialised)"
        );
    }

    // P3 regression: ATELIER.md written via tempfile+persist, not bare
    // fs::write — a crash mid-write would otherwise leave a truncated
    // file that the next init silently skips. After init succeeds, no
    // sibling temp file should remain under the repo root.
    #[test]
    fn init_does_not_leak_tempfile_in_repo_root_after_atomic_write() {
        let r = TempRepo::new("p3-atomic-md");
        init(&r.path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&r.path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write should clean up its temp file: {leftovers:?}"
        );
        assert!(r.path.join("ATELIER.md").exists());
    }

    // P3 regression: appending the .atelier entry is now read-modify-write
    // via tempfile+rename, not OpenOptions::append. The end-state must
    // still match the original behavior — one `.atelier/` line, preserved
    // trailing newline, no doubling.
    #[test]
    fn gitignore_append_is_idempotent_and_preserves_trailing_newline() {
        let r = TempRepo::new("p3-gi-atomic");
        std::fs::write(r.path.join(".gitignore"), "target/\n").unwrap();
        init(&r.path).unwrap();
        let gi = std::fs::read_to_string(r.path.join(".gitignore")).unwrap();
        assert!(gi.ends_with("\n"));
        assert_eq!(
            gi.matches(".atelier/").count(),
            1,
            "should append exactly once: {gi:?}"
        );
        // Re-running init must NOT append again (gitignore_has_atelier_entry
        // already detects the line; this verifies atomic_append isn't
        // called the second time).
        init(&r.path).unwrap();
        let gi2 = std::fs::read_to_string(r.path.join(".gitignore")).unwrap();
        assert_eq!(gi, gi2);
    }
}
