//! Typed application configuration: section structs, validation, and
//! TOML persistence (see `io`) plus Windows per-user autostart (see
//! `autostart`).

pub mod autostart;
pub mod io;

pub use autostart::{format_autostart_command, get_windows_autostart, set_windows_autostart};
pub use io::{
    default_cache_dir, default_config_path, default_log_dir, load_config_from, save_config_to,
};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Physical screen coordinates of the selected subtitle capture region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRegion {
    pub monitor_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Last saved screen position of the floating overlay window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowPosition {
    pub x: f32,
    pub y: f32,
}

/// General capture, OCR path, logging, and window behavior settings (`Chung` tab).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub capture_fps: u32,
    pub region: Option<CaptureRegion>,
    pub wxocr_dll_path: Option<PathBuf>,
    pub weixin_dir_path: Option<PathBuf>,
    pub enable_subtitle_log: bool,
    pub subtitle_log_dir: Option<PathBuf>,
    pub close_to_tray: bool,
    pub start_with_windows: bool,
    pub start_minimized_to_tray: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            capture_fps: 10,
            region: None,
            wxocr_dll_path: None,
            weixin_dir_path: None,
            enable_subtitle_log: true,
            subtitle_log_dir: None,
            close_to_tray: true,
            start_with_windows: false,
            start_minimized_to_tray: false,
        }
    }
}

/// Translation API bridge and pre-OCR/deduplication thresholds (`Dịch` tab).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslationConfig {
    pub server_port: u16,
    pub dedupe_threshold: f32,
    pub pixel_diff_threshold: f32,
    pub blank_reset_frames: u32,
}

impl Default for TranslationConfig {
    fn default() -> Self {
        Self {
            server_port: 18765,
            dedupe_threshold: 0.93,
            pixel_diff_threshold: 0.02,
            blank_reset_frames: 5,
        }
    }
}

/// Vietnamese text-to-speech voice and playback settings (`Giọng đọc` tab).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub voice: String,
    pub volume: f32,
    pub speech_rate: String,
    pub auto_read: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            voice: "vi-VN-NamMinhNeural".to_string(),
            volume: 0.75,
            speech_rate: "+0%".to_string(),
            auto_read: true,
        }
    }
}

/// Overlay typography, opacity, visibility, and colors (`Hiển thị` tab).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub font_size_px: f32,
    pub overlay_opacity: f32,
    pub show_chinese_line: bool,
    pub show_vietnamese_line: bool,
    pub always_on_top: bool,
    pub chinese_color_hex: String,
    pub vietnamese_color_hex: String,
    pub overlay_position: Option<WindowPosition>,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            font_size_px: 18.0,
            overlay_opacity: 0.85,
            show_chinese_line: true,
            show_vietnamese_line: true,
            always_on_top: true,
            chinese_color_hex: "#FFFFFF".to_string(),
            vietnamese_color_hex: "#4ADE80".to_string(),
            overlay_position: None,
        }
    }
}

/// Global keyboard shortcuts (`Phím tắt` tab).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub toggle_start_stop: String,
    pub select_region: String,
    pub toggle_overlay: String,
    pub toggle_mute: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            toggle_start_stop: "Ctrl+Alt+S".to_string(),
            select_region: "Ctrl+Alt+R".to_string(),
            toggle_overlay: "Ctrl+Alt+H".to_string(),
            toggle_mute: "Ctrl+Alt+M".to_string(),
        }
    }
}

/// Top-level `cap2voice` application configuration serialized as TOML.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub translation: TranslationConfig,
    pub voice: VoiceConfig,
    pub display: DisplayConfig,
    pub hotkeys: HotkeyConfig,
}

/// Typed errors emitted by configuration and autostart operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConfigError {
    #[error("APPDATA environment variable is not set")]
    MissingAppDataDir,

    #[error("invalid configuration value: {0}")]
    Validation(String),

    #[error("I/O error on {}: {detail}", path.display())]
    Io { path: PathBuf, detail: String },

    #[error("failed to serialize configuration to TOML: {0}")]
    Serialize(String),

    #[error(
        "failed to back up corrupt configuration {} to {}: {detail}",
        source_path.display(),
        backup_path.display()
    )]
    BackupFailed {
        source_path: PathBuf,
        backup_path: PathBuf,
        detail: String,
    },

    #[error("Windows registry {action} failed with Win32 error code {code}")]
    Registry { action: &'static str, code: u32 },

    #[error("Windows autostart registry is only supported on Windows")]
    UnsupportedPlatform,
}

/// Non-fatal warning returned when a corrupt config file is backed up and defaults loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoadWarning {
    pub backup_path: PathBuf,
    pub reason: String,
}

/// Result of loading `config.toml` from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLoadOutcome {
    pub config: AppConfig,
    pub warning: Option<ConfigLoadWarning>,
}

fn validate_f32_range(name: &str, value: f32, min: f32, max: f32) -> Result<(), ConfigError> {
    if !value.is_finite() || value < min || value > max {
        return Err(ConfigError::Validation(format!(
            "{name} must be between {min} and {max} (got {value})"
        )));
    }
    Ok(())
}

fn is_valid_speech_rate(rate: &str) -> bool {
    let Some(rest) = rate.strip_prefix(['+', '-']) else {
        return false;
    };
    let Some(digits) = rest.strip_suffix('%') else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_valid_hex_color(hex: &str) -> bool {
    hex.len() == 7 && hex.starts_with('#') && hex[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

impl AppConfig {
    /// Validate all numeric ranges, color strings, and required fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=60).contains(&self.general.capture_fps) {
            return Err(ConfigError::Validation(format!(
                "capture_fps must be between 1 and 60 (got {})",
                self.general.capture_fps
            )));
        }
        if let Some(region) = &self.general.region {
            if region.width < 20 || region.height < 16 {
                return Err(ConfigError::Validation(format!(
                    "capture region must be at least 20x16 px (got {}x{})",
                    region.width, region.height
                )));
            }
        }
        if self.translation.server_port < 1024 {
            return Err(ConfigError::Validation(format!(
                "server_port must be >= 1024 (got {})",
                self.translation.server_port
            )));
        }
        validate_f32_range(
            "dedupe_threshold",
            self.translation.dedupe_threshold,
            0.50,
            1.00,
        )?;
        validate_f32_range(
            "pixel_diff_threshold",
            self.translation.pixel_diff_threshold,
            0.001,
            0.50,
        )?;
        if self.translation.blank_reset_frames == 0 {
            return Err(ConfigError::Validation(
                "blank_reset_frames must be >= 1".to_string(),
            ));
        }
        if self.voice.voice.trim().is_empty() {
            return Err(ConfigError::Validation(
                "voice identifier must not be empty".to_string(),
            ));
        }
        validate_f32_range("volume", self.voice.volume, 0.0, 1.0)?;
        if !is_valid_speech_rate(&self.voice.speech_rate) {
            return Err(ConfigError::Validation(format!(
                "speech_rate must match [+-]<digits>% (got '{}')",
                self.voice.speech_rate
            )));
        }
        validate_f32_range("font_size_px", self.display.font_size_px, 12.0, 40.0)?;
        validate_f32_range("overlay_opacity", self.display.overlay_opacity, 0.30, 1.00)?;
        if !is_valid_hex_color(&self.display.chinese_color_hex) {
            return Err(ConfigError::Validation(format!(
                "chinese_color_hex must be #RRGGBB (got '{}')",
                self.display.chinese_color_hex
            )));
        }
        if !is_valid_hex_color(&self.display.vietnamese_color_hex) {
            return Err(ConfigError::Validation(format!(
                "vietnamese_color_hex must be #RRGGBB (got '{}')",
                self.display.vietnamese_color_hex
            )));
        }
        Ok(())
    }

    /// Resolve the active subtitle log directory, using `%APPDATA%\cap2voice\logs` by default.
    pub fn effective_log_dir(&self) -> Result<PathBuf, ConfigError> {
        match &self.general.subtitle_log_dir {
            Some(dir) => Ok(dir.clone()),
            None => default_log_dir(),
        }
    }

    /// Resolve the translation + TTS cache directory, using
    /// `%APPDATA%\cap2voice\cache` by default (A3 / gh #2). There is no
    /// separate config option for the cache dir; it always lives next to
    /// the logs under the same `%APPDATA%\cap2voice` root.
    pub fn effective_cache_dir(&self) -> Result<PathBuf, ConfigError> {
        default_cache_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, CaptureRegion, ConfigError};

    #[test]
    fn default_config_matches_spec_v1_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.validate(), Ok(()));

        assert_eq!(cfg.general.capture_fps, 10);
        assert_eq!(cfg.general.region, None);
        assert!(cfg.general.enable_subtitle_log);
        assert!(cfg.general.close_to_tray);
        assert!(!cfg.general.start_with_windows);
        assert!(!cfg.general.start_minimized_to_tray);

        assert_eq!(cfg.translation.server_port, 18765);
        assert!((cfg.translation.dedupe_threshold - 0.93).abs() < f32::EPSILON);
        assert!((cfg.translation.pixel_diff_threshold - 0.02).abs() < f32::EPSILON);
        assert_eq!(cfg.translation.blank_reset_frames, 5);

        assert_eq!(cfg.voice.voice, "vi-VN-NamMinhNeural");
        assert!((cfg.voice.volume - 0.75).abs() < f32::EPSILON);
        assert_eq!(cfg.voice.speech_rate, "+0%");
        assert!(cfg.voice.auto_read);

        assert!((cfg.display.font_size_px - 18.0).abs() < f32::EPSILON);
        assert!((cfg.display.overlay_opacity - 0.85).abs() < f32::EPSILON);
        assert!(cfg.display.show_chinese_line);
        assert!(cfg.display.show_vietnamese_line);
        assert!(cfg.display.always_on_top);
        assert_eq!(cfg.display.chinese_color_hex, "#FFFFFF");
        assert_eq!(cfg.display.vietnamese_color_hex, "#4ADE80");

        assert_eq!(cfg.hotkeys.toggle_start_stop, "Ctrl+Alt+S");
        assert_eq!(cfg.hotkeys.select_region, "Ctrl+Alt+R");
        assert_eq!(cfg.hotkeys.toggle_overlay, "Ctrl+Alt+H");
        assert_eq!(cfg.hotkeys.toggle_mute, "Ctrl+Alt+M");
    }

    #[test]
    fn validate_rejects_invalid_bounds() {
        let mut cases: Vec<(&str, AppConfig)> = Vec::new();

        let mut c = AppConfig::default();
        c.general.capture_fps = 0;
        cases.push(("zero fps", c));

        let mut c = AppConfig::default();
        c.general.region = Some(CaptureRegion {
            monitor_id: "M1".to_string(),
            x: 0,
            y: 0,
            width: 19,
            height: 16,
        });
        cases.push(("region too narrow", c));

        let mut c = AppConfig::default();
        c.translation.server_port = 80;
        cases.push(("privileged port", c));

        let mut c = AppConfig::default();
        c.translation.dedupe_threshold = 1.2;
        cases.push(("dedupe > 1.0", c));

        let mut c = AppConfig::default();
        c.translation.pixel_diff_threshold = 0.0;
        cases.push(("zero pixel diff", c));

        let mut c = AppConfig::default();
        c.translation.blank_reset_frames = 0;
        cases.push(("zero blank reset frames", c));

        let mut c = AppConfig::default();
        c.voice.volume = -0.1;
        cases.push(("negative volume", c));

        let mut c = AppConfig::default();
        c.voice.speech_rate = "fast".to_string();
        cases.push(("invalid speech rate", c));

        let mut c = AppConfig::default();
        c.voice.voice = "   ".to_string();
        cases.push(("blank voice", c));

        let mut c = AppConfig::default();
        c.display.font_size_px = 8.0;
        cases.push(("font size below 12px", c));

        let mut c = AppConfig::default();
        c.display.overlay_opacity = 0.1;
        cases.push(("opacity below 0.3", c));

        let mut c = AppConfig::default();
        c.display.chinese_color_hex = "white".to_string();
        cases.push(("invalid hex color", c));

        for (label, cfg) in cases {
            let res = cfg.validate();
            assert!(
                matches!(res, Err(ConfigError::Validation(_))),
                "expected Validation error for '{label}', got {res:?}"
            );
        }
    }
}
