<script lang="ts">
  // Header row: app brand + workspace selector. v60.47 — dropped the
  // legacy state/EditStaged/scrub meta items; in chat-only mode none of
  // them ever populated (state never advances, no staging happens, the
  // scrub keys have no backend), so they were dead labels. If a
  // Runner-driven mode comes back into the GUI later, the meta block
  // can be restored from git history at this file's commit prior to
  // v60.47.

  import { invoke } from '@tauri-apps/api/core'
  import { open } from '@tauri-apps/plugin-dialog'
  import { onMount } from 'svelte'

  // v60.49 — right-column collapse toggle, owned by App.svelte.
  type Props = {
    rightCollapsed?: boolean
    onToggleRight?: () => void
  }
  let { rightCollapsed = false, onToggleRight }: Props = $props()

  let workspacePath: string = $state('')
  let editing: boolean = $state(false)
  let draft: string = $state('')
  let saving: boolean = $state(false)
  let errorMsg: string | null = $state(null)

  onMount(async () => {
    try {
      workspacePath = await invoke<string>('get_workspace')
    } catch (e) {
      console.warn('get_workspace failed', e)
    }
  })

  function startEdit() {
    draft = workspacePath
    errorMsg = null
    editing = true
  }
  function cancelEdit() {
    editing = false
    errorMsg = null
  }
  async function saveEdit() {
    const trimmed = draft.trim()
    if (!trimmed || trimmed === workspacePath) {
      editing = false
      return
    }
    saving = true
    errorMsg = null
    try {
      const resolved = await invoke<string>('set_workspace', { path: trimmed })
      workspacePath = resolved
      editing = false
    } catch (e) {
      errorMsg = String(e)
    } finally {
      saving = false
    }
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault()
      void saveEdit()
    } else if (e.key === 'Escape') {
      e.preventDefault()
      cancelEdit()
    }
  }

  // v60.46 — native OS folder picker. Opens Finder/Explorer/etc;
  // the user-chosen path goes straight through `set_workspace`,
  // skipping the text-input draft state entirely.
  async function browse() {
    errorMsg = null
    try {
      const picked = await open({
        directory: true,
        multiple: false,
        title: 'Pick a workspace folder',
        defaultPath: workspacePath || undefined,
      })
      if (!picked || typeof picked !== 'string') return // user cancelled
      saving = true
      const resolved = await invoke<string>('set_workspace', { path: picked })
      workspacePath = resolved
      editing = false
    } catch (e) {
      errorMsg = String(e)
    } finally {
      saving = false
    }
  }

  // Show a short friendly form ("~/foo/bar" if under HOME, else the
  // last 2 path segments) in the read-only view; full path lives in
  // the title attribute. Edit mode always shows the canonicalised
  // absolute form so the user can see what they're about to change.
  let workspaceShort = $derived.by(() => {
    if (!workspacePath) return ''
    // The Rust side doesn't tell us HOME, so we settle for "…/last/two".
    const parts = workspacePath.split('/').filter(Boolean)
    if (parts.length <= 2) return workspacePath
    return '…/' + parts.slice(-2).join('/')
  })
</script>

<header class="header">
  <h1 class="brand">atelier</h1>
  <div class="meta">
    <span class="meta-item workspace">
      <span class="meta-label">workspace</span>
      {#if editing}
        <input
          class="workspace-input"
          type="text"
          bind:value={draft}
          onkeydown={onKey}
          placeholder="/absolute/path/to/repo"
          disabled={saving}
          autofocus
        />
        <button class="workspace-btn" onclick={saveEdit} disabled={saving}>
          {saving ? '…' : 'save'}
        </button>
        <button class="workspace-btn cancel" onclick={cancelEdit} disabled={saving}>
          cancel
        </button>
        {#if errorMsg}
          <span class="workspace-error" title={errorMsg}>{errorMsg}</span>
        {/if}
      {:else}
        <button
          class="workspace-view"
          onclick={startEdit}
          title={workspacePath || 'click to set workspace'}
        >
          {workspaceShort || '<unset>'}
        </button>
        <button
          class="workspace-btn"
          onclick={browse}
          disabled={saving}
          title="open a native folder picker"
        >
          Browse…
        </button>
      {/if}
    </span>
    <!-- v60.49 — collapse/expand the right column. Arrow points the
         direction the panels will move on click. When expanded, the
         arrow points right (→) for "push the panels out"; when
         collapsed, it points left (←) for "bring the panels back in". -->
    <button
      class="collapse-btn"
      onclick={() => onToggleRight?.()}
      aria-pressed={rightCollapsed}
      title={rightCollapsed ? 'show right-side panels' : 'hide right-side panels'}
      aria-label={rightCollapsed ? 'show right-side panels' : 'hide right-side panels'}
    >
      {rightCollapsed ? '←' : '→'}
    </button>
  </div>
</header>

<style>
  .header {
    display: flex;
    /* v60.55 — top-align every header item so the wordmark cap-line
       and the meta-row baselines share the same upper edge. Pre-v60.55
       used `align-items: center`, which left the small meta text
       floating mid-height next to the larger wordmark. */
    align-items: flex-start;
    gap: 1.5rem;
    padding: 0.6rem 1rem;
    border-bottom: 1px solid var(--border-pane);
    background: var(--bg-pane);
  }
  /* v60.49 — wordmark matches `assets/banner.svg`:
       font-family Iowan Old Style serif, weight 400,
       cream `#f0ead6`, slight negative tracking.
     v60.55 — rendered lowercase to match the banner. */
  .brand {
    margin: 0;
    font-family: 'Iowan Old Style', Georgia, 'Times New Roman', serif;
    font-size: 1.6rem;
    font-weight: 400;
    color: #f0ead6;
    letter-spacing: -0.02em;
    line-height: 1;
  }
  .meta {
    /* v60.48 — pinned to the far right of the header. `margin-left: auto`
       is the canonical flexbox idiom for "leave the brand on the left,
       slide everything else to the right edge." */
    margin-left: auto;
    display: flex;
    gap: 1.25rem;
    color: var(--fg-muted);
    font-size: var(--fs-small);
    font-family: var(--font-mono);
    flex-wrap: wrap;
  }
  .meta-item {
    display: inline-flex;
    gap: 0.4rem;
    align-items: baseline;
  }
  .meta-label {
    color: var(--fg-dim);
  }
  /* v60.45 — workspace selector */
  .meta-item.workspace {
    gap: 0.5rem;
  }
  .workspace-view {
    color: var(--accent-cyan);
    background: transparent;
    border: 1px dashed transparent;
    padding: 0.1rem 0.4rem;
    border-radius: 3px;
    font-family: var(--font-mono);
    font-size: var(--fs-small);
    cursor: pointer;
  }
  .workspace-view:hover {
    border-color: var(--border-pane-strong);
    background: var(--bg-pane-alt);
  }
  .workspace-input {
    min-width: 22rem;
    padding: 0.15rem 0.4rem;
    background: var(--bg-pane-alt);
    color: var(--fg-default);
    border: 1px solid var(--accent-cyan);
    border-radius: 3px;
    font-family: var(--font-mono);
    font-size: var(--fs-small);
  }
  .workspace-input:focus {
    outline: none;
  }
  .workspace-btn {
    padding: 0.15rem 0.5rem;
    background: var(--accent-cyan);
    color: #062131;
    border: 1px solid var(--accent-cyan);
    border-radius: 3px;
    font-weight: 600;
    cursor: pointer;
    font-size: 0.7rem;
  }
  .workspace-btn.cancel {
    background: transparent;
    color: var(--fg-dim);
    border-color: var(--border-pane-strong);
    font-weight: 400;
  }
  .workspace-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .workspace-error {
    color: var(--accent-red);
    font-size: 0.7rem;
    max-width: 18rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* v60.49 — right-column collapse toggle. Square button so the arrow
     glyph centres cleanly; hover lifts it just enough to feel
     interactive without competing with the workspace controls. */
  .collapse-btn {
    width: 1.6rem;
    height: 1.6rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: 1px solid var(--border-pane-strong);
    border-radius: 3px;
    color: var(--fg-default);
    font-family: var(--font-mono);
    font-size: 0.9rem;
    line-height: 1;
    cursor: pointer;
    padding: 0;
  }
  .collapse-btn:hover {
    background: var(--bg-pane-alt);
    border-color: var(--accent-cyan);
    color: var(--accent-cyan);
  }
  .collapse-btn[aria-pressed='true'] {
    color: var(--accent-cyan);
    border-color: var(--accent-cyan);
  }
</style>
