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
    ensure_inside_workspace_creatable, ensure_inside_workspace_existing, resolve_repo_path,
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
pub mod edit_file;
pub mod grep;
pub mod list_dir;
pub mod read_file;
pub mod shell;
pub mod write_file;
