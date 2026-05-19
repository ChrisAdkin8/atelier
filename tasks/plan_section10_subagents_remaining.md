# Plan — §10 Sub-agent delegation: remaining work

**Parent plan:** `tasks/plan_section10_subagents.md`
**As of:** v60.58

## State of play

From the code survey:

| Item | State |
|---|---|
| WU-1 registry, WU-3 spawner trait, WU-4 tool impl | Done |
| WU-7 §7 gate ordering | Done |
| WU-8 bus *event variants* | Done (all 5 variants in `session.rs:575–607`, wired into GUI + TUI projections) |
| WU-5 typed persistence map | Done (typed `BTreeMap<String, PersistedSubagent>`) |
| WU-5 resume safety | **Gap** — resume does not carry forward prior sub-agents or mark in-flight as cancelled |
| WU-6 trust budget | **Spec-deferred**: spec line 550 says `max_budget` is "future field — not v1". Only a cost-fields test needed. |
| WU-8 bus capacity | **Small gap** — `EVENT_BUFFER = 256` in `session.rs:45` is never scaled by `BUS_FANOUT_FACTOR = 4` from `subagents.rs:50`; not yet applied in `Runner` |
| WU-10 GUI pane | **Blocked** — GUI is in chat-REPL mode (v60.43-49); no Runner-backed agent mode to surface sub-agent events in UI |
| WU-11 TUI pane | Pending — event log has string entries; no dedicated sub-agent list widget |
| WU-13 remaining tests | 5 of 7 scenarios still unwritten |
| WU-14 cancel-race docs | Trivial — one-line description update |

---

## Success criteria

1. `cargo test -p atelier-cli --test run_integration -- subagent` ≥ 4 tests green (adds cancel cascade + cost-fields + schema round-trip).
2. Resume against a session that had a completed sub-agent preserves the `subagents` map entry in the next save.
3. Resume against a session that had an in-flight sub-agent emits `SubagentCancelled` and writes `status: "cancelled"`.
4. `cargo test -p atelier-cli` green (no regression).
5. `cargo fmt --check && cargo clippy --workspace -- -D warnings` clean.
6. `make check` passes (schema round-trip for `subagents` map validates against `schemas/session/v1.json`).

---

## Work units (execution order)

### R-1 · Resume safety fix  *(small, ~1 day)*

**File:** `crates/atelier-cli/src/runner.rs`

**Problem (lines 2327-2329):** The snapshot built on resume only carries forward `recovery_log` from the prior on-disk session. The `subagents` field is reconstructed purely from `spawner.drain_completed()` — which is empty on a fresh resumed run that spawns no new sub-agents. Two consequences:
1. Previously completed sub-agents are **lost** from the session JSON after a resume+re-save.
2. Sub-agents that were `running` when the prior run was interrupted are never marked `cancelled`.

**Fix — two insertions after line 2328:**

```rust
// Carry forward completed sub-agents from the prior run so the
// subagents map is additive across resumes.
if let Some(resumed) = &resumed_session {
    for (id, rec) in &resumed.subagents {
        snapshot.subagents.entry(id.clone()).or_insert_with(|| rec.clone());
    }
}
```

The `entry().or_insert_with()` pattern means a sub-agent that re-ran in the resumed session wins (drain_completed writes it first); prior completed entries fill in the rest.

**Cancellation of in-flight entries:** add a second block immediately after:

```rust
// Mark any sub-agent that was `running` in the prior session as
// cancelled — sub-agent runs are not resumed in v1 (spec §10).
for rec in snapshot.subagents.values_mut() {
    if rec.status == "running" {
        rec.status = "cancelled".to_string();
        let _ = try_emit(
            &bus,
            Event::SubagentCancelled {
                id: rec.description.clone(), // best proxy without the id here
                reason: "resume_inflight".to_string(),
            },
        );
    }
}
```

Note: `PersistedSubagent.status` is a plain `String`; the "running" value is not currently written by any code path (all writes go through `drain_completed` which only records terminal states). Still, the guard is defensive and closes the spec §10 contract. If a "running" entry somehow appears (future change or manual edit), it won't leak into the next session.

**Tests:**
- `resume_preserves_prior_completed_subagents` — resume against a session JSON that has one `subagents` entry; after the resumed run, assert the map entry survives in the next save.
- `resume_marks_inflight_subagents_cancelled` — manually inject a `status: "running"` entry into a session JSON; resume + run; assert the entry is `"cancelled"` in the final save and `SubagentCancelled` was emitted.

**Verify:** `cargo test -p atelier-cli --test run_integration -- resume_preserves_prior resume_marks_inflight`

---

### R-2 · Bus capacity scaling  *(trivial, ~30 min)*

**File:** `crates/atelier-core/src/session.rs` (line 842) and/or `crates/atelier-cli/src/runner.rs`

**Problem:** `broadcast::channel(EVENT_BUFFER)` uses `EVENT_BUFFER = 256` unconditionally. The constant `BUS_FANOUT_FACTOR = 4` exists in `subagents.rs` but is never applied. Each `Runner` (root or child) has its own independent bus, so a depth-3 tree has 4 independent 256-slot channels — this is actually adequate for the architecture. However, the constant was defined for this purpose and the plan document promises it is applied.

**Fix:** In `session.rs` or in `Runner::new` (whichever is the canonical construction point), scale the channel:

```rust
use atelier_core::subagents::BUS_FANOUT_FACTOR;
let (events, _) = broadcast::channel(EVENT_BUFFER * BUS_FANOUT_FACTOR);
```

This is a one-line change that applies `BUS_FANOUT_FACTOR = 4` → channel size 1,024. It's strictly additive (larger buffer never breaks subscribers).

**Verify:** `cargo test -p atelier-core` (no regression; no new test needed since the constant exercise is covered by the bus-drop path in existing tests).

---

### R-3 · Cost-fields populated test (WU-6 substitute)  *(small, ~half-day)*

**Context:** The original WU-6 spec called for `TrustBudget::reconcile_subagent`. The spec (line 550) says `max_budget` is "future field — not v1". No `TrustBudget` type should be built.

**What to deliver instead:** a test that locks in the v1 cost contract — that after a sub-agent run, `SubagentCost` fields are non-zero.

**Test:** `subagent_cost_fields_populated` in `crates/atelier-cli/tests/run_integration.rs`.

Script the mock responses so the child runner makes one model call (the existing `MockResponse` already tracks `prompt_tokens` etc. via the adapter — confirm this via a read of `MockAdapter::chat`). After `runner.run()`, access `report.ledger_entries` and assert the `prompt_tokens > 0` on the `ModelCall` entry attributed to the sub-agent run. Alternatively, drive the full path and assert `session.json`'s `subagents.<id>.prompt_tokens > 0`.

**Verify:** `cargo test -p atelier-cli --test run_integration -- subagent_cost`

---

### R-4 · Session schema round-trip test  *(small, ~1 day)*

**Context:** Success criterion 3 from the original plan — proves `session.json` with a `subagents` map validates against `schemas/session/v1.json`.

**Approach:** add `tests/test_session.py::test_subagent_field_validates` to the Python rig.

```python
def test_subagent_field_validates(tmp_path, schema_registry):
    """Session JSON with populated subagents map validates against session/v1.json schema."""
    # Synthesise a minimal session JSON that exercises the subagents map.
    session = load_fixture("session_with_subagent.json")  # new fixture
    validate(session, schema_registry["session/v1.json"])  # must not raise
```

Create `tests/fixtures/session_with_subagent.json` — a session fixture with one entry in `subagents` covering all required fields (`subagent_type`, `description`, `status`, `result`, `turns_used`, `prompt_tokens`, `completion_tokens`, `cached_tokens`) and one with the optional `cost_usd` present.

**Verify:** `make rig-tests` (target `test_subagent_field_validates`).

---

### R-5 · Cancel cascade test  *(small, ~1 day)*

**Test:** `subagent_cancel_cascade` in `crates/atelier-cli/tests/run_integration.rs`.

Script a parent runner that spawns a sub-agent; the sub-agent's scripted response does NOT call `harness_meta(claimed_done=true)` — it only calls a tool with a long timeout. Before the sub-agent's turn completes, trip the parent's cancel token. Assert:
- `runner.run()` returns `Ok(report)` with `report.final_state` in `{Done, Failed, AwaitingUser}`.
- The `SubagentCancelled` event was emitted on the bus.

**Implementation note:** The sub-agent shares the parent's `CancellationToken` chain (child token via `req.parent_cancel.child_token()`), so tripping the parent token should cascade automatically. The test validates this contract.

**Verify:** `cargo test -p atelier-cli --test run_integration -- subagent_cancel_cascade`

---

### R-6 · TUI sub-agent list widget  *(medium, ~2 days)*

**File:** `crates/atelier-tui/src/lib.rs`

The TUI currently appends one-line strings to the event log for each `Subagent*` event. This satisfies basic observability. A dedicated list widget provides:
- A persistent list of active + completed sub-agents (not buried in the event log scroll).
- Status badge per row.
- Turn progress counter while running.

**State addition:**
```rust
pub struct SubagentEntry {
    pub id: String,
    pub subagent_type: String,
    pub description: String,
    pub status: String,       // "running" | "completed" | "failed" | "cancelled"
    pub turn: u32,
    pub max_turns: u32,
}
// In AppState:
pub subagents: Vec<SubagentEntry>,
```

**Render (right column, below plan pane):** a ratatui `Block` titled "Sub-agents" with a `List` widget. One row per entry: `[sa-N] <type> <status_badge> "<description>" — turn N/M`.

**Event wiring in `apply_event`:**
- `SubagentSpawned` → push new `SubagentEntry { status: "running", … }`.
- `SubagentTurnAdvanced` → find by id, update `turn`.
- `SubagentCompleted` → find by id, set `status`.
- `SubagentCancelled` → find by id, set `status: "cancelled"`.
- `SubagentToolCall` → no state change (the event-log string is enough).

**Verify by hand:** scripted Mock run via `cargo run -p atelier-tui`; sub-agent row appears, status badge flips.

---

### R-7 · GUI sub-agent panel  *(medium, ~3 days — deferred pending GUI agent mode)*

**Blocked by:** the GUI was pivoted to chat-REPL mode in v60.43–49. `start_chat_run` calls `adapter.chat(messages, &[])` with zero tools — no Runner, no dispatcher, no sub-agent spawning. Sub-agent events can't surface in the GUI until there is a Runner-backed "agent run" path in the GUI.

**Prerequisite:** a `start_agent_run` Tauri command that wires back through `Runner::run` (not the current `start_chat_run` shortcut). This is a GUI mode decision tracked separately.

**Do not implement WU-10 until the GUI has a Runner-backed mode.** Mark as `blocked` in the effort table.

---

### R-8 · Cancel-race documentation  *(trivial, ~30 min)*

**File:** `crates/atelier-core/tools/spawn_subagent.v1.json`

Append to the `description` field of the `cancel` oneOf shape:

> "Cancel cooperates at tool-call boundaries. An in-flight long-running `shell` tool inside the sub-agent will not be interrupted until it yields or times out — identical to parent-level cancel semantics."

**Verify:** `cargo clippy --workspace -- -D warnings` (the JSON file is validated at startup by `BuiltInToolWrapper`; a schema-invalid description causes a startup panic).

---

## Execution order

```
R-8  (30 min, no deps)
R-2  (30 min, no deps)
R-1  (1 day — most important correctness fix)
R-3  (0.5 day, depends on nothing)
R-4  (1 day, depends on nothing)
R-5  (1 day, depends on R-1 for cancel-token architecture understanding)
R-6  (2 days, can run in parallel with R-4/R-5)
R-7  (blocked on GUI agent-mode pivot)
```

Total for R-1 through R-6 + R-8: **~6 days single-engineer**.

---

## Deferred (not in this plan)

| Item | Reason |
|---|---|
| WU-6 `TrustBudget::reconcile_subagent` | Spec §10 line 550 explicitly says "future field — not v1" |
| WU-7 G5 MCP allowlist shadowing test | Requires a mock MCP server fixture; complex setup; deferred to a dedicated MCP-test sprint |
| WU-10 GUI sub-agent pane | Blocked on GUI having a Runner-backed agent mode (see R-7) |
| WU-12 §4 time-travel | §4 checkpoint surface doesn't exist yet; deferred to Phase D |
| WU-14 cancel-race doc | Folded into R-8 above |

---

## Effort table

| Work unit | Estimate | Status |
|---|---|---|
| R-1 resume safety | 1 day | **done v60.59** |
| R-2 bus capacity | 30 min | **done v60.59** |
| R-3 cost-fields test | 0.5 day | **done v60.59** |
| R-4 schema round-trip rig test | 1 day | **done v60.59** |
| R-5 cancel cascade test | 1 day | **done v60.59** |
| R-6 TUI sub-agent widget | 2 days | **done v60.59** |
| R-7 GUI sub-agent pane | 3 days | **done v60.60** |
| R-8 cancel-race docs | 30 min | **done v60.59** |

---

## Verification report (v60.60)

| Criterion | Command | Status |
|---|---|---|
| Rust compiles clean | `cargo build -p atelier-gui` | ✓ |
| TypeScript clean | `npm run check` (in `crates/atelier-gui/ui`) | ✓ 0 errors |
| fmt + clippy clean | `cargo fmt --check && cargo clippy -p atelier-gui -- -D warnings` | ✓ |
| Full gate | `make check` | ✓ 81/81 artifacts, 168 rig, 14 workloads |

## Verification report (v60.59)

| Criterion | Command | Status |
|---|---|---|
| 4 subagent tests green | `cargo test -p atelier-cli --test run_integration -- subagent resume` | ✓ 5 passed |
| Schema round-trip passes | `make rig-tests` | ✓ 168 passed |
| No regression | `cargo test -p atelier-tui --lib` | ✓ 111 passed |
| fmt + clippy clean | `cargo fmt --check && cargo clippy --workspace -- -D warnings` | ✓ |
| Full gate | `make check` | ✓ 81/81 artifacts, 168 rig, 14 workloads |
