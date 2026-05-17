<script lang="ts">
  // Prompt composer. v47: kicks off a demo-scripted run via the
  // `start_demo_run` Tauri command. The run drives the dispatcher
  // through the AwaitApproval gate; the user sees the staging
  // banner in DiffPane, accepts or rejects, and watches the
  // resulting commit land.
  //
  // Cmd+Enter / Ctrl+Enter submits without taking the mouse off
  // the keyboard.

  import { invoke } from '@tauri-apps/api/core'

  type Props = {
    busy: boolean
  }

  let { busy }: Props = $props()

  let prompt: string = $state('')
  let error: string | null = $state(null)
  let starting: boolean = $state(false)

  async function start() {
    const trimmed = prompt.trim()
    if (!trimmed || busy || starting) return
    starting = true
    error = null
    try {
      await invoke('start_demo_run', { prompt: trimmed })
      prompt = ''
    } catch (e) {
      error = String(e)
    } finally {
      starting = false
    }
  }

  function onKey(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
      e.preventDefault()
      void start()
    }
  }

  let canSubmit = $derived(
    !busy && !starting && prompt.trim().length > 0,
  )
</script>

<section class="composer">
  <textarea
    placeholder={busy
      ? 'a run is in progress — wait for it to finish or scrub back'
      : 'type a prompt and hit Cmd+Enter (or click Send) to start a demo run'}
    bind:value={prompt}
    onkeydown={onKey}
    disabled={busy || starting}
    rows="2"
  ></textarea>
  <div class="composer-actions">
    <span class="hint">
      Cmd+Enter to send · scripted mock adapter · AwaitApproval policy
    </span>
    <button
      class="send"
      onclick={start}
      disabled={!canSubmit}
    >
      {starting ? 'starting…' : 'Send'}
    </button>
  </div>
  {#if error}
    <p class="error">{error}</p>
  {/if}
</section>

<style>
  .composer {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.6rem 1rem;
    background: var(--bg-pane);
    border-top: 1px solid var(--border-pane);
  }
  textarea {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    padding: 0.45rem 0.6rem;
    background: var(--bg-pane-alt);
    color: var(--fg-default);
    border: 1px solid var(--border-pane-strong);
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: 0.85rem;
    line-height: 1.4;
  }
  textarea:focus {
    outline: none;
    border-color: var(--accent-cyan);
  }
  textarea:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .composer-actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .hint {
    color: var(--fg-dim);
    font-size: 0.7rem;
    font-family: var(--font-mono);
  }
  .send {
    padding: 0.3rem 0.9rem;
    border-radius: 4px;
    border: 1px solid var(--accent-cyan);
    background: var(--accent-cyan);
    color: #062131;
    font-weight: 600;
    cursor: pointer;
  }
  .send:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .error {
    margin: 0;
    color: var(--accent-red);
    font-size: 0.75rem;
    font-family: var(--font-mono);
  }
</style>
