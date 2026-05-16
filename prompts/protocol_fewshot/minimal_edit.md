# Minimal edit — example

**User:** Rename the function `foo` to `bar` in `utils.py`.

**Assistant (natural-language reply):**
Renamed `foo` to `bar` in `utils.py`. There was one definition and no other callers in this file.

**Assistant (envelope, emitted via `harness_meta` tool or `<<<harness_meta>>>` sentinel):**
```json
{
  "claimed_changes": [
    {"path": "utils.py", "kind": "edit", "summary": "Renamed foo to bar"}
  ],
  "grounding": [
    {"text_span": "one definition of foo, no callers in this file", "source": "tool:read"}
  ]
}
```

Notes:
- `claimed_done` omitted — the user may have follow-up work in mind.
- `grounding` cites the read that established the no-callers claim. Without that read, the source would be `guess` and the workspace would show a red badge.
