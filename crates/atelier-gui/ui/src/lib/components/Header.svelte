<script lang="ts">
  // Header row: app brand + live counters. Mirrors the TUI header
  // (`render_header` in `crates/atelier-tui/src/lib.rs`):
  //   state label · EditStaged count · scrub indicator
  //
  // Pure presentation — all data passed in by `App.svelte`.

  type Props = {
    currentState: string
    editStagedCount: number
    scrubOffset: number | null
  }

  let { currentState, editStagedCount, scrubOffset }: Props = $props()

  let stateLabel = $derived(currentState || '<no transitions yet>')
  let scrubLabel = $derived(scrubOffset == null ? 'HEAD' : `-${scrubOffset}`)
  let scrubClass = $derived(scrubOffset == null ? 'head' : 'pinned')
</script>

<header class="header">
  <h1>Atelier</h1>
  <div class="meta">
    <span class="meta-item">
      <span class="meta-label">state</span>
      <span class="meta-value">{stateLabel}</span>
    </span>
    <span class="meta-item">
      <span class="meta-label">EditStaged</span>
      <span class="meta-value count">{editStagedCount}</span>
    </span>
    <span class="meta-item">
      <span class="meta-label">scrub</span>
      <span class="meta-value scrub {scrubClass}">{scrubLabel}</span>
    </span>
  </div>
</header>

<style>
  .header {
    display: flex;
    align-items: center;
    gap: 1.5rem;
    padding: 0.6rem 1rem;
    border-bottom: 1px solid var(--border-pane);
    background: var(--bg-pane);
  }
  h1 {
    margin: 0;
    font-size: var(--fs-h1);
    font-weight: 600;
    letter-spacing: 0.01em;
  }
  .meta {
    display: flex;
    gap: 1.25rem;
    color: var(--fg-muted);
    font-size: var(--fs-small);
    font-family: var(--font-mono);
  }
  .meta-item {
    display: inline-flex;
    gap: 0.4rem;
    align-items: baseline;
  }
  .meta-label {
    color: var(--fg-dim);
  }
  .meta-value {
    color: var(--fg-default);
  }
  .meta-value.count {
    color: var(--accent-green);
    font-weight: 600;
  }
  .meta-value.scrub.head {
    color: var(--accent-green);
  }
  .meta-value.scrub.pinned {
    color: var(--accent-yellow);
    font-weight: 600;
  }
</style>
