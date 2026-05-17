<script lang="ts">
  // v54 — §5 Memory panel.
  //
  // Per-card list of `MemoryCardSummary` from the Rust producer.
  // Distinct from the Context panel: cards here are durable
  // across sessions (long-term knowledge about the user / repo /
  // project conventions), whereas Context panel rows live for one
  // prompt-cache lifetime.
  //
  // Row shape:
  //   * pin glyph (📌) when the card is pinned — pinned cards
  //     survive eviction passes during compaction;
  //   * title (first non-empty line of the card body);
  //   * body preview (next ~200 chars, truncated with ellipsis);
  //   * relative "last used" badge on the right.
  //
  // Empty state ("no memory cards yet") is rendered explicitly so
  // a fresh session is visibly idle rather than indistinguishable
  // from a broken pane.

  import type { MemoryCardSummary } from '../state'
  import { invoke } from '@tauri-apps/api/core'

  interface Props {
    cards: MemoryCardSummary[]
  }
  let { cards }: Props = $props()

  // v55 — editable round-trips. The dispatcher mutator re-emits
  // `MemoryCards` on each successful op so we don't update local
  // state; we just wait for the snapshot to land. Promote is the
  // only one with a non-bool return — it writes to
  // `~/.atelier/memory/` and reports the resulting path.
  let draft: string = $state('')
  let toast: string | null = $state(null)
  let toastError: boolean = $state(false)

  async function add() {
    const content = draft.trim()
    if (!content) return
    try {
      await invoke<string>('add_memory_card', { content })
      draft = ''
    } catch (e) {
      showToast(String(e), true)
    }
  }

  async function deleteCard(id: string) {
    try {
      await invoke<null>('delete_memory_card', { id })
    } catch (e) {
      showToast(String(e), true)
    }
  }

  async function promote(id: string) {
    try {
      const r = await invoke<{ path: string; bytes: number }>(
        'promote_memory_card',
        { id },
      )
      showToast(`promoted → ${r.path} (${r.bytes} bytes)`, false)
    } catch (e) {
      showToast(String(e), true)
    }
  }

  function showToast(msg: string, isError: boolean) {
    toast = msg
    toastError = isError
    setTimeout(() => {
      if (toast === msg) toast = null
    }, 4500)
  }

  /// "2026-05-17T12:34:56Z" → "2026-05-17 12:34". The full
  /// timestamp is kept in the `title` tooltip; the badge is the
  /// compact form so it fits in the row even at narrow widths.
  function shortTimestamp(iso: string): string {
    if (!iso) return ''
    // Defensive: tolerate anything that isn't ISO 8601-ish.
    const m = iso.match(/^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2})/)
    return m ? `${m[1]} ${m[2]}` : iso
  }

  function tooltipFor(card: MemoryCardSummary): string {
    const parts = [`id: ${card.id}`]
    if (card.created_at) parts.push(`created: ${card.created_at}`)
    if (card.last_used) parts.push(`last used: ${card.last_used}`)
    if (card.pinned) parts.push('pinned')
    return parts.join('\n')
  }
</script>

<section class="pane memory-pane">
  <header class="pane-title">§5 Memory</header>

  <form class="composer" onsubmit={(e) => { e.preventDefault(); void add() }}>
    <textarea
      bind:value={draft}
      placeholder="add a memory card…"
      rows="2"
      aria-label="new memory card content"
    ></textarea>
    <button type="submit" disabled={!draft.trim()}>add</button>
  </form>

  {#if cards.length === 0}
    <p class="empty">no memory cards yet</p>
  {:else}
    <ul class="rows">
      {#each cards as card (card.id)}
        <li
          class="row"
          class:row-pinned={card.pinned}
          title={tooltipFor(card)}
        >
          <div class="row-head">
            {#if card.pinned}
              <span class="pin" aria-label="pinned">📌</span>
            {/if}
            <span class="title">{card.title || '(untitled)'}</span>
            <span class="when">{shortTimestamp(card.last_used)}</span>
            <span class="row-actions">
              <button
                class="action"
                onclick={() => void promote(card.id)}
                title="promote to ~/.atelier/memory/"
                aria-label="promote {card.id}"
              >
                ↑ promote
              </button>
              <button
                class="action danger"
                onclick={() => void deleteCard(card.id)}
                title="delete"
                aria-label="delete {card.id}"
              >
                ✕
              </button>
            </span>
          </div>
          {#if card.body_preview}
            <p class="preview">{card.body_preview}</p>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if toast}
    <p class="toast" class:toast-error={toastError}>{toast}</p>
  {/if}
</section>

<style>
  .memory-pane {
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
    padding: 0.35rem 0.6rem;
    border-bottom: 1px dotted var(--border-pane);
    font-family: var(--font-mono);
    font-size: 0.78rem;
    line-height: 1.35;
  }
  .row:last-child {
    border-bottom: none;
  }
  .row:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.04));
  }
  .row-pinned {
    /* Pinned cards get a subtle accent, mirroring ContextPane. */
    background: rgba(255, 215, 0, 0.04);
  }
  .row-head {
    display: grid;
    grid-template-columns: auto 1fr auto auto;
    gap: 0.4rem;
    align-items: baseline;
  }
  .composer {
    display: flex;
    gap: 0.4rem;
    align-items: stretch;
    padding: 0.35rem 0.6rem;
    border-bottom: 1px dotted var(--border-pane);
  }
  .composer textarea {
    flex: 1;
    resize: vertical;
    min-height: 1.6rem;
    background: var(--bg-input, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-pane);
    color: var(--fg-default, #ddd);
    border-radius: 3px;
    padding: 0.25rem 0.4rem;
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }
  .composer button {
    background: transparent;
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    color: var(--fg-default, #ddd);
    cursor: pointer;
    font-family: inherit;
    font-size: 0.75rem;
    padding: 0 0.7rem;
  }
  .composer button:hover:not(:disabled) {
    background: var(--bg-hover, rgba(255, 255, 255, 0.06));
  }
  .composer button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .row-actions {
    display: inline-flex;
    gap: 0.2rem;
    opacity: 0.4;
    transition: opacity 0.1s;
  }
  .row:hover .row-actions {
    opacity: 1;
  }
  .action {
    background: transparent;
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    color: var(--fg-default, #ddd);
    cursor: pointer;
    font-family: inherit;
    font-size: 0.7rem;
    padding: 0 0.3rem;
    line-height: 1.2;
  }
  .action:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.06));
  }
  .action.danger:hover {
    color: #f88;
    border-color: #844;
  }
  .toast {
    margin: 0;
    padding: 0.3rem 0.6rem;
    font-size: 0.72rem;
    color: var(--fg-dim);
    border-top: 1px dotted var(--border-pane);
    background: rgba(0, 200, 100, 0.04);
  }
  .toast-error {
    color: #f88;
    background: rgba(200, 0, 0, 0.05);
  }
  .pin {
    font-size: 0.7rem;
  }
  .title {
    color: var(--fg-default, #ddd);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .when {
    color: var(--fg-dim);
    font-variant-numeric: tabular-nums;
    font-size: 0.7rem;
  }
  .preview {
    margin: 0.2rem 0 0 0;
    color: var(--fg-dim);
    /* Two-line clamp — keeps the panel scannable while still showing
       enough of the card body to be useful. `line-clamp` is the
       standard; `-webkit-line-clamp` is the established alias
       browsers ship today. Specifying both quiets svelte-check
       and remains compatible everywhere. */
    display: -webkit-box;
    line-clamp: 2;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
</style>
