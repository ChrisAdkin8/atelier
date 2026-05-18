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
  import DiffPane from './lib/components/DiffPane.svelte'
  import PlanPane from './lib/components/PlanPane.svelte'
  import MetersPane from './lib/components/MetersPane.svelte'
  import ContextPane from './lib/components/ContextPane.svelte'
  import MemoryPane from './lib/components/MemoryPane.svelte'
  import MentalModelPane from './lib/components/MentalModelPane.svelte'
  import Composer from './lib/components/Composer.svelte'
  import ConcurrentEditModal from './lib/components/ConcurrentEditModal.svelte'

  // NOTE (v49): named `app` rather than `state` because svelte-check
  // 4.x's TS-mode treats `let state = $state(...)` as the Svelte-3-era
  // store-auto-subscribe syntax (`$store`) and emits a spurious
  // "Block-scoped variable '$state' used before its declaration." A
  // future svelte-check version may fix this — if so, feel free to
  // rename, but verify `npm run check` stays clean.
  let app = $state(initialState())
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
  // and the next time we land in a terminal app. Used to disable
  // the Composer's submit button.
  let runBusy = $derived(
    app.currentState !== '' &&
      app.currentState !== 'Idle' &&
      app.currentState !== 'Done' &&
      app.currentState !== 'Failed',
  )
  // Composer is busy while either a run is in flight OR the bus
  // listener hasn't subscribed yet.
  let composerBusy = $derived(!listenerReady || runBusy)

  onMount(async () => {
    unlisten = await listen<BridgedEvent>('atelier://event', (e) => {
      app = applyEvent(app, e.payload)
    })
    listenerReady = true
    window.addEventListener('keydown', onKeyDown)
  })

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
  type SwapOption = { kind: 'mock' | 'anthropic' | 'openai_compat'; model_id: string; label: string }
  const swapOptions: SwapOption[] = [
    { kind: 'mock', model_id: 'mock:default', label: 'mock' },
    { kind: 'anthropic', model_id: 'anthropic:claude-opus-4-7', label: 'anthropic · claude-opus-4-7' },
    { kind: 'anthropic', model_id: 'anthropic:claude-sonnet-4-6', label: 'anthropic · claude-sonnet-4-6' },
    {
      kind: 'openai_compat',
      model_id: 'local:qwen2.5-coder:7b',
      label: 'openai-compat · local qwen2.5-coder:7b',
    },
  ]

  // Pick the dropdown's selected option from the current model id when
  // possible; otherwise fall back to the first entry so the `<select>`
  // never renders a blank state.
  let selectedSwapIndex = $derived.by(() => {
    const id = app.currentModel?.modelId
    if (!id) return 0
    const idx = swapOptions.findIndex((o) => o.model_id === id)
    return idx < 0 ? 0 : idx
  })

  async function onSwapChange(e: Event) {
    const sel = e.currentTarget as HTMLSelectElement
    const idx = Number(sel.value)
    const opt = swapOptions[idx]
    if (!opt) return
    try {
      await invoke('swap_adapter', {
        provider: { kind: opt.kind, model_id: opt.model_id },
      })
    } catch (err) {
      // Surface failures via console; the system `MessageCommitted`
      // event the Rust side emits on success is the happy-path feedback.
      console.warn('swap_adapter failed', err)
    }
  }
</script>

<div class="app">
  <Header
    currentState={app.currentState}
    editStagedCount={app.editStagedCount}
    scrubOffset={app.scrubOffset}
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
      title="show / hide the §5 mental-model panel (off by default)"
    >
      §5 mental model {showMentalModel ? '▾' : '▸'}
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

  <main class="grid">
    <div class="pane-slot conversation-slot">
      <ConversationPane conversation={app.conversation} />
    </div>
    <div class="pane-slot plan-slot">
      <!-- v54: the top-right slot stacks the Plan canvas above
           the Memory panel. Plan reflects what the agent is about
           to do; Memory reflects what it remembers long-term. The
           two are upstream of every other §5 surface so they
           share the highest-visibility column. -->
      <div class="plan-stack">
        <PlanPane planSteps={app.planSteps} />
        <MemoryPane cards={app.memoryCards} />
      </div>
    </div>
    <div class="pane-slot diff-slot">
      <DiffPane
        recentEdits={app.recentEdits}
        pendingApproval={app.pendingApproval}
        claimedChanges={app.claimedChanges}
      />
    </div>
    <div class="pane-slot meters-slot">
      <!-- v53: the bottom-right slot stacks the aggregate Meters
           (cost + context-window gauge) above the per-item Context
           panel. The Meters pane stays fixed-height; the Context
           pane fills the remaining vertical space because per-item
           rows are what scales as the agent's context grows. -->
      <div class="meters-stack">
        <MetersPane
          totalCostUsd={app.totalCostUsd}
          knownTokens={app.contextTokens.known}
          unknownTokens={app.contextTokens.unknown}
          contextWindowTokens={app.contextWindowTokens}
          verificationStatus={app.verificationStatus}
          lastOverflowResolution={app.lastOverflowResolution}
        />
        <ContextPane items={app.contextItems} />
      </div>
    </div>
  </main>

  <Composer busy={composerBusy} />

  <footer class="help">
    <!-- Left side: scrub keys. -->
    <span>[ prev</span>
    <span>] next</span>
    <span>g HEAD</span>
    {#if app.scrubOffset != null}
      <span class="hint">[pinned: g returns to HEAD]</span>
    {/if}

    <!-- v52 — active BYOM model on the right side. Empty until the
         Runner emits its one-shot `ModelProfileLoaded` event at session
         start; populated thereafter for the lifetime of the run.
         v60.7 — when the model carries a §1 capability matrix row
         (always set once the runner has wired the cross-walk), the
         `title=` tooltip lists each capability + its claim so the user
         can see at a glance which columns are broken.  Any
         `ClaimedButBroken` cell is surfaced inline as a yellow
         "broken: …" tag so a degraded model is unmissable. -->
    <span class="model-badge" aria-label="active model">
      {#if app.currentModel}
        <span class="model-id" title={modelBadgeTooltip(app.currentModel)}>
          {app.currentModel.modelId}
        </span>
        <span class="model-sep">·</span>
        <span class="model-strategy" title="§2 emission strategy">
          {app.currentModel.strategy}
        </span>
        <span class="model-sep">·</span>
        <span class="model-outcome" title="probe outcome">
          {app.currentModel.outcome}
        </span>
        {#if capabilityBrokenLabel(app.currentModel.capabilityRow)}
          <span class="model-sep">·</span>
          <span class="model-broken" title="§1 capability matrix · auto-degraded">
            {capabilityBrokenLabel(app.currentModel.capabilityRow)}
          </span>
        {/if}
      {:else}
        <span class="model-pending">no model</span>
      {/if}
      <!-- v60.10 B2 follow-on — provider swap dropdown. Stub UI;
           sends `{ kind, model_id }` to the `swap_adapter` Tauri
           command. The full mid-run swap behaviour lands when the
           v60.10 B2 bundle merges to main. -->
      <select
        class="swap-select"
        value={String(selectedSwapIndex)}
        onchange={onSwapChange}
        title="swap adapter (§1 BYOM)"
        data-testid="swap-adapter-select"
      >
        {#each swapOptions as opt, i (opt.model_id)}
          <option value={String(i)}>{opt.label}</option>
        {/each}
      </select>
    </span>
  </footer>

  {#if app.concurrentEditModal}
    <ConcurrentEditModal
      paths={app.concurrentEditModal.paths}
      observedAt={app.concurrentEditModal.observedAt}
    />
  {/if}
</div>

<style>
  .app {
    display: grid;
    /* Header / mental-model toggle / [optional mental-model panel]
       / main grid / Composer / help footer. The mental-model rows
       collapse to `auto` height so the layout stays compact when
       the panel is hidden. */
    grid-template-rows: auto auto auto 1fr auto auto;
    height: 100vh;
    min-height: 0;
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
  .pane-slot {
    min-width: 0;
    min-height: 0;
    display: flex;
  }
  .pane-slot > :global(*) {
    flex: 1;
    min-width: 0;
  }
  .conversation-slot {
    grid-row: 1;
    grid-column: 1;
  }
  .plan-slot {
    grid-row: 1;
    grid-column: 2;
  }
  .diff-slot {
    grid-row: 2;
    grid-column: 1;
  }
  .meters-slot {
    grid-row: 2;
    grid-column: 2;
  }
  /* v53 — Meters stays fixed-height (its content is two gauges);
     Context takes the remaining vertical space because the row
     count grows with the agent's working set. */
  .meters-stack {
    display: grid;
    grid-template-rows: auto 1fr;
    gap: var(--gap-pane, 0.5rem);
    width: 100%;
    min-height: 0;
  }
  .meters-stack > :global(:first-child) {
    flex: none;
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
  .help {
    display: flex;
    gap: 1rem;
    align-items: center;
    padding: 0.35rem 1rem;
    border-top: 1px solid var(--border-pane);
    background: var(--bg-pane);
    color: var(--fg-dim);
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }
  .help .hint {
    color: var(--accent-yellow);
  }
  /* v52 — push the model badge to the right edge of the footer.
     `margin-left: auto` is the canonical flexbox idiom for "all
     siblings hug the left; this one hugs the right." */
  .help .model-badge {
    margin-left: auto;
    display: inline-flex;
    align-items: baseline;
    gap: 0.35rem;
    color: var(--fg-default, var(--fg-dim));
  }
  .help .model-id {
    color: var(--accent-cyan, #6cc);
    font-weight: 500;
  }
  .help .model-strategy {
    color: var(--accent-green, #9c9);
  }
  .help .model-outcome {
    color: var(--fg-dim);
  }
  .help .model-sep {
    color: var(--fg-dim);
    opacity: 0.6;
  }
  .help .model-pending {
    color: var(--fg-dim);
    font-style: italic;
  }
  /* v60.7 — yellow "broken: …" suffix when the §1 capability
     matrix has any `claimed_but_broken` cell. Mirrors the TUI's
     yellow-bold styling for the same hint. */
  .help .model-broken {
    color: var(--accent-yellow, #cc6);
    font-weight: 600;
  }
  /* v60.10 B2 follow-on — provider swap dropdown. Minimal styling
     so the affordance reads as a control without overpowering the
     model badge text. */
  .help .swap-select {
    margin-left: 0.5rem;
    background: var(--bg-pane-alt);
    color: var(--fg-default);
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    font-family: var(--font-mono);
    font-size: 0.7rem;
    padding: 0.1rem 0.3rem;
  }
</style>
