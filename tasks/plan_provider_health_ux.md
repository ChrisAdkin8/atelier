# Plan — provider-health UX: see endpoint failure before you type, recover in one click

Date: 2026-06-04. Source: design discussion following the vLLM-ELB outage triage (SG/IP drift made the default profile unreachable; the GUI showed an empty badge, a silently-wrong 8K context cap, and an `[agent error]` string only after the first send). Builds on v60.98 (connect timeouts, probe-on-swap, mock-fallback removal) and v60.99/v60.100 (failure-visibility, fail-fast).

**The core UX flaw:** endpoint failure is discovered at send-time, expressed as an error string, with recovery left as an exercise. Three layers fix it, ordered by value: passive health status (L1), actionable failure card (L2), background re-probe with recovery toast (L3).

**Reviewed 2026-06-04 (same session) — corrections applied inline:** the `[agent error]` line is GUI-bridge-local (not on the core bus), so L2 **replaces** it with the card instead of duplicating; L2's Retry now rides a `retry {command, prompt}` payload on the event (the Rust arm knows the exact post-expansion prompt) — the shared `routing.ts` extraction is deleted; L1-2 now names the `build_swap_adapter` signature change and its three callers plus the `AppHandle` param `snapshot_current_model` needs; a `SessionState.endpoint_health` slot is added for transition detection; reducer resets health/verified state on `ModelProfileLoaded` with pinned event ordering; the L3 re-probe parses `max_model_len` from the same response so the stale 8K cap recovers with the dot; any HTTP response (including 401) counts as reachable; L2's cause helper gets both `AdapterError` and `RunError` entry points.

**Implemented 2026-06-04 (v60.102):** all three layers shipped in one session. Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, all tests, svelte-check 0 errors, `make check` 180/180.

**Final review (second pass, same day):** `max_model_len: Option<u32>` added to the health event (L3's cap recovery had nowhere to carry the value); the unverified-cap rule corrected to `source === 'adapter' && !window_verified` (the naive rule put a spurious `?` on every static-table model, e.g. OpenAI proper); `EndpointHealthStatus::Unknown` dropped (never emitted — frontend `null` is the unknown state); L2-3's `SwapOptionWire.reachable` wire field dropped (the same paragraph said the frontend overlays from L1 state, so the field was dead on arrival); L2-1's failure arm explicitly writes the `endpoint_health` slot (else a send-time-discovered outage never arms L3).

**Facts verified against code before writing (do not re-derive):**
- `build_swap_adapter` discards the probe outcome — `let _ = tokio::time::timeout(…, adapter.probe_context_window())` (`atelier-gui/src/lib.rs:~1021`); `probe_context_window` returns `()` and only logs internally (`openai_compat.rs:253`).
- `CapabilityRowSource::Probe` does **not** mean "probe verified the window" — per its doc it fires only when a probe observation produced a `ClaimedButBroken` cell (`capability_matrix.rs:57-67`). The unverified-cap marker therefore needs new state; it cannot ride `source`.
- `AdapterError` already classifies the causes L2 needs: `Auth`, `Unreachable`, `Malformed`, `RateLimited`, `Provider{status,body}`, `ContextOverflow`, not-configured (`adapter/mod.rs:251`).
- The v60.80 auto-memory path exists end-to-end: `auto_card_for_error(&AdapterError) -> Option<AutoCard>` (gui lib.rs:1471), `emit_auto_memory_card` (lib.rs:1603) writes the card and emits `MemoryCards`. The card body already contains the "Likely fix" text L2 wants to surface inline.
- `lastOverflowResolution` (the "toast precedent") is stashed in `state.ts:609` but **never rendered** — no consumer exists outside state.ts. L3 builds the toast render fresh. (L-D-7: surface was claimed, wire was cut.)
- The failure-path system line is `MessageCommitted { role: System, text: "[agent error] {e}" }` at gui lib.rs:~2730 (agent) and ~2431 (chat); both already call `emit_auto_memory_card` adjacently. These emissions go through `emit_event(&app, …)` — the **Tauri bridge, GUI-local** — not the core broadcast bus; the TUI never sees them, so the GUI may replace the line freely (L2-1).
- `SwapOptionWire` (lib.rs:~640) carries `kind/model_id/label/base_url/is_default` — room for a `reachable` hint exists; the dropdown renders `★` for default already.
- Composer history (v60.97) retains the last submitted prompt, but only after `invoke()` resolves — an ordering invariant L2's Retry must NOT depend on; Retry instead rides an explicit `retry_command`/`retry_prompt` payload on the failure event (see L2-1).

## Design constraints (binding)

1. **No automatic fallback to another profile, ever.** Same trap as the Mock fallback removed in v60.98 and the silent executor-routing fallback removed in Q3 (v60.100): the user asked for a specific model; silently substituting another is worse than a clear failure. Any fallback is a **one-click offer** the user accepts explicitly.
2. **Health state must never gate the run path.** A stale "unreachable" verdict must not block a send — the user may have just fixed the server. Health is advisory UI; the adapter's own connect timeout remains the enforcement.
3. **New cross-boundary enums get an agreement test in the first commit** (L-D-5): `wire_label()` == serde projection for any new event kind or status enum.
4. **Scope: OpenAI-compat profiles only for v1.** Anthropic has no cheap unauthenticated health endpoint; Mock is always healthy. Both render "no dot" (not grey "unknown" — absence of claim, not unknown claim).

## Standing gates (every PR)

- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p atelier-core -p atelier-cli -p atelier-gui` (where touched)
- `npm --prefix crates/atelier-gui/ui run check` (svelte-check, 0 errors)
- `make check`

---

## Layer 1 — passive health status on the model badge + unverified-cap marker (PR-1)

### L1-1 — `probe_context_window` returns an outcome instead of `()`

- File: `crates/atelier-core/src/adapter/openai_compat.rs:253`.
- Current: every exit path (`GET failed`, non-2xx, JSON parse fail, missing `data`, missing `max_model_len`, success) logs at `debug`/`info` and returns `()`. The caller can't distinguish "verified 32k" from "endpoint dead, kept the 8,192 seed".
- Fix: return a `ProbeOutcome` enum from the same function:
  ```rust
  pub enum ProbeOutcome {
      /// /v1/models answered and carried max_model_len — window verified.
      WindowVerified { tokens: u32 },
      /// /v1/models answered but had no max_model_len (OpenAI proper,
      /// older llama.cpp). Endpoint is REACHABLE; window stays unverified.
      ReachableNoWindow,
      /// Connect/request failed — endpoint unreachable.
      Unreachable { error: String },
      /// Reachable but the response was malformed (non-2xx, bad JSON).
      MalformedResponse { detail: String },
  }
  ```
  All four arms map 1:1 onto the existing exit paths — this is a return-value change, not a logic change. There is exactly **one** caller today (`atelier-gui/src/lib.rs:1023`; verified by grep — despite the method's doc comment claiming "atelier-gui and atelier-cli invoke this", the CLI never calls it). Fix that stale doc comment in the same commit. Don't add `#[must_use]` — the enum is informational and the single caller consumes it anyway.
- Side-note (out of scope, one line in the plan for honesty): because the CLI never probes, the TUI/CLI context meter shows the 8,192 seed for every OpenAI-compat endpoint. A follow-on could call the probe from the CLI build path; not this plan.
- **Verify:** extend the existing probe unit tests (openai_compat.rs has reachable/missing-field/error-path coverage around lines 1277-1414) to assert the returned variant per path.

### L1-2 — `EndpointHealth` state + bus event

- Files: `crates/atelier-core/src/session.rs` (event), `crates/atelier-gui/src/lib.rs` (emission + state).
- Fix, in four named pieces (the original draft hid the plumbing):
  1. Core event: `Event::EndpointHealthChanged { model_id: String, base_url: String, status: EndpointHealthStatus, checked_at: String, window_verified: bool, max_model_len: Option<u32>, error: Option<String> }` with `EndpointHealthStatus::{Reachable, Unreachable}` + `wire_label()`. Two variants only — `Unknown` was in the first draft but no site ever emits it (Mock/Anthropic emit nothing; frontend `null` is the unknown state). Mapping from L1-1: `WindowVerified | ReachableNoWindow | MalformedResponse → Reachable` (malformed is reachable-but-suspect; carry the detail in `error` rather than inventing a third UI state); `Unreachable → Unreachable`. `window_verified` is true only for `WindowVerified`, and `max_model_len` carries the verified value (`Some(tokens)` for `WindowVerified`, `None` otherwise) — L3's cap recovery rides this field; without it the recovery event has nowhere to put the re-probed window.
  2. Signature change: `build_swap_adapter` returns `Result<(Arc<dyn Adapter>, Option<EndpointHealth>), String>` (None for Mock/Anthropic). **Three callers** thread it (verified): `swap_adapter` (lib.rs:1161), `resolve_default_adapter` (lib.rs:2183 — propagates the tuple to its own callers), `resolve_executor_adapter` (lib.rs:2230 — **discards** it; executor health is out of scope, note the discard explicitly with a comment).
  3. Storage: `SessionState` gains `endpoint_health: Mutex<Option<EndpointHealth>>`. Every command-level site that builds/swaps an adapter writes it. This is what L3's transition detection and the dropdown overlay read — without it there is no "previous status" to compare against.
  4. Emission: `swap_adapter`, `start_chat_run`, `start_agent_run` already own an `AppHandle` and emit after the build. `snapshot_current_model` does **not** take one — add the `app: AppHandle` parameter (Tauri commands accept it freely).
- **Event ordering (pinned):** emit `EndpointHealthChanged` **after** `ModelProfileLoaded` at every site. The reducer resets health state on `ModelProfileLoaded` (L1-3), so health-then-profile would self-clobber.
- Wire-format hygiene: agreement test asserting `EndpointHealthStatus::wire_label()` matches the serde projection, same shape as the v60 batch (L-D-5).
- TUI: no rendering this cycle; its `apply()` default arm must tolerate the unknown event (it does — verify with the existing unknown-event test if one exists, else note).
- **Verify:** GUI-side unit test: build against an unreachable base_url (port 1 on localhost), assert an `EndpointHealthChanged { status: Unreachable }` lands on the bridge after the `ModelProfileLoaded`.

### L1-3 — frontend health dot + tooltip

- Files: `crates/atelier-gui/ui/src/lib/state.ts`, `App.svelte`.
- Fix: `state.ts` gains `endpointHealth: { status: 'reachable' | 'unreachable', checkedAt: number, error: string | null } | null` (`null` = no claim — Mock/Anthropic/unchecked), reduced from the new event (non-modal — plain `as` cast per the existing convention, castPayload not required). **Reset rule:** the `ModelProfileLoaded` arm sets `endpointHealth = null` (and `contextWindowVerified = true`, see L1-4) so health from a previous profile can't survive a swap — the dead-vLLM red dot must not linger after switching to Anthropic. The pinned emission order in L1-2 guarantees the fresh health event lands after the reset. `App.svelte`'s model badge gains a dot before the fit label: green (reachable), red (unreachable), nothing (null / non-openai-compat). Badge tooltip (`modelBadgeTooltip`) appends `endpoint: unreachable — connect timeout (checked 12s ago)` when health is present.
- "Checked Ns ago" needs a ticking clock: reuse the elapsed-time pattern the overflow toast *intended* (a 1s `setInterval` in the footer scope, cleared on destroy) — or simply render absolute time (`checked 14:32:05`) and skip the timer. **Recommend absolute time** — no timer lifecycle, no re-render churn; the freshness signal L3 needs comes from re-probe updates anyway.
- **Verify:** svelte-check clean; manual: launch GUI with the dead-ELB profile → red dot + tooltip cause visible before any prompt is sent.

### L1-4 — unverified context-cap marker

- Files: `state.ts`, `App.svelte` footer meter.
- Current: `contextWindowTokens` falls back to the 8,192 seed (or the 200k default) with no provenance; the meter renders `0 / 8,192 (0%)` looking authoritative.
- Fix: `state.ts` gains `contextWindowVerified: boolean`. **The rule (corrected in final review):** unverified **iff** `capability_row.source === 'adapter' && !window_verified`. The naive rule (`!window_verified` alone) would put a spurious `?` on every static-table model: an OpenAI-compat profile pointing at OpenAI proper takes its 128k window from the **static capability table** (trusted), while the probe legitimately returns `ReachableNoWindow` — table-derived numbers must render clean. `capability_row.source` (`static` / `adapter` / `probe`) is already on `currentModel`; only `adapter`-sourced windows are the 8,192 seed the marker exists to flag (`probe`-sourced rows are static-table-crossed, also trusted).
- Mechanics: reset `contextWindowVerified = true` on `ModelProfileLoaded` (L1-3's reset rule); the trailing `EndpointHealthChanged` applies the rule above using its `window_verified` field plus the already-reduced `currentModel.capabilityRow.source`. Meter renders the denominator with a trailing `?` and `opacity: 0.6` when unverified, tooltip gains `cap unverified — endpoint did not report max_model_len`.
- **Verify:** svelte-check; manual: dead endpoint shows `0 / 8,192? (0%)` dimmed; live vLLM shows `0 / 32,768 (0%)` normal.

> L1 sequencing: L1-1 → L1-2 → L1-3/L1-4 (3 and 4 are independent of each other).

---

## Layer 2 — actionable failure card in the conversation (PR-2)

### L2-1 — structured `ProviderFailureCard` event

- Files: `crates/atelier-core/src/session.rs`, `crates/atelier-gui/src/lib.rs` (chat ~2431 and agent ~2746 failure arms).
- Current: failure surfaces as `MessageCommitted { role: System, text: "[agent error] {e}" }` — unstructured, no recovery affordance. The adjacent `auto_card_for_error` call already classifies the `AdapterError` and drafts the memory card with a "Likely fix" body.
- **Replace, don't duplicate (corrected in review):** the `[agent error]` `MessageCommitted` is emitted via `emit_event(&app, …)` — the Tauri bridge, **GUI-local** — not the core broadcast bus; the TUI never sees it. So the GUI failure arms emit `ProviderFailure` **instead of** the string line. The event-log pane still records the failure via the generic events append; nothing else consumed the string.
- Fix: add `Event::ProviderFailure { model_id, base_url, cause: ProviderFailureCause, message: String, fix_hint: Option<String>, memory_card_path: Option<String>, retry_command: String, retry_prompt: String }`. `ProviderFailureCause::{Unreachable, Auth, RateLimited, Malformed, NotConfigured, Other}`. The classification helper needs **two entry points**, mirroring the existing pair: from `&AdapterError` (chat path) and from `&RunError` (agent path — `RunError::AdapterChain` exposes the typed source; the legacy `Adapter(String)` variant falls back to string matching, which is exactly why `auto_card_for_run_error` exists separately at lib.rs:1572). Refactor both `auto_card_for_*` fns and the new cause mapping around one shared match so the three can't drift.
- **Retry payload (corrected in review):** `retry_command` (`"start_chat_run"` / `"start_agent_run"`) and `retry_prompt` (the exact prompt the failed run used — post-skill-expansion, since `invoke_skill` expands GUI-side before calling `start_agent_run`). The Rust failure arm has both in scope. This makes the frontend Retry a single `invoke(retry_command, { prompt: retry_prompt })` — no history-tail coupling, no re-routing, no skills-list dependency. Prompts are ≤64KB by the existing cap; acceptable on the bridge.
- `fix_hint` carries the AutoCard body (the "Likely fix" text); `memory_card_path` the written card path.
- Agreement test for `ProviderFailureCause::wire_label()` (L-D-5).
- Also emit `EndpointHealthChanged { Unreachable }` here when the cause is `Unreachable` — a send-time failure is the freshest health signal there is, and it arms L3's re-probe loop. **This arm writes the `SessionState.endpoint_health` slot too** (L1-2 listed only build/swap sites as writers; L3's transition detection reads the slot, so the failure arm must keep it current or a send-time-discovered outage never spawns the re-probe loop).
- **Verify:** GUI unit test: drive the chat error arm with `AdapterError::Unreachable`, assert the `ProviderFailure` bridge event (with non-empty `fix_hint` and correct `retry_*`) and assert the `[agent error]` `MessageCommitted` is **no longer** emitted.

### L2-2 — ConversationPane failure card render

- Files: `ui/src/lib/state.ts`, `ui/src/lib/components/ConversationPane.svelte`, `Composer.svelte` or `App.svelte` for the retry plumbing.
- Fix: reducer appends a `failureCard` conversation entry (a new line-kind in the conversation model, alongside text lines). ConversationPane renders it as a bordered card (reuse `.agent-warning` styling family from Composer):
  - title: `provider unreachable — qwen2.5-72b-awq`
  - body: cause + base_url + `fix_hint` (rendered as the markdown it already is, through the existing InlineRenderers path)
  - footnote when `memory_card_path` present: `saved to memory — see MemoryPane`
  - **Retry** button: `invoke(card.retry_command, { prompt: card.retry_prompt })` — the payload carried on the event (L2-1). No history-tail read, no shared routing module (the earlier draft's `routing.ts` extraction is deleted: the history-tail approach depended on the undocumented ordering invariant that `pushHistory` fires before the async failure event, and re-routing raw `/slash` text would have needed the skills list). Disable the button while `busy` (the existing run-in-flight guard rejects the invoke anyway; disabling avoids a confusing rejection toast).
  - **Switch profile** button: focuses/opens the footer's existing `swap-select` (`data-testid="swap-adapter-select"`) — an App-level callback prop threaded to ConversationPane; no new dropdown.
- Cards are conversation history — they scroll away naturally; no dismissal state needed.
- **Verify:** svelte-check; manual: kill the local server, send a prompt, observe the card with working Retry (after restarting the server) and Switch (opens dropdown).

### L2-3 — reachable hint in the swap dropdown (small, optional within PR-2)

- Files: `App.svelte` only (corrected in final review: the first draft added `reachable: Option<bool>` to `SwapOptionWire` *and* said the frontend overlays from L1 state — the wire field would never be read; dropped).
- Fix: pure frontend overlay — render `⚠` next to the dropdown option whose `model_id`/`base_url` matches the active profile when `endpointHealth.status === 'unreachable'`. No Rust change, no probing in `list_provider_profiles`. Full multi-profile health sweep is **out of scope** (it would fire N network probes on every dropdown open; revisit only if users ask).
- **Verify:** svelte-check; review.

---

## Layer 3 — background re-probe + recovery toast (PR-3)

### L3-1 — re-probe loop on the Rust side

- File: `crates/atelier-gui/src/lib.rs`.
- Fix: when health transitions to `Unreachable` (detected against the `SessionState.endpoint_health` slot from L1-2), spawn a `tauri::async_runtime::spawn` loop: probe `GET {base_url}/models` with the 4s bound, backoff 30s → 60s → 120s (cap), emit `EndpointHealthChanged` after every attempt (keeps `checked_at` fresh), stop when (a) status flips to `Reachable`, (b) the adapter is swapped (generation guard), or (c) the app exits.
- **Reachability rule:** any HTTP response counts as `Reachable` — including 401/403/5xx. A vLLM behind `--api-key` answers 401 to the bare probe; the endpoint is alive and the red dot must clear. Only connect/DNS/timeout failures are `Unreachable`. (Auth problems surface at send-time as a `ProviderFailure { cause: Auth }` card — a different problem with a different fix.)
- **Cap recovery rides the same response:** when the 200-path body carries `max_model_len`, parse it and set `window_verified: true` + the value on the health event; the reducer updates `contextWindowTokens` + clears the `?` marker. Without this, recovery flips the dot green while the meter still shows the stale `8,192?`. **Known limitation (accepted):** the *adapter-internal* `capabilities().context_window_tokens` stays at the seed until the adapter is rebuilt (probe needs `&mut`, the live adapter is shared `Arc`); this affects suitability scoring inputs, not the meter. A "rebuild same profile on recovery" follow-on can close it — same-profile rebuild is not a fallback and doesn't violate the design constraints, but it's scope creep here.
- Concurrency guard: a `health_probe_generation: Arc<AtomicU64>` in `SessionState`; the loop captures its generation at spawn and exits when the global has moved on (bumped by every `swap_adapter` / default-resolve). Prevents two loops after swap-to-another-then-back. This is the same drop-guard discipline as `RunCleanup`.
- The loop probes with a bare reqwest client (connect_timeout 4s), NOT through the live adapter — the adapter is `Arc<dyn Adapter>` shared with possible in-flight runs and `probe_context_window` needs `&mut self`. No bearer header needed given the reachability rule above.
- **Verify:** unit test with a generation bump asserting loop exit; unit test asserting a 401 response maps to `Reachable`; manual: kill server → red dot; restart server → dot flips green within ~30s and the meter cap recovers.

### L3-2 — recovery toast

- Files: `ui/src/lib/state.ts`, `App.svelte`.
- Current: there is **no existing toast renderer** — `lastOverflowResolution` (state.ts:609) was built for one but nothing consumes it. Build the render fresh; design it so the overflow toast can adopt it later (don't wire overflow now — separate concern).
- Fix: reducer tracks the previous health status; on `unreachable → reachable` transition set `recoveryToast: { text: "qwen2.5-72b-awq is back online", at: Date.now() }`. App.svelte renders a fixed-position toast above the footer, fading after ~6s (CSS animation, no JS timer needed — `animation: toast-fade 6s forwards` + remove on `animationend`).
- Respect `prefers-reduced-motion` (drop the fade, keep visibility + manual dismiss on click).
- **Verify:** svelte-check; manual end-to-end with server kill/restart.

---

## Suggested PR shape

| PR | Items | Risk | Test surface |
|---|---|---|---|
| PR-1 | L1-1 … L1-4 (health dot + unverified cap) | Low — additive return value, one new event, CSS | probe-outcome unit tests, wire agreement test, unreachable-build test |
| PR-2 | L2-1 … L2-3 (failure card) | Medium — new conversation line kind touches ConversationPane render path | failure-arm unit test, agreement test, manual retry/switch |
| PR-3 | L3-1 … L3-2 (re-probe + toast) | Low-medium — background task lifecycle | generation-guard test, manual kill/restart cycle |

Land strictly in order: L2 consumes L1's event; L3 consumes both. If only one PR lands, PR-1 converts "discover at send-time" into "see at a glance" — that is most of the value.

## Out of scope (decided, not deferred-by-accident)

- Multi-profile health sweep on dropdown open (N probes per open; no demand yet).
- TUI health rendering (TUI's footer has the model badge but no health state; do after PR-1 proves the event shape).
- Anthropic endpoint health (no cheap unauthenticated check; a HEAD to api.anthropic.com tells you about the network, not your key).
- Wiring the orphaned `lastOverflowResolution` toast (pre-existing L-D-7 case; tracked here as a note, fix alongside L3-2 only if trivial).
- Auto-fallback of any kind (see Design constraints — binding).
