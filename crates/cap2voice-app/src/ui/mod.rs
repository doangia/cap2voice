//! Desktop UI modules (`eframe` 0.35 shell, composite fonts, floating overlay).

pub mod fonts;
pub mod hotkeys;
pub mod icon;
pub mod overlay;
pub mod region;
pub mod settings;
pub mod tray;

use crate::api::{
    check_health, discover_server_dir, HealthError, HealthStatus, HttpTranslator, HttpTtsEngine,
    ServerProcess, ServerProcessConfig, ServerProcessError, TtsEngine, TtsError,
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_HEALTH_TIMEOUT, DEFAULT_TRANSLATE_TIMEOUT,
    DEFAULT_TTS_TIMEOUT,
};
use crate::audio::{AudioPlayback, AudioPlayer};
use crate::capture::{FrameCapturer, XcapCapturer};
use crate::config::{
    default_config_path, load_config_from, save_config_to, set_windows_autostart, AppConfig,
    ConfigError, WindowPosition,
};
use crate::ocr::{resolve_wechat_paths_from_config, OcrEngine, WeChatOcrAdapter};
use crate::pipeline::{
    PipelineDeps, PipelineHandle, PipelineRunner, PipelineSettings, PipelineSnapshot,
    PipelineStatus, SubtitleLogger,
};
use eframe::egui;
use global_hotkey::GlobalHotKeyEvent;
use hotkeys::{HotkeyAction, HotkeyManager};
use overlay::{
    apply_exclude_from_capture_hwnd, overlay_overlaps_region, render_overlay_ui,
    DisplayAffinityError, OverlayCommand, OverlayState, PositionTracker, ServerUiStatus,
    DEFAULT_OVERLAY_SIZE, POSITION_SAVE_DEBOUNCE,
};
use region::{
    build_region_viewport_builder, region_viewport_id, render_region_selector_ui,
    select_active_monitor_index, RegionSelectorOutcome, RegionSelectorState,
};
use settings::{
    build_settings_viewport_builder, render_settings_ui, settings_viewport_id, AsyncFeedbackState,
    SettingsCommand, SettingsDraftState, SETTINGS_PREVIEW_SAMPLE_TEXT,
};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tray::{should_cancel_close, tray_action_for_menu_event, TrayIconHandle, TrayMenuAction};
use tray_icon::{menu::MenuEvent, TrayIconEvent};

fn configured_window_level(config: &AppConfig) -> egui::WindowLevel {
    if config.display.always_on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    }
}

/// Build `eframe::NativeOptions` for the borderless, transparent, always-on-top overlay.
pub fn build_native_options(config: &AppConfig) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("cap2voice")
        .with_inner_size(DEFAULT_OVERLAY_SIZE)
        .with_min_inner_size([460.0, 140.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_visible(!config.general.start_minimized_to_tray)
        .with_window_level(configured_window_level(config));

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionSelectionTarget {
    LiveConfig,
    SettingsDraft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SettingsCommitEffects {
    pub port_changed: bool,
    pub wechat_paths_changed: bool,
    pub always_on_top_changed: bool,
}

struct TtsPreviewAudio {
    mp3_bytes: Vec<u8>,
    volume: f32,
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
    last_overlay_outer_pos: Option<WindowPosition>,
    region_selector: Option<RegionSelectorState>,
    region_selector_focused: bool,
    region_selection_target: RegionSelectionTarget,
    settings_draft: Option<SettingsDraftState>,
    settings_focused: bool,
    health_check_rx: Option<mpsc::Receiver<Result<HealthStatus, HealthError>>>,
    tts_preview_rx: Option<mpsc::Receiver<Result<TtsPreviewAudio, TtsError>>>,
    server_rx: Option<mpsc::Receiver<Result<ServerProcess, ServerProcessError>>>,
    server: Option<ServerProcess>,
    capturer: Arc<dyn FrameCapturer>,
    ocr: Option<Arc<dyn OcrEngine>>,
    audio: Option<Arc<dyn AudioPlayback>>,
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

fn spawn_server_startup(
    runtime: &tokio::runtime::Runtime,
    port: u16,
    ctx: egui::Context,
) -> mpsc::Receiver<Result<ServerProcess, ServerProcessError>> {
    let (server_tx, server_rx) = mpsc::sync_channel(1);
    runtime.spawn(async move {
        let res = match resolve_server_directory() {
            Ok(server_dir) => {
                ServerProcess::start(ServerProcessConfig::for_port(port, server_dir)).await
            }
            Err(err) => Err(err),
        };
        let _ = server_tx.send(res);
        ctx.request_repaint();
    });
    server_rx
}

pub(crate) fn commit_settings_to_state<F>(
    current_config: &mut AppConfig,
    config_path: Option<&Path>,
    ui_state: &mut OverlayState,
    new_config: AppConfig,
    apply_autostart: F,
) -> Result<SettingsCommitEffects, String>
where
    F: Fn(bool) -> Result<(), ConfigError>,
{
    new_config.validate().map_err(|err| err.to_string())?;

    let prev_autostart = current_config.general.start_with_windows;
    let next_autostart = new_config.general.start_with_windows;
    let autostart_changed = prev_autostart != next_autostart;

    if autostart_changed {
        apply_autostart(next_autostart)
            .map_err(|err| format!("Không thể cập nhật khởi động cùng Windows: {err}"))?;
    }

    if let Some(path) = config_path {
        if let Err(err) = save_config_to(path, &new_config) {
            if autostart_changed {
                let _ = apply_autostart(prev_autostart);
            }
            return Err(format!("Không lưu được cấu hình {}: {err}", path.display()));
        }
    }

    let effects = SettingsCommitEffects {
        port_changed: current_config.translation.server_port != new_config.translation.server_port,
        wechat_paths_changed: current_config.general.wxocr_dll_path
            != new_config.general.wxocr_dll_path
            || current_config.general.weixin_dir_path != new_config.general.weixin_dir_path,
        always_on_top_changed: current_config.display.always_on_top
            != new_config.display.always_on_top,
    };

    *current_config = new_config;
    ui_state.selected_voice = current_config.voice.voice.clone();
    ui_state.ui_notice = None;
    Ok(effects)
}

fn build_live_pipeline_settings(
    config: &AppConfig,
    state: &OverlayState,
) -> Result<PipelineSettings, crate::pipeline::PipelineError> {
    let mut settings = PipelineSettings::from_app_config(config)?;
    settings.muted = state.muted;
    settings.voice = state.selected_voice.clone();
    Ok(settings)
}

pub(crate) fn apply_region_selection_outcome(
    config: &mut AppConfig,
    state: &mut OverlayState,
    outcome: &RegionSelectorOutcome,
) -> bool {
    match outcome {
        RegionSelectorOutcome::Confirmed(region) => {
            config.general.region = Some(region.clone());
            state.region_panel_open = false;
            state.ui_notice = None;
            true
        }
        RegionSelectorOutcome::Cancelled | RegionSelectorOutcome::Pending => false,
    }
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

        let server_rx = spawn_server_startup(
            &runtime,
            bootstrap.config.translation.server_port,
            cc.egui_ctx.clone(),
        );

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
            settings_draft: None,
            settings_focused: false,
            health_check_rx: None,
            tts_preview_rx: None,
            server_rx: Some(server_rx),
            server: None,
            capturer: Arc::new(XcapCapturer::new()),
            ocr: None,
            audio: None,
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

    fn poll_background_channels(&mut self, ctx: &egui::Context) {
        let tray_actions: Vec<TrayMenuAction> = match &self.tray_action_rx {
            Some(rx) => {
                let mut actions = Vec::new();
                while let Ok(action) = rx.try_recv() {
                    actions.push(action);
                }
                actions
            }
            None => Vec::new(),
        };
        for action in tray_actions {
            self.dispatch_tray_action(ctx, action);
        }
        let hotkey_actions: Vec<HotkeyAction> = match &self.hotkey_event_rx {
            Some(rx) => {
                let mut actions = Vec::new();
                while let Ok(event) = rx.try_recv() {
                    if let Some(manager) = &self.hotkeys {
                        if let Some(action) = manager.action_for_event(&event) {
                            actions.push(action);
                        }
                    }
                }
                actions
            }
            None => Vec::new(),
        };
        for action in hotkey_actions {
            self.dispatch_hotkey_action(ctx, action);
        }

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

        if let Some(rx) = &self.health_check_rx {
            match rx.try_recv() {
                Ok(Ok(health)) => {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.connection_status = AsyncFeedbackState::Success(format!(
                            "Kết nối OK (v{} · {} · {})",
                            health.version, health.translator, health.tts
                        ));
                    }
                    self.health_check_rx = None;
                }
                Ok(Err(err)) => {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.connection_status =
                            AsyncFeedbackState::Error(format!("Lỗi kết nối: {err}"));
                    }
                    self.health_check_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.connection_status = AsyncFeedbackState::Error(
                            "Luồng kiểm tra kết nối đã dừng bất ngờ".to_string(),
                        );
                    }
                    self.health_check_rx = None;
                }
            }
        }

        let preview_result = if let Some(rx) = &self.tts_preview_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.tts_preview_rx = None;
                    Some(Ok(res))
                }
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.tts_preview_rx = None;
                    Some(Err("Luồng nghe thử TTS đã dừng bất ngờ".to_string()))
                }
            }
        } else {
            None
        };

        if let Some(outcome) = preview_result {
            match outcome {
                Ok(Ok(TtsPreviewAudio { mp3_bytes, volume })) => {
                    if self.audio.is_none() {
                        match AudioPlayer::new(volume) {
                            Ok(player) => {
                                self.audio = Some(Arc::new(player));
                            }
                            Err(err) => {
                                if let Some(draft) = &mut self.settings_draft {
                                    draft.tts_preview_status =
                                        AsyncFeedbackState::Error(format!("Lỗi âm thanh: {err}"));
                                }
                            }
                        }
                    }
                    if let Some(audio) = &self.audio {
                        let play_res = audio
                            .set_volume(volume)
                            .and_then(|()| audio.enqueue_mp3(mp3_bytes));
                        if let Some(draft) = &mut self.settings_draft {
                            draft.tts_preview_status = match play_res {
                                Ok(_) => AsyncFeedbackState::Success(
                                    "Đang phát mẫu giọng đọc".to_string(),
                                ),
                                Err(err) => {
                                    AsyncFeedbackState::Error(format!("Lỗi phát âm thanh: {err}"))
                                }
                            };
                        }
                    }
                }
                Ok(Err(err)) => {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.tts_preview_status =
                            AsyncFeedbackState::Error(format!("Lỗi TTS: {err}"));
                    }
                }
                Err(msg) => {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.tts_preview_status = AsyncFeedbackState::Error(msg);
                    }
                }
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

        let settings = match build_live_pipeline_settings(&self.config, &self.state) {
            Ok(s) => s,
            Err(err) => {
                self.state.ui_notice = Some(err.to_string());
                return;
            }
        };

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

    fn open_region_selector(&mut self, ctx: &egui::Context, target: RegionSelectionTarget) {
        let monitors = match self.capturer.list_monitors() {
            Ok(list) => list,
            Err(err) => {
                let msg = format!("Không thể liệt kê màn hình để chọn vùng: {err}");
                if target == RegionSelectionTarget::SettingsDraft {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.validation_error = Some(msg);
                    }
                } else {
                    self.state.ui_notice = Some(msg);
                }
                return;
            }
        };
        let active_idx = select_active_monitor_index(
            &monitors,
            self.last_overlay_outer_pos.as_ref(),
            ctx.pixels_per_point(),
        );
        let selector = match RegionSelectorState::new(monitors, active_idx) {
            Ok(s) => s,
            Err(err) => {
                if target == RegionSelectionTarget::SettingsDraft {
                    if let Some(draft) = &mut self.settings_draft {
                        draft.validation_error = Some(err.to_string());
                    }
                } else {
                    self.state.ui_notice = Some(err.to_string());
                }
                return;
            }
        };

        self.state.region_panel_open = false;
        self.region_selection_target = target;
        self.region_selector = Some(selector);
        self.region_selector_focused = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
        ctx.request_repaint();
    }

    fn finish_region_selection(&mut self, ctx: &egui::Context, outcome: RegionSelectorOutcome) {
        self.region_selector = None;
        self.region_selector_focused = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(configured_window_level(
            &self.config,
        )));

        if self.region_selection_target == RegionSelectionTarget::SettingsDraft {
            self.region_selection_target = RegionSelectionTarget::LiveConfig;
            if let RegionSelectorOutcome::Confirmed(region) = outcome {
                if let Some(draft) = &mut self.settings_draft {
                    draft.set_draft_region(region);
                }
            }
            self.settings_focused = false;
            ctx.request_repaint();
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();

        if apply_region_selection_outcome(&mut self.config, &mut self.state, &outcome) {
            self.persist_config();
            if let Some(handle) = &self.pipeline_handle {
                match build_live_pipeline_settings(&self.config, &self.state) {
                    Ok(settings) => {
                        if let Err(err) = handle.update_settings(settings) {
                            self.state.ui_notice = Some(err.to_string());
                        }
                    }
                    Err(err) => {
                        self.state.ui_notice = Some(err.to_string());
                    }
                }
            }
        }
    }

    fn open_settings_window(&mut self, ctx: &egui::Context) {
        self.state.settings_panel_open = false;
        if self.settings_draft.is_none() {
            self.settings_draft = Some(SettingsDraftState::from_config(&self.config));
        }
        self.settings_focused = false;
        ctx.request_repaint();
    }

    fn handle_settings_command(&mut self, ctx: &egui::Context, cmd: SettingsCommand) {
        match cmd {
            SettingsCommand::Cancel => {
                if let Some(draft) = self.settings_draft.take() {
                    let _ = draft.discard();
                }
                if let Some(handle) = &self.pipeline_handle {
                    if let Err(err) = handle.set_volume(self.config.voice.volume) {
                        self.state.ui_notice = Some(err.to_string());
                    }
                } else if let Some(audio) = &self.audio {
                    if let Err(err) = audio.set_volume(self.config.voice.volume) {
                        self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                    }
                }
                self.health_check_rx = None;
                self.tts_preview_rx = None;
                self.settings_focused = false;
                ctx.request_repaint();
            }
            SettingsCommand::SelectRegion => {
                self.open_region_selector(ctx, RegionSelectionTarget::SettingsDraft);
            }
            SettingsCommand::TestConnection { port } => {
                if let Some(draft) = &mut self.settings_draft {
                    draft.connection_status = AsyncFeedbackState::InProgress(format!(
                        "Đang kiểm tra 127.0.0.1:{port}..."
                    ));
                }
                let (tx, rx) = mpsc::sync_channel(1);
                self.health_check_rx = Some(rx);
                let ctx_wake = ctx.clone();
                self.runtime.spawn(async move {
                    let base_url = format!("http://127.0.0.1:{port}");
                    let res = check_health(&base_url, DEFAULT_HEALTH_TIMEOUT).await;
                    let _ = tx.send(res);
                    ctx_wake.request_repaint();
                });
                ctx.request_repaint();
            }
            SettingsCommand::PreviewVoice {
                port,
                voice,
                rate,
                volume,
            } => {
                if self.audio.is_none() {
                    match AudioPlayer::new(volume) {
                        Ok(player) => {
                            self.audio = Some(Arc::new(player));
                        }
                        Err(err) => {
                            if let Some(draft) = &mut self.settings_draft {
                                draft.tts_preview_status =
                                    AsyncFeedbackState::Error(format!("Lỗi âm thanh: {err}"));
                            }
                            return;
                        }
                    }
                }
                if let Some(draft) = &mut self.settings_draft {
                    draft.tts_preview_status =
                        AsyncFeedbackState::InProgress(format!("Đang tạo mẫu giọng {voice}..."));
                }
                let (tx, rx) = mpsc::sync_channel(1);
                self.tts_preview_rx = Some(rx);
                let ctx_wake = ctx.clone();
                self.runtime.spawn(async move {
                    let base_url = format!("http://127.0.0.1:{port}");
                    let res = match HttpTtsEngine::with_base_url(
                        base_url,
                        DEFAULT_CONNECT_TIMEOUT,
                        DEFAULT_TTS_TIMEOUT,
                    ) {
                        Ok(client) => client
                            .synthesize(SETTINGS_PREVIEW_SAMPLE_TEXT, &voice, &rate)
                            .await
                            .map(|mp3_bytes| TtsPreviewAudio { mp3_bytes, volume }),
                        Err(err) => Err(err),
                    };
                    let _ = tx.send(res);
                    ctx_wake.request_repaint();
                });
                ctx.request_repaint();
            }
            SettingsCommand::Save(new_config) => {
                let prev_hotkeys = self.config.hotkeys.clone();
                let exe_res = std::env::current_exe().map_err(|err| ConfigError::Io {
                    path: PathBuf::from("cap2voice-app.exe"),
                    detail: err.to_string(),
                });
                let commit_res = commit_settings_to_state(
                    &mut self.config,
                    self.config_path.as_deref(),
                    &mut self.state,
                    *new_config,
                    |enabled| {
                        let exe = exe_res.as_ref().map_err(Clone::clone)?;
                        set_windows_autostart(enabled, exe)
                    },
                );

                match commit_res {
                    Ok(effects) => {
                        self.settings_draft = None;
                        self.settings_focused = false;
                        self.health_check_rx = None;
                        self.tts_preview_rx = None;

                        if effects.wechat_paths_changed {
                            self.ocr = None;
                        }
                        if effects.always_on_top_changed {
                            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                                configured_window_level(&self.config),
                            ));
                        }
                        if let Some(handle) = &self.pipeline_handle {
                            match build_live_pipeline_settings(&self.config, &self.state) {
                                Ok(settings) => {
                                    if let Err(err) = handle.update_settings(settings) {
                                        self.state.ui_notice = Some(err.to_string());
                                    }
                                }
                                Err(err) => {
                                    self.state.ui_notice = Some(err.to_string());
                                }
                            }
                        } else if let Some(audio) = &self.audio {
                            if let Err(err) = audio.set_volume(self.config.voice.volume) {
                                self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                            }
                        }
                        if effects.port_changed {
                            self.server = None;
                            self.state.server_status = ServerUiStatus::Starting;
                            let port = self.config.translation.server_port;
                            self.server_rx =
                                Some(spawn_server_startup(&self.runtime, port, ctx.clone()));
                        }
                        if prev_hotkeys != self.config.hotkeys {
                            self.reregister_hotkeys();
                        }
                        ctx.request_repaint();
                    }
                    Err(err_msg) => {
                        if let Some(draft) = &mut self.settings_draft {
                            draft.validation_error = Some(err_msg);
                        }
                    }
                }
            }
        }
    }

    fn show_overlay(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.overlay_visible = true;
        ctx.request_repaint();
    }

    fn hide_overlay(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.overlay_visible = false;
        ctx.request_repaint();
    }

    fn dispatch_tray_action(&mut self, ctx: &egui::Context, action: TrayMenuAction) {
        match action {
            TrayMenuAction::ShowWindow => self.show_overlay(ctx),
            TrayMenuAction::StartTranslation => self.handle_start_pipeline(ctx),
            TrayMenuAction::StopTranslation => self.handle_stop_pipeline(ctx),
            TrayMenuAction::OpenSettings => self.open_settings_window(ctx),
            TrayMenuAction::Quit => {
                self.quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn dispatch_hotkey_action(&mut self, ctx: &egui::Context, action: HotkeyAction) {
        match action {
            HotkeyAction::ToggleStartStop => {
                if self.state.is_running() {
                    self.handle_stop_pipeline(ctx);
                } else {
                    self.handle_start_pipeline(ctx);
                }
            }
            HotkeyAction::SelectRegion => {
                if !self.overlay_visible {
                    self.show_overlay(ctx);
                }
                self.open_region_selector(ctx, RegionSelectionTarget::LiveConfig);
            }
            HotkeyAction::ToggleOverlay => {
                if self.overlay_visible {
                    self.hide_overlay(ctx);
                } else {
                    self.show_overlay(ctx);
                }
            }
            HotkeyAction::ToggleMute => self.handle_toggle_mute(),
        }
    }

    fn sync_tray_menu_state(&mut self) {
        let running = self.state.is_running();
        if self.last_tray_running_state != Some(running) {
            self.last_tray_running_state = Some(running);
            if let Some(tray) = &self.tray {
                tray.set_running(running);
            }
        }
    }

    fn reregister_hotkeys(&mut self) {
        match HotkeyManager::register_all(&self.config.hotkeys) {
            Ok(manager) => {
                self.hotkeys = Some(manager);
            }
            Err(err) => {
                self.hotkeys = None;
                self.state.ui_notice = Some(format!("Không đăng ký được phím tắt: {err}"));
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
                    self.open_region_selector(ctx, RegionSelectionTarget::LiveConfig);
                }
                OverlayCommand::ToggleSettingsPanel => self.open_settings_window(ctx),
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

            let commands = egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    render_overlay_ui(ui, &mut self.state, &self.config, &self.font_warnings)
                })
                .inner;
            self.dispatch_commands(&ctx, commands);
        }

        if matches!(self.state.server_status, ServerUiStatus::Starting)
            || self.state.is_running()
            || self.health_check_rx.is_some()
            || self.tts_preview_rx.is_some()
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
    use super::region::RegionSelectorOutcome;
    use super::settings::SettingsDraftState;
    use super::{
        apply_region_selection_outcome, build_native_options, commit_settings_to_state,
        format_server_startup_error, stop_pipeline_with_timeout, SettingsCommitEffects,
    };
    use crate::api::{
        BoxFuture, HttpTranslator, HttpTtsEngine, ServerProcessError, TranslateError, Translator,
        TtsEngine, TtsError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_TRANSLATE_TIMEOUT,
        DEFAULT_TTS_TIMEOUT,
    };
    use crate::audio::{AudioError, AudioPlayback, EnqueueOutcome};
    use crate::capture::{CaptureError, CapturedFrame, FrameCapturer, MonitorInfo};
    use crate::config::{load_config_from, AppConfig, CaptureRegion, ConfigError, WindowPosition};
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
            hold_delay: Duration::ZERO,
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
        draft_state.server_port_input = "18888".to_string();

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
                port_changed: true,
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
