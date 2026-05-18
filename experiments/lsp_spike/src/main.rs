//! async-lsp maturity spike. Mirror of `experiments/rmcp_spike/src/main.rs`.
//!
//! Run with one of:
//!   cargo run -- stdio
//!   cargo run -- crash
//!   cargo run -- decline
//!
//! See ../README.md for the decision matrix this fills in.
//!
//! Status: harness landed v60.25 (Phase B Track C1). Verdict PENDING — the
//! operator must execute this spike against `typescript-language-server` and
//! record the outcome in the README.
//!
//! Implementation notes (sketch — to refine as the spike runs):
//!   - Targets async-lsp 0.2.x (the latest 0.x line) + lsp-types 0.95.
//!   - Stdio path: spawn `npx -y typescript-language-server --stdio` via
//!     `tokio::process::Command`, wrap stdin/stdout in async-lsp's
//!     `ServerSocket::new_client`, drive `initialize` → `initialized` →
//!     `textDocument/didOpen`, await the matching
//!     `textDocument/publishDiagnostics` notification.
//!   - Crash path: send `SIGKILL` to the spawned child mid-handshake and
//!     observe how async-lsp surfaces the disconnect.
//!   - Decline path: spawn the server and immediately call `shutdown` without
//!     sending `initialized`, simulating the user dismissing the first-use
//!     prompt. Verify no zombie processes remain.
//!
//! The fixture file `nonexistent.ts` deliberately references a method that
//! doesn't exist on `Foo`, so `typescript-language-server` emits a
//! `Property 'nonExistentMethod' does not exist on type 'Foo'` diagnostic —
//! the same shape the §7 Tier-1 verify path will consume in Track C2.

use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::process::Command;

const FIXTURE_DIR: &str = "/tmp/atelier-lsp-spike-fixture";
const FIXTURE_TS: &str = r#"export class Foo {
    bar(): number { return 42; }
}

const f = new Foo();
// Deliberate hallucinated method — the LSP must surface this as
// "Property 'nonExistentMethod' does not exist on type 'Foo'".
f.nonExistentMethod();
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match mode {
        "stdio" => run_stdio().await,
        "crash" => run_crash().await,
        "decline" => run_decline().await,
        _ => {
            eprintln!(
                "lsp-spike: pick a mode\n  cargo run -- stdio\n  cargo run -- crash\n  cargo run -- decline"
            );
            std::process::exit(2);
        }
    }
}

/// Prepare the fixture directory + TypeScript file the spike opens.
fn ensure_fixture() -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from(FIXTURE_DIR);
    std::fs::create_dir_all(&dir).context("create fixture dir")?;
    let ts = dir.join("nonexistent.ts");
    std::fs::write(&ts, FIXTURE_TS).context("write fixture .ts")?;
    Ok(ts)
}

async fn run_stdio() -> anyhow::Result<()> {
    let _ts = ensure_fixture()?;
    eprintln!(
        "stdio: spawning `npx -y typescript-language-server --stdio` against {FIXTURE_DIR}…"
    );
    // Spawn the server. The spike intentionally leaves the async-lsp wiring
    // sketched-but-not-driven below so the harness compiles standalone (no
    // npm install required at build time). The first operator to execute the
    // spike fills in the actual driver loop based on async-lsp 0.2's docs.
    let mut cmd = Command::new("npx");
    cmd.args(["-y", "typescript-language-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _child = cmd.spawn().context("spawn typescript-language-server (need npx + node 22+)")?;
    eprintln!("stdio: child spawned. TODO operator: drive initialize / didOpen / wait-for-diagnostic.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    eprintln!("stdio: tearing down (spike harness is intentionally stub-shaped).");
    Ok(())
}

async fn run_crash() -> anyhow::Result<()> {
    let _ts = ensure_fixture()?;
    eprintln!("crash: spawning, then SIGKILL mid-handshake…");
    let mut cmd = Command::new("npx");
    cmd.args(["-y", "typescript-language-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn typescript-language-server")?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    child.kill().await.context("SIGKILL child")?;
    eprintln!("crash: child killed. TODO operator: assert async-lsp surfaces a typed error.");
    Ok(())
}

async fn run_decline() -> anyhow::Result<()> {
    let _ts = ensure_fixture()?;
    eprintln!("decline: spawning, then exiting without `initialized`…");
    let mut cmd = Command::new("npx");
    cmd.args(["-y", "typescript-language-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn typescript-language-server")?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    child.kill().await.context("teardown child")?;
    eprintln!("decline: harness exited without sending `initialized`. TODO operator: assert no zombies.");
    Ok(())
}
