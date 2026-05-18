# Deep code scan — atelier @ v60.27 (commit 7714f5a)

Date: 2026-05-18. Read-only audit. No files modified.

**Reviewers (7 parallel agents):**

1. `atelier-core` rust-reviewer
2. `atelier-cli` rust-reviewer
3. `atelier-gui` (Rust side) rust-reviewer
4. `atelier-tui` rust-reviewer
5. Cross-cutting security audit
6. Python rig + JSON Schemas + GitHub Actions + Makefile
7. Svelte 5 frontend (`crates/atelier-gui/ui/`)

**Tally:** 0 Critical · 16 High · ~38 Medium · ~50 Low · ~5 Informational.

---

## Critical / High — fix-before-1.0

### Secrets & credentials

1. **`.envrc` contains live `ANTHROPIC_API_KEY` in cleartext.** Gitignored, but present on disk inside ~15 `.claude/worktrees/agent-*/` clones. Rotate now; move to keychain / 1Password CLI.
2. **`swap_adapter` is a renderer-callable command that picks `base_url` + reads `OPENAI_API_KEY` from env.** Any XSS in the webview becomes credential exfil. — `crates/atelier-gui/src/lib.rs:671-744`. Fix: gate behind consent modal + base_url allowlist.
3. **`AdapterError::{Auth,Provider}` stores raw response bodies and is `Serialize`.** Future RunReport/session persistence will leak any body fragment that echoed the request — including pasted prompt secrets. — `adapter/anthropic.rs:529,539,565` and `adapter/openai_compat.rs` equivalents. Fix: redact + cap to 256 B.
4. **Reqwest clients use default redirect policy.** A 302 on the auth-bearing path forwards `x-api-key` / `Authorization` to the new origin. — `anthropic.rs:218-246, 280-308`, `openai_compat.rs:265-298, 331-364`. Fix: `.redirect(Policy::none())` on the cred-bearing client.

### MCP / egress

5. **No host allowlist on HTTP MCP servers.** `mcp/http_launcher.rs` dials any URL declared in `mcp_servers.json` once the operator approves the *name*. The URL itself isn't audited.
6. **Tool invocations post-handshake emit no audit row.** `http_launcher.rs:53-56` defers per-`call_tool` egress to "the dispatcher's §12 hook" — not wired yet.

### DoS / unbounded reads

7. **Non-stream HTTP `.bytes().await` is uncapped.** A hostile or proxy-injected response can OOM the process. — `anthropic.rs:235`, `openai_compat.rs:287`. Fix: switch to `bytes_stream()` + 32 MiB cap.
8. **SSE per-event accumulator unbounded.** Only the line buffer is capped; an infinite sequence of `data:` lines feeding one event grows `current_event_data` forever. — `anthropic.rs:638-825`, `openai_compat.rs:660,790-793`.

### Cancellation / liveness

9. **Dispatcher has no per-tool deadline or `CancellationToken`.** A hanging MCP server pins a turn indefinitely. — `dispatcher.rs:474-693`. The §2.5 actor's token is not threaded into `ToolContext`.
10. **No SIGINT/SIGTERM handler in CLI `main`.** `^C` mid-run skips `OnDiskSession::save_to` and leaves a partial `session.json`, which `--resume` then reads. — `crates/atelier-cli/src/runner.rs` Drop guards never fire on async cancellation from an uninstalled signal handler.

### Durability

11. **`write_with_sync` truncates then writes with no fsync-before-rename.** Process death between `File::create` and `sync_all` publishes a zero-length file, defeating §3 atomicity. — `staging.rs:417,779-783`.

### Concurrency

12. **File watcher holds parking_lot `Mutex` across blocking `canonicalize` syscalls.** Concurrent `track()` callers starve. — `file_watcher.rs:188,229-238`. Fix: hoist canonicalization above the lock, or `spawn_blocking`.

### TUI hygiene

13. **Terminal raw-mode leak window.** `enable_raw_mode()`, `EnterAlternateScreen`, `Terminal::new` run before the `TerminalGuard` is bound; a `?` on either of the latter two leaves the user's shell in raw mode. — `atelier-tui/src/lib.rs:2466-2474`.
14. **`KeyEvent.kind` never filtered.** On Windows ConPTY + kitty/wezterm/foot, Press+Release both fire → every keystroke double-fires. — `atelier-tui/src/lib.rs:2237-2430`.
15. **ANSI / control chars passthrough.** LLM, tool-result and file-name strings are rendered raw into ratatui `Span::raw`. A hostile model can forge UI inside the TUI. — multiple call sites starting at `lib.rs:1238`.

### Wire-label drift

16. **`schemas/protocol/overhead.v1.json` enumerates `"json_mode"`, but the rest of the codebase emits `"json_sentinel"`.** Producer/schema split-brain. — `schemas/protocol/overhead.v1.json:19` vs. `schemas/ci/protocol_conformance.v1.json:71` and `Strategy::as_str()`.

---

## Medium — fix-this-quarter

### Test seams & API surface

- `Runner::with_adapter_for_test` / `with_starting_strategy_override` / `with_tier1_diagnostics_for_test` / `with_degradation_window` / `with_degradation_threshold` are `pub` with only `#[doc(hidden)]`. Production downstream code can pin stale strategies. — `runner.rs:681-689, 827, 838, 892, 904`.
- `compact_context_items` is the only path that observes the post-`swap_adapter` adapter; the renderer is told via `AdapterSwapped` that the swap is live before the Runner sees it. UI lies. — `lib.rs:660-670`.
- AppState `default` arm silently drops unknown event variants. Adapter bugs become invisible. — `ui/src/lib/state.ts:560-566`.

### XSS / IPC

- Mermaid initialized without `securityLevel: 'strict'`. LLM-driven `claimedChanges` and memory-card bodies feed `target.innerHTML = svg`. — `InlineRenderers.svelte:157,168`.
- `resolveImageSrc` accepts any agent-emitted line ending in image-extension, passes through `convertFileSrc`. Path-traversal + asset-protocol exfil vector. — `InlineRenderers.svelte:119-127`.
- `concurrentEditModal` doesn't `inert` DiffPane underneath — Enter-fires stale approval. — `App.svelte:353`.

### CI / supply chain

- Three nightly workflows commit to `main` with no `git pull --rebase` — 30-min spacing is unrealistic for ~90-min Phase A run; second push can silently lose an artifact. — `.github/workflows/nightly_*.yml`.
- All third-party actions tag-pinned (`@v4`, `@v5`). Mutable; SHA-pin per GitHub hardening guidance.
- `check.yml` has no top-level `permissions:` block.

### Runner correctness

- `OPENAI_BASE_URL` env-var overrides documented CLI > profile > defaults precedence. — `main.rs:580-583`.
- `AwaitingUser` final state exits 0, masking stalled agents from CI gates. — `main.rs:339-450`.
- Compact-retry re-sends stale `messages_for_call` after context mutation. — `runner.rs:1430-1559`.
- `swap_adapter` is `async` but never awaits; holds `parking_lot` locks across the false async boundary. — `runner.rs:730-767`.
- `conformance_status` binary reads `tests/phase_b_gate/last_run.json` via build-time `CARGO_MANIFEST_DIR` join — binary built in one worktree, run elsewhere, reads the wrong file. — `bin/conformance_status.rs:77-83`.

### Schemas / hygiene

- `.atelier/sessions/` is not in `.gitignore`. One `git add .` after a local run leaks session UUIDs + partial completions.
- `schemas/protocol/overhead.v1.json` lacks `additionalProperties: false` (only schema without it) — typos pass.
- `schemas/audit/egress.v1.json` is missing the `kind` discriminator that `mcp_egress.v1.json` / `subprocess_egress.v1.json` all require — schemas not mutually exclusive at validation time.
- `validate_artifacts.py` silently skips JSON paths nothing in the rule table matches. New artifacts can land un-validated.
- Workload runner uses `subprocess.run(cmd, shell=True)` + no process-group kill on timeout — leaks grandchildren.

### Other

- LSP install subprocess and HTTP MCP server lifecycle inherit unbounded PATH/env — design for argv + `ENV_PASSTHROUGH` allowlist before wiring.
- Anthropic / OpenAI `chat()` retries clone `messages_for_call` but mishandle compaction state.
- `dispatcher.rs::extract_read_paths` adds workspace root to file-watcher read-set when grep arg is empty → every save fires `FilesChanged`. — `:957-996`.
- `truncate_to_bytes` exceeds its cap by 3 bytes (the `…` suffix). — `lsp/typescript.rs:112-124`.
- `read_file.byte_len` reports the file's total size while `contents` is truncated — UIs computing `contents.len() == byte_len` mis-read truncation. — `tools/read_file.rs:128`.
- Memory `sanitize_filename` returns empty / pure-dot strings unguarded. — `memory.rs:370-380`.
- MCP protocol version check uses `format!("{:?}", ...).contains(SUPPORTED_PROTOCOL_VERSION)` — brittle Debug-string match. — `mcp/stdio_launcher.rs:414-422`.
- `LspApprovals::save` (`lsp/approval.rs:69-94`) lacks `sync_all` on the temp file + parent dir fsync; other atomic-write helpers (`init.rs`, `persistence.rs`) do both.
- Audit appenders (`audit.rs:269-277`) flush but never `sync_all` — §11/§12 audit rows can be lost on crash.
- Subprocess pipe-take `.expect("piped stdout was requested above")` panics in production on future plumbing changes. — `subprocess.rs:228-244`.
- `submit_approval` silently returns `false` if dispatcher dropped (cancellation) between submit and remove — UI accept-set lost on the floor. — `dispatcher.rs:1147-1152`.

---

## Low — backlog hygiene

Roughly 50 Low items across the seven reports. Highlights:

- Needless clones in hot paths (`AutoApprove::approve`, `latency_f64` double-projection in runner).
- `.unwrap_or_default()` on serde projections that mask serialisation regressions (`runner.rs:1793`, `lib.rs:1003,1010,1013,1028` in GUI bridge).
- Double `canonicalize` in `file_watcher::track` (lines 99 and 123).
- Missing panic hook for terminal restore (`atelier-tui/src/lib.rs:2453`).
- O(n) `Vec::remove(0)` event-log pop (`atelier-tui/src/lib.rs:759`) — use VecDeque.
- Mermaid DOM id injection via `head.path` (use `CSS.escape`).
- Windows path edge cases in `is_safe_repo_relative` (device paths, drive-relative, null bytes).
- Stale "v60.6 reversibility" copy text in ContextPane confirm dialog.
- Several `Regex::new` calls inside hot error paths instead of `OnceLock`.
- `model_badge_width` uses `chars().count()` (no unicode-width).
- `read_file.rs:120` reserves 64 KiB even for tiny `take` values.
- `audit.rs:233` `APPEND_LOCK` is process-wide static `Mutex` — POSIX `O_APPEND` saves cross-process appends ≤4 KiB.
- Tag-pinned third-party actions in CI.
- Multiple `let _ = events.send(...)` sites blind the codebase to broadcast lag.

---

## Cross-cutting themes — worth their own bundles

1. **Cancellation token threading.** `CancellationToken` exists in §2.5 but doesn't reach `ToolContext`, `Dispatcher::dispatch`, the HTTP path, or `sink_handle`. Aborts on shutdown rely on runtime drop rather than cooperative abort.
2. **`let _ = events.send(...)` pattern (~30 sites in dispatcher).** Correct as written but blinds the codebase to broadcast lag. A single tracing-instrumented helper would surface it.
3. **Atomic-write discipline is 80% there.** Most call sites use `NamedTempFile::persist` + dir fsync; the exceptions (`LspApprovals::save`, audit appenders, `write_with_sync` in staging) are the durability liabilities listed above.
4. **Test-seam leak.** Public `with_*_for_test` builders + `#[allow(dead_code)]` + `#[doc(hidden)]` is convention-only — no compile-time enforcement. A `#[cfg(any(test, feature = "test-seams"))]` gate would put teeth into the rule.
5. **Wire-label agreement.** The crate has `wire_label()` ↔ serde tests on most cross-boundary enums (L-D-5), but `schemas/protocol/overhead.v1.json` slipped — worth a one-time sweep that every schema enum is asserted against the producing Rust enum in tests.

---

## Suggested next moves — three file-disjoint bundles (L-D-2 pattern)

### v60.28 — secrets & egress hardening (H1–H8)

- Rotate `.envrc` key and move to keychain / 1Password CLI.
- Redact adapter `Auth` / `Provider` error bodies; cap to 256 B.
- Switch non-stream HTTP path to `bytes_stream()` + 32 MiB cap.
- Bound SSE per-event accumulator alongside line buffer.
- `.redirect(Policy::none())` on cred-bearing reqwest clients.
- Add MCP HTTP egress allowlist + per-`call_tool` audit row.
- Gate `swap_adapter` behind consent modal + base_url allowlist.

### v60.29 — liveness & durability (H9–H12)

- Thread `CancellationToken` into `ToolContext` + add per-tool deadline.
- Install `tokio::signal::ctrl_c` race in `main.rs::run_run`.
- Fix `write_with_sync` ordering (sibling temp + fsync before rename).
- Hoist `canonicalize` out of the `file_watcher` parking_lot lock.

### v60.30 — TUI / Frontend hygiene (H13–H15 + UI Mediums)

- `KeyEventKind::Press` filter in TUI input loop.
- TerminalGuard ordering + panic hook for terminal restore.
- ANSI sanitiser for all LLM/tool/file content rendered into ratatui spans.
- Mermaid `securityLevel: 'strict'` + DOM id `CSS.escape`.
- Image-src normaliser (reject `..`, require markdown form).
- Modal `inert` on DiffPane during `concurrentEditModal`.

---

## Reviewer notes

- All seven reports are preserved in the agent transcripts under `/private/tmp/claude-502/.../tasks/`.
- `cargo audit` could not be run in this session (sandbox-blocked). Recommend confirming CI green before any release-tagged bundle ships.
- `npm audit` for the Svelte frontend also not run; the deps are very current (Svelte 5.55.7, Vite 8.0.13, mermaid 11.15.0) so odds of high-severity advisories are low, but wiring `npm audit --audit-level=high` into `make check` would close the loop.
