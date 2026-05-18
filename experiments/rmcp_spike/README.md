# `rmcp` maturity spike

**Status: executed v60.10 (2026-05-18). Verdict: GO WITH CAVEATS.** This spike
was the single highest-leverage Phase A prerequisite — it established whether
`rmcp` (the Rust MCP SDK) is viable as `atelier-core`'s MCP client, or whether
§15 needed re-spec'ing around a direct wire-protocol implementation. Verdict
recorded in the **Outcome** section at the bottom; smells worth flagging are
listed inline in the matrix.

The spike code in `src/main.rs` runs three modes against the published
`@modelcontextprotocol/server-filesystem` npm package:

- `stdio` — happy path: spawn, handshake, list tools, call `list_directory`, shutdown.
- `crash` — kill the spawned server with `SIGKILL` and check that rmcp surfaces
  a typed `ServiceError::Transport` rather than hanging.
- `http` — left as a stub. HTTP/SSE wiring is deferred to v60.11+; pulling in
  the `transport-sse` feature drags axum + reqwest + an SSE stack into
  `atelier-core` before the stdio path is fully wired through the dispatcher.

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

## Decision matrix (filled in v60.10, rmcp 0.1.5)

| Capability | Outcome | Notes |
|---|---|---|
| stdio transport — register a server | **pass** | `TokioChildProcess::new(&mut Command)` + `().serve(transport).await`. Handshake completes in ~700ms (mostly npx cold-start). |
| stdio — list tools | **pass** | `peer.list_tools(None)` returns 14 tools from `server-filesystem`. Pagination wrapper `list_all_tools()` also exists. |
| stdio — invoke a tool, parse JSON result | **pass** | `peer.call_tool(CallToolRequestParam { name, arguments })` → `CallToolResult { content: Vec<Content>, is_error }`. `content[i].raw.as_text()` extracts text payloads. |
| HTTP transport — connect + list + invoke | **deferred (v60.11+)** | rmcp has `transport-sse`; not enabled because pulling axum + reqwest + sse-stream into `atelier-core` is its own bundle. Per `tasks/todo.md` §15. |
| Crash recovery — server killed mid-dispatch | **clean error** | SIGKILL → `call_tool` returns `Err(ServiceError::Transport(io::Error("disconnected")))` in ~20µs. Serve loop exits with `QuitReason::Closed`. No zombies after run. |
| Streaming responses | **not needed v1** | MCP's `tools/call` is request-response; streaming applies to logging/progress notifications which atelier won't surface in v1. |
| API stability — public surface stable? | **shifting** | rmcp's crates.io history shows 0.1.x → 1.x.x line discontinuity (the newer 1.7.0 on crates.io is a different fork, not a continuation). 0.1.5 is the latest 0.1.x. Smells: `Tool.input_schema` is `Arc<JsonObject>` (`serde_json::Map` not `Value`); `Tool.name`/`description` are `Cow<'static, str>`; `Implementation::from_build_env()` injects the *caller's* crate name as `client_info.name` (we'll want to override that in `atelier-core` so MCP servers see `atelier` not `atelier-core`). |
| Binary size impact on `atelier-core` | **5–20 MB** | rmcp pulls schemars (~700KB) + paste + base64 + tokio-util/codec. Acceptable. Exact rmcp v0.1.5 dep graph: 7 new crates (base64, dyn-clone, paste, rmcp-macros, schemars, schemars_derive, serde_derive_internals). |
| Total session time | ~25 min (Phase 1 spike runs) | + ~90 min downstream wiring (Phases 2–4). |

### Smells worth flagging for §15 follow-up bundles

1. **Broken feature gating.** `rmcp/src/model/capabilities.rs` unconditionally uses `paste::paste!` but the `paste` dep is gated behind the `macros` feature. So `default-features = false` + `client` + `transport-child-process` doesn't compile — you have to keep `macros` in the feature set even though you don't use the `#[tool]` proc macro. Documented at the top of `Cargo.toml`.
2. **No public PID accessor on `TokioChildProcess`.** Once you give the `Child` to rmcp, you lose direct access to it. For atelier's launcher this means: shutdown via the `CancellationToken` (`client.cancel().await` — graceful) is the only first-class path. If we ever need force-kill, we either fish the PID via `pgrep -f` (what the crash test does) or fork rmcp. The launcher in this bundle uses `client.cancel()` exclusively.
3. **Natural-EOF stdout path doesn't always wake the framed codec.** When the child dies but `client.cancel()` isn't fired, the serve loop sometimes doesn't notice the EOF promptly. Mitigation: atelier's `McpServerHandle::shutdown` ALWAYS uses `client.cancel()` (CancellationToken) rather than relying on EOF propagation.
4. **`Tool.input_schema` is `Arc<JsonObject>`, not `serde_json::Value`.** When we wire MCP tools into the dispatcher (future bundle), the schema-validate-arguments path needs to wrap that map in a `Value::Object` for `jsonschema::Validator`.
5. **`Implementation::from_build_env()` reports the caller's crate name.** Servers will see `atelier-core` as the client name unless we override `ClientInfo::default()` with a custom `Implementation { name: "atelier", version: env!("CARGO_PKG_VERSION") }`. The launcher in this bundle passes `()` as the `ClientHandler`, which inherits the default — fine for v1 but flagged for v60.11+.

## Decision

Pick one:

- **GO** — at minimum: stdio works end-to-end, crash is a clean error (not a panic / hang), HTTP either works or is not v1-critical. Proceed with the §15 plan as written.
- **GO WITH CAVEATS** — stdio works but one of HTTP / streaming / crash recovery has issues. Document the caveat in spec §15 and adjust Phase A scope.
- **NO-GO** — stdio is broken, panics, or `rmcp` is not maintained. Re-spec §15 around a direct wire-protocol implementation (the MCP JSON-RPC spec is small; ~500–800 lines of Rust to reimplement the client side).

## Outcome

- Decision: **GO WITH CAVEATS**
- Date: 2026-05-18
- Operator: Phase A §15 rmcp foundation bundle (v60.10)
- rmcp version actually tested: **0.1.5** (latest in the 0.1.x line; pinned via `rmcp = "0.1"` workspace dep)
- Caveats / re-spec notes:
  - Stick to rmcp 0.1.x for now; treat the 1.x.x line on crates.io as a separate fork (verify upstream maintenance before considering a bump).
  - `atelier-core`'s launcher uses **stdio only** in this bundle. HTTP/SSE wiring is deferred to v60.11+ to avoid pulling axum + reqwest stack in before the dispatcher integration is designed.
  - The launcher shuts down via `RunningService::cancel()` (cancellation token) exclusively — do not rely on EOF propagation through the framed codec.
  - When wiring tools into the dispatcher (v60.11+ "built-ins-as-MCP refactor"), normalise `Tool.input_schema: Arc<serde_json::Map>` → `serde_json::Value::Object(...)` before handing to `jsonschema::Validator`.
  - Consider injecting a custom `ClientInfo` so MCP servers see `atelier` as the client name (otherwise they see `atelier-core`, the immediate crate-of-record).

## Why a documented procedure rather than `cargo run` in CI

The spike requires:
- A real MCP server subprocess (`@modelcontextprotocol/server-filesystem`), which requires `npx` + network for the first run to pull the package.
- An interactive judgment call on "is the error message reasonable?", which doesn't fit a binary CI gate.
- Decision authority over Phase A scope (GO / NO-GO / RE-SPEC), which belongs to a human.

CI can run the spike as a smoke test once `rmcp` is committed to (post-decision), but the decision itself is a human task.
