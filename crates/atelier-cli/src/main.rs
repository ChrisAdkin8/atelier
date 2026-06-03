//! `atelier` — command-line entry point.
//!
//! Subcommands:
//!   * `init` (spec §11 project bootstrap).
//!   * `run`  (Phase C unblock (1) — drive a turn against the configured
//!     adapter; today only `--provider mock` is wired, but the runner is
//!     adapter-agnostic so the §1 Anthropic adapter slots in unchanged).
//!
//! Future subcommands (per spec §11 credential storage): `login`,
//! `logout`, `rotate`, `whoami`.

// v47: `runner` now lives in the crate's library (see `src/lib.rs`).
// Import the binary's view via the library name.
use atelier_cli::overhead;
use atelier_cli::runner;
use atelier_core::Adapter;

use atelier_core::config::{
    LoadedConfig, ProbePolicyName, ProviderKind, ProviderProfile, ProvidersConfig,
};

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
atelier — coding harness CLI

USAGE:
    atelier <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS:
    init [PATH]                    Bootstrap an Atelier project at PATH
                                   (default: current dir). Idempotent. Spec §11.
    run [OPTIONS] [PROMPT]         Drive a turn against the configured adapter,
                                   loop until claimed_done, run DoD checks,
                                   persist the session. Phase C unblock (1).
    protocol-overhead [OPTIONS]    Measure §2 emission-strategy overhead against
                                   the scripted MockAdapter fixtures, write the
                                   result to tests/protocol/overhead.json, and
                                   (optionally) flag >10% drift vs the rolling
                                   median. Backs the nightly CI job.
    find [OPTIONS]                 Query the most recent (or named) session for
                                   what the agent knows about a given file path.
                                   Appends a FindProbe to the session's
                                   `find_probes.json` so the §5 UX target's
                                   median-elapsed-ms can be computed. Exits 0
                                   cleanly when no session exists yet.
    providers <VERB> [ARGS]         Manage provider credentials and test
                                   configured provider profiles.

`atelier run` may read defaults from a TOML config (v53):

    <repo>/.atelier/providers.toml    project scope (preferred)
    ~/.atelier/providers.toml         user scope (fallback)

If both exist, the project file wins. The file declares named profiles
under [providers.<name>] tables; `default = \"<name>\"` picks one;
`--profile <NAME>` on the CLI overrides the default. Per-field flags
below still override individual fields of the resolved profile.

Layering, top wins: CLI flags > resolved profile > built-in defaults
(provider=mock, max-turns=32, probe=auto). On invocation the binary
prints `atelier run: using config <path> [profile <NAME>]` so it is
visible which file (and profile within it) is active.

Example `.atelier/providers.toml`:

    default = \"local\"

    [providers.local]
    provider = \"openai-compat\"
    base_url = \"http://localhost:11434/v1\"
    model    = \"local:qwen2.5-coder:7b\"

    [providers.cloud]
    provider = \"anthropic\"
    model    = \"anthropic:claude-opus-4-7\"

    [runner]
    max_turns = 32

    [probe]
    policy = \"auto\"

`atelier run` options:
    --profile <NAME>               Select a named profile from
                                   providers.toml (overrides `default`).
                                   Errors if the name isn't present
                                   in the file.
    --provider <NAME>              Adapter to use. One of:
                                     mock          (default) — no network.
                                     anthropic     — Messages API (`ANTHROPIC_API_KEY`).
                                     openai-compat — any server speaking
                                                     `POST /v1/chat/completions`
                                                     (LM Studio, llama-server,
                                                     vLLM, sglang, Ollama via
                                                     its `/v1/` compat layer,
                                                     OpenAI itself).
    --model <ID>                   Model id for the chosen provider, e.g.
                                   `anthropic:claude-opus-4-7` or
                                   `local:llama3:8b`. Required for the
                                   network providers; ignored for `mock`.
    --base-url <URL>               openai-compat only: full URL ending in
                                   `/v1` — e.g. `http://localhost:11434/v1`
                                   (Ollama), `http://localhost:1234/v1`
                                   (LM Studio), `http://localhost:8080/v1`
                                   (llama.cpp server). For openai-compat
                                   pointing at OpenAI itself, omit to use
                                   `https://api.openai.com/v1` and set
                                   `OPENAI_API_KEY`.
    --workspace <PATH>             Repo root. Defaults to current dir.
    --max-turns <N>                Bail after N turns without claimed_done.
                                   Default 32 (PROVISIONAL).
    --prompt-file <PATH>           Read the prompt from PATH instead of argv.
                                   Use `-` for stdin.
    --no-probe                     Skip the v51 probe-on-first-use
                                   calibration. Falls back to a default
                                   strategy from `Adapter::capabilities()`.
                                   The §1 conformance tracker still
                                   degrades at runtime if the model
                                   misbehaves. Useful when running
                                   offline or against a server you
                                   know is fine.
    --force-probe                  Re-probe even when a cached profile
                                   is present. Overwrites the cache
                                   entry on success. `--no-probe` and
                                   `--force-probe` are mutually
                                   exclusive.
    --non-interactive              Headless mode (§14). Auto-approves
                                   every staged write and auto-reloads
                                   on concurrent edits — no modals are
                                   shown. Use for CI / scripted runs.
    --resume <SESSION-UUID>        Resume a previously-persisted session
                                   (§14). Reads .atelier/sessions/<uuid>/
                                   session.json and replays the
                                   conversation prefix up to the last
                                   completed tool round-trip. The
                                   prompt is appended as a fresh user
                                   turn (omit it to resume without
                                   adding one).

OPTIONS:
    -h, --help     Print this message.
    -V, --version  Print the version.
";

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(subcommand) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    match subcommand.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "-V" | "--version" => {
            println!("atelier {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "init" => run_init(args),
        "run" => run_run(args),
        "protocol-overhead" => run_protocol_overhead(args),
        "find" => run_find(args),
        "providers" => run_providers(args),
        "skills" => run_skills(args),
        other => {
            eprintln!("atelier: unknown subcommand `{other}`\n");
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run_init(mut args: impl Iterator<Item = String>) -> ExitCode {
    let repo_root = match args.next() {
        Some(arg) if arg == "-h" || arg == "--help" => {
            println!("atelier init [PATH]\n\nBootstrap an Atelier project at PATH (default: current dir). Idempotent.");
            return ExitCode::SUCCESS;
        }
        Some(path) => PathBuf::from(path),
        None => match env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("atelier init: cannot read current directory: {e}");
                return ExitCode::from(1);
            }
        },
    };

    match atelier_core::init(&repo_root) {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atelier init: {e}");
            ExitCode::from(1)
        }
    }
}

// ---------- `atelier providers` ----------

const PROVIDERS_USAGE: &str = "atelier providers <VERB> [ARGS]\n\
\n\
VERBs:\n\
    auth <profile> [--from-command <CMD>|--from-stdin|--env <NAME>] [--workspace <PATH>] [--no-update]\n\
                  Store a provider API key in the OS keychain and, by default,\n\
                  add/update `[providers.<profile>].api_key` in providers.toml.\n\
    test <profile> [--workspace <PATH>]\n\
                  Resolve the profile credential and call the OpenAI-compatible\n\
                  `/models` endpoint without printing the secret.\n\
    score <profile> [--workspace <PATH>] [--force-probe] [--json]\n\
                  Probe/cache the profile and score the model's suitability\n\
                  for Atelier tool-using agent runs.\n";

fn run_providers(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("-h" | "--help") => {
            print!("{PROVIDERS_USAGE}");
            ExitCode::SUCCESS
        }
        Some("auth") => providers_auth(args),
        Some("test") => providers_test(args),
        Some("score") => providers_score(args),
        Some(other) => {
            eprintln!("atelier providers: unknown verb `{other}`\n");
            eprintln!("{PROVIDERS_USAGE}");
            ExitCode::from(2)
        }
        None => {
            eprintln!("{PROVIDERS_USAGE}");
            ExitCode::from(2)
        }
    }
}

enum ProviderSecretSource {
    Command(String),
    Stdin,
    Env(String),
}

fn providers_auth(args: impl Iterator<Item = String>) -> ExitCode {
    let mut profile = None;
    let mut source = None;
    let mut workspace = None;
    let mut update_config = true;
    let mut iter = args;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{PROVIDERS_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--from-command" => match iter.next() {
                Some(cmd) => source = Some(ProviderSecretSource::Command(cmd)),
                None => {
                    eprintln!("atelier providers auth: --from-command requires a command string");
                    return ExitCode::from(2);
                }
            },
            "--from-stdin" => source = Some(ProviderSecretSource::Stdin),
            "--env" => match iter.next() {
                Some(name) => source = Some(ProviderSecretSource::Env(name)),
                None => {
                    eprintln!("atelier providers auth: --env requires a variable name");
                    return ExitCode::from(2);
                }
            },
            "--workspace" => match iter.next() {
                Some(path) => workspace = Some(PathBuf::from(path)),
                None => {
                    eprintln!("atelier providers auth: --workspace requires a path");
                    return ExitCode::from(2);
                }
            },
            "--no-update" => update_config = false,
            other if other.starts_with('-') => {
                eprintln!("atelier providers auth: unknown option `{other}`");
                return ExitCode::from(2);
            }
            other => {
                if profile.replace(other.to_string()).is_some() {
                    eprintln!("atelier providers auth: only one profile name is accepted");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let Some(profile) = profile else {
        eprintln!("atelier providers auth: <profile> is required");
        return ExitCode::from(2);
    };
    let Some(source) = source else {
        eprintln!("atelier providers auth: choose --from-command, --from-stdin, or --env");
        return ExitCode::from(2);
    };
    let workspace = match workspace.or_else(|| env::current_dir().ok()) {
        Some(path) => path,
        None => {
            eprintln!("atelier providers auth: cannot determine current directory");
            return ExitCode::from(1);
        }
    };
    let loaded = match ProvidersConfig::load(&workspace) {
        Ok(Some(loaded)) => loaded,
        Ok(None) => {
            eprintln!(
                "atelier providers auth: no providers.toml found for {}",
                workspace.display()
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("atelier providers auth: config error: {e}");
            return ExitCode::from(2);
        }
    };
    let Some(profile_config) = loaded.config.providers.get(&profile) else {
        eprintln!(
            "atelier providers auth: profile {profile:?} not found in {}",
            loaded.path.display()
        );
        return ExitCode::from(2);
    };
    let key_ref = profile_config
        .api_key
        .clone()
        .unwrap_or_else(|| atelier_core::default_provider_api_key_ref(&profile));
    if !key_ref.starts_with("keyring:") {
        eprintln!("atelier providers auth: profile {profile:?} has api_key = {key_ref:?}; auth can only write keyring: references");
        return ExitCode::from(2);
    }
    let secret = match read_provider_secret(source) {
        Ok(secret) => secret,
        Err(e) => {
            eprintln!("atelier providers auth: {e}");
            return ExitCode::from(1);
        }
    };
    if secret.is_empty() {
        eprintln!("atelier providers auth: secret source produced an empty value");
        return ExitCode::from(1);
    }
    if let Err(e) = atelier_core::store_api_key_ref(&key_ref, &secret) {
        eprintln!("atelier providers auth: {e}");
        return ExitCode::from(1);
    }
    if update_config {
        if let Err(e) = upsert_profile_api_key(&loaded.path, &profile, &key_ref) {
            eprintln!(
                "atelier providers auth: stored key, but failed to update {}: {e}",
                loaded.path.display()
            );
            return ExitCode::from(1);
        }
    }
    println!("atelier providers auth: stored credential for profile {profile:?} in {key_ref}");
    if !update_config {
        println!("Add this to [providers.{profile}]: api_key = {key_ref:?}");
    }
    ExitCode::SUCCESS
}

fn providers_test(args: impl Iterator<Item = String>) -> ExitCode {
    let mut profile = None;
    let mut workspace = None;
    let mut iter = args;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{PROVIDERS_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--workspace" => match iter.next() {
                Some(path) => workspace = Some(PathBuf::from(path)),
                None => {
                    eprintln!("atelier providers test: --workspace requires a path");
                    return ExitCode::from(2);
                }
            },
            other if other.starts_with('-') => {
                eprintln!("atelier providers test: unknown option `{other}`");
                return ExitCode::from(2);
            }
            other => {
                if profile.replace(other.to_string()).is_some() {
                    eprintln!("atelier providers test: only one profile name is accepted");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let Some(profile) = profile else {
        eprintln!("atelier providers test: <profile> is required");
        return ExitCode::from(2);
    };
    let workspace = match workspace.or_else(|| env::current_dir().ok()) {
        Some(path) => path,
        None => {
            eprintln!("atelier providers test: cannot determine current directory");
            return ExitCode::from(1);
        }
    };
    let loaded = match ProvidersConfig::load(&workspace) {
        Ok(Some(loaded)) => loaded,
        Ok(None) => {
            eprintln!(
                "atelier providers test: no providers.toml found for {}",
                workspace.display()
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("atelier providers test: config error: {e}");
            return ExitCode::from(2);
        }
    };
    let Some(profile_config) = loaded.config.providers.get(&profile) else {
        eprintln!(
            "atelier providers test: profile {profile:?} not found in {}",
            loaded.path.display()
        );
        return ExitCode::from(2);
    };
    if profile_config.provider != Some(ProviderKind::OpenaiCompat) {
        eprintln!(
            "atelier providers test: profile {profile:?} is not provider = \"openai-compat\""
        );
        return ExitCode::from(2);
    }
    let api_key = match atelier_core::resolve_openai_api_key(profile_config.api_key.as_deref()) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("atelier providers test: {e}");
            return ExitCode::from(1);
        }
    };
    let base = effective_openai_base_url(profile_config.base_url.clone());
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("atelier providers test: tokio runtime build failed: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(test_openai_compat_models(&base, &api_key)) {
        Ok(model_count) => {
            println!(
                "atelier providers test: profile {profile:?} reached {base}/models ({model_count} model(s))"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atelier providers test: {e}");
            ExitCode::from(1)
        }
    }
}

fn providers_score(args: impl Iterator<Item = String>) -> ExitCode {
    let mut profile = None;
    let mut workspace = None;
    let mut force_probe = false;
    let mut json = false;
    let mut iter = args;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{PROVIDERS_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--workspace" => match iter.next() {
                Some(path) => workspace = Some(PathBuf::from(path)),
                None => {
                    eprintln!("atelier providers score: --workspace requires a path");
                    return ExitCode::from(2);
                }
            },
            "--force-probe" => force_probe = true,
            "--json" => json = true,
            other if other.starts_with('-') => {
                eprintln!("atelier providers score: unknown option `{other}`");
                return ExitCode::from(2);
            }
            other => {
                if profile.replace(other.to_string()).is_some() {
                    eprintln!("atelier providers score: only one profile name is accepted");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let Some(profile_name) = profile else {
        eprintln!("atelier providers score: <profile> is required");
        return ExitCode::from(2);
    };
    let workspace = match workspace.or_else(|| env::current_dir().ok()) {
        Some(path) => path,
        None => {
            eprintln!("atelier providers score: cannot determine current directory");
            return ExitCode::from(1);
        }
    };
    let loaded = match ProvidersConfig::load(&workspace) {
        Ok(Some(loaded)) => loaded,
        Ok(None) => {
            eprintln!(
                "atelier providers score: no providers.toml found for {}",
                workspace.display()
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("atelier providers score: config error: {e}");
            return ExitCode::from(2);
        }
    };
    let Some(profile_config) = loaded.config.providers.get(&profile_name) else {
        eprintln!(
            "atelier providers score: profile {profile_name:?} not found in {}",
            loaded.path.display()
        );
        return ExitCode::from(2);
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("atelier providers score: tokio runtime build failed: {e}");
            return ExitCode::from(1);
        }
    };

    match rt.block_on(score_provider_profile(
        &profile_name,
        profile_config,
        force_probe,
    )) {
        Ok((profile, outcome, row, suitability)) => {
            if json {
                let value = serde_json::json!({
                    "profile": profile_name,
                    "probe_outcome": outcome.wire_label(),
                    "model_profile": profile,
                    "capability_matrix": row,
                    "suitability": suitability,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
                );
            } else {
                print_suitability_report(&profile_name, outcome, &row, &suitability);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atelier providers score: {e}");
            ExitCode::from(1)
        }
    }
}

async fn score_provider_profile(
    profile_name: &str,
    profile: &ProviderProfile,
    force_probe: bool,
) -> Result<ProviderScore, String> {
    let (adapter, base_url, force_profile) = build_scoring_adapter(profile_name, profile)?;
    let caps = adapter.capabilities();
    let (model_profile, outcome) = if let Some(profile) = force_profile {
        (
            profile,
            atelier_core::adapter::model_profile::ProbeLoadOutcome::NotCached,
        )
    } else {
        atelier_core::adapter::model_profile::ProfileStore::user_default()
            .load_or_probe(
                adapter.as_ref(),
                &base_url,
                force_probe,
                atelier_core::time::now_rfc3339(),
            )
            .await
            .map_err(|e| e.to_string())?
    };
    let base_row =
        atelier_core::adapter::capability_matrix::matrix_row_for(adapter.model_id(), &caps);
    let row =
        atelier_core::adapter::capability_matrix::crosswalk_with_profile(base_row, &model_profile);
    let suitability =
        atelier_core::adapter::model_profile::score_model_suitability(&model_profile, &row);
    Ok((model_profile, outcome, row, suitability))
}

type ProviderScore = (
    atelier_core::adapter::model_profile::ModelProfile,
    atelier_core::adapter::model_profile::ProbeLoadOutcome,
    atelier_core::adapter::capability_matrix::CapabilityMatrixRow,
    atelier_core::adapter::model_profile::ModelSuitability,
);

type ScoringAdapterParts = (
    std::sync::Arc<dyn Adapter>,
    String,
    Option<atelier_core::adapter::model_profile::ModelProfile>,
);

fn build_scoring_adapter(
    profile_name: &str,
    profile: &ProviderProfile,
) -> Result<ScoringAdapterParts, String> {
    let kind = profile
        .provider
        .ok_or_else(|| format!("profile {profile_name:?} has no `provider` field"))?;
    let model_id = profile.model.clone().unwrap_or_else(|| match kind {
        ProviderKind::Mock => "mock:default".to_string(),
        ProviderKind::Anthropic => "anthropic:claude-opus-4-7".to_string(),
        ProviderKind::OpenaiCompat => String::new(),
    });
    if model_id.is_empty() {
        return Err(format!(
            "profile {profile_name:?}: provider `openai-compat` requires a `model`"
        ));
    }

    match kind {
        ProviderKind::Mock => {
            let adapter =
                std::sync::Arc::new(atelier_core::adapter::MockAdapter::new(model_id.clone()));
            let profile =
                atelier_core::adapter::model_profile::ModelProfile::skipped_for_well_known(
                    model_id,
                    atelier_core::protocol_strategy::Strategy::NativeTool,
                    adapter.capabilities().context_window_tokens,
                    atelier_core::adapter::model_profile::DEFAULT_PROFILE_MAX_TOKENS,
                    atelier_core::time::now_rfc3339(),
                );
            Ok((adapter, String::new(), Some(profile)))
        }
        ProviderKind::Anthropic => {
            if !model_id.starts_with("anthropic:") {
                return Err(format!(
                    "profile {profile_name:?}: anthropic model must be prefixed `anthropic:`"
                ));
            }
            let adapter = atelier_core::adapter::anthropic::AnthropicAdapter::from_env(model_id)
                .map_err(|e| e.to_string())?;
            Ok((std::sync::Arc::new(adapter), String::new(), None))
        }
        ProviderKind::OpenaiCompat => {
            atelier_core::provider_profile_base_url_may_receive_credential(
                profile.base_url.as_deref(),
                false,
                atelier_core::api_key_ref_may_resolve(
                    profile.api_key.as_deref(),
                    atelier_core::OPENAI_API_KEY_ENV,
                ),
            )?;
            let base_url = effective_openai_base_url(profile.base_url.clone());
            if !preflight_base_url(&base_url) {
                return Err(format!(
                    "profile {profile_name:?}: server at {base_url:?} is unreachable \
                     (TCP connect timed out)"
                ));
            }
            let api_key = atelier_core::resolve_openai_api_key(profile.api_key.as_deref())
                .map_err(|e| format!("provider credential: {e}"))?;
            let mut adapter = atelier_core::adapter::openai_compat::OpenAiCompatAdapter::new(
                api_key,
                model_id,
                base_url.clone(),
            );
            if profile.cache_prompt.unwrap_or(false) {
                adapter = adapter.with_cache_prompt(true);
            }
            Ok((std::sync::Arc::new(adapter), base_url, None))
        }
    }
}

fn print_suitability_report(
    profile_name: &str,
    outcome: atelier_core::adapter::model_profile::ProbeLoadOutcome,
    row: &atelier_core::adapter::capability_matrix::CapabilityMatrixRow,
    suitability: &atelier_core::adapter::model_profile::ModelSuitability,
) {
    println!(
        "atelier providers score: profile {profile_name:?} model {} scored {}/100 ({})",
        suitability.model_id,
        suitability.score,
        suitability.grade.wire_label()
    );
    println!("probe outcome: {}", outcome.wire_label());
    println!("matrix source: {}", row.source.wire_label());
    println!("recommendation: {}", suitability.recommendation);
    println!();
    println!("factors:");
    for factor in &suitability.factors {
        println!(
            "  - {:<18} {:>2}/{:<2} {}",
            factor.name, factor.awarded, factor.max, factor.detail
        );
    }
    if !suitability.strengths.is_empty() {
        println!();
        println!("strengths:");
        for strength in &suitability.strengths {
            println!("  - {strength}");
        }
    }
    if !suitability.risks.is_empty() {
        println!();
        println!("risks:");
        for risk in &suitability.risks {
            println!("  - {risk}");
        }
    }
}

fn read_provider_secret(source: ProviderSecretSource) -> Result<String, String> {
    let raw = match source {
        ProviderSecretSource::Command(cmd) => run_secret_command(&cmd)?,
        ProviderSecretSource::Stdin => {
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .map_err(|e| format!("read stdin: {e}"))?;
            input
        }
        ProviderSecretSource::Env(name) => {
            env::var(&name).map_err(|_| format!("environment variable {name} is not set"))?
        }
    };
    Ok(raw.trim_end_matches(['\r', '\n']).to_string())
}

fn run_secret_command(cmd: &str) -> Result<String, String> {
    #[cfg(unix)]
    let output = Command::new("/bin/sh").arg("-c").arg(cmd).output();
    #[cfg(windows)]
    let output = Command::new("cmd").arg("/C").arg(cmd).output();
    let output = output.map_err(|e| format!("run --from-command: {e}"))?;
    if !output.status.success() {
        return Err(format!("--from-command exited with {}", output.status));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| format!("--from-command output was not UTF-8: {e}"))
}

async fn test_openai_compat_models(base_url: &str, api_key: &str) -> Result<usize, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let mut req = client.get(&url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let res = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|e| format!("read response body: {e}"))?;
    if !status.is_success() {
        return Err(format!("GET {url} returned HTTP {status}: {body}"));
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse /models response: {e}"))?;
    Ok(value
        .get("data")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0))
}

fn effective_openai_base_url(base_url: Option<String>) -> String {
    base_url
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
}

fn upsert_profile_api_key(path: &Path, profile: &str, key_ref: &str) -> io::Result<()> {
    let body = fs::read_to_string(path)?;
    let mut lines: Vec<String> = body.lines().map(ToString::to_string).collect();
    let had_trailing_newline = body.ends_with('\n');
    let Some(start) = lines
        .iter()
        .position(|line| provider_header_matches(line, profile))
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("profile {profile:?} not found"),
        ));
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(idx, line)| {
            let trimmed = line.trim();
            (trimmed.starts_with('[') && trimmed.ends_with(']')).then_some(idx)
        })
        .unwrap_or(lines.len());
    let replacement = format!("api_key = \"{}\"", toml_escape_string(key_ref));
    if let Some(idx) = lines[start + 1..end]
        .iter()
        .position(|line| line.trim_start().starts_with("api_key"))
        .map(|rel| start + 1 + rel)
    {
        lines[idx] = replacement;
    } else {
        lines.insert(end, replacement);
    }
    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    fs::write(path, out)
}

fn provider_header_matches(line: &str, profile: &str) -> bool {
    let trimmed = line.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    let Some(name) = inner.strip_prefix("providers.") else {
        return false;
    };
    name == profile
        || (name.starts_with('"') && name.ends_with('"') && &name[1..name.len() - 1] == profile)
}

fn toml_escape_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------- `atelier skills` ----------
//
// Catalogue inspection + user-authoring verbs for §15 skills.
//
//   `atelier skills`              — print the resolved catalogue in
//                                   spec `/help` format.
//   `atelier skills new <name>`   — scaffold a new manifest at the
//                                   right scope.
//   `atelier skills validate`     — lint manifests without running them.
//   `atelier skills edit <name>`  — open the resolved manifest in $EDITOR.
//   `atelier skills delete <name>`— remove a user/repo-scope manifest.
//   `atelier skills show <name>`  — print the resolved manifest + source.

const SKILLS_USAGE: &str = "atelier skills [VERB] [ARGS]\n\
\n\
With no VERB, prints the registered skill catalogue (bundled + \
~/.atelier/skills/ + <repo>/.atelier/skills/) in `/help` format.\n\
\n\
VERBs:\n\
    new <name> [--scope user|repo] [--from <name>]\n\
                  Scaffold a starter manifest. --scope user writes to\n\
                  ~/.atelier/skills/; --scope repo (default) writes to\n\
                  <workspace>/.atelier/skills/. --from <existing> seeds\n\
                  the body from an already-registered skill.\n\
    validate [path]\n\
                  Lint a manifest (or every manifest in the registry\n\
                  when no path is given). Exits non-zero on any failure.\n\
    edit <name>   Resolve <name> through the registry and open the\n\
                  winning manifest in $EDITOR. Refuses to edit a bundled\n\
                  manifest in place — use `new --from <name>` instead.\n\
    delete <name> Remove a user- or per-repo-scope manifest.\n\
    show <name>   Print the resolved manifest + its source path.\n";

fn run_skills(mut args: impl Iterator<Item = String>) -> ExitCode {
    let verb = args.next();
    match verb.as_deref() {
        Some("-h" | "--help") => {
            print!("{SKILLS_USAGE}");
            ExitCode::SUCCESS
        }
        Some("new") => skills::run_new(args),
        Some("validate") => skills::run_validate(args),
        Some("edit") => skills::run_edit(args),
        Some("delete") => skills::run_delete(args),
        Some("show") => skills::run_show(args),
        Some(unknown) => {
            eprintln!("atelier skills: unknown verb `{unknown}`\n");
            eprintln!("{SKILLS_USAGE}");
            ExitCode::from(2)
        }
        None => skills::run_list(),
    }
}

mod skills {
    //! `atelier skills` subcommand implementations.
    //!
    //! All verbs share the same registry-load + path-resolution logic
    //! kept private to this module. Errors print to stderr; exit codes
    //! follow the rest of the CLI (1 = runtime error, 2 = bad usage).

    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command as StdCommand, ExitCode, Stdio};

    use atelier_core::skills::{Skill, SkillRegistry, SkillSource};

    /// Where to write a new manifest. `Repo` is the spec-recommended
    /// default — sharing skills via git is the most common workflow.
    enum Scope {
        Repo,
        User,
    }

    fn workspace_or_exit(verb: &str) -> Result<PathBuf, ExitCode> {
        env::current_dir().map_err(|e| {
            eprintln!("atelier skills {verb}: cannot read current directory: {e}");
            ExitCode::from(1)
        })
    }

    fn registry_or_exit(workspace: &Path, verb: &str) -> Result<SkillRegistry, ExitCode> {
        let home = env::var_os("HOME").map(PathBuf::from);
        SkillRegistry::load(workspace, home.as_deref()).map_err(|e| {
            eprintln!("atelier skills {verb}: {}", friendly_load_error(&e));
            ExitCode::from(1)
        })
    }

    /// S22 — map the most common authoring mistakes to friendlier
    /// one-liners. The default `jsonschema` formatter is verbose and
    /// JSON-Pointer-heavy; users authoring their first manifest deserve
    /// "name must be lowercase letters / digits / `_-`" not
    /// "/name does not match pattern …".
    fn friendly_load_error(e: &atelier_core::skills::SkillLoadError) -> String {
        let raw = e.to_string();
        if raw.contains("does not match") && raw.contains("name") {
            return format!(
                "name must be lowercase letters / digits / `_-`, starting with a letter (raw: {raw})"
            );
        }
        if raw.contains("required") {
            return format!(
                "missing required field — see examples/skills/explain.v1.json for a complete manifest (raw: {raw})"
            );
        }
        raw
    }

    fn validate_slug(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("skill name must not be empty".into());
        }
        let mut chars = name.chars();
        let first = chars.next().unwrap();
        if !first.is_ascii_lowercase() {
            return Err(format!(
                "skill name must start with a lowercase letter, got `{name}`"
            ));
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
                return Err(format!(
                    "skill name must be [a-z0-9_-]+, got `{name}` (offending char `{c}`)"
                ));
            }
        }
        Ok(())
    }

    fn scope_dir(scope: &Scope, workspace: &Path) -> Result<PathBuf, String> {
        match scope {
            Scope::Repo => Ok(workspace.join(".atelier/skills")),
            Scope::User => {
                let home = env::var_os("HOME")
                    .map(PathBuf::from)
                    .ok_or_else(|| "HOME is not set; cannot resolve user scope".to_string())?;
                Ok(home.join(".atelier/skills"))
            }
        }
    }

    /// S20 helper — render a `Skill` to a `(text, source_path)` pair
    /// for `show`. The text is canonical JSON; the path is `None` for
    /// bundled skills (they live inside the binary).
    fn render_skill_for_show(workspace: &Path, skill: &Skill) -> (String, Option<PathBuf>) {
        let value = serde_json::json!({
            "version": skill.version,
            "name": skill.name,
            "description": skill.description,
            "prompt_template": skill.prompt_template,
            "args": skill.args,
            "pinned_context": skill.pinned_context,
            "tools_required": skill.tools_required,
            "proactive_trigger": skill.proactive_trigger,
            "side_effect_class": skill.side_effect_class.as_str(),
            "source": skill.source.as_str(),
        });
        let path = match skill.source {
            SkillSource::Bundled => None,
            SkillSource::UserHome => env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| h.join(format!(".atelier/skills/{}.json", skill.name))),
            SkillSource::RepoLocal => {
                Some(workspace.join(format!(".atelier/skills/{}.json", skill.name)))
            }
        };
        (serde_json::to_string_pretty(&value).unwrap(), path)
    }

    // ---------- verbs ----------

    pub fn run_list() -> ExitCode {
        let workspace = match workspace_or_exit("") {
            Ok(p) => p,
            Err(c) => return c,
        };
        let registry = match registry_or_exit(&workspace, "") {
            Ok(r) => r,
            Err(c) => return c,
        };
        print!("{}", registry.format_help());
        ExitCode::SUCCESS
    }

    /// S16 — scaffold a new manifest.
    pub fn run_new(args: impl Iterator<Item = String>) -> ExitCode {
        let mut name: Option<String> = None;
        let mut scope = Scope::Repo;
        let mut from: Option<String> = None;
        let mut iter = args.peekable();
        while let Some(a) = iter.next() {
            match a.as_str() {
                "-h" | "--help" => {
                    println!(
                        "atelier skills new <name> [--scope user|repo] [--from <existing>]\n\nScaffold a starter manifest. Refuses to overwrite. Opens the new file in $EDITOR if set; otherwise prints the path."
                    );
                    return ExitCode::SUCCESS;
                }
                "--scope" => match iter.next().as_deref() {
                    Some("user") => scope = Scope::User,
                    Some("repo") => scope = Scope::Repo,
                    other => {
                        eprintln!(
                            "atelier skills new: --scope requires `user` or `repo`, got {other:?}"
                        );
                        return ExitCode::from(2);
                    }
                },
                "--from" => match iter.next() {
                    Some(v) => from = Some(v),
                    None => {
                        eprintln!("atelier skills new: --from requires a skill name");
                        return ExitCode::from(2);
                    }
                },
                _ => {
                    if name.is_some() {
                        eprintln!("atelier skills new: unexpected argument `{a}`");
                        return ExitCode::from(2);
                    }
                    name = Some(a);
                }
            }
        }
        let Some(name) = name else {
            eprintln!("atelier skills new: <name> is required\n");
            eprintln!("{}", super::SKILLS_USAGE);
            return ExitCode::from(2);
        };
        if let Err(e) = validate_slug(&name) {
            eprintln!("atelier skills new: {e}");
            return ExitCode::from(2);
        }
        let workspace = match workspace_or_exit("new") {
            Ok(p) => p,
            Err(c) => return c,
        };
        let dir = match scope_dir(&scope, &workspace) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("atelier skills new: {e}");
                return ExitCode::from(1);
            }
        };
        let target = dir.join(format!("{name}.json"));
        if target.exists() {
            eprintln!(
                "atelier skills new: {} already exists; refusing to overwrite",
                target.display()
            );
            return ExitCode::from(1);
        }
        // Seed body — either from an existing skill or from a minimal
        // starter template that demonstrates the substitution surface.
        let seed = match from {
            Some(parent) => {
                let registry = match registry_or_exit(&workspace, "new") {
                    Ok(r) => r,
                    Err(c) => return c,
                };
                let Some(skill) = registry.get(&parent) else {
                    eprintln!(
                        "atelier skills new: --from `{parent}` not found; available: {}",
                        registry.names().cloned().collect::<Vec<_>>().join(", ")
                    );
                    return ExitCode::from(1);
                };
                let mut value = serde_json::json!({
                    "version": skill.version,
                    "name": name,
                    "description": skill.description,
                    "prompt_template": skill.prompt_template,
                    "side_effect_class": skill.side_effect_class.as_str(),
                });
                if !skill.args.is_empty() {
                    value["args"] = serde_json::to_value(&skill.args).unwrap();
                }
                if !skill.pinned_context.is_empty() {
                    value["pinned_context"] = serde_json::to_value(&skill.pinned_context).unwrap();
                }
                if !skill.tools_required.is_empty() {
                    value["tools_required"] = serde_json::to_value(&skill.tools_required).unwrap();
                }
                if let Some(p) = &skill.proactive_trigger {
                    value["proactive_trigger"] = serde_json::Value::String(p.clone());
                }
                serde_json::to_string_pretty(&value).unwrap()
            }
            None => serde_json::to_string_pretty(&serde_json::json!({
                "version": 1,
                "name": name,
                "description": "<one-line description shown in /help>",
                "prompt_template": "Describe what you want done. ${target} is a sample arg.",
                "args": [
                    {
                        "name": "target",
                        "description": "What this skill should operate on.",
                        "required": true
                    }
                ],
                "pinned_context": ["ATELIER.md"],
                "side_effect_class": "local-safe"
            }))
            .unwrap(),
        };
        // Capture the pre-existing entry (if any) before writing so
        // we can give a "this shadows X" heads-up — that's only
        // meaningful when the skill already existed in a *different*
        // layer (post-write the new file always wins its scope).
        let prior_layer: Option<SkillSource> = registry_or_exit(&workspace, "new")
            .ok()
            .and_then(|r| r.get(&name).map(|s| s.source.clone()));
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("atelier skills new: mkdir {}: {e}", dir.display());
            return ExitCode::from(1);
        }
        if let Err(e) = fs::write(&target, format!("{seed}\n")) {
            eprintln!("atelier skills new: write {}: {e}", target.display());
            return ExitCode::from(1);
        }
        // S25 — naming-conflict heads-up (Open Question #5). Show
        // shadowing only when the pre-existing skill was in a
        // different layer than the one we just wrote.
        let new_layer = match scope {
            Scope::Repo => SkillSource::RepoLocal,
            Scope::User => SkillSource::UserHome,
        };
        match prior_layer {
            Some(layer) if layer != new_layer => println!(
                "atelier skills new: created {} (shadows existing /{} from {})",
                target.display(),
                name,
                layer.help_tag(),
            ),
            _ => println!("atelier skills new: created {}", target.display()),
        }
        // Open in $EDITOR if set — friendlier UX than asking the user
        // to find the path. Failure is non-fatal.
        if let Some(editor) = env::var_os("EDITOR") {
            let status = StdCommand::new(&editor)
                .arg(&target)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status();
            if let Err(e) = status {
                eprintln!(
                    "atelier skills new: $EDITOR ({:?}) failed: {e}; manifest is at {}",
                    editor,
                    target.display()
                );
            }
        }
        ExitCode::SUCCESS
    }

    /// S17 — lint a manifest, or every manifest in the registry when
    /// no path is given. Exits non-zero on any failure so pre-commit
    /// hooks can adopt it.
    pub fn run_validate(mut args: impl Iterator<Item = String>) -> ExitCode {
        let first = args.next();
        if matches!(first.as_deref(), Some("-h" | "--help")) {
            println!(
                "atelier skills validate [path]\n\nLint a manifest file, or every manifest in the resolved registry when no path is given. Exits non-zero on any failure."
            );
            return ExitCode::SUCCESS;
        }
        if let Some(path) = first {
            return validate_one_file(Path::new(&path));
        }
        // Walk the resolved registry.
        let workspace = match workspace_or_exit("validate") {
            Ok(p) => p,
            Err(c) => return c,
        };
        let registry = match registry_or_exit(&workspace, "validate") {
            Ok(r) => r,
            Err(c) => return c,
        };
        let mut failures = 0;
        for skill in registry.iter() {
            // Pinned-context existence check (S23) — warn, don't fail.
            for pin in &skill.pinned_context {
                let absolute = if Path::new(pin).is_absolute() {
                    PathBuf::from(pin)
                } else {
                    workspace.join(pin)
                };
                if !absolute.exists() {
                    eprintln!(
                        "atelier skills validate: warn: skill `/{}` pins {} which doesn't exist in this workspace",
                        skill.name, pin
                    );
                }
            }
            // Substitution lint — every `${name}` in prompt_template
            // must resolve to a declared arg, `${repo_root}`, or
            // `${atelier_md}`.
            for var in scan_template_vars(&skill.prompt_template) {
                if var == "repo_root" || var == "atelier_md" {
                    continue;
                }
                if !skill.args.iter().any(|a| a.name == var) {
                    eprintln!(
                        "atelier skills validate: skill `/{}` references ${{{var}}} but no arg `{var}` is declared",
                        skill.name
                    );
                    failures += 1;
                }
            }
        }
        if failures == 0 {
            println!("atelier skills validate: {} skill(s) ok", registry.len());
            ExitCode::SUCCESS
        } else {
            eprintln!("atelier skills validate: {failures} failure(s)");
            ExitCode::from(1)
        }
    }

    fn validate_one_file(path: &Path) -> ExitCode {
        let body = match fs::read_to_string(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("atelier skills validate: read {}: {e}", path.display());
                return ExitCode::from(1);
            }
        };
        match Skill::from_manifest_json(&body, SkillSource::RepoLocal) {
            Ok(_) => {
                println!("atelier skills validate: {} ok", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!(
                    "atelier skills validate: {}: {}",
                    path.display(),
                    friendly_load_error(&e)
                );
                ExitCode::from(1)
            }
        }
    }

    fn scan_template_vars(template: &str) -> Vec<String> {
        let mut out = Vec::new();
        let bytes = template.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'$' && bytes[i + 1] == b'{' {
                if let Some(rel_end) = template[i + 2..].find('}') {
                    let name = &template[i + 2..i + 2 + rel_end];
                    out.push(name.to_string());
                    i += 2 + rel_end + 1;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// S18 — open the resolved manifest in $EDITOR. Refuses bundled
    /// (immutable in-binary); user must `new --from <name>` to fork.
    pub fn run_edit(args: impl Iterator<Item = String>) -> ExitCode {
        let mut iter = args;
        let Some(name) = iter.next() else {
            eprintln!("atelier skills edit: <name> is required");
            return ExitCode::from(2);
        };
        if name == "-h" || name == "--help" {
            println!("atelier skills edit <name>\n\nOpen the resolved manifest in $EDITOR. Refuses bundled — use `atelier skills new --from <name> --scope user` to fork.");
            return ExitCode::SUCCESS;
        }
        let workspace = match workspace_or_exit("edit") {
            Ok(p) => p,
            Err(c) => return c,
        };
        let registry = match registry_or_exit(&workspace, "edit") {
            Ok(r) => r,
            Err(c) => return c,
        };
        let Some(skill) = registry.get(&name) else {
            eprintln!("atelier skills edit: unknown skill `{name}`");
            return ExitCode::from(1);
        };
        let (_, path) = render_skill_for_show(&workspace, skill);
        let Some(path) = path else {
            eprintln!(
                "atelier skills edit: `/{name}` is bundled — fork via `atelier skills new --from {name} --scope user`"
            );
            return ExitCode::from(1);
        };
        let editor = match env::var_os("EDITOR") {
            Some(e) => e,
            None => {
                eprintln!(
                    "atelier skills edit: $EDITOR is not set; manifest is at {}",
                    path.display()
                );
                return ExitCode::from(1);
            }
        };
        let status = StdCommand::new(&editor)
            .arg(&path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        match status {
            Ok(s) if s.success() => ExitCode::SUCCESS,
            Ok(s) => {
                eprintln!("atelier skills edit: $EDITOR exited with {s}");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("atelier skills edit: spawn $EDITOR ({editor:?}): {e}");
                ExitCode::from(1)
            }
        }
    }

    /// S19 — delete a user- or per-repo-scope manifest. Refuses
    /// bundled (those are in-binary). On shadow removal, prints which
    /// skill will be active afterwards.
    pub fn run_delete(args: impl Iterator<Item = String>) -> ExitCode {
        let mut iter = args;
        let Some(name) = iter.next() else {
            eprintln!("atelier skills delete: <name> is required");
            return ExitCode::from(2);
        };
        if name == "-h" || name == "--help" {
            println!("atelier skills delete <name>\n\nRemove a user- or per-repo-scope manifest. Refuses bundled.");
            return ExitCode::SUCCESS;
        }
        let workspace = match workspace_or_exit("delete") {
            Ok(p) => p,
            Err(c) => return c,
        };
        let registry = match registry_or_exit(&workspace, "delete") {
            Ok(r) => r,
            Err(c) => return c,
        };
        let Some(skill) = registry.get(&name) else {
            eprintln!("atelier skills delete: unknown skill `{name}`");
            return ExitCode::from(1);
        };
        let (_, path) = render_skill_for_show(&workspace, skill);
        let Some(path) = path else {
            eprintln!("atelier skills delete: `/{name}` is bundled and cannot be deleted");
            return ExitCode::from(1);
        };
        if let Err(e) = fs::remove_file(&path) {
            eprintln!("atelier skills delete: unlink {}: {e}", path.display());
            return ExitCode::from(1);
        }
        // Reload to see what wins next.
        let registry2 = registry_or_exit(&workspace, "delete").ok();
        if let Some(reg2) = registry2 {
            match reg2.get(&name) {
                Some(s) => println!(
                    "atelier skills delete: removed {}; `/{name}` from {} is now active",
                    path.display(),
                    s.source.help_tag()
                ),
                None => println!(
                    "atelier skills delete: removed {}; no `/{name}` remains",
                    path.display()
                ),
            }
        } else {
            println!("atelier skills delete: removed {}", path.display());
        }
        ExitCode::SUCCESS
    }

    /// S20 — print the resolved manifest + its source path + a
    /// `[shadows: <other>]` line if a lower-precedence skill of the
    /// same name exists.
    pub fn run_show(args: impl Iterator<Item = String>) -> ExitCode {
        let mut iter = args;
        let Some(name) = iter.next() else {
            eprintln!("atelier skills show: <name> is required");
            return ExitCode::from(2);
        };
        if name == "-h" || name == "--help" {
            println!("atelier skills show <name>\n\nPrint the resolved manifest + source path.");
            return ExitCode::SUCCESS;
        }
        let workspace = match workspace_or_exit("show") {
            Ok(p) => p,
            Err(c) => return c,
        };
        let registry = match registry_or_exit(&workspace, "show") {
            Ok(r) => r,
            Err(c) => return c,
        };
        let Some(skill) = registry.get(&name) else {
            eprintln!("atelier skills show: unknown skill `{name}`");
            return ExitCode::from(1);
        };
        let (text, path) = render_skill_for_show(&workspace, skill);
        match path {
            Some(p) => println!("# source: {}", p.display()),
            None => println!("# source: bundled (in-binary)"),
        }
        println!("{text}");
        ExitCode::SUCCESS
    }
}

// ---------- `atelier run` ----------
//
// The function is structured top-down so the data flow reads in
// stages:
//
//   parse argv  →  resolve workspace  →  load TOML config  →
//   layer CLI > TOML > defaults  →  build Runner  →  run.
//
// Each stage hands typed values to the next; nothing reaches the
// Runner that hasn't been validated.

/// Raw CLI flags before any defaulting or config-merging is applied.
/// Everything is `Option<T>` so the precedence resolver can tell
/// "user didn't say" from "user said this." `prompt_args` is the
/// only field that's intrinsically a `Vec` because positional words
/// concatenate; `no_probe` / `force_probe` are bare bools because a
/// flag is either there or not.
struct CliArgs {
    /// `--profile <NAME>` — selects which `[providers.<name>]` table
    /// in providers.toml is the *base* of the resolved provider.
    /// `None` means "fall back to `default` in the file, then to
    /// built-in defaults."
    profile: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    workspace: Option<PathBuf>,
    max_turns: Option<usize>,
    prompt_file: Option<PathBuf>,
    prompt_args: Vec<String>,
    no_probe: bool,
    force_probe: bool,
    /// v61 — `--non-interactive`: composite flag that disables every
    /// modal flow (approval + concurrent-edit). Always present (false
    /// when omitted) so `parse_cli` doesn't need an `Option<bool>`.
    non_interactive: bool,
    /// v61 — `--resume <UUID>`: when present, the runner reads the
    /// on-disk session under `.atelier/sessions/<uuid>/`, replays its
    /// conversation prefix, and appends the supplied prompt as a
    /// fresh user turn (or skips it if empty).
    resume: Option<uuid::Uuid>,
}

impl CliArgs {
    fn empty() -> Self {
        Self {
            profile: None,
            provider: None,
            model: None,
            base_url: None,
            workspace: None,
            max_turns: None,
            prompt_file: None,
            prompt_args: Vec::new(),
            no_probe: false,
            force_probe: false,
            non_interactive: false,
            resume: None,
        }
    }
}

/// Either a fully parsed [`CliArgs`] or an exit code (`--help`,
/// missing-value error). The caller dispatches on the result; this
/// keeps the parsing function flat — no early `return ExitCode` from
/// inside the parse loop.
///
/// v61: `CliArgs` grew enough fields (Option<PathBuf>, Option<Uuid>,
/// Vec<String>, …) to trip clippy's `large_enum_variant`. Boxing the
/// `Ok` variant pays one extra allocation per CLI invocation — well
/// under the cost of a `parse_cli` round-trip — and keeps the parse
/// loop flat.
enum CliParseResult {
    Ok(Box<CliArgs>),
    Exit(ExitCode),
}

fn parse_cli(mut args: impl Iterator<Item = String>) -> CliParseResult {
    let mut out = CliArgs::empty();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return CliParseResult::Exit(ExitCode::SUCCESS);
            }
            "--profile" => match args.next() {
                Some(v) => out.profile = Some(v),
                None => return missing_value("--profile", "name"),
            },
            "--provider" => match args.next() {
                Some(v) => out.provider = Some(v),
                None => return missing_value("--provider", "value"),
            },
            "--model" => match args.next() {
                Some(v) => out.model = Some(v),
                None => return missing_value("--model", "value"),
            },
            "--base-url" => match args.next() {
                Some(v) => out.base_url = Some(v),
                None => return missing_value("--base-url", "URL"),
            },
            "--workspace" => match args.next() {
                Some(v) => out.workspace = Some(PathBuf::from(v)),
                None => return missing_value("--workspace", "path"),
            },
            "--max-turns" => match args.next().and_then(|s| s.parse::<usize>().ok()) {
                Some(0) => return missing_value("--max-turns", "positive integer >= 1"),
                Some(n) => out.max_turns = Some(n),
                None => return missing_value("--max-turns", "positive integer >= 1"),
            },
            "--prompt-file" => match args.next() {
                Some(v) => out.prompt_file = Some(PathBuf::from(v)),
                None => return missing_value("--prompt-file", "path"),
            },
            "--no-probe" => out.no_probe = true,
            "--force-probe" => out.force_probe = true,
            "--non-interactive" => out.non_interactive = true,
            "--resume" => match args.next() {
                Some(v) => match uuid::Uuid::parse_str(&v) {
                    Ok(u) => out.resume = Some(u),
                    Err(_) => {
                        eprintln!("atelier run: --resume requires a UUID, got {v:?}");
                        return CliParseResult::Exit(ExitCode::from(2));
                    }
                },
                None => return missing_value("--resume", "session-uuid"),
            },
            // Everything else is positional prompt text.
            _ => out.prompt_args.push(a),
        }
    }
    CliParseResult::Ok(Box::new(out))
}

fn missing_value(flag: &str, kind: &str) -> CliParseResult {
    eprintln!("atelier run: {flag} requires a {kind}");
    CliParseResult::Exit(ExitCode::from(2))
}

fn run_run(args: impl Iterator<Item = String>) -> ExitCode {
    // 1. Parse argv into a typed CliArgs.
    let cli = match parse_cli(args) {
        CliParseResult::Ok(c) => c,
        CliParseResult::Exit(code) => return code,
    };

    if cli.no_probe && cli.force_probe {
        eprintln!("atelier run: --no-probe and --force-probe are mutually exclusive");
        return ExitCode::from(2);
    }

    // 2. Resolve the workspace path. Needed before config load
    //    because `<workspace>/.atelier/providers.toml` is the
    //    project scope.
    let workspace = match cli
        .workspace
        .clone()
        .map(Ok)
        .unwrap_or_else(env::current_dir)
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("atelier run: cannot read current directory: {e}");
            return ExitCode::from(1);
        }
    };

    // 3. Load the TOML config (best-effort; absent is OK). A
    //    malformed file is fatal — silently ignoring it would let a
    //    typo silently fall back to defaults, which is exactly the
    //    surprise this layer exists to prevent.
    let loaded = match ProvidersConfig::load(&workspace) {
        Ok(opt) => opt,
        Err(e) => {
            eprintln!("atelier run: config error: {e}");
            return ExitCode::from(2);
        }
    };
    let config = loaded
        .as_ref()
        .map(|l| l.config.clone())
        .unwrap_or_default();

    // 4. Resolve which named profile (if any) is the *base* of the
    //    provider settings. CLI `--profile` overrides the file's
    //    `default`. None of either yields `None` and the CLI flags
    //    are expected to specify everything they need directly.
    let resolved_profile = match config.resolve_profile(cli.profile.as_deref()) {
        Ok(p) => p.map(|(name, profile)| (name.to_string(), profile.clone())),
        Err(e) => {
            eprintln!("atelier run: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(LoadedConfig { path, .. }) = &loaded {
        match &resolved_profile {
            Some((name, _)) => println!(
                "atelier run: using config {} (profile {name:?})",
                path.display(),
            ),
            None => println!("atelier run: using config {}", path.display()),
        }
    }

    // 5. Layer CLI > resolved profile > defaults into the runtime
    //    values the Runner needs.
    let provider_choice =
        match resolve_provider_choice(&cli, resolved_profile.as_ref().map(|(_, p)| p)) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("atelier run: {e}");
                return ExitCode::from(2);
            }
        };
    let max_turns = cli
        .max_turns
        .or_else(|| config.runner.as_ref().and_then(|r| r.max_turns));
    let probe_policy_override = resolve_probe_policy(&cli, &config);

    // 5. Read the prompt (positional or --prompt-file or stdin).
    //    v61: an empty prompt is permitted when `--resume` is in play —
    //    the runner just picks up the conversation prefix from disk.
    let prompt = if cli.resume.is_some() && cli.prompt_args.is_empty() && cli.prompt_file.is_none()
    {
        String::new()
    } else {
        match read_prompt_from_cli(&cli) {
            Ok(s) => s,
            Err(code) => return code,
        }
    };

    // 6. Build the tokio runtime + Runner + run.
    //
    // For the mock provider with no scripted responses the loop has
    // nothing to do — the adapter returns NotConfigured on the first
    // chat call. v0 binary use is the docs walkthrough; the
    // integration tests construct `Runner` directly with
    // `Mock { responses }` to script real turns.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("atelier run: tokio runtime build failed: {e}");
            return ExitCode::from(1);
        }
    };

    // v60.51 §15 — load the skill registry before we move
    // `workspace` into `Runner::new`. `$HOME` (the same env var used
    // by the profile store) is the canonical home-dir lookup; missing
    // is OK and just means only bundled + per-repo layers contribute.
    let home_dir = std::env::var_os("HOME").map(PathBuf::from);
    let registry = match atelier_core::skills::SkillRegistry::load(&workspace, home_dir.as_deref())
    {
        Ok(r) => std::sync::Arc::new(r),
        Err(e) => {
            eprintln!("atelier run: skills: {e}");
            return ExitCode::from(2);
        }
    };

    // v60.51 §15 — `/help` is a harness-intercepted CLI verb (spec
    // §15 line 785). Print the help block and exit cleanly without
    // building the rest of the runtime so the model never sees the
    // help text in its context window.
    {
        let trimmed = prompt.trim();
        if trimmed == "/help" || trimmed.starts_with("/help ") {
            print!("{}", registry.format_help());
            return ExitCode::SUCCESS;
        }
    }

    let mut runner =
        match runner::Runner::new(workspace, provider_choice, runner::EventSink::Stdout) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("atelier run: {e}");
                return ExitCode::from(1);
            }
        };
    runner = runner.with_skill_registry(registry);
    // §1 per-task routing — if [routing] specifies an executor profile,
    // build a second adapter for tool-result turns.
    if let Some(exec_name) = config.routing.as_ref().and_then(|r| r.executor.as_deref()) {
        match build_executor_adapter(exec_name, &config) {
            Ok(exec) => {
                runner = runner.with_executor_adapter(exec);
                tracing::info!(
                    profile = exec_name,
                    "per-task routing: executor adapter loaded"
                );
            }
            Err(e) => eprintln!("atelier run: routing.executor: {e}"),
        }
    }
    if let Some(n) = max_turns {
        runner = runner.with_max_turns(n);
    }
    if let Some(policy) = probe_policy_override {
        runner = runner.with_probe_policy(policy);
    }
    // v61 — §14 flags. Order matters: `with_non_interactive(true)`
    // forces both AutoApproveAll and AutoReload, so call it last so a
    // user's earlier policy choices (none today, but future flags)
    // can't override the headless guarantee.
    if let Some(uuid) = cli.resume {
        runner = runner.with_resume(uuid);
    }
    if cli.non_interactive {
        runner = runner.with_non_interactive(true);
    }

    // v60.29 H10 — wire a SIGINT/SIGTERM handler. The handler trips
    // a cancellation token the runner threads down through the §2.5
    // actor + dispatcher; in-flight tools surface `ToolError::Cancelled`
    // and the run loop returns naturally, letting the existing
    // run-and-save tail in `Runner::run` persist the partial session.
    // On signal we exit 130 (SIGINT) or 143 (SIGTERM) per POSIX.
    let cancel = tokio_util::sync::CancellationToken::new();
    runner = runner.with_external_cancel(cancel.clone());

    let signal_result = rt.block_on(run_with_signal_handling(runner, prompt, cancel));
    match signal_result {
        SignalOutcome::Completed(Ok(report)) => {
            println!(
                "atelier run: session {} ended in {:?} after {} turn(s){}",
                report.session_id,
                report.final_state,
                report.turns,
                match report.dod_passed {
                    Some(true) => "; DoD: pass",
                    Some(false) => "; DoD: fail",
                    None => "; DoD: not configured",
                }
            );
            ExitCode::from(atelier_cli::exit_code_for_final_state(report.final_state))
        }
        SignalOutcome::Completed(Err(e)) => {
            eprintln!("atelier run: {e}");
            ExitCode::from(1)
        }
        SignalOutcome::Interrupted { exit_code } => {
            eprintln!("atelier run: interrupted; partial session persisted");
            ExitCode::from(exit_code)
        }
    }
}

/// v60.29 H10 — signal-aware variant of "run a future to completion".
///
/// Races the `runner.run(prompt)` future against
/// `tokio::signal::ctrl_c()` and (unix only) SIGTERM. On signal: trips
/// the supplied `CancellationToken` and awaits the run future to
/// completion so the runner's normal teardown — including the
/// `OnDiskSession::save_to` tail — runs against the partial state. The
/// exit code follows POSIX convention: 130 for SIGINT, 143 for SIGTERM.
enum SignalOutcome {
    Completed(Result<runner::RunReport, runner::RunError>),
    Interrupted { exit_code: u8 },
}

async fn run_with_signal_handling(
    runner: runner::Runner,
    prompt: String,
    cancel: tokio_util::sync::CancellationToken,
) -> SignalOutcome {
    use tokio::signal;
    #[cfg(unix)]
    use tokio::signal::unix::{signal as unix_signal, SignalKind};

    let mut run_fut = Box::pin(runner.run(prompt));
    // First select: race the run future against the signals. On
    // signal, trip the token, then await the run to its persistence
    // tail. The runner's own teardown writes the partial session.
    let signal_exit_code: u8;
    #[cfg(unix)]
    {
        let mut sigterm = match unix_signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler init failed; ^C still wired");
                None
            }
        };
        tokio::select! {
            res = &mut run_fut => return SignalOutcome::Completed(res),
            _ = signal::ctrl_c() => {
                signal_exit_code = 130;
            }
            _ = async { match sigterm.as_mut() { Some(s) => { s.recv().await; }, None => std::future::pending::<()>().await } } => {
                signal_exit_code = 143;
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            res = &mut run_fut => return SignalOutcome::Completed(res),
            _ = signal::ctrl_c() => {
                signal_exit_code = 130;
            }
        }
    }

    cancel.cancel();
    let _ = run_fut.await;
    SignalOutcome::Interrupted {
        exit_code: signal_exit_code,
    }
}

/// Layer CLI flags on top of the resolved profile to produce the final
/// [`runner::ProviderChoice`]. Returns a printable error string on
/// validation failure so the caller emits one consistent
/// `atelier run: <error>` line.
///
/// Precedence per field is `cli.or(profile).or(default)`. The
/// `profile` here is the named `[providers.<name>]` table picked by
/// either `--profile` or the file's `default`.
fn resolve_provider_choice(
    cli: &CliArgs,
    profile: Option<&ProviderProfile>,
) -> Result<runner::ProviderChoice, String> {
    // Resolve `provider` first because the other fields depend on
    // which adapter we're talking to.
    let kind = resolve_provider_kind(cli.provider.as_deref(), profile)?;
    let model = cli
        .model
        .clone()
        .or_else(|| profile.and_then(|p| p.model.clone()));
    let base_url = cli
        .base_url
        .clone()
        .or_else(|| profile.and_then(|p| p.base_url.clone()));

    match kind {
        ProviderKind::Mock => {
            // Only reject --base-url when it came from the CLI; a profile
            // that has base_url for an openai-compat section is harmless
            // when --provider mock is used explicitly.
            if cli.base_url.is_some() {
                return Err("--base-url is only valid with provider `openai-compat`".into());
            }
            Ok(runner::ProviderChoice::Mock {
                responses: Vec::new(),
            })
        }
        ProviderKind::Anthropic => {
            let model_id = model.unwrap_or_else(|| "anthropic:claude-opus-4-7".to_string());
            if !model_id.starts_with("anthropic:") {
                return Err(format!(
                    "model for provider `anthropic` must be prefixed `anthropic:` \
                     (got {model_id:?}); e.g. anthropic:claude-opus-4-7"
                ));
            }
            if cli.base_url.is_some() {
                return Err("--base-url is only valid with provider `openai-compat`".into());
            }
            Ok(runner::ProviderChoice::Anthropic { model_id })
        }
        ProviderKind::OpenaiCompat => {
            let Some(model_id) = model else {
                return Err("provider `openai-compat` requires a model id \
                     (CLI `--model <ID>` or TOML `[providers.<name>].model = \"...\"`); \
                     e.g. `local:llama3:8b` or `openai:gpt-4o-mini`. The id is \
                     sent verbatim to the server."
                    .into());
            };
            let api_key_ref = profile.and_then(|p| p.api_key.as_deref());
            atelier_core::provider_profile_base_url_may_receive_credential(
                base_url.as_deref(),
                cli.base_url.is_some(),
                atelier_core::api_key_ref_may_resolve(
                    api_key_ref,
                    atelier_core::OPENAI_API_KEY_ENV,
                ),
            )?;
            let api_key = atelier_core::resolve_openai_api_key(api_key_ref)
                .map_err(|e| format!("provider credential: {e}"))?;
            let cache_prompt = profile.and_then(|p| p.cache_prompt).unwrap_or(false);
            Ok(runner::ProviderChoice::OpenAiCompat {
                model_id,
                base_url,
                api_key: Some(api_key),
                cache_prompt,
            })
        }
    }
}

/// Resolve which `ProviderKind` to use. CLI `--provider <NAME>` wins;
/// then the resolved profile's `provider` field; otherwise fall back
/// to `Mock` so a fresh repo with no config still runs.
fn resolve_provider_kind(
    cli_provider: Option<&str>,
    profile: Option<&ProviderProfile>,
) -> Result<ProviderKind, String> {
    if let Some(p) = cli_provider {
        return parse_provider_string(p, "--provider");
    }
    if let Some(kind) = profile.and_then(|p| p.provider) {
        return Ok(kind);
    }
    Ok(ProviderKind::Mock)
}

fn parse_provider_string(s: &str, source: &str) -> Result<ProviderKind, String> {
    match s {
        "mock" => Ok(ProviderKind::Mock),
        "anthropic" => Ok(ProviderKind::Anthropic),
        "openai-compat" => Ok(ProviderKind::OpenaiCompat),
        other => Err(format!(
            "{source}: unknown provider {other:?}. Supported: `mock`, `anthropic`, \
             `openai-compat`. (`bedrock`, `vertex` land in Phase E/F.)"
        )),
    }
}

/// Layer CLI `--no-probe` / `--force-probe` over the TOML
/// `[probe].policy`. Returns `Some(policy)` when the runner should
/// override its per-provider default — `None` means "leave the
/// Runner's built-in default in place" (which is `Skip` for Mock /
/// Anthropic and `Auto` for OpenAI-compat).
fn resolve_probe_policy(cli: &CliArgs, config: &ProvidersConfig) -> Option<runner::ProbePolicy> {
    if cli.no_probe {
        return Some(runner::ProbePolicy::Skip);
    }
    if cli.force_probe {
        return Some(runner::ProbePolicy::Force);
    }
    config
        .probe
        .as_ref()
        .and_then(|p| p.policy)
        .map(|p| match p {
            ProbePolicyName::Auto => runner::ProbePolicy::Auto,
            ProbePolicyName::Skip => runner::ProbePolicy::Skip,
            ProbePolicyName::Force => runner::ProbePolicy::Force,
        })
}

/// Extract `host:port` from an HTTP/HTTPS base URL for TCP reachability checks.
fn extract_host_port(base_url: &str) -> Option<String> {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))?;
    let host_port = rest.split('/').next()?;
    if host_port.is_empty() {
        return None;
    }
    if host_port.contains(':') {
        Some(host_port.to_string())
    } else {
        let default_port = if base_url.starts_with("https://") {
            "443"
        } else {
            "80"
        };
        Some(format!("{host_port}:{default_port}"))
    }
}

/// Returns `true` when the host:port in `base_url` accepts a TCP connection
/// within 1 second. Optimistically returns `true` if the URL can't be parsed.
fn preflight_base_url(base_url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Some(addr_str) = extract_host_port(base_url) else {
        return true;
    };
    let addrs: Vec<_> = match addr_str.to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return true,
    };
    for sa in addrs {
        if std::net::TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(1)).is_ok() {
            return true;
        }
    }
    false
}

/// Build an executor adapter from a named profile in `config`. Used for
/// §1 per-task routing. Errors are non-fatal — the caller logs a warning
/// and falls back to the single-adapter path.
fn build_executor_adapter(
    profile_name: &str,
    config: &ProvidersConfig,
) -> Result<std::sync::Arc<dyn atelier_core::adapter::Adapter>, String> {
    let (_, profile) = config
        .resolve_profile(Some(profile_name))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("profile {profile_name:?} not found in providers.toml"))?;
    let kind = profile
        .provider
        .ok_or_else(|| format!("profile {profile_name:?} has no `provider` field"))?;
    let model_id = profile
        .model
        .clone()
        .ok_or_else(|| format!("profile {profile_name:?} has no `model` field"))?;
    match kind {
        ProviderKind::OpenaiCompat => {
            let base_url = profile.base_url.clone().unwrap_or_default();
            atelier_core::provider_profile_base_url_may_receive_credential(
                profile.base_url.as_deref(),
                false,
                atelier_core::api_key_ref_may_resolve(
                    profile.api_key.as_deref(),
                    atelier_core::OPENAI_API_KEY_ENV,
                ),
            )?;
            if !preflight_base_url(&base_url) {
                return Err(format!(
                    "executor profile {profile_name:?}: server at {base_url:?} is unreachable \
                     (TCP connect timed out); skipping executor adapter"
                ));
            }
            let api_key = atelier_core::resolve_openai_api_key(profile.api_key.as_deref())
                .map_err(|e| {
                    format!("executor profile {profile_name:?}: provider credential: {e}")
                })?;
            let mut adapter = atelier_core::adapter::openai_compat::OpenAiCompatAdapter::new(
                api_key, model_id, base_url,
            );
            if profile.cache_prompt.unwrap_or(false) {
                adapter = adapter.with_cache_prompt(true);
            }
            Ok(std::sync::Arc::new(adapter))
        }
        ProviderKind::Anthropic => {
            let adapter = atelier_core::adapter::anthropic::AnthropicAdapter::from_env(model_id)
                .map_err(|e| e.to_string())?;
            Ok(std::sync::Arc::new(adapter))
        }
        ProviderKind::Mock => Err(format!(
            "profile {profile_name:?}: mock provider is not valid for routing.executor"
        )),
    }
}

// ---------- `atelier protocol-overhead` ----------
//
// The subcommand is intentionally small: it forwards to
// `atelier_cli::overhead::run` with paths layered (CLI > defaults) and
// prints a one-line summary on success. The harness module owns the
// schema-aware writer + regression check; the binary's job is argv
// parsing and exit-code mapping.

const PROTOCOL_OVERHEAD_USAGE: &str = "\
atelier protocol-overhead — measure §2 emission-strategy overhead

USAGE:
    atelier protocol-overhead [OPTIONS]

OPTIONS:
    --workspace <PATH>           Project root (default: current dir).
                                 Used to resolve --fixtures-dir / --out
                                 when those are not absolute.
    --fixtures-dir <PATH>        Override the fixture directory
                                 (default: <workspace>/tests/protocol/fixtures).
    --out <PATH>                 Override the output file path
                                 (default: <workspace>/tests/protocol/overhead.json).
    --provider <NAME>            Provider name written to the report.
                                 Default: \"mock\".
    --model-id <ID>              Model id written to the report.
                                 Default: \"mock:protocol-overhead\".
    --check-regression           Compare current median_overhead_pct
                                 against the prior file's
                                 rolling_median.value and exit non-zero
                                 on drift > --regression-threshold-pct.
                                 The output file is still rewritten.
    --regression-threshold-pct <N>  Drift percentage that constitutes a
                                 regression. Default: 10.0.
    -h, --help                   Print this message.
";

struct OverheadArgs {
    workspace: Option<PathBuf>,
    fixtures_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    provider: Option<String>,
    model_id: Option<String>,
    check_regression: bool,
    regression_threshold_pct: Option<f64>,
}

impl OverheadArgs {
    fn empty() -> Self {
        Self {
            workspace: None,
            fixtures_dir: None,
            out: None,
            provider: None,
            model_id: None,
            check_regression: false,
            regression_threshold_pct: None,
        }
    }
}

fn run_protocol_overhead(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut out = OverheadArgs::empty();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{PROTOCOL_OVERHEAD_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--workspace" => match args.next() {
                Some(v) => out.workspace = Some(PathBuf::from(v)),
                None => {
                    eprintln!("atelier protocol-overhead: --workspace requires a path");
                    return ExitCode::from(2);
                }
            },
            "--fixtures-dir" => match args.next() {
                Some(v) => out.fixtures_dir = Some(PathBuf::from(v)),
                None => {
                    eprintln!("atelier protocol-overhead: --fixtures-dir requires a path");
                    return ExitCode::from(2);
                }
            },
            "--out" => match args.next() {
                Some(v) => out.out = Some(PathBuf::from(v)),
                None => {
                    eprintln!("atelier protocol-overhead: --out requires a path");
                    return ExitCode::from(2);
                }
            },
            "--provider" => match args.next() {
                Some(v) => out.provider = Some(v),
                None => {
                    eprintln!("atelier protocol-overhead: --provider requires a name");
                    return ExitCode::from(2);
                }
            },
            "--model-id" => match args.next() {
                Some(v) => out.model_id = Some(v),
                None => {
                    eprintln!("atelier protocol-overhead: --model-id requires an id");
                    return ExitCode::from(2);
                }
            },
            "--check-regression" => out.check_regression = true,
            "--regression-threshold-pct" => match args.next().and_then(|s| s.parse::<f64>().ok()) {
                Some(n) if n.is_finite() && n >= 0.0 => out.regression_threshold_pct = Some(n),
                _ => {
                    eprintln!(
                        "atelier protocol-overhead: --regression-threshold-pct requires a non-negative number"
                    );
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("atelier protocol-overhead: unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    let workspace = match out
        .workspace
        .clone()
        .map(Ok)
        .unwrap_or_else(env::current_dir)
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("atelier protocol-overhead: cannot read current directory: {e}");
            return ExitCode::from(1);
        }
    };
    let mut config = overhead::OverheadConfig::with_workspace(&workspace);
    if let Some(p) = out.fixtures_dir {
        config.fixtures_dir = p;
    }
    if let Some(p) = out.out {
        config.out_path = p;
    }
    if let Some(p) = out.provider {
        config.provider = p;
    }
    if let Some(m) = out.model_id {
        config.model_id = m;
    }
    if let Some(t) = out.regression_threshold_pct {
        config.regression_threshold_pct = t;
    }
    config.check_regression = out.check_regression;

    match overhead::run(&config) {
        Ok(report) => {
            println!(
                "atelier protocol-overhead: wrote {} (providers: {}, version {})",
                config.out_path.display(),
                report.providers.len(),
                report.version
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atelier protocol-overhead: {e}");
            // Regression is the load-bearing failure path for the
            // nightly job. Distinguish it with a dedicated exit code so
            // the workflow can branch on signal vs. infrastructure.
            match e {
                overhead::OverheadError::Regression { .. } => ExitCode::from(3),
                _ => ExitCode::from(1),
            }
        }
    }
}

// ---------- v60.20 `atelier find` subcommand ----------

const FIND_USAGE: &str = "\
atelier find — query the most recent (or named) session for what the
agent already knows about a given file path. Appends a FindProbe to
the session's `find_probes.json` so the §5 UX target's
median-elapsed-ms can be computed.

USAGE:
    atelier find --path <PATH> [OPTIONS]

OPTIONS:
    --path <PATH>          Path to search for (required). Substring-matched
                           against every conversation entry's serialized JSON.
    --workspace <PATH>     Workspace root. Default: current directory.
    --session <UUID>       Specific session UUID. Default: the most recently
                           modified session directory under
                           `<workspace>/.atelier/sessions/`.
    --dry-run              Do NOT append a probe to find_probes.json. Used by
                           the canonical t13 fixture so `make check` runs
                           don't bloat the seeded probe log.
    -h, --help             Show this help.

EXIT CODES:
    0   query completed (matches may be 0 — that is still success)
    1   query errored (workspace missing / session.json malformed)
    2   bad argument (missing --path, unknown flag)

`atelier find` exits 0 when the workspace has no sessions yet — a
fresh repo doesn't have an agent to query, and that's not an error.
";

#[derive(Default)]
struct FindCliArgs {
    path: Option<String>,
    workspace: Option<PathBuf>,
    session: Option<String>,
    dry_run: bool,
}

fn run_find(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut out = FindCliArgs::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{FIND_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--path" => match args.next() {
                Some(v) => out.path = Some(v),
                None => {
                    eprintln!("atelier find: --path requires a value");
                    return ExitCode::from(2);
                }
            },
            "--workspace" => match args.next() {
                Some(v) => out.workspace = Some(PathBuf::from(v)),
                None => {
                    eprintln!("atelier find: --workspace requires a path");
                    return ExitCode::from(2);
                }
            },
            "--session" => match args.next() {
                Some(v) => out.session = Some(v),
                None => {
                    eprintln!("atelier find: --session requires a UUID");
                    return ExitCode::from(2);
                }
            },
            "--dry-run" => out.dry_run = true,
            other => {
                eprintln!("atelier find: unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(path) = out.path else {
        eprintln!("atelier find: --path is required\n");
        eprintln!("{FIND_USAGE}");
        return ExitCode::from(2);
    };

    let workspace = match out
        .workspace
        .clone()
        .map(Ok)
        .unwrap_or_else(env::current_dir)
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("atelier find: cannot read current directory: {e}");
            return ExitCode::from(1);
        }
    };

    let session = match out.session.as_deref() {
        Some(s) => match uuid::Uuid::parse_str(s) {
            Ok(u) => Some(u),
            Err(_) => {
                eprintln!("atelier find: --session {s:?} is not a valid UUID");
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    let query = atelier_cli::find::FindQuery {
        workspace,
        path: path.clone(),
        session,
        dry_run: out.dry_run,
    };
    match atelier_cli::find::find(query) {
        Ok(outcome) => {
            match outcome.session_uuid {
                None => {
                    println!("atelier find: no session found under <workspace>/.atelier/sessions/ — nothing to query yet.");
                }
                Some(uuid) => {
                    if outcome.matches.is_empty() {
                        println!(
                            "atelier find: 0 matches for {path:?} in session {uuid} ({} ms)",
                            outcome.elapsed_ms
                        );
                    } else {
                        println!(
                            "atelier find: {} matches for {path:?} in session {uuid} ({} ms)",
                            outcome.matches.len(),
                            outcome.elapsed_ms
                        );
                        for m in &outcome.matches {
                            println!("  turn {} [{}]: {}", m.turn_index, m.role, m.excerpt);
                        }
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atelier find: {e}");
            ExitCode::from(1)
        }
    }
}

/// Read the prompt from (in order): positional argv words,
/// `--prompt-file`, or stdin. Rejects an empty prompt up-front so the
/// Runner doesn't have to.
fn read_prompt_from_cli(cli: &CliArgs) -> Result<String, ExitCode> {
    let prompt = if !cli.prompt_args.is_empty() {
        cli.prompt_args.join(" ")
    } else {
        // No positional prompt — read from --prompt-file or stdin.
        let p = cli
            .prompt_file
            .as_deref()
            .filter(|p| p.to_str() != Some("-"));
        match runner::read_prompt(p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("atelier run: cannot read prompt: {e}");
                return Err(ExitCode::from(1));
            }
        }
    };
    if prompt.trim().is_empty() {
        eprintln!("atelier run: prompt is empty");
        return Err(ExitCode::from(2));
    }
    Ok(prompt)
}

#[cfg(test)]
mod trust_boundary_tests {
    use super::*;

    fn openai_profile(base_url: &str) -> ProviderProfile {
        ProviderProfile {
            provider: Some(ProviderKind::OpenaiCompat),
            model: Some("local:test".to_string()),
            base_url: Some(base_url.to_string()),
            api_key: None,
            cache_prompt: None,
        }
    }

    #[test]
    fn explicit_cli_base_url_is_allowed_by_provider_resolution() {
        let mut cli = CliArgs::empty();
        cli.provider = Some("openai-compat".to_string());
        cli.model = Some("local:test".to_string());
        cli.base_url = Some("https://custom.example/v1".to_string());

        let choice = resolve_provider_choice(&cli, None).expect("explicit endpoint is user intent");
        match choice {
            runner::ProviderChoice::OpenAiCompat { base_url, .. } => {
                assert_eq!(base_url.as_deref(), Some("https://custom.example/v1"));
            }
            _ => panic!("unexpected provider choice"),
        }
    }

    #[test]
    fn core_trust_boundary_rejects_credentialed_profile_base_url() {
        let err = atelier_core::provider_profile_base_url_may_receive_credential(
            Some("https://evil.example/v1"),
            false,
            true,
        )
        .expect_err("repo profile must not silently receive credentials");
        assert!(err.contains("allowlist"), "got: {err}");
    }

    #[test]
    fn profile_base_url_resolution_still_accepts_allowlisted_hosts() {
        let cli = CliArgs::empty();
        let profile = openai_profile("https://api.openai.com/v1");

        let choice = resolve_provider_choice(&cli, Some(&profile)).expect("allowlisted profile");
        match choice {
            runner::ProviderChoice::OpenAiCompat { base_url, .. } => {
                assert_eq!(base_url.as_deref(), Some("https://api.openai.com/v1"));
            }
            _ => panic!("unexpected provider choice"),
        }
    }
}
