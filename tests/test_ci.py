"""Linting tests for `.github/workflows/*.yml`.

Covers:
- M12: every workflow that pushes to `main` must rebase first and declare a
  `concurrency:` block.
- M13: every `uses:` value must be SHA-pinned (full 40-char hex).
- M14: every workflow must declare a top-level `permissions:` block; jobs that
  need write access opt in per-job.
"""
import re
import os
import subprocess
import sys
from pathlib import Path

import pytest

try:
    import yaml
except ImportError:
    pytest.skip("PyYAML not installed; the rig deps include it via pyproject.toml [rig]", allow_module_level=True)

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS_DIR = ROOT / ".github" / "workflows"

SHA_PIN_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[^@\s]+)?@[a-f0-9]{40}( |$)")


def _workflow_files():
    return sorted({*WORKFLOWS_DIR.glob("*.yml"), *WORKFLOWS_DIR.glob("*.yaml")})


def _parse(path: Path) -> dict:
    return yaml.safe_load(path.read_text())


def _flatten_uses(doc):
    """Yield every step- and job-level `uses:` string in a workflow doc."""
    for job in (doc.get("jobs") or {}).values():
        if not isinstance(job, dict):
            continue
        if isinstance(job.get("uses"), str):
            yield job["uses"]
        for step in job.get("steps", []) or []:
            if isinstance(step, dict) and "uses" in step:
                yield step["uses"]


def test_workflow_dir_has_files():
    files = _workflow_files()
    assert files, f"no workflows found in {WORKFLOWS_DIR}"


def test_actions_are_sha_pinned():
    """M13: every `uses:` value must be SHA-pinned (full 40-char hex)."""
    offenders = []
    for path in _workflow_files():
        for use in _flatten_uses(_parse(path)):
            if not SHA_PIN_RE.match(use):
                offenders.append(f"{path.name}: {use}")
    assert not offenders, (
        "workflow `uses:` entries must be SHA-pinned; offenders: "
        + "; ".join(offenders)
    )


def test_flatten_uses_includes_job_level_reusable_workflows():
    doc = {"jobs": {"call": {"uses": "owner/repo/.github/workflows/reuse.yml@v1"}}}
    assert list(_flatten_uses(doc)) == ["owner/repo/.github/workflows/reuse.yml@v1"]
    assert not SHA_PIN_RE.match(next(_flatten_uses(doc)))


def test_workflow_files_include_yaml_extension(tmp_path, monkeypatch):
    workflow = tmp_path / "bad.yaml"
    workflow.write_text("name: bad\n", encoding="utf-8")
    monkeypatch.setattr(sys.modules[__name__], "WORKFLOWS_DIR", tmp_path)
    assert _workflow_files() == [workflow]


def _phase_b_compose_python() -> str:
    workflow = _parse(WORKFLOWS_DIR / "nightly_phase_b_gate.yml")
    for step in workflow["jobs"]["measure"]["steps"]:
        if step.get("id") == "compose":
            run = step["run"]
            start = run.index("python3 - <<'PY'") + len("python3 - <<'PY'")
            end = run.index("\nPY", start)
            return run[start:end].strip()
    raise AssertionError("compose step not found")


def test_phase_b_compose_honours_nonzero_live_exit_with_summary(tmp_path):
    summary = tmp_path / "phase_b_summary.json"
    summary.write_text("[]", encoding="utf-8")
    output = tmp_path / "github_output"
    (tmp_path / "tests" / "phase_b_gate").mkdir(parents=True)

    subprocess.run(["git", "init", "--quiet"], cwd=tmp_path, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=tmp_path, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=tmp_path, check=True)
    (tmp_path / "README").write_text("x", encoding="utf-8")
    subprocess.run(["git", "add", "README"], cwd=tmp_path, check=True)
    subprocess.run(["git", "commit", "--quiet", "-m", "init"], cwd=tmp_path, check=True)

    env = os.environ | {
        "PHASE_B_SKIPPED": "false",
        "PHASE_B_SUMMARY_PATH": str(summary),
        "PHASE_B_EXIT_CODE": "7",
        "CALIBRATION_PHASE": "true",
        "PHASE_B_FLOOR": "0.95",
        "RUN_URL": "https://github.com/example/repo/actions/runs/1",
        "GITHUB_OUTPUT": str(output),
    }
    r = subprocess.run(
        [sys.executable, "-c", _phase_b_compose_python()],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
    )
    assert r.returncode == 0, f"stderr: {r.stderr}\nstdout: {r.stdout}"
    payload = yaml.safe_load((tmp_path / "tests" / "phase_b_gate" / "last_run.json").read_text())
    assert payload["all_passed"] is False
    assert payload["status"] == "red"
    assert "all_passed=false" in output.read_text()


def test_every_workflow_has_top_level_permissions():
    """M14: every workflow must declare a top-level `permissions:` block."""
    offenders = []
    for path in _workflow_files():
        doc = _parse(path)
        if "permissions" not in doc:
            offenders.append(path.name)
    assert not offenders, (
        "workflows missing top-level `permissions:` block: " + ", ".join(offenders)
    )


def test_check_yml_top_level_permissions_is_read_only():
    """M14: the per-PR `check.yml` must default to `contents: read`."""
    doc = _parse(WORKFLOWS_DIR / "check.yml")
    perms = doc.get("permissions")
    assert perms == {"contents": "read"}, (
        f"expected check.yml top-level permissions = {{contents: read}}, got {perms!r}"
    )


# v60.37 D6/RIG-M6 — discover nightly workflows dynamically. A future
# `nightly_*.yml` (e.g. the Phase B Track B live-OpenAI gate) no longer
# requires extending three hand-maintained literal lists — it picks up
# the new workflow on first commit. Workflows that don't push to main
# can opt out via `nightly_dummy_*.yml` (none today).
def _nightly_workflows():
    return [p.name for p in WORKFLOWS_DIR.glob("nightly_*.yml")]


@pytest.mark.parametrize("name", _nightly_workflows())
def test_nightly_workflows_have_concurrency_block(name):
    """M12: every nightly that commits to `main` must declare a `concurrency:`
    block grouping it with its siblings, so two nightlies can't race the same
    git ref.
    """
    doc = _parse(WORKFLOWS_DIR / name)
    conc = doc.get("concurrency")
    assert isinstance(conc, dict), f"{name}: missing `concurrency:` block"
    assert conc.get("group") == "nightly-artifact-commits", (
        f"{name}: expected concurrency.group=nightly-artifact-commits, got {conc.get('group')!r}"
    )
    assert conc.get("cancel-in-progress") is False, (
        f"{name}: cancel-in-progress must be false so the earlier run can complete its push"
    )


@pytest.mark.parametrize("name", _nightly_workflows())
def test_nightly_workflows_rebase_before_push(name):
    """M12: the commit step in every nightly must `git pull --rebase origin main`
    before pushing, and on rebase failure must abort the workflow with `exit 1`.
    """
    text = (WORKFLOWS_DIR / name).read_text()
    assert "git pull --rebase origin main" in text, (
        f"{name}: missing `git pull --rebase origin main` before push"
    )
    # Failure path must surface an error and abort, not silently continue.
    assert "rebase --abort" in text and "exit 1" in text, (
        f"{name}: rebase failure path missing — must `rebase --abort` and `exit 1`"
    )


# ---- v60.36 H1/H2 — privilege separation across the nightly commit boundary --
#
# The nightlies commit refreshed gate artifacts back to `main`. Before
# v60.36, a single job both installed transitive deps (`pip install ".[rig]"`,
# Cargo registry resolution) and held `${{ secrets.GITHUB_TOKEN }}` with
# `contents: write`. A compromise of any transitive dep granted push access
# to a protected branch. v60.36 splits the workflows into a `measure` job
# (no token, no write permission) and a `commit` job (write permission,
# but only `actions/checkout` + `actions/download-artifact` + stock git
# — no dependency resolution). These tests lock in the split.

_DEP_INSTALL_TOKENS = (
    "pip install",
    "cargo test",
    "cargo build",
    "cargo run",
    "cargo clippy",
    "cargo fmt",
    "npm install",
    "npm ci",
    "yarn install",
    "pnpm install",
    "make check",
    "make schemas",
    "make artifacts",
    "make rig-tests",
)


@pytest.mark.parametrize("name", _nightly_workflows())
def test_nightly_workflows_default_to_read_only_permissions(name):
    """v60.36 H1: every nightly that commits to `main` must default the top-level
    `permissions:` to `contents: read`. Jobs that need write permission opt in
    per-job, so transitive-dep compromise in a dependency-installing job can't
    push back to main.
    """
    doc = _parse(WORKFLOWS_DIR / name)
    perms = doc.get("permissions")
    assert perms == {"contents": "read"}, (
        f"{name}: expected top-level permissions = {{contents: read}}, got {perms!r}; "
        "see v60.36 H1 privilege-split"
    )


@pytest.mark.parametrize("name", _nightly_workflows())
def test_nightly_write_jobs_do_not_install_deps(name):
    """v60.36 H1/H2: any job with `permissions: contents: write` must not run
    any dependency-installing step (pip install, cargo test/build, npm install,
    make check/schemas/artifacts/rig-tests, etc.). The commit job is restricted
    to `actions/checkout` + `actions/download-artifact` + stock `git`.

    Without this gate, a malicious transitive dep installed in the commit job
    would run with `contents: write` and a valid `GITHUB_TOKEN`.
    """
    doc = _parse(WORKFLOWS_DIR / name)
    offenders = []
    for job_name, job in (doc.get("jobs") or {}).items():
        job_perms = job.get("permissions") or {}
        if job_perms.get("contents") != "write":
            continue
        for step in job.get("steps", []) or []:
            run = step.get("run") if isinstance(step, dict) else None
            if not run:
                continue
            for token in _DEP_INSTALL_TOKENS:
                if token in run:
                    offenders.append(f"{name}: job `{job_name}` step runs `{token}`")
    assert not offenders, (
        "jobs with `contents: write` must not install untrusted dependencies; "
        "offenders: " + "; ".join(offenders)
    )


def test_every_job_declares_timeout_minutes():
    """v60.37 C1/CI-3 — every job in every workflow must declare
    `timeout-minutes:` so a hung step can't burn the GitHub-imposed 6-hour
    default. Surfaces hangs as workflow failures within a bounded
    interval instead of letting them sit silently.
    """
    offenders = []
    for path in _workflow_files():
        doc = _parse(path)
        for job_name, job in (doc.get("jobs") or {}).items():
            if "timeout-minutes" not in job:
                offenders.append(f"{path.name}: job `{job_name}`")
    assert not offenders, (
        "workflow jobs missing `timeout-minutes:`; offenders: "
        + "; ".join(offenders)
    )


def test_check_yml_declares_concurrency_group():
    """v60.37 C2/CI-4 — `check.yml` must declare a workflow-level
    `concurrency:` group with `cancel-in-progress: true` so two rapid
    pushes to the same PR don't duplicate the matrix run.
    """
    doc = _parse(WORKFLOWS_DIR / "check.yml")
    conc = doc.get("concurrency")
    assert isinstance(conc, dict), "check.yml: missing `concurrency:` block"
    assert conc.get("cancel-in-progress") is True, (
        "check.yml: concurrency.cancel-in-progress must be true so stale runs are cancelled"
    )
    # Group must contain `github.ref` so distinct refs don't collide.
    group = conc.get("group", "")
    assert "github.ref" in group, (
        f"check.yml: concurrency.group must include `github.ref`; got {group!r}"
    )


@pytest.mark.parametrize("name", _nightly_workflows())
def test_nightly_has_separate_commit_job(name):
    """v60.36 H1: every nightly must have a dedicated commit job that holds
    `contents: write` — confirming the privilege split actually shipped, not
    just the read-only default.
    """
    doc = _parse(WORKFLOWS_DIR / name)
    writers = [
        n for n, j in (doc.get("jobs") or {}).items()
        if (j.get("permissions") or {}).get("contents") == "write"
    ]
    assert len(writers) == 1, (
        f"{name}: expected exactly one job with `permissions: contents: write`; "
        f"found {writers!r}"
    )
