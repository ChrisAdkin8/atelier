# Atelier — project context

Atelier is a BYOM (bring-your-own-model) coding harness: agent loop, dispatcher, verification gates, hooks, cost ledger, GUI + TUI, end-to-end runnable. Providers landed: Mock, Anthropic Messages API, and OpenAI-compatible (LM Studio / llama-server / vLLM / sglang / Ollama / OpenAI itself) through a production `rmcp` MCP client. See `tasks/todo.md` for active state, `CHANGELOG.md` for the version trail, and `STATUS.md` for the gate matrix.

## Constraints worth remembering

- **No `.claude/` paths or `$CLAUDE_PROJECT_DIR` in tracked source.** Atelier is a BYOM harness; hardcoding one host harness's paths is a contract violation.
  - Paths: use `.atelier/` instead. Env-vars: `ATELIER_PROJECT_DIR` instead.
  - In hook scripts, derive `ATELIER_PROJECT_DIR` from `${BASH_SOURCE[0]}` so the script is self-locating regardless of host.
  - In `.atelier/settings.json` hook commands, use project-relative paths — BYOM-compatible host harnesses run hooks with `cwd=project root`.
  - Background: `.atelier/memory/feedback_atelier_path_directive.md`. Enforced by `test_no_claude_paths_in_tracked_source` in `tests/test_runner.py`.
- The harness hard-codes two read paths we can't relocate. We satisfy them with shim symlinks instead of real files:
  - `<atelier>/.claude/settings.json` → `.atelier/settings.json`
  - `<atelier>/CLAUDE.md` → `ATELIER.md`
  Both shims are gitignored. `.atelier/settings.local.json` is per-user and also gitignored (regenerate locally). Edit the real files in `.atelier/` or `ATELIER.md`.
- Don't break the symlink at `~/.claude/projects/-Users-chris-adkin-Projects-atelier/memory` → `<atelier>/.atelier/memory`. The harness reads memory through it.
- `tests/workload/canonical/` fixtures are not pytest collection targets. They're copied to tempdirs by the runner.

## Don'ts

- **New code defaults to `pub(crate)`, not `pub`.** Widen to workspace-`pub` only when a named consumer outside the crate needs it. Core, CLI, and TUI all skew heavily toward over-exposure; check `.atelier/metrics/snapshot.json` for the live per-crate ratio before widening.
- **Don't add to `dispatcher.rs` or `Runner::run` without splitting first.** Both are the workspace's known sprawl outliers; before adding, sketch the split in `tasks/todo.md`, land the refactor, then add the feature. Current LOC in `.atelier/metrics/snapshot.json` under `crates.*.largest_files` and `slop_indicators.fn_length_pure_python.max`.
- **New `unsafe` requires a `// SAFETY:` comment within 3 lines.** Document the invariant the caller must uphold; a safe wrapper with a focused test is preferred.
- **One error-handling idiom per module.** Match the surrounding code. `atelier-core` is the most idiom-diverse crate — read the adjacent file before importing a new style.
- **Never commit or log raw API keys.** Local dev: keys in `.envrc` (gitignored). Day-to-day: `atelier providers auth <profile>` stores the secret in the OS keychain, referenced as `api_key = "keyring:SERVICE/USER"` in `providers.toml`. CI: `env:NAME` indirection. Secrets-in-git is irreversible — key rotation plus history scrubbing.

## Stack

- **Rust workspace** pinned to 1.85 (`rust-toolchain.toml`). Four crates in a clear data flow: `atelier-core` (agent loop, BYOM adapters, ledger — no UI) is consumed by `atelier-cli` (hybrid lib+bin exposing a `Runner` library), which both `atelier-gui` (Tauri 2 + Svelte 5) and `atelier-tui` (ratatui + crossterm) link against to drive scripted runs.
- **Python rig** in `tests/` validates schemas, artifacts, and workload runs. Installed via `pyproject.toml [optional-dependencies.rig]`.
- **MCP-first tool transport** via the production `rmcp` client. Built-in tools and registered MCP tools share the same dispatcher surface — hooks, ledger, trust budget, and verification gates treat them uniformly.
- **Skills + sub-agents.** Skills live in `.atelier/skills/`; sub-agents spawn through the `spawn_subagent` built-in tool. Both ride the same dispatcher surface that built-in and MCP tools use, so hooks, ledger, and trust budget apply uniformly.
- **LSP-backed Tier-1 verification.** Lives in `crates/atelier-core/src/lsp/`; runs language-server diagnostics against staged edits as a verification gate before they land.
- **BYOM providers landed**: Mock, Anthropic Messages API (`anthropic:` prefix, `ANTHROPIC_API_KEY`), and OpenAI-compatible (`openai-compat` with `--base-url`; covers LM Studio, llama-server, vLLM, sglang, Ollama's `/v1/`, OpenAI). Bedrock + Vertex sit in later phases.

## Canonical commands

- `make check` — full rig: schema meta-validation → artifact validation → rig self-tests → workload dry-run. Run this before claiming anything is fixed.
- `make schemas` / `make artifacts` / `make rig-tests` / `make dry-run` — individual stages.
- `make metrics` — JSON code-quality snapshot at `.atelier/metrics/snapshot.json` (LOC, sprawl, production-path proxies, AI-slop indicators, visibility ratios). Timestamped copies under `.atelier/metrics/history/` for trend diff.
- `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test -p atelier-core` — Rust gates that CI runs.
- `make install-rig` — create `.venv/` (if absent) and install the rig deps. Subsequent Make targets auto-prefer `.venv/bin/python`, falling back to system `python3` (CI installs deps directly).

## Layout pointers

- `coding-harness-spec.md` — the spec. Cite section numbers (e.g., §2.5, §15) when relevant.
- `schemas/` — JSON Schemas. Cross-schema `$ref`s resolve via `tests/_schema_helpers.py`.
- `tests/workload/canonical/` — task fixtures. Don't run pytest *inside* canonical/; the runner copies each to a tempdir (`pyproject.toml` excludes it).
- `tasks/todo.md` — current phased build plan. Active state lives here, not in this file.
- `CHANGELOG.md` — spec + rig revisions. Latest version lives here, not in this file.
- `crates/atelier-core/src/adapter/` — the BYOM surface: `mod.rs` (trait + Mock), `anthropic.rs` (Messages API), `openai_compat.rs` (OpenAI-compatible against LM Studio / llama-server / vLLM / Ollama / OpenAI), `model_profile.rs` (probe-on-first-use cache).
- `crates/atelier-cli/src/runner.rs` — the `Runner` that wires the §2.5 actor + dispatcher + ledger + adapter + probe into a runnable loop. Linked by both `atelier-cli` (binary) and the GUI/TUI (driver mode).
- `README.md` — end-user quick start for local LLMs (Ollama / LM Studio / llama-server / vLLM / OpenAI). Profile config lives in `.atelier/providers.toml`; keychain auth via `atelier providers auth <profile>` (`api_key = "keyring:SERVICE/USER"`), or `env:NAME` indirection for CI.

## Verification convention

"Done" is a state transition the rig verifies, not a claim. After any change touching schemas, fixtures, or rig code: `make check` must pass. After any Rust change: `cargo fmt --check && cargo clippy -- -D warnings && cargo test -p atelier-core`. State the exact command you ran in your verification report.

## Memory system

Project-specific memory lives in `.atelier/memory/` (symlinked from the harness-mandated preload path under `~/.claude/projects/.../memory/` — the only residual harness `.claude/` reference for atelier). Cross-project lessons live in `~/.atelier/memory/`. Full layout, lifecycle, and tools (`memcheck`, `mempromote`, `memrecall`) in `.atelier/docs/memory-system.md`.

Four homes:
- **`ATELIER.md`** (this file) — always-loaded stable facts. Auto-loaded via the `CLAUDE.md → ATELIER.md` symlink shim at the repo root.
- **`.atelier/memory/`** — retrievable facts (prefs, project decisions with dates, references, feedback). Indexed by `MEMORY.md`.
- **`tasks/todo.md`** — active in-progress state. Volatile.
- **`tasks/lessons.md`** — per-project process lessons (per the user's global rule in `~/.claude/CLAUDE.md`).

## About this file

This file is loaded into every Claude Code conversation in this project — every line costs tokens on every turn. Add content only if it changes the assistant's behaviour and isn't derivable from the code or git history. Version-specific claims belong in `CHANGELOG.md`; in-flight work belongs in `tasks/todo.md`. Edit `ATELIER.md` directly; `CLAUDE.md` is a read-only symlink shim.
