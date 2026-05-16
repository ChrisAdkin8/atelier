# Atelier

[![check](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml/badge.svg)](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A pre-implementation coding harness designed around three convictions:

- The AI is a collaborator sharing a workspace, not a chat box with side effects.
- Bring-your-own-model is the architectural starting point.
- "Done" is a property the harness *verifies*, not a claim the model makes.

The spec is in [`coding-harness-spec.md`](coding-harness-spec.md). The supporting calibration rig is wired and self-testing; the harness itself is the next phase.

---

## Status

**Nothing is built yet beyond the rig.** The repository contains a full specification, the schemas and fixtures that will validate the harness once it exists, and a Rust workspace scaffold ready to be filled in.

| | Count | Where |
|---|---|---|
| Specification sections | — | [`coding-harness-spec.md`](coding-harness-spec.md) |
| JSON schemas (Draft 2020-12) | 20 | [`schemas/`](schemas/) |
| Canonical workload fixtures | 11 (10 Py + 1 TS) | [`tests/workload/canonical/`](tests/workload/canonical/) |
| Validated artifacts | 50 | [`tests/`](tests/) |
| Rig self-tests | 112 | [`tests/`](tests/) |
| Phased build plan | see file | [`tasks/todo.md`](tasks/todo.md) |

`make check` runs schema meta-validation → artifact validation → rig self-tests → workload dry-run. All currently green; CI runs the same pipeline on every push/PR.

For the invariants the rig *enforces* (no-op-harness exploits, cross-schema `$ref` resolution, schema versioning, BYOM baselines, etc.), see [`tests/README.md`](tests/README.md). For what's blocking Phase A coding, see [`tasks/todo.md`](tasks/todo.md).

---

## Quick start

```sh
# 1. Toolchain (one-time). Full detail: docs/toolchain.md
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. Build the CLI and bootstrap a project with it
cargo install --path crates/atelier-cli   # puts `atelier` on $PATH
atelier init /path/to/some/repo

# 3. (Optional) Run the rig against this repo
make install-rig
make check
```

The first `cargo` call inside this repo will silently fetch the pinned **Rust 1.85.0** toolchain via `rust-toolchain.toml` — see [`docs/toolchain.md`](docs/toolchain.md) for why this exact version is required and how to troubleshoot the `edition2024` error if it appears.

---

## How it's built

Atelier is a **Rust workspace**, split into four crates:

| Crate | Role |
|---|---|
| [`atelier-core`](crates/atelier-core/) | Agent loop, BYOM adapters, MCP client, session state, checkpoints, cost ledger. **No UI dependencies.** The §2.5 state machine lives here. |
| [`atelier-cli`](crates/atelier-cli/) | Headless `atelier` binary. Currently provides `atelier init`; future home for `atelier login/logout/rotate/whoami` (spec §11). |
| [`atelier-gui`](crates/atelier-gui/) | Tauri 2.x shell consuming `atelier-core` via a broadcast channel. Scaffold. |
| [`atelier-tui`](crates/atelier-tui/) | `ratatui` + `crossterm` frontend over the same broadcast channel. Scaffold. |

**Agent loop.** Single-turn streaming state machine on `tokio`. Cancellation uses Rust drop semantics — no invented cancel protocol. Verification is a state transition (`Verifying`), not an agent capability. See spec §2.5.

**Tool transport.** `atelier-core` ships an MCP client (via the `rmcp` crate) on day one. Any MCP-compliant server — filesystem, GitHub, web search, databases, custom — can be registered via `mcp_servers.json` (schema: `schemas/config/mcp_servers.v1.json`) without writing Atelier-specific glue. Built-in tools (file ops, shell, search) are exposed through the same interface, so verification gates, hooks, ledger, and trust budget treat built-in and external tools uniformly. See spec §15. For `rmcp` dependency wiring detail, see [`crates/atelier-core/README.md`](crates/atelier-core/README.md).

---

## Layout (top-level)

```
.
├── coding-harness-spec.md   the spec
├── CHANGELOG.md             spec + rig revisions
├── Cargo.toml               Rust workspace root (pins rmcp = "0.1")
├── rust-toolchain.toml      pinned Rust 1.85.0
├── crates/                  atelier-core / -cli / -gui / -tui
├── schemas/                 20 JSON Schemas (see schemas/README.md)
├── tests/                   the calibration rig (validators, fixtures, runner)
├── examples/                reference manifests (tools, hooks, skills, subagents, config)
├── prompts/                 Model Protocol few-shot examples
├── experiments/             one-off spikes (e.g. rmcp_spike)
├── tasks/todo.md            phased build plan + open questions
├── ci/                      nightly CI job stubs
├── docs/                    toolchain & full-tree reference docs
└── .github/                 workflows, PR template, issue templates
```

For the exhaustive tree with one-line annotations on every subdirectory, see [`docs/layout.md`](docs/layout.md).

---

## `atelier init` — project bootstrap

From the root of any repo:

```sh
atelier init
```

This creates `<repo>/.atelier/{sessions,tools,hooks}/`, writes a seeded `ATELIER.md` at the repo root if one isn't already present (template: `crates/atelier-core/templates/ATELIER.md`), and appends `.atelier/` to an existing `.gitignore`.

`atelier init` is **idempotent** and **never overwrites an existing `ATELIER.md`**. `ATELIER.md` is the project-level user-config file — Atelier reads it at session start and injects it into the system prompt. Equivalent to Cursor's `.cursorrules` / Claude Code's `CLAUDE.md`.

Build/install options (debug build, release build, install on `$PATH`, run without installing) are in [`crates/atelier-cli/README.md`](crates/atelier-cli/README.md).

---

## Running the rig

```sh
make install-rig      # one-time: creates .venv/ and installs ".[rig]" into it
make check            # full pipeline: schemas + artifacts + rig self-tests + dry-runs
```

Individual stages:

```sh
make schemas          # meta-validate schemas/*.json
make artifacts        # validate concrete artifacts against schemas
make rig-tests        # pytest the rig itself
make dry-run          # full JSON output of dry-runs
make summary          # one-line OK/FAIL per task
make clean            # remove __pycache__ and .pytest_cache trees
```

---

## What's intentionally absent

- **No CI provider beyond GitHub Actions.** The Makefile is portable; other providers (Buildkite, GitLab CI) can wrap `make check` similarly.
- **No harness implementation.** The spec describes one. The rig is ready to measure it. Build it.

---

## License

Apache 2.0. See [`LICENSE`](LICENSE). All Phase A code (`atelier-core`, `atelier-cli`, `atelier-gui`, `atelier-tui`) inherits this license via the workspace `Cargo.toml`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop, conventions, and PR process. Spec questions and design proposals belong in GitHub Discussions; bugs and feature requests use the templates under `.github/ISSUE_TEMPLATE/`. `make check` must be green before opening a PR — CI runs the same pipeline.
