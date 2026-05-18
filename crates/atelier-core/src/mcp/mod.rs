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
//!
//! What's deferred to v60.11+ (per `tasks/todo.md` §15):
//!
//!   - HTTP/SSE transport launcher.
//!   - Wiring MCP tools into `crate::dispatcher` (built-ins-as-MCP refactor).
//!   - Surfacing `Resources` as §5 `ContextItem`s.
//!   - The §15 mechanical gate.
//!
//! Q7 verdict (recorded in `experiments/rmcp_spike/README.md`): **GO WITH
//! CAVEATS**. rmcp 0.1.5 works for stdio; flagged smells live in the spike
//! README and the v60.11+ wiring bundle must read those before designing
//! the dispatcher integration.

pub mod errors;
pub mod stdio_launcher;

pub use errors::McpLaunchError;
pub use stdio_launcher::{
    default_sandbox_for_workspace, launch_stdio_server, McpServerHandle, McpTool,
    FIRST_LIST_TIMEOUT_MS, HANDSHAKE_TIMEOUT_MS, SUPPORTED_PROTOCOL_VERSION,
};
