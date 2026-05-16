# t03 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0 (3 tests pass)
- `python -c "import json; d = json.load(open('fixture/config.json')); assert 'legacy_name' not in d and 'name' in d"` exits 0

## Files that must change
- `fixture/config.json` — keys renamed
- `fixture/config.py` — reads new keys

## Invariants
- No file added or removed
- The two pre-existing tests (`test_environment`, `test_timeout`) keep their original assertions; only the data they read against changes

## Permission-prompt expectations (PROVISIONAL)
- ≤2 prompts under §8 defaults

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2 turns
