# t13 fixture skeleton

This fixture is a measurement probe — there's no real code to refactor.
The harness instrumentation (`atelier find`) walks the per-session
context snapshot and records the latency of "find context items
matching this path" probes.

The `meta.json` next to this file lists three queries the operator
runs in sequence. The first two should match seeded items in the
session under test; the third matches no items and exercises the
"empty-result-still-records-latency" branch.
