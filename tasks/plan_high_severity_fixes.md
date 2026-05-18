# Plan — High-severity fixes from `deep_code_scan_v60.27.md`

Date: 2026-05-18. Source: `tasks/deep_code_scan_v60.27.md` (16 High findings, 0 Critical).

The audit grouped the 16 High items into three file-disjoint bundles (v60.28 / v60.29 / v60.30). This plan turns each item into a concrete change with files, intended diff shape, and a verification gate. Bundles are independent under L-D-2 (file-disjoint parallel-bundle release pattern) and can be developed concurrently on separate worktrees; H16 is a one-line schema typo that piggybacks on v60.28.

## Standing gates (all bundles)

Per `ATELIER.md`'s verification convention, every PR must show:

- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test -p atelier-core` (and `-p atelier-cli` where the change touches it)
- `make check` (rig: schemas → artifacts → rig-tests → workload dry-run)

Each item below adds a **targeted** verification on top of these — the smallest test that would have caught the issue.

---

## v60.28 — Secrets & egress hardening (H1–H8 + H16)

Touches: `crates/atelier-core/src/adapter/{anthropic,openai_compat}.rs`, `crates/atelier-core/src/mcp/http_launcher.rs`, `crates/atelier-gui/src/lib.rs`, `schemas/protocol/overhead.v1.json`.

### H1 — Rotate `.envrc` ANTHROPIC_API_KEY *(operator action, do first)*

- Revoke the existing key in the Anthropic console.
- Mint a new key, store in 1Password / `security add-generic-password` keychain entry.
- Update `.envrc` to source from keychain (or replace with `direnv`'s `dotenv` + an outside-repo `.env`).
- `git grep -n "sk-ant-"` returns zero hits in tracked source.
- Sweep `~/Projects/atelier/.claude/worktrees/agent-*/.envrc` clones (~15 of them) — `find . -name .envrc -exec grep -l sk-ant {} +` then overwrite.
- **Verify:** `rg --hidden -g '!.git' 'sk-ant-' .` → empty. `direnv exec . env | grep ANTHROPIC_API_KEY` still resolves.

### H2 — Gate `swap_adapter` Tauri command

- File: `crates/atelier-gui/src/lib.rs:671-744`.
- Add a `base_url` allowlist constant (`anthropic.com`, `api.openai.com`, `localhost`, `127.0.0.1`, plus a configurable list loaded from `providers.toml`).
- Reject calls whose `base_url` host does not parse to an allowlisted host. Error: `AdapterSwapError::BaseUrlNotAllowed`.
- Add a consent-modal event on the Tauri bus before the swap completes; renderer must reply with explicit accept. Refactor: hoist current "swap and emit `AdapterSwapped`" into "emit `AdapterSwapPending` → await `AdapterSwapAccepted | Rejected`".
- **Verify:** unit test injects a `base_url=https://evil.example` and asserts the swap is refused; a second test asserts a localhost swap succeeds; a Playwright/Webdriver step (deferred if not wired) covers the modal round-trip.

### H3 — Redact + cap adapter error bodies

- Files: `crates/atelier-core/src/adapter/anthropic.rs:529,539,565`, equivalents in `openai_compat.rs`, plus the `AdapterError::Auth { body }` / `Provider { body }` variants in `adapter/mod.rs`.
- Introduce a `redact_response_body(&str) -> String` helper: strip anything matching `(?i)(sk-ant-[A-Za-z0-9_-]+|sk-[A-Za-z0-9]{20,}|Bearer\s+[^\s"']+|"api_key"\s*:\s*"[^"]*")`, then `take_chars(256)` (not bytes — UTF-8 safe truncation that doesn't split codepoints).
- Wire it at the *construction* sites of every `AdapterError::{Auth,Provider}`, not in `Display`, so anything serialising the error (RunReport JSONL, session.json) gets the redacted form.
- **Verify:** new `adapter::redaction_tests::secrets_never_serialised` round-trips an `AdapterError::Auth { body: "sk-ant-abc... extra payload" }` through `serde_json::to_string` and asserts neither `sk-ant-` nor any 30+ char token survives.

### H4 — Disable redirects on credential-bearing reqwest clients

- Files: `anthropic.rs:218-246, 280-308`, `openai_compat.rs:265-298, 331-364`.
- On the cred-bearing client builder, append `.redirect(reqwest::redirect::Policy::none())`. (The probe / non-cred client can keep limited redirects.)
- **Verify:** new test spawns a tiny `axum` test server that 302s to `127.0.0.1:0`; assert the adapter sees the 302 status code rather than auto-following. Confirms the `x-api-key` / `Authorization` header is never forwarded.

### H5 — MCP HTTP host allowlist

- File: `crates/atelier-core/src/mcp/http_launcher.rs:53-56,121-142`.
- Extend `McpServerConfig` (in `schemas/config/mcp_servers.v1.json` + Rust type) with an optional `allowed_hosts: Vec<String>` per server entry; default to `[host(url)]` when omitted.
- Reject `call_tool` egress whose effective URL host doesn't match. Surface as `McpLaunchError::HostNotAllowed`.
- Update `schemas/config/mcp_servers.v1.json` (add field, keep `additionalProperties: false`); update fixture in `tests/fixtures/mcp_servers/`.
- **Verify:** unit test in `http_launcher.rs` asserts that a `call_tool` whose redirect or post-handshake URL host changes mid-session is refused. Schema test in `tests/test_schemas.py` asserts the new field round-trips.

### H6 — MCP per-`call_tool` audit row

- File: `crates/atelier-core/src/mcp/http_launcher.rs` (replace the "deferred to dispatcher" TODO at lines 53-56) and `dispatcher.rs` where `McpToolWrapper::invoke` is called.
- On each `call_tool`, emit a `schemas/audit/mcp_egress.v1.json`-shaped row through the existing audit appender. Reuse the handshake row's fields (`server_name`, `url`, `tool_name`, `bytes_in`, `bytes_out`, `latency_ms`).
- **Verify:** integration test under `tests/integration/mcp_audit.rs` runs the stdio launcher fixture (`@modelcontextprotocol/server-filesystem`), invokes one tool, then asserts the audit log has both a `handshake` row and a `call_tool` row, schema-validated.

### H7 — Cap non-stream HTTP `.bytes().await`

- Files: `anthropic.rs:235`, `openai_compat.rs:287`.
- Replace `resp.bytes().await?` with a streamed accumulator: `let mut buf = Vec::with_capacity(64 * 1024); while let Some(chunk) = resp.chunk().await? { if buf.len() + chunk.len() > 32 * 1024 * 1024 { return Err(AdapterError::ResponseTooLarge { limit: 32 << 20 }); } buf.extend_from_slice(&chunk); }`.
- Add `AdapterError::ResponseTooLarge { limit: usize }` variant + serde + `wire_label()` + the L-D-5 wire-label/serde agreement test.
- **Verify:** new test in `adapter::limit_tests` feeds a 33-MiB body via `wiremock` and asserts `Err(AdapterError::ResponseTooLarge { .. })`.

### H8 — Cap SSE per-event accumulator

- Files: `anthropic.rs:638-825`, `openai_compat.rs:660,790-793`.
- Add a `current_event_data: BoundedString` wrapper (or just an inline `if current_event_data.len() + line.len() > MAX_EVENT_BYTES`). Pick `MAX_EVENT_BYTES = 8 * 1024 * 1024`.
- Surface overflow as `AdapterError::SseEventTooLarge { limit }`; reuse the L-D-5 wire-label/serde test scaffold from H7.
- **Verify:** new test feeds 1000 × `data: <chunk>\n` lines totalling > 8 MiB into the SSE parser; assert the error fires before OOM-suspect growth.

### H16 — `overhead.v1.json` wire-label drift (piggyback)

- File: `schemas/protocol/overhead.v1.json:19`.
- Replace `"json_mode"` with `"json_sentinel"` in the enum.
- **Verify:** add an assertion in `tests/test_schemas.py` (or extend the existing wire-label test) that every `enum: [...]` value across `schemas/` is reachable from `Strategy::as_str()` / `Strategy::wire_label()`. One-time sweep across all schema enums to catch siblings.

### Bundle gate

`make check && cargo test -p atelier-core && cargo test -p atelier-gui --features test-seams` plus a new `make secret-grep` target that fails if `rg 'sk-ant-|sk-[A-Za-z0-9]{20,}'` finds tracked-source hits.

---

## v60.29 — Liveness & durability (H9–H12)

Touches: `crates/atelier-core/src/{dispatcher.rs,file_watcher.rs,staging.rs}`, `crates/atelier-cli/src/{runner.rs,main.rs}`.

### H9 — Thread `CancellationToken` into `ToolContext` + per-tool deadline

- Files: `crates/atelier-core/src/dispatcher.rs:474-693`, the `ToolContext` definition, every `Tool::invoke` impl, plus the §2.5 actor that owns the root token.
- Add `cancel: tokio_util::sync::CancellationToken` and `deadline: std::time::Duration` to `ToolContext`. Default deadline 60s; per-tool override via the new `tool_manifest.v1.json` `deadline_ms` field (consumed by both `BuiltInToolWrapper` and `McpToolWrapper`).
- In `dispatcher.rs` (`invoke_tool` loop), race the tool future against `tokio::select! { _ = cancel.cancelled() => Err(ToolError::Cancelled), _ = tokio::time::sleep(deadline) => Err(ToolError::Deadline) }`.
- Each built-in's blocking work (`read_file`, `shell`, etc.) is already in `tokio::task::spawn_blocking`; cancellation aborts the join handle.
- **Verify:** new test in `dispatcher::cancellation_tests` invokes a `MockSlowTool` that sleeps 5s with `deadline: 200ms`; assert `Err(ToolError::Deadline)` returns within 300ms wall-clock.

### H10 — SIGINT/SIGTERM handler in CLI `main`

- File: `crates/atelier-cli/src/main.rs::run_run` and `crates/atelier-cli/src/runner.rs`.
- Race the existing run-future against `tokio::signal::ctrl_c()` + (on unix) `signal(SignalKind::terminate())`. On signal: cancel the root `CancellationToken` from H9, then `await runner.shutdown()` which calls `OnDiskSession::save_to` synchronously, then exit with 130/143.
- Refactor: extract the current "run-and-save" tail of `run_run` into `Runner::shutdown(&mut self) -> Result<()>` so both happy-path and signal-path call the same code.
- **Verify:** new integration test under `crates/atelier-cli/tests/sigint_resume.rs` spawns `atelier run …`, sends SIGINT after the first tool call, then runs `atelier run --resume <UUID>` and asserts the recovery log includes the partial turn (not a zero-length `session.json`).

### H11 — Fix `write_with_sync` ordering in staging

- File: `crates/atelier-core/src/staging.rs:417,779-783` (function `write_with_sync`).
- Rewrite: write to a sibling `{path}.atelier-tmp.<pid>.<rand>`; `sync_all`; rename to `path`; `fsync_dir_best_effort(parent)`. Reuse the existing `fsync_dir_best_effort` helper at lines 789-793.
- Update the doc comment to reflect "tmp → fsync → rename → fsync-dir" rather than "create → write → sync".
- **Verify:** new test in `staging::durability_tests` uses `nix::sys::signal::kill` (or a simpler injected panic in a test-only branch) between the tmp-write and rename; assert the staged target either does not exist or has full contents (never zero-length).

### H12 — Hoist canonicalize out of file_watcher lock

- File: `crates/atelier-core/src/file_watcher.rs:188,229-238` and the double-canonicalize at `:99,123` noted as a Low.
- Refactor: call `std::fs::canonicalize` *before* taking the `parking_lot::Mutex` in `track()`. Pass the canonical `PathBuf` into the locked critical section.
- For the watcher event-receive path: same shape — canonicalize on the notify worker thread, only the `HashSet<PathBuf>` membership check stays inside the lock.
- Fold the duplicate canonicalize at line 99/123 into one call.
- **Verify:** new test in `file_watcher::contention_tests` spawns 32 `track()` calls in parallel against a path on a slow filesystem (use `nfs-mock` or a 100ms sleep injected via a test hook); assert P99 wait time stays < 5ms (was ~100ms × N before).

### Bundle gate

`cargo test -p atelier-core --lib -- staging:: file_watcher:: dispatcher::cancellation` and the new `crates/atelier-cli/tests/sigint_resume.rs`. Manual: `^C` a real `atelier run` mid-tool and confirm `--resume` works.

---

## v60.30 — TUI / Frontend hygiene (H13–H15 + UI Mediums)

Touches: `crates/atelier-tui/src/lib.rs`, `crates/atelier-gui/ui/src/lib/{InlineRenderers,App}.svelte`, `crates/atelier-gui/ui/src/lib/state.ts`.

### H13 — TerminalGuard ordering + panic hook

- File: `crates/atelier-tui/src/lib.rs:2453,2466-2474`.
- Restructure `setup_terminal()`:
  ```
  enable_raw_mode()?;
  let guard = TerminalGuard;          // binds the disable on Drop *before* anything else fallible
  execute!(stdout(), EnterAlternateScreen)?;
  let term = Terminal::new(CrosstermBackend::new(stdout()))?;
  ```
- Install a panic hook (line 2453's gap): `std::panic::set_hook` that calls `disable_raw_mode()` + `LeaveAlternateScreen` + chains the default hook.
- **Verify:** new test `terminal_guard::leak_test` simulates `Terminal::new` failing (test-only override) and asserts raw-mode is disabled afterwards.

### H14 — KeyEventKind::Press filter

- File: `crates/atelier-tui/src/lib.rs:2237-2430`.
- One-line guard at the top of the key-event arm: `if key.kind != KeyEventKind::Press { continue; }`.
- **Verify:** unit test feeds a `KeyEvent { kind: Release, .. }` and asserts no state mutation occurs.

### H15 — ANSI / control-char sanitiser

- File: `crates/atelier-tui/src/lib.rs:1238` and every other call site that pushes LLM/tool/file content into `Span::raw`.
- Add a `safe_span(s: &str) -> String` helper that strips `\x1b` (ESC), `\x07` (BEL), `\x9b` (CSI), and other C0/C1 control chars except `\t` and `\n`. Convert printable but tricky chars (zero-width, RTL override) to U+FFFD or visible escape (e.g. `\u{202e}` → `<RLO>`).
- Use it at every `Span::raw(<content>)` site that consumes externally-supplied strings; static UI labels keep `Span::raw` as-is.
- **Verify:** new property test in `tui::sanitiser_tests` asserts `safe_span` is idempotent and never emits raw `\x1b`. End-to-end: feed an LLM event containing `"\x1b[2JOWNED"` and assert the rendered ratatui buffer contains the literal escape spelling, not a cleared screen.

### UI Mediums folded in

These ride v60.30 because they share `crates/atelier-gui/ui/`:

- **Mermaid `securityLevel: 'strict'`** — `InlineRenderers.svelte:157,168`. Pass `mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', ... })` once at module top; replace `target.innerHTML = svg` with a sanitised path using DOMPurify already in the dep tree (or wrap in `DocumentFragment` parsing).
- **`resolveImageSrc` normaliser** — `InlineRenderers.svelte:119-127`. Reject paths containing `..`, require markdown `![alt](rel/path.ext)` form (not bare filename heuristic), only allow extensions `[png|jpg|jpeg|gif|svg|webp]`, and resolve relative to the session workspace root before handing to `convertFileSrc`.
- **`concurrentEditModal` modal inerting** — `App.svelte:353`. Add `inert` attribute (or `aria-hidden="true" + pointer-events:none`) on the DiffPane while the modal is open so Enter-handling can't accept stale hunks.
- **AppState default arm** — `ui/src/lib/state.ts:560-566`. Replace the silent `default` with a `console.error` plus a dev-only `throw` (gated on `import.meta.env.DEV`), so unknown event variants are visible in CI.
- **Mermaid DOM id escape** — Low item; pair with H15's UI work since it's the same file.

### Bundle gate

`cargo test -p atelier-tui` + `cargo clippy -p atelier-tui -- -D warnings`. Frontend: `cd crates/atelier-gui/ui && npm run check && npm run test`. Manual smoke: drive the TUI with an LLM that emits `\x1b[2J` and the GUI with a memory card containing `<img src=javascript:alert(1)>`; both must render literal text.

---

## Sequencing & risk

- All three bundles are file-disjoint per L-D-2: parallel worktrees `agent-v60.28`, `agent-v60.29`, `agent-v60.30` can land in any order.
- Start with **v60.28** — it has the highest user-visible blast radius (secrets, MCP egress) and the smallest per-item diffs.
- **v60.29** is the largest single bundle (H9 alone touches every `Tool` impl); plan two days for it.
- **v60.30** can be split if scheduling pressure hits: H14 (one line) + H13 (small) ship as a hotfix, H15 + UI Mediums as v60.30 proper.
- Each bundle ends with: green CI, `CHANGELOG.md` entry, tag, and a one-line digest in `tasks/todo.md`.

## Out of scope (covered elsewhere)

- The ~38 Mediums and ~50 Lows are not in this plan — they belong to follow-on hygiene bundles or `tasks/todo.md` backlog.
- `cargo audit` and `npm audit --audit-level=high` integration into `make check` (reviewer note 2 + 3) → separate "supply-chain gates" mini-bundle.
- Phase B operator actions (LSP spike, ANTHROPIC_API_KEY GH Actions wiring, 7-night calibration window) are orthogonal and tracked in `tasks/todo.md`.
