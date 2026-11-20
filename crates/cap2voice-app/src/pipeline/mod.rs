//! Async subtitle translation pipeline orchestrator and daily UTF-8 logger.

pub mod logger;

pub use logger::{daily_log_path, format_subtitle_log_line, SubtitleLogError, SubtitleLogger};

use crate::api::{TranslateError, Translator, TtsEngine};
use crate::audio::{validate_volume, AudioError, AudioPlayback};
use crate::capture::{validate_region_dimensions, FrameCapturer, PixelDiffGate};
use crate::config::{AppConfig, CaptureRegion};
use crate::ocr::{extract_chinese_subtitle, FuzzyDeduplicator, OcrEngine, TextDeduplicator};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};

/// Default backoff delay after an upstream Google Translate HTTP 429 rate-limit response.
pub const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(2);

/// Maximum concurrent in-flight translation/TTS subtitle tasks spawned by the pipeline.
pub const MAX_INFLIGHT_TRANSLATIONS: usize = 4;

/// Visible Vietnamese error banner for network/upstream translation failures (`docs/spec.md` §3.3).
pub const ERR_BANNER_TRANSLATE_NETWORK: &str = "Mất kết nối mạng — không thể dịch";

/// Visible Vietnamese error banner for Google Translate HTTP 429 rate limits (`docs/spec.md` §3.3).
pub const ERR_BANNER_TRANSLATE_RATE_LIMIT: &str =
    "Google Translate bị giới hạn tốc độ — tạm chờ...";

/// Visible Vietnamese error banner when the local Python server is down (`docs/spec.md` §3.3).
pub const ERR_BANNER_SERVER_UNAVAILABLE: &str = "Server dịch/TTS chưa sẵn sàng";

/// Typed startup and runtime control errors emitted by `PipelineRunner` and `PipelineHandle`.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PipelineError {
    #[error("chưa chọn vùng phụ đề — hãy bấm 'Chọn vùng' trước khi bắt đầu")]
    RegionNotConfigured,

    #[error("cấu hình pipeline không hợp lệ: {0}")]
    InvalidSettings(String),

    #[error("không tìm thấy Tokio runtime đang hoạt động để khởi chạy pipeline")]
    MissingTokioRuntime,

    #[error("pipeline đã dừng")]
    AlreadyStopped,

    #[error("lỗi âm thanh: {0}")]
    Audio(#[from] AudioError),
}

/// High-level pipeline lifecycle status surfaced to the overlay and system tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStatus {
    Idle,
    Running,
}

/// Classification of the active pipeline error banner for UI styling and clearing logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineErrorKind {
    Capture,
    Ocr,
    TranslateNetwork,
    TranslateRateLimited,
    TranslateServerUnavailable,
    TranslateOther,
    Tts,
    Audio,
    Logger,
}

/// Visible non-silent error banner displayed on the overlay and Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineErrorBanner {
    pub kind: PipelineErrorKind,
    pub message: String,
}

/// Diagnostic counters tracked by `PipelineRunner`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineMetrics {
    pub frames_captured: u64,
    pub frames_skipped_pixel_diff: u64,
    pub ocr_calls: u64,
    pub subtitles_emitted: u64,
    pub translations_succeeded: u64,
    pub stale_translations_discarded: u64,
    pub tts_calls: u64,
    pub logged_pairs: u64,
}

/// Latest immutable UI/observer snapshot published over `watch::Receiver<PipelineSnapshot>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineSnapshot {
    pub status: PipelineStatus,
    pub latest_seq: u64,
    pub chinese_line: String,
    pub vietnamese_line: String,
    pub error_banner: Option<PipelineErrorBanner>,
    pub metrics: PipelineMetrics,
}

impl Default for PipelineSnapshot {
    fn default() -> Self {
        Self {
            status: PipelineStatus::Idle,
            latest_seq: 0,
            chinese_line: String::new(),
            vietnamese_line: String::new(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
        }
    }
}

/// Live-updatable configuration snapshot governing pipeline capture, dedupe, TTS, and logging.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSettings {
    pub region: CaptureRegion,
    pub capture_fps: u32,
    pub pixel_diff_threshold: f32,
    pub dedupe_threshold: f32,
    pub blank_reset_frames: u32,
    pub voice: String,
    pub speech_rate: String,
    pub auto_read: bool,
    pub muted: bool,
    pub volume: f32,
    pub enable_subtitle_log: bool,
    pub rate_limit_backoff: Duration,
}

fn validate_unit_f32(name: &str, value: f32, min: f32, max: f32) -> Result<(), PipelineError> {
    if !value.is_finite() || value < min || value > max {
        return Err(PipelineError::InvalidSettings(format!(
            "{name} phải nằm trong {min}..={max} (nhận {value})"
        )));
    }
    Ok(())
}

impl PipelineSettings {
    /// Derive validated `PipelineSettings` from a persisted `AppConfig`.
    pub fn from_app_config(config: &AppConfig) -> Result<Self, PipelineError> {
        config
            .validate()
            .map_err(|err| PipelineError::InvalidSettings(err.to_string()))?;
        let region = config
            .general
            .region
            .clone()
            .ok_or(PipelineError::RegionNotConfigured)?;
        let settings = Self {
            region,
            capture_fps: config.general.capture_fps,
            pixel_diff_threshold: config.translation.pixel_diff_threshold,
            dedupe_threshold: config.translation.dedupe_threshold,
            blank_reset_frames: config.translation.blank_reset_frames,
            voice: config.voice.voice.clone(),
            speech_rate: config.voice.speech_rate.clone(),
            auto_read: config.voice.auto_read,
            muted: false,
            volume: config.voice.volume,
            enable_subtitle_log: config.general.enable_subtitle_log,
            rate_limit_backoff: DEFAULT_RATE_LIMIT_BACKOFF,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Validate capture FPS, region bounds, thresholds, voice, and audio volume.
    pub fn validate(&self) -> Result<(), PipelineError> {
        if !(1..=60).contains(&self.capture_fps) {
            return Err(PipelineError::InvalidSettings(format!(
                "capture_fps phải nằm trong 1..=60 (nhận {})",
                self.capture_fps
            )));
        }
        validate_region_dimensions(self.region.width, self.region.height)
            .map_err(|err| PipelineError::InvalidSettings(err.to_string()))?;
        validate_unit_f32(
            "pixel_diff_threshold",
            self.pixel_diff_threshold,
            0.001,
            0.50,
        )?;
        validate_unit_f32("dedupe_threshold", self.dedupe_threshold, 0.50, 1.00)?;
        if self.blank_reset_frames == 0 {
            return Err(PipelineError::InvalidSettings(
                "blank_reset_frames phải >= 1".to_string(),
            ));
        }
        if self.voice.trim().is_empty() {
            return Err(PipelineError::InvalidSettings(
                "voice không được để trống".to_string(),
            ));
        }
        if self.speech_rate.trim().is_empty() {
            return Err(PipelineError::InvalidSettings(
                "speech_rate không được để trống".to_string(),
            ));
        }
        let _ = validate_volume(self.volume)?;
        Ok(())
    }
}

/// Map a typed `TranslateError` into the explicit Vietnamese UI error banner.
pub fn map_translate_error_banner(err: &TranslateError) -> PipelineErrorBanner {
    match err {
        TranslateError::RateLimited { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateRateLimited,
            message: ERR_BANNER_TRANSLATE_RATE_LIMIT.to_string(),
        },
        TranslateError::Upstream { .. } | TranslateError::Timeout { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateNetwork,
            message: ERR_BANNER_TRANSLATE_NETWORK.to_string(),
        },
        TranslateError::ServerUnavailable { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateServerUnavailable,
            message: ERR_BANNER_SERVER_UNAVAILABLE.to_string(),
        },
        TranslateError::EmptyInput
        | TranslateError::InvalidInput { .. }
        | TranslateError::InvalidResponse { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateOther,
            message: format!("Lỗi dịch: {err}"),
        },
    }
}

/// Trait-object dependencies injected into `PipelineRunner::start`.
#[derive(Clone)]
pub struct PipelineDeps {
    pub capturer: Arc<dyn FrameCapturer>,
    pub ocr: Arc<dyn OcrEngine>,
    pub translator: Arc<dyn Translator>,
    pub tts: Arc<dyn TtsEngine>,
    pub audio: Arc<dyn AudioPlayback>,
    pub logger: Option<Arc<SubtitleLogger>>,
}

struct SharedPipelineState {
    settings: PipelineSettings,
    snapshot: PipelineSnapshot,
    snapshot_tx: watch::Sender<PipelineSnapshot>,
    rate_limit_until: Option<Instant>,
    stopped: bool,
}

impl SharedPipelineState {
    fn publish(&self) {
        let _ = self.snapshot_tx.send(self.snapshot.clone());
    }

    fn set_banner(&mut self, kind: PipelineErrorKind, message: String) {
        self.snapshot.error_banner = Some(PipelineErrorBanner { kind, message });
        self.publish();
    }

    fn clear_banner_if_kind(&mut self, kind: PipelineErrorKind) {
        if self
            .snapshot
            .error_banner
            .as_ref()
            .is_some_and(|banner| banner.kind == kind)
        {
            self.snapshot.error_banner = None;
            self.publish();
        }
    }
}

fn fps_to_interval(fps: u32) -> Duration {
    let nanos = 1_000_000_000u64 / u64::from(fps.max(1));
    Duration::from_nanos(nanos.max(1))
}

/// Entrypoint for launching the background async capture/OCR/translate/TTS pipeline.
pub struct PipelineRunner;

impl PipelineRunner {
    /// Start the pipeline on the currently active Tokio runtime.
    pub fn start(
        settings: PipelineSettings,
        deps: PipelineDeps,
    ) -> Result<PipelineHandle, PipelineError> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| PipelineError::MissingTokioRuntime)?;
        Self::start_on(&rt, settings, deps)
    }

    /// Start the pipeline on an explicit Tokio runtime handle.
    pub fn start_on(
        rt: &tokio::runtime::Handle,
        settings: PipelineSettings,
        deps: PipelineDeps,
    ) -> Result<PipelineHandle, PipelineError> {
        settings.validate()?;
        deps.audio.set_volume(settings.volume)?;

        let initial_snapshot = PipelineSnapshot {
            status: PipelineStatus::Running,
            ..PipelineSnapshot::default()
        };
        let (snapshot_tx, snapshot_rx) = watch::channel(initial_snapshot.clone());
        let (stop_tx, stop_rx) = watch::channel(false);
        let (settings_tx, settings_rx) = watch::channel(());

        let shared = Arc::new(Mutex::new(SharedPipelineState {
            settings,
            snapshot: initial_snapshot,
            snapshot_tx,
            rate_limit_until: None,
            stopped: false,
        }));

        let worker_shared = Arc::clone(&shared);
        let worker_deps = deps.clone();
        let task_handle = rt.spawn(async move {
            run_orchestrator_loop(worker_shared, worker_deps, stop_rx, settings_rx).await;
        });

        Ok(PipelineHandle {
            shared,
            deps,
            snapshot_rx,
            stop_tx,
            settings_tx,
            task_handle: Some(task_handle),
        })
    }
}

/// Control handle for observing snapshots, applying live setting edits, and stopping cleanly.
pub struct PipelineHandle {
    shared: Arc<Mutex<SharedPipelineState>>,
    deps: PipelineDeps,
    snapshot_rx: watch::Receiver<PipelineSnapshot>,
    stop_tx: watch::Sender<bool>,
    settings_tx: watch::Sender<()>,
    task_handle: Option<JoinHandle<()>>,
}

impl PipelineHandle {
    pub fn subscribe(&self) -> watch::Receiver<PipelineSnapshot> {
        self.snapshot_rx.clone()
    }

    pub fn snapshot(&self) -> PipelineSnapshot {
        self.snapshot_rx.borrow().clone()
    }

    fn modify_settings(
        &self,
        mutate: impl FnOnce(&mut PipelineSettings),
    ) -> Result<(), PipelineError> {
        let mut candidate = {
            let state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if state.stopped {
                return Err(PipelineError::AlreadyStopped);
            }
            state.settings.clone()
        };
        mutate(&mut candidate);
        self.update_settings(candidate)
    }

    pub fn update_settings(&self, settings: PipelineSettings) -> Result<(), PipelineError> {
        settings.validate()?;

        let (prev_volume, now_muted) = {
            let mut state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if state.stopped {
                return Err(PipelineError::AlreadyStopped);
            }
            let prev_volume = state.settings.volume;
            let now_muted = settings.muted && !state.settings.muted;
            state.settings = settings.clone();
            (prev_volume, now_muted)
        };

        if (settings.volume - prev_volume).abs() > f32::EPSILON {
            self.deps.audio.set_volume(settings.volume)?;
        }
        if now_muted {
            self.deps.audio.stop_and_clear()?;
        }

        let _ = self.settings_tx.send(());
        Ok(())
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), PipelineError> {
        self.modify_settings(|s| s.muted = muted)
    }

    pub fn set_voice(&self, voice: impl Into<String>) -> Result<(), PipelineError> {
        let voice = voice.into();
        self.modify_settings(|s| s.voice = voice)
    }

    pub fn set_volume(&self, volume: f32) -> Result<(), PipelineError> {
        self.modify_settings(|s| s.volume = volume)
    }

    pub fn set_auto_read(&self, auto_read: bool) -> Result<(), PipelineError> {
        self.modify_settings(|s| s.auto_read = auto_read)
    }

    /// Stop capture/OCR, abort any in-flight translations, clear TTS audio, and wait for idle.
    pub async fn stop(mut self) {
        {
            let mut state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
        }
        let _ = self.stop_tx.send(true);
        let _ = self.deps.audio.stop_and_clear();
        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        let mut state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        state.snapshot.status = PipelineStatus::Idle;
        state.publish();
        let _ = self.stop_tx.send(true);
        let _ = self.deps.audio.stop_and_clear();
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }
}

async fn run_orchestrator_loop(
    shared: Arc<Mutex<SharedPipelineState>>,
    deps: PipelineDeps,
    mut stop_rx: watch::Receiver<bool>,
    mut settings_rx: watch::Receiver<()>,
) {
    let initial = shared
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .settings
        .clone();
    let mut active_fps = initial.capture_fps;
    let mut active_pixel_threshold = initial.pixel_diff_threshold;
    let mut active_region = initial.region;

    let mut pixel_diff = PixelDiffGate::with_threshold(active_pixel_threshold);
    let mut deduper = FuzzyDeduplicator::new(initial.dedupe_threshold, initial.blank_reset_frames);

    let mut ticker = tokio::time::interval(fps_to_interval(active_fps));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let semaphore = Arc::new(Semaphore::new(MAX_INFLIGHT_TRANSLATIONS));
    let mut inflight: JoinSet<()> = JoinSet::new();

    loop {
        while inflight.try_join_next().is_some() {}

        if *stop_rx.borrow() {
            break;
        }

        tokio::select! {
            biased;
            _ = stop_rx.changed() => {
                break;
            }
            _ = settings_rx.changed() => {
                let latest = shared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .settings
                    .clone();
                if latest.capture_fps != active_fps {
                    active_fps = latest.capture_fps;
                    ticker = tokio::time::interval(fps_to_interval(active_fps));
                    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                }
                if (latest.pixel_diff_threshold - active_pixel_threshold).abs() > f32::EPSILON
                    || latest.region != active_region
                {
                    active_pixel_threshold = latest.pixel_diff_threshold;
                    active_region = latest.region;
                    pixel_diff = PixelDiffGate::with_threshold(active_pixel_threshold);
                }
                deduper.set_threshold(latest.dedupe_threshold);
                deduper.set_blank_reset_frames(latest.blank_reset_frames);
            }
            _ = ticker.tick() => {
                if let Some(audio_err) = deps.audio.take_last_error() {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.set_banner(
                        PipelineErrorKind::Audio,
                        format!("Lỗi phát âm thanh: {audio_err}"),
                    );
                }

                let region = shared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .settings
                    .region
                    .clone();
                let capturer = Arc::clone(&deps.capturer);
                let cap_join = tokio::select! {
                    biased;
                    _ = stop_rx.changed() => break,
                    res = tokio::task::spawn_blocking(move || capturer.capture_region(&region)) => res,
                };

                let frame = match cap_join {
                    Ok(Ok(frame)) => {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.snapshot.metrics.frames_captured += 1;
                        state.clear_banner_if_kind(PipelineErrorKind::Capture);
                        frame
                    }
                    Ok(Err(cap_err)) => {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.set_banner(PipelineErrorKind::Capture, cap_err.to_string());
                        continue;
                    }
                    Err(join_err) => {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.set_banner(
                            PipelineErrorKind::Capture,
                            format!("Lỗi luồng chụp màn hình: {join_err}"),
                        );
                        continue;
                    }
                };

                if !pixel_diff.should_run_ocr(&frame) {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.snapshot.metrics.frames_skipped_pixel_diff += 1;
                    state.publish();
                    continue;
                }

                let ocr_engine = Arc::clone(&deps.ocr);
                let ocr_join = tokio::select! {
                    biased;
                    _ = stop_rx.changed() => break,
                    res = tokio::task::spawn_blocking(move || ocr_engine.recognize_frame(&frame)) => res,
                };

                let ocr_result = match ocr_join {
                    Ok(Ok(res)) => {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.snapshot.metrics.ocr_calls += 1;
                        state.clear_banner_if_kind(PipelineErrorKind::Ocr);
                        res
                    }
                    Ok(Err(ocr_err)) => {
                        pixel_diff.reset();
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.snapshot.metrics.ocr_calls += 1;
                        state.set_banner(PipelineErrorKind::Ocr, ocr_err.to_string());
                        continue;
                    }
                    Err(join_err) => {
                        pixel_diff.reset();
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.set_banner(
                            PipelineErrorKind::Ocr,
                            format!("Lỗi luồng OCR: {join_err}"),
                        );
                        continue;
                    }
                };

                let filtered = extract_chinese_subtitle(&ocr_result.ocr_response);
                let Some(chinese) = deduper.process(filtered.as_deref()) else {
                    let state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.publish();
                    continue;
                };

                let seq = {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stopped {
                        break;
                    }
                    state.snapshot.latest_seq = state.snapshot.latest_seq.saturating_add(1);
                    let seq = state.snapshot.latest_seq;
                    state.snapshot.chinese_line = chinese.clone();
                    state.snapshot.vietnamese_line.clear();
                    state.snapshot.metrics.subtitles_emitted += 1;
                    state.publish();
                    seq
                };

                let task_shared = Arc::clone(&shared);
                let task_deps = deps.clone();
                let task_sem = Arc::clone(&semaphore);
                inflight.spawn(async move {
                    process_emitted_subtitle(seq, chinese, task_shared, task_deps, task_sem).await;
                });
            }
        }
    }

    inflight.abort_all();
    while inflight.join_next().await.is_some() {}
    let _ = deps.audio.stop_and_clear();
    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
    state.stopped = true;
    state.snapshot.status = PipelineStatus::Idle;
    state.publish();
}

async fn process_emitted_subtitle(
    seq: u64,
    chinese: String,
    shared: Arc<Mutex<SharedPipelineState>>,
    deps: PipelineDeps,
    semaphore: Arc<Semaphore>,
) {
    let Ok(_permit) = semaphore.acquire_owned().await else {
        return;
    };

    let backoff_until = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return;
        }
        if seq < state.snapshot.latest_seq {
            state.snapshot.metrics.stale_translations_discarded += 1;
            state.publish();
            return;
        }
        state.rate_limit_until
    };

    if let Some(until) = backoff_until {
        if until > Instant::now() {
            tokio::time::sleep_until(until).await;
        }
    }

    {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return;
        }
        if seq < state.snapshot.latest_seq {
            state.snapshot.metrics.stale_translations_discarded += 1;
            state.publish();
            return;
        }
    }

    let translate_res = deps.translator.translate(&chinese).await;
    let (vietnamese, enable_log, should_tts, voice, rate) = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return;
        }
        if seq < state.snapshot.latest_seq {
            state.snapshot.metrics.stale_translations_discarded += 1;
            state.publish();
            return;
        }

        match translate_res {
            Ok(vi) => {
                state.rate_limit_until = None;
                state.snapshot.vietnamese_line = vi.clone();
                state.snapshot.metrics.translations_succeeded += 1;
                state.snapshot.error_banner = None;
                let enable_log = state.settings.enable_subtitle_log;
                let should_tts = state.settings.auto_read && !state.settings.muted;
                let voice = state.settings.voice.clone();
                let rate = state.settings.speech_rate.clone();
                state.publish();
                (vi, enable_log, should_tts, voice, rate)
            }
            Err(err) => {
                state.snapshot.vietnamese_line.clear();
                if matches!(err, TranslateError::RateLimited { .. }) {
                    let backoff = state.settings.rate_limit_backoff;
                    if !backoff.is_zero() {
                        state.rate_limit_until = Some(Instant::now() + backoff);
                    }
                }
                let banner = map_translate_error_banner(&err);
                state.set_banner(banner.kind, banner.message);
                return;
            }
        }
    };

    if enable_log {
        if let Some(logger) = &deps.logger {
            match logger.log_pair(&chinese, &vietnamese) {
                Ok(_) => {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.snapshot.metrics.logged_pairs += 1;
                    state.publish();
                }
                Err(err) => {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.set_banner(PipelineErrorKind::Logger, err.to_string());
                }
            }
        }
    }

    if !should_tts {
        return;
    }

    let tts_res = deps.tts.synthesize(&vietnamese, &voice, &rate).await;
    let mp3_bytes = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        state.snapshot.metrics.tts_calls += 1;
        if state.stopped || !state.settings.auto_read || state.settings.muted {
            state.publish();
            return;
        }
        match tts_res {
            Ok(bytes) => bytes,
            Err(err) => {
                state.set_banner(PipelineErrorKind::Tts, format!("Lỗi giọng đọc TTS: {err}"));
                return;
            }
        }
    };

    if let Err(err) = deps.audio.enqueue_mp3(mp3_bytes) {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        state.set_banner(
            PipelineErrorKind::Audio,
            format!("Lỗi phát âm thanh: {err}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        map_translate_error_banner, PipelineError, PipelineErrorBanner, PipelineErrorKind,
        PipelineSettings, ERR_BANNER_SERVER_UNAVAILABLE, ERR_BANNER_TRANSLATE_NETWORK,
        ERR_BANNER_TRANSLATE_RATE_LIMIT,
    };
    use crate::api::TranslateError;
    use crate::config::{AppConfig, CaptureRegion};

    #[test]
    fn pipeline_settings_from_app_config_validates_and_requires_region() {
        let cfg = AppConfig::default();
        assert_eq!(
            PipelineSettings::from_app_config(&cfg),
            Err(PipelineError::RegionNotConfigured)
        );

        let mut valid_cfg = AppConfig::default();
        valid_cfg.general.region = Some(CaptureRegion {
            monitor_id: "DISPLAY1".to_string(),
            x: 100,
            y: 900,
            width: 800,
            height: 80,
        });
        let settings = match PipelineSettings::from_app_config(&valid_cfg) {
            Ok(s) => s,
            Err(err) => panic!("expected valid AppConfig conversion, got {err}"),
        };
        assert_eq!(settings.capture_fps, 10);
        assert_eq!(settings.voice, "vi-VN-NamMinhNeural");
        assert!(!settings.muted);
        assert!(settings.auto_read);
        assert_eq!(settings.validate(), Ok(()));

        let mut bad_fps = settings;
        bad_fps.capture_fps = 0;
        assert!(matches!(
            bad_fps.validate(),
            Err(PipelineError::InvalidSettings(_))
        ));
    }

    #[test]
    fn translate_error_maps_to_expected_vietnamese_banner_kinds() {
        assert_eq!(
            map_translate_error_banner(&TranslateError::RateLimited {
                detail: "429".to_string(),
            }),
            PipelineErrorBanner {
                kind: PipelineErrorKind::TranslateRateLimited,
                message: ERR_BANNER_TRANSLATE_RATE_LIMIT.to_string(),
            }
        );
        assert_eq!(
            map_translate_error_banner(&TranslateError::Upstream {
                status: 502,
                detail: "bad gateway".to_string(),
            }),
            PipelineErrorBanner {
                kind: PipelineErrorKind::TranslateNetwork,
                message: ERR_BANNER_TRANSLATE_NETWORK.to_string(),
            }
        );
        assert_eq!(
            map_translate_error_banner(&TranslateError::Timeout { timeout_ms: 8000 }),
            PipelineErrorBanner {
                kind: PipelineErrorKind::TranslateNetwork,
                message: ERR_BANNER_TRANSLATE_NETWORK.to_string(),
            }
        );
        assert_eq!(
            map_translate_error_banner(&TranslateError::ServerUnavailable {
                detail: "connection refused".to_string(),
            }),
            PipelineErrorBanner {
                kind: PipelineErrorKind::TranslateServerUnavailable,
                message: ERR_BANNER_SERVER_UNAVAILABLE.to_string(),
            }
        );
    }
}
