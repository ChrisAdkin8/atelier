# t10 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0 (7 tests pass)
- `python -c "import sys; sys.path.insert(0, 'fixture'); from lru import LRUCache; c = LRUCache(2); c.put('a', 1); c.put('b', 2); c.put('c', 3); assert c.get('a') is None and c.get('b') == 2 and c.get('c') == 3"` exits 0

## Invariants
- Only `fixture/lru.py` modified
- `fixture/tests/test_lru.py` byte-equal to starting state
- `LRUCache` class exposes `__init__(capacity)`, `get(key)`, `put(key, value)`

## Permission-prompt expectations (PROVISIONAL — measure before relying on these)
- Tool calls: 1 read (lru.py), 1 read (test_lru.py), 1 write (lru.py), 1 test invocation, possibly 1 retry = 4–5 actions
- Baseline (current Claude Code): record on `tests/baselines/permission_prompts.json`
- Atelier target: ≤2 prompts with §8 learning

## Turn-budget
- Hard cap: 20 turns
- Expected median: 3 turns (read spec + tests, implement, fix any test failure)
