# Atelier — Status

[← back to README](README.md)

The harness is mid-build. The spec, schemas, canonical workload, and self-testing rig are fully wired; the agent loop is not yet end-to-end runnable. This file tracks **what has landed**, **what is in flight**, and **what gates each phase**. The ordered build plan lives in [`tasks/todo.md`](tasks/todo.md); spec/rig change history is in [`CHANGELOG.md`](CHANGELOG.md).

---

## Current state

**The agent loop is not yet end-to-end runnable**, but the Phase A foundation and the Phase B protocol/verification subset have landed under [`crates/atelier-core/src/`](crates/atelier-core/src/):

- §2.5 per-session actor — tokio runtime, mpsc inbox, broadcast event bus, bounded tool semaphore, drop-on-cancel.
- §3 atomic diff staging — tempfile + tree-sitter JSON pre-commit + SHA-256 conflict check.
- §11 sandbox profile generators — macOS `sandbox-exec` `.sb` + Linux `bwrap` argv.
- §14 on-disk session + global registry + recovery-log scaffold.
- §15 hook manifest loader with first-use approval store.
- §2 typed envelope + three emission strategies (`native_tool` / `json_sentinel` / `regex_prose`) + downshifting conformance tracker.
- §7 did-it-do-what-it-said diff (`Discrepancy::{Claimed, Unclaimed, KindMismatch, DuplicateClaim}`) and per-repo DoD config (`schemas/config/dod.v1.json` + loader).
- §2.5 state machine, §2.5 tool-error taxonomy, and the `atelier init` bootstrap.

**Still in flight under Phase A** (see [`tasks/todo.md`](tasks/todo.md)): the BYOM adapter trait + Anthropic/LiteLLM adapters, the MCP client (gated on the `rmcp` spike), the tool dispatcher, the file-watcher integration, the `atelier run` CLI, the Phase B real-model conformance gate, and the Tier-1 hallucination detector (gated on Q3 LSP UX).

### Rig counts

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

For the invariants the rig *enforces* (no-op-harness exploits, cross-schema `$ref` resolution, schema versioning, BYOM baselines, …) see [`tests/README.md`](tests/README.md). For what is blocking Phase A coding, see [`tasks/todo.md`](tasks/todo.md).

---

## Phase A — piece-by-piece tracker

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

---

## Phase A acceptance gate

*Canonical workload runs end-to-end against Anthropic + LiteLLM through the §2.5 loop, with one third-party MCP server registered and exercised and atomic-application green on a multi-file fixture.*

The full ordered build plan is in [`tasks/todo.md`](tasks/todo.md).
