"""End-to-end tests for the two validator scripts.

These invoke the validators as subprocesses against the real repo, then
against synthetic broken inputs in a tempdir, to confirm pass/fail behavior.
"""
import json
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
SCHEMA_VALIDATOR = ROOT / "tests" / "validate_schemas.py"
ARTIFACT_VALIDATOR = ROOT / "tests" / "validate_artifacts.py"


def run(script):
    return subprocess.run([sys.executable, str(script)], cwd=ROOT, capture_output=True, text=True)


def test_validate_schemas_passes_on_real_repo():
    r = run(SCHEMA_VALIDATOR)
    assert r.returncode == 0, f"stderr: {r.stderr}\nstdout: {r.stdout}"
    assert "schemas valid" in r.stdout


def test_validate_artifacts_passes_on_real_repo():
    r = run(ARTIFACT_VALIDATOR)
    assert r.returncode == 0, f"stderr: {r.stderr}\nstdout: {r.stdout}"
    assert "artifacts validated" in r.stdout


def test_validate_schemas_rejects_malformed_schema(tmp_path, monkeypatch):
    """Drop a deliberately-broken schema next to a copy of the validator and assert failure."""
    # Use the validator's logic against a manually constructed bad schema.
    # Easier than copying the validator into tmp_path: import and call.
    import importlib.util

    spec = importlib.util.spec_from_file_location("validate_schemas", SCHEMA_VALIDATOR)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    bad = tmp_path / "bad.json"
    bad.write_text(json.dumps({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "not-a-type"}))
    ok, msg = mod.check_schema(bad)
    assert not ok
    assert "meta-validation failed" in msg


def test_validate_schemas_detects_invalid_json(tmp_path):
    import importlib.util

    spec = importlib.util.spec_from_file_location("validate_schemas", SCHEMA_VALIDATOR)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    bad = tmp_path / "bad.json"
    bad.write_text("{not: valid: json}")
    ok, msg = mod.check_schema(bad)
    assert not ok
    assert "invalid JSON" in msg


@pytest.fixture
def _m10_probe():
    """v60.37 D2/RIG-M2 — fixture-managed scratch dir for the M10 probe.

    pytest's fixture finalizer runs even when the test raises (Ctrl-C,
    assertion failure, pytest -x), so the previous try/finally pattern's
    orphan-dir hazard is eliminated. The probe still has to live inside
    `tests/results/` because that's where the validator's ARTIFACT_ROOTS
    looks; we just guarantee cleanup more robustly.
    """
    scratch_dir = ROOT / "tests" / "results" / "_m10_probe"
    scratch_file = scratch_dir / "synthetic.json"
    scratch_dir.mkdir(parents=True, exist_ok=True)
    scratch_file.write_text("{}", encoding="utf-8")
    yield scratch_dir
    # pytest invokes this teardown even if the test errored / was
    # interrupted via Ctrl-C (KeyboardInterrupt propagates through the
    # yield-resume normally). SIGKILL of the entire pytest process is
    # the only mode that can still leave the dir behind — that case
    # already requires manual recovery anyway.
    scratch_file.unlink(missing_ok=True)
    try:
        scratch_dir.rmdir()
    except OSError:
        # Someone else dropped a file in between; leave the dir alone.
        pass


def test_validate_artifacts_fails_on_unmatched_path(_m10_probe):
    """M10: an unrecognised JSON path inside an artifact root must exit non-zero.

    Drops a synthetic file two levels deep under `tests/results/` (the
    `tests/results/*.json` rule only matches depth 1). With no rule
    covering the path, the validator must fail loud.
    """
    r = run(ARTIFACT_VALIDATOR)
    assert r.returncode != 0, (
        f"expected non-zero exit on unmatched path; stdout: {r.stdout}"
    )
    assert "UNMATCHED" in (r.stdout + r.stderr), (
        f"expected UNMATCHED diagnostic; stderr: {r.stderr}"
    )
    assert "_m10_probe/synthetic.json" in (r.stdout + r.stderr)


def test_validate_artifacts_honours_unvalidated_annotation():
    """M10: the `# unvalidated:` annotation in the rule table opts a glob out
    of validation cleanly. The live `tests/audit/ambiguous_row.json` fixture is
    intentionally invalid against every schema; the annotation keeps the
    validator green and surfaces the file as SKIP, not OK or FAIL.
    """
    import importlib.util

    spec = importlib.util.spec_from_file_location("validate_artifacts", ARTIFACT_VALIDATOR)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    fixture_glob = "tests/audit/*.json"
    rules = [g for g, _, _ in mod.JSON_RULES]
    assert fixture_glob in rules, (
        f"expected {fixture_glob} in JSON_RULES; got {rules}"
    )
    schema_field = next(s for g, s, _ in mod.JSON_RULES if g == fixture_glob)
    assert schema_field == mod.UNVALIDATED
    r = run(ARTIFACT_VALIDATOR)
    assert r.returncode == 0, f"stderr: {r.stderr}\nstdout: {r.stdout}"
    assert "SKIP tests/audit/ambiguous_row.json" in r.stdout


def test_validate_artifacts_rejects_non_object_fenced_json(tmp_path):
    """Fenced JSON arrays/strings must not be silently skipped for envelope validation."""
    import importlib.util

    spec = importlib.util.spec_from_file_location("validate_artifacts", ARTIFACT_VALIDATOR)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    fixture = tmp_path / "fewshot.md"
    fixture.write_text("```json\n[]\n```\n", encoding="utf-8")
    schema = json.loads((ROOT / "schemas" / "model_protocol" / "envelope.v1.json").read_text())
    validator = mod.validator_for(schema, registry=mod.build_schema_registry())

    results = mod.validate_envelopes_in_markdown(fixture, validator)
    assert results
    assert results[0][0] is False
    assert "is not of type 'object'" in results[0][1]
