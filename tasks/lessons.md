# Atelier — per-project process lessons

Per the host-harness self-improvement loop: after corrections, capture the failure mode + the prevention rule. Per-project lessons live here; cross-project lessons go to `~/.atelier/memory/feedback_*.md` (cross-machine) or `.atelier/memory/feedback_*.md` (project-scoped).

This file is volatile — entries are pruned when the underlying class of mistake stops happening.

---

## v60.36–v60.38 — Deep-scan response

### Subagent stream-watchdog stalls on large-scope rust-reviewer tasks

**Failure**: launched four parallel deep-scan agents (atelier-core, atelier-cli, atelier-gui+tui, non-Rust). Three of the rust-reviewer agents timed out on the 600s stream watchdog after producing meaningful partial output but before delivering a report. The non-Rust scan was too broad (rig + schemas + workflows + shell + frontend in one prompt) and also stalled.

**Prevention**: when delegating a deep scan, keep per-agent scope under one crate-or-equivalent. If a single area (e.g., a 30k-LoC crate) is the target, split by module — give one agent `crates/atelier-core/src/{adapter,session,dispatcher}.rs` and another the rest. For multi-area scans, split by directory tree (rig in one agent, schemas in another) rather than bundling. Watch for the failure mode: snippets in `<result>` tags that look like the agent was about to produce findings — the work happened, the delivery failed.

### Workflow privilege-split needs verified artifact-action SHAs

**Failure**: refactoring three nightly workflows into measure+commit jobs needed `actions/upload-artifact` + `actions/download-artifact` SHAs. Without web access I'd have had to either guess (risky — wrong SHA breaks the nightly silently) or punt the fix.

**Prevention**: when a deferred tool (here `WebFetch`) exists, load it via `ToolSearch` *before* spending tokens deliberating around the gap. For GitHub specifically, `gh api repos/<owner>/<repo>/git/refs/tags/<tag>` returns the commit SHA without needing WebFetch at all — faster path. The general lesson is "check the tool surface before declaring an action infeasible."

### Heredoc-with-step-output interpolation is a foot-gun even with safe values

**Failure**: `nightly_phase_a_gate.yml`'s `Compose` step interpolated `${{ steps.X.outputs.Y }}` directly into a JSON literal inside a heredoc. The values are integers today, but the pattern is a quiet hazard for any future step output that grows a quote, newline, or NaN.

**Prevention**: build CI artifacts via Python (or `jq -n`) with structured field-by-field assignment, not via shell heredoc string concatenation. Step outputs flow through `env:` block + `os.environ.get(...)` so quoting is handled by the JSON encoder. The pattern also lets you use `allow_nan=False` to fail loudly on producer NaN.

---

## v50 — OpenAI-compatible adapter

### Anthropic ≠ OpenAI for tool-call argument encoding

**Failure**: First `openai_compat.rs` draft tried to send tool-call `function.arguments` as a `serde_json::Value` (Anthropic's `tool_use.input` shape). Wire format requires a JSON-encoded **string**.

**Prevention**: When porting an adapter from one provider to another, write a wiremock test that asserts the *exact request body shape* the server expects (`assert_eq!` against a captured fixture, not just "200 OK"). The tool-call round-trip is the highest-fidelity test — if it doesn't match byte-for-byte, multi-turn flows silently corrupt.

### SSE parsers must be `\r\n` / `\n` / `\r` tolerant

**Failure**: `OpenAiSseSource` initially split on `\n` only. Some providers (and some `curl --no-buffer` reverse proxies) emit `\r\n` line terminators; lone `\r` sneaks in too when a server flushes mid-frame.

**Prevention**: Mirror `anthropic.rs`'s line-buffered state machine on every SSE parser. The split happens on **bytes**, not strings — only attempt UTF-8 decode on the assembled event payload, never on a raw chunk.

### Drop guards beat manual cleanup on every exit path

**Failure**: Per-run workspace cleanup in `atelier-gui/src/lib.rs` was a tail call. An error mid-loop left orphan tempdirs.

**Prevention**: Any resource that needs cleanup on every exit path (success / `?`-propagated error / panic) gets a `Drop` impl. `RunCleanup`, `DispatcherHandleGuard`, `TerminalGuard` are the pattern. Tail calls don't survive panics; `Drop` does.

---

## v51 — Probe-on-first-use

### Sentinel tags are project constants, not free strings

**Failure**: Probe driver hardcoded `<<<envelope>>>` as the open tag in the calibration prompt + tests. The actual tag is `<<<harness_meta>>>` (`protocol_strategy::SENTINEL_OPEN`). Four tests failed because the model's "correct" reply didn't parse.

**Prevention**: When a calibration / golden prompt depends on a project constant, import the constant — don't retype the string. `use crate::protocol_strategy::{SENTINEL_OPEN, SENTINEL_CLOSE};` and build the prompt with `format!("{SENTINEL_OPEN}…{SENTINEL_CLOSE}")`. Tests use the same constants.

### Distinguish fatal probe errors from "this strategy didn't work"

**Failure**: First draft of `probe_model` returned `AdapterError` from any probe call failure. A transient `Malformed` response from one probe killed the whole probe; the cache stayed empty; the next run paid two more round-trips.

**Prevention**: `is_fatal_for_probe(&err)` distinguishes `Auth` / `NotConfigured` / `Unreachable` / `ContextOverflow` (propagate — no point continuing) from `Malformed` / `Provider` / `RateLimited` (record a note, set the flag to `false`, continue). The probe always completes when the endpoint is reachable, and the cache records what actually happened.

### Static vs dynamic capability detection are complementary, not alternatives

**Insight (not a failure)**: When the user asked for adaptive model detection, three approaches existed: (1) static capability matrix, (2) probe-on-first-use, (3) adaptive few-shot. The right answer wasn't to pick one — it was to ship the probe first because it discovers truth, and leave room for the static table to override the probe for well-known models (Anthropic, Mock — we already do this via `ProbePolicy::Skip`).

**Prevention**: When the design space looks like "A vs B", check whether they're complementary layers. Probe + static table is the cleanest decomposition; the static table is the cache hit path for known models, probe is the slow path for unknown ones.

### Cache key needs a separator

**Failure**: Almost shipped `sha256(model_id + base_url)` without a separator. `("ab", "cd")` and `("a", "bcd")` would have produced the same hash.

**Prevention**: `cache_path_does_not_collide_via_concat_ambiguity` test locks this in. Any time a hash function takes a tuple of strings, the prevention rule is "use an in-band separator that can't appear in the inputs" — `"\n"` works here because model_ids never contain newlines.

---

## Cross-cutting observations from v41–v51

### Bundle commit per phase, not per fix

**Pattern that worked**: v41–v50's GUI panes, hunk approval, driver modes, and OpenAI-compat adapter all sat uncommitted on a single feature branch and landed as one large commit (`a44b223`, +8816 / −477). The user prefers this for refactors and feature-blocks; many small commits would have churned the changelog without adding signal.

**When to break this rule**: A genuinely independent fix (like the probe work in v51, which doesn't depend on any v41–v50 internal state) lands as its own commit. The signal is "would a reviewer want to bisect through this?"

### Documentation rots faster than code

**Pattern**: README.md and STATUS.md both still claimed "atelier run coming with Phase A" and described the GUI/TUI as "Scaffold" through v50. The deep documentation sweep at the end of v51 caught it.

**Prevention**: Every CHANGELOG entry that lands a user-visible feature is also a TODO to update README.md / STATUS.md / per-crate READMEs. Better practice: a `make docs-check` linter that greps for stale "coming soon" / "not yet" claims against the current `CHANGELOG.md` headers. Worth building when the count of crusty claims gets above two.

---

## v52–v60.17 — Phase A close-out lessons (referenced by Phase D/E/F entries)

Lessons distilled from the v52–v60.17 trail: four deep-scan audit rounds, four parallel-bundle releases, the rmcp foundation, the §15 built-ins-as-MCP refactor, the Phase A nightly gate, and the live-API t01 bring-up. Each lesson carries a stable ID (L-D-1 through L-D-10) so `tasks/todo.md` can reference them at phase entry.

### L-D-1 — Mock-only gates lie; live-probe muscle has to be part of each phase

**Failure**: v60.15–v60.17 surfaced *four* stacked bugs during live-API bring-up (dead `tools_spec` stub returning `Vec::new()`; macOS sandbox missing homebrew prefixes; `harness_meta` tool never advertised; runner emitted no system prompt). All four passed `cargo test --workspace` because every mock script reliably emits tool calls + `claimed_done=true` on turn 0 — the loop exits before any of these surfaces can be exercised.

**Prevention**: every phase gets at least one `#[ignore]`-gated live-API integration test from day 1 (the shape v60.10 established with `mcp_integration_npx`). Phase D's `--re-execute` divergence report must run against a real provider, not just a replayed mock. Phase E's trust-budget calibration *cannot* be deferred to "later" — reserve the live-API budget line in the same PR that lands the feature. Phase F's local-LLM adapters each need a canonical-workload probe against the real daemon before the merge.

### L-D-2 — Parallel bundles must be file-disjoint, especially on shared registries

**Failure**: every parallel-bundle release (v60.7, v60.8, v60.9, v60.10, v60.11) hit the same three additive collisions — `session.rs::Event` enum + `kind()` match, GUI `bridge_event` + Svelte `state.ts applyEvent/projectEvent`, TUI `apply` + `project_event`. Resolutions were always "keep both," but v60.8 once produced a git-auto-merged "frankentest" inside `run_integration.rs`. v60.10's B2 oversight was worse: the CHANGELOG claimed a merge that had never run, surviving the push because the docs sweep didn't verify each claimed bundle's merge commit existed on main.

**Prevention**: any PR introducing a new `Event::*` variant lands sequentially in a prep commit with empty match arms in all four sinks; only the *body* of each arm is safe to parallelise. Bundles touching disjoint registries can run in parallel; bundles touching shared registries are sequential, full stop. Docs sweep at end of a parallel batch greps `git log --grep="Merge .*: <bundle>" main` for each claimed bundle before pushing — a missing match fails the PR.

### L-D-3 — Tier/fallback ladders are the project's signature pattern; reuse the shape

**Insight (not a failure)**: three independent ladders shipped with the same shape — typed enum + `wire_label()` + serde-agreement test + `*Hint`/`*Status` projection for both UIs + colour-coded badge + "fallback was used" event on the bus: verification (`Tier1Lsp → Tier2TreeSitter → Tier3Textual → NotRun`, v60.8 A2), strategy degradation (`NativeTool → JsonSentinel → RegexProse`, one-way, v60.8 A4), context overflow (`Compact → Reroute → Surface` with `MAX_OVERFLOW_RETRIES` cap, v60.9 B1).

**Prevention**: when Phase D adds checkpoint eviction (`Cached → Compressed → Diff-only → Evicted`), Phase E adds sandbox preview (`DryRun → PayloadPreview → NoPreview`) or redaction tiers (`PerPathRule → DefaultMask → LocalOnly`), and Phase F adds sub-agent failure modes (`Resolved → AdvisoryOnly → Cancelled`) — reuse this exact shape. Skipping any element (especially the bus event for "fallback was used") makes the fallback silent, which the verify-tier indicator was specifically built to avoid.

### L-D-4 — Atomicity / fsync / TOCTOU bugs land late; route all writes through `atomic_write`

**Failure**: the same bug shape was fixed at least five times — ATELIER.md/init (v41 P3, v42 v25.2-C), sessions (v41 P3), registry (v35 M5), memory promotion across both GUI + TUI drivers (v58 M-sec-2, v59 M-sec-2 partial, v60 M-1), staging splice→commit symlink recheck (v58 H8, v59 M-sec-6), `commit_selected_hunks` two-pass atomicity (v58 C1). Each was either a write-rename without parent-dir fsync, or a re-check missing across a stage/commit gap.

**Prevention**: any new persistence path (Phase D checkpoint blobs, Phase E telemetry queue, Phase F sub-agent session shards) routes through `atomic_write` + `fsync_dir_best_effort` from `crates/atelier-core/src/init.rs`. Symlink re-check at commit time *if* the artifact can be edited externally (resume and fork are exactly the case where it can). `#[must_use]` on every `Result` from a persistence call — v60 retrofitted this to `ConformanceSnapshot::rate()` only after a `unwrap_or(1.0)` rubber-stamp landed.

### L-D-5 — Wire-format hygiene needs an agreement test, not a convention

**Failure**: every enum that crossed the bus or schema boundary drifted at least once — TUI `Debug` strings (v58 H7), GUI `projectEvent` short labels (v59), OpenAI duplicate-completion clobber (v59 H4). v60 closed the discipline by adding agreement tests for `Provenance`, `Payload`, `TokenSource`, `PlanStatus`, `ClaimedChangeKind`, `MessageRole`, `ProbeLoadOutcome`, `SideEffectClass`, `HookEvent`.

**Prevention**: Phase D/E/F will add new cross-boundary enums (`CheckpointKind`, `InterruptSource`, `TrustBudgetClass`, `RedactionRule`, `TelemetryChannel`, `SubagentType`, `RouterRole`). The agreement test asserting `wire_label()` == serde rename projection lands in the *first* commit of the enum, not as a follow-on audit fix. The cost of adding it later is consistently 3–5× higher because every consumer has to be re-verified.

### L-D-6 — PROVISIONAL constants need a nightly calibration job, not a future-self promise

**Failure**: existing PROVISIONALs — `MAX_OVERFLOW_RETRIES = 2` + `OVERFLOW_SAFETY_MARGIN_PCT = 25%` (v60.9 B1), `DEFAULT_DEGRADATION_WINDOW = 20` + `THRESHOLD = 3` (v60.8 A4), local cost `$0.00028/sec` (v60.7), recursion depth cap = 3 (v18) — were all shipped with comments saying "calibrate against Q1." Q1 was resolved in v6; none of the constants have moved.

**Prevention**: every PROVISIONAL gets a `// PROVISIONAL: <reason>, calibrate via <script-or-fixture>` source comment, and a nightly job (the `nightly_phase_a_gate.yml` shape from v60.13 — schema-typed artifact + status binary + commit-back) instruments the constant against the canonical workload and records the implied value. Calibration becomes "swap the const for the median," not research. Phase E §8 will add four more PROVISIONALs (N=20, action costs 1/20, K=3) — wire the calibration job in the same PR.

### L-D-7 — A "claimed but broken" surface is half a bug; integration-test the actual wire

**Failure**: v60.8 A2 shipped the verify-tier UI but `verify_pass` was never called from `runner.rs`. v60.10 B2 shipped a Tauri `swap_adapter` command that updated the slot + emitted events but didn't actually swap the adapter inside a running `Runner`. v54 carried memory cards on the bus though no source wires in. Each was honest in the CHANGELOG, but the surface looked complete.

**Prevention**: every new cross-cutting feature gets at least one integration test asserting end-to-end wire behaviour, not surface behaviour. For Phase D's cancel-and-restart: "scripted run, cancel at chunk N, assert (a) no further chunks emitted, (b) tool dispatcher reports no pending invocation, (c) `final_state == AwaitingUser`." For Phase E §8 refund-on-verified-pass, Phase F §10.1 cascade cancellation — same shape. If the test is hard to write, the wire is probably cut.

### L-D-8 — Adapter parity surfaces only when the same workload runs against ≥2 adapters

**Failure**: per-adapter few-shot override (v60.9 B4) only emerged after Anthropic's `JsonSentinel` parse-rate visibly diverged from OpenAI-compat's. Anthropic SSE bugs (v41 P2, v42 v25.2-B, v25.3) needed live traffic. Haiku-vs-Sonnet behaviour asymmetry (v60.15) — same Anthropic API, two different failure modes (wedge vs hard 400).

**Prevention**: Phase E's native Bedrock + Vertex adapters each ship with a side-by-side canonical-workload run against the LiteLLM-proxied baseline of the same model (t01/t02/t05/t06/t10). Phase F's Ollama/llama.cpp/MLX trio each get the same. The cost of a 5-task canonical run on a real endpoint is small; the cost of debugging a per-adapter quirk after merge is large.

### L-D-9 — Priority lattices need to be written as a table on day 1

**Failure**: v60 L-sec-3 inverted `merge_stop_reason`'s priority because the original ranked ToolUse above Refusal — a content-filter + tool_calls turn would have dispatched the tool. The lattice was under-specified; the bug only surfaced when a model emitted the unusual combination.

**Prevention**: every Phase D/E/F feature with conflicting signals locks the priority in a table in the CHANGELOG entry + a direct table-driven test (the shape v60 used). Specifically: §4 fork conflict (parent edit vs child edit on same file/line), §6 interrupt priority (user-cancel vs sandbox-violation vs `claimed_done`), §8 trust-budget priority (per-path policy vs action-shape vs first-use approval), §10 cascading cancellation (sub-agent `claimed_done` racing parent cancel).

### L-D-10 — Worktree-isolation hygiene needs a CI step, not a convention

**Failure**: v60.10 candor — B2's CHANGELOG claimed a merge that had never happened (survived through the push); B3's agent edited the parent repo's working tree before catching itself. Cross-project memory file `feedback_worktree_isolation_drift.md` exists but is invisible to CI.

**Prevention**: extend the `quality-cheap` CI job (v60.14) with a check that, given a PR body claiming "this lands bundles X, Y, Z," runs `git log --grep="Merge .*: <bundle>" main` and fails on any unmatched claim. Cheap, mechanical, and catches the failure mode at the moment of risk.
