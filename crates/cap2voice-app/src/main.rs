#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Entrypoint for the `cap2voice` Windows desktop application.

use std::process::ExitCode;

fn main() -> ExitCode {
    match cap2voice_app::ui::run_app() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cap2voice: lỗi giao diện desktop: {err}");
            ExitCode::FAILURE
        }
    }
}
