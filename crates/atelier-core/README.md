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
