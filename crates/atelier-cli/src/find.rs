//! v60.20 — `atelier find --path <P>` subcommand.
//!
//! Closes the Phase C `[x]`-deferred row: the `FindProbe` +
//! `FindProbeLog` instrumentation landed in v60.7 but the user-facing
//! CLI subcommand was deferred. This module bridges the two.
//!
//! ## What "find what agent knows about file X" means operationally
//!
//! The spec §5 UX target asks: given a path P, how fast can the user
//! see what the running agent already knows about it? The agent's
//! knowledge lives in two places:
//!
//! 1. The runtime `ContextManager` — but that's in-memory and lost
//!    when the process exits.
//! 2. The persisted `session.json::conversation[]` — a JSON record
//!    of every assistant turn + tool result the agent has produced.
//!
//! For a CLI subcommand running outside the live process, (2) is the
//! only sustainable source. We walk the most-recent (or named)
//! session's conversation entries and grep for P in:
//! * any text content (substring match)
//! * any `tool_calls[].arguments` (serialized JSON, substring match)
//! * any `tool_call_id` (rarely useful; included for completeness)
//!
//! Matches are returned with a one-line excerpt the caller can render.
//! The total elapsed wall-clock (request to last match) is recorded
//! as a [`FindProbe`] in the session's `find_probes.json` so the
//! median-elapsed-ms target has data to compute against.
//!
//! ## "No session present" semantics
//!
//! The canonical fixture t13 includes a check that asserts `atelier
//! find --path X` exits 0 when no session exists in the workspace.
//! That's the right ergonomic for a brand-new repo: the user is
//! asking "what does the agent know?" and the honest answer "nothing
//! — no session yet" is not an error condition.

use std::path::{Path, PathBuf};
use std::time::Instant;

use atelier_core::persistence::OnDiskSession;
use uuid::Uuid;

use crate::instrumentation::{FindProbe, FindProbeLog};

/// Outcome of a `find` query. The CLI handler renders this; tests
/// assert against it without re-parsing stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindOutcome {
    /// Selected session UUID, or `None` if the workspace had no
    /// sessions at all.
    pub session_uuid: Option<Uuid>,
    /// Conversation-entry matches in walk order.
    pub matches: Vec<FindMatch>,
    /// Elapsed wall-clock from invocation through the last match
    /// (or end-of-walk for zero-match runs).
    pub elapsed_ms: u64,
}

/// One conversation entry matched against the query path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindMatch {
    /// 0-indexed position in `OnDiskSession::conversation[]`.
    pub turn_index: usize,
    /// `role` field of the matched conversation entry.
    pub role: String,
    /// Short excerpt around the first occurrence of the query path.
    /// At most ~160 chars; longer entries are truncated with a
    /// trailing `…`.
    pub excerpt: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FindError {
    #[error("workspace path does not exist: {}", .0.display())]
    WorkspaceMissing(PathBuf),

    #[error("named session {0} not found in workspace .atelier/sessions/")]
    SessionNotFound(Uuid),

    #[error("session uuid {0:?} does not parse as a UUID")]
    InvalidSessionUuid(String),

    #[error("session.json at {} is malformed: {source}", .path.display())]
    SessionMalformed {
        path: PathBuf,
        #[source]
        source: atelier_core::persistence::PersistenceError,
    },

    #[error("io error reading workspace sessions: {0}")]
    Io(#[from] std::io::Error),
}

/// Query parameters for [`find`]. `session` selects an explicit
/// session UUID; `None` picks the most recently mtime'd session
/// directory under `<workspace>/.atelier/sessions/`.
#[derive(Debug, Clone)]
pub struct FindQuery {
    pub workspace: PathBuf,
    pub path: String,
    pub session: Option<Uuid>,
    /// When true, the find probe is **not** appended to the session's
    /// `find_probes.json`. Used by the canonical fixture's check
    /// command so a `make check` run on a stale workspace doesn't
    /// keep growing the probe log.
    pub dry_run: bool,
}

/// Run a find query end-to-end.
///
/// Returns `Ok(FindOutcome { session_uuid: None, … })` when the
/// workspace has no sessions yet — this is a valid state, not an
/// error. Use [`FindOutcome::session_uuid`] to distinguish.
pub fn find(query: FindQuery) -> Result<FindOutcome, FindError> {
    let started = Instant::now();

    if !query.workspace.exists() {
        return Err(FindError::WorkspaceMissing(query.workspace.clone()));
    }

    // 1. Resolve the session directory.
    let session_uuid = match query.session {
        Some(u) => {
            let dir = OnDiskSession::session_dir(&query.workspace, u);
            if !dir.is_dir() {
                return Err(FindError::SessionNotFound(u));
            }
            Some(u)
        }
        None => most_recent_session(&query.workspace)?,
    };

    let Some(uuid) = session_uuid else {
        // Empty workspace — return cleanly. Skip the probe append
        // because there's no session_dir to write into.
        let elapsed_ms = elapsed_to_ms(started.elapsed());
        return Ok(FindOutcome {
            session_uuid: None,
            matches: vec![],
            elapsed_ms,
        });
    };

    // 2. Load the session.
    let session_dir = OnDiskSession::session_dir(&query.workspace, uuid);
    let session =
        OnDiskSession::load_from(&session_dir).map_err(|e| FindError::SessionMalformed {
            path: session_dir.join("session.json"),
            source: e,
        })?;

    // 3. Walk the conversation and match.
    let matches = match_conversation(&session.conversation, &query.path);

    let elapsed_ms = elapsed_to_ms(started.elapsed());

    // 4. Append a probe to the session's log (unless --dry-run).
    if !query.dry_run {
        let probe = FindProbe {
            queried_at: atelier_core::time::now_rfc3339(),
            path: query.path.clone(),
            matched: matches.len(),
            elapsed_ms,
        };
        // Append failure is non-fatal: the CLI command's contract is
        // "tell the user what the agent knows," not "guarantee the
        // probe was logged." A failing append is logged via tracing
        // (the caller can wire RUST_LOG=warn to see it).
        if let Err(e) = FindProbeLog::append(&session_dir, probe) {
            tracing::warn!(
                error = %e,
                path = ?session_dir,
                "atelier find: failed to append find_probes.json; probe lost"
            );
        }
    }

    Ok(FindOutcome {
        session_uuid,
        matches,
        elapsed_ms,
    })
}

/// Find the most recently modified session directory under
/// `<workspace>/.atelier/sessions/`. Returns `Ok(None)` if the
/// `sessions/` directory doesn't exist or is empty — the canonical
/// "fresh workspace" state.
fn most_recent_session(workspace: &Path) -> Result<Option<Uuid>, FindError> {
    let dir = workspace.join(".atelier").join("sessions");
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(std::time::SystemTime, Uuid)> = None;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(name) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match best {
            Some((cur, _)) if cur >= mtime => {}
            _ => best = Some((mtime, uuid)),
        }
    }
    Ok(best.map(|(_, u)| u))
}

/// Walk the session's `conversation[]` array and return one
/// [`FindMatch`] per entry that contains `path` somewhere in its
/// serialized form. Uses case-sensitive substring matching — file
/// paths on macOS / Linux / Windows are all case-sensitive at the
/// system call layer when present in tool arguments (the matter of
/// whether the filesystem actually cares is orthogonal; what's on
/// the wire is the source of truth).
fn match_conversation(conversation: &[serde_json::Value], path: &str) -> Vec<FindMatch> {
    let mut matches = Vec::new();
    for (i, entry) in conversation.iter().enumerate() {
        // Serialize the whole entry to JSON; substring-match against
        // the result. This catches the path no matter where it lives:
        // - content (assistant prose / user attachments)
        // - tool_calls[*].arguments (tool invocations)
        // - tool_call_id (rare but completable)
        let serialized = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !serialized.contains(path) {
            continue;
        }
        let role = entry
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("?")
            .to_string();
        let excerpt = build_excerpt(&serialized, path, 160);
        matches.push(FindMatch {
            turn_index: i,
            role,
            excerpt,
        });
    }
    matches
}

/// Build a short excerpt centered on the first occurrence of `needle`
/// within `haystack`, capped to roughly `max_len` characters. Uses
/// char boundaries so multi-byte UTF-8 sequences aren't sliced.
fn build_excerpt(haystack: &str, needle: &str, max_len: usize) -> String {
    let Some(byte_idx) = haystack.find(needle) else {
        // Shouldn't happen — caller already verified containment.
        return haystack.chars().take(max_len).collect();
    };
    let half_before = max_len / 2;
    let half_after = max_len - half_before;
    // Walk back from byte_idx up to `half_before` chars.
    let start = haystack[..byte_idx]
        .char_indices()
        .rev()
        .take(half_before)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let after = &haystack[byte_idx..];
    let end_rel = after
        .char_indices()
        .take(half_after)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(after.len());
    let end = byte_idx + end_rel;
    let mut excerpt = String::with_capacity(max_len + 2);
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(&haystack[start..end]);
    if end < haystack.len() {
        excerpt.push('…');
    }
    excerpt
}

fn elapsed_to_ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelier_core::persistence::OnDiskSession;
    use serde_json::json;
    use tempfile::TempDir;

    fn fresh_workspace() -> TempDir {
        TempDir::new().unwrap()
    }

    fn seeded_session(workspace: &Path, conversation: Vec<serde_json::Value>) -> Uuid {
        let uuid = Uuid::new_v4();
        let mut session = OnDiskSession::fresh(uuid, "test", "2026-05-18T00:00:00Z");
        session.conversation = conversation;
        let dir = OnDiskSession::session_dir(workspace, uuid);
        std::fs::create_dir_all(&dir).unwrap();
        session.save_to(&dir).unwrap();
        uuid
    }

    #[test]
    fn find_returns_none_session_when_workspace_has_no_sessions() {
        let ws = fresh_workspace();
        let outcome = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "src/parser/lex.rs".into(),
            session: None,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(outcome.session_uuid, None);
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn find_matches_path_in_tool_arguments() {
        let ws = fresh_workspace();
        let uuid = seeded_session(
            ws.path(),
            vec![
                json!({
                    "role": "user",
                    "content": "please investigate the lexer",
                    "tool_calls": []
                }),
                json!({
                    "role": "assistant",
                    "content": "I'll read the lexer.",
                    "tool_calls": [{
                        "id": "tc-1",
                        "name": "read_file",
                        "arguments": {"path": "src/parser/lex.rs"}
                    }]
                }),
                json!({
                    "role": "tool",
                    "tool_call_id": "tc-1",
                    "content": "{\"byte_len\": 420, \"contents\": \"fn lex(...)\"}",
                    "tool_calls": []
                }),
            ],
        );
        let outcome = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "src/parser/lex.rs".into(),
            session: Some(uuid),
            dry_run: true, // don't pollute the seeded session's probe log
        })
        .unwrap();
        assert_eq!(outcome.session_uuid, Some(uuid));
        // The assistant tool_call carries the path verbatim; the user
        // turn says "lexer" which doesn't contain the full path.
        let indices: Vec<usize> = outcome.matches.iter().map(|m| m.turn_index).collect();
        assert!(
            indices.contains(&1),
            "must match the assistant tool_call entry, got {:?}",
            indices
        );
    }

    #[test]
    fn find_returns_zero_matches_when_path_is_unknown_to_the_agent() {
        let ws = fresh_workspace();
        let uuid = seeded_session(
            ws.path(),
            vec![json!({
                "role": "user",
                "content": "investigate utils.py",
                "tool_calls": []
            })],
        );
        let outcome = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "totally/different/file.rs".into(),
            session: Some(uuid),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(outcome.session_uuid, Some(uuid));
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn find_picks_most_recent_session_when_none_specified() {
        let ws = fresh_workspace();
        let _older = seeded_session(
            ws.path(),
            vec![json!({"role":"user","content":"older","tool_calls":[]})],
        );
        // Sleep wider than the coarsest plausible mtime resolution
        // (1 s on some filesystems) so the second `seeded_session`
        // call's directory mtime is strictly later. Tests on
        // sub-second-mtime filesystems pay the sleep but don't lose
        // ordering accuracy.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let newer = seeded_session(
            ws.path(),
            vec![json!({"role":"user","content":"src/parser/lex.rs","tool_calls":[]})],
        );

        let outcome = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "src/parser/lex.rs".into(),
            session: None,
            dry_run: true,
        })
        .unwrap();
        assert_eq!(outcome.session_uuid, Some(newer));
    }

    #[test]
    fn find_named_session_not_in_workspace_errors() {
        let ws = fresh_workspace();
        let bogus = Uuid::new_v4();
        let err = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "x".into(),
            session: Some(bogus),
            dry_run: true,
        })
        .unwrap_err();
        assert!(matches!(err, FindError::SessionNotFound(u) if u == bogus));
    }

    #[test]
    fn find_appends_probe_unless_dry_run() {
        let ws = fresh_workspace();
        let uuid = seeded_session(
            ws.path(),
            vec![json!({"role":"user","content":"probe me","tool_calls":[]})],
        );
        // First call: non-dry-run → probe appended.
        let _ = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "anything".into(),
            session: Some(uuid),
            dry_run: false,
        })
        .unwrap();
        let session_dir = OnDiskSession::session_dir(ws.path(), uuid);
        let log_path = FindProbeLog::path_for(&session_dir);
        assert!(
            log_path.exists(),
            "probe log must be created after non-dry-run"
        );
        let log = FindProbeLog::load_from(&session_dir).unwrap();
        assert_eq!(log.probes.len(), 1);

        // Second call: dry-run → no append.
        let _ = find(FindQuery {
            workspace: ws.path().to_path_buf(),
            path: "anything-else".into(),
            session: Some(uuid),
            dry_run: true,
        })
        .unwrap();
        let log = FindProbeLog::load_from(&session_dir).unwrap();
        assert_eq!(
            log.probes.len(),
            1,
            "dry-run must not append; expected 1 probe, got {}",
            log.probes.len()
        );
    }

    #[test]
    fn build_excerpt_truncates_around_needle_with_char_boundary_safety() {
        let s = "alpha bravo charlie delta echo foxtrot golf";
        let ex = build_excerpt(s, "delta", 20);
        assert!(ex.contains("delta"));
        // ≤ max_len chars of haystack + two ellipsis chars on either side.
        assert!(
            ex.chars().count() <= 22,
            "len caps at ~max_len chars: got {:?} ({} chars)",
            ex,
            ex.chars().count()
        );
        // Multi-byte safety.
        let utf8 = "α β γ δ_target δ ε";
        let ex = build_excerpt(utf8, "δ_target", 16);
        assert!(ex.contains("δ_target"));
    }
}
