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

pub use runner::{
    DispatcherHandle, EventSink, MockResponse, ProviderChoice, RunError, RunReport, Runner,
};

// v50: re-export ApprovalPolicy from atelier-core so a downstream
// driver (GUI / TUI) configuring a Runner doesn't have to depend on
// atelier-core directly just to flip the policy. The blessed import
// path is `atelier_cli::ApprovalPolicy`.
pub use atelier_core::dispatcher::ApprovalPolicy;
