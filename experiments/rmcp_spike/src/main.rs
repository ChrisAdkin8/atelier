//! rmcp maturity spike.
//!
//! Run with one of:
//!   cargo run -- stdio
//!   cargo run -- http <url>
//!   cargo run -- crash
//!
//! See ../README.md for the decision matrix this fills in.
//!
//! Implementation notes (v60.10, May 2026):
//!   - Pinned to rmcp 0.1.5 (latest 0.1.x; the higher-numbered `rmcp` releases on
//!     crates.io belong to a separate fork at the time of writing).
//!   - Stdio path uses `rmcp::transport::TokioChildProcess::new(&mut Command)` →
//!     `().serve(transport).await` (the unit type implements `ClientHandler` as
//!     a no-op handler, perfect for a discovery client that only consumes
//!     server-pushed events).
//!   - The returned `RunningService<RoleClient, ()>` derefs into a `Peer<RoleClient>`
//!     which exposes `peer.list_tools(None)`, `peer.list_all_tools()`,
//!     `peer.call_tool(CallToolRequestParam { name, arguments })`, etc.
//!   - HTTP/SSE is intentionally a stub — `transport-sse` pulls in axum + reqwest +
//!     a Server-Sent-Events stack we don't want to wire just to fill the matrix.
//!     Decision: defer to v60.11 (per todo.md "HTTP/SSE client" row).

use std::env;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use rmcp::model::{CallToolRequestParam, PaginatedRequestParamInner};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::process::Command;

/// Directory the spike server is rooted at. Created on demand by every run so
/// the spike is idempotent across machines.
const FIXTURE_DIR: &str = "/tmp/atelier-mcp-spike-fixture";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match mode {
        "stdio" => run_stdio().await,
        "http" => {
            let url = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("http requires a URL"))?;
            run_http(url).await
        }
        "crash" => run_crash().await,
        _ => {
            println!("usage: rmcp-spike [stdio | http <url> | crash]");
            println!();
            println!("See README.md for the decision matrix.");
            Ok(())
        }
    }
}

/// Build the fixture directory if missing and drop a small file in it so a
/// `list_directory` call has something to enumerate. Returns the canonicalised
/// path — on macOS `/tmp` symlinks to `/private/tmp`, and the filesystem
/// server's "allowed directories" check is symlink-strict, so we need the
/// resolved form to pass the same path twice.
fn ensure_fixture() -> anyhow::Result<std::path::PathBuf> {
    let dir = Path::new(FIXTURE_DIR);
    std::fs::create_dir_all(dir).with_context(|| format!("creating fixture dir {dir:?}"))?;
    let probe = dir.join("hello.txt");
    if !probe.exists() {
        std::fs::write(&probe, b"hello from rmcp spike\n")
            .with_context(|| format!("writing fixture file {probe:?}"))?;
    }
    let canonical = std::fs::canonicalize(dir)
        .with_context(|| format!("canonicalising fixture dir {dir:?}"))?;
    Ok(canonical)
}

/// Spawn `@modelcontextprotocol/server-filesystem` rooted at `fixture_dir`
/// (already canonicalised by [`ensure_fixture`]) and return a connected client.
async fn spawn_filesystem_server(
    fixture_dir: &Path,
) -> anyhow::Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>> {
    let mut cmd = Command::new("npx");
    cmd.arg("-y")
        .arg("@modelcontextprotocol/server-filesystem")
        .arg(fixture_dir);
    let transport = TokioChildProcess::new(&mut cmd)
        .context("spawn @modelcontextprotocol/server-filesystem")?;
    // `()` is the no-op `ClientHandler` — we don't intend to receive server-pushed
    // sampling/roots requests in the spike, just talk tools/call.
    let client = ().serve(transport).await.context("initialize handshake")?;
    Ok(client)
}

/// Spike step 2: register the filesystem server over stdio, list tools, invoke one.
///
/// EXPECTED outcome: prints JSON-encoded tool list and the result of a list_directory call.
async fn run_stdio() -> anyhow::Result<()> {
    println!("=== STDIO ===");
    let fixture = ensure_fixture()?;
    println!(
        "Spawn target: npx -y @modelcontextprotocol/server-filesystem {}",
        fixture.display()
    );

    let t0 = Instant::now();
    let client = spawn_filesystem_server(&fixture).await?;
    println!(
        "handshake completed in {:?}; protocol_version={:?}",
        t0.elapsed(),
        client.peer().peer_info().protocol_version
    );

    // List tools — the filesystem server advertises ~10 tools (list_directory,
    // read_file, write_file, etc.). We use `list_tools(None)` to get the first page;
    // `list_all_tools()` is the paginating wrapper.
    let tools = client
        .list_tools(Some(PaginatedRequestParamInner { cursor: None }))
        .await?;
    println!("server advertised {} tool(s):", tools.tools.len());
    for t in &tools.tools {
        println!("  - {} — {}", t.name, t.description);
    }

    // Sanity: assert list_directory is present.
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    if !names.iter().any(|n| *n == "list_directory") {
        anyhow::bail!("expected `list_directory` in tools list, got: {:?}", names);
    }

    // Invoke list_directory on the fixture dir.
    let mut args = serde_json::Map::new();
    args.insert(
        "path".into(),
        serde_json::Value::String(fixture.to_string_lossy().into_owned()),
    );
    let result = client
        .call_tool(CallToolRequestParam {
            name: "list_directory".into(),
            arguments: Some(args),
        })
        .await?;
    println!("list_directory result (is_error={:?}):", result.is_error);
    for c in &result.content {
        if let Some(text) = c.raw.as_text() {
            println!("  text: {}", text.text);
        }
    }
    if result.is_error == Some(true) {
        anyhow::bail!("list_directory came back with is_error=true");
    }

    // Clean shutdown — cancels the serve loop and joins the task.
    let quit = client.cancel().await?;
    println!("shutdown via client.cancel() — QuitReason::{quit:?}");
    Ok(())
}

/// Spike step 3: HTTP / SSE transport — deferred per the v60.10 decision matrix.
async fn run_http(url: &str) -> anyhow::Result<()> {
    println!("=== HTTP ===");
    println!("Target: {url}");
    println!();
    println!("DEFERRED to v60.11+ (per `tasks/todo.md` §15 row 'HTTP/SSE client').");
    println!();
    println!("rmcp 0.1.5 ships an SSE transport behind the `transport-sse` feature");
    println!("(reqwest + sse-stream + axum). The Phase A bundle stops at stdio per");
    println!("the bundle spec; wiring HTTP/SSE pulls in network egress concerns we");
    println!("haven't audited (§12 egress accounting needs to thread through here).");
    Ok(())
}

/// Spike step 4: kill the stdio server mid-dispatch; observe rmcp's behavior.
///
/// rmcp's `TokioChildProcess::new` takes ownership of `tokio::process::Child`
/// internally, so we have no public PID accessor once the transport is
/// constructed. On macOS the `npx` wrapper spawns a separate child node
/// process for the actual MCP server, so `Child::id()` (if we had it) would
/// give us the wrapper PID, not the server's. We work around this with a
/// `pgrep -f` lookup against the well-known argv signature `server-filesystem`
/// post-handshake — by which time the server process is definitely live.
async fn run_crash() -> anyhow::Result<()> {
    println!("=== CRASH ===");
    let fixture = ensure_fixture()?;
    let fixture_str = fixture.to_string_lossy().into_owned();

    let mut cmd = Command::new("npx");
    cmd.arg("-y")
        .arg("@modelcontextprotocol/server-filesystem")
        .arg(&fixture_str);
    let transport = TokioChildProcess::new(&mut cmd)?;
    let client = ().serve(transport).await?;
    println!("handshake completed; resolving server PID via pgrep -f");

    // Sleep briefly to let npx finish spawning the actual node server-filesystem
    // child; then resolve its PID by argv signature.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let server_pid = pgrep_server_pid(&fixture_str)
        .ok_or_else(|| anyhow::anyhow!("could not resolve server-filesystem PID via pgrep"))?;
    println!("server PID: {server_pid}");

    // Kill the server BEFORE issuing the dispatch and wait long enough for
    // the OS to actually reap the descriptors. macOS SIGKILL is asynchronous;
    // a 50ms grace was too short in observation (the server still drained
    // its single buffered request and wrote a response before dying).
    let rc = unsafe { libc::kill(server_pid, libc::SIGKILL) };
    println!("SIGKILL → pid {server_pid}, kill(2) rc={rc}");
    // 500ms is generous on macOS — the npx-wrapper'd node usually dies in
    // < 50ms but the rare case where it has work in-flight justifies waiting.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut args = serde_json::Map::new();
    args.insert(
        "path".into(),
        serde_json::Value::String(fixture_str.clone()),
    );
    let t0 = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        client.call_tool(CallToolRequestParam {
            name: "list_directory".into(),
            arguments: Some(args),
        }),
    )
    .await;
    let elapsed = t0.elapsed();

    match outcome {
        Ok(Ok(r)) => {
            // Server returned a response despite the SIGKILL — unexpected.
            // Could happen if the kill signal hadn't actually delivered yet
            // by the time we issued the call.
            println!(
                "call_tool returned OK in {elapsed:?} (kill did not race in). \
                 result.is_error={:?}, content.len={}",
                r.is_error,
                r.content.len()
            );
        }
        Ok(Err(e)) => {
            println!(
                "call_tool returned Err in {elapsed:?} — GOOD: rmcp surfaced a \
                 typed ServiceError rather than hanging. err = {e:#}"
            );
        }
        Err(_) => {
            println!(
                "call_tool HUNG past 5s after SIGKILL — BAD: rmcp's dispatch \
                 future does not notice transport death. Atelier wrappers MUST \
                 add their own per-call timeout."
            );
        }
    }

    // Smell-test the serve loop's reaction to a dead child:
    //   `client.cancel()` fires the CancellationToken (the `_ = serve_loop_ct.cancelled()`
    //   branch in `serve_inner`'s select arm) AND joins the handle. So `cancel()`
    //   is robust whether or not stdout-EOF propagated cleanly.
    //
    //   Observation (rmcp 0.1.5): the natural-EOF path (`Some(m) = None` arm)
    //   does not always wake the framed codec promptly when the child is
    //   SIGKILL'd. The atelier stdio launcher therefore MUST shut down via
    //   `cancel()` (cancellation token), not rely on EOF — recorded as smell
    //   #2 in the README matrix.
    let shutdown_outcome = tokio::time::timeout(Duration::from_secs(5), client.cancel()).await;
    match shutdown_outcome {
        Ok(Ok(reason)) => println!("serve loop quit via cancel(): QuitReason::{reason:?}"),
        Ok(Err(join_err)) => println!("serve loop join error after cancel(): {join_err}"),
        Err(_) => {
            println!("serve loop did not quit within 5s of cancel() — HARD HANG");
        }
    }

    Ok(())
}

/// Resolve the PID of the live `server-filesystem` MCP server by argv match.
/// Returns the PID with the longest TTY-less lifetime (the actual server, not
/// the spawning shell). Returns `None` if no match.
fn pgrep_server_pid(fixture_marker: &str) -> Option<i32> {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(format!("server-filesystem.*{fixture_marker}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Take the LAST matching PID — pgrep prints in PID order, and the spawn
    // chain (sh → npx → node-launcher → node-server) places the actual server
    // process latest.
    stdout
        .lines()
        .filter_map(|s| s.trim().parse::<i32>().ok())
        .last()
}
