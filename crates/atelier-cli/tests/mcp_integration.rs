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
use atelier_core::{
    default_mcp_sandbox, launch_http_server, launch_stdio_server, McpEgressEvent, McpLaunchError,
    McpServerHandle,
};

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

// ---------- v60.11 C1: §15 HTTP/SSE launcher ----------

/// Build an `http`/`sse` manifest fixture for the integration-test layer.
/// Mirrors `filesystem_server_manifest` but for the remote transport.
fn http_manifest(
    name: &str,
    transport: Transport,
    url: &str,
    headers: BTreeMap<String, String>,
    allow_net: bool,
) -> McpServerManifest {
    McpServerManifest {
        name: name.into(),
        transport,
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        url: Some(url.into()),
        headers,
        side_effect_class: Some(SideEffectClass::SharedState),
        allow_net,
        enabled: true,
    }
}

/// Always-on smoke test: an `http` manifest with `allow_net: false` must
/// be refused before any network I/O happens. Mirrors the stdio
/// launcher's egress posture but at the manifest level — HTTP/SSE
/// servers have no in-process equivalent of the proxy-to-port-1 block.
#[tokio::test]
async fn http_launcher_rejects_allow_net_false_manifest() {
    let workspace_tmp = tempfile::TempDir::new().unwrap();
    let policy =
        default_mcp_sandbox(workspace_tmp.path().to_path_buf(), false).expect("sandbox build");
    let audit_dir = workspace_tmp.path().join("audit");
    let manifest = http_manifest(
        "search-http",
        Transport::Http,
        "https://example.invalid/mcp",
        BTreeMap::new(),
        false, // <-- the gate
    );
    let err = launch_http_server(&manifest, &policy, &audit_dir)
        .await
        .expect_err("allow_net=false must refuse to launch http transport");
    match err {
        McpLaunchError::Refused { name, message } => {
            assert_eq!(name, "search-http");
            assert!(
                message.contains("allow_net=true"),
                "message should explain the policy, got {message:?}"
            );
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    // §12 egress audit row landed in <audit_dir>/audit.log with the
    // shape pinned by `schemas/audit/mcp_egress.v1.json`.
    let log_path = audit_dir.join("audit.log");
    assert!(log_path.exists(), "audit log was not created");
    let body = std::fs::read_to_string(&log_path).expect("read audit log");
    let row: McpEgressEvent = serde_json::from_str(body.lines().next().expect("at least one row"))
        .expect("audit row parses");
    assert_eq!(row.kind, "mcp-http-request");
    assert_eq!(row.provider, "search-http");
    assert_eq!(row.outcome, "blocked");
    assert_eq!(row.reason.as_deref(), Some("allow_net=false"));
    // Defence in depth: serialized row must not carry a `headers` field.
    let raw: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
    assert!(
        !raw.as_object().unwrap().contains_key("headers"),
        "audit row must NOT serialize headers; got {raw}"
    );
}

/// Same gate but on `Transport::Sse` — the launcher accepts both
/// variants and refuses both when `allow_net == false`.
#[tokio::test]
async fn http_launcher_rejects_sse_allow_net_false_manifest() {
    let workspace_tmp = tempfile::TempDir::new().unwrap();
    let policy =
        default_mcp_sandbox(workspace_tmp.path().to_path_buf(), false).expect("sandbox build");
    let audit_dir = workspace_tmp.path().join("audit");
    let manifest = http_manifest(
        "search-sse",
        Transport::Sse,
        "https://example.invalid/mcp",
        BTreeMap::new(),
        false,
    );
    let err = launch_http_server(&manifest, &policy, &audit_dir)
        .await
        .expect_err("allow_net=false must refuse to launch sse transport");
    assert!(
        matches!(err, McpLaunchError::Refused { .. }),
        "expected Refused, got {err:?}"
    );
}

/// `#[ignore]`-gated live integration test placeholder for the §15
/// HTTP/SSE launcher. There is no publicly-running MCP HTTP/SSE echo
/// server we can rely on, so the live test is a stub the operator runs
/// manually against a server they spin up locally (e.g. via
/// `npx @modelcontextprotocol/server-everything --transport sse`).
///
/// Run with:
///   cargo test -p atelier-cli --test mcp_integration \
///     -- --ignored live_http_launcher
///
/// TODO(v60.11+): once a public reference SSE server exists, point the
/// test at it and drop the env-var gate. The MCP 2025-03-26 spec
/// introduces a streamable-HTTP transport that should land alongside
/// the rmcp 0.2.x line.
#[tokio::test]
#[ignore = "no public MCP HTTP/SSE echo server; run manually against a local SSE server"]
async fn live_http_launcher_against_local_sse_server() {
    let url = match std::env::var("ATELIER_MCP_SSE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!(
                "ATELIER_MCP_SSE_URL not set; start a local SSE MCP server and re-run with \
                 ATELIER_MCP_SSE_URL=http://localhost:3001/sse"
            );
            return;
        }
    };
    let workspace_tmp = tempfile::TempDir::new().unwrap();
    let policy =
        default_mcp_sandbox(workspace_tmp.path().to_path_buf(), true).expect("sandbox build");
    let audit_dir = workspace_tmp.path().join("audit");
    let manifest = http_manifest("live-sse", Transport::Sse, &url, BTreeMap::new(), true);
    let handle: McpServerHandle = launch_http_server(&manifest, &policy, &audit_dir)
        .await
        .expect("launch_http_server against local SSE server");
    assert_eq!(handle.name(), "live-sse");
    let tools = handle.list_tools().await.expect("list_tools");
    eprintln!("live SSE server advertised {} tools", tools.len());
    handle.shutdown().await.expect("shutdown");
    // Verify the audit log captured at least the handshake + list-tools rows.
    let log_path = audit_dir.join("audit.log");
    let body = std::fs::read_to_string(&log_path).expect("read audit log");
    let row_count = body.lines().count();
    assert!(row_count >= 2, "expected >= 2 audit rows, got {row_count}");
}
