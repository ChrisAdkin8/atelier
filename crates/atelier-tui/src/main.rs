use std::process::ExitCode;

fn main() -> ExitCode {
    match atelier_tui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atelier-tui: {e}");
            ExitCode::from(1)
        }
    }
}
