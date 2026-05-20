# atelier-cli

Hybrid library + binary. Produces the `atelier` binary; exports a `Runner` library the GUI and TUI link against for their own driver modes. Depends on `atelier-core` — no GUI / web-stack pulls (the GUI lives in `atelier-gui`).

Spec references: §11 (project bootstrap, credential storage), §1 (BYOM Runner), §2.5 (agent-loop driver).

## Current state

Two subcommands implemented:

- `atelier init [PATH]` — bootstrap a repo at `PATH` (defaults to `cwd`). Idempotent; never overwrites an existing `ATELIER.md`. Backed by `atelier_core::init`.
- `atelier run [OPTIONS] [PROMPT]` — drive the agent loop. Wires the §2.5 actor + §15 dispatcher + eight built-in tools + registered MCP tools + §15 hooks + §7 DoD + §11 sandbox + §1 typed ledger + §1 probe-on-first-use against the chosen adapter; loops turns until `claimed_done: true`; transitions to `Verifying` for DoD checks; persists the session to `<repo>/.atelier/sessions/<uuid>/`. Flags:
  - `--provider {mock,anthropic,openai-compat}` — chosen BYOM adapter.
  - `--model <ID>` — `<provider>:<model>` form. Required for the network providers; ignored for `mock`.
  - `--base-url <URL>` — `openai-compat` only. Full URL ending in `/v1`. Omit to use OpenAI itself.
  - `--workspace PATH`, `--max-turns N`, `--prompt-file PATH` (or `-` for stdin).
  - `--no-probe` / `--force-probe` (v51) — skip or force the probe-on-first-use calibration.

Planned (spec §11 credential storage; not yet implemented):

- `atelier login <provider>` — extends to non-API-key shapes too (`atelier login bedrock` verifies the AWS chain; `atelier login vertex` verifies ADC; `atelier login ollama` is a no-op).
- `atelier logout <provider>`
- `atelier rotate <provider>`
- `atelier whoami`

## Architecture

`atelier-cli` is a hybrid lib+bin (v47). The agent-loop logic lives in `src/runner.rs` as a pure `Runner` API; `src/lib.rs` re-exports the blessed types (`Runner`, `ProviderChoice`, `MockResponse`, `EventSink`, `RunError`, `RunReport`, `DispatcherHandle`, `ApprovalPolicy`, `ProbePolicy`). Integration tests and the GUI/TUI driver modes link against this library; the binary `src/main.rs` is argv parsing + `Runner::new(...).run(prompt)`. Sink choice is `EventSink::{Stdout, Capture, Null, Callback}` — `Stdout` for the binary, `Capture` for tests asserting on event sequences, `Null` for tests that don't care, `Callback` for the GUI's webview bridge.

`Runner::new` is fallible: real providers (`Anthropic`) need credentials at construction time, so a missing `ANTHROPIC_API_KEY` surfaces as `RunError::Config` rather than failing on the first chat call. The `Mock` and `OpenAiCompat` branches are infallible (empty `OPENAI_API_KEY` is allowed — most local servers don't require auth; a 401 from a server that does surfaces as `AdapterError::Auth` on first call).

## Provider notes

- **`mock`** — in-tree `MockAdapter`. The binary's `run` command queues no responses; the integration tests in `tests/run_integration.rs` script them directly via `ProviderChoice::Mock { responses }`.
- **`anthropic`** — `crates/atelier-core/src/adapter/anthropic.rs`. Talks to `POST /v1/messages` (`anthropic-version: 2023-06-01`). Streams via SSE; native tool use via the `tool_use` content block so the §2 envelope rides as the `harness_meta` tool's arguments. Tests use `wiremock` to stand up a fake endpoint — no live API calls in CI. Reads `ANTHROPIC_API_KEY` from the environment.
- **`openai-compat`** (v50) — `crates/atelier-core/src/adapter/openai_compat.rs`. Talks to `POST <base_url>/chat/completions` against any OpenAI-shaped server. Tested against LM Studio, llama-server, vLLM, sglang, Ollama (its `/v1/` compat layer), and OpenAI itself. Streams via SSE; tool calls round-trip through OpenAI's `tool_calls` array (each `function.arguments` is JSON-encoded on the wire, parsed back into `serde_json::Value` for `ToolCallRequest`). HTTP error mapping aligns with `anthropic.rs`. 19 wiremock tests. Reads `OPENAI_API_KEY` (optional — empty allowed) and `OPENAI_BASE_URL` (overridable via `--base-url`).

## Probe-on-first-use (v51)

The first time the Runner sees a `(model_id, base_url)` pair under an `openai-compat` provider, it fires two short calibration calls — one tests native tool use, one tests the JSON-sentinel envelope — and caches the resulting `ModelProfile` to `~/.atelier/model_profiles/<sha256-hex>.json`. Cache hit on subsequent runs is free. `Mock` and `Anthropic` are well-characterised and skip the probe by default. CLI overrides: `--no-probe` (skip; use capability defaults) and `--force-probe` (re-probe even if cached). Implementation: [`crates/atelier-core/src/adapter/model_profile.rs`](../atelier-core/src/adapter/model_profile.rs).

## Config file (v53)

CLI flags layer on top of an optional `.atelier/providers.toml` that can declare multiple named profiles. Precedence, top wins:

```text
  1. CLI flags                                    (per-invocation)
  2. Resolved profile (from providers.toml)       (named, persisted)
  3. Built-in defaults                            (mock, 32 turns, auto probe)
```

The "resolved profile" is whichever `[providers.<name>]` table matches `--profile <NAME>` from the CLI, or the file's top-level `default = "<name>"` field. Per-field flags (`--provider`, `--model`, `--base-url`, `--max-turns`, `--no-probe`/`--force-probe`) still override individual fields of the resolved profile.

The binary prints `atelier run: using config <path> (profile "<name>")` on every run so you can confirm what's active. A malformed file is fatal (exit 2 with the path + parse-error message) — silently ignoring it would let a typo slip the runtime back to defaults. Schema, discovery rules, and validation live in [`crates/atelier-core/src/config.rs`](../atelier-core/src/config.rs); the top-level [README §5](../../README.md#5-configure-with-atelierproviderstoml--v53) walks through the format and worked examples.

In `main.rs` the layering is implemented as a flat top-down narrative — `parse_cli` → resolve workspace → `ProvidersConfig::load` → `resolve_profile` → `resolve_provider_choice` / `resolve_probe_policy` → build `Runner`. Each stage hands typed values to the next; nothing reaches the Runner that hasn't been validated.

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
