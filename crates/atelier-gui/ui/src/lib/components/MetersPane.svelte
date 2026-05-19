<script lang="ts">
  // Cost + context meters. Mirror of TUI `render_cost_meter` +
  // `render_context_meter` (`crates/atelier-tui/src/lib.rs`).
  //
  //   * Cost: simple `$0.XXXX` label, no upper-bound (no meaningful
  //     denominator).
  //   * Context: known-tokens / window ratio rendered as a gauge, with
  //     an explicit "+N unknown" suffix when `unknown > 0` so a
  //     silently-underreporting meter (spec §5 contract) is visible.
  //   * Verify (v62): a small badge showing which §7 verification tier
  //     ran on the last verify pass — Tier 1 LSP (green), Tier 2
  //     tree-sitter (yellow), Tier 3 textual (orange), or "verify off"
  //     (gray) when no pass has happened yet. Surfaces dropped
  //     hallucination coverage when a higher-tier producer is
  //     unavailable.

  import { onDestroy } from 'svelte'
  import {
    verificationTierLabel,
    type VerificationStatus,
  } from '../state'

  // v60.9 B1 follow-on — overflow toast decay window. Toast renders
  // for this many milliseconds after a `ContextOverflowResolved`
  // event lands, then fades out (the state field itself stays
  // populated so a debug surface can still inspect the most recent
  // resolution after the toast disappears).
  const OVERFLOW_TOAST_MS = 5000

  type OverflowResolution = {
    resolution: string
    freed_tokens: number | null
    items_compacted: number | null
    at: number
  }

  type Props = {
    totalCostUsd: number
    knownTokens: number
    unknownTokens: number
    contextWindowTokens: number
    verificationStatus: VerificationStatus
    lastOverflowResolution: OverflowResolution | null
  }

  let {
    totalCostUsd,
    knownTokens,
    unknownTokens,
    contextWindowTokens,
    verificationStatus,
    lastOverflowResolution,
  }: Props = $props()

  // v60.9 B1 follow-on — re-render `nowMs` on a 500ms tick so the
  // derived `overflowToastVisible` flips off ~5s after the last
  // overflow without the caller having to push a separate "decay
  // expired" event.
  //
  // v60.37 B4/UI-3 — only schedule the interval when there's a toast
  // to decay. Before this fix the 500ms ticker ran for the lifetime of
  // the component regardless of `lastOverflowResolution`, burning
  // ~7200 needless rerenders per hour while the user did anything but
  // overflow.
  let nowMs = $state(Date.now())
  let tick: ReturnType<typeof setInterval> | null = null
  $effect(() => {
    if (lastOverflowResolution == null) {
      if (tick != null) {
        clearInterval(tick)
        tick = null
      }
      return
    }
    nowMs = Date.now()
    tick = setInterval(() => {
      nowMs = Date.now()
    }, 500)
    return () => {
      if (tick != null) {
        clearInterval(tick)
        tick = null
      }
    }
  })
  onDestroy(() => {
    if (tick != null) clearInterval(tick)
  })

  let overflowToastVisible = $derived(
    lastOverflowResolution != null &&
      nowMs - lastOverflowResolution.at < OVERFLOW_TOAST_MS,
  )
  let overflowLabel = $derived.by(() => {
    if (!lastOverflowResolution) return ''
    const { resolution, freed_tokens, items_compacted } = lastOverflowResolution
    if (freed_tokens != null && items_compacted != null) {
      return `${resolution} · ${items_compacted} items · ${freed_tokens} tokens`
    }
    return resolution
  })

  // Floor the denominator at 1 so the percentage is well-defined even
  // before the host publishes the real context window.
  let safeWindow = $derived(Math.max(contextWindowTokens, 1))
  let ratio = $derived(Math.min(1, Math.max(0, knownTokens / safeWindow)))
  let percent = $derived(Math.round(ratio * 1000) / 10)

  let costLabel = $derived(`$${totalCostUsd.toFixed(4)}`)

  let unknownLabel = $derived(unknownTokens > 0 ? ` (+${unknownTokens} unknown)` : '')

  // v62 — verify-pass badge. The class drives the colour cue per the
  // spec §7 tier semantics; the label string comes from the shared
  // `verificationTierLabel` so the TUI and GUI render identical copy.
  let verifyTier = $derived(verificationStatus.tier)
  let verifyLabel = $derived(verificationTierLabel(verifyTier))
  let verifyTitle = $derived(
    `verification tier · ${verificationStatus.file_count} file(s) · ` +
      `${verificationStatus.claim_count} claim(s)`,
  )
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

    <div class="meter verify">
      <div class="meter-row">
        <span class="meter-label">verify</span>
        <span
          class="badge verify-{verifyTier}"
          title={verifyTitle}
          data-testid="verify-tier-badge"
        >
          {verifyLabel}
        </span>
      </div>
    </div>

    <!-- v60.9 B1 follow-on — §1 BYOM context-overflow resolution
         toast. Visible for ~5s after a `ContextOverflowResolved`
         event; fades back out via the `nowMs` tick. The toast still
         renders as a structural element when hidden so the meter
         column doesn't reflow on every resolve; the `hidden` class
         drops opacity to zero. -->
    <div
      class="overflow-toast"
      class:hidden={!overflowToastVisible}
      role="status"
      aria-live="polite"
      data-testid="overflow-toast"
    >
      {#if lastOverflowResolution}
        <span class="meter-label">overflow</span>
        <span class="overflow-label">{overflowLabel}</span>
      {/if}
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
  /* v62 — §7 verify-pass badge. Colour family encodes the tier so
     the user can spot a "fallback to textual" run at a glance:
     green = full LSP coverage, yellow = tree-sitter (syntactic only),
     orange = textual lying-agent only, gray = no verify pass ran. */
  .badge {
    display: inline-block;
    padding: 0.05rem 0.4rem;
    border-radius: 4px;
    border: 1px solid currentColor;
    font-size: 0.65rem;
    text-transform: lowercase;
    letter-spacing: 0.04em;
    font-weight: 600;
  }
  .badge.verify-tier1_lsp {
    color: var(--accent-green, #4caf50);
  }
  .badge.verify-tier2_tree_sitter {
    color: var(--accent-yellow);
  }
  .badge.verify-tier3_textual {
    color: var(--accent-orange, #e67e22);
  }
  .badge.verify-not_run {
    color: var(--fg-dim);
  }
  /* v60.9 B1 follow-on — context-overflow resolution toast. Renders
     under the meters row; fades out via the `.hidden` class once
     `nowMs - at` exceeds the decay window. Border + accent colour
     match the cyan family used elsewhere in the meters column. */
  .overflow-toast {
    display: flex;
    gap: 0.4rem;
    align-items: baseline;
    padding: 0.2rem 0.5rem;
    border: 1px solid var(--accent-cyan, #6cc);
    border-radius: 4px;
    background: var(--bg-pane-alt);
    color: var(--accent-cyan, #6cc);
    font-size: 0.7rem;
    opacity: 1;
    transition: opacity 400ms ease;
  }
  .overflow-toast.hidden {
    opacity: 0;
    pointer-events: none;
  }
  .overflow-toast .overflow-label {
    color: var(--fg-default);
    font-weight: 600;
  }
</style>
