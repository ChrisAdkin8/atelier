---
name: project-subagent-hang-investigation
description: Investigation notes for "two-subagent task hangs" against the local-model harness (2026-05-20). Captures the spawn flow, concurrency model, and likely root cause(s).
metadata:
  type: project
verified: 2026-05-20
---

User report: when the running atelier harness is asked to spawn TWO sub-agents in one turn, the prompt hangs.

**Why:** the two `spawn_subagent` tool calls fan out via `futures::future::join_all`
in `runner.rs:2167`. Each branch awaits a child `Runner::run` that itself
calls `adapter.chat()` against the **same** `Arc<dyn Adapter>` as the parent
(spec §10 — sub-agents share the model). Against a single-stream local
LLM (mlx-lm, llama.cpp, Ollama loaded with one slot), the two concurrent
HTTP requests queue at the server. With `DEFAULT_MAX_TURNS = 25` per
sub-agent and Qwen-class throughput ≈10–30s/turn, the parent's `join_all`
can sit for >20 minutes before resolving — looks like a hang from the UI.

**How to apply:**
- Local servers that do not multiplex inference (mlx-lm.server, llama-server with `--parallel 1`, `ollama` without `OLLAMA_NUM_PARALLEL>1`) cannot serve two child runners in parallel; they serialise and double the wall-clock cost. Recommend either (a) raising the server's parallelism, (b) capping `max_turns` per sub-agent in the registry's `default_max_turns`, or (c) serialising the dispatch (revert §10 R-2 `join_all` to a sequential `for`).
- The §1 per-task routing knob (`[routing].executor`) lets the EXECUTOR turns of each child run against a smaller faster model — but tool-result turns only fire after the planner emits the call, so this doesn't help when the spawn_subagent calls are emitted by the **parent**. Useful only if the children themselves spawn more children.
- The spawner's `in_flight` map is misused: lines 253–262 of `subagent_spawner.rs` insert into the map and then immediately remove on the very next line **before** awaiting the handle. `cancel()` cannot find the entry once `await handle` begins. Pre-existing bug, separate from the hang but worth fixing.

**Code paths verified:**
- Parent dispatch fan-out: `crates/atelier-cli/src/runner.rs:2147-2173` (`futures::future::join_all` over `real_tool_calls`).
- Spawn path: `crates/atelier-cli/src/subagent_spawner.rs:130-296` (`RunnerSpawner::spawn` → `tokio::spawn(child_runner.run(prompt))` → `.await`).
- Shared adapter: `subagent_spawner.rs:206` (`with_adapter(self.adapter.clone())`).
- Children do NOT inherit `executor_adapter` — they always use the primary (`subagent_spawner.rs:205-217` omits `with_executor_adapter`).
- Approval gate is NOT a factor — `spawn_subagent` returns `staged_writes: None` so the `PendingApprovalGate` is bypassed (`tools/spawn_subagent.rs:163-166`).
- Broadcast bus is NOT a factor — `EVENT_BUFFER = 256 * BUS_FANOUT_FACTOR(4) = 1024`; `try_emit` is non-blocking (`session.rs:83-107`).
- Sink drain has a 5-second timeout at shutdown (`runner.rs:2677`).
- Child runners run with `ProbePolicy::Skip` — no extra probe calls (`subagent_spawner.rs:207`).

**Local environment observed (2026-05-20):**
- Ollama 0.20.0 running on `:11434`, no `OLLAMA_NUM_PARALLEL` env var → defaults to auto (1 or 4 depending on memory headroom).
- mlx-lm on `:8080` is NOT running (curl returns empty). The `[routing].executor = "qwen3-4b-dwq"` profile points at that port, so executor-routed turns (parent's tool-result turns after `spawn_subagent` returns) would fail. That fires AFTER children complete, not during, so it can't cause the hang itself — but it's a latent bug for any task that uses tool calls and tool-result follow-ups.
- Default profile is `qwen27b` → Ollama-backed `qwen3.5:27b` (~17 GB Q4_K_M). Two parallel slots would need ~34 GB unified memory; Ollama silently degrades to 1 slot when short.

**Recommended fixes (priority order):**
1. **Bound the wall-clock**: lower `DEFAULT_MAX_TURNS` for sub-agents from 25 to ~10 in `subagents.rs:41`, or per-type cap via `default_max_turns` in the registry. A two-sub-agent fan-out on a local 27B will otherwise sit for 20+ minutes worst case.
2. **Pre-flight the executor adapter**: have `resolve_executor_adapter` probe `/v1/models` and warn (or refuse to wire) when the executor profile is unreachable. Today it silently builds an adapter that will fail later.
3. **Surface progress in the GUI**: the parent emits `SubagentTurnAdvanced` for each child turn; ensure the Composer's spinner reflects that traffic instead of looking idle.
4. **Fix the spawner's `in_flight` insert/immediate-remove anti-pattern** (`subagent_spawner.rs:253-262`): the entry is removed before `.await`, so `cancel()` cannot find a running child. Either keep the entry until after the await (handle aborts on cancel via the cancel token) or drop the bookkeeping entirely.

Links: [[project_gui_dev_launch]] (run the GUI in dev mode to reproduce against a real local server).
