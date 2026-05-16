<p align="center">
  <img src="assets/banner-loop.png" alt="atelier — a coding harness for BYOM agents: the agent loop, MCP transport, and verification gates between a model and your repo" width="100%">
</p>

# Atelier

[![check](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml/badge.svg)](https://github.com/ChrisAdkin8/atelier/actions/workflows/check.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Atelier is a coding harness for AI software engineering — the agent loop, tool transport, verification gates, hooks, and cost ledger that sit between a model and your repository.** It is built spec-first: a complete specification, JSON schemas, canonical workload fixtures, and a self-testing calibration rig exist *before* the harness, so the harness can be measured against fixed criteria as it lands.

### What makes it different

- **Bring-your-own-model from day one.** Not a vendor wrapper. Provider adapters are pluggable; `atelier-core` has no SDK bias and no hardcoded host paths.
- **Verification is a state, not a claim.** The agent loop has an explicit `Verifying` transition (spec §2.5). A task is "done" only when the harness can prove it — tests pass, schemas validate, gates clear — not when the model asserts so.
- **MCP-first tool transport.** Built-in tools (file ops, shell, search) and external MCP servers (filesystem, GitHub, databases, custom) share one interface via `rmcp`. Hooks, ledger, trust budget, and verification gates treat them uniformly (spec §15).
- **Headless core, swappable frontends.** `atelier-core` ships no UI. The Tauri GUI and `ratatui` TUI consume the same broadcast channel; a third frontend is additive, not invasive.
- **Cost ledger and trust budget as first-class concerns.** Every tool call, token, and side effect is accounted for. Observability is built in, not bolted on.
- **The AI is a collaborator in a workspace, not a chat box with side effects.** Sessions, checkpoints, hooks, and file boundaries are explicit.

The spec is in [`coding-harness-spec.md`](coding-harness-spec.md). The supporting calibration rig is wired and self-testing; the harness itself is the next phase.

---

## Status

**The agent loop is not yet end-to-end runnable**, but the Phase A foundation and the Phase B protocol/verification subset have landed under [`crates/atelier-core/src/`](crates/atelier-core/src/): the §2.5 per-session actor (tokio runtime, mpsc inbox, broadcast event bus, bounded tool semaphore, drop-on-cancel), §3 atomic diff staging (tempfile + tree-sitter JSON pre-commit + SHA-256 conflict check), §11 sandbox profile generators (macOS `sandbox-exec` `.sb` + Linux `bwrap` argv), the §14 on-disk session + global registry + recovery-log scaffold, the §15 hook manifest loader with first-use approval store, the §2 typed envelope + three emission strategies (`native_tool` / `json_sentinel` / `regex_prose`) + downshifting conformance tracker, the §7 did-it-do-what-it-said diff (`Discrepancy::{Claimed, Unclaimed, KindMismatch, DuplicateClaim}`), and the §7 per-repo DoD config (`schemas/config/dod.v1.json` + loader). Plus the existing §2.5 state-machine, the §2.5 tool-error taxonomy, and the `atelier init` bootstrap. What's still in flight under [Phase A](tasks/todo.md): the BYOM adapter trait + Anthropic/LiteLLM adapters, the MCP client (gated on the `rmcp` spike), the tool dispatcher, the file-watcher integration, the `atelier run` CLI, the Phase B real-model conformance gate, and the Tier-1 hallucination detector (gated on Q3 LSP UX).

| | Count | Where |
|---|---|---|
| Specification sections | — | [`coding-harness-spec.md`](coding-harness-spec.md) |
| JSON schemas (Draft 2020-12) | 21 | [`schemas/`](schemas/) |
| Canonical workload fixtures | 11 (10 Py + 1 TS) | [`tests/workload/canonical/`](tests/workload/canonical/) |
| Validated artifacts | 52 | [`tests/`](tests/) |
| Rig self-tests | 112 | [`tests/`](tests/) |
| `atelier-core` Rust unit tests | 379 | [`crates/atelier-core/src/`](crates/atelier-core/src/) |
| `atelier-cli` integration tests | 10 | [`crates/atelier-cli/tests/`](crates/atelier-cli/tests/) |
| `atelier-gui` unit tests | 6 | [`crates/atelier-gui/src/lib.rs`](crates/atelier-gui/src/lib.rs) |
| `atelier-tui` unit tests | 10 | [`crates/atelier-tui/src/lib.rs`](crates/atelier-tui/src/lib.rs) |
| Phased build plan | see file | [`tasks/todo.md`](tasks/todo.md) |

`make check` runs schema meta-validation → artifact validation → rig self-tests → workload dry-run. All currently green; CI runs the same pipeline on every push/PR alongside `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test -p atelier-core`.

For the invariants the rig *enforces* (no-op-harness exploits, cross-schema `$ref` resolution, schema versioning, BYOM baselines, etc.), see [`tests/README.md`](tests/README.md). For what's blocking Phase A coding, see [`tasks/todo.md`](tasks/todo.md).

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

Status of the pieces:

| Piece | Where it lands | State |
|---|---|---|
| §2.5 state machine + transition table | [`crates/atelier-core/src/state.rs`](crates/atelier-core/src/state.rs) | **done** (enum + legal-transition table + hook traits + unit tests) |
| §2.5 tool error taxonomy | [`crates/atelier-core/src/error.rs`](crates/atelier-core/src/error.rs) | **done** (incl. recovery routing tests) |
| §2.5 session actor (tokio + mpsc + broadcast + semaphore + cancellation token) | [`crates/atelier-core/src/session.rs`](crates/atelier-core/src/session.rs) | **done** (runtime mechanics; drives the state machine; broadcasts events to UI subscribers) |
| §3 atomic diff staging (tempfile + tree-sitter pre-commit + conflict check) | [`crates/atelier-core/src/staging.rs`](crates/atelier-core/src/staging.rs) | **done** (all-or-nothing per turn; JSON grammar bundled; remaining Tier-1 grammars surface as `grammar-missing` until bundled) |
| §11 sandbox profile generators (macOS `.sb` + Linux `bwrap`) | [`crates/atelier-core/src/sandbox.rs`](crates/atelier-core/src/sandbox.rs) | **done** (default deny; `/etc` and `/usr/local` writes rejected at policy-build time; subprocess launcher lands with the dispatcher) |
| §14 on-disk session + recovery log + global registry | [`crates/atelier-core/src/persistence.rs`](crates/atelier-core/src/persistence.rs) | **done** (typed `OnDiskSession` matching `schemas/session/v1.json`; atomic save; version-skew rejected on load) |
| §15 hook manifest loader + first-use approval | [`crates/atelier-core/src/hooks.rs`](crates/atelier-core/src/hooks.rs) | **done** (per-repo-overrides-global discovery; approval store at `<hook-dir>/_approvals.json`; subprocess execution lands with the dispatcher) |
| §2 envelope types | [`crates/atelier-core/src/protocol.rs`](crates/atelier-core/src/protocol.rs) | **done** (typed `Envelope` matching `schemas/model_protocol/envelope.v1.json`; round-trips all three bundled few-shot examples) |
| §2 three emission strategies (native_tool / json_sentinel / regex_prose) | [`crates/atelier-core/src/protocol_strategy.rs`](crates/atelier-core/src/protocol_strategy.rs) | **done** (encode + parse for all three; `downshift()` chain; regex-prose deliberately lossy per spec) |
| §2 conformance tracker (re-prompt + downshift + ring buffer) | [`crates/atelier-core/src/protocol_conformance.rs`](crates/atelier-core/src/protocol_conformance.rs) | **done** (`TurnConformance` decides Reprompt/Downshift/EscalateToUser; cross-call `ConformanceRingBuffer` for §1 `conformance()`) |
| §7 did-it-do-what-it-said diff | [`crates/atelier-core/src/verify.rs`](crates/atelier-core/src/verify.rs) | **done** (pure `compare(envelope, observed) -> Vec<Discrepancy>`; lying-agent gate signal) |
| §7 per-repo DoD config | [`crates/atelier-core/src/dod.rs`](crates/atelier-core/src/dod.rs) + [`schemas/config/dod.v1.json`](schemas/config/dod.v1.json) | **done** (per-repo overrides global; `by_tier` helper; consumed by the Verifying state) |
| Phase C data layer — §5 context manager | [`crates/atelier-core/src/context.rs`](crates/atelier-core/src/context.rs) | **done** (typed `ContextItem` + `ContextManager`; `evict` returns a `CacheBustEvent` for the §1 ledger; `TokenSnapshot` splits known from unavailable) |
| Phase C data layer — typed memory + plan | [`crates/atelier-core/src/memory.rs`](crates/atelier-core/src/memory.rs), [`crates/atelier-core/src/plan.rs`](crates/atelier-core/src/plan.rs) | **done** (typed `MemoryCard` + `MemoryStore::promote_to_global`; typed `PlanStep` + `PlanCanvas::apply_envelope` for the §2 `plan_update` field; `OnDiskSession.memory` and `plan.steps` retyped, session round-trip preserved) |
| Phase C data layer — incremental diff stream | [`crates/atelier-core/src/diff.rs`](crates/atelier-core/src/diff.rs) + `staging.rs` + `session::Event::EditStaged` | **done** (`Hunks::{Same, Lines, Binary, Created, Deleted}` via `similar`; per-file hunks computed at commit time, race-free with the conflict-check pre-image read; `edit_staged_events` pure translator for the bus) |
| §1 BYOM adapter trait | [`crates/atelier-core/src/adapter.rs`](crates/atelier-core/src/adapter.rs) | **done** (async `Adapter` trait + `Capabilities` matrix + `CapabilityClaim::ClaimedButBroken` flag + `MockAdapter` queueing pre-built `ChunkStream`s; deterministic `ContextOverflow` via configurable window) |
| §1 typed cost ledger | [`crates/atelier-core/src/ledger.rs`](crates/atelier-core/src/ledger.rs) | **done** (`LedgerEntry::{ModelCall, ToolCall, CacheBust}` enforces per-kind required fields at compile time; `OnDiskSession.cost_ledger` retyped, all 4 examples still round-trip; `cache_bust_from` bridges the §5 context manager's eviction event) |
| §15 tool dispatcher skeleton | [`crates/atelier-core/src/dispatcher.rs`](crates/atelier-core/src/dispatcher.rs) | **done** (async `Tool` trait + `ToolRegistry` + `Dispatcher::dispatch`; identifies hooks via `HookSet::for_tool_event`, translates `CommitReport` → `EditStaged` events, returns pure `DispatchOutcome` for caller to side-effect; failed dispatches still ledgered; unknown-tool fails closed) |
| Shared subprocess+sandbox+timeout helper | [`crates/atelier-core/src/subprocess.rs`](crates/atelier-core/src/subprocess.rs) | **done** (`run(program, args, &SubprocessSpec)` with concurrent pipe drain + timeout + reap; `sandboxed_argv` for macOS sandbox-exec / Linux bwrap; powers both shell tool and `ShellHookExecutor`) |
| `SessionDispatcher` convenience wrapper | [`crates/atelier-core/src/dispatcher.rs`](crates/atelier-core/src/dispatcher.rs) | **done** (owns `Arc<Ledger>` + `broadcast::Sender<Event>`; appends ledger entry + broadcasts events after each dispatch; no-subscribers errors swallowed silently) |
| §15 built-in tools (7) + `ShellHookExecutor` | [`crates/atelier-core/src/tools/`](crates/atelier-core/src/tools/) + `dispatcher.rs::ShellHookExecutor` | **done** (`read_file`, `list_dir`, `grep`, `write_file`, `edit_file`, `ast_grep`, `shell`; writes route through `Staging`; shell + executor share the subprocess helper; macOS sandbox profile now imports `system.sb` so subprocess loading works inside the sandbox) |
| §1 Anthropic adapter (against real Messages API) | `crates/atelier-core/src/adapter/anthropic.rs` | planned (next; trait + `MockAdapter` already in place — Anthropic impl is self-contained) |
| §15 MCP client + tool dispatcher | `crates/atelier-core/src/mcp/`, `tools/` | planned (gated on the `rmcp` spike at [`experiments/rmcp_spike/`](experiments/rmcp_spike/); 8 built-in tool manifests already bundled under [`crates/atelier-core/tools/`](crates/atelier-core/tools/)) |
| §14 file-watcher (fsevents/inotify) + concurrent-edit modal | TBD | planned (needs the tool dispatcher's read-set tracking) |
| `atelier run` CLI subcommand | [`crates/atelier-cli/`](crates/atelier-cli/) | planned (after the adapter + dispatcher land) |
| §1 LiteLLM adapter + Phase A mechanical gate | various | planned (closes Phase A) |

The full ordered build plan is in [`tasks/todo.md`](tasks/todo.md); the Phase A acceptance gate is *canonical workload runs end-to-end against Anthropic + LiteLLM through the §2.5 loop, with one third-party MCP server registered and exercised and atomic-application green on a multi-file fixture.*

---

## What's intentionally absent

- **No CI provider beyond GitHub Actions.** The Makefile is portable; other providers (Buildkite, GitLab CI) can wrap `make check` similarly.
- **No agent-loop runtime yet.** The spec describes one; the rig is ready to measure it; Phase A is building it.

---

## License

Apache 2.0. See [`LICENSE`](LICENSE). All Phase A code (`atelier-core`, `atelier-cli`, `atelier-gui`, `atelier-tui`) inherits this license via the workspace `Cargo.toml`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop, conventions, and PR process. Spec questions and design proposals belong in GitHub Discussions; bugs and feature requests use the templates under `.github/ISSUE_TEMPLATE/`. `make check`, `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test -p atelier-core` must all be green before opening a PR — CI runs the same set.
