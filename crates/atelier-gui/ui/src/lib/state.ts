// In-memory view state for the GUI shell. Mirror of the TUI's `AppState`
// shape (`crates/atelier-tui/src/lib.rs`) — the same fields, the same
// invariants, the same apply() reducer logic. The two surfaces share a
// data contract because they consume the same `atelier://event` broadcast
// stream.
//
// Pure functions only — no DOM, no Svelte runes. Components import this
// module and wrap the state in `$state` themselves. This split keeps the
// state-mutation logic exercisable in isolation if a vitest harness ever
// lands.

// ---- BridgedEvent shape from the Rust side ----
//
// The Rust shell projects `atelier_core::session::Event` onto the JSON
// shape below via `bridge_event` (see `crates/atelier-gui/src/lib.rs`).
// The frontend matches on `kind` and reads the per-variant `payload`.

export type BridgedEvent = {
  kind: string
  payload: unknown
}

// ---- Domain types — match the Rust producer side ----

export type ConversationRole = 'user' | 'assistant' | 'tool' | 'system'

export type ConversationLine = {
  role: ConversationRole
  text: string
}

// Mirror of `atelier_core::diff::Hunks`. Serde tag is `kind`.
export type Hunks =
  | { kind: 'same' }
  | { kind: 'binary' }
  | { kind: 'created'; new_byte_len: number; new_line_count: number }
  | { kind: 'deleted'; old_byte_len: number; old_line_count: number }
  | { kind: 'lines'; hunks: Hunk[] }

export type Hunk = {
  old_range: LineRange
  new_range: LineRange
  old_lines: string[]
  new_lines: string[]
}

export type LineRange = { start: number; end: number }

export type StagedEdit = {
  path: string
  hunks: Hunks
}

export type PlanStatus = 'pending' | 'in_progress' | 'done' | 'skipped'

export type PlanStep = {
  id: string
  text: string
  status: PlanStatus
  constraints?: string[]
}

// Mirror of `atelier_core::ledger::LedgerEntry`. Serde tag is `kind`.
export type LedgerEntry =
  | {
      kind: 'model_call'
      timestamp: string
      model_id: string
      cost_usd?: number | null
      tool_name?: string
      // Other fields exist on the wire but we only need cost + a label.
    }
  | {
      kind: 'tool_call'
      timestamp: string
      tool_name: string
      cost_usd?: number | null
    }
  | {
      kind: 'cache_bust'
      timestamp: string
      note: string
    }

// ---- AppState — the single source of truth for the UI ----

/// Default context-window denominator. Mirrors
/// `DEFAULT_CONTEXT_WINDOW_TOKENS` in atelier-tui (200k = Anthropic
/// Sonnet/Opus 4.x).
export const DEFAULT_CONTEXT_WINDOW_TOKENS = 200_000

/// Cap on remembered conversation lines. Matches the TUI's
/// `MAX_CONVERSATION_LINES`.
export const MAX_CONVERSATION_LINES = 1_000

/// Cap on remembered staged-edit history. Matches `MAX_DIFF_HISTORY`.
export const MAX_DIFF_HISTORY = 16

/// Cap on the raw event log (newest-last). Matches `MAX_EVENT_LOG`.
export const MAX_EVENT_LOG = 1_000

export type EventLogEntry = {
  kind: string
  detail: string
}

/// One pending file in a `StagingPendingApproval` event.
export type PendingApprovalFile = {
  path: string
  hunks: Hunks
}

/// Spec §3 pending hunk-approval state.
export type PendingApproval = {
  commitId: string
  files: PendingApprovalFile[]
}

/// v53 — one row in the §5 Context panel. Mirror of
/// `atelier_core::context::ContextItemSummary`; bridged across
/// `atelier://event` as the payload of the `ContextItems` event.
export type ContextItemSummary = {
  id: string
  kind: string
  label: string
  provenance: string
  provenance_detail?: string | null
  tokens: number
  /// `exact` / `approx` / `unavailable`.
  token_source: string
  pinned: boolean
}

/// v54 — one row in the §5 Memory panel. Mirror of
/// `atelier_core::memory::MemoryCardSummary`; bridged across
/// `atelier://event` as the payload of the `MemoryCards` event.
export type MemoryCardSummary = {
  id: string
  title: string
  body_preview: string
  /// RFC 3339.
  created_at: string
  /// RFC 3339.
  last_used: string
  pinned: boolean
}

/// v52 — the active BYOM model, populated by `ModelProfileLoaded`.
/// Rendered in the footer (`App.svelte`) on the right-hand side so the
/// user always knows which model + strategy the run is using.
/// `null` until the Runner emits its one-shot `ModelProfileLoaded`
/// event at session start.
export type CurrentModel = {
  /// `<provider>:<model>`, e.g. `local:qwen2.5-coder:7b`.
  modelId: string
  /// `http://localhost:11434/v1` etc. Empty for adapters that don't
  /// speak HTTP (mock / anthropic).
  baseUrl: string
  /// `native_tool` / `json_sentinel` / `regex_prose`. See
  /// `atelier_core::protocol_strategy::Strategy`.
  strategy: string
  /// `cache_hit` / `probed` / `reprobed` / `not_cached`. See
  /// `atelier_core::adapter::model_profile::ProbeLoadOutcome`.
  outcome: string
}

export type AppState = {
  events: EventLogEntry[]
  currentState: string
  editStagedCount: number
  conversation: ConversationLine[]
  recentEdits: StagedEdit[]
  planSteps: PlanStep[]
  totalCostUsd: number
  contextTokens: { known: number; unknown: number }
  contextWindowTokens: number
  // Scrub UI state. `null` = HEAD (live); `n` = pinned `n` steps back.
  // Phase D §4 owns the actual time-travel machinery; the GUI just
  // records intent the same way the TUI does.
  scrubOffset: number | null
  /// Outstanding hunk approval. `null` when no commit is awaiting
  /// user decision; populated by `StagingPendingApproval`, cleared by
  /// `CommitDecision`. See `DiffPane.svelte` for the buttons that
  /// invoke the `submit_approval` Tauri command.
  pendingApproval: PendingApproval | null
  /// v52 — active BYOM model. See [`CurrentModel`].
  currentModel: CurrentModel | null
  /// v53 — §5 Context panel rows. Replaced wholesale on each
  /// `ContextItems` event; populated by the Runner at every turn
  /// boundary alongside the aggregate `ContextSnapshot`.
  contextItems: ContextItemSummary[]
  /// v54 — §5 Memory panel rows. Replaced wholesale on each
  /// `MemoryCards` event; populated by the Runner alongside
  /// `ContextItems`. Distinct from context items: cards are
  /// durable across sessions; context items are per-turn.
  memoryCards: MemoryCardSummary[]
  /// v56 — per-file rationale from the envelope's `claimed_changes`,
  /// keyed by repo-relative path. The DiffPane renders this next to
  /// the file header so the user can see the agent's stated "why".
  /// Wholesale-replaced on each `ClaimedChanges` event.
  claimedChanges: Record<string, string>
}

export function initialState(): AppState {
  return {
    events: [],
    currentState: '',
    editStagedCount: 0,
    conversation: [],
    recentEdits: [],
    planSteps: [],
    totalCostUsd: 0,
    contextTokens: { known: 0, unknown: 0 },
    contextWindowTokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
    scrubOffset: null,
    pendingApproval: null,
    currentModel: null,
    contextItems: [],
    memoryCards: [],
    claimedChanges: {},
  }
}

// ---- applyEvent: pure reducer ----
//
// Returns a NEW state object (no in-place mutation) so Svelte's `$state`
// reactivity proxy can diff cleanly. Mirror of `AppState::apply` in
// `crates/atelier-tui/src/lib.rs`.

export function applyEvent(state: AppState, evt: BridgedEvent): AppState {
  const logged = projectEvent(evt)
  const events = pushBounded(state.events, logged, MAX_EVENT_LOG)

  switch (evt.kind) {
    case 'EditStaged': {
      const p = evt.payload as { path: string; hunks: Hunks }
      const edit: StagedEdit = { path: p.path, hunks: p.hunks }
      const recentEdits = [edit, ...state.recentEdits].slice(0, MAX_DIFF_HISTORY)
      return { ...state, events, editStagedCount: state.editStagedCount + 1, recentEdits }
    }
    case 'Transitioned': {
      const p = evt.payload as { from: string; to: string }
      return { ...state, events, currentState: p.to }
    }
    case 'MessageCommitted': {
      const p = evt.payload as { role: ConversationRole; text: string }
      // v58 (stale-comment fix) — the bridge now serialises Rust's
      // `MessageRole` via `MessageRole::wire_label()` (canonical
      // lowercase). The defensive coercion stays as a guard against a
      // future variant added Rust-side that the GUI doesn't yet know
      // about — surfaces as "system" rather than a blank prefix.
      const role: ConversationRole = isConversationRole(p.role) ? p.role : 'system'
      const line: ConversationLine = { role, text: p.text }
      const conversation = pushBounded(state.conversation, line, MAX_CONVERSATION_LINES)
      return { ...state, events, conversation }
    }
    case 'PlanSnapshot': {
      const p = evt.payload as { steps: PlanStep[] }
      return { ...state, events, planSteps: p.steps ?? [] }
    }
    case 'LedgerAppended': {
      const p = evt.payload as { entry: LedgerEntry }
      const c = ledgerEntryCost(p.entry)
      const totalCostUsd = c == null ? state.totalCostUsd : state.totalCostUsd + c
      return { ...state, events, totalCostUsd }
    }
    case 'ContextSnapshot': {
      const p = evt.payload as { known_tokens: number; unknown_tokens: number }
      return {
        ...state,
        events,
        contextTokens: { known: p.known_tokens, unknown: p.unknown_tokens },
      }
    }
    case 'StagingPendingApproval': {
      const p = evt.payload as { commit_id: string; files: { path: string; hunks: Hunks }[] }
      const pending: PendingApproval = {
        commitId: p.commit_id,
        files: (p.files ?? []).map((f) => ({ path: f.path, hunks: f.hunks })),
      }
      return { ...state, events, pendingApproval: pending }
    }
    case 'CommitDecision': {
      // The dispatcher resolved the pending — clear it. Per-file
      // EditStaged events for the committed paths arrive separately.
      return { ...state, events, pendingApproval: null }
    }
    case 'ModelProfileLoaded': {
      const p = evt.payload as {
        model_id: string
        base_url: string
        strategy: string
        outcome: string
      }
      const currentModel: CurrentModel = {
        modelId: p.model_id,
        baseUrl: p.base_url,
        strategy: p.strategy,
        outcome: p.outcome,
      }
      return { ...state, events, currentModel }
    }
    case 'ContextItems': {
      // v53 — replace the panel's items wholesale. Snapshots come
      // at every turn boundary, so a stale partial render is never
      // preferable to the freshest snapshot.
      const p = evt.payload as { items: ContextItemSummary[] }
      return { ...state, events, contextItems: p.items ?? [] }
    }
    case 'MemoryCards': {
      // v54 — same wholesale-replace policy as ContextItems.
      const p = evt.payload as { cards: MemoryCardSummary[] }
      return { ...state, events, memoryCards: p.cards ?? [] }
    }
    case 'ClaimedChanges': {
      // v56 — wholesale-replace the path→rationale map. The DiffPane
      // reads this to render the agent's "why this change?" summary
      // next to the file header.
      const p = evt.payload as {
        changes: { path: string; kind: string; summary: string }[]
      }
      const map: Record<string, string> = Object.create(null)
      for (const c of p.changes ?? []) {
        map[c.path] = c.summary
      }
      return { ...state, events, claimedChanges: map }
    }
    // Variants we don't fold into pane state — just the event log.
    case 'IllegalTransitionAttempted':
    case 'Cancelled':
    case 'Shutdown':
    default:
      return { ...state, events }
  }
}

// ---- Scrubber commands. Mirror of TUI ScrubCommand. ----

export type ScrubCommand = 'prev' | 'next' | 'head'

export function applyScrub(state: AppState, cmd: ScrubCommand): AppState {
  let next: number | null
  switch (cmd) {
    case 'head':
      next = null
      break
    case 'prev':
      next = (state.scrubOffset ?? 0) + 1
      break
    case 'next': {
      if (state.scrubOffset == null) {
        next = null
      } else {
        const n = state.scrubOffset - 1
        next = n <= 0 ? null : n
      }
      break
    }
  }
  return { ...state, scrubOffset: next }
}

// ---- helpers ----

function pushBounded<T>(arr: T[], item: T, cap: number): T[] {
  const next = [...arr, item]
  if (next.length <= cap) return next
  return next.slice(next.length - cap)
}

function ledgerEntryCost(entry: LedgerEntry): number | null {
  switch (entry.kind) {
    case 'model_call':
    case 'tool_call':
      return entry.cost_usd ?? null
    case 'cache_bust':
      // CacheBust entries carry no cost field — see TUI comment.
      return null
  }
}

function isConversationRole(s: string): s is ConversationRole {
  return s === 'user' || s === 'assistant' || s === 'tool' || s === 'system'
}

export function projectEvent(evt: BridgedEvent): EventLogEntry {
  // v58 (H7-residual + GUI label-drift fix) — `kind` comes from the
  // BridgedEvent's `kind` field, which is sourced from Rust's
  // `SessionEvent::kind()` (canonical variant names). Pre-v58 this
  // function synthesised its own short labels (Message,
  // PendingApproval, IllegalTransition, ModelProfile) which drifted
  // from the TUI projection by construction.
  const kind = evt.kind
  switch (evt.kind) {
    case 'MessageCommitted': {
      const p = evt.payload as { role: string; text: string }
      const firstLine = (p.text ?? '').split('\n')[0] ?? ''
      return { kind, detail: `${p.role}: ${firstLine.slice(0, 60)}` }
    }
    case 'PlanSnapshot': {
      const p = evt.payload as { steps: PlanStep[] }
      return { kind, detail: `${p.steps?.length ?? 0} steps` }
    }
    case 'LedgerAppended': {
      const p = evt.payload as { entry: LedgerEntry }
      const label = p.entry.kind === 'tool_call' ? `tool_call:${p.entry.tool_name}` : p.entry.kind
      return { kind, detail: label }
    }
    case 'ContextSnapshot': {
      const p = evt.payload as { known_tokens: number; unknown_tokens: number }
      return {
        kind,
        detail: `known=${p.known_tokens} unknown=${p.unknown_tokens}`,
      }
    }
    case 'StagingPendingApproval': {
      const p = evt.payload as { files: unknown[] }
      const n = Array.isArray(p.files) ? p.files.length : 0
      return { kind, detail: `${n} files awaiting approval` }
    }
    case 'CommitDecision': {
      const p = evt.payload as { committed: unknown[]; dropped: unknown[] }
      const c = Array.isArray(p.committed) ? p.committed.length : 0
      const d = Array.isArray(p.dropped) ? p.dropped.length : 0
      return { kind, detail: `committed=${c} dropped=${d}` }
    }
    case 'Transitioned': {
      const p = evt.payload as { from: string; to: string }
      return { kind, detail: `${p.from} → ${p.to}` }
    }
    case 'IllegalTransitionAttempted': {
      const p = evt.payload as { from: string; to: string }
      return { kind, detail: `${p.from} ↛ ${p.to}` }
    }
    case 'EditStaged': {
      const p = evt.payload as { path: string }
      return { kind, detail: p.path }
    }
    case 'ModelProfileLoaded': {
      const p = evt.payload as { model_id: string; strategy: string; outcome: string }
      return {
        kind,
        detail: `${p.model_id} · strategy=${p.strategy} · ${p.outcome}`,
      }
    }
    case 'ContextItems': {
      const p = evt.payload as { items?: unknown[] }
      const n = Array.isArray(p.items) ? p.items.length : 0
      return { kind, detail: `${n} items` }
    }
    case 'MemoryCards': {
      const p = evt.payload as { cards?: unknown[] }
      const n = Array.isArray(p.cards) ? p.cards.length : 0
      return { kind, detail: `${n} cards` }
    }
    case 'ClaimedChanges': {
      const p = evt.payload as { changes?: unknown[] }
      const n = Array.isArray(p.changes) ? p.changes.length : 0
      return { kind, detail: `${n} file rationale(s)` }
    }
    case 'Cancelled':
      return { kind, detail: '' }
    case 'Shutdown':
      return { kind, detail: '' }
    default:
      return { kind, detail: '' }
  }
}

// ---- role colour mapping ----
//
// Single source of truth so the conversation pane and any future
// transcript-export feature stay in sync. Matches TUI palette.

export function roleColour(role: ConversationRole): string {
  switch (role) {
    case 'user':
      return 'var(--role-user)'
    case 'assistant':
      return 'var(--role-assistant)'
    case 'tool':
      return 'var(--role-tool)'
    case 'system':
      return 'var(--role-system)'
  }
}
