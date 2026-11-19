//! HTTP client traits/implementations for the temporary local FastAPI server
//! and managed Python server child-process lifecycle.

pub mod http_client;
pub mod server_proc;

pub use http_client::{
    check_health, HttpTranslator, HttpTtsEngine, DEFAULT_CONNECT_TIMEOUT, DEFAULT_HEALTH_TIMEOUT,
    DEFAULT_TRANSLATE_TIMEOUT, DEFAULT_TTS_TIMEOUT,
};
pub use server_proc::{
    discover_python_executable, discover_python_executable_with_path, discover_server_dir,
    ServerProcess, ServerProcessConfig, DEFAULT_HEALTH_POLL_INTERVAL, DEFAULT_HOST,
    DEFAULT_SHUTDOWN_GRACE, DEFAULT_STARTUP_TIMEOUT,
};

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

/// Boxed `Send` future alias used by async `Translator` and `TtsEngine` traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Response payload returned by `GET /health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    pub status: String,
    pub version: String,
    pub translator: String,
    pub tts: String,
}

/// Request payload sent to `POST /translate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslateRequest {
    pub text: String,
}

/// Response payload returned by `POST /translate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslateResponse {
    pub text: String,
}

/// Request payload sent to `POST /tts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsRequest {
    pub text: String,
    pub voice: String,
    pub rate: String,
}

/// Structured error body returned by `server/server.py`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub error: String,
    pub detail: String,
}

/// Typed errors emitted by `Translator` implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TranslateError {
    #[error("chuỗi cần dịch không được để trống")]
    EmptyInput,

    #[error("yêu cầu dịch không hợp lệ: {detail}")]
    InvalidInput { detail: String },

    #[error("Google Translate bị giới hạn tốc độ — tạm chờ: {detail}")]
    RateLimited { detail: String },

    #[error("lỗi dịch từ máy chủ (HTTP {status}): {detail}")]
    Upstream { status: u16, detail: String },

    #[error("mất kết nối server dịch/TTS local: {detail}")]
    ServerUnavailable { detail: String },

    #[error("hết thời gian chờ dịch sau {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },

    #[error("phản hồi dịch không hợp lệ: {detail}")]
    InvalidResponse { detail: String },
}

/// Typed errors emitted by `TtsEngine` implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TtsError {
    #[error("văn bản đọc TTS không được để trống")]
    EmptyInput,

    #[error("yêu cầu TTS không hợp lệ: {detail}")]
    InvalidInput { detail: String },

    #[error("lỗi tạo giọng đọc TTS (HTTP {status}): {detail}")]
    Upstream { status: u16, detail: String },

    #[error("mất kết nối server dịch/TTS local: {detail}")]
    ServerUnavailable { detail: String },

    #[error("hết thời gian chờ TTS sau {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },

    #[error("phản hồi âm thanh TTS không hợp lệ: {detail}")]
    InvalidResponse { detail: String },
}

/// Typed errors emitted by `GET /health` probes.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HealthError {
    #[error("server dịch/TTS chưa sẵn sàng: {detail}")]
    ServerUnavailable { detail: String },

    #[error("hết thời gian kiểm tra /health sau {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },

    #[error("endpoint /health trả về HTTP {status}: {detail}")]
    UnexpectedStatus { status: u16, detail: String },

    #[error("dữ liệu /health không hợp lệ: {detail}")]
    InvalidResponse { detail: String },
}

/// Typed errors emitted by `ServerProcess` discovery, launch, and shutdown.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ServerProcessError {
    #[error("không tìm thấy thư mục server Python (chứa server.py) từ {searched}")]
    ServerDirNotFound { searched: String },

    #[error("không tìm thấy Python để chạy server local ({searched}) — hãy tạo server/.venv và cài server/requirements.txt")]
    PythonNotFound { searched: String },

    #[error("không thể khởi chạy {program}: {detail}")]
    SpawnFailed { program: String, detail: String },

    #[error("lỗi Windows Job Object tại {action} (Win32 code {code})")]
    JobObjectFailed { action: &'static str, code: u32 },

    #[error("server Python thoát sớm (mã {exit_code:?}): {stderr}")]
    ExitedEarly {
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("server Python trên 127.0.0.1:{port} không sẵn sàng sau {timeout_ms} ms: {detail}")]
    StartupTimeout {
        port: u16,
        timeout_ms: u64,
        detail: String,
    },

    #[error("lỗi khởi tạo HTTP client cho server local: {0}")]
    HttpClientInit(String),
}

/// Asynchronous Chinese-to-Vietnamese translator boundary.
pub trait Translator: Send + Sync {
    fn translate<'a>(&'a self, chinese: &'a str) -> BoxFuture<'a, Result<String, TranslateError>>;
}

/// Asynchronous Vietnamese text-to-speech synthesis boundary (returns raw MP3 bytes).
pub trait TtsEngine: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, TtsError>>;
}
