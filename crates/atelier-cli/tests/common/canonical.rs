//! Canonical-workload fixture loader for Rust integration tests.
//!
//! Mirrors the Python rig's reader at `tests/workload/runner/runner.py`.
//! Reads `tests/workload/canonical/<task_dir>/{meta.json, prompt.md,
//! checks.json, fixture/}` and exposes typed access for Rust integration
//! tests that drive the §2.5 Runner against canonical fixtures.
//!
//! Path resolution walks up from `CARGO_MANIFEST_DIR` (the atelier-cli
//! crate root) to the workspace root, then descends into
//! `tests/workload/canonical/`. This keeps the test deterministic
//! whether `cargo test` is invoked from the workspace root or the crate
//! directory.
//!
//! Phase A — Rust integration consumer for the priority subset
//! (t01, t02, t05, t06, t10). Regex-based stdout/stderr patterns are
//! intentionally not supported: the priority subset uses only the
//! `command + exit_code(_ne) + stdout/stderr_contains + file_unchanged`
//! primitives. Patterns surface as a failing CheckResult so an
//! accidental dependency in a later fixture is loud, not silent.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Mirrors `tests/workload/canonical/<task>/meta.json`. Only the fields
/// the Rust runner uses are typed; unknown fields are silently ignored
/// because the rig may carry rig-only metadata.
#[derive(Debug, Deserialize)]
pub struct Meta {
    pub version: u32,
    pub task_id: String,
    pub title: String,
    pub priority: bool,
    pub turn_cap: usize,
    #[serde(default)]
    pub exercises: Vec<String>,
}

/// One element of the `expect` block on a command check.
#[derive(Debug, Default, Deserialize)]
pub struct ExpectSpec {
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_code_ne: Option<i32>,
    #[serde(default)]
    pub stdout_contains: Option<String>,
    #[serde(default)]
    pub stderr_contains: Option<String>,
    #[serde(default)]
    pub stdout_pattern: Option<String>,
    #[serde(default)]
    pub stderr_pattern: Option<String>,
}

/// One entry from `checks.json`. Either `command` + `expect` (run a
/// shell command and validate its outcome) OR `file_unchanged` (assert
/// the named file's bytes match the fixture baseline).
#[derive(Debug, Deserialize)]
pub struct CheckSpec {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub file_unchanged: Option<String>,
    #[serde(default)]
    pub expect: Option<ExpectSpec>,
}

#[derive(Debug, Deserialize)]
struct ChecksFile {
    #[allow(dead_code)]
    version: u32,
    checks: Vec<CheckSpec>,
}

/// A loaded canonical task ready to drive a Runner against.
pub struct CanonicalTask {
    pub task_id: String,
    pub prompt: String,
    pub fixture_dir: PathBuf,
    pub meta: Meta,
    pub checks: Vec<CheckSpec>,
}

impl CanonicalTask {
    /// Load by directory name under `tests/workload/canonical/`, e.g.
    /// `"t01_add_pure_function"`.
    pub fn load(task_dir_name: &str) -> std::io::Result<Self> {
        let task_root = canonical_dir().join(task_dir_name);

        if !task_root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("canonical task not found: {}", task_root.display()),
            ));
        }

        let meta_bytes = std::fs::read_to_string(task_root.join("meta.json"))?;
        let meta: Meta = serde_json::from_str(&meta_bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("meta.json parse: {e}"),
            )
        })?;

        let prompt = std::fs::read_to_string(task_root.join("prompt.md"))?;

        let checks_bytes = std::fs::read_to_string(task_root.join("checks.json"))?;
        let checks_file: ChecksFile = serde_json::from_str(&checks_bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("checks.json parse: {e}"),
            )
        })?;

        Ok(Self {
            task_id: meta.task_id.clone(),
            prompt,
            fixture_dir: task_root.join("fixture"),
            meta,
            checks: checks_file.checks,
        })
    }

    /// Copy `<task>/fixture/` into a fresh `TempDir`. Drop the returned
    /// handle to clean up. The tempdir is the workspace root the
    /// Runner sees during a scripted canonical run.
    pub fn copy_fixture_to_tempdir(&self) -> std::io::Result<tempfile::TempDir> {
        let td = tempfile::TempDir::new()?;
        copy_dir_recursive(&self.fixture_dir, td.path())?;
        Ok(td)
    }
}

/// Result of one `CheckSpec` after `run_checks`. The Rust runner is
/// hermetic — no shell pipelines and no Python rig involvement, so the
/// message field carries diagnostic context for test failure output.
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

impl CheckResult {
    fn ok(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            message: message.into(),
        }
    }
    fn fail(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            message: message.into(),
        }
    }
}

/// Execute every check in `task` against the given workspace root and
/// collect results. Does not panic on individual failures; the caller
/// asserts on the aggregate.
///
/// **Side effect:** removes `<workspace>/.atelier/` before running
/// checks. The Runner writes session bookkeeping (`session.json`,
/// `recovery_log`, `audit.log`, …) under `.atelier/sessions/<sid>/`
/// during a real run. Several canonical checks `grep -r` the workspace
/// — without this cleanup, the prompt persisted inside `session.json`
/// matches the grep and fails the check on agent-correct work. The
/// Python rig dodges this with `--dry-run`; the Rust integration
/// tests need an equivalent. No canonical fixture's expected state
/// includes `.atelier/`, so the cleanup is sound.
pub fn run_checks(task: &CanonicalTask, workspace: &Path) -> Vec<CheckResult> {
    let _ = std::fs::remove_dir_all(workspace.join(".atelier"));
    task.checks
        .iter()
        .map(|c| run_one_check(task, c, workspace))
        .collect()
}

/// Helper for the common assertion shape — every check must pass.
pub fn assert_all_checks_pass(results: &[CheckResult]) {
    let failed: Vec<&CheckResult> = results.iter().filter(|r| !r.passed).collect();
    if failed.is_empty() {
        return;
    }
    let mut msg = String::from("canonical checks failed:\n");
    for r in failed {
        msg.push_str(&format!("  - {}: {}\n", r.name, r.message));
    }
    panic!("{msg}");
}

fn run_one_check(task: &CanonicalTask, check: &CheckSpec, workspace: &Path) -> CheckResult {
    if let Some(rel) = &check.file_unchanged {
        let baseline = task.fixture_dir.join(rel);
        let current = workspace.join(rel);
        let baseline_bytes = std::fs::read(&baseline).ok();
        let current_bytes = std::fs::read(&current).ok();
        return if baseline_bytes == current_bytes {
            CheckResult::ok(&check.name, format!("{rel} unchanged"))
        } else {
            CheckResult::fail(&check.name, format!("{rel} changed (expected unchanged)"))
        };
    }

    let Some(cmd) = &check.command else {
        return CheckResult::fail(&check.name, "check has neither command nor file_unchanged");
    };

    let output = match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return CheckResult::fail(&check.name, format!("spawn `{cmd}` failed: {e}"));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit = output.status.code();

    let Some(expect) = &check.expect else {
        return if output.status.success() {
            CheckResult::ok(&check.name, format!("exit {exit:?} (no explicit expect)"))
        } else {
            CheckResult::fail(
                &check.name,
                format!("exit {exit:?} non-zero; stderr: {stderr}"),
            )
        };
    };

    if expect.stdout_pattern.is_some() || expect.stderr_pattern.is_some() {
        return CheckResult::fail(
            &check.name,
            "stdout_pattern/stderr_pattern not yet supported by the Rust runner; \
             add regex support before depending on these primitives",
        );
    }

    if let Some(want) = expect.exit_code {
        if exit != Some(want) {
            return CheckResult::fail(
                &check.name,
                format!("exit {exit:?} != {want}; stderr: {stderr}"),
            );
        }
    }
    if let Some(forbid) = expect.exit_code_ne {
        if exit == Some(forbid) {
            return CheckResult::fail(&check.name, format!("exit {forbid} (forbidden)"));
        }
    }
    if let Some(needle) = &expect.stdout_contains {
        if !stdout.contains(needle.as_str()) {
            return CheckResult::fail(
                &check.name,
                format!("stdout missing {needle:?}; got: {stdout}"),
            );
        }
    }
    if let Some(needle) = &expect.stderr_contains {
        if !stderr.contains(needle.as_str()) {
            return CheckResult::fail(
                &check.name,
                format!("stderr missing {needle:?}; got: {stderr}"),
            );
        }
    }

    CheckResult::ok(&check.name, format!("exit {exit:?} matched expect"))
}

/// Returns `true` when `python3 -m pytest --version` succeeds. The
/// canonical priority subset's checks shell out `python3 -m pytest` to
/// validate fixture state; tests that depend on it should skip cleanly
/// when this probe returns `false`, matching the
/// `npx_availability_probe` pattern in `mcp_integration.rs`.
pub fn python3_pytest_available() -> bool {
    std::process::Command::new("python3")
        .args(["-m", "pytest", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &dst_path)?;
        }
        // symlinks intentionally skipped — canonical fixtures don't
        // contain any; if they did, the rig already documents that the
        // runner copies into a tempdir hermetically.
    }
    Ok(())
}

/// Resolve the on-disk path to `tests/workload/canonical/`. Walks up
/// from `CARGO_MANIFEST_DIR` (the atelier-cli crate root) to the
/// workspace root.
fn canonical_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR must be set during cargo test");
    Path::new(&manifest)
        .parent() // crates/atelier-cli → crates
        .and_then(Path::parent) // crates → workspace root
        .expect("workspace root unreachable from atelier-cli manifest")
        .join("tests/workload/canonical")
}
