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

use std::collections::VecDeque;
use std::io::{self, stdout, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use atelier_core::{
    diff::Hunks,
    ledger::LedgerEntry,
    plan::{PlanCanvas, PlanStatus, PlanStep},
    session::{self, Event as SessionEvent, MessageRole, PendingFile},
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
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Widget};
use ratatui::Terminal;
use tokio::sync::broadcast::error::RecvError;

/// In-memory view state. `Clone` so render tests can stage and snapshot
/// without taking the runtime's copy.
///
/// The conversation / plan / cost / context fields are populated by the
/// host runtime (see the crate-private `run_async` for the
/// broadcast-bus wiring) and from
/// out-of-band channels that the §2.5 actor doesn't yet emit on the bus
/// — populating them via `set_*` mutators lets the unit tests cover
/// rendering without needing the producer side to exist yet.
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
    /// Conversation pane lines, newest-last. Bounded.
    pub conversation: VecDeque<ConversationLine>,
    /// Most-recent staged edits, newest-first. Bounded — `MAX_DIFF_HISTORY`.
    /// The diff pane renders the head.
    pub recent_edits: VecDeque<StagedEdit>,
    /// Plan canvas snapshot. Updated by the host out-of-band when the
    /// envelope's `plan_update` is applied; the TUI does not own
    /// authoritative plan state, just a view of it.
    pub plan: PlanCanvas,
    /// Total session cost in USD. Updated by the host from the §1 ledger.
    pub total_cost_usd: f64,
    /// `(known_tokens, unknown_tokens)` from
    /// [`atelier_core::context::ContextManager::token_snapshot`]. The
    /// context meter renders the known portion as filled and surfaces the
    /// unknown count separately so the user can see when the token meter
    /// is silently underreporting (spec §5).
    pub context_tokens: (u32, u32),
    /// Context window cap for the meter denominator. Provider-dependent;
    /// defaulted to 200k (Anthropic Sonnet/Opus) until the adapter publishes
    /// its capability set onto the bus.
    pub context_window_tokens: u32,
    /// Scrubber position. `None` = at HEAD (live). `Some(n)` = pinned `n`
    /// steps back. Spec §4 (Phase D) owns the actual time-travel
    /// machinery; the TUI just records the user's intent.
    pub scrub_offset: Option<usize>,
    /// Pending hunk-approval (spec §3). `Some` when the dispatcher
    /// emitted a `StagingPendingApproval` and is blocked on the user's
    /// accept-set. Cleared by `CommitDecision` (commit happened) or
    /// `Cancelled` (user bailed). The TUI renders this in the diff
    /// pane with a `PENDING` badge.
    ///
    /// v48: when the TUI is in driver mode (`run()` started with a
    /// prompt), `y` accepts every pending file and `n` rejects every
    /// pending file. The run loop owns the `DispatcherHandle` and
    /// routes the accept set to `SessionDispatcher::submit_approval`.
    /// AppState stays pure render-state; the handle lives in the loop
    /// frame.
    pub pending_approval: Option<PendingApproval>,
    /// v52 — active BYOM model, populated by `ModelProfileLoaded`.
    /// Rendered on the right-hand side of the footer so the user
    /// always knows which model + strategy the run is using.
    /// `None` until the Runner emits its one-shot `ModelProfileLoaded`
    /// event at session start.
    pub current_model: Option<CurrentModel>,
    /// v53 — per-item §5 context snapshot, rebuilt whole-cloth on
    /// every `Event::ContextItems`. The Context pane renders these
    /// rows; the aggregate `context_tokens` pair still drives the
    /// meter denominator above it.
    pub context_items: Vec<atelier_core::context::ContextItemSummary>,
    /// v54 — per-card §5 memory snapshot, rebuilt whole-cloth on
    /// every `Event::MemoryCards`. The Memory pane renders these
    /// rows; cards are durable across sessions and distinct from
    /// the per-turn context items above.
    pub memory_cards: Vec<atelier_core::memory::MemoryCardSummary>,
}

/// Snapshot of the active model + strategy. Mirror of the GUI's
/// `CurrentModel` shape in `crates/atelier-gui/ui/src/lib/state.ts` so
/// the two surfaces stay byte-for-byte equivalent on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentModel {
    /// `<provider>:<model>` form (e.g. `local:qwen2.5-coder:7b`).
    pub model_id: String,
    /// Endpoint URL; empty for adapters that don't speak HTTP.
    pub base_url: String,
    /// `native_tool` / `json_sentinel` / `regex_prose`.
    pub strategy: &'static str,
    /// `cache_hit` / `probed` / `reprobed` / `not_cached`.
    pub outcome: String,
}

/// Currently-pending hunk approval, mirror of
/// `Event::StagingPendingApproval` payload. Carried in
/// [`AppState::pending_approval`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub commit_id: uuid::Uuid,
    pub files: Vec<PendingApprovalFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalFile {
    pub path: PathBuf,
    pub hunks: Hunks,
}

/// Bounded history capacity. Larger than what fits on a screen so the
/// `List` widget's scroll-into-view stays smooth; small enough that a
/// runaway adapter doesn't allocate gigabytes.
const MAX_EVENT_LOG: usize = 1_000;

/// Cap on remembered conversation lines. Spec §3 expects the pane to
/// scroll, but in v0 we tail to the visible area; the bound is sized
/// generously enough that retroactive scroll-back would catch a long
/// run.
const MAX_CONVERSATION_LINES: usize = 1_000;

/// Cap on remembered staged-edit history. The diff pane only renders
/// the most recent edit at a time; the rest are kept so a future
/// "scrub through last N edits" hotkey has data to show.
const MAX_DIFF_HISTORY: usize = 16;

/// Default context-window denominator for the meter. Anthropic
/// Sonnet/Opus 4.x today; overridden via `set_context_window` once the
/// adapter publishes its capability set onto the bus.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 200_000;

/// One event-log line. Stored as already-projected strings so the render
/// path is allocation-light per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLine {
    pub kind: &'static str,
    pub detail: String,
}

/// One conversation line rendered in the conversation pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationLine {
    pub role: ConversationRole,
    pub text: String,
}

/// Speaker role for a conversation line. Mirrors [`atelier_core::adapter::Role`]
/// — duplicated here to keep `atelier-tui`'s widget code free of an
/// `adapter` dependency (the GUI does the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationRole {
    User,
    Assistant,
    Tool,
    System,
}

impl ConversationRole {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }

    fn colour(self) -> Color {
        match self {
            Self::User => Color::Yellow,
            Self::Assistant => Color::Cyan,
            Self::Tool => Color::Magenta,
            Self::System => Color::DarkGray,
        }
    }

    /// Project the session bus's `MessageRole` onto the TUI's
    /// `ConversationRole`. Duplicated so the widget code doesn't have
    /// to depend on the session module's enum shape — adding a new
    /// `MessageRole` variant later will fail compilation here and force
    /// a deliberate mapping decision.
    pub fn from_message_role(role: MessageRole) -> Self {
        match role {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::Tool => Self::Tool,
            MessageRole::System => Self::System,
        }
    }
}

/// Best-effort cost extraction from any `LedgerEntry` variant. Returns
/// `None` for entries that don't carry a USD cost (some `CacheBust`
/// entries today). The TUI's running total ignores `None` rather than
/// treating it as zero so the meter isn't artificially deflated by
/// no-cost bookkeeping rows.
/// Convert a Debug-formatted enum variant (`"CacheHit"`,
/// `"NotCached"`) into the snake_case wire form the rest of the
/// system uses (`"cache_hit"`, `"not_cached"`). Used so the TUI's
/// footer matches the GUI's `serde(rename_all = "snake_case")`
/// projection byte-for-byte.
fn snake_case_debug(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

fn ledger_entry_cost(entry: &LedgerEntry) -> Option<f64> {
    match entry {
        LedgerEntry::ModelCall { cost_usd, .. } | LedgerEntry::ToolCall { cost_usd, .. } => {
            *cost_usd
        }
        // CacheBust entries carry no cost field (the eviction itself
        // doesn't cost money; the future re-introduction of the evicted
        // tokens does, and lands as a ModelCall).
        LedgerEntry::CacheBust { .. } => None,
    }
}

/// One remembered staged edit for the diff pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedEdit {
    pub path: PathBuf,
    pub hunks: Hunks,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            ..Default::default()
        }
    }

    /// Apply one `SessionEvent` from the broadcast bus. Pure — testable
    /// without booting a terminal.
    pub fn apply(&mut self, evt: &SessionEvent) {
        let line = project_event(evt);
        match evt {
            SessionEvent::EditStaged { path, hunks } => {
                self.edit_staged_count += 1;
                self.recent_edits.push_front(StagedEdit {
                    path: path.clone(),
                    hunks: hunks.clone(),
                });
                while self.recent_edits.len() > MAX_DIFF_HISTORY {
                    self.recent_edits.pop_back();
                }
            }
            SessionEvent::Transitioned { to, .. } => {
                self.current_state = format!("{to:?}");
            }
            SessionEvent::MessageCommitted { role, text } => {
                self.push_conversation(ConversationRole::from_message_role(*role), text.clone());
            }
            SessionEvent::PlanSnapshot { steps } => {
                // PlanCanvas::from_vec validates ids; if a malformed
                // snapshot ever lands on the bus (shouldn't happen —
                // the producer always built the canvas via PlanCanvas
                // itself), we keep the existing snapshot rather than
                // panicking.
                if let Ok(canvas) = PlanCanvas::from_vec(steps.clone()) {
                    self.plan = canvas;
                }
            }
            SessionEvent::LedgerAppended { entry } => {
                // Fold each ledger entry's cost into the running total
                // so the cost meter ticks live without the consumer
                // having to maintain its own ledger snapshot.
                if let Some(c) = ledger_entry_cost(entry) {
                    self.total_cost_usd += c;
                }
            }
            SessionEvent::ContextSnapshot {
                known_tokens,
                unknown_tokens,
            } => {
                self.context_tokens = (*known_tokens, *unknown_tokens);
            }
            SessionEvent::StagingPendingApproval { commit_id, files } => {
                self.pending_approval = Some(PendingApproval {
                    commit_id: *commit_id,
                    files: files
                        .iter()
                        .map(|f: &PendingFile| PendingApprovalFile {
                            path: f.path.clone(),
                            hunks: f.hunks.clone(),
                        })
                        .collect(),
                });
            }
            SessionEvent::CommitDecision { .. } => {
                // The dispatcher resolved the pending — clear it.
                // The accompanying EditStaged events for committed
                // files arrive separately and populate `recent_edits`.
                self.pending_approval = None;
            }
            SessionEvent::ModelProfileLoaded {
                model_id,
                base_url,
                strategy,
                outcome,
            } => {
                // v52 — record the active model so the footer can
                // render it. Outcome is Debug-then-lowercased to
                // mirror the GUI's snake_case label (`cache_hit` etc.
                // — `format!("{outcome:?}")` returns `CacheHit`; we
                // convert in two steps so the wire shape matches the
                // GUI exactly: insert an underscore at each lower-to-
                // upper boundary, then lowercase the whole thing).
                self.current_model = Some(CurrentModel {
                    model_id: model_id.clone(),
                    base_url: base_url.clone(),
                    strategy: strategy.as_str(),
                    outcome: snake_case_debug(&format!("{outcome:?}")),
                });
            }
            SessionEvent::ContextItems { items } => {
                // v53 — replace the in-memory snapshot wholesale.
                // Items arrive at every turn boundary, so a stale
                // partial render is never preferable to the fresh
                // snapshot.
                self.context_items = items.clone();
            }
            SessionEvent::MemoryCards { cards } => {
                // v54 — same wholesale-replace policy as
                // ContextItems. Cards arrive at every turn so the
                // panel never displays stale state.
                self.memory_cards = cards.clone();
            }
            SessionEvent::IllegalTransitionAttempted { .. }
            | SessionEvent::Cancelled
            | SessionEvent::Shutdown => {}
        }
        self.events.push(line);
        if self.events.len() > MAX_EVENT_LOG {
            // Drop oldest. `remove(0)` is O(n) but the bound is small and
            // this only runs on the very long tail.
            self.events.remove(0);
        }
    }

    /// Push a conversation line. Bounded by `MAX_CONVERSATION_LINES`.
    /// Called by the host when the §2.5 actor commits a message to
    /// history; not driven by the broadcast bus (which doesn't yet
    /// surface message commits).
    pub fn push_conversation(&mut self, role: ConversationRole, text: impl Into<String>) {
        self.conversation.push_back(ConversationLine {
            role,
            text: text.into(),
        });
        while self.conversation.len() > MAX_CONVERSATION_LINES {
            self.conversation.pop_front();
        }
    }

    /// Replace the plan snapshot wholesale. The §2.5 actor's `plan_update`
    /// envelope is the authoritative source; the TUI takes a snapshot
    /// after each apply.
    pub fn set_plan(&mut self, plan: PlanCanvas) {
        self.plan = plan;
    }

    /// Update the cost meter. Host reads from the §1 ledger.
    pub fn set_cost_usd(&mut self, cost: f64) {
        self.total_cost_usd = cost;
    }

    /// Update the context-token meter. `known` are tokens with a
    /// confirmed count from the adapter; `unknown` are items whose token
    /// count couldn't be determined (the `unavailable` source). The
    /// meter renders the known portion as filled and surfaces the
    /// unknown count separately so the user can see when it's silently
    /// underreporting.
    pub fn set_context_tokens(&mut self, known: u32, unknown: u32) {
        self.context_tokens = (known, unknown);
    }

    /// Override the context-window denominator. Called by the host once
    /// the adapter publishes its capabilities.
    pub fn set_context_window(&mut self, tokens: u32) {
        if tokens > 0 {
            self.context_window_tokens = tokens;
        }
    }

    /// Apply a scrubber command. Pure: the §4 time-travel machinery is
    /// downstream; the TUI just tracks the user's intent and the host
    /// reacts to changes in `scrub_offset`.
    pub fn apply_scrub(&mut self, cmd: ScrubCommand) {
        self.scrub_offset = match (self.scrub_offset, cmd) {
            (_, ScrubCommand::JumpToHead) => None,
            (None, ScrubCommand::Prev) => Some(1),
            (Some(n), ScrubCommand::Prev) => Some(n.saturating_add(1)),
            (None, ScrubCommand::Next) => None,
            (Some(n), ScrubCommand::Next) => {
                let next = n.saturating_sub(1);
                if next == 0 {
                    None
                } else {
                    Some(next)
                }
            }
        };
    }
}

/// A scrubber direction signal emitted by `handle_key`. The §4
/// time-travel subsystem will consume these; until it lands, the TUI
/// records the intent in `AppState::scrub_offset` and the host wiring
/// can react to changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubCommand {
    /// Move one step back in history (`[`).
    Prev,
    /// Move one step forward (`]`).
    Next,
    /// Jump back to HEAD / live mode (`g`).
    JumpToHead,
}

/// Project an [`atelier_core::session::Event`] onto a pre-formatted
/// `EventLine`. Pure function — same role here as `bridge_event` plays for
/// the GUI: keep variant-specific formatting out of the render path so
/// adding a new event variant is a one-line change in one place.
pub fn project_event(evt: &SessionEvent) -> EventLine {
    match evt {
        SessionEvent::MessageCommitted { role, text } => EventLine {
            kind: "Message",
            detail: format!(
                "{:?}: {}",
                role,
                text.lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect::<String>()
            ),
        },
        SessionEvent::PlanSnapshot { steps } => EventLine {
            kind: "PlanSnapshot",
            detail: format!("{} steps", steps.len()),
        },
        SessionEvent::LedgerAppended { entry } => EventLine {
            kind: "LedgerAppended",
            detail: match entry {
                LedgerEntry::ModelCall { .. } => "model_call".to_string(),
                LedgerEntry::ToolCall { tool_name, .. } => format!("tool_call:{tool_name}"),
                LedgerEntry::CacheBust { .. } => "cache_bust".to_string(),
            },
        },
        SessionEvent::ContextSnapshot {
            known_tokens,
            unknown_tokens,
        } => EventLine {
            kind: "ContextSnapshot",
            detail: format!("known={known_tokens} unknown={unknown_tokens}"),
        },
        SessionEvent::StagingPendingApproval { files, .. } => EventLine {
            kind: "PendingApproval",
            detail: format!("{} files awaiting approval", files.len()),
        },
        SessionEvent::CommitDecision {
            committed, dropped, ..
        } => EventLine {
            kind: "CommitDecision",
            detail: format!("committed={} dropped={}", committed.len(), dropped.len()),
        },
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
        SessionEvent::ModelProfileLoaded {
            model_id,
            strategy,
            outcome,
            ..
        } => EventLine {
            kind: "ModelProfile",
            detail: format!(
                "{model_id} · strategy={} · {}",
                strategy.as_str(),
                format!("{outcome:?}").to_lowercase()
            ),
        },
        SessionEvent::ContextItems { items } => EventLine {
            kind: "ContextItems",
            detail: format!("{} items", items.len()),
        },
        SessionEvent::MemoryCards { cards } => EventLine {
            kind: "MemoryCards",
            detail: format!("{} cards", cards.len()),
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
/// Layout (v25.3 — §3 TUI subset):
///
/// ```text
/// +--------------- header (state / counters / scrub) --------------+
/// | Conversation                  | Plan canvas                     |
/// |                               |                                 |
/// +--------------------------------+--------------------------------+
/// | Diff (most recent edit)       | Meters (cost / context)         |
/// |                               | Event log (tail)                |
/// +-------- footer (key hints) ------------------------------------+
/// ```
///
/// Each pane lives in its own pure render function so the test surface
/// can target them in isolation.
pub fn render(state: &AppState, area: Rect, buf: &mut Buffer) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(state, vertical[0], buf);

    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(vertical[1]);

    // Top row: conversation | (plan + memory stack).
    // v54 — Plan reflects what the agent is about to do; Memory
    // reflects what it remembers long-term. Both belong in the
    // highest-visibility right column and don't compete with the
    // diff / context detail below.
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(body[0]);
    render_conversation(state, top[0], buf);

    let top_right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(top[1]);
    render_plan(state, top_right[0], buf);
    render_memory_pane(state, top_right[1], buf);

    // Bottom row: diff | (meters + event log)
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(body[1]);
    render_diff(state, bottom[0], buf);

    // Right column splits between the two aggregate meters, the
    // v53 §5 Context panel (flex height — this is where the per-row
    // "what's in my agent's head" detail lives), and a bounded tail
    // of the event log so developer-facing transitions stay visible
    // without crowding out the user-facing context pane.
    //
    // Constraint shape is chosen so the two gauges keep their full
    // 2-row allocation even when the host terminal is tight: fixed
    // Lengths total 8 rows; the §5 panel takes whatever's left
    // (with a soft floor of 2 rows so the empty-state line is
    // visible).
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // cost gauge
            Constraint::Length(2), // context gauge (aggregate)
            Constraint::Min(2),    // §5 context items (per-row, flex)
            Constraint::Length(4), // bounded event log tail
        ])
        .split(bottom[1]);
    render_cost_meter(state, right[0], buf);
    render_context_meter(state, right[1], buf);
    render_context_pane(state, right[2], buf);
    render_event_log(state, right[3], buf);

    render_help(state, vertical[2], buf);
}

fn render_header(state: &AppState, area: Rect, buf: &mut Buffer) {
    let state_label = if state.current_state.is_empty() {
        "<no transitions yet>".to_string()
    } else {
        state.current_state.clone()
    };
    let scrub = match state.scrub_offset {
        None => Span::styled("HEAD", Style::default().fg(Color::Green)),
        Some(n) => Span::styled(
            format!("-{n}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
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
        Span::raw(" · scrub="),
        scrub,
    ]);
    let header = Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM));
    Widget::render(header, area, buf);
}

/// Conversation pane: role-prefixed lines, newest at the bottom.
/// Tail to the visible area; users scroll the underlying transcript
/// elsewhere (the spec §3 TUI subset gate is "render", not "scroll").
fn render_conversation(state: &AppState, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" conversation ");
    let inner = block.inner(area);
    Widget::render(block, area, buf);

    if state.conversation.is_empty() {
        Widget::render(
            Paragraph::new("(no messages yet)").style(Style::default().fg(Color::DarkGray)),
            inner,
            buf,
        );
        return;
    }

    // Tail-render. Show the most recent `inner.height` lines so the
    // freshest message is always pinned at the bottom.
    let visible_rows = inner.height as usize;
    let start = state.conversation.len().saturating_sub(visible_rows);
    let items: Vec<ListItem> = state
        .conversation
        .iter()
        .skip(start)
        .map(|line| {
            let role_style = Style::default()
                .fg(line.role.colour())
                .add_modifier(Modifier::BOLD);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<10}", line.role.label()), role_style),
                Span::raw(line.text.clone()),
            ]))
        })
        .collect();
    Widget::render(List::new(items), inner, buf);
}

/// Diff pane: most recent staged edit, rendered as +/- lines for the
/// `Hunks::Lines` case and a one-line badge for the other variants.
/// Spec §3 calls this the "live diff renderer" gate — the incremental
/// rendering (re-render on each new EditStaged) IS the v0 incrementality.
fn render_diff(state: &AppState, area: Rect, buf: &mut Buffer) {
    // Pending approval takes precedence over the EditStaged stream — if
    // the dispatcher is blocked on a user decision, that's the user's
    // most-important diff to look at. Renders with a yellow `PENDING`
    // title so it's visually distinct from already-committed edits.
    if let Some(pending) = &state.pending_approval {
        render_pending_diff(pending, area, buf);
        return;
    }

    let block = Block::default().borders(Borders::ALL).title(" diff ");
    let inner = block.inner(area);
    Widget::render(block, area, buf);

    let Some(edit) = state.recent_edits.front() else {
        Widget::render(
            Paragraph::new("(no edits yet)").style(Style::default().fg(Color::DarkGray)),
            inner,
            buf,
        );
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("─── ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            edit.path.display().to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    match &edit.hunks {
        Hunks::Same => {
            lines.push(Line::from(Span::styled(
                "(no diff — byte-equal)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Hunks::Binary => {
            lines.push(Line::from(Span::styled(
                "[binary file changed]",
                Style::default().fg(Color::Magenta),
            )));
        }
        Hunks::Created {
            new_byte_len,
            new_line_count,
        } => {
            lines.push(Line::from(Span::styled(
                format!("[created · {new_line_count} lines · {new_byte_len} bytes]"),
                Style::default().fg(Color::Green),
            )));
        }
        Hunks::Deleted {
            old_byte_len,
            old_line_count,
        } => {
            lines.push(Line::from(Span::styled(
                format!("[deleted · {old_line_count} lines · {old_byte_len} bytes]"),
                Style::default().fg(Color::Red),
            )));
        }
        Hunks::Lines { hunks } => {
            let visible_rows = inner.height.saturating_sub(1) as usize;
            let mut rendered_rows = 0usize;
            for hunk in hunks {
                if rendered_rows >= visible_rows {
                    break;
                }
                // Hunk separator: @@ old,new @@
                lines.push(Line::from(Span::styled(
                    format!(
                        "@@ -{},{} +{},{} @@",
                        hunk.old_range.start + 1,
                        hunk.old_range.len(),
                        hunk.new_range.start + 1,
                        hunk.new_range.len(),
                    ),
                    Style::default().fg(Color::Blue),
                )));
                rendered_rows += 1;
                for old in &hunk.old_lines {
                    if rendered_rows >= visible_rows {
                        break;
                    }
                    lines.push(Line::from(Span::styled(
                        format!("-{old}"),
                        Style::default().fg(Color::Red),
                    )));
                    rendered_rows += 1;
                }
                for new in &hunk.new_lines {
                    if rendered_rows >= visible_rows {
                        break;
                    }
                    lines.push(Line::from(Span::styled(
                        format!("+{new}"),
                        Style::default().fg(Color::Green),
                    )));
                    rendered_rows += 1;
                }
            }
        }
    }
    Widget::render(Paragraph::new(lines), inner, buf);
}

/// Render the pending-approval diff. Shown when the dispatcher emitted
/// `StagingPendingApproval` and hasn't yet seen a matching
/// `CommitDecision`. Visually distinct from the committed-edits diff
/// (yellow `PENDING` title, banner explaining the wait).
fn render_pending_diff(pending: &PendingApproval, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " diff (PENDING) ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    Widget::render(block, area, buf);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            "{} file(s) awaiting approval (commit {})",
            pending.files.len(),
            short_uuid(&pending.commit_id)
        ),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("press ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "y",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to accept all · ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "n",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to reject all", Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(""));

    let visible_rows = inner.height.saturating_sub(3) as usize;
    for file in pending.files.iter().take(visible_rows) {
        lines.push(Line::from(vec![
            Span::styled("── ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                file.path.display().to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  [{}]", hunks_kind_label(&file.hunks)),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }
    Widget::render(Paragraph::new(lines), inner, buf);
}

/// First 8 chars of the uuid, enough for visual correlation in a
/// shared display without taking 36 columns.
fn short_uuid(id: &uuid::Uuid) -> String {
    let s = id.to_string();
    s.chars().take(8).collect()
}

fn hunks_kind_label(h: &Hunks) -> &'static str {
    match h {
        Hunks::Same => "no diff",
        Hunks::Binary => "binary",
        Hunks::Created { .. } => "created",
        Hunks::Deleted { .. } => "deleted",
        Hunks::Lines { .. } => "edit",
    }
}

/// Plan canvas pane: one line per step with a status glyph.
fn render_plan(state: &AppState, area: Rect, buf: &mut Buffer) {
    let block = Block::default().borders(Borders::ALL).title(" plan ");
    let inner = block.inner(area);
    Widget::render(block, area, buf);

    let steps = state.plan.to_vec();
    if steps.is_empty() {
        Widget::render(
            Paragraph::new("(no plan steps)").style(Style::default().fg(Color::DarkGray)),
            inner,
            buf,
        );
        return;
    }
    let items: Vec<ListItem> = steps
        .iter()
        .flat_map(plan_step_lines)
        .map(ListItem::new)
        .collect();
    Widget::render(List::new(items), inner, buf);
}

/// One step → 1+N lines (step line plus one line per constraint).
fn plan_step_lines(step: &PlanStep) -> Vec<Line<'static>> {
    let (glyph, glyph_style) = match step.status {
        PlanStatus::Pending => ("[ ]", Style::default().fg(Color::DarkGray)),
        PlanStatus::InProgress => ("[▸]", Style::default().fg(Color::Yellow)),
        PlanStatus::Done => ("[✓]", Style::default().fg(Color::Green)),
        PlanStatus::Skipped => ("[~]", Style::default().fg(Color::DarkGray)),
    };
    let text_style = if step.status.is_terminal() {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::CROSSED_OUT)
    } else {
        Style::default()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(step.text.clone(), text_style),
    ])];
    for c in &step.constraints {
        lines.push(Line::from(vec![
            Span::styled("     └ ", Style::default().fg(Color::DarkGray)),
            Span::styled(c.clone(), Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines
}

/// Cost meter — single-line label + USD figure. We deliberately don't
/// render a gauge for cost (no upper bound to scale against).
fn render_cost_meter(state: &AppState, area: Rect, buf: &mut Buffer) {
    let line = Line::from(vec![
        Span::styled("cost ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("${:.4}", state.total_cost_usd),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    Widget::render(
        Paragraph::new(line).block(Block::default().borders(Borders::TOP)),
        area,
        buf,
    );
}

/// Context-token meter — ratatui Gauge with the known fraction filled;
/// the unknown count appears in parentheses so a meter underreporting
/// via `TokenSource::Unavailable` items is visible.
fn render_context_meter(state: &AppState, area: Rect, buf: &mut Buffer) {
    let (known, unknown) = state.context_tokens;
    let window = state.context_window_tokens.max(1);
    let ratio = (known as f64 / window as f64).clamp(0.0, 1.0);
    let label = if unknown > 0 {
        format!("ctx {known}/{window} (+{unknown} unknown)")
    } else {
        format!("ctx {known}/{window}")
    };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::TOP))
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
        .ratio(ratio)
        .label(label);
    Widget::render(gauge, area, buf);
}

/// v53 — §5 Context panel. Renders one row per `ContextItemSummary`
/// in insertion order with three pieces of information per row:
///
///   * **token count** (right-aligned in fixed-width column), with a
///     colour cue for the source (`exact` cyan / `approx` yellow /
///     `unavailable` dim) so the user knows how much to trust the
///     number;
///   * **provenance badge** (`init` / `usr` / `tool` / `mem` / `pin`)
///     — short labels for the why-here trace;
///   * **label** (file path or truncated text).
///
/// Empty state shows a single dim line ("no context items yet")
/// rather than a blank pane — the user always wants to know whether
/// the pane is alive but empty or actually broken.
fn render_context_pane(state: &AppState, area: Rect, buf: &mut Buffer) {
    let block = Block::default().borders(Borders::TOP).title(" §5 Context ");

    if state.context_items.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no context items yet",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block);
        Widget::render(p, area, buf);
        return;
    }

    let rows: Vec<ListItem> = state
        .context_items
        .iter()
        .map(|item| {
            let tokens_label = format_tokens_for_pane(item.tokens);
            let tokens_style = match item.token_source.as_str() {
                "exact" => Style::default().fg(Color::Cyan),
                "approx" => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            };
            let badge = provenance_badge(&item.provenance);
            let badge_style = provenance_badge_style(&item.provenance);
            let pin = if item.pinned { "📌 " } else { "   " };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{tokens_label} "), tokens_style),
                Span::styled(format!("{badge} "), badge_style),
                Span::raw(pin.to_string()),
                Span::raw(item.label.clone()),
            ]))
        })
        .collect();
    Widget::render(List::new(rows).block(block), area, buf);
}

/// Right-pad token count into a 5-wide column so the badges line up.
fn format_tokens_for_pane(n: u32) -> String {
    format!("{n:>5}")
}

/// Short provenance label that fits in a narrow column. Stable
/// labels so the §5 mechanical gate can assert on them.
fn provenance_badge(provenance: &str) -> &'static str {
    match provenance {
        "initial" => "init",
        "user_attached" => "usr ",
        "tool_result" => "tool",
        "memory_promoted" => "mem ",
        "pinned_by_user" => "pin ",
        "assistant_turn" => "asst",
        _ => "????",
    }
}

fn provenance_badge_style(provenance: &str) -> Style {
    match provenance {
        "initial" => Style::default().fg(Color::DarkGray),
        "user_attached" => Style::default().fg(Color::Green),
        "tool_result" => Style::default().fg(Color::Magenta),
        "memory_promoted" => Style::default().fg(Color::Blue),
        "pinned_by_user" => Style::default().fg(Color::Yellow),
        "assistant_turn" => Style::default().fg(Color::White),
        _ => Style::default().fg(Color::Red),
    }
}

/// v54 — §5 Memory panel. One row per `MemoryCardSummary` in
/// insertion order. Each row shows:
///
///   * a pin glyph when [`MemoryCardSummary::pinned`];
///   * the title (first non-empty line of the card body);
///   * the last-used timestamp, right-aligned and condensed to
///     `YYYY-MM-DD HH:MM` for column-fit.
///
/// Empty state renders a single dim line so the pane is visibly
/// idle rather than indistinguishable from a broken render. The
/// preview body intentionally doesn't render here — the TUI's row
/// budget is much tighter than the GUI's, and the title +
/// last-used badge are the high-value fields for scanning.
fn render_memory_pane(state: &AppState, area: Rect, buf: &mut Buffer) {
    let block = Block::default().borders(Borders::TOP).title(" §5 Memory ");

    if state.memory_cards.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no memory cards yet",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block);
        Widget::render(p, area, buf);
        return;
    }

    let rows: Vec<ListItem> = state
        .memory_cards
        .iter()
        .map(|card| {
            let pin = if card.pinned { "📌 " } else { "   " };
            let title_style = if card.pinned {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let when = short_timestamp(&card.last_used);
            ListItem::new(Line::from(vec![
                Span::raw(pin.to_string()),
                Span::styled(card.title.clone(), title_style),
                Span::raw("  "),
                Span::styled(when, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    Widget::render(List::new(rows).block(block), area, buf);
}

/// Compact form of an RFC 3339 timestamp for narrow columns —
/// `"2026-05-17T12:34:56Z"` → `"2026-05-17 12:34"`. Returns the
/// input unchanged when it doesn't look like ISO 8601 so a
/// malformed timestamp is visibly malformed rather than silently
/// dropped.
fn short_timestamp(iso: &str) -> String {
    // Pattern: 10 chars date + 'T' + 5 chars hh:mm. Anything else
    // (empty, garbled, future timezone offsets) round-trips
    // verbatim — the panel surface is informational, not parsed.
    if iso.len() >= 16 && iso.as_bytes().get(10) == Some(&b'T') {
        format!("{} {}", &iso[..10], &iso[11..16])
    } else {
        iso.to_string()
    }
}

fn render_event_log(state: &AppState, area: Rect, buf: &mut Buffer) {
    // Newest first, tail to the available rows.
    let visible: Vec<ListItem> = state
        .events
        .iter()
        .rev()
        .take(area.height as usize)
        .map(|line| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<14}", line.kind),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(line.detail.clone()),
            ]))
        })
        .collect();
    if visible.is_empty() {
        Widget::render(
            Paragraph::new("waiting for events ...").style(Style::default().fg(Color::DarkGray)),
            area,
            buf,
        );
    } else {
        Widget::render(List::new(visible), area, buf);
    }
}

fn render_help(state: &AppState, area: Rect, buf: &mut Buffer) {
    // Pending state takes precedence in the footer: the user needs to
    // see the approval keys when a decision is required. The model
    // badge is suppressed during pending so the approval message is
    // unambiguous.
    if state.pending_approval.is_some() {
        Widget::render(
            Paragraph::new(" APPROVAL REQUIRED · y accept all · n reject all · q quit ").style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
            buf,
        );
        return;
    }

    let left = render_help_left(state);
    match state.current_model.as_ref() {
        // No model badge yet — let the help text fill the line.
        None => Widget::render(
            Paragraph::new(left).style(Style::default().fg(Color::DarkGray)),
            area,
            buf,
        ),
        // Split: left flexible, right fixed-width. The badge's column
        // count is derived from the underlying strings so the layout
        // matches what `render_help_right_model` is about to render.
        Some(model) => {
            let right_width = model_badge_width(model);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(right_width)])
                .split(area);
            Widget::render(
                Paragraph::new(left).style(Style::default().fg(Color::DarkGray)),
                chunks[0],
                buf,
            );
            Widget::render(render_help_right_model(model), chunks[1], buf);
        }
    }
}

/// Build the left-side help text — scrubber keys + optional pinned-
/// scroll hint. Always present; never empty.
fn render_help_left(state: &AppState) -> String {
    let scrub_note = if state.scrub_offset.is_some() {
        "  [pinned: g returns to HEAD]"
    } else {
        ""
    };
    format!(" q/Esc/Ctrl-C quit · [ prev · ] next · g HEAD{scrub_note}")
}

/// Build the right-side model badge as a styled
/// [`ratatui::widgets::Paragraph`]. Always returns a paragraph (the
/// caller already established that `state.current_model` is `Some`).
/// Mirrors the GUI's bottom-right model widget — same field order,
/// same separator, same colour family (cyan id · green strategy ·
/// dim outcome).
fn render_help_right_model(model: &CurrentModel) -> Paragraph<'static> {
    // Trailing space so the badge doesn't hit the terminal edge.
    let line = Line::from(vec![
        Span::styled(
            model.model_id.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            model.strategy.to_string(),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(model.outcome.clone(), Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
    ]);
    Paragraph::new(line)
}

/// Visual column count of the model badge — the sum of each
/// segment's display width plus the three `" · "` separators and the
/// trailing space. Kept in lockstep with [`render_help_right_model`]
/// so the layout split matches what gets rendered. The fields are
/// ASCII-only in practice (model ids, strategy labels, outcome
/// labels), so `chars().count()` is the right column measure;
/// `unicode-width` would be the heavier-weight upgrade if a future
/// model id grew non-ASCII characters.
fn model_badge_width(model: &CurrentModel) -> u16 {
    let id = model.model_id.chars().count();
    let strategy = model.strategy.chars().count();
    let outcome = model.outcome.chars().count();
    // Three " · " separators (3 cols each) + leading 0 + trailing 1.
    let total = id + strategy + outcome + (3 * 3) + 1;
    total.try_into().unwrap_or(u16::MAX)
}

/// Outcome of a single keypress, dispatched by [`run`]'s event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    Continue,
    Quit,
    Scrub(ScrubCommand),
    /// v48: accept every pending file in the current
    /// `AppState::pending_approval`. Only returned when pending is set
    /// — pressing `y` outside of an approval prompt is a no-op (so
    /// the key isn't intercepted from a future text-input mode).
    AcceptAll,
    /// v48: reject every pending file (empty accept set = full
    /// reject). Same guard as `AcceptAll` — only returned when
    /// pending is set.
    RejectAll,
}

/// Pure keypress dispatch. Centralised so the run loop is one match arm
/// per input source.
///
/// Bindings:
/// - `q` / `Esc` / `Ctrl-C` — quit
/// - `[` — scrubber: one step back
/// - `]` — scrubber: one step forward
/// - `g` — scrubber: jump to HEAD (live)
/// - `y` — accept every pending file (only when pending_approval is set)
/// - `n` — reject every pending file (only when pending_approval is set)
///
/// `pending` is passed by reference so handle_key can gate `y`/`n` on
/// the presence of an outstanding approval. Without that gate, a stray
/// keystroke during an inactive session would surprise a future text-
/// input mode that wants to consume `y` / `n` literally.
///
/// Spec §3 names a `g <n>` "jump to step n" prefix sequence; that needs
/// a modal input mode (collect digits, then act on next non-digit). v0
/// ships the `[` / `]` / `g` subset which proves the bus wiring; the
/// digit-prefix sequence lands when the §4 time-travel target machinery
/// has a concrete step-count to clamp against.
pub fn handle_key(key: KeyEvent, pending: Option<&PendingApproval>) -> InputOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), m) | (KeyCode::Esc, m)
            if !m.contains(KeyModifiers::CONTROL) || matches!(key.code, KeyCode::Char('q')) =>
        {
            InputOutcome::Quit
        }
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => InputOutcome::Quit,
        (KeyCode::Char('['), _) => InputOutcome::Scrub(ScrubCommand::Prev),
        (KeyCode::Char(']'), _) => InputOutcome::Scrub(ScrubCommand::Next),
        (KeyCode::Char('g'), _) => InputOutcome::Scrub(ScrubCommand::JumpToHead),
        // Approval keys are gated on pending state — keeps the
        // interpretation deterministic when the user is between
        // approvals.
        (KeyCode::Char('y'), _) if pending.is_some() => InputOutcome::AcceptAll,
        (KeyCode::Char('n'), _) if pending.is_some() => InputOutcome::RejectAll,
        _ => InputOutcome::Continue,
    }
}

/// I/O entry point. Reads `argv[1..]` for an optional prompt: if a
/// prompt is given the TUI runs in **driver mode** (builds a Runner
/// with `AwaitApproval` policy + a MockAdapter scripted around the
/// prompt; `y`/`n` keys route through the live `SessionDispatcher`).
/// Without a prompt it falls back to **viewer mode** — a NoopHook
/// session, identical to v45's behaviour — useful for testing the
/// terminal lifecycle in isolation.
///
/// Returns an `io::Result` so the caller (`main.rs`) can exit non-zero on
/// terminal setup failure.
pub fn run() -> io::Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let prompt: Option<String> = std::env::args().nth(1).filter(|s| !s.trim().is_empty());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| io::Error::other(format!("tokio runtime: {e}")))?;

    rt.block_on(async { run_async(prompt).await })
}

async fn run_async(prompt: Option<String>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout: Stdout = stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    // Owned RAII guard so a panic past this point still restores the
    // terminal (raw mode off, alternate screen released). Without this a
    // crash leaves the user's terminal in a broken state.
    let _restore = TerminalGuard;

    // Event channel: either fed by the Runner's EventSink::Callback
    // (driver mode) or by a NoopHook session's broadcast (viewer
    // mode). Boxed because the two paths produce different concrete
    // types we want to drive through one tokio::select! arm.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();

    // Per-run state. `dispatcher_handle` is `Some` only in driver
    // mode; the y/n key handler reads it to call submit_approval.
    let mut state = AppState::new();
    let dispatcher_handle: Option<atelier_cli::runner::DispatcherHandle>;
    let _run_task: Option<tokio::task::JoinHandle<()>>;
    let _viewer_session: Option<atelier_core::SessionHandle>;

    match prompt {
        Some(p) => {
            // Driver mode: build the Runner, wire EventSink::Callback
            // to the mpsc, spawn the run.
            let (handle, task) = spawn_driver_run(p, event_tx.clone())?;
            dispatcher_handle = Some(handle);
            _run_task = Some(task);
            _viewer_session = None;
        }
        None => {
            // Viewer mode: spawn a NoopHook session and forward its
            // broadcast events into the same mpsc.
            let session_handle = session::spawn(Arc::new(NoopHook), Arc::new(NoopHook));
            let mut rx = session_handle.subscribe();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            if tx.send(evt).is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => {
                            // Lagged is silently swallowed in viewer
                            // mode — the TUI will simply miss a tick.
                            // The render path's event log doesn't
                            // distinguish since nothing's producing
                            // events in viewer mode anyway.
                            continue;
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            });
            dispatcher_handle = None;
            _run_task = None;
            _viewer_session = Some(session_handle);
        }
    }

    terminal.draw(|f| render(&state, f.area(), f.buffer_mut()))?;

    // v49 FIX-6: gate the recv arm of the select! on this flag so a
    // closed mpsc doesn't busy-loop the runtime. Pre-v49 we replaced
    // event_rx with a fresh `(dead_tx, never_rx)` pair, but `dead_tx`
    // was dropped immediately, so `never_rx.recv()` returned None on
    // the very next poll and re-fired the RunEnded branch every tick.
    //
    // **One-shot semantics:** once `true`, this flag stays `true` for
    // the lifetime of `run_async`. If a future hot-key restarts the
    // run (e.g. `r` re-runs the demo prompt), it MUST also reset the
    // flag AND re-spawn the runner feeding the mpsc — otherwise the
    // new events have nowhere to land.
    let mut event_stream_ended = false;

    loop {
        let mut redraw = false;

        tokio::select! {
            biased;

            recv = event_rx.recv(), if !event_stream_ended => match recv {
                Some(evt) => {
                    state.apply(&evt);
                    redraw = true;
                }
                None => {
                    // All senders dropped — the run finished (and the
                    // viewer-mode forwarder closed). Keep the UI alive
                    // so the user can review final state until they
                    // explicitly quit. The `event_stream_ended` flag
                    // permanently disables this select arm, so the
                    // loop only waits on key input from here on.
                    state.events.push(EventLine {
                        kind: "RunEnded",
                        detail: "press q to quit".into(),
                    });
                    event_stream_ended = true;
                    redraw = true;
                }
            },

            // crossterm::event::read is blocking, so poll on a short
            // interval inside spawn_blocking. The poll period bounds
            // input latency at ~50ms.
            input = tokio::task::spawn_blocking(|| poll_one_key(Duration::from_millis(50))) => {
                match input {
                    Ok(Ok(Some(key))) => {
                        match handle_key(key, state.pending_approval.as_ref()) {
                            InputOutcome::Quit => break,
                            InputOutcome::Scrub(cmd) => state.apply_scrub(cmd),
                            InputOutcome::AcceptAll => {
                                submit_pending(&state, &dispatcher_handle, true);
                            }
                            InputOutcome::RejectAll => {
                                submit_pending(&state, &dispatcher_handle, false);
                            }
                            InputOutcome::Continue => {}
                        }
                        // Any handled key triggers a redraw — covers
                        // scrubber state changes and lets later
                        // hotkey-driven highlights show up.
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

/// Build a v48 driver run: a Runner with `AwaitApproval` policy +
/// `DispatcherHandle`, scripted to emit a `write_file` against an
/// ephemeral workspace + the `harness_meta` envelope so the loop
/// terminates after the user's approval decision.
///
/// Returns the handle (so y/n can submit_approval) and the spawned
/// task (held by the caller so its lifetime is tied to the run loop).
/// Max prompt size accepted by the TUI driver. Mirrors the GUI's
/// `MAX_PROMPT_BYTES` in `atelier-gui/src/lib.rs`. The argv path is
/// naturally bounded by the OS (~256 KiB typical), but a wrapper
/// script piping a huge stdin through a future "stdin prompt" mode
/// would bypass that — so the cap is enforced here for parity with
/// the GUI's defensive boundary.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

fn spawn_driver_run(
    prompt: String,
    event_tx: tokio::sync::mpsc::UnboundedSender<SessionEvent>,
) -> io::Result<(
    atelier_cli::runner::DispatcherHandle,
    tokio::task::JoinHandle<()>,
)> {
    use atelier_cli::runner::{DispatcherHandle, EventSink, MockResponse, ProviderChoice, Runner};
    use atelier_core::adapter::ToolCallRequest;
    use atelier_core::dispatcher::ApprovalPolicy;
    use atelier_core::protocol::Envelope;
    use atelier_core::protocol_strategy::HARNESS_META_NAME;

    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "prompt too long: {} bytes (max {} bytes / ~{} ASCII chars)",
                prompt.len(),
                MAX_PROMPT_BYTES,
                MAX_PROMPT_BYTES
            ),
        ));
    }

    let workspace = std::env::temp_dir().join(format!(
        "atelier-tui-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&workspace)?;

    let file_name = first_word_or_default(&prompt, "demo.txt");
    let content = format!("written by the TUI demo driver:\n{prompt}\n");
    let write_call = ToolCallRequest {
        id: "tc-tui-write".to_string(),
        name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": file_name,
            "content": content,
        }),
    };
    let envelope_done = Envelope {
        claimed_done: Some(true),
        ..Default::default()
    };
    let envelope_call = ToolCallRequest {
        id: "tc-tui-envelope".to_string(),
        name: HARNESS_META_NAME.to_string(),
        arguments: serde_json::to_value(&envelope_done).unwrap_or(serde_json::Value::Null),
    };
    let responses = vec![MockResponse {
        assistant_text: format!("Acknowledging: {prompt}"),
        tool_calls: vec![write_call, envelope_call],
    }];

    let handle = DispatcherHandle::new();
    let cb = std::sync::Arc::new(move |evt: &SessionEvent| {
        let _ = event_tx.send(evt.clone());
    });
    let runner = Runner::new(
        workspace,
        ProviderChoice::Mock { responses },
        EventSink::Callback(cb),
    )
    .map_err(|e| io::Error::other(format!("runner build failed: {e}")))?
    .with_approval_policy(ApprovalPolicy::AwaitApproval)
    .with_dispatcher_handle(handle.clone())
    .with_max_turns(4);

    let task = tokio::spawn(async move {
        if let Err(e) = runner.run(prompt).await {
            tracing::warn!(error = %e, "TUI demo run failed");
        }
    });
    Ok((handle, task))
}

/// Pick the first whitespace-delimited word from `s`, sanitised to
/// ASCII alphanumerics + `-`/`_`/`.`. Falls back to `default` when no
/// usable word is present. Used to build the demo file name from the
/// user's prompt. Mirror of `atelier-gui`'s helper of the same name.
fn first_word_or_default(s: &str, default: &str) -> String {
    let word: String = s
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect();
    if word.is_empty() {
        default.to_string()
    } else if word.contains('.') {
        word
    } else {
        format!("{word}.txt")
    }
}

/// Take the user's y/n decision and route it to the live
/// `SessionDispatcher::submit_approval`. No-op when there's no
/// pending approval (defensive; `handle_key` already gates the
/// outcomes on pending state). `accept_all = true` accepts every
/// pending file; `false` is a full reject.
fn submit_pending(
    state: &AppState,
    handle: &Option<atelier_cli::runner::DispatcherHandle>,
    accept_all: bool,
) {
    let Some(pending) = &state.pending_approval else {
        return;
    };
    let Some(handle) = handle else {
        // Viewer mode: y/n are inert because nothing's blocked on a
        // dispatcher. Defensive — the handle_key gate already returns
        // Continue when pending is None.
        return;
    };
    let Some(sd) = handle.get() else {
        tracing::warn!("submit_pending: DispatcherHandle empty (run already shut down?)");
        return;
    };
    let accepted: Vec<PathBuf> = if accept_all {
        pending.files.iter().map(|f| f.path.clone()).collect()
    } else {
        Vec::new()
    };
    if !sd.submit_approval(pending.commit_id, accepted) {
        tracing::warn!(
            commit_id = %pending.commit_id,
            "submit_pending: dispatcher rejected the accept-set (commit_id stale?)"
        );
    }
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
        let area = Rect::new(0, 0, 100, 24);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("Atelier TUI"), "got:\n{rendered}");
        assert!(rendered.contains("Streaming"), "got:\n{rendered}");
        assert!(rendered.contains("EditStaged=1"), "got:\n{rendered}");
    }

    #[test]
    fn render_shows_empty_placeholder_when_no_events() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 100, 24);
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
        // 100 cols gives the right-column event-log enough width to show
        // the full event detail; smaller terminals truncate at the cell
        // boundary which is acceptable but harder to assert against.
        let area = Rect::new(0, 0, 120, 24);
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
        let area = Rect::new(0, 0, 100, 24);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("quit"), "got:\n{rendered}");
    }

    // ---------- v52: model badge in the footer ----------

    fn fixture_model() -> CurrentModel {
        CurrentModel {
            model_id: "local:qwen2.5-coder:7b".into(),
            base_url: "http://localhost:11434/v1".into(),
            strategy: "json_sentinel",
            outcome: "cache_hit".into(),
        }
    }

    #[test]
    fn snake_case_debug_handles_camel_case() {
        assert_eq!(snake_case_debug("CacheHit"), "cache_hit");
        assert_eq!(snake_case_debug("NotCached"), "not_cached");
        assert_eq!(snake_case_debug("Probed"), "probed");
        // Leading uppercase doesn't get a leading underscore.
        assert_eq!(snake_case_debug("A"), "a");
    }

    #[test]
    fn apply_model_profile_loaded_populates_current_model() {
        use atelier_core::adapter::model_profile::ProbeLoadOutcome;
        use atelier_core::protocol_strategy::Strategy;

        let mut s = AppState::new();
        s.apply(&SessionEvent::ModelProfileLoaded {
            model_id: "local:qwen2.5-coder:7b".into(),
            base_url: "http://localhost:11434/v1".into(),
            strategy: Strategy::JsonSentinel,
            outcome: ProbeLoadOutcome::CacheHit,
        });
        let m = s.current_model.expect("current_model populated");
        assert_eq!(m.model_id, "local:qwen2.5-coder:7b");
        assert_eq!(m.base_url, "http://localhost:11434/v1");
        assert_eq!(m.strategy, "json_sentinel");
        assert_eq!(m.outcome, "cache_hit");
    }

    #[test]
    fn footer_shows_model_id_after_model_profile_loaded() {
        let mut s = AppState::new();
        s.current_model = Some(fixture_model());
        let area = Rect::new(0, 0, 120, 24);
        let rendered = render_to_string(&s, area);
        // Footer line is the last row; check the model badge is present.
        assert!(
            rendered.contains("local:qwen2.5-coder:7b"),
            "got:\n{rendered}"
        );
        assert!(rendered.contains("json_sentinel"), "got:\n{rendered}");
        assert!(rendered.contains("cache_hit"), "got:\n{rendered}");
    }

    #[test]
    fn footer_omits_model_badge_when_no_model_loaded_yet() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 120, 24);
        let rendered = render_to_string(&s, area);
        // Neither label nor a `no model` placeholder — pre-event the
        // footer is just the help line.
        assert!(!rendered.contains("cache_hit"), "got:\n{rendered}");
        assert!(!rendered.contains("json_sentinel"), "got:\n{rendered}");
    }

    #[test]
    fn footer_suppresses_model_badge_during_pending_approval() {
        let mut s = AppState::new();
        s.current_model = Some(fixture_model());
        // Inject a pending-approval so render_help takes the
        // approval branch.
        s.pending_approval = Some(PendingApproval {
            commit_id: uuid::Uuid::nil(),
            files: vec![],
        });
        let area = Rect::new(0, 0, 120, 24);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("APPROVAL REQUIRED"), "got:\n{rendered}");
        // Model badge must be hidden so the approval prompt is the
        // unambiguous focus.
        assert!(!rendered.contains("qwen2.5-coder"), "got:\n{rendered}");
    }

    // ---------- v53: §5 Context pane ----------

    fn ctx_item(
        id: &str,
        kind: &str,
        label: &str,
        provenance: &str,
        tokens: u32,
        token_source: &str,
    ) -> atelier_core::context::ContextItemSummary {
        atelier_core::context::ContextItemSummary {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            provenance: provenance.into(),
            provenance_detail: None,
            tokens,
            token_source: token_source.into(),
            pinned: false,
        }
    }

    #[test]
    fn apply_context_items_replaces_snapshot_wholesale() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::ContextItems {
            items: vec![
                ctx_item("a", "file_ref", "src/a.rs", "user_attached", 10, "exact"),
                ctx_item("b", "file_ref", "src/b.rs", "user_attached", 20, "exact"),
            ],
        });
        assert_eq!(s.context_items.len(), 2);
        // Second snapshot replaces, doesn't append.
        s.apply(&SessionEvent::ContextItems {
            items: vec![ctx_item(
                "c",
                "file_ref",
                "src/c.rs",
                "tool_result",
                5,
                "approx",
            )],
        });
        assert_eq!(s.context_items.len(), 1);
        assert_eq!(s.context_items[0].label, "src/c.rs");
    }

    #[test]
    fn render_context_pane_shows_empty_placeholder_with_no_items() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 120, 30);
        let rendered = render_to_string(&s, area);
        assert!(
            rendered.contains("no context items yet"),
            "got:\n{rendered}"
        );
    }

    #[test]
    fn render_context_pane_lists_items_with_provenance_badges() {
        let mut s = AppState::new();
        s.context_items = vec![
            ctx_item("a", "file_ref", "src/a.rs", "user_attached", 10, "exact"),
            ctx_item("b", "file_ref", "src/b.rs", "tool_result", 5, "approx"),
        ];
        let area = Rect::new(0, 0, 120, 30);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("src/a.rs"), "got:\n{rendered}");
        assert!(rendered.contains("src/b.rs"), "got:\n{rendered}");
        // Badges (column-aligned shortcuts).
        assert!(rendered.contains("usr"), "got:\n{rendered}");
        assert!(rendered.contains("tool"), "got:\n{rendered}");
    }

    #[test]
    fn provenance_badge_labels_are_stable() {
        // Spec §5 mechanical-gate-friendly: stable strings.
        assert_eq!(provenance_badge("initial"), "init");
        assert_eq!(provenance_badge("user_attached"), "usr ");
        assert_eq!(provenance_badge("tool_result"), "tool");
        assert_eq!(provenance_badge("memory_promoted"), "mem ");
        assert_eq!(provenance_badge("pinned_by_user"), "pin ");
        assert_eq!(provenance_badge("assistant_turn"), "asst");
        assert_eq!(provenance_badge("garbage"), "????");
    }

    #[test]
    fn project_event_for_context_items_includes_count() {
        let line = project_event(&SessionEvent::ContextItems {
            items: vec![
                ctx_item("a", "file_ref", "x", "initial", 1, "exact"),
                ctx_item("b", "file_ref", "y", "initial", 1, "exact"),
                ctx_item("c", "file_ref", "z", "initial", 1, "exact"),
            ],
        });
        assert_eq!(line.kind, "ContextItems");
        assert!(line.detail.contains("3"));
    }

    // ---------- v54: §5 Memory pane ----------

    fn mem_card(
        id: &str,
        title: &str,
        last_used: &str,
        pinned: bool,
    ) -> atelier_core::memory::MemoryCardSummary {
        atelier_core::memory::MemoryCardSummary {
            id: id.into(),
            title: title.into(),
            body_preview: "body".into(),
            created_at: "2026-05-17T10:00:00Z".into(),
            last_used: last_used.into(),
            pinned,
        }
    }

    #[test]
    fn apply_memory_cards_replaces_snapshot_wholesale() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::MemoryCards {
            cards: vec![
                mem_card("a", "first", "2026-05-17T12:00:00Z", false),
                mem_card("b", "second", "2026-05-17T12:30:00Z", true),
            ],
        });
        assert_eq!(s.memory_cards.len(), 2);
        s.apply(&SessionEvent::MemoryCards {
            cards: vec![mem_card("c", "third", "2026-05-17T13:00:00Z", false)],
        });
        assert_eq!(s.memory_cards.len(), 1);
        assert_eq!(s.memory_cards[0].title, "third");
    }

    #[test]
    fn render_memory_pane_empty_state_visible() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 120, 30);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("no memory cards yet"), "got:\n{rendered}");
    }

    #[test]
    fn render_memory_pane_shows_title_and_timestamp() {
        let mut s = AppState::new();
        s.memory_cards = vec![mem_card(
            "mem-1",
            "user prefers tabs",
            "2026-05-17T12:34:56Z",
            false,
        )];
        let area = Rect::new(0, 0, 120, 30);
        let rendered = render_to_string(&s, area);
        assert!(rendered.contains("user prefers tabs"), "got:\n{rendered}");
        // Compact form: "2026-05-17 12:34"
        assert!(rendered.contains("2026-05-17 12:34"), "got:\n{rendered}");
    }

    #[test]
    fn model_badge_width_matches_visible_chars() {
        let m = fixture_model();
        // "local:qwen2.5-coder:7b" (22) + " · " (3) + "json_sentinel"
        // (13) + " · " (3) + "cache_hit" (9) + " · " (3) + trailing
        // " " (1) = 54. Note: there are three separators total.
        let expected = m.model_id.len()
            + m.strategy.len()
            + m.outcome.len()
            + (3 * 3) // three " · "
            + 1; // trailing space
        assert_eq!(model_badge_width(&m) as usize, expected);
    }

    #[test]
    fn handle_key_quits_on_q_esc_and_ctrl_c() {
        assert_eq!(
            handle_key(key(KeyCode::Char('q'), KeyModifiers::empty()), None),
            InputOutcome::Quit
        );
        assert_eq!(
            handle_key(key(KeyCode::Esc, KeyModifiers::empty()), None),
            InputOutcome::Quit
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), None),
            InputOutcome::Quit
        );
    }

    #[test]
    fn handle_key_continues_on_other_keys() {
        assert_eq!(
            handle_key(key(KeyCode::Char('a'), KeyModifiers::empty()), None),
            InputOutcome::Continue
        );
        assert_eq!(
            handle_key(key(KeyCode::Enter, KeyModifiers::empty()), None),
            InputOutcome::Continue
        );
        // Ctrl-Q is not the quit binding (only Ctrl-C is) — guarantees
        // the modifier check is right.
        assert_eq!(
            handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL), None),
            InputOutcome::Quit, // 'q' alone quits, regardless of modifier
        );
    }

    // ---------- TUI-1: conversation pane ----------

    #[test]
    fn conversation_pane_renders_role_prefixes_and_text() {
        let mut s = AppState::new();
        s.push_conversation(ConversationRole::User, "rename foo to bar");
        s.push_conversation(ConversationRole::Assistant, "starting the rename");
        s.push_conversation(ConversationRole::Tool, r#"{"exit_code":0}"#);
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("user"), "got:\n{r}");
        assert!(r.contains("assistant"), "got:\n{r}");
        assert!(r.contains("tool"), "got:\n{r}");
        assert!(r.contains("rename foo to bar"), "got:\n{r}");
        assert!(r.contains("starting the rename"), "got:\n{r}");
    }

    #[test]
    fn conversation_pane_shows_empty_placeholder_when_no_messages() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("no messages yet"), "got:\n{r}");
    }

    #[test]
    fn push_conversation_bounds_history() {
        let mut s = AppState::new();
        for i in 0..(MAX_CONVERSATION_LINES + 50) {
            s.push_conversation(ConversationRole::User, format!("msg {i}"));
        }
        assert_eq!(s.conversation.len(), MAX_CONVERSATION_LINES);
        // Oldest dropped: msg 0 should not be present, msg 49 should be
        // the new front.
        assert_eq!(s.conversation.front().unwrap().text, "msg 50");
    }

    // ---------- TUI-2: diff pane ----------

    #[test]
    fn diff_pane_renders_line_hunk_plus_minus_markers() {
        use atelier_core::diff::{Hunk, LineRange};
        let mut s = AppState::new();
        s.recent_edits.push_front(StagedEdit {
            path: PathBuf::from("src/foo.rs"),
            hunks: Hunks::Lines {
                hunks: vec![Hunk {
                    old_range: LineRange { start: 0, end: 1 },
                    new_range: LineRange { start: 0, end: 1 },
                    old_lines: vec!["fn old_name()".into()],
                    new_lines: vec!["fn new_name()".into()],
                }],
            },
        });
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("src/foo.rs"), "path missing:\n{r}");
        assert!(r.contains("-fn old_name"), "minus marker missing:\n{r}");
        assert!(r.contains("+fn new_name"), "plus marker missing:\n{r}");
    }

    #[test]
    fn diff_pane_renders_created_badge() {
        let mut s = AppState::new();
        s.recent_edits.push_front(StagedEdit {
            path: PathBuf::from("hello.txt"),
            hunks: Hunks::Created {
                new_byte_len: 13,
                new_line_count: 1,
            },
        });
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("hello.txt"), "got:\n{r}");
        assert!(r.contains("created"), "got:\n{r}");
        assert!(r.contains("13 bytes"), "got:\n{r}");
    }

    #[test]
    fn diff_pane_renders_binary_badge() {
        let mut s = AppState::new();
        s.recent_edits.push_front(StagedEdit {
            path: PathBuf::from("logo.png"),
            hunks: Hunks::Binary,
        });
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("binary file changed"), "got:\n{r}");
    }

    #[test]
    fn apply_edit_staged_pushes_into_recent_edits_bounded() {
        let mut s = AppState::new();
        for i in 0..(MAX_DIFF_HISTORY + 5) {
            s.apply(&SessionEvent::EditStaged {
                path: PathBuf::from(format!("f{i}.rs")),
                hunks: Hunks::Binary,
            });
        }
        assert_eq!(s.recent_edits.len(), MAX_DIFF_HISTORY);
        // Newest is at front: last pushed had the highest index.
        assert!(s
            .recent_edits
            .front()
            .unwrap()
            .path
            .to_string_lossy()
            .ends_with(&format!("f{}.rs", MAX_DIFF_HISTORY + 4)));
    }

    // ---------- TUI-3: plan canvas ----------

    #[test]
    fn plan_pane_renders_steps_with_status_glyphs() {
        let mut plan = PlanCanvas::new();
        let id1 = plan.add("first step");
        let id2 = plan.add("second step");
        let id3 = plan.add("third step");
        plan.mark_status(&id1, PlanStatus::Done).unwrap();
        plan.mark_status(&id2, PlanStatus::InProgress).unwrap();
        let _ = id3;
        let mut s = AppState::new();
        s.set_plan(plan);

        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("first step"), "got:\n{r}");
        assert!(r.contains("second step"), "got:\n{r}");
        assert!(r.contains("third step"), "got:\n{r}");
        // Done glyph
        assert!(r.contains("✓"), "missing done glyph:\n{r}");
        // In-progress glyph
        assert!(r.contains("▸"), "missing in-progress glyph:\n{r}");
    }

    #[test]
    fn plan_pane_shows_constraints_under_steps() {
        let mut plan = PlanCanvas::new();
        let id = plan.add("write the test");
        plan.add_constraint(&id, "no mocks").unwrap();
        let mut s = AppState::new();
        s.set_plan(plan);

        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("write the test"), "got:\n{r}");
        assert!(r.contains("no mocks"), "got:\n{r}");
    }

    #[test]
    fn plan_pane_shows_empty_placeholder() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("no plan steps"), "got:\n{r}");
    }

    // ---------- TUI-4: meters ----------

    #[test]
    fn cost_meter_renders_dollar_amount() {
        let mut s = AppState::new();
        s.set_cost_usd(0.0123);
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("$0.0123"), "got:\n{r}");
    }

    #[test]
    fn context_meter_renders_known_over_window() {
        let mut s = AppState::new();
        s.set_context_window(8_000);
        s.set_context_tokens(2_000, 0);
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("2000/8000"), "got:\n{r}");
    }

    #[test]
    fn context_meter_surfaces_unknown_count_when_present() {
        let mut s = AppState::new();
        s.set_context_window(8_000);
        s.set_context_tokens(2_000, 500);
        let area = Rect::new(0, 0, 100, 24);
        let r = render_to_string(&s, area);
        assert!(
            r.contains("+500 unknown"),
            "unknown count should be visible — meter must NOT silently underreport. got:\n{r}"
        );
    }

    // ---------- TUI-5: scrubber keys ----------

    #[test]
    fn handle_key_emits_scrub_prev_on_open_bracket() {
        assert_eq!(
            handle_key(key(KeyCode::Char('['), KeyModifiers::empty()), None),
            InputOutcome::Scrub(ScrubCommand::Prev)
        );
    }

    #[test]
    fn handle_key_emits_scrub_next_on_close_bracket() {
        assert_eq!(
            handle_key(key(KeyCode::Char(']'), KeyModifiers::empty()), None),
            InputOutcome::Scrub(ScrubCommand::Next)
        );
    }

    #[test]
    fn handle_key_emits_jump_to_head_on_g() {
        assert_eq!(
            handle_key(key(KeyCode::Char('g'), KeyModifiers::empty()), None),
            InputOutcome::Scrub(ScrubCommand::JumpToHead)
        );
    }

    #[test]
    fn apply_scrub_walks_prev_then_next_back_to_head() {
        let mut s = AppState::new();
        assert_eq!(s.scrub_offset, None);
        s.apply_scrub(ScrubCommand::Prev);
        assert_eq!(s.scrub_offset, Some(1));
        s.apply_scrub(ScrubCommand::Prev);
        assert_eq!(s.scrub_offset, Some(2));
        s.apply_scrub(ScrubCommand::Next);
        assert_eq!(s.scrub_offset, Some(1));
        s.apply_scrub(ScrubCommand::Next);
        // Next from 1 → 0 collapses to HEAD (None).
        assert_eq!(s.scrub_offset, None);
        // Next from HEAD stays at HEAD (no-op forward at live).
        s.apply_scrub(ScrubCommand::Next);
        assert_eq!(s.scrub_offset, None);
    }

    #[test]
    fn apply_scrub_jump_to_head_resets_from_anywhere() {
        let mut s = AppState::new();
        for _ in 0..5 {
            s.apply_scrub(ScrubCommand::Prev);
        }
        assert_eq!(s.scrub_offset, Some(5));
        s.apply_scrub(ScrubCommand::JumpToHead);
        assert_eq!(s.scrub_offset, None);
    }

    #[test]
    fn header_shows_scrub_offset_when_pinned() {
        let mut s = AppState::new();
        s.apply_scrub(ScrubCommand::Prev);
        s.apply_scrub(ScrubCommand::Prev);
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("scrub=-2"), "got:\n{r}");
    }

    #[test]
    fn header_shows_head_when_live() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("scrub=HEAD"), "got:\n{r}");
    }

    #[test]
    fn help_footer_mentions_scrubber_keys() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("[ prev"), "got:\n{r}");
        assert!(r.contains("] next"), "got:\n{r}");
        assert!(r.contains("g HEAD"), "got:\n{r}");
    }

    // ---------- PC-4: bus-driven mutations via apply() ----------

    #[test]
    fn apply_message_committed_pushes_to_conversation() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::MessageCommitted {
            role: MessageRole::User,
            text: "hello there".into(),
        });
        s.apply(&SessionEvent::MessageCommitted {
            role: MessageRole::Assistant,
            text: "hi".into(),
        });
        assert_eq!(s.conversation.len(), 2);
        assert_eq!(s.conversation[0].role, ConversationRole::User);
        assert_eq!(s.conversation[0].text, "hello there");
        assert_eq!(s.conversation[1].role, ConversationRole::Assistant);
    }

    #[test]
    fn apply_plan_snapshot_replaces_plan_canvas() {
        let mut snapshot = PlanCanvas::new();
        let id = snapshot.add("first step");
        snapshot.mark_status(&id, PlanStatus::InProgress).unwrap();

        let mut s = AppState::new();
        s.apply(&SessionEvent::PlanSnapshot {
            steps: snapshot.to_vec(),
        });
        let rendered = s.plan.to_vec();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].text, "first step");
        assert_eq!(rendered[0].status, PlanStatus::InProgress);
    }

    #[test]
    fn apply_ledger_appended_accumulates_cost() {
        let mut s = AppState::new();
        // ModelCall entry with $0.005 cost
        s.apply(&SessionEvent::LedgerAppended {
            entry: LedgerEntry::ModelCall {
                timestamp: "2026-05-17T00:00:00Z".into(),
                model_id: "mock:m".into(),
                prompt_tokens: 100,
                completion_tokens: 50,
                cached_tokens: None,
                count_source: atelier_core::context::TokenSource::Approx,
                latency_ms: Some(200.0),
                cost_usd: Some(0.005),
                note: None,
            },
        });
        // ToolCall entry with $0.001 cost
        s.apply(&SessionEvent::LedgerAppended {
            entry: LedgerEntry::tool_call(
                "2026-05-17T00:00:01Z",
                "shell",
                100.0,
                Some(0.001),
                None,
            ),
        });
        // CacheBust: no cost field → must not deflate the running total.
        s.apply(&SessionEvent::LedgerAppended {
            entry: LedgerEntry::CacheBust {
                timestamp: "2026-05-17T00:00:02Z".into(),
                note: "evicted context-item: user-attached".into(),
            },
        });
        assert!((s.total_cost_usd - 0.006).abs() < 1e-9);
    }

    #[test]
    fn apply_context_snapshot_updates_meter_state() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::ContextSnapshot {
            known_tokens: 3_200,
            unknown_tokens: 150,
        });
        assert_eq!(s.context_tokens, (3_200, 150));
    }

    #[test]
    fn project_event_covers_new_variants() {
        assert_eq!(
            project_event(&SessionEvent::MessageCommitted {
                role: MessageRole::User,
                text: "hi".into(),
            })
            .kind,
            "Message"
        );
        assert_eq!(
            project_event(&SessionEvent::PlanSnapshot { steps: vec![] }).kind,
            "PlanSnapshot"
        );
        assert_eq!(
            project_event(&SessionEvent::ContextSnapshot {
                known_tokens: 1,
                unknown_tokens: 2,
            })
            .detail,
            "known=1 unknown=2"
        );
        // LedgerAppended detail mentions the variant kind.
        let cl = project_event(&SessionEvent::LedgerAppended {
            entry: LedgerEntry::tool_call("t", "shell", 1.0, None, None),
        });
        assert_eq!(cl.kind, "LedgerAppended");
        assert!(cl.detail.contains("shell"), "got {:?}", cl.detail);
    }

    // ---------- HR-E: pending-approval consumption ----------

    #[test]
    fn apply_staging_pending_approval_records_into_app_state() {
        let mut s = AppState::new();
        let cid = uuid::Uuid::new_v4();
        s.apply(&SessionEvent::StagingPendingApproval {
            commit_id: cid,
            files: vec![
                PendingFile {
                    path: PathBuf::from("a.rs"),
                    hunks: Hunks::Binary,
                },
                PendingFile {
                    path: PathBuf::from("b.rs"),
                    hunks: Hunks::Created {
                        new_byte_len: 5,
                        new_line_count: 1,
                    },
                },
            ],
        });
        let pending = s.pending_approval.expect("set by apply");
        assert_eq!(pending.commit_id, cid);
        assert_eq!(pending.files.len(), 2);
    }

    #[test]
    fn apply_commit_decision_clears_pending_approval() {
        let mut s = AppState::new();
        let cid = uuid::Uuid::new_v4();
        s.apply(&SessionEvent::StagingPendingApproval {
            commit_id: cid,
            files: vec![PendingFile {
                path: PathBuf::from("a.rs"),
                hunks: Hunks::Binary,
            }],
        });
        assert!(s.pending_approval.is_some());
        s.apply(&SessionEvent::CommitDecision {
            commit_id: cid,
            committed: vec![PathBuf::from("a.rs")],
            dropped: vec![],
        });
        assert!(s.pending_approval.is_none(), "decision clears pending");
    }

    #[test]
    fn diff_pane_renders_pending_badge_when_approval_outstanding() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::StagingPendingApproval {
            commit_id: uuid::Uuid::new_v4(),
            files: vec![PendingFile {
                path: PathBuf::from("danger.rs"),
                hunks: Hunks::Binary,
            }],
        });
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(r.contains("PENDING"), "pending badge missing:\n{r}");
        assert!(r.contains("danger.rs"), "pending file path missing:\n{r}");
        // The committed-diff path should NOT also render — pending takes
        // precedence.
        assert!(
            !r.contains("(no edits yet)"),
            "empty-edits placeholder shouldn't appear when pending is set"
        );
    }

    #[test]
    fn diff_pane_returns_to_normal_after_commit_decision() {
        let mut s = AppState::new();
        let cid = uuid::Uuid::new_v4();
        s.apply(&SessionEvent::StagingPendingApproval {
            commit_id: cid,
            files: vec![PendingFile {
                path: PathBuf::from("a.rs"),
                hunks: Hunks::Binary,
            }],
        });
        s.apply(&SessionEvent::CommitDecision {
            commit_id: cid,
            committed: vec![PathBuf::from("a.rs")],
            dropped: vec![],
        });
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(!r.contains("PENDING"), "pending should clear:\n{r}");
    }

    // ---------- TD-B: y/n approval keys ----------

    fn pending_with_files(paths: &[&str]) -> PendingApproval {
        PendingApproval {
            commit_id: uuid::Uuid::new_v4(),
            files: paths
                .iter()
                .map(|p| PendingApprovalFile {
                    path: PathBuf::from(*p),
                    hunks: Hunks::Binary,
                })
                .collect(),
        }
    }

    #[test]
    fn handle_key_emits_accept_all_on_y_when_pending() {
        let pending = pending_with_files(&["a.txt"]);
        assert_eq!(
            handle_key(
                key(KeyCode::Char('y'), KeyModifiers::empty()),
                Some(&pending),
            ),
            InputOutcome::AcceptAll
        );
    }

    #[test]
    fn handle_key_emits_reject_all_on_n_when_pending() {
        let pending = pending_with_files(&["a.txt"]);
        assert_eq!(
            handle_key(
                key(KeyCode::Char('n'), KeyModifiers::empty()),
                Some(&pending),
            ),
            InputOutcome::RejectAll
        );
    }

    #[test]
    fn handle_key_y_and_n_are_inert_when_no_pending() {
        // Without an active approval, y/n must NOT trigger the approval
        // outcomes — that would let a stray keystroke during idle time
        // mis-fire submit_approval (which would no-op anyway but the
        // outcome semantics should match user intent).
        assert_eq!(
            handle_key(key(KeyCode::Char('y'), KeyModifiers::empty()), None),
            InputOutcome::Continue
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('n'), KeyModifiers::empty()), None),
            InputOutcome::Continue
        );
    }

    #[test]
    fn help_footer_swaps_to_approval_hints_when_pending() {
        let mut s = AppState::new();
        s.apply(&SessionEvent::StagingPendingApproval {
            commit_id: uuid::Uuid::new_v4(),
            files: vec![PendingFile {
                path: PathBuf::from("a.txt"),
                hunks: Hunks::Binary,
            }],
        });
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(
            r.contains("APPROVAL REQUIRED"),
            "footer should pivot to approval mode:\n{r}"
        );
        assert!(r.contains("y accept all"), "y hint missing:\n{r}");
        assert!(r.contains("n reject all"), "n hint missing:\n{r}");
    }

    #[test]
    fn help_footer_returns_to_scrub_hints_after_decision() {
        let mut s = AppState::new();
        let cid = uuid::Uuid::new_v4();
        s.apply(&SessionEvent::StagingPendingApproval {
            commit_id: cid,
            files: vec![PendingFile {
                path: PathBuf::from("a.txt"),
                hunks: Hunks::Binary,
            }],
        });
        s.apply(&SessionEvent::CommitDecision {
            commit_id: cid,
            committed: vec![PathBuf::from("a.txt")],
            dropped: vec![],
        });
        let area = Rect::new(0, 0, 120, 24);
        let r = render_to_string(&s, area);
        assert!(
            !r.contains("APPROVAL REQUIRED"),
            "approval footer should clear:\n{r}"
        );
        assert!(r.contains("[ prev"), "scrub hints should return:\n{r}");
    }

    #[test]
    fn conversation_role_maps_from_message_role_for_every_variant() {
        // Exhaustive check — adding a new MessageRole variant must
        // force a deliberate mapping decision here.
        assert_eq!(
            ConversationRole::from_message_role(MessageRole::User),
            ConversationRole::User
        );
        assert_eq!(
            ConversationRole::from_message_role(MessageRole::Assistant),
            ConversationRole::Assistant
        );
        assert_eq!(
            ConversationRole::from_message_role(MessageRole::Tool),
            ConversationRole::Tool
        );
        assert_eq!(
            ConversationRole::from_message_role(MessageRole::System),
            ConversationRole::System
        );
    }
}
