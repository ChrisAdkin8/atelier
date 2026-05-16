# atelier-cli

Headless command-line entry point. Produces the `atelier` binary. Depends only on `atelier-core` — no TUI, GUI, or web-stack pulls.

Spec references: §11 (project bootstrap, credential storage).

## Current state

One subcommand implemented:

- `atelier init [PATH]` — bootstrap a repo at `PATH` (defaults to `cwd`). Idempotent; never overwrites an existing `ATELIER.md`. Backed by `atelier_core::init`.

Planned (spec §11 credential storage; not yet implemented):

- `atelier login <provider>`
- `atelier logout <provider>`
- `atelier rotate <provider>`
- `atelier whoami`

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
```
