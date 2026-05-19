//! v60.53 §15 — TUI slash-command completion state machine.
//!
//! Mirrors the GUI's Composer autocomplete logic: the user enters
//! "slash input" mode (typically via the `/` key from `Normal`),
//! types a partial name, hits `Tab` to cycle through matches, and
//! commits with `Enter`. The state machine is deliberately
//! self-contained — no IO, no ratatui, no event-bus — so the apply
//! logic can be exercised deterministically from unit tests.
//!
//! The TUI's existing input-mode set (`InputMode::TextInput`,
//! `EvictConfirm`, etc.) gates this through `handle_key` in
//! `lib.rs`; the bridge surface is the [`SlashState`] type and the
//! [`SlashState::apply`] step function.
//!
//! Spec §15 lines 786–797 — the visible list rendering matches the
//! `/help` format format the rest of the harness uses.

use atelier_core::skills::SkillRegistry;

/// What `apply` returns to the run loop. `Continue` means stay in
/// slash-input mode; `Cancel` exits the mode without committing;
/// `Commit` exits and asks the run loop to expand + send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashOutcome {
    /// Keep the modal open — the user is still typing.
    Continue,
    /// User pressed Esc; close the modal without commit.
    Cancel,
    /// User pressed Enter on a complete `/<name> [args]`; the run
    /// loop should resolve through `SkillRegistry::get(name)` +
    /// `substitute(...)`.
    Commit { raw: String },
}

/// One key event distilled into a verb the state machine understands.
/// `handle_key` in `lib.rs` is the translator — we keep this enum
/// minimal so the unit tests don't need to fake a full `KeyEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashEvent {
    /// User typed a printable character.
    Char(char),
    /// Backspace — drop the last character from `buffer`.
    Backspace,
    /// Tab — accept the highlighted completion (replace the partial
    /// name with the full one + trailing space).
    Tab,
    /// `↓` — move highlight down.
    SelectNext,
    /// `↑` — move highlight up.
    SelectPrev,
    /// Enter — commit the current buffer.
    Enter,
    /// Esc — cancel without commit.
    Esc,
}

/// Snapshot of the slash-input modal. The TUI owns one of these in
/// `InputMode::SlashInput { state: SlashState }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashState {
    /// What the user has typed so far, *including* the leading `/`.
    /// Holding the slash in the buffer keeps the rendering logic
    /// (and the eventual `Runner::run` call) symmetric with the GUI
    /// path.
    pub buffer: String,
    /// Index into the filtered match list. Reset to 0 every time
    /// the buffer changes so the highlight follows the user.
    pub selected: usize,
    /// Sorted list of skill names known to the registry. Sourced
    /// once at modal-open time so the apply step is O(1) in registry
    /// access.
    pub all_names: Vec<String>,
}

impl SlashState {
    /// Build a new slash-input state. The buffer starts with the
    /// literal `/` so the user can backspace it to cancel naturally.
    pub fn new(registry: &SkillRegistry) -> Self {
        let mut names: Vec<String> = registry.names().cloned().collect();
        names.sort();
        Self {
            buffer: "/".to_string(),
            selected: 0,
            all_names: names,
        }
    }

    /// Compute the filtered match list. v60.55 — no length cap; the
    /// caller (ratatui popup) is expected to scroll long lists. The
    /// bundled set has 19 skills today and continues to grow.
    pub fn matches(&self) -> Vec<&str> {
        let head = self.head();
        let head_end = self.buffer.find(char::is_whitespace);
        if head_end.is_some() {
            // User has typed past the name — no completion suggestions.
            return Vec::new();
        }
        self.all_names
            .iter()
            .filter(|n| n.starts_with(head))
            .map(|s| s.as_str())
            .collect()
    }

    /// Return the prefix the user has typed after the leading `/`,
    /// up to the first whitespace.
    pub fn head(&self) -> &str {
        let after_slash = self.buffer.strip_prefix('/').unwrap_or(&self.buffer);
        match after_slash.find(char::is_whitespace) {
            Some(i) => &after_slash[..i],
            None => after_slash,
        }
    }

    /// Apply a single keypress. The TUI's `handle_key` builds these
    /// from `KeyEvent` and feeds them through here; the return value
    /// tells the run loop what to do next.
    pub fn apply(&mut self, ev: SlashEvent) -> SlashOutcome {
        match ev {
            SlashEvent::Char(c) => {
                self.buffer.push(c);
                self.selected = 0;
                SlashOutcome::Continue
            }
            SlashEvent::Backspace => {
                // Treat backspacing the leading `/` as a cancel —
                // it's the most intuitive escape for a user who
                // accidentally hit `/`.
                if self.buffer.len() <= 1 {
                    return SlashOutcome::Cancel;
                }
                self.buffer.pop();
                self.selected = 0;
                SlashOutcome::Continue
            }
            SlashEvent::Tab => {
                let matches = self.matches();
                if matches.is_empty() {
                    return SlashOutcome::Continue;
                }
                let pick = matches[self.selected.min(matches.len() - 1)].to_string();
                self.buffer = format!("/{pick} ");
                self.selected = 0;
                SlashOutcome::Continue
            }
            SlashEvent::SelectNext => {
                let n = self.matches().len();
                if n > 0 {
                    self.selected = (self.selected + 1) % n;
                }
                SlashOutcome::Continue
            }
            SlashEvent::SelectPrev => {
                let n = self.matches().len();
                if n > 0 {
                    self.selected = (self.selected + n - 1) % n;
                }
                SlashOutcome::Continue
            }
            SlashEvent::Enter => SlashOutcome::Commit {
                raw: self.buffer.clone(),
            },
            SlashEvent::Esc => SlashOutcome::Cancel,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SkillRegistry {
        let dir = tempfile::TempDir::new().unwrap();
        SkillRegistry::load(dir.path(), None).unwrap()
    }

    #[test]
    fn new_state_starts_with_slash_and_zero_selection() {
        let reg = registry();
        let s = SlashState::new(&reg);
        assert_eq!(s.buffer, "/");
        assert_eq!(s.selected, 0);
        assert!(s.all_names.contains(&"review".to_string()));
    }

    #[test]
    fn typing_filters_matches() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        assert_eq!(s.apply(SlashEvent::Char('r')), SlashOutcome::Continue);
        let m = s.matches();
        // `review` + `refactor` match the `r` prefix.
        assert!(m.contains(&"review"), "matches: {m:?}");
        assert!(m.contains(&"refactor"), "matches: {m:?}");
    }

    #[test]
    fn tab_replaces_partial_with_first_match() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        s.apply(SlashEvent::Char('r'));
        s.apply(SlashEvent::Char('e'));
        s.apply(SlashEvent::Char('v'));
        let _ = s.apply(SlashEvent::Tab);
        assert_eq!(s.buffer, "/review ");
    }

    #[test]
    fn select_next_wraps_around_matches() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        s.apply(SlashEvent::Char('r')); // review, refactor
        let n = s.matches().len();
        assert!(n >= 2);
        for _ in 0..n {
            s.apply(SlashEvent::SelectNext);
        }
        assert_eq!(s.selected, 0, "selected must wrap back to 0");
    }

    #[test]
    fn enter_commits_buffer_unchanged() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        s.apply(SlashEvent::Char('r'));
        s.apply(SlashEvent::Char('e'));
        s.apply(SlashEvent::Char('v'));
        s.apply(SlashEvent::Char('i'));
        s.apply(SlashEvent::Char('e'));
        s.apply(SlashEvent::Char('w'));
        let out = s.apply(SlashEvent::Enter);
        assert_eq!(
            out,
            SlashOutcome::Commit {
                raw: "/review".into()
            }
        );
    }

    #[test]
    fn backspace_to_empty_cancels() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        assert_eq!(s.buffer, "/");
        let out = s.apply(SlashEvent::Backspace);
        assert_eq!(out, SlashOutcome::Cancel);
    }

    #[test]
    fn esc_cancels_immediately() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        s.apply(SlashEvent::Char('r'));
        let out = s.apply(SlashEvent::Esc);
        assert_eq!(out, SlashOutcome::Cancel);
    }

    #[test]
    fn matches_disappear_once_whitespace_typed() {
        let reg = registry();
        let mut s = SlashState::new(&reg);
        s.apply(SlashEvent::Char('r'));
        s.apply(SlashEvent::Char('e'));
        s.apply(SlashEvent::Char('v'));
        assert!(!s.matches().is_empty());
        s.apply(SlashEvent::Char(' '));
        assert!(
            s.matches().is_empty(),
            "after whitespace user is typing args, not the name"
        );
    }
}
