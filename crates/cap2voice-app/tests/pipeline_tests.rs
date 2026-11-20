//! Async integration tests for `PipelineRunner`, sequence guard, `auto_read`,
//! error surfacing, live settings updates, and clean stop, plus real-PC 60s smoke test.

use cap2voice_app::api::{
    BoxFuture, HttpTranslator, HttpTtsEngine, ServerProcess, ServerProcessConfig, TranslateError,
    Translator, TtsEngine, TtsError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_TRANSLATE_TIMEOUT,
    DEFAULT_TTS_TIMEOUT,
};
use cap2voice_app::audio::{AudioError, AudioPlayback, AudioPlayer, EnqueueOutcome};
use cap2voice_app::capture::{
    CaptureError, CapturedFrame, FrameCapturer, MonitorInfo, XcapCapturer,
};
use cap2voice_app::config::{
    default_config_path, default_log_dir, load_config_from, CaptureRegion,
};
use cap2voice_app::ocr::{
    resolve_wechat_paths_from_config, OcrAdapterError, OcrEngine, OcrError, OcrResult, TextBlock,
    WeChatOcrAdapter,
};
use cap2voice_app::pipeline::{
    daily_log_path, PipelineDeps, PipelineErrorKind, PipelineRunner, PipelineSettings,
    PipelineSnapshot, PipelineStatus, SubtitleLogger, ERR_BANNER_SERVER_UNAVAILABLE,
    ERR_BANNER_TRANSLATE_NETWORK, ERR_BANNER_TRANSLATE_RATE_LIMIT,
};
use chrono::Local;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Notify};

const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

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
            "cap2voice_pipe_{label}_{}_{}_{}",
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

fn solid_frame(luma: u8) -> CapturedFrame {
    let w = 64u32;
    let h = 20u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[luma, luma, luma, 255]);
    }
    match CapturedFrame::new(w, h, rgba) {
        Ok(frame) => frame,
        Err(err) => panic!("failed building test frame: {err}"),
    }
}

fn frame_with_band(base_luma: u8, band_luma: u8) -> CapturedFrame {
    let w = 64u32;
    let h = 20u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let v = if (6..14).contains(&y) {
            band_luma
        } else {
            base_luma
        };
        for _ in 0..w {
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
    }
    match CapturedFrame::new(w, h, rgba) {
        Ok(frame) => frame,
        Err(err) => panic!("failed building test frame: {err}"),
    }
}

fn ocr_block(text: &str) -> OcrResult {
    OcrResult {
        errcode: 0,
        imgpath: "frame.png".to_string(),
        width: 64,
        height: 20,
        ocr_response: vec![TextBlock {
            left: 4.0,
            top: 4.0,
            right: 60.0,
            bottom: 18.0,
            rate: 0.99,
            text: text.to_string(),
            details: Vec::new(),
        }],
    }
}

fn test_region() -> CaptureRegion {
    CaptureRegion {
        monitor_id: "TEST_MON".to_string(),
        x: 0,
        y: 0,
        width: 64,
        height: 20,
    }
}

fn test_settings(fps: u32) -> PipelineSettings {
    PipelineSettings {
        region: test_region(),
        capture_fps: fps,
        pixel_diff_threshold: 0.02,
        dedupe_threshold: 0.93,
        blank_reset_frames: 5,
        voice: "vi-VN-NamMinhNeural".to_string(),
        speech_rate: "+0%".to_string(),
        auto_read: true,
        muted: false,
        volume: 0.75,
        enable_subtitle_log: true,
        rate_limit_backoff: Duration::from_millis(20),
    }
}

struct MockCapturer {
    scripted: Mutex<VecDeque<Result<CapturedFrame, CaptureError>>>,
    fallback: Mutex<Result<CapturedFrame, CaptureError>>,
    calls: AtomicU64,
}

impl MockCapturer {
    fn new(
        scripted: Vec<Result<CapturedFrame, CaptureError>>,
        fallback: Result<CapturedFrame, CaptureError>,
    ) -> Self {
        Self {
            scripted: Mutex::new(VecDeque::from(scripted)),
            fallback: Mutex::new(fallback),
            calls: AtomicU64::new(0),
        }
    }

    fn set_fallback(&self, next: Result<CapturedFrame, CaptureError>) {
        *self.fallback.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }

    fn push_frame(&self, item: Result<CapturedFrame, CaptureError>) {
        self.scripted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(item);
    }

    fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl FrameCapturer for MockCapturer {
    fn list_monitors(&self) -> Result<Vec<MonitorInfo>, CaptureError> {
        Ok(vec![MonitorInfo {
            id: "TEST_MON".to_string(),
            name: "Test".to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 1.0,
            is_primary: true,
        }])
    }

    fn capture_region(&self, _region: &CaptureRegion) -> Result<CapturedFrame, CaptureError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.scripted.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next) = queue.pop_front() {
            return next;
        }
        self.fallback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

struct MockOcrEngine {
    scripted: Mutex<VecDeque<Result<OcrResult, OcrAdapterError>>>,
    fallback: Mutex<Result<OcrResult, OcrAdapterError>>,
    calls: AtomicU64,
}

impl MockOcrEngine {
    fn new(
        scripted: Vec<Result<OcrResult, OcrAdapterError>>,
        fallback: Result<OcrResult, OcrAdapterError>,
    ) -> Self {
        Self {
            scripted: Mutex::new(VecDeque::from(scripted)),
            fallback: Mutex::new(fallback),
            calls: AtomicU64::new(0),
        }
    }

    fn set_fallback(&self, next: Result<OcrResult, OcrAdapterError>) {
        *self.fallback.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }

    fn push_result(&self, item: Result<OcrResult, OcrAdapterError>) {
        self.scripted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(item);
    }

    fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl OcrEngine for MockOcrEngine {
    fn recognize_frame(&self, _frame: &CapturedFrame) -> Result<OcrResult, OcrAdapterError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.scripted.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next) = queue.pop_front() {
            return next;
        }
        self.fallback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

struct PendingTranslate {
    chinese: String,
    tx: tokio::sync::oneshot::Sender<Result<String, TranslateError>>,
}

struct ControllableTranslator {
    immediate: Mutex<VecDeque<Result<String, TranslateError>>>,
    manual_mode: AtomicBool,
    pending: Mutex<Vec<PendingTranslate>>,
    notify_in: Notify,
    calls: Mutex<Vec<String>>,
}

impl ControllableTranslator {
    fn scripted(responses: Vec<Result<String, TranslateError>>) -> Self {
        Self {
            immediate: Mutex::new(VecDeque::from(responses)),
            manual_mode: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
            notify_in: Notify::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn manual() -> Self {
        Self {
            immediate: Mutex::new(VecDeque::new()),
            manual_mode: AtomicBool::new(true),
            pending: Mutex::new(Vec::new()),
            notify_in: Notify::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    async fn wait_for_pending(&self, count: usize) {
        let wait = async {
            loop {
                let notified = self.notify_in.notified();
                if self.pending.lock().unwrap_or_else(|e| e.into_inner()).len() >= count {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(TEST_WAIT_TIMEOUT, wait).await.is_err() {
            panic!("timed out waiting for {count} pending translate requests");
        }
    }

    fn resolve_at(&self, index: usize, result: Result<String, TranslateError>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let item = pending.remove(index);
        let _ = item.tx.send(result);
    }

    fn pending_chinese(&self) -> Vec<String> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|p| p.chinese.clone())
            .collect()
    }

    fn push_response(&self, result: Result<String, TranslateError>) {
        self.immediate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(result);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Translator for ControllableTranslator {
    fn translate<'a>(&'a self, chinese: &'a str) -> BoxFuture<'a, Result<String, TranslateError>> {
        let owned = chinese.to_string();
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(owned.clone());

            if self.manual_mode.load(Ordering::Acquire) {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(PendingTranslate { chinese: owned, tx });
                self.notify_in.notify_waiters();
                return match rx.await {
                    Ok(res) => res,
                    Err(_) => Err(TranslateError::ServerUnavailable {
                        detail: "aborted".to_string(),
                    }),
                };
            }

            let next = self
                .immediate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            match next {
                Some(res) => res,
                None => Ok(format!("VI({owned})")),
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedTtsCall {
    text: String,
    voice: String,
    rate: String,
}

struct MockTtsEngine {
    scripted: Mutex<VecDeque<Result<Vec<u8>, TtsError>>>,
    calls: Mutex<Vec<RecordedTtsCall>>,
}

impl MockTtsEngine {
    fn new(scripted: Vec<Result<Vec<u8>, TtsError>>) -> Self {
        Self {
            scripted: Mutex::new(VecDeque::from(scripted)),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<RecordedTtsCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl TtsEngine for MockTtsEngine {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, TtsError>> {
        let req = RecordedTtsCall {
            text: text.to_string(),
            voice: voice.to_string(),
            rate: rate.to_string(),
        };
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(req.clone());
            let next = self
                .scripted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            match next {
                Some(res) => res,
                None => Ok(format!("MP3({}:{})", req.voice, req.text).into_bytes()),
            }
        })
    }
}

struct MockAudioPlayback {
    enqueued: Mutex<Vec<Vec<u8>>>,
    enqueue_results: Mutex<VecDeque<Result<EnqueueOutcome, AudioError>>>,
    stop_count: AtomicU64,
    volume: Mutex<f32>,
    last_error: Mutex<Option<AudioError>>,
}

impl MockAudioPlayback {
    fn new() -> Self {
        Self {
            enqueued: Mutex::new(Vec::new()),
            enqueue_results: Mutex::new(VecDeque::new()),
            stop_count: AtomicU64::new(0),
            volume: Mutex::new(0.75),
            last_error: Mutex::new(None),
        }
    }

    fn push_enqueue_result(&self, res: Result<EnqueueOutcome, AudioError>) {
        self.enqueue_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(res);
    }

    fn enqueued_strings(&self) -> Vec<String> {
        self.enqueued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    fn stop_calls(&self) -> u64 {
        self.stop_count.load(Ordering::Relaxed)
    }
}

impl AudioPlayback for MockAudioPlayback {
    fn enqueue_mp3(&self, mp3_bytes: Vec<u8>) -> Result<EnqueueOutcome, AudioError> {
        if let Some(res) = self
            .enqueue_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            return res;
        }
        let mut enqueued = self.enqueued.lock().unwrap_or_else(|e| e.into_inner());
        enqueued.push(mp3_bytes);
        Ok(EnqueueOutcome {
            pending_len: enqueued.len().min(3),
            dropped_oldest: false,
        })
    }

    fn set_volume(&self, volume: f32) -> Result<(), AudioError> {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner()) = volume;
        Ok(())
    }

    fn volume(&self) -> f32 {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stop_and_clear(&self) -> Result<(), AudioError> {
        self.stop_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn is_playing(&self) -> bool {
        false
    }

    fn pending_len(&self) -> usize {
        0
    }

    fn is_idle(&self) -> bool {
        true
    }

    fn last_error(&self) -> Option<AudioError> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn take_last_error(&self) -> Option<AudioError> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

async fn wait_for_snapshot(
    rx: &mut watch::Receiver<PipelineSnapshot>,
    label: &str,
    mut pred: impl FnMut(&PipelineSnapshot) -> bool,
) -> PipelineSnapshot {
    let wait = async {
        loop {
            let current = rx.borrow_and_update().clone();
            if pred(&current) {
                return current;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    };
    match tokio::time::timeout(TEST_WAIT_TIMEOUT, wait).await {
        Ok(snap) => snap,
        Err(_) => {
            panic!(
                "timed out waiting for snapshot condition '{label}'; latest={:?}",
                *rx.borrow()
            );
        }
    }
}

#[tokio::test]
async fn pipeline_happy_path_captures_skips_static_frames_translates_speaks_and_logs_utf8() {
    let temp = TempDirGuard::new("happy");
    let capturer = Arc::new(MockCapturer::new(
        vec![
            Ok(solid_frame(20)),
            Ok(solid_frame(20)),
            Ok(solid_frame(20)),
            Ok(solid_frame(20)),
            Ok(solid_frame(20)),
            Ok(frame_with_band(20, 230)),
        ],
        Ok(frame_with_band(20, 230)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![
            Ok(ocr_block("夜深了，雨还在下。")),
            Ok(ocr_block("明日再议。")),
        ],
        Ok(ocr_block("明日再议。")),
    ));
    let translator = Arc::new(ControllableTranslator::scripted(vec![
        Ok("Đêm đã khuya, mưa vẫn còn rơi.".to_string()),
        Ok("Ngày mai bàn tiếp.".to_string()),
    ]));
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer: capturer.clone(),
            ocr: ocr.clone(),
            translator: translator.clone(),
            tts: tts.clone(),
            audio: audio.clone(),
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let snap = wait_for_snapshot(&mut rx, "two pairs translated, spoken, and logged", |s| {
        s.metrics.logged_pairs == 2
            && s.metrics.tts_calls == 2
            && s.metrics.frames_skipped_pixel_diff >= 4
    })
    .await;

    assert_eq!(snap.status, PipelineStatus::Running);
    assert_eq!(snap.chinese_line, "明日再议。");
    assert_eq!(snap.vietnamese_line, "Ngày mai bàn tiếp.");
    assert_eq!(snap.error_banner, None);
    assert_eq!(ocr.call_count(), 2);
    assert_eq!(snap.metrics.ocr_calls, 2);
    assert_eq!(snap.metrics.subtitles_emitted, 2);
    assert_eq!(snap.metrics.translations_succeeded, 2);
    assert_eq!(snap.metrics.tts_calls, 2);
    assert_eq!(
        translator.calls(),
        vec!["夜深了，雨还在下。".to_string(), "明日再议。".to_string()]
    );

    assert_eq!(
        audio.enqueued_strings(),
        vec![
            "MP3(vi-VN-NamMinhNeural:Đêm đã khuya, mưa vẫn còn rơi.)".to_string(),
            "MP3(vi-VN-NamMinhNeural:Ngày mai bàn tiếp.)".to_string(),
        ]
    );

    let today_log = daily_log_path(temp.path(), Local::now().date_naive());
    let content = match std::fs::read_to_string(&today_log) {
        Ok(s) => s,
        Err(err) => panic!("failed reading daily log {}: {err}", today_log.display()),
    };
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("] 夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi."));
    assert!(lines[1].ends_with("] 明日再议。 : Ngày mai bàn tiếp."));

    handle.stop().await;
    assert_eq!(rx.borrow().status, PipelineStatus::Idle);
}

#[tokio::test]
async fn pipeline_discards_out_of_order_translation_response_without_overwriting_newer_subtitle() {
    let temp = TempDirGuard::new("ooo");
    let capturer = Arc::new(MockCapturer::new(
        vec![Ok(solid_frame(10)), Ok(frame_with_band(10, 220))],
        Ok(frame_with_band(10, 220)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Ok(ocr_block("第一句。")), Ok(ocr_block("第二句。"))],
        Ok(ocr_block("第二句。")),
    ));
    let translator = Arc::new(ControllableTranslator::manual());
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer,
            ocr,
            translator: translator.clone(),
            tts: tts.clone(),
            audio: audio.clone(),
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    translator.wait_for_pending(2).await;
    assert_eq!(
        translator.pending_chinese(),
        vec!["第一句。".to_string(), "第二句。".to_string()]
    );

    let mut rx = handle.subscribe();
    // Resolve newer seq 2 FIRST, then resolve stale seq 1 LATE.
    translator.resolve_at(1, Ok("Câu thứ hai.".to_string()));
    let after_seq2 = wait_for_snapshot(&mut rx, "seq 2 translated", |s| {
        s.latest_seq == 2 && s.vietnamese_line == "Câu thứ hai." && s.metrics.logged_pairs == 1
    })
    .await;
    assert_eq!(after_seq2.chinese_line, "第二句。");

    translator.resolve_at(0, Ok("Câu thứ nhất ĐẾN TRỄ.".to_string()));
    let after_stale = wait_for_snapshot(&mut rx, "stale seq 1 discarded", |s| {
        s.metrics.stale_translations_discarded == 1
    })
    .await;

    assert_eq!(after_stale.chinese_line, "第二句。");
    assert_eq!(after_stale.vietnamese_line, "Câu thứ hai.");
    assert_eq!(after_stale.metrics.translations_succeeded, 1);
    assert_eq!(after_stale.metrics.logged_pairs, 1);
    assert_eq!(tts.calls().len(), 1);
    assert_eq!(tts.calls()[0].text, "Câu thứ hai.");
    assert_eq!(
        audio.enqueued_strings(),
        vec!["MP3(vi-VN-NamMinhNeural:Câu thứ hai.)".to_string()]
    );

    let today_log = daily_log_path(temp.path(), Local::now().date_naive());
    let content = std::fs::read_to_string(&today_log).unwrap_or_default();
    assert!(!content.contains("ĐẾN TRỄ"));
    assert!(content.contains("第二句。 : Câu thứ hai."));

    handle.stop().await;
}

#[tokio::test]
async fn pipeline_auto_read_false_makes_zero_tts_calls_while_translating_and_logging() {
    let temp = TempDirGuard::new("no_tts");
    let capturer = Arc::new(MockCapturer::new(
        vec![Ok(solid_frame(10))],
        Ok(solid_frame(10)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Ok(ocr_block("你好世界。"))],
        Ok(ocr_block("你好世界。")),
    ));
    let translator = Arc::new(ControllableTranslator::scripted(vec![Ok(
        "Xin chào thế giới.".to_string(),
    )]));
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let mut settings = test_settings(60);
    settings.auto_read = false;

    let handle = match PipelineRunner::start(
        settings,
        PipelineDeps {
            capturer,
            ocr,
            translator,
            tts: tts.clone(),
            audio: audio.clone(),
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let snap = wait_for_snapshot(&mut rx, "translated and logged with auto_read=false", |s| {
        s.metrics.logged_pairs == 1
    })
    .await;

    assert_eq!(snap.chinese_line, "你好世界。");
    assert_eq!(snap.vietnamese_line, "Xin chào thế giới.");
    assert_eq!(snap.metrics.tts_calls, 0);
    assert!(tts.calls().is_empty());
    assert!(audio.enqueued_strings().is_empty());

    handle.stop().await;
}

#[tokio::test]
async fn pipeline_translation_and_network_errors_clear_stale_vietnamese_and_emit_vietnamese_banners_with_429_backoff(
) {
    let temp = TempDirGuard::new("tr_err");
    let capturer = Arc::new(MockCapturer::new(
        vec![Ok(solid_frame(10)), Ok(frame_with_band(10, 220))],
        Ok(frame_with_band(10, 220)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Ok(ocr_block("第一句。")), Ok(ocr_block("第二句。"))],
        Ok(ocr_block("第二句。")),
    ));
    let translator = Arc::new(ControllableTranslator::scripted(vec![
        Ok("Bản dịch cũ hợp lệ.".to_string()),
        Err(TranslateError::Upstream {
            status: 502,
            detail: "network offline".to_string(),
        }),
    ]));
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer: capturer.clone(),
            ocr: ocr.clone(),
            translator: translator.clone(),
            tts,
            audio,
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let net_err = wait_for_snapshot(
        &mut rx,
        "network error banner clears stale Vietnamese",
        |s| {
            s.latest_seq == 2
                && s.error_banner
                    .as_ref()
                    .is_some_and(|b| b.kind == PipelineErrorKind::TranslateNetwork)
        },
    )
    .await;
    assert_eq!(net_err.chinese_line, "第二句。");
    assert_eq!(net_err.vietnamese_line, "");
    assert_eq!(
        net_err.error_banner.as_ref().map(|b| b.message.as_str()),
        Some(ERR_BANNER_TRANSLATE_NETWORK)
    );

    // Next subtitle hits 429 rate-limit -> explicit 429 banner, then server down, then recovery clears banner.
    translator.push_response(Err(TranslateError::RateLimited {
        detail: "429".to_string(),
    }));
    ocr.push_result(Ok(ocr_block("第三句。")));
    capturer.push_frame(Ok(solid_frame(240)));

    let rate_err = wait_for_snapshot(&mut rx, "429 banner", |s| {
        s.error_banner
            .as_ref()
            .is_some_and(|b| b.kind == PipelineErrorKind::TranslateRateLimited)
    })
    .await;
    assert_eq!(
        rate_err.error_banner.as_ref().map(|b| b.message.as_str()),
        Some(ERR_BANNER_TRANSLATE_RATE_LIMIT)
    );

    translator.push_response(Err(TranslateError::ServerUnavailable {
        detail: "down".to_string(),
    }));
    ocr.push_result(Ok(ocr_block("第四句。")));
    capturer.push_frame(Ok(frame_with_band(240, 20)));

    let srv_err = wait_for_snapshot(&mut rx, "server unavailable banner", |s| {
        s.error_banner
            .as_ref()
            .is_some_and(|b| b.kind == PipelineErrorKind::TranslateServerUnavailable)
    })
    .await;
    assert_eq!(
        srv_err.error_banner.as_ref().map(|b| b.message.as_str()),
        Some(ERR_BANNER_SERVER_UNAVAILABLE)
    );

    translator.push_response(Ok("Phục hồi thành công.".to_string()));
    ocr.push_result(Ok(ocr_block("第五句。")));
    capturer.push_frame(Ok(solid_frame(120)));

    let recovered = wait_for_snapshot(&mut rx, "recovered clears error banner", |s| {
        s.vietnamese_line == "Phục hồi thành công."
    })
    .await;
    assert_eq!(recovered.error_banner, None);

    handle.stop().await;
}

#[tokio::test]
async fn pipeline_tts_and_audio_errors_surface_non_blocking_warning_while_keeping_translation_and_logging(
) {
    let temp = TempDirGuard::new("tts_err");
    let capturer = Arc::new(MockCapturer::new(
        vec![Ok(solid_frame(15)), Ok(frame_with_band(15, 220))],
        Ok(frame_with_band(15, 220)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Ok(ocr_block("第一句。")), Ok(ocr_block("第二句。"))],
        Ok(ocr_block("第二句。")),
    ));
    let translator = Arc::new(ControllableTranslator::scripted(vec![
        Ok("Bản dịch một.".to_string()),
        Ok("Bản dịch hai.".to_string()),
    ]));
    let tts = Arc::new(MockTtsEngine::new(vec![
        Err(TtsError::Upstream {
            status: 503,
            detail: "Edge-TTS offline".to_string(),
        }),
        Ok(b"MP3_OK".to_vec()),
    ]));
    let audio = Arc::new(MockAudioPlayback::new());
    audio.push_enqueue_result(Err(AudioError::DecodeFailed {
        detail: "bad mp3 frame".to_string(),
    }));
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer,
            ocr,
            translator,
            tts,
            audio,
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let tts_warn = wait_for_snapshot(&mut rx, "TTS error banner keeps Vietnamese line", |s| {
        s.error_banner
            .as_ref()
            .is_some_and(|b| b.kind == PipelineErrorKind::Tts)
    })
    .await;
    assert_eq!(tts_warn.vietnamese_line, "Bản dịch một.");

    let audio_warn = wait_for_snapshot(
        &mut rx,
        "Audio error banner keeps translation and log",
        |s| {
            s.metrics.logged_pairs == 2
                && s.error_banner
                    .as_ref()
                    .is_some_and(|b| b.kind == PipelineErrorKind::Audio)
        },
    )
    .await;
    assert_eq!(audio_warn.vietnamese_line, "Bản dịch hai.");

    handle.stop().await;
}

#[tokio::test]
async fn pipeline_capture_and_ocr_errors_surface_explicit_vietnamese_banners_without_panicking() {
    let temp = TempDirGuard::new("cap_ocr_err");
    let capturer = Arc::new(MockCapturer::new(
        vec![Err(CaptureError::MonitorUnavailable {
            monitor_id: "HDMI-1".to_string(),
        })],
        Ok(solid_frame(30)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Err(OcrAdapterError::Engine(OcrError::RequestTimeout {
            timeout_ms: 3000,
        }))],
        Ok(ocr_block("恢复正常。")),
    ));
    let translator = Arc::new(ControllableTranslator::scripted(vec![Ok(
        "Đã hoạt động lại.".to_string(),
    )]));
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer,
            ocr,
            translator,
            tts,
            audio,
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let cap_err = wait_for_snapshot(&mut rx, "capture error banner", |s| {
        s.error_banner
            .as_ref()
            .is_some_and(|b| b.kind == PipelineErrorKind::Capture)
    })
    .await;
    assert!(cap_err
        .error_banner
        .as_ref()
        .is_some_and(|b| b.message.contains("HDMI-1")));

    let ocr_err = wait_for_snapshot(&mut rx, "ocr error banner", |s| {
        s.error_banner
            .as_ref()
            .is_some_and(|b| b.kind == PipelineErrorKind::Ocr)
    })
    .await;
    assert!(ocr_err
        .error_banner
        .as_ref()
        .is_some_and(|b| b.message.contains("WeChat OCR")));

    let ok_snap = wait_for_snapshot(&mut rx, "recovers after capture and ocr errors", |s| {
        s.vietnamese_line == "Đã hoạt động lại."
    })
    .await;
    assert_eq!(ok_snap.error_banner, None);

    handle.stop().await;
}

#[tokio::test]
async fn pipeline_live_settings_update_switches_voice_and_mute_stops_audio_immediately() {
    let temp = TempDirGuard::new("live_cfg");
    let capturer = Arc::new(MockCapturer::new(
        vec![Ok(solid_frame(15))],
        Ok(solid_frame(15)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Ok(ocr_block("第一句。"))],
        Ok(ocr_block("第一句。")),
    ));
    let translator = Arc::new(ControllableTranslator::scripted(Vec::new()));
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer: capturer.clone(),
            ocr: ocr.clone(),
            translator,
            tts: tts.clone(),
            audio: audio.clone(),
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let _ = wait_for_snapshot(&mut rx, "first clip spoken", |s| s.metrics.tts_calls == 1).await;
    assert_eq!(tts.calls()[0].voice, "vi-VN-NamMinhNeural");

    assert_eq!(handle.set_voice("vi-VN-HoaiMyNeural"), Ok(()));
    assert_eq!(handle.set_volume(0.35), Ok(()));
    assert!((audio.volume() - 0.35).abs() < f32::EPSILON);

    ocr.set_fallback(Ok(ocr_block("第二句。")));
    capturer.set_fallback(Ok(frame_with_band(15, 220)));

    let _ = wait_for_snapshot(&mut rx, "second clip uses new voice", |s| {
        s.metrics.tts_calls == 2
    })
    .await;
    assert_eq!(tts.calls()[1].voice, "vi-VN-HoaiMyNeural");

    let stops_before_mute = audio.stop_calls();
    assert_eq!(handle.set_muted(true), Ok(()));
    assert!(audio.stop_calls() > stops_before_mute);

    ocr.set_fallback(Ok(ocr_block("第三句。")));
    capturer.set_fallback(Ok(solid_frame(240)));

    let after_mute = wait_for_snapshot(&mut rx, "muted translates without TTS", |s| {
        s.latest_seq == 3 && s.vietnamese_line == "VI(第三句。)"
    })
    .await;
    assert_eq!(after_mute.metrics.tts_calls, 2);
    assert_eq!(tts.calls().len(), 2);

    assert_eq!(handle.set_muted(false), Ok(()));
    assert_eq!(handle.set_auto_read(false), Ok(()));
    assert_eq!(handle.snapshot().latest_seq, 3);

    ocr.set_fallback(Ok(ocr_block("第四句。")));
    capturer.set_fallback(Ok(frame_with_band(240, 20)));
    let after_auto_read_off =
        wait_for_snapshot(&mut rx, "auto_read=false translates without TTS", |s| {
            s.latest_seq == 4 && s.vietnamese_line == "VI(第四句。)"
        })
        .await;
    assert_eq!(after_auto_read_off.metrics.tts_calls, 2);
    assert_eq!(tts.calls().len(), 2);

    handle.stop().await;
}

#[tokio::test]
async fn pipeline_stop_clears_audio_aborts_inflight_translation_and_returns_idle() {
    let temp = TempDirGuard::new("stop");
    let capturer = Arc::new(MockCapturer::new(
        vec![Ok(solid_frame(10))],
        Ok(solid_frame(10)),
    ));
    let ocr = Arc::new(MockOcrEngine::new(
        vec![Ok(ocr_block("正在翻译中。"))],
        Ok(ocr_block("正在翻译中。")),
    ));
    let translator = Arc::new(ControllableTranslator::manual());
    let tts = Arc::new(MockTtsEngine::new(Vec::new()));
    let audio = Arc::new(MockAudioPlayback::new());
    let logger = Arc::new(SubtitleLogger::new(temp.path().to_path_buf()));

    let handle = match PipelineRunner::start(
        test_settings(60),
        PipelineDeps {
            capturer: capturer.clone(),
            ocr,
            translator: translator.clone(),
            tts,
            audio: audio.clone(),
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    translator.wait_for_pending(1).await;
    let rx = handle.subscribe();
    let calls_at_stop = capturer.call_count();

    if tokio::time::timeout(TEST_WAIT_TIMEOUT, handle.stop())
        .await
        .is_err()
    {
        panic!("timed out stopping PipelineHandle with inflight translation");
    }

    assert!(audio.stop_calls() >= 1);
    assert_eq!(rx.borrow().status, PipelineStatus::Idle);

    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        capturer.call_count() <= calls_at_stop + 1,
        "expected capture loop to stop promptly; calls before={calls_at_stop}, after={}",
        capturer.call_count()
    );
}

fn unused_loopback_port() -> u16 {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        panic!("failed binding ephemeral TCP port");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("failed reading ephemeral port");
    };
    addr.port()
}

fn resolve_smoke_region(
    capturer: &XcapCapturer,
    configured: Option<&CaptureRegion>,
) -> CaptureRegion {
    let monitors = match capturer.list_monitors() {
        Ok(list) => list,
        Err(err) => panic!("failed listing monitors: {err}"),
    };
    let primary = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .unwrap_or_else(|| panic!("no OS monitors available"));

    if let Ok(raw) = std::env::var("CAP2VOICE_SMOKE_REGION") {
        let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
        if parts.len() == 4 {
            if let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                parts[0].parse::<i32>(),
                parts[1].parse::<i32>(),
                parts[2].parse::<u32>(),
                parts[3].parse::<u32>(),
            ) {
                return CaptureRegion {
                    monitor_id: primary.id.clone(),
                    x,
                    y,
                    width: w,
                    height: h,
                };
            }
        }
        panic!("invalid CAP2VOICE_SMOKE_REGION='{raw}', expected x,y,width,height");
    }

    if let Some(region) = configured {
        return region.clone();
    }

    let w = ((primary.width as f32) * 0.60).round() as u32;
    let h = ((primary.height as f32) * 0.14).round().max(60.0) as u32;
    let x = primary.x + ((primary.width.saturating_sub(w)) / 2) as i32;
    let y = primary.y + (primary.height.saturating_sub(h + 80)) as i32;
    CaptureRegion {
        monitor_id: primary.id.clone(),
        x,
        y,
        width: w.max(20),
        height: h.max(16),
    }
}

#[tokio::test]
#[ignore = "60-second real-PC headless smoke test over a live hardcoded-Chinese-subtitle video"]
async fn real_pc_60s_headless_pipeline_smoke_test() {
    let smoke_secs = std::env::var("CAP2VOICE_SMOKE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);

    let mut cfg = match default_config_path() {
        Ok(path) => match load_config_from(&path) {
            Ok(outcome) => outcome.config,
            Err(err) => panic!("failed loading config {}: {err}", path.display()),
        },
        Err(err) => panic!("failed resolving config path: {err}"),
    };

    let capturer = Arc::new(XcapCapturer::new());
    let region = resolve_smoke_region(&capturer, cfg.general.region.as_ref());
    println!(
        "smoke region: monitor={} ({}, {}) {}x{} px; duration={}s",
        region.monitor_id, region.x, region.y, region.width, region.height, smoke_secs
    );
    cfg.general.region = Some(region);
    let settings = match PipelineSettings::from_app_config(&cfg) {
        Ok(s) => s,
        Err(err) => panic!("invalid PipelineSettings from config: {err}"),
    };

    let ocr_paths = match resolve_wechat_paths_from_config(&cfg.general) {
        Ok(paths) => paths,
        Err(err) => panic!("failed discovering WeChat OCR paths: {err}"),
    };
    let ocr = Arc::new(WeChatOcrAdapter::new(ocr_paths));
    if let Err(err) = ocr.warmup() {
        panic!("WeChatOcrAdapter warmup failed: {err}");
    }

    let server_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("server");
    let port = unused_loopback_port();
    let server = match ServerProcess::start(ServerProcessConfig::for_port(port, server_dir)).await {
        Ok(srv) => srv,
        Err(err) => panic!("ServerProcess::start failed: {err}"),
    };
    let translator = match HttpTranslator::with_base_url(
        server.base_url().to_string(),
        DEFAULT_CONNECT_TIMEOUT,
        DEFAULT_TRANSLATE_TIMEOUT,
    ) {
        Ok(t) => Arc::new(t),
        Err(err) => panic!("HttpTranslator init failed: {err}"),
    };
    let tts = match HttpTtsEngine::with_base_url(
        server.base_url().to_string(),
        DEFAULT_CONNECT_TIMEOUT,
        DEFAULT_TTS_TIMEOUT,
    ) {
        Ok(t) => Arc::new(t),
        Err(err) => panic!("HttpTtsEngine init failed: {err}"),
    };
    let audio = match AudioPlayer::new(settings.volume) {
        Ok(p) => Arc::new(p),
        Err(err) => panic!("AudioPlayer init failed: {err}"),
    };

    let log_dir = match cfg.effective_log_dir() {
        Ok(dir) => dir,
        Err(_) => match default_log_dir() {
            Ok(dir) => dir,
            Err(err) => panic!("failed resolving log dir: {err}"),
        },
    };
    let logger = Arc::new(SubtitleLogger::new(log_dir.clone()));

    let handle = match PipelineRunner::start(
        settings,
        PipelineDeps {
            capturer,
            ocr,
            translator,
            tts,
            audio,
            logger: Some(logger),
        },
    ) {
        Ok(h) => h,
        Err(err) => panic!("PipelineRunner::start failed: {err}"),
    };

    let mut rx = handle.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(smoke_secs);
    let mut last_logged_seq = 0u64;
    let mut last_logged_vi = String::new();

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rx.changed()).await {
            Ok(Ok(())) => {
                let snap = rx.borrow().clone();
                if snap.latest_seq != last_logged_seq
                    || (snap.vietnamese_line != last_logged_vi && !snap.vietnamese_line.is_empty())
                {
                    println!(
                        "[seq={}] zh='{}' | vi='{}'",
                        snap.latest_seq, snap.chinese_line, snap.vietnamese_line
                    );
                    last_logged_seq = snap.latest_seq;
                    last_logged_vi = snap.vietnamese_line.clone();
                }
                if let Some(banner) = &snap.error_banner {
                    println!("[banner {:?}] {}", banner.kind, banner.message);
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }

    let final_snap = handle.snapshot();
    handle.stop().await;
    assert_eq!(rx.borrow().status, PipelineStatus::Idle);
    drop(server);

    let today_log = daily_log_path(&log_dir, Local::now().date_naive());
    println!(
        "smoke finished: metrics={:?}, log_file={}",
        final_snap.metrics,
        today_log.display()
    );
}
