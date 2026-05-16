# Atelier — project context

Atelier is a **pre-implementation coding harness**: agent loop, BYOM adapters, MCP transport, verification gates, hooks, cost ledger. Nothing is built yet beyond a full spec, schemas, and a self-testing calibration rig. The harness is the product; this repo is the rig that will verify it.

## Stack

- **Rust workspace**, pinned to 1.85 (`rust-toolchain.toml`). Three crates: `atelier-core` (agent loop, MCP client, session state, ledger — no UI), `atelier-gui` (Tauri 2.x scaffold), `atelier-tui` (ratatui + crossterm scaffold). GUI and TUI both consume `atelier-core` via a broadcast channel.
- **Python rig** in `tests/` validates schemas, artifacts, and workload runs. Pinned via `pyproject.toml [optional-dependencies.rig]`.
- **MCP-first tool transport** via `rmcp` crate. Built-in and external tools share the MCP interface.

## Canonical commands

- `make check` — full rig: schema meta-validation → artifact validation → rig self-tests → workload dry-run. Run this before claiming anything is fixed.
- `make schemas` / `make artifacts` / `make rig-tests` / `make dry-run` — individual stages.
- `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test -p atelier-core` — Rust gates that CI runs.
- `make install-rig` — create `.venv/` (if absent) and install the rig deps into it. Subsequent Make targets auto-prefer `.venv/bin/python` when present, falling back to system `python3` (CI installs deps directly and uses the fallback).

## Layout pointers (read these, not the README, when you need orientation)

- `coding-harness-spec.md` — the spec. Cite section numbers (e.g., §2.5, §15) when relevant.
- `schemas/` — 20 JSON Schemas. Cross-schema `$ref`s resolve via `tests/_schema_helpers.py`.
- `tests/workload/canonical/` — 11 task fixtures. Don't run pytest *inside* canonical/; the runner copies each to a tempdir (`pyproject.toml` excludes it).
- `tasks/todo.md` — current phased build plan. Active state lives here, not in this file.
- `CHANGELOG.md` — spec + rig revisions.

## Verification convention

"Done" is a state transition the rig verifies, not a claim. After any change touching schemas, fixtures, or rig code: `make check` must pass. After any Rust change: `cargo fmt --check && cargo clippy -- -D warnings && cargo test -p atelier-core`. State the exact command you ran in your verification report.

## Memory system

Project-specific memory lives in `.atelier/memory/` (symlinked from the harness-mandated preload path under `~/.claude/projects/.../memory/` — the only residual harness `.claude/` reference for atelier). Cross-project lessons live in `~/.atelier/memory/`. See `.atelier/docs/memory-system.md` for full layout, tools (`memcheck`, `mempromote`, `memrecall`), and lifecycle.

What goes where:
- **This file (`ATELIER.md`)** — always-loaded stable facts about atelier. Slow-changing. Auto-loaded by the harness via a `CLAUDE.md → ATELIER.md` symlink shim at the repo root.
- **`.atelier/memory/`** — retrievable facts (user prefs, project decisions with dates, references, feedback). Indexed by `MEMORY.md`.
- **`tasks/todo.md`** — active in-progress state. Volatile.
- **`tasks/lessons.md`** — per-project process lessons (per the user's global rule in `~/.claude/CLAUDE.md`).

## Constraints worth remembering

- Atelier is a BYOM (bring-your-own-model) harness. New tracked source must not reference `.claude/` paths *or* `$CLAUDE_PROJECT_DIR` — that couples the repo to one host harness. Use `.atelier/` for paths and `ATELIER_PROJECT_DIR` for env-vars. In hook scripts, derive `ATELIER_PROJECT_DIR` from `${BASH_SOURCE[0]}` so the script is self-locating regardless of host. In `.atelier/settings.json` hook commands, use project-relative paths (BYOM-compatible host harnesses run hooks with `cwd=project root`). See `.atelier/memory/feedback_atelier_path_directive.md` and the enforcing test `test_no_claude_paths_in_tracked_source` in `tests/test_runner.py`.
- The harness hard-codes two read paths that we can't relocate. We satisfy them with shim symlinks instead of real files:
  - `<atelier>/.claude/settings.json` → `.atelier/settings.json`
  - `<atelier>/CLAUDE.md` → `ATELIER.md`
  Both shims are gitignored. `.atelier/settings.local.json` is per-user and also gitignored (regenerate locally). Edit the real files in `.atelier/` or `ATELIER.md`.
- Don't break the symlink at `~/.claude/projects/-Users-chris-adkin-Projects-atelier/memory` → `<atelier>/.atelier/memory`. The harness reads memory through it.
- `tests/workload/canonical/` fixtures are not pytest collection targets. They're copied to tempdirs by the runner.
