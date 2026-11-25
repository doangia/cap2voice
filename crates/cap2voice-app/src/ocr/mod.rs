//! OCR adapter wrapping `wcocr-rs`, WeChat 4.x path auto-discovery,
//! CJK text filtering, and fuzzy deduplication.

pub mod filter;

pub use filter::{
    extract_chinese_subtitle, extract_chinese_subtitle_lines,
    extract_chinese_subtitle_lines_with_decisions, filter_cjk_text, is_cjk_ideograph,
    join_sorted_blocks, normalize_for_comparison, normalized_similarity, sort_text_blocks,
    FuzzyDeduplicator, GeometricSplitAction, GeometricSplitDecision, GeometricSplitKind,
    TextDeduplicator, DEFAULT_BLANK_RESET_FRAMES, DEFAULT_DEDUPE_THRESHOLD,
};
pub use wcocr_rs::{CharDetail, OcrError, OcrResult, TextBlock};

use crate::capture::CapturedFrame;
use crate::config::GeneralConfig;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;
use wcocr_rs::WeChatOcr;

/// Default per-frame OCR request timeout (aligned with `CONSTRAINTS.md` §2 `<= 3100 ms` bound).
pub const DEFAULT_OCR_TIMEOUT: Duration = Duration::from_secs(3);

/// Default WeChat OCR mojo handshake timeout on lazy session initialization.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolved filesystem paths required to initialize `wcocr_rs::WeChatOcr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeChatOcrPaths {
    pub wxocr_dll: PathBuf,
    pub weixin_dir: PathBuf,
}

/// Typed errors emitted by path discovery and the OCR adapter.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OcrAdapterError {
    #[error("Không tìm thấy wxocr.dll trong {searched} — hãy kiểm tra cài đặt WeChat 4.x hoặc chọn thủ công trong Cài đặt")]
    WxocrNotFound { searched: String },

    #[error("Không tìm thấy thư mục Weixin 4.x (chứa mmmojo_64.dll) trong {searched} — hãy kiểm tra cài đặt WeChat hoặc chọn thủ công trong Cài đặt")]
    WeixinDirNotFound { searched: String },

    #[error("Đường dẫn {kind} đã cấu hình không hợp lệ hoặc bị thiếu: {}", path.display())]
    InvalidConfiguredPath { kind: &'static str, path: PathBuf },

    #[error("Lỗi ghi ảnh tạm OCR tại {}: {detail}", path.display())]
    TempFrameIo { path: PathBuf, detail: String },

    #[error("Lỗi WeChat OCR engine: {0}")]
    Engine(#[from] wcocr_rs::OcrError),

    #[error("WeChat OCR trả về mã lỗi khác 0 (errcode={errcode})")]
    NonZeroErrCode { errcode: i32 },
}

/// Abstract OCR engine boundary implemented by `WeChatOcrAdapter`.
pub trait OcrEngine: Send + Sync {
    fn recognize_frame(&self, frame: &CapturedFrame) -> Result<OcrResult, OcrAdapterError>;
}

/// Return `%TEMP%\cap2voice\frame.png`, the reused V1 OCR temp frame file path.
pub fn default_temp_frame_path() -> PathBuf {
    std::env::temp_dir().join("cap2voice").join("frame.png")
}

fn version_sort_key(name: &str) -> (bool, Vec<u64>, String) {
    let mut parts = Vec::new();
    let mut is_numeric = !name.is_empty();
    for seg in name.split('.') {
        if let Ok(num) = seg.parse::<u64>() {
            parts.push(num);
        } else {
            is_numeric = false;
            parts.clear();
            break;
        }
    }
    (is_numeric, parts, name.to_string())
}

fn discover_latest_versioned_entry(
    root: &Path,
    build_target: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return None;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let dir = entry.path();
            if !dir.is_dir() {
                return None;
            }
            let target = build_target(&dir)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            Some((version_sort_key(&name), target))
        })
        .max_by(|(ka, _), (kb, _)| ka.cmp(kb))
        .map(|(_, target)| target)
}

/// Discover the newest WeChatOcr plugin DLL (`<plugins_root>\<version>\extracted\wxocr.dll`).
pub fn discover_wxocr_in(plugins_root: &Path) -> Result<PathBuf, OcrAdapterError> {
    discover_latest_versioned_entry(plugins_root, |dir| {
        let dll = dir.join("extracted").join("wxocr.dll");
        dll.is_file().then_some(dll)
    })
    .ok_or_else(|| OcrAdapterError::WxocrNotFound {
        searched: plugins_root.display().to_string(),
    })
}

/// Discover the newest Weixin 4.x runtime folder containing `mmmojo_64.dll`.
pub fn discover_weixin_dir_in(weixin_root: &Path) -> Result<PathBuf, OcrAdapterError> {
    discover_latest_versioned_entry(weixin_root, |dir| {
        dir.join("mmmojo_64.dll")
            .is_file()
            .then(|| dir.to_path_buf())
    })
    .ok_or_else(|| OcrAdapterError::WeixinDirNotFound {
        searched: weixin_root.display().to_string(),
    })
}

/// Resolve `wxocr.dll` and Weixin 4.x directory from explicit overrides or candidate roots.
pub fn resolve_wechat_paths_with_roots(
    wxocr_override: Option<&Path>,
    weixin_override: Option<&Path>,
    plugins_root: Option<&Path>,
    weixin_root: Option<&Path>,
) -> Result<WeChatOcrPaths, OcrAdapterError> {
    let wxocr_dll = match wxocr_override {
        Some(path) => {
            if !path.is_file() {
                return Err(OcrAdapterError::InvalidConfiguredPath {
                    kind: "wxocr.dll",
                    path: path.to_path_buf(),
                });
            }
            path.to_path_buf()
        }
        None => match plugins_root {
            Some(root) => discover_wxocr_in(root)?,
            None => {
                return Err(OcrAdapterError::WxocrNotFound {
                    searched: r"%APPDATA%\Tencent\xwechat\XPlugin\plugins\WeChatOcr".to_string(),
                });
            }
        },
    };

    let weixin_dir = match weixin_override {
        Some(path) => {
            if !path.is_dir() {
                return Err(OcrAdapterError::InvalidConfiguredPath {
                    kind: "thư mục Weixin 4.x",
                    path: path.to_path_buf(),
                });
            }
            if !path.join("mmmojo_64.dll").is_file() {
                return Err(OcrAdapterError::InvalidConfiguredPath {
                    kind: "thư mục Weixin 4.x (thiếu mmmojo_64.dll)",
                    path: path.to_path_buf(),
                });
            }
            path.to_path_buf()
        }
        None => match weixin_root {
            Some(root) => discover_weixin_dir_in(root)?,
            None => {
                return Err(OcrAdapterError::WeixinDirNotFound {
                    searched: r"C:\Program Files\Tencent\Weixin".to_string(),
                });
            }
        },
    };

    Ok(WeChatOcrPaths {
        wxocr_dll,
        weixin_dir,
    })
}

/// Auto-discover WeChat 4.x OCR paths on Windows, honoring explicit manual overrides.
pub fn resolve_wechat_paths(
    wxocr_override: Option<&Path>,
    weixin_override: Option<&Path>,
) -> Result<WeChatOcrPaths, OcrAdapterError> {
    let plugins_root = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).join(r"Tencent\xwechat\XPlugin\plugins\WeChatOcr"));
    let weixin_root = std::env::var_os("ProgramFiles")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).join(r"Tencent\Weixin"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Tencent\Weixin"));

    resolve_wechat_paths_with_roots(
        wxocr_override,
        weixin_override,
        plugins_root.as_deref(),
        Some(weixin_root.as_path()),
    )
}

/// Convenience wrapper reading manual overrides from `GeneralConfig`.
pub fn resolve_wechat_paths_from_config(
    general: &GeneralConfig,
) -> Result<WeChatOcrPaths, OcrAdapterError> {
    resolve_wechat_paths(
        general.wxocr_dll_path.as_deref(),
        general.weixin_dir_path.as_deref(),
    )
}

/// Thread-safe `OcrEngine` adapter lazily holding one `WeChatOcr` session and
/// writing frames to a single reused temp PNG file.
pub struct WeChatOcrAdapter {
    paths: WeChatOcrPaths,
    temp_frame_path: PathBuf,
    handshake_timeout: Duration,
    request_timeout: Duration,
    session: Mutex<Option<WeChatOcr>>,
}

impl WeChatOcrAdapter {
    pub fn new(paths: WeChatOcrPaths) -> Self {
        Self {
            paths,
            temp_frame_path: default_temp_frame_path(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: DEFAULT_OCR_TIMEOUT,
            session: Mutex::new(None),
        }
    }

    pub fn with_temp_frame_path(mut self, path: PathBuf) -> Self {
        self.temp_frame_path = path;
        self
    }

    pub fn with_timeouts(mut self, handshake: Duration, request: Duration) -> Self {
        self.handshake_timeout = handshake;
        self.request_timeout = request;
        self
    }

    pub fn paths(&self) -> &WeChatOcrPaths {
        &self.paths
    }

    pub fn temp_frame_path(&self) -> &Path {
        &self.temp_frame_path
    }

    fn ensure_session<'a>(
        &self,
        guard: &'a mut Option<WeChatOcr>,
    ) -> Result<&'a WeChatOcr, OcrAdapterError> {
        if guard.is_none() {
            let ocr = WeChatOcr::new(&self.paths.wxocr_dll, &self.paths.weixin_dir)?;
            ocr.wait_connection(self.handshake_timeout)?;
            *guard = Some(ocr);
        }
        let Some(ocr) = guard.as_ref() else {
            return Err(OcrAdapterError::Engine(OcrError::StartFailed(
                "OCR session unavailable after initialization".to_string(),
            )));
        };
        Ok(ocr)
    }

    /// Eagerly initialize the `WeChatOcr` session and complete the Mojo handshake.
    pub fn warmup(&self) -> Result<(), OcrAdapterError> {
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.ensure_session(&mut guard)?;
        Ok(())
    }
}

impl OcrEngine for WeChatOcrAdapter {
    fn recognize_frame(&self, frame: &CapturedFrame) -> Result<OcrResult, OcrAdapterError> {
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let ocr = self.ensure_session(&mut guard)?;

        frame
            .save_png(&self.temp_frame_path)
            .map_err(|err| OcrAdapterError::TempFrameIo {
                path: self.temp_frame_path.clone(),
                detail: err.to_string(),
            })?;

        let result = ocr.ocr_file(&self.temp_frame_path, self.request_timeout)?;
        if result.errcode != 0 {
            return Err(OcrAdapterError::NonZeroErrCode {
                errcode: result.errcode,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{default_temp_frame_path, version_sort_key};
    use std::path::Path;

    #[test]
    fn version_key_compares_numeric_segments_rather_than_lexicographically() {
        assert!(version_sort_key("4.0.10.1") > version_sort_key("4.0.2.28"));
        assert!(version_sort_key("10001") > version_sort_key("8022"));
        assert!(version_sort_key("8022") > version_sort_key("backup"));
    }

    #[test]
    fn default_temp_frame_path_uses_cap2voice_frame_png() {
        let path = default_temp_frame_path();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("frame.png"));
        assert_eq!(
            path.parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str()),
            Some("cap2voice")
        );
    }
}
