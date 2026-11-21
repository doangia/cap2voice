//! Windows per-user autostart registry integration
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice`).

use crate::config::ConfigError;
use std::path::Path;

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
    use super::format_autostart_command;
    use crate::config::ConfigError;
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
    use super::{format_autostart_command, get_windows_autostart, set_windows_autostart};
    use crate::config::ConfigError;
    use std::path::Path;

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
