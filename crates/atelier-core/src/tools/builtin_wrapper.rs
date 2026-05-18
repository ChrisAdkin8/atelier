//! §15 built-in tool wrapper.
//!
//! Sibling of [`crate::mcp::mcp_tool::McpToolWrapper`]. Both wrappers
//! present the same [`crate::dispatcher::Tool`] surface to the
//! dispatcher: name + side-effect class + JSONSchema-validated args +
//! delegated execution. The dispatcher (and the §2.5 state machine that
//! drives it) does not branch on tool origin — the spec §15 promise.
//!
//! What the wrapper does that a bare `Tool` impl doesn't:
//!
//!   1. **Manifest is source of truth.** `name`, `description`,
//!      `side_effect_class` and `input_schema` come from the bundled
//!      `crates/atelier-core/tools/*.v1.json` manifest, not from the
//!      Rust impl. A manifest/impl drift caught at startup is better
//!      than silent disagreement at dispatch time.
//!   2. **JSONSchema-level argument validation.** The inner impl's
//!      `serde(deny_unknown_fields)` deserialization catches *most*
//!      shape errors, but it can't express patterns, ranges, or
//!      `oneOf` constraints that the manifest does. The wrapper runs
//!      `jsonschema::Validator::iter_errors` on the raw `Value` before
//!      handing it to the inner impl.
//!
//! Construction is via [`BuiltInToolWrapper::from_manifest_json`],
//! which takes the manifest JSON string (loaded by the caller via
//! `include_str!`) and an `Arc<dyn Tool>` that supplies the actual
//! execution. The wrapper's `name()` returns the manifest-declared
//! name, NOT the inner impl's — those should agree (and a startup
//! check verifies it), but the manifest wins in the rare case they
//! drift, so the dispatcher routes by the surface the model sees.

use std::sync::Arc;

use async_trait::async_trait;
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::Value;

use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;
use crate::mcp::mcp_tool::{compile_input_schema, validate_args_against};

/// Minimal projection of the bundled `tool_manifest.v1.json` shape —
/// only the fields the wrapper needs to behave correctly. The
/// `output_schema` and `implementation` fields exist in the on-disk
/// manifest but the wrapper doesn't consume them.
#[derive(Debug, Clone, Deserialize)]
struct ManifestProjection {
    name: String,
    description: String,
    side_effect_class: SideEffectClass,
    input_schema: Value,
    /// v60.29 H9 — per-tool wall-clock deadline in milliseconds.
    /// Optional; absence inherits `DEFAULT_TOOL_DEADLINE`. Threaded
    /// onto `Tool::deadline_override` so the dispatcher's
    /// `tokio::select!` picks it up.
    #[serde(default)]
    deadline_ms: Option<u64>,
}

/// Adapter that routes a built-in tool's execution through a wrapper
/// whose metadata is sourced from the bundled manifest. Construct one
/// per built-in tool via [`Self::from_manifest_json`]; register the
/// resulting `Arc<dyn Tool>` into [`crate::dispatcher::ToolRegistry`]
/// alongside [`crate::mcp::mcp_tool::McpToolWrapper`]s.
///
/// Hand-rolled `Debug` (`Arc<dyn Tool>` isn't `Debug`) — surfaces the
/// fields a human cares about at a glance and skips the inner.
pub struct BuiltInToolWrapper {
    name: String,
    description: String,
    side_effect_class: SideEffectClass,
    input_schema: Value,
    validator: Arc<Validator>,
    /// v60.29 H9 — per-tool deadline override from the bundled
    /// manifest's `deadline_ms` field. `None` means inherit the
    /// runner default.
    deadline: Option<std::time::Duration>,
    /// The inner `Tool` impl that supplies the actual execution.
    /// Stored as `Arc<dyn Tool>` so a future test can swap it for a
    /// mock without changing the wrapper.
    inner: Arc<dyn Tool>,
}

impl BuiltInToolWrapper {
    /// Build a wrapper from the raw manifest JSON + an inner `Tool`
    /// impl. Fails if the manifest doesn't parse, its `input_schema`
    /// doesn't compile to a [`jsonschema::Validator`], or the inner
    /// impl's `name()` disagrees with the manifest's `name`.
    pub fn from_manifest_json(
        manifest_json: &str,
        inner: Arc<dyn Tool>,
    ) -> Result<Self, BuiltInWrapError> {
        let parsed: ManifestProjection = serde_json::from_str(manifest_json)
            .map_err(|e| BuiltInWrapError::ManifestParse(e.to_string()))?;

        if parsed.name != inner.name() {
            return Err(BuiltInWrapError::NameMismatch {
                manifest: parsed.name,
                inner: inner.name().to_string(),
            });
        }

        let validator =
            compile_input_schema(&parsed.input_schema).map_err(BuiltInWrapError::SchemaCompile)?;

        Ok(Self {
            name: parsed.name,
            description: parsed.description,
            side_effect_class: parsed.side_effect_class,
            input_schema: parsed.input_schema,
            validator: Arc::new(validator),
            deadline: parsed.deadline_ms.map(std::time::Duration::from_millis),
            inner,
        })
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

impl std::fmt::Debug for BuiltInToolWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltInToolWrapper")
            .field("name", &self.name)
            .field("side_effect_class", &self.side_effect_class)
            .finish_non_exhaustive()
    }
}

/// Construction-time failure modes for [`BuiltInToolWrapper`]. All
/// are programmer errors (a missing field, a malformed schema, a
/// manifest/impl name drift) — at runtime they surface as a
/// `RunError::Config` from the registry builder, not as a tool
/// dispatch error.
#[derive(Debug, thiserror::Error)]
pub enum BuiltInWrapError {
    #[error("manifest does not parse as tool_manifest.v1.json: {0}")]
    ManifestParse(String),

    #[error("manifest input_schema does not compile to a JSONSchema validator: {0}")]
    SchemaCompile(String),

    #[error(
        "manifest name {manifest:?} does not match inner Tool::name() {inner:?} — \
         drift between the bundled manifest and the Rust impl"
    )]
    NameMismatch { manifest: String, inner: String },
}

#[async_trait]
impl Tool for BuiltInToolWrapper {
    fn name(&self) -> &str {
        &self.name
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
        // First gate: manifest JSONSchema (catches patterns / ranges /
        // oneOf / additionalProperties:false that serde can't always
        // express).
        validate_args_against(&self.validator, args)?;
        // Second gate: delegate to the inner impl so a custom
        // override (none of the built-ins use one today, but the
        // trait permits it) still runs.
        self.inner.validate_args(args)
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        self.inner.execute(args, ctx).await
    }

    fn deadline_override(&self) -> Option<std::time::Duration> {
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::{SideEffectClass, ToolContext};
    use crate::error::ToolError;
    use crate::sandbox::SandboxPolicy;
    use serde_json::json;

    /// A bare-bones inner tool we drive directly from tests. Returns
    /// the args it received as `output` so we can assert the wrapper
    /// passes them through unchanged.
    struct EchoTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            self.name
        }
        fn side_effect_class(&self) -> SideEffectClass {
            // Different on purpose from manifest values below — proves
            // the wrapper uses the manifest's class, not the inner's.
            SideEffectClass::SharedState
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                output: args,
                staged_writes: None,
            })
        }
    }

    fn manifest_for(name: &str, sec: &str) -> String {
        json!({
            "version": 1,
            "name": name,
            "description": format!("{name} description"),
            "side_effect_class": sec,
            "input_schema": {
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"}
                }
            }
        })
        .to_string()
    }

    #[test]
    fn name_comes_from_manifest_not_inner() {
        let m = manifest_for("read_file", "local-safe");
        let w =
            BuiltInToolWrapper::from_manifest_json(&m, Arc::new(EchoTool { name: "read_file" }))
                .unwrap();
        assert_eq!(w.name(), "read_file");
        assert_eq!(w.description(), "read_file description");
    }

    #[test]
    fn side_effect_class_comes_from_manifest_not_inner() {
        // Manifest says local-safe, inner echo says SharedState. The
        // wrapper must report local-safe — the manifest is the source
        // of truth.
        let m = manifest_for("read_file", "local-safe");
        let w =
            BuiltInToolWrapper::from_manifest_json(&m, Arc::new(EchoTool { name: "read_file" }))
                .unwrap();
        assert_eq!(w.side_effect_class(), SideEffectClass::LocalSafe);
    }

    #[test]
    fn name_mismatch_rejected() {
        let m = manifest_for("read_file", "local-safe");
        let err = BuiltInToolWrapper::from_manifest_json(
            &m,
            Arc::new(EchoTool {
                name: "something_else",
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, BuiltInWrapError::NameMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn malformed_manifest_rejected() {
        let err = BuiltInToolWrapper::from_manifest_json(
            r#"{"not": "a manifest"}"#,
            Arc::new(EchoTool { name: "x" }),
        )
        .unwrap_err();
        assert!(matches!(err, BuiltInWrapError::ManifestParse(_)));
    }

    #[test]
    fn invalid_schema_rejected() {
        let m = json!({
            "version": 1,
            "name": "x",
            "description": "x",
            "side_effect_class": "local-safe",
            "input_schema": { "type": "not-a-real-type" }
        })
        .to_string();
        let err = BuiltInToolWrapper::from_manifest_json(&m, Arc::new(EchoTool { name: "x" }))
            .unwrap_err();
        assert!(matches!(err, BuiltInWrapError::SchemaCompile(_)));
    }

    #[test]
    fn validate_args_runs_manifest_schema() {
        // The manifest requires `path` and forbids extra props. Both
        // failure modes must reject without going to the inner impl.
        let m = manifest_for("read_file", "local-safe");
        let w =
            BuiltInToolWrapper::from_manifest_json(&m, Arc::new(EchoTool { name: "read_file" }))
                .unwrap();

        // Missing required field.
        let err1 = w.validate_args(&json!({})).unwrap_err();
        assert!(
            err1.contains("path") || err1.contains("required"),
            "got {err1:?}"
        );

        // Extra field forbidden by additionalProperties:false.
        let err2 = w
            .validate_args(&json!({"path": "a.txt", "bogus": 1}))
            .unwrap_err();
        assert!(
            err2.contains("bogus") || err2.contains("additional"),
            "got {err2:?}"
        );

        // Wrong type for declared field.
        let err3 = w.validate_args(&json!({"path": 42})).unwrap_err();
        assert!(!err3.is_empty());

        // Happy path.
        assert!(w.validate_args(&json!({"path": "a.txt"})).is_ok());
    }

    #[tokio::test]
    async fn execute_delegates_to_inner() {
        let m = manifest_for("read_file", "local-safe");
        let w =
            BuiltInToolWrapper::from_manifest_json(&m, Arc::new(EchoTool { name: "read_file" }))
                .unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let sandbox = SandboxPolicy::restrictive(dir.path()).unwrap();
        let ctx = ToolContext {
            workspace_root: dir.path(),
            sandbox: &sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
        };
        let r = w
            .execute(json!({"path": "a.txt"}), &ctx)
            .await
            .expect("execute");
        // EchoTool returns its args verbatim.
        assert_eq!(r.output, json!({"path": "a.txt"}));
    }

    /// Every bundled manifest must round-trip through the wrapper —
    /// catches drift between the manifest set on disk and the
    /// `register_builtins` map (and rejects any manifest whose
    /// `input_schema` jsonschema 0.x can't compile).
    #[test]
    fn all_bundled_manifests_parse() {
        // The manifest set as of v60.13 — kept in lockstep with
        // `register_builtins` below.
        let manifests = [
            ("read_file", include_str!("../../tools/read_file.v1.json")),
            ("list_dir", include_str!("../../tools/list_dir.v1.json")),
            ("grep", include_str!("../../tools/grep.v1.json")),
            ("write_file", include_str!("../../tools/write_file.v1.json")),
            ("edit_file", include_str!("../../tools/edit_file.v1.json")),
            ("ast_grep", include_str!("../../tools/ast_grep.v1.json")),
            ("shell", include_str!("../../tools/shell.v1.json")),
        ];
        for (expected_name, body) in manifests {
            let parsed: ManifestProjection = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("manifest {expected_name} parse: {e}"));
            assert_eq!(parsed.name, expected_name);
            compile_input_schema(&parsed.input_schema)
                .unwrap_or_else(|e| panic!("manifest {expected_name} schema: {e}"));
        }
    }
}
