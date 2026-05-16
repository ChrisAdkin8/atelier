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

Scaffold only. `ratatui` and `crossterm` deps are declared in the workspace but commented in this crate's `Cargo.toml` until the first widget lands.

## Bootstrap (implementor's first day)

Unlike `atelier-gui`, there is no interactive init step — `ratatui` is just a Rust crate.

1. **Uncomment** `ratatui`, `crossterm`, `tokio`, and `tracing-subscriber` in this crate's `Cargo.toml`.
2. **Pick a layout primitive.** `ratatui::layout::Layout::new(Direction::Vertical, [...])` is the standard; the §3 TUI subset (conversation, diff, file tree, plan canvas, two meters, scrubber) maps to nested `Layout`s. Sketch the geometry before writing widgets.
3. **Wire input.** Spawn a blocking thread (or `tokio::task::spawn_blocking`) around `crossterm::event::read` and feed an `mpsc` into the main event loop. Key bindings for the spec: `[` `]` scrubber step, `g <n>` jump, plus the usual Vim-ish navigation.
4. **Subscribe to `atelier-core`.** Open a `tokio::sync::broadcast::Receiver` from the core's session-event channel; redraw on receive. The same channel feeds the GUI — keep render logic out of `atelier-core`.
5. **First milestone.** Conversation pane + cost meter rendering live from a real `atelier-core` session. Diff pane and scrubber come next; the plan-canvas tree last.

## Anti-bootstrap

- Don't depend on Tauri or anything web-stack from this crate. TUI is a separate binary; the only shared code is via `atelier-core`.
- Don't put loop logic here. Events come from `atelier-core`; this crate only renders and forwards user input.
