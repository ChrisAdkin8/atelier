<script lang="ts">
  // v61 — §14 concurrent-edit modal. Surfaced when the file-watcher
  // reports an external edit to a file in the agent's read-set. The
  // user picks Reload / Wait / Pause; the choice is routed to the
  // dispatcher via the `resolve_concurrent_edit` Tauri command.
  //
  // Mirrors the v60.5 CompactConfirm + v60.6 ExpandConfirm modal
  // patterns: small payload prop, three explicit action buttons, no
  // implicit "click outside to dismiss" affordance (the auto-pause
  // timer in the runner is the ultimate fallback, not the close box).

  import { invoke } from '@tauri-apps/api/core'
  import { onMount } from 'svelte'

  type Props = {
    paths: string[]
    observedAt: string
  }

  let { paths, observedAt }: Props = $props()

  let busy = $state(false)
  let error: string | null = $state(null)
  // v60.37 B3 — focus trap + Escape handler. The first action button
  // (Reload) takes focus on mount; Tab cycles between the three
  // buttons; Escape routes to the safer "Pause" arm (matches the
  // existing 5-min auto-pause fallback, so Escape == "I'm not ready
  // to decide yet").
  let reloadBtn: HTMLButtonElement | undefined = $state()
  let waitBtn: HTMLButtonElement | undefined = $state()
  let pauseBtn: HTMLButtonElement | undefined = $state()

  onMount(() => {
    reloadBtn?.focus()
  })

  async function resolve(choice: 'reload' | 'wait' | 'pause') {
    if (busy) return
    busy = true
    error = null
    try {
      const ok = await invoke<boolean>('resolve_concurrent_edit', { choice })
      if (!ok) {
        error = 'no active dispatcher — try again after a run starts'
      }
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }

  function onKey(e: KeyboardEvent) {
    if (busy) return
    if (e.key === 'Escape') {
      e.preventDefault()
      e.stopPropagation()
      resolve('pause')
      return
    }
    if (e.key === 'Tab') {
      const focused = document.activeElement
      const order = [reloadBtn, waitBtn, pauseBtn].filter(
        (b): b is HTMLButtonElement => b !== undefined,
      )
      if (order.length === 0) return
      const idx = order.indexOf(focused as HTMLButtonElement)
      if (idx === -1) {
        e.preventDefault()
        order[0]?.focus()
        return
      }
      const next = e.shiftKey
        ? (idx - 1 + order.length) % order.length
        : (idx + 1) % order.length
      e.preventDefault()
      order[next]?.focus()
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="modal-backdrop" role="dialog" aria-modal="true" aria-label="External edit detected">
  <section class="modal">
    <header>
      <h2>External edit detected</h2>
      <p class="observed-at">observed at {observedAt}</p>
    </header>

    <p class="lede">
      {paths.length} file{paths.length === 1 ? '' : 's'} in the agent's read-set
      changed on disk. Choose how the next tool call should react:
    </p>

    <ul class="path-list">
      {#each paths.slice(0, 10) as p}
        <li><code>{p}</code></li>
      {/each}
      {#if paths.length > 10}
        <li class="more">… and {paths.length - 10} more</li>
      {/if}
    </ul>

    <div class="actions">
      <button
        bind:this={reloadBtn}
        type="button"
        disabled={busy}
        onclick={() => resolve('reload')}
      >
        <strong>Reload</strong> — drop the queued call; re-read the files next turn
      </button>
      <button
        bind:this={waitBtn}
        type="button"
        disabled={busy}
        onclick={() => resolve('wait')}
      >
        <strong>Wait</strong> — keep the call queued; resolve when you say so
      </button>
      <button
        bind:this={pauseBtn}
        type="button"
        disabled={busy}
        onclick={() => resolve('pause')}
      >
        <strong>Pause</strong> — same as Wait; auto-Reload after 5 minutes
      </button>
    </div>

    {#if error}
      <p class="error">{error}</p>
    {/if}
  </section>
</div>

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    display: grid;
    place-items: center;
    z-index: 9999;
  }
  .modal {
    background: var(--bg, #1c1c1c);
    color: var(--fg, #eaeaea);
    border: 1px solid #b070d0;
    border-radius: 6px;
    padding: 1rem 1.25rem;
    max-width: 36rem;
    min-width: 24rem;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
  }
  .modal h2 {
    margin: 0;
    color: #d08fee;
    font-size: 1.1rem;
  }
  .observed-at {
    margin: 0.25rem 0 0.5rem;
    color: #888;
    font-size: 0.85rem;
    font-family: monospace;
  }
  .lede {
    margin: 0.5rem 0 0.75rem;
    color: var(--fg, #eaeaea);
  }
  .path-list {
    margin: 0 0 1rem;
    padding-left: 1.25rem;
    max-height: 8rem;
    overflow-y: auto;
    font-family: monospace;
    font-size: 0.85rem;
  }
  .path-list .more {
    list-style: none;
    color: #888;
    font-style: italic;
    margin-left: -1.25rem;
  }
  .actions {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .actions button {
    text-align: left;
    padding: 0.5rem 0.75rem;
    background: #2a2a2a;
    color: inherit;
    border: 1px solid #444;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.92rem;
  }
  .actions button:hover:not(:disabled) {
    background: #353535;
    border-color: #b070d0;
  }
  .actions button:disabled {
    opacity: 0.5;
    cursor: wait;
  }
  .actions strong {
    color: #d08fee;
    margin-right: 0.4rem;
  }
  .error {
    color: #ff7070;
    margin-top: 0.75rem;
    font-size: 0.9rem;
  }
</style>
