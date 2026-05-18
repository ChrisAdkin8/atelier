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

use atelier_core::config::{
    LoadedConfig, ProbePolicyName, ProviderKind, ProviderProfile, ProvidersConfig,
};

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

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
                Some(n) => out.max_turns = Some(n),
                None => return missing_value("--max-turns", "positive integer"),
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

    let mut runner =
        match runner::Runner::new(workspace, provider_choice, runner::EventSink::Stdout) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("atelier run: {e}");
                return ExitCode::from(1);
            }
        };
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

    match rt.block_on(runner.run(prompt)) {
        Ok(report) => {
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
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atelier run: {e}");
            ExitCode::from(1)
        }
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
            if base_url.is_some() {
                return Err("base_url is only valid with provider `openai-compat`".into());
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
            if base_url.is_some() {
                return Err("base_url is only valid with provider `openai-compat`".into());
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
            Ok(runner::ProviderChoice::OpenAiCompat { model_id, base_url })
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
