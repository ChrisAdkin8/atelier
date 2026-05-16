# t06 — Expected outcome

## Mechanical checks
- `pytest fixture/` exits 0
- `python -c "import sys; sys.path.insert(0, 'fixture'); from mycli import main; assert main(['World']) == 'Hello, World!'"` exits 0 (verbose-off default preserved)
- `python -c "import sys; sys.path.insert(0, 'fixture'); from mycli import main; assert main(['--verbose', 'World']) == '[VERBOSE] Hello, World!'"` exits 0
- `python -c "import sys; sys.path.insert(0, 'fixture'); from mycli import build_parser; assert '--verbose' in build_parser().format_help()"` exits 0

## Invariants
- Original 3 tests in `test_mycli.py` still pass (existing tests preserved)
- At least 1 new test for `--verbose` (off-state and on-state, ideally both)
- `mycli.py` exposes `build_parser()` and `main(argv=None)` as before (signatures unchanged; behavior extended)

## Permission-prompt expectations (PROVISIONAL — measure before relying on these)
- Tool calls: 1 read (mycli.py), 1 read (test_mycli.py), 1 write (mycli.py), 1 write (test_mycli.py), 1 test invocation = 5 actions
- Baseline (current Claude Code): record on `tests/baselines/permission_prompts.json`
- Atelier target: ≤2 prompts with §8 learning (after first write in `mycli.py` shape and first write in `tests/` shape are approved, the test run auto-approves)

## Turn-budget
- Hard cap: 20 turns
- Expected median: 2 turns (read + plan; write both files + test in one turn)
