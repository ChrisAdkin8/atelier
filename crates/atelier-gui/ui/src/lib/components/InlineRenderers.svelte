<script lang="ts">
  // Phase C close — inline fenced-block + image renderers for the
  // §3 diff / §5 memory panes.
  //
  // Detects three patterns in arbitrary text and renders them inline
  // alongside the surrounding plaintext:
  //
  //   1. ```mermaid …``` — renders via the `mermaid` npm package's
  //      `mermaid.render()` API into a `<div>`.
  //   2. ```d2 …``` — textual "render-not-available" placeholder
  //      with the source preserved as a `<pre>` (no D2 npm package
  //      we can rely on; the spec accepts this as polish v0).
  //   3. Bare image URLs / repo-relative image paths (.png/.jpg/.svg/
  //      .gif). Tauri-routed via `convertFileSrc()` for repo files;
  //      absolute http(s) URLs pass through. The detector is
  //      conservative — only triggers on a line whose entire content
  //      is the image path so prose with embedded extensions doesn't
  //      false-positive.
  //
  // Minimum viable: no full markdown parser; no syntax highlighting;
  // no D2 rendering. Polish layer, not a content viewer.

  import { onMount } from 'svelte'
  import { convertFileSrc } from '@tauri-apps/api/core'

  // v60.30 — Initialise mermaid exactly once at module load, with
  // `securityLevel: 'strict'` so user-controlled diagram source can't
  // execute scripts or include foreign HTML in the rendered SVG. The
  // top-level await is fine because Vite resolves the dynamic import
  // lazily; this runs only once per session and is awaited inside
  // `renderMermaid` before any `mermaid.render()` call.
  let mermaidInitPromise: Promise<typeof import('mermaid').default> | null = null
  async function getMermaid(): Promise<typeof import('mermaid').default> {
    if (mermaidInitPromise) return mermaidInitPromise
    mermaidInitPromise = (async () => {
      const mod = await import('mermaid')
      const m = mod.default
      m.initialize({
        startOnLoad: false,
        securityLevel: 'strict',
        theme: 'dark',
      })
      return m
    })()
    return mermaidInitPromise
  }

  // v60.30 — DOM-id escape: when interpolating an attacker-controllable
  // id into the DOM-query selector below we must reject anything
  // outside [A-Za-z0-9_-]. The blockIdBase is caller-controlled (could
  // come from event-loop turn ids etc.); be conservative.
  function safeDomId(s: string): string {
    return s.replace(/[^A-Za-z0-9_-]/g, '_')
  }

  // v60.30 — image-path normaliser allow-list. Used by resolveImageSrc.
  const IMG_EXT = /\.(png|jpg|jpeg|gif|svg|webp)$/i

  type Props = {
    text: string
    /// Used by mermaid to scope `<g id="…">` so concurrent diagrams
    /// don't collide. Caller should pass a stable per-block id.
    blockId: string
  }
  let { text, blockId }: Props = $props()

  type Block =
    | { kind: 'prose'; text: string }
    | { kind: 'mermaid'; source: string; id: string }
    | { kind: 'd2'; source: string }
    | { kind: 'image'; src: string; raw: string }

  /// Parse `text` into an ordered list of blocks. Pure function —
  /// exercisable from a unit test without booting mermaid.
  export function parseBlocks(text: string, blockIdBase: string): Block[] {
    const out: Block[] = []
    if (!text) return out

    // Fenced-block regex: ``` followed by an infostring on its own
    // line, then content, then ``` on its own line. Non-greedy so
    // sequential blocks don't merge.
    const fence = /```([a-zA-Z0-9_-]+)\n([\s\S]*?)\n```/g
    let lastEnd = 0
    let m: RegExpExecArray | null
    let fenceIdx = 0
    while ((m = fence.exec(text)) !== null) {
      const [whole, lang, source] = m
      const start = m.index
      // Anything before this fence is prose (further split for images).
      if (start > lastEnd) {
        const proseChunk = text.slice(lastEnd, start)
        out.push(...splitProseForImages(proseChunk))
      }
      const lower = lang.toLowerCase()
      if (lower === 'mermaid') {
        out.push({
          kind: 'mermaid',
          source,
          id: safeDomId(`${blockIdBase}-mer-${fenceIdx}`),
        })
      } else if (lower === 'd2') {
        out.push({ kind: 'd2', source })
      } else {
        // Other languages stay as prose (the calling pane has its
        // own monospace renderer for the raw text).
        out.push({ kind: 'prose', text: whole })
      }
      lastEnd = start + whole.length
      fenceIdx += 1
    }
    if (lastEnd < text.length) {
      out.push(...splitProseForImages(text.slice(lastEnd)))
    }
    return out
  }

  // v60.30 — recognise markdown image form on a whole line. We don't
  // have a full markdown AST in scope; the regex matches
  // `![alt](path)` where the path ends in an allow-listed image
  // extension. Bare filename heuristics are no longer accepted —
  // `report.png` on its own line is now plain prose.
  const MD_IMAGE_LINE = /^!\[([^\]]*)\]\(([^)\s]+)\)$/

  function matchImageLine(s: string): { alt: string; path: string } | null {
    if (!s) return null
    const m = s.match(MD_IMAGE_LINE)
    if (!m) return null
    const path = m[2]
    if (!IMG_EXT.test(path)) return null
    return { alt: m[1], path }
  }

  function splitProseForImages(text: string): Block[] {
    const out: Block[] = []
    const lines = text.split('\n')
    let buf: string[] = []
    const flushBuf = () => {
      if (buf.length) {
        out.push({ kind: 'prose', text: buf.join('\n') })
        buf = []
      }
    }
    for (const line of lines) {
      const trimmed = line.trim()
      const img = matchImageLine(trimmed)
      if (img) {
        flushBuf()
        out.push({
          kind: 'image',
          src: resolveImageSrc(img.path),
          raw: img.alt || img.path,
        })
      } else {
        buf.push(line)
      }
    }
    flushBuf()
    return out
  }

  // v60.30 — Stricter image source resolver.
  //
  //   * Reject paths containing `..` (directory traversal).
  //   * Reject absolute filesystem paths (`/etc/passwd`).
  //   * Require a known image extension.
  //   * `http(s)`/`data:` URLs pass through unchanged so external
  //     references still work.
  //   * Repo-relative paths are converted via Tauri's asset protocol.
  //
  // Anything that fails the allow-list returns an empty string so the
  // <img> renders broken instead of silently fetching attacker content.
  export function resolveImageSrc(s: string): string {
    if (!s) return ''
    if (/^https?:\/\//.test(s) || s.startsWith('data:')) return s
    if (s.includes('..')) return ''
    if (s.startsWith('/')) return ''
    if (!IMG_EXT.test(s)) return ''
    try {
      return convertFileSrc(s)
    } catch {
      return ''
    }
  }

  let containerEl: HTMLDivElement | null = $state(null)

  // Re-render mermaid blocks on every text change. We re-import the
  // module lazily so a session that never opens an inline diagram
  // doesn't pay the mermaid bundle cost.
  let blocks = $derived(parseBlocks(text, blockId))

  $effect(() => {
    if (!containerEl) return
    const mermaidBlocks = blocks.filter(
      (b): b is Extract<Block, { kind: 'mermaid' }> => b.kind === 'mermaid',
    )
    if (mermaidBlocks.length === 0) return
    void renderMermaid(mermaidBlocks)
  })

  async function renderMermaid(
    items: Extract<Block, { kind: 'mermaid' }>[],
  ): Promise<void> {
    let mermaid: typeof import('mermaid').default
    try {
      mermaid = await getMermaid()
    } catch (e) {
      console.warn('mermaid import failed:', e)
      return
    }
    for (const item of items) {
      const safeId = safeDomId(item.id)
      const target = containerEl?.querySelector<HTMLDivElement>(
        `[data-mermaid-target="${safeId}"]`,
      )
      if (!target) continue
      try {
        const { svg } = await mermaid.render(`${safeId}-svg`, item.source)
        // v60.30 — never assign mermaid output via innerHTML even
        // though `securityLevel: 'strict'` already strips scripts.
        // Parse into a detached document, walk to the <svg> root,
        // and append the parsed node into the target. This rejects
        // any sibling/wrapper HTML the strict-mode policy missed.
        target.replaceChildren()
        const parser = new DOMParser()
        const doc = parser.parseFromString(svg, 'image/svg+xml')
        const svgEl = doc.documentElement
        if (svgEl && svgEl.tagName.toLowerCase() === 'svg') {
          target.appendChild(document.importNode(svgEl, true))
        } else {
          // Parse error: show the raw mermaid render error path
          // rather than injecting unsanitised text.
          const pre = document.createElement('pre')
          pre.className = 'mermaid-error'
          pre.textContent = 'mermaid render produced no <svg>'
          target.appendChild(pre)
        }
      } catch (e) {
        // Build the error node via DOM APIs (textContent) so the
        // exception string can't smuggle markup.
        target.replaceChildren()
        const pre = document.createElement('pre')
        pre.className = 'mermaid-error'
        pre.textContent = `mermaid render failed: ${String(e)}`
        target.appendChild(pre)
      }
    }
  }

</script>

<div class="inline-renderers" bind:this={containerEl}>
  {#each blocks as block, i (i)}
    {#if block.kind === 'prose'}
      <pre class="prose">{block.text}</pre>
    {:else if block.kind === 'mermaid'}
      <div class="mermaid-block" data-mermaid-target={block.id}>
        <span class="hint">rendering mermaid diagram…</span>
      </div>
    {:else if block.kind === 'd2'}
      <div class="d2-block">
        <span class="hint">D2 render not available (showing source)</span>
        <pre class="d2-source">{block.source}</pre>
      </div>
    {:else if block.kind === 'image'}
      <div class="image-block">
        <img src={block.src} alt={block.raw} loading="lazy" />
        <span class="caption">{block.raw}</span>
      </div>
    {/if}
  {/each}
</div>

<style>
  .inline-renderers {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .prose {
    margin: 0;
    white-space: pre-wrap;
    font-family: inherit;
    font-size: inherit;
    color: inherit;
  }
  .mermaid-block,
  .d2-block,
  .image-block {
    border: 1px dotted var(--border-pane);
    padding: 0.4rem;
    border-radius: 3px;
    background: rgba(255, 255, 255, 0.02);
  }
  .mermaid-block :global(svg) {
    max-width: 100%;
    height: auto;
  }
  .hint {
    color: var(--fg-dim);
    font-style: italic;
    font-size: 0.7rem;
  }
  .d2-source {
    margin: 0.3rem 0 0 0;
    color: var(--fg-dim);
    font-size: 0.72rem;
    white-space: pre-wrap;
  }
  .image-block img {
    max-width: 100%;
    height: auto;
    display: block;
  }
  .caption {
    display: block;
    margin-top: 0.2rem;
    color: var(--fg-dim);
    font-size: 0.68rem;
    font-style: italic;
  }
</style>
