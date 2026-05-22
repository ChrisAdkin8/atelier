<p align="center">
  <img src="assets/banner-loop.png" alt="atelier — a coding harness for BYOM agents: the agent loop, MCP transport, and verification gates between a model and your repo" width="100%">
</p>

<p align="center">
  <a href="#what-makes-it-different"><b>Why</b></a> ·
  <a href="#quick-start"><b>Quick start</b></a> ·
  <a href="coding-harness-spec.md"><b>Spec</b></a> ·
  <a href="#build"><b>Build</b></a> ·
  <a href="#how-a-run-works"><b>How a run works</b></a> ·
  <a href="STATUS.md"><b>Status</b></a> ·
  <a href="CONTRIBUTING.md"><b>Contributing</b></a>
</p>

# Atelier

[![check](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml/badge.svg)](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Atelier is a coding harness for AI software engineering** — the agent loop, tool transport, verification gates, hooks, and cost ledger that sit between a model and your repository. It is built spec-first: a complete specification, JSON schemas, canonical workload fixtures, and a self-testing calibration rig exist *before* the harness, so the harness can be measured against fixed criteria as it lands.

The spec is in [`coding-harness-spec.md`](coding-harness-spec.md). Where the build currently stands — what has landed, what is in flight — is in [`STATUS.md`](STATUS.md).

Recent quality/design notes are tracked in [`CODE_QUALITY_METRICS.md`](CODE_QUALITY_METRICS.md), [`tasks/design_risks.md`](tasks/design_risks.md), and the critical/high remediation plan in [`tasks/plan_design_risks_critical_high.md`](tasks/plan_design_risks_critical_high.md).

<p align="center">
  <img src="assets/harness-architecture.png" alt="Atelier architecture diagram showing the CLI, TUI, GUI, Runner orchestration, atelier-core runtime, BYOM adapters, tool transport, persistence, verification, and external gates" width="100%">
</p>

---

## What makes it different

- **Bring-your-own-model from day one.** Not a vendor wrapper. Provider adapters are pluggable; `atelier-core` has no SDK bias and no hardcoded host paths.
- **Verification is a state, not a claim.** The agent loop has an explicit `Verifying` transition (spec §2.5). A task is "done" only when the harness can prove it — tests pass, schemas validate, gates clear — not when the model asserts so.
- **MCP-first tool transport.** Built-in tools (file ops, shell, search) and external MCP servers (filesystem, GitHub, databases, custom) share one interface via `rmcp`. Hooks, ledger, trust budget, and verification gates treat them uniformly (spec §15).
- **A workspace with swappable frontends.** The agent is a collaborator with explicit sessions, checkpoints, hooks, and file boundaries — not a chat box with side effects. `atelier-core` ships no UI; the Tauri GUI and `ratatui` TUI consume the same broadcast channel, and a third frontend is additive.
- **Cost ledger and trust budget as first-class concerns.** Every tool call, token, and side effect is accounted for. Observability is built in, not bolted on.

---

## Quick start

Use this path when you want to try Atelier from a checkout. The first run below uses the Mock provider, so it needs **no API key, no network, and no model server**. It still exercises the real agent loop, dispatcher, staging, and session persistence.

```sh
cargo install --path crates/atelier-cli
atelier init
atelier run --provider mock "rename foo to bar"
```

That creates `.atelier/`, seeds `ATELIER.md` if needed, and writes the run record under `.atelier/sessions/<uuid>/`.

### Run against a model

Pick whichever provider you already have:

```sh
# Anthropic Messages API. Requires ANTHROPIC_API_KEY in your environment.
atelier run --provider anthropic --model anthropic:claude-opus-4-7 "<prompt>"

# OpenAI-compatible servers: Ollama, LM Studio, llama-server, vLLM, sglang, OpenAI.
atelier run --provider openai-compat \
  --base-url http://localhost:11434/v1 \
  --model local:qwen2.5-coder:7b \
  "<prompt>"
```

If your OpenAI-compatible endpoint needs a bearer token, store it in your OS keychain instead of committing it to `providers.toml`:

```sh
atelier providers auth qwen2.5-72b-awq \
  --from-command "terraform -chdir=../atelier-bedrock-infra/terraform output -raw openai_api_key"
atelier providers test qwen2.5-72b-awq
atelier run --profile qwen2.5-72b-awq "<prompt>"
```

`OPENAI_API_KEY` still wins for CI and one-off shells. Profile keys are only references such as `keyring:atelier/providers/qwen2.5-72b-awq` or `env:NAME`; plaintext secrets are rejected.

### Open the GUI

```sh
cargo install tauri-cli --version "^2.0" --locked
npm --prefix crates/atelier-gui/ui install
cd crates/atelier-gui
cargo tauri dev
```

The GUI opens as a Tauri desktop window and uses Vite at `http://127.0.0.1:1420/` during development. Use **Browse…** in the top-right to select the repo you want Atelier to operate on; the choice is saved to `~/.atelier/gui.toml`. Chat mode is for conversational model turns, while Agent mode uses the Runner and writes resumable sessions under the selected workspace. If a previous session has been deleted or cleaned up, the GUI drops that stale resume pointer and starts a fresh durable session rather than failing the next prompt.

### Bootstrap — what `atelier init` lays down

```sh
atelier init                # current directory
atelier init /path/to/repo  # explicit path
```

Idempotent — re-running on an initialised repo prints `atelier init: no changes (repo already initialised)`, and an existing `ATELIER.md` is never overwritten. The command creates `.atelier/{sessions,tools,hooks}/`, writes a seeded `ATELIER.md` (template: [`crates/atelier-core/templates/ATELIER.md`](crates/atelier-core/templates/ATELIER.md)) if none is present, and appends `.atelier/` to an existing `.gitignore`:

```
<repo>/
├── .atelier/
│   ├── sessions/       # per-session state, checkpoints, ledger
│   ├── tools/          # user tool manifests; see examples/tools/
│   ├── hooks/          # pre-/post-tool / on-verify-* scripts; see examples/hooks/
│   └── providers.toml  # provider profiles + runtime config (see below); optional
├── ATELIER.md          # system-prompt config; edit freely
└── .gitignore          # ".atelier/" appended if a .gitignore exists
```

`ATELIER.md` is the project-level user-config file — injected into the system prompt at session start, equivalent to Cursor's `.cursorrules` / Claude Code's `CLAUDE.md`. Reference manifests for tools, hooks, skills, sub-agents, and config (`mcp_servers.json`, `permission_shapes.json`, …) live in [`examples/`](examples/); validate against `schemas/` before wiring them in.

### Pin defaults — `.atelier/providers.toml`

Re-typing `--provider … --base-url … --model …` gets old fast. Drop a small TOML file in the repo and `atelier run` picks it up automatically — keep several named profiles (`local`, `cloud`, `staging`, …) side by side and switch with `--profile <NAME>`.

**Where it lives.** Two scopes are searched in order; the first that exists wins. Both are optional — missing both falls through to built-in defaults (provider `mock`, max-turns 32, probe `auto`).

| Path | Scope | Typical use |
|---|---|---|
| `<repo>/.atelier/providers.toml` | **Project** (committed) | Repo wants Anthropic with a specific model on every clone. |
| `~/.atelier/providers.toml`      | **User** (not committed) | Your machine talks to a local LM Studio. |

**Shape.** Profiles live under `[providers.<name>]` tables; the optional top-level `default` picks the one used when `--profile` is absent. Every field inside a profile is optional — a profile with only `provider = "anthropic"` inherits defaults for the rest.

```toml
# .atelier/providers.toml

default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"
api_key  = "keyring:atelier/providers/local"  # optional; no plaintext secrets

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
- **`[providers.<name>].provider`** — which adapter. `"mock"` (no network), `"anthropic"` (Messages API; reads `ANTHROPIC_API_KEY`), or `"openai-compat"` (any `POST /v1/chat/completions` server: LM Studio, llama-server, vLLM, sglang, Ollama, OpenAI itself; reads `OPENAI_API_KEY` when set, otherwise `[providers.<name>].api_key` — empty allowed for local servers).
- **`[providers.<name>].model`** — the model id sent verbatim to the server. By convention `<provider>:<model>` (`anthropic:claude-opus-4-7`, `local:qwen2.5-coder:7b`, `openai:gpt-4o-mini`). The `<provider>:` prefix is the cost-ledger label; the part after the colon is what the server matches against.
- **`[providers.<name>].base_url`** — full URL ending in `/v1`. **Only valid with `provider = "openai-compat"`** — combining it with `anthropic` or `mock` is a config error. Omit to default to `https://api.openai.com/v1` (OpenAI itself). If `OPENAI_API_KEY` is present, repo/profile-sourced URLs must be loopback or known provider hosts unless the user supplied `--base-url` explicitly for that run. The built-in trusted host list includes OpenAI, Anthropic, loopback, and the project-owned Atelier dev vLLM ALB (`http://atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com/v1`); see [`docs/trust-boundary.md`](docs/trust-boundary.md).
- **`[providers.<name>].api_key`** — optional credential reference, never a plaintext secret. Supported forms are `keyring:USER`, `keyring:SERVICE/USER`, and `env:NAME`. Use `atelier providers auth <profile> --from-command "<command>"` or `--from-stdin` to store a key in the OS keychain and add/update this field automatically. `OPENAI_API_KEY` still overrides this for CI and one-off runs.
- **`[runner].max_turns`** *(top-level)* — bail after N turns without `claimed_done`. Maps onto `--max-turns`. Built-in default `32`.
- **`[probe].policy`** *(top-level)* — v51 probe-on-first-use. `"auto"` (cache-first; probe on miss; default for `openai-compat`), `"skip"` (never probe; default for `mock` + `anthropic`), or `"force"` (re-probe even when cached).

</details>

**Override precedence**, top wins:

```text
  1. CLI flags                              (per-invocation)
  2. Resolved profile (from providers.toml) (named, persisted)
  3. Built-in defaults                      (mock, 32 turns, auto probe)
```

The resolved profile is whichever `[providers.<name>]` matches `--profile <NAME>`, or the file's `default`, or nothing (then the CLI must specify the relevant flags). Per-field flags (`--provider`, `--model`, `--base-url`, `--max-turns`, `--no-probe`/`--force-probe`) still override individual fields of the resolved profile. With the file above, `atelier run "…"` uses `local`/openai-compat; `atelier run --profile cloud "…"` flips to Anthropic; `atelier run --provider mock "…"` overrides the resolved profile's provider only (its `model`/`base_url` drop because they don't apply to `mock`).

**Verifying what's active.** Every `atelier run` prints which config file (if any) loaded and which profile resolved (`atelier run: using config /Users/you/proj/.atelier/providers.toml (profile "local")`). Once the loop starts, the GUI footer and TUI footer render the active model id + §2 strategy + probe outcome, e.g. `local:qwen2.5-coder:7b · json_sentinel · cache_hit`. The same surfaces show the §5 **Context panel**: each row lists a context item, token count, and why-here badge (`init` / `usr` / `tool` / `mem` / `pin` / `asst`). If what's active isn't what you expected, re-check the precedence above — most surprises are "user-scope file edited but project-scope file is winning."

**Errors are fatal.** A file that exists but doesn't parse (typo, wrong type, unknown field, `default` referencing a missing profile, `base_url` paired with a non-openai-compat provider) exits with code 2 and a message naming the file + what's wrong — no silent fall-through to defaults.

```text
atelier run: config error: config at /Users/you/proj/.atelier/providers.toml
is invalid: [providers.cloud].base_url is only valid when [providers.cloud].provider = "openai-compat"
(got provider = "anthropic")
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

Other servers, same `--provider openai-compat` switch — only `--base-url` changes: LM Studio (`http://localhost:1234/v1`), llama-server (`http://localhost:8080/v1`), vLLM / sglang (`http://localhost:8000/v1`), OpenAI itself (omit `--base-url`; set `OPENAI_API_KEY` or configure `api_key = "keyring:..."`).

First use against a given `(model, base_url)` fires a short calibration probe (one native tool-call test + one JSON-sentinel envelope test) and caches the resulting `ModelProfile` to `~/.atelier/model_profiles/<hash>.json` for subsequent runs. The §1 conformance tracker still degrades at runtime if the live model misbehaves — the cached profile is the *initial* strategy hint, not a contract.

<details>
<summary><b>All <code>atelier run</code> flags</b></summary>

| Flag | Purpose |
|---|---|
| `--provider {mock,anthropic,openai-compat}` | Which adapter. Overrides `[providers.<name>].provider` from `providers.toml`. |
| `--profile <NAME>` | Pick a named profile from `providers.toml`. Overrides the file's `default`. |
| `--model <ID>` | Model id (`anthropic:claude-opus-4-7`, `local:llama3:8b`, `openai:gpt-4o-mini`, …). |
| `--base-url <URL>` | OpenAI-compat only. e.g. `http://localhost:11434/v1` for Ollama. |
| `--workspace <PATH>` | Repo root; defaults to current dir. |
| `--max-turns <N>` | Bail-out cap (default 32). |
| `--prompt-file <PATH>` | Read prompt from file; `-` for stdin. |
| `--no-probe` / `--force-probe` | Skip / force the v51 probe-on-first-use calibration. |

</details>

### Multi-pane workspace

The Tauri GUI (`cargo tauri dev` from `crates/atelier-gui/`) and the ratatui TUI (`cargo run -p atelier-tui -- "<prompt>"`) expose the same engine through different surfaces. The GUI is the easiest daily workspace: chat/agent composer, provider swapping, Context, Memory, Plan, Sub-agent, and meters panels. The TUI is the most complete live-agent surface today: conversation, context/memory/plan, diff, meters, scrubber keys, sub-agent token counts, and file-level accept/reject.

Runner-backed GUI Agent turns persist through the same `.atelier/sessions/<uuid>/` layout as the CLI. Follow-up prompts resume from the durable session UUID, and the GUI validates the manifest before resuming so a deleted session directory does not poison the next submit.

For first-time build prerequisites (rustup, pinned toolchain), see [Build](#build) below. For what each run actually does inside the agent loop, see [How a run works](#how-a-run-works). Piece-by-piece state of the build is in [`STATUS.md`](STATUS.md#phase-a--piece-by-piece-tracker).

---

## Layout

Atelier is a **Rust workspace**. Four crates under [`crates/`](crates/):

| Crate | Role |
|---|---|
| [`atelier-core`](crates/atelier-core/) | Agent loop, BYOM adapters (Mock + Anthropic + OpenAI-compatible as of v50), session state, dispatcher, eight built-in tools, cost ledger, §1 probe-on-first-use cache (v51). **No UI dependencies.** The §2.5 state machine lives here. |
| [`atelier-cli`](crates/atelier-cli/) | Hybrid lib + binary. The `atelier` binary provides `atelier init` and `atelier run` (the end-to-end agent-loop driver); the library exports a `Runner` the GUI and TUI link against for their own driver modes. |
| [`atelier-gui`](crates/atelier-gui/) | Tauri 2.x + Svelte 5 shell. Chat/Agent composer with Context, Memory, Plan, Sub-agent, and meters panels; provider swapping; inline Mermaid / image rendering; drag-and-drop plan reorder; concurrent-run guard; and durable session-resume validation. |
| [`atelier-tui`](crates/atelier-tui/) | `ratatui` + `crossterm` driver. Live agent workspace with conversation, context/memory/plan, diff, meters, scrubber keys `[` `]` `g`, and file-level accept/reject from the diff pane. Run with `cargo run -p atelier-tui -- "<prompt>"` for driver mode, no argument for viewer mode. |

### Rust stack

| Area | Components | Role in Atelier |
|---|---|---|
| **Toolchain / workspace** | Rust 1.85, edition 2021, Cargo workspace resolver 2, `Cargo.lock`, thin-LTO release profile | Pins the build across all crates and keeps shared dependency versions centralised in the root `Cargo.toml`. |
| **Internal crates** | `atelier-core`, `atelier-cli`, `atelier-gui`, `atelier-tui` | Splits the harness into a UI-free core, a reusable runner/CLI, a Tauri desktop shell, and a terminal UI. |
| **Async runtime** | `tokio`, `tokio-util`, `futures`, `async-trait` | Drives the agent loop, adapter calls, cancellation tokens, channels, subagents, MCP/LSP subprocesses, and async trait boundaries. |
| **Serialization / config / schemas** | `serde`, `serde_json`, `toml`, `jsonschema` | Loads provider/config TOML, parses typed envelopes, persists sessions/ledgers/audit records, validates tool manifests and schema-backed artifacts. |
| **HTTP and streaming IO** | `reqwest`, `bytes` | Talks to Anthropic, OpenAI-compatible endpoints, and HTTP/SSE MCP servers; handles streaming response bodies. |
| **MCP transport** | `rmcp` | Provides the Model Context Protocol client, stdio child-process transport, and SSE transport used by external tool servers. |
| **GUI shell** | `tauri`, `tauri-build`, `tauri-plugin-dialog` | Hosts the Svelte desktop frontend, exposes Rust commands, embeds production assets via `custom-protocol`, and opens native folder-picker dialogs. |
| **Terminal UI** | `ratatui`, `crossterm` | Renders the TUI workspace and handles terminal input, alternate-screen mode, key events, and live pane updates. |
| **LSP verification** | `async-lsp`, `lsp-types`, `tower` | Launches language-server sessions and converts diagnostics into verification discrepancies for the Tier-1 hallucination detector. |
| **Filesystem and process safety** | `notify`, `tempfile`, `sha2`, `libc` | Watches concurrent edits, performs atomic writes/staging, hashes pre-edit content and checkpoints, and hard-kills timed-out Unix process groups. |
| **Diff and syntax checks** | `similar`, `tree-sitter`, `tree-sitter-json` | Produces incremental diff/hunk data and runs pre-commit syntax checks before staged edits are committed. |
| **Built-in tool helpers** | `regex`, `walkdir` | Implements repository grep and recursive directory traversal for built-in search/listing tools. |
| **Errors, logging, shared state** | `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `parking_lot`, `uuid` | Provides typed errors, contextual errors, structured logs, non-poisoning locks for shared state, and IDs for sessions/commits/events. |
| **Credentials** | `keyring` | Supports OS-backed credential storage for provider credentials and future auth surfaces. |
| **Rust tests / test seams** | `assert_cmd`, `predicates`, `wiremock` | Spawns CLI integration tests, asserts stdout/stderr/exit behaviour, and stubs provider HTTP APIs without live credentials. |
| **Spike crates** | `experiments/rmcp_spike`, `experiments/lsp_spike` | Captures dependency maturity checks for rmcp/MCP and async-lsp before those paths are promoted into `atelier-core`. |

Top-level tree:

```
.
├── coding-harness-spec.md   the spec
├── STATUS.md                what's landed / in flight / planned
├── CHANGELOG.md             spec + rig revisions
├── Cargo.toml               Rust workspace root (pins rmcp = "0.1")
├── rust-toolchain.toml      pinned Rust 1.85.0
├── crates/                  atelier-core / -cli / -gui / -tui
├── schemas/                 26 JSON Schemas (see schemas/README.md)
├── tests/                   the calibration rig (validators, fixtures, runner)
├── examples/                reference manifests (tools, hooks, skills, subagents, config)
├── prompts/                 Model Protocol few-shot examples
├── experiments/             one-off spikes (e.g. rmcp_spike)
├── tasks/                   build plan, design risks, remediation plans
├── ci/                      nightly CI job stubs
├── docs/                    toolchain, layout, and trust-boundary reference docs
└── .github/                 workflows, PR template, issue templates
```

For the exhaustive tree with one-line annotations on every subdirectory, see [`docs/layout.md`](docs/layout.md).

**Agent loop.** Single-turn streaming state machine on `tokio`. Cancellation uses Rust drop semantics — no invented cancel protocol. Verification is a state transition (`Verifying`), not an agent capability. The states and the legal transition table are in [`crates/atelier-core/src/state.rs`](crates/atelier-core/src/state.rs); the spec lives in §2.5.

**Tool transport.** `atelier-core` ships an MCP client (via the `rmcp` crate). Any MCP-compliant server — filesystem, GitHub, web search, databases, custom — can be registered via `mcp_servers.json` (schema: `schemas/config/mcp_servers.v1.json`) without writing Atelier-specific glue. Built-in tools (file ops, shell, search) are exposed through the same interface, so verification gates, hooks, ledger, and trust budget treat built-in and external tools uniformly. See spec §15. For `rmcp` dependency wiring detail, see [`crates/atelier-core/README.md`](crates/atelier-core/README.md).

---

## Memory

Atelier remembers things across sessions — your preferences, project conventions, gotchas you hit once and don't want to hit again — by writing **memory cards** to disk and replaying them as a system-prompt prefix on every chat turn. There are two places cards live, and that's deliberate: some things you want to follow you everywhere (your favourite languages, your code-review tone), others belong to one repo and would be noise outside it (this project's pinned Rust version, the local server's port).

| Action in the GUI | Where the card ends up | When to use it |
|---|---|---|
| **Promote** an in-session card from the Memory panel | `~/.atelier/memory/<slug>.md` — your personal "atelier root" | For facts that apply across every project — your name, your tone preferences, your standard tooling, anything you'd otherwise re-explain in each new session. |
| **Add** a card in the Memory panel and leave it un-promoted | In-session only (memory, not disk) | For one-off notes you want during the current chat but don't need to survive a relaunch. |
| **Auto-drafted** when the harness hits a recognised error | `<workspace>/.atelier/memory/auto_<slug>.md` — scoped to the current project | Happens for you: when `adapter.chat()` returns an `Auth` / `Unreachable` / `ContextOverflow` / `RateLimited` / etc. error, the harness drops a markdown card describing what went wrong and the likely fix, then mentions it in the chat. Repeat occurrences overwrite the same file. |
| **Hand-written** at `<workspace>/.atelier/memory/<your-slug>.md` | Same as auto-drafted — workspace-scope | For facts you want the agent to know in *this* repo but nowhere else — its quirks, its conventions, its decision history. |

On every chat turn the agent loads both directories (workspace **and** atelier root), strips the Jekyll-style frontmatter, and includes them as a system message before your prompt. Total memory is capped at 16 KiB so a runaway memory directory can't blow the context window; older / lexicographically-later cards are dropped first with a `tracing::warn`.

**The format.** A memory card is just a markdown file with a small frontmatter block:

```markdown
---
name: my-card-slug
description: One-liner summary the agent sees as the TL;DR.
metadata:
  type: feedback
---

Body of the card. The agent reads this verbatim. Write it as if you were
briefing a colleague who just walked into the room.
```

**Picking your workspace.** The Memory feature only works once you've pointed the GUI at a real folder (otherwise the workspace is a throwaway tempdir and any cards written there are deleted on shutdown). Use the **`Browse…`** button in the top-right of the GUI to open a native folder picker; the choice persists to `~/.atelier/gui.toml` so the next launch picks up where you left off. Auto-drafting is skipped silently while the workspace is still the tempdir, so you'll never end up with orphan cards in `/var/folders/`.

---

## Skills

Skills are named, slash-invoked prompt expansions — `/review`, `/fix`, `/audit`, etc. The harness ships 19 bundled skills out of the box; you can override any of them in `~/.atelier/skills/` (your scope) or `<workspace>/.atelier/skills/` (per-repo, checked into git so the team shares the same shortcut).

When you type `/review` in the GUI Composer (or `atelier run /review` on the CLI), the harness looks up the skill's manifest, expands `${arg}` substitutions against any args you passed, and sends the expanded body as your next user turn. The §2.5 agent loop runs unchanged — skills don't introduce a new state, they just save you typing the same prompt repeatedly.

| Action | Where it lands | When to use it |
|---|---|---|
| **Type `/<name>`** in the Composer | The model sees the expanded `prompt_template` | The everyday case — type `/review` and the model gets the bundled review prompt. |
| **`atelier skills new <name>` (CLI)** | Per-repo (`--scope repo`, default) or user-home (`--scope user`) | Author a new skill. Use `--from <existing>` to fork the bundled body and customise it. |
| **`atelier skills validate`** | Lints every registered manifest (or one path) | Pre-commit hook–friendly. Catches typos in `${arg}` references and bad slugs. |
| **`atelier skills show <name>`** | Prints the resolved manifest + its source path | "Is *this* the one I edited?" |

The full bundled set: `/review`, `/security-review`, `/test`, `/explain`, `/fix`, `/document`, `/refactor`, `/optimize`, `/commit`, `/changelog`, `/audit`, `/spec`, `/sweep`, `/scan`, `/plan`, `/diagram`, `/triage`, `/release`, `/document-sweep`. Run `atelier skills` for the live list with descriptions + override sources.

**Substitution variables** available in `prompt_template`:

- `${<arg_name>}` — declared args from the manifest's `args` list.
- `${repo_root}` — absolute path of the repo root.
- `${atelier_md}` — contents of `<repo>/ATELIER.md`, or empty if absent.

**Layered override**: per-repo wins over user-home, which wins over bundled. `atelier skills delete <name>` removes a user/repo manifest and unshadows whatever was below it. See `crates/atelier-core/skills/` for the bundled manifests and `examples/skills/` for hand-rolled examples covering the full schema surface.

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
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace
make check                # rig: schemas + artifacts + 185 self-tests + dry-runs
```

CI runs the same set on every push/PR (`.github/workflows/check.yml` + `rust` job).

For `rmcp` dependency wiring and troubleshooting (`edition2024` error, proxy/network issues), see [`crates/atelier-core/README.md`](crates/atelier-core/README.md). For the CLI's current and planned subcommands, see [`crates/atelier-cli/README.md`](crates/atelier-cli/README.md).

---

## How a run works

`atelier run` is the end-to-end agent-loop driver. Three provider families are live today: Mock, Anthropic Messages API, and any OpenAI-compatible `POST /v1/chat/completions` server. Invocations + configuration are in [Quick start](#quick-start).

Under the hood, each run loads `ATELIER.md` into the system prompt, opens a session under `.atelier/sessions/<uuid>/`, calls the configured BYOM adapter, streams tool calls through the §15 dispatcher (eight built-in tools plus registered MCP tools), applies edits atomically (`tempfile` + tree-sitter pre-commit check, spec §3), and either transitions to `Verifying` on `claimed_done: true` or bails after `--max-turns`. The session manifest (`session.json`) conforms to `schemas/session/v1.json`; long append-like arrays are split into `conversation.jsonl` and `ledger.jsonl` sidecars with a `resume_index.json` cursor so `--resume` and GUI follow-up turns can reconstruct the last safe prefix without hydrating unrelated session sections.

---

## The rig

The rig is the agent-loop verifier. It runs the 11 canonical workload fixtures in dry-run mode, validates every artifact against its schema, and runs 185 self-tests. CI runs it on every push and PR.

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
- **No Bedrock / Vertex adapters yet.** Phase E/F. The OpenAI-compatible adapter already covers the bulk of the local-LLM space, OpenAI itself, vLLM, and LiteLLM-shaped gateways that expose chat-completions.
- **No signed desktop release yet.** The GUI is a Tauri dev/build target with placeholder icons and no notarized installers.
- **No generic keychain interpolation for MCP/hook manifests yet.** Provider API keys support OS-keychain references today. MCP/hook `${env:...}` interpolation remains supported; generic `${keychain:...}` interpolation still fails closed until that broader credentials surface lands.

---

## Maintainers

[@ChrisAdkin8](https://github.com/ChrisAdkin8)

## Security

See [`SECURITY.md`](SECURITY.md) for the supported-versions policy and how to report a vulnerability privately. Please don't open a public issue for suspected security defects — use the channel in `SECURITY.md` instead.

## License

Apache 2.0. See [`LICENSE`](LICENSE). All Phase A code (`atelier-core`, `atelier-cli`, `atelier-gui`, `atelier-tui`) inherits this license via the workspace `Cargo.toml`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop, conventions, and PR process. Spec questions and design proposals belong in GitHub Discussions; bugs and feature requests use the templates under `.github/ISSUE_TEMPLATE/`. `make check`, `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test -p atelier-core` must all be green before opening a PR — CI runs the same set.
