<p align="center">
  <img src="assets/banner-loop.png" alt="atelier — a coding harness for BYOM agents: the agent loop, MCP transport, and verification gates between a model and your repo" width="100%">
</p>

<p align="center">
  <a href="#what-makes-it-different"><b>Why</b></a> ·
  <a href="coding-harness-spec.md"><b>Spec</b></a> ·
  <a href="#build"><b>Build</b></a> ·
  <a href="#configure-and-run"><b>Run</b></a> ·
  <a href="STATUS.md"><b>Status</b></a> ·
  <a href="CONTRIBUTING.md"><b>Contributing</b></a>
</p>

# Atelier

[![check](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml/badge.svg)](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Atelier is a coding harness for AI software engineering** — the agent loop, tool transport, verification gates, hooks, and cost ledger that sit between a model and your repository. It is built spec-first: a complete specification, JSON schemas, canonical workload fixtures, and a self-testing calibration rig exist *before* the harness, so the harness can be measured against fixed criteria as it lands.

The spec is in [`coding-harness-spec.md`](coding-harness-spec.md). Where the build currently stands — what has landed, what is in flight — is in [`STATUS.md`](STATUS.md).

---

## What makes it different

- **Bring-your-own-model from day one.** Not a vendor wrapper. Provider adapters are pluggable; `atelier-core` has no SDK bias and no hardcoded host paths.
- **Verification is a state, not a claim.** The agent loop has an explicit `Verifying` transition (spec §2.5). A task is "done" only when the harness can prove it — tests pass, schemas validate, gates clear — not when the model asserts so.
- **MCP-first tool transport.** Built-in tools (file ops, shell, search) and external MCP servers (filesystem, GitHub, databases, custom) share one interface via `rmcp`. Hooks, ledger, trust budget, and verification gates treat them uniformly (spec §15).
- **Headless core, swappable frontends.** `atelier-core` ships no UI. The Tauri GUI and `ratatui` TUI consume the same broadcast channel; a third frontend is additive, not invasive.
- **Cost ledger and trust budget as first-class concerns.** Every tool call, token, and side effect is accounted for. Observability is built in, not bolted on.
- **The AI is a collaborator in a workspace, not a chat box with side effects.** Sessions, checkpoints, hooks, and file boundaries are explicit.

---

## Layout

Atelier is a **Rust workspace**. Four crates under [`crates/`](crates/):

| Crate | Role |
|---|---|
| [`atelier-core`](crates/atelier-core/) | Agent loop, BYOM adapters, MCP client, session state, checkpoints, cost ledger. **No UI dependencies.** The §2.5 state machine lives here. |
| [`atelier-cli`](crates/atelier-cli/) | Headless `atelier` binary. Currently provides `atelier init`; future home for `atelier run` (Phase A) and `atelier login/logout/rotate/whoami` (spec §11). |
| [`atelier-gui`](crates/atelier-gui/) | Tauri 2.x shell consuming `atelier-core` via a broadcast channel. Scaffold. |
| [`atelier-tui`](crates/atelier-tui/) | `ratatui` + `crossterm` frontend over the same broadcast channel. Scaffold. |

Top-level tree:

```
.
├── coding-harness-spec.md   the spec
├── STATUS.md                what's landed / in flight / planned
├── CHANGELOG.md             spec + rig revisions
├── Cargo.toml               Rust workspace root (pins rmcp = "0.1")
├── rust-toolchain.toml      pinned Rust 1.85.0
├── crates/                  atelier-core / -cli / -gui / -tui
├── schemas/                 21 JSON Schemas (see schemas/README.md)
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

**Agent loop.** Single-turn streaming state machine on `tokio`. Cancellation uses Rust drop semantics — no invented cancel protocol. Verification is a state transition (`Verifying`), not an agent capability. The states and the legal transition table are in [`crates/atelier-core/src/state.rs`](crates/atelier-core/src/state.rs); the spec lives in §2.5.

**Tool transport.** `atelier-core` ships an MCP client (via the `rmcp` crate). Any MCP-compliant server — filesystem, GitHub, web search, databases, custom — can be registered via `mcp_servers.json` (schema: `schemas/config/mcp_servers.v1.json`) without writing Atelier-specific glue. Built-in tools (file ops, shell, search) are exposed through the same interface, so verification gates, hooks, ledger, and trust budget treat built-in and external tools uniformly. See spec §15. For `rmcp` dependency wiring detail, see [`crates/atelier-core/README.md`](crates/atelier-core/README.md).

---

## Build

The toolchain is **pinned Rust 1.85.0** via `rust-toolchain.toml` — the first `cargo` call inside this repo silently fetches it. See [`docs/toolchain.md`](docs/toolchain.md) for the reason and for troubleshooting the `edition2024` error if it surfaces.

### One-time setup

```sh
# Install rustup if you don't have it (Linux/macOS).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Build the `atelier` CLI

```sh
cargo build -p atelier-cli              # debug   -> target/debug/atelier
cargo build -p atelier-cli --release    # release -> target/release/atelier
cargo install --path crates/atelier-cli # install -> ~/.cargo/bin/atelier (on $PATH)
```

Smoke test:

```sh
./target/debug/atelier --version
./target/debug/atelier init --help
```

### Build / test the headless core

`atelier-core` has no UI dependencies and is the centre of gravity for every gate.

```sh
cargo build -p atelier-core
cargo test  -p atelier-core
```

### CI gates (run before opening a PR)

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test  -p atelier-core
make check                # rig: schemas + artifacts + 112 self-tests + dry-runs
```

CI runs the same set on every push/PR (`.github/workflows/check.yml` + `rust` job).

For `rmcp` dependency wiring and troubleshooting (`edition2024` error, proxy/network issues), see [`crates/atelier-core/README.md`](crates/atelier-core/README.md). For the CLI's current and planned subcommands, see [`crates/atelier-cli/README.md`](crates/atelier-cli/README.md).

---

## Configure and run

Until the Phase A agent loop lands, the runnable surface is **(a)** `atelier init` to bootstrap a project, and **(b)** `make check` to drive the calibration rig that will verify the harness as it's built. The planned `atelier run` flow is sketched at the bottom of this section.

### 1. Bootstrap a project — `atelier init`

From the root of any repo:

```sh
atelier init                # current directory
atelier init /path/to/repo  # explicit path
```

This creates `<repo>/.atelier/{sessions,tools,hooks}/`, writes a seeded `ATELIER.md` at the repo root if none is present (template: [`crates/atelier-core/templates/ATELIER.md`](crates/atelier-core/templates/ATELIER.md)), and appends `.atelier/` to an existing `.gitignore`.

`atelier init` is **idempotent** and **never overwrites an existing `ATELIER.md`**. Re-running on an initialised repo prints `atelier init: no changes (repo already initialised)`.

`ATELIER.md` is the project-level user-config file — the harness reads it at session start and injects it into the system prompt. Equivalent to Cursor's `.cursorrules` / Claude Code's `CLAUDE.md`.

### 2. Project layout after `init`

```
<repo>/
├── .atelier/
│   ├── sessions/   # per-session state, checkpoints, ledger (.atelier/sessions/<uuid>/)
│   ├── tools/      # user-supplied tool manifests; see examples/tools/
│   └── hooks/      # pre-tool / post-tool / on-verify-* hook scripts; see examples/hooks/
├── ATELIER.md      # system-prompt config; edit freely
└── .gitignore      # ".atelier/" appended if a .gitignore exists
```

Reference manifests for tools, hooks, skills, sub-agents, and config (`mcp_servers.json`, `permission_shapes.json`, …) live in [`examples/`](examples/). Validate them against `schemas/` before wiring them in.

### 3. Run the rig

The rig is the agent-loop verifier today. It runs the 11 canonical workload fixtures in dry-run mode, validates every artifact against its schema, and runs 112 self-tests.

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

### 4. Drive the harness — `atelier run` *(coming with Phase A)*

The Phase A target is a single subcommand that drives the §2.5 loop end-to-end:

```sh
atelier run "fix the failing test in src/parser.rs"
```

This will: load `ATELIER.md` into the system prompt, open a session under `.atelier/sessions/<uuid>/`, call the configured BYOM adapter (Anthropic first; LiteLLM-shaped next), stream tool calls through the unified MCP dispatch (built-in + external servers), apply edits atomically (`tempfile` + tree-sitter pre-commit check, spec §3), and either transition to `Verifying` on `claimed_done: true` or fail explicitly. Cost-ledger entries land per call; session JSON conforms to `schemas/session/v1.json`.

Piece-by-piece state of the Phase A build — what's done, what's planned, where each piece lands in the tree — is in [`STATUS.md`](STATUS.md#phase-a--piece-by-piece-tracker).

---

## What's intentionally absent

- **No CI provider beyond GitHub Actions.** The Makefile is portable; other providers (Buildkite, GitLab CI) can wrap `make check` similarly.
- **No agent-loop runtime yet.** The spec describes one; the rig is ready to measure it; Phase A is building it.

---

## License

Apache 2.0. See [`LICENSE`](LICENSE). All Phase A code (`atelier-core`, `atelier-cli`, `atelier-gui`, `atelier-tui`) inherits this license via the workspace `Cargo.toml`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop, conventions, and PR process. Spec questions and design proposals belong in GitHub Discussions; bugs and feature requests use the templates under `.github/ISSUE_TEMPLATE/`. `make check`, `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test -p atelier-core` must all be green before opening a PR — CI runs the same set.
