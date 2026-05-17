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
  import type { BridgedEvent } from './lib/state'
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
  import Composer from './lib/components/Composer.svelte'

  // NOTE (v49): named `app` rather than `state` because svelte-check
  // 4.x's TS-mode treats `let state = $state(...)` as the Svelte-3-era
  // store-auto-subscribe syntax (`$store`) and emits a spurious
  // "Block-scoped variable '$state' used before its declaration." A
  // future svelte-check version may fix this — if so, feel free to
  // rename, but verify `npm run check` stays clean.
  let app = $state(initialState())
  let unlisten: UnlistenFn | null = null

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
</script>

<div class="app">
  <Header
    currentState={app.currentState}
    editStagedCount={app.editStagedCount}
    scrubOffset={app.scrubOffset}
  />

  <main class="grid">
    <div class="pane-slot conversation-slot">
      <ConversationPane conversation={app.conversation} />
    </div>
    <div class="pane-slot plan-slot">
      <PlanPane planSteps={app.planSteps} />
    </div>
    <div class="pane-slot diff-slot">
      <DiffPane
        recentEdits={app.recentEdits}
        pendingApproval={app.pendingApproval}
      />
    </div>
    <div class="pane-slot meters-slot">
      <MetersPane
        totalCostUsd={app.totalCostUsd}
        knownTokens={app.contextTokens.known}
        unknownTokens={app.contextTokens.unknown}
        contextWindowTokens={app.contextWindowTokens}
      />
    </div>
  </main>

  <Composer busy={composerBusy} />

  <footer class="help">
    <span>[ prev</span>
    <span>] next</span>
    <span>g HEAD</span>
    {#if app.scrubOffset != null}
      <span class="hint">[pinned: g returns to HEAD]</span>
    {/if}
  </footer>
</div>

<style>
  .app {
    display: grid;
    /* Header / panes / Composer / help footer. */
    grid-template-rows: auto 1fr auto auto;
    height: 100vh;
    min-height: 0;
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
</style>
