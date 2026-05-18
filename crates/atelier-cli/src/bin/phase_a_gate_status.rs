//! `atelier-phase-a-gate-status` — one-line digest of the most-recent
//! Phase A nightly gate run, read from `tests/phase_a_gate/last_run.json`.
//!
//! Spec context: Phase A's mechanical gates (rig, clippy, workspace tests,
//! canonical workloads, npx-gated MCP integration) run nightly via
//! `.github/workflows/nightly_phase_a_gate.yml`; this binary is the
//! "is Phase A green today?" probe. The workflow itself writes the
//! `last_run.json` artifact and validates it against
//! `schemas/ci/phase_a_gate.v1.json`; this binary is purely a reader.
//!
//! Exit codes:
//!   - 0   — last run is `all_passed: true`
//!   - 1   — last run has ≥1 `failed` gate (the workflow's "red" state)
//!   - 2   — the artifact is missing, unreadable, or malformed
//!
//! Output (stdout, one line per gate; final line is the digest):
//!
//! ```text
//! 2026-05-18T06:00:00Z 8b96991 fmt:passed clippy:passed cargo_test_workspace:passed \
//!   rig_check:passed mcp_integration_npx:skipped
//! Phase A: GREEN  (5 gates: 4 passed, 0 failed, 1 skipped)
//! ```
//!
//! Why a separate binary rather than an `atelier <subcommand>`: the nightly
//! workflow runs this with no other harness state in scope (no session,
//! no adapter); building a full `atelier` invocation for what is a 30-line
//! JSON read would be wasteful. The single-purpose binary also makes the
//! CI step's `run:` line trivial to grep for.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

/// In-tree default path. The CLI accepts a single positional argument to
/// override (so a worktree or a CI matrix can point at a non-canonical
/// location); otherwise we resolve relative to the manifest dir at
/// build time.
fn default_artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/phase_a_gate/last_run.json")
}

#[derive(Debug, Deserialize)]
struct LastRun {
    version: u32,
    run_id: String,
    git_sha: String,
    all_passed: bool,
    gates: Vec<Gate>,
}

#[derive(Debug, Deserialize)]
struct Gate {
    name: String,
    status: String,
    #[serde(default)]
    #[allow(dead_code)] // surfaced in the per-gate line only when present
    duration_secs: Option<f64>,
    #[serde(default)]
    details: Option<String>,
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_artifact_path);
    let exit_code = run(&path);
    std::process::exit(exit_code);
}

fn run(path: &std::path::Path) -> i32 {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("phase-a-gate-status: cannot read {}: {e}", path.display());
            return 2;
        }
    };
    let parsed: LastRun = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "phase-a-gate-status: {} is not valid phase_a_gate.v1.json: {e}",
                path.display()
            );
            return 2;
        }
    };
    if parsed.version != 1 {
        eprintln!(
            "phase-a-gate-status: unsupported version {} in {} (only v1 is read)",
            parsed.version,
            path.display()
        );
        return 2;
    }

    // Per-gate one-liner.
    let mut header = format!("{} {}", parsed.run_id, parsed.git_sha);
    for g in &parsed.gates {
        header.push(' ');
        header.push_str(&g.name);
        header.push(':');
        header.push_str(&g.status);
    }
    println!("{header}");

    // Tally + digest.
    let (passed, failed, skipped) = tally(&parsed.gates);
    let label = if parsed.all_passed { "GREEN" } else { "RED" };
    println!(
        "Phase A: {label}  ({} gates: {} passed, {} failed, {} skipped)",
        parsed.gates.len(),
        passed,
        failed,
        skipped,
    );

    // Surface any failed-gate detail on stderr so a CI summary picks it up.
    if !parsed.all_passed {
        for g in &parsed.gates {
            if g.status == "failed" {
                eprintln!(
                    "  {}: {}",
                    g.name,
                    g.details.as_deref().unwrap_or("(no details)")
                );
            }
        }
        return 1;
    }
    0
}

fn tally(gates: &[Gate]) -> (usize, usize, usize) {
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    for g in gates {
        match g.status.as_str() {
            "passed" => passed += 1,
            "failed" => failed += 1,
            "skipped" => skipped += 1,
            _ => {} // schema-rejected at write time; tolerated here
        }
    }
    (passed, failed, skipped)
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
            "run_id": "2026-05-18T06:00:00Z",
            "git_sha": "abc1234",
            "all_passed": true,
            "gates": [
                {"name": "fmt", "status": "passed"},
                {"name": "clippy", "status": "passed"},
                {"name": "mcp_integration_npx", "status": "skipped"},
            ],
        }));
        assert_eq!(run(f.path()), 0);
    }

    #[test]
    fn red_run_exits_one() {
        let f = write_tmp(json!({
            "version": 1,
            "run_id": "2026-05-18T06:00:00Z",
            "git_sha": "abc1234",
            "all_passed": false,
            "gates": [
                {"name": "fmt", "status": "passed"},
                {"name": "clippy", "status": "failed", "details": "warning treated as error"},
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
            "run_id": "2026-05-18T06:00:00Z",
            "git_sha": "abc1234",
            "all_passed": true,
            "gates": [{"name": "fmt", "status": "passed"}],
        }));
        assert_eq!(run(f.path()), 2);
    }

    #[test]
    fn tally_counts_each_status() {
        let gates = vec![
            Gate {
                name: "a".into(),
                status: "passed".into(),
                duration_secs: None,
                details: None,
            },
            Gate {
                name: "b".into(),
                status: "passed".into(),
                duration_secs: None,
                details: None,
            },
            Gate {
                name: "c".into(),
                status: "failed".into(),
                duration_secs: None,
                details: None,
            },
            Gate {
                name: "d".into(),
                status: "skipped".into(),
                duration_secs: None,
                details: None,
            },
        ];
        assert_eq!(tally(&gates), (2, 1, 1));
    }

    /// The bundled seed `last_run.json` must round-trip through the
    /// reader — catches a future schema change that breaks the binary
    /// before it lands in CI.
    #[test]
    fn bundled_seed_artifact_parses() {
        let path = default_artifact_path();
        // The seed may not be present on every worktree (e.g. someone
        // running the test before pulling Track C's files) — treat
        // ENOENT as a skip, not a failure.
        if !path.exists() {
            return;
        }
        let exit = run(&path);
        // The seed has all_passed: true (one skipped, rest passed).
        assert_eq!(exit, 0, "seed artifact at {} did not pass", path.display());
    }
}
