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

Not in TUI (GUI-only): drag-and-drop, Mermaid/D2 inline rendering, browser previews, visual hunk-rewrite.

## Current state

**Phase C unblock (4) — bootstrapped (v40).** `cargo run -p atelier-tui` opens an alternate-screen UI that subscribes to the `atelier-core` broadcast bus and renders the event log + an `EditStaged` counter live. `cargo test -p atelier-tui` exercises 10 unit tests against the pure `render` + `apply` + `handle_key` surface.

Multi-pane widgets (conversation, diff, file tree, plan canvas, cost + context meters, timeline scrubber) sit on top of this foundation. The bootstrap intentionally ships one panel: the smallest snapshot that proves the broadcast bus reaches the terminal.

## Quick start

```sh
cargo run -p atelier-tui       # opens the TUI; q / Esc / Ctrl-C to quit
cargo test -p atelier-tui      # 10 unit tests on the pure render + state
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
- Don't add a Cancel command from the TUI yet. The §2.5 actor's cancel semantics are typed and tested; wiring a keypress into them needs the typed-command direction added first (mirrors the `ping`-only IPC in `atelier-gui`).

## Spec references

- §3 Workspace UI (TUI subset)
- §2.5 Agent loop (this crate is an event consumer, not a producer of loop state)
