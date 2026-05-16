Implement an `LRUCache` class in `lru.py` matching this specification.

## API
```
class LRUCache:
    def __init__(self, capacity: int): ...
    def get(self, key) -> Any | None: ...
    def put(self, key, value) -> None: ...
```

## Semantics
- `get(key)` returns the value if present, `None` otherwise. A `get` that hits marks the key as most-recently-used.
- `put(key, value)` inserts or updates. If the cache is at capacity and the key is new, the least-recently-used entry is evicted to make room.
- `capacity` is a positive integer.
- Operations should be O(1) amortized, but the runner does not measure this; correctness is what's checked.

The test file `tests/test_lru.py` already exists and pins the semantics. `pytest fixture/` must pass.
