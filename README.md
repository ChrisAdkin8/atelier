<p align="center">
  <img src="assets/banner-loop.png" alt="atelier — a coding harness for BYOM agents: the agent loop, MCP transport, and verification gates between a model and your repo" width="100%">
</p>

<p align="center">
  <a href="#what-makes-it-different"><b>Why</b></a> ·
  <a href="#quick-start"><b>Quick start</b></a> ·
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

## Quick start

The Mock provider drives the full agent loop with **no network and no model** — useful as a 30-second smoke test that the loop, dispatcher, staging, and persistence are all wired up on your machine.

```sh
cargo install --path crates/atelier-cli          # builds + installs the `atelier` binary
atelier init                                      # bootstraps .atelier/ + a seeded ATELIER.md
atelier run --provider mock "rename foo to bar"   # runs a turn; session lands in .atelier/sessions/<uuid>/
```

To drive a real model, swap the provider:

```sh
# Anthropic — Messages API (set ANTHROPIC_API_KEY)
atelier run --provider anthropic --model anthropic:claude-opus-4-7 "<prompt>"

# Any OpenAI-compatible server — local (Ollama / LM Studio / llama-server / vLLM / sglang) or cloud
atelier run --provider openai-compat \
    --base-url http://localhost:11434/v1 \
    --model local:qwen2.5-coder:7b "<prompt>"
```

### Running against a local LLM

Quickest path on macOS / Linux is Ollama:

```sh
brew install ollama && brew services start ollama   # macOS; or `ollama serve` in a terminal
ollama pull qwen2.5-coder:7b                        # ~4.7 GB; fits comfortably on an M1 Pro
atelier run --provider openai-compat \
    --base-url http://localhost:11434/v1 \
    --model local:qwen2.5-coder:7b "<prompt>"
```

Other servers, same `--provider openai-compat` switch — only `--base-url` changes: LM Studio (`http://localhost:1234/v1`), llama-server (`http://localhost:8080/v1`), vLLM / sglang (`http://localhost:8000/v1`), OpenAI itself (omit `--base-url`; set `OPENAI_API_KEY`).

On first use the harness fires a short calibration probe (one native tool-call test + one JSON-sentinel envelope test) and writes a `ModelProfile` to `~/.atelier/model_profiles/<hash>.json`. Subsequent runs against the same `(model, base_url)` pair use the cached profile. The §1 conformance tracker still degrades at runtime if the live model misbehaves — the cached profile is the *initial* strategy hint, not a contract.

### Skip the re-typing

Pin the flags as a named profile in `.atelier/providers.toml` (see [§5 below](#5-configure-with-atelierproviderstoml--v53) for the full format and override-precedence rules):

```toml
default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"
```

Now `atelier run "<prompt>"` is enough. Add a `[providers.cloud]` entry pointing at Anthropic and you can flip between them with `atelier run --profile cloud "…"`.

### Multi-pane workspace

The Tauri GUI (`cargo tauri dev` from `crates/atelier-gui/`) and the ratatui TUI (`cargo run -p atelier-tui -- "<prompt>"`) drive the same loop with a live multi-pane workspace (conversation, diff with hunk accept/reject, plan canvas, cost + context meters, §5 Context panel, footer with active model badge).

For the deeper "what's happening under the hood" walkthrough, read on through **Configure and run** below. For first-time build prerequisites (rustup, pinned toolchain), see [Build](#build); piece-by-piece state of the build is in [`STATUS.md`](STATUS.md#phase-a--piece-by-piece-tracker).

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

Runnable surface today (v51):

- **`atelier init`** — bootstrap a project.
- **`atelier run`** — drive the end-to-end agent loop against Mock / Anthropic / any OpenAI-compatible server.
- **GUI + TUI driver modes** — the same loop with a live, multi-pane workspace.
- **`make check`** — the calibration rig that verifies the harness on every push.

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

The end-to-end agent-loop driver. Three providers live today (v51): Mock, Anthropic Messages API, and any OpenAI-compatible `POST /v1/chat/completions` server. Invocations are in [Quick start](#quick-start) above.

Under the hood, each run: loads `ATELIER.md` into the system prompt, opens a session under `.atelier/sessions/<uuid>/`, calls the configured BYOM adapter, streams tool calls through the §15 dispatcher (seven built-in tools — MCP-hosted external tools land when the `rmcp` spike clears), applies edits atomically (`tempfile` + tree-sitter pre-commit check, spec §3), and either transitions to `Verifying` on `claimed_done: true` or bails after `--max-turns`. Cost-ledger entries land per call; session JSON conforms to `schemas/session/v1.json`.

<details>
<summary><b>All <code>atelier run</code> flags</b></summary>

| Flag | Purpose |
|---|---|
| `--provider {mock,anthropic,openai-compat}` | Which adapter. |
| `--model <ID>` | Model id (`anthropic:claude-opus-4-7`, `local:llama3:8b`, `openai:gpt-4o-mini`, …). |
| `--base-url <URL>` | OpenAI-compat only. e.g. `http://localhost:11434/v1` for Ollama. |
| `--workspace <PATH>` | Repo root; defaults to current dir. |
| `--max-turns <N>` | Bail-out cap (default 32). |
| `--prompt-file <PATH>` | Read prompt from file; `-` for stdin. |
| `--no-probe` / `--force-probe` | Skip / force the v51 probe-on-first-use calibration. |

</details>

### 5. Configure with `.atelier/providers.toml`  *(v53)*

Re-typing `--provider openai-compat --base-url … --model …` every invocation gets old fast. Drop a small TOML file in the repo and `atelier run` picks it up automatically — you can keep several named profiles (`local`, `cloud`, `staging`, …) side by side and switch between them with one flag.

#### Where the file lives

Two scopes are searched, in order. The first that exists wins:

| Path | Scope | Typical use |
|---|---|---|
| `<repo>/.atelier/providers.toml` | **Project** | The repo wants Anthropic with a specific model on every clone. Committed. |
| `~/.atelier/providers.toml`      | **User**    | Your machine talks to a local LM Studio. Not committed; lives in your home dir. |

Both files are optional. Missing both is fine — `atelier run` falls through to built-in defaults (provider `mock`, max-turns 32, probe `auto`).

#### The shape

Profiles live under `[providers.<name>]` tables; `default = "<name>"` (optional) picks which one `atelier run` uses when no `--profile` flag is given. Every field inside a profile is optional — a profile with only `provider = "anthropic"` is valid and inherits defaults for the rest.

```toml
# .atelier/providers.toml

default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"

[providers.cloud]
provider = "anthropic"
model    = "anthropic:claude-opus-4-7"

# Orthogonal runtime knobs — same file, top-level sections.

[runner]
max_turns = 32

[probe]
policy = "auto"                          # "auto" | "skip" | "force"
```

<details>
<summary><b>Field-by-field reference</b></summary>

- **`default`** *(top-level, optional)* — name of the `[providers.<name>]` table to use when `--profile` isn't passed. Must reference an existing table; a typo here is a config error, not a silent fall-through.
- **`[providers.<name>].provider`** — which adapter. `"mock"` (no network), `"anthropic"` (Messages API; reads `ANTHROPIC_API_KEY`), or `"openai-compat"` (any `POST /v1/chat/completions` server: LM Studio, llama-server, vLLM, sglang, Ollama, OpenAI itself; reads `OPENAI_API_KEY` — empty allowed for local servers).
- **`[providers.<name>].model`** — the model id sent verbatim to the server. By convention `<provider>:<model>` (`anthropic:claude-opus-4-7`, `local:qwen2.5-coder:7b`, `openai:gpt-4o-mini`). The `<provider>:` prefix is the cost-ledger label; the part after the colon is what the server matches against.
- **`[providers.<name>].base_url`** — full URL ending in `/v1`. **Only valid with `provider = "openai-compat"`** — combining it with `anthropic` or `mock` is a config error. Omit to default to `https://api.openai.com/v1` (OpenAI itself).
- **`[runner].max_turns`** *(top-level)* — bail after N turns without `claimed_done`. Maps onto `--max-turns`. Built-in default `32`.
- **`[probe].policy`** *(top-level)* — v51 probe-on-first-use. `"auto"` (cache-first; probe on miss; default for `openai-compat`), `"skip"` (never probe; default for `mock` + `anthropic`), or `"force"` (re-probe even when cached).

</details>

#### Override precedence

Top wins; layers compose:

```text
  1. CLI flags                                    (per-invocation)
  2. Resolved profile (from providers.toml)       (named, persisted)
  3. Built-in defaults                            (mock, 32 turns, auto probe)
```

The "resolved profile" is whichever `[providers.<name>]` table matches `--profile <NAME>` from the CLI, or the file's `default` field, or nothing at all (in which case the CLI must specify the relevant flags directly). Per-field flags (`--provider`, `--model`, `--base-url`, `--max-turns`, `--no-probe`/`--force-probe`) still override individual fields of the resolved profile.

Concrete examples, given the file above:

| Command | Resolved profile | Adapter | Notes |
|---|---|---|---|
| `atelier run "fix it"`            | `local` (via `default`)   | openai-compat | What you'd run day-to-day. |
| `atelier run --profile cloud "…"` | `cloud` (via CLI)         | anthropic     | Same file, different profile. |
| `atelier run --provider mock "…"` | `local` (still resolved) → overridden | mock | CLI wins; `model`/`base_url` from the profile drop because they don't apply to `mock`. |

#### Verifying what's active

On every `atelier run` the binary prints exactly which config file (if any) it loaded, and which profile it resolved:

```text
atelier run: using config /Users/you/proj/.atelier/providers.toml (profile "local")
```

Once the loop starts, the GUI footer (bottom-right) and the TUI footer (right side of the help line) both render the active model id, §2 strategy, and probe outcome:

```text
local:qwen2.5-coder:7b · json_sentinel · cache_hit
```

The same surfaces show the v53 §5 **Context panel** in the bottom-right of the workspace — per-row listing of every item in the agent's context window with a token count (colour-cued by source: cyan exact / yellow approx / dim unavailable) and a why-here badge (`init` / `usr` / `tool` / `mem` / `pin` / `asst`). Useful when you want to know exactly what the model is seeing.

If something isn't what you expected there, re-check the precedence above — most surprises are "I edited the user-scope file but the project-scope file is winning."

#### When the config is rejected

A file that exists but doesn't parse (typo, wrong type, unknown field, `default` referencing a missing profile, `base_url` paired with a non-openai-compat provider) is **fatal**: `atelier run` exits with code 2 and tells you which file + what's wrong. Silently ignoring a malformed config would let a typo silently fall back to defaults, which is exactly the surprise this layer exists to prevent.

```text
atelier run: config error: config at /Users/you/proj/.atelier/providers.toml
is invalid: [providers.cloud].base_url is only valid when [providers.cloud].provider = "openai-compat"
(got provider = "anthropic")
```

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
