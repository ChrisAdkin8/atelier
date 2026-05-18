//! §2 emission strategies — encode + parse.
//!
//! Spec §2 "Emission strategies, auto-selected":
//!   1. **Native tool call** (`harness_meta` tool). Cleanest.
//!   2. **JSON-mode side channel** — sentinel-bracketed
//!      `<<<harness_meta>>>{...}<<<end>>>`.
//!   3. **Regex-prose** — tagged sections. Lossy; UI badges degrade to gray.
//!
//! Each strategy is a pair: encode (turn an [`Envelope`] into the payload the
//! adapter sends to the model as a few-shot demonstration, or extracts on the
//! return path) and parse (extract an [`Envelope`] from the model's reply).
//! Strategies are downshifted by [`crate::protocol_conformance`] after
//! repeated conformance failures.
//!
//! The encoders all produce the **same** envelope JSON in different
//! wrappers, so round-tripping the same `Envelope` through any strategy
//! returns the same value (modulo lossiness of the regex-prose fallback,
//! documented inline).

use serde::{Deserialize, Serialize};

use crate::protocol::{Envelope, EnvelopeError};

/// Sentinel name advertised in every emission strategy. Mirrors the spec
/// (`harness_meta` tool name, `<<<harness_meta>>>` sentinel tag).
pub const HARNESS_META_NAME: &str = "harness_meta";

/// Opening / closing sentinels for [`Strategy::JsonSentinel`]. Picked so
/// arbitrary model prose is exceedingly unlikely to contain them
/// accidentally — triple-angle-brackets + `harness_meta` + `end`.
pub const SENTINEL_OPEN: &str = "<<<harness_meta>>>";
pub const SENTINEL_CLOSE: &str = "<<<end>>>";

/// Tag prefixes for [`Strategy::RegexProse`]. Each tag opens a section; the
/// next tag or end-of-input closes it. Lossy by design — the regex-prose
/// fallback can't carry nested structure like `plan_update.ops`.
pub const PROSE_TAG_CHANGES: &str = "CHANGED-FILES:";
pub const PROSE_TAG_DONE: &str = "DONE:";
pub const PROSE_TAG_GROUNDING: &str = "GROUNDING:";
pub const PROSE_TAG_UNCERTAINTY: &str = "UNCERTAINTY:";

/// Auto-selected emission strategy. Ordered by quality; downshifting walks
/// from native → sentinel → prose. Spec §2 "Conformance enforcement":
/// after 3 consecutive failures the harness downshifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    NativeTool,
    JsonSentinel,
    RegexProse,
}

impl Strategy {
    /// The strategy below this one in the quality stack, or `None` if this
    /// is already the lowest. Used by the conformance tracker.
    pub fn downshift(self) -> Option<Self> {
        match self {
            Self::NativeTool => Some(Self::JsonSentinel),
            Self::JsonSentinel => Some(Self::RegexProse),
            Self::RegexProse => None,
        }
    }

    /// Stable label for log lines, ledger entries, and UI badges.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeTool => "native_tool",
            Self::JsonSentinel => "json_sentinel",
            Self::RegexProse => "regex_prose",
        }
    }

    /// Tolerance rank: bigger number = more tolerant of model misbehaviour.
    /// `NativeTool` is the strictest carrier (the provider's typed
    /// tool-call channel must be honoured); `RegexProse` is the most
    /// forgiving (any tagged prose section that resembles the contract
    /// counts). The `less_tolerant_than` helper degrades in this
    /// direction.
    pub fn tolerance_rank(self) -> u8 {
        match self {
            Self::NativeTool => 0,
            Self::JsonSentinel => 1,
            Self::RegexProse => 2,
        }
    }

    /// `true` if `self` is strictly less tolerant than `other` — i.e.
    /// `self` lives higher in the quality stack and should degrade
    /// toward `other`. The degradation walk
    /// `NativeTool → JsonSentinel → RegexProse` is encoded here so a
    /// future quality-stack change is a single-table edit instead of
    /// hunting down every `match` in the runner.
    pub fn less_tolerant_than(self, other: Self) -> bool {
        self.tolerance_rank() < other.tolerance_rank()
    }
}

// ---------- native-tool ----------

/// Encode an envelope as a synthetic tool-call invocation named
/// `harness_meta`. The adapter forwards the args JSON verbatim into the
/// provider's native tool-use channel (Anthropic `tool_use`, OpenAI
/// `tool_calls`).
pub fn encode_native_tool(env: &Envelope) -> Result<NativeToolCall, StrategyError> {
    env.validate().map_err(StrategyError::Envelope)?;
    Ok(NativeToolCall {
        name: HARNESS_META_NAME.into(),
        arguments: serde_json::to_value(env).map_err(|e| StrategyError::Encode(e.to_string()))?,
    })
}

/// Parse a model-emitted tool call back into an envelope. Rejects if the
/// tool name is not `harness_meta`.
pub fn parse_native_tool(call: &NativeToolCall) -> Result<Envelope, StrategyError> {
    if call.name != HARNESS_META_NAME {
        return Err(StrategyError::WrongToolName(call.name.clone()));
    }
    Envelope::from_value(call.arguments.clone()).map_err(StrategyError::Envelope)
}

/// Shape of the synthetic tool call. Carried in `NativeTool` emission and
/// surfaced through the adapter so the provider's native tool-call channel
/// can transport it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Build the synthetic [`crate::adapter::ToolSpec`] that advertises the
/// `harness_meta` envelope channel to the model under [`Strategy::NativeTool`].
///
/// The runner prepends this to the real tool list (read_file, write_file, …)
/// when the active strategy is `NativeTool`, so the model knows the channel
/// exists. Without it, providers like Claude have no idea the harness
/// expects an envelope and stall trying to communicate "I'm done" via
/// prose alone — surfaced first by the t01 live re-probe where the model
/// completed the task, ran tests successfully, and then burned the
/// remaining turn budget describing the result with no way to signal
/// completion.
///
/// The schema mirrors `schemas/model_protocol/envelope.v1.json` so an
/// envelope round-trips cleanly through the provider's tool-use channel.
pub fn harness_meta_tool_spec() -> crate::adapter::ToolSpec {
    crate::adapter::ToolSpec {
        name: HARNESS_META_NAME.into(),
        description: "Atelier §2 protocol envelope. Call this tool to signal \
                      structured progress alongside your natural-language reply: \
                      `claimed_done: true` when the user's task is complete, \
                      `claimed_changes` listing every file you created / edited / \
                      deleted, and optionally `plan_update` / `uncertainty` / \
                      `grounding`. The harness consumes the envelope; it is not \
                      shown to the user. Always invoke `harness_meta` on the turn \
                      that completes the task, even if you also speak in prose."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "claimed_changes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["path", "kind", "summary"],
                        "properties": {
                            "path": {"type": "string"},
                            "kind": {"enum": ["edit", "create", "delete"]},
                            "summary": {"type": "string", "maxLength": 500}
                        }
                    }
                },
                "claimed_done": {"type": "boolean"},
                "uncertainty": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["about", "kind", "asks"],
                        "properties": {
                            "about": {"type": "string"},
                            "kind": {"enum": ["missing-context", "ambiguous-spec", "untestable-claim"]},
                            "asks": {"type": "string"}
                        }
                    }
                },
                "plan_update": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["ops"],
                    "properties": {
                        "ops": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["op", "step"],
                                "properties": {
                                    "op": {"enum": ["add", "remove", "reorder", "complete"]},
                                    "step": {"type": "string"}
                                }
                            }
                        }
                    }
                },
                "grounding": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["text_span", "source"],
                        "properties": {
                            "text_span": {"type": "string"},
                            "source": {"enum": ["tool:read", "tool:grep", "context:file", "guess"]}
                        }
                    }
                },
                "constraints_acknowledged": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        }),
    }
}

// ---------- json-sentinel ----------

/// Encode an envelope as JSON wrapped in sentinels. The text returned here is
/// appended to the model's assistant turn (after the natural-language reply).
pub fn encode_json_sentinel(env: &Envelope) -> Result<String, StrategyError> {
    env.validate().map_err(StrategyError::Envelope)?;
    let json = serde_json::to_string(env).map_err(|e| StrategyError::Encode(e.to_string()))?;
    Ok(format!("{SENTINEL_OPEN}{json}{SENTINEL_CLOSE}"))
}

/// Extract + parse the envelope from a model reply that wraps it in the
/// sentinel tags. Returns the envelope **and** the unwrapped natural-language
/// portion so the UI can render the two streams separately.
pub fn parse_json_sentinel(reply: &str) -> Result<JsonSentinelParse, StrategyError> {
    let start = reply
        .find(SENTINEL_OPEN)
        .ok_or(StrategyError::SentinelMissing)?;
    let after_open = start + SENTINEL_OPEN.len();
    let rest = &reply[after_open..];

    // Parse one JSON value off the front of `rest` and remember where it
    // ended. Using `StreamDeserializer::byte_offset()` gives us the exact
    // byte length of the JSON value — an embedded `<<<end>>>` inside a
    // JSON string literal is part of the value, NOT a premature close
    // tag. Pre-v25.2 we naively used `find(SENTINEL_CLOSE)` and a model
    // emitting `{"summary":"see <<<end>>> tag"}` would corrupt the parse.
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
    let value = stream
        .next()
        .ok_or_else(|| {
            StrategyError::Envelope(EnvelopeError::Parse(
                "no JSON value after sentinel open tag".into(),
            ))
        })?
        .map_err(|e| StrategyError::Envelope(EnvelopeError::Parse(e.to_string())))?;
    let json_end = stream.byte_offset();

    let env = Envelope::from_value(value).map_err(StrategyError::Envelope)?;

    // After the JSON value: optional whitespace, then the close tag.
    // Anything else means the JSON ended mid-envelope or the close tag
    // is missing entirely.
    let after_json = &rest[json_end..];
    let after_json_trimmed = after_json.trim_start();
    if !after_json_trimmed.starts_with(SENTINEL_CLOSE) {
        return Err(StrategyError::SentinelMissing);
    }
    let close_start = after_json.len() - after_json_trimmed.len();
    let close_end_in_rest = json_end + close_start + SENTINEL_CLOSE.len();

    // Spec §2: sentinel is appended at end-of-turn. Trailing whitespace is
    // OK (newlines from the wire), but any non-whitespace after the close
    // tag is either a second envelope or post-envelope chatter the
    // contract forbids.
    let trailing = rest[close_end_in_rest..].trim();
    if !trailing.is_empty() {
        return Err(StrategyError::TrailingContentAfterSentinel {
            length: trailing.len(),
            prefix: bounded_prefix(trailing, TRAILING_PREFIX_BYTES),
        });
    }

    // The natural-language portion is everything *before* the open tag.
    let prose = reply[..start].trim_end().to_string();

    Ok(JsonSentinelParse {
        envelope: env,
        prose,
    })
}

/// How many bytes of trailing content we surface in
/// [`StrategyError::TrailingContentAfterSentinel`]. 64 is enough to see
/// "looks like a second sentinel" vs. "looks like prose" at a glance
/// without leaking unbounded model output into logs.
const TRAILING_PREFIX_BYTES: usize = 64;

/// Take up to `max_bytes` of the input, suffixed with `…` if truncated.
/// Splits on a UTF-8 char boundary so we never emit invalid UTF-8 in
/// the error string.
fn bounded_prefix(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Walk back to the last char boundary ≤ max_bytes. `str::is_char_boundary`
    // is O(1), so this loop is bounded by UTF-8's 4-byte max.
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&s[..cut]);
    out.push('…');
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSentinelParse {
    pub envelope: Envelope,
    pub prose: String,
}

// ---------- regex-prose ----------

/// Encode an envelope as a tagged-section prose appendage. **Lossy** —
/// `plan_update` and `constraints_acknowledged` are not representable in this
/// strategy and are silently dropped. The conformance tracker logs the loss
/// at strategy-downshift time so the user sees that fields are missing.
pub fn encode_regex_prose(env: &Envelope) -> Result<String, StrategyError> {
    env.validate().map_err(StrategyError::Envelope)?;
    let mut out = String::new();

    if let Some(changes) = &env.claimed_changes {
        if !changes.is_empty() {
            out.push_str(PROSE_TAG_CHANGES);
            out.push('\n');
            for c in changes {
                // `<kind> <path>: <summary>` — newline-separated, easy to
                // re-parse. Newlines in summaries are flattened.
                let summary = c.summary.replace('\n', " ");
                out.push_str(&format!(
                    "  {} {}: {}\n",
                    kind_short(c.kind),
                    c.path,
                    summary
                ));
            }
        }
    }

    if let Some(done) = env.claimed_done {
        out.push_str(&format!(
            "{PROSE_TAG_DONE} {}\n",
            if done { "yes" } else { "no" }
        ));
    }

    if let Some(g) = &env.grounding {
        if !g.is_empty() {
            out.push_str(PROSE_TAG_GROUNDING);
            out.push('\n');
            for item in g {
                let text = item.text_span.replace('\n', " ");
                out.push_str(&format!("  [{}] {}\n", source_short(item.source), text));
            }
        }
    }

    if let Some(u) = &env.uncertainty {
        if !u.is_empty() {
            out.push_str(PROSE_TAG_UNCERTAINTY);
            out.push('\n');
            for item in u {
                let asks = item.asks.replace('\n', " ");
                out.push_str(&format!(
                    "  [{}] {}: {}\n",
                    uncertainty_short(item.kind),
                    item.about,
                    asks
                ));
            }
        }
    }

    Ok(out)
}

/// Best-effort parse of the prose strategy. Spec §2: "Lossy; UI badges
/// degrade to gray." Any field this parser cannot recover lands as `None`,
/// which the UI renders as the absent/gray state — never as default-OK.
pub fn parse_regex_prose(text: &str) -> Result<Envelope, StrategyError> {
    let mut env = Envelope::default();

    for section in split_sections(text) {
        match section.tag {
            PROSE_TAG_CHANGES => {
                let mut acc = Vec::new();
                for line in section.body.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(c) = parse_change_line(line) {
                        acc.push(c);
                    }
                }
                if !acc.is_empty() {
                    env.claimed_changes = Some(acc);
                }
            }
            PROSE_TAG_DONE => {
                let v = section.body.trim().to_ascii_lowercase();
                env.claimed_done = match v.as_str() {
                    "yes" | "true" => Some(true),
                    "no" | "false" => Some(false),
                    _ => None,
                };
            }
            PROSE_TAG_GROUNDING => {
                let mut acc = Vec::new();
                for line in section.body.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(g) = parse_grounding_line(line) {
                        acc.push(g);
                    }
                }
                if !acc.is_empty() {
                    env.grounding = Some(acc);
                }
            }
            PROSE_TAG_UNCERTAINTY => {
                let mut acc = Vec::new();
                for line in section.body.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(u) = parse_uncertainty_line(line) {
                        acc.push(u);
                    }
                }
                if !acc.is_empty() {
                    env.uncertainty = Some(acc);
                }
            }
            _ => {}
        }
    }

    // Validation still runs — the regex parser must not be the way to
    // smuggle a too-long summary past the schema.
    env.validate().map_err(StrategyError::Envelope)?;
    Ok(env)
}

// --- regex-prose helpers ---

struct Section<'a> {
    tag: &'a str,
    body: &'a str,
}

const ALL_TAGS: &[&str] = &[
    PROSE_TAG_CHANGES,
    PROSE_TAG_DONE,
    PROSE_TAG_GROUNDING,
    PROSE_TAG_UNCERTAINTY,
];

fn split_sections(text: &str) -> Vec<Section<'_>> {
    // Find every occurrence of every tag and sort by byte offset. Body of
    // section N is text between end-of-tag-N and start-of-tag-N+1 (or EOF).
    let mut hits: Vec<(usize, &'static str)> = Vec::new();
    for &tag in ALL_TAGS {
        let mut start = 0usize;
        while let Some(rel) = text[start..].find(tag) {
            let abs = start + rel;
            // Only count a tag if it starts at a line boundary (or doc
            // start) — prevents matching inside an unrelated prose word.
            let line_start = abs == 0 || text.as_bytes()[abs - 1] == b'\n';
            if line_start {
                hits.push((abs, tag));
            }
            start = abs + tag.len();
        }
    }
    hits.sort_by_key(|h| h.0);

    let mut out = Vec::with_capacity(hits.len());
    for (i, &(start, tag)) in hits.iter().enumerate() {
        let body_start = start + tag.len();
        let body_end = if i + 1 < hits.len() {
            hits[i + 1].0
        } else {
            text.len()
        };
        out.push(Section {
            tag,
            body: &text[body_start..body_end],
        });
    }
    out
}

fn parse_change_line(line: &str) -> Option<crate::protocol::ClaimedChange> {
    // Expected shape: "<kind> <path>: <summary>"
    let (kind_str, rest) = line.split_once(' ')?;
    let kind = match kind_str {
        "E" | "edit" => crate::protocol::ClaimedChangeKind::Edit,
        "C" | "create" => crate::protocol::ClaimedChangeKind::Create,
        "D" | "delete" => crate::protocol::ClaimedChangeKind::Delete,
        _ => return None,
    };
    let (path, summary) = rest.split_once(": ")?;
    Some(crate::protocol::ClaimedChange {
        path: path.to_string(),
        kind,
        summary: summary.to_string(),
    })
}

fn parse_grounding_line(line: &str) -> Option<crate::protocol::Grounding> {
    // Expected: "[<source>] <text>"
    let line = line.strip_prefix('[')?;
    let (src_str, rest) = line.split_once("] ")?;
    let source = match src_str {
        "R" | "tool:read" => crate::protocol::GroundingSource::ToolRead,
        "G" | "tool:grep" => crate::protocol::GroundingSource::ToolGrep,
        "F" | "context:file" => crate::protocol::GroundingSource::ContextFile,
        "?" | "guess" => crate::protocol::GroundingSource::Guess,
        _ => return None,
    };
    Some(crate::protocol::Grounding {
        text_span: rest.to_string(),
        source,
    })
}

fn parse_uncertainty_line(line: &str) -> Option<crate::protocol::Uncertainty> {
    // Expected: "[<kind>] <about>: <asks>"
    let line = line.strip_prefix('[')?;
    let (kind_str, rest) = line.split_once("] ")?;
    let kind = match kind_str {
        "M" | "missing-context" => crate::protocol::UncertaintyKind::MissingContext,
        "A" | "ambiguous-spec" => crate::protocol::UncertaintyKind::AmbiguousSpec,
        "U" | "untestable-claim" => crate::protocol::UncertaintyKind::UntestableClaim,
        _ => return None,
    };
    let (about, asks) = rest.split_once(": ")?;
    Some(crate::protocol::Uncertainty {
        about: about.to_string(),
        kind,
        asks: asks.to_string(),
    })
}

fn kind_short(k: crate::protocol::ClaimedChangeKind) -> &'static str {
    match k {
        crate::protocol::ClaimedChangeKind::Edit => "E",
        crate::protocol::ClaimedChangeKind::Create => "C",
        crate::protocol::ClaimedChangeKind::Delete => "D",
    }
}

fn source_short(s: crate::protocol::GroundingSource) -> &'static str {
    match s {
        crate::protocol::GroundingSource::ToolRead => "R",
        crate::protocol::GroundingSource::ToolGrep => "G",
        crate::protocol::GroundingSource::ContextFile => "F",
        crate::protocol::GroundingSource::Guess => "?",
    }
}

fn uncertainty_short(k: crate::protocol::UncertaintyKind) -> &'static str {
    match k {
        crate::protocol::UncertaintyKind::MissingContext => "M",
        crate::protocol::UncertaintyKind::AmbiguousSpec => "A",
        crate::protocol::UncertaintyKind::UntestableClaim => "U",
    }
}

// ---------- measurement ----------

/// One round-trip overhead measurement for an [`Envelope`] under one
/// [`Strategy`]. Produced by [`measure_overhead`] and consumed by the §2
/// nightly protocol-overhead harness (`atelier protocol-overhead`).
///
/// `bytes_on_wire` is the length of the *encoded* envelope payload — for
/// `NativeTool` we encode the synthetic `harness_meta` tool call as JSON
/// (the shape the adapter actually transmits in a `tool_use` block); for
/// the prose strategies it's the UTF-8 byte length of the sentinel-wrapped
/// or tagged string. `tokens_envelope` is the chars/4 heuristic — the same
/// approximation [`crate::adapter::TokenSource::Approx`] uses, so the
/// number is directly comparable across providers without dragging in a
/// model-specific tokenizer. `parse_time_ns` is the wall-clock time of one
/// decode pass back into a typed `Envelope`.
///
/// The three figures together let the nightly job compute median
/// percentage overhead against a no-protocol baseline and surface
/// regressions per the §2 spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverheadMeasurement {
    /// Wire-format byte length the adapter would transmit.
    pub bytes_on_wire: u64,
    /// Approximate token count (chars / 4) for the encoded payload.
    pub tokens_envelope: u64,
    /// Wall-clock nanoseconds to decode the payload back into an
    /// [`Envelope`]. Single sample — the caller drives N iterations and
    /// computes whatever statistic the §2 contract calls for.
    pub parse_time_ns: u64,
}

/// Chars-per-token used by [`measure_overhead`]'s `tokens_envelope`
/// estimate. Spec §1 "approximate counting" — the same 4:1 ratio the
/// `Approx` token-count source assumes throughout the harness.
pub const APPROX_CHARS_PER_TOKEN: u64 = 4;

/// Encode `env` under `strategy`, then time one decode pass.
///
/// Returns the encoded byte length, the chars/4 token approximation, and
/// the wall-clock parse time in nanoseconds.
///
/// **Lossy round-trip on `RegexProse`** is intentional and matches the
/// strategy's documented contract (`plan_update`,
/// `constraints_acknowledged` are dropped). For overhead measurement that
/// is fine: we are measuring how many bytes/tokens the *carrier* costs,
/// not whether the carrier preserves every envelope field.
pub fn measure_overhead(
    env: &Envelope,
    strategy: Strategy,
) -> Result<OverheadMeasurement, StrategyError> {
    let payload = match strategy {
        Strategy::NativeTool => {
            let call = encode_native_tool(env)?;
            // The wire representation of a native tool call is the JSON
            // serialisation of `{name, arguments}` — that is what the
            // adapter packages into a `tool_use` block. Using
            // `to_string` (not `to_string_pretty`) so we measure the
            // compact form the adapter actually sends.
            serde_json::to_string(&call).map_err(|e| StrategyError::Encode(e.to_string()))?
        }
        Strategy::JsonSentinel => encode_json_sentinel(env)?,
        Strategy::RegexProse => encode_regex_prose(env)?,
    };

    let bytes_on_wire = payload.len() as u64;
    let tokens_envelope = (payload.chars().count() as u64).div_ceil(APPROX_CHARS_PER_TOKEN);

    let start = std::time::Instant::now();
    match strategy {
        Strategy::NativeTool => {
            // Re-derive the typed `NativeToolCall` from the serialised
            // payload so the parse time mirrors the real wire path
            // (deserialise the tool call, then decode the envelope).
            let call: NativeToolCall =
                serde_json::from_str(&payload).map_err(|e| StrategyError::Encode(e.to_string()))?;
            let _ = parse_native_tool(&call)?;
        }
        Strategy::JsonSentinel => {
            let _ = parse_json_sentinel(&payload)?;
        }
        Strategy::RegexProse => {
            let _ = parse_regex_prose(&payload)?;
        }
    }
    let parse_time_ns = start.elapsed().as_nanos() as u64;

    Ok(OverheadMeasurement {
        bytes_on_wire,
        tokens_envelope,
        parse_time_ns,
    })
}

// ---------- errors ----------

#[derive(Debug, thiserror::Error)]
pub enum StrategyError {
    #[error("envelope error: {0}")]
    Envelope(#[from] EnvelopeError),

    #[error("encode failure: {0}")]
    Encode(String),

    #[error("expected tool name {HARNESS_META_NAME:?} but got {0:?}")]
    WrongToolName(String),

    #[error("sentinel tags not found in model reply")]
    SentinelMissing,

    /// Spec §2: the JSON sentinel envelope is appended at end-of-turn —
    /// any non-whitespace content after the close tag is either a second
    /// envelope the model emitted (which would be silently lost) or
    /// post-envelope chatter (which violates the contract). Either way
    /// the reply is malformed; fail loudly rather than rubber-stamp.
    ///
    /// `length` is the total trailing-content byte length; `prefix`
    /// carries up to the module-private `TRAILING_PREFIX_BYTES` of the
    /// trailing content so a developer triaging the failure can tell
    /// "looks like a second
    /// sentinel" from "looks like leftover prose" without re-deriving
    /// from logs.
    #[error("non-whitespace content after sentinel close tag (length {length} bytes; prefix: {prefix:?})")]
    TrailingContentAfterSentinel { length: usize, prefix: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        ClaimedChange, ClaimedChangeKind, Grounding, GroundingSource, PlanOp, PlanOpKind,
        PlanUpdate, Uncertainty, UncertaintyKind,
    };

    fn example_envelope() -> Envelope {
        Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "utils.py".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "Renamed foo to bar".into(),
            }]),
            claimed_done: Some(true),
            grounding: Some(vec![Grounding {
                text_span: "one definition of foo".into(),
                source: GroundingSource::ToolRead,
            }]),
            ..Default::default()
        }
    }

    // ---------- strategy registry ----------

    #[test]
    fn harness_meta_tool_spec_round_trips_a_real_envelope_through_its_schema() {
        let spec = harness_meta_tool_spec();
        assert_eq!(spec.name, HARNESS_META_NAME);
        assert!(!spec.description.is_empty(), "schema needs a description");
        // The schema must validate a real envelope (including the
        // `kind` enum lowercase serialisation) so the model's tool
        // call round-trips into `Envelope::from_value` without rejection.
        let envelope_json = serde_json::to_value(example_envelope()).unwrap();
        let validator =
            jsonschema::validator_for(&spec.input_schema).expect("input_schema compiles");
        let errs: Vec<_> = validator.iter_errors(&envelope_json).collect();
        assert!(errs.is_empty(), "envelope must validate: {errs:?}");
        // Unknown top-level fields are rejected (additionalProperties: false).
        let bad = serde_json::json!({"claimed_done": true, "surprise": 1});
        assert!(validator.iter_errors(&bad).next().is_some());
    }

    #[test]
    fn downshift_walks_native_to_sentinel_to_prose_then_stops() {
        assert_eq!(
            Strategy::NativeTool.downshift(),
            Some(Strategy::JsonSentinel)
        );
        assert_eq!(
            Strategy::JsonSentinel.downshift(),
            Some(Strategy::RegexProse)
        );
        assert_eq!(Strategy::RegexProse.downshift(), None);
    }

    #[test]
    fn strategy_labels_are_stable_for_logs_and_ledger() {
        assert_eq!(Strategy::NativeTool.as_str(), "native_tool");
        assert_eq!(Strategy::JsonSentinel.as_str(), "json_sentinel");
        assert_eq!(Strategy::RegexProse.as_str(), "regex_prose");
    }

    #[test]
    fn tolerance_rank_orders_native_below_sentinel_below_prose() {
        // The degradation walk is encoded in this ordering. A bigger
        // rank means "more tolerant of model misbehaviour" — the value
        // the runner walks toward when it observes malformed envelopes.
        assert!(
            Strategy::NativeTool.tolerance_rank() < Strategy::JsonSentinel.tolerance_rank(),
            "NativeTool should be strictly less tolerant than JsonSentinel"
        );
        assert!(
            Strategy::JsonSentinel.tolerance_rank() < Strategy::RegexProse.tolerance_rank(),
            "JsonSentinel should be strictly less tolerant than RegexProse"
        );
    }

    #[test]
    fn less_tolerant_than_walks_the_degradation_chain() {
        // The full triangular check: every strategy is less tolerant
        // than its `downshift` target and not less tolerant than itself.
        assert!(Strategy::NativeTool.less_tolerant_than(Strategy::JsonSentinel));
        assert!(Strategy::NativeTool.less_tolerant_than(Strategy::RegexProse));
        assert!(Strategy::JsonSentinel.less_tolerant_than(Strategy::RegexProse));

        assert!(!Strategy::JsonSentinel.less_tolerant_than(Strategy::NativeTool));
        assert!(!Strategy::RegexProse.less_tolerant_than(Strategy::NativeTool));
        assert!(!Strategy::RegexProse.less_tolerant_than(Strategy::JsonSentinel));

        for s in [
            Strategy::NativeTool,
            Strategy::JsonSentinel,
            Strategy::RegexProse,
        ] {
            assert!(!s.less_tolerant_than(s), "{s:?} less_tolerant_than itself");
        }
    }

    // ---------- native-tool ----------

    #[test]
    fn native_tool_round_trips_envelope_exactly() {
        let env = example_envelope();
        let call = encode_native_tool(&env).unwrap();
        assert_eq!(call.name, HARNESS_META_NAME);
        let back = parse_native_tool(&call).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn native_tool_round_trips_plan_update_losslessly() {
        let env = Envelope {
            plan_update: Some(PlanUpdate {
                ops: vec![PlanOp {
                    op: PlanOpKind::Add,
                    step: "write parser".into(),
                }],
            }),
            ..Default::default()
        };
        let call = encode_native_tool(&env).unwrap();
        let back = parse_native_tool(&call).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn native_tool_rejects_call_with_wrong_name() {
        let call = NativeToolCall {
            name: "some_other_tool".into(),
            arguments: serde_json::json!({}),
        };
        let err = parse_native_tool(&call).unwrap_err();
        assert!(matches!(err, StrategyError::WrongToolName(_)));
    }

    #[test]
    fn native_tool_propagates_envelope_validation_errors() {
        let call = NativeToolCall {
            name: HARNESS_META_NAME.into(),
            arguments: serde_json::json!({
                "claimed_changes": [{
                    "path": "a", "kind": "edit", "summary": "x".repeat(501)
                }]
            }),
        };
        let err = parse_native_tool(&call).unwrap_err();
        assert!(matches!(err, StrategyError::Envelope(_)));
    }

    // ---------- json-sentinel ----------

    #[test]
    fn json_sentinel_round_trips_envelope_exactly() {
        let env = example_envelope();
        let payload = encode_json_sentinel(&env).unwrap();
        assert!(payload.starts_with(SENTINEL_OPEN));
        assert!(payload.ends_with(SENTINEL_CLOSE));
        let parsed = parse_json_sentinel(&payload).unwrap();
        assert_eq!(parsed.envelope, env);
        assert_eq!(parsed.prose, "");
    }

    #[test]
    fn json_sentinel_separates_prose_from_envelope() {
        let env = example_envelope();
        let appendage = encode_json_sentinel(&env).unwrap();
        let reply = format!("Here is the rename you asked for.\n\n{appendage}");
        let parsed = parse_json_sentinel(&reply).unwrap();
        assert_eq!(parsed.envelope, env);
        assert_eq!(parsed.prose, "Here is the rename you asked for.");
    }

    #[test]
    fn json_sentinel_missing_tags_is_a_distinct_error() {
        let err = parse_json_sentinel("just prose, no envelope").unwrap_err();
        assert!(matches!(err, StrategyError::SentinelMissing));
    }

    #[test]
    fn json_sentinel_partial_open_tag_is_missing() {
        let err = parse_json_sentinel(&format!("text {SENTINEL_OPEN}{{}}")).unwrap_err();
        // Open tag present but close missing → SentinelMissing.
        assert!(matches!(err, StrategyError::SentinelMissing));
    }

    #[test]
    fn json_sentinel_malformed_inner_json_surfaces_as_envelope_error() {
        let bad = format!("{SENTINEL_OPEN}not json{SENTINEL_CLOSE}");
        let err = parse_json_sentinel(&bad).unwrap_err();
        assert!(matches!(err, StrategyError::Envelope(_)));
    }

    // P4 regression: a second envelope (or any non-whitespace text) after
    // the close tag must be rejected. Pre-P4 the parser silently discarded
    // anything after the close, which let a model emit two envelopes and
    // have only the first survive — a fail-open path.
    #[test]
    fn json_sentinel_rejects_trailing_content_after_close_tag() {
        let env = example_envelope();
        let appendage = encode_json_sentinel(&env).unwrap();
        let reply = format!("{appendage}\nthen some chatter the model added");
        let err = parse_json_sentinel(&reply).unwrap_err();
        match err {
            StrategyError::TrailingContentAfterSentinel { length, prefix } => {
                assert!(length > 0);
                assert!(
                    prefix.contains("then some chatter"),
                    "prefix should carry triage info, got {prefix:?}"
                );
            }
            other => panic!("expected TrailingContentAfterSentinel, got {other:?}"),
        }
    }

    // v25.2-A regression: an envelope whose JSON string content contains
    // the literal "<<<end>>>" close tag must parse correctly. Pre-fix the
    // parser used `find(SENTINEL_CLOSE)` and truncated mid-string,
    // surfacing as Envelope::Parse instead of clean success.
    #[test]
    fn json_sentinel_handles_close_tag_embedded_in_json_string() {
        let env = Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "notes.md".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "see the <<<end>>> tag mentioned in the docs".into(),
            }]),
            claimed_done: Some(false),
            ..Default::default()
        };
        let appendage = encode_json_sentinel(&env).unwrap();
        let parsed = parse_json_sentinel(&appendage).unwrap();
        assert_eq!(parsed.envelope, env);
    }

    // Companion case: open-tag embedded in the JSON. Same parser principle.
    #[test]
    fn json_sentinel_handles_open_tag_embedded_in_json_string() {
        let env = Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "notes.md".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "describes the <<<harness_meta>>> protocol".into(),
            }]),
            claimed_done: Some(false),
            ..Default::default()
        };
        let appendage = encode_json_sentinel(&env).unwrap();
        let parsed = parse_json_sentinel(&appendage).unwrap();
        assert_eq!(parsed.envelope, env);
    }

    #[test]
    fn json_sentinel_accepts_trailing_whitespace_after_close_tag() {
        // Real wire payloads often have a trailing newline. Trailing
        // whitespace (newlines, tabs, spaces) is fine — only
        // non-whitespace is a contract violation.
        let env = example_envelope();
        let appendage = encode_json_sentinel(&env).unwrap();
        let reply = format!("{appendage}\n\n  \n");
        let parsed = parse_json_sentinel(&reply).unwrap();
        assert_eq!(parsed.envelope, env);
    }

    #[test]
    fn json_sentinel_rejects_double_envelope() {
        // The specific footgun the audit named: a model emits two
        // envelopes; pre-P4 the parser silently kept only the first.
        // Now we error so the conformance tracker sees it.
        let env = example_envelope();
        let one = encode_json_sentinel(&env).unwrap();
        let reply = format!("{one}{one}");
        let err = parse_json_sentinel(&reply).unwrap_err();
        match err {
            StrategyError::TrailingContentAfterSentinel { prefix, .. } => {
                // The second sentinel should be visible in the prefix —
                // proves the error is genuinely from the right cause.
                assert!(
                    prefix.contains(SENTINEL_OPEN),
                    "prefix should contain the second sentinel, got {prefix:?}"
                );
            }
            other => panic!("expected TrailingContentAfterSentinel, got {other:?}"),
        }
    }

    // ---------- regex-prose ----------

    #[test]
    fn regex_prose_round_trips_a_simple_envelope() {
        let env = example_envelope();
        let prose = encode_regex_prose(&env).unwrap();
        let parsed = parse_regex_prose(&prose).unwrap();
        assert_eq!(parsed, env);
    }

    #[test]
    fn regex_prose_drops_plan_update_and_constraints_per_spec() {
        // Spec §2: "Lossy; UI badges degrade to gray." plan_update +
        // constraints_acknowledged have no carrier in the prose strategy.
        let env = Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "a".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "edit".into(),
            }]),
            plan_update: Some(PlanUpdate {
                ops: vec![PlanOp {
                    op: PlanOpKind::Add,
                    step: "x".into(),
                }],
            }),
            constraints_acknowledged: Some(vec!["no new deps".into()]),
            ..Default::default()
        };
        let prose = encode_regex_prose(&env).unwrap();
        let parsed = parse_regex_prose(&prose).unwrap();
        // claimed_changes survives.
        assert_eq!(parsed.claimed_changes, env.claimed_changes);
        // plan_update and constraints dropped — surface as None so the UI
        // renders gray rather than substituting "everything OK."
        assert!(parsed.plan_update.is_none());
        assert!(parsed.constraints_acknowledged.is_none());
    }

    #[test]
    fn regex_prose_handles_all_uncertainty_kinds() {
        let env = Envelope {
            uncertainty: Some(vec![
                Uncertainty {
                    about: "a".into(),
                    kind: UncertaintyKind::MissingContext,
                    asks: "x".into(),
                },
                Uncertainty {
                    about: "b".into(),
                    kind: UncertaintyKind::AmbiguousSpec,
                    asks: "y".into(),
                },
                Uncertainty {
                    about: "c".into(),
                    kind: UncertaintyKind::UntestableClaim,
                    asks: "z".into(),
                },
            ]),
            ..Default::default()
        };
        let prose = encode_regex_prose(&env).unwrap();
        let parsed = parse_regex_prose(&prose).unwrap();
        assert_eq!(parsed.uncertainty, env.uncertainty);
    }

    #[test]
    fn regex_prose_handles_all_grounding_sources() {
        let env = Envelope {
            grounding: Some(vec![
                Grounding {
                    text_span: "a".into(),
                    source: GroundingSource::ToolRead,
                },
                Grounding {
                    text_span: "b".into(),
                    source: GroundingSource::ToolGrep,
                },
                Grounding {
                    text_span: "c".into(),
                    source: GroundingSource::ContextFile,
                },
                Grounding {
                    text_span: "d".into(),
                    source: GroundingSource::Guess,
                },
            ]),
            ..Default::default()
        };
        let prose = encode_regex_prose(&env).unwrap();
        let parsed = parse_regex_prose(&prose).unwrap();
        assert_eq!(parsed.grounding, env.grounding);
    }

    #[test]
    fn regex_prose_done_supports_yes_no_true_false() {
        for (literal, want) in [
            ("yes", true),
            ("no", false),
            ("true", true),
            ("false", false),
        ] {
            let text = format!("{PROSE_TAG_DONE} {literal}\n");
            let parsed = parse_regex_prose(&text).unwrap();
            assert_eq!(parsed.claimed_done, Some(want));
        }
    }

    #[test]
    fn regex_prose_unrecognised_done_value_is_absent_not_default() {
        // Spec §2 "Degradation policy": never substitute everything-OK.
        let text = format!("{PROSE_TAG_DONE} maybe\n");
        let parsed = parse_regex_prose(&text).unwrap();
        assert_eq!(parsed.claimed_done, None);
    }

    #[test]
    fn regex_prose_ignores_unrecognised_tags() {
        let text = "RANDOM-PROSE: nothing here\n\nMore prose.";
        let parsed = parse_regex_prose(text).unwrap();
        assert_eq!(parsed, Envelope::default());
    }

    #[test]
    fn regex_prose_only_matches_tags_at_line_start() {
        // A tag-shaped string embedded mid-prose must not be treated as a tag.
        let env = Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "a".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "see CHANGED-FILES: discussion in docs".into(),
            }]),
            ..Default::default()
        };
        let prose = encode_regex_prose(&env).unwrap();
        let parsed = parse_regex_prose(&prose).unwrap();
        // The summary's embedded tag-shape must not corrupt parsing.
        assert_eq!(
            parsed.claimed_changes.as_ref().unwrap()[0].summary,
            env.claimed_changes.as_ref().unwrap()[0].summary
        );
    }

    #[test]
    fn regex_prose_propagates_envelope_validation_failures() {
        // Forge a too-long summary in the prose. The parser builds an
        // Envelope and re-validates — schema cap is upheld here too.
        let long = "x".repeat(600);
        let text = format!("{PROSE_TAG_CHANGES}\n  E a.py: {long}\n");
        let err = parse_regex_prose(&text).unwrap_err();
        assert!(matches!(err, StrategyError::Envelope(_)));
    }

    // ---------- measure_overhead ----------

    #[test]
    fn measure_overhead_reports_nonzero_bytes_and_tokens_for_all_strategies() {
        // The §2 nightly harness expects every strategy to yield a
        // positive `bytes_on_wire` and `tokens_envelope` against a
        // non-trivial envelope. A zero here would mean encode silently
        // returned an empty payload — exactly the failure mode the
        // measurement was added to catch.
        let env = example_envelope();
        for strategy in [
            Strategy::NativeTool,
            Strategy::JsonSentinel,
            Strategy::RegexProse,
        ] {
            let m = measure_overhead(&env, strategy)
                .unwrap_or_else(|e| panic!("measure {strategy:?}: {e}"));
            assert!(m.bytes_on_wire > 0, "{strategy:?} bytes_on_wire was 0");
            assert!(m.tokens_envelope > 0, "{strategy:?} tokens_envelope was 0");
            // parse_time_ns is best-effort; a single round-trip can be
            // <1 ns on some hosts (sub-tick resolution). Don't assert a
            // floor — just that the field is populated (u64 is always so).
        }
    }

    #[test]
    fn measure_overhead_token_approximation_matches_chars_per_token_ratio() {
        // The token figure is `ceil(chars / 4)`. Reach into the raw
        // payload to confirm we're not double-counting bytes vs. chars.
        let env = example_envelope();
        let m = measure_overhead(&env, Strategy::JsonSentinel).unwrap();
        let payload = encode_json_sentinel(&env).unwrap();
        let expected = (payload.chars().count() as u64).div_ceil(APPROX_CHARS_PER_TOKEN);
        assert_eq!(m.tokens_envelope, expected);
        assert_eq!(m.bytes_on_wire, payload.len() as u64);
    }

    #[test]
    fn measure_overhead_propagates_validation_errors() {
        // An invalid envelope (summary length over the schema cap) must
        // surface as `Envelope`, not silently produce a number.
        let env = Envelope {
            claimed_changes: Some(vec![ClaimedChange {
                path: "a".into(),
                kind: ClaimedChangeKind::Edit,
                summary: "x".repeat(600),
            }]),
            ..Default::default()
        };
        let err = measure_overhead(&env, Strategy::NativeTool).unwrap_err();
        assert!(matches!(err, StrategyError::Envelope(_)));
    }
}
