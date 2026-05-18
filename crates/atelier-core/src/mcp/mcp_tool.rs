//! §15 MCP tool wrapper.
//!
//! Adapts a single MCP-server-advertised tool ([`crate::mcp::McpTool`])
//! to the [`crate::dispatcher::Tool`] trait, so the §15 dispatcher can
//! route a `ToolCallRequest` onto it transparently alongside the built-in
//! tools. One [`McpToolWrapper`] per advertised tool; multiple wrappers
//! from the same server share an [`Arc<McpServerHandle>`].
//!
//! Spec §15 — *"Built-in tools (file ops, shell, search) and MCP-routed
//! tools share the same `ToolDispatching → ToolExecuting` state
//! transitions. The loop does not branch on tool origin."* This module
//! is the seam that makes that promise concrete.
//!
//! Error mapping (`rmcp::ServiceError` / [`crate::mcp::McpLaunchError`]
//! → [`crate::error::ToolError`]):
//!
//! | rmcp / launcher                        | ToolError                  |
//! |----------------------------------------|----------------------------|
//! | `Refused { … }` (transport/wire error) | `McpServerCrashed`         |
//! | `ChildExited { … }`                    | `McpServerUnreachable`     |
//! | any other launcher variant             | `ExecutionFailed`          |
//! | `CallToolResult { is_error: true, …}`  | `ExecutionFailed`          |
//!
//! `validate_args` consults the tool's advertised `input_schema` and
//! rejects with `SchemaViolation` on shape mismatch. Built-in tools
//! lean on serde-`deny_unknown_fields` for the same effect; MCP tools
//! get the explicit JSONSchema route because their schemas are
//! server-supplied and may express constraints serde can't.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use jsonschema::Validator;
use serde_json::Value;

use crate::audit::{append_mcp_egress, McpEgressEvent, McpEgressOutcome, McpEgressPhase};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;
use crate::mcp::stdio_launcher::McpServerHandle;
use crate::mcp::McpLaunchError;
use crate::time::now_rfc3339;

/// Tool impl that routes a single MCP-server-advertised tool through
/// an [`McpServerHandle`]. Construct one per `(server, tool)` pair via
/// [`Self::new`]; register with [`crate::dispatcher::ToolRegistry`]
/// alongside built-in tools.
///
/// The wrapper keeps an `Arc<McpServerHandle>` rather than a raw
/// reference so multiple wrappers from the same server can share the
/// underlying rmcp running-service for the session's lifetime. The
/// handle itself is internally async — `call_tool` takes `&self` and
/// is safe to invoke concurrently across wrappers.
pub struct McpToolWrapper {
    /// Originating server name (matches the manifest).  Used for
    /// `ToolError::McpServer*` attribution + the §11 audit log row.
    server_name: String,
    /// Tool name as advertised by the server.  This is the value the
    /// dispatcher will key on; it must be unique across the whole
    /// registry (so two servers exposing a same-named tool must be
    /// surfaced separately by the caller — typical convention is to
    /// prefix with the server name, but the policy lives in
    /// `register_mcp_servers`, not here).
    tool_name: String,
    /// Server-supplied description.  Surfaced to the model via the
    /// adapter `ToolSpec` map (built elsewhere).
    description: String,
    /// Server-supplied JSON Schema for the tool's arguments.  Used by
    /// [`Self::validate_args`] to fail-fast on shape mismatch.
    input_schema: Value,
    /// Pre-compiled validator over `input_schema`.  Compiled once at
    /// construction; a malformed schema is reported as `Err` so the
    /// register helper can skip the tool with a typed warning.  We
    /// store an `Arc<Validator>` for cheap clones across the
    /// dispatcher's handler arc — `Validator` is not `Clone`.
    validator: Arc<Validator>,
    /// Shared rmcp handle for the originating server.
    handle: Arc<McpServerHandle>,
    /// §8 trust-budget classification.  Comes from the server
    /// manifest's per-server default (per-tool override is a tool-
    /// manifest concern and lands when we wire the manifest in).
    side_effect_class: SideEffectClass,
    /// v60.28 H5 / H6 — egress config for http/sse servers. `None` for
    /// stdio servers (no URL, no egress audit). When `Some`, every
    /// `call_tool` checks the URL host against `allowed_hosts` and
    /// appends an `McpEgressEvent` row to `<audit_dir>/audit.log`.
    egress: Option<EgressContext>,
}

#[derive(Debug, Clone)]
struct EgressContext {
    url: String,
    allowed_hosts: Vec<String>,
    audit_dir: PathBuf,
}

impl McpToolWrapper {
    /// Construct a wrapper.  Compiles the `input_schema` once; an
    /// invalid schema is returned as `Err` so the caller can skip the
    /// tool with a warning rather than tripping at first dispatch.
    pub fn new(
        server_name: impl Into<String>,
        tool_name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handle: Arc<McpServerHandle>,
        side_effect_class: SideEffectClass,
    ) -> Result<Self, String> {
        let validator = compile_input_schema(&input_schema)?;
        Ok(Self {
            server_name: server_name.into(),
            tool_name: tool_name.into(),
            description: description.into(),
            input_schema,
            validator: Arc::new(validator),
            handle,
            side_effect_class,
            egress: None,
        })
    }

    /// v60.28 H5 / H6 — opt into per-`call_tool` host allowlist enforcement
    /// plus an `McpEgressEvent` row written through the existing audit
    /// appender. Stdio servers should not call this; the launcher passes
    /// the resolved URL, allowlist (defaulting to `[host(url)]`), and
    /// audit dir for http/sse servers.
    pub fn with_egress_audit(
        mut self,
        url: impl Into<String>,
        allowed_hosts: Vec<String>,
        audit_dir: PathBuf,
    ) -> Self {
        self.egress = Some(EgressContext {
            url: url.into(),
            allowed_hosts,
            audit_dir,
        });
        self
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

/// Compile an MCP-advertised `input_schema` value to a
/// [`jsonschema::Validator`].  Pulled out of [`McpToolWrapper::new`]
/// so unit tests can exercise the validation surface without a live
/// `McpServerHandle`.
pub(crate) fn compile_input_schema(input_schema: &Value) -> Result<Validator, String> {
    jsonschema::validator_for(input_schema)
        .map_err(|e| format!("input_schema is not a valid JSON Schema: {e}"))
}

/// Validate `args` against a compiled validator.  Pure function so
/// tests can drive the surface without instantiating a wrapper.
///
/// `validator.iter_errors` returns the full sequence of failures so a
/// single SchemaViolation surfaces every offending field rather than
/// the first one.  Stops at the first error if there's exactly one —
/// no allocation cost.
pub(crate) fn validate_args_against(validator: &Validator, args: &Value) -> Result<(), String> {
    let errs: Vec<String> = validator.iter_errors(args).map(|e| e.to_string()).collect();
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn side_effect_class(&self) -> SideEffectClass {
        self.side_effect_class
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn validate_args(&self, args: &Value) -> Result<(), String> {
        validate_args_against(&self.validator, args)
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        // MCP wants the args as an object (or absent).  Anything else
        // is a schema violation — but `validate_args` should have
        // caught it; we defend in depth here so a future caller that
        // bypasses validation still fails closed.
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(ToolError::SchemaViolation {
                    tool: self.tool_name.clone(),
                    error: format!(
                        "MCP tools expect an object or null `arguments`, got {}",
                        match other {
                            Value::Array(_) => "array",
                            Value::String(_) => "string",
                            Value::Number(_) => "number",
                            Value::Bool(_) => "boolean",
                            _ => "unknown",
                        }
                    ),
                });
            }
        };

        // v60.28 H5 — enforce the per-server allowed_hosts before egress.
        if let Some(eg) = &self.egress {
            if let Some(host) = host_of_url(&eg.url) {
                if !eg.allowed_hosts.iter().any(|h| h == &host) {
                    write_call_tool_audit(
                        &eg.audit_dir,
                        &self.server_name,
                        &eg.url,
                        &self.tool_name,
                        McpEgressOutcome::Blocked,
                        Some(format!("host {host:?} not in allowed_hosts")),
                    );
                    return Err(map_launch_error(
                        &self.server_name,
                        &self.tool_name,
                        McpLaunchError::HostNotAllowed {
                            name: self.server_name.clone(),
                            host,
                        },
                    ));
                }
            }
        }

        let result = self
            .handle
            .call_tool(self.tool_name.clone(), arguments)
            .await
            .map_err(|e| {
                if let Some(eg) = &self.egress {
                    write_call_tool_audit(
                        &eg.audit_dir,
                        &self.server_name,
                        &eg.url,
                        &self.tool_name,
                        McpEgressOutcome::Failure,
                        Some(format!("{e}")),
                    );
                }
                map_launch_error(&self.server_name, &self.tool_name, e)
            })?;

        if let Some(eg) = &self.egress {
            write_call_tool_audit(
                &eg.audit_dir,
                &self.server_name,
                &eg.url,
                &self.tool_name,
                McpEgressOutcome::Success,
                None,
            );
        }

        if result.is_error == Some(true) {
            // Server-side tool error.  Stringify the content blocks
            // we can recognise (text) and pass them through; binary
            // content is summarised by length.  Maps onto
            // `ExecutionFailed`, which the §2.5 state machine routes
            // to `Retry`.
            let stderr = stringify_content(&result.content);
            return Err(ToolError::ExecutionFailed {
                tool: self.tool_name.clone(),
                exit_code: -1,
                stderr,
            });
        }

        // Success: project the rmcp `CallToolResult` onto a
        // `ToolResult` whose `output` is the JSON serialisation of
        // the result.  MCP tools don't go through §3 staging, so
        // `staged_writes` is always `None` — any file changes a
        // remote tool makes happen on the server's side of the
        // boundary and aren't visible to the harness as staged
        // hunks.
        let output = serde_json::to_value(&result).map_err(|e| ToolError::ResultMalformed {
            tool: self.tool_name.clone(),
            parse_error: format!("serialising CallToolResult: {e}"),
        })?;
        Ok(ToolResult {
            output,
            staged_writes: None,
        })
    }
}

/// v60.28 H5 — parse the host portion of a URL string. Returns lowercase
/// host without port. Tolerant of `scheme://host[:port]/path` shapes; no
/// dependency on the `url` crate.
pub(crate) fn host_of_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('.');
    // Strip optional `user@` prefix.
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    // Strip `:port` suffix; defend against IPv6 brackets.
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        // `[ipv6]:port` form — keep through the closing bracket.
        stripped.split_once(']').map(|(h, _)| h).unwrap_or(stripped)
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// v60.28 H6 — append one `call-tool` audit row through the existing
/// `append_mcp_egress` appender. Logs (but does not propagate) I/O
/// failures: an unwritable audit log must not silently block the call
/// or break the dispatcher.
fn write_call_tool_audit(
    audit_dir: &std::path::Path,
    server_name: &str,
    url: &str,
    tool_name: &str,
    outcome: McpEgressOutcome,
    reason: Option<String>,
) {
    let event = McpEgressEvent::new(
        now_rfc3339(),
        server_name,
        url,
        McpEgressPhase::CallTool,
        outcome,
        reason,
        Some(tool_name.to_string()),
    );
    let path = audit_dir.join("audit.log");
    if let Err(e) = append_mcp_egress(&path, &event) {
        tracing::warn!(
            target = "atelier::mcp::audit",
            error = %e,
            "append_mcp_egress call_tool row dropped for path={path:?}",
        );
    }
}

/// Map a launcher / transport error to the typed [`ToolError`] surface
/// the dispatcher expects.  Centralised so a future variant in either
/// enum gets a single review touch.
fn map_launch_error(server_name: &str, tool_name: &str, e: McpLaunchError) -> ToolError {
    match e {
        // The launcher's `call_tool` wraps every rmcp transport
        // failure (including method-not-found, server panicked
        // between calls, sandbox refused egress) into `Refused`.
        // Map onto McpServerCrashed so the §2.5 RetryBudgeted path
        // applies — three retries against the same broken server
        // and we escalate to AwaitUser.
        McpLaunchError::Refused { name, message } => ToolError::McpServerCrashed {
            server: name,
            last_message: Some(message),
        },
        McpLaunchError::ChildExited { name, .. } => {
            ToolError::McpServerUnreachable { server: name }
        }
        McpLaunchError::Handshake { name, message } => ToolError::McpServerCrashed {
            server: name,
            last_message: Some(message),
        },
        // The launcher's pre-flight variants shouldn't reach here at
        // call_tool time (they'd have failed at launch), but if a
        // future launcher refactor changes that, map them onto an
        // ExecutionFailed so we don't drop the detail.
        other => ToolError::ExecutionFailed {
            tool: format!("{server_name}::{tool_name}"),
            exit_code: -1,
            stderr: format!("{other}"),
        },
    }
}

/// Render an rmcp `CallToolResult.content` vector as a flat string for
/// stderr-style error surfacing.  Text content rides through verbatim;
/// images / embedded resources are summarised so we never inline a
/// multi-MB base64 blob into a `ToolError` payload.
fn stringify_content(content: &[rmcp::model::Content]) -> String {
    use rmcp::model::RawContent;
    let mut out = String::new();
    for (i, c) in content.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match &c.raw {
            RawContent::Text(t) => out.push_str(&t.text),
            RawContent::Image(img) => {
                out.push_str(&format!(
                    "<image: {} bytes (base64), mime={}>",
                    img.data.len(),
                    img.mime_type
                ));
            }
            RawContent::Resource(_) => {
                out.push_str("<embedded resource>");
            }
        }
    }
    out
}

// ---------- tests ----------
//
// The launcher's `McpServerHandle` owns a live `rmcp::RunningService`
// that's not constructible without a real subprocess — so our unit
// tests exercise the wrapper's pure surfaces (validate_args, error
// mapping, side_effect_class) and leave the round-trip integration
// to `crates/atelier-cli/tests/mcp_integration.rs`.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The launcher's `McpServerHandle` owns a live `rmcp::RunningService`
    // that can't be constructed without a real subprocess + handshake,
    // so the `execute` path (which takes `self: &McpToolWrapper`) is
    // exercised by the integration test in
    // `crates/atelier-cli/tests/mcp_integration.rs`.  Here we exercise:
    //
    //   * `compile_input_schema` — pure: schema in, validator out.
    //   * `validate_args_against` — pure: validator + value → result.
    //   * `map_launch_error` — pure: launcher error → ToolError.
    //   * `stringify_content` — pure: rmcp Content blocks → string.
    //
    // Together they cover the bug-prone seams (schema mis-compile,
    // wrong arg shape, transport-error mapping, content rendering)
    // while keeping the unit tests handle-free.

    /// We exercise `map_launch_error` directly — it's a pure
    /// function that takes the launcher's typed error and returns
    /// the dispatcher's typed error.
    #[test]
    fn launch_refused_maps_to_mcp_server_crashed() {
        let e = map_launch_error(
            "fs",
            "list_directory",
            McpLaunchError::Refused {
                name: "fs".into(),
                message: "method not found".into(),
            },
        );
        match e {
            ToolError::McpServerCrashed {
                server,
                last_message,
            } => {
                assert_eq!(server, "fs");
                assert_eq!(last_message.as_deref(), Some("method not found"));
            }
            other => panic!("expected McpServerCrashed, got {other:?}"),
        }
    }

    #[test]
    fn launch_child_exited_maps_to_mcp_server_unreachable() {
        let e = map_launch_error(
            "fs",
            "list_directory",
            McpLaunchError::ChildExited {
                name: "fs".into(),
                code: Some(1),
            },
        );
        assert!(matches!(
            e,
            ToolError::McpServerUnreachable { ref server } if server == "fs"
        ));
    }

    #[test]
    fn launch_handshake_maps_to_mcp_server_crashed() {
        let e = map_launch_error(
            "fs",
            "list_directory",
            McpLaunchError::Handshake {
                name: "fs".into(),
                message: "timeout".into(),
            },
        );
        match e {
            ToolError::McpServerCrashed {
                server,
                last_message,
            } => {
                assert_eq!(server, "fs");
                assert!(last_message.unwrap().contains("timeout"));
            }
            other => panic!("expected McpServerCrashed, got {other:?}"),
        }
    }

    #[test]
    fn launch_config_error_maps_to_execution_failed() {
        // UnsupportedTransport is a config error; if it somehow reaches
        // `map_launch_error` (impossible in production today; defensive
        // mapping for forward-compat) it must surface as ExecutionFailed
        // rather than the McpServer* variants so the dispatcher doesn't
        // mis-classify it as a transient failure.
        let e = map_launch_error(
            "fs",
            "list_directory",
            McpLaunchError::UnsupportedTransport {
                name: "fs".into(),
                transport: "sse".into(),
            },
        );
        match e {
            ToolError::ExecutionFailed { tool, .. } => {
                assert_eq!(tool, "fs::list_directory");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[test]
    fn stringify_content_renders_text_blocks() {
        use rmcp::model::{Annotated, RawContent, RawTextContent};
        let blocks = vec![
            Annotated::new(
                RawContent::Text(RawTextContent {
                    text: "hello".into(),
                }),
                None,
            ),
            Annotated::new(
                RawContent::Text(RawTextContent {
                    text: "world".into(),
                }),
                None,
            ),
        ];
        assert_eq!(stringify_content(&blocks), "hello\nworld");
    }

    #[test]
    fn stringify_content_summarises_image_blocks() {
        use rmcp::model::{Annotated, RawContent, RawImageContent};
        let blocks = vec![Annotated::new(
            RawContent::Image(RawImageContent {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            }),
            None,
        )];
        let s = stringify_content(&blocks);
        assert!(s.contains("image"));
        assert!(s.contains("image/png"));
    }

    #[test]
    fn compile_input_schema_accepts_valid_object_schema() {
        // The shape MCP servers typically advertise — `type: object`
        // with `properties` — must round-trip cleanly.
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });
        let v = compile_input_schema(&schema).expect("valid schema must compile");
        // Valid input passes …
        let good = json!({ "path": "/tmp/x" });
        assert!(validate_args_against(&v, &good).is_ok());
        // … and the `required` field is enforced.
        let bad = json!({});
        let err = validate_args_against(&v, &bad).expect_err("missing required must fail");
        assert!(
            err.contains("path") || err.contains("required"),
            "got {err:?}"
        );
    }

    #[test]
    fn compile_input_schema_rejects_bogus_schema() {
        // jsonschema rejects a schema whose `type` is not recognised.
        let schema = json!({ "type": "not-a-real-type" });
        let err = compile_input_schema(&schema).expect_err("bogus type must reject");
        assert!(
            err.contains("not a valid JSON Schema"),
            "expected the wrapper's error wording, got {err:?}"
        );
    }

    #[test]
    fn validate_args_rejects_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } },
            "required": ["count"]
        });
        let v = compile_input_schema(&schema).unwrap();
        // String when integer is required.
        let bad = json!({ "count": "five" });
        let err = validate_args_against(&v, &bad).expect_err("type mismatch must fail");
        assert!(!err.is_empty());
    }

    // ---------- v60.28 H5 host parsing ----------

    #[test]
    fn host_of_url_strips_scheme_port_path() {
        assert_eq!(
            host_of_url("https://search.example.com/mcp?x=1"),
            Some("search.example.com".into())
        );
        assert_eq!(
            host_of_url("http://127.0.0.1:8080/mcp"),
            Some("127.0.0.1".into())
        );
        assert_eq!(
            host_of_url("https://user:pass@host.example/"),
            Some("host.example".into())
        );
        assert_eq!(host_of_url("https://[::1]:9000/mcp"), Some("::1".into()));
    }

    #[test]
    fn host_of_url_lowercases() {
        assert_eq!(
            host_of_url("https://Search.Example.COM/mcp"),
            Some("search.example.com".into())
        );
    }

    // ---------- v60.28 H6 audit row shape ----------

    #[test]
    fn write_call_tool_audit_emits_one_ndjson_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let audit = dir.path();
        write_call_tool_audit(
            audit,
            "search",
            "https://search.example/mcp",
            "list_index",
            McpEgressOutcome::Success,
            None,
        );
        let body = std::fs::read_to_string(audit.join("audit.log")).expect("audit row landed");
        let row: McpEgressEvent = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(row.kind, "mcp-http-request");
        assert_eq!(row.provider, "search");
        assert_eq!(row.phase, "call-tool");
        assert_eq!(row.outcome, "success");
        assert_eq!(row.tool_name.as_deref(), Some("list_index"));
    }

    #[test]
    fn write_call_tool_audit_records_blocked_host() {
        let dir = tempfile::TempDir::new().unwrap();
        let audit = dir.path();
        write_call_tool_audit(
            audit,
            "search",
            "https://evil.example/mcp",
            "list_index",
            McpEgressOutcome::Blocked,
            Some("host \"evil.example\" not in allowed_hosts".into()),
        );
        let body = std::fs::read_to_string(audit.join("audit.log")).expect("audit row landed");
        let row: McpEgressEvent = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(row.outcome, "blocked");
        assert!(row.reason.unwrap().contains("evil.example"));
    }
}
