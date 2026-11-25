//! Async subtitle translation pipeline orchestrator and daily UTF-8 logger.

pub mod cache;
pub mod logger;

pub use cache::{normalize_cache_key, CacheError, CachedAudio, TranslationCache, TtsCache};
pub use logger::{
    daily_log_path, daily_ocr_debug_path, format_ocr_debug_line, format_subtitle_log_line,
    OcrDebugLogger, SubtitleLogError, SubtitleLogger,
};

use crate::api::{TranslateError, Translator, TtsEngine, TtsSynthesis};
use crate::audio::{validate_volume, AudioError, AudioPlayback};
use crate::capture::{validate_region_dimensions, FrameCapturer, PixelDiffGate};
use crate::config::{AppConfig, CaptureRegion};
use crate::ocr::{
    extract_chinese_subtitle_lines_with_decisions, normalize_for_comparison, normalized_similarity,
    FuzzyDeduplicator, OcrEngine, TextDeduplicator, DEFAULT_DEDUPE_THRESHOLD,
};
use icu_segmenter::options::SentenceBreakInvariantOptions;
use icu_segmenter::{SentenceSegmenter, SentenceSegmenterBorrowed};
use std::sync::{Arc, LazyLock, Mutex};
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

/// One completed subtitle pair (Chinese + its Vietnamese translation)
/// as shown on the overlay. The pipeline keeps the last two for the
/// optional subtitle-history display (fixed-size overlay, 2026-11-25):
/// the overlay shows the newest `config.display.history_lines` of
/// them, dimmed, above the current subtitle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitlePair {
    pub chinese: String,
    pub vietnamese: String,
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
    /// Most recently completed subtitle pairs, most-recent-first, at
    /// most `MAX_HISTORY_PAIRS` (see `crate::ui::overlay`).
    pub history: Vec<SubtitlePair>,
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
            history: Vec::new(),
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
    /// Raw-OCR debug log toggle (merge fix, 2026-11-25): recorded here so
    /// the orchestrator knows the setting, but the logger itself lives in
    /// `PipelineDeps` (built at pipeline start) — the toggle takes effect
    /// on the next pipeline start.
    pub debug_logging: bool,
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
            debug_logging: config.general.debug_logging,
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
    /// Raw-OCR debug log (merge fix, 2026-11-25): one line per OCR call
    /// with the raw blocks, before dedupe/hold/buffer. Lives beside the
    /// subtitle log (`ocr-debug-YYYY-MM-DD.log`); `None` disables it.
    /// Constructed together with `logger` from the same log directory.
    pub ocr_debug_logger: Option<Arc<OcrDebugLogger>>,
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
    /// In-progress multi-line display group (2-line bug fix): the frame
    /// whose lines the overlay is currently showing, if it had more than
    /// one line. `None` for single-line frames.
    display_group: Option<DisplayGroup>,
    /// Last completed subtitle pair that became the displayed current
    /// pair (most recent first is `history`). Only Completed landings
    /// that actually update the display set this; preliminaries and
    /// stale completions never do.
    last_completed_pair: Option<SubtitlePair>,
    /// Emission sequence of `last_completed_pair` (0 = none recorded).
    /// History only moves forward: a translation that resolves out of
    /// order (older `seq`) never moves it backwards.
    last_completed_seq: u64,
    /// Completed pairs that used to be current, most-recent-first, at
    /// most two (mirrored into `snapshot.history` on every change).
    history: Vec<SubtitlePair>,
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

    /// Record a resolved Vietnamese translation for a line of the current
    /// multi-line display group (2-line bug fix). Delegates to the group;
    /// `NotGrouped` when there is no group or the text is not one of its
    /// lines (single-line frames and superseded frames).
    fn note_group_translation(&mut self, chinese: &str, vietnamese: String) -> GroupContribution {
        match self.display_group.as_mut() {
            Some(group) => group.note(chinese, vietnamese),
            None => GroupContribution::NotGrouped,
        }
    }

    /// Record a Completed pair for subtitle history, in emission order
    /// (subtitle history, 2026-11-25). Called when a Completed
    /// translation succeeds — single line or joined group — and moves
    /// the previous completed pair into history, then records this pair
    /// as the latest, mirroring the history into the snapshot.
    ///
    /// This runs at translation-complete time, not at emit time: the
    /// previous design pushed at emit (when the next sentence takes
    /// the screen) but only recorded the pair at display time (after
    /// its translation finished), so a sentence emitted while the
    /// previous translation was still in flight was lost from history
    /// (the next emission's push ran before the previous pair was
    /// recorded). Recording push+record atomically here closes that
    /// race. The display update itself stays gated by staleness
    /// elsewhere; history only needs the sentence to have completed
    /// (translated + logged), and its Chinese was already shown on
    /// screen at emit time.
    ///
    /// Ordering by `seq`: a pair newer than the latest becomes the new
    /// latest (the old latest moves into history). A pair OLDER than
    /// the latest — typically a multi-line group whose lines land after
    /// a newer single line was already recorded — is inserted at the
    /// front of history instead of being dropped: it was emitted after
    /// the pairs already in history, only its translation landed late,
    /// so it is newer than them and belongs ahead of them (the current
    /// latest stays latest). Guards: nothing is pushed when there is no
    /// previous pair, when the new sentence repeats the previous pair's
    /// text, or when the previous pair is already in history.
    fn record_completed_for_history(&mut self, seq: u64, chinese: String, vietnamese: String) {
        // Never record a pair that is already the latest or already in
        // history (a repeated line, or a group line resolving twice).
        if self
            .last_completed_pair
            .as_ref()
            .is_some_and(|pair| pair.chinese == chinese)
            || self.history.iter().any(|pair| pair.chinese == chinese)
        {
            return;
        }
        if seq >= self.last_completed_seq {
            if let Some(prev) = self.last_completed_pair.take() {
                if prev.chinese != chinese
                    && !self.history.iter().any(|pair| pair.chinese == prev.chinese)
                {
                    self.history.insert(0, prev);
                    self.history.truncate(2);
                }
            }
            self.last_completed_pair = Some(SubtitlePair {
                chinese,
                vietnamese,
            });
            self.last_completed_seq = seq;
        } else {
            // Older pair resolving late: insert at the front of history
            // (newer than the pairs already there); the latest stays.
            self.history.insert(
                0,
                SubtitlePair {
                    chinese,
                    vietnamese,
                },
            );
            self.history.truncate(2);
        }
        self.snapshot.history = self.history.clone();
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
            display_group: None,
            last_completed_pair: None,
            last_completed_seq: 0,
            history: Vec::new(),
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
    /// Text of the most recently replaced pending line (`None` when the
    /// last push did not replace anything). Read by the OCR debug log so
    /// a hold replacement can show old → new (merge fix, 2026-11-25).
    last_replaced: Option<String>,
}

impl SubtitleHold {
    pub fn new(hold_delay: Duration) -> Self {
        Self {
            hold_delay,
            pending: None,
            replaced: 0,
            last_replaced: None,
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

    /// Text of the line replaced by the most recent push, if the push
    /// replaced a pending line.
    pub fn last_replaced(&self) -> Option<&str> {
        self.last_replaced.as_deref()
    }

    /// Text of the currently pending line, if any (the newest line after
    /// the most recent push).
    pub fn pending_text(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .map(|pending| pending.chinese.as_str())
    }

    /// Push a newly deduped line. Returns `true` when it replaced a pending
    /// line. A zero hold delay stores the line with a deadline of `now`, so
    /// `take_ready(now)` returns it right away.
    pub fn push(&mut self, chinese: String, now: Instant) -> bool {
        self.last_replaced = self.pending.take().map(|pending| pending.chinese);
        if self.last_replaced.is_some() {
            self.replaced += 1;
        }
        self.pending = Some(PendingSubtitle {
            chinese,
            deadline: now + self.hold_delay,
        });
        self.last_replaced.is_some()
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

/// Sentence segmenter (ICU4X, UAX #29) shared by the buffer — replaces the
/// hand-written 7-character `SENTENCE_FINAL_PUNCT` list (merge fix,
/// 2026-11-25). `SentenceSegmenter::new` is a `const fn` returning the
/// borrowed `&'static` view (`SentenceSegmenterBorrowed`: `Copy`,
/// `Send + Sync`); the owned `SentenceSegmenter` holds an `Rc`-carted data
/// payload and is neither `Send` nor `Sync`, so it cannot live in a static.
/// `compiled_data` bakes the sentence-break tables into the binary; the
/// segmenter is built once per process. See `segment_is_complete` for how
/// a trailing fragment is judged.
static SENTENCE_SEGMENTER: LazyLock<SentenceSegmenterBorrowed<'static>> =
    LazyLock::new(|| SentenceSegmenter::new(SentenceBreakInvariantOptions::default()));

/// Split `text` into sentence segments with the shared segmenter (UAX
/// #29). `segment_str` yields breakpoint byte offsets (always starting at
/// 0 and ending at `text.len()`), so the segments are the slices between
/// consecutive breakpoints.
fn sentence_segments(text: &str) -> Vec<&str> {
    let breaks: Vec<usize> = SENTENCE_SEGMENTER.segment_str(text).collect();
    breaks
        .windows(2)
        .map(|pair| &text[pair[0]..pair[1]])
        .collect()
}

/// Whether a sentence segment ends a sentence, judged by the segmenter
/// itself (UAX #29 has no "is this a complete sentence?" query): append a
/// dummy sentence after the segment; if the segmenter breaks right after
/// the segment, the segment ended with sentence-terminal punctuation —
/// allowing trailing closers such as `"` or `」`, which the old punct list
/// stranded in the buffer as a lone `"` preliminary. If it does not break
/// (no terminal punctuation), the segment is an unfinished fragment.
///
/// This also fixes two cases the old list got wrong: `3.5` (the old list
/// split at the `.`) and `...` after a closing quote.
///
/// One explicit exception: ICU4X 2.4 (invariant locale) does NOT break
/// after U+2026 HORIZONTAL ELLIPSIS — verified by probe 2026-11-25
/// (`等一下…… X`, `等一下…… 再见`, and `等一下……你好` all stay one
/// segment), although a trailing ellipsis is a complete subtitle line in
/// practice and the old list treated it as terminal. A segment ending in
/// `…` is therefore treated as complete by an explicit check, preserving
/// that one behavior instead of silently losing it.
fn segment_is_complete(segment: &str) -> bool {
    if segment.trim_end().ends_with('\u{2026}') {
        return true;
    }
    let probe = format!("{segment} X");
    match sentence_segments(&probe).into_iter().next() {
        Some(first) => first.trim_end() == segment.trim_end(),
        None => false,
    }
}

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

/// How an incoming fragment relates to the buffered sentence (merge fix,
/// 2026-11-25). Comparison uses the same normalized text (CJK ideographs +
/// digits only) and the same normalized-Levenshtein similarity as the
/// upstream `FuzzyDeduplicator`, so the buffer and the dedupe agree on what
/// counts as "the same subtitle".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FragmentRelation {
    /// The new fragment extends the buffer (the buffer's normalized text
    /// is a strict prefix of the fragment's): a progressive reveal, or a
    /// re-read with more text. The longer reading wins.
    ExtendsLonger,
    /// The new fragment is a strict prefix of the buffer (a truncated
    /// re-read): the buffer already holds the longer reading.
    ExtendsShorter,
    /// The fragment is highly similar to the buffer (>= the configured
    /// dedupe threshold): the same subtitle re-read with OCR misread(s).
    /// The freshest reading wins.
    SameSubtitle,
    /// None of the above: a new subtitle began (hard cut). The buffer
    /// completes first; the fragment starts a fresh sentence.
    NewSubtitle,
}

/// What `SentenceBuffer::push` decided about the latest fragment (merge
/// fix, 2026-11-25). Read by the OCR debug log so the next real run can
/// show, per fragment, whether the buffer extended, kept, replaced, or
/// flushed-and-restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferDecision {
    /// The fragment extended the buffer (progressive reveal).
    Extended,
    /// The fragment was a truncated re-read; the buffer kept the longer
    /// reading.
    KeptLonger,
    /// The fragment replaced the buffer (highly similar re-read).
    ReplacedSimilar,
    /// The fragment was a new subtitle; the buffer flushed first.
    NewSubtitle,
}

impl BufferDecision {
    /// Short stable label for the debug log.
    pub fn as_str(&self) -> &'static str {
        match self {
            BufferDecision::Extended => "Extended",
            BufferDecision::KeptLonger => "KeptLonger",
            BufferDecision::ReplacedSimilar => "ReplacedSimilar",
            BufferDecision::NewSubtitle => "NewSubtitle",
        }
    }
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
/// assembles those stable lines into sentences. The hold suppresses
/// transitional garbage frames; the buffer decides, per fragment, whether
/// the fragment continues, re-reads, or replaces the buffered sentence
/// (merge fix, 2026-11-25 — see `push`).
///
/// Known limit, stated plainly: a sentence split across two subtitle cards
/// whose second card shares no prefix and no high similarity with the
/// first (e.g. `你好，我是` then `小明。`) is indistinguishable from a new
/// subtitle, and this rule treats it as a new subtitle — the first card
/// completes on its own and the second starts fresh. The old behavior
/// appended everything, which merged unrelated consecutive subtitles into
/// one long line (real log 2026-11-25: 33/383 lines merged); the new
/// behavior only ever joins fragments that provably continue or re-read
/// the buffered sentence. Each half still translates correctly on its own.
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
    /// Normalized-Levenshtein similarity at or above which an incoming
    /// fragment counts as a re-read of the buffered subtitle (merge fix,
    /// 2026-11-25). Always kept equal to the upstream dedupe threshold:
    /// the orchestrator sets it from `PipelineSettings` at construction
    /// and on every settings change.
    similarity_threshold: f32,
    /// What `push` decided about the latest fragment (`None` when the
    /// buffer was empty and the fragment simply started a sentence).
    /// Read by the OCR debug log (merge fix, 2026-11-25).
    last_decision: Option<BufferDecision>,
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
            similarity_threshold: DEFAULT_DEDUPE_THRESHOLD,
            last_decision: None,
        }
    }

    /// Keep the re-read similarity threshold equal to the configured
    /// dedupe threshold (called by the orchestrator at construction and
    /// on settings changes).
    pub fn set_similarity_threshold(&mut self, threshold: f32) {
        self.similarity_threshold = threshold;
    }

    /// What the latest `push` decided about its fragment (`None` when
    /// the fragment simply started a sentence in an empty buffer).
    pub fn last_decision(&self) -> Option<BufferDecision> {
        self.last_decision
    }

    /// Current buffered (incomplete) text, if any.
    pub fn buffered_text(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Decide, per fragment, what the fragment does to the buffered
    /// sentence (merge fix, 2026-11-25 — the old code appended every
    /// fragment unconditionally, so two consecutive subtitles with no
    /// terminal punctuation and no blank gap merged into one long line):
    ///
    /// * fragment EXTENDS the buffer (buffer is a strict prefix of the
    ///   fragment, or vice versa) — progressive reveal / re-read with
    ///   more text: REPLACE the buffer with the longer reading.
    /// * fragment is HIGHLY SIMILAR to the buffer (>= the configured
    ///   dedupe threshold) — same subtitle re-read with OCR misread(s):
    ///   REPLACE the buffer with the freshest reading.
    /// * otherwise — a new subtitle began: FLUSH the buffer as Completed
    ///   first, then push the fragment fresh.
    ///
    /// Returns the events to act on, in order: zero or more completed
    /// sentences, then at most one preliminary for the incomplete remainder.
    pub fn push(&mut self, fragment: String, now: Instant) -> Vec<SentenceEvent> {
        self.blank_frames = 0;
        self.last_push = Some(now);
        if self.buffer.is_empty() {
            self.buffer_started = Some(now);
            self.last_decision = None;
            self.buffer = fragment;
            return self.drain_completed();
        }
        let mut events = Vec::new();
        match self.classify_fragment(&fragment) {
            FragmentRelation::ExtendsLonger => {
                self.last_decision = Some(BufferDecision::Extended);
                self.buffer = fragment;
            }
            FragmentRelation::SameSubtitle => {
                self.last_decision = Some(BufferDecision::ReplacedSimilar);
                self.buffer = fragment;
            }
            FragmentRelation::ExtendsShorter => {
                // Truncated re-read: the buffer already holds the longer
                // reading, so keep it (only the timeout is refreshed).
                self.last_decision = Some(BufferDecision::KeptLonger);
            }
            FragmentRelation::NewSubtitle => {
                self.last_decision = Some(BufferDecision::NewSubtitle);
                if let Some(done) = self.flush() {
                    events.push(SentenceEvent::Completed(done));
                }
                // `flush` clears the timing state; the fresh fragment
                // starts a new sentence, so restart it here.
                self.buffer_started = Some(now);
                self.last_push = Some(now);
                self.buffer = fragment;
            }
        }
        events.extend(self.drain_completed());
        events
    }

    /// Classify an incoming fragment against the buffered sentence. Both
    /// sides are compared normalized (CJK ideographs + digits only), the
    /// same normalization the upstream dedupe uses, so punctuation and
    /// spacing differences never decide the outcome.
    fn classify_fragment(&self, fragment: &str) -> FragmentRelation {
        let norm_buf = normalize_for_comparison(&self.buffer);
        let norm_frag = normalize_for_comparison(fragment);
        let buf_len = norm_buf.chars().count();
        let frag_len = norm_frag.chars().count();
        if frag_len > buf_len && norm_frag.starts_with(&norm_buf) {
            return FragmentRelation::ExtendsLonger;
        }
        if buf_len > frag_len && norm_buf.starts_with(&norm_frag) {
            return FragmentRelation::ExtendsShorter;
        }
        if normalized_similarity(&norm_buf, &norm_frag) >= self.similarity_threshold {
            return FragmentRelation::SameSubtitle;
        }
        FragmentRelation::NewSubtitle
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
    /// incomplete remainder, if any. Sentence boundaries come from the
    /// ICU4X segmenter (UAX #29, merge fix 2026-11-25), not from a
    /// hand-written punctuation list: every segment except the last is a
    /// complete sentence by definition, and the last segment is complete
    /// only when `segment_is_complete` says so. The incomplete remainder
    /// stays buffered (and is emitted as a Preliminary for display).
    fn drain_completed(&mut self) -> Vec<SentenceEvent> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let segments: Vec<String> = sentence_segments(&self.buffer)
            .into_iter()
            .map(|segment| segment.trim().to_string())
            .filter(|segment| !segment.is_empty())
            .collect();
        let mut events = Vec::new();
        let last_idx = segments.len().saturating_sub(1);
        let mut remainder = String::new();
        for (idx, segment) in segments.into_iter().enumerate() {
            if idx < last_idx || segment_is_complete(&segment) {
                events.push(SentenceEvent::Completed(segment));
            } else {
                remainder = segment;
            }
        }
        self.buffer = remainder.clone();
        if !remainder.is_empty() {
            events.push(SentenceEvent::Preliminary(remainder));
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
        // History is recorded when the translation succeeds
        // (`record_completed_for_history` in `process_emitted_subtitle`),
        // not here: pushing at emit time raced with the previous
        // sentence's translation still being in flight.
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

/// What recording a line's translation into the current display group
/// means for the overlay (2-line subtitle bug fix).
#[derive(Debug, Clone, PartialEq, Eq)]
enum GroupContribution {
    /// The line is not part of the current display group: use the
    /// ordinary single-line display rules.
    NotGrouped,
    /// The line belongs to the group but siblings are still pending:
    /// do not show this line's translation alone (the overlay would
    /// otherwise flash a single line before the group completes).
    GroupedPending,
    /// Every line of the group now has a translation: the joined
    /// translations in top-to-bottom line order.
    GroupedComplete(String),
}

/// One display unit for a multi-line subtitle frame (2-line bug fix).
///
/// A frame with N visual lines is shown on the overlay as N Chinese lines
/// plus (once resolved) N Vietnamese lines, while log/TTS still treat each
/// line as a separate utterance (A2 / gh #1). Without this, the N lines
/// are emitted back-to-back into a single-line display: each emission
/// overwrites `snapshot.chinese_line`, the first lines' translations are
/// suppressed as stale, and the overlay only ever shows the last line —
/// which is exactly what Danh saw (only "再见。" for a "你好。 / 再见。"
/// frame) even though both lines were logged.
///
/// Keyed by the Chinese line text, not seq: a line is emitted once as
/// Preliminary and again as Completed when it is flushed later, so the
/// seq is not stable across the two emissions, but the text is.
#[derive(Debug, Clone, Default)]
struct DisplayGroup {
    /// The frame's lines in top-to-bottom order, each with its resolved
    /// Vietnamese translation (`None` until that line's translation lands).
    lines: Vec<(String, Option<String>)>,
}

impl DisplayGroup {
    fn new(lines: Vec<String>) -> Self {
        Self {
            lines: lines.into_iter().map(|line| (line, None)).collect(),
        }
    }

    /// The frame's Chinese lines joined in top-to-bottom order, as
    /// displayed on the overlay (subtitle history, 2026-11-25).
    fn joined_chinese(&self) -> String {
        self.lines
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Record a resolved Vietnamese translation for one of the group's
    /// lines (matched by Chinese text; the first unresolved entry wins,
    /// so identical twin lines fill in order).
    fn note(&mut self, chinese: &str, vietnamese: String) -> GroupContribution {
        let mut found = false;
        for (line, vi) in self.lines.iter_mut() {
            if line == chinese && vi.is_none() {
                *vi = Some(vietnamese);
                found = true;
                break;
            }
        }
        if !found {
            return GroupContribution::NotGrouped;
        }
        if self.lines.iter().all(|(_, vi)| vi.is_some()) {
            let mut joined = String::new();
            for (_, vi) in &self.lines {
                if !joined.is_empty() {
                    joined.push('\n');
                }
                if let Some(v) = vi {
                    joined.push_str(v);
                }
            }
            GroupContribution::GroupedComplete(joined)
        } else {
            GroupContribution::GroupedPending
        }
    }
}

/// Whether a character is a CJK ideograph (Han). Chinese writes no spaces
/// between words, so whitespace between two CJK clusters almost always
/// marks a speaker change (or a line break OCR collapsed into one line).
fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{3400}'..='\u{4DBF}'   // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
        | '\u{F900}'..='\u{FAFF}' // Compatibility Ideographs
        | '\u{20000}'..='\u{2A6DF}' // Extension B
        | '\u{2A700}'..='\u{2B73F}' // Extension C
        | '\u{2B740}'..='\u{2B81F}' // Extension D
        | '\u{2B820}'..='\u{2CEAF}' // Extension E
        | '\u{2CEB0}'..='\u{2EBEF}' // Extension F
        | '\u{30000}'..='\u{3134F}' // Extension G
        | '\u{31350}'..='\u{323AF}') // Extension H
}

/// Dash characters that mark a second speaker in subtitles: ASCII
/// hyphen-minus, full-width hyphen-minus, en dash, em dash.
fn is_speaker_dash(ch: char) -> bool {
    matches!(ch, '-' | '\u{FF0D}' | '\u{2013}' | '\u{2014}')
}

/// CJK / ASCII punctuation ignored when looking for the CJK clusters on
/// either side of a separator run — a comma or closing quote between the
/// cluster and the separator does not hide the speaker change.
fn is_split_context_punct(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '？'
            | '！'
            | '；'
            | '：'
            | '、'
            | '‘'
            | '’'
            | '“'
            | '”'
            | '「'
            | '」'
            | '『'
            | '』'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '…'
            | '·'
            | ','
            | '.'
            | '?'
            | '!'
            | ';'
            | ':'
            | '\''
            | '"'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
    )
}

/// Nearest char to the left (`left = true`, scanning back from `pos`) or
/// right (`left = false`, scanning forward from `pos`) of a separator run,
/// skipping punctuation and separator chars. Returns the char's index in
/// `chars` plus the char itself.
fn nearest_context_char(chars: &[(usize, char)], pos: usize, left: bool) -> Option<(usize, char)> {
    if left {
        let mut i = pos;
        while i > 0 {
            i -= 1;
            let (_, ch) = chars[i];
            if !(ch.is_whitespace() || is_speaker_dash(ch) || is_split_context_punct(ch)) {
                return Some((i, ch));
            }
        }
        None
    } else {
        let mut i = pos;
        while i < chars.len() {
            let (_, ch) = chars[i];
            if !(ch.is_whitespace() || is_speaker_dash(ch) || is_split_context_punct(ch)) {
                return Some((i, ch));
            }
            i += 1;
        }
        None
    }
}

/// Length of the CJK run ending at (and including) the char at `pos`.
/// Call only when that char is CJK.
fn cjk_run_len_before(chars: &[(usize, char)], pos: usize) -> usize {
    let mut len = 0;
    let mut i = pos;
    loop {
        if !is_cjk(chars[i].1) {
            break;
        }
        len += 1;
        if i == 0 {
            break;
        }
        i -= 1;
    }
    len
}

/// Length of the CJK run starting at (and including) the char at `pos`.
/// Call only when that char is CJK.
fn cjk_run_len_after(chars: &[(usize, char)], pos: usize) -> usize {
    let mut len = 0;
    let mut i = pos;
    while i < chars.len() {
        if !is_cjk(chars[i].1) {
            break;
        }
        len += 1;
        i += 1;
    }
    len
}

/// Strip leading speaker dashes (and spaces) from a piece — the dash that
/// marks the second speaker (e.g. OCR `A -B` split on the space leaves
/// `-B`). Applied to every piece except the first: a dash leading the
/// whole line is kept verbatim (it may be OCR noise, not a speaker mark).
fn strip_leading_speaker_dash(piece: &str) -> &str {
    piece.trim_start_matches(|ch: char| ch.is_whitespace() || is_speaker_dash(ch))
}

/// Split one OCR line into speaker utterances (speaker layer, merge fix
/// 2026-11-25 — sits ON TOP of the ICU4X sentence layer in
/// `SentenceBuffer::drain_completed`).
///
/// Two speaker-change signals:
/// (a) whitespace between two CJK clusters — Chinese uses no spaces
///     between words, so this is almost always a speaker change or a
///     line break OCR collapsed into one line;
/// (b) a dash (`-`, `－`, `–`, `—`) between two CJK clusters, or at the
///     start of the second piece.
///
/// Guardrails (do NOT split): the separator's neighbours are not both
/// CJK (numbers/dates like `9 日` or `3.5`, CJK next to Latin/digits);
/// either adjacent CJK cluster is shorter than 2 characters (a lone
/// interjection such as `是 啊` stays one line); punctuation between the
/// cluster and the separator is ignored when finding the neighbours.
///
/// Known limit, stated plainly: OCR sometimes misreads `-` or inserts
/// spurious spaces (a single sentence split at a spurious space becomes
/// two utterances — each still translates on its own), and two sentences
/// stuck together with no signal at all cannot be split (the blind case,
/// e.g. `大家就开心地享受吧好的好的那就再见了` stays one line).
pub fn split_speakers(line: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut pieces: Vec<String> = Vec::new();
    let mut piece_start = 0usize;
    let mut idx = 0usize;
    while idx < chars.len() {
        let (byte_idx, ch) = chars[idx];
        if !(ch.is_whitespace() || is_speaker_dash(ch)) {
            idx += 1;
            continue;
        }
        // Maximal separator run (whitespace and/or dashes) starting here.
        let run_start = byte_idx;
        let mut run_end = byte_idx + ch.len_utf8();
        let mut j = idx + 1;
        while j < chars.len() {
            let (b, c) = chars[j];
            if c.is_whitespace() || is_speaker_dash(c) {
                run_end = b + c.len_utf8();
                j += 1;
            } else {
                break;
            }
        }
        let left = nearest_context_char(&chars, idx, true);
        let right = nearest_context_char(&chars, j, false);
        let split_here = match (left, right) {
            (Some((li, lch)), Some((ri, rch))) if is_cjk(lch) && is_cjk(rch) => {
                cjk_run_len_before(&chars, li) >= 2 && cjk_run_len_after(&chars, ri) >= 2
            }
            _ => false,
        };
        if split_here {
            let piece = line[piece_start..run_start].trim();
            if !piece.is_empty() {
                pieces.push(piece.to_string());
            }
            piece_start = run_end;
        }
        idx = j;
    }
    let tail = line[piece_start..].trim();
    if !tail.is_empty() {
        pieces.push(tail.to_string());
    }
    // A dash leading a later piece marks its speaker; strip it (the first
    // piece keeps a leading dash verbatim — see `strip_leading_speaker_dash`).
    for piece in pieces.iter_mut().skip(1) {
        let stripped = strip_leading_speaker_dash(piece);
        if !stripped.is_empty() {
            *piece = stripped.to_string();
        }
    }
    pieces.retain(|piece| !piece.is_empty());
    if pieces.is_empty() {
        pieces.push(line.trim().to_string());
    }
    pieces
}

/// Split one OCR line into the utterances it would become after the
/// speaker rules (this layer) and sentence segmentation (ICU4X, in
/// `SentenceBuffer::drain_completed`): speaker pieces, each further split
/// at sentence boundaries, with the trailing incomplete segment kept as
/// its own utterance (it flushes as one line on blank/timeout). Used by
/// the log-counting example and by tests; the live pipeline applies the
/// same two layers in sequence.
pub fn split_line_into_utterances(line: &str) -> Vec<String> {
    let mut utterances = Vec::new();
    for piece in split_speakers(line) {
        for segment in sentence_segments(&piece) {
            let segment = segment.trim();
            if !segment.is_empty() {
                utterances.push(segment.to_string());
            }
        }
    }
    if utterances.is_empty() {
        utterances.push(line.trim().to_string());
    }
    utterances
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
    // A new held frame supersedes the previous display: drop any
    // in-progress multi-line group so a late translation from the old
    // frame can never overwrite the new frame's display.
    {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        state.display_group = None;
    }
    // Speaker layer (merge fix, 2026-11-25): each visual line is first
    // split into speaker utterances (`split_speakers`); every piece is a
    // separate utterance exactly like a visual line from the same frame
    // (first piece continues the buffer via `push`, the rest flush first
    // via `push_new_line`). The display group below is built from the
    // pieces, so the overlay shows every speaker's line.
    let mut lines: Vec<String> = Vec::new();
    for line in joined.split('\n').filter(|line| !line.is_empty()) {
        let pieces = split_speakers(line);
        if pieces.len() > 1 {
            if let Some(dbg) = &deps.ocr_debug_logger {
                let _ = dbg.log_speaker_split(line, &pieces);
            }
        }
        lines.extend(pieces);
    }
    let mut first_line = true;
    for line in &lines {
        let events = if first_line {
            sentence_buffer.push(line.to_string(), Instant::now())
        } else {
            sentence_buffer.push_new_line(line.to_string(), Instant::now())
        };
        // Buffer decision for the debug log: what the buffer did with
        // this piece, which sentences completed, and what stays buffered.
        if let Some(dbg) = &deps.ocr_debug_logger {
            let completed: Vec<String> = events
                .iter()
                .filter_map(|event| match event {
                    SentenceEvent::Completed(text) => Some(text.clone()),
                    SentenceEvent::Preliminary(_) => None,
                })
                .collect();
            let _ = dbg.log_buffer_decision(
                sentence_buffer
                    .last_decision()
                    .map_or("Started", |decision| decision.as_str()),
                line,
                &completed,
                sentence_buffer.buffered_text(),
            );
        }
        first_line = false;
        if !emit_sentence_events(events, shared, deps, semaphore, inflight) {
            return false;
        }
    }
    // Multi-line frame: the overlay shows every line, not just the last.
    // Each per-line emission above set `chinese_line` to that line in turn
    // (no repaint happens between them, so the UI never sees the
    // intermediate values); override with the joined lines now, and
    // register the group so the per-line translations land in line order.
    if lines.len() > 1 {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        state.display_group = Some(DisplayGroup::new(lines.clone()));
        state.snapshot.chinese_line = lines.join("\n");
        state.snapshot.vietnamese_line.clear();
        state.publish();
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
    // emits stable lines, the buffer assembles them into sentences. Its
    // re-read threshold tracks the dedupe threshold (merge fix,
    // 2026-11-25), so both agree on what counts as "the same subtitle".
    let mut sentence_buffer =
        SentenceBuffer::new(DEFAULT_SENTENCE_TIMEOUT, DEFAULT_SENTENCE_BLANK_FRAMES);
    sentence_buffer.set_similarity_threshold(initial.dedupe_threshold);
    // Consecutive frames skipped by the pixel-diff gate (merge fix,
    // 2026-11-25): logged as one aggregate line when OCR runs again, so
    // the debug log does not flood while the subtitle is static.
    let mut pixel_skip_run: u32 = 0;

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
            if let Some(dbg) = &deps.ocr_debug_logger {
                let _ = dbg.log_buffer_flush("timeout", &completed);
            }
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
                sentence_buffer.set_similarity_threshold(latest.dedupe_threshold);
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
                    pixel_skip_run = pixel_skip_run.saturating_add(1);
                    continue;
                }

                // The frame changed: report the skipped run (if any) as one
                // aggregate debug line, then reset the counter.
                if pixel_skip_run > 0 {
                    if let Some(dbg) = &deps.ocr_debug_logger {
                        let _ = dbg.log_pixel_diff_skipped(pixel_skip_run);
                    }
                    pixel_skip_run = 0;
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
                let (lines, geo_decisions) =
                    extract_chinese_subtitle_lines_with_decisions(&ocr_result.ocr_response);
                // Raw-OCR debug log (merge fix, 2026-11-25): one line per
                // OCR call with the raw blocks, before dedupe/hold/buffer,
                // so the next real run can prove whether a missed sentence
                // was an OCR miss (blocks=0), a filter drop (blocks but no
                // lines), or a buffer swallow (blocks + lines present).
                // Best-effort: a logging failure must never break the
                // pipeline. No line is written when OCR did not run at all
                // (pixel-diff skip), so the log stays silent while idle.
                if let Some(dbg) = &deps.ocr_debug_logger {
                    let _ = dbg.log_blocks(&ocr_result.ocr_response, &lines);
                    // Geometric split/join decisions (gap, threshold, boxes)
                    // so a later run can show why a line was split — or
                    // kept whole — by the OCR-box geometry.
                    for decision in &geo_decisions {
                        let _ = dbg.log_geometric_split(decision);
                    }
                }
                if lines.is_empty() {
                    // Blank / non-CJK frame: keep the dedupe blank counting
                    // and tell the SentenceBuffer the subtitle may have
                    // disappeared; a long enough blank run completes the
                    // buffered sentence.
                    let _ = deduper.process(None);
                    if let Some(completed) = sentence_buffer.note_blank(Instant::now()) {
                        if let Some(dbg) = &deps.ocr_debug_logger {
                            let _ = dbg.log_buffer_flush("blank", &completed);
                        }
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
                    // Dedupe dropped a re-read of the previous line: log
                    // it (with its similarity) so the debug log shows
                    // which lines the dedupe swallowed.
                    if let Some(dbg) = &deps.ocr_debug_logger {
                        let _ = dbg.log_dedupe_dropped(&joined, deduper.last_similarity());
                    }
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
                    if let Some(dbg) = &deps.ocr_debug_logger {
                        if let (Some(old), Some(new)) =
                            (hold.last_replaced(), hold.pending_text())
                        {
                            let _ = dbg.log_hold_replaced(old, new);
                        }
                    }
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
                // Multi-line display group (2-line bug fix): contribute
                // this line's translation; when every line of the frame
                // has one, show the joined translations in line order.
                // A grouped line still contributes even when its seq is
                // stale (it is older only because the frame's later lines
                // were emitted after it); the group, not the seq, decides
                // the display. A grouped line whose siblings are pending
                // shows nothing yet, so the overlay never flashes a
                // single line of a multi-line frame.
                match state.note_group_translation(&chinese, vi.clone()) {
                    GroupContribution::GroupedComplete(joined) => {
                        state.snapshot.vietnamese_line = joined.clone();
                        if is_completed {
                            // The group completed (all lines translated);
                            // record its joined pair for history. No
                            // still-on-screen guard: the group's Chinese
                            // was already shown at emit time, and the
                            // next subtitle may already have replaced
                            // `snapshot.chinese_line` by the time these
                            // translations resolve — gating on it dropped
                            // whole groups from history.
                            if let Some(group_chinese) = state
                                .display_group
                                .as_ref()
                                .map(DisplayGroup::joined_chinese)
                            {
                                state.record_completed_for_history(seq, group_chinese, joined);
                            }
                        }
                    }
                    GroupContribution::GroupedPending => {}
                    GroupContribution::NotGrouped => {
                        if !display_stale {
                            state.snapshot.vietnamese_line = vi.clone();
                        }
                        if is_completed {
                            // History records on translation success
                            // even when the display update was superseded:
                            // the sentence completed (translated +
                            // logged) and its Chinese was shown at emit
                            // time. The display update above stays gated.
                            state.record_completed_for_history(seq, chinese.clone(), vi.clone());
                        }
                    }
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
    // skips the synthesis call entirely. `tts_calls` counts real syntheses.
    let (cached_audio, tts_cache_hit) = match &deps.tts_cache {
        Some(cache) => match cache.get(&vietnamese, &voice, &rate) {
            Some(audio) => (Some(audio), true),
            None => (None, false),
        },
        None => (None, false),
    };

    let tts_res = match cached_audio {
        Some(audio) => Ok(TtsSynthesis {
            audio: audio.bytes,
            format: audio.format,
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

    let (audio_bytes, audio_format) = {
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
                // Store freshly synthesized audio in the TTS cache (A3),
                // in the engine's format (MP3 for Edge-TTS, WAV for Piper).
                if !tts_cache_hit {
                    if let Some(cache) = &deps.tts_cache {
                        let _ = cache.put(
                            &vietnamese,
                            &voice,
                            &rate,
                            CachedAudio::new(synthesis.audio.clone(), synthesis.format),
                        );
                    }
                }
                (synthesis.audio, synthesis.format)
            }
            Err(err) => {
                state.set_banner(PipelineErrorKind::Tts, format!("Lỗi giọng đọc TTS: {err}"));
                return;
            }
        }
    };

    if let Err(err) = deps.audio.enqueue_audio(audio_bytes, audio_format) {
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
        map_translate_error_banner, split_line_into_utterances, split_speakers, BufferDecision,
        DisplayGroup, Duration, GroupContribution, Instant, PipelineError, PipelineErrorBanner,
        PipelineErrorKind, PipelineSettings, PipelineSnapshot, SentenceBuffer, SentenceEvent,
        SharedPipelineState, SubtitleHold, SubtitlePair, DEFAULT_HOLD_DELAY,
        DEFAULT_SENTENCE_BLANK_FRAMES, ERR_BANNER_ENGINE_UNAVAILABLE, ERR_BANNER_TRANSLATE_NETWORK,
        MAX_SENTENCE_BUFFER_AGE,
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
        // A re-read that EXTENDS the buffered sentence (progressive
        // reveal) replaces the buffer with the longer reading, which then
        // completes on its punctuation.
        // (Merge fix, 2026-11-25: the old test pushed only the new part,
        // "早上见。", which the fix now treats as a new subtitle — a bare
        // continuation with no shared prefix is indistinguishable from a
        // hard cut to the next subtitle, so the buffer flushes first.
        // See `sentence_buffer_splits_merged_subtitles_from_real_log`.)
        assert_eq!(
            buf.push("明天早上见。".to_string(), t0 + Duration::from_millis(200)),
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
        // A later fragment with no shared prefix and low similarity is a
        // NEW subtitle (merge fix, 2026-11-25): the buffered fragment
        // completes on its own first instead of being appended to. (The
        // old expectation, Completed("你好，我是小明。"), was the
        // append-always behavior that merged consecutive subtitles.)
        assert_eq!(
            buf.push("小明。".to_string(), t0 + Duration::from_millis(2000)),
            vec![
                SentenceEvent::Completed("你好，我是".to_string()),
                SentenceEvent::Completed("小明。".to_string()),
            ]
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
        // (Merge fix, 2026-11-25: "，好吗" shares no prefix with "等一下"
        // and is not similar to it, so it is a new subtitle — the buffered
        // fragment completes first instead of being appended to.)
        assert_eq!(
            buf.push("，好吗".to_string(), t0),
            vec![
                SentenceEvent::Completed("等一下".to_string()),
                SentenceEvent::Preliminary("，好吗".to_string()),
            ]
        );
        assert_eq!(buf.note_blank(t0), None);
        assert_eq!(buf.note_blank(t0), Some("，好吗".to_string()));
    }

    #[test]
    fn sentence_buffer_replaces_on_progressive_reveal_and_similar_reread() {
        // Merge fix, 2026-11-25: the buffer's re-read threshold tracks
        // Danh's configured dedupe threshold (0.84), so these tests run
        // at the live value.
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        buf.set_similarity_threshold(0.84);

        // Progressive reveal: each re-read extends the buffer; the longer
        // reading replaces it, never appends.
        assert_eq!(
            buf.push("明天".to_string(), t0),
            vec![SentenceEvent::Preliminary("明天".to_string())]
        );
        assert_eq!(
            buf.push("明天早上见".to_string(), t0 + Duration::from_millis(200)),
            vec![SentenceEvent::Preliminary("明天早上见".to_string())]
        );
        assert_eq!(
            buf.push("明天早上见。".to_string(), t0 + Duration::from_millis(400)),
            vec![SentenceEvent::Completed("明天早上见。".to_string())]
        );
        assert!(buf.is_empty());

        // Truncated re-read: the buffer keeps the longer reading.
        assert_eq!(
            buf.push("你好，我是小明".to_string(), t0),
            vec![SentenceEvent::Preliminary("你好，我是小明".to_string())]
        );
        assert_eq!(
            buf.push("你好，我是".to_string(), t0 + Duration::from_millis(200)),
            vec![SentenceEvent::Preliminary("你好，我是小明".to_string())]
        );
        assert_eq!(buf.buffered_text(), "你好，我是小明");

        // Similar re-read (one OCR misread in 10 chars, similarity 0.90):
        // the freshest reading replaces the buffer.
        let mut buf2 =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        buf2.set_similarity_threshold(0.84);
        assert_eq!(
            buf2.push("今天晚上我们一起吃饭".to_string(), t0),
            vec![SentenceEvent::Preliminary(
                "今天晚上我们一起吃饭".to_string()
            )]
        );
        assert_eq!(
            buf2.push(
                "今天晚上我们一起吃反".to_string(),
                t0 + Duration::from_millis(200)
            ),
            vec![SentenceEvent::Preliminary(
                "今天晚上我们一起吃反".to_string()
            )]
        );
        assert_eq!(buf2.buffered_text(), "今天晚上我们一起吃反");
    }

    #[test]
    fn sentence_buffer_splits_merged_subtitles_from_real_log() {
        // Real merged lines from Danh's run, subtitles-2026-11-25.log
        // (merge fix, 2026-11-25): consecutive subtitles with no terminal
        // punctuation and no blank gap used to append into one long line.
        // Each case below must now come out as separate lines (or, for a
        // re-read, a single collapsed line) instead of one merged line.
        let t0 = Instant::now();
        let new_buf = || {
            let mut buf =
                SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
            buf.set_similarity_threshold(0.84);
            buf
        };

        // L14: two subtitles merged into one 30-char line.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("啊 下个月啊到金泽住一晚吗".to_string(), t0),
            vec![SentenceEvent::Preliminary(
                "啊 下个月啊到金泽住一晚吗".to_string()
            )]
        );
        assert_eq!(
            buf.push("哦 挺好的嘛".to_string(), t0 + Duration::from_millis(200)),
            vec![
                SentenceEvent::Completed("啊 下个月啊到金泽住一晚吗".to_string()),
                SentenceEvent::Preliminary("哦 挺好的嘛".to_string()),
            ]
        );
        assert_eq!(
            buf.push(
                "是 是呀虽然我也很想去 但有点难呢".to_string(),
                t0 + Duration::from_millis(400)
            ),
            vec![
                SentenceEvent::Completed("哦 挺好的嘛".to_string()),
                SentenceEvent::Preliminary("是 是呀虽然我也很想去 但有点难呢".to_string()),
            ]
        );

        // L73: a second subtitle appended to the first.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("美花小姐请让我再干你一次吧".to_string(), t0),
            vec![SentenceEvent::Preliminary(
                "美花小姐请让我再干你一次吧".to_string()
            )]
        );
        assert_eq!(
            buf.push(
                "请吧美花小姐 我喜欢你 我爱你".to_string(),
                t0 + Duration::from_millis(200)
            ),
            vec![
                SentenceEvent::Completed("美花小姐请让我再干你一次吧".to_string()),
                SentenceEvent::Preliminary("请吧美花小姐 我喜欢你 我爱你".to_string()),
            ]
        );

        // L116: the same sentence read twice (second read with a "--"
        // artifact) must collapse to one line, not append into a doubled
        // line. The exact on-screen mechanism for L116 is unconfirmed —
        // the OCR debug log will show the raw blocks on the next run —
        // but a normalized-identical re-read must never append.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("他很温柔 下次也让爸爸见见他".to_string(), t0),
            vec![SentenceEvent::Preliminary(
                "他很温柔 下次也让爸爸见见他".to_string()
            )]
        );
        assert_eq!(
            buf.push(
                "他很温柔--下次也让爸爸见见他".to_string(),
                t0 + Duration::from_millis(200)
            ),
            vec![SentenceEvent::Preliminary(
                "他很温柔--下次也让爸爸见见他".to_string()
            )]
        );
        assert_eq!(buf.buffered_text(), "他很温柔--下次也让爸爸见见他");

        // L119: the same (A + B) pair repeated; each card must complete
        // on its own instead of accumulating into one tripled line.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("一让你受苦了 对不起".to_string(), t0),
            vec![SentenceEvent::Preliminary(
                "一让你受苦了 对不起".to_string()
            )]
        );
        assert_eq!(
            buf.push("一太好了".to_string(), t0 + Duration::from_millis(200)),
            vec![
                SentenceEvent::Completed("一让你受苦了 对不起".to_string()),
                SentenceEvent::Preliminary("一太好了".to_string()),
            ]
        );
        assert_eq!(
            buf.push(
                "一让你受苦了 对不起".to_string(),
                t0 + Duration::from_millis(400)
            ),
            vec![
                SentenceEvent::Completed("一太好了".to_string()),
                SentenceEvent::Preliminary("一让你受苦了 对不起".to_string()),
            ]
        );

        // L126: the identical subtitle three times must stay one line.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("好厉害 智子 你又长大了不少嘛".to_string(), t0),
            vec![SentenceEvent::Preliminary(
                "好厉害 智子 你又长大了不少嘛".to_string()
            )]
        );
        assert_eq!(
            buf.push(
                "好厉害 智子 你又长大了不少嘛".to_string(),
                t0 + Duration::from_millis(200)
            ),
            vec![SentenceEvent::Preliminary(
                "好厉害 智子 你又长大了不少嘛".to_string()
            )]
        );
        assert_eq!(
            buf.push(
                "好厉害 智子 你又长大了不少嘛".to_string(),
                t0 + Duration::from_millis(400)
            ),
            vec![SentenceEvent::Preliminary(
                "好厉害 智子 你又长大了不少嘛".to_string()
            )]
        );
        assert_eq!(buf.buffered_text(), "好厉害 智子 你又长大了不少嘛");

        // 00:16:01: OCR misread variants BELOW the 0.84 threshold
        // (similarity 0.80) are no longer merged into one line — but they
        // still emit as separate lines. The threshold itself is unchanged
        // by this fix; a lower buffer threshold is a possible follow-up
        // if the repeats prove noisy in practice.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("要咆早饭吗".to_string(), t0),
            vec![SentenceEvent::Preliminary("要咆早饭吗".to_string())]
        );
        assert_eq!(
            buf.push("要唣早饭吗".to_string(), t0 + Duration::from_millis(200)),
            vec![
                SentenceEvent::Completed("要咆早饭吗".to_string()),
                SentenceEvent::Preliminary("要唣早饭吗".to_string()),
            ]
        );
        assert_eq!(
            buf.push("要唣且饭吗".to_string(), t0 + Duration::from_millis(400)),
            vec![
                SentenceEvent::Completed("要唣早饭吗".to_string()),
                SentenceEvent::Preliminary("要唣且饭吗".to_string()),
            ]
        );
    }

    #[test]
    fn sentence_buffer_icu4x_splits_closing_quote_ellipsis_and_ascii() {
        // UAX #29 cases the old 7-character punctuation list could not
        // express (merge fix, 2026-11-25 — ICU4X layer).
        let t0 = Instant::now();
        let new_buf =
            || SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);

        // Closing quote after terminal punctuation belongs to the
        // sentence: the old list stranded a lone `"` as a preliminary.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("他说\"你好。\"".to_string(), t0),
            vec![SentenceEvent::Completed("他说\"你好。\"".to_string())]
        );
        assert!(buf.is_empty());

        // A doubled ellipsis is one sentence, not two: the old list
        // drained at the first `…` and emitted a bare `…` completion.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("等一下……".to_string(), t0),
            vec![SentenceEvent::Completed("等一下……".to_string())]
        );
        assert!(buf.is_empty());

        // A decimal point is NOT a sentence break: the old list split
        // `3.5元` into Completed("3.") + Preliminary("5元").
        let mut buf = new_buf();
        assert_eq!(
            buf.push("3.5元".to_string(), t0),
            vec![SentenceEvent::Preliminary("3.5元".to_string())]
        );
        assert_eq!(buf.buffered_text(), "3.5元");

        // ASCII terminal punctuation still splits sentences.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("Hello. World".to_string(), t0),
            vec![
                SentenceEvent::Completed("Hello.".to_string()),
                SentenceEvent::Preliminary("World".to_string()),
            ]
        );
        assert_eq!(buf.buffered_text(), "World");

        // Three sentences in one fragment: every segment completes.
        let mut buf = new_buf();
        assert_eq!(
            buf.push("你好。我很好。你呢？".to_string(), t0),
            vec![
                SentenceEvent::Completed("你好。".to_string()),
                SentenceEvent::Completed("我很好。".to_string()),
                SentenceEvent::Completed("你呢？".to_string()),
            ]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn split_speakers_splits_two_speakers_and_keeps_guardrails() {
        // (a) Whitespace between two CJK clusters: a speaker change.
        assert_eq!(
            split_speakers("你先睡吧 不用等我吃饭了"),
            vec!["你先睡吧".to_string(), "不用等我吃饭了".to_string()]
        );
        // (b) Dash between two CJK clusters: ASCII, full-width, em dash.
        assert_eq!(
            split_speakers("你好吗 - 我很好"),
            vec!["你好吗".to_string(), "我很好".to_string()]
        );
        assert_eq!(
            split_speakers("你好吗－我很好"),
            vec!["你好吗".to_string(), "我很好".to_string()]
        );
        assert_eq!(
            split_speakers("你好吗—我很好"),
            vec!["你好吗".to_string(), "我很好".to_string()]
        );
        // A comma before the separator does not hide the speaker change.
        assert_eq!(
            split_speakers("你好吗，我很好 你先睡吧"),
            vec!["你好吗，我很好".to_string(), "你先睡吧".to_string()]
        );
        // Real log L116 (both variants): the space and the "--" artifact
        // are both speaker separators.
        assert_eq!(
            split_speakers("他很温柔 下次也让爸爸见见他"),
            vec!["他很温柔".to_string(), "下次也让爸爸见见他".to_string()]
        );
        assert_eq!(
            split_speakers("他很温柔--下次也让爸爸见见他"),
            vec!["他很温柔".to_string(), "下次也让爸爸见见他".to_string()]
        );

        // Guardrails: no split when a neighbour is not CJK (numbers,
        // dates, CJK next to Latin/digits) or when either adjacent CJK
        // cluster is shorter than 2 characters.
        assert_eq!(split_speakers("9 日"), vec!["9 日".to_string()]);
        assert_eq!(split_speakers("3.5 元"), vec!["3.5 元".to_string()]);
        assert_eq!(split_speakers("是 啊"), vec!["是 啊".to_string()]);
        assert_eq!(split_speakers("Hello 你好"), vec!["Hello 你好".to_string()]);
        assert_eq!(split_speakers("你好 123"), vec!["你好 123".to_string()]);
        assert_eq!(split_speakers("你好吗-我"), vec!["你好吗-我".to_string()]);
        assert_eq!(split_speakers("你好"), vec!["你好".to_string()]);
    }

    #[test]
    fn split_line_into_utterances_reports_the_blind_case() {
        // The blind case (merge fix, 2026-11-25): two sentences stuck
        // together with NO signal — no space, no dash, no punctuation.
        // Neither layer can split it; it stays one line. This test pins
        // that honest limit instead of hiding it.
        assert_eq!(
            split_line_into_utterances("大家就开心地享受吧好的好的那就再见了"),
            vec!["大家就开心地享受吧好的好的那就再见了".to_string()]
        );
        // Speaker layer splits these; the sentence layer splits this one.
        assert_eq!(
            split_line_into_utterances("你先睡吧 不用等我吃饭了"),
            vec!["你先睡吧".to_string(), "不用等我吃饭了".to_string()]
        );
        assert_eq!(
            split_line_into_utterances("你好吗 - 我很好。"),
            vec!["你好吗".to_string(), "我很好。".to_string()]
        );
        assert_eq!(
            split_line_into_utterances("你好。我很好"),
            vec!["你好。".to_string(), "我很好".to_string()]
        );
    }

    #[test]
    fn sentence_buffer_records_last_decision() {
        let t0 = Instant::now();
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);

        // Empty buffer: the fragment just starts a sentence.
        let _ = buf.push("明天".to_string(), t0);
        assert_eq!(buf.last_decision(), None);
        // Progressive reveal extends the buffer.
        let _ = buf.push("明天早上见。".to_string(), t0 + Duration::from_millis(100));
        assert_eq!(buf.last_decision(), Some(BufferDecision::Extended));

        // A re-read identical after normalization replaces the buffer.
        let _ = buf.push("你好吗".to_string(), t0 + Duration::from_millis(200));
        assert_eq!(buf.last_decision(), None);
        // An unrelated fragment is a new subtitle: flush first.
        let _ = buf.push("再见".to_string(), t0 + Duration::from_millis(300));
        assert_eq!(buf.last_decision(), Some(BufferDecision::NewSubtitle));
        // A re-read of the fresh buffer replaces it (same normalized text).
        let _ = buf.push("再见！".to_string(), t0 + Duration::from_millis(400));
        assert_eq!(buf.last_decision(), Some(BufferDecision::ReplacedSimilar));

        // A truncated re-read keeps the longer buffered reading.
        let mut buf =
            SentenceBuffer::new(Duration::from_millis(1200), DEFAULT_SENTENCE_BLANK_FRAMES);
        let _ = buf.push("明天早上见".to_string(), t0);
        let _ = buf.push("明天早上".to_string(), t0 + Duration::from_millis(100));
        assert_eq!(buf.last_decision(), Some(BufferDecision::KeptLonger));
        assert_eq!(buf.buffered_text(), "明天早上见");
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

    #[test]
    fn display_group_joins_two_line_translations_in_line_order() {
        // The 2-line bug: "你好。" resolves AFTER "再见。" (its
        // translation is slower), yet the display must show the lines
        // top-to-bottom, not in resolve order.
        let mut group = DisplayGroup::new(vec!["你好。".to_string(), "再见。".to_string()]);
        assert_eq!(
            group.note("再见。", "Tạm biệt.".to_string()),
            GroupContribution::GroupedPending
        );
        assert_eq!(
            group.note("你好。", "Chào.".to_string()),
            GroupContribution::GroupedComplete("Chào.\nTạm biệt.".to_string())
        );
    }

    #[test]
    fn display_group_ignores_lines_outside_the_group() {
        let mut group = DisplayGroup::new(vec!["你好。".to_string(), "再见。".to_string()]);
        assert_eq!(
            group.note("小明。", "Tiểu Minh.".to_string()),
            GroupContribution::NotGrouped
        );
        // A re-resolved line (Preliminary then Completed for the same
        // text) does not fill the entry twice.
        assert_eq!(
            group.note("你好。", "Chào.".to_string()),
            GroupContribution::GroupedPending
        );
        assert_eq!(
            group.note("你好。", "Chào.".to_string()),
            GroupContribution::NotGrouped
        );
        assert_eq!(
            group.note("再见。", "Tạm biệt.".to_string()),
            GroupContribution::GroupedComplete("Chào.\nTạm biệt.".to_string())
        );
    }

    #[test]
    fn display_group_fills_identical_twin_lines_in_order() {
        let mut group = DisplayGroup::new(vec!["你好。".to_string(), "你好。".to_string()]);
        assert_eq!(
            group.note("你好。", "Chào.".to_string()),
            GroupContribution::GroupedPending
        );
        assert_eq!(
            group.note("你好。", "Chào.".to_string()),
            GroupContribution::GroupedComplete("Chào.\nChào.".to_string())
        );
    }

    fn test_shared_state() -> SharedPipelineState {
        let mut cfg = AppConfig::default();
        cfg.general.region = Some(CaptureRegion {
            monitor_id: "DISPLAY1".to_string(),
            x: 100,
            y: 900,
            width: 800,
            height: 80,
        });
        let settings = PipelineSettings::from_app_config(&cfg)
            .expect("valid AppConfig converts to PipelineSettings");
        let (snapshot_tx, _snapshot_rx) = tokio::sync::watch::channel(PipelineSnapshot::default());
        SharedPipelineState {
            settings,
            snapshot: PipelineSnapshot::default(),
            snapshot_tx,
            stopped: false,
            display_group: None,
            last_completed_pair: None,
            last_completed_seq: 0,
            history: Vec::new(),
        }
    }

    fn pair(chinese: &str, vietnamese: &str) -> SubtitlePair {
        SubtitlePair {
            chinese: chinese.to_string(),
            vietnamese: vietnamese.to_string(),
        }
    }

    #[test]
    fn history_keeps_last_two_completed_pairs_most_recent_first() {
        let mut state = test_shared_state();
        assert!(state.history.is_empty());

        // A completes (seq 1): latest, nothing in history yet.
        state.record_completed_for_history(1, "第一句。".to_string(), "Câu thứ nhất.".to_string());
        assert!(state.history.is_empty());

        // B completes (seq 2): A moves into history.
        state.record_completed_for_history(2, "第二句。".to_string(), "Câu thứ hai.".to_string());
        assert_eq!(state.history, vec![pair("第一句。", "Câu thứ nhất.")]);
        assert_eq!(state.snapshot.history, state.history);

        // C completes (seq 3): B joins, A stays second.
        state.record_completed_for_history(3, "第三句。".to_string(), "Câu thứ ba.".to_string());
        assert_eq!(
            state.history,
            vec![
                pair("第二句。", "Câu thứ hai."),
                pair("第一句。", "Câu thứ nhất."),
            ]
        );

        // D completes (seq 4): the oldest (A) ages out (at most two kept).
        state.record_completed_for_history(4, "第四句。".to_string(), "Câu thứ tư.".to_string());
        assert_eq!(
            state.history,
            vec![
                pair("第三句。", "Câu thứ ba."),
                pair("第二句。", "Câu thứ hai."),
            ]
        );
    }

    #[test]
    fn history_ignores_same_text_repeat() {
        let mut state = test_shared_state();
        state.record_completed_for_history(1, "你好。".to_string(), "Chào.".to_string());
        // A repeated line must not push itself into history.
        state.record_completed_for_history(2, "你好。".to_string(), "Chào.".to_string());
        assert!(state.history.is_empty());
        assert_eq!(state.last_completed_pair, Some(pair("你好。", "Chào.")));
    }

    #[test]
    fn history_ignores_out_of_order_completion() {
        let mut state = test_shared_state();
        state.record_completed_for_history(1, "第一句。".to_string(), "Câu thứ nhất.".to_string());
        state.record_completed_for_history(3, "第三句。".to_string(), "Câu thứ ba.".to_string());
        assert_eq!(state.history, vec![pair("第一句。", "Câu thứ nhất.")]);
        // An older seq resolving late (e.g. a multi-line group whose
        // lines land after a newer single line) is inserted at the front
        // of history — newer than the pairs already there — while the
        // latest stays latest. It must not move history backwards.
        state.record_completed_for_history(2, "第二句。".to_string(), "Câu thứ hai.".to_string());
        assert_eq!(
            state.history,
            vec![
                pair("第二句。", "Câu thứ hai."),
                pair("第一句。", "Câu thứ nhất."),
            ]
        );
        assert_eq!(
            state.last_completed_pair,
            Some(pair("第三句。", "Câu thứ ba."))
        );
        assert_eq!(state.last_completed_seq, 3);
    }
}
