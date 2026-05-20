# Plan — Medium-severity fixes from `deep_code_scan_v60.27.md`

Date: 2026-05-19. Source: the v60.27 audit (Mediums section + open cross-cutting themes + reviewer notes). The 16 Highs (+ five UI Mediums) shipped as v60.28–v60.31; this plan picks up the remaining ~26 Mediums plus three open cross-cutting themes and two supply-chain reviewer notes. Total: 31 items across four file-disjoint bundles.

Items are numbered **M01–M31** for traceability in commit messages and PR descriptions.

## Standing gates (all bundles)

Same as the High-severity plan — every PR must show:

- `cargo fmt --check`
- `cargo clippy -- -D warnings` (default targets; `--all-targets` carries a pre-existing v60.29 dead-code warning on `Runner::with_external_cancel` that is out of scope here — fix it as part of M06's test-seam gate sweep if it lands on the same path)
- `cargo test -p atelier-core` (and `-p atelier-cli` / `-p atelier-gui` / `-p atelier-tui` where touched)
- `make check` (rig: schemas → artifacts → rig-tests → workload dry-run)

Each item below adds a **targeted** verification on top of these — the smallest test that would have caught the issue.

---

## v60.32 — Runner correctness + test-seam discipline (M01–M06)

Touches: `crates/atelier-cli/src/{main.rs,runner.rs}`, `crates/atelier-cli/src/bin/conformance_status.rs`, `crates/atelier-gui/src/lib.rs` (M05 only — read-only consumer of the binary's output).

### M01 — `OPENAI_BASE_URL` should not silently override CLI / profile

- File: `crates/atelier-cli/src/main.rs:580-583`.
- Current behaviour: `std::env::var("OPENAI_BASE_URL")` is consulted after CLI flags + resolved profile, inverting the documented `CLI > profile > defaults > env` precedence.
- Fix: only read `OPENAI_BASE_URL` when *no* CLI `--base-url` and *no* profile `base_url` is present. Emit a one-shot `tracing::info!` recording which layer won so an operator can diagnose surprise origins.
- **Verify:** new test in `cli::base_url_precedence_tests` constructs three scenarios — (CLI flag wins over env, profile wins over env, env wins over default) — and pins the resolved value at each layer.

### M02 — `AwaitingUser` final state should signal non-zero from CLI

- File: `crates/atelier-cli/src/main.rs:339-450` (the `run_run` exit-code resolution).
- Current behaviour: a session that ends in `AwaitingUser` (agent stalled, no `claimed_done`, no tool call) returns exit 0, so CI gates can't distinguish "completed" from "stalled".
- Fix: map `AwaitingUser` to a distinct exit code (proposed: 6 — `Stalled`; keep 0 reserved for `Done`, 130/143 for the v60.29 signal handlers). Add to `ExitCode` enum if one exists, else literal with a comment naming the meaning.
- **Verify:** integration test in `crates/atelier-cli/tests/exit_codes.rs` scripts a Mock adapter that emits text without `claimed_done`; assert the process exits 6, not 0.

### M03 — Compact-retry must not re-send stale `messages_for_call`

- File: `crates/atelier-cli/src/runner.rs:1430-1559` (the §1 `ContextOverflow` → compact → retry path).
- Current behaviour: after the compaction mutator runs, the retry path re-sends the pre-compaction `messages_for_call` snapshot — defeating the compaction.
- Fix: re-project `messages_for_call` from the post-mutation `ContextManager`, then issue the retry. Audit the surrounding `MessagesForCall` builder for cached state that needs invalidation.
- **Verify:** new test in `runner::compact_retry_tests` runs a Mock adapter that returns `ContextOverflow` on call 1; assert call 2's payload reflects the compaction (fewer tokens than call 1's).

### M04 — `Runner::swap_adapter` is sync-shaped behind an `async fn`

- File: `crates/atelier-cli/src/runner.rs:730-767`.
- Current behaviour: declared `async` but never `.await`s. Holds `parking_lot::Mutex` guards across the false async boundary — a future caller that adds an `.await` inside the function will deadlock the executor.
- Fix: either make it `fn` (removing the `async` annotation) **or** restructure so the locks are scoped strictly above any future `.await`. Prefer the former — it's less likely to regress.
- **Verify:** clippy / unused-async lint already flags this once `async` is removed; the existing tests in `runner::swap_*_tests` continue to pass.

### M05 — `conformance_status` resolves its data file at build time

- File: `crates/atelier-cli/src/bin/conformance_status.rs:77-83`.
- Current behaviour: builds `tests/phase_b_gate/last_run.json` against `env!("CARGO_MANIFEST_DIR")` at compile time. A binary built in one workspace and run from another reads the wrong file or fails.
- Fix: resolve relative to the binary's run-time CWD (or `ATELIER_PROJECT_DIR` when set). Fall back to `CARGO_MANIFEST_DIR` only with a `--debug` flag so test runs still work.
- **Verify:** integration test that runs the built binary from a tempdir with a copied fixture; assert it finds the file via CWD, not the build-time path.

### M06 — Test-seam builders must be compile-time gated

- Files: `crates/atelier-cli/src/runner.rs:681-689, 827, 838, 892, 904` — `Runner::with_adapter_for_test`, `with_starting_strategy_override`, `with_tier1_diagnostics_for_test`, `with_degradation_window`, `with_degradation_threshold`. All currently `pub` + `#[doc(hidden)]`.
- Fix: gate under `#[cfg(any(test, feature = "test-seams"))]`; add a `test-seams` feature to `Cargo.toml` (default off) and switch existing `#[cfg(test)]` callers to enable it on the `dev-dependencies` side. Production consumers can no longer pin stale strategies.
- This also closes cross-cutting theme #4 from the audit ("Test-seam leak").
- While here, fix the pre-existing v60.29 `--all-targets` clippy warning on `Runner::with_external_cancel` the same way.
- **Verify:** `cargo build --no-default-features -p atelier-cli` succeeds; `cargo test -p atelier-cli` continues to pass (it pulls in the feature via dev-deps); `cargo clippy --all-targets -- -D warnings` is now clean.

### Bundle gate

`cargo test -p atelier-cli && cargo build --no-default-features -p atelier-cli && cargo clippy --workspace --all-targets -- -D warnings` plus the new `crates/atelier-cli/tests/exit_codes.rs`.

---

## v60.33 — Schemas, rig, CI hygiene (M07–M14)

Touches: `schemas/protocol/overhead.v1.json`, `schemas/audit/egress.v1.json`, `tests/validate_artifacts.py`, `tests/workload/runner.py` (or wherever the workload runner lives), `.gitignore`, `.github/workflows/*.yml`.

### M07 — Add `.atelier/sessions/` to `.gitignore`

- File: `.gitignore`.
- Add `.atelier/sessions/` (recursive). One `git add .` after a local run currently stages session UUIDs + partial completions.
- **Verify:** new pytest assertion in `tests/test_runner.py` that walks `.atelier/sessions/<uuid>/` and confirms `git check-ignore` returns matched (skip on CI where the dir won't exist).

### M08 — `schemas/protocol/overhead.v1.json` needs `additionalProperties: false`

- File: `schemas/protocol/overhead.v1.json`.
- The only schema in `schemas/` without the flag; typos pass validation.
- **Verify:** extend the existing schema lint in `tests/test_schemas.py` to assert *every* object schema sets `additionalProperties: false` (one-time sweep), then add a fixture with a typo'd field and assert validation rejects it.

### M09 — `schemas/audit/egress.v1.json` missing `kind` discriminator

- File: `schemas/audit/egress.v1.json`.
- Sibling schemas (`mcp_egress.v1.json`, `subprocess_egress.v1.json`) carry a `kind: "mcp-http-request" | "subprocess"` discriminator; this one doesn't, so the union isn't mutually exclusive at validation time and a malformed row can pass twice.
- Fix: add `kind` as a required enum field on the base schema; bump consumers that build the row to populate it.
- **Verify:** new fixture in `tests/audit/` that's valid against neither sibling once `kind` is required; pin the rejection.

### M10 — `validate_artifacts.py` must fail loud on unmatched paths

- File: `tests/validate_artifacts.py`.
- Current behaviour: a JSON artifact path the rule table doesn't match is silently skipped. New artifacts can land unvalidated.
- Fix: unmatched paths produce a non-zero exit with the offending path printed. Allow an explicit `# unvalidated:` annotation in the rule table for genuinely free-form artifacts.
- **Verify:** add an artifact with a name not in the rule table; assert `make artifacts` fails. Add the annotation to the rule table; assert it now passes.

### M11 — Workload runner: `shell=False` + process-group kill on timeout

- File: `tests/workload/runner.py` (or wherever `subprocess.run(cmd, shell=True)` lives).
- Current behaviour: `shell=True` allows shell metacharacters in fixture commands; no process-group kill on timeout leaks grandchildren on Unix.
- Fix: switch to argv-list `subprocess.run([...], shell=False)`. Wrap the child with `start_new_session=True` (Unix) and `os.killpg(os.getpgid(p.pid), SIGKILL)` on timeout. Match the v25 P1 subprocess discipline already used in `crates/atelier-core/src/subprocess.rs`.
- **Verify:** new rig test that spawns a fixture which itself spawns a long-lived grandchild; force a timeout; assert no descendants survive after the test returns.

### M12 — Nightly workflows must rebase before pushing

- Files: `.github/workflows/nightly_phase_a_gate.yml` (and the two sibling nightlies — Phase B conformance + protocol-overhead).
- Current behaviour: three nightlies commit to `main` without `git pull --rebase`. A ~90-min Phase A run schedules at xx:30; a Phase B run scheduled 30 min later can overlap and the second push silently loses the first's artifact.
- Fix: `git pull --rebase origin main` before `git commit`; on rebase conflict, abort the workflow with a clear error so the operator can investigate. Optionally serialise via `concurrency: { group: nightly-artifact-commits, cancel-in-progress: false }`.
- **Verify:** add a YAML lint assertion in `tests/test_ci.py` (or a `make ci-lint` target) that every nightly workflow contains both the `pull --rebase` step and a `concurrency:` block.

### M13 — SHA-pin third-party GitHub Actions

- Files: every `uses: actions/checkout@v4` / `uses: actions/setup-python@v5` / similar across `.github/workflows/*.yml`.
- Current behaviour: tag-pinned. Tags are mutable; the GitHub hardening guide requires SHA pins.
- Fix: replace each tag with the full SHA, leave the human-readable version in a trailing comment (`@<sha> # v4.2.1`). Use `gh api repos/<owner>/<repo>/git/refs/tags/<tag>` to fetch each SHA once.
- **Verify:** new `tests/test_ci.py::test_actions_are_sha_pinned` asserts every `uses:` value matches `^[a-z0-9_-]+/[a-z0-9_-]+@[a-f0-9]{40}( |$)`.

### M14 — `check.yml` needs a top-level `permissions:` block

- File: `.github/workflows/check.yml`.
- Current behaviour: no `permissions:`, so the workflow inherits the repo default (typically too broad).
- Fix: add `permissions: { contents: read }` at the top level; opt back into write permissions per-job only where needed.
- **Verify:** part of the same `tests/test_ci.py` sweep — assert every workflow file declares an explicit top-level `permissions:` block.

### Bundle gate

`make check && python -m pytest tests/test_ci.py tests/test_schemas.py -v`.

---

## v60.34 — Durability, audit polish, hardening (M15–M26)

Touches: `crates/atelier-core/src/{audit.rs,subprocess.rs,memory.rs,dispatcher.rs}`, `crates/atelier-core/src/lsp/{approval.rs,typescript.rs}`, `crates/atelier-core/src/mcp/stdio_launcher.rs`, `crates/atelier-core/src/tools/read_file.rs`, `crates/atelier-core/src/adapter/{anthropic.rs,openai_compat.rs}`, `crates/atelier-gui/src/lib.rs`.

### M15 — `LspApprovals::save` needs fsync + dir fsync

- File: `crates/atelier-core/src/lsp/approval.rs:69-94`.
- Current behaviour: `NamedTempFile::persist` writes the file but doesn't `sync_all` the contents or `fsync_dir_best_effort` the parent. Other atomic-write helpers (`init.rs`, `persistence.rs`, post-v60.29 `staging.rs`) do both.
- Fix: align with the v60.29 H11 pattern — `sync_all` on the temp file before `persist`, then `fsync_dir_best_effort` the parent.
- **Verify:** new test in `lsp::approval::durability_tests` mirrors the v60.29 H11 pattern: inject a panic between persist and dir-fsync; assert the on-disk approval is either absent or fully written.

### M16 — Audit appenders must `sync_all`

- File: `crates/atelier-core/src/audit.rs:269-277`.
- Current behaviour: appenders flush but never `sync_all`. §11 sandbox-egress + §12 MCP-egress audit rows can be lost on crash, which defeats the entire point of the audit log.
- Fix: `sync_all` after each appended row. (The append batch is small and infrequent enough that the syscall cost is acceptable.) Document the tradeoff inline so the next reviewer doesn't undo it for "performance".
- **Verify:** new test in `audit::durability_tests` writes a row, simulates a process crash before `Drop`, and asserts the row is readable on the next process start.

### M17 — Subprocess pipe-take must not panic in production

- File: `crates/atelier-core/src/subprocess.rs:228-244`.
- Current behaviour: `.expect("piped stdout was requested above")` panics if a future refactor changes the `Stdio::piped()` calls upstream. A production panic in the subprocess machinery aborts a session ungracefully.
- Fix: replace `.expect(...)` with a typed `SubprocessError::PipePlumbingChanged` returned as `Err`; the dispatcher already routes tool errors to the recovery surface.
- **Verify:** new test in `subprocess::pipe_handling_tests` constructs a `Command` with `Stdio::inherit()` for stdout and asserts the typed error rather than a panic.

### M18 — `read_file.byte_len` must reflect what's actually returned

- File: `crates/atelier-core/src/tools/read_file.rs:128`.
- Current behaviour: reports the file's *total* size while `contents` is truncated. A UI computing `contents.len() == byte_len` mis-reads "truncated" as "complete".
- Fix: return `byte_len = contents.len()`; add a separate `total_byte_len: u64` field carrying the file's full size for callers that need it.
- **Verify:** new test in `tools::read_file::tests` reads a 10 MB file with a 4 KB cap; asserts `byte_len == 4096` and `total_byte_len == 10_485_760`.

### M19 — `memory::sanitize_filename` must reject empty / pure-dot strings

- File: `crates/atelier-core/src/memory.rs:370-380`.
- Current behaviour: returns empty string or `"."` for inputs like `""`, `"..."`, `"/./"` — both of which the filesystem treats specially and produce surprise files.
- Fix: after sanitisation, reject anything that's empty, all-dots, or `..`. Return `Result<String, MemoryError::InvalidFilename { input }>`.
- **Verify:** new property test in `memory::sanitiser_tests` asserts `sanitize_filename` never returns the strings `""`, `"."`, `".."`, or any sequence matching `^\.+$`.

### M20 — MCP protocol version match must be typed

- File: `crates/atelier-core/src/mcp/stdio_launcher.rs:414-422`.
- Current behaviour: `format!("{:?}", ...).contains(SUPPORTED_PROTOCOL_VERSION)` — brittle Debug-string match that breaks if `rmcp` ever changes its `Debug` impl.
- Fix: bind the version from the handshake response into a typed `ProtocolVersion` enum / newtype; match on it directly. Cite the `rmcp::protocol::ProtocolVersion` symbol path.
- **Verify:** new test in `mcp::stdio_launcher::version_tests` round-trips a known version through the parser; asserts unknown versions return `McpLaunchError::UnsupportedProtocol { reported }`.

### M21 — `truncate_to_bytes` overshoots its cap by 3 bytes

- File: `crates/atelier-core/src/lsp/typescript.rs:112-124`.
- Current behaviour: truncates at the cap, then appends `"…"` (3 bytes UTF-8). Output is `cap + 3`; cap is a lie.
- Fix: truncate at `cap - 3` (or `cap - '…'.len_utf8()`); if `cap < 3`, omit the ellipsis entirely.
- **Verify:** new test in `lsp::typescript::truncate_tests` asserts the returned length is always `≤ cap` across `cap = 0, 1, 2, 3, 4, 1000, …`.

### M22 — `dispatcher::extract_read_paths` over-tracks on empty grep arg

- File: `crates/atelier-core/src/dispatcher.rs:957-996`.
- Current behaviour: when the grep tool is invoked with an empty path arg it adds the workspace root to the file-watcher read-set. Every save anywhere in the workspace then fires `FilesChanged`.
- Fix: skip the read-set update when the resolved path is the workspace root *and* the arg was empty / "." — the user didn't actually scope the read.
- **Verify:** new test in `dispatcher::read_set_tests` invokes the grep tool with arg "" against a temp workspace; asserts the read-set is empty afterwards.

### M23 — LSP install + HTTP MCP server must honour `ENV_PASSTHROUGH`

- Files: `crates/atelier-core/src/lsp/install.rs` (if exists; else wherever `npm install` / `pip install` is shelled out) and `crates/atelier-core/src/mcp/http_launcher.rs`.
- Current behaviour: subprocess + HTTP MCP server lifecycle inherit unbounded PATH/env from the parent. Hostile or polluted env in the parent leaks into a tool's exec environment.
- Fix: reuse the `subprocess::ENV_PASSTHROUGH` allowlist already enforced by the shell tool (v25 P1). Document that LSP installs may need an opt-in extension for `NPM_*` / `PIP_*` vars.
- **Verify:** new test in `lsp::install_tests` sets a sentinel env var the allowlist doesn't include; asserts the child process doesn't see it.

### M24 — Adapter chat-retry must handle compaction state

- Files: `crates/atelier-core/src/adapter/anthropic.rs`, `crates/atelier-core/src/adapter/openai_compat.rs`.
- Current behaviour: retry path clones `messages_for_call` from the pre-error snapshot. If the error triggered a compaction in the runner (v60.5 path), the retry payload is stale.
- Fix: surface compaction via a typed signal the runner already emits; the adapter retry path now treats compaction as "do not retry; bubble the error so the runner re-projects messages". Pairs with M03's runner-side fix.
- **Verify:** new test in `adapter::anthropic::retry_tests` scripts a 429 followed by a successful response; asserts the retry payload is byte-equal to the original (no compaction case) and is re-projected when compaction is signalled.

### M25 — GUI `compact_context_items` race vs `swap_adapter`

- File: `crates/atelier-gui/src/lib.rs:660-670`.
- Current behaviour: `compact_context_items` is the only renderer-visible path that observes the *post-swap* adapter. The renderer is told via `AdapterSwapped` that the swap is live before the Runner sees it; a compaction issued in that window calls into the old adapter.
- Fix: emit `AdapterSwapped` only after the Runner-side swap has been observed (use the v60.31 consent gate's oneshot to pair the events). Alternatively: stamp the compaction call with the model id it expected and reject if the live adapter's id has drifted.
- **Verify:** new test in `gui::adapter_swap_tests` issues swap + compact in tight sequence; asserts the compaction sees the post-swap adapter or surfaces a typed `ModelDrift` error rather than calling the old one.

### M26 — `submit_approval` failure mode must be visible

- File: `crates/atelier-core/src/dispatcher.rs:1147-1152`.
- Current behaviour: if the dispatcher is dropped (cancellation) between the renderer's submit and the `remove` call, `submit_approval` silently returns `false`. The user's accept-set is lost on the floor with no log.
- Fix: emit `tracing::warn!("submit_approval: dispatcher gone; accept-set discarded", commit_id = %commit_id)` before the return. Surface the same condition to the renderer via a typed error so the UI can render a toast.
- **Verify:** new test in `dispatcher::submit_approval_tests` drops the dispatcher between submit and remove; asserts the warn log fires and the typed error reaches the caller.

### Bundle gate

`cargo test -p atelier-core && cargo test -p atelier-gui` plus the new durability tests in `lsp::approval::durability_tests` / `audit::durability_tests`.

---

## v60.35 — Supply-chain gates + broadcast-lag instrumentation (M27–M31)

Touches: `Makefile`, `.github/workflows/check.yml`, `crates/atelier-core/src/session.rs` (helper), call sites across `crates/atelier-core/src/dispatcher.rs` and friends.

### M27 — `make audit` target (cargo audit + npm audit)

- File: `Makefile`.
- Add a new phony target `audit` that runs `cargo audit --deny warnings` and `cd crates/atelier-gui/ui && npm audit --audit-level=high`. Both must exit 0 for the target to succeed.
- **Verify:** `make audit` runs locally; both stages exit 0 on a clean tree.

### M28 — Wire `make audit` into CI

- File: `.github/workflows/check.yml`.
- Add a `audit` job that depends on `setup` and runs `make audit`. Cache the cargo-audit binary install across runs.
- **Verify:** the workflow has a new green check on PRs; deliberately bumping a dep to a known-vulnerable version on a scratch branch turns the check red.

### M29 — Broadcast-lag instrumentation helper

- Files: `crates/atelier-core/src/session.rs` (helper) + ~30 call sites that today read `let _ = events.send(...)`.
- Add a `try_emit(bus: &broadcast::Sender<Event>, ev: Event)` helper that increments a `BROADCAST_LAGGED` counter (`AtomicU64`) when send returns `Err(SendError(_))`, plus a `tracing::warn!` on the first lag in any 1s window.
- Replace the ~30 `let _ = events.send(...)` sites with `try_emit(&bus, ...)`. This is the cross-cutting theme #2 from the audit.
- **Verify:** new test in `session::broadcast_tests` saturates the bus past the channel capacity, then asserts `BROADCAST_LAGGED.load()` is non-zero.

### M30 — Cross-cutting atomic-write sweep

- Pure documentation pass: extend `crates/atelier-core/src/init.rs`'s `atomic_write` doc comment to enumerate every atomic-write site in the workspace (staging, persistence, init, LspApprovals after M15, audit after M16). Make it the canonical reference for the pattern. Cross-cutting theme #3 closure.
- **Verify:** no code change; the existing tests stay green.

### M31 — Reviewer-noted `cargo audit` + `npm audit` documented in CONTRIBUTING

- File: `CONTRIBUTING.md` (or wherever the contribution checklist lives).
- Add a line under "Before opening a PR": *"Run `make audit` and confirm both gates are green."*
- **Verify:** trivially reviewable.

### Bundle gate

`make audit && make check`.

---

## Sequencing & risk

- The four bundles are file-disjoint per L-D-2 and can be developed concurrently on separate worktrees. Parallel-bundle release pattern same as v60.28–v60.31.
- Start with **v60.33 (schemas / CI)** — smallest diffs, highest CI gate value, no Rust code touched.
- **v60.32 (Runner correctness)** has the highest user-visible behaviour change risk (M02's exit-code change). Coordinate with anyone wiring atelier into CI before merging.
- **v60.34** is the largest bundle (12 items); the durability items (M15, M16) and `read_file` (M18) are most impactful for production runs. Plan two days.
- **v60.35** can ride alongside any of the others — the supply-chain gates are standalone CI bolt-ons.
- Each bundle ends with: green CI, `CHANGELOG.md` entry, tag, and a one-line digest in `tasks/todo.md`.

## Out of scope (covered elsewhere or follow-on)

- The ~50 Lows from the audit. A separate "hygiene sweep" bundle should batch them by file (one PR per touched file is usually the cleanest path).
- The 5 Informationals — backlog only.
- Phase B operator actions tracked in `tasks/todo.md`.
- `make secret-grep` — declined as redundant with `gitleaks` (which the deep-scan workflow already runs).
