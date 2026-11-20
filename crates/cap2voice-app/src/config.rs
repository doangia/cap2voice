//! Typed application configuration, atomic TOML persistence, corrupt-file backup,
//! and Windows per-user autostart registry integration.

use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
}

fn appdata_root() -> Result<PathBuf, ConfigError> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Err(ConfigError::MissingAppDataDir);
    };
    if appdata.is_empty() {
        return Err(ConfigError::MissingAppDataDir);
    }
    Ok(PathBuf::from(appdata).join("cap2voice"))
}

/// Return `%APPDATA%\cap2voice\config.toml`.
pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    Ok(appdata_root()?.join("config.toml"))
}

/// Return `%APPDATA%\cap2voice\logs`.
pub fn default_log_dir() -> Result<PathBuf, ConfigError> {
    Ok(appdata_root()?.join("logs"))
}

fn backup_corrupt_file(path: &Path) -> Result<PathBuf, ConfigError> {
    let mut backup_os = path.as_os_str().to_os_string();
    backup_os.push(".bak");
    let backup_path = PathBuf::from(backup_os);
    std::fs::copy(path, &backup_path).map_err(|e| ConfigError::BackupFailed {
        source_path: path.to_path_buf(),
        backup_path: backup_path.clone(),
        detail: e.to_string(),
    })?;
    Ok(backup_path)
}

/// Load and validate `AppConfig` from `path`, backing up corrupt files to `*.bak`.
pub fn load_config_from(path: &Path) -> Result<ConfigLoadOutcome, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(ConfigLoadOutcome {
                config: AppConfig::default(),
                warning: None,
            });
        }
        Err(err) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                detail: err.to_string(),
            });
        }
    };

    match toml::from_str::<AppConfig>(&raw)
        .map_err(|e| e.to_string())
        .and_then(|cfg| {
            cfg.validate().map_err(|e| e.to_string())?;
            Ok(cfg)
        }) {
        Ok(config) => Ok(ConfigLoadOutcome {
            config,
            warning: None,
        }),
        Err(reason) => {
            let backup_path = backup_corrupt_file(path)?;
            Ok(ConfigLoadOutcome {
                config: AppConfig::default(),
                warning: Some(ConfigLoadWarning {
                    backup_path,
                    reason,
                }),
            })
        }
    }
}

static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_and_sync(tmp_path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let mut file = std::fs::File::create(tmp_path).map_err(|e| ConfigError::Io {
        path: tmp_path.to_path_buf(),
        detail: e.to_string(),
    })?;
    if let Err(err) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        return Err(ConfigError::Io {
            path: tmp_path.to_path_buf(),
            detail: err.to_string(),
        });
    }
    Ok(())
}

/// Validate and atomically persist `AppConfig` to `path` via a temp file + rename.
pub fn save_config_to(path: &Path, config: &AppConfig) -> Result<(), ConfigError> {
    config.validate()?;
    let serialized =
        toml::to_string_pretty(config).map_err(|e| ConfigError::Serialize(e.to_string()))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::Io {
                path: parent.to_path_buf(),
                detail: e.to_string(),
            })?;
        }
    }

    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp_path = path.with_file_name(format!("{file_name}.tmp.{}.{seq}", std::process::id()));

    if let Err(err) = write_and_sync(&tmp_path, serialized.as_bytes()) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            detail: err.to_string(),
        });
    }
    Ok(())
}

/// Format a quoted executable command string for Windows Run registry entries.
pub fn format_autostart_command(exe_path: &Path) -> Result<String, ConfigError> {
    let raw = exe_path.to_string_lossy();
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(
            "executable path for Windows autostart must not be empty".to_string(),
        ));
    }
    Ok(format!("\"{trimmed}\""))
}

#[cfg(windows)]
mod win_autostart {
    use super::{format_autostart_command, ConfigError};
    use std::path::Path;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "cap2voice";

    struct RegKeyGuard(HKEY);

    impl Drop for RegKeyGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    RegCloseKey(self.0);
                }
            }
        }
    }

    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open_run_key(sam: u32) -> Result<Option<RegKeyGuard>, ConfigError> {
        let subkey_w = to_wide_null(RUN_SUBKEY);
        let mut key: HKEY = std::ptr::null_mut();
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey_w.as_ptr(), 0, sam, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(ConfigError::Registry {
                action: "RegOpenKeyExW",
                code: status,
            });
        }
        Ok(Some(RegKeyGuard(key)))
    }

    fn create_run_key() -> Result<RegKeyGuard, ConfigError> {
        let subkey_w = to_wide_null(RUN_SUBKEY);
        let mut key: HKEY = std::ptr::null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(ConfigError::Registry {
                action: "RegCreateKeyExW",
                code: status,
            });
        }
        Ok(RegKeyGuard(key))
    }

    pub(super) fn set(enabled: bool, exe_path: &Path) -> Result<(), ConfigError> {
        let value_w = to_wide_null(VALUE_NAME);
        if !enabled {
            let Some(guard) = open_run_key(KEY_SET_VALUE)? else {
                return Ok(());
            };
            let del_status = unsafe { RegDeleteValueW(guard.0, value_w.as_ptr()) };
            if del_status == ERROR_SUCCESS || del_status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            return Err(ConfigError::Registry {
                action: "RegDeleteValueW",
                code: del_status,
            });
        }

        let cmd = format_autostart_command(exe_path)?;
        let cmd_w = to_wide_null(&cmd);
        let mut bytes = Vec::with_capacity(cmd_w.len() * 2);
        for unit in cmd_w {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let byte_len = u32::try_from(bytes.len()).map_err(|_| {
            ConfigError::Validation("autostart command exceeds registry size limit".to_string())
        })?;

        let guard = create_run_key()?;
        let set_status = unsafe {
            RegSetValueExW(
                guard.0,
                value_w.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                byte_len,
            )
        };
        if set_status != ERROR_SUCCESS {
            return Err(ConfigError::Registry {
                action: "RegSetValueExW",
                code: set_status,
            });
        }
        Ok(())
    }

    pub(super) fn get() -> Result<Option<String>, ConfigError> {
        let Some(guard) = open_run_key(KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let value_w = to_wide_null(VALUE_NAME);

        let mut reg_type: u32 = 0;
        let mut byte_len: u32 = 0;
        let size_status = unsafe {
            RegQueryValueExW(
                guard.0,
                value_w.as_ptr(),
                std::ptr::null_mut(),
                &mut reg_type,
                std::ptr::null_mut(),
                &mut byte_len,
            )
        };
        if size_status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if size_status != ERROR_SUCCESS && size_status != ERROR_MORE_DATA {
            return Err(ConfigError::Registry {
                action: "RegQueryValueExW(size)",
                code: size_status,
            });
        }
        if reg_type != REG_SZ && reg_type != REG_EXPAND_SZ {
            return Err(ConfigError::Registry {
                action: "RegQueryValueExW(type)",
                code: reg_type,
            });
        }

        let mut buf = vec![0u8; byte_len as usize];
        let read_status = unsafe {
            RegQueryValueExW(
                guard.0,
                value_w.as_ptr(),
                std::ptr::null_mut(),
                &mut reg_type,
                buf.as_mut_ptr(),
                &mut byte_len,
            )
        };
        if read_status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if read_status != ERROR_SUCCESS {
            return Err(ConfigError::Registry {
                action: "RegQueryValueExW(read)",
                code: read_status,
            });
        }

        let valid_len = (byte_len as usize).min(buf.len());
        let units: Vec<u16> = buf[..valid_len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|&u| u != 0)
            .collect();
        Ok(Some(String::from_utf16_lossy(&units)))
    }
}

/// Enable or disable per-user Windows startup (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice`).
pub fn set_windows_autostart(enabled: bool, exe_path: &Path) -> Result<(), ConfigError> {
    #[cfg(windows)]
    {
        win_autostart::set(enabled, exe_path)
    }
    #[cfg(not(windows))]
    {
        let _ = (enabled, exe_path);
        Err(ConfigError::UnsupportedPlatform)
    }
}

/// Read the current per-user Windows startup entry for `cap2voice`, if present.
pub fn get_windows_autostart() -> Result<Option<String>, ConfigError> {
    #[cfg(windows)]
    {
        win_autostart::get()
    }
    #[cfg(not(windows))]
    {
        Err(ConfigError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        default_config_path, default_log_dir, format_autostart_command, get_windows_autostart,
        load_config_from, save_config_to, set_windows_autostart, AppConfig, CaptureRegion,
        ConfigError, ConfigLoadOutcome, WindowPosition,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "cap2voice_cfg_{label}_{}_{}_{}",
                std::process::id(),
                nanos,
                seq
            ));
            let Ok(()) = std::fs::create_dir_all(&dir) else {
                panic!("failed to create temp test dir {}", dir.display());
            };
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
    fn save_and_load_roundtrip_preserves_all_fields_and_overwrites_atomically() {
        let temp = TempDirGuard::new("roundtrip");
        let cfg_path = temp.path().join("nested").join("config.toml");

        let mut first = AppConfig::default();
        first.general.capture_fps = 15;
        first.general.region = Some(CaptureRegion {
            monitor_id: "DISPLAY1".to_string(),
            x: 420,
            y: 1020,
            width: 1280,
            height: 60,
        });
        first.general.wxocr_dll_path = Some(PathBuf::from(r"C:\WeChat\wxocr.dll"));
        first.general.weixin_dir_path = Some(PathBuf::from(r"C:\WeChat\4.0.2.28"));
        first.general.subtitle_log_dir = Some(PathBuf::from(r"D:\Logs\cap2voice"));
        first.general.close_to_tray = false;
        first.general.start_with_windows = true;
        first.general.start_minimized_to_tray = true;
        first.translation.server_port = 19000;
        first.translation.dedupe_threshold = 0.90;
        first.translation.pixel_diff_threshold = 0.05;
        first.translation.blank_reset_frames = 8;
        first.voice.voice = "vi-VN-HoaiMyNeural".to_string();
        first.voice.volume = 0.60;
        first.voice.speech_rate = "+10%".to_string();
        first.voice.auto_read = false;
        first.display.font_size_px = 24.0;
        first.display.overlay_opacity = 0.90;
        first.display.chinese_color_hex = "#F8FAFC".to_string();
        first.display.vietnamese_color_hex = "#22C55E".to_string();
        first.display.overlay_position = Some(WindowPosition { x: 250.5, y: 780.0 });
        first.hotkeys.toggle_mute = "Ctrl+Shift+M".to_string();

        assert_eq!(save_config_to(&cfg_path, &first), Ok(()));
        let loaded_first = load_config_from(&cfg_path);
        assert_eq!(
            loaded_first,
            Ok(ConfigLoadOutcome {
                config: first.clone(),
                warning: None,
            })
        );

        let mut second = first;
        second.voice.volume = 0.45;
        second.general.capture_fps = 5;
        assert_eq!(save_config_to(&cfg_path, &second), Ok(()));
        assert_eq!(
            load_config_from(&cfg_path),
            Ok(ConfigLoadOutcome {
                config: second,
                warning: None,
            })
        );

        let Some(parent) = cfg_path.parent() else {
            panic!("config path missing parent");
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            panic!("failed to read config directory");
        };
        let names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["config.toml".to_string()]);
    }

    #[test]
    fn load_missing_file_returns_defaults_without_warning() {
        let temp = TempDirGuard::new("missing");
        let missing_path = temp.path().join("does_not_exist.toml");

        let outcome = load_config_from(&missing_path);
        assert_eq!(
            outcome,
            Ok(ConfigLoadOutcome {
                config: AppConfig::default(),
                warning: None,
            })
        );
        assert!(!missing_path.exists());
    }

    #[test]
    fn load_corrupt_toml_backs_up_to_bak_and_returns_warning() {
        let temp = TempDirGuard::new("corrupt_toml");
        let cfg_path = temp.path().join("config.toml");
        let corrupt_content = "[general\nbroken = !!!";
        let Ok(()) = std::fs::write(&cfg_path, corrupt_content) else {
            panic!("failed to write corrupt test fixture");
        };

        let Ok(outcome) = load_config_from(&cfg_path) else {
            panic!("corrupt config must fall back with warning after backup, not hard fail");
        };

        assert_eq!(outcome.config, AppConfig::default());
        let Some(warning) = outcome.warning else {
            panic!("expected ConfigLoadWarning for corrupt config.toml");
        };
        let expected_bak = temp.path().join("config.toml.bak");
        assert_eq!(warning.backup_path, expected_bak);
        assert!(!warning.reason.is_empty());
        assert_eq!(
            std::fs::read_to_string(&expected_bak).ok().as_deref(),
            Some(corrupt_content)
        );
        assert_eq!(
            std::fs::read_to_string(&cfg_path).ok().as_deref(),
            Some(corrupt_content)
        );
    }

    #[test]
    fn load_semantically_invalid_config_backs_up_to_bak_and_returns_warning() {
        let temp = TempDirGuard::new("invalid_semantic");
        let cfg_path = temp.path().join("config.toml");
        let bad_toml = "[voice]\nvolume = 2.5\n";
        let Ok(()) = std::fs::write(&cfg_path, bad_toml) else {
            panic!("failed to write invalid semantic fixture");
        };

        let Ok(outcome) = load_config_from(&cfg_path) else {
            panic!("semantically invalid config must back up and return defaults with warning");
        };

        assert_eq!(outcome.config, AppConfig::default());
        let Some(warning) = outcome.warning else {
            panic!("expected warning for out-of-range volume");
        };
        let expected_bak = temp.path().join("config.toml.bak");
        assert_eq!(warning.backup_path, expected_bak);
        assert_eq!(
            std::fs::read_to_string(&expected_bak).ok().as_deref(),
            Some(bad_toml)
        );
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

    #[test]
    fn format_autostart_command_quotes_path_with_spaces() {
        let exe = Path::new(r"C:\Program Files\cap2voice\cap2voice-app.exe");
        assert_eq!(
            format_autostart_command(exe),
            Ok(r#""C:\Program Files\cap2voice\cap2voice-app.exe""#.to_string())
        );

        let empty = Path::new("");
        assert!(matches!(
            format_autostart_command(empty),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn default_paths_and_effective_log_dir_resolve_correctly() {
        let Ok(cfg_path) = default_config_path() else {
            panic!("expected APPDATA to resolve default_config_path");
        };
        assert!(cfg_path.ends_with(Path::new("cap2voice").join("config.toml")));

        let Ok(log_dir) = default_log_dir() else {
            panic!("expected APPDATA to resolve default_log_dir");
        };
        assert!(log_dir.ends_with(Path::new("cap2voice").join("logs")));

        let mut cfg = AppConfig::default();
        assert_eq!(cfg.effective_log_dir(), Ok(log_dir));

        let custom = PathBuf::from(r"D:\CustomLogs\cap2voice");
        cfg.general.subtitle_log_dir = Some(custom.clone());
        assert_eq!(cfg.effective_log_dir(), Ok(custom));
    }

    #[test]
    #[ignore = "mutates HKCU Windows Run registry key on real Windows PC"]
    fn real_pc_windows_autostart_sets_and_removes_registry_value() {
        struct RegistryRestoreGuard(Option<String>);
        impl Drop for RegistryRestoreGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(prev) => {
                        let trimmed = prev.trim_matches('"');
                        let _ = set_windows_autostart(true, Path::new(trimmed));
                    }
                    None => {
                        let _ = set_windows_autostart(false, Path::new("unused.exe"));
                    }
                }
            }
        }

        let Ok(previous) = get_windows_autostart() else {
            panic!("failed reading pre-test HKCU Run registry key");
        };
        let _restore = RegistryRestoreGuard(previous);

        let test_exe = Path::new(r"C:\Program Files\cap2voice\cap2voice-app.exe");
        let expected_cmd = r#""C:\Program Files\cap2voice\cap2voice-app.exe""#;

        assert_eq!(set_windows_autostart(true, test_exe), Ok(()));
        assert_eq!(get_windows_autostart(), Ok(Some(expected_cmd.to_string())));

        assert_eq!(set_windows_autostart(false, test_exe), Ok(()));
        assert_eq!(get_windows_autostart(), Ok(None));

        // Removing again must be idempotent.
        assert_eq!(set_windows_autostart(false, test_exe), Ok(()));
        assert_eq!(get_windows_autostart(), Ok(None));
    }
}
