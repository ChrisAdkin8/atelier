# Plan - resolve critical/high design risks

Date: 2026-05-21.

Scope: remediation plan for the **Critical** and **High** risks listed in `tasks/design_risks.md`.

Out of scope for this plan: medium/low risks, feature work, coverage tooling, and broad style cleanup unless needed to make a risk remediation safe.

## Success criteria

- A single trust-boundary contract exists and is enforced by tests across CLI/TUI/GUI, built-in tools, MCP tools, persistence writes, provider config, and audit logging.
- `Runner::run` is decomposed into named, testable phases without changing external behavior.
- GUI backend responsibilities are split into modules so provider/workspace/chat/memory/skills/event policy no longer live in one file.
- TUI/dispatcher/runtime monoliths are split enough that review-critical code paths are isolated and covered by focused tests.
- Existing gates stay green: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `make check`, and `make audit`.

## Bundle A - Critical: trust-boundary contract and invariant tests

Risk addressed: **Security/trust boundaries are distributed across multiple policy islands**.

### A1 - Document the enforceable trust boundary

- Add a short architecture note under `docs/` or `tasks/` that defines the security invariants:
  - All file writes must pass workspace containment before filesystem mutation.
  - Built-in and MCP tool calls must share dispatcher, approval, hook, audit, ledger, and verification semantics.
  - Network egress must be explicit, audited, and denied by default for subprocesses.
  - Provider base URLs that can receive credentials must be allowlisted or explicitly approved.
  - Persistence/session/compaction writes must not follow repo-controlled symlinks outside the workspace.
- Link the note from `tasks/design_risks.md`.

### A2 - Add cross-surface trust-boundary tests

- Add focused tests proving CLI/TUI/GUI paths route through the same policy-sensitive surfaces where practical:
  - Provider base-url rejection/approval semantics for GUI default, GUI swap, GUI executor, and CLI config resolution.
  - Built-in vs MCP tool registration produces equivalent dispatcher/audit/ledger behavior for side-effecting calls.
  - Persistence and compaction paths reject symlinked `.atelier` escape attempts.
  - Non-interactive concurrent-edit resolution still uses the same read-set/watcher policy.
- Prefer existing test crates/modules; do not introduce a new test framework.

### A3 - Centralize trust-boundary helpers

- Ensure path containment, egress approval, provider allowlisting, and persistence directory creation use shared helpers rather than ad hoc local checks.
- Where a helper cannot be shared yet, add a named test that locks the local behavior to the same invariant.

### A4 - Gate

- `cargo test -p atelier-core path_safety persistence staging mcp`
- `cargo test -p atelier-cli --test run_integration`
- `cargo test -p atelier-gui`
- `cargo test -p atelier-tui`
- Full standing gates from success criteria.

## Bundle B - High: decompose `Runner`

Risk addressed: **`Runner` is becoming the integration kernel**.

### B1 - Map current responsibilities

- Create an internal module map for `crates/atelier-cli/src/runner.rs`:
  - adapter/model-call preparation
  - protocol parsing/conformance
  - tool dispatch
  - verification
  - persistence/recovery/resume
  - sub-agent coordination
  - compaction/context-overflow handling
  - routing/executor-adapter selection
- Identify behavior seams that already have tests and seams that need tests before extraction.

### B2 - Extract phase modules without behavior changes

Suggested target shape:

- `runner/model_call.rs` - message projection, strategy selection, adapter invocation.
- `runner/tool_phase.rs` - dispatch requests, collect outcomes, feed tool results back.
- `runner/verify_phase.rs` - §7 verification, DoD, LSP tiering.
- `runner/persistence_phase.rs` - session save/resume/recovery-log handling.
- `runner/subagent_phase.rs` - drain/wait/cancel/merge child agents.
- `runner/compaction_phase.rs` - context-overflow compaction/retry.

Keep `Runner::run` as a high-level state machine that calls named phase functions.

### B3 - Preserve public API

- Do not change `atelier_cli::{Runner, ProviderChoice, EventSink, RunReport}` signatures unless a later plan explicitly approves it.
- Keep GUI/TUI driver compatibility during extraction.

### B4 - Gate

- `cargo test -p atelier-cli --test run_integration`
- `cargo test -p atelier-cli`
- `cargo test -p atelier-gui`
- `cargo test -p atelier-tui`
- Full standing gates.

## Bundle C - High: split GUI backend orchestration

Risk addressed: **GUI backend is becoming a second orchestration layer**.

### C1 - Introduce backend modules

Split `crates/atelier-gui/src/lib.rs` into responsibility modules:

- `state.rs` - `SessionState`, handles, shared state types.
- `provider.rs` - default/swap/executor adapter resolution, allowlist checks, preflight.
- `workspace.rs` - workspace selection, canonicalization, `gui.toml` load/save.
- `chat.rs` - `start_chat_run`, cancellation, memory-prefix handling.
- `agent.rs` - Runner-backed `start_agent_run`.
- `memory_commands.rs` - add/delete/promote/auto-draft memory commands.
- `skills_commands.rs` - list/invoke skill commands.
- `events.rs` - `bridge_event` and event projection tests.

### C2 - Remove duplicated policy from command handlers

- Command handlers should validate IPC shape and delegate policy-sensitive decisions to shared helpers/modules.
- Provider allowlisting and workspace containment must not be reimplemented inside individual commands.

### C3 - Stabilize event projection

- Convert `bridge_event` tests toward table-driven cases or snapshot-style assertions for all externally visible event variants.
- Keep JSON shape stable unless paired with UI state updates.

### C4 - Gate

- `cargo test -p atelier-gui`
- `npm --prefix crates/atelier-gui/ui run check`
- `npm --prefix crates/atelier-gui/ui run build`
- Full standing gates.

## Bundle D - High: split large runtime/UI monoliths

Risk addressed: **Large monolithic runtime/UI files concentrate too much behavior**.

### D1 - TUI module split

Target modules under `crates/atelier-tui/src/`:

- `state.rs` - `AppState`, pane state, sub-agent/slash/LSP state.
- `events.rs` - `apply`, `project_event`, event log projection.
- `input.rs` - `handle_key`, input modes, key outcome tests.
- `render/` - pane renderers split by pane.
- `run_loop.rs` - terminal lifecycle and async select loop.

Keep the public crate surface unchanged while moving code.

### D2 - Dispatcher module split

Target modules under `crates/atelier-core/src/dispatcher/`:

- `registry.rs` - `ToolRegistry`, registration errors.
- `hooks.rs` - hook phase orchestration.
- `dispatch.rs` - dispatch lifecycle.
- `approval.rs` - approval/concurrent-edit policy.
- `events.rs` - event construction helpers.

Keep `atelier_core::dispatcher::*` re-exports stable during migration.

### D3 - Size thresholds

- Add a soft documented threshold for review attention:
  - production module > 2,000 nonblank LOC
  - production function > 250 nonblank LOC
  - security-sensitive function > 150 nonblank LOC
- Do not fail CI immediately; first publish metrics and create follow-up tasks.

### D4 - Gate

- `cargo test -p atelier-tui`
- `cargo test -p atelier-core dispatcher`
- Full standing gates.

## Recommended order

1. **Bundle A** first: it reduces the risk of behavior drift while refactors happen.
2. **Bundle B** second: `Runner` is the highest-blast-radius runtime file.
3. **Bundle C** third: GUI policy drift is easier to avoid once runner/core boundaries are clearer.
4. **Bundle D** fourth: split large files after invariant and phase tests protect behavior.

## Implementation status

Updated 2026-05-21:

- **Bundle A:** implemented initial shared trust-boundary contract in `docs/trust-boundary.md`, added `atelier_core::trust_boundary`, wired GUI provider checks and CLI profile credential-egress checks through the shared predicates, and added focused regression tests.
- **Bundle B:** started. Extracted the §14 concurrent-edit resolver phase from `runner.rs` into `runner/concurrent_edit.rs` with isolated tests. Remaining Runner phases still need extraction.
- **Bundle C:** started. Extracted GUI provider allowlist / effective URL / preflight helpers into `crates/atelier-gui/src/provider.rs`. Remaining GUI backend command modules still need extraction.
- **Bundle D:** not yet implemented beyond documenting thresholds in this plan and `CODE_QUALITY_METRICS.md`.

## Non-goals

- Do not change user-visible behavior as part of structural extraction.
- Do not rename public CLI flags or Tauri command names.
- Do not remove compatibility re-exports until downstream call sites are migrated and a separate deprecation plan exists.
- Do not combine broad refactors with new feature work.
