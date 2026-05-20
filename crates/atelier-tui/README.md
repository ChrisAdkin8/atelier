# atelier-tui

Terminal UI using `ratatui` + `crossterm`. Consumes the same `atelier-core` events as the GUI; renders the subset documented in spec §3.

## TUI subset (per spec §3)

- Conversation pane
- Textual live diff
- File tree (agent's working set)
- Plan canvas (collapsible tree)
- Cost meter
- Context-window meter
- Timeline scrubber — keys: `[` `]` step, `g <n>` jump

Not in TUI (GUI-only): drag-and-drop, Mermaid/D2 inline rendering, browser previews, visual hunk-rewrite, and native folder picking.

## Current state

**Driver mode.** Same `Runner` library the CLI uses, pumped into ratatui widgets. Two run modes:

- **Driver mode** — `cargo run -p atelier-tui -- "<prompt>"`. Builds a `Runner` with `ApprovalPolicy::AwaitApproval`, pops a yellow `(PENDING)` diff banner when staging hits the approval gate, and routes `y` / `n` keys through `SessionDispatcher::submit_approval`. Footer pivots to `APPROVAL REQUIRED · y accept all · n reject all · q quit` while a decision is pending.
- **Viewer mode** — `cargo run -p atelier-tui` (no prompt). Spawns a NoopHook session, subscribes to its bus, renders the panes — useful for testing the terminal lifecycle in isolation.

Panes wired: conversation, textual live diff (Hunks::Lines `@@` headers + `+`/`-`/`Created`/`Deleted`/`Binary`/`Same` badges), plan canvas tree, memory, sub-agents, cost meter, context meter (Gauge with `+N unknown` for `TokenSource::Unavailable` items), scrubber keys `[` / `]` / `g`, LSP install prompt, and slash-skill completion. `Event::ModelProfileLoaded` prints a one-line "ModelProfile · strategy=… · …" event in the log so the active §2 strategy is visible at-a-glance.

**v52 — model badge in the footer.** The right side of the help line renders `model_id · strategy · outcome` (cyan bold id, green strategy, dim outcome) for the lifetime of the run. Pre-event the footer is just the scrub-key help line; during an outstanding hunk approval the badge is suppressed so the yellow `APPROVAL REQUIRED` banner is the unambiguous focus. The split is one ratatui `Layout::default().direction(Horizontal).constraints([Min(0), Length(badge_width)])`; `model_badge_width()` computes the column count from the underlying strings so the layout matches what gets rendered.

**v53 — §5 Context panel in the right column.** Between the aggregate context gauge and the bounded event-log tail, `render_context_pane` renders one row per `ContextItemSummary` from `Event::ContextItems`: right-aligned token count (cyan exact / yellow approx / dim unavailable), short provenance badge (`init`/`usr`/`tool`/`mem`/`pin`/`asst`), pin glyph, label. Empty-state placeholder before the first event. Constraint shape `[Length(2), Length(2), Min(2), Length(4)]` keeps the gauges' 2-row allocation intact even when the terminal is tight; the §5 panel takes whatever remains.

**§5 Memory + §10.1 Sub-agents in the top-right column.** Top-right column is split between Plan, Memory, and Sub-agents. `render_memory_pane` shows one row per `MemoryCardSummary`; `render_subagents_pane` mirrors child-agent status and turn counters so delegated work is visible while the parent is waiting.

`cargo test -p atelier-tui` exercises the pure `render` + `apply` + `handle_key` + `project_event` + model-badge + §5 Context/Memory + §10.1 Sub-agent + slash-completion surfaces, plus the approval-key tests.

What's not here yet: file tree (needs `OnDiskSession.files` snapshot the actor doesn't surface), `g <n>` step-index prefix (needs §4 time-travel step count to clamp against).

## Quick start

```sh
cargo run -p atelier-tui -- "rename my-script"   # driver mode
cargo run -p atelier-tui                          # viewer mode
cargo test -p atelier-tui
```

## Architecture

```
   ┌──────────────────────────────┐
   │ crates/atelier-tui/src/      │
   │   lib.rs                     │
   │   - AppState (pure)          │   draws onto
   │   - render(buf, area, state) │──────────────┐
   │   - handle_key(KeyEvent)     │              ▼
   │   - run() (tokio loop)       │      ratatui::Terminal
   └──────────────────────────────┘     (CrosstermBackend, raw mode,
                 │ subscribes to                alternate screen)
                 ▼
   ┌──────────────────────────────┐
   │ atelier_core::session::      │   broadcast::Receiver<Event>
   │   spawn(...) → Handle        │
   └──────────────────────────────┘
```

The split is deliberate:

- **`AppState`** + **`apply`** + **`project_event`** + **`render`** + **`handle_key`** are pure. No I/O. Tests exercise them via `Buffer::empty(Rect)` instead of a real terminal.
- **`run`** owns the impure parts: raw mode, alternate screen, the `tokio::select!` loop, `spawn_blocking` around `crossterm::event::poll`. It also installs a `TerminalGuard` RAII restorer so a panic past terminal-setup still puts the user's terminal back into a sane state.

Adding a new `Event` variant means one match arm in `project_event` and (if it changes display state) one arm in `apply`. Widgets compose by adding rows to the `Layout` constraints in `render`.

## Anti-bootstrap

- Don't depend on Tauri or anything web-stack from this crate. TUI is a separate binary; the only shared code is via `atelier-core`.
- Don't put loop logic here. Events come from `atelier-core`; this crate only renders and forwards user input.
- Don't read events directly off the broadcast inside the render path. Mutate `AppState` in `apply`; render reads `AppState`. Mixing the two is what makes terminal UIs become unmaintainable.
- Don't add a Cancel command from the TUI yet. The §2.5 actor's cancel semantics are typed and tested; wiring a keypress into them needs the typed-command direction added first.

## Spec references

- §3 Workspace UI (TUI subset)
- §2.5 Agent loop (this crate is an event consumer, not a producer of loop state)
