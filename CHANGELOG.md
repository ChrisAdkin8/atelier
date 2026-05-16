# Atelier Spec — Changelog

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
