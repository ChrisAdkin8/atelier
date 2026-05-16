# t05 — Expected outcome

## Bug to find
`format_duration(7200)` is supposed to return `"2h"` but returns `"2h0m"`. The implementation forgets to special-case `minutes == 0` when there are hours. The fix is to drop the `Ym` segment when minutes is zero.

A possible variant fix: replace the final `return` with a conditional that emits `f"{hours}h"` when `minutes == 0`. The exact code shape is not specified; what's specified is that all five tests pass.

## Mechanical checks
- `pytest fixture/` exits 0
- `git diff fixture/tests/` (or filesystem equivalent) shows no changes — the agent must not modify the test file
- `python -c "import sys; sys.path.insert(0, 'fixture'); from duration import format_duration; assert format_duration(7200) == '2h'; assert format_duration(5400) == '1h30m'; assert format_duration(1500) == '25m'; assert format_duration(0) == '0m'"` exits 0

## Invariants
- Only `fixture/duration.py` is modified
- The function signature is unchanged
- The docstring is preserved or improved (not deleted)

## Anti-patterns
- Modifying the failing test instead of the production code is a failure mode, not a fix. The runner asserts the test file is byte-equal to its starting state.

## Permission-prompt expectations (PROVISIONAL — measure before relying on these)
- Tool calls: 1 read (duration.py), 1 read (test_duration.py), 1 write (duration.py), 1 test invocation = 4 actions
- Baseline (current Claude Code): record on `tests/baselines/permission_prompts.json`
- Atelier target: ≤2 prompts with §8 learning

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2 turns (read both files together, make the fix + verify in one turn)
