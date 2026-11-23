//! Application bootstrap: config load, native window options, native
//! engine construction, and shutdown helpers (D1+A5: no Python server).

use super::overlay::{apply_exclude_from_capture_hwnd, DisplayAffinityError, DEFAULT_OVERLAY_SIZE};
use crate::config::{default_config_path, load_config_from, AppConfig};
use crate::pipeline::PipelineHandle;
use crate::translate::{
    default_dict_dir, default_model_dir, load_default_dictionaries, SwitchableTranslator,
};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
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

/// Build the shared native translator (D1+A5): Vietphrase dictionaries from
/// `%APPDATA%\cap2voice\vietphrase\data` when present, Helsinki model
/// from `%APPDATA%\cap2voice\models\opus-mt-zh-vi` (lazy-loaded in the
/// background when it is the configured engine). Returns `None` plus a
/// user-visible warning when NEITHER engine is available; a missing
/// Vietphrase dictionary set alone only warns (Helsinki still works).
pub(crate) fn build_native_translator(
    config: &AppConfig,
) -> (Option<Arc<SwitchableTranslator>>, Vec<String>) {
    let mut warnings = Vec::new();
    let dicts = match load_default_dictionaries() {
        Ok(dicts) => Some(dicts),
        Err(err) => {
            let dir = default_dict_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| "<không xác định>".to_string());
            warnings.push(format!(
                "Không tải được từ điển Vietphrase tại {dir} ({err}) — engine Vietphrase không khả dụng"
            ));
            None
        }
    };
    let model_dir = match default_model_dir() {
        Ok(dir) => dir,
        Err(err) => {
            warnings.push(format!("Không xác định được thư mục model Helsinki: {err}"));
            PathBuf::new()
        }
    };
    match SwitchableTranslator::new(dicts, model_dir, config.translation.engine) {
        Ok(translator) => (Some(translator), warnings),
        Err(err) => {
            warnings.push(format!("Không có engine dịch khả dụng: {err}"));
            (None, warnings)
        }
    }
}

pub(crate) fn stop_pipeline_with_timeout(
    runtime: &tokio::runtime::Runtime,
    handle: PipelineHandle,
    timeout: Duration,
) -> bool {
    runtime.block_on(async move { tokio::time::timeout(timeout, handle.stop()).await.is_ok() })
}
