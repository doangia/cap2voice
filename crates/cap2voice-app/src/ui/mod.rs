//! Desktop UI modules (`eframe` 0.35 shell, composite fonts, floating overlay).

pub mod bootstrap;
pub mod commands;
pub mod fonts;
pub mod hotkeys;
pub mod icon;
pub mod icons;
pub mod inplace;
pub mod overlay;
pub mod pipeline;
pub mod region;
pub mod settings;
pub mod tray;

use crate::api::TtsError;
use crate::audio::AudioPlayback;
use crate::capture::{FrameCapturer, XcapCapturer};
use crate::config::{AppConfig, CaptureRegion, WindowPosition};
use crate::ocr::OcrEngine;
use crate::pipeline::{PipelineHandle, PipelineSnapshot, TranslationCache, TtsCache};
use crate::translate::SwitchableTranslator;
use crate::tts::EdgeTtsEngine;
pub(crate) use bootstrap::{
    apply_wda_to_handle, build_native_options, build_native_translator, load_bootstrap,
    stop_pipeline_with_timeout, AppBootstrap,
};
pub(crate) use commands::RegionSelectionTarget;
use eframe::egui;
use global_hotkey::GlobalHotKeyEvent;
use hotkeys::HotkeyManager;
use inplace::{
    build_inplace_viewport_builder, inplace_viewport_id, region_logical_rect_with_monitors,
    render_inplace_subtitle_ui, INPLACE_WINDOW_TITLE,
};
use overlay::{
    apply_exclude_from_capture_by_title, clamp_overlay_resize, overlay_desired_height,
    render_overlay_ui, OverlayState, PositionTracker, ServerUiStatus, POSITION_SAVE_DEBOUNCE,
};
pub(crate) use pipeline::TtsPreviewAudio;
#[cfg(test)]
pub(crate) use pipeline::{
    apply_region_selection_outcome, commit_settings_to_state, SettingsCommitEffects,
};
use region::{
    build_region_viewport_builder, region_viewport_id, render_region_selector_ui,
    RegionSelectorOutcome, RegionSelectorState,
};
use settings::{
    build_settings_viewport_builder, render_settings_ui, settings_viewport_id, SettingsDraftState,
};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tray::{should_cancel_close, tray_action_for_menu_event, TrayIconHandle, TrayMenuAction};
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconEvent;

/// Payload delivered by the background translator loader thread: the
/// built translator (`None` when no engine is available) plus
/// user-visible warnings about missing dictionaries/models.
type TranslatorLoadResult = (Option<Arc<SwitchableTranslator>>, Vec<String>);

/// Root `eframe` application shell coordinating native engine lifecycle, pipeline, and overlay UI.
pub struct Cap2VoiceApp {
    runtime: tokio::runtime::Runtime,
    config_path: Option<PathBuf>,
    config: AppConfig,
    state: OverlayState,
    font_warnings: Vec<String>,
    wda_attempted_frame: bool,
    wda_error_detail: Option<String>,
    position_tracker: PositionTracker,
    last_overlay_outer_pos: Option<WindowPosition>,
    region_selector: Option<RegionSelectorState>,
    region_selector_focused: bool,
    region_selection_target: RegionSelectionTarget,
    /// Capture region the cached in-place viewport rect was computed
    /// from; a region change (re-select) recomputes the rect so the
    /// text window follows the new region.
    inplace_rect_source: Option<CaptureRegion>,
    /// Cached logical (position, size) of the in-place subtitle
    /// viewport, equal to the capture region in logical coordinates.
    inplace_viewport_rect: Option<(egui::Pos2, egui::Vec2)>,
    /// Whether `WDA_EXCLUDEFROMCAPTURE` was already attempted on the
    /// in-place subtitle window (once per app run).
    inplace_wda_attempted: bool,
    /// Height (logical px) this app last set automatically for multi-line
    /// subtitles (2-line bug fix); `None` when the height is the user's
    /// own (manual resize or default). Used to shrink back after a
    /// multi-line frame without undoing a manual resize.
    overlay_auto_height: Option<f32>,
    /// WDA failure detail for the in-place window (shown as a notice).
    inplace_wda_error: Option<String>,
    settings_draft: Option<SettingsDraftState>,
    settings_focused: bool,
    tts_preview_rx: Option<mpsc::Receiver<Result<TtsPreviewAudio, TtsError>>>,
    /// In-flight Chinese click-to-replay synthesis (D2), same payload shape
    /// as the voice preview channel but reported via `ui_notice` instead of
    /// the settings draft.
    chinese_replay_rx: Option<mpsc::Receiver<Result<TtsPreviewAudio, TtsError>>>,
    /// Shared native translation engine (D1+A5). Built on a background
    /// thread at startup because Vietphrase dictionary load takes seconds;
    /// `None` until the loader thread delivers it (see `translator_rx`).
    translator: Option<Arc<SwitchableTranslator>>,
    /// Loader-thread channel for the native translator (see above). Carries
    /// the built translator (or `None` when no engine is available) plus
    /// user-visible warnings about missing dictionaries/models.
    translator_rx: Option<mpsc::Receiver<TranslatorLoadResult>>,
    /// Set when the translator loader thread reports that no engine is
    /// available (or dies): shown as the overlay engine status. `None`
    /// while the loader is still running.
    translator_error: Option<String>,
    /// Native Edge-TTS client (D1): stateless, always available.
    tts_engine: Arc<EdgeTtsEngine>,
    capturer: Arc<dyn FrameCapturer>,
    ocr: Option<Arc<dyn OcrEngine>>,
    audio: Option<Arc<dyn AudioPlayback>>,
    /// Translation + TTS caches (A3 / gh #2), shared with the pipeline.
    /// Created lazily from `config.effective_cache_dir()` so the Settings
    /// "clear cache" action works even before the pipeline ever started.
    translation_cache: Option<Arc<TranslationCache>>,
    tts_cache: Option<Arc<TtsCache>>,
    pipeline_handle: Option<PipelineHandle>,
    pipeline_rx: Option<watch::Receiver<PipelineSnapshot>>,
    repaint_forwarder: Option<tokio::task::JoinHandle<()>>,
    tray: Option<TrayIconHandle>,
    hotkeys: Option<HotkeyManager>,
    tray_action_rx: Option<mpsc::Receiver<TrayMenuAction>>,
    hotkey_event_rx: Option<mpsc::Receiver<GlobalHotKeyEvent>>,
    quit_requested: bool,
    overlay_visible: bool,
    last_tray_running_state: Option<bool>,
}

impl Cap2VoiceApp {
    pub(crate) fn new(
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

        // Build the native translator on a background thread (D1+A5):
        // loading the Vietphrase dictionaries takes ~7 s on the real PC,
        // so it must never block the first overlay paint. The loader sends
        // the built translator (or `None`) plus warnings; the overlay
        // header shows "Đang tải model dịch…" meanwhile (see
        // `poll_background_channels`).
        let (translator_tx, translator_rx) = mpsc::sync_channel::<TranslatorLoadResult>(1);
        let translator_config = bootstrap.config.clone();
        let ctx_wake = cc.egui_ctx.clone();
        let translator_spawn = std::thread::Builder::new()
            .name("cap2voice-translator-load".to_string())
            .spawn(move || {
                let (translator, warnings) = build_native_translator(&translator_config);
                let _ = translator_tx.send((translator, warnings));
                ctx_wake.request_repaint();
            });
        let mut translator_error = None;
        let translator_rx = match translator_spawn {
            Ok(_) => Some(translator_rx),
            Err(err) => {
                translator_error = Some(format!(
                    "Không tạo được luồng tải engine dịch: {err} — kiểm tra model/từ điển"
                ));
                None
            }
        };

        let position_tracker = PositionTracker::new(
            bootstrap.config.display.overlay_position.clone(),
            POSITION_SAVE_DEBOUNCE,
        );
        let initial_outer_pos = bootstrap.config.display.overlay_position.clone();

        let mut tray_startup_notices: Vec<String> = Vec::new();
        let tray = match TrayIconHandle::new() {
            Ok(handle) => Some(handle),
            Err(err) => {
                tray_startup_notices
                    .push(format!("Không tạo được biểu tượng khay hệ thống: {err}"));
                None
            }
        };
        let hotkeys = match HotkeyManager::register_all(&bootstrap.config.hotkeys) {
            Ok(manager) => Some(manager),
            Err(err) => {
                tray_startup_notices.push(format!("Không đăng ký được phím tắt: {err}"));
                None
            }
        };
        if !tray_startup_notices.is_empty() {
            state.ui_notice = Some(tray_startup_notices.join(" "));
        }

        let (tray_action_tx, tray_action_rx) = mpsc::channel::<TrayMenuAction>();
        let (hotkey_event_tx, hotkey_event_rx) = mpsc::channel::<GlobalHotKeyEvent>();
        let ctx_for_threads = cc.egui_ctx.clone();
        let mut forwarder_errors: Vec<String> = Vec::new();
        let menu_spawn = std::thread::Builder::new()
            .name("cap2voice-tray-menu".to_string())
            .spawn({
                let tx = tray_action_tx.clone();
                let ctx_wake = ctx_for_threads.clone();
                move || {
                    for event in MenuEvent::receiver().iter() {
                        if let Some(action) = tray_action_for_menu_event(&event) {
                            if tx.send(action).is_err() {
                                break;
                            }
                            ctx_wake.request_repaint();
                        }
                    }
                }
            });
        if let Err(err) = menu_spawn {
            forwarder_errors.push(format!("luồng menu khay hệ thống: {err}"));
        }
        let click_spawn = std::thread::Builder::new()
            .name("cap2voice-tray-click".to_string())
            .spawn({
                let tx = tray_action_tx;
                let ctx_wake = ctx_for_threads.clone();
                move || {
                    for event in TrayIconEvent::receiver().iter() {
                        let restore = match &event {
                            TrayIconEvent::Click {
                                button,
                                button_state,
                                ..
                            } => {
                                *button == tray_icon::MouseButton::Left
                                    && *button_state == tray_icon::MouseButtonState::Up
                            }
                            TrayIconEvent::DoubleClick { .. } => true,
                            _ => false,
                        };
                        if restore {
                            if tx.send(TrayMenuAction::ShowWindow).is_err() {
                                break;
                            }
                            ctx_wake.request_repaint();
                        }
                    }
                }
            });
        if let Err(err) = click_spawn {
            forwarder_errors.push(format!("luồng click khay hệ thống: {err}"));
        }
        let hotkey_spawn = std::thread::Builder::new()
            .name("cap2voice-hotkeys".to_string())
            .spawn({
                let tx = hotkey_event_tx;
                let ctx_wake = ctx_for_threads;
                move || {
                    for event in GlobalHotKeyEvent::receiver().iter() {
                        if tx.send(event).is_err() {
                            break;
                        }
                        ctx_wake.request_repaint();
                    }
                }
            });
        if let Err(err) = hotkey_spawn {
            forwarder_errors.push(format!("luồng phím tắt: {err}"));
        }
        if !forwarder_errors.is_empty() {
            let detail = forwarder_errors.join("; ");
            state.ui_notice = Some(match &state.ui_notice {
                Some(existing) => format!("{existing} Không khởi động được {detail}."),
                None => format!("Không khởi động được {detail}."),
            });
        }
        let start_minimized_to_tray = bootstrap.config.general.start_minimized_to_tray;

        Self {
            runtime,
            config_path: bootstrap.config_path,
            config: bootstrap.config,
            state,
            font_warnings: font_report.warnings,
            wda_attempted_frame: false,
            wda_error_detail,
            position_tracker,
            last_overlay_outer_pos: initial_outer_pos,
            region_selector: None,
            region_selector_focused: false,
            region_selection_target: RegionSelectionTarget::LiveConfig,
            inplace_rect_source: None,
            inplace_viewport_rect: None,
            inplace_wda_attempted: false,
            overlay_auto_height: None,
            inplace_wda_error: None,
            settings_draft: None,
            settings_focused: false,
            tts_preview_rx: None,
            chinese_replay_rx: None,
            translator: None,
            translator_rx,
            translator_error,
            tts_engine: Arc::new(EdgeTtsEngine::new()),
            capturer: Arc::new(XcapCapturer::new()),
            ocr: None,
            audio: None,
            translation_cache: None,
            tts_cache: None,
            pipeline_handle: None,
            pipeline_rx: None,
            repaint_forwarder: None,
            tray,
            hotkeys,
            tray_action_rx: Some(tray_action_rx),
            hotkey_event_rx: Some(hotkey_event_rx),
            quit_requested: false,
            overlay_visible: !start_minimized_to_tray,
            last_tray_running_state: None,
        }
    }

    /// Logical (position, size) of the in-place subtitle viewport for
    /// the current capture region, recomputed when the region changes
    /// (re-select) so the text window follows it. Returns `None` when
    /// no region is set. The `bool` is `true` when the rect was just
    /// recomputed (the caller then moves/resizes the live viewport).
    fn refresh_inplace_rect(
        &mut self,
        ctx: &egui::Context,
    ) -> Option<((egui::Pos2, egui::Vec2), bool)> {
        let region = self.config.general.region.clone()?;
        if self.inplace_rect_source.as_ref() == Some(&region) {
            return self.inplace_viewport_rect.map(|rect| (rect, false));
        }
        let fallback = ctx.pixels_per_point();
        let monitors = self.capturer.list_monitors().unwrap_or_default();
        let rect = region_logical_rect_with_monitors(&region, &monitors, fallback);
        self.inplace_rect_source = Some(region);
        self.inplace_viewport_rect = Some(rect);
        Some((rect, true))
    }

    /// Grow/shrink the overlay window height so multi-line subtitles fit
    /// (2-line bug fix). The default 204 px window fits one Chinese + one
    /// Vietnamese line; a 2-line frame needs ~55 px more. Policy: grow
    /// when the content needs more height than the window has; shrink
    /// back only when the window is still at a height this method set
    /// (never undo a manual resize, except to keep content visible).
    fn apply_multiline_height(&mut self, ctx: &egui::Context) {
        let desired = overlay_desired_height(
            &self.config,
            &self.state.pipeline.chinese_line,
            &self.state.pipeline.vietnamese_line,
        );
        let Some(size) = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size())) else {
            return;
        };
        let current = size.y;
        let at_auto_height = self
            .overlay_auto_height
            .is_some_and(|h| (current - h).abs() < 1.0);
        let should_resize =
            current + 0.5 < desired || (at_auto_height && (current - desired).abs() > 0.5);
        if should_resize {
            let next = clamp_overlay_resize([size.x, desired]);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(next.into()));
            self.overlay_auto_height = Some(next[1]);
        } else if !at_auto_height && current + 0.5 >= desired {
            // The user resized away from the auto height: stop tracking it.
            self.overlay_auto_height = None;
        }
    }
}

impl eframe::App for Cap2VoiceApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ui.input(|i| i.viewport().close_requested())
            && should_cancel_close(self.config.general.close_to_tray, self.quit_requested)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_overlay(&ctx);
        }
        self.poll_background_channels(&ctx);
        self.sync_tray_menu_state();
        self.check_wda_fallback(frame, ui);

        if self.region_selector.is_none() {
            if let Some(outer_rect) = ui.input(|i| i.viewport().outer_rect) {
                let outer_pos = WindowPosition {
                    x: outer_rect.min.x,
                    y: outer_rect.min.y,
                };
                self.last_overlay_outer_pos = Some(outer_pos);
                let now = Instant::now();
                self.position_tracker
                    .observe(outer_rect.min.x, outer_rect.min.y, now);
                if let Some(ready_pos) = self.position_tracker.take_ready_to_save(now) {
                    self.save_overlay_position(ready_pos);
                }
            }
        }

        if let Some(monitor) = self
            .region_selector
            .as_ref()
            .and_then(RegionSelectorState::active_monitor)
            .cloned()
        {
            // The in-place subtitle window is not shown while selecting
            // a region (immediate viewport closes); re-apply WDA next
            // time it opens, since that is a new HWND.
            self.inplace_wda_attempted = false;
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |_| {});

            let builder = build_region_viewport_builder(&monitor);
            let outcome = ctx.show_viewport_immediate(
                region_viewport_id(),
                builder,
                |viewport_ui, _class| {
                    if !self.region_selector_focused {
                        self.region_selector_focused = true;
                        viewport_ui
                            .ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(viewport_ui, |inner_ui| {
                            if let Some(selector) = &mut self.region_selector {
                                render_region_selector_ui(inner_ui, selector)
                            } else {
                                RegionSelectorOutcome::Pending
                            }
                        })
                        .inner
                },
            );
            if !matches!(outcome, RegionSelectorOutcome::Pending) {
                self.finish_region_selection(&ctx, outcome);
            }
        } else {
            if self.settings_draft.is_some() {
                let settings_cmd = ctx.show_viewport_immediate(
                    settings_viewport_id(),
                    build_settings_viewport_builder(),
                    |viewport_ui, _class| {
                        if !self.settings_focused {
                            self.settings_focused = true;
                            viewport_ui
                                .ctx()
                                .send_viewport_cmd(egui::ViewportCommand::Focus);
                        }
                        if let Some(draft) = &mut self.settings_draft {
                            render_settings_ui(viewport_ui, draft)
                        } else {
                            None
                        }
                    },
                );
                if let Some(cmd) = settings_cmd {
                    self.handle_settings_command(&ctx, cmd);
                }
            }

            // In-place subtitle window (ADDITIONAL, alongside the
            // floating overlay, which stays visible and usable): a
            // separate transparent window exactly over the capture
            // region, showing only the translated subtitle text on its
            // adjustable-opacity background. Hidden while the region
            // selector is open (we are in the `else` branch here) and
            // whenever the mode is off.
            let mut inplace_shown = false;
            if self.config.display.inplace_subtitle {
                if let Some(((pos, size), rect_changed)) = self.refresh_inplace_rect(&ctx) {
                    inplace_shown = true;
                    ctx.show_viewport_immediate(
                        inplace_viewport_id(),
                        build_inplace_viewport_builder(pos, size),
                        |viewport_ui, _class| {
                            if !self.inplace_wda_attempted {
                                self.inplace_wda_attempted = true;
                                match apply_exclude_from_capture_by_title(INPLACE_WINDOW_TITLE) {
                                    Ok(()) => {
                                        self.inplace_wda_error = None;
                                    }
                                    Err(err) => {
                                        self.inplace_wda_error = Some(err.to_string());
                                        // Without WDA the background
                                        // rectangle sits on the capture
                                        // region and hides the original
                                        // subtitle from OCR, stalling
                                        // the pipeline on the old line.
                                        if self.state.ui_notice.is_none() {
                                            self.state.ui_notice = Some(format!(
                                                "Cảnh báo: không loại được cửa sổ phụ đề tại chỗ khỏi capture ({err}); phụ đề gốc có thể bị che khỏi OCR."
                                            ));
                                        }
                                    }
                                }
                            }
                            if rect_changed {
                                viewport_ui
                                    .ctx()
                                    .send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                                viewport_ui
                                    .ctx()
                                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                            }
                            egui::CentralPanel::default()
                                .frame(egui::Frame::NONE)
                                .show(viewport_ui, |inner_ui| {
                                    render_inplace_subtitle_ui(
                                        inner_ui,
                                        &self.state.pipeline.chinese_line,
                                        &self.state.pinyin_line,
                                        &self.state.pipeline.vietnamese_line,
                                        &self.config,
                                    );
                                });
                        },
                    );
                }
            }
            if !inplace_shown {
                // The window is closed when not shown (immediate
                // viewport); re-apply WDA next time it opens, since
                // the reopened window is a new HWND.
                self.inplace_wda_attempted = false;
            }

            let commands = egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    render_overlay_ui(ui, &mut self.state, &self.config, &self.font_warnings)
                })
                .inner;
            self.dispatch_commands(&ctx, commands);
            // Multi-line subtitles need a taller window (2-line bug fix).
            self.apply_multiline_height(&ctx);
        }

        if matches!(self.state.server_status, ServerUiStatus::Starting)
            || self.state.is_running()
            || self.tts_preview_rx.is_some()
            || self.chinese_replay_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.tray.take();
        self.hotkeys.take();
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
    use super::region::RegionSelectorOutcome;
    use super::settings::SettingsDraftState;
    use super::{
        apply_region_selection_outcome, build_native_options, commit_settings_to_state,
        stop_pipeline_with_timeout, SettingsCommitEffects,
    };
    use crate::api::{BoxFuture, TranslateError, Translator, TtsEngine, TtsError, TtsSynthesis};
    use crate::audio::{AudioError, AudioPlayback, EnqueueOutcome};
    use crate::capture::{CaptureError, CapturedFrame, FrameCapturer, MonitorInfo};
    use crate::config::{load_config_from, AppConfig, CaptureRegion, ConfigError, WindowPosition};
    use crate::ocr::{OcrAdapterError, OcrEngine, OcrResult, TextBlock};
    use crate::pipeline::{
        PipelineDeps, PipelineRunner, PipelineSettings, PipelineStatus, SubtitleLogger,
    };
    use crate::translate::TranslationEngine;
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
        ) -> BoxFuture<'a, Result<TtsSynthesis, TtsError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(TtsSynthesis {
                    audio: vec![0x49, 0x44, 0x33, 0x01],
                    retries: Vec::new(),
                })
            })
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
            hold_delay: Duration::ZERO,
            voice: "vi-VN-NamMinhNeural".to_string(),
            speech_rate: "+0%".to_string(),
            auto_read: true,
            muted: false,
            volume: 0.75,
            enable_subtitle_log: false,
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
            translation_cache: None,
            tts_cache: None,
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
            translation_cache: None,
            tts_cache: None,
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
            translation_cache: None,
            tts_cache: None,
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

    #[test]
    fn region_selection_confirmed_updates_config_and_clears_missing_region_notice() {
        let mut cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        state.server_status = ServerUiStatus::Ready;
        assert!(!state.request_start(false));
        assert!(state.region_panel_open);
        assert!(state.ui_notice.is_some());

        let selected = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 320,
            y: 900,
            width: 1280,
            height: 80,
        };
        let changed = apply_region_selection_outcome(
            &mut cfg,
            &mut state,
            &RegionSelectorOutcome::Confirmed(selected.clone()),
        );
        assert!(changed);
        assert_eq!(cfg.general.region, Some(selected));
        assert!(!state.region_panel_open);
        assert_eq!(state.active_error_banner(), None);
    }

    #[test]
    fn region_selection_cancelled_preserves_existing_saved_region() {
        let existing = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 420,
            y: 1020,
            width: 1280,
            height: 60,
        };
        let mut cfg = AppConfig::default();
        cfg.general.region = Some(existing.clone());
        let mut state = OverlayState::from_config(&cfg, Vec::new());

        let changed =
            apply_region_selection_outcome(&mut cfg, &mut state, &RegionSelectorOutcome::Cancelled);
        assert!(!changed);
        assert_eq!(cfg.general.region, Some(existing));
    }

    #[test]
    fn settings_commit_saves_atomically_updates_live_state_and_invokes_autostart_on_toggle() {
        let temp_dir = TestTempDir::new("settings_commit_ok");
        let cfg_path = temp_dir.path().join("config.toml");

        let mut live_cfg = AppConfig::default();
        let mut ui_state = OverlayState::from_config(&live_cfg, Vec::new());

        let mut draft_state = SettingsDraftState::from_config(&live_cfg);
        draft_state.draft.general.capture_fps = 15;
        draft_state.draft.general.start_with_windows = true;
        draft_state.draft.voice.voice = "vi-VN-HoaiMyNeural".to_string();
        draft_state.draft.voice.volume = 0.55;
        draft_state.draft.display.font_size_px = 24.0;
        draft_state.draft.display.always_on_top = false;
        draft_state.draft.translation.engine = TranslationEngine::Vietphrase;

        let committed = match draft_state.try_commit() {
            Ok(c) => c,
            Err(err) => panic!("expected draft commit to pass: {err}"),
        };

        let autostart_calls = Mutex::new(Vec::new());
        let effects = match commit_settings_to_state(
            &mut live_cfg,
            Some(&cfg_path),
            &mut ui_state,
            committed.clone(),
            |enabled| {
                autostart_calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(enabled);
                Ok(())
            },
        ) {
            Ok(eff) => eff,
            Err(err) => panic!("expected commit_settings_to_state to succeed: {err}"),
        };

        assert_eq!(
            effects,
            SettingsCommitEffects {
                engine_changed: true,
                wechat_paths_changed: false,
                always_on_top_changed: true,
            }
        );
        assert_eq!(
            *autostart_calls.lock().unwrap_or_else(|e| e.into_inner()),
            vec![true]
        );
        assert_eq!(live_cfg, committed);
        assert_eq!(ui_state.selected_voice, "vi-VN-HoaiMyNeural");

        let reloaded = match load_config_from(&cfg_path) {
            Ok(out) => out.config,
            Err(err) => panic!("expected saved config.toml to reload cleanly: {err}"),
        };
        assert_eq!(reloaded, committed);
    }

    #[test]
    fn settings_commit_aborts_without_modifying_config_when_autostart_fails() {
        let temp_dir = TestTempDir::new("settings_commit_autostart_fail");
        let cfg_path = temp_dir.path().join("config.toml");

        let mut live_cfg = AppConfig::default();
        let initial_cfg = live_cfg.clone();
        let mut ui_state = OverlayState::from_config(&live_cfg, Vec::new());

        let mut next_cfg = AppConfig::default();
        next_cfg.general.start_with_windows = true;
        next_cfg.voice.voice = "vi-VN-HoaiMyNeural".to_string();

        let res = commit_settings_to_state(
            &mut live_cfg,
            Some(&cfg_path),
            &mut ui_state,
            next_cfg,
            |_enabled| {
                Err(ConfigError::Registry {
                    action: "RegSetValueExW",
                    code: 5,
                })
            },
        );
        assert!(res.is_err());
        assert_eq!(live_cfg, initial_cfg);
        assert!(!cfg_path.exists());
        assert_eq!(ui_state.selected_voice, initial_cfg.voice.voice);
    }

    #[test]
    fn settings_cancel_leaves_config_file_and_live_overlay_state_unchanged() {
        let mut live_cfg = AppConfig::default();
        live_cfg.voice.volume = 0.75;
        let ui_state = OverlayState::from_config(&live_cfg, Vec::new());

        let mut draft_state = SettingsDraftState::from_config(&live_cfg);
        draft_state.draft.voice.volume = 0.15;
        draft_state.draft.voice.voice = "vi-VN-HoaiMyNeural".to_string();
        draft_state.draft.display.font_size_px = 30.0;

        let discarded = draft_state.discard();
        assert_eq!(discarded, live_cfg);
        assert_eq!(ui_state.selected_voice, "vi-VN-NamMinhNeural");
        assert!((live_cfg.voice.volume - 0.75).abs() < 1e-6);
    }

    #[test]
    fn settings_select_region_updates_draft_until_saved_or_discarded() {
        let temp_dir = TestTempDir::new("settings_region_draft");
        let cfg_path = temp_dir.path().join("config.toml");

        let saved_region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 100,
            y: 800,
            width: 800,
            height: 80,
        };
        let mut live_cfg = AppConfig::default();
        live_cfg.general.region = Some(saved_region.clone());
        let mut ui_state = OverlayState::from_config(&live_cfg, Vec::new());

        let mut draft_state = SettingsDraftState::from_config(&live_cfg);
        let new_region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 420,
            y: 1020,
            width: 1280,
            height: 60,
        };
        draft_state.set_draft_region(new_region.clone());
        assert_eq!(live_cfg.general.region, Some(saved_region.clone()));
        assert!(!cfg_path.exists());

        let committed = match draft_state.try_commit() {
            Ok(c) => c,
            Err(err) => panic!("expected draft commit to pass: {err}"),
        };
        let res = commit_settings_to_state(
            &mut live_cfg,
            Some(&cfg_path),
            &mut ui_state,
            committed,
            |_enabled| Ok(()),
        );
        assert!(res.is_ok());
        assert_eq!(live_cfg.general.region, Some(new_region.clone()));
        assert!(cfg_path.exists());
    }
}
