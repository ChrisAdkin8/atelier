The function `pipeline` in `pipeline.py` mixes three concerns: parsing, scoring, and reporting. Refactor it into a top-level `pipeline` function plus three single-purpose helpers (`parse_rows`, `score_rows`, `format_report`). The top-level `pipeline` body should be a short composition.

Constraints:
- All existing tests must still pass.
- Public API is unchanged: callers continue to call `pipeline(raw)` with the same input and the same return shape.
- Do not change the test file.

This task is a §3 UX-target measurement fixture: the operator drives the GUI/TUI with the **conversation pane hidden** for the duration. The harness instrumentation captures which panes were visible and writes a per-run record so the spec target ("refactor without conversation pane open") can be replayed and verified.
