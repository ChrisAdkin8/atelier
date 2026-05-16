// Suppress the console window on Windows in release builds — Tauri shells
// don't want a stray cmd.exe popping up alongside the webview.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    atelier_gui::run();
}
