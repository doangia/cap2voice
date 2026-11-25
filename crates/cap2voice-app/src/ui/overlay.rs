//! Floating 2-line subtitle overlay UI, pure state reducer, drag-position persistence,
//! and Windows `WDA_EXCLUDEFROMCAPTURE` capture self-exclusion.

use crate::config::{AppConfig, CaptureRegion, WindowPosition, WindowSize};
use crate::pipeline::{PipelineErrorKind, PipelineSnapshot, PipelineStatus, SubtitlePair};
use crate::ui::fonts::{opacity_to_alpha, parse_hex_color};
use crate::ui::icons::{IconButton, UiIcon};
use eframe::egui;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Default floating overlay dimensions in logical pixels.
/// Height grew from 178 to 204 in D2 to fit the pinyin line above the
/// Chinese line (pinyin ~18 px + spacing).
///
/// Fixed-size overlay (2026-11-25): this size is set once at startup
/// (or restored from `config.display.overlay_size`) and then only
/// changes when the user drags the resize grip. The window NEVER
/// resizes with the subtitle content; the text shrinks to fit instead.
/// (This supersedes the 24/11 2-line fix, which grew the window to
/// ~259 px for two-line subtitles — both lines still display, just
/// smaller when needed.)
pub const DEFAULT_OVERLAY_SIZE: [f32; 2] = [620.0, 204.0];

/// Minimum overlay window size (also the viewport `min_inner_size`).
pub const OVERLAY_MIN_SIZE: [f32; 2] = [460.0, 140.0];

/// Maximum overlay window size for the resize grip.
pub const OVERLAY_MAX_SIZE: [f32; 2] = [1200.0, 600.0];

/// Minimum readable subtitle font size (also the `font_size_px` config
/// clamp floor): subtitle text never shrinks below this.
pub const MIN_SUBTITLE_FONT_SIZE: f32 = 12.0;

/// Vertical space reserved below the subtitle body for the fixed chrome:
/// separator block (10 + 1 + 8) + action bar (28) + grip row (4 + 14)
/// = 65 px. The body fit budget is `available_height - this`.
pub const OVERLAY_FOOTER_RESERVE: f32 = 65.0;

/// Opacity factor for subtitle-history lines (2026-11-25): history text
/// renders at 55% of the current lines' alpha so it reads as clearly
/// dimmer, not as a second current line.
pub const HISTORY_DIM_ALPHA: f32 = 0.55;

/// Maximum number of completed subtitle pairs the pipeline keeps for
/// the history display (the setting offers 0, 1, or 2).
pub const MAX_HISTORY_PAIRS: usize = 2;

/// One text block the subtitle-fit pass lays out: a (possibly multi-line)
/// piece of the overlay body with its base font size and fixed spacing.
/// Blocks render top-to-bottom in list order.
#[derive(Debug, Clone, PartialEq)]
pub struct FitBlock {
    /// Text to lay out (`\n` separates visual lines).
    pub text: String,
    /// Font size at scale 1.0.
    pub base_size: f32,
    /// Fixed gap above this block when it is not the first shown block.
    pub space_before: f32,
    /// Drop order when even the floor size does not fit: lower drops
    /// first (oldest history, then pinyin, then current lines). Blocks
    /// sharing an order drop together before the next order.
    pub drop_order: u8,
    /// When true the whole block drops at once (history pairs, pinyin);
    /// when false leading lines drop one at a time (current lines).
    pub drop_whole: bool,
}

/// Result of the subtitle-fit pass: the uniform scale for every shown
/// block and the text each block renders (`None` when the whole block
/// was dropped).
#[derive(Debug, Clone, PartialEq)]
pub struct FitResult {
    pub scale: f32,
    pub texts: Vec<Option<String>>,
    /// Whether any line or block was dropped to fit.
    pub truncated: bool,
}

/// Choose a uniform scale for `blocks` so all of them fit in
/// `budget_height` (laid-out height at the given `measure`, which must
/// measure real wrapped text height at a font size — in the overlay it
/// is `ui.painter().layout`, so a long single line counts at its
/// wrapped height, not its character count).
///
/// The scale is uniform: every shown block shrinks by the same factor,
/// never below `floor_scale` (the floor is `MIN_SUBTITLE_FONT_SIZE /
/// base Vietnamese size`). When even the floor does not fit, content
/// drops in `drop_order` order (oldest history first, then pinyin,
/// then the leading lines of the current lines, oldest first); a block
/// that lost leading lines gets a "…" prefix on its first remaining
/// line. The last remaining line is never dropped: if nothing fits,
/// that one line renders (clipped) and the window still never grows.
pub fn fit_subtitle_blocks(
    blocks: &[FitBlock],
    floor_scale: f32,
    budget_height: f32,
    measure: &dyn Fn(&str, f32) -> f32,
) -> FitResult {
    let floor_scale = floor_scale.clamp(0.0, 1.0);
    // Kept lines per block; `dropped_from_front[i]` counts lines dropped
    // from the front of block i (for the "…" marker).
    let mut kept: Vec<Vec<String>> = blocks
        .iter()
        .map(|b| b.text.split('\n').map(str::to_string).collect())
        .collect();
    let mut dropped_from_front = vec![0usize; blocks.len()];

    // Rendered text of one block: kept lines joined, prefixed with "…"
    // when leading lines were dropped. `None` when the block is gone.
    let rendered = |kept: &[Vec<String>], dropped_from_front: &[usize], idx: usize| {
        if kept[idx].is_empty() {
            return None;
        }
        let mut text = kept[idx].join("\n");
        if dropped_from_front[idx] > 0 {
            text.insert(0, '…');
        }
        Some(text)
    };

    // Total laid-out height of the kept blocks at `scale`, including the
    // gap above each shown block except the first.
    let total_height = |kept: &[Vec<String>], dropped_from_front: &[usize], scale: f32| {
        let mut total = 0.0;
        let mut first_shown = true;
        for (idx, block) in blocks.iter().enumerate() {
            let Some(text) = rendered(kept, dropped_from_front, idx) else {
                continue;
            };
            if !first_shown {
                total += block.space_before;
            }
            first_shown = false;
            total += measure(&text, block.base_size * scale);
        }
        total
    };

    // Largest scale in [floor, 1.0] that fits (24 binary-search steps).
    // `measure` is non-decreasing in font size (bigger text wraps to the
    // same or more lines), so the fit set is a single interval.
    let best_scale = |kept: &[Vec<String>], dropped_from_front: &[usize]| {
        if total_height(kept, dropped_from_front, 1.0) <= budget_height {
            return 1.0;
        }
        let mut lo = floor_scale;
        let mut hi = 1.0f32;
        for _ in 0..24 {
            let mid = (lo + hi) / 2.0;
            if total_height(kept, dropped_from_front, mid) <= budget_height {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };

    let mut truncated = false;
    if total_height(&kept, &dropped_from_front, floor_scale) > budget_height {
        // Even the floor size does not fit: drop content, lowest
        // drop_order first, until it fits or nothing droppable remains.
        loop {
            if total_height(&kept, &dropped_from_front, floor_scale) <= budget_height {
                break;
            }
            let total_lines: usize = kept.iter().map(Vec::len).sum();
            if total_lines <= 1 {
                break; // Never drop the last remaining line.
            }
            // Lowest drop_order among blocks that can still drop: a
            // whole-drop block with any line, or a line-drop block with
            // more than one line (a block's last line never drops — the
            // current Chinese line must not vanish while its Vietnamese
            // translation is still shown).
            let Some(order) = blocks
                .iter()
                .enumerate()
                .filter(|(idx, block)| {
                    !kept[*idx].is_empty() && (block.drop_whole || kept[*idx].len() > 1)
                })
                .map(|(_, block)| block.drop_order)
                .min()
            else {
                break;
            };
            // Whole-drop blocks at this order go together (a history
            // pair's Chinese + Vietnamese lines drop as one unit, so a
            // Vietnamese line never survives without its Chinese line).
            let whole: Vec<usize> = blocks
                .iter()
                .enumerate()
                .filter(|(idx, block)| {
                    block.drop_order == order && block.drop_whole && !kept[*idx].is_empty()
                })
                .map(|(idx, _)| idx)
                .collect();
            if !whole.is_empty() {
                let remaining_after: usize = kept
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| !whole.contains(idx))
                    .map(|(_, lines)| lines.len())
                    .sum();
                if remaining_after == 0 {
                    break;
                }
                for idx in whole {
                    kept[idx].clear();
                }
            } else {
                let Some(idx) = blocks.iter().enumerate().find_map(|(idx, block)| {
                    (block.drop_order == order && kept[idx].len() > 1).then_some(idx)
                }) else {
                    break;
                };
                kept[idx].remove(0);
                dropped_from_front[idx] += 1;
            }
            truncated = true;
        }
    }

    // The survivors may fit larger than the floor now that content was
    // dropped: scale them back up to fill the budget.
    let scale = best_scale(&kept, &dropped_from_front);
    let texts = (0..blocks.len())
        .map(|idx| rendered(&kept, &dropped_from_front, idx))
        .collect();
    FitResult {
        scale,
        texts,
        truncated,
    }
}

/// Dim a text color for history lines: same hue, `HISTORY_DIM_ALPHA` of
/// the alpha, so history reads as clearly dimmer than the current lines.
pub fn dim_color(color: egui::Color32) -> egui::Color32 {
    let [r, g, b, a] = color.to_array();
    egui::Color32::from_rgba_unmultiplied(r, g, b, ((a as f32) * HISTORY_DIM_ALPHA) as u8)
}

/// Clamp a resize-grip target size to the allowed overlay bounds.
pub fn clamp_overlay_resize(size: [f32; 2]) -> [f32; 2] {
    [
        size[0].clamp(OVERLAY_MIN_SIZE[0], OVERLAY_MAX_SIZE[0]),
        size[1].clamp(OVERLAY_MIN_SIZE[1], OVERLAY_MAX_SIZE[1]),
    ]
}

/// Chinese subtitle line scale factor relative to `base_font_size_px` (`docs/spec.md` §3.5).
pub const CHINESE_FONT_SCALE: f32 = 1.35;

/// Pinyin line scale factor relative to the Chinese line size (D2).
pub const PINYIN_FONT_SCALE: f32 = 0.72;

/// Pinyin line text color (muted slate, distinct from the white Chinese line).
pub const PINYIN_TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(148, 163, 184);

/// Debounce interval before persisting dragged `(x, y)` coordinates to `config.toml`.
pub const POSITION_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Native translation engine readiness status shown in the overlay
/// header (D1+A5: there is no server anymore). `Starting` means the
/// Helsinki model is still loading in the background (translations fall
/// back to Vietphrase meanwhile); `Ready` means at least one engine can
/// translate; `Failed` means no engine is available at all.
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
    ToggleRegionPanel,
    ToggleSettingsPanel,
    CloseWindow,
    /// Replay the current Chinese line in Chinese (click on the pinyin or
    /// Chinese line, D2). The shell no-ops when no Chinese line is shown.
    ReplayChinese,
    /// Replay the current Vietnamese line in Vietnamese (click on the
    /// Vietnamese line). Uses the CURRENT TTS engine — Piper when Piper is
    /// the active engine — unlike `ReplayChinese`, which always uses
    /// Edge-TTS. No-ops when no Vietnamese line is shown.
    ReplayVietnamese,
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
    /// Tone-marked pinyin of `pipeline.chinese_line` (D2), recomputed in
    /// `apply_pipeline_snapshot` whenever the Chinese line changes.
    pub pinyin_line: String,
    /// Most recently completed subtitle pairs, most-recent-first (the
    /// pipeline keeps at most `MAX_HISTORY_PAIRS`). Copied from the
    /// snapshot in `apply_pipeline_snapshot`; the overlay shows the
    /// newest `config.display.history_lines` of them, dimmed, above
    /// the current subtitle.
    pub history: Vec<SubtitlePair>,
    pub muted: bool,
    pub ui_notice: Option<String>,
    pub region_panel_open: bool,
    pub settings_panel_open: bool,
    pub exclude_from_capture_ok: bool,
    pub startup_warnings: Vec<String>,
}

impl OverlayState {
    pub fn from_config(_config: &AppConfig, startup_warnings: Vec<String>) -> Self {
        Self {
            server_status: ServerUiStatus::Starting,
            pipeline: PipelineSnapshot::default(),
            pinyin_line: String::new(),
            history: Vec::new(),
            muted: false,
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
        // Starting only means the Helsinki model is still loading;
        // translations fall back to Vietphrase meanwhile, so starting
        // the pipeline is allowed. Only a failed engine blocks it.
        matches!(
            self.server_status,
            ServerUiStatus::Starting | ServerUiStatus::Ready
        ) && !self.is_running()
    }

    pub fn can_stop(&self) -> bool {
        self.is_running()
    }

    pub fn status_label(&self) -> &str {
        match &self.server_status {
            ServerUiStatus::Starting => "Đang tải model dịch…",
            ServerUiStatus::Failed(_) => "Lỗi engine dịch",
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
        if snapshot.chinese_line != self.pipeline.chinese_line {
            self.pinyin_line = crate::pinyin::to_pinyin_line(&snapshot.chinese_line);
        }
        self.history = snapshot.history.clone();
        self.pipeline = snapshot;
    }

    /// Validate preconditions for clicking `Bắt đầu`; sets explicit Vietnamese notice when invalid.
    pub fn request_start(&mut self, has_region: bool) -> bool {
        if matches!(self.server_status, ServerUiStatus::Failed(_)) {
            self.ui_notice = Some("Engine dịch chưa sẵn sàng — kiểm tra model/từ điển".to_string());
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

/// Debounces native window size updates so grip resizes persist cleanly
/// without disk churn (mirrors `PositionTracker`; fixed-size overlay,
/// 2026-11-25). The window size only changes via the resize grip, so a
/// size change here is always the user's own resize.
#[derive(Debug, Clone)]
pub struct SizeTracker {
    last_saved: Option<WindowSize>,
    pending: Option<WindowSize>,
    last_changed_at: Option<Instant>,
    debounce: Duration,
}

impl SizeTracker {
    pub fn new(initial: Option<WindowSize>, debounce: Duration) -> Self {
        Self {
            last_saved: initial,
            pending: None,
            last_changed_at: None,
            debounce,
        }
    }

    /// Record the latest observed inner window size `(width, height)` in
    /// logical points.
    pub fn observe(&mut self, width: f32, height: f32, now: Instant) {
        if !width.is_finite() || !height.is_finite() {
            return;
        }
        if let Some(prev) = self.pending.as_ref().or(self.last_saved.as_ref()) {
            if (width - prev.width).abs() < 1.0 && (height - prev.height).abs() < 1.0 {
                return;
            }
        }
        self.pending = Some(WindowSize { width, height });
        self.last_changed_at = Some(now);
    }

    /// Return `Some(size)` once the window has kept the same size for at
    /// least `debounce`.
    pub fn take_ready_to_save(&mut self, now: Instant) -> Option<WindowSize> {
        let changed_at = self.last_changed_at?;
        if now.duration_since(changed_at) >= self.debounce {
            self.pending
        } else {
            None
        }
    }

    /// Return any unsaved pending size (used on clean shutdown).
    pub fn take_pending(&mut self) -> Option<WindowSize> {
        self.last_changed_at = None;
        self.pending.take()
    }

    pub fn mark_saved(&mut self, size: WindowSize) {
        self.last_saved = Some(size);
        self.pending = None;
        self.last_changed_at = None;
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

/// Apply `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` to a
/// window found by exact title (used for the in-place subtitle viewport,
/// whose raw handle is not exposed to the `eframe` app code).
pub fn apply_exclude_from_capture_by_title(title: &str) -> Result<(), DisplayAffinityError> {
    if title.is_empty() {
        return Err(DisplayAffinityError::MissingHwnd);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        };
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
        if hwnd.is_null() {
            return Err(DisplayAffinityError::MissingHwnd);
        }
        let ok = unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(DisplayAffinityError::Win32 { code });
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        Err(DisplayAffinityError::UnsupportedPlatform)
    }
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
                    if ui
                        .add(IconButton::small(UiIcon::Close))
                        .on_hover_text("Đóng")
                        .clicked()
                    {
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
                        "Cài đặt hiện tại: {} fps · {} · âm lượng {}% · chữ {} px · độ mờ {}%",
                        config.general.capture_fps,
                        config.voice.voice,
                        (config.voice.volume * 100.0).round() as u32,
                        config.display.font_size_px.round() as u32,
                        (config.display.overlay_opacity * 100.0).round() as u32,
                    );
                    ui.label(
                        egui::RichText::new(summary)
                            .size(12.0)
                            .color(egui::Color32::from_rgb(203, 213, 225)),
                    );
                    if ui
                        .add(IconButton::small(UiIcon::Close))
                        .on_hover_text("Đóng")
                        .clicked()
                    {
                        state.settings_panel_open = false;
                    }
                });
            });
        ui.add_space(6.0);
    }
}

/// How one subtitle-fit block renders: its color and click action.
/// History blocks are view-only (no replay); current lines keep
/// click-to-replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyBlockKind {
    HistoryChinese,
    HistoryVietnamese,
    Pinyin,
    Chinese,
    Vietnamese,
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
                        .add(IconButton::new(UiIcon::Close))
                        .on_hover_text("Đóng")
                        .clicked()
                    {
                        commands.push(OverlayCommand::CloseWindow);
                    }
                    if ui
                        .add(IconButton::new(UiIcon::Settings))
                        .on_hover_text("Cài đặt")
                        .clicked()
                    {
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

            // Fixed-size overlay body (2026-11-25): the window never
            // resizes with the content. All visible lines — history
            // pairs (dimmed, oldest first) plus the current pinyin /
            // Chinese / Vietnamese — share one uniform scale chosen each
            // frame so they fit the window's content area.
            let zh_color =
                parse_hex_color(&config.display.chinese_color_hex).unwrap_or(egui::Color32::WHITE);
            let vi_color = parse_hex_color(&config.display.vietnamese_color_hex)
                .unwrap_or(egui::Color32::from_rgb(74, 222, 128));
            let vi_size = config.display.font_size_px.clamp(12.0, 40.0);
            let zh_size = vi_size * CHINESE_FONT_SCALE;

            // Current Vietnamese display text/color (placeholder or
            // error message when no translation is available yet).
            let is_translate_error = state.pipeline.error_banner.as_ref().is_some_and(|b| {
                matches!(
                    b.kind,
                    PipelineErrorKind::TranslateNetwork
                        | PipelineErrorKind::TranslateEngineUnavailable
                        | PipelineErrorKind::TranslateOther
                )
            });
            let (vi_text, vi_display_color) = if !state.pipeline.vietnamese_line.is_empty() {
                (state.pipeline.vietnamese_line.clone(), vi_color)
            } else if is_translate_error {
                (
                    "[Lỗi dịch — kiểm tra kết nối mạng]".to_string(),
                    egui::Color32::from_rgb(252, 165, 165),
                )
            } else {
                (
                    "Bản dịch tiếng Việt sẽ hiển thị tại đây.".to_string(),
                    egui::Color32::from_rgb(134, 239, 172),
                )
            };
            let (zh_text, zh_display_color) = if state.pipeline.chinese_line.is_empty() {
                (
                    "Chờ phụ đề tiếng Trung...".to_string(),
                    egui::Color32::from_rgb(148, 163, 184),
                )
            } else {
                (state.pipeline.chinese_line.clone(), zh_color)
            };

            // Fit blocks, top-to-bottom: history pairs (oldest first),
            // then the current pinyin / Chinese / Vietnamese.
            let history_count = (config.display.history_lines as usize).min(MAX_HISTORY_PAIRS);
            // `state.history` is most-recent-first; display oldest first.
            let shown_history: Vec<&SubtitlePair> =
                state.history.iter().take(history_count).rev().collect();
            let mut blocks: Vec<FitBlock> = Vec::new();
            let mut kinds: Vec<BodyBlockKind> = Vec::new();
            for (pair_idx, pair) in shown_history.iter().enumerate() {
                let order = pair_idx as u8; // Oldest pair drops first.
                blocks.push(FitBlock {
                    text: pair.chinese.clone(),
                    base_size: zh_size,
                    space_before: if blocks.is_empty() { 0.0 } else { 4.0 },
                    drop_order: order,
                    drop_whole: true,
                });
                kinds.push(BodyBlockKind::HistoryChinese);
                blocks.push(FitBlock {
                    text: pair.vietnamese.clone(),
                    base_size: vi_size,
                    space_before: 2.0,
                    drop_order: order,
                    drop_whole: true,
                });
                kinds.push(BodyBlockKind::HistoryVietnamese);
            }
            let history_shown = !blocks.is_empty();
            let pinyin_shown = config.display.show_chinese_line
                && config.display.show_pinyin_line
                && !state.pinyin_line.is_empty();
            if pinyin_shown {
                blocks.push(FitBlock {
                    text: state.pinyin_line.clone(),
                    base_size: zh_size * PINYIN_FONT_SCALE,
                    space_before: if history_shown { 8.0 } else { 0.0 },
                    drop_order: 2,
                    drop_whole: true,
                });
                kinds.push(BodyBlockKind::Pinyin);
            }
            if config.display.show_chinese_line {
                blocks.push(FitBlock {
                    text: zh_text,
                    base_size: zh_size,
                    space_before: if pinyin_shown {
                        2.0
                    } else if history_shown {
                        8.0
                    } else {
                        0.0
                    },
                    drop_order: 3,
                    drop_whole: false,
                });
                kinds.push(BodyBlockKind::Chinese);
            }
            if config.display.show_vietnamese_line {
                blocks.push(FitBlock {
                    text: vi_text,
                    base_size: vi_size,
                    space_before: 2.0,
                    drop_order: 4,
                    drop_whole: false,
                });
                kinds.push(BodyBlockKind::Vietnamese);
            }

            // Fit the blocks into the body budget with real egui text
            // layout (wrapped line count at the candidate size), then
            // render each surviving block as one label so the measured
            // and rendered text match exactly.
            let body_width = ui.available_width();
            let budget = (ui.available_height() - OVERLAY_FOOTER_RESERVE).max(0.0);
            let painter = ui.painter();
            let floor_scale = MIN_SUBTITLE_FONT_SIZE / vi_size;
            let fit = fit_subtitle_blocks(&blocks, floor_scale, budget, &|text, size| {
                painter
                    .layout(
                        text.to_string(),
                        egui::FontId::proportional(size),
                        egui::Color32::WHITE,
                        body_width,
                    )
                    .size()
                    .y
            });

            let body_resp = ui.vertical_centered(|ui| {
                ui.set_min_width(ui.available_width());
                let mut first_shown = true;
                for (idx, block) in blocks.iter().enumerate() {
                    let Some(text) = fit.texts.get(idx).and_then(Option::as_ref) else {
                        continue;
                    };
                    if !first_shown {
                        ui.add_space(block.space_before);
                    }
                    first_shown = false;
                    let size = block.base_size * fit.scale;
                    // History lines are view-only (no replay); current
                    // lines keep click-to-replay, and a drag still moves
                    // the window (egui separates click from drag by
                    // movement).
                    let (color, replay) = match kinds[idx] {
                        BodyBlockKind::HistoryChinese => (dim_color(zh_color), None),
                        BodyBlockKind::HistoryVietnamese => (dim_color(vi_color), None),
                        BodyBlockKind::Pinyin => {
                            (PINYIN_TEXT_COLOR, Some(OverlayCommand::ReplayChinese))
                        }
                        BodyBlockKind::Chinese => {
                            (zh_display_color, Some(OverlayCommand::ReplayChinese))
                        }
                        BodyBlockKind::Vietnamese => {
                            (vi_display_color, Some(OverlayCommand::ReplayVietnamese))
                        }
                    };
                    let resp = ui.add(
                        egui::Label::new(egui::RichText::new(text).size(size).color(color))
                            .wrap()
                            .sense(egui::Sense::click_and_drag()),
                    );
                    if let Some(cmd) = replay {
                        // Only replay real lines, never placeholders.
                        let is_real = match kinds[idx] {
                            BodyBlockKind::Pinyin => true,
                            BodyBlockKind::Chinese => !state.pipeline.chinese_line.is_empty(),
                            BodyBlockKind::Vietnamese => !state.pipeline.vietnamese_line.is_empty(),
                            _ => false,
                        };
                        if resp.clicked() && is_real {
                            commands.push(cmd);
                        }
                    }
                    if resp.drag_started_by(egui::PointerButton::Primary) {
                        start_drag = true;
                    }
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

            // Bottom action bar (icon buttons + tooltips): region | start | stop | mute.
            // Voice selection lives ONLY in Settings (Giọng đọc tab) — the
            // overlay no longer has a voice dropdown (2026-11-24).
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                if ui
                    .add(IconButton::new(UiIcon::Region))
                    .on_hover_text("Chọn vùng")
                    .clicked()
                {
                    commands.push(OverlayCommand::ToggleRegionPanel);
                }
                if ui
                    .add_enabled(state.can_start(), IconButton::new(UiIcon::Play))
                    .on_hover_text("Bắt đầu")
                    .clicked()
                {
                    commands.push(OverlayCommand::StartPipeline);
                }
                if ui
                    .add_enabled(state.can_stop(), IconButton::new(UiIcon::Stop))
                    .on_hover_text("Dừng")
                    .clicked()
                {
                    commands.push(OverlayCommand::StopPipeline);
                }

                let (mute_icon, mute_tip) = if state.muted {
                    (UiIcon::SpeakerMuted, "Bật tiếng")
                } else {
                    (UiIcon::Speaker, "Tắt tiếng")
                };
                if ui
                    .add(IconButton::new(mute_icon))
                    .on_hover_text(mute_tip)
                    .clicked()
                {
                    commands.push(OverlayCommand::ToggleMute);
                }
            });

            // Bottom-right resize grip: the borderless window has no
            // native resize border to grab, so dragging this grip
            // resizes the window (clamped to the overlay bounds).
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let (grip_rect, grip_resp) =
                    ui.allocate_exact_size(egui::vec2(20.0, 14.0), egui::Sense::drag());
                let grip_stroke = egui::Stroke::new(
                    1.5,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 70),
                );
                for i in 0..3 {
                    let off = i as f32 * 5.0;
                    ui.painter().line_segment(
                        [
                            egui::pos2(grip_rect.right() - off - 8.0, grip_rect.bottom() - 2.0),
                            egui::pos2(grip_rect.right() - 2.0, grip_rect.bottom() - off - 8.0),
                        ],
                        grip_stroke,
                    );
                }
                let grip_resp = grip_resp.on_hover_text("Kéo để thay đổi kích thước cửa sổ");
                if grip_resp.dragged() {
                    let delta = ui.input(|i| i.pointer.delta());
                    if let Some(outer) = ui.input(|i| i.viewport().outer_rect) {
                        let cur = outer.size();
                        let next = clamp_overlay_resize([cur.x + delta.x, cur.y + delta.y]);
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::InnerSize(next.into()));
                    }
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
        apply_exclude_from_capture_hwnd, clamp_overlay_resize, dim_color, fit_subtitle_blocks,
        overlay_overlaps_region, DisplayAffinityError, FitBlock, OverlayState, PositionTracker,
        ServerUiStatus, SizeTracker, MAX_HISTORY_PAIRS, MIN_SUBTITLE_FONT_SIZE,
    };
    use crate::config::{AppConfig, CaptureRegion, WindowPosition, WindowSize};
    use crate::pipeline::{
        PipelineErrorBanner, PipelineErrorKind, PipelineMetrics, PipelineSnapshot, PipelineStatus,
        SubtitlePair, ERR_BANNER_TRANSLATE_NETWORK,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn overlay_state_controls_reflect_server_and_pipeline_lifecycle() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());

        // Starting = Helsinki model still loading; the pipeline may
        // start (Vietphrase covers translations until the model is ready).
        assert!(state.can_start());
        assert!(!state.can_stop());
        assert_eq!(state.status_label(), "Đang tải model dịch…");

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
            history: Vec::new(),
        });
        assert!(!state.can_start());
        assert!(state.can_stop());
        assert_eq!(state.status_label(), "Đang dịch");

        state.server_status = ServerUiStatus::Failed("Không có engine dịch khả dụng".to_string());
        state.pipeline.status = PipelineStatus::Idle;
        assert!(!state.can_start());
        assert!(!state.can_stop());
        assert_eq!(state.status_label(), "Lỗi engine dịch");
        assert!(state
            .active_error_banner()
            .as_deref()
            .is_some_and(|msg| msg.contains("Không có engine dịch khả dụng")));
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
            history: Vec::new(),
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
            history: Vec::new(),
        });
        assert_eq!(state.active_error_banner(), None);
        assert_eq!(state.pipeline.vietnamese_line, "Câu thứ hai.");
    }

    #[test]
    fn overlay_state_mute_toggle_updates_cleanly() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        assert!(!state.muted);

        state.muted = true;
        assert!(state.muted);
        // Voice selection lives ONLY in Settings (no `selected_voice` on
        // the overlay state since 2026-11-24); the overlay reads the
        // voice from `AppConfig` instead.
        assert_eq!(cfg.voice.voice, "vi-VN-NamMinhNeural");
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
    fn overlay_state_recomputes_pinyin_when_chinese_line_changes() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        assert_eq!(state.pinyin_line, "");

        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 1,
            chinese_line: "你好，我是小明。".to_string(),
            vietnamese_line: "Ngươi tốt, ta là tiểu Minh.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
            history: Vec::new(),
        });
        assert_eq!(state.pinyin_line, "nǐ hǎo， wǒ shì xiǎo míng。");

        // Same line again: pinyin stays (no recompute needed).
        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 2,
            chinese_line: "你好，我是小明。".to_string(),
            vietnamese_line: "Ngươi tốt, ta là tiểu Minh.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
            history: Vec::new(),
        });
        assert_eq!(state.pinyin_line, "nǐ hǎo， wǒ shì xiǎo míng。");

        // New line: pinyin follows. Empty line: pinyin clears.
        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 3,
            chinese_line: "再见。".to_string(),
            vietnamese_line: "Tái kiến.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
            history: Vec::new(),
        });
        assert_eq!(state.pinyin_line, "zài jiàn。");

        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Idle,
            latest_seq: 4,
            chinese_line: String::new(),
            vietnamese_line: String::new(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
            history: Vec::new(),
        });
        assert_eq!(state.pinyin_line, "");
    }

    #[test]
    fn clamp_overlay_resize_keeps_size_within_min_max_bounds() {
        assert_eq!(clamp_overlay_resize([620.0, 204.0]), [620.0, 204.0]);
        assert_eq!(clamp_overlay_resize([100.0, 50.0]), [460.0, 140.0]);
        assert_eq!(clamp_overlay_resize([5000.0, 2000.0]), [1200.0, 600.0]);
        assert_eq!(clamp_overlay_resize([460.0, 140.0]), [460.0, 140.0]);
        assert_eq!(clamp_overlay_resize([1200.0, 600.0]), [1200.0, 600.0]);
        assert_eq!(clamp_overlay_resize([300.0, 300.0]), [460.0, 300.0]);
    }

    /// Stub measure: each visual line is 1.3x the font size tall; a
    /// logical line wraps every 10 chars (simulating a narrow body).
    fn stub_measure(text: &str, size: f32) -> f32 {
        let lines: f32 = text
            .split('\n')
            .map(|line| (line.chars().count() as f32 / 10.0).ceil().max(1.0))
            .sum();
        lines * size * 1.3
    }

    fn fit_block(
        text: &str,
        base_size: f32,
        space_before: f32,
        drop_order: u8,
        drop_whole: bool,
    ) -> FitBlock {
        FitBlock {
            text: text.to_string(),
            base_size,
            space_before,
            drop_order,
            drop_whole,
        }
    }

    fn fitted_height(blocks: &[FitBlock], fit: &super::FitResult) -> f32 {
        let mut total = 0.0;
        let mut first_shown = true;
        for (idx, block) in blocks.iter().enumerate() {
            let Some(text) = fit.texts[idx].as_ref() else {
                continue;
            };
            if !first_shown {
                total += block.space_before;
            }
            first_shown = false;
            total += stub_measure(text, block.base_size * fit.scale);
        }
        total
    }

    #[test]
    fn fit_keeps_single_line_at_full_scale() {
        let blocks = vec![
            fit_block("你好，我是小明。", 24.3, 0.0, 3, false),
            fit_block("Xin chào, tôi là Tiểu Minh.", 18.0, 4.0, 4, false),
        ];
        let fit = fit_subtitle_blocks(&blocks, MIN_SUBTITLE_FONT_SIZE / 18.0, 200.0, &stub_measure);
        assert_eq!(fit.scale, 1.0);
        assert!(!fit.truncated);
        assert_eq!(fit.texts[0].as_deref(), Some("你好，我是小明。"));
        assert_eq!(fit.texts[1].as_deref(), Some("Xin chào, tôi là Tiểu Minh."));
    }

    #[test]
    fn fit_shrinks_two_lines_uniformly_and_stays_inside_budget() {
        let blocks = vec![
            fit_block("你好。\n再见。", 24.3, 0.0, 3, false),
            fit_block("Chào.\nTạm biệt.", 18.0, 4.0, 4, false),
        ];
        // Full size needs ~114 px; budget 90 forces a uniform shrink.
        let fit = fit_subtitle_blocks(&blocks, MIN_SUBTITLE_FONT_SIZE / 18.0, 90.0, &stub_measure);
        assert!(
            fit.scale < 1.0,
            "expected a shrink, got scale {}",
            fit.scale
        );
        assert!(fit.scale >= MIN_SUBTITLE_FONT_SIZE / 18.0);
        assert!(!fit.truncated);
        assert_eq!(fit.texts[0].as_deref(), Some("你好。\n再见。"));
        assert_eq!(fit.texts[1].as_deref(), Some("Chào.\nTạm biệt."));
        assert!(fitted_height(&blocks, &fit) <= 90.0);
    }

    #[test]
    fn fit_drops_oldest_lines_and_marks_ellipsis_when_floor_does_not_fit() {
        let blocks = vec![
            fit_block("一。\n二。\n三。\n四。\n五。\n六。", 24.3, 0.0, 3, false),
            fit_block("Một.\nHai.\nBa.\nBốn.\nNăm.\nSáu.", 18.0, 4.0, 4, false),
        ];
        let fit = fit_subtitle_blocks(&blocks, MIN_SUBTITLE_FONT_SIZE / 18.0, 50.0, &stub_measure);
        assert!(fit.truncated);
        assert!(fit.scale >= MIN_SUBTITLE_FONT_SIZE / 18.0);
        // Only the newest lines survive, marked with "…" (never the
        // whole current subtitle: at least one line always remains).
        assert_eq!(fit.texts[0].as_deref(), Some("…六。"));
        assert_eq!(fit.texts[1].as_deref(), Some("…Sáu."));
        assert!(fitted_height(&blocks, &fit) <= 50.0);
    }

    #[test]
    fn fit_never_shrinks_below_floor_and_keeps_one_line() {
        let blocks = vec![
            fit_block("一。\n二。\n三。", 24.3, 0.0, 3, false),
            fit_block("Một.\nHai.\nBa.", 18.0, 4.0, 4, false),
        ];
        let fit = fit_subtitle_blocks(&blocks, MIN_SUBTITLE_FONT_SIZE / 18.0, 0.0, &stub_measure);
        assert!(fit.truncated);
        assert_eq!(fit.scale, MIN_SUBTITLE_FONT_SIZE / 18.0);
        // Each block keeps its last (newest) line: the current pair is
        // never reduced to a single language.
        assert_eq!(fit.texts[0].as_deref(), Some("…三。"));
        assert_eq!(fit.texts[1].as_deref(), Some("…Ba."));
    }

    #[test]
    fn fit_drops_history_pair_before_current_lines() {
        let blocks = vec![
            fit_block("之前。", 24.3, 0.0, 0, true),
            fit_block("Trước đó.", 18.0, 4.0, 0, true),
            fit_block("nǐ hǎo。", 17.5, 8.0, 2, true),
            fit_block("你好。", 24.3, 2.0, 3, false),
            fit_block("Chào.", 18.0, 4.0, 4, false),
        ];
        // Budget 60 fits the current lines at the floor but not the
        // history pair with them.
        let fit = fit_subtitle_blocks(&blocks, MIN_SUBTITLE_FONT_SIZE / 18.0, 60.0, &stub_measure);
        assert!(fit.truncated);
        // The whole history pair drops together (no orphaned
        // Vietnamese line without its Chinese line).
        assert_eq!(fit.texts[0], None);
        assert_eq!(fit.texts[1], None);
        assert_eq!(fit.texts[2].as_deref(), Some("nǐ hǎo。"));
        assert_eq!(fit.texts[3].as_deref(), Some("你好。"));
        assert_eq!(fit.texts[4].as_deref(), Some("Chào."));
        assert!(fit.scale >= MIN_SUBTITLE_FONT_SIZE / 18.0);
        assert!(fitted_height(&blocks, &fit) <= 60.0);
    }

    #[test]
    fn fit_long_single_line_wraps_and_shrinks_with_real_layout() {
        // Real egui layout (headless): a 60-char Chinese line wraps at
        // the 588 px body width, so its laid-out height is several lines
        // and the fit must shrink it instead of growing the window.
        let ctx = eframe::egui::Context::default();
        let raw_input = eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                eframe::egui::vec2(620.0, 204.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(raw_input, |ui| {
            // 120 chars: ~4 wrapped rows at the 588 px body width in
            // the headless default font (a 61-char line wraps to only
            // 2 rows, which is not enough to prove the shrink).
            let long_line = "夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下夜深了雨还在下。";
            let single_line_height = ui
                .painter()
                .layout(
                    "你好。".to_string(),
                    eframe::egui::FontId::proportional(24.3),
                    eframe::egui::Color32::WHITE,
                    588.0,
                )
                .size()
                .y;
            let long_height = ui
                .painter()
                .layout(
                    long_line.to_string(),
                    eframe::egui::FontId::proportional(24.3),
                    eframe::egui::Color32::WHITE,
                    588.0,
                )
                .size()
                .y;
            assert!(
                long_height > single_line_height * 2.0,
                "expected the long line to wrap to 3+ rows, got {long_height} vs {single_line_height}"
            );
            let blocks = vec![
                fit_block(long_line, 24.3, 0.0, 3, false),
                fit_block("Đêm đã khuya, mưa vẫn còn rơi.", 18.0, 4.0, 4, false),
            ];
            let fit = fit_subtitle_blocks(&blocks, MIN_SUBTITLE_FONT_SIZE / 18.0, 90.0, &|text, size| {
                ui.painter()
                    .layout(
                        text.to_string(),
                        eframe::egui::FontId::proportional(size),
                        eframe::egui::Color32::WHITE,
                        588.0,
                    )
                    .size()
                    .y
            });
            assert!(fit.scale < 1.0, "expected the wrapped line to shrink, got {}", fit.scale);
            assert_eq!(fit.texts[0].as_deref(), Some(long_line));
        });
    }

    #[test]
    fn dim_color_halves_alpha_keeping_hue() {
        // `Color32` stores premultiplied alpha, so compare the
        // un-premultiplied values: white round-trips exactly.
        assert_eq!(
            dim_color(eframe::egui::Color32::WHITE).to_srgba_unmultiplied(),
            [255, 255, 255, 140]
        );
        // Green keeps its hue at 55% alpha (premultiplication rounds,
        // so compare approximately).
        let [r, g, b, a] =
            dim_color(eframe::egui::Color32::from_rgb(74, 222, 128)).to_srgba_unmultiplied();
        assert_eq!(a, 140);
        assert!((r as i16 - 74).abs() <= 2, "r={r}");
        assert!((g as i16 - 222).abs() <= 2, "g={g}");
        assert!((b as i16 - 128).abs() <= 2, "b={b}");
    }

    #[test]
    fn size_tracker_debounces_resize_and_commits_after_delay() {
        let t0 = Instant::now();
        let initial = WindowSize {
            width: 620.0,
            height: 204.0,
        };
        let mut tracker = SizeTracker::new(Some(initial), Duration::from_millis(400));

        tracker.observe(620.2, 204.3, t0);
        assert_eq!(
            tracker.take_ready_to_save(t0 + Duration::from_millis(500)),
            None
        );

        tracker.observe(800.0, 300.0, t0 + Duration::from_millis(600));
        assert_eq!(
            tracker.take_ready_to_save(t0 + Duration::from_millis(900)),
            None
        );

        let ready = tracker.take_ready_to_save(t0 + Duration::from_millis(1050));
        assert_eq!(
            ready,
            Some(WindowSize {
                width: 800.0,
                height: 300.0
            })
        );
        if let Some(size) = ready {
            tracker.mark_saved(size);
        }
        assert_eq!(tracker.take_pending(), None);
    }

    #[test]
    fn overlay_state_copies_history_from_snapshot() {
        let cfg = AppConfig::default();
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        assert!(state.history.is_empty());

        let pair_a = SubtitlePair {
            chinese: "第一句。".to_string(),
            vietnamese: "Câu thứ nhất.".to_string(),
        };
        let pair_b = SubtitlePair {
            chinese: "第二句。".to_string(),
            vietnamese: "Câu thứ hai.".to_string(),
        };
        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 2,
            chinese_line: "第三句。".to_string(),
            vietnamese_line: "Câu thứ ba.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
            history: vec![pair_b.clone(), pair_a.clone()],
        });
        // Most-recent-first, as published by the pipeline.
        assert_eq!(state.history, vec![pair_b, pair_a]);
        assert_eq!(state.history.len(), MAX_HISTORY_PAIRS);
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
            history: Vec::new(),
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

    #[test]
    fn render_overlay_ui_draws_history_lines_headlessly_without_panic() {
        let ctx = eframe::egui::Context::default();
        let mut cfg = AppConfig::default();
        cfg.display.history_lines = 2;
        let mut state = OverlayState::from_config(&cfg, Vec::new());
        state.server_status = ServerUiStatus::Ready;
        state.apply_pipeline_snapshot(PipelineSnapshot {
            status: PipelineStatus::Running,
            latest_seq: 3,
            chinese_line: "第三句。".to_string(),
            vietnamese_line: "Câu thứ ba.".to_string(),
            error_banner: None,
            metrics: PipelineMetrics::default(),
            history: vec![
                SubtitlePair {
                    chinese: "第二句。".to_string(),
                    vietnamese: "Câu thứ hai.".to_string(),
                },
                SubtitlePair {
                    chinese: "第一句。".to_string(),
                    vietnamese: "Câu thứ nhất.".to_string(),
                },
            ],
        });

        let raw_input = eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                eframe::egui::vec2(620.0, 204.0),
            )),
            ..Default::default()
        };
        let full_output = ctx.run_ui(raw_input.clone(), |ui| {
            let cmds = super::render_overlay_ui(ui, &mut state, &cfg, &[]);
            assert!(cmds.is_empty());
        });
        assert!(!full_output.shapes.is_empty());

        // History off: the same snapshot renders without the history
        // pairs (and without panic).
        cfg.display.history_lines = 0;
        let full_output = ctx.run_ui(raw_input, |ui| {
            let cmds = super::render_overlay_ui(ui, &mut state, &cfg, &[]);
            assert!(cmds.is_empty());
        });
        assert!(!full_output.shapes.is_empty());
    }
}
