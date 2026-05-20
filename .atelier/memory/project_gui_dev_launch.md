---
name: project_gui_dev_launch
description: How to correctly launch the atelier GUI in development — must use cargo tauri dev, not cargo run
metadata:
  type: project
---

Use `cargo tauri dev` (from `crates/atelier-gui/`) to launch the GUI, not `cargo run -p atelier-gui`.

**Why:** Tauri's dev build expects a Vite dev server at `http://localhost:1420`. `cargo tauri dev` starts both the Vite server and the Rust binary together. `cargo run` skips the Vite step — the webview connects to nothing and shows a blank white window.

**How to apply:** Whenever the user asks to "fire up the GUI" or launch atelier-gui, always use:
```sh
cd crates/atelier-gui && cargo tauri dev
```
For a production binary (no dev server needed), use `cargo build --release -p atelier-gui` which bundles the frontend via the `custom-protocol` feature (v60.42).
