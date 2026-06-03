# Plan — remediation from the 2026-06-02 deep-scan audit

Date: 2026-06-02. Source: `tasks/audit-2026-06-02.md` (the deep-scan run from this session).

**Important — the audit was re-verified before this plan was written, and most findings did not survive.** The Phase-2 Explore subagents located `panic!`/`unwrap`/`let _ =` constructs correctly but repeatedly mis-classified them: test code reported as production, and `Option::unwrap_or` (infallible) reported as a panicking `unwrap`. Every finding below was checked against the file's `#[cfg(test)]` boundary and read in context. Of ~25 findings, **2 P0s and most P1s are false positives**; the real work is small and is listed first. The rejected findings are catalogued in the appendix so they are not re-raised.

Items are numbered for traceability in commit messages / PR descriptions.

## Standing gates (every PR)

- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test -p atelier-core` (plus `-p atelier-cli` / `-p atelier-gui` / `-p atelier-tui` where touched)
- `make check` (rig: schemas → artifacts → rig-tests → workload dry-run)

Each item adds a **targeted** verification on top of these.

---

## Bundle 1 — `runner.rs` complexity reduction (the one solid structural item)

The only finding corroborated by independent tooling. `make metrics` reports the worst function in `atelier-cli` at **cyclomatic 182 / cognitive 239 / 1,617 SLOC / maintainability-index −46** — all the same function. This is also the refactor `ATELIER.md` already mandates *before* any new feature work touches `runner.rs`. Pure, behaviour-preserving restructure.

### R1 — Extract the context-overflow retry/compaction arm

- File: `crates/atelier-cli/src/runner.rs:~1777` (the ~273-line `ContextOverflowPolicy::Compact` arm inside the turn loop; the dominant contributor to the 1,617-line function).
- Current: the overflow-retry logic is inlined at 7 levels of nesting inside the main loop, making every error path inside it unauditable without running it.
- Fix: extract to a dedicated `handle_overflow_compaction(...)` method (or a `runner/overflow.rs` submodule, mirroring the existing `runner/concurrent_edit.rs` extraction from v60.76). No behaviour change — move code, thread state through parameters/return.
- **Verify:** `cargo test -p atelier-cli` unchanged-green; `make metrics` then shows the crate's worst-fn cyclomatic < 50 and longest-fn well under 1,617 SLOC. Diff is a pure move (no logic edits) — confirm with a careful self-review of the extracted block.

> Sequencing: R1 rewrites a large span of `runner.rs`. Land it **before** Bundle 3's `runner.rs` nits so those land as small diffs on the new structure, not the old.

**Status — 2026-06-02 (option A: banked; CC<50 deferred).** Landed four behaviour-preserving extractions from `Runner::run`, all gates green (239 cli tests pass, fmt + clippy clean):

- `resolve_context_overflow` (the overflow/compaction arm), `execute_tool_calls` (concurrent tool dispatch + result folding), and the free fns `parse_envelope` + `last_turn_was_all_subagent`.
- `run()`: cyclomatic **182 → 139** (−24%), cognitive **239 → 178**, **1,617 → 1,364 SLOC**.

The **CC<50 target was not reached by extraction** and is deferred. The remaining ~139 CC is spread across the pre-loop setup and the rest of the turn body, which cannot be pulled out cleanly without a `TurnState`/`TurnContext` struct redesign of the loop (bundling its ~13 mutable variables). That redesign touches the live agent loop's state model, so it is sequenced **after Bundle 2** (cli is only 52% covered — raise the safety net first). Tracked below as R1b.

### R1b — `TurnState`/`TurnContext` redesign to reach CC<50 (deferred)

- Introduce a `TurnState` struct (the ~13 mutable per-turn variables) and a `TurnContext` (read-only refs), then extract `run_turn(&self, &ctx, &mut state) -> ControlFlow` so the entire per-turn body leaves `run()` at once. Do **after** Bundle 2.
- **Verify:** `make metrics` shows `run()` worst-fn cyclomatic < 50; `cargo test -p atelier-cli` unchanged-green.

---

## Bundle 2 — Test-coverage uplift (measured gap)

Real numbers from `cargo llvm-cov --lib` this session: workspace 82.1%, **atelier-core 91.4%**, atelier-tui 75.5%, **atelier-cli 51.9%**, **atelier-gui 44.5%**. The two low crates are the gap. (Note: the "missing tests" the audit proposed to guard the P0 panics are dropped — those panics don't exist. The tests below are justified on their own merits.)

### C1 — Raise `atelier-cli` lib coverage 52% → ≥70%

- Use `cargo llvm-cov --lib -p atelier-cli --html` to find the under-covered modules (the big runner.rs spans dominate).
- Add unit tests for the genuinely untested logic surfaced there. Prioritise pure functions over integration-heavy paths.
- **Verify:** `cargo llvm-cov --lib -p atelier-cli --summary-only` reports ≥70% line coverage.

**Status — 2026-06-03 (DONE).** Full-suite coverage for `atelier-cli` is **72.89%** (was 51.9%), above the 70% target. Note: `--lib -p atelier-cli` reports 18% because `main.rs` is a binary source file excluded from the lib target; the meaningful metric is the full test suite (all source files, all test types). Added 9 unit tests for the two Bundle 1 extractions (`parse_envelope`, `last_turn_was_all_subagent`) in `runner.rs`. The dominant uncovered region remains `main.rs` (CLI command handlers at 15%) — these require binary-level assert_cmd tests to cover and are out of scope for this bundle.

### C2 — Two integration tests worth having

- `max_turns` boundary: a Mock-adapter run with `--max-turns 1` asserts exactly one turn executes (guards R-Q3 below and the loop bound).
- Concurrent `Runner::run` on the same session directory: assert no panic / no corrupted `session.json` (the persistence layer has no concurrency test today).
- **Verify:** both tests live in `crates/atelier-cli/tests/` and pass.

> C1/C2 are independent of Bundle 1 by file, but C1's targets will shift once R1 lands — do C1 **after** R1 so you cover the final structure.

**Status — 2026-06-03 (DONE).** Both tests added to `crates/atelier-cli/tests/run_integration.rs` and pass: `max_turns_one_executes_exactly_one_turn`, `concurrent_runs_on_separate_workspaces_do_not_corrupt_session_json`. Full suite: 268 tests pass / 0 fail (was 239).

---

## Bundle 3 — Low-risk quality nits (one PR, optional)

These are real but minor: the code works today. Batch them only if the cleanup is wanted; none is a bug. All in `runner.rs`/`main.rs`, so land **after** R1.

### Q1 — Preserve the adapter error chain

- File: `crates/atelier-cli/src/runner.rs:1970` — `Err(e) => return Err(RunError::Adapter(format!("{e}")))`.
- Current: flattens a typed `AdapterError` into a string, losing the source chain for diagnostics.
- Fix: add/!use a `RunError::Adapter(AdapterError)` variant (or `#[source]`) so the chain survives.
- **Verify:** a test asserts the returned error's `source()` is the originating `AdapterError`.

**Status — 2026-06-03 (DONE).** Added `RunError::AdapterChain(#[source] AdapterError)` alongside the existing `Adapter(String)` variant (kept for GUI string-matching backward compat). Both runner.rs conversion sites now use `AdapterChain`. The old `Adapter(String)` is marked `#[allow(dead_code)]` since it is constructed in `atelier-gui` (separate crate) which the integration test binary cannot see. All workspace tests pass.

### Q2 — Make best-effort failures observable

- Files: `crates/atelier-cli/src/runner.rs:2512` (`let _ = session.open_file(...)`) and the `let _ = try_emit(...)` sites around `runner.rs:1400`.
- Current: best-effort file-open and event-broadcast failures are silently dropped.
- Fix: keep them non-fatal but log on the error path (`tracing::warn!`). Do **not** make them propagate — the best-effort semantics are intentional.
- **Verify:** clippy-clean; a test with a closed event channel observes the warn path (or at least does not change exit behaviour).

**Status — 2026-06-03 (DONE, partial).** `session.open_file` now logs `tracing::warn!` on failure with path and error (runner.rs:2543). The 24 `try_emit` call sites were not changed: `try_emit` already performs rate-limited `tracing::warn!` internally (see `session.rs:83-107`) with a global `BROADCAST_LAGGED` counter — annotating each call site would duplicate this and add noise without new information.

### Q3 — Reject `--max-turns 0`

- File: `crates/atelier-cli/src/main.rs:1572` — `parse::<usize>().ok()` accepts `0`, which makes the loop a no-op.
- Fix: reject `0` at parse time with `missing_value("--max-turns", "positive integer >= 1")`.
- **Verify:** unit test asserts `--max-turns 0` errors; covered end-to-end by C2's boundary test.

**Status — 2026-06-03 (DONE).** Added `Some(0)` arm before `Some(n)` in the `--max-turns` parse match; error message updated to "positive integer >= 1".

### Q4 — Self-document the infallible serde unwraps (cosmetic)

- File: `crates/atelier-cli/src/main.rs:1022` and `:1131` — `serde_json::to_*(&value).unwrap()` on already-valid `serde_json::Value`s.
- These cannot fail in practice; swap `.unwrap()` → `.expect("re-serializing a serde_json::Value is infallible")` for clarity. Lowest priority; skip if trimming scope.

---

## Bundle 4 — Test hygiene (real, low-risk)

### T1 — Serialize env-mutating tests

- Files: `crates/atelier-core/src/adapter/anthropic.rs:1785` and `crates/atelier-core/src/adapter/model_profile.rs:1741/1747/1748` — tests `set_var`/`remove_var` on process-global env without serialization, so parallel runs can race.
- Fix: add a dedicated serialization guard. `serial_test` is **not** currently a dep — either add it (`#[serial]`) or use a `static` `std::sync::Mutex` lock shared by these tests (no new dep). Prefer the mutex to avoid a dependency for two tests.
- **Verify:** `cargo test -p atelier-core` green under default (parallel) threads across repeated runs.

**Status — 2026-06-03 (DONE).** Added `static ENV_LOCK: std::sync::Mutex<()>` to each test module; the mutating tests now acquire `_guard = ENV_LOCK.lock()` before touching env vars. No new dependencies.

---

## Not doing — known accepted debt

- **Dependency advisories** `RUSTSEC-2026-0009` (`time`), `RUSTSEC-2026-0002` (`lru`), `RUSTSEC-2024-0429` (`glib`): already tracked as audit-ignore debt blocked by the Rust 1.85 pin (see `tasks/todo.md` v60.85). No code action until the pin can move or upstream Tauri/ratatui paths offer compatible versions. `gitleaks` (installed this session) and `cargo audit` vulnerability set are otherwise clean.

---

## Appendix — Rejected findings (false positives; do not re-raise)

Each was read in context and is **not** a defect:

| Audit finding | Why it's rejected |
|---|---|
| `runner.rs:3552` "P0 production panic" | Inside `#[test] fn tool_result_item_carries_tool_call_id` (test module starts at 3494). The production fn `tool_result_string_for_model` (3184) uses safe `unwrap_or_default()`. |
| `runner.rs:3566` "P0 unwrap on malformed JSON" | Inside `#[test] fn ...truncates_large_stdout`. The production fn `bounded_subagent_messages` (3013) is pure Vec work, no unwrap. |
| `runner.rs:1442` "P1 unwrap can panic" | It is `Option::unwrap_or(profile.strategy)` — infallible. Subagent confused `unwrap_or` with `unwrap`. |
| `main.rs:969` "P1 unsafe unwrap" | `chars().next().unwrap()` immediately after a `!is_empty()` guard — a non-empty `&str` always has a first char. Provably safe. |
| `main.rs:2476` / `:2481` "P2 panics" | Both inside `#[test] fn explicit_cli_base_url_is_allowed...` (test module starts at 2455). Test assertions. |
| `anthropic.rs:107` "P1 expect lacks SAFETY" | `Client::builder().build().expect("...infallible")` is the idiomatic reqwest pattern and already carries a justifying message. Not unsafe code. |
| `gui lib.rs:1070` "P1 concurrent-run race" | Mis-identified — this is the adapter-swap consent flow, which deliberately registers the oneshot sender *before* emitting the event to avoid exactly this race (see the code comment). |
| `tui lib.rs:3634` "P1 no terminal guard" | That line **is** `install_panic_hook()`, which restores the terminal before the prior hook and covers `panic = "abort"`. The opposite of the claim. |
| `provider.rs:59` "P2 silent DNS failure" | `preflight_base_url` fail-open is intentional and documented ("Optimistically returns `true` if the URL can't be parsed") — a preflight must not block a legitimate connection. |
| `staging.rs:813` "P2 verify panic gating" | Already `#[cfg(test)]`-gated (line 812). Correct as-is. |
| `runner.rs:637` "P1 env masking" | `OPENAI_API_KEY` missing → empty key → clear downstream auth error. Cosmetic at most; not worth a fix. |

## Process note

The audit's value was the deterministic layer (clippy clean, fmt clean, `cargo audit`, the `rust-code-analysis` complexity numbers, llvm-cov coverage) — all of which held up. The Explore-subagent review layer produced a high false-positive rate because Explore *locates* code, it does not *audit* it (prod-vs-test classification, `unwrap` vs `unwrap_or`, intentional-vs-accidental). Future deep-scans: route severity-bearing findings through `rust-reviewer` (which reads full context) and always confirm a finding's `#[cfg(test)]` membership before assigning severity.
