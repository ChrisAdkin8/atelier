# Atelier — Status

[← back to README](README.md)

The harness is **end-to-end runnable** for the Phase A/B/C scope, against Mock, Anthropic, and any OpenAI-compatible server (LM Studio, llama-server, vLLM, sglang, Ollama, OpenAI itself). The Tauri GUI is currently a chat-REPL workspace with context/memory/plan/sub-agent surfaces; the `ratatui` TUI remains the live agent workspace with diff and file-level approval controls. The spec, schemas, canonical workload, and self-testing rig are fully wired and verify the harness as it grows. This file tracks **what has landed**, **what is in flight**, and **what gates each phase**. Ordered build plan: [`tasks/todo.md`](tasks/todo.md); version-by-version trail: [`CHANGELOG.md`](CHANGELOG.md) (latest **v60.75**).

---

## Current state

The Phase A foundation, Phase B protocol/verification subset, and Phase C workspace surface are all in. Concretely, under [`crates/atelier-core/src/`](crates/atelier-core/src/) and the three sibling crates:

**Core runtime (`atelier-core`)**
- §2.5 per-session actor — tokio runtime, mpsc inbox, broadcast event bus, bounded tool semaphore, drop-on-cancel.
- §2.5 state machine + tool-error taxonomy + transition table.
- §3 atomic diff staging — tempfile + tree-sitter JSON pre-commit + SHA-256 conflict check + incremental `Hunks` stream over the bus.
- §3 file-level accept/reject (v46 contract) — `ApprovalPolicy::{AutoApproveAll, AwaitApproval}`, `StagedBatch::commit_selected`, `StagingPendingApproval` / `CommitDecision` events.
- §11 sandbox profile generators — macOS `sandbox-exec` `.sb` + Linux `bwrap` argv; default-deny; `/etc` and `/usr/local` writes rejected at policy-build time.
- §14 on-disk session + global registry + recovery-log scaffold; schema-valid `session.json` manifest plus `conversation.jsonl` / `ledger.jsonl` sidecars and `resume_index.json` cursor; atomic save with directory fsync; 0700 session dirs on Unix.
- §15 hook manifest loader + first-use approval; `ShellHookExecutor` runs hooks via `sandboxed_argv` + `subprocess::run`; `time_budget_ms` warns past but never blocks.
- §15 dispatcher + `ToolRegistry` + eight built-in tools: `read_file`, `list_dir`, `grep`, `write_file`, `edit_file`, `ast_grep`, `shell`, `spawn_subagent` — all routed through the same dispatcher with hook + ledger + event-bus uniformity.
- §2 typed envelope + three emission strategies (`native_tool` / `json_sentinel` / `regex_prose`) + downshifting conformance tracker (100-call ring buffer).
- §7 did-it-do-what-it-said diff + per-repo DoD config loader.
- §5 typed `ContextManager` + `MemoryStore` + `PlanCanvas`; `CacheBustEvent` bridges into the §1 ledger.
- §1 BYOM adapter trait + capability matrix + bounded conformance ring. **Three providers in:** `MockAdapter` (always), `AnthropicAdapter` (Messages API, native tool use, SSE streaming, full error taxonomy), `OpenAiCompatAdapter` (v50 — works against any `POST /v1/chat/completions` server: LM Studio, llama-server, vLLM, sglang, Ollama compat layer, OpenAI itself).
- §1 typed cost ledger — `LedgerEntry::{ModelCall, ToolCall, CacheBust}` with per-kind required fields enforced at compile time.
- §1 probe-on-first-use (v51) — `crates/atelier-core/src/adapter/model_profile.rs`. Caches per-model strategy observations to `~/.atelier/model_profiles/<hash>.json`; `Event::ModelProfileLoaded` surfaces to UIs; `ProfileStore::load_or_probe` is the entry point.
- **`.atelier/providers.toml` loader (v53)** — `crates/atelier-core/src/config.rs`. Multi-profile TOML at `<repo>/.atelier/providers.toml` (project) then `~/.atelier/providers.toml` (user); `default = "<name>"` + `[providers.<name>]` tables; `--profile <NAME>` switches between them; CLI flags still win field-by-field. GUI + TUI footers render the active model id + §2 strategy + probe outcome in the bottom-right.
- **§5 Context panel (v53)** — `crates/atelier-core/src/context.rs::ContextItemSummary` + `ContextManager::summarise()` + `Event::ContextItems`; rendered by both UIs (Svelte `ContextPane.svelte`, TUI `render_context_pane`) as per-row token counts + provenance badges. Closes the stated §5 mechanical gate ("API assertions for token counts and why-here; cache-bust ledger entry on eviction").
- **§5 Memory panel (v54)** — `crates/atelier-core/src/memory.rs::MemoryCardSummary` + `MemoryStore::summarise()` + `Event::MemoryCards`; rendered by both UIs (Svelte `MemoryPane.svelte`, TUI `render_memory_pane`) above their respective Plan panes. Empty until a card source wires in; event surface in place so future cards-from-tool / cards-from-replay are purely additive.

**CLI + drivers**
- `atelier init` — bootstraps `.atelier/{sessions,tools,hooks}/` + seeded `ATELIER.md` + `.gitignore` append. Idempotent.
- `atelier run` — drives the full §2.5 loop: load hooks + DoD, build sandbox + dispatcher + ledger, spawn session, resolve `ModelProfile`, broadcast events, run turns until `claimed_done` or `--max-turns`, persist session under `.atelier/sessions/<uuid>/`. Flags: `--provider {mock,anthropic,openai-compat}`, `--model`, `--base-url`, `--workspace`, `--max-turns`, `--prompt-file`, `--no-probe`, `--force-probe`.
- `atelier-gui` (Tauri 2.x + Svelte 5) — chat-REPL workspace (Header / ConversationPane / ContextPane / MemoryPane / PlanPane / SubagentPane / MetersPane / Composer), native folder picker, provider swap, memory auto-drafting/promotion, skills autocomplete, and Runner-backed agent flows where needed. Concurrent-run guard via `Arc<AtomicBool>`; 64 KB prompt cap; per-run UUID workspaces with drop-guard cleanup.
- `atelier-tui` (ratatui + crossterm) — conversation pane, textual diff, plan/context/memory/sub-agent panes, slash-skill completion, LSP install prompt, cost + context meters, scrubber keys `[`/`]`/`g`. Driver mode via `cargo run -p atelier-tui -- "<prompt>"`; `y` / `n` route through `SessionDispatcher::submit_approval`.

### Rig counts (as of v51)

| | Count | Where |
|---|---|---|
| Specification sections | — | [`coding-harness-spec.md`](coding-harness-spec.md) |
| JSON schemas (Draft 2020-12) | 21 | [`schemas/`](schemas/) |
| Canonical workload fixtures | 11 (10 Py + 1 TS) | [`tests/workload/canonical/`](tests/workload/canonical/) |
| Validated artifacts | 52 | [`tests/`](tests/) |
| Rig self-tests | 112 | [`tests/`](tests/) |
| `atelier-core` Rust unit tests | 506 | [`crates/atelier-core/src/`](crates/atelier-core/src/) |
| `atelier-cli` integration tests | 19 | [`crates/atelier-cli/tests/`](crates/atelier-cli/tests/) |
| `atelier-gui` unit tests | 15 | [`crates/atelier-gui/src/lib.rs`](crates/atelier-gui/src/lib.rs) |
| `atelier-tui` unit tests | 65 | [`crates/atelier-tui/src/lib.rs`](crates/atelier-tui/src/lib.rs) |
| Phased build plan | see file | [`tasks/todo.md`](tasks/todo.md) |

`make check` runs schema meta-validation → artifact validation → rig self-tests → workload dry-run. All currently green; CI runs the same pipeline on every push/PR alongside `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.

For the invariants the rig *enforces* (no-op-harness exploits, cross-schema `$ref` resolution, schema versioning, BYOM baselines, …) see [`tests/README.md`](tests/README.md). For active work and what gates each piece, see [`tasks/todo.md`](tasks/todo.md).

---

## Phase A — piece-by-piece tracker

| Piece | Where it lands | State |
|---|---|---|
| §2.5 state machine + transition table | [`crates/atelier-core/src/state.rs`](crates/atelier-core/src/state.rs) | **done** (enum + legal-transition table + hook traits + unit tests) |
| §2.5 tool error taxonomy | [`crates/atelier-core/src/error.rs`](crates/atelier-core/src/error.rs) | **done** (incl. recovery routing tests) |
| §2.5 session actor (tokio + mpsc + broadcast + semaphore + cancellation token) | [`crates/atelier-core/src/session.rs`](crates/atelier-core/src/session.rs) | **done** (runtime mechanics; drives the state machine; broadcasts events to UI subscribers) |
| §3 atomic diff staging | [`crates/atelier-core/src/staging.rs`](crates/atelier-core/src/staging.rs) | **done** (tempfile + tree-sitter JSON pre-commit + SHA-256 conflict check; parent-dir fsync; all-or-nothing per turn) |
| §3 file-level accept/reject | `staging.rs` + `dispatcher.rs` + `session.rs` | **done** (v46 contract; `commit_selected`; bus events; round-trips via `submit_approval`) |
| §11 sandbox profile generators (macOS `.sb` + Linux `bwrap`) | [`crates/atelier-core/src/sandbox.rs`](crates/atelier-core/src/sandbox.rs) | **done** (default deny; `/etc` and `/usr/local` writes rejected at policy-build time) |
| §14 on-disk session + recovery log + global registry | [`crates/atelier-core/src/persistence.rs`](crates/atelier-core/src/persistence.rs) | **done** (typed `OnDiskSession`; schema-valid manifest + JSONL sidecars; resume cursor; compaction; atomic save; version-skew rejected on load; 0700 dirs on Unix) |
| §15 hook manifest loader + first-use approval | [`crates/atelier-core/src/hooks.rs`](crates/atelier-core/src/hooks.rs) | **done** (per-repo-overrides-global; `_approvals.json` store) |
| §15 dispatcher + 8 built-in tools + `ShellHookExecutor` | [`crates/atelier-core/src/dispatcher.rs`](crates/atelier-core/src/dispatcher.rs) + [`crates/atelier-core/src/tools/`](crates/atelier-core/src/tools/) | **done** (`read_file`, `list_dir`, `grep`, `write_file`, `edit_file`, `ast_grep`, `shell`, `spawn_subagent`; writes route through staging; sandboxed subprocess) |
| §2 envelope types | [`crates/atelier-core/src/protocol.rs`](crates/atelier-core/src/protocol.rs) | **done** |
| §2 three emission strategies | [`crates/atelier-core/src/protocol_strategy.rs`](crates/atelier-core/src/protocol_strategy.rs) | **done** (`native_tool` / `json_sentinel` / `regex_prose` with `downshift()` chain) |
| §2 conformance tracker | [`crates/atelier-core/src/protocol_conformance.rs`](crates/atelier-core/src/protocol_conformance.rs) | **done** (100-call ring buffer; `rate()` returns `Option<f32>`) |
| §7 did-it-do-what-it-said diff | [`crates/atelier-core/src/verify.rs`](crates/atelier-core/src/verify.rs) | **done** |
| §7 per-repo DoD config | [`crates/atelier-core/src/dod.rs`](crates/atelier-core/src/dod.rs) | **done** (executor still stubbed — warns when DoD configured but checks not run) |
| §5 typed context / memory / plan | [`crates/atelier-core/src/{context,memory,plan}.rs`](crates/atelier-core/src/) | **done** |
| §3 incremental diff stream | [`crates/atelier-core/src/diff.rs`](crates/atelier-core/src/diff.rs) + `session::Event::EditStaged` | **done** (`Hunks::{Same, Lines, Binary, Created, Deleted}` via `similar`) |
| §1 BYOM adapter trait + `MockAdapter` | [`crates/atelier-core/src/adapter/mod.rs`](crates/atelier-core/src/adapter/mod.rs) | **done** |
| §1 Anthropic adapter | [`crates/atelier-core/src/adapter/anthropic.rs`](crates/atelier-core/src/adapter/anthropic.rs) | **done** (chat + SSE stream; native tool use; full error taxonomy; 18 wiremock tests) |
| §1 OpenAI-compatible adapter | [`crates/atelier-core/src/adapter/openai_compat.rs`](crates/atelier-core/src/adapter/openai_compat.rs) | **done** v50 (LM Studio / llama-server / vLLM / Ollama / OpenAI; 19 wiremock tests) |
| §1 probe-on-first-use cache | [`crates/atelier-core/src/adapter/model_profile.rs`](crates/atelier-core/src/adapter/model_profile.rs) | **done** v51 (`ModelProfile` + `ProfileStore::load_or_probe`; CLI `--no-probe` / `--force-probe`; bus `Event::ModelProfileLoaded`) |
| §1 typed cost ledger | [`crates/atelier-core/src/ledger.rs`](crates/atelier-core/src/ledger.rs) | **done** |
| `atelier run` CLI subcommand | [`crates/atelier-cli/src/{main,runner,lib}.rs`](crates/atelier-cli/src/) | **done** (hybrid lib+bin; `Runner` linked by GUI/TUI for driver mode) |
| Tauri GUI workspace | [`crates/atelier-gui/src/lib.rs`](crates/atelier-gui/src/lib.rs) + [`crates/atelier-gui/ui/src/`](crates/atelier-gui/ui/src/) | **done** (chat-REPL workspace, provider swap, context/memory/plan/sub-agent panes, skills autocomplete, native workspace picker) |
| ratatui TUI driver mode | [`crates/atelier-tui/src/lib.rs`](crates/atelier-tui/src/lib.rs) | **done** v48 (driver + viewer modes; `y` / `n` approval keys) |
| §15 MCP client (`rmcp`) | `crates/atelier-core/src/mcp/` | **done** — stdio + HTTP/SSE launchers, dispatcher registration, egress audit, MCP resources as context items |
| §14 file-watcher (fsevents/inotify) + concurrent-edit modal | [`crates/atelier-core/src/file_watcher.rs`](crates/atelier-core/src/file_watcher.rs) + UI drivers | **done** — dispatcher read-set tracking, GUI/TUI resolution surfaces, non-interactive auto-reload |
| §1 LiteLLM adapter | `crates/atelier-core/src/adapter/litellm.rs` | **planned** — overlaps significantly with the OpenAI-compat adapter; may not be needed if the `openai-compat` surface covers the LiteLLM-shaped gateway |
| Phase A mechanical gate | various | **partial** — canonical workload runs against Mock + Anthropic + OpenAI-compat through the §2.5 loop with atomic-application green on multi-file rename. **Outstanding:** real third-party MCP server registered + exercised (gated on MCP client landing). |

---

## Phase A acceptance gate

*Canonical workload runs end-to-end against Anthropic + OpenAI-compat (and a LiteLLM-shaped gateway when added) through the §2.5 loop, with one third-party MCP server registered and exercised and atomic-application green on a multi-file fixture.*

Status: closed. The model-side path is in (three providers, scripted multi-file rename through the loop, atomic application verified), and the third-party MCP-server side is covered by the `@modelcontextprotocol/server-filesystem` gate.

The full ordered build plan is in [`tasks/todo.md`](tasks/todo.md).
