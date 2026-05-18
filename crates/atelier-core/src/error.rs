//! Tool error taxonomy and state-machine recovery routing.
//!
//! Spec §2.5 — "Tool error model". Each variant carries the structured context
//! needed to surface the failure to the user *and* to decide how the §2.5 state
//! machine should react.

use std::time::Duration;

/// Errors that can occur during tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("sandbox blocked {tool} from {attempted}")]
    SandboxViolation { tool: String, attempted: String },

    #[error("{tool} timed out after {elapsed:?}")]
    Timeout { tool: String, elapsed: Duration },

    #[error("MCP server {server} unreachable")]
    McpServerUnreachable { server: String },

    #[error("MCP server {server} crashed")]
    McpServerCrashed {
        server: String,
        last_message: Option<String>,
    },

    #[error("{tool} returned malformed result: {parse_error}")]
    ResultMalformed { tool: String, parse_error: String },

    #[error("permission denied for {tool}: {reason}")]
    PermissionDenied { tool: String, reason: String },

    #[error("{tool} exited {exit_code}: {stderr}")]
    ExecutionFailed {
        tool: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("{tool} output violated its schema: {error}")]
    SchemaViolation { tool: String, error: String },

    /// v60.29 H9 — caller-initiated cancellation. Tripped by the
    /// session's root `CancellationToken` (SIGINT/SIGTERM in
    /// `atelier-cli`, an explicit `SessionCommand::Cancel` from a
    /// driver, or a parent `tokio::select!` racer giving up).
    #[error("{tool} cancelled before completion")]
    Cancelled { tool: String },

    /// v60.29 H9 — per-tool deadline exceeded. The default is 60s; a
    /// `tool_manifest.v1.json` `deadline_ms` field overrides per-tool.
    /// The `Duration` is what the dispatcher actually waited.
    #[error("{tool} exceeded its deadline of {deadline:?}")]
    Deadline { tool: String, deadline: Duration },
}

impl ToolError {
    /// How the §2.5 state machine should react to this error.
    pub fn recovery(&self) -> Recovery {
        match self {
            Self::SandboxViolation { .. } => Recovery::Fail,
            Self::PermissionDenied { .. } => Recovery::AwaitUser,
            Self::Timeout { .. }
            | Self::Deadline { .. }
            | Self::ResultMalformed { .. }
            | Self::ExecutionFailed { .. }
            | Self::SchemaViolation { .. } => Recovery::Retry,
            Self::McpServerUnreachable { .. } | Self::McpServerCrashed { .. } => {
                Recovery::RetryBudgeted
            }
            // Cancellation came from a higher layer (SIGINT, parent
            // dropped, explicit cancel); the agent loop should stop,
            // not retry into a tighter loop with the same cancel still
            // armed.
            Self::Cancelled { .. } => Recovery::Fail,
        }
    }

    /// The error-kind tag persisted in session `tool_fixtures.error.kind`
    /// (`schemas/session/v1.json`). Doubles as the stable wire label
    /// (no serde derive on `ToolError`; this method is the single
    /// source of truth) — pinned by the `tool_error_kind_*` tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SandboxViolation { .. } => "SandboxViolation",
            Self::Timeout { .. } => "Timeout",
            Self::McpServerUnreachable { .. } => "McpServerUnreachable",
            Self::McpServerCrashed { .. } => "McpServerCrashed",
            Self::ResultMalformed { .. } => "ResultMalformed",
            Self::PermissionDenied { .. } => "PermissionDenied",
            Self::ExecutionFailed { .. } => "ExecutionFailed",
            Self::SchemaViolation { .. } => "SchemaViolation",
            Self::Cancelled { .. } => "Cancelled",
            Self::Deadline { .. } => "Deadline",
        }
    }
}

/// State-machine routing decision per spec §2.5 "Tool error model".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Transition to `Failed`. Do not retry.
    Fail,
    /// Transition to `AwaitingUser`. User decision required.
    AwaitUser,
    /// Transition back to `Streaming` with the error injected as a tool result.
    /// Unbounded — the agent decides when to give up.
    Retry,
    /// Like `Retry`, but tracked against a per-source retry budget (default 3).
    /// On budget exhaustion, escalates to `AwaitUser`.
    RetryBudgeted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_violation_fails_fast() {
        let e = ToolError::SandboxViolation {
            tool: "shell".into(),
            attempted: "curl evil.example".into(),
        };
        assert_eq!(e.recovery(), Recovery::Fail);
        assert_eq!(e.kind(), "SandboxViolation");
    }

    #[test]
    fn mcp_crash_is_budgeted_retry() {
        let e = ToolError::McpServerCrashed {
            server: "filesystem".into(),
            last_message: None,
        };
        assert_eq!(e.recovery(), Recovery::RetryBudgeted);
        assert_eq!(e.kind(), "McpServerCrashed");
    }

    #[test]
    fn permission_denied_awaits_user() {
        let e = ToolError::PermissionDenied {
            tool: "write_file".into(),
            reason: "outside repo scope".into(),
        };
        assert_eq!(e.recovery(), Recovery::AwaitUser);
    }

    #[test]
    fn execution_failed_retries() {
        let e = ToolError::ExecutionFailed {
            tool: "pytest".into(),
            exit_code: 1,
            stderr: "1 failed".into(),
        };
        assert_eq!(e.recovery(), Recovery::Retry);
    }

    /// v60.29 H9 — L-D-5 wire-label agreement.
    ///
    /// `ToolError` doesn't carry a serde derive (it's a runtime error,
    /// not a wire payload) — `kind()` is the wire label. Each variant
    /// must produce a stable tag that the session-schema enum lists
    /// and that the post-mortem ledger pins.
    #[test]
    fn tool_error_kind_labels_are_stable() {
        assert_eq!(
            ToolError::SandboxViolation {
                tool: "x".into(),
                attempted: "y".into()
            }
            .kind(),
            "SandboxViolation"
        );
        assert_eq!(
            ToolError::Timeout {
                tool: "x".into(),
                elapsed: Duration::from_secs(1)
            }
            .kind(),
            "Timeout"
        );
        assert_eq!(
            ToolError::McpServerUnreachable { server: "x".into() }.kind(),
            "McpServerUnreachable"
        );
        assert_eq!(
            ToolError::McpServerCrashed {
                server: "x".into(),
                last_message: None
            }
            .kind(),
            "McpServerCrashed"
        );
        assert_eq!(
            ToolError::ResultMalformed {
                tool: "x".into(),
                parse_error: "y".into()
            }
            .kind(),
            "ResultMalformed"
        );
        assert_eq!(
            ToolError::PermissionDenied {
                tool: "x".into(),
                reason: "y".into()
            }
            .kind(),
            "PermissionDenied"
        );
        assert_eq!(
            ToolError::ExecutionFailed {
                tool: "x".into(),
                exit_code: 1,
                stderr: "y".into()
            }
            .kind(),
            "ExecutionFailed"
        );
        assert_eq!(
            ToolError::SchemaViolation {
                tool: "x".into(),
                error: "y".into()
            }
            .kind(),
            "SchemaViolation"
        );
        assert_eq!(
            ToolError::Cancelled { tool: "x".into() }.kind(),
            "Cancelled"
        );
        assert_eq!(
            ToolError::Deadline {
                tool: "x".into(),
                deadline: Duration::from_secs(1)
            }
            .kind(),
            "Deadline"
        );
    }

    #[test]
    fn cancelled_is_terminal() {
        let e = ToolError::Cancelled { tool: "x".into() };
        assert_eq!(e.recovery(), Recovery::Fail);
    }

    #[test]
    fn deadline_retries() {
        let e = ToolError::Deadline {
            tool: "x".into(),
            deadline: Duration::from_millis(200),
        };
        assert_eq!(e.recovery(), Recovery::Retry);
    }
}
