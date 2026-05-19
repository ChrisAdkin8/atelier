//! `atelier-conformance-status` — one-line digest of the most-recent
//! Phase B §2 real-model conformance nightly run, read from
//! `tests/phase_b_gate/last_run.json`.
//!
//! Spec context: Phase B's mechanical gate text is **§2 mechanical +
//! real-model conformance ≥95% (PROVISIONAL); §7 lying-agent and
//! hallucinating-agent fixtures.** The "real-model conformance" half
//! runs nightly via `.github/workflows/nightly_phase_b_gate.yml` against
//! `anthropic:claude-haiku-4-5` and (when the secret is wired) hosted
//! OpenAI. Each run writes a `ConformanceSummary` row per strategy plus
//! an aggregate `ConformanceStatus` verdict to
//! `tests/phase_b_gate/last_run.json`, validated against
//! `schemas/ci/protocol_conformance.v1.json`. This binary reads the
//! committed artifact for downstream consumers.
//!
//! Exit codes:
//!   - 0   — last run is `all_passed: true` (Green or Yellow verdict)
//!   - 1   — last run is `all_passed: false` (Red verdict)
//!   - 2   — the artifact is missing, unreadable, or malformed
//!
//! Output (stdout):
//!
//! ```text
//! 2026-05-18T06:30:00Z 64f0fa6 status=yellow calibration=true floor=0.95
//!   native_tool: 14/15 turns (0.933)
//!   json_sentinel: 6/6 turns (1.000)
//! Phase B §2: YELLOW (calibration phase, 2/2 strategies above floor)
//! ```
//!
//! Why the same shape as `phase_a_gate_status`: the workflow → artifact →
//! status binary triplet is the project's standard nightly pattern
//! (per v60.13 Track C). The Phase B half deliberately mirrors it so a
//! maintainer reading either binary picks up the other's structure for
//! free.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

/// In-tree default path. Override via positional arg for worktrees /
/// CI matrices.
///
/// v60.32 M05 — resolve relative to runtime CWD (or
/// `ATELIER_PROJECT_DIR` when set). A binary built in one workspace
/// and run from another previously read the build-time manifest dir,
/// surfacing stale data. The build-time manifest dir remains
/// available as a last-resort fallback for `--debug` callers; without
/// the flag we only consult runtime locations.
fn default_artifact_path() -> PathBuf {
    let rel = std::path::Path::new("tests/phase_b_gate/last_run.json");
    if let Ok(project_dir) = std::env::var("ATELIER_PROJECT_DIR") {
        return PathBuf::from(project_dir).join(rel);
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(rel),
        Err(_) => PathBuf::from(rel),
    }
}

/// `--debug` fallback for test runs: the build-time manifest dir.
/// Keeps `bundled_seed_artifact_parses` working without depending on
/// the integration-test harness setting CWD.
fn debug_artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/phase_b_gate/last_run.json")
}

#[derive(Debug, Deserialize)]
struct LastRun {
    version: u32,
    run_id: String,
    git_sha: String,
    all_passed: bool,
    status: String,
    calibration_phase: bool,
    #[serde(default)]
    floor: Option<f32>,
    summaries: Vec<Summary>,
    #[serde(default)]
    #[allow(dead_code)]
    providers_tested: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Summary {
    strategy: String,
    total_turns: u32,
    malformed_turns: u32,
    rate: f32,
    #[serde(default)]
    #[allow(dead_code)]
    verdict: Option<String>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let path = match first.as_deref() {
        Some("--debug") => args
            .next()
            .map(PathBuf::from)
            .unwrap_or_else(debug_artifact_path),
        Some(p) => PathBuf::from(p),
        None => default_artifact_path(),
    };
    let exit_code = run(&path);
    std::process::exit(exit_code);
}

fn run(path: &std::path::Path) -> i32 {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("conformance-status: cannot read {}: {e}", path.display());
            return 2;
        }
    };
    let parsed: LastRun = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "conformance-status: {} is not valid protocol_conformance.v1.json: {e}",
                path.display()
            );
            return 2;
        }
    };
    if parsed.version != 1 {
        eprintln!(
            "conformance-status: unsupported version {} in {} (only v1 is read)",
            parsed.version,
            path.display()
        );
        return 2;
    }

    // Header line: run id + sha + verdict.
    let floor_str = parsed
        .floor
        .map(|f| format!(" floor={f:.2}"))
        .unwrap_or_default();
    println!(
        "{} {} status={} calibration={}{}",
        parsed.run_id, parsed.git_sha, parsed.status, parsed.calibration_phase, floor_str,
    );

    // Per-strategy lines.
    for s in &parsed.summaries {
        let successes = s.total_turns.saturating_sub(s.malformed_turns);
        println!(
            "  {}: {}/{} turns ({:.3})",
            s.strategy, successes, s.total_turns, s.rate,
        );
    }

    // Digest line. The verdict label takes the wire label and uppercases
    // it; the second clause is the run-relative summary maintainers want
    // when they skim the workflow output.
    let label = parsed.status.to_uppercase();
    let calibration_suffix = if parsed.calibration_phase {
        " (calibration phase)".to_string()
    } else {
        String::new()
    };
    let n_strategies = parsed.summaries.len();
    if n_strategies == 0 {
        println!("Phase B §2: {label}{calibration_suffix} — no evidence yet");
    } else {
        let above_floor = parsed
            .summaries
            .iter()
            .filter(|s| s.rate >= parsed.floor.unwrap_or(0.95))
            .count();
        println!(
            "Phase B §2: {label}{calibration_suffix} ({above_floor}/{n_strategies} strategies above floor)",
        );
    }

    if !parsed.all_passed {
        // Red verdict — name the offending strategies.
        for s in &parsed.summaries {
            let floor = parsed.floor.unwrap_or(0.95);
            if s.rate < floor && s.total_turns >= 20 {
                eprintln!(
                    "  {} below floor: rate={:.3} (floor={:.2}, evidence={} turns)",
                    s.strategy, s.rate, floor, s.total_turns,
                );
            }
        }
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn write_tmp(body: serde_json::Value) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(serde_json::to_string(&body).unwrap().as_bytes())
            .unwrap();
        f
    }

    #[test]
    fn green_run_exits_zero() {
        let f = write_tmp(json!({
            "version": 1,
            "run_id": "2026-05-25T06:30:00Z",
            "git_sha": "abc1234",
            "all_passed": true,
            "status": "green",
            "calibration_phase": false,
            "floor": 0.95,
            "summaries": [
                {
                    "strategy": "native_tool",
                    "total_turns": 25,
                    "malformed_turns": 1,
                    "rate": 0.96,
                },
            ],
        }));
        assert_eq!(run(f.path()), 0);
    }

    #[test]
    fn yellow_during_calibration_exits_zero() {
        // Calibration phase: not enough evidence yet → Yellow → all_passed
        // stays true → exit 0.
        let f = write_tmp(json!({
            "version": 1,
            "run_id": "2026-05-19T06:30:00Z",
            "git_sha": "abc1234",
            "all_passed": true,
            "status": "yellow",
            "calibration_phase": true,
            "floor": 0.95,
            "summaries": [],
        }));
        assert_eq!(run(f.path()), 0);
    }

    #[test]
    fn red_run_exits_one() {
        let f = write_tmp(json!({
            "version": 1,
            "run_id": "2026-05-25T06:30:00Z",
            "git_sha": "abc1234",
            "all_passed": false,
            "status": "red",
            "calibration_phase": false,
            "floor": 0.95,
            "summaries": [
                {
                    "strategy": "native_tool",
                    "total_turns": 25,
                    "malformed_turns": 5,
                    "rate": 0.80,
                },
            ],
        }));
        assert_eq!(run(f.path()), 1);
    }

    #[test]
    fn missing_file_exits_two() {
        let nonexistent = std::path::Path::new("/dev/null/no-such-file-9d4b2");
        assert_eq!(run(nonexistent), 2);
    }

    #[test]
    fn malformed_json_exits_two() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{ not json").unwrap();
        assert_eq!(run(f.path()), 2);
    }

    #[test]
    fn unsupported_version_exits_two() {
        let f = write_tmp(json!({
            "version": 99,
            "run_id": "2026-05-25T06:30:00Z",
            "git_sha": "abc1234",
            "all_passed": true,
            "status": "green",
            "calibration_phase": false,
            "summaries": [],
        }));
        assert_eq!(run(f.path()), 2);
    }

    /// The bundled seed `last_run.json` must round-trip through the
    /// reader — catches a future schema change that breaks the binary
    /// before it lands in CI.
    #[test]
    fn bundled_seed_artifact_parses() {
        let path = debug_artifact_path();
        if !path.exists() {
            return;
        }
        let exit = run(&path);
        // The seed is calibration_phase=true → all_passed=true → exit 0.
        assert_eq!(exit, 0, "seed artifact at {} did not pass", path.display());
    }

    /// v60.32 M05 — `ATELIER_PROJECT_DIR` should win over CWD when set,
    /// and CWD itself should drive the result when the env var is
    /// absent. Build-time manifest dir is no longer the default.
    #[test]
    fn default_artifact_path_prefers_project_dir_env_then_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = std::path::Path::new("tests/phase_b_gate/last_run.json");

        // SAFETY: tests in the same process can race on env vars and CWD;
        // serialise via a process-wide mutex.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let prior_pd = std::env::var("ATELIER_PROJECT_DIR").ok();
        let prior_cwd = std::env::current_dir().ok();

        // ATELIER_PROJECT_DIR set → wins (no CWD canonicalisation needed
        // because we pass the env path through unmodified).
        unsafe {
            std::env::set_var("ATELIER_PROJECT_DIR", tmp.path());
        }
        assert_eq!(default_artifact_path(), tmp.path().join(rel));

        // No env var → CWD wins. We assert that the returned path is
        // anchored at CWD, not the build-time manifest dir, by checking
        // it does NOT start with `CARGO_MANIFEST_DIR` and DOES end with
        // the relative artifact path.
        unsafe {
            std::env::remove_var("ATELIER_PROJECT_DIR");
        }
        std::env::set_current_dir(tmp.path()).unwrap();
        let resolved = default_artifact_path();
        assert!(
            resolved.ends_with(rel),
            "expected resolved path to end with {rel:?}, got {resolved:?}"
        );
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            !resolved.starts_with(manifest),
            "default_artifact_path leaked the build-time manifest dir: {resolved:?}"
        );

        // Restore.
        if let Some(prior) = prior_pd {
            unsafe {
                std::env::set_var("ATELIER_PROJECT_DIR", prior);
            }
        }
        if let Some(prior) = prior_cwd {
            let _ = std::env::set_current_dir(prior);
        }
    }
}
