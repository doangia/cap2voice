//! Application bootstrap: config load, native window options, server
//! startup, and shutdown helpers.

use super::overlay::{apply_exclude_from_capture_hwnd, DisplayAffinityError, DEFAULT_OVERLAY_SIZE};
use crate::api::{discover_server_dir, ServerProcess, ServerProcessConfig, ServerProcessError};
use crate::config::{default_config_path, load_config_from, AppConfig};
use crate::pipeline::PipelineHandle;
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

pub(crate) fn configured_window_level(config: &AppConfig) -> egui::WindowLevel {
    if config.display.always_on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    }
}

/// Build `eframe::NativeOptions` for the borderless, transparent, always-on-top overlay.
pub fn build_native_options(config: &AppConfig) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("cap2voice")
        .with_inner_size(DEFAULT_OVERLAY_SIZE)
        .with_min_inner_size([460.0, 140.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_visible(!config.general.start_minimized_to_tray)
        .with_window_level(configured_window_level(config));

    if let Some(pos) = &config.display.overlay_position {
        viewport = viewport.with_position(egui::Pos2::new(pos.x, pos.y));
    }

    eframe::NativeOptions {
        viewport,
        ..Default::default()
    }
}

/// Format `ServerProcessError` into a clear, actionable Vietnamese banner for the user.
pub fn format_server_startup_error(err: &ServerProcessError) -> String {
    match err {
        ServerProcessError::ExitedEarly { stderr, .. }
            if stderr.contains("ModuleNotFoundError") || stderr.contains("No module named") =>
        {
            format!(
                "Không khởi động được server dịch/TTS local: {err} — hãy cài thư viện từ server/requirements.txt"
            )
        }
        _ => format!("Không khởi động được server dịch/TTS local: {err}"),
    }
}

fn resolve_server_directory() -> Result<PathBuf, ServerProcessError> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if let Ok(dir) = discover_server_dir(parent) {
                return Ok(dir);
            }
        }
    }
    let cwd = std::env::current_dir().map_err(|err| ServerProcessError::ServerDirNotFound {
        searched: format!("current_dir error: {err}"),
    })?;
    discover_server_dir(&cwd)
}

#[cfg(windows)]
pub(crate) fn apply_wda_to_handle(
    handle: &impl raw_window_handle::HasWindowHandle,
) -> Result<(), DisplayAffinityError> {
    use raw_window_handle::RawWindowHandle;
    let window_handle = handle
        .window_handle()
        .map_err(|_| DisplayAffinityError::MissingHwnd)?;
    match window_handle.as_raw() {
        RawWindowHandle::Win32(win32) => apply_exclude_from_capture_hwnd(win32.hwnd.get()),
        _ => Err(DisplayAffinityError::UnsupportedPlatform),
    }
}

#[cfg(not(windows))]
pub(crate) fn apply_wda_to_handle(
    _handle: &impl raw_window_handle::HasWindowHandle,
) -> Result<(), DisplayAffinityError> {
    Err(DisplayAffinityError::UnsupportedPlatform)
}

pub(crate) struct AppBootstrap {
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) config: AppConfig,
    pub(crate) startup_warnings: Vec<String>,
}

pub(crate) fn load_bootstrap() -> AppBootstrap {
    let mut startup_warnings = Vec::new();
    let config_path = match default_config_path() {
        Ok(path) => Some(path),
        Err(err) => {
            startup_warnings.push(format!(
                "Không xác định được thư mục cấu hình (%APPDATA%): {err}"
            ));
            None
        }
    };

    let config = if let Some(path) = &config_path {
        match load_config_from(path) {
            Ok(outcome) => {
                if let Some(warn) = outcome.warning {
                    startup_warnings.push(format!(
                        "Tệp cấu hình bị hỏng (đã sao lưu tại {}): {}",
                        warn.backup_path.display(),
                        warn.reason
                    ));
                }
                outcome.config
            }
            Err(err) => {
                startup_warnings.push(format!("Lỗi đọc config.toml: {err}"));
                AppConfig::default()
            }
        }
    } else {
        AppConfig::default()
    };

    AppBootstrap {
        config_path,
        config,
        startup_warnings,
    }
}

pub(crate) fn spawn_server_startup(
    runtime: &tokio::runtime::Runtime,
    port: u16,
    ctx: egui::Context,
) -> mpsc::Receiver<Result<ServerProcess, ServerProcessError>> {
    let (server_tx, server_rx) = mpsc::sync_channel(1);
    runtime.spawn(async move {
        let res = match resolve_server_directory() {
            Ok(server_dir) => {
                ServerProcess::start(ServerProcessConfig::for_port(port, server_dir)).await
            }
            Err(err) => Err(err),
        };
        let _ = server_tx.send(res);
        ctx.request_repaint();
    });
    server_rx
}

pub(crate) fn stop_pipeline_with_timeout(
    runtime: &tokio::runtime::Runtime,
    handle: PipelineHandle,
    timeout: Duration,
) -> bool {
    runtime.block_on(async move { tokio::time::timeout(timeout, handle.stop()).await.is_ok() })
}
