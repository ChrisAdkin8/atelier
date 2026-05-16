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
    Adapter, AdapterError, Capabilities, CapabilityClaim, ChatResponse, ChunkSource, ChunkStream,
    Message, Role, StreamChunk, TokenCount, ToolCallRequest, ToolSpec, Usage,
};
use crate::context::TokenSource;
use crate::protocol_conformance::{ConformanceRingBuffer, ConformanceSnapshot};
use crate::protocol_strategy::Strategy;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;

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
    /// Defaults to [`DEFAULT_MAX_TOKENS`].
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
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            return Err(map_http_error(status, &body_bytes));
        }
        let parsed: AnthropicMessage = serde_json::from_slice(&body_bytes)
            .map_err(|e| AdapterError::Malformed(format!("non-stream body: {e}")))?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(parsed.into_chat_response(latency_ms))
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
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
            return Err(map_http_error(status, &body_bytes));
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
            Role::Assistant => out.push(json!({
                "role": "assistant",
                "content": m.content,
            })),
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
        let usage = self.usage.unwrap_or_default();
        ChatResponse {
            text,
            tool_calls,
            usage: Usage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                cached_tokens: usage.cache_read_input_tokens,
                count_source: TokenSource::Exact,
                latency_ms: Some(latency_ms),
            },
            strategy,
        }
    }
}

// ---------- HTTP error mapping ----------

fn map_http_error(status: StatusCode, body: &[u8]) -> AdapterError {
    // Anthropic error body shape: `{"type":"error","error":{"type":"...","message":"..."}}`
    let body_str = String::from_utf8_lossy(body).into_owned();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AdapterError::Auth(body_str),
        StatusCode::TOO_MANY_REQUESTS => {
            // Anthropic includes `retry-after` in seconds on 429. Without
            // the header we conservatively default to 1s — short enough to
            // not stall the loop, long enough to let a burst clear.
            AdapterError::RateLimited {
                retry_after_ms: 1_000,
            }
        }
        s if s.is_server_error() => AdapterError::Provider {
            status: status.as_u16(),
            body: body_str,
        },
        // 400 with `input_too_long` / `prompt_too_long` → ContextOverflow.
        StatusCode::BAD_REQUEST if body_str.contains("too_long") => AdapterError::ContextOverflow {
            needed_tokens: 0,
            limit_tokens: 0,
        },
        _ => AdapterError::Provider {
            status: status.as_u16(),
            body: body_str,
        },
    }
}

// ---------- SSE streaming ----------

type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Parses Anthropic SSE events into [`StreamChunk`] values incrementally.
///
/// State machine: each SSE event (`message_start`, `content_block_*`,
/// `message_delta`, `message_stop`, `error`) is parsed off the byte stream
/// and translated into 0..N chunks queued in `pending_chunks`. `next()`
/// drains the queue, then pulls more bytes when empty.
struct AnthropicSseSource {
    body: BodyStream,
    buffer: Vec<u8>,
    started: std::time::Instant,
    text_acc: String,
    tool_blocks: std::collections::HashMap<u32, ToolBlockInProgress>,
    /// Stable order of tool calls as they appeared on the wire (HashMap
    /// gives no ordering guarantee, but the harness needs FIFO so the
    /// dispatcher executes them in the order the model issued them).
    tool_order: Vec<u32>,
    usage: AnthropicUsage,
    pending_chunks: std::collections::VecDeque<StreamChunk>,
    finished: bool,
}

struct ToolBlockInProgress {
    id: String,
    name: String,
    partial_json: String,
}

impl AnthropicSseSource {
    fn new(body: BodyStream, started: std::time::Instant) -> Self {
        Self {
            body,
            buffer: Vec::with_capacity(4096),
            started,
            text_acc: String::new(),
            tool_blocks: std::collections::HashMap::new(),
            tool_order: Vec::new(),
            usage: AnthropicUsage::default(),
            pending_chunks: std::collections::VecDeque::new(),
            finished: false,
        }
    }

    /// Try to extract one complete SSE event (`field: value\nfield: value\n\n`)
    /// from the head of the buffer. Returns the parsed payload (just the
    /// `data:` line(s) joined; `event:` is informational since the JSON
    /// already carries `type`).
    fn try_parse_event(&mut self) -> Option<String> {
        let split = self
            .buffer
            .windows(2)
            .position(|w| w == b"\n\n")
            .or_else(|| self.buffer.windows(4).position(|w| w == b"\r\n\r\n"))?;
        let (sep_len, event_len) = if self.buffer[split..].starts_with(b"\r\n\r\n") {
            (4, split)
        } else {
            (2, split)
        };
        let event_bytes = self.buffer.drain(..event_len + sep_len).collect::<Vec<_>>();
        let event_str = String::from_utf8_lossy(&event_bytes[..event_len]);
        let mut data = String::new();
        for line in event_str.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
            }
        }
        if data.is_empty() {
            // A `ping`-only event with no data payload, or a malformed
            // event. Skip it; caller will loop and parse the next.
            return Some(String::new());
        }
        Some(data)
    }

    fn handle_event(&mut self, data: &str) {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
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
                if let Some(u) = v.get("usage") {
                    if let Ok(parsed) = serde_json::from_value::<AnthropicUsage>(u.clone()) {
                        if parsed.output_tokens > 0 {
                            self.usage.output_tokens = parsed.output_tokens;
                        }
                    }
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
                        body: msg,
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
        ChatResponse {
            text: std::mem::take(&mut self.text_acc),
            tool_calls,
            usage: Usage {
                prompt_tokens: self.usage.input_tokens,
                completion_tokens: self.usage.output_tokens,
                cached_tokens: self.usage.cache_read_input_tokens,
                count_source: TokenSource::Exact,
                latency_ms: Some(latency_ms),
            },
            strategy,
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
            // Try parsing whatever is buffered before pulling more bytes.
            if let Some(data) = self.try_parse_event() {
                if !data.is_empty() {
                    self.handle_event(&data);
                }
                continue;
            }
            match self.body.next().await {
                Some(Ok(bytes)) => self.buffer.extend_from_slice(&bytes),
                Some(Err(e)) => {
                    self.finished = true;
                    return Some(StreamChunk::Error {
                        error: AdapterError::Unreachable(e.to_string()),
                    });
                }
                None => {
                    // Stream closed without `message_stop`. Emit Complete
                    // with what we have so the loop terminates rather than
                    // hanging — but record an Error first so the caller
                    // knows the turn was truncated.
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
        Message {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
        }
    }

    fn system(content: &str) -> Message {
        Message {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
        }
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
        };
        let r = adapter_for(&server)
            .chat(
                &[
                    user("x"),
                    Message {
                        role: Role::Assistant,
                        content: "calling tool".into(),
                        tool_call_id: None,
                    },
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
}
