# Contributing to Atelier

Thanks for your interest. Atelier is pre-implementation: the spec, schemas, and calibration rig are in place; the harness itself is the next phase. There are several useful ways to contribute right now.

## Where to read first

1. `README.md` for orientation and the state of the project.
2. `coding-harness-spec.md` — at minimum the table of contents and §0 (mission), §1, §2, and §7 (the load-bearing pillars).
3. `tasks/todo.md` for the phased build plan, the open-questions table, and the two external-action items blocking Phase A code.
4. `tests/workload/canonical/README.md` — the 11-task calibration workload. The priority subset for the backend milestone is **t01, t02, t05, t06, t10**.
5. `schemas/README.md` — the data model. Useful before touching any artifact under `examples/`, `prompts/`, or `crates/atelier-core/{skills,catalog,templates,tools}/`.
6. `CHANGELOG.md` — how the spec arrived at its current shape; useful if you want to know *why* a section is the way it is.
7. `docs/layout.md` — exhaustive repo tree if the top-level README's high-level listing isn't enough.

## What can be contributed now

- **Spec edits.** Tighten or clarify any pillar. If the change is more than a typo, open a Discussion first.
- **Schema improvements.** Tighten existing schemas, add regression tests, or close a gap in coverage.
- **Workload fixtures.** Add new canonical tasks (t12+) to broaden language and shape coverage. Each task needs `prompt.md`, `expected.md`, `fixture/`, `meta.json`, and `checks.json` per the existing pattern.
- **Rig hardening.** New cases in `tests/test_schemas.py` / `tests/test_validators.py` / `tests/test_runner.py`.
- **Phase A implementation.** The cargo workspace is scaffolded. Start at `crates/atelier-core/` and the §2.5 state machine.
- **MCP catalog entries.** Curated, well-known MCP servers worth bundling.
- **rmcp spike execution.** Run `experiments/rmcp_spike/` on a real machine and fill in the decision matrix.

## Dev loop

Local validation before opening a PR:

```sh
make install-rig    # one-time: creates .venv/ and installs the rig deps (jsonschema, referencing, pytest) into it
make check          # schemas + artifacts + rig self-tests + workload dry-run
```

`make check` must be green. CI runs the same pipeline on every push and PR; a red CI run is a merge blocker.

For Rust changes:

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

CI runs these same gates on a matrix of **Ubuntu + macOS**. The toolchain is pinned to **Rust 1.85.0** via `rust-toolchain.toml` (see [`docs/toolchain.md`](docs/toolchain.md) for why this exact version is required). The reference machine — used for any timing or baseline procedure — is documented at `tests/perf/reference.md`.

## Conventions

- **Smallest change that works.** Don't refactor adjacent code unless it materially reduces risk.
- **Match existing style.** Spec sections use `## N. Title` for pillars and `### Subtitle` for subsections. Schemas use `additionalProperties: false` by default; use `oneOf` / `allOf` + `if/then` to keep contracts tight.
- **PROVISIONAL parameters.** Numeric defaults without calibration carry the PROVISIONAL marker and name the calibration method. New parameters should follow the same pattern.
- **No unverified Rust.** If your change touches a Rust crate, `cargo check --workspace` must pass locally. The spec accepts unverified Rust scaffolds only in the explicit Phase A pre-implementation phase; once Phase A starts, every commit should compile.

### Filename conventions

Schemas live at `schemas/<area>/<name>.v1.json` — the `.v1.json` suffix is load-bearing for the versioning policy in `schemas/versions.md`.

Concrete artifacts that conform to a versioned schema use one of two forms:

- **`.v1.json`** — preferred for human-curated examples under `examples/`. The schema version is part of the human-readable identity of the file, mirroring the schema's filename.
- **`.json`** (no version suffix) — used for runtime-overrideable artifacts under `crates/atelier-core/{skills,catalog,templates}/` and per-user files at `~/.atelier/...` or `<repo>/.atelier/...`. The schema version is conveyed by the *directory* (a future v2 lives at `crates/atelier-core/skills_v2/` and `~/.atelier/skills_v2/`), not the filename. This lets bundled artifacts be referenced by short names (e.g., `/review` reads `skills/review.json`) without the version cluttering invocation.

The validator (`tests/validate_artifacts.py`) carries both globs explicitly per artifact type; new artifact categories should pick one convention and stick to it.

## PR process

1. Open a Discussion if the change is non-trivial — spec edits, new pillars, breaking schema changes.
2. Fork → branch → PR. Branch names: `kebab-case`, ideally referencing the issue / discussion.
3. Use the PR template (`.github/PULL_REQUEST_TEMPLATE.md`). Required: which spec section / schema / pillar; what changed; how it was verified; whether it bumps any tally in `README.md` or `CHANGELOG.md`.
4. CI must be green. Reviewers may request additional regression coverage if the change widens a contract.
5. Maintainer merges; CHANGELOG entry lands in the same PR (under the next unreleased version header).

## Reporting bugs / requesting features

Use the templates in `.github/ISSUE_TEMPLATE/`. Spec questions and "should Atelier do X?" conversations belong in Discussions, not Issues.

## Security

See `SECURITY.md`. Do not file public issues for vulnerabilities.

## Code of conduct

See `CODE_OF_CONDUCT.md`. By participating, you agree to abide by it.

## License

Atelier is Apache 2.0 (see `LICENSE`). All contributions are accepted under the same license; the Apache 2.0 license includes a Contributor License Agreement clause covering inbound contributions.
