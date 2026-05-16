# atelier-cli

Headless command-line entry point. Produces the `atelier` binary. Depends only on `atelier-core` — no TUI, GUI, or web-stack pulls.

Spec references: §11 (project bootstrap, credential storage).

## Current state

Two subcommands implemented:

- `atelier init [PATH]` — bootstrap a repo at `PATH` (defaults to `cwd`). Idempotent; never overwrites an existing `ATELIER.md`. Backed by `atelier_core::init`.
- `atelier run [OPTIONS] [PROMPT]` — drive the agent loop. Wires the §2.5 actor + §15 dispatcher + 7 built-in tools + §15 hooks + §7 DoD + §11 sandbox + §1 typed ledger against the chosen adapter; loops turns until `claimed_done: true`; transitions to `Verifying` for DoD checks; persists the session to `<repo>/.atelier/sessions/<uuid>/`. Flags: `--provider mock|anthropic`, `--model anthropic:claude-opus-4-7` (required prefix when `--provider anthropic`), `--workspace PATH`, `--max-turns N`, `--prompt-file PATH` (or `-` for stdin). Phase C unblock (1) + (2).

Planned (spec §11 credential storage; not yet implemented):

- `atelier login <provider>` — extends to non-API-key shapes too (`atelier login bedrock` verifies the AWS chain; `atelier login vertex` verifies ADC; `atelier login ollama` is a no-op).
- `atelier logout <provider>`
- `atelier rotate <provider>`
- `atelier whoami`

## Architecture

`atelier-cli` is a thin shell. The actual agent-loop logic lives in `src/runner.rs` as a pure `Runner` API, so integration tests (and future GUI/TUI wiring) drive it without going through the binary. The binary itself is argv parsing + `Runner::new(...).run(prompt)`. Sink choice is `EventSink::{Stdout, Capture, Null}` — `Stdout` for the binary, `Capture` for tests asserting on event sequences, `Null` for tests that don't care.

`Runner::new` is fallible: real providers (`Anthropic`) need credentials at construction time, so a missing `ANTHROPIC_API_KEY` surfaces as `RunError::Config` rather than failing on the first chat call. The `Mock` branch is infallible.

## Provider notes

- **`mock`** — in-tree `MockAdapter`. The binary's `run` command queues no responses; the integration tests in `tests/run_integration.rs` script them directly via `ProviderChoice::Mock { responses }`.
- **`anthropic`** — `crates/atelier-core/src/adapter/anthropic.rs`. Talks to `POST /v1/messages` (`anthropic-version: 2023-06-01`). Streams via SSE; native tool use via the `tool_use` content block so the §2 envelope can ride as the `harness_meta` tool's arguments. Tests use `wiremock` to stand up a fake endpoint — no live API calls in CI. The binary reads `ANTHROPIC_API_KEY` from the environment.

## Build

```sh
cargo build -p atelier-cli              # debug build -> target/debug/atelier
cargo build -p atelier-cli --release    # release build -> target/release/atelier
```

## Install

```sh
cargo install --path crates/atelier-cli # puts `atelier` on $PATH (~/.cargo/bin)
```

Verify:

```sh
atelier --version
atelier init /tmp/demo-repo
atelier run --help
```
