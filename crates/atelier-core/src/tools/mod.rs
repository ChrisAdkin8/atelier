//! §15 built-in tools.
//!
//! Spec §15 "Built-in tools":
//!   > Built-in tools (file ops, shell, search — bundled in `atelier-core`)
//!   > and MCP-routed tools (external servers) flow through the same
//!   > `ToolDispatching → ToolExecuting` state transitions. The loop does
//!   > not branch on tool origin.
//!
//! Each module implements one [`crate::dispatcher::Tool`]; the dispatcher
//! invokes them via the same trait it'll use for MCP-routed proxies once
//! `rmcp` lands. Manifests at `crates/atelier-core/tools/*.v1.json`
//! advertise these tools to the model + carry the JSON-Schema input
//! definitions the harness uses to validate model-emitted arguments before
//! dispatch.
//!
//! Path safety is uniform across all file-touching tools: every accepted
//! `path` argument is repo-relative, no `..` components, no absolute paths,
//! and **canonicalized** so symlinks pointing outside the workspace are
//! rejected. The helpers in [`crate::path_safety`] enforce both the
//! syntax-level check ([`crate::path_safety::resolve_repo_path`]) and the
//! symlink-containment check
//! ([`crate::path_safety::ensure_inside_workspace_existing`] /
//! [`crate::path_safety::ensure_inside_workspace_creatable`]). The
//! harness's own file I/O does **not** flow through the §11 sandbox
//! profile generator (which wraps shelled-out subprocesses only), so
//! containment has to live here.

pub use crate::path_safety::{
    create_dir_all_inside_workspace, ensure_inside_workspace_creatable,
    ensure_inside_workspace_existing, resolve_repo_path,
};

/// Map a [`tokio::task::JoinError`] from a tool's `spawn_blocking` wrapper
/// onto a [`crate::error::ToolError::ExecutionFailed`], preserving the
/// panic payload when the closure panicked. Without this, every blocking-
/// pool failure surfaces as the generic "task X panicked" message and the
/// underlying assertion / unwrap / explicit panic message is dropped on
/// the floor — a real debugging hazard. Every built-in tool's
/// `.await.map_err(...)` ends with this helper.
pub(crate) fn join_error_to_tool_error(
    tool: &'static str,
    join_err: tokio::task::JoinError,
) -> crate::error::ToolError {
    let stderr = if join_err.is_panic() {
        // `into_panic` returns Box<dyn Any + Send>; the common case is
        // a &'static str or a String payload from `panic!(...)`. Try
        // both before falling back to a generic label.
        let payload = join_err.into_panic();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        format!("blocking pool panic: {msg}")
    } else {
        format!("blocking pool join failure: {join_err}")
    };
    crate::error::ToolError::ExecutionFailed {
        tool: tool.to_string(),
        exit_code: -1,
        stderr,
    }
}

pub mod ast_grep;
pub mod builtin_wrapper;
pub mod edit_file;
pub mod grep;
pub mod list_dir;
pub mod read_file;
pub mod shell;
pub mod spawn_subagent;
pub mod write_file;

pub use builtin_wrapper::{BuiltInToolWrapper, BuiltInWrapError};

use std::sync::Arc;

use crate::dispatcher::{RegisterError, Tool, ToolRegistry};
use crate::subagents::{SubagentSpawner, SubagentTypeRegistry};

/// Runtime dependencies required by tools that need late-bound state at
/// registration time. Today only `spawn_subagent` uses this; the seven
/// static built-ins construct their executors with `Arc::new(<Type>)`.
pub struct BuiltinDeps {
    pub spawner: Arc<dyn SubagentSpawner>,
    pub type_registry: Arc<SubagentTypeRegistry>,
}

/// Bundled-manifest set for the seven static built-in tools. Each `(name,
/// manifest_json, executor_ctor)` row lives in lockstep with
/// `crates/atelier-core/tools/*.v1.json` and the corresponding module
/// in this directory. A manifest/impl name drift is caught at startup
/// by [`BuiltInToolWrapper::from_manifest_json`] (returns
/// [`BuiltInWrapError::NameMismatch`]).
fn builtin_table() -> Vec<(&'static str, &'static str, Arc<dyn Tool>)> {
    vec![
        (
            "read_file",
            include_str!("../../tools/read_file.v1.json"),
            Arc::new(read_file::ReadFile),
        ),
        (
            "list_dir",
            include_str!("../../tools/list_dir.v1.json"),
            Arc::new(list_dir::ListDir),
        ),
        (
            "grep",
            include_str!("../../tools/grep.v1.json"),
            Arc::new(grep::Grep),
        ),
        (
            "write_file",
            include_str!("../../tools/write_file.v1.json"),
            Arc::new(write_file::WriteFile),
        ),
        (
            "edit_file",
            include_str!("../../tools/edit_file.v1.json"),
            Arc::new(edit_file::EditFile),
        ),
        (
            "ast_grep",
            include_str!("../../tools/ast_grep.v1.json"),
            Arc::new(ast_grep::AstGrep),
        ),
        (
            "shell",
            include_str!("../../tools/shell.v1.json"),
            Arc::new(shell::Shell),
        ),
    ]
}

/// Outcome of a [`register_builtins`] call. Mirrors the shape of
/// [`crate::mcp::registration::RegisterMcpReport`] so the runner can
/// surface built-in and MCP-routed registrations through the same
/// reporting path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RegisterBuiltinsReport {
    /// Names of tools that landed in the registry, in registration
    /// order.
    pub tools_registered: Vec<String>,
}

/// Construction-time failure surface for [`register_builtins`]. A
/// failure here is a programmer error (manifest/impl drift, malformed
/// manifest JSON, duplicate name) — the runner maps this onto
/// `RunError::Config` so the binary refuses to start with a hint.
#[derive(Debug, thiserror::Error)]
pub enum RegisterBuiltinsError {
    #[error("built-in tool {name:?}: {source}")]
    Wrap {
        name: String,
        #[source]
        source: BuiltInWrapError,
    },

    #[error("built-in tool {name:?}: registry rejected: {source}")]
    Register {
        name: String,
        #[source]
        source: RegisterError,
    },
}

/// Walk the bundled built-in tool manifests, wrap each impl with a
/// [`BuiltInToolWrapper`] (manifest as source of truth for
/// name/description/side-effect-class/input-schema), and register the
/// wrappers into `registry`. Spec §15: built-ins and MCP-routed tools
/// share the same dispatch surface; this is the seam that makes it
/// concrete for the built-in side.
///
/// Called once at session startup, BEFORE
/// [`crate::mcp::registration::register_mcp_servers`] so that built-in
/// names win on collision (an MCP server advertising `read_file` is
/// recorded as a `ServerFailure`).
pub fn register_builtins(
    registry: &mut ToolRegistry,
    deps: Option<BuiltinDeps>,
) -> Result<RegisterBuiltinsReport, RegisterBuiltinsError> {
    let mut report = RegisterBuiltinsReport::default();
    for (name, manifest_json, inner) in builtin_table() {
        let wrapper =
            BuiltInToolWrapper::from_manifest_json(manifest_json, inner).map_err(|source| {
                RegisterBuiltinsError::Wrap {
                    name: name.to_string(),
                    source,
                }
            })?;
        registry
            .register(Arc::new(wrapper))
            .map_err(|source| RegisterBuiltinsError::Register {
                name: name.to_string(),
                source,
            })?;
        report.tools_registered.push(name.to_string());
    }

    // §10 spawn_subagent — registered only when runtime deps are supplied.
    // Callers that haven't wired a SubagentSpawner (e.g. bare unit tests)
    // pass `None` and get the original 7-tool set.
    if let Some(d) = deps {
        let name = "spawn_subagent";
        let inner = Arc::new(spawn_subagent::SpawnSubagent::new(
            d.spawner,
            d.type_registry,
        ));
        let wrapper = BuiltInToolWrapper::from_manifest_json(
            include_str!("../../tools/spawn_subagent.v1.json"),
            inner,
        )
        .map_err(|source| RegisterBuiltinsError::Wrap {
            name: name.to_string(),
            source,
        })?;
        registry
            .register(Arc::new(wrapper))
            .map_err(|source| RegisterBuiltinsError::Register {
                name: name.to_string(),
                source,
            })?;
        report.tools_registered.push(name.to_string());
    }

    Ok(report)
}

#[cfg(test)]
mod register_tests {
    use super::*;
    use crate::dispatcher::SideEffectClass;

    /// All seven built-ins land in the registry, the order matches
    /// the table, every name resolves, and the manifest-declared
    /// side-effect class flows through the wrapper.
    #[test]
    fn register_builtins_registers_all_seven_with_correct_metadata() {
        let mut registry = ToolRegistry::new();
        let report = register_builtins(&mut registry, None).expect("register");
        assert_eq!(
            report.tools_registered,
            vec![
                "read_file",
                "list_dir",
                "grep",
                "write_file",
                "edit_file",
                "ast_grep",
                "shell",
            ],
        );
        assert_eq!(registry.len(), 7);

        // Spot-check the trust-budget classes from the manifests
        // (catches a manifest/impl drift where the manifest claims
        // local-safe but the wrapper somehow inherited local-risky
        // from the inner impl — the spec §8 trust budget routes on
        // this value).
        let read_file = registry.get("read_file").unwrap();
        assert_eq!(read_file.side_effect_class(), SideEffectClass::LocalSafe);
        let write_file = registry.get("write_file").unwrap();
        assert_eq!(write_file.side_effect_class(), SideEffectClass::LocalRisky);
        let shell = registry.get("shell").unwrap();
        assert_eq!(shell.side_effect_class(), SideEffectClass::LocalRisky);
    }

    /// Calling twice is a duplicate-name error — surfaces as
    /// `RegisterBuiltinsError::Register`, NOT a silent overwrite.
    #[test]
    fn register_builtins_is_idempotent_only_once() {
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, None).unwrap();
        let err = register_builtins(&mut registry, None).unwrap_err();
        assert!(matches!(err, RegisterBuiltinsError::Register { .. }));
    }

    /// Schema validation runs ahead of inner execution — passing an
    /// unknown field through `read_file` is rejected by the wrapper's
    /// manifest schema, never reaches `ReadFile::execute`.
    #[test]
    fn wrapper_rejects_unknown_field_via_manifest_schema() {
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, None).unwrap();
        let read_file = registry.get("read_file").unwrap();
        let err = read_file
            .validate_args(&serde_json::json!({"path": "a.txt", "bogus": 1}))
            .unwrap_err();
        assert!(
            err.contains("bogus") || err.contains("additional"),
            "got {err:?}"
        );
    }
}
