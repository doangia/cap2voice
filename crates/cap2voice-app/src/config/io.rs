//! Configuration file paths, atomic TOML load/save, and corrupt-file backup.

use super::{AppConfig, ConfigError, ConfigLoadOutcome, ConfigLoadWarning};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Return `%APPDATA%\cap2voice\cache` (A3 / gh #2).
pub fn default_cache_dir() -> Result<PathBuf, ConfigError> {
    Ok(appdata_root()?.join("cache"))
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

#[cfg(test)]
mod tests {
    use super::{default_config_path, default_log_dir, load_config_from, save_config_to};
    use crate::config::{AppConfig, CaptureRegion, ConfigLoadOutcome, WindowPosition};
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
        first.translation.engine = crate::translate::TranslationEngine::Vietphrase;
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
}
