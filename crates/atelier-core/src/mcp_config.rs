//! §15 MCP server registration — `mcp_servers.json` loader + first-use
//! approval store.
//!
//! This is the **rmcp-free data layer**. It parses
//! `<workspace>/.atelier/mcp_servers.json` (per
//! `schemas/config/mcp_servers.v1.json`), validates each entry against the
//! schema, and exposes a per-repo persistent map of server-name → approval
//! timestamp. The actual MCP client (`rmcp`) plugs into this surface in a
//! later bundle: it walks the returned `Vec<McpServerManifest>`, asks the
//! `McpApprovals` which servers still need first-use approval, and launches
//! / connects the rest.
//!
//! Spec §15 (line 741): *"Server registration is a §8 trust-budget event on
//! first use."* — i.e. approval is at the *server* level. Granting trust to
//! a server grants it to every tool that server exposes (the per-tool
//! `side_effect_class` then governs each individual call, defaulting to the
//! server-level value declared here).
//!
//! Discovery path: `<workspace_root>/.atelier/mcp_servers.json`.
//! Approval store path: `<workspace_root>/.atelier/mcp_servers/_approvals.json`
//! (mirrors the §15 hooks approval-file convention — leading underscore so it
//! cannot collide with a user-chosen server name, since `name` must match
//! `^[a-z]…`).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jsonschema::Validator;
use serde::{Deserialize, Serialize};

use crate::persistence::PersistenceError;

/// Schema version expected by this build (`version: 1` in the manifest).
pub const MCP_SERVERS_VERSION: u32 = 1;

/// Filename for the on-disk manifest, under `<workspace>/.atelier/`.
pub const MCP_SERVERS_FILE: &str = "mcp_servers.json";

/// Per-repo directory that holds the first-use approval store. Picked to
/// match the §15 hooks convention (`<workspace>/.atelier/hooks/_approvals.json`):
/// approvals live next to the thing they approve.
pub const MCP_SERVERS_DIR: &str = "mcp_servers";

/// Filename for the first-use approval store. Leading `_` is deliberate:
/// the server-name regex `^[a-z][a-z0-9_-]*$` forbids a leading underscore,
/// so no user-chosen server name can ever collide.
pub const MCP_APPROVALS_FILE: &str = "_approvals.json";

/// Embedded copy of `schemas/config/mcp_servers.v1.json`. Embedding keeps
/// loader validation hermetic — the schema is part of the binary, so a user
/// running a built `atelier` outside the repo still gets schema-level
/// validation. Updated by hand when the schema is revised.
const MCP_SERVERS_SCHEMA_JSON: &str = include_str!("../../../schemas/config/mcp_servers.v1.json");

/// Lazily-compiled validator. `jsonschema::Validator` is `Send + Sync` and
/// reusable, so we compile once per process.
fn schema_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value = serde_json::from_str(MCP_SERVERS_SCHEMA_JSON)
            .expect("embedded mcp_servers.v1.json parses as JSON");
        jsonschema::validator_for(&schema)
            .expect("embedded mcp_servers.v1.json is a valid JSON Schema")
    })
}

// ---------- enums (kebab-case on the wire, mirroring the schema) ----------

/// Transport for an MCP server registration. Mirrors the
/// `transport` enum in `schemas/config/mcp_servers.v1.json` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Launch as a subprocess; communicate over stdio. Requires `command`.
    Stdio,
    /// Connect to a remote HTTP endpoint. Requires `url`. Counted as
    /// egress (§12).
    Http,
    /// Connect to a remote Server-Sent-Events endpoint. Requires `url`.
    /// Counted as egress (§12).
    Sse,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

/// Default trust-budget classification for all tools advertised by an MCP
/// server. Mirrors `crate::dispatcher::SideEffectClass` *exactly* (same wire
/// labels) so the dispatcher can ingest this value as-is once the rmcp
/// client lands.
///
/// We deliberately introduce a sibling type rather than re-using
/// `crate::dispatcher::SideEffectClass`: the dispatcher's value carries
/// trust-budget *cost* semantics; this one is a pure config field with
/// no behaviour attached, and they may evolve independently (per-tool
/// override is a tool-manifest concern, not a server-manifest one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffectClass {
    /// Read-only or contained-to-temp.
    LocalSafe,
    /// Writes inside the repo.
    LocalRisky,
    /// Affects shared state outside the workspace.
    SharedState,
    /// Irreversible side effect.
    Irreversible,
}

impl SideEffectClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalSafe => "local-safe",
            Self::LocalRisky => "local-risky",
            Self::SharedState => "shared-state",
            Self::Irreversible => "irreversible",
        }
    }
}

// ---------- typed manifest ----------

/// On-disk top-level shape. Mirrors `schemas/config/mcp_servers.v1.json`
/// exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct McpServersDoc {
    version: u32,
    servers: Vec<McpServerManifest>,
}

/// A single MCP server registration. Mirrors `schemas/config/mcp_servers.v1.json`
/// — every field present in the schema lives here. Fields added in a future
/// (additive) schema revision should land here with `#[serde(default)]` so
/// older Rust builds keep parsing newer files.
///
/// Validation invariants beyond what serde enforces (name regex, transport
/// requires `command` vs `url`, etc.) are checked at load time by the
/// embedded JSON Schema validator. `[`load_mcp_servers`]` rejects any file
/// that fails those checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpServerManifest {
    /// Unique identifier within the session. Must match `^[a-z][a-z0-9_-]*$`
    /// (enforced by the schema). Surfaces in the trust-budget UI and the
    /// egress audit log.
    pub name: String,

    /// Transport class — drives which of (`command`/`args`/`env`) vs
    /// (`url`/`headers`) are required.
    pub transport: Transport,

    /// Shell command for `stdio` transport (e.g.
    /// `npx @modelcontextprotocol/server-filesystem`). Required when
    /// `transport == Stdio`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Arguments to `command` (stdio only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Environment variables for the subprocess (stdio only). Values
    /// support `${env:NAME}` and `${keychain:NAME}` interpolation per
    /// §11 credential storage.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    /// Endpoint URL for `http`/`sse` transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// HTTP headers for `http`/`sse` transport. Same interpolation rules as
    /// `env`. Plaintext literals accepted but discouraged for anything
    /// resembling a secret. `Authorization` headers are redacted in the
    /// §12 egress audit log.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Default §8 trust-budget classification applied to all tools
    /// advertised by this server. Per-tool overrides allowed in the tool
    /// manifest (handled by the rmcp client when it lands).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect_class: Option<SideEffectClass>,

    /// stdio transport only: if true, the subprocess sandbox permits
    /// outbound network. http/sse transports imply network; this flag is
    /// ignored for them.
    #[serde(default)]
    pub allow_net: bool,

    /// v60.28 H5 — http/sse transport: hostnames the launcher is
    /// allowed to dispatch to on this server's behalf. `None` means
    /// "default to [host(url)]" when first resolved. Reject any
    /// per-call URL whose host doesn't match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_hosts: Option<Vec<String>>,

    /// Set to `false` to keep the registration in the file but skip
    /// launching the server. The loader filters disabled entries out of
    /// the returned `Vec` so callers don't need to remember to gate on it.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl McpServerManifest {
    /// `true` iff `transport ∈ {Http, Sse}` — convenience used by the rmcp
    /// client (future bundle) to decide whether this server counts as a
    /// §12 egress target.
    pub fn is_remote(&self) -> bool {
        matches!(self.transport, Transport::Http | Transport::Sse)
    }
}

// ---------- env / header interpolation ----------

/// Resolve `${env:NAME}` tokens against the current environment. Plaintext
/// values pass through unchanged.
///
/// `${keychain:NAME}` is reserved for future OS keychain integration and
/// fails closed with `McpConfigError::KeychainNotYet` so callers see a typed
/// error instead of a silent empty string.
pub fn interpolate(value: &str) -> Result<String, McpConfigError> {
    // We scan the string left-to-right looking for `${env:NAME}` /
    // `${keychain:NAME}`. Anything else (including stray `$`, `${`, or
    // mismatched braces) is passed through literally — same lenient
    // semantics the v52 shell-style interpolation used. Unknown prefixes
    // (`${foo:NAME}` for a prefix other than `env` / `keychain`) are
    // rejected loudly: silently passing them through would mask typos
    // like `${keyring:…}`.
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated `${` — pass the literal `${…` through.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let token = &after[..end];
        let resolved = resolve_token(token)?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve_token(token: &str) -> Result<String, McpConfigError> {
    if let Some(name) = token.strip_prefix("env:") {
        return std::env::var(name).map_err(|_| McpConfigError::EnvVarUnset(name.to_string()));
    }
    if let Some(name) = token.strip_prefix("keychain:") {
        return Err(McpConfigError::KeychainNotYet(name.to_string()));
    }
    Err(McpConfigError::UnknownInterpolation(token.to_string()))
}

// ---------- loader ----------

/// Read `<workspace_root>/.atelier/mcp_servers.json` and return the list of
/// **enabled** server registrations.
///
///   - Missing file → `Ok(Vec::new())`. A fresh repo with no MCP config is
///     a valid state, not an error.
///   - Schema-invalid file → `Err(McpConfigError::SchemaViolation { … })`
///     listing every validator error. Atomic — partial loads are not
///     surfaced; either the whole file validates or none of it is
///     returned.
///   - Duplicate `name` → `Err(McpConfigError::DuplicateName(name))`.
///   - `enabled: false` entries are dropped from the returned vec (they
///     stay on disk; the loader just filters them out so the rmcp client
///     doesn't have to remember to gate every iteration on the flag).
pub fn load_mcp_servers(workspace_root: &Path) -> Result<Vec<McpServerManifest>, McpConfigError> {
    let path = workspace_root.join(".atelier").join(MCP_SERVERS_FILE);
    // v60.37 A2 — cap at 1 MiB so a pathological mcp_servers.json
    // can't OOM the agent at startup.
    let bytes = match crate::io_caps::read_capped(&path, crate::io_caps::CAP_MCP_CONFIG) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(McpConfigError::Io { path, source: e }),
    };
    parse_mcp_servers(&bytes, &path)
}

/// Parse-and-validate a `mcp_servers.json` byte blob. Split out for test
/// callers that want to assert on validation behaviour without writing a
/// tempdir.
pub fn parse_mcp_servers(
    bytes: &[u8],
    source: &Path,
) -> Result<Vec<McpServerManifest>, McpConfigError> {
    // First parse as `serde_json::Value` so the schema validator has
    // something to walk; then parse again into the typed shape after
    // schema validation passes. Doing the schema pass first means a
    // typo in an enum value surfaces as a schema error (precise path
    // through the document) rather than a serde "unknown variant"
    // (location-less).
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| McpConfigError::Parse {
            path: source.to_path_buf(),
            message: e.to_string(),
        })?;

    let validator = schema_validator();
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|e| e.to_string())
        .collect();
    if !errors.is_empty() {
        return Err(McpConfigError::SchemaViolation {
            path: source.to_path_buf(),
            errors,
        });
    }

    let doc: McpServersDoc = serde_json::from_value(value).map_err(|e| McpConfigError::Parse {
        path: source.to_path_buf(),
        message: e.to_string(),
    })?;

    if doc.version != MCP_SERVERS_VERSION {
        return Err(McpConfigError::IncompatibleVersion {
            path: source.to_path_buf(),
            got: doc.version,
            expected: MCP_SERVERS_VERSION,
        });
    }

    // Schema enforces shape; we still need duplicate-name detection (JSON
    // Schema can't express "unique by .name within an array" without
    // `uniqueItems` of the *whole object*, which would let two servers
    // differing only in `enabled` coexist).
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    for s in &doc.servers {
        if seen.insert(s.name.clone(), ()).is_some() {
            return Err(McpConfigError::DuplicateName(s.name.clone()));
        }
    }

    Ok(doc.servers.into_iter().filter(|s| s.enabled).collect())
}

// ---------- first-use approval store ----------

/// Per-repo first-use approval store. Mirrors [`crate::hooks::HookApprovals`]
/// in shape and durability semantics:
///
///   - JSON file on disk, written atomically via `NamedTempFile::persist`.
///   - Map of `name → approval_timestamp` (RFC 3339, stringly typed to
///     match `OnDiskSession::created_at`).
///   - `is_approved` / `approve` / `revoke` are pure in-memory ops; `save`
///     persists.
///
/// Spec §15 (line 593): approval is at the *server* level. Granting trust
/// to a server grants it to all that server's tools. Per-tool
/// `side_effect_class` then governs each individual call.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpApprovals {
    #[serde(default)]
    pub approved: BTreeMap<String, String>,
}

impl McpApprovals {
    /// Load the approvals file. Missing file → empty store (the common
    /// case on first run); malformed file → typed error.
    pub fn load(path: &Path) -> Result<Self, PersistenceError> {
        // v60.37 A2 — cap at 1 MiB so a pathological approvals file
        // can't OOM the agent at startup.
        match crate::io_caps::read_capped(path, crate::io_caps::CAP_MCP_CONFIG) {
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

    /// Atomically write the approvals file. Creates parent dirs as needed
    /// — the `<workspace>/.atelier/mcp_servers/` directory does not exist
    /// in a fresh repo.
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
        // v60.37 A1 — full atomic-write discipline: data + metadata
        // fsync, then atomic rename, then parent-dir fsync. Without
        // sync_all + fsync_dir, a power loss between persist() and the
        // next natural fs sync can leave the directory entry in its
        // pre-rename state on stable storage.
        tmp.as_file().sync_all().map_err(|e| PersistenceError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.persist(path).map_err(|e| PersistenceError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
        crate::path_safety::fsync_dir(parent).map_err(|e| PersistenceError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        Ok(())
    }

    /// Mark `name` approved at `granted_at` (RFC 3339 string). Idempotent
    /// — re-approving overwrites the timestamp.
    pub fn approve(&mut self, name: impl Into<String>, granted_at: impl Into<String>) {
        self.approved.insert(name.into(), granted_at.into());
    }

    /// Drop an approval. Returns the previous timestamp if there was one.
    pub fn revoke(&mut self, name: &str) -> Option<String> {
        self.approved.remove(name)
    }

    pub fn is_approved(&self, name: &str) -> bool {
        self.approved.contains_key(name)
    }

    /// Return the subset of `loaded` that has not yet been approved — the
    /// list the UI shows the user on first run. The borrow is on the
    /// caller's slice so we don't allocate a copy of the manifests.
    pub fn pending<'a>(&self, loaded: &'a [McpServerManifest]) -> Vec<&'a McpServerManifest> {
        loaded
            .iter()
            .filter(|s| !self.is_approved(&s.name))
            .collect()
    }
}

/// Conventional location of the approvals store under a workspace root:
/// `<workspace>/.atelier/mcp_servers/_approvals.json`.
pub fn approvals_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".atelier")
        .join(MCP_SERVERS_DIR)
        .join(MCP_APPROVALS_FILE)
}

// ---------- errors ----------

#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("I/O failure reading mcp_servers.json at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("mcp_servers.json at {path} is not valid JSON: {message}")]
    Parse { path: PathBuf, message: String },

    #[error(
        "mcp_servers.json at {path} fails schema validation: {}",
        errors.join("; ")
    )]
    SchemaViolation { path: PathBuf, errors: Vec<String> },

    #[error("mcp_servers.json at {path} uses version {got}, this build expects {expected}")]
    IncompatibleVersion {
        path: PathBuf,
        got: u32,
        expected: u32,
    },

    #[error("duplicate server name in mcp_servers.json: {0:?}")]
    DuplicateName(String),

    #[error("environment variable not set: {0}")]
    EnvVarUnset(String),

    #[error(
        "keychain interpolation lands with the rmcp client; cannot resolve ${{keychain:{0}}} yet"
    )]
    KeychainNotYet(String),

    #[error(
        "unknown interpolation prefix in token {0:?}; supported prefixes are `env:` and `keychain:`"
    )]
    UnknownInterpolation(String),
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(repo: &Path, body: &str) -> PathBuf {
        let dir = repo.join(".atelier");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MCP_SERVERS_FILE);
        std::fs::write(&path, body).unwrap();
        path
    }

    // ---------- enum wire labels ----------

    #[test]
    fn transport_serde_round_trip() {
        for (label, value) in [
            ("stdio", Transport::Stdio),
            ("http", Transport::Http),
            ("sse", Transport::Sse),
        ] {
            assert_eq!(value.as_str(), label);
            let json = serde_json::to_value(value).unwrap();
            assert_eq!(json.as_str(), Some(label));
            let back: Transport = serde_json::from_value(json).unwrap();
            assert_eq!(back, value);
        }
    }

    #[test]
    fn side_effect_class_serde_round_trip() {
        for (label, value) in [
            ("local-safe", SideEffectClass::LocalSafe),
            ("local-risky", SideEffectClass::LocalRisky),
            ("shared-state", SideEffectClass::SharedState),
            ("irreversible", SideEffectClass::Irreversible),
        ] {
            assert_eq!(value.as_str(), label);
            let json = serde_json::to_value(value).unwrap();
            assert_eq!(json.as_str(), Some(label));
            let back: SideEffectClass = serde_json::from_value(json).unwrap();
            assert_eq!(back, value);
        }
    }

    // ---------- happy paths ----------

    #[test]
    fn load_missing_file_returns_empty_vec() {
        let tmp = TempDir::new().unwrap();
        let loaded = load_mcp_servers(tmp.path()).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn loads_filesystem_stdio_example_from_schema() {
        // This is the example from the schema's `examples` block,
        // verbatim. If the schema's example doesn't load, the schema
        // and the loader have drifted.
        let body = r#"{
            "version": 1,
            "servers": [
                {
                    "name": "filesystem",
                    "transport": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/projects/myrepo"],
                    "side_effect_class": "local-risky"
                }
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        let loaded = load_mcp_servers(tmp.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        let s = &loaded[0];
        assert_eq!(s.name, "filesystem");
        assert_eq!(s.transport, Transport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(s.args.len(), 3);
        assert_eq!(s.side_effect_class, Some(SideEffectClass::LocalRisky));
        assert!(s.enabled);
        assert!(!s.allow_net);
    }

    #[test]
    fn loads_http_example_from_schema() {
        let body = r#"{
            "version": 1,
            "servers": [
                {
                    "name": "websearch",
                    "transport": "http",
                    "url": "https://search.example/mcp",
                    "allow_net": true,
                    "headers": {"Authorization": "Bearer ${env:WEBSEARCH_TOKEN}"},
                    "side_effect_class": "shared-state"
                }
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        let loaded = load_mcp_servers(tmp.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        let s = &loaded[0];
        assert_eq!(s.transport, Transport::Http);
        assert!(s.is_remote());
        assert_eq!(s.url.as_deref(), Some("https://search.example/mcp"));
        assert_eq!(
            s.headers.get("Authorization").map(String::as_str),
            Some("Bearer ${env:WEBSEARCH_TOKEN}")
        );
    }

    #[test]
    fn disabled_entries_are_filtered_out() {
        let body = r#"{
            "version": 1,
            "servers": [
                {"name": "live", "transport": "stdio", "command": "echo"},
                {"name": "off", "transport": "stdio", "command": "echo", "enabled": false}
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        let loaded = load_mcp_servers(tmp.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "live");
    }

    // ---------- validation ----------

    #[test]
    fn duplicate_server_names_rejected() {
        let body = r#"{
            "version": 1,
            "servers": [
                {"name": "fs", "transport": "stdio", "command": "a"},
                {"name": "fs", "transport": "stdio", "command": "b"}
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        match load_mcp_servers(tmp.path()).unwrap_err() {
            McpConfigError::DuplicateName(n) => assert_eq!(n, "fs"),
            other => panic!("expected DuplicateName, got {other:?}"),
        }
    }

    #[test]
    fn stdio_without_command_is_rejected() {
        // Schema: `if transport == "stdio" then required: ["command"]`.
        let body = r#"{
            "version": 1,
            "servers": [
                {"name": "fs", "transport": "stdio"}
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        match load_mcp_servers(tmp.path()).unwrap_err() {
            McpConfigError::SchemaViolation { errors, .. } => {
                assert!(
                    errors.iter().any(|e| e.contains("command")),
                    "errors should mention `command`, got {errors:?}"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn http_without_url_is_rejected() {
        // Schema: `if transport ∈ {http, sse} then required: ["url"]`.
        let body = r#"{
            "version": 1,
            "servers": [
                {"name": "ws", "transport": "http"}
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        match load_mcp_servers(tmp.path()).unwrap_err() {
            McpConfigError::SchemaViolation { errors, .. } => {
                assert!(
                    errors.iter().any(|e| e.contains("url")),
                    "errors should mention `url`, got {errors:?}"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn invalid_server_name_is_rejected() {
        // Schema: `name` must match `^[a-z][a-z0-9_-]*$`.
        let body = r#"{
            "version": 1,
            "servers": [
                {"name": "Bad-Name", "transport": "stdio", "command": "x"}
            ]
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        match load_mcp_servers(tmp.path()).unwrap_err() {
            McpConfigError::SchemaViolation { .. } => {}
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        // Schema: `additionalProperties: false` at the top level.
        let body = r#"{
            "version": 1,
            "servers": [],
            "garbage": 1
        }"#;
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), body);
        match load_mcp_servers(tmp.path()).unwrap_err() {
            McpConfigError::SchemaViolation { .. } => {}
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "{not json");
        match load_mcp_servers(tmp.path()).unwrap_err() {
            McpConfigError::Parse { .. } => {}
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    // ---------- interpolation ----------

    #[test]
    fn interpolate_env_token() {
        // SAFETY: per-process env mutation. Variable name is sufficiently
        // unique that no parallel test should touch it.
        let var = "ATELIER_TEST_INTERPOLATE_ENV";
        unsafe { std::env::set_var(var, "hello") };
        let out = interpolate(&format!("Bearer ${{env:{var}}} world")).unwrap();
        unsafe { std::env::remove_var(var) };
        assert_eq!(out, "Bearer hello world");
    }

    #[test]
    fn interpolate_missing_env_token_errors() {
        let var = "ATELIER_TEST_INTERPOLATE_MISSING";
        // ensure it isn't accidentally set
        unsafe { std::env::remove_var(var) };
        let err = interpolate(&format!("${{env:{var}}}")).unwrap_err();
        match err {
            McpConfigError::EnvVarUnset(n) => assert_eq!(n, var),
            other => panic!("expected EnvVarUnset, got {other:?}"),
        }
    }

    #[test]
    fn interpolate_keychain_token_is_deferred() {
        let err = interpolate("${keychain:WEBSEARCH_TOKEN}").unwrap_err();
        match err {
            McpConfigError::KeychainNotYet(n) => assert_eq!(n, "WEBSEARCH_TOKEN"),
            other => panic!("expected KeychainNotYet, got {other:?}"),
        }
    }

    #[test]
    fn interpolate_unknown_prefix_errors() {
        let err = interpolate("${keyring:NAME}").unwrap_err();
        match err {
            McpConfigError::UnknownInterpolation(t) => assert_eq!(t, "keyring:NAME"),
            other => panic!("expected UnknownInterpolation, got {other:?}"),
        }
    }

    #[test]
    fn interpolate_plaintext_passes_through() {
        let s = "no tokens here, just text";
        assert_eq!(interpolate(s).unwrap(), s);
    }

    #[test]
    fn interpolate_unterminated_brace_passes_through() {
        // A literal `${` with no closing `}` is left in place rather than
        // erroring — interpolation is lenient on shape, strict on values.
        let s = "literal ${not-a-token";
        assert_eq!(interpolate(s).unwrap(), s);
    }

    // ---------- approval store ----------

    #[test]
    fn approvals_round_trip_through_serde() {
        let tmp = TempDir::new().unwrap();
        let path = approvals_path(tmp.path());
        let mut a = McpApprovals::default();
        a.approve("filesystem", "2026-05-18T10:00:00Z");
        a.approve("websearch", "2026-05-18T10:05:00Z");
        a.save(&path).unwrap();
        let back = McpApprovals::load(&path).unwrap();
        assert_eq!(back, a);
        assert!(path.ends_with(".atelier/mcp_servers/_approvals.json"));
    }

    #[test]
    fn approvals_load_missing_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        let path = approvals_path(tmp.path());
        let loaded = McpApprovals::load(&path).unwrap();
        assert!(loaded.approved.is_empty());
    }

    #[test]
    fn approve_is_idempotent_and_revoke_clears() {
        let mut a = McpApprovals::default();
        a.approve("fs", "2026-05-18T10:00:00Z");
        a.approve("fs", "2026-05-18T11:00:00Z");
        assert_eq!(
            a.approved.get("fs").map(String::as_str),
            Some("2026-05-18T11:00:00Z")
        );
        assert!(a.is_approved("fs"));
        assert_eq!(a.revoke("fs"), Some("2026-05-18T11:00:00Z".to_string()));
        assert!(!a.is_approved("fs"));
        assert!(a.revoke("fs").is_none());
    }

    #[test]
    fn pending_returns_only_unapproved_servers() {
        let loaded = vec![
            McpServerManifest {
                name: "approved".into(),
                transport: Transport::Stdio,
                command: Some("a".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                side_effect_class: None,
                allow_net: false,
                allowed_hosts: None,
                enabled: true,
            },
            McpServerManifest {
                name: "fresh".into(),
                transport: Transport::Stdio,
                command: Some("b".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                side_effect_class: None,
                allow_net: false,
                allowed_hosts: None,
                enabled: true,
            },
        ];
        let mut a = McpApprovals::default();
        a.approve("approved", "2026-05-18T10:00:00Z");
        let pending = a.pending(&loaded);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "fresh");
    }

    #[test]
    fn manifest_round_trip_through_loader() {
        // serialize a manifest, write it to disk, re-load, compare.
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("PORT".into(), "8080".into());
        let original = McpServerManifest {
            name: "round-trip".into(),
            transport: Transport::Stdio,
            command: Some("server".into()),
            args: vec!["--port".into(), "8080".into()],
            env,
            url: None,
            headers: BTreeMap::new(),
            side_effect_class: Some(SideEffectClass::LocalSafe),
            allow_net: true,
            allowed_hosts: None,
            enabled: true,
        };
        let doc = McpServersDoc {
            version: 1,
            servers: vec![original.clone()],
        };
        let body = serde_json::to_string_pretty(&doc).unwrap();
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), &body);
        let loaded = load_mcp_servers(tmp.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], original);
    }
}
