//! §15 hook manifest loader + first-use approval.
//!
//! Spec §15 "Hooks":
//!   Pre-tool / post-tool / on-verify-pass / on-verify-fail. Each declares a
//!   time budget; **over-budget = warn and continue, never block.** Hooks
//!   wrap both built-in tool calls and MCP-routed tool calls uniformly — no
//!   special case.
//!
//! Spec §11 "Policy":
//!   Hooks require per-hook approval on first use; subsequent runs use §8
//!   trust budget.
//!
//! Schema lives at `schemas/config/hook_manifest.v1.json`; one example ships
//! at `examples/hooks/log_pre_tool.v1.json`. This module is the Rust loader
//! and the per-hook approval store. **Hook execution** (subprocess and
//! `reqwest::post`) lands with the §15 tool dispatcher and the §11
//! subprocess launcher.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::persistence::PersistenceError;

/// Schema version expected by this build (`version: 1` in the manifest).
pub const HOOK_MANIFEST_VERSION: u32 = 1;

/// Per-hook approval file. Co-locates with manifests so the discovery rules
/// (per-repo overrides global) carry across both. Hidden by a leading `_`
/// so a name-overlap with a user hook is impossible (`^[a-z]…` regex rules
/// out leading underscore).
pub const APPROVALS_FILE: &str = "_approvals.json";

/// Allowed `name` per `schemas/config/hook_manifest.v1.json`:
/// `^[a-z][a-z0-9_-]*$`.
fn name_is_valid(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Lifecycle events a hook can register for. Kebab-case in JSON to match the
/// schema enum exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    PreTool,
    PostTool,
    OnVerifyPass,
    OnVerifyFail,
}

impl HookEvent {
    /// Whether `tool_filter` applies. Spec §15: ignored for on-verify-*.
    pub fn supports_tool_filter(self) -> bool {
        matches!(self, Self::PreTool | Self::PostTool)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreTool => "pre-tool",
            Self::PostTool => "post-tool",
            Self::OnVerifyPass => "on-verify-pass",
            Self::OnVerifyFail => "on-verify-fail",
        }
    }
}

/// Hook impl. Discriminated by `kind` in JSON. `Shell` and `Http` are the
/// two ways the schema allows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum HookImplementation {
    Shell {
        command: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

/// Loaded hook manifest. Round-trips through
/// `schemas/config/hook_manifest.v1.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HookManifest {
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_filter: Option<Vec<String>>,
    pub implementation: HookImplementation,
    pub time_budget_ms: u64,
    #[serde(default)]
    pub allow_net: bool,
}

impl HookManifest {
    /// Parse bytes + run the schema invariants the loader needs at runtime
    /// (version match, name regex, sane budget). serde already enforces the
    /// shape via `deny_unknown_fields`.
    pub fn from_json(bytes: &[u8]) -> Result<Self, HookError> {
        let m: Self = serde_json::from_slice(bytes).map_err(|e| HookError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), HookError> {
        if self.version != HOOK_MANIFEST_VERSION {
            return Err(HookError::IncompatibleVersion {
                got: self.version,
                expected: HOOK_MANIFEST_VERSION,
            });
        }
        if !name_is_valid(&self.name) {
            return Err(HookError::InvalidName(self.name.clone()));
        }
        if self.time_budget_ms == 0 {
            return Err(HookError::InvalidBudget);
        }
        if let Some(ref filter) = self.tool_filter {
            if !self.event.supports_tool_filter() && !filter.is_empty() {
                return Err(HookError::FilterNotSupportedForEvent(self.event));
            }
        }
        match &self.implementation {
            HookImplementation::Shell { command, .. } if command.is_empty() => {
                return Err(HookError::EmptyShellCommand);
            }
            HookImplementation::Http { url, .. } if url.is_empty() => {
                return Err(HookError::EmptyHttpUrl);
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether this hook fires for a given tool call. `tool_filter` patterns
    /// support a single trailing `*` wildcard and exact match. on-verify-*
    /// events ignore the filter entirely (they have no tool name).
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        if !self.event.supports_tool_filter() {
            return false;
        }
        let Some(ref filter) = self.tool_filter else {
            return true;
        };
        if filter.is_empty() {
            return true;
        }
        filter.iter().any(|pat| simple_glob(pat, tool_name))
    }
}

/// Minimal glob: `*` alone matches anything; `prefix*` is prefix match;
/// `*suffix` is suffix match; otherwise exact. Sufficient for tool names —
/// upgrade to `globset` if real patterns are ever needed.
fn simple_glob(pattern: &str, target: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return target.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return target.ends_with(suffix);
    }
    pattern == target
}

/// All hooks visible to a session — per-repo overlaid on top of global. Per
/// spec §15 same-name per-repo hooks shadow the global definition.
#[derive(Debug, Default, Clone)]
pub struct HookSet {
    by_name: BTreeMap<String, HookManifest>,
}

impl HookSet {
    /// Empty hook set — useful for tests and for sessions that opt out of
    /// hooks entirely.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load every `*.json` from `dir`. Missing dir is treated as empty
    /// (a fresh repo before any hook is registered). The approvals file
    /// (`_approvals.json`) and any other `_`-prefixed file is skipped.
    pub fn load_dir(dir: &Path) -> Result<Self, HookError> {
        let mut set = Self::default();
        set.merge_dir(dir)?;
        Ok(set)
    }

    /// Merge another directory's manifests on top of `self`. Used to layer
    /// per-repo manifests over global ones — later calls override earlier.
    pub fn merge_dir(&mut self, dir: &Path) -> Result<&mut Self, HookError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(self),
            Err(e) => {
                return Err(HookError::Io {
                    path: dir.to_path_buf(),
                    source: e,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|e| HookError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if !file_name.ends_with(".json") || file_name.starts_with('_') {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| HookError::Io {
                path: path.clone(),
                source: e,
            })?;
            let manifest = HookManifest::from_json(&bytes).map_err(|e| match e {
                HookError::Parse(msg) => HookError::ParseAt {
                    path: path.clone(),
                    message: msg,
                },
                other => other,
            })?;
            // Spec §15: per-repo hooks shadow same-name global ones.
            // Surface the shadow via `tracing::info!` so a user adding a
            // per-repo hook with an accidental name collision sees the
            // global is no longer firing.
            let name = manifest.name.clone();
            if self.by_name.contains_key(&name) {
                tracing::info!(
                    hook = %name,
                    source = %path.display(),
                    "per-repo hook manifest shadows an earlier (global) definition with the same name"
                );
            }
            self.by_name.insert(name, manifest);
        }
        Ok(self)
    }

    /// All hooks registered for a given event, in deterministic name order.
    pub fn for_event(&self, event: HookEvent) -> Vec<&HookManifest> {
        self.by_name.values().filter(|m| m.event == event).collect()
    }

    /// All hooks that should fire for a specific tool call at a tool event.
    /// Convenience wrapper around `for_event` + `matches_tool`.
    pub fn for_tool_event(&self, event: HookEvent, tool_name: &str) -> Vec<&HookManifest> {
        self.for_event(event)
            .into_iter()
            .filter(|m| m.matches_tool(tool_name))
            .collect()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// First-use approval store. Spec §11 / §15: each hook needs explicit user
/// approval the first time it runs in a given scope (per-repo). After that
/// subsequent invocations are gated by §8 trust budget — not this store.
///
/// Persisted as JSON next to the manifests so disabling a hook is one `rm`
/// of its entry (or of the whole file to revoke everything).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HookApprovals {
    #[serde(default)]
    pub approved: BTreeMap<String, String>,
}

impl HookApprovals {
    pub fn load(path: &Path) -> Result<Self, PersistenceError> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| PersistenceError::Deserialize {
                path: path.to_path_buf(),
                error: e.to_string(),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PersistenceError::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), PersistenceError> {
        let parent = path.parent().ok_or_else(|| PersistenceError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "approvals path has no parent"),
        })?;
        std::fs::create_dir_all(parent).map_err(|e| PersistenceError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| PersistenceError::Serialize(e.to_string()))?;
        let mut tmp =
            tempfile::NamedTempFile::new_in(parent).map_err(|e| PersistenceError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        io::Write::write_all(tmp.as_file_mut(), &json).map_err(|e| PersistenceError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.persist(path).map_err(|e| PersistenceError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
        Ok(())
    }

    /// Mark a hook approved. `granted_at` is an RFC 3339 timestamp; callers
    /// pass `time::OffsetDateTime::now_utc()` or equivalent rendered to
    /// string. Keeping it stringly typed mirrors `OnDiskSession::created_at`.
    pub fn approve(&mut self, name: impl Into<String>, granted_at: impl Into<String>) {
        self.approved.insert(name.into(), granted_at.into());
    }

    pub fn revoke(&mut self, name: &str) -> Option<String> {
        self.approved.remove(name)
    }

    pub fn is_approved(&self, name: &str) -> bool {
        self.approved.contains_key(name)
    }

    /// Partition a list of hook manifests into (already-approved, pending
    /// first-use approval). UI prompts the user once for the pending set;
    /// after approval each name moves to the approved side and is recorded.
    pub fn partition<'a, I>(&self, hooks: I) -> (Vec<&'a HookManifest>, Vec<&'a HookManifest>)
    where
        I: IntoIterator<Item = &'a HookManifest>,
    {
        let mut approved = Vec::new();
        let mut pending = Vec::new();
        for h in hooks {
            if self.is_approved(&h.name) {
                approved.push(h);
            } else {
                pending.push(h);
            }
        }
        (approved, pending)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("hook manifest parse error: {0}")]
    Parse(String),

    #[error("hook manifest at {path} failed to parse: {message}")]
    ParseAt { path: PathBuf, message: String },

    #[error("hook manifest version {got} != supported {expected}")]
    IncompatibleVersion { got: u32, expected: u32 },

    #[error("hook name {0:?} violates the manifest schema (^[a-z][a-z0-9_-]*$)")]
    InvalidName(String),

    #[error("time_budget_ms must be >= 1")]
    InvalidBudget,

    #[error("tool_filter is not supported for event {0:?}")]
    FilterNotSupportedForEvent(HookEvent),

    #[error("shell hook command must not be empty")]
    EmptyShellCommand,

    #[error("http hook url must not be empty")]
    EmptyHttpUrl,

    #[error("hook I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn parses_bundled_example_manifest() {
        let bytes = std::fs::read(
            // repo-relative path; tests run with cwd = crate dir, walk up to repo root.
            Path::new("../../examples/hooks/log_pre_tool.v1.json"),
        )
        .expect("example manifest readable");
        let m = HookManifest::from_json(&bytes).unwrap();
        assert_eq!(m.name, "log_pre_tool");
        assert_eq!(m.event, HookEvent::PreTool);
        assert_eq!(m.time_budget_ms, 50);
        match m.implementation {
            HookImplementation::Shell { command, .. } => {
                assert_eq!(command, "atelier-hook-log-pre-tool")
            }
            other => panic!("expected Shell, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_shell_manifest_through_json() {
        let m = HookManifest {
            version: 1,
            name: "lint".into(),
            description: Some("lint after every write".into()),
            event: HookEvent::PostTool,
            tool_filter: Some(vec!["write_file".into(), "edit_file".into()]),
            implementation: HookImplementation::Shell {
                command: "ruff check".into(),
                env: BTreeMap::new(),
            },
            time_budget_ms: 200,
            allow_net: false,
        };
        let json = serde_json::to_vec(&m).unwrap();
        let back = HookManifest::from_json(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn round_trips_http_manifest_through_json() {
        let mut headers = BTreeMap::new();
        headers.insert("X-Atelier".into(), "1".into());
        let m = HookManifest {
            version: 1,
            name: "audit".into(),
            description: None,
            event: HookEvent::PreTool,
            tool_filter: None,
            implementation: HookImplementation::Http {
                url: "https://example.com/audit".into(),
                headers,
            },
            time_budget_ms: 100,
            allow_net: true,
        };
        let json = serde_json::to_vec(&m).unwrap();
        let back = HookManifest::from_json(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn rejects_unsupported_manifest_version() {
        let json = r#"{
            "version": 2, "name": "x", "event": "pre-tool",
            "implementation": {"kind": "shell", "command": "echo"},
            "time_budget_ms": 50
        }"#;
        let err = HookManifest::from_json(json.as_bytes()).unwrap_err();
        assert!(matches!(err, HookError::IncompatibleVersion { got: 2, .. }));
    }

    #[test]
    fn rejects_names_not_matching_schema_regex() {
        for bad in ["Bad", "1leading", "with space", "punct!", ""] {
            let json = format!(
                r#"{{
                    "version": 1, "name": "{bad}", "event": "pre-tool",
                    "implementation": {{"kind": "shell", "command": "echo"}},
                    "time_budget_ms": 50
                }}"#
            );
            let err = HookManifest::from_json(json.as_bytes()).unwrap_err();
            assert!(
                matches!(err, HookError::InvalidName(_)),
                "expected InvalidName for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_zero_time_budget() {
        let json = r#"{
            "version": 1, "name": "x", "event": "pre-tool",
            "implementation": {"kind": "shell", "command": "echo"},
            "time_budget_ms": 0
        }"#;
        let err = HookManifest::from_json(json.as_bytes()).unwrap_err();
        assert!(matches!(err, HookError::InvalidBudget));
    }

    #[test]
    fn rejects_unknown_fields_via_serde() {
        let json = r#"{
            "version": 1, "name": "x", "event": "pre-tool",
            "implementation": {"kind": "shell", "command": "echo"},
            "time_budget_ms": 50,
            "unknown_field": true
        }"#;
        let err = HookManifest::from_json(json.as_bytes()).unwrap_err();
        assert!(matches!(err, HookError::Parse(_)));
    }

    #[test]
    fn empty_shell_command_is_rejected() {
        let json = r#"{
            "version": 1, "name": "x", "event": "pre-tool",
            "implementation": {"kind": "shell", "command": ""},
            "time_budget_ms": 50
        }"#;
        let err = HookManifest::from_json(json.as_bytes()).unwrap_err();
        assert!(matches!(err, HookError::EmptyShellCommand));
    }

    // ---------- HookSet loading ----------

    fn shell_manifest_json(name: &str, event: &str, command: &str) -> String {
        format!(
            r#"{{
                "version": 1,
                "name": "{name}",
                "event": "{event}",
                "implementation": {{"kind": "shell", "command": "{command}"}},
                "time_budget_ms": 50
            }}"#
        )
    }

    #[test]
    fn load_dir_reads_only_json_and_skips_underscored() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "a.json",
            &shell_manifest_json("a", "pre-tool", "echo a"),
        );
        write(
            dir.path(),
            "b.json",
            &shell_manifest_json("b", "post-tool", "echo b"),
        );
        write(dir.path(), "_approvals.json", "{}"); // approval store — skip
        write(dir.path(), "notes.md", "# README"); // non-json — skip

        let set = HookSet::load_dir(dir.path()).unwrap();
        assert_eq!(set.len(), 2);
        let names: Vec<&str> = set.names().collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn missing_hooks_dir_is_an_empty_set() {
        let dir = TempDir::new().unwrap();
        let set = HookSet::load_dir(&dir.path().join("does-not-exist")).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn merge_dir_overrides_same_name_with_repo_local_definition() {
        let global = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();
        write(
            global.path(),
            "lint.json",
            &shell_manifest_json("lint", "post-tool", "global"),
        );
        write(
            local.path(),
            "lint.json",
            &shell_manifest_json("lint", "post-tool", "local"),
        );

        let mut set = HookSet::load_dir(global.path()).unwrap();
        set.merge_dir(local.path()).unwrap();

        let hooks = set.for_event(HookEvent::PostTool);
        assert_eq!(hooks.len(), 1);
        match &hooks[0].implementation {
            HookImplementation::Shell { command, .. } => assert_eq!(command, "local"),
            _ => panic!("expected shell"),
        }
    }

    #[test]
    fn for_event_filters_by_event() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "pre.json",
            &shell_manifest_json("pre", "pre-tool", "echo"),
        );
        write(
            dir.path(),
            "verify.json",
            &shell_manifest_json("verify", "on-verify-pass", "echo"),
        );
        let set = HookSet::load_dir(dir.path()).unwrap();
        assert_eq!(set.for_event(HookEvent::PreTool).len(), 1);
        assert_eq!(set.for_event(HookEvent::OnVerifyPass).len(), 1);
        assert_eq!(set.for_event(HookEvent::PostTool).len(), 0);
    }

    #[test]
    fn for_tool_event_respects_tool_filter_globs() {
        let m_all = HookManifest {
            version: 1,
            name: "all".into(),
            description: None,
            event: HookEvent::PreTool,
            tool_filter: None,
            implementation: HookImplementation::Shell {
                command: "echo".into(),
                env: BTreeMap::new(),
            },
            time_budget_ms: 50,
            allow_net: false,
        };
        let m_write = HookManifest {
            name: "writes".into(),
            tool_filter: Some(vec!["write_*".into()]),
            ..m_all.clone()
        };
        let m_exact = HookManifest {
            name: "shell_only".into(),
            tool_filter: Some(vec!["shell".into()]),
            ..m_all.clone()
        };
        assert!(m_all.matches_tool("anything"));
        assert!(m_write.matches_tool("write_file"));
        assert!(!m_write.matches_tool("read_file"));
        assert!(m_exact.matches_tool("shell"));
        assert!(!m_exact.matches_tool("shell-extra"));
    }

    #[test]
    fn on_verify_events_never_match_a_tool() {
        let m = HookManifest {
            version: 1,
            name: "v".into(),
            description: None,
            event: HookEvent::OnVerifyPass,
            tool_filter: Some(vec!["*".into()]),
            implementation: HookImplementation::Shell {
                command: "echo".into(),
                env: BTreeMap::new(),
            },
            time_budget_ms: 50,
            allow_net: false,
        };
        let err = m.validate().unwrap_err();
        assert!(matches!(err, HookError::FilterNotSupportedForEvent(_)));
    }

    // ---------- Approval store ----------

    #[test]
    fn approval_store_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(APPROVALS_FILE);
        let mut approvals = HookApprovals::default();
        approvals.approve("lint", "2026-05-16T10:00:00Z");
        approvals.approve("audit", "2026-05-16T10:05:00Z");
        approvals.save(&path).unwrap();

        let loaded = HookApprovals::load(&path).unwrap();
        assert!(loaded.is_approved("lint"));
        assert!(loaded.is_approved("audit"));
        assert!(!loaded.is_approved("never"));
        assert_eq!(loaded.approved.get("lint").unwrap(), "2026-05-16T10:00:00Z");
    }

    #[test]
    fn approval_store_load_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let approvals = HookApprovals::load(&dir.path().join("nope.json")).unwrap();
        assert!(approvals.approved.is_empty());
    }

    #[test]
    fn approval_store_revoke_removes_entry() {
        let mut approvals = HookApprovals::default();
        approvals.approve("lint", "2026-05-16T10:00:00Z");
        assert!(approvals.is_approved("lint"));
        let removed = approvals.revoke("lint").unwrap();
        assert_eq!(removed, "2026-05-16T10:00:00Z");
        assert!(!approvals.is_approved("lint"));
        assert!(approvals.revoke("lint").is_none());
    }

    #[test]
    fn partition_splits_hooks_into_approved_and_pending() {
        let m = HookManifest {
            version: 1,
            name: "lint".into(),
            description: None,
            event: HookEvent::PostTool,
            tool_filter: None,
            implementation: HookImplementation::Shell {
                command: "echo".into(),
                env: BTreeMap::new(),
            },
            time_budget_ms: 50,
            allow_net: false,
        };
        let n = HookManifest {
            name: "audit".into(),
            ..m.clone()
        };
        let mut approvals = HookApprovals::default();
        approvals.approve("lint", "2026-05-16T10:00:00Z");

        let (ok, pending) = approvals.partition([&m, &n]);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].name, "lint");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "audit");
    }
}
