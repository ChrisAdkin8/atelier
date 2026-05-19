# v60.37 — Medium-severity fixes

**Scope:** 23 findings from the 2026-05-19 deep scan, grouped into four file-disjoint bundles. Each bundle is one commit.

| Bundle | Theme | Items |
|---|---|---|
| A | Rust core/cli correctness | CORE-M1 atomic-write discipline; CORE-M2 config-loader size caps; CLI-M1 compaction cost_usd; CLI-M2 overhead.json atomic write |
| B | GUI Rust + Svelte hardening | GUI-M1 host_of_url scheme; GUI-M2 OPENAI_BASE_URL bypass; UI-1..UI-5 |
| C | CI / shell hygiene | CI-3..CI-7; SH-1, SH-2 |
| D | Rig + schemas | RIG-M1..M6; SCH-M1..M7 |

---

## Bundle A — Rust core/cli correctness

### A1 — Atomic-write discipline: add `sync_all` + parent-dir `fsync` to every `NamedTempFile::persist` site

**Where:**
- `crates/atelier-core/src/mcp_config.rs:408-417`
- `crates/atelier-cli/src/memory_promote.rs:119-123`
- `crates/atelier-cli/src/instrumentation.rs:121-123` and `:194-196`
- `crates/atelier-cli/src/compaction_blob.rs:144-149`

**Why:** `persistence.rs` and `init.rs` do the full pattern (write → `sync_all` → `persist` → `fsync_dir`). The five sites above stop at `persist` — `rename(2)` is atomic for the inode change but the directory-entry update is buffered until the next natural fs sync. A power loss between `persist` and natural sync can leave the directory in its pre-rename state on stable storage.

**Fix:**
1. Promote `persistence::fsync_dir` to `atelier_core::path_safety::fsync_dir` (public). The function already exists with the right `cfg(unix)` / `cfg(windows)` guard.
2. At each site above, replace the bare `tmp.persist(...)` with: write → `tmp.as_file().sync_all()?` → `tmp.persist(target)?` → `path_safety::fsync_dir(parent)?`.

**Verify:** new unit test per site asserts the file exists after a `panic!` injected between persist and fsync_dir (gated by the existing `durability-tests` cargo feature). Existing tests stay green.

### A2 — Config-loader size caps

**Where:**
- `crates/atelier-core/src/config.rs:266` (`fs::read_to_string` on `providers.toml`)
- `crates/atelier-core/src/mcp_config.rs:291` and `:381` (`mcp_servers.toml` / `mcp_catalog.json`)
- `crates/atelier-core/src/persistence.rs:365`, `:437` (`session.json` / `recovery_log`)
- `crates/atelier-core/src/dod.rs:236` (`dod.v1.json`)
- `crates/atelier-core/src/hooks.rs:236`, `:305`, `:450` (hook manifests)
- `crates/atelier-core/src/adapter/model_profile.rs:273` (probe-on-first-use cache)

**Why:** every site reads the file fully into memory with no cap. A user-writable `.atelier/hooks/` config is the highest-risk surface — a runaway model or hostile commit could write a multi-GB JSON and OOM the agent at next startup.

**Fix:** centralise a `pub fn read_capped<P: AsRef<Path>>(path: P, max_bytes: usize) -> Result<Vec<u8>, ConfigReadError>` in a new `atelier_core::io_caps` module. Caps:
- hooks, providers, mcp_servers, mcp_catalog, dod, model_profile: **1 MiB** each
- session.json: **16 MiB** (legitimate sessions accumulate)
- recovery_log: **64 MiB** (append-only)

Open file → `metadata().len() > cap` → fast-fail with explicit error before any allocation.

**Verify:** new unit test per cap asserts a synthetic oversize file produces the explicit `ConfigReadError::TooLarge { size, cap }` rather than OOMing the test process.

### A3 — Compaction ledger cost_usd: respect `ModelCostPolicy`

**Where:** `crates/atelier-cli/src/compaction.rs:150` (`cost_usd: None` hardcoded).

**Why:** the runner's main loop applies `ModelCostPolicy::LatencyWeighted` to local/Mock model-calls. Compaction issues a real ModelCall through the same adapter but always logs `cost_usd: None`, so local-provider compaction summaries silently drift from the policy.

**Fix:** thread `ModelCostPolicy` into `compaction::compact`:
```rust
pub async fn compact(
    adapter: &dyn Adapter,
    dispatcher: &SessionDispatcher,
    workspace_root: &Path,
    session_id: &str,
    ids: Vec<String>,
    now: &str,
    cost_policy: ModelCostPolicy,    // new
) -> Result<CompactionResult, CompactionRunError>
```
Compute `cost_usd` the same way the runner's main loop does (`atelier_core::ledger::local_cost_usd` for `LatencyWeighted`; `None` for `UnknownPending`).

Update both call sites (runner.rs, Tauri `compact_context_items` command, TUI Mutation::Compact) to pass the active policy.

**Verify:** new unit test in `tests/test_compaction.rs` asserts a Mock-adapter compaction emits a non-None `cost_usd`.

### A4 — `overhead.rs::write` atomic rewrite

**Where:** `crates/atelier-cli/src/overhead.rs:531` (`fs::write(path, json)`).

**Why:** the `protocol-overhead` nightly refreshes `tests/protocol/overhead.json` in tracked source via a raw `fs::write`. A crash between truncate and write leaves a partial file committed.

**Fix:** route through the same `NamedTempFile + sync_all + persist + fsync_dir` pattern as A1.

**Verify:** existing overhead tests pass; new test asserts a synthetic write-then-crash leaves either the new file or the prior file intact, never a partial.

---

## Bundle B — GUI Rust + Svelte hardening

### B1 — `host_of_url` scheme validation

**Where:** `crates/atelier-gui/src/lib.rs:703-723`.

**Why:** `host_of_url("localhost")` (no scheme) returns `Some("localhost")` which the allowlist accepts. `host_of_url("gopher://api.anthropic.com/x")` also returns `Some("api.anthropic.com")`. While `reqwest` will fail on non-http schemes today, this is defence-in-depth thinness — a future adapter or copy-paste of this helper could be exploited.

**Fix:** require `https?://` prefix at the top of `host_of_url`. Return `None` if absent. Also lowercase-compare schemes.

**Verify:** new unit tests assert `is_base_url_allowed(Some("localhost"))` and `is_base_url_allowed(Some("gopher://api.anthropic.com/v1"))` both return `false`; `is_base_url_allowed(Some("http://localhost:11434/v1"))` stays `true`.

### B2 — `OPENAI_BASE_URL` env fallback bypasses allowlist

**Where:** `crates/atelier-gui/src/lib.rs:744-748` (`build_swap_adapter`).

**Why:** the H2 allowlist gate checks `pending_base_url` from the wire payload only. If `provider.base_url == None`, the gate sees `None` (allowed), but `build_swap_adapter` then reads `OPENAI_BASE_URL` env and uses it verbatim. A polluted `.envrc` (e.g. from a malicious commit some dev cherry-picked) can route the next swap to an attacker-controlled host without consent.

**Fix:** at the top of `swap_adapter`, resolve the effective base_url **before** the allowlist check:
```rust
let effective_base_url = match &provider {
    SwapProviderWire::OpenAiCompat { base_url, .. } => {
        base_url.clone()
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
    },
    _ => None,
};
if !is_base_url_allowed(effective_base_url.as_deref()) { ... }
```

The consent modal should display the **resolved** URL (so the user sees what they're approving), not just the wire-format value.

**Verify:** new unit test sets `OPENAI_BASE_URL=http://attacker.test/v1`, calls `swap_adapter` with `base_url=None`, asserts `AdapterSwapRejected`.

### B3 — Modal focus trap + Escape handler + global-keydown gate (UI-1, UI-2)

**Where:** `crates/atelier-gui/ui/src/lib/components/ConcurrentEditModal.svelte`, `SwapConsentModal.svelte`, `App.svelte:83,113-129`.

**Why:** both modals render `role="dialog" aria-modal="true"` but do not focus the first action button, do not trap Tab, and do not handle Escape. The global `App.svelte` `onKeyDown` happily routes `[`/`]`/`g` to the underlying conversation while a swap is pending consent.

**Fix:**
- Each modal: `onMount` focuses the first button; `onkeydown` on the backdrop catches `Escape` and routes to the "reject" action (`pause` for concurrent-edit, `rejected` for swap consent); a `tabindex` cycle clamps focus inside the modal.
- `App.svelte::onKeyDown`: early-return if `app.concurrentEditModal != null || app.pendingSwap != null`.

**Verify:** Playwright/Vitest assertion that pressing `g` with the swap modal open does not change `app.activeTurn`; pressing Escape on either modal dispatches the reject action.

### B4 — `MetersPane` setInterval scoped to lifetime of overflow toast (UI-3)

**Where:** `crates/atelier-gui/ui/src/lib/components/MetersPane.svelte:60-64`.

**Fix:** wrap the ticker in `$effect` that depends on `lastOverflowResolution != null`; return cleanup that clears the interval when the toast goes away.

**Verify:** instrument a counter on `nowMs` writes; assert the count stops incrementing once `lastOverflowResolution` is cleared.

### B5 — `swapOptions` hydrated from `providers.toml` (UI-4)

**Where:** `crates/atelier-gui/ui/src/App.svelte:183-192` + new Tauri command.

**Fix:** add `#[tauri::command] fn list_provider_profiles() -> Vec<ProviderProfile>` that delegates to `ProvidersConfig::load`. App.svelte calls it on mount; `swapOptions` is a `$derived` of the result.

**Verify:** end-to-end test with a `providers.toml` containing a `[providers.local-codestral]` block; assert the dropdown shows it.

### B6 — `state.ts` payload runtime guards in dev (UI-5)

**Where:** `crates/atelier-gui/ui/src/lib/state.ts`.

**Fix:** add `is<EventName>Payload(p): p is <Type>` predicate per `Event` arm; in `import.meta.env.DEV`, throw on guard failure; in prod, silently coerce (existing behaviour).

**Verify:** dev-mode test sends a malformed `FilesChanged` payload; expect a thrown error referencing the missing field.

---

## Bundle C — CI / shell hygiene

### C1 — `timeout-minutes` on every job + step (CI-3)

**Fix:** add `timeout-minutes: 20` to every job in `check.yml`; `timeout-minutes: 45` per job + `timeout-minutes: 5` per gate-step in nightlies. **Verify:** new `tests/test_ci.py::test_every_job_has_timeout_minutes`.

### C2 — `concurrency:` group on `check.yml` (CI-4)

**Fix:** workflow-top `concurrency: { group: check-${{ github.workflow }}-${{ github.ref }}, cancel-in-progress: true }`. **Verify:** new `tests/test_ci.py` asserts `check.yml` has a top-level `concurrency` block (the rig already checks nightlies).

### C3 — Replace heredoc-with-`${{ steps.* }}` injection pattern (CI-5)

**Where:** `nightly_phase_a_gate.yml:256-272`.

**Fix:** write each step output to `$GITHUB_ENV`, reference via `$VAR_NAME` inside the heredoc; or build the JSON via a small Python step using `json.dumps`.

**Verify:** new `tests/test_ci.py::test_no_steps_interpolation_into_heredoc` greps for `<<EOF` blocks immediately preceded by lines containing `${{ steps.` interpolation; fails if any found.

### C4 — Validate `summary_path` before splicing (CI-6)

**Where:** `nightly_phase_b_gate.yml:167,200`.

**Fix:** before the heredoc, `jq empty "$summary_path" || exit 1`. Then `summaries=$(jq -c . "$summary_path")` (normalise to one line).

**Verify:** included in the same CI test that runs against the existing nightly-rig fixtures.

### C5 — Cache `cargo-audit` and `cargo-machete` in the `quality` job (CI-7)

**Fix:** mirror the audit job's `actions/cache@...` block in `quality`, keyed by `${{ runner.os }}-cargo-tools-v1`. **Verify:** workflow `quality` step shows a cache hit on second PR run.

### C6 — `assets/build-icon.sh` trap-rm tempdir (SH-1)

**Fix:** after the two `mktemp -d` calls, set `trap 'rm -rf "$ICONSET_ROOT" "$ICO_TMP"' EXIT`. **Verify:** unit test `bash -c '. assets/build-icon.sh; ls $TMPDIR/atelier-icon.* 2>/dev/null'` is empty after run.

### C7 — Hook scripts surface jq absence (SH-2)

**Fix:** `bounded-reads.sh`, `save-nudge.sh` emit one `echo "[bounded-reads] jq missing — hook inert" >&2` before `exit 0`. **Verify:** ad-hoc test with `PATH=/usr/local/bin` (no jq) runs the hook and stderr contains the marker.

---

## Bundle D — Rig + schemas

### D1 — Centralise UTF-8 read helper (RIG-M1)

**Fix:** new `_schema_helpers.read_text(path)` that does `path.read_text(encoding="utf-8")`. Replace 22 call sites. **Verify:** `grep -rn 'read_text()$' tests/` returns empty after the sweep.

### D2 — `test_validate_artifacts_fails_on_unmatched_path` uses tmp_path (RIG-M2)

**Fix:** stage `synthetic.json` into `tmp_path` and monkey-patch the validator's `ARTIFACT_ROOTS` to include `tmp_path`. **Verify:** killing the test mid-flight does not leave artifacts in the working tree.

### D3 — `_run_with_pg_timeout` returns captured pipe contents (RIG-M3)

**Fix:** see H4 (already covered there).

### D4 — Type-hint sweep on rig fns (RIG-M4)

**Fix:** annotate every fn in `tests/workload/runner/runner.py`, `_schema_helpers.py`, `validate_artifacts.py`. **Verify:** `python -m pyright tests/ 2>&1 | grep error || echo OK` (if pyright is available locally; not gated in CI yet).

### D5 — Runner `--out` write error handling (RIG-M5)

**Fix:** `runner.py:323-324` wraps in `try/except OSError`, prints to stderr, exits non-zero. **Verify:** new test sets `--out /dev/full` (Linux), asserts non-zero exit + stderr message.

### D6 — Workflow lint discovers nightlies dynamically (RIG-M6)

**Fix:** `tests/test_ci.py:81-99` replaces literal nightly list with `[p for p in WORKFLOWS_DIR.glob("nightly_*.yml")]`. **Verify:** add a new `nightly_dummy.yml` fixture, assert it's picked up.

### D7 — Constrain free-form schema fields (SCH-M1, M2, M4, M5, M6, M7)

Single sweep across `schemas/`:
- `session/v1.json:114,281`: `args` → `{"type": "object"}`.
- `protocol/overhead.v1.json:22-25`: `median_overhead_pct` → `"minimum": -1.0` + `python -c 'json.dump(..., allow_nan=False)'` in writer.
- `config/mcp_servers.v1.json:36-49`, `config/hook_manifest.v1.json:40-42`: env/headers values `"maxLength": 4096`.
- `workload/runner_result.v1.json:9`: `runner_version` → `"const": 1`; checks-item `required` includes `"kind"`.
- `audit/mcp_egress.v1.json`, `config/mcp_servers.v1.json`: url `"pattern": "^https?://"`.
- `telemetry/payload.v1.json:42-46`: token + cost fields `"minimum": 0`.

**Verify:** `make schemas` + `make artifacts` green. New `tests/test_schemas.py` cases assert each malformed input fails validation.

### D8 — `protocol-overhead` writer uses `allow_nan=False` (SCH-M2 companion)

**Where:** wherever `runner.py` / `overhead.rs` (Rust) writes the protocol-overhead artifact.

**Fix (Python):** `json.dump(payload, fp, allow_nan=False)`. **Fix (Rust):** `serde_json` rejects NaN/Inf by default, so this is a Python-only change.

**Verify:** new rig test feeds a NaN through and asserts the writer raises.

---

## Bundle verification (per bundle)

- `make check` ⇒ green.
- `cargo fmt --check && cargo clippy -- -D warnings && cargo test --workspace`.
- Each item has an explicit unit test or rig test verifying the fix.
- CHANGELOG entry per bundle (e.g., `v60.37.A`, `v60.37.B`, …).

## Cross-bundle file disjointness

- A touches `crates/atelier-core/src/{config,mcp_config,persistence,dod,hooks,path_safety}.rs` + `crates/atelier-core/src/adapter/model_profile.rs` + `crates/atelier-cli/src/{compaction,compaction_blob,instrumentation,memory_promote,overhead,runner}.rs`
- B touches `crates/atelier-gui/src/lib.rs` + `crates/atelier-gui/ui/src/**`
- C touches `.github/workflows/*.yml` + `tests/test_ci.py` + `assets/build-icon.sh` + `.atelier/hooks/*.sh`
- D touches `tests/**` + `schemas/**`

Four parallel-safe bundles (with the caveat that bundle A and bundle C both lightly modify `tests/test_ci.py`; that overlap is small and easily resolved).
