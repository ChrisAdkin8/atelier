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


# ---- mcp_servers: transport-conditional required fields ----

def test_mcp_servers_stdio_with_command_valid():
    schema = load("config/mcp_servers.v1.json")
    jsonschema.validate({"version": 1, "servers": [{"name": "fs", "transport": "stdio", "command": "npx"}]}, schema)


def test_mcp_servers_http_with_url_valid():
    schema = load("config/mcp_servers.v1.json")
    jsonschema.validate({"version": 1, "servers": [{"name": "ws", "transport": "http", "url": "https://x.example/mcp"}]}, schema)


def test_mcp_servers_stdio_without_command_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "fs", "transport": "stdio"}]}, schema)


def test_mcp_servers_http_without_url_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({"version": 1, "servers": [{"name": "ws", "transport": "http"}]}, schema)


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
            "name": "ws", "transport": "http", "url": "https://x.example/mcp",
            "allowed_hosts": ["x.example", "y.example"],
        }],
    }, schema)


def test_mcp_servers_allowed_hosts_wrong_type_rejected():
    schema = load("config/mcp_servers.v1.json")
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate({
            "version": 1,
            "servers": [{
                "name": "ws", "transport": "http", "url": "https://x.example/mcp",
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
                "npm_package": "@modelcontextprotocol/server-filesystem",
                "command_template": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"],
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
                {"name": "api_key", "description": "API key", "where": "header"}
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


def test_session_subagent_missing_required_rejected():
    schema = load("session/v1.json")
    doc = _session_skeleton()
    doc["subagents"] = {
        "sa-1": {
            "subagent_type": "researcher",
            "started_at": "2026-05-16T00:00:00Z",
            "status": "completed",
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
