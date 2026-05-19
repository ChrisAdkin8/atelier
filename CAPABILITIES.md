# What Atelier can do

Atelier is a coding harness: a desktop app, a terminal app, and a CLI that lets you point a large language model at your codebase and ask it to make changes. The harness watches the model, runs its tool calls in a sandbox, shows you every proposed edit before anything touches disk, and keeps a durable record of the whole session so you can pause, resume, or rewind.

This document is the end-user tour. If you want the contributor-facing internals, start with `ATELIER.md`; for the formal contract, see `coding-harness-spec.md`.

---

## Bring your own model

Atelier doesn't ship a model. You connect it to one of these:

- **Mock adapter** — an in-process scripted adapter. Useful for trying the workflow without spending tokens; powers the test suite.
- **Anthropic Messages API** — Claude Opus, Sonnet, Haiku. Set `ANTHROPIC_API_KEY` and pass `--provider anthropic --model anthropic:claude-opus-4-7` (or any other model id).
- **OpenAI-compatible servers** — any HTTP endpoint that speaks the OpenAI chat-completions wire format. Covers:
  - **OpenAI itself** (default base URL).
  - **LM Studio** at `http://localhost:1234/v1`.
  - **llama-server** at `http://localhost:8080/v1`.
  - **vLLM** at `http://localhost:8000/v1`.
  - **sglang** at the same shape.
  - **Ollama** at `http://localhost:11434/v1`.
  - Anything else conforming to the OpenAI compat surface.

Switching providers is one flag (`--provider`, `--model`, `--base-url`) or a named profile in `.atelier/providers.toml`. You can mid-session swap from the GUI's dropdown — the harness tears down the old adapter, builds the new one, and confirms with you via a consent modal before any new turn fires. Context, memory, plan state, and conversation history all survive the swap.

**First-use calibration.** The first time you use a new local model, Atelier fires a short probe (one tool-call test + one JSON-sentinel test) to learn its actual capabilities versus what its name advertises. The result is cached under `~/.atelier/model_profiles/` so subsequent sessions skip the probe.

**Capability matrix.** For nine well-known models the harness ships a built-in `capability_matrix` that records what each model can really do versus what it claims; the model's footer badge tells you if there's a mismatch.

---

## Three ways to drive it

Atelier is one engine with three faces:

- **`atelier` CLI** — `cargo run -p atelier-cli -- run "<prompt>"`. Headless, scriptable, great for one-shot prompts and CI workloads.
- **TUI** — terminal app built on ratatui. Three panes (conversation, context/memory/plan, diff), scrubber-style history, full keyboard discipline. Good fit when you live in tmux.
- **GUI** — Tauri 2 + Svelte 5 desktop app. Same three panes plus inline Mermaid / image rendering, drag-and-drop plan reorder, hunk-level diff acceptance with finger-paint precision.

The CLI, TUI, and GUI all consume the same `atelier-core` engine through a broadcast event channel. Whatever surface you prefer, the underlying behaviour is the same.

---

## What happens when you submit a prompt

1. **You write a prompt.** Plain English; the model picks up context from any pinned files and from your memory cards.
2. **The agent loop runs.** Each turn, the model can:
   - Emit a structured envelope alongside its prose (claimed changes, plan updates, uncertainty markers).
   - Call one or more built-in tools (`read_file`, `list_dir`, `grep`, `ast_grep`, `write_file`, `edit_file`, `shell`).
   - Call any MCP-registered external tool.
3. **The dispatcher runs tool calls in a sandbox.** Filesystem reads are confined to the repo (symlink escapes are rejected); writes stage into a per-session staging area first. Network egress from the `shell` tool is denied by default; every blocked attempt is audited.
4. **Edits land in the staging area.** Before anything hits your real working tree, you see the proposed change in the diff pane. The model's own rationale ("Why this change?") sits next to each diff — drawn from its `claimed_changes` envelope.
5. **You accept or reject — per hunk in the GUI, per file in the TUI.** Accepted hunks atomically apply to disk; rejected hunks vanish. The accept path runs symlink-containment + per-file size checks again at commit time so a race between staging and apply can't escape the workspace.
6. **Verification fires.** Atelier checks that the changes actually match what the model claimed (did-it-do-what-it-said). Mismatches surface as `VerificationFailed` events.
7. **The turn ledger updates.** Token counts, latency, and (for local providers) latency-weighted cost land in the cost meter.

The model can `claimed_done: true` to end the run, or you can stop it any time with Ctrl+C — the harness preserves any partial output in the session's `recovery_log` slot so you can resume.

---

## Editable round-trips on every surface

The harness isn't read-only. The GUI and TUI both let you edit live state without restarting the run:

- **Context pane** — pin a file so the agent always sees it, unpin to free tokens, evict an item entirely (with a confirmation prompt that warns about cache busting).
- **Memory pane** — add a memory card, delete one, or promote a per-session card to your global memory at `~/.atelier/memory/` so it survives across sessions.
- **Plan pane** — add a step, mark it in-progress / done, attach a constraint, remove it, or drag-and-drop to reorder (GUI only; the TUI uses the same dispatcher surface via keyboard).
- **Mental-model panel** — off by default. When you enable it, the text you write is injected as a second System message on every adapter call, so the model has your latest understanding of the codebase at hand. The cost is disclosed in the panel's header (`~N tokens / turn`).

---

## Non-destructive context compaction with reversible Expand

When your context fills up, you don't have to start a fresh session. Select two or more items in the context pane and hit Compact:

1. The harness asks the active model to summarise the selected items.
2. The summary lands as a pinned memory card with a `compacted_from` link.
3. The original items are written to `.atelier/sessions/<sid>/compactions/<comp-uuid>.json`.
4. The compacted slots in context are freed.

The summary card has an `⤴ expand` button. Clicking it restores the original items and warns you about the cache-rewarm token cost before doing so. Compaction is reversible by design — you never lose the originals.

If the model returns a context-overflow error mid-turn, the harness can auto-compact the largest unpinned items and retry, up to a defence-in-depth retry cap (so a runaway compaction loop is bounded).

---

## Verification and safety

- **§7 verification.** After tool calls land, the harness checks the model's `claimed_changes` against the actual staged diffs. Tier-1 also probes language servers (TypeScript today) to catch hallucinated symbols — the model says "I added `foo`" but no LSP can find `foo` anywhere.
- **§11 sandbox.** Built-in tools and shelled-out subprocesses run with a default-deny network policy; reads are repo-scoped; writes to `/etc`, `/usr/local`, etc. are refused.
- **§12 audit log.** Every blocked network attempt, every MCP HTTP request, every LSP install prompt writes a structured row to `<workspace>/.atelier/sessions/<uuid>/audit.log`. Exportable for privacy review.
- **§14 concurrent-edit modal.** A file watcher tracks the dispatcher's read-set. If you (or another process) modify a file the agent has read, the next tool call pauses and the harness asks you whether to Reload (drop the queued call and re-read), Wait (keep the call queued), or Pause (5-minute auto-Reload fallback). The `--non-interactive` flag auto-resolves to Reload.
- **§14 crash recovery.** Sessions persist to `<workspace>/.atelier/sessions/<uuid>/session.json` with full atomic-write discipline (tempfile → fsync → rename → fsync of parent dir). After a `kill -9` mid-turn, `atelier run --resume <uuid>` picks up at the last fully-completed tool call; partial output lives in `recovery_log` and never gets confused for finished conversation.
- **Supply-chain gates.** `make audit` runs `cargo audit --deny warnings` against the Rust workspace, `npm audit --audit-level=high` against the GUI's frontend deps, and a Shai-Hulud / npm supply-chain IoC sweep (no malicious workflow file, no `preinstall`/`postinstall` hooks, every tarball resolved from `registry.npmjs.org`).

---

## MCP — external tools without rebuilding

Atelier is MCP-first. You can register external Model Context Protocol servers (stdio or HTTP/SSE transports) in `.atelier/mcp_servers.json`; the harness handshakes with each one, lists its tools, and surfaces them through the same dispatcher as the built-ins. Hooks, ledger, trust budget, and verification gates treat MCP-routed and built-in tools uniformly.

First use of an unfamiliar MCP server triggers an approval prompt — once you say yes, the approval is persisted to `.atelier/mcp_approvals.json` for the workspace.

MCP **resources** (read-only attachments a server can advertise) surface in your context pane with a `Provenance::McpResource` label so you always know where a piece of context came from.

---

## Cost and accounting

The cost ledger records every adapter call:

- **Token counts** — prompt, completion, cached (when the provider reports it).
- **Count source** — declared per-adapter as `Provider`, `Approx`, or `Inferred` so you know whether the number came from the wire or from a local heuristic.
- **Latency** — measured at the adapter boundary.
- **Cost in USD** — local providers (Mock, OpenAI-compat against a self-hosted server) get a latency-weighted `$0.00028/sec` attribution; cloud providers (Anthropic, hosted OpenAI) leave the field empty until per-provider pricing tables ship.

The §3 cost meter in the GUI/TUI footer shows the running total. The ledger is JSON, lives in `session.json`, and is yours — export it however you like.

---

## Hooks (§15)

Drop a hook manifest under `.atelier/hooks/` and it fires on the event you target (pre-tool, post-tool, user-prompt-submit, session-start). Hooks are non-blocking by spec — they can suggest, log, or annotate but they can't veto a tool call. The bundled `bounded-reads.sh` nudges the model when it tries to read a >500-line file without bounds; `save-nudge.sh` prompts you to consider saving a memory when your prompt looks like a durable directive.

Each hook needs explicit first-use approval, persisted to `.atelier/hook_approvals.json`. The approval is per-repo, so a hook approved in one workspace doesn't silently fire in another.

---

## Skills (§15)

Skills are named slash-invoked prompt expansions. The harness ships 14 bundled (`/review`, `/security-review`, `/test`, `/explain`, `/fix`, `/document`, `/refactor`, `/optimize`, `/commit`, `/changelog`, `/audit`, `/spec`, `/sweep`, `/scan`) and you can override or add new ones in `~/.atelier/skills/` (your scope) or `<workspace>/.atelier/skills/` (per-repo, checked into git).

Typing `/review` in the GUI Composer (or `atelier run /review` on the CLI) expands the skill's `prompt_template` with `${arg}` substitution and routes the expanded text as the next user turn. The §2.5 agent loop runs unchanged — skills are a prompt-expansion layer, not a new transport. Cost-ledger discipline: every skill invocation is annotated as `note: "skill: <name>"` on the next `model_call` ledger entry.

Authoring shortcuts:

```
atelier skills                 # list every registered skill (resolved + grouped)
atelier skills new my-helper   # scaffold a starter manifest in <repo>/.atelier/skills/
atelier skills validate        # lint every manifest in the registry
atelier skills show review     # print the resolved manifest + source path
```

The proactive-trigger surface (model self-suggests a skill via the §9 uncertainty UI) is **deferred** to a later bundle; bundled manifests that carry a `proactive_trigger` (e.g. `/security-review`) still work manually today.

---

## Sub-agent delegation (contract only today)

The §10.1 surface for spawning specialised sub-agents (researcher, test-runner, general-purpose, code-reviewer) is locked in via the bundled `spawn_subagent` tool. The schemas, manifests, and session-state slots all exist; the runtime that actually launches a sub-agent and gathers its findings lands in Phase D/E.

---

## What you control via `.atelier/`

```
<repo>/.atelier/
  providers.toml          # named provider profiles
  mcp_servers.json        # external MCP server registrations
  mcp_approvals.json      # first-use approvals (auto-managed)
  hooks/                  # per-repo hook manifests
  hook_approvals.json     # auto-managed
  sessions/<uuid>/
    session.json          # durable session state
    audit.log             # §12 audit rows (JSONL)
    compactions/          # reversible compaction blobs
    recovery_log          # partial output preserved across crashes
  memory/                 # per-project retrievable memory cards
  settings.json           # per-project hook + driver settings
```

Anything global lives in `~/.atelier/` instead:

```
~/.atelier/
  providers.toml          # cross-project provider defaults
  model_profiles/         # per-model probe-on-first-use cache
  memory/                 # cross-project memory cards
  registry.json           # session-uuid → workspace map (rebuildable)
```

---

## What the harness doesn't do (yet)

Set expectations honestly:

- **No Bedrock or Vertex AI adapter.** Anthropic + OpenAI-compatible cover the common cases today; Bedrock / Vertex are Phase E/F work.
- **No fully autonomous mode.** Even with `--non-interactive`, the verification gate, concurrent-edit modal, and approval flow are designed around having a human in the loop. The harness is a power tool, not an autopilot.
- **No multi-session orchestration.** One workspace, one active session at a time. The session registry knows about your past sessions, but launching a fan-out of agents is not a v1 feature.
- **No web UI or remote backend.** Atelier runs entirely on your machine. Your prompts go to your chosen model provider; nothing else phones home.
- **No built-in fine-tuning or local-model training.** Use the model you have; the harness is the loop around it, not a model factory.
- **Live-API CI gates are operator-wired.** The Phase A nightly gate runs `anthropic:claude-haiku-4-5` against five canonical tasks if (and only if) `ANTHROPIC_API_KEY` is wired into GitHub Actions secrets. Without that, the gate records `status: skipped` and stays out of your way.

---

## A minute-long quick start

```sh
# 1. Install rig deps (one-time; creates .venv/).
make install-rig

# 2. Sanity-check the install.
make check

# 3. Point Atelier at a model. Three choices:
#
#    Local LLM via Ollama:
ollama pull qwen2.5-coder:7b
cargo run -p atelier-cli -- run \
  --provider openai-compat \
  --base-url http://localhost:11434/v1 \
  --model local:qwen2.5-coder:7b \
  "Add a smoke test for the price-quote helper."

#    Anthropic Claude:
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p atelier-cli -- run \
  --provider anthropic \
  --model anthropic:claude-opus-4-7 \
  "Add a smoke test for the price-quote helper."

#    Mock adapter (no tokens, scripted):
cargo run -p atelier-cli -- run --provider mock "Try the workflow"

# 4. Or open the desktop app.
cargo tauri dev      # GUI
cargo run -p atelier-tui  # TUI
```

For deeper integration — named profiles, MCP server registration, hooks, custom workflows — see `ATELIER.md`. For the per-version trail of what landed when, see `CHANGELOG.md`.
