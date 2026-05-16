# t08 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0 (≥5 tests total — happy path plus four validations)
- The happy-path test is unchanged
- Each new validation case raises `ValueError` (verified via `pytest.raises`)
- `transfer(-5, "alice", "bob")` raises `ValueError` (negative)
- `transfer(10, "alice", "alice")` raises `ValueError` (same account)
- `transfer(10, "alice", "ghost")` raises `ValueError` (missing account)
- `transfer(99999, "alice", "bob")` raises `ValueError` (overdraft)

## Invariants
- Only `fixture/transfer.py` and `fixture/tests/test_transfer.py` modified
- `transfer(amount, from_acct, to_acct)` signature unchanged
- `ACCOUNTS` dict retains its starting contents (`alice: 100`, `bob: 50`, `carol: 200`)

## Permission-prompt expectations (PROVISIONAL)
- ≤2 prompts under §8 defaults

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2 turns
