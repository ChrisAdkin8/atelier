//! Project bootstrap — `atelier init` (spec §11).
//!
//! Idempotent. Re-running on an initialised repo reports "no changes".

use std::fs;
use std::io::{self, Write};
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
        fs::write(&atelier_md, ATELIER_MD_TEMPLATE)?;
        true
    };

    let gitignore = repo_root.join(".gitignore");
    let (gitignore_present, gitignore_updated) = if gitignore.is_file() {
        let existing = fs::read_to_string(&gitignore)?;
        if gitignore_has_atelier_entry(&existing) {
            (true, false)
        } else {
            append_atelier_entry(&gitignore, &existing)?;
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

fn append_atelier_entry(path: &Path, existing: &str) -> io::Result<()> {
    let mut f = fs::OpenOptions::new().append(true).open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    f.write_all(b".atelier/\n")?;
    Ok(())
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
}
