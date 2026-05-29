<script lang="ts">
  import type { SubagentEntry } from '../state'

  type Props = {
    subagents: SubagentEntry[]
    currentGeneration: number
  }

  let { subagents, currentGeneration }: Props = $props()

  // §10 demote — sub-agents spawned before the current user turn collapse
  // into one expandable rollup; current-generation ones render in full.
  let prior = $derived(subagents.filter((s) => s.generation < currentGeneration))
  let current = $derived(subagents.filter((s) => s.generation >= currentGeneration))
  let priorPrompt = $derived(prior.reduce((n, s) => n + (s.promptTokens ?? 0), 0))
  let priorCompletion = $derived(
    prior.reduce((n, s) => n + (s.completionTokens ?? 0), 0),
  )

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

{#snippet saRow(sa: SubagentEntry)}
  <li class="sa-row {statusClass(sa.status)}">
    <span class="badge">{statusBadge(sa.status)}</span>
    <span class="type">{sa.subagentType || '?'}</span>
    <span class="desc">"{sa.description}"</span>
    <span class="meta">
      <span
        class="tokens"
        title={`prompt ${sa.promptTokens ?? 0}, completion ${sa.completionTokens ?? 0}, cached ${sa.cachedTokens ?? 0}`}
      >
        ↑{sa.promptTokens ?? 0} ↓{sa.completionTokens ?? 0}
      </span>
      <span class="turns">turn {sa.turn}/{sa.maxTurns}</span>
    </span>
  </li>
{/snippet}

<section class="subagent-pane">
  <h3 class="pane-title">Sub-agents</h3>
  {#if subagents.length === 0}
    <p class="empty">no sub-agents spawned</p>
  {:else}
    <ul class="sa-list">
      {#if prior.length > 0}
        <li class="sa-prior">
          <details>
            <summary>
              {prior.length} prior sub-agent{prior.length === 1 ? '' : 's'}
              <span class="tokens">↑{priorPrompt} ↓{priorCompletion}</span>
            </summary>
            <ul class="sa-sublist">
              {#each prior as sa (sa.id)}
                {@render saRow(sa)}
              {/each}
            </ul>
          </details>
        </li>
      {/if}
      {#each current as sa (sa.id)}
        {@render saRow(sa)}
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
    grid-template-columns: 6ch minmax(6ch, 8ch) minmax(0, 1fr) minmax(12rem, max-content);
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
  .meta {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 0.2rem 0.6rem;
    min-width: 0;
  }
  .tokens {
    color: var(--accent-cyan);
    white-space: nowrap;
    font-size: 0.68rem;
  }
  .sa-prior {
    border-bottom: 1px solid var(--border-pane);
  }
  .sa-prior summary {
    cursor: pointer;
    padding: 0.2rem 0;
    font-size: 0.7rem;
    font-family: var(--font-mono);
    color: var(--fg-dim);
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
  }
  .sa-prior summary .tokens {
    font-size: 0.66rem;
  }
  .sa-sublist {
    list-style: none;
    margin: 0;
    padding: 0;
    opacity: 0.6;
  }
</style>
