# Phase B closeout plan

Mirror of the v60.13 Phase A close-out plan shape (Tracks A/B/C). Use this file as the single source of truth while Phase B is in flight; once green, fold the green ticks back into `tasks/todo.md` and archive this file.

## Gate text

Per `coding-harness-spec.md:866` and `tasks/todo.md:206`:

> **§2 mechanical + real-model conformance ≥95% (PROVISIONAL); §7 lying-agent and hallucinating-agent fixtures.**

## Current state (as of v60.19)

- §2 mechanical gate (`todo.md:220`) — `[~]` partial. Pure-function tests pass; end-to-end mock-driven gate landed but doesn't yet cover all three strategies as a snapshot suite.
- §2 real-model conformance ≥95% (`todo.md:221`) — `[ ]` open. Anthropic live runs landed v60.18 but the conformance number is not formally measured + asserted. OpenAI-compat live half hasn't run.
- §7 lying-agent gate (`todo.md:230`) — `[x]` green at v60.12.
- §7 hallucinating-agent gate (`todo.md:231`) — `[ ]` open. Depends on Tier-1 LSP detector (`todo.md:227`) and LSP auto-install plumbing (`todo.md:228`). Q3 was resolved at v60.12 as prompt-on-first-use; the design is settled, the implementation is not.

## Acceptance criteria (gate green when all hold)

1. `tasks/todo.md:220` flips `[~]` → `[x]` — §2 end-to-end snapshot tests green across all three strategies against MockAdapter.
2. `tasks/todo.md:221` flips `[ ]` → `[x]` — measured real-model conformance recorded against Anthropic + OpenAI-compat, asserted against the calibrated threshold (see Track A).
3. `tasks/todo.md:227, 228, 231` flip `[ ]` → `[x]` — TypeScript Tier-1 detector + LSP auto-install prompt + Tier-2 decline-fallback + hallucinating-agent fixture all green.
4. DoD checklist mirror at `tasks/todo.md:429-446` reconciled with the per-phase truth.
5. `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && make check` all green.
6. Phase B nightly gate (`phase_b_gate_status` binary, sibling of v60.13's `phase_a_gate_status`) emits `Phase B: GREEN`.

## Pre-work decisions (ratified)

| # | Decision | Choice | Rationale |
|---|---|---|---|
| 1 | LSP client crate | `async-lsp`, gated on `experiments/lsp_spike/` | Same shape as the v60.10 rmcp spike. Avoid hand-rolling JSON-RPC framing + lifecycle for one phase's gate. Bail to hand-rolled if the spike surfaces a rmcp-0.1.5-class set of smells. |
| 2 | OpenAI-compat target for nightly | Hosted OpenAI via `secrets.OPENAI_API_KEY` | Cheapest reliable + CI-friendly path. Same secret-gated step shape as v60.19's Anthropic gate; records `skipped` (not `failed`) when the secret is absent. `--base-url` keeps LiteLLM / Ollama runnable locally. |
| 3 | Conformance threshold | Record-for-7-nights, then assert at `max(0.95, observed_p5)` | Applies **L-D-6**. The spec's 95% is PROVISIONAL; the calibration window turns it into a measured floor. First-run-records is the only way to find out whether 95% is even achievable on Haiku 4.5 — it might need to be higher (it cleared 19/20 turns on t01) or might need temporarily lower while we calibrate. |

## Work tracks

Five tracks: A, B, C, D parallelisable on day 1; C splits into C1 → C2 → C3 sequentially. Total estimated wall-clock: 3–4 working days assuming the parallel bundles run in worktrees per the v60.10/v60.11 pattern.

### Sequential prep commit (lands first, ~1 hour)

Per **L-D-2** — any PR that introduces a new `Event::*` variant lands sequentially in a prep commit with empty match arms in all four sinks, so the four parallel bundles don't collide on `session.rs::Event`, GUI `bridge_event`, Svelte `state.ts applyEvent`, or TUI `apply`/`project_event`.

**Prep commit lands:**
- `Event::RequestLspInstall { language: String, candidate_packages: Vec<String> }` (C1)
- `Event::LspInstallResolved { language: String, outcome: LspInstallOutcome }` (C1)
- `Event::VerificationFailed` already exists from v60.12; no change needed for C3.
- All four sinks (`bridge_event`, `state.ts applyEvent`, `state.ts projectEvent`, TUI `apply` + `project_event`) get empty match arms.

### Track A — §2 real-model conformance harness + nightly gate

**Files:**
- `crates/atelier-core/src/protocol_conformance.rs` — extend with `ConformanceSummary { strategy, total_turns, malformed_turns, rate }` projection.
- `crates/atelier-cli/src/bin/conformance_status.rs` (new) — sibling of `phase_a_gate_status`.
- `schemas/ci/protocol_conformance.v1.json` (new) — typed artifact.
- `tests/phase_b_gate/last_run.json` (new) — schema-conforming nightly result, committed back.
- `.github/workflows/nightly_phase_b_gate.yml` (new) — separate from Phase A nightly so its failure surface is independent.

**What it does:** runs the canonical priority subset (t01, t02, t05, t06, t10) against `anthropic:claude-haiku-4-5` and a hosted OpenAI model, captures `ProtocolConformance` snapshots per strategy per run, writes the per-strategy rate to the artifact. **Weeks 1**: records-only (the `assert` step writes the result but always passes; calibration phase). **Week 2 onwards**: asserts against `max(0.95, observed_p5)` computed from the rolling 7-day window.

**Lessons applied:**
- **L-D-1** — live API calls from day 1 (~$0.20/night Anthropic + ~$0.50/night OpenAI per v60.18 cost data).
- **L-D-5** — `ConformanceSummary` and `ConformanceStatus` (Green/Yellow/Red) get `wire_label()` ↔ serde agreement tests in the first commit.
- **L-D-6** — calibration job lands in the same PR. Const lives next to `DEFAULT_DEGRADATION_THRESHOLD` in `protocol_conformance.rs` with `// PROVISIONAL: 95% floor per spec, calibrated against observed_p5 over rolling 7-day window. See tests/phase_b_gate/last_run.json.`

**Definition of done:** `phase_b_gate_status` binary prints `Phase B §2: GREEN (N runs, p5 rate R%)`; nightly workflow green on `main`.

### Track B — Live OpenAI-compat canonical runs

**Files:**
- `crates/atelier-cli/tests/run_integration.rs` — five new `phase_b_live_openai_compat_t<NN>_*` `#[ignore]`-gated tests, mirroring the v60.18 Anthropic shape.
- `.github/workflows/nightly_phase_b_gate.yml` (extends Track A) — `secrets.OPENAI_API_KEY`-gated step running the five tests against hosted `gpt-4o-mini` or similar cheap-and-capable model.

**Lessons applied:**
- **L-D-1** — `#[ignore]`-gated live tests from day 1; nightly records `skipped` when secret absent (v60.19 pattern).
- **L-D-8** — same five fixtures (t01, t02, t05, t06, t10) run against both Anthropic and OpenAI-compat. Surfaces per-adapter quirks before they become Phase E debugging cost.

**Choice point — confirm before starting:** hosted OpenAI vs LiteLLM proxy. Default = hosted OpenAI for simplicity; LiteLLM is the literal DoD line at `todo.md:432` but adds proxy-stand-up cost. Either satisfies the gate text.

**Definition of done:** five `phase_b_live_openai_compat_t<NN>_*` tests green locally; nightly workflow `phase_b_live_openai_compat` gate records `passed` or `skipped` (never `failed` unless OPENAI_API_KEY is present and the test actually fails).

### Track C1 — LSP client infrastructure (spike + foundation)

**Files (new module):**
- `experiments/lsp_spike/` (new directory) — spike against `async-lsp = "0.2"` + `typescript-language-server` over stdio. Mirror `experiments/rmcp_spike/`'s decision matrix: handshake latency, crash recovery, shutdown reliability, type-system fit, known smells. Outcome is **GO** / **GO-WITH-CAVEATS** / **NO-GO**, written to `experiments/lsp_spike/README.md`.
- `crates/atelier-core/src/lsp/mod.rs` (new) — `LspServerHandle`, `launch_typescript_server`, `shutdown` mirror of `mcp::stdio_launcher`.
- `crates/atelier-core/src/lsp/approval.rs` (new) — `LspApprovals` mirror of v60.8's `McpApprovals` bit-for-bit: persistent map of `language → approval_timestamp` at `<workspace>/.atelier/lsp/_approvals.json`.
- `schemas/audit/lsp_install.v1.json` (new) — install-event audit shape.

**Lessons applied:**
- **L-D-3** — `LspInstallOutcome::{Installed, Declined, AlreadyPresent, Failed}` is a tier-fallback shape; reuse `VerificationTier` ladder pattern (typed enum + `wire_label()` + serde agreement + bus event for "fallback was used").
- **L-D-5** — `LspInstallOutcome` and `LspClientError` get `wire_label()` ↔ serde agreement tests in the first commit.
- **L-D-7** — integration test asserting the wire: scripted run → `RequestLspInstall` fires → approval persists → install runs in §11 sandbox → `LspInstallResolved { Installed }` fires → re-run skips the prompt. Decline arm: `Declined` fires → next verify uses `Tier2TreeSitter` observable on the bus.
- **L-D-1** — `#[ignore]`-gated live test against `typescript-language-server` installed locally, mirroring `mcp_integration_npx`.

**Definition of done:** approval store round-trips; install runs sandboxed via the existing `subprocess::sandboxed_argv` helper (`allow_net: true` only during install, not during use); decline-falls-back-to-Tier-2 observable on the bus.

### Track C2 — TypeScript Tier-1 verify path *(sequential after C1)*

**Files:**
- `crates/atelier-core/src/verify.rs` — flesh out the `Tier1Lsp` producer (v60.8 wire-reserved it).
- `crates/atelier-core/src/lsp/typescript.rs` (new) — subprocess wrapper around `typescript-language-server`; `textDocument/publishDiagnostics` consumer; maps LSP diagnostics into `Discrepancy::HallucinatedSymbol { path, span, symbol, lsp_message }`.
- `crates/atelier-cli/src/runner.rs` — **fix the v60.8 follow-on**: actually call `dispatcher.verify_pass()` instead of just transitioning to `State::Verifying`. The current Runner transitions but doesn't invoke verify — this is exactly the **L-D-7** "claimed but broken" surface the lessons file warns about.

**Lessons applied:**
- **L-D-7** — Tier-1 producer AND the runner wiring land in the *same* commit. Otherwise the tier indicator stays half-wired.
- **L-D-8** — once Tier 1 lands, re-run t01/t02/t05/t06/t10 against both Anthropic and OpenAI-compat to verify the detector doesn't false-fire on honest agents (lying-agent test from v60.12 is the negative-control half; need a positive-control regression too).

**New discrepancy variant — locked before code lands:**

```rust
enum Discrepancy {
    Claimed { path: PathBuf },                // v60.12
    Unclaimed { path: PathBuf },              // v60.12
    KindMismatch { path: PathBuf, .. },       // v60.12
    DuplicateClaim { path: PathBuf },         // v60.12
    HallucinatedSymbol {                       // NEW (Phase B)
        path: PathBuf,
        line: u32,
        column: u32,
        symbol: String,
        lsp_message: String,
    },
}
```

**Definition of done:** Tier-1 verify path runs for `.ts` files in the canonical fixtures; Tier-3 textual remains the decline-fallback. Runner integration test asserts `verify_pass` is actually called (regression for the v60.8 follow-on).

### Track C3 — Hallucinating-agent fixture + gate *(sequential after C2)*

**Files:**
- `tests/workload/canonical/t14_hallucinating_agent_typescript/` (new) — fixture with a `.ts` file where the envelope claims to call a method that doesn't exist on the target type.
- `crates/atelier-cli/tests/run_integration.rs` — new `mock_hallucinating_agent_fixture_flagged_within_one_turn_phase_b_seven_gate` test, mirroring v60.12's `mock_lying_agent_fixture_flagged_within_one_turn_phase_a_seven_gate` shape.

**Fixture shape:** TypeScript file with a known type; envelope's `claimed_changes` reference a call site like `foo.nonExistentMethod()` where `foo`'s type has no `nonExistentMethod`. Tier 1 catches it via `typescript-language-server`'s `Property 'nonExistentMethod' does not exist on type 'Foo'` diagnostic. Tier 3 textual *would not* — that's the whole point of Tier 1.

**Test asserts:**
- `Event::VerificationFailed { tier: Tier1Lsp, discrepancies }` fires within one turn.
- `discrepancies` contains exactly one `HallucinatedSymbol { symbol: "nonExistentMethod", lsp_message: <contains "does not exist on type 'Foo'"> }`.
- The v60.12 `mock_lying_agent_fixture_flagged_within_one_turn_phase_a_seven_gate` still passes (no regression on Tier 3).

**Lessons applied:**
- **L-D-9** — write the lying-vs-hallucinating priority lattice as a table: a turn that triggers both (wrong file *and* wrong type) emits all matching discrepancies; no discrepancy variant takes priority over another, but **`Event::VerificationFailed { tier }`** uses the *highest tier that ran* (Tier 1 if Tier 1 ran, regardless of whether Tier 3 also would have failed). Pin via a paired table-driven test.

**Definition of done:** the fixture flags within one turn; v60.12 lying-agent test still passes; both gates surface on the same bus arm without conflict.

### Track D — §2 mechanical-gate completion + bookkeeping sweep

**Files:**
- `crates/atelier-cli/tests/run_integration.rs` — three new tests: `mock_drives_t01_via_strategy_native_tool_phase_b_two_gate`, `…via_strategy_json_sentinel…`, `…via_strategy_regex_prose…`. Each scripts a `MockAdapter` emitting envelopes via the named strategy, runs t01 through the §2.5 loop, asserts `final_state == Done` + `verification_passed`.
- `tasks/todo.md:220` flips `[~]` → `[x]`.
- `tasks/todo.md:429-446` — reconcile the DoD checklist mirror against per-phase truth. Phase A gate green, §3 GUI 10-file gate green, §5 context-panel API assertions green, Phase C closed.

**Lessons applied:**
- **L-D-7** — three strategies × end-to-end run, not just round-trip on the encoder. The pure-function tests already pass; what's missing is the integration coverage.

**Definition of done:** three new tests green; DoD checklist mirror matches reality.

## Suggested timeline

```
Day 0   ──────── Prep commit (Event variants, empty match arms)
Day 1   ┌── Track A (conformance harness)
        ├── Track B (OpenAI-compat live runs)
        ├── Track C1 (LSP spike + foundation)
        └── Track D (mock gate + bookkeeping)
Day 2   ──────── Track C2 (TypeScript Tier-1 verify path)
Day 3   ──────── Track C3 (hallucinating fixture)
Day 4   ──────── Track A enters records-only week
                 Phase B gate validation + sign-off
Day 11  ──────── Track A flips records-only → asserts (after 7-night calibration)
```

Four parallel bundles on day 1 (file-disjoint per **L-D-2**); C2 + C3 sequential because they each consume the previous track's surface. The 7-night calibration window for Track A's threshold runs in the background — Phase B gate text doesn't require the threshold to be asserted, only that the conformance rate is *measured*; assertion is the harder L-D-6 discipline we want to land regardless.

## Risk register

| Risk | Mitigation |
|---|---|
| `async-lsp` shows rmcp-0.1.5-class smells | Spike runs first (Track C1 prefix); bail to hand-rolled if needed. Spike budget: 4 hours wall-clock. |
| Haiku 4.5 conformance rate < 95% | The calibration window surfaces this — if observed p5 is e.g. 91%, the threshold becomes `max(0.95, 0.91) = 0.95` and we have a real bug to chase (likely few-shot or system-prompt tuning). |
| TypeScript LSP install fails on CI (npm flakiness) | Same posture as `mcp_integration_npx` — gate is informational (records `skipped` on install failure, not `failed`). |
| Sequential prep commit collides with in-flight work | Land prep commit on a quiet `main` (no parallel-bundle work in flight). |
| Worktree-isolation drift (per v60.10 candor) | Apply **L-D-10** — when this plan executes, the CI quality-cheap job greps `git log --grep="Merge .*: Track <X>"` against bundle claims. |

## Lessons map (cross-reference)

| Lesson | Where it's applied in Phase B |
|---|---|
| **L-D-1** Live-probe muscle | Tracks A, B, C1 (npx-gated TS LSP test) |
| **L-D-2** File-disjoint parallel | Prep commit for new `Event::*` variants; four bundles on day 1 |
| **L-D-3** Tier/fallback ladder | `LspInstallOutcome` enum (C1); existing `VerificationTier` ladder consumed by C2 |
| **L-D-4** atomic_write everywhere | `LspApprovals` persistence (C1) routes through `atomic_write` |
| **L-D-5** wire_label agreement test | `ConformanceSummary`, `ConformanceStatus` (A), `LspInstallOutcome`, `LspClientError` (C1), `Discrepancy::HallucinatedSymbol` wire shape (C2) |
| **L-D-6** PROVISIONAL + nightly calibration | Track A's `max(0.95, observed_p5)` calibration window |
| **L-D-7** Claimed-but-broken surfaces | C2 fixes the v60.8 `verify_pass` follow-on while landing Tier 1 |
| **L-D-8** Multi-adapter parity | Tracks A + B run the same five fixtures against both adapters |
| **L-D-9** Priority lattices on day 1 | C3 locks lying-vs-hallucinating discrepancy precedence |
| **L-D-10** Worktree-isolation hygiene | Apply at execution time, not specified here |

## When this plan is done

- Move green ticks back into `tasks/todo.md` (Phase B section).
- Update `CLAUDE.md` / `ATELIER.md` if the project description needs to flip from "Phase A close" to "Phase B close."
- Archive this file (rename to `tasks/archive/phase_b_closeout.md` or delete; the CHANGELOG entries will carry the trail).
- Write the CHANGELOG entry: `v60.NN — Phase B gate green (§2 conformance + §7 hallucinating-agent)`.
