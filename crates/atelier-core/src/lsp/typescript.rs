//! TypeScript LSP diagnostic → [`crate::verify::Discrepancy`] mapper
//! (Phase B Track C2).
//!
//! The live LSP client (`async-lsp` against `typescript-language-server`)
//! lands once `experiments/lsp_spike/` resolves a GO verdict. Until then,
//! this module is a **pure function** that takes a typed `DiagnosticInput`
//! (mirroring the subset of `lsp_types::Diagnostic` we consume — line,
//! column, message) and returns the `Discrepancy::HallucinatedSymbol` row
//! the §7 Tier-1 verify path emits when TypeScript flags a non-existent
//! method or property.
//!
//! Why a hand-rolled `DiagnosticInput` rather than pulling `lsp-types`
//! today: `lsp-types` would drag the dep into `atelier-core` before the
//! spike has resolved. The mapper's input is three fields wide; cheaper to
//! re-export them later than to pull a 200 kB types crate now. When the
//! live client lands in C2's follow-on bundle, the LSP receiver translates
//! `lsp_types::Diagnostic` → `DiagnosticInput` at the boundary so this
//! pure module never changes.
//!
//! **Hallucinated-symbol heuristic.** TypeScript's `typescript-language-server`
//! emits messages like:
//!
//!   - `Property 'nonExistentMethod' does not exist on type 'Foo'`
//!   - `Cannot find name 'someUndefinedFunc'`
//!
//! Both shapes are the §7 hallucinated-symbol signal (the model wrote
//! code against an API that doesn't exist). The mapper extracts the
//! offending symbol via regex-free string parsing (single-quote-delimited
//! token between the lead-in keyword and the rest of the message); a
//! diagnostic that doesn't match either shape returns `None` so the
//! caller can fall through to Tier 3 textual without false-firing.

use crate::verify::Discrepancy;

/// Subset of LSP `Diagnostic` the mapper consumes. 0-indexed in LSP wire
/// format; the mapper converts to 1-indexed before constructing the
/// `Discrepancy::HallucinatedSymbol` so the discrepancy's line/column
/// quote directly in user-facing text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticInput {
    /// 0-indexed line number from `Diagnostic.range.start.line`.
    pub line_zero_indexed: u32,
    /// 0-indexed UTF-16 character offset from
    /// `Diagnostic.range.start.character`. LSP uses UTF-16 code units;
    /// for ASCII-only TypeScript source this matches the byte offset.
    pub character_zero_indexed: u32,
    /// Diagnostic message verbatim. The mapper caps it at
    /// [`MAX_LSP_MESSAGE_BYTES`] when constructing the discrepancy so a
    /// runaway server can't bloat the discrepancy list.
    pub message: String,
}

/// Cap on `lsp_message` length when constructing
/// `Discrepancy::HallucinatedSymbol`. Matches the
/// `schemas/audit/lsp_install.v1.json::reason.maxLength` posture (1 KiB).
pub const MAX_LSP_MESSAGE_BYTES: usize = 1024;

/// Map a single TypeScript diagnostic to a `Discrepancy::HallucinatedSymbol`
/// when the message matches the hallucinated-symbol heuristic. Returns
/// `None` for any diagnostic the heuristic doesn't recognise so the
/// caller can fall through to Tier 3 textual.
///
/// `path` is the workspace-relative file path the diagnostic came from
/// (e.g. `src/foo.ts`). Callers should pre-canonicalise: a diagnostic
/// from an absolute path should be rebased to the workspace root before
/// reaching this mapper.
pub fn map_diagnostic_to_discrepancy(
    path: &str,
    diagnostic: &DiagnosticInput,
) -> Option<Discrepancy> {
    let symbol = extract_hallucinated_symbol(&diagnostic.message)?;
    let line = diagnostic.line_zero_indexed.saturating_add(1);
    let column = diagnostic.character_zero_indexed.saturating_add(1);
    let lsp_message = truncate_to_bytes(&diagnostic.message, MAX_LSP_MESSAGE_BYTES);
    Some(Discrepancy::HallucinatedSymbol {
        path: path.to_string(),
        line,
        column,
        symbol,
        lsp_message,
    })
}

/// Pure helper: pull the hallucinated-symbol identifier out of a
/// TypeScript diagnostic message. Two shapes today:
///
///   - `Property 'X' does not exist on type 'Y'` → `X`.
///   - `Cannot find name 'X'` → `X`.
///
/// Both shapes single-quote the symbol; we slice between the first two
/// single quotes after the lead-in keyword. Returns `None` for any
/// other message shape (the caller treats `None` as "this diagnostic
/// isn't a hallucinated-symbol signal").
fn extract_hallucinated_symbol(message: &str) -> Option<String> {
    // `Property 'X' does not exist on type 'Y'` — slice between the
    // FIRST pair of single quotes.
    if let Some(after_lead) = message.strip_prefix("Property '") {
        let end = after_lead.find('\'')?;
        return Some(after_lead[..end].to_string());
    }
    // `Cannot find name 'X'`.
    if let Some(after_lead) = message.strip_prefix("Cannot find name '") {
        let end = after_lead.find('\'')?;
        return Some(after_lead[..end].to_string());
    }
    None
}

/// Truncate `s` to at most `max_bytes`, splitting on a UTF-8 boundary
/// and appending a single-character ellipsis when truncation occurred.
/// Same helper shape as `protocol_strategy::bounded_prefix`.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&s[..cut]);
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_does_not_exist_pattern_yields_hallucinated_symbol() {
        let d = DiagnosticInput {
            line_zero_indexed: 11,     // → 12 in the discrepancy
            character_zero_indexed: 3, // → 4 in the discrepancy
            message: "Property 'nonExistentMethod' does not exist on type 'Foo'".into(),
        };
        let discrepancy =
            map_diagnostic_to_discrepancy("src/foo.ts", &d).expect("hallucinated symbol must map");
        match discrepancy {
            Discrepancy::HallucinatedSymbol {
                path,
                line,
                column,
                symbol,
                lsp_message,
            } => {
                assert_eq!(path, "src/foo.ts");
                assert_eq!(line, 12);
                assert_eq!(column, 4);
                assert_eq!(symbol, "nonExistentMethod");
                assert!(lsp_message.contains("Property 'nonExistentMethod'"));
                assert!(lsp_message.contains("type 'Foo'"));
            }
            other => panic!("expected HallucinatedSymbol, got {other:?}"),
        }
    }

    #[test]
    fn cannot_find_name_pattern_yields_hallucinated_symbol() {
        let d = DiagnosticInput {
            line_zero_indexed: 0,
            character_zero_indexed: 0,
            message: "Cannot find name 'someUndefinedFunc'.".into(),
        };
        let r = map_diagnostic_to_discrepancy("a.ts", &d).expect("cannot-find-name must map");
        match r {
            Discrepancy::HallucinatedSymbol { symbol, .. } => {
                assert_eq!(symbol, "someUndefinedFunc");
            }
            other => panic!("expected HallucinatedSymbol, got {other:?}"),
        }
    }

    #[test]
    fn unrelated_diagnostic_returns_none() {
        // A diagnostic the heuristic doesn't recognise — caller falls
        // through to Tier 3 textual without a false-positive.
        let d = DiagnosticInput {
            line_zero_indexed: 5,
            character_zero_indexed: 2,
            message: "Argument of type 'string' is not assignable to parameter of type 'number'"
                .into(),
        };
        assert!(map_diagnostic_to_discrepancy("x.ts", &d).is_none());
    }

    #[test]
    fn diagnostic_line_column_become_one_indexed() {
        // LSP wire is 0-indexed; the mapper bumps both fields by 1 so
        // the discrepancy's line/column quote directly in user text.
        let d = DiagnosticInput {
            line_zero_indexed: 0,
            character_zero_indexed: 0,
            message: "Property 'foo' does not exist on type 'Bar'".into(),
        };
        let r = map_diagnostic_to_discrepancy("a.ts", &d).unwrap();
        if let Discrepancy::HallucinatedSymbol { line, column, .. } = r {
            assert_eq!(line, 1);
            assert_eq!(column, 1);
        } else {
            panic!("expected HallucinatedSymbol");
        }
    }

    #[test]
    fn lsp_message_is_capped_at_one_kib() {
        // A runaway server emitting 4 KiB of message text would bloat
        // the discrepancy list — the mapper caps it at 1 KiB.
        let huge = "Property 'evil' does not exist on type ".to_string() + &"X".repeat(4 * 1024);
        let d = DiagnosticInput {
            line_zero_indexed: 0,
            character_zero_indexed: 0,
            message: huge,
        };
        let r = map_diagnostic_to_discrepancy("a.ts", &d).unwrap();
        if let Discrepancy::HallucinatedSymbol { lsp_message, .. } = r {
            // 1024 bytes + the 3-byte UTF-8 ellipsis.
            assert!(lsp_message.len() <= MAX_LSP_MESSAGE_BYTES + 4);
            assert!(lsp_message.ends_with('…'));
        }
    }

    #[test]
    fn truncate_to_bytes_respects_utf8_boundary() {
        // A 4-byte UTF-8 codepoint at the cut point would otherwise
        // produce invalid UTF-8 — the helper walks back to the previous
        // boundary.
        let s = "🦀".repeat(100); // 400 bytes
        let trimmed = truncate_to_bytes(&s, 6); // not a codepoint boundary
        assert!(trimmed.ends_with('…'));
        // 4 bytes (one 🦀) + the ellipsis.
        assert_eq!(trimmed.chars().count(), 2);
    }
}
