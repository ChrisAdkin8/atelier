# Atelier Spec — Changelog

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
