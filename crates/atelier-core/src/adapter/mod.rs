//! §1 BYOM adapter trait.
//!
//! Spec §1 "Adapter trait":
//!   * `chat(messages, tools?) -> response`
//!   * `stream(messages, tools?) -> AsyncIterator[chunk]`
//!   * `count_tokens(messages) -> { count, source: "exact"|"approx"|"unavailable" }`
//!   * `capabilities() -> Capabilities`
//!   * `conformance() -> ConformanceStats` (bounded 100-call ring buffer)
//!
//! Spec §1 "Capability matrix":
//!   Per-capability `Supported / ClaimedButBroken / Unsupported`. The
//!   "claimed-but-broken" column flags adapters whose provider advertises a
//!   capability (e.g., native tool use) but whose actual emissions
//!   repeatedly fail the §2 conformance check.
//!
//! This module is the **abstraction**. Concrete adapters land in sibling
//! modules ([`anthropic`] is the first; `openai_compat`, `ollama`,
//! `bedrock`, `vertex` later). [`MockAdapter`] is the in-tree adapter used
//! by every downstream test (dispatcher, end-to-end loop) — it keeps those
//! tests off the network and lets us inject specific failure modes.

pub mod anthropic;
pub mod capability_matrix;
pub mod model_profile;
pub mod openai_compat;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::context::TokenSource;
use crate::protocol_conformance::{ConformanceRingBuffer, ConformanceSnapshot, Sample};
use crate::protocol_strategy::Strategy;

/// One turn of conversation history fed into the adapter. Kept minimal and
/// concrete — the per-provider quirks (tool-use blocks, vision parts,
/// thinking, cache control) translate from this in the adapter impl, not in
/// the harness. The §2 envelope rides alongside via the adapter's chosen
/// emission strategy and is not part of `Message` itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// When `role == Tool`, references the tool-call id this result
    /// satisfies. The adapter forwards it on the wire as the provider
    /// expects (e.g., Anthropic `tool_use_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// When `role == Assistant`, the tool-use blocks the model issued in
    /// this turn. The adapter forwards them back on the next request so
    /// multi-turn tool flows round-trip without losing the original
    /// tool_use ids — pre-P5 the adapter flattened this to text-only,
    /// which broke any provider whose protocol requires the prior
    /// `tool_use` block to reference its matching `tool_result` (Anthropic,
    /// OpenAI, Bedrock, Gemini all do).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
}

impl Message {
    /// Construct a user/system/assistant message with no tool plumbing.
    /// Most call sites use this — the `tool_call_id` + `tool_calls` cases
    /// are rare enough to warrant explicit construction.
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool the adapter advertises to the provider's native tool-use channel
/// when [`Strategy::NativeTool`] is selected. Schema is the JSON Schema for
/// the tool's input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Why the model stopped generating. Cross-provider abstraction over
/// Anthropic's `stop_reason`, OpenAI's `finish_reason`, etc.
///
/// - `EndTurn` — the model voluntarily stopped (Anthropic `end_turn`,
///   OpenAI `stop`). The turn is complete.
/// - `MaxTokens` — the response was truncated because `max_tokens` was
///   reached. The harness should treat this as a soft failure: the
///   envelope (if any) is likely incomplete and re-prompting is in order.
/// - `ToolUse` — the model wants to invoke one or more tools; the
///   adapter has populated `ChatResponse.tool_calls`.
/// - `StopSequence` — a configured stop sequence was emitted.
/// - `Refusal` — the model refused (Anthropic `refusal`). The harness
///   surfaces this to the user; do not retry.
/// - `Other` — unrecognised provider-specific value; treat as
///   `EndTurn` for terminal-state purposes but log so we know to extend
///   the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
    Other,
}

/// One model invocation's structured response. Streaming adapters emit
/// [`StreamChunk`] values incrementally and assemble this at the end; the
/// non-streaming `chat()` returns it directly.
///
/// The harness extracts the [`crate::protocol::Envelope`] from `text`
/// (via the active [`Strategy`]) or from `tool_calls` (native-tool
/// strategy). The cost-ledger entry is materialised from `usage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    pub usage: Usage,
    /// Which §2 strategy this response was emitted under. The adapter is
    /// authoritative on this — the harness reads it to decide which parser
    /// to use without re-inferring from response shape.
    pub strategy: Strategy,
    /// Why the model stopped. Provider-agnostic; see [`StopReason`].
    /// `None` only when the adapter cannot determine it (truncated stream,
    /// pre-`StopReason` provider). The harness must distinguish `MaxTokens`
    /// and `Refusal` from `EndTurn` — silently treating truncated-mid-thought
    /// as success is the bug this field exists to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

/// Native tool-use call requested by the model. The dispatcher executes it
/// and feeds the result back as a [`Role::Tool`] message in the next turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Incremental chunk emitted by [`Adapter::stream`]. The UI renders text
/// chunks live (§3 "live diff updates as the agent edits" extends to the
/// conversation pane). The envelope is only valid at end-of-turn (spec §2.5
/// "the envelope is never rendered token-by-token") so adapters surface it
/// via a final `Complete` chunk rather than streaming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamChunk {
    /// Text token(s). Multiple tokens per chunk allowed for efficiency.
    Text { delta: String },
    /// Provider has decided to invoke a tool. Surface immediately so the
    /// UI can mark the tool-call card `dispatching` (spec §2.5 streaming
    /// UI semantics).
    ToolCallStarted { id: String, name: String },
    /// Streamed arguments for a tool call; the dispatcher waits for
    /// `ToolCallCompleted` before invoking.
    ToolCallDelta { id: String, args_delta: String },
    /// Tool-call args are complete and parseable.
    ToolCallCompleted {
        id: String,
        arguments: serde_json::Value,
    },
    /// Final chunk: assembled response + envelope-bearing material the
    /// adapter has extracted.
    Complete { response: ChatResponse },
    /// Adapter-level error mid-stream; the §2.5 actor treats this as an
    /// `ExecutionFailed` on the model "tool" and routes via `Recovery`.
    Error { error: AdapterError },
}

/// Per-call token usage. Mirrors `cost_ledger.kind = model_call` required
/// fields in `schemas/session/v1.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    pub count_source: TokenSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// What [`Adapter::count_tokens`] returns. `source` carries forward into the
/// cost ledger so a downstream consumer (trust budget, routing) can weight
/// the count appropriately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount {
    pub count: u32,
    pub source: TokenSource,
}

/// Spec §1 "Capability matrix" — one entry per row. The harness reads this
/// at session start to decide which §2 strategy to begin with and which
/// features (vision, prompt caching, …) to enable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub native_tool_use: CapabilityClaim,
    pub streaming: CapabilityClaim,
    pub vision: CapabilityClaim,
    pub prompt_cache: CapabilityClaim,
    pub structured_output: CapabilityClaim,
    /// `≥128k context` per the spec table. Adapter declares the actual
    /// window in `context_window_tokens`; `claim` covers whether the
    /// provider honours the declared cap in practice.
    pub long_context: CapabilityClaim,
    pub context_window_tokens: u32,
}

/// Three-state capability flag. `ClaimedButBroken` is the column from spec
/// §1's matrix — the provider advertises support but the adapter's
/// `conformance()` window shows persistent failures. The harness reads this
/// to auto-degrade (e.g., native_tool → json_sentinel) without waiting for
/// the per-turn budget to trip again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClaim {
    Supported,
    ClaimedButBroken,
    Unsupported,
}

impl CapabilityClaim {
    /// Whether the capability is usable. Both `Supported` and `Unsupported`
    /// are honest; only `ClaimedButBroken` is the trap state the matrix
    /// exists to surface.
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Adapter-level errors. Mapped onto `ToolError` by the §2.5 state machine
/// when the model invocation itself fails (as opposed to a tool call within
/// the turn).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterError {
    /// Context exceeded the routed model's window. Per spec §1 the state
    /// machine cancels the dispatch and transitions to `AwaitingUser` with
    /// the three-option modal (Compact / Reroute / Cancel). Never falls
    /// back to silent truncation.
    #[error("context overflow: needed {needed_tokens} tokens, model accepts {limit_tokens}")]
    ContextOverflow {
        needed_tokens: u32,
        limit_tokens: u32,
    },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("provider unreachable: {0}")]
    Unreachable(String),
    #[error("provider returned malformed response: {0}")]
    Malformed(String),
    #[error("rate-limited; retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("provider error: status {status}, body: {body}")]
    Provider { status: u16, body: String },
    #[error("adapter not configured: {0}")]
    NotConfigured(String),
}

impl AdapterError {
    /// Whether the §2.5 state machine should hand back to the user (vs.
    /// retry-budgeted). Mirrors the spec table for `Recovery` routing —
    /// auth errors and context overflow demand human input, transient
    /// failures don't.
    pub fn requires_user_decision(&self) -> bool {
        matches!(
            self,
            Self::ContextOverflow { .. } | Self::Auth(_) | Self::NotConfigured(_)
        )
    }
}

/// The BYOM trait every provider integration implements. `async_trait`
/// keeps the call sites ergonomic at the cost of one extra heap allocation
/// per call — negligible against the network latency this is fronting.
#[async_trait]
pub trait Adapter: Send + Sync {
    /// Human-readable identifier used in the cost ledger's `model_id` field
    /// (`<provider>:<model>`, e.g. `anthropic:claude-opus-4-7`).
    fn model_id(&self) -> &str;

    /// Capability matrix snapshot. Spec §1.
    fn capabilities(&self) -> Capabilities;

    /// Bounded 100-call conformance snapshot. Spec §1 + §2.
    fn conformance(&self) -> ConformanceSnapshot;

    /// Best-effort token count. Spec §1: "`unavailable` falls back to
    /// character/4 with one warning per session." The caller (the
    /// dispatcher) does that fallback; the adapter just declares the source.
    async fn count_tokens(&self, messages: &[Message]) -> Result<TokenCount, AdapterError>;

    /// Non-streaming completion. Adapters that only expose streaming should
    /// implement this in terms of `stream()`; the default impl below does so.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, AdapterError> {
        let mut chunks = self.stream(messages, tools).await?;
        // The stream iterator is returned as a boxed dyn — pull until the
        // terminal `Complete` (or surface the first `Error`).
        while let Some(chunk) = chunks.next().await {
            match chunk {
                StreamChunk::Complete { response } => return Ok(response),
                StreamChunk::Error { error } => return Err(error),
                _ => continue,
            }
        }
        Err(AdapterError::Malformed(
            "stream ended without Complete or Error chunk".into(),
        ))
    }

    /// Streaming completion. Returns a [`ChunkStream`] the dispatcher
    /// drives one chunk at a time. The choice of [`Strategy`] (which §2
    /// emission mode) is the adapter's; the harness reads it off the final
    /// `ChatResponse.strategy`.
    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChunkStream, AdapterError>;

    /// v60.9 — per-adapter few-shot override. Returns `Some(messages)` to
    /// replace the shared baseline for the given strategy, or `None` to
    /// fall back to the harness's default. Default impl returns `None`;
    /// adapters with strategy-specific quirks (Anthropic's `tool_use`
    /// shape, OpenAI's structured-output formatting) override.
    ///
    /// The runner consults this once per session at start-up, caches the
    /// result, and prepends it to the per-turn message history. Override
    /// implementations must therefore return self-contained `Message`
    /// pairs (the canonical shape is one `Role::User` example followed
    /// by one `Role::Assistant` envelope-bearing reply) that can sit at
    /// the head of the conversation without dangling tool-call ids.
    fn few_shot_override(&self, strategy: Strategy) -> Option<Vec<Message>> {
        let _ = strategy;
        None
    }
}

/// Boxed async stream of [`StreamChunk`] values. `Send` because the
/// dispatcher pulls chunks from inside a tokio task. Constructed via
/// [`ChunkStream::from_vec`] for tests; production adapters wrap an SSE
/// reader.
pub struct ChunkStream {
    inner: Box<dyn ChunkSource + Send + Unpin>,
}

#[async_trait]
pub(crate) trait ChunkSource {
    async fn next(&mut self) -> Option<StreamChunk>;
}

impl ChunkStream {
    /// Construct from a `Vec` of pre-built chunks. The primary tool for
    /// tests; production adapters provide their own `ChunkSource`.
    pub fn from_vec(chunks: Vec<StreamChunk>) -> Self {
        Self {
            inner: Box::new(VecChunks {
                chunks: chunks.into_iter().collect(),
            }),
        }
    }

    /// Wrap a sibling-module `ChunkSource` implementation. Crate-internal
    /// so concrete adapters in `adapter::*` can supply their own streaming
    /// machinery (e.g., the SSE reader in [`anthropic`]) without exposing
    /// the `ChunkSource` trait to the public API.
    pub(crate) fn from_inner(inner: Box<dyn ChunkSource + Send + Unpin>) -> Self {
        Self { inner }
    }

    pub async fn next(&mut self) -> Option<StreamChunk> {
        self.inner.next().await
    }
}

impl fmt::Debug for ChunkStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkStream").finish_non_exhaustive()
    }
}

struct VecChunks {
    chunks: std::collections::VecDeque<StreamChunk>,
}

#[async_trait]
impl ChunkSource for VecChunks {
    async fn next(&mut self) -> Option<StreamChunk> {
        self.chunks.pop_front()
    }
}

// ---------- Mock adapter ----------

/// In-tree mock used by every downstream test. Lets tests pre-load a
/// queue of `ChunkStream`s the adapter will return in order on successive
/// `chat`/`stream` calls. Provides a knob for each capability so the
/// "claimed-but-broken" path can be exercised by deliberately returning
/// malformed envelopes — per spec §1 acceptance gate.
pub struct MockAdapter {
    model_id: String,
    capabilities: Capabilities,
    streams: Mutex<std::collections::VecDeque<Vec<StreamChunk>>>,
    token_count_per_message: u32,
    token_count_source: TokenSource,
    ring: Mutex<ConformanceRingBuffer>,
}

impl MockAdapter {
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            capabilities: Capabilities {
                native_tool_use: CapabilityClaim::Supported,
                streaming: CapabilityClaim::Supported,
                vision: CapabilityClaim::Unsupported,
                prompt_cache: CapabilityClaim::Unsupported,
                structured_output: CapabilityClaim::Supported,
                long_context: CapabilityClaim::Supported,
                context_window_tokens: 200_000,
            },
            streams: Mutex::new(std::collections::VecDeque::new()),
            token_count_per_message: 4,
            token_count_source: TokenSource::Approx,
            ring: Mutex::new(ConformanceRingBuffer::new()),
        }
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Cap the long-context capability to a specific window. Combined with
    /// the `should_overflow` queue, lets tests exercise the §1
    /// `ContextOverflowError` path deterministically.
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.capabilities.context_window_tokens = tokens;
        self
    }

    /// Queue a stream of chunks to be returned on the next `stream()` call.
    /// Multiple queued streams are consumed in FIFO order.
    pub fn queue_stream(&self, chunks: Vec<StreamChunk>) {
        self.streams.lock().push_back(chunks);
    }

    /// Convenience: queue a single-shot response that emits one text chunk +
    /// a final `Complete`. The most common test shape.
    pub fn queue_text_response(&self, text: impl Into<String>) {
        let text: String = text.into();
        self.queue_stream(vec![
            StreamChunk::Text {
                delta: text.clone(),
            },
            StreamChunk::Complete {
                response: ChatResponse {
                    text,
                    tool_calls: Vec::new(),
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        cached_tokens: None,
                        count_source: TokenSource::Approx,
                        latency_ms: Some(0),
                    },
                    strategy: Strategy::JsonSentinel,
                    stop_reason: Some(StopReason::EndTurn),
                },
            },
        ]);
    }

    /// Record a conformance sample directly (lets tests assert the matrix
    /// reflects what the §2 conformance tracker has recorded).
    pub fn record_conformance(&self, sample: Sample) {
        self.ring.lock().record(sample);
    }
}

#[async_trait]
impl Adapter for MockAdapter {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn conformance(&self) -> ConformanceSnapshot {
        self.ring.lock().snapshot()
    }

    async fn count_tokens(&self, messages: &[Message]) -> Result<TokenCount, AdapterError> {
        Ok(TokenCount {
            count: self.token_count_per_message * messages.len() as u32,
            source: self.token_count_source,
        })
    }

    async fn stream(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChunkStream, AdapterError> {
        // Cheap overflow check so tests can drive the ContextOverflow path
        // without queueing a chunk for it.
        let approx_tokens = self
            .count_tokens(messages)
            .await
            .map(|c| c.count)
            .unwrap_or(0);
        if approx_tokens > self.capabilities.context_window_tokens {
            return Err(AdapterError::ContextOverflow {
                needed_tokens: approx_tokens,
                limit_tokens: self.capabilities.context_window_tokens,
            });
        }

        let chunks = self
            .streams
            .lock()
            .pop_front()
            .ok_or_else(|| AdapterError::NotConfigured("no queued stream".into()))?;
        Ok(ChunkStream::from_vec(chunks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    fn complete_chunk(text: &str) -> StreamChunk {
        StreamChunk::Complete {
            response: ChatResponse {
                text: text.into(),
                tool_calls: Vec::new(),
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cached_tokens: None,
                    count_source: TokenSource::Exact,
                    latency_ms: Some(42),
                },
                strategy: Strategy::JsonSentinel,
                stop_reason: Some(StopReason::EndTurn),
            },
        }
    }

    // ---------- capability matrix ----------

    #[test]
    fn capability_claim_only_supported_is_usable() {
        assert!(CapabilityClaim::Supported.is_usable());
        assert!(!CapabilityClaim::ClaimedButBroken.is_usable());
        assert!(!CapabilityClaim::Unsupported.is_usable());
    }

    #[test]
    fn capability_claim_round_trips_through_serde() {
        for c in [
            CapabilityClaim::Supported,
            CapabilityClaim::ClaimedButBroken,
            CapabilityClaim::Unsupported,
        ] {
            let json = serde_json::to_string(&c).unwrap();
            let back: CapabilityClaim = serde_json::from_str(&json).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn capabilities_round_trip_through_serde() {
        let caps = Capabilities {
            native_tool_use: CapabilityClaim::Supported,
            streaming: CapabilityClaim::Supported,
            vision: CapabilityClaim::Unsupported,
            prompt_cache: CapabilityClaim::ClaimedButBroken,
            structured_output: CapabilityClaim::Supported,
            long_context: CapabilityClaim::Supported,
            context_window_tokens: 200_000,
        };
        let json = serde_json::to_string(&caps).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back, caps);
    }

    // ---------- error routing ----------

    #[test]
    fn context_overflow_requires_user_decision() {
        let e = AdapterError::ContextOverflow {
            needed_tokens: 300_000,
            limit_tokens: 200_000,
        };
        assert!(e.requires_user_decision());
    }

    #[test]
    fn transient_errors_do_not_require_user_decision() {
        for e in [
            AdapterError::Unreachable("dns".into()),
            AdapterError::Malformed("bad json".into()),
            AdapterError::RateLimited {
                retry_after_ms: 1000,
            },
            AdapterError::Provider {
                status: 500,
                body: "x".into(),
            },
        ] {
            assert!(!e.requires_user_decision(), "{e:?} should retry");
        }
    }

    #[test]
    fn auth_and_not_configured_require_user_decision() {
        assert!(AdapterError::Auth("expired".into()).requires_user_decision());
        assert!(AdapterError::NotConfigured("no key".into()).requires_user_decision());
    }

    // ---------- MockAdapter wiring ----------

    #[tokio::test]
    async fn mock_chat_drains_queued_stream_to_complete_response() {
        let m = MockAdapter::new("mock:test");
        m.queue_stream(vec![
            StreamChunk::Text {
                delta: "hello".into(),
            },
            complete_chunk("hello"),
        ]);
        let r = m.chat(&[msg(Role::User, "hi")], &[]).await.unwrap();
        assert_eq!(r.text, "hello");
        assert_eq!(r.strategy, Strategy::JsonSentinel);
    }

    #[tokio::test]
    async fn mock_chat_propagates_inline_error_chunk() {
        let m = MockAdapter::new("mock:test");
        m.queue_stream(vec![StreamChunk::Error {
            error: AdapterError::Malformed("nope".into()),
        }]);
        let err = m.chat(&[msg(Role::User, "hi")], &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)));
    }

    #[tokio::test]
    async fn mock_chat_errors_when_no_queued_stream() {
        let m = MockAdapter::new("mock:test");
        let err = m.chat(&[msg(Role::User, "hi")], &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn mock_chat_errors_when_stream_ends_without_complete() {
        let m = MockAdapter::new("mock:test");
        m.queue_stream(vec![StreamChunk::Text {
            delta: "no complete".into(),
        }]);
        let err = m.chat(&[msg(Role::User, "hi")], &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)));
    }

    #[tokio::test]
    async fn mock_stream_returns_queued_chunks_in_order() {
        let m = MockAdapter::new("mock:test");
        m.queue_stream(vec![
            StreamChunk::Text { delta: "a".into() },
            StreamChunk::Text { delta: "b".into() },
            complete_chunk("ab"),
        ]);
        let mut s = m.stream(&[msg(Role::User, "x")], &[]).await.unwrap();
        let mut texts = Vec::new();
        while let Some(c) = s.next().await {
            match c {
                StreamChunk::Text { delta } => texts.push(delta),
                StreamChunk::Complete { .. } => break,
                _ => {}
            }
        }
        assert_eq!(texts, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn mock_count_tokens_returns_per_message_estimate() {
        let m = MockAdapter::new("mock:test");
        let t = m
            .count_tokens(&[msg(Role::User, "a"), msg(Role::Assistant, "b")])
            .await
            .unwrap();
        assert_eq!(t.count, 8);
        assert_eq!(t.source, TokenSource::Approx);
    }

    #[tokio::test]
    async fn mock_short_window_triggers_context_overflow() {
        let m = MockAdapter::new("mock:test").with_context_window(5);
        // Each Message costs 4 tokens via the mock, so two messages = 8.
        let err = m
            .stream(&[msg(Role::User, "alpha"), msg(Role::User, "beta")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::ContextOverflow {
                needed_tokens,
                limit_tokens,
            } => {
                assert_eq!(needed_tokens, 8);
                assert_eq!(limit_tokens, 5);
            }
            other => panic!("expected ContextOverflow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_queue_text_response_short_circuits_chat() {
        let m = MockAdapter::new("mock:test");
        m.queue_text_response("ack");
        let r = m.chat(&[msg(Role::User, "hi")], &[]).await.unwrap();
        assert_eq!(r.text, "ack");
    }

    #[tokio::test]
    async fn mock_streams_drain_fifo_across_multiple_calls() {
        let m = MockAdapter::new("mock:test");
        m.queue_text_response("first");
        m.queue_text_response("second");
        let r1 = m.chat(&[msg(Role::User, "x")], &[]).await.unwrap();
        let r2 = m.chat(&[msg(Role::User, "x")], &[]).await.unwrap();
        assert_eq!(r1.text, "first");
        assert_eq!(r2.text, "second");
    }

    #[tokio::test]
    async fn mock_conformance_starts_empty_and_records_samples() {
        let m = MockAdapter::new("mock:test");
        let snap = m.conformance();
        assert_eq!(snap.total, 0);
        m.record_conformance(Sample {
            strategy: Strategy::NativeTool,
            ok: true,
        });
        m.record_conformance(Sample {
            strategy: Strategy::NativeTool,
            ok: false,
        });
        let snap = m.conformance();
        assert_eq!(snap.total, 2);
        assert_eq!(snap.successes, 1);
    }

    #[test]
    fn message_with_tool_call_id_round_trips() {
        let m = Message {
            role: Role::Tool,
            content: "result".into(),
            tool_call_id: Some("tc-7".into()),
            tool_calls: Vec::new(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        // Without tool_call_id, the field is absent in JSON.
        let m2 = Message {
            role: Role::User,
            content: "ask".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        };
        let json2 = serde_json::to_string(&m2).unwrap();
        assert!(!json2.contains("tool_call_id"));
    }

    #[test]
    fn role_serializes_lowercase() {
        for (lit, r) in [
            ("system", Role::System),
            ("user", Role::User),
            ("assistant", Role::Assistant),
            ("tool", Role::Tool),
        ] {
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{lit}\""));
        }
    }

    #[test]
    fn stream_chunk_variants_round_trip() {
        for chunk in [
            StreamChunk::Text { delta: "x".into() },
            StreamChunk::ToolCallStarted {
                id: "tc-1".into(),
                name: "shell".into(),
            },
            StreamChunk::ToolCallDelta {
                id: "tc-1".into(),
                args_delta: "{".into(),
            },
            StreamChunk::ToolCallCompleted {
                id: "tc-1".into(),
                arguments: serde_json::json!({"cmd": "echo"}),
            },
            complete_chunk("done"),
            StreamChunk::Error {
                error: AdapterError::Malformed("x".into()),
            },
        ] {
            let json = serde_json::to_string(&chunk).unwrap();
            let back: StreamChunk = serde_json::from_str(&json).unwrap();
            assert_eq!(back, chunk);
        }
    }

    // ---------- v60.9 few-shot override ----------

    #[test]
    fn mock_few_shot_override_returns_none_by_default() {
        let m = MockAdapter::new("mock:test");
        for s in [
            Strategy::NativeTool,
            Strategy::JsonSentinel,
            Strategy::RegexProse,
        ] {
            assert!(
                m.few_shot_override(s).is_none(),
                "Mock must keep the baseline for {s:?}; overrides are an opt-in for \
                 provider-specific quirks"
            );
        }
    }
}
