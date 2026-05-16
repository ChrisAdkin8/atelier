# t07 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0 (all 6 tests pass after refactor)
- `processor.py` defines at least **4 top-level callables** (the `process` entrypoint plus ≥3 helpers)
- The `process` function body is **≤10 statements** (heuristic for "short composition")
- Static check: `grep -E '^def [a-zA-Z_]+' fixture/processor.py | wc -l` returns ≥4
- `fixture/tests/test_processor.py` byte-equal to starting state

## Invariants
- Public API unchanged: `process(raw_data)` accepts the same inputs and returns the same shape
- No new files added; no test file modified

## Permission-prompt expectations (PROVISIONAL)
- ≤2 prompts under §8 defaults

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2–3 turns
