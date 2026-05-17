//! §7 per-repo definition-of-done config loader.
//!
//! Schema: `schemas/config/dod.v1.json`. Mirrors the loader pattern used by
//! `crate::hooks` — round-trips through `serde`, runs the runtime invariants
//! that JSON Schema cannot express (name regex, sane timeout, repo-relative
//! `working_dir`), and refuses unknown fields via `deny_unknown_fields`.
//!
//! Discovery (spec §7, generalised from the §15 hooks pattern):
//!
//!   * `<repo>/.atelier/dod.json` — per-repo (load first if present).
//!   * `~/.atelier/dod.json` — global fallback (used only if no per-repo file).
//!
//! Loaded at session start; consumed by the [`crate::state::State::Verifying`]
//! state, which shells out each check inside the §11 sandbox profile.
//! Per-check pass / fail folds into the §1 ledger and the
//! [`crate::verify`] discrepancy report.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version supported by this build.
pub const DOD_VERSION: u32 = 1;

/// Filename inside `.atelier/` for the per-repo and global DoD files.
pub const DOD_FILE: &str = "dod.json";

/// Top-level DoD config. Round-trips `schemas/config/dod.v1.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DodConfig {
    pub version: u32,
    pub checks: Vec<DodCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DodCheck {
    pub name: String,
    pub tier: DodTier,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub expect: ExpectClause,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Spec §7 lists test / typecheck / lint / build as the canonical tiers.
/// `Custom` is the escape hatch for log-line assertions and similar one-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DodTier {
    Test,
    Typecheck,
    Lint,
    Build,
    Custom,
}

impl DodTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Typecheck => "typecheck",
            Self::Lint => "lint",
            Self::Build => "build",
            Self::Custom => "custom",
        }
    }
}

/// Assertions on a check's outcome. At least one field must be `Some`. The
/// schema enforces this with `anyOf`; the loader re-enforces it as a
/// belt-and-braces against drift in either direction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectClause {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code_ne: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_pattern: Option<String>,
}

impl ExpectClause {
    fn is_empty(&self) -> bool {
        self.exit_code.is_none()
            && self.exit_code_ne.is_none()
            && self.stdout_contains.is_none()
            && self.stderr_contains.is_none()
            && self.stdout_pattern.is_none()
            && self.stderr_pattern.is_none()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DodError {
    #[error("DoD config parse error: {0}")]
    Parse(String),

    #[error("DoD config at {path} failed to parse: {message}")]
    ParseAt { path: PathBuf, message: String },

    #[error("DoD config version {got} != supported {expected}")]
    IncompatibleVersion { got: u32, expected: u32 },

    #[error("DoD `checks` array must contain at least one entry")]
    EmptyChecks,

    #[error("DoD check {0:?} violates the name regex ^[a-z][a-z0-9_-]*$")]
    InvalidName(String),

    #[error("DoD check {0:?}: command must not be empty")]
    EmptyCommand(String),

    #[error("DoD check {0:?}: expect clause must contain at least one assertion")]
    EmptyExpect(String),

    #[error("DoD check {0:?}: working_dir {1:?} must be repo-relative (no leading / or `..`)")]
    WorkingDirEscapes(String, String),

    #[error("DoD check {0:?}: timeout_ms must be >= 1")]
    InvalidTimeout(String),

    #[error("DoD I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl DodConfig {
    pub fn from_json(bytes: &[u8]) -> Result<Self, DodError> {
        let cfg: Self =
            serde_json::from_slice(bytes).map_err(|e| DodError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), DodError> {
        if self.version != DOD_VERSION {
            return Err(DodError::IncompatibleVersion {
                got: self.version,
                expected: DOD_VERSION,
            });
        }
        if self.checks.is_empty() {
            return Err(DodError::EmptyChecks);
        }
        for c in &self.checks {
            if !name_is_valid(&c.name) {
                return Err(DodError::InvalidName(c.name.clone()));
            }
            if c.command.is_empty() {
                return Err(DodError::EmptyCommand(c.name.clone()));
            }
            if c.expect.is_empty() {
                return Err(DodError::EmptyExpect(c.name.clone()));
            }
            if let Some(t) = c.timeout_ms {
                if t == 0 {
                    return Err(DodError::InvalidTimeout(c.name.clone()));
                }
            }
            if let Some(wd) = &c.working_dir {
                if wd.starts_with('/') || wd.split('/').any(|seg| seg == "..") {
                    return Err(DodError::WorkingDirEscapes(c.name.clone(), wd.clone()));
                }
            }
        }
        Ok(())
    }

    /// Per-repo path: `<repo>/.atelier/dod.json`.
    pub fn per_repo_path(repo_root: &Path) -> PathBuf {
        repo_root.join(".atelier").join(DOD_FILE)
    }

    /// Global path: `$HOME/.atelier/dod.json`. Returns `None` when home
    /// cannot be resolved.
    pub fn global_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".atelier").join(DOD_FILE))
    }

    /// Discovery: per-repo file overrides global. Missing both is
    /// `Ok(None)`.
    ///
    /// **Fail-open warning.** Callers MUST NOT treat `Ok(None)` as
    /// "verification passed". `None` means "no DoD was configured" —
    /// the Verifying state should degrade to a UI banner ("no DoD
    /// configured for this repo") and the persisted session's
    /// `dod_passed` field should be `None`, not `Some(true)`. Reporting
    /// `Some(true)` because no checks were defined is the
    /// rubber-stamp anti-pattern the §7 contract exists to prevent.
    /// See [`Self::paths_searched`] if you want to log where discovery
    /// looked.
    pub fn load(repo_root: &Path) -> Result<Option<Self>, DodError> {
        let per_repo = Self::per_repo_path(repo_root);
        if per_repo.is_file() {
            return Ok(Some(Self::load_from(&per_repo)?));
        }
        if let Some(global) = Self::global_path() {
            if global.is_file() {
                return Ok(Some(Self::load_from(&global)?));
            }
        }
        Ok(None)
    }

    /// The paths [`Self::load`] would (or did) consult, in priority order.
    /// Useful for telling the user *why* DoD discovery returned `None` —
    /// e.g. logging "no DoD configured (searched: <paths>)" instead of
    /// silently degrading.
    pub fn paths_searched(repo_root: &Path) -> Vec<PathBuf> {
        let mut out = vec![Self::per_repo_path(repo_root)];
        if let Some(global) = Self::global_path() {
            out.push(global);
        }
        out
    }

    /// Load from an explicit path (testing, atypical layouts).
    pub fn load_from(path: &Path) -> Result<Self, DodError> {
        let bytes = std::fs::read(path).map_err(|e| DodError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::from_json(&bytes).map_err(|e| match e {
            DodError::Parse(message) => DodError::ParseAt {
                path: path.to_path_buf(),
                message,
            },
            other => other,
        })
    }

    /// Convenience selector — checks by tier in declared order. UI groups
    /// the Verifying-state report by tier so the user sees `test` results
    /// distinctly from `lint` etc.
    pub fn by_tier(&self, tier: DodTier) -> Vec<&DodCheck> {
        self.checks.iter().filter(|c| c.tier == tier).collect()
    }
}

fn name_is_valid(s: &str) -> bool {
    // Matches `^[a-z][a-z0-9_-]*$` — same regex as `hooks::HookManifest::name`
    // so the trust budget can treat them uniformly.
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_the_bundled_example() {
        let bytes = std::fs::read(Path::new("../../examples/config/dod.v1.json")).unwrap();
        let cfg = DodConfig::from_json(&bytes).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.checks.len(), 3);
        let names: Vec<&str> = cfg.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["pytest", "ruff", "mypy"]);
    }

    #[test]
    fn round_trips_through_json() {
        let cfg = DodConfig {
            version: 1,
            checks: vec![DodCheck {
                name: "build".into(),
                tier: DodTier::Build,
                command: "cargo build".into(),
                working_dir: None,
                timeout_ms: Some(60_000),
                expect: ExpectClause {
                    exit_code: Some(0),
                    ..Default::default()
                },
                tags: vec!["slow".into()],
            }],
        };
        let json = serde_json::to_vec(&cfg).unwrap();
        let back = DodConfig::from_json(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn rejects_unsupported_version() {
        let bad = r#"{"version": 7, "checks": [{"name":"x","tier":"test","command":"x","expect":{"exit_code":0}}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::IncompatibleVersion { got: 7, .. }));
    }

    #[test]
    fn rejects_empty_checks_array() {
        let bad = r#"{"version": 1, "checks": []}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::EmptyChecks));
    }

    #[test]
    fn rejects_invalid_names() {
        for bad in ["Bad", "1lead", "with space", ""] {
            let json = format!(
                r#"{{"version":1,"checks":[{{"name":"{bad}","tier":"test","command":"x","expect":{{"exit_code":0}}}}]}}"#
            );
            let err = DodConfig::from_json(json.as_bytes()).unwrap_err();
            assert!(
                matches!(err, DodError::InvalidName(_)),
                "expected InvalidName for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_empty_command() {
        let bad = r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"","expect":{"exit_code":0}}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::EmptyCommand(_)));
    }

    #[test]
    fn rejects_empty_expect_clause() {
        // serde accepts an empty object for ExpectClause (all fields are
        // optional); the runtime validator catches it. The JSON Schema's
        // `anyOf` also catches it at validation time at the rig.
        let bad =
            r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"echo","expect":{}}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::EmptyExpect(_)));
    }

    #[test]
    fn rejects_zero_timeout() {
        let bad = r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"x","timeout_ms":0,"expect":{"exit_code":0}}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::InvalidTimeout(_)));
    }

    #[test]
    fn rejects_absolute_working_dir() {
        let bad = r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"x","working_dir":"/etc","expect":{"exit_code":0}}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::WorkingDirEscapes(..)));
    }

    #[test]
    fn rejects_parent_dir_escape_in_working_dir() {
        let bad = r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"x","working_dir":"../outside","expect":{"exit_code":0}}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::WorkingDirEscapes(..)));
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let bad = r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"x","expect":{"exit_code":0}}],"unknown":1}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::Parse(_)));
    }

    #[test]
    fn rejects_unknown_check_fields() {
        let bad = r#"{"version":1,"checks":[{"name":"x","tier":"test","command":"x","expect":{"exit_code":0},"weird":true}]}"#;
        let err = DodConfig::from_json(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, DodError::Parse(_)));
    }

    #[test]
    fn all_tiers_round_trip() {
        for (literal, tier) in [
            ("test", DodTier::Test),
            ("typecheck", DodTier::Typecheck),
            ("lint", DodTier::Lint),
            ("build", DodTier::Build),
            ("custom", DodTier::Custom),
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, format!("\"{literal}\""));
            let back: DodTier = serde_json::from_str(&json).unwrap();
            assert_eq!(back, tier);
        }
    }

    #[test]
    fn by_tier_filters_in_declared_order() {
        let cfg: DodConfig = serde_json::from_str(
            r#"{"version":1,"checks":[
                {"name":"a","tier":"lint","command":"x","expect":{"exit_code":0}},
                {"name":"b","tier":"test","command":"x","expect":{"exit_code":0}},
                {"name":"c","tier":"lint","command":"x","expect":{"exit_code":0}}
            ]}"#,
        )
        .unwrap();
        let lints = cfg.by_tier(DodTier::Lint);
        let names: Vec<&str> = lints.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert_eq!(cfg.by_tier(DodTier::Build).len(), 0);
    }

    // ---------- discovery ----------

    #[test]
    fn load_returns_none_when_no_dod_present() {
        let dir = TempDir::new().unwrap();
        // Empty repo; no global path expected to exist either, but to be
        // safe we point HOME at the temp dir so the global lookup also
        // misses cleanly.
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", dir.path());
        let result = DodConfig::load(dir.path()).unwrap();
        // Restore HOME before assertion to avoid corrupting the env on
        // failure.
        if let Some(h) = saved_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        assert!(result.is_none());
    }

    #[test]
    fn load_prefers_per_repo_over_global() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();

        // Per-repo DoD: name "from_repo".
        let per_repo_dir = repo.path().join(".atelier");
        std::fs::create_dir_all(&per_repo_dir).unwrap();
        std::fs::write(
            per_repo_dir.join(DOD_FILE),
            r#"{"version":1,"checks":[{"name":"from_repo","tier":"test","command":"x","expect":{"exit_code":0}}]}"#,
        )
        .unwrap();

        // Global DoD: name "from_global".
        let global_dir = home.path().join(".atelier");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join(DOD_FILE),
            r#"{"version":1,"checks":[{"name":"from_global","tier":"test","command":"x","expect":{"exit_code":0}}]}"#,
        )
        .unwrap();

        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let loaded = DodConfig::load(repo.path()).unwrap().unwrap();
        if let Some(h) = saved_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        assert_eq!(loaded.checks[0].name, "from_repo");
    }

    #[test]
    fn load_from_explicit_path_surfaces_io_error_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let err = DodConfig::load_from(&dir.path().join("nope.json")).unwrap_err();
        assert!(matches!(err, DodError::Io { .. }));
    }
}
