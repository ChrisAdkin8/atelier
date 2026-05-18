//! §5 Mental Model — user-editable mental-model panel.
//!
//! Spec §5 "Visible context / memory / plan":
//!   * "Mental-model panel — off by default, cost-disclosed on enable"
//!
//! Phase C close (v60.7): this is the data layer. The text is
//! user-editable; v0 does **not** feed it into the model — the cost
//! disclosure is "0 tokens per turn at present" until a future
//! version actually injects it. The toggle + text round-trips
//! through `SessionDispatcher::set_mental_model` and broadcasts on
//! the bus as `Event::MentalModelSnapshot { enabled, text_tokens }`
//! so subscribed UIs converge.
//!
//! Off by default so a freshly-spawned session doesn't surprise the
//! user with a panel they didn't ask for. Once enabled the panel
//! stays enabled across the session lifetime (and is captured by
//! the §14 on-disk session at end-of-run via the consumer's
//! choice — `MentalModel` itself is pure data and does no I/O).

use serde::{Deserialize, Serialize};

/// User-editable mental-model. Off by default. Holds free-form text
/// plus an `enabled` flag (visibility gate for the GUI/TUI panel)
/// and an RFC 3339 `updated_at` timestamp set whenever
/// [`Self::set`] mutates the contents.
///
/// **Token cost is `0` in v0** — the harness does not inject the
/// text into the prompt window. The text_tokens projection on
/// [`MentalModelSnapshot`] therefore mirrors the underlying byte
/// approximation, but the cost-disclosure label rendered next to
/// the toggle reads "0 tokens per turn at present" until a future
/// version actually injects it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentalModel {
    /// Visibility / inject toggle. UIs render the panel only when
    /// `true`; v0 inject is a no-op regardless of this flag.
    pub enabled: bool,
    /// Free-form user text. Empty by default. Validated against the
    /// shared `text_safety::validate_user_text` predicate on every
    /// set so a hostile copy/paste can't smuggle Trojan-Source
    /// bytes into the panel.
    pub text: String,
    /// RFC 3339 timestamp of the last successful [`Self::set`]
    /// call. Empty before any mutation lands.
    pub updated_at: String,
}

/// Projection of [`MentalModel`] broadcast on the bus. Carries an
/// approximate `text_tokens` count so the UI's cost-disclosure
/// badge can render without re-computing. Token cost is the byte
/// length divided by 4 — the same coarse approximation the §1
/// adapter uses for unknown counts; it's correct within an order
/// of magnitude for English prose and is honest about being
/// approximate.
///
/// v0: the panel is **not** injected into the prompt. `text_tokens`
/// is therefore informational; the cost the user pays per turn is
/// 0 until a future revision actually injects the text. UIs render
/// the disclosure literally as "0 tokens per turn at present".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentalModelSnapshot {
    pub enabled: bool,
    pub text: String,
    pub text_tokens: u32,
    pub updated_at: String,
}

impl MentalModelSnapshot {
    /// Approximate token count from a UTF-8 string. Same coarse
    /// approximation the §1 adapter uses when it can't get a real
    /// count from the provider. Bytes/4; saturates at u32::MAX.
    pub fn approx_tokens(text: &str) -> u32 {
        (text.len() / 4).min(u32::MAX as usize) as u32
    }

    pub fn from_model(m: &MentalModel) -> Self {
        Self {
            enabled: m.enabled,
            text: m.text.clone(),
            text_tokens: Self::approx_tokens(&m.text),
            updated_at: m.updated_at.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MentalModelError {
    /// Same `text_safety` predicate as `MemoryCard::content` so a
    /// future Unicode revision is a one-line change. Message
    /// carries the offending byte / line / reason.
    #[error("mental model text is invalid: {0}")]
    InvalidText(String),
}

impl MentalModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the text + enabled flag. Validates the text against
    /// the shared `text_safety::validate_user_text` predicate;
    /// rejects on any invalid byte without mutating state. `now`
    /// is the RFC 3339 timestamp the caller supplies (matches the
    /// `MemoryCard::touch` convention).
    pub fn set(
        &mut self,
        text: String,
        enabled: bool,
        now: impl Into<String>,
    ) -> Result<(), MentalModelError> {
        crate::text_safety::validate_user_text(&text, /* check_frontmatter */ false)
            .map_err(MentalModelError::InvalidText)?;
        self.text = text;
        self.enabled = enabled;
        self.updated_at = now.into();
        Ok(())
    }

    /// Build a projection suitable for the bus.
    pub fn snapshot(&self) -> MentalModelSnapshot {
        MentalModelSnapshot::from_model(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_with_empty_text() {
        let m = MentalModel::new();
        assert!(!m.enabled);
        assert!(m.text.is_empty());
        assert!(m.updated_at.is_empty());
    }

    #[test]
    fn set_updates_text_enabled_and_timestamp() {
        let mut m = MentalModel::new();
        m.set(
            "the user prefers tabs".to_string(),
            true,
            "2026-05-17T12:00:00Z",
        )
        .unwrap();
        assert!(m.enabled);
        assert_eq!(m.text, "the user prefers tabs");
        assert_eq!(m.updated_at, "2026-05-17T12:00:00Z");
    }

    #[test]
    fn set_rejects_invalid_text_atomically() {
        let mut m = MentalModel::new();
        // NUL byte triggers text_safety rejection.
        let err = m.set("contains\0nul".to_string(), true, "t").unwrap_err();
        assert!(matches!(err, MentalModelError::InvalidText(_)));
        // State unchanged on error.
        assert!(!m.enabled);
        assert!(m.text.is_empty());
        assert!(m.updated_at.is_empty());
    }

    #[test]
    fn snapshot_carries_approx_token_count() {
        let mut m = MentalModel::new();
        m.set("a".repeat(40), true, "t").unwrap();
        let s = m.snapshot();
        assert!(s.enabled);
        assert_eq!(s.text_tokens, 10); // 40 bytes / 4
    }

    #[test]
    fn approx_tokens_for_empty_string_is_zero() {
        assert_eq!(MentalModelSnapshot::approx_tokens(""), 0);
    }

    #[test]
    fn set_can_toggle_off_while_keeping_text() {
        let mut m = MentalModel::new();
        m.set("notes".into(), true, "t1").unwrap();
        m.set("notes".into(), false, "t2").unwrap();
        assert!(!m.enabled);
        assert_eq!(m.text, "notes");
        assert_eq!(m.updated_at, "t2");
    }
}
