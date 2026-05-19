//! §1 Anthropic Messages API adapter.
//!
//! Talks to `POST /v1/messages` with `anthropic-version: 2023-06-01`. Native
//! tool use via the `tool_use` content block — the §2 envelope rides as
//! arguments to a `harness_meta` tool the harness declares to the model.
//! Streaming via SSE; the parser maps Anthropic's event types onto the
//! adapter-agnostic [`StreamChunk`] enum so the §2.5 actor's stream-handling
//! code is provider-blind.
//!
//! # Configuration
//!
//! Two constructors:
//!
//! - [`AnthropicAdapter::new`] — explicit API key + model id, used by tests
//!   that point `with_base_url` at a wiremock stub.
//! - [`AnthropicAdapter::from_env`] — reads `ANTHROPIC_API_KEY` (the only
//!   spec §11 credential resolution shape live in v0; keychain/`atelier
//!   login` is a Phase E enhancement).
//!
//! # Tests
//!
//! All tests use `wiremock` to stand up a fake Messages endpoint and
//! `with_base_url` to point the adapter at it. **No live API calls in CI.**
//! `make check` / `cargo test` never hit `api.anthropic.com`.
//!
//! # What this adapter does NOT do (yet)
//!
//! - Prompt caching (cache_control content blocks). The `prompt_cache`
//!   capability is reported as `Unsupported` until §1 caching lands.
//! - Vision (image content blocks). `vision` reported `Unsupported`.
//! - Token counting via `POST /v1/messages/count_tokens`. `count_tokens`
//!   returns the approx character/4 fallback with `TokenSource::Approx`
//!   until the real endpoint is wired (separate session — needs its own
//!   error shape and rate-limit handling).
//! - Retries on `RateLimited` / `Unreachable`. The §2.5 actor's `Recovery`
//!   routing owns retry policy; the adapter just reports errors faithfully.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use parking_lot::Mutex;
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    redact_response_body, Adapter, AdapterError, Capabilities, CapabilityClaim, ChatResponse,
    ChunkSource, ChunkStream, Message, Role, StopReason, StreamChunk, TokenCount, ToolCallRequest,
    ToolSpec, Usage,
};
use crate::context::TokenSource;
use crate::protocol_conformance::{ConformanceRingBuffer, ConformanceSnapshot};
use crate::protocol_strategy::Strategy;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;
/// v60.28 H7 — hard cap on a single non-stream HTTP response body. A
/// hostile or runaway upstream must not be allowed to push us into OOM.
const MAX_RESPONSE_BODY_BYTES: usize = 32 << 20;

/// Concrete BYOM adapter for Anthropic's Messages API.
///
/// `Debug` redacts `api_key`; printing the struct (e.g. in a panic message)
/// must never leak the secret. `Client` and `ConformanceRingBuffer` are
/// rendered with `non_exhaustive` placeholders to keep noise down.
pub struct AnthropicAdapter {
    model_id: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    capabilities: Capabilities,
    http: Client,
    ring: Arc<Mutex<ConformanceRingBuffer>>,
}

impl AnthropicAdapter {
    /// New adapter with explicit credentials. The `model_id` is the
    /// `<provider>:<model>` form the cost ledger expects, e.g.
    /// `anthropic:claude-opus-4-7`. The provider-side model name (what we
    /// send on the wire) is the part after the colon.
    pub fn new(api_key: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            capabilities: Capabilities {
                native_tool_use: CapabilityClaim::Supported,
                streaming: CapabilityClaim::Supported,
                vision: CapabilityClaim::Unsupported,
                prompt_cache: CapabilityClaim::Unsupported,
                structured_output: CapabilityClaim::Supported,
                long_context: CapabilityClaim::Supported,
                context_window_tokens: 200_000,
            },
            http: Client::builder()
                .timeout(Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest::Client::builder default config is infallible"),
            ring: Arc::new(Mutex::new(ConformanceRingBuffer::new())),
        }
    }

    /// Override the base URL — used exclusively by tests to point the
    /// client at a wiremock stub. **Do not call from production code.**
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the `max_tokens` cap Anthropic requires on every request.
    /// Defaults to the module-private `DEFAULT_MAX_TOKENS` constant.
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Read `ANTHROPIC_API_KEY` from the environment. The CLI binary
    /// uses this; tests use [`AnthropicAdapter::new`] directly.
    pub fn from_env(model_id: impl Into<String>) -> Result<Self, AdapterError> {
        let key = std::env::var(API_KEY_ENV).map_err(|_| {
            AdapterError::NotConfigured(format!(
                "{API_KEY_ENV} is not set; export it or use `atelier login anthropic` (Phase E)"
            ))
        })?;
        if key.trim().is_empty() {
            return Err(AdapterError::NotConfigured(format!(
                "{API_KEY_ENV} is empty"
            )));
        }
        Ok(Self::new(key, model_id))
    }

    /// Return the provider-side model name (the part after `<provider>:`).
    /// Used both for outgoing wire requests and for `Debug` so the test
    /// suite can assert on the parsed model without exposing the key.
    pub fn provider_model_name(&self) -> &str {
        self.model_id
            .split_once(':')
            .map(|(_, m)| m)
            .unwrap_or(&self.model_id)
    }

    fn build_request_body(&self, messages: &[Message], tools: &[ToolSpec], stream: bool) -> Value {
        let (system_text, msgs) = split_system_and_messages(messages);
        let mut body = json!({
            "model": self.provider_model_name(),
            "max_tokens": self.max_tokens,
            "messages": msgs,
            "stream": stream,
        });
        if !system_text.is_empty() {
            body["system"] = Value::String(system_text);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.input_schema,
                        })
                    })
                    .collect(),
            );
        }
        body
    }
}

impl std::fmt::Debug for AnthropicAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicAdapter")
            .field("model_id", &self.model_id)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Adapter for AnthropicAdapter {
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
        // §1: "char/4 fallback with one warning per session." The harness
        // owns the warning; we report the source so it knows. The real
        // `count_tokens` endpoint will be wired in a separate session.
        let chars: usize = messages.iter().map(|m| m.content.chars().count()).sum();
        let approx = chars.div_ceil(4) as u32;
        Ok(TokenCount {
            count: approx,
            source: TokenSource::Approx,
        })
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, AdapterError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = self.build_request_body(messages, tools, false);
        let started = std::time::Instant::now();
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
        let status = resp.status();
        // Snapshot headers before bytes() consumes the response. Needed
        // for Retry-After on 429.
        let headers = resp.headers().clone();
        let body_bytes = read_capped_body(resp).await?;
        if !status.is_success() {
            return Err(map_http_error(status, &headers, &body_bytes));
        }
        let parsed: AnthropicMessage = serde_json::from_slice(&body_bytes)
            .map_err(|e| AdapterError::Malformed(format!("non-stream body: {e}")))?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(parsed.into_chat_response(latency_ms))
    }

    /// v60.9 — Anthropic-flavoured few-shot override.
    ///
    /// * `NativeTool` → `None`. Claude's `tool_use` block is the canonical
    ///   carrier for the §2 envelope; the shared baseline already prompts
    ///   for it correctly.
    /// * `JsonSentinel` → a short user/assistant pair that emphasises the
    ///   exact `<<<harness_meta>>>` / `<<<end>>>` sentinel pair Claude
    ///   follows most reliably when its native tool-use channel is
    ///   unavailable (e.g., a long-context degradation). We include the
    ///   sentinel pair inline so the model sees the carrier explicitly
    ///   rather than relying on the prose description alone.
    /// * `RegexProse` → `None`. The prose fallback is lossy by design and
    ///   the shared baseline already covers it; no adapter-specific
    ///   phrasing improves it.
    fn few_shot_override(&self, strategy: Strategy) -> Option<Vec<Message>> {
        match strategy {
            Strategy::JsonSentinel => Some(vec![
                Message::text(Role::User, "Rename `foo` to `bar` in utils.py."),
                Message::text(
                    Role::Assistant,
                    "Renamed `foo` to `bar` in `utils.py`.\n\n\
                     <<<harness_meta>>>\
                     {\"claimed_changes\":[\
                     {\"path\":\"utils.py\",\"kind\":\"edit\",\
                     \"summary\":\"Renamed foo to bar\"}]}\
                     <<<end>>>",
                ),
            ]),
            Strategy::NativeTool | Strategy::RegexProse => None,
        }
    }

    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChunkStream, AdapterError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = self.build_request_body(messages, tools, true);
        let started = std::time::Instant::now();
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            // Snapshot headers before bytes() consumes the response.
            let headers = resp.headers().clone();
            let body_bytes = read_capped_body(resp).await?;
            return Err(map_http_error(status, &headers, &body_bytes));
        }
        let body_stream: BodyStream = Box::pin(resp.bytes_stream());
        let source = AnthropicSseSource::new(body_stream, started);
        Ok(ChunkStream::from_inner(Box::new(source)))
    }
}

// ---------- Request shaping ----------

/// Anthropic requires `system` outside the `messages` array. We accept a
/// homogeneous `&[Message]` from the harness and split here. Multiple
/// system messages concatenate with a blank line.
fn split_system_and_messages(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&m.content);
            }
            Role::User => out.push(json!({
                "role": "user",
                "content": m.content,
            })),
            Role::Assistant => {
                // If the assistant turn included tool_use blocks, re-emit
                // them alongside any text in the order: text first, then
                // tool_use blocks. The model's prior tool_use ids MUST
                // round-trip exactly because the subsequent tool_result
                // blocks reference them by id (Anthropic's protocol
                // rejects unmatched ids). Pre-P5 this flattened to
                // text-only and broke multi-turn tool flows.
                if m.tool_calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": m.content,
                    }));
                } else {
                    let mut blocks: Vec<Value> = Vec::new();
                    if !m.content.is_empty() {
                        blocks.push(json!({
                            "type": "text",
                            "text": m.content,
                        }));
                    }
                    for tc in &m.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }));
                    }
                    out.push(json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
            }
            Role::Tool => {
                // Anthropic represents tool results as a user-role message
                // whose content is a `tool_result` block.
                let id = m.tool_call_id.clone().unwrap_or_default();
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": m.content,
                    }],
                }));
            }
        }
    }
    (system, out)
}

// ---------- Response parsing ----------

#[derive(Deserialize)]
struct AnthropicMessage {
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Map Anthropic's wire-side `stop_reason` strings onto our cross-provider
/// [`StopReason`]. Unknown values fall through to `Other`; an absent reason
/// is reported as `None` so the harness can distinguish "provider didn't
/// say" from "provider said something specific".
fn map_stop_reason(raw: Option<&str>) -> Option<StopReason> {
    let s = raw?;
    Some(match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Other,
    })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Anthropic occasionally returns other block types (e.g. `thinking`,
    /// `redacted_thinking`) we don't yet consume. Capture them silently so
    /// deny_unknown_fields doesn't fail unrelated parses.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

impl AnthropicMessage {
    fn into_chat_response(self, latency_ms: u64) -> ChatResponse {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in self.content {
            match block {
                AnthropicContentBlock::Text { text: t } => text.push_str(&t),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCallRequest {
                        id,
                        name,
                        arguments: input,
                    });
                }
                AnthropicContentBlock::Unknown => {}
            }
        }
        let strategy = if tool_calls.is_empty() {
            Strategy::JsonSentinel
        } else {
            Strategy::NativeTool
        };
        // §1 BYOM ledger discipline (v60.7) — `count_source` reports
        // the *actual* origin of the numbers, not a blanket optimistic
        // claim. Anthropic's Messages API always returns `usage`, but
        // a malformed / truncated response can land here without one;
        // in that case we surface `Unavailable` so the §1 cost ledger
        // and the §3 cost meter can flag the row as imprecise.
        let (usage_struct, count_source) = match self.usage {
            Some(u) => (u, TokenSource::Exact),
            None => (AnthropicUsage::default(), TokenSource::Unavailable),
        };
        let stop_reason = map_stop_reason(self.stop_reason.as_deref());
        ChatResponse {
            text,
            tool_calls,
            usage: Usage {
                prompt_tokens: usage_struct.input_tokens,
                completion_tokens: usage_struct.output_tokens,
                cached_tokens: usage_struct.cache_read_input_tokens,
                count_source,
                latency_ms: Some(latency_ms),
            },
            strategy,
            stop_reason,
        }
    }
}

// ---------- HTTP error mapping ----------

/// Parse the `Retry-After` HTTP header. Per RFC 7231 the value is either
/// "delta-seconds" (a non-negative integer) or an HTTP-date; Anthropic
/// emits the seconds form. We only parse the seconds form here — an
/// HTTP-date is rare on 429 and we fall back to the default rather than
/// pull in a date parser.
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?;
    let s = v.to_str().ok()?.trim();
    let secs: u64 = s.parse().ok()?;
    // Cap at 5 minutes to prevent a hostile server from wedging the loop
    // forever with `Retry-After: 999999`. Also floor at
    // `MIN_RATE_LIMIT_BACKOFF_MS` — a `Retry-After: 0` from a confused
    // proxy would otherwise let the caller hot-loop the API and turn
    // a brief overload into a self-inflicted DoS.
    let capped = secs.min(300);
    let ms = capped.saturating_mul(1_000);
    Some(ms.max(MIN_RATE_LIMIT_BACKOFF_MS))
}

/// Conservative default when the server omits `Retry-After` on 429. Short
/// enough to not stall the loop, long enough to let a burst clear.
const DEFAULT_RATE_LIMIT_BACKOFF_MS: u64 = 1_000;

/// Floor for `Retry-After` parsing. A server that emits
/// `Retry-After: 0` (some proxies do this when they don't actually know
/// the right value) must not let us hot-loop.
const MIN_RATE_LIMIT_BACKOFF_MS: u64 = 100;

/// v60.28 H7 — streamed accumulator with a `MAX_RESPONSE_BODY_BYTES`
/// cap. Replaces `resp.bytes().await?` so a hostile or runaway upstream
/// can't push the harness into OOM. Returns `AdapterError::Unreachable`
/// on transport failure and `AdapterError::ResponseTooLarge` if the
/// accumulated bytes would exceed the cap.
async fn read_capped_body(mut resp: reqwest::Response) -> Result<Vec<u8>, AdapterError> {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| AdapterError::Unreachable(e.to_string()))?
    {
        if buf.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(AdapterError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn map_http_error(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> AdapterError {
    // Anthropic error body shape: `{"type":"error","error":{"type":"...","message":"..."}}`
    let body_str = String::from_utf8_lossy(body).into_owned();

    // Try to lift `needed_tokens` / `limit_tokens` out of the error body
    // for ContextOverflow so the modal isn't reduced to "0 of 0". The
    // body shape is `{"error":{"message":"... 250000 tokens > 200000 ..."}}`
    // — exact phrasing isn't stable, but if Anthropic embeds numbers in
    // the message we'll extract the first two.
    let (overflow_needed, overflow_limit) = extract_overflow_numbers(&body_str);

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            AdapterError::Auth(redact_response_body(&body_str))
        }
        StatusCode::TOO_MANY_REQUESTS => AdapterError::RateLimited {
            retry_after_ms: parse_retry_after_ms(headers).unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF_MS),
        },
        s if s.is_server_error() => AdapterError::Provider {
            status: status.as_u16(),
            body: redact_response_body(&body_str),
        },
        // 400 → ContextOverflow when the body identifies the request as
        // exceeding the context window. Anthropic uses three known markers:
        //   * legacy error-type names: `prompt_too_long`, `input_too_long`
        //   * prose form: `"is too long"` (current shape)
        // We match exactly these three rather than any substring containing
        // `too_long` so a future error like `"argument too_long_to_serialize"`
        // doesn't false-positive into ContextOverflow and mask the real
        // failure.
        StatusCode::BAD_REQUEST
            if body_str.contains("prompt_too_long")
                || body_str.contains("input_too_long")
                || body_str.contains("is too long") =>
        {
            AdapterError::ContextOverflow {
                needed_tokens: overflow_needed,
                limit_tokens: overflow_limit,
            }
        }
        _ => AdapterError::Provider {
            status: status.as_u16(),
            body: redact_response_body(&body_str),
        },
    }
}

/// Best-effort extraction of `(needed, limit)` token counts from the
/// Anthropic error message. Returns `(0, 0)` if not present — the modal UI
/// renders that as "unknown / unknown" rather than a confidently-wrong
/// "0 of 0".
fn extract_overflow_numbers(body: &str) -> (u32, u32) {
    // v25.3-B: anchor on the canonical Anthropic overflow phrasing
    // rather than taking the first two ASCII-digit runs left-to-right.
    // The pre-fix positional scan would silently misreport if a future
    // error format embeds a request_id, model context length, or
    // timestamp ahead of the token numbers.
    //
    // Two shapes we accept:
    //   * "NNN tokens > MMM"  (the dominant form; the `MMM` carries the
    //     limit even when not explicitly labelled)
    //   * "NNN tokens" alone   (some error variants drop the comparator;
    //     we report `(NNN, 0)` so the UI shows "needed N, limit unknown"
    //     instead of "0 of 0")
    //
    // Compile both regexes once; static patterns can't fail compilation
    // in practice — the fallback is here only to avoid an `unwrap()`
    // that could panic on a future regex-syntax rev.
    let with_limit = match regex::Regex::new(r"\b(\d+)\s+tokens\b\s*>\s*(\d+)") {
        Ok(r) => r,
        Err(_) => return (0, 0),
    };
    if let Some(cap) = with_limit.captures(body) {
        let needed = cap
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let limit = cap
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        return (needed, limit);
    }
    let just_needed = match regex::Regex::new(r"\b(\d+)\s+tokens\b") {
        Ok(r) => r,
        Err(_) => return (0, 0),
    };
    if let Some(cap) = just_needed.captures(body) {
        let needed = cap
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        return (needed, 0);
    }
    (0, 0)
}

// ---------- SSE streaming ----------

type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Parses Anthropic SSE events into [`StreamChunk`] values incrementally.
///
/// State machine: bytes from the body stream feed a byte buffer, the
/// buffer is split into lines (per WHATWG SSE: `\r\n`, `\n`, or `\r`),
/// lines accumulate the per-event `data:` payload until a blank line is
/// seen, then the event is dispatched to [`Self::handle_event`].
///
/// The line splitter operates on raw bytes — UTF-8 decoding happens only
/// once a full event's data payload has been assembled, so a multi-byte
/// codepoint split across two TCP chunks (or any chunk boundary) never
/// corrupts a frame.
struct AnthropicSseSource {
    body: BodyStream,
    /// Raw bytes waiting to be split into lines. Capped to
    /// `MAX_SSE_BUFFER_BYTES` to prevent OOM if the server emits a giant
    /// line without a terminator.
    buffer: Vec<u8>,
    /// `data:` line bytes accumulated for the current event. Joined with
    /// `\n` per SSE (see comment in `parse_lines_into_events`).
    current_event_data: Vec<u8>,
    started: std::time::Instant,
    text_acc: String,
    tool_blocks: std::collections::HashMap<u32, ToolBlockInProgress>,
    /// Stable order of tool calls as they appeared on the wire (HashMap
    /// gives no ordering guarantee, but the harness needs FIFO so the
    /// dispatcher executes them in the order the model issued them).
    tool_order: Vec<u32>,
    usage: AnthropicUsage,
    /// v60.7 §1 BYOM ledger discipline — `true` once any wire event
    /// carried a `usage` block we successfully decoded. When the stream
    /// terminates without `usage_observed = true`, the final
    /// `ChatResponse` reports `TokenSource::Unavailable` so the §1 cost
    /// ledger and §3 cost meter can distinguish "provider said zero"
    /// from "provider said nothing."
    usage_observed: bool,
    /// Most recent `stop_reason` carried by a `message_delta` event.
    /// Propagated to the final `ChatResponse` so the harness can tell
    /// `end_turn` from `max_tokens` / `refusal`.
    stop_reason: Option<StopReason>,
    pending_chunks: std::collections::VecDeque<StreamChunk>,
    finished: bool,
}

/// Per-event-buffer cap. Anthropic events are typically a few KB; an
/// 8 MiB ceiling catches a hostile or buggy server emitting an unbounded
/// line without a terminator before it OOMs the parent.
const MAX_SSE_BUFFER_BYTES: usize = 8 << 20;
/// v60.28 H8 — per-event accumulator cap for the `current_event_data`
/// buffer. Distinct from `MAX_SSE_BUFFER_BYTES` (which caps the raw
/// incoming line buffer); this caps the assembled `data:` payload bytes
/// for a single event before it's dispatched.
const MAX_SSE_EVENT_BYTES: usize = 8 << 20;

struct ToolBlockInProgress {
    id: String,
    name: String,
    partial_json: String,
}

/// Result of attempting to take one line off the front of `buffer`.
enum LineOutcome {
    /// One full line (excluding terminator); empty `Vec` indicates the
    /// blank-line event terminator.
    Got(Vec<u8>),
    /// Not enough bytes yet — caller should pull more from the body
    /// stream. Returned when the buffer ends with a bare `\r` (which
    /// could still grow into a `\r\n`).
    NeedMore,
}

impl AnthropicSseSource {
    fn new(body: BodyStream, started: std::time::Instant) -> Self {
        Self {
            body,
            buffer: Vec::with_capacity(4096),
            current_event_data: Vec::new(),
            started,
            text_acc: String::new(),
            tool_blocks: std::collections::HashMap::new(),
            tool_order: Vec::new(),
            usage: AnthropicUsage::default(),
            usage_observed: false,
            stop_reason: None,
            pending_chunks: std::collections::VecDeque::new(),
            finished: false,
        }
    }

    /// Drain one SSE line from the head of `buffer`. Per WHATWG SSE,
    /// lines may end with `\r\n`, `\n`, or a lone `\r`. We find the first
    /// CR or LF; if it's CR but the next byte hasn't arrived yet, we
    /// report `NeedMore` so we don't misclassify a `\r\n` chunk-split as
    /// a lone-`\r` line ending.
    fn take_line(&mut self, at_eof: bool) -> LineOutcome {
        let nl = self.buffer.iter().position(|&b| b == b'\n' || b == b'\r');
        let idx = match nl {
            Some(i) => i,
            None => {
                // No terminator in buffer. At EOF a non-spec server may
                // have closed mid-line; treat the remaining bytes as a
                // final unterminated line so we don't silently drop
                // the trailing data.
                if at_eof && !self.buffer.is_empty() {
                    let line = std::mem::take(&mut self.buffer);
                    return LineOutcome::Got(line);
                }
                return LineOutcome::NeedMore;
            }
        };
        // CR may be the first byte of CRLF. If we have only the CR and
        // more bytes might still arrive, wait.
        let is_cr = self.buffer[idx] == b'\r';
        if is_cr && idx + 1 >= self.buffer.len() && !at_eof {
            return LineOutcome::NeedMore;
        }
        let terminator_len =
            if is_cr && idx + 1 < self.buffer.len() && self.buffer[idx + 1] == b'\n' {
                2
            } else {
                1
            };
        let line: Vec<u8> = self.buffer.drain(..idx).collect();
        self.buffer.drain(..terminator_len);
        LineOutcome::Got(line)
    }

    /// Pump the line splitter over whatever is currently in `buffer`,
    /// accumulating `data:` line bytes into `current_event_data` and
    /// dispatching events when a blank-line terminator is seen. The
    /// `at_eof` flag tells the line splitter that a trailing `\r` should
    /// be treated as a complete terminator (otherwise we'd wait forever
    /// for a `\n` that won't come).
    fn drain_buffer(&mut self, at_eof: bool) {
        loop {
            match self.take_line(at_eof) {
                LineOutcome::NeedMore => {
                    // At EOF: a server that flushed a `data:` line
                    // followed by stream-close (no terminating blank
                    // line) would leave us with a pending event in
                    // `current_event_data` and no further lines to
                    // consume. Dispatch it as if a blank line had
                    // arrived — robustness against non-spec servers.
                    if at_eof && !self.current_event_data.is_empty() {
                        let data = std::mem::take(&mut self.current_event_data);
                        if let Ok(s) = String::from_utf8(data) {
                            self.handle_event(&s);
                        }
                    }
                    return;
                }
                LineOutcome::Got(line) if line.is_empty() => {
                    // Blank line = event terminator.
                    if !self.current_event_data.is_empty() {
                        let data = std::mem::take(&mut self.current_event_data);
                        match String::from_utf8(data) {
                            Ok(s) => self.handle_event(&s),
                            Err(_) => {
                                self.pending_chunks.push_back(StreamChunk::Error {
                                    error: AdapterError::Malformed(
                                        "SSE event payload was not valid UTF-8".into(),
                                    ),
                                });
                                self.finished = true;
                                return;
                            }
                        }
                    }
                    // else: empty event (keep-alive comment terminator);
                    // loop and try the next line.
                }
                LineOutcome::Got(line) => {
                    // Comment line per SSE spec (starts with ':').
                    if line.first() == Some(&b':') {
                        continue;
                    }
                    // Field name = bytes up to (but not including) first
                    // ':'. After ':', skip one optional space, then take
                    // the rest as the value.
                    let (field, value) = match line.iter().position(|&b| b == b':') {
                        Some(i) => {
                            let (f, rest) = line.split_at(i);
                            // rest[0] is ':'; skip it, then skip one optional space.
                            let mut v = &rest[1..];
                            if v.first() == Some(&b' ') {
                                v = &v[1..];
                            }
                            (f, v)
                        }
                        // SSE spec: a line with no ':' is the field name
                        // with empty value. Anthropic doesn't use this
                        // shape; ignore.
                        None => continue,
                    };
                    if field == b"data" {
                        let extra = if self.current_event_data.is_empty() {
                            value.len()
                        } else {
                            value.len() + 1
                        };
                        if self.current_event_data.len().saturating_add(extra) > MAX_SSE_EVENT_BYTES
                        {
                            self.pending_chunks.push_back(StreamChunk::Error {
                                error: AdapterError::SseEventTooLarge {
                                    limit: MAX_SSE_EVENT_BYTES,
                                },
                            });
                            self.finished = true;
                            return;
                        }
                        if !self.current_event_data.is_empty() {
                            self.current_event_data.push(b'\n');
                        }
                        self.current_event_data.extend_from_slice(value);
                    }
                    // `event:`, `id:`, `retry:` ignored — Anthropic's JSON
                    // already carries the type.
                }
            }
        }
    }

    fn handle_event(&mut self, data: &str) {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            // Non-JSON event mid-stream. Emit Error and stop. We could
            // preserve `text_acc` / `tool_blocks` here by also pushing a
            // partial `Complete`, but the default `chat()` returns on the
            // first Complete-or-Error, so a Complete-then-Error pair
            // would silently rubber-stamp the malformed turn as
            // successful. A streaming consumer that wants partial state
            // can read it off the source directly before propagating the
            // Error to its caller.
            self.pending_chunks.push_back(StreamChunk::Error {
                error: AdapterError::Malformed(format!("non-JSON SSE event: {data}")),
            });
            self.finished = true;
            return;
        };
        let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
            return;
        };
        match ty {
            "ping" | "message_start" => {
                // message_start carries initial usage; capture if present.
                if let Some(msg) = v.get("message") {
                    if let Some(u) = msg.get("usage") {
                        if let Ok(parsed) = serde_json::from_value::<AnthropicUsage>(u.clone()) {
                            // Only overwrite if we don't already have output
                            // tokens (message_delta is authoritative for
                            // output, message_start for input).
                            self.usage.input_tokens = parsed.input_tokens;
                            if parsed.cache_read_input_tokens.is_some() {
                                self.usage.cache_read_input_tokens = parsed.cache_read_input_tokens;
                            }
                            // §1: usage was actually observed on the
                            // wire — `count_source` will be `Exact`.
                            self.usage_observed = true;
                        }
                    }
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                if let Some(block) = v.get("content_block") {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("tool_use") => {
                            let id = block
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|s| s.as_str())
                                .unwrap_or_default()
                                .to_string();
                            self.pending_chunks.push_back(StreamChunk::ToolCallStarted {
                                id: id.clone(),
                                name: name.clone(),
                            });
                            self.tool_blocks.insert(
                                idx,
                                ToolBlockInProgress {
                                    id,
                                    name,
                                    partial_json: String::new(),
                                },
                            );
                            self.tool_order.push(idx);
                        }
                        Some("text") => {
                            // text already in `text` field; appended on deltas.
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                if !t.is_empty() {
                                    self.text_acc.push_str(t);
                                    self.pending_chunks.push_back(StreamChunk::Text {
                                        delta: t.to_string(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                if let Some(delta) = v.get("delta") {
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(|t| t.as_str()) {
                                self.text_acc.push_str(t);
                                self.pending_chunks.push_back(StreamChunk::Text {
                                    delta: t.to_string(),
                                });
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(t) = delta.get("partial_json").and_then(|t| t.as_str()) {
                                if let Some(block) = self.tool_blocks.get_mut(&idx) {
                                    block.partial_json.push_str(t);
                                    self.pending_chunks.push_back(StreamChunk::ToolCallDelta {
                                        id: block.id.clone(),
                                        args_delta: t.to_string(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                if let Some(block) = self.tool_blocks.get_mut(&idx) {
                    // Empty input → Anthropic sends `{}`. Tolerate both.
                    let raw = if block.partial_json.is_empty() {
                        "{}".to_string()
                    } else {
                        block.partial_json.clone()
                    };
                    let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                    self.pending_chunks
                        .push_back(StreamChunk::ToolCallCompleted {
                            id: block.id.clone(),
                            arguments: args,
                        });
                }
            }
            "message_delta" => {
                // Carries final usage (`output_tokens`) and stop_reason.
                // The `usage` sub-object on `message_delta` only reports
                // output_tokens — pre-existing input_tokens and
                // cache_read_input_tokens captured at `message_start`
                // must NOT be clobbered, so we extract only the output
                // figure rather than deserializing the whole struct.
                //
                // v25.3-B: always overwrite (was: gated on `> 0`).
                // Anthropic reports output_tokens monotonically and the
                // last `message_delta` is authoritative — clobbering
                // with the latest value is correct. The old `> 0` guard
                // would leave stale values around if a mid-stream
                // `message_delta` ever reported `0` (which shouldn't
                // happen per protocol, but we shouldn't rely on it).
                if let Some(u) = v.get("usage") {
                    if let Some(out) = u.get("output_tokens").and_then(|n| n.as_u64()) {
                        self.usage.output_tokens = out as u32;
                        // §1: usage was actually observed on the wire.
                        self.usage_observed = true;
                    }
                }
                // stop_reason lives in `delta.stop_reason` for streaming
                // (matches the non-streaming response shape).
                if let Some(reason) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.stop_reason = map_stop_reason(Some(reason));
                }
            }
            "message_stop" => {
                let response = self.assemble_response();
                self.pending_chunks
                    .push_back(StreamChunk::Complete { response });
                self.finished = true;
            }
            "error" => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("anthropic stream error")
                    .to_string();
                self.pending_chunks.push_back(StreamChunk::Error {
                    error: AdapterError::Provider {
                        status: 0,
                        body: redact_response_body(&msg),
                    },
                });
                self.finished = true;
            }
            _ => {}
        }
    }

    fn assemble_response(&mut self) -> ChatResponse {
        let mut tool_calls = Vec::new();
        // Walk in wire order so the dispatcher executes in the order the
        // model issued.
        for idx in &self.tool_order {
            if let Some(block) = self.tool_blocks.get(idx) {
                let raw = if block.partial_json.is_empty() {
                    "{}".to_string()
                } else {
                    block.partial_json.clone()
                };
                let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                tool_calls.push(ToolCallRequest {
                    id: block.id.clone(),
                    name: block.name.clone(),
                    arguments: args,
                });
            }
        }
        let strategy = if tool_calls.is_empty() {
            Strategy::JsonSentinel
        } else {
            Strategy::NativeTool
        };
        let latency_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // §1 BYOM ledger discipline (v60.7) — `Exact` iff the wire
        // actually carried `usage`. A stream that completed without
        // any usage event reports `Unavailable` so the §1 cost
        // ledger downstream can flag the row honestly rather than
        // rubber-stamping `prompt_tokens=0, completion_tokens=0`
        // as ground truth.
        let count_source = if self.usage_observed {
            TokenSource::Exact
        } else {
            TokenSource::Unavailable
        };
        ChatResponse {
            text: std::mem::take(&mut self.text_acc),
            tool_calls,
            usage: Usage {
                prompt_tokens: self.usage.input_tokens,
                completion_tokens: self.usage.output_tokens,
                cached_tokens: self.usage.cache_read_input_tokens,
                count_source,
                latency_ms: Some(latency_ms),
            },
            strategy,
            stop_reason: self.stop_reason,
        }
    }
}

#[async_trait]
impl ChunkSource for AnthropicSseSource {
    async fn next(&mut self) -> Option<StreamChunk> {
        loop {
            if let Some(c) = self.pending_chunks.pop_front() {
                return Some(c);
            }
            if self.finished {
                return None;
            }
            // Drain whatever is buffered before pulling more bytes.
            self.drain_buffer(false);
            if let Some(c) = self.pending_chunks.pop_front() {
                return Some(c);
            }
            if self.finished {
                continue;
            }
            // Bounded buffer (§ deep-scan finding): a server that emits a
            // gigabyte of bytes with no terminator must not OOM the parent.
            if self.buffer.len() > MAX_SSE_BUFFER_BYTES {
                self.finished = true;
                return Some(StreamChunk::Error {
                    error: AdapterError::Malformed(format!(
                        "SSE buffer exceeded {} bytes without an event terminator",
                        MAX_SSE_BUFFER_BYTES
                    )),
                });
            }
            match self.body.next().await {
                Some(Ok(bytes)) => {
                    // Pre-check the cap BEFORE extending. The post-extend
                    // check at the top of the loop misses the case where
                    // a single chunk is itself larger than the cap (e.g. a
                    // pathological 100 MB chunk).
                    if self.buffer.len().saturating_add(bytes.len()) > MAX_SSE_BUFFER_BYTES {
                        self.finished = true;
                        return Some(StreamChunk::Error {
                            error: AdapterError::Malformed(format!(
                                "SSE chunk would push buffer past {} bytes",
                                MAX_SSE_BUFFER_BYTES
                            )),
                        });
                    }
                    self.buffer.extend_from_slice(&bytes);
                }
                Some(Err(e)) => {
                    self.finished = true;
                    return Some(StreamChunk::Error {
                        error: AdapterError::Unreachable(e.to_string()),
                    });
                }
                None => {
                    // Stream EOF. Flush any trailing event (with at_eof=true
                    // so a final bare `\r` counts as a terminator).
                    self.drain_buffer(true);
                    if let Some(c) = self.pending_chunks.pop_front() {
                        return Some(c);
                    }
                    self.finished = true;
                    return Some(StreamChunk::Error {
                        error: AdapterError::Malformed(
                            "anthropic stream ended without message_stop".into(),
                        ),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn user(content: &str) -> Message {
        Message::text(Role::User, content)
    }

    fn system(content: &str) -> Message {
        Message::text(Role::System, content)
    }

    fn adapter_for(server: &MockServer) -> AnthropicAdapter {
        AnthropicAdapter::new("test-key", "anthropic:claude-opus-4-7").with_base_url(server.uri())
    }

    fn sse(events: &[(&str, Value)]) -> String {
        let mut out = String::new();
        for (event, data) in events {
            out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
        }
        out
    }

    #[tokio::test]
    async fn chat_happy_path_returns_assembled_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4-7",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": " world"},
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 12, "output_tokens": 3},
            })))
            .mount(&server)
            .await;

        let resp = adapter_for(&server).chat(&[user("hi")], &[]).await.unwrap();
        assert_eq!(resp.text, "hello world");
        assert_eq!(resp.tool_calls.len(), 0);
        assert_eq!(resp.strategy, Strategy::JsonSentinel);
        assert_eq!(resp.usage.prompt_tokens, 12);
        assert_eq!(resp.usage.completion_tokens, 3);
        assert_eq!(resp.usage.count_source, TokenSource::Exact);
    }

    #[tokio::test]
    async fn chat_extracts_native_tool_use_as_tool_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_02",
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "toolu_1",
                     "name": "write_file", "input": {"path": "x", "content": "y"}},
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 5, "output_tokens": 2},
            })))
            .mount(&server)
            .await;

        let resp = adapter_for(&server)
            .chat(&[user("write x")], &[])
            .await
            .unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "toolu_1");
        assert_eq!(resp.tool_calls[0].name, "write_file");
        assert_eq!(resp.tool_calls[0].arguments["path"], "x");
        assert_eq!(resp.strategy, Strategy::NativeTool);
    }

    #[tokio::test]
    async fn chat_401_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
        assert!(err.requires_user_decision());
    }

    #[tokio::test]
    async fn chat_429_maps_to_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::RateLimited { .. }));
        assert!(!err.requires_user_decision());
    }

    #[tokio::test]
    async fn chat_5xx_maps_to_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::Provider { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_400_with_too_long_body_maps_to_context_overflow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt_too_long"}}"#,
            ))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::ContextOverflow { .. }));
        assert!(err.requires_user_decision());
    }

    #[tokio::test]
    async fn chat_malformed_body_maps_to_malformed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<<<not json>>>"))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)));
    }

    #[tokio::test]
    async fn stream_parses_text_only_response() {
        let server = MockServer::start().await;
        let body = sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":7,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let mut stream = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap();
        let mut texts = Vec::new();
        let mut final_resp = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Text { delta } => texts.push(delta),
                StreamChunk::Complete { response } => {
                    final_resp = Some(response);
                    break;
                }
                StreamChunk::Error { error } => panic!("unexpected error: {error:?}"),
                _ => {}
            }
        }
        assert_eq!(texts, vec!["hi", " there"]);
        let r = final_resp.expect("Complete chunk");
        assert_eq!(r.text, "hi there");
        assert_eq!(r.tool_calls.len(), 0);
        assert_eq!(r.usage.prompt_tokens, 7);
        assert_eq!(r.usage.completion_tokens, 2);
        assert_eq!(r.strategy, Strategy::JsonSentinel);
    }

    #[tokio::test]
    async fn stream_assembles_native_tool_use_across_input_json_deltas() {
        let server = MockServer::start().await;
        let body = sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"msg_2","usage":{"input_tokens":3,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_a","name":"write_file","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"a.txt\",\"content\":\"hi\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let mut stream = adapter_for(&server)
            .stream(&[user("write a.txt")], &[])
            .await
            .unwrap();
        let mut started = 0;
        let mut deltas = String::new();
        let mut completed = 0;
        let mut final_resp = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::ToolCallStarted { id, name } => {
                    started += 1;
                    assert_eq!(id, "toolu_a");
                    assert_eq!(name, "write_file");
                }
                StreamChunk::ToolCallDelta { args_delta, .. } => deltas.push_str(&args_delta),
                StreamChunk::ToolCallCompleted { id, arguments } => {
                    completed += 1;
                    assert_eq!(id, "toolu_a");
                    assert_eq!(arguments["path"], "a.txt");
                    assert_eq!(arguments["content"], "hi");
                }
                StreamChunk::Complete { response } => {
                    final_resp = Some(response);
                    break;
                }
                StreamChunk::Error { error } => panic!("unexpected: {error:?}"),
                StreamChunk::Text { .. } => {}
            }
        }
        assert_eq!(started, 1);
        assert_eq!(completed, 1);
        assert_eq!(deltas, r#"{"path":"a.txt","content":"hi"}"#);
        let r = final_resp.unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
        assert_eq!(r.strategy, Strategy::NativeTool);
    }

    #[tokio::test]
    async fn stream_4xx_maps_to_error_before_first_chunk() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn stream_truncated_emits_error_then_finishes() {
        // No message_stop event — the stream ends after a half-formed turn.
        let server = MockServer::start().await;
        let body = sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}),
            ),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let mut stream = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap();
        let mut saw_text = false;
        let mut saw_error = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Text { .. } => saw_text = true,
                StreamChunk::Error { error } => {
                    assert!(matches!(error, AdapterError::Malformed(_)));
                    saw_error = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_text);
        assert!(saw_error);
    }

    #[tokio::test]
    async fn stream_provider_error_event_propagates() {
        let server = MockServer::start().await;
        let body = sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":0}}}),
            ),
            (
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"please retry"}}),
            ),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let mut stream = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap();
        let mut saw = false;
        while let Some(chunk) = stream.next().await {
            if let StreamChunk::Error { error } = chunk {
                match error {
                    AdapterError::Provider { body, .. } => {
                        assert!(body.contains("please retry"));
                        saw = true;
                    }
                    other => panic!("unexpected: {other:?}"),
                }
                break;
            }
        }
        assert!(saw);
    }

    // ---------- request shaping ----------

    #[tokio::test]
    async fn request_body_splits_system_message_and_forwards_tools() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "model": "claude-opus-4-7",
                "system": "you are a coding agent",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "harness_meta"}],
                "stream": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_x",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })))
            .mount(&server)
            .await;

        let tool = ToolSpec {
            name: "harness_meta".into(),
            description: "envelope channel".into(),
            input_schema: json!({"type": "object"}),
        };
        let r = adapter_for(&server)
            .chat(
                &[system("you are a coding agent"), user("hi")],
                std::slice::from_ref(&tool),
            )
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }

    #[tokio::test]
    async fn tool_role_messages_become_tool_result_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": "calling tool"},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_q", "content": "{\"ok\":true}"}
                    ]},
                ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_y",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })))
            .mount(&server)
            .await;

        let tool_msg = Message {
            role: Role::Tool,
            content: r#"{"ok":true}"#.into(),
            tool_call_id: Some("toolu_q".into()),
            tool_calls: Vec::new(),
        };
        let r = adapter_for(&server)
            .chat(
                &[
                    user("x"),
                    Message::text(Role::Assistant, "calling tool"),
                    tool_msg,
                ],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(r.text, "done");
    }

    #[tokio::test]
    async fn count_tokens_reports_approx_source() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let t = a.count_tokens(&[user("twelve chars")]).await.unwrap();
        assert_eq!(t.source, TokenSource::Approx);
        assert!(t.count > 0);
    }

    #[test]
    fn from_env_errors_when_key_missing() {
        // SAFETY: this test sets+unsets a process-wide env var. Other tests
        // never touch ANTHROPIC_API_KEY, so the race window is empty in
        // practice; if a future test does, it must serialize with this one.
        unsafe {
            std::env::remove_var(API_KEY_ENV);
        }
        let err = AnthropicAdapter::from_env("anthropic:claude-opus-4-7").unwrap_err();
        assert!(matches!(err, AdapterError::NotConfigured(_)));
    }

    #[test]
    fn model_id_round_trips_through_constructor() {
        let a = AnthropicAdapter::new("k", "anthropic:claude-opus-4-7");
        assert_eq!(a.model_id(), "anthropic:claude-opus-4-7");
        assert_eq!(a.provider_model_name(), "claude-opus-4-7");
    }

    #[test]
    fn capabilities_match_spec_defaults() {
        let a = AnthropicAdapter::new("k", "anthropic:m");
        let caps = a.capabilities();
        assert_eq!(caps.native_tool_use, CapabilityClaim::Supported);
        assert_eq!(caps.streaming, CapabilityClaim::Supported);
        assert_eq!(caps.long_context, CapabilityClaim::Supported);
        assert_eq!(caps.context_window_tokens, 200_000);
    }

    // ---------- P5 regression: assistant tool_calls round-trip ----------

    // The audit named this at anthropic.rs:283: pre-P5, sending back a
    // prior assistant turn with tool_use blocks flattened the blocks to
    // text-only on the next request, breaking multi-turn tool flows
    // because Anthropic requires the subsequent `tool_result.tool_use_id`
    // to match a tool_use that was actually sent.
    #[tokio::test]
    async fn assistant_turn_with_tool_calls_round_trips_as_content_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // The body MUST carry the prior assistant tool_use blocks
            // as a content array, with id + name + input preserved.
            .and(wiremock::matchers::body_partial_json(json!({
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "calling shell"},
                        {
                            "type": "tool_use",
                            "id": "toolu_42",
                            "name": "shell",
                            "input": {"command": "echo hi"}
                        }
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_42", "content": "{\"exit_code\":0}"}
                    ]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_p5",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })))
            .mount(&server)
            .await;

        let assistant_with_tool_call = Message {
            role: Role::Assistant,
            content: "calling shell".into(),
            tool_call_id: None,
            tool_calls: vec![ToolCallRequest {
                id: "toolu_42".into(),
                name: "shell".into(),
                arguments: json!({"command": "echo hi"}),
            }],
        };
        let tool_result = Message {
            role: Role::Tool,
            content: r#"{"exit_code":0}"#.into(),
            tool_call_id: Some("toolu_42".into()),
            tool_calls: Vec::new(),
        };
        let r = adapter_for(&server)
            .chat(&[user("x"), assistant_with_tool_call, tool_result], &[])
            .await
            .unwrap();
        assert_eq!(r.text, "done");
    }

    #[tokio::test]
    async fn assistant_turn_without_tool_calls_stays_plain_string_content() {
        // Backwards compat: when an assistant message has no tool_calls,
        // we emit `content: "string"` (the legacy shape), not a
        // single-block array. Some providers special-case the string
        // form; we don't gratuitously break that.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": "just text"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_p5b",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })))
            .mount(&server)
            .await;
        let r = adapter_for(&server)
            .chat(
                &[user("x"), Message::text(Role::Assistant, "just text")],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }

    // ---------- P2 regression tests: SSE parser correctness ----------

    // The pre-fix parser scanned the byte buffer for `\n\n` to find event
    // boundaries; a payload containing literal `\n\n` (or that happened to
    // straddle a chunk so the buffer briefly looked terminated) would
    // split mid-event. The new parser is line-oriented and never confuses
    // payload bytes with frame terminators.
    //
    // Anthropic JSON never contains a raw newline in a string (JSON
    // requires `\\n`), but the test belt-and-braces against any future
    // event whose `data:` payload reuses the byte.
    #[tokio::test]
    async fn sse_parser_propagates_stop_reason_from_message_delta() {
        let server = MockServer::start().await;
        let body = sse(&[
            (
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {"usage": {"input_tokens": 10, "output_tokens": 0}}
                }),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "hi"}
                }),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "max_tokens"},
                    "usage": {"output_tokens": 3}
                }),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let mut s = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap();
        let mut response = None;
        while let Some(c) = s.next().await {
            if let StreamChunk::Complete { response: r } = c {
                response = Some(r);
                break;
            }
        }
        let r = response.expect("stream should have produced Complete");
        assert_eq!(r.stop_reason, Some(StopReason::MaxTokens));
        // Plus: message_delta usage.output_tokens DOES land, but
        // message_start's input_tokens MUST NOT be clobbered.
        assert_eq!(r.usage.prompt_tokens, 10);
        assert_eq!(r.usage.completion_tokens, 3);
    }

    #[tokio::test]
    async fn sse_parser_propagates_stop_reason_end_turn_via_non_stream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_sr",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })))
            .mount(&server)
            .await;
        let r = adapter_for(&server).chat(&[user("hi")], &[]).await.unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn sse_parser_propagates_stop_reason_refusal_via_non_stream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_rfs",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "I cannot help with that."}],
                "stop_reason": "refusal",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })))
            .mount(&server)
            .await;
        let r = adapter_for(&server).chat(&[user("hi")], &[]).await.unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::Refusal));
    }

    #[tokio::test]
    async fn sse_parser_handles_crlf_line_terminators() {
        // Servers behind some HTTP proxies normalise to CRLF; the SSE
        // spec accepts \r\n, \n, or lone \r. Same payload as the happy
        // path, but every line terminator is \r\n.
        let server = MockServer::start().await;
        let mut body = String::new();
        for line in [
            r#"event: message_start"#,
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            "",
            r#"event: content_block_start"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "",
            r#"event: content_block_delta"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            "",
            r#"event: content_block_stop"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            r#"event: message_delta"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            "",
            r#"event: message_stop"#,
            r#"data: {"type":"message_stop"}"#,
            "",
        ] {
            body.push_str(line);
            body.push_str("\r\n");
        }
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let mut s = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap();
        let mut response = None;
        while let Some(c) = s.next().await {
            if let StreamChunk::Complete { response: r } = c {
                response = Some(r);
                break;
            }
        }
        let r = response.expect("stream should have produced Complete");
        assert_eq!(r.text, "hi");
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
    }

    // Direct AnthropicSseSource exercise: a multi-byte UTF-8 codepoint
    // split across two chunks must NOT corrupt the payload. The pre-fix
    // parser ran `from_utf8_lossy` on every chunk window — a split would
    // emit U+FFFD. With the new line-oriented parser, UTF-8 decoding
    // happens only on the full assembled event payload.
    //
    // We feed bytes via a hand-rolled stream so we control the chunk
    // boundary precisely.
    #[tokio::test]
    async fn sse_parser_preserves_multibyte_utf8_split_across_chunks() {
        use futures::stream;

        // Payload includes "💯" (4-byte UTF-8: F0 9F 92 AF). We split the
        // SSE body at byte 1 of the emoji, which is the worst case for
        // any parser that converts per-chunk.
        let event = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"💯"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

event: message_stop
data: {"type":"message_stop"}

"#;
        let bytes_all: Vec<u8> = event.as_bytes().to_vec();

        // Find the emoji's first byte (0xF0) and split there + 1, so
        // chunk_a ends mid-codepoint.
        let split_at = bytes_all
            .iter()
            .position(|&b| b == 0xF0)
            .expect("emoji should be in payload")
            + 1;
        let chunk_a = bytes_all[..split_at].to_vec();
        let chunk_b = bytes_all[split_at..].to_vec();

        let body_stream = stream::iter(vec![
            Ok::<Bytes, reqwest::Error>(Bytes::from(chunk_a)),
            Ok::<Bytes, reqwest::Error>(Bytes::from(chunk_b)),
        ]);
        let src = AnthropicSseSource::new(Box::pin(body_stream), std::time::Instant::now());
        let mut s = ChunkStream::from_inner(Box::new(src));

        let mut text = String::new();
        let mut got_complete = false;
        while let Some(c) = s.next().await {
            match c {
                StreamChunk::Text { delta } => text.push_str(&delta),
                StreamChunk::Complete { response } => {
                    text = response.text;
                    got_complete = true;
                    break;
                }
                StreamChunk::Error { error } => panic!("unexpected error: {error:?}"),
                _ => {}
            }
        }
        assert!(got_complete, "stream should have completed");
        assert_eq!(text, "💯", "emoji should round-trip intact across chunks");
    }

    // The pre-fix parser scanned for `\n\n` anywhere in the byte buffer.
    // Drive the parser with a payload split byte-by-byte so the scanner
    // can never "see" a complete \n\n until the actual frame terminator.
    // The new parser is line-oriented and resilient to this.
    #[tokio::test]
    async fn sse_parser_handles_one_byte_per_chunk_stream() {
        use futures::stream;

        let event = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

event: message_stop
data: {"type":"message_stop"}

"#;
        let one_byte_chunks: Vec<Result<Bytes, reqwest::Error>> =
            event.bytes().map(|b| Ok(Bytes::from(vec![b]))).collect();
        let body_stream = stream::iter(one_byte_chunks);
        let src = AnthropicSseSource::new(Box::pin(body_stream), std::time::Instant::now());
        let mut s = ChunkStream::from_inner(Box::new(src));

        let mut text = String::new();
        let mut got_complete = false;
        while let Some(c) = s.next().await {
            match c {
                StreamChunk::Text { delta } => text.push_str(&delta),
                StreamChunk::Complete { response } => {
                    text = response.text;
                    got_complete = true;
                    break;
                }
                StreamChunk::Error { error } => panic!("unexpected error: {error:?}"),
                _ => {}
            }
        }
        assert!(got_complete);
        assert_eq!(text, "hello");
    }

    // ---------- P2 regression tests: Retry-After parsing ----------

    #[tokio::test]
    async fn chat_429_with_retry_after_header_propagates_seconds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_string(r#"{"error":{"type":"rate_limit_error"}}"#),
            )
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::RateLimited { retry_after_ms } => {
                assert_eq!(retry_after_ms, 7_000, "should parse seconds → 7000 ms");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_429_with_absurd_retry_after_is_capped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "999999")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::RateLimited { retry_after_ms } => {
                // 300s cap → 300_000 ms.
                assert_eq!(retry_after_ms, 300_000);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_429_with_zero_retry_after_is_floored_to_prevent_hot_loop() {
        // v25.2-B: a confused proxy emitting `Retry-After: 0` must not
        // let us hot-loop the API — that turns a brief overload into a
        // self-inflicted DoS. Floor to MIN_RATE_LIMIT_BACKOFF_MS.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::RateLimited { retry_after_ms } => {
                assert_eq!(retry_after_ms, MIN_RATE_LIMIT_BACKOFF_MS);
                assert!(retry_after_ms >= 100, "must not allow hot-loop");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_429_without_retry_after_uses_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string(""))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::RateLimited { retry_after_ms } => {
                assert_eq!(retry_after_ms, DEFAULT_RATE_LIMIT_BACKOFF_MS);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    // ---------- P2 regression tests: ContextOverflow numbers ----------

    #[tokio::test]
    async fn chat_400_extracts_overflow_token_counts_when_present() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
            ))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::ContextOverflow {
                needed_tokens,
                limit_tokens,
            } => {
                assert_eq!(needed_tokens, 250_000);
                assert_eq!(limit_tokens, 200_000);
            }
            other => panic!("expected ContextOverflow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_400_with_unrelated_too_long_substring_is_not_context_overflow() {
        // A 400 about something else entirely; the body contains the
        // substring `too_long` only as part of a different identifier
        // (`too_long_to_serialize`). Must NOT collapse to ContextOverflow.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"type":"invalid_request_error","message":"argument too_long_to_serialize"}}"#,
            ))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdapterError::Provider { .. }),
            "expected Provider, got {err:?}"
        );
    }

    // ---------- v60.9 few-shot override ----------

    use crate::protocol_strategy::{SENTINEL_CLOSE, SENTINEL_OPEN};

    #[test]
    fn anthropic_few_shot_override_returns_some_for_json_sentinel() {
        let a = AnthropicAdapter::new("test-key", "anthropic:claude-opus-4-7");
        let msgs = a
            .few_shot_override(Strategy::JsonSentinel)
            .expect("Anthropic overrides JsonSentinel with a sentinel-first example");
        assert_eq!(msgs.len(), 2, "user + assistant pair");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        // The override must include the literal sentinel pair so Claude
        // sees the carrier shape, not just a prose description of it.
        assert!(
            msgs[1].content.contains(SENTINEL_OPEN),
            "assistant turn must demonstrate the sentinel open: {:?}",
            msgs[1].content,
        );
        assert!(
            msgs[1].content.contains(SENTINEL_CLOSE),
            "assistant turn must demonstrate the sentinel close: {:?}",
            msgs[1].content,
        );
    }

    #[test]
    fn anthropic_few_shot_override_returns_none_for_native_tool() {
        let a = AnthropicAdapter::new("test-key", "anthropic:claude-opus-4-7");
        assert!(
            a.few_shot_override(Strategy::NativeTool).is_none(),
            "NativeTool is the canonical Claude carrier; baseline suffices"
        );
    }

    #[test]
    fn anthropic_few_shot_override_returns_none_for_regex_prose() {
        let a = AnthropicAdapter::new("test-key", "anthropic:claude-opus-4-7");
        assert!(
            a.few_shot_override(Strategy::RegexProse).is_none(),
            "RegexProse falls through to the shared baseline"
        );
    }

    // ---------- v60.28 H4 — credential redirects are not followed ----------

    #[tokio::test]
    async fn chat_does_not_follow_302_redirect_with_credentials() {
        // The cred-bearing client must refuse to auto-follow redirects so a
        // hostile upstream can't peel `x-api-key` to a different host.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1:1/sink"),
            )
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        // 302 falls through to the catch-all in map_http_error → Provider{302}.
        match err {
            AdapterError::Provider { status, .. } => assert_eq!(status, 302),
            other => panic!("expected Provider{{302}}, got {other:?}"),
        }
    }

    // ---------- v60.28 H7 — non-stream body cap ----------

    #[tokio::test]
    async fn chat_response_body_above_cap_returns_response_too_large() {
        let server = MockServer::start().await;
        // 33 MiB body — over the 32 MiB cap.
        let big = vec![b'x'; (MAX_RESPONSE_BODY_BYTES) + 1];
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(big))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::ResponseTooLarge { limit } => {
                assert_eq!(limit, MAX_RESPONSE_BODY_BYTES);
            }
            other => panic!("expected ResponseTooLarge, got {other:?}"),
        }
    }

    // ---------- v60.28 H8 — SSE per-event cap ----------

    #[tokio::test]
    async fn stream_sse_event_above_cap_emits_sse_event_too_large() {
        // Build a single SSE event whose accumulated `data:` payload
        // exceeds the cap.  We split it across ~1000 `data:` lines so
        // the line splitter has to walk many appends before tripping
        // the per-event cap (worst case for the accumulator).
        let chunk = "x".repeat(16 * 1024);
        let mut body = String::new();
        for _ in 0..((MAX_SSE_EVENT_BYTES / chunk.len()) + 8) {
            body.push_str("data: ");
            body.push_str(&chunk);
            body.push('\n');
        }
        body.push('\n');
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let mut s = adapter_for(&server)
            .stream(&[user("hi")], &[])
            .await
            .unwrap();
        let mut got_too_large = false;
        while let Some(c) = s.next().await {
            if let StreamChunk::Error {
                error: AdapterError::SseEventTooLarge { limit },
            } = &c
            {
                assert_eq!(*limit, MAX_SSE_EVENT_BYTES);
                got_too_large = true;
                break;
            }
        }
        assert!(
            got_too_large,
            "expected SseEventTooLarge from oversized event"
        );
    }
}

#[cfg(test)]
mod retry_tests {
    //! v60.34 (M24) — contract pin: the adapter never internally retries.
    //!
    //! The runner's `ContextOverflow` compact-retry loop (see
    //! `crates/atelier-cli/src/runner.rs`) owns retry policy. If the
    //! adapter were to retry internally (e.g., to mask a transient 429
    //! or a context-overflow), it would do so with a stale
    //! `messages_for_call` snapshot — defeating compaction that ran in
    //! the runner. This pin asserts each error class surfaces
    //! immediately to the runner so the runner can choose to re-project
    //! or bubble the error.
    //!
    //! Pairs with v60.32 M03's runner-side fix.
    use super::*;
    use crate::adapter::{Adapter, Message, Role};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> AnthropicAdapter {
        AnthropicAdapter::new("test-key", "anthropic:claude-opus-4-7").with_base_url(server.uri())
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    #[tokio::test]
    async fn chat_does_not_internally_retry_on_429() {
        // If the adapter retried internally, this would either succeed
        // (after some delay) or be a different error. The pin: 429
        // surfaces RateLimited on the FIRST call, no retry, runner-owned.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .expect(1) // exactly one request — no internal retry
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdapterError::RateLimited { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn chat_does_not_internally_retry_on_context_overflow() {
        // Context-overflow MUST bubble to the runner so the runner's
        // compaction path can re-project messages_for_call. An internal
        // retry would re-send the stale payload.
        let server = MockServer::start().await;
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string(body))
            .expect(1) // exactly one request — overflow bubbles, no retry
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdapterError::ContextOverflow { .. }),
            "got {err:?}"
        );
    }
}
