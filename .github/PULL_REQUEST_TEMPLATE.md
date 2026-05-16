<!--
PR template for Atelier. Delete sections that don't apply, but keep the headers
so reviewers can scan quickly. See CONTRIBUTING.md for the full process.
-->

## What changed

<!-- One paragraph. What does this PR do? -->

## Where it lands

<!-- Tick all that apply. -->

- [ ] Spec (`coding-harness-spec.md`) — section: __
- [ ] Schema(s) — file: __
- [ ] Rig (`tests/validate_*.py`, `tests/test_*.py`, runner)
- [ ] Canonical workload fixture (t__)
- [ ] Example artifact (`examples/`)
- [ ] Rust crate(s) — `atelier-core` / `atelier-gui` / `atelier-tui`
- [ ] Docs (README, CHANGELOG, per-dir README)
- [ ] Hygiene (LICENSE, SECURITY, CoC, CI)
- [ ] Other: __

## Why

<!-- The motivation. If a Discussion or Issue exists, link it. -->

Refs: #

## Verification

<!-- How did you verify the change? `make check` is the floor for most PRs. -->

- [ ] `make check` green locally
- [ ] Rust changes: `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, `cargo fmt --check`
- [ ] New schema → regression tests added to `tests/test_schemas.py`
- [ ] New canonical fixture → `meta.json` + `checks.json` present; runner dry-run passes
- [ ] Spec change → cross-references checked (no broken links to schemas / sections / paths)

## Tallies

<!-- If this changes the schema / artifact / rig-test count, update README.md
and CHANGELOG.md accordingly. -->

- Schemas: __ → __
- Artifacts: __ → __
- Rig self-tests: __ → __
- Workload dry-runs: __ → __

## Risks / call-outs

<!-- Anything reviewers should pay particular attention to: PROVISIONAL
parameters introduced, cross-pillar implications, breaking schema changes
(versioning policy applies — bump to v2 in a new file), etc. -->

## Checklist

- [ ] CHANGELOG entry added under the next unreleased version header
- [ ] Documentation updated (per-directory READMEs touched if layout changed)
- [ ] No secrets, real credentials, or personally identifying information committed
- [ ] PROVISIONAL parameters carry a documented calibration method
