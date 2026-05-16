//! Phase C data layer — incremental diff hunks.
//!
//! Spec §3 "Workspace, not chat log":
//!   * "Live diff updates as the agent edits."
//!   * "Hunk-level accept / reject / rewrite."
//!
//! This module is the pure hunk-extraction primitive: given two byte buffers
//! (pre-image vs. post-image of one file), produce a list of [`Hunk`]s that
//! the §3 live-diff renderer paints. The §3 mechanical gate operates at
//! per-tool-call granularity — one [`Event::EditStaged`](crate::session::Event)
//! event per file edited per tool call — so hunks here are line-based, not
//! token-level. Line-based matches the diff format §14 already settled on.
//!
//! Binary files (NUL byte in the first 8 KB, same heuristic as §14 diff
//! storage) opt out: [`hunks_for`] returns `Hunks::Binary` and the UI
//! renders a "binary file changed" badge rather than producing line hunks
//! that would be misleading.

use serde::{Deserialize, Serialize};

/// One contiguous group of changes within a file. Ranges are 0-based line
/// indices, half-open `[start, end)` — matches `similar`'s grouped-ops shape
/// and the GNU unified-diff convention once you subtract 1 from `start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// Line range in the pre-image that this hunk replaces. Empty range
    /// (`start == end`) means "pure insertion at this line".
    pub old_range: LineRange,
    /// Line range in the post-image after the hunk applies. Empty range
    /// means "pure deletion of the `old_range`".
    pub new_range: LineRange,
    /// Old lines verbatim, each *without* a trailing `\n` (so the UI can
    /// choose its own line-end rendering).
    pub old_lines: Vec<String>,
    /// New lines verbatim, same convention.
    pub new_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

/// What [`hunks_for`] returns.
///
/// * [`Hunks::Same`] — the two buffers are byte-equal; no hunks needed.
/// * [`Hunks::Lines`] — line-based hunk list (the common case).
/// * [`Hunks::Binary`] — one buffer (or both) is binary; the UI shows a
///   binary-changed badge instead of trying to render bytes-as-lines.
/// * [`Hunks::Created`] / [`Hunks::Deleted`] — the §3 staging step emits
///   these when the pre-image or post-image is `None`, respectively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hunks {
    Same,
    Lines {
        hunks: Vec<Hunk>,
    },
    Binary,
    Created {
        new_byte_len: usize,
        new_line_count: usize,
    },
    Deleted {
        old_byte_len: usize,
        old_line_count: usize,
    },
}

impl Hunks {
    pub fn is_empty_diff(&self) -> bool {
        matches!(self, Self::Same)
    }
}

/// Compare two byte buffers and emit hunks. Binary buffers (NUL in the
/// first 8 KB of either side) short-circuit to `Hunks::Binary`.
pub fn hunks_for(old: &[u8], new: &[u8]) -> Hunks {
    if old == new {
        return Hunks::Same;
    }
    if looks_binary(old) || looks_binary(new) {
        return Hunks::Binary;
    }
    // Non-UTF-8 text (latin-1, shift-jis, mojibake'd files) can't be
    // line-diffed safely — silently coercing to "" via `unwrap_or` would
    // produce bogus "no diff" results when the two buffers actually
    // differ. Treat them as binary so the UI surfaces the right badge.
    let (old_text, new_text) = match (std::str::from_utf8(old), std::str::from_utf8(new)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Hunks::Binary,
    };

    let diff = similar::TextDiff::from_lines(old_text, new_text);
    // 0-context groups: each group is exactly the changed slice, no
    // surrounding equal lines. Matches the §3 "Hunk-level accept/reject"
    // requirement — each hunk is independently actionable.
    let groups = diff.grouped_ops(0);

    let mut hunks = Vec::with_capacity(groups.len());
    for group in groups {
        // A group is a list of contiguous ops; in 0-context mode the
        // first/last are the actual change.
        let first = group.first().expect("similar guarantees non-empty groups");
        let last = group.last().expect("similar guarantees non-empty groups");

        let old_start = first.old_range().start;
        let new_start = first.new_range().start;
        let old_end = last.old_range().end;
        let new_end = last.new_range().end;

        let old_lines: Vec<String> = old_text
            .lines()
            .skip(old_start)
            .take(old_end - old_start)
            .map(|s| s.to_string())
            .collect();
        let new_lines: Vec<String> = new_text
            .lines()
            .skip(new_start)
            .take(new_end - new_start)
            .map(|s| s.to_string())
            .collect();

        hunks.push(Hunk {
            old_range: LineRange {
                start: old_start,
                end: old_end,
            },
            new_range: LineRange {
                start: new_start,
                end: new_end,
            },
            old_lines,
            new_lines,
        });
    }
    Hunks::Lines { hunks }
}

/// Diff a creation — pre-image absent, post-image present. Carries a byte +
/// line count so the UI's "new file (N lines)" badge has a real number
/// without re-counting.
pub fn hunks_for_created(new: &[u8]) -> Hunks {
    if looks_binary(new) {
        return Hunks::Binary;
    }
    // Same rule as `hunks_for`: non-UTF-8 text bytes are surfaced as
    // Binary so the UI doesn't see a misleading `new_line_count: 0` for
    // a latin-1 / shift-jis file with no NUL bytes.
    let new_text = match std::str::from_utf8(new) {
        Ok(t) => t,
        Err(_) => return Hunks::Binary,
    };
    Hunks::Created {
        new_byte_len: new.len(),
        new_line_count: new_text.lines().count(),
    }
}

/// Diff a deletion — pre-image present, post-image absent.
pub fn hunks_for_deleted(old: &[u8]) -> Hunks {
    if looks_binary(old) {
        return Hunks::Binary;
    }
    let old_text = match std::str::from_utf8(old) {
        Ok(t) => t,
        Err(_) => return Hunks::Binary,
    };
    Hunks::Deleted {
        old_byte_len: old.len(),
        old_line_count: old_text.lines().count(),
    }
}

/// Spec §14 "Binary files: detected by NUL byte in the first 8 KB." Use the
/// same heuristic here so the UI's hunk renderer and the §14 diff-blob
/// store agree on what counts as binary.
fn looks_binary(buf: &[u8]) -> bool {
    let window = &buf[..buf.len().min(8 * 1024)];
    window.contains(&0u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk_lines(h: &Hunks) -> Vec<&Hunk> {
        match h {
            Hunks::Lines { hunks } => hunks.iter().collect(),
            other => panic!("expected Lines, got {other:?}"),
        }
    }

    // ---------- boundaries ----------

    #[test]
    fn identical_buffers_yield_same() {
        assert!(matches!(hunks_for(b"", b""), Hunks::Same));
        assert!(matches!(hunks_for(b"abc", b"abc"), Hunks::Same));
        assert!(matches!(
            hunks_for(b"hello\nworld\n", b"hello\nworld\n"),
            Hunks::Same
        ));
    }

    #[test]
    fn pure_insertion_at_end_produces_one_hunk_with_empty_old_range() {
        let old = b"a\nb\nc\n";
        let new = b"a\nb\nc\nd\n";
        let result = hunks_for(old, new);
        let hunks = hunk_lines(&result);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_range.len(), 0);
        assert_eq!(hunks[0].new_lines, vec!["d".to_string()]);
        assert!(hunks[0].old_lines.is_empty());
    }

    #[test]
    fn pure_insertion_at_start_produces_one_hunk_with_empty_old_range() {
        let old = b"b\nc\n";
        let new = b"a\nb\nc\n";
        let result = hunks_for(old, new);
        let hunks = hunk_lines(&result);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_range.len(), 0);
        assert_eq!(hunks[0].new_lines, vec!["a".to_string()]);
    }

    #[test]
    fn pure_deletion_produces_one_hunk_with_empty_new_range() {
        let old = b"a\nb\nc\n";
        let new = b"a\nc\n";
        let result = hunks_for(old, new);
        let hunks = hunk_lines(&result);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].new_range.len(), 0);
        assert_eq!(hunks[0].old_lines, vec!["b".to_string()]);
    }

    #[test]
    fn modification_records_both_old_and_new_lines() {
        let old = b"alpha\nbeta\ngamma\n";
        let new = b"alpha\nBETA\ngamma\n";
        let result = hunks_for(old, new);
        let hunks = hunk_lines(&result);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, vec!["beta".to_string()]);
        assert_eq!(hunks[0].new_lines, vec!["BETA".to_string()]);
    }

    #[test]
    fn two_unrelated_changes_yield_two_hunks() {
        let old = b"a\nb\nc\nd\ne\n";
        let new = b"A\nb\nc\nd\nE\n";
        let result = hunks_for(old, new);
        let hunks = hunk_lines(&result);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_lines, vec!["a".to_string()]);
        assert_eq!(hunks[0].new_lines, vec!["A".to_string()]);
        assert_eq!(hunks[1].old_lines, vec!["e".to_string()]);
        assert_eq!(hunks[1].new_lines, vec!["E".to_string()]);
    }

    #[test]
    fn unicode_lines_are_preserved() {
        let old = "hello\nwörld\n".as_bytes();
        let new = "hello\n世界\n".as_bytes();
        let result = hunks_for(old, new);
        let hunks = hunk_lines(&result);
        assert_eq!(hunks[0].old_lines, vec!["wörld".to_string()]);
        assert_eq!(hunks[0].new_lines, vec!["世界".to_string()]);
    }

    // ---------- binary detection ----------

    #[test]
    fn nul_byte_in_either_buffer_yields_binary() {
        let old = vec![b'a', 0u8, b'c'];
        let new = vec![b'x', b'y'];
        assert!(matches!(hunks_for(&old, &new), Hunks::Binary));
        assert!(matches!(hunks_for(&new, &old), Hunks::Binary));
    }

    #[test]
    fn non_utf8_text_bytes_yield_binary_not_silent_corruption() {
        // Two different latin-1 strings (no NUL bytes, so the binary
        // heuristic misses them) that the prior `unwrap_or("")` path
        // silently coerced into identical empty strings.
        let old = b"caf\xe9\n"; // "café\n" in latin-1
        let new = b"caf\xe9 au lait\n";
        assert!(matches!(hunks_for(old, new), Hunks::Binary));
    }

    #[test]
    fn nul_byte_past_8kb_is_not_classified_binary() {
        let mut old = vec![b'a'; 9 * 1024];
        let mut new = old.clone();
        // Differ inside the first 8 KB so there's something to diff.
        new[100] = b'b';
        // Add a NUL past the 8 KB window.
        old.push(0u8);
        new.push(0u8);
        assert!(matches!(hunks_for(&old, &new), Hunks::Lines { .. }));
    }

    #[test]
    fn created_for_text_returns_byte_and_line_count() {
        let new = b"first\nsecond\nthird\n";
        assert_eq!(
            hunks_for_created(new),
            Hunks::Created {
                new_byte_len: new.len(),
                new_line_count: 3
            }
        );
    }

    #[test]
    fn created_for_binary_returns_binary() {
        assert!(matches!(hunks_for_created(b"\x00\x01\x02"), Hunks::Binary));
    }

    #[test]
    fn created_for_non_utf8_text_returns_binary() {
        // latin-1 "café\n" — no NUL bytes so the binary heuristic misses,
        // but the bytes aren't UTF-8. Pre-fix this returned
        // Created{new_line_count: 0}, silently mis-reporting the file's
        // shape to the UI.
        assert!(matches!(hunks_for_created(b"caf\xe9\n"), Hunks::Binary));
    }

    #[test]
    fn deleted_for_non_utf8_text_returns_binary() {
        assert!(matches!(hunks_for_deleted(b"caf\xe9\n"), Hunks::Binary));
    }

    #[test]
    fn deleted_for_text_returns_byte_and_line_count() {
        let old = b"line1\nline2\n";
        assert_eq!(
            hunks_for_deleted(old),
            Hunks::Deleted {
                old_byte_len: old.len(),
                old_line_count: 2
            }
        );
    }

    #[test]
    fn is_empty_diff_only_true_for_same() {
        assert!(Hunks::Same.is_empty_diff());
        assert!(!Hunks::Lines { hunks: vec![] }.is_empty_diff());
        assert!(!Hunks::Binary.is_empty_diff());
    }

    // ---------- serde round trip (for session bus events) ----------

    #[test]
    fn hunks_round_trip_through_serde_json() {
        let h = Hunks::Lines {
            hunks: vec![Hunk {
                old_range: LineRange { start: 0, end: 1 },
                new_range: LineRange { start: 0, end: 1 },
                old_lines: vec!["a".into()],
                new_lines: vec!["b".into()],
            }],
        };
        let json = serde_json::to_string(&h).unwrap();
        let back: Hunks = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn binary_variant_round_trips() {
        let h = Hunks::Binary;
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"binary\""));
        let back: Hunks = serde_json::from_str(&s).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn created_and_deleted_round_trip() {
        for h in [
            Hunks::Created {
                new_byte_len: 42,
                new_line_count: 3,
            },
            Hunks::Deleted {
                old_byte_len: 99,
                old_line_count: 7,
            },
        ] {
            let s = serde_json::to_string(&h).unwrap();
            let back: Hunks = serde_json::from_str(&s).unwrap();
            assert_eq!(back, h);
        }
    }
}
