# `rmcp` maturity spike

**Status: procedure documented; not yet executed.** This spike is the single highest-leverage Phase A prerequisite. It establishes whether `rmcp` (the official Rust MCP SDK) is viable as `atelier-core`'s MCP client, or whether §15 needs re-spec'ing around a direct wire-protocol implementation.

The spike code in `src/main.rs` is a skeleton — it has not been `cargo check`'d in this repo (no `cargo` available during the documentation pass). An implementor with a Rust toolchain runs it on the reference machine (`tests/perf/reference.md`) in ~30–60 minutes and fills in the decision matrix below.

## Procedure

1. **Setup** (5 min):
   ```sh
   cargo new --bin rmcp-spike    # outside this repo, or override into experiments/
   cd rmcp-spike
   # Copy Cargo.toml and src/main.rs from experiments/rmcp_spike/ here.
   # Ensure npx is available: `which npx` (Node 22+ recommended; matches the reference machine).
   cargo build
   ```

2. **Stdio test** (10 min):
   ```sh
   cargo run -- stdio
   ```
   Expected: registers `npx -y @modelcontextprotocol/server-filesystem /tmp/spike-sandbox`,
   lists advertised tools, calls `list_directory` on `/tmp/spike-sandbox`,
   prints the JSON response, exits 0.

3. **HTTP test** (10 min):
   ```sh
   # Start any HTTP MCP server (or use a community-hosted one for which you have a token).
   # If no HTTP server is reachable, mark this row "skipped" in the matrix.
   cargo run -- http https://your.mcp.server/path
   ```
   Expected: connects, lists tools, calls one, exits 0.

4. **Crash recovery test** (5 min):
   ```sh
   cargo run -- crash
   ```
   Expected: spawns the stdio server, kills it mid-dispatch with SIGKILL, observes
   how `rmcp` surfaces the failure. Look for: clean `Result::Err`, no panic,
   no hung child process, reasonable error message.

5. **Streaming test** (10 min):
   If `rmcp` supports streaming responses, exercise one. If not, note in matrix.

6. **API stability survey** (10 min):
   Read the `rmcp` changelog and recent commits. Note: how often do public APIs change?
   Is it post-1.0 yet? Any pinned compatibility issues?

7. **Fill in the matrix below** and commit to this README.

## Decision matrix

Tick each cell after running the corresponding step. The aggregate dictates go/no-go.

| Capability | Outcome | Notes |
|---|---|---|
| stdio transport — register a server | ⬜ pass / ⬜ fail | |
| stdio — list tools | ⬜ pass / ⬜ fail | |
| stdio — invoke a tool, parse JSON result | ⬜ pass / ⬜ fail | |
| HTTP transport — connect + list + invoke | ⬜ pass / ⬜ fail / ⬜ skipped | |
| Crash recovery — server killed mid-dispatch | ⬜ clean error / ⬜ panic / ⬜ hangs | |
| Streaming responses | ⬜ supported / ⬜ blocked / ⬜ not needed v1 | |
| API stability — public surface stable? | ⬜ yes / ⬜ shifting / ⬜ unknown | |
| Binary size impact on `atelier-core` | ⬜ <5 MB / ⬜ 5–20 MB / ⬜ >20 MB | |
| Total session time | _MM:SS_ | |

## Decision

Pick one:

- **GO** — at minimum: stdio works end-to-end, crash is a clean error (not a panic / hang), HTTP either works or is not v1-critical. Proceed with the §15 plan as written.
- **GO WITH CAVEATS** — stdio works but one of HTTP / streaming / crash recovery has issues. Document the caveat in spec §15 and adjust Phase A scope.
- **NO-GO** — stdio is broken, panics, or `rmcp` is not maintained. Re-spec §15 around a direct wire-protocol implementation (the MCP JSON-RPC spec is small; ~500–800 lines of Rust to reimplement the client side).

## Outcome

_Fill in after running:_

- Decision: ⬜ GO / ⬜ GO WITH CAVEATS / ⬜ NO-GO
- Date: _____
- Operator: _____
- Caveats / re-spec notes: _____

## Why a documented procedure rather than `cargo run` in CI

The spike requires:
- A real MCP server subprocess (`@modelcontextprotocol/server-filesystem`), which requires `npx` + network for the first run to pull the package.
- An interactive judgment call on "is the error message reasonable?", which doesn't fit a binary CI gate.
- Decision authority over Phase A scope (GO / NO-GO / RE-SPEC), which belongs to a human.

CI can run the spike as a smoke test once `rmcp` is committed to (post-decision), but the decision itself is a human task.
