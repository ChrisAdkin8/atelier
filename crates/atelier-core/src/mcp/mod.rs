//! §15 MCP client surface.
//!
//! This module is the **rmcp-using** half of the §15 stack — its sibling
//! `crate::mcp_config` is the rmcp-free data layer (manifest loader +
//! approval store). They split apart on purpose: the data layer compiles in
//! a CI configuration that does not pull rmcp's compile-time-heavy macros,
//! and the future GUI Tauri command surface needs only the data half for
//! the approval UI.
//!
//! Module layout:
//!
//!   - [`errors`] — typed `McpLaunchError` with stable wire-label variants.
//!   - [`stdio_launcher`] — `launch_stdio_server` + `McpServerHandle` +
//!     `McpTool` projection of `rmcp::model::Tool`.
//!   - [`http_launcher`] — `launch_http_server` for `http`/`sse` transports
//!     (v60.11 C1), with §12 egress audit per `schemas/audit/mcp_egress.v1.json`.
//!
//! What's deferred to v60.11+ later bundles (per `tasks/todo.md` §15):
//!
//!   - Wiring MCP tools into `crate::dispatcher` (built-ins-as-MCP refactor).
//!   - Surfacing `Resources` as §5 `ContextItem`s.
//!   - Per-`call_tool`-invocation audit rows (the C1 launcher audits the
//!     handshake + initial `tools/list` probe; later dispatch invocations
//!     produce their own rows once the dispatcher integration lands).
//!   - The §15 mechanical gate.
//!
//! Q7 verdict (recorded in `experiments/rmcp_spike/README.md`): **GO WITH
//! CAVEATS**. rmcp 0.1.5 works for stdio + SSE; flagged smells live in the
//! spike README and the v60.11+ wiring bundle must read those before
//! designing the dispatcher integration.

pub mod errors;
pub mod http_launcher;
pub mod stdio_launcher;

pub use errors::McpLaunchError;
pub use http_launcher::launch_http_server;
pub use stdio_launcher::{
    default_sandbox_for_workspace, launch_stdio_server, McpServerHandle, McpTool,
    FIRST_LIST_TIMEOUT_MS, HANDSHAKE_TIMEOUT_MS, SUPPORTED_PROTOCOL_VERSION,
};
