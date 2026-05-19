//! Atelier CLI library surface.
//!
//! v47: split from the `atelier` binary so other crates — primarily
//! `atelier-gui` and `atelier-tui` driving sessions in v47/v48 — can
//! build a `SessionDispatcher` and drive the turn loop without
//! reimplementing the wiring. The `[[bin]] atelier` in `main.rs`
//! consumes this same module.
//!
//! v49: the `runner` module is now `pub(crate)`-only with an
//! explicit `pub use` of the blessed types below. Internal helpers
//! (`extract_native_envelope`, `built_in_registry`, `now_rfc3339`,
//! `days_to_ymd`, `registry_to_tool_specs`, `build_mock_adapter`,
//! `advance`, `adapter_to_run_error`) were previously reachable as
//! `atelier_cli::runner::*` because `pub mod runner;` re-exported the
//! whole module surface. Tightening it here prevents downstream
//! consumers from accidentally depending on internals that the
//! runner needs to refactor freely.

// Module is `pub` so the integration tests in `tests/` can reach it
// via `atelier_cli::runner::*`. The blessed types are re-exported at
// the crate root below — application code outside this crate should
// import via `atelier_cli::{Runner, ProviderChoice, ...}` and never
// `atelier_cli::runner::*`.
//
// The `pub` is therefore a deliberate test-affordance, not part of
// the supported API. A future move to truly private (e.g., by
// migrating tests off the module path) is a one-line change.
pub mod runner;

/// v60 — shared "promote a memory card to disk" writer. Both
/// drivers (GUI Tauri command + TUI mutation handler) route through
/// here so the v58/v59 hardening (HOME validation, canonical-root
/// containment, atomic write, size cap) applies uniformly. See the
/// module-level doc for the failure modes it closes.
pub mod memory_promote;

/// v60.5 — hardened reader/writer for §5 compaction blobs. Mirrors
/// `memory_promote`'s discipline (canonical-root containment, atomic
/// `NamedTempFile::persist`, per-call size cap) for the
/// `.atelier/sessions/<sid>/compactions/<id>.json` payload that
/// v60.6 Expand will replay.
pub mod compaction_blob;

/// v60.5 — §5 non-destructive compaction orchestration. Composes the
/// adapter summary call + blob write + dispatcher mutator into a
/// single async free function the GUI Tauri command and TUI
/// `Mutation::Compact` arm both delegate to.
pub mod compaction;

/// v60.6 — §5 Expand orchestration. Symmetric counterpart to
/// [`compaction`]: reads the on-disk blob produced by a v60.5+
/// compaction and replays the originals back into context.
pub mod expansion;

/// §2 protocol-overhead harness. Backs the `atelier protocol-overhead`
/// CLI subcommand and the nightly CI job; measures bytes-on-wire,
/// approximate tokens, and parse time per emission strategy against
/// scripted MockAdapter fixtures.
pub mod overhead;

/// Phase C close — UX-target measurement instrumentation.
/// `PaneVisibilityRecord` + `FindProbeLog` write sibling JSON
/// alongside `session.json` so the §3 ("refactor without
/// conversation pane open") and §5 ("find what agent knows about
/// file X median <5 s") spec targets have on-disk observables to
/// read. Both are advisory — failing to write either does not fail
/// the run.
pub mod instrumentation;

/// v60.20 — `atelier find --path <P>` subcommand. Searches the
/// most recent (or named) session's conversation transcript for
/// content related to a given path and appends a [`FindProbe`] to
/// the session's `find_probes.json` so the median-elapsed-ms UX
/// target has data to read.
pub mod find;

pub use runner::{
    AdapterHandle, DispatcherHandle, EventSink, MockResponse, ProbePolicy, ProviderChoice,
    RunError, RunReport, Runner,
};

/// v60.32 M02 — map a `RunReport.final_state` to the `atelier` binary's
/// exit code so CI gates can distinguish "completed" (0) from
/// "stalled, agent waiting for the user" (6). 130/143 stay reserved
/// for the v60.29 signal handlers; non-zero adapter errors stay on 1.
pub fn exit_code_for_final_state(state: atelier_core::State) -> u8 {
    match state {
        atelier_core::State::AwaitingUser => 6,
        _ => 0,
    }
}

// v50: re-export ApprovalPolicy from atelier-core so a downstream
// driver (GUI / TUI) configuring a Runner doesn't have to depend on
// atelier-core directly just to flip the policy. The blessed import
// path is `atelier_cli::ApprovalPolicy`.
pub use atelier_core::dispatcher::ApprovalPolicy;
