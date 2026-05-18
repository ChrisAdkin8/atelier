//! §15 MCP-server tool registration helper.
//!
//! At session startup the runner walks the loaded `mcp_servers.json`
//! manifests, asks the `McpApprovals` store which servers the user has
//! already trusted, launches each approved + enabled server via
//! [`crate::mcp::launch_stdio_server`], then calls
//! [`register_mcp_servers`] to wrap each server's advertised tools as
//! [`crate::mcp::mcp_tool::McpToolWrapper`]s and add them to the
//! dispatcher's [`crate::dispatcher::ToolRegistry`] alongside the
//! built-ins.
//!
//! Pending-approval and refused servers don't register; the report's
//! [`RegisterMcpReport::servers_pending_approval`] field surfaces them
//! so the GUI's first-use trust-budget prompt has a single source of
//! truth (no second walk through the manifest list).
//!
//! Server-launch failures are NOT fatal — they're collected in
//! [`RegisterMcpReport::server_failures`] so one badly-configured
//! server can't break the whole session.  The runner is expected to
//! surface these on the bus as warnings.
//!
//! This module is the data-layer half of the wiring; the call-site
//! plumbing (constructing the report, emitting events) is the
//! Runner's job (handled by Bundle C3).

use std::path::Path;
use std::sync::Arc;

use crate::context::{
    ContextError, ContextItem, ContextItemId, ContextManager, Payload, Provenance, TokenCount,
    TokenSource,
};
use crate::dispatcher::{SideEffectClass, ToolRegistry};
use crate::mcp::launch_stdio_server;
use crate::mcp::mcp_tool::McpToolWrapper;
use crate::mcp::stdio_launcher::{McpResourceDescriptor, McpServerHandle};
use crate::mcp::McpLaunchError;
use crate::mcp_config::{
    McpApprovals, McpServerManifest, SideEffectClass as ConfigSideEffectClass, Transport,
};
use crate::sandbox::SandboxPolicy;

/// Outcome of one `register_mcp_servers` call.  Returned by value so
/// the caller (Runner) can ledger / broadcast without re-walking the
/// manifest list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RegisterMcpReport {
    /// Names of servers whose launch + tool-registration succeeded.
    pub servers_registered: Vec<String>,
    /// Names of every tool that landed in the registry.  Order is
    /// `(server, tool)` in input order — useful for tests asserting
    /// no duplicates.
    pub tools_registered: Vec<String>,
    /// Names of servers from the manifest list that haven't been
    /// approved yet.  The GUI's trust-budget prompt walks this to
    /// build the first-use approval flow.  Disabled servers do NOT
    /// appear here (they're filtered before approval is checked).
    pub servers_pending_approval: Vec<String>,
    /// Server-launch failures, keyed by server name.  The runner
    /// surfaces these as warnings; one bad server doesn't abort the
    /// rest of the registration.
    pub server_failures: Vec<ServerFailure>,
}

/// One server-launch failure recorded by [`register_mcp_servers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFailure {
    pub server_name: String,
    /// String form of the typed [`McpLaunchError`].  We stringify at
    /// the boundary because `McpLaunchError`'s `Spawn` variant wraps
    /// an `io::Error`, which is not `PartialEq`/`Clone` — keeping the
    /// outer report `Eq + Clone` makes it cheap to assert on in tests
    /// and to ride along on the session event bus.
    pub reason: String,
}

/// Bundle of live MCP server handles + the typed [`RegisterMcpReport`]
/// they produced.  Returned by
/// [`launch_and_register_mcp_servers`] when the caller needs both —
/// typically the Runner, which needs the handles to call
/// `register_mcp_resources_as_context` (and, later, to issue shutdown
/// on session teardown).
///
/// Two-phase shape: `report` is `Eq + Clone` (rides on the bus,
/// assertions in tests); `handles` is not (it owns rmcp running
/// services).
pub struct LaunchedMcpServers {
    pub report: RegisterMcpReport,
    /// Live handles, in input-manifest order, for every server in
    /// `report.servers_registered`.  Same length as that list.
    pub handles: Vec<Arc<McpServerHandle>>,
}

/// v60.11 — register every enabled, approved MCP server's tools into
/// the dispatcher registry alongside the built-ins.  Called once at
/// session startup (typically right after the built-in tools are
/// registered).
///
/// Filtering rules:
///   1. `manifest.enabled == false` → skipped silently (not approval-
///      pending, not a failure — the user disabled it).
///   2. `approvals.is_approved(&manifest.name) == false` → recorded in
///      `servers_pending_approval` and skipped (no launch).
///   3. `manifest.transport != Stdio` → recorded as a failure with a
///      typed-error reason (we don't ship the HTTP launcher in this
///      bundle; C1 is on that surface).
///   4. Launch failure → recorded as a failure; the rest of the list
///      still processes.
///   5. `list_tools` failure → recorded as a failure; any tools we'd
///      already registered for this server stay in the registry (the
///      registry has no rollback API and a partial registration is
///      strictly better than a forced full-rollback if the server is
///      live enough to invoke other tools).
///
/// Tool-name collision policy:
///   - If a built-in tool already uses a given name, the MCP tool is
///     skipped with a `ServerFailure` entry.  Built-ins win because
///     they were registered first by the runner.
///   - Two MCP servers exposing the same tool name: the second
///     registration fails the same way.  Operators avoid this by
///     prefixing tool names in the server's own manifest (`tools/list`
///     surface) — we don't auto-prefix because the prefix would then
///     have to flow into the model's tool-call surface and the
///     adapter's `ToolSpec` map, which is out of scope here.
pub async fn register_mcp_servers(
    registry: &mut ToolRegistry,
    manifests: &[McpServerManifest],
    approvals: &McpApprovals,
    sandbox: &SandboxPolicy,
    audit_dir: &Path,
) -> RegisterMcpReport {
    launch_and_register_mcp_servers(registry, manifests, approvals, sandbox, audit_dir)
        .await
        .report
}

/// Same as [`register_mcp_servers`] but also returns the live
/// [`McpServerHandle`]s for every successfully-registered server so
/// the caller can subsequently surface their resources via
/// [`register_mcp_resources_as_context`] without re-launching.
///
/// The Runner is the primary caller; tests that don't need the
/// handles can keep using [`register_mcp_servers`].
pub async fn launch_and_register_mcp_servers(
    registry: &mut ToolRegistry,
    manifests: &[McpServerManifest],
    approvals: &McpApprovals,
    sandbox: &SandboxPolicy,
    audit_dir: &Path,
) -> LaunchedMcpServers {
    let mut report = RegisterMcpReport::default();
    let mut handles: Vec<Arc<McpServerHandle>> = Vec::new();

    for manifest in manifests {
        // (1) disabled — silent skip.
        if !manifest.enabled {
            continue;
        }

        // (2) not yet approved — record + skip.
        if !approvals.is_approved(&manifest.name) {
            report.servers_pending_approval.push(manifest.name.clone());
            continue;
        }

        // (3) non-stdio transport not yet supported.
        if !matches!(manifest.transport, Transport::Stdio) {
            report.server_failures.push(ServerFailure {
                server_name: manifest.name.clone(),
                reason: format!(
                    "transport {:?} not supported in this build (only stdio)",
                    manifest.transport.as_str()
                ),
            });
            continue;
        }

        // (4) launch the server.
        let handle = match launch_stdio_server(manifest, sandbox, audit_dir).await {
            Ok(h) => Arc::new(h),
            Err(e) => {
                report.server_failures.push(ServerFailure {
                    server_name: manifest.name.clone(),
                    reason: launch_error_to_string(&e),
                });
                continue;
            }
        };

        // (5) list_tools, then register each.
        let tools = match handle.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                report.server_failures.push(ServerFailure {
                    server_name: manifest.name.clone(),
                    reason: format!("list_tools failed: {}", launch_error_to_string(&e)),
                });
                continue;
            }
        };

        let server_side_effect = manifest_side_effect(manifest);
        let mut registered_this_server = 0usize;
        for tool in tools {
            let wrapper = match McpToolWrapper::new(
                &manifest.name,
                tool.name.clone(),
                tool.description.clone(),
                tool.input_schema.clone(),
                handle.clone(),
                server_side_effect,
            ) {
                Ok(w) => w,
                Err(reason) => {
                    report.server_failures.push(ServerFailure {
                        server_name: manifest.name.clone(),
                        reason: format!("tool {:?} has invalid input_schema: {reason}", tool.name),
                    });
                    continue;
                }
            };
            let name = tool.name.clone();
            match registry.register(Arc::new(wrapper)) {
                Ok(()) => {
                    report.tools_registered.push(name);
                    registered_this_server += 1;
                }
                Err(e) => {
                    report.server_failures.push(ServerFailure {
                        server_name: manifest.name.clone(),
                        reason: format!("tool {name:?} registration failed: {e}"),
                    });
                }
            }
        }

        if registered_this_server > 0 {
            report.servers_registered.push(manifest.name.clone());
            handles.push(handle);
        }
    }

    LaunchedMcpServers { report, handles }
}

/// v60.11 (§15) — project an [`McpResourceDescriptor`] onto a
/// [`ContextItem`] suitable for [`ContextManager::add`].  Pure: no
/// I/O, no async.  The dispatcher's
/// [`register_mcp_resources_as_context`] uses this internally; tests
/// can drive it directly to assert the projection shape.
///
/// `now` carries the RFC 3339 timestamp the caller wants stamped on
/// `added_at` / `last_used` (matching the rest of the harness's
/// "caller supplies clock" convention).
pub fn mcp_resource_to_context_item(
    server_name: &str,
    resource: &McpResourceDescriptor,
    now: &str,
) -> ContextItem {
    // Two payload shapes are sensible for an MCP-advertised resource:
    //
    //   * `Payload::BlobRef` — opaque; the URI doubles as the
    //     "content address" since rmcp's spec ties identity to URI
    //     (the resource may be virtual / non-file-backed).  The
    //     sha256 is computed from the URI bytes so the §14 diff-blob
    //     store doesn't collide with file-content blobs.
    //   * `Payload::FileRef` — only when the URI is a `file://` URI
    //     pointing at a path; useful for the GUI's file-icon row.
    //     The harness can't *resolve* the file yet (the server owns
    //     access), but treating it as a FileRef gives the §5 panel a
    //     better label.
    //
    // We default to BlobRef for safety: an arbitrary URI scheme
    // (`memory://`, `db://schema/users`) wouldn't make sense as a
    // FileRef.  The GUI can re-skin on top of that.
    let payload = Payload::BlobRef {
        sha256_hex: sha256_hex_of_str(&resource.uri),
        mime_type: resource.mime_type.clone(),
    };

    ContextItem {
        id: ContextItemId::new(),
        payload,
        tokens: TokenCount {
            // The harness can't count tokens for a resource it hasn't
            // fetched; surface Unavailable so the §5 panel renders the
            // token cell as gray instead of underreporting.
            count: 0,
            source: TokenSource::Unavailable,
        },
        provenance: Provenance::McpResource {
            server_name: server_name.to_string(),
            resource_uri: resource.uri.clone(),
        },
        // User-driven pinning still applies; resources start unpinned.
        pinned: false,
        added_at: now.to_string(),
        last_used: now.to_string(),
    }
}

/// Compute `sha256_hex` of an arbitrary string.  Used by
/// [`mcp_resource_to_context_item`] to derive a stable blob id from
/// the resource URI.  Centralised so a future change of digest
/// (e.g., blake3) touches one site.  Hex-encodes manually to avoid
/// pulling in the `hex` crate just for this one site —
/// `crate::staging::sha256` returns the same `[u8; 32]` shape.
fn sha256_hex_of_str(s: &str) -> String {
    let digest = crate::staging::sha256(s.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// v60.11 (§15) — for every server in `handles`, call `list_resources`
/// and add the projected [`ContextItem`]s to `ctx_manager` (each as
/// [`Provenance::McpResource`]).  Returns a per-server projection
/// report so the Runner can ledger or surface server-specific
/// failures.
///
/// `handles` typically comes straight from
/// [`launch_and_register_mcp_servers`]'s `LaunchedMcpServers.handles`,
/// paired with `report.servers_registered` for the names.  We take
/// them as parallel `&[Arc<McpServerHandle>]` + `&[String]` rather
/// than re-introducing a packed struct so the caller can pass a
/// subset (e.g. retry-only-the-failed-server flows).
///
/// Servers whose `list_resources` returns an error (typically because
/// they don't expose the resources protocol) are recorded as
/// `failed_servers` but the rest still proceed.  An empty `Ok` list
/// (the server speaks the protocol but advertises zero resources)
/// is silently fine.
pub async fn register_mcp_resources_as_context(
    ctx_manager: &mut ContextManager,
    server_names: &[String],
    handles: &[Arc<McpServerHandle>],
    now: &str,
) -> RegisterMcpResourcesReport {
    debug_assert_eq!(
        server_names.len(),
        handles.len(),
        "server_names and handles must be parallel slices"
    );
    let mut out = RegisterMcpResourcesReport::default();
    for (name, handle) in server_names.iter().zip(handles.iter()) {
        match handle.list_resources().await {
            Ok(resources) => {
                let mut added_here = 0usize;
                for r in &resources {
                    let item = mcp_resource_to_context_item(name, r, now);
                    let id = item.id;
                    match try_add_with_collision_recovery(ctx_manager, item) {
                        Ok(()) => {
                            added_here += 1;
                            out.added_item_ids.push(id);
                        }
                        Err(e) => {
                            out.failed_servers.push(ServerFailure {
                                server_name: name.clone(),
                                reason: format!("add {:?}: {e}", r.uri),
                            });
                        }
                    }
                }
                if added_here > 0 {
                    out.servers_with_resources.push(name.clone());
                }
            }
            Err(e) => {
                out.failed_servers.push(ServerFailure {
                    server_name: name.clone(),
                    reason: format!("list_resources: {}", launch_error_to_string(&e)),
                });
            }
        }
    }
    out
}

/// Outcome of [`register_mcp_resources_as_context`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RegisterMcpResourcesReport {
    /// Names of servers that successfully exposed ≥1 resource.
    pub servers_with_resources: Vec<String>,
    /// Ids of every [`ContextItem`] added by this call.  Same order
    /// as added.  Lets the Runner emit a snapshot covering only the
    /// fresh items.
    pub added_item_ids: Vec<ContextItemId>,
    /// Servers whose `list_resources` failed (or whose individual
    /// items failed to add).  Per-server, not per-resource.
    pub failed_servers: Vec<ServerFailure>,
}

/// `ContextManager::add` is infallible at the API level (it
/// silently overwrites an existing id), but our newly-minted ids
/// from `ContextItemId::new()` shouldn't collide.  This wrapper
/// exists for forward-compat in case the manager ever returns a
/// typed collision error; today it's effectively
/// `manager.add(item); Ok(())`.
fn try_add_with_collision_recovery(
    manager: &mut ContextManager,
    item: ContextItem,
) -> Result<(), ContextError> {
    manager.add(item);
    Ok(())
}

/// Project the manifest's optional [`ConfigSideEffectClass`] onto the
/// dispatcher's [`SideEffectClass`].  Mirrors the labels exactly (kebab
/// case on the wire).  Missing manifest value defaults to `LocalRisky`
/// — the conservative choice for an untyped external tool.
fn manifest_side_effect(manifest: &McpServerManifest) -> SideEffectClass {
    match manifest.side_effect_class {
        Some(ConfigSideEffectClass::LocalSafe) => SideEffectClass::LocalSafe,
        Some(ConfigSideEffectClass::LocalRisky) => SideEffectClass::LocalRisky,
        Some(ConfigSideEffectClass::SharedState) => SideEffectClass::SharedState,
        Some(ConfigSideEffectClass::Irreversible) => SideEffectClass::Irreversible,
        None => SideEffectClass::LocalRisky,
    }
}

fn launch_error_to_string(e: &McpLaunchError) -> String {
    format!("{e}")
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::{Tool, ToolContext, ToolResult};
    use crate::error::ToolError;
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// Stub built-in tool used to test the name-collision branch.
    struct StubTool {
        name: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn side_effect_class(&self) -> SideEffectClass {
            SideEffectClass::LocalSafe
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("test stub never runs")
        }
    }

    fn manifest(name: &str, transport: Transport, enabled: bool) -> McpServerManifest {
        McpServerManifest {
            name: name.into(),
            transport,
            command: match transport {
                Transport::Stdio => Some("/usr/bin/true".into()),
                _ => None,
            },
            args: vec![],
            env: BTreeMap::new(),
            url: match transport {
                Transport::Http | Transport::Sse => Some("https://example/mcp".into()),
                _ => None,
            },
            headers: BTreeMap::new(),
            side_effect_class: Some(ConfigSideEffectClass::LocalSafe),
            allow_net: false,
            allowed_hosts: None,
            enabled,
        }
    }

    fn tmp_workspace_and_sandbox() -> (TempDir, SandboxPolicy, std::path::PathBuf) {
        let ws = TempDir::new().unwrap();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let audit = ws.path().join("audit");
        (ws, policy, audit)
    }

    #[tokio::test]
    async fn empty_manifests_yields_empty_report() {
        let (_ws, sandbox, audit) = tmp_workspace_and_sandbox();
        let approvals = McpApprovals::default();
        let mut registry = ToolRegistry::new();
        let report = register_mcp_servers(&mut registry, &[], &approvals, &sandbox, &audit).await;
        assert_eq!(report, RegisterMcpReport::default());
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn disabled_servers_silently_skipped() {
        let (_ws, sandbox, audit) = tmp_workspace_and_sandbox();
        let approvals = McpApprovals::default();
        let mut registry = ToolRegistry::new();
        let manifests = vec![manifest("disabled", Transport::Stdio, false)];
        let report =
            register_mcp_servers(&mut registry, &manifests, &approvals, &sandbox, &audit).await;
        assert!(report.servers_registered.is_empty());
        assert!(report.tools_registered.is_empty());
        assert!(report.servers_pending_approval.is_empty());
        assert!(report.server_failures.is_empty());
    }

    #[tokio::test]
    async fn unapproved_servers_recorded_pending_not_launched() {
        let (_ws, sandbox, audit) = tmp_workspace_and_sandbox();
        // Empty approvals store — `pending-fs` is not approved.
        let approvals = McpApprovals::default();
        let mut registry = ToolRegistry::new();
        let manifests = vec![manifest("pending-fs", Transport::Stdio, true)];
        let report =
            register_mcp_servers(&mut registry, &manifests, &approvals, &sandbox, &audit).await;
        assert_eq!(
            report.servers_pending_approval,
            vec!["pending-fs".to_string()]
        );
        assert!(report.servers_registered.is_empty());
        assert!(report.tools_registered.is_empty());
        assert!(report.server_failures.is_empty());
        // No tools landed in the registry because the server was
        // never launched.
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn non_stdio_transport_recorded_as_failure() {
        let (_ws, sandbox, audit) = tmp_workspace_and_sandbox();
        let mut approvals = McpApprovals::default();
        approvals.approve("remote-fs", "2026-05-17T00:00:00Z");
        let mut registry = ToolRegistry::new();
        let manifests = vec![manifest("remote-fs", Transport::Http, true)];
        let report =
            register_mcp_servers(&mut registry, &manifests, &approvals, &sandbox, &audit).await;
        assert_eq!(report.server_failures.len(), 1);
        let failure = &report.server_failures[0];
        assert_eq!(failure.server_name, "remote-fs");
        assert!(
            failure.reason.contains("transport") && failure.reason.contains("http"),
            "got {:?}",
            failure.reason
        );
        assert!(report.servers_registered.is_empty());
    }

    #[tokio::test]
    async fn launch_failure_recorded_session_continues() {
        // Approved stdio server pointing at a binary that doesn't
        // exist — launch fails with Spawn, server_failures records it,
        // and the registry stays untouched.
        let (_ws, sandbox, audit) = tmp_workspace_and_sandbox();
        let mut approvals = McpApprovals::default();
        approvals.approve("bad-server", "2026-05-17T00:00:00Z");

        let m = McpServerManifest {
            name: "bad-server".into(),
            transport: Transport::Stdio,
            command: Some("/nonexistent/atelier-mcp-test-binary".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            side_effect_class: Some(ConfigSideEffectClass::LocalSafe),
            allow_net: false,
            allowed_hosts: None,
            enabled: true,
        };

        let mut registry = ToolRegistry::new();
        // Pre-register a built-in so we can assert the failed server
        // didn't trample anything.
        registry
            .register(Arc::new(StubTool {
                name: "stub_builtin".into(),
            }))
            .expect("stub register");

        let report = register_mcp_servers(&mut registry, &[m], &approvals, &sandbox, &audit).await;
        assert_eq!(report.server_failures.len(), 1);
        assert_eq!(report.server_failures[0].server_name, "bad-server");
        assert!(report.servers_registered.is_empty());
        // The pre-existing built-in stayed put.
        assert!(registry.get("stub_builtin").is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn manifest_side_effect_defaults_to_local_risky() {
        let mut m = manifest("x", Transport::Stdio, true);
        m.side_effect_class = None;
        assert_eq!(manifest_side_effect(&m), SideEffectClass::LocalRisky);
    }

    #[test]
    fn mcp_resource_projection_shape() {
        let r = McpResourceDescriptor {
            uri: "file:///etc/hosts".into(),
            name: "hosts".into(),
            mime_type: Some("text/plain".into()),
            description: None,
        };
        let item = mcp_resource_to_context_item("fs", &r, "2026-05-17T00:00:00Z");
        match item.provenance {
            Provenance::McpResource {
                server_name,
                resource_uri,
            } => {
                assert_eq!(server_name, "fs");
                assert_eq!(resource_uri, "file:///etc/hosts");
            }
            other => panic!("expected McpResource, got {other:?}"),
        }
        match &item.payload {
            Payload::BlobRef {
                sha256_hex,
                mime_type,
            } => {
                assert_eq!(sha256_hex.len(), 64);
                assert!(sha256_hex.chars().all(|c| c.is_ascii_hexdigit()));
                assert_eq!(mime_type.as_deref(), Some("text/plain"));
            }
            other => panic!("expected BlobRef, got {other:?}"),
        }
        assert_eq!(item.tokens.count, 0);
        assert!(matches!(item.tokens.source, TokenSource::Unavailable));
        assert!(!item.pinned);
        assert_eq!(item.added_at, "2026-05-17T00:00:00Z");
    }

    #[test]
    fn mcp_resource_projection_blob_hash_is_deterministic() {
        let r1 = McpResourceDescriptor {
            uri: "memory://foo".into(),
            name: "n".into(),
            mime_type: None,
            description: None,
        };
        let r2 = r1.clone();
        let a = mcp_resource_to_context_item("s", &r1, "t");
        let b = mcp_resource_to_context_item("s", &r2, "t");
        match (&a.payload, &b.payload) {
            (Payload::BlobRef { sha256_hex: ha, .. }, Payload::BlobRef { sha256_hex: hb, .. }) => {
                assert_eq!(ha, hb)
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn manifest_side_effect_round_trips_all_variants() {
        for (config, dispatcher) in [
            (ConfigSideEffectClass::LocalSafe, SideEffectClass::LocalSafe),
            (
                ConfigSideEffectClass::LocalRisky,
                SideEffectClass::LocalRisky,
            ),
            (
                ConfigSideEffectClass::SharedState,
                SideEffectClass::SharedState,
            ),
            (
                ConfigSideEffectClass::Irreversible,
                SideEffectClass::Irreversible,
            ),
        ] {
            let mut m = manifest("x", Transport::Stdio, true);
            m.side_effect_class = Some(config);
            assert_eq!(manifest_side_effect(&m), dispatcher);
        }
    }
}
