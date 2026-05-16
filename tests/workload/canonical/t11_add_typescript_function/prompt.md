Add a TypeScript function `divisibleBy(n: number, m: number): boolean` to `utils.ts`. It must return `true` iff `n` is divisible by `m`, and throw an `Error` when `m` is 0.

The existing tests in `tests/test_utils.ts` import `divisibleBy` from `../utils.ts` and exercise four cases: `(6, 2)` → `true`, `(7, 2)` → `false`, `(0, 5)` → `true`, `(5, 0)` throws.

Run the tests with `node --test tests/test_utils.ts` and make them pass. Do not modify the test file.
