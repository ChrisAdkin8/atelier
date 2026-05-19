//! §10 Sub-agent type registry.
//!
//! A **sub-agent type** is a manifest (`schemas/config/subagent_type.v1.json`)
//! declaring a `name`, `description`, `system_prompt_addendum`, optional
//! `tool_allowlist`, `default_max_turns`, `model_routing` override, and
//! `side_effect_class_cap`. When the parent agent invokes `spawn_subagent`
//! with `subagent_type: '<name>'`, the harness materialises a fresh §2.5
//! state machine configured by this manifest.
//!
//! ## Storage (layered override, later wins)
//!
//!   1. **Bundled** — `include_str!`'d from `crates/atelier-core/subagents/`.
//!   2. **Global** — `~/.atelier/subagents/<name>.json`.
//!   3. **Per-repo** — `<workspace>/.atelier/subagents/<name>.json`.
//!
//! All three layers tolerate absence. A clean workspace with no
//! `~/.atelier/subagents/` directory still loads the three bundled types.
//!
//! ## Spec constants (PROVISIONAL — §10 line 521, line 556)
//!
//!   - [`DEFAULT_MAX_TURNS`] = 25
//!   - [`RECURSION_DEPTH_CAP`] = 3
//!   - [`BUS_FANOUT_FACTOR`] = 4  (1 + RECURSION_DEPTH_CAP)

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::dispatcher::SideEffectClass;

/// PROVISIONAL — spec §10 line 521. Default turn cap when neither the
/// sub-agent type manifest nor the `spawn_subagent` invocation specifies one.
pub const DEFAULT_MAX_TURNS: u32 = 25;

/// PROVISIONAL — spec §10 line 556. Maximum recursion depth; attempts beyond
/// this return `ToolError::SchemaViolation`.
pub const RECURSION_DEPTH_CAP: u8 = 3;

/// Broadcast bus capacity multiplier for sub-agent event fanout. The session
/// bus capacity is multiplied by this constant so a depth-3 spawn tree does
/// not overflow the channel under burst conditions.
pub const BUS_FANOUT_FACTOR: usize = 1 + RECURSION_DEPTH_CAP as usize; // 4

// ---------- schema validator ----------

const SUBAGENT_TYPE_SCHEMA_JSON: &str =
    include_str!("../../../schemas/config/subagent_type.v1.json");

const ROUTING_SCHEMA_JSON: &str = include_str!("../../../schemas/config/routing.v1.json");

fn schema_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value = serde_json::from_str(SUBAGENT_TYPE_SCHEMA_JSON)
            .expect("embedded subagent_type.v1.json parses as JSON");
        let routing: serde_json::Value = serde_json::from_str(ROUTING_SCHEMA_JSON)
            .expect("embedded routing.v1.json parses as JSON");
        let routing_resource = jsonschema::Resource::from_contents(routing)
            .expect("routing.v1.json is a valid JSON Schema resource");
        jsonschema::options()
            .with_resource(
                "https://atelier.example/schemas/config/routing.v1.json",
                routing_resource,
            )
            .build(&schema)
            .expect("embedded subagent_type.v1.json is a valid JSON Schema")
    })
}

// ---------- bundled manifests ----------

fn bundled_manifests() -> &'static [(&'static str, &'static str)] {
    &[
        ("researcher", include_str!("../subagents/researcher.json")),
        ("test-runner", include_str!("../subagents/test-runner.json")),
        (
            "general-purpose",
            include_str!("../subagents/general-purpose.json"),
        ),
    ]
}

// ---------- types ----------

/// Where a [`SubagentType`] was loaded from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentTypeSource {
    Bundled,
    UserHome,
    RepoLocal,
}

impl SubagentTypeSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::UserHome => "home",
            Self::RepoLocal => "repo",
        }
    }
}

/// A loaded sub-agent type manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentType {
    pub version: u32,
    pub name: String,
    pub description: String,
    pub system_prompt_addendum: String,
    /// If present, restricts the sub-agent's callable tool set.
    pub tool_allowlist: Option<Vec<String>>,
    /// Turn cap for this type. Falls back to [`DEFAULT_MAX_TURNS`] if absent.
    pub default_max_turns: Option<u32>,
    /// Optional per-subagent routing override stored as raw JSON (references
    /// `schemas/config/routing.v1.json`; resolved at spawn time by
    /// `RunnerSpawner`).
    pub model_routing: Option<serde_json::Value>,
    /// Maximum side-effect class this sub-agent may invoke. `None` = no cap.
    pub side_effect_class_cap: Option<SideEffectClass>,
    pub source: SubagentTypeSource,
}

impl SubagentType {
    /// Effective max turns: per-type manifest value or the spec default.
    pub fn effective_max_turns(&self) -> u32 {
        self.default_max_turns.unwrap_or(DEFAULT_MAX_TURNS)
    }
}

/// Wire shape for deserialisation: matches the schema exactly.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentTypeWire {
    version: u32,
    name: String,
    description: String,
    system_prompt_addendum: String,
    #[serde(default)]
    tool_allowlist: Option<Vec<String>>,
    #[serde(default)]
    default_max_turns: Option<u32>,
    #[serde(default)]
    model_routing: Option<serde_json::Value>,
    #[serde(default)]
    side_effect_class_cap: Option<SideEffectClass>,
}

impl SubagentType {
    /// Parse a manifest body, validate against the bundled schema, then
    /// deserialise. Mirrors `Skill::from_manifest_json`.
    pub fn from_manifest_json(
        body: &str,
        source: SubagentTypeSource,
    ) -> Result<Self, SubagentTypeLoadError> {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|e| SubagentTypeLoadError::Parse(e.to_string()))?;
        let errs: Vec<String> = schema_validator()
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        if !errs.is_empty() {
            return Err(SubagentTypeLoadError::Schema(errs.join("; ")));
        }
        let wire: SubagentTypeWire = serde_json::from_value(value)
            .map_err(|e| SubagentTypeLoadError::Parse(e.to_string()))?;
        Ok(Self {
            version: wire.version,
            name: wire.name,
            description: wire.description,
            system_prompt_addendum: wire.system_prompt_addendum,
            tool_allowlist: wire.tool_allowlist,
            default_max_turns: wire.default_max_turns,
            model_routing: wire.model_routing,
            side_effect_class_cap: wire.side_effect_class_cap,
            source,
        })
    }
}

// ---------- errors ----------

#[derive(Debug, thiserror::Error)]
pub enum SubagentTypeLoadError {
    #[error("subagent type manifest does not parse as JSON: {0}")]
    Parse(String),
    #[error("subagent type manifest fails schema validation: {0}")]
    Schema(String),
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ---------- registry ----------

/// Collection of sub-agent types, indexed by name. `BTreeMap` for stable
/// iteration order.
#[derive(Debug, Clone, Default)]
pub struct SubagentTypeRegistry {
    types: BTreeMap<String, SubagentType>,
}

impl SubagentTypeRegistry {
    /// Walk the three layers in spec order (bundled → global → per-repo).
    /// Later wins on name collisions. Tolerates missing on-disk layers.
    pub fn load(repo_root: &Path, home_dir: Option<&Path>) -> Result<Self, SubagentTypeLoadError> {
        let mut types: BTreeMap<String, SubagentType> = BTreeMap::new();

        // Layer 1: bundled.
        for (canonical_name, body) in bundled_manifests() {
            let ty = SubagentType::from_manifest_json(body, SubagentTypeSource::Bundled)?;
            debug_assert_eq!(
                &ty.name, canonical_name,
                "bundled subagent manifest filename / name drift: {canonical_name} vs {}",
                ty.name
            );
            types.insert(ty.name.clone(), ty);
        }

        // Layer 2: ~/.atelier/subagents/.
        if let Some(home) = home_dir {
            for ty in Self::scan_dir(
                &home.join(".atelier/subagents"),
                SubagentTypeSource::UserHome,
            )? {
                types.insert(ty.name.clone(), ty);
            }
        }

        // Layer 3: <repo>/.atelier/subagents/.
        for ty in Self::scan_dir(
            &repo_root.join(".atelier/subagents"),
            SubagentTypeSource::RepoLocal,
        )? {
            types.insert(ty.name.clone(), ty);
        }

        Ok(Self { types })
    }

    fn scan_dir(
        dir: &Path,
        source: SubagentTypeSource,
    ) -> Result<Vec<SubagentType>, SubagentTypeLoadError> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let entries = fs::read_dir(dir).map_err(|e| SubagentTypeLoadError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| SubagentTypeLoadError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let body = fs::read_to_string(&path).map_err(|e| SubagentTypeLoadError::Io {
                path: path.clone(),
                source: e,
            })?;
            let ty = SubagentType::from_manifest_json(&body, source.clone())?;
            out.push(ty);
        }
        Ok(out)
    }

    /// Construct from an iterator of already-parsed types. Used in tests.
    pub fn from_types(types: impl IntoIterator<Item = SubagentType>) -> Self {
        Self {
            types: types.into_iter().map(|t| (t.name.clone(), t)).collect(),
        }
    }

    /// Look up a type by name. `None` = unregistered / not found.
    pub fn get(&self, name: &str) -> Option<&SubagentType> {
        self.types.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.types.keys().map(|k| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &SubagentType> {
        self.types.values()
    }
}

// ---------- sub-agent identity ----------

/// Unique identifier for a sub-agent invocation. Derived from a UUID so the
/// `session.subagents` map is stable across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubagentId(pub Uuid);

impl SubagentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Human-readable short form used in bus events and UI labels.
    pub fn short(&self) -> String {
        format!("sa-{}", &self.0.to_string()[..8])
    }
}

impl Default for SubagentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SubagentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------- spawner contract ----------

/// Terminal state of a sub-agent's §2.5 state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

impl fmt::Display for SubagentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        };
        write!(f, "{s}")
    }
}

/// Cost summary for a completed sub-agent run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentCost {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Result returned from a completed sub-agent invocation.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub id: SubagentId,
    /// The single final assistant message from the sub-agent.
    pub result: String,
    pub status: SubagentStatus,
    pub turns_used: u32,
    pub cost: SubagentCost,
}

/// Request to spawn a sub-agent. Constructed by the `spawn_subagent`
/// tool impl from the validated arguments + resolved type manifest.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Caller-assigned ID; included in bus events and session persistence.
    pub id: SubagentId,
    /// The parent's recursion depth (0 = root runner).
    pub parent_depth: u8,
    /// Parent's CancellationToken — the spawner creates a child token
    /// via `parent_cancel.child_token()` so cancellation cascades.
    pub parent_cancel: CancellationToken,
    /// Sub-agent type manifest resolved from the registry.
    pub subagent_type: SubagentType,
    /// One-line summary surfaced in UI cards and bus events.
    pub description: String,
    /// The user-role message the sub-agent begins with.
    pub prompt: String,
    /// Per-invocation cap overriding `subagent_type.default_max_turns`.
    pub max_turns_override: Option<u32>,
    /// Per-invocation allowlist overriding `subagent_type.tool_allowlist`.
    pub tool_allowlist_override: Option<Vec<String>>,
}

impl SpawnRequest {
    /// Effective max turns: per-invocation override, then type default, then
    /// the spec's PROVISIONAL default.
    pub fn effective_max_turns(&self) -> u32 {
        self.max_turns_override
            .or(self.subagent_type.default_max_turns)
            .unwrap_or(DEFAULT_MAX_TURNS)
    }

    /// Effective tool allowlist: per-invocation override, then type manifest.
    /// `None` means inherit the parent's full set.
    pub fn effective_tool_allowlist(&self) -> Option<Vec<String>> {
        self.tool_allowlist_override
            .clone()
            .or_else(|| self.subagent_type.tool_allowlist.clone())
    }
}

/// Error that can occur when spawning or cancelling a sub-agent.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("recursion depth cap ({cap}) reached — spawn refused")]
    DepthCapExceeded { cap: u8 },
    #[error("sub-agent {id} run failed: {reason}")]
    RunFailed { id: SubagentId, reason: String },
    #[error("sub-agent spawn infrastructure error: {0}")]
    Internal(String),
}

/// Error that can occur when cancelling a sub-agent.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    #[error("unknown subagent_id: {0}")]
    NotFound(SubagentId),
}

/// Inversion-of-control seam: `spawn_subagent` (in `atelier-core`) calls
/// through this trait; `RunnerSpawner` (in `atelier-cli`) provides the
/// implementation that constructs and runs a child `Runner`.
///
/// The trait lives in `atelier-core` so `SpawnSubagent::execute` can hold
/// an `Arc<dyn SubagentSpawner>` without a circular crate dependency.
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    /// Spawn a sub-agent and run it to completion.  Blocks (async) until the
    /// sub-agent terminates (completed / failed / timed_out) or is cancelled
    /// via a CancellationToken. The caller should `tokio::spawn` this if it
    /// wants fire-and-forget semantics; the `spawn_subagent` tool awaits it
    /// inline (the spec says the tool returns only once the sub-agent is done).
    async fn spawn(&self, req: SpawnRequest) -> Result<SubagentResult, SpawnError>;

    /// Cancel a running sub-agent by ID. The cancellation token hierarchy
    /// propagates automatically; this method additionally purges the ID from
    /// the in-flight registry so `wait_all` terminates promptly.
    async fn cancel(&self, id: &SubagentId) -> Result<(), CancelError>;

    /// Block until all sub-agents spawned by the given parent have terminated
    /// (for any status). Called by the §7 gate in the parent's Verifying step.
    /// Returns immediately when there are no running children.
    async fn wait_all(&self, parent_id: &SubagentId);
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn bundled_types_load_clean() {
        let dir = TempDir::new().unwrap();
        let reg = SubagentTypeRegistry::load(dir.path(), None).unwrap();
        assert_eq!(reg.len(), 3);
        let names: Vec<&str> = reg.names().collect();
        assert!(names.contains(&"researcher"));
        assert!(names.contains(&"test-runner"));
        assert!(names.contains(&"general-purpose"));
    }

    #[test]
    fn researcher_has_tool_allowlist_and_cap() {
        let dir = TempDir::new().unwrap();
        let reg = SubagentTypeRegistry::load(dir.path(), None).unwrap();
        let r = reg.get("researcher").unwrap();
        let allowlist = r.tool_allowlist.as_ref().expect("researcher has allowlist");
        assert!(allowlist.contains(&"read_file".to_string()));
        assert!(!allowlist.contains(&"write_file".to_string()));
        assert_eq!(r.side_effect_class_cap, Some(SideEffectClass::LocalSafe));
    }

    #[test]
    fn general_purpose_has_no_cap() {
        let dir = TempDir::new().unwrap();
        let reg = SubagentTypeRegistry::load(dir.path(), None).unwrap();
        let gp = reg.get("general-purpose").unwrap();
        assert!(gp.tool_allowlist.is_none());
        assert!(gp.side_effect_class_cap.is_none());
    }

    #[test]
    fn per_repo_override_wins_over_bundled() {
        let repo_dir = TempDir::new().unwrap();
        let subagent_dir = repo_dir.path().join(".atelier/subagents");
        fs::create_dir_all(&subagent_dir).unwrap();

        // Write a per-repo researcher with a wider allowlist.
        let override_json = r#"{
            "version": 1,
            "name": "researcher",
            "description": "Overridden researcher",
            "system_prompt_addendum": "You are overridden.",
            "tool_allowlist": ["read_file", "list_dir", "grep", "ast_grep", "shell"]
        }"#;
        fs::write(subagent_dir.join("researcher.json"), override_json).unwrap();

        let reg = SubagentTypeRegistry::load(repo_root(repo_dir.path()), None).unwrap();
        let r = reg.get("researcher").unwrap();
        assert_eq!(r.source, SubagentTypeSource::RepoLocal);
        let allowlist = r.tool_allowlist.as_ref().unwrap();
        assert!(allowlist.contains(&"shell".to_string()));
    }

    #[test]
    fn schema_invalid_manifest_surfaces_error() {
        let body = r#"{"version": 1, "name": "bad", "unknown_field": true}"#;
        let err = SubagentType::from_manifest_json(body, SubagentTypeSource::Bundled).unwrap_err();
        assert!(matches!(
            err,
            SubagentTypeLoadError::Schema(_) | SubagentTypeLoadError::Parse(_)
        ));
    }

    #[test]
    fn effective_max_turns_falls_back_to_default() {
        let body = r#"{
            "version": 1,
            "name": "no-turns",
            "description": "desc",
            "system_prompt_addendum": "addendum"
        }"#;
        let ty = SubagentType::from_manifest_json(body, SubagentTypeSource::Bundled).unwrap();
        assert_eq!(ty.effective_max_turns(), DEFAULT_MAX_TURNS);
    }

    #[test]
    fn constants_match_spec() {
        assert_eq!(DEFAULT_MAX_TURNS, 25);
        assert_eq!(RECURSION_DEPTH_CAP, 3);
        assert_eq!(BUS_FANOUT_FACTOR, 4);
    }

    // Helper: the registry load API takes repo_root where .atelier/ lives.
    fn repo_root(path: &Path) -> &Path {
        path
    }
}
