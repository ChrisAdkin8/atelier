<script lang="ts">
  // Prompt composer. Non-slash path routes through `start_agent_run`
  // (Runner-backed, full tools + sub-agent support). Slash path routes
  // through `invoke_skill` which also calls `start_agent_run` after
  // skill expansion. `start_chat_run` is no longer used here.
  //
  // v60.52 §15 — when the input starts with `/`, surface a transient
  // skill autocomplete menu. `Tab` accepts the highlighted match;
  // `Enter` (no Cmd/Ctrl) commits a complete `/<name> [args]` via
  // `invoke_skill`. `Esc` closes the menu without committing.

  import { invoke } from '@tauri-apps/api/core'
  import { onMount } from 'svelte'

  type Props = {
    busy: boolean
    thinking: boolean
  }

  let { busy, thinking }: Props = $props()

  type SkillEntry = {
    name: string
    description: string
    proactive: boolean
    source: string
    args: string[]
  }

  let prompt: string = $state('')
  let error: string | null = $state(null)
  let starting: boolean = $state(false)
  let skills: SkillEntry[] = $state([])
  let menuSelectedIndex: number = $state(0)

  onMount(async () => {
    try {
      skills = await invoke<SkillEntry[]>('list_skills')
    } catch (e) {
      // Non-fatal — the autocomplete is polish, not core function.
      console.warn('list_skills failed', e)
    }
  })

  // Filter the menu based on the in-progress slash token. Returns []
  // when the input doesn't begin with `/` (menu is hidden).
  //
  // v60.55 — no length cap. The bundled set has 19 skills today (and
  // growing); a slice(0, 8) hid the rest. The CSS gives the popup
  // `max-height: 14rem` + `overflow-y: auto`, so a long list scrolls
  // rather than truncates silently.
  let menuMatches = $derived.by(() => {
    const trimmed = prompt.trimStart()
    if (!trimmed.startsWith('/')) return []
    const headEnd = trimmed.search(/\s/)
    const head = headEnd === -1 ? trimmed.slice(1) : trimmed.slice(1, headEnd)
    if (headEnd !== -1) return [] // user already typed past the name
    return skills.filter((s) => s.name.startsWith(head))
  })

  let menuVisible = $derived(menuMatches.length > 0)

  // Keep selection in range as the filter shrinks.
  $effect(() => {
    if (menuSelectedIndex >= menuMatches.length) {
      menuSelectedIndex = 0
    }
  })

  function parseSlash(input: string): { name: string; args: Record<string, string> } | null {
    const trimmed = input.trim()
    if (!trimmed.startsWith('/')) return null
    const rest = trimmed.slice(1)
    const sp = rest.search(/\s/)
    const name = sp === -1 ? rest : rest.slice(0, sp)
    if (!name) return null
    const tail = sp === -1 ? '' : rest.slice(sp).trim()
    if (!tail) return { name, args: {} }
    const args: Record<string, string> = {}
    // Single-arg positional fallback: if the matching skill declares
    // exactly one arg AND the tail has no top-level `=`, bind the
    // whole tail to that arg.
    const skill = skills.find((s) => s.name === name)
    const hasEquals = /(^|\s)[A-Za-z_][A-Za-z0-9_]*=/.test(tail)
    if (skill && skill.args.length === 1 && !hasEquals) {
      args[skill.args[0]] = tail
      return { name, args }
    }
    // key=value tokens, with double-quoted values supported.
    const tokRe = /([A-Za-z_][A-Za-z0-9_]*)=("([^"]*)"|(\S+))/g
    let m: RegExpExecArray | null
    while ((m = tokRe.exec(tail)) !== null) {
      args[m[1]] = m[3] ?? m[4]
    }
    return { name, args }
  }

  async function stopRun() {
    try {
      await invoke('cancel_run')
    } catch (e) {
      // Non-fatal — run may have already finished by the time this fires.
      console.warn('cancel_run failed', e)
    }
  }

  async function commit() {
    const trimmed = prompt.trim()
    if (!trimmed || busy || starting) return
    starting = true
    error = null
    try {
      const slash = parseSlash(trimmed)
      if (slash) {
        await invoke('invoke_skill', { name: slash.name, args: slash.args })
      } else {
        await invoke('start_agent_run', { prompt: trimmed })
      }
      prompt = ''
    } catch (e) {
      error = String(e)
    } finally {
      starting = false
    }
  }

  function acceptHighlighted() {
    if (!menuVisible) return
    const pick = menuMatches[menuSelectedIndex]
    if (!pick) return
    // Replace the partial slash with the full name + space so the
    // user can type args next.
    prompt = `/${pick.name} `
  }

  function onKey(e: KeyboardEvent) {
    if (menuVisible) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        menuSelectedIndex = (menuSelectedIndex + 1) % menuMatches.length
        return
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault()
        menuSelectedIndex =
          (menuSelectedIndex - 1 + menuMatches.length) % menuMatches.length
        return
      }
      if (e.key === 'Tab') {
        e.preventDefault()
        acceptHighlighted()
        return
      }
      if (e.key === 'Escape') {
        e.preventDefault()
        prompt = ''
        return
      }
    }
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
      e.preventDefault()
      void commit()
    }
  }

  let canSubmit = $derived(
    !busy && !starting && prompt.trim().length > 0,
  )

  // v60.55 — auto-scroll the selected row into view as ↑/↓ moves the
  // highlight. Without this, pressing ↓ past the visible items moves
  // the highlight off-screen and the menu stays put; the user has to
  // mouse-scroll to follow it. `block: 'nearest'` only scrolls when
  // the row is actually out of view, so a click+drag selection inside
  // the viewport doesn't snap the scrollbar.
  let menuItemEls: (HTMLLIElement | null)[] = $state([])
  $effect(() => {
    const el = menuItemEls[menuSelectedIndex]
    if (el) el.scrollIntoView({ block: 'nearest' })
  })
</script>

<section class="composer" class:thinking>
  <textarea
    placeholder={busy
      ? 'a turn is in progress — wait for it to finish'
      : 'type a prompt and hit Cmd+Enter — or start with `/` for a skill (Tab to autocomplete)'}
    bind:value={prompt}
    onkeydown={onKey}
    disabled={busy || starting}
    rows="2"
  ></textarea>
  {#if menuVisible}
    <ul class="skill-menu">
      {#each menuMatches as match, i}
        <li bind:this={menuItemEls[i]} class:selected={i === menuSelectedIndex}>
          <span class="slug">/{match.name}</span>
          <span class="desc">{match.description}</span>
          {#if match.proactive}<span class="tag">[proactive]</span>{/if}
          <span class="src">[{match.source}]</span>
        </li>
      {/each}
    </ul>
  {/if}
  <div class="composer-actions">
    <span class="hint">
      Cmd+Enter to send · `/` for skills (Tab to autocomplete, Esc to clear)
    </span>
    <div class="btn-group">
      <button
        class="send"
        onclick={commit}
        disabled={!canSubmit}
      >
        {starting ? 'starting…' : 'Send'}
      </button>
      {#if busy || thinking}
        <button class="stop" onclick={stopRun} title="Stop current run">
          &#9632; Stop
        </button>
      {/if}
    </div>
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
    transition: border-top-color 0.2s ease;
  }
  .composer.thinking {
    border-top-color: var(--accent-cyan);
    animation: thinking-glow 2s ease-in-out infinite;
  }
  @keyframes thinking-glow {
    0%, 100% { box-shadow: 0 -4px 12px -3px rgba(121, 192, 255, 0.15); }
    50%       { box-shadow: 0 -4px 20px -3px rgba(121, 192, 255, 0.45); }
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
  .skill-menu {
    list-style: none;
    margin: 0;
    padding: 0.25rem 0;
    background: var(--bg-pane-alt);
    border: 1px solid var(--border-pane-strong);
    border-radius: 4px;
    max-height: 14rem;
    overflow-y: auto;
    font-family: var(--font-mono);
    font-size: 0.78rem;
  }
  .skill-menu li {
    padding: 0.2rem 0.6rem;
    display: grid;
    grid-template-columns: 10rem 1fr auto auto;
    gap: 0.6rem;
    align-items: baseline;
  }
  .skill-menu li.selected {
    background: var(--accent-cyan);
    color: #062131;
  }
  .skill-menu .slug {
    color: var(--accent-cyan);
    font-weight: 600;
  }
  .skill-menu li.selected .slug {
    color: #062131;
  }
  .skill-menu .desc {
    color: var(--fg-default);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .skill-menu .tag,
  .skill-menu .src {
    color: var(--fg-dim);
    font-size: 0.7rem;
  }
  .composer-actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .btn-group {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-shrink: 0;
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
  .stop {
    padding: 0.3rem 0.9rem;
    border-radius: 4px;
    border: 1px solid var(--accent-red, #e06c75);
    background: var(--accent-red, #e06c75);
    color: #fff;
    font-weight: 600;
    cursor: pointer;
    font-size: 0.85rem;
  }
  .stop:hover {
    filter: brightness(1.15);
  }
  .error {
    margin: 0;
    color: var(--accent-red);
    font-size: 0.75rem;
    font-family: var(--font-mono);
  }
</style>
