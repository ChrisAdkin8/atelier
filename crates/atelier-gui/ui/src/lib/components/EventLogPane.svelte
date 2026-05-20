<script lang="ts">
  import type { EventLogEntry } from '../state'

  type Props = {
    events: EventLogEntry[]
  }

  let { events }: Props = $props()

  // State stores newest-first so the latest event is visible without
  // cloning/reversing the full bounded log on every invalidation.
  let newestFirst = $derived(events)
</script>

<section class="event-log-pane">
  <h3 class="pane-title">Event Log</h3>
  {#if newestFirst.length === 0}
    <p class="empty">no events yet</p>
  {:else}
    <ul class="log-list">
      {#each newestFirst as entry (entry.ts + entry.kind + entry.detail)}
        <li class="log-row">
          <span class="ts">{entry.ts}</span>
          <span class="kind">{entry.kind}</span>
          {#if entry.detail}
            <span class="detail">{entry.detail}</span>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .event-log-pane {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    padding: 0.4rem 0.6rem;
    border: 1px solid var(--border-pane);
    border-radius: 4px;
    background: var(--bg-pane);
    overflow: hidden;
  }
  .pane-title {
    margin: 0 0 0.3rem 0;
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--fg-dim);
    font-family: var(--font-mono);
    flex-shrink: 0;
  }
  .empty {
    margin: 0;
    color: var(--fg-dim);
    font-size: 0.75rem;
    font-family: var(--font-mono);
    font-style: italic;
  }
  .log-list {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }
  .log-row {
    display: flex;
    gap: 0.5rem;
    padding: 0.1rem 0;
    font-size: 0.7rem;
    font-family: var(--font-mono);
    border-bottom: 1px solid var(--border-pane);
    align-items: baseline;
    min-width: 0;
  }
  .log-row:last-child {
    border-bottom: none;
  }
  .ts {
    color: var(--fg-dim);
    white-space: nowrap;
    flex-shrink: 0;
    opacity: 0.7;
  }
  .kind {
    color: var(--accent-cyan);
    white-space: nowrap;
    flex-shrink: 0;
  }
  .detail {
    color: var(--fg-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
</style>
