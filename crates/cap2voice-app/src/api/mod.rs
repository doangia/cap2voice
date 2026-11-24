//! Native translation/TTS engine traits and typed errors (D1+A5).
//!
//! There is no HTTP client and no Python server anymore: translation is
//! in-process Rust (`translate::SwitchableTranslator`, Helsinki ONNX or
//! Vietphrase) and TTS is a direct Edge-TTS WebSocket client
//! (`tts::EdgeTtsEngine`).

use crate::audio::AudioFormat;
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

/// Boxed `Send` future alias used by async `Translator` and `TtsEngine` traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Typed errors emitted by `Translator` implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TranslateError {
    #[error("chuỗi cần dịch không được để trống")]
    EmptyInput,

    #[error("yêu cầu dịch không hợp lệ: {detail}")]
    InvalidInput { detail: String },

    #[error("lỗi engine dịch (HTTP {status}): {detail}")]
    Upstream { status: u16, detail: String },

    #[error("không có engine dịch khả dụng: {detail}")]
    EngineUnavailable { detail: String },

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
    Upstream {
        status: u16,
        detail: String,
        /// Retries taken before this error was returned (empty when the
        /// error came from the first attempt). Not shown in the short
        /// message; the pipeline logs them to the subtitle log file.
        retries: Vec<TtsRetryAttempt>,
    },

    #[error("mất kết nối mạng tới Edge-TTS: {detail}")]
    Network {
        detail: String,
        /// Retries taken before this error was returned (see `Upstream`).
        retries: Vec<TtsRetryAttempt>,
    },

    #[error("không có engine TTS khả dụng: {detail}")]
    EngineUnavailable { detail: String },

    #[error("hết thời gian chờ TTS sau {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },

    #[error("phản hồi âm thanh TTS không hợp lệ: {detail}")]
    InvalidResponse { detail: String },
}

impl TtsError {
    /// Retry attempts recorded while producing this error (empty when the
    /// error came from the first attempt or is not retryable).
    pub fn retries(&self) -> &[TtsRetryAttempt] {
        match self {
            TtsError::Upstream { retries, .. } | TtsError::Network { retries, .. } => retries,
            _ => &[],
        }
    }

    /// Attach the retry attempts that led to this error. No-op for
    /// variants that do not carry retries.
    pub fn set_retries(&mut self, retries: Vec<TtsRetryAttempt>) {
        match self {
            TtsError::Upstream { retries: slot, .. } | TtsError::Network { retries: slot, .. } => {
                *slot = retries;
            }
            _ => {}
        }
    }
}

/// Maximum Edge-TTS synthesis retries on transient failures (HTTP 429 /
/// 5xx / network errors). The retry loop sleeps with exponential backoff
/// (500 ms, 1 s, 2 s) between attempts; the pipeline shows this count in
/// the subtitle log (`thử lại 1/3`).
pub const TTS_MAX_RETRIES: u32 = 3;

/// One recorded TTS retry attempt, for the subtitle log: which retry, what
/// the service answered, and how long the client waited before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsRetryAttempt {
    /// 1-based retry number (1 = first retry after the initial attempt).
    pub attempt: u32,
    /// HTTP status when the failure carried one (429 / 5xx); `None` for
    /// network errors with no response.
    pub status: Option<u16>,
    /// Short summary of the failure that triggered this retry.
    pub detail: String,
    /// Backoff waited before this retry, in milliseconds.
    pub backoff_ms: u64,
}

/// A completed TTS synthesis: the audio bytes in `format` plus the retries
/// it took to get them (empty when the first attempt succeeded). The pipeline
/// logs non-empty `retries` to the subtitle log file so a transient 429 stays
/// visible even though the sentence still got its audio. Edge-TTS returns
/// MP3; Piper returns WAV (PCM16, 22050 Hz mono) — the format travels with
/// the bytes so the cache and the player decode them correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsSynthesis {
    pub audio: Vec<u8>,
    pub format: AudioFormat,
    pub retries: Vec<TtsRetryAttempt>,
}

/// Asynchronous Chinese-to-Vietnamese translator boundary.
pub trait Translator: Send + Sync {
    fn translate<'a>(&'a self, chinese: &'a str) -> BoxFuture<'a, Result<String, TranslateError>>;

    /// Stable engine id for translation-cache keys (`"helsinki"` /
    /// `"vietphrase"`). `None` when the translator has no engine concept
    /// (test doubles); the pipeline then keys by `"unknown"`.
    fn engine_id(&self) -> Option<&'static str> {
        None
    }
}

/// Asynchronous Vietnamese text-to-speech synthesis boundary (returns the
/// audio bytes in `TtsSynthesis::format` plus any retries it took; see
/// `TtsSynthesis`). The `voice` id is engine-dependent: an Edge-TTS voice id
/// (`vi-VN-NamMinhNeural`, …) for the Edge engine, a Piper voice id
/// (`ngochuyen`, …) for the Piper engine. The caller (pipeline / Settings
/// preview) passes the voice that matches the active engine.
pub trait TtsEngine: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<TtsSynthesis, TtsError>>;
}
