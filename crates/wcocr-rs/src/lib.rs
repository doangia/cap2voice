//! Internal Rust library crate wrapping WeChat 4.x OCR through `mmmojo_64.dll`.

use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod mojo;
pub mod ocr;
pub mod proto;

pub use ocr::{CharDetail, OcrResult, TextBlock, WeChatOcr};

/// Typed errors returned by the `wcocr-rs` OCR wrapper.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OcrError {
    #[error("invalid {expected} path: {}", path.display())]
    InvalidPath {
        path: PathBuf,
        expected: &'static str,
    },

    #[error("failed to load {}: {detail}", path.display())]
    DllLoadFailed { path: PathBuf, detail: String },

    #[error("failed to start mmmojo environment: {0}")]
    StartFailed(String),

    #[error("OCR handshake timed out after {timeout_ms} ms")]
    HandshakeTimeout { timeout_ms: u64 },

    #[error("WeChat OCR plugin reported OCR is not supported")]
    NotSupported,

    #[error("OCR request timed out after {timeout_ms} ms")]
    RequestTimeout { timeout_ms: u64 },

    #[error("failed to send OCR request: {0}")]
    SendFailed(String),

    #[error("invalid image path {}: {detail}", path.display())]
    InvalidImagePath { path: PathBuf, detail: String },
}

/// Encode a Rust `&str` as a null-terminated UTF-16 wide string for Windows FFI.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Strip the Windows `\\?\` or `\\?\UNC\` verbatim prefix added by `std::fs::canonicalize`,
/// returning a standard absolute path accepted by WeChat's OCR subprocess.
pub fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_verbatim_prefix, to_wide, OcrError};
    use std::path::{Path, PathBuf};

    #[test]
    fn to_wide_encodes_ascii_with_null_terminator() {
        assert_eq!(to_wide("abc"), vec![97, 98, 99, 0]);
    }

    #[test]
    fn to_wide_encodes_empty_string_as_single_null() {
        assert_eq!(to_wide(""), vec![0]);
    }

    #[test]
    fn to_wide_encodes_chinese_filename_roundtrip() {
        let wide = to_wide("test呀哈哟.png");
        assert_eq!(wide.last(), Some(&0));
        let decoded = String::from_utf16(&wide[..wide.len() - 1]);
        assert_eq!(decoded.ok().as_deref(), Some("test呀哈哟.png"));
    }

    #[test]
    fn strip_verbatim_prefix_removes_drive_prefix() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\foo\bar.png")),
            PathBuf::from(r"C:\foo\bar.png")
        );
    }

    #[test]
    fn strip_verbatim_prefix_converts_unc_prefix() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\a.png")),
            PathBuf::from(r"\\server\share\a.png")
        );
    }

    #[test]
    fn strip_verbatim_prefix_preserves_normal_path() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\foo\bar.png")),
            PathBuf::from(r"C:\foo\bar.png")
        );
    }

    #[test]
    fn ocr_error_display_messages_include_context() {
        let cases = [
            (
                OcrError::InvalidPath {
                    path: PathBuf::from(r"C:\missing\wxocr.dll"),
                    expected: "wxocr.dll file",
                },
                r"C:\missing\wxocr.dll",
            ),
            (
                OcrError::DllLoadFailed {
                    path: PathBuf::from(r"C:\wx\mmmojo_64.dll"),
                    detail: "module not found".to_string(),
                },
                "module not found",
            ),
            (
                OcrError::StartFailed("CreateMMMojoEnvironment returned null".to_string()),
                "CreateMMMojoEnvironment returned null",
            ),
            (OcrError::HandshakeTimeout { timeout_ms: 5000 }, "5000 ms"),
            (OcrError::NotSupported, "not supported"),
            (OcrError::RequestTimeout { timeout_ms: 3000 }, "3000 ms"),
            (
                OcrError::SendFailed("channel closed".to_string()),
                "channel closed",
            ),
            (
                OcrError::InvalidImagePath {
                    path: PathBuf::from(r"C:\bad.png"),
                    detail: "not found".to_string(),
                },
                r"C:\bad.png",
            ),
        ];

        for (err, expected_substr) in cases {
            let msg = err.to_string();
            assert!(
                msg.contains(expected_substr),
                "expected '{msg}' to contain '{expected_substr}'"
            );
        }
    }
}
