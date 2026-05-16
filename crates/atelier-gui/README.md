# atelier-gui

Tauri 2.x shell. Consumes `atelier-core` over a broadcast channel; renders the workspace described in spec §3.

## Current state

Scaffold only. `cargo tauri init` has not been run, so `tauri.conf.json`, `build.rs`, `icons/`, `capabilities/`, and the frontend skeleton do not exist. The Tauri workspace deps are declared at the repo root but commented out in this crate's `Cargo.toml` so `cargo build` doesn't fail before init runs.

## Bootstrap (implementor's first day)

1. **Install the Tauri 2 CLI.** `cargo install tauri-cli --version "^2.0"` (one-time per machine).
2. **Run `cargo tauri init`** from this directory. It is interactive — answer the questions per step 3.
3. **Answers to give**:
   - **App name**: `Atelier`.
   - **Window title**: `Atelier`.
   - **Frontend stack**: **TypeScript + Vite + Svelte** (small bundle, fast cold start). Solid or React work; avoid plain JS for the multi-pane UI in spec §3.
   - **Dev server URL**: `http://localhost:1420` (Vite default).
   - **Web assets location**: defaults to `../dist`; keep it relative to the crate root.
   - **Bundle identifier**: reverse-DNS, e.g., `dev.atelier.app`. Pin this — changing it later breaks codesign chains.
4. **Configure capabilities** (Tauri 2 puts these in `capabilities/`). Atelier needs:
   - Filesystem read scoped to the user's repo (write scope is gated at runtime by §11 sandbox + §8 trust budget).
   - Shell execution (sandboxed per §11).
   - HTTP for remote LLM providers and MCP HTTP/SSE servers.
   - Notification (for the §14 concurrent-edit modal).
5. **Uncomment the Tauri deps** in `Cargo.toml` — only after step 2 has generated `tauri.conf.json` and `build.rs`.
6. **Verify `build.rs`** exists in this crate's root and calls `tauri_build::build()`. `tauri init` should produce this; if it doesn't, write it yourself.
7. **Initialise the frontend**: `cd <frontend-dir> && npm create vite@latest .` (or framework equivalent). Add `@tauri-apps/api` for IPC.
8. **First end-to-end demo**: emit a synthetic event from `atelier-core`, subscribe in the frontend via Tauri's event API, render it. Smallest "atelier-gui consumes atelier-core" milestone and the first real Phase A deliverable for this crate.

## Anti-bootstrap (don't)

- Don't run `tauri init` from the repo root — it pollutes the workspace with frontend cruft outside `crates/atelier-gui/`.
- Don't pick a bundle identifier you don't own. `dev.atelier.app` is fine until you have a real domain.
- Don't configure codesign / notarization / installers in Phase A. Local development only.
- Don't add Tauri plugins ad-hoc — every plugin is a capability expansion that needs review against §11.

## Spec references

- §3 Workspace UI
- §2.5 Agent loop (this crate is an event consumer, not a producer of loop state)
- §11 Security (capability scoping)
- §14 Persistence (concurrent-edit modal lives here)
