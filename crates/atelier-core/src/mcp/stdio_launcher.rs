//! §15 stdio MCP server launcher.
//!
//! Bridges an [`McpServerManifest`] (typed wrapper around `mcp_servers.json`)
//! to an `rmcp` client. Owns the `tokio::process::Child` (via rmcp's
//! `TokioChildProcess`) and the `RunningService<RoleClient, ()>` for its
//! lifetime. Shutdown goes through the cancellation token (`cancel()`),
//! never EOF — see the v60.10 spike README for why.
//!
//! Sandboxing posture (spec §11):
//!
//!   - The launcher applies the same env-scrubbing rules as `subprocess::run`
//!     (allowlist + manifest overrides + secret-substring redaction).
//!   - When `manifest.allow_net == false`, we shadow `http_proxy` /
//!     `https_proxy` / `HTTP_PROXY` / `HTTPS_PROXY` / `all_proxy` / `ALL_PROXY`
//!     to `http://127.0.0.1:1` (TCPmux — RFC 1078, unused on real hosts so
//!     connections refuse immediately). Same egress block as the `shell` tool.
//!   - We deliberately **don't** wrap the MCP server in `sandbox-exec` /
//!     `bwrap` in this bundle. Reason: the existing wrappers assume a
//!     short-lived child driven by stdin/stdout under a foreground pipe; an
//!     MCP server is long-lived and shares its stdio bidirectionally with
//!     `rmcp`. Adding sandbox wrapping requires `--no-net` + a writable-temp
//!     scope tailored to the server's needs, which is a separate v60.11+
//!     bundle. Egress is still blocked via the proxy trick.
//!
//! What this bundle ships:
//!
//!   - `launch_stdio_server(manifest, sandbox, audit_dir)` — spawn + handshake.
//!   - `McpServerHandle` — owns the rmcp running service; exposes
//!     `list_tools()` / `shutdown()` plus a generic `peer()` accessor for
//!     `call_tool` etc.
//!   - `McpTool` — atelier-side projection of `rmcp::model::Tool`. Owned
//!     `String` fields (rmcp uses `Cow<'static, str>` + `Arc<JsonObject>` —
//!     not load-bearing for atelier).
//!
//! What this bundle does NOT do:
//!
//!   - Register MCP tools into the §15 dispatcher. That's the v60.11+
//!     "built-ins-as-MCP refactor" bundle.
//!   - Map `Tool.input_schema` → §15 `ToolSpec`. Same reason.
//!   - Emit `Event::McpServerLaunched` on the session bus. Same reason.
//!   - HTTP/SSE transport. Same reason.
//!
//! Audit posture: the launcher accepts an `audit_dir` parameter so a future
//! bundle can plumb in §12 egress accounting (for `http`/`sse` manifests
//! once those are wired). Today it's used only to validate the path exists;
//! the actual audit-log write happens in the dispatcher integration that
//! lands later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::{CallToolRequestParam, CallToolResult, PaginatedRequestParamInner};
use rmcp::service::{RoleClient, RunningService};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::mcp::errors::McpLaunchError;
use crate::mcp_config::{interpolate as mcp_interpolate, McpServerManifest, Transport};
use crate::sandbox::SandboxPolicy;
use crate::subprocess::ENV_PASSTHROUGH;

/// Time budget for the rmcp `initialize` handshake. Generous — the npx-launched
/// servers we exercise in the spike take ~700ms cold-start because of node
/// startup. A locally-installed MCP binary should be < 100ms.
pub const HANDSHAKE_TIMEOUT_MS: u64 = 10_000;

/// Time budget for the initial `tools/list` probe used as a "is the server
/// actually functional?" check during launch. Shorter than handshake — the
/// server is already up by this point.
pub const FIRST_LIST_TIMEOUT_MS: u64 = 5_000;

/// MCP protocol version the rmcp 0.1.5 client sends. Future bundles may
/// relax this to a range. Spec §15 doesn't pin a protocol version explicitly;
/// we record what we accept so a server bumping to a newer version surfaces
/// a typed error instead of silently mis-talking.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2024-11-05";

/// Atelier-side projection of `rmcp::model::Tool`. Owned `String` fields so
/// callers don't carry an `Arc<JsonObject>` around (and so the dispatcher
/// integration in v60.11+ can map this onto its own `ToolSpec` shape without
/// being coupled to rmcp's internal types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTool {
    /// Tool name as the MCP server advertised it. Becomes the dispatcher key
    /// once `built-ins-as-MCP` lands.
    pub name: String,
    /// Description text the server provided (may be empty).
    pub description: String,
    /// JSON Schema for the tool's input parameters, as a `serde_json::Value::Object`.
    /// rmcp stores this as `Arc<serde_json::Map<String, Value>>`; we wrap it in
    /// a `Value::Object` here so callers can pass it to `jsonschema::Validator`
    /// without an extra clone+wrap. Smell #4 from the spike README.
    pub input_schema: serde_json::Value,
}

impl From<&rmcp::model::Tool> for McpTool {
    fn from(t: &rmcp::model::Tool) -> Self {
        // `input_schema` is `Arc<JsonObject>` where `JsonObject =
        // serde_json::Map<String, Value>`. Wrap into a `Value::Object` so it's
        // usable as a JSON Schema document.
        let schema = serde_json::Value::Object((*t.input_schema).clone());
        Self {
            name: t.name.to_string(),
            description: t.description.to_string(),
            input_schema: schema,
        }
    }
}

/// Live handle on a launched MCP server. Owns the rmcp `RunningService`,
/// which in turn owns the `tokio::process::Child` (via `TokioChildProcess`).
/// Dropping this handle does NOT cleanly shut the server down — call
/// [`McpServerHandle::shutdown`] to fire the cancellation token. (Drop will
/// SIGKILL the child via `kill_on_drop(true)` set by rmcp internally; that's
/// a safety net, not the primary path.)
pub struct McpServerHandle {
    name: String,
    client: RunningService<RoleClient, ()>,
    /// Snapshot of the server's `InitializeResult` captured during handshake.
    /// Borrow-free access via accessors; we keep a clone instead of a reference
    /// to the rmcp internal state because the rmcp 0.1.5 `peer_info()` API
    /// returns `&ServerInfo` which is tied to the running service's lifetime.
    server_info: rmcp::model::ServerInfo,
}

impl std::fmt::Debug for McpServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // RunningService is intentionally opaque. Surface the data fields
        // that are useful for log lines + test asserts; skip the rmcp guts.
        f.debug_struct("McpServerHandle")
            .field("name", &self.name)
            .field("protocol_version", &self.server_info.protocol_version)
            .field("server_info", &self.server_info.server_info)
            .finish()
    }
}

impl McpServerHandle {
    /// Crate-internal constructor used by both `launch_stdio_server`
    /// (this module) and `launch_http_server` (the v60.11 C1 sibling
    /// `http_launcher.rs`). Kept `pub(crate)` so external callers can
    /// only obtain a handle via one of the typed launch functions.
    pub(crate) fn new(
        name: String,
        client: RunningService<RoleClient, ()>,
        server_info: rmcp::model::ServerInfo,
    ) -> Self {
        Self {
            name,
            client,
            server_info,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Server-advertised capability set captured at handshake time. Used by
    /// the trust-budget UI and (in v60.11+) by the resources/prompts wiring.
    pub fn capabilities(&self) -> &rmcp::model::ServerCapabilities {
        &self.server_info.capabilities
    }

    /// Borrow the rmcp peer so callers can issue arbitrary MCP requests
    /// (`call_tool`, `read_resource`, `list_prompts`, …). Exposed as
    /// `Peer<RoleClient>` because the v60.11+ dispatcher integration will
    /// need this surface; for now the launcher only uses `list_tools` and
    /// `call_tool` internally via the typed wrappers.
    pub fn peer(&self) -> &rmcp::service::Peer<RoleClient> {
        self.client.peer()
    }

    /// List the server's tools, with rmcp's built-in pagination wrapper.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, McpLaunchError> {
        let tools =
            self.client
                .peer()
                .list_all_tools()
                .await
                .map_err(|e| McpLaunchError::Refused {
                    name: self.name.clone(),
                    message: format!("{e}"),
                })?;
        Ok(tools.iter().map(McpTool::from).collect())
    }

    /// Invoke a tool by name. Thin wrapper over `peer.call_tool` so the
    /// launcher's typed-error surface is consistent.
    pub async fn call_tool(
        &self,
        name: impl Into<String>,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, McpLaunchError> {
        let name_s = name.into();
        let tool_name: std::borrow::Cow<'static, str> = std::borrow::Cow::Owned(name_s.clone());
        self.client
            .peer()
            .call_tool(CallToolRequestParam {
                name: tool_name,
                arguments,
            })
            .await
            .map_err(|e| McpLaunchError::Refused {
                name: self.name.clone(),
                message: format!("call_tool {name_s:?}: {e}"),
            })
    }

    /// Fire the cancellation token and wait for the rmcp serve loop to join.
    /// Returns once the child process is unreachable from rmcp's side; the
    /// underlying `tokio::process::Child` will be dropped (which in turn
    /// SIGKILLs the child if it hasn't exited via the `kill_on_drop(true)`
    /// rmcp sets internally).
    ///
    /// **Always use this** rather than letting the handle drop. The EOF path
    /// through the framed codec is not reliable in rmcp 0.1.5 (spike smell #3).
    pub async fn shutdown(self) -> Result<(), McpLaunchError> {
        let name = self.name;
        match self.client.cancel().await {
            Ok(_quit) => Ok(()),
            Err(join_err) => Err(McpLaunchError::ChildExited {
                name,
                // join errors don't surface an exit code; record `None` so
                // the caller knows the loop is dead but doesn't have a
                // wait-status to report.
                code: join_err.is_panic().then_some(-1),
            }),
        }
    }
}

/// Launch a stdio MCP server described by `manifest`. Returns a connected
/// `McpServerHandle` once the JSON-RPC `initialize` handshake completes AND
/// the first `tools/list` request succeeds.
///
/// `sandbox` is currently used only for its `allow_net` flag (the egress
/// proxy block); a future bundle (v60.11+) will wrap the subprocess in the
/// full §11 sandbox profile (`sandbox-exec` / `bwrap`).
///
/// `audit_dir` is validated for existence (it's where the future §12
/// egress audit log will live) but not yet written to.
pub async fn launch_stdio_server(
    manifest: &McpServerManifest,
    sandbox: &SandboxPolicy,
    audit_dir: &Path,
) -> Result<McpServerHandle, McpLaunchError> {
    // ---------- pre-launch validation ----------

    if !matches!(manifest.transport, Transport::Stdio) {
        return Err(McpLaunchError::UnsupportedTransport {
            name: manifest.name.clone(),
            transport: manifest.transport.as_str().to_string(),
        });
    }

    let command = manifest
        .command
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| McpLaunchError::InvalidManifest {
            name: manifest.name.clone(),
            reason: "stdio transport requires a non-empty `command`".into(),
        })?;

    // Audit dir is just validated existence-wise here; the §12 plumbing that
    // actually writes to it lands later.
    if !audit_dir.exists() {
        std::fs::create_dir_all(audit_dir).map_err(|e| McpLaunchError::InvalidManifest {
            name: manifest.name.clone(),
            reason: format!("audit_dir {audit_dir:?} could not be created: {e}"),
        })?;
    }

    // ---------- interpolate args + env ----------

    let mut interp_args: Vec<String> = Vec::with_capacity(manifest.args.len());
    for arg in &manifest.args {
        let v = mcp_interpolate(arg).map_err(|e| McpLaunchError::Interpolation {
            name: manifest.name.clone(),
            reason: e.to_string(),
        })?;
        interp_args.push(v);
    }
    let mut interp_env: std::collections::BTreeMap<String, String> = Default::default();
    for (k, v) in &manifest.env {
        let resolved = mcp_interpolate(v).map_err(|e| McpLaunchError::Interpolation {
            name: manifest.name.clone(),
            reason: format!("env {k:?}: {e}"),
        })?;
        interp_env.insert(k.clone(), resolved);
    }

    // ---------- build Command with hardened env ----------

    let mut cmd = Command::new(command);
    cmd.args(&interp_args);

    // env_clear + allowlist passthrough — same posture as `subprocess::run`.
    cmd.env_clear();
    for key in ENV_PASSTHROUGH {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    // Egress: if either the manifest OR the sandbox policy says "no net",
    // we install the proxy-to-port-1 block. AND-of-the-two would let either
    // surface relax the other; OR-of-the-two is the conservative read.
    let net_allowed = manifest.allow_net && sandbox.allow_net_flag();
    if !net_allowed {
        for key in [
            "http_proxy",
            "https_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
        ] {
            cmd.env(key, "http://127.0.0.1:1");
        }
        cmd.env("NO_PROXY", "");
        cmd.env("no_proxy", "");
    }

    // Manifest-declared env overrides (interpolated above). Applied AFTER
    // the egress block so a manifest that legitimately needs `http_proxy`
    // (e.g. corporate proxy on an `allow_net: true` server) can override.
    for (k, v) in &interp_env {
        cmd.env(k, v);
    }

    // ---------- spawn + handshake ----------

    let transport =
        rmcp::transport::TokioChildProcess::new(&mut cmd).map_err(|e| McpLaunchError::Spawn {
            name: manifest.name.clone(),
            command: command.to_string(),
            source: e,
        })?;

    let handshake = tokio::time::timeout(
        Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
        ().serve(transport),
    )
    .await
    .map_err(|_| McpLaunchError::Handshake {
        name: manifest.name.clone(),
        message: format!("initialize did not complete within {HANDSHAKE_TIMEOUT_MS}ms"),
    })?
    .map_err(|e: std::io::Error| McpLaunchError::Handshake {
        name: manifest.name.clone(),
        message: format!("{e}"),
    })?;

    // Snapshot server info for capability inspection. rmcp's `Peer::peer_info()`
    // returns `&ServerInfo`; clone once at launch so later access is borrow-free.
    let server_info: rmcp::model::ServerInfo = handshake.peer().peer_info().clone();

    // Protocol-version check. The version is a string like `"2024-11-05"`;
    // rmcp 0.1.5 wraps it in a `ProtocolVersion(String)` newtype. Match on the
    // string form so we don't bind to the specific newtype shape.
    let version = format!("{:?}", server_info.protocol_version);
    if !version.contains(SUPPORTED_PROTOCOL_VERSION) {
        // Best-effort cleanup; the spike confirmed `cancel()` is the safe path.
        let _ = handshake.cancel().await;
        return Err(McpLaunchError::ProtocolMismatch {
            name: manifest.name.clone(),
            version,
        });
    }

    // ---------- first-use sanity probe ----------
    //
    // List tools as a "is this server actually functional?" check. A server
    // that handshakes but errors on `tools/list` is mis-configured; we want
    // the launcher to surface that as `Refused` rather than letting the
    // caller find out at first dispatch.
    let probe = tokio::time::timeout(
        Duration::from_millis(FIRST_LIST_TIMEOUT_MS),
        handshake
            .peer()
            .list_tools(Some(PaginatedRequestParamInner { cursor: None })),
    )
    .await
    .map_err(|_| McpLaunchError::Refused {
        name: manifest.name.clone(),
        message: format!("first tools/list did not return within {FIRST_LIST_TIMEOUT_MS}ms"),
    })?;
    if let Err(e) = probe {
        let _ = handshake.cancel().await;
        return Err(McpLaunchError::Refused {
            name: manifest.name.clone(),
            message: format!("first tools/list refused: {e}"),
        });
    }

    Ok(McpServerHandle {
        name: manifest.name.clone(),
        client: handshake,
        server_info,
    })
}

/// Build a fresh `SandboxPolicy` suitable for launching MCP servers from
/// `workspace_root`. Convenience wrapper around `SandboxPolicy::restrictive`
/// that also wires `allow_net` through. Callers that want finer control
/// (extra read paths, etc.) construct the policy themselves.
pub fn default_sandbox_for_workspace(
    workspace_root: PathBuf,
    allow_net: bool,
) -> Result<SandboxPolicy, crate::sandbox::SandboxError> {
    let policy = SandboxPolicy::restrictive(workspace_root)?;
    if allow_net {
        Ok(policy.with_net())
    } else {
        Ok(policy)
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_config::{SideEffectClass, Transport};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// Construct a `McpServerManifest` from positional args. Tests do this
    /// often enough to factor out.
    fn manifest(
        name: &str,
        transport: Transport,
        command: Option<&str>,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        allow_net: bool,
    ) -> McpServerManifest {
        McpServerManifest {
            name: name.into(),
            transport,
            command: command.map(str::to_string),
            args,
            env,
            url: None,
            headers: BTreeMap::new(),
            side_effect_class: Some(SideEffectClass::LocalSafe),
            allow_net,
            enabled: true,
        }
    }

    fn tmp_workspace() -> TempDir {
        TempDir::new().unwrap()
    }

    #[tokio::test]
    async fn rejects_non_stdio_transport() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut m = manifest("fs", Transport::Http, None, vec![], BTreeMap::new(), false);
        m.url = Some("https://example/mcp".into());
        let audit = ws.path().join("audit");
        let err = launch_stdio_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::UnsupportedTransport { name, transport } => {
                assert_eq!(name, "fs");
                assert_eq!(transport, "http");
            }
            other => panic!("expected UnsupportedTransport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_stdio_with_missing_command() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let m = manifest("fs", Transport::Stdio, None, vec![], BTreeMap::new(), false);
        let audit = ws.path().join("audit");
        let err = launch_stdio_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::InvalidManifest { name, reason } => {
                assert_eq!(name, "fs");
                assert!(
                    reason.contains("command"),
                    "expected reason to mention command, got {reason:?}"
                );
            }
            other => panic!("expected InvalidManifest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_stdio_with_empty_command() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let m = manifest(
            "fs",
            Transport::Stdio,
            Some(""),
            vec![],
            BTreeMap::new(),
            false,
        );
        let audit = ws.path().join("audit");
        let err = launch_stdio_server(&m, &policy, &audit).await.unwrap_err();
        assert!(matches!(err, McpLaunchError::InvalidManifest { .. }));
    }

    #[tokio::test]
    async fn rejects_missing_env_interpolation() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert(
            "TOKEN".into(),
            "${env:ATELIER_MCP_LAUNCHER_TEST_DEFINITELY_UNSET_VAR}".into(),
        );
        // Defence: ensure the var isn't set by some surrounding context.
        // SAFETY: per-process env mutation, see `mcp_config::interpolate_*` tests.
        unsafe { std::env::remove_var("ATELIER_MCP_LAUNCHER_TEST_DEFINITELY_UNSET_VAR") };
        let m = manifest(
            "fs",
            Transport::Stdio,
            // /bin/true exists on macOS and every Linux distro CI runs on.
            Some("/usr/bin/true"),
            vec![],
            env,
            false,
        );
        let audit = ws.path().join("audit");
        let err = launch_stdio_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::Interpolation { name, reason } => {
                assert_eq!(name, "fs");
                assert!(
                    reason.contains("ATELIER_MCP_LAUNCHER_TEST_DEFINITELY_UNSET_VAR"),
                    "reason should mention the missing var name, got {reason:?}"
                );
            }
            other => panic!("expected Interpolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_failure_surfaces_typed_error() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let m = manifest(
            "fs",
            Transport::Stdio,
            Some("/this/binary/does/not/exist/atelier-mcp-spike-nonexistent"),
            vec![],
            BTreeMap::new(),
            false,
        );
        let audit = ws.path().join("audit");
        let err = launch_stdio_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::Spawn { name, command, .. } => {
                assert_eq!(name, "fs");
                assert!(command.contains("atelier-mcp-spike-nonexistent"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_failure_when_target_is_not_an_mcp_server() {
        // `/bin/cat` reads stdin and echoes it. It will never emit a valid
        // JSON-RPC `initialize` response, so the handshake must time out (or
        // fail with a transport error before timing out — both are fine).
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        // Cat needs to exist. On macOS it's at /bin/cat; on Linux too.
        let cat_path = if Path::new("/bin/cat").exists() {
            "/bin/cat"
        } else {
            "/usr/bin/cat"
        };
        let m = manifest(
            "fs",
            Transport::Stdio,
            Some(cat_path),
            vec![],
            BTreeMap::new(),
            false,
        );
        let audit = ws.path().join("audit");

        // We deliberately pass a SHORTER timeout than the launcher's default
        // 10s by wrapping the launch in another timeout — otherwise this test
        // would take 10s every run, which is too slow.
        // We also tolerate either Handshake or ChildExited as the variant:
        // depending on platform, cat may exit on EOF (when we close its stdin)
        // before the handshake timer fires.
        let outcome = tokio::time::timeout(
            Duration::from_millis(15_000),
            launch_stdio_server(&m, &policy, &audit),
        )
        .await
        .expect("launch should not exceed 15s wall-clock");
        match outcome {
            Err(McpLaunchError::Handshake { name, .. }) => assert_eq!(name, "fs"),
            Err(other) => panic!("expected Handshake error, got {other:?}"),
            Ok(_) => panic!("a /bin/cat 'server' should NEVER complete the handshake"),
        }
    }

    #[test]
    fn mcp_tool_from_rmcp_tool() {
        use rmcp::model::{JsonObject, Tool};
        use std::sync::Arc;

        let mut schema: JsonObject = serde_json::Map::new();
        schema.insert("type".into(), serde_json::Value::String("object".into()));
        let rmcp_tool = Tool::new("greet", "say hi", Arc::new(schema));
        let mt = McpTool::from(&rmcp_tool);
        assert_eq!(mt.name, "greet");
        assert_eq!(mt.description, "say hi");
        // input_schema wraps the map in Value::Object.
        let obj = mt.input_schema.as_object().expect("must be a json object");
        assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("object"));
    }

    #[test]
    fn default_sandbox_for_workspace_respects_allow_net() {
        let ws = tmp_workspace();
        let p_off = default_sandbox_for_workspace(ws.path().to_path_buf(), false).unwrap();
        assert!(!p_off.allow_net_flag());
        let p_on = default_sandbox_for_workspace(ws.path().to_path_buf(), true).unwrap();
        assert!(p_on.allow_net_flag());
    }

    #[test]
    fn launch_error_classification_is_stable() {
        let cases = [
            (
                McpLaunchError::UnsupportedTransport {
                    name: "x".into(),
                    transport: "http".into(),
                },
                true,
            ),
            (
                McpLaunchError::InvalidManifest {
                    name: "x".into(),
                    reason: "y".into(),
                },
                true,
            ),
            (
                McpLaunchError::Interpolation {
                    name: "x".into(),
                    reason: "y".into(),
                },
                true,
            ),
            (
                McpLaunchError::ProtocolMismatch {
                    name: "x".into(),
                    version: "y".into(),
                },
                true,
            ),
            (
                McpLaunchError::Spawn {
                    name: "x".into(),
                    command: "y".into(),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "z"),
                },
                false,
            ),
            (
                McpLaunchError::Handshake {
                    name: "x".into(),
                    message: "y".into(),
                },
                false,
            ),
            (
                McpLaunchError::ChildExited {
                    name: "x".into(),
                    code: Some(1),
                },
                false,
            ),
            (
                McpLaunchError::Refused {
                    name: "x".into(),
                    message: "y".into(),
                },
                false,
            ),
        ];
        for (e, expected) in cases {
            assert_eq!(e.is_config_error(), expected, "wrong class for {e:?}");
        }
    }
}
