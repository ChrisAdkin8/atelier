//! rmcp maturity spike.
//!
//! Run with one of:
//!   cargo run -- stdio
//!   cargo run -- http <url>
//!   cargo run -- crash
//!
//! See ../README.md for the decision matrix this fills in.
//!
//! NOTE: This is a skeleton, not a verified build. The exact rmcp API surface
//! (constructor names, transport types) will need to be adapted to the actual
//! rmcp version pinned in Cargo.toml. An implementor running this will see
//! compilation errors first; treat those as part of the spike — they tell you
//! about the API stability dimension of the matrix.

use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match mode {
        "stdio" => run_stdio().await,
        "http" => {
            let url = args.get(2).ok_or_else(|| anyhow::anyhow!("http requires a URL"))?;
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

/// Spike step 2: register the filesystem server over stdio, list tools, invoke one.
///
/// EXPECTED outcome: prints JSON-encoded tool list and the result of a list_directory call.
/// FAILURE modes to record: client constructor missing/renamed, transport handshake hangs,
/// JSON-RPC decoding fails, tool invocation returns malformed result.
async fn run_stdio() -> anyhow::Result<()> {
    println!("=== STDIO ===");
    println!("Spawn target: npx -y @modelcontextprotocol/server-filesystem /tmp/spike-sandbox");
    println!();
    println!("TODO(implementor): wire rmcp's stdio transport here.");
    println!("Reference: https://github.com/modelcontextprotocol/rust-sdk");
    println!();
    println!("Steps to implement:");
    println!("  1. mkdir /tmp/spike-sandbox and drop a small file in it");
    println!("  2. Create an rmcp client over stdio against `npx -y @modelcontextprotocol/server-filesystem /tmp/spike-sandbox`");
    println!("  3. Call the 'initialize' / 'tools/list' method (whichever rmcp surfaces)");
    println!("  4. Pretty-print the tool list");
    println!("  5. Call 'tools/call' for 'list_directory' with `{{\"path\": \"/tmp/spike-sandbox\"}}`");
    println!("  6. Pretty-print the result");
    println!("  7. Shut the server down cleanly; assert no zombie process via `ps`");

    Ok(())
}

/// Spike step 3: HTTP / SSE transport.
async fn run_http(url: &str) -> anyhow::Result<()> {
    println!("=== HTTP ===");
    println!("Target: {}", url);
    println!();
    println!("TODO(implementor): wire rmcp's HTTP/SSE transport here.");
    println!();
    println!("Steps to implement:");
    println!("  1. Construct rmcp HTTP client against the URL");
    println!("  2. List tools, invoke one, pretty-print");
    println!("  3. Tear down the connection cleanly");
    println!();
    println!("If rmcp does not support HTTP/SSE in the pinned version, mark this row");
    println!("'skipped' in the decision matrix. HTTP MCP is less common than stdio");
    println!("and not strictly v1-critical.");

    Ok(())
}

/// Spike step 4: kill the stdio server mid-dispatch; observe rmcp's behavior.
async fn run_crash() -> anyhow::Result<()> {
    println!("=== CRASH ===");
    println!();
    println!("TODO(implementor):");
    println!("  1. Spawn the stdio server (as in step 2)");
    println!("  2. Issue a tool call");
    println!("  3. After dispatch but before completion, send SIGKILL to the child");
    println!("  4. Observe rmcp's behavior:");
    println!("       a. Does the Future resolve to a clean Err?  (good)");
    println!("       b. Does it hang indefinitely?  (bad)");
    println!("       c. Does it panic?  (very bad)");
    println!("       d. Is the zombie reaped?  (`ps` after — child must not linger)");
    println!();
    println!("Record the outcome in the README decision matrix.");

    Ok(())
}
