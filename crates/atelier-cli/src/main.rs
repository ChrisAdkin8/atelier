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
use atelier_cli::runner;

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

`atelier run` options:
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

fn run_run(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut provider = "mock".to_string();
    let mut model: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut max_turns: Option<usize> = None;
    let mut prompt_file: Option<PathBuf> = None;
    let mut prompt_args: Vec<String> = Vec::new();

    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--provider" => match args.next() {
                Some(v) => provider = v,
                None => {
                    eprintln!("atelier run: --provider requires a value");
                    return ExitCode::from(2);
                }
            },
            "--model" => match args.next() {
                Some(v) => model = Some(v),
                None => {
                    eprintln!("atelier run: --model requires a value");
                    return ExitCode::from(2);
                }
            },
            "--base-url" => match args.next() {
                Some(v) => base_url = Some(v),
                None => {
                    eprintln!("atelier run: --base-url requires a URL");
                    return ExitCode::from(2);
                }
            },
            "--workspace" => match args.next() {
                Some(v) => workspace = Some(PathBuf::from(v)),
                None => {
                    eprintln!("atelier run: --workspace requires a path");
                    return ExitCode::from(2);
                }
            },
            "--max-turns" => match args.next().and_then(|s| s.parse::<usize>().ok()) {
                Some(n) => max_turns = Some(n),
                None => {
                    eprintln!("atelier run: --max-turns requires a positive integer");
                    return ExitCode::from(2);
                }
            },
            "--prompt-file" => match args.next() {
                Some(v) => prompt_file = Some(PathBuf::from(v)),
                None => {
                    eprintln!("atelier run: --prompt-file requires a path");
                    return ExitCode::from(2);
                }
            },
            // Everything else is treated as positional prompt text.
            _ => prompt_args.push(a),
        }
    }

    let provider_choice = match provider.as_str() {
        "mock" => runner::ProviderChoice::Mock {
            responses: Vec::new(),
        },
        "anthropic" => {
            let model_id = model.unwrap_or_else(|| "anthropic:claude-opus-4-7".to_string());
            if !model_id.starts_with("anthropic:") {
                eprintln!(
                    "atelier run: --model for --provider anthropic must be prefixed \
                     `anthropic:` (got {model_id:?}); e.g. anthropic:claude-opus-4-7"
                );
                return ExitCode::from(2);
            }
            if base_url.is_some() {
                eprintln!("atelier run: --base-url is only valid with --provider openai-compat");
                return ExitCode::from(2);
            }
            runner::ProviderChoice::Anthropic { model_id }
        }
        "openai-compat" => {
            let Some(model_id) = model else {
                eprintln!(
                    "atelier run: --provider openai-compat requires --model <ID> \
                     (e.g. `local:llama3:8b` or `gpt-4o-mini`); the id is sent \
                     verbatim to the server"
                );
                return ExitCode::from(2);
            };
            runner::ProviderChoice::OpenAiCompat { model_id, base_url }
        }
        other => {
            eprintln!(
                "atelier run: unknown provider {other:?}. Supported: `mock`, \
                 `anthropic`, `openai-compat`. (`bedrock`, `vertex` land in Phase E/F.)"
            );
            return ExitCode::from(2);
        }
    };

    let workspace = match workspace {
        Some(p) => p,
        None => match env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("atelier run: cannot read current directory: {e}");
                return ExitCode::from(1);
            }
        },
    };

    let prompt = if !prompt_args.is_empty() {
        prompt_args.join(" ")
    } else {
        // No positional prompt — read from --prompt-file or stdin.
        let p = prompt_file.as_deref().filter(|p| p.to_str() != Some("-"));
        match runner::read_prompt(p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("atelier run: cannot read prompt: {e}");
                return ExitCode::from(1);
            }
        }
    };

    if prompt.trim().is_empty() {
        eprintln!("atelier run: prompt is empty");
        return ExitCode::from(2);
    }

    // For the mock provider with no scripted responses, the loop has
    // nothing to do — the adapter would return NotConfigured on the first
    // chat call. v0 binary use is the docs walkthrough; the integration
    // tests construct `Runner` directly with `Mock { responses }` to
    // script real turns.
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
