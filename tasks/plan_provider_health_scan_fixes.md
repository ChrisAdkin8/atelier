# Plan — fixes for the v60.102 provider-health deep-scan findings

Date: 2026-06-04. Source: targeted deep scan of the uncommitted v60.102 changes (4 parallel Explore audits, all severity-bearing findings parent-verified). Findings: 2×P1, 1×P2, 1×P3, 4 false positives (catalogued at the bottom so they aren't re-raised).

**Reviewed 2026-06-04 (same session) — the review found one P0 the scan missed and corrected three fix designs:**
- **F0 (new, fix first): self-deadlock in `snapshot_current_model`** — the tail holds the `endpoint_health` lock guard while calling `emit_endpoint_health`, which re-locks the same non-reentrant `std::sync::Mutex`. Deadlocks (or panics, platform-dependent) on the **first hydrate** whenever an OpenAI-compat default resolves. Both the deep scan (which explicitly cleared the Mutex discipline) and the v60.102 implementation missed it — because the plan's manual verify steps were never executed (see F6).
- F1b's identity comparison corrected to **model_id only**: GUI-side emissions build profiles via `skipped_for_well_known`, which hard-codes `base_url: String::new()` (model_profile.rs:446), while the Runner's emission carries the probe profile's real base_url — comparing base_url across the two paths always mismatches and would defeat F1b entirely.
- F2 extended to explicitly include the mid-stream `StreamChunk::Error` arm (it emits `ProviderFailure` but no health event even for `Unreachable` — confirmed by read).
- F5's test as drafted would fail before reaching the probe: `resolve_openai_api_key(None)` errors when `OPENAI_API_KEY` is unset (credentials.rs:64-76). The test must provide a key via serialized env-var set (the v60.91 `ENV_LOCK` pattern).
- F1a's bus-only design verified safe: `App.svelte`'s `onMount` awaits `listen(...)` **before** calling `hydrateCurrentModel()` (App.svelte:108→150), so no bus event can be lost to a not-yet-subscribed listener.

**Re-reviewed 2026-06-04 (third pass, post-amendment) — one more code bug caught, three plan defects fixed:**
- **New finding (P2, folded into F2): failure arms emit health for non-OpenAI-compat adapters.** `chat_base_url` falls back to `""` via `unwrap_or_default()` (lib.rs:2437) when the health slot is empty — an Anthropic network failure raises `AdapterError::Unreachable`, the arm emits `EndpointHealthChanged` with an empty base_url, and the L3 loop spawns probing `"/models"` forever. Violates the binding scope rule ("OpenAI-compat only; Anthropic/Mock render no dot"). F2 gains a non-empty-base_url guard.
- **Credential resolve precedes the allowlist check in `build_swap_adapter`** (lib.rs:1051-1054, verified) — the keychain/env is touched for URLs that will be rejected, and the existing `default_openai_profile_uses_swap_allowlist` test silently depends on `OPENAI_API_KEY` being set in the dev environment (it passed locally because direnv exports it). F5 gains a sub-item to reorder allowlist-first and de-env-depend the existing test.
- F0's fix snippet relied on a subtle `MutexGuard` deref-clone; made explicit.
- F1a wrongly said the `BridgedEvent` import might become unused — the bus listener still uses it; reworded.
- Noted: every hydrate re-emits health via `emit_endpoint_health`, which re-bumps the generation and restarts an in-flight re-probe loop's backoff at 30s. Acceptable at hydrate frequency (mount + workspace change); documented so the implementer doesn't "fix" it into a regression.

**Facts verified against code before writing (do not re-derive):**
- App.svelte applies events through **two paths**: the `atelier://event` bus listener (`App.svelte:108`, applies every `BridgedEvent`) and the awaited `snapshot_current_model` return (`App.svelte:155-156`). Any command that both emits an event and returns it gets that event applied **twice**.
- `snapshot_current_model` has exactly one caller (`App.svelte` `hydrateCurrentModel`); no Rust test invokes it. Changing its contract breaks nothing else.
- **The Runner emits its own `ModelProfileLoaded` mid-agent-run** (`runner.rs:2678`, via the EventSink callback → `emit_event`). This lands after `start_agent_run`'s health emit, so the reducer's unconditional reset wipes `endpointHealth` on every agent run — a third instance of the bug class the scan caught at snapshot time. **The naive fix (delete one emit line) does not cover this; the reset rule itself must change.**
- `Manager` is already imported in gui `lib.rs:37`; `app.state::<SessionState>()` is callable inside spawned closures (Tauri 2 `Manager` is implemented on `AppHandle`). The failure arms' "state is not accessible here" workaround comment is **wrong** — state is reachable; the isolated `Arc::new(AtomicU64::new(0))` counters were never necessary.
- `emit_endpoint_health` (lib.rs:~3232) already does the full correct sequence: slot write → generation bump → event emit → conditional L3 spawn with the **shared** counter.
- `SwapOptionWire.model_id` and `currentModel.modelId` both originate from the same `providers.toml` `model` field — the dropdown overlay comparison is sound (scan false-positive #1; re-pinned here because a fix touching this area might "helpfully" change it).

## Design rule (binding, carries forward)

**One delivery path per event.** A Tauri command either emits an event on the bus **or** returns it for the caller to apply — never both. The frontend listener applies every bus event; a returned-and-applied event is a double reduction. Violations of this rule caused P1-1.

## Standing gates (every PR)

- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p atelier-core -p atelier-gui` (where touched)
- `npm --prefix crates/atelier-gui/ui run check` (svelte-check, 0 errors)
- `make check`

---

## F0 — self-deadlock in `snapshot_current_model` (P0, fix first)

- File: `crates/atelier-gui/src/lib.rs` (~770-776).
- Current:
  ```rust
  if let Ok(guard) = state.endpoint_health.lock() {
      if let Some(health) = guard.as_ref() {
          emit_endpoint_health(&app, &state, health);  // re-locks endpoint_health → deadlock
      }
  }
  ```
  `emit_endpoint_health` writes the slot via `state.endpoint_health.lock()` while the caller's `guard` is still alive. `std::sync::Mutex` is non-reentrant: same-thread re-lock deadlocks or panics. Triggers on the first hydrate after a successful OpenAI-compat default resolve — the GUI hangs at startup with the dead-ELB profile *or* a healthy one.
- Fix: clone the health out of the lock, drop the guard, then emit:
  ```rust
  let health_snapshot: Option<EndpointHealth> =
      state.endpoint_health.lock().ok().and_then(|g| (*g).clone());
  if let Some(health) = health_snapshot {
      emit_endpoint_health(&app, &state, &health);
  }
  ```
  (The explicit `(*g).clone()` matters: `g.clone()` only works through a subtle `MutexGuard` deref and reads as cloning the guard — write the deref so the next reader doesn't "simplify" it into a compile error or a held-guard refactor.)
  F1a restructures this same tail; land F0's shape as part of the F1a commit but verify it as its own behaviour (the deadlock exists independent of the double-emit).
- Note: this re-emission path calls `emit_endpoint_health`, which bumps the generation and (if unreachable) replaces any in-flight re-probe loop, resetting its backoff to 30s. At hydrate frequency (mount, workspace change) this is fine — do not "optimise" it away; the bump is what guarantees at most one loop.
- **Verify:** manual — GUI must reach a painted badge with an OpenAI-compat default configured (pre-fix it hangs). This is the first manual step to run; everything else depends on the app starting.

## F1 — health-reset correctness (P1, two complementary halves)

The bug class: `ModelProfileLoaded` unconditionally resets `endpointHealth`/`contextWindowVerified`, and the event arrives on more paths than the v60.102 ordering contract anticipated (command-return double-apply at snapshot; Runner re-emission mid-agent-run). Half A removes the redundant delivery; half B makes the reset survive the deliveries that legitimately remain.

### F1a — single delivery path for `snapshot_current_model`

- File: `crates/atelier-gui/src/lib.rs` (`snapshot_current_model`, ~line 715) + `ui/src/App.svelte` (`hydrateCurrentModel`, line 153-160).
- Current: the command emits `ModelProfileLoaded` on the bus (`emit_event(&app, &mpl)`), emits `EndpointHealthChanged` on the bus, **and** returns `bridge_event(&mpl)` which App.svelte applies — the profile event is reduced twice, and the second application (command return, applied after the health event) resets the just-set health.
- Fix: bus-only delivery. Change the return type to `Result<(), String>`; keep the two ordered bus emits (`ModelProfileLoaded` then `EndpointHealthChanged` — same-channel ordering is preserved by Tauri's event queue). Update `hydrateCurrentModel` to `await invoke('snapshot_current_model')` without applying a return value. The `BridgedEvent` type import in App.svelte **stays** — the bus listener (`listen<BridgedEvent>`) still uses it; only the generic on this one `invoke` goes.
- Why bus-only rather than return-only: the health event has no return slot (one command, two events), and return-only would re-create the ordering race — the bus health event can land before the awaited return is applied, putting the reset after the health again.
- **Verify:** manual — launch GUI with the dead-ELB profile: red dot appears at mount and **stays** (pre-fix it flashes and clears). `cargo test -p atelier-gui` green (no Rust test calls the command — verified, so no test updates needed).

### F1b — identity-keyed reset in the reducer

- File: `crates/atelier-gui/ui/src/lib/state.ts` (`ModelProfileLoaded` arm).
- Current: every `ModelProfileLoaded` resets `endpointHealth = null` and `contextWindowVerified = true`. The Runner re-emits `ModelProfileLoaded` for the **same** model mid-agent-run (`runner.rs:2678`) → health wiped during every agent run, no later event restores it.
- Fix: reset only when the model identity actually changed — **model_id only** (corrected in review):
  ```ts
  const identityChanged = state.currentModel?.modelId !== p.model_id
  // reset endpointHealth / contextWindowVerified only when identityChanged
  ```
  Same-identity re-emissions (Runner probe refresh, redundant hydrates) keep existing health; a real swap still resets. **Do not compare `base_url`:** GUI-side emissions build their profile via `skipped_for_well_known`, which hard-codes `base_url: String::new()` (model_profile.rs:446), while the Runner's mid-run emission carries the probe profile's real base_url — a base_url comparison mismatches on every agent run and silently reverts to always-reset, defeating the fix. Caveat accepted: two profiles sharing a model_id across different endpoints would keep stale health for the instant between the swap's `ModelProfileLoaded` and its `EndpointHealthChanged` — the trailing health event overwrites it immediately (pinned ordering), so the residual exposure is one frame.
- Note: F1a and F1b overlap on the snapshot path but not on the agent-run path — F1b is the half that fixes the Runner re-emission; do not drop it as "redundant with F1a" during implementation.
- Also clear `recoveryToast` when `identityChanged` (a "X back online" toast must not survive a swap to Y) — folds part of F4 in here naturally.
- **Verify:** manual — dead-ELB profile, send an agent prompt: the red dot survives the run. svelte-check clean.

## F2 — failure-arm re-probe loops join the real generation guard (P1)

- Files: `crates/atelier-gui/src/lib.rs`, chat failure arm (~2546-2586), **mid-stream `StreamChunk::Error` arm (~2618-2635 — confirmed in review: it emits `ProviderFailure` but no health event even when the error is `Unreachable`; a connection dropped mid-stream is exactly as dead as one that failed up front)**, and agent failure arm (~2941-2960).
- Current: both arms hand `spawn_health_reprobe` a **fresh** `Arc::new(AtomicU64::new(0))` because the closure "can't reach state" (per the inline comment). Consequences: (a) adapter swaps never cancel these loops — they probe the dead endpoint forever at 120s cadence; (b) repeated failures stack multiple uncoordinated loops; (c) the `endpoint_health` slot is never written by the failure arms, so `snapshot_current_model`'s re-emit serves stale "reachable" health after a send-time outage.
- Fix: the premise is false — `Manager` is implemented on `AppHandle`, so inside the spawned task: `let state = app_clone.state::<SessionState>();` then call the existing `emit_endpoint_health(&app_clone, state.inner(), &health)`. This replaces the hand-rolled event construction + isolated spawn in **all three** arms with the one helper that already does slot-write → generation-bump → emit → guarded-spawn. The mid-stream arm gains a `matches!(error, AdapterError::Unreachable(_))` guard + the same helper call. Delete the stale "state is not accessible" comments. Net diff is negative.
- **Scope guard (added in re-review):** only emit health when `!base_url.is_empty()`. `chat_base_url`/`agent_base_url` fall back to `""` via `unwrap_or_default()` (lib.rs:2437) when the health slot is empty — which is exactly the Anthropic/Mock case (those adapters never write the slot). Without the guard, an Anthropic network failure emits `EndpointHealthChanged { base_url: "" }` and spawns a junk re-probe loop against `"/models"`, violating the binding scope rule (no dot for non-OpenAI-compat). Belt-and-braces: `spawn_health_reprobe` itself should early-return on an empty base_url.
- **Lock-discipline note (post-F0):** `emit_endpoint_health` locks `endpoint_health` internally — callers must never hold that lock when calling it (F0 was exactly this). When unifying the arms, ensure any base_url read from the slot is cloned-and-dropped before the helper call.
- Dedup guard: `emit_endpoint_health` bumps the generation on **every** call, which cancels any prior loop before spawning the next — repeated failures now replace the loop instead of stacking. Confirm this property holds when reading the helper; it is the mechanism, not a side effect.
- **Verify:** unit test on the generation mechanics if extractable without an `AppHandle`; otherwise: manual — kill the server, send a prompt (loop spawns), swap to Mock in the dropdown, watch the logs: the re-probe loop must stop within one backoff interval (generation mismatch). Plus careful review that both arms now route through `emit_endpoint_health`.

## F3 — retry command allowlist (P2)

- File: `crates/atelier-gui/ui/src/lib/components/ConversationPane.svelte` (`retryCard`, ~line 46).
- Current: `invoke(command, { prompt })` with `command` taken verbatim from the event payload. Producer is trusted today; the IPC boundary still deserves a guard.
- Fix:
  ```ts
  const RETRY_COMMANDS = new Set(['start_chat_run', 'start_agent_run'])
  if (!RETRY_COMMANDS.has(command)) {
    console.warn(`[atelier] refusing retry with unknown command: ${command}`)
    return
  }
  ```
  Console-warn rather than silent return — a wire-drift here should be visible (same philosophy as the v60.100 Q7 shape-guard warns).
- **Verify:** svelte-check; review.

## F4 — recovery toast lifecycle (P3)

- File: `crates/atelier-gui/ui/src/lib/state.ts` (`EndpointHealthChanged` arm) + the `identityChanged` clear from F1b.
- Current: `recoveryToast` is only ever set (on `unreachable → reachable`) or preserved; nothing in the reducer clears it. A flap back to `unreachable` leaves "X is back online" on screen next to a red dot.
- Fix: in the `EndpointHealthChanged` arm — `status === 'unreachable'` clears the toast; transition `unreachable → reachable` sets it; other reachable events preserve (don't cut a mid-animation toast short). Combined with F1b's identity-change clear, every stale-toast path is covered.
- **Verify:** svelte-check; manual flap test if convenient (kill server mid-toast).

## F5 — close the v60.102 verification gap (carried omission)

- The original plan's L1-2 verify step ("GUI-side unit test: build against an unreachable base_url, assert `EndpointHealthChanged { Unreachable }` lands after `ModelProfileLoaded`") was **not implemented** in v60.102 — the agreement tests landed, the behavioural test did not. The F1 ordering bug is exactly the class that test would have caught.
- Fix: add a Rust unit test in gui `lib.rs` that calls `build_swap_adapter` with an allowlisted-but-dead base_url (`http://127.0.0.1:<closed port>/v1`, same closed-listener trick as `probe_returns_unreachable_on_connect_failure`) and asserts the returned `EndpointHealth` is `Some` with `status: Unreachable` and `window_verified: false`. Full bus-ordering assertion needs an `AppHandle` (skip — document why); the tuple-level test pins the data the ordering depends on.
- **Credential gotcha (caught in review):** `build_swap_adapter` calls `resolve_openai_api_key(api_key)` before the probe; with `api_key: None` and no `OPENAI_API_KEY` in the environment it returns `Err` and the test never reaches the probe. The test must guarantee a key, serialized with a local `static ENV_LOCK: Mutex<()>` (the v60.91 T1 pattern — gui's test module doesn't have one yet; add it). **Set-if-absent and restore-prior-value only — never unset an existing var** (other tests in the same process may read it concurrently; only lock-takers are serialized).
- **F5b — allowlist before credentials (re-review finding):** `build_swap_adapter` resolves credentials (env/keychain touch) *before* `ensure_base_url_allowed` (lib.rs:1051-1054). Two consequences, both worth the 3-line reorder: (a) the trust boundary should reject a URL before any secret material is read for it; (b) the existing `default_openai_profile_uses_swap_allowlist` test silently depends on `OPENAI_API_KEY` being exported in the dev shell (direnv) — it asserts the error contains "allowlist" but on a clean machine the credential error fires first and the assertion fails. Reorder allowlist-first; the existing test becomes env-independent for free.
- **Timing note:** the probe inside `build_swap_adapter` is bounded by `PROBE_TIMEOUT_SECS = 4`; the closed-port connect refuses in milliseconds, so the test stays fast.
- **Verify:** the new test passes; runs without a Tauri runtime; the full gui suite passes in a shell **without** `OPENAI_API_KEY` exported (`env -u OPENAI_API_KEY cargo test -p atelier-gui`) — this also proves F5b fixed the latent env-dependence.

## F6 — lessons entries (process)

Add to `tasks/lessons.md` when the PR lands — three lessons, not one:
1. **Two-delivery-path pitfall**: a Tauri command return + bus emit both reduce on the frontend; pick one delivery path per event (the binding design rule above).
2. **Reset-on-event must be identity-keyed** when the event can recur for the same entity (the Runner re-emits `ModelProfileLoaded` mid-run; any future "reset on X" reducer arm should ask "can X arrive again without the state actually changing?").
3. **Manual verify steps are gates, not suggestions.** v60.102's plan listed "manual: launch GUI with the dead-ELB profile" as the L1-3 verify; it was never run, and the F0 deadlock — which that exact step would have caught in seconds — shipped into the working tree while every automated gate stayed green. If a manual step is skipped, the verification report must say so explicitly instead of marking the item complete.

---

## Suggested PR shape

One PR, commits in order F0+F1a → F1b → F2 → F3 → F4 → F5 (F6 rides the last commit). F0 and F1a share a commit (same code region; F0's restructure is subsumed by F1a's rewrite of the tail) but get separate verification lines. F1a and F1b are deliberately separate commits — if F1b's identity comparison causes an unexpected regression, F1a alone still fixes the snapshot path and can survive a revert of F1b.

| Item | Risk | Test surface |
|---|---|---|
| F0 deadlock | Low fix, P0 impact | manual: GUI reaches painted badge (pre-fix: hangs) |
| F1a single delivery path | Low — one caller, no test callers (verified) | manual dead-ELB mount; existing suites green |
| F1b identity-keyed reset | Low-medium — changes reset semantics for all profiles | manual agent-run dot persistence; svelte-check |
| F2 state-via-AppHandle (3 arms) + empty-base_url scope guard | Low — net-negative diff onto an existing helper | manual swap-cancels-loop; review |
| F3 allowlist | Trivial | svelte-check |
| F4 toast lifecycle | Trivial | svelte-check |
| F5 missing test (+ENV_LOCK) / F5b allowlist-before-credentials | Low | new test green; full gui suite green under `env -u OPENAI_API_KEY` |

**The manual steps above are mandatory gates this time** (see F6 lesson 3). Run them with the real GUI (`cargo tauri dev` from `crates/atelier-gui/`) before marking any item complete.

## False positives from the scan (do not re-raise)

1. `isUnreachableActive` model_id comparison — both sides sourced from `providers.toml` `model`; they match.
2. `modelBadgeTooltip` stale closure — Svelte 5 `$state` tracks property reads inside functions called from template expressions; `app.endpointHealth` is a live dependency.
3. `contextWindowVerified` with null `capabilityRow` — null row → trusted is correct; the `?` marker exists only for `source === 'adapter'` seeds.
4. The subagent's "no double-emit in snapshot_current_model" — overturned by parent verification; the double-emit is real and is F1a.
