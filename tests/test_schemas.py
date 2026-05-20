"""Schema regression suite.

For each schema, a small valid+invalid corpus locks the contract. If a future
edit accidentally relaxes a constraint (e.g., removes a `oneOf`), the relevant
invalid case starts passing and this test fires.

Also exercises cross-schema `$ref` resolution (e.g., `session.envelope` →
`model_protocol/envelope`) via the shared registry from `_schema_helpers`.
"""
import json
import sys
from pathlib import Path

import jsonschema
import pytest

ROOT = Path(__file__).resolve().parent.parent
SCHEMAS = ROOT / "schemas"

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _schema_helpers import build_schema_registry, validator_for  # noqa: E402

REGISTRY = build_schema_registry()


def load(rel):
    return json.loads((SCHEMAS / rel).read_text())


def validate_with_registry(schema, doc):
    """Validate `doc` against `schema` with cross-schema $refs resolved locally."""
    v = validator_for(schema, registry=REGISTRY)
    errors = list(v.iter_errors(doc))
    if errors:
        raise errors[0]


# ---- envelope ----

def test_envelope_minimal_valid():
    schema = load("model_protocol/envelope.v1.json")
    jsonschema.validate({}, schema)  # all fields optional


def test_envelope_full_valid():
    schema = load("model_protocol/envelope.v1.json")
    jsonschema.validate({
        "claimed_changes": [{"path": "a.py", "kind": "edit", "summary": "x"}],
        "claimed_done": True,
        "grounding": [{"text_span": "s", "source": "tool:read"}],
    }, schema)


def test_envelope_bad_kind_rejected():
    schema = load("model_protocol/envelope.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "claimed_changes": [{"path": "a.py", "kind": "frobnicate", "summary": "x"}]
        }, schema)


def test_envelope_bad_grounding_source_rejected():
    schema = load("model_protocol/envelope.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "grounding": [{"text_span": "s", "source": "made_up"}]
        }, schema)


def test_envelope_version_const_pinned():
    """v60.36 H7 — envelope schema must pin `version: {const: 1}` so a v2
    envelope can be distinguished from a v1 envelope structurally. Missing
    `version` still validates (back-compat); explicit `version: 1` validates;
    `version: 2` is rejected.
    """
    schema = load("model_protocol/envelope.v1.json")
    # back-compat: missing version is OK (not in required)
    jsonschema.validate({"claimed_done": True}, schema)
    # explicit v1 is OK
    jsonschema.validate({"version": 1, "claimed_done": True}, schema)
    # v2 is rejected
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 2, "claimed_done": True}, schema)


def test_schema_format_checker_rejects_invalid_uuid_datetime_and_uri():
    """Shared validators must enforce JSON Schema `format` annotations."""
    telemetry = load("telemetry/payload.v1.json")
    bad_telemetry = {
        "version": 1,
        "channel": "usage",
        "atelier_version": "0.0.0",
        "session_uuid": "not-a-uuid",
        "sent_at": "not-a-date-time",
        "body": {"feature": "x", "count": 1},
    }
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(telemetry, bad_telemetry)

    phase_a = load("ci/phase_a_gate.v1.json")
    bad_phase_a = {
        "version": 1,
        "run_id": "not-a-date-time",
        "git_sha": "abcdef0",
        "workflow_run_url": "http://[::1",
        "all_passed": True,
        "gates": [{"name": "fmt", "status": "passed"}],
    }
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(phase_a, bad_phase_a)


def test_audit_schemas_cap_free_form_strings():
    """v60.36 H5 — audit schemas must cap every free-form string field so a
    misbehaving producer can't bloat the audit log. Probes a representative
    set: `egress.v1.json::redactions_applied[].pattern`, `mcp_egress.v1.json::url`,
    `mcp_egress.v1.json::reason`, `subprocess_egress.v1.json::destination`.
    """
    # egress: redactions_applied[].pattern capped at 512.
    egress = load("audit/egress.v1.json")
    base = {
        "version": 1,
        "kind": "model-call",
        "timestamp": "2026-05-19T00:00:00Z",
        "provider": "anthropic",
        "model_id": "claude-haiku",
        "content_hash": "sha256-abc",
        "redaction_policy_id": "default",
        "tokens": {"prompt": 1, "completion": 1},
    }
    # 512-byte pattern is OK; 513-byte rejected.
    ok = dict(base, redactions_applied=[{"pattern": "x" * 512, "count": 1}])
    jsonschema.validate(ok, egress)
    bad = dict(base, redactions_applied=[{"pattern": "x" * 513, "count": 1}])
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(bad, egress)

    # mcp_egress: url capped at 2048 + http(s) only.
    mcp = load("audit/mcp_egress.v1.json")
    base = {
        "version": 1,
        "kind": "mcp-http-request",
        "timestamp": "2026-05-19T00:00:00Z",
        "provider": "filesystem",
        "url": "https://example.test/mcp",
        "phase": "handshake",
        "outcome": "success",
    }
    jsonschema.validate(base, mcp)
    # non-http scheme rejected
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(dict(base, url="file:///etc/passwd"), mcp)
    # 2049-byte URL rejected
    big = "https://example.test/" + ("a" * 2048)
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(dict(base, url=big), mcp)

    # subprocess_egress: destination capped at 1024.
    sub = load("audit/subprocess_egress.v1.json")
    base = {
        "version": 1,
        "kind": "subprocess-egress",
        "timestamp": "2026-05-19T00:00:00Z",
        "tool_call_id": "tc-1",
        "tool_name": "shell",
        "destination": "evil.example",
        "outcome": "blocked",
        "reason": "sandbox-deny-net",
    }
    jsonschema.validate(base, sub)
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(dict(base, destination="x" * 1025), sub)


def test_ci_git_sha_accepts_mixed_case():
    """v60.36 H6 — phase_a_gate and protocol_conformance both accept mixed-case
    git SHAs. Historic artifacts and certain `gh`-CLI outputs aren't guaranteed
    lowercase.
    """
    for name in ("ci/phase_a_gate.v1.json", "ci/protocol_conformance.v1.json"):
        schema = load(name)
        # Extract the git_sha subschema to validate it in isolation
        git_sha_schema = schema["properties"]["git_sha"]
        # Mixed case is OK
        jsonschema.validate("AbC1234", git_sha_schema)
        # All lowercase is OK
        jsonschema.validate("abc1234", git_sha_schema)
        # All uppercase is OK
        jsonschema.validate("ABCDEF1", git_sha_schema)
        # Non-hex rejected
        with pytest.raises(jsonschema.ValidationError):
            jsonschema.validate("xyz1234", git_sha_schema)


# ---- session: tool_fixtures oneOf result/error ----

def _minimal_session(tool_fixtures):
    return {
        "session_uuid": "00000000-0000-0000-0000-000000000000",
        "harness_session_version": 1,
        "atelier_version": "0.0.0",
        "created_at": "2026-05-15T00:00:00Z",
        "conversation": [],
        "cost_ledger": [],
        "checkpoints": {"root": "r", "nodes": {"r": {"parent": None, "diff_ref": "d", "created_at": "2026-05-15T00:00:00Z"}}},
        "tool_fixtures": tool_fixtures,
        "memory": [],
        "plan": {"steps": []},
        "constraints": [],
        "recovery_log": [],
    }


def test_session_tool_fixture_with_result_valid():
    schema = load("session/v1.json")
    jsonschema.validate(_minimal_session({
        "c1": {"tool_name": "shell", "args": {}, "result": "ok", "captured_at": "2026-05-15T00:00:00Z"}
    }), schema)


def test_session_tool_fixture_with_error_valid():
    schema = load("session/v1.json")
    jsonschema.validate(_minimal_session({
        "c2": {"tool_name": "shell", "args": {}, "error": {"kind": "Timeout", "message": "took too long"}, "captured_at": "2026-05-15T00:00:00Z"}
    }), schema)


def test_session_tool_fixture_both_rejected():
    schema = load("session/v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(_minimal_session({
            "c": {"tool_name": "x", "args": {}, "result": 1, "error": {"kind": "Timeout", "message": "m"}, "captured_at": "2026-05-15T00:00:00Z"}
        }), schema)


def test_session_tool_fixture_neither_rejected():
    schema = load("session/v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(_minimal_session({
            "c": {"tool_name": "x", "args": {}, "captured_at": "2026-05-15T00:00:00Z"}
        }), schema)


def test_session_tool_fixture_bad_error_kind_rejected():
    schema = load("session/v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(_minimal_session({
            "c": {"tool_name": "x", "args": {}, "error": {"kind": "NotAValidKind", "message": "m"}, "captured_at": "2026-05-15T00:00:00Z"}
        }), schema)


# ---- session: subagents map (§10 delegation) ----

def _session_with_subagent(subagent_entries):
    """Build a minimal session doc that has a populated subagents map."""
    doc = _minimal_session({})
    doc["subagents"] = subagent_entries
    return doc


def test_subagent_field_validates():
    """Session JSON with populated subagents map validates against session/v1.json."""
    schema = load("session/v1.json")
    doc = _session_with_subagent({
        "sa-abc": {
            "subagent_type": "researcher",
            "description": "research async Rust",
            "started_at": "2026-05-19T10:00:00Z",
            "finished_at": "2026-05-19T10:00:05Z",
            "status": "completed",
            "max_turns": 5,
            "turns_used": 1,
            "result": "Tokio uses a work-stealing scheduler.",
            "cost_summary": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "cached_tokens": 0,
                "cost_usd": 0.001,
            },
        }
    })
    validate_with_registry(schema, doc)


def test_subagent_minimal_status_only_validates():
    """status alone satisfies the subagents entry required-fields contract."""
    schema = load("session/v1.json")
    doc = _session_with_subagent({"sa-1": {"status": "cancelled"}})
    validate_with_registry(schema, doc)


def test_subagent_description_without_parent_turn_id_validates():
    """description present, parent_turn_id absent — must validate (both are optional)."""
    schema = load("session/v1.json")
    doc = _session_with_subagent({
        "sa-2": {
            "description": "in-flight task cancelled on resume",
            "status": "cancelled",
        }
    })
    validate_with_registry(schema, doc)


def test_subagent_bad_status_rejected():
    """Unrecognised status value must fail."""
    schema = load("session/v1.json")
    doc = _session_with_subagent({"sa-3": {"status": "UNKNOWN"}})
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_subagent_extra_field_rejected():
    """additionalProperties: false — unknown field must fail."""
    schema = load("session/v1.json")
    doc = _session_with_subagent({"sa-4": {"status": "completed", "extra_field": "x"}})
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


# ---- mcp_servers: transport-conditional required fields ----

def test_mcp_servers_stdio_with_command_valid():
    schema = load("config/mcp_servers.v1.json")
    jsonschema.validate({"version": 1, "servers": [{"name": "fs", "transport": "stdio", "command": "npx"}]}, schema)


def test_mcp_servers_http_with_url_valid():
    schema = load("config/mcp_servers.v1.json")
    jsonschema.validate({"version": 1, "servers": [{"name": "ws", "transport": "http", "url": "https://x.example/mcp", "allow_net": True}]}, schema)


def test_mcp_servers_stdio_without_command_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "fs", "transport": "stdio"}]}, schema)


def test_mcp_servers_http_without_url_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "ws", "transport": "http"}]}, schema)


def test_mcp_servers_http_requires_allow_net_true():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "ws", "transport": "http", "url": "https://x.example/mcp"}]}, schema)
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "ws", "transport": "sse", "url": "https://x.example/mcp", "allow_net": False}]}, schema)


def test_mcp_servers_bad_name_pattern_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "FS-Caps", "transport": "stdio", "command": "x"}]}, schema)


def test_mcp_servers_allowed_hosts_round_trips():
    # v60.28 H5 — `allowed_hosts` is an optional `string[]`. Round-trips
    # cleanly under the schema's `additionalProperties: false` posture.
    schema = load("config/mcp_servers.v1.json")
    jsonschema.validate({
        "version": 1,
        "servers": [{
            "name": "ws", "transport": "http", "url": "https://x.example/mcp", "allow_net": True,
            "allowed_hosts": ["x.example", "y.example"],
        }],
    }, schema)


def test_mcp_servers_allowed_hosts_wrong_type_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "servers": [{
                "name": "ws", "transport": "http", "url": "https://x.example/mcp", "allow_net": True,
                "allowed_hosts": "not-an-array",
            }],
        }, schema)


# ---- schema enum sweep — every value reachable via Strategy::as_str() (H16) ----

def test_schema_strategy_enums_align_with_strategy_as_str():
    # v60.28 H16 — every `strategy` enum value across `schemas/` must be a
    # member of the canonical strategy wire-label set (mirrored by
    # `Strategy::as_str()` in `crates/atelier-core/src/protocol_strategy.rs`).
    # Pre-v60.28 `overhead.v1.json` shipped a stale `json_mode` that
    # `Strategy::as_str()` never emits. Sweep all schema files; flag any
    # `strategy` enum whose values don't match.
    allowed = {"native_tool", "json_sentinel", "regex_prose"}
    for path in SCHEMAS.rglob("*.json"):
        text = path.read_text()
        if '"strategy"' not in text:
            continue
        schema = json.loads(text)
        for enum_values in _strategy_enums(schema):
            extras = set(enum_values) - allowed
            assert not extras, f"{path} has stale strategy enum value(s) {extras}"


def _strategy_enums(node):
    """Yield every list of enum values for a property named `strategy`."""
    if isinstance(node, dict):
        for k, v in node.items():
            if k == "strategy" and isinstance(v, dict) and isinstance(v.get("enum"), list):
                yield v["enum"]
            yield from _strategy_enums(v)
    elif isinstance(node, list):
        for item in node:
            yield from _strategy_enums(item)


# ---- task_meta ----

def test_task_meta_minimal_valid():
    schema = load("workload/task_meta.v1.json")
    jsonschema.validate({"version": 1, "task_id": "t01", "expected_starting_returncode": 0, "turn_cap": 20}, schema)


def test_task_meta_language_typescript_valid():
    schema = load("workload/task_meta.v1.json")
    jsonschema.validate({
        "version": 1, "task_id": "t11", "expected_starting_returncode": 0, "turn_cap": 20,
        "language": "typescript", "test_command": ["npm", "test"],
    }, schema)


def test_task_meta_bad_language_rejected():
    schema = load("workload/task_meta.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1, "task_id": "tx", "expected_starting_returncode": 0, "turn_cap": 20,
            "language": "fortran",
        }, schema)


def test_task_meta_missing_version_rejected():
    schema = load("workload/task_meta.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"task_id": "t01", "expected_starting_returncode": 0, "turn_cap": 20}, schema)


# ---- task_checks ----

def test_task_checks_command_valid():
    schema = load("workload/task_checks.v1.json")
    jsonschema.validate({"version": 1, "checks": [
        {"name": "pytest", "command": "pytest", "expect": {"exit_code": 0}}
    ]}, schema)


def test_task_checks_file_unchanged_valid():
    schema = load("workload/task_checks.v1.json")
    jsonschema.validate({"version": 1, "checks": [
        {"name": "test untouched", "file_unchanged": "tests/test_x.py"}
    ]}, schema)


def test_task_checks_both_command_and_file_unchanged_rejected():
    schema = load("workload/task_checks.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "checks": [
            {"name": "x", "command": "x", "expect": {"exit_code": 0}, "file_unchanged": "y"}
        ]}, schema)


def test_task_checks_command_without_expect_rejected():
    schema = load("workload/task_checks.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "checks": [{"name": "x", "command": "x"}]}, schema)


def test_task_checks_empty_expect_rejected():
    schema = load("workload/task_checks.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "checks": [
            {"name": "x", "command": "x", "expect": {}}
        ]}, schema)


# ---- baselines ----

def test_baseline_minimal_valid():
    schema = load("baselines/permission_prompts.v1.json")
    jsonschema.validate({
        "version": 1,
        "captured_at": "2026-05-15T00:00:00Z",
        "baseline_harness_name": "claude-code",
        "baseline_harness_version": "0.0.0",
        "model_id": "anthropic:claude-sonnet-4-6",
        "reference_machine": "reference.md@abc123",
        "tasks": [{"task_id": "t01", "median_prompt_count": 3, "runs": [3, 3, 3]}],
    }, schema)


def test_baseline_fewer_than_3_runs_rejected():
    schema = load("baselines/permission_prompts.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "captured_at": "2026-05-15T00:00:00Z",
            "baseline_harness_name": "claude-code",
            "baseline_harness_version": "0.0.0",
            "model_id": "x",
            "reference_machine": "x",
            "tasks": [{"task_id": "t01", "median_prompt_count": 3, "runs": [3, 3]}],
        }, schema)


def test_baseline_byom_neutral():
    """The schema must accept any vendor's harness/model — not just Anthropic-shaped values."""
    schema = load("baselines/permission_prompts.v1.json")
    for name, model in [
        ("aider", "openai:gpt-4.1"),
        ("cursor-agent", "ollama:qwen2.5-coder:7b"),
        ("atelier", "anthropic:claude-opus-4-7"),
    ]:
        jsonschema.validate({
            "version": 1,
            "captured_at": "2026-05-15T00:00:00Z",
            "baseline_harness_name": name,
            "baseline_harness_version": "0.0.0",
            "model_id": model,
            "reference_machine": "reference.md@abc123",
            "tasks": [{"task_id": "t01", "median_prompt_count": 3, "runs": [3, 3, 3]}],
        }, schema)


# ---- runner_result ----

def test_runner_result_dry_run_valid():
    schema = load("workload/runner_result.v1.json")
    jsonschema.validate({
        "runner_version": 1,
        "results": [{
            "mode": "dry-run", "task_id": "t01", "ok": True,
            "expected_starting_returncode": 5,
            "starting_state": {"returncode": 5, "elapsed_s": 0.1},
        }],
    }, schema)


def test_runner_result_harness_valid():
    schema = load("workload/runner_result.v1.json")
    jsonschema.validate({
        "runner_version": 1,
        "results": [{
            "mode": "harness", "task_id": "t01", "ok": True,
            "harness": {"returncode": 0, "elapsed_s": 1.2, "timed_out": False},
            "post_state": {"returncode": 0, "elapsed_s": 0.2},
            "checks": [{"name": "pytest", "ok": True, "kind": "command", "exit_code": 0, "reason": None}],
        }],
    }, schema)


def test_runner_result_unknown_mode_rejected():
    schema = load("workload/runner_result.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "runner_version": 1,
            "results": [{"mode": "garbage", "task_id": "x", "ok": True}],
        }, schema)


# ---- cross-schema $ref: session.envelope -> model_protocol/envelope ----

def _session_skeleton(conversation=None, tool_fixtures=None):
    return {
        "session_uuid": "33333333-3333-4333-8333-333333333333",
        "harness_session_version": 1,
        "atelier_version": "0.0.0",
        "created_at": "2026-05-15T00:00:00Z",
        "conversation": conversation or [],
        "cost_ledger": [],
        "checkpoints": {"root": "r", "nodes": {"r": {"parent": None, "diff_ref": "d", "created_at": "2026-05-15T00:00:00Z"}}},
        "tool_fixtures": tool_fixtures or {},
        "memory": [],
        "plan": {"steps": []},
        "constraints": [],
        "recovery_log": [],
    }


def test_session_with_valid_envelope_passes_cross_schema():
    """The session schema $refs envelope; with the registry, validation must traverse it."""
    schema = load("session/v1.json")
    doc = _session_skeleton(conversation=[
        {"turn_id": "t1", "role": "assistant", "content": "ok",
         "envelope": {"claimed_changes": [{"path": "a.py", "kind": "edit", "summary": "x"}]}}
    ])
    validate_with_registry(schema, doc)  # must not raise


def test_session_with_invalid_envelope_kind_rejected():
    """Cross-schema check: bad envelope kind must trip the inner schema's enum."""
    schema = load("session/v1.json")
    doc = _session_skeleton(conversation=[
        {"turn_id": "t1", "role": "assistant", "content": "ok",
         "envelope": {"claimed_changes": [{"path": "a.py", "kind": "frobnicate", "summary": "x"}]}}
    ])
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_session_with_invalid_grounding_source_rejected():
    schema = load("session/v1.json")
    doc = _session_skeleton(conversation=[
        {"turn_id": "t1", "role": "assistant", "content": "ok",
         "envelope": {"grounding": [{"text_span": "s", "source": "made_up"}]}}
    ])
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_example_session_files_validate():
    """The committed examples under tests/sessions/examples/ must validate."""
    schema = load("session/v1.json")
    examples_dir = ROOT / "tests" / "sessions" / "examples"
    files = sorted(examples_dir.glob("*.json"))
    assert files, "expected committed example sessions"
    for path in files:
        validate_with_registry(schema, json.loads(path.read_text()))


def test_unregistered_schema_ref_would_fail_without_registry():
    """Sanity: confirm that the cross-schema $ref *is* doing work — without the
    registry, a session-with-envelope would fail to validate."""
    schema = load("session/v1.json")
    doc = _session_skeleton(conversation=[
        {"turn_id": "t1", "role": "assistant", "content": "ok",
         "envelope": {"claimed_changes": [{"path": "a.py", "kind": "edit", "summary": "x"}]}}
    ])
    with pytest.raises(Exception):
        # Default jsonschema.validate has no registry; the $ref to atelier.example
        # cannot be resolved. (We catch broadly because the exact exception class
        # depends on the installed referencing version.)
        jsonschema.validate(doc, schema)


# ---- tool_manifest ----

def test_tool_manifest_shell_minimal_valid():
    schema = load("config/tool_manifest.v1.json")
    validate_with_registry(schema, {
        "version": 1,
        "name": "grep",
        "side_effect_class": "local-safe",
        "input_schema": {"type": "object"},
        "implementation": {"kind": "shell", "command": "grep"},
    })


def test_tool_manifest_http_minimal_valid():
    schema = load("config/tool_manifest.v1.json")
    validate_with_registry(schema, {
        "version": 1,
        "name": "web_fetch",
        "side_effect_class": "shared-state",
        "input_schema": {"type": "object"},
        "implementation": {"kind": "http", "url": "https://api.example/fetch"},
    })


def test_tool_manifest_bad_name_rejected():
    schema = load("config/tool_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "Grep-With-Caps",
            "side_effect_class": "local-safe",
            "input_schema": {"type": "object"},
            "implementation": {"kind": "shell", "command": "grep"},
        })


def test_tool_manifest_bad_side_effect_rejected():
    schema = load("config/tool_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "side_effect_class": "very-risky",
            "input_schema": {"type": "object"},
            "implementation": {"kind": "shell", "command": "x"},
        })


def test_tool_manifest_shell_without_command_rejected():
    schema = load("config/tool_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "side_effect_class": "local-safe",
            "input_schema": {"type": "object"},
            "implementation": {"kind": "shell"},
        })


def test_tool_manifest_http_without_url_rejected():
    schema = load("config/tool_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "side_effect_class": "local-safe",
            "input_schema": {"type": "object"},
            "implementation": {"kind": "http"},
        })


def test_tool_manifest_builtin_kind_valid():
    """Built-in tools shipped in atelier-core use `implementation.kind: builtin`."""
    schema = load("config/tool_manifest.v1.json")
    validate_with_registry(schema, {
        "version": 1,
        "name": "read_file",
        "side_effect_class": "local-safe",
        "input_schema": {"type": "object"},
        "implementation": {"kind": "builtin"},
    })


def test_tool_manifest_builtin_rejects_extra_fields():
    """builtin kind takes no transport config."""
    schema = load("config/tool_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "side_effect_class": "local-safe",
            "input_schema": {"type": "object"},
            "implementation": {"kind": "builtin", "command": "x"},
        })


# ---- hook_manifest ----

def test_hook_manifest_minimal_valid():
    schema = load("config/hook_manifest.v1.json")
    validate_with_registry(schema, {
        "version": 1,
        "name": "log-pre",
        "event": "pre-tool",
        "implementation": {"kind": "shell", "command": "log"},
        "time_budget_ms": 50,
    })


def test_hook_manifest_bad_event_rejected():
    schema = load("config/hook_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "event": "after-everything",
            "implementation": {"kind": "shell", "command": "x"},
            "time_budget_ms": 50,
        })


def test_hook_manifest_zero_time_budget_rejected():
    schema = load("config/hook_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "event": "pre-tool",
            "implementation": {"kind": "shell", "command": "x"},
            "time_budget_ms": 0,
        })


def test_hook_manifest_tool_filter_accepts_list():
    schema = load("config/hook_manifest.v1.json")
    validate_with_registry(schema, {
        "version": 1,
        "name": "x",
        "event": "pre-tool",
        "tool_filter": ["write_file", "shell*"],
        "implementation": {"kind": "shell", "command": "x"},
        "time_budget_ms": 50,
    })


def test_hook_manifest_rejects_impl_timeout_ms():
    """Regression lock for N1: hooks must NOT permit implementation-level timeout_ms.
    Hooks have manifest-level time_budget_ms with warn-but-never-block semantics (§15);
    a hard impl timeout would change that contract."""
    schema = load("config/hook_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "event": "pre-tool",
            "implementation": {"kind": "shell", "command": "x", "timeout_ms": 100},
            "time_budget_ms": 50,
        })


def test_hook_manifest_rejects_http_implementation_until_executor_exists():
    schema = load("config/hook_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "event": "pre-tool",
            "implementation": {"kind": "http", "url": "https://hooks.example/atelier"},
            "time_budget_ms": 50,
        })


# ---- routing config ----

def test_routing_minimal_valid():
    schema = load("config/routing.v1.json")
    jsonschema.validate({"version": 1, "executor": "anthropic:claude-sonnet-4-6"}, schema)


def test_routing_full_valid():
    schema = load("config/routing.v1.json")
    jsonschema.validate({
        "version": 1,
        "executor": "anthropic:claude-sonnet-4-6",
        "planner": "anthropic:claude-opus-4-7",
        "critic": "ollama:qwen2.5-coder:7b",
    }, schema)


def test_routing_null_planner_and_critic_valid():
    schema = load("config/routing.v1.json")
    jsonschema.validate({
        "version": 1,
        "executor": "anthropic:claude-sonnet-4-6",
        "planner": None,
        "critic": None,
    }, schema)


def test_routing_executor_required():
    schema = load("config/routing.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "planner": "anthropic:x"}, schema)


def test_routing_bad_model_ref_format_rejected():
    schema = load("config/routing.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "executor": "no_colon_separator"}, schema)


def test_routing_bad_provider_caps_rejected():
    schema = load("config/routing.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "executor": "Anthropic:claude-sonnet"}, schema)


# ---- permission_state ----

def test_permission_state_empty_lists_valid():
    schema = load("config/permission_state.v1.json")
    jsonschema.validate({
        "version": 1,
        "scope": "repo",
        "always_allow": [],
        "always_deny": [],
    }, schema)


def test_permission_state_argv0_shape_valid():
    schema = load("config/permission_state.v1.json")
    jsonschema.validate({
        "version": 1,
        "scope": "repo",
        "always_allow": [{
            "tool": "bash",
            "shape": {"kind": "argv0-and-flagset", "argv0": "git", "flag_names": ["--short"]},
            "captured_at": "2026-05-16T00:00:00Z",
        }],
        "always_deny": [],
    }, schema)


def test_permission_state_path_glob_shape_valid():
    schema = load("config/permission_state.v1.json")
    jsonschema.validate({
        "version": 1,
        "scope": "global",
        "always_allow": [{
            "tool": "write_file",
            "shape": {"kind": "path-glob", "glob": "src/**"},
            "captured_at": "2026-05-16T00:00:00Z",
        }],
        "always_deny": [],
    }, schema)


def test_permission_state_exact_match_shape_valid():
    schema = load("config/permission_state.v1.json")
    jsonschema.validate({
        "version": 1,
        "scope": "repo",
        "always_allow": [],
        "always_deny": [{
            "tool": "write_file",
            "shape": {"kind": "exact-match", "args": {"path": ".env"}},
            "captured_at": "2026-05-16T00:00:00Z",
        }],
    }, schema)


def test_permission_state_unknown_shape_kind_rejected():
    schema = load("config/permission_state.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "scope": "repo",
            "always_allow": [{
                "tool": "x",
                "shape": {"kind": "regex", "pattern": "x"},
                "captured_at": "2026-05-16T00:00:00Z",
            }],
            "always_deny": [],
        }, schema)


def test_permission_state_bad_scope_rejected():
    schema = load("config/permission_state.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "scope": "machine",
            "always_allow": [],
            "always_deny": [],
        }, schema)


# ---- cost_ledger per-kind required fields ----

def _session_with_ledger(entries):
    return _session_skeleton() | {"cost_ledger": entries}


def test_cost_ledger_model_call_with_required_fields_passes():
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "model_call",
        "model_id": "anthropic:claude-sonnet-4-6",
        "prompt_tokens": 100,
        "completion_tokens": 30,
        "count_source": "exact",
    }])
    validate_with_registry(schema, doc)


def test_cost_ledger_model_call_missing_model_id_rejected():
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "model_call",
        "prompt_tokens": 100,
        "completion_tokens": 30,
        "count_source": "exact",
    }])
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_cost_ledger_cache_bust_requires_note():
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "cache_bust",
    }])
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_cost_ledger_cache_bust_with_note_passes():
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "cache_bust",
        "note": "fork: ck-3 -> ck-3a",
    }])
    validate_with_registry(schema, doc)


def test_cost_ledger_tool_call_requires_latency_and_tool_name():
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "tool_call",
    }])
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_cost_ledger_tool_call_missing_tool_name_rejected():
    """B11: tool_call entries must carry tool_name so replay can link to tool_fixtures."""
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "tool_call",
        "latency_ms": 45,
    }])
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_cost_ledger_tool_call_with_required_fields_passes():
    schema = load("session/v1.json")
    doc = _session_with_ledger([{
        "timestamp": "2026-05-16T00:00:00Z",
        "kind": "tool_call",
        "tool_name": "shell",
        "latency_ms": 45,
    }])
    validate_with_registry(schema, doc)


# ---- skill_manifest ----

def test_skill_manifest_minimal_valid():
    schema = load("config/skill_manifest.v1.json")
    jsonschema.validate({
        "version": 1,
        "name": "review",
        "description": "Review the diff",
        "prompt_template": "Review the diff.",
    }, schema)


def test_skill_manifest_full_valid():
    schema = load("config/skill_manifest.v1.json")
    jsonschema.validate({
        "version": 1,
        "name": "explain",
        "description": "Explain code",
        "prompt_template": "Explain ${target} at ${detail} detail.",
        "args": [
            {"name": "target", "required": True},
            {"name": "detail", "default": "normal"},
        ],
        "pinned_context": ["ATELIER.md"],
        "tools_required": ["read_file", "grep"],
        "proactive_trigger": "When the user asks an explanatory question.",
        "side_effect_class": "local-safe",
    }, schema)


def test_skill_manifest_bad_name_rejected():
    schema = load("config/skill_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "Security-Review",
            "description": "x",
            "prompt_template": "x",
        }, schema)


def test_skill_manifest_missing_template_rejected():
    schema = load("config/skill_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
        }, schema)


def test_skill_manifest_bad_side_effect_rejected():
    schema = load("config/skill_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
            "prompt_template": "x",
            "side_effect_class": "highly-risky",
        }, schema)


def test_skill_manifest_arg_with_bad_name_rejected():
    schema = load("config/skill_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
            "prompt_template": "x",
            "args": [{"name": "Bad-Arg-Name"}],
        }, schema)


def test_skill_manifest_required_arg_default_rejected():
    schema = load("config/skill_manifest.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
            "prompt_template": "x",
            "args": [{"name": "target", "required": True, "default": "src"}],
        }, schema)


def test_skill_manifest_optional_arg_default_valid():
    schema = load("config/skill_manifest.v1.json")
    jsonschema.validate({
        "version": 1,
        "name": "x",
        "description": "x",
        "prompt_template": "x",
        "args": [{"name": "target", "required": False, "default": "src"}],
    }, schema)


# ---- telemetry ----


def _telemetry(channel, body):
    return {
        "version": 1,
        "channel": channel,
        "atelier_version": "0.0.0",
        "session_uuid": "33333333-3333-4333-8333-333333333333",
        "sent_at": "2026-05-20T00:00:00Z",
        "body": body,
    }


def test_telemetry_channel_body_pairs_valid():
    schema = load("telemetry/payload.v1.json")
    validate_with_registry(schema, _telemetry("crash", {"stack": "trace", "exit_code": 1}))
    validate_with_registry(schema, _telemetry("perf", {"ledger_summary": {"total_prompt_tokens": 1}}))
    validate_with_registry(schema, _telemetry("usage", {"feature": "skill", "count": 1}))


def test_telemetry_cross_channel_body_rejected():
    schema = load("telemetry/payload.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, _telemetry("crash", {"feature": "skill", "count": 1}))


# ---- mcp_catalog ----

def test_mcp_catalog_minimal_valid():
    schema = load("config/mcp_catalog.v1.json")
    jsonschema.validate({
        "version": 1,
        "servers": [{
            "name": "filesystem",
            "display_name": "Filesystem",
            "description": "Read and write local files.",
            "transport": "stdio",
            "install": {
                "kind": "npm",
                "npm_package": "@modelcontextprotocol/server-filesystem@0.6.2",
                "command_template": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem@0.6.2", "/path"],
                },
            },
        }],
    }, schema)


def test_mcp_catalog_http_install_valid():
    schema = load("config/mcp_catalog.v1.json")
    jsonschema.validate({
        "version": 1,
        "servers": [{
            "name": "web",
            "display_name": "Web",
            "description": "Hosted web fetch.",
            "transport": "http",
            "install": {
                "kind": "http",
                "url": "https://example.com/mcp",
                "documentation_url": "https://example.com/docs",
            },
        }],
    }, schema)


def test_mcp_catalog_npm_install_without_package_rejected():
    schema = load("config/mcp_catalog.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "servers": [{
                "name": "x",
                "display_name": "X",
                "description": "x",
                "transport": "stdio",
                "install": {"kind": "npm"},
            }],
        }, schema)


def test_mcp_catalog_npm_package_must_be_version_pinned():
    schema = load("config/mcp_catalog.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "servers": [{
                "name": "filesystem",
                "display_name": "Filesystem",
                "description": "Read and write local files.",
                "transport": "stdio",
                "install": {
                    "kind": "npm",
                    "npm_package": "@modelcontextprotocol/server-filesystem",
                },
            }],
        }, schema)


def test_mcp_catalog_install_kind_mismatch_rejected():
    schema = load("config/mcp_catalog.v1.json")
    # binary install requires command_template, not npm_package
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "servers": [{
                "name": "x",
                "display_name": "X",
                "description": "x",
                "transport": "stdio",
                "install": {"kind": "binary", "npm_package": "x"},
            }],
        }, schema)


def test_mcp_catalog_requires_secrets_shape():
    schema = load("config/mcp_catalog.v1.json")
    jsonschema.validate({
        "version": 1,
        "servers": [{
            "name": "x",
            "display_name": "X",
            "description": "x",
            "transport": "http",
            "install": {"kind": "http", "url": "https://x.example"},
            "requires_secrets": [
                {"name": "api_key", "description": "API key", "where": "header", "header_name": "Authorization"}
            ],
        }],
    }, schema)


def test_mcp_catalog_env_secret_requires_env_name():
    schema = load("config/mcp_catalog.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "servers": [{
                "name": "x",
                "display_name": "X",
                "description": "x",
                "transport": "http",
                "install": {"kind": "http", "url": "https://x.example"},
                "requires_secrets": [
                    {"name": "api_key", "description": "API key", "where": "env"}
                ],
            }],
        }, schema)


# ---- subagent_type ----

def test_subagent_type_minimal_valid():
    schema = load("config/subagent_type.v1.json")
    jsonschema.validate({
        "version": 1,
        "name": "researcher",
        "description": "Read-only research sub-agent.",
        "system_prompt_addendum": "You are a research sub-agent...",
    }, schema)


def test_subagent_type_full_valid():
    schema = load("config/subagent_type.v1.json")
    validate_with_registry(schema, {
        "version": 1,
        "name": "code-reviewer",
        "description": "Independent reviewer",
        "system_prompt_addendum": "You are a reviewer...",
        "tool_allowlist": ["read_file", "grep"],
        "default_max_turns": 20,
        "side_effect_class_cap": "local-safe",
        "model_routing": {
            "version": 1,
            "executor": "anthropic:claude-opus-4-7",
        },
    })


def test_subagent_type_bad_name_rejected():
    schema = load("config/subagent_type.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "Researcher",
            "description": "x",
            "system_prompt_addendum": "x",
        }, schema)


def test_subagent_type_missing_addendum_rejected():
    schema = load("config/subagent_type.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
        }, schema)


def test_subagent_type_bad_side_effect_cap_rejected():
    schema = load("config/subagent_type.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
            "system_prompt_addendum": "x",
            "side_effect_class_cap": "irreversible-and-more",
        }, schema)


def test_subagent_type_zero_max_turns_rejected():
    schema = load("config/subagent_type.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "name": "x",
            "description": "x",
            "system_prompt_addendum": "x",
            "default_max_turns": 0,
        }, schema)


def test_subagent_type_routing_override_invalid_rejected():
    schema = load("config/subagent_type.v1.json")
    # bad model_ref pattern inside the routing override (no provider colon)
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, {
            "version": 1,
            "name": "x",
            "description": "x",
            "system_prompt_addendum": "x",
            "model_routing": {"version": 1, "executor": "no-colon-here"},
        })


# ---- session.subagents ----

def test_session_with_subagents_valid():
    schema = load("session/v1.json")
    doc = _session_skeleton()
    doc["subagents"] = {
        "sa-1": {
            "parent_turn_id": "turn-2",
            "subagent_type": "researcher",
            "started_at": "2026-05-16T00:00:00Z",
            "finished_at": "2026-05-16T00:01:00Z",
            "status": "completed",
            "max_turns": 10,
            "turns_used": 3,
            "tool_allowlist": ["read_file", "grep"],
            "conversation": [
                {"turn_id": "sa-1.turn-1", "role": "user", "content": "investigate"},
                {"turn_id": "sa-1.turn-2", "role": "assistant", "content": "done", "envelope": {"claimed_done": True}},
            ],
            "result": "summary",
            "cost_summary": {"prompt_tokens": 100, "completion_tokens": 30, "cost_usd": 0.0008},
        }
    }
    validate_with_registry(schema, doc)


def test_session_subagent_missing_status_rejected():
    """status is the only required field; omitting it must be rejected."""
    schema = load("session/v1.json")
    doc = _session_skeleton()
    doc["subagents"] = {
        "sa-1": {
            "subagent_type": "researcher",
            "started_at": "2026-05-16T00:00:00Z",
        }
    }
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_session_subagent_bad_status_rejected():
    schema = load("session/v1.json")
    doc = _session_skeleton()
    doc["subagents"] = {
        "sa-1": {
            "parent_turn_id": "t",
            "started_at": "2026-05-16T00:00:00Z",
            "status": "fine-i-guess",
        }
    }
    with pytest.raises(jsonschema.ValidationError):
        validate_with_registry(schema, doc)


def test_session_subagents_field_is_optional():
    schema = load("session/v1.json")
    # Existing minimal-success-style session without `subagents` still validates.
    doc = _session_skeleton()
    validate_with_registry(schema, doc)


# ---- additionalProperties: false sweep across every object sub-schema ----

_CONDITIONAL_KEYS = {"if", "then", "else"}


def _iter_object_subschemas(node, path="$", under_conditional=False):
    """Yield (path, subschema) pairs for every dict that has a `properties` key.

    `if`/`then`/`else` sub-schemas are skipped — they are partial-validation
    discriminators inside `allOf` and must not set `additionalProperties: false`
    (it would invalidate any document with more fields than the discriminator).
    """
    if isinstance(node, dict):
        if not under_conditional and "properties" in node and isinstance(node["properties"], dict):
            yield path, node
        for key, val in node.items():
            child_path = f"{path}.{key}"
            child_under_conditional = under_conditional or key in _CONDITIONAL_KEYS
            yield from _iter_object_subschemas(val, child_path, child_under_conditional)
    elif isinstance(node, list):
        for i, item in enumerate(node):
            yield from _iter_object_subschemas(item, f"{path}[{i}]", under_conditional)


def test_every_object_subschema_declares_additional_properties():
    """Typos in artifact JSON should be caught by validation, not silently ignored.

    Every object sub-schema that enumerates `properties` must declare its
    `additionalProperties` posture — either `false` (the default expectation)
    or an explicit schema/`true` for documented free-form payloads. A *missing*
    key is the offense; the implicit JSON Schema default is `true`, which lets
    typos through.

    Discriminator-only sub-schemas (those at `if`/`then`/`else` positions inside
    `allOf`) are intentionally exempted — adding `additionalProperties: false`
    there would break the conditional.
    """
    offenders = []
    for schema_path in sorted((ROOT / "schemas").rglob("*.json")):
        schema = json.loads(schema_path.read_text())
        for path, sub in _iter_object_subschemas(schema):
            if "additionalProperties" not in sub:
                offenders.append(f"{schema_path.relative_to(ROOT)} {path}")
    assert not offenders, (
        "object sub-schemas missing `additionalProperties` declaration: "
        + "; ".join(offenders)
    )


def test_overhead_schema_rejects_typoed_field():
    schema = load("protocol/overhead.v1.json")
    bad = {
        "version": 1,
        "measured_at": "2026-05-19T00:00:00Z",
        "providers": [{
            "provider": "mock",
            "model_id": "mock:test",
            "strategy": "native_tool",
            "median_overhead_pct": 1.0,
            "conformance_rate": 1.0,
            "median_overhead_pcnt": 2.0,
        }],
    }
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(bad, schema)


# ---- audit egress: `kind` discriminator forces mutual-exclusion ----

def test_ambiguous_audit_row_rejected_by_every_sibling():
    """Fixture is shaped like a union of all three audit row variants.

    Once `kind` is a required `const` on each sibling schema, the row must be
    rejected by ALL THREE — its missing/wrong `kind` means it satisfies none.
    Before M09, the row would silently validate against `egress.v1.json`
    (no `kind` requirement there) even while looking like a subprocess egress.
    """
    row = json.loads((ROOT / "tests" / "audit" / "ambiguous_row.json").read_text())
    for schema_rel in (
        "audit/egress.v1.json",
        "audit/subprocess_egress.v1.json",
        "audit/mcp_egress.v1.json",
    ):
        schema = load(schema_rel)
        with pytest.raises(jsonschema.ValidationError):
            jsonschema.validate(row, schema)


def test_model_call_egress_row_requires_kind():
    schema = load("audit/egress.v1.json")
    without_kind = {
        "version": 1,
        "timestamp": "2026-05-19T00:00:00Z",
        "provider": "anthropic",
        "model_id": "anthropic:claude-haiku-4-5",
        "content_hash": "sha256-abc",
        "redaction_policy_id": "default",
        "tokens": {"prompt": 1, "completion": 1},
    }
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(without_kind, schema)
    with_kind = dict(without_kind, kind="model-call")
    jsonschema.validate(with_kind, schema)
