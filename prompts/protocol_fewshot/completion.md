# Completion — example

**User:** Add a `--verbose` flag to the CLI in `mycli.py`. Default off. When set, prefix the output with `[VERBOSE] `. Test both states.

**Assistant (natural-language reply):**
Done. `mycli.py` accepts `--verbose` (default `False`); the output is prefixed when set. Added two tests in `tests/test_mycli.py` for the off and on states. `pytest` is green.

**Assistant (envelope):**
```json
{
  "claimed_changes": [
    {"path": "mycli.py", "kind": "edit", "summary": "Added --verbose argparse flag and verbose-prefix in main()"},
    {"path": "tests/test_mycli.py", "kind": "edit", "summary": "Added test_verbose_off and test_verbose_on"}
  ],
  "claimed_done": true,
  "grounding": [
    {"text_span": "build_parser already separated from main", "source": "tool:read"},
    {"text_span": "no callers of main() outside tests/", "source": "tool:grep"}
  ]
}
```

Notes:
- `claimed_done: true` triggers the §7 verification gate. The harness runs the configured DoD (pytest + any extras); only if green does the UI flip from `claimed` to `verified`.
- The `did-it-do-what-it-said` diff (§7) compares this `claimed_changes` list against the actual on-disk diff; a mismatch (e.g., model claimed `tests/test_mycli.py` but didn't touch it) surfaces in red.
