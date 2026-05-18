//! Phase C close — runner-side instrumentation hooks for §3/§5
//! UX-target measurement.
//!
//! Two records land alongside `session.json` under
//! `<workspace>/.atelier/sessions/<sid>/`:
//!
//!   * `pane_visibility.json` — which UI panes were visible during the
//!     run. Used by the §3 "refactor without conversation pane open"
//!     UX target (canonical fixture
//!     `tests/workload/canonical/t12_refactor_no_conversation_pane`).
//!     Defaults to all panes visible — explicit `false`s come from
//!     the driver via [`Runner::with_pane_visibility`].
//!   * `find_probes.json` — append-only log of "find what agent knows
//!     about file X" probe calls + their median response time. The
//!     §5 UX target ("median <5 s") reads this file. The
//!     `atelier find` CLI subcommand appends each probe; the runner
//!     creates the file if it doesn't exist.
//!
//! Both are *advisory* — failing to write either does not fail the
//! run. The measurement subsystem reads them lazily.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One pane's visibility for a single run. Mirrors the GUI/TUI pane
/// names. Booleans default to `true` (visible) so a driver that
/// doesn't supply a record leaves the §3 measurement at its default
/// "everything was open" assumption — and the t12 fixture's
/// explicit `conversation: false` is what makes the run interesting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneVisibility {
    #[serde(default = "default_true")]
    pub conversation: bool,
    #[serde(default = "default_true")]
    pub diff: bool,
    #[serde(default = "default_true")]
    pub plan: bool,
    #[serde(default = "default_true")]
    pub memory: bool,
    #[serde(default = "default_true")]
    pub context: bool,
    /// Phase C close — §5 mental-model panel. Off by default; only
    /// `true` when the user explicitly toggled the panel on for the
    /// run. Mirrors the dispatcher's `MentalModel.enabled` default.
    #[serde(default)]
    pub mental_model: bool,
}

impl PaneVisibility {
    /// All panes visible (the unset / "default driver" baseline).
    pub fn all_visible() -> Self {
        Self {
            conversation: true,
            diff: true,
            plan: true,
            memory: true,
            context: true,
            mental_model: false, // off by default — separate contract
        }
    }
}

impl Default for PaneVisibility {
    fn default() -> Self {
        Self::all_visible()
    }
}

fn default_true() -> bool {
    true
}

/// Top-level record written to `pane_visibility.json`. Carries the
/// visibility map plus the RFC 3339 timestamp the record was
/// captured. Schema-stable across future revisions: new fields land
/// as `#[serde(default)]` so older readers keep parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneVisibilityRecord {
    pub session_id: String,
    pub captured_at: String,
    pub panes: PaneVisibility,
    /// Free-form driver label ("gui", "tui", "headless"). Helpful
    /// when reconciling a UX-target measurement across runs.
    #[serde(default)]
    pub driver: String,
}

impl PaneVisibilityRecord {
    pub fn new(
        session_id: impl Into<String>,
        captured_at: impl Into<String>,
        panes: PaneVisibility,
        driver: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            captured_at: captured_at.into(),
            panes,
            driver: driver.into(),
        }
    }

    /// Compute the absolute path of the sibling JSON file for the
    /// given session directory. Stable shape: a follow-on
    /// measurement pass can resolve this without re-deriving from
    /// the session uuid.
    pub fn path_for(session_dir: &Path) -> PathBuf {
        session_dir.join("pane_visibility.json")
    }

    /// Best-effort write. Returns the on-disk path on success;
    /// callers log `Err` but do not propagate (per the
    /// "instrumentation is advisory" rule).
    pub fn save_to(&self, session_dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(session_dir)?;
        let target = Self::path_for(session_dir);
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let mut tmp = tempfile::NamedTempFile::new_in(session_dir)?;
        std::io::Write::write_all(&mut tmp, &json)?;
        tmp.persist(&target).map_err(|e| e.error)?;
        Ok(target)
    }

    pub fn load_from(session_dir: &Path) -> std::io::Result<Self> {
        let path = Self::path_for(session_dir);
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }
}

/// One entry in `find_probes.json`. Captures the file path that was
/// queried, the number of `ContextItemSummary` rows that matched,
/// and the elapsed time (ms) from request to first match. The
/// median over the rolling window is the spec §5 UX target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindProbe {
    pub queried_at: String,
    pub path: String,
    pub matched: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindProbeLog {
    #[serde(default)]
    pub probes: Vec<FindProbe>,
}

impl FindProbeLog {
    /// Median elapsed_ms across all recorded probes. Returns `None`
    /// on an empty log so the caller can disambiguate "below
    /// threshold" from "no data yet".
    pub fn median_elapsed_ms(&self) -> Option<u64> {
        if self.probes.is_empty() {
            return None;
        }
        let mut samples: Vec<u64> = self.probes.iter().map(|p| p.elapsed_ms).collect();
        samples.sort_unstable();
        let mid = samples.len() / 2;
        if samples.len() % 2 == 0 {
            Some((samples[mid - 1] + samples[mid]) / 2)
        } else {
            Some(samples[mid])
        }
    }

    pub fn path_for(session_dir: &Path) -> PathBuf {
        session_dir.join("find_probes.json")
    }

    pub fn load_from(session_dir: &Path) -> std::io::Result<Self> {
        let path = Self::path_for(session_dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomic append. Reads the existing log (or starts fresh),
    /// pushes the new probe, writes the whole file back via a
    /// rename. Same discipline as `OnDiskSession::save_to`.
    pub fn append(session_dir: &Path, probe: FindProbe) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(session_dir)?;
        let mut log = Self::load_from(session_dir)?;
        log.probes.push(probe);
        let target = Self::path_for(session_dir);
        let json = serde_json::to_vec_pretty(&log).map_err(std::io::Error::other)?;
        let mut tmp = tempfile::NamedTempFile::new_in(session_dir)?;
        std::io::Write::write_all(&mut tmp, &json)?;
        tmp.persist(&target).map_err(|e| e.error)?;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pane_visibility_default_is_all_visible_except_mental_model() {
        let v = PaneVisibility::default();
        assert!(v.conversation);
        assert!(v.diff);
        assert!(v.plan);
        assert!(v.memory);
        assert!(v.context);
        assert!(!v.mental_model);
    }

    #[test]
    fn pane_visibility_record_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let rec = PaneVisibilityRecord::new(
            "abc",
            "2026-05-17T12:00:00Z",
            PaneVisibility {
                conversation: false,
                diff: true,
                plan: true,
                memory: false,
                context: true,
                mental_model: false,
            },
            "gui",
        );
        let path = rec.save_to(dir.path()).unwrap();
        assert!(path.ends_with("pane_visibility.json"));
        let loaded = PaneVisibilityRecord::load_from(dir.path()).unwrap();
        assert_eq!(loaded, rec);
    }

    #[test]
    fn pane_visibility_missing_fields_default_true() {
        let json = r#"{"session_id":"x","captured_at":"t","panes":{},"driver":""}"#;
        let rec: PaneVisibilityRecord = serde_json::from_str(json).unwrap();
        assert!(rec.panes.conversation);
        assert!(!rec.panes.mental_model);
    }

    #[test]
    fn find_probe_log_median_handles_odd_and_even_lengths() {
        let mut log = FindProbeLog::default();
        assert_eq!(log.median_elapsed_ms(), None);

        let mk = |ms: u64| FindProbe {
            queried_at: "t".into(),
            path: "src/lib.rs".into(),
            matched: 1,
            elapsed_ms: ms,
        };
        log.probes.push(mk(100));
        assert_eq!(log.median_elapsed_ms(), Some(100));
        log.probes.push(mk(300));
        assert_eq!(log.median_elapsed_ms(), Some(200));
        log.probes.push(mk(200));
        assert_eq!(log.median_elapsed_ms(), Some(200));
    }

    #[test]
    fn find_probe_log_append_preserves_existing_entries() {
        let dir = TempDir::new().unwrap();
        let mk = |path: &str, ms: u64| FindProbe {
            queried_at: "t".into(),
            path: path.into(),
            matched: 1,
            elapsed_ms: ms,
        };
        FindProbeLog::append(dir.path(), mk("a", 100)).unwrap();
        FindProbeLog::append(dir.path(), mk("b", 200)).unwrap();
        let log = FindProbeLog::load_from(dir.path()).unwrap();
        assert_eq!(log.probes.len(), 2);
        assert_eq!(log.probes[0].path, "a");
        assert_eq!(log.probes[1].path, "b");
    }

    #[test]
    fn find_probe_log_load_returns_default_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let log = FindProbeLog::load_from(dir.path()).unwrap();
        assert!(log.probes.is_empty());
    }
}
