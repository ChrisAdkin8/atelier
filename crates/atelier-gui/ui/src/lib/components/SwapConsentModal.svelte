<script lang="ts">
  // v60.28 H2 follow-on — adapter-swap consent modal. Opens when the
  // Rust `swap_adapter` command emits `AdapterSwapPending` after the
  // base_url allowlist gate passes. The user picks Accept / Reject;
  // the choice routes to the Rust side via the `respond_to_swap`
  // Tauri command keyed by `swapId`. `swap_adapter` is awaiting the
  // reply inside a `tokio::sync::oneshot` with a 120s timeout — a
  // closed modal without a reply trips the timeout and the swap is
  // refused.
  //
  // Pattern mirrors `ConcurrentEditModal`: backdrop + section + two
  // explicit action buttons + no implicit dismiss affordance (the
  // 120s timeout is the ultimate fallback, not a close box).

  import { invoke } from '@tauri-apps/api/core'
  import { onMount } from 'svelte'

  type Props = {
    swapId: string
    toModelId: string
    baseUrl: string
  }

  let { swapId, toModelId, baseUrl }: Props = $props()

  let busy = $state(false)
  let error: string | null = $state(null)
  // v60.37 B3 — focus trap + Escape handler. The first action button
  // receives focus on mount; Tab cycles between the two buttons; Escape
  // routes to the safer "Reject" arm (matching ConcurrentEditModal's
  // Escape-to-pause convention). The 120s timeout is still the ultimate
  // fallback; this just gives keyboard users a fast path to refuse.
  let acceptBtn: HTMLButtonElement | undefined = $state()
  let rejectBtn: HTMLButtonElement | undefined = $state()

  onMount(() => {
    acceptBtn?.focus()
  })

  async function respond(decision: 'accepted' | 'rejected') {
    if (busy) return
    busy = true
    error = null
    try {
      await invoke<void>('respond_to_swap', { swapId, decision })
    } catch (e) {
      // Most common failure: swap timed out (registry slot already
      // dropped). Show the error so the user knows the swap won't
      // proceed; the `AdapterSwapRejected` event will clear the
      // modal on its own arm.
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
      respond('rejected')
      return
    }
    // Tab-trap between the two action buttons. With only two focusable
    // elements there's no actual cycle path to manage; we just bounce
    // focus back to the other button if Tab tries to leave.
    if (e.key === 'Tab') {
      const focused = document.activeElement
      if (focused === acceptBtn && !e.shiftKey) {
        e.preventDefault()
        rejectBtn?.focus()
      } else if (focused === rejectBtn && e.shiftKey) {
        e.preventDefault()
        acceptBtn?.focus()
      } else if (focused !== acceptBtn && focused !== rejectBtn) {
        // Focus escaped — pull it back.
        e.preventDefault()
        acceptBtn?.focus()
      }
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div
  class="modal-backdrop"
  role="dialog"
  aria-modal="true"
  aria-label="Confirm adapter swap"
>
  <section class="modal">
    <header>
      <h2>Confirm adapter swap</h2>
      <p class="swap-id">id {swapId}</p>
    </header>

    <p class="lede">
      Swap the active provider to <code>{toModelId}</code>?
    </p>
    {#if baseUrl}
      <p class="meta">
        Endpoint: <code>{baseUrl}</code>
      </p>
    {/if}
    <p class="hint">
      Accepting tears down the current adapter and any cached probe / capability
      state. In-flight chat futures complete against the old adapter; the next
      turn uses the new one.
    </p>

    <div class="actions">
      <button
        bind:this={acceptBtn}
        type="button"
        disabled={busy}
        onclick={() => respond('accepted')}
      >
        <strong>Accept</strong> — swap in {toModelId}
      </button>
      <button
        bind:this={rejectBtn}
        type="button"
        disabled={busy}
        onclick={() => respond('rejected')}
      >
        <strong>Reject</strong> — keep the current adapter
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
  .swap-id {
    margin: 0.25rem 0 0.5rem;
    color: #888;
    font-size: 0.8rem;
    font-family: monospace;
  }
  .lede {
    margin: 0.5rem 0 0.5rem;
  }
  .meta {
    margin: 0 0 0.5rem;
    color: #aaa;
    font-size: 0.9rem;
  }
  .hint {
    margin: 0 0 0.9rem;
    color: #888;
    font-size: 0.85rem;
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
