//! Live pipeline lifecycle and settings commit: starting/stopping the
//! subtitle pipeline, committing settings drafts, and applying region
//! selection outcomes.

use super::hotkeys::HotkeyAction;
use super::overlay::{OverlayState, ServerUiStatus};
use super::region::RegionSelectorOutcome;
use super::settings::AsyncFeedbackState;
use super::tray::TrayMenuAction;
use crate::api::{Translator, TtsEngine};
use crate::audio::AudioPlayer;
use crate::config::{save_config_to, AppConfig, ConfigError, WindowPosition};
use crate::ocr::{resolve_wechat_paths_from_config, WeChatOcrAdapter};
use crate::pipeline::{
    OcrDebugLogger, PipelineDeps, PipelineRunner, PipelineSettings, PipelineSnapshot,
    PipelineStatus, SubtitleLogger, TranslationCache, TtsCache,
};
use crate::translate::{HelsinkiState, TranslationEngine};
use eframe::egui;
use std::path::Path;
use std::sync::{mpsc, Arc};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SettingsCommitEffects {
    pub engine_changed: bool,
    pub wechat_paths_changed: bool,
    pub always_on_top_changed: bool,
    pub tts_engine_changed: bool,
    pub piper_voice_changed: bool,
}

pub(crate) struct TtsPreviewAudio {
    pub(crate) audio_bytes: Vec<u8>,
    pub(crate) format: crate::audio::AudioFormat,
    pub(crate) volume: f32,
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
        engine_changed: current_config.translation.engine != new_config.translation.engine,
        wechat_paths_changed: current_config.general.wxocr_dll_path
            != new_config.general.wxocr_dll_path
            || current_config.general.weixin_dir_path != new_config.general.weixin_dir_path,
        always_on_top_changed: current_config.display.always_on_top
            != new_config.display.always_on_top,
        tts_engine_changed: current_config.voice.engine != new_config.voice.engine,
        piper_voice_changed: new_config.voice.engine == crate::tts::TtsEngineKind::Piper
            && current_config.voice.voice != new_config.voice.voice,
    };

    *current_config = new_config;
    ui_state.ui_notice = None;
    Ok(effects)
}

pub(crate) fn build_live_pipeline_settings(
    config: &AppConfig,
    state: &OverlayState,
) -> Result<PipelineSettings, crate::pipeline::PipelineError> {
    let mut settings = PipelineSettings::from_app_config(config)?;
    settings.muted = state.muted;
    // The voice comes from the config — the overlay has no voice selector
    // (voice selection lives ONLY in Settings since 2026-11-24).
    settings.voice = config.voice.voice.clone();
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

impl super::Cap2VoiceApp {
    pub(crate) fn poll_background_channels(&mut self, ctx: &egui::Context) {
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

        // Receive the background translator build (D1+A5): dictionary
        // load takes seconds, so it never blocks app startup. If the
        // user already switched the engine in Settings while the loader
        // was running, align the arrived translator with the live config.
        if let Some(rx) = &self.translator_rx {
            match rx.try_recv() {
                Ok((translator, warnings)) => {
                    self.state.startup_warnings.extend(warnings);
                    if translator.is_none() {
                        self.translator_error = Some(
                            "Không có engine dịch khả dụng — kiểm tra model/từ điển".to_string(),
                        );
                    } else if let Some(translator) = &translator {
                        translator.set_engine(self.config.translation.engine);
                    }
                    self.translator = translator;
                    self.translator_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.translator_error =
                        Some("Luồng tải engine dịch đã dừng bất ngờ".to_string());
                    self.translator_rx = None;
                }
            }
        }

        // Track the native translator status on the overlay (D1+A5):
        // the Helsinki model loads in the background, so the header shows
        // "Đang tải model dịch…" until it is ready.
        match &self.translator {
            None => {
                self.state.server_status = match &self.translator_error {
                    Some(err) => ServerUiStatus::Failed(err.clone()),
                    // Loader still running: Starting, not Failed.
                    None => ServerUiStatus::Starting,
                };
            }
            Some(translator) => {
                let status = translator.status();
                self.state.server_status = match (&status.engine, &status.helsinki) {
                    (TranslationEngine::Helsinki, HelsinkiState::Loading) => {
                        ServerUiStatus::Starting
                    }
                    (TranslationEngine::Helsinki, HelsinkiState::Failed(detail))
                        if !status.vietphrase_available =>
                    {
                        ServerUiStatus::Failed(format!(
                            "Model Helsinki lỗi và không có Vietphrase: {detail}"
                        ))
                    }
                    _ => ServerUiStatus::Ready,
                };
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
                Ok(Ok(TtsPreviewAudio {
                    audio_bytes,
                    format,
                    volume,
                })) => {
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
                            .and_then(|()| audio.enqueue_audio(audio_bytes, format));
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

        let replay_result = if let Some(rx) = &self.chinese_replay_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.chinese_replay_rx = None;
                    Some(Ok(res))
                }
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.chinese_replay_rx = None;
                    Some(Err("Luồng nghe lại tiếng Trung đã dừng bất ngờ".to_string()))
                }
            }
        } else {
            None
        };

        if let Some(outcome) = replay_result {
            match outcome {
                Ok(Ok(TtsPreviewAudio {
                    audio_bytes,
                    format,
                    volume,
                })) => {
                    if self.audio.is_none() {
                        match AudioPlayer::new(volume) {
                            Ok(player) => {
                                self.audio = Some(Arc::new(player));
                            }
                            Err(err) => {
                                self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                            }
                        }
                    }
                    if let Some(audio) = &self.audio {
                        let play_res = audio
                            .set_volume(volume)
                            .and_then(|()| audio.enqueue_audio(audio_bytes, format));
                        if let Err(err) = play_res {
                            self.state.ui_notice = Some(format!("Lỗi phát âm thanh: {err}"));
                        }
                    }
                }
                Ok(Err(err)) => {
                    self.state.ui_notice = Some(format!("Không nghe lại được tiếng Trung: {err}"));
                }
                Err(msg) => {
                    self.state.ui_notice = Some(msg);
                }
            }
        }

        let vietnamese_replay_result = if let Some(rx) = &self.vietnamese_replay_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.vietnamese_replay_rx = None;
                    Some(Ok(res))
                }
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.vietnamese_replay_rx = None;
                    Some(Err("Luồng nghe lại tiếng Việt đã dừng bất ngờ".to_string()))
                }
            }
        } else {
            None
        };

        if let Some(outcome) = vietnamese_replay_result {
            match outcome {
                Ok(Ok(TtsPreviewAudio {
                    audio_bytes,
                    format,
                    volume,
                })) => {
                    if self.audio.is_none() {
                        match AudioPlayer::new(volume) {
                            Ok(player) => {
                                self.audio = Some(Arc::new(player));
                            }
                            Err(err) => {
                                self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                            }
                        }
                    }
                    if let Some(audio) = &self.audio {
                        let play_res = audio
                            .set_volume(volume)
                            .and_then(|()| audio.enqueue_audio(audio_bytes, format));
                        if let Err(err) = play_res {
                            self.state.ui_notice = Some(format!("Lỗi phát âm thanh: {err}"));
                        }
                    }
                }
                Ok(Err(err)) => {
                    self.state.ui_notice = Some(format!("Không nghe lại được tiếng Việt: {err}"));
                }
                Err(msg) => {
                    self.state.ui_notice = Some(msg);
                }
            }
        }

        // Piper voice download progress (Settings → Giọng đọc): the
        // download runs on a background thread; each event updates the
        // draft's progress state so the Settings UI shows progress.
        // The channel disconnects when the download thread ends (after
        // sending Done/Error), so a disconnect also clears the receiver.
        if let Some(rx) = &self.piper_download_rx {
            let mut finished = false;
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        finished = matches!(
                            event,
                            crate::tts::PiperDownloadEvent::Done { .. }
                                | crate::tts::PiperDownloadEvent::Error(_)
                        );
                        if let Some(draft) = &mut self.settings_draft {
                            draft.apply_piper_download_event(event);
                        }
                        if finished {
                            break;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
            if finished {
                self.piper_download_rx = None;
            }
        }
    }

    pub(crate) fn persist_config(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        if let Err(err) = save_config_to(path, &self.config) {
            self.state.ui_notice =
                Some(format!("Không lưu được cấu hình {}: {err}", path.display()));
        }
    }

    pub(crate) fn save_overlay_position(&mut self, pos: WindowPosition) {
        self.config.display.overlay_position = Some(pos.clone());
        self.persist_config();
        self.position_tracker.mark_saved(pos);
    }

    /// Load (or reuse) the translation + TTS caches from
    /// `config.effective_cache_dir()` (A3 / gh #2). Kept on the app so the
    /// Settings "clear cache" action can reach the same `Arc`s the
    /// pipeline uses, even before the pipeline ever started.
    pub(crate) fn ensure_caches(
        &mut self,
    ) -> Result<(Arc<TranslationCache>, Arc<TtsCache>), String> {
        if let (Some(t), Some(a)) = (&self.translation_cache, &self.tts_cache) {
            return Ok((Arc::clone(t), Arc::clone(a)));
        }
        let dir = self
            .config
            .effective_cache_dir()
            .map_err(|err| err.to_string())?;
        let translation = Arc::new(TranslationCache::load(&dir).map_err(|err| err.to_string())?);
        let tts = Arc::new(TtsCache::load(&dir).map_err(|err| err.to_string())?);
        self.translation_cache = Some(Arc::clone(&translation));
        self.tts_cache = Some(Arc::clone(&tts));
        Ok((translation, tts))
    }

    pub(crate) fn handle_start_pipeline(&mut self, ctx: &egui::Context) {
        if !self
            .state
            .request_start(self.config.general.region.is_some())
        {
            return;
        }
        let Some(translator) = self.translator.clone() else {
            let msg = if self.translator_error.is_some() {
                "Không có engine dịch khả dụng — kiểm tra model/từ điển"
            } else {
                "Engine dịch đang tải, thử lại sau giây lát"
            };
            self.state.ui_notice = Some(msg.to_string());
            return;
        };
        let tts_engine = Arc::clone(&self.tts_engine);

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

        // The shared native engines (built once at startup): the
        // translator is the `Arc<SwitchableTranslator>` behind `dyn
        // Translator`, so a Settings engine switch applies to the running
        // pipeline too; the TTS engine is the native Edge-TTS client.
        let translator: Arc<dyn Translator> = translator;
        let tts: Arc<dyn TtsEngine> = tts_engine;

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

        // Raw-OCR debug log (merge fix, 2026-11-25): built ONLY when the
        // Settings → Chung toggle `debug_logging` is ON (default OFF).
        // The toggle takes effect on the next pipeline start: the logger
        // is built once here and lives in `PipelineDeps`. When OFF, no
        // debug file is created and the pipeline pays nothing but a
        // `None` check per decision (the counters are already kept for
        // the UI metrics).
        let ocr_debug_logger = if self.config.general.debug_logging {
            match self.config.effective_log_dir() {
                Ok(dir) => Some(Arc::new(OcrDebugLogger::new(dir))),
                Err(err) => {
                    self.state.ui_notice = Some(format!("Không xác định được thư mục log: {err}"));
                    return;
                }
            }
        } else {
            None
        };

        // Translation + TTS caches (A3 / gh #2). A load failure is not
        // fatal: the pipeline runs uncached and retries the load next start.
        let (translation_cache, tts_cache) = match self.ensure_caches() {
            Ok(caches) => (Some(caches.0), Some(caches.1)),
            Err(err) => {
                self.state.ui_notice = Some(format!(
                    "Không tải được bộ nhớ đệm, chạy không cache: {err}"
                ));
                (None, None)
            }
        };

        let deps = PipelineDeps {
            capturer: Arc::clone(&self.capturer),
            ocr,
            translator,
            tts,
            audio,
            logger,
            ocr_debug_logger,
            translation_cache,
            tts_cache,
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

    pub(crate) fn start_repaint_forwarder(
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

    pub(crate) fn handle_stop_pipeline(&mut self, ctx: &egui::Context) {
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
}
