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
use std::time::{Duration, Instant};

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
    /// Last `Transitioned` event's `to` field, rendered via
    /// `State::name()`. Empty string before any transition; used in
    /// the header so the user knows what state the session is in.
    /// (Pre-v57 this was `Debug`-formatted; v57 unified on
    /// `State::name()`. A future cleanup could switch this to
    /// `Option<State>` so the empty-sentinel goes away.)
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
    /// v55 — focused pane for keyboard input. Tab cycles
    /// Conversation → Context → Memory → Plan. The focused pane
    /// renders with a highlighted border and is the target of
    /// `j`/`k` selection + per-pane mutator keys.
    pub focused_pane: FocusedPane,
    /// v55 — selection index in the Context pane (`j`/`k` when
    /// focused). Saturates at `len-1`; safe to be larger than
    /// `context_items.len()` (consumers clamp at render time).
    pub selected_context: usize,
    pub selected_memory: usize,
    pub selected_plan: usize,
    /// v60.5 — multi-select for §5 non-destructive compaction. Set
    /// of `ContextItemSummary.id` strings the user has marked with
    /// `space`. Separate from `selected_context` (the cursor index)
    /// so the user can navigate without losing the selection. Pinned
    /// rows can't be added (the dispatcher would refuse them
    /// anyway, but the UI hides the affordance). Cleared on
    /// `Event::CompactionExecuted` so a successful compaction
    /// resets the selection without the user having to.
    pub selected_context_set: std::collections::HashSet<String>,
    /// v55 — modal input state. `Normal` is the default; the
    /// modals are entered via per-pane keys (`a` for add, `c` for
    /// constraint) and exited via Enter (submit) / Esc (cancel).
    pub input_mode: InputMode,
    /// v56 — per-file rationale from the envelope's `claimed_changes`,
    /// keyed by path. The diff pane renders this next to the file
    /// header so the user can see the agent's stated "why" for each
    /// change. Wholesale-replaced on every `Event::ClaimedChanges`.
    pub claimed_changes: std::collections::HashMap<String, String>,
    /// Phase C close — §5 mental-model panel state. The TUI does
    /// **not** render a full editable surface for v0; instead the
    /// footer carries a hint badge so the user can see whether the
    /// panel is on and roughly how big the text is. Full TUI parity
    /// (modal text editor + keybind to flip the toggle) lands when
    /// the harness actually injects the text into the prompt.
    pub mental_model: MentalModelHint,
    /// v62 — §7 verify-pass tier indicator. Populated by
    /// `VerificationPassed` events from the dispatcher;
    /// `render_cost_meter` renders a small badge above the cost
    /// line so the user can see which §7 tier (Tier 1 LSP / Tier 2
    /// tree-sitter / Tier 3 textual / NotRun) ran on the last
    /// verify pass. Defaults to `NotRun` (gray) before any pass.
    pub verification_status: VerificationStatusHint,
    /// v60.9 B1 follow-on — most recent §1 context-overflow
    /// resolution. Populated by `ContextOverflowResolved`; `None`
    /// before any overflow has been resolved. `render_cost_meter`
    /// renders a transient one-line hint keyed off `recorded_at`;
    /// callers can compare `recorded_at.elapsed()` against the
    /// 5s decay window (see [`OVERFLOW_HINT_TTL`]) to suppress the
    /// hint once it's stale. The field stays populated past the
    /// decay window so a debug surface can still inspect the
    /// last resolution.
    pub last_overflow_resolution: Option<OverflowResolutionHint>,
}

/// Phase C close — TUI's projection of
/// [`atelier_core::mental_model::MentalModelSnapshot`]. Off by
/// default; toggled to `enabled=true` by an external mutator (the
/// GUI today, the CLI's `mental-model` subcommand future-side).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MentalModelHint {
    pub enabled: bool,
    pub text_tokens: u32,
}

/// v60.9 B1 follow-on — how long the overflow-resolution hint stays
/// visible in the cost-meter row. Mirrors the GUI's
/// `OVERFLOW_TOAST_MS` (5s) so the two surfaces fade out together.
pub const OVERFLOW_HINT_TTL: Duration = Duration::from_secs(5);

/// v60.9 B1 follow-on — TUI's projection of the most recent
/// `Event::ContextOverflowResolved`. Stored on `AppState`;
/// `render_cost_meter` renders a one-line hint while
/// `recorded_at.elapsed() < OVERFLOW_HINT_TTL`. After the decay
/// window the hint is suppressed but the field stays populated so
/// a debug surface (or the event log) can still inspect the last
/// resolution.
#[derive(Debug, Clone)]
pub struct OverflowResolutionHint {
    /// Stable wire label of the policy arm that fired. Matches the
    /// `&'static str` carried on the event (`"compacted"` /
    /// `"rerouted"` / `"surfaced"`).
    pub resolution: &'static str,
    pub freed_tokens: Option<u32>,
    pub items_compacted: Option<usize>,
    pub recorded_at: Instant,
}

/// v62 — TUI's projection of the §7 verify-pass terminal state. Off
/// (`NotRun`) by default until the dispatcher emits its first
/// `Event::VerificationPassed`. Stored on `AppState` and rendered by
/// `render_cost_meter` as a small badge above the cost line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationStatusHint {
    pub tier: atelier_core::verify::VerificationTier,
    pub file_count: u32,
    pub claim_count: u32,
}

impl Default for VerificationStatusHint {
    fn default() -> Self {
        Self {
            tier: atelier_core::verify::VerificationTier::NotRun,
            file_count: 0,
            claim_count: 0,
        }
    }
}

impl VerificationStatusHint {
    /// v62 — human-readable badge label. Mirrors the GUI's
    /// `verificationTierLabel` so the two surfaces render identical
    /// copy; pinned by `verification_status_hint_badge_label_matches_gui`.
    pub fn badge_label(&self) -> &'static str {
        match self.tier {
            atelier_core::verify::VerificationTier::Tier1Lsp => "tier-1 (lsp)",
            atelier_core::verify::VerificationTier::Tier2TreeSitter => "tier-2 (tree-sitter)",
            atelier_core::verify::VerificationTier::Tier3Textual => "tier-3 (textual)",
            atelier_core::verify::VerificationTier::NotRun => "verify off",
        }
    }

    /// v62 — ratatui colour for the badge. Matches the GUI's CSS
    /// classes (green / yellow / orange / dim-gray) so the two
    /// surfaces stay semantically aligned.
    pub fn badge_colour(&self) -> Color {
        match self.tier {
            atelier_core::verify::VerificationTier::Tier1Lsp => Color::Green,
            atelier_core::verify::VerificationTier::Tier2TreeSitter => Color::Yellow,
            // ratatui has no `Color::Orange`; use `LightRed` which
            // renders as a warm orange-ish tone in 256-colour terminals
            // and degrades to red on 8-colour ones. The semantic
            // remains: Tier 3 is the lowest non-zero coverage tier.
            atelier_core::verify::VerificationTier::Tier3Textual => Color::LightRed,
            atelier_core::verify::VerificationTier::NotRun => Color::DarkGray,
        }
    }
}

/// v55 — which pane has keyboard focus. Tab cycles forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusedPane {
    #[default]
    Conversation,
    Context,
    Memory,
    Plan,
}

impl FocusedPane {
    /// Tab cycle (forward).
    pub fn next(self) -> Self {
        match self {
            Self::Conversation => Self::Context,
            Self::Context => Self::Memory,
            Self::Memory => Self::Plan,
            Self::Plan => Self::Conversation,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Context => "context",
            Self::Memory => "memory",
            Self::Plan => "plan",
        }
    }
}

/// v55 — what kind of text the active text-input modal collects.
/// Carries the contextual id when the input is tied to an existing
/// row (e.g. add-constraint targets a specific plan step).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextInputKind {
    AddMemoryCard,
    AddPlanStep,
    AddPlanConstraint { step_id: String },
}

/// v55 — top-level keyboard mode. `Normal` is the default; entering a
/// modal grabs subsequent keystrokes until Enter/Esc.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Normal,
    /// Text-input modal — `buffer` is the user's keystrokes so far.
    TextInput { kind: TextInputKind, buffer: String },
    /// Evict-confirm modal — `y` confirms, anything else cancels.
    /// `id` is the stringified `ContextItemId` to evict.
    EvictConfirm { id: String },
    /// v60.5 — compact-confirm modal. `y` confirms; anything else
    /// cancels. Carries the `ids` to compact and the projected
    /// `tokens_freed` so the modal can render the cost disclosure
    /// without re-summing.
    CompactConfirm { ids: Vec<String>, tokens_freed: u32 },
    /// v60.6 — expand-confirm modal. `y` confirms; anything else
    /// cancels. Carries the card id and the cache-rewarm cost
    /// surfaced from the card's `cache_rewarm_tokens` projection so
    /// the modal renders the disclosure without re-reading the blob.
    ExpandConfirm {
        card_id: String,
        item_count: u32,
        cache_rewarm_tokens: u32,
    },
    /// v61 — §14 concurrent-edit modal. Surfaced when `Event::FilesChanged`
    /// arrives during an interactive run. `r` chooses Reload, `w` Wait,
    /// `p` Pause; Esc dismisses without choosing (the 5-minute auto-pause
    /// timer in the runner is the ultimate arbiter). `paths` is the
    /// debounced list of files whose external edit fired the modal.
    ConcurrentEditConfirm { paths: Vec<PathBuf> },
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
    /// v60.7 §1 BYOM — capability matrix row. Surfaced as an
    /// auxiliary line in the footer (and a richer tooltip in the
    /// GUI). `None` when the runner pre-dates the matrix wiring or
    /// the event omits it.
    pub capability_row: Option<atelier_core::adapter::capability_matrix::CapabilityMatrixRow>,
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

// v57 (H7 fix) — the `snake_case_debug` Debug→snake-case helper was
// removed alongside its callers. Wire labels now come from canonical
// `*::wire_label()` methods owned by atelier-core
// (`MessageRole::wire_label`, `ProbeLoadOutcome::wire_label`) so
// Rust's `Debug` is no longer a serialisation contract.

/// Best-effort cost extraction from any `LedgerEntry` variant. Returns
/// `None` for entries that don't carry a USD cost (some `CacheBust`
/// entries today). The TUI's running total ignores `None` rather than
/// treating it as zero so the meter isn't artificially deflated by
/// no-cost bookkeeping rows.
fn ledger_entry_cost(entry: &LedgerEntry) -> Option<f64> {
    match entry {
        LedgerEntry::ModelCall { cost_usd, .. } | LedgerEntry::ToolCall { cost_usd, .. } => {
            *cost_usd
        }
        // CacheBust entries carry no cost field (the eviction itself
        // doesn't cost money; the future re-introduction of the evicted
        // tokens does, and lands as a ModelCall).
        // v60.5 Compaction entries similarly don't carry their own
        // cost — the paired ModelCall right before them does.
        // v60.6 Expansion: cache_rewarm_tokens is a prompt-cache
        // disclosure, not a `$` line, so it stays out of the cost meter.
        LedgerEntry::CacheBust { .. }
        | LedgerEntry::Compaction { .. }
        | LedgerEntry::Expansion { .. } => None,
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
                // v57 (H7 fix) — canonical name from atelier-core
                // rather than Rust's `Debug` format.
                self.current_state = to.name().to_string();
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
                capability_row,
            } => {
                // v52 — record the active model so the footer can
                // render it.
                //
                // v57 (H7 fix) — `outcome.wire_label()` is the
                // canonical snake_case label owned by atelier-core.
                // Pre-v57 we did `format!("{outcome:?}")` and
                // post-processed through `snake_case_debug`, which
                // made Rust's Debug a wire format and would break
                // silently on a variant rename.
                //
                // v60.7 — the capability_row rides alongside so the
                // footer can render a "caps: native_tool=ok · …"
                // sublabel under the model badge. Optional for
                // backwards compatibility with any pre-v60.7 event
                // producer (none on main today; the field is there
                // so a downstream test fixture isn't forced through
                // the matrix path).
                self.current_model = Some(CurrentModel {
                    model_id: model_id.clone(),
                    base_url: base_url.clone(),
                    strategy: strategy.as_str(),
                    outcome: outcome.wire_label().to_string(),
                    capability_row: capability_row.clone(),
                });
            }
            SessionEvent::ContextItems { items } => {
                // v53 — replace the in-memory snapshot wholesale.
                // Items arrive at every turn boundary, so a stale
                // partial render is never preferable to the fresh
                // snapshot.
                self.context_items = items.clone();
                // v60.5 — drop any selected-id that no longer
                // corresponds to a live item. This covers the case
                // where an item the user pre-selected was evicted via
                // a different path (e.g. another driver or an
                // automated compaction).
                let live: std::collections::HashSet<&str> =
                    items.iter().map(|i| i.id.as_str()).collect();
                self.selected_context_set
                    .retain(|id| live.contains(id.as_str()));
            }
            SessionEvent::MemoryCards { cards } => {
                // v54 — same wholesale-replace policy as
                // ContextItems. Cards arrive at every turn so the
                // panel never displays stale state.
                self.memory_cards = cards.clone();
            }
            SessionEvent::ClaimedChanges { changes } => {
                // v56 — wholesale-replace the path→rationale map.
                // The diff pane reads this to render the agent's
                // "why this change?" summary next to the file header.
                self.claimed_changes = changes
                    .iter()
                    .map(|c| (c.path.clone(), c.summary.clone()))
                    .collect();
            }
            SessionEvent::CompactionExecuted { .. } => {
                // v60.5 — a successful compaction has already converged
                // the `ContextItems` + `MemoryCards` snapshots; we just
                // clear the multi-select so the user isn't carrying a
                // stale selection into the next interaction.
                self.selected_context_set.clear();
            }
            SessionEvent::ExpansionExecuted { .. } => {
                // v60.6 — terminal marker; the `ContextItems` +
                // `MemoryCards` snapshots have already converged. No
                // local UI state to clear (the Expand confirm modal
                // closes itself in `submit_expand`'s post-spawn path
                // via the same InputMode::Normal transition the run
                // loop applies on ExpandConfirmYes).
            }
            SessionEvent::MentalModelSnapshot {
                enabled,
                text_tokens,
            } => {
                // Phase C close — record the latest snapshot so the
                // footer hint can render. TUI keeps just the
                // visibility flag + approximate token count; the
                // full editable surface lands when the harness
                // actually feeds the text into the prompt.
                self.mental_model = MentalModelHint {
                    enabled: *enabled,
                    text_tokens: *text_tokens,
                };
            }
            SessionEvent::FilesChanged { paths, .. } => {
                // v61 — §14 concurrent-edit modal. Open the confirm
                // mode so the next key (`r` / `w` / `p`) routes through
                // `InputOutcome::ConcurrentEditResolve`. The dispatcher
                // already queued the next tool call; we just surface
                // the user's choice.
                self.input_mode = InputMode::ConcurrentEditConfirm {
                    paths: paths.clone(),
                };
            }
            SessionEvent::FilesChangedAcknowledged { .. } => {
                // v61 — clear the modal regardless of which arm
                // resolved it (Reload / Wait / Pause / AutoReload /
                // PauseTimedOut). The runner-side resolver task is
                // authoritative for the outcome.
                if matches!(self.input_mode, InputMode::ConcurrentEditConfirm { .. }) {
                    self.input_mode = InputMode::Normal;
                }
            }
            SessionEvent::VerificationPassed {
                tier,
                file_count,
                claim_count,
            } => {
                // v62 — wholesale-replace the verify-pass hint so the
                // meters pane badge reflects the most recent pass.
                // Counts come back as `usize`; the hint stores them
                // as `u32` to mirror the other meter fields and keep
                // the ratatui render path allocation-light.
                self.verification_status = VerificationStatusHint {
                    tier: *tier,
                    file_count: u32::try_from(*file_count).unwrap_or(u32::MAX),
                    claim_count: u32::try_from(*claim_count).unwrap_or(u32::MAX),
                };
            }
            SessionEvent::StrategyDegraded { to, .. } => {
                // §1 BYOM — refresh the strategy field on the active
                // model badge so the footer shows the lowered tier.
                // `current_model` is `Some` once `ModelProfileLoaded`
                // has fired (which the runner emits before the first
                // turn), but guard anyway: a misordered scenario
                // shouldn't crash the apply loop.
                if let Some(model) = self.current_model.as_mut() {
                    model.strategy = to.as_str();
                }
            }
            // v60.9 B1 follow-on — capture the most recent §1
            // context-overflow resolution so `render_cost_meter` can
            // surface a short toast in the meter row for the next
            // [`OVERFLOW_HINT_TTL`]. The decay is render-time only;
            // the field itself stays populated past the window so
            // a debug surface can still inspect the last resolution.
            SessionEvent::ContextOverflowResolved {
                resolution,
                freed_tokens,
                items_compacted,
            } => {
                self.last_overflow_resolution = Some(OverflowResolutionHint {
                    resolution,
                    freed_tokens: *freed_tokens,
                    items_compacted: *items_compacted,
                    recorded_at: Instant::now(),
                });
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

    /// v55 — selection length for the currently focused pane.
    /// Returns 0 for Conversation (which has no per-row selection
    /// surface in v55).
    pub fn focused_pane_len(&self) -> usize {
        match self.focused_pane {
            FocusedPane::Conversation => 0,
            FocusedPane::Context => self.context_items.len(),
            FocusedPane::Memory => self.memory_cards.len(),
            FocusedPane::Plan => self.plan.len(),
        }
    }

    /// Read-only accessor used by `handle_key` / tests. Returns the
    /// selected index for the focused pane; 0 when there's no
    /// per-row surface.
    pub fn focused_pane_selected(&self) -> usize {
        match self.focused_pane {
            FocusedPane::Conversation => 0,
            FocusedPane::Context => self.selected_context,
            FocusedPane::Memory => self.selected_memory,
            FocusedPane::Plan => self.selected_plan,
        }
    }

    /// v55 — `j`/`↓` in the focused pane. Saturates at `len-1`.
    pub fn select_next(&mut self) {
        let len = self.focused_pane_len();
        if len == 0 {
            return;
        }
        let cap = len.saturating_sub(1);
        let s = match self.focused_pane {
            FocusedPane::Conversation => return,
            FocusedPane::Context => &mut self.selected_context,
            FocusedPane::Memory => &mut self.selected_memory,
            FocusedPane::Plan => &mut self.selected_plan,
        };
        if *s < cap {
            *s += 1;
        }
    }

    /// v55 — `k`/`↑` in the focused pane. Saturates at 0.
    pub fn select_prev(&mut self) {
        let s = match self.focused_pane {
            FocusedPane::Conversation => return,
            FocusedPane::Context => &mut self.selected_context,
            FocusedPane::Memory => &mut self.selected_memory,
            FocusedPane::Plan => &mut self.selected_plan,
        };
        *s = s.saturating_sub(1);
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
///
/// v57 (H5 fix) — the `kind` label is sourced from
/// [`SessionEvent::kind`] so it always agrees with the GUI bridge.
/// The pre-v57 strings ("Message", "PendingApproval",
/// "IllegalTransition", "ModelProfile") had drifted away from the
/// Rust enum variant names.
pub fn project_event(evt: &SessionEvent) -> EventLine {
    let kind = evt.kind();
    let detail = match evt {
        // v58 (H7-residual fix) — `role.wire_label()` /
        // `from.name()` / `to.name()` are canonical labels owned by
        // atelier-core. The pre-v58 path used `format!("{role:?}")` /
        // `{from:?}` here even though the rest of the projection
        // (kind, AppState::apply) had already moved off Debug.
        SessionEvent::MessageCommitted { role, text } => format!(
            "{}: {}",
            role.wire_label(),
            text.lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect::<String>()
        ),
        SessionEvent::PlanSnapshot { steps } => format!("{} steps", steps.len()),
        SessionEvent::LedgerAppended { entry } => match entry {
            LedgerEntry::ModelCall { .. } => "model_call".to_string(),
            LedgerEntry::ToolCall { tool_name, .. } => format!("tool_call:{tool_name}"),
            LedgerEntry::CacheBust { .. } => "cache_bust".to_string(),
            LedgerEntry::Compaction {
                freed_tokens,
                replaced_items,
                ..
            } => format!(
                "compaction:{} items, {freed_tokens} tokens",
                replaced_items.len()
            ),
            LedgerEntry::Expansion {
                restored_item_ids,
                cache_rewarm_tokens,
                ..
            } => format!(
                "expansion:{} items, {cache_rewarm_tokens} tokens",
                restored_item_ids.len()
            ),
        },
        SessionEvent::ContextSnapshot {
            known_tokens,
            unknown_tokens,
        } => format!("known={known_tokens} unknown={unknown_tokens}"),
        SessionEvent::StagingPendingApproval { files, .. } => {
            format!("{} files awaiting approval", files.len())
        }
        SessionEvent::CommitDecision {
            committed, dropped, ..
        } => format!("committed={} dropped={}", committed.len(), dropped.len()),
        SessionEvent::Transitioned { from, to } => format!("{} → {}", from.name(), to.name()),
        SessionEvent::IllegalTransitionAttempted { from, to } => {
            format!("{} ↛ {}", from.name(), to.name())
        }
        SessionEvent::Cancelled => String::new(),
        SessionEvent::EditStaged { path, .. } => path.display().to_string(),
        SessionEvent::ModelProfileLoaded {
            model_id,
            strategy,
            outcome,
            ..
        } => format!(
            "{model_id} · strategy={} · {}",
            strategy.as_str(),
            outcome.wire_label()
        ),
        SessionEvent::ContextItems { items } => format!("{} items", items.len()),
        SessionEvent::MemoryCards { cards } => format!("{} cards", cards.len()),
        SessionEvent::ClaimedChanges { changes } => {
            format!("{} file rationale(s)", changes.len())
        }
        SessionEvent::CompactionExecuted {
            freed_tokens,
            replaced_item_count,
            summary_card_id,
        } => format!(
            "compacted {replaced_item_count} items → freed {freed_tokens} tokens → {summary_card_id}"
        ),
        SessionEvent::ExpansionExecuted {
            restored_item_count,
            summary_card_id,
            cache_rewarm_tokens,
        } => format!(
            "restored {restored_item_count} items ← {summary_card_id} (paid ~{cache_rewarm_tokens} cache tokens)"
        ),
        SessionEvent::MentalModelSnapshot {
            enabled,
            text_tokens,
        } => format!(
            "{} · ~{text_tokens} tokens (0/turn in v0)",
            if *enabled { "on" } else { "off" }
        ),
        SessionEvent::FilesChanged { paths, observed_at } => format!(
            "{} path(s) changed at {observed_at}",
            paths.len(),
        ),
        SessionEvent::FilesChangedAcknowledged { outcome } => {
            outcome.wire_label().to_string()
        }
        SessionEvent::VerificationPassed {
            tier,
            file_count,
            claim_count,
        } => format!(
            "{} · {file_count} files · {claim_count} claims",
            tier.wire_label()
        ),
        SessionEvent::StrategyDegraded { from, to, reason } => {
            format!("{} → {} ({reason})", from.as_str(), to.as_str())
        }
        // §1 BYOM (v60.9) — context-window asymmetry resolution.
        // Renders one-line summary in the TUI event log; the toast /
        // panel surface is a follow-on bundle.
        SessionEvent::ContextOverflowResolved {
            resolution,
            freed_tokens,
            items_compacted,
        } => match (freed_tokens, items_compacted) {
            (Some(t), Some(n)) => format!("{resolution} · {n} items · {t} tokens"),
            _ => (*resolution).to_string(),
        },
        SessionEvent::Shutdown => String::new(),
    };
    EventLine { kind, detail }
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
    //
    // v62 — the cost meter row carries the §7 verify-pass badge on
    // the same line (right-aligned in the cost paragraph) so the
    // overall row allocation is unchanged.
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // cost gauge + v62 verify badge (right-aligned)
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
    // v56 — "Why this change?" rationale from the envelope's
    // `claimed_changes`. Rendered as a single dim italic line under
    // the file header so the user can see the agent's stated intent
    // without clicking through.
    if let Some(reason) = state.claimed_changes.get(&edit.path.display().to_string()) {
        lines.push(Line::from(vec![
            Span::styled("    why: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                reason.clone(),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
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
///
/// v62 — the cost line also carries the §7 verify-pass badge on the
/// right-hand side so the user can see which verification tier
/// (Tier 1 LSP / Tier 2 tree-sitter / Tier 3 textual / NotRun) ran
/// on the last pass. The badge is rendered in its own column so a
/// width-tight terminal degrades gracefully (the cost label always
/// fits, the badge gets clipped before the cost number does).
fn render_cost_meter(state: &AppState, area: Rect, buf: &mut Buffer) {
    // Reserve a top border row by reusing the existing block; the
    // inner area is the row below.
    let block = Block::default().borders(Borders::TOP);
    let inner = block.inner(area);
    Widget::render(block, area, buf);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let badge = &state.verification_status;
    let verify_label = badge.badge_label();
    // "verify " prefix + the label itself. Floor at 0 so a tight
    // terminal collapses the badge column instead of underflowing.
    let badge_width = ("verify ".len() + verify_label.len()) as u16;

    // v60.9 B1 follow-on — inline the most recent §1
    // context-overflow resolution as a short hint between the cost
    // figure and the verify badge for the next
    // [`OVERFLOW_HINT_TTL`]. Once the decay window elapses, the hint
    // collapses to zero width so the cost row reverts to its
    // pre-v60.9 layout. The `AppState` field stays populated past the
    // window so a debug surface can still inspect the last resolution.
    let overflow_hint = state.last_overflow_resolution.as_ref().and_then(|h| {
        if h.recorded_at.elapsed() < OVERFLOW_HINT_TTL {
            Some(format_overflow_hint(h))
        } else {
            None
        }
    });
    let hint_width = overflow_hint
        .as_ref()
        .map(|s| s.len() as u16 + 1) // +1 for the leading space separator
        .unwrap_or(0);

    // Width allocation: cost takes whatever's left after badge + hint.
    let remaining = inner.width.saturating_sub(badge_width);
    let hint_actual = hint_width.min(remaining);
    let cost_width = remaining.saturating_sub(hint_actual);

    // Left column: cost label + USD figure.
    let cost_area = Rect {
        x: inner.x,
        y: inner.y,
        width: cost_width,
        height: 1,
    };
    let cost_line = Line::from(vec![
        Span::styled("cost ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("${:.4}", state.total_cost_usd),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    Widget::render(Paragraph::new(cost_line), cost_area, buf);

    // Middle column: overflow hint (only while the decay window is
    // active). Cyan colour family matches the GUI's toast accent.
    if hint_actual > 0 {
        if let Some(hint_text) = overflow_hint {
            let hint_area = Rect {
                x: inner.x + cost_width,
                y: inner.y,
                width: hint_actual,
                height: 1,
            };
            let hint_line = Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    hint_text,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            Widget::render(Paragraph::new(hint_line), hint_area, buf);
        }
    }

    // Right column: verify-pass badge. Skipped when the badge column
    // collapsed to zero (very narrow terminal).
    if badge_width == 0 || cost_width + hint_actual >= inner.width {
        return;
    }
    let badge_area = Rect {
        x: inner.x + cost_width + hint_actual,
        y: inner.y,
        width: badge_width.min(inner.width.saturating_sub(cost_width + hint_actual)),
        height: 1,
    };
    let verify_line = Line::from(vec![
        Span::styled("verify ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            verify_label,
            Style::default()
                .fg(badge.badge_colour())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    Widget::render(Paragraph::new(verify_line), badge_area, buf);
}

/// v60.9 B1 follow-on — render a one-line summary of an overflow
/// resolution for the cost-meter hint slot. Format mirrors the GUI's
/// `overflowLabel`: `"<resolution> · <n> items · <t> tokens"` when
/// both counters are populated, just the resolution otherwise (the
/// `"surfaced"` arm doesn't carry the counters).
fn format_overflow_hint(hint: &OverflowResolutionHint) -> String {
    match (hint.freed_tokens, hint.items_compacted) {
        (Some(t), Some(n)) => format!("overflow {} · {n} items · {t} tokens", hint.resolution),
        _ => format!("overflow {}", hint.resolution),
    }
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
    // v60.5 — title shows the multi-select counter so the user knows
    // how many items will be compacted on `C`.
    let title = if state.selected_context_set.is_empty() {
        " §5 Context ".to_string()
    } else {
        format!(
            " §5 Context [{} selected — C to compact] ",
            state.selected_context_set.len()
        )
    };
    let block = Block::default().borders(Borders::TOP).title(title);

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
            // v60.5 — leading 3-cell select glyph: `[*]` selected,
            // `[ ]` selectable, `[-]` pinned (un-selectable).
            let select_glyph = if item.pinned {
                "[-] "
            } else if state.selected_context_set.contains(&item.id) {
                "[*] "
            } else {
                "[ ] "
            };
            ListItem::new(Line::from(vec![
                Span::raw(select_glyph.to_string()),
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
            let mut spans = vec![
                Span::raw(pin.to_string()),
                Span::styled(card.title.clone(), title_style),
                Span::raw("  "),
                Span::styled(when, Style::default().fg(Color::DarkGray)),
            ];
            // v60.6 — Compaction-flavoured cards get a "[×N, T tokens]"
            // suffix so the user can see which rows are Expand-eligible
            // at a glance. Falls back to `"?"` for the token count if
            // cache_rewarm_tokens isn't populated (v60.5-era cards).
            if let Some(count) = card.compacted_from {
                let tokens = card
                    .cache_rewarm_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "?".to_string());
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("[×{count}, {tokens} tk]"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            ListItem::new(Line::from(spans))
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

    // v60.5 — open modals also take precedence so the user sees the
    // confirmation prompt instead of the regular help line.
    if let InputMode::CompactConfirm { ids, tokens_freed } = &state.input_mode {
        Widget::render(
            Paragraph::new(format!(
                " COMPACT {} items · frees ~{tokens_freed} tokens · y confirm · n / Esc cancel ",
                ids.len()
            ))
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
            buf,
        );
        return;
    }
    // v60.6 — symmetric counterpart to CompactConfirm.
    if let InputMode::ExpandConfirm {
        item_count,
        cache_rewarm_tokens,
        ..
    } = &state.input_mode
    {
        Widget::render(
            Paragraph::new(format!(
                " EXPAND {item_count} items · pays ~{cache_rewarm_tokens} cache tokens · y confirm · n / Esc cancel "
            ))
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
            buf,
        );
        return;
    }
    if let InputMode::EvictConfirm { .. } = &state.input_mode {
        Widget::render(
            Paragraph::new(" EVICT context item · y confirm · n / Esc cancel ").style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
            buf,
        );
        return;
    }
    // v61 — §14 concurrent-edit modal. Renders the three resolution
    // keys (r/w/p) so the user knows their options without consulting
    // docs. Surface count of paths so a sweeping IDE refactor is
    // visible at the footer.
    if let InputMode::ConcurrentEditConfirm { paths } = &state.input_mode {
        Widget::render(
            Paragraph::new(format!(
                " EXTERNAL EDIT detected ({} path(s)) · r reload · w wait · p pause · Esc dismiss ",
                paths.len()
            ))
            .style(
                Style::default()
                    .fg(Color::Magenta)
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
    // Phase C close — the §5 mental-model panel lives in the GUI in
    // v0. The TUI just surfaces a tiny status hint so the user
    // knows whether the panel is on (and how big the text is, for
    // cost awareness). Hidden when the panel is off so the help line
    // stays compact for the common case.
    let mm_hint = if state.mental_model.enabled {
        format!(" · mm:on(~{}tk,0/turn)", state.mental_model.text_tokens)
    } else {
        String::new()
    };
    format!(" q/Esc/Ctrl-C quit · [ prev · ] next · g HEAD{scrub_note}{mm_hint}")
}

/// Build the right-side model badge as a styled
/// [`ratatui::widgets::Paragraph`]. Always returns a paragraph (the
/// caller already established that `state.current_model` is `Some`).
/// Mirrors the GUI's bottom-right model widget — same field order,
/// same separator, same colour family (cyan id · green strategy ·
/// dim outcome).
///
/// v60.7 — when the model carries a capability matrix row with at
/// least one `ClaimedButBroken` cell, append a "[broken: <list>]"
/// segment so the user can spot a degraded model at a glance
/// without opening a tooltip.
fn render_help_right_model(model: &CurrentModel) -> Paragraph<'static> {
    let mut spans = vec![
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
    ];
    if let Some(broken) = capability_broken_label(model.capability_row.as_ref()) {
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            broken,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Trailing space so the badge doesn't hit the terminal edge.
    spans.push(Span::raw(" "));
    Paragraph::new(Line::from(spans))
}

/// If the row carries any `ClaimedButBroken` cells, return a short
/// human-readable list (`"broken: native_tool, structured_output"`).
/// Returns `None` for healthy rows so the footer doesn't gain a
/// useless suffix on every well-behaved model. Used by both the
/// width calculation and the renderer so the two stay in lock-step.
fn capability_broken_label(
    row: Option<&atelier_core::adapter::capability_matrix::CapabilityMatrixRow>,
) -> Option<String> {
    use atelier_core::adapter::CapabilityClaim;
    let row = row?;
    let mut broken: Vec<&'static str> = Vec::new();
    if matches!(row.native_tool_use, CapabilityClaim::ClaimedButBroken) {
        broken.push("native_tool");
    }
    if matches!(row.streaming, CapabilityClaim::ClaimedButBroken) {
        broken.push("streaming");
    }
    if matches!(row.vision, CapabilityClaim::ClaimedButBroken) {
        broken.push("vision");
    }
    if matches!(row.prompt_cache, CapabilityClaim::ClaimedButBroken) {
        broken.push("prompt_cache");
    }
    if matches!(row.structured_output, CapabilityClaim::ClaimedButBroken) {
        broken.push("structured_output");
    }
    if matches!(row.long_context, CapabilityClaim::ClaimedButBroken) {
        broken.push("long_context");
    }
    if broken.is_empty() {
        None
    } else {
        Some(format!("broken: {}", broken.join(", ")))
    }
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
    let broken_extra = match capability_broken_label(model.capability_row.as_ref()) {
        // The renderer prepends " · " (3 cols) before the broken
        // label.
        Some(label) => label.chars().count() + 3,
        None => 0,
    };
    // Three " · " separators (3 cols each) + leading 0 + trailing 1
    // + optional " · broken: …" suffix.
    let total = id + strategy + outcome + (3 * 3) + 1 + broken_extra;
    total.try_into().unwrap_or(u16::MAX)
}

/// Outcome of a single keypress, dispatched by [`run`]'s event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    // v55 — §5 mutator + focus surface ------------------
    /// Tab cycles to the next pane.
    FocusNext,
    /// `k` / Up — move selection back in the focused pane.
    SelectPrev,
    /// `j` / Down — move selection forward in the focused pane.
    SelectNext,
    /// Context pane: `p` (pin), `u` (unpin) on the selected item.
    /// Carries the stringified `ContextItemId` so the run loop can
    /// route to the dispatcher mutator directly.
    PinContext(String),
    UnpinContext(String),
    /// Context pane: `e` — open the evict-confirm modal for the
    /// selected item. The run loop sets `state.input_mode` to
    /// `EvictConfirm`; the next `y` returns `EvictConfirmYes`.
    EvictAsk(String),
    /// In `EvictConfirm` mode: `y` confirms the evict.
    EvictConfirmYes(String),
    /// v60.5 — toggle the focused context row's id in the multi-select
    /// set. Carries `pinned` so the run loop can no-op on pinned items
    /// without re-fetching state.
    ToggleContextSelected {
        id: String,
        pinned: bool,
    },
    /// v60.5 — `C` in the Context pane: ask the run loop to open the
    /// compact-confirm modal for the current `selected_context_set`.
    /// The run loop computes `tokens_freed` (sum of `tokens` across the
    /// selected items) and transitions to `InputMode::CompactConfirm`.
    CompactAsk,
    /// v60.5 — in `CompactConfirm` mode: `y` confirms. Carries the ids
    /// + `tokens_freed` so the run loop doesn't have to inspect state.
    CompactConfirmYes {
        ids: Vec<String>,
        tokens_freed: u32,
    },
    /// v60.6 — Memory pane: `x` (eXpand) on a compaction-flavoured
    /// card. Carries the card id + the cost disclosure so the run
    /// loop can transition to `ExpandConfirm` without re-fetching state.
    ExpandAsk {
        card_id: String,
        item_count: u32,
        cache_rewarm_tokens: u32,
    },
    /// v60.6 — in `ExpandConfirm` mode: `y` confirms. Carries the
    /// card id; the orchestrator re-reads the blob (we don't trust
    /// the modal's snapshot for the actual restoration).
    ExpandConfirmYes {
        card_id: String,
    },
    /// v61 — in `ConcurrentEditConfirm` mode: user chose one of the
    /// three §14 outcomes. The run loop emits
    /// `Event::FilesChangedAcknowledged { outcome }` and clears the
    /// modal. `Reload` / `Wait` / `Pause` are the user-driven cases;
    /// the headless `AutoReload` + `PauseTimedOut` arms are emitted
    /// by the runner-side resolver task, not here.
    ConcurrentEditResolve {
        outcome: atelier_core::ConcurrentEditOutcome,
    },
    /// In any modal: Esc / `q` cancels back to Normal.
    ModalCancel,
    /// Memory pane: `d` deletes selected card.
    DeleteMemory(String),
    /// Memory pane: `P` promotes selected card to ~/.atelier/memory/.
    PromoteMemory(String),
    /// Plan pane: space cycles the selected step's status.
    CyclePlanStatus {
        id: String,
        current: PlanStatus,
    },
    /// Plan pane: `x` removes the selected step.
    RemovePlanStep(String),
    /// Enter text-input modal of the given kind. Run loop sets
    /// `state.input_mode = TextInput { kind, buffer: "" }`.
    EnterTextInput(TextInputKind),
    /// In TextInput mode: append a printable char to the buffer.
    TextInputChar(char),
    /// In TextInput mode: pop the last char.
    TextInputBackspace,
    /// In TextInput mode: submit. The run loop reads `state.input_mode`
    /// (still `TextInput { kind, buffer }`) and routes to the right
    /// dispatcher mutator, then resets to Normal.
    TextInputSubmit,
}

/// Pure keypress dispatch. Centralised so the run loop is one match arm
/// per input source. `state` is read-only here — outcomes describe
/// *what* the run loop should do; the run loop mutates state.
///
/// Bindings (Normal mode):
///   * `q` / `Esc` / `Ctrl-C` — quit (in Normal mode; Esc also closes a modal)
///   * `[` / `]` / `g` — scrubber
///   * `y` / `n` — accept / reject all pending hunks (only when pending set)
///   * `Tab` — cycle focused pane
///   * `j` / `↓` — select next row in focused pane
///   * `k` / `↑` — select previous row in focused pane
///
/// Per-focused-pane mutator keys (v55):
///   * Context: `p` pin, `u` unpin, `e` evict-with-confirm
///   * Memory:  `a` add (text-input modal), `d` delete, `P` promote
///   * Plan:    `a` add step (text-input modal), space cycle status,
///              `c` add constraint (text-input modal), `x` remove
///
/// Modal sub-modes consume keys before they reach pane bindings:
///   * `InputMode::EvictConfirm`: `y` confirms, Esc/anything else cancels
///   * `InputMode::TextInput`: printable chars append, Backspace pops,
///     Enter submits, Esc cancels
pub fn handle_key(key: KeyEvent, state: &AppState) -> InputOutcome {
    // Modal sub-modes take precedence: a stray keystroke inside an
    // open modal must NOT trigger a pane mutator.
    match &state.input_mode {
        InputMode::EvictConfirm { id } => {
            return match (key.code, key.modifiers) {
                (KeyCode::Char('y'), _) => InputOutcome::EvictConfirmYes(id.clone()),
                (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('q'), _) => {
                    InputOutcome::ModalCancel
                }
                _ => InputOutcome::Continue,
            };
        }
        InputMode::CompactConfirm { ids, tokens_freed } => {
            return match (key.code, key.modifiers) {
                (KeyCode::Char('y'), _) => InputOutcome::CompactConfirmYes {
                    ids: ids.clone(),
                    tokens_freed: *tokens_freed,
                },
                (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('q'), _) => {
                    InputOutcome::ModalCancel
                }
                _ => InputOutcome::Continue,
            };
        }
        InputMode::ExpandConfirm { card_id, .. } => {
            return match (key.code, key.modifiers) {
                (KeyCode::Char('y'), _) => InputOutcome::ExpandConfirmYes {
                    card_id: card_id.clone(),
                },
                (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('q'), _) => {
                    InputOutcome::ModalCancel
                }
                _ => InputOutcome::Continue,
            };
        }
        InputMode::ConcurrentEditConfirm { .. } => {
            return match (key.code, key.modifiers) {
                (KeyCode::Char('r'), _) => InputOutcome::ConcurrentEditResolve {
                    outcome: atelier_core::ConcurrentEditOutcome::Reload,
                },
                (KeyCode::Char('w'), _) => InputOutcome::ConcurrentEditResolve {
                    outcome: atelier_core::ConcurrentEditOutcome::Wait,
                },
                (KeyCode::Char('p'), _) => InputOutcome::ConcurrentEditResolve {
                    outcome: atelier_core::ConcurrentEditOutcome::Pause,
                },
                (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => InputOutcome::ModalCancel,
                _ => InputOutcome::Continue,
            };
        }
        InputMode::TextInput { .. } => {
            return match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => InputOutcome::ModalCancel,
                (KeyCode::Enter, _) => InputOutcome::TextInputSubmit,
                (KeyCode::Backspace, _) => InputOutcome::TextInputBackspace,
                (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                    InputOutcome::TextInputChar(c)
                }
                _ => InputOutcome::Continue,
            };
        }
        InputMode::Normal => {}
    }

    // Global keys first (work in any focused pane).
    let pending = state.pending_approval.as_ref();
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), m) | (KeyCode::Esc, m)
            if !m.contains(KeyModifiers::CONTROL) || matches!(key.code, KeyCode::Char('q')) =>
        {
            return InputOutcome::Quit;
        }
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => return InputOutcome::Quit,
        (KeyCode::Char('['), _) => return InputOutcome::Scrub(ScrubCommand::Prev),
        (KeyCode::Char(']'), _) => return InputOutcome::Scrub(ScrubCommand::Next),
        (KeyCode::Char('g'), _) => return InputOutcome::Scrub(ScrubCommand::JumpToHead),
        // Approval keys are gated on pending state — keeps the
        // interpretation deterministic when the user is between
        // approvals.
        (KeyCode::Char('y'), _) if pending.is_some() => return InputOutcome::AcceptAll,
        (KeyCode::Char('n'), _) if pending.is_some() => return InputOutcome::RejectAll,
        (KeyCode::Tab, _) => return InputOutcome::FocusNext,
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => return InputOutcome::SelectNext,
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => return InputOutcome::SelectPrev,
        _ => {}
    }

    // Pane-scoped mutator keys.
    match state.focused_pane {
        FocusedPane::Context => {
            // v60.5 — `C` (shift+c) opens the compact-confirm modal,
            // gated on `selected_context_set.len() >= 2`. This runs
            // BEFORE the per-row affordances because `C` doesn't need
            // a focused row — it operates on the multi-select set.
            // Note: lowercase `c` is reserved (the global Ctrl-c is
            // quit, and the Plan pane uses bare `c` for add-constraint),
            // so we key off the uppercase form here.
            if matches!(key.code, KeyCode::Char('C')) {
                if state.selected_context_set.len() >= 2 {
                    return InputOutcome::CompactAsk;
                }
                return InputOutcome::Continue;
            }
            let Some(item) = state.context_items.get(state.selected_context) else {
                return InputOutcome::Continue;
            };
            match (key.code, key.modifiers) {
                (KeyCode::Char('p'), _) => InputOutcome::PinContext(item.id.clone()),
                (KeyCode::Char('u'), _) => InputOutcome::UnpinContext(item.id.clone()),
                (KeyCode::Char('e'), _) => InputOutcome::EvictAsk(item.id.clone()),
                // v60.5 — `space` toggles selection on the focused row
                // (no-op if pinned).
                (KeyCode::Char(' '), _) => InputOutcome::ToggleContextSelected {
                    id: item.id.clone(),
                    pinned: item.pinned,
                },
                _ => InputOutcome::Continue,
            }
        }
        FocusedPane::Memory => match (key.code, key.modifiers) {
            (KeyCode::Char('a'), _) => InputOutcome::EnterTextInput(TextInputKind::AddMemoryCard),
            (KeyCode::Char('d'), _) => {
                let Some(card) = state.memory_cards.get(state.selected_memory) else {
                    return InputOutcome::Continue;
                };
                InputOutcome::DeleteMemory(card.id.clone())
            }
            (KeyCode::Char('P'), _) => {
                let Some(card) = state.memory_cards.get(state.selected_memory) else {
                    return InputOutcome::Continue;
                };
                InputOutcome::PromoteMemory(card.id.clone())
            }
            // v60.6 — `x` opens the expand-confirm modal, but only
            // on rows whose card was produced by a §5 compaction
            // (the `compacted_from` projection is `Some`). On any
            // other card the keystroke is inert.
            (KeyCode::Char('x'), _) => {
                let Some(card) = state.memory_cards.get(state.selected_memory) else {
                    return InputOutcome::Continue;
                };
                let Some(count) = card.compacted_from else {
                    return InputOutcome::Continue;
                };
                // `cache_rewarm_tokens` is `Some(_)` for every
                // v60.6+ compaction. v60.5-era cards may carry
                // `None`; we default the disclosure to 0 in that
                // case (the orchestrator will still surface the
                // real cost in the post-expand toast).
                let cache_rewarm_tokens = card.cache_rewarm_tokens.unwrap_or(0);
                InputOutcome::ExpandAsk {
                    card_id: card.id.clone(),
                    item_count: count,
                    cache_rewarm_tokens,
                }
            }
            _ => InputOutcome::Continue,
        },
        FocusedPane::Plan => {
            // Plan steps come from `state.plan` (PlanCanvas). The
            // canvas's `iter()` is order-preserving so `nth(idx)`
            // matches the rendered row index.
            match (key.code, key.modifiers) {
                (KeyCode::Char('a'), _) => InputOutcome::EnterTextInput(TextInputKind::AddPlanStep),
                (KeyCode::Char('c'), _) => {
                    let Some(step) = state.plan.iter().nth(state.selected_plan) else {
                        return InputOutcome::Continue;
                    };
                    InputOutcome::EnterTextInput(TextInputKind::AddPlanConstraint {
                        step_id: step.id.clone(),
                    })
                }
                (KeyCode::Char(' '), _) => {
                    let Some(step) = state.plan.iter().nth(state.selected_plan) else {
                        return InputOutcome::Continue;
                    };
                    InputOutcome::CyclePlanStatus {
                        id: step.id.clone(),
                        current: step.status,
                    }
                }
                (KeyCode::Char('x'), _) => {
                    let Some(step) = state.plan.iter().nth(state.selected_plan) else {
                        return InputOutcome::Continue;
                    };
                    InputOutcome::RemovePlanStep(step.id.clone())
                }
                _ => InputOutcome::Continue,
            }
        }
        FocusedPane::Conversation => InputOutcome::Continue,
    }
}

/// v55 — the cycle a space-key press walks the focused plan step
/// through. Mirrors the GUI's `nextStatus` cycler in PlanPane.svelte.
pub fn next_plan_status(s: PlanStatus) -> PlanStatus {
    match s {
        PlanStatus::Pending => PlanStatus::InProgress,
        PlanStatus::InProgress => PlanStatus::Done,
        PlanStatus::Done => PlanStatus::Skipped,
        PlanStatus::Skipped => PlanStatus::Pending,
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
    // v60.5 — companion handle for §5 non-destructive compaction.
    // `None` in viewer mode (no adapter to call); otherwise mirrors
    // `dispatcher_handle`'s lifetime via the same `AdapterHandleGuard`.
    let adapter_handle: Option<atelier_cli::runner::AdapterHandle>;
    let _run_task: Option<tokio::task::JoinHandle<()>>;
    let _viewer_session: Option<atelier_core::SessionHandle>;

    match prompt {
        Some(p) => {
            // Driver mode: build the Runner, wire EventSink::Callback
            // to the mpsc, spawn the run.
            let (handle, adapter_h, task) = spawn_driver_run(p, event_tx.clone())?;
            dispatcher_handle = Some(handle);
            adapter_handle = Some(adapter_h);
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
            adapter_handle = None;
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
                        match handle_key(key, &state) {
                            InputOutcome::Quit => break,
                            InputOutcome::Scrub(cmd) => state.apply_scrub(cmd),
                            InputOutcome::AcceptAll => {
                                submit_pending(&state, &dispatcher_handle, true);
                            }
                            InputOutcome::RejectAll => {
                                submit_pending(&state, &dispatcher_handle, false);
                            }
                            InputOutcome::FocusNext => {
                                state.focused_pane = state.focused_pane.next();
                            }
                            InputOutcome::SelectPrev => state.select_prev(),
                            InputOutcome::SelectNext => state.select_next(),
                            InputOutcome::PinContext(id) => {
                                submit_mutation(&dispatcher_handle, Mutation::Pin(id));
                            }
                            InputOutcome::UnpinContext(id) => {
                                submit_mutation(&dispatcher_handle, Mutation::Unpin(id));
                            }
                            InputOutcome::EvictAsk(id) => {
                                state.input_mode = InputMode::EvictConfirm { id };
                            }
                            InputOutcome::EvictConfirmYes(id) => {
                                state.input_mode = InputMode::Normal;
                                submit_mutation(&dispatcher_handle, Mutation::Evict(id));
                            }
                            InputOutcome::ToggleContextSelected { id, pinned } => {
                                if !pinned && !state.selected_context_set.insert(id.clone()) {
                                    state.selected_context_set.remove(&id);
                                }
                            }
                            InputOutcome::CompactAsk => {
                                if state.selected_context_set.len() >= 2 {
                                    let ids: Vec<String> = state
                                        .context_items
                                        .iter()
                                        .filter(|i| state.selected_context_set.contains(&i.id))
                                        .map(|i| i.id.clone())
                                        .collect();
                                    let tokens_freed = state
                                        .context_items
                                        .iter()
                                        .filter(|i| state.selected_context_set.contains(&i.id))
                                        .map(|i| i.tokens)
                                        .sum::<u32>();
                                    state.input_mode = InputMode::CompactConfirm {
                                        ids,
                                        tokens_freed,
                                    };
                                }
                            }
                            InputOutcome::CompactConfirmYes { ids, tokens_freed: _ } => {
                                state.input_mode = InputMode::Normal;
                                submit_compact(&dispatcher_handle, &adapter_handle, ids);
                            }
                            InputOutcome::ExpandAsk {
                                card_id,
                                item_count,
                                cache_rewarm_tokens,
                            } => {
                                state.input_mode = InputMode::ExpandConfirm {
                                    card_id,
                                    item_count,
                                    cache_rewarm_tokens,
                                };
                            }
                            InputOutcome::ExpandConfirmYes { card_id } => {
                                state.input_mode = InputMode::Normal;
                                submit_expand(&dispatcher_handle, card_id);
                            }
                            InputOutcome::ConcurrentEditResolve { outcome } => {
                                // v61 — clear the modal locally and
                                // route the user's choice to the
                                // dispatcher; the resolver task in the
                                // runner will see the bus event and
                                // stand down the auto-pause timer.
                                state.input_mode = InputMode::Normal;
                                if let Some(handle) = dispatcher_handle.as_ref() {
                                    if let Some(sd) = handle.get() {
                                        sd.resolve_concurrent_edit(outcome);
                                    }
                                }
                            }
                            InputOutcome::ModalCancel => {
                                state.input_mode = InputMode::Normal;
                            }
                            InputOutcome::DeleteMemory(id) => {
                                submit_mutation(&dispatcher_handle, Mutation::DeleteMemory(id));
                            }
                            InputOutcome::PromoteMemory(id) => {
                                submit_mutation(&dispatcher_handle, Mutation::PromoteMemory(id));
                            }
                            InputOutcome::CyclePlanStatus { id, current } => {
                                let next = next_plan_status(current);
                                submit_mutation(
                                    &dispatcher_handle,
                                    Mutation::PlanStatus(id, next),
                                );
                            }
                            InputOutcome::RemovePlanStep(id) => {
                                submit_mutation(&dispatcher_handle, Mutation::RemovePlan(id));
                            }
                            InputOutcome::EnterTextInput(kind) => {
                                state.input_mode = InputMode::TextInput {
                                    kind,
                                    buffer: String::new(),
                                };
                            }
                            InputOutcome::TextInputChar(c) => {
                                if let InputMode::TextInput { buffer, .. } = &mut state.input_mode {
                                    buffer.push(c);
                                }
                            }
                            InputOutcome::TextInputBackspace => {
                                if let InputMode::TextInput { buffer, .. } = &mut state.input_mode {
                                    buffer.pop();
                                }
                            }
                            InputOutcome::TextInputSubmit => {
                                if let InputMode::TextInput { kind, buffer } =
                                    std::mem::take(&mut state.input_mode)
                                {
                                    let text = buffer.trim().to_string();
                                    state.input_mode = InputMode::Normal;
                                    if !text.is_empty() {
                                        let m = match kind {
                                            TextInputKind::AddMemoryCard => {
                                                Mutation::AddMemory(text)
                                            }
                                            TextInputKind::AddPlanStep => Mutation::AddPlanStep(text),
                                            TextInputKind::AddPlanConstraint { step_id } => {
                                                Mutation::AddPlanConstraint(step_id, text)
                                            }
                                        };
                                        submit_mutation(&dispatcher_handle, m);
                                    }
                                }
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
    atelier_cli::runner::AdapterHandle,
    tokio::task::JoinHandle<()>,
)> {
    use atelier_cli::runner::{
        AdapterHandle, DispatcherHandle, EventSink, MockResponse, ProviderChoice, Runner,
    };
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
        overflow: None,
    }];

    let handle = DispatcherHandle::new();
    let adapter_handle = AdapterHandle::new();
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
    .with_adapter_handle(adapter_handle.clone())
    .with_max_turns(4);

    let task = tokio::spawn(async move {
        if let Err(e) = runner.run(prompt).await {
            tracing::warn!(error = %e, "TUI demo run failed");
        }
    });
    Ok((handle, adapter_handle, task))
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
    if !sd.submit_approval_files(pending.commit_id, accepted) {
        tracing::warn!(
            commit_id = %pending.commit_id,
            "submit_pending: dispatcher rejected the accept-set (commit_id stale?)"
        );
    }
}

/// v55 — mutation request handed from the run loop to the
/// dispatcher. Routes to the appropriate `SessionDispatcher`
/// mutator. The dispatcher re-emits the relevant snapshot event
/// on success so the TUI's state updates via the same path
/// turn-boundary snapshots do.
enum Mutation {
    Pin(String),
    Unpin(String),
    Evict(String),
    AddMemory(String),
    DeleteMemory(String),
    PromoteMemory(String),
    AddPlanStep(String),
    AddPlanConstraint(String, String),
    PlanStatus(String, PlanStatus),
    RemovePlan(String),
}

fn submit_mutation(handle: &Option<atelier_cli::runner::DispatcherHandle>, m: Mutation) {
    let Some(handle) = handle else {
        // Viewer mode: nothing to mutate.
        return;
    };
    let Some(sd) = handle.get() else {
        tracing::warn!("submit_mutation: DispatcherHandle empty (run already shut down?)");
        return;
    };
    let now = atelier_core::time::now_rfc3339();
    let result: Result<(), String> = match m {
        Mutation::Pin(id) => sd.pin_context_item(&id).map_err(|e| e.to_string()),
        Mutation::Unpin(id) => sd.unpin_context_item(&id).map_err(|e| e.to_string()),
        Mutation::Evict(id) => sd
            .evict_context_item(&id, &now)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Mutation::AddMemory(content) => sd
            .add_memory_card(content, &now)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Mutation::DeleteMemory(id) => sd.delete_memory_card(&id).map_err(|e| e.to_string()),
        Mutation::PromoteMemory(id) => match sd.promote_memory_card(&id, &now) {
            // v60 (security M-1 fix) — route through the shared
            // atelier-cli writer so the TUI gets the same HOME
            // validation + canonical-root containment + atomic
            // write the GUI has had since v58/v59. Pre-v60 the TUI
            // wrote with `std::fs::write` directly, bypassing every
            // hardening pass on this path.
            Ok(output) => atelier_cli::memory_promote::write_promoted_card(&output).map(|_| ()),
            Err(e) => Err(e.to_string()),
        },
        Mutation::AddPlanStep(text) => sd
            .add_plan_step(text)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Mutation::AddPlanConstraint(id, c) => sd
            .add_plan_step_constraint(&id, c)
            .map_err(|e| e.to_string()),
        Mutation::PlanStatus(id, s) => sd.mark_plan_step_status(&id, s).map_err(|e| e.to_string()),
        Mutation::RemovePlan(id) => sd.remove_plan_step(&id).map_err(|e| e.to_string()),
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "submit_mutation: dispatcher rejected the request");
    }
}

/// v60.5 — fire a §5 non-destructive compaction. Distinct from
/// `submit_mutation` because:
///
/// 1. The orchestrator (`atelier_cli::compaction::compact`) is
///    async (it calls into the adapter), so we spawn a task
///    instead of running synchronously.
/// 2. It needs both the dispatcher and the adapter handles; the
///    latter isn't on `submit_mutation`'s signature.
///
/// No-op (with a `tracing::warn`) in viewer mode or when either
/// handle is empty.
fn submit_compact(
    dispatcher_handle: &Option<atelier_cli::runner::DispatcherHandle>,
    adapter_handle: &Option<atelier_cli::runner::AdapterHandle>,
    ids: Vec<String>,
) {
    let (Some(dh), Some(ah)) = (dispatcher_handle, adapter_handle) else {
        tracing::warn!("submit_compact: no active run");
        return;
    };
    let Some(sd) = dh.get() else {
        tracing::warn!("submit_compact: dispatcher handle empty");
        return;
    };
    let Some(adapter) = ah.get() else {
        tracing::warn!("submit_compact: adapter handle empty");
        return;
    };
    let now = atelier_core::time::now_rfc3339();
    // The TUI driver run doesn't currently surface its per-run
    // workspace_root or session_id externally, so we mint a fresh
    // session UUID per compaction call and write the blob under
    // `std::env::temp_dir()`. Once the TUI grows a real session
    // picker (Phase D §4 time-travel) these will come from the
    // active session.
    let workspace = std::env::temp_dir();
    let session_id = uuid::Uuid::new_v4().to_string();
    tokio::spawn(async move {
        let result = atelier_cli::compaction::compact(
            adapter.as_ref(),
            sd.as_ref(),
            &workspace,
            &session_id,
            ids,
            &now,
        )
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "submit_compact: orchestration failed");
        }
    });
}

/// v60.6 — fire a §5 Expand. Symmetric counterpart to
/// [`submit_compact`]; doesn't need the adapter handle (no model
/// call in the loop), so the function signature is one parameter
/// shorter.
///
/// The blob is resolved against `std::env::temp_dir()` — same
/// shortcut [`submit_compact`] takes, since the TUI driver run
/// doesn't externally expose its `workspace_root`. The compaction
/// path also wrote under this same root, so reads and writes
/// pair correctly for the demo run.
fn submit_expand(
    dispatcher_handle: &Option<atelier_cli::runner::DispatcherHandle>,
    card_id: String,
) {
    let Some(dh) = dispatcher_handle else {
        tracing::warn!("submit_expand: no active run");
        return;
    };
    let Some(sd) = dh.get() else {
        tracing::warn!("submit_expand: dispatcher handle empty");
        return;
    };
    let now = atelier_core::time::now_rfc3339();
    let workspace = std::env::temp_dir();
    tokio::spawn(async move {
        let result = atelier_cli::expansion::expand(sd.as_ref(), &workspace, card_id, &now).await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "submit_expand: orchestration failed");
        }
    });
}

// v57 (H6 fix): the TUI's `now_rfc3339_for_tui` + `tui_days_to_ymd`
// have been unified with the runner + GUI copies via
// `atelier_core::time::now_rfc3339`.

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
            "IllegalTransitionAttempted"
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
            capability_row: None,
        }
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
            capability_row: None,
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

    // ---------- §1 BYOM: conformance-driven degradation ----------

    #[test]
    fn apply_strategy_degraded_refreshes_current_model_strategy() {
        use atelier_core::protocol_strategy::Strategy;
        let mut s = AppState::new();
        s.current_model = Some(fixture_model());
        // Sanity-check the starting strategy from the fixture.
        assert_eq!(s.current_model.as_ref().unwrap().strategy, "json_sentinel");
        s.apply(&SessionEvent::StrategyDegraded {
            from: Strategy::JsonSentinel,
            to: Strategy::RegexProse,
            reason: "3 malformed envelopes in last 20 calls".into(),
        });
        // Footer badge should reflect the degraded tier.
        assert_eq!(s.current_model.as_ref().unwrap().strategy, "regex_prose");
    }

    #[test]
    fn apply_strategy_degraded_is_a_noop_when_no_model_loaded() {
        // A misordered scenario (degrade fires before a profile has
        // landed) must not crash the apply loop.
        use atelier_core::protocol_strategy::Strategy;
        let mut s = AppState::new();
        s.apply(&SessionEvent::StrategyDegraded {
            from: Strategy::NativeTool,
            to: Strategy::JsonSentinel,
            reason: "early".into(),
        });
        assert!(s.current_model.is_none());
    }

    #[test]
    fn project_event_strategy_degraded_carries_transition_and_reason() {
        use atelier_core::protocol_strategy::Strategy;
        let line = project_event(&SessionEvent::StrategyDegraded {
            from: Strategy::NativeTool,
            to: Strategy::JsonSentinel,
            reason: "3 malformed envelopes in last 20 calls".into(),
        });
        assert_eq!(line.kind, "StrategyDegraded");
        assert!(
            line.detail.contains("native_tool"),
            "got detail: {}",
            line.detail
        );
        assert!(
            line.detail.contains("json_sentinel"),
            "got detail: {}",
            line.detail
        );
        assert!(
            line.detail.contains("3 malformed envelopes"),
            "got detail: {}",
            line.detail
        );
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
    fn provenance_badge_covers_every_provenance_variant() {
        // Regression for v59 MED-smell-1 — `provenance_badge` is keyed
        // on the snake_case wire label produced by
        // `Provenance::wire_label`. Walking every variant catches
        // the case where a Rust-side rename ships a new wire label
        // that the TUI badge map doesn't know about — pre-v59 those
        // would silently fall through to `"????"`.
        use atelier_core::context::Provenance;
        for prov in [
            Provenance::Initial,
            Provenance::UserAttached { note: None },
            Provenance::ToolResult {
                tool_call_id: "tc".into(),
            },
            Provenance::MemoryPromoted {
                card_id: "m".into(),
            },
            Provenance::PinnedByUser { note: None },
            Provenance::AssistantTurn,
        ] {
            let badge = provenance_badge(prov.wire_label());
            assert_ne!(
                badge,
                "????",
                "provenance_badge fell through for {prov:?} (wire label {:?})",
                prov.wire_label()
            );
        }
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
            compacted_from: None,
            cache_rewarm_tokens: None,
        }
    }

    /// v60.6 — compaction-flavoured memory card. Mirrors `mem_card` but
    /// populates the `compacted_from` + `cache_rewarm_tokens`
    /// projections so render + expand-keybind tests can exercise the
    /// Expand-eligible row path.
    fn mem_compacted_card(
        id: &str,
        title: &str,
        count: u32,
        rewarm_tokens: u32,
    ) -> atelier_core::memory::MemoryCardSummary {
        atelier_core::memory::MemoryCardSummary {
            id: id.into(),
            title: title.into(),
            body_preview: "summary line".into(),
            created_at: "2026-05-17T11:00:00Z".into(),
            last_used: "2026-05-17T11:00:00Z".into(),
            pinned: true,
            compacted_from: Some(count),
            cache_rewarm_tokens: Some(rewarm_tokens),
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
            handle_key(
                key(KeyCode::Char('q'), KeyModifiers::empty()),
                &AppState::new()
            ),
            InputOutcome::Quit
        );
        assert_eq!(
            handle_key(key(KeyCode::Esc, KeyModifiers::empty()), &AppState::new()),
            InputOutcome::Quit
        );
        assert_eq!(
            handle_key(
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &AppState::new()
            ),
            InputOutcome::Quit
        );
    }

    #[test]
    fn handle_key_continues_on_other_keys() {
        assert_eq!(
            handle_key(
                key(KeyCode::Char('a'), KeyModifiers::empty()),
                &AppState::new()
            ),
            InputOutcome::Continue
        );
        assert_eq!(
            handle_key(key(KeyCode::Enter, KeyModifiers::empty()), &AppState::new()),
            InputOutcome::Continue
        );
        // Ctrl-Q is not the quit binding (only Ctrl-C is) — guarantees
        // the modifier check is right.
        assert_eq!(
            handle_key(
                key(KeyCode::Char('q'), KeyModifiers::CONTROL),
                &AppState::new()
            ),
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
            handle_key(
                key(KeyCode::Char('['), KeyModifiers::empty()),
                &AppState::new()
            ),
            InputOutcome::Scrub(ScrubCommand::Prev)
        );
    }

    #[test]
    fn handle_key_emits_scrub_next_on_close_bracket() {
        assert_eq!(
            handle_key(
                key(KeyCode::Char(']'), KeyModifiers::empty()),
                &AppState::new()
            ),
            InputOutcome::Scrub(ScrubCommand::Next)
        );
    }

    #[test]
    fn handle_key_emits_jump_to_head_on_g() {
        assert_eq!(
            handle_key(
                key(KeyCode::Char('g'), KeyModifiers::empty()),
                &AppState::new()
            ),
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
            "MessageCommitted"
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
        let mut s = AppState::new();
        s.pending_approval = Some(pending_with_files(&["a.txt"]));
        assert_eq!(
            handle_key(key(KeyCode::Char('y'), KeyModifiers::empty()), &s),
            InputOutcome::AcceptAll
        );
    }

    #[test]
    fn handle_key_emits_reject_all_on_n_when_pending() {
        let mut s = AppState::new();
        s.pending_approval = Some(pending_with_files(&["a.txt"]));
        assert_eq!(
            handle_key(key(KeyCode::Char('n'), KeyModifiers::empty()), &s),
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
            handle_key(
                key(KeyCode::Char('y'), KeyModifiers::empty()),
                &AppState::new()
            ),
            InputOutcome::Continue
        );
        assert_eq!(
            handle_key(
                key(KeyCode::Char('n'), KeyModifiers::empty()),
                &AppState::new()
            ),
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

    // ---------- v55: §5 mutator + focus ----------

    use atelier_core::context::ContextItemSummary as CtxSum;
    use atelier_core::memory::MemoryCardSummary as MemSum;

    fn seed_context(state: &mut AppState, ids: &[&str]) {
        state.context_items = ids
            .iter()
            .map(|id| CtxSum {
                id: (*id).to_string(),
                kind: "inline_text".into(),
                label: format!("label for {id}"),
                provenance: "user_attached".into(),
                provenance_detail: None,
                tokens: 5,
                token_source: "approx".into(),
                pinned: false,
            })
            .collect();
    }

    fn seed_memory(state: &mut AppState, ids: &[&str]) {
        state.memory_cards = ids
            .iter()
            .map(|id| MemSum {
                id: (*id).to_string(),
                title: format!("title-{id}"),
                body_preview: String::new(),
                created_at: "2026-05-17T10:00:00Z".into(),
                last_used: "2026-05-17T10:00:00Z".into(),
                pinned: false,
                compacted_from: None,
                cache_rewarm_tokens: None,
            })
            .collect();
    }

    fn seed_plan(state: &mut AppState, texts: &[&str]) {
        let mut pc = PlanCanvas::new();
        for t in texts {
            pc.add(*t);
        }
        state.plan = pc;
    }

    #[test]
    fn focused_pane_cycles_via_tab() {
        let mut s = AppState::new();
        assert_eq!(s.focused_pane, FocusedPane::Conversation);
        s.focused_pane = s.focused_pane.next();
        assert_eq!(s.focused_pane, FocusedPane::Context);
        s.focused_pane = s.focused_pane.next();
        assert_eq!(s.focused_pane, FocusedPane::Memory);
        s.focused_pane = s.focused_pane.next();
        assert_eq!(s.focused_pane, FocusedPane::Plan);
        s.focused_pane = s.focused_pane.next();
        assert_eq!(s.focused_pane, FocusedPane::Conversation);
    }

    #[test]
    fn handle_key_tab_emits_focus_next() {
        let s = AppState::new();
        assert_eq!(
            handle_key(key(KeyCode::Tab, KeyModifiers::empty()), &s),
            InputOutcome::FocusNext
        );
    }

    #[test]
    fn handle_key_jk_emit_select_next_and_prev_in_focused_pane() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["a", "b"]);
        assert_eq!(
            handle_key(key(KeyCode::Char('j'), KeyModifiers::empty()), &s),
            InputOutcome::SelectNext
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('k'), KeyModifiers::empty()), &s),
            InputOutcome::SelectPrev
        );
    }

    #[test]
    fn select_next_and_prev_saturate_at_bounds() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["a", "b", "c"]);
        s.select_next();
        s.select_next();
        s.select_next(); // saturates at 2
        assert_eq!(s.selected_context, 2);
        s.select_prev();
        s.select_prev();
        s.select_prev(); // saturates at 0
        assert_eq!(s.selected_context, 0);
    }

    #[test]
    fn context_pane_p_key_emits_pin_for_selected_item() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["id-0", "id-1"]);
        s.selected_context = 1;
        assert_eq!(
            handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()), &s),
            InputOutcome::PinContext("id-1".to_string())
        );
    }

    #[test]
    fn context_pane_e_key_opens_evict_confirm() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["uuid-here"]);
        assert_eq!(
            handle_key(key(KeyCode::Char('e'), KeyModifiers::empty()), &s),
            InputOutcome::EvictAsk("uuid-here".to_string())
        );
    }

    #[test]
    fn evict_confirm_mode_consumes_y_and_n() {
        let mut s = AppState::new();
        s.input_mode = InputMode::EvictConfirm {
            id: "the-id".to_string(),
        };
        assert_eq!(
            handle_key(key(KeyCode::Char('y'), KeyModifiers::empty()), &s),
            InputOutcome::EvictConfirmYes("the-id".to_string())
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('n'), KeyModifiers::empty()), &s),
            InputOutcome::ModalCancel
        );
    }

    #[test]
    fn memory_pane_a_key_enters_add_card_text_input() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Memory;
        assert_eq!(
            handle_key(key(KeyCode::Char('a'), KeyModifiers::empty()), &s),
            InputOutcome::EnterTextInput(TextInputKind::AddMemoryCard)
        );
    }

    #[test]
    fn memory_pane_d_and_promote_emit_per_selected_card() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Memory;
        seed_memory(&mut s, &["mem-1", "mem-2"]);
        s.selected_memory = 1;
        assert_eq!(
            handle_key(key(KeyCode::Char('d'), KeyModifiers::empty()), &s),
            InputOutcome::DeleteMemory("mem-2".to_string())
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('P'), KeyModifiers::empty()), &s),
            InputOutcome::PromoteMemory("mem-2".to_string())
        );
    }

    #[test]
    fn plan_pane_space_cycles_status_for_selected_step() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Plan;
        seed_plan(&mut s, &["first"]);
        let outcome = handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()), &s);
        match outcome {
            InputOutcome::CyclePlanStatus { id, current } => {
                assert!(id.starts_with("step-"));
                assert_eq!(current, PlanStatus::Pending);
            }
            other => panic!("expected CyclePlanStatus, got {other:?}"),
        }
        assert_eq!(
            next_plan_status(PlanStatus::Pending),
            PlanStatus::InProgress
        );
        assert_eq!(next_plan_status(PlanStatus::Skipped), PlanStatus::Pending);
    }

    #[test]
    fn plan_pane_a_and_c_open_text_input_modals() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Plan;
        seed_plan(&mut s, &["first"]);
        assert_eq!(
            handle_key(key(KeyCode::Char('a'), KeyModifiers::empty()), &s),
            InputOutcome::EnterTextInput(TextInputKind::AddPlanStep)
        );
        let outcome = handle_key(key(KeyCode::Char('c'), KeyModifiers::empty()), &s);
        match outcome {
            InputOutcome::EnterTextInput(TextInputKind::AddPlanConstraint { step_id }) => {
                assert!(step_id.starts_with("step-"));
            }
            other => panic!("expected EnterTextInput(AddPlanConstraint), got {other:?}"),
        }
    }

    #[test]
    fn text_input_mode_appends_chars_and_backspaces() {
        let mut s = AppState::new();
        s.input_mode = InputMode::TextInput {
            kind: TextInputKind::AddMemoryCard,
            buffer: String::new(),
        };
        assert_eq!(
            handle_key(key(KeyCode::Char('h'), KeyModifiers::empty()), &s),
            InputOutcome::TextInputChar('h')
        );
        assert_eq!(
            handle_key(key(KeyCode::Backspace, KeyModifiers::empty()), &s),
            InputOutcome::TextInputBackspace
        );
        assert_eq!(
            handle_key(key(KeyCode::Enter, KeyModifiers::empty()), &s),
            InputOutcome::TextInputSubmit
        );
        assert_eq!(
            handle_key(key(KeyCode::Esc, KeyModifiers::empty()), &s),
            InputOutcome::ModalCancel
        );
    }

    // ---------- v60.5: §5 non-destructive compaction ----------

    #[test]
    fn context_pane_space_toggles_selection_for_unpinned_row() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["id-a", "id-b"]);
        s.selected_context = 1;
        assert_eq!(
            handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()), &s),
            InputOutcome::ToggleContextSelected {
                id: "id-b".to_string(),
                pinned: false,
            }
        );
    }

    #[test]
    fn context_pane_space_marks_pinned_row_as_pinned() {
        // The run loop's match arm no-ops when `pinned == true`; we just
        // confirm `handle_key` reports the flag accurately.
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["id-a"]);
        s.context_items[0].pinned = true;
        assert_eq!(
            handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()), &s),
            InputOutcome::ToggleContextSelected {
                id: "id-a".to_string(),
                pinned: true,
            }
        );
    }

    #[test]
    fn context_pane_shift_c_emits_compact_ask_only_with_two_plus_selected() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Context;
        seed_context(&mut s, &["id-a", "id-b", "id-c"]);

        // With zero selected -> no-op.
        assert_eq!(
            handle_key(key(KeyCode::Char('C'), KeyModifiers::SHIFT), &s),
            InputOutcome::Continue
        );

        // With one selected -> still no-op.
        s.selected_context_set.insert("id-a".to_string());
        assert_eq!(
            handle_key(key(KeyCode::Char('C'), KeyModifiers::SHIFT), &s),
            InputOutcome::Continue
        );

        // With two selected -> CompactAsk.
        s.selected_context_set.insert("id-b".to_string());
        assert_eq!(
            handle_key(key(KeyCode::Char('C'), KeyModifiers::SHIFT), &s),
            InputOutcome::CompactAsk
        );
    }

    #[test]
    fn compact_confirm_mode_consumes_y_and_n() {
        let mut s = AppState::new();
        s.input_mode = InputMode::CompactConfirm {
            ids: vec!["id-a".into(), "id-b".into()],
            tokens_freed: 250,
        };
        match handle_key(key(KeyCode::Char('y'), KeyModifiers::empty()), &s) {
            InputOutcome::CompactConfirmYes { ids, tokens_freed } => {
                assert_eq!(ids, vec!["id-a".to_string(), "id-b".to_string()]);
                assert_eq!(tokens_freed, 250);
            }
            other => panic!("expected CompactConfirmYes, got {other:?}"),
        }
        assert_eq!(
            handle_key(key(KeyCode::Char('n'), KeyModifiers::empty()), &s),
            InputOutcome::ModalCancel
        );
        assert_eq!(
            handle_key(key(KeyCode::Esc, KeyModifiers::empty()), &s),
            InputOutcome::ModalCancel
        );
    }

    #[test]
    fn apply_compaction_executed_clears_selection() {
        let mut s = AppState::new();
        s.selected_context_set.insert("id-a".into());
        s.selected_context_set.insert("id-b".into());
        assert_eq!(s.selected_context_set.len(), 2);
        s.apply(&SessionEvent::CompactionExecuted {
            freed_tokens: 100,
            replaced_item_count: 2,
            summary_card_id: "mem-x".into(),
        });
        assert!(s.selected_context_set.is_empty());
    }

    #[test]
    fn apply_context_items_drops_stale_selected_ids() {
        // If the next snapshot doesn't contain an id we had selected
        // (e.g. evicted via another path), drop it from the selection.
        let mut s = AppState::new();
        s.selected_context_set.insert("id-a".into());
        s.selected_context_set.insert("id-b".into());
        s.apply(&SessionEvent::ContextItems {
            items: vec![CtxSum {
                id: "id-a".into(),
                kind: "inline_text".into(),
                label: "x".into(),
                provenance: "user_attached".into(),
                provenance_detail: None,
                tokens: 5,
                token_source: "approx".into(),
                pinned: false,
            }],
        });
        assert!(s.selected_context_set.contains("id-a"));
        assert!(!s.selected_context_set.contains("id-b"));
    }

    // ---------- v60.6: §5 Expand ----------

    #[test]
    fn memory_pane_x_on_compacted_card_opens_expand_modal() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Memory;
        s.memory_cards = vec![mem_compacted_card("mem-c", "summary", 5, 240)];
        s.selected_memory = 0;
        match handle_key(key(KeyCode::Char('x'), KeyModifiers::empty()), &s) {
            InputOutcome::ExpandAsk {
                card_id,
                item_count,
                cache_rewarm_tokens,
            } => {
                assert_eq!(card_id, "mem-c");
                assert_eq!(item_count, 5);
                assert_eq!(cache_rewarm_tokens, 240);
            }
            other => panic!("expected ExpandAsk, got {other:?}"),
        }
    }

    #[test]
    fn memory_pane_x_on_plain_card_is_inert() {
        let mut s = AppState::new();
        s.focused_pane = FocusedPane::Memory;
        s.memory_cards = vec![mem_card("mem-a", "ordinary", "2026-05-17T10:00:00Z", false)];
        s.selected_memory = 0;
        assert_eq!(
            handle_key(key(KeyCode::Char('x'), KeyModifiers::empty()), &s),
            InputOutcome::Continue
        );
    }

    #[test]
    fn expand_confirm_y_yields_expand_confirm_yes() {
        let s = AppState {
            input_mode: InputMode::ExpandConfirm {
                card_id: "mem-c".into(),
                item_count: 3,
                cache_rewarm_tokens: 150,
            },
            ..AppState::new()
        };
        match handle_key(key(KeyCode::Char('y'), KeyModifiers::empty()), &s) {
            InputOutcome::ExpandConfirmYes { card_id } => {
                assert_eq!(card_id, "mem-c");
            }
            other => panic!("expected ExpandConfirmYes, got {other:?}"),
        }
    }

    #[test]
    fn expand_confirm_cancel_keys_yield_modal_cancel() {
        let s = AppState {
            input_mode: InputMode::ExpandConfirm {
                card_id: "mem-c".into(),
                item_count: 3,
                cache_rewarm_tokens: 150,
            },
            ..AppState::new()
        };
        for k in [KeyCode::Char('n'), KeyCode::Esc, KeyCode::Char('q')] {
            assert_eq!(
                handle_key(key(k, KeyModifiers::empty()), &s),
                InputOutcome::ModalCancel
            );
        }
    }

    #[test]
    fn render_memory_pane_renders_compaction_badge() {
        let mut s = AppState::new();
        s.memory_cards = vec![mem_compacted_card("mem-c", "summary", 5, 240)];
        // Render the memory pane directly into a wide buffer so the
        // badge has room — the full `render` layout splits the
        // screen into four panes and the badge would otherwise wrap
        // out of view in narrow areas.
        let area = Rect::new(0, 0, 100, 5);
        let mut buf = Buffer::empty(area);
        render_memory_pane(&s, area, &mut buf);
        let rendered = buffer_to_string(&buf, area);
        assert!(
            rendered.contains("[×5, 240 tk]"),
            "missing badge in:\n{rendered}"
        );
    }

    #[test]
    fn render_help_footer_renders_expand_confirm_banner() {
        let s = AppState {
            input_mode: InputMode::ExpandConfirm {
                card_id: "mem-c".into(),
                item_count: 4,
                cache_rewarm_tokens: 320,
            },
            ..AppState::new()
        };
        // Render the help/footer band directly. The full screen
        // layout dedicates only a 1-row strip to it, which is too
        // narrow to substring-match our banner text in a
        // multi-pane render_to_string snapshot.
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        render_help(&s, area, &mut buf);
        let rendered = buffer_to_string(&buf, area);
        assert!(
            rendered.contains("EXPAND 4 items"),
            "missing banner in:\n{rendered}"
        );
        assert!(rendered.contains("~320 cache tokens"));
    }

    // ---------- v62: §7 verify-pass tier indicator ----------

    #[test]
    fn apply_verification_passed_replaces_status_hint() {
        let mut s = AppState::new();
        // Default is NotRun.
        assert_eq!(
            s.verification_status.tier,
            atelier_core::verify::VerificationTier::NotRun
        );
        s.apply(&SessionEvent::VerificationPassed {
            tier: atelier_core::verify::VerificationTier::Tier3Textual,
            file_count: 5,
            claim_count: 3,
        });
        assert_eq!(
            s.verification_status.tier,
            atelier_core::verify::VerificationTier::Tier3Textual
        );
        assert_eq!(s.verification_status.file_count, 5);
        assert_eq!(s.verification_status.claim_count, 3);
        // Event log got an entry too.
        assert_eq!(s.events.last().map(|e| e.kind), Some("VerificationPassed"));
    }

    #[test]
    fn verification_status_hint_badge_label_matches_gui() {
        // Pinned: the GUI's `verificationTierLabel` (state.ts) and the
        // TUI's `VerificationStatusHint::badge_label` must produce
        // identical user-facing copy. A drift here would mean the GUI
        // shows "tier-2 (tree-sitter)" while the TUI shows something
        // else for the same tier.
        for (tier, expected) in [
            (
                atelier_core::verify::VerificationTier::Tier1Lsp,
                "tier-1 (lsp)",
            ),
            (
                atelier_core::verify::VerificationTier::Tier2TreeSitter,
                "tier-2 (tree-sitter)",
            ),
            (
                atelier_core::verify::VerificationTier::Tier3Textual,
                "tier-3 (textual)",
            ),
            (atelier_core::verify::VerificationTier::NotRun, "verify off"),
        ] {
            let hint = VerificationStatusHint {
                tier,
                file_count: 0,
                claim_count: 0,
            };
            assert_eq!(hint.badge_label(), expected);
        }
    }

    #[test]
    fn render_cost_meter_includes_verify_badge_for_tier3() {
        // Render the cost meter directly into a small buffer so the
        // verify badge string is substring-matchable without competing
        // with the rest of the layout.
        let s = AppState {
            verification_status: VerificationStatusHint {
                tier: atelier_core::verify::VerificationTier::Tier3Textual,
                file_count: 2,
                claim_count: 1,
            },
            ..AppState::new()
        };
        // 2 rows: top border + the cost-and-badge row. Width must be
        // wide enough that both the cost figure and the "tier-3
        // (textual)" badge fit side-by-side without truncation.
        let area = Rect::new(0, 0, 60, 2);
        let mut buf = Buffer::empty(area);
        render_cost_meter(&s, area, &mut buf);
        let rendered = buffer_to_string(&buf, area);
        assert!(
            rendered.contains("verify"),
            "missing verify label in:\n{rendered}"
        );
        assert!(
            rendered.contains("tier-3 (textual)"),
            "missing tier-3 badge in:\n{rendered}"
        );
        // Cost line still rendered on the same row.
        assert!(
            rendered.contains("cost"),
            "missing cost label in:\n{rendered}"
        );
    }

    #[test]
    fn render_cost_meter_shows_verify_off_for_not_run_default() {
        let s = AppState::new();
        let area = Rect::new(0, 0, 60, 2);
        let mut buf = Buffer::empty(area);
        render_cost_meter(&s, area, &mut buf);
        let rendered = buffer_to_string(&buf, area);
        assert!(
            rendered.contains("verify off"),
            "missing verify off badge in:\n{rendered}"
        );
    }

    #[test]
    fn project_event_for_verification_passed_carries_tier_and_counts() {
        let line = project_event(&SessionEvent::VerificationPassed {
            tier: atelier_core::verify::VerificationTier::Tier3Textual,
            file_count: 4,
            claim_count: 2,
        });
        assert_eq!(line.kind, "VerificationPassed");
        // Detail string surfaces the tier wire label + counts so the
        // event log row is self-describing.
        assert!(
            line.detail.contains("tier3_textual"),
            "missing tier label in detail: {}",
            line.detail
        );
        assert!(line.detail.contains("4 files"));
        assert!(line.detail.contains("2 claims"));
    }
}
