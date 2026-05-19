//! Built-in `spawn_subagent` tool (§10 delegation mode). Manifest:
//! `crates/atelier-core/tools/spawn_subagent.v1.json`.
//!
//! Two invocation shapes (enforced by the manifest's `oneOf`):
//!   1. **Spawn**: `{description, prompt, subagent_type?, tool_allowlist?, max_turns?}`
//!   2. **Cancel**: `{subagent_id, cancel: true}`
//!
//! For the spawn shape the tool:
//!   - Resolves the sub-agent type from the registry (default `general-purpose`).
//!   - Checks recursion depth against [`RECURSION_DEPTH_CAP`]; returns
//!     `ToolError::SchemaViolation` if exceeded (spec §10 line 556).
//!   - Delegates to [`SubagentSpawner::spawn`] and awaits completion.
//!   - Returns the sub-agent's final message + status + cost summary.
//!
//! For the cancel shape the tool delegates to [`SubagentSpawner::cancel`] and
//! returns immediately.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::dispatcher::{SideEffectClass, ToolContext, ToolResult};
use crate::error::ToolError;
use crate::subagents::{
    SpawnRequest, SubagentId, SubagentSpawner, SubagentTypeRegistry, RECURSION_DEPTH_CAP,
};

pub struct SpawnSubagent {
    pub(crate) spawner: Arc<dyn SubagentSpawner>,
    pub(crate) type_registry: Arc<SubagentTypeRegistry>,
}

impl SpawnSubagent {
    pub fn new(
        spawner: Arc<dyn SubagentSpawner>,
        type_registry: Arc<SubagentTypeRegistry>,
    ) -> Self {
        Self {
            spawner,
            type_registry,
        }
    }
}

// ---------- argument shapes ----------

#[derive(Debug, Deserialize)]
struct SpawnArgs {
    description: String,
    prompt: String,
    #[serde(default)]
    subagent_type: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    tool_allowlist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CancelArgs {
    subagent_id: String,
    // `cancel: true` is enforced by the manifest oneOf; we just check the field is present.
}

// ---------- Tool impl ----------

#[async_trait]
impl crate::dispatcher::Tool for SpawnSubagent {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn side_effect_class(&self) -> SideEffectClass {
        SideEffectClass::LocalRisky
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // The manifest's oneOf differentiates spawn vs cancel by presence of
        // `cancel: true`. Check cancel first (it's the simpler arm).
        if args.get("cancel").and_then(|v| v.as_bool()) == Some(true) {
            return self.do_cancel(args, ctx).await;
        }
        self.do_spawn(args, ctx).await
    }
}

impl SpawnSubagent {
    async fn do_spawn(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let spawn_args: SpawnArgs =
            serde_json::from_value(args).map_err(|e| ToolError::SchemaViolation {
                tool: "spawn_subagent".into(),
                error: format!("spawn shape: {e}"),
            })?;

        // Recursion depth check (spec §10 line 556).
        let depth = ctx.subagent_depth;
        if depth >= RECURSION_DEPTH_CAP {
            return Err(ToolError::SchemaViolation {
                tool: "spawn_subagent".into(),
                error: format!(
                    "recursion depth cap ({RECURSION_DEPTH_CAP}) reached — spawn refused"
                ),
            });
        }

        // Resolve the sub-agent type; default `general-purpose`.
        let type_name = spawn_args
            .subagent_type
            .as_deref()
            .unwrap_or("general-purpose");
        let subagent_type = self
            .type_registry
            .get(type_name)
            .ok_or_else(|| ToolError::SchemaViolation {
                tool: "spawn_subagent".into(),
                error: format!("unknown subagent_type: {type_name:?}"),
            })?
            .clone();

        let id = SubagentId::new();
        let req = SpawnRequest {
            id: id.clone(),
            parent_depth: depth,
            parent_cancel: ctx.cancel.clone(),
            subagent_type,
            description: spawn_args.description,
            prompt: spawn_args.prompt,
            max_turns_override: spawn_args.max_turns,
            tool_allowlist_override: spawn_args.tool_allowlist,
        };

        let result = self
            .spawner
            .spawn(req)
            .await
            .map_err(|e| ToolError::ExecutionFailed {
                tool: "spawn_subagent".into(),
                exit_code: -1,
                stderr: e.to_string(),
            })?;

        let output = serde_json::json!({
            "subagent_id": result.id.to_string(),
            "result": result.result,
            "status": result.status.to_string(),
            "turns_used": result.turns_used,
            "cost": {
                "prompt_tokens": result.cost.prompt_tokens,
                "completion_tokens": result.cost.completion_tokens,
                "cached_tokens": result.cost.cached_tokens,
                "cost_usd": result.cost.cost_usd,
            }
        });
        Ok(ToolResult {
            output,
            staged_writes: None,
        })
    }

    async fn do_cancel(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let cancel_args: CancelArgs =
            serde_json::from_value(args).map_err(|e| ToolError::SchemaViolation {
                tool: "spawn_subagent".into(),
                error: format!("cancel shape: {e}"),
            })?;

        let id_uuid = cancel_args.subagent_id.parse::<uuid::Uuid>().map_err(|e| {
            ToolError::SchemaViolation {
                tool: "spawn_subagent".into(),
                error: format!("subagent_id is not a valid UUID: {e}"),
            }
        })?;
        let id = SubagentId(id_uuid);

        self.spawner
            .cancel(&id)
            .await
            .map_err(|e| ToolError::ExecutionFailed {
                tool: "spawn_subagent".into(),
                exit_code: -1,
                stderr: e.to_string(),
            })?;

        Ok(ToolResult {
            output: serde_json::json!({
                "subagent_id": id.to_string(),
                "status": "cancelled"
            }),
            staged_writes: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::Tool;
    use crate::subagents::{SubagentCost, SubagentResult, SubagentStatus};
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct MockSpawner {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SubagentSpawner for MockSpawner {
        async fn spawn(
            &self,
            req: SpawnRequest,
        ) -> Result<SubagentResult, crate::subagents::SpawnError> {
            self.calls.lock().unwrap().push(req.id.to_string());
            Ok(SubagentResult {
                id: req.id,
                result: "mock result".to_string(),
                status: SubagentStatus::Completed,
                turns_used: 3,
                cost: SubagentCost {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    cached_tokens: 0,
                    cost_usd: None,
                },
            })
        }

        async fn cancel(&self, _id: &SubagentId) -> Result<(), crate::subagents::CancelError> {
            Ok(())
        }

        async fn wait_all(&self, _parent_id: &SubagentId) {}
    }

    fn make_tool() -> SpawnSubagent {
        let reg = SubagentTypeRegistry::load(std::path::Path::new("/tmp"), None).unwrap();
        SpawnSubagent::new(
            Arc::new(MockSpawner {
                calls: Mutex::new(vec![]),
            }),
            Arc::new(reg),
        )
    }

    // Returns (dir, sandbox, cancel) — caller must keep `dir` alive to keep
    // the sandbox path valid for the duration of the test.
    fn make_ctx_parts(
        depth: u8,
    ) -> (
        tempfile::TempDir,
        crate::sandbox::SandboxPolicy,
        CancellationToken,
    ) {
        let dir = tempfile::TempDir::new().unwrap();
        let policy = crate::sandbox::SandboxPolicy::restrictive(dir.path()).unwrap();
        let token = CancellationToken::new();
        let _ = depth; // used by caller for subagent_depth
        (dir, policy, token)
    }

    #[tokio::test]
    async fn spawn_happy_path_general_purpose() {
        let tool = make_tool();
        let (_dir, sandbox, cancel) = make_ctx_parts(0);
        let ctx = ToolContext {
            workspace_root: std::path::Path::new("/tmp"),
            sandbox: &sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel,
            deadline: std::time::Duration::from_secs(60),
            subagent_depth: 0,
        };
        let args = serde_json::json!({
            "description": "test task",
            "prompt": "do something"
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        assert_eq!(result.output["status"], "completed");
        assert_eq!(result.output["turns_used"], 3);
        assert_eq!(result.output["result"], "mock result");
    }

    #[tokio::test]
    async fn spawn_unknown_type_returns_schema_violation() {
        let tool = make_tool();
        let (_dir, sandbox, cancel) = make_ctx_parts(0);
        let ctx = ToolContext {
            workspace_root: std::path::Path::new("/tmp"),
            sandbox: &sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel,
            deadline: std::time::Duration::from_secs(60),
            subagent_depth: 0,
        };
        let args = serde_json::json!({
            "description": "test",
            "prompt": "do it",
            "subagent_type": "nonexistent-type"
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn depth_cap_enforced() {
        let tool = make_tool();
        let (_dir, sandbox, cancel) = make_ctx_parts(0);
        let ctx = ToolContext {
            workspace_root: std::path::Path::new("/tmp"),
            sandbox: &sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel,
            deadline: std::time::Duration::from_secs(60),
            subagent_depth: RECURSION_DEPTH_CAP,
        };
        let args = serde_json::json!({
            "description": "overflow attempt",
            "prompt": "spawn child"
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(
            matches!(&err, ToolError::SchemaViolation { error, .. } if error.contains("cap")),
            "expected depth-cap SchemaViolation, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cancel_shape_dispatches() {
        let tool = make_tool();
        let (_dir, sandbox, cancel) = make_ctx_parts(0);
        let id = SubagentId::new();
        let ctx = ToolContext {
            workspace_root: std::path::Path::new("/tmp"),
            sandbox: &sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel,
            deadline: std::time::Duration::from_secs(60),
            subagent_depth: 0,
        };
        let args = serde_json::json!({
            "subagent_id": id.to_string(),
            "cancel": true
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        assert_eq!(result.output["status"], "cancelled");
    }

    #[tokio::test]
    async fn oneof_shapes_invalid_cancel_false_rejected() {
        let tool = make_tool();
        let (_dir, sandbox, cancel) = make_ctx_parts(0);
        let id = SubagentId::new();
        let ctx = ToolContext {
            workspace_root: std::path::Path::new("/tmp"),
            sandbox: &sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel,
            deadline: std::time::Duration::from_secs(60),
            subagent_depth: 0,
        };
        // cancel: false falls into the spawn arm (not cancel shape),
        // which will fail because prompt/description are missing.
        let args = serde_json::json!({
            "subagent_id": id.to_string(),
            "cancel": false
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }
}
