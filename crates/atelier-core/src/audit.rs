//! §11 + §12 — egress audit log for subprocess tool calls.
//!
//! Spec §11 "Acceptance gate (mechanical)":
//!   > Model attempts `curl evil.example`; blocked; attempt logged.
//!
//! Spec §12 "Privacy":
//!   > Egress audit log exportable per `schemas/audit/egress.v1.json`.
//!
//! The existing `schemas/audit/egress.v1.json` shape is keyed on **remote
//! model calls** (provider + model_id + content_hash + token counts). That
//! shape doesn't fit a *subprocess* egress attempt (`shell` tool running
//! `curl https://evil.example`), so this module emits a separate record
//! type conforming to `schemas/audit/subprocess_egress.v1.json`. Both kinds
//! of audit entry share the same on-disk file
//! (`<workspace>/.atelier/sessions/<sid>/audit.log`, NDJSON), discriminated
//! by the `kind` field — model-call rows carry `"kind": "model-call"` and
//! subprocess rows carry `"kind": "subprocess-egress"`. Today only the
//! subprocess variant is written by this crate; the model-call producer
//! lands when the §12 redaction layer ships.
//!
//! ## Contract
//!
//! * **One line per event.** `append_subprocess_egress` opens in
//!   append-mode, writes a single `\n`-terminated JSON row, and flushes.
//!   Concurrent appends from two `shell` tool calls in the same session
//!   are safe: NDJSON has no inter-line dependency, and POSIX append-mode
//!   `write(2)` is atomic for writes ≤ `PIPE_BUF` (4 KiB everywhere we
//!   ship). Per-row payload is well under that bound.
//!
//! * **Schema discipline.** The fields here mirror
//!   `schemas/audit/subprocess_egress.v1.json` 1:1. A `wire_label_*` test
//!   in the unit-test block round-trips a known good record through
//!   `serde_json` so the in-Rust shape and the on-disk schema can't drift.
//!
//! * **Never propagates.** Append failures (disk full, perms) are logged
//!   via `tracing::warn!` but never error back to the caller. Spec §11
//!   blocks the egress; the audit row is a secondary record. We'd rather
//!   block the egress without a row than fail the dispatch because the
//!   audit log is unwritable.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One row of `audit.log` describing a blocked (or attempted) subprocess
/// egress. Conforms to `schemas/audit/subprocess_egress.v1.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressEvent {
    /// Schema version. Locked to `1`. Bump in lockstep with the JSON
    /// Schema's `const`.
    pub version: u32,
    /// Discriminator shared with future model-call rows. Always
    /// `"subprocess-egress"` for instances built by this module.
    pub kind: String,
    /// RFC 3339 timestamp at the moment the egress attempt was
    /// detected. Caller-supplied so tests can pin a deterministic value.
    pub timestamp: String,
    /// ID of the tool call that produced the attempt. Matches the
    /// `ToolCallRequest::id` the model emitted, which is the same id
    /// that appears on the `DispatchOutcome` and in the persisted
    /// `session.json`'s conversation log.
    pub tool_call_id: String,
    /// Name of the originating tool (`"shell"` in the only producer
    /// shipping today, but kept generic so a future MCP-routed tool can
    /// reuse the same audit shape).
    pub tool_name: String,
    /// Host + optional port the subprocess tried to reach. Parsed out
    /// of the command string. We deliberately do NOT log the full
    /// command — that often carries query strings + bearer tokens.
    pub destination: String,
    /// Outcome of the harness's enforcement. Today always `"blocked"`;
    /// reserved for a future `"allowed"` once the policy gains an
    /// allow-list mode.
    pub outcome: String,
    /// Why the enforcement fired. Today always `"sandbox-deny-net"`
    /// (the §11 default policy says `allow_net: false`).
    pub reason: String,
}

impl EgressEvent {
    /// Build a `kind = "subprocess-egress"` event for the most common
    /// case: a `shell` tool call that the harness refused to dispatch
    /// because the sandbox policy forbade network egress.
    pub fn blocked_subprocess_egress(
        timestamp: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        destination: impl Into<String>,
    ) -> Self {
        Self {
            version: 1,
            kind: "subprocess-egress".to_string(),
            timestamp: timestamp.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            destination: destination.into(),
            outcome: "blocked".to_string(),
            reason: "sandbox-deny-net".to_string(),
        }
    }
}

/// One row of `audit.log` describing an outbound HTTP/SSE request to a
/// remote MCP server. Conforms to `schemas/audit/mcp_egress.v1.json`. A
/// sibling of [`EgressEvent`]: same file, different `kind`. Spec §12 line
/// 656: *"MCP HTTP/SSE servers count as egress targets and are logged the
/// same way as LLM providers."*
///
/// Header redaction posture: the *caller* is responsible for never
/// constructing this struct with a header-bearing field. The shape itself
/// has no `headers` member — there is no on-disk representation of the
/// `Authorization` value, full stop. URLs are recorded verbatim because
/// they are routing data; secrets-in-URL is an anti-pattern handled by the
/// §12 redaction layer (out of scope here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpEgressEvent {
    /// Schema version. Locked to `1`.
    pub version: u32,
    /// Discriminator shared with `EgressEvent`'s `kind` field. Always
    /// `"mcp-http-request"` for instances built by this module.
    pub kind: String,
    /// RFC 3339 timestamp at the moment the request was initiated.
    pub timestamp: String,
    /// MCP server name (`manifest.name`). Per spec §12, "the `provider`
    /// field on the audit record carries the MCP server name."
    pub provider: String,
    /// Full endpoint URL. Query strings + fragments retained.
    pub url: String,
    /// Lifecycle phase that produced the request (handshake / list-tools /
    /// call-tool / shutdown).
    pub phase: String,
    /// Outcome from the harness's perspective.
    pub outcome: String,
    /// Free-form diagnostic note; omitted for `success` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Tool name for `phase == "call-tool"` rows. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// Lifecycle phase enum surfaced as a string in the `phase` audit field.
/// Kept as an enum (rather than free `&str`) so the producer can't drift
/// the wire-label set from the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpEgressPhase {
    Handshake,
    ListTools,
    CallTool,
    Shutdown,
}

impl McpEgressPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::ListTools => "list-tools",
            Self::CallTool => "call-tool",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Outcome enum surfaced as the `outcome` audit field. Schema-pinned to
/// `success | failure | blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpEgressOutcome {
    Success,
    Failure,
    Blocked,
}

impl McpEgressOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Blocked => "blocked",
        }
    }
}

impl McpEgressEvent {
    /// Helper for the common builder shape: phase + outcome enums, optional
    /// reason + tool name. Callers in production code construct via this
    /// to guarantee a schema-conformant `kind` / `version`.
    pub fn new(
        timestamp: impl Into<String>,
        provider: impl Into<String>,
        url: impl Into<String>,
        phase: McpEgressPhase,
        outcome: McpEgressOutcome,
        reason: Option<String>,
        tool_name: Option<String>,
    ) -> Self {
        Self {
            version: 1,
            kind: "mcp-http-request".to_string(),
            timestamp: timestamp.into(),
            provider: provider.into(),
            url: url.into(),
            phase: phase.as_str().to_string(),
            outcome: outcome.as_str().to_string(),
            reason,
            tool_name,
        }
    }
}

/// Audit-log error surface. Today only `Io` is reachable; reserved for a
/// future JSON-encoding failure if the event struct grows a fallible
/// field. Callers don't propagate — see module docs — but the typed
/// shape makes the `tracing::warn!` site honest about what went wrong.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit log I/O at {path:?}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("audit log serialize: {0}")]
    Serialize(String),
}

/// Synchronisation primitive for in-process concurrent writers. POSIX
/// append-mode `write(2)` is atomic for writes ≤ `PIPE_BUF`, so we'd be
/// safe across processes too — but tokio-spawned dispatch tasks within a
/// single process all hit the same file descriptor, and the small mutex
/// keeps the line ordering deterministic for tests.
static APPEND_LOCK: Mutex<()> = Mutex::new(());

/// Append one [`EgressEvent`] as a single `\n`-terminated JSON line to
/// `path`. Creates `path` (and its parent directory) if absent.
///
/// Returns `Ok(())` on success and `Err(AuditError)` on I/O or
/// serialization failure. Callers in production code log the error and
/// continue (the egress is still blocked); the typed return is for the
/// integration test which asserts the on-disk shape.
pub fn append_subprocess_egress(path: &Path, event: &EgressEvent) -> Result<(), AuditError> {
    let mut line =
        serde_json::to_string(event).map_err(|e| AuditError::Serialize(e.to_string()))?;
    line.push('\n');

    let _guard = APPEND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| AuditError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
    }

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| AuditError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    f.write_all(line.as_bytes()).map_err(|e| AuditError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    f.flush().map_err(|e| AuditError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Append one [`McpEgressEvent`] as a single `\n`-terminated JSON line to
/// `path`. Same on-disk contract as [`append_subprocess_egress`] —
/// NDJSON, append-mode, parent dirs created on demand. Producer is the
/// §15 HTTP/SSE MCP launcher; rows for stdio launches are NOT emitted
/// (stdio is the local-only path, no egress).
pub fn append_mcp_egress(path: &Path, event: &McpEgressEvent) -> Result<(), AuditError> {
    let mut line =
        serde_json::to_string(event).map_err(|e| AuditError::Serialize(e.to_string()))?;
    line.push('\n');

    let _guard = APPEND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| AuditError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
    }

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| AuditError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    f.write_all(line.as_bytes()).map_err(|e| AuditError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    f.flush().map_err(|e| AuditError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_event_has_locked_version_and_kind() {
        let e = EgressEvent::blocked_subprocess_egress(
            "2026-05-17T10:00:00Z",
            "tc-1",
            "shell",
            "evil.example",
        );
        assert_eq!(e.version, 1);
        assert_eq!(e.kind, "subprocess-egress");
        assert_eq!(e.outcome, "blocked");
        assert_eq!(e.reason, "sandbox-deny-net");
        assert_eq!(e.destination, "evil.example");
        assert_eq!(e.tool_call_id, "tc-1");
        assert_eq!(e.tool_name, "shell");
        assert_eq!(e.timestamp, "2026-05-17T10:00:00Z");
    }

    #[test]
    fn round_trip_matches_schema_field_set() {
        // Pin the wire shape: any rename / removal of a serde field
        // here is a schema change requiring a coordinated bump.
        let e = EgressEvent::blocked_subprocess_egress(
            "2026-05-17T10:00:00Z",
            "tc-1",
            "shell",
            "evil.example",
        );
        let v = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "destination",
                "kind",
                "outcome",
                "reason",
                "timestamp",
                "tool_call_id",
                "tool_name",
                "version",
            ]
        );
    }

    #[test]
    fn append_writes_one_ndjson_line_per_event() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sub").join("audit.log");
        let e = EgressEvent::blocked_subprocess_egress(
            "2026-05-17T10:00:00Z",
            "tc-1",
            "shell",
            "evil.example",
        );
        append_subprocess_egress(&path, &e).unwrap();
        append_subprocess_egress(&path, &e).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        // Each event lands on its own line; the file is exactly two
        // lines (terminating newlines included) — no extra padding,
        // no rewriting.
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line is parseable JSON.
        for l in &lines {
            let parsed: EgressEvent = serde_json::from_str(l).unwrap();
            assert_eq!(parsed.version, 1);
            assert_eq!(parsed.destination, "evil.example");
        }
    }

    #[test]
    fn append_creates_missing_parent_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        // Two levels of missing parents — matches a fresh session dir
        // before the runner has written `session.json`.
        let path = dir.path().join("a").join("b").join("audit.log");
        let e = EgressEvent::blocked_subprocess_egress("t", "tc", "shell", "host");
        append_subprocess_egress(&path, &e).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn mcp_egress_event_has_locked_version_and_kind() {
        let e = McpEgressEvent::new(
            "2026-05-17T10:00:00Z",
            "search-mcp",
            "https://search.example/mcp",
            McpEgressPhase::Handshake,
            McpEgressOutcome::Success,
            None,
            None,
        );
        assert_eq!(e.version, 1);
        assert_eq!(e.kind, "mcp-http-request");
        assert_eq!(e.phase, "handshake");
        assert_eq!(e.outcome, "success");
        assert_eq!(e.provider, "search-mcp");
    }

    #[test]
    fn mcp_egress_event_round_trip_schema_field_set() {
        // Pin the wire shape: `headers` MUST NOT appear on the value
        // (the launcher strips it before construction). Round-trip
        // ensures serde drops `None`-typed optional fields.
        let e = McpEgressEvent::new(
            "2026-05-17T10:00:00Z",
            "search-mcp",
            "https://search.example/mcp",
            McpEgressPhase::CallTool,
            McpEgressOutcome::Failure,
            Some("http-status".into()),
            Some("search".into()),
        );
        let v = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("headers"), "headers must never appear");
        // sanity check: required keys all present
        for k in [
            "version",
            "kind",
            "timestamp",
            "provider",
            "url",
            "phase",
            "outcome",
        ] {
            assert!(obj.contains_key(k), "missing required key {k}");
        }
        // optional keys included when populated
        assert!(obj.contains_key("reason"));
        assert!(obj.contains_key("tool_name"));
    }

    #[test]
    fn mcp_egress_omits_optional_fields_when_none() {
        let e = McpEgressEvent::new(
            "2026-05-17T10:00:00Z",
            "search-mcp",
            "https://search.example/mcp",
            McpEgressPhase::ListTools,
            McpEgressOutcome::Success,
            None,
            None,
        );
        let v = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("reason"));
        assert!(!obj.contains_key("tool_name"));
    }

    #[test]
    fn append_mcp_egress_writes_ndjson_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("audit.log");
        let e1 = McpEgressEvent::new(
            "2026-05-17T10:00:00Z",
            "search-mcp",
            "https://search.example/mcp",
            McpEgressPhase::Handshake,
            McpEgressOutcome::Success,
            None,
            None,
        );
        let e2 = McpEgressEvent::new(
            "2026-05-17T10:00:01Z",
            "search-mcp",
            "https://search.example/mcp",
            McpEgressPhase::ListTools,
            McpEgressOutcome::Failure,
            Some("http-status".into()),
            None,
        );
        append_mcp_egress(&path, &e1).unwrap();
        append_mcp_egress(&path, &e2).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed1: McpEgressEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed1.phase, "handshake");
        let parsed2: McpEgressEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed2.phase, "list-tools");
        assert_eq!(parsed2.outcome, "failure");
    }
}
