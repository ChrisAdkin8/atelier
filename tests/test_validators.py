"""End-to-end tests for the two validator scripts.

These invoke the validators as subprocesses against the real repo, then
against synthetic broken inputs in a tempdir, to confirm pass/fail behavior.
"""
import json
import subprocess
import sys
from pathlib import Path

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
