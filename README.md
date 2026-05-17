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
| [`atelier-core`](crates/atelier-core/) | Agent loop, BYOM adapters (Mock + Anthropic + OpenAI-compatible as of v50), session state, dispatcher, seven built-in tools, cost ledger, §1 probe-on-first-use cache (v51). **No UI dependencies.** The §2.5 state machine lives here. |
| [`atelier-cli`](crates/atelier-cli/) | Hybrid lib + binary. The `atelier` binary provides `atelier init` and `atelier run` (the end-to-end agent-loop driver); the library exports a `Runner` the GUI and TUI link against for their own driver modes. |
| [`atelier-gui`](crates/atelier-gui/) | Tauri 2.x + Svelte 5 driver. Multi-pane workspace (conversation / diff / plan / meters / composer); hunk accept-reject wired through the live `SessionDispatcher`; concurrent-run guard + per-run UUID workspaces. |
| [`atelier-tui`](crates/atelier-tui/) | `ratatui` + `crossterm` driver. Same panes as the GUI plus scrubber keys `[` `]` `g`; `y` / `n` route through `SessionDispatcher::submit_approval`. Run with `cargo run -p atelier-tui -- "<prompt>"` for driver mode, no argument for viewer mode. |

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

The runnable surface today (v51): **(a)** `atelier init` to bootstrap a project, **(b)** `atelier run` to drive the end-to-end agent loop against Mock / Anthropic / any OpenAI-compatible server, **(c)** the GUI and TUI driver modes for the same loop with a visible workspace, and **(d)** `make check` to drive the calibration rig that verifies the harness on every push.

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

### 4. Drive the harness — `atelier run`

The end-to-end agent-loop driver. Three providers live today (v51):

```sh
# Mock — for tests + dev-loop walkthroughs (no network)
atelier run --provider mock "anything goes"

# Anthropic Messages API (set ANTHROPIC_API_KEY)
atelier run --provider anthropic --model anthropic:claude-opus-4-7 \
    "fix the failing test in src/parser.rs"

# OpenAI-compatible — any server speaking POST /v1/chat/completions
# (Ollama, LM Studio, llama-server, vLLM, sglang, OpenAI itself)
atelier run --provider openai-compat \
    --base-url http://localhost:11434/v1 \
    --model local:qwen2.5-coder:7b \
    "add a hello() function to src/main.rs"
```

What this does: loads `ATELIER.md` into the system prompt, opens a session under `.atelier/sessions/<uuid>/`, calls the configured BYOM adapter, streams tool calls through the §15 dispatcher (seven built-in tools — MCP-hosted external tools land when the `rmcp` spike clears), applies edits atomically (`tempfile` + tree-sitter pre-commit check, spec §3), and either transitions to `Verifying` on `claimed_done: true` or bails after `--max-turns`. Cost-ledger entries land per call; session JSON conforms to `schemas/session/v1.json`.

Useful flags:

| Flag | Purpose |
|---|---|
| `--provider {mock,anthropic,openai-compat}` | Which adapter. |
| `--model <ID>` | Model id (`anthropic:claude-opus-4-7`, `local:llama3:8b`, `openai:gpt-4o-mini`, …). |
| `--base-url <URL>` | OpenAI-compat only. e.g. `http://localhost:11434/v1` for Ollama. |
| `--workspace <PATH>` | Repo root; defaults to current dir. |
| `--max-turns <N>` | Bail-out cap (default 32). |
| `--prompt-file <PATH>` | Read prompt from file; `-` for stdin. |
| `--no-probe` / `--force-probe` | Skip / force the v51 probe-on-first-use calibration. |

### 5. Running against a local LLM

Quickest path on macOS / Linux:

```sh
brew install ollama && brew services start ollama   # macOS; or `ollama serve` in a terminal
ollama pull qwen2.5-coder:7b                        # ~4.7 GB; fits comfortably on an M1 Pro
atelier run --provider openai-compat \
    --base-url http://localhost:11434/v1 \
    --model local:qwen2.5-coder:7b \
    "<prompt>"
```

On first use the harness fires a short calibration probe (one native tool-call test + one JSON-sentinel envelope test) and writes a `ModelProfile` to `~/.atelier/model_profiles/<hash>.json`. Subsequent runs against the same `(model, base_url)` pair use the cached profile. The §1 conformance tracker still degrades at runtime if the live model misbehaves — the cached profile is the *initial* strategy hint, not a contract.

LM Studio (`http://localhost:1234/v1`), llama-server (`http://localhost:8080/v1`), vLLM / sglang (`http://localhost:8000/v1`), and OpenAI itself (omit `--base-url`; set `OPENAI_API_KEY`) all work through the same `--provider openai-compat` switch.

### 6. Driver-mode GUI and TUI

The same `Runner` powers both UIs. Run the GUI with `cargo tauri dev` (in `crates/atelier-gui/`); type a prompt into the Composer and the multi-pane workspace renders conversation / diff / plan / meters live. The TUI runs as `cargo run -p atelier-tui -- "<prompt>"` (driver mode) or `cargo run -p atelier-tui` (viewer mode). Both speak the same broadcast bus and the same `SessionDispatcher::submit_approval` round-trip for hunk accept/reject.

Piece-by-piece state of the build — what's landed, what's planned, where each piece lives in the tree — is in [`STATUS.md`](STATUS.md#phase-a--piece-by-piece-tracker).

---

## What's intentionally absent

- **No CI provider beyond GitHub Actions.** The Makefile is portable; other providers (Buildkite, GitLab CI) can wrap `make check` similarly.
- **No MCP client yet.** Built-in tools (file ops + shell + search) run end-to-end through the dispatcher; the `rmcp`-based MCP client for external tool servers is gated on the spike at [`experiments/rmcp_spike/`](experiments/rmcp_spike/).
- **No Bedrock / Vertex adapters yet.** Phase E/F. The OpenAI-compatible adapter covers the bulk of the local-LLM space and OpenAI itself; the LiteLLM-shaped gateway may not need a separate adapter once that surface is in.

---

## License

Apache 2.0. See [`LICENSE`](LICENSE). All Phase A code (`atelier-core`, `atelier-cli`, `atelier-gui`, `atelier-tui`) inherits this license via the workspace `Cargo.toml`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop, conventions, and PR process. Spec questions and design proposals belong in GitHub Discussions; bugs and feature requests use the templates under `.github/ISSUE_TEMPLATE/`. `make check`, `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test -p atelier-core` must all be green before opening a PR — CI runs the same set.
