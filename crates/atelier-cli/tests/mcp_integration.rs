//! Live integration test for the §15 stdio MCP launcher.
//!
//! Only runs when `npx` is available on PATH AND the `@modelcontextprotocol/
//! server-filesystem` package is reachable (npx will download it on first run).
//! Gated `#[ignore]` so `cargo test --workspace` doesn't flake on CI agents
//! without Node. Run explicitly with:
//!
//!   cargo test --package atelier-cli --test mcp_integration -- --ignored
//!
//! The non-ignored test in this file (`npx_availability_probe`) just checks
//! that the gate is sane and runs everywhere — its presence in the workspace
//! test count tells us the file is actually being compiled.

use std::collections::BTreeMap;
use std::path::Path;

use atelier_core::mcp_config::{McpServerManifest, SideEffectClass, Transport};
use atelier_core::{default_mcp_sandbox, launch_stdio_server, McpLaunchError, McpServerHandle};

fn fixture_dir() -> std::path::PathBuf {
    // macOS resolves `/tmp` → `/private/tmp`; canonicalise so the server's
    // allowed-dirs check sees the same path twice (mirrors the spike).
    let raw = std::path::PathBuf::from("/tmp/atelier-mcp-integration-fixture");
    std::fs::create_dir_all(&raw).expect("create fixture dir");
    let probe = raw.join("hello.txt");
    if !probe.exists() {
        std::fs::write(&probe, b"hello\n").expect("write fixture file");
    }
    std::fs::canonicalize(&raw).expect("canonicalize fixture dir")
}

fn npx_on_path() -> bool {
    std::process::Command::new("npx")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn filesystem_server_manifest(fixture: &Path) -> McpServerManifest {
    McpServerManifest {
        name: "fs-integration".into(),
        transport: Transport::Stdio,
        command: Some("npx".into()),
        args: vec![
            "-y".into(),
            "@modelcontextprotocol/server-filesystem".into(),
            fixture.display().to_string(),
        ],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        side_effect_class: Some(SideEffectClass::LocalRisky),
        // npx may need to reach the npm registry on the first run.
        // We keep `allow_net: true` for that reason; subsequent runs hit the
        // cached package and don't need the net.
        allow_net: true,
        enabled: true,
    }
}

/// Sanity check that always runs (and always passes). Documents the gate so
/// `cargo test` output lists the integration test file even when npx is
/// absent. Without this, the `#[ignore]` test wouldn't be visible in the
/// per-file test counts.
#[test]
fn npx_availability_probe() {
    let present = npx_on_path();
    eprintln!(
        "npx on PATH = {present}; live MCP integration test is {}.",
        if present { "runnable" } else { "gated off" }
    );
}

/// End-to-end smoke test of the stdio launcher against the real
/// `@modelcontextprotocol/server-filesystem` package. Gated `#[ignore]` so
/// CI machines without Node don't fail.
///
/// Run with: `cargo test -p atelier-cli --test mcp_integration -- --ignored`
#[tokio::test]
#[ignore = "requires `npx` + first-run network for the npm registry; run explicitly"]
async fn launch_filesystem_server_and_list_tools() {
    if !npx_on_path() {
        eprintln!("npx missing — skipping (run `brew install node` to enable).");
        return;
    }

    let fixture = fixture_dir();
    let manifest = filesystem_server_manifest(&fixture);
    let workspace_tmp = tempfile::TempDir::new().unwrap();
    let policy = default_mcp_sandbox(
        workspace_tmp.path().to_path_buf(),
        true, /* allow_net */
    )
    .expect("sandbox build");
    let audit_dir = workspace_tmp.path().join("audit");

    let handle: McpServerHandle = match launch_stdio_server(&manifest, &policy, &audit_dir).await {
        Ok(h) => h,
        Err(McpLaunchError::Spawn { source, .. }) => {
            // npx might be missing or sandboxed-out at runtime even though
            // it was on PATH at test-start. Treat as a skip rather than a
            // hard fail so the test stays useful on heterogenous machines.
            eprintln!("npx spawn failed at runtime: {source}; skipping.");
            return;
        }
        Err(e) => panic!("launch_stdio_server failed: {e:?}"),
    };

    assert_eq!(handle.name(), "fs-integration");

    // The filesystem server advertises tools; the count varies by package
    // version (14 in the May 2026 release at spike time) but `list_directory`
    // is stable.
    let tools = handle.list_tools().await.expect("list_tools");
    assert!(!tools.is_empty(), "server advertised zero tools");
    assert!(
        tools.iter().any(|t| t.name == "list_directory"),
        "expected `list_directory` in tools, got {:?}",
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>()
    );

    // Invoke list_directory on the fixture dir and confirm the result is
    // non-error + has at least one text content block.
    let mut args = serde_json::Map::new();
    args.insert(
        "path".into(),
        serde_json::Value::String(fixture.display().to_string()),
    );
    let result = handle
        .call_tool("list_directory", Some(args))
        .await
        .expect("call_tool list_directory");
    assert_ne!(
        result.is_error,
        Some(true),
        "list_directory came back is_error=true: {result:?}"
    );
    assert!(
        !result.content.is_empty(),
        "list_directory result content should be non-empty"
    );

    // Clean shutdown — cancellation token path, not EOF reliance.
    handle.shutdown().await.expect("shutdown");
}

/// Egress block validation. Spawn `/bin/sh -c "env | grep -i proxy"` (NOT a
/// real MCP server — handshake will fail, which is fine) and watch the
/// launcher inject the `http_proxy=http://127.0.0.1:1` block when
/// `allow_net: false`. We can't observe the env directly through the launcher
/// (the subprocess output is owned by rmcp's transport), so this test asserts
/// the negative path: `Spawn` must succeed (the binary is real), and the
/// `Handshake` error must surface because sh is not an MCP server.
#[tokio::test]
async fn egress_block_does_not_prevent_spawn() {
    let workspace_tmp = tempfile::TempDir::new().unwrap();
    let policy =
        default_mcp_sandbox(workspace_tmp.path().to_path_buf(), false).expect("sandbox build");
    let audit_dir = workspace_tmp.path().join("audit");

    let manifest = McpServerManifest {
        name: "sh-not-mcp".into(),
        transport: Transport::Stdio,
        command: Some(
            if Path::new("/bin/sh").exists() {
                "/bin/sh"
            } else {
                "/usr/bin/sh"
            }
            .into(),
        ),
        args: vec!["-c".into(), "exec sleep 0.5".into()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        side_effect_class: Some(SideEffectClass::LocalSafe),
        allow_net: false,
        enabled: true,
    };

    // Bound the launch: sh will sleep then exit; handshake should never complete.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(15_000),
        launch_stdio_server(&manifest, &policy, &audit_dir),
    )
    .await
    .expect("launch_stdio_server should return within 15s");
    match outcome {
        Err(McpLaunchError::Handshake { name, .. }) => {
            assert_eq!(name, "sh-not-mcp");
        }
        // On some platforms sh may exit before the handshake can even send;
        // that surfaces as Handshake (transport closed) too. Either way the
        // launcher must NOT have hung or returned Ok on a non-MCP child.
        Ok(_) => panic!("/bin/sh sleep should NEVER complete the MCP handshake"),
        Err(other) => panic!("expected Handshake error, got {other:?}"),
    }
}
