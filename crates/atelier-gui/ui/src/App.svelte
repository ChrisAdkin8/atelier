<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'

  // Mirrors the `BridgedEvent` shape emitted by the Rust shell. Kept loose
  // until §3's typed envelope/diff schemas are surfaced to TypeScript via
  // the spec's eventual schema export.
  type BridgedEvent = {
    kind: string
    payload: unknown
  }

  let events: BridgedEvent[] = $state([])
  let editStagedCount = $state(0)
  let unlisten: UnlistenFn | null = null

  onMount(async () => {
    unlisten = await listen<BridgedEvent>('atelier://event', (e) => {
      events = [...events, e.payload].slice(-200)
      if (e.payload.kind === 'EditStaged') {
        editStagedCount += 1
      }
    })
  })

  onDestroy(() => {
    unlisten?.()
  })
</script>

<main>
  <header>
    <h1>Atelier</h1>
    <p class="subtitle">
      Phase&nbsp;C unblock&nbsp;(3) bootstrap. Subscribed to <code>atelier://event</code>;
      &nbsp;<strong>{editStagedCount}</strong> <code>EditStaged</code> events seen.
    </p>
  </header>

  <section class="event-log">
    <h2>Event bus</h2>
    {#if events.length === 0}
      <p class="empty">Waiting for events from the Rust shell. Start a session
        (the broadcast channel from <code>atelier-core::SessionHandle</code> is
        forwarded here via the <code>EventBridge</code> in
        <code>crates/atelier-gui/src/lib.rs</code>).</p>
    {:else}
      <ol>
        {#each events as ev, i (i)}
          <li>
            <span class="kind">{ev.kind}</span>
            <code>{JSON.stringify(ev.payload)}</code>
          </li>
        {/each}
      </ol>
    {/if}
  </section>
</main>

<style>
  :global(body) {
    margin: 0;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
    background: #1a1b1e;
    color: #e6e6e6;
  }
  main {
    max-width: 920px;
    margin: 0 auto;
    padding: 2rem 1.5rem;
  }
  header h1 {
    font-size: 1.6rem;
    margin: 0 0 0.25rem;
  }
  .subtitle {
    color: #9aa0a6;
    margin: 0 0 1.5rem;
    font-size: 0.9rem;
  }
  .event-log {
    border: 1px solid #2c2d31;
    border-radius: 8px;
    padding: 1rem 1.25rem;
    background: #101113;
  }
  .event-log h2 {
    font-size: 1rem;
    margin: 0 0 0.75rem;
    color: #9aa0a6;
    font-weight: 500;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .empty {
    color: #6b7280;
    font-size: 0.85rem;
    margin: 0;
  }
  ol {
    margin: 0;
    padding-left: 1.25rem;
    font-family: 'SF Mono', Menlo, Consolas, monospace;
    font-size: 0.8rem;
    line-height: 1.6;
  }
  .kind {
    display: inline-block;
    min-width: 8rem;
    color: #79c0ff;
  }
  code {
    color: #c9d1d9;
  }
</style>
