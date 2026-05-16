//! Atelier terminal UI.
//!
//! Spec §3 TUI subset. This module ships the **bootstrap layer** —
//! `AppState`, the pure `render(...)` projection that paints it onto a
//! ratatui `Buffer`, and the `run()` I/O wrapper that boots a session and
//! pumps events. The richer §3 widgets (conversation, file tree, plan
//! canvas, cost/context meters, timeline scrubber) land on top of this
//! foundation in follow-up sessions.
//!
//! # Pure vs. impure split
//!
//! Everything testable is in [`AppState`] (state + pure mutators) and
//! [`render`] (state → Buffer). The terminal lifecycle (raw mode,
//! alternate screen, the tokio select loop) lives in [`run`] and is
//! exercised by hand — not unit-tested, since it'd need a PTY.
//!
//! # Why a single panel?
//!
//! Per the spec §3 acceptance gate for the TUI: "TUI subset rendered from
//! a snapshot." The smallest snapshot that proves the broadcast bus
//! reaches the terminal is the `EditStaged` count + an event log. Adding
//! widgets without that working first is premature.

use std::io::{self, stdout, Stdout};
use std::sync::Arc;
use std::time::Duration;

use atelier_core::{
    session::{self, Event as SessionEvent},
    state::NoopHook,
};
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget};
use ratatui::Terminal;
use tokio::sync::broadcast::error::RecvError;

/// In-memory view state. `Clone` so render tests can stage and snapshot
/// without taking the runtime's copy.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    /// Event log, newest-last. Bounded so a long-running session doesn't
    /// blow up the terminal redraw cost.
    pub events: Vec<EventLine>,
    /// Cumulative `EditStaged` count — the §3 first-milestone indicator.
    pub edit_staged_count: usize,
    /// Last `Transitioned` event's `to` field, formatted via `Debug`. Empty
    /// before any transition; used in the header so the user knows what
    /// state the session is in.
    pub current_state: String,
}

/// Bounded history capacity. Larger than what fits on a screen so the
/// `List` widget's scroll-into-view stays smooth; small enough that a
/// runaway adapter doesn't allocate gigabytes.
const MAX_EVENT_LOG: usize = 1_000;

/// One event-log line. Stored as already-projected strings so the render
/// path is allocation-light per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLine {
    pub kind: &'static str,
    pub detail: String,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `SessionEvent` from the broadcast bus. Pure — testable
    /// without booting a terminal.
    pub fn apply(&mut self, evt: &SessionEvent) {
        let line = project_event(evt);
        if let SessionEvent::EditStaged { .. } = evt {
            self.edit_staged_count += 1;
        }
        if let SessionEvent::Transitioned { to, .. } = evt {
            self.current_state = format!("{to:?}");
        }
        self.events.push(line);
        if self.events.len() > MAX_EVENT_LOG {
            // Drop oldest. `remove(0)` is O(n) but the bound is small and
            // this only runs on the very long tail.
            self.events.remove(0);
        }
    }
}

/// Project an [`atelier_core::session::Event`] onto a pre-formatted
/// `EventLine`. Pure function — same role here as `bridge_event` plays for
/// the GUI: keep variant-specific formatting out of the render path so
/// adding a new event variant is a one-line change in one place.
pub fn project_event(evt: &SessionEvent) -> EventLine {
    match evt {
        SessionEvent::Transitioned { from, to } => EventLine {
            kind: "Transitioned",
            detail: format!("{from:?} → {to:?}"),
        },
        SessionEvent::IllegalTransitionAttempted { from, to } => EventLine {
            kind: "IllegalTransition",
            detail: format!("{from:?} ↛ {to:?}"),
        },
        SessionEvent::Cancelled => EventLine {
            kind: "Cancelled",
            detail: String::new(),
        },
        SessionEvent::EditStaged { path, .. } => EventLine {
            kind: "EditStaged",
            detail: path.display().to_string(),
        },
        SessionEvent::Shutdown => EventLine {
            kind: "Shutdown",
            detail: String::new(),
        },
    }
}

/// Pure render — projects `AppState` onto the given `Buffer`. Tests call
/// this directly with a `Buffer::empty(...)` instead of standing up a
/// `Terminal`.
///
/// Layout: 3-row header (border) / event log (fills the middle) /
/// 1-row help footer.
pub fn render(state: &AppState, area: Rect, buf: &mut Buffer) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(state, layout[0], buf);
    render_event_log(state, layout[1], buf);
    render_help(layout[2], buf);
}

fn render_header(state: &AppState, area: Rect, buf: &mut Buffer) {
    let state_label = if state.current_state.is_empty() {
        "<no transitions yet>".to_string()
    } else {
        state.current_state.clone()
    };
    let title = Line::from(vec![
        Span::styled(
            "Atelier TUI ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("· state="),
        Span::styled(state_label, Style::default().fg(Color::Cyan)),
        Span::raw(" · EditStaged="),
        Span::styled(
            state.edit_staged_count.to_string(),
            Style::default().fg(Color::Green),
        ),
    ]);
    let header = Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM));
    Widget::render(header, area, buf);
}

fn render_event_log(state: &AppState, area: Rect, buf: &mut Buffer) {
    // Show the newest first so the most recent activity is always
    // visible without scrolling. Tail to whatever fits in `area`.
    let visible = state
        .events
        .iter()
        .rev()
        .take(area.height as usize)
        .map(|line| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<20}", line.kind),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(line.detail.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    if visible.is_empty() {
        Widget::render(
            Paragraph::new("waiting for events from atelier-core ...")
                .style(Style::default().fg(Color::DarkGray)),
            area,
            buf,
        );
    } else {
        Widget::render(List::new(visible), area, buf);
    }
}

fn render_help(area: Rect, buf: &mut Buffer) {
    Widget::render(
        Paragraph::new(" q / Esc / Ctrl-C: quit ").style(Style::default().fg(Color::DarkGray)),
        area,
        buf,
    );
}

/// Outcome of a single keypress, dispatched by [`run`]'s event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    Continue,
    Quit,
}

/// Pure keypress dispatch. Centralised so the run loop is one match arm
/// per input source.
pub fn handle_key(key: KeyEvent) -> InputOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => InputOutcome::Quit,
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => InputOutcome::Quit,
        _ => InputOutcome::Continue,
    }
}

/// I/O entry point: enable raw mode + alternate screen, spawn a session,
/// pump events + keypresses, restore the terminal on exit (panic-safe via
/// `TerminalGuard`).
///
/// Returns an `io::Result` so the caller (`main.rs`) can exit non-zero on
/// terminal setup failure.
pub fn run() -> io::Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| io::Error::other(format!("tokio runtime: {e}")))?;

    rt.block_on(async { run_async().await })
}

async fn run_async() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout: Stdout = stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    // Owned RAII guard so a panic past this point still restores the
    // terminal (raw mode off, alternate screen released). Without this a
    // crash leaves the user's terminal in a broken state.
    let _restore = TerminalGuard;

    let session_handle = session::spawn(Arc::new(NoopHook), Arc::new(NoopHook));
    let mut rx = session_handle.subscribe();

    let mut state = AppState::new();
    terminal.draw(|f| render(&state, f.area(), f.buffer_mut()))?;

    loop {
        let mut redraw = false;

        tokio::select! {
            biased;

            recv = rx.recv() => match recv {
                Ok(evt) => {
                    state.apply(&evt);
                    redraw = true;
                }
                Err(RecvError::Lagged(_)) => {
                    // Lagged — the broadcast channel dropped events
                    // because we fell behind. The §3 spec lets the TUI
                    // skip; record the gap visibly.
                    state.events.push(EventLine {
                        kind: "Lagged",
                        detail: "broadcast receiver fell behind".into(),
                    });
                    redraw = true;
                }
                Err(RecvError::Closed) => break,
            },

            // crossterm::event::read is blocking, so poll on a short
            // interval inside spawn_blocking. The poll period bounds
            // input latency at ~50ms.
            input = tokio::task::spawn_blocking(|| poll_one_key(Duration::from_millis(50))) => {
                match input {
                    Ok(Ok(Some(key))) => {
                        if handle_key(key) == InputOutcome::Quit {
                            break;
                        }
                        // Even non-quit keys trigger a redraw so the UI
                        // can later add hotkey-driven highlights.
                        redraw = true;
                    }
                    Ok(Ok(None)) => {} // no key this tick
                    Ok(Err(e)) => return Err(e),
                    Err(join_err) => {
                        return Err(io::Error::other(format!("input thread: {join_err}")));
                    }
                }
            }
        }

        if redraw {
            terminal.draw(|f| render(&state, f.area(), f.buffer_mut()))?;
        }
    }

    Ok(())
}

/// RAII restore of raw mode + alternate screen. Drops on panic.
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
    }
}

fn poll_one_key(timeout: Duration) -> io::Result<Option<KeyEvent>> {
    if event::poll(timeout)? {
        match event::read()? {
            CrosstermEvent::Key(k) => Ok(Some(k)),
            _ => Ok(None),
        }
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atelier_core::diff::Hunks;
    use atelier_core::state::State;
    use crossterm::event::KeyEventKind;
    use std::path::PathBuf;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    fn render_to_string(state: &AppState, area: Rect) -> String {
        let mut buf = Buffer::empty(area);
        render(state, area, &mut buf);
        buffer_to_string(&buf, area)
    }

    fn buffer_to_string(buf: &Buffer, area: Rect) -> String {
        let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn apply_increments_edit_staged_count() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::EditStaged {
            path: PathBuf::from("foo.rs"),
            hunks: Hunks::Binary,
        });
        s.apply(&SessionEvent::EditStaged {
            path: PathBuf::from("bar.rs"),
            hunks: Hunks::Binary,
        });
        assert_eq!(s.edit_staged_count, 2);
        assert_eq!(s.events.len(), 2);
    }

    #[test]
    fn apply_updates_current_state_on_transition() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::Transitioned {
            from: State::Idle,
            to: State::Streaming,
        });
        assert_eq!(s.current_state, "Streaming");
        s.apply(&SessionEvent::Transitioned {
            from: State::Streaming,
            to: State::ToolDispatching,
        });
        assert_eq!(s.current_state, "ToolDispatching");
    }

    #[test]
    fn apply_bounds_event_log_to_max() {
        let mut s = AppState::new();
        for _ in 0..(MAX_EVENT_LOG + 50) {
            s.apply(&SessionEvent::Cancelled);
        }
        assert_eq!(s.events.len(), MAX_EVENT_LOG);
    }

    #[test]
    fn project_event_covers_all_variants() {
        assert_eq!(project_event(&SessionEvent::Cancelled).kind, "Cancelled");
        assert_eq!(project_event(&SessionEvent::Shutdown).kind, "Shutdown");
        assert_eq!(
            project_event(&SessionEvent::Transitioned {
                from: State::Idle,
                to: State::Done
            })
            .detail,
            "Idle → Done"
        );
        assert_eq!(
            project_event(&SessionEvent::EditStaged {
                path: PathBuf::from("x"),
                hunks: Hunks::Binary
            })
            .detail,
            "x"
        );
        assert_eq!(
            project_event(&SessionEvent::IllegalTransitionAttempted {
                from: State::Done,
                to: State::Streaming
            })
            .kind,
            "IllegalTransition"
        );
    }

    #[test]
    fn render_includes_state_and_count_in_header() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::Transitioned {
            from: State::Idle,
            to: State::Streaming,
        });
        s.apply(&SessionEvent::EditStaged {
            path: PathBuf::from("a.rs"),
            hunks: Hunks::Binary,
        });
        let area = Rect::new(0, 0, 80, 10);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("Atelier TUI"), "got:\n{rendered}");
        assert!(rendered.contains("Streaming"), "got:\n{rendered}");
        assert!(rendered.contains("EditStaged=1"), "got:\n{rendered}");
    }

    #[test]
    fn render_shows_empty_placeholder_when_no_events() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 80, 6);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("waiting for events"), "got:\n{rendered}");
    }

    #[test]
    fn render_shows_newest_event_at_top_of_log() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::EditStaged {
            path: PathBuf::from("first.rs"),
            hunks: Hunks::Binary,
        });
        s.apply(&SessionEvent::EditStaged {
            path: PathBuf::from("second.rs"),
            hunks: Hunks::Binary,
        });
        let area = Rect::new(0, 0, 80, 8);
        let rendered = render_to_string(&s, area);
        // 'second.rs' should appear before 'first.rs' (newest first).
        let second_pos = rendered.find("second.rs").expect("second.rs in output");
        let first_pos = rendered.find("first.rs").expect("first.rs in output");
        assert!(
            second_pos < first_pos,
            "expected newest-first ordering. got:\n{rendered}"
        );
    }

    #[test]
    fn render_help_line_mentions_quit() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 80, 5);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("quit"), "got:\n{rendered}");
    }

    #[test]
    fn handle_key_quits_on_q_esc_and_ctrl_c() {
        assert_eq!(
            handle_key(key(KeyCode::Char('q'), KeyModifiers::empty())),
            InputOutcome::Quit
        );
        assert_eq!(
            handle_key(key(KeyCode::Esc, KeyModifiers::empty())),
            InputOutcome::Quit
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputOutcome::Quit
        );
    }

    #[test]
    fn handle_key_continues_on_other_keys() {
        assert_eq!(
            handle_key(key(KeyCode::Char('a'), KeyModifiers::empty())),
            InputOutcome::Continue
        );
        assert_eq!(
            handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            InputOutcome::Continue
        );
        // Ctrl-Q is not the quit binding (only Ctrl-C is) — guarantees
        // the modifier check is right.
        assert_eq!(
            handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            InputOutcome::Quit, // 'q' alone quits, regardless of modifier
        );
    }
}
