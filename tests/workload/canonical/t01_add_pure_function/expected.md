# t01 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0
- `python -c "import sys; sys.path.insert(0, 'fixture'); from utils import divisible_by; assert divisible_by(6, 2) is True; assert divisible_by(7, 2) is False; assert divisible_by(0, 5) is True"` exits 0
- `python -c "import sys; sys.path.insert(0, 'fixture'); from utils import divisible_by; divisible_by(5, 0)"` exits non-zero with `ValueError`

## Invariants
- `fixture/utils.py` contains a function named `divisible_by` with two integer parameters
- `fixture/tests/test_utils.py` contains at least 4 test functions (one per required case)
- `fixture/pyproject.toml` not modified
- No files added or removed outside `fixture/utils.py` and `fixture/tests/test_utils.py`

## Permission-prompt expectations (informs §8 baseline and Atelier target)
- Reasonable upper bound on tool calls: 2 reads (utils, test file), 2 writes (utils, test file), 1 test invocation = **5 actions**
- Baseline (current Claude Code with default settings): expect ~5 permission prompts on this task; record actual on `tests/baselines/permission_prompts.json`
- Atelier target: with §8 learning + `src/**` + `tests/**` per-path defaults, ≤2 permission prompts (first test-run + first write each may prompt; subsequent same-shape actions auto-approved)

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2–3 turns (read, write+test in one turn; possibly re-run)
