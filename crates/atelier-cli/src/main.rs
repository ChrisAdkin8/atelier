//! `atelier` — command-line entry point.
//!
//! Currently implements one subcommand: `init` (spec §11 project bootstrap).
//! Future subcommands (per spec §11 credential storage): `login`, `logout`,
//! `rotate`, `whoami`.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
atelier — coding harness CLI

USAGE:
    atelier <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS:
    init [PATH]    Bootstrap an Atelier project at PATH (default: current dir).
                   Idempotent. See spec §11.

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
