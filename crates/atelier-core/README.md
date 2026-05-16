# atelier-core

The Atelier harness core. No UI dependencies. Everything the agent loop, BYOM adapters, MCP client, session state, checkpoints, and ledger need lives here. `atelier-gui` and `atelier-tui` consume this crate over a broadcast channel.

Spec references: §1, §2, §2.5, §4, §14, §15.

## Current state

Scaffold only. The error taxonomy (§2.5 "Tool error model") is implemented and unit-tested in `src/error.rs`. Nothing else is here yet.

## Build

```
cargo build -p atelier-core
cargo test  -p atelier-core
```

## `rmcp` dependency wiring

`rmcp` is the official Rust SDK for the **Model Context Protocol** — Atelier's tool transport (spec §15). There is **no separate install step**: `rmcp` is a Cargo dependency that resolves from crates.io on first build.

### Where `rmcp` lives

The dependency is declared in two coordinated places — the version pin at the workspace root, and the actual consumer here in `atelier-core` (the crate that owns the MCP client; `atelier-gui`, `atelier-tui`, and `atelier-cli` reach `rmcp` transitively through `atelier-core` when they need to).

**1. Workspace root** — `../../Cargo.toml`:

```toml
[workspace.dependencies]
rmcp = "0.1"
```

**2. Consuming crate** — `Cargo.toml` (this crate):

```toml
[dependencies]
rmcp = { workspace = true }
```

This pattern — pin the version once at the root, reference it as `{ workspace = true }` from each consuming crate — is how every workspace dependency is wired. It keeps versions synchronized across crates and means a bump only happens in one place.

If a future workspace crate ever needs `rmcp` directly (rather than via `atelier-core`), add the same `rmcp = { workspace = true }` line to its `[dependencies]` — **never** redeclare the version.

### Fetch and verify

```sh
cargo fetch                       # download rmcp + transitive deps from crates.io
cargo check -p atelier-core       # confirm rmcp resolves and compiles cleanly
```

A successful `cargo check` ends with a line like:

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.85s
```

### Troubleshooting

- **`feature edition2024 is required`** — your toolchain is older than 1.85.0. See [`../../docs/toolchain.md`](../../docs/toolchain.md) for the pinned-toolchain story.
- **Network errors during `cargo fetch`** — `rmcp` and its transitive deps are pulled from crates.io. Check your network, or set `CARGO_HTTP_PROXY` if you're behind a corporate proxy.

### The maturity spike

For the standalone `rmcp` maturity-assessment spike — a separate experiment, not part of the Cargo workspace — see `../../experiments/rmcp_spike/README.md`. Its outcome (GO / GO-WITH-CAVEATS / NO-GO) is a Phase A prerequisite per `../../tasks/todo.md`.

## What lives here (planned)

- `error` — tool error taxonomy with state-machine recovery routing *(present)*
- `protocol` — Model Protocol envelope types + emission/parsing for the three §2 strategies
- `adapter` — BYOM `Adapter` trait + first-party adapters (Anthropic, OpenAI, LiteLLM-shaped, Ollama)
- `mcp` — MCP client wrapping `rmcp`; stdio + HTTP/SSE transports; server registration
- `tools` — built-in tools exposed via the same MCP interface (read/write/edit, shell, grep, ast-grep)
- `loop` — §2.5 state machine, per-session actor, cancellation
- `session` — session artifact (per `schemas/session/v1.json`), checkpoint tree, ledger
- `verify` — §7 verification gate runner; did-it-do-what-it-said diff
- `secrets` — OS keychain (`keyring`) + env-var fallback; `${env:…}` / `${keychain:…}` interpolation
