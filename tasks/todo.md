# Atelier — Build Tracker

## Status
Pre-implementation of the harness itself. The supporting test rig is fully wired. Change history in `../CHANGELOG.md`.

**Rig state (verified by `make check`):**
- 20/20 schemas meta-validate
- 50/50 artifacts validate against schemas
- 112/112 rig self-tests pass
- 11/11 canonical fixtures pass dry-run
- Reference machine spec populated (M1 Pro / 32 GB / macOS 26.4.1)
- CI runs `make check` on push/PR (`.github/workflows/check.yml`); separate `rust` job runs `cargo fmt`, `cargo clippy -D warnings`, `cargo test -p atelier-core` on Ubuntu + macOS with the pinned 1.85.0 toolchain
- Cross-schema `$ref` resolves via the shared registry in `tests/_schema_helpers.py` (session → envelope, subagent-type → routing, tool manifest → `_implementation.v1.json`)
- Per-kind cost-ledger fields enforced via `allOf`/`if`/`then`; `tool_call` requires `tool_name` so replay can link the ledger entry to its `tool_fixtures` row
- ATELIER.md seed template embedded at `crates/atelier-core/templates/ATELIER.md` for `atelier init`
- Skills system (§15): schema + 3 bundled skills (`/review`, `/security-review`, `/test`) + 1 example
- MCP server catalog: schema + bundled list of 8 servers (`filesystem`, `git`, `sqlite`, `fetch`, `memory`, `github`, `postgres`, `puppeteer`) for the GUI's "Browse catalog"
- **Built-in tool manifests (§15)**: 8 bundled under `crates/atelier-core/tools/` — `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`, `spawn_subagent`. Matches spec §15 L722. All use the `implementation.kind: builtin` discriminator.
- **BYOM vendor-neutral baselines**: `schemas/baselines/permission_prompts.v1.json` uses `baseline_harness_name` + `baseline_harness_version` (renamed from `claude_code_version`). The format accepts any harness; Atelier compares against whichever harness §8 selects as reference. `.gitignore` excludes `.claude/`, `.cursor/`, `.aider/`, `.copilot/` so per-user agent configs do not leak.
- **Sub-agent delegation (§10.1, contract-only)**: schema + 3 bundled types (`researcher`, `test-runner`, `general-purpose`) + 1 example (`code-reviewer`) + session-schema `subagents` field. Spawn via the built-in `spawn_subagent` tool (schema at `crates/atelier-core/tools/spawn_subagent.v1.json`). Implementation lands in Phase D/E; contract is locked.
- Apache 2.0 license; SECURITY.md, CODE_OF_CONDUCT.md, CONTRIBUTING.md, PR + issue templates committed

**Implementation language: Rust.** Three crates: `atelier-core`, `atelier-gui` (Tauri 2.x), `atelier-tui` (`ratatui` + `crossterm`). Agent loop: single-turn streaming state machine on `tokio`. Tool transport: **MCP-first via `rmcp`**, with built-in tools exposed through the same interface. See spec §2.5 and §15.

**Outside this session — two external-action items before Phase A coding begins:**
1. **`experiments/rmcp_spike/`** — execute the spike procedure on the reference machine. ~30–60 min; outcome is GO / GO-WITH-CAVEATS / NO-GO on `rmcp` as the §15 client.
2. **`tests/baselines/permission_prompts.json`** — capture Claude Code baseline per `tests/workload/canonical/baseline_procedure.md`. Required for §8's UX target; doesn't block Phase A code.

**Phase A implementation order (per spec §"Phased build plan"):** §2.5 skeleton → §11 sandbox → §14 recovery scaffold → §15 MCP + built-in tools → §1 Anthropic adapter → §1 LiteLLM adapter → §15 hooks → §1 mechanical gate → §14 modal + §11 gate + crash test → Phase A gate.

## Working notes
- Schemas live under `../schemas/`. Spec references them by path.
- The Rust workspace scaffold compiles in principle (atelier-core has lib.rs + error.rs); the GUI and TUI crates are intentionally minimal stubs until `cargo tauri init` is run (see `crates/atelier-gui/README.md`).
- No real harness code written yet; that's Phase A.

## Open questions (must resolve before the phase that depends on them)

| # | Question | Blocks | Owner | Due | Status |
|---|---|---|---|---|---|
| Q1 | Canonical workload priority tasks t05, t06, t10 | Phase A §1 gate | — | before Phase A | **resolved v6** |
| Q2 | Reference machine spec | All perf gates | — | before Phase A | **resolved v13** (M1 Pro / 32 GB / macOS 26.4.1) |
| Q3 | LSP auto-install UX | Phase B §7 Tier-1 | unassigned | before Phase B | open |
| Q4 | Checkpoint storage backing | Phase D §4 | — | before Phase D | **resolved v13** (per-repo `.atelier/sessions/<uuid>/` + content-addressed diff blobs; see spec §14) |
| Q5 | Baseline capture against Claude Code | Phase E §8 UX target | unassigned | before Phase E | open (external action) |
| Q6 | Telemetry collector | Phase E §13 | unassigned | before Phase E | open |
| Q7 | `rmcp` maturity assessment | Phase A §15 | unassigned | before Phase A | open (procedure documented at `experiments/rmcp_spike/`) |

---

## Known smells, not blocking

Accumulated from six rounds of deep-audit. None of these block Phase A; each is a quality-of-life or robustness item that's been triaged as "fix opportunistically, not urgent." Listed so the build tracker is the single source of truth (audit-N43, v24).

**Bugs that fail open**
- **B2 / B3** — `t09_migrate_signature/checks.json` greps a non-existent `orders/` directory and uses an over-strict regex for the migrated signature. Works today only because `; true` swallows the grep failure.
- **B6** — runner's `run_check` crashes on a malformed `re.search` pattern in `checks.json`. Wrap with a try/except that turns it into a check failure.
- **B7** — `extract_meta` silently skips schema validation when `jsonschema` isn't installed. Log a warning or treat as a hard requirement.
- **B8** — `mcp_servers.v1.json` example uses `${TOKEN}` interpolation; description says `${env:TOKEN}` / `${keychain:TOKEN}`. Update the example.
- **B9 / B10** — `compare_baselines.py` aggregates by summing medians (statistically meaningless) and silently passes when one side is missing tasks.

**Schema / config smells**
- **S3** — `task_meta.v1.json` description says default test_command starts with `python`; runner uses `python3`.
- **S4** — `crates/atelier-core/catalog/mcp_servers.json` is a *catalog* (matches `mcp_catalog.v1.json`), not a *registration* (`mcp_servers.v1.json`). Confusing filename.
- **S6** — `session/v1.json` `cost_ledger.items` doesn't set `additionalProperties: false`.
- **S7** — `atelier_meta_sentinel.v1.json` uses `additionalProperties: true`. Dilutes the contract.
- **S8** — `permission_shapes.v1.json` uses `globs` (plural array); `permission_state.v1.json` uses `glob` (singular). Drift across the family.
- **S9** — `cost_ledger.model_id` has no pattern, and the four example sessions are inconsistent about provider-prefixing.
- **S14** — `test_schemas.py::test_unregistered_schema_ref_would_fail_without_registry` catches a bare `Exception`. Narrow it.
- **S16** — `validate_schemas.py` doesn't detect `$id` collisions; two schemas with identical `$id` would silently overwrite in the registry.
- **S17** — `validate_envelopes_in_markdown` rejects any markdown file without a fenced JSON block. Fragile if a doc file ever lands in `prompts/protocol_fewshot/`.

**Runner brittleness**
- **S10 / S15** — `subprocess.run(..., shell=True, text=True)` in `run_check` is an injection surface (trusted input today) and crashes on non-UTF-8 child output.
- **S11** — `list_tasks()` returns full dir names then `main()` splits them back to IDs. Round-trip is wasteful.
- **Br1** — t02's `grep -r compute_total` runs without an explicit path or excludes; works empirically but depends on grep's default-to-`.` behavior and the absence of cache pollution.
- **Br3** — `runner.py` derives `ROOT` as `parent.parent.parent.parent`. Move the runner one directory and it silently points elsewhere.
- **Br4** — `load_task` matches by `startswith(task_id + "_")`; if a future task is named `t1_…` (3-digit prefix), the matcher becomes ambiguous.
- **Br5** — `META_RE` is lazy; multiple `<<<atelier-meta>>>` blocks emit only the first to the sentinel validator.

**Cargo / workspace**
- **Br9 / N32 / Br11** — `[workspace.dependencies]` lists ~14 crates (tauri, ratatui, rmcp, …) that no current crate consumes. Aspirational; will churn the lockfile on `cargo update`. Either delete until needed or move to a comment block.
- **Br10** — `mcp_catalog.v1.json` `install.kind` (npm/binary/http) and `transport` (stdio/http/sse) coupling is implicit. A `transport: sse` server can only use `kind: http`.

## Phase A — Foundation
**Gate:** §1 mechanical + §11 mechanical + crash-and-recover scripted test, plus `atelier-core` drives canonical priority subset end-to-end via the §2.5 loop, plus at least one third-party MCP server registered and exercised, plus atomic-application gate green on a multi-file fixture.

### Phase A blocker decisions (ratified in v10)
- [x] Cargo workspace + `rust-toolchain.toml` (pinned 1.85.0; bumped from 1.83.0 when wiring `rmcp` into `atelier-core` — `rmcp-macros 0.1.5` requires `edition2024`) — scaffolded in repo root
- [x] Three crates scaffolded under `crates/`: `atelier-core`, `atelier-gui`, `atelier-tui`
- [x] Tauri 2.x pinned in `[workspace.dependencies]`
- [x] Diff-application atomicity: all-or-nothing per turn, no opt-out (spec §3)
- [x] Tool error taxonomy implemented in `crates/atelier-core/src/error.rs` with state-machine routing + unit tests
- [x] Credential storage decided: `keyring` primary, env-var override, interpolation tokens in MCP manifest (spec §11)

### §2.5 Agent loop / `atelier-core` scaffold
- [x] Cargo workspace; `atelier-core` crate created (lib + `error` module)
- [ ] `tokio` multi-threaded runtime; per-session actor with `mpsc` inbox + broadcast event channel
- [ ] State machine enum: `Idle / Streaming / ToolDispatching / ToolExecuting / Verifying / AwaitingUser / Failed / Done`
- [ ] Per-transition checkpoint hook (wires to §4) and ledger hook (wires to §1)
- [ ] Bounded in-turn tool parallelism via `Semaphore` (cap=4, PROVISIONAL)
- [ ] Cancellation via drop (no protocol); recovery_log capture on cancel
- [x] Tool error taxonomy + `Recovery` routing — `crates/atelier-core/src/error.rs`
- [ ] Atomic-application staging via `tempfile` + tree-sitter pre-commit validators (spec §3)
- [ ] **Mechanical gate (covered by Phase A overall):** the state machine drives t01, t02, t05, t06, t10 end-to-end against the Anthropic adapter without bypassing any transition

### §15 MCP client (Phase A)
- [ ] `rmcp` dependency added to `atelier-core`
- [ ] stdio MCP server launcher (subprocess inside §11 sandbox; `allow_net` from `mcp_servers.json`)
- [ ] HTTP / SSE MCP client (egress audited per §12)
- [ ] `mcp_servers.json` loader with validation against `schemas/config/mcp_servers.v1.json`
- [ ] Built-in tools exposed via internal MCP-compatible interface (file ops, shell, grep, ast-grep)
- [ ] Tool registration: server-level trust-budget approval on first use; per-tool `side_effect_class` from MCP tool metadata + per-server default
- [ ] MCP resources surfaced as §5 context items
- [ ] MCP prompts: deferred to v0.2
- [ ] **Mechanical gate (covered by Phase A overall):** `@modelcontextprotocol/server-filesystem` registered and exercised during canonical-workload runs

### §1 BYOM
- [ ] Adapter trait: `chat`, `stream`, `count_tokens` (with source), `capabilities()`, `conformance()` (bounded 100-call ring buffer) — *Depends on Q2*
- [ ] Capability matrix as machine-readable config + UI rendering, "claimed-but-broken" column
- [ ] Anthropic adapter
- [ ] OpenAI-compatible / LiteLLM-shaped adapter
- [ ] Context-window asymmetry: compact / reroute / `ContextOverflowError`
- [ ] Cost ledger emission per call with declared `count_tokens` source
- [ ] Latency-weighted local cost; default `$0.00028/sec`
- [ ] Conformance-driven degradation: tool-use → JSON-mode after 3 malformed calls in 20-call window
- [ ] **PROVISIONAL calibration:** conformance window/threshold against canonical workload — *Depends on Q1*
- [ ] **Mechanical gate:** canonical workload × Anthropic + LiteLLM; mock adapters exercise "absent" and "claimed-but-broken"
- [ ] UX target: mid-session provider swap preserves work

### §11 Security
- [ ] sandbox-exec `.sb` profile generator (macOS)
- [ ] bubblewrap wrapper (Linux)
- [ ] Default policy: repo-scoped FS, no network, no writes to `/etc` or `/usr/local`
- [ ] Per-tool `--allow-net` opt-in via manifest
- [ ] Out-of-repo reads gated by per-path policy
- [ ] **Mechanical gate:** `curl evil.example` blocked; logged with provenance

### §14 Persistence (crash + concurrent edits)
- [ ] Resume-at-last-completed-tool-call on restart
- [ ] In-flight stream discarded; partial preserved in `recovery_log` slot (NOT conversation history)
- [ ] File-watcher (fsevents / inotify) for read-set files
- [ ] Concurrent-edit modal at tool-call boundary (queue next dispatch, no mid-stream cancel)
- [ ] Three named options + 5-min auto-pause (PROVISIONAL)
- [ ] `--non-interactive` flag for headless contexts
- [ ] Session versioning per `schemas/versions.md`
- [ ] **Mechanical gate:** kill -9 mid-tool-call; restart; state restored

### §15 Hook contract (Phase A subset)
- [ ] Hook manifest schema (pre-tool / post-tool / on-verify-pass / on-verify-fail)
- [ ] Time-budget declaration; warn-and-continue on over-budget
- [ ] Per-hook first-use approval

---

## Phase B — Protocol and Trust
**Depends on:** Phase A green.
**Gate:** §2 mechanical + real-model conformance ≥95% (PROVISIONAL); §7 lying-agent and hallucinating-agent fixtures.

### §2 Model Protocol
- [ ] Envelope per `schemas/model_protocol/envelope.v1.json` (already written)
- [ ] Canonical system-prompt fragment + three few-shot examples at `prompts/protocol_fewshot/`
- [ ] Per-adapter few-shot override hook
- [ ] Strategy 1: native tool (`harness_meta`)
- [ ] Strategy 2: JSON-mode sentinel-bracketed appendage
- [ ] Strategy 3: regex-prose fallback
- [ ] Conformance enforcement: re-prompt on malformed, downshift after 3 failures in a turn
- [ ] Universal UI degradation policy (every consumer defines absent-field render)
- [ ] Mock-model fixtures for all three strategies
- [ ] Overhead measurement harness writes `tests/protocol/overhead.json` nightly per `schemas/protocol/overhead.v1.json`
- [ ] CI job `ci/nightly/protocol_overhead.yml` with 10%-over-7-days regression alert
- [ ] **Mechanical gate:** snapshot tests across all three strategies
- [ ] **Mechanical gate:** real-model conformance — Anthropic + OpenAI canonical workload, ≥95% (PROVISIONAL)

### §7 Verification (Phase B subset)
**Depends on:** §2 envelope stable, §11 sandbox (for shelling out to LSPs)
- [ ] DoD config schema per repo
- [ ] Did-it-do-what-it-said diff (consumes §2 `claimed_changes`)
- [ ] Hallucination detector Tier 1 — TypeScript
- [ ] LSP shell-out + auto-install prompt; decline → Tier-2 fallback; UI retry — *Depends on Q3*
- [ ] UI tier indicator
- [ ] **Mechanical gate:** lying-agent fixture flagged within 1 turn
- [ ] **Mechanical gate:** hallucinating-agent fixture flagged within 1 turn (TypeScript)

---

## Phase C — Workspace surface
**Depends on:** Phase B (UI consumes envelope).
**Gate:** 10-file rename mechanical; context-panel API assertions.

### §3 Workspace UI
- [ ] Multi-pane GUI layout
- [ ] Live diff renderer (incremental)
- [ ] Hunk accept / reject
- [ ] Hunk rewrite (GUI only; later)
- [ ] TUI subset: conv, textual diff, file tree, plan canvas tree, cost + context meters, scrubber controls `[ ] g <n>`
- [ ] Drag-and-drop (GUI only)
- [ ] Inline rendering Mermaid / D2 / images / browser previews (GUI only)
- [ ] "Why this change?" UI consuming §2 `grounding`
- [ ] **Mechanical gate:** scripted 10-file rename; live-diff incremental; final diff byte-equal
- [ ] **Mechanical gate:** TUI runs same fixture; subset snapshot
- [ ] UX target: refactor without conversation pane open

### §5 Visible context / memory / plan
- [ ] Context-panel API (token counts + why-here trace per item)
- [ ] Pin / unpin / evict with cache-bust confirm
- [ ] Memory panel: editable cards + last-used + one-click promote
- [ ] Plan canvas (editable tree; reorder, constraints, manual mark-done)
- [ ] Non-destructive compaction; expansion gated with cost disclosure
- [ ] Mental-model panel — off by default, cost-disclosed on enable
- [ ] **Mechanical gate:** API assertions for token counts and why-here; cache-bust ledger entry on eviction
- [ ] UX target: "find what agent knows about file X" median <5 s

---

## Phase D — Time and steerability
**Depends on:** Phase C (timeline scrubber lives in workspace).
**Gate:** rewind-fork-merge mechanical; 3-interrupt mechanical.

### §4 Time travel
- [ ] Checkpoint format: diff-based per-action — *Depends on Q4*
- [ ] **PROVISIONAL calibration:** 500 MB default budget — measure checkpoint size distribution; pick value retaining useful working set
- [ ] Eviction: oldest non-forked; visible prune indicator
- [ ] Timeline scrubber (GUI drag + TUI keyboard)
- [ ] Fork + branch compare
- [ ] Fork cost preview when source is in cached prefix
- [ ] `cache_bust` ledger event on fork / manual eviction / expansion
- [ ] Cherry-pick with compatibility check; "manual replay" surface
- [ ] **Tool-result fixture subsystem** — every tool call records input + output; replay reads from fixtures
- [ ] `--re-execute` replay mode with divergence report
- [ ] **Mechanical gate:** rewind-5 → fork → modify → merge passes fixture test suite
- [ ] **Mechanical gate:** replay byte-equal 3× against non-deterministic provider

### §6 Steerability
- [ ] Cancel-and-restart plumbing (mid-stream)
- [ ] Constraint pins persisted across turns
- [ ] Hot-swap plan applied at next turn
- [ ] Interrupt-with-edit injection (user message appended at restart)
- [ ] Per-tool kill switch
- [ ] **Mechanical gate:** 3 sequential interrupts; final output respects all 3 constraints

---

## Phase E — Trust calibration and uncertainty UI
**Depends on:** Phase B–D usage data for calibration. Q5 baseline capture must complete before §8 UX target can be evaluated.
**Gate:** §8 learning + per-path mechanical; §9 snapshot; §12 redaction mechanical.

### §8 Trust budgets
- [ ] Action classifier (local-safe / risky / shared / irreversible)
- [ ] **PROVISIONAL calibration:** N=20 — instrument canonical workload, pick for 80% median budget usage — *Depends on Q1*
- [ ] **PROVISIONAL calibration:** action costs (1, 20) — calibrated against same workload
- [ ] **PROVISIONAL calibration:** K=3 — false-positive rate <5% on action-shape grouping
- [ ] Refund-on-verified-pass
- [ ] Permission learning with action-shape grouping per `schemas/config/permission_shapes.v1.json` (already written)
- [ ] Per-path policy (glob-based, user-editable)
- [ ] Sandbox preview where dry-run exists; payload preview otherwise
- [ ] **Baseline capture:** *Depends on Q5*
- [ ] **Mechanical gate:** approval-shape learning after 3 same-shape approvals
- [ ] **Mechanical gate:** per-path policy gates `migrations/**` and passes `src/**`
- [ ] UX target: prompt count ≤30% of baseline

### §9 Uncertainty UI
- [ ] Grounding badges (green/yellow/red) from §2 `grounding`
- [ ] Gray-state rendering when §2 unavailable
- [ ] Uncertainty signal → human-input prompt
- [ ] Mental-model panel (link to §5)
- [ ] "Proceeded under protest" record via §2 structured override
- [ ] **Mechanical gate:** mock model emits one uncertainty + one `guess` grounding; UI snapshot matches

### §12 Privacy
- [ ] Per-call record per `schemas/audit/egress.v1.json` (already written)
- [ ] Per-path redaction defaults
- [ ] Local-only mode
- [ ] **Mechanical gate:** `.env` content blocked; placeholders sent

### §13 Telemetry
- [ ] Three opt-in channels per `schemas/telemetry/payload.v1.json` (already written) — *Depends on Q6*
- [ ] Pre-send inspector UI
- [ ] 90-day retention at collector; export-and-delete endpoint
- [ ] Hard guarantee: no prompt content in any channel

---

## Phase F — Deferred
- [ ] §1 OpenAI adapter; Ollama / llama.cpp / MLX-LM adapters; per-task routing
- [ ] §7 Tier 1 — Go, Rust, Java, C#
- [ ] §7 Tier 2 — Python, Ruby, PHP
- [ ] §7 Tier 3 — shell, config DSLs
- [ ] §7 Auto-scaffolding with model-family tag, 7-day same-family refusal (PROVISIONAL)
- [ ] §10 Multi-agent
- [ ] §15 Tool plug-in manifest; community adapter packaging

---

## DoD checklist mirror

### Backend milestone — Phase A + B (internal; not user-facing)
- [ ] Phase A gate green
- [ ] Phase B gate green
- [ ] Schema validation passing for every Phase A/B artifact
- [ ] Canonical workload priority subset (t01, t02, t05, t06, t10) completes against Anthropic + LiteLLM via API
- [ ] Crash-and-recover preserves state

### First user-facing release — Phase A + B + C
- [ ] Backend milestone met
- [ ] §3 GUI 10-file rename gate green
- [ ] §5 context-panel API assertions green
- [ ] Cold start GUI <4 s

### Full v1
- [ ] All pillar mechanical gates green
- [ ] Canonical workload completes against Anthropic, OpenAI, local Qwen
- [ ] §8 ≤30% UX target met
- [ ] Performance budgets met
- [ ] All PROVISIONAL parameters replaced with calibrated values

---

## Lessons captured during spec evolution

1. Hard tradeoffs documented but not decided are blockers, not options.
2. "Mechanically verifiable" requires a runnable script; UX targets need user studies and shouldn't gate ship.
3. Cross-pillar protocol assumptions hide until extracted as their own pillar.
4. Cache-bust cost is invisible unless ledgered.
5. Provider abstractions overpromise; calibrate features to backend reality.
6. Token-spending background tasks default off, with cost disclosure on enable.
7. Mock-model gates prove the mock, not the model; need real-model conformance gates too.
8. Numbers without calibration plans are guesses with extra steps.
9. Replay determinism requires tool-result fixtures.
10. A linear build order hides the DAG; phase grouping is honest.
11. `local-free` distorts routing; default the rate to a defensible non-zero value.
12. A spec without a v0.1 is a wishlist; phase-based grouping makes v0.1 implicit instead of separate.
13. Citation tables without real sources are placeholders pretending to be citations; better omitted than faked.
14. Each spec revision adds material; periodically remove rather than only add — schemas externalize, prose tightens, sections collapse.
15. The calibration workload is the gate-unblocker; without it, no number can be set and no UX target can be evaluated. Write it first.
