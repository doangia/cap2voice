//! Daily UTF-8 `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` subtitle logger.
//!
//! The same daily file also carries TTS diagnostics: a retry line for each
//! transient Edge-TTS failure the client rode out, and a failure line when
//! a sentence ultimately lost its audio. Both keep the file's shape — one
//! sanitized single line per event, same `[timestamp]` prefix, same daily
//! rotation — so the log stays one chronological stream instead of a
//! second format.

use crate::api::{TtsError, TtsRetryAttempt, TTS_MAX_RETRIES};
use crate::ocr::{GeometricSplitAction, GeometricSplitDecision, GeometricSplitKind};
use chrono::{NaiveDate, NaiveDateTime};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;
use wcocr_rs::TextBlock;

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

/// Build the daily raw-OCR debug log file path (`ocr-debug-YYYY-MM-DD.log`).
pub fn daily_ocr_debug_path(log_dir: &Path, date: NaiveDate) -> PathBuf {
    log_dir.join(format!("ocr-debug-{}.log", date.format("%Y-%m-%d")))
}

/// Escape raw OCR text for a single debug-log line: newlines become
/// spaces, backslashes and double quotes are escaped, so one OCR call
/// stays exactly one line.
fn escape_debug_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\r' | '\n' => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Format one raw-OCR debug line, e.g.
/// `[14:40:46.353] OCR blocks=2 | [222,29-305,74 r=0.99] "你好。" | [220,63-310,109 r=0.99] "再见。" | lines=["你好。", "再见。"]`
///
/// Block text is the raw OCR text, before CJK filtering; `lines` are the
/// filtered Chinese lines the pipeline derived from those blocks. A frame
/// with no blocks logs `OCR blocks=0 | lines=[]`, so an OCR miss is
/// distinguishable from a pixel-diff skip (no line at all), and blocks
/// with no lines pin the miss on the CJK filter instead of the engine.
pub fn format_ocr_debug_line(
    timestamp: NaiveDateTime,
    blocks: &[TextBlock],
    lines: &[String],
) -> String {
    let mut out = format!(
        "[{}] OCR blocks={}",
        timestamp.format("%H:%M:%S%.3f"),
        blocks.len()
    );
    for block in blocks {
        out.push_str(&format!(
            " | [{:.0},{:.0}-{:.0},{:.0} r={:.2}] \"{}\"",
            block.left,
            block.top,
            block.right,
            block.bottom,
            block.rate,
            escape_debug_text(block.text.trim())
        ));
    }
    let quoted: Vec<String> = lines
        .iter()
        .map(|line| format!("\"{}\"", escape_debug_text(line)))
        .collect();
    out.push_str(&format!(" | lines=[{}]\n", quoted.join(", ")));
    out
}

/// Append one pre-formatted line to the daily file at `path` (opened or
/// rotated as needed), flushing immediately. Shared by the subtitle log
/// and the OCR debug log so the rotation behavior stays identical.
fn append_line_to_daily_file(
    state: &Mutex<Option<OpenDailyFile>>,
    log_dir: &Path,
    date: NaiveDate,
    path: PathBuf,
    line: String,
) -> Result<PathBuf, SubtitleLogError> {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let needs_reopen = match guard.as_ref() {
        Some(open) => open.date != date,
        None => true,
    };
    if needs_reopen {
        std::fs::create_dir_all(log_dir).map_err(|err| SubtitleLogError::Io {
            path: log_dir.to_path_buf(),
            detail: err.to_string(),
        })?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| SubtitleLogError::Io {
                path: path.clone(),
                detail: err.to_string(),
            })?;
        *guard = Some(OpenDailyFile {
            date,
            path: path.clone(),
            file,
        });
    }

    let Some(open) = guard.as_mut() else {
        return Err(SubtitleLogError::Io {
            path,
            detail: "open daily log file handle missing".to_string(),
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
        let path = daily_log_path(&self.log_dir, date);
        append_line_to_daily_file(&self.state, &self.log_dir, date, path, line)
    }
}

/// Thread-safe daily raw-OCR debug log writer (merge fix, 2026-11-25).
///
/// One line per OCR call, written next to the subtitle log with the same
/// daily rotation and immediate flush. The file is opened lazily on the
/// first OCR call, so an idle pipeline writes nothing and creates no
/// file; when OCR runs, each call costs one short line. Only OCR text is
/// ever logged — never audio, never translations.
pub struct OcrDebugLogger {
    log_dir: PathBuf,
    state: Mutex<Option<OpenDailyFile>>,
}

impl OcrDebugLogger {
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            state: Mutex::new(None),
        }
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Append and flush one raw-OCR debug line using the current local
    /// timestamp. Best-effort diagnostics: the caller ignores the error.
    pub fn log_blocks(
        &self,
        blocks: &[TextBlock],
        lines: &[String],
    ) -> Result<PathBuf, SubtitleLogError> {
        let now = chrono::Local::now().naive_local();
        self.log_blocks_at(now, blocks, lines)
    }

    /// Append and flush one raw-OCR debug line at an explicit timestamp.
    pub fn log_blocks_at(
        &self,
        timestamp: NaiveDateTime,
        blocks: &[TextBlock],
        lines: &[String],
    ) -> Result<PathBuf, SubtitleLogError> {
        let line = format_ocr_debug_line(timestamp, blocks, lines);
        self.write_debug_line(timestamp, line)
    }

    /// Append one pre-formatted debug line at an explicit timestamp
    /// (shared by every debug-line kind below).
    fn write_debug_line(
        &self,
        timestamp: NaiveDateTime,
        line: String,
    ) -> Result<PathBuf, SubtitleLogError> {
        let date = timestamp.date();
        let path = daily_ocr_debug_path(&self.log_dir, date);
        append_line_to_daily_file(&self.state, &self.log_dir, date, path, line)
    }

    /// Current local timestamp plus its `[HH:MM:SS.mmm]` rendering, for
    /// the decision lines (all share the same clock as the OCR lines).
    fn now_debug_timestamp() -> (NaiveDateTime, String) {
        let now = chrono::Local::now().naive_local();
        (now, now.format("%H:%M:%S%.3f").to_string())
    }

    /// Aggregate line for a run of frames skipped by the pixel-diff gate
    /// (the subtitle was static, so OCR did not run). One line per run,
    /// not one per frame, so the log does not flood while idle.
    pub fn log_pixel_diff_skipped(&self, frames: u32) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        self.write_debug_line(now, format!("[{ts}] PIXEL-DIFF skipped {frames} frames\n"))
    }

    /// A line the fuzzy dedupe swallowed as a re-read of the previous
    /// line, with the similarity that decided it.
    pub fn log_dedupe_dropped(
        &self,
        text: &str,
        similarity: Option<f32>,
    ) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        let sim = similarity.map_or(String::new(), |s| format!(" sim={s:.2}"));
        self.write_debug_line(
            now,
            format!(
                "[{ts}] DEDUPE dropped \"{}\"{sim}\n",
                escape_debug_text(text)
            ),
        )
    }

    /// A held line the hold-and-replace debounce replaced with a newer
    /// line (transitional garbage suppressed).
    pub fn log_hold_replaced(&self, old: &str, new: &str) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        self.write_debug_line(
            now,
            format!(
                "[{ts}] HOLD replaced \"{}\" with \"{}\"\n",
                escape_debug_text(old),
                escape_debug_text(new)
            ),
        )
    }

    /// One OCR line the speaker rules split into several utterances.
    pub fn log_speaker_split(
        &self,
        original: &str,
        pieces: &[String],
    ) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        let quoted: Vec<String> = pieces
            .iter()
            .map(|piece| format!("\"{}\"", escape_debug_text(piece)))
            .collect();
        self.write_debug_line(
            now,
            format!(
                "[{ts}] SPEAKER split \"{}\" into [{}]\n",
                escape_debug_text(original),
                quoted.join(", ")
            ),
        )
    }

    /// What the `SentenceBuffer` decided about one fragment: the relation
    /// (`Started` when the buffer was empty), the fragment, the sentences
    /// that completed from it, and what stays buffered.
    pub fn log_buffer_decision(
        &self,
        relation: &str,
        fragment: &str,
        completed: &[String],
        buffer: &str,
    ) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        let flushed: Vec<String> = completed
            .iter()
            .map(|text| format!("\"{}\"", escape_debug_text(text)))
            .collect();
        self.write_debug_line(
            now,
            format!(
                "[{ts}] BUFFER {relation} frag=\"{}\" flushed=[{}] buf=\"{}\"\n",
                escape_debug_text(fragment),
                flushed.join(", "),
                escape_debug_text(buffer)
            ),
        )
    }

    /// A buffered sentence completed by a blank run or by the timeout
    /// (`reason` is `blank` or `timeout`).
    pub fn log_buffer_flush(&self, reason: &str, text: &str) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        self.write_debug_line(
            now,
            format!(
                "[{ts}] BUFFER flush({reason}) \"{}\"\n",
                escape_debug_text(text)
            ),
        )
    }

    /// One geometric split/join decision (merge fix, 2026-11-25): the gap
    /// that was judged, the threshold it was judged against, the boxes on
    /// either side, and the resulting texts. `kind` is `block-gap`
    /// (between two blocks/pieces on one row) or `char-gap` (between two
    /// character boxes inside one block); `action` is `split` or `joined`.
    pub fn log_geometric_split(
        &self,
        decision: &GeometricSplitDecision,
    ) -> Result<PathBuf, SubtitleLogError> {
        let (now, ts) = Self::now_debug_timestamp();
        let kind = match decision.kind {
            GeometricSplitKind::BlockGap => "block-gap",
            GeometricSplitKind::CharGap => "char-gap",
        };
        let action = match decision.action {
            GeometricSplitAction::Split => "split",
            GeometricSplitAction::Joined => "joined",
        };
        let (ll, lt, lr, lb) = decision.left_box;
        let (rl, rt, rr, rb) = decision.right_box;
        self.write_debug_line(
            now,
            format!(
                "[{ts}] GEO {action} {kind} gap={:.0}px thr={:.0}px boxes=[{ll:.0},{lt:.0}-{lr:.0},{lb:.0}] [{rl:.0},{rt:.0}-{rr:.0},{rb:.0}] \"{}\" | \"{}\"\n",
                decision.gap_px,
                decision.threshold_px,
                escape_debug_text(&decision.left_text),
                escape_debug_text(&decision.right_text)
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        daily_log_path, daily_ocr_debug_path, format_ocr_debug_line, format_subtitle_log_line,
        GeometricSplitAction, GeometricSplitDecision, GeometricSplitKind, OcrDebugLogger,
        SubtitleLogError, SubtitleLogger,
    };
    use chrono::NaiveDate;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use wcocr_rs::TextBlock;

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
    fn format_ocr_debug_line_matches_known_24_11_format() {
        // The 24/11 debug-trace build wrote these exact lines; the real
        // logger must keep the same shape so old and new files compare.
        let ts = NaiveDate::from_ymd_opt(2026, 11, 24)
            .unwrap()
            .and_hms_milli_opt(14, 40, 46, 353)
            .unwrap();
        let blocks = vec![TextBlock {
            left: 192.0,
            top: 33.0,
            right: 363.0,
            bottom: 71.0,
            rate: 1.0,
            text: "你好，我是".to_string(),
            details: vec![],
        }];
        assert_eq!(
            format_ocr_debug_line(ts, &blocks, &["你好，我是".to_string()]),
            "[14:40:46.353] OCR blocks=1 | [192,33-363,71 r=1.00] \"你好，我是\" | lines=[\"你好，我是\"]\n"
        );

        // Two blocks, one per visual line; an empty result pins an OCR miss.
        let ts2 = NaiveDate::from_ymd_opt(2026, 11, 24)
            .unwrap()
            .and_hms_milli_opt(14, 40, 56, 331)
            .unwrap();
        let blocks2 = vec![
            TextBlock {
                left: 222.0,
                top: 29.0,
                right: 305.0,
                bottom: 74.0,
                rate: 0.99,
                text: "你好。".to_string(),
                details: vec![],
            },
            TextBlock {
                left: 220.0,
                top: 63.0,
                right: 310.0,
                bottom: 109.0,
                rate: 0.99,
                text: "再见。".to_string(),
                details: vec![],
            },
        ];
        assert_eq!(
            format_ocr_debug_line(
                ts2,
                &blocks2,
                &["你好。".to_string(), "再见。".to_string()]
            ),
            "[14:40:56.331] OCR blocks=2 | [222,29-305,74 r=0.99] \"你好。\" | [220,63-310,109 r=0.99] \"再见。\" | lines=[\"你好。\", \"再见。\"]\n"
        );
        assert_eq!(
            format_ocr_debug_line(ts2, &[], &[]),
            "[14:40:56.331] OCR blocks=0 | lines=[]\n"
        );
        // Blocks but no lines: the CJK filter dropped everything.
        assert_eq!(
            format_ocr_debug_line(ts2, &blocks2[..1], &[]),
            "[14:40:56.331] OCR blocks=1 | [222,29-305,74 r=0.99] \"你好。\" | lines=[]\n"
        );
    }

    #[test]
    fn ocr_debug_logger_appends_and_rotates_daily_files() {
        let temp = TempDirGuard::new("ocr_dbg");
        let logger = OcrDebugLogger::new(temp.path().to_path_buf());
        let block = TextBlock {
            left: 192.0,
            top: 33.0,
            right: 363.0,
            bottom: 71.0,
            rate: 1.0,
            text: "你好，我是".to_string(),
            details: vec![],
        };

        let day1 = match logger.log_blocks_at(
            dt(2026, 11, 24, 23, 59, 58),
            std::slice::from_ref(&block),
            &["你好，我是".to_string()],
        ) {
            Ok(p) => p,
            Err(err) => panic!("day1 write failed: {err}"),
        };
        let day2 = match logger.log_blocks_at(dt(2026, 11, 25, 0, 0, 2), &[], &[]) {
            Ok(p) => p,
            Err(err) => panic!("day2 write failed: {err}"),
        };

        assert_ne!(day1, day2);
        assert_eq!(
            day1.file_name().and_then(|n| n.to_str()),
            Some("ocr-debug-2026-11-24.log")
        );
        assert_eq!(
            day2.file_name().and_then(|n| n.to_str()),
            Some("ocr-debug-2026-11-25.log")
        );
        assert_eq!(
            std::fs::read_to_string(&day1).ok().as_deref(),
            Some("[23:59:58.000] OCR blocks=1 | [192,33-363,71 r=1.00] \"你好，我是\" | lines=[\"你好，我是\"]\n")
        );
        assert_eq!(
            std::fs::read_to_string(&day2).ok().as_deref(),
            Some("[00:00:02.000] OCR blocks=0 | lines=[]\n")
        );
        assert_eq!(
            daily_ocr_debug_path(temp.path(), NaiveDate::from_ymd_opt(2026, 11, 24).unwrap()),
            day1
        );
    }

    #[test]
    fn ocr_debug_logger_decision_lines_carry_the_pipeline_reason() {
        // Decision lines use the current clock, so the test strips the
        // `[HH:MM:SS.mmm] ` prefix and compares the rest.
        fn strip_debug_timestamp(line: &str) -> &str {
            match line.find("] ") {
                Some(idx) => &line[idx + 2..],
                None => line,
            }
        }
        let temp = TempDirGuard::new("ocr_dbg_decisions");
        let logger = OcrDebugLogger::new(temp.path().to_path_buf());
        let _ = logger.log_pixel_diff_skipped(87).unwrap();
        let _ = logger.log_dedupe_dropped("你好吗", Some(0.97)).unwrap();
        let _ = logger.log_dedupe_dropped("再见", None).unwrap();
        let _ = logger
            .log_hold_replaced("对不起时说找来晚了。", "对不起，我来晚了。")
            .unwrap();
        let _ = logger
            .log_speaker_split(
                "你先睡吧 不用等我吃饭了",
                &["你先睡吧".to_string(), "不用等我吃饭了".to_string()],
            )
            .unwrap();
        let _ = logger
            .log_buffer_decision(
                "NewSubtitle",
                "哦 挺好的嘛",
                &["啊 下个月啊到金泽住一晚吗".to_string()],
                "哦 挺好的嘛",
            )
            .unwrap();
        let _ = logger.log_buffer_flush("blank", "你好吗").unwrap();
        let _ = logger
            .log_geometric_split(&GeometricSplitDecision {
                kind: GeometricSplitKind::BlockGap,
                action: GeometricSplitAction::Split,
                gap_px: 82.0,
                threshold_px: 19.5,
                left_box: (95.0, 85.0, 212.0, 128.0),
                right_box: (294.0, 86.0, 410.0, 129.0),
                left_text: "你好吗".to_string(),
                right_text: "我很好".to_string(),
            })
            .unwrap();
        let _ = logger
            .log_geometric_split(&GeometricSplitDecision {
                kind: GeometricSplitKind::CharGap,
                action: GeometricSplitAction::Split,
                gap_px: 91.0,
                threshold_px: 19.0,
                left_box: (165.0, 85.0, 203.0, 127.0),
                right_box: (294.0, 86.0, 332.0, 128.0),
                left_text: "你好吗".to_string(),
                right_text: "我很好".to_string(),
            })
            .unwrap();

        let path = daily_ocr_debug_path(temp.path(), chrono::Local::now().date_naive());
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => panic!("failed reading {}: {err}", path.display()),
        };
        let stripped: Vec<&str> = content.lines().map(strip_debug_timestamp).collect();
        assert_eq!(
            stripped,
            vec![
                "PIXEL-DIFF skipped 87 frames",
                "DEDUPE dropped \"你好吗\" sim=0.97",
                "DEDUPE dropped \"再见\"",
                "HOLD replaced \"对不起时说找来晚了。\" with \"对不起，我来晚了。\"",
                "SPEAKER split \"你先睡吧 不用等我吃饭了\" into [\"你先睡吧\", \"不用等我吃饭了\"]",
                "BUFFER NewSubtitle frag=\"哦 挺好的嘛\" flushed=[\"啊 下个月啊到金泽住一晚吗\"] buf=\"哦 挺好的嘛\"",
                "BUFFER flush(blank) \"你好吗\"",
                "GEO split block-gap gap=82px thr=20px boxes=[95,85-212,128] [294,86-410,129] \"你好吗\" | \"我很好\"",
                "GEO split char-gap gap=91px thr=19px boxes=[165,85-203,127] [294,86-332,128] \"你好吗\" | \"我很好\"",
            ]
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
