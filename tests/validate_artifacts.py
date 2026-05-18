#!/usr/bin/env python3
"""Validate concrete artifacts in the repo against their declared schemas.

Distinct from tests/validate_schemas.py, which only meta-validates the schemas themselves.

Currently validates:
  - Every meta.json under tests/workload/canonical/ vs schemas/workload/task_meta.v1.json
  - Every checks.json under tests/workload/canonical/ vs schemas/workload/task_checks.v1.json
  - Every example session under tests/sessions/examples/ vs schemas/session/v1.json (with cross-schema $ref resolution)
  - Every baseline file under tests/baselines/ (when present) vs schemas/baselines/permission_prompts.v1.json
  - Every protocol overhead file under tests/protocol/ (when present) vs schemas/protocol/overhead.v1.json
  - Every runner result file under tests/results/ (when present) vs schemas/workload/runner_result.v1.json
  - Every Phase A nightly gate result under tests/phase_a_gate/ vs schemas/ci/phase_a_gate.v1.json
  - Fenced ```json blocks inside prompts/protocol_fewshot/*.md vs schemas/model_protocol/envelope.v1.json

Exit code 0 if all artifacts pass. Non-zero otherwise.
"""
import json
import re
import sys
from pathlib import Path

try:
    import jsonschema  # noqa: F401  (imported transitively via _schema_helpers)
except ImportError:
    print("jsonschema not installed; pip install jsonschema", file=sys.stderr)
    sys.exit(2)

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _schema_helpers import build_schema_registry, validator_for  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent

# (glob, schema rel-path, description)
JSON_RULES = [
    ("tests/workload/canonical/*/meta.json", "schemas/workload/task_meta.v1.json", "task meta"),
    ("tests/workload/canonical/*/checks.json", "schemas/workload/task_checks.v1.json", "task checks"),
    ("tests/sessions/examples/*.json", "schemas/session/v1.json", "session"),
    ("examples/tools/*.json", "schemas/config/tool_manifest.v1.json", "tool manifest"),
    ("crates/atelier-core/tools/*.json", "schemas/config/tool_manifest.v1.json", "bundled built-in tool"),
    ("examples/hooks/*.json", "schemas/config/hook_manifest.v1.json", "hook manifest"),
    ("examples/config/routing*.json", "schemas/config/routing.v1.json", "routing config"),
    ("examples/config/permissions*.json", "schemas/config/permission_state.v1.json", "permission state"),
    ("examples/config/dod*.json", "schemas/config/dod.v1.json", "DoD config"),
    ("examples/skills/*.json", "schemas/config/skill_manifest.v1.json", "skill manifest"),
    ("crates/atelier-core/skills/*.json", "schemas/config/skill_manifest.v1.json", "bundled skill"),
    ("examples/subagents/*.json", "schemas/config/subagent_type.v1.json", "subagent type"),
    ("crates/atelier-core/subagents/*.json", "schemas/config/subagent_type.v1.json", "bundled subagent"),
    ("crates/atelier-core/catalog/mcp_servers.json", "schemas/config/mcp_catalog.v1.json", "MCP catalog"),
    ("tests/baselines/*.json", "schemas/baselines/permission_prompts.v1.json", "baseline data"),
    ("tests/protocol/*.json", "schemas/protocol/overhead.v1.json", "protocol overhead"),
    ("tests/results/*.json", "schemas/workload/runner_result.v1.json", "runner result"),
    ("tests/phase_a_gate/*.json", "schemas/ci/phase_a_gate.v1.json", "phase A gate result"),
]

# Markdown files whose fenced ```json blocks should validate against an envelope schema.
ENVELOPE_RULES = [
    ("prompts/protocol_fewshot/*.md", "schemas/model_protocol/envelope.v1.json", "fewshot envelope"),
]

FENCED_JSON_RE = re.compile(r"```json\s*\n(.*?)\n```", re.DOTALL)


def load_schema(path):
    return json.loads((ROOT / path).read_text())


def validate_json_file(path, validator, desc):
    try:
        data = json.loads(path.read_text())
    except json.JSONDecodeError as e:
        return False, f"invalid JSON: {e}"
    errors = sorted(validator.iter_errors(data), key=lambda e: list(e.path))
    if errors:
        return False, "; ".join(f"{list(e.path)}: {e.message}" for e in errors)
    return True, "ok"


def validate_envelopes_in_markdown(path, validator):
    text = path.read_text()
    blocks = FENCED_JSON_RE.findall(text)
    if not blocks:
        return [(False, "no ```json blocks found")]
    results = []
    for i, block in enumerate(blocks, 1):
        try:
            data = json.loads(block)
        except json.JSONDecodeError as e:
            results.append((False, f"block {i}: invalid JSON: {e}"))
            continue
        if not isinstance(data, dict):
            results.append((True, f"block {i}: non-object (skipped)"))
            continue
        errors = sorted(validator.iter_errors(data), key=lambda e: list(e.path))
        if errors:
            results.append((False, f"block {i}: " + "; ".join(f"{list(e.path)}: {e.message}" for e in errors)))
        else:
            results.append((True, f"block {i}: ok"))
    return results


def main():
    total = 0
    failures = []
    registry = build_schema_registry()  # build once; reuse across rules

    for glob, schema_rel, desc in JSON_RULES:
        matches = sorted(ROOT.glob(glob))
        if not matches:
            print(f"--   no {desc} files matched {glob}")
            continue
        schema = load_schema(schema_rel)
        validator = validator_for(schema, registry=registry)
        for artifact in matches:
            total += 1
            ok, msg = validate_json_file(artifact, validator, desc)
            rel = artifact.relative_to(ROOT)
            if ok:
                print(f"OK   {rel}")
            else:
                print(f"FAIL {rel}: {msg}", file=sys.stderr)
                failures.append((rel, msg))

    for glob, schema_rel, desc in ENVELOPE_RULES:
        # README files document the directory; they aren't example files.
        matches = sorted(p for p in ROOT.glob(glob) if p.name.lower() != "readme.md")
        if not matches:
            print(f"--   no {desc} files matched {glob}")
            continue
        schema = load_schema(schema_rel)
        validator = validator_for(schema, registry=registry)
        for artifact in matches:
            rel = artifact.relative_to(ROOT)
            block_results = validate_envelopes_in_markdown(artifact, validator)
            for ok, msg in block_results:
                total += 1
                if ok:
                    print(f"OK   {rel} [{desc}] {msg}")
                else:
                    print(f"FAIL {rel} [{desc}] {msg}", file=sys.stderr)
                    failures.append((rel, msg))

    print(f"\n{total - len(failures)}/{total} artifacts validated")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
