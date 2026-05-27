# What Goes Into a Coding Harness

Most AI coding tools are chat interfaces with side effects. A prompt goes in, code comes out, and whatever happened in between is opaque. A *coding harness* is a different bet: the agent is a collaborator sharing a workspace, not a chat box with side effects. "Done" is a property the harness verifies, not a claim the model makes.

This post walks through the essential components of a coding harness — what problem each solves, how they interact, and how each maps to the concrete implementation in Atelier.

---

## The Core Premise

Three convictions drive the architecture:

1. **Bring your own model.** The loop must not care which model is behind it. Vendor lock-in at the inference layer undermines everything else — portability, cost control, capability testing. Every other component is designed around this.
2. **The model protocol is the trust surface.** The agent's claims — what it changed, how confident it is, what plan it is executing — must be machine-readable. If claims live in prose, nothing downstream can verify them. If nothing can verify them, verification gates are impossible.
3. **Done is a state transition the harness owns.** Not a claim in the model's output. The harness decides when a turn is complete, what it produced, and whether that matches what the model said it did.

These three convictions are load-bearing in sequence: BYOM makes the harness portable; the model protocol makes verification possible; verification is what separates the harness from a wrapper.

---

## 1. Model Abstraction (BYOM)

The BYOM adapter is the harness's only contact point with the inference layer. Every provider — cloud, local, mock — implements the same trait:

```rust
async fn chat(&self, messages: &[Message], tools: Option<&[ToolSpec]>) -> Result<Response>;
async fn stream(&self, ...) -> impl Stream<Item = Chunk>;
fn count_tokens(&self, messages: &[Message]) -> TokenCount;
fn capabilities(&self) -> Capabilities;
fn conformance(&self) -> ConformanceStats;
```

The `capabilities()` call is what enables graceful degradation. If a model doesn't support native tool use, the harness emulates it via JSON-mode. If streaming isn't available, it blocks on the full response. The capability matrix is explicit: every entry has an "if absent" column and a "claimed-but-broken" column — because a provider claiming a capability it consistently fails to deliver is a distinct failure mode from one that honestly reports it as unsupported.

The `conformance()` method surfaces a bounded ring buffer of the last 100 calls. This is how the harness detects capability drift in-session — not by trusting the provider's advertised spec, but by observing actual emission behaviour.

**In the repo:** `crates/atelier-core/src/adapter/` — `mod.rs` holds the trait and `MockAdapter`; `anthropic.rs` and `openai_compat.rs` are the two production implementations. `model_profile.rs` caches the probe-on-first-use capability result to disk so calibration doesn't re-run on every invocation.

---

## 2. The Model Protocol Envelope

The agent's turn output is not a blob of text. It is a structured envelope alongside the text — fields for claimed changes, uncertainty signals, grounding sources, and plan updates. The envelope is the harness's trust surface: it is the mechanism by which verification gates, the UI, and the cost ledger know what the model *said it did*.

Three emission strategies are auto-selected based on what the model supports:

1. **Native tool call** (`harness_meta` tool) — cleanest, no overhead.
2. **JSON-mode side channel** — sentinel-bracketed: `<<<harness_meta>>>{...}<<<end>>>`. Used when the model supports structured JSON output but not a harness-specific tool.
3. **Regex-prose** — tagged sections extracted from free text. Lossy; the UI downgrades envelope-derived panels to a gray "unknown" state.

The strategy selection is automatic and recorded. Each failed envelope parse triggers a re-prompt with the validation error inline; three consecutive failures in a turn force a downshift to the next lower strategy. This is the protocol conformance loop — the harness does not give up on structure silently.

**Why this matters for verification:** the `claimed_changes` field is a machine-readable list of what the model said it edited. The verification gate compares this against the actual on-disk diff. Mismatch — claimed an edit that didn't happen, or made an edit that wasn't claimed — is the "lying-agent" signal. Without the envelope, this comparison is a natural-language parsing problem. With it, it is a data structure comparison.

**In the repo:** `crates/atelier-core/src/protocol.rs` defines the `Envelope` struct; `protocol_strategy.rs` and `protocol_conformance.rs` handle auto-selection and the conformance ring buffer. Schema: `schemas/model_protocol/envelope.v1.json`.

---

## 3. The Agent Loop State Machine

The loop is a single-turn streaming state machine. One turn: the model streams text, tool calls, and envelope fields concurrently. The harness dispatches tool calls, records results, and either starts the next turn or transitions to verification when the envelope says `claimed_done: true`.

Named states:

```
Idle → Streaming → ToolDispatching → ToolExecuting → Streaming (loop)
                                                   → Verifying → Verified | Streaming (retry) | Failed
                ↘ AwaitingUser → Streaming (resume)
```

Every transition writes a checkpoint and a ledger entry. This is not a logging afterthought — it is what makes time travel and cancellation semantics tractable. Cancel means drop the current state; the partial output is preserved in a recovery log, and the session transitions to `AwaitingUser`. There is no invented cancel protocol. Rust drop semantics do the work.

The loop is intentionally unambitious: no mandatory plan-then-execute, no ReAct scratchpad, no per-turn self-critique. These are all opt-in, either through the envelope's `plan_update` field or through the routing configuration that sends a separate critic model against the same transcript. The loop stays simple so verification semantics stay tractable.

Concurrency is bounded. When the model emits parallel tool calls (both Anthropic and OpenAI support this), the harness dispatches them concurrently behind a semaphore capped at four. The per-session state lives behind a tokio `Mutex` or a dedicated actor with an `mpsc` inbox; UI consumers subscribe to a broadcast channel so no shared mutable state crosses the frontend boundary.

**In the repo:** `crates/atelier-core/src/state.rs` defines the named states and legal transitions; `crates/atelier-cli/src/runner.rs` wires the full loop — adapter, dispatcher, ledger, probe, and persistence — into the `Runner` struct that both frontends embed.

---

## 4. The Unified Dispatcher Surface

All tools — built-in file operations, registered MCP servers, sub-agents — enter the loop through one dispatcher. The loop does not branch on tool origin. This single surface is what makes the following invariants hold uniformly across everything the agent can do:

- **Hooks fire on every tool call.** Pre-tool and post-tool hooks from `.atelier/settings.json` apply whether the call is a `read_file`, an MCP tool on an external server, or a spawned sub-agent.
- **Every call is ledgered.** Latency, tool name, cost, and side-effect class are recorded identically regardless of where the tool lives.
- **Trust budget is checked before dispatch.** Each tool declares a `SideEffectClass` — `LocalSafe`, `LocalRisky`, `SharedState`, or `Irreversible` — and the dispatcher checks the budget before the call executes.
- **The sandbox gate applies.** Path safety and sandboxing wrap every dispatch, not just file operations.

The dispatcher returns a `DispatchOutcome` rather than writing to the ledger directly. This keeps it pure — testable without a running session actor — and mirrors the pattern used throughout the core: `staging.rs`, `verify.rs`, and `context.rs` all follow the same shape.

Eight built-in tools are currently implemented: `read_file`, `list_dir`, `grep`, `write_file`, `edit_file`, `ast_grep`, `shell`, and `spawn_subagent`. MCP-routed tools slot in as a `Tool` impl that proxies to the `rmcp` client — transparently, from the dispatcher's perspective.

**In the repo:** `crates/atelier-core/src/dispatcher.rs` is the central orchestrator; `crates/atelier-core/src/tools/` holds the built-in implementations; `crates/atelier-core/src/mcp/` wraps the production `rmcp` client.

---

## 5. Verification Gates

The verification gate is the component that earns the "harness" label. When the model emits `claimed_done: true`, the loop transitions from `Streaming` to `Verifying`. The harness owns this transition; the agent cannot bypass it.

Verification is tiered, and the active tier is surfaced to the user so they always know how strong the current check is:

- **Tier 1 — LSP diagnostics.** The highest confidence check. The harness runs the relevant language server against the staged edits and injects any diagnostics back into the next turn as tool results. Lives in `crates/atelier-core/src/lsp/`.
- **Tier 2 — tree-sitter syntax.** Syntactic checks against a bundled set of grammars (Python, TypeScript, JavaScript, Rust, Go, JSON, TOML). Runs in `staging.rs` before edits are committed. Falls back to Tier 3 if no grammar is available for a given file extension.
- **Tier 3 — textual lying-agent check.** A pure structural comparison: the claimed changes list from the envelope against the actual on-disk diff. This always runs, regardless of tier. Mismatches — claimed-but-not-present, present-but-not-claimed, wrong operation kind — surface as red in the UI and trip the mechanical gate.

Multi-file edits apply all-or-nothing. Every write from a turn is staged to a temp tree (`tempfile::TempDir`), syntax-checked, and only atomically moved into the workspace on all-pass. On any failure, the temp tree is discarded and the error is injected into the next turn's context as a `ToolError`. The agent sees the same error a developer would see — not a harness-specific message.

**In the repo:** `crates/atelier-core/src/verify.rs` (pure lying-agent comparator); `crates/atelier-core/src/staging.rs` (atomic staging + tree-sitter checks); `crates/atelier-core/src/lsp/` (Tier 1 LSP integration).

---

## 6. Cost Ledger and Trust Budget

Two accounting systems run in parallel, tracking different things.

The **cost ledger** tracks what was spent: prompt tokens, completion tokens, cached tokens, wall-clock latency, and dollar cost. Every model call, tool call, and cache-bust event appends an entry. Local models use a latency-weighted cost (`wall_clock_seconds × local_rate`, defaulting to `$0.00028/s`). The source of the token count is recorded alongside the count itself — `"exact"`, `"approx"`, or `"unavailable"` — because the accuracy of cost attribution depends on it. This is not cosmetic: it is what makes per-task routing a rational decision rather than a guess.

The **trust budget** tracks what the agent is allowed to do autonomously. Each tool call declares a `SideEffectClass`. Read-only operations cost nothing. Writes inside the workspace cost one budget unit. Operations affecting shared state — posting a comment, pushing a branch — cost twenty. Irreversible operations cost twenty and require a double-confirm. The dispatcher checks the budget before execution; when the budget is exhausted, the turn transitions to `AwaitingUser` and the agent cannot proceed without human input. This is the mechanism behind the autonomy dial: you can widen the budget for trusted tasks and tighten it for high-risk ones.

**In the repo:** `crates/atelier-core/src/ledger.rs` (append-only `RwLock`-backed ledger); `crates/atelier-core/src/trust_boundary.rs` (budget tracking and enforcement).

---

## 7. Context Management and Persistence

The agent's context is not just the conversation history. It includes the plan canvas, memory items, file tree markers, and pinned context. All of it has token counts attached, and all of it can be evicted by the user or compacted by the harness when the active model's context window is about to overflow.

Context overflow is a typed error, not silent truncation. When overflow is detected, the harness cancels the in-flight dispatch and transitions to `AwaitingUser` with three options: compact (default), reroute to a larger-window model, or cancel the turn. In headless mode, it defaults to compact; if compaction fails to fit, it falls through to cancel and exits non-zero. Silent truncation is explicitly prohibited because it makes the agent's behaviour unpredictable in ways neither the user nor the verification gate can detect.

Session persistence is diff-based. Every state transition writes a checkpoint. The on-disk representation is the session schema (`schemas/session/v1.json`), and the diff engine records the delta between checkpoints rather than snapshots — this keeps checkpoint size proportional to what changed rather than to the total session size. Resume works by replaying checkpoints from the last clean state; the UI exposes a timeline scrubber for navigating history.

**In the repo:** `crates/atelier-core/src/context.rs` (token budget + eviction); `crates/atelier-core/src/memory.rs` and `plan.rs` (memory items and plan canvas); `crates/atelier-core/src/persistence.rs` (on-disk session); `crates/atelier-core/src/diff.rs` (diff-based snapshot engine); `crates/atelier-core/src/session.rs` (event model).

---

## 8. Security and Sandboxing

Security in a coding harness is structural, not advisory. The three layers:

**Path safety** validates every file path before any tool touches the filesystem. Paths outside the workspace boundary are rejected; symlink traversal is checked; the check runs in the dispatcher before the tool executes, not inside the tool itself.

**Sandboxing** wraps shell and subprocess operations. On macOS this is `sandbox-exec`; on Linux it is `bubblewrap`. Both are invoked via `tokio::process`. The sandbox gate is not a best-effort advisory — a `SandboxViolation` error transitions the session directly to `Failed` and is not auto-recoverable.

**Trust boundary** is the third layer — the dispatcher's side-effect classification system described above. The design intent is that even if a tool implementation has a bug, the dispatcher's budget check catches the side-effect class before the damage is done.

Credentials follow the same principle: no raw API keys in `providers.toml` or environment variables in tracked files. The `atelier providers auth <profile>` command stores secrets in the OS keychain and writes `api_key = "keyring:SERVICE/USER"` to the config; CI uses `env:NAME` indirection. The harness reads the reference and resolves it at startup — the secret never appears in a log or config file.

**In the repo:** `crates/atelier-core/src/sandbox.rs`; `crates/atelier-core/src/path_safety.rs`; `crates/atelier-core/src/trust_boundary.rs`; `crates/atelier-core/src/credentials.rs`.

---

## 9. Hooks and Extensibility

Hooks are user-defined shell commands or HTTP callbacks that fire on loop events — before and after tool calls, on session start and end, on verification outcomes. They are configured in `.atelier/settings.json` and execute via the dispatcher's hook surface, meaning the same uniformity that applies to tools applies to hooks: they fire on every tool call regardless of origin, and their execution is time-budgeted so a slow hook cannot stall the loop.

Skills are a higher-level extensibility mechanism: prompted workflows that ride the same dispatcher surface as built-in tools. They live in `.atelier/skills/`, can invoke built-in or MCP tools, and are subject to the same ledger, trust budget, and verification gates as everything else.

**In the repo:** `crates/atelier-core/src/hooks.rs`; `crates/atelier-core/src/skills.rs`; `.atelier/skills/`.

---

## 10. Frontend Separation

The agent loop lives entirely in `atelier-core`, which has no UI dependencies. Both frontends — the GUI (Tauri 2 + Svelte 5) and the TUI (ratatui + crossterm) — consume `atelier-core` through the same broadcast channel. They do not re-implement the loop; they embed `atelier-cli::Runner` and drive it.

This separation is the single most important architectural decision for long-term maintainability. The specification calls it out explicitly as a failure mode to prevent: "Embedding loop logic in either UI is the single most likely architectural failure mode and is explicitly prohibited." The data flow is one-directional: `atelier-core` ← `atelier-cli` ← `atelier-gui` / `atelier-tui`. Neither frontend can modify loop state directly; they can only send inputs and subscribe to events.

The consequence is genuine GUI/TUI parity: when a verification gate is tightened, both frontends reflect it immediately. When a new adapter is added, both frontends gain access to it without any frontend code changing.

---

## Putting It Together

The components form a clear dependency order:

```
BYOM adapters → Model Protocol → Agent Loop → Dispatcher
                                            ↘ Verification
                                            ↘ Ledger + Trust Budget
                                            ↘ Security
                                            ↘ Hooks
                                            ↘ Persistence
```

Everything upstream of the dispatcher is the harness's *input* machinery — getting a structured, accountable signal from the model. Everything downstream is *accountability* machinery — checking that signal against reality, recording what happened, and enforcing what the agent is allowed to do.

The model protocol envelope is the pivot point. It is why verification is a data comparison rather than a language parsing problem, why the cost ledger has accurate attribution, and why the trust budget can be calibrated. Without it, the components are independently useful but do not compose into a harness. With it, "done" can be a property the harness verifies rather than a claim the model makes — which was the premise from the start.
