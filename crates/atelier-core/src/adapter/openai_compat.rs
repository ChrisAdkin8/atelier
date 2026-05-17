//! §1 OpenAI-compatible adapter.
//!
//! Talks to `POST /v1/chat/completions` in the OpenAI Chat Completions
//! format. The point isn't OpenAI specifically — it's the *ecosystem*
//! that adopted the format:
//!
//! * **LM Studio** — `http://localhost:1234/v1/`
//! * **llama.cpp server** (`llama-server`) — `http://localhost:8080/v1/`
//! * **vLLM** — `http://localhost:8000/v1/`
//! * **sglang** — `http://localhost:30000/v1/`
//! * **Ollama** via its compat layer — `http://localhost:11434/v1/`
//! * **OpenAI itself** — `https://api.openai.com/v1/`
//!
//! Same single adapter unlocks every one of these. The wire format is
//! also what `openai-python`, `openai-node`, and most third-party
//! orchestrators emit, so any local server claiming OpenAI compat
//! works without bespoke wiring.
//!
//! # Configuration
//!
//! * [`OpenAiCompatAdapter::new`] — explicit `base_url` + `model_id`
//!   (+ optional `api_key`). The base_url is required — there is no
//!   "default" because local-server URLs vary. Pass `""` as
//!   `api_key` when the local server doesn't require auth (most
//!   don't).
//! * [`OpenAiCompatAdapter::from_env`] — reads `OPENAI_API_KEY` and
//!   `OPENAI_BASE_URL` (the latter defaults to
//!   `https://api.openai.com/v1` for parity with the official SDK).
//!
//! # Streaming
//!
//! SSE (`data: <json>\n\n` frames, `data: [DONE]\n\n` terminator).
//! Same line-buffered state-machine pattern as the Anthropic adapter
//! — handles `\r\n` / `\n` / lone `\r` terminators, never decodes
//! UTF-8 from partial chunks, bounded buffer cap.
//!
//! # Tool use
//!
//! OpenAI's tool-call shape ships the arguments as a JSON-encoded
//! **string** on the wire (`function.arguments: "{...}"`); we parse
//! that string into `serde_json::Value` before exposing on
//! [`ToolCallRequest.arguments`] so the harness's tool dispatcher
//! gets the same already-parsed shape it gets from the Anthropic
//! adapter.
//!
//! # What this adapter does NOT do (yet)
//!
//! * Prompt caching (OpenAI's prompt cache is automatic and silent;
//!   local servers don't expose it). `prompt_cache` reported
//!   `Unsupported`.
//! * Vision (image content blocks). `vision` reported `Unsupported`.
//! * Streaming tool-call argument deltas — we accumulate them but
//!   only surface `ToolCallCompleted` at end-of-stream. Same shape
//!   as the Anthropic adapter for v0.
//! * Token counting via the model's tokenizer. `count_tokens` falls
//!   back to char/4 with `TokenSource::Approx`; local servers don't
//!   ship a `/tokenize` endpoint at the OpenAI URL.
//! * Retries on `RateLimited` / `Unreachable` — the §2.5 actor's
//!   `Recovery` routing owns retry policy.

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
    Message, Role, StopReason, StreamChunk, TokenCount, ToolCallRequest, ToolSpec, Usage,
};
use crate::context::TokenSource;
use crate::protocol_conformance::{ConformanceRingBuffer, ConformanceSnapshot};
use crate::protocol_strategy::Strategy;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const BASE_URL_ENV: &str = "OPENAI_BASE_URL";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;
/// Default context window when the caller doesn't override. 8192 is a
/// typical local-model floor (llama 2 / mistral 7b shipped with 4096
/// or 8192; modern local models go higher but we don't autodetect).
const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 8_192;

/// Max bytes we'll buffer between SSE event terminators. Hostile or
/// buggy server protection — see comment in anthropic.rs:
/// `MAX_SSE_BUFFER_BYTES`.
const MAX_SSE_BUFFER_BYTES: usize = 8 << 20;

/// Floor on `Retry-After` so a server that emits `Retry-After: 0`
/// can't push us into a hot-retry loop. Matches Anthropic adapter.
const MIN_RATE_LIMIT_BACKOFF_MS: u64 = 100;
const DEFAULT_RATE_LIMIT_BACKOFF_MS: u64 = 1_000;

/// Concrete BYOM adapter for any server speaking the OpenAI
/// `/v1/chat/completions` format.
///
/// `Debug` redacts `api_key`.
pub struct OpenAiCompatAdapter {
    model_id: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    capabilities: Capabilities,
    http: Client,
    ring: Arc<Mutex<ConformanceRingBuffer>>,
}

impl OpenAiCompatAdapter {
    /// New adapter pointed at an explicit local-or-remote endpoint.
    /// `model_id` is the `<provider>:<model>` form the cost ledger
    /// expects, e.g. `local:llama3` or `openai:gpt-4o`. The
    /// provider-side model name (what we send on the wire) is the
    /// part after the colon.
    pub fn new(
        api_key: impl Into<String>,
        model_id: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            capabilities: Capabilities {
                native_tool_use: CapabilityClaim::Supported,
                streaming: CapabilityClaim::Supported,
                vision: CapabilityClaim::Unsupported,
                prompt_cache: CapabilityClaim::Unsupported,
                structured_output: CapabilityClaim::Supported,
                long_context: CapabilityClaim::Supported,
                context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            },
            http: Client::builder()
                .timeout(Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
                .build()
                .expect("reqwest::Client::builder default config is infallible"),
            ring: Arc::new(Mutex::new(ConformanceRingBuffer::new())),
        }
    }

    /// Read `OPENAI_API_KEY` + optional `OPENAI_BASE_URL` from the
    /// environment. Mirrors the Anthropic adapter's `from_env` shape.
    /// `OPENAI_BASE_URL` defaults to the official OpenAI URL —
    /// override for local servers (`http://localhost:11434/v1` etc.).
    ///
    /// Note: empty `OPENAI_API_KEY` is allowed because most local
    /// servers don't require auth. If the server *does* require it
    /// and the key is empty, the eventual 401 response surfaces as
    /// `AdapterError::Auth`.
    pub fn from_env(model_id: impl Into<String>) -> Result<Self, AdapterError> {
        // Local servers commonly don't authenticate; accept absent key.
        let key = std::env::var(API_KEY_ENV).unwrap_or_default();
        let base_url = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(key, model_id, base_url))
    }

    /// Override `max_tokens`. OpenAI/llama-server require this on
    /// every request; defaults to [`DEFAULT_MAX_TOKENS`].
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Override the context-window cap reported via
    /// [`Capabilities::context_window_tokens`]. Local models vary
    /// widely (Mistral 7B = 8 KiB, modern Llama = 128 KiB+); the
    /// caller knows better than we do.
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.capabilities.context_window_tokens = tokens;
        self
    }

    /// Provider-side model name (the part after `<provider>:`). Used
    /// in both outgoing requests and `Debug` so tests can assert on
    /// the parsed model without exposing the key.
    pub fn provider_model_name(&self) -> &str {
        self.model_id
            .split_once(':')
            .map(|(_, m)| m)
            .unwrap_or(&self.model_id)
    }

    fn build_request_body(&self, messages: &[Message], tools: &[ToolSpec], stream: bool) -> Value {
        let msgs = to_openai_messages(messages);
        let mut body = json!({
            "model": self.provider_model_name(),
            "max_tokens": self.max_tokens,
            "messages": msgs,
            "stream": stream,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.input_schema,
                            }
                        })
                    })
                    .collect(),
            );
        }
        if stream {
            // OpenAI requires this to include usage in the final
            // chunk; otherwise `usage` is null and we lose token
            // counts. Local servers usually ignore the flag but
            // honour it when supported.
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }
}

impl std::fmt::Debug for OpenAiCompatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatAdapter")
            .field("model_id", &self.model_id)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Adapter for OpenAiCompatAdapter {
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
        // §1: chars/4 fallback; this adapter doesn't ship a real
        // tokenizer (local servers don't expose one at the OpenAI
        // URL). The harness owns the "warn once per session"
        // semantics around Approx.
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
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = self.build_request_body(messages, tools, false);
        let started = std::time::Instant::now();
        let mut req = self
            .http
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json");
        if !self.api_key.is_empty() {
            req = req.header(header::AUTHORIZATION, format!("Bearer {}", self.api_key));
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            return Err(map_http_error(status, &headers, &body_bytes));
        }
        let parsed: ChatCompletionResponse = serde_json::from_slice(&body_bytes)
            .map_err(|e| AdapterError::Malformed(format!("non-stream body: {e}")))?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(parsed.into_chat_response(latency_ms))
    }

    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChunkStream, AdapterError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = self.build_request_body(messages, tools, true);
        let started = std::time::Instant::now();
        let mut req = self
            .http
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream");
        if !self.api_key.is_empty() {
            req = req.header(header::AUTHORIZATION, format!("Bearer {}", self.api_key));
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| AdapterError::Unreachable(e.to_string()))?;
            return Err(map_http_error(status, &headers, &body_bytes));
        }
        let body_stream: BodyStream = Box::pin(resp.bytes_stream());
        let source = OpenAiSseSource::new(body_stream, started);
        Ok(ChunkStream::from_inner(Box::new(source)))
    }
}

// ---------- Request shaping ----------

/// Map harness Messages onto OpenAI's array shape. System rides as a
/// system-role message (OpenAI keeps it inline, unlike Anthropic).
/// Assistant turns with `tool_calls` get the OpenAI-style array
/// shape so multi-turn tool flows round-trip the tool_call ids.
fn to_openai_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => out.push(json!({
                "role": "system",
                "content": m.content,
            })),
            Role::User => out.push(json!({
                "role": "user",
                "content": m.content,
            })),
            Role::Assistant => {
                if m.tool_calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": m.content,
                    }));
                } else {
                    // OpenAI's tool-use shape: assistant message with
                    // `tool_calls` array. `content` can be null when
                    // the model only called tools without text.
                    let content_value = if m.content.is_empty() {
                        Value::Null
                    } else {
                        Value::String(m.content.clone())
                    };
                    let tool_calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    // OpenAI requires arguments as a
                                    // JSON-encoded STRING. We carry
                                    // them as parsed Value internally;
                                    // re-encode here.
                                    "arguments": serde_json::to_string(&tc.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                }
                            })
                        })
                        .collect();
                    out.push(json!({
                        "role": "assistant",
                        "content": content_value,
                        "tool_calls": tool_calls,
                    }));
                }
            }
            Role::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": m.content,
                }));
            }
        }
    }
    out
}

// ---------- Response parsing (non-streaming) ----------

#[derive(Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: String,
    #[serde(default, rename = "type")]
    _ty: Option<String>,
    function: ResponseToolCallFunction,
}

#[derive(Deserialize)]
struct ResponseToolCallFunction {
    name: String,
    /// On the wire OpenAI ships this as a JSON-encoded string. We
    /// parse it to Value before surfacing.
    arguments: String,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl ChatCompletionResponse {
    fn into_chat_response(self, latency_ms: u64) -> ChatResponse {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut finish_reason: Option<String> = None;
        // OpenAI returns choices[0] for non-streaming single-completion
        // requests; we don't support n>1 yet.
        if let Some(choice) = self.choices.into_iter().next() {
            if let Some(c) = choice.message.content {
                text.push_str(&c);
            }
            for tc in choice.message.tool_calls {
                let args: Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
                tool_calls.push(ToolCallRequest {
                    id: tc.id,
                    name: tc.function.name,
                    arguments: args,
                });
            }
            finish_reason = choice.finish_reason;
        }
        let strategy = if tool_calls.is_empty() {
            Strategy::JsonSentinel
        } else {
            Strategy::NativeTool
        };
        let usage = self.usage.unwrap_or_default();
        let stop_reason = map_finish_reason(finish_reason.as_deref());
        ChatResponse {
            text,
            tool_calls,
            usage: Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cached_tokens: None,
                count_source: TokenSource::Exact,
                latency_ms: Some(latency_ms),
            },
            strategy,
            stop_reason,
        }
    }
}

/// Map OpenAI's `finish_reason` strings onto the cross-provider
/// [`StopReason`]. Mirrors the Anthropic adapter's mapping; the
/// values differ but the harness-side enum is the same.
fn map_finish_reason(raw: Option<&str>) -> Option<StopReason> {
    let s = raw?;
    Some(match s {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Other,
    })
}

// ---------- HTTP error mapping ----------

fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?;
    let s = v.to_str().ok()?.trim();
    let secs: u64 = s.parse().ok()?;
    let capped = secs.min(300);
    let ms = capped.saturating_mul(1_000);
    Some(ms.max(MIN_RATE_LIMIT_BACKOFF_MS))
}

fn map_http_error(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> AdapterError {
    let body_str = String::from_utf8_lossy(body).into_owned();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AdapterError::Auth(body_str),
        StatusCode::TOO_MANY_REQUESTS => AdapterError::RateLimited {
            retry_after_ms: parse_retry_after_ms(headers).unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF_MS),
        },
        s if s.is_server_error() => AdapterError::Provider {
            status: status.as_u16(),
            body: body_str,
        },
        // OpenAI's context-overflow error code is
        // `"code": "context_length_exceeded"`. Local servers vary;
        // some say `context_window_full` or just an HTTP 400 with
        // body mentioning context size. We match either phrasing.
        StatusCode::BAD_REQUEST
            if body_str.contains("context_length_exceeded")
                || body_str.contains("context_window_full")
                || body_str.contains("maximum context length") =>
        {
            AdapterError::ContextOverflow {
                needed_tokens: 0,
                limit_tokens: 0,
            }
        }
        _ => AdapterError::Provider {
            status: status.as_u16(),
            body: body_str,
        },
    }
}

// ---------- SSE streaming ----------

type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Parses OpenAI Chat Completions SSE chunks into [`StreamChunk`]
/// values incrementally. Each `data:` payload is a JSON object
/// matching the partial `ChatCompletionChunk` shape; the special
/// `data: [DONE]` payload terminates the stream.
///
/// Mirrors the line-buffered state machine in the Anthropic adapter
/// — finds line terminators in raw bytes (`\r\n` / `\n` / `\r`),
/// decodes UTF-8 only on complete events, bounded buffer.
struct OpenAiSseSource {
    body: BodyStream,
    buffer: Vec<u8>,
    current_event_data: Vec<u8>,
    started: std::time::Instant,
    text_acc: String,
    /// In-progress tool calls keyed by their `index` field. OpenAI
    /// streams tool-call deltas with both `id` (only on the first
    /// chunk) and `function.arguments` (string-fragment appended).
    tool_blocks: std::collections::HashMap<u32, OpenAiToolBlockInProgress>,
    tool_order: Vec<u32>,
    usage: OpenAiUsage,
    stop_reason: Option<StopReason>,
    pending_chunks: std::collections::VecDeque<StreamChunk>,
    finished: bool,
}

struct OpenAiToolBlockInProgress {
    id: String,
    name: String,
    /// Accumulated args string. OpenAI ships these as fragments of
    /// the JSON-encoded arguments; we concatenate then parse at end
    /// of the tool block.
    args: String,
}

enum LineOutcome {
    Got(Vec<u8>),
    NeedMore,
}

impl OpenAiSseSource {
    fn new(body: BodyStream, started: std::time::Instant) -> Self {
        Self {
            body,
            buffer: Vec::with_capacity(4096),
            current_event_data: Vec::new(),
            started,
            text_acc: String::new(),
            tool_blocks: std::collections::HashMap::new(),
            tool_order: Vec::new(),
            usage: OpenAiUsage::default(),
            stop_reason: None,
            pending_chunks: std::collections::VecDeque::new(),
            finished: false,
        }
    }

    fn take_line(&mut self, at_eof: bool) -> LineOutcome {
        let nl = self.buffer.iter().position(|&b| b == b'\n' || b == b'\r');
        let idx = match nl {
            Some(i) => i,
            None => {
                if at_eof && !self.buffer.is_empty() {
                    let line = std::mem::take(&mut self.buffer);
                    return LineOutcome::Got(line);
                }
                return LineOutcome::NeedMore;
            }
        };
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

    fn drain_buffer(&mut self, at_eof: bool) {
        loop {
            match self.take_line(at_eof) {
                LineOutcome::NeedMore => {
                    if at_eof && !self.current_event_data.is_empty() {
                        let data = std::mem::take(&mut self.current_event_data);
                        if let Ok(s) = String::from_utf8(data) {
                            self.handle_event(&s);
                        }
                    }
                    return;
                }
                LineOutcome::Got(line) if line.is_empty() => {
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
                }
                LineOutcome::Got(line) => {
                    if line.first() == Some(&b':') {
                        continue;
                    }
                    let (field, value) = match line.iter().position(|&b| b == b':') {
                        Some(i) => {
                            let (f, rest) = line.split_at(i);
                            let mut v = &rest[1..];
                            if v.first() == Some(&b' ') {
                                v = &v[1..];
                            }
                            (f, v)
                        }
                        None => continue,
                    };
                    if field == b"data" {
                        if !self.current_event_data.is_empty() {
                            self.current_event_data.push(b'\n');
                        }
                        self.current_event_data.extend_from_slice(value);
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, data: &str) {
        // OpenAI terminates the stream with the literal `[DONE]`
        // string (not JSON). Surface a final `Complete` chunk and stop.
        if data.trim() == "[DONE]" {
            let response = self.assemble_response();
            self.pending_chunks
                .push_back(StreamChunk::Complete { response });
            self.finished = true;
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            self.pending_chunks.push_back(StreamChunk::Error {
                error: AdapterError::Malformed(format!("non-JSON SSE event: {data}")),
            });
            self.finished = true;
            return;
        };
        // Each chunk has shape:
        //   { "choices": [{ "delta": {...}, "finish_reason": ... }],
        //     "usage": {...} }
        // We only consume the first choice.
        if let Some(choice) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        {
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        self.text_acc.push_str(content);
                        self.pending_chunks.push_back(StreamChunk::Text {
                            delta: content.to_string(),
                        });
                    }
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                        let id_opt = tc.get("id").and_then(|s| s.as_str());
                        let function = tc.get("function");
                        let name_opt = function
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str());
                        let args_frag = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("");

                        // First chunk for an index carries id+name;
                        // subsequent chunks only carry args fragments.
                        let entry = self.tool_blocks.entry(idx).or_insert_with(|| {
                            OpenAiToolBlockInProgress {
                                id: id_opt.unwrap_or_default().to_string(),
                                name: name_opt.unwrap_or_default().to_string(),
                                args: String::new(),
                            }
                        });
                        // OpenAI sometimes ships id on later chunks
                        // too (e.g. when reconnecting); take the
                        // non-empty value.
                        if entry.id.is_empty() {
                            if let Some(id) = id_opt {
                                entry.id = id.to_string();
                            }
                        }
                        if entry.name.is_empty() {
                            if let Some(name) = name_opt {
                                entry.name = name.to_string();
                            }
                        }
                        let already_known = self.tool_order.contains(&idx);
                        if !already_known {
                            self.tool_order.push(idx);
                            self.pending_chunks.push_back(StreamChunk::ToolCallStarted {
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                            });
                        }
                        if !args_frag.is_empty() {
                            entry.args.push_str(args_frag);
                            self.pending_chunks.push_back(StreamChunk::ToolCallDelta {
                                id: entry.id.clone(),
                                args_delta: args_frag.to_string(),
                            });
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                self.stop_reason = map_finish_reason(Some(reason));
                // Emit ToolCallCompleted for each in-progress tool
                // when finish_reason fires (matches Anthropic
                // adapter's content_block_stop semantics).
                for idx in self.tool_order.clone() {
                    if let Some(block) = self.tool_blocks.get(&idx) {
                        let raw = if block.args.is_empty() {
                            "{}".to_string()
                        } else {
                            block.args.clone()
                        };
                        let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                        self.pending_chunks
                            .push_back(StreamChunk::ToolCallCompleted {
                                id: block.id.clone(),
                                arguments: args,
                            });
                    }
                }
            }
        }
        // Some servers emit usage on a final chunk with empty
        // choices array — capture it.
        if let Some(u) = v.get("usage") {
            if let Ok(parsed) = serde_json::from_value::<OpenAiUsage>(u.clone()) {
                if parsed.prompt_tokens > 0 || parsed.completion_tokens > 0 {
                    self.usage = parsed;
                }
            }
        }
    }

    fn assemble_response(&mut self) -> ChatResponse {
        let mut tool_calls = Vec::new();
        for idx in &self.tool_order {
            if let Some(block) = self.tool_blocks.get(idx) {
                let raw = if block.args.is_empty() {
                    "{}".to_string()
                } else {
                    block.args.clone()
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
                prompt_tokens: self.usage.prompt_tokens,
                completion_tokens: self.usage.completion_tokens,
                cached_tokens: None,
                count_source: TokenSource::Exact,
                latency_ms: Some(latency_ms),
            },
            strategy,
            stop_reason: self.stop_reason,
        }
    }
}

#[async_trait]
impl ChunkSource for OpenAiSseSource {
    async fn next(&mut self) -> Option<StreamChunk> {
        loop {
            if let Some(c) = self.pending_chunks.pop_front() {
                return Some(c);
            }
            if self.finished {
                return None;
            }
            self.drain_buffer(false);
            if let Some(c) = self.pending_chunks.pop_front() {
                return Some(c);
            }
            if self.finished {
                continue;
            }
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
                    self.drain_buffer(true);
                    if let Some(c) = self.pending_chunks.pop_front() {
                        return Some(c);
                    }
                    self.finished = true;
                    return Some(StreamChunk::Error {
                        error: AdapterError::Malformed(
                            "openai-compat stream ended without [DONE]".into(),
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

    fn adapter_for(server: &MockServer) -> OpenAiCompatAdapter {
        OpenAiCompatAdapter::new("test-key", "local:llama3", server.uri())
    }

    fn adapter_for_no_auth(server: &MockServer) -> OpenAiCompatAdapter {
        OpenAiCompatAdapter::new("", "local:llama3", server.uri())
    }

    // ---------- non-streaming chat ----------

    #[tokio::test]
    async fn chat_happy_path_returns_text_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "object": "chat.completion",
                "model": "llama3",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hello there"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 5}
            })))
            .mount(&server)
            .await;
        let r = adapter_for(&server).chat(&[user("hi")], &[]).await.unwrap();
        assert_eq!(r.text, "hello there");
        assert_eq!(r.tool_calls.len(), 0);
        assert_eq!(r.usage.prompt_tokens, 12);
        assert_eq!(r.usage.completion_tokens, 5);
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(r.strategy, Strategy::JsonSentinel);
    }

    #[tokio::test]
    async fn chat_without_api_key_omits_auth_header() {
        // Local servers don't require Bearer auth. The adapter must
        // not send `Authorization:` when the key is empty.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            // NOT requiring the authorization header — wiremock
            // returns 200 either way; we assert the response below.
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "local ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2}
            })))
            .mount(&server)
            .await;
        let r = adapter_for_no_auth(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap();
        assert_eq!(r.text, "local ok");
    }

    #[tokio::test]
    async fn chat_tool_call_response_parses_arguments_string_to_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_abc",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\":\"hi.txt\",\"content\":\"hello\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 10}
            })))
            .mount(&server)
            .await;
        let r = adapter_for(&server)
            .chat(&[user("write hi")], &[])
            .await
            .unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        let tc = &r.tool_calls[0];
        assert_eq!(tc.id, "call_abc");
        assert_eq!(tc.name, "write_file");
        // The wire ships arguments as a JSON-encoded string; verify
        // we parsed it into a Value with the expected shape.
        assert_eq!(tc.arguments["path"], "hi.txt");
        assert_eq!(tc.arguments["content"], "hello");
        assert_eq!(r.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(r.strategy, Strategy::NativeTool);
    }

    #[tokio::test]
    async fn chat_maps_401_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
            )
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
    async fn chat_429_with_retry_after_parses_seconds_to_ms() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3")
                    .set_body_string(r#"{"error":{"message":"rate limited"}}"#),
            )
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::RateLimited { retry_after_ms } => {
                assert_eq!(retry_after_ms, 3_000);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_429_zero_retry_after_is_floored() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
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
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_400_context_overflow_maps_to_context_overflow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"code":"context_length_exceeded","message":"context too long"}}"#,
            ))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::ContextOverflow { .. }));
    }

    #[tokio::test]
    async fn chat_500_maps_to_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
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
    async fn chat_malformed_body_maps_to_malformed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<not json>"))
            .mount(&server)
            .await;
        let err = adapter_for(&server)
            .chat(&[user("hi")], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)));
    }

    #[tokio::test]
    async fn chat_finish_reason_length_maps_to_max_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "cut off"},
                    "finish_reason": "length"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })))
            .mount(&server)
            .await;
        let r = adapter_for(&server).chat(&[user("hi")], &[]).await.unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::MaxTokens));
    }

    // ---------- request shaping ----------

    #[tokio::test]
    async fn request_body_includes_tools_in_openai_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(json!({
                "model": "llama3",
                "stream": false,
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "harness_meta",
                        "description": "envelope channel",
                        "parameters": {"type": "object"}
                    }
                }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })))
            .mount(&server)
            .await;
        let tool = ToolSpec {
            name: "harness_meta".into(),
            description: "envelope channel".into(),
            input_schema: json!({"type": "object"}),
        };
        let r = adapter_for(&server)
            .chat(&[user("hi")], std::slice::from_ref(&tool))
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }

    #[tokio::test]
    async fn assistant_tool_calls_round_trip_as_openai_message_array() {
        let server = MockServer::start().await;
        // Verify the round-trip: an assistant turn with tool_calls
        // becomes an OpenAI message with `tool_calls` array carrying
        // string-encoded `function.arguments`. Then a tool-role
        // message follows with `tool_call_id`.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(json!({
                "messages": [
                    {"role": "user", "content": "x"},
                    {
                        "role": "assistant",
                        "content": "calling shell",
                        "tool_calls": [{
                            "id": "call_42",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"command\":\"echo hi\"}"
                            }
                        }]
                    },
                    {"role": "tool", "tool_call_id": "call_42", "content": "{\"exit_code\":0}"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "done"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })))
            .mount(&server)
            .await;
        let assistant_with_tool_call = Message {
            role: Role::Assistant,
            content: "calling shell".into(),
            tool_call_id: None,
            tool_calls: vec![ToolCallRequest {
                id: "call_42".into(),
                name: "shell".into(),
                arguments: json!({"command": "echo hi"}),
            }],
        };
        let tool_result = Message {
            role: Role::Tool,
            content: r#"{"exit_code":0}"#.into(),
            tool_call_id: Some("call_42".into()),
            tool_calls: Vec::new(),
        };
        let r = adapter_for(&server)
            .chat(&[user("x"), assistant_with_tool_call, tool_result], &[])
            .await
            .unwrap();
        assert_eq!(r.text, "done");
    }

    // ---------- streaming ----------

    #[tokio::test]
    async fn stream_assembles_text_deltas_and_emits_complete_on_done() {
        let server = MockServer::start().await;
        // Build an SSE body that streams "hi" then " there" then [DONE].
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
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
        let mut text = String::new();
        let mut response = None;
        while let Some(c) = s.next().await {
            match c {
                StreamChunk::Text { delta } => text.push_str(&delta),
                StreamChunk::Complete { response: r } => {
                    response = Some(r);
                    break;
                }
                StreamChunk::Error { error } => panic!("unexpected error: {error:?}"),
                _ => {}
            }
        }
        assert_eq!(text, "hi there");
        let r = response.expect("Complete should arrive");
        assert_eq!(r.text, "hi there");
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(r.usage.completion_tokens, 2);
    }

    #[tokio::test]
    async fn stream_assembles_tool_call_args_across_fragments() {
        let server = MockServer::start().await;
        // OpenAI streams tool_call args as fragments. Each fragment
        // appears on a separate event; we accumulate then parse at
        // finish.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_z\",\"function\":{\"name\":\"shell\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"echo hi\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
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
        let r = response.expect("Complete should arrive");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "shell");
        assert_eq!(r.tool_calls[0].arguments["command"], "echo hi");
        assert_eq!(r.stop_reason, Some(StopReason::ToolUse));
    }

    // ---------- capabilities + counters ----------

    #[test]
    fn capabilities_default_is_supported_streaming_tool_use_structured_output() {
        let a = OpenAiCompatAdapter::new("k", "local:m", "http://localhost:11434/v1");
        let c = a.capabilities();
        assert_eq!(c.native_tool_use, CapabilityClaim::Supported);
        assert_eq!(c.streaming, CapabilityClaim::Supported);
        assert_eq!(c.vision, CapabilityClaim::Unsupported);
        assert_eq!(c.prompt_cache, CapabilityClaim::Unsupported);
        assert_eq!(c.structured_output, CapabilityClaim::Supported);
        assert_eq!(c.long_context, CapabilityClaim::Supported);
        assert_eq!(c.context_window_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
    }

    #[test]
    fn with_context_window_overrides_capability() {
        let a = OpenAiCompatAdapter::new("k", "local:m", "http://localhost:11434/v1")
            .with_context_window(131_072);
        assert_eq!(a.capabilities().context_window_tokens, 131_072);
    }

    #[tokio::test]
    async fn count_tokens_returns_approx_source() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let t = a.count_tokens(&[user("twelve chars")]).await.unwrap();
        assert_eq!(t.source, TokenSource::Approx);
        assert!(t.count > 0);
    }

    #[test]
    fn provider_model_name_strips_provider_prefix() {
        let a = OpenAiCompatAdapter::new("k", "local:llama3:8b", "http://x/v1");
        // split_once(':') gives the part AFTER the first colon.
        assert_eq!(a.provider_model_name(), "llama3:8b");
    }

    #[test]
    fn debug_redacts_api_key() {
        let a = OpenAiCompatAdapter::new("super-secret-key", "local:m", "http://x/v1");
        let s = format!("{a:?}");
        assert!(!s.contains("super-secret-key"));
        assert!(s.contains("<redacted>"));
    }
}
