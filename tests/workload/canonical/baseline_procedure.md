# Baseline measurement procedure

How to capture the prompt-count baseline that backs §8's `≤30%-of-baseline` UX target. Run once per Atelier release that intends to claim the target, and again whenever the reference harness or reference machine changes meaningfully.

## Inputs

- **Reference machine** — described at `tests/perf/reference.md`.
- **Reference harness** — chosen by spec §8 for the current Atelier version. Pin the exact version string and record it in `tests/baselines/permission_prompts.json` under `baseline_harness_name` + `baseline_harness_version`.
- **Model** — provider-prefixed model identifier (e.g., `anthropic:claude-sonnet-4-6`, `openai:gpt-4.1`); recorded as `model_id`.
- **Task list** — all 11 canonical tasks under `tests/workload/canonical/t01_…` through `t11_…`.

## Procedure

For each task `tNN`:

1. Copy `tests/workload/canonical/tNN/fixture/` to a clean temp directory.
2. Start a fresh session of the reference harness in that directory (no prior context, default settings, default permissions).
3. Paste the contents of `prompt.md` as the first user message.
4. Let the harness run until the agent declares completion or hits the task's `meta.json` `turn_cap` (default 20).
5. Run the mechanical checks from `checks.json` against the temp directory (the workload runner does this automatically when invoked with `--harness-cmd`).
6. Record per-task:
   - `task_id`
   - per-run `prompt_count` (every UI element requiring a click to proceed; multi-choice prompts count once; auto-approved actions count zero)
   - `turn_count`
   - `total_tokens` (if surfaced by the harness)
   - `wall_clock_s`
7. Repeat steps 1–6 **three times** per task. Discard runs that crashed or hit the turn cap; if a task fails this way ≥2 of 3 runs, mark it `unmeasured` and proceed.

## Output

Write `tests/baselines/permission_prompts.json` matching `schemas/baselines/permission_prompts.v1.json`. Required fields:

- `version: 1`
- `captured_at`
- `baseline_harness_name` + `baseline_harness_version`
- `model_id`
- `reference_machine`
- For each task: `task_id`, `median_prompt_count`, `runs` (the per-run prompt counts).

Validate before commit:

```sh
python3 tests/validate_artifacts.py   # picks up the file via the baselines glob
```

## Counting rules

- A "permission prompt" is any UI block requiring a click to proceed.
- A prompt offering "always allow this session" counts as one prompt, regardless of subsequent auto-approved actions.
- A multi-choice prompt (e.g., "approve / approve always / deny") counts as one.
- Auto-approved actions count as zero.

## Atelier comparison

Once §8 is implemented, run the same workload against Atelier with default settings. Write `tests/baselines/atelier_prompts.json` using the same schema (`baseline_harness_name: "atelier"`). Compare via:

```sh
python tests/workload/runner/compare_baselines.py \
    --baseline tests/baselines/permission_prompts.json \
    --atelier  tests/baselines/atelier_prompts.json \
    --target-ratio 0.30
```

Exit 0 iff aggregate Atelier ≤ 30% of aggregate baseline.

## When to re-capture

- Reference harness ships a major release that changes default permission behaviour.
- A task is added to or removed from the canonical workload.
- The reference machine spec changes.

Old baseline files live under `tests/baselines/archive/` for trend analysis.
