<script lang="ts">
  // Conversation pane. Mirror of TUI `render_conversation`
  // (`crates/atelier-tui/src/lib.rs`):
  //   role-prefixed list, newest pinned at bottom, scrollable.
  //
  // The auto-scroll-to-bottom behaviour is the GUI's contribution over
  // the TUI tail-render: when a new message arrives, scroll the list
  // container so the freshest message stays visible.

  import type { ConversationLine } from '../state'
  import { roleColour } from '../state'

  type Props = {
    conversation: ConversationLine[]
  }

  let { conversation }: Props = $props()

  let scrollEl: HTMLDivElement | null = $state(null)

  // Re-pin to the bottom whenever a new line arrives. `length` is the
  // tracked reactive value; `$effect` re-runs whenever it changes.
  $effect(() => {
    const _len = conversation.length
    queueMicrotask(() => {
      if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight
    })
  })
</script>

<section class="pane">
  <header class="pane-title">Conversation</header>
  <div class="scroll" bind:this={scrollEl}>
    {#if conversation.length === 0}
      <p class="empty">no messages yet</p>
    {:else}
      <ol class="lines">
        {#each conversation as line, i (i)}
          <li>
            <span class="role" style="color: {roleColour(line.role)}">
              {line.role}
            </span>
            <span class="text">{line.text}</span>
          </li>
        {/each}
      </ol>
    {/if}
  </div>
</section>

<style>
  .pane {
    display: flex;
    flex-direction: column;
    background: var(--bg-pane);
    border: 1px solid var(--border-pane);
    border-radius: var(--radius-pane);
    overflow: hidden;
    min-height: 0;
  }
  .pane-title {
    padding: 0.4rem 0.75rem;
    background: var(--bg-pane-alt);
    border-bottom: 1px solid var(--border-pane);
    color: var(--fg-muted);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 0.7rem;
    font-weight: 600;
  }
  .scroll {
    flex: 1;
    overflow-y: auto;
    padding: 0.5rem 0.75rem;
    font-family: var(--font-mono);
    font-size: var(--fs-small);
    line-height: 1.55;
  }
  .empty {
    color: var(--fg-dim);
    margin: 0;
    font-style: italic;
  }
  .lines {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .lines li {
    display: grid;
    grid-template-columns: 5.5rem 1fr;
    gap: 0.5rem;
    padding: 0.1rem 0;
    align-items: baseline;
    word-break: break-word;
  }
  .role {
    font-weight: 600;
    text-align: right;
  }
  .text {
    color: var(--fg-default);
    white-space: pre-wrap;
  }
</style>
