#!/usr/bin/env python3
"""Validate every JSON Schema in `schemas/` is itself a valid schema.

Usage:
  python tests/validate_schemas.py

Exit code 0 if all schemas pass meta-validation. Non-zero otherwise.
This is the validator the spec's phase-gate schema-validation step calls.
"""
import json
import sys
from pathlib import Path

try:
    import jsonschema
    from jsonschema.validators import validator_for
except ImportError:
    print("jsonschema not installed; install with: pip install jsonschema", file=sys.stderr)
    sys.exit(2)

SCHEMAS_DIR = Path(__file__).resolve().parent.parent / "schemas"


def check_schema(path: Path) -> tuple[bool, str]:
    try:
        schema = json.loads(path.read_text())
    except json.JSONDecodeError as e:
        return False, f"invalid JSON: {e}"
    if not isinstance(schema, dict):
        return False, "top-level must be a JSON object"
    Validator = validator_for(schema)
    try:
        Validator.check_schema(schema)
    except jsonschema.exceptions.SchemaError as e:
        return False, f"meta-validation failed: {e.message}"
    return True, "ok"


def main() -> int:
    if not SCHEMAS_DIR.is_dir():
        print(f"schemas dir not found: {SCHEMAS_DIR}", file=sys.stderr)
        return 1
    files = sorted(SCHEMAS_DIR.rglob("*.json"))
    if not files:
        print(f"no .json schemas under {SCHEMAS_DIR}", file=sys.stderr)
        return 1
    failures = []
    for path in files:
        ok, msg = check_schema(path)
        rel = path.relative_to(SCHEMAS_DIR.parent)
        if ok:
            print(f"OK   {rel}")
        else:
            print(f"FAIL {rel}: {msg}", file=sys.stderr)
            failures.append((rel, msg))
    print(f"\n{len(files) - len(failures)}/{len(files)} schemas valid")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
