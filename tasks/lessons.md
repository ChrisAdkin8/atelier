# Atelier — per-project process lessons

Per the host-harness self-improvement loop: after corrections, capture the failure mode + the prevention rule. Per-project lessons live here; cross-project lessons go to `~/.atelier/memory/feedback_*.md` (cross-machine) or `.atelier/memory/feedback_*.md` (project-scoped).

This file is volatile — entries are pruned when the underlying class of mistake stops happening.

---

## v50 — OpenAI-compatible adapter

### Anthropic ≠ OpenAI for tool-call argument encoding

**Failure**: First `openai_compat.rs` draft tried to send tool-call `function.arguments` as a `serde_json::Value` (Anthropic's `tool_use.input` shape). Wire format requires a JSON-encoded **string**.

**Prevention**: When porting an adapter from one provider to another, write a wiremock test that asserts the *exact request body shape* the server expects (`assert_eq!` against a captured fixture, not just "200 OK"). The tool-call round-trip is the highest-fidelity test — if it doesn't match byte-for-byte, multi-turn flows silently corrupt.

### SSE parsers must be `\r\n` / `\n` / `\r` tolerant

**Failure**: `OpenAiSseSource` initially split on `\n` only. Some providers (and some `curl --no-buffer` reverse proxies) emit `\r\n` line terminators; lone `\r` sneaks in too when a server flushes mid-frame.

**Prevention**: Mirror `anthropic.rs`'s line-buffered state machine on every SSE parser. The split happens on **bytes**, not strings — only attempt UTF-8 decode on the assembled event payload, never on a raw chunk.

### Drop guards beat manual cleanup on every exit path

**Failure**: Per-run workspace cleanup in `atelier-gui/src/lib.rs` was a tail call. An error mid-loop left orphan tempdirs.

**Prevention**: Any resource that needs cleanup on every exit path (success / `?`-propagated error / panic) gets a `Drop` impl. `RunCleanup`, `DispatcherHandleGuard`, `TerminalGuard` are the pattern. Tail calls don't survive panics; `Drop` does.

---

## v51 — Probe-on-first-use

### Sentinel tags are project constants, not free strings

**Failure**: Probe driver hardcoded `<<<envelope>>>` as the open tag in the calibration prompt + tests. The actual tag is `<<<harness_meta>>>` (`protocol_strategy::SENTINEL_OPEN`). Four tests failed because the model's "correct" reply didn't parse.

**Prevention**: When a calibration / golden prompt depends on a project constant, import the constant — don't retype the string. `use crate::protocol_strategy::{SENTINEL_OPEN, SENTINEL_CLOSE};` and build the prompt with `format!("{SENTINEL_OPEN}…{SENTINEL_CLOSE}")`. Tests use the same constants.

### Distinguish fatal probe errors from "this strategy didn't work"

**Failure**: First draft of `probe_model` returned `AdapterError` from any probe call failure. A transient `Malformed` response from one probe killed the whole probe; the cache stayed empty; the next run paid two more round-trips.

**Prevention**: `is_fatal_for_probe(&err)` distinguishes `Auth` / `NotConfigured` / `Unreachable` / `ContextOverflow` (propagate — no point continuing) from `Malformed` / `Provider` / `RateLimited` (record a note, set the flag to `false`, continue). The probe always completes when the endpoint is reachable, and the cache records what actually happened.

### Static vs dynamic capability detection are complementary, not alternatives

**Insight (not a failure)**: When the user asked for adaptive model detection, three approaches existed: (1) static capability matrix, (2) probe-on-first-use, (3) adaptive few-shot. The right answer wasn't to pick one — it was to ship the probe first because it discovers truth, and leave room for the static table to override the probe for well-known models (Anthropic, Mock — we already do this via `ProbePolicy::Skip`).

**Prevention**: When the design space looks like "A vs B", check whether they're complementary layers. Probe + static table is the cleanest decomposition; the static table is the cache hit path for known models, probe is the slow path for unknown ones.

### Cache key needs a separator

**Failure**: Almost shipped `sha256(model_id + base_url)` without a separator. `("ab", "cd")` and `("a", "bcd")` would have produced the same hash.

**Prevention**: `cache_path_does_not_collide_via_concat_ambiguity` test locks this in. Any time a hash function takes a tuple of strings, the prevention rule is "use an in-band separator that can't appear in the inputs" — `"\n"` works here because model_ids never contain newlines.

---

## Cross-cutting observations from v41–v51

### Bundle commit per phase, not per fix

**Pattern that worked**: v41–v50's GUI panes, hunk approval, driver modes, and OpenAI-compat adapter all sat uncommitted on a single feature branch and landed as one large commit (`a44b223`, +8816 / −477). The user prefers this for refactors and feature-blocks; many small commits would have churned the changelog without adding signal.

**When to break this rule**: A genuinely independent fix (like the probe work in v51, which doesn't depend on any v41–v50 internal state) lands as its own commit. The signal is "would a reviewer want to bisect through this?"

### Documentation rots faster than code

**Pattern**: README.md and STATUS.md both still claimed "atelier run coming with Phase A" and described the GUI/TUI as "Scaffold" through v50. The deep documentation sweep at the end of v51 caught it.

**Prevention**: Every CHANGELOG entry that lands a user-visible feature is also a TODO to update README.md / STATUS.md / per-crate READMEs. Better practice: a `make docs-check` linter that greps for stale "coming soon" / "not yet" claims against the current `CHANGELOG.md` headers. Worth building when the count of crusty claims gets above two.
