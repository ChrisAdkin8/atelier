<script lang="ts">
  import type { SubagentEntry } from '../state'

  type Props = {
    subagents: SubagentEntry[]
  }

  let { subagents }: Props = $props()

  function statusBadge(status: string): string {
    switch (status) {
      case 'running':
        return '[run ]'
      case 'completed':
        return '[done]'
      case 'failed':
        return '[fail]'
      case 'cancelled':
        return '[canc]'
      default:
        return '[    ]'
    }
  }

  function statusClass(status: string): string {
    switch (status) {
      case 'running':
        return 'running'
      case 'completed':
        return 'completed'
      case 'failed':
        return 'failed'
      case 'cancelled':
        return 'cancelled'
      default:
        return ''
    }
  }
</script>

<section class="subagent-pane">
  <h3 class="pane-title">Sub-agents</h3>
  {#if subagents.length === 0}
    <p class="empty">no sub-agents spawned</p>
  {:else}
    <ul class="sa-list">
      {#each subagents as sa (sa.id)}
        <li class="sa-row {statusClass(sa.status)}">
          <span class="badge">{statusBadge(sa.status)}</span>
          <span class="type">{sa.subagentType || '?'}</span>
          <span class="desc">"{sa.description}"</span>
          <span class="turns">turn {sa.turn}/{sa.maxTurns}</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .subagent-pane {
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
  .sa-list {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }
  .sa-row {
    display: grid;
    grid-template-columns: 6ch 8ch 1fr auto;
    gap: 0.4rem;
    padding: 0.15rem 0;
    font-size: 0.72rem;
    font-family: var(--font-mono);
    border-bottom: 1px solid var(--border-pane);
    align-items: baseline;
  }
  .sa-row:last-child {
    border-bottom: none;
  }
  .badge {
    color: var(--fg-dim);
    white-space: nowrap;
  }
  .sa-row.running .badge {
    color: var(--accent-cyan);
  }
  .sa-row.completed .badge {
    color: var(--accent-green, #5faf5f);
  }
  .sa-row.failed .badge {
    color: var(--accent-red);
  }
  .sa-row.cancelled .badge {
    color: var(--fg-dim);
  }
  .type {
    color: var(--fg-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .desc {
    color: var(--fg-default);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .turns {
    color: var(--fg-dim);
    white-space: nowrap;
    font-size: 0.68rem;
  }
</style>
