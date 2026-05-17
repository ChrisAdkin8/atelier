<script lang="ts">
  // Diff pane. Mirror of TUI `render_diff` (`crates/atelier-tui/src/lib.rs`):
  // most-recent staged edit, hunks rendered with `+`/`-` markers when
  // `Hunks::Lines`, badges for `Created` / `Deleted` / `Binary` / `Same`.
  //
  // Pending approval (spec §3) takes precedence: when the dispatcher
  // emitted `StagingPendingApproval` we render an APPROVAL banner with
  // per-file checkboxes and Accept-Selected / Reject-All buttons. The
  // buttons invoke the `submit_approval` Tauri command which routes
  // to the live `SessionDispatcher::submit_approval` (wired in v47).

  import type { StagedEdit, Hunk, PendingApproval } from '../state'
  import { invoke } from '@tauri-apps/api/core'

  type Props = {
    recentEdits: StagedEdit[]
    pendingApproval: PendingApproval | null
    /// v56 — per-file "why this change?" rationale from the envelope's
    /// `claimed_changes`. Keyed by repo-relative path. Optional so
    /// pre-v56 callers and tests can omit it.
    claimedChanges?: Record<string, string>
  }

  let {
    recentEdits,
    pendingApproval,
    claimedChanges = {},
  }: Props = $props()

  let head = $derived(recentEdits[0] ?? null)

  // v56 — per-file hunk-toggle map. For `Hunks::Lines` files this
  // tracks which hunk indices are accepted; for other hunk kinds it's
  // a single file-level boolean (`fileChecked` is the source of truth
  // and `hunkChecked` is empty). The null-prototype wrap is preserved
  // from v49 to keep hostile paths (`__proto__`, `constructor`) from
  // reaching Object.prototype.
  type FileToggle = {
    fileChecked: boolean
    hunkChecked: boolean[]
  }
  let toggles: Record<string, FileToggle> = $state(Object.create(null))

  // Error surfaced to the user if `submit_approval` returns false
  // (dispatcher rejected the commit_id — typically stale / already
  // approved). Cleared whenever a new pending arrives.
  let submitError: string | null = $state(null)

  $effect(() => {
    // Only clear `submitError` when a *new* pending arrives — a
    // `CommitDecision` that transitions pendingApproval to null
    // shouldn't wipe a just-surfaced error before the user reads it.
    if (pendingApproval) submitError = null
    if (!pendingApproval) {
      toggles = withNullProtoToggles({})
      return
    }
    const init: Record<string, FileToggle> = {}
    for (const f of pendingApproval.files) {
      const hunkCount =
        f.hunks.kind === 'lines' ? f.hunks.hunks.length : 0
      init[f.path] = {
        fileChecked: true,
        hunkChecked: new Array(hunkCount).fill(true),
      }
    }
    toggles = withNullProtoToggles(init)
  })

  type FileApprovalWire =
    | { mode: 'all' }
    | { mode: 'hunks'; indices: number[] }

  function buildSelection(): Record<string, FileApprovalWire> {
    const out: Record<string, FileApprovalWire> = Object.create(null)
    if (!pendingApproval) return out
    for (const f of pendingApproval.files) {
      const t = toggles[f.path]
      if (!t || !t.fileChecked) continue
      if (f.hunks.kind === 'lines') {
        const indices = t.hunkChecked
          .map((c, i) => (c ? i : -1))
          .filter((i) => i >= 0)
        if (indices.length === 0) continue
        if (indices.length === t.hunkChecked.length) {
          out[f.path] = { mode: 'all' }
        } else {
          out[f.path] = { mode: 'hunks', indices }
        }
      } else {
        out[f.path] = { mode: 'all' }
      }
    }
    return out
  }

  async function submit(selection: Record<string, FileApprovalWire>) {
    if (!pendingApproval) return
    submitError = null
    try {
      const ok = await invoke<boolean>('submit_approval', {
        commitId: pendingApproval.commitId,
        selection,
      })
      if (!ok) {
        submitError =
          'dispatcher rejected this approval (commit_id stale or already resolved)'
      }
    } catch (e) {
      submitError = String(e)
    }
  }

  function acceptSelected() {
    void submit(buildSelection())
  }

  function rejectAll() {
    void submit({})
  }

  function setFile(path: string, checked: boolean) {
    const t = toggles[path]
    if (!t) return
    const next: FileToggle = {
      fileChecked: checked,
      hunkChecked: t.hunkChecked.map(() => checked),
    }
    toggles = withNullProtoToggles({ ...toggles, [path]: next })
  }

  function setHunk(path: string, hunkIndex: number, checked: boolean) {
    const t = toggles[path]
    if (!t) return
    const hunkChecked = t.hunkChecked.slice()
    hunkChecked[hunkIndex] = checked
    // File-level checkbox reflects: any hunk checked → true.
    const fileChecked = hunkChecked.some((c) => c)
    toggles = withNullProtoToggles({
      ...toggles,
      [path]: { fileChecked, hunkChecked },
    })
  }

  function fileLabel(path: string): string {
    const t = toggles[path]
    if (!t) return ''
    if (t.hunkChecked.length === 0) return ''
    const accepted = t.hunkChecked.filter((c) => c).length
    return `${accepted} / ${t.hunkChecked.length} hunks`
  }

  /// Copy `source` into a fresh null-prototype object. The
  /// spread/destructure patterns above produce default-prototype
  /// objects that would lose this mitigation otherwise.
  function withNullProtoToggles(
    source: Record<string, FileToggle>,
  ): Record<string, FileToggle> {
    return Object.assign(Object.create(null), source)
  }
</script>

<section class="pane" class:pending={pendingApproval != null}>
  <header class="pane-title">
    {pendingApproval ? 'diff (PENDING)' : 'diff'}
  </header>
  <div class="scroll">
    {#if pendingApproval}
      <div class="approval-banner">
        <p>
          <strong>{pendingApproval.files.length}</strong> file(s)
          awaiting approval —
          <span class="commit-id">{pendingApproval.commitId.slice(0, 8)}</span>
        </p>
        <div class="approval-actions">
          <button class="primary" onclick={acceptSelected}>
            accept selected
          </button>
          <button class="danger" onclick={rejectAll}>
            reject all
          </button>
        </div>
      </div>
      <ul class="pending-files">
        {#each pendingApproval.files as file (file.path)}
          <li>
            <label class="file-row">
              <input
                type="checkbox"
                checked={toggles[file.path]?.fileChecked ?? false}
                onchange={(e) => setFile(file.path, e.currentTarget.checked)}
              />
              <span class="file-path">{file.path}</span>
              <span class="hunks-kind">[{file.hunks.kind}]</span>
              {#if file.hunks.kind === 'lines'}
                <span class="hunks-count">{fileLabel(file.path)}</span>
              {/if}
            </label>
            {#if file.hunks.kind === 'lines' && file.hunks.hunks.length > 1}
              <ul class="pending-hunks">
                {#each file.hunks.hunks as hunk, hi (hi)}
                  <li>
                    <label class="hunk-row">
                      <input
                        type="checkbox"
                        checked={toggles[file.path]?.hunkChecked[hi] ?? false}
                        onchange={(e) =>
                          setHunk(file.path, hi, e.currentTarget.checked)}
                      />
                      <span class="hunk-header">
                        @@ -{hunk.old_range.start + 1},{hunk.old_range.end -
                          hunk.old_range.start}
                        +{hunk.new_range.start + 1},{hunk.new_range.end -
                          hunk.new_range.start} @@
                      </span>
                      <span class="hunk-summary">
                        −{hunk.old_lines.length} / +{hunk.new_lines.length}
                      </span>
                    </label>
                  </li>
                {/each}
              </ul>
            {/if}
          </li>
        {/each}
      </ul>
      {#if submitError}
        <p class="submit-error">{submitError}</p>
      {/if}
    {:else if head == null}
      <p class="empty">no edits yet</p>
    {:else}
      <div class="path">─── <strong>{head.path}</strong></div>
      {#if claimedChanges[head.path]}
        <p class="why">
          <span class="why-label">why:</span>
          {claimedChanges[head.path]}
        </p>
      {/if}

      {#if head.hunks.kind === 'same'}
        <p class="badge muted">no diff — byte-equal</p>
      {:else if head.hunks.kind === 'binary'}
        <p class="badge binary">[binary file changed]</p>
      {:else if head.hunks.kind === 'created'}
        <p class="badge created">
          [created · {head.hunks.new_line_count} lines ·
          {head.hunks.new_byte_len} bytes]
        </p>
      {:else if head.hunks.kind === 'deleted'}
        <p class="badge deleted">
          [deleted · {head.hunks.old_line_count} lines ·
          {head.hunks.old_byte_len} bytes]
        </p>
      {:else if head.hunks.kind === 'lines'}
        <div class="hunks">
          {#each head.hunks.hunks as hunk, hi (hi)}
            {@render hunkBlock(hunk)}
          {/each}
        </div>
      {/if}
    {/if}
  </div>
</section>

{#snippet hunkBlock(hunk: Hunk)}
  <div class="hunk">
    <div class="hunk-header">
      @@ -{hunk.old_range.start + 1},{hunk.old_range.end - hunk.old_range.start}
      +{hunk.new_range.start + 1},{hunk.new_range.end - hunk.new_range.start} @@
    </div>
    {#each hunk.old_lines as line, oi (`o-${oi}`)}
      <div class="line remove">-{line}</div>
    {/each}
    {#each hunk.new_lines as line, ni (`n-${ni}`)}
      <div class="line add">+{line}</div>
    {/each}
  </div>
{/snippet}

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
  .pane.pending {
    border-color: var(--accent-yellow);
  }
  .pane.pending .pane-title {
    color: var(--accent-yellow);
    font-weight: 700;
  }
  .approval-banner {
    background: rgba(226, 192, 141, 0.08);
    border: 1px solid var(--accent-yellow);
    border-radius: 4px;
    padding: 0.5rem 0.6rem;
    margin-bottom: 0.6rem;
    font-family: var(--font-ui);
    font-size: 0.8rem;
  }
  .approval-banner p {
    margin: 0 0 0.4rem 0;
    color: var(--fg-default);
  }
  .commit-id {
    color: var(--accent-yellow);
    font-family: var(--font-mono);
  }
  .approval-actions {
    display: flex;
    gap: 0.5rem;
  }
  .approval-actions button {
    padding: 0.25rem 0.6rem;
    border-radius: 3px;
    border: 1px solid var(--border-pane-strong);
    background: var(--bg-pane-alt);
    color: var(--fg-default);
    cursor: pointer;
    font-size: 0.75rem;
  }
  .approval-actions button.primary {
    background: var(--accent-green);
    color: #062712;
    border-color: var(--accent-green);
    font-weight: 600;
  }
  .approval-actions button.danger {
    background: transparent;
    color: var(--accent-red);
    border-color: var(--accent-red);
  }
  .pending-files {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .pending-files > li {
    margin-bottom: 0.25rem;
  }
  .file-row {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    padding: 0.15rem 0;
    cursor: pointer;
  }
  .file-row:hover {
    background: var(--bg-pane-alt);
  }
  .file-path {
    color: var(--fg-default);
    flex: 1;
  }
  .hunks-kind {
    color: var(--accent-yellow);
    font-size: 0.7rem;
    text-transform: uppercase;
  }
  .hunks-count {
    color: var(--fg-muted);
    font-size: 0.7rem;
    font-family: var(--font-mono);
  }
  .pending-hunks {
    list-style: none;
    margin: 0 0 0.25rem 1.6rem;
    padding: 0;
    border-left: 1px dotted var(--border-pane);
    padding-left: 0.5rem;
  }
  .hunk-row {
    display: flex;
    gap: 0.4rem;
    align-items: baseline;
    padding: 0.05rem 0;
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }
  .hunk-row:hover {
    background: var(--bg-pane-alt);
  }
  .hunk-row .hunk-header {
    color: var(--accent-yellow);
    flex: 0 0 auto;
  }
  .hunk-summary {
    color: var(--fg-dim);
    font-size: 0.68rem;
  }
  .why {
    margin: 0.1rem 0 0.4rem 1.6rem;
    color: var(--fg-muted);
    font-style: italic;
    font-size: 0.78rem;
    font-family: var(--font-ui);
  }
  .why-label {
    color: var(--fg-dim);
    font-style: normal;
    font-family: var(--font-mono);
    margin-right: 0.3rem;
  }
  .submit-error {
    margin: 0.5rem 0 0 0;
    padding: 0.35rem 0.5rem;
    background: rgba(249, 117, 131, 0.08);
    border: 1px solid var(--accent-red);
    border-radius: 4px;
    color: var(--accent-red);
    font-family: var(--font-mono);
    font-size: 0.75rem;
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
    line-height: 1.5;
  }
  .empty {
    color: var(--fg-dim);
    margin: 0;
    font-style: italic;
  }
  .path {
    color: var(--accent-cyan);
    margin-bottom: 0.5rem;
  }
  .badge {
    margin: 0;
  }
  .badge.muted {
    color: var(--fg-dim);
  }
  .badge.binary {
    color: var(--accent-magenta);
  }
  .badge.created {
    color: var(--diff-add);
  }
  .badge.deleted {
    color: var(--diff-remove);
  }
  .hunks {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .hunk-header {
    color: var(--diff-hunk-header);
  }
  .line {
    white-space: pre-wrap;
    word-break: break-word;
  }
  .line.add {
    color: var(--diff-add);
  }
  .line.remove {
    color: var(--diff-remove);
  }
</style>
