<script lang="ts">
  // Conversation pane. Mirror of TUI `render_conversation`
  // (`crates/atelier-tui/src/lib.rs`):
  //   role-prefixed list, newest pinned at bottom, scrollable.
  //
  // The auto-scroll-to-bottom behaviour is the GUI's contribution over
  // the TUI tail-render: when a new message arrives, scroll the list
  // container so the freshest message stays visible.
  //
  // L2-2 — failure_card conversation lines render as bordered cards with
  // Retry and Switch profile buttons.

  import { invoke } from '@tauri-apps/api/core'
  import type { ConversationLine } from '../state'
  import { roleColour } from '../state'

  type Props = {
    conversation: ConversationLine[]
    streamingAssistant?: string
    busy?: boolean
    onSwitchProfile?: () => void
  }

  let { conversation, streamingAssistant = '', busy = false, onSwitchProfile }: Props = $props()

  let scrollEl: HTMLDivElement | null = $state(null)
  let scrollFrame: number | null = null
  let retrying: boolean = $state(false)

  // Re-pin to the bottom only when the user is already following the tail.
  // Streaming deltas can arrive quickly, so coalesce writes through rAF to
  // avoid forcing layout once per token.
  $effect(() => {
    const _len = conversation.length
    const _stream = streamingAssistant
    if (!scrollEl) return
    const distanceFromBottom = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight
    if (distanceFromBottom > 96) return
    if (scrollFrame !== null) cancelAnimationFrame(scrollFrame)
    scrollFrame = requestAnimationFrame(() => {
      if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight
      scrollFrame = null
    })
  })

  // F3: allowlist guard — the command string comes from a Tauri bus event
  // payload; validate before invoking so a wire-drift or crafted event can't
  // call an arbitrary registered command. Console-warn (not silent return) so
  // this class of drift is visible in DevTools, per the v60.100 Q7 precedent.
  const RETRY_COMMANDS = new Set(['start_chat_run', 'start_agent_run'])

  async function retryCard(command: string, prompt: string) {
    if (busy || retrying) return
    if (!RETRY_COMMANDS.has(command)) {
      console.warn(`[atelier] refusing retry with unknown command: ${command}`)
      return
    }
    retrying = true
    try {
      await invoke(command, { prompt })
    } catch (e) {
      console.warn('retry failed', e)
    } finally {
      retrying = false
    }
  }
</script>

<section class="pane">
  <header class="pane-title">Conversation</header>
  <div class="scroll" bind:this={scrollEl}>
    {#if conversation.length === 0 && streamingAssistant.length === 0}
      <p class="empty">no messages yet</p>
    {:else}
      <ol class="lines">
        {#each conversation as line, i (i)}
          {#if line.kind === 'failure_card'}
            <li class="failure-card-item">
              <div class="failure-card">
                <div class="failure-title">{line.title}</div>
                <div class="failure-message">{line.message}</div>
                {#if line.fixHint}
                  <div class="failure-hint">{line.fixHint}</div>
                {/if}
                {#if line.memoryCardPath}
                  <div class="failure-footnote">saved to memory — see MemoryPane</div>
                {/if}
                <div class="failure-actions">
                  <button
                    class="failure-btn retry-btn"
                    disabled={busy || retrying}
                    onclick={() => retryCard(line.retryCommand, line.retryPrompt)}
                  >
                    {retrying ? 'retrying…' : 'Retry'}
                  </button>
                  <button
                    class="failure-btn switch-btn"
                    onclick={() => onSwitchProfile?.()}
                  >
                    Switch profile
                  </button>
                </div>
              </div>
            </li>
          {:else}
            <li>
              <span class="role" style="color: {roleColour(line.role)}">
                {line.role}
              </span>
              <span class="text">{line.text}</span>
            </li>
          {/if}
        {/each}
        {#if streamingAssistant.length > 0}
          <li class="streaming">
            <span class="role" style="color: {roleColour('assistant')}">assistant</span>
            <span class="text streaming-text">{streamingAssistant}<span class="cursor">▍</span></span>
          </li>
        {/if}
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
  .cursor {
    display: inline-block;
    animation: blink 1s step-end infinite;
    color: var(--fg-muted);
  }
  @keyframes blink {
    0%, 100% { opacity: 1; }
    50%       { opacity: 0; }
  }

  /* L2-2 — failure card */
  .failure-card-item {
    display: block;
    grid-column: 1 / -1;
    padding: 0.35rem 0;
  }
  .failure-card {
    border: 1px solid color-mix(in srgb, var(--accent-red, #e06c75) 65%, var(--border-pane));
    border-radius: 6px;
    background: color-mix(in srgb, var(--accent-red, #e06c75) 8%, var(--bg-pane));
    padding: 0.6rem 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    font-family: var(--font-mono);
    font-size: var(--fs-small);
  }
  .failure-title {
    color: var(--accent-red, #e06c75);
    font-weight: 600;
    font-size: 0.78rem;
  }
  .failure-message {
    color: var(--fg-default);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .failure-hint {
    color: var(--fg-muted);
    white-space: pre-wrap;
    word-break: break-word;
    border-top: 1px solid var(--border-pane);
    padding-top: 0.3rem;
    margin-top: 0.1rem;
  }
  .failure-footnote {
    color: var(--fg-dim);
    font-size: 0.7rem;
    font-style: italic;
  }
  .failure-actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.25rem;
  }
  .failure-btn {
    padding: 0.2rem 0.65rem;
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    cursor: pointer;
    border: 1px solid var(--border-pane-strong);
    background: var(--bg-pane-alt);
    color: var(--fg-default);
  }
  .failure-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .retry-btn {
    border-color: var(--accent-cyan);
    color: var(--accent-cyan);
  }
  .retry-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent-cyan) 12%, var(--bg-pane-alt));
  }
  .switch-btn:hover:not(:disabled) {
    background: var(--bg-pane);
    border-color: var(--fg-default);
  }
</style>
