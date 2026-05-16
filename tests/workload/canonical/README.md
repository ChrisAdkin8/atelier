# Canonical workload — 11 tasks

The calibration source for every PROVISIONAL parameter in `coding-harness-spec.md` and the workload for the §8 `≤30%-of-baseline` permission-prompt UX target.

Each task ships with:

- `prompt.md` — the literal user message fed to the harness.
- `fixture/` — starting repo state. Copied to a tempdir by the runner; never edited in place.
- `meta.json` — task metadata (per `schemas/workload/task_meta.v1.json`): `expected_starting_returncode`, `turn_cap`, optional `language` + `test_command`.
- `checks.json` — structured mechanical assertions (per `schemas/workload/task_checks.v1.json`) the runner executes after the harness completes.
- `expected.md` — human-readable success criteria, mirrored mechanically by `checks.json`.

**The workload is the gate-unblocker.** No PROVISIONAL number in the spec can be set until this workload runs against a real harness.

## Task list

| ID  | Title                           | Exercises                                      | Priority |
|-----|---------------------------------|------------------------------------------------|----------|
| t01 | `add_pure_function`             | Single-file edit, test creation, test execution | ✓        |
| t02 | `rename_symbol_multi_file`      | Multi-file edits, cross-file refs, refactor safety | ✓        |
| t03 | `config_migration`              | Read JSON config, rewrite key names, migrate callers |          |
| t04 | `add_missing_test`              | Read existing code, infer intent, write tests   |          |
| t05 | `fix_bug_from_failing_test`     | Failure → diagnosis → fix; resist editing the test | ✓        |
| t06 | `add_cli_flag`                  | Argparse touch, help text, behavior change      | ✓        |
| t07 | `refactor_preserve_behavior`    | Behavior preservation; structural change        |          |
| t08 | `add_input_validation`          | Edge cases, error paths, parametric tests       |          |
| t09 | `migrate_signature`             | API change, caller migration to keyword-only    |          |
| t10 | `implement_from_spec`           | Spec → code; tests-first habit (LRUCache)       | ✓        |
| t11 | `add_typescript_function`       | TypeScript edit, `node --test`; §7 Tier-1 target |          |

The priority subset (`t01`, `t02`, `t05`, `t06`, `t10`) is the backend milestone gate (Phase A + B). `t11` covers TypeScript so §7 Tier-1 has a Phase B target. The other five round out full §8 calibration coverage.

## What "complete" means

A task is complete when every check in `checks.json` passes within the task's `turn_cap` (default 20, PROVISIONAL) without modifying any file flagged by a `file_unchanged` check. The harness records: turn count, tool-call count, permission-prompt count, total tokens (prompt + completion + cached), `$` cost, and wall-clock latency.

## Baseline and §8 target

Baseline values come from running this workload against a chosen reference harness on the reference machine (`tests/perf/reference.md`). The schema (`schemas/baselines/permission_prompts.v1.json`) is vendor-neutral; the §8 spec selects which harness counts as the v0.1 reference. See `baseline_procedure.md` for the capture procedure.

After Atelier implements §8, the same workload runs against Atelier with default settings. Per-task and aggregate prompt counts are recorded the same way. Target: aggregate Atelier prompt count ≤30% of aggregate baseline.

## Status

All 11 fixtures pass `--dry-run` (starting return code matches `meta.json`).

- [x] t01–t11 — runner dry-run OK
- [x] Baseline procedure (`baseline_procedure.md`) written
- [x] Comparison script (`runner/compare_baselines.py`) written
- [ ] Baseline data captured against a reference harness (file: `tests/baselines/permission_prompts.json`) — external action

## File layout per task

```
tests/workload/canonical/
  t01_add_pure_function/
    prompt.md         # user message fed to the harness
    expected.md       # human-readable success criteria
    meta.json         # task_meta schema
    checks.json       # task_checks schema (executed by the runner)
    fixture/
      utils.py
      tests/test_utils.py
      pyproject.toml
  t02_rename_symbol_multi_file/
    ...
```

The runner lives at `tests/workload/runner/runner.py`. `tests/workload/canonical/` itself is excluded from pytest collection by the root `pyproject.toml` — each task's fixture is its own mini-project and runs only inside the runner's tempdir.
