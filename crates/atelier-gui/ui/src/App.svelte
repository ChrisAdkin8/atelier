<script lang="ts">
  // Atelier GUI shell — multi-pane workspace (spec §3 GUI mechanical gate).
  //
  // Layout mirrors the TUI subset (`crates/atelier-tui/src/lib.rs`):
  //
  //   +-- header ----------------------------------------------+
  //   | Conversation (60%)            | Plan (40%)             |
  //   +-------------------------------+------------------------+
  //   | Diff (60%)                    | Meters (40%)           |
  //   +-- footer (key hints, scrub) ---------------------------+
  //
  // This module is the only place the broadcast bus is subscribed; pane
  // components consume slices of `state` via typed props. The reducer
  // (`applyEvent` in `./lib/state`) is pure and parallels TUI `apply()`.

  import { onMount, onDestroy } from 'svelte'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { invoke } from '@tauri-apps/api/core'
  import type {
    BridgedEvent,
    CapabilityMatrixRow,
    CurrentModel,
    MentalModel,
  } from './lib/state'
  import {
    initialState,
    applyEvent,
    applyScrub,
    type ScrubCommand,
  } from './lib/state'
  import Header from './lib/components/Header.svelte'
  import ConversationPane from './lib/components/ConversationPane.svelte'
  import PlanPane from './lib/components/PlanPane.svelte'
  import ContextPane from './lib/components/ContextPane.svelte'
  import MemoryPane from './lib/components/MemoryPane.svelte'
  import MentalModelPane from './lib/components/MentalModelPane.svelte'
  import SubagentPane from './lib/components/SubagentPane.svelte'
  import EventLogPane from './lib/components/EventLogPane.svelte'
  import Composer from './lib/components/Composer.svelte'
  import ConcurrentEditModal from './lib/components/ConcurrentEditModal.svelte'
  import SwapConsentModal from './lib/components/SwapConsentModal.svelte'

  // NOTE (v49): named `app` rather than `state` because svelte-check
  // 4.x's TS-mode treats `let state = $state(...)` as the Svelte-3-era
  // store-auto-subscribe syntax (`$store`) and emits a spurious
  // "Block-scoped variable '$state' used before its declaration." A
  // future svelte-check version may fix this — if so, feel free to
  // rename, but verify `npm run check` stays clean.
  let app = $state(initialState())

  // v60.49 — right-column collapse toggle. Persists to localStorage
  // so the user's choice survives a relaunch. When collapsed, the
  // Conversation pane gets the full window width and the Plan / Memory
  // / Meters / Context panels are hidden via a class on `.grid`.
  let rightPanelCollapsed: boolean = $state(
    (() => {
      try {
        return localStorage.getItem('atelier:right-collapsed') === '1'
      } catch {
        return false
      }
    })(),
  )
  function toggleRightPanel() {
    rightPanelCollapsed = !rightPanelCollapsed
    try {
      localStorage.setItem(
        'atelier:right-collapsed',
        rightPanelCollapsed ? '1' : '0',
      )
    } catch {
      // localStorage disabled (private browsing, sandboxed webview, etc.);
      // toggle still works in-session, just doesn't persist.
    }
  }
  let unlisten: UnlistenFn | null = null

  // Phase C close — §5 mental-model panel visibility. Off by default;
  // the header toggle flips it. Pure UI state (not part of the
  // dispatcher's MentalModel) — a user can show the panel without
  // enabling mental-model injection, and vice versa.
  let showMentalModel: boolean = $state(false)

  // v49 FIX-9: the bus listener is awaited inside `onMount`, so the
  // Composer must be disabled until `listen()` resolves — otherwise a
  // fast user could click Send before we've subscribed, and the first
  // events of the run (MessageCommitted for the user prompt, possibly
  // even StagingPendingApproval) would be dropped.
  let listenerReady = $state(false)

  // A run is "in flight" between the first Transitioned-into-non-Idle
  // and the next time we land in a terminal state. Feeds into `composerBusy`
  // to disable the Composer's submit button during active turns.
  let runBusy = $derived(
    app.currentState !== '' &&
      app.currentState !== 'Idle' &&
      app.currentState !== 'Done' &&
      app.currentState !== 'Failed' &&
      app.currentState !== 'AwaitingUser',
  )
  // Composer is busy while either a run is in flight OR the bus
  // listener hasn't subscribed yet. `runInFlight` covers the dead zone
  // before the first Transitioned event (model probe, session setup).
  let composerBusy = $derived(!listenerReady || runBusy || app.runInFlight)

  onMount(async () => {
    unlisten = await listen<BridgedEvent>('atelier://event', (e) => {
      app = applyEvent(app, e.payload)
    })
    listenerReady = true
    window.addEventListener('keydown', onKeyDown)
    // v60.37 B5 — hydrate the swap dropdown from
    // `.atelier/providers.toml`. The Rust command falls back to the
    // built-in default list when no file is found, so this never
    // produces an empty array; on a malformed TOML it logs a `warn!`
    // on the backend and we get the defaults too. A network/IPC
    // failure leaves the inline single-mock fallback in place.
    try {
      const opts = await invoke<SwapOption[]>('list_provider_profiles')
      if (opts.length > 0) {
        swapOptions = opts
        // v60.55 — point the dropdown at the default row on hydration
        // so the visible selection matches the model the backend's
        // `resolve_default_adapter` would actually use. Without this,
        // the dropdown starts at index 0 (alphabetical first), which
        // can disagree with `providers.toml`'s `default = "<name>"`.
        const defaultIdx = opts.findIndex((o) => o.is_default)
        if (defaultIdx >= 0) dropdownIndex = defaultIdx
      }
    } catch (err) {
      console.warn('list_provider_profiles failed; keeping inline fallback', err)
    }
  })

  // Re-fetch provider profiles after the user sets a new workspace so
  // the dropdown reflects the new workspace's providers.toml without a reload.
  async function reloadProviderProfiles() {
    try {
      const opts = await invoke<SwapOption[]>('list_provider_profiles')
      if (opts.length > 0) {
        swapOptions = opts
        const defaultIdx = opts.findIndex((o) => o.is_default)
        if (defaultIdx >= 0) dropdownIndex = defaultIdx
      }
    } catch (err) {
      console.warn('list_provider_profiles reload failed', err)
    }
  }

  // Phase C close — hydrate the mental-model panel on demand. The
  // dispatcher seeds an empty `MentalModel` per session; we read it
  // the first time the user opens the panel so the textarea
  // reflects any prior value (e.g. from a swap-driver round-trip).
  async function hydrateMentalModel() {
    try {
      const m = await invoke<MentalModel>('snapshot_mental_model')
      app = { ...app, mentalModel: m }
    } catch {
      // No active dispatcher yet — the empty default is fine.
    }
  }

  async function toggleMentalModelPanel() {
    showMentalModel = !showMentalModel
    if (showMentalModel) await hydrateMentalModel()
  }

  onDestroy(() => {
    unlisten?.()
    window.removeEventListener('keydown', onKeyDown)
  })

  // Scrubber keys, mirror of the TUI bindings. `[` prev / `]` next /
  // `g` jump to HEAD. Phase D §4 will hook these into actual time
  // travel; v45 records intent the same way the TUI does so the
  // contract is established.
  function onKeyDown(e: KeyboardEvent) {
    // v60.37 B3/UI-2 — early-return when ANY modal is open. The modals
    // (ConcurrentEditModal, SwapConsentModal) attach their own keydown
    // handler and the scrub handler must not race them — pressing `g`
    // mid-modal would scrub the conversation behind the modal.
    if (app.concurrentEditModal || app.pendingSwap) return
    // Ignore when focus is in a Composer-style input. Limitation: this
    // check is by `tagName` only — a future component built from a
    // contenteditable div or a custom Svelte element would leak
    // `[`/`]`/`g` to the scrub handler. Add an `e.target.matches('[contenteditable]')`
    // check when that lands.
    const target = e.target as HTMLElement | null
    if (target && /^(INPUT|TEXTAREA)$/.test(target.tagName)) return
    let cmd: ScrubCommand | null = null
    if (e.key === '[') cmd = 'prev'
    else if (e.key === ']') cmd = 'next'
    else if (e.key === 'g') cmd = 'head'
    if (cmd) {
      app = applyScrub(app, cmd)
      e.preventDefault()
    }
  }

  // v60.7 §1 BYOM — tooltip and badge helpers for the footer's
  // model-id span. When the model carries a capability matrix row,
  // the tooltip lists each capability with its claim (and the row's
  // provenance — static / adapter / probe) so the user can audit
  // the §1 matrix without opening a separate panel. Falls back to
  // the baseUrl-only tooltip when no row is present (pre-v60.7
  // events, or an unidentified model the static table doesn't cover
  // *and* the adapter declined to declare capabilities).
  function modelBadgeTooltip(model: CurrentModel): string {
    const row = model.capabilityRow
    if (!row) return model.baseUrl
    const lines: string[] = []
    if (model.baseUrl) lines.push(model.baseUrl)
    if (row.display_label) lines.push(row.display_label)
    lines.push(`window: ${row.context_window_tokens.toLocaleString()} tokens`)
    lines.push(`native_tool_use: ${row.native_tool_use}`)
    lines.push(`streaming: ${row.streaming}`)
    lines.push(`vision: ${row.vision}`)
    lines.push(`prompt_cache: ${row.prompt_cache}`)
    lines.push(`structured_output: ${row.structured_output}`)
    lines.push(`long_context: ${row.long_context}`)
    lines.push(`source: ${row.source}`)
    return lines.join('\n')
  }

  // Returns a short "broken: <list>" label when any column on the
  // matrix row is `claimed_but_broken`, or `null` for a healthy
  // row. Mirrors the TUI's `capability_broken_label` so the two
  // drivers surface the same degradation hint.
  function capabilityBrokenLabel(row: CapabilityMatrixRow | null): string | null {
    if (!row) return null
    const broken: string[] = []
    if (row.native_tool_use === 'claimed_but_broken') broken.push('native_tool')
    if (row.streaming === 'claimed_but_broken') broken.push('streaming')
    if (row.vision === 'claimed_but_broken') broken.push('vision')
    if (row.prompt_cache === 'claimed_but_broken') broken.push('prompt_cache')
    if (row.structured_output === 'claimed_but_broken') broken.push('structured_output')
    if (row.long_context === 'claimed_but_broken') broken.push('long_context')
    return broken.length === 0 ? null : `broken: ${broken.join(', ')}`
  }

  // v60.10 B2 follow-on — footer provider swap dropdown.
  //
  // The list is hard-coded for now: one option per known adapter
  // family with a representative model id. A future cycle can hydrate
  // this from `.atelier/providers.toml` (the same source `Runner::new`
  // consults). The Tauri command surface is `swap_adapter(provider:
  // SwapProviderWire)`; the dropdown sends `{ kind, model_id }` and
  // the round-trip is recorded as a system `MessageCommitted` in the
  // conversation pane until the full B2 bundle (mid-run swap on the
  // Rust side) merges to main.
  // v60.37 B5 — `base_url` carried through so OpenAiCompat profiles
  // route their swap through the configured endpoint instead of the
  // server-side OPENAI_BASE_URL env fallback.
  type SwapOption = {
    kind: 'mock' | 'anthropic' | 'openai_compat'
    model_id: string
    label: string
    base_url?: string | null
    // v60.55 — `true` iff this row matches `providers.toml`'s
    // `default = "<name>"` selector (or, in the built-in fallback,
    // the row `Runner::new` would pick). The dropdown prefixes a
    // ★ glyph so the user can tell at a glance which model the
    // chat path falls back to.
    is_default?: boolean
  }
  // Hydrated on mount from the Rust-side `list_provider_profiles`
  // command, which reads `.atelier/providers.toml`. Until that
  // round-trip lands, the dropdown shows an inline fallback so the
  // first paint isn't blank.
  let swapOptions: SwapOption[] = $state([
    { kind: 'mock', model_id: 'mock:default', label: 'mock', is_default: true },
  ])

  // Pick the dropdown's selected option from the current model id when
  // possible; otherwise fall back to the first entry so the `<select>`
  // never renders a blank state.
  //
  // v60.38 L4/UI-7 — use a local `$state` for the dropdown index so the
  // user's selection is sticky across the swap-pending window. Before
  // this change, `selectedSwapIndex` was `$derived` from
  // `currentModel.modelId`, which meant the dropdown could briefly
  // snap back to the pre-swap value while the round-trip was in flight.
  let dropdownIndex: number = $state(0)
  // Keep the dropdown in sync with the model id on external updates
  // (initial load, AdapterSwapped event from another driver). When the
  // user picks an option, `onSwapChange` updates this state directly;
  // the effect then re-runs only when `currentModel.modelId` actually
  // changes, not on every render.
  $effect(() => {
    const id = app.currentModel?.modelId
    if (!id) {
      // v60.55 — when no model is active yet (first paint, pre-hydration,
      // or a torn-down session), point at the default row so the visible
      // selection matches what `resolve_default_adapter` would pick on
      // the first chat submit.
      const defaultIdx = swapOptions.findIndex((o) => o.is_default)
      dropdownIndex = defaultIdx >= 0 ? defaultIdx : 0
      return
    }
    const idx = swapOptions.findIndex((o) => o.model_id === id)
    if (idx >= 0 && idx !== dropdownIndex) {
      dropdownIndex = idx
    }
  })

  async function onSwapChange(e: Event) {
    const sel = e.currentTarget as HTMLSelectElement
    const idx = Number(sel.value)
    const opt = swapOptions[idx]
    if (!opt) return
    // v60.38 L4/UI-7 — pin the user's selection immediately. The
    // effect tied to `currentModel.modelId` will reconcile if the swap
    // succeeds; on rejection the AdapterSwapRejected event leaves
    // `currentModel` unchanged and the effect snaps the dropdown back.
    dropdownIndex = idx
    // v60.37 B5 — forward `base_url` for OpenAiCompat profiles so the
    // server-side allowlist gate (and the resulting adapter) routes to
    // the configured endpoint, not the OPENAI_BASE_URL env fallback.
    const provider: Record<string, string> = { kind: opt.kind, model_id: opt.model_id }
    if (opt.kind === 'openai_compat' && opt.base_url) {
      provider.base_url = opt.base_url
    }
    try {
      await invoke('swap_adapter', { provider })
    } catch (err) {
      // Surface failures via console; the system `MessageCommitted`
      // event the Rust side emits on success is the happy-path feedback.
      console.warn('swap_adapter failed', err)
    }
  }
</script>

<div class="app">
  <Header
    rightCollapsed={rightPanelCollapsed}
    onToggleRight={toggleRightPanel}
    onWorkspaceSet={reloadProviderProfiles}
  />

  <!-- Phase C close — mental-model panel toggle. Lives in its own
       row above the grid so the four-pane layout below is unchanged
       (off-by-default contract). -->
  <div class="mental-model-toggle">
    <button
      type="button"
      class="toggle-btn"
      onclick={() => void toggleMentalModelPanel()}
      aria-expanded={showMentalModel}
      aria-controls="mental-model-panel"
      title="show / hide the mental-model panel (off by default)"
    >
      Mental Model {showMentalModel ? '▾' : '▸'}
      {#if app.mentalModel.enabled}
        <span class="enabled-dot" aria-label="enabled">●</span>
      {/if}
    </button>
  </div>
  {#if showMentalModel}
    <div class="mental-model-row" id="mental-model-panel">
      <MentalModelPane mentalModel={app.mentalModel} />
    </div>
  {/if}

  <main class="grid" class:right-collapsed={rightPanelCollapsed}>
    <div class="pane-slot conversation-slot">
      <ConversationPane conversation={app.conversation} streamingAssistant={app.streamingAssistant} />
    </div>
    <div class="pane-slot plan-slot">
      <!-- v54: the top-right slot stacks the Plan canvas above
           the Memory panel. Plan reflects what the agent is about
           to do; Memory reflects what it remembers long-term. The
           two are upstream of every other §5 surface so they
           share the highest-visibility column. -->
      <div class="plan-stack" class:has-subagents={app.subagents.length > 0}>
        <PlanPane planSteps={app.planSteps} />
        <MemoryPane cards={app.memoryCards} />
        {#if app.subagents.length > 0}
          <SubagentPane subagents={app.subagents} />
        {/if}
      </div>
    </div>
    <div class="pane-slot context-slot">
      <div class="context-stack">
        <ContextPane items={app.contextItems} />
        <EventLogPane events={app.events} />
      </div>
    </div>
  </main>

  <Composer busy={composerBusy} thinking={app.runInFlight} />

  <footer class="help">
    <!-- Left column: context window usage + cost. -->
    <span class="footer-left">
      {#if app.contextWindowTokens > 0}
        {@const used = app.contextTokens.known}
        {@const cap = app.contextWindowTokens}
        {@const pct = Math.min(100, Math.round((used / cap) * 100))}
        <span class="ctx-meter" title="context window usage">
          <span class="ctx-label">ctx</span>
          <span class="ctx-bar"
            ><span class="ctx-fill" style="width: {pct}%"></span></span>
          <span class="ctx-text">{used.toLocaleString()} / {cap.toLocaleString()} ({pct}%)</span>
        </span>
      {/if}
      <span class="cost-meter" title="session cost (USD)">
        <span class="cost-label">cost</span>
        <span class="cost-value">${app.totalCostUsd.toFixed(4)}</span>
      </span>
    </span>

    <!-- Centre column: token meter. -->
    <span class="footer-center">
      <span class="tok-meter" title="last turn token counts (prompt / completion / cached)">
        <span class="tok-label">↑</span><span class="tok-value">{(app.lastTurnTokens?.prompt ?? 0).toLocaleString()}</span>
        <span class="tok-sep">·</span>
        <span class="tok-label">↓</span><span class="tok-value">{(app.lastTurnTokens?.completion ?? 0).toLocaleString()}</span>
        {#if (app.lastTurnTokens?.cached ?? 0) > 0}
          <span class="tok-sep">·</span>
          <span class="tok-label">cache</span><span class="tok-value cached">{(app.lastTurnTokens?.cached ?? 0).toLocaleString()}</span>
        {/if}
      </span>
    </span>

    <!-- Right column: provider swap dropdown only. -->
    <span class="footer-right">
      <select
        class="swap-select"
        value={String(dropdownIndex)}
        onchange={onSwapChange}
        disabled={app.pendingSwap != null}
        title={app.pendingSwap != null
          ? 'swap pending consent — respond to the modal first'
          : 'swap adapter (§1 BYOM)'}
        data-testid="swap-adapter-select"
      >
        {#each swapOptions as opt, i (opt.model_id)}
          <option value={String(i)}>
            {opt.is_default ? '★ ' : ''}{opt.label}
          </option>
        {/each}
      </select>
    </span>
  </footer>

  {#if app.stalledAt != null}
    <div class="stall-banner" role="alert">
      Model stalled — no tool calls or done signal. Type a follow-up or rephrase your request.
    </div>
  {/if}

  {#if app.concurrentEditModal}
    <ConcurrentEditModal
      paths={app.concurrentEditModal.paths}
      observedAt={app.concurrentEditModal.observedAt}
    />
  {/if}

  {#if app.pendingSwap}
    <SwapConsentModal
      swapId={app.pendingSwap.swapId}
      toModelId={app.pendingSwap.toModelId}
      baseUrl={app.pendingSwap.baseUrl}
    />
  {/if}
</div>

<style>
  .app {
    /* v60.44 — flexbox column instead of grid.
       Was: `display: grid; grid-template-rows: auto auto auto 1fr auto auto;`.
       The grid declared 6 row tracks but only 5 children render when the
       mental-model panel is hidden (its `{#if}` wrapper is absent, not
       zero-height). Grid auto-placement then shifted every child up by
       one track, assigning the `1fr` slot to the Composer instead of the
       main pane grid — so on full-screen the Composer textarea ballooned
       to fill the screen and the help footer kept the bottom edge.
       Flexbox sizes each child by content; only `.grid` claims `flex: 1`.
       The Composer always sits where it's written in the JSX. */
    display: flex;
    flex-direction: column;
    height: 100vh;
    min-height: 0;
  }
  .app > :global(.grid) {
    flex: 1;
    min-height: 0;
  }
  .stall-banner {
    position: fixed;
    bottom: 3rem;
    left: 50%;
    transform: translateX(-50%);
    padding: 0.4rem 1rem;
    background: var(--accent-amber, #c8a43a);
    color: #1a1209;
    font-size: 0.75rem;
    font-family: var(--font-mono);
    border-radius: 4px;
    z-index: 200;
    pointer-events: none;
  }
  .mental-model-toggle {
    display: flex;
    justify-content: flex-end;
    padding: 0.25rem 0.75rem 0 0.75rem;
  }
  .toggle-btn {
    background: transparent;
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    color: var(--fg-dim);
    font-family: var(--font-mono);
    font-size: 0.7rem;
    padding: 0.15rem 0.55rem;
    cursor: pointer;
  }
  .toggle-btn:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.06));
    color: var(--fg-default);
  }
  .toggle-btn .enabled-dot {
    margin-left: 0.3rem;
    color: var(--accent-green, #9c9);
    font-size: 0.6rem;
  }
  .mental-model-row {
    padding: 0.25rem 0.75rem 0 0.75rem;
    min-height: 0;
    display: flex;
  }
  .mental-model-row > :global(*) {
    flex: 1;
    min-width: 0;
  }
  .grid {
    display: grid;
    grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
    grid-template-rows: minmax(0, 1fr) minmax(0, 1fr);
    gap: var(--gap-pane);
    padding: var(--gap-pane);
    min-height: 0;
  }
  /* v60.49 — when collapsed, Conversation takes the full width and the
     right-column slots are pulled out of layout entirely (no reserved
     gutter). Selectors target the slot classes so other panels that
     might land in column 2 later inherit the same behaviour for free. */
  .grid.right-collapsed {
    grid-template-columns: minmax(0, 1fr);
  }
  .grid.right-collapsed .plan-slot,
  .grid.right-collapsed .context-slot {
    display: none;
  }
  .grid.right-collapsed .conversation-slot {
    grid-column: 1;
  }
  .pane-slot {
    min-width: 0;
    min-height: 0;
    display: flex;
  }
  .pane-slot > :global(*) {
    flex: 1;
    min-width: 0;
    /* v60.43 — without `min-height: 0`, an inner element with
       `overflow-y: auto` (ConversationPane's `.scroll`) can't shrink
       below its content height — i.e. it overflows the grid cell
       instead of scrolling. Setting min-height: 0 lets the cell's
       fixed height clamp the flex child so the inner scroll engages. */
    min-height: 0;
  }
  .conversation-slot {
    grid-row: 1 / span 2;
    grid-column: 1;
  }
  .plan-slot {
    grid-row: 1;
    grid-column: 2;
  }
  .context-slot {
    grid-row: 2;
    grid-column: 2;
  }
  /* v54 — Plan stays at the top (typically 4-8 short rows, so a
     soft `auto` height suits it); Memory takes the remaining
     vertical space because card counts can grow. */
  .plan-stack {
    display: grid;
    grid-template-rows: auto 1fr;
    gap: var(--gap-pane, 0.5rem);
    width: 100%;
    min-height: 0;
  }
  /* When sub-agents are running, add a third row capped at 10rem so
     the sub-agent panel doesn't crowd out Plan and Memory. */
  .plan-stack.has-subagents {
    grid-template-rows: auto 1fr minmax(5rem, 10rem);
  }
  .context-stack {
    display: grid;
    grid-template-rows: 1fr 1fr;
    gap: var(--gap-pane, 0.5rem);
    width: 100%;
    min-height: 0;
  }
  /* v60.50 — every direct child of `.help` inherits the same
     0.75rem font-size + line-height: 1 so the labels, values, and
     controls share a single typographic baseline. The earlier mix
     (0.65rem for tracked-caps labels, 0.7rem for the swap-select)
     drifted text vertically by ~1-2px. `align-items: center` plus a
     unified line-height fixes the v-align without per-element hacks. */
  .help {
    display: flex;
    align-items: center;
    padding: 0.35rem 1rem;
    border-top: 1px solid var(--border-pane);
    background: var(--bg-pane);
    color: var(--fg-dim);
    font-family: var(--font-mono);
    font-size: 0.75rem;
    line-height: 1;
  }
  .help * {
    font-size: inherit;
    line-height: inherit;
  }
  /* v60.43 — context-window usage meter. `margin-left: auto` pushes
     it to the right alongside the model badge so left-side scrub keys
     keep their stable position. */
  .help .ctx-meter {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--fg-dim);
  }
  .help .ctx-label {
    text-transform: uppercase;
    letter-spacing: 0.05em;
    opacity: 0.85;
  }
  .help .ctx-bar {
    position: relative;
    display: inline-block;
    width: 7rem;
    height: 0.55rem;
    background: var(--bg-pane-alt, rgba(255, 255, 255, 0.08));
    border: 1px solid var(--border-pane-strong, rgba(255, 255, 255, 0.18));
    border-radius: 2px;
    overflow: hidden;
  }
  .help .ctx-fill {
    position: absolute;
    inset: 0 auto 0 0;
    background: var(--accent-cyan, #6cc);
    transition: width 0.2s ease-out;
  }
  .help .ctx-text {
    font-variant-numeric: tabular-nums;
    color: var(--fg-default, var(--fg-dim));
  }
  /* v60.44 — cost meter, sibling of ctx-meter. No `margin-left:auto`
     here; the ctx-meter already claimed the auto-margin so cost sits
     immediately to its right via the parent flex `gap`. */
  .help .footer-left {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 1rem;
  }
  .help .footer-center {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .help .footer-right {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: flex-end;
  }
  .help .tok-meter {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    color: var(--fg-dim);
  }
  .help .tok-label {
    opacity: 0.7;
  }
  .help .tok-value {
    font-variant-numeric: tabular-nums;
    color: var(--fg-default);
  }
  .help .tok-value.cached {
    color: var(--accent-cyan);
  }
  .help .tok-sep {
    opacity: 0.4;
  }
  .help .cost-meter {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--fg-dim);
  }
  .help .cost-label {
    text-transform: uppercase;
    letter-spacing: 0.05em;
    opacity: 0.85;
  }
  .help .cost-value {
    font-variant-numeric: tabular-nums;
    color: var(--accent-green, #4ec9b0);
  }
  .help .swap-select {
    margin-left: 0;
    background: var(--bg-pane-alt);
    color: var(--fg-default);
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    font-family: var(--font-mono);
    /* v60.50 — sized via inherited 0.75rem from `.help *`; padding bumped
       slightly so the box height matches the adjacent text-only spans. */
    padding: 0.15rem 0.35rem;
  }
</style>
