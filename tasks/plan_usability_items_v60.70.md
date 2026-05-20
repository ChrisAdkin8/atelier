# Plan — usability items v60.70+

Date: 2026-05-20. Source: session-ranked usability backlog. Four independent items, ordered by dependency; items 2 and 4 are parallel-capable. Item numbers reference the session ranking for traceability.

Items are lettered **U01–U15** for commit-message traceability.

---

## Standing gates (all bundles)

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p atelier-core` (and `-p atelier-cli` / `-p atelier-gui` / `-p atelier-tui` where touched)
- `make check` if schemas / fixtures change

Each item below states its own targeted verification on top of these.

---

## Item 6 — Commit or revert `config.rs` routing feature (U01)

**What:** `crates/atelier-core/src/config.rs` has uncommitted changes adding `RoutingSection` struct, `routing: Option<RoutingSection>` to `ProvidersConfig`, and `cache_prompt: Option<bool>` to `ProviderProfile`. Half-written, unstaged, no tests.

**Why first:** unclean working tree blocks reliable `cargo clippy` and `cargo test` passes; also prevents safe branching for the subsequent items.

**Decision required before starting:** does the routing feature belong in the immediate next bundle, or should it be reverted? If the feature is not needed imminently, revert is the right call — it can be re-introduced properly. If it is needed, commit it with tests before touching any other file.

### U01a — If committing

**Scope:** `crates/atelier-core/src/config.rs` only.

**Success criteria:**
1. `RoutingSection` + `ProviderProfile::cache_prompt` are fully typed and `serde`-round-trippable with `deny_unknown_fields`.
2. `ProvidersConfig` round-trips a config containing a `[routing]` section through `toml::from_str` + `toml::to_string` without loss.
3. New unit test `config_routing_section_roundtrips` in `crates/atelier-core/src/config.rs` asserts the above.
4. `ProvidersConfig::load` still works when `[routing]` is absent (schema-optional field).
5. All standing gates green.

**Verify:** `cargo test -p atelier-core config_routing` passes; `cargo clippy -p atelier-core -- -D warnings` clean.

### U01b — If reverting

```sh
git checkout -- crates/atelier-core/src/config.rs
```

Confirm with `git diff --stat` that the tree is clean before proceeding.

---

## Item 4 — Node.js 20 → 24 action version bumps (U02)

**Where:** all files under `.github/workflows/`.

**Why:** GitHub Actions will force Node.js 24 on June 2, 2026 for all `actions/*` that currently ship Node.js 20 runtimes. After that date the old SHA pins emit deprecation warnings, and eventually errors.

**Affected actions** (verify with `grep -rn 'actions/checkout\|actions/setup-python\|actions/setup-node\|actions/upload-artifact\|actions/download-artifact\|actions/cache' .github/workflows/`):
- `actions/checkout` — currently `v4` (Node.js 20); bump to a SHA-pinned `v4` commit on the `node20→node24` branch, or to `v5` if released.
- `actions/setup-python` — same.
- `actions/setup-node` — same.
- `actions/upload-artifact` / `actions/download-artifact` / `actions/cache` — check each.

**Constraint:** every `uses:` must stay SHA-pinned per the existing `tests/test_ci.py::test_all_uses_are_sha_pinned` check. Do not switch to bare tag refs.

### U02 — Steps

1. For each action, find the latest commit SHA on the `v5` (or `v4` Node-24) branch of the upstream repo.
2. Update `uses: actions/<name>@<old-sha>` → `uses: actions/<name>@<new-sha> # <tag>` across all workflow files.
3. Run `make check` (the `test_ci.py::test_all_uses_are_sha_pinned` test validates the format).

**Success criteria:**
1. `grep -rn 'node20' .github/workflows/` returns zero matches in `uses:` lines.
2. `tests/test_ci.py::test_all_uses_are_sha_pinned` still passes.
3. All standing gates green.

**Verify:** `python -m pytest tests/test_ci.py -v` passes.

---

## Item 5 — TUI skills completion run-loop wiring (U03–U06)

**Context:** `SlashState` state machine lives in `crates/atelier-tui/src/skills_completion.rs` (landed v60.50–v60.54). The GUI has full Tab-complete + ↑/↓ navigation wired in `Composer.svelte`. The TUI run loop does not invoke `SlashState` at all — `/` prefix input falls through to the prompt verbatim.

**Spec reference:** §15 lines 765–810 (skills surface); the TUI is the heads-down editing UI so slash-command completion there is higher-leverage than GUI-only.

**Depends on:** U01 (clean tree) — no other blocking dependency.

### U03 — Audit current TUI input path

Read `crates/atelier-tui/src/app.rs` (or equivalent run-loop file) and locate:
- Where `KeyEvent` characters are appended to the Composer input buffer.
- Where `Enter` submits the buffer as a prompt.
- The current import / usage of `skills_completion::SlashState` (likely zero — confirm with `grep -rn 'SlashState\|slash_state' crates/atelier-tui/`).

**Deliverable:** a comment or note identifying the three insertion points (key-append, enter-submit, render) before any code is written.

### U04 — Wire `SlashState` into the TUI input loop

**Where:** `crates/atelier-tui/src/app.rs` (or whichever file owns the input state machine).

**Contract to satisfy (mirror GUI behaviour):**
- Typing `/` into an otherwise-empty Composer line initialises `SlashState::Active`.
- Each subsequent character narrows the completion list via `SlashState::update(&input)`.
- `↑` / `↓` cycle through matches (`SlashState::select_prev` / `::select_next`).
- `Tab` or `Enter` on a highlighted item replaces the buffer with the full skill prompt text.
- `Esc` clears `SlashState` and leaves the buffer as typed.
- Any non-`/`-prefixed input leaves `SlashState::Inactive` (no regression on normal prompts).

**Success criteria:**
1. `grep -rn 'SlashState' crates/atelier-tui/src/` shows at least one `use` and one call-site.
2. Existing TUI unit tests in `crates/atelier-tui/` still pass.
3. New unit test `tui_slash_completion_tab_selects_skill` in `crates/atelier-tui/src/` (or `tests/`) covers the Tab-complete path using `SlashState` directly (no PTY needed).

### U05 — Render completion list in TUI

**Where:** `crates/atelier-tui/src/` render module (wherever the Composer / help footer is rendered).

When `SlashState::Active` with ≥1 match:
- Render a small popup (or inline footer row) listing up to 5 matches, with the selected item highlighted via `Style::reversed()` or equivalent.
- When `SlashState::Inactive` or no matches: no popup (no regression on the existing footer).

**Success criteria:**
1. New unit test `tui_slash_completion_renders_popup_when_active` asserts that the render output contains the skill name string when `SlashState` has a selected item.
2. `cargo test -p atelier-tui` passes.

### U06 — Integration smoke test

Add a `#[test]` (no PTY) in `crates/atelier-tui/tests/` or inline that:
1. Feeds `KeyCode::Char('/')`, `KeyCode::Char('r')`, `KeyCode::Tab` into the TUI state machine.
2. Asserts the resulting prompt buffer starts with the expanded text of the first `/r*` skill (e.g. `/review`'s `prompt_template`).

**Verify:** `cargo test -p atelier-tui -- skills` passes; all standing gates green.

---

## Item 2 — Phase B §7 LSP live receiver (U07–U12)

**Context:** the data layer (v60.25), pure-function mapper (v60.26), and hallucinating-agent fixture gate (v60.27) are all green. The one remaining piece is an `async-lsp 0.2` receiver that:
1. Spawns `typescript-language-server --stdio` inside the §11 sandbox.
2. Opens each `.ts` file the agent wrote.
3. Collects `textDocument/publishDiagnostics` notifications.
4. Translates them via `crate::lsp::typescript::map_diagnostic(DiagnosticInput)` into `Discrepancy::HallucinatedSymbol`.

The spike harness already exists at `experiments/lsp_spike/`. The decision is **GO** per v60.25's Q3 resolution — `async-lsp 0.2` with prompt-on-first-use install approval.

**Depends on:** U01 (clean tree). Does not depend on U02–U06.

### U07 — `async-lsp` dep + `lsp_types` receiver skeleton

**Where:** `crates/atelier-core/Cargo.toml` + new `crates/atelier-core/src/lsp/receiver.rs`.

**Steps:**
1. Add `async-lsp = "0.2"` and `lsp-types = "0.95"` (or whichever version the spike used — check `experiments/lsp_spike/Cargo.toml`) to `[workspace.dependencies]` and `atelier-core`'s `[dependencies]`.
2. New `crates/atelier-core/src/lsp/receiver.rs` with:
   - `LspSession { client: LspClient, diagnostics: Arc<Mutex<Vec<lsp_types::Diagnostic>>> }` (or equivalent).
   - `LspSession::open_file(path, content)` — sends `textDocument/didOpen` notification.
   - `LspSession::collect_diagnostics(timeout: Duration) -> Vec<DiagnosticInput>` — waits up to `timeout` for `publishDiagnostics`, translates from `lsp_types` → `DiagnosticInput`.
   - `LspSession::shutdown()` — sends `shutdown` + `exit`, awaits clean exit.
3. Feature-gate under `#[cfg(feature = "lsp")]` if the dep is heavy; otherwise unconditional.

**Success criteria:**
1. `cargo build -p atelier-core` compiles with the new dep.
2. No new `cargo clippy` warnings.

### U08 — `LspLauncher` — spawn + handshake inside §11 sandbox

**Where:** new `crates/atelier-core/src/lsp/launcher.rs`.

**Contract:**
- `LspLauncher::spawn(workspace_root, sandbox, approvals) -> Result<LspSession, LspLaunchError>`.
- Checks `approvals.is_approved("typescript-language-server")` before spawning; returns `LspLaunchError::NotApproved` if not.
- Spawns `typescript-language-server --stdio` via `subprocess::sandboxed_argv`.
- Completes `initialize` / `initialized` handshake.
- Returns `LspSession` on success.

**Success criteria:**
1. Unit test `lsp_launcher_returns_not_approved_when_no_approval` (mock — no real process).
2. `#[ignore]`-gated integration test `lsp_launcher_spawns_and_handshakes` passes locally when `typescript-language-server` is on PATH.

### U09 — Wire `LspLauncher` into `Runner::run()` verify phase

**Where:** `crates/atelier-cli/src/runner.rs` — the `Verifying` transition.

**Contract:**
- After `SessionDispatcher::verify_pass` produces the Tier-3 / Tier-2 result, if any committed `.ts` files exist AND `LspApprovals::is_approved("typescript-language-server")`:
  - Spawn `LspSession`, open each `.ts` file, collect diagnostics (timeout = 10s PROVISIONAL), translate → `Discrepancy::HallucinatedSymbol`, merge into `VerificationRun` via existing `verify_pass_with_tier1`.
  - Emit `Event::VerificationPassed { tier: Tier1Lsp, … }` or `Event::VerificationFailed { tier: Tier1Lsp, … }` accordingly.
- If `LspLaunchError::NotApproved`: emit `Event::RequestLspInstall { language: "typescript", … }` and fall through to Tier-2.
- If any other `LspLaunchError`: log warn + fall through to Tier-2 (never block a run on LSP unavailability).

**Success criteria:**
1. Existing `mock_hallucinating_agent_fixture_flagged_within_one_turn_phase_b_seven_gate` still passes (uses `with_tier1_diagnostics_for_test` test seam — unchanged).
2. New `#[ignore]`-gated integration test `runner_lsp_live_detects_hallucinated_symbol` drives a scripted MockAdapter that writes a `.ts` file with a known bad symbol, runs the real LSP receiver, and asserts `Event::VerificationFailed { tier: Tier1Lsp }`.
3. New unit test `runner_lsp_launch_error_falls_through_to_tier2` asserts fall-through behaviour when `LspLaunchError::NotApproved` is returned.

### U10 — `Event::RequestLspInstall` GUI + TUI surface

**Where:** `crates/atelier-gui/src/lib.rs` (bridge) + `crates/atelier-gui/ui/src/lib/state.ts` (reducer) + `crates/atelier-tui/src/app.rs` (apply).

**Contract:** when `Event::RequestLspInstall` arrives on the bus:
- GUI: show a modal (reuse the `McpApprovals` first-use shape from v60.8) with "Install typescript-language-server?" + Approve / Decline buttons. Approve → Tauri command `approve_lsp_server("typescript-language-server")` → `LspApprovals::approve` + persist. Decline → `LspApprovals` unchanged; tier falls to Tier-2 (no further prompt this session).
- TUI: `InputMode::LspInstallConfirm { language, candidates }` — `y` approves, `n` declines; rendered in the help footer.

**Success criteria:**
1. New GUI component test: `bridge_event` maps `RequestLspInstall` to the correct state shape.
2. New TUI unit test: `apply` maps `RequestLspInstall` to `InputMode::LspInstallConfirm`.
3. `cargo test -p atelier-gui && cargo test -p atelier-tui` pass.

### U11 — `LspApprovals` Tauri commands

**Where:** `crates/atelier-gui/src/lib.rs`.

Two new commands mirroring the `McpApprovals` pattern:
- `approve_lsp_server(language: String)` — loads `LspApprovals`, calls `approve`, saves.
- `revoke_lsp_server(language: String)` — loads, calls `revoke`, saves.

**Success criteria:**
1. New unit tests in `crates/atelier-gui/src/lib.rs` (the existing `#[cfg(test)]` block) cover both commands.
2. `cargo test -p atelier-gui` passes.

### U12 — Phase B §7 gate update

Update `todo.md` and `CHANGELOG.md`:
- Mark the `[~] Hallucination detector Tier 1 — TypeScript` item as `[x]` once U07–U11 are verified end-to-end.
- Mark the `[~] LSP shell-out + auto-install prompt` item as `[x]`.
- Add a `v60.7x` CHANGELOG entry.

**Final verify for U07–U12:**
- `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` (U09's `#[ignore]`-gated test not required for green — it only runs locally when `typescript-language-server` is on PATH)
- `make check`

---

## Execution order

```
U01 (config.rs cleanup)
  ├─▶ U02 (Node.js 20→24)          # independent of U03+
  ├─▶ U03–U06 (TUI skills)         # independent of U07+
  └─▶ U07–U12 (LSP live receiver)  # independent of U02, U03
```

U02, U03–U06, and U07–U12 are all unblocked once U01 is done and can be worked in parallel across sessions.
