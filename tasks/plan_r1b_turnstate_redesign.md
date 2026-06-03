# Plan — R1b: `TurnState`/`TurnContext` redesign to bring `Runner::run` under CC 50

Date: 2026-06-03. Source: `tasks/plan_audit_2026-06-02_fixes.md` (Bundle 1 → R1b). Prereqs met: Bundle 1 (v60.89, extractions landed) and Bundle 2 (v60.90, coverage 72.9% — the safety net this redesign needs).

**Goal.** Drive the worst function in `crates/atelier-cli/src/runner.rs` below cyclomatic complexity 50 by restructuring `Runner::run`'s agent turn loop around two data structs (mutable `TurnState`, read-only `TurnContext`) and an extracted `run_turn` method — *without changing behaviour*. This is the structural endpoint Bundle 1 deferred: arm-by-arm extraction took `run()` from 182→139 CC but plateaued because the remaining complexity is spread across the pre-loop setup and the per-turn body, which cannot be pulled out cleanly while ~13 mutable variables are threaded as locals.

**This is the highest-risk item in the audit remediation.** It rewrites the live agent loop's state model. Do it as its own focused effort, one phase per commit, with the **workspace-wide** gate green after every phase (`cargo clippy --workspace` + `cargo test --workspace` — never just `-p atelier-cli`; see Design invariant 1).

---

## Success criteria (checkable)

- [ ] **Every function defined in `runner.rs` is < 50 cyclomatic** (`run`, `run_turn`, and each new helper). This is the real target — scoped to the file R1b edits.
- [ ] **Scope caveat — `make metrics` crate max will read ~45, not the `run()` number.** The crate-wide worst function is currently `run` (140); the **second-worst is `run_run` in `main.rs` at CC 45**, which R1b does **not** touch. After R1b succeeds, `make metrics` → `crates.atelier-cli.complexity.cyclomatic.max` will show ≈45 (= `run_run`), still < 50 but with only a 5-point margin in an out-of-scope file. Do **not** read that crate-max number as a measure of R1b; verify `run`/`run_turn`/helpers directly via `rust-code-analysis-cli -m -p crates/atelier-cli/src/runner.rs`. (Optional future cleanup: split `main.rs::run_run` — tracked separately, not part of R1b.)
- [ ] **`Send` preserved** — `cargo clippy --workspace --all-targets -- -D warnings` is clean (see Standing gates). `run()`'s future is spawned `Send` by `atelier-gui` (`tauri::async_runtime::spawn`, lib.rs:1402); a `!Send` regression compiles fine under `-p atelier-cli` and only breaks `atelier-gui`, so the workspace build is the real check.
- [ ] `cargo test --workspace` unchanged-green (same pass count, no skips added).
- [ ] `cargo fmt --all -- --check` clean.
- [ ] `make check` 180/180.
- [ ] **No public API change** — `Runner::run`'s signature is unchanged; `TurnState`/`TurnContext`/`TurnControl` are `pub(super)` (visible only to the parent `runner` module that constructs them), matching the `runner/concurrent_edit.rs` precedent. Their fields are `pub(super)` too so `runner.rs` can build/mutate them.
- [ ] **`run_turn` needs no `#[allow(clippy::too_many_arguments)]`** — it takes 4 params (`&self, ctx, state, turn`). The absence of that allow is the checkable proof the two-struct factoring achieved its purpose (vs. a ~24-arg method).
- [ ] **Behaviour-preserving** — no change to event emission order, state-transition sequence, or `RunReport` contents. The existing tests assert these (overflow/compaction, stall guard, max_turns boundary, concurrent run, ledger, transitions); they must pass unmodified.

## Standing gates (every phase, not just the final PR)

A `Send` regression or a borrow error in the redesign can be invisible to a single-crate test run and only surface when `atelier-gui` spawns the future. So **every phase** runs the full workspace, not `-p atelier-cli`:

- `cargo fmt --all -- --check`
- **`cargo clippy --workspace --all-targets -- -D warnings`** — the load-bearing gate. Catches `Send` regressions (via the `atelier-gui` spawn site) *and* `clippy::await_holding_lock`. **`cargo test -p atelier-cli` alone is NOT sufficient — it cannot see a `!Send` break.**
- `cargo test --workspace`
- `rust-code-analysis-cli -m -p crates/atelier-cli/src/runner.rs` to record the per-function CC delta in the commit message (don't rely on the crate-max metric — see scope caveat).

---

## Current-state analysis (as of v60.92, `runner.rs`)

`run()` = lines 1426–~2790. Three regions:

| Region | Lines (approx) | Rough role |
|---|---|---|
| **Pre-loop setup** | 1426–1942 | hooks/DoD/sandbox/registry/dispatcher/ledger build; session spawn; sink/watcher/resolver task spawns; model profile probe + capability row + suitability; resume hydration; system prompt + few-shot; initial state + cache inits |
| **Turn loop body** | 1943–~2443 | `for turn in 0..self.max_turns { … }` — per-turn: tools_spec, mental-model closure, adapter call + overflow (already extracted), ledger ModelCall, `parse_envelope` (extracted), conformance degradation, assistant commit + plan_update + claimed_changes, `execute_tool_calls` (extracted), per-turn context/memory snapshot emission, claimed_done break, stall guard (`last_turn_was_all_subagent`, extracted) |
| **Post-loop** | ~2444–2790 | DoD warning; build `OnDiskSession` snapshot (~2630) + persist; assemble `RunReport`; abort/await `auto_reload_task`, `sink_handle`, `file_watcher_handle` |

### The 13 loop-carried mutable variables → `TurnState`

| Field | Current decl | Type |
|---|---|---|
| `active_strategy` | 1726 | `Strategy` |
| `envelope_conformance` | 1730 | `ConformanceRingBuffer` |
| `messages` | 1749 | `Vec<Message>` |
| `last_context_items` | 1910 | `Option<Vec<ContextItemSummary>>` |
| `last_context_meter` | 1911 | `Option<(u32, u32)>` |
| `last_memory_cards` | 1912 | `Option<Vec<MemoryCardSummary>>` |
| `last_plan_steps` | 1913 | `Option<Vec<PlanStep>>` |
| `token_count_cache` | 1914 | `Option<(u64, AdapterTokenCount)>` |
| `turns` | 1924 | `usize` |
| `final_state` | 1925 | `State` |
| `observed_changes` | 1938 | `Vec<ObservedChange>` |
| `last_envelope` | 1939 | `Envelope` |
| `last_assistant_text` | 1941 | `Option<String>` |

`event_rx` (1585), `resumed_session` (1802), and `snapshot` (2630) are **not** loop-carried (drain subscription / resume hydration / post-loop persistence) — they stay as `run()` locals.

### Read-only context the loop reads → `TurnContext`

Borrowed from `run()` setup locals: `workspace` (1427, `PathBuf`), `sandbox` (1443, `SandboxPolicy`), `session_handle` (1489), `bus` (1498, `broadcast::Sender<Event>`), `session_dispatcher` (1520, `Arc<SessionDispatcher>`), `context_manager`/`memory_store`/`plan_canvas` (1477–1479, `Arc<Mutex<…>>`), `tools_spec` (1468), `audit_log_path` (1747), `session_id` (1738, `Copy`).

`self.*` fields the loop uses (free via `&self`, not in `TurnContext`): `adapter`, `executor_adapter`, `subagent_depth`, `cost_policy`, `degradation_window`, `degradation_threshold`, `max_turns`, and the already-extracted `execute_tool_calls`.

### Control flow in the loop body (drives the `TurnControl` enum)

- **0** `continue` (the overflow retry `continue` already lives inside the extracted inner adapter loop / `resolve_context_overflow`).
- **3** `break` — the inner `let response = loop { … break r }` (stays inside `run_turn`), plus the two turn-loop breaks: claimed_done → `Verifying`, stall → `AwaitingUser`/`Verifying`. Both set `final_state` before breaking.
- **2** `return Err` + **5** `.await?` — error exits from `run()` entirely.

So the turn loop body maps to: `Continue` (next turn), `Break` (leave loop; `final_state` already set in `state`), or error (propagate via `?`).

---

## Target design

```rust
// runner/turn.rs (new submodule, mirrors runner/concurrent_edit.rs)

/// Mutable state carried across turns of the agent loop.
/// `pub(super)` (+ `pub(super)` fields) so `runner.rs` builds/mutates it;
/// matches the `runner/concurrent_edit.rs` visibility precedent.
pub(super) struct TurnState {
    pub(super) active_strategy: Strategy,
    pub(super) envelope_conformance: ConformanceRingBuffer,
    pub(super) messages: Vec<Message>,
    pub(super) observed_changes: Vec<ObservedChange>,
    pub(super) last_envelope: Envelope,
    pub(super) last_assistant_text: Option<String>,
    pub(super) token_count_cache: Option<(u64, AdapterTokenCount)>,
    pub(super) last_context_items: Option<Vec<ContextItemSummary>>,
    pub(super) last_context_meter: Option<(u32, u32)>,
    pub(super) last_memory_cards: Option<Vec<MemoryCardSummary>>,
    pub(super) last_plan_steps: Option<Vec<PlanStep>>,
    pub(super) turns: usize,
    pub(super) final_state: State,
}

/// Read-only references the turn loop needs. One lifetime; all fields
/// borrow `run()` locals that outlive the loop. Arcs are borrowed (not
/// cloned) to keep it zero-cost.
pub(super) struct TurnContext<'a> {
    pub(super) workspace: &'a Path,
    pub(super) sandbox: &'a SandboxPolicy,
    pub(super) session_handle: &'a SessionHandle,
    pub(super) bus: &'a broadcast::Sender<Event>,
    pub(super) session_dispatcher: &'a SessionDispatcher,
    pub(super) context_manager: &'a parking_lot::Mutex<ContextManager>,
    pub(super) memory_store: &'a parking_lot::Mutex<MemoryStore>,
    pub(super) plan_canvas: &'a parking_lot::Mutex<PlanCanvas>,
    pub(super) tools_spec: &'a [ToolSpec],
    pub(super) audit_log_path: &'a Path,
    pub(super) session_id: SessionId,
}

/// What `run_turn` tells the loop to do next.
pub(super) enum TurnControl {
    Continue,
    Break,
}
```

`run_turn` is a method on `Runner` (needs `&self` for adapter/policy fields):

```rust
async fn run_turn(
    &self,
    ctx: &TurnContext<'_>,
    state: &mut TurnState,
    turn: usize,
) -> Result<TurnControl, RunError>
```

`run()`'s loop collapses to:

```rust
for turn in 0..self.max_turns {
    match self.run_turn(&ctx, &mut state, turn).await? {
        TurnControl::Continue => {}
        TurnControl::Break => break,
    }
}
```

**Why structs over a param-list `run_turn`.** A method taking the 13 state vars + 11 context refs as parameters is ~24 args — a worse smell than the long function, and clippy's `too_many_arguments` would need a blanket allow. The two-struct split is the idiomatic Rust factoring: `&TurnContext` (shared) and `&mut TurnState` (exclusive) are disjoint borrows, so there is no aliasing conflict, and `&self` supplies the Runner fields.

### Design invariants (must hold at every phase)

1. **`run_turn`'s future must stay `Send`.** `atelier-gui` spawns `runner.run().await` via `tauri::async_runtime::spawn` (lib.rs:1402), which requires `Send`. **Never bind a `parking_lot::MutexGuard` (from `ctx.context_manager`/`memory_store`/`plan_canvas`) and hold it across an `.await`.** The current loop is already disciplined — every lock is a method-chained temporary dropped at statement end (`ctx.context_manager.lock().panel_snapshot().items`, `…lock().add(…)`, `…lock().summarise()`); keep that pattern. A violation compiles fine under `cargo test -p atelier-cli` and only breaks the `atelier-gui` build — which is why the standing gate is workspace-wide.

2. **Closure captures of `TurnState` fields are the #1 borrow hazard.** The per-turn `project_messages_for_call` closure captures `ctx.session_dispatcher` and reads `messages`. Once `messages` is `state.messages`, a long-lived closure borrowing `&state.messages` can collide with a later `&mut state.messages`. Rust 2021 disjoint closure captures help but don't always save it. *Mitigation:* before defining the closure, pull what it needs into a local (e.g. snapshot the mental-model text into a local `String`), or build `messages_for_call` inline without a persistent closure. Pass disjoint fields at call sites (`&mut state.messages`, `&mut state.observed_changes`) rather than `&mut state` whole.

3. **Post-loop code reads — and one writes — `state`.** The rule: after the loop, every use of a loop-carried variable becomes `state.<x>` (the compiler enforces this — the locals no longer exist — but trust the enumeration below, not memory). `state` must stay borrowable **mutably** after the loop because `final_state` is *written* there. The complete post-loop set (verified against lines 2444–2792 as of v60.92):

   | Var | Post-loop use | Where |
   |---|---|---|
   | `final_state` | **read + written** — `if final_state == Verifying { … final_state = Done }` | 2470, 2615 |
   | `observed_changes` | §7 verify pass + LSP re-ingest | 2486, 2503, 2603 |
   | `last_envelope` | §7 verify pass | 2481, 2603 |
   | `messages` | persisted into the `OnDiskSession` conversation | 2644 |
   | `envelope_conformance` | `.snapshot()` → `RunReport` | 2779 |
   | `turns` | → `RunReport` (`turns`, `turns_used`) | 2787–2788 |
   | `last_assistant_text` | moved into `RunReport.final_assistant_text` | 2792 |

   Implications: (a) do **not** move `state` into a post-loop helper before the DoD/verify step — `final_state` is still mutated there (`state.final_state = State::Done`); (b) `last_assistant_text` is *moved* out (use the field directly or `std::mem::take`); (c) the `turns = turn + 1` assignment moves into `run_turn` (writing `state.turns`); the max-turns-reached path leaves `state.final_state` at its last value (today `Streaming`) — preserved because the loop exits via the range with the last `Continue`. This table doubles as the Phase 0 "what reads from state" checklist.

---

## Phased implementation (one commit per phase, green at each)

> **Per-phase gate (all phases):** `cargo fmt --all -- --check` · **`cargo clippy --workspace --all-targets -- -D warnings`** · `cargo test --workspace` · record per-function CC via `rust-code-analysis-cli -m -p crates/atelier-cli/src/runner.rs`. The workspace clippy/test is mandatory at *every* phase — a `!Send` or borrow regression in `run_turn` is invisible to `-p atelier-cli` and only fails the `atelier-gui` build (see Design invariant 1).

### Phase 0 — Coverage pre-flight (no production change)

Before moving any code, confirm the arms about to move are actually guarded by an asserting test — a pure move is only safe if a test pins the behaviour it relocates.

- Map each moved arm to a test that asserts its observable effect: assistant commit → `MessageCommitted`; tool dispatch → existing dispatch tests; per-turn snapshots → `ContextItems`/`ContextSnapshot`/`MemoryCards`; ledger → `LedgerAppended`; transitions → the `Transitioned` assertions; claimed_done/stall → the max_turns + stall tests.
- **Known suspected blind spot:** the `plan_update → PlanSnapshot` emission. Grep the test suite for `PlanSnapshot`; if nothing asserts it, add one small integration test (Mock adapter emitting an envelope with a `plan_update`) **before** Phase 2 so the move is covered.
- **Verify:** every moved arm has ≥1 asserting test; add the minimum missing test(s). No production code changes in this phase.

### Phase 1 — Introduce `TurnState` and migrate the 13 vars (scaffolding)

- Add `runner/turn.rs` (mirrors `runner/concurrent_edit.rs`); declare `TurnState`, `TurnContext<'a>`, `TurnControl`. Wire `mod turn;` + `use` into `runner.rs`.
- **Integration-test resolution — no new `#[path]` needed.** `tests/run_integration.rs` mounts the runner via `#[path = "../src/runner.rs"] mod runner;`; rustc resolves `mod turn;` to `../src/runner/turn.rs` automatically (the existing `mod concurrent_edit;` already relies on this exact mechanism, and those tests pass). The `cargo test --workspace` gate compiles the integration-test crate, so a resolution mistake fails loudly and immediately.
- Replace the 13 `let mut <x>` in `run()` with one `let mut state = TurnState { … };` and rewrite every in-loop reference `<x>` → `state.<x>` (and the extracted-helper call sites that take `&mut messages` / `&mut observed_changes` / `&mut token_count_cache` → `&mut state.messages`, etc.).
- **No extraction yet** — purely a data move; `run()`'s CC is unchanged.
- **Honest framing:** this phase is *scaffolding for Phase 2, not a standalone improvement* — a `TurnState` used only inside one function and passed nowhere is indirection without benefit. It earns its keep only once `run_turn` exists. If you stop the project here, revert this phase too (don't leave an orphan struct). Treat Phase 1+2 as the atomic unit; they may be combined into one commit if the diff stays reviewable.
- **Verify:** workspace gate green. (Large mechanical diff; compiler + tests catch any mis-rename.)

### Phase 2 — Build `TurnContext`; extract `run_turn`

- After setup, construct `let ctx = TurnContext { workspace: &workspace, … };`. (Field inits are coercion sites, so `&session_dispatcher` / `&context_manager` deref-coerce from `&Arc<T>` to the `&T` field type — no `&*` needed.)
- **Location:** `run_turn` stays in `runner.rs`'s `impl Runner` block (it needs `&self` for `adapter`/`executor_adapter`/policy fields); only the data types live in `runner/turn.rs`. It must compile **without** `#[allow(clippy::too_many_arguments)]` — it has 4 params (`&self, ctx, state, turn`). If that allow becomes necessary, the struct factoring has failed; stop and reconsider rather than suppressing the lint.
- Move the entire `for turn` **body** into `run_turn(&self, ctx, state, turn) -> Result<TurnControl, RunError>`. Translate the two turn-loop `break`s → `return Ok(TurnControl::Break)` (with `state.final_state` set first, as today); the fall-through end-of-body → `Ok(TurnControl::Continue)`; keep all `?` error propagation. Move the `turns = turn + 1` write to `state.turns` inside `run_turn` (Design invariant 3).
- Apply Design invariant 2 here: the `project_messages_for_call` closure must not hold a `&state.messages` borrow that collides with the later `&mut state.messages`.
- `run()`'s loop becomes the 4-line match above.
- **Verify:** workspace gate green; record `run()` and `run_turn` CC (run() should drop sharply; run_turn will be large — Phase 4 handles it).

### Phase 3 — Reduce `run()`'s pre-loop setup CC (conditional)

If after Phase 2 `run()` is still ≥ 50 (likely — the pre-loop setup is dense), reduce it. **Two strategies — prefer (a):**

- **(a) Sub-block extraction (recommended, lower risk).** Extract the dense, cohesive setup *sub-blocks* into focused helpers that return owned values, leaving the wiring in `run()`: e.g. `resolve_model_profile(&self) -> (profile, capability_row, suitability, active_strategy)`, `build_system_prompt(&self, …) -> Vec<Message>` (system prompt + few-shot), and the resume-hydration block. This buys most of the CC reduction with **no ownership gymnastics** — the spawned task handles and borrowed locals stay in `run()`.
- **(b) Monolithic `prepare_run` (only if (a) is insufficient).** Extract the whole setup into `prepare_run(&self) -> Result<RunScaffold, RunError>`. ⚠️ This is borrow-awkward: `TurnContext<'a>` holds references, so `RunScaffold` must *own* the values and expose an `as_context(&self) -> TurnContext<'_>`, and the spawned tasks (`auto_reload_task`, `sink_handle`, `file_watcher_handle`) must be returned and joined/aborted in `run()` post-loop, never dropped inside `prepare_run`. **Concrete trap:** `session_handle` is *consumed* post-loop — `session_handle.send(SessionCommand::Shutdown).await` (2759) then `drop(session_handle)` (2764). If `RunScaffold` owns `session_handle` while `TurnContext` borrows `&scaffold.session_handle`, that borrow must end (loop + ctx dropped) before the scaffold can hand the handle to the post-loop shutdown — an ordering the borrow checker will enforce but that's fiddly to satisfy. Strategy (a) sidesteps this entirely (handle stays a `run()` local). Only reach for (b) if (a) leaves `run()` ≥ 50.
- **Verify:** workspace gate green; `run()` CC < 50.

### Phase 4 — Decompose `run_turn` until it is < 50

- `run_turn` after Phase 2 will itself be > 50 (it inherited the whole body). Extract its remaining cohesive arms, each behaviour-preserving, until `run_turn` < 50. Candidate arms (each already self-contained):
  - **per-turn snapshot emission** — the context-snapshot + context-items + memory-cards + plan-steps diff-and-emit block (the `last_*` caches) → `emit_turn_snapshots(ctx, state)`.
  - **assistant-turn commit** — push assistant message + context item + `MessageCommitted` + plan_update apply + claimed_changes emit → `commit_assistant_turn(...)`.
  - **stall-guard decision** — the `!made_tool_calls` branch (uses the already-extracted `last_turn_was_all_subagent`) → `resolve_stall(...)` returning the `TurnControl`/`final_state`.
  - **(fallback) adapter-response acquisition** — the inner `let response = loop { … }` (adapter call + `resolve_context_overflow`) → `acquire_response(...)`, if the three above don't get `run_turn` under 50.
- **Escape hatch (avoid over-fragmentation).** The goal is readability, not a number. If reaching < 50 would require more than ~3 cohesive helpers (i.e. you're carving out tiny non-cohesive fragments just to drop CC), **stop, accept the floor, and document the achieved value** (e.g. "run_turn at 54, further splitting would harm readability") in this plan and the commit message. CC < 50 is a target, not a defect gate — consistent with how Bundle 1 was banked at 139 rather than forced.
- **Verify:** every `runner.rs` function < 50 (or documented floor); `cargo test --workspace` green; `make check` 180/180.

### Phase 5 — Docs + final verification

- Update `tasks/plan_audit_2026-06-02_fixes.md` (R1b → DONE), `tasks/todo.md`, `CHANGELOG.md`, `STATUS.md` with the final per-function CC numbers (and note the new crate-max worst-fn is `main.rs::run_run` ≈45, out of scope).
- Run the full standing-gate suite and the coverage check: `cargo llvm-cov --workspace --summary-only` — atelier-cli line coverage must not regress below 70% (use the **full-suite** number, not `--lib -p atelier-cli`, which misleadingly excludes `main.rs` — see the Bundle 2 C1 note).

---

## Risk analysis & mitigations

- **`!Send` regression invisible to single-crate tests (highest-impact).** Holding a `parking_lot::MutexGuard` across `.await` in `run_turn` makes the future `!Send`; `cargo test -p atelier-cli` stays green but `atelier-gui` fails to build at its `tauri::async_runtime::spawn` site. *Mitigation:* Design invariant 1 (method-chained locks only) + the workspace-wide per-phase gate (clippy `--workspace` also fires `await_holding_lock`). This is why the gate is never scoped to `-p atelier-cli`.
- **Silent behaviour change in the core loop** (event order, transition sequence, cache-diffing). *Mitigation:* every phase is a pure move; Phase 0 confirms each moved arm is test-guarded first. The Bundle 2 tests (`max_turns_one_executes_exactly_one_turn`, `concurrent_runs…`, `parse_envelope`, `last_turn_*`) plus the existing overflow/compaction/stall/ledger/transition tests are the contract. Run them every phase; never edit a test to make it pass.
- **Borrow-checker churn** from `TurnContext<'a>` references, `&mut state` aliasing, and closure captures (Design invariant 2). *Mitigation:* `&TurnContext` (shared) and `&mut TurnState` (exclusive) are disjoint; split field borrows at call sites (`&mut state.messages`, not `&mut state`); pre-bind closure inputs into locals.
- **Phase 1's wide rename diff** hides a typo. *Mitigation:* compiler catches type mismatches; tests catch logic. Keep Phase 1 rename-only (no logic edits) so the diff is reviewable as mechanical.
- **Setup-extraction ownership (Phase 3b)** — a monolithic `prepare_run` returning a struct that `TurnContext` borrows from, plus task-handle lifetimes, is the trickiest borrow case. *Mitigation:* prefer Phase 3a (sub-block extraction) which avoids it entirely; the SIGINT/resume tests (`sigint_resume.rs`, `external_cancel_writes_partial_session_to_disk`) guard cleanup semantics either way.

## Rollback

Each phase is one commit. If a phase regresses and the cause isn't obvious within two attempts, `git revert` that phase's commit. **Caveat:** Phase 1 (struct migration) is scaffolding, not a standalone win — if you revert Phase 2, revert Phase 1 too rather than leaving an orphan `TurnState` used in a single function. Phases 3 and 4 *are* independent increments and can be reverted individually.

## Sequencing & estimate

Independent of Bundle 3 (landed) and Bundle 4 (landed). **Estimate: ~1–1.5 days** of focused work — Phase 2 (extraction + the `Send`/closure borrow work) and Phase 4 (decomposition) are the substantive ones, and the borrow/`Send` friction makes the earlier "½ day" optimistic. Do not interleave with other `runner.rs` changes — land R1b as a contiguous series so the mechanical diffs stay reviewable.
