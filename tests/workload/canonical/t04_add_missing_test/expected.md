# t04 — Expected outcome

## Mechanical checks
- `pytest fixture/ --co -q` reports at least 4 collected tests
- `pytest fixture/` exits 0
- At least one test asserts `parse_iso_date("2024-02-29")` returns a valid date (leap-year)
- At least one test asserts `parse_iso_date("not-a-date")` raises `ValueError`

## Invariants
- `fixture/iso.py` unchanged
- Only `fixture/tests/test_iso.py` modified

## Permission-prompt expectations (PROVISIONAL)
- ≤2 prompts under §8 defaults

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2 turns
