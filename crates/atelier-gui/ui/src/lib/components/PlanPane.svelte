<script lang="ts">
  // Plan canvas pane. Mirror of TUI `render_plan` + `plan_step_lines`
  // (`crates/atelier-tui/src/lib.rs`): status glyphs, indented
  // constraints, strike-through for terminal-state steps.

  import type { PlanStep, PlanStatus } from '../state'
  import { invoke } from '@tauri-apps/api/core'
  import { onDestroy } from 'svelte'

  type Props = {
    planSteps: PlanStep[]
  }

  let { planSteps }: Props = $props()

  let draftText: string = $state('')
  let constraintFor: string | null = $state(null)
  let constraintDraft: string = $state('')
  let toast: string | null = $state(null)
  let toastError: boolean = $state(false)

  // v60.6 — HTML5 drag-and-drop reorder. Replaces the v55 up/down
  // arrow buttons; the dispatcher mutator (`reorder_plan_steps`) is
  // unchanged. Drag state is in-component (not persisted) and
  // cleared on drop/dragend; the dispatcher re-emits the matching
  // `PlanSnapshot` on success so the visual reorder happens
  // wholesale on the next event tick rather than being applied
  // optimistically in this component.
  let dragSourceIdx: number | null = $state(null)
  let dragOverIdx: number | null = $state(null)

  function glyph(status: PlanStatus): string {
    switch (status) {
      case 'pending':
        return '[ ]'
      case 'in_progress':
        return '[▸]'
      case 'done':
        return '[✓]'
      case 'skipped':
        return '[~]'
    }
  }

  function isTerminal(status: PlanStatus): boolean {
    return status === 'done' || status === 'skipped'
  }

  /// Status cycler: pending → in_progress → done → skipped → pending.
  /// The dispatcher's `mark_plan_step_status` re-emits PlanSnapshot
  /// so we don't update local state.
  function nextStatus(s: PlanStatus): PlanStatus {
    switch (s) {
      case 'pending':
        return 'in_progress'
      case 'in_progress':
        return 'done'
      case 'done':
        return 'skipped'
      case 'skipped':
        return 'pending'
    }
  }

  async function addStep() {
    const text = draftText.trim()
    if (!text) return
    try {
      await invoke<string>('add_plan_step', { text })
      draftText = ''
    } catch (e) {
      showToast(String(e), true)
    }
  }

  async function cycleStatus(step: PlanStep) {
    try {
      await invoke<null>('mark_plan_step_status', {
        id: step.id,
        status: nextStatus(step.status),
      })
    } catch (e) {
      showToast(String(e), true)
    }
  }

  async function removeStep(id: string) {
    try {
      await invoke<null>('remove_plan_step', { id })
    } catch (e) {
      showToast(String(e), true)
    }
  }

  function openConstraint(id: string) {
    constraintFor = id
    constraintDraft = ''
  }

  function cancelConstraint() {
    constraintFor = null
    constraintDraft = ''
  }

  async function saveConstraint() {
    if (!constraintFor) return
    const text = constraintDraft.trim()
    if (!text) {
      cancelConstraint()
      return
    }
    const id = constraintFor
    constraintFor = null
    constraintDraft = ''
    try {
      await invoke<null>('add_plan_step_constraint', { id, constraint: text })
    } catch (e) {
      showToast(String(e), true)
    }
  }

  /// Pure helper extracted so the reorder math is unit-testable
  /// without spinning up Tauri. Returns a fresh array where the
  /// element originally at `from` lands at `to`. If `from === to` (or
  /// the indices are out of range), returns a shallow copy unchanged.
  export function reorderArray<T>(arr: T[], from: number, to: number): T[] {
    if (from < 0 || from >= arr.length || to < 0 || to >= arr.length || from === to) {
      return arr.slice()
    }
    const next = arr.slice()
    const [moved] = next.splice(from, 1)
    next.splice(to, 0, moved)
    return next
  }

  function onDragStart(idx: number, ev: DragEvent) {
    dragSourceIdx = idx
    if (ev.dataTransfer) {
      ev.dataTransfer.effectAllowed = 'move'
      // Some browsers refuse to start a drag without setData payload.
      ev.dataTransfer.setData('text/plain', String(idx))
    }
  }

  function onDragOver(idx: number, ev: DragEvent) {
    if (dragSourceIdx === null) return
    ev.preventDefault()
    if (ev.dataTransfer) {
      ev.dataTransfer.dropEffect = 'move'
    }
    dragOverIdx = idx
  }

  function onDragLeave(idx: number) {
    if (dragOverIdx === idx) {
      dragOverIdx = null
    }
  }

  function onDragEnd() {
    dragSourceIdx = null
    dragOverIdx = null
  }

  async function onDrop(idx: number, ev: DragEvent) {
    ev.preventDefault()
    const from = dragSourceIdx
    dragSourceIdx = null
    dragOverIdx = null
    if (from === null || from === idx) return
    const ids = planSteps.map((s) => s.id)
    const reordered = reorderArray(ids, from, idx)
    await reorder(reordered)
  }

  async function reorder(ordering: string[]) {
    try {
      await invoke<null>('reorder_plan_steps', { ordering })
    } catch (e) {
      showToast(String(e), true)
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
    }, 4000)
  }

  onDestroy(() => {
    if (toastTimer != null) clearTimeout(toastTimer)
  })
</script>

<section class="pane">
  <header class="pane-title">Plan</header>
  <form
    class="composer"
    onsubmit={(e) => {
      e.preventDefault()
      void addStep()
    }}
  >
    <input
      type="text"
      bind:value={draftText}
      placeholder="add a plan step…"
      aria-label="new plan step text"
    />
    <button type="submit" disabled={!draftText.trim()}>add</button>
  </form>
  <div class="scroll">
    {#if planSteps.length === 0}
      <p class="empty">no plan steps</p>
    {:else}
      <ul class="steps">
        {#each planSteps as step, idx (step.id)}
          <li
            class="step-item"
            class:dragging={dragSourceIdx === idx}
            class:drag-target={dragOverIdx === idx && dragSourceIdx !== idx}
            draggable="true"
            ondragstart={(e) => onDragStart(idx, e)}
            ondragover={(e) => onDragOver(idx, e)}
            ondragleave={() => onDragLeave(idx)}
            ondrop={(e) => void onDrop(idx, e)}
            ondragend={onDragEnd}
            aria-label="plan step {idx + 1} of {planSteps.length}"
          >
            <div class="step-row">
              <span
                class="drag-handle"
                title="drag to reorder"
                aria-hidden="true"
              >⋮⋮</span>
              <button
                class="glyph-btn {step.status}"
                onclick={() => void cycleStatus(step)}
                title="cycle status (pending → in_progress → done → skipped)"
                aria-label="cycle status for {step.text}"
              >
                {glyph(step.status)}
              </button>
              <span
                class="step-text"
                class:terminal={isTerminal(step.status)}
              >
                {step.text}
              </span>
              <span class="step-actions">
                <button
                  class="action"
                  onclick={() => openConstraint(step.id)}
                  title="add constraint"
                  aria-label="add constraint to {step.text}"
                >+c</button>
                <button
                  class="action danger"
                  onclick={() => void removeStep(step.id)}
                  title="remove step"
                  aria-label="remove {step.text}"
                >✕</button>
              </span>
            </div>
            {#if constraintFor === step.id}
              <form
                class="constraint-form"
                onsubmit={(e) => {
                  e.preventDefault()
                  void saveConstraint()
                }}
              >
                <input
                  type="text"
                  bind:value={constraintDraft}
                  placeholder="constraint text…"
                  aria-label="new constraint for {step.text}"
                />
                <button type="submit">save</button>
                <button type="button" onclick={cancelConstraint}>cancel</button>
              </form>
            {/if}
            {#if step.constraints && step.constraints.length > 0}
              <ul class="constraints">
                {#each step.constraints as c, ci (`${step.id}-c-${ci}`)}
                  <li>└ {c}</li>
                {/each}
              </ul>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </div>
  {#if toast}
    <p class="toast" class:toast-error={toastError}>{toast}</p>
  {/if}
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
  .steps,
  .constraints {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .steps > li + li {
    margin-top: 0.35rem;
  }
  .step-row {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
  }
  .glyph-btn {
    font-weight: 600;
    flex: 0 0 1.6rem;
    background: transparent;
    border: none;
    cursor: pointer;
    font-family: inherit;
    font-size: inherit;
    padding: 0;
    text-align: left;
  }
  .glyph-btn:hover {
    text-decoration: underline;
  }
  .glyph-btn.pending {
    color: var(--status-pending);
  }
  .glyph-btn.in_progress {
    color: var(--status-in-progress);
  }
  .glyph-btn.done {
    color: var(--status-done);
  }
  .glyph-btn.skipped {
    color: var(--status-skipped);
  }
  .step-actions {
    margin-left: auto;
    display: inline-flex;
    gap: 0.2rem;
    opacity: 0.4;
    transition: opacity 0.1s;
  }
  .steps li:hover .step-actions {
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
  .action:hover:not(:disabled) {
    background: var(--bg-hover, rgba(255, 255, 255, 0.06));
  }
  .action:disabled {
    opacity: 0.3;
    cursor: not-allowed;
  }
  .action.danger:hover:not(:disabled) {
    color: #f88;
    border-color: #844;
  }
  .composer {
    display: flex;
    gap: 0.4rem;
    align-items: stretch;
    padding: 0.35rem 0.75rem;
    border-bottom: 1px dotted var(--border-pane);
  }
  .composer input {
    flex: 1;
    background: var(--bg-input, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-pane);
    color: var(--fg-default, #ddd);
    border-radius: 3px;
    padding: 0.2rem 0.4rem;
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
  .constraint-form {
    display: flex;
    gap: 0.3rem;
    margin: 0.3rem 0 0 2.1rem;
    font-size: 0.72rem;
  }
  .constraint-form input {
    flex: 1;
    background: var(--bg-input, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-pane);
    color: var(--fg-default, #ddd);
    border-radius: 3px;
    padding: 0.15rem 0.3rem;
    font-family: var(--font-mono);
    font-size: 0.7rem;
  }
  .constraint-form button {
    background: transparent;
    border: 1px solid var(--border-pane);
    border-radius: 3px;
    color: var(--fg-default, #ddd);
    cursor: pointer;
    font-family: inherit;
    font-size: 0.7rem;
    padding: 0 0.4rem;
  }
  .toast {
    margin: 0;
    padding: 0.3rem 0.75rem;
    font-size: 0.72rem;
    color: var(--fg-dim);
    border-top: 1px dotted var(--border-pane);
    background: rgba(0, 200, 100, 0.04);
  }
  .toast-error {
    color: #f88;
    background: rgba(200, 0, 0, 0.05);
  }
  .step-text {
    color: var(--fg-default);
    word-break: break-word;
  }
  .step-text.terminal {
    color: var(--fg-dim);
    text-decoration: line-through;
  }
  .constraints {
    margin-left: 2.4rem;
    color: var(--fg-dim);
    font-size: 0.75rem;
  }
  .constraints li {
    padding: 0.05rem 0;
  }
  /* v60.6 — drag-and-drop reorder affordances. */
  .step-item {
    cursor: default;
  }
  .step-item.dragging {
    opacity: 0.4;
  }
  .step-item.drag-target {
    /* Top border highlights the drop slot — the moved item lands
       at this row's index, displacing the row downward. */
    border-top: 2px solid var(--fg-accent, #6cf);
    margin-top: -2px;
  }
  .drag-handle {
    color: var(--fg-dim);
    font-size: 0.85rem;
    cursor: grab;
    user-select: none;
    flex: 0 0 1rem;
    line-height: 1;
    padding: 0 0.2rem 0 0;
    opacity: 0.4;
    transition: opacity 0.1s;
  }
  .step-item:hover .drag-handle {
    opacity: 1;
  }
  .step-item.dragging .drag-handle {
    cursor: grabbing;
  }
</style>
