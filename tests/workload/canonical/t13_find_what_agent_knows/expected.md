# t13 — Find what agent knows about file X

Spec §5 UX target: "find what agent knows about file X median <5 s".

This is a probe fixture, not a behaviour gate. The `atelier find`
subcommand takes a `--path` argument and walks every session under
`<workspace>/.atelier/sessions/<sid>/` to collect `ContextItemSummary`
rows whose `kind == "file_ref"` and whose `label` matches. Each call
appends a `FindProbe` entry to `find_probes.json` with the elapsed
time from request to first match.

The 5-second target is measured as the median over the rolling probe
log; the rolling window is whatever the operator chooses to inspect.
The fixture's `find_probe.queries` list is a suggested driver
sequence — three probes against paths the seeded agent knows about
plus one against a path the agent does not. The first three should
hit; the fourth should miss.
