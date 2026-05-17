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
pub mod config;
pub mod context;
pub mod diff;
pub mod dispatcher;
pub mod dod;
pub mod error;
pub mod hooks;
pub mod init;
pub mod ledger;
pub mod memory;
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
pub mod tools;
pub mod verify;

pub use adapter::{
    Adapter, AdapterError, Capabilities, CapabilityClaim, ChatResponse, ChunkStream, Message,
    MockAdapter, Role, StreamChunk, ToolCallRequest, ToolSpec, Usage,
};
pub use context::{
    CacheBustEvent, ContextError, ContextItem, ContextItemId, ContextManager, Payload, Provenance,
    TokenCount, TokenSnapshot, TokenSource,
};
pub use diff::{hunks_for, hunks_for_created, hunks_for_deleted, Hunk, Hunks, LineRange};
pub use dispatcher::{
    DispatchOutcome, Dispatcher, HookExecutor, HookPhases, NoopHookExecutor, RegisterError,
    SessionDispatcher, ShellHookExecutor, SideEffectClass, Tool, ToolContext, ToolRegistry,
    ToolResult,
};
pub use dod::{DodCheck, DodConfig, DodError, DodTier, ExpectClause, DOD_FILE, DOD_VERSION};
pub use error::{Recovery, ToolError};
pub use hooks::{
    HookApprovals, HookError, HookEvent, HookImplementation, HookManifest, HookSet, APPROVALS_FILE,
    HOOK_MANIFEST_VERSION,
};
pub use init::{init, InitSummary, ATELIER_MD_TEMPLATE};
pub use ledger::{
    local_cost_usd, Kind as LedgerKind, Ledger, LedgerEntry, DEFAULT_LOCAL_RATE_USD_PER_SEC,
};
pub use memory::{MemoryCard, MemoryError, MemoryStore, PromoteOutput};
pub use persistence::{
    Checkpoints, OnDiskSession, PersistenceError, Plan, RecoveryEntry, RecoveryReason, Registry,
    RegistryEntry, DIFFS_SUBDIR, HARNESS_SESSION_VERSION, SESSION_FILE,
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
    encode_json_sentinel, encode_native_tool, encode_regex_prose, parse_json_sentinel,
    parse_native_tool, parse_regex_prose, JsonSentinelParse, NativeToolCall, Strategy,
    StrategyError, HARNESS_META_NAME, PROSE_TAG_CHANGES, PROSE_TAG_DONE, PROSE_TAG_GROUNDING,
    PROSE_TAG_UNCERTAINTY, SENTINEL_CLOSE, SENTINEL_OPEN,
};
pub use sandbox::{linux_bwrap_argv, macos_profile, SandboxError, SandboxPolicy};
pub use session::{
    edit_staged_events, spawn as spawn_session, spawn_with_capacity as spawn_session_with_capacity,
    Command, Event, Handle as SessionHandle, SessionId, EVENT_BUFFER, INBOX_CAPACITY,
    TOOL_PARALLELISM_CAP,
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
