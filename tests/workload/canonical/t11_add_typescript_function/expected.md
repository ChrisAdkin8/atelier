# t11 — Expected outcome

## Mechanical checks
- `node --test tests/test_utils.ts` exits 0 (all 4 tests pass)
- `tests/test_utils.ts` byte-equal to starting state (agent must not modify the test)
- `divisibleBy` is exported from `utils.ts` as a function

## Invariants
- Only `fixture/utils.ts` modified
- `fixture/tests/test_utils.ts` unchanged
- No new dependencies added to `package.json`

## Why TypeScript
This task exists so §7 Tier-1 hallucination detection has somewhere to run. Phase B's mechanical gate names a TypeScript fixture explicitly; without one the gate is unanchored. The shape mirrors t01 (single-file pure function + tests) so cross-language calibration is apples-to-apples.

## Permission-prompt expectations (PROVISIONAL)
- ≤2 prompts under §8 defaults

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2 turns
