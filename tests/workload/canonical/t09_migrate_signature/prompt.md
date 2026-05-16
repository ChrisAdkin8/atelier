The function `parse(input, opts={})` in `parse.py` uses a mutable default argument (an anti-pattern) and a positional `opts` parameter. Migrate the signature to:

```python
def parse(input, *, opts=None):
    ...
```

Then update every caller in this codebase to pass `opts` as a keyword argument. If a caller was passing no `opts`, leave it as-is. If it was passing `opts` positionally or via dict, switch it to `opts=…`.

After migration, **no caller should pass `opts` positionally**, and all tests must pass.
