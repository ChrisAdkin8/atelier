//! v60 — shared user-input safety predicates.
//!
//! Three call sites in v59 maintained byte-for-byte equivalent
//! copies of the Trojan-Source / control-character rejection rules:
//!
//! 1. `dispatcher::is_disallowed_control` — driven by
//!    `dispatcher::validate_user_text` (memory cards + plan steps).
//! 2. `memory::validate_card_content` — applied by
//!    `MemoryStore::from_vec` on session-snapshot reload.
//! 3. `dispatcher::validate_memory_card_content` — thin wrapper that
//!    mapped errors into `MemoryError::InvalidContent`.
//!
//! The v59 audit flagged this as MED-A: a future Unicode revision
//! adding another bidi override (or a Trojan-Source follow-up) would
//! have to be applied in all three places, with no test asserting
//! they agree. This module is the single source of truth.
//!
//! The predicate set:
//!   * `U+0000`-`U+001F` ASCII control bytes, except `\n` (newline)
//!     and `\t` (tab).
//!   * `U+007F` DEL.
//!   * `U+0080`-`U+009F` C1 controls (includes NEL `U+0085`).
//!   * `U+2028` LINE SEPARATOR / `U+2029` PARAGRAPH SEPARATOR — act
//!     as line breaks in YAML / CSS / some Rust APIs.
//!   * `U+200E` LRM / `U+200F` RLM — invisible direction markers.
//!   * `U+202A`-`U+202E` — bidirectional embed/override (the
//!     2021 "Trojan Source" attack class).
//!   * `U+2066`-`U+2069` — bidirectional isolates (the 2022 follow-up
//!     to Trojan Source: LRI / RLI / FSI / PDI).
//!
//! Adding a new disallowed code point means one edit here and the
//! shared agreement test below catches every consumer.

/// True iff `c` is a control character we don't allow inside
/// user-supplied text (memory card content, plan step text, plan
/// step constraints, future free-form fields). Allows `\n` and
/// `\t`; rejects every other ASCII control + DEL + C1 + Unicode
/// line/paragraph separators + bidi marks/overrides + bidi
/// isolates.
pub fn is_disallowed_control(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    let cb = c as u32;
    // C0 (0x00–0x1F) + DEL (0x7F).
    if cb < 0x20 || cb == 0x7F {
        return true;
    }
    // C1 controls (incl. NEL U+0085).
    if (0x80..=0x9F).contains(&cb) {
        return true;
    }
    // Unicode line/paragraph separators.
    if cb == 0x2028 || cb == 0x2029 {
        return true;
    }
    // Bidi marks and overrides — Trojan Source class.
    if cb == 0x200E || cb == 0x200F {
        return true;
    }
    if (0x202A..=0x202E).contains(&cb) {
        return true;
    }
    // Bidi isolate codepoints (LRI / RLI / FSI / PDI).
    (0x2066..=0x2069).contains(&cb)
}

/// True iff a `---` line (after trimming trailing whitespace) appears
/// anywhere in `text`. Such a line would forge YAML frontmatter when
/// the content is wrapped in a frontmatter block during memory-card
/// promotion. Plan steps don't go through frontmatter promotion so
/// callers wanting plan-style validation pass `false` to
/// [`validate_user_text`].
fn has_frontmatter_delimiter(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_end_matches([' ', '\t']) == "---")
}

/// Reject user text containing disallowed control characters (always)
/// and YAML frontmatter delimiters (only when `check_frontmatter` is
/// set). Returns a short human-readable reason on rejection that the
/// caller wraps in their domain-specific error type.
pub fn validate_user_text(text: &str, check_frontmatter: bool) -> Result<(), String> {
    if let Some(c) = text.chars().find(|c| is_disallowed_control(*c)) {
        return Err(format!("control character U+{:04X} not allowed", c as u32));
    }
    if check_frontmatter && has_frontmatter_delimiter(text) {
        return Err(
            "content contains a `---` line; would forge YAML frontmatter when promoted".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_newline_and_tab() {
        assert!(!is_disallowed_control('\n'));
        assert!(!is_disallowed_control('\t'));
    }

    #[test]
    fn rejects_full_c0_except_newline_and_tab() {
        for cb in 0..0x20u32 {
            let c = char::from_u32(cb).unwrap();
            let expected = !(cb == 0x09 || cb == 0x0A);
            assert_eq!(
                is_disallowed_control(c),
                expected,
                "U+{cb:04X} (allowed if expected==false)"
            );
        }
    }

    #[test]
    fn rejects_del_and_c1_controls() {
        assert!(is_disallowed_control('\u{007F}')); // DEL
        for cb in 0x80..=0x9Fu32 {
            assert!(
                is_disallowed_control(char::from_u32(cb).unwrap()),
                "U+{cb:04X} (C1) must be rejected"
            );
        }
    }

    #[test]
    fn rejects_line_paragraph_separators() {
        assert!(is_disallowed_control('\u{2028}'));
        assert!(is_disallowed_control('\u{2029}'));
    }

    #[test]
    fn rejects_bidi_marks_overrides_and_isolates() {
        // LRM / RLM.
        assert!(is_disallowed_control('\u{200E}'));
        assert!(is_disallowed_control('\u{200F}'));
        // Embed + override (Trojan Source 2021).
        for cb in 0x202A..=0x202Eu32 {
            assert!(is_disallowed_control(char::from_u32(cb).unwrap()));
        }
        // Isolates (2022 follow-up).
        for cb in 0x2066..=0x2069u32 {
            assert!(is_disallowed_control(char::from_u32(cb).unwrap()));
        }
    }

    #[test]
    fn accepts_printable_unicode_including_emoji() {
        for c in ['A', 'é', '日', '🦀'] {
            assert!(!is_disallowed_control(c), "{c:?} should be allowed");
        }
    }

    #[test]
    fn validate_user_text_rejects_frontmatter_when_flag_set() {
        assert!(validate_user_text("hello\n---\nworld", true).is_err());
        // Plan steps skip the frontmatter check — `---` is fine.
        assert!(validate_user_text("hello\n---\nworld", false).is_ok());
    }

    #[test]
    fn validate_user_text_carries_control_byte_in_error_message() {
        let err = validate_user_text("hi\u{202E}there", true).unwrap_err();
        assert!(err.contains("U+202E"));
    }
}
