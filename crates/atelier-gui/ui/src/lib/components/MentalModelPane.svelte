<script lang="ts">
  // Phase C close — §5 Mental Model panel.
  //
  // Off by default; toggled visible via the header button in
  // `App.svelte`. The text is user-editable; v60.20 **does** inject
  // it as a second System message on every per-turn `adapter.chat`
  // call when `enabled && text.trim() !== ''`. The cost-disclosure
  // label below renders `~N tokens / turn` when injection is live and
  // `0 tokens / turn` otherwise — `text_tokens` is the byte/4
  // approximation matching the runner's wire-side cost.
  //
  // Two-way binding via the `set_mental_model` Tauri command;
  // round-trips re-emit `MentalModelSnapshot` on the bus so other
  // listeners (the header toggle, future TUI / web clients) stay in
  // sync.

  import type { MentalModel } from '../state'
  import { invoke } from '@tauri-apps/api/core'

  interface Props {
    mentalModel: MentalModel
  }
  let { mentalModel }: Props = $props()

  // Local draft state — committed to the dispatcher on Save. We
  // intentionally don't bind the textarea to the snapshot directly
  // so concurrent re-emits from another driver don't yank the
  // user's input mid-type. Both `draft` and `lastSyncedText` start
  // empty and are populated by the `$effect` below as soon as the
  // initial snapshot arrives — avoiding the Svelte 5
  // `state_referenced_locally` warning that would fire if we read
  // `mentalModel.text` straight inside `$state(...)`.
  let draft: string = $state('')
  let lastSyncedText: string = $state('')
  let saving: boolean = $state(false)
  let toast: string | null = $state(null)
  let toastError: boolean = $state(false)

  // Re-sync the draft when an external snapshot arrives AND the user
  // hasn't started editing it locally. The "user has edits" check
  // compares against the last text we synced — that way a snapshot
  // arriving while the user types doesn't wipe their typing, but a
  // fresh-session re-emit (e.g. driver swap) does refresh the panel.
  $effect(() => {
    const incoming = mentalModel.text
    if (draft === lastSyncedText && draft !== incoming) {
      draft = incoming
      lastSyncedText = incoming
    }
  })

  let hasUnsavedEdits = $derived(draft !== mentalModel.text)

  async function save(newEnabled: boolean) {
    if (saving) return
    saving = true
    try {
      const updated = await invoke<MentalModel>('set_mental_model', {
        text: draft,
        enabled: newEnabled,
      })
      lastSyncedText = updated.text
      const injected = newEnabled && updated.text.trim() !== ''
      showToast(
        injected
          ? `saved — ~${updated.text_tokens} tokens / turn injected`
          : newEnabled
            ? 'saved (enabled, but text is empty — nothing injected)'
            : 'saved (disabled)',
        false,
      )
    } catch (e) {
      showToast(String(e), true)
    } finally {
      saving = false
    }
  }

  // Live cost-disclosure label. When the panel is enabled and the
  // text is non-empty, the runner injects on every per-turn chat
  // call, so the user pays ~text_tokens per turn. Otherwise 0.
  let perTurnCost = $derived(
    mentalModel.enabled && mentalModel.text.trim() !== ''
      ? `~${mentalModel.text_tokens} tokens / turn`
      : '0 tokens / turn',
  )
  let costBadgeTitle = $derived(
    mentalModel.enabled && mentalModel.text.trim() !== ''
      ? 'Text injected as a second System message on every adapter.chat call.'
      : 'Inactive — panel disabled or text is empty.',
  )

  async function toggleEnabled() {
    await save(!mentalModel.enabled)
  }

  async function commitEdits() {
    await save(mentalModel.enabled)
  }

  function showToast(msg: string, isError: boolean) {
    toast = msg
    toastError = isError
    setTimeout(() => {
      if (toast === msg) toast = null
    }, 3500)
  }
</script>

<section class="pane mental-model-pane">
  <header class="pane-title">
    <span>§5 Mental Model</span>
    <span class="pane-actions">
      <span class="cost-badge" title={costBadgeTitle}>
        {perTurnCost}
      </span>
      <button
        class="action"
        onclick={() => void toggleEnabled()}
        aria-pressed={mentalModel.enabled}
        title={mentalModel.enabled ? 'disable panel' : 'enable panel'}
      >
        {mentalModel.enabled ? 'on' : 'off'}
      </button>
    </span>
  </header>

  <div class="body">
    <textarea
      bind:value={draft}
      placeholder="free-form notes the agent should keep in mind…"
      aria-label="mental model text"
      rows="6"
    ></textarea>
    <div class="actions">
      <span class="info">
        {#if mentalModel.text_tokens > 0}
          ~{mentalModel.text_tokens} text tokens
        {/if}
        {#if mentalModel.updated_at}
          · updated {mentalModel.updated_at.slice(0, 16).replace('T', ' ')}
        {/if}
      </span>
      <button
        type="button"
        class="action save"
        disabled={saving || !hasUnsavedEdits}
        onclick={() => void commitEdits()}
      >
        {saving ? 'saving…' : hasUnsavedEdits ? 'save' : 'saved'}
      </button>
    </div>
    {#if toast}
      <p class="toast" class:toast-error={toastError}>{toast}</p>
    {/if}
  </div>
</section>

<style>
  .mental-model-pane {
    display: flex;
    flex-direction: column;
    background: var(--bg-pane);
    border: 1px solid var(--border-pane);
    border-radius: var(--radius-pane);
    overflow: hidden;
    min-height: 0;
  }
  .pane-title {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.6rem;
    padding: 0.35rem 0.6rem;
    border-bottom: 1px solid var(--border-pane);
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--fg-dim);
    letter-spacing: 0.05em;
  }
  .pane-actions {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
  }
  .cost-badge {
    color: var(--fg-dim);
    font-style: italic;
    font-size: 0.7rem;
  }
  .body {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.5rem 0.6rem;
  }
  textarea {
    background: var(--bg-input, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-pane);
    color: var(--fg-default, #ddd);
    border-radius: 3px;
    padding: 0.35rem 0.45rem;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    resize: vertical;
  }
  .actions {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
  }
  .info {
    color: var(--fg-dim);
    font-size: 0.7rem;
  }
  .action {
    background: transparent;
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    color: var(--fg-default, #ddd);
    cursor: pointer;
    font-family: inherit;
    font-size: 0.72rem;
    padding: 0.15rem 0.55rem;
  }
  .action:hover:not(:disabled) {
    background: var(--bg-hover, rgba(255, 255, 255, 0.06));
  }
  .action:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .action.save {
    color: #9c9;
    border-color: #4a6;
  }
  .toast {
    margin: 0;
    padding: 0.25rem 0.5rem;
    font-size: 0.72rem;
    color: var(--fg-dim);
    border-radius: 3px;
    background: rgba(0, 200, 100, 0.04);
  }
  .toast-error {
    color: #f88;
    background: rgba(200, 0, 0, 0.05);
  }
</style>
