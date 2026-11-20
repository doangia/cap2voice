//! Daily UTF-8 `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` subtitle logger.

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
}
