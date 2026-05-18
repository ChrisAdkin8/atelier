# Atelier — project context

Atelier is a **coding harness, end-to-end runnable on Phase A/B/C scope**: agent loop, BYOM adapters, verification gates, hooks, cost ledger, GUI + TUI. As of v60.11 Phase C is fully closed, §14 has a kill-9 mechanical gate, §11 has a sandbox-egress mechanical gate, §7 has a UI tier indicator, §15's config-loader + first-use approval store has landed (rmcp client still deferred), and §1 BYOM has runtime conformance-driven strategy degradation (NativeTool → JsonSentinel → RegexProse on 3-of-20 PROVISIONAL malformed-envelope rate). v60.9 adds §1 context-window asymmetry (Compact / Reroute / Surface — Compact wires v60.5 compaction into the agent loop when an adapter returns `ContextOverflow`) and §2 per-adapter few-shot override (Anthropic + OpenAI-compat both customise the `JsonSentinel` few-shot for their parser quirks). v60.10 resolves Q7 (rmcp 0.1.5 — **GO WITH CAVEATS**) and lands the §15 foundation: rmcp dep on `atelier-core`, new `mcp::stdio_launcher` that exercises `@modelcontextprotocol/server-filesystem` end-to-end (handshake + tool list + invoke + clean shutdown via `CancellationToken`), plus §1 mid-session provider swap (`Runner::swap_adapter` preserves context/memory/plan/conversation/pending-approval, resets conformance window + strategy + capability row + few-shot cache). HTTP/SSE, built-ins-as-MCP refactor, MCP resources as §5 context items, and the §15 mechanical gate sit on top and land in v60.11+. v60.11 closes most of that: HTTP/SSE launcher with §12 egress audit (`schemas/audit/mcp_egress.v1.json`), dispatcher tool registration via `McpToolWrapper`+`register_mcp_servers`, MCP resources surfaced as `ContextItem`s with new `Provenance::McpResource` variant. Built-ins-as-MCP refactor and the §15 mechanical gate remain. (mental-model panel, inline Mermaid/D2/image rendering, both UX-target measurement workloads landed) and §14 has its concurrent-edit + resume + kill-9-recovery mechanical gate. The full pipeline runs against three providers (Mock, Anthropic, OpenAI-compatible — which covers LM Studio / llama-server / vLLM / sglang / Ollama / OpenAI itself), with **hunk-level accept/reject** (sub-file granularity in the GUI; TUI is file-level via the same dispatcher surface), GUI driver mode, TUI driver mode, probe-on-first-use model adaptation, a multi-profile `.atelier/providers.toml` config, full **editable round-trips** on both §5 panels (Context pin/unpin/evict-with-cache-bust-confirm; Memory add/delete/promote-to-`~/.atelier/memory/`; Plan add/cycle/constraint/remove + GUI drag-and-drop reorder), a **"Why this change?" rationale** rendered next to each diff from the envelope's `claimed_changes`, **§5 non-destructive context compaction with reversible Expand** (multi-select → adapter-generated summary → pinned `MemoryCard` with `compacted_from` link → originals written to `.atelier/sessions/<sid>/compactions/<comp-uuid>.json`; `⤴ expand` button restores them with cache-rewarm cost disclosure), and the v60.7 **§5 mental-model panel** (off by default, cost-disclosed when enabled). v60.7 also lands the **§1 BYOM cost-ledger discipline + capability matrix** (per-call `count_source` declared faithfully per-adapter, latency-weighted local cost at `$0.00028/sec` for Mock + non-cloud OpenAI-compat, static `capability_matrix` for 9 well-known models with `claimed_but_broken` cross-walk from probe observations rendered in GUI tooltip + TUI footer suffix), the **§14 concurrent-edit story** (per-session `notify`-driven file watcher tracks the tool dispatcher's read-set; `Event::FilesChanged` → modal in GUI/TUI with Reload/Wait/Pause options + 5-min auto-pause; `--non-interactive` flag forces `AutoReload`), **resume-at-last-completed-tool-call** (`Runner::with_resume(uuid)` + `--resume <UUID>` CLI flag; recovery_log surfaces as `MessageRole::System`), and the **§2 protocol-overhead harness** (`atelier protocol-overhead` subcommand + nightly CI job that flags >10% drift vs the 7-day rolling median). The §3 mechanical gate (10-file rename, live-diff incremental, final diff byte-equal) is green. v57–v60 ran four consecutive deep-audit / fix rounds against the v56 surface — ~45 correctness/security/hygiene fixes (two-pass `commit_selected_hunks` atomicity, symlink-TOCTOU recheck inside commit, `merge_stop_reason` priority lattice with Refusal>ToolUse, atomic `NamedTempFile::persist` writes for memory promotion in *both* drivers, shared `text_safety` + `memory_promote` modules so a future Unicode revision is a one-line change, plan-text validation on both `from_vec` and `apply_envelope`, secret-substring redaction for cloud creds, per-call size caps on `read_file`/`write_file`/`edit_file`, `wire_label`-vs-serde agreement tests across every cross-boundary enum). The GUI and TUI footers render the active model id + §2 strategy + probe outcome + capability-matrix tooltip in the bottom-right. `atelier-core` carries the §2.5 actor, §3 atomic staging with the v56 per-hunk splice + v58 two-pass commit + v59 stage→commit symlink recheck + incremental diff stream, §11 sandbox profiles, §14 on-disk session + recovery log + registry + v60.7 `resume_conversation_prefix` + `file_watcher`, §15 hook loader + first-use approval + dispatcher + seven built-in tools + v55 §5 mutator surface + v60.5 `compact_context_items` mutator + v60.6 `expand_memory_card` mutator (with `ContextManager::add_batch` for atomic restore) + v60.7 `set_mental_model` / `resolve_concurrent_edit` mutators, §2 typed envelope + three emission strategies + conformance tracker + v60.7 `measure_overhead` helper, §7 did-it-do-what-it-said + DoD loader, §5 typed context/memory/plan + `ContextItemSummary` + `MemoryCardSummary` projections (carrying v60.5's `compacted_from` link + v60.6's `cache_rewarm_tokens` cost disclosure) + v60.7 `mental_model::MentalModel`, §1 probe-on-first-use cache + v60.7 `capability_matrix`, the v53 `ProvidersConfig` loader, the v60 shared `text_safety` predicate, the v60.5 `compaction_blob` writer/reader (consumed by v60.6 Expand), and the v60.7 `instrumentation::{PaneVisibilityRecord, FindProbeLog}` on-disk records. The §15 MCP-over-`rmcp` client is the remaining big-ticket Phase A item; the harness drives built-in tools end-to-end today. See `tasks/todo.md` for what's done vs. in flight; `CHANGELOG.md` for the version-by-version trail (latest: v60.11).

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
- `CHANGELOG.md` — spec + rig revisions; v60.5 = latest.
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
