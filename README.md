# Atelier

[![check](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml/badge.svg)](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A pre-implementation coding harness. Atelier is designed around three convictions:

- The AI is a collaborator sharing a workspace, not a chat box with side effects.
- Bring-your-own-model is the architectural starting point.
- "Done" is a property the harness verifies, not a claim the model makes.

## State of the project

**Nothing is built yet.** This repository contains:

- a full **specification** (`coding-harness-spec.md`);
- **20 JSON schemas** covering every persistent or interchange artifact;
- a **calibration rig** — 11 canonical workload fixtures (10 Python + 1 TypeScript), a workload runner that executes per-task structured checks, two artifact validators with cross-schema `$ref` resolution, a 112-test rig self-test suite, four example session artifacts, and a baseline-comparison tool;
- a **build tracker** (`tasks/todo.md`) with the phased build plan;
- **CI** (`.github/workflows/check.yml`) running the full rig pipeline plus Rust gates on every push/PR.

The rig is wired end-to-end and self-testing. `make check` runs schema meta-validation → artifact validation → rig self-tests → workload dry-run.

**Current state:** 20 schemas / 50 artifacts / 112 rig tests / 11 dry-runs — all passing.

### Properties the rig enforces

- No-op-harness exploits caught on t05, t07, t11.
- Cross-schema `$ref`s (session → envelope; subagent-type → routing; tool manifests → `_implementation.v1.json`) resolve via the shared registry in `tests/_schema_helpers.py`.
- Hooks inline their implementation `oneOf` so a tool-only `timeout_ms` can't leak into the hook contract (spec §15: hooks warn, never block).
- Every schema carries a `version: const 1` discriminator.
- Built-in tool manifests (`read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`, `spawn_subagent`) live under `crates/atelier-core/tools/` and match spec §15 L722.
- Baselines are vendor-neutral (`baseline_harness_name` + `baseline_harness_version`). The §8 reference baseline is a spec choice, not a schema commitment — BYOM by construction.

### CI

- **Matrix:** Ubuntu + macOS.
- **Rust gates:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test -p atelier-core`.
- **Toolchain:** Rust pinned to **1.85.0** via `rust-toolchain.toml` (see [Setting up the Rust toolchain](#setting-up-the-rust-toolchain) for why this exact version).
- **Reference machine:** M1 Pro / 32 GB / macOS 26.4.1, documented at `tests/perf/reference.md`.

## Stack

Atelier is written in **Rust**, split into three crates:

- **`atelier-core`** — agent loop, BYOM adapters, MCP client, session state, checkpoints, cost ledger. No UI dependencies. The §2.5 state machine lives here.
- **`atelier-gui`** — Tauri 2.x shell consuming `atelier-core` via a broadcast channel.
- **`atelier-tui`** — `ratatui` + `crossterm` frontend consuming the same crate the same way.

### Agent loop

The agent loop is a **single-turn streaming state machine** on `tokio`. Cancellation uses Rust drop semantics (no invented cancel protocol). Verification is a state transition, not an agent capability. See spec §2.5 for the architecture rationale and crate choices.

### Tool transport: MCP, out of the box

`atelier-core` ships an MCP client (via the `rmcp` crate) on day one. Any MCP-compliant server — filesystem, GitHub, web search, databases, custom — can be registered via `mcp_servers.json` (schema: `schemas/config/mcp_servers.v1.json`) without writing Atelier-specific glue.

Built-in tools (file ops, shell, search) are exposed through the same interface, so the rest of the harness (verification gates, hooks, ledger, trust budget) treats built-in and external tools uniformly. See spec §15.

## Layout

```
.
├── README.md                          you are here
├── CHANGELOG.md                       spec + rig revisions
├── coding-harness-spec.md             the spec
├── Cargo.toml                         Rust workspace root (pins `rmcp = "0.1"`)
├── rust-toolchain.toml                pinned Rust 1.85.0
├── crates/
│   ├── atelier-core/                  agent loop, BYOM adapters, MCP client, session state
│   │   ├── Cargo.toml                 declares `rmcp = { workspace = true }` — the MCP client lives here
│   │   ├── catalog/                   bundled MCP server catalog
│   │   ├── skills/                    bundled skills (/review, /security-review, /test)
│   │   ├── subagents/                 bundled sub-agent types (researcher, test-runner, general-purpose)
│   │   ├── tools/                     bundled built-in tool manifests (read_file, write_file, edit_file, list_dir, grep, ast_grep, shell, spawn_subagent) — matches spec §15
│   │   └── templates/                 ATELIER.md seed template
│   ├── atelier-gui/                   Tauri 2.x shell (scaffold)
│   └── atelier-tui/                   ratatui + crossterm frontend (scaffold)
├── pyproject.toml                     rig manifest (jsonschema, pytest)
├── Makefile                           one-command rig orchestration
├── schemas/                           20 JSON Schemas (see schemas/README.md)
├── tasks/
│   └── todo.md                        phased build plan + open questions
├── tests/
│   ├── _schema_helpers.py             shared registry for cross-schema $ref resolution
│   ├── validate_schemas.py            meta-validate every schema
│   ├── validate_artifacts.py          validate artifacts + envelope JSON in fewshot
│   ├── test_schemas.py                schema regression suite (valid+invalid corpora; cross-schema $ref)
│   ├── test_validators.py             end-to-end validator tests
│   ├── test_runner.py                 runner internals + subprocess tests
│   ├── perf/reference.md              reference machine spec (populated v13: M1 Pro / 32 GB / macOS 26.4.1)
│   ├── sessions/examples/             example session artifacts validated against schemas/session/v1.json
│   └── workload/
│       ├── canonical/                 11 task fixtures (10 Python + 1 TypeScript) + README + baseline procedure
│       │                              each task: prompt.md, expected.md, fixture/, meta.json, checks.json
│       └── runner/                    workload runner + baseline comparison tool
├── examples/                          reference manifests for pluggable extension points
│   ├── tools/                         custom tool manifests
│   ├── hooks/                         hook manifests
│   ├── skills/                        skill manifests (invocable as /<name>)
│   ├── subagents/                     sub-agent type manifests (spawned via spawn_subagent)
│   └── config/                        routing.json + persistent permission state examples
├── prompts/
│   └── protocol_fewshot/              Model Protocol few-shot examples (validated by validate_artifacts.py)
├── experiments/
│   └── rmcp_spike/                    Phase A prerequisite: rmcp maturity assessment procedure
├── LICENSE                            Apache 2.0
├── SECURITY.md                        vulnerability disclosure policy
├── CODE_OF_CONDUCT.md                 Contributor Covenant 2.1
├── CONTRIBUTING.md                    how to contribute
├── .github/
│   ├── workflows/check.yml            runs `make check` on every push/PR
│   ├── PULL_REQUEST_TEMPLATE.md       PR template
│   └── ISSUE_TEMPLATE/                bug-report + feature-request forms
└── ci/
    └── nightly/                       nightly CI job stubs (e.g., protocol overhead)
```

## Setting up the Rust toolchain

The workspace targets **Rust 1.85.0**, pinned via `rust-toolchain.toml`. This exact minimum is forced by Cargo's `edition2024` feature, which `rmcp-macros 0.1.5` (a transitive dependency of `rmcp`) requires. Earlier toolchains fail the build with:

```
error: feature `edition2024` is required
```

You don't install the toolchain directly. Instead install **`rustup`** — the toolchain manager that bundles `cargo` and `rustc` — and it will fetch Rust 1.85.0 automatically the first time `cargo` runs inside this repo (honoring `rust-toolchain.toml`).

### 1. Install `rustup`

**macOS / Linux:**

```sh
# Install rustup (cargo, rustc, toolchain manager).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Load cargo into the current shell (or open a new terminal).
source "$HOME/.cargo/env"
```

**Windows:** download and run `rustup-init.exe` from <https://rustup.rs>, then open a new terminal.

### 2. Verify

Run these **from inside this repo** — `rustup` honors `rust-toolchain.toml` only when invoked within a workspace that has one:

```sh
cargo --version       # cargo 1.85.0 (...)
rustc --version       # rustc 1.85.0 (...)
```

The first `cargo` invocation triggers `rustup` to download the pinned toolchain — expect a one-time delay of ~30–90 seconds. Subsequent invocations are instant.

## Installing `rmcp`

`rmcp` is the official Rust SDK for the **Model Context Protocol** — Atelier's tool transport (spec §15). There is **no separate install step**: `rmcp` is a Cargo dependency that resolves from crates.io on first build.

### Where `rmcp` lives

The dependency is declared in two coordinated places — the version pin at the workspace root, and the actual consumer in `atelier-core` (the crate that owns the MCP client; `atelier-gui` and `atelier-tui` reach `rmcp` transitively through `atelier-core`).

**1. Workspace root** — `Cargo.toml`:

```toml
[workspace.dependencies]
rmcp = "0.1"
```

**2. Consuming crate** — `crates/atelier-core/Cargo.toml`:

```toml
[dependencies]
rmcp = { workspace = true }
```

This pattern — pin the version once at the root, reference it as `{ workspace = true }` from each consuming crate — is how every workspace dependency is wired. It keeps versions synchronized across crates and means a bump only happens in one place.

If a future workspace crate ever needs `rmcp` directly (rather than via `atelier-core`), add the same `rmcp = { workspace = true }` line to its `[dependencies]` — **never** redeclare the version.

### Fetch and verify

```sh
cargo fetch                       # download rmcp + transitive deps from crates.io
cargo check -p atelier-core       # confirm rmcp resolves and compiles cleanly
```

A successful `cargo check` ends with a line like:

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.85s
```

### Troubleshooting

- **`feature edition2024 is required`** — your toolchain is older than 1.85.0. Re-check `rustc --version` and confirm you ran `source "$HOME/.cargo/env"` in the current shell. If `rustup` was installed before the 1.85.0 pin landed, run `rustup update`.
- **Network errors during `cargo fetch`** — `rmcp` and its transitive deps are pulled from crates.io. Check your network, or set `CARGO_HTTP_PROXY` if you're behind a corporate proxy.

### The maturity spike

For the standalone `rmcp` maturity-assessment spike — a separate experiment, not part of the Cargo workspace — see `experiments/rmcp_spike/README.md`. Its outcome (GO / GO-WITH-CAVEATS / NO-GO) is a Phase A prerequisite per `tasks/todo.md`.

## Running the rig

```sh
make install-rig      # one-time: creates .venv/ and installs ".[rig]" into it
make check            # meta-validate schemas, validate artifacts, dry-run all 11 tasks
```

Individual commands:

```sh
make schemas          # meta-validate schemas/*.json
make artifacts        # validate concrete artifacts against schemas
make rig-tests        # pytest the rig itself
make dry-run          # full JSON output of dry-runs
make summary          # one-line OK/FAIL per task
make clean            # remove __pycache__ and .pytest_cache trees
```

## Project bootstrap (when the harness ships)

Once `atelier-core` is built, run from the root of any repo:

```sh
atelier init
```

This creates `<repo>/.atelier/` with:

- the subdirectories `sessions/`, `tools/`, `hooks/`;
- a seeded `ATELIER.md` at the repo root if one isn't already present (template at `crates/atelier-core/templates/ATELIER.md`);
- a `.atelier/` entry appended to an existing `.gitignore`.

`atelier init` is **idempotent** and **never overwrites an existing `ATELIER.md`**. `ATELIER.md` is the project-level user-config file — Atelier reads it at session start and injects it into the system prompt. Equivalent to Cursor's `.cursorrules` / Claude Code's `CLAUDE.md`.

## Where to read next

For a new contributor:

1. **`coding-harness-spec.md`** — the design. Start at the table of contents, then §0 (mission), then §1 + §2 + §7 which are the load-bearing pillars.
2. **`tasks/todo.md`** — what is and isn't done; the open questions block specific phases.
3. **`tests/workload/canonical/README.md`** — the 11-task calibration workload; the priority subset for the backend milestone is t01, t02, t05, t06, t10.
4. **`schemas/README.md`** — the data model.
5. **`CHANGELOG.md`** — how the spec arrived at its current shape; useful if you want to know why a section is the way it is.

## What's blocking work to start

Two remaining external-action items the rig cannot produce on its own:

1. **`tests/baselines/permission_prompts.json`** — captured by running the procedure at `tests/workload/canonical/baseline_procedure.md` against current Claude Code on the reference machine. Requires Claude Code installed and a person to drive it through the workload.
2. **`experiments/rmcp_spike/`** outcome — execute the spike procedure in a real Rust environment. Confirms (or rejects) `rmcp` as the §15 MCP client; Phase A §15 code shouldn't start before this resolves. ~30–60 minutes of human time on the reference machine.

After those: Phase A implementation (see `coding-harness-spec.md` → Phased build plan).

## What's intentionally absent

- **No CI provider beyond GitHub Actions.** The Makefile is portable; other CI providers (Buildkite, GitLab CI) can wrap `make check` similarly.
- **No harness implementation.** The spec describes one. The rig is ready to measure it. Build it.

## License

Apache 2.0. See [LICENSE](LICENSE). All Phase A code (`atelier-core`, `atelier-gui`, `atelier-tui`) inherits this license via the workspace `Cargo.toml`.

## Contributing

Bug reports and feature requests use the templates under `.github/ISSUE_TEMPLATE/`. Spec questions and design proposals belong in Discussions.

Before opening a PR, `make check` should be green locally. CI runs the same pipeline.
