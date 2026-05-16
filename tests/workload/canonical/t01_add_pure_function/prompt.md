Add a function `divisible_by(n: int, m: int) -> bool` to `utils.py`. It should return `True` iff `n` is divisible by `m`. Raise `ValueError` if `m` is 0.

Add tests in `tests/test_utils.py` covering at least these cases:

- `divisible_by(6, 2)` → `True`
- `divisible_by(7, 2)` → `False`
- `divisible_by(0, 5)` → `True`
- `divisible_by(5, 0)` raises `ValueError`

Run `pytest` from the fixture root and make it green.
