# t09 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0 (all 6 tests pass)
- `grep -E 'def parse\(' fixture/parse.py` matches `def parse(input, *, opts=None):` (signature migrated)
- `grep -nE 'parse\([^,)]+,\s*\{' fixture/` returns no matches (no caller passes opts positionally as a dict literal)
- `grep -nE 'parse\([^)]+,\s*opts=' fixture/` returns ≥3 matches (callers using opts kwarg)

## Invariants
- `parse(input)` (no opts) still works — `batch.py` does this
- All callers in `user.py`, `admin.py`, `batch.py`, `api.py` modified consistently
- Test file `tests/test_callers.py` is byte-equal to starting state

## Permission-prompt expectations (PROVISIONAL)
- ≤3 prompts under §8 defaults (multiple files but same directory shape)

## Turn-budget
- Hard cap: 20 turns
- Expected median: 3 turns
