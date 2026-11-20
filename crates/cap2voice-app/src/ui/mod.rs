//! Desktop UI modules (`eframe` 0.35 shell, composite fonts, floating overlay).

pub mod fonts;
pub mod overlay;

use crate::api::{
    discover_server_dir, HttpTranslator, HttpTtsEngine, ServerProcess, ServerProcessConfig,
    ServerProcessError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_TRANSLATE_TIMEOUT, DEFAULT_TTS_TIMEOUT,
};
use crate::audio::{AudioPlayback, AudioPlayer};
use crate::capture::{FrameCapturer, XcapCapturer};
use crate::config::{
    default_config_path, load_config_from, save_config_to, AppConfig, WindowPosition,
};
use crate::ocr::{resolve_wechat_paths_from_config, OcrEngine, WeChatOcrAdapter};
use crate::pipeline::{
    PipelineDeps, PipelineHandle, PipelineRunner, PipelineSettings, PipelineSnapshot,
    PipelineStatus, SubtitleLogger,
};
use eframe::egui;
use overlay::{
    apply_exclude_from_capture_hwnd, overlay_overlaps_region, render_overlay_ui,
    DisplayAffinityError, OverlayCommand, OverlayState, PositionTracker, ServerUiStatus,
    DEFAULT_OVERLAY_SIZE, POSITION_SAVE_DEBOUNCE,
};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Build `eframe::NativeOptions` for the borderless, transparent, always-on-top overlay.
pub fn build_native_options(config: &AppConfig) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("cap2voice")
        .with_inner_size(DEFAULT_OVERLAY_SIZE)
        .with_min_inner_size([460.0, 140.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true);

    viewport = if config.display.always_on_top {
        viewport.with_window_level(egui::WindowLevel::AlwaysOnTop)
    } else {
        viewport.with_window_level(egui::WindowLevel::Normal)
    };

    if let Some(pos) = &config.display.overlay_position {
        viewport = viewport.with_position(egui::Pos2::new(pos.x, pos.y));
    }

    eframe::NativeOptions {
        viewport,
        ..Default::default()
    }
}

/// Format `ServerProcessError` into a clear, actionable Vietnamese banner for the user.
pub fn format_server_startup_error(err: &ServerProcessError) -> String {
    match err {
        ServerProcessError::ExitedEarly { stderr, .. }
            if stderr.contains("ModuleNotFoundError") || stderr.contains("No module named") =>
        {
            format!(
                "Không khởi động được server dịch/TTS local: {err} — hãy cài thư viện từ server/requirements.txt"
            )
        }
        _ => format!("Không khởi động được server dịch/TTS local: {err}"),
    }
}

fn resolve_server_directory() -> Result<PathBuf, ServerProcessError> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if let Ok(dir) = discover_server_dir(parent) {
                return Ok(dir);
            }
        }
    }
    let cwd = std::env::current_dir().map_err(|err| ServerProcessError::ServerDirNotFound {
        searched: format!("current_dir error: {err}"),
    })?;
    discover_server_dir(&cwd)
}

#[cfg(windows)]
fn apply_wda_to_handle(
    handle: &impl raw_window_handle::HasWindowHandle,
) -> Result<(), DisplayAffinityError> {
    use raw_window_handle::RawWindowHandle;
    let window_handle = handle
        .window_handle()
        .map_err(|_| DisplayAffinityError::MissingHwnd)?;
    match window_handle.as_raw() {
        RawWindowHandle::Win32(win32) => apply_exclude_from_capture_hwnd(win32.hwnd.get()),
        _ => Err(DisplayAffinityError::UnsupportedPlatform),
    }
}

#[cfg(not(windows))]
fn apply_wda_to_handle(
    _handle: &impl raw_window_handle::HasWindowHandle,
) -> Result<(), DisplayAffinityError> {
    Err(DisplayAffinityError::UnsupportedPlatform)
}

struct AppBootstrap {
    config_path: Option<PathBuf>,
    config: AppConfig,
    startup_warnings: Vec<String>,
}

fn load_bootstrap() -> AppBootstrap {
    let mut startup_warnings = Vec::new();
    let config_path = match default_config_path() {
        Ok(path) => Some(path),
        Err(err) => {
            startup_warnings.push(format!(
                "Không xác định được thư mục cấu hình (%APPDATA%): {err}"
            ));
            None
        }
    };

    let config = if let Some(path) = &config_path {
        match load_config_from(path) {
            Ok(outcome) => {
                if let Some(warn) = outcome.warning {
                    startup_warnings.push(format!(
                        "Tệp cấu hình bị hỏng (đã sao lưu tại {}): {}",
                        warn.backup_path.display(),
                        warn.reason
                    ));
                }
                outcome.config
            }
            Err(err) => {
                startup_warnings.push(format!("Lỗi đọc config.toml: {err}"));
                AppConfig::default()
            }
        }
    } else {
        AppConfig::default()
    };

    AppBootstrap {
        config_path,
        config,
        startup_warnings,
    }
}

/// Root `eframe` application shell coordinating server lifecycle, pipeline, and overlay UI.
pub struct Cap2VoiceApp {
    runtime: tokio::runtime::Runtime,
    config_path: Option<PathBuf>,
    config: AppConfig,
    state: OverlayState,
    font_warnings: Vec<String>,
    wda_attempted_frame: bool,
    wda_error_detail: Option<String>,
    position_tracker: PositionTracker,
    server_rx: Option<mpsc::Receiver<Result<ServerProcess, ServerProcessError>>>,
    server: Option<ServerProcess>,
    capturer: Arc<dyn FrameCapturer>,
    ocr: Option<Arc<dyn OcrEngine>>,
    audio: Option<Arc<dyn AudioPlayback>>,
    pipeline_handle: Option<PipelineHandle>,
    pipeline_rx: Option<watch::Receiver<PipelineSnapshot>>,
    repaint_forwarder: Option<tokio::task::JoinHandle<()>>,
}

impl Cap2VoiceApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        runtime: tokio::runtime::Runtime,
        bootstrap: AppBootstrap,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let font_report = fonts::install_windows_fonts(&cc.egui_ctx);

        let mut state = OverlayState::from_config(&bootstrap.config, bootstrap.startup_warnings);
        let mut wda_error_detail = None;
        match apply_wda_to_handle(cc) {
            Ok(()) => {
                state.exclude_from_capture_ok = true;
            }
            Err(err) => {
                wda_error_detail = Some(err.to_string());
            }
        }

        let (server_tx, server_rx) = mpsc::sync_channel(1);
        let port = bootstrap.config.translation.server_port;
        let ctx_for_server = cc.egui_ctx.clone();
        runtime.spawn(async move {
            let res = match resolve_server_directory() {
                Ok(server_dir) => {
                    ServerProcess::start(ServerProcessConfig::for_port(port, server_dir)).await
                }
                Err(err) => Err(err),
            };
            let _ = server_tx.send(res);
            ctx_for_server.request_repaint();
        });

        let position_tracker = PositionTracker::new(
            bootstrap.config.display.overlay_position.clone(),
            POSITION_SAVE_DEBOUNCE,
        );

        Self {
            runtime,
            config_path: bootstrap.config_path,
            config: bootstrap.config,
            state,
            font_warnings: font_report.warnings,
            wda_attempted_frame: false,
            wda_error_detail,
            position_tracker,
            server_rx: Some(server_rx),
            server: None,
            capturer: Arc::new(XcapCapturer::new()),
            ocr: None,
            audio: None,
            pipeline_handle: None,
            pipeline_rx: None,
            repaint_forwarder: None,
        }
    }

    fn poll_background_channels(&mut self) {
        if let Some(rx) = &self.server_rx {
            match rx.try_recv() {
                Ok(Ok(srv)) => {
                    self.server = Some(srv);
                    self.state.server_status = ServerUiStatus::Ready;
                    self.server_rx = None;
                }
                Ok(Err(err)) => {
                    self.state.server_status =
                        ServerUiStatus::Failed(format_server_startup_error(&err));
                    self.server_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.state.server_status = ServerUiStatus::Failed(
                        "Luồng khởi động server dịch/TTS đã dừng bất ngờ".to_string(),
                    );
                    self.server_rx = None;
                }
            }
        }

        let latest_snap = if let Some(rx) = &mut self.pipeline_rx {
            if rx.has_changed().unwrap_or(false) {
                Some(rx.borrow_and_update().clone())
            } else {
                None
            }
        } else {
            None
        };
        if let Some(snap) = latest_snap {
            let stopped_now = snap.status == PipelineStatus::Idle && self.pipeline_handle.is_none();
            self.state.apply_pipeline_snapshot(snap);
            if stopped_now {
                self.pipeline_rx = None;
            }
        }
    }

    fn persist_config(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        if let Err(err) = save_config_to(path, &self.config) {
            self.state.ui_notice =
                Some(format!("Không lưu được cấu hình {}: {err}", path.display()));
        }
    }

    fn save_overlay_position(&mut self, pos: WindowPosition) {
        self.config.display.overlay_position = Some(pos.clone());
        self.persist_config();
        self.position_tracker.mark_saved(pos);
    }

    fn handle_start_pipeline(&mut self, ctx: &egui::Context) {
        if !self
            .state
            .request_start(self.config.general.region.is_some())
        {
            return;
        }
        let Some(server) = &self.server else {
            self.state.ui_notice = Some("Server dịch/TTS chưa sẵn sàng".to_string());
            return;
        };

        let mut settings = match PipelineSettings::from_app_config(&self.config) {
            Ok(s) => s,
            Err(err) => {
                self.state.ui_notice = Some(err.to_string());
                return;
            }
        };
        settings.muted = self.state.muted;
        settings.voice = self.state.selected_voice.clone();

        if self.ocr.is_none() {
            match resolve_wechat_paths_from_config(&self.config.general) {
                Ok(paths) => {
                    self.ocr = Some(Arc::new(WeChatOcrAdapter::new(paths)));
                }
                Err(err) => {
                    self.state.ui_notice = Some(err.to_string());
                    return;
                }
            }
        }
        let Some(ocr) = self.ocr.clone() else {
            return;
        };

        if self.audio.is_none() {
            match AudioPlayer::new(self.config.voice.volume) {
                Ok(player) => {
                    self.audio = Some(Arc::new(player));
                }
                Err(err) => {
                    self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                    return;
                }
            }
        }
        let Some(audio) = self.audio.clone() else {
            return;
        };

        let base_url = server.base_url().to_string();
        let translator = match HttpTranslator::with_base_url(
            base_url.clone(),
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_TRANSLATE_TIMEOUT,
        ) {
            Ok(client) => Arc::new(client),
            Err(err) => {
                self.state.ui_notice = Some(err.to_string());
                return;
            }
        };
        let tts = match HttpTtsEngine::with_base_url(
            base_url,
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_TTS_TIMEOUT,
        ) {
            Ok(client) => Arc::new(client),
            Err(err) => {
                self.state.ui_notice = Some(err.to_string());
                return;
            }
        };

        let logger = if self.config.general.enable_subtitle_log {
            match self.config.effective_log_dir() {
                Ok(dir) => Some(Arc::new(SubtitleLogger::new(dir))),
                Err(err) => {
                    self.state.ui_notice = Some(format!("Không xác định được thư mục log: {err}"));
                    return;
                }
            }
        } else {
            None
        };

        let deps = PipelineDeps {
            capturer: Arc::clone(&self.capturer),
            ocr,
            translator,
            tts,
            audio,
            logger,
        };

        match PipelineRunner::start_on(self.runtime.handle(), settings, deps) {
            Ok(handle) => {
                let rx = handle.subscribe();
                self.state.apply_pipeline_snapshot(rx.borrow().clone());
                self.pipeline_rx = Some(rx.clone());
                self.pipeline_handle = Some(handle);
                self.start_repaint_forwarder(rx, ctx.clone());
            }
            Err(err) => {
                self.state.ui_notice = Some(err.to_string());
            }
        }
    }

    fn start_repaint_forwarder(
        &mut self,
        mut rx: watch::Receiver<PipelineSnapshot>,
        ctx: egui::Context,
    ) {
        if let Some(old) = self.repaint_forwarder.take() {
            old.abort();
        }
        let task = self.runtime.spawn(async move {
            while rx.changed().await.is_ok() {
                ctx.request_repaint();
            }
        });
        self.repaint_forwarder = Some(task);
    }

    fn handle_stop_pipeline(&mut self, ctx: &egui::Context) {
        if let Some(forwarder) = self.repaint_forwarder.take() {
            forwarder.abort();
        }
        if let Some(handle) = self.pipeline_handle.take() {
            self.state.pipeline.status = PipelineStatus::Idle;
            let ctx_wake = ctx.clone();
            self.runtime.spawn(async move {
                handle.stop().await;
                ctx_wake.request_repaint();
            });
        }
    }

    fn handle_toggle_mute(&mut self) {
        self.state.muted = !self.state.muted;
        if let Some(handle) = &self.pipeline_handle {
            if let Err(err) = handle.set_muted(self.state.muted) {
                self.state.ui_notice = Some(err.to_string());
            }
        } else if self.state.muted {
            if let Some(audio) = &self.audio {
                if let Err(err) = audio.stop_and_clear() {
                    self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                }
            }
        }
    }

    fn handle_select_voice(&mut self, voice: String) {
        self.state.selected_voice = voice.clone();
        self.config.voice.voice = voice.clone();
        self.persist_config();
        if let Some(handle) = &self.pipeline_handle {
            if let Err(err) = handle.set_voice(voice) {
                self.state.ui_notice = Some(err.to_string());
            }
        }
    }

    fn dispatch_commands(&mut self, ctx: &egui::Context, commands: Vec<OverlayCommand>) {
        for cmd in commands {
            match cmd {
                OverlayCommand::StartPipeline => self.handle_start_pipeline(ctx),
                OverlayCommand::StopPipeline => self.handle_stop_pipeline(ctx),
                OverlayCommand::ToggleMute => self.handle_toggle_mute(),
                OverlayCommand::SelectVoice(voice) => self.handle_select_voice(voice),
                OverlayCommand::ToggleRegionPanel => {
                    self.state.region_panel_open = !self.state.region_panel_open;
                }
                OverlayCommand::ToggleSettingsPanel => {
                    self.state.settings_panel_open = !self.state.settings_panel_open;
                }
                OverlayCommand::CloseWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn check_wda_fallback(&mut self, frame: &eframe::Frame, ui: &egui::Ui) {
        if !self.state.exclude_from_capture_ok && !self.wda_attempted_frame {
            self.wda_attempted_frame = true;
            match apply_wda_to_handle(frame) {
                Ok(()) => {
                    self.state.exclude_from_capture_ok = true;
                    self.wda_error_detail = None;
                }
                Err(err) => {
                    self.wda_error_detail = Some(err.to_string());
                }
            }
        }

        if !self.state.exclude_from_capture_ok {
            if let (Some(region), Some(outer_rect)) = (
                &self.config.general.region,
                ui.input(|i| i.viewport().outer_rect),
            ) {
                let pos = WindowPosition {
                    x: outer_rect.min.x,
                    y: outer_rect.min.y,
                };
                let size = [outer_rect.width(), outer_rect.height()];
                if overlay_overlaps_region(pos, size, ui.ctx().pixels_per_point(), region)
                    && self.state.ui_notice.is_none()
                {
                    let detail = self.wda_error_detail.as_deref().unwrap_or("không khả dụng");
                    self.state.ui_notice = Some(format!(
                        "Cảnh báo: WDA_EXCLUDEFROMCAPTURE {detail} và overlay đang đè lên vùng phụ đề."
                    ));
                }
            }
        }
    }
}

fn stop_pipeline_with_timeout(
    runtime: &tokio::runtime::Runtime,
    handle: PipelineHandle,
    timeout: Duration,
) -> bool {
    runtime.block_on(async move { tokio::time::timeout(timeout, handle.stop()).await.is_ok() })
}

impl eframe::App for Cap2VoiceApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.poll_background_channels();
        self.check_wda_fallback(frame, ui);

        if let Some(outer_rect) = ui.input(|i| i.viewport().outer_rect) {
            let now = Instant::now();
            self.position_tracker
                .observe(outer_rect.min.x, outer_rect.min.y, now);
            if let Some(ready_pos) = self.position_tracker.take_ready_to_save(now) {
                self.save_overlay_position(ready_pos);
            }
        }

        let commands = egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                render_overlay_ui(ui, &mut self.state, &self.config, &self.font_warnings)
            })
            .inner;

        let ctx = ui.ctx().clone();
        self.dispatch_commands(&ctx, commands);

        if matches!(self.state.server_status, ServerUiStatus::Starting) || self.state.is_running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(pending_pos) = self.position_tracker.take_pending() {
            self.save_overlay_position(pending_pos);
        }
        if let Some(forwarder) = self.repaint_forwarder.take() {
            forwarder.abort();
        }
        if let Some(handle) = self.pipeline_handle.take() {
            let _ = stop_pipeline_with_timeout(&self.runtime, handle, Duration::from_secs(2));
        }
        self.audio = None;
        self.server = None;
    }
}

/// Launch the `cap2voice` desktop application.
pub fn run_app() -> Result<(), eframe::Error> {
    let bootstrap = load_bootstrap();
    let options = build_native_options(&bootstrap.config);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("cap2voice-tokio")
        .build()
        .map_err(|err| eframe::Error::AppCreation(Box::new(err)))?;

    eframe::run_native(
        "cap2voice",
        options,
        Box::new(move |cc| Ok(Box::new(Cap2VoiceApp::new(cc, runtime, bootstrap)))),
    )
}

#[cfg(test)]
mod tests {
    use super::overlay::{OverlayState, ServerUiStatus};
    use super::{build_native_options, format_server_startup_error, stop_pipeline_with_timeout};
    use crate::api::{
        BoxFuture, HttpTranslator, HttpTtsEngine, ServerProcessError, TranslateError, Translator,
        TtsEngine, TtsError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_TRANSLATE_TIMEOUT,
        DEFAULT_TTS_TIMEOUT,
    };
    use crate::audio::{AudioError, AudioPlayback, EnqueueOutcome};
    use crate::capture::{CaptureError, CapturedFrame, FrameCapturer, MonitorInfo};
    use crate::config::{AppConfig, CaptureRegion, WindowPosition};
    use crate::ocr::{OcrAdapterError, OcrEngine, OcrResult, TextBlock};
    use crate::pipeline::{
        PipelineDeps, PipelineRunner, PipelineSettings, PipelineStatus, SubtitleLogger,
    };
    use eframe::egui::{Pos2, WindowLevel};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "cap2voice_ui_{label}_{}_{}_{}",
                std::process::id(),
                nanos,
                seq
            ));
            if let Err(err) = std::fs::create_dir_all(&dir) {
                panic!("failed creating temp test dir {}: {err}", dir.display());
            }
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TestCapturer {
        result: Result<CapturedFrame, CaptureError>,
    }

    impl FrameCapturer for TestCapturer {
        fn list_monitors(&self) -> Result<Vec<MonitorInfo>, CaptureError> {
            Ok(vec![MonitorInfo {
                id: r"\\.\DISPLAY1".to_string(),
                name: "Test Monitor".to_string(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 1.0,
                is_primary: true,
            }])
        }

        fn capture_region(&self, _region: &CaptureRegion) -> Result<CapturedFrame, CaptureError> {
            self.result.clone()
        }
    }

    struct TestOcr {
        text: String,
        calls: AtomicU64,
    }

    impl OcrEngine for TestOcr {
        fn recognize_frame(&self, _frame: &CapturedFrame) -> Result<OcrResult, OcrAdapterError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(OcrResult {
                errcode: 0,
                imgpath: "frame.png".to_string(),
                width: 64,
                height: 20,
                ocr_response: vec![TextBlock {
                    left: 0.0,
                    top: 0.0,
                    right: 64.0,
                    bottom: 20.0,
                    rate: 0.99,
                    text: self.text.clone(),
                    details: Vec::new(),
                }],
            })
        }
    }

    struct TestTranslator {
        translated: String,
    }

    impl Translator for TestTranslator {
        fn translate<'a>(
            &'a self,
            _chinese: &'a str,
        ) -> BoxFuture<'a, Result<String, TranslateError>> {
            let out = self.translated.clone();
            Box::pin(async move { Ok(out) })
        }
    }

    struct TestTts {
        calls: AtomicU64,
    }

    impl TtsEngine for TestTts {
        fn synthesize<'a>(
            &'a self,
            _text: &'a str,
            _voice: &'a str,
            _rate: &'a str,
        ) -> BoxFuture<'a, Result<Vec<u8>, TtsError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(vec![0x49, 0x44, 0x33, 0x01]) })
        }
    }

    struct TestAudio {
        volume: Mutex<f32>,
        enqueued: AtomicU64,
        stopped: AtomicU64,
    }

    impl TestAudio {
        fn new() -> Self {
            Self {
                volume: Mutex::new(0.75),
                enqueued: AtomicU64::new(0),
                stopped: AtomicU64::new(0),
            }
        }
    }

    impl AudioPlayback for TestAudio {
        fn enqueue_mp3(&self, _mp3_bytes: Vec<u8>) -> Result<EnqueueOutcome, AudioError> {
            self.enqueued.fetch_add(1, Ordering::Relaxed);
            Ok(EnqueueOutcome {
                pending_len: 1,
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
            self.stopped.fetch_add(1, Ordering::Relaxed);
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
            None
        }

        fn take_last_error(&self) -> Option<AudioError> {
            None
        }
    }

    fn build_test_runtime() -> tokio::runtime::Runtime {
        match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("cap2voice-ui-test")
            .build()
        {
            Ok(rt) => rt,
            Err(err) => panic!("failed building test tokio runtime: {err}"),
        }
    }

    fn test_rgba_frame(luma: u8) -> CapturedFrame {
        let w = 64u32;
        let h = 20u32;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[luma, luma, luma, 255]);
        }
        match CapturedFrame::new(w, h, rgba) {
            Ok(frame) => frame,
            Err(err) => panic!("failed creating test frame: {err}"),
        }
    }

    fn test_pipeline_settings(monitor_id: &str) -> PipelineSettings {
        PipelineSettings {
            region: CaptureRegion {
                monitor_id: monitor_id.to_string(),
                x: 0,
                y: 0,
                width: 64,
                height: 20,
            },
            capture_fps: 30,
            pixel_diff_threshold: 0.02,
            dedupe_threshold: 0.93,
            blank_reset_frames: 5,
            voice: "vi-VN-NamMinhNeural".to_string(),
            speech_rate: "+0%".to_string(),
            auto_read: true,
            muted: false,
            volume: 0.75,
            enable_subtitle_log: false,
            rate_limit_backoff: Duration::from_millis(20),
        }
    }

    #[test]
    fn build_native_options_configures_borderless_transparent_always_on_top_and_saved_position() {
        let mut cfg = AppConfig::default();
        cfg.display.always_on_top = true;
        cfg.display.overlay_position = Some(WindowPosition { x: 320.0, y: 740.0 });

        let opts = build_native_options(&cfg);
        assert_eq!(opts.viewport.decorations, Some(false));
        assert_eq!(opts.viewport.transparent, Some(true));
        assert_eq!(opts.viewport.window_level, Some(WindowLevel::AlwaysOnTop));
        assert_eq!(opts.viewport.position, Some(Pos2::new(320.0, 740.0)));

        cfg.display.always_on_top = false;
        cfg.display.overlay_position = None;
        let normal_opts = build_native_options(&cfg);
        assert_eq!(normal_opts.viewport.window_level, Some(WindowLevel::Normal));
        assert_eq!(normal_opts.viewport.position, None);
    }

    #[test]
    fn server_startup_error_formats_clear_vietnamese_diagnostics() {
        let py_err = format_server_startup_error(&ServerProcessError::PythonNotFound {
            searched: "PATH".to_string(),
        });
        assert!(py_err.contains("Python"));
        assert!(py_err.contains("requirements.txt"));

        let early_err = format_server_startup_error(&ServerProcessError::ExitedEarly {
            exit_code: Some(1),
            stderr: "ModuleNotFoundError: No module named 'fastapi'".to_string(),
        });
        assert!(early_err.contains("fastapi"));
        assert!(early_err.contains("requirements.txt"));
    }

    #[test]
    fn stop_pipeline_with_timeout_succeeds_from_non_tokio_ui_thread_without_panic() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "test must run on a plain non-Tokio UI thread"
        );

        let runtime = build_test_runtime();
        let deps = PipelineDeps {
            capturer: Arc::new(TestCapturer {
                result: Ok(test_rgba_frame(200)),
            }),
            ocr: Arc::new(TestOcr {
                text: "夜深了，雨还在下。".to_string(),
                calls: AtomicU64::new(0),
            }),
            translator: Arc::new(TestTranslator {
                translated: "Đêm đã khuya, mưa vẫn còn rơi.".to_string(),
            }),
            tts: Arc::new(TestTts {
                calls: AtomicU64::new(0),
            }),
            audio: Arc::new(TestAudio::new()),
            logger: None,
        };

        let handle = match PipelineRunner::start_on(
            runtime.handle(),
            test_pipeline_settings(r"\\.\DISPLAY1"),
            deps,
        ) {
            Ok(h) => h,
            Err(err) => panic!("expected PipelineRunner::start_on to succeed: {err}"),
        };

        let mut rx = handle.subscribe();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if rx.has_changed().unwrap_or(false) {
                let snap = rx.borrow_and_update().clone();
                if snap.metrics.frames_captured >= 1 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            rx.borrow().metrics.frames_captured >= 1,
            "pipeline should capture at least one frame on background runtime"
        );

        let stopped = stop_pipeline_with_timeout(&runtime, handle, Duration::from_secs(2));
        assert!(
            stopped,
            "stop_pipeline_with_timeout should complete within timeout"
        );
        assert_eq!(rx.borrow().status, PipelineStatus::Idle);
    }

    #[test]
    fn ui_thread_start_on_runs_capture_ocr_translate_audio_and_stops_cleanly() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "test must run on a plain non-Tokio UI thread"
        );

        let translator_http = HttpTranslator::with_base_url(
            "http://127.0.0.1:18765",
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_TRANSLATE_TIMEOUT,
        );
        assert!(
            translator_http.is_ok(),
            "HttpTranslator client init should work outside Tokio runtime: {translator_http:?}"
        );
        let tts_http = HttpTtsEngine::with_base_url(
            "http://127.0.0.1:18765",
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_TTS_TIMEOUT,
        );
        assert!(
            tts_http.is_ok(),
            "HttpTtsEngine client init should work outside Tokio runtime: {tts_http:?}"
        );

        let temp_dir = TestTempDir::new("ui_start_on");
        let runtime = build_test_runtime();
        let ocr = Arc::new(TestOcr {
            text: "夜深了，雨还在下。".to_string(),
            calls: AtomicU64::new(0),
        });
        let tts = Arc::new(TestTts {
            calls: AtomicU64::new(0),
        });
        let audio = Arc::new(TestAudio::new());

        let mut settings = test_pipeline_settings(r"\\.\DISPLAY1");
        settings.enable_subtitle_log = true;

        let deps = PipelineDeps {
            capturer: Arc::new(TestCapturer {
                result: Ok(test_rgba_frame(200)),
            }),
            ocr: ocr.clone(),
            translator: Arc::new(TestTranslator {
                translated: "Đêm đã khuya, mưa vẫn còn rơi.".to_string(),
            }),
            tts: tts.clone(),
            audio: audio.clone(),
            logger: Some(Arc::new(SubtitleLogger::new(temp_dir.path().to_path_buf()))),
        };

        let handle = match PipelineRunner::start_on(runtime.handle(), settings, deps) {
            Ok(h) => h,
            Err(err) => panic!("expected PipelineRunner::start_on to succeed: {err}"),
        };

        let mut cfg = AppConfig::default();
        cfg.general.region = Some(CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 0,
            y: 0,
            width: 64,
            height: 20,
        });
        let mut overlay_state = OverlayState::from_config(&cfg, Vec::new());
        overlay_state.server_status = ServerUiStatus::Ready;
        assert!(overlay_state.request_start(true));

        let mut rx = handle.subscribe();
        overlay_state.apply_pipeline_snapshot(rx.borrow().clone());

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if rx.has_changed().unwrap_or(false) {
                overlay_state.apply_pipeline_snapshot(rx.borrow_and_update().clone());
                if overlay_state.pipeline.vietnamese_line == "Đêm đã khuya, mưa vẫn còn rơi."
                    && overlay_state.pipeline.metrics.logged_pairs >= 1
                    && audio.enqueued.load(Ordering::Relaxed) >= 1
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(overlay_state.pipeline.chinese_line, "夜深了，雨还在下。");
        assert_eq!(
            overlay_state.pipeline.vietnamese_line,
            "Đêm đã khuya, mưa vẫn còn rơi."
        );
        assert_eq!(overlay_state.active_error_banner(), None);
        assert!(overlay_state.pipeline.metrics.frames_captured >= 1);
        assert!(overlay_state.pipeline.metrics.ocr_calls >= 1);
        assert!(ocr.calls.load(Ordering::Relaxed) >= 1);
        assert!(tts.calls.load(Ordering::Relaxed) >= 1);
        assert!(audio.enqueued.load(Ordering::Relaxed) >= 1);
        assert!(overlay_state.pipeline.metrics.logged_pairs >= 1);

        assert_eq!(handle.set_muted(true), Ok(()));
        assert_eq!(handle.set_voice("vi-VN-HoaiMyNeural"), Ok(()));
        assert!(audio.stopped.load(Ordering::Relaxed) >= 1);

        let stopped = stop_pipeline_with_timeout(&runtime, handle, Duration::from_secs(2));
        assert!(
            stopped,
            "stop_pipeline_with_timeout should stop pipeline cleanly"
        );
        assert_eq!(rx.borrow().status, PipelineStatus::Idle);
    }

    #[test]
    fn ui_thread_start_on_surfaces_monitor_unavailable_banner_without_calling_ocr() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "test must run on a plain non-Tokio UI thread"
        );

        let runtime = build_test_runtime();
        let ocr = Arc::new(TestOcr {
            text: "夜深了，雨还在下。".to_string(),
            calls: AtomicU64::new(0),
        });
        let audio = Arc::new(TestAudio::new());
        let deps = PipelineDeps {
            capturer: Arc::new(TestCapturer {
                result: Err(CaptureError::MonitorUnavailable {
                    monitor_id: "DISPLAY1".to_string(),
                }),
            }),
            ocr: ocr.clone(),
            translator: Arc::new(TestTranslator {
                translated: "Đêm đã khuya, mưa vẫn còn rơi.".to_string(),
            }),
            tts: Arc::new(TestTts {
                calls: AtomicU64::new(0),
            }),
            audio,
            logger: None,
        };

        let handle = match PipelineRunner::start_on(
            runtime.handle(),
            test_pipeline_settings("DISPLAY1"),
            deps,
        ) {
            Ok(h) => h,
            Err(err) => panic!("expected PipelineRunner::start_on to succeed: {err}"),
        };

        let mut cfg = AppConfig::default();
        cfg.general.region = Some(CaptureRegion {
            monitor_id: "DISPLAY1".to_string(),
            x: 0,
            y: 0,
            width: 64,
            height: 20,
        });
        let mut overlay_state = OverlayState::from_config(&cfg, Vec::new());
        overlay_state.server_status = ServerUiStatus::Ready;
        let mut rx = handle.subscribe();
        overlay_state.apply_pipeline_snapshot(rx.borrow().clone());

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if rx.has_changed().unwrap_or(false) {
                overlay_state.apply_pipeline_snapshot(rx.borrow_and_update().clone());
                if overlay_state.pipeline.error_banner.is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(overlay_state.pipeline.status, PipelineStatus::Running);
        let banner = overlay_state.active_error_banner();
        assert!(
            banner.as_deref().is_some_and(
                |msg| msg.contains("không tìm thấy màn hình") && msg.contains("DISPLAY1")
            ),
            "expected explicit Vietnamese MonitorUnavailable banner, got {banner:?}"
        );
        assert_eq!(ocr.calls.load(Ordering::Relaxed), 0);

        let stopped = stop_pipeline_with_timeout(&runtime, handle, Duration::from_secs(2));
        assert!(
            stopped,
            "stop_pipeline_with_timeout should stop pipeline cleanly"
        );
        assert_eq!(rx.borrow().status, PipelineStatus::Idle);
    }
}
