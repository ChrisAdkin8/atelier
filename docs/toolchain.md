# Rust toolchain setup

The workspace targets **Rust 1.85.0**, pinned via `rust-toolchain.toml`. This exact minimum is forced by Cargo's `edition2024` feature, which `rmcp-macros 0.1.5` (a transitive dependency of `rmcp`) requires. Earlier toolchains fail the build with:

```
error: feature `edition2024` is required
```

You don't install the toolchain directly. Instead install **`rustup`** — the toolchain manager that bundles `cargo` and `rustc` — and it will fetch Rust 1.85.0 automatically the first time `cargo` runs inside this repo (honoring `rust-toolchain.toml`).

## 1. Install `rustup`

**macOS / Linux:**

```sh
# Install rustup (cargo, rustc, toolchain manager).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Load cargo into the current shell (or open a new terminal).
source "$HOME/.cargo/env"
```

**Windows:** download and run `rustup-init.exe` from <https://rustup.rs>, then open a new terminal.

## 2. Verify

Run these **from inside this repo** — `rustup` honors `rust-toolchain.toml` only when invoked within a workspace that has one:

```sh
cargo --version       # cargo 1.85.0 (...)
rustc --version       # rustc 1.85.0 (...)
```

The first `cargo` invocation triggers `rustup` to download the pinned toolchain — expect a one-time delay of ~30–90 seconds. Subsequent invocations are instant.

## Troubleshooting

- **`feature edition2024 is required`** — your toolchain is older than 1.85.0. Re-check `rustc --version` and confirm you ran `source "$HOME/.cargo/env"` in the current shell. If `rustup` was installed before the 1.85.0 pin landed, run `rustup update`.
