//! v53 — `.atelier/providers.toml` loader.
//!
//! Atelier's runtime knobs (which BYOM adapter, which model, which base
//! URL, max turns, probe policy) live in a small TOML file the binary
//! picks up automatically. v53 reshaped the format so a single file can
//! declare *multiple named profiles* (local LLM, cloud-hosted Anthropic,
//! a self-hosted vLLM cluster, …) and the user picks one with a flag
//! instead of re-typing everything.
//!
//! # On-disk shape
//!
//! ```toml
//! # .atelier/providers.toml
//!
//! default = "local"   # optional; picks one of [providers.<name>] tables
//!
//! [providers.local]
//! provider = "openai-compat"
//! base_url = "http://localhost:11434/v1"
//! model    = "local:qwen2.5-coder:7b"
//!
//! [providers.cloud]
//! provider = "anthropic"
//! model    = "anthropic:claude-opus-4-7"
//!
//! # Optional orthogonal sections — runtime knobs that aren't
//! # provider-specific.
//!
//! [runner]
//! max_turns = 32
//!
//! [probe]
//! policy = "auto"            # "auto" | "skip" | "force"
//! ```
//!
//! Every section and every field inside a profile is optional. A profile
//! with only `provider = "anthropic"` is valid and inherits defaults for
//! the rest. The `default` field is optional too — without it the binary
//! falls through to built-in defaults (`mock`) unless the CLI passes
//! `--profile <NAME>`.
//!
//! # Override precedence
//!
//! Top wins; layers compose:
//!
//! ```text
//!   1. CLI flags                                  (per-invocation)
//!   2. resolved profile fields (from the file)    (named, persisted)
//!   3. Built-in defaults                          (provider=mock,
//!                                                  max_turns=32,
//!                                                  probe=auto)
//! ```
//!
//! The "resolved profile" is whichever `[providers.<name>]` table
//! matches:
//!
//!   - `--profile <NAME>` from the CLI, if given;
//!   - otherwise the `default = "<NAME>"` field from the file;
//!   - otherwise no profile is resolved (the CLI is expected to
//!     specify all relevant flags directly).
//!
//! Per-field CLI flags (`--provider`, `--model`, `--base-url`,
//! `--max-turns`, `--no-probe`/`--force-probe`) still override the
//! resolved profile field-by-field, so the user can flip just one
//! knob without copying the whole profile.
//!
//! # Discovery
//!
//! [`ProvidersConfig::load`] consults `<repo>/.atelier/providers.toml`
//! first and falls back to `~/.atelier/providers.toml`. Missing both is
//! *not* an error — `Ok(None)` so the caller can use built-in defaults.
//! A file that exists but doesn't parse is fatal: silently ignoring a
//! malformed config would let a typo silently shift the runtime to
//! defaults.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------- on-disk shape (what serde reads) ----------

/// Top-level providers config. The shape mirrors the v53 example in
/// this module's docstring: a `default` selector + a map of named
/// `[providers.<name>]` profiles + optional orthogonal `[runner]` /
/// `[probe]` sections.
///
/// Use [`ProvidersConfig::load`] for normal callers; tests construct
/// directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Optional. Name of the profile [`Self::resolve_profile`] picks
    /// when the CLI doesn't pass `--profile <NAME>`. Must reference
    /// one of the keys in [`Self::providers`] or the file is rejected
    /// with [`ConfigError::Invalid`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Named profiles. Each key is the user-chosen profile name
    /// (`local`, `cloud`, `staging`, …); the value carries the
    /// adapter kind + provider-specific knobs. Empty map is allowed
    /// (the file can declare only `[runner]` / `[probe]` if that's
    /// what the user wants).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderProfile>,

    /// Loop-driver knobs (max turns, etc.). See [`RunnerSection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<RunnerSection>,

    /// v51 probe-on-first-use policy. See [`ProbeSection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeSection>,
}

/// One `[providers.<name>]` table. Every field is optional so a
/// half-populated profile can still merge cleanly with CLI overrides
/// (CLI supplies the missing pieces).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    /// Which BYOM adapter. Maps onto
    /// [`crate::adapter::Adapter`] impls in `atelier-core`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,

    /// Model id. By convention `<provider>:<model>` —
    /// `anthropic:claude-opus-4-7`, `local:qwen2.5-coder:7b`,
    /// `openai:gpt-4o-mini`. Sent verbatim on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Base URL for `openai-compat` only. Full URL ending in `/v1`,
    /// e.g. `http://localhost:11434/v1` for Ollama. Cross-section
    /// validation rejects this combined with `provider = "anthropic"`
    /// or `"mock"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Adapter discriminator. Same values as v52 but the field renamed
/// from `kind` → `provider` to match the v53 example.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// In-tree `MockAdapter`. No network. Default if nothing else
    /// resolves.
    Mock,
    /// Anthropic Messages API. Reads `ANTHROPIC_API_KEY`.
    Anthropic,
    /// Any `POST <base_url>/chat/completions` server: LM Studio,
    /// llama-server, vLLM, sglang, Ollama (via `/v1/`), or OpenAI
    /// itself. Reads `OPENAI_API_KEY` (optional — empty allowed).
    OpenaiCompat,
}

impl ProviderKind {
    /// Stable label for log lines + the UI status line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Anthropic => "anthropic",
            Self::OpenaiCompat => "openai-compat",
        }
    }
}

/// Top-level `[runner]` section. Currently just `max_turns`; future
/// fields slot in here (sandbox profile override, DoD timeout
/// multiplier, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerSection {
    /// Bail after N turns without `claimed_done`. Built-in default 32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
}

/// Top-level `[probe]` section — v51 probe-on-first-use policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSection {
    /// Probe policy. Built-in default `auto` (cache-first; probe on
    /// miss for `openai-compat`; skip for `mock` + `anthropic`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ProbePolicyName>,
}

/// `[probe].policy` value. Maps onto `atelier_cli::runner::ProbePolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbePolicyName {
    /// Cache-first; probe on miss. Default for `openai-compat`.
    Auto,
    /// Never probe; use capability defaults.
    Skip,
    /// Re-probe even when cached.
    Force,
}

impl ProbePolicyName {
    /// Stable label for log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Skip => "skip",
            Self::Force => "force",
        }
    }
}

// ---------- loading + discovery ----------

/// Filename inside `.atelier/` (or `~/.atelier/`).
pub const CONFIG_FILE_NAME: &str = "providers.toml";

/// Directory under the repo root.
pub const PROJECT_CONFIG_DIR: &str = ".atelier";

/// Directory under `$HOME` (or the harness's user-scope root).
pub const USER_CONFIG_DIR: &str = ".atelier";

impl ProvidersConfig {
    /// Resolve the active config by consulting, in order:
    ///
    ///   1. `<repo_root>/.atelier/providers.toml`
    ///   2. `~/.atelier/providers.toml`
    ///
    /// Returns the first file that exists, parsed and validated, paired
    /// with its absolute path so callers can log which file was loaded.
    /// Missing both is `Ok(None)` — not an error, because a fresh repo
    /// with no config should still run on built-in defaults.
    ///
    /// A file that exists but fails to parse is fatal: silently
    /// ignoring a malformed config would let a typo (`max_turns =
    /// "32"` instead of `32`) silently shift the runtime to defaults.
    pub fn load(repo_root: &Path) -> Result<Option<LoadedConfig>, ConfigError> {
        Self::load_with_home(repo_root, home_dir().as_deref())
    }

    /// Variant of [`Self::load`] that takes an explicit home-dir
    /// override. Production callers go through [`Self::load`] (which
    /// reads `$HOME` / `%USERPROFILE%`); tests use this entry to pin
    /// a tempdir so they don't depend on the developer's
    /// `~/.atelier/providers.toml` state. `None` disables the user
    /// scope entirely (only the project file is consulted).
    pub fn load_with_home(
        repo_root: &Path,
        home_override: Option<&Path>,
    ) -> Result<Option<LoadedConfig>, ConfigError> {
        for path in Self::paths_searched_with_home(repo_root, home_override) {
            match Self::try_load_one(&path)? {
                Some(config) => return Ok(Some(LoadedConfig { path, config })),
                None => continue,
            }
        }
        Ok(None)
    }

    /// The paths [`Self::load`] would (or did) consult, in priority
    /// order (project first, user second). Useful for telling the user
    /// `no config found (searched: <paths>)` instead of silently
    /// running on defaults.
    pub fn paths_searched(repo_root: &Path) -> Vec<PathBuf> {
        Self::paths_searched_with_home(repo_root, home_dir().as_deref())
    }

    /// `paths_searched` with an explicit home override. See
    /// [`Self::load_with_home`].
    pub fn paths_searched_with_home(
        repo_root: &Path,
        home_override: Option<&Path>,
    ) -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(2);
        out.push(repo_root.join(PROJECT_CONFIG_DIR).join(CONFIG_FILE_NAME));
        if let Some(home) = home_override {
            out.push(home.join(USER_CONFIG_DIR).join(CONFIG_FILE_NAME));
        }
        out
    }

    /// Try to load a single file. `Ok(None)` means the file doesn't
    /// exist; `Ok(Some)` means it parsed cleanly; `Err` means it
    /// exists but is malformed.
    fn try_load_one(path: &Path) -> Result<Option<Self>, ConfigError> {
        // v60.37 A2 — cap at 1 MiB so a pathologically large providers.toml
        // (runaway model, hostile commit) can't OOM the agent at startup.
        let bytes =
            match crate::io_caps::read_capped_to_string(path, crate::io_caps::CAP_PROVIDERS_TOML) {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(ConfigError::Io {
                        path: path.to_path_buf(),
                        source: e,
                    });
                }
            };
        let parsed: Self = toml::from_str(&bytes).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.message().to_string(),
        })?;
        parsed.validate(path)?;
        Ok(Some(parsed))
    }

    /// Cross-section invariants that serde can't enforce on its own.
    /// Today:
    ///
    ///   - If `default` is set, it must reference an existing
    ///     `[providers.<name>]` table.
    ///   - Each profile's `base_url` requires `provider =
    ///     "openai-compat"`. Combining `base_url` with any other
    ///     adapter is a clear mistake (anthropic + a `base_url` does
    ///     nothing useful).
    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(name) = &self.default {
            if !self.providers.contains_key(name) {
                let available: Vec<&str> = self.providers.keys().map(String::as_str).collect();
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "default = {name:?} but no [providers.{name}] table exists. \
                         Available profiles: {available:?}"
                    ),
                });
            }
        }
        for (name, profile) in &self.providers {
            if profile.base_url.is_some()
                && profile.provider.is_some()
                && profile.provider != Some(ProviderKind::OpenaiCompat)
            {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "[providers.{name}].base_url is only valid when \
                         [providers.{name}].provider = \"openai-compat\" \
                         (got provider = {:?})",
                        profile.provider.unwrap().as_str()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Pick the active profile. Priority: explicit `cli_profile` name
    /// (from `--profile <NAME>`) > `self.default`. Returns `None` if
    /// neither resolves; returns an error if the named profile
    /// doesn't exist.
    ///
    /// This is the single entry point the CLI uses to bridge the
    /// file shape onto the field-by-field overrides the CLI then
    /// applies — keeping the resolution logic in one place means the
    /// override-precedence rules above stay true regardless of how
    /// many sites consult the file.
    pub fn resolve_profile<'a>(
        &'a self,
        cli_profile: Option<&'a str>,
    ) -> Result<Option<(&'a str, &'a ProviderProfile)>, ConfigError> {
        let name = cli_profile.or(self.default.as_deref());
        let Some(name) = name else {
            return Ok(None);
        };
        match self.providers.get(name) {
            Some(p) => Ok(Some((name, p))),
            None => {
                let available: Vec<&str> = self.providers.keys().map(String::as_str).collect();
                Err(ConfigError::Invalid {
                    path: PathBuf::new(),
                    message: format!(
                        "profile {name:?} requested but not defined in providers.toml. \
                         Available profiles: {available:?}"
                    ),
                })
            }
        }
    }
}

/// What [`ProvidersConfig::load`] returns: the parsed config plus the
/// absolute path it came from, so the binary can print
/// `atelier run: using config <path>` and the user can confirm which
/// file is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: ProvidersConfig,
}

/// Errors from [`ProvidersConfig::load`] and
/// [`ProvidersConfig::resolve_profile`]. The caller usually surfaces
/// these as `atelier run: config error: …` and exits with code 2 (bad
/// invocation) rather than 1 (runtime failure) — a malformed config or
/// a missing profile is a user-fixable input, not a transient runtime
/// problem.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O failure reading config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("config at {path} is not valid TOML: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("config at {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write `body` to `<dir>/.atelier/providers.toml` and return the
    /// repo root.
    fn write_project_config(dir: &Path, body: &str) {
        let project_dir = dir.join(PROJECT_CONFIG_DIR);
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join(CONFIG_FILE_NAME), body).unwrap();
    }

    // ---------- shape ----------

    #[test]
    fn empty_config_parses_to_all_default() {
        let parsed: ProvidersConfig = toml::from_str("").unwrap();
        assert!(parsed.default.is_none());
        assert!(parsed.providers.is_empty());
        assert!(parsed.runner.is_none());
        assert!(parsed.probe.is_none());
    }

    #[test]
    fn user_example_parses() {
        let body = r#"
default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"

[providers.cloud]
provider = "anthropic"
model    = "anthropic:claude-opus-4-7"
"#;
        let parsed: ProvidersConfig = toml::from_str(body).unwrap();
        assert_eq!(parsed.default.as_deref(), Some("local"));
        assert_eq!(parsed.providers.len(), 2);

        let local = parsed.providers.get("local").unwrap();
        assert_eq!(local.provider, Some(ProviderKind::OpenaiCompat));
        assert_eq!(local.base_url.as_deref(), Some("http://localhost:11434/v1"));
        assert_eq!(local.model.as_deref(), Some("local:qwen2.5-coder:7b"));

        let cloud = parsed.providers.get("cloud").unwrap();
        assert_eq!(cloud.provider, Some(ProviderKind::Anthropic));
        assert_eq!(cloud.model.as_deref(), Some("anthropic:claude-opus-4-7"));
        assert!(cloud.base_url.is_none());
    }

    #[test]
    fn provider_kind_kebab_on_wire() {
        let parsed: ProvidersConfig =
            toml::from_str("[providers.x]\nprovider = \"openai-compat\"\n").unwrap();
        assert_eq!(
            parsed.providers.get("x").unwrap().provider,
            Some(ProviderKind::OpenaiCompat)
        );
    }

    #[test]
    fn probe_policy_lowercase() {
        for (literal, expected) in [
            ("auto", ProbePolicyName::Auto),
            ("skip", ProbePolicyName::Skip),
            ("force", ProbePolicyName::Force),
        ] {
            let body = format!("[probe]\npolicy = \"{literal}\"\n");
            let parsed: ProvidersConfig = toml::from_str(&body).unwrap();
            assert_eq!(parsed.probe.unwrap().policy, Some(expected));
        }
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let err = toml::from_str::<ProvidersConfig>("garbage = 1\n").unwrap_err();
        assert!(
            err.message().contains("unknown") || err.message().contains("garbage"),
            "got {}",
            err.message()
        );
    }

    #[test]
    fn unknown_profile_field_is_rejected() {
        let body = "[providers.x]\nweird = 1\n";
        let err = toml::from_str::<ProvidersConfig>(body).unwrap_err();
        assert!(err.message().contains("unknown") || err.message().contains("weird"));
    }

    #[test]
    fn provider_kind_label_round_trips() {
        assert_eq!(ProviderKind::Mock.as_str(), "mock");
        assert_eq!(ProviderKind::Anthropic.as_str(), "anthropic");
        assert_eq!(ProviderKind::OpenaiCompat.as_str(), "openai-compat");
    }

    // ---------- discovery + load ----------

    #[test]
    fn paths_searched_lists_project_then_user() {
        let tmp = TempDir::new().unwrap();
        let paths = ProvidersConfig::paths_searched(tmp.path());
        assert!(paths[0].ends_with(".atelier/providers.toml"));
        assert!(paths[0].starts_with(tmp.path()));
    }

    #[test]
    fn load_returns_none_when_no_config_exists() {
        let tmp_home = TempDir::new().unwrap();
        let tmp_repo = TempDir::new().unwrap();
        let prior = std::env::var_os("HOME");
        // SAFETY: per-process env mutation. The set/restore pattern
        // protects parallel tests as long as no other test writes to
        // HOME simultaneously — this is the only test that does.
        unsafe {
            std::env::set_var("HOME", tmp_home.path());
        }
        let loaded = ProvidersConfig::load(tmp_repo.path()).unwrap();
        unsafe {
            match prior {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(loaded.is_none());
    }

    #[test]
    fn load_picks_up_project_config_when_present() {
        let tmp_repo = TempDir::new().unwrap();
        write_project_config(
            tmp_repo.path(),
            r#"
default = "local"

[providers.local]
provider = "openai-compat"
base_url = "http://localhost:11434/v1"
model    = "local:qwen2.5-coder:7b"

[runner]
max_turns = 16

[probe]
policy = "skip"
"#,
        );
        let loaded = ProvidersConfig::load(tmp_repo.path()).unwrap().unwrap();
        assert!(loaded.path.ends_with(".atelier/providers.toml"));
        assert_eq!(loaded.config.default.as_deref(), Some("local"));
        assert_eq!(loaded.config.providers.len(), 1);
        assert_eq!(loaded.config.runner.unwrap().max_turns, Some(16));
        assert_eq!(
            loaded.config.probe.unwrap().policy,
            Some(ProbePolicyName::Skip)
        );
    }

    #[test]
    fn malformed_config_is_a_parse_error() {
        let tmp_repo = TempDir::new().unwrap();
        write_project_config(tmp_repo.path(), "[runner]\nmax_turns = \"thirty-two\"\n");
        let err = ProvidersConfig::load(tmp_repo.path()).unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => {
                assert!(path.ends_with(".atelier/providers.toml"));
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    // ---------- validation ----------

    #[test]
    fn default_must_reference_an_existing_profile() {
        let tmp_repo = TempDir::new().unwrap();
        write_project_config(
            tmp_repo.path(),
            "default = \"ghost\"\n\n[providers.local]\nprovider = \"mock\"\n",
        );
        let err = ProvidersConfig::load(tmp_repo.path()).unwrap_err();
        match err {
            ConfigError::Invalid { message, .. } => {
                assert!(message.contains("ghost"));
                assert!(message.contains("Available"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn base_url_with_non_openai_compat_provider_is_invalid() {
        let tmp_repo = TempDir::new().unwrap();
        write_project_config(
            tmp_repo.path(),
            r#"
[providers.cloud]
provider = "anthropic"
base_url = "https://api.anthropic.com/v1"
"#,
        );
        let err = ProvidersConfig::load(tmp_repo.path()).unwrap_err();
        match err {
            ConfigError::Invalid { message, .. } => {
                assert!(message.contains("base_url"));
                assert!(message.contains("openai-compat"));
                assert!(message.contains("cloud"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn base_url_without_provider_is_allowed() {
        let tmp_repo = TempDir::new().unwrap();
        write_project_config(
            tmp_repo.path(),
            "[providers.x]\nbase_url = \"http://x/v1\"\n",
        );
        let loaded = ProvidersConfig::load(tmp_repo.path()).unwrap().unwrap();
        let x = loaded.config.providers.get("x").unwrap();
        assert_eq!(x.base_url.as_deref(), Some("http://x/v1"));
        assert!(x.provider.is_none());
    }

    // ---------- profile resolution ----------

    fn config_with_two_profiles() -> ProvidersConfig {
        let mut providers = BTreeMap::new();
        providers.insert(
            "local".to_string(),
            ProviderProfile {
                provider: Some(ProviderKind::OpenaiCompat),
                model: Some("local:m".into()),
                base_url: Some("http://x/v1".into()),
            },
        );
        providers.insert(
            "cloud".to_string(),
            ProviderProfile {
                provider: Some(ProviderKind::Anthropic),
                model: Some("anthropic:m".into()),
                base_url: None,
            },
        );
        ProvidersConfig {
            default: Some("local".into()),
            providers,
            runner: None,
            probe: None,
        }
    }

    #[test]
    fn resolve_profile_prefers_cli_over_default() {
        let cfg = config_with_two_profiles();
        let (name, profile) = cfg.resolve_profile(Some("cloud")).unwrap().unwrap();
        assert_eq!(name, "cloud");
        assert_eq!(profile.provider, Some(ProviderKind::Anthropic));
    }

    #[test]
    fn resolve_profile_falls_back_to_default() {
        let cfg = config_with_two_profiles();
        let (name, profile) = cfg.resolve_profile(None).unwrap().unwrap();
        assert_eq!(name, "local");
        assert_eq!(profile.provider, Some(ProviderKind::OpenaiCompat));
    }

    #[test]
    fn resolve_profile_returns_none_without_cli_or_default() {
        let mut cfg = config_with_two_profiles();
        cfg.default = None;
        assert!(cfg.resolve_profile(None).unwrap().is_none());
    }

    #[test]
    fn resolve_profile_errors_on_missing_name() {
        let cfg = config_with_two_profiles();
        let err = cfg.resolve_profile(Some("ghost")).unwrap_err();
        match err {
            ConfigError::Invalid { message, .. } => {
                assert!(message.contains("ghost"));
                assert!(message.contains("local") || message.contains("Available"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_through_serde() {
        let cfg = config_with_two_profiles();
        let text = toml::to_string(&cfg).unwrap();
        let parsed: ProvidersConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);
    }
}
