# v60.38 — Low-severity fixes

**Scope:** 10 findings from the 2026-05-19 deep scan. Single hygiene-sweep bundle.

Low-severity items by convention are batched as one commit; no need for sub-bundles.

---

## L1 — CI: heredoc dedent in `nightly_phase_a_gate.yml`

**Where:** `nightly_phase_a_gate.yml:256` onward.

**Fix:** switch to `cat > file.json <<'JSON' … JSON` with the outer heredoc at column-1; or build the JSON via `python -c 'import json,sys; json.dump(...)'` to eliminate heredoc indentation drift.

## L2 — `save-nudge.sh` bounds prompt length

**Where:** `.atelier/hooks/save-nudge.sh:32-42`.

**Fix:** `prompt=${prompt:0:8192}` before the `case "$prompt" in` matcher.

## L3 — Toast `setTimeout` handles cleaned in `onDestroy`

**Where:** `crates/atelier-gui/ui/src/lib/components/Composer.svelte`, `MentalModelPane.svelte:104`, `MemoryPane.svelte:117`.

**Fix:** capture each `setTimeout` handle in a `$state` and `clearTimeout` in `onDestroy`.

## L4 — `App.svelte::selectedSwapIndex` not raced against `currentModel`

**Where:** `App.svelte:197-202, 349`.

**Fix:** introduce a local `$state` `dropdownIndex` synced from `currentModel.modelId` via `$effect`; `<select value={dropdownIndex}>` binds to the local state.

## L5 — `test_no_claude_paths_in_tracked_source` suffix list inverted to a skip set

**Where:** `tests/test_runner.py:280-285`.

**Fix:** replace the include-list `{".md", ".json", …}` with a skip-set `{".png", ".svg", ".lock", ".ico", ".woff", ".woff2"}` + `is_text` heuristic for unknown extensions (try-decode the first 512 bytes as UTF-8).

## L6 — `compare_baselines.py` renders `n/a` on inf

**Where:** `tests/baselines/compare_baselines.py:57`.

**Fix:** if the divisor is 0, emit `"   n/a"` instead of `"     inf"`.

## L7 — Audit-schema `provider` field consistency

**Where:** `schemas/audit/egress.v1.json:17` (no pattern) vs `audit/mcp_egress.v1.json:28-31` (regex-constrained).

**Fix:** apply the same `^[a-z][a-z0-9_-]*$` pattern to `egress.v1.json::provider`.

## L8 — `protocol/overhead.v1.json::providers` per-row version

**Where:** `schemas/protocol/overhead.v1.json`.

**Fix:** add `"version": {"const": 1}` to the per-row object (optional, not required, so existing artifacts still validate). Documents the row-shape version separately from the top-level artifact version.

## L9 — `mcp_catalog.v1.json::requires_secrets[*].where` enum extension hook

**Where:** `schemas/config/mcp_catalog.v1.json:101-104`.

**Fix:** **Defer**. The current enum (`["header", "env"]`) covers all bundled catalog entries; add `argv` only when the first MCP server requiring CLI-arg secrets is wired. Mark as `TODO` in a top-of-file comment.

## L10 — `InlineRenderers.svelte::parseBlocks` migrated to `<script module>`

**Where:** `crates/atelier-gui/ui/src/lib/components/InlineRenderers.svelte:75`.

**Fix:** move the exported helper into a `<script module>` block (Svelte 5 idiom for module-level exports). Functional today via the legacy `<script lang="ts">` export but unconventional.

---

## Bundle verification

- `make check` ⇒ green.
- `cargo fmt --check && cargo clippy -- -D warnings && cargo test --workspace`.
- Frontend `npm run check` (vitest + svelte-check) — same state as before this bundle.
- CHANGELOG entry: `v60.38: deep-scan low-severity hygiene sweep`.

## Informational items (no-op, recorded for future)

- **CI-9** — `cache-cargo-audit` cache key lacks toolchain-version component. Acceptable while `rust-toolchain.toml` is pinned.
- **CI-10** — `nightly_phase_b_gate.yml` post-calibration branch is still a placeholder. Tracked in `tasks/todo.md` Phase B operator actions.
- **SH-4** — `session-start-memcheck.sh:13` `$HOME` dereference would error under `set -u`. Hard error is the right behaviour; no change.
- **SCH-I1, I2, I3** — schema-level conventions already enforced by `test_schemas.py`. No change.
- **RIG-I1, I2** — Python rig is shell-injection / unsafe-deserialise free. No change.
- **UI-8** — `InlineRenderers.svelte` `<script module>` migration: L10 above.
