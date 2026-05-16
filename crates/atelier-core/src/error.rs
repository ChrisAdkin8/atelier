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
}

impl ToolError {
    /// How the §2.5 state machine should react to this error.
    pub fn recovery(&self) -> Recovery {
        match self {
            Self::SandboxViolation { .. } => Recovery::Fail,
            Self::PermissionDenied { .. } => Recovery::AwaitUser,
            Self::Timeout { .. }
            | Self::ResultMalformed { .. }
            | Self::ExecutionFailed { .. }
            | Self::SchemaViolation { .. } => Recovery::Retry,
            Self::McpServerUnreachable { .. } | Self::McpServerCrashed { .. } => {
                Recovery::RetryBudgeted
            }
        }
    }

    /// The error-kind tag persisted in session `tool_fixtures.error.kind`
    /// (`schemas/session/v1.json`).
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
}
