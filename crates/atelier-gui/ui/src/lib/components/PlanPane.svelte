<script lang="ts">
  // Plan canvas pane. Mirror of TUI `render_plan` + `plan_step_lines`
  // (`crates/atelier-tui/src/lib.rs`): status glyphs, indented
  // constraints, strike-through for terminal-state steps.

  import type { PlanStep, PlanStatus } from '../state'

  type Props = {
    planSteps: PlanStep[]
  }

  let { planSteps }: Props = $props()

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
</script>

<section class="pane">
  <header class="pane-title">plan</header>
  <div class="scroll">
    {#if planSteps.length === 0}
      <p class="empty">no plan steps</p>
    {:else}
      <ul class="steps">
        {#each planSteps as step (step.id)}
          <li>
            <div class="step-row">
              <span class="glyph {step.status}">{glyph(step.status)}</span>
              <span
                class="step-text"
                class:terminal={isTerminal(step.status)}
              >
                {step.text}
              </span>
            </div>
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
  .glyph {
    font-weight: 600;
    flex: 0 0 1.6rem;
  }
  .glyph.pending {
    color: var(--status-pending);
  }
  .glyph.in_progress {
    color: var(--status-in-progress);
  }
  .glyph.done {
    color: var(--status-done);
  }
  .glyph.skipped {
    color: var(--status-skipped);
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
</style>
