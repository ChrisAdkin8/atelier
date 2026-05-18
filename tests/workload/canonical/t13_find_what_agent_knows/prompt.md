Investigate the parser bug: `src/parser/lex.rs` is producing the wrong token for backslash-escapes inside string literals. The test in `tests/parser_strings.py` documents the expected behaviour. Patch `lex.rs` so the test passes; do not change the test file.

This fixture exists primarily to seed the §5 context manager with a handful of `FileRef` items so the `atelier find` subcommand has something to match against. The measurement is the time-to-first-match, not the patch itself.
