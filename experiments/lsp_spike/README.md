# `async-lsp` maturity spike

**Status: harness landed v60.25 (Phase B Track C1). Verdict: PENDING — operator must execute.** Mirror of `experiments/rmcp_spike/`. This spike establishes whether `async-lsp` (the Rust LSP client library) is viable as `atelier-core`'s LSP client for §7 Tier-1 verification, or whether the foundation needs to re-spec around a hand-rolled JSON-RPC + LSP message handler. Verdict will be recorded in the **Outcome** section at the bottom; smells worth flagging belong in the matrix as the spike runs.

The spike code in `src/main.rs` runs three modes against the published `typescript-language-server` npm package:

- `stdio` — happy path: spawn, `initialize` handshake, `textDocument/didOpen` a file with a known type error, wait for the matching `publishDiagnostics`, `shutdown` cleanly.
- `crash` — kill the spawned server with `SIGKILL` and check that `async-lsp` surfaces a typed error rather than hanging.
- `decline` — exit cleanly without sending `initialized`, simulating the user declining the first-use prompt; verify the launcher tears down without leaking the child process.

## Procedure

1. **Setup** (5 min):
   ```sh
   cd experiments/lsp_spike
   # Install typescript-language-server. `npx` auto-fetches it on first use; the
   # alternative is `npm install -g typescript-language-server typescript`.
   which npx  # Node 22+ recommended; matches the reference machine.
   cargo build
   ```

2. **Stdio test** (10 min):
   ```sh
   cargo run -- stdio
   ```
   Expected: registers `npx -y typescript-language-server --stdio`, completes the LSP `initialize` handshake, opens a fixture file with a deliberately wrong method call (`foo.nonExistentMethod()`), receives the diagnostic, prints the JSON, exits 0.

3. **Crash recovery test** (5 min):
   ```sh
   cargo run -- crash
   ```
   Expected: spawns the LSP server, kills it mid-dispatch with SIGKILL, observes how `async-lsp` surfaces the failure. Look for: clean `Result::Err`, no panic, no hung child process, reasonable error message.

4. **Decline path** (5 min):
   ```sh
   cargo run -- decline
   ```
   Expected: spawns the server, exits without sending `initialized`, verifies the child terminates and no zombie processes remain. This is the path the user takes when they decline the first-use prompt.

5. **API stability survey** (10 min):
   Read the `async-lsp` changelog and recent commits. Note: how often do public APIs change? Is it post-1.0 yet? Any pinned compatibility issues with the surrounding `tower` / `tokio` versions? Match the rmcp spike's "binary size impact" row too.

6. **Fill in the matrix below** and commit to this README.

## Decision matrix (to fill in)

| Capability | Outcome | Notes |
|---|---|---|
| stdio transport — register a server | _pending_ | |
| stdio — initialize handshake | _pending_ | |
| stdio — `textDocument/didOpen` + receive `publishDiagnostics` | _pending_ | |
| stdio — graceful shutdown via `shutdown` + `exit` | _pending_ | |
| Crash recovery — server killed mid-dispatch | _pending_ | |
| Decline path — exit without `initialized` | _pending_ | |
| API stability — public surface stable? | _pending_ | |
| Binary size impact on `atelier-core` | _pending_ | |
| Total session time | _pending_ | |

### Smells worth flagging for the Track C1 foundation bundle

(Operator: list anything you notice. The rmcp spike found 5 smells in 25 minutes; budget similar.)

## Decision

Pick one:

- **GO** — at minimum: stdio works end-to-end, crash is a clean error (not a panic / hang), decline path tears down cleanly. Proceed with the Track C1 foundation as written: `LspServerHandle` mirroring `McpServerHandle`, `launch_typescript_server` mirroring `launch_stdio_server`.
- **GO WITH CAVEATS** — stdio works but one of crash recovery / decline path / shutdown has issues. Document the caveat in `tasks/phase_b_closeout.md` and adjust the foundation scope.
- **NO-GO** — stdio is broken, `async-lsp` panics, or the crate is unmaintained. Re-spec the foundation around a hand-rolled JSON-RPC + LSP layer (LSP's wire format is well-documented; ~600–1000 lines of Rust to implement the client side).

## Outcome

- Decision: **PENDING**
- Date: _to fill_
- Operator: _to fill_
- `async-lsp` version actually tested: _to fill_
- Caveats / re-spec notes:
  - _to fill_

Once the verdict is recorded, link the actually-tested `async-lsp` version into `crates/atelier-core/Cargo.toml` so the foundation work picks up the same version the spike validated.
