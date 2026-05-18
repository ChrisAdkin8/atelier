# Atelier Spec — Changelog

## v60.13 — 2026-05-18 (Track A: §15 built-ins-as-MCP surface-symmetry refactor + Track C: Phase A nightly gate workflow)

Closes Tracks A and C from the Phase A close-out plan. A sibling `BuiltInToolWrapper` mirrors `McpToolWrapper`'s exact shape so the two registration paths converge at the dispatcher boundary (Track A). A new `.github/workflows/nightly_phase_a_gate.yml` runs the mechanical Phase A gates every night, records pass/fail to `tests/phase_a_gate/last_run.json` per a new `schemas/ci/phase_a_gate.v1.json`, commits the artifact back, and surfaces a one-line digest via the new `phase_a_gate_status` binary (Track C). Workspace tests **1020 → 1038** (+18; +11 wrapper/register from A, +7 status binary from C).

### Track C — Phase A nightly gate workflow

The nightly fires at 06:30 UTC (30 minutes after `nightly_protocol_overhead.yml` so the two `git push origin HEAD:main` calls don't race) and walks five gates with `continue-on-error: true` so one failure doesn't short-circuit the rest:

1. **`fmt`** — `cargo fmt --all -- --check`
2. **`clippy`** — `cargo clippy --workspace --all-targets -- -D warnings`
3. **`cargo_test_workspace`** — `cargo test --workspace`
4. **`rig_check`** — `make check` (schema meta-validation + artifact validation + 112 rig tests + 13 canonical workloads)
5. **`mcp_integration_npx`** — `cargo test -p atelier-cli --test mcp_integration -- --include-ignored` (the npx-gated MCP integration suite). **Informational, not red** — npm-registry flakiness shouldn't flip Phase A; the digest surfaces a failure but `all_passed` stays true.

Each step captures its exit code + wall-clock duration into a step output; a final `compose` step assembles `tests/phase_a_gate/last_run.json` against the schema, validates the fresh artifact via `tests/validate_artifacts.py`, commits + pushes to `main`, and uploads the `phase_a_gate_status` binary's one-line digest into the run's `GITHUB_STEP_SUMMARY`. A red gate also surfaces as a `::error::` annotation on the workflow run so it's visible on the actions tab without drilling into per-step logs.

### `schemas/ci/phase_a_gate.v1.json`

New schema family (`schemas/ci/` directory is new — sits alongside `schemas/protocol/` and `schemas/audit/`). Required fields: `version`, `run_id` (RFC 3339), `git_sha` (7-40 hex), `all_passed` (boolean — separately stored from the gate array so a reader can short-circuit), `gates: array of {name, status, ?duration_secs, ?details}`. `status` is one of `passed | failed | skipped`. `name` must be `^[a-z][a-z0-9_]*$` so a future analytics tooling can rely on the snake_case shape. `details` capped at 1 KiB so a malformed gate can't bloat the artifact.

Wired into `tests/validate_artifacts.py`'s `JSON_RULES` table so every PR's `make check` validates the file against the schema — a schema break is caught synchronously, not only on the next nightly firing.

### `crates/atelier-cli/src/bin/phase_a_gate_status.rs` (new binary)

Single-purpose reader, ~250 lines including tests:

- Accepts an optional positional path argument; defaults to `tests/phase_a_gate/last_run.json` resolved relative to `CARGO_MANIFEST_DIR` at build time.
- Prints two lines: a per-gate digest (`<run_id> <git_sha> <name>:<status> …`) plus a `Phase A: GREEN | RED  (N gates: P passed, F failed, S skipped)` digest.
- Exit codes: **0** = green, **1** = red (≥1 `failed`), **2** = artifact missing / malformed / unsupported version.
- A `failed` gate's `details` field is surfaced on stderr so a CI summary picks it up without parsing the JSON.
- 7 new unit tests via `tempfile`: `green_run_exits_zero`, `red_run_exits_one`, `missing_file_exits_two`, `malformed_json_exits_two`, `unsupported_version_exits_two`, `tally_counts_each_status`, `bundled_seed_artifact_parses` (drift gate against the in-tree seed).

Why a separate binary rather than an `atelier <subcommand>`: the nightly runs it with no harness state in scope (no session, no adapter); building a full `atelier` invocation for what is a 30-line JSON read would be wasteful. Cargo auto-discovers `src/bin/*.rs` so no `Cargo.toml` change is needed.

### Seed `tests/phase_a_gate/last_run.json`

One row per gate at `passed` status (with the `mcp_integration_npx` row marked `skipped` because the seed predates the first nightly firing). Subsequent nightly runs overwrite this file in place; the seed is committed so the workflow has something to validate against on its first run and so the `phase_a_gate_status` binary doesn't 404 in a fresh clone.

### Track A — surface symmetry — same shape as `McpToolWrapper`

### Surface symmetry — same shape as `McpToolWrapper`

The spec §15 invariant ("built-in tools and MCP-routed tools share the same `ToolDispatching → ToolExecuting` state transitions; the loop does not branch on tool origin") was already runtime-true at v60.11 — both registration paths hand the dispatcher an `Arc<dyn Tool>`. What v60.13 adds is **construction-time symmetry**: the bundled `tool_manifest.v1.json` files at `crates/atelier-core/tools/*.v1.json` are now the source of truth for `name`, `description`, `side_effect_class` and `input_schema` for built-ins, exactly as the server-advertised `tools/list` payload is for MCP-routed tools.

New module `crates/atelier-core/src/tools/builtin_wrapper.rs` (~340 lines including tests):

- `BuiltInToolWrapper` struct: holds `name`, `description`, `side_effect_class`, `input_schema: Value`, compiled `Arc<jsonschema::Validator>`, and `inner: Arc<dyn Tool>`. `impl Tool` delegates `execute` to the inner; `validate_args` runs the manifest's JSONSchema validator first (catches patterns / ranges / `oneOf` / `additionalProperties:false` that serde can't always express) THEN delegates to the inner.
- `BuiltInToolWrapper::from_manifest_json(manifest_json: &str, inner: Arc<dyn Tool>) -> Result<Self, BuiltInWrapError>` — parses the manifest, compiles the schema, asserts `parsed.name == inner.name()` and fails as `BuiltInWrapError::NameMismatch` otherwise so manifest/impl drift is a startup failure, not a silent dispatch error.
- `BuiltInWrapError` enum: `ManifestParse(String)`, `SchemaCompile(String)`, `NameMismatch { manifest, inner }`.
- The wrapper reuses `crate::mcp::mcp_tool::{compile_input_schema, validate_args_against}` so a future revision of the JSONSchema compilation path touches both wrappers in one place.

### `register_builtins` — `register_mcp_servers` sibling

`crates/atelier-core/src/tools/mod.rs::register_builtins(registry: &mut ToolRegistry) -> Result<RegisterBuiltinsReport, RegisterBuiltinsError>` walks a static 7-row `builtin_table()` (each row: name + `include_str!`-loaded manifest JSON + `Arc<dyn Tool>` inner), builds a wrapper per row via `BuiltInToolWrapper::from_manifest_json`, and registers each into the registry. Returns `RegisterBuiltinsReport { tools_registered: Vec<String> }` so the runner can ledger the registration alongside MCP-server registrations in one report shape.

`spawn_subagent.v1.json` exists in the manifest set but its Rust `Tool` impl hasn't landed (§10 delegation work) — the table leaves it out until the impl arrives; the manifest's existence is a forward-looking surface contract.

The runner's `crates/atelier-cli/src/runner.rs::built_in_registry()` is now a 4-line delegation to `register_builtins`. Direct imports of the seven tool structs from the runner go away; the `Tool` import becomes unused and is dropped from the use-list.

### Tests — 11 new

- `builtin_wrapper::tests` (8) — `name_comes_from_manifest_not_inner`, `side_effect_class_comes_from_manifest_not_inner`, `name_mismatch_rejected`, `malformed_manifest_rejected`, `invalid_schema_rejected`, `validate_args_runs_manifest_schema`, `execute_delegates_to_inner`, `all_bundled_manifests_parse` (drift gate: every one of the 7 bundled manifests parses + its schema compiles).
- `tools::register_tests` (3) — `register_builtins_registers_all_seven_with_correct_metadata` (asserts the registration order matches the table + spot-checks `read_file: LocalSafe`, `write_file: LocalRisky`, `shell: LocalRisky`), `register_builtins_is_idempotent_only_once` (a second call surfaces as `RegisterBuiltinsError::Register` rather than silently overwriting), `wrapper_rejects_unknown_field_via_manifest_schema` (the JSONSchema gate rejects `additionalProperties:false` violations ahead of the inner impl).

The seven inner `Tool` impls (`ReadFile`, `WriteFile`, `EditFile`, `ListDir`, `Grep`, `AstGrep`, `Shell`) and their ~30 existing unit tests are untouched — the refactor is purely additive at the inner layer. The 794 atelier-core tests + 72 atelier-cli tests + 94 atelier-gui tests + the TUI suite all stay green; `make check` runs all 112 rig tests + 13 canonical workloads + 57 artifacts.

### Why not literal in-process MCP for the built-ins

A literal in-process MCP transport for the built-ins (so they'd share `McpToolWrapper` not just its shape) was considered and rejected:

1. rmcp 0.1.5 has no in-process transport — only stdio + SSE. Wrapping each built-in in a `tokio::process::Command` spawn just to round-trip through rmcp's JSON-RPC framing is pure overhead.
2. Three built-ins (`write_file`, `edit_file`, `shell`) carry handles to in-process state — `Staging` (TempDir-owning), `SandboxPolicy`, the audit-log path — that don't cross an MCP boundary cleanly. The MCP server's view of the workspace would need to either rebuild these or take them by side-channel, neither of which is in scope.
3. No current consumer wants atelier embedded as a sub-process for another harness. If/when that lands, the v60.13 wrapper symmetry is the right shape to start from.

The wrapper symmetry buys the spec §15 promise (uniform dispatch shape) without paying for the speculative IPC layer.

## v60.12 — 2026-05-18 (Phase A close: canonical priority subset offline gates + §7 lying-agent E2E)

Phase A's "atelier-core drives canonical priority subset end-to-end via the §2.5 loop" line lands offline against `ProviderChoice::Mock`, and the §7 lying-agent gate (`tasks/todo.md:228`) closes after a real fix in `dispatcher::verify_pass`. Live-API gates (against Anthropic + OpenAI-compat) and the nightly workflow remain for follow-up Track B + Track C. Workspace tests **1018 → 1020** (+1 paired dispatcher unit test for the new branch, +6 new integration tests in atelier-cli, –5 reused slots = +2 net at the suite-level summary).

### A1 — canonical fixture loader

New test-helper module under `crates/atelier-cli/tests/common/` — first Rust consumer of `tests/workload/canonical/`.

- `tests/common/mod.rs` (8 lines) — declares `pub mod canonical` with `#![allow(dead_code)]` so per-integration-test-file unused-warning noise stays quiet.
- `tests/common/canonical.rs` (~270 lines) — `CanonicalTask::load("t01_…")` reads `meta.json` + `prompt.md` + `checks.json`; resolves the workspace path via `CARGO_MANIFEST_DIR`. Helpers: `copy_fixture_to_tempdir`, `run_checks`, `assert_all_checks_pass`, `python3_pytest_available`. Supports `command` + `exit_code(_ne)` + `stdout/stderr_contains` + `file_unchanged` (the primitives the priority subset uses); `stdout_pattern`/`stderr_pattern` surface as a failing `CheckResult` rather than passing silently (no priority canonical task depends on them today).
- `run_checks` removes `<workspace>/.atelier/` before running shell-based checks — the Runner writes `.atelier/sessions/<sid>/session.json` (containing the prompt verbatim) during a real run, which trips `grep -r` checks like t02's "no occurrence of `compute_total` remains." The Python rig dodges this with `--dry-run`; the Rust runner is hermetic so it removes the bookkeeping directly. No canonical fixture's expected state includes `.atelier/`, so the cleanup is sound.

### A2 — t01 mock-scripted canonical gate

`mock_drives_t01_canonical_priority_subset_offline_phase_a_gate` in `crates/atelier-cli/tests/run_integration.rs`. Loads `t01_add_pure_function`, scripts one `MockResponse` that writes `utils.py` (the `divisible_by` impl) + `tests/test_utils.py` (four tests) + `mock_envelope_tool_call(envelope_done_claiming_edits(&["utils.py", "tests/test_utils.py"]))`, drives the Runner, asserts `final_state == Done`, asserts `Event::VerificationPassed { tier: Tier3Textual, file_count: 2, .. }` fires, runs all 5 t01 canonical checks (pytest exit 0 + the four `divisible_by(…)` per-call assertions).

Skips cleanly when `python3 -m pytest` is unavailable via the new `python3_pytest_available()` probe (mirrors `mcp_integration.rs::npx_availability_probe`).

### A3 — t02, t05, t06, t10 mock-scripted canonical gates

Four more priority canonical tasks, same shape as A2:

- **t02 `rename_symbol_multi_file`** — nine `write_file` calls in one turn renaming `compute_total` → `compute_grand_total` across `README.md` + 5 `orders/` modules + 3 `tests/` modules. The check `grep -r compute_total` must return non-zero (no match); pytest must still pass.
- **t05 `fix_bug_from_failing_test`** — patches `format_duration` to handle the `minutes == 0` case (returning `"2h"` not `"2h0m"`). The check `file_unchanged: tests/test_duration.py` mechanically verifies the agent didn't modify the spec.
- **t06 `add_cli_flag`** — adds `--verbose` to `mycli.py` + new tests in `tests/test_mycli.py`. Both existing-test-passes and new-flag-works are asserted.
- **t10 `implement_from_spec`** — implements `LRUCache` (OrderedDict-backed, O(1)) against the seven-test spec in `tests/test_lru.py` (which is `file_unchanged`-pinned).

New helper `envelope_done_claiming_edits(&[paths])` mints an honest envelope whose `claimed_changes` cover every modified path as `ClaimedChangeKind::Edit` — the §7 gate's `verify::compare` treats Edit-vs-Modified as agreement, so the loop reaches `VerificationPassed` (rather than `VerificationFailed` for a silent edit, which the lying-agent gate covers separately).

### A4 — §7 lying-agent E2E gate (closes `tasks/todo.md:228`)

Real fix to a latent bug: `dispatcher::verify_pass` previously emitted `Event::VerificationPassed` *regardless* of whether `crate::verify::compare` returned discrepancies — the §7 detector logic existed but its signal never reached the bus. v60.12 wires it.

**Producer-side change** (`crates/atelier-core/src/dispatcher.rs`):

```rust
if run.discrepancies.is_empty() {
    let _ = self.events.send(Event::VerificationPassed { tier, file_count, claim_count });
} else {
    let _ = self.events.send(Event::VerificationFailed { tier, discrepancies: run.discrepancies.clone() });
}
```

**New event variant** (`crates/atelier-core/src/session.rs`): `Event::VerificationFailed { tier, discrepancies: Vec<Discrepancy> }`. The `kind()` arm returns `"VerificationFailed"`.

**Consumer arms**:

- **TUI** (`crates/atelier-tui/src/lib.rs`) — `apply` refreshes `verification_status` with the new tier (so the badge knows verify ran); `project_event` builds a one-line summary `"tier-3 (textual) · 2 discrepancies · a.txt: claimed edit but workspace diff is empty"` for the event log. The red-failed badge variant lands in Phase C.
- **GUI bridge** (`crates/atelier-gui/src/lib.rs::bridge_event`) — emits a `{"tier": …, "discrepancy_count": N, "discrepancies": [{"kind": "claimed" | "unclaimed" | "kind_mismatch" | "duplicate_claim", "path": …, …}]}` JSON payload to the Svelte side. Wire shape is stable; the GUI badge update lands in Phase C.
- **`ObservedKind::wire_label`** is now public (previously `as_str` was private), mirroring `VerificationTier::wire_label` and `ClaimedChangeKind::wire_label`, so cross-crate consumers don't need to re-encode the enum.

**End-to-end gate** (`crates/atelier-cli/tests/run_integration.rs`): `mock_lying_agent_fixture_flagged_within_one_turn_phase_a_seven_gate` scripts an envelope claiming `a.txt` while the actual tool call writes `b.txt`. Asserts within one turn: `Event::VerificationFailed { tier: Tier3Textual, discrepancies }` fires, `VerificationPassed` does NOT fire, `discrepancies` carries both `Discrepancy::Claimed { a.txt }` and `Discrepancy::Unclaimed { b.txt }`. Reaches `State::Done` — the §7 gate surfaces the signal but doesn't abort the run (trust budget consumes the discrepancy list downstream).

**Paired unit tests** (`crates/atelier-core/src/dispatcher.rs::tests`) — `verify_pass_emits_failed_event_when_discrepancies_present` + `verify_pass_emits_passed_event_when_workspace_agrees`. Pin both arms of the new branch; replace the previous (buggy) `verify_pass_emits_tier3_event_with_counts` which expected `VerificationPassed` for a discrepancy case.

### A5 — doc updates

- `tasks/todo.md`:228 flipped `[~]` → `[x]` (§7 lying-agent gate closed offline).
- `tasks/todo.md`:151 / 162 / 174 flipped `[ ]` → `[~]` with offline-landed notes and pointers to the remaining live-API + nightly-CI portions in Track B / C.

### What's *not* in v60.12

Live-API tests against Anthropic + OpenAI-compat (Track B) and the new nightly workflow (Track C `.github/workflows/nightly_phase_a_gate.yml`) are deferred. They need an `ANTHROPIC_API_KEY` secret + maintainer approval for the first run cost. The plan at `/Users/chris.adkin/.claude/plans/fluffy-painting-llama.md` documents them.

The §2 real-model conformance ≥95% gate (`tasks/todo.md:219`) is Phase B work; lands with Track B.

The §7 hallucinating-agent Tier-1 detector (`tasks/todo.md:225, 229`) stays gated on Q3 (LSP auto-install UX). Spec line 132 does not include it under Phase A.

---

## v60.11 — 2026-05-18 (three-bundle parallel release: §15 wave 2 + polish + B2 recovery)

Three bundles ran in parallel (C1 HTTP/SSE launcher, C2 dispatcher MCP tool registration + resources as §5 context, C3 polish trio). C3 caught an oversight in the v60.10 release: **B2's commit `3209a9e` (mid-session provider swap) was never actually merged into main during v60.10** despite the CHANGELOG claiming it. The orchestrator ran `git merge` for B3 only and skipped B2. v60.11 recovers B2 first, then lands C1+C2+C3 on top. The v60.10 docs entry's B2 claims are now actually deployed. Workspace tests **974 → 1018** (+44, including B2's +6). All gates green.

### B2 recovery — §1 mid-session provider swap (carried over from v60.10)

Merged as commit `3209a9e` (the original B2 worktree branch was still around). See the v60.10 CHANGELOG entry for the full feature description: `Runner::swap_adapter`, `Event::AdapterSwapped`, GUI Tauri command + `SwapProviderWire`/`SwapResult` wire types, state-preservation matrix (carries ContextManager/MemoryStore/PlanCanvas/conversation/pending-approval; resets conformance/strategy/capability/few-shot), `RecordingMockAdapter` test helper + 2 integration tests. The v60.10 description is now accurate.

### C1 — §15 HTTP/SSE MCP client launcher

Sibling to v60.10's `stdio_launcher.rs`. Closes the "HTTP / SSE MCP client (egress audited per §12)" row.

- New `crates/atelier-core/src/mcp/http_launcher.rs` (~772 lines + 12 unit tests). Uses `rmcp::transport::SseTransport::start_with_client` — rmcp 0.1.5 has only one remote transport (SSE), so both `Transport::Http` and `Transport::Sse` manifest variants route through it.
- Egress audit: every outbound HTTP request writes one `McpEgressEvent` row to `<audit_dir>/audit.log` per the new `schemas/audit/mcp_egress.v1.json` schema, with `kind: "mcp-http-request"` + `provider` + `url` + `phase: handshake | list_tools | call_tool` + `outcome: success | failure | blocked`. Authorization headers are NEVER serialised — the audit shape has no `headers` key.
- `allow_net: false` semantics for HTTP/SSE manifests = refuse-to-launch with `McpLaunchError::Refused("HTTP/SSE transport requires allow_net=true")`. Stdio is the local-only path; HTTP/SSE explicitly opts in to egress.
- New error variants: `HttpStatus`, `SseStream`, `InvalidHeader`.
- New `audit::McpEgressEvent` shape + `append_mcp_egress` helper (atomic append, mirrors v60.8's `EgressEvent` discipline).
- Live test gated `#[ignore]` reading `ATELIER_MCP_SSE_URL` env-var; rmcp's `SseTransportError::Reqwest` is the only path that surfaces a status code, so some `502`-style failures land as `SseStream` rather than `HttpStatus` — the test tolerates both.

### C2 — §15 dispatcher MCP tool registration + MCP resources as §5 context items

Closes two rows in one bundle: built-in-style tool registration for external MCP servers, plus MCP resources surfaced as `ContextItem`s.

- New `crates/atelier-core/src/mcp/mcp_tool.rs` — `McpToolWrapper` implements the `Tool` trait by routing calls through `McpServerHandle::call_tool`. Carries `server_name`, `tool_name`, `description`, `input_schema`, `Arc<Mutex<McpServerHandle>>` (shared across all tools of one server), and `side_effect_class` (per-tool override or per-server default from the manifest).
- New `crates/atelier-core/src/mcp/registration.rs` — `register_mcp_servers(registry, manifests, approvals, sandbox, audit_dir)` launches each enabled+approved server, lists its tools, registers each as an `McpToolWrapper`. Returns `RegisterMcpReport { servers_registered, tools_registered, servers_pending_approval, server_failures }`. Refused servers and pending-approval servers don't register; per-server failures don't abort the whole registration.
- New `McpServerHandle::list_resources()` + `McpResourceDescriptor { uri, name, mime_type, description }`. Companion helper `mcp_resource_to_context_item` + `register_mcp_resources_as_context` builds a `ContextItem` per resource with `Provenance::McpResource { server_name, resource_uri }`, `payload: BlobRef { sha256_hex: <computed-from-uri>, mime_type }`, `tokens: { count: 0, source: Unavailable }`.
- **Closed-enum break**: new `Provenance::McpResource` variant in `crates/atelier-core/src/context.rs`. Wire label `"mcp_resource"` pinned by the v58 wire-label-agreement test. Match sites updated: `ContextItemSummary::from_item` (context.rs), `cache_bust_from` (ledger.rs), TUI badge map + `provenance_badge_style` (Cyan), badge-covers-every-variant test.
- Integration test `register_and_dispatch_mcp_routed_call` (gated `#[ignore]` on npx) exercises the full path: launch server → register tools → dispatch a tool call routed through `McpToolWrapper` → assert the result rides on the bus like a built-in tool call.
- `McpToolWrapper::execute` is not unit-tested directly (constructing an `McpServerHandle` requires a real rmcp service); the pure pieces (`compile_input_schema`, `validate_args_against`, `map_launch_error`, `stringify_content`) are exercised individually + `execute` is covered by the gated integration test.

### C3 — polish trio (v60.7/8/10 follow-on debt)

Three small follow-ons grouped:

- **(a) `verify_pass` wired into runner**: closes the v60.8 A2 known gap. Runner's per-turn loop now harvests `EditStaged` events as `ObservedChange`s + stashes the last envelope, then calls `dispatcher.verify_pass(&envelope, &observed, now)` (or `emit_verify_not_run` when there's nothing to verify) before `State::Done`. New integration test `run_emits_verification_passed_tier3_when_write_file_observed` pins the contract.
- **(b) `Event::ContextOverflowResolved` UI rendering**: closes the v60.9 B1 follow-on. GUI MetersPane gains a 5s overflow toast with `setInterval` decay; new `state.ts::lastOverflowResolution` field + `applyEvent`/`projectEvent` arms. TUI gets `OverflowResolutionHint` struct + `OVERFLOW_HINT_TTL` const + inline hint slot in `render_cost_meter` decaying after 5s.
- **(c) GUI footer dropdown for `swap_adapter`**: closes the v60.10 B2 UI affordance follow-on. New `<select>` in `App.svelte` near the model badge listing the known adapter families (`mock` / `anthropic` / `openai_compat`); on change, fires `invoke('swap_adapter', { provider })` against B2's real Tauri command (NOT the stub C3 had to write as a fallback — see merge resolution below).

### Cross-bundle merge resolution

Merge order: **B2-recovery → C1 → C2 → C3**. Three conflict files on the C2 merge (`mcp/mod.rs`, `lib.rs`, `mcp_integration.rs`) — all additive re-export collisions, "keep both" resolution. Two conflict files on the C3 merge:

- `crates/atelier-gui/src/lib.rs` — both B2 (recovered) and C3 registered `swap_adapter` Tauri commands. C3 wrote a *stub* version against the assumption that B2's full impl wasn't on main yet (it wasn't, until I merged B2 first). The stub is removed; B2's real async impl (with `AdapterHandle::swap` + `Event::AdapterSwapped` emission + fresh `ModelProfileLoaded` re-emission) is what's deployed.
- `crates/atelier-tui/src/lib.rs` — C3 added an active `ContextOverflowResolved` handler upstream of the blanket no-op arm. B2 had added `AdapterSwapped` to the no-op arm. Resolved by keeping C3's active handler + the `AdapterSwapped` no-op arm.

The `Provenance::McpResource` closed-enum break required updating every `Provenance` match site. C2's agent caught the obvious ones (`ContextItemSummary::from_item`, `cache_bust_from`, TUI badge maps + test). All other match sites were verified at merge time.

### Workspace test count delta

- atelier-core unit: 746 → 782 (+36: 12 from C1 http_launcher + 4 from C1 audit + ~14 from C2 dispatcher/registration + ~6 from C2 mcp_tool)
- atelier-cli unit (lib): 45 → 45 (unchanged)
- atelier-cli integration: 63 → 64 (+1 C3 verify_pass)
- atelier-cli mcp_integration: 3 → 4 always-on + 3 `#[ignore]` (+1 C1 always-on, +1 C2 ignored, +1 C1 ignored)
- atelier-gui: 29 → 29 (unchanged; B2 had already added the bridge test)
- atelier-tui: 94 → 94 (B2 added 2 TUI tests in its recovery; C2 added 0; C3 added 0)
- Total: **974 → 1018** (+44)

### Process candor — the B2 oversight

The v60.10 CHANGELOG entry as previously deployed claimed B2's mid-session-provider-swap landed alongside B3. It didn't. The orchestrator (me, last session) ran `git merge --no-ff worktree-agent-a854bcd084ccde3c8 -m "Merge B3: ..."` after both bundles finished, then wrote a CHANGELOG entry covering both. No corresponding `git merge` was ever run for B2. The mistake survived through the v60.10 push because the docs commit + push happened without re-verifying that the claimed code paths existed on main.

This was caught by the C3 agent's report: "the v60.10 CHANGELOG claimed `Runner::swap_adapter` exists but the GUI surface has no `swap_adapter` Tauri command on main." That observation triggered a `git log --oneline` audit of `c91d851..HEAD` which confirmed the missing merge.

The recovery in v60.11: B2's commit `3209a9e` was still reachable via its worktree branch, so a fresh `git merge --no-ff worktree-agent-a71cfa12e8016bf18` recovered the work cleanly with no conflicts (no main commits had since touched B2's territory). Then C3's stub `swap_adapter` (which it had written defensively assuming B2 wasn't on main) was deleted during the C3 merge in favour of B2's real implementation.

Lesson for future parallel batches (already captured in `~/.atelier/memory/feedback_worktree_isolation_drift.md` for the related drift-into-parent-repo issue): the docs sweep at the end of a parallel batch should grep main's `git log` for each claimed bundle's merge commit before pushing. A bundle-not-merged failure is observationally identical to a bundle-merged-but-empty failure unless you check.

## v60.10 — 2026-05-18 (two-bundle parallel release: §15 rmcp foundation + §1 mid-session provider swap)

B3 + B2 ran in parallel worktrees, then merged sequentially into main (B2 first, B3 second — both fully disjoint). Workspace tests **963 → 974** (+11). All gates green. **Q7 resolved: GO WITH CAVEATS** on rmcp 0.1.5.

### B3 — §15 rmcp foundation (Q7 spike + dep + stdio launcher)

The §15 MCP-client residual was the biggest-ticket remaining Phase A item. This bundle resolves Q7 (rmcp maturity), adds the dep to `atelier-core`, and ships a stdio-launcher that spawns `@modelcontextprotocol/server-filesystem` end-to-end. The remaining §15 rows (HTTP/SSE, built-ins-as-MCP refactor, MCP resources as §5 context items, dispatcher wiring, mechanical gate) sit on top of this foundation and land in v60.11+.

**Q7 verdict — GO WITH CAVEATS** (`experiments/rmcp_spike/README.md` carries the full matrix):

- Stdio handshake against `@modelcontextprotocol/server-filesystem`: ~700ms cold-start via npx, then `list_tools` + `call_tool` clean. `list_directory` returns the expected 14-tool surface.
- Crash recovery: SIGKILL on the live server PID surfaces `ServiceError::Transport("disconnected")` in ~20µs; serve loop quits `Closed` cleanly. No zombies.
- Shutdown via `client.cancel()` (the `CancellationToken` path) is reliable; the natural stdout-EOF path doesn't wake the framed codec.

**Five rmcp 0.1.5 smells worth flagging for v60.11+**:

1. Broken feature gating — `paste::paste!` used unconditionally inside `capabilities.rs` but gated behind the `macros` feature. Setting `default-features = false` breaks the build.
2. No public PID accessor on `TokioChildProcess` once rmcp owns the `Child`. Shutdown must go through `client.cancel()`, not direct subprocess signalling.
3. Natural stdout-EOF path doesn't reliably wake the framed codec — `shutdown()` always uses cancel.
4. `Tool.input_schema` is `Arc<serde_json::Map>`, not `Value::Object`. The launcher wraps it once at projection time so callers see `Value`.
5. `Implementation::from_build_env()` injects the caller's *crate name* as `client_info.name` — MCP servers see "atelier-core" rather than "atelier". Override at v60.11+ dispatcher wiring.

**Files shipped (B3)**:
- `crates/atelier-core/Cargo.toml` + workspace `Cargo.toml` — `rmcp = "0.1.5"` dep.
- `crates/atelier-core/src/mcp/mod.rs` + `errors.rs` + `stdio_launcher.rs` (~685 lines + 9 unit tests). `launch_stdio_server(manifest, sandbox, audit_dir) -> McpServerHandle` does the handshake; `list_tools`, `call_tool`, `shutdown` round out the surface. Respects v60.8's `mcp_config::McpServerManifest` (transport, env interpolation, allow_net) end-to-end.
- `experiments/rmcp_spike/` — fully implemented stdio + crash modes; README's decision matrix populated.
- `crates/atelier-cli/tests/mcp_integration.rs` — 2 always-on tests (`npx_availability_probe`, `egress_block_does_not_prevent_spawn`) + 1 `#[ignore]`-gated live-npx test that exercises the full handshake against the filesystem MCP server.

**Out of scope (deferred to v60.11+)**: HTTP/SSE transport, built-ins-as-MCP refactor, MCP resources as §5 context items, dispatcher wiring (the launcher exposes the surface; the dispatcher doesn't yet register MCP tools alongside built-ins), and the §15 mechanical gate (canonical-workload run with `@modelcontextprotocol/server-filesystem` registered).

**Known gaps documented in code**:
- `launch_stdio_server`'s `audit_dir` parameter is existence-checked but doesn't yet write `§12` egress audit entries — that lands with the dispatcher integration.
- The launcher doesn't wrap the MCP server in `sandbox-exec`/`bwrap` — the existing `sandboxed_argv` infrastructure assumes a short-lived child. A long-lived-MCP-aware sandbox is its own v60.11+ design problem. Egress is still blocked via the `http_proxy=127.0.0.1:1` env block from v60.8.

### B2 — §1 BYOM mid-session provider swap

Closes the §1 BYOM UX-target row: "mid-session provider swap preserves work."

- New `Runner::swap_adapter(new_adapter, now)` method. Per-turn-boundary operation — the caller swaps between `run()` invocations (the types enforce it: `run()` takes `&self`, `swap_adapter` takes `&mut self`). The pre-swap adapter's in-flight `chat()` is not cancelled; drop-on-cancel applies via the existing `CancellationToken`.
- New `Event::AdapterSwapped { from_model_id, to_model_id, swapped_at }` on the bus + standard `kind()` arm + GUI `bridge_event` + Svelte `state.ts` reducer arm + TUI `apply` / `project_event` arms.
- New `AdapterHandle::swap(new)` public setter so the live slot updates atomically with the swap. Pending `swap_adapter` requests queue on `Runner.pending_adapter_swap` and the `AdapterSwapped` event fires on the next `run()` startup.
- GUI Tauri command `swap_adapter(provider: SwapProviderWire) -> SwapResult` where `SwapProviderWire { kind: "mock" | "anthropic" | "openai_compat", model_id, base_url? }`. Builds the new adapter via a refactored `build_swap_adapter` helper.
- State-preservation matrix (carries vs resets across the swap):
  - **Carries**: `ContextManager`, `MemoryStore`, `PlanCanvas`, conversation transcript (via on-disk session + `with_resume`), `StagingPendingApproval`.
  - **Resets**: `ConformanceRingBuffer` (new adapter = new behaviour signal), `Strategy` (re-resolved from new `ModelProfile`), `CapabilityMatrixRow` (refreshed from new model), few-shot cache (forcibly cleared in `swap_adapter`).
  - **Recomputed at construction**: `CostPolicy` is fixed at `Runner::new` time; the caller decides the policy when building the new adapter.
- `RecordingMockAdapter` helper + 2 integration tests in `run_integration.rs`.

**Known follow-ons / candor**:

- The GUI's `swap_adapter` Tauri command emits the bus events + updates the live `AdapterHandle` slot, but does **not** swap the adapter inside a running `Runner` — the Runner reads `self.adapter` per turn, not from the slot. True mid-`run()` swap needs a future Runner refactor to read from a shared slot.
- The `RecordingMockAdapter` had to force `Strategy::JsonSentinel` because `OnDiskSession::resume_conversation_prefix` truncates at orphan tool-call ids — a `harness_meta` tool_call without a matching tool_result would have dropped the assistant turn on resume. Worth documenting in the resume contract.
- No UI affordance lands here — the Tauri command surface is reachable via `invoke('swap_adapter', { provider })`; a footer dropdown / command palette entry is follow-on work.

### Workspace test count delta

- atelier-core unit: 737 → 746 (+9 from `mcp::stdio_launcher::tests`)
- atelier-cli unit (lib): 45 → 45 (unchanged)
- atelier-cli integration: 61 → 63 (+2 B2 swap round-trips)
- atelier-cli mcp_integration (new binary): 2 always-on + 1 `#[ignore]`-gated live-npx
- atelier-gui: 28 → 28 (B2 added 1 GUI bridge test; B3 zero)
- atelier-tui: 92 → 92 (B2 added 2 TUI tests)
- Total: **963 → 974** (+11 including the new mcp_integration binary)

### Cross-bundle merge resolution

Merge order: B2 → B3. **Zero conflicts.** The file-scoping discipline in the briefs paid off:
- B2 touched `runner.rs`, `session.rs::Event`, GUI/TUI projections, integration tests.
- B3 touched `experiments/rmcp_spike/`, `crates/atelier-core/Cargo.toml`, the new `crates/atelier-core/src/mcp/` module, `crates/atelier-cli/tests/mcp_integration.rs` (new file).
- The only file both bundles touched was `crates/atelier-core/src/lib.rs` for re-exports — and git's auto-merge handled the additive case cleanly.

This is the cleanest parallel batch since v60.7 — same lesson, smaller bundles, file-disjoint by design.

### Process candor

B3's agent reported a mid-flight slip: it initially developed in the main repo's working tree rather than the worktree, then caught the mistake + copied the changes into the worktree and reverted the main repo. The final commit is correctly on the worktree's branch; the main repo was verified clean before the merge. Worth noting in the parallel-agent pattern as a sharp edge: agents in `isolation: "worktree"` mode can accidentally edit the parent repo if they cd around or use absolute paths incorrectly. The agent's self-correction was honest and clean — no tracked-file leakage between repos.

## v60.9 — 2026-05-18 (two-bundle parallel release: §1 context-window asymmetry + §2 per-adapter few-shot override)

Two-bundle parallel release. B1 + B4 ran in isolated worktrees, then merged sequentially into main (B1 first because its `MockResponse::overflow` field change had wider workspace blast radius). Workspace tests **928 → 963** (+35). All gates green.

### B1 — §1 BYOM context-window asymmetry (Compact / Reroute / Surface)

Closes the spec promise on what happens when an adapter returns `AdapterError::ContextOverflow`. Three policies, runner-side, configurable per-session.

- New `ContextOverflowPolicy::{Compact, Reroute, Surface}` enum + `Runner::with_overflow_policy(policy)` builder (default = `Compact`).
- **Compact**: auto-selects unpinned context items (token-count-descending) via the new pure `pick_overflow_compaction_targets(summaries, needed, limit, current_total)` helper, feeds them to the v60.5 compaction orchestrator, then retries the turn. Drops down to `Surface` after `MAX_OVERFLOW_RETRIES = 2` consecutive overflows (defends against runaway compaction loops).
- **Reroute**: wireable stub for the v60.10+ routing-dispatcher work — returns `RunError::Config("reroute not yet implemented")`.
- **Surface**: propagates `RunError::ContextOverflow { needed_tokens, limit_tokens }` as a typed error.
- New `Event::ContextOverflowResolved { resolution: "compacted" | "rerouted" | "surfaced", freed_tokens: Option<u32>, items_compacted: Option<usize> }` on the bus. GUI `bridge_event` + TUI `project_event` arms wire-projected; no GUI/TUI rendering arm in this bundle (the bus event lands but no toast/panel renders it — follow-on).
- Auto-selector heuristic: filter unpinned → sort token-count-descending → compute `raw_target = needed - (limit - current_total)` (saturating) → floor at the smallest unpinned candidate's tokens → pad by `OVERFLOW_SAFETY_MARGIN_PCT = 25%` → greedy accumulate. `MAX_OVERFLOW_RETRIES = 2` and the 25% margin are PROVISIONAL pending Q1 calibration.
- `MockResponse::{new, context_overflow}` constructors + new `MockResponse.overflow: Option<(u32, u32)>` field for test seam. The struct-literal MockResponse pattern now requires `overflow: None`; ~30 existing call sites updated.
- 7 new tests: 6 unit tests on the policy match + auto-selector heuristic (extracted as pure helpers), 1 integration test scripts MockAdapter overflow on turn 1, asserts compaction fires + retry succeeds + `Event::ContextOverflowResolved { resolution: "compacted", .. }` lands.

### B4 — §2 model protocol per-adapter few-shot override

Closes the spec promise on per-adapter few-shot customisation. Each adapter can override the shared baseline for a given `Strategy`.

- New trait method on `Adapter`:
  ```rust
  fn few_shot_override(&self, strategy: Strategy) -> Option<Vec<Message>> { None }
  ```
  Default returns `None` (fall back to baseline). MockAdapter keeps the default.
- **AnthropicAdapter**: returns `Some(messages)` for `JsonSentinel` — a Claude-flavoured user/assistant pair with the literal `<<<harness_meta>>>{...}<<<end>>>` sentinel inline so Claude sees the carrier shape. `NativeTool` + `RegexProse` return `None`.
- **OpenAiCompatAdapter**: returns `Some(messages)` for `JsonSentinel` — assistant turn starts with `<<<harness_meta>>>` (no prose preface) and contains only strict JSON between sentinels, biasing local OSS models toward strict-JSON emission without narration.
- Runner wiring: new `Runner.few_shot_cache: parking_lot::Mutex<Option<Vec<Message>>>` field. The override is computed once per session (cached) on first turn. If `Some`, those messages are prepended before the resume/fresh-run bootstrap; if `None`, the existing baseline path runs unchanged.
- New `Runner::with_adapter_for_test(adapter)` test-only builder (`#[doc(hidden)]` + `#[allow(dead_code)]`) for swapping in custom adapter impls in integration tests.
- `async-trait` added as a dev-dep of `atelier-cli` (it's already a normal dep of `atelier-core`) so test adapters can implement the trait directly.
- 9 new tests: per-adapter unit tests (Mock `none-by-default`; Anthropic 3 strategies; OpenAI-compat 3 strategies); 2 integration round-trip tests via `MockAdapterWithOverride`.

### Workspace test count delta

- atelier-core unit: 729 → 737 (+8: B4 adapter overrides)
- atelier-cli unit (lib): 39 → 45 (+6: B1 policy + auto-selector)
- atelier-cli integration: 40 → 61 (+21: B1 1 integration + 20 from B1's `#[path]`-mounted compaction tests; B4 2 round-trips)
- atelier-gui: 28 → 28 (unchanged)
- atelier-tui: 92 → 92 (unchanged)
- Total: **928 → 963**

### Cross-bundle merge resolution

Branches forked from `109fc62`-then-merged-into-`6763c0a` (v60.8 docs). Merge order: B1 → B4. Single conflict on `crates/atelier-cli/src/runner.rs`:
- Both bundles added a new field to the `Runner` struct (`overflow_policy` from B1, `few_shot_cache` from B4) — resolved as additive "keep both."
- Both bundles added a new initialiser line in `Runner::new` — additive "keep both."

No other conflicts. B4 explicitly avoided `session.rs` (B1's territory); B1 explicitly avoided `adapter/*` and `protocol_strategy.rs` (B4's territory). The discipline-driven brief paid off — minimal merge cost vs the v60.8 batch where the agents stepped on each other's `session.rs::Event::kind()` match.

### Deferred to follow-on bundles

- §15 rmcp foundation (B3): blocked behind this release per the user's "B1 + B4 in parallel, then B3" plan. Picked up next.
- §1 mid-session provider swap: defer to a sequential pass (would conflict heavily with B1's overflow handler in `runner.rs`).
- GUI/TUI rendering of `Event::ContextOverflowResolved`: a small follow-on toast on the GUI + footer hint on the TUI.
- `--overflow-policy` CLI flag on the binary: deferred (binary defaults to `Compact`).

## v60.8 — 2026-05-18 (four-bundle release: §11 egress gate, §7 tier indicator, §15 mcp_servers loader, §1 conformance degradation)

Second four-bundle parallel release in two days. Four sub-agent worktrees → four merges into main → one docs commit. Workspace tests **861 → 928** (+67). All gates green: `cargo fmt --check`, `cargo clippy --workspace -D warnings`, `cargo test --workspace`, `npm run check`, `make check` (112 rig tests, 13 canonical fixtures).

### A3 — §15 mcp_servers.json loader + first-use approval store

The rmcp-free config layer. Lands the schema-driven loader and the trust-budget approval store so the eventual rmcp client can plug into a settled surface.

- New `crates/atelier-core/src/mcp_config.rs` (~890 lines including +23 unit tests): typed `McpServerManifest { name, transport, command, args, env, url, headers, side_effect_class, allow_net, enabled }`; `Transport::{Stdio, Http, Sse}`; `SideEffectClass::{LocalSafe, LocalRisky, SharedState, Irreversible}` (sibling to the dispatcher's enum — config-layer concerns vs trust-budget cost semantics evolve independently).
- `pub fn load_mcp_servers(workspace_root) -> Result<Vec<McpServerManifest>, McpConfigError>`: reads `<workspace>/.atelier/mcp_servers.json`; absent file = empty vec (fresh-repo state, not an error); validates each entry against the embedded `schemas/config/mcp_servers.v1.json` via `jsonschema`; rejects duplicate names; filters out `enabled: false` entries before return.
- `mcp_interpolate(s)` free function: resolves `${env:NAME}` from `std::env::var` at request time (not at load time, so secrets never persist into the parsed manifest); `${keychain:NAME}` returns `McpConfigError::KeychainNotYet` — explicit handoff to the future rmcp client.
- `McpApprovals` (mirror of `HookApprovals`): per-server first-use trust-budget store at `<workspace>/.atelier/mcp_servers/_approvals.json`; methods `approve`, `is_approved`, `pending(loaded)`, `save`, `load`. Per spec §15 line 741 ("server registration is a §8 trust-budget event on first use"), approval is at the server level — granting trust to a server grants it to all that server's tools.

### A1 — §11 sandbox egress mechanical gate

Spec §11 + §12: a `shell` tool call attempting egress to a host outside the sandbox profile's allow-list is blocked AND audited.

- Block mechanism (portable, dev-friendly): when the sandbox profile says `allow_net: false`, `subprocess::run` injects `http_proxy=http://127.0.0.1:1` / `https_proxy=http://127.0.0.1:1` into the child's environment. Any HTTP client inside the child (curl, wget, fetch) fails to connect to a closed loopback port. Linux namespaces are non-portable; macOS pf rules need sudo; the proxy approach is the realistic choice and is documented inline.
- New `crates/atelier-core/src/audit.rs`: `EgressEvent { version, kind, tool_call_id, tool_name, destination, outcome, reason, timestamp }` per the new `schemas/audit/subprocess_egress.v1.json`. Newline-delimited JSON, one entry per line, persisted at `<workspace>/.atelier/sessions/<sid>/audit.log`.
- Every built-in tool that launches a subprocess (`shell`, `grep`, `ast_grep`, `read_file`, `list_dir`, `write_file`, `edit_file`) now threads its `tool_call_id` into the subprocess layer so blocked-egress events carry the originating call in the audit trail.
- Integration test `shell_curl_evil_example_is_blocked_and_audited` scripts a `shell` tool call attempting `curl https://evil.example/secrets`; asserts (a) non-zero exit + run reaches Done after later turn declares claimed_done, (b) audit.log contains exactly one `EgressEvent` JSON line referencing `evil.example` + `tc-curl-evil` tool_call_id + RFC 3339 timestamp, (c) `OnDiskSession::load_from` round-trips session.json validating the schema.

### A2 — §7 UI tier indicator

Visibility into verification coverage. When Tier 1 (LSP) is unavailable and the harness falls back to Tier 2 / Tier 3, the user sees the drop in a coloured footer badge rather than silently getting weaker checks.

- New `VerificationTier` enum in `crates/atelier-core/src/verify.rs`: `Tier1Lsp` / `Tier2TreeSitter` / `Tier3Textual` / `NotRun` with `wire_label()` + serde `rename_all = "snake_case"`. Wire labels (`tier1_lsp`, `tier2_tree_sitter`, `tier3_textual`, `not_run`) pinned by an agreement test.
- New `VerificationRun { tier, file_count, claim_count, discrepancies }` with `tier3_textual()` and `not_run()` constructors. Tier 1 is wire-reserved but has no producer (LSP work gated on Q3); Tier 2 producer wiring is a Phase D follow-on.
- New `Event::VerificationPassed { tier, file_count, claim_count }` (kind `VerificationPassed`); `SessionDispatcher::verify_pass` runs Tier 3 textual + emits the event; `emit_verify_not_run` is the explicit "verification disabled" sentinel.
- GUI MetersPane gains a colour-coded verify badge: green (Tier 1), yellow (Tier 2), orange (Tier 3), gray (NotRun). New `state.ts` types `VerificationTier` + `VerificationStatus` + `verificationTierLabel()` helper. TUI: `VerificationStatusHint` with `badge_label`/`badge_colour`, surfaced right-aligned on the cost row.
- 13 new tests pin the wire-label agreement, the bridge, and the badge rendering.
- **Known follow-on**: the dispatcher's `verify_pass` is **not yet called from `runner.rs`** — the Runner still transitions to `State::Verifying` without invoking it. Wiring the call site is a small follow-on; the doc + `emit_verify_not_run` sentinel make the absence explicit rather than silent.

### A4 — §1 BYOM conformance-driven strategy degradation

The runner walks the active §2 strategy toward more-tolerant forms when the rolling-window malformed-envelope rate crosses a threshold. PROVISIONAL defaults (3-of-20) — calibration row depends on Q1.

- New constants in `crates/atelier-core/src/protocol_conformance.rs`: `DEFAULT_DEGRADATION_WINDOW: usize = 20` + `DEFAULT_DEGRADATION_THRESHOLD: u32 = 3`. `ProtocolConformance::should_degrade()` returns true when the rolling window has ≥ threshold malformed events out of ≥ window total.
- `Strategy::less_tolerant_than` + degradation order (`NativeTool < JsonSentinel < RegexProse`). `Strategy::degrade_one_step` walks toward the more-tolerant end of the stack; degradation is one-way for the session (no auto-promotion).
- Runner wiring: each turn's parse outcome feeds `conformance.record(...)`. When `should_degrade()` is true, the runner decrements the active strategy one step and emits `Event::StrategyDegraded { from, to, reason }`. `Runner::with_degradation_window(n)` + `with_degradation_threshold(t)` builders let integration tests dial the threshold down without queueing 20 mock responses.
- New `Event::StrategyDegraded` (kind `StrategyDegraded`) on the bus; GUI bridge serialises `from`/`to` via `Strategy::as_str` ("native_tool" / "json_sentinel" / "regex_prose"); GUI `state.ts` `applyEvent` arm updates `currentModel.strategy` so the footer badge reflects the lowered tier. TUI's apply arm does the same on `current_model.strategy`.
- Two new integration tests in `crates/atelier-cli/tests/run_integration.rs`:
  - `run_degrades_strategy_after_three_malformed_envelopes_in_window`: 4-turn scripted MockAdapter with 3 malformed responses + one JSON-sentinel envelope; asserts exactly one `StrategyDegraded(NativeTool → JsonSentinel)` event fires.
  - `run_does_not_emit_strategy_degraded_when_envelopes_are_clean`: pins the "no false positives" half — a clean envelope doesn't fire the degrade arm even with threshold dialled to 1.

### Workspace test count delta

- atelier-core unit: 675 → 729 (+54: +23 mcp_config + +7 verify + +6 audit + +18 protocol_conformance/strategy)
- atelier-cli unit (lib): 39 → 39 (unchanged — A1/A2/A3/A4 added tests to atelier-core or integration suite)
- atelier-cli integration: 37 → 40 (+3: 1 egress gate, 2 strategy degradation)
- atelier-gui: 26 → 28 (+2: VerificationPassed bridge + StrategyDegraded bridge)
- atelier-tui: 84 → 92 (+8: tier badge rendering + degradation apply arm)
- Total: **861 → 928**

### Cross-bundle merge resolution notes

Branches forked from `109fc62` (v60.7 docs) and merged in order **A3 → A1 → A2 → A4**. A3 was fully isolated (zero conflicts). A1 + A2 merged cleanly via git's ort strategy (additive changes to disjoint sections). A4 collided with A2 on **five files**, all additive collisions on shared registries:

- `session.rs::Event` enum + `kind()` match: keep both `VerificationPassed` (A2) + `StrategyDegraded` (A4).
- `atelier-gui/src/lib.rs::bridge_event` match + tests: keep both arms.
- `atelier-gui/ui/src/lib/state.ts` applyEvent + projectEvent arms: keep both cases.
- `atelier-tui/src/lib.rs` apply + project_event arms: keep both.
- `atelier-cli/tests/run_integration.rs`: the conflict here was structural — git auto-merged the shared `let runner = Runner::new(...)` scaffold INSIDE both test functions, producing a frankentest. Resolved by extracting each test cleanly from its source worktree and re-appending them in order.

No semantic conflicts. The pattern is now well-established: bundles that touch the `Event` enum / `bridge_event` / `state.ts applyEvent` will always collide on those registries, but the resolution is always "keep both."

## v60.7 — 2026-05-18 (four-bundle release: §2 protocol overhead, Phase C close, §1 BYOM ledger, §14 persistence)

Four bundles landed in parallel from separate sub-agent worktrees, then merged sequentially into main. Workspace tests **788 → 861** (+73). All gates green: `cargo fmt --check`, `cargo clippy --workspace -D warnings`, `cargo test --workspace`, `npm run check`, `make check` (11/11 canonical fixtures + 13 with new t12/t13, 112 rig tests).

### Bundle 5 — §2 protocol-overhead harness + nightly CI + fixtures

- New `atelier_core::protocol_strategy::measure_overhead` returns bytes-on-wire, approximate tokens, and parse-time-ms per emission strategy (`native_tool` / `json_sentinel` / `regex_prose`).
- New `atelier protocol-overhead` CLI subcommand runs the harness against scripted `MockAdapter` fixtures (`tests/protocol/fixtures/{native_tool,json_sentinel,regex_prose}.json`) and writes `tests/protocol/overhead.json` per `schemas/protocol/overhead.v1.json` (additive optional fields).
- New nightly GitHub Actions workflow `.github/workflows/nightly_protocol_overhead.yml` runs the harness daily; fails on >10% drift vs the rolling 7-day median.
- New `atelier_cli::overhead` module + 7 unit tests; 3 strategy-side tests for `measure_overhead`.

### Bundle 1 — Phase C close (mental-model panel + inline renderers + UX-target workloads)

Closes the four remaining Phase C residuals. **Tasks/todo.md** §3 + §5 rows fully ticked.

- **§5 mental-model panel** (off by default, cost-disclosed). New `atelier_core::mental_model::{MentalModel, MentalModelSnapshot, MentalModelError}`; `SessionDispatcher::{set_mental_model, snapshot_mental_model}`; `Event::MentalModelSnapshot { enabled, text_tokens }`; new Tauri commands `set_mental_model` + `snapshot_mental_model`; new `MentalModelPane.svelte` + header toggle row in `App.svelte`; TUI footer hint `mm:on(~Ntk,0/turn)` driven by a new `MentalModelHint` projection. v0 explicitly does NOT inject text into the prompt; the cost-disclosure badge reads "0 tokens per turn at present" until that ships.
- **§3 inline rendering Mermaid / D2 / images**. New `InlineRenderers.svelte`; `mermaid@^11.4.1` npm dep added; integrated in `DiffPane.svelte` and `MemoryPane.svelte`. Mermaid + image cases render inline; D2 falls back to a "render not available, showing source" placeholder.
- **§3 UX target measurement: refactor without conversation pane open**. New `PaneVisibility` + `PaneVisibilityRecord` in `atelier_cli::instrumentation`; `Runner::with_pane_visibility(panes, driver)` builder writes `<session_dir>/pane_visibility.json` at end of run. New canonical fixture `tests/workload/canonical/t12_refactor_no_conversation_pane/` exercises the path.
- **§5 UX target measurement: "find what agent knows about file X"**. New `FindProbe` + `FindProbeLog` (atomic append + median) in `atelier_cli::instrumentation`. New fixture `tests/workload/canonical/t13_find_what_agent_knows/`. The matching `atelier find --path <P>` CLI subcommand is deferred to a later bundle — the on-disk format is frozen now.
- **`schemas/workload/task_meta.v1.json`** extended with optional `pane_visibility` and `find_probe` objects (additive, no migration).

### Bundle 2 — §1 BYOM ledger discipline + capability matrix

- **Per-call cost ledger emission with declared `count_tokens` source**. Anthropic + OpenAI-compat adapters now set `count_source: TokenSource::Exact` iff the wire carried a `usage` block, else `Unavailable`. Mock stays `Exact` when its scripted response declares tokens, else `Unavailable`.
- **Latency-weighted local cost; default `$0.00028/sec`**. New `ModelCostPolicy::{LatencyWeighted, UnknownPending}` enum + `Runner.cost_policy` field; computed once at `Runner::new` time from `ProviderChoice` + base URL. Local providers (Mock, OpenAI-compat against non-`api.openai.com`) emit `cost_usd = Some(local_cost_usd(latency_ms, DEFAULT_LOCAL_RATE_USD_PER_SEC))`; cloud providers (Anthropic, hosted OpenAI) emit `cost_usd = None` until per-provider pricing tables ship. New private helper `is_openai_cloud_base_url`.
- **Capability matrix**. New `atelier_core::adapter::capability_matrix` module: static lookup table for 9 well-known models (`anthropic:claude-opus-4-7`, `openai-compat:gpt-4o`, `local:qwen2.5-coder:7b`, etc.) mapping to `Capabilities { native_tool_use, streaming, vision, prompt_cache, structured_output, long_context, context_window_tokens }` with `CapabilityClaim::{Supported, ClaimedButBroken, Unsupported}`. Cross-walks with `ModelProfile` probe observations to flag `ClaimedButBroken`. `Event::ModelProfileLoaded` gains an optional `capability_row` field; GUI footer renders a tooltip with the full row + a yellow "broken: <list>" badge when any column is `claimed_but_broken`; TUI footer renders the same suffix in `render_help_right_model`.

### Bundle 4 — §14 file-watcher + concurrent-edit + resume + SIGKILL gate

- **File-watcher integration**. New `atelier_core::file_watcher` module with `FileWatcherHandle`, `spawn_file_watcher`, `FILE_WATCH_DEBOUNCE`, `FileWatcherError`. Wraps the `notify` crate; debounces edit bursts; emits `Event::FilesChanged { paths, observed_at }` on the bus. `SessionDispatcher` gains a `file_watcher` field + `with_file_watcher` builder; the dispatcher tracks the read-set from each successful `read_file`/`list_dir`/`grep`/`ast_grep` dispatch via a new `extract_read_paths` helper.
- **Concurrent-edit modal at tool-call boundary**. New `Event::FilesChangedAcknowledged { outcome }` companion event; new `ConcurrentEditPolicy::{Modal, AutoReload}` and `ConcurrentEditOutcome::{Reload, Wait, Pause, AutoReload, PauseTimedOut}` enums; `SessionDispatcher::resolve_concurrent_edit` + new Tauri command `resolve_concurrent_edit`. The dispatcher queues the *next* tool dispatch (spec §14: never cancel mid-stream); the GUI's new `ConcurrentEditModal.svelte` surfaces the user choice; TUI gets a new `InputMode::ConcurrentEditConfirm { paths }` rendered in `render_help` with `r`/`w`/`p` keybinds.
- **Three named options + 5-min auto-pause (PROVISIONAL)**. `Pause` arms a 5-minute `tokio::sleep`; on timeout the resolver task auto-reloads (emits `ConcurrentEditOutcome::PauseTimedOut`).
- **Resume-at-last-completed-tool-call**. New `OnDiskSession::{resume_conversation_prefix, append_conversation_turn}` traversal; new `ConversationEntry` struct. `Runner::with_resume(uuid)` builder loads the on-disk session, replays the conversation prefix as `Event::MessageCommitted` (recovery_log surfaces as `MessageRole::System`), and hands off to the normal turn loop. CLI flag `--resume <UUID>`.
- **`--non-interactive` flag**. New `Runner::with_non_interactive` builder + CLI flag; sets `ApprovalPolicy::AutoApproveAll` + `ConcurrentEditPolicy::AutoReload`. `CliParseResult::Ok` now carries `Box<CliArgs>` to keep the variant size small.
- **Mechanical gate: kill -9 mid-tool-call → restart → state restored**. New integration test `sigkill_then_resume_recovers_partial_state_and_advances_to_done`. Real `kill -9` is platform-specific and CI-flaky; the test instead simulates the post-crash on-disk state (orphan assistant turn + `RecoveryReason::Crash` entry) and verifies the resume path drops the orphan, surfaces the partial output as a System message, and advances to `State::Done`. Equivalent coverage of the resume code; deterministic on CI.

### Workspace test count delta

- atelier-core unit: 633 → 675 (+42)
- atelier-cli unit (lib): 31 → 39 (+8: 6 instrumentation + 2 cost-policy)
- atelier-cli integration: 26 → 37 (+11: 2 pane-visibility, 1 SIGKILL gate, ~8 from B1's `runner` `#[path]` re-include exposing extra integration tests)
- atelier-gui: 24 → 26 (+2: bridges for `MentalModelSnapshot`, `ModelProfileLoaded` capability_row, `ExpansionExecuted`)
- atelier-tui: 84 → 84 (unchanged — TUI mental-model is a hint, not a modal)
- Total: **788 → 861**

### Cross-bundle merge resolution notes

The four worktrees branched from `eac03ec` (post-v60.6 docs) and were merged sequentially in order **B5 → B1 → B2 → B4**. The minor conflicts that needed manual resolution, all on the same load-bearing registries:

- `atelier-cli/src/lib.rs` — additive `pub mod overhead` (B5) + `pub mod instrumentation` (B1).
- `atelier-core/src/session.rs` — Event enum + `kind()` match: additive variants `MentalModelSnapshot` (B1), `FilesChanged` (B4), `FilesChangedAcknowledged` (B4); modified variant `ModelProfileLoaded` gains optional `capability_row` (B2).
- `atelier-core/src/dispatcher.rs` — `SessionDispatcher` struct + `::new` initializer: additive fields `mental_model` (B1) + `file_watcher` (B4).
- `atelier-cli/src/runner.rs` — `Runner` struct + `::new` initializer: additive fields `pane_visibility` (B1) + `cost_policy` + `ModelCostPolicy` enum (B2) + `concurrent_edit_policy` + `resume_from` + `non_interactive` (B4).
- `atelier-gui/src/lib.rs` — `invoke_handler!` macro list + `bridge_event` match: additive Tauri commands + event projections.
- `atelier-gui/ui/src/lib/state.ts` + `App.svelte` — additive type imports + `applyEvent` arms.
- `atelier-tui/src/lib.rs` — additive `apply` arms + `project_event` arms in `render_help`.
- `tests/test_runner.py` — added `.claude` to the excluded-parts filesystem walk so the `test_no_claude_paths_in_tracked_source` lint ignores harness-managed worktrees under `.claude/worktrees/` (runtime-only state, never tracked).

No semantic conflicts — every conflict was a textual collision on a shared registry where the right answer was "keep both additions."

## v60.6 — 2026-05-17 (§5 Expand + drag-and-drop plan reorder)

Closes two Phase C rows in a single release:

1. The §5 **Expand** affordance, the symmetric counterpart to v60.5's compact-only landing. Restores the originals from the on-disk blob, drops the summary card, ledgers the operation, and surfaces the cache-rewarm cost before the user confirms. No schema migration — the v60.5 blob format (`compaction_blob v1`) is the contract.
2. The §3 GUI **drag-and-drop** plan reorder (Phase C `[ ]` row). Replaces the up/down arrow buttons in `PlanPane.svelte` with HTML5 drag-and-drop against the existing `reorder_plan_steps` Tauri mutator. TUI keeps its existing keyboard-driven reorder (no terminal drag).

User-visible:

- **GUI Memory pane**: compaction-generated cards gain a *"compacted from N items · ~T tokens to re-warm"* badge under the title; the row gains an `⤴ expand` button (only when `compacted_from` is set); clicking opens an inline confirm dialog that quotes the exact cache-rewarm cost. Confirm fires the new `expand_memory_card` Tauri command; the toast reads "restored N items · ~T cache tokens re-warmed".
- **TUI Memory pane**: every compaction-flavoured row carries a cyan `[×N, T tk]` suffix so the user can scan for Expand-eligible cards at a glance. `x` (eXpand) on a selected compaction card opens an `EXPAND N items · pays ~T cache tokens` cyan banner; `y` confirms, `n` / `Esc` cancels.
- **GUI Plan pane**: each step gains a `⋮⋮` drag handle on the left; rows are `draggable="true"` with HTML5 `dragstart`/`dragover`/`drop` handlers. Drop target shows a top-border accent indicator; the visual reorder is wholesale-applied on the next `PlanSnapshot` event (no optimistic update). The v55 up/down arrow buttons are removed.

Data layer (atelier-core):

- New `LedgerEntry::Expansion { restored_item_ids, summary_card_id, cache_rewarm_tokens }` variant + matching `Kind::Expansion` discriminator + schema bump in `schemas/session/v1.json` (`kind` enum widened, per-kind `allOf` adds `Expansion` requireds). Like `Compaction`, never carries its own `cost_usd` — `cache_rewarm_tokens` is a prompt-cache disclosure, not a `$` line.
- New `Event::ExpansionExecuted { restored_item_count, summary_card_id, cache_rewarm_tokens }` event. Emitted by the dispatcher mutator after `LedgerAppended(Expansion)` → `ContextItems` → `MemoryCards` snapshots converge; UIs use it as the terminal "show the toast" signal.
- New `CompactionSource.cache_rewarm_tokens: u32` field (optional via `serde(default)` so v60.5-era sessions round-trip as 0). The compaction path now records the freed-tokens sum here so v60.6 Expand can surface the cost without re-reading the blob.
- New `MemoryCardSummary.cache_rewarm_tokens: Option<u32>` projection — set iff `compacted_from` is set, so the bus payload still stays small.
- New `ContextManager::add_batch(items)` — atomic Pass-1 collision check (against both existing state and within-batch duplicates), Pass-2 insert in order. Rejects via the new `ContextError::AlreadyPresent` variant so a buggy double-expand can't silently overwrite a live item.

Dispatcher / orchestration:

- `SessionDispatcher::expand_memory_card(card_id, items, now) -> Result<ExpansionOutput, ExpansionError>` — the new sync mutator. Validates the card exists + has `compacted_from`, validates the items match the recorded ids (count + ids in order), atomically restores via `add_batch`, drops the summary card, appends `LedgerEntry::Expansion`, and emits the bus events in a fixed order.
- `SessionDispatcher::snapshot_memory_card(card_id) -> Option<MemoryCard>` — non-mutating clone for the orchestrator to extract the `compacted_from` link + blob path before calling the mutator.
- New `atelier_cli::expansion::expand(dispatcher, workspace_root, card_id, now)` orchestrator. Composes the three steps (snapshot card → blob read → dispatcher mutator) into one `async` free function the GUI Tauri command + TUI `submit_expand` helper both delegate to. Refuses to act on a blob with the wrong version (`COMPACTION_BLOB_VERSION` mismatch).

Tests landed (~33 new):

- 5 in `atelier-core/ledger.rs`: `Kind::Expansion` wire label, `Expansion` serde + cost + timestamp helpers, `entries_without_cost` excludes Expansion.
- 1 in `atelier-core/session.rs`: `Event::ExpansionExecuted.kind()` pinning.
- 3 in `atelier-core/memory.rs`: `CompactionSource.cache_rewarm_tokens` round-trip, v60.5-era backwards-compat default, `MemoryCardSummary` projection.
- 5 in `atelier-core/context.rs`: `add_batch` happy path with insertion-tail, collision-with-existing rejects atomically, duplicate-within-input rejection, empty-noop, preserves provenance.
- 7 in `atelier-core/dispatcher.rs`: snapshot returns clone / returns None for missing, `expand_memory_card` happy path with full event sequence, unknown card, non-compaction card, item-count mismatch atomic, item-id mismatch atomic, id-collision rolls back via `add_batch`.
- 4 in `atelier-cli/expansion.rs`: happy path round-trip, unknown-card, plain-card, missing-blob.
- 1 in `atelier-gui/lib.rs`: bridge `ExpansionExecuted`.
- 6 in `atelier-tui/lib.rs`: `x`-keybind opens modal only on compacted cards (with inert on plain), `ExpandConfirm` `y`/`n`/Esc, badge rendering, `EXPAND` banner rendering.
- 1 integration test in `atelier-cli/tests/run_integration.rs`: scripted MockAdapter; compact then expand the same items; asserts the full event sequence + the items return with their original ids/tokens/provenance.

Workspace test count: **755 → 788**. `make check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `npm run check` all green.

Drag-and-drop:

- `PlanPane.svelte` exports a pure `reorderArray<T>(arr, from, to)` helper for the splice math (separable from Svelte for future Vitest coverage).
- `<li>` rows are `draggable="true"`; `ondragstart` captures source idx; `ondragover` calls `preventDefault()` to enable drop; `ondrop` calls `reorderArray` and invokes `reorder_plan_steps`. The dispatcher's existing `PlanSnapshot` re-emit drives the visual reorder.

## v60.5 — 2026-05-17 (§5 non-destructive context compaction, compact-only)

Closes the §5 spec promise *non-destructive compaction with cost disclosure* on the compact side; v60.6 lands the matching Expand affordance against the frozen blob format. Compact-only ships a complete contract — the originals are written to disk, ledgered, and pointed at from the summary card — so v60.6 is a UI flip rather than a new wire shape.

User-visible:

- **GUI Context pane**: every row gains a checkbox column (disabled on pinned rows); a "Compact N selected" button surfaces in the pane header once ≥2 items are selected; clicking it opens an inline confirm dialog showing the projected `frees ~Nk tokens`, with a one-line note that the operation is reversible in v60.6. Confirm fires the new `compact_context_items` Tauri command.
- **TUI Context pane**: `space` toggles the focused row's id in a multi-select set (no-op on pinned); `C` (shift-c) opens a `CompactConfirm` modal that renders the same cost disclosure in the help footer; `y` confirms, `n` / `Esc` cancels. A new `[*]` / `[ ]` / `[-]` glyph in the leftmost column shows per-row state.
- **Memory pane** (GUI + TUI): compaction-generated cards carry a small "compacted from N items" hint via the new `MemoryCardSummary.compacted_from` projection.

Data layer (atelier-core):

- New `LedgerEntry::Compaction { freed_tokens, replaced_items, summary_card_id, expansion_blob_path }` variant + matching `Kind::Compaction` discriminator + the schema bump in `schemas/session/v1.json` (`kind` enum widened; per-kind `allOf` adds `Compaction` requireds). Compaction entries never carry their own `cost_usd` — the immediately preceding `ModelCall` records the summary-generation cost.
- New `Event::CompactionExecuted { freed_tokens, replaced_item_count, summary_card_id }` event. Emitted by the dispatcher mutator after `LedgerAppended(Compaction)` → `ContextItems` → `MemoryCards` have already converged the panels; UIs use it as the "clear my multi-select / show the toast" signal.
- New `MemoryCard.compacted_from: Option<CompactionSource>` field (and `CompactionSource { item_ids, expansion_blob_path, compacted_at }` struct) that links the summary card back to the originals + the on-disk blob v60.6 Expand will read. Optional; existing bundled session fixtures round-trip unchanged.
- New `MemoryCardSummary.compacted_from: Option<u32>` projection (item count only) so the bus payload stays small.
- New `ContextManager::evict_batch(&[ContextItemId], evicted_at)` — atomic Pass-1 pin/missing check, Pass-2 evict. Rejects duplicate ids at Pass 1 (the second copy hits the dup guard).

Dispatcher / orchestration:

- `SessionDispatcher::compact_context_items(ids, summary_text, expansion_blob_path, now) -> Result<CompactionOutput, CompactionError>` is the new sync mutator. Validates the summary via the shared `text_safety::validate_user_text`, atomically evicts via `evict_batch`, mints a pinned summary `MemoryCard` carrying the `CompactionSource` link, appends `LedgerEntry::Compaction`, and emits the bus events in a fixed order.
- `SessionDispatcher::snapshot_context_items(&[String]) -> Result<Vec<ContextItem>, ContextError>` — non-mutating clone for the orchestrator to feed `compaction_blob::write` *before* the eviction. Same id-validation as the other dispatcher mutators (`parse_context_item_id`).
- `SessionDispatcher::append_ledger_entry(entry)` — append + broadcast convenience, lets the orchestrator record the summary `ModelCall` without holding its own `Arc<Ledger>` clone.
- New `atelier_cli::compaction::compact(adapter, dispatcher, workspace_root, session_id, ids, now)` orchestrator. Composes the five steps (snapshot → adapter chat → blob write → ledger ModelCall → dispatcher mutator) into one async free function the GUI Tauri command and the TUI `submit_compact` helper both delegate to. Fixed summary system prompt; 16 KiB cap on the response; `MockAdapter::queue_text_response`-friendly so tests pre-seed the summary.
- New `atelier_cli::compaction_blob` module. `write(workspace_root, session_id, compacted_at, items)` persists a `CompactionBlob { version: 1, blob_id, compacted_at, items }` envelope under `<workspace>/.atelier/sessions/<session_id>/compactions/<comp-uuid>.json` via `NamedTempFile::persist`; symmetric `read(workspace_root, relative_path)` for v60.6. Mirrors `memory_promote`'s hardening discipline (canonical containment, session-id hygiene, 4 MiB cap).
- New `atelier_cli::AdapterHandle` — companion to `DispatcherHandle`, with the same `set` / `clear` / Drop-guard lifecycle. Lets the GUI Tauri command + TUI mutation arm reach the live adapter without re-constructing the per-provider adapter.

Tests landed (~44 new):

- 6 in `atelier-core/memory.rs`: serde round-trip with/without `compacted_from`, `CompactionSource` round-trip, `MemoryCardSummary` projection.
- 5 in `atelier-core/context.rs`: `evict_batch` happy path, pin-blocks-all-or-nothing, unknown-id-error, empty-noop, duplicate-id rejection.
- 5 in `atelier-core/ledger.rs`: wire-label test extended with `compaction`, `LedgerEntry::Compaction` serde + cost, `entries_without_cost` excludes Compaction.
- 1 in `atelier-core/session.rs`: `Event::CompactionExecuted.kind()` pinning.
- 9 in `atelier-core/dispatcher.rs`: full `compact_context_items` coverage (happy path, empty, pinned-atomic, unknown-id, malformed-id, Trojan-Source, frontmatter-rejection, snapshot ordering, snapshot-unknown).
- 8 in `atelier-cli/compaction_blob.rs`: round-trip, oversize, path-traversal, non-`.atelier/sessions/` prefix, non-`.json`, parent-dir creation, invalid-session-id, relative-workspace.
- 4 in `atelier-cli/compaction.rs`: happy path (ModelCall + Compaction ledger order), empty-ids skips adapter, oversize-summary rejection, dispatcher-error doesn't leak state.
- 2 in `atelier-gui/lib.rs`: bridge `CompactionExecuted` and `MemoryCards.compacted_from` projection.
- 6 in `atelier-tui/lib.rs`: `space`-toggle (unpinned + pinned), `Shift-C` gating on ≥2 selected, `CompactConfirm` modal `y`/`n`, `apply(CompactionExecuted)` clears selection, `apply(ContextItems)` drops stale selected ids.
- 1 integration test in `atelier-cli/tests/run_integration.rs`: scripted MockAdapter; asserts the full event sequence + the on-disk blob round-trips back to the original `ContextItem`s.

Workspace test count: **711 → 755**. `make check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `npm run check` all green.

Deferred to v60.6:

- Expand: `MemoryPane.svelte` button + `SessionDispatcher::expand_memory_card` mutator + `compaction_blob::read` consumer + the cache-rewarm cost disclosure on the expand confirm.

## v57–v60 — 2026-05-17 (four-round audit / fix sweep)

Four consecutive deep-scan / fix rounds against the v56 codebase. Each round produced a synthesised audit report (bugs / smells / security in parallel) and closed every non-LOW finding in the next round. Trajectory:

| Round | CRITICAL | HIGH | MEDIUM | LOW |
|-------|---------:|-----:|-------:|----:|
| v56 scan (post-§3 sweep) | 1 | 8 | 12 | ≥10 |
| v57 scan | 0 | 3 | 8 | ~10 |
| v58 scan | 0 | 1 | 6 | 10 |
| v59 scan | 0 | 2 | 4 | 8 |
| **v60 scan (final)** | **0** | **0** | **0** | **6** (deferred) |

Net: **45+ correctness / security / hygiene fixes** with **~150 new tests** pinning the regressions. Workspace went from 689 → 711 tests; the rig (`make check`) stays green throughout. The remaining open items are six deferred smells (justified or low-impact: `ConversationRole`/`MessageRole` duplication, speculative `CheckpointHook`/`LedgerHook`, Svelte `each`-by-index key on bounded list, `AppState::default()` zero-sentinel, `with_approval_policy` mem::replace style, version-marker comment noise).

### v60 — final fix sweep (this version)

Closes the six MEDIUM-and-above findings from the v59 audit and consolidates the v57/v58/v59 disciplines into single sources of truth.

- **HIGH-bug-1 / HIGH-bug-2: plan-text validation gaps.** `PlanCanvas::apply_envelope` (model-controlled) and `PlanCanvas::from_vec` (snapshot-reload) both bypassed v59's `validate_plan_text`. Closed by a new `plan::validate_plan_step_text` helper invoked from both paths; bad ops drop with reason via `ApplyReport`, bad snapshots fail to load with `PlanError::InvalidContent`. Tests for each.

- **Security M-1: TUI promote_memory_card bypass.** The TUI `Mutation::PromoteMemory` had a copy-paste of the *pre-v58* unvalidated disk writer; the GUI got v58+v59 hardening but the TUI didn't. Closed by extracting `atelier_cli::memory_promote::write_promoted_card` and routing both drivers through it. The shared helper enforces HOME absolute + canonicalize != `/` (closes audit L-2: multi-slash / relative HOME bypass), filename hygiene (no separators / leading-dot / control chars / `..`), per-call size cap, canonical-root containment via `canonicalize(target.parent())`, and atomic `NamedTempFile::persist`. 4 unit tests.

- **Security L-3: Refusal > ToolUse priority.** v59's `merge_stop_reason` ranked ToolUse above Refusal so a server emitting both `content_filter` and `tool_calls` would dispatch the tool. v60 inverts this — Refusal is hard-overriding by spec. Direct table-driven test pins every pair; new integration test for the reverse-order finish_reason case.

- **MED-A: shared text-safety predicate.** Three byte-for-byte copies of the Trojan-Source / control-char rule set across `dispatcher.rs` and `memory.rs`. Lifted into a new `atelier_core::text_safety` module (`is_disallowed_control`, `validate_user_text`). Memory + plan + future free-form text validators all delegate. Adding a new disallowed code point is now one edit. Module ships with its own exhaustive C0/DEL/C1/separator/bidi tests.

- **MED-B + MED-C: wire_label discipline on two more enums.** Added serde-agreement tests for `SideEffectClass::as_str` and `HookEvent::as_str`, mirroring the v58/v59 discipline on `Provenance` / `Payload` / `TokenSource` / `PlanStatus` / `ClaimedChangeKind` / `MessageRole` / `ProbeLoadOutcome`. Every enum that crosses the bus or the schema boundary now has a test asserting the hand-written label matches the serde rename projection.

### v59 — fix sweep responding to v58 audit

Closed the v58 audit's HIGH (TUI Debug-as-wire residual, GUI projectEvent label drift, OpenAI duplicate-completion stop_reason clobber) plus 7 MEDIUM items.

- **H7-residual:** TUI `project_event` `detail` strings still used Rust `Debug` for `MessageRole` / `State`. Routed through `wire_label()` / `State::name()`.
- **GUI projectEvent drift:** Svelte event-log emitted pre-v57 short labels (`PendingApproval`, `IllegalTransition`, `ModelProfile`); v59 routes `kind` from the BridgedEvent's canonical `kind` field set by Rust's `SessionEvent::kind()`.
- **H4-residual:** OpenAi adapter latches stop_reason on first non-None — duplicate `finish_reason` chunks no longer clobber `tool_calls` with `stop`.
- **M-sec-6:** Splice path re-validates symlink containment at commit time. The pre-v59 stage/commit gap could be exploited by a concurrent process planting a symlink between stage and approval.
- **M-sec-1b:** `write_file` (`MAX_WRITE_BYTES = 16 MiB`) + `edit_file` (`MAX_EDIT_NEW_TEXT_BYTES = 16 MiB`) per-call size caps applied at the args boundary.
- **M-sec-2 partial + regression:** `promote_memory_card` rejects `.` / `..` / leading-dot relative paths and canonicalizes `target.parent()` against the memory root. Held `tempfile::TempDir` in `SessionState` so RAII cleans the per-process workspace on shutdown (v58 `.keep()` was leaking the parent dir).
- **L-sec-1:** `read_file` streams via `File::open` + `seek` + `take(MAX_READ_BYTES).read_to_end` — no full slurp before the cap is consulted. A 50 GB file is now correctly capped.
- **L-sec-2 / L-sec-3:** `SECRET_KEY_SUBSTRINGS` expanded for cloud creds (AWS, GitHub PATs, cookies, bearer); `validate_memory_card_content` extended with U+2066–U+2069 bidi isolate codepoints.
- **wire_label discipline:** added agreement tests across `Provenance`, `Payload`, `TokenSource`, `PlanStatus`, `MessageRole`, `ProbeLoadOutcome`, `ClaimedChangeKind`. Producer + serde projections can no longer drift.
- **Plumbing:** `merge_stop_reason` priority-aware; `MemoryStore::from_vec` validates content; `SessionState.workspace_root` retired in favour of `workspace_root()` accessor; provenance_badge exhaustiveness test on the TUI side.

### v58 — fix sweep responding to v57 audit

Closed the v57 audit's CRITICAL (commit_selected_hunks atomicity), 7 of 8 HIGH, and 5 of 8 MEDIUM items.

- **C1:** `commit_selected_hunks` is now two-pass — splice + mkdir in Pass 1, rename in Pass 2. A splice failure no longer leaves Pass-1 files already renamed. Regression test pins this.
- **H1:** `PendingApprovalGate` registers a `PendingEntryGuard` Drop guard so a cancelled dispatch future doesn't leak a oneshot::Sender in the pending map.
- **H2:** `WriteFile`'s `bytes_written` now uses `content.len()` (was deriving from `Hunks::Created` only — returned 0 for any overwrite).
- **H4:** OpenAiCompatAdapter dedupes `ToolCallCompleted` on duplicate finish_reason chunks via a `block.completed` flag.
- **H8 (security):** `Shell` tool's `cwd` now passes through `ensure_inside_workspace_existing` — closed the symlink-escape parallel to the v55 file tools.
- **H5 / H6 / H7:** wire-format hygiene. `SessionEvent::kind()` canonical labels; `now_rfc3339` lifted into `atelier_core::time` (was 3 copies); `MessageRole::wire_label` + `State::name` + `ProbeLoadOutcome::wire_label` replace `Debug` as the wire format.
- **M-sec-1 through M-sec-5:** Tauri command size caps; `promote_memory_card` hardening (canonicalize + atomic NamedTempFile::persist + size cap); `read_file` `MAX_READ_BYTES = 4 MiB`; hook payload secret redaction (`SECRET_KEY_SUBSTRINGS`); memory card content rejects NUL/control bytes + `---` frontmatter delimiter.
- **L cleanup:** `ContextError::Malformed` distinct from `NotFound`; `start_demo_run` uses `tempfile::TempDir`; `kill_process_group` uses `i32::try_from(pid)`; `submit_approval` validates path keys at the IPC boundary; sandbox profile rejects control bytes in repo paths.

### v57 — fix sweep responding to v56 audit

Closed the v56 audit's CRITICAL + most HIGH/MEDIUM items.

- **H6 / H7 / H8:** lifted `now_rfc3339`, started Debug→serde wire transition, Shell symlink containment.
- **M-bug-1 through M-bug-3:** envelope parse errors log via `tracing::warn`; `with_approval_policy(AutoApproveAll)` reverts the gate (was a no-op); initial `ContextItems` snapshot emitted before turn loop.
- Multi-round audit kicked off here.

## v56 — 2026-05-17

**§3 surface close-out.** Three checklist rows tick to `[x]` in one cohesive change: hunk rewrite (sub-file accept/reject), the production-scale 10-file mechanical gate test, and "Why this change?" UI rendering the envelope's `claimed_changes` rationale next to each diff. The §3 row count drops from six open to three (drag-and-drop, inline Mermaid/D2/images, UX-target measurement — all GUI-only finishing touches).

### Hunk rewrite (sub-file accept/reject)

The pre-v56 commit contract was file-level — accept the entire staged file or reject it whole. v56 widens it so the user can keep some hunks of a Lines diff and reject others; the staging layer splices pre-image lines for rejected hunks against new lines for accepted hunks and writes the spliced bytes through the existing rename phase.

- **`crates/atelier-core/src/staging.rs`** — new `FileApproval { All | Hunks(Vec<usize>) }` enum + `HunkSelection = HashMap<PathBuf, FileApproval>` type alias. `StagedBatch` retains the pre-image bytes (`pre_images: BTreeMap<PathBuf, Option<Vec<u8>>>`) captured during `stage()` so partial-hunk commits can splice without a second read. New primary commit method `commit_selected_hunks(&HunkSelection)`; the pre-v56 `commit_selected(&HashSet<PathBuf>)` is retained as a thin file-level wrapper. New private `splice_hunks(pre, new, hunks, accepted)` uses `str::split_inclusive('\n')` so the file's trailing-newline convention survives the splice. For non-Lines hunk kinds (`Created` / `Deleted` / `Binary` / `Same`) per-hunk indices are meaningless — non-empty selection falls back to `All`, empty selection drops the file. 9 new tests: file-level parity, partial splice, drop-on-empty-Lines, created-fallback, omitted-path-is-rejected, invalid-index filtering, trailing-newline preservation (both with and without the final newline).

  **Trade-off documented**: a partial-hunk splice is NOT re-validated against the syntax check (the pre-commit check ran against the agent's full new file). A spliced output may parse-fail; the UI is on the hook to surface this if it becomes a real issue.

- **`crates/atelier-core/src/dispatcher.rs`** — `ApprovalGate::approve` widened from `Vec<PathBuf>` to `HunkSelection`. `AutoApprove` builds an `All` selection over every pending file (behaviour unchanged). `PendingApprovalGate` parks a `oneshot::Sender<HunkSelection>` (was `Sender<Vec<PathBuf>>`) and `SessionDispatcher::submit_approval(commit_id, HunkSelection)` is the new wire signature. `submit_approval_files(commit_id, Vec<PathBuf>)` retained as a file-level compat wrapper so existing callers (TUI's `submit_pending`, integration tests) keep their existing call sites. New dispatcher integration test (`submit_approval_with_per_hunk_selection_routes_to_commit_selected_hunks`) drives a 2-hunk file end-to-end through the AwaitApproval gate, accepts hunk 0, rejects hunk 1, asserts the on-disk content is the spliced result.

- **`crates/atelier-gui/src/lib.rs`** — `submit_approval` Tauri command's payload changes from `accepted: Vec<String>` to `selection: HashMap<String, FileApprovalWire>` where `FileApprovalWire` is a tagged enum (`{"mode":"all"}` or `{"mode":"hunks","indices":[…]}`).

- **`crates/atelier-gui/ui/src/lib/components/DiffPane.svelte`** — pending-approval UI replaces the per-file checkbox row with a file row + indented per-hunk checkbox list for Lines diffs. File-level checkbox toggles all hunks in lockstep; toggling individual hunks updates a `fileChecked` reflection (any-hunk-checked = file-included). The "accept selected" button submits the live toggle state as the new wire shape; "reject all" sends an empty selection. Hunk rows show `@@ -old,len +new,len @@` plus `−N / +M` counts so the user can pick from a glance.

- **TUI deferred**: the terminal pending banner continues to ship file-level `y`/`n` approval via `submit_approval_files`. A per-hunk picker in the TUI needs a per-hunk focus + selection model layered on top of the v55 pane-focus model — a meaningful UX problem that deserves its own session, mirroring how v55's editable Memory and Plan flows trimmed to GUI-only for some interactions.

### §3 10-file mechanical gate

- **`crates/atelier-cli/tests/run_integration.rs`** — `v56_phase_c_mechanical_gate_at_ten_files_lines_up_live_diff_and_final_state` scripts a MockAdapter run with 10 sequential `write_file` tool calls + a final `claimed_done` envelope. Asserts: report shows 11 turns (10 writes + done); each on-disk file is byte-equal to the reference; exactly 10 `EditStaged` events on the bus, in commit order matching the scripted path sequence. The pre-v56 3-file `run_scripted_multi_file_rename_drives_phase_c_mechanical_gate` is retained as a brisker smoke test.

### "Why this change?" UI (claimed_changes rationale)

- **`crates/atelier-core/src/session.rs`** — new `Event::ClaimedChanges { changes: Vec<ClaimedChangeSummary> }` variant + matching `ClaimedChangeSummary { path, kind, summary }` struct (kind flattened to a string so consumers don't import the protocol enum just to render badges).

- **`crates/atelier-cli/src/runner.rs`** — the turn loop emits `Event::ClaimedChanges` whenever the envelope carries `claimed_changes`. Renders alongside the existing `PlanSnapshot` emission point so all per-turn rationale arrives in one coherent batch.

- **`crates/atelier-gui/src/lib.rs`** — `bridge_event` adds a `ClaimedChanges` arm projecting each entry as `{path, kind, summary}` JSON. New unit test `bridge_claimed_changes_passes_per_file_summary` covers the projection.

- **`crates/atelier-gui/ui/src/lib/state.ts`** — `AppState.claimedChanges: Record<string, string>` (path → summary). New reducer arm wholesale-replaces the map on each event; `projectEvent` shows "N file rationale(s)" in the event log tail.

- **`crates/atelier-gui/ui/src/lib/components/DiffPane.svelte`** — renders a `why:` line under each file header when `claimedChanges[path]` is set. Styled as dim italic so it sits visually behind the diff content.

- **`crates/atelier-tui/src/lib.rs`** — new `AppState.claimed_changes: HashMap<String, String>` field. `apply` arm replaces the map; `render_diff` shows the rationale as a dim-italic line under the file header. `project_event` adds a `ClaimedChanges` event-log entry.

- **`crates/atelier-cli/tests/run_integration.rs`** — `v56_envelope_claimed_changes_surfaces_as_bus_event` builds an envelope with `claimed_changes`, runs the MockAdapter, asserts the bus carries a `ClaimedChanges` event with the matching path/kind/summary.

### Out of scope (deliberate)

- The envelope's other rationale field, `grounding` (textual-claim citations to `tool:read` / `tool:grep` / `context:file` / `guess`), is a different surface — sidebar / inline span annotations — and lands separately.
- Per-hunk TUI picker (see Hunk rewrite section). File-level `y`/`n` continues to work via the `submit_approval_files` compat wrapper.

## v55 — 2026-05-17

**§5 editable round-trips.** Closes the three `[~]` items in the §5 build tracker by adding the write-back path the panels were missing: pin / unpin / evict on context items, add / delete / promote on memory cards, add / status-cycle / constraint / reorder / remove on plan steps. The data layer (`ContextManager::{pin,unpin,evict}`, `MemoryStore::{add,evict,promote_to_global}`, `PlanCanvas::{add,mark_status,add_constraint,reorder,remove}`) was already pure-rust since v44; v55 wires it through the dispatcher to both UIs.

One pre-requisite refactor landed alongside: the Runner now owns a real `Arc<parking_lot::Mutex<ContextManager>>` populated as messages append, replacing the v53 `summarise_messages(&messages)` transcript projection. Pin / evict on a transcript projection have no semantics; pin / evict on the manager do.

### Plumbing (`atelier-core`)

- **`crates/atelier-core/src/context.rs`** — new `Provenance::AssistantTurn` variant + matching `ContextItemSummary` mapping (renders as `"assistant_turn"` per the existing GUI badge). Round-trip test added to the variants-roundtrip suite.

- **`crates/atelier-core/src/dispatcher.rs`** — `SessionDispatcher` gains three `Arc<parking_lot::Mutex<…>>` fields (`context_manager`, `memory_store`, `plan_canvas`) and a `with_shared_state(...)` builder. `new()` seeds each with a fresh empty instance so the unit-test surface is unchanged. 11 new mutator methods: `pin_context_item`, `unpin_context_item`, `evict_context_item`, `add_memory_card`, `delete_memory_card`, `promote_memory_card`, `add_plan_step`, `remove_plan_step`, `mark_plan_step_status`, `add_plan_step_constraint`, `reorder_plan_steps`. Each acquires the lock, calls the pure data-layer op, drops the lock, then re-emits the matching Snapshot event. `evict_context_item` additionally appends `LedgerEntry::cache_bust_from(&event)` to the ledger and emits `Event::LedgerAppended` so the cost meter ticks. 14 new tests covering happy path, idempotency, unknown-id error, and pinned-cannot-evict-without-ledger.

- **`crates/atelier-core/src/ledger.rs`** — `cache_bust_from`'s match exhausts the new `Provenance::AssistantTurn` variant (label `"assistant-turn"`).

### Runner (`atelier-cli`)

- **`crates/atelier-cli/src/runner.rs`** — `Runner::run` constructs `Arc<Mutex<ContextManager>>` / `Arc<Mutex<MemoryStore>>` / `Arc<Mutex<PlanCanvas>>` once and clones the Arcs into the `SessionDispatcher` via `with_shared_state(...)`. Each message append (user prompt at start, assistant after chat, tool result after dispatch) now also adds a `ContextItem` to the manager via three small private helpers: `context_item_for_user_prompt`, `context_item_for_assistant_turn`, `context_item_for_tool_result` (each maps to the right `Provenance` variant and tags `TokenSource::Approx` chars/4 counts). `Event::ContextItems` payload now comes from `context_manager.lock().summarise()` instead of `summarise_messages(&messages)`. The old projection + its 5 tests have been deleted; 4 new tests cover the helpers' provenance + token mapping.

### GUI

- **`crates/atelier-gui/src/lib.rs`** — 11 new Tauri commands mirror the dispatcher mutators (one per mutator), plus a `require_dispatcher(state)` helper that 404s when no run is in flight. `promote_memory_card` does the actual disk write under `~/.atelier/memory/<relative_path>` so the data layer stays I/O-free. Wire-format status strings (`"pending"` / `"in_progress"` / `"done"` / `"skipped"`) are parsed into `PlanStatus` via `parse_plan_status`; unknown labels are rejected rather than coerced. 2 new tests on the parser.

- **`crates/atelier-gui/ui/src/lib/components/ContextPane.svelte`** — per-row 📌/un-📌 toggle + ✕ evict button. The evict button opens an inline confirm card ("evict — frees ~N tokens. ledgered as cache-bust.") with confirm/cancel; confirm calls `evict_context_item` and surfaces "evicted — freed N tokens" in a 4-second toast.

- **`crates/atelier-gui/ui/src/lib/components/MemoryPane.svelte`** — top textarea + add button; per-row "↑ promote" and "✕" delete buttons. Promote shows "promoted → /path/to/file.md (N bytes)" in a toast.

- **`crates/atelier-gui/ui/src/lib/components/PlanPane.svelte`** — top text input + add button; per-row status cycler button (the glyph itself is the button — cycles `pending → in_progress → done → skipped → pending` on click), `↑` / `↓` reorder arrows, `+c` add-constraint (opens an inline form), `✕` remove.

### TUI

- **`crates/atelier-tui/src/lib.rs`** — `AppState` gains `focused_pane: FocusedPane`, `selected_context`/`selected_memory`/`selected_plan: usize`, and `input_mode: InputMode`. `FocusedPane::next()` is the Tab cycler. `InputMode` has three variants: `Normal`, `TextInput { kind: TextInputKind, buffer: String }`, `EvictConfirm { id: String }`. `handle_key`'s signature changed from `(KeyEvent, Option<&PendingApproval>)` to `(KeyEvent, &AppState)` so it can dispatch on focused pane + modal state. New keybindings (Normal mode): Tab cycles panes; `j`/`k` (or arrow keys) navigate within the focused pane. Per-pane mutator keys: Context = `p`/`u`/`e`; Memory = `a` (add modal) / `d` / `P`; Plan = `a` (add modal) / `space` (cycle status) / `c` (constraint modal) / `x`. Modal sub-modes grab keys before pane bindings — text-input modals append chars / backspace / Enter to submit / Esc to cancel; evict-confirm consumes `y` (confirm) / `n` or Esc (cancel). 12 new pure-fn unit tests on the keybind decoder + focus + select state. Mutations flow through a new private `submit_mutation` helper that mirrors `submit_pending`.

### Integration tests

- **`crates/atelier-cli/tests/run_integration.rs`** — 3 new end-to-end round-trips drive a scripted `MockAdapter` run, wait for the relevant snapshot event, invoke a dispatcher mutator via `DispatcherHandle::get()`, and assert that a follow-up snapshot reflects the change: `v55_pin_context_item_round_trips_through_dispatcher`, `v55_add_memory_card_round_trips_through_dispatcher`, `v55_mark_plan_step_done_round_trips_through_dispatcher`.

### Deferred (deliberately out of scope)

- Memory card in-place content edit (UI form-state machinery; add + delete + promote prove the round-trip).
- Plan drag-and-drop reorder (the up/down arrow path covers the contract; drag-and-drop is a separate §3 GUI-only checklist row).
- Non-destructive compaction / mental-model panel (separate §5 rows in the build tracker, untouched here).

## v54 — 2026-05-17

**§5 Memory panel.** Companion to v53's Context panel: cards on the bus, rendered in the top-right column of both UIs above what the agent is about to do (Plan) — Memory is what the agent knows long-term, Plan is what it's about to act on. The `MemoryStore` data layer was already in `atelier-core` since v44; v54 adds the bus projection (`MemoryCardSummary` + `Event::MemoryCards`), wires the Runner to publish a snapshot per turn boundary, and lands matching Svelte + ratatui panels. The Runner ships an empty card list today (no card source is wired yet — no add-card tool, no session-replay loader); the event surface is in place so any future card source is purely additive.

Plus a small README cleanup: §6 "Running against a local LLM" merged into the **Quick start** (which already showed the openai-compat one-liner) so users hit the local-LLM walkthrough at the top of the file instead of after the deeper configuration material.

### New surface

- **`crates/atelier-core/src/memory.rs`** — `MemoryCardSummary` flat projection of `MemoryCard`:
  - `title` = first non-empty line of `content` (markdown convention).
  - `body_preview` = remaining text, capped at `MEMORY_BODY_PREVIEW_CHARS = 200` with a trailing ellipsis when truncated.
  - `created_at`, `last_used`, `pinned` carried through verbatim.
  - `MemoryStore::summarise()` materialises the per-card list in insertion order.
  - 8 new tests cover title extraction (incl. leading-blank-line skip), preview truncation at the cap, empty/single-line edge cases, pinned + timestamp pass-through, insertion-order preservation, and serde round-trip.

- **`crates/atelier-core/src/session.rs`** — new `Event::MemoryCards { cards: Vec<MemoryCardSummary> }` variant. Emitted at the same turn boundary as `ContextItems` so the two §5 panels (context = per-turn, memory = durable) update coherently.

- **`crates/atelier-cli/src/runner.rs`** — per-run `MemoryStore::new()` (empty for now); `Event::MemoryCards { cards: memory_store.summarise() }` broadcast alongside `Event::ContextItems` after each turn. The empty snapshot is intentional — surfaces the "no memory cards yet" placeholder so the panel is visibly idle rather than indistinguishable from a broken render.

- **`crates/atelier-gui/`** — new Svelte `MemoryPane.svelte`:
  - One row per card: optional pin glyph (📌), title (bold), compact `YYYY-MM-DD HH:MM` "last used" badge on the right, two-line body preview clamped via `line-clamp: 2` (with `-webkit-line-clamp` for browser compatibility).
  - Tooltip carries full id + created/last-used timestamps so the panel surface stays compact.
  - Pinned rows get a subtle yellow accent — mirrors ContextPane.
  - Wired into `App.svelte`'s top-right slot stacked under `PlanPane` via a `plan-stack` CSS grid (`auto / 1fr` so Plan keeps fit-content height and Memory takes the flex space).
  - `bridge_event` projection passes `MemoryCardSummary` through `serde_json::to_value` (snake_case wire shape, directly renderable). 1 new bridge test.
  - `state.ts`: `MemoryCardSummary` type, `applyEvent` reducer arm (wholesale-replace policy mirroring `ContextItems`), `initialState.memoryCards: []`.

- **`crates/atelier-tui/`** — new `render_memory_pane`:
  - Top-right column split vertically 50/50 between Plan (top) and Memory (bottom) — mirrors the GUI's stack and keeps both §5 surfaces in the highest-visibility column.
  - Per row: pin glyph, title (bold + white when pinned), compact `YYYY-MM-DD HH:MM` last-used timestamp. Body preview deliberately omitted — the TUI row budget is tighter than the GUI's, and title + last-used are the high-value scanning fields.
  - `short_timestamp(iso)` helper trims ISO 8601 to date + hh:mm; tolerates non-ISO input by passing it through verbatim so a malformed timestamp is visible rather than dropped.
  - `AppState.memory_cards` field; `apply` arm with wholesale-replace; `project_event` arm yields `"MemoryCards N cards"` event-log line. 3 new tests.

### README cleanup

- **`README.md`** — §6 "Running against a local LLM" merged into **Quick start** as a subsection. The walkthrough (Ollama install + `--provider openai-compat --base-url …` invocation + other-servers table + probe-on-first-use note) now lives at the top of the file. §5 (Configure with providers.toml) stays where it is as the deeper configuration reference; the Quick start subsection links to it.

### Demo flow

```sh
$ atelier run "<prompt>"
…
# Bus emits, per turn:
#   ContextSnapshot { known_tokens, unknown_tokens }       (aggregate meter)
#   ContextItems { items: [system_prompt, user_message, …] }  (per-row Context panel)
#   MemoryCards { cards: [] }                              (per-row Memory panel — empty until a source wires in)

# GUI top-right column:
#   ┌─ Plan ──────┐
#   │ • step 1    │   plan canvas tree (existing v44)
#   │ • step 2    │
#   ├─ §5 Memory ─┤
#   │ no memory   │   empty state until a card source is wired
#   │   cards yet │
#   └─────────────┘

# TUI top-right column has the same split via Layout::default()
# .direction(Vertical).constraints([Percentage(50), Percentage(50)]).
```

### Verified

- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` → **atelier-core 506** (+8 from `memory::MemoryCardSummary`) + **atelier-cli 19** + **atelier-gui 15** (+1 bridge) + **atelier-tui 65** (+3 panel) = **605 passing**.
- `make check` — schemas + 52 artifacts + 112 rig tests + 11 dry-runs all OK.
- `npm run check` in `crates/atelier-gui/ui/` — 96 files (+1 `MemoryPane.svelte`), 0 errors, 0 warnings.
- `cargo doc --workspace --no-deps` emits 0 warnings.

### §5 mechanical gate status (post-v54)
- ✅ Context-panel API (v53)
- ✅ Mechanical gate: API assertions for token counts + why-here (v53) + cache-bust ledger entry on eviction (v44)
- ⏳ Pin / unpin / evict UI round-trip — data layer + render done; UI buttons + dispatcher round-trip deferred
- ✅ **Memory panel: editable cards + last-used + one-click promote** (v54 — view path; the "editable" + "one-click promote" UI round-trips land with the pin/unpin UI work)
- ⏳ Plan canvas editing
- ⏳ Non-destructive compaction; expansion gated with cost disclosure
- ⏳ Mental-model panel

## v53 — 2026-05-17

**`.atelier/providers.toml` (named profiles) + §5 Context panel.** Two pieces landed together: the v52 single-provider config is reshaped into a multi-profile TOML with a `default` selector and a `--profile` CLI flag, and the GUI + TUI gain a §5 Context panel showing per-item token counts + provenance ("why is this in my agent's head?"). The §5 panel ties off one of the few remaining stated Phase C mechanical gates (`API assertions for token counts and why-here; cache-bust ledger entry on eviction`).

### TOML format change — v52 `config.toml` → v53 `providers.toml`

**Breaking change** against the v52-introduced format. v52 was committed only days earlier and not in the wild, so this is a clean rewrite rather than a migration.

```toml
# .atelier/providers.toml

default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"

[providers.cloud]
provider = "anthropic"
model    = "anthropic:claude-opus-4-7"

[runner]
max_turns = 32

[probe]
policy = "auto"
```

What changed:

| v52 | v53 |
|---|---|
| `.atelier/config.toml` | `.atelier/providers.toml` |
| Single `[provider]` table | `[providers.<name>]` map + `default` selector |
| Field name `kind` | Field name `provider` |
| `--no-probe`/`--force-probe` only | adds `--profile <NAME>` |

Why: a real harness session often wants more than one provider on hand — local LLM for fast iteration, cloud-hosted Anthropic for hard reasoning, a vLLM cluster for production-flavoured runs. v52's single-provider shape forced editing the file (or all the CLI flags) on every switch. v53 keeps every profile in one file and switches between them with `--profile <NAME>`. The `default` field picks which profile applies when `--profile` isn't passed; per-field CLI flags (`--provider`, `--model`, `--base-url`, …) still override individual fields of the resolved profile.

### New surface

- **`crates/atelier-core/src/config.rs`** rewritten:
  - `ProvidersConfig` document: `default: Option<String>`, `providers: BTreeMap<String, ProviderProfile>`, plus top-level optional `[runner]` and `[probe]` sections. `serde(deny_unknown_fields)` everywhere.
  - `ProviderProfile` with `provider`/`model`/`base_url` fields (all optional so a half-populated profile can layer with CLI flags).
  - `ProvidersConfig::resolve_profile(cli_profile)` — CLI > `default` > none. Returns `Result<Option<(name, &profile)>, ConfigError>` so a typo in `--profile` lists the available profiles instead of falling through silently.
  - `validate()` enforces two cross-section invariants: `default` references an existing profile, and `base_url` requires `provider = "openai-compat"`. Each carries a path + a typed error.
  - 19 unit tests (up from 14) cover the user's example verbatim, kebab/lowercase wire forms, discovery, malformed-file rejection, missing-default-name rejection, missing-profile rejection, base_url+wrong-provider rejection, base_url-without-provider allowed (CLI may supply later), round-trip through serde, and the three resolution paths (CLI / default / neither).

- **`crates/atelier-cli/src/main.rs`** — new `--profile <NAME>` flag. `parse_cli` extended; `resolve_provider_choice` now takes a resolved `Option<&ProviderProfile>` instead of the v52 `Option<&ProviderSection>`. On every run the binary prints `atelier run: using config <path> (profile "<name>")` so the active resolution is visible.

### §5 Context panel — per-row "what's in my agent's head"

- **`crates/atelier-core/src/context.rs`** — new `ContextItemSummary` flat projection of `ContextItem`:
  - `kind`: `"file_ref"` / `"inline_text"` / `"blob_ref"`.
  - `label`: file path / first-80-chars-of-text-plus-ellipsis / sha-prefix.
  - `provenance` + optional `provenance_detail`: the why-here trace.
  - `tokens` + `token_source`: count and reliability label.
  - `ContextManager::summarise()` → `Vec<ContextItemSummary>` in insertion order.
  - 7 new tests cover each `Payload` variant's label shape, each `Provenance` variant's mapping, insertion-order preservation, and round-trip through serde.

- **`crates/atelier-core/src/session.rs`** — new `Event::ContextItems { items: Vec<ContextItemSummary> }` variant. Emitted at the same turn boundary as the existing `ContextSnapshot` so the aggregate meter denominator and the per-item rows stay coherent.

- **`crates/atelier-cli/src/runner.rs`** — `summarise_messages(&[Message]) -> Vec<ContextItemSummary>` helper. Maps each `Role` onto a provenance label (`System → initial`, `User → user_attached`, `Assistant → assistant_turn`, `Tool → tool_result` with the message's `tool_call_id` as `provenance_detail`). Token attribution is `chars/4` tagged `approx` — honest about being a rough number. Emitted alongside `ContextSnapshot` after each turn. 5 unit tests.

- **`crates/atelier-gui/`** — new Svelte `ContextPane.svelte` component renders rows with right-aligned token counts (cyan exact / yellow approx / dim unavailable), short provenance badges (`init`/`usr`/`tool`/`mem`/`pin`/`asst`), and the item label. Empty-state placeholder before the first event. Wired into `App.svelte`'s bottom-right slot stacked under the existing aggregate `MetersPane` (CSS grid `auto / 1fr` so meters keep fixed height; context takes the flex space). `bridge_event` projects `ContextItems` through `serde_json::to_value(ContextItemSummary)` so the webview gets the wire shape verbatim — no second mapping layer. 1 new bridge test.

- **`crates/atelier-tui/`** — new `render_context_pane` renders the same panel in the right column between the context gauge and the bounded event log tail. Pane title `§5 Context`; rows use the same colour palette as the GUI for cross-surface consistency. `AppState.context_items` is replaced wholesale on every `ContextItems` event (snapshots come at every turn boundary; a stale partial render is never preferable to the fresh snapshot). Constraint shape tightened to `[Length(2), Length(2), Min(2), Length(4)]` so the cost + context gauges keep their full 2-row allocation even in tight test areas. 5 new tests + project_event coverage.

### Demo flow

```sh
# v53 single-file, multi-profile config:
cat > .atelier/providers.toml <<EOF
default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"

[providers.cloud]
provider = "anthropic"
model    = "anthropic:claude-opus-4-7"
EOF

$ atelier run "add a hello() function"
atelier run: using config /Users/you/proj/.atelier/providers.toml (profile "local")
…
# Bus emits: ModelProfileLoaded { strategy: JsonSentinel, outcome: CacheHit }
# Bus emits: ContextItems { items: [system_prompt, user_message, assistant_turn, …] }

# Flip to cloud for one run, same file:
$ atelier run --profile cloud "now do the hard version"
atelier run: using config /Users/you/proj/.atelier/providers.toml (profile "cloud")
```

### Verified

- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` → **atelier-core 498** (+12 from v52: +7 ContextItemSummary, +5 resolver/discovery deltas) + **atelier-cli 19** (+5 summarise_messages) + **atelier-gui 14** (+1 bridge) + **atelier-tui 62** (+10 panel + project_event + layout) = **593 passing**.
- `make check` — schemas + 52 artifacts + 112 rig tests + 11 dry-runs all OK.
- `npm run check` in `crates/atelier-gui/ui/` — 95 files (+1 for `ContextPane.svelte`), 0 errors, 0 warnings.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 498 atelier-core unit tests + 19 atelier-cli integration tests + 14 atelier-gui unit tests + 62 atelier-tui unit tests** (atelier-core +12, atelier-cli +5, atelier-gui +1, atelier-tui +10 from v52).

### §5 mechanical gate status
- ✅ **API assertions for token counts** — `ContextItems` event ships per-item `tokens` + `token_source`, asserted in tests across all four crates.
- ✅ **API assertions for why-here per item** — `provenance` + `provenance_detail` ship in every row; mapped from `context::Provenance` (and `summarise_messages` for the runtime path); tests assert stable labels.
- ✅ **Cache-bust ledger entry on eviction** — landed in v44 (`ContextManager::evict` returns `CacheBustEvent`; `Ledger::cache_bust_from` writes it). Not new in v53, but the panel makes it visible.
- ⏳ **Pin / unpin / evict with cache-bust confirm** in the UI — data layer is there (`ContextManager::{pin, unpin, evict}`); the UI buttons are deferred.
- ⏳ **Memory panel** — separate work item.

## v52 — 2026-05-17

**`.atelier/config.toml` + model badge in the GUI/TUI footer.** Atelier's runtime knobs — which BYOM adapter, which model, which base URL, max turns, probe policy — now live in a small TOML file the binary picks up automatically. Per-repo override (committed) → user-scope fallback (`~/.atelier/config.toml`) → built-in defaults. CLI flags still win at the top. The GUI and TUI both render the active model id + §2 strategy + probe outcome in the bottom-right of their footer, so a glance tells you which provider you're talking to.

### New surface

- **`crates/atelier-core/src/config.rs`** (NEW, ~600 lines):
  - `AtelierConfig` document with three optional sections: `[provider]` (`kind`, `model`, `base_url`), `[runner]` (`max_turns`), `[probe]` (`policy`). Every field is `Option<T>` so a one-line config (`[provider] kind = "anthropic"`) is valid and inherits defaults for the rest.
  - `ProviderKind` enum (`Mock` / `Anthropic` / `OpenaiCompat`, kebab-case on the wire) and `ProbePolicyName` enum (`Auto` / `Skip` / `Force`, lowercase on the wire). Both derive `as_str()` for log lines + the UI status line.
  - `AtelierConfig::load(repo_root)` walks the path list: `<repo>/.atelier/config.toml` first, then `~/.atelier/config.toml`. Missing both is `Ok(None)` (not an error); a file that exists but doesn't parse is fatal (`ConfigError::Parse` with the file path) so a typo can't silently shift the runtime to defaults.
  - `AtelierConfig::paths_searched(repo_root)` mirrors the search list for "no config found, searched …" logging.
  - Cross-section validation: `[provider].base_url` requires `[provider].kind = "openai-compat"`. `ConfigError::Invalid` carries the file path + a typed message.
  - `serde(deny_unknown_fields)` on every struct so a typo'd `[provider].mod_el = "..."` is a parse error, not a silent fall-through.
  - 14 unit tests cover shape (every field optional, kebab/lowercase wire forms, unknown-field rejection), discovery (project before user, missing-both yields None), validation (`base_url` requires `openai-compat`; `base_url` without `kind` is allowed because CLI may supply `kind` later), round-trip through serde.

- **`crates/atelier-cli/src/main.rs`** — `run_run` refactored into a top-down narrative: parse argv → resolve workspace → load TOML → layer CLI > TOML > defaults → build Runner → run. New `CliArgs` struct holds raw `Option<T>` flags; new helpers `resolve_provider_choice`, `resolve_provider_kind`, `resolve_probe_policy`, `read_prompt_from_cli`. The binary prints `atelier run: using config <path>` so users can confirm which file is active. Usage text expanded with a config example block.

- **`crates/atelier-gui/ui/src/lib/state.ts`** — new `CurrentModel` type + `applyEvent` arm for `ModelProfileLoaded` populating `state.currentModel`. `projectEvent` adds a `ModelProfile` event-log line.

- **`crates/atelier-gui/ui/src/App.svelte`** — footer extended with a right-aligned `.model-badge` (CSS `margin-left: auto` flex idiom) rendering `model_id · strategy · outcome` with cyan id, green strategy, dim outcome. Falls back to `no model` placeholder before the first event.

- **`crates/atelier-gui/src/lib.rs`** — `bridge_event` for `ModelProfileLoaded` now serialises `outcome` via `serde_json::to_value(ProbeLoadOutcome)` so the wire shape is `snake_case` (`cache_hit` / `probed` / `reprobed` / `not_cached`) directly usable in the UI. Pre-v52 used `format!("{:?}").to_lowercase()` which produced `cachehit`.

- **`crates/atelier-tui/src/lib.rs`** — new `CurrentModel` struct on `AppState`. `apply` populates it from `ModelProfileLoaded`. `render_help` split into `render_help_left` + `render_help_right_model` + `model_badge_width` so the layout split between scrub keys (left, flexible) and the model badge (right, fixed-width) is one ratatui `Layout::default().direction(Horizontal).constraints([Min(0), Length(badge_width)])`. The pending-approval banner suppresses the badge so the approval prompt is the unambiguous focus.

- **`crates/atelier-tui/src/lib.rs`** — new `snake_case_debug` helper inserts underscores at camel-case boundaries so the TUI's `outcome` label matches the GUI's `serde(rename_all = "snake_case")` projection byte-for-byte.

### Demo flow

```sh
# One-time: pin the local LLM defaults for this repo.
cat > .atelier/config.toml <<EOF
[provider]
kind     = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"
EOF

# Now every invocation only needs a prompt:
$ atelier run "add a hello() function"
atelier run: using config /Users/you/proj/.atelier/config.toml
…

# GUI footer (bottom-right):
#   local:qwen2.5-coder:7b · json_sentinel · cache_hit

# TUI footer (right of the help line):
#    q/Esc/Ctrl-C quit · [ prev · ] next · g HEAD     local:qwen2.5-coder:7b · json_sentinel · cache_hit
```

### CLI override layering (top wins)

```text
  1. CLI flags                         (per-invocation overrides)
  2. <repo>/.atelier/config.toml       (project scope)
  3. ~/.atelier/config.toml            (user scope)
  4. Built-in defaults                 (mock, 32 turns, auto probe)
```

### Verified

- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` → **atelier-core 486** (+14 from `config`) + **atelier-cli 14** + **atelier-gui 13** (+1 from the new `bridge_event` test) + **atelier-tui 52** (+6 from the model-badge tests) = **565 passing**.
- `make check` — schemas + 52 artifacts + 112 rig tests + 11 dry-runs all OK.
- `npm run check` in `crates/atelier-gui/ui/` — 94 files, 0 errors, 0 warnings.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 486 atelier-core unit tests + 14 atelier-cli integration tests + 13 atelier-gui unit tests + 52 atelier-tui unit tests** (atelier-core +14, atelier-gui +1, atelier-tui +6 from v51).

## v51 — 2026-05-17

**Probe-on-first-use model adaptation (§1).** Atelier now fires a short calibration round-trip the first time it encounters a new `(model_id, base_url)` pair, observes whether the model handles native tool calls and JSON-sentinel envelopes, picks the appropriate §2 strategy, and caches the result to `~/.atelier/model_profiles/<hash>.json` so subsequent runs skip the probe. The cached profile is emitted on the bus as a new `Event::ModelProfileLoaded` so the GUI and TUI can render the active strategy badge. The Anthropic and Mock adapters skip the probe (they're well-characterised); only `openai-compat` is probed by default. CLI flags `--no-probe` and `--force-probe` override.

### New surface

- **`crates/atelier-core/src/adapter/model_profile.rs`** (NEW, ~900 lines):
  - `ModelProfile` struct: schema-versioned on-disk shape with model_id, base_url, probed_at, strategy, supports_native_tools, supports_streaming, utf8_clean, context_window_tokens, max_tokens, notes. Atomic `save_to` / `load_from` mirror `persistence.rs` idioms (tempfile + persist + fsync_dir_best_effort); load rejects mismatched `PROFILE_SCHEMA_VERSION` with `ProfileError::IncompatibleVersion`.
  - `ProbeObservation` struct + `decide_strategy(&obs) -> Strategy` pure function. Preference order: `NativeTool > JsonSentinel > RegexProse`. Eight branch tests lock the decision rule.
  - `probe_model(adapter)` async driver: fires two calibration calls — (A) ask the model to invoke a `harness_calibration_echo` tool with `{"value": "ok"}` and check round-trip, (B) ask for an exact `<<<harness_meta>>>{"claimed_done":true}<<<end>>>` and parse with `parse_json_sentinel`. UTF-8 cleanliness (U+FFFD detection) recorded as a side signal. Fatal adapter errors (`Auth`, `NotConfigured`, `Unreachable`, `ContextOverflow`) propagate; transient errors (`Malformed`, `Provider`, `RateLimited`) record a note and the strategy flag stays `false`.
  - `ProfileStore` cache: `user_default()` honours `ATELIER_PROFILE_DIR` then `~/.atelier/model_profiles/`. `load_or_probe(adapter, base_url, force_reprobe, probed_at)` is the entry point — returns `(ModelProfile, ProbeLoadOutcome)` where the outcome distinguishes `CacheHit / Probed / Reprobed / NotCached`. Best-effort persistence: a save failure downgrades the outcome to `NotCached` but never fails the run. 34 unit tests cover save/load round-trip, version-mismatch rejection, cache hit doesn't call adapter, force-reprobe overwrites, stale-schema reprobes, ephemeral store, fatal probe error doesn't leave stale state on disk.
  - Cache key: `sha256(model_id || "\n" || base_url)[..16]` (64 bits) — stable, collision-resistant against the `("ab", "cd")` vs `("a", "bcd")` ambiguity (test `cache_path_does_not_collide_via_concat_ambiguity`).
- **`crates/atelier-core/src/session.rs`** — new `Event::ModelProfileLoaded { model_id, base_url, strategy, outcome }` variant. Emitted by the Runner once, after the probe step resolves, before the first turn. UI consumers render a "strategy badge" off it.
- **`crates/atelier-cli/src/runner.rs`** — new `ProbePolicy::{Auto, Skip, Force}` enum and `Runner::with_probe_policy` builder. `Runner::new` sets per-provider defaults: `Mock` and `Anthropic` → `Skip` (well-known); `OpenAiCompat` → `Auto` (cache-first, probe on miss). The Runner's `run()` resolves a `ModelProfile` before the turn loop and broadcasts `Event::ModelProfileLoaded`. A probe failure logs a warning and falls back to a stub profile so the run continues — the §1 conformance tracker still drives runtime strategy selection.
- **`crates/atelier-cli/src/main.rs`** — `--no-probe` and `--force-probe` CLI flags. Mutually exclusive (exit 2 on both). Usage text updated.
- **`crates/atelier-gui/src/lib.rs`** — `bridge_event` adds a `ModelProfileLoaded` projection so the webview can render the badge.
- **`crates/atelier-tui/src/lib.rs`** — `project_event` adds a `ModelProfile` event line; `apply` includes the variant in the no-op set (informational, doesn't change pane state).

### What the probe *does* and *doesn't* change in v51

- **Does:** populate a cached `ModelProfile` per `(model_id, base_url)`, broadcast it on the bus, log the cache-hit / probe outcome, and surface strategy guidance to UIs.
- **Doesn't yet:** rewire the adapter's initial strategy from the cached value. The adapter still picks its own strategy at construction time; the §1 conformance tracker degrades from there at runtime if the model misbehaves. Threading `profile.strategy` into the adapter as an initial-strategy hint is a v52 follow-on — the present commit lands the observation layer with all the cache + invariants in place, so v52 is a one-call wiring change.

### Demo flow

```text
$ cargo run -p atelier-cli -- run --provider openai-compat \
    --base-url http://localhost:11434/v1 --model local:qwen2.5-coder:7b \
    "add a hello function"

# First run — probe round-trips:
[INFO atelier::probe] model profile probed and cached
    model_id=local:qwen2.5-coder:7b base_url=http://localhost:11434/v1
    strategy=json_sentinel cache_path=~/.atelier/model_profiles/<hash>.json
    forced=false
# Bus emits: ModelProfileLoaded { strategy: JsonSentinel, outcome: Probed }

# Second run — cache hit:
[INFO atelier::probe] model profile cache hit
    strategy=json_sentinel
# Bus emits: ModelProfileLoaded { strategy: JsonSentinel, outcome: CacheHit }

# Force re-probe (e.g., after a model upgrade):
$ cargo run -p atelier-cli -- run --provider openai-compat \
    --base-url http://localhost:11434/v1 --model local:qwen2.5-coder:7b \
    --force-probe "..."
# Bus emits: ModelProfileLoaded { strategy: ?, outcome: Reprobed }
```

### Verified

- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` → **atelier-core 472** (was 438; +34 from `adapter::model_profile`) + **atelier-cli 14** + **atelier-gui 12** + **atelier-tui 46** = **544 passing**.
- `make check` — schemas + 52 artifacts + 112 rig tests + 11 dry-runs all OK.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 472 atelier-core unit tests + 14 atelier-cli integration tests + 12 atelier-gui unit tests + 46 atelier-tui unit tests** (atelier-core +34 from v50).

### §1 capability/conformance status
- **Adapter trait surface**: `chat`, `stream`, `count_tokens`, `capabilities`, `conformance` — all live since v38.
- **Conformance ring buffer + degradation** (§2): live since v15.
- **Capability matrix as machine-readable config**: deferred — the static-table approach (option 1 of the v51 design discussion) is a separate path that would land a `capabilities.toml` lookup before construction. Probe-on-first-use is the dynamic counterpart; both can coexist.
- **"Claimed-but-broken" column**: surfaced via `CapabilityClaim::ClaimedButBroken`; the probe doesn't write this yet — it records observations directly. A v52 cross-walk between `ProbeObservation` and `CapabilityClaim` is the natural next step.

## v50 — 2026-05-17

**OpenAI-compatible adapter lands + v49 LOW residuals closed.** Atelier now talks to any server speaking `POST /v1/chat/completions` — LM Studio, llama.cpp's `llama-server`, vLLM, sglang, Ollama (via its `/v1/` compat layer), and OpenAI itself. Pair with the existing Anthropic adapter and the `Mock` for tests, that's three of the four §1 BYOM providers in. Companion to the adapter: four v49 LOW residuals (LR-1..4) cleaned up from the rescan.

### v50 features

- **`crates/atelier-core/src/adapter/openai_compat.rs`** (NEW, ~870 lines). `OpenAiCompatAdapter` implements `Adapter` end-to-end:
  - `chat()` — non-streaming, single round-trip to `<base_url>/chat/completions`. Tool calls are surfaced through OpenAI's `tool_calls` array (each `function.arguments` is a JSON-encoded string on the wire, which the adapter parses back into `serde_json::Value` for `ToolCallRequest::arguments`). `finish_reason` mapped to `StopReason` (`stop`→`EndTurn`, `length`→`MaxTokens`, `tool_calls`→`ToolUse`, `content_filter`→`Refusal`).
  - `stream()` — SSE parser mirroring `anthropic.rs`'s line-buffered state machine: handles `\r\n`/`\n`/`\r`, UTF-8 decoded only on complete events, `[DONE]` terminator recognised, 8 MB buffer cap. Tool-call deltas keyed by `index` so fragmented JSON across multiple SSE frames re-assembles correctly; arguments parsed once at finish.
  - `count_tokens()` — chars/4 approximation tagged `TokenSource::Approx` (no server-side counter exists for the compat protocol; ContextManager treats this as fallback).
  - HTTP error mapping (`map_http_error`): 401→`Auth`, 429→`RateLimited` with `Retry-After` honored (clamped to `MIN_RATE_LIMIT_BACKOFF_MS=100`), 400 with `code: "context_length_exceeded"`→`ContextOverflow`, 5xx + other→`Provider`. Network/serde failures→`Network`/`Protocol` per the established taxonomy.
  - `to_openai_messages()` mapping: `System`/`User` inline; `Assistant` carries `tool_calls` as an array with `function.arguments` re-encoded as JSON strings; `Tool` role with required `tool_call_id`. Round-trips through the wire format.
  - Constants: `DEFAULT_BASE_URL=https://api.openai.com/v1`, `API_KEY_ENV=OPENAI_API_KEY`, `BASE_URL_ENV=OPENAI_BASE_URL`, `DEFAULT_MAX_TOKENS=4096`, `DEFAULT_CONTEXT_WINDOW_TOKENS=8192` (overridable via `with_context_window`).
  - **19 wiremock tests** covering: happy path, no-auth (empty key), tool calls, 401, 429 with Retry-After, 429 zero floor, context overflow, 500, malformed body, length finish reason, tools shape, assistant tool_calls round-trip, streaming text, streaming tool args, capabilities, context window override, token count, model-name parsing, `Debug` redaction.
- **`crates/atelier-core/src/adapter/mod.rs`** — `pub mod openai_compat;` next to `pub mod anthropic;`.
- **`crates/atelier-cli/src/runner.rs`** — new `ProviderChoice::OpenAiCompat { model_id, base_url: Option<String> }` variant. `Runner::new` reads `OPENAI_API_KEY` (empty string allowed — most local servers don't require auth; a 401 from a server that *does* require it surfaces as `AdapterError::Auth` on first call). `None` `base_url` falls back to `OPENAI_BASE_URL`, then to the adapter's `DEFAULT_BASE_URL`.
- **`crates/atelier-cli/src/main.rs`** — new `--base-url <URL>` flag and `openai-compat` provider arm. Usage text expanded with concrete defaults for the common local servers (LM Studio :1234, llama-server :8080, Ollama :11434). `--model` is now required for `openai-compat`; `--base-url` is rejected for any other provider with a clear error.

### Demo flow

```text
# Local-LLM dev loop (LM Studio with `qwen2.5-coder:7b` loaded):
$ cargo run -p atelier-cli -- run \
    --provider openai-compat \
    --base-url http://localhost:1234/v1 \
    --model local:qwen2.5-coder:7b \
    "add a hello() function to src/main.rs"

# Ollama via its OpenAI-compat surface:
$ cargo run -p atelier-cli -- run \
    --provider openai-compat \
    --base-url http://localhost:11434/v1 \
    --model local:llama3:8b \
    "fix the failing test in tests/parser_test.rs"

# OpenAI itself (omit --base-url; export OPENAI_API_KEY):
$ OPENAI_API_KEY=sk-... cargo run -p atelier-cli -- run \
    --provider openai-compat \
    --model openai:gpt-4o-mini \
    "..."
```

### v49 LOW residuals closed

- **LR-1** — `crates/atelier-core/src/session.rs`, `crates/atelier-cli/src/lib.rs`, `crates/atelier-gui/src/lib.rs`, `crates/atelier-gui/ui/src/App.svelte`. Doc-only: `CommitDecision` docstring updated to reflect the v49 emission order (per-file `EditStaged` → `LedgerAppended` → `CommitDecision`), `ApprovalPolicy` re-exported from `atelier_cli` for consumers, `remove_dir_all` symlink-safety comment, prompt-too-long error clarifies bytes vs chars, App.svelte `state`→`app` rename inline-documented.
- **LR-2** — `crates/atelier-tui/src/lib.rs`. `MAX_PROMPT_BYTES = 64 KiB` cap on `spawn_driver_run`'s prompt arg, parity with the GUI's v49 boundary check. Oversized prompts return `io::Error::new(InvalidInput, ...)` before any allocation grows. `event_stream_ended` one-shot semantics now documented inline.
- **LR-3** — `crates/atelier-core/src/dispatcher.rs`. Extended `session_dispatcher_broadcasts_edit_staged_for_writes` to assert `CommitDecision` arrives *after* `LedgerAppended` and that under `AutoApproveAll` the decision's `committed` set lists every changed file with `dropped` empty. Locks the v49 ordering fix against regression.
- **LR-4** — Deferred (low-value, deeper refactor — atelier-tui's `_run_task: Option<JoinHandle>` would need a `Drop` to abort the spawned task; revisit when the TUI driver mode grows a quit-while-running scenario beyond the current end-of-run cleanup).

### Verified

- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` → **atelier-core 438** (was 419; +19 openai_compat tests) + **atelier-cli 14** + **atelier-gui 12** + **atelier-tui 46**. All green.
- `make check` — schemas + 52 artifacts + 112 rig tests + 11 dry-runs all OK.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 438 atelier-core unit tests + 14 atelier-cli integration tests + 12 atelier-gui unit tests + 46 atelier-tui unit tests** (atelier-core +19 from v49).

### Phase-1 BYOM status
- **Mock** (in-tree, `MockAdapter`) — v0
- **Anthropic** Messages API — v45
- **OpenAI-compatible** (LM Studio, llama-server, vLLM, sglang, Ollama-compat, OpenAI) — **v50**
- **Bedrock / Vertex** — Phase E/F

## v49 — 2026-05-17

**Audit follow-up: ten v48 deep-scan findings fixed.** No new features — all hardening / correctness against the cross-cutting concerns the v48 scan surfaced. Highest-impact items: event-ordering inversion, missing Runner cleanup on error paths, no concurrent-run guard in the GUI, prototype-pollution surface in DiffPane's accept toggle, mount-race losing the first run's events.

- **`crates/atelier-core/src/dispatcher.rs`** — FIX-1. `ApprovalGate::notify_outcome` removed; the dispatcher's commit branch now builds an `ApprovalSummary { commit_id, committed, dropped }` and stores it on `DispatchOutcome.approval_summary`. `SessionDispatcher::dispatch` emits the bus events in the canonical user-visible-first order: per-file `EditStaged` → `LedgerAppended` → `CommitDecision`. Closes the v48 audit's "documented intent inverted" finding.
- **`crates/atelier-cli/src/runner.rs`** — FIX-2. New `DispatcherHandleGuard` private struct with a `Drop` impl that runs on every exit path from `Runner::run` (success, `?`-propagated errors, panic). Pre-v49 the `handle.clear()` was a tail call only the success path reached — an error mid-loop would leave a stale Arc pointing at a torn-down dispatcher.
- **`crates/atelier-gui/src/lib.rs`** — FIX-3 + FIX-5 + FIX-10. `SessionState.run_in_flight: Arc<AtomicBool>` guards against concurrent `start_demo_run` calls (compare_exchange Acquire/Relaxed; rejected calls return a typed error the frontend surfaces). `MAX_PROMPT_BYTES = 64 KiB` cap on the Tauri command's `prompt` argument so a multi-GB string can't OOM the process before any rejection. Each `start_demo_run` now creates a fresh UUID-named subdirectory under `workspace_root`; a new `RunCleanup` Drop guard on the spawned task clears the run-in-flight flag *and* (best-effort) removes the per-run workspace on every exit path — solving both "v47 demo clobbered by v48 demo" and "workspace leak across launches."
- **`crates/atelier-cli/src/lib.rs`** + **runner.rs** — FIX-4. Documented that `pub mod runner;` is a deliberate test affordance, not a supported API surface, and re-export the blessed types (`Runner`, `ProviderChoice`, `MockResponse`, `EventSink`, `RunError`, `RunReport`, `DispatcherHandle`) at the crate root. Verified the `runner` module's internal helpers (`extract_native_envelope`, `built_in_registry`, `now_rfc3339`, `days_to_ymd`, `registry_to_tool_specs`, `build_mock_adapter`, `spawn_sink_drain`, `adapter_to_run_error`) are all module-private `fn`, not `pub` — they were never actually reachable as `atelier_cli::runner::*`. The audit's HIGH finding was over-stated; the only real leak was `read_prompt` (binary-internal but `pub` because the bin crate is separate from the lib crate), now documented.
- **`crates/atelier-tui/src/lib.rs`** — FIX-6 + FIX-8. New `event_stream_ended: bool` flag gates the `recv` arm of the run loop's `tokio::select!` via the `, if !event_stream_ended` guard — closes the v48 busy-loop where the post-RunEnded `never_rx` re-fired `None` on every poll, appending "RunEnded" lines forever. `render_pending_diff` banner replaced the v46-era developer text ("submit via `SessionDispatcher::submit_approval(commit_id, accepted)`") with a coloured user-facing line: "press **y** to accept all · **n** to reject all" — matching the keys the v48 handler already binds.
- **`crates/atelier-gui/ui/src/lib/components/DiffPane.svelte`** — FIX-7. `acceptedPaths` switched from a literal `Record<string, boolean>` (vulnerable to prototype pollution when paths like `__proto__` or `constructor` are used as keys) to `Object.create(null)` — a null-prototype object that can't reach `Object.prototype`. `togglePath` does a copy-on-write update so Svelte's reactivity proxy still sees the assignment. Also added `submitError` state — when `submit_approval` returns false (stale commit_id), the user now sees an inline red error instead of a silent `console.warn`. The Tauri command's return value is now consumed (previously discarded).
- **`crates/atelier-gui/ui/src/App.svelte`** — FIX-9. New `listenerReady: boolean` state; `composerBusy` derived from `!listenerReady || runBusy` so the Composer's Send button is disabled until `await listen('atelier://event')` resolves. Pre-v49 a fast user could click Send before mount finished and lose the first run's events. Local state var renamed `state` → `app` to dodge a TypeScript-mode quirk in svelte-check that was treating `let state = $state(...)` as the Svelte-3-era store-auto-subscribe syntax.

Verified: `cargo test --workspace` → **atelier-core 419 + atelier-cli 14 + atelier-gui 12 + atelier-tui 46** (unchanged test counts — these are correctness fixes, not new tests; the existing tests still pass through the refactor); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `npm run check` → 94 files, 0 errors, 0 warnings; `npm run build` → 62.6 kB JS / 22.8 kB gzip; `make check` green.

### Findings still deferred (per v48 audit)

These are documented in the audit but deferred — they're lower-impact or require deeper refactors:

- `dispatcher.rs:613` — `rx.await.unwrap_or_default()` collapses "user explicitly rejected" with "consumer dropped oneshot" into the same empty-Vec result.
- `session.rs:192-199` — `PendingFile` drops `SyntaxOutcome`; UI can't show grammar-missing/not-applicable badges.
- `state.rs` — `AwaitingApproval` transitions defined but never emitted (matters when §4 checkpoint replay lands).
- `atelier-cli/tests/run_integration.rs` — `#[path]` test still compiles a second copy of runner.rs (low-impact; would require migrating tests to use the lib).
- `atelier-tui` — `_run_task: Option<JoinHandle>` doesn't abort the task on Drop (runner keeps executing in background after user quits).
- Hand-rolled `now_rfc3339` instead of `chrono`/`time` dep.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 419 atelier-core unit tests + 14 atelier-cli integration tests + 12 atelier-gui unit tests + 46 atelier-tui unit tests** (unchanged from v48).

## v48 — 2026-05-17

**TUI driver mode lands.** Same v47 pattern, terminal edition: pass a prompt as `argv[1]` and the TUI builds a `Runner` with `AwaitApproval` policy, drives a scripted MockAdapter run, pops the pending-diff banner with the `(PENDING)` title, waits for `y`/`n`, routes the decision to the live `SessionDispatcher::submit_approval`. `cargo run -p atelier-tui -- "rename foo"` is now a working end-to-end demo of the spec §3 hunk accept/reject contract from a terminal.

- **`crates/atelier-tui/Cargo.toml`** — TD-A. Added `atelier-cli` + `serde_json` workspace deps (same hop the GUI takes in v47).
- **`crates/atelier-tui/src/lib.rs`** — TD-B + TD-C. Two new `InputOutcome` variants: `AcceptAll`, `RejectAll`. `handle_key` signature changed to `handle_key(key, pending: Option<&PendingApproval>)`; `y`/`n` only return their accept/reject outcomes when `pending` is `Some`, otherwise they fall through to `Continue` (keeps the keys safe for a future text-input mode). The run loop gained two modes:
  - **Driver mode** (when `argv[1]` is a non-empty prompt): builds a Runner with `AwaitApproval` + `DispatcherHandle`, `EventSink::Callback` feeds an mpsc that the select-loop drains. `y` accepts every pending file via `submit_approval(commit_id, all_paths)`; `n` rejects with an empty accept-set.
  - **Viewer mode** (no prompt arg): preserved v45 behaviour — spawns a NoopHook session, forwards its broadcast onto the same mpsc. Useful for testing the terminal lifecycle in isolation.
  - New helpers: `spawn_driver_run`, `submit_pending`, `first_word_or_default` (mirror of the GUI's helper of the same name; same sanitisation rules).
- **`crates/atelier-tui/src/lib.rs`** (render path) — `render_help` now pivots to a yellow bold `APPROVAL REQUIRED · y accept all · n reject all · q quit` line when `pending_approval` is set, returning to the scrub-keys footer once `CommitDecision` clears the pending state.
- **5 new tests** (`handle_key_emits_accept_all_on_y_when_pending`, `..._reject_all_on_n_when_pending`, `..._y_and_n_are_inert_when_no_pending`, `help_footer_swaps_to_approval_hints_when_pending`, `help_footer_returns_to_scrub_hints_after_decision`) lock the y/n contract + footer pivot. Existing handle_key tests updated to pass the new `pending` argument (always `None` for non-approval cases).

Verified: `cargo test --workspace` → **atelier-core 419 + atelier-cli 14 + atelier-gui 12 + atelier-tui 46** (was 419 / 14 / 12 / 41 in v47; +5 TUI tests for the approval keys + footer pivot); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` green.

### Demo flow

```text
$ cargo run -p atelier-tui -- "rename my-script"

  ratatui terminal opens
  ↓ Runner spawns, scripts a write_file → my-script.txt
  ↓ dispatcher hits AwaitApproval
  ↓ TUI DiffPane shows yellow (PENDING) box with my-script.txt
  ↓ footer pivots to "APPROVAL REQUIRED · y accept all · n reject all · q quit"

  user presses y
  ↓ submit_pending() calls SessionDispatcher::submit_approval(commit_id, [my-script.txt])
  ↓ dispatcher resumes, runs commit_selected
  ↓ EditStaged + CommitDecision land on the bus
  ↓ pending banner clears
  ↓ footer returns to "q quit · [ prev · ] next · g HEAD"

  on disk: /tmp/atelier-tui-<pid>-<nanos>/my-script.txt now contains
  the demo write
```

### Phase C status — both UIs are now drivers

| Surface | v45 | v46 | v47 | v48 |
|---|---|---|---|---|
| TUI rendering | ✓ multi-pane | ✓ pending state | ✓ pending state | ✓ |
| TUI driver | — | — | — | ✓ (v48) |
| GUI rendering | ✓ multi-pane | ✓ pending state | ✓ pending state | ✓ |
| GUI driver | — | — | ✓ (v47) | ✓ |
| Hunk accept/reject contract | — | ✓ (file-level) | ✓ | ✓ |

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 419 atelier-core unit tests + 14 atelier-cli integration tests + 12 atelier-gui unit tests + 46 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 419 / 14 / 12 / 41).

## v47 — 2026-05-17

**GUI becomes a driver — hunk accept/reject works end-to-end through the webview.** The Svelte DiffPane's accept/reject buttons now route to a live `SessionDispatcher::submit_approval`, not a logging stub. The GUI builds + drives its own scripted run with `AwaitApproval` policy; the user types a prompt in the new Composer, sees the staging banner appear, clicks accept (or rejects per-file), and watches the committed write land in the workspace.

- **`crates/atelier-cli/Cargo.toml`** — DR-A. Hybrid lib+bin. New `[lib] name = "atelier_cli"` so the runner is reachable from other crates (atelier-gui in particular). Binary `[[bin]] atelier` unchanged.
- **`crates/atelier-cli/src/lib.rs`** — DR-A. New module that re-exports the runner's public surface (`Runner`, `ProviderChoice`, `MockResponse`, `EventSink`, `RunError`, `RunReport`).
- **`crates/atelier-cli/src/main.rs`** — switched from `mod runner;` to `use atelier_cli::runner;` so the binary and the library share one source file.
- **`crates/atelier-cli/src/runner.rs`** — DR-B. New `DispatcherHandle` (a shared `Arc<parking_lot::Mutex<Option<Arc<SessionDispatcher>>>>`) that the runner populates as soon as the dispatcher is built and clears on shutdown. New builder methods `Runner::with_approval_policy(ApprovalPolicy)` and `Runner::with_dispatcher_handle(DispatcherHandle)`. The dispatcher is now wrapped in `Arc` so the handle hand-off is cheap. New `EventSink::Callback(Arc<dyn Fn(&Event) + Send + Sync>)` variant — the drain task invokes the callback per event. The GUI uses it to forward bus events into the Tauri webview without standing up an external broadcast subscription.
- **`crates/atelier-gui/Cargo.toml`** — DR-C. Added `atelier-cli` and `parking_lot` workspace deps.
- **`crates/atelier-gui/src/lib.rs`** — DR-C + DR-D. `SessionState` redesigned: drops the pre-spawned session, holds a `DispatcherHandle` + an ephemeral `workspace_root` per process. `submit_approval` Tauri command now reads the dispatcher from the handle and calls `SessionDispatcher::submit_approval(commit_id, accepted)` for real. New `start_demo_run(prompt)` Tauri command — builds a `Runner` with `MockAdapter` scripted to emit a `write_file` + `harness_meta` envelope, installs `AwaitApproval` policy + the `DispatcherHandle`, wires `EventSink::Callback` to forward bus events to the webview as `atelier://event`, spawns the run loop on `tauri::async_runtime`. The file name is derived from the prompt's first word so the user sees their input reflected on disk.
- **`crates/atelier-gui/ui/src/lib/components/Composer.svelte`** — DR-E. New textarea + Send button at the bottom of the workspace. Cmd/Ctrl+Enter submits. Disabled while a run is in flight (`busy` derived from `state.currentState`). Errors from the Tauri command surface inline.
- **`crates/atelier-gui/ui/src/App.svelte`** — wires `Composer` into the layout grid (header / panes / composer / footer). `runBusy` derived from `currentState` so Composer disables itself during the run.
- **`crates/atelier-cli/tests/run_integration.rs`** — DR-F. Two new tests (`await_approval_via_runner_with_dispatcher_handle_round_trips` and `..._full_reject_drops_the_write`) prove the Runner-side contract exactly matches what the GUI's `start_demo_run` builds: spawn a run with AwaitApproval + DispatcherHandle, watch the captured events for `StagingPendingApproval`, call `dispatcher.submit_approval` (accept-all or full-reject), verify the run terminates in `Done` and the file does/doesn't land on disk. Also asserts `DispatcherHandle.get()` returns `None` after the run shuts down (clean-up contract).

Verified: `cargo test --workspace` → **atelier-core 419 + atelier-cli 14 + atelier-gui 12 + atelier-tui 41** (was 419 / 12 / 12 / 41 in v46; +2 cli integration tests for the GUI-shaped driver path); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `npm run check` → 94 files, 0 errors, 0 warnings; `npm run build` → 62.3 kB JS bundle (22.7 kB gzip); `make check` green.

### What still isn't wired

- **Real-provider runs**: `start_demo_run` is scripted (MockAdapter). Routing live `--provider anthropic` runs from the GUI needs API-key input + provider selector + the lifecycle of multi-turn flows; v47 stops at "the demo proves the end-to-end approval contract end-to-end."
- **Per-hunk granularity**: still file-level. Sub-file accept/reject requires reworking `Staging::commit_selected` to accept `Vec<(PathBuf, HunkSet)>`.
- **TUI driver mode**: TUI is still bootstrap + render. Wiring it as a driver follows the same `DispatcherHandle` pattern; the API is now ready.
- **State-machine `AwaitingApproval` transition**: still not emitted by the runner. The dispatcher blocks correctly on its oneshot but the `State` enum doesn't move through `AwaitingApproval` during the wait. Cosmetic for now; matters when checkpoints/replay land in §4.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 419 atelier-core unit tests + 14 atelier-cli integration tests + 12 atelier-gui unit tests + 41 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 419 / 12 / 12 / 41).

## v46 — 2026-05-17

**§3 hunk accept/reject lands at the contract level.** The dispatcher now gates commit on user approval when configured to do so. The flow: tool stages → dispatcher emits `StagingPendingApproval` → consumer (TUI/GUI) shows pending diff with accept/reject controls → consumer calls `SessionDispatcher::submit_approval(commit_id, accepted)` → dispatcher resumes, calls `StagedBatch::commit_selected(accepted)`, emits `CommitDecision` then `EditStaged` for each committed file. The pure Rust contract is end-to-end tested; the GUI's `submit_approval` Tauri command logs the intent today (the GUI doesn't yet drive its own dispatcher — that wiring lands when the GUI grows from viewer into driver).

- **`crates/atelier-core/src/staging.rs`** — HR-A. `Staging::commit()` split into `Staging::stage() -> StagedBatch` + `StagedBatch::commit_selected(accepted) -> CommitReport` + `StagedBatch::commit_all()`. Existing `Staging::commit()` preserved as `stage().commit_all()` for callers that don't want approval gating. `StagedBatch` owns the `TempDir`; dropping it without committing discards the temp tree (same all-or-nothing semantic as v45). Not `Clone` (duplicating the handle would race for the same staged paths). 7 new tests: stage-no-rename, commit_all parity, commit_selected partial-accept, empty-accept full-reject, idempotent stale-path ignore, drop-without-commit cleanup, commit() === stage().commit_all().
- **`crates/atelier-core/src/dispatcher.rs`** — HR-B + HR-D. `ToolResult.staged_writes: Option<CommitReport>` → `Option<StagedBatch>`; `ToolResult` dropped `Clone` derive (no caller used it). New `ApprovalGate` async trait + default `AutoApprove` impl (commits all) + `PendingApprovalGate` impl on the SessionDispatcher (emits `StagingPendingApproval`, waits on oneshot). New `ApprovalPolicy { AutoApproveAll (default), AwaitApproval }`. `Dispatcher::with_approval_gate` + `SessionDispatcher::with_approval_policy` builder methods. New `SessionDispatcher::submit_approval(commit_id, accepted) -> bool` (returns `false` when commit_id is unknown). The dispatcher's commit step now: stage → gate.approve(commit_id, pending) → commit_selected(accepted) → gate.notify_outcome(committed, dropped) → events. Commit failures fold into `ToolError::ExecutionFailed`. 3 new tests: pending-event + selective accept, full-reject drops everything, submit_approval for unknown commit_id returns false. EchoTool test fixture rewritten to build a real `StagedBatch` against a tempdir workspace (was a synthetic CommitReport).
- **`crates/atelier-core/src/state.rs`** — HR-C. New `State::AwaitingApproval` variant. New transitions: `ToolExecuting → AwaitingApproval`, `AwaitingApproval → ToolExecuting`, `AwaitingApproval → Failed`.
- **`crates/atelier-core/src/session.rs`** — HR-C. New `Event::StagingPendingApproval { commit_id: Uuid, files: Vec<PendingFile> }` (PendingFile carries path + hunks) and `Event::CommitDecision { commit_id, committed: Vec<PathBuf>, dropped: Vec<PathBuf> }`. Approval routing is deliberately NOT a session::Command — the actor's job is "validate transitions, fire hooks"; the approval lifecycle lives next to the staging it controls. Documented in-place.
- **`crates/atelier-core/src/tools/write_file.rs`, `tools/edit_file.rs`** — HR-B. Tools call `Staging::stage()` instead of `.commit()` and return `StagedBatch` in `staged_writes`. The dispatcher's auto-approve path produces identical end-state behaviour. Existing tool unit tests updated to call `commit_all()` themselves to verify on-disk results (they're testing the tool, not the dispatcher).
- **`crates/atelier-tui/src/lib.rs`** — HR-E. `AppState.pending_approval: Option<PendingApproval>` + `PendingApprovalFile` types. `apply()` folds `StagingPendingApproval` → set pending, `CommitDecision` → clear pending. `render_diff` defers to new `render_pending_diff` when pending is set — yellow `(PENDING)` title + banner + per-file path list. New `hunks_kind_label` / `short_uuid` helpers. `project_event` covers the two new variants. 4 new tests: apply records pending, decision clears pending, render shows badge + path, render returns to normal after decision. Total TUI tests: 41 (was 37).
- **`crates/atelier-tui/Cargo.toml`** — `uuid` workspace dep added (for `PendingApproval.commit_id`).
- **`crates/atelier-gui/src/lib.rs`** — HR-F. `bridge_event` covers `StagingPendingApproval` + `CommitDecision`. New Tauri command `submit_approval(commit_id, accepted) -> bool` — currently a logging stub; real routing waits on the GUI shell becoming a driver. 2 new bridge tests.
- **`crates/atelier-gui/Cargo.toml`** — `uuid` workspace dep added.
- **`crates/atelier-gui/ui/src/lib/state.ts`** — HR-F. `PendingApprovalFile` + `PendingApproval` types; `AppState.pendingApproval: PendingApproval | null`. `applyEvent` handles both new variants (mirror of TUI `apply()`). `projectEvent` covers both for the event log.
- **`crates/atelier-gui/ui/src/lib/components/DiffPane.svelte`** — HR-F. New `pendingApproval` prop. When non-null, renders an APPROVAL banner with commit-id, per-file checkboxes, "accept selected" / "reject all" buttons. Buttons invoke the `submit_approval` Tauri command. Yellow border + bold `PENDING` title visually distinguish from the committed-diff path. Per-file accept-toggle state resets when a new pending arrives (UX: "review and reject what you don't want", not "opt in to every file").
- **`crates/atelier-gui/ui/src/App.svelte`** — threads `pendingApproval` from app state into `DiffPane`.

Verified: `cargo test --workspace` → **atelier-core 419 + atelier-cli 12 + atelier-gui 12 + atelier-tui 41** (was 409 / 12 / 10 / 37 in v45; +16 new tests across HR-A through HR-F); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `npm run check` → 0 errors, 0 warnings; `npm run build` → 59.8 kB JS bundle (21.8 kB gzip); `make check` green.

The `submit_approval` Tauri command in `atelier-gui/src/lib.rs` is a logging stub. The GUI shell today is a viewer of events from a session running elsewhere (the production driver is `atelier run` in atelier-cli). Routing the approval back to a live `SessionDispatcher::submit_approval` requires the GUI to drive its own session — a separate piece of work that builds on this contract. Until then, the bus + state-machine + dispatcher round-trip is exercised end-to-end via `await_approval_emits_pending_event_and_blocks_until_submit` in `dispatcher::tests` (drives the full round-trip via direct `submit_approval` calls).

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 419 atelier-core unit tests + 12 atelier-cli integration tests + 12 atelier-gui unit tests + 41 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 409 / 12 / 10 / 37).

## v45 — 2026-05-17

**§3 GUI multi-pane workspace lands.** Mirrors the v43/v44 TUI subset in the Tauri webview. Same data contract (the `atelier://event` bus), same panes (conversation / plan / diff / cost+context meters), same scrubber keys. With v44's producer-side wiring already on the bus, `cargo tauri dev` now renders a live four-pane workspace fed by a real session.

- **`crates/atelier-gui/ui/src/lib/state.ts`** — pure-TS state module mirroring the TUI's `AppState`. Same field shapes, same caps (`MAX_CONVERSATION_LINES = 1000`, `MAX_DIFF_HISTORY = 16`, `MAX_EVENT_LOG = 1000`, `DEFAULT_CONTEXT_WINDOW_TOKENS = 200000`), same `applyEvent` reducer logic as the Rust `AppState::apply`. Types: `BridgedEvent`, `ConversationRole`, `ConversationLine`, `Hunks`, `Hunk`, `LineRange`, `StagedEdit`, `PlanStatus`, `PlanStep`, `LedgerEntry`, `AppState`. Functions: `initialState()`, `applyEvent(state, event)`, `applyScrub(state, cmd)`, `projectEvent(event)`, `roleColour(role)`. Pure — no DOM, no Svelte runes; components wrap in `$state` themselves. Mirroring keeps the contract parallel for the day a vitest harness lands.
- **`crates/atelier-gui/ui/src/app.css`** — global theme tokens. Palette mirrors the TUI's ratatui colours (user=yellow, assistant=cyan, tool=magenta, system=grey; diff add=green, remove=red, hunk-header=blue) so users switching between surfaces see the same visual contract. Plain CSS variables; per-component styles reference `var(--*)`.
- **`crates/atelier-gui/ui/src/lib/components/Header.svelte`** — app brand + meta strip: `state=<label>`, `EditStaged=N`, `scrub=HEAD|-N`. Yellow when pinned, green when at HEAD — same colours as the TUI header.
- **`crates/atelier-gui/ui/src/lib/components/ConversationPane.svelte`** — role-prefixed list, auto-scrolls to bottom on new messages via `$effect` watching `conversation.length`. Each line is a 2-column grid: role label (right-aligned, role-coloured) + text (`white-space: pre-wrap`, breaks long words).
- **`crates/atelier-gui/ui/src/lib/components/DiffPane.svelte`** — renders the head of `recentEdits` with full `Hunks` variant coverage: `Lines` produces per-hunk `@@ -old,len +new,len @@` headers + `-`/`+` lines; `Created` / `Deleted` / `Binary` / `Same` show coloured badges. Uses a Svelte 5 `{#snippet}` for the hunk block so the markup stays factored.
- **`crates/atelier-gui/ui/src/lib/components/PlanPane.svelte`** — step glyphs (`[ ]` / `[▸]` / `[✓]` / `[~]`) coloured by status, constraints indented under each step, terminal-status steps render strike-through with muted text.
- **`crates/atelier-gui/ui/src/lib/components/MetersPane.svelte`** — cost as `$0.XXXX` (yellow, no upper bound); context as a custom progress bar with `known/window` label and an explicit `+N unknown` suffix when `unknown > 0` so a silently-underreporting meter is visible (spec §5 contract). ARIA `role="progressbar"` for accessibility.
- **`crates/atelier-gui/ui/src/App.svelte`** — composes the four panes plus header + footer. CSS grid: header / `(conversation 60% | plan 40%)` / `(diff 60% | meters 40%)` / footer. Subscribes to `atelier://event` once, runs every payload through `applyEvent`, passes typed slices to each child. Owns the keyboard listener: `[` / `]` / `g` route through `applyScrub` for parity with the TUI scrubber.
- **`crates/atelier-gui/src/lib.rs`** — unchanged from v44; the bridge already projects all four new variants.

Verified: `npm run check` → 92 files, 0 errors, 0 warnings; `npm run build` → 56.5 kB JS bundle (20.7 kB gzip), 7.6 kB CSS. `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` → atelier-core 409 + atelier-cli 12 + atelier-gui 10 + atelier-tui 37 (unchanged from v44 — no new Rust); `make check` green.

The webview is not exercised in CI (no PTY-equivalent for Tauri), but the contract is pinned at three levels: (1) `bridge_event` unit tests in atelier-gui Rust assert the JSON shape every variant produces; (2) the pure-TS `state.ts` reducer is structurally identical to the TUI's Rust `apply()` — same caps, same fold semantics, same fallbacks; (3) `svelte-check` catches typos against `BridgedEvent` payload shapes.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 409 atelier-core unit tests + 12 atelier-cli integration tests + 10 atelier-gui unit tests + 37 atelier-tui unit tests** (Rust totals unchanged from v44; +1 frontend bundle).

## v44 — 2026-05-17

**Producer side of the §3/§5 broadcast bus wired.** Four new `Event` variants on the bus, emitted by the dispatcher + turn driver, consumed by both UIs. The v43 TUI multi-pane widgets already rendered conversation / plan / cost / context from `AppState` fields; pre-v44 nothing populated those fields in a real run. Now: `cargo run -p atelier-cli run --provider mock "..."` drives a live conversation pane, plan canvas, cost meter, and context meter end-to-end. Closes the producer-side gap the v43 TUI subset deferred.

- **`crates/atelier-core/src/session.rs`** — `Event` extended with `MessageCommitted { role, text }`, `PlanSnapshot { steps }`, `LedgerAppended { entry }`, `ContextSnapshot { known_tokens, unknown_tokens }`. New `MessageRole { System, User, Assistant, Tool }` enum (duplicated from `adapter::Role` to keep `session` free of an `adapter` dep). Snapshot-shaped events (not deltas) so a late-joining subscriber converges on the next event without replay.
- **`crates/atelier-core/src/dispatcher.rs`** — `SessionDispatcher::dispatch` now broadcasts `LedgerAppended` after every ledger append. Ordering matters: `EditStaged` (user-visible side effects) ships BEFORE `LedgerAppended` (bookkeeping) so a UI consumer rendering both a diff pane and a cost meter sees the diff arrive first. Failed tool calls still emit `LedgerAppended` (cost meter must count the failed call against the trust budget — spec §1 doesn't carve out a "free failure" path); `EditStaged` is not emitted in that case (no staged writes).
- **`crates/atelier-cli/src/runner.rs`** — turn driver now broadcasts: `MessageCommitted::User` for the initial prompt, `MessageCommitted::Assistant` after each model turn, `MessageCommitted::Tool` after each tool result. Maintains a `PlanCanvas` across turns, applies `envelope.plan_update` on each turn, and emits `PlanSnapshot` per turn. Emits `ContextSnapshot { known_tokens, unknown_tokens: 0 }` at end-of-turn via `adapter.count_tokens(&messages)` (the runner doesn't yet wire a full §5 ContextManager; once it does, `unknown_tokens` will reflect the `TokenSource::Unavailable` items).
- **`crates/atelier-tui/src/lib.rs`** — `AppState::apply` extended to consume the four new variants: `MessageCommitted` → `push_conversation`; `PlanSnapshot` → rebuild `PlanCanvas` from the snapshot vec; `LedgerAppended` → fold per-entry cost into `total_cost_usd` (CacheBust entries carry no cost field and are skipped, not zeroed); `ContextSnapshot` → update `context_tokens`. New `ConversationRole::from_message_role` exhaustive mapping so adding a `MessageRole` variant later forces a deliberate decision. `ledger_entry_cost` helper centralises the per-variant cost extraction. `project_event` extended for the new variants in the event log.
- **`crates/atelier-gui/src/lib.rs`** — `bridge_event` projects the four new variants onto the webview JSON shape: `MessageCommitted` → `{ role, text }`; `PlanSnapshot` → `{ steps }`; `LedgerAppended` → `{ entry }`; `ContextSnapshot` → `{ known_tokens, unknown_tokens }`. The frontend `App.svelte` will consume these in the next iteration.
- **Integration test `run_broadcasts_message_plan_ledger_and_context_events`** — drives a scripted single-turn run with a `write_file` tool call + the `harness_meta` envelope, captures the bus via `EventSink::Capture`, asserts at least 3 `MessageCommitted` (user/assistant/tool), at least 1 `LedgerAppended`, at least 1 `ContextSnapshot`. Pins the producer contract end-to-end.

Verified: `cargo test --workspace` → **atelier-core 409 + atelier-cli 12 + atelier-gui 10 + atelier-tui 37** (was 409 / 11 / 6 / 31 in v43; +11 new tests: +1 atelier-cli integration, +4 atelier-gui bridge, +6 atelier-tui apply/project, +1 atelier-core dispatcher reordering); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` green.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 409 atelier-core unit tests + 12 atelier-cli integration tests + 10 atelier-gui unit tests + 37 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 409 / 11 / 6 / 31).

## v43 — 2026-05-17

**v25.3 residuals pass + §3 TUI subset multi-pane widgets.** Four remaining residuals from the v25.2 deferred list closed; the TUI shifts from bootstrap-only ("EditStaged counter + event log") to a real four-pane layout matching the §3 TUI subset spec (conversation / plan / diff / cost+context meters) with scrubber-key plumbing. Phase C "§3 TUI subset" mechanical gate is wired at the rendering level — the only missing piece is the producer side (the §2.5 actor doesn't yet broadcast conversation commits / plan applies / ledger ticks; the TUI's `set_conversation` / `set_plan` / `set_cost_usd` / `set_context_tokens` mutators are the seam the producer side will plug into).

**Residuals fixed (v25.3-A through D):**

- **`crates/atelier-core/src/subprocess.rs`** — reader-task awaits now bounded by `tokio::time::timeout(POST_KILL_REAP_TIMEOUT)`. A leaked descendant outside the pgid that keeps a pipe open can no longer hang the runtime forever — partial output is discarded on elapse and a `tracing::warn!` carries the program/pid for diagnosis.
- **`crates/atelier-core/src/adapter/anthropic.rs`** — `extract_overflow_numbers` rewritten with two anchored regexes (`\b(\d+)\s+tokens\b\s*>\s*(\d+)` and a fallback `\b(\d+)\s+tokens\b`). A future error format that embeds a request_id or timestamp before the token counts can no longer misreport via positional scan. `message_delta` `output_tokens` now always overwrites (was: gated on `> 0`) — Anthropic emits the value monotonically and the last delta is authoritative.
- **`crates/atelier-core/src/staging.rs`** — staging tempdir is `fsync_dir_best_effort`'d before the rename phase. The staged files were already content-fsync'd via `write_with_sync`, but the *staging-tree dirents* were still in the cache — a crash between staging completion and a successful rename could surface as ENOENT mid-batch.
- **`crates/atelier-core/src/persistence.rs`** — two new regression tests (`save_to_re_tightens_relaxed_session_dir`, `registry_save_re_tightens_relaxed_parent_dir`) explicitly cover the chmod-relaxed → save → re-tightened path. Pre-fix the existing tests only checked fresh dirs, which would be 0700 from umask on CI anyway.

**§3 TUI subset multi-pane (v25.3 TUI-1 through TUI-5):**

- **`crates/atelier-tui/src/lib.rs`** — `AppState` extended with `conversation` (bounded `VecDeque<ConversationLine>`), `recent_edits` (bounded `VecDeque<StagedEdit>`), `plan: PlanCanvas`, `total_cost_usd`, `context_tokens: (u32, u32)` (known + unknown), `context_window_tokens` (defaulted to 200k), and `scrub_offset`. New types: `ConversationLine`, `ConversationRole { User, Assistant, Tool, System }` with stable colour mapping, `StagedEdit`, `ScrubCommand { Prev, Next, JumpToHead }`. `InputOutcome` gains `Scrub(ScrubCommand)`.
- **Conversation pane** — role-prefixed list, tail-rendered (newest pinned at bottom), with empty-state placeholder.
- **Diff pane** — renders the most recent `EditStaged` via `Hunks` variants: `Lines` produces `@@ -old,len +new,len @@` headers with `-`/`+` markers; `Created` / `Deleted` show line+byte-count badges; `Binary` and `Same` show their badges. Truncates to the available rows.
- **Plan canvas pane** — per-step glyphs (`[ ]` pending, `[▸]` in-progress, `[✓]` done, `[~]` skipped); terminal-status steps render strike-through; constraints render indented under their step.
- **Cost + context meters** — cost as `$0.XXXX` (no upper bound; meter would be misleading); context as a ratatui `Gauge` with the known/window ratio, plus an explicit `+N unknown` suffix when items have `TokenSource::Unavailable` so a silently-underreporting meter is visible (spec §5 contract).
- **Scrubber-key plumbing** — `[` emits `ScrubCommand::Prev`, `]` emits `Next`, `g` emits `JumpToHead`. `apply_scrub` walks an `Option<usize>` offset (None = HEAD), with `Next` from `Some(1)` collapsing back to HEAD. Header renders `scrub=HEAD` or `scrub=-N`; help footer documents the keys + adds a pinned-mode hint. The §4 time-travel subsystem will consume the offset; until then the TUI just records intent.
- **Layout** — header (2 rows) / top row split conversation+plan (60/40) / bottom row split diff and a vertical strip of cost-gauge + context-gauge + event-log tail (60/40) / 1-row help footer. The existing event-log widget moves into the bottom-right vertical strip; the bus-driven counters still go in the header.

Verified: `cargo test --workspace` → **atelier-core 409 + atelier-cli 11 + atelier-gui 6 + atelier-tui 31** (was 407 / 11 / 6 / 10 in v42; +23 new tests: +2 atelier-core regression on 0700 re-tightening, +21 atelier-tui on the new panes and scrubber); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` green.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 409 atelier-core unit tests + 11 atelier-cli integration tests + 6 atelier-gui unit tests + 31 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 407 / 11 / 6 / 10).

## v42 — 2026-05-16

**Deep-scan v25.2 — residuals pass.** A second pass over the v25.1 re-scan findings. Six load-bearing residuals fixed; the rest documented as deferred quality-of-life items.

- **`crates/atelier-core/src/protocol_strategy.rs`** — v25.2-A. `parse_json_sentinel` now scans past the JSON value via `serde_json::StreamDeserializer::byte_offset()` instead of `find(SENTINEL_CLOSE)`. An embedded `<<<end>>>` (or `<<<harness_meta>>>`) inside a JSON string literal no longer truncates the parse — pre-fix a model emitting `{"summary":"see <<<end>>> tag"}` would surface as `Envelope::Parse` and be miscategorised in the conformance ring. New `TrailingContentAfterSentinel { length, prefix }` variant carries up to 64 bytes of trailing content (UTF-8 char-boundary safe) for triage. Two new regression tests: embedded close-tag and embedded open-tag in summary strings.
- **`crates/atelier-core/src/adapter/anthropic.rs`** — v25.2-B. `parse_retry_after_ms` floors at `MIN_RATE_LIMIT_BACKOFF_MS = 100` so a confused proxy emitting `Retry-After: 0` no longer lets the harness hot-loop the API. SSE EOF now flushes a partial event whose `data:` line lacks a terminating blank line (non-spec server protection) — `take_line(at_eof=true)` consumes the remaining bytes as a final line, and `drain_buffer(at_eof=true)` dispatches the buffered event before reporting "stream ended without message_stop". `handle_event` Malformed-event handling documented (does NOT push a partial Complete first, because the default `chat()` would silently rubber-stamp the malformed turn). New regression test for `Retry-After: 0`.
- **`crates/atelier-core/src/init.rs`** — v25.2-C. `atomic_write` now `fsync_dir_best_effort`s the parent after `persist()` so a power loss between rename and natural dirent fsync can't roll ATELIER.md or `.gitignore` back to pre-write state. Same pattern staging.rs and persistence.rs already use.
- **`crates/atelier-core/src/persistence.rs`** — v25.2-D. `restrict_dir_mode` now emits a `tracing::warn!` on `set_permissions` failure (with the dir's current mode for context) so the spec §14 "0700" promise can't be silently violated on shared hosts. Also warns when stat itself fails.
- **`crates/atelier-core/src/protocol_conformance.rs`** — v25.2-E. `ConformanceSnapshot::rate()` now `#[must_use]` so a stray `unwrap_or(1.0)` after a refactor is at least linted. Empty-buffer test renamed from `empty_buffer_has_perfect_rate_so_new_adapters_dont_fail_a_threshold_check` (stale, contradicted the post-P4 assertion) to `empty_buffer_reports_no_evidence_not_perfect_rate`.
- **`crates/atelier-cli/src/runner.rs`** — v25.2-F. Tool-error feedback path uses `serde_json::json!({ "error": e.to_string() }).to_string()` instead of the unescaped `format!` — error messages containing quotes, backslashes, or newlines now produce valid JSON the model can parse. Assistant turn's `tool_calls` now retains the `harness_meta` envelope-bearing call (filtering moved to a separate `real_tool_calls` view) so the envelope tool_use id survives in conversation history; only dispatch filters it out, not history. New integration test exercising the failing-tool path with characters that need escaping.

Verified: `cargo test --workspace` → **atelier-core 407 + atelier-cli 11 + atelier-gui 6 + atelier-tui 10** (was 404 / 10 / 6 / 10 in v41; +8 new regression tests across A/B/F); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` green.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 407 atelier-core unit tests + 11 atelier-cli integration tests + 6 atelier-gui unit tests + 10 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 404 / 10 / 6 / 10).

## v41 — 2026-05-16

**Deep-scan v25 — five priority groups fixed.** A fresh 6-subsystem audit produced ~230 findings; the highest-priority groups (subprocess hardening, SSE parser correctness, atomicity, fail-open paths, BYOM trait shape) landed in one pass with full rig + workspace verification green.

- **`crates/atelier-core/src/subprocess.rs`** — P1. Env scrubbing: `env_clear()` + explicit `ENV_PASSTHROUGH` allowlist (PATH, HOME, USER, LOGNAME, LANG, LC_*, TERM, TZ, TMPDIR, SHELL). `ANTHROPIC_API_KEY`, `AWS_*`, `GITHUB_TOKEN`, `SSH_AUTH_SOCK` no longer leak into model-controlled tool invocations. Child put in its own process group via tokio's `Command::process_group(0)` on Unix; on timeout we `libc::kill(-pgid, SIGKILL)` so grandchildren (`sh -c "long | pipe"`) are reaped, not orphaned. Per-pipe byte cap (default 1 MiB) with `stdout_truncated`/`stderr_truncated` flags. New `read_capped` helper. Tests cover env strip, PATH passthrough, byte cap truncation, killpg-reaches-grandchildren.
- **`crates/atelier-core/src/adapter/anthropic.rs`** — P2 + P5. **P2:** rewrote `AnthropicSseSource` as a proper line-buffered state machine. `take_line` finds first `\r`/`\n`, handles `\r\n`/`\n`/lone `\r`, waits if buffer ends mid-CRLF. UTF-8 decoding happens only on the assembled event payload — multi-byte codepoints split across TCP chunks no longer corrupt. Bounded buffer (8 MiB) prevents OOM on missing terminators. `message_delta.delta.stop_reason` parsed and propagated; non-stream path too. `Retry-After` header parsed (seconds, 300s cap) replacing hardcoded 1s. `extract_overflow_numbers` lifts `needed`/`limit` out of the body. `too_long` substring tightened to three specific Anthropic markers. **P5:** assistant turn re-sent with `tool_use` content blocks (text + tool_use array) instead of flattened text-only — preserves `tool_use_id` for matching `tool_result` blocks. New tests: chunk-boundary split, one-byte-per-chunk stream, CRLF line terminators, 4-byte emoji split mid-codepoint, stop_reason propagation, Retry-After parsing + 300s cap, overflow token extraction, double-envelope rejection, assistant tool_calls round-trip.
- **`crates/atelier-core/src/adapter/mod.rs`** — `StopReason` enum (`EndTurn`/`MaxTokens`/`ToolUse`/`StopSequence`/`Refusal`/`Other`). `ChatResponse.stop_reason: Option<StopReason>`. `Message.tool_calls: Vec<ToolCallRequest>` + `Message::text(role, content)` constructor.
- **`crates/atelier-core/src/staging.rs`** — P3. Staged file writes use new `write_with_sync` (create → write → `sync_all` → close); rename loop collects unique parents into `BTreeSet` and `fsync_dir_best_effort`s each after the batch. A power-loss between rename N and rename N+1 no longer rolls the workspace back to its pre-batch state.
- **`crates/atelier-core/src/persistence.rs`** — P3. `restrict_dir_mode` helper tightens `sessions/` and `~/.atelier/` directories to 0700 on Unix. Regression tests for both.
- **`crates/atelier-core/src/init.rs`** — P3. `atomic_write` (tempfile + persist) replaces bare `fs::write` for ATELIER.md; `atomic_append_atelier_entry` does read-modify-write through the same helper for `.gitignore`. Crash mid-write can no longer leave a truncated remnant that the next `init` silently skips. Regression test asserts no leftover `.tmpXXX` after init.
- **`crates/atelier-core/src/protocol_conformance.rs`** — P4. `ConformanceSnapshot::rate()` returns `Option<f32>` — empty buffer is `None` ("no evidence"), no silent 1.0 rubber-stamp. Added `has_evidence()` predicate.
- **`crates/atelier-core/src/protocol_strategy.rs`** — P4. `parse_json_sentinel` errors with new `StrategyError::TrailingContentAfterSentinel` on any non-whitespace after the close tag. Catches the double-envelope drop the audit named. Trailing whitespace (newlines from the wire) is still fine.
- **`crates/atelier-core/src/dod.rs`** — P4. `DodConfig::load` doc-warns callers against treating `Ok(None)` as "verification passed". New `paths_searched(repo_root)` helper so callers can log where discovery looked.
- **`crates/atelier-cli/src/runner.rs`** — P4 + P5. `dod_passed = Some(true)` placeholder removed — now `None` until a real DoD runner lands (was lying to downstream readers). Assistant turn pushed with `tool_calls` so multi-turn tool flows preserve the original ids end-to-end.
- **`crates/atelier-core/src/tools/shell.rs`** — surfaces `stdout_truncated`/`stderr_truncated` in the tool's JSON output.
- **`Cargo.toml` + `crates/atelier-core/Cargo.toml`** — `libc = "0.2"` workspace dep, target-gated to `cfg(unix)` in atelier-core.

Verified: `cargo test --workspace` → **atelier-core 404 + atelier-cli 10 + atelier-gui 6 + atelier-tui 10** (was 379 / 10 / 6 / 10; +25 new regression tests across P1–P5); `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` green (21 schemas / 52 artifacts / 112 rig tests / 11 dry-runs).

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 404 atelier-core unit tests + 10 atelier-cli integration tests + 6 atelier-gui unit tests + 10 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 379 / 10 / 6 / 10).

## v40 — 2026-05-16
**Phase C unblock (4) — TUI bootstrap lands.** `crates/atelier-tui` is no longer a scaffold. `cargo run -p atelier-tui` opens a ratatui + crossterm shell that subscribes to the same `atelier-core` broadcast bus the GUI does, renders an event log + an `EditStaged` counter live, and quits cleanly on `q` / `Esc` / `Ctrl-C`. Closes the §3 TUI subset snapshot gate at the wiring level; the richer widgets (conversation, diff, file tree, plan canvas, cost + context meters, timeline scrubber) sit on top.

- **`crates/atelier-tui/Cargo.toml`** — uncommented `ratatui`, `crossterm`, `tokio`, `tracing(-subscriber)` deps; added `tokio-stream`; added `[lib]` so tests can call `render` / `apply` / `handle_key` / `project_event` without booting a terminal.
- **`crates/atelier-tui/src/lib.rs`** — new. Three-zone layout (header / event log / help footer) drawn from an `AppState` that an `apply(&Event)` mutator updates as events arrive on the broadcast bus. Newest events first (no scroll), bounded `MAX_EVENT_LOG = 1_000` so a long-running session can't OOM. Header shows the most recent transition's `to` state + cumulative `EditStaged` count. `handle_key` dispatches `q` / `Esc` / `Ctrl-C` → `InputOutcome::Quit`. `run()` boots a `tokio` multi-thread runtime, enables raw mode + alternate screen, installs a `TerminalGuard` RAII restorer (panic-safe), and runs a `tokio::select!` over the broadcast and a `spawn_blocking` `crossterm::event::poll(50ms)`. Lag-handling: `RecvError::Lagged(_)` synthesises a visible `Lagged` line in the log so a slow-to-redraw TUI doesn't silently lose events.
- **`crates/atelier-tui/src/main.rs`** — three lines. Returns `ExitCode::from(1)` on `io::Error` so terminal-setup failures surface in `$?`.
- **10 unit tests** cover the pure surface: `apply` increments / state-tracking / log-bound, `project_event` for all five `Event` variants, `render` for header content (state + counter), the empty-state placeholder, newest-first ordering in the log, the help footer mentioning `quit`, and `handle_key` quitting on q / Esc / Ctrl-C while continuing on other keys. Tests render onto a `Buffer::empty(Rect)` directly — no PTY needed.
- **`crates/atelier-tui/README.md`** — rewritten. Current state, quick start (`cargo run -p atelier-tui`, `cargo test -p atelier-tui`), ASCII architecture diagram of the pure-vs-impure split, anti-bootstrap retained + extended (don't read off the broadcast inside the render path; don't add Cancel until the typed-command direction is wired the same way `atelier-gui` will need).

Lockfile pins required to stay on rustc 1.85 (ratatui's `instability` proc-macro and its `darling` dep moved their MSRV recently): `instability` 0.3.7. (`darling` was already pinned 0.20.11 in v39 for the GUI; the same pin covers the TUI.)

Verified: `cargo test --workspace` → **atelier-core 379 + atelier-cli 10 + atelier-gui 6 + atelier-tui 10**; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` end-to-end green. Did **not** drive `cargo run -p atelier-tui` interactively — the terminal loop is best verified by a human (alt-screen + raw mode are visual).

Phase C unblockers complete:
- [x] (1) `atelier run` CLI subcommand (v37)
- [x] (2) §1 Anthropic adapter (v38)
- [x] (3) Tauri GUI bootstrap (v39)
- [x] (4) TUI widgets (this entry)

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 379 atelier-core unit tests + 10 atelier-cli integration tests + 6 atelier-gui unit tests + 10 atelier-tui unit tests** (was 21 / 52 / 112 / 11 / 379 / 10 / 6 / 0).

## v39 — 2026-05-16
**Phase C unblock (3) — Tauri GUI bootstrap lands.** `crates/atelier-gui` is no longer a scaffold. The Rust shell + Svelte panel + IPC bridge are wired; `cargo build -p atelier-gui`, `cargo tauri info`, `npm run check`, and `npm run build` all pass. The first panel subscribes to the atelier-core broadcast bus and counts `EditStaged` events — the smallest end-to-end demonstration that the spec §3 wiring round-trips.

D1–D4 decisions captured: `dev.atelier.app` (placeholder bundle id), `Atelier` (product/window title), TypeScript + Vite + Svelte 5, `http://localhost:1420` (Vite pinned with `strictPort: true`).

- **`crates/atelier-gui/Cargo.toml`** — uncommented `tauri`, `tokio`, `tracing(-subscriber)`, `serde(_json)`, `tokio-stream`, `tauri-build`. Added `[lib]` so integration tests can pull in `bridge_event` without going through the binary.
- **`crates/atelier-gui/src/lib.rs`** — new. `run()` boots Tauri, spawns `atelier_core::session::Handle` with `NoopHook`s, and starts a tokio task that pumps the broadcast `Event` stream onto Tauri's event bus as `atelier://event`. Manual `bridge_event` function projects each `Event` variant onto a `{kind, payload}` JSON shape — pure function, 6 unit tests cover the five variants + serialization round-trip. Chose to hand-roll the projection rather than add `Serialize` to `atelier_core::session::Event` so the core enum's serialization surface stays intentional. Single `ping` IPC command lets the eventual integration test confirm round-trip without booting a full session.
- **`crates/atelier-gui/src/main.rs`** — three lines. Calls `atelier_gui::run()` from the `[lib]` crate. `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` to suppress the stray console on Windows release builds.
- **`crates/atelier-gui/build.rs`** — three lines. `tauri_build::build()`.
- **`crates/atelier-gui/tauri.conf.json`** — schema-pinned config; single `main` window 1200×800, narrow CSP (`default-src 'self'`), `frontendDist: "../ui/dist"`, `devUrl: "http://localhost:1420"`. Bundle targets `all` with one placeholder PNG icon.
- **`crates/atelier-gui/capabilities/default.json`** — deliberately narrow: only `core:default` + `core:event:default`. No fs/shell/http — webview must go through the Rust shell, which goes through the §15 dispatcher.
- **`crates/atelier-gui/icons/icon.png`** — 32×32 transparent placeholder, generated via a Python one-liner (zlib + struct, ~80 bytes). Replace with `cargo tauri icon` before the first signed release.
- **`crates/atelier-gui/ui/`** — Vite + Svelte 5 + TypeScript scaffold from `npm create vite@latest`. `App.svelte` subscribes via `@tauri-apps/api/event#listen` and renders an event log + `EditStaged` counter. `vite.config.ts` pinned to `port: 1420, strictPort: true` so Vite can't silently roll to 1421 and 404 the webview. Demo Counter / hero / Svelte+Vite logo assets deleted; `src/app.css` reduced to a comment so component-scoped styles in `App.svelte` own the cascade.
- **`crates/atelier-gui/README.md`** — rewritten from a planning doc to a state-of-the-bootstrap doc. Captures the D1–D4 decisions and where they live in the generated files, the quick-start commands, and an ASCII architecture diagram of the broadcast bridge. Anti-bootstrap retained + extended.
- **`.gitignore`** — added `crates/atelier-gui/ui/{node_modules,dist,.svelte-kit}/`.

Lockfile pins required to stay on rustc 1.85 (Tauri's transitive deps moved their MSRV to 1.86/1.88 in recent releases): `darling` 0.20.11, `serde_with`/`serde_with_macros` 3.14.0, `time` 0.3.41 (pulls `time-core` 0.1.4 + `time-macros` 0.2.22 + `deranged` 0.4.0 + `num-conv` 0.1.0), `plist` 1.8.0, `quick-xml` 0.38.4. `tauri-cli` installed via `cargo install tauri-cli --version "^2.0" --locked`.

Verified: `cargo test --workspace` → **atelier-core 379 + atelier-cli 10 + atelier-gui 6**; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` end-to-end green; `npm --prefix crates/atelier-gui/ui run check` clean; `npm --prefix crates/atelier-gui/ui run build` produces `dist/`. Did **not** drive `cargo tauri dev` (opens an interactive webview window — best verified by a human).

Phase C unblockers status:
- [x] (1) `atelier run` CLI subcommand (v37)
- [x] (2) §1 Anthropic adapter (v38)
- [x] (3) Tauri GUI bootstrap (this entry)
- [ ] (4) TUI widgets — last one

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 379 atelier-core unit tests + 10 atelier-cli integration tests + 6 atelier-gui unit tests** (was 21 / 52 / 112 / 11 / 379 / 10 / 0).

## v38 — 2026-05-16
**Phase C unblock (2) — §1 Anthropic adapter lands.** First real BYOM provider plugged into the `atelier run` loop. Concrete `Adapter` impl talks to `POST https://api.anthropic.com/v1/messages` (`anthropic-version: 2023-06-01`) for both non-streaming `chat()` and streaming `stream()`. Native tool use translates Anthropic's `tool_use` content blocks into `ToolCallRequest`s so the §2 envelope can ride as the `harness_meta` tool's arguments — exactly as Phase B's `Strategy::NativeTool` requires.

- **`crates/atelier-core/src/adapter/anthropic.rs`** — new `AnthropicAdapter`. `new(api_key, model_id)` for explicit credentials; `with_base_url(url)` for tests; `from_env(model_id)` reads `ANTHROPIC_API_KEY`. `Debug` redacts the key.
  - `chat()` — non-streaming POST; parses `content` blocks (`text` + `tool_use`); returns `ChatResponse` with `strategy = NativeTool` iff any tool_use was emitted.
  - `stream()` — POST with `stream: true`; the new `AnthropicSseSource` (private `ChunkSource` impl) parses SSE events (`message_start`, `content_block_*`, `message_delta`, `message_stop`, `error`) into `StreamChunk` values incrementally. Tool-call arguments accumulate across `input_json_delta` events; `content_block_stop` flushes a fully-parsed `ToolCallCompleted`.
  - HTTP error mapping: `401/403` → `Auth`, `429` → `RateLimited`, `5xx` → `Provider`, `400` containing `too_long` → `ContextOverflow`, malformed body → `Malformed`. Truncated streams emit a final `Error` chunk so the loop terminates rather than hanging.
  - `count_tokens()` returns the spec §1 `char/4` fallback with `TokenSource::Approx`; wiring the real `/v1/messages/count_tokens` endpoint is deferred (separate session — needs its own error shape and rate-limit handling). `prompt_cache` and `vision` declared `Unsupported` until those land.
  - **18 unit tests against `wiremock`** covering happy-path chat + tool-use, all error mappings, SSE text-only response, SSE native tool use across multiple `input_json_delta` chunks, SSE truncation, SSE provider `error` event, request shaping (system message split, tool spec forwarding, tool-result block mapping), `from_env`, model-id round-trip, capability defaults. **No live API calls in CI.**
- **`crates/atelier-core/src/adapter/`** — `adapter.rs` restructured to `adapter/mod.rs` so concrete adapters can live as siblings (`adapter/anthropic.rs` first; `openai_compat`, `ollama`, `bedrock`, `vertex` later). `ChunkSource` made `pub(crate)` + `ChunkStream::from_inner` constructor added for sibling-module use. Public API surface unchanged for existing consumers.
- **`crates/atelier-cli/src/runner.rs`** — `ProviderChoice::Anthropic { model_id }` variant added. `Runner::new` becomes fallible (`Result<Self, RunError>`) because Anthropic needs credentials at construction time; `Config` for missing env vars, `Adapter` for everything else.
- **`crates/atelier-cli/src/main.rs`** — `--provider anthropic` accepted. New `--model <id>` flag (defaults to `anthropic:claude-opus-4-7` for the anthropic provider, rejects ids that aren't prefixed `anthropic:`). Unknown providers now error with the supported set listed.
- **`crates/atelier-cli/tests/run_integration.rs`** — 2 new binary tests: `--provider anthropic` without `ANTHROPIC_API_KEY` errors with the env-var name; `--provider anthropic --model claude-opus-4-7` (missing prefix) errors usefully.

Workspace deps added: `wiremock = "0.6"` (dev), `bytes = "1"`. atelier-core gains `reqwest` + `bytes` deps and `wiremock` dev-dep. Lockfile pins: `idna_adapter` 1.2.1, `icu_locale_core/properties/properties_data/normalizer/normalizer_data/provider/collections` ≤ 2.1.1 (the latest 2.2.0 line requires rustc 1.86; we stay on 1.85).

Verified: `cargo test --workspace` → **atelier-core 379 + atelier-cli 10 integration**; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` end-to-end green.

Phase C unblockers status:
- [x] (1) `atelier run` CLI subcommand (v37)
- [x] (2) §1 Anthropic adapter (this entry)
- [ ] (3) Tauri GUI bootstrap — needs interactive D1–D4
- [ ] (4) TUI widgets — parallel to (3)

`atelier run --provider anthropic --model anthropic:claude-opus-4-7 "..."` is now meaningful end-to-end against a live API; the integration tests stay on the mock so CI never touches the network.

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 379 atelier-core unit tests + 10 atelier-cli integration tests** (was 21 / 52 / 112 / 11 / 361 / 8).

## v37 — 2026-05-16
**Phase C unblock (1) — `atelier run` CLI subcommand lands.** First end-to-end driver of the agent loop. Wires the §2.5 actor + §15 dispatcher + 7 built-in tools + §15 hooks + §7 DoD + §11 sandbox + §1 typed ledger against the in-tree `MockAdapter`. The §3 mechanical gate (scripted multi-file rename, byte-equal final diff) is now runnable in CI against the mock; the same code runs against any future adapter (Anthropic next) without changes.

- **`crates/atelier-cli/src/runner.rs`** — new `Runner` API with `Runner::new(workspace, provider, sink)` + `with_max_turns(n)` + `run(prompt)`. Loop: load `HookSet` + `DodConfig` → build `Dispatcher` with all 7 built-in tools + `ShellHookExecutor` → spawn `Session` actor → loop turns (`adapter.chat` → parse envelope via `protocol_strategy` → dispatch tool calls via `SessionDispatcher` → feed results back into messages) until `claimed_done: true` or `max_turns`. Transition to `Verifying` for DoD checks, persist via `OnDiskSession::save_to` to `<repo>/.atelier/sessions/<uuid>/session.json`. `EventSink::{Stdout, Capture, Null}` for binary vs. tests vs. silence.
- **`crates/atelier-cli/src/main.rs`** — `atelier run [OPTIONS] [PROMPT]` subcommand. Flags: `--provider mock` (only `mock` for v0; `anthropic` lands with unblock 2), `--workspace PATH`, `--max-turns N`, `--prompt-file PATH` (or `-` for stdin). Prints session id + final state + DoD outcome on success; surface a useful error pointing at Phase C unblock (2) when an unsupported provider is named.
- **`crates/atelier-cli/tests/run_integration.rs`** — 8 integration tests:
  - loops until `claimed_done` and reaches `State::Done`
  - dispatches real `write_file` tool calls and loops back into the next turn
  - bails after `max_turns` without `claimed_done` (no infinite loop)
  - **scripted multi-file rename — the §3 mechanical gate against MockAdapter** (3 files; the spec's gate scales to 10 with the same shape)
  - persists session.json under `.atelier/sessions/<uuid>/`
  - `assert_cmd`-driven binary tests: `--help` lists `run` + `--provider`, unknown provider errors helpfully, empty prompt rejected
- **Drop-order fix uncovered by the integration tests:** `SessionDispatcher` holds a `broadcast::Sender` clone; without dropping it before awaiting the event-drain task, the runner hung waiting for a channel that couldn't close. The runner now drops `session_dispatcher` then `session_handle` before awaiting, with a safety `tokio::time::timeout` wrapping the await so a future regression can't hang the process.

Workspace deps added: `assert_cmd = "2"`, `predicates = "3"`. atelier-cli gains `tokio` (full), `serde_json`, `parking_lot`, `tracing`, `thiserror`.

Verified: `cargo test --workspace` → **atelier-core 361 + atelier-cli 8 integration**; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `make check` end-to-end green.

Phase C unblockers status:
- [x] (1) `atelier run` CLI subcommand
- [ ] (2) §1 Anthropic adapter — next session
- [ ] (3) Tauri GUI bootstrap — needs interactive D1–D4
- [ ] (4) TUI widgets — parallel to (3)

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 361 atelier-core unit tests + 8 atelier-cli integration tests** (was 21 / 52 / 112 / 11 / 361 / 0).

## v36 — 2026-05-16
**Spec edits to clear the path for multi-provider / multi-model routing.** No new code — three small structural changes so the user's eventual Bedrock + Vertex + Ollama / llama.cpp / MLX-LM adapters land cleanly into the existing phase plan instead of forcing schema bumps or auth-layer surgery later.

- **Free-form roles in `schemas/config/routing.v1.json`.** `executor` stays required (catch-all loop runner and fallback for any role-less plan step). `planner` and `critic` stay as well-known optional roles with their specific UI semantics. **Any additional key is now a free-form custom role** — `documenter`, `web_trawler`, `architect`, `reviewer`, anything the user wants — mapped to a `<provider>:<model>` ref or null. The dispatcher (Phase E work) will route a turn to a custom role when a `PlanStep` carries a matching role tag. `additionalProperties` swapped from `false` to a `model_ref`-or-null shape; description updated; spec §1 "Per-task routing" rewritten to spell out the loose-vs-strict-roles choice (now loose).
- **`examples/config/routing_multimodel.v1.json`** — new bundled example that demonstrates the user's scenario verbatim: cloud frontier for `architect` / `reviewer`, local Ollama for `documenter` / `web_trawler`. Validated by the rig (21/21 schemas, 52/52 artifacts).
- **Spec §11 "Credentials abstraction"** — new subsection introducing the `CredentialsProvider` trait + `CredentialShape::{ApiKey, AwsSigV4, GcpAdc, Local}`. The existing keychain/env flow is the `ApiKey` impl; SigV4 (Bedrock) and ADC (Vertex) gain dedicated shapes so adapters declare *how* they authenticate without each adapter reimplementing the resolution chain. CLI surface extends with `atelier login bedrock` / `atelier login vertex` / `atelier login ollama`. Audit (§12) records the resolved shape, never the secret.
- **Spec §"Phased build plan"** — Phase E gains native Bedrock + Vertex adapters + per-task routing UI as named items (calibrated against Phase B–D ledger data; LiteLLM proxy from Phase A covers them day-one). Phase F's "OpenAI and local adapters; per-task routing" line replaced with per-adapter named items (Ollama / llama.cpp / MLX-LM) plus the explicit note that the LiteLLM proxy already handles them transparently.
- **`tasks/todo.md`** — Phase E gets a new "Native cloud adapters + per-task routing UI" subsection (4 items + 2 prereqs: `CredentialsProvider` trait + CLI extension). Phase F's adapter list breaks out into per-provider items.

Why this is structural-only: the user asked where to land Bedrock / Vertex / local LLMs / multi-model routing. Today the spec's `routing.v1.json` fixes 3 roles, which doesn't map to the user's task-affinity model. Today §11 covers API-key auth only. Fixing both now (small spec + schema edits) lets the eventual adapter work in Phase E / Phase F slot in without forcing a routing v2 or §11 rewrite mid-build.

Verified: `make check` green — 21/21 schemas, **52/52 artifacts** (was 51; +1 for `routing_multimodel.v1.json`), 112 rig tests, 11/11 canonical dry-runs. **Rust unchanged** (no atelier-core code touched this rev).

### Rig counts
- **21 schemas / 52 artifacts / 112 tests / 11 dry-runs / 361 Rust unit tests** (was 21 / 51 / 112 / 11 / 361).

## v35 — 2026-05-16
**All remaining v34-analysis items closed.** Four medium-severity fixes (one regression of a v34 partial fix + three new) and seven low-severity cleanups. The deep analysis run after v34 surfaced these; this rev clears the list.

- **M1-incomplete — `diff::hunks_for_created` / `hunks_for_deleted` non-UTF-8.** v34 only patched `hunks_for`. The two sibling functions still silently coerced non-UTF-8 bytes to `""` via `unwrap_or`, producing `Created{new_line_count: 0}` for a real-world latin-1 file. Same fix applied: non-UTF-8 → `Hunks::Binary`. Two new tests (`created_for_non_utf8_text_returns_binary`, `deleted_for_non_utf8_text_returns_binary`).
- **M3 — `subprocess::run` post-kill timeout now observable.** The 5 s `POST_KILL_REAP_TIMEOUT` block previously silently swallowed both successful and timed-out reaps. Both still surface to the caller as `(None, true)` (correct — same observable shape) but a `tracing::warn!` with the program name, child PID, and reap-timeout-ms fires when the post-kill wait itself times out, so operators can distinguish "killed and reaped clean" from "killed but the kernel hasn't released it → possible zombie".
- **M4 — dispatcher hooks run in parallel.** `Dispatcher::dispatch`'s pre/post hook loops swapped from sequential `for manifest in …` to `futures::future::join_all(...)`. N pre-tool hooks now share one round of fork/exec overhead instead of serialising it. Spec §15 warn-but-never-block is preserved (failure isolation lives inside the executor). `futures` was already a workspace dep; no new dep.
- **M5 — `OnDiskSession::save_to` + `Registry::save` fsync the parent dir.** Atomic rename guarantees content visibility but not durability of the directory entry — a power loss right after `persist` returns can roll the rename back. Both call sites now invoke a new `cfg(unix)` `fsync_dir(parent)` helper after `tmp.persist`. Windows fallback is a deliberate no-op (spec §11 doesn't target it).
- **L4 — `MockAdapter` swapped to `parking_lot::Mutex`.** Same poison-tolerance treatment as v34 gave `Ledger`. Removes the last 3 `.lock().unwrap()` patterns in the crate.
- **L5 — schema `cost_ledger.items` gains `additionalProperties: false`.** Matches the tight-contract default the rest of `schemas/session/v1.json` uses; closes the v32 S6 smell. Rust serde already rejected extras (`LedgerEntry` is a tagged enum), so this affects only non-Rust validators of the schema.
- **L6 — `spawn_blocking` panic payload preserved.** New shared helper `tools::join_error_to_tool_error(NAME, join_err)` branches on `is_panic`, downcasts the `Box<dyn Any>` payload to `&str` / `String`, and surfaces it via `stderr: "blocking pool panic: <message>"`. All 6 file tools' `.await.map_err(...)` lines consolidate into one call to the helper.
- **L7 — `Send + Sync` posture documented.** `ContextManager`, `MemoryStore`, `PlanCanvas` all gained a doc-comment note that they're not internally `Send + Sync` (no interior mutability) and need external `Arc<Mutex<_>>` for shared access.
- **L8 — `HookSet::merge_dir` emits a shadow warning.** A per-repo hook silently replacing a same-named global is now `tracing::info!`-ed with the hook name + path of the shadowing manifest. UX paper cut closed; future "why isn't my global hook firing?" debugging gets a log line to grep for.
- **L9 — `shell` tool clones the session sandbox.** Previously rebuilt the policy from scratch via `SandboxPolicy::restrictive(ctx.sandbox.repo_root())`, silently dropping `extra_read_paths` / `extra_write_paths`. Now `ctx.sandbox.clone()` preserves session extras across shell calls.
- **L10 — `HookExecutor` privacy expectation documented.** Trait doc-comment calls out that the `payload` carries tool arguments verbatim (shell command strings, paths, write contents) and that hook implementations persisting payloads must treat them as sensitive — the §12 redaction layer (when it lands) will route hook payloads through the same filter.
- **L11 — `Staging::ensure_target_inside_workspace` TOCTOU caveat documented.** The single-threaded-per-turn assumption that closes the race is now spelled out in the helper's doc, with a note that parallelising the apply step would reopen it and should switch to `openat`-style relative-fd I/O.

Verified: `cargo test -p atelier-core --lib` → **361 passed** (was 359; +2 for the two new diff tests); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green.

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 361 Rust unit tests** (was 21 / 51 / 112 / 11 / 359).

## v34 — 2026-05-16
**All remaining v32 / v33 analysis items addressed.** Closes the HIGH-severity runtime issues (blocking I/O stalling tokio, poisonable ledger lock), the MEDIUM correctness issues (non-UTF-8 diff corruption, unbounded post-kill wait), and the LOW documentation + test-hygiene drift.

- **H1 — blocking I/O moved to the blocking pool.** Every file-touching `Tool::execute` (`read_file`, `list_dir`, `grep`, `write_file`, `edit_file`, `ast_grep`) now wraps its `std::fs::*` + `walkdir` + `Staging::commit` work in `tokio::task::spawn_blocking`. The args parse + sandbox-policy clone happen on the async side (cheap); the I/O happens on the blocking pool. A `JoinError` from the blocking pool maps to `ToolError::ExecutionFailed`. Net effect: a multi-MB read or deep walk no longer pins a tokio worker thread, so the §2.5 actor inbox + broadcast bus stay responsive even under load. `shell` was already async via `subprocess::run`.
- **H2 — `Ledger` swapped from `std::sync::RwLock` to `parking_lot::RwLock`.** Removes all 8 `.expect("ledger lock poisoned")` sites. `parking_lot` doesn't poison on a panic-with-write-guard, so a single panicking tool can no longer brick every subsequent ledger read. External API unchanged. `parking_lot` added as a direct dep (already transitive via tokio).
- **M1 — `diff::hunks_for` non-UTF-8 inputs now return `Hunks::Binary`.** The prior `unwrap_or("")` silently coerced non-UTF-8 buffers into identical empty strings, returning a bogus "no diff" when two different latin-1 / shift-jis buffers were compared. New test `non_utf8_text_bytes_yield_binary_not_silent_corruption` proves the fix.
- **M2 — `subprocess::run` post-kill wait bounded.** After `start_kill`, `child.wait()` is now wrapped in `tokio::time::timeout(POST_KILL_REAP_TIMEOUT)` (5 s). A child stuck in D-state (pending uninterruptible I/O — e.g., a hung NFS mount) can ignore SIGKILL until the kernel releases it; the prior code would block the worker thread forever. Constant declared at module top with the rationale.
- **L1 — misleading `Ledger::clone` docstring removed.** Replaced with explicit "share via `Arc<Ledger>`, not by cloning" + a note that the underlying `parking_lot::RwLock` makes the ledger panic-tolerant.
- **L2 — `Discrepancy::DuplicateClaim` orthogonality documented.** The duplicate flag + per-path `Claimed`/`KindMismatch` discrepancies are intentionally both surfaced — the duplicate is a model-quality signal, the per-path comparison is a verification signal. Doc-comment makes the design explicit and points UIs at `Discrepancy::path` for grouping.
- **L3 — tool tests use the actual tempdir as `SandboxPolicy::restrictive` root.** 33 `SandboxPolicy::restrictive("/tmp/x")` sites swapped to `SandboxPolicy::restrictive(dir.path())` (or `ws.path()` for the symlink tests). Tests are now consistent with the realistic case where the workspace and sandbox root match — important because the sandbox is per-session, and tests previously got away with the mismatch only because file tools don't enforce sandbox.

Verified: `cargo test -p atelier-core --lib` → **359 passed** (was 358; +1 for the M1 non-UTF-8 test); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green.

Workspace dep added: `parking_lot = "0.12"`.

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 359 Rust unit tests** (was 21 / 51 / 112 / 11 / 358).

## v33 — 2026-05-16
**Three critical issues from the v32 deep analysis fixed.** Closes the symlink-escape bypass, wires hook execution into the dispatch lifecycle, and adds the `validate_args` trait seam.

- **C1 — symlink containment in file tools + `Staging`.** New module `crates/atelier-core/src/path_safety.rs` with `resolve_repo_path` (syntax-level; rejects absolute paths + `..`), `ensure_inside_workspace_existing` (canonicalize-and-prefix-check; catches the symlink-to-outside attack), and `ensure_inside_workspace_creatable` (same, for not-yet-existing targets). Every file-touching tool now calls the appropriate helper after `resolve_repo_path`: `read_file`, `list_dir`, `edit_file`, `write_file`, `grep`, `ast_grep`. `grep` and `ast_grep` additionally skip symlinks at the leaf — `WalkDir::follow_links(false)` only controls traversal, not whether a reported leaf is itself a symlink to outside. `Staging::commit` does its own containment check via `ensure_target_inside_workspace` (walks up to the deepest existing ancestor, canonicalizes it, asserts prefix) so direct `Staging` callers also get the guarantee. 10 new unit tests covering symlink-to-outside in both file and directory positions, repo-internal symlinks still accepted, missing files / missing parents.
- **C2 — `HookExecutor` actually fires from `Dispatcher::dispatch`.** Dispatcher gains `executor: Arc<dyn HookExecutor>` (default `NoopHookExecutor`) + `Dispatcher::with_executor` builder. `dispatch` now: lookup → validate_args → **pre-tool hooks** → execute → build outcome → **post-tool hooks** → return. Per spec §15 "warn-but-never-block", the executor's own time-budget + error logging stays inside the executor; the dispatcher just `.await`s. Pre-tool payload = `{event, tool_name, tool_call_id, arguments}`; post-tool payload adds `{ok, error_kind?}` so a hook can act on outcomes. 3 new unit tests with a recording mock executor verify both phases fire in order, payload shape is correct, and unknown-tool short-circuits before any hook runs.
- **C3 — `Tool::validate_args` trait seam.** New trait method `validate_args(&self, args: &serde_json::Value) -> Result<(), String>`; default `Ok(())`. Dispatcher calls it between lookup and pre-tool hooks; `Err(msg)` short-circuits with `ToolError::SchemaViolation` (ledger entry recorded, no hooks fire, no execute attempted). **Built-in tools rely on the default** because their `execute` impls deserialise via `#[serde(deny_unknown_fields)]` typed structs that produce `SchemaViolation` on shape errors — equivalent to running the bundled manifest's `input_schema` for the constraints those manifests express (types, required, enums, unknown fields). The seam is built so MCP-routed tools and any future built-in with constraints serde can't express (regex, length bounds, `oneOf`/`anyOf` semantics) plug in a real JSONSchema validator without dispatcher churn. 1 new dispatcher test proves the gate fires before execute and hooks.

**Why no `jsonschema` dep was added.** The workspace's `jsonschema = "0.26"` pin transitively requires `icu_*` 2.x which requires rustc 1.86+; we're pinned 1.85.0. The honest fix is the trait-seam-with-serde-fallback above; bumping toolchain or downgrading `jsonschema` to a non-icu version would be its own commit with its own scope.

**Drive-by:** `tools/grep.rs` and `tools/ast_grep.rs` use the canonical walk root (`&root`) for `strip_prefix` of reported paths, not `ctx.workspace_root` — the canonical and uncanonical forms differ on macOS (`/var/folders/...` vs `/private/var/folders/...`) and the prior code accidentally returned absolute paths when they mismatched.

Verified: `cargo test -p atelier-core --lib` → **358 passed** (was 344; +14 across path_safety + symlink tests in read_file/grep + Staging containment test + dispatcher's three new hook-execution tests + validate_args gate test); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green.

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 358 Rust unit tests** (was 21 / 51 / 112 / 11 / 344).

## v32 — 2026-05-16
**Phase C UI unblockers — four follow-ons + the seven built-in tools land.** Closes the loop on the three honest call-outs from v31 (subprocess+sandbox plumbing extracted, dispatcher's pure/wrapped split made explicit, gui bootstrap docs split into decisions vs. mechanical) and ships the §15 built-in tool implementations.

- **`crates/atelier-gui/README.md`** rewritten as a D1–D4 decisions table (each row: choice / why it matters / safe default) plus an M1–M6 mechanical-steps table. D1 (bundle id) flagged irreversible-for-codesign; D3 (frontend stack) flagged load-bearing-once-chosen. New anti-bootstrap entry: don't build a `SessionViewModel` aggregator in `atelier-core` before the frontend exists.
- **Shared subprocess+sandbox+timeout helper** (`crates/atelier-core/src/subprocess.rs`). `run(program, args, &SubprocessSpec) -> SubprocessOutcome { exit_code, stdout, stderr, duration_ms, timed_out }` spawns under `tokio::process::Command`, drains stdout + stderr in concurrent reader tasks (no pipe-deadlock), times out via `tokio::time::timeout` → SIGKILL → reap. `sandboxed_argv(argv, &SandboxPolicy)` returns the platform-specific `(program, wrapped_args)` pair: macOS = `("sandbox-exec", ["-p", profile, "--", argv...])`, Linux = `("bwrap", linux_bwrap_argv(policy, argv))`, other = `SubprocessError::UnsupportedPlatform`. CI doesn't install `bubblewrap`, so the test suite uses bare `run` against `echo`/`sh -c` (no sandbox dep); cfg-gated tests exercise the wrapped path on macOS where `sandbox-exec` is always present.
- **`SessionDispatcher`** (`crates/atelier-core/src/dispatcher.rs`). Thin wrapper around the pure `Dispatcher`; owns `Arc<Ledger>` + `broadcast::Sender<Event>` and performs the two side effects after each dispatch (`ledger.append` + `for ev in events { sender.send(ev) }`). Pure `Dispatcher` stays the unit-test surface. `Sender::send` returning Err for "no subscribers" is silently swallowed — headless runs don't surface dispatcher errors when no UI is attached. `Handle::events_sender()` newly exposed so the wiring code can plumb the cloned `Sender` in at session start.
- **`crates/atelier-core/src/tools/`** — seven `Tool` impls + a shared `resolve_repo_path` helper enforcing "repo-relative, no `..`, no absolute" uniformly:
  - `read_file` — offset/length window with truncation flag.
  - `list_dir` — sorted entries, dot-files hidden by default.
  - `grep` — regex via `regex` crate; walks via `walkdir`; skips dot-dirs / binary (NUL-in-8KB) / files >1 MB; tempdir-prefix workaround for `filter_entry` rejecting roots starting with `.tmp`.
  - `write_file` — routes through `Staging::commit`; staged-writes report flows into `Event::EditStaged`.
  - `edit_file` — anchor-based patch; rejects ambiguous anchors; routes through `Staging` with `expected_pre_hash` for §14 concurrent-edit detection.
  - `ast_grep` — `kind:<node-kind>` patterns over bundled `tree-sitter-json`; richer pattern syntax + other Tier-1 grammars land alongside §7 hallucination detector.
  - `shell` — `sh -c` via `subprocess::sandboxed_argv` + `subprocess::run`; cwd is repo-relative; `allow_net` derives a fresh `with_net` policy.
- **`ShellHookExecutor`** (dispatcher.rs) — concrete `HookExecutor` impl spawning the hook's `command` via `sh -c` inside the session sandbox, forwarding the hook payload as `ATELIER_HOOK_PAYLOAD` env-var. Warns past `time_budget_ms` via `tracing` but **never blocks** (spec §15). Non-shell impls log + skip.

**Drive-by fix in `sandbox::macos_profile`** — now `(import "system.sb")`s Apple's baseline profile so subprocess loading actually works inside the sandbox. Without this, the hand-rolled enumeration of allowed paths was incomplete and `sandbox-exec` killed children with SIGABRT during dyld setup. Test asserts the import precedes `(deny default)` so the explicit restrictions still override the baseline's allows.

Workspace deps added: `regex = "1.11"`, `walkdir = "2.5"`.

Verified: `cargo test -p atelier-core --lib` → **344 passed** (was 289; +55 across subprocess + SessionDispatcher + tools/ + ShellHookExecutor); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green (21/21 schemas, 51/51 artifacts, 112 rig tests, 11/11 canonical dry-runs).

Explicitly **not done this round** — tracked as the remaining Phase C UI unblocker:
- §1 Anthropic adapter against the real Messages API. Trait + `MockAdapter` (v31) and dispatcher + built-in tools (this rev) leave it as a self-contained piece: SSE streaming + native tool-use channel + `wiremock`/recorded-fixture-based tests (no live API in CI).

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 344 Rust unit tests** (was 21 / 51 / 112 / 11 / 289).

## v31 — 2026-05-16
**Phase C UI unblockers — first three of five.** Spec §"Phased build plan" Phase C section was extended in v30 to spell out the five unblockers; this rev lands items 1–3 (the trait + ledger + dispatcher skeleton). Items 4 (seven built-in tool impls) and 5 (Anthropic adapter against the real Messages API) follow in their own commits — bundling them here would produce shallow stubs against my prior pattern of one substantial module per round.

- **§1 BYOM adapter trait** (`crates/atelier-core/src/adapter.rs`). Async `Adapter` trait: `model_id / capabilities / conformance / count_tokens / chat / stream`. `chat` has a default impl in terms of `stream` so streaming-only providers cost nothing extra. `Capabilities { native_tool_use, streaming, vision, prompt_cache, structured_output, long_context, context_window_tokens }`; `CapabilityClaim::{Supported, ClaimedButBroken, Unsupported}` flags the "claimed-but-broken" trap state from spec §1's matrix. `AdapterError` covers `ContextOverflow / Auth / Unreachable / Malformed / RateLimited / Provider / NotConfigured`; `requires_user_decision()` maps each to the §2.5 `Recovery` routing. `Message / Role / ToolSpec / ToolCallRequest / ChatResponse / Usage / StreamChunk::{Text, ToolCallStarted, ToolCallDelta, ToolCallCompleted, Complete, Error}` all round-trip through serde. `MockAdapter` queues a FIFO of `ChunkStream`s + has a `with_context_window` knob that fires `ContextOverflow` deterministically; `record_conformance` lets tests assert the matrix-vs-ring-buffer interaction. Workspace dep added: `async-trait`.
- **§1 typed cost ledger** (`crates/atelier-core/src/ledger.rs` + retypes `OnDiskSession.cost_ledger`). `LedgerEntry::{ModelCall, ToolCall, CacheBust}` enforces the schema's per-kind required fields at compile time (cannot construct a `ToolCall` without `tool_name`/`latency_ms`, a `ModelCall` without `model_id`/`prompt_tokens`/etc.). `Ledger` is append-only, `RwLock`-backed; `append / to_vec / from_vec / by_kind / total_cost_usd / total_tokens / entries_without_cost` (latter so the §3 cost meter renders "$1.23 + N unknown" rather than understating). Helpers: `LedgerEntry::tool_call(...)`, `LedgerEntry::cache_bust_from(&CacheBustEvent)` bridges the §5 context manager's eviction event into a ledger entry without `context.rs` importing the ledger. `local_cost_usd(latency_ms, rate)` + `DEFAULT_LOCAL_RATE_USD_PER_SEC = $0.00028/sec` (spec §1 PROVISIONAL). `OnDiskSession.cost_ledger: Vec<serde_json::Value>` → `Vec<LedgerEntry>`; all 4 bundled session examples still round-trip.
- **§15 tool dispatcher skeleton** (`crates/atelier-core/src/dispatcher.rs`). Async `Tool` trait (`name`, `side_effect_class`, `execute(args, &ToolContext)`); `ToolRegistry` keyed by name with sorted iteration + duplicate-name rejection. `Dispatcher::dispatch` walks the per-tool-call lifecycle: lookup tool → identify pre-tool / post-tool hooks via `HookSet::for_tool_event` → execute → translate any `staged_writes: CommitReport` into per-file `Event::EditStaged` via the `edit_staged_events` helper (already built in v30) → build a `LedgerEntry::ToolCall` with measured latency + local cost. Returns a `DispatchOutcome` — pure (no side effects); the caller appends to the ledger + broadcasts events. Failed dispatches still produce a ledger entry; unknown tool names fail closed with `ToolError::ExecutionFailed` so the harness can never silently no-op a model-emitted call. `SideEffectClass::{LocalSafe, LocalRisky, SharedState, Irreversible}` with `budget_cost()` matching spec §8 PROVISIONAL (0/1/20/20). `HookExecutor` trait + `NoopHookExecutor` sketched; real subprocess execution lands with item 4's tool-impls follow-on (it shares the §11 sandbox launcher those tools need).

Verified: `cargo test -p atelier-core --lib` → **289 passed** (was 242; +47 across the three new modules); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green (21/21 schemas, 51/51 artifacts including session round-trips of the now-typed `cost_ledger`, 112 rig tests, 11/11 canonical dry-runs).

Explicitly **not done this round** — each is tracked in `tasks/todo.md` as the remaining Phase C UI unblockers:
- §15 built-in tool implementations (`read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`). Each gets its own module; the dispatcher already accepts them via the `Tool` trait. Lands across multiple commits.
- §1 Anthropic adapter against the real Messages API. Needs SSE streaming + tool-use channel + `wiremock`/recorded-fixture-based tests (no live API in CI). The trait + `MockAdapter` this rev landed make this self-contained.
- Real **hook subprocess execution** (the `HookExecutor` concrete impl) — pairs naturally with the `shell` tool impl since both wrap `tokio::process` inside the §11 sandbox.

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 289 Rust unit tests** (was 21 / 51 / 112 / 11 / 242).

## v30 — 2026-05-16
**Phase C data-layer prerequisites — four typed APIs the UI will consume.** Lays the data underneath the Phase C UI work without touching the GUI/TUI bootstrap. Spec §"Phased build plan" Phase C section was extended to spell out these prerequisites explicitly.

- **§5 context manager** (`crates/atelier-core/src/context.rs`). `ContextItem { id, payload, tokens: TokenCount{count,source}, provenance, pinned, added_at, last_used }`. `Payload::{FileRef, InlineText, BlobRef}` covers the three concrete shapes the workspace renders; `Provenance::{Initial, UserAttached, ToolResult, MemoryPromoted, PinnedByUser}` carries the why-here trace. `ContextManager` insertion-ordered with `add / pin / unpin / evict / touch / iter / token_snapshot`. `evict` refuses pinned items and returns a `CacheBustEvent` the caller forwards to the §1 cost ledger as `kind: cache_bust` — keeps the module pure of I/O. `TokenSnapshot` separates known from `Unavailable` so the §5 token meter never silently underreports.
- **§5 typed memory** (`crates/atelier-core/src/memory.rs` + retypes `OnDiskSession.memory`). `MemoryCard` matching the schema exactly (`id, content, created_at, last_used, pinned?`); `MemoryStore` with `add / touch / pin / unpin / evict / promote_to_global`. `promote_to_global` returns `PromoteOutput { relative_path, bytes }` for the caller to write (same purity discipline as `context.rs`). `OnDiskSession.memory: Vec<serde_json::Value>` → `Vec<MemoryCard>`; all 4 bundled session examples still round-trip and `make artifacts` validates them.
- **§5 typed plan** (`crates/atelier-core/src/plan.rs` + retypes `OnDiskSession.plan.steps`). `PlanStep { id, text, status, constraints? }` + `PlanStatus::{Pending, InProgress, Done, Skipped}`. `PlanCanvas` with auto-id `add`, `insert` (rejects duplicates, advances next-serial past imported `step-N` ids), `remove`, `mark_status / mark_done / mark_skipped`, idempotent `add_constraint`, and `reorder` that validates membership before mutating. `apply_envelope(&PlanUpdate) -> ApplyReport` consumes the §2 envelope's `plan_update` field (best-effort text-match for `complete`/`remove`; `reorder` from an envelope is intentionally dropped with a UI-visible reason). `OnDiskSession.plan.steps: Vec<serde_json::Value>` → `Vec<PlanStep>`.
- **Incremental diff stream** (`crates/atelier-core/src/diff.rs` + `staging::FileOutcome.hunks` + `session::Event::EditStaged`). `Hunks::{Same, Lines{hunks}, Binary, Created, Deleted}` via the `similar` crate. Binary detection uses §14's "NUL in first 8 KB" rule so the diff layer and the §14 diff-blob store agree. `staging::Staging::commit` now reads the pre-image once per file (for both conflict check and hunk extraction; race-free) and stamps the `Hunks` onto every `FileOutcome`. `session::Event::EditStaged { path, hunks }` is the §3 "live diff updates as the agent edits" carrier; `session::edit_staged_events(&CommitReport)` is the pure translator the tool dispatcher will call to forward each commit's per-file events onto the bus.

Workspace deps added: `similar = "2.7"`.

Verified: `cargo test -p atelier-core --lib` → **242 passed** (was 172; +70 across the four new modules); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green (21/21 schemas, 51/51 artifacts including round-trips of the retyped session memory + plan fields, 112 rig tests, 11/11 canonical dry-runs).

Explicitly **not done this round** — each is tracked in `tasks/todo.md`:
- Phase C UI itself — `cargo tauri init` and TUI widgets still need the interactive bootstrap and an adapter producing real envelopes. The data layer this rev landed is what those UIs will consume.
- §5 non-destructive compaction with cost disclosure + mental-model panel — defers until the GUI work begins.
- §5 mechanical gate (context-panel API assertions; cache-bust ledger entry on eviction) — needs the eventual UI to assert against; the underlying ops + `CacheBustEvent` data are in place and unit-tested.

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 242 Rust unit tests** (was 21 / 51 / 112 / 11 / 172).

## v29 — 2026-05-16
**Phase B foundation — §2 protocol + §7 verification (subset, code-first).** Five modules land. Phase B's real-model conformance gate (≥95% on canonical workload across Anthropic + OpenAI) still needs §1 adapters; everything that can be built as a pure data layer is now built and tested.

- **§2 envelope types** (`crates/atelier-core/src/protocol.rs`). Typed `Envelope` mirroring `schemas/model_protocol/envelope.v1.json` with `serde(deny_unknown_fields)`. Round-trips all three bundled `prompts/protocol_fewshot/` examples. Runtime validates the schema's `maxLength: 500` summary cap (JSON Schema's runtime cost in the rig is paid here too). Every optional field is `Option<_>` so absent vs. default is type-distinct — enforces spec §2 "never silently substitute 'everything OK.'"
- **§2 three emission strategies** (`crates/atelier-core/src/protocol_strategy.rs`). `Strategy::{NativeTool, JsonSentinel, RegexProse}` with `downshift()` chain. Each strategy has an `encode`/`parse` pair. `parse_json_sentinel` returns `(envelope, prose)` so UI renders the two streams separately. The regex-prose fallback is deliberately lossy per spec (drops `plan_update` and `constraints_acknowledged`); both round-trip absent on re-parse, surfacing as gray badges in the UI.
- **§2 conformance tracker** (`crates/atelier-core/src/protocol_conformance.rs`). `TurnConformance` issues `TurnDecision::{Reprompt, Downshift, EscalateToUser}` — `Reprompt` 3× per strategy, then downshift, then escalate at the bottom of the stack. Cross-call `ConformanceRingBuffer` (capacity 100, PROVISIONAL) for the §1 `Adapter::conformance()` window with `snapshot()` returning per-strategy success counts.
- **§7 did-it-do-what-it-said** (`crates/atelier-core/src/verify.rs`). Pure function `compare(envelope, &[ObservedChange]) -> Vec<Discrepancy>`. Detects: claimed-but-not-observed, observed-but-not-claimed, kind-mismatch (e.g. claimed delete + observed modify), duplicate claims. Lying-agent gate's primary signal.
- **§7 DoD config** (`crates/atelier-core/src/dod.rs` + `schemas/config/dod.v1.json` + `examples/config/dod.v1.json`). `DodConfig` loader with `(name, tier, command, working_dir, timeout_ms, expect, tags)` checks. Tier enum matches spec §7 (`test / typecheck / lint / build / custom`). Discovery: per-repo `<repo>/.atelier/dod.json` overrides global `~/.atelier/dod.json`; missing both is a soft no-config state. Validates name regex (shared with hook names), absolute / `..`-escaping `working_dir`, zero timeouts, and unknown fields. Schema-validated end-to-end by the rig.

Verified: `cargo test -p atelier-core --lib` → **172 passed** (was 97; +75 across the five new modules); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green (**51/51 artifacts** including the new DoD example, **112 rig tests**, **11/11 canonical dry-runs**).

Explicitly **not done this round** — each is tracked in `tasks/todo.md`:
- §2 nightly protocol-overhead measurement harness + `ci/nightly/protocol_overhead.yml` — gated on adapter to drive real model calls.
- §2 per-adapter few-shot override hook — defers to the BYOM adapter trait (§1).
- §2 real-model conformance gate (Anthropic + OpenAI canonical workload ≥95%) — needs Phase A adapters.
- §7 Tier-1 hallucination detector (TypeScript LSP) — gated on Q3 (LSP auto-install UX) + `tower-lsp` integration.
- §7 lying-agent and hallucinating-agent mechanical gates — same; pure-function detector code is in place and unit-tested.

### Rig counts
- **21 schemas / 51 artifacts / 112 tests / 11 dry-runs / 172 Rust unit tests** (was 20 / 50 / 112 / 11 / 97).

## v28 — 2026-05-16
**Phase A foundation — five unblocked modules land in `atelier-core`.** Wires up the runtime mechanics that Phase A's mechanical gate hangs off, without taking on the items blocked by external actions (rmcp spike Q7, baseline capture Q5).

- **§2.5 session actor** (`crates/atelier-core/src/session.rs`). Per-session tokio task with `mpsc` inbox, `broadcast` event channel, bounded `Semaphore` (cap 4, PROVISIONAL) for in-turn tool parallelism, and `tokio_util::CancellationToken` for drop-on-cancel. Every transition goes through `Transition::new` (validates against `LEGAL_TRANSITIONS`) and fires `CheckpointHook` + `LedgerHook` before broadcast. Illegal transitions surface as `Event::IllegalTransitionAttempted` rather than panic. Terminal states (`Done`, `Failed`) end the actor.
- **§3 atomic diff staging** (`crates/atelier-core/src/staging.rs`). `Staging::commit` stages every write into a same-filesystem `TempDir`, runs the syntax check + SHA-256 pre-hash conflict check, then lexicographically renames. Any validation failure leaves the workspace untouched. `TreeSitterSyntaxCheck` bundles `tree-sitter-json` and reports `Pass / Fail / NotApplicable / GrammarMissing` per spec §3 (other Tier-1 extensions return `GrammarMissing` until their grammars are bundled). Absolute paths and `..` escapes are rejected at `add` time.
- **§11 sandbox profile generators** (`crates/atelier-core/src/sandbox.rs`). `macos_profile(&SandboxPolicy)` emits a `(deny default)` `sandbox-exec` `.sb` profile; `linux_bwrap_argv` emits the bubblewrap argv with `--unshare-net/-pid/-uts/-ipc/-user-try`, tmpfs `/tmp`, RO bind for `/usr`, `/lib`, `/bin`, `/sbin`, `/etc`, and `--die-with-parent`. Network is denied by default; `with_net()` flips both platforms. Writes to `/etc` and `/usr/local` are rejected at policy-build time per spec §11.
- **§14 crash-recovery scaffold** (`crates/atelier-core/src/persistence.rs`). Typed `OnDiskSession` matching `schemas/session/v1.json`; atomic `save_to` via `tempfile::NamedTempFile::persist`; `load_from` rejects mismatched `harness_session_version` with a typed error. `RecoveryEntry` + `RecoveryReason::{Crash, UserCancel, Timeout, ConcurrentEditPause}` + `append_recovery`. Global `Registry` at `~/.atelier/registry.json` with `touch / forget / save / load` (missing file = empty per spec).
- **§15 hook manifest loader** (`crates/atelier-core/src/hooks.rs`). `HookManifest::from_json` round-trips `schemas/config/hook_manifest.v1.json` and enforces the runtime invariants serde can't (`version == 1`, `name` regex, `time_budget_ms >= 1`, `tool_filter` not set for `on-verify-*`, non-empty command/url). `HookSet::load_dir` + `merge_dir` give per-repo-overrides-global discovery. `HookApprovals` is the first-use approval store with atomic save under `_approvals.json` (`_` prefix keeps it out of the name regex space) and a `partition` helper for the UI prompt.

Workspace deps added: `sha2`, `tree-sitter`, `tree-sitter-json`, `uuid`. `atelier-core` now depends on `tokio`, `tokio-util`, `futures`, `tracing`, `uuid`, `tempfile`, `sha2`, `tree-sitter`, `tree-sitter-json`.

Verified: `cargo test -p atelier-core --lib` → **97 passed** (was 21; +76 across the five new modules); `cargo fmt --check` clean; `cargo clippy -p atelier-core --all-targets -- -D warnings` clean; `make check` end-to-end green (`50/50 artifacts`, `112 passed`, `11/11 dry-runs OK`).

Explicitly **not done this round** — each is tracked in `tasks/todo.md`:
- File-watcher integration (§14) — needs the tool dispatcher's read-set tracking.
- Concurrent-edit modal flow (§14) — UX surface; queues at tool-call boundary.
- Hook execution (§15) — subprocess wrapper lands with the §15 tool dispatcher.
- Diff-blob storage (§4) — bundled with checkpoint store.
- Anthropic / LiteLLM adapters (§1) — Q2 is resolved but the adapters are a multi-session block of their own.
- MCP client (§15) — gated on Q7 rmcp spike.

### Rig counts
- **20 schemas / 50 artifacts / 112 tests / 11 dry-runs / 97 Rust unit tests** (was 21).

## v27 — 2026-05-16
**Onboarding fixes: README CI badge URL + `make install-rig` on Homebrew Python.** Two unrelated friction points hit on a fresh checkout, plus one latent packaging bug surfaced by the second fix.

- **README CI badge URL.** Placeholder `OWNER` in the `github.com/OWNER/atelier/...` badge URL replaced with `ChrisAdkin8`. The accompanying "replace `OWNER` once the repo lives on GitHub" comment is removed. Resolves the placeholder noted in v3 (CHANGELOG line 310, preserved as a historical record).
- **`make install-rig` now uses a project-local venv.** On macOS Homebrew Python (PEP 668 externally-managed), `pip install --user ".[rig]"` is refused. The target now creates `.venv/` (if absent) and installs the rig deps into it. Other Make targets pick up `.venv/bin/python` via a new `VENV_PY` detection in the Makefile and fall back to system `python3` — so CI (which installs deps directly per `.github/workflows/check.yml`) is unaffected. `.venv/` added to `.gitignore`.
- **`pyproject.toml [tool.setuptools] packages = []`.** Latent bug surfaced once the install actually built a wheel: setuptools' auto-discovery picked up sibling dirs (`crates/`, `target/`, `schemas/`, `prompts/`, `experiments/`) as top-level packages and refused to build. The rig has no importable Python module — it's scripts under `tests/` run via `python3 tests/...` — so the correct fix is to declare zero packages explicitly.
- **Docs synced**: `README.md` (install-rig blurb), `CONTRIBUTING.md` (dev-loop comment), `ATELIER.md` (canonical-commands blurb).

Verified: `make install-rig` succeeds on Homebrew Python (`Successfully installed atelier-0.0.0 ... pytest-9.0.3 ...`); `make check` then runs end-to-end against `.venv/bin/python` — `50/50 artifacts validated`, `112 passed in 20.61s`, all 11 task dry-runs `OK`.

### Rig counts
- **20 schemas / 50 artifacts / 112 tests / 11 dry-runs** — unchanged from v26.

## v26 — 2026-05-16
**Toolchain bump: Rust 1.83.0 → 1.85.0.** Triggered by wiring `rmcp = { workspace = true }` into `atelier-core`; the transitive `rmcp-macros 0.1.5` requires Cargo's `edition2024` feature, which only stabilized in Rust 1.85.0. Without the bump, `cargo check -p atelier-core` fails with *"feature `edition2024` is required"*.

- **`rust-toolchain.toml`** channel → `1.85.0`.
- **Root `Cargo.toml`** `rust-version` → `1.85`.
- **`.github/workflows/check.yml`** `dtolnay/rust-toolchain@v1` toolchain input → `1.85.0`.
- **Docs synced**: `ATELIER.md`, `README.md`, `tasks/todo.md`, spec §211. Historical 1.83.0 references in earlier CHANGELOG entries are preserved as factual at-the-time records.
- **Drive-by**: `crates/atelier-gui/src/main.rs` reformatted by the 1.85 rustfmt (default function-call wrapping shifted).

Verified: `cargo check -p atelier-core` resolves `rmcp v0.1.5` clean; `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test -p atelier-core` (4 passed) all green.

### Rig counts
- **20 schemas / 50 artifacts / 112 tests / 11 dry-runs** — unchanged from v25.

## v25 — 2026-05-16
**Hook polish.** Two one-line cleanups to `bounded-reads.sh` flagged by the round-seven re-scan.

- **N44.** Silenced `jq`'s parse-error stderr on malformed-JSON payloads. The hook stays non-blocking per spec §15, but no longer logs `jq: parse error: Invalid numeric literal...` on every glitch payload. Added `2>/dev/null` to the first jq call and an early-exit when `tool_name` is empty or `null`.
- **N47.** Stripped `wc -l`'s left-padding from the nudge message. Before: `"Read on      889-line file without limit..."`. After: `"Read on 889-line file without limit..."`.

Verified end-to-end: malformed payload → quiet exit 0; empty stdin → quiet exit 0; legit unbounded Read still nudges (with clean formatting); Read with `limit` is silent; Grep `content` without `head_limit` still nudges.

### Rig counts
- **20 schemas / 50 artifacts / 112 tests / 11 dry-runs** — unchanged from v24.

## v24 — 2026-05-16
**Removal hygiene + audit-debt visibility.** Five follow-ups from round-six audit, plus the carry-over list promoted to a discoverable home.

### Removal hygiene — stale references swept (B21–B25)
When v21 removed `delete_file.v1.json` and v23 untracked `.atelier/settings.local.json`, several descriptions/examples/tests still pointed at them. Each fixed:
- `crates/atelier-core/tools/shell.v1.json` description: "use `write_file`/`delete_file`" → "use `write_file` or `edit_file`" (the actual spec-§15 surgical-edit tool, added in v21).
- `schemas/config/_implementation.v1.json` `builtin` description: hardcoded list of built-in tool names → pointer to spec §15 L722 (the canonical list, no future drift).
- `examples/config/permissions.v1.json`: always-deny `delete_file` example → `write_file` with the same path-pinning rationale.
- `schemas/config/permission_shapes.v1.json` examples block: `bash`/`delete_file` → `shell`/`edit_file` (real tool names from the current registry).
- `tests/test_schemas.py::test_permission_state_exact_match_shape_valid`: same swap.
- `.atelier/README.md`: directory tree no longer lists `settings.local.json` or `bin/`; symlink table is two rows, not three; settings.local.json explained as per-user gitignored state.
- `.atelier/memory/feedback_config_scope.md`: "watch for an existing settings.local.json" → "settings.local.json is per-user state managed by the host harness and gitignored."

### Doc-drift guard (Br13)
- **New test `tests/test_runner.py::test_tool_name_mentions_resolve`** — scans every bundled built-in tool manifest's `description` strings for backticked identifiers matching `*_file` / `*_dir` (the regression shape) and asserts each resolves to an actual manifest. Verified: passes clean; rejects an injected `\`frobnicate_file\`` reference; passes again after revert. Intentionally narrow — catches the original B22-class bug without false-positiving on JSON-Schema property names like `old_text`, `subagent_type`.

### Host-harness contract documented (N41)
- **New `.atelier/docs/host-harness-contract.md`** — spells out the six things a BYOM host must provide for the hooks to fire correctly: `cwd=project_root`, JSON-on-stdin, `additionalContext`-on-stdout, advisory exit codes, no required env vars, suggested time budget. Plus a 2-command smoke test a new host integrator can run to verify. Removes the "every BYOM-compatible host honors X" handwave from `.atelier/settings.json`'s comment.

### Hook script consistency (N40)
- `bounded-reads.sh` switched from `set -euo pipefail` to `set -uo pipefail` to match the other two hooks. All three now use the same discipline (no `-e`; inline `|| exit 0` for fall-through), with a comment explaining why (spec §15: hooks must never block the turn).

### Audit-debt visibility (N43)
- **`tasks/todo.md` gains a "Known smells, not blocking" section** with the ~22 carry-overs that have survived six audit rounds. Triage stance: fix opportunistically, not urgent. The build tracker is now the single source of truth for what's known-but-deferred, so future audits can re-flag selectively instead of restating the entire list.

### Rig counts
- 20 → **20 schemas** unchanged.
- **50 artifacts** unchanged.
- 111 → **112 rig tests** (+1 `test_tool_name_mentions_resolve`).

## v23 — 2026-05-16
**BYOM env-var pass + buildable rig + paranoid CI pins.** Seven follow-ups from the round-five audit, no spec changes.

### `$CLAUDE_PROJECT_DIR` removed from tracked source
The hooks previously referenced `$CLAUDE_PROJECT_DIR` — set by the host harness (Claude Code), not by Atelier. That's a vendor-coupling the BYOM directive doesn't allow. Replacement strategy:
- **Hook scripts** (`bounded-reads.sh`, `save-nudge.sh`, `session-start-memcheck.sh`) now derive `ATELIER_PROJECT_DIR` from `${BASH_SOURCE[0]}` at the top, so they work regardless of host harness or clone location.
- **`.atelier/settings.json`** hook commands switched to project-root-relative paths (`.atelier/hooks/...`). The host harness runs hook commands with `cwd=project root`, so no env var is needed at the config layer.
- `session-start-memcheck.sh` also had a hardcoded `$HOME/Projects/atelier/...` path (B13); that's gone too — the same `ATELIER_PROJECT_DIR` derivation handles it.

Net effect: `grep -r 'CLAUDE\|\\.claude' .atelier/hooks/ .atelier/settings.json` returns nothing. The BYOM lint guards against regression.

### Other follow-ups
- **B19 — `pyproject.toml` `[build-system]`** added (setuptools backend). `pip install ".[rig]"` (used by CI and `make install-rig`) needs a PEP 517 backend to be declared; the install worked on lenient pip versions but was one release away from breaking.
- **N33 — `.atelier/settings.local.json` gitignored.** Per-user permission allowlists for the host harness regenerate locally; the file no longer ships. Dropped from the BYOM lint allowlist accordingly.
- **N34 — README CHANGELOG range** updated from "v1 → v13" to a generic "spec + rig revisions" (the range was nine versions stale).
- **B20 — BYOM lint docstring** rewritten to match the code's exact-match allowlist, with each allowed entry annotated inline. No more "glob suggested, but exact-match enforced" mismatch.
- **B12 / N39 — empty `.atelier/bin/`** removed. Tools (`memcheck.sh`, `mempromote.py`, `memrecall.py`) live in `~/.atelier/bin/` per `.atelier/docs/memory-system.md`; no in-repo landing zone was actually needed.
- **Br12 — `dtolnay/rust-toolchain@v1`** pin replaces `@stable`. The `@stable` ref tracks the action's default branch; `@v1` is the semver pin the maintainer ships for reproducibility.

### Quiet hardening of the hooks
While rewriting the hooks for the BYOM pass, three extra hardenings:
- `command -v jq >/dev/null || exit 0` at the top of `bounded-reads.sh` and `save-nudge.sh` — quietly no-op on systems without `jq` instead of failing loudly with a hook-error log line.
- `bounded-reads.sh` line-counts only known-text extensions (`*.md`/`*.py`/`*.rs`/…), so a `Read` on a binary doesn't `wc -l` garbage.
- `bounded-reads.sh` uses `wc -l` instead of `awk 'END{print NR}'` — same result, smaller surface.

### Rig counts
- **20 schemas** unchanged.
- 50 → **50 artifacts** (settings.local.json untrack is JSON but it lived under `.atelier/`, not under any `JSON_RULES` glob — net zero).
- **111 rig tests** unchanged.

## v22 — 2026-05-16
**Directive lock-in: Atelier uses `.atelier/`, never `.claude/`.** No spec changes; this is enforcement of a project policy the user surfaced explicitly ("ensure that .atelier is always used instead of .claude").

### Why this is a directive, not a preference
Atelier is a BYOM (bring-your-own-model) harness. Hardcoding another vendor's directory name into tracked source quietly couples the repo to one host harness. The "Why Claude appeared in the code" table from v21 walked through each kind of reference and graded each one; this PR adds an automated guard so the policy doesn't regress.

### What's new
- **`tests/test_runner.py::test_no_claude_paths_in_tracked_source`** — lint that walks every tracked text file, skipping symlinks (which are the documented harness-shim exception: `.claude/settings.json` → `../.atelier/settings.json`; `CLAUDE.md` → `ATELIER.md`), and rejects any `.claude` or `.claudeignore` substring outside a tight allowlist. The allowlist is: `.gitignore`, `CHANGELOG.md`, `ATELIER.md`, `.atelier/README.md`, `.atelier/docs/memory-system.md`, `.atelier/memory/feedback_*.md`, `.atelier/memory/MEMORY.md`, `.atelier/settings.local.json`, `coding-harness-spec.md`, `tasks/todo.md`, and the test file itself. Each entry has a documented rationale in the test's docstring. Verified: the lint catches a fresh `.claude/foo` injection into `schemas/README.md`.
- **Project memory `.atelier/memory/feedback_atelier_path_directive.md`** — durable directive: "In atelier specifically, all project-scoped config goes under `.atelier/`. New `.claude/` paths are forbidden in tracked source." Indexed from `MEMORY.md` so future sessions pick it up.

### What is and is not a violation
*Violations* (lint-rejected): tracked source files outside the allowlist containing `.claude/`, `.claudeignore`, or `claude_code_version`-style field names. Build artefacts, symlinks pointing into `.atelier/`, and the documented historical-record files are exempt.

*Not violations*: example data using `anthropic:claude-sonnet-4-6` model strings (these are *vendor:model identifiers* in a multi-vendor BYOM list, not paths or schema fields). The routing schema's description lists six providers including `anthropic`; examples picking one for concreteness is a documentation choice, not a structural commitment.

### Rig counts
- **20 schemas** unchanged.
- **50 artifacts** unchanged.
- 110 → **111 rig tests** (+1 `test_no_claude_paths_in_tracked_source`).

## v21 — 2026-05-16
**Third audit follow-up + BYOM vendor-neutrality pass.** Seven ranked items from the v20 audit plus a sweep of Claude-specific references that crept into the schema layer. No spec changes (but several drifts *against* the spec are corrected).

### Spec-alignment fixes (drifts I introduced in v20)
- **`spawn_subagent.v1.json`** now matches spec §10.1:
  - `side_effect_class: local-risky` (was `shared-state`).
  - `subagent_type` is *optional* (defaults to `general-purpose` per spec §10.1 L515).
  - Cancellation shape (`{subagent_id, cancel: true}`) is now expressible via `input_schema.oneOf {spawn | cancel}`, including `not` constraints that reject mixed shapes.
- **Built-in tool inventory matches spec §15 L722.** Added `edit_file.v1.json` (surgical text-replace tool, atomic, fails if `old_text` is not unique unless `expected_count` is set). Removed `delete_file.v1.json` (not in spec). Final inventory: `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `ast_grep`, `shell`, `spawn_subagent`.
- **`with_delegation.json`** `tool_fixtures.tc-1.args` now includes `prompt`, conforming to `spawn_subagent.v1.json`'s input_schema. Previously the args differed between the conversation entry and the tool_fixtures entry — replay would have lost the prompt.

### Cleanup of my own redundancies
- **t08 conftest.py removed.** The fixture's `test_transfer.py` already isolates state via `setup_function`; the conftest I added in v20 was belt-and-braces. Two layers doing the same job is worse than one — dropped the conftest.
- **`examples/tools/grep.v1.json` removed.** It defined `name: "grep"`, colliding with the built-in `crates/atelier-core/tools/grep.v1.json` shipped in v20. `examples/tools/` now contains only `web_fetch.v1.json` (a `shared-state` http example) as the demo of how to register a *custom* tool. The README is updated to point at `crates/atelier-core/tools/` for built-ins.

### CI tightening
- **CI installs from `pyproject.toml [project.optional-dependencies] rig`** via `pip install ".[rig]"`. The hand-written dep list in `check.yml` is gone — `pyproject.toml` is now the single source of truth (Makefile's `install-rig` target follows suit). Bumping a rig dep no longer needs three files updated.
- **`dtolnay/rust-toolchain@stable` + `toolchain: "1.83.0"`** input replaces `@1.83.0` ref-tag form. The action's version-shaped tags are best-effort; `@stable` is always tagged. Functionally identical but avoids a CI failure if the tag ever moves.

### BYOM vendor-neutrality (the "why is Claude in the code?" question)
The repo is a bring-your-own-model harness, but a `claude_code_version` field was hardcoded into the baselines schema — a structural commitment to one specific competitor. That's now removed:
- **`schemas/baselines/permission_prompts.v1.json`** field rename: `claude_code_version` → `baseline_harness_name` + `baseline_harness_version`. The schema is now vendor-neutral (any harness with a measurable prompt count can use these slots). The §8 calibration spec still names Claude Code as the v0.1 reference baseline, but that's a *choice* the data records, not a structural commitment of the format.
- **`compare_baselines.py`** updated to use the new field names; header line now shows whatever `baseline_harness_name` the file records (`claude-code`, `aider`, `cursor-agent`, `atelier`, etc.).
- **New test `test_baseline_byom_neutral`** runs three concrete vendor combinations (`aider+openai`, `cursor-agent+ollama`, `atelier+anthropic`) through the schema to lock in the multi-vendor contract.
- **`.gitignore` now excludes `.claude/`, `.cursor/`, `.aider/`, `.copilot/`.** Two committed files (`.claude/settings.json`, `.claude/settings.local.json`) were per-user Claude Code config that leaked into the repo. Removed and gitignored alongside other agent-harnesses' equivalents.
- **`grep.v1.json` description** previously referenced `.claudeignore` as an excluded-paths source. Updated to `.atelierignore` (with `.gitignore` as fallback) — the built-in shouldn't advertise another harness's config file.

What's intentionally left alone: example artifacts (`tests/sessions/examples/*.json`, `examples/config/routing.v1.json`, `examples/subagents/code-reviewer.v1.json`) that use `anthropic:claude-sonnet-4-6` as illustrative model strings. These are *examples* of model strings, not structural commitments — the BYOM contract says any provider-prefixed string is valid (`schemas/config/routing.v1.json` lists `anthropic`, `openai`, `litellm`, `ollama`, `mlx`, `llamacpp` in the description). Examples picking one vendor is a documentation choice, not a hardcoded dependency.

### Rig counts
- **20 schemas** unchanged.
- 51 → **50 artifacts** (+1 `edit_file.v1.json`, −1 `delete_file.v1.json`, −1 `examples/tools/grep.v1.json`; net −1).
- 109 → **110 rig tests** (+1 `test_baseline_byom_neutral`).

## v20 — 2026-05-16
**Second audit follow-up.** Six high-impact fixes from the post-v19 deep audit. No spec changes.

### Self-inflicted regression undone
- **`hook_manifest.v1.json`** — implementation `oneOf` inlined again instead of `$ref`'ing `config/_implementation.v1.json`. The shared schema carried a `timeout_ms` field intended for tools only; the v19 refactor accidentally let hooks set it, contradicting §15's "hooks never block, they only warn" contract (`time_budget_ms`). New regression test `test_hook_manifest_rejects_impl_timeout_ms` locks the contract.

### Schema coverage gaps closed
- **`crates/atelier-core/tools/spawn_subagent.v1.json`** — first authoritative schema for the `spawn_subagent` built-in tool. `input_schema` requires `{subagent_type, description, prompt}` with optional `max_turns` / `tool_allowlist` overrides. `output_schema` describes `{subagent_id, result, status, turns_used, cost?}`. `with_delegation.json` was the only prior source; that's now a conformance example, not the contract.
- **`config/_implementation.v1.json`** gained a `builtin` kind (third `oneOf` branch). Built-in tools that route to an internal handler now have a way to declare themselves; no `command` / `url` required. `tool_manifest.v1.json` `$ref`'s the shared schema and so picks this up automatically. Two new tests: `test_tool_manifest_builtin_kind_valid` and `test_tool_manifest_builtin_rejects_extra_fields`.
- **`schemas/session/v1.json`** — `cost_ledger.tool_call` entries now require `tool_name` in addition to `latency_ms`. Replay can now link a ledger entry to its `tool_fixtures` row programmatically instead of regex-parsing the free-form `note`. All four example sessions updated. New test `test_cost_ledger_tool_call_missing_tool_name_rejected`.

### Built-in tool manifests shipped
- Eight new manifests under `crates/atelier-core/tools/`: `read_file`, `write_file`, `delete_file`, `list_dir`, `grep`, `ast_grep`, `shell`, `spawn_subagent`. Each declares its `input_schema`, `output_schema`, `side_effect_class`, and `implementation: {kind: builtin}`. These resolve the dangling references in `crates/atelier-core/subagents/*.json` `tool_allowlist` (researcher cites `read_file`, `list_dir`, `grep`, `ast_grep`; test-runner cites `read_file`, `list_dir`, `grep`, `shell`) and in `examples/subagents/code-reviewer.v1.json`. `validate_artifacts.py` picks up the new directory via a new rule.

### Test-isolation footgun closed
- **`t08_add_input_validation/fixture/tests/conftest.py`** added. Snapshots and restores the module-level `transfer.ACCOUNTS` dict around every test via an autouse fixture. Confirmed: a test that mutates `ACCOUNTS["alice"]` does not leak the change to later tests. The agent's job is validation, not state-isolation plumbing.

### Dependency + CI tightening
- **`pyproject.toml`** and **`Makefile`** now declare `referencing>=0.35` explicitly (the rig's `_schema_helpers.py` imports it directly; previously it landed only as a transitive dep of `jsonschema>=4.18`).
- **`.github/workflows/check.yml`** rust job: explicit `dtolnay/rust-toolchain@1.83.0` step with `components: rustfmt, clippy` so the install happens deterministically before any cargo step. `actions/cache` key now includes `rust-toolchain.toml` so a channel bump invalidates the cache (previously only `Cargo.toml` was hashed; a toolchain bump silently reused stale `target/` artefacts).

### Rig counts
- **20 schemas** unchanged (no new schema files added; `_implementation.v1.json` grew a `builtin` branch in-place).
- 43 → **51 artifacts** (+8 built-in tool manifests under `crates/atelier-core/tools/`).
- 105 → **109 rig tests** (+4: hook timeout regression lock, tool_manifest builtin kind valid, tool_manifest builtin rejects extras, cost_ledger tool_name required).

## v19 — 2026-05-16
**Audit follow-up.** Six bug/smell/brittleness fixes from the deep audit, no spec changes.

### Bugs fixed
- **t03 `checks.json`** — `open('fixture/config.json')` → `open('config.json')`. The runner copies fixture *contents* flat into the workdir, so the prefixed path produced a spurious `FileNotFoundError` on every harness run. Latent because CI only exercises `--dry-run`. Reproduced in a fresh fixture copy before/after the fix.
- **t07 `checks.json`** callable count — replaced `grep -cE '^def …'` with an `ast.walk` count of `FunctionDef`/`AsyncFunctionDef`. The original rejected valid class-based refactors (4 methods + 1 shim → 1 top-level `def`) and rewarded dummy top-level stubs.
- **runner `run_test_command`** now takes a `timeout_s` (default 120 s); on `TimeoutExpired` returns `returncode=-1`, `timed_out=True`. `schemas/workload/runner_result.v1.json` `pytest_result` $def extended with `timed_out: boolean` and tightened to `additionalProperties: false`.

### Smells addressed
- **`.pytest_cache/` and `__pycache__/`** under `tests/workload/canonical/*/fixture/` removed (10 + 18 dirs). Gitignore patterns already matched but the dirs had been tracked.
- **`version: const 1`** is now a required top-level field on `task_meta`, `baselines/permission_prompts`, `audit/egress`, `telemetry/payload`, and `protocol/overhead`. All 11 `meta.json` artifacts updated to include `"version": 1`. `runner_result` keeps its descriptive `runner_version` name.
- **`session/v1.json` turn shape** extracted to `$defs/turn`; both `conversation` and `subagents.*.conversation` `$ref` it. ~25 lines of duplication removed.
- **`config/_implementation.v1.json`** introduced — shared shell/http `oneOf`. `tool_manifest.v1.json` and `hook_manifest.v1.json` now `$ref` it. Cross-file `$ref` resolves via the existing schema registry; affected test_schemas tests switched to `validate_with_registry`.

### Brittleness addressed
- **Rust now exercised in CI.** New `rust` job (matrix on ubuntu + macos) runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test -p atelier-core`. Toolchain pinned via `rust-toolchain.toml` (1.83.0).
- **Harness smoke + checks lint added.** Two new pytest tests in `test_runner.py`: (a) `test_checks_commands_do_not_reference_fixture_prefix` lints all `checks.json` for the original t03 bug shape; (b) `test_runner_harness_smoke_all_tasks_emit_checks` runs the runner with `--harness-cmd true` against every canonical task and asserts each task ran at least one check with a kind.

### Rig counts
- 19 → **20 schemas** (added `config/_implementation.v1.json`).
- 102 → **105 rig tests** (added 3: meta version-required, checks-fixture-prefix lint, harness-smoke).
- 43 artifacts (unchanged; all 11 `meta.json` now carry `version: 1`).

## v18 — 2026-05-16
**Sub-agent delegation** added as a spec + schema contract. Implementation lands in Phase D/E; the contract is locked now so Phase A can scaffold against it.

### Spec §10 expansion
- §10 split into three modes:
  - **§10.1 Delegation mode (Phase D/E)** — the new headline. Parent invokes `spawn_subagent` (built-in tool); harness materialises a fresh §2.5 state machine with isolated context, optional tool allowlist, optional side-effect cap, optional routing override; sub-agent runs to completion and returns a single tool-result message. Full contract: tool input/output shape, sub-agent type system, session-state representation, interactions with §4/§7/§8/§11/§3, cancellation semantics (cascading), recursion depth cap (3, PROVISIONAL).
  - **§10.2 Comparison mode (Phase F)** — kept (same task, different routings, side-by-side).
  - **§10.3 Background critic (Phase F)** — kept.

### New schema
- **`schemas/config/subagent_type.v1.json`** — sub-agent type manifest. Required: `version`, `name`, `description`, `system_prompt_addendum`. Optional: `tool_allowlist`, `default_max_turns`, `model_routing` (via `$ref` into `routing.v1.json` — exercises the schema registry cross-reference), `side_effect_class_cap`.

### Updated schema
- **`schemas/session/v1.json`** — added optional `subagents` field. Map keyed by `subagent_id` containing per-sub-agent `parent_turn_id`, `subagent_type`, `started_at`/`finished_at`, `status` (running/completed/failed/timed_out/cancelled), `max_turns`/`turns_used`, `tool_allowlist`, full `conversation` array (with envelope `$ref`), `result` text, `cost_summary`. Existing example sessions still validate (field is optional).

### Bundled + example
- **`crates/atelier-core/subagents/researcher.json`** — read-only research sub-agent (`local-safe` cap; tool allowlist: read_file/list_dir/grep/ast_grep; 25-turn default).
- **`crates/atelier-core/subagents/test-runner.json`** — runs project tests; read + shell only; `local-risky` cap; 10-turn default.
- **`crates/atelier-core/subagents/general-purpose.json`** — catch-all; inherits parent's tool set; 30-turn default; no cap.
- **`examples/subagents/code-reviewer.v1.json`** — independent reviewer with Opus routing override + `local-safe` cap; exercises the cross-schema `$ref` to routing in practice.

### New example session
- **`tests/sessions/examples/with_delegation.json`** — full round-trip: parent invokes `spawn_subagent(researcher, ...)`, the tool-fixture captures the result, the `subagents` map records the sub-agent's complete conversation with envelope and cost summary. Locks the schema's delegation flow end-to-end.

### Rig upgrades
- `validate_artifacts.py` gains rules for `examples/subagents/*.json` and `crates/atelier-core/subagents/*.json` against the new schema.
- `test_schemas.py` gains **11 new tests** — 7 for subagent_type (minimal/full-with-routing-$ref/bad-name/missing-addendum/bad-side-effect-cap/zero-max-turns/bad-nested-routing), 4 for session.subagents (with/missing-required/bad-status/optional-when-absent).

### Final tallies
- **19 schemas / 43 artifacts / 102 rig self-tests / 11 dry-runs** — all passing.

### Documentation sweep
- Spec §10 — rewritten and expanded.
- `schemas/README.md` — row for `subagent_type.v1.json`.
- `examples/README.md` — layout + current-example entries.
- `tests/README.md` — 102-test count + new schemas/$ref listed.
- `README.md` — tally line, layout tree (adds `examples/subagents/`, `crates/atelier-core/subagents/`).
- `tasks/todo.md` — status block updated; sub-agent delegation listed as contract-locked, implementation-deferred.

## v17 — 2026-05-16
Four small consistency gaps closed; MCP catalog doubled (4 → 8 servers).

### Spec additions
- **§14 Diff blob format** — new subsection. Unified diff (`diff -u`) as the on-disk format for `<sha256>.diff` blobs. Large files (>1 MB, PROVISIONAL) bypass diff encoding and store as `<sha256>.full`. Binary files (detected by NUL byte in first 8 KB) always use `.full`. Blobs over 4 KB are zstd-compressed (`.zst`). Reconstruction by walking parent → child applying each `diff_ref`. Locks the contract Phase D §4 needs.
- **§14 Headless exit codes** — new table enumerating `--non-interactive` exit codes: 0 success, 1 verification gate failed, 2 ContextOverflowError fall-through, 3 concurrent-edit modal timeout, 4 sandbox violation, 5 model adapter unavailable, 6 envelope schema violation exhausted, 7 permission denied; 64–78 reserved for sysexits(3); 100+ tool-specific propagation. Forward-compatible — future versions add only.
- **§15 `/help` output format** — specifies the per-skill line format (`/<name>  <description>  [proactive]  <source>`), sort order (bundled → global → per-repo, alphabetical within group), override behavior (winners shown, suppressed dupes hidden), and the trailing CLI-verb summary line.

### CONTRIBUTING addition
- **Filename conventions** subsection — documents the `.v1.json` (examples) vs `.json` (bundled, runtime-overrideable) split. Reasoning: bundled artifacts carry the schema version in the *directory* (a v2 lives at `crates/atelier-core/skills_v2/`), letting short names like `/review` map cleanly to `skills/review.json`. Examples mirror schema filenames for human readability.

### MCP catalog expansion
Bundled MCP catalog grew from 4 → 8 servers. Added:
- **`memory`** — knowledge graph persistence across sessions (`local-risky`).
- **`github`** — GitHub issues/PRs/repos via PAT (`shared-state`).
- **`postgres`** — PostgreSQL query/update via connection string (`shared-state`); recommended read-only-by-default deployment.
- **`puppeteer`** — headless browser automation (`shared-state`); JavaScript-rendered web content.

All four match the existing catalog schema (`schemas/config/mcp_catalog.v1.json`); the validator already covers them.

### Rig
- No new schemas — additions ride existing validation rules.
- `make check` confirms: **18 schemas / 38 artifacts / 91 rig self-tests / 11 dry-runs** still all passing.

### Documentation sweep
- `tasks/todo.md` — bundled-catalog line updated to list all 8 servers.
- `CONTRIBUTING.md` — Filename conventions subsection.
- No other doc count changes (artifact / schema / test tallies unchanged in v17).

## v16 — 2026-05-16
OSS hygiene + MCP catalog + fork-tree example session + **Skills system**.

### Hygiene (items 1–4)
- **`SECURITY.md`** — vulnerability disclosure policy with SLOs (acknowledge ≤3 business days, initial assessment ≤10, public disclosure ≤90), in/out-of-scope rules, hardening expectations.
- **`CODE_OF_CONDUCT.md`** — Contributor Covenant 2.1, adapted.
- **`CONTRIBUTING.md`** — dev loop, conventions, PROVISIONAL discipline, PR process, license note.
- **`.github/PULL_REQUEST_TEMPLATE.md`** — structured PR template: what / where it lands / why / verification / tallies / risks / checklist.

### MCP catalog (item 5)
- **`schemas/config/mcp_catalog.v1.json`** — schema for the GUI's "Browse catalog". `oneOf` discriminates install kinds (`npm` / `binary` / `http`), optional `requires_secrets` list with `where: header | env`.
- **`crates/atelier-core/catalog/mcp_servers.json`** — bundled curated list: filesystem, git, sqlite, fetch (canonical first-party MCP servers).

### Fork-tree + recovery example session (items 6 + 7)
- **`tests/sessions/examples/with_fork_and_recovery.json`** — exercises checkpoint tree with a fork (ck-2 → main, ck-2a → alternative), `fork_label` field, a `cache_bust` ledger entry for the fork, a populated `recovery_log` entry from a hypothetical SIGKILL mid-class-implementation. Locks both schema features in one example.

### Skills system (new harness capability)
- **`schemas/config/skill_manifest.v1.json`** — schema. Required: `version`, `name`, `description`, `prompt_template`. Optional: `args` (with `required` + `default`), `pinned_context`, `tools_required`, `proactive_trigger`, `side_effect_class`.
- **Bundled skills** at `crates/atelier-core/skills/`:
  - **`/review`** — diff review (regressions / coverage / security / convention violations).
  - **`/security-review`** — security audit with `proactive_trigger` so the model suggests it when auth/credential/secret code changes.
  - **`/test`** — runs the project's test command from ATELIER.md's "Useful commands"; falls back to language defaults.
- **`/help` and `/init`** documented as harness-intercepted CLI verbs, not skill manifests — they don't reach the model.
- **Example skill** `examples/skills/explain.v1.json` exercises args (`${target}`, `${detail_level}` with default), `pinned_context`.
- **Spec §15 new subsection** describes invocation (manual `/<name>` vs proactive via `proactive_trigger`), storage layers (`~/.atelier/skills/` → `<repo>/.atelier/skills/` → bundled), substitution variables (`${arg}`, `${repo_root}`, `${atelier_md}`), and cost-ledger tracking (skill recorded as a `note` on the expanded turn's `model_call` entry).

### Rig upgrades
- `validate_artifacts.py` gains rules for `examples/skills/*.json`, `crates/atelier-core/skills/*.json`, and `crates/atelier-core/catalog/mcp_servers.json`.
- `test_schemas.py` gains **11 new tests** — 6 for skill_manifest (minimal/full/bad name/missing template/bad side-effect/bad arg name), 5 for mcp_catalog (minimal/http/npm-without-package/install-kind-mismatch/requires_secrets shape).
- New tallies: **18 schemas, 38 artifacts, 91 rig self-tests**, all passing.

### Documentation sweep
- `README.md` — tally line + layout tree updated (adds `examples/skills/`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`, `.github/PULL_REQUEST_TEMPLATE.md`, the bundled `catalog/`, `skills/`, `templates/` under `crates/atelier-core/`).
- `schemas/README.md` — rows for `skill_manifest.v1.json` and `mcp_catalog.v1.json`.
- `examples/README.md` — skill manifest row + current-example entry.
- `tests/README.md` — 91-test count + new schemas listed.
- `tasks/todo.md` — status block updated to v16 tallies.
- Spec §15 — Skills subsection inserted between Hooks and Providers.

## v15 — 2026-05-16
Decisions spec'd for the four "decided in prose, unspecified" gaps; UX and hygiene gaps closed.

### Spec decisions
- **§3 Tree-sitter grammar list.** Tier 1 (bundled in v1): Python, TypeScript/TSX, JavaScript/JSX, Rust, Go, JSON, TOML, YAML — explicit `.ext` → grammar-crate mapping. Tier 2 deferred to v0.2 (Java, C#, Ruby, C/C++, shell, markdown, HTML, CSS). Files with no matching grammar skip the syntax check; the atomic-application step still runs the conflict check + on-disk move. UI annotation `syntax-check: pass | fail | not-applicable | grammar-missing`. Bundle-size budget: ~3–5 MB, revisit if >10 MB.
- **§2.5 Streaming UI semantics.** Three named states: during-turn (`pending` envelope panels alongside streaming text + tool cards), turn-end-valid (envelope populates downstream panels), turn-end-invalid (warning bar + automatic re-prompt loop visible). Envelope is never rendered token-by-token; users never see a half-parsed `claimed_changes` array.
- **§1 `ContextOverflowError` UX.** Modal with three named options: Compact (default; runs §5 compaction, retries automatically), Reroute (switch to larger-window model from routing config), Cancel turn. Headless mode defaults to Compact → fall-through to Cancel-turn on persistent failure. Overflow events recorded as `cache_bust` ledger entries.
- **§15 MCP server discovery.** GUI's Servers panel: list with status badges, "Add server" form (transport-conditional, mirrors the schema `oneOf`), "Browse catalog" of curated MCP servers bundled at `crates/atelier-core/catalog/mcp_servers.json`. TUI keeps JSON-edit ergonomics. Remote catalog auto-fetch deferred to v0.2.

### Hygiene + project polish
- **`LICENSE`** — Apache 2.0 committed at repo root; workspace `Cargo.toml` `license = "Apache-2.0"` (was `"TBD"`). Includes patent grant (relevant for a tools project anyone might adopt commercially).
- **`.github/ISSUE_TEMPLATE/`** — `bug_report.yml` (structured form: what-happened / expected / repro / version / surface / environment / output), `feature_request.yml` (problem / proposal / alternatives / scope dropdown / priority hint), `config.yml` (disables blank issues, links to Discussions for spec/design talk).
- **CI badge** in README — links to `.github/workflows/check.yml` runs; license badge added alongside. Placeholder `OWNER` in the URL until the repo lives on GitHub.
- **README** — removed `LICENSE absent` from "intentionally absent"; added "License" + "Contributing" sections; layout tree adds `LICENSE` and `.github/ISSUE_TEMPLATE/`.

### No rig changes
v15 is purely spec + docs + project polish. The rig still reports **16 schemas / 32 artifacts / 80 rig self-tests / 11 dry-runs** — `make check` re-verified all green.

## v14 — 2026-05-16
Schema completeness pass + project-level config file (ATELIER.md).

### New schemas
- **`schemas/config/routing.v1.json`** — per-task model routing for the §1 planner/executor/critic roles. `<provider>:<model>` strings with a documented pattern that admits Ollama-style `name:tag` model IDs. Example at `examples/config/routing.v1.json`.
- **`schemas/config/permission_state.v1.json`** — persistent permission-learning state. `always_allow` / `always_deny` arrays of shape entries; three shape kinds (`argv0-and-flagset`, `path-glob`, `exact-match`) matching `schemas/config/permission_shapes.v1.json`. Per-repo `.atelier/permissions.json` overrides global `~/.atelier/permissions.json`. Example at `examples/config/permissions.v1.json`.

### Tightened existing schema
- **`schemas/session/v1.json`** — `cost_ledger` entries now enforce per-kind required fields via `allOf`/`if`/`then`:
  - `kind: "model_call"` requires `model_id`, `prompt_tokens`, `completion_tokens`, `count_source`.
  - `kind: "cache_bust"` requires `note`.
  - `kind: "tool_call"` requires `latency_ms`.

  The committed example sessions already conformed; no fixture updates needed.

### Project config — ATELIER.md
- **Seed template** at `crates/atelier-core/templates/ATELIER.md`. Markdown with `<!-- HTML comments -->` for the human reader (stripped before injection into the system prompt). Five suggested sections: project description, conventions, don't-touch, useful commands, free-form.
- **Harness contract — `atelier init`** specified in spec §11. Idempotent project bootstrap: creates `<repo>/.atelier/{sessions,tools,hooks}/`, writes `ATELIER.md` from the seed if absent (never overwrites), appends `.atelier/` to existing `.gitignore`. CLI command implemented as part of Phase A.
- **Spec §5 subsection** describes ATELIER.md as a §5 (visible context) artifact loaded at session start and injected into the system prompt as persistent context.

### Rig upgrades
- `validate_artifacts.py` gains two new rules (`examples/config/routing.v1.json` and `examples/config/permissions.v1.json`).
- `test_schemas.py` gains **18 new regression tests** — 6 for routing config (valid minimal/full, null roles, required executor, bad pattern, capitalised provider rejected), 6 for permission state (each shape kind valid; unknown shape kind rejected; bad scope rejected), 6 for the per-kind cost-ledger required fields (each kind's positive + negative cases).
- New tallies: **16 schemas, 32 artifacts, 80 rig self-tests**, all passing.

### Documentation sweep
- `README.md` — tally line updated; layout tree adds `examples/config/`; new "Project bootstrap" section documenting `atelier init` and ATELIER.md.
- `tests/README.md` — table reflects 80 tests + new schemas mentioned.
- `schemas/README.md` — rows for `routing.v1.json` and `permission_state.v1.json` added.
- `examples/README.md` — layout table + current-examples table extended.
- `tasks/todo.md` — status block updated to v14 tallies.
- Spec — §1 (routing), §5 (ATELIER.md + project bootstrap), §8 (persistent permission state), §11 (atelier init).

## v13 — 2026-05-16
Three Phase A blockers closed; full documentation sweep.

### Phase A blockers — closed
- **Reference machine spec** (`tests/perf/reference.md`) populated against this laptop: MacBook Pro (`MacBookPro18,1`), Apple M1 Pro (10 cores, 8P + 2E), 32 GB RAM, 926 GB SSD, macOS 26.4.1 (build `25E253`), Python 3.14.4, Node v25.8.2. Performance budgets in the spec are now anchored.
- **Session storage on-disk layout** decided and written into spec §14: hybrid per-repo `.atelier/sessions/<uuid>/` (session JSON + content-addressed diff blobs) + global `~/.atelier/registry.json` index. Also resolves what Phase D §4's `diff_ref` strings point at, ahead of schedule.
- **Tool manifest + hook manifest schemas** added:
  - `schemas/config/tool_manifest.v1.json` — registers custom tools with shell or http implementation, side-effect class, input/output JSONSchemas, `${env:…}` / `${keychain:…}` interpolation.
  - `schemas/config/hook_manifest.v1.json` — registers pre-tool / post-tool / on-verify-* hooks with a required `time_budget_ms` and optional `tool_filter` globs.
  - Both decisively distinguish shell vs http implementation via `oneOf` on `implementation.kind`.

### Example manifests + rmcp spike
- `examples/tools/grep.v1.json` (local-safe shell tool) and `examples/tools/web_fetch.v1.json` (shared-state http tool using `${keychain:…}` interpolation).
- `examples/hooks/log_pre_tool.v1.json` (pre-tool shell hook with 50 ms time budget).
- `examples/README.md` documents the global vs per-repo override convention.
- `experiments/rmcp_spike/` — Phase A prerequisite. Documented procedure + decision matrix + Rust skeleton an implementor runs on the reference machine in ~30–60 min to decide GO / GO-WITH-CAVEATS / NO-GO on `rmcp`. Skeleton is intentionally a stub since `cargo` was unavailable during this documentation pass.

### Rig upgrades
- `validate_artifacts.py` gains rules for `examples/tools/*.json` and `examples/hooks/*.json`.
- `test_schemas.py` gains 10 new tests covering tool + hook manifest valid/invalid corpora.
- New tallies: **14 schemas, 30 artifacts, 62 rig self-tests**, all passing.

### Documentation sweep
- `README.md` — updated layout tree (adds `examples/`, `experiments/`), tally line (14/30/62), "what's blocking work" section (now lists rmcp spike + baseline capture; reference machine moved off the blocker list).
- `tests/README.md` — table reflects 62 tests, tool + hook manifest mention, reference machine populated.
- `schemas/README.md` — adds rows for the two new manifest schemas.
- `tasks/todo.md` — status block updated to v13 totals; Q2 marked resolved; Q4 (checkpoint storage) marked resolved early via the session-storage decision; new Q7 added for the rmcp spike.
- Spec — §14 gains an "On-disk storage" subsection.

### What v13 did NOT change
- The Rust crates still compile in principle but have not been `cargo check`'d in this session (no cargo here).
- Phase A code remains unwritten; nothing in v13 closes the implementation gap, only the Phase A *prerequisites*.

## v12 — 2026-05-15
Closed the last rig-side verification gap: session-artifact validation, including cross-schema `$ref` resolution that previously failed silently.

### Cross-schema reference resolution
- **`tests/_schema_helpers.py`** — new shared module. Builds a `referencing.Registry` mapping every schema's `$id` URL to its local-file content. Both `validate_artifacts.py` and `test_schemas.py` import from here.
- Without this, the session schema's `$ref` to `model_protocol/envelope.v1.json` raised `Unresolvable` and tests that included an envelope silently never exercised the inner schema. Locked-in proof: a new test asserts the registry is load-bearing.

### Example session artifacts
- **`tests/sessions/examples/minimal_success.json`** — a complete successful turn (read → write → pytest, `claimed_done: true`, full envelope, cost ledger, checkpoint pair, three tool fixtures with results).
- **`tests/sessions/examples/with_tool_error.json`** — a turn where the shell tool was blocked by the sandbox. Exercises the `ToolError` taxonomy in `tool_fixtures.error.kind` (`SandboxViolation`), the `uncertainty` envelope path, the `plan` field, and a `constraints` pin.
- **`validate_artifacts.py`** gains a `tests/sessions/examples/*.json` rule pointing at `schemas/session/v1.json`. Both committed examples validate end-to-end with cross-schema $ref traversal.

### New regression tests (in `test_schemas.py`)
- `test_session_with_valid_envelope_passes_cross_schema` — happy path.
- `test_session_with_invalid_envelope_kind_rejected` — bad envelope `kind` trips the inner schema's enum via $ref.
- `test_session_with_invalid_grounding_source_rejected` — bad grounding source likewise.
- `test_example_session_files_validate` — the committed example files validate as-is.
- `test_unregistered_schema_ref_would_fail_without_registry` — sanity guard.

### Verification status
- 11 schemas meta-validated.
- 27 artifacts validated (was 25; +2 example sessions).
- 52 rig self-tests passing (was 47; +5 cross-schema).
- 11 task dry-runs passing.

## v11 — 2026-05-15
All verification gaps closed. Rig is now self-testing and CI-ready.

### Runner upgrades
- **Per-task `checks.json`** for all 11 tasks. Structured assertions (`command + expect{exit_code/contains/pattern}` or `file_unchanged` byte-equal hash check). The runner executes every check after the harness completes and folds per-check results into the result JSON. Closes the no-op-harness exploit on tasks whose starting state is already passing.
- **Schema for checks**: new `schemas/workload/task_checks.v1.json` with `oneOf` enforcement (command XOR file-unchanged) and `anyOf` requiring at least one assertion in `expect`.
- **`<<<atelier-meta>>>` sentinel validation**: after extraction, the payload is validated against `schemas/workload/atelier_meta_sentinel.v1.json`. Violations land in the result's `harness.meta_schema_violation` field and fail the task.
- **`test_command` per task**: `meta.json` carries an optional argv list defaulting to `pytest`; lets non-Python fixtures specify their own runner.
- **`language` per task**: optional `language` enum (`python` / `typescript` / `go` / `rust`).
- **Result schema** (`schemas/workload/runner_result.v1.json`): adds `checks` array per harness result and `meta_schema_violation` on the harness sub-object.

### t11 TypeScript fixture
- **New `t11_add_typescript_function`** — TypeScript equivalent of t01. Uses Node's built-in test runner via `node --test tests/test_utils.ts` (Node 22+ handles `.ts` directly). Exists so §7 Tier-1 hallucination detector has somewhere to run when implemented. Verified end-to-end: starting state fails (rc=1), synthetic real implementation passes, no-op caught.

### Artifact validator upgrades
- **Fenced JSON in few-shot markdown** now validates against the envelope schema. Catches drift between `prompts/protocol_fewshot/*.md` and `schemas/model_protocol/envelope.v1.json`.
- README.md files in glob targets are skipped (they're documentation, not examples).
- `task_checks.v1.json` added to the artifact-validator's rules.

### Rig self-tests
- **`tests/test_schemas.py`** — 26 tests. Schema regression suite locking valid+invalid corpus per schema.
- **`tests/test_validators.py`** — 4 tests. End-to-end invocation of both validator scripts plus direct internals.
- **`tests/test_runner.py`** — 17 tests. `load_task`, `extract_meta` (valid / parse-error / schema-violation paths), `run_check` (all assertion types), subprocess invocations including no-op detection on t05 and t07.
- Total: **47 rig tests, all passing.**

### Makefile + CI
- `make rig-tests` target added; `make check` now runs `schemas → artifacts → rig-tests → summary`.
- **`.github/workflows/check.yml`** — runs `make check` on every push and PR against `ubuntu-latest` and `macos-latest`. Python 3.12 + Node 22.

### Verification status
- 11 schemas meta-validated.
- 25 artifacts validated.
- 47 rig self-tests passing.
- 11 task dry-runs passing.
- No-op exploit verified caught on t05, t07, t11.

## v10 — 2026-05-15
Phase A blockers resolved. Five decisions ratified in spec and scaffolded in code.

### 1. Rust workspace
- **Cargo workspace at repo root** with three member crates under `crates/`:
  - `atelier-core` — agent loop, BYOM adapters, MCP client, session state (no UI deps)
  - `atelier-gui` — Tauri 2.x shell (scaffold)
  - `atelier-tui` — ratatui + crossterm (scaffold)
- **`rust-toolchain.toml`** pins Rust 1.83.0 + rustfmt + clippy.
- **`[workspace.dependencies]`** is the single source of truth for version pins; member crates use `{ workspace = true }`.
- **`.gitignore`** at repo root for `target/`, pycache, editor cruft.

### 2. Tauri 2.x
- Pinned to `2.2` in the workspace deps. Spec §2.5 crate table updated. Frontend stack (TypeScript + Vite + Svelte recommended) chosen by the implementor on first `tauri init`.

### 3. Diff-application atomicity
- **All-or-nothing per turn. No opt-out.** New §3 "Atomic application" subsection: stage to temp tree, run pre-commit validators, atomic move on all-pass, discard + structured error on any failure. One §4 checkpoint per turn covers the whole batch. §7 verification gate runs against the known post-state.

### 4. Tool error model
- **Named taxonomy** in spec §2.5 "Tool error model" with explicit state-machine routing per variant.
- **Rust types** in `crates/atelier-core/src/error.rs` (`ToolError` + `Recovery` enums), unit-tested for the routing decisions.
- **Session schema update**: `tool_fixtures` entries now carry either `result` (success) or `error` (failure with `kind` matching the taxonomy + `message`). Enforced via `oneOf`.

### 5. Credential storage
- **OS keychain primary** via `keyring`; env var override; plaintext config forbidden.
- New §11 "Credential storage" subsection: resolution order, CLI commands (`atelier login/logout/rotate/whoami`), interpolation tokens `${env:NAME}` and `${keychain:NAME}`.
- **MCP servers schema updated**: `env` and `headers` field descriptions document the interpolation tokens.

### Crate-choices table additions (spec §2.5)
- `tokio-util` (cancellation), `tempfile` (atomic staging), `keyring` (secrets), `thiserror`/`anyhow` (errors), `tracing` (logging) all added.
- `Tauri` pin raised to **2.x** explicitly.

### README + todo
- README layout tree adds `Cargo.toml`, `rust-toolchain.toml`, `crates/`.
- todo's Phase A gains explicit decision-receipts: workspace scaffolded, Tauri version pinned, diff atomicity decided, error taxonomy live in code, secrets via keyring.

## v9 — 2026-05-15
MCP as primary tool transport.
- **Spec preamble**: `atelier-core` now lists "MCP client" alongside agent loop and BYOM adapters.
- **§2.5 Agent loop**: added `rmcp` to the crate-choices table; added a "Tool dispatch is unified" subsection — built-in and MCP-routed tools go through the same state transitions.
- **§5 Visible context**: context-panel items can now be MCP resources (per §15), surfaced uniformly.
- **§11 Security**: added an MCP-servers subsection — stdio servers run inside the sandbox; HTTP/SSE servers count as egress; server registration goes through §8 trust budget at the server level.
- **§12 Privacy**: MCP HTTP/SSE servers explicitly count as egress targets and are recorded in the audit log; local-only mode disables them.
- **§15 Extensibility** rewritten — MCP is now the primary tool transport. Built-in tools (file ops, shell, search) exposed via the same internal MCP interface for uniformity. Hooks wrap built-in and MCP-routed calls identically. MCP resources mapped to §5 context; MCP prompts deferred to v0.2.
- **Phase A build plan** adds the §15 MCP client (via `rmcp`) and an extended gate: at least one third-party MCP server (`@modelcontextprotocol/server-filesystem`) must register and dispatch during canonical-workload runs.
- **New schema**: `schemas/config/mcp_servers.v1.json` — server registration manifest, with transport-conditional required fields (`command` for stdio, `url` for http/sse).
- **README** Stack section calls out MCP-out-of-the-box.
- **`tasks/todo.md`** gains a §15 MCP-client work list under Phase A.

## v8 — 2026-05-15
Architecture decisions ratified.
- **Implementation language: Rust.** Three crates declared in the spec preamble: `atelier-core` (agent loop, BYOM adapters, session state — no UI deps), `atelier-gui` (Tauri shell), `atelier-tui` (`ratatui` + `crossterm`).
- **Added §2.5 Agent loop.** Single-turn streaming state machine on `tokio`; named states (`Idle / Streaming / ToolDispatching / ToolExecuting / Verifying / AwaitingUser / Failed / Done`); cancel via Rust drop semantics; bounded in-turn tool parallelism (cap=4 PROVISIONAL). Rejected alternatives table (ReAct scratchpad, mandatory plan-then-execute, Reflexion, ToT, hierarchical loop) with reasons.
- **§3 GUI/TUI parity decision** now names Tauri (GUI) and `ratatui` (TUI) explicitly; both consume `atelier-core` via the broadcast channel.
- **§6 Steerability** points to §2.5: cancellation is drop semantics, not an invented protocol.
- **§7 Verification** clarifies that `claimed_done` triggers a `Verifying` state transition in the §2.5 state machine; the harness owns the transition.
- **Phase A build plan updated** to scaffold the Cargo workspace and `atelier-core` first, with the agent-loop gate folded into the overall Phase A gate.
- **TOC updated** to include §2.5.
- **README** gains a "Stack" section naming Rust + the three crates.
- **`tasks/todo.md`** gains a new §2.5 work list under Phase A.

## v7 — 2026-05-15
Rig polish + remaining fixtures + project plumbing.
- **Wrote the remaining five workload fixtures.** t03 (config migration, rc=1 starting state), t04 (add missing test, rc=5), t07 (refactor preserve behavior, rc=0 starting state with 6 tests), t08 (add input validation, rc=0 starting state with 1 test), t09 (migrate signature, rc=0 starting state with 6 tests). All ten canonical tasks now exist.
- **Added per-task `meta.json`** for all 10 tasks, declaring `expected_starting_returncode`, `turn_cap`, priority flag, and exercises. Backed by `schemas/workload/task_meta.v1.json`.
- **Upgraded the runner** to read `meta.json`, assert the dry-run pytest return code matches the declared value, and produce structured output conforming to `schemas/workload/runner_result.v1.json`. Added `--summary` mode and `--harness-timeout-s` flag; the previously-hardcoded 300s timeout is now PROVISIONAL with a calibration note in the source.
- **Wrote `tests/validate_artifacts.py`** — validates concrete artifacts (meta files, baselines, overhead reports, runner results) against their declared schemas. Distinct from `tests/validate_schemas.py` which meta-validates the schemas themselves.
- **Added `schemas/workload/atelier_meta_sentinel.v1.json`** formalising the `<<<atelier-meta>>>…<<<end>>>` payload format harnesses optionally emit for telemetry.
- **Added root `pyproject.toml`** declaring `jsonschema` and `pytest` under the optional `rig` extra; `norecursedirs` excludes the per-task fixtures from project-level pytest collection.
- **Added `Makefile`** with targets: `check` (schemas + artifacts + summary), `schemas`, `artifacts`, `dry-run`, `summary`, `install-rig`, `clean`. Single-command orchestration.
- **Wrote `compare_baselines.py`** (was a forward reference in v6) — diffs an Atelier prompt-count file against the Claude Code baseline, reports per-task ratios + aggregate, exits 0 iff aggregate ≤ target ratio.
- **Verified end-to-end:** `make check` passes — 10 schemas meta-validated, 10 task-meta artifacts validated, all 10 dry-runs match their declared starting return codes.

## v6 — 2026-05-15
First round where the spec text changed only in minor ways; the bulk of work is implementation artifacts.
- **Wrote the remaining three priority workload fixtures.** t05 (fix-bug-from-failing-test; pytest rc=1 at starting state, as designed), t06 (add-cli-flag; pytest rc=0 at starting state with 3 existing tests), t10 (implement-from-spec; pytest rc=2 at starting state — `LRUCache` not implemented yet, 7 tests waiting). All five priority fixtures now exist.
- **Wrote the workload runner** at `tests/workload/runner/runner.py`. Supports `--dry-run` (validate fixture starting state, no harness) and `--harness-cmd CMD` (invoke a harness via shell, pipe prompt to stdin). Extracts an optional `<<<atelier-meta>>>{json}<<<end>>>` block from harness stdout for turn-count and timing telemetry. **Verified end-to-end against all 5 priority tasks in dry-run mode.**
- **Wrote the schema validator** at `tests/validate_schemas.py`. Iterates `schemas/**.json`, runs JSON-Schema meta-validation, reports pass/fail per file. **Run against the current 7 schemas; all 7 pass meta-validation.**
- **Wrote `baseline_procedure.md`.** Specifies how to capture the Claude Code baseline: reference machine, version pin, model, per-task three-run median, counting rules, when to recapture.
- **Spec updated to point at the runner and validator** so the schema-validation phase-gate step has a runnable form.

## v5 — 2026-05-15
- **Wrote t01 and t02 workload fixtures.** `t01_add_pure_function/` (5 files; pytest collects 0 tests in starting state, exit 0) and `t02_rename_symbol_multi_file/` (10 files; pytest passes 6 tests in starting state). Both fixtures verified locally with `pytest`.
- **Added the session artifact schema** at `schemas/session/v1.json`. The session is the central persistent unit; it wraps conversation history (with envelopes), cost ledger, checkpoint tree, tool-result fixtures, memory, plan, constraints, and the recovery log. Other schemas reference into it.
- **Fixed the DoD inconsistency** introduced in v4. "Phase A + B (first shippable)" is now relabelled "Backend milestone — Phase A + B (internal; not user-facing)"; the §3 GUI gate moves to a new "First user-facing release — Phase A + B + C" section. The first user-facing release is no longer claimed before the UI pillar ships.
- **Marked `$0.00028/sec` PROVISIONAL** with calibration method (survey actual hardware costs once §13 telemetry yields usage data).
- **Added schema validation as a phase-gate requirement.** Every phase gate now includes a schema-validation step; every artifact emitted by phase tests must validate against its `schemas/` schema; a failing validation blocks the gate.
- **Workload README status updated.** t01 and t02 boxes checked; priority subset (t01, t02, t05, t06, t10) marked.

## v4 — 2026-05-15
- **Named the harness: Atelier.** Spec header and prose updated.
- **Removed the published-criticisms citation table.** v3's table was structurally good but every row pointed at the same placeholder source. Brought back later if/when real external sources exist.
- **Moved schemas out of the spec.** `schemas/` directory now holds:
  - `baselines/permission_prompts.v1.json`
  - `protocol/overhead.v1.json`
  - `model_protocol/envelope.v1.json`
  - `telemetry/payload.v1.json`
  - `audit/egress.v1.json`
  - `config/permission_shapes.v1.json`
  - `versions.md` (compatibility matrix for the three independent version streams)
- **Collapsed v0.1 MIP and full v1.** Phases A+B are now explicitly called out as "the smallest shippable harness"; the v0.1-specific table and cut list are gone.
- **Removed self-referential change history from spec.** All "addresses v2…", "resolves…" etc. removed; spec reads clean to a fresh implementer.
- **Wrote the canonical workload** at `tests/workload/canonical/README.md`. 10 tasks listed with success criteria. Priority subset (t01, t02, t05, t06, t10) named for Phase A+B unblock.
- **Fixed the §6/§14 mid-stream cancel inconsistency.** §14's concurrent-edit modal now operates at tool-call boundaries — queue the next dispatch rather than cancel mid-stream. The modal no longer depends on §6's cancel plumbing.
- **Specified `conformance()` overhead.** Bounded ring buffer of last 100 calls, in-memory only.
- **Specified LSP-decline path.** Declined auto-install → Tier-1 degrades to Tier-2 for that language; UI offers one-click retry.
- **Changed local-cost default** from `$0/sec` to `$0.00028/sec` (≈ amortized consumer GPU). Local cost now visible by default in routing decisions.
- **Added headless behavior** for §14 modal: `--non-interactive` flag auto-resolves to "accept external edits"; without it, headless contexts time out at the auto-pause threshold and exit non-zero.
- **Specified action-shape for shell-style tools:** `argv[0]` + flag-name set (not flag values). Examples given in spec; schema at `schemas/config/permission_shapes.v1.json`.
- **Fixed recovery-log placement.** Partial mid-turn output no longer goes into conversation history (which would mislead the next turn's model); it goes to a `recovery_log` slot surfaced as a UI banner.
- **Marked previously unmarked numbers PROVISIONAL:** §2 95% conformance threshold, §7 7-day same-family window, §14 5-minute auto-pause, §15 200ms hook budget — all now PROVISIONAL with calibration methods.
- **Added `--re-execute` replay mode** to §4 — live re-run instead of fixture playback; comparison report shows divergence.
- **Added nightly CI job for overhead refresh** at `ci/nightly/protocol_overhead.yml` with a 10%-over-7-days regression alert.

## v3
- v0.1 MIP defined.
- Build order replaced with phased DAG.
- Capability matrix "claimed-but-broken" column added.
- Local cost latency-weighted (default $0/sec).
- Model Protocol prompting strategy + few-shot examples.
- Tier-1 LSP scoped to TypeScript for v0.1; shell-out decision.
- Tool-result fixture replay subsystem.
- Performance budgets split (internal / end-to-end / hooks).
- Published-criticisms citation table (later cut in v4).
- Schemas as appendix (later moved to `schemas/` in v4).

## v2
- Model Protocol extracted as §2.
- Hard tradeoffs decided in-line.
- Acceptance gates split: mechanical vs UX.
- Security, Privacy, Telemetry, Persistence, Extensibility sections added.
- Steerability reframed as cancel-and-restart.

## v1
- 9 pillars + cross-cutting + hard tradeoffs.
