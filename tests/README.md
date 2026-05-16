# tests/

The Atelier rig — schemas, validators, fixtures, and the workload runner. Driven by `make check` at the repo root; CI runs the same pipeline on every push/PR.

## Layers

| Layer | Files | What it does |
|---|---|---|
| Schema meta-validation | `validate_schemas.py` | Every `schemas/**.json` is itself a valid JSON Schema. |
| Artifact validation | `validate_artifacts.py` | Concrete artifacts (task metas/checks, example sessions, tool/hook/skill/subagent manifests, the bundled MCP catalog) validate against their declared schema. Fenced ```json``` blocks in `prompts/protocol_fewshot/*.md` are extracted and validated against `model_protocol/envelope.v1.json`. Cross-schema `$ref`s resolve via the local registry. |
| Schema registry | `_schema_helpers.py` | Shared `referencing.Registry` mapping every schema's `$id` to its local file. Lets `validate_artifacts.py` and `test_schemas.py` resolve cross-schema `$ref`s without network access. |
| Rig self-tests | `test_schemas.py`, `test_validators.py`, `test_runner.py` | 112 pytest tests (see breakdown below). |
| Example sessions | `sessions/examples/` | Four example session JSON files (success, tool-error, fork-and-recovery, delegation) exercising the full session schema including the envelope `$ref` and the `subagents` field. |
| Workload | `workload/` | 11 canonical fixtures + runner (with default 120 s `test_command` timeout) + baseline-comparison tool. |
| Performance reference | `perf/reference.md` | Reference machine spec (M1 Pro / 32 GB / macOS 26.4.1). |

### What the 112 self-tests cover

- Schema regression with valid + invalid corpora across every config schema.
- Cross-schema `$ref` resolution: session → envelope, subagent-type → routing, tool manifest → `_implementation.v1.json`.
- Per-kind `cost_ledger` required fields, including `tool_name` for `tool_call`.
- The `builtin` implementation kind on tool manifests.
- Vendor-neutral baseline schema (`baseline_harness_name` / `baseline_harness_version`).
- End-to-end validator invocations (subprocess) and runner internals.
- No-op-harness detection on t05 / t07; harness-smoke for every task's `checks.json`.
- Lints: `fixture/`-path leak, hook-impl `timeout_ms` regression lock, `.claude/` path-leak (BYOM directive), stale `*_file` / `*_dir` tool-name mentions.

## What's not here

- **The baseline JSON file.** `tests/baselines/permission_prompts.json` is captured by running `workload/canonical/baseline_procedure.md` against a reference harness; it doesn't yet exist (external action required).

## Running it

```sh
make check                          # everything

# Or individually:
python tests/validate_schemas.py
python tests/validate_artifacts.py
python tests/workload/runner/runner.py --task all --dry-run --summary

# Single task, single check stage:
python tests/workload/runner/runner.py --task t05 --dry-run

# Against a real harness:
python tests/workload/runner/runner.py --task t01 \
    --harness-cmd "your-harness --workdir {dir} --prompt-from-stdin"
```

See `workload/runner/README.md` for runner details.
