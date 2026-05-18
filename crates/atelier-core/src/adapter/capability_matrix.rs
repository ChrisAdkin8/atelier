//! §1 BYOM capability matrix — static lookup + probe cross-walk.
//!
//! Spec §1 "Capability matrix":
//!   Per-capability `Supported / ClaimedButBroken / Unsupported`. Static
//!   table for well-known models so the runtime knows what to expect
//!   *before* the first call; the §1 probe cross-walks observed
//!   behaviour against that claim and flips a row to
//!   `ClaimedButBroken` when reality diverges from the provider's
//!   advertised spec.
//!
//! This module is the static side. The dynamic side lives in
//! `adapter::model_profile` (probe-on-first-use); the two meet in
//! [`crosswalk_with_profile`] which produces a [`CapabilityMatrixRow`]
//! the GUI/TUI render in the footer tooltip.
//!
//! The lookup is keyed by the harness-side `<provider>:<model>` form
//! (e.g. `anthropic:claude-opus-4-7`, `openai-compat:gpt-4o`). Unknown
//! ids fall back to a conservative default — every column
//! `Unsupported` except `streaming` (every adapter we ship streams).

use serde::{Deserialize, Serialize};

use super::{Capabilities, CapabilityClaim};
use crate::adapter::model_profile::ModelProfile;

/// One row of the §1 capability matrix as surfaced to the UI. Mirrors
/// the shape of [`Capabilities`] but uses per-field
/// [`CapabilityClaim`]s that have already been cross-walked against
/// any available [`ModelProfile`] probe observations. The footer
/// tooltip in both drivers renders this verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityMatrixRow {
    /// `<provider>:<model>` — the lookup key.
    pub model_id: String,
    /// Stable human label for the footer tooltip ("Anthropic Opus
    /// 4.7", "Local Qwen2.5-Coder 7B", …). Empty for unknown models.
    pub display_label: String,
    pub native_tool_use: CapabilityClaim,
    pub streaming: CapabilityClaim,
    pub vision: CapabilityClaim,
    pub prompt_cache: CapabilityClaim,
    pub structured_output: CapabilityClaim,
    pub long_context: CapabilityClaim,
    pub context_window_tokens: u32,
    /// Where this row came from — `static` (table hit), `adapter` (no
    /// table hit, used the adapter's `capabilities()`), or `probe`
    /// (table hit *plus* a probe cross-walk that flipped at least one
    /// column). Useful triage for "why does the matrix say X?".
    pub source: CapabilityRowSource,
}

/// Provenance label for a [`CapabilityMatrixRow`]. The UI surfaces
/// this as a sub-label on the tooltip so a `claimed_but_broken` cell
/// can be traced to either a static config or a live probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRowSource {
    /// The row came straight from the static lookup table; no probe
    /// observations were merged in.
    Static,
    /// No static entry — the row was derived from the adapter's
    /// runtime `capabilities()` declaration.
    Adapter,
    /// A static row was crossed with a probe observation that
    /// produced at least one `ClaimedButBroken` cell.
    Probe,
}

impl CapabilityRowSource {
    /// Canonical lowercase wire label. Used by the GUI/TUI projection
    /// layers so the string never goes through `format!("{:?}")` (which
    /// would couple the wire format to Rust's `Debug` derive).
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Adapter => "adapter",
            Self::Probe => "probe",
        }
    }
}

/// Static entry in the matrix table. Kept private to this module —
/// callers either look up a full [`CapabilityMatrixRow`] (via
/// [`lookup_static`] / [`crosswalk_with_profile`]) or fall back to the
/// adapter's runtime declaration.
struct Entry {
    model_id: &'static str,
    display_label: &'static str,
    native_tool_use: CapabilityClaim,
    streaming: CapabilityClaim,
    vision: CapabilityClaim,
    prompt_cache: CapabilityClaim,
    structured_output: CapabilityClaim,
    long_context: CapabilityClaim,
    context_window_tokens: u32,
}

// The static table itself. Pre-populated with the providers atelier
// already supports (v51 BYOM landed three: Mock, Anthropic,
// OpenAI-compatible). New rows go here when a new model is
// well-characterised enough that the runtime probe is redundant.
//
// PROVISIONAL — the context-window and capability values are sourced
// from each provider's published spec at the time of writing. They're
// authoritative until a probe observation flips a column to
// `ClaimedButBroken`; the table is never the only source of truth at
// runtime.
const STATIC_TABLE: &[Entry] = &[
    // ---- Anthropic Claude family ----
    Entry {
        model_id: "anthropic:claude-opus-4-7",
        display_label: "Anthropic Claude Opus 4.7",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Supported,
        prompt_cache: CapabilityClaim::Supported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 200_000,
    },
    Entry {
        model_id: "anthropic:claude-sonnet-4-7",
        display_label: "Anthropic Claude Sonnet 4.7",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Supported,
        prompt_cache: CapabilityClaim::Supported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 200_000,
    },
    Entry {
        model_id: "anthropic:claude-haiku-4-7",
        display_label: "Anthropic Claude Haiku 4.7",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Supported,
        prompt_cache: CapabilityClaim::Supported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 200_000,
    },
    // ---- OpenAI (cloud, via openai-compat with no --base-url) ----
    Entry {
        model_id: "openai-compat:gpt-4o",
        display_label: "OpenAI GPT-4o",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Supported,
        prompt_cache: CapabilityClaim::Supported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 128_000,
    },
    Entry {
        model_id: "openai-compat:gpt-4o-mini",
        display_label: "OpenAI GPT-4o-mini",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Supported,
        prompt_cache: CapabilityClaim::Supported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 128_000,
    },
    // ---- Local self-hosted (via openai-compat with --base-url) ----
    //
    // Local models vary wildly; the table here covers the ones the
    // canonical `--model local:<tag>` workflow exercises. Vision /
    // prompt-cache are `Unsupported` for most local servers (Ollama,
    // llama-server, LM Studio) by default; the probe is the
    // authoritative source for the actual model's behaviour.
    Entry {
        model_id: "openai-compat:local:qwen2.5-coder:7b",
        display_label: "Local Qwen2.5-Coder 7B",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Unsupported,
        prompt_cache: CapabilityClaim::Unsupported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 32_768,
    },
    Entry {
        model_id: "openai-compat:local:qwen2.5-coder:32b",
        display_label: "Local Qwen2.5-Coder 32B",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Unsupported,
        prompt_cache: CapabilityClaim::Unsupported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 32_768,
    },
    Entry {
        model_id: "openai-compat:local:llama-3.3-70b",
        display_label: "Local Llama 3.3 70B",
        native_tool_use: CapabilityClaim::Supported,
        streaming: CapabilityClaim::Supported,
        vision: CapabilityClaim::Unsupported,
        prompt_cache: CapabilityClaim::Unsupported,
        structured_output: CapabilityClaim::Supported,
        long_context: CapabilityClaim::Supported,
        context_window_tokens: 128_000,
    },
];

/// Look up a static matrix row by `<provider>:<model>` id. Returns
/// `None` when the model isn't in the table — callers fall back to
/// [`from_adapter_capabilities`].
pub fn lookup_static(model_id: &str) -> Option<CapabilityMatrixRow> {
    STATIC_TABLE
        .iter()
        .find(|e| e.model_id == model_id)
        .map(|e| CapabilityMatrixRow {
            model_id: e.model_id.to_string(),
            display_label: e.display_label.to_string(),
            native_tool_use: e.native_tool_use,
            streaming: e.streaming,
            vision: e.vision,
            prompt_cache: e.prompt_cache,
            structured_output: e.structured_output,
            long_context: e.long_context,
            context_window_tokens: e.context_window_tokens,
            source: CapabilityRowSource::Static,
        })
}

/// Derive a matrix row from the adapter's runtime [`Capabilities`]
/// declaration. Used when the static table has no entry for the
/// model id — every capability column carries straight through and
/// `source` is [`CapabilityRowSource::Adapter`]. The display label is
/// empty so the UI can fall back to rendering the raw `model_id`.
pub fn from_adapter_capabilities(model_id: &str, caps: &Capabilities) -> CapabilityMatrixRow {
    CapabilityMatrixRow {
        model_id: model_id.to_string(),
        display_label: String::new(),
        native_tool_use: caps.native_tool_use,
        streaming: caps.streaming,
        vision: caps.vision,
        prompt_cache: caps.prompt_cache,
        structured_output: caps.structured_output,
        long_context: caps.long_context,
        context_window_tokens: caps.context_window_tokens,
        source: CapabilityRowSource::Adapter,
    }
}

/// Public entry point: build a matrix row for the model id, preferring
/// the static table and falling back to the adapter's runtime
/// declaration. The adapter's `capabilities()` informs the
/// `context_window_tokens` even on a static hit if the adapter has a
/// tighter claim than the table (a local model server may advertise a
/// shorter window than the underlying weights support).
pub fn matrix_row_for(model_id: &str, caps: &Capabilities) -> CapabilityMatrixRow {
    match lookup_static(model_id) {
        Some(mut row) => {
            // Adapter's window claim wins when smaller (it's the
            // *runtime* limit, not the model's theoretical max).
            if caps.context_window_tokens > 0
                && caps.context_window_tokens < row.context_window_tokens
            {
                row.context_window_tokens = caps.context_window_tokens;
            }
            row
        }
        None => from_adapter_capabilities(model_id, caps),
    }
}

/// Cross-walk a matrix row with a [`ModelProfile`] from the §1
/// probe-on-first-use cache. Flips columns to
/// [`CapabilityClaim::ClaimedButBroken`] when the probe observed the
/// model failing a capability the static table (or adapter) claims it
/// supports. Returns a fresh row with `source =
/// CapabilityRowSource::Probe` when any column changed; otherwise the
/// input row is returned unchanged.
///
/// The cross-walk is currently limited to two columns the probe
/// directly observes:
///   * `native_tool_use` — flipped if `profile.supports_native_tools`
///     is false but the matrix claims `Supported`.
///   * `structured_output` — flipped if `profile.strategy` is
///     `RegexProse` (the probe couldn't get a JSON sentinel through)
///     but the matrix claims `Supported`.
///
/// Streaming is not crossed even though the profile carries
/// `supports_streaming`, because the harness drives streaming through
/// the same code path as non-streaming and a probe failure there is
/// usually a transient network issue rather than a permanent model
/// limitation.
pub fn crosswalk_with_profile(
    mut row: CapabilityMatrixRow,
    profile: &ModelProfile,
) -> CapabilityMatrixRow {
    let mut changed = false;

    if matches!(row.native_tool_use, CapabilityClaim::Supported) && !profile.supports_native_tools {
        row.native_tool_use = CapabilityClaim::ClaimedButBroken;
        changed = true;
    }

    if matches!(row.structured_output, CapabilityClaim::Supported)
        && matches!(
            profile.strategy,
            crate::protocol_strategy::Strategy::RegexProse
        )
    {
        row.structured_output = CapabilityClaim::ClaimedButBroken;
        changed = true;
    }

    if changed {
        row.source = CapabilityRowSource::Probe;
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::model_profile::{ModelProfile, PROFILE_SCHEMA_VERSION};
    use crate::protocol_strategy::Strategy;

    fn caps_all_supported() -> Capabilities {
        Capabilities {
            native_tool_use: CapabilityClaim::Supported,
            streaming: CapabilityClaim::Supported,
            vision: CapabilityClaim::Supported,
            prompt_cache: CapabilityClaim::Supported,
            structured_output: CapabilityClaim::Supported,
            long_context: CapabilityClaim::Supported,
            context_window_tokens: 200_000,
        }
    }

    fn profile_supporting_tools(strategy: Strategy, supports_tools: bool) -> ModelProfile {
        ModelProfile {
            schema_version: PROFILE_SCHEMA_VERSION,
            model_id: "anthropic:claude-opus-4-7".into(),
            base_url: String::new(),
            probed_at: "2026-05-17T00:00:00Z".into(),
            strategy,
            supports_native_tools: supports_tools,
            supports_streaming: true,
            utf8_clean: true,
            context_window_tokens: 200_000,
            max_tokens: 4096,
            notes: vec![],
        }
    }

    #[test]
    fn static_table_includes_anthropic_opus() {
        let row = lookup_static("anthropic:claude-opus-4-7").expect("opus row");
        assert_eq!(row.display_label, "Anthropic Claude Opus 4.7");
        assert!(matches!(row.native_tool_use, CapabilityClaim::Supported));
        assert_eq!(row.context_window_tokens, 200_000);
        assert_eq!(row.source, CapabilityRowSource::Static);
    }

    #[test]
    fn static_table_includes_openai_gpt4o() {
        let row = lookup_static("openai-compat:gpt-4o").expect("gpt-4o row");
        assert!(matches!(row.vision, CapabilityClaim::Supported));
        assert_eq!(row.context_window_tokens, 128_000);
    }

    #[test]
    fn static_table_includes_local_qwen() {
        let row = lookup_static("openai-compat:local:qwen2.5-coder:7b").expect("qwen row");
        // Local models default to no vision and no prompt cache.
        assert!(matches!(row.vision, CapabilityClaim::Unsupported));
        assert!(matches!(row.prompt_cache, CapabilityClaim::Unsupported));
    }

    #[test]
    fn lookup_static_returns_none_for_unknown_model() {
        assert!(lookup_static("unknown:model").is_none());
    }

    #[test]
    fn matrix_row_for_falls_back_to_adapter_when_unknown() {
        let row = matrix_row_for("unknown:model", &caps_all_supported());
        assert_eq!(row.source, CapabilityRowSource::Adapter);
        assert_eq!(row.model_id, "unknown:model");
        assert_eq!(row.display_label, "");
    }

    #[test]
    fn matrix_row_for_clamps_window_to_adapters_smaller_claim() {
        let mut caps = caps_all_supported();
        caps.context_window_tokens = 8_192; // tighter than table's 200k
        let row = matrix_row_for("anthropic:claude-opus-4-7", &caps);
        assert_eq!(row.context_window_tokens, 8_192);
        // Provenance stays `static` — the table row was used, only
        // the window was clamped to the adapter's runtime claim.
        assert_eq!(row.source, CapabilityRowSource::Static);
    }

    #[test]
    fn crosswalk_flips_native_tool_when_probe_says_unsupported() {
        let row = lookup_static("anthropic:claude-opus-4-7").unwrap();
        let crossed = crosswalk_with_profile(
            row,
            &profile_supporting_tools(Strategy::JsonSentinel, false),
        );
        assert!(matches!(
            crossed.native_tool_use,
            CapabilityClaim::ClaimedButBroken
        ));
        assert_eq!(crossed.source, CapabilityRowSource::Probe);
    }

    #[test]
    fn crosswalk_flips_structured_output_when_probe_falls_to_regex_prose() {
        let row = lookup_static("anthropic:claude-opus-4-7").unwrap();
        let crossed =
            crosswalk_with_profile(row, &profile_supporting_tools(Strategy::RegexProse, true));
        assert!(matches!(
            crossed.structured_output,
            CapabilityClaim::ClaimedButBroken
        ));
        assert_eq!(crossed.source, CapabilityRowSource::Probe);
    }

    #[test]
    fn crosswalk_preserves_source_when_no_columns_flip() {
        let row = lookup_static("anthropic:claude-opus-4-7").unwrap();
        let crossed =
            crosswalk_with_profile(row, &profile_supporting_tools(Strategy::NativeTool, true));
        // Nothing flipped — provenance must stay `static`.
        assert_eq!(crossed.source, CapabilityRowSource::Static);
        assert!(matches!(
            crossed.native_tool_use,
            CapabilityClaim::Supported
        ));
        assert!(matches!(
            crossed.structured_output,
            CapabilityClaim::Supported
        ));
    }

    #[test]
    fn crosswalk_does_not_flip_columns_already_unsupported() {
        // A model whose static row says vision=Unsupported should not
        // be touched by the probe cross-walk — `ClaimedButBroken` only
        // makes sense when the claim is `Supported`.
        let row = lookup_static("openai-compat:local:qwen2.5-coder:7b").unwrap();
        let crossed =
            crosswalk_with_profile(row, &profile_supporting_tools(Strategy::RegexProse, false));
        // structured_output was Supported, so it flips. vision was
        // already Unsupported, so it stays.
        assert!(matches!(crossed.vision, CapabilityClaim::Unsupported));
        assert!(matches!(
            crossed.structured_output,
            CapabilityClaim::ClaimedButBroken
        ));
    }

    #[test]
    fn capability_row_source_wire_labels_round_trip() {
        for s in [
            CapabilityRowSource::Static,
            CapabilityRowSource::Adapter,
            CapabilityRowSource::Probe,
        ] {
            let label = s.wire_label();
            let json = serde_json::to_string(&s).unwrap();
            // serde rename_all=snake_case must match wire_label().
            assert_eq!(json, format!("\"{label}\""));
            let back: CapabilityRowSource = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn matrix_row_round_trips_through_serde() {
        let row = lookup_static("anthropic:claude-opus-4-7").unwrap();
        let json = serde_json::to_string(&row).unwrap();
        let back: CapabilityMatrixRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back, row);
    }
}
