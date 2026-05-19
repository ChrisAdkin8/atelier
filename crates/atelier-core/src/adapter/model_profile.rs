//! v51 — Probe-on-first-use model profile cache (PROBE-1).
//!
//! When the harness first encounters a `(model_id, base_url)` pair the
//! Runner fires one or two short calibration calls to determine which
//! §2 emission strategy the model can actually handle (native
//! tool-use? JSON-sentinel envelopes? prose-only?). The result is
//! captured here and persisted to disk so subsequent runs skip the
//! probe.
//!
//! Cache layout: `~/.atelier/model_profiles/<hash>.json` where
//! `hash = sha256(model_id || "\n" || base_url)[..16]` (64 bits — ample
//! for the universe of `(model, server)` pairs a single user touches).
//!
//! The probe cache lives at user scope (not project scope) because the
//! observation is about the *model*, not the project. Pointing the same
//! Qwen-Coder at the same LM Studio from any repo yields the same
//! profile; a global cache maximises hit rate. The
//! `ATELIER_PROFILE_DIR` env var lets tests redirect the cache.
//!
//! This module is the *storage half* of PROBE-* — the probe driver
//! (PROBE-3) populates the struct, and [`ProfileStore::load_or_probe`]
//! (PROBE-4) is the entry point callers use.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol_strategy::{parse_json_sentinel, Strategy, SENTINEL_CLOSE, SENTINEL_OPEN};

use super::{Adapter, AdapterError, Message, Role, ToolSpec};

/// Schema version of the on-disk profile. Bumped when the probe
/// semantics change (e.g. a new field is added that affects strategy
/// selection). A load with a mismatched version returns
/// [`ProfileError::IncompatibleVersion`] so the caller can re-probe
/// rather than acting on stale observations.
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

/// Filename pattern for cache entries.
pub(crate) const PROFILE_FILE_EXT: &str = "json";

/// Directory name under `~/.atelier/` (or `$ATELIER_PROFILE_DIR`).
pub(crate) const PROFILE_DIR_NAME: &str = "model_profiles";

/// Env var that overrides the on-disk cache location. Set to an
/// absolute path; tests use a per-test `tempfile::TempDir`.
pub const PROFILE_DIR_ENV: &str = "ATELIER_PROFILE_DIR";

/// Cached observations from a one-time probe of a
/// `(model_id, base_url)` pair. Fields are intentionally minimal in
/// PROBE-1 — the probe driver populates them in PROBE-3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProfile {
    /// Bumped per [`PROFILE_SCHEMA_VERSION`] — drives re-probe on skew.
    pub schema_version: u32,
    /// `<provider>:<model>` form, as passed to the adapter at
    /// construction (e.g. `local:qwen2.5-coder:7b`).
    pub model_id: String,
    /// Full URL of the chat-completions endpoint, ending in `/v1`
    /// (e.g. `http://localhost:11434/v1`). Empty allowed when the
    /// adapter doesn't speak HTTP (mock/anthropic), in which case the
    /// profile is just a stub.
    pub base_url: String,
    /// RFC 3339 timestamp of when the probe completed. Informational
    /// only — cache invalidation is by schema version, not by age.
    pub probed_at: String,
    /// Best §2 strategy the model handled in the probe. Runner uses
    /// this as the *initial* strategy; the existing §1 conformance
    /// tracker still degrades if reality diverges from the cache.
    pub strategy: Strategy,
    /// `true` iff the probe successfully round-tripped a native tool
    /// call. When `false`, [`Self::strategy`] won't be `NativeTool`.
    pub supports_native_tools: bool,
    /// `true` iff `stream()` produced parseable SSE frames during the
    /// probe (when the probe exercised the streaming path).
    pub supports_streaming: bool,
    /// `false` if any probe response contained the U+FFFD replacement
    /// character or invalid UTF-8 byte sequences. Signals a model
    /// whose tokenizer is mismatched against the prompt encoding.
    pub utf8_clean: bool,
    /// Best-known context window the model claims, or the adapter
    /// default if the probe couldn't determine it.
    pub context_window_tokens: u32,
    /// Per-call `max_tokens` cap.
    pub max_tokens: u32,
    /// Free-text triage hints captured by the probe (e.g. "tool calls
    /// returned arguments as a non-JSON-string"). Surfaced via
    /// `tracing::info!` when the profile is loaded.
    pub notes: Vec<String>,
}

/// Raw evidence collected by the probe driver (PROBE-3). The pure
/// [`decide_strategy`] function maps these flags onto a §2 [`Strategy`]
/// preference. Separated from [`ModelProfile`] so the decision rule is
/// trivially unit-testable without spinning up an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeObservation {
    /// `true` iff the probe sent a native-tool calibration call and
    /// the response contained a parseable tool-call whose arguments
    /// round-tripped through JSON.
    pub native_tool_call_succeeded: bool,
    /// `true` iff the probe sent a JSON-sentinel calibration prompt
    /// and the response contained a parseable
    /// `<<<harness_meta>>>...<<<end>>>` block.
    pub json_sentinel_succeeded: bool,
    /// `false` if any probe response contained the U+FFFD replacement
    /// character or invalid UTF-8 byte sequences. Doesn't affect
    /// strategy choice — recorded for triage.
    pub utf8_clean: bool,
    /// `true` iff the probe exercised the streaming path and got
    /// well-formed SSE frames. Doesn't affect strategy choice —
    /// recorded so the Runner knows whether to prefer `stream()`
    /// over `chat()`.
    pub streaming_ok: bool,
}

impl ProbeObservation {
    /// All-false starting point — `decide_strategy` against this
    /// yields `RegexProse`, which is the safe last-resort.
    pub fn empty() -> Self {
        Self {
            native_tool_call_succeeded: false,
            json_sentinel_succeeded: false,
            utf8_clean: true,
            streaming_ok: false,
        }
    }
}

/// Map probe observations onto the best §2 strategy the model handled.
/// Preference order: `NativeTool > JsonSentinel > RegexProse`. The §1
/// conformance tracker still degrades at runtime if the live model
/// diverges from this static decision.
pub fn decide_strategy(obs: &ProbeObservation) -> Strategy {
    if obs.native_tool_call_succeeded {
        Strategy::NativeTool
    } else if obs.json_sentinel_succeeded {
        Strategy::JsonSentinel
    } else {
        Strategy::RegexProse
    }
}

impl ModelProfile {
    /// Construct a profile from raw probe observations. Strategy is
    /// derived via [`decide_strategy`]; the remaining fields carry the
    /// observations forward so the cache file is self-describing.
    /// `probed_at` is an RFC 3339 timestamp captured by the caller (so
    /// tests can pin it; the Runner uses its existing `now_rfc3339`).
    pub fn from_observations(
        model_id: impl Into<String>,
        base_url: impl Into<String>,
        probed_at: impl Into<String>,
        obs: ProbeObservation,
        context_window_tokens: u32,
        max_tokens: u32,
        mut notes: Vec<String>,
    ) -> Self {
        let strategy = decide_strategy(&obs);
        if !obs.utf8_clean {
            notes.push("utf8_clean=false: probe responses contained replacement chars".into());
        }
        if !obs.streaming_ok {
            notes.push("streaming_ok=false: SSE frames unparseable or absent".into());
        }
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            model_id: model_id.into(),
            base_url: base_url.into(),
            probed_at: probed_at.into(),
            strategy,
            supports_native_tools: obs.native_tool_call_succeeded,
            supports_streaming: obs.streaming_ok,
            utf8_clean: obs.utf8_clean,
            context_window_tokens,
            max_tokens,
            notes,
        }
    }

    /// Stub profile used by adapters that don't need a probe (Mock,
    /// Anthropic — their behaviour is well-characterised). Schema
    /// version is current so a load of a written stub doesn't trip
    /// the version check.
    pub fn skipped_for_well_known(
        model_id: impl Into<String>,
        strategy: Strategy,
        context_window_tokens: u32,
        max_tokens: u32,
        probed_at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            model_id: model_id.into(),
            base_url: String::new(),
            probed_at: probed_at.into(),
            strategy,
            supports_native_tools: matches!(strategy, Strategy::NativeTool),
            supports_streaming: true,
            utf8_clean: true,
            context_window_tokens,
            max_tokens,
            notes: vec!["well_known: probe skipped".to_string()],
        }
    }

    /// Compute the canonical cache filename for the given
    /// `(model_id, base_url)` pair. The hash is the first 16 hex
    /// characters of `sha256(model_id || "\n" || base_url)` — 64 bits
    /// of collision space, which is comfortable for the universe of
    /// pairs a single user encounters.
    pub fn cache_path(model_profiles_dir: &Path, model_id: &str, base_url: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(model_id.as_bytes());
        hasher.update(b"\n");
        hasher.update(base_url.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        model_profiles_dir.join(format!("{hex}.{PROFILE_FILE_EXT}"))
    }

    /// Atomic write: serialize to a tempfile in the same directory,
    /// `sync_all`, rename, then fsync the parent. Mirrors
    /// `persistence.rs::OnDiskSession::save_to`. The directory is
    /// created with the user's umask — model profiles don't contain
    /// secrets, so 0700 isn't necessary (unlike session.json).
    pub fn save_to(&self, path: &Path) -> Result<(), ProfileError> {
        let dir = path.parent().ok_or_else(|| ProfileError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile path has no parent directory",
            ),
        })?;
        std::fs::create_dir_all(dir).map_err(|e| ProfileError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;

        let json =
            serde_json::to_vec_pretty(self).map_err(|e| ProfileError::Serialize(e.to_string()))?;

        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| ProfileError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        io::Write::write_all(tmp.as_file_mut(), &json).map_err(|e| ProfileError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.as_file().sync_all().map_err(|e| ProfileError::Io {
            path: tmp.path().to_path_buf(),
            source: e,
        })?;
        tmp.persist(path).map_err(|e| ProfileError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
        fsync_dir_best_effort(dir);
        Ok(())
    }

    /// Load and deserialize. Rejects mismatched schema versions with
    /// [`ProfileError::IncompatibleVersion`] so callers re-probe
    /// rather than trust stale data.
    pub fn load_from(path: &Path) -> Result<Self, ProfileError> {
        // v60.37 A2 — cap at 1 MiB; a profile is a single capability
        // matrix + probe outcome, well under this size legitimately.
        let bytes =
            crate::io_caps::read_capped(path, crate::io_caps::CAP_MODEL_PROFILE).map_err(|e| {
                ProfileError::Io {
                    path: path.to_path_buf(),
                    source: e,
                }
            })?;
        let profile: Self =
            serde_json::from_slice(&bytes).map_err(|e| ProfileError::Deserialize {
                path: path.to_path_buf(),
                error: e.to_string(),
            })?;
        if profile.schema_version != PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::IncompatibleVersion {
                path: path.to_path_buf(),
                got: profile.schema_version,
                expected: PROFILE_SCHEMA_VERSION,
            });
        }
        Ok(profile)
    }
}

/// Errors raised by [`ModelProfile::save_to`] and
/// [`ModelProfile::load_from`]. The Runner translates these into
/// "fell back to defaults; re-probing" log lines — a corrupted or
/// stale cache file is never fatal.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("I/O failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("serialize failure: {0}")]
    Serialize(String),

    #[error("deserialize failure at {path}: {error}")]
    Deserialize { path: PathBuf, error: String },

    #[error(
        "model profile at {path} uses schema_version {got}, this build expects {expected}; \
         re-probe will overwrite"
    )]
    IncompatibleVersion {
        path: PathBuf,
        got: u32,
        expected: u32,
    },
}

/// Resolve the on-disk profile cache directory. Honours the
/// `ATELIER_PROFILE_DIR` env override (used by tests); otherwise
/// defaults to `$HOME/.atelier/model_profiles/`. Returns `None` only
/// when neither env var nor `HOME` is set — extremely rare; callers
/// treat it as "probe but don't cache".
pub fn default_profile_dir() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var(PROFILE_DIR_ENV) {
        if !override_dir.is_empty() {
            return Some(PathBuf::from(override_dir));
        }
    }
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".atelier").join(PROFILE_DIR_NAME))
}

/// Default per-call `max_tokens` cap recorded on a freshly probed
/// profile. The probe driver itself doesn't measure this — there's no
/// reliable way to elicit a model's actual `max_tokens` ceiling from a
/// single calibration call. The Runner can override the profile's
/// `max_tokens` from CLI flags or adapter-specific knowledge; this is
/// just the sensible-default fallback.
pub const DEFAULT_PROFILE_MAX_TOKENS: u32 = 4096;

/// Name of the synthetic tool the native-tool probe asks the model to
/// call. Distinct from `harness_meta` (the production envelope-carrier)
/// so a model's probe response can't be mistaken for an envelope by the
/// rest of the runner.
pub const PROBE_TOOL_NAME: &str = "harness_calibration_echo";

/// Expected argument value the native-tool probe sends and checks. The
/// probe is considered a success only when the model echoes this exact
/// value back through the tool call's arguments — a partial echo (e.g.
/// the model paraphrasing the value) doesn't count.
pub const PROBE_TOOL_EXPECTED_VALUE: &str = "ok";

/// Fire one or two short calibration calls against an [`Adapter`] and
/// return a [`ProbeObservation`]. Used by [`ProfileStore::load_or_probe`]
/// (PROBE-4) on cache miss.
///
/// Probe A — *native tool use*: send a tool spec for
/// [`PROBE_TOOL_NAME`] and a one-line user message asking the model to
/// call it with `{"value": "ok"}`. Success means the response contains
/// a `tool_calls` entry whose name matches and whose `arguments.value`
/// equals [`PROBE_TOOL_EXPECTED_VALUE`].
///
/// Probe B — *JSON-sentinel envelope*: ask the model to reply with
/// exactly `<<<harness_meta>>>{"claimed_done":true}<<<end>>>`. Success
/// means [`parse_json_sentinel`] returns `Ok` against the response
/// text.
///
/// Error handling: a genuine connectivity / auth failure on Probe A
/// (`Unreachable`, `Auth`, `NotConfigured`, `ContextOverflow`) is
/// propagated immediately — there's no point trying Probe B against a
/// dead endpoint, and we don't want to cache "this model is
/// `RegexProse`-only" because of a network hiccup. Other errors
/// (`Malformed`, `Provider`, `RateLimited`) are recorded as notes and
/// the corresponding flag stays `false`; the function still returns
/// `Ok` so the caller can either re-probe later or cache a
/// best-effort profile.
pub async fn probe_model(adapter: &dyn Adapter) -> Result<ProbeObservation, AdapterError> {
    let mut obs = ProbeObservation::empty();
    let mut notes: Vec<String> = Vec::new();

    // Probe A — native tool call.
    let tool_spec = ToolSpec {
        name: PROBE_TOOL_NAME.to_string(),
        description: "Atelier calibration probe. Call this tool with the requested value."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }),
    };
    let probe_a_messages = [
        Message::text(
            Role::System,
            "You are an Atelier calibration probe. Follow the user's instructions exactly. \
             Do not add commentary.",
        ),
        Message::text(
            Role::User,
            format!(
                "Call the tool `{PROBE_TOOL_NAME}` with the argument \
                 {{\"value\": \"{PROBE_TOOL_EXPECTED_VALUE}\"}}."
            ),
        ),
    ];
    match adapter.chat(&probe_a_messages, &[tool_spec]).await {
        Ok(resp) => {
            let matched = resp.tool_calls.iter().any(|tc| {
                tc.name == PROBE_TOOL_NAME
                    && tc.arguments.get("value").and_then(|v| v.as_str())
                        == Some(PROBE_TOOL_EXPECTED_VALUE)
            });
            obs.native_tool_call_succeeded = matched;
            if !matched {
                notes.push(format!(
                    "native_probe: model did not emit a matching tool call \
                     (got {} tool_calls, text {} chars)",
                    resp.tool_calls.len(),
                    resp.text.chars().count()
                ));
            }
            if contains_replacement_char(&resp.text) {
                obs.utf8_clean = false;
            }
        }
        Err(e) if is_fatal_for_probe(&e) => return Err(e),
        Err(e) => notes.push(format!("native_probe: adapter error {e}")),
    }

    // Probe B — JSON-sentinel envelope.
    let probe_b_messages = [
        Message::text(
            Role::System,
            "You are an Atelier calibration probe. Output ONLY the requested envelope \
             with no surrounding prose, no code fences, no leading or trailing whitespace.",
        ),
        Message::text(
            Role::User,
            format!("Reply with exactly: {SENTINEL_OPEN}{{\"claimed_done\":true}}{SENTINEL_CLOSE}"),
        ),
    ];
    match adapter.chat(&probe_b_messages, &[]).await {
        Ok(resp) => {
            obs.json_sentinel_succeeded = parse_json_sentinel(&resp.text).is_ok();
            if !obs.json_sentinel_succeeded {
                notes.push(format!(
                    "sentinel_probe: envelope not parseable (text len = {})",
                    resp.text.len()
                ));
            }
            if contains_replacement_char(&resp.text) {
                obs.utf8_clean = false;
            }
        }
        Err(e) if is_fatal_for_probe(&e) => return Err(e),
        Err(e) => notes.push(format!("sentinel_probe: adapter error {e}")),
    }

    // Streaming is not separately probed in v51 — the §1 capability
    // claim is the best available signal until a streaming-specific
    // calibration lands. `from_observations` records this as a note
    // when `streaming_ok=false`, so absent evidence we set it true
    // here and let the caller override based on capabilities() if
    // they want a stricter signal.
    obs.streaming_ok = adapter.capabilities().streaming.is_usable();

    // Notes are stored on the eventual `ModelProfile`, not on the
    // observation itself — the caller threads them through
    // `ModelProfile::from_observations`. Stash them on a side channel
    // by logging; the caller can also rebuild them from the
    // observation flags. (We intentionally don't widen the
    // `ProbeObservation` type with a notes vec because then it's no
    // longer `Copy`, which complicates the unit tests for the pure
    // decision function.)
    for note in &notes {
        tracing::debug!(target: "atelier::probe", note = note.as_str(), "probe note");
    }
    Ok(obs)
}

/// Errors that should short-circuit a probe rather than be recorded as
/// "strategy didn't work." Mirrors [`AdapterError::requires_user_decision`]
/// plus the unreachable case (no point poking a dead endpoint twice).
fn is_fatal_for_probe(err: &AdapterError) -> bool {
    matches!(
        err,
        AdapterError::Auth(_)
            | AdapterError::NotConfigured(_)
            | AdapterError::ContextOverflow { .. }
            | AdapterError::Unreachable(_)
    )
}

fn contains_replacement_char(s: &str) -> bool {
    s.contains('\u{FFFD}')
}

// ---------- PROBE-4: ProfileStore ----------

/// Why a [`ProfileStore::load_or_probe`] call returned a given profile.
/// Surfaced to the Runner (and onto the event bus as
/// `Event::ModelProfileLoaded`) so the UI can tell the user whether
/// they hit a warm cache or just paid a probe round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeLoadOutcome {
    /// A valid, schema-current profile was found on disk and used
    /// without probing. The cheap-and-common path after the first
    /// run against a given `(model_id, base_url)`.
    CacheHit,
    /// No cached profile existed; the probe ran and the result was
    /// written to the cache.
    Probed,
    /// A cached profile existed but was either stale (mismatched
    /// schema version) or the caller asked for `force_reprobe`. The
    /// new probe overwrote the prior file.
    Reprobed,
    /// The probe ran but the result was not persisted — either the
    /// store is ephemeral (no cache dir) or the save itself failed.
    /// The returned profile is still trustworthy for this session.
    NotCached,
}

impl ProbeLoadOutcome {
    /// v57 (H7 fix) — canonical snake_case wire label. Mirrors the
    /// `serde(rename_all = "snake_case")` projection so consumers
    /// don't have to round-trip through `serde_json::to_value` just
    /// to render the badge.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::CacheHit => "cache_hit",
            Self::Probed => "probed",
            Self::Reprobed => "reprobed",
            Self::NotCached => "not_cached",
        }
    }
}

/// Filesystem-backed profile cache. `dir = None` means "probe every
/// time, never persist" — used by tests and by callers that don't have
/// a writable `$HOME` (sandboxed CI, container builds).
///
/// Typical construction: [`ProfileStore::user_default`] (which honours
/// `ATELIER_PROFILE_DIR`); tests use [`ProfileStore::at`] with a
/// `TempDir`.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    dir: Option<PathBuf>,
}

impl ProfileStore {
    /// Standard user-scope store under `~/.atelier/model_profiles/`,
    /// honouring `ATELIER_PROFILE_DIR`. Returns an ephemeral store if
    /// neither override nor `HOME` is set.
    pub fn user_default() -> Self {
        Self {
            dir: default_profile_dir(),
        }
    }

    /// Store backed by an explicit directory. The directory is created
    /// lazily on the first successful save.
    pub fn at(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }

    /// Probe every call; never persist. Use this when callers want
    /// fresh observations without touching shared state.
    pub fn ephemeral() -> Self {
        Self { dir: None }
    }

    /// The configured cache directory, if any. Public so the Runner
    /// can log it.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Look up the profile for `(adapter.model_id(), base_url)`. On a
    /// cache hit with the current schema version, returns the cached
    /// profile without calling the adapter. Otherwise fires
    /// [`probe_model`], builds a profile, and writes it (if the store
    /// has a directory).
    ///
    /// `force_reprobe = true` bypasses the cache read and overwrites
    /// any existing file with the fresh probe.
    ///
    /// `probed_at` is the RFC 3339 timestamp the caller wants recorded
    /// on a freshly written profile (so tests can pin it; the Runner
    /// uses its existing `now_rfc3339()`).
    pub async fn load_or_probe(
        &self,
        adapter: &dyn Adapter,
        base_url: &str,
        force_reprobe: bool,
        probed_at: impl Into<String>,
    ) -> Result<(ModelProfile, ProbeLoadOutcome), AdapterError> {
        let model_id = adapter.model_id().to_string();
        let caps = adapter.capabilities();

        // 1. Cache lookup unless explicitly bypassed.
        let mut had_stale_entry = false;
        if !force_reprobe {
            if let Some(dir) = &self.dir {
                let path = ModelProfile::cache_path(dir, &model_id, base_url);
                match ModelProfile::load_from(&path) {
                    Ok(profile) => {
                        tracing::info!(
                            target: "atelier::probe",
                            model_id = %model_id,
                            base_url = %base_url,
                            strategy = profile.strategy.as_str(),
                            cache_path = %path.display(),
                            "model profile cache hit"
                        );
                        return Ok((profile, ProbeLoadOutcome::CacheHit));
                    }
                    Err(ProfileError::Io { ref source, .. })
                        if source.kind() == io::ErrorKind::NotFound =>
                    {
                        // Cache miss — fall through to probe.
                    }
                    Err(e) => {
                        // Stale schema, corrupted JSON, permissions, etc.
                        // Log and re-probe; the new profile overwrites.
                        had_stale_entry = true;
                        tracing::warn!(
                            target: "atelier::probe",
                            model_id = %model_id,
                            cache_path = %path.display(),
                            error = %e,
                            "model profile cache load failed; will re-probe"
                        );
                    }
                }
            }
        }

        // 2. Probe. Genuine network / auth failures propagate; the
        //    caller decides whether to fall back to a stub profile or
        //    fail the run.
        let obs = probe_model(adapter).await?;
        let profile = ModelProfile::from_observations(
            model_id.clone(),
            base_url.to_string(),
            probed_at,
            obs,
            caps.context_window_tokens,
            DEFAULT_PROFILE_MAX_TOKENS,
            Vec::new(),
        );

        // 3. Persist (best-effort). A save failure downgrades the
        //    outcome but doesn't fail the run — the in-memory profile
        //    is still valid for this session.
        let outcome = match &self.dir {
            Some(dir) => {
                let path = ModelProfile::cache_path(dir, &model_id, base_url);
                match profile.save_to(&path) {
                    Ok(()) => {
                        tracing::info!(
                            target: "atelier::probe",
                            model_id = %model_id,
                            base_url = %base_url,
                            strategy = profile.strategy.as_str(),
                            cache_path = %path.display(),
                            forced = force_reprobe,
                            "model profile probed and cached"
                        );
                        if force_reprobe || had_stale_entry {
                            ProbeLoadOutcome::Reprobed
                        } else {
                            ProbeLoadOutcome::Probed
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "atelier::probe",
                            model_id = %model_id,
                            cache_path = %path.display(),
                            error = %e,
                            "model profile probed but persistence failed"
                        );
                        ProbeLoadOutcome::NotCached
                    }
                }
            }
            None => ProbeLoadOutcome::NotCached,
        };

        Ok((profile, outcome))
    }
}

#[cfg(unix)]
fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(not(unix))]
fn fsync_dir_best_effort(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{
        ChatResponse, MockAdapter, StopReason, StreamChunk, ToolCallRequest, Usage,
    };
    use crate::context::TokenSource;
    use tempfile::TempDir;

    fn complete(text: &str, tool_calls: Vec<ToolCallRequest>) -> StreamChunk {
        StreamChunk::Complete {
            response: ChatResponse {
                text: text.into(),
                tool_calls,
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
        }
    }

    fn tool_call(name: &str, value: &str) -> ToolCallRequest {
        ToolCallRequest {
            id: format!("call_{name}"),
            name: name.into(),
            arguments: serde_json::json!({ "value": value }),
        }
    }

    fn fixture(model_id: &str, base_url: &str, strategy: Strategy) -> ModelProfile {
        ModelProfile {
            schema_version: PROFILE_SCHEMA_VERSION,
            model_id: model_id.to_string(),
            base_url: base_url.to_string(),
            probed_at: "2026-05-17T12:00:00Z".to_string(),
            strategy,
            supports_native_tools: matches!(strategy, Strategy::NativeTool),
            supports_streaming: true,
            utf8_clean: true,
            context_window_tokens: 8192,
            max_tokens: 4096,
            notes: vec!["probe ok".to_string()],
        }
    }

    #[test]
    fn probe_load_outcome_wire_label_agrees_with_serde() {
        // Regression for v58 HIGH-bug-1 — pin `wire_label` to the
        // serde `rename_all = "snake_case"` projection so a variant
        // rename can't drift between the hand match and serde.
        for outcome in [
            ProbeLoadOutcome::CacheHit,
            ProbeLoadOutcome::Probed,
            ProbeLoadOutcome::Reprobed,
            ProbeLoadOutcome::NotCached,
        ] {
            let json = serde_json::to_value(outcome).unwrap();
            let serde_label = json
                .as_str()
                .expect("ProbeLoadOutcome serializes as a string");
            assert_eq!(
                serde_label,
                outcome.wire_label(),
                "wire_label({outcome:?}) must match serde projection",
            );
        }
    }

    #[test]
    fn cache_path_is_stable_for_same_inputs() {
        let dir = Path::new("/tmp/profiles");
        let a = ModelProfile::cache_path(dir, "local:qwen", "http://localhost:11434/v1");
        let b = ModelProfile::cache_path(dir, "local:qwen", "http://localhost:11434/v1");
        assert_eq!(a, b);
        // 16 hex chars + ".json" + dir prefix
        let name = a.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 16 + 1 + 4);
        assert!(name.ends_with(".json"));
        assert!(name.chars().take(16).all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_path_differs_for_different_models() {
        let dir = Path::new("/tmp/profiles");
        let a = ModelProfile::cache_path(dir, "local:qwen", "http://localhost:11434/v1");
        let b = ModelProfile::cache_path(dir, "local:llama", "http://localhost:11434/v1");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_path_differs_for_different_base_urls() {
        let dir = Path::new("/tmp/profiles");
        let a = ModelProfile::cache_path(dir, "local:qwen", "http://localhost:11434/v1");
        let b = ModelProfile::cache_path(dir, "local:qwen", "http://localhost:1234/v1");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_path_does_not_collide_via_concat_ambiguity() {
        // `("ab", "cd")` and `("a", "bcd")` would collide if we'd
        // concatenated without a separator. The "\n" between fields
        // prevents that — these must produce different hashes.
        let dir = Path::new("/tmp/profiles");
        let a = ModelProfile::cache_path(dir, "ab", "cd");
        let b = ModelProfile::cache_path(dir, "a", "bcd");
        assert_ne!(a, b);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let path = ModelProfile::cache_path(tmp.path(), "local:qwen", "http://x/v1");
        let written = fixture("local:qwen", "http://x/v1", Strategy::JsonSentinel);
        written.save_to(&path).expect("save");
        let loaded = ModelProfile::load_from(&path).expect("load");
        assert_eq!(loaded, written);
    }

    #[test]
    fn load_rejects_mismatched_schema_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stale.json");
        let stale = serde_json::json!({
            "schema_version": PROFILE_SCHEMA_VERSION + 99,
            "model_id": "local:qwen",
            "base_url": "http://x/v1",
            "probed_at": "2026-01-01T00:00:00Z",
            "strategy": "json_sentinel",
            "supports_native_tools": false,
            "supports_streaming": true,
            "utf8_clean": true,
            "context_window_tokens": 8192,
            "max_tokens": 4096,
            "notes": []
        });
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let err = ModelProfile::load_from(&path).expect_err("should reject");
        match err {
            ProfileError::IncompatibleVersion { got, expected, .. } => {
                assert_eq!(got, PROFILE_SCHEMA_VERSION + 99);
                assert_eq!(expected, PROFILE_SCHEMA_VERSION);
            }
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
    }

    #[test]
    fn load_surfaces_deserialize_error_with_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("garbage.json");
        std::fs::write(&path, b"not json at all").unwrap();
        let err = ModelProfile::load_from(&path).expect_err("should fail");
        match err {
            ProfileError::Deserialize { path: p, .. } => assert_eq!(p, path),
            other => panic!("expected Deserialize, got {other:?}"),
        }
    }

    #[test]
    fn save_creates_parent_directory_if_missing() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        let path = ModelProfile::cache_path(&nested, "m", "u");
        let p = fixture("m", "u", Strategy::NativeTool);
        p.save_to(&path).expect("save should mkdir -p");
        assert!(path.exists());
    }

    #[test]
    fn save_atomic_does_not_leave_partial_on_serialize_success() {
        // We can't easily induce a serialize failure for our struct,
        // but we can prove the temp-file/persist pattern doesn't
        // leave dotfiles behind on a successful write.
        let tmp = TempDir::new().unwrap();
        let path = ModelProfile::cache_path(tmp.path(), "m", "u");
        let p = fixture("m", "u", Strategy::JsonSentinel);
        p.save_to(&path).unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "exactly the target file, no leftover tmp");
    }

    fn obs(native: bool, sentinel: bool, utf8: bool, streaming: bool) -> ProbeObservation {
        ProbeObservation {
            native_tool_call_succeeded: native,
            json_sentinel_succeeded: sentinel,
            utf8_clean: utf8,
            streaming_ok: streaming,
        }
    }

    #[test]
    fn decide_strategy_prefers_native_when_native_works() {
        assert_eq!(
            decide_strategy(&obs(true, true, true, true)),
            Strategy::NativeTool
        );
        // Native wins even if sentinel also worked.
        assert_eq!(
            decide_strategy(&obs(true, false, true, true)),
            Strategy::NativeTool
        );
    }

    #[test]
    fn decide_strategy_falls_back_to_sentinel_when_native_fails() {
        assert_eq!(
            decide_strategy(&obs(false, true, true, true)),
            Strategy::JsonSentinel
        );
    }

    #[test]
    fn decide_strategy_falls_back_to_prose_when_both_fail() {
        assert_eq!(
            decide_strategy(&obs(false, false, true, true)),
            Strategy::RegexProse
        );
    }

    #[test]
    fn decide_strategy_ignores_utf8_and_streaming_signals() {
        // utf8/streaming are recorded but don't shift strategy.
        let native_with_dirty = obs(true, false, false, false);
        assert_eq!(decide_strategy(&native_with_dirty), Strategy::NativeTool);
        let sentinel_with_dirty = obs(false, true, false, false);
        assert_eq!(
            decide_strategy(&sentinel_with_dirty),
            Strategy::JsonSentinel
        );
        let prose_with_dirty = obs(false, false, false, false);
        assert_eq!(decide_strategy(&prose_with_dirty), Strategy::RegexProse);
    }

    #[test]
    fn empty_observation_yields_prose() {
        assert_eq!(
            decide_strategy(&ProbeObservation::empty()),
            Strategy::RegexProse
        );
    }

    #[test]
    fn from_observations_derives_strategy_and_passes_through_signals() {
        let p = ModelProfile::from_observations(
            "local:qwen",
            "http://x/v1",
            "2026-05-17T12:00:00Z",
            obs(false, true, true, true),
            8192,
            4096,
            vec!["probe ok".to_string()],
        );
        assert_eq!(p.strategy, Strategy::JsonSentinel);
        assert!(!p.supports_native_tools);
        assert!(p.supports_streaming);
        assert!(p.utf8_clean);
        assert_eq!(p.notes, vec!["probe ok".to_string()]);
    }

    #[test]
    fn from_observations_appends_utf8_warning_to_notes() {
        let p = ModelProfile::from_observations(
            "local:qwen",
            "http://x/v1",
            "2026-05-17T12:00:00Z",
            obs(true, true, false, true),
            8192,
            4096,
            Vec::new(),
        );
        assert_eq!(p.strategy, Strategy::NativeTool);
        assert!(!p.utf8_clean);
        assert!(
            p.notes.iter().any(|n| n.contains("utf8_clean=false")),
            "expected utf8 warning in notes: {:?}",
            p.notes
        );
    }

    #[test]
    fn from_observations_appends_streaming_warning_when_streaming_unparseable() {
        let p = ModelProfile::from_observations(
            "local:qwen",
            "http://x/v1",
            "2026-05-17T12:00:00Z",
            obs(true, true, true, false),
            8192,
            4096,
            Vec::new(),
        );
        assert!(!p.supports_streaming);
        assert!(
            p.notes.iter().any(|n| n.contains("streaming_ok=false")),
            "expected streaming warning in notes: {:?}",
            p.notes
        );
    }

    #[test]
    fn skipped_for_well_known_marks_no_native_when_strategy_lower() {
        let p = ModelProfile::skipped_for_well_known(
            "mock:run",
            Strategy::JsonSentinel,
            4096,
            1024,
            "2026-05-17T00:00:00Z",
        );
        assert_eq!(p.strategy, Strategy::JsonSentinel);
        assert!(!p.supports_native_tools);
    }

    #[test]
    fn skipped_for_well_known_marks_native_when_strategy_native() {
        let p = ModelProfile::skipped_for_well_known(
            "anthropic:claude-opus-4-7",
            Strategy::NativeTool,
            200_000,
            8192,
            "2026-05-17T00:00:00Z",
        );
        assert_eq!(p.strategy, Strategy::NativeTool);
        assert!(p.supports_native_tools);
    }

    // ---------- probe driver (PROBE-3) ----------

    #[tokio::test]
    async fn probe_records_native_tool_when_model_calls_expected_tool() {
        let mock = MockAdapter::new("mock:probe");
        // Probe A: model calls the calibration tool with the right value.
        mock.queue_stream(vec![complete(
            "",
            vec![tool_call(PROBE_TOOL_NAME, PROBE_TOOL_EXPECTED_VALUE)],
        )]);
        // Probe B: model emits a sentinel-wrapped envelope.
        mock.queue_stream(vec![complete(
            "<<<harness_meta>>>{\"claimed_done\":true}<<<end>>>",
            vec![],
        )]);

        let obs = probe_model(&mock).await.expect("probe should succeed");
        assert!(obs.native_tool_call_succeeded);
        assert!(obs.json_sentinel_succeeded);
        assert!(obs.utf8_clean);
        assert!(obs.streaming_ok);
        assert_eq!(decide_strategy(&obs), Strategy::NativeTool);
    }

    #[tokio::test]
    async fn probe_rejects_native_tool_when_args_do_not_match() {
        let mock = MockAdapter::new("mock:probe");
        // Right tool name, wrong value — not a match.
        mock.queue_stream(vec![complete(
            "",
            vec![tool_call(PROBE_TOOL_NAME, "something-else")],
        )]);
        mock.queue_stream(vec![complete(
            "<<<harness_meta>>>{\"claimed_done\":true}<<<end>>>",
            vec![],
        )]);

        let obs = probe_model(&mock).await.expect("probe should succeed");
        assert!(!obs.native_tool_call_succeeded);
        assert!(obs.json_sentinel_succeeded);
        assert_eq!(decide_strategy(&obs), Strategy::JsonSentinel);
    }

    #[tokio::test]
    async fn probe_rejects_native_tool_when_wrong_tool_name() {
        let mock = MockAdapter::new("mock:probe");
        mock.queue_stream(vec![complete(
            "",
            vec![tool_call("some_other_tool", PROBE_TOOL_EXPECTED_VALUE)],
        )]);
        mock.queue_stream(vec![complete(
            "<<<harness_meta>>>{\"claimed_done\":true}<<<end>>>",
            vec![],
        )]);

        let obs = probe_model(&mock).await.expect("probe should succeed");
        assert!(!obs.native_tool_call_succeeded);
        assert_eq!(decide_strategy(&obs), Strategy::JsonSentinel);
    }

    #[tokio::test]
    async fn probe_records_sentinel_failure_when_envelope_unparseable() {
        let mock = MockAdapter::new("mock:probe");
        // Probe A: no tool call.
        mock.queue_stream(vec![complete("I cannot do that", vec![])]);
        // Probe B: prose, no sentinels.
        mock.queue_stream(vec![complete("here's an envelope: { ...", vec![])]);

        let obs = probe_model(&mock).await.expect("probe should succeed");
        assert!(!obs.native_tool_call_succeeded);
        assert!(!obs.json_sentinel_succeeded);
        // Both probes failed — falls back to RegexProse.
        assert_eq!(decide_strategy(&obs), Strategy::RegexProse);
    }

    #[tokio::test]
    async fn probe_propagates_auth_error_immediately() {
        // No queued stream → MockAdapter raises NotConfigured, which
        // is fatal for the probe. The probe must propagate rather
        // than continue to Probe B.
        let mock = MockAdapter::new("mock:probe");
        let err = probe_model(&mock)
            .await
            .expect_err("probe must propagate fatal adapter error");
        assert!(matches!(err, AdapterError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn probe_detects_replacement_char_in_response_text() {
        let mock = MockAdapter::new("mock:probe");
        // U+FFFD in the response text — signals a tokenizer mismatch
        // on the wire even though Rust strings are always valid UTF-8.
        mock.queue_stream(vec![complete("oops \u{FFFD} byte", vec![])]);
        mock.queue_stream(vec![complete(
            "<<<harness_meta>>>{\"claimed_done\":true}<<<end>>>",
            vec![],
        )]);

        let obs = probe_model(&mock).await.expect("probe");
        assert!(!obs.utf8_clean);
        // Sentinel was clean, so strategy is still JsonSentinel.
        assert_eq!(decide_strategy(&obs), Strategy::JsonSentinel);
    }

    #[tokio::test]
    async fn probe_threads_streaming_signal_from_capabilities() {
        // Default MockAdapter advertises Supported streaming.
        let mock = MockAdapter::new("mock:probe");
        mock.queue_stream(vec![complete("", vec![])]);
        mock.queue_stream(vec![complete("", vec![])]);
        let obs = probe_model(&mock).await.expect("probe");
        assert!(obs.streaming_ok);
    }

    #[test]
    fn is_fatal_for_probe_matches_user_decision_errors_plus_unreachable() {
        assert!(is_fatal_for_probe(&AdapterError::Auth("bad".into())));
        assert!(is_fatal_for_probe(&AdapterError::NotConfigured("x".into())));
        assert!(is_fatal_for_probe(&AdapterError::Unreachable("net".into())));
        assert!(is_fatal_for_probe(&AdapterError::ContextOverflow {
            needed_tokens: 100,
            limit_tokens: 50
        }));
        // Non-fatal: probe records a note and continues.
        assert!(!is_fatal_for_probe(&AdapterError::Malformed("bad".into())));
        assert!(!is_fatal_for_probe(&AdapterError::RateLimited {
            retry_after_ms: 1000
        }));
        assert!(!is_fatal_for_probe(&AdapterError::Provider {
            status: 500,
            body: "oops".into()
        }));
    }

    // ---------- profile store (PROBE-4) ----------

    /// Queue two probe responses on the mock so probe_model runs through.
    /// `native_success` controls whether Probe A round-trips a matching
    /// tool call; `sentinel_success` controls Probe B.
    fn seed_probe(mock: &MockAdapter, native_success: bool, sentinel_success: bool) {
        let probe_a_tool_calls = if native_success {
            vec![tool_call(PROBE_TOOL_NAME, PROBE_TOOL_EXPECTED_VALUE)]
        } else {
            Vec::new()
        };
        let probe_b_text = if sentinel_success {
            "<<<harness_meta>>>{\"claimed_done\":true}<<<end>>>".to_string()
        } else {
            "sorry, can't help".to_string()
        };
        mock.queue_stream(vec![complete("", probe_a_tool_calls)]);
        mock.queue_stream(vec![complete(&probe_b_text, vec![])]);
    }

    #[tokio::test]
    async fn store_probes_and_caches_on_first_use() {
        let tmp = TempDir::new().unwrap();
        let store = ProfileStore::at(tmp.path().to_path_buf());
        let mock = MockAdapter::new("mock:store");
        seed_probe(&mock, true, true);

        let (profile, outcome) = store
            .load_or_probe(&mock, "http://x/v1", false, "2026-05-17T12:00:00Z")
            .await
            .expect("first probe should succeed");
        assert_eq!(outcome, ProbeLoadOutcome::Probed);
        assert_eq!(profile.strategy, Strategy::NativeTool);
        assert_eq!(profile.base_url, "http://x/v1");
        assert_eq!(profile.model_id, "mock:store");

        // Cache file exists at the canonical path.
        let path = ModelProfile::cache_path(tmp.path(), "mock:store", "http://x/v1");
        assert!(path.exists(), "cache file should have been written");
    }

    #[tokio::test]
    async fn store_returns_cache_hit_without_calling_adapter() {
        let tmp = TempDir::new().unwrap();
        let store = ProfileStore::at(tmp.path().to_path_buf());

        // Pre-populate the cache.
        let cached = ModelProfile {
            schema_version: PROFILE_SCHEMA_VERSION,
            model_id: "mock:store".into(),
            base_url: "http://x/v1".into(),
            probed_at: "2026-05-17T00:00:00Z".into(),
            strategy: Strategy::JsonSentinel,
            supports_native_tools: false,
            supports_streaming: true,
            utf8_clean: true,
            context_window_tokens: 8192,
            max_tokens: 4096,
            notes: vec!["pre-seeded".into()],
        };
        let path = ModelProfile::cache_path(tmp.path(), "mock:store", "http://x/v1");
        cached.save_to(&path).unwrap();

        // Mock with NO queued streams → if load_or_probe calls chat(),
        // it'll fail with NotConfigured. Cache hit must skip chat() entirely.
        let mock = MockAdapter::new("mock:store");

        let (profile, outcome) = store
            .load_or_probe(&mock, "http://x/v1", false, "2026-05-17T12:00:00Z")
            .await
            .expect("cache hit must not call adapter");
        assert_eq!(outcome, ProbeLoadOutcome::CacheHit);
        assert_eq!(profile, cached);
    }

    #[tokio::test]
    async fn store_reprobes_when_force_reprobe_set() {
        let tmp = TempDir::new().unwrap();
        let store = ProfileStore::at(tmp.path().to_path_buf());

        // Cache says JsonSentinel.
        let cached = ModelProfile {
            schema_version: PROFILE_SCHEMA_VERSION,
            model_id: "mock:store".into(),
            base_url: "http://x/v1".into(),
            probed_at: "2026-05-17T00:00:00Z".into(),
            strategy: Strategy::JsonSentinel,
            supports_native_tools: false,
            supports_streaming: true,
            utf8_clean: true,
            context_window_tokens: 8192,
            max_tokens: 4096,
            notes: vec![],
        };
        let path = ModelProfile::cache_path(tmp.path(), "mock:store", "http://x/v1");
        cached.save_to(&path).unwrap();

        // But fresh probe says NativeTool. With force_reprobe, the
        // fresh probe wins and the file is overwritten.
        let mock = MockAdapter::new("mock:store");
        seed_probe(&mock, true, true);

        let (profile, outcome) = store
            .load_or_probe(&mock, "http://x/v1", true, "2026-05-17T12:00:00Z")
            .await
            .expect("forced reprobe");
        assert_eq!(outcome, ProbeLoadOutcome::Reprobed);
        assert_eq!(profile.strategy, Strategy::NativeTool);
        assert_eq!(profile.probed_at, "2026-05-17T12:00:00Z");

        let on_disk = ModelProfile::load_from(&path).unwrap();
        assert_eq!(on_disk.strategy, Strategy::NativeTool);
    }

    #[tokio::test]
    async fn store_reprobes_when_cached_schema_is_stale() {
        let tmp = TempDir::new().unwrap();
        let store = ProfileStore::at(tmp.path().to_path_buf());

        // Hand-write a stale-schema cache file.
        let path = ModelProfile::cache_path(tmp.path(), "mock:store", "http://x/v1");
        let stale = serde_json::json!({
            "schema_version": PROFILE_SCHEMA_VERSION + 99,
            "model_id": "mock:store",
            "base_url": "http://x/v1",
            "probed_at": "2025-01-01T00:00:00Z",
            "strategy": "native_tool",
            "supports_native_tools": true,
            "supports_streaming": true,
            "utf8_clean": true,
            "context_window_tokens": 1,
            "max_tokens": 1,
            "notes": []
        });
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();

        let mock = MockAdapter::new("mock:store");
        seed_probe(&mock, false, true);

        let (profile, outcome) = store
            .load_or_probe(&mock, "http://x/v1", false, "2026-05-17T12:00:00Z")
            .await
            .expect("stale cache should reprobe");
        assert_eq!(outcome, ProbeLoadOutcome::Reprobed);
        assert_eq!(profile.strategy, Strategy::JsonSentinel);
        assert_eq!(profile.context_window_tokens, 200_000);
    }

    #[tokio::test]
    async fn ephemeral_store_probes_but_does_not_persist() {
        let store = ProfileStore::ephemeral();
        let mock = MockAdapter::new("mock:store");
        seed_probe(&mock, false, true);

        let (profile, outcome) = store
            .load_or_probe(&mock, "http://x/v1", false, "2026-05-17T12:00:00Z")
            .await
            .expect("ephemeral probe");
        assert_eq!(outcome, ProbeLoadOutcome::NotCached);
        assert_eq!(profile.strategy, Strategy::JsonSentinel);
        assert!(store.dir().is_none());
    }

    #[tokio::test]
    async fn store_propagates_fatal_probe_error_without_caching() {
        let tmp = TempDir::new().unwrap();
        let store = ProfileStore::at(tmp.path().to_path_buf());
        // No queued streams → MockAdapter raises NotConfigured (which
        // probe_model treats as fatal).
        let mock = MockAdapter::new("mock:store");

        let err = store
            .load_or_probe(&mock, "http://x/v1", false, "2026-05-17T12:00:00Z")
            .await
            .expect_err("fatal probe error must propagate");
        assert!(matches!(err, AdapterError::NotConfigured(_)));

        // Nothing should have been written to disk.
        let path = ModelProfile::cache_path(tmp.path(), "mock:store", "http://x/v1");
        assert!(!path.exists());
    }

    #[test]
    fn default_profile_dir_honours_env_override() {
        // Save + restore the env so we don't leak into other tests
        // that share the same process (cargo test by default runs
        // tests in parallel within a crate; this var is read-only in
        // those threads, so a sequential-in-test save/restore is the
        // simplest safety net).
        let prior = std::env::var(PROFILE_DIR_ENV).ok();
        // SAFETY: this is the documented test-only override; set+restore
        // pattern protects parallel tests as long as no other test
        // writes to the same var simultaneously. Our test suite has
        // exactly one test that touches this env var.
        unsafe {
            std::env::set_var(PROFILE_DIR_ENV, "/tmp/atelier-profiles-test");
        }
        let resolved = default_profile_dir();
        assert_eq!(resolved, Some(PathBuf::from("/tmp/atelier-profiles-test")));
        unsafe {
            match prior {
                Some(v) => std::env::set_var(PROFILE_DIR_ENV, v),
                None => std::env::remove_var(PROFILE_DIR_ENV),
            }
        }
    }
}
