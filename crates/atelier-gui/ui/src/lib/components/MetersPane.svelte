<script lang="ts">
  // Cost + context meters. Mirror of TUI `render_cost_meter` +
  // `render_context_meter` (`crates/atelier-tui/src/lib.rs`).
  //
  //   * Cost: simple `$0.XXXX` label, no upper-bound (no meaningful
  //     denominator).
  //   * Context: known-tokens / window ratio rendered as a gauge, with
  //     an explicit "+N unknown" suffix when `unknown > 0` so a
  //     silently-underreporting meter (spec §5 contract) is visible.

  type Props = {
    totalCostUsd: number
    knownTokens: number
    unknownTokens: number
    contextWindowTokens: number
  }

  let { totalCostUsd, knownTokens, unknownTokens, contextWindowTokens }: Props =
    $props()

  // Floor the denominator at 1 so the percentage is well-defined even
  // before the host publishes the real context window.
  let safeWindow = $derived(Math.max(contextWindowTokens, 1))
  let ratio = $derived(Math.min(1, Math.max(0, knownTokens / safeWindow)))
  let percent = $derived(Math.round(ratio * 1000) / 10)

  let costLabel = $derived(`$${totalCostUsd.toFixed(4)}`)

  let unknownLabel = $derived(unknownTokens > 0 ? ` (+${unknownTokens} unknown)` : '')
</script>

<section class="pane">
  <header class="pane-title">meters</header>
  <div class="body">
    <div class="meter cost">
      <div class="meter-row">
        <span class="meter-label">cost</span>
        <span class="meter-value cost">{costLabel}</span>
      </div>
    </div>

    <div class="meter context">
      <div class="meter-row">
        <span class="meter-label">ctx</span>
        <span class="meter-value">
          {knownTokens}/{contextWindowTokens}<span class="unknown">{unknownLabel}</span>
        </span>
      </div>
      <div class="bar" role="progressbar"
        aria-valuemin="0"
        aria-valuemax={contextWindowTokens}
        aria-valuenow={knownTokens}>
        <div class="bar-fill" style:width="{percent}%"></div>
      </div>
    </div>
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
  .body {
    padding: 0.75rem 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    font-family: var(--font-mono);
    font-size: var(--fs-small);
  }
  .meter-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: 0.3rem;
  }
  .meter-label {
    color: var(--fg-dim);
  }
  .meter-value {
    color: var(--fg-default);
  }
  .meter-value.cost {
    color: var(--accent-yellow);
    font-weight: 600;
  }
  .unknown {
    color: var(--accent-yellow);
  }
  .bar {
    width: 100%;
    height: 8px;
    background: var(--bg-pane-alt);
    border: 1px solid var(--border-pane);
    border-radius: 4px;
    overflow: hidden;
  }
  .bar-fill {
    height: 100%;
    background: var(--accent-cyan);
    transition: width 120ms linear;
  }
</style>
