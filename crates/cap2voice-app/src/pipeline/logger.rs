//! Daily UTF-8 `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` subtitle logger.
//!
//! The same daily file also carries TTS diagnostics: a retry line for each
//! transient Edge-TTS failure the client rode out, and a failure line when
//! a sentence ultimately lost its audio. Both keep the file's shape — one
//! sanitized single line per event, same `[timestamp]` prefix, same daily
//! rotation — so the log stays one chronological stream instead of a
//! second format.

use crate::api::{TtsError, TtsRetryAttempt, TTS_MAX_RETRIES};
use chrono::{NaiveDate, NaiveDateTime};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

/// Typed errors emitted when validating or writing subtitle log entries.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SubtitleLogError {
    #[error("không thể ghi log phụ đề khi {field} bị rỗng")]
    EmptySubtitle { field: &'static str },

    #[error("lỗi ghi tệp log phụ đề {}: {detail}", path.display())]
    Io { path: PathBuf, detail: String },
}

/// Build the daily UTF-8 subtitle log file path (`subtitles-YYYY-MM-DD.log`).
pub fn daily_log_path(log_dir: &Path, date: NaiveDate) -> PathBuf {
    log_dir.join(format!("subtitles-{}.log", date.format("%Y-%m-%d")))
}

fn sanitize_single_line(raw: &str) -> String {
    raw.split(['\r', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_clean_log_line(timestamp: NaiveDateTime, clean_zh: &str, clean_vi: &str) -> String {
    format!(
        "[{}] {} : {}\n",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        clean_zh,
        clean_vi
    )
}

/// Format a single flushed log line (`[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>\n`),
/// flattening embedded newlines so each subtitle pair occupies exactly one line.
pub fn format_subtitle_log_line(
    timestamp: NaiveDateTime,
    chinese: &str,
    vietnamese: &str,
) -> String {
    let clean_zh = sanitize_single_line(chinese);
    let clean_vi = sanitize_single_line(vietnamese);
    format_clean_log_line(timestamp, &clean_zh, &clean_vi)
}

/// Format a TTS retry line, e.g.
/// `[2026-11-23 15:30:00] TTS thử lại 1/3 (vi-VN-NamMinhNeural, HTTP 429, chờ 500ms): Xin chào.`
/// The status is `HTTP <status>` for upstream failures, `lỗi mạng` for
/// network errors with no response.
pub fn format_tts_retry_line(
    timestamp: NaiveDateTime,
    vietnamese: &str,
    voice: &str,
    attempt: &TtsRetryAttempt,
) -> String {
    let clean_vi = sanitize_single_line(vietnamese);
    let clean_voice = sanitize_single_line(voice);
    let status = attempt
        .status
        .map_or("lỗi mạng".to_string(), |s| format!("HTTP {s}"));
    format!(
        "[{}] TTS thử lại {}/{} ({}, {}, chờ {}ms): {}\n",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        attempt.attempt,
        TTS_MAX_RETRIES,
        clean_voice,
        status,
        attempt.backoff_ms,
        clean_vi
    )
}

/// Format a TTS failure line, e.g.
/// `[2026-11-23 15:30:02] TTS THẤT BẠI (vi-VN-NamMinhNeural) sau 3 lần thử: lỗi tạo giọng đọc TTS (HTTP 429): … : Xin chào.`
/// The retry count is omitted when the error came from the first attempt.
pub fn format_tts_failure_line(
    timestamp: NaiveDateTime,
    vietnamese: &str,
    voice: &str,
    error: &TtsError,
) -> String {
    let clean_vi = sanitize_single_line(vietnamese);
    let clean_voice = sanitize_single_line(voice);
    let clean_err = sanitize_single_line(&error.to_string());
    let ts = timestamp.format("%Y-%m-%d %H:%M:%S");
    if error.retries().is_empty() {
        format!("[{ts}] TTS THẤT BẠI ({clean_voice}): {clean_err} : {clean_vi}\n")
    } else {
        format!(
            "[{ts}] TTS THẤT BẠI ({clean_voice}) sau {} lần thử: {clean_err} : {clean_vi}\n",
            error.retries().len()
        )
    }
}

struct OpenDailyFile {
    date: NaiveDate,
    path: PathBuf,
    file: File,
}

/// Thread-safe daily UTF-8 subtitle log writer that flushes each pair immediately.
pub struct SubtitleLogger {
    log_dir: PathBuf,
    state: Mutex<Option<OpenDailyFile>>,
}

impl SubtitleLogger {
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            state: Mutex::new(None),
        }
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Append and flush one `Chinese : Vietnamese` pair using the current local timestamp.
    pub fn log_pair(&self, chinese: &str, vietnamese: &str) -> Result<PathBuf, SubtitleLogError> {
        let now = chrono::Local::now().naive_local();
        self.log_pair_at(now, chinese, vietnamese)
    }

    /// Append and flush one `Chinese : Vietnamese` pair at an explicit timestamp.
    pub fn log_pair_at(
        &self,
        timestamp: NaiveDateTime,
        chinese: &str,
        vietnamese: &str,
    ) -> Result<PathBuf, SubtitleLogError> {
        let clean_zh = sanitize_single_line(chinese);
        if clean_zh.is_empty() {
            return Err(SubtitleLogError::EmptySubtitle { field: "chinese" });
        }
        let clean_vi = sanitize_single_line(vietnamese);
        if clean_vi.is_empty() {
            return Err(SubtitleLogError::EmptySubtitle {
                field: "vietnamese",
            });
        }

        let line = format_clean_log_line(timestamp, &clean_zh, &clean_vi);
        self.write_line_at(timestamp, line)
    }

    /// Append and flush one TTS retry line using the current local
    /// timestamp. Best-effort diagnostics: the caller ignores the error
    /// (the overlay banner already shows the failure, if any).
    pub fn log_tts_retry(
        &self,
        vietnamese: &str,
        voice: &str,
        attempt: &TtsRetryAttempt,
    ) -> Result<PathBuf, SubtitleLogError> {
        let now = chrono::Local::now().naive_local();
        self.log_tts_retry_at(now, vietnamese, voice, attempt)
    }

    /// Append and flush one TTS retry line at an explicit timestamp.
    pub fn log_tts_retry_at(
        &self,
        timestamp: NaiveDateTime,
        vietnamese: &str,
        voice: &str,
        attempt: &TtsRetryAttempt,
    ) -> Result<PathBuf, SubtitleLogError> {
        self.write_line_at(
            timestamp,
            format_tts_retry_line(timestamp, vietnamese, voice, attempt),
        )
    }

    /// Append and flush one TTS failure line using the current local
    /// timestamp (sentence, voice, status/error, retry count).
    pub fn log_tts_failure(
        &self,
        vietnamese: &str,
        voice: &str,
        error: &TtsError,
    ) -> Result<PathBuf, SubtitleLogError> {
        let now = chrono::Local::now().naive_local();
        self.log_tts_failure_at(now, vietnamese, voice, error)
    }

    /// Append and flush one TTS failure line at an explicit timestamp.
    pub fn log_tts_failure_at(
        &self,
        timestamp: NaiveDateTime,
        vietnamese: &str,
        voice: &str,
        error: &TtsError,
    ) -> Result<PathBuf, SubtitleLogError> {
        self.write_line_at(
            timestamp,
            format_tts_failure_line(timestamp, vietnamese, voice, error),
        )
    }

    /// Append one pre-formatted line to the daily file for `timestamp`,
    /// opening (or rotating) the file as needed, and flush immediately.
    fn write_line_at(
        &self,
        timestamp: NaiveDateTime,
        line: String,
    ) -> Result<PathBuf, SubtitleLogError> {
        let date = timestamp.date();

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let needs_reopen = match guard.as_ref() {
            Some(open) => open.date != date,
            None => true,
        };
        if needs_reopen {
            std::fs::create_dir_all(&self.log_dir).map_err(|err| SubtitleLogError::Io {
                path: self.log_dir.clone(),
                detail: err.to_string(),
            })?;
            let path = daily_log_path(&self.log_dir, date);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|err| SubtitleLogError::Io {
                    path: path.clone(),
                    detail: err.to_string(),
                })?;
            *guard = Some(OpenDailyFile { date, path, file });
        }

        let Some(open) = guard.as_mut() else {
            return Err(SubtitleLogError::Io {
                path: daily_log_path(&self.log_dir, date),
                detail: "open daily subtitle log file handle missing".to_string(),
            });
        };

        if let Err(err) = open
            .file
            .write_all(line.as_bytes())
            .and_then(|()| open.file.flush())
        {
            let err_path = open.path.clone();
            *guard = None;
            return Err(SubtitleLogError::Io {
                path: err_path,
                detail: err.to_string(),
            });
        }

        Ok(open.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{daily_log_path, format_subtitle_log_line, SubtitleLogError, SubtitleLogger};
    use chrono::NaiveDate;
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
                "cap2voice_log_{label}_{}_{}_{}",
                std::process::id(),
                nanos,
                seq
            ));
            let Ok(()) = std::fs::create_dir_all(&dir) else {
                panic!("failed creating temp dir {}", dir.display());
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

    fn dt(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> chrono::NaiveDateTime {
        let Some(date) = NaiveDate::from_ymd_opt(y, m, d) else {
            panic!("invalid date {y}-{m}-{d}");
        };
        let Some(dt) = date.and_hms_opt(hh, mm, ss) else {
            panic!("invalid hms {hh}:{mm}:{ss}");
        };
        dt
    }

    #[test]
    fn format_subtitle_log_line_produces_exact_spec_format_and_sanitizes_newlines() {
        let ts = dt(2026, 11, 19, 21, 45, 12);
        assert_eq!(
            format_subtitle_log_line(ts, "夜深了，雨还在下。", "Đêm đã khuya, mưa vẫn còn rơi."),
            "[2026-11-19 21:45:12] 夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi.\n"
        );
        assert_eq!(
            format_subtitle_log_line(ts, "  第一行\r\n第二行  ", " Dòng 1\nDòng 2 "),
            "[2026-11-19 21:45:12] 第一行 第二行 : Dòng 1 Dòng 2\n"
        );
    }

    #[test]
    fn subtitle_logger_appends_and_flushes_utf8_immediately_without_closing() {
        let temp = TempDirGuard::new("flush");
        let log_dir = temp.path().join("nested_logs");
        let logger = SubtitleLogger::new(log_dir.clone());

        let path1 = match logger.log_pair_at(
            dt(2026, 11, 20, 18, 5, 1),
            "夜深了，雨还在下。",
            "Đêm đã khuya, mưa vẫn còn rơi.",
        ) {
            Ok(p) => p,
            Err(err) => panic!("expected first log write to succeed: {err}"),
        };
        assert_eq!(
            path1,
            daily_log_path(&log_dir, NaiveDate::from_ymd_opt(2026, 11, 20).unwrap())
        );

        let content_after_first = match std::fs::read_to_string(&path1) {
            Ok(s) => s,
            Err(err) => panic!("expected flushed log readable while logger stays alive: {err}"),
        };
        assert_eq!(
            content_after_first,
            "[2026-11-20 18:05:01] 夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi.\n"
        );

        let path2 = match logger.log_pair_at(
            dt(2026, 11, 20, 18, 5, 9),
            "明日再议。",
            "Ngày mai bàn tiếp.",
        ) {
            Ok(p) => p,
            Err(err) => panic!("expected second log write to succeed: {err}"),
        };
        assert_eq!(path2, path1);

        let content_after_second = match std::fs::read_to_string(&path1) {
            Ok(s) => s,
            Err(err) => panic!("expected updated log readable immediately: {err}"),
        };
        assert_eq!(
            content_after_second,
            "[2026-11-20 18:05:01] 夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi.\n\
             [2026-11-20 18:05:09] 明日再议。 : Ngày mai bàn tiếp.\n"
        );
    }

    #[test]
    fn subtitle_logger_rotates_daily_files_across_midnight() {
        let temp = TempDirGuard::new("rotate");
        let logger = SubtitleLogger::new(temp.path().to_path_buf());

        let day1 = match logger.log_pair_at(dt(2026, 11, 20, 23, 59, 58), "再见", "Tạm biệt")
        {
            Ok(p) => p,
            Err(err) => panic!("day1 write failed: {err}"),
        };
        let day2 = match logger.log_pair_at(dt(2026, 11, 21, 0, 0, 2), "早安", "Chào buổi sáng")
        {
            Ok(p) => p,
            Err(err) => panic!("day2 write failed: {err}"),
        };

        assert_ne!(day1, day2);
        assert_eq!(
            day1.file_name().and_then(|n| n.to_str()),
            Some("subtitles-2026-11-20.log")
        );
        assert_eq!(
            day2.file_name().and_then(|n| n.to_str()),
            Some("subtitles-2026-11-21.log")
        );
        assert_eq!(
            std::fs::read_to_string(&day1).ok().as_deref(),
            Some("[2026-11-20 23:59:58] 再见 : Tạm biệt\n")
        );
        assert_eq!(
            std::fs::read_to_string(&day2).ok().as_deref(),
            Some("[2026-11-21 00:00:02] 早安 : Chào buổi sáng\n")
        );
    }

    #[test]
    fn subtitle_logger_returns_typed_error_on_invalid_log_path() {
        let temp = TempDirGuard::new("errors");
        let logger_empty_check = SubtitleLogger::new(temp.path().to_path_buf());
        assert_eq!(
            logger_empty_check.log_pair_at(dt(2026, 11, 20, 12, 0, 0), "   ", "Xin chào"),
            Err(SubtitleLogError::EmptySubtitle { field: "chinese" })
        );
        assert_eq!(
            logger_empty_check.log_pair_at(dt(2026, 11, 20, 12, 0, 0), "你好", " \t "),
            Err(SubtitleLogError::EmptySubtitle {
                field: "vietnamese"
            })
        );

        let blocking_file = temp.path().join("not_a_directory");
        let Ok(()) = std::fs::write(&blocking_file, b"file-blocks-dir") else {
            panic!("failed creating blocking file");
        };
        let bad_logger = SubtitleLogger::new(blocking_file);
        assert!(matches!(
            bad_logger.log_pair_at(dt(2026, 11, 20, 12, 0, 0), "你好", "Xin chào"),
            Err(SubtitleLogError::Io { .. })
        ));
    }

    #[test]
    fn format_tts_retry_and_failure_lines_carry_attempt_status_voice_and_sentence() {
        use super::{format_tts_failure_line, format_tts_retry_line};
        use crate::api::{TtsError, TtsRetryAttempt};
        let ts = dt(2026, 11, 23, 15, 30, 0);
        assert_eq!(
            format_tts_retry_line(
                ts,
                "Xin chào, tôi là Tiểu Minh.",
                "vi-VN-NamMinhNeural",
                &TtsRetryAttempt {
                    attempt: 1,
                    status: Some(429),
                    detail: "Too Many Requests".to_string(),
                    backoff_ms: 500,
                },
            ),
            "[2026-11-23 15:30:00] TTS thử lại 1/3 (vi-VN-NamMinhNeural, HTTP 429, chờ 500ms): Xin chào, tôi là Tiểu Minh.\n"
        );
        // Network failure: no HTTP status.
        assert_eq!(
            format_tts_retry_line(
                ts,
                "Xin chào.",
                "vi-VN-NamMinhNeural",
                &TtsRetryAttempt {
                    attempt: 2,
                    status: None,
                    detail: "reset".to_string(),
                    backoff_ms: 1000,
                },
            ),
            "[2026-11-23 15:30:00] TTS thử lại 2/3 (vi-VN-NamMinhNeural, lỗi mạng, chờ 1000ms): Xin chào.\n"
        );
        // Failure after retries names the count; without retries it is omitted.
        let mut err = TtsError::Upstream {
            status: 429,
            detail: "Too Many Requests".to_string(),
            retries: vec![
                TtsRetryAttempt {
                    attempt: 1,
                    status: Some(429),
                    detail: "Too Many Requests".to_string(),
                    backoff_ms: 500,
                },
                TtsRetryAttempt {
                    attempt: 2,
                    status: Some(429),
                    detail: "Too Many Requests".to_string(),
                    backoff_ms: 1000,
                },
                TtsRetryAttempt {
                    attempt: 3,
                    status: Some(429),
                    detail: "Too Many Requests".to_string(),
                    backoff_ms: 2000,
                },
            ],
        };
        assert_eq!(
            format_tts_failure_line(ts, "Xin chào.", "vi-VN-NamMinhNeural", &err),
            "[2026-11-23 15:30:00] TTS THẤT BẠI (vi-VN-NamMinhNeural) sau 3 lần thử: lỗi tạo giọng đọc TTS (HTTP 429): Too Many Requests : Xin chào.\n"
        );
        err.set_retries(Vec::new());
        assert_eq!(
            format_tts_failure_line(ts, "Xin chào.", "vi-VN-NamMinhNeural", &err),
            "[2026-11-23 15:30:00] TTS THẤT BẠI (vi-VN-NamMinhNeural): lỗi tạo giọng đọc TTS (HTTP 429): Too Many Requests : Xin chào.\n"
        );
    }

    #[test]
    fn subtitle_logger_appends_tts_retry_and_failure_lines_to_the_daily_file() {
        use crate::api::{TtsError, TtsRetryAttempt};
        let temp = TempDirGuard::new("tts_log");
        let logger = SubtitleLogger::new(temp.path().to_path_buf());
        let path = match logger.log_pair_at(
            dt(2026, 11, 23, 15, 30, 0),
            "你好，我是小明。",
            "Xin chào, tôi là Tiểu Minh.",
        ) {
            Ok(p) => p,
            Err(err) => panic!("log_pair failed: {err}"),
        };
        logger
            .log_tts_retry_at(
                dt(2026, 11, 23, 15, 30, 0),
                "Xin chào, tôi là Tiểu Minh.",
                "vi-VN-NamMinhNeural",
                &TtsRetryAttempt {
                    attempt: 1,
                    status: Some(429),
                    detail: "Too Many Requests".to_string(),
                    backoff_ms: 500,
                },
            )
            .expect("log_tts_retry");
        logger
            .log_tts_failure_at(
                dt(2026, 11, 23, 15, 30, 0),
                "Xin chào, tôi là Tiểu Minh.",
                "vi-VN-NamMinhNeural",
                &TtsError::Upstream {
                    status: 429,
                    detail: "Too Many Requests".to_string(),
                    retries: vec![TtsRetryAttempt {
                        attempt: 1,
                        status: Some(429),
                        detail: "Too Many Requests".to_string(),
                        backoff_ms: 500,
                    }],
                },
            )
            .expect("log_tts_failure");
        let content = std::fs::read_to_string(&path).expect("read log file");
        assert_eq!(
            content,
            "[2026-11-23 15:30:00] 你好，我是小明。 : Xin chào, tôi là Tiểu Minh.\n\
             [2026-11-23 15:30:00] TTS thử lại 1/3 (vi-VN-NamMinhNeural, HTTP 429, chờ 500ms): Xin chào, tôi là Tiểu Minh.\n\
             [2026-11-23 15:30:00] TTS THẤT BẠI (vi-VN-NamMinhNeural) sau 1 lần thử: lỗi tạo giọng đọc TTS (HTTP 429): Too Many Requests : Xin chào, tôi là Tiểu Minh.\n"
        );
    }
}
