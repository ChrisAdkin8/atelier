//! atelier-core: agent loop, BYOM adapters, MCP client, session state.
//!
//! Spec references:
//!   §1   BYOM adapter trait
//!   §2   Model Protocol envelope
//!   §2.5 Agent loop state machine
//!   §4   Time travel / checkpoints
//!   §14  Persistence & recovery
//!   §15  Extensibility — MCP-first tool transport

pub mod adapter;
pub mod audit;
pub mod config;
pub mod context;
pub mod diff;
pub mod dispatcher;
pub mod dod;
pub mod error;
pub mod file_watcher;
pub mod hooks;
pub mod init;
pub mod ledger;
pub mod mcp;
pub mod mcp_config;
pub mod memory;
pub mod mental_model;
pub mod path_safety;
pub mod persistence;
pub mod plan;
pub mod protocol;
pub mod protocol_conformance;
pub mod protocol_strategy;
pub mod sandbox;
pub mod session;
pub mod staging;
pub mod state;
pub mod subprocess;
pub mod text_safety;
pub mod time;
pub mod tools;
pub mod verify;

pub use adapter::{
    Adapter, AdapterError, Capabilities, CapabilityClaim, ChatResponse, ChunkStream, Message,
    MockAdapter, Role, StreamChunk, ToolCallRequest, ToolSpec, Usage,
};
pub use audit::{append_subprocess_egress, AuditError, EgressEvent};
pub use context::{
    CacheBustEvent, ContextError, ContextItem, ContextItemId, ContextManager, Payload, Provenance,
    TokenCount, TokenSnapshot, TokenSource,
};
pub use diff::{hunks_for, hunks_for_created, hunks_for_deleted, Hunk, Hunks, LineRange};
pub use dispatcher::{
    ConcurrentEditPolicy, DispatchOutcome, Dispatcher, HookExecutor, HookPhases, NoopHookExecutor,
    RegisterError, SessionDispatcher, ShellHookExecutor, SideEffectClass, Tool, ToolContext,
    ToolRegistry, ToolResult,
};
pub use dod::{DodCheck, DodConfig, DodError, DodTier, ExpectClause, DOD_FILE, DOD_VERSION};
pub use error::{Recovery, ToolError};
pub use file_watcher::{
    spawn as spawn_file_watcher, FileWatcherError, FileWatcherHandle, FILE_WATCH_DEBOUNCE,
};
pub use hooks::{
    HookApprovals, HookError, HookEvent, HookImplementation, HookManifest, HookSet, APPROVALS_FILE,
    HOOK_MANIFEST_VERSION,
};
pub use init::{init, InitSummary, ATELIER_MD_TEMPLATE};
pub use ledger::{
    local_cost_usd, Kind as LedgerKind, Ledger, LedgerEntry, DEFAULT_LOCAL_RATE_USD_PER_SEC,
};
pub use mcp::{
    default_sandbox_for_workspace as default_mcp_sandbox, launch_stdio_server, McpLaunchError,
    McpServerHandle, McpTool, FIRST_LIST_TIMEOUT_MS as MCP_FIRST_LIST_TIMEOUT_MS,
    HANDSHAKE_TIMEOUT_MS as MCP_HANDSHAKE_TIMEOUT_MS,
    SUPPORTED_PROTOCOL_VERSION as MCP_SUPPORTED_PROTOCOL_VERSION,
};
pub use mcp_config::{
    approvals_path as mcp_approvals_path, interpolate as mcp_interpolate, load_mcp_servers,
    parse_mcp_servers, McpApprovals, McpConfigError, McpServerManifest,
    SideEffectClass as McpSideEffectClass, Transport as McpTransport, MCP_APPROVALS_FILE,
    MCP_SERVERS_DIR, MCP_SERVERS_FILE, MCP_SERVERS_VERSION,
};
pub use memory::{MemoryCard, MemoryError, MemoryStore, PromoteOutput};
pub use mental_model::{MentalModel, MentalModelError, MentalModelSnapshot};
pub use persistence::{
    Checkpoints, ConversationEntry, OnDiskSession, PersistenceError, Plan, RecoveryEntry,
    RecoveryReason, Registry, RegistryEntry, DIFFS_SUBDIR, HARNESS_SESSION_VERSION, SESSION_FILE,
};
pub use plan::{ApplyReport, PlanCanvas, PlanError, PlanStatus, PlanStep};
pub use protocol::{
    ClaimedChange, ClaimedChangeKind, Envelope, EnvelopeError, Grounding, GroundingSource, PlanOp,
    PlanOpKind, PlanUpdate, Uncertainty, UncertaintyKind,
};
pub use protocol_conformance::{
    ConformanceRingBuffer, ConformanceSnapshot, Sample, TurnConformance, TurnDecision,
    CONFORMANCE_WINDOW, TURN_FAILURE_BUDGET,
};
pub use protocol_strategy::{
    encode_json_sentinel, encode_native_tool, encode_regex_prose, measure_overhead,
    parse_json_sentinel, parse_native_tool, parse_regex_prose, JsonSentinelParse, NativeToolCall,
    OverheadMeasurement, Strategy, StrategyError, APPROX_CHARS_PER_TOKEN, HARNESS_META_NAME,
    PROSE_TAG_CHANGES, PROSE_TAG_DONE, PROSE_TAG_GROUNDING, PROSE_TAG_UNCERTAINTY, SENTINEL_CLOSE,
    SENTINEL_OPEN,
};
pub use sandbox::{linux_bwrap_argv, macos_profile, SandboxError, SandboxPolicy};
pub use session::{
    edit_staged_events, spawn as spawn_session, spawn_with_capacity as spawn_session_with_capacity,
    Command, ConcurrentEditOutcome, Event, Handle as SessionHandle, SessionId, EVENT_BUFFER,
    INBOX_CAPACITY, TOOL_PARALLELISM_CAP,
};
pub use staging::{
    sha256, CommitReport, FileOutcome, NoopSyntaxCheck, StagedWrite, Staging, StagingError,
    SyntaxCheck, SyntaxOutcome, TreeSitterSyntaxCheck,
};
pub use state::{
    CheckpointHook, IllegalTransition, LedgerHook, NoopHook, State, Transition, LEGAL_TRANSITIONS,
};
pub use subprocess::{
    run as run_subprocess, sandboxed_argv, SubprocessError, SubprocessOutcome, SubprocessSpec,
};
pub use verify::{compare as compare_envelope_to_diff, Discrepancy, ObservedChange, ObservedKind};
