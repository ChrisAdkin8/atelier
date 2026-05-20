//! §15 HTTP / SSE MCP server launcher.
//!
//! Sibling of [`crate::mcp::stdio_launcher`]. Connects to a remote MCP
//! server over HTTP+SSE using rmcp's `SseTransport`. Returns the same
//! [`McpServerHandle`] type so dispatcher integration in C2/v60.11+ can
//! treat both transports uniformly.
//!
//! Spec §12 line 656 — *"MCP HTTP/SSE servers count as egress targets and
//! are logged the same way as LLM providers. The `provider` field on the
//! audit record carries the MCP server name."* — that's what this module
//! emits via [`crate::audit::McpEgressEvent`] / [`append_mcp_egress`].
//!
//! ## Transport choice
//!
//! rmcp 0.1.5 ships only one remote transport: `SseTransport` (an HTTP
//! POST endpoint for client→server messages + a server→client SSE
//! stream). It has no separate "HTTP-only" transport. So both
//! `Transport::Http` and `Transport::Sse` manifests route through the
//! same `SseTransport` here. The manifest enum distinction is preserved
//! for forward-compatibility — once rmcp ships streamable-HTTP per the
//! MCP 2025-03-26 spec, the `Http` variant can switch over without a
//! manifest schema change. For now the audit row's `provider` field
//! identifies which server connected and the `phase`/`outcome`/`reason`
//! triplet captures lifecycle detail.
//!
//! ## `allow_net` semantics
//!
//! HTTP/SSE servers definitionally require network. If `manifest.allow_net
//! == false`, the launcher refuses with [`McpLaunchError::Refused`] and
//! writes a `blocked` audit row — same posture as the §11 subprocess
//! egress block but at the manifest level. Stdio launchers DO support
//! `allow_net: false` (the proxy-to-port-1 trick blocks egress while
//! letting the subprocess run); HTTP/SSE has no such middle ground.
//!
//! ## Header redaction
//!
//! Headers are resolved at launch time via `mcp_interpolate` (the same
//! function the stdio launcher uses for env vars). Resolved values are
//! baked into the reqwest `Client::default_headers` so they ride on
//! every outgoing request. They are NEVER:
//!
//!   - persisted back to the manifest file
//!   - written to the audit log (the [`McpEgressEvent`] shape has no
//!     `headers` field at all — defence in depth against future
//!     drift)
//!   - included in error payloads (`HttpStatus` / `SseStream` carry
//!     status code / message string, no headers)
//!
//! ## What this bundle does NOT do
//!
//!   - Wire MCP tools into `crate::dispatcher` (C2's territory).
//!   - Surface `Resources` / `Prompts` (later bundles).
//!   - Per-request audit on `call_tool` invocations after launch
//!     (handled by the dispatcher once it grows the §12 egress hook).
//!     The launcher itself audits handshake + the initial tools/list
//!     probe.

use std::path::Path;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::model::PaginatedRequestParamInner;
use rmcp::transport::SseTransport;
use rmcp::ServiceExt;

use crate::audit::{append_mcp_egress, McpEgressEvent, McpEgressOutcome, McpEgressPhase};
use crate::mcp::errors::McpLaunchError;
use crate::mcp::stdio_launcher::{
    McpServerHandle, FIRST_LIST_TIMEOUT_MS, HANDSHAKE_TIMEOUT_MS, SUPPORTED_PROTOCOL_VERSION,
};
use crate::mcp_config::{interpolate as mcp_interpolate, McpServerManifest, Transport};
use crate::time::now_rfc3339;

/// Default reqwest connect+read timeout for the SSE transport, in
/// milliseconds. Sized to match the stdio handshake budget so a slow
/// server times out the same way regardless of transport.
const HTTP_REQUEST_TIMEOUT_MS: u64 = HANDSHAKE_TIMEOUT_MS;

/// Filename inside `audit_dir` where the launcher appends
/// `McpEgressEvent` rows. Same name the stdio launcher's future audit
/// hook will use; we keep all egress kinds in one NDJSON file so
/// `atelier audit export` can stream them with a single open.
const AUDIT_LOG_FILE: &str = "audit.log";

/// Launch a remote HTTP/SSE MCP server described by `manifest`. Performs
/// the same `initialize` handshake + first-`tools/list` probe as the
/// stdio launcher, but over SSE.
///
/// Returns an [`McpServerHandle`] on success — the same type the stdio
/// launcher returns, so the dispatcher integration in C2/v60.11+ doesn't
/// need a per-transport switch.
///
/// Audit posture: every outbound request the launcher initiates is
/// recorded in `<audit_dir>/audit.log` as a `kind: "mcp-http-request"`
/// NDJSON row. The `Authorization`-bearing headers are never serialized
/// — the audit shape has no headers field at all (see module docs).
pub async fn launch_http_server(
    manifest: &McpServerManifest,
    sandbox: &crate::sandbox::SandboxPolicy,
    audit_dir: &Path,
) -> Result<McpServerHandle, McpLaunchError> {
    // ---------- pre-launch validation ----------

    if !matches!(manifest.transport, Transport::Http | Transport::Sse) {
        return Err(McpLaunchError::UnsupportedTransport {
            name: manifest.name.clone(),
            transport: manifest.transport.as_str().to_string(),
        });
    }

    // `allow_net == false` on an http/sse manifest is a configuration
    // error: HTTP/SSE servers definitionally need network. Refuse to
    // launch and emit a `blocked` audit row so the trust-budget UI can
    // surface "edit your `mcp_servers.json`" with full context.
    //
    // The sandbox's own `allow_net_flag` is informational here — the
    // SSE transport runs in-process (rmcp owns the reqwest client), so
    // there's no subprocess to scope. Manifest-level intent is the
    // ground truth.
    let _ = sandbox; // intentional: scoped allow_net is stdio's tool
    if !manifest.allow_net {
        // Audit the refusal before returning. `url` is the manifest's
        // declared URL (which we've already established is required by
        // the schema for http/sse transports, but defend anyway).
        let url = manifest.url.clone().unwrap_or_default();
        write_audit_row(
            audit_dir,
            McpEgressEvent::new(
                now_rfc3339(),
                &manifest.name,
                &url,
                McpEgressPhase::Handshake,
                McpEgressOutcome::Blocked,
                Some("allow_net=false".into()),
                None,
            ),
        );
        return Err(McpLaunchError::Refused {
            name: manifest.name.clone(),
            message: "HTTP/SSE transport requires allow_net=true".into(),
        });
    }

    let url = manifest
        .url
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| McpLaunchError::InvalidManifest {
            name: manifest.name.clone(),
            reason: "http/sse transport requires a non-empty `url`".into(),
        })?;

    // Audit dir validated existence-wise here. The append helper will
    // create it later, but doing this up-front means we fail loud if
    // the parent is e.g. read-only.
    if !audit_dir.exists() {
        std::fs::create_dir_all(audit_dir).map_err(|e| McpLaunchError::InvalidManifest {
            name: manifest.name.clone(),
            reason: format!("audit_dir {audit_dir:?} could not be created: {e}"),
        })?;
    }

    // ---------- resolve headers (request-time interpolation) ----------
    //
    // Resolved values live ONLY in the reqwest client's default_headers
    // for the lifetime of this process — never persisted to disk, never
    // copied into manifest values, never echoed into audit rows.
    let mut header_map = HeaderMap::new();
    for (k, raw) in &manifest.headers {
        let resolved = mcp_interpolate(raw).map_err(|e| McpLaunchError::Interpolation {
            name: manifest.name.clone(),
            reason: format!("header {k:?}: {e}"),
        })?;
        let name =
            HeaderName::from_bytes(k.as_bytes()).map_err(|e| McpLaunchError::InvalidHeader {
                name: manifest.name.clone(),
                header: k.clone(),
                reason: format!("invalid header name: {e}"),
            })?;
        let value =
            HeaderValue::from_str(&resolved).map_err(|e| McpLaunchError::InvalidHeader {
                name: manifest.name.clone(),
                header: k.clone(),
                // Value is intentionally NOT echoed (it may be the
                // resolved secret). Only the structural reason.
                reason: format!("invalid header value: {e}"),
            })?;
        header_map.insert(name, value);
    }

    // ---------- build reqwest client ----------

    // v60.34 (M23) — disable reqwest's default proxy-from-env (HTTP_PROXY
    // / HTTPS_PROXY / NO_PROXY). Those env vars are NOT on
    // `subprocess::ENV_PASSTHROUGH`; allowing them to silently redirect
    // MCP traffic through an attacker-controlled proxy would defeat the
    // §12 egress audit trail. A future opt-in path can re-add proxy
    // support via an explicit manifest field.
    let client = reqwest::Client::builder()
        .default_headers(header_map)
        .timeout(Duration::from_millis(HTTP_REQUEST_TIMEOUT_MS))
        .no_proxy()
        .build()
        .map_err(|e| McpLaunchError::SseStream {
            name: manifest.name.clone(),
            message: format!("build reqwest client: {e}"),
        })?;

    // ---------- handshake (SSE endpoint negotiation + rmcp initialize) ----------

    let handshake_phase = McpEgressPhase::Handshake;
    let transport = match tokio::time::timeout(
        Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
        SseTransport::start_with_client(url, client),
    )
    .await
    {
        Err(_) => {
            audit_failure(
                audit_dir,
                &manifest.name,
                url,
                handshake_phase,
                "transport-timeout",
            );
            return Err(McpLaunchError::Handshake {
                name: manifest.name.clone(),
                message: format!(
                    "SSE endpoint negotiation timed out after {HANDSHAKE_TIMEOUT_MS}ms"
                ),
            });
        }
        Ok(Err(e)) => {
            // Try to surface the HTTP status code if reqwest knows it
            // — gives the UI a precise "is this a 401 or a 503?" signal.
            let status = sse_error_status(&e);
            audit_failure(
                audit_dir,
                &manifest.name,
                url,
                handshake_phase,
                if status.is_some() {
                    "http-status"
                } else {
                    "sse-stream"
                },
            );
            return Err(match status {
                Some(s) => McpLaunchError::HttpStatus {
                    name: manifest.name.clone(),
                    status: s,
                },
                None => McpLaunchError::SseStream {
                    name: manifest.name.clone(),
                    message: format!("{e}"),
                },
            });
        }
        Ok(Ok(t)) => t,
    };

    // SSE endpoint negotiation succeeded — write the audit row for it.
    write_audit_row(
        audit_dir,
        McpEgressEvent::new(
            now_rfc3339(),
            &manifest.name,
            url,
            handshake_phase,
            McpEgressOutcome::Success,
            None,
            None,
        ),
    );

    // Now drive the rmcp `initialize` JSON-RPC handshake on top of the
    // negotiated SSE transport.
    let handshake = match tokio::time::timeout(
        Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
        ().serve(transport),
    )
    .await
    {
        Err(_) => {
            audit_failure(
                audit_dir,
                &manifest.name,
                url,
                handshake_phase,
                "initialize-timeout",
            );
            return Err(McpLaunchError::Handshake {
                name: manifest.name.clone(),
                message: format!("initialize did not complete within {HANDSHAKE_TIMEOUT_MS}ms"),
            });
        }
        Ok(Err(e)) => {
            audit_failure(
                audit_dir,
                &manifest.name,
                url,
                handshake_phase,
                "initialize-failed",
            );
            return Err(McpLaunchError::Handshake {
                name: manifest.name.clone(),
                message: format!("{e}"),
            });
        }
        Ok(Ok(h)) => h,
    };

    // Snapshot server info; rmcp `peer_info()` returns a borrow into
    // running-service state we don't want to keep alive elsewhere.
    let server_info: rmcp::model::ServerInfo = handshake.peer().peer_info().clone();

    // Protocol-version check: extract the serde string value and compare
    // exactly. Debug-string substring matching can falsely accept versions
    // that merely contain the supported version as text.
    let version = match protocol_version_string(&server_info.protocol_version) {
        Ok(version) => version,
        Err(version) => {
            let _ = handshake.cancel().await;
            audit_failure(
                audit_dir,
                &manifest.name,
                url,
                handshake_phase,
                "protocol-mismatch",
            );
            return Err(McpLaunchError::ProtocolMismatch {
                name: manifest.name.clone(),
                version,
            });
        }
    };
    if version != SUPPORTED_PROTOCOL_VERSION {
        let _ = handshake.cancel().await;
        audit_failure(
            audit_dir,
            &manifest.name,
            url,
            handshake_phase,
            "protocol-mismatch",
        );
        return Err(McpLaunchError::ProtocolMismatch {
            name: manifest.name.clone(),
            version,
        });
    }

    // ---------- first-use sanity probe ----------

    let list_phase = McpEgressPhase::ListTools;
    let probe = tokio::time::timeout(
        Duration::from_millis(FIRST_LIST_TIMEOUT_MS),
        handshake
            .peer()
            .list_tools(Some(PaginatedRequestParamInner { cursor: None })),
    )
    .await;

    match probe {
        Err(_) => {
            let _ = handshake.cancel().await;
            audit_failure(audit_dir, &manifest.name, url, list_phase, "list-timeout");
            return Err(McpLaunchError::Refused {
                name: manifest.name.clone(),
                message: format!(
                    "first tools/list did not return within {FIRST_LIST_TIMEOUT_MS}ms"
                ),
            });
        }
        Ok(Err(e)) => {
            let _ = handshake.cancel().await;
            audit_failure(audit_dir, &manifest.name, url, list_phase, "list-refused");
            return Err(McpLaunchError::Refused {
                name: manifest.name.clone(),
                message: format!("first tools/list refused: {e}"),
            });
        }
        Ok(Ok(_)) => {
            write_audit_row(
                audit_dir,
                McpEgressEvent::new(
                    now_rfc3339(),
                    &manifest.name,
                    url,
                    list_phase,
                    McpEgressOutcome::Success,
                    None,
                    None,
                ),
            );
        }
    }

    Ok(McpServerHandle::new(
        manifest.name.clone(),
        handshake,
        server_info,
    ))
}

fn protocol_version_string(version: &rmcp::model::ProtocolVersion) -> Result<String, String> {
    match serde_json::to_value(version) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(format!("{version:?}")),
    }
}

/// Best-effort HTTP status extractor for rmcp's `SseTransportError`. The
/// error type is opaque-by-design (non_exhaustive), but its `Reqwest`
/// variant wraps a `reqwest::Error` from which we can pull a status code
/// when it's available (i.e. the request reached the server and got a
/// response). Returns `None` for transport-level errors (DNS, TLS,
/// premature EOF) where no status code applies.
fn sse_error_status(err: &rmcp::transport::sse::SseTransportError) -> Option<u16> {
    use rmcp::transport::sse::SseTransportError;
    match err {
        SseTransportError::Reqwest(re) => re.status().map(|s| s.as_u16()),
        _ => None,
    }
}

/// Append a single [`McpEgressEvent`] to `<audit_dir>/audit.log`. Audit
/// I/O failures are logged via `tracing::warn!` and swallowed — the
/// launch must not fail just because the audit log is unwritable (same
/// posture as `append_subprocess_egress`).
fn write_audit_row(audit_dir: &Path, event: McpEgressEvent) {
    let path = audit_dir.join(AUDIT_LOG_FILE);
    if let Err(e) = append_mcp_egress(&path, &event) {
        tracing::warn!(
            target = "atelier::mcp::audit",
            error = %e,
            "append_mcp_egress failed for path={path:?}; row dropped",
        );
    }
}

/// Convenience wrapper for the failure case: build + append a single
/// `outcome: failure` row.
fn audit_failure(audit_dir: &Path, provider: &str, url: &str, phase: McpEgressPhase, reason: &str) {
    write_audit_row(
        audit_dir,
        McpEgressEvent::new(
            now_rfc3339(),
            provider,
            url,
            phase,
            McpEgressOutcome::Failure,
            Some(reason.to_string()),
            None,
        ),
    );
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_config::{SideEffectClass, Transport};
    use crate::sandbox::SandboxPolicy;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// Construct an `http`/`sse` manifest fixture.
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
            allowed_hosts: None,
            enabled: true,
        }
    }

    fn tmp_workspace() -> TempDir {
        TempDir::new().unwrap()
    }

    #[tokio::test]
    async fn rejects_stdio_transport() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut m = http_manifest(
            "fs",
            Transport::Http,
            "https://example.invalid/mcp",
            BTreeMap::new(),
            true,
        );
        // Sneak the wrong transport in to exercise the gate.
        m.transport = Transport::Stdio;
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::UnsupportedTransport { transport, .. } => {
                assert_eq!(transport, "stdio");
            }
            other => panic!("expected UnsupportedTransport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_allow_net_false_writes_blocked_audit_row() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let m = http_manifest(
            "search",
            Transport::Http,
            "https://example.invalid/mcp",
            BTreeMap::new(),
            false, // <-- the gate
        );
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::Refused { name, message } => {
                assert_eq!(name, "search");
                assert!(
                    message.contains("allow_net=true"),
                    "message should explain the gate, got {message:?}"
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        // Audit row landed.
        let log = std::fs::read_to_string(audit.join("audit.log")).expect("audit log written");
        let row: McpEgressEvent = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(row.kind, "mcp-http-request");
        assert_eq!(row.outcome, "blocked");
        assert_eq!(row.provider, "search");
        assert_eq!(row.reason.as_deref(), Some("allow_net=false"));
    }

    #[tokio::test]
    async fn rejects_missing_url() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut m = http_manifest(
            "search",
            Transport::Http,
            "https://example.invalid/mcp",
            BTreeMap::new(),
            true,
        );
        m.url = None;
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        assert!(
            matches!(err, McpLaunchError::InvalidManifest { .. }),
            "expected InvalidManifest, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_empty_url() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let m = http_manifest("search", Transport::Sse, "", BTreeMap::new(), true);
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        assert!(
            matches!(err, McpLaunchError::InvalidManifest { .. }),
            "expected InvalidManifest, got {err:?}"
        );
    }

    #[tokio::test]
    async fn missing_env_var_for_header_surfaces_typed_error() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".into(),
            "Bearer ${env:ATELIER_HTTP_LAUNCHER_TEST_DEFINITELY_UNSET}".into(),
        );
        // SAFETY: per-process env mutation, see `mcp_config::interpolate_*` tests.
        unsafe { std::env::remove_var("ATELIER_HTTP_LAUNCHER_TEST_DEFINITELY_UNSET") };
        let m = http_manifest(
            "search",
            Transport::Http,
            "https://example.invalid/mcp",
            headers,
            true,
        );
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::Interpolation { name, reason } => {
                assert_eq!(name, "search");
                assert!(reason.contains("Authorization"));
                assert!(reason.contains("ATELIER_HTTP_LAUNCHER_TEST_DEFINITELY_UNSET"));
            }
            other => panic!("expected Interpolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_header_name_surfaces_typed_error() {
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut headers = BTreeMap::new();
        // Spaces are not valid in a header name per RFC 7230.
        headers.insert("Invalid Header".into(), "value".into());
        let m = http_manifest(
            "search",
            Transport::Http,
            "https://example.invalid/mcp",
            headers,
            true,
        );
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::InvalidHeader { name, header, .. } => {
                assert_eq!(name, "search");
                assert_eq!(header, "Invalid Header");
            }
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_404_surfaces_as_http_status_with_audit_failure_row() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&mock)
            .await;
        let m = http_manifest("search", Transport::Sse, &mock.uri(), BTreeMap::new(), true);
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::HttpStatus { name, status } => {
                assert_eq!(name, "search");
                assert_eq!(status, 404);
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
        // Audit row should be a failure with reason="http-status".
        let log = std::fs::read_to_string(audit.join("audit.log")).expect("audit log");
        let row: McpEgressEvent = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(row.outcome, "failure");
        assert_eq!(row.phase, "handshake");
        assert_eq!(row.reason.as_deref(), Some("http-status"));
    }

    #[tokio::test]
    async fn http_500_surfaces_as_http_status() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&mock)
            .await;
        let m = http_manifest(
            "search",
            Transport::Http,
            &mock.uri(),
            BTreeMap::new(),
            true,
        );
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::HttpStatus { status, .. } => assert_eq!(status, 500),
            other => panic!("expected HttpStatus(500), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_content_type_surfaces_as_sse_stream_error() {
        // Wiremock returns 200 with content-type: text/plain (NOT
        // text/event-stream). rmcp's SseTransport bails before any
        // event arrives.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("hello"),
            )
            .mount(&mock)
            .await;
        let m = http_manifest("search", Transport::Sse, &mock.uri(), BTreeMap::new(), true);
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        match err {
            McpLaunchError::SseStream { name, .. } => {
                assert_eq!(name, "search");
            }
            // Some content-type mismatches surface via the status path;
            // accept either as a transport-layer rejection.
            McpLaunchError::HttpStatus { .. } => {}
            other => panic!("expected SseStream or HttpStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dns_failure_surfaces_as_sse_stream_error() {
        // A reserved invalid TLD — RFC 6761 reserves `.invalid` for
        // exactly this purpose. DNS resolution must fail, not redirect.
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let m = http_manifest(
            "search",
            Transport::Http,
            "http://atelier-mcp-spike-nonexistent.invalid/mcp",
            BTreeMap::new(),
            true,
        );
        let audit = ws.path().join("audit");
        let err = tokio::time::timeout(
            Duration::from_secs(15),
            launch_http_server(&m, &policy, &audit),
        )
        .await
        .expect("launch should not exceed 15s wall-clock")
        .unwrap_err();
        match err {
            McpLaunchError::SseStream { name, .. } => assert_eq!(name, "search"),
            McpLaunchError::Handshake { .. } => {}
            other => panic!("expected SseStream / Handshake, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn header_value_never_appears_in_error_string() {
        // Defence in depth: if a user puts a literal secret in a header
        // value AND the value is invalid (e.g. has a CR/LF), the
        // resulting error message must not echo the value back.
        let ws = tmp_workspace();
        let policy = SandboxPolicy::restrictive(ws.path()).unwrap();
        let mut headers = BTreeMap::new();
        // CR + LF in a header value is rejected by reqwest. The literal
        // is also distinctive enough that an accidental echo would be
        // easy to assert against.
        let secret_value = "Bearer SECRET-SHIBBOLETH-VALUE\r\nEvil-Injection: yes";
        headers.insert("Authorization".into(), secret_value.into());
        let m = http_manifest(
            "search",
            Transport::Http,
            "https://example.invalid/mcp",
            headers,
            true,
        );
        let audit = ws.path().join("audit");
        let err = launch_http_server(&m, &policy, &audit).await.unwrap_err();
        let rendered = format!("{err}");
        assert!(
            !rendered.contains("SHIBBOLETH"),
            "error string must not echo the header value, got: {rendered}"
        );
    }

    #[test]
    fn audit_helper_writes_one_line() {
        let dir = tempfile::TempDir::new().unwrap();
        let audit = dir.path();
        write_audit_row(
            audit,
            McpEgressEvent::new(
                "2026-05-17T10:00:00Z",
                "x",
                "https://example.invalid/mcp",
                McpEgressPhase::Handshake,
                McpEgressOutcome::Success,
                None,
                None,
            ),
        );
        let body = std::fs::read_to_string(audit.join("audit.log")).unwrap();
        assert_eq!(body.lines().count(), 1);
        let row: McpEgressEvent = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(row.provider, "x");
    }

    #[test]
    fn protocol_version_string_uses_exact_serde_value() {
        let supported = rmcp::model::ProtocolVersion::V_2024_11_05;
        assert_eq!(
            protocol_version_string(&supported).unwrap(),
            SUPPORTED_PROTOCOL_VERSION
        );

        let unsupported: rmcp::model::ProtocolVersion =
            serde_json::from_value(serde_json::Value::String("prefix-2024-11-05-suffix".into()))
                .expect("rmcp accepts unknown protocol versions");
        assert_eq!(
            protocol_version_string(&unsupported).unwrap(),
            "prefix-2024-11-05-suffix"
        );
        assert_ne!(
            protocol_version_string(&unsupported).unwrap(),
            SUPPORTED_PROTOCOL_VERSION
        );
    }
}
