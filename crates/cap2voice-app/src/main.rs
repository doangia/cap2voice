//! Entrypoint for the `cap2voice` Windows desktop application.

use cap2voice_app::config;
use std::process::ExitCode;

fn main() -> ExitCode {
    let config_path = match config::default_config_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("cap2voice: {err}");
            return ExitCode::FAILURE;
        }
    };

    match config::load_config_from(&config_path) {
        Ok(outcome) => {
            if let Some(warning) = outcome.warning {
                eprintln!(
                    "cap2voice: config warning ({}); backed up to {}",
                    warning.reason,
                    warning.backup_path.display()
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("cap2voice: failed to load config: {err}");
            ExitCode::FAILURE
        }
    }
}
