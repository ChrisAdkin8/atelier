Add a `--verbose` flag to the CLI in `mycli.py`.

Behavior:
- Default: False (off)
- When set, the output is prefixed with `[VERBOSE] ` (note the trailing space)
- `python mycli.py --help` must show the new flag

Add a test (or tests) for the new behavior in `tests/test_mycli.py`. Existing tests must still pass.
