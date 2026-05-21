# Plan — near-term harness performance track v60.73+

Date: 2026-05-20. Source: repo performance scope review. This plan prioritises the near-term performance track: first reduce UI event batching / reducer churn and duplicate per-turn snapshots, then tackle incremental session persistence as the larger architectural improvement.

The work is ordered to land low-risk, user-visible latency wins before changing the persistence model. Items are labelled **PERF01–PERF12** for commit-message traceability.

Implementation note: Track C uses **Option B** — schema-valid `session.json` manifest plus `conversation.jsonl` / `ledger.jsonl` sidecars, with snapshot-only session fallback on load. v60.75 completes the focused Track C hardening pass with `resume_index.json` cursoring, indexed resume-prefix loading, sidecar compaction, and compatibility tests.

---

## Standing gates

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p atelier-core` when core/runtime files change
- `cargo test -p atelier-cli` when `Runner` changes
- `cargo test -p atelier-gui` and frontend checks when GUI bridge / Svelte state changes
- `cargo test -p atelier-tui` when TUI state/render paths change
- `make check` if schemas, fixtures, or persisted artifacts change

Performance work must preserve existing safety semantics: event ordering, approval gating, crash recovery, and §14 resume behaviour take priority over raw throughput.

---

## Goals

1. Reduce bursty GUI/TUI update cost during streaming, sub-agent fan-out, and high-event runs.
2. Remove duplicate per-turn context / memory / token snapshot work in `Runner::run`.
3. Make long-session save/resume scale better without weakening atomicity or power-loss durability.
4. Add targeted regression tests so future event or persistence changes do not reintroduce full-state churn.

## Non-goals

- No provider protocol changes.
- No schema rewrite unless incremental persistence requires a versioned additive shape.
- No removal of `fsync`/atomic-write safety in the name of speed.
- No UI optimistic updates that can diverge permanently from backend truth.

---

## Track A — UI event batching + reducer churn (PERF01–PERF05)

**Why first:** this is the quickest user-visible win. The GUI currently reducer-applies each bus event one-by-one and clones bounded arrays on every event. The TUI redraw path also does allocation-heavy projections per event.

### PERF01 — Baseline event/update hot paths

**Files:**
- `crates/atelier-gui/src/lib.rs`
- `crates/atelier-gui/ui/src/lib/state.ts`
- `crates/atelier-gui/ui/src/lib/components/EventLogPane.svelte`
- `crates/atelier-gui/ui/src/lib/components/ConversationPane.svelte`
- `crates/atelier-tui/src/lib.rs`

**Steps:**
1. Add lightweight test-only or dev-only counters around GUI event bridge emission and reducer application.
2. Capture a scripted high-event run shape: streamed assistant deltas, multiple `SubagentTurnAdvanced`, `ContextItems`, and `MemoryCards`.
3. Record the baseline in the PR description / changelog entry, not as a committed benchmark artifact unless a stable harness already exists.

**Success criteria:**
1. Hot event classes are identified before changing behaviour.
2. No production telemetry or persistent user data is added.

### PERF02 — Coalesce low-value GUI bridge events

**Files:**
- `crates/atelier-gui/src/lib.rs`
- `crates/atelier-gui/ui/src/lib/state.ts`

**Candidate event classes:**
- `AssistantTextDelta`
- `SubagentTurnAdvanced`
- repeated `ContextSnapshot`
- repeated event-log-only variants that do not gate user decisions

**Contract:**
- Do not batch across semantic boundary events: `StagingPendingApproval`, `CommitDecision`, `FilesChanged`, `AdapterSwapPending`, `VerificationFailed`, `RunFinished`, and cancellation events must stay ordered and prompt.
- Batching window should be short enough to preserve perceived streaming responsiveness, e.g. frame-sized or low tens of milliseconds.
- The reducer must accept both single events and a batch wrapper, or batching must happen entirely inside the bridge without changing the wire shape.

**Success criteria:**
1. Streaming text still appears live.
2. Approval / concurrent-edit / swap modals still open immediately.
3. A burst of sub-agent progress no longer causes one IPC + full reducer pass per turn tick.

### PERF03 — Reduce reducer whole-array churn

**Files:**
- `crates/atelier-gui/ui/src/lib/state.ts`
- `crates/atelier-gui/ui/src/lib/components/EventLogPane.svelte`
- `crates/atelier-gui/ui/src/lib/components/SubagentPane.svelte`

**Steps:**
1. Keep event log in newest-first order or render from the tail without `[...events].reverse()` on every invalidation.
2. Replace sub-agent `map(...)` updates with a keyed update helper or indexed state projection.
3. Keep bounded arrays without repeated copy+slice where possible.

**Success criteria:**
1. `MAX_EVENT_LOG` and `MAX_CONVERSATION_LINES` caps still hold.
2. Event order rendered to the user is unchanged.
3. Sub-agent rows update by id without rebuilding every row for each progress event.

### PERF04 — Throttle conversation auto-scroll

**Files:**
- `crates/atelier-gui/ui/src/lib/components/ConversationPane.svelte`

**Contract:**
- Auto-scroll only when the user is already near the bottom.
- Use `requestAnimationFrame` or equivalent to avoid layout thrash during token streaming.
- If the user scrolls up, streaming must not yank them back to the bottom.

**Success criteria:**
1. Streaming remains readable at bottom.
2. Manual scroll position is respected.
3. No accessibility regression in keyboard-only use.

### PERF05 — TUI bounded queues and render allocation pass

**Files:**
- `crates/atelier-tui/src/lib.rs`

**Steps:**
1. Convert event-log storage paths that prune with `remove(0)` to `VecDeque`.
2. Avoid formatting/rendering more rows than the visible pane can display where practical.
3. Keep existing pure render tests; add targeted tests for queue bounds and newest/oldest ordering.

**Success criteria:**
1. `cargo test -p atelier-tui` passes.
2. TUI event log and conversation panes render the same visible order as before.

---

## Track B — Duplicate per-turn snapshots (PERF06–PERF08)

**Why second:** this reduces steady-state runner overhead without changing persisted data. It is lower architectural risk than Track C and improves both GUI and TUI because fewer large snapshots hit the bus.

### PERF06 — Collapse duplicate context summarisation

**Files:**
- `crates/atelier-cli/src/runner.rs`
- `crates/atelier-core/src/context.rs`

**Current hotspot:** overflow handling and turn-boundary emission call `ContextManager::summarise()` multiple times and re-sum token counts from summaries.

**Steps:**
1. Add a single snapshot helper that returns summaries plus token totals from one lock acquisition.
2. Use that helper in overflow target selection and turn-boundary `ContextItems` / `ContextSnapshot` emission.
3. Preserve the existing distinction between adapter `count_tokens(&messages)` and per-item approximate token attribution.

**Success criteria:**
1. No double-lock/double-summarise path remains in `Runner::run` for the same turn boundary.
2. Context meter and context pane still agree after each turn.

### PERF07 — Emit state snapshots only when changed

**Files:**
- `crates/atelier-cli/src/runner.rs`
- `crates/atelier-core/src/context.rs`
- `crates/atelier-core/src/memory.rs`
- `crates/atelier-core/src/plan.rs`

**Contract:**
- Initial snapshots still emit so late-joining UI subscribers converge.
- At turn boundaries, `ContextItems`, `MemoryCards`, and `PlanSnapshot` should skip re-emission if the projection is byte-for-byte or version-identical to the last emitted state.
- Mutator paths still emit immediately when the user changes context, memory, or plan state.

**Success criteria:**
1. A no-op assistant turn does not rebroadcast identical context/memory/plan snapshots.
2. Pin / unpin / evict / memory add-delete / plan status changes still surface immediately.
3. GUI and TUI remain convergent after joining mid-run.

### PERF08 — Token count cache discipline

**Files:**
- `crates/atelier-cli/src/runner.rs`
- `crates/atelier-core/src/adapter/mod.rs`

**Steps:**
1. Cache the last `adapter.count_tokens(&messages)` result by message-history fingerprint within a run.
2. Invalidate on any message append, compaction, expansion, or mental-model injection change.
3. Keep `TokenSource` honest: cached `Approx` remains `Approx`; do not relabel as `Exact`.

**Success criteria:**
1. Repeated turn-boundary UI emissions do not re-count unchanged message history.
2. Context overflow decisions still use fresh counts after compaction / expansion.

---

## Track C — Incremental session persistence (PERF09–PERF12)

**Why last:** this is the largest win for long sessions and resume latency, but it touches §14 crash recovery. Land it only after Tracks A/B are green so regressions are isolated.

### PERF09 — Persistence design spike

**Files:**
- `crates/atelier-core/src/persistence.rs`
- `schemas/session/v1.json`
- `coding-harness-spec.md` if a spec clarification is needed

**Decision to make before implementation:**

| Option | Shape | Pros | Cons |
|---|---|---|---|
| A | Keep `session.json`, add sidecar append log for conversation / ledger | Additive, easy rollback | Compaction/checkpoint logic needed |
| B | Split `session.json` into manifest + append-only `conversation.jsonl` / `ledger.jsonl` | Good long-session scaling | More files and migration paths |
| C | Keep full snapshot but write less often | Smallest change | Does not solve end-of-run large save or resume scan |

**Recommended default:** Option B if the schema can remain additive; Option A if compatibility pressure is high.

**Success criteria:**
1. Decision recorded at top of this file or in `tasks/todo.md` before code lands.
2. Crash-recovery invariants are explicitly listed: atomic append, recover last complete row, preserve resume prefix, no partial row accepted as complete.

### PERF10 — Add append-only conversation / ledger writer

**Status:** Partially landed in v60.73/v60.75 as split sidecar persistence. Completed turn and ledger rows are stored outside the hot manifest, but true per-turn append during an active run remains a future optimisation; end-of-run writes still materialise the current in-memory session snapshot before splitting it into sidecars.

**Files:**
- `crates/atelier-core/src/persistence.rs`
- `crates/atelier-cli/src/runner.rs`

**Contract:**
- Completed conversation turns are appended as they complete, not only materialised at end-of-run.
- Ledger entries can be appended incrementally or checkpointed with a monotonic cursor.
- Writes use the existing atomic / fsync discipline or an equally durable append protocol.
- Partial writes after kill-9 are ignored or recovered into `recovery_log`, never treated as completed conversation.

**Success criteria:**
1. Kill-9 recovery tests still pass.
2. Resume sees the last completed tool call and no partial assistant/tool output.
3. End-of-run save no longer serializes the entire conversation history just to persist new turns.

### PERF11 — Resume cursor / index

**Status:** Done in v60.75 via `resume_index.json`. New split sessions record the last quiescent conversation row count and ledger row count; `OnDiskSession::resume_conversation_prefix_from_dir` reads only the indexed conversation prefix from `conversation.jsonl`, while snapshot-only sessions still resume from the manifest array.

**Files:**
- `crates/atelier-core/src/persistence.rs`
- `crates/atelier-cli/src/runner.rs`

**Steps:**
1. Persist a resume cursor for the last fully completed turn / tool call.
2. Make `resume_conversation_prefix()` read only up to that cursor without reparsing unrelated session sections.
3. Keep a fallback path for old `session.json` snapshots so existing sessions still resume.

**Success criteria:**
1. Existing session fixtures load.
2. New incremental sessions resume without scanning the full persisted document.
3. Version mismatch / migration errors remain explicit, not silent.

### PERF12 — Snapshot compaction and compatibility tests

**Status:** Done for the sidecar-backed shape in v60.75. `OnDiskSession::compact_split_sidecars` rewrites complete JSONL rows, drops an incomplete trailing row, and refreshes the resume cursor; tests cover sidecar hydration, partial trailing rows, indexed resume, and compaction cursor refresh. The session schema did not change.

**Files:**
- `crates/atelier-core/src/persistence.rs`
- `crates/atelier-cli/tests/`
- `schemas/session/v1.json` if changed

**Steps:**
1. Add a compaction path that can fold append logs into a fresh snapshot when logs exceed a threshold.
2. Add tests for:
   - old snapshot-only session resume,
   - new incremental session resume,
   - partial append row ignored,
   - compaction preserves resume prefix and ledger totals.

**Success criteria:**
1. Long sessions do not grow unbounded append files without a compaction story.
2. `make check` passes if schemas/artifacts change.
3. Crash-recovery semantics match pre-change behaviour.

---

## Suggested landing order

1. **PERF01** baseline.
2. **PERF03 + PERF04 + PERF05** reducer/render quick wins.
3. **PERF02** batching once tests protect event ordering.
4. **PERF06 + PERF07 + PERF08** runner snapshot reductions.
5. **PERF09** persistence design decision.
6. **PERF10 + PERF11** incremental writer and resume cursor.
7. **PERF12** compaction / compatibility hardening.

Tracks A and B can be developed in parallel after PERF01. Track C should wait until A/B are merged to avoid mixing user-visible UI churn with durability changes.

---

## Done definition for the full track

1. High-event GUI runs show fewer bridge emissions / reducer applications for equivalent user-visible output.
2. `Runner::run` no longer emits duplicate unchanged context / memory / plan snapshots at normal turn boundaries.
3. Long-session persistence avoids full-session rewrite for each newly completed turn while preserving atomic crash recovery.
4. Existing §14 resume and kill-9 tests remain green.
5. All standing gates pass.
