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
  import InlineRenderers from './InlineRenderers.svelte'
  import { onDestroy } from 'svelte'

  // Phase C close — only attempt inline rendering when the preview
  // contains a fence or an image-shaped URL; otherwise we stay on
  // the v54 lightweight `<p>` preview to keep the row compact.
  function previewHasRenderable(s: string): boolean {
    if (!s) return false
    if (s.includes('```mermaid') || s.includes('```d2')) return true
    return /\.(png|jpe?g|gif|svg|webp)\b/i.test(s)
  }

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

  // v60.6 — Expand confirm state. Open at most one row at a time;
  // confirm triggers `expand_memory_card`, which on success removes
  // the card via the next `MemoryCards` snapshot.
  let expandConfirmFor: string | null = $state(null)
  let expandInFlight: boolean = $state(false)

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

  function askExpand(id: string) {
    expandConfirmFor = id
  }

  function cancelExpand() {
    expandConfirmFor = null
  }

  async function confirmExpand(id: string) {
    if (expandInFlight) return
    expandInFlight = true
    try {
      const r = await invoke<{
        restored_item_count: number
        summary_card_id: string
        cache_rewarm_tokens: number
      }>('expand_memory_card', { id })
      showToast(
        `restored ${r.restored_item_count} items · ~${r.cache_rewarm_tokens} cache tokens re-warmed`,
        false,
      )
    } catch (e) {
      showToast(String(e), true)
    } finally {
      expandInFlight = false
      expandConfirmFor = null
    }
  }

  // v60.38 L3/UI-6 — capture each toast's timer so we can cancel on
  // unmount. Without this, a stale `toast = null` write can fire after
  // the component is gone.
  let toastTimer: ReturnType<typeof setTimeout> | null = null

  function showToast(msg: string, isError: boolean) {
    toast = msg
    toastError = isError
    if (toastTimer != null) clearTimeout(toastTimer)
    toastTimer = setTimeout(() => {
      if (toast === msg) toast = null
      toastTimer = null
    }, 4500)
  }

  onDestroy(() => {
    if (toastTimer != null) clearTimeout(toastTimer)
  })

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
  <header class="pane-title">Memory</header>

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
              {#if card.compacted_from}
                <button
                  class="action expand"
                  onclick={() => askExpand(card.id)}
                  disabled={expandInFlight && expandConfirmFor !== card.id}
                  title="expand: restore the {card.compacted_from} original context items from disk"
                  aria-label="expand {card.id}"
                >
                  ⤴ expand
                </button>
              {/if}
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
          {#if card.compacted_from}
            <p class="compaction-badge">
              compacted from {card.compacted_from} item{card.compacted_from === 1
                ? ''
                : 's'}{card.cache_rewarm_tokens != null
                ? ` · ~${card.cache_rewarm_tokens} tokens to re-warm`
                : ''}
            </p>
          {/if}
          {#if expandConfirmFor === card.id && card.compacted_from}
            <div class="expand-confirm" role="dialog" aria-label="confirm expand">
              <span class="expand-msg">
                Restore {card.compacted_from} items · pays ~{card.cache_rewarm_tokens ?? '?'}
                cache tokens to re-warm the prompt
              </span>
              <span class="expand-buttons">
                <button
                  class="action danger"
                  disabled={expandInFlight}
                  onclick={() => void confirmExpand(card.id)}
                >
                  {expandInFlight ? 'expanding…' : 'expand'}
                </button>
                <button
                  class="action"
                  disabled={expandInFlight}
                  onclick={cancelExpand}
                >
                  cancel
                </button>
              </span>
            </div>
          {/if}
          {#if card.body_preview}
            {#if previewHasRenderable(card.body_preview)}
              <div class="preview rich">
                <InlineRenderers
                  text={card.body_preview}
                  blockId={`mem-${card.id}`}
                />
              </div>
            {:else}
              <p class="preview">{card.body_preview}</p>
            {/if}
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
  /* v60.6 — Expand affordance + cost-disclosure badge. */
  .action.expand:hover:not(:disabled) {
    color: #9cf;
    border-color: #467;
  }
  .compaction-badge {
    margin: 0.2rem 0 0 0;
    color: var(--fg-dim);
    font-size: 0.7rem;
    font-style: italic;
    opacity: 0.85;
  }
  .expand-confirm {
    margin: 0.3rem 0 0 0;
    padding: 0.25rem 0.4rem;
    border: 1px solid #467;
    border-radius: 3px;
    background: rgba(120, 180, 220, 0.05);
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: center;
    font-size: 0.72rem;
  }
  .expand-msg {
    color: var(--fg-default, #ddd);
    flex: 1;
  }
  .expand-buttons {
    display: inline-flex;
    gap: 0.3rem;
  }
</style>
