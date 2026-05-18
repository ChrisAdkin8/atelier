# t12 — Refactor without the conversation pane open

This is a §3 UX-target measurement fixture. The intent is operator-facing,
not a behaviour gate:

- the user opens the GUI / TUI with the conversation pane hidden
  (collapsed or never opened);
- they drive the refactor of `pipeline.py` into 4 callables (top-level +
  3 helpers) using only the Diff and Plan / Context panels;
- the harness records pane-visibility state to
  `.atelier/sessions/<sid>/pane_visibility.json` so a follow-on
  measurement pass can replay the run and verify the refactor completed
  with the conversation pane never visible.

The behaviour checks below assert the refactor itself; the
pane-visibility record is the *observable* the spec target reads.
