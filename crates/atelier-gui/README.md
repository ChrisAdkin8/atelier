# atelier-gui

Tauri 2.x shell. Consumes `atelier-core` over a broadcast channel; renders the workspace described in spec §3.

## Current state

**Chat/Agent workspace.** Svelte 5 layout backed by a Tauri shell. Chat mode talks to the configured adapter directly for conversational turns; Agent mode uses `atelier-cli::Runner` for tool-using flows. The same Rust shell exposes workspace state, provider swapping, memory, context, skills, and sub-agent progress.

What's wired:

- **Panes**: Header / ConversationPane / ContextPane / MemoryPane / SubagentPane / MetersPane / Composer, composed in `App.svelte` as a CSS grid.
- **Event bus**: subscribes to `atelier://event` and folds events through a pure-TS `applyEvent` reducer mirroring the TUI state machine. The new v51 `ModelProfileLoaded` event is projected through `bridge_event` so the strategy badge can render off it.
- **Chat turns**: the Composer has explicit **Chat** and **Agent** modes. Chat mode uses `start_chat_run` and sends messages to `adapter.chat(messages, &[])` so chat-only OpenAI-compatible providers work without tool-call support. Agent mode uses `start_agent_run` for Runner-backed flows that need tools and sub-agent events; the selected provider must support tool calls.
- **Defensive plumbing**: concurrent-run guard via `Arc<AtomicBool>`, 64 KB prompt cap, per-run workspace cleanup via `RunCleanup` drop guard, `listenerReady` gate so a fast user can't lose the first run's events, provider-swap base-URL allowlist shared with `atelier-core` (OpenAI, Anthropic, loopback, and the project-owned Atelier dev vLLM ALB), bounded `~/.atelier/gui.toml` parsing, TOML-based workspace persistence, and durable resume-pointer validation before every Runner-backed Agent submit.
- **Model badge** (v52, expanded v60.83): footer's bottom-right renders the active model plus its suitability score (`Excellent 95`, etc.) for the lifetime of the run. Clicking opens the score breakdown, strengths, and risks. Populated when `ModelProfileLoaded` lands.
- **§5 Context panel** (v53): bottom-right slot stacks `MetersPane` (fixed) above the new `ContextPane.svelte` (flex). Renders one row per `ContextItemSummary` from `Event::ContextItems` — right-aligned token count (colour-cued: cyan exact / yellow approx / dim unavailable), short provenance badge (`init`/`usr`/`tool`/`mem`/`pin`/`asst`), and the item's label with a tooltip carrying the full provenance trace. Empty-state placeholder before the first `ContextItems` event.
- **§5 Memory panel**: top-right slot stacks `MemoryPane` and `SubagentPane`. Memory rows support add/delete/promote interactions; promoted cards persist to `~/.atelier/memory/`, while workspace-scoped auto-drafts live under `<workspace>/.atelier/memory/`.

What's intentionally *not* here yet:

- File tree pane (needs `OnDiskSession.files` snapshot the actor doesn't surface yet).
- GUI DiffPane / `submit_approval` path. The current GUI is chat-first; file-level approval remains in the TUI/dispatcher surface.
- Real icons. `icons/icon.png` is a 32×32 placeholder so dev builds succeed; replace before the first signed release.
- Codesign / notarization. The release workflow can build unsigned `.dmg`,
  `.AppImage`, and `.deb` bundles for GitHub Releases.

## Quick start

For end users, download the GUI bundle for your platform from the latest GitHub
Release. The current bundles are unsigned; macOS users may need to approve the
app in System Settings until signing and notarization are configured.

For local development:

```sh
cargo install tauri-cli --version "^2.0" --locked    # one-time per machine
npm --prefix crates/atelier-gui/ui install            # one-time per checkout
cd crates/atelier-gui && cargo tauri dev              # spins up Vite + Rust shell + webview
```

`cargo tauri dev` runs the Vite dev server (port 1420), waits for it to be ready, builds the Rust shell, and opens the webview. Hot-reload works for the Svelte side; the Rust shell rebuilds on save and the webview re-opens.

Use **Browse…** in the header to select the repo/workspace you want Atelier to operate on. The selection is persisted in `~/.atelier/gui.toml`. Agent-mode follow-up submits resume from the last durable session in that workspace; if `session.json` has been deleted or cleaned up, the GUI clears the stale pointer and starts a fresh session instead of failing the next prompt.

For tests without the webview:

```sh
cargo test -p atelier-gui
npm --prefix crates/atelier-gui/ui run check   # svelte-check + tsc
npm --prefix crates/atelier-gui/ui run build   # production frontend build
```

## How the bootstrap landed (decisions + mechanical steps, in retrospect)

The decisions captured up front (D1–D4) and the resulting on-disk files:

| Decision | Chosen | Where it lives |
|---|---|---|
| D1 Bundle identifier | `dev.atelier.app` (placeholder — swap before signed release) | `tauri.conf.json` `identifier` |
| D2 App name | `Atelier` | `tauri.conf.json` `productName` + the single window `title` |
| D3 Frontend stack | TypeScript + Vite + Svelte 5 | `ui/` |
| D4 Dev server URL | `http://localhost:1420` | `tauri.conf.json` `build.devUrl` + `ui/vite.config.ts` |

Mechanical steps were generated by hand rather than via interactive `cargo tauri init`, so the file shapes are explicit:

- `tauri.conf.json` — pinned schema, one `main` window, narrow CSP (`default-src 'self'`).
- `capabilities/default.json` — deliberately narrow: only `core:default` + `core:event:default`. No fs/shell/http capabilities — webview can't bypass the §15 dispatcher's sandbox.
- `build.rs` — three lines, calls `tauri_build::build()`.
- `icons/icon.png` — 32×32 transparent placeholder. Generate real icons with `cargo tauri icon path/to/source.png` before any signed release.
- `Cargo.toml` — `tauri`, `tokio`, `tracing(-subscriber)`, `serde`, `serde_json` deps live; `tauri-build` as build-dep.
- `ui/` — Vite + Svelte 5 + TypeScript, plus `@tauri-apps/api` for the event subscription in `App.svelte`. Vite pinned to `strictPort: true` on 1420 so it can't silently roll over to 1421 and 404 the webview.

## Architecture

```
   ┌──────────────────────────────┐
   │ ui/src/App.svelte            │   listen('atelier://event', …)
   │ (Svelte 5 + Vite dev server) │◀─────────────────┐
   └──────────────────────────────┘                  │
                                                     │ Tauri IPC (event bus)
                                                     │
   ┌──────────────────────────────┐                  │
   │ crates/atelier-gui/src/      │   AppHandle::emit│
   │   lib.rs · main.rs           │──────────────────┘
   │   - spawns session::Handle   │
   │   - bridge_event(…) → JSON   │
   │   - Tauri::Builder::run()    │
   └──────────────────────────────┘
                 │ subscribes to
                 ▼
   ┌──────────────────────────────┐
   │ atelier_core::session::      │   broadcast::Sender<Event>
   │   spawn(…) → Handle          │
   └──────────────────────────────┘
```

`bridge_event` is a pure function (unit-tested) that projects each
`session::Event` variant onto a `{kind, payload}` JSON shape the webview
matches on. Adding a new event variant means adding one arm here and one
case in `App.svelte` — no schema change in `atelier-core`.

## Anti-bootstrap (don't)

- Don't run `cargo tauri init` from the repo root — it pollutes the workspace with frontend cruft outside `crates/atelier-gui/`.
- Don't pick a bundle identifier you don't own *for a codesigned build*. `dev.atelier.app` is fine for local development; swap it for one you own before the first signed release.
- Don't configure codesign / notarization / installers in Phase A. Local development only.
- Don't add Tauri plugins ad-hoc — every plugin is a capability expansion that needs review against §11.
- Don't build a `SessionViewModel` aggregator in `atelier-core` before the multi-pane workspace exists. The typed `ContextManager` / `MemoryStore` / `PlanCanvas` / `Ledger` (and `bridge_event` here) are already the right consumption surface.
- Don't expand `capabilities/default.json` beyond what a panel actually needs. The default capability list intentionally excludes `fs`, `shell`, and `http` — the webview must go through the Rust shell, which goes through the §15 dispatcher.

## Spec references

- §3 Workspace UI
- §2.5 Agent loop (this crate is an event consumer, not a producer of loop state)
- §11 Security (capability scoping)
- §14 Persistence (concurrent-edit modal lives here)
