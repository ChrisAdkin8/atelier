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

mod runner;

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
    --provider <NAME>              Adapter to use. `mock` (default) exercises
                                   the loop with no network. `anthropic`
                                   talks to the Messages API and reads
                                   `ANTHROPIC_API_KEY` from the environment;
                                   pair with `--model <id>` to pick a
                                   specific model.
    --model <ID>                   Model id for the chosen provider, e.g.
                                   `anthropic:claude-opus-4-7`. Required
                                   for `--provider anthropic`; ignored
                                   for `mock`.
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
            runner::ProviderChoice::Anthropic { model_id }
        }
        other => {
            eprintln!(
                "atelier run: unknown provider {other:?}. Supported: `mock`, \
                 `anthropic`. (`bedrock`, `vertex`, `ollama` land in Phase E/F.)"
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
