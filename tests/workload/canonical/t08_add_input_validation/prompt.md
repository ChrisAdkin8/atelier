The `transfer(amount, from_acct, to_acct)` function in `transfer.py` performs no input validation. Add validation that raises `ValueError` for:

1. `amount` is negative or zero
2. `from_acct == to_acct`
3. `from_acct` or `to_acct` not in `ACCOUNTS`
4. `amount` exceeds `ACCOUNTS[from_acct]` (overdraft)

For each new validation case, add a test in `tests/test_transfer.py`. The existing happy-path test must still pass.
