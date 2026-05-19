# Plan — Low-severity hygiene sweep from `deep_code_scan_v60.27.md`

Date: 2026-05-19. Source: the v60.27 audit (Low section, ll. 115–132). The 16 Highs shipped as v60.28–v60.31; the 31 Mediums + cross-cutting themes are queued as v60.32–v60.35 in `plan_medium_severity_fixes.md`. This plan picks up the ~10 explicit Low items (the audit cites "roughly 50" but only highlights these) and one catch-all for the unenumerated remainder.

The audit file itself is **not** in the working tree — it was untracked in commit `2bf8e64` to keep the BYOM rule clean. The recovered copy lives at `/tmp/deep_code_scan_v60.27.md` (regenerable via `git show 2bf8e64^:tasks/deep_code_scan_v60.27.md`).

Items are numbered **L01–L11** for traceability in commit messages and PR descriptions.

## Already closed — do not re-plan

Three Lows from the audit are resolved by work that landed after v60.27. Listed here so reviewers don't open redundant PRs:

- **`deep_code_scan_v60.27.md:121` — double `canonicalize` in `file_watcher::track`.** Closed by v60.29 H12 — `canonicalize_for_track` is now the single call point (`crates/atelier-core/src/file_watcher.rs:156-164`).
- **`:122` — missing panic hook for terminal restore.** Closed by v60.30 H13 — `install_panic_hook()` in `crates/atelier-tui/src/lib.rs` chains the previous hook + disables raw mode + leaves alt-screen.
- **`:124` — Mermaid DOM id injection via `head.path`.** Closed by v60.30 — `safeDomId()` in `InlineRenderers.svelte` whitelists alphanumerics + `_-` before reaching `querySelector` / `mermaid.render`. Stricter than the audit's `CSS.escape` suggestion.

## Duplicates of medium-plan items — do not re-plan

- **`:131` — tag-pinned third-party actions in CI.** Covered by **M13** (SHA-pin sweep) in v60.33.
- **`:132` — `let _ = events.send(...)` blinds the codebase to broadcast lag.** Covered by **M29** (`try_emit` helper + 30-site replacement) in v60.35.

## Standing gates (all PRs)

Same as the High / Medium plans:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p <touched crate>` (and `-p atelier-core` if any shared module is touched)
- `make check` if schemas / fixtures / rig code is touched

Each item below adds a **targeted** verification on top of these — the smallest test that would have caught the issue, except where the item is a pure-deletion or a pure-rename (see L09).

---

## v60.36 — Low-severity hygiene sweep (L01–L11)

This bundle is *not* file-disjoint with the v60.32–v60.35 bundles — Low items touch some of the same files as M03, M18, M16, etc. Sequence v60.36 **after** the medium bundles land so the touched-line numbers stay stable. Within v60.36, the four sub-bundles below are file-disjoint and can be developed concurrently per L-D-2 on separate worktrees.

### Sub-bundle A — `atelier-core` hot-path & hygiene (L01, L05, L08, L09, L10)

Touches: `crates/atelier-core/src/{dispatcher.rs,adapter/anthropic.rs,adapter/openai_compat.rs,adapter/mod.rs,tools/read_file.rs,audit.rs}`.

#### L01 — Drop the needless clone in `AutoApprove::approve`

- File: `crates/atelier-core/src/dispatcher.rs:428-450` (the `AutoApprove` impl of `ApprovalSurface::approve`).
- The audit flagged a clone of `pending` (or equivalent) inside the hot dispatch path that's never observed after the function returns.
- Fix: return / consume by value where possible; if the borrow checker forces a clone, factor the field that's actually needed (usually the `commit_id`) and clone only that.
- **Verify:** `cargo clippy --workspace --all-targets -- -D warnings -W clippy::redundant_clone` is clean on the file. No behavioural test needed — the existing `dispatcher::approval_tests` cover the surface.

#### L05 — `Regex::new` in hot error paths → `OnceLock`

- Files: `crates/atelier-core/src/adapter/mod.rs:316`, `crates/atelier-core/src/adapter/anthropic.rs:616,631` (and any sibling site in `openai_compat.rs` that the audit listed; grep for `regex::Regex::new` and review).
- Current behaviour: each `AdapterError::ContextOverflow` / token-limit parse compiles the regex on the failure path. Failure paths are exactly where we *don't* want a fresh compile.
- Fix: replace with `static FOO: OnceLock<Regex> = OnceLock::new();` + `.get_or_init(|| Regex::new(...).expect(...))`. Document that `.expect` is acceptable because the regex literal is compile-time-constant.
- **Verify:** new test in `adapter::regex_cache_tests` calls each accessor twice in succession and asserts the second call returns the same `&'static Regex` pointer (use `Arc::ptr_eq` or stable `as *const`).

#### L08 — `read_file` over-reserves on tiny `take` values

- File: `crates/atelier-core/src/tools/read_file.rs:120` (the `Vec::with_capacity` site that hard-codes 64 KiB).
- Current behaviour: a `read_file` invocation with `byte_cap = 256` still pre-allocates 64 KiB. For workloads that probe many small files (LSP-style "show me line N"), that's a real working-set hit.
- Fix: `Vec::with_capacity(byte_cap.min(64 * 1024))`. Document inline that the cap exists to avoid one realloc for typical reads but mustn't dominate the requested size.
- **Verify:** new test in `tools::read_file::tests` reads a 1 KB file with `byte_cap = 256`; assert `contents.capacity() <= 256 + small_slack` (where `small_slack` accounts for Vec growth policy — pick a conservative `512`).
- **Pairs with M18:** if M18 (`byte_len` vs `total_byte_len`) lands first, rebase this on top of the new field layout.

#### L09 — `audit.rs` `APPEND_LOCK` is process-wide

- File: `crates/atelier-core/src/audit.rs:233`.
- Current behaviour: a single static `Mutex` serialises *all* audit writers in the process. POSIX `O_APPEND` guarantees atomic append for writes ≤ `PIPE_BUF` (typically 4 KiB) — the in-process lock is redundant on Unix for the typical row.
- Fix: keep the in-process lock as the fast path (it doubles as a write-batching point), but document the `O_APPEND` invariant inline so a future reviewer doesn't widen the lock's scope thinking it's load-bearing. Add a debug-assert that audit rows stay under 4 KiB; route over-sized rows to a slow path that still takes the lock.
- **Pairs with M16:** if M16 (`sync_all` after each row) lands first, rebase this on top of the new write loop.
- **Verify:** new test in `audit::concurrency_tests` spawns N threads each appending one row; asserts every row appears intact in the file and that no row is interleaved with another. (This is largely already covered; the new bit is the size assertion.)

#### L10 — Wire-label sweep already covered, but the **audit** missed `protocol/overhead.v1.json`

- Not a fresh code change — this is a process item. The cross-cutting theme #5 in the audit (and M08 in the medium plan) already mandates `additionalProperties: false` everywhere. While reviewing M08, also extend the schema-enum-vs-Rust-enum assertion test to cover *every* `wire_label()`-bearing enum in the codebase, not just the ones currently tested.
- **Verify:** rolled into M08's verification — no separate test needed.

### Sub-bundle B — `atelier-cli` runner hygiene (L02, L03)

Touches: `crates/atelier-cli/src/runner.rs`.

#### L02 — `latency_f64` double-projection

- File: `crates/atelier-cli/src/runner.rs:1607-1625`.
- Current behaviour: `latency_f64 = response.usage.latency_ms.map(|ms| ms as f64)` is computed, then `ModelCostPolicy::LatencyWeighted` re-projects it, then `latency_ms: latency_f64` reads it again. The intermediate is fine; the audit flag is that the same `as f64` cast happens on two paths inside the `match`.
- Fix: factor the cost calculation into a helper `fn weighted_local_cost(latency_ms: u32) -> f64` so both arms use the same projection. Drop the `latency_f64` local if it's then only used in one place.
- **Verify:** `cargo test -p atelier-cli runner::cost_ledger_tests` continues to pass with byte-identical ledger output (use the existing snapshot test).

#### L03 — `.unwrap_or_default()` masks serde regressions in runner

- File: `crates/atelier-cli/src/runner.rs:1793` (and any sibling sites the audit didn't enumerate — grep for `\.unwrap_or_default\(\)` in this file).
- Current behaviour: a failed serde projection silently substitutes `Default::default()`. If the upstream type ever drops or renames a field, the runner emits an empty value with no log.
- Fix: replace with `match result { Ok(v) => v, Err(e) => { tracing::warn!(error = %e, "serde projection failed"); Default::default() } }`. Don't fail the run — the audit's severity is Low, not Medium, because the existing tests cover the happy path; the warn is purely a regression beacon.
- **Verify:** new test in `runner::serde_projection_tests` constructs a malformed payload through the existing fixture infrastructure and asserts the warn log is emitted exactly once (use `tracing-test`'s `traced_test` macro, already a dev-dep elsewhere in the workspace).

### Sub-bundle C — `atelier-gui` Rust + Svelte hygiene (L04, L06, L07)

Touches: `crates/atelier-gui/src/lib.rs`, `crates/atelier-gui/ui/src/lib/components/ContextPane.svelte`.

#### L04 — `.unwrap_or_default()` in GUI bridge

- File: `crates/atelier-gui/src/lib.rs:1003, 1010, 1013, 1028`.
- Same shape as L03 but on the renderer-bound projection of `Event`. Same fix (warn + default), same severity.
- **Verify:** the existing GUI bridge tests already round-trip every variant; add one test that injects a corrupted event row and asserts the warn fires + the default lands at the renderer.

#### L06 — Windows path edge cases in `is_safe_repo_relative`

- File: `crates/atelier-gui/src/lib.rs:222-269`.
- Current behaviour: the predicate rejects `..` and absolute paths but doesn't catch Windows-specific escapes — device paths (`\\.\C:`), drive-relative paths (`C:foo` — no slash; resolves against the drive's CWD), or null bytes (which Windows treats as terminators inside some APIs).
- Fix: extend the predicate with explicit rejection for `\\?\`, `\\.\`, `<letter>:` (with or without `\`), and any embedded `\0`. Add the same checks to whatever sibling helper validates pinned-context paths, if it exists.
- **Verify:** extend `is_safe_repo_relative_accepts_normal_paths_rejects_escapes` (`lib.rs:1732`) with the new rejection cases. Run the test on the Windows CI runner too (if the matrix already covers Windows; otherwise note that this is best-effort coverage on Linux/macOS).

#### L07 — Stale "v60.6 reversibility" copy in `ContextPane.svelte`

- File: `crates/atelier-gui/ui/src/lib/components/ContextPane.svelte:206`.
- Current copy: "summary added as pinned memory card; reversible in v60.6." (v60.6 is the *past*; the affordance landed two months ago.)
- Fix: rewrite to user-facing tense — "summary added as a pinned memory card; expandable from the Memory panel."
- **Verify:** visual check via `npm run dev` + opening the confirm dialog; no test needed.

### Sub-bundle D — `atelier-tui` hygiene (L11)

Touches: `crates/atelier-tui/src/lib.rs`.

#### L11 — `Vec::remove(0)` event-log pop is O(n); `model_badge_width` is char-count, not column-count

Two unrelated TUI items batched into one PR because they share a file.

- **L11a:** `crates/atelier-tui/src/lib.rs:765` — `self.events.remove(0)` is O(n) in event-log length and runs on every event. Replace `events: Vec<...>` with `VecDeque<...>` so `pop_front()` is O(1). Touch every read site (the audit'll have flagged only the worst one); most should swap cleanly because the surrounding code only iterates.
- **L11b:** `crates/atelier-tui/src/lib.rs:2164-2173` — `model_badge_width` sums `chars().count()` across the four label fragments. For CJK / emoji / combining marks, `chars().count()` is not the rendered column width, so the right-side footer can overflow or under-fill. Switch to the `unicode-width` crate's `UnicodeWidthStr::width` (already in the workspace's dep graph via ratatui).
- **Verify:** for L11a, replace the existing event-log push/pop benchmark with a `cargo test` that adds 10k events and asserts the pop is constant-time (run 1000 pops, assert wall-clock < 50 ms — generous, just guards against an accidental O(n)). For L11b, extend `model_badge_width_matches_visible_chars` (`lib.rs:3746`) with a CJK + emoji case (e.g. `"中文"` is 4 columns, not 2 chars).

### Bundle gate

`cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && make check`.

---

## "The unenumerated rest"

The audit cites "roughly 50 Low items across the seven reports" but only highlights the 13 listed above. The remaining ~37 items live in the agent transcripts under `/private/tmp/claude-502/.../tasks/` (per the audit's reviewer notes, l. 178), which is **not durable** — that path is the OS tempdir from the audit session and almost certainly gone.

Two options for the unenumerated rest:

- **(a) Re-scan.** A fresh `/deep-scan` against `main` would re-surface them (and probably a different set, since the codebase has moved by four bundles). Cleanest path; cost is roughly one Claude session.
- **(b) Leave them on the floor.** Lows are by definition non-load-bearing. If they were important they'd have been promoted.

**Recommendation:** (a), but defer until v60.36 ships. Re-scanning before then means triaging Lows that may already be closed by L01–L11. Schedule the re-scan as part of the v61.0 prep window.

---

## Sequencing & risk

- **Do not start v60.36 before v60.32–v60.35 ship.** Several Lows touch the same files as Mediums (L02 / L03 / L04 share files with M03 / M18 / M16); landing them in the wrong order means rebasing every PR.
- Within v60.36, **Sub-bundles A / B / C / D are file-disjoint** and can be developed concurrently on separate worktrees. Parallel-bundle release pattern same as v60.28–v60.31.
- Risk is **low across the board** — these are hygiene items. The one item with non-trivial user-visible surface is **L11a** (Vec → VecDeque); the event log is observed by the TUI render loop, so confirm no caller indexes by integer position before merging.
- Each sub-bundle ends with: green CI, `CHANGELOG.md` entry, tag, one-line digest in `tasks/todo.md`.

## Out of scope

- The 5 Informationals from the audit — backlog only.
- The Lows already closed by v60.29 / v60.30 (canonicalize, panic hook, Mermaid DOM id) — see "Already closed" header.
- The Lows already covered by the medium plan (M13 SHA-pin, M29 `try_emit`) — see "Duplicates" header.
- The unenumerated ~37 Lows — see "The unenumerated rest" above; defer to a re-scan in v61.0 prep.
