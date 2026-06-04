# Plan — remediation from the 2026-06-04 deep-scan audit

Date: 2026-06-04. Source: [`tasks/audit-2026-06-04.md`](audit-2026-06-04.md) (deep-scan run from this session; all severity-bearing findings were context-verified before inclusion, per the 2026-06-02 lesson — four subagent findings were rejected during verification and are catalogued in the audit's "rejected" section, not re-raised here).

Phase 1 was fully green (clippy, fmt, gitleaks, npm audit), so unlike the 2026-06-02 plan there is no structural refactor bundle — the work is small, behavioural, and concentrated in one seam: **the CLI driver's failure-reporting path**. Three P1s compound each other there.

**Reviewed 2026-06-04 (same session) — corrections applied inline:** F3's `eprintln!` moved out of `Runner` into main.rs (Runner is linked by the raw-mode TUI; library-side stderr writes would corrupt its screen); F2 gained the missing bus event and a breaking-change callout; F1's verify step corrected (DoD config is not in providers.toml); Q3's exit code corrected to the operational-failure convention (1, not 2) and extended to cover the GUI's parallel fallback; Q4 noted as pure plumbing (the timeout is already a resolver parameter). Facts verified during review: the actor is in `Streaming` at loop exhaustion (`execute_tool_calls` advances back at runner.rs:1243); `RunReport` is constructed only in runner.rs (GUI/TUI/tests are readers only); dispatcher.rs has no tool-error logging, confirming Q1.

Items are numbered for traceability in commit messages / PR descriptions.

## Standing gates (every PR)

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p atelier-cli` (plus other crates where touched)
- `make check` (rig: schemas → artifacts → rig-tests → workload dry-run)

Each item adds a **targeted** verification on top of these.

---

## Bundle 1 — CLI failure-visibility (the three P1s, one PR) ✓ DONE v60.99

**Status 2026-06-04:** all three items implemented and verified. Gates: `cargo fmt --check` clean, `cargo clippy --workspace --all-targets -D warnings` clean, `cargo test -p atelier-cli` (54 unit tests), `run_integration` (118 tests) all pass, `make check` 180/180. One pre-existing test (`run_bails_after_max_turns_without_claimed_done`) had an assertion pinning the old incorrect behaviour (`!= AwaitingUser`); updated to assert the now-correct contract (`== AwaitingUser`). New tests added: `max_turns_exhaustion_reports_awaiting_user_and_stalled_event`, `persist_failure_sets_report_persist_error`.

## Bundle 1 — CLI failure-visibility (the three P1s, one PR)

A run that times out reports success (exit 0); a session that fails to persist reports success; and the warnings that would have disclosed both go into a tracing void because the binary never installs a subscriber. Fix all three together — they share a regression-test surface and individually each fix is a few lines.

### F1 — Install a tracing subscriber in the CLI binary

- File: `crates/atelier-cli/src/main.rs` (top of `main()`).
- Current: `grep -rln tracing_subscriber crates/*/src` shows atelier-gui and atelier-tui install one; atelier-cli does not. Every `tracing::warn!`/`info!` emitted from `runner.rs` during `atelier run` is dropped — session-save failures (F3), the "DoD config loaded but checks not wired" warning (`runner.rs:2820`), probe failures, `pane_visibility.json` write failures.
- Fix: install `tracing_subscriber::fmt()` writing to **stderr** with an `EnvFilter` defaulting to `warn` (respect `RUST_LOG` for override). Stderr, not stdout — `atelier run` output may be machine-consumed. `tracing-subscriber` (with `env-filter`) is already a workspace dep used by gui+tui; cli's Cargo.toml adds one `{ workspace = true }` line.
- **Verify:** run `atelier run` against a deliberately unreachable provider base-url; the probe-failure warn is now visible on stderr (pre-F1 it was dropped). `cargo test -p atelier-cli` green — in particular any test asserting *stdout* content stays green, proving the subscriber doesn't pollute machine-readable output.

### F2 — `final_state = AwaitingUser` on max_turns exhaustion

- File: `crates/atelier-cli/src/runner.rs:2805-2810` (the `for turn in 0..self.max_turns` loop in `run()`).
- Current: when every turn returns `TurnControl::Continue` and the loop exhausts, `state.final_state` is whatever the last turn set — `Streaming` (`runner.rs:2221`). `run_verification_pass` only fires on `Verifying`, so the `RunReport` carries `Streaming` and `exit_code_for_final_state` (`lib.rs:89`) maps it to **0**. CI treats a timed-out run as success. The doc comment on `run()` (`runner.rs:2366`) already promises "`max_turns` reached (timeout; `final_state = AwaitingUser`)" — the contract exists, the implementation drifted.
- Fix: track whether the loop exited via `Break` or exhaustion (e.g. a `completed` flag, or inspect `state.turns == self.max_turns && state.final_state` not terminal); on exhaustion, `advance(session_handle, State::Streaming, State::AwaitingUser)` — verified during review: the actor **is** in `Streaming` at exhaustion because `execute_tool_calls` advances `ToolExecuting → Streaming` at `runner.rs:1243`, the same from-state the stall path uses at `runner.rs:2356`. Then set `state.final_state = State::AwaitingUser`. (If a future `Continue` path leaves the actor elsewhere, `advance` errors and the regression test below catches it.)
- Also emit a bus event on exhaustion: the stall path emits `Event::AgentStalled` (`session.rs:511`) but the exhaustion path emits nothing, so a GUI/TUI user watches the run simply stop. Reuse `AgentStalled { turn, reason: "max_turns (<N>) exhausted without claimed_done" }` — reusing the existing variant avoids a new wire enum and the L-D-5 agreement-test burden.
- **Breaking-change callout:** exit code on timeout changes 0 → 6. Any user script that (incorrectly but practically) relied on 0 breaks. Flag prominently in the CHANGELOG entry; this is the documented contract finally being honoured, not a new contract.
- **Verify (regression test):** Mock-adapter run with `max_turns = 1` and a scripted turn that never claims done asserts `report.final_state == State::AwaitingUser` **and** `exit_code_for_final_state(report.final_state) == 6` (unit-level — binary-level assert_cmd remains out of scope per Bundle 2 of the 06-02 plan). Assert the `AgentStalled` event was broadcast. Existing test `max_turns_one_executes_exactly_one_turn` (added v60.90) is adjacent — extend or sibling it.

### F3 — Surface session-persist failure

- File: `crates/atelier-cli/src/runner.rs:1539` (`snapshot.save_split_to` in `build_and_persist_session`).
- Current: failure is `tracing::warn!`-only (invisible until F1) and the run exits 0. The "best-effort" rationale comment predates v61's conversation write-back — `--resume` now depends on this write succeeding; a user whose disk filled gets a silent non-resumable session.
- Fix (graded, layered by driver): (a) keep the `tracing::warn!` in Runner (visible once F1 lands); (b) add a `persist_error: Option<String>` field to `RunReport` — `build_and_persist_session` currently returns `()` and its result is discarded at the call site (~`runner.rs:2858`), so the method signature changes to return the outcome; (c) **main.rs** (not Runner) reads the field and `eprintln!`s.
  **Why the eprintln must NOT live in Runner:** Runner is linked as a library by the TUI, which runs in raw mode + alternate screen — a library-side stderr write during a TUI-driven run corrupts the rendered screen. Each driver surfaces the field its own way: main.rs prints to stderr, the TUI can render a status line, the GUI can toast. Do **not** fail the run — the in-memory result is still valid; the failure is about *durability*, and the graded surface keeps that distinction.
- Compile impact (verified during review): `RunReport` is constructed only in `runner.rs` — GUI/TUI and the cli integration tests are readers only, so the field addition breaks no external construction site.
- **Verify (regression test):** unit test pointing `session_dir` at a read-only directory asserts `report.persist_error.is_some()`; a main.rs-level check (or careful review) confirms the stderr message. `cargo build -p atelier-gui -p atelier-tui` confirms the readers compile unchanged.

> Sequencing within Bundle 1: F1 first (it makes F3's existing warn visible and gives F2's test useful diagnostics), then F2, then F3. One PR, three commits, traceable as F1/F2/F3.

---

## Dependency debt (P1, tracked — no code action this cycle)

### D1 — RUSTSEC-2026-0009 (`time` 0.3.41, DoS via stack exhaustion)

- Transitive dependency; documented audit-ignore debt since v60.85 while the workspace is pinned to Rust 1.85 (`rust-toolchain.toml`).
- Action: re-evaluate at the next toolchain-pin review. If the pin moves, `cargo update -p time` and drop this item. Until then it stays on the audit-ignore list — re-document the acceptance in the next audit rather than letting it look fresh.
- **Verify:** `cargo audit` after any toolchain change; this item disappears from the report.

---

## Bundle 2 — P2 quality items (one PR, land after Bundle 1) ✓ DONE v60.100

**Status 2026-06-04:** all six code items implemented and verified. Gates: `cargo fmt --check` clean, `cargo clippy --workspace --all-targets -D warnings` clean, `cargo test -p atelier-core -p atelier-cli` (77+57 unit + 121 integration tests) all pass, `svelte-check` 0 errors, `make check` 180/180. Q6 and Q8 intentionally no code action. New tests: `bounded_resume_prefix_prose_only_passes_through_without_synthetic_note`, `bounded_resume_prefix_synthetic_note_counts_dropped_rows`, `runner_section_pause_timeout_secs_parses`, `runner_section_pause_timeout_secs_absent_is_none`, `subagent_type_registry_loads_with_missing_home`.

## Bundle 2 — P2 quality items (one PR, land after Bundle 1)

Q1 and Q5 touch the same files as Bundle 1 — land after it to keep diffs small. None of these is a bug today.

### Q1 — Preserve tool-error source chains

- File: `crates/atelier-cli/src/runner.rs:1222` (`Err(e) => serde_json::json!({ "error": e.to_string() })` in the tool-result fold).
- Current: tool errors are `.to_string()`-ed into the JSON tool-result payload, and review confirmed this is the **only** host-side touchpoint — dispatcher.rs has no tool-error logging of its own, so the typed chain is lost entirely. (The *payload* string is fine — the model needs text — the loss is on the host-side diagnostic path.)
- Fix: log the typed error (with chain, via `tracing::warn!(error = ?e)`) before stringifying for the payload. **Logging-only change** — this is a different layer from the v60.91 `AdapterChain` work; do not add a `RunError` variant (tool errors are recoverable within the turn, not run-fatal). Depends on F1 to be visible.
- **Verify:** unit test asserting a failed tool call logs the chain (or, minimally, careful review — this is a logging-only change).

### Q2 — Document (or summarise) resume's tool-roundtrip drop

- File: `crates/atelier-cli/src/runner.rs:3273` (`bounded_resume_prefix`).
- Current: when history isn't prose-only, all tool-call/tool-result rows are dropped on `--resume`. Intentional and commented, but invisible to the user — a resumed session silently loses why the previous run stalled.
- Fix (two options, pick at implementation time):
  - (a) cheap: document in `--resume`'s help text + README resume section.
  - (b) better: inject one synthetic system line ("resume dropped N tool roundtrips from the prior session") so the *model* also knows context is missing.
  - Recommend (b) — it costs one string and fixes the model-side blindness, not just the human-side.
- **Verify:** unit test on `bounded_resume_prefix` (or its caller) asserting the synthetic line appears when rows were dropped and not when prose-only passes through.

### Q3 — Executor-routing failure: fail fast

- File: `crates/atelier-cli/src/main.rs:1763`.
- Current: `[routing].executor` profile failure prints to stderr then silently falls back to single-adapter. The user explicitly opted into routing; degraded behaviour shouldn't be a footnote.
- Fix: make it fatal — the user wrote the routing table; if they want best-effort they can remove it. Exit code follows the **operational-failure convention (1)**, matching `run_init`'s failure paths — not 2, which main.rs reserves for usage errors. A `--lenient-routing` escape hatch is speculative; don't add it unless someone asks.
- **Driver consistency (added in review):** the GUI has the same fallback — `resolve_executor_adapter` (`atelier-gui/src/lib.rs:2189`) returns `Err` for a misconfigured profile; audit its call site and make the GUI surface the failure too (error toast / AgentRunRejected path) rather than silently running single-adapter. Fixing only the CLI leaves the two drivers behaving differently on identical config.
- **Verify:** integration test with a `providers.toml` whose `[routing].executor` names a missing profile asserts exit 1 + actionable stderr message; GUI-side unit test (or review) confirms the error propagates instead of falling back.

### Q4 — Configurable concurrent-edit pause timeout

- File: `crates/atelier-cli/src/runner.rs:2493` (hard-coded `5 * 60`s Modal pause).
- Current: no override; on expiry the session auto-resumes without approval.
- Fix: lift to a `[runner] pause_timeout_secs` key in `providers.toml` (the `[runner] max_turns` precedent exists in `config.rs`), defaulting to 300. CLI flag not needed — config-file is the right scope for a policy knob. **Smaller than it looks (verified in review):** `pause_timeout` is already a parameter of the concurrent-edit resolver (`runner/concurrent_edit.rs:14`); `runner.rs:2493` just passes the hard-coded constant, so the change is pure plumbing: config key → Runner field → existing parameter.
- **Verify:** unit test on config parse (key present/absent → override/default); existing concurrent-edit tests unchanged-green.

### Q5 — Warn when `HOME` is missing (registries degrade to bundled-only)

- Files: `crates/atelier-cli/src/runner.rs:2391, 2721, 1721` (subagent types, model profiles, skills).
- Current: missing `HOME` silently degrades to bundled-only registries.
- Fix: one `tracing::warn!` at each site (or a shared helper) — "HOME unset; user-level <registry> not loaded". Depends on F1 to be visible; that's why it's sequenced after Bundle 1.
- **Verify:** unit test with `HOME` removed (use the existing `ENV_LOCK` pattern from v60.91 to serialize env mutation) asserting the warn fires and the bundled set still loads.

### Q6 — TOCTOU windows in core (no action)

- Files: `crates/atelier-core/src/tools/grep.rs:118`, `staging.rs:381`.
- Both fail closed (error propagated / read capped); the audit records them so a future refactor doesn't accidentally widen the window. **No code change.** Already documented in `tasks/audit-2026-06-04.md`.

### Q7 — Production-mode shape-guard warning in the GUI frontend

- File: `crates/atelier-gui/ui/src/lib/state.ts:382` (`castPayload`).
- Current: event shape guards are DEV-only; production silently coerces missing fields on renamed payloads (acknowledged v60.37 B6 trade-off).
- Fix: in prod mode, `console.warn` once per event-kind on shape mismatch (keep the no-throw behaviour). A Set of already-warned kinds caps the noise.
- **Verify:** `npm --prefix crates/atelier-gui/ui run check` clean; unit-level check if the frontend test rig grows one — otherwise manual: feed a mis-shaped payload in dev build, observe single warn.

### Q8 — Unmaintained/unsound dep warnings (no action)

- GTK3 bindings (Tauri 2 Linux backend — upstream-bound), `lru` 0.12.5, `glib` 0.18.5. Documented debt since v60.85, re-confirmed this audit. Re-evaluate alongside D1 at the next toolchain/Tauri bump. **No code change.**

---

## Suggested PR shape

| PR | Items | Risk | Test surface |
|---|---|---|---|
| PR-1 | F1 + F2 + F3 ("CLI failure-visibility") | Low-medium — F2 changes the timeout exit code 0→6 (breaking for scripts that relied on 0; CHANGELOG callout required) | exit-code + AgentStalled regression test, persist-failure test |
| PR-2 | Q1 + Q3 + Q5 (diagnostics polish; Q3 touches the GUI call site too) | Low | routing fail-fast test (cli + gui), HOME-unset test |
| PR-3 | Q2 + Q4 (resume + timeout policy) | Low-medium (touches resume semantics) | resume synthetic-line test, config-parse test |
| PR-4 | Q7 (frontend) | Trivial | svelte-check |
| — | D1, Q6, Q8 | Tracked, no action | next audit re-confirms |

Total estimated diff across PR-1..PR-4 is small (each item is lines-to-tens-of-lines); the value is concentrated in PR-1, which closes the "timed-out run reports success" hole. If only one PR lands this cycle, land PR-1.
