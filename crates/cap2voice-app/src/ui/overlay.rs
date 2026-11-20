//! Floating 2-line subtitle overlay UI, pure state reducer, drag-position persistence,
//! and Windows `WDA_EXCLUDEFROMCAPTURE` capture self-exclusion.

use crate::config::{AppConfig, CaptureRegion, WindowPosition};
use crate::pipeline::{PipelineErrorKind, PipelineSnapshot, PipelineStatus};
use crate::ui::fonts::{opacity_to_alpha, parse_hex_color};
use eframe::egui;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Default floating overlay dimensions in logical pixels.
pub const DEFAULT_OVERLAY_SIZE: [f32; 2] = [620.0, 178.0];

/// Chinese subtitle line scale factor relative to `base_font_size_px` (`docs/spec.md` §3.5).
pub const CHINESE_FONT_SCALE: f32 = 1.35;

/// Built-in Edge-TTS Vietnamese voice options (`id`, Vietnamese UI label).
pub const AVAILABLE_VOICES: &[(&str, &str)] = &[
    ("vi-VN-NamMinhNeural", "vi-VN-NamMinhNeural (Nam)"),
    ("vi-VN-HoaiMyNeural", "vi-VN-HoaiMyNeural (Nữ)"),
];

/// Debounce interval before persisting dragged `(x, y)` coordinates to `config.toml`.
pub const POSITION_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Local Python server readiness status shown in the overlay header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerUiStatus {
    Starting,
    Ready,
    Failed(String),
}

/// Commands emitted by `render_overlay_ui` back to the `eframe` shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayCommand {
    StartPipeline,
    StopPipeline,
    ToggleMute,
    SelectVoice(String),
    ToggleRegionPanel,
    ToggleSettingsPanel,
    CloseWindow,
}

/// Typed errors emitted when applying `WDA_EXCLUDEFROMCAPTURE` to the overlay HWND.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DisplayAffinityError {
    #[error("không lấy được HWND của cửa sổ overlay")]
    MissingHwnd,

    #[error("SetWindowDisplayAffinity thất bại (Win32 code {code})")]
    Win32 { code: u32 },

    #[error("WDA_EXCLUDEFROMCAPTURE chỉ hỗ trợ trên Windows")]
    UnsupportedPlatform,
}

/// Pure UI state model for the floating 2-line subtitle overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayState {
    pub server_status: ServerUiStatus,
    pub pipeline: PipelineSnapshot,
    pub muted: bool,
    pub selected_voice: String,
    pub ui_notice: Option<String>,
    pub region_panel_open: bool,
    pub settings_panel_open: bool,
    pub exclude_from_capture_ok: bool,
    pub startup_warnings: Vec<String>,
}

impl OverlayState {
    pub fn from_config(config: &AppConfig, startup_warnings: Vec<String>) -> Self {
        Self {
            server_status: ServerUiStatus::Starting,
            pipeline: PipelineSnapshot::default(),
            muted: false,
            selected_voice: config.voice.voice.clone(),
            ui_notice: None,
            region_panel_open: false,
            settings_panel_open: false,
            exclude_from_capture_ok: false,
            startup_warnings,
        }
    }

    pub fn is_running(&self) -> bool {
        self.pipeline.status == PipelineStatus::Running
    }

    pub fn can_start(&self) -> bool {
        matches!(self.server_status, ServerUiStatus::Ready) && !self.is_running()
    }

    pub fn can_stop(&self) -> bool {
        self.is_running()
    }

    pub fn status_label(&self) -> &str {
        match &self.server_status {
            ServerUiStatus::Starting => "Đang khởi động server...",
            ServerUiStatus::Failed(_) => "Lỗi server",
            ServerUiStatus::Ready => {
                if self.is_running() {
                    "Đang dịch"
                } else {
                    "Đang chờ"
                }
            }
        }
    }

    pub fn status_color(&self) -> egui::Color32 {
        if self.active_error_banner().is_some() {
            return egui::Color32::from_rgb(239, 68, 68);
        }
        match &self.server_status {
            ServerUiStatus::Starting => egui::Color32::from_rgb(250, 204, 21),
            ServerUiStatus::Failed(_) => egui::Color32::from_rgb(239, 68, 68),
            ServerUiStatus::Ready => {
                if self.is_running() {
                    egui::Color32::from_rgb(74, 222, 128)
                } else {
                    egui::Color32::from_rgb(148, 163, 184)
                }
            }
        }
    }

    /// Synchronize overlay state with a newly published `PipelineSnapshot`.
    pub fn apply_pipeline_snapshot(&mut self, snapshot: PipelineSnapshot) {
        if snapshot.error_banner.is_some() || !snapshot.vietnamese_line.is_empty() {
            self.ui_notice = None;
        }
        self.pipeline = snapshot;
    }

    /// Validate preconditions for clicking `Bắt đầu`; sets explicit Vietnamese notice when invalid.
    pub fn request_start(&mut self, has_region: bool) -> bool {
        if !matches!(self.server_status, ServerUiStatus::Ready) {
            self.ui_notice = Some("Server dịch/TTS chưa sẵn sàng".to_string());
            return false;
        }
        if !has_region {
            self.region_panel_open = true;
            self.ui_notice = Some(
                "Chưa chọn vùng phụ đề — hãy bấm 'Chọn vùng' hoặc cấu hình tọa độ trước."
                    .to_string(),
            );
            return false;
        }
        self.ui_notice = None;
        true
    }

    /// Return the highest-priority visible Vietnamese error banner, if any.
    pub fn active_error_banner(&self) -> Option<String> {
        if let ServerUiStatus::Failed(err) = &self.server_status {
            return Some(err.clone());
        }
        if let Some(banner) = &self.pipeline.error_banner {
            return Some(banner.message.clone());
        }
        if let Some(notice) = &self.ui_notice {
            return Some(notice.clone());
        }
        None
    }
}

/// Debounces native window move updates so dragging persists cleanly without disk churn.
#[derive(Debug, Clone)]
pub struct PositionTracker {
    last_saved: Option<WindowPosition>,
    pending: Option<WindowPosition>,
    last_moved_at: Option<Instant>,
    debounce: Duration,
}

impl PositionTracker {
    pub fn new(initial: Option<WindowPosition>, debounce: Duration) -> Self {
        Self {
            last_saved: initial,
            pending: None,
            last_moved_at: None,
            debounce,
        }
    }

    /// Record the latest observed outer window origin `(x, y)` in logical points.
    pub fn observe(&mut self, x: f32, y: f32, now: Instant) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        if let Some(prev) = self.pending.as_ref().or(self.last_saved.as_ref()) {
            if (x - prev.x).abs() < 1.0 && (y - prev.y).abs() < 1.0 {
                return;
            }
        }
        self.pending = Some(WindowPosition { x, y });
        self.last_moved_at = Some(now);
    }

    /// Return `Some(pos)` once the window has remained stationary for at least `debounce`.
    pub fn take_ready_to_save(&mut self, now: Instant) -> Option<WindowPosition> {
        let moved_at = self.last_moved_at?;
        if now.duration_since(moved_at) >= self.debounce {
            self.pending.clone()
        } else {
            None
        }
    }

    /// Return any unsaved pending coordinates (used on clean shutdown).
    pub fn take_pending(&mut self) -> Option<WindowPosition> {
        self.last_moved_at = None;
        self.pending.take()
    }

    pub fn mark_saved(&mut self, pos: WindowPosition) {
        self.last_saved = Some(pos);
        self.pending = None;
        self.last_moved_at = None;
    }
}

/// Check whether the overlay outer rectangle overlaps the saved physical capture region.
pub fn overlay_overlaps_region(
    overlay_pos_logical: WindowPosition,
    overlay_size_logical: [f32; 2],
    pixels_per_point: f32,
    region: &CaptureRegion,
) -> bool {
    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let ov_left = overlay_pos_logical.x * scale;
    let ov_top = overlay_pos_logical.y * scale;
    let ov_right = ov_left + overlay_size_logical[0].max(0.0) * scale;
    let ov_bottom = ov_top + overlay_size_logical[1].max(0.0) * scale;

    let reg_left = region.x as f32;
    let reg_top = region.y as f32;
    let reg_right = reg_left + region.width as f32;
    let reg_bottom = reg_top + region.height as f32;

    ov_left < reg_right && ov_right > reg_left && ov_top < reg_bottom && ov_bottom > reg_top
}

/// Apply `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` on Windows 10 2004+.
pub fn apply_exclude_from_capture_hwnd(hwnd: isize) -> Result<(), DisplayAffinityError> {
    if hwnd == 0 {
        return Err(DisplayAffinityError::MissingHwnd);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{GetLastError, HWND};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        };
        let ok = unsafe { SetWindowDisplayAffinity(hwnd as HWND, WDA_EXCLUDEFROMCAPTURE) };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(DisplayAffinityError::Win32 { code });
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(DisplayAffinityError::UnsupportedPlatform)
    }
}

fn action_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).size(13.0))
        .corner_radius(egui::CornerRadius::same(8))
        .fill(egui::Color32::from_rgba_unmultiplied(40, 44, 52, 220))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
        ))
}

fn render_inline_info_panels(ui: &mut egui::Ui, state: &mut OverlayState, config: &AppConfig) {
    if state.region_panel_open {
        egui::Frame::new()
            .fill(egui::Color32::from_rgba_unmultiplied(30, 34, 42, 225))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(239, 68, 68, 110),
            ))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let summary = match &config.general.region {
                        Some(r) => format!(
                            "Vùng phụ đề ({}): {}, {} · {} × {} px",
                            r.monitor_id, r.x, r.y, r.width, r.height
                        ),
                        None => {
                            "Vùng phụ đề: Chưa chọn vùng — bấm 'Chọn vùng' để kéo chọn trên màn hình"
                                .to_string()
                        }
                    };
                    ui.label(
                        egui::RichText::new(summary)
                            .size(12.0)
                            .color(egui::Color32::from_rgb(226, 232, 240)),
                    );
                    if ui.small_button("Đóng").clicked() {
                        state.region_panel_open = false;
                    }
                });
            });
        ui.add_space(6.0);
    }

    if state.settings_panel_open {
        egui::Frame::new()
            .fill(egui::Color32::from_rgba_unmultiplied(30, 34, 42, 225))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(148, 163, 184, 90),
            ))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let summary = format!(
                        "Cài đặt nhanh (cửa sổ 5 tab ở Task 12): {} fps · {} · âm lượng {}% · chữ {} px · độ mờ {}%",
                        config.general.capture_fps,
                        state.selected_voice,
                        (config.voice.volume * 100.0).round() as u32,
                        config.display.font_size_px.round() as u32,
                        (config.display.overlay_opacity * 100.0).round() as u32,
                    );
                    ui.label(
                        egui::RichText::new(summary)
                            .size(12.0)
                            .color(egui::Color32::from_rgb(203, 213, 225)),
                    );
                    if ui.small_button("Đóng").clicked() {
                        state.settings_panel_open = false;
                    }
                });
            });
        ui.add_space(6.0);
    }
}

/// Render the borderless rounded dark semi-transparent 2-line subtitle overlay matching Mockup 1.
pub fn render_overlay_ui(
    ui: &mut egui::Ui,
    state: &mut OverlayState,
    config: &AppConfig,
    font_warnings: &[String],
) -> Vec<OverlayCommand> {
    let mut commands = Vec::new();
    let alpha = opacity_to_alpha(config.display.overlay_opacity);
    let bg_color = egui::Color32::from_rgba_unmultiplied(20, 22, 26, alpha);
    let border_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 36);

    let mut start_drag = false;

    egui::Frame::new()
        .fill(bg_color)
        .stroke(egui::Stroke::new(1.0, border_color))
        .corner_radius(egui::CornerRadius::same(16))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());

            // Compact top header strip: draggable status area on left, Settings + Close on right.
            ui.horizontal(|ui| {
                let header_left = ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot_rect.center(), 4.5, state.status_color());
                    ui.label(
                        egui::RichText::new(state.status_label())
                            .size(12.0)
                            .color(egui::Color32::from_rgb(203, 213, 225)),
                    );
                });
                if header_left
                    .response
                    .interact(egui::Sense::drag())
                    .drag_started_by(egui::PointerButton::Primary)
                {
                    start_drag = true;
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(action_button("×"))
                        .on_hover_text("Đóng cửa sổ")
                        .clicked()
                    {
                        commands.push(OverlayCommand::CloseWindow);
                    }
                    if ui.add(action_button("Cài đặt")).clicked() {
                        commands.push(OverlayCommand::ToggleSettingsPanel);
                    }
                    let gap = ui.available_size();
                    if gap.x > 4.0 {
                        let (_rect, gap_resp) =
                            ui.allocate_exact_size(egui::vec2(gap.x, 22.0), egui::Sense::drag());
                        if gap_resp.drag_started_by(egui::PointerButton::Primary) {
                            start_drag = true;
                        }
                    }
                });
            });

            if let Some(banner_msg) = state.active_error_banner() {
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(69, 20, 27, 235))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(239, 68, 68)))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(banner_msg)
                                .size(12.5)
                                .color(egui::Color32::from_rgb(254, 202, 202)),
                        );
                    });
            } else if let Some(warning) = font_warnings
                .first()
                .or_else(|| state.startup_warnings.first())
            {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(warning)
                        .size(11.5)
                        .color(egui::Color32::from_rgb(253, 224, 71)),
                );
            }

            ui.add_space(8.0);
            render_inline_info_panels(ui, state, config);

            // Centered 2-line subtitle body (Chinese white above Vietnamese green).
            let zh_color =
                parse_hex_color(&config.display.chinese_color_hex).unwrap_or(egui::Color32::WHITE);
            let vi_color = parse_hex_color(&config.display.vietnamese_color_hex)
                .unwrap_or(egui::Color32::from_rgb(74, 222, 128));
            let vi_size = config.display.font_size_px.clamp(12.0, 40.0);
            let zh_size = vi_size * CHINESE_FONT_SCALE;

            let body_resp = ui.vertical_centered(|ui| {
                ui.set_min_width(ui.available_width());
                if config.display.show_chinese_line {
                    let (zh_text, color) = if state.pipeline.chinese_line.is_empty() {
                        (
                            "Chờ phụ đề tiếng Trung...",
                            egui::Color32::from_rgb(148, 163, 184),
                        )
                    } else {
                        (state.pipeline.chinese_line.as_str(), zh_color)
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(zh_text).size(zh_size).color(color))
                            .wrap(),
                    );
                }

                if config.display.show_vietnamese_line {
                    ui.add_space(4.0);
                    let is_translate_error =
                        state.pipeline.error_banner.as_ref().is_some_and(|b| {
                            matches!(
                                b.kind,
                                PipelineErrorKind::TranslateNetwork
                                    | PipelineErrorKind::TranslateRateLimited
                                    | PipelineErrorKind::TranslateServerUnavailable
                                    | PipelineErrorKind::TranslateOther
                            )
                        });
                    let (vi_text, color) = if !state.pipeline.vietnamese_line.is_empty() {
                        (state.pipeline.vietnamese_line.as_str(), vi_color)
                    } else if is_translate_error {
                        (
                            "[Lỗi dịch — kiểm tra kết nối mạng]",
                            egui::Color32::from_rgb(252, 165, 165),
                        )
                    } else {
                        (
                            "Bản dịch tiếng Việt sẽ hiển thị tại đây.",
                            egui::Color32::from_rgb(134, 239, 172),
                        )
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(vi_text).size(vi_size).color(color))
                            .wrap(),
                    );
                }
            });
            if body_resp
                .response
                .interact(egui::Sense::drag())
                .drag_started_by(egui::PointerButton::Primary)
            {
                start_drag = true;
            }

            ui.add_space(10.0);
            let sep_rect = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
            ui.painter().line_segment(
                [sep_rect.left_center(), sep_rect.right_center()],
                egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                ),
            );
            ui.add_space(8.0);

            // Bottom action bar: Chọn vùng | Bắt đầu | Dừng | Mute | Voice dropdown.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                if ui.add(action_button("Chọn vùng")).clicked() {
                    commands.push(OverlayCommand::ToggleRegionPanel);
                }
                if ui
                    .add_enabled(state.can_start(), action_button("Bắt đầu"))
                    .clicked()
                {
                    commands.push(OverlayCommand::StartPipeline);
                }
                if ui
                    .add_enabled(state.can_stop(), action_button("Dừng"))
                    .clicked()
                {
                    commands.push(OverlayCommand::StopPipeline);
                }

                let mute_label = if state.muted {
                    "Bật tiếng"
                } else {
                    "Tắt tiếng"
                };
                if ui.add(action_button(mute_label)).clicked() {
                    commands.push(OverlayCommand::ToggleMute);
                }

                let current_voice_label = AVAILABLE_VOICES
                    .iter()
                    .find(|(id, _)| *id == state.selected_voice)
                    .map(|(_, label)| *label)
                    .unwrap_or(state.selected_voice.as_str());

                let mut pending_voice = state.selected_voice.clone();
                egui::ComboBox::from_id_salt("overlay_voice_select")
                    .selected_text(egui::RichText::new(current_voice_label).size(12.5))
                    .width(205.0)
                    .show_ui(ui, |ui| {
                        for (voice_id, label) in AVAILABLE_VOICES {
                            ui.selectable_value(
                                &mut pending_voice,
                                (*voice_id).to_string(),
                                *label,
                            );
                        }
                    });
                if pending_voice != state.selected_voice {
                    commands.push(OverlayCommand::SelectVoice(pending_voice));
                }
            });
        });

    if start_drag {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    commands
}

#[cfg(test)]
mod tests {
    use super::{
        apply_exclude_from_capture_hwnd, overlay_overlaps_region, DisplayAffinityError,
        OverlayState, PositionTracker, ServerUiStatus,
    };
    use crate::config::{AppConfig, CaptureRegion, WindowPosition};
    use crate::pipeline::{
        PipelineErrorBanner, PipelineErrorKind, PipelineMetrics, PipelineSnapshot, PipelineStatus,
        ERR_BANNER_TRANSLATE_NETWORK,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn overlay_state_controls_reflect_server_and_pipeline_lifecycle() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());

        assert!(!state.can_start());
        assert!(!state.can_stop());
        assert_eq!(state.status_label(), "Đang khởi động server...");

        state.server_status = ServerUiStatus::Ready;
        assert!(state.can_start());
        assert!(!state.can_stop());
        assert_eq!(state.status_label(), "Đang chờ");

        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 1,
            chinese_line: "夜深了，雨还在下。".to_string(),
            vietnamese_line: "Đêm đã khuya, mưa vẫn còn rơi.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
        });
        assert!(!state.can_start());
        assert!(state.can_stop());
        assert_eq!(state.status_label(), "Đang dịch");

        state.server_status = ServerUiStatus::Failed("Python missing".to_string());
        state.pipeline.status = PipelineStatus::Idle;
        assert!(!state.can_start());
        assert!(!state.can_stop());
        assert_eq!(state.status_label(), "Lỗi server");
        assert!(state
            .active_error_banner()
            .as_deref()
            .is_some_and(|msg| msg.contains("Python missing")));
    }

    #[test]
    fn overlay_state_start_without_region_sets_vietnamese_prompt() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        state.server_status = ServerUiStatus::Ready;

        assert!(!state.request_start(false));
        assert!(state.region_panel_open);
        assert!(state
            .active_error_banner()
            .as_deref()
            .is_some_and(|msg| msg.contains("Chọn vùng")));

        assert!(state.request_start(true));
        assert_eq!(state.ui_notice, None);
    }

    #[test]
    fn overlay_state_error_banner_surfaces_and_clears_on_recovery() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        state.server_status = ServerUiStatus::Ready;
        state.ui_notice = Some("Thông báo tạm".to_string());

        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 1,
            chinese_line: "第二句。".to_string(),
            vietnamese_line: String::new(),
            error_banner: Some(PipelineErrorBanner {
                kind: PipelineErrorKind::TranslateNetwork,
                message: ERR_BANNER_TRANSLATE_NETWORK.to_string(),
            }),
            metrics: PipelineMetrics::default(),
        });
        assert_eq!(
            state.active_error_banner().as_deref(),
            Some(ERR_BANNER_TRANSLATE_NETWORK)
        );
        assert_eq!(state.pipeline.vietnamese_line, "");

        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 2,
            chinese_line: "第二句。".to_string(),
            vietnamese_line: "Câu thứ hai.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
        });
        assert_eq!(state.active_error_banner(), None);
        assert_eq!(state.pipeline.vietnamese_line, "Câu thứ hai.");
    }

    #[test]
    fn overlay_state_voice_switch_and_mute_toggle_update_cleanly() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        assert_eq!(state.selected_voice, "vi-VN-NamMinhNeural");
        assert!(!state.muted);

        state.muted = true;
        state.selected_voice = "vi-VN-HoaiMyNeural".to_string();
        assert!(state.muted);
        assert_eq!(state.selected_voice, "vi-VN-HoaiMyNeural");
    }

    #[test]
    fn overlay_overlaps_region_respects_dpi_scale_factor() {
        let region = CaptureRegion {
            monitor_id: "DISPLAY1".to_string(),
            x: 300,
            y: 900,
            width: 800,
            height: 80,
        };
        assert!(!overlay_overlaps_region(
            WindowPosition { x: 100.0, y: 100.0 },
            [620.0, 178.0],
            1.0,
            &region
        ));
        // Logical x=250, y=500, size=620x178 at 150% DPI -> physical [375..1305, 750..1017] -> overlaps!
        assert!(overlay_overlaps_region(
            WindowPosition { x: 250.0, y: 500.0 },
            [620.0, 178.0],
            1.5,
            &region
        ));
    }

    #[test]
    fn position_tracker_debounces_drag_and_commits_after_delay() {
        let t0 = Instant::now();
        let initial = WindowPosition { x: 100.0, y: 200.0 };
        let mut tracker = PositionTracker::new(Some(initial.clone()), Duration::from_millis(400));

        tracker.observe(100.2, 200.3, t0);
        assert_eq!(
            tracker.take_ready_to_save(t0 + Duration::from_millis(500)),
            None
        );

        tracker.observe(240.0, 480.0, t0 + Duration::from_millis(600));
        assert_eq!(
            tracker.take_ready_to_save(t0 + Duration::from_millis(900)),
            None
        );

        let ready = tracker.take_ready_to_save(t0 + Duration::from_millis(1050));
        assert_eq!(ready, Some(WindowPosition { x: 240.0, y: 480.0 }));
        if let Some(pos) = ready {
            tracker.mark_saved(pos);
        }
        assert_eq!(tracker.take_pending(), None);
        assert_ne!(initial, WindowPosition { x: 240.0, y: 480.0 });
    }

    #[test]
    fn apply_exclude_from_capture_rejects_null_hwnd() {
        assert_eq!(
            apply_exclude_from_capture_hwnd(0),
            Err(DisplayAffinityError::MissingHwnd)
        );
    }

    #[test]
    fn render_overlay_ui_draws_two_lines_and_controls_headlessly_without_panic() {
        let ctx = eframe::egui::Context::default();
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        state.server_status = ServerUiStatus::Ready;
        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 1,
            chinese_line: "夜深了，雨还在下。".to_string(),
            vietnamese_line: "Đêm đã khuya, mưa vẫn còn rơi.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
        });

        let raw_input = eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                eframe::egui::vec2(620.0, 178.0),
            )),
            ..Default::default()
        };
        let full_output = ctx.run_ui(raw_input, |ui| {
            let cmds = super::render_overlay_ui(ui, &mut state, &cfg, &[]);
            assert!(cmds.is_empty());
        });
        assert!(!full_output.shapes.is_empty());
    }
}
