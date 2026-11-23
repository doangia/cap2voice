//! User-initiated commands: tray/hotkey dispatch, region selection,
//! settings window, mute/voice toggles, and overlay show/hide.

use super::bootstrap::{apply_wda_to_handle, configured_window_level};
use super::hotkeys::{HotkeyAction, HotkeyManager};
use super::overlay::{overlay_overlaps_region, OverlayCommand};
use super::pipeline::{
    apply_region_selection_outcome, build_live_pipeline_settings, commit_settings_to_state,
    TtsPreviewAudio,
};
use super::region::{select_active_monitor_index, RegionSelectorOutcome, RegionSelectorState};
use super::settings::{
    AsyncFeedbackState, SettingsCommand, SettingsDraftState, SETTINGS_PREVIEW_SAMPLE_TEXT,
};
use super::tray::TrayMenuAction;
use crate::api::TtsEngine;
use crate::audio::AudioPlayer;
use crate::config::{set_windows_autostart, ConfigError, WindowPosition};
use eframe::egui;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

/// Edge-TTS voice for the Chinese click-to-replay (D2): a clear standard
/// female Mandarin voice, synthesized by the native Edge-TTS client.
pub(crate) const CHINESE_REPLAY_VOICE: &str = "zh-CN-XiaoxiaoNeural";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionSelectionTarget {
    LiveConfig,
    SettingsDraft,
}

impl super::Cap2VoiceApp {
    pub(crate) fn handle_toggle_mute(&mut self) {
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

    pub(crate) fn handle_select_voice(&mut self, voice: String) {
        self.state.selected_voice = voice.clone();
        self.config.voice.voice = voice.clone();
        self.persist_config();
        if let Some(handle) = &self.pipeline_handle {
            if let Err(err) = handle.set_voice(voice) {
                self.state.ui_notice = Some(err.to_string());
            }
        }
    }

    /// Replay the current Chinese line in Chinese (D2: click on the pinyin
    /// or Chinese line). Uses the A3 TTS cache first (the key includes the
    /// voice, so Chinese entries never collide with Vietnamese ones); on a
    /// miss synthesizes via the native Edge-TTS client and stores the
    /// result in the cache. An explicit user action, so it plays even
    /// while muted (mute only silences automatic Vietnamese reading).
    /// No-ops when no Chinese line is shown.
    pub(crate) fn handle_replay_chinese(&mut self, ctx: &egui::Context) {
        let chinese = self.state.pipeline.chinese_line.trim().to_string();
        if chinese.is_empty() {
            return;
        }
        let tts_engine = Arc::clone(&self.tts_engine);

        let voice = CHINESE_REPLAY_VOICE;
        let rate = self.config.voice.speech_rate.clone();
        let volume = self.config.voice.volume;

        // Fast path: cached audio for this exact (line, voice, rate).
        let tts_cache = match self.ensure_caches() {
            Ok((_, cache)) => Some(cache),
            Err(_) => None,
        };
        if let Some(cache) = &tts_cache {
            if let Some(mp3) = cache.get(&chinese, voice, &rate) {
                self.play_replay_audio(mp3, volume);
                return;
            }
        }

        if self.audio.is_none() {
            match AudioPlayer::new(volume) {
                Ok(player) => {
                    self.audio = Some(Arc::new(player));
                }
                Err(err) => {
                    self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                    return;
                }
            }
        }

        let (tx, rx) = mpsc::sync_channel(1);
        self.chinese_replay_rx = Some(rx);
        let ctx_wake = ctx.clone();
        self.runtime.spawn(async move {
            let res = tts_engine
                .synthesize(&chinese, voice, &rate)
                .await
                .map(|synthesis| {
                    if let Some(cache) = &tts_cache {
                        let _ = cache.put(&chinese, voice, &rate, synthesis.audio.clone());
                    }
                    TtsPreviewAudio {
                        mp3_bytes: synthesis.audio,
                        volume,
                    }
                });
            let _ = tx.send(res);
            ctx_wake.request_repaint();
        });
        ctx.request_repaint();
    }

    /// Ensure the audio player exists and enqueue already-synthesized MP3
    /// bytes for immediate playback (cache-hit path of click-to-replay).
    fn play_replay_audio(&mut self, mp3_bytes: Vec<u8>, volume: f32) {
        if self.audio.is_none() {
            match AudioPlayer::new(volume) {
                Ok(player) => {
                    self.audio = Some(Arc::new(player));
                }
                Err(err) => {
                    self.state.ui_notice = Some(format!("Lỗi âm thanh: {err}"));
                    return;
                }
            }
        }
        if let Some(audio) = &self.audio {
            let play_res = audio
                .set_volume(volume)
                .and_then(|()| audio.enqueue_mp3(mp3_bytes));
            if let Err(err) = play_res {
                self.state.ui_notice = Some(format!("Lỗi phát âm thanh: {err}"));
            }
        }
    }

    pub(crate) fn open_region_selector(
        &mut self,
        ctx: &egui::Context,
        target: RegionSelectionTarget,
    ) {
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

    pub(crate) fn finish_region_selection(
        &mut self,
        ctx: &egui::Context,
        outcome: RegionSelectorOutcome,
    ) {
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

    pub(crate) fn open_settings_window(&mut self, ctx: &egui::Context) {
        self.state.settings_panel_open = false;
        if self.settings_draft.is_none() {
            let mut draft = SettingsDraftState::from_config(&self.config);
            // Snapshot the live translator status for the Dịch tab
            // (D1+A5): the engine selector edits the draft; the status
            // line shows what the running translator reports right now.
            draft.translator_status = self.translator.as_ref().map(|t| t.status());
            self.settings_draft = Some(draft);
        }
        self.settings_focused = false;
        ctx.request_repaint();
    }

    pub(crate) fn handle_settings_command(&mut self, ctx: &egui::Context, cmd: SettingsCommand) {
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
                self.tts_preview_rx = None;
                self.settings_focused = false;
                ctx.request_repaint();
            }
            SettingsCommand::ClearCache => {
                // Clear the translation + TTS caches (A3 / gh #2). Loads
                // them first if the pipeline never started, so the button
                // also clears disk entries from earlier runs.
                match self.ensure_caches() {
                    Ok((translation, tts)) => {
                        let _ = translation.clear();
                        let _ = tts.clear();
                        self.state.ui_notice = Some("Đã xóa bộ nhớ đệm".to_string());
                    }
                    Err(err) => {
                        self.state.ui_notice = Some(format!("Không xóa được bộ nhớ đệm: {err}"));
                    }
                }
                ctx.request_repaint();
            }
            SettingsCommand::SelectRegion => {
                self.open_region_selector(ctx, RegionSelectionTarget::SettingsDraft);
            }
            SettingsCommand::PreviewVoice {
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
                let tts_engine = Arc::clone(&self.tts_engine);
                let (tx, rx) = mpsc::sync_channel(1);
                self.tts_preview_rx = Some(rx);
                let ctx_wake = ctx.clone();
                self.runtime.spawn(async move {
                    let res = tts_engine
                        .synthesize(SETTINGS_PREVIEW_SAMPLE_TEXT, &voice, &rate)
                        .await
                        .map(|synthesis| TtsPreviewAudio {
                            mp3_bytes: synthesis.audio,
                            volume,
                        });
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
                        if effects.engine_changed {
                            // Runtime engine switch (no restart): the
                            // pipeline holds the same `Arc`, so the next
                            // translation already uses the new engine.
                            if let Some(translator) = &self.translator {
                                translator.set_engine(self.config.translation.engine);
                            }
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

    pub(crate) fn show_overlay(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.overlay_visible = true;
        ctx.request_repaint();
    }

    pub(crate) fn hide_overlay(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.overlay_visible = false;
        ctx.request_repaint();
    }

    pub(crate) fn dispatch_tray_action(&mut self, ctx: &egui::Context, action: TrayMenuAction) {
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

    pub(crate) fn dispatch_hotkey_action(&mut self, ctx: &egui::Context, action: HotkeyAction) {
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

    pub(crate) fn sync_tray_menu_state(&mut self) {
        let running = self.state.is_running();
        if self.last_tray_running_state != Some(running) {
            self.last_tray_running_state = Some(running);
            if let Some(tray) = &self.tray {
                tray.set_running(running);
            }
        }
    }

    pub(crate) fn reregister_hotkeys(&mut self) {
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

    pub(crate) fn dispatch_commands(&mut self, ctx: &egui::Context, commands: Vec<OverlayCommand>) {
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
                OverlayCommand::ReplayChinese => self.handle_replay_chinese(ctx),
                OverlayCommand::CloseWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    pub(crate) fn check_wda_fallback(&mut self, frame: &eframe::Frame, ui: &egui::Ui) {
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
