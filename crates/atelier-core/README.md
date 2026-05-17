# atelier-core

The Atelier harness core. No UI dependencies. Everything the agent loop, BYOM adapters, session state, dispatcher, built-in tools, ledger, and the §1 probe-on-first-use cache need lives here. `atelier-cli::Runner` ties them together; `atelier-gui` and `atelier-tui` consume the crate over a broadcast channel.

Spec references: §1, §2, §2.5, §3, §4, §7, §11, §14, §15.

## Current state

The crate is **end-to-end runnable** for Phase A/B/C scope: three BYOM adapters live (Mock, Anthropic, OpenAI-compatible — the third covers LM Studio, llama-server, vLLM, sglang, Ollama, OpenAI itself), seven built-in tools route through the §15 dispatcher with §11 sandbox enforcement, hunk accept/reject lives end-to-end via `SessionDispatcher::submit_approval`, and v51 adds probe-on-first-use model adaptation. **472 unit tests, all green** (v51: +34 from `adapter::model_profile`). The only big Phase A item still outstanding is the §15 MCP client (gated on the `rmcp` spike outcome) — the built-in tool dispatcher is the same surface a future MCP client will share, so all hook / ledger / verification wiring is in place.

| Module | Spec | What it gives you |
|---|---|---|
| `src/error.rs` | §2.5 | Tool error taxonomy (`ToolError`) + `Recovery` routing enum + unit tests for the routing table. |
| `src/state.rs` | §2.5 | State enum (`Idle / Streaming / ToolDispatching / ToolExecuting / Verifying / AwaitingUser / Failed / Done`), `LEGAL_TRANSITIONS` table, validated `Transition::new` constructor, `CheckpointHook` + `LedgerHook` traits + `NoopHook` default. |
| `src/session.rs` | §2.5 | Per-session tokio actor: `mpsc` inbox, `broadcast` event bus, `tokio_util::CancellationToken`, bounded `Semaphore` (cap 4 PROVISIONAL). `spawn(checkpoint, ledger) -> Handle`; every `Command::Advance` validates against the spec table and fires the hooks before broadcast. `Cancel` trips the token; the turn-driver advances through the legal path to `AwaitingUser`. |
| `src/protocol.rs` | §2 | Typed `Envelope` mirroring `schemas/model_protocol/envelope.v1.json`. `serde(deny_unknown_fields)`; runtime validates the schema's `maxLength: 500` summary cap. Round-trips all three bundled few-shot examples. Optional fields are `Option<_>` so absent vs. default is type-distinct (spec §2 degradation policy). |
| `src/protocol_strategy.rs` | §2 | Three emission strategies (`Strategy::{NativeTool, JsonSentinel, RegexProse}`) with `downshift()` chain; each has an `encode`/`parse` pair. `parse_json_sentinel` returns the envelope and the natural-language prose separately so the UI can render the two streams. Regex-prose is lossy per spec — `plan_update` and `constraints_acknowledged` drop and re-parse as `None`. |
| `src/protocol_conformance.rs` | §2 (+ §1) | `TurnConformance` issues `TurnDecision::{Reprompt, Downshift, EscalateToUser}` — re-prompt 3× per strategy then downshift, escalate at the bottom of the stack. Cross-call `ConformanceRingBuffer` (capacity 100, PROVISIONAL) for the §1 `Adapter::conformance()` window. |
| `src/staging.rs` | §3 | `Staging::commit` — all-or-nothing multi-file write. Stages into a same-filesystem `tempfile::TempDir`, runs `SyntaxCheck` (tree-sitter; JSON bundled, other Tier-1 extensions return `GrammarMissing`) + SHA-256 pre-edit conflict check, then lexicographically renames. Validation failures leave the workspace untouched. `..` escapes + absolute paths rejected at `add`. Per-file `Hunks` stamped onto each `FileOutcome` for the §3 live diff renderer (pre-image read once for both conflict + diff; race-free). |
| `src/diff.rs` | §3 (Phase C) | `hunks_for / hunks_for_created / hunks_for_deleted` via `similar`. `Hunks::{Same, Lines, Binary, Created, Deleted}` covers the four shapes the workspace renders. Binary detection matches §14 ("NUL in first 8 KB"). |
| `src/context.rs` | §5 (Phase C) | Typed `ContextItem` + insertion-ordered `ContextManager`. `Payload::{FileRef, InlineText, BlobRef}` and `Provenance::{Initial, UserAttached, ToolResult, MemoryPromoted, PinnedByUser}` carry the why-here trace. `evict` returns a `CacheBustEvent` for the §1 cost ledger. `TokenSnapshot` splits known from `Unavailable` so the meter never silently underreports. |
| `src/memory.rs` | §5 (Phase C) | Typed `MemoryCard` matching the schema; `MemoryStore` with `add / touch / pin / unpin / evict`. `promote_to_global` returns `PromoteOutput { relative_path, bytes }` for the caller to write — keeps the module pure of I/O. `OnDiskSession.memory` retyped from `Vec<Value>` to `Vec<MemoryCard>` with on-disk round-trip preserved. |
| `src/plan.rs` | §5 (Phase C) | Typed `PlanStep` + `PlanCanvas` with auto-id `add`, `insert`, `remove`, `mark_status`, idempotent `add_constraint`, and `reorder` that validates membership before mutating. `apply_envelope(&PlanUpdate)` consumes the §2 `plan_update` field (text-match for complete/remove; envelope-driven `reorder` intentionally dropped). |
| `src/adapter/` | §1 | Async `Adapter` trait + typed `Capabilities` matrix + `CapabilityClaim::{Supported, ClaimedButBroken, Unsupported}` + `AdapterError` with `requires_user_decision()` for §2.5 routing. `MockAdapter` (in `mod.rs`) queues pre-built `ChunkStream`s for downstream tests. **Real adapters:** `anthropic.rs` (v38; Messages API, SSE streaming, native tool use, 18 wiremock tests), `openai_compat.rs` (v50; any `POST /v1/chat/completions` server — LM Studio / llama-server / vLLM / sglang / Ollama / OpenAI; 19 wiremock tests), `model_profile.rs` (v51; probe-on-first-use cache — `ModelProfile`, `ProbeObservation`, `decide_strategy`, `probe_model`, `ProfileStore::load_or_probe`; 34 tests). No network anywhere in the test suite — every adapter is exercised against `wiremock`. |
| `src/ledger.rs` | §1 (Phase C unblocker) | Typed `LedgerEntry::{ModelCall, ToolCall, CacheBust}` enforcing the schema's per-kind required fields at compile time. `Ledger` append-only (`parking_lot::RwLock<Vec>` — no poisoning, so a panicking writer can't brick later reads) with `total_cost_usd / entries_without_cost / total_tokens` for the §3 cost meter. `local_cost_usd` + `DEFAULT_LOCAL_RATE_USD_PER_SEC` for latency-weighted local cost. `OnDiskSession.cost_ledger` retyped. Share via `Arc<Ledger>`, never `clone`. |
| `src/dispatcher.rs` | §15 (Phase C unblocker) | Async `Tool` trait + `ToolRegistry` + `Dispatcher::dispatch` walking the per-tool-call lifecycle (lookup → identify hooks via `HookSet::for_tool_event` → execute → translate `CommitReport` → per-file `Event::EditStaged` → build `LedgerEntry::ToolCall`). Returns a pure `DispatchOutcome` — caller side-effects. `SessionDispatcher` wraps it with `Arc<Ledger>` + `broadcast::Sender<Event>` for the runtime path. `HookExecutor` trait + `ShellHookExecutor` (concrete, via the subprocess helper) + `NoopHookExecutor` (test default). |
| `src/subprocess.rs` | §11 / §15 (Phase C unblocker) | Shared `run(program, args, &SubprocessSpec)` over `tokio::process` with concurrent stdout/stderr drain + timeout + reap. `sandboxed_argv(argv, &SandboxPolicy)` produces the macOS sandbox-exec / Linux bwrap-wrapped argv. Powers both the `shell` built-in tool and `ShellHookExecutor` so the §11 plumbing isn't duplicated. |
| `src/tools/` | §15 (Phase C unblocker) | Seven `Tool` impls: `read_file`, `list_dir`, `grep` (regex + walkdir; skips binary + hidden dirs + symlinks), `write_file` (via `Staging`), `edit_file` (anchor-based; rejects ambiguous matches; via `Staging` with `expected_pre_hash`), `ast_grep` (`kind:<node-kind>` over tree-sitter-json), `shell` (via subprocess helper + sandbox profile). Path safety enforced uniformly via `crate::path_safety` (syntax + symlink-containment). Every file-touching tool wraps its blocking I/O in `tokio::task::spawn_blocking` so the async runtime stays responsive. |
| `src/path_safety.rs` | §11 (security) | Repo-relative path validation + canonicalize-and-prefix-check for symlink containment. Every file-touching tool calls it after `resolve_repo_path`; `Staging::commit` does the equivalent inline. Catches the symlink-escape attack (repo-internal `link.txt` → `/etc/passwd`) that the §11 sandbox profile generator doesn't cover (the profile only wraps shelled-out subprocesses, not the harness's own I/O). |
| `src/verify.rs` | §7 | Pure `compare(envelope, &[ObservedChange]) -> Vec<Discrepancy>` for the did-it-do-what-it-said gate. Detects claimed-but-missing, silent-edit, kind-mismatch (e.g. claimed delete + observed modify), duplicate claims. |
| `src/dod.rs` | §7 | `DodConfig` loader for `schemas/config/dod.v1.json`. Discovery: per-repo `<repo>/.atelier/dod.json` overrides global `~/.atelier/dod.json`; missing both is soft no-config. Validates name regex, absolute / `..`-escaping `working_dir`, zero timeouts. `by_tier` helper for UI grouping. |
| `src/sandbox.rs` | §11 | Profile generators (no subprocess launch yet): `macos_profile(&SandboxPolicy) -> String` for `sandbox-exec`, `linux_bwrap_argv(&SandboxPolicy, &[&str]) -> Vec<String>` for bubblewrap. Default: deny network, RO system dirs, RW repo + `/tmp` (tmpfs on Linux). Writes to `/etc` and `/usr/local` rejected at policy-build time. `with_net()` flips the default deny. |
| `src/persistence.rs` | §14 | Typed `OnDiskSession` round-tripping `schemas/session/v1.json`; atomic `save_to` (`NamedTempFile::persist`); `load_from` rejects mismatched `HARNESS_SESSION_VERSION`. `RecoveryEntry` + `RecoveryReason::{Crash, UserCancel, Timeout, ConcurrentEditPause}` for the §14 recovery_log. Global `Registry` (`~/.atelier/registry.json`) with `touch` / `forget` / atomic save. |
| `src/hooks.rs` | §15 | `HookManifest` loader for `schemas/config/hook_manifest.v1.json` (validates version, name regex, budget, filter-event compatibility, non-empty command/url). `HookSet::load_dir` + `merge_dir` for per-repo-overrides-global discovery. `HookApprovals` first-use approval store at `<hook-dir>/_approvals.json` (atomic save, `partition` helper for the UI prompt). |
| `src/init.rs` | §11 | `atelier init` bootstrap — creates `.atelier/{sessions,tools,hooks}/`, seeds `ATELIER.md`, appends `.atelier/` to `.gitignore`. Idempotent. |

## Build

```
cargo build -p atelier-core
cargo test  -p atelier-core   # 472 tests (v51)
```

## `rmcp` dependency wiring

`rmcp` is the official Rust SDK for the **Model Context Protocol** — Atelier's tool transport (spec §15). There is **no separate install step**: `rmcp` is a Cargo dependency that resolves from crates.io on first build.

### Where `rmcp` lives

The dependency is declared in two coordinated places — the version pin at the workspace root, and the actual consumer here in `atelier-core` (the crate that owns the MCP client; `atelier-gui`, `atelier-tui`, and `atelier-cli` reach `rmcp` transitively through `atelier-core` when they need to).

**1. Workspace root** — `../../Cargo.toml`:

```toml
[workspace.dependencies]
rmcp = "0.1"
```

**2. Consuming crate** — `Cargo.toml` (this crate):

```toml
[dependencies]
rmcp = { workspace = true }
```

This pattern — pin the version once at the root, reference it as `{ workspace = true }` from each consuming crate — is how every workspace dependency is wired. It keeps versions synchronized across crates and means a bump only happens in one place.

If a future workspace crate ever needs `rmcp` directly (rather than via `atelier-core`), add the same `rmcp = { workspace = true }` line to its `[dependencies]` — **never** redeclare the version.

### Fetch and verify

```sh
cargo fetch                       # download rmcp + transitive deps from crates.io
cargo check -p atelier-core       # confirm rmcp resolves and compiles cleanly
```

A successful `cargo check` ends with a line like:

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.85s
```

### Troubleshooting

- **`feature edition2024 is required`** — your toolchain is older than 1.85.0. See [`../../docs/toolchain.md`](../../docs/toolchain.md) for the pinned-toolchain story.
- **Network errors during `cargo fetch`** — `rmcp` and its transitive deps are pulled from crates.io. Check your network, or set `CARGO_HTTP_PROXY` if you're behind a corporate proxy.

### The maturity spike

For the standalone `rmcp` maturity-assessment spike — a separate experiment, not part of the Cargo workspace — see `../../experiments/rmcp_spike/README.md`. Its outcome (GO / GO-WITH-CAVEATS / NO-GO) is a Phase A prerequisite per `../../tasks/todo.md`.

## What's still planned

The table above lists what exists today. Outstanding (in roughly the order they unblock each other):

- `mcp` — MCP client wrapping `rmcp`; stdio + HTTP/SSE transports; server registration from `mcp_servers.json`. Gated on the `rmcp` maturity spike at `../../experiments/rmcp_spike/`. The dispatcher is already structured around the `Tool` trait; the MCP client lands as a new `Tool` impl shape that wraps `rmcp::Client`.
- LiteLLM-shaped adapter — likely subsumed by `openai_compat` if the LiteLLM gateway speaks the OpenAI chat-completions surface, which it does. Re-evaluate once a concrete LiteLLM regression is in the canonical workload.
- Bedrock + Vertex adapters (Phase E/F).
- DoD-check executor — the loader is in but the runtime that actually shells out to `dod.checks[].command` and folds results into the `Verifying` transition is stubbed. The Runner emits a one-shot warning when a DoD config is present so callers see that checks aren't being honoured.
- `secrets` — OS keychain (`keyring`) + env-var fallback; `${env:…}` / `${keychain:…}` interpolation per spec §11. Today `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` are read directly from the environment.
- `checkpoint` — §4 diff-blob storage under `.atelier/sessions/<uuid>/diffs/`; integrates with `persistence::OnDiskSession::checkpoints`.
- `watcher` — `notify` integration for §14 concurrent-edit detection; queues at tool-call boundary, never cancels mid-stream. The crate already declares the `notify` dep in the workspace; the integration is gated on the dispatcher exposing its read-set.
- Probe-driven initial-strategy hint — v51 lands the observation layer; threading `ModelProfile.strategy` into the adapter's first-turn strategy (so the warm-up doesn't waste a downshift cycle) is a v52 one-line wiring change.

The 8 built-in tool manifests live under `tools/`; subagent type manifests under `subagents/`; skill manifests under `skills/`; the MCP catalog at `catalog/mcp_servers.json`. The dispatcher reads the tool manifests at session start.
