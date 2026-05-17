# Atelier — project context

Atelier is a **coding harness, end-to-end runnable on Phase A/B/C scope**: agent loop, BYOM adapters, verification gates, hooks, cost ledger, GUI + TUI. As of v54 the full pipeline runs against three providers (Mock, Anthropic, OpenAI-compatible — which covers LM Studio / llama-server / vLLM / sglang / Ollama / OpenAI itself), with hunk accept/reject, GUI driver mode, TUI driver mode, probe-on-first-use model adaptation, a multi-profile `.atelier/providers.toml` config, and both §5 panels: Context (per-turn items with token counts + provenance) and Memory (durable cards). The GUI and TUI footers render the active model id + §2 strategy + probe outcome in the bottom-right. `atelier-core` carries the §2.5 actor, §3 atomic staging + incremental diff stream, §11 sandbox profiles, §14 on-disk session + recovery log + registry, §15 hook loader + first-use approval + dispatcher + seven built-in tools, §2 typed envelope + three emission strategies + conformance tracker, §7 did-it-do-what-it-said + DoD loader, §5 typed context/memory/plan + `ContextItemSummary` + `MemoryCardSummary` projections, §1 probe-on-first-use cache, and the v53 `ProvidersConfig` loader. The §15 MCP-over-`rmcp` client is the remaining big-ticket Phase A item; the harness drives built-in tools end-to-end today. See `tasks/todo.md` for what's done vs. in flight; `CHANGELOG.md` for the version-by-version trail (latest: v54).

## Stack

- **Rust workspace**, pinned to 1.85 (`rust-toolchain.toml`). Four crates: `atelier-core` (agent loop, BYOM adapters, session state, ledger — no UI), `atelier-cli` (hybrid lib+bin; the `atelier` binary plus a `Runner` library the GUI/TUI link against), `atelier-gui` (Tauri 2.x + Svelte 5 driver), `atelier-tui` (ratatui + crossterm driver). GUI and TUI both consume `atelier-core` via a broadcast channel and embed `atelier-cli::Runner` to drive scripted runs.
- **Python rig** in `tests/` validates schemas, artifacts, and workload runs. Pinned via `pyproject.toml [optional-dependencies.rig]`.
- **MCP-first tool transport** via `rmcp` crate (gated on the spike at `experiments/rmcp_spike/`). Built-in tools (seven landed: `read_file`, `list_dir`, `grep`, `write_file`, `edit_file`, `ast_grep`, `shell`) share the dispatcher with the future MCP-hosted external tools — hooks, ledger, trust budget, and verification gates treat them uniformly.
- **BYOM providers landed (v51):** Mock (always), Anthropic Messages API (`anthropic:` model prefix, `ANTHROPIC_API_KEY`), OpenAI-compatible (`openai-compat` with `--base-url`; works against LM Studio, llama-server, vLLM, sglang, Ollama's `/v1/` compat surface, and OpenAI itself; `OPENAI_API_KEY` honoured but optional). Bedrock + Vertex sit in Phase E/F.

## Canonical commands

- `make check` — full rig: schema meta-validation → artifact validation → rig self-tests → workload dry-run. Run this before claiming anything is fixed.
- `make schemas` / `make artifacts` / `make rig-tests` / `make dry-run` — individual stages.
- `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test -p atelier-core` — Rust gates that CI runs.
- `make install-rig` — create `.venv/` (if absent) and install the rig deps into it. Subsequent Make targets auto-prefer `.venv/bin/python` when present, falling back to system `python3` (CI installs deps directly and uses the fallback).

## Layout pointers (read these, not the README, when you need orientation)

- `coding-harness-spec.md` — the spec. Cite section numbers (e.g., §2.5, §15) when relevant.
- `schemas/` — 21 JSON Schemas. Cross-schema `$ref`s resolve via `tests/_schema_helpers.py`.
- `tests/workload/canonical/` — 11 task fixtures. Don't run pytest *inside* canonical/; the runner copies each to a tempdir (`pyproject.toml` excludes it).
- `tasks/todo.md` — current phased build plan. Active state lives here, not in this file.
- `CHANGELOG.md` — spec + rig revisions; v51 = latest.
- `crates/atelier-core/src/adapter/` — the BYOM surface: `mod.rs` (trait + Mock), `anthropic.rs` (Messages API), `openai_compat.rs` (v50; OpenAI-compatible against LM Studio / llama-server / vLLM / Ollama / OpenAI), `model_profile.rs` (v51; probe-on-first-use cache).
- `crates/atelier-cli/src/runner.rs` — the `Runner` that wires the §2.5 actor + dispatcher + ledger + adapter + probe into a runnable loop. Linked by both `atelier-cli` (binary) and the GUI/TUI (driver mode).

## Running a local LLM through the harness (v50+)

`atelier-compat` works with any OpenAI-style chat-completions server. Quickest path:

```sh
brew install ollama && brew services start ollama
ollama pull qwen2.5-coder:7b
cargo run -p atelier-cli -- run \
    --provider openai-compat \
    --base-url http://localhost:11434/v1 \
    --model local:qwen2.5-coder:7b \
    "<prompt>"
```

On first use the harness fires a short calibration probe (one tool-call test + one JSON-sentinel test) and caches the resulting `ModelProfile` to `~/.atelier/model_profiles/<hash>.json`. Override with `--no-probe` (skip; use capability defaults) or `--force-probe` (re-probe even if cached). LM Studio (`:1234`), llama-server (`:8080`), vLLM (`:8000`), and OpenAI itself (no `--base-url`, set `OPENAI_API_KEY`) all work through the same flag.

To skip re-typing the flags every invocation, drop them into `<repo>/.atelier/providers.toml` (v53; renamed + reshaped from v52's `config.toml`). The binary loads it automatically:

```toml
default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"

[providers.cloud]
provider = "anthropic"
model    = "anthropic:claude-opus-4-7"
```

Multiple named profiles; `--profile <NAME>` switches between them; `default` picks one when no flag is given. Per-field CLI flags still override individual fields of the resolved profile. Precedence (top wins): CLI > resolved profile > built-in defaults. The active model + strategy + probe outcome render in the bottom-right of the GUI/TUI footer; the v53 §5 Context panel lists per-item token counts + provenance in the same right-side column.

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
