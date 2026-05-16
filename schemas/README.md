# Schemas

JSON Schema (Draft 2020-12) definitions for every persistent or interchange artifact in Atelier. Validated end-to-end by `tests/validate_schemas.py` (meta-validation) and `tests/validate_artifacts.py` (document-against-schema).

## Layout

| Path | Validates | Spec ref |
|---|---|---|
| `model_protocol/envelope.v1.json` | The JSON envelope a model emits per turn (claimed_changes, grounding, uncertainty, plan_update, claimed_done, constraints_acknowledged) | §2 |
| `session/v1.json` | The complete session artifact — conversation history, ledger, checkpoints, tool fixtures, memory, plan, constraints, recovery log | §14, central |
| `baselines/permission_prompts.v1.json` | Per-task prompt-count baseline data. Vendor-neutral: `baseline_harness_name` + `baseline_harness_version` slots accept any harness. The §8 calibration spec selects the v0.1 reference baseline. | §8 |
| `protocol/overhead.v1.json` | Nightly Model Protocol overhead measurements per adapter | §2 |
| `audit/egress.v1.json` | Per-remote-call record (content hash, redaction policy, tokens) | §12 |
| `telemetry/payload.v1.json` | Opt-in telemetry payloads (crash / perf / usage channels) | §13 |
| `config/permission_shapes.v1.json` | Per-tool permission-learning shape grouping | §8 |
| `config/mcp_servers.v1.json` | MCP server registration manifest — declares stdio/HTTP/SSE servers Atelier launches or connects to at session start | §15 |
| `config/tool_manifest.v1.json` | Tool manifest — registers a built-in, custom, or user-supplied tool (name, input/output schemas, side-effect class, `shell`/`http`/`builtin` implementation). Bundled built-ins at `crates/atelier-core/tools/`; user-supplied examples in `examples/tools/`. Implementation `oneOf` lives in `_implementation.v1.json`. | §15 |
| `config/_implementation.v1.json` | **Shared sub-schema** — implementation `oneOf` for tools: `shell` (subprocess + optional `timeout_ms`), `http` (POST to URL), `builtin` (handled inside atelier-core; no transport config). Referenced via `$ref` from `tool_manifest.v1.json`. *Not* referenced by `hook_manifest.v1.json` — hooks inline an equivalent `oneOf` minus `timeout_ms` so the tool-only field cannot leak in (hooks use manifest-level `time_budget_ms`, warn-but-never-block). Filename prefixed `_` to mark it as a shared internal schema. | §15 |
| `config/hook_manifest.v1.json` | Hook manifest — registers a pre-tool / post-tool / on-verify-* hook (event, optional tool_filter, implementation, time_budget_ms). Implementation `oneOf` is intentionally inlined (not shared) to keep `timeout_ms` out. Examples in `examples/hooks/`. | §15 |
| `config/routing.v1.json` | Per-task model routing — assigns `<provider>:<model>` strings to the executor/planner/critic roles. Per-repo `.atelier/routing.json` overrides global `~/.atelier/routing.json`. Example: `examples/config/routing.v1.json`. | §1 |
| `config/permission_state.v1.json` | Persistent permission-learning state — `.atelier/permissions.json` (per-repo) and `~/.atelier/permissions.json` (global) lists of always-allow / always-deny shape entries. Example: `examples/config/permissions.v1.json`. | §8 |
| `config/skill_manifest.v1.json` | Skill manifest — registers a user- or agent-invocable `/<name>` procedure. Bundled skills at `crates/atelier-core/skills/`; user overrides at `.atelier/skills/`. Example: `examples/skills/explain.v1.json`. | §15 |
| `config/mcp_catalog.v1.json` | MCP server catalog — curated metadata about well-known servers for the GUI's "Browse catalog". Bundled at `crates/atelier-core/catalog/mcp_servers.json`; user override at `~/.atelier/catalog.json`. | §15 |
| `config/subagent_type.v1.json` | Sub-agent type manifest — registers a named sub-agent the parent can spawn via the `spawn_subagent` tool. Bundled types at `crates/atelier-core/subagents/`; user overrides at `.atelier/subagents/`. Example: `examples/subagents/code-reviewer.v1.json`. | §10 delegation |
| `workload/task_meta.v1.json` | Per-canonical-task metadata (expected starting rc, turn cap, priority, optional language + test_command) | rig |
| `workload/task_checks.v1.json` | Per-task structured mechanical-check list — runnable form of expected.md's checks. `command + expect` or `file_unchanged`. | rig |
| `workload/runner_result.v1.json` | Output of `tests/workload/runner/runner.py` — includes per-check results + sentinel violation field | rig |
| `workload/atelier_meta_sentinel.v1.json` | Optional JSON block a harness emits between `<<<atelier-meta>>>...<<<end>>>` on stdout for telemetry. Validated by the runner after extraction. | rig |

## Versioning

- File names embed a version (`.v1.json`). New versions ship as new files; old files stay readable.
- The spec, the session schema, and the protocol envelope each have independent version streams; their compatibility matrix is at `versions.md`.
- Migration tooling for breaking session-schema changes is not yet written. The contract: any breaking schema bump ships with a one-way migration script in `schemas/migrations/`.

## Conventions

- Every schema has `$schema` and `$id`. The `$id` URLs use the placeholder host `atelier.example/` — the validators do not currently resolve `$ref` across these URLs at runtime, only within-file.
- `additionalProperties: false` is the default where the shape is closed (e.g., envelope fields, task meta). Where the shape is intentionally open (telemetry sentinel payload), `additionalProperties: true` is explicit.

## Validating

```sh
make schemas      # meta-validate every schema file
make artifacts    # validate concrete artifacts (meta.json, baselines, etc.) against schemas
```
