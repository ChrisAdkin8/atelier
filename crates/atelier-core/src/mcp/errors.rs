//! §15 MCP launcher errors.
//!
//! Mapped against rmcp 0.1.5's error surface (`rmcp::ServiceError` and
//! `std::io::Error` from the subprocess spawn) so the launcher can be a
//! thin shim — `From<…>` conversions live here.
//!
//! Discriminants are stable wire-labels: the future `Event::McpLaunchFailed`
//! variant + the trust-budget UI both read them. Don't rename without bumping
//! the session-event schema.

use std::io;

use thiserror::Error;

/// Errors raised by the §15 stdio launcher.
///
/// Variant taxonomy (intentionally narrow, matching the four ways an MCP
/// launch can fail; further wire-failure detail lives in `Refused`'s payload):
///   - `Spawn` — the subprocess never started (`Command::spawn` failed).
///     Wraps the raw `io::Error`. Typical cause: command not on PATH,
///     EACCES on the binary, missing interpreter.
///   - `Handshake` — the subprocess started but the JSON-RPC `initialize`
///     handshake never completed. Wraps an opaque message string (rmcp's
///     `ServiceError` is `non_exhaustive`, so we don't try to preserve its
///     enum structure across the boundary).
///   - `ProtocolMismatch` — the server returned an `initialize` response
///     but its `protocolVersion` is outside what this build supports. The
///     launcher records the version it saw for the trust-budget UI to render.
///   - `Refused` — the server completed initialise but immediately returned
///     an error on the first subsequent request (`tools/list`). Typically
///     means the server is sandboxed-out / misconfigured. Carries the
///     `ServiceError`-stringified payload for diagnostics.
///   - `ChildExited` — the subprocess exited (clean or signal) before / during
///     handshake. Carries the exit code if any (`-1` for signal-killed).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpLaunchError {
    /// `tokio::process::Command::spawn` failed — the OS rejected the launch.
    #[error("failed to spawn MCP server {name:?} ({command:?}): {source}")]
    Spawn {
        name: String,
        command: String,
        #[source]
        source: io::Error,
    },

    /// rmcp's `initialize` handshake never completed. Wraps an opaque message
    /// because `rmcp::ServiceError` is `non_exhaustive` and its concrete
    /// variants are unstable across rmcp 0.1.x point releases.
    #[error("MCP server {name:?} failed initialize handshake: {message}")]
    Handshake { name: String, message: String },

    /// The server advertised a `protocolVersion` we don't support. Atelier's
    /// supported set is encoded by `compatible_protocol_version` in
    /// `stdio_launcher.rs`. Future bundles may relax to a range.
    #[error("MCP server {name:?} advertised unsupported protocolVersion {version:?}")]
    ProtocolMismatch { name: String, version: String },

    /// First post-handshake request (`tools/list`) returned an error. Usually
    /// means the server is alive but refusing to operate (mis-configured
    /// args, sandbox denied a path, etc.).
    #[error("MCP server {name:?} refused first tools/list: {message}")]
    Refused { name: String, message: String },

    /// The subprocess exited before the launcher finished. `code` is `None`
    /// for signal-kills (e.g. server panicked into SIGABRT). Distinct from
    /// `Spawn` (which is pre-exec) and `Handshake` (which catches in-band
    /// rmcp errors).
    #[error("MCP server {name:?} child process exited with code {code:?} before launch completed")]
    ChildExited { name: String, code: Option<i32> },

    /// The manifest references a transport we don't (yet) support. Used by
    /// the launcher to refuse `http`/`sse` manifests until v60.11+.
    #[error(
        "MCP server {name:?} uses transport {transport:?}; only `stdio` is supported in this build"
    )]
    UnsupportedTransport { name: String, transport: String },

    /// Manifest validation caught at launch time (e.g. `stdio` transport with
    /// no `command` — should already be filtered by the schema, but the
    /// launcher double-checks for defence-in-depth).
    #[error("MCP server {name:?} manifest is invalid: {reason}")]
    InvalidManifest { name: String, reason: String },

    /// Env / header interpolation produced an error (e.g. `${env:MISSING}`).
    /// Wraps `crate::mcp_config::McpConfigError`'s display.
    #[error("MCP server {name:?} env interpolation failed: {reason}")]
    Interpolation { name: String, reason: String },
}

impl McpLaunchError {
    /// `true` for errors that suggest the manifest is wrong (vs. the server
    /// being temporarily down). The trust-budget UI uses this to decide
    /// whether to surface "edit your `mcp_servers.json`" vs. "retry?".
    pub fn is_config_error(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedTransport { .. }
                | Self::InvalidManifest { .. }
                | Self::Interpolation { .. }
                | Self::ProtocolMismatch { .. }
        )
    }
}
