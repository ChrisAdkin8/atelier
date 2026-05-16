# Workload runner

Tools for executing canonical workload tasks against a harness and comparing prompt counts against a baseline harness.

## `runner.py`

Runs one or all canonical tasks. Two modes.

### Dry-run — fixture validation, no harness

```sh
python runner.py --task all --dry-run --summary
```

Copies each fixture to a tempdir, runs the task's `test_command` (default `python3 -m pytest --tb=short -q`), and asserts the return code matches the task's `meta.json` `expected_starting_returncode`. Catches fixture drift.

### Harness mode — real benchmarking

```sh
python runner.py --task t01 --harness-cmd "your-harness --workdir {dir} --stdin"
```

- `{dir}` in the command is replaced with the tempdir containing the fixture copy.
- The task's `prompt.md` is piped to the harness's stdin.
- After the harness exits (or hits `--harness-timeout-s`, default 300 PROVISIONAL), the task's `test_command` runs (its own timeout is 120s; both surface as `timed_out: true` in the result).
- If the harness emits a `<<<atelier-meta>>>{json}<<<end>>>` block on stdout (per `schemas/workload/atelier_meta_sentinel.v1.json`), the JSON is captured under `harness.meta` in the result.

### Output

JSON conforming to `schemas/workload/runner_result.v1.json`. Use `--out PATH` to write to a file or `--summary` for one-line-per-task.

### Exit code

Zero iff every task in the batch passed. For `--dry-run`, "passed" means actual rc matched the expected one. For `--harness-cmd`, "passed" means the harness didn't time out, the post-state `test_command` returned 0, and every `checks.json` check succeeded.

## `compare_baselines.py`

Compares an Atelier prompt-count file against a baseline-harness prompt-count file. Both conform to `schemas/baselines/permission_prompts.v1.json` (vendor-neutral — `baseline_harness_name` + `baseline_harness_version`).

```sh
python compare_baselines.py \
    --baseline tests/baselines/permission_prompts.json \
    --atelier  tests/baselines/atelier_prompts.json \
    --target-ratio 0.30
```

Reports per-task ratios + aggregate. Exits 0 iff aggregate Atelier prompt count is `≤ target-ratio × baseline`. The default `0.30` matches the spec's §8 UX target (PROVISIONAL).

The §8 reference baseline harness is chosen by the spec, not the schema. Comparing two Atelier runs (e.g., regression checks) uses the same schema with `baseline_harness_name: "atelier"` on both sides.

## Verification properties

- The `<<<atelier-meta>>>` block, if present, is validated against `schemas/workload/atelier_meta_sentinel.v1.json` after extraction. Schema violations fail the task and land in `harness.meta_schema_violation`.
- Every task's `checks.json` (per `schemas/workload/task_checks.v1.json`) is executed after the harness completes. Per-check results land in the result's `checks` array. This closes the do-nothing-exploit on tasks (t07, t08) whose starting state is already passing — structural assertions (≥4 callables in processor.py, ≥5 collected tests, etc.) catch agents that didn't actually edit anything.
- `file_unchanged` checks compute SHA-256 over the original-fixture file vs the post-harness file in the tempdir; modification or deletion is detected.
- The `test_command` (from `meta.json`) is per-task and defaults to `python3 -m pytest --tb=short -q`. Non-Python fixtures specify their own (e.g., t11 TypeScript uses `node --test tests/test_utils.ts`).

## Known limitations

- `tests/results/` referenced from `validate_artifacts.py` is not auto-populated by the runner. Use `--out PATH` to write results; organise them under that path if persistence is wanted.

## Schemas

| File | What |
|---|---|
| `schemas/workload/task_meta.v1.json` | Per-task `meta.json` |
| `schemas/workload/runner_result.v1.json` | This script's JSON output |
| `schemas/workload/atelier_meta_sentinel.v1.json` | The harness-stdout sentinel block |
| `schemas/baselines/permission_prompts.v1.json` | Vendor-neutral prompt-count file used for both the §8 reference and Atelier's own runs |
