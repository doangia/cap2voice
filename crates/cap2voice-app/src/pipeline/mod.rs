//! Async subtitle translation pipeline orchestrator and daily UTF-8 logger.

pub mod cache;
pub mod logger;

pub use cache::{normalize_cache_key, CacheError, TranslationCache, TtsCache};
pub use logger::{daily_log_path, format_subtitle_log_line, SubtitleLogError, SubtitleLogger};

use crate::api::{TranslateError, Translator, TtsEngine, TtsSynthesis};
use crate::audio::{validate_volume, AudioError, AudioPlayback};
use crate::capture::{validate_region_dimensions, FrameCapturer, PixelDiffGate};
use crate::config::{AppConfig, CaptureRegion};
use crate::ocr::{extract_chinese_subtitle_lines, FuzzyDeduplicator, OcrEngine, TextDeduplicator};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};

/// Default hold window for the subtitle hold-and-replace debounce (Task 15).
/// A newly OCRed line is held for this long before emission; if a different
/// line arrives during the window it replaces the held line, so only the
/// final (stable) line is translated, logged and read aloud. This suppresses
/// the garbage lines OCR produces while a subtitle is mid-transition (old
/// line fading out, new line fading in). The dedupe threshold is NOT changed.
pub const DEFAULT_HOLD_DELAY: Duration = Duration::from_millis(400);

/// Maximum hold window allowed by `PipelineSettings::validate`, so the hold
/// can never push subtitle latency past the 2,0 s spec gate on its own.
pub const MAX_HOLD_DELAY: Duration = Duration::from_secs(1);

/// Maximum concurrent in-flight translation/TTS subtitle tasks spawned by the pipeline.
pub const MAX_INFLIGHT_TRANSLATIONS: usize = 4;

/// Visible Vietnamese error banner for network/upstream translation failures (`docs/spec.md` §3.3).
pub const ERR_BANNER_TRANSLATE_NETWORK: &str = "Mất kết nối mạng — không thể dịch";

/// Visible Vietnamese error banner when no translation engine can
/// translate (`docs/spec.md` §3.3): both the Helsinki model and the
/// Vietphrase dictionaries are unavailable.
pub const ERR_BANNER_ENGINE_UNAVAILABLE: &str =
    "Không có engine dịch khả dụng — kiểm tra model/từ điển";

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
    TranslateEngineUnavailable,
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
    /// Held lines discarded because a newer line replaced them inside the
    /// hold window (Task 15) — i.e. suppressed transitional garbage lines.
    pub held_lines_replaced: u64,
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
    /// Hold-and-replace window (Task 15). `Duration::ZERO` disables the hold
    /// and emits lines immediately.
    pub hold_delay: Duration,
    pub voice: String,
    pub speech_rate: String,
    pub auto_read: bool,
    pub muted: bool,
    pub volume: f32,
    pub enable_subtitle_log: bool,
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
            hold_delay: DEFAULT_HOLD_DELAY,
            voice: config.voice.voice.clone(),
            speech_rate: config.voice.speech_rate.clone(),
            auto_read: config.voice.auto_read,
            muted: false,
            volume: config.voice.volume,
            enable_subtitle_log: config.general.enable_subtitle_log,
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
        if self.hold_delay > MAX_HOLD_DELAY {
            return Err(PipelineError::InvalidSettings(format!(
                "hold_delay phải <= {:?} (nhận {:?})",
                MAX_HOLD_DELAY, self.hold_delay
            )));
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
        TranslateError::Upstream { .. } | TranslateError::Timeout { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateNetwork,
            message: ERR_BANNER_TRANSLATE_NETWORK.to_string(),
        },
        TranslateError::EngineUnavailable { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateEngineUnavailable,
            message: ERR_BANNER_ENGINE_UNAVAILABLE.to_string(),
        },
        TranslateError::EmptyInput
        | TranslateError::InvalidInput { .. }
        | TranslateError::InvalidResponse { .. } => PipelineErrorBanner {
            kind: PipelineErrorKind::TranslateOther,
            message: format!("Lỗi dịch: {err}"),
        },
    }
}

/// Whether an emitted subtitle is a fast preliminary translation or a
/// completed sentence (A2 / gh #1). Preliminary translations display only;
/// completed sentences display (replacing any preliminary), are logged, and
/// are spoken via TTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleKind {
    Preliminary,
    Completed,
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
    /// Translation cache (A3 / gh #2). Consulted only for completed
    /// sentences; the preliminary translation never consults it. `None`
    /// disables caching (the translator is always called).
    pub translation_cache: Option<Arc<TranslationCache>>,
    /// TTS audio cache (A3 / gh #2). Consulted only for completed
    /// sentences. `None` disables caching.
    pub tts_cache: Option<Arc<TtsCache>>,
}

struct SharedPipelineState {
    settings: PipelineSettings,
    snapshot: PipelineSnapshot,
    snapshot_tx: watch::Sender<PipelineSnapshot>,
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

/// A subtitle line that passed the fuzzy dedupe and is being held for the
/// hold-and-replace window (Task 15).
#[derive(Debug)]
struct PendingSubtitle {
    chinese: String,
    deadline: Instant,
}

/// Hold-and-replace debounce for newly OCRed subtitle lines (Task 15).
///
/// A line that passes the fuzzy dedupe is held for `hold_delay` before it is
/// emitted. If a different line arrives during the hold window it replaces
/// the held line, so only the final (stable) line is translated, logged and
/// read aloud. This suppresses the garbage lines OCR produces while a
/// subtitle is mid-transition (old line fading out, new line fading in).
/// The dedupe threshold is NOT changed.
#[derive(Debug)]
pub struct SubtitleHold {
    hold_delay: Duration,
    pending: Option<PendingSubtitle>,
    replaced: u64,
}

impl SubtitleHold {
    pub fn new(hold_delay: Duration) -> Self {
        Self {
            hold_delay,
            pending: None,
            replaced: 0,
        }
    }

    pub fn hold_delay(&self) -> Duration {
        self.hold_delay
    }

    pub fn set_hold_delay(&mut self, hold_delay: Duration) {
        self.hold_delay = hold_delay;
    }

    /// Number of times a held line was replaced by a newer line
    /// (i.e. suppressed transitional garbage lines).
    pub fn replaced(&self) -> u64 {
        self.replaced
    }

    /// Push a newly deduped line. Returns `true` when it replaced a pending
    /// line. A zero hold delay stores the line with a deadline of `now`, so
    /// `take_ready(now)` returns it right away.
    pub fn push(&mut self, chinese: String, now: Instant) -> bool {
        let replaced = self.pending.is_some();
        if replaced {
            self.replaced += 1;
        }
        self.pending = Some(PendingSubtitle {
            chinese,
            deadline: now + self.hold_delay,
        });
        replaced
    }

    /// Take the held line once its deadline has passed. Returns `None` when
    /// nothing is pending or the deadline has not been reached yet.
    pub fn take_ready(&mut self, now: Instant) -> Option<String> {
        let ready = self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline);
        if ready {
            self.pending.take().map(|pending| pending.chinese)
        } else {
            None
        }
    }
}

/// Sentence-final punctuation that completes a subtitle sentence (A2 / gh #1).
/// Both fullwidth CJK forms and their halfwidth ASCII equivalents.
const SENTENCE_FINAL_PUNCT: [char; 7] = ['。', '？', '！', '…', '.', '!', '?'];

/// Default timeout for completing a buffered sentence that received no new
/// fragment (A2 / gh #1): 1.2 s, inside the 1.0–1.5 s range from the issue.
pub const DEFAULT_SENTENCE_TIMEOUT: Duration = Duration::from_millis(1200);

/// Default blank-frame count that completes a buffered sentence when the
/// subtitle disappears (A2 / gh #1): 5 frames ≈ 0.5 s at 10 fps, matching
/// the dedupe `blank_reset_frames` default.
pub const DEFAULT_SENTENCE_BLANK_FRAMES: u32 = 5;

/// Maximum time a sentence may stay buffered (A2 / gh #1): a safety valve
/// so an unpunctuated fragment displayed for a very long time is not held
/// forever when the subtitle never disappears cleanly (the `touch` calls
/// keep resetting the short timeout while the subtitle is on screen).
/// 10 s is far above any real split-sentence gap (a display lasts ~2–6 s)
/// and only fires for pathological long displays.
pub const MAX_SENTENCE_BUFFER_AGE: Duration = Duration::from_secs(10);

/// What the pipeline should do with a `SentenceBuffer` event (A2 / gh #1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentenceEvent {
    /// Translate the buffer-so-far for display only. No logging, no TTS.
    /// Keeps Vietnamese visible within ~2 s of the subtitle appearing.
    Preliminary(String),
    /// Translate the whole completed sentence; display it (replacing any
    /// preliminary), log the pair, and speak it via TTS.
    Completed(String),
}

/// Accumulates OCR fragments into sentences (A2 / gh #1).
///
/// The translation unit is a *sentence*, not an OCR frame: a long sentence
/// split across two subtitle displays is accumulated and translated as one
/// unit, while two visual lines in one frame are separate utterances (the
/// caller flushes before pushing a new line from the same frame).
///
/// Composes with the Task 15 hold-and-replace debounce: the hold sits
/// *upstream* and only emits stable lines; the buffer sits *downstream* and
/// accumulates those stable lines into sentences. The hold suppresses
/// transitional garbage frames; the buffer assembles real fragments.
#[derive(Debug)]
pub struct SentenceBuffer {
    buffer: String,
    last_push: Option<Instant>,
    /// When the current buffered sentence started (first fragment). Used
    /// for the `MAX_SENTENCE_BUFFER_AGE` safety valve.
    buffer_started: Option<Instant>,
    blank_frames: u32,
    blank_frames_to_flush: u32,
    timeout: Duration,
}

impl SentenceBuffer {
    pub fn new(timeout: Duration, blank_frames_to_flush: u32) -> Self {
        Self {
            buffer: String::new(),
            last_push: None,
            buffer_started: None,
            blank_frames: 0,
            blank_frames_to_flush: blank_frames_to_flush.max(1),
            timeout,
        }
    }

    /// Current buffered (incomplete) text, if any.
    pub fn buffered_text(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Push a fragment that continues the current sentence (the next display
    /// of the same sentence, or the first line of a frame). Returns the
    /// events to act on, in order: zero or more completed sentences, then
    /// at most one preliminary for the incomplete remainder.
    pub fn push(&mut self, fragment: String, now: Instant) -> Vec<SentenceEvent> {
        self.blank_frames = 0;
        if self.buffer.is_empty() {
            self.buffer_started = Some(now);
        }
        self.last_push = Some(now);
        self.buffer.push_str(&fragment);
        self.drain_completed()
    }

    /// Push a new visual line from the *same* frame as the previous line.
    /// The previous line's sentence is a separate utterance, so it is
    /// completed first; then the new line starts a fresh sentence. Returns
    /// the events to act on, in order.
    pub fn push_new_line(&mut self, fragment: String, now: Instant) -> Vec<SentenceEvent> {
        let mut events = Vec::new();
        if let Some(done) = self.flush() {
            events.push(SentenceEvent::Completed(done));
        }
        events.extend(self.push(fragment, now));
        events
    }

    /// Note a blank / non-CJK frame (the subtitle may have disappeared).
    /// Returns the completed sentence once the blank run reaches the
    /// threshold, or `None`.
    pub fn note_blank(&mut self, _now: Instant) -> Option<String> {
        self.blank_frames = self.blank_frames.saturating_add(1);
        if self.blank_frames >= self.blank_frames_to_flush {
            self.blank_frames = 0;
            self.flush()
        } else {
            None
        }
    }

    /// Flush the buffered sentence as completed when it has content and no
    /// new fragment arrived for `timeout`, or when the buffer has held a
    /// sentence for longer than `MAX_SENTENCE_BUFFER_AGE` (safety valve
    /// for unpunctuated long displays). Returns the sentence, or `None`.
    pub fn flush_if_timed_out(&mut self, now: Instant) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        // Safety valve: never hold a sentence longer than the max age,
        // even if `touch` keeps resetting the short timeout.
        if let Some(started) = self.buffer_started {
            if now.saturating_duration_since(started) >= MAX_SENTENCE_BUFFER_AGE {
                return self.flush();
            }
        }
        let last = self.last_push?;
        if now.saturating_duration_since(last) >= self.timeout {
            self.flush()
        } else {
            None
        }
    }

    /// Note that the current subtitle is still on screen (the frame is
    /// unchanged, or the OCR text duplicates the buffered fragment).
    /// Resets the completion timeout so a long-displayed fragment is not
    /// completed while it is still visible (A2 / gh #1). The dedupe and
    /// pixel-diff gates hide repeated reads of the same subtitle from
    /// `push`, so without this the timeout would fire mid-display and
    /// split a sentence across two log lines.
    pub fn touch(&mut self, now: Instant) {
        if !self.buffer.is_empty() {
            self.last_push = Some(now);
        }
    }

    /// Take the buffered text as a completed sentence (if non-empty) and
    /// clear the buffer.
    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        self.last_push = None;
        self.buffer_started = None;
        self.blank_frames = 0;
        Some(std::mem::take(&mut self.buffer))
    }

    /// Split off every complete sentence at the end of the buffer (a
    /// fragment may contain more than one sentence), returning a
    /// `Completed` event per sentence plus a `Preliminary` for the
    /// incomplete remainder, if any.
    fn drain_completed(&mut self) -> Vec<SentenceEvent> {
        let mut events = Vec::new();
        while let Some(end) = self.buffer.char_indices().find_map(|(i, ch)| {
            SENTENCE_FINAL_PUNCT
                .contains(&ch)
                .then_some(i + ch.len_utf8())
        }) {
            let sentence: String = self.buffer.drain(..end).collect();
            events.push(SentenceEvent::Completed(sentence));
        }
        if !self.buffer.is_empty() {
            events.push(SentenceEvent::Preliminary(self.buffer.clone()));
        }
        events
    }
}
/// Assign a sequence number, publish the Chinese line, and spawn the
/// translate/log/TTS task for an emitted subtitle. `kind` selects the
/// preliminary (display only) or completed (display + log + TTS) path
/// (A2 / gh #1). Returns `false` when the pipeline is already stopped
/// (caller must break the loop).
fn emit_subtitle(
    chinese: String,
    kind: SubtitleKind,
    shared: &Arc<Mutex<SharedPipelineState>>,
    deps: &PipelineDeps,
    semaphore: &Arc<Semaphore>,
    inflight: &mut JoinSet<()>,
) -> bool {
    let seq = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return false;
        }
        state.snapshot.latest_seq = state.snapshot.latest_seq.saturating_add(1);
        let seq = state.snapshot.latest_seq;
        state.snapshot.chinese_line = chinese.clone();
        state.snapshot.vietnamese_line.clear();
        state.snapshot.metrics.subtitles_emitted += 1;
        state.publish();
        seq
    };

    let task_shared = Arc::clone(shared);
    let task_deps = deps.clone();
    let task_sem = Arc::clone(semaphore);
    inflight.spawn(async move {
        process_emitted_subtitle(seq, chinese, kind, task_shared, task_deps, task_sem).await;
    });
    true
}

/// Emit every event from a `SentenceBuffer` push, in order. Returns `false`
/// when the pipeline is already stopped.
fn emit_sentence_events(
    events: Vec<SentenceEvent>,
    shared: &Arc<Mutex<SharedPipelineState>>,
    deps: &PipelineDeps,
    semaphore: &Arc<Semaphore>,
    inflight: &mut JoinSet<()>,
) -> bool {
    for event in events {
        let (text, kind) = match event {
            SentenceEvent::Preliminary(text) => (text, SubtitleKind::Preliminary),
            SentenceEvent::Completed(text) => (text, SubtitleKind::Completed),
        };
        if !emit_subtitle(text, kind, shared, deps, semaphore, inflight) {
            return false;
        }
    }
    true
}

/// Split a held frame text (lines joined with "\n") into visual lines,
/// feed each to the `SentenceBuffer`, and emit the resulting events. A new
/// line in the same frame is a separate utterance, so the buffer completes
/// the previous sentence before starting it. Returns `false` when the
/// pipeline is already stopped.
fn emit_held_lines(
    joined: String,
    sentence_buffer: &mut SentenceBuffer,
    shared: &Arc<Mutex<SharedPipelineState>>,
    deps: &PipelineDeps,
    semaphore: &Arc<Semaphore>,
    inflight: &mut JoinSet<()>,
) -> bool {
    let mut first_line = true;
    for line in joined.split('\n') {
        if line.is_empty() {
            continue;
        }
        let events = if first_line {
            sentence_buffer.push(line.to_string(), Instant::now())
        } else {
            sentence_buffer.push_new_line(line.to_string(), Instant::now())
        };
        first_line = false;
        if !emit_sentence_events(events, shared, deps, semaphore, inflight) {
            return false;
        }
    }
    true
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
    let mut hold = SubtitleHold::new(initial.hold_delay);
    // SentenceBuffer (A2 / gh #1) sits downstream of the hold: the hold
    // emits stable lines, the buffer accumulates them into sentences.
    let mut sentence_buffer =
        SentenceBuffer::new(DEFAULT_SENTENCE_TIMEOUT, DEFAULT_SENTENCE_BLANK_FRAMES);

    let mut ticker = tokio::time::interval(fps_to_interval(active_fps));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let semaphore = Arc::new(Semaphore::new(MAX_INFLIGHT_TRANSLATIONS));
    let mut inflight: JoinSet<()> = JoinSet::new();

    loop {
        while inflight.try_join_next().is_some() {}

        if *stop_rx.borrow() {
            break;
        }

        // Emit a held subtitle whose hold window has expired (Task 15).
        if let Some(chinese) = hold.take_ready(Instant::now()) {
            if !emit_held_lines(
                chinese,
                &mut sentence_buffer,
                &shared,
                &deps,
                &semaphore,
                &mut inflight,
            ) {
                break;
            }
        }

        // Complete a buffered sentence that received no new fragment for
        // the sentence timeout (A2 / gh #1).
        if let Some(completed) = sentence_buffer.flush_if_timed_out(Instant::now()) {
            if !emit_subtitle(
                completed,
                SubtitleKind::Completed,
                &shared,
                &deps,
                &semaphore,
                &mut inflight,
            ) {
                break;
            }
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
                hold.set_hold_delay(latest.hold_delay);
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
                    // The frame is unchanged, so the current subtitle is
                    // still on screen: keep the SentenceBuffer timeout from
                    // firing mid-display (A2 / gh #1).
                    sentence_buffer.touch(Instant::now());
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

                // Extract one filtered line per visual line (top-to-bottom,
                // A2 / gh #1). An empty list means the frame has no CJK text.
                let lines = extract_chinese_subtitle_lines(&ocr_result.ocr_response);
                if lines.is_empty() {
                    // Blank / non-CJK frame: keep the dedupe blank counting
                    // and tell the SentenceBuffer the subtitle may have
                    // disappeared; a long enough blank run completes the
                    // buffered sentence.
                    let _ = deduper.process(None);
                    if let Some(completed) = sentence_buffer.note_blank(Instant::now()) {
                        if !emit_subtitle(
                            completed,
                            SubtitleKind::Completed,
                            &shared,
                            &deps,
                            &semaphore,
                            &mut inflight,
                        ) {
                            break;
                        }
                    }
                    continue;
                }

                // Dedupe + hold operate on the frame's lines joined with
                // "\n" (the dedupe normalizes to CJK + digits only, so the
                // separator does not affect comparison). The hold emits the
                // joined text; the loop top splits it back into lines.
                let joined = lines.join("\n");
                let Some(chinese) = deduper.process(Some(&joined)) else {
                    let state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.publish();
                    // The OCR text duplicates the previous line, so the
                    // subtitle is still on screen: keep the SentenceBuffer
                    // timeout from firing mid-display (A2 / gh #1).
                    sentence_buffer.touch(Instant::now());
                    continue;
                };

                // Hold-and-replace (Task 15): hold the new line for
                // `hold_delay`; a newer line arriving during the window
                // replaces it, so only the final (stable) line is emitted.
                // A zero hold delay emits immediately.
                if hold.push(chinese, Instant::now()) {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.snapshot.metrics.held_lines_replaced += 1;
                    state.publish();
                }
                if let Some(ready) = hold.take_ready(Instant::now()) {
                    if !emit_held_lines(
                        ready,
                        &mut sentence_buffer,
                        &shared,
                        &deps,
                        &semaphore,
                        &mut inflight,
                    ) {
                        break;
                    }
                }
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
    kind: SubtitleKind,
    shared: Arc<Mutex<SharedPipelineState>>,
    deps: PipelineDeps,
    semaphore: Arc<Semaphore>,
) {
    let Ok(_permit) = semaphore.acquire_owned().await else {
        return;
    };

    let is_completed = kind == SubtitleKind::Completed;

    // Preliminary translations are display-only: a newer emission makes
    // them stale and they are discarded entirely. A completed sentence is
    // never discarded as stale — it must be logged and spoken even when a
    // newer preliminary exists; only its display update is skipped then.
    {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return;
        }
        if !is_completed && seq < state.snapshot.latest_seq {
            state.snapshot.metrics.stale_translations_discarded += 1;
            state.publish();
            return;
        }
    }

    // Completed sentences consult the translation cache (A3 / gh #2); the
    // preliminary translation never does. The key is engine-scoped
    // (`helsinki|…` / `vietphrase|…`) so switching engines never serves
    // the other engine's translation. `get` normalizes the key
    // internally, so the raw sentence is passed here.
    let engine_id = deps.translator.engine_id().unwrap_or("unknown");
    let cached_vietnamese = if is_completed {
        deps.translation_cache
            .as_ref()
            .and_then(|cache| cache.get(&chinese, engine_id))
    } else {
        None
    };

    let translate_res = match cached_vietnamese {
        Some(vi) => Ok(vi),
        None => deps.translator.translate(&chinese).await,
    };

    let (vietnamese, enable_log, should_tts, voice, rate) = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return;
        }
        // For a completed sentence, a stale seq only skips the display
        // update (a newer emission already owns the screen); the log and
        // TTS below still run. A completed sentence is NEVER discarded
        // for resolving out of order: two sentences emitted together
        // (e.g. two speakers in one frame, A2 / gh #1) resolve
        // concurrently, and discarding the slower one would lose a line
        // of dialogue. The display stays on the newest sentence; the
        // log/TTS stream keeps every completed sentence. For a
        // preliminary, any staleness discards the whole task (checked above).
        let display_stale = seq < state.snapshot.latest_seq;
        if !is_completed && display_stale {
            state.snapshot.metrics.stale_translations_discarded += 1;
            state.publish();
            return;
        }

        match translate_res {
            Ok(vi) => {
                if !display_stale {
                    state.snapshot.vietnamese_line = vi.clone();
                }
                state.snapshot.metrics.translations_succeeded += 1;
                state.snapshot.error_banner = None;
                // Store a completed translation in the cache (A3). The
                // preliminary translation is never cached.
                if is_completed {
                    if let Some(cache) = &deps.translation_cache {
                        let _ = cache.put(&chinese, vi.clone(), engine_id);
                    }
                }
                let enable_log = is_completed && state.settings.enable_subtitle_log;
                let should_tts = is_completed && state.settings.auto_read && !state.settings.muted;
                let voice = state.settings.voice.clone();
                let rate = state.settings.speech_rate.clone();
                state.publish();
                (vi, enable_log, should_tts, voice, rate)
            }
            Err(err) => {
                if !display_stale {
                    state.snapshot.vietnamese_line.clear();
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

    // Completed sentences consult the TTS cache (A3 / gh #2); a cache hit
    // skips the `/tts` call entirely. `tts_calls` counts real syntheses.
    let (cached_mp3, tts_cache_hit) = match &deps.tts_cache {
        Some(cache) => match cache.get(&vietnamese, &voice, &rate) {
            Some(mp3) => (Some(mp3), true),
            None => (None, false),
        },
        None => (None, false),
    };

    let tts_res = match cached_mp3 {
        Some(mp3) => Ok(TtsSynthesis {
            audio: mp3,
            retries: Vec::new(),
        }),
        None => deps.tts.synthesize(&vietnamese, &voice, &rate).await,
    };

    // Retries and ultimate failures go to the subtitle log file, not just
    // the transient overlay banner: a 429 the client rode out must still
    // be visible afterwards, and a sentence that lost its audio must not
    // vanish silently. A cache hit has no retries to report. The log
    // write is best-effort (a failure here must not break the TTS path).
    if let Some(logger) = &deps.logger {
        match &tts_res {
            Ok(synthesis) => {
                for attempt in &synthesis.retries {
                    let _ = logger.log_tts_retry(&vietnamese, &voice, attempt);
                }
            }
            Err(err) => {
                // Log every retry attempt first, then the failure summary,
                // so a sentence that lost its audio still shows the full
                // 429 → retry → 429 → … trail in the file.
                for attempt in err.retries() {
                    let _ = logger.log_tts_retry(&vietnamese, &voice, attempt);
                }
                let _ = logger.log_tts_failure(&vietnamese, &voice, err);
            }
        }
    }

    let mp3_bytes = {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        if !tts_cache_hit {
            state.snapshot.metrics.tts_calls += 1;
        }
        if state.stopped || !state.settings.auto_read || state.settings.muted {
            state.publish();
            return;
        }
        match tts_res {
            Ok(synthesis) => {
                // Store freshly synthesized audio in the TTS cache (A3).
                if !tts_cache_hit {
                    if let Some(cache) = &deps.tts_cache {
                        let _ = cache.put(&vietnamese, &voice, &rate, synthesis.audio.clone());
                    }
                }
                synthesis.audio
            }
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
        map_translate_error_banner, Duration, Instant, PipelineError, PipelineErrorBanner,
        PipelineErrorKind, PipelineSettings, SentenceBuffer, SentenceEvent, SubtitleHold,
        DEFAULT_HOLD_DELAY, DEFAULT_SENTENCE_BLANK_FRAMES, ERR_BANNER_ENGINE_UNAVAILABLE,
        ERR_BANNER_TRANSLATE_NETWORK, MAX_SENTENCE_BUFFER_AGE,
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
    fn subtitle_hold_emits_after_delay_and_replaces_within_window() {
        let t0 = Instant::now();
        let mut hold = SubtitleHold::new(Duration::from_millis(400));

        // Single line: not ready before the deadline, ready at the deadline.
        assert!(!hold.push("夜深了，雨还在下。".to_string(), t0));
        assert_eq!(hold.take_ready(t0 + Duration::from_millis(399)), None);
        assert_eq!(
            hold.take_ready(t0 + Duration::from_millis(400)),
            Some("夜深了，雨还在下。".to_string())
        );
        assert_eq!(hold.take_ready(t0 + Duration::from_millis(400)), None);

        // A newer line inside the window replaces the held line (garbage suppressed).
        assert!(!hold.push("对不起时说找来晚了。".to_string(), t0));
        assert!(hold.push(
            "对不起，我来晚了。".to_string(),
            t0 + Duration::from_millis(100)
        ));
        assert_eq!(hold.replaced(), 1);
        // The garbage line's deadline (t0+400) has passed, but the clean line
        // is held until t0+500.
        assert_eq!(hold.take_ready(t0 + Duration::from_millis(400)), None);
        assert_eq!(
            hold.take_ready(t0 + Duration::from_millis(500)),
            Some("对不起，我来晚了。".to_string())
        );

        // Three rapid lines: only the last one survives.
        assert!(!hold.push("第一句。".to_string(), t0));
        assert!(hold.push("第二句。".to_string(), t0 + Duration::from_millis(50)));
        assert!(hold.push("第三句。".to_string(), t0 + Duration::from_millis(100)));
        assert_eq!(hold.replaced(), 3);
        assert_eq!(
            hold.take_ready(t0 + Duration::from_millis(500)),
            Some("第三句。".to_string())
        );
    }

    #[test]
    fn subtitle_hold_zero_delay_emits_immediately() {
        let t0 = Instant::now();
        let mut hold = SubtitleHold::new(Duration::ZERO);
        assert!(!hold.push("夜深了，雨还在下。".to_string(), t0));
        assert_eq!(hold.take_ready(t0), Some("夜深了，雨还在下。".to_string()));
    }

    #[test]
    fn sentence_buffer_accumulates_fragments_and_completes_on_punctuation() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        // First fragment: no final punctuation → preliminary only.
        assert_eq!(
            buf.push("明天".to_string(), t0),
            vec![SentenceEvent::Preliminary("明天".to_string())]
        );
        // Second fragment completes the sentence → re-translate the whole.
        assert_eq!(
            buf.push("早上见。".to_string(), t0 + Duration::from_millis(200)),
            vec![SentenceEvent::Completed("明天早上见。".to_string())]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn sentence_buffer_completes_immediately_on_final_punctuation() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        assert_eq!(
            buf.push("你好吗？".to_string(), t0),
            vec![SentenceEvent::Completed("你好吗？".to_string())]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn sentence_buffer_splits_two_sentences_in_one_fragment() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        assert_eq!(
            buf.push("你好。我很好".to_string(), t0),
            vec![
                SentenceEvent::Completed("你好。".to_string()),
                SentenceEvent::Preliminary("我很好".to_string()),
            ]
        );
        assert_eq!(buf.buffered_text(), "我很好");
    }

    #[test]
    fn sentence_buffer_new_line_completes_previous_utterance() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        // First visual line: incomplete, preliminary only.
        assert_eq!(
            buf.push("你好吗".to_string(), t0),
            vec![SentenceEvent::Preliminary("你好吗".to_string())]
        );
        // Second visual line in the SAME frame: separate utterance — the
        // first line completes before the second starts.
        assert_eq!(
            buf.push_new_line("我很好。".to_string(), t0),
            vec![
                SentenceEvent::Completed("你好吗".to_string()),
                SentenceEvent::Completed("我很好。".to_string()),
            ]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn sentence_buffer_flushes_on_blank_run_and_on_timeout() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        assert_eq!(
            buf.push("等一下".to_string(), t0),
            vec![SentenceEvent::Preliminary("等一下".to_string())]
        );
        // Fewer blanks than the threshold: nothing yet.
        for _ in 0..(DEFAULT_SENTENCE_BLANK_FRAMES - 1) {
            assert_eq!(buf.note_blank(t0), None);
        }
        // The threshold blank completes the sentence (subtitle disappeared).
        assert_eq!(buf.note_blank(t0), Some("等一下".to_string()));
        assert!(buf.is_empty());

        // Timeout: content with no new fragment for the timeout window.
        assert_eq!(
            buf.push("再见".to_string(), t0),
            vec![SentenceEvent::Preliminary("再见".to_string())]
        );
        assert_eq!(
            buf.flush_if_timed_out(t0 + Duration::from_millis(1199)),
            None
        );
        assert_eq!(
            buf.flush_if_timed_out(t0 + Duration::from_millis(1200)),
            Some("再见".to_string())
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn sentence_buffer_touch_keeps_long_displayed_fragment_from_timing_out() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        // Fragment with no final punctuation: preliminary only.
        assert_eq!(
            buf.push("你好，我是".to_string(), t0),
            vec![SentenceEvent::Preliminary("你好，我是".to_string())]
        );
        // The subtitle stays on screen (dedupe / pixel-diff hide the
        // repeated reads): touching the buffer keeps the timeout from
        // firing, so the fragment is NOT completed at t0 + 1200 ms.
        buf.touch(t0 + Duration::from_millis(1100));
        assert_eq!(
            buf.flush_if_timed_out(t0 + Duration::from_millis(1200)),
            None
        );
        // A continuation arriving later still joins the same sentence.
        assert_eq!(
            buf.push("小明。".to_string(), t0 + Duration::from_millis(2000)),
            vec![SentenceEvent::Completed("你好，我是小明。".to_string())]
        );
        assert!(buf.is_empty());

        // Touching an empty buffer is a no-op (no phantom timeout).
        buf.touch(t0 + Duration::from_millis(3000));
        assert_eq!(
            buf.flush_if_timed_out(t0 + Duration::from_millis(5000)),
            None
        );
    }

    #[test]
    fn sentence_buffer_max_age_completes_unpunctuated_long_display() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        assert_eq!(
            buf.push("是".to_string(), t0),
            vec![SentenceEvent::Preliminary("是".to_string())]
        );
        // Repeated touches (subtitle still on screen) keep the short
        // timeout from firing, but the 10 s max-age safety valve still
        // completes the sentence.
        for millis in (1000..10000).step_by(1000) {
            buf.touch(t0 + Duration::from_millis(millis));
            assert_eq!(
                buf.flush_if_timed_out(t0 + Duration::from_millis(millis)),
                None
            );
        }
        assert_eq!(
            buf.flush_if_timed_out(t0 + MAX_SENTENCE_BUFFER_AGE),
            Some("是".to_string())
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn sentence_buffer_blank_resets_when_a_fragment_arrives() {
        let t0 = Instant::now();
        let mut buf = SentenceBuffer::new(Duration::from_millis(1200), 2);
        assert_eq!(
            buf.push("等一下".to_string(), t0),
            vec![SentenceEvent::Preliminary("等一下".to_string())]
        );
        assert_eq!(buf.note_blank(t0), None);
        // A new fragment resets the blank run, so one more blank is not enough.
        assert_eq!(
            buf.push("，好吗".to_string(), t0),
            vec![SentenceEvent::Preliminary("等一下，好吗".to_string())]
        );
        assert_eq!(buf.note_blank(t0), None);
        assert_eq!(buf.note_blank(t0), Some("等一下，好吗".to_string()));
    }

    #[test]
    fn pipeline_settings_hold_delay_defaults_to_400ms_and_caps_at_1s() {
        let mut cfg = AppConfig::default();
        cfg.general.region = Some(CaptureRegion {
            monitor_id: "DISPLAY1".to_string(),
            x: 100,
            y: 900,
            width: 800,
            height: 80,
        });
        let settings = match PipelineSettings::from_app_config(&cfg) {
            Ok(s) => s,
            Err(err) => panic!("expected valid AppConfig conversion, got {err}"),
        };
        assert_eq!(settings.hold_delay, DEFAULT_HOLD_DELAY);
        assert_eq!(settings.hold_delay, Duration::from_millis(400));

        let mut too_long = settings;
        too_long.hold_delay = Duration::from_millis(1001);
        assert!(matches!(
            too_long.validate(),
            Err(PipelineError::InvalidSettings(_))
        ));
    }

    #[test]
    fn translate_error_maps_to_expected_vietnamese_banner_kinds() {
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
            map_translate_error_banner(&TranslateError::EngineUnavailable {
                detail: "no model, no dictionaries".to_string(),
            }),
            PipelineErrorBanner {
                kind: PipelineErrorKind::TranslateEngineUnavailable,
                message: ERR_BANNER_ENGINE_UNAVAILABLE.to_string(),
            }
        );
    }
}
