# Nightly CI jobs

**Status: stub.** Per-PR validation already runs as `make check` via `.github/workflows/check.yml`. This directory documents the *nightly* jobs the spec calls for; they need credentials and runners that the per-PR pipeline doesn't have, so they wait for a Phase A decision on where to host them.

## `protocol_overhead.yml` (planned)

Per `coding-harness-spec.md` §2, Atelier ships a nightly job that measures Model Protocol overhead:

1. Run the canonical workload's priority subset (t01, t02, t05, t06, t10) against each in-tree adapter.
2. For each (adapter, strategy) pair, record:
   - Median percentage token overhead vs. a no-protocol baseline.
   - Envelope conformance rate after the re-prompt loop.
3. Write the result to `tests/protocol/overhead.json` per `schemas/protocol/overhead.v1.json`.
4. Validate the file via `tests/validate_artifacts.py`.
5. Compare against the prior 7 days; alert if median overhead drifted up by >10% on any (adapter, strategy).

Requires API credentials for at least one remote provider plus a local backend (Ollama recommended) on the runner. Not viable on free GitHub Actions runners without secrets management.

## Why this isn't wired yet

Writing real YAML before the credential and runner story is settled would ship a file that fails every night. The spec mentions the job by name so the contract is locked; the body waits for a decision in Phase E.
