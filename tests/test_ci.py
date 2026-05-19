"""Linting tests for `.github/workflows/*.yml`.

Covers:
- M12: every workflow that pushes to `main` must rebase first and declare a
  `concurrency:` block.
- M13: every `uses:` value must be SHA-pinned (full 40-char hex).
- M14: every workflow must declare a top-level `permissions:` block; jobs that
  need write access opt in per-job.
"""
import re
from pathlib import Path

import pytest

try:
    import yaml
except ImportError:
    pytest.skip("PyYAML not installed; the rig deps include it via pyproject.toml [rig]", allow_module_level=True)

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS_DIR = ROOT / ".github" / "workflows"

SHA_PIN_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[a-f0-9]{40}( |$)")


def _workflow_files():
    return sorted(WORKFLOWS_DIR.glob("*.yml"))


def _parse(path: Path) -> dict:
    return yaml.safe_load(path.read_text())


def _flatten_uses(doc):
    """Yield every `uses:` string in a workflow doc."""
    for job in (doc.get("jobs") or {}).values():
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


@pytest.mark.parametrize("name", [
    "nightly_phase_a_gate.yml",
    "nightly_phase_b_gate.yml",
    "nightly_protocol_overhead.yml",
])
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


@pytest.mark.parametrize("name", [
    "nightly_phase_a_gate.yml",
    "nightly_phase_b_gate.yml",
    "nightly_protocol_overhead.yml",
])
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
