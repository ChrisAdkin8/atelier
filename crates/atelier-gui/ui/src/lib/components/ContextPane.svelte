<script lang="ts">
  // v53 — §5 Context panel.
  //
  // One row per `ContextItemSummary` from the Rust producer, in
  // insertion order. Three pieces of information per row:
  //
  //   * `tokens` (right-aligned, fixed width) — the per-item token
  //     count. Colour-cued by `token_source`: cyan for `exact`,
  //     yellow for `approx`, dim grey for `unavailable` so the user
  //     can see at a glance how much to trust the number.
  //   * provenance badge — short label (`init`/`usr`/`tool`/`mem`/
  //     `pin`/`asst`) for the why-here trace. The full provenance
  //     string + optional detail (tool-call id, memory-card id,
  //     user note) lives in the row's `title` tooltip.
  //   * `label` — file path for `file_ref`, truncated first line
  //     for `inline_text`, sha256 prefix for `blob_ref`.
  //
  // Empty state ("no context items yet") is rendered explicitly so
  // an unstarted run is visibly idle rather than indistinguishable
  // from a broken pane.

  import type { ContextItemSummary } from '../state'

  interface Props {
    items: ContextItemSummary[]
  }
  let { items }: Props = $props()

  /// Map the snake_case provenance to a 4-char column-aligned label.
  /// Stable strings: the §5 mechanical gate asserts on them.
  function provenanceBadge(p: string): string {
    switch (p) {
      case 'initial':
        return 'init'
      case 'user_attached':
        return 'usr'
      case 'tool_result':
        return 'tool'
      case 'memory_promoted':
        return 'mem'
      case 'pinned_by_user':
        return 'pin'
      case 'assistant_turn':
        return 'asst'
      default:
        return '????'
    }
  }

  function tooltipFor(item: ContextItemSummary): string {
    const parts = [`kind: ${item.kind}`, `provenance: ${item.provenance}`]
    if (item.provenance_detail) parts.push(`detail: ${item.provenance_detail}`)
    parts.push(`tokens: ${item.tokens} (${item.token_source})`)
    if (item.pinned) parts.push('pinned')
    return parts.join('\n')
  }
</script>

<section class="pane context-pane">
  <header class="pane-title">§5 Context</header>

  {#if items.length === 0}
    <p class="empty">no context items yet</p>
  {:else}
    <ul class="rows">
      {#each items as item (item.id)}
        <li
          class="row"
          class:row-pinned={item.pinned}
          title={tooltipFor(item)}
        >
          <span class="tokens token-source-{item.token_source}">
            {item.tokens}
          </span>
          <span class="badge badge-{item.provenance}">
            {provenanceBadge(item.provenance)}
          </span>
          {#if item.pinned}
            <span class="pin" aria-label="pinned">📌</span>
          {/if}
          <span class="label">{item.label}</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .context-pane {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    background: var(--bg-pane);
    border: 1px solid var(--border-pane);
  }
  .pane-title {
    padding: 0.35rem 0.6rem;
    border-bottom: 1px solid var(--border-pane);
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--fg-dim);
    letter-spacing: 0.05em;
  }
  .empty {
    margin: 0;
    padding: 0.5rem 0.6rem;
    color: var(--fg-dim);
    font-style: italic;
    font-size: 0.8rem;
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 0.25rem 0;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }
  .row {
    display: grid;
    grid-template-columns: 4ch 5ch auto 1fr;
    gap: 0.45rem;
    align-items: baseline;
    padding: 0.15rem 0.6rem;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    line-height: 1.3;
  }
  .row:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.04));
  }
  .row-pinned {
    /* Pinned items get a faint accent so they're scannable. */
    background: rgba(255, 215, 0, 0.04);
  }
  .tokens {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  /* Token-source colour cues mirror the TUI's choices. */
  .token-source-exact {
    color: var(--accent-cyan, #6cc);
  }
  .token-source-approx {
    color: var(--accent-yellow, #cc9);
  }
  .token-source-unavailable {
    color: var(--fg-dim);
  }
  .badge {
    text-align: left;
    font-weight: 500;
  }
  /* Provenance colours mirror the TUI palette. */
  .badge-initial {
    color: var(--fg-dim);
  }
  .badge-user_attached {
    color: var(--accent-green, #9c9);
  }
  .badge-tool_result {
    color: var(--accent-magenta, #c9c);
  }
  .badge-memory_promoted {
    color: var(--accent-blue, #99c);
  }
  .badge-pinned_by_user {
    color: var(--accent-yellow, #cc9);
  }
  .badge-assistant_turn {
    color: var(--fg-default, #ddd);
  }
  .pin {
    font-size: 0.7rem;
  }
  .label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
