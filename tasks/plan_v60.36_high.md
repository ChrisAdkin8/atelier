# v60.36 — High-severity fixes

**Scope:** seven findings from the 2026-05-19 deep scan. CI supply-chain (×2), Python rig robustness (×2), schema constraints (×3). Single bundle, sequentially merged.

**Out of scope:** the v60.28 H1 operator action (rotate the leaked Anthropic key) — already tracked separately.

---

## H1 — CI: split nightly-commit privilege from dependency install

**Where:** `.github/workflows/nightly_phase_a_gate.yml`, `nightly_phase_b_gate.yml`, `nightly_protocol_overhead.yml` (any workflow with `contents: write` that also runs `pip install` / `cargo test` after checkout).

**Why:** the same job both installs untrusted transitive deps (PyPI `.[rig]`, Cargo registry) AND holds `${{ secrets.GITHUB_TOKEN }}` with `contents: write` to push `tests/phase_a_gate/last_run.json` etc. back to `main`. A compromise of any transitive dep grants push access to a protected branch.

**Fix:** privilege-separate. Two jobs:
1. `measure` — no token, no `contents: write`, downloads deps, runs gates, uploads artifact via `actions/upload-artifact`.
2. `commit` — `needs: measure`, `contents: write`, downloads the artifact via `actions/download-artifact`, runs **only** `actions/github-script@<sha>` (or `git` against a path it writes itself from the artifact) to commit + push. No `pip install`, no `cargo`, no `npm`, no untrusted dep resolution.

**Verify:**
- `cat .github/workflows/nightly_*.yml | grep -A2 'contents: write'` shows the permission only on the commit job.
- New test in `tests/test_ci.py`: assert that any job declaring `permissions: contents: write` has no `run:` step that runs `pip install`, `cargo test`, `cargo build`, or `npm install`.

## H2 — CI: drop `GITHUB_TOKEN` from any step that runs cargo/pip

**Where:** same three nightly workflows.

**Why:** companion to H1. Even after privilege-split, default `GITHUB_TOKEN` env is exposed to every step of the privileged job; a malicious dep can exfiltrate it. The token must only be available to the explicit commit step.

**Fix:**
- Top of every job: `env: { GITHUB_TOKEN: "" }` (defensive scrub).
- Only the explicit commit step sets `env: { GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }} }` inline.

**Verify:** new `tests/test_ci.py::test_no_blanket_github_token` greps for top-level workflow `env: GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}` and fails the build if found.

## H3 — RIG: `extract_meta` raises `NameError` if `jsonschema` import partially fails

**Where:** `tests/workload/runner/runner.py:220-228`.

**Why:** the function imports `jsonschema` inside a `try` then references `jsonschema.ValidationError` in a sibling `except` — if the import raises anything other than `ImportError` (e.g. a `ModuleNotFoundError` subclass surfaced under an editable install with a broken re-export), the `except` clause itself errors with `NameError`, crashing the whole workload run.

**Fix:**
```python
try:
    import jsonschema
except ImportError:
    return payload, None
try:
    jsonschema.validate(payload, _SENTINEL_SCHEMA)
except (jsonschema.ValidationError, jsonschema.SchemaError) as e:
    return payload, str(e)
return payload, None
```

**Verify:** new unit test in `tests/test_runner.py` monkey-patches `sys.modules["jsonschema"]` to raise on attribute access and asserts `extract_meta` returns `(payload, None)` rather than raising.

## H4 — RIG: surface `TimeoutExpired.stderr` on harness timeout

**Where:** `tests/workload/runner/runner.py:118-123` (`_run_with_pg_timeout`) and `runner.py:283-289` (`harness_run`).

**Why:** on timeout, `result = None` and the harness-result payload silently drops `e.stderr`. A timed-out workload produces an artifact with empty `stdout_tail`, no stderr, and no signal as to *what* the harness was doing — debugging is reduced to re-running locally.

**Fix:**
- In `_run_with_pg_timeout`, after `_kill_process_group`, read directly from `proc.stdout` / `proc.stderr` pipes via `.read()` rather than calling `communicate` twice.
- Re-raise `TimeoutExpired` with captured `stdout` + `stderr` populated.
- In `harness_run`, when `timed_out`, surface the tail of stderr alongside stdout: `stderr_tail = (e.stderr[-1000:] if e.stderr else "")`.

**Verify:** new `tests/test_workload_runner.py::test_timeout_surfaces_stderr` spawns a Python sleep-and-write subprocess, times it out at 0.2s, and asserts the result has non-empty `stderr_tail`.

## H5 — SCH: cap regex / free-form fields in audit schemas

**Where:** `schemas/audit/egress.v1.json:27` (`redactions_applied[].pattern`), `schemas/audit/mcp_egress.v1.json` (any `reason` / `description` field), `schemas/audit/subprocess_egress.v1.json` (same sweep).

**Why:** unbounded-length regex patterns are a ReDoS surface AND an audit-log bloat surface (a misbehaving producer can write a multi-megabyte pattern per row, ballooning the log silently). The `audit/lsp_install.v1.json:53` already caps at `maxLength: 1024` — generalise.

**Fix:** add `"maxLength": 512` to every regex / free-form string field across `audit/*.v1.json` that doesn't already have a cap.

**Verify:**
- Run `make schemas` — still meta-validates.
- New rig test asserts every audit-schema string field has either an explicit `maxLength` or an enum/const constraint.

## H6 — SCH: accept mixed-case `git_sha` and dedupe the pattern

**Where:** `schemas/ci/phase_a_gate.v1.json:18`, `schemas/ci/protocol_conformance.v1.json:26`.

**Why:** the regex `^[0-9a-f]{7,40}$` only accepts lowercase, while `gh` and historic artifacts may produce mixed casing. Same regex is duplicated across two files — single-source-of-truth violation.

**Fix:** either:
- (Recommended) Change to `^[0-9a-fA-F]{7,40}$` in both files and add a `$defs` extract in a shared file (e.g., `schemas/_shared/refs.v1.json`) referenced by both via `$ref`. Existing schemas don't yet have a shared `$defs` file — adding one is a one-time investment.
- Or document the lowercase normalisation contract in `.github/workflows/*.yml` for every producer.

**Verify:** new rig test feeds a mixed-case SHA through both schemas and asserts validation passes.

## H7 — SCH: pin `model_protocol/envelope.v1.json` with `version: {const: 1}`

**Where:** `schemas/model_protocol/envelope.v1.json`.

**Why:** every other artifact pins `version` via `const: 1`. The envelope is the one cross-boundary type without it. A v2 envelope rolling out without a `version` discriminator means in-flight `session.json` files with old envelopes cannot be distinguished from new ones structurally — replay logic will silently misinterpret.

**Fix:**
```json
"version": { "const": 1 }
```
Added to `properties`. **Not** in `required` (so old artifacts validate; the missing field is treated as `1` per the const-pin convention used elsewhere).

**Verify:**
- `make schemas` passes.
- A new artifact in `tests/test_schemas.py` asserts a v1 envelope (with explicit `version: 1`) validates AND an envelope missing `version` still validates (back-compat).
- A future envelope with `version: 2` fails validation — locks in the discriminator semantics.

---

## Bundle verification

- `make check` ⇒ 21/21 schemas, 52/52 artifacts, all rig tests, dry-run.
- `cargo fmt --check && cargo clippy -- -D warnings && cargo test --workspace` — no Rust regression even though no Rust code is touched here.
- One commit per item; verification report appended to `tasks/todo.md`.
- CHANGELOG entry: `v60.36: deep-scan high-severity bundle — CI privilege-split, rig robustness, schema constraints`.

## File-disjoint check

- H1, H2 touch `.github/workflows/*.yml` + `tests/test_ci.py`
- H3, H4 touch `tests/workload/runner/runner.py` + `tests/test_*.py`
- H5, H6, H7 touch `schemas/**` + `tests/test_schemas.py`

The three groups are file-disjoint, so they can be parallel-bundled if needed; sequential is simpler given the small change count.
