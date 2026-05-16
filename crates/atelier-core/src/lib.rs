//! atelier-core: agent loop, BYOM adapters, MCP client, session state.
//!
//! Spec references:
//!   §1   BYOM adapter trait
//!   §2   Model Protocol envelope
//!   §2.5 Agent loop state machine
//!   §4   Time travel / checkpoints
//!   §14  Persistence & recovery
//!   §15  Extensibility — MCP-first tool transport

pub mod error;
pub mod init;

pub use error::{Recovery, ToolError};
pub use init::{init, InitSummary, ATELIER_MD_TEMPLATE};
