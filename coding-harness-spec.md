# Atelier — Coding Harness Spec

**Purpose.** Atelier is a coding harness built around three convictions: the AI is a collaborator sharing a workspace, not a chat box with side effects; bring-your-own-model is the architectural starting point; "done" is a property the harness verifies, not a claim the model makes.

**How to use.** Each pillar is an independently shippable milestone with a *machine-checkable gate* and, where relevant, a *separate UX target*. Mechanical gates block ship. UX targets are measured but non-blocking.

**Numeric parameters marked PROVISIONAL** carry a guess value pending calibration; each names the calibration method. The calibration data source for every PROVISIONAL value is the canonical workload at `tests/workload/canonical/` — that workload is the single blocking artifact for setting these numbers.

**Schemas** live in `schemas/` and are referenced inline. The central artifact, the session, is schematized at `schemas/session/v1.json` — every other schema (envelope, ledger, audit, telemetry) is either embedded in or referenced from the session. **Change history** lives in `CHANGELOG.md`.

**Implementation.** Atelier is written in Rust. Three crates: `atelier-core` (agent loop, BYOM adapters, MCP client, session state, checkpoints, ledger — no UI dependencies); `atelier-gui` (Tauri shell consuming `atelier-core` over an event channel); `atelier-tui` (`ratatui` frontend consuming the same crate the same way). The agent loop and crate organization are defined in §2.5; tool transport — MCP-first — is defined in §15.

## Contents

- §0 Mission
- §1 Bring-Your-Own-Model — adapter trait, capability matrix, cost ledger
- §2 Model Protocol — the structured envelope; prompting strategy; three emission strategies
- §2.5 Agent loop — single-turn streaming state machine; tokio async; cancel via drop; crate organization
- §3 Workspace — multi-pane layout; GUI / TUI parity decision
- §4 Time travel — diff-based checkpoints; cache economics; replay
- §5 Visible context, memory, plan
- §6 Steerability — cancel-and-restart honesty
- §7 Verification gates — did-it-do-what-it-said; tiered hallucination detector; auto-scaffolding
- §8 Trust budgets — N units, K-threshold learning, baseline measurement
- §9 Uncertainty UI
- §10 Multi-agent (deferred)
- §11 Security & sandboxing
- §12 Privacy
- §13 Telemetry
- §14 Persistence & recovery
- §15 Extensibility — MCP-first tool transport; hooks; provider plug-ins
- Cross-cutting requirements + performance budgets
- Hard tradeoff decisions (traceability summary)
- Phased build plan — Phases A → F; A+B internal, A+B+C first user-facing
- Definition of done

Load-bearing pillars for first read: §1, §2, §7. Everything else is in service of those.

---

## 0. Mission

Atelier is a coding harness where the user can always see what the agent is doing, why, what it believes, and how to redirect it — without waiting for a turn to end.

### Convictions, ranked
1. Without BYOM (§1), nothing is portable.
2. Without the Model Protocol (§2), Pillars 7 and 9 are unbuildable.
3. Without Verification Gates (§7), there is no trust differentiator.
4. Everything else is sequenced by dependency.

### Non-goals
- Not an IDE replacement.
- Not a model trainer.
- Not a hosted SaaS. Local-first; optional sync later.
- No vendor lock-in. Any chat-completions-shaped API works subject to §1's degradation rules.

---

## 1. Bring-Your-Own-Model

### Adapter trait
- `chat(messages, tools?) -> response`
- `stream(messages, tools?) -> AsyncIterator[chunk]`
- `count_tokens(messages) -> { count, source: "exact" | "approx" | "unavailable" }`
- `capabilities() -> Capabilities`
- `conformance() -> ConformanceStats` — running per-adapter stats kept in a bounded ring buffer of the last 100 calls, in-memory only, zero persistence cost.

`count_tokens` is best-effort by contract. Source is surfaced in the cost ledger. `unavailable` falls back to character/4 with one warning per session.

### Capability matrix
| Capability | Required for | If absent | If claimed-but-broken |
|---|---|---|---|
| Native tool use | §3, §6, §7 | Emulate via JSON-mode | After ≥3 malformed calls in 20-call window (PROVISIONAL — calibration: false-positive rate <5% on the canonical workload), auto-degrade to JSON-mode |
| Streaming | §3, §6 | Block on full response | n/a |
| Vision | Screenshot diffs | Disable feature | n/a |
| Prompt cache | Ledger accuracy, §4 fork preview | Uncached pricing | Cache-invalidation events ledgered |
| Structured output | §2 native / JSON-mode | Regex-prose | Repeated schema violation → drop to regex-prose for remaining session |
| ≥128k context | Large refactors | Compact (§5), reroute, or `ContextOverflowError`. Never silent truncation. | n/a |

### Per-task routing
Role-based routing is config-driven per `schemas/config/routing.v1.json`. **`executor` is the only required role** (the catch-all that runs the §2.5 loop and acts as the fallback for any plan step without a role tag). **`planner` and `critic` are well-known optional roles** with specific UI semantics: planner emits `plan_update` at task start; critic runs in the §10 side pane. **Any additional role name is free-form** — common examples: `documenter`, `web_trawler`, `architect`, `reviewer`. The dispatcher routes a turn to a custom role when the active plan step (§5 `PlanStep`) carries a matching `role` tag in its `constraints`. Per-repo `<repo>/.atelier/routing.json` overrides global `~/.atelier/routing.json`. When absent, all roles fall back to the executor. Handoff between roles is via conversation transcript + Model Protocol envelope (§2), not raw token streams.

This is the lever for cost-aware multi-model workflows: e.g., `documenter: ollama:llama3.2:3b` for low-cost documentation and web-trawling tasks, `architect: anthropic:claude-opus-4-7` for deep design reasoning and code review. See `examples/config/routing_multimodel.v1.json` for a worked example.

### Cost ledger
Every call records: prompt / completion / cached tokens, latency, model ID, `$` cost. Local cost is latency-weighted: `wall_clock_seconds × local_rate`. **`local_rate` defaults to `$0.00028/sec`** (PROVISIONAL — derived as a cloud A100 hourly rate / 3600; calibration: survey actual user hardware costs once §13 telemetry yields usage data, then default to the median override). User can override to `$0` or any other value. Surfacing local cost in the ledger lets per-task routing make rational planner/executor choices.

### Context-window asymmetry
When a routed model's window < active context: compact (§5), reroute, or typed error. Silent truncation forbidden.

`ContextOverflowError` UX: when the typed error fires, the §2.5 state machine cancels the in-flight dispatch and transitions to `AwaitingUser` with a modal offering three named options:

1. **Compact** *(default)*. Runs §5's non-destructive compaction against stale conversation regions; estimated token savings shown. On compaction success, retries the turn automatically.
2. **Reroute to larger-window model.** Lists routing-config alternatives with sufficient context (per `schemas/config/routing.v1.json`); user picks one for this turn. Fails clearly with a typed error if none configured.
3. **Cancel turn.** Returns to `AwaitingUser`; user can manually evict context-panel items (§5), edit the plan canvas, or rewrite the request.

Headless mode (`--non-interactive`, §14): defaults to **Compact**; if compaction fails to fit, falls through to **Cancel turn** and the harness exits non-zero.

The overflow event is recorded in the cost ledger as a `cache_bust` entry with `note: "ContextOverflowError -> <chosen action>"` (per §14 on-disk session storage).

### Acceptance gates

**Mechanical:** canonical workload runs against Anthropic Sonnet, OpenAI GPT-4-class, local Qwen via Ollama, and a LiteLLM-shaped mock. All four complete. Ledger entries non-null with declared source. Capability matrix populated; "absent" and "claimed-but-broken" paths each exercised by a deliberately-broken mock.

**UX target:** mid-session provider swap preserves work; user is informed of capability differences.

---

## 2. Model Protocol

### Envelope
Schema: `schemas/model_protocol/envelope.v1.json`. Fields: `claimed_changes`, `claimed_done`, `uncertainty`, `plan_update`, `grounding`, `constraints_acknowledged`. All optional except `claimed_changes` and `grounding`, which are required when the turn made edits or factual claims.

### Prompting strategy
A canonical system-prompt fragment primes the model to emit envelopes. Three few-shot examples ship at `prompts/protocol_fewshot/`: minimal edit, edit with raised uncertainty, completion with full claim. Per-adapter overrides allowed via a documented hook — different models respond differently to few-shot priming, and the spec does not pretend one example set fits all.

### Emission strategies, auto-selected
1. **Native tool call** (`harness_meta` tool). Cleanest.
2. **JSON-mode side channel** — sentinel-bracketed `<<<harness_meta>>>{...}<<<end>>>`. Per-adapter overhead in `tests/protocol/overhead.json`, refreshed by a nightly CI job (`ci/nightly/protocol_overhead.yml`); overhead drift >10% over 7 days fires a regression alert.
3. **Regex-prose** — tagged sections. Lossy; UI badges degrade to gray.

### Conformance enforcement
Malformed envelope → re-prompt with validation error inline. After 3 consecutive failures in a turn, downshift strategy and re-run the turn. Persistent failure surfaces a model-quality warning.

### Degradation policy
Every UI consumer of a protocol field defines its absent-field rendering. Default: visible "unknown" state. Never silently substitute "everything OK."

### Acceptance gates

**Mechanical:** three mock models (native / JSON-mode / prose-only) drive a fixture task; harness extracts each envelope and renders the matching UI state.

**Real-model conformance:** Anthropic Sonnet and OpenAI GPT-4-class run the canonical workload. Envelope conformance after the re-prompt loop must be **≥95%** (PROVISIONAL — calibration: pick the lowest threshold above which the lying-agent and hallucinating-agent gates in §7 still reliably trip).

---

## 2.5 Agent loop

**Architecture.** Atelier runs a **single-turn streaming loop**. A turn is one model invocation that streams text, tool calls, and envelope fields concurrently. The harness dispatches tool calls (with bounded in-turn parallelism), records their results, and either loops back into another turn or transitions to verification when `claimed_done: true`. No ReAct scratchpad, no mandatory plan-then-execute, no per-turn self-critique.

### Rejected alternatives
| Alternative | Why not |
|---|---|
| ReAct (explicit scratchpad) | Modern models reason implicitly; forcing `Thought:` emission burns tokens for no quality gain. The Model Protocol envelope already captures the structured signals worth surfacing. |
| Mandatory plan-then-execute | Slow for simple tasks (e.g., t01 doesn't need a plan). The envelope's `plan_update` lets the agent opt in. |
| Reflexion / self-critique every turn | 2–3× cost. The §10 background critic is the right home for this and is opt-in. |
| Tree-of-Thoughts | Too expensive; §4 fork-and-cherry-pick gives users the same exploration capability under their control. |
| Hierarchical planner-executor-critic loop | This is a routing decision (§1), orthogonal to the loop. Three models can run sequentially against the same loop; you don't need a different loop. |

### State machine

Named states: `Idle`, `Streaming`, `ToolDispatching`, `ToolExecuting`, `Verifying`, `AwaitingUser`, `Failed`, `Done`. Transitions:

```
Idle → Streaming → ToolDispatching → ToolExecuting → Streaming (loop)
                                                   → Verifying → Verified | Streaming (retry) | Failed
                ↘ AwaitingUser → Streaming (resume)
```

Every transition writes a checkpoint (§4) and a ledger entry (§1). **Time travel = rewind N transitions. Cancel = drop the current state.**

### Concurrency
- **tokio multi-threaded runtime.**
- **Per-session actor:** session state lives behind a `tokio::sync::Mutex` or a dedicated task with an `mpsc` inbox. UI consumers subscribe to a broadcast channel; no shared mutable state crosses the FFI boundary.
- **Bounded in-turn tool parallelism:** when the model emits N parallel tool calls (Anthropic parallel tool use / OpenAI parallel function calls), dispatch concurrently with a `Semaphore` capped at **4** (PROVISIONAL — calibration: contention on the reference machine running the canonical workload).

### Cancellation
Relies on Rust drop semantics — no invented cancel protocol. When the user cancels:
1. The active model stream is dropped (tokio aborts the stream).
2. Any tool-call `JoinHandle`s in `ToolExecuting` are dropped.
3. Partial output is preserved in the `recovery_log` slot (§14).
4. The session transitions to `AwaitingUser`.

This is the implementation backing §6's "cancel-and-restart" framing.

### Verification integration
`claimed_done: true` in the envelope triggers the transition `Streaming → Verifying`. The Verifying state spawns the configured DoD (§7) as a child process. Outcomes:
- All green → `Verified` (UI flips `claimed` → `verified`).
- Any red → back to `Streaming` with the failure output injected as a tool result.

The harness owns the transition. The agent cannot bypass it.

### Planning and critic — orthogonal
- **Planning** is opt-in. The agent emits `plan_update` in the envelope when it wants to contribute to the plan canvas (§5). The harness never gates on it. A constraint pin can require planning behaviorally (e.g., "tasks touching ≥3 files emit `plan_update` first").
- **Critic** is §10's background critic — a separate model invocation against each turn's output, surfaced in a side pane. Same single-turn loop; different routing target.

### Crate organization
- **`atelier-core`** — agent loop, Model Protocol implementation, BYOM adapters, session state, checkpoints, cost ledger. **No UI dependencies.**
- **`atelier-gui`** — Tauri shell consuming `atelier-core` via a broadcast channel.
- **`atelier-tui`** — `ratatui` frontend consuming the same crate the same way.

This separation is what makes §3's GUI/TUI parity actually shippable. Embedding loop logic in either UI is the single most likely architectural failure mode and is explicitly prohibited.

### Crate choices (in-tree)
| Concern | Crate |
|---|---|
| Async runtime | `tokio` |
| Cancellation primitive | `tokio-util` (`CancellationToken`) |
| Adapter trait | `async-trait` |
| JSON + schema | `serde_json` + `jsonschema` |
| Streaming compose | `futures` |
| HTTP (remote adapters) | `reqwest` (rustls-tls) |
| MCP client (§15) | `rmcp` (maturity-assess during scaffold; fall back to wire-protocol if blocked) |
| LSP client (§7) | `tower-lsp` |
| Sandbox (§11) | shell out to `sandbox-exec` / `bubblewrap` via `tokio::process` |
| GUI shell (§3) | **Tauri 2.x** |
| TUI (§3) | `ratatui` + `crossterm` |
| File watcher (§14) | `notify` |
| Atomic diff staging (§3) | `tempfile` |
| Credential storage (§11) | `keyring` |
| Errors | `thiserror` (library), `anyhow` (binaries) |
| Tracing | `tracing` + `tracing-subscriber` |

Toolchain pinned to Rust **1.85.0** via `rust-toolchain.toml` (minimum required for Cargo's `edition2024` feature, which `rmcp-macros 0.1.5` depends on). Cargo workspace at repo root; three crates under `crates/`. All version pins live in the workspace `[workspace.dependencies]` table; member crates reference them with `name = { workspace = true }`.

### Tool error model

Tools fail in a small set of ways. The state machine routes each error class to a specific recovery transition.

| Error | Transition | Notes |
|---|---|---|
| `SandboxViolation` | `→ Failed` | Security event; do not auto-recover. |
| `Timeout` | `→ Streaming` | Inject error into next turn; agent retries or abandons. |
| `McpServerUnreachable` | `→ Streaming` (budget=3) | Retry budgeted; on exhaustion `→ AwaitingUser`. |
| `McpServerCrashed` | `→ Streaming` (budget=3) | On 3rd crash, drop server for session, `→ AwaitingUser`. |
| `ResultMalformed` | `→ Streaming` | Inject parse error; agent retries. |
| `PermissionDenied` | `→ AwaitingUser` | User decision required. |
| `ExecutionFailed` | `→ Streaming` | Inject exit code + stderr; normal debug flow. |
| `SchemaViolation` | `→ Streaming` | Inject schema error; agent retries or reports. |

Canonical Rust definitions live in `atelier-core::error` (taxonomy + `Recovery` enum + unit tests for the routing). The session schema's `tool_fixtures` carries an optional `error` object (`kind`, `message`) when a fixture records a failed call instead of a successful one.

### Tool dispatch is unified
Built-in tools (file ops, shell, search — bundled in `atelier-core`) and MCP-routed tools (external servers per §15) flow through the same `ToolDispatching → ToolExecuting` state transitions. The loop does not branch on tool origin: same parallelism cap, same checkpoint/ledger writes, same sandbox model (§11), same cancellation semantics.

### Streaming UI semantics
Text tokens and tool calls stream; the envelope does not. UI consumers must define rendering for three states.

**During a turn (`Streaming`/`ToolDispatching`/`ToolExecuting`):**
- Conversation pane: text tokens append in real time as they arrive.
- Tool-call cards: appear in the conversation the moment the model emits the call. Card status badge transitions: `queued` → `dispatching` → `executing` → `done` / `failed`.
- Envelope-derived panels (claimed_changes, grounding badges, uncertainty prompts): render in a `pending` state — visible scaffolding, no content. Pending state uses the same gray/neutral palette as the `grounding: absent` fallback so users don't read "empty panel" as "model said nothing."

**Turn end + envelope parses successfully:**
- Claimed_changes panel populates against the actual on-disk diff (did-it-do-what-it-said visible, §7).
- Grounding badges color (green / yellow / red).
- Uncertainty prompts (if any) surface as inline UI elements, not buried prose (§9).

**Turn end + envelope parses with errors (malformed JSON, schema violation, missing required field):**
- A `envelope-invalid` warning bar appears at the bottom of the conversation pane with the validation error.
- The re-prompt loop (§2 conformance enforcement) kicks in automatically; the UI shows "re-prompting model with validation error… (attempt N of 3)".
- On 3rd failure, the harness downshifts to the next-lower §2 emission strategy and the bar updates to reflect that.

The envelope is never rendered token-by-token; users never see a half-parsed `claimed_changes` array.

### What this rules out
- **Multi-turn upfront planning** as default behavior (opt-in only).
- **Token-level streaming of envelope fields.** Envelope is parsed as a whole at end-of-turn (or end-of-strategy-2 second pass). Text and tool calls stream; envelope does not. Avoids partial-UI states for `grounding` / `uncertainty`.
- **Speculative tool execution.** Tool calls dispatch only after the model emits them.

### Acceptance gates
§2.5 has no separate gate. The agent-loop contract is exercised by Phase A's gate (canonical workload runs end-to-end against Anthropic + LiteLLM adapters through this loop), Phase B's verification fixtures (transitions through `Verifying`), and Phase D's interrupt test (three sequential cancel-and-restart cycles).

---

## 3. Workspace, not chat log

### Requirements
- Multi-pane: conversation, live diff, file tree (agent's working set), plan canvas, memory, cost meter, context meter. Progressive disclosure.
- Live diff updates as the agent edits.
- Hunk-level accept / reject / rewrite. "Why this change?" cites §2 `grounding`.
- Drag-and-drop: file → forced context; hunk → "do this differently."
- Inline rendering: images, Mermaid/D2, tables, test-result trees, browser previews.
- Terminal is one pane; slash commands work.

### GUI / TUI parity
- **GUI is canonical.** Implementation: Tauri 2.x shell (`atelier-gui` crate) consuming `atelier-core` via a broadcast channel — see §2.5.
- **TUI subset:** conversation, textual diff, file tree, plan canvas (collapsible tree), cost meter, context meter, timeline scrubber (keys: `[` `]` step, `g <n>` jump). Implementation: `ratatui` + `crossterm` (`atelier-tui` crate), consuming the same `atelier-core` events.
- **GUI-only:** drag-and-drop, Mermaid/D2 inline, browser previews, visual hunk-rewrite.

### Atomic application
Multi-file edits emitted in a single turn apply **all-or-nothing**. The harness:

1. Stages every write from the turn to a temp tree (`tempfile::TempDir`).
2. Runs pre-commit validators (syntax check via tree-sitter where available; conflict check against current workspace state).
3. **On all-pass:** atomically moves the temp tree contents into the workspace; emits one §4 checkpoint covering the whole batch; verification gate (§7) runs against the known post-state.
4. **On any failure:** discards the temp tree; emits a `ToolError` per §2.5 (`ExecutionFailed`, `SchemaViolation`, or class-appropriate) back into the next turn's context.

No per-edit opt-out. If the agent wants independent edits, it emits multiple turns — the existing turn structure is the lever. This keeps verification semantics simple and §4 checkpoints clean.

### Tree-sitter grammar coverage
The pre-commit syntax check uses tree-sitter. Atelier bundles grammars for **Tier 1** languages in `atelier-core`; Tier 2 is deferred to v0.2.

**Tier 1 — bundled in v1.** File extension → grammar crate:
- `.py` — `tree-sitter-python`
- `.ts`, `.tsx` — `tree-sitter-typescript`
- `.js`, `.jsx` — `tree-sitter-javascript`
- `.rs` — `tree-sitter-rust`
- `.go` — `tree-sitter-go`
- `.json` — `tree-sitter-json`
- `.toml` — `tree-sitter-toml`
- `.yaml`, `.yml` — `tree-sitter-yaml`

**Tier 2 — deferred to v0.2.** Java, C#, Ruby, C/C++, Shell, Markdown, HTML, CSS.

**Files with no matching grammar** (Tier 1 or 2) skip the syntax check; the atomic-application step still runs the conflict check and the on-disk move. The UI annotates the per-file outcome with `syntax-check: pass | fail | not-applicable | grammar-missing`.

Bundled grammar size adds ~3–5 MB to the `atelier-core` binary; revisit if it grows past 10 MB.

### Acceptance gates

**Mechanical:** 10-file scripted rename. Agent emits per-file `claimed_changes`. Live diff updates incrementally. Final diff byte-equal to ref answer. TUI runs same fixture; subset snapshot-asserted.

**UX target:** user drives the refactor without opening the conversation pane.

---

## 4. Time travel

### Requirements
- Per-action diff-based checkpoint covering file, conversation, memory, plan state.
- Timeline scrubber; fork; cherry-pick (scoped); branch comparison.
- Storage-bounded; auto-prune; never silent.

### Storage budget
- Diff-based only. No full-state snapshots.
- Default **500 MB per repo** (PROVISIONAL — calibration: measure checkpoint size distribution on canonical workload, pick a budget that retains a useful working set without exceeding typical laptop disk headroom).
- Eviction: oldest non-forked first. Visible prune indicator.

### Cache economics
Fork, manual eviction, and compaction-expansion each emit a `cache_bust` ledger event with cost delta. Fork dialog shows preview when source is in a cached prefix.

### Cherry-pick scoping
Allowed only when source tool call's referenced files exist in target at compatible content. Otherwise: "manual replay" surface.

### Replayable sessions
- Every tool call records inputs **and outputs** as fixtures in the session artifact.
- Replay reads tool results from fixtures, not by re-executing tools.
- Model output: deterministic when provider honors `seed` + `temperature=0`; otherwise the harness records model output as a fixture and replay plays it back.
- `--re-execute` flag on the replay command performs a live re-run instead of fixture playback; comparison report shows where re-execution diverged from the recorded run.

### Acceptance gates (mechanical)
- Rewind-5 → fork → modify → merge passes the fixture test suite.
- `cache_bust` events emitted per fork; cost delta non-zero where source was cached.
- Replay determinism: a session recorded against a non-deterministic provider replays byte-equal three times.

---

## 5. Visible context, memory, plan

### Requirements
- Context panel: every item with token counts; pin / unpin / evict. Items can be files, conversation turns, tool results, **or MCP resources** (per §15) — the panel does not distinguish origin in its API.
- "Why is this in context?" trace per item.
- Memory panel: editable cards, last-used timestamps, one-click promote.
- Plan canvas: editable tree; reorder, constraints, manual mark-done. Re-entering plan mode is one keystroke.
- Non-destructive compaction; expansion gated.

### Cache-aware eviction
- Pin: free.
- Unpin same-turn additions: free.
- Otherwise: confirm dialog with cache-bust cost estimate.

### Mental-model panel
Off by default. Enabling shows projected per-session token cost at the active executor's observed rate.

### Project config (`ATELIER.md`)
A markdown file at the repo root provides project-specific persistent instructions to the agent — equivalent to Cursor's `.cursorrules` or Claude Code's `CLAUDE.md`. Atelier reads it at session start and injects its contents into the system prompt; treat it as context that applies to every turn.

The seed template (shipped embedded in `atelier-core`; written by `atelier init`) uses these sections, which users may delete, rewrite, or reorganise freely:

- *What this project is* — one-paragraph orientation.
- *Conventions* — formatter, linter, test command, file organisation.
- *Don't touch* — generated files, vendored deps, off-limits paths.
- *Useful commands* — `make test`, `cargo nextest run`, etc.
- *Anything else* — free-form notes.

The file is plain markdown; the harness imposes no schema. HTML comments (`<!-- … -->`) are stripped before injection — usable for notes to humans the model never sees. The seed template is a convention, not a contract.

### Acceptance gates

**Mechanical:** context-panel API returns token count + why-here trace per item; evicting a cached item emits `cache_bust` ledger entry.

**UX target:** "find what the agent knows about file X" median <5 s. User study; non-blocking.

---

## 6. Steerability

### Framing
Cancel-and-restart with state preservation, not native pause/resume.

On interrupt:
1. Cancel the active stream.
2. Preserve completed tool calls and partial output.
3. Reissue a turn with the user's correction injected as a user message; partial state available as context.

**Implementation:** the §2.5 state machine treats cancellation as Rust drop semantics — the active `Streaming` or `ToolExecuting` state is dropped, tokio aborts the inner futures, and the session transitions to `AwaitingUser` with partial output captured in the `recovery_log` slot (§14). No invented cancel protocol.

### Between-turn vs mid-turn
| Action | Between turns | Mid-turn |
|---|---|---|
| Add constraint pin | Applied to next turn | Cancel; restart with constraint in context |
| Edit plan canvas | Applied to next turn | Cancel; restart with new plan |
| Per-tool kill switch | Applied to next turn | Cancel; surfaced; user decides re-start |
| Interrupt-with-edit | n/a | Cancel; restart with user message appended |

Inter-action pauses (e.g., §14 concurrent-edit modal) operate at tool-call boundaries — they queue the next dispatch rather than cancel mid-stream. They do not depend on this pillar's cancel plumbing.

### Acceptance gate (mechanical)
Scripted test interrupts a long multi-file edit 3 times. Each is a cancel-and-restart with the new constraint in the next turn. Final output respects all 3 constraints.

---

## 7. Verification gates

### Requirements
- Per-task machine-checkable DoD: test, type-check, lint, build, exit codes, optional screenshot diff, log-line assertions.
- The **harness owns the `done` transition**; agent emits `claimed_done` via §2; UI shows `claimed` and `verified` as distinct. Implementation: a `Verifying` state in the §2.5 state machine, entered automatically on `claimed_done: true` and exited only by the configured DoD's outcome. The gate runs against the post-state produced by §3's atomic-application step — partial edit states never reach `Verifying`.
- Did-it-do-what-it-said diff consumes §2 `claimed_changes`.
- Hallucination detector (tiered, below).
- Auto-scaffolded gates (guarded, below).

### Hallucination detector — language tiers
- **Tier 1 (LSP, hard symbol check):** TypeScript first. Go and Rust next. Java and C# later.
- **Tier 2 (best-effort AST/regex):** Python, Ruby, PHP.
- **Tier 3 (textual):** shell, config DSLs.

UI surfaces the tier.

### LSP packaging
Shell-out with auto-install prompt on first use of a language. If the user declines auto-install: Tier-1 degrades to Tier 2 (best-effort AST/regex) for that language; UI shows "Tier 2 — LSP not installed" with a one-click retry. The harness never silently runs without a hallucination detector.

### Auto-scaffolded gates — tautology guard
- Explicit user accept before a scaffolded test counts as a gate.
- "Scaffolded-by-`<model-family>`" tag persisted across sessions.
- A scaffolded test cannot gate edits by the same model family for 7 days (PROVISIONAL — calibration: empirical false-positive rate of self-gating against the canonical workload). Cross-family gating allowed.

### Acceptance gates (mechanical)
- Lying-agent fixture flagged within 1 turn via did-it-do-what-it-said diff.
- Hallucinating-agent fixture flagged within 1 turn (TypeScript Tier 1).
- Same-family tautology guard refuses gating by a scaffolded test the same family wrote within the window.

---

## 8. Trust budgets

### Classifier
Each tool call: `local-safe` / `local-risky` / `shared-state` / `irreversible`.

### Budget
- Default session budget `N = 20` units (PROVISIONAL — calibration: median session reaches 80% budget on canonical workload without refill prompts).
- Costs: local-safe = 0; local-risky = 1; shared-state = 20 (always asks); irreversible = 20 + double-confirm. All PROVISIONAL, calibrated together.
- Refund: +1 when a verified test pass follows the most recent local-risky action.

### Permission learning
After `K = 3` (PROVISIONAL) approvals of the same action shape, offer "always allow." Shape definition:
- **Same tool name**, AND
- **Same `side_effect_class`**, AND
- **Match against the per-tool shape config** at `schemas/config/permission_shapes.v1.json`.

Shape config defines per-tool grouping. For `bash`, two calls share shape iff `argv[0]` matches and the set of `--flag` names matches (flag *values* may differ). `git status` and `git status --short` are different shapes; `git status --short` and `git status --short -b` are different shapes; `cat foo.txt` and `cat bar.txt` are the same shape.

For `write_file`, two calls share shape iff the path matches the same configured glob (default: directory depth 2).

### Persistent permission state
"Always allow / always deny" decisions persist across sessions per `schemas/config/permission_state.v1.json`. Two files:
- **Per-repo:** `<repo>/.atelier/permissions.json`. Takes precedence on conflict.
- **Global:** `~/.atelier/permissions.json`. Fallback.

Loaded at session start; merged with `always_deny` overriding `always_allow` on conflict. The user can edit these files by hand or via the `atelier perms` CLI (TBD; not Phase A). Each entry records `tool`, `shape` (matching the per-tool shape kind), `captured_at`, and an optional human `note`.

### Per-path policy
Glob-based, user-editable. Defaults: `src/**` proceed; `migrations/**`, `**/*.tf`, `.env*` ask.

### Sandbox vs payload preview
- Ops with dry-run: show dry-run.
- Ops without: show payload preview.
- Never claim a sandbox where none exists.

### Baseline measurement
The `≤30% of v1 Claude Code` UX target follows the procedure documented at `tests/workload/canonical/baseline_procedure.md`. Baseline data conforms to `schemas/baselines/permission_prompts.v1.json`. The workload runner at `tests/workload/runner/runner.py` executes a task against either the harness (`--harness-cmd`) or in dry-run mode (`--dry-run`) for fixture validation. The runner asserts each task's `meta.json` (`schemas/workload/task_meta.v1.json`) `expected_starting_returncode`, executes every assertion in `checks.json` (`schemas/workload/task_checks.v1.json`) — including `file_unchanged` hash checks that catch a no-op harness on tasks whose starting state is already passing — and writes results conforming to `schemas/workload/runner_result.v1.json`. Sentinel blocks (`<<<atelier-meta>>>...<<<end>>>`) emitted by harness stdout are validated against `schemas/workload/atelier_meta_sentinel.v1.json` after extraction. Three layers back the schema-validation phase gate: `tests/validate_schemas.py` (meta-validates the schemas themselves), `tests/validate_artifacts.py` (validates concrete artifacts including example session files at `tests/sessions/examples/`, plus envelope JSON inside few-shot markdown), and the rig self-test suite (`tests/test_schemas.py`, `tests/test_validators.py`, `tests/test_runner.py` — 52 tests, all assertions schema-locked, including cross-schema `$ref` resolution from session → envelope). The cross-schema registry lives in `tests/_schema_helpers.py` and is shared between the artifact validator and the regression tests. All four — schemas, artifacts, rig tests, and dry-run — run via `make check`. CI runs `make check` on every push/PR via `.github/workflows/check.yml`. Baseline comparison runs via `tests/workload/runner/compare_baselines.py`.

### Acceptance gates

**Mechanical:** approval-shape learning after 3 same-shape approvals; per-path policy gates `migrations/**` and passes `src/**`.

**UX target:** prompt count ≤30% of captured baseline.

---

## 9. Uncertainty UI

### Requirements
- Per-output grounding badge: green / yellow / red, from §2 `grounding`.
- Structured uncertainty signal → human-input prompt.
- Mental-model panel: see §5 (opt-in).
- Disagreement disclosure: "proceeded under protest" recorded when user overrides; visible later.

### Degradation
Models that don't emit §2 grounding render badges **gray** with tooltip "model lacks grounding API." Never substitute green.

### Acceptance gate (mechanical)
Mock model emits one `uncertainty` and one `grounding: guess`. UI shows uncertainty prompt and one red badge. Snapshot test.

---

## 10. Multi-agent and parallelism

Three distinct modes; build them in the order shown. Delegation mode is the most commonly requested feature and lands in Phase D / E. Comparison and critic modes remain Phase F.

### 10.1 Delegation mode — sub-agents (Phase D / E)

The parent agent invokes the built-in **`spawn_subagent`** tool to delegate an isolated side task. The harness materialises a fresh §2.5 state machine with its own context, tool set, and trust budget; the sub-agent runs to completion and returns a single message to the parent's tool-result slot. The parent never sees intermediate steps.

**`spawn_subagent` tool contract.** Built-in tool, side-effect class `local-risky` (because the sub-agent may itself make risky calls inside its budget). Input:

| Field | Type | Required | Notes |
|---|---|---|---|
| `description` | string | yes | Short, surfaced in the parent's conversation card and the sub-agent pane title. |
| `prompt` | string | yes | The first user message in the sub-agent's conversation. |
| `subagent_type` | string | no | Name of a registered sub-agent type (`schemas/config/subagent_type.v1.json`). Defaults to `general-purpose`. |
| `tool_allowlist` | array<string> | no | Restricts the sub-agent's tool set. Defaults to the type's allowlist or the parent's full set. |
| `max_turns` | integer | no | Per-invocation cap. Defaults to the type's `default_max_turns` or the §10 default (25, PROVISIONAL). |

Output:

```json
{
  "subagent_id": "sa-1",
  "result": "<final assistant message>",
  "status": "completed | failed | timed_out | cancelled",
  "turns_used": 4,
  "cost": {"prompt_tokens": 1400, "completion_tokens": 220, "cost_usd": 0.0042}
}
```

**Sub-agent types.** Bundled at `crates/atelier-core/subagents/`; user-overridable at `~/.atelier/subagents/` (global) and `<repo>/.atelier/subagents/` (per-repo, highest precedence). v1 bundled:

- **`researcher`** — read-only investigation; `tool_allowlist: read_file/list_dir/grep/ast_grep`; `side_effect_class_cap: local-safe`.
- **`test-runner`** — runs the project's test command and reports; read + shell, no edits; cap `local-risky`.
- **`general-purpose`** — catch-all; inherits the parent's full tool set; no cap.

Manifest schema: `schemas/config/subagent_type.v1.json`. Each type may override the session's routing (`model_routing` field).

**Session-state representation.** Each sub-agent invocation appears in the session schema's `subagents` map (keyed by `subagent_id`), with its own conversation, tool fixtures, turn count, status, and `cost_summary` that rolls up into the parent's cost ledger.

**Sub-agent interactions with other pillars:**

- **§4 Time travel.** A sub-agent's checkpoints chain off the parent's at the spawn-point. Rewinding past the spawn-point also rewinds the sub-agent (it disappears). Forking a checkpoint that contains an in-progress sub-agent forks both.
- **§7 Verification gates.** The parent's `claimed_done` gate runs only after all spawned sub-agents have terminated. A sub-agent can carry its own `claimed_done`; the parent reads it from the `result` text but the harness's gate runs against the *parent's* DoD, not the sub-agent's.
- **§8 Trust budget.** Sub-agent inherits the parent's remaining budget. Optionally the `spawn_subagent` call can pass a sub-budget (`max_budget` future field — not v1); unused budget returns to the parent on completion.
- **§11 Sandbox.** Sub-agent runs inside the same sandbox profile as the parent; `tool_allowlist` and `side_effect_class_cap` provide additional belt-and-braces restriction.
- **§3 UI.** Each active sub-agent appears as a card under the parent's conversation pane; click expands to a dedicated sub-agent pane. Status badge `running` → `completed` / `failed` / `timed_out` / `cancelled`.

**Cancellation.** The parent can cancel a running sub-agent by re-invoking `spawn_subagent` with the same `subagent_id` plus `cancel: true`, or via the UI's cancel button on the sub-agent card. Cancellation cascades — if a sub-agent has spawned its own sub-agents, those cancel too.

**Recursion depth.** Sub-agents may themselves spawn sub-agents. Default depth limit is **3** (PROVISIONAL; calibration: empirical max useful depth on the canonical workload). Beyond the limit, `spawn_subagent` fails with a `ToolError::SchemaViolation` (per §2.5).

### 10.2 Comparison mode (Phase F)

Run the same task against multiple model configs side-by-side. Distinct from delegation: same prompt, different routings, no handoff. UI shows N panes in parallel; user picks the winner with one click; ledger records all N sessions' costs.

### 10.3 Background critic (Phase F)

A low-cost model critiques every parent-agent output in an advisory side pane. Off by default; cost-multiplier disclosed on enable. Does not gate the §2.5 state machine.

### Acceptance gates

**Delegation mode (Phase D/E mechanical):** parent invokes `spawn_subagent` with `subagent_type: researcher`; sub-agent runs to completion within its turn budget; result returns as a tool-call message to the parent; session schema's `subagents` field populates and validates; parent's verification gate (if any) runs after the sub-agent completes.

**Comparison mode (Phase F):** same refactor against three configs in parallel; ledger records all three.

**Background critic (Phase F):** critic emits an advisory envelope alongside the parent's; the parent's §2.5 state machine is unchanged.

---

## 11. Security & sandboxing

### Implementation
- **macOS:** `sandbox-exec` with generated `.sb` profile per tool call.
- **Linux:** `bubblewrap` with read-only repo bind mounts, tmpfs `/tmp`, no network unless `--allow-net` is set on the tool manifest.
- **Windows:** not supported in v1. WSL recommended.
- Containers deferred.

### Policy
- Default: repo-scoped FS, no network egress, no writes to `/etc` or `/usr/local`.
- Out-of-repo reads require approval (per-path policy applies).
- Hooks require per-hook approval on first use; subsequent runs use §8 trust budget.
- Harness never executes model-emitted code outside the tool-call approval flow.

### MCP servers
- **stdio MCP servers** (the common case) are launched as subprocesses inside the same sandbox profile as any other tool call: repo-scoped FS, no network unless `allow_net: true` is set on the server in `mcp_servers.json` (`schemas/config/mcp_servers.v1.json`).
- **HTTP / SSE MCP servers** are remote endpoints; their URLs are subject to the §12 egress audit log and the redaction policy.
- First-use approval for each registered MCP server runs through the §8 trust budget at the *server* level (not the tool level — granting trust to a server grants it to all tools that server exposes, subject to per-tool `side_effect_class`).

### Credential storage
- **Primary:** OS keychain via the `keyring` Rust crate. macOS Keychain, Linux Secret Service (libsecret), Windows Credential Manager. Entries keyed by `service = "atelier"`, `user = "anthropic" | "openai" | "googleai" | <mcp-server-name>`.
- **Override:** environment variables — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc. Takes precedence over keychain. Used for CI and env-hygiene-conscious users.
- **Forbidden:** plaintext keys in any committed config file. `mcp_servers.json` `headers` values support the interpolation tokens `${env:NAME}` and `${keychain:NAME}`; literal secrets are rejected at load.
- **Resolution order at session start:** env var → keychain → typed error pointing the user at `atelier login <provider>`.
- **CLI commands** (provided by `atelier-gui` / `atelier-tui`, backed by `atelier-core::secrets`):
  - `atelier login <provider>` — stores a key (reads from stdin or prompts interactively).
  - `atelier logout <provider>` — removes the keychain entry.
  - `atelier rotate <provider>` — convenience for logout + login.
  - `atelier whoami` — lists configured providers without revealing keys.
- **Audit:** §12 egress audit records `Authorization` headers as redacted; `${keychain:…}` and `${env:…}` interpolation is resolved at request time and never persisted in the audit record.

### Credentials abstraction (non-API-key providers)
Not every adapter authenticates with a static API key. AWS Bedrock signs each request with SigV4 over the AWS credential chain; GCP Vertex AI uses Application Default Credentials (ADC); local LLMs (Ollama, llama.cpp, MLX-LM) typically need no auth at all. To keep the §1 `Adapter` trait clean of provider-specific auth code, `atelier-core` exposes a small `CredentialsProvider` trait that adapters consume:

```
pub trait CredentialsProvider: Send + Sync {
    fn shape(&self) -> CredentialShape;
    fn resolve(&self) -> Result<ResolvedCredentials, CredentialError>;
}

pub enum CredentialShape {
    ApiKey,           // header injection — current keychain/env flow
    AwsSigV4,         // per-request request signer
    GcpAdc,           // bearer token from google-cloud-auth
    Local,            // no auth
}
```

Each `CredentialShape` has its own resolution logic:
- **`ApiKey`** — env → keychain → typed error (the existing flow above).
- **`AwsSigV4`** — AWS credential chain (env `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` → `~/.aws/credentials` profile → IAM instance role → SSO refresh). Pluggable via `aws-config` crate.
- **`GcpAdc`** — Application Default Credentials chain (env `GOOGLE_APPLICATION_CREDENTIALS` → `gcloud auth application-default login` cache → service-account JSON → GCE metadata server). Pluggable via the `google-cloud-auth` crate or equivalent.
- **`Local`** — no-op; resolves to empty credentials.

CLI surface extends accordingly:
- `atelier login bedrock` — verifies the AWS credential chain resolves; if not, prompts for SSO start-url or access-key.
- `atelier login vertex` — verifies ADC resolves; if not, prompts for `gcloud auth application-default login`.
- `atelier login ollama` (or any `Local` provider) — no-op with a confirmation message.

Audit (§12) records the resolved `CredentialShape` (not the credentials themselves) so users can see *how* a remote call authenticated without exposing secrets.

### `atelier init` — project bootstrap
Idempotent project-scaffold command (provided by `atelier-gui` / `atelier-tui`, backed by `atelier-core::init`). Run from a repo root, it:

1. Creates `<repo>/.atelier/` if absent, with subdirs `sessions/`, `tools/`, `hooks/` (empty placeholders).
2. Writes `<repo>/ATELIER.md` from the seed template at `crates/atelier-core/templates/ATELIER.md` (embedded via `include_str!`) **if** the file does not already exist. Never overwrites a user's existing `ATELIER.md`.
3. Appends `.atelier/` to `<repo>/.gitignore` if a `.gitignore` exists and the entry is absent. Does not create a new `.gitignore`.
4. Prints a one-line summary of what it did (or didn't do, on each step), and exits 0.

Re-runnable safely on an already-initialised repo; reports "no changes" if everything is in place.

### Acceptance gate (mechanical)
Model attempts `curl evil.example`; blocked; attempt logged.

---

## 12. Privacy

- Per-call record: content hash, target provider, redaction applied.
- Per-path redaction defaults: `**/secrets/**`, `.env*`, `**/*.pem`, `**/*.key`. Never sent to remote models; substituted with placeholders.
- Egress audit log exportable per `schemas/audit/egress.v1.json`. **MCP HTTP/SSE servers count as egress targets and are logged the same way as LLM providers** — `provider` field on the audit record carries the MCP server name.
- Local-only mode: remote adapters refuse to start; MCP servers with HTTP/SSE transport also refuse to start.

### Acceptance gate (mechanical)
Redaction blocks `.env` content; mock provider receives placeholders.

---

## 13. Telemetry

Three independently opt-in channels, all off by default:
- **Crash reports:** stack trace, harness version, OS, exit code.
- **Performance:** tool-call latency histograms, ledger summary stats.
- **Usage events:** anonymized feature usage.

Universal guarantees: no prompt content under any setting; outgoing payloads inspectable before send; 90-day retention at the collector; export-and-delete endpoint.

Payload schema: `schemas/telemetry/payload.v1.json`.

---

## 14. Persistence and recovery

### Mid-turn crash
On restart, harness resumes at the last completed tool call. In-flight stream is discarded. **Partial output is preserved in a dedicated `recovery_log` slot, not in conversation history** — this prevents the next turn's model from misreading the partial as a completed action. The recovery log surfaces in the UI as a banner; the user can manually inject any salvageable content into the conversation if desired.

### Concurrent edits
File-watcher (fsevents / inotify) detects external edits to files in the agent's read set. Harness queues the next tool-call dispatch (does not cancel the current one) and surfaces a modal with three named options:

1. **Accept external edits and re-plan** (default). External diff appears in the next turn's context; agent acknowledges via `constraints_acknowledged`.
2. **Revert external edits.** Reverts to pre-edit state; turn resumes. Confirm dialog required.
3. **Open three-way merge in diff pane.** User resolves manually; harness waits.

Pause is <1 s from filesystem event to modal. If no response within 5 minutes (PROVISIONAL), the turn auto-pauses with state preserved and the user is notified.

**Headless mode:** `--non-interactive` flag (for CI / scripted use) auto-resolves to option 1 with the chosen resolution logged. Without the flag, headless contexts that hit the modal will time out at the auto-pause threshold and exit non-zero.

### On-disk storage
Sessions live in a **hybrid layout**: per-repo storage + global registry.

- **Per-repo:** `<repo>/.atelier/sessions/<session-uuid>/session.json` + `<repo>/.atelier/sessions/<session-uuid>/diffs/<sha256>.diff`. The session document and its diff blobs travel with the repo; users can `.gitignore` the whole `.atelier/` tree (default), or check in selected sessions if they want to share a replayable artifact.
- **Global registry:** `~/.atelier/registry.json` — a small index mapping session UUID → repo path + last-touched timestamp. Lets the CLI/GUI answer "list my recent sessions" without scanning every repo. Rebuilt opportunistically; safe to delete.
- **Diff blobs are content-addressed:** `<sha256>.diff` under the session's `diffs/` directory. The session schema's `checkpoints.nodes[*].diff_ref` field references blobs by their hash.

Local-only mode (§12) and `--non-interactive` are orthogonal to this layout. Out-of-repo sessions (e.g., one-off runs without a repo) fall back to `~/.atelier/sessions/<uuid>/`.

### Diff blob format
Each blob at `diffs/<sha256>.diff` stores the change between a checkpoint and its parent. **Format: unified diff** (`diff -u` output, with a header naming the file paths and a `--- /dev/null` / `+++ /dev/null` convention for adds and deletes). Rationale: text, human-readable, well-tooled, applies cleanly with `patch` or any diff library.

**Large-file threshold:** files larger than **1 MB** (PROVISIONAL — calibration: median project file size in the canonical workload + a 95th-percentile margin) bypass diff encoding and store the whole new content at `diffs/<sha256>.full`. The session schema's `diff_ref` accepts both `<sha256>.diff` and `<sha256>.full`.

**Binary files:** detected by NUL byte in the first 8 KB. Stored as `.full` blobs (no diff encoding); the unified-diff format isn't safe for binary content.

**Compression:** blobs above 4 KB are stored as `.diff.zst` / `.full.zst` (zstd). Below the threshold, stored uncompressed for fast `cat` debugging.

**Reconstruction:** to materialise a checkpoint's workspace state, walk parent → child applying each `diff_ref` in order from the root. `.full` blobs replace rather than apply.

### Headless exit codes
When `--non-interactive` is set, the harness must terminate cleanly on conditions a human would normally resolve interactively. Exit code policy:

| Code | Condition | Notes |
|---|---|---|
| 0 | Turn completed; verification gate (if invoked) green | Success |
| 1 | Verification gate failed (e.g., `pytest` returned non-zero) | The model's claimed_done was overridden by §7 |
| 2 | `ContextOverflowError` fell through to "Cancel turn" | Compact attempted but couldn't fit; no larger-window adapter configured |
| 3 | Concurrent-edit modal hit the auto-pause threshold | 5-minute default; configurable |
| 4 | Sandbox violation in headless mode | §11 SandboxViolation; `allow_net` not granted |
| 5 | Model adapter unavailable | API key missing, provider unreachable, or `conformance()` exhausted retries |
| 6 | Envelope schema violation, all three §2 strategies failed | Persistent malformed output from the model |
| 7 | Permission denied; no `always_allow` entry matched | `--non-interactive` cannot prompt for trust-budget approval |
| 64–78 | Reserved for [sysexits(3)](https://man.freebsd.org/cgi/man.cgi?query=sysexits&sektion=3) standard codes | E.g., `64` `EX_USAGE`, `66` `EX_NOINPUT` |
| 100+ | Tool-specific propagation | A tool exited with code 100; the harness exits with that same code if the tool's failure aborted the run |

CI scripts can rely on these codes; future versions add only — never remove or repurpose.

### Versioning
Session artifacts carry `harness_session_version`. Schema-breaking upgrades ship a one-way migration tool. The spec, the session schema, and the protocol envelope schema each have independent version numbers; their compatibility matrix is maintained at `schemas/versions.md`.

---

## 15. Extensibility

### Tools — MCP-first
The **primary tool transport is the Model Context Protocol (MCP)**. `atelier-core` ships an MCP client (via the `rmcp` crate, §2.5) on day one. Any MCP-compliant server — filesystem, GitHub, Slack, web search, databases, custom — can be registered without writing Atelier-specific glue.

**Server registration:** users edit `mcp_servers.json` (per `schemas/config/mcp_servers.v1.json`). Atelier reads it at session start, launches stdio servers as subprocesses inside the §11 sandbox, opens HTTP/SSE connections (subject to §12 egress audit), and registers the tools each server advertises. Server registration is a §8 trust-budget event on first use.

**Discovery — the Servers panel (GUI):** editing JSON is fine for power users and acceptable in the TUI; GUI users get more. The GUI's Servers panel:

- Lists registered servers from `mcp_servers.json` with status (running / stopped / errored), last-used timestamp, advertised tool count, enable/disable toggle.
- **Add server** button → form that writes a schema-valid entry to `mcp_servers.json`. Transport-conditional form (stdio: command + args; http/sse: url + headers) mirrors the schema's `oneOf`.
- **Browse catalog** button → opens an in-app curated catalog of well-known MCP servers (`@modelcontextprotocol/server-filesystem`, GitHub, Slack, web-search providers) bundled with the harness as a versioned JSON list at `crates/atelier-core/catalog/mcp_servers.json`. Adding from the catalog generates the registration entry pre-filled; user just supplies any required secrets (via `${keychain:NAME}` interpolation, §11).
- **Edit / remove** per row. Edits write back through the schema validator; invalid edits show the validation error in the form, not silently.

Bundled catalog refresh: a `make refresh-catalog` target pulls the latest catalog from the harness's release artifacts; users can override with their own catalog path via `~/.atelier/catalog.json` if they want a private/internal list. Remote catalog auto-fetch is deferred to v0.2.

TUI does not get the catalog browser in v1 — the TUI subset per §3 stays focused; TUI users edit JSON or use the GUI for first-time setup.

**Built-in tools** (in `atelier-core`, exposed via the same MCP interface internally — no special case): `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell` (sandboxed). Built-in tools are surfaced exactly like external MCP tools so the rest of the harness (verification gates, hooks, ledger, trust budget) treats them uniformly.

**Tool dispatch:** flows through the §2.5 `ToolDispatching → ToolExecuting` state transitions regardless of origin. Bounded parallelism, checkpoint/ledger writes, cancellation semantics, and sandbox model are uniform.

**MCP resources** (read-only data: file contents, search results, etc.) appear in the §5 context panel as first-class items — pinnable, evictable, with the usual why-here trace.

**MCP prompts** (pre-defined prompt templates) are deferred to v0.2; the protocol surface is acknowledged but Atelier v1 does not yet consume them.

### Hooks
Pre-tool / post-tool / on-verify-pass / on-verify-fail. Each declares a time budget; over-budget = warn and continue, never block. Hooks wrap both built-in tool calls and MCP-routed tool calls uniformly — no special case.

### Skills
Named, user- or agent-invocable procedures. A **skill** is a manifest (`schemas/config/skill_manifest.v1.json`) declaring a `name`, `description`, `prompt_template`, optional `args`, optional `pinned_context`, optional `tools_required`, and optional `proactive_trigger`. Distinct from tools (model-invoked per-turn primitives), hooks (lifecycle event responders), and MCP servers (external tool providers).

**Invocation:**

- **Manual** — user types `/<name> [args]` in the conversation pane (GUI/TUI). The harness expands `prompt_template` with `${arg}` substitution; the expanded text becomes the next user turn. The §2.5 agent loop runs unchanged.
- **Proactive** — when the skill carries `proactive_trigger`, that text is summarised in the system prompt so the model is aware of it. When the model judges the trigger condition matches, it suggests the skill via the §9 uncertainty UI ("Run /<name>? — _reason_"); user accepts or dismisses.

**Storage** (read at session start, same convention as tools / hooks / mcp_servers):

- `~/.atelier/skills/<name>.json` — global.
- `<repo>/.atelier/skills/<name>.json` — per-repo, overrides same-name global.
- Bundled in `crates/atelier-core/skills/` — overridable by either of the above.

**Bundled v1.** Three skills ship with the harness:

- **`/review`** — review the current diff for regressions, missing tests, security concerns, ATELIER.md violations.
- **`/security-review`** — security audit; carries `proactive_trigger` so the model suggests it when authentication/credential/secret-handling code changes.
- **`/test`** — run the project's test command (from ATELIER.md's "Useful commands") and summarise.

`/help` and `/init` are **harness-intercepted CLI verbs**, not skill manifests — they don't reach the model.

**`/help` output format:** the conversation pane prints one line per registered skill:

```
/<name>  <description>  [proactive]  <source>
```

- `<name>` is left-justified to the longest registered skill name.
- `[proactive]` marker appears only if the skill carries `proactive_trigger`.
- `<source>` is one of `[bundled]`, `[~/.atelier/skills/]`, `[<repo>/.atelier/skills/]`.

Skills sort by group (bundled → global → per-repo), then alphabetical within group. When a per-repo or global skill overrides a same-name bundled or global one, only the winner is shown; the suppressed entry is silently skipped. Below the skill list, `/help` prints a one-line summary of harness-intercepted CLI verbs (`/init`, `/help`) and the `atelier <subcommand>` shell verbs (`atelier login`, `atelier logout`, `atelier rotate`, `atelier whoami`).

**Substitution variables** available in `prompt_template`:

- `${<arg_name>}` — declared args.
- `${repo_root}` — absolute path of the repo root the session was started in.
- `${atelier_md}` — contents of the repo's `ATELIER.md`, or empty string if absent.

**Cost-ledger tracking** — skill invocations are recorded as a `note` on the next turn's `model_call` ledger entry: `"skill: <name>"`. No additional ledger event; the skill is a prompt expansion, not a separate turn.

### Providers
§1 adapter-trait implementers (LLM providers, distinct from MCP servers). First-class adapters live in-tree; community adapters live out-of-tree. Adapter packaging is a Rust workspace concern, not an MCP concern.

### Acceptance gate (mechanical)
Phase A delivers a working MCP client that registers at least one third-party MCP server (the official `@modelcontextprotocol/server-filesystem` reference server) and dispatches at least one tool call through it during the canonical workload runs.

---

## Cross-cutting requirements

- Local-first; no required cloud.
- Replayable sessions per §4.
- Hook system per §15.
- Config is data; JSONSchema-documented; UI-editable.
- Accessibility: keyboard-first; screen-reader; TUI parity per §3.

### Performance budgets
- **Harness-internal overhead** (dispatch + ledger write + checkpoint write): median <50 ms. Excludes tool execution and verification gate runtime.
- **End-to-end tool roundtrip**: tool-class-dependent; logged, not budgeted.
- **Verification gate runtime**: logged with progress UI; not budgeted.
- **Per-hook budget**: 200 ms median (PROVISIONAL — calibrated against observed hook usage). Over-budget hooks emit a warning panel; turn proceeds.
- **Cold start**: GUI <4 s, TUI <1 s. Both with progress indicator.

Reference machine spec: `tests/perf/reference.md`.

---

## Hard tradeoff decisions

1. Time-travel storage — diff-based, 500 MB default (PROVISIONAL). §4
2. GUI / TUI parity — GUI canonical, TUI subset listed. §3
3. Streaming threshold — 200 ms.
4. Local-model concurrency — auto-serialize via `capabilities()`.
5. Verification-gate scaffolding — opt-in; cross-family gating allowed, same-family refused for 7 days (PROVISIONAL). §7
6. LSP packaging — shell-out with auto-install prompt; decline degrades to Tier 2. §7
7. Sandbox — sandbox-exec (macOS), bubblewrap (Linux); containers deferred. §11
8. Local cost — latency-weighted; default `$0.00028/sec`. §1
9. Replay — tool-result fixtures; provider seed honored where supported. §4

---

## Phased build plan

Phases group pillars that ship together. **Phase A + B is the internal backend milestone** — the smallest end-to-end working pipeline against a real model with verified completion, exposed only via the harness's tool and protocol APIs. **Phase A + B + C is the first user-facing release** — adds the workspace UI required for a real user to drive a refactor without scripting against the APIs. Within a phase, items can run in parallel; the phase gate combines all items. **Every phase gate includes schema validation**: every artifact emitted by phase tests (session files, ledger entries, envelopes, audit log entries, telemetry payloads, baseline files) must validate against its schema in `schemas/`. A failing schema validation blocks the phase gate.

### Phase A — Foundation
- `atelier-core` crate scaffolded with the §2.5 state machine, `tokio` runtime, broadcast event channel
- §1 BYOM adapter trait + two adapters (Anthropic + LiteLLM-shaped); OpenAI adapter and local adapters deferred
- §15 MCP client (via `rmcp`) — stdio + HTTP transports; built-in tools exposed via the same interface
- §11 Security
- §14 Persistence (crash recovery, concurrent-edit modal at tool-call boundary)
- §15 Hook contract (so §11 can call hooks safely)

**Gate:** §1 mechanical + §11 mechanical + crash-and-recover scripted test. **Plus:** `atelier-core` drives canonical workload priority subset (t01, t02, t05, t06, t10) end-to-end via the §2.5 loop, with at least one third-party MCP server (`@modelcontextprotocol/server-filesystem`) registered and exercised during the runs.

### Phase B — Protocol and trust
- §2 Model Protocol (all three strategies; real-model conformance gate)
- §7 Verification (did-it-do-what-it-said + TypeScript Tier-1 hallucination detector)

**Gate:** §2 mechanical + real-model conformance ≥95% (PROVISIONAL); §7 lying-agent and hallucinating-agent fixtures.

### Phase C — Workspace surface
- §3 Workspace UI (GUI first; TUI subset after GUI gate)
- §5 Visible context / memory / plan

**Gate:** 10-file rename mechanical; context-panel API assertions.

**Data-layer prerequisites (land in `atelier-core` before the UI work begins).** The Phase C UIs consume typed APIs, not stubs. Four modules in `atelier-core` carry the data the GUI / TUI render — they're built first so the UI work can target a stable surface rather than reshape it from `Vec<serde_json::Value>` placeholders later:

1. **Context manager** — typed `ContextItem` with token counts, why-here provenance trace, pin / unpin / evict operations; eviction emits a `cache_bust` ledger entry per §1.
2. **Typed memory** — replaces `session.memory: Vec<Value>` with `MemoryCard` (matches the schema's `{id, content, created_at, last_used, pinned}`); operations: add, touch, pin/unpin, evict, promote (write to the global memory dir).
3. **Typed plan** — replaces `session.plan.steps: Vec<Value>` with `PlanStep` (matches the schema's `{id, text, status, constraints?}`); operations: add, reorder, mark_done, mark_skipped, add_constraint.
4. **Incremental diff event stream** — per-tool-call `EditStaged { path, hunks }` events on the §2.5 broadcast bus, derived from §3 staging's commit report by diffing the pre-image against the staged bytes. Feeds the §3 "Live diff updates as the agent edits" requirement at the granularity the agent loop actually produces (per tool call, not per token).

**UI unblockers (atelier-core).** The data layer above is necessary but not sufficient: the UIs assume a running agent loop driving real envelopes through that data layer. Five items in `atelier-core` (and one in `atelier-cli`) close the loop. They land in dependency order; each is unit-testable in isolation:

1. **§1 BYOM adapter trait** — `Adapter` with `chat`, `stream`, `count_tokens(source: TokenSource)`, `capabilities()`, `conformance()` (backed by the §2 `ConformanceRingBuffer` already built). Carries `Capabilities`, `CapabilityClaim::{Supported, ClaimedButBroken, Unsupported}`, `ContextOverflowError`. Mock impl for downstream tests.
2. **§1 Typed cost ledger** — `LedgerEntry::{ModelCall, ToolCall, CacheBust}` mirroring `schemas/session/v1.json` `cost_ledger[]`. Replaces `OnDiskSession.cost_ledger: Vec<Value>`. Append-only; consumed by every adapter / dispatcher / context eviction.
3. **§15 tool dispatcher** — `Tool` trait + `Dispatcher`. Per tool-call: §15 pre-tool hooks → execute inside §11 sandbox profile → §3 atomic staging (for writes) → §15 post-tool hooks → publish `Event::EditStaged` (via the `edit_staged_events` helper already built) → append `LedgerEntry::ToolCall`. Mock `Tool` for tests.
4. **§15 built-in tool implementations** — `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`. Manifests already bundled at `crates/atelier-core/tools/`; each gets a Rust `Tool` impl. Lands across multiple commits; the dispatcher does not block on all seven being present. The `shell` tool and the §15 `ShellHookExecutor` share a single subprocess + sandbox + warn-but-never-block-timeout helper (`crates/atelier-core/src/subprocess.rs`) so the §11 plumbing isn't duplicated between the two consumers.
5. **§1 Anthropic adapter** — concrete `Adapter` against the Anthropic Messages API. Streaming via SSE; native tool-use channel for §2 strategy 1. Tests against recorded HTTP fixtures rather than live calls.

A thin `SessionDispatcher` wraps the pure [`Dispatcher`](#) with a `&Ledger` and the session bus, so the agent-loop integration code calls one method instead of reimplementing the "append entry + broadcast events" boilerplate per call. The pure `Dispatcher` stays the test surface; `SessionDispatcher` is the runtime surface.

**Phase C UI-gate unblock actions (in dependency order).** Once the items above land, four wiring steps turn the Phase C gates from "blocked on no driver" into "runnable":

1. **`atelier run` CLI subcommand** (`crates/atelier-cli`) — wires the §2.5 actor + `Dispatcher` + `ToolRegistry` (all 7 built-in tools) + `HookSet` + `DodConfig` + `SandboxPolicy` + `Ledger` into a runnable loop. Reads prompt from arg or stdin; subscribes to the broadcast bus; loops turns until `claimed_done: true`; transitions to `Verifying` for DoD checks; saves the session via `OnDiskSession::save_to`. Drives the §3 mechanical gate (10-file scripted rename, byte-equal final diff) against `MockAdapter` immediately; same code runs against any later adapter without changes.
2. **§1 Anthropic adapter** — turns "agent loop runs against a mock" into "agent loop runs against a real model." After this, Phase B's ≥95% real-model conformance gate becomes runnable, and the GUI / TUI gates have real envelopes to render.
3. **Tauri GUI bootstrap** (`crates/atelier-gui`) — `cargo tauri init` (D1–D4 interactive: bundle id, app name, frontend stack, dev server URL) plus the mechanical M1–M6 per the crate README. First panel subscribes to `Handle::subscribe` and renders `Event::EditStaged`.
4. **TUI widgets** (`crates/atelier-tui`) — `ratatui` + `crossterm` against the same broadcast bus; ships the §3 TUI subset (conversation, textual diff, file tree, plan canvas, cost + context meters, timeline scrubber).

Steps 1 + 2 close §3's 10-file-rename gate and §5's context-panel-API + cache-bust-ledger gates. Steps 3 + 4 close the per-frontend snapshot gates and the UX targets.

The MCP client (`rmcp`) and the LiteLLM adapter follow once Q7 (rmcp spike) and the Anthropic adapter are in. Step 3's interactive `cargo tauri init` is the one moment the build needs a human — `crates/atelier-gui/README.md` separates D1–D4 (irreversible decisions) from M1–M6 (mechanical setup) so it's fast.

### Phase D — Time and steerability
- §4 Time travel
- §6 Steerability

**Gate:** rewind-fork-merge mechanical; 3-interrupt mechanical.

### Phase E — Trust calibration and surrounding UI
- §8 Trust budgets (calibrated using accumulated Phase B–D data)
- §9 Uncertainty UI
- §12 Privacy
- §13 Telemetry
- §1 **Native cloud adapters** — AWS Bedrock (SigV4 auth via the new §11 `CredentialsProvider::AwsSigV4`) and GCP Vertex AI (ADC auth via `CredentialsProvider::GcpAdc`). The Phase A LiteLLM proxy gets you both day-one at the cost of an extra hop; the native adapters here are for first-class streaming, capability advertisement, and per-provider cost reporting that the routing UI needs.
- §1 **Per-task routing UI** — surfaces the `schemas/config/routing.v1.json` configuration in the GUI / TUI, including free-form custom roles (e.g., `documenter`, `architect`). Depends on ≥3 adapters being available so the cost-aware planner-vs-executor split has something to choose between.

**Gate:** §8 learning + per-path mechanical; §9 snapshot; §12 redaction mechanical; routing UI demonstrably switches models per role on the canonical workload.

### Phase F — Deferred
- §10 Multi-agent
- §1 OpenAI adapter (native) — Phase A's LiteLLM proxy covers it; native lands here for parity with Anthropic
- §1 **Local LLM adapters** — Ollama first (HTTP/SSE; simplest), then llama.cpp (native bindings or HTTP server mode), then MLX-LM (Apple-Silicon-specific). All consume `CredentialsProvider::Local` (no-op auth) and benefit from the latency-weighted `local_rate` cost model from §1.
- §7 Tier 1 (Go, Rust, then Java, C#); Tier 2 (Python, Ruby, PHP); Tier 3; auto-scaffolding
- §15 Tool plug-in manifest; community adapter packaging

---

## Definition of done

### Backend milestone — Phase A + B (internal; not user-facing)
- [ ] Phase A gate green
- [ ] Phase B gate green
- [ ] Schema validation passing for every Phase A/B artifact
- [ ] Canonical workload priority subset (t01, t02, t05, t06, t10) completes against Anthropic + LiteLLM adapters via API
- [ ] Crash-and-recover preserves state

### First user-facing release — Phase A + B + C
- [ ] Backend milestone met
- [ ] §3 GUI 10-file rename gate green
- [ ] §5 context-panel API assertions green
- [ ] Cold start GUI <4 s

### Full v1
- [ ] Every pillar's mechanical gate green
- [ ] Canonical workload completes against Anthropic, OpenAI, local Qwen
- [ ] §8 ≤30% UX target met against captured baseline
- [ ] Performance budgets met on reference machine
- [ ] All PROVISIONAL parameters replaced with calibrated values + recorded calibration data

### UX targets (measured; non-blocking)
- New user, non-trivial task, 30 min — user study; reported.
- "Find what agent knows" median <5 s — user study; reported.
- Multi-file refactor without conversation pane — user study; reported.
