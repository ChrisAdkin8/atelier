# Plan — §10 Sub-agent delegation (code-complete)

**Spec target:** `coding-harness-spec.md` §10.1 (delegation mode). §10.2 (comparison) and §10.3 (background critic) are Phase F and explicitly out of scope here.

**Status:** Core runtime code-complete as of v60.56–v60.58. Remaining: WU-6 (trust budget), WU-8 (bus events), WU-10/11/12 (UI), WU-13 partial (5/7 test scenarios), WU-14 (docs).
- Tool manifest on disk: `crates/atelier-core/tools/spawn_subagent.v1.json`
- Subagent-type schema: `schemas/config/subagent_type.v1.json`
- 3 bundled subagent types on disk: `crates/atelier-core/subagents/{researcher,test-runner,general-purpose}.json`
- Session schema `subagents` map: `schemas/session/v1.json:225`
- `Persisted::subagents: Option<serde_json::Value>` placeholder at `crates/atelier-core/src/persistence.rs:93`
- Built-in registration table comment at `crates/atelier-core/src/tools/mod.rs:86–91` explicitly notes spawn_subagent is excluded until the executor lands
- `tokio_util::sync::CancellationToken` already threaded through `Session` (`crates/atelier-core/src/session.rs:732`) and `ToolContext::cancel` (`crates/atelier-core/src/dispatcher.rs:168`)

**Acceptance gate (verbatim from spec line 568):**
> Parent invokes `spawn_subagent` with `subagent_type: researcher`; sub-agent runs to completion within its turn budget; result returns as a tool-call message to the parent; session schema's `subagents` field populates and validates; parent's verification gate (if any) runs after the sub-agent completes.

---

## Success criteria (checkable)

1. [x] `cargo test -p atelier-core subagents::` green — 7 passed (v60.56).
2. [x] `cargo test -p atelier-cli --test run_integration -- subagent` green — 2 passed (`subagent_delegation_end_to_end`, `subagent_depth_cap_surfaces_as_tool_error`) (v60.58).
3. [ ] `make rig-tests` green — `tests/test_session.py::test_subagent_field_validates` pending (rig test not yet written).
4. [x] Recursion depth 4 attempt returns `ToolError::SchemaViolation` — `subagent_depth_cap_surfaces_as_tool_error` passes (v60.58).
5. [ ] Cancel cascade: cancelling depth-1 sub-agent terminates depth-2 grandchild within 5s — pending (WU-13 scenario 4).
6. [ ] `cargo test -p atelier-core trust_budget_subagent` — pending (WU-6).
7. [x] `cargo fmt --check && cargo clippy --workspace -- -D warnings && make check` all green (v60.58).

Verification report at the end lists the exact command + tail of output for each criterion above.

---

## Work units (in execution order — each is a discrete checkpoint)

### WU-1 · Sub-agent type registry  *(done — v60.56)*

**New module:** `crates/atelier-core/src/subagents/mod.rs` (mirror `crate::skills`).

- `pub struct SubagentType { name, description, system_prompt_addendum, tool_allowlist: Option<Vec<String>>, default_max_turns: Option<u32>, model_routing: Option<RoutingConfig>, side_effect_class_cap: Option<SideEffectClass> }`
- `pub struct SubagentTypeRegistry` — `HashMap<String, SubagentType>`
- `SubagentTypeRegistry::load(repo_root: &Path) -> Result<Self, LoadError>`:
  1. Walk `include_dir!` over `crates/atelier-core/subagents/*.json` (bundled).
  2. Overlay `~/.atelier/subagents/*.json` (global).
  3. Overlay `<repo>/.atelier/subagents/*.json` (per-repo, wins).
  4. JSON-schema-validate each entry against `schemas/config/subagent_type.v1.json` via existing `jsonschema::Validator` plumbing.
- `pub const DEFAULT_MAX_TURNS: u32 = 25;` (spec §10 line 521 PROVISIONAL).
- `pub const RECURSION_DEPTH_CAP: u8 = 3;` (spec §10 line 556 PROVISIONAL).
- Tests: bundled types load; per-repo override of `researcher.tool_allowlist` wins; schema-invalid manifest surfaces a `LoadError::Schema` not a panic.

**Wire-up:** `lib.rs` adds `pub mod subagents;`. No other crate changes.

**Verify:** `cargo test -p atelier-core subagents::`.

---

### WU-2 · `SessionCore` extraction  *(SKIPPED — replaced by IoC trait approach in v60.56)*

Rather than a full `SessionCore` extraction, the implementation uses `SubagentSpawner` as a trait seam: `spawn_subagent` in `atelier-core` calls through the trait; `RunnerSpawner` in `atelier-cli` implements it by constructing a child `Runner` directly. This avoids the large refactor while satisfying the layering contract.

### WU-2 (original) · `SessionCore` extraction  *(deferred to Phase F if ever needed)*

**Why first?** Today `Runner` (`crates/atelier-cli/src/runner.rs`, 3,132 lines) owns its bus, ledger, dispatcher, conformance window, conversation, mental-model panel, and the §2.5 loop driver. A child sub-agent needs its own conversation/budget/turn-counter/cancel-token but must share the parent's dispatcher, sandbox profile, ledger, and bus subscription.

**Refactor:** extract a re-entrant primitive — `crate::SessionCore` lives in `atelier-cli/src/session_core.rs`.

```rust
pub struct SessionCore {
    id: SubagentId,                          // "root" for the top-level runner
    parent_id: Option<SubagentId>,
    depth: u8,                                // 0 for root, +1 per spawn
    conversation: Vec<Turn>,
    context_mgr: ContextManager,
    plan: PlanState,
    memory: MemoryState,
    conformance: ConformanceTracker,
    strategy: Strategy,
    turn_count: u32,
    max_turns: u32,
    cancel: CancellationToken,                // child_token() of parent's
    ledger_ref: Arc<CostLedger>,              // shared with parent
    dispatcher_ref: Arc<Dispatcher>,          // shared
    bus_tx: broadcast::Sender<Event>,         // shared
    sandbox: SandboxProfile,                  // inherited
    routing: RoutingConfig,                   // may be overridden per-subagent
    tool_allowlist: Option<HashSet<String>>,  // None = inherit
    side_effect_cap: Option<SideEffectClass>,
}

impl SessionCore {
    pub async fn drive_to_completion(self) -> SessionResult { ... }
}
```

**`Runner` becomes a thin wrapper** that owns the global state (adapter, hook set, sandbox-policy generator, file watcher, GUI swap-adapter receiver) and instantiates exactly one root `SessionCore`. The §2.5 loop body (the inner `loop { … }` from `runner.rs::run`) moves into `SessionCore::drive_to_completion`.

**Migration approach — keep diff reviewable:**
1. Inline-extract `SessionCore` from `Runner` first — both structs co-exist, `Runner` delegates one method at a time.
2. Move turn-driver state (`conversation`, `context_mgr`, `plan`, `memory`, `turn_count`, `strategy`, `conformance`) into `SessionCore`.
3. Leave adapter, hooks, file watcher, swap-adapter receiver, mental-model panel on `Runner`.
4. `cargo test -p atelier-cli` green after each commit.

**Risk:** this is the biggest single piece of work. Recommend pairing or a 2-day spike before committing.

**Verify after WU-2:** all existing `atelier-cli` tests still green; no behaviour change at the public `Runner::run` surface.

---

### WU-3 · `SubagentSpawner` trait + handle registry  *(done — v60.56)*

**Why a trait?** The `spawn_subagent` Tool impl lives in `atelier-core`, but actually instantiating a child `SessionCore` requires the adapter, hook set, and sandbox-policy machinery that lives in `atelier-cli`. Inverting via a trait keeps the layering clean.

**New file:** `crates/atelier-core/src/subagents/spawner.rs`.

```rust
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    async fn spawn(&self, req: SpawnRequest) -> Result<SpawnHandle, SpawnError>;
    async fn cancel(&self, id: &SubagentId) -> Result<(), CancelError>;
}

pub struct SpawnRequest {
    pub parent_id: SubagentId,
    pub parent_depth: u8,
    pub parent_cancel: CancellationToken,
    pub parent_budget_remaining: TrustBudget,
    pub subagent_type: SubagentType,         // resolved by tool impl from registry
    pub description: String,
    pub prompt: String,
    pub max_turns_override: Option<u32>,
    pub tool_allowlist_override: Option<Vec<String>>,
}

pub struct SpawnHandle {
    pub id: SubagentId,
    pub join: JoinHandle<SessionResult>,
}
```

**Registry of running children:** `Arc<Mutex<HashMap<SubagentId, RunningChild>>>` on the spawner impl. `RunningChild` carries `CancellationToken` + `JoinHandle`. Cancellation cascade is `parent_token.child_token()` → tokio handles propagation transparently. Verify with a depth-3 cascade test.

**Implementation in `atelier-cli`:** `RunnerSpawner` impl of `SubagentSpawner` constructs a child `SessionCore` with shared dispatcher/ledger/bus and runs it on `tokio::spawn`.

**Verify:** `cargo test -p atelier-core subagents::spawner` covers depth-cap enforcement and the running-children registry.

---

### WU-4 · `spawn_subagent` Tool impl  *(done — v60.56)*

**Resolves gotcha G6** (spawn vs cancel `oneOf`).

**New file:** `crates/atelier-core/src/tools/spawn_subagent.rs`.

```rust
pub struct SpawnSubagent {
    spawner: Arc<dyn SubagentSpawner>,
    type_registry: Arc<SubagentTypeRegistry>,
}

#[async_trait]
impl Tool for SpawnSubagent {
    fn name(&self) -> &'static str { "spawn_subagent" }
    fn side_effect_class(&self) -> SideEffectClass { SideEffectClass::LocalRisky }
    async fn execute(&self, ctx: ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        // 1. dispatch oneOf: spawn vs cancel (manifest already validates the JSON schema)
        // 2. spawn path:
        //      - look up subagent_type (default "general-purpose")
        //      - depth check: if ctx.depth >= RECURSION_DEPTH_CAP → SchemaViolation
        //      - build SpawnRequest, call spawner.spawn(), await handle.join
        //      - map SessionResult → ToolOutput { subagent_id, result, status, turns_used, cost }
        // 3. cancel path: spawner.cancel(id)
    }
}
```

**Threading `depth` into `ToolContext`** is the only `dispatcher.rs` change. Add `pub depth: u8` to `ToolContext` (default 0 for root `SessionCore`). Single-line additions everywhere `ToolContext` is constructed.

**Register the executor:** add a new row to `builtin_table()` in `crates/atelier-core/src/tools/mod.rs:92`:

```rust
(
    "spawn_subagent",
    include_str!("../../tools/spawn_subagent.v1.json"),
    Arc::new(spawn_subagent::SpawnSubagent::new(spawner, type_registry)),
),
```

The dispatcher needs the spawner + type registry up front, so promote `register_builtins` to take a `BuiltinDeps { spawner: Arc<dyn SubagentSpawner>, type_registry: Arc<SubagentTypeRegistry> }`. Remove the "excluded from builtin_table" caveat from the doc comment.

**Tests:**
- Spawn happy-path against `MockAdapter` (sub-agent does one tool call and emits `claimed_done`); parent receives `{status: "completed", result: "...", turns_used: N}`.
- Recursion depth 3 succeeds; depth 4 returns `ToolError::SchemaViolation`.
- Cancel path: spawn child → immediately invoke cancel shape → child terminates within 5s, status `cancelled`.
- `tool_allowlist` enforcement: `researcher` cannot call `write_file` even if parent could (refused at dispatcher pre-check).
- `side_effect_class_cap`: `researcher` cap `local-safe` refuses a `local-risky` tool call mid-run.
- **G6 — oneOf shape coverage:** spawn shape with `cancel: true` rejected (additional-property rule + `not` clause); cancel shape with `prompt`/`description` rejected; cancel shape with `cancel: false` rejected (const violation).

**Verify:** `cargo test -p atelier-core tools::spawn_subagent`.

---

### WU-5 · Session persistence round-trip  *(done — v60.56, partial)*

Typed `BTreeMap<String, PersistedSubagent>` landed. The full WU-5 spec (resume marks in-flight as `Cancelled`, rig test) is still pending — see deferred items.

### WU-5 (remaining deferred)

**Resolves gotcha G1** (resume + in-flight subagents).

**Change:** replace `pub subagents: Option<serde_json::Value>` at `persistence.rs:93` with a typed map:

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PersistedSubagent {
    pub subagent_type: String,
    pub description: String,
    pub status: SubagentStatus,
    pub turn_count: u32,
    pub max_turns: u32,
    pub spawned_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub conversation: Vec<PersistedTurn>,
    pub cost_summary: CostSummary,
    pub parent_id: Option<SubagentId>,
}

pub subagents: BTreeMap<SubagentId, PersistedSubagent>,
```

`SessionCore::drive_to_completion` writes its entry into the parent's persistence-snapshot on every checkpoint (already debounced — reuse existing path). On crash + resume, `Runner::with_resume` reconstitutes children with `status: cancelled` if not `completed/failed/timed_out` — sub-agent runs are *not* resumed in v1 (spec doesn't require it).

**G1 implementation specifics:**
- Add `SubagentStatus::Cancelled { reason: CancelReason::ResumeInflight }` variant.
- In `Runner::with_resume`, after loading the persisted session, walk `persisted.subagents` and mutate any non-terminal entry to `Cancelled { reason: ResumeInflight }` *before* the §2.5 loop reattaches.
- Emit `Event::SubagentCancelled` for each so GUI/TUI render the badge correctly.
- Recovery log records a `recovery::SubagentForceClosed { id, reason }` entry per `MessageRole::System` per spec §14.
- Unit test: `cargo test -p atelier-cli resume_marks_inflight_subagents_cancelled`.

**Rig test:** add `tests/test_session.py::test_subagent_field_validates` — runs a Mock-driven `researcher` sub-agent end-to-end, snapshots the session JSON, asserts schema-validate against `schemas/session/v1.json`.

**Verify:** `make rig-tests` green; `cargo test -p atelier-core persistence::subagents_roundtrip`.

---

### WU-6 · Cost ledger rollup + trust-budget inheritance  *(pending)*

**Resolves gotcha G4** (outer `local-risky` / inner `irreversible`).

`SessionCore` holds `Arc<CostLedger>` (parent's). Every sub-agent call is appended with a new field `subagent_id: Option<SubagentId>`. The `cost` block in the `spawn_subagent` tool output is a filter over the ledger for that id.

**Trust budget:** sub-agent starts with parent's remaining budget; on completion, the sub-agent's *unspent* portion returns to the parent (spec line 550). One subtle point — sub-agent calls debit the shared ledger as they happen, so the trust budget at session level is already shared; rollup is essentially bookkeeping. Add a `TrustBudget::reconcile_subagent(id)` helper for the explicit return-on-completion semantics.

**G4 — outer-class / inner-class enforcement test:** add `cargo test -p atelier-core inner_irreversible_debits_parent_budget`. A sub-agent invokes a hypothetical `irreversible`-class tool inside its body; assert that (a) the parent's `irreversible` budget counter ticks down (not the sub-agent's, because the ledger is shared), and (b) if the parent has already exhausted its `irreversible` budget the call is refused at dispatch even though the *outer* `spawn_subagent` was `local-risky`. This locks in the contract that the outer side-effect class is a routing hint, not a permission grant for inner calls.

**Verify:** `cargo test -p atelier-core trust_budget_subagent` and `cargo test -p atelier-core inner_irreversible_debits_parent_budget`.

---

### WU-7 · §7 verification-gate ordering  *(done — v60.57)*

Spec line 548: "The parent's `claimed_done` gate runs only after all spawned sub-agents have terminated."

**Change:** in `SessionCore::drive_to_completion`, when the parent emits `claimed_done: true`, before running the §7 dispatcher gate, await `spawner.wait_all_descendants(self.id)`. The spawner tracks descendants via the in-memory `HashMap<SubagentId, RunningChild>`.

**Test:** parent emits `claimed_done` while a sub-agent is still mid-loop → §7 gate fires only after sub-agent terminates → ordering verified by event sequence on the bus.

**Verify:** `cargo test -p atelier-cli claimed_done_waits_for_subagents`.

---

### WU-8 · Bus events + capacity scaling  *(pending)*

**Resolves gotcha G2** (bus capacity).

New variants on `crate::event::Event`:

```rust
SubagentSpawned { id: SubagentId, parent_id: SubagentId, subagent_type: String, description: String },
SubagentTurnAdvanced { id: SubagentId, turn: u32, max_turns: u32 },
SubagentToolCall { id: SubagentId, tool: String },
SubagentCompleted { id: SubagentId, status: SubagentStatus, turns_used: u32 },
SubagentCancelled { id: SubagentId, reason: CancelReason },
```

Update `crates/atelier-gui/src/state.ts` `castPayload` and `crates/atelier-tui/src/state.rs` projection arms. Both crates render subagents into the existing event log; full UI panes come in WU-10.

**G2 — bus capacity:**
- Today's bus capacity (find: `broadcast::channel(CAP)` in `runner.rs`) is sized for a single agent. With sub-agents, events fan out from depth-0 + depth-1 + depth-2 + depth-3 simultaneously.
- Multiply the configured capacity by `(1 + RECURSION_DEPTH_CAP)` — i.e., 4× the current value — at `Runner::new`. Make the multiplier a named constant `subagents::BUS_FANOUT_FACTOR`.
- On `broadcast::error::SendError` (slow consumer) emit a `tracing::warn!` with the dropped event's discriminant. Add a counter to the existing instrumentation surface.
- Unit test: `cargo test -p atelier-cli bus_capacity_survives_depth3_burst` — synthesise a depth-3 spawn tree with each level emitting 50 events back-to-back; assert no `SendError`.

**Verify:** `cargo test -p atelier-cli event_emission_for_subagent_lifecycle` and `bus_capacity_survives_depth3_burst`.

---

### WU-9 · Skills `/subagent` (optional convenience)  *(skip for v1)*

Out of scope — `spawn_subagent` is invoked by the model, not the user. Skills are a separate surface.

---

### WU-10 · GUI sub-agent card + pane  *(pending)*

- New Svelte component `crates/atelier-gui/src/lib/SubagentCard.svelte` — shown under the parent's conversation pane; status badge (`running`/`completed`/`failed`/`timed_out`/`cancelled`); click expands to a dedicated pane.
- New `crates/atelier-gui/src/lib/SubagentPane.svelte` — mirrors `ConversationPane.svelte` against the sub-agent's conversation.
- State slice: `subagentsById: Record<SubagentId, SubagentState>` in `state.ts`. Hydrated from bus events.
- Cancel button on the card wires to a new `cancel_subagent` Tauri command → `Runner::cancel_subagent(id)` → spawner's `cancel`.

**Verify by hand:** drive the `MockAdapter` with a scripted run that spawns a researcher; pane appears, expands, status flips, cancel button works.

---

### WU-11 · TUI sub-agent line  *(pending)*

New ratatui block in the right column listing active sub-agents (one row per: `[sa-1] researcher ▶ "audit auth path" — turn 3/25`). `Tab` expands a selected sub-agent into a full conversation view (reuse the existing conversation widget against the sub-agent's conversation).

**Verify by hand:** scripted Mock run; sub-agent row appears, flips status, `c` (cancel) terminates it.

---

### WU-12 · §4 time-travel interaction  *(pending)*

(No floating gotchas here — fully self-contained.)

Spec line 547: "A sub-agent's checkpoints chain off the parent's at the spawn-point. Rewinding past the spawn-point also rewinds the sub-agent (it disappears). Forking a checkpoint that contains an in-progress sub-agent forks both."

- Checkpoint records its `subagent_state_snapshot: BTreeMap<SubagentId, PersistedSubagent>` at write time.
- Rewinding past a `SubagentSpawned` event drops the sub-agent from session state and emits a `SubagentCancelled { reason: TimeTravelRewind }` for UI cleanup.
- Forking a checkpoint clones the snapshot.

**Verify:** `cargo test -p atelier-core time_travel_subagent_rewind` + `..._fork`.

---

### WU-13 · Tests — end-to-end Mock-driven  *(done — v60.58, partial)*

Two tests landed in `crates/atelier-cli/tests/run_integration.rs`: `subagent_delegation_end_to_end` (spec §10 line 568 acceptance gate) and `subagent_depth_cap_surfaces_as_tool_error`. Scenarios 1, 3, 5 from the original list are covered. Remaining scenarios (cancel cascade, trust-budget rollup, full session-schema round-trip, G5 allowlist vs MCP shadowing) are still pending.

### WU-13 (remaining — pending)

**Resolves gotcha G5** (allowlist vs MCP name collision).

**New integration test:** `crates/atelier-cli/tests/subagent_e2e.rs`

Scenarios driven via `MockAdapter` scripted responses:
1. Happy path: parent prompts → spawns `researcher` → sub-agent does 3 tool calls + final message → parent gets one tool-result with the final message + status `completed`.
2. Sub-agent intermediate turns absent from parent conversation (assert directly on `parent.conversation`).
3. Recursion depth 3 OK, depth 4 → `ToolError::SchemaViolation`.
4. Cancel cascade depth 3.
5. Parent `claimed_done` waits for sub-agent before §7 gate.
6. Trust-budget rollup: parent's remaining budget after sub-agent completion = (pre-spawn budget) − (sub-agent consumed).
7. Session-schema round-trip: persisted session validates against `schemas/session/v1.json`.
8. **G5 — allowlist vs MCP shadowing:** Register a mock MCP server that advertises a tool named `read_file`. Per `register_mcp_servers` policy (built-ins win on collision, MCP server marked `ServerFailure`), the MCP `read_file` is rejected. Then spawn a `researcher` whose `tool_allowlist` contains `read_file`. Assert the sub-agent invokes the *built-in* `read_file`, never the MCP version. Repeat with a non-colliding MCP tool name (`mcp_search`) NOT in `researcher`'s allowlist — assert the sub-agent's call is refused with `ToolError::NotAllowed`.

**Verify:** `cargo test -p atelier-cli subagent_e2e`.

---

### WU-14 · Cancel-race documentation  *(pending)*

**Resolves gotcha G3** (cancel re-invocation race).

The cooperative-cancel semantics are inherited from §14 — long-running shells only yield at `tokio::select!` boundaries, so `{subagent_id, cancel: true}` cannot interrupt an in-flight `shell` tool until the shell process itself terminates or hits its timeout. This is **identical** to parent-level cancel behaviour today.

Deliverables:
- Update `crates/atelier-core/tools/spawn_subagent.v1.json` `description`: append "Cancel cooperates at tool-call boundaries; an in-flight long-running shell tool will not be interrupted until it yields or times out (same semantics as parent-level cancel)."
- Add a CAVEAT block to spec §10 line 554 area (PROVISIONAL marker, since the spec text already covers the general cancel surface).
- Doc-test in `crates/atelier-core/src/subagents/spawner.rs`: scenario where cancel is invoked during a sleep-based mock-shell, asserts the documented latency (delay until next yield).

**Verify:** `cargo test --doc -p atelier-core subagents::spawner`.

---

## Effort estimate (rolled up)

| Work unit | Effort |
|---|---|
| WU-1 registry | **done v60.56** |
| WU-2 SessionCore extract | **skipped** — IoC trait approach used instead |
| WU-3 spawner trait | **done v60.56** |
| WU-4 Tool impl + register (incl. G6 oneOf coverage) | **done v60.56** |
| WU-5 persistence (incl. G1 resume cleanup) | **partial v60.56** — typed map done; resume cleanup pending |
| WU-6 ledger + budget (incl. G4 inner-class test) | pending |
| WU-7 §7 gate ordering | **done v60.57** |
| WU-8 bus events + capacity (incl. G2 fanout) | pending |
| WU-10 GUI panes | pending |
| WU-11 TUI line | pending |
| WU-12 time-travel | pending |
| WU-13 end-to-end tests (incl. G5 MCP shadowing) | **partial v60.58** — 2/7 scenarios done |
| WU-14 cancel-race documentation (G3) | pending |

Roughly **1,500–2,500 LOC** across `atelier-core` + `atelier-cli` + `atelier-gui` + `atelier-tui` + tests.

---

## Sequencing & checkpoints

1. **Land WU-1 + WU-2 in their own PR.** That's the highest-risk surface; ship it independently so a SessionCore regression doesn't tangle with sub-agent semantics.
2. **WU-3 + WU-4 + WU-5 in one PR.** This is the smallest set that turns spawn_subagent on end-to-end; gate it behind the §10 acceptance test (WU-13.1 + WU-13.2 + WU-13.7).
3. **WU-6 + WU-7 + WU-8 in one PR.** Crosses the §7/§8 pillar boundaries; needs to land together to keep semantics coherent.
4. **WU-10 + WU-11 in one PR.** Pure UI; reviewable independently against a feature-complete backend.
5. **WU-12 last.** Time-travel touches `§4`, which is its own pillar — easier to validate against a known-good sub-agent lifecycle.

Each PR independently runs `cargo fmt --check && cargo clippy -- -D warnings && cargo test && make check`.

---

## Gotcha → WU traceability matrix

Every gotcha is bound to a work unit and a verifiable test. None are left as "document at review time" floaters.

| ID | Gotcha | Owning WU | Verification |
|---|---|---|---|
| G1 | `Runner::with_resume` and in-flight sub-agents | WU-5 | `cargo test -p atelier-cli resume_marks_inflight_subagents_cancelled` |
| G2 | Broadcast bus capacity under depth-3 fanout | WU-8 | `cargo test -p atelier-cli bus_capacity_survives_depth3_burst` |
| G3 | Cancel re-invocation race (cooperative-cancel boundary) | WU-14 | `cargo test --doc -p atelier-core subagents::spawner` + manifest + spec PROVISIONAL note |
| G4 | Outer `local-risky` / inner `irreversible` budget routing | WU-6 | `cargo test -p atelier-core inner_irreversible_debits_parent_budget` |
| G5 | `tool_allowlist` vs MCP-routed name collision | WU-13 | `cargo test -p atelier-cli subagent_e2e::mcp_shadowing` |
| G6 | spawn vs cancel `oneOf` shape coverage | WU-4 | `cargo test -p atelier-core tools::spawn_subagent::oneof_shapes` |

**Self-check:** the original 6-item gotcha list has been folded into the work units above. There is no "appendix of regrets" left for review-time — each gotcha now has an owner, a test, and a one-line entry in this matrix that points to both.

---

## Verification report (v60.56–v60.58)

```
[✓] cargo test -p atelier-core -- subagents::         (7 tests, 0 failures)
[✓] cargo test -p atelier-core -- spawn_subagent       (5 tests, 0 failures)
[✓] cargo test -p atelier-cli --test run_integration -- subagent
                                                       (2 tests, 0 failures)
[✓] cargo fmt --check && cargo clippy --workspace -- -D warnings  (clean)
[ ] make rig-tests (test_subagent_field_validates)     (not yet written)
[ ] cargo test -p atelier-core trust_budget_subagent   (WU-6 pending)
```

## Verification report template (remaining WUs)

End each remaining WU with a row in this table:

```
[✓] cargo test -p atelier-core subagents::          (N tests, 0 failures)
[✓] cargo test -p atelier-cli subagent_e2e          (7 tests, 0 failures)
[✓] make rig-tests                                    (rig: pass; test_subagent_field_validates: pass)
[✓] cargo fmt --check && cargo clippy -- -D warnings  (clean)
[✓] make check                                        (schemas + artifacts + rig + dry-run: pass)
```

No "I think it's done" — only commands + their output.
