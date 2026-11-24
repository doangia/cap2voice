//! Fully transparent fullscreen region-selector viewport matching Mockup 2
//! (`region-select`) with red dashed selection border and DPI physical mapping.
//!
//! The window shows the desktop/video underneath untouched — only the
//! dragged selection rectangle (red dashed border), the coordinate badge,
//! and the top instruction pill are painted — so the user can align the
//! selection exactly over the burned-in subtitle text.
//!
//! Transparency, option B (2026-11-24, `docs/spec.md` §3.1): the window
//! itself is NOT transparent on this PC — eframe/glow fails to create a
//! transparent window ("Cannot create transparent window: the GL config
//! does not support it"; the WGL pixel format has no alpha on the AMD
//! Radeon, and winit 0.30 uses DWM blur-behind, not `WS_EX_LAYERED`).
//! An earlier color-key attempt made the window transparent but broke
//! drag-select (click-through) and was reverted. So instead of relying
//! on the compositor, the selector paints a FROZEN fullscreen snapshot
//! of the active monitor (captured with the existing `xcap` backend at
//! selector open, re-taken when the user switches monitors with Tab) as
//! its background texture. The snapshot is frozen at open time — not a
//! live feed — which is the intended design: the user aligns the drag
//! over the video frame as it was when the selector opened. The selector
//! window is also excluded from screen capture
//! (`WDA_EXCLUDEFROMCAPTURE`) so it never appears in the app's own
//! captures.

use crate::capture::{
    resolve_monitor_crop, validate_region_dimensions, CaptureError, MonitorInfo, MIN_REGION_HEIGHT,
    MIN_REGION_WIDTH,
};
use crate::config::{CaptureRegion, WindowPosition};
use eframe::egui;

/// Top-center prompt pill title shown over the region selector.
pub const REGION_PROMPT_TITLE: &str = "Kéo chuột để chọn vùng phụ đề";

/// Title of the region-selector viewport window (used by
/// `build_region_viewport_builder` and by the `WDA_EXCLUDEFROMCAPTURE`
/// lookup, which finds the window by title).
pub const REGION_VIEWPORT_TITLE: &str = "cap2voice - Chọn vùng phụ đề";

/// Secondary cancellation instruction rendered beneath/alongside the prompt pill.
pub const REGION_PROMPT_HINT: &str = "Nhấn Esc hoặc chuột phải để huỷ";

/// Red dashed selection rectangle color (`#EF4444`, `docs/spec.md` §3.1).
pub const SELECTION_BORDER_COLOR: egui::Color32 = egui::Color32::from_rgb(239, 68, 68);

/// Viewport ID used by `egui::Context::show_viewport_immediate` for region selection.
pub fn region_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("cap2voice_region_selector")
}

/// Live physical-pixel coordinates and dimensions formatted for the selection corner badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalSelectionPreview {
    pub monitor_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PhysicalSelectionPreview {
    /// Format `<W> × <H> px · <X>, <Y>` badge text matching Mockup 2.
    pub fn format_badge_text(&self) -> String {
        format!(
            "{} × {} px · {}, {}",
            self.width, self.height, self.x, self.y
        )
    }
}

/// Logical input events processed by `RegionSelectorState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegionSelectorEvent {
    PointerPressed(egui::Pos2),
    PointerMoved(egui::Pos2),
    PointerReleased(egui::Pos2),
    CancelRequested,
}

/// Completion state returned by `RegionSelectorState::handle_event` and `render_region_selector_ui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionSelectorOutcome {
    Pending,
    Cancelled,
    Confirmed(CaptureRegion),
}

/// State machine driving region drag geometry, minimum-size validation, and multi-monitor cycling.
#[derive(Debug, Clone)]
pub struct RegionSelectorState {
    monitors: Vec<MonitorInfo>,
    active_index: usize,
    drag_anchor: Option<egui::Pos2>,
    drag_current: Option<egui::Pos2>,
    hint_message: Option<String>,
}

impl RegionSelectorState {
    /// Construct a region selector state for the discovered monitor list.
    pub fn new(monitors: Vec<MonitorInfo>, active_index: usize) -> Result<Self, CaptureError> {
        if monitors.is_empty() {
            return Err(CaptureError::MonitorUnavailable {
                monitor_id: "PRIMARY".to_string(),
            });
        }
        let clamped_index = active_index.min(monitors.len().saturating_sub(1));
        Ok(Self {
            monitors,
            active_index: clamped_index,
            drag_anchor: None,
            drag_current: None,
            hint_message: None,
        })
    }

    /// Return metadata for the monitor currently targeted by the fullscreen overlay.
    pub fn active_monitor(&self) -> Option<&MonitorInfo> {
        self.monitors.get(self.active_index)
    }

    /// Return total number of connected monitors available to cycle.
    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    /// Cycle focus to the next connected monitor and clear any in-progress drag.
    pub fn cycle_monitor(&mut self) -> bool {
        if self.monitors.len() <= 1 {
            return false;
        }
        self.active_index = (self.active_index + 1) % self.monitors.len();
        self.drag_anchor = None;
        self.drag_current = None;
        self.hint_message = None;
        true
    }

    /// Return normalized logical drag rectangle within the current viewport, if dragging.
    pub fn selection_rect_logical(&self) -> Option<egui::Rect> {
        let (Some(anchor), Some(current)) = (self.drag_anchor, self.drag_current) else {
            return None;
        };
        Some(egui::Rect::from_two_pos(anchor, current))
    }

    /// Compute live physical coordinates and size for the current drag selection.
    pub fn physical_preview(&self) -> Option<PhysicalSelectionPreview> {
        let monitor = self.active_monitor()?;
        let (Some(anchor), Some(current)) = (self.drag_anchor, self.drag_current) else {
            return None;
        };
        logical_drag_to_physical_preview(monitor, anchor, current)
    }

    /// Return brief Vietnamese validation feedback when a drag is smaller than minimum size.
    pub fn hint_message(&self) -> Option<&str> {
        self.hint_message.as_deref()
    }

    /// Apply a pointer or cancel event and yield whether selection is pending, cancelled, or confirmed.
    pub fn handle_event(&mut self, event: RegionSelectorEvent) -> RegionSelectorOutcome {
        match event {
            RegionSelectorEvent::PointerPressed(pos) => {
                self.drag_anchor = Some(pos);
                self.drag_current = Some(pos);
                self.hint_message = None;
                RegionSelectorOutcome::Pending
            }
            RegionSelectorEvent::PointerMoved(pos) => {
                if self.drag_anchor.is_some() {
                    self.drag_current = Some(pos);
                }
                RegionSelectorOutcome::Pending
            }
            RegionSelectorEvent::PointerReleased(pos) => {
                let Some(anchor) = self.drag_anchor.take() else {
                    self.drag_current = None;
                    return RegionSelectorOutcome::Pending;
                };
                self.drag_current = None;

                let Some(monitor) = self.active_monitor() else {
                    return RegionSelectorOutcome::Cancelled;
                };
                match logical_drag_to_physical_region(monitor, anchor, pos) {
                    Ok(region) => {
                        self.hint_message = None;
                        RegionSelectorOutcome::Confirmed(region)
                    }
                    Err(CaptureError::InvalidRegion { width, height }) => {
                        self.hint_message = Some(format!(
                            "Vùng chọn quá nhỏ ({width} × {height} px) — cần tối thiểu {MIN_REGION_WIDTH} × {MIN_REGION_HEIGHT} px"
                        ));
                        RegionSelectorOutcome::Pending
                    }
                    Err(err) => {
                        self.hint_message = Some(err.to_string());
                        RegionSelectorOutcome::Pending
                    }
                }
            }
            RegionSelectorEvent::CancelRequested => {
                self.drag_anchor = None;
                self.drag_current = None;
                self.hint_message = None;
                RegionSelectorOutcome::Cancelled
            }
        }
    }
}

fn validate_monitor_scale(monitor: &MonitorInfo) -> Result<f32, CaptureError> {
    if !monitor.scale_factor.is_finite() || monitor.scale_factor <= 0.0 {
        return Err(CaptureError::Backend(format!(
            "invalid monitor scale factor {}",
            monitor.scale_factor
        )));
    }
    Ok(monitor.scale_factor)
}

fn monitor_local_bounds_f32(
    monitor: &MonitorInfo,
    start_logical: egui::Pos2,
    end_logical: egui::Pos2,
) -> Result<(i32, i32, u32, u32), CaptureError> {
    if !start_logical.x.is_finite()
        || !start_logical.y.is_finite()
        || !end_logical.x.is_finite()
        || !end_logical.y.is_finite()
    {
        return Err(CaptureError::InvalidRegion {
            width: 0,
            height: 0,
        });
    }

    let scale = validate_monitor_scale(monitor)?;
    let min_lx = start_logical.x.min(end_logical.x);
    let max_lx = start_logical.x.max(end_logical.x);
    let min_ly = start_logical.y.min(end_logical.y);
    let max_ly = start_logical.y.max(end_logical.y);

    // Clamp both endpoints in physical monitor coordinates so right/bottom drags never overflow.
    let max_w = monitor.width as f32;
    let max_h = monitor.height as f32;
    let px0 = (min_lx * scale).round().clamp(0.0, max_w) as u32;
    let px1 = (max_lx * scale).round().clamp(0.0, max_w) as u32;
    let py0 = (min_ly * scale).round().clamp(0.0, max_h) as u32;
    let py1 = (max_ly * scale).round().clamp(0.0, max_h) as u32;

    let width = px1.saturating_sub(px0);
    let height = py1.saturating_sub(py0);
    let rel_x = i32::try_from(px0).unwrap_or(i32::MAX);
    let rel_y = i32::try_from(py0).unwrap_or(i32::MAX);
    Ok((
        monitor.x.saturating_add(rel_x),
        monitor.y.saturating_add(rel_y),
        width,
        height,
    ))
}

/// Pick the monitor index containing the overlay origin, falling back to primary or index `0`.
pub fn select_active_monitor_index(
    monitors: &[MonitorInfo],
    overlay_pos_logical: Option<&WindowPosition>,
    pixels_per_point: f32,
) -> usize {
    if monitors.is_empty() {
        return 0;
    }

    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };

    if let Some(pos) = overlay_pos_logical {
        if pos.x.is_finite() && pos.y.is_finite() {
            let phys_x = (pos.x * scale).round() as i64;
            let phys_y = (pos.y * scale).round() as i64;
            if let Some(idx) = monitors.iter().position(|m| {
                let mx0 = i64::from(m.x);
                let my0 = i64::from(m.y);
                let mx1 = mx0 + i64::from(m.width);
                let my1 = my0 + i64::from(m.height);
                phys_x >= mx0 && phys_x < mx1 && phys_y >= my0 && phys_y < my1
            }) {
                return idx;
            }
        }
    }

    monitors.iter().position(|m| m.is_primary).unwrap_or(0)
}

/// Convert logical monitor-local drag endpoints into physical pixel preview coordinates.
pub fn logical_drag_to_physical_preview(
    monitor: &MonitorInfo,
    start_logical: egui::Pos2,
    end_logical: egui::Pos2,
) -> Option<PhysicalSelectionPreview> {
    let (x, y, width, height) =
        monitor_local_bounds_f32(monitor, start_logical, end_logical).ok()?;
    Some(PhysicalSelectionPreview {
        monitor_id: monitor.id.clone(),
        x,
        y,
        width,
        height,
    })
}

/// Convert logical monitor-local drag endpoints into a validated physical `CaptureRegion`.
pub fn logical_drag_to_physical_region(
    monitor: &MonitorInfo,
    start_logical: egui::Pos2,
    end_logical: egui::Pos2,
) -> Result<CaptureRegion, CaptureError> {
    let (x, y, width, height) = monitor_local_bounds_f32(monitor, start_logical, end_logical)?;
    validate_region_dimensions(width, height)?;
    let region = CaptureRegion {
        monitor_id: monitor.id.clone(),
        x,
        y,
        width,
        height,
    };
    let _ = resolve_monitor_crop(&region, std::slice::from_ref(monitor))?;
    Ok(region)
}

/// Fullscreen physical `CaptureRegion` covering `monitor` (option-B
/// snapshot source: the selector background is a frozen capture of the
/// whole active monitor, taken when the selector opens).
pub fn fullscreen_region_for_monitor(monitor: &MonitorInfo) -> CaptureRegion {
    CaptureRegion {
        monitor_id: monitor.id.clone(),
        x: monitor.x,
        y: monitor.y,
        width: monitor.width,
        height: monitor.height,
    }
}

/// Position the live `<W> × <H> px · <X>, <Y>` badge near the selection without clipping off-screen.
pub fn clamp_badge_position(
    selection: egui::Rect,
    screen: egui::Rect,
    badge_size: egui::Vec2,
) -> egui::Pos2 {
    let margin = 8.0;
    let preferred_x = (selection.right() - badge_size.x + 12.0).max(selection.left());
    let mut y = selection.bottom() + 8.0;
    if y + badge_size.y > screen.bottom() - margin {
        y = selection.top() - badge_size.y - 8.0;
    }

    let min_x = screen.left() + margin;
    let max_x = (screen.right() - margin - badge_size.x).max(min_x);
    let min_y = screen.top() + margin;
    let max_y = (screen.bottom() - margin - badge_size.y).max(min_y);

    egui::pos2(preferred_x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// Build the borderless, transparent, always-on-top viewport covering `monitor`.
pub fn build_region_viewport_builder(monitor: &MonitorInfo) -> egui::ViewportBuilder {
    let scale = if monitor.scale_factor.is_finite() && monitor.scale_factor > 0.0 {
        monitor.scale_factor
    } else {
        1.0
    };
    let pos = egui::pos2(monitor.x as f32 / scale, monitor.y as f32 / scale);
    let size = egui::vec2(monitor.width as f32 / scale, monitor.height as f32 / scale);

    egui::ViewportBuilder::default()
        .with_title(REGION_VIEWPORT_TITLE)
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_taskbar(false)
        .with_active(true)
        .with_window_level(egui::WindowLevel::AlwaysOnTop)
        .with_clamp_size_to_monitor_size(false)
        .with_position(pos)
        .with_inner_size(size)
}

fn paint_dashed_selection_border(painter: &egui::Painter, rect: egui::Rect) {
    let stroke = egui::Stroke::new(2.5, SELECTION_BORDER_COLOR);
    let dash = 10.0;
    let gap = 6.0;
    let corners = [
        [rect.left_top(), rect.right_top()],
        [rect.right_top(), rect.right_bottom()],
        [rect.right_bottom(), rect.left_bottom()],
        [rect.left_bottom(), rect.left_top()],
    ];
    for edge in corners {
        painter.extend(egui::Shape::dashed_line(&edge, stroke, dash, gap));
    }
}

fn paint_top_prompt_pill(painter: &egui::Painter, screen: egui::Rect, state: &RegionSelectorState) {
    let title_galley = painter.layout_no_wrap(
        REGION_PROMPT_TITLE.to_string(),
        egui::FontId::proportional(15.0),
        egui::Color32::WHITE,
    );
    let hint_text = if state.monitor_count() > 1 {
        format!("{REGION_PROMPT_HINT} · Tab: đổi màn hình")
    } else {
        REGION_PROMPT_HINT.to_string()
    };
    let hint_galley = painter.layout_no_wrap(
        hint_text,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(148, 163, 184),
    );

    let icon_w = 22.0;
    let pad_x = 18.0;
    let pad_y = 9.0;
    let gap_icon_text = 10.0;
    let content_w = (icon_w + gap_icon_text + title_galley.size().x).max(hint_galley.size().x);
    let pill_w = content_w + pad_x * 2.0;
    let pill_h = title_galley.size().y + 4.0 + hint_galley.size().y + pad_y * 2.0;
    let pill_rect = egui::Rect::from_center_size(
        egui::pos2(screen.center().x, screen.top() + 20.0 + pill_h * 0.5),
        egui::vec2(pill_w, pill_h),
    );

    painter.rect(
        pill_rect,
        egui::CornerRadius::same(22),
        egui::Color32::from_rgba_unmultiplied(24, 27, 34, 238),
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 42),
        ),
        egui::StrokeKind::Inside,
    );

    let title_row_y = pill_rect.top() + pad_y + title_galley.size().y * 0.5;
    let icon_center = egui::pos2(
        pill_rect.center().x - (title_galley.size().x + gap_icon_text) * 0.5,
        title_row_y,
    );
    let accent = egui::Color32::from_rgb(96, 165, 250);
    let icon_stroke = egui::Stroke::new(1.5, accent);
    painter.circle_stroke(icon_center, 7.0, icon_stroke);
    painter.line_segment(
        [
            egui::pos2(icon_center.x - 10.0, icon_center.y),
            egui::pos2(icon_center.x + 10.0, icon_center.y),
        ],
        icon_stroke,
    );
    painter.line_segment(
        [
            egui::pos2(icon_center.x, icon_center.y - 10.0),
            egui::pos2(icon_center.x, icon_center.y + 10.0),
        ],
        icon_stroke,
    );

    painter.galley(
        egui::pos2(icon_center.x + icon_w * 0.5 + 4.0, pill_rect.top() + pad_y),
        title_galley,
        egui::Color32::WHITE,
    );
    painter.galley(
        egui::pos2(
            pill_rect.center().x - hint_galley.size().x * 0.5,
            pill_rect.bottom() - pad_y - hint_galley.size().y,
        ),
        hint_galley,
        egui::Color32::from_rgb(148, 163, 184),
    );

    if let Some(msg) = state.hint_message() {
        let warn_galley = painter.layout_no_wrap(
            msg.to_string(),
            egui::FontId::proportional(12.5),
            egui::Color32::from_rgb(254, 202, 202),
        );
        let warn_size = warn_galley.size() + egui::vec2(24.0, 12.0);
        let warn_rect = egui::Rect::from_center_size(
            egui::pos2(
                screen.center().x,
                pill_rect.bottom() + 10.0 + warn_size.y * 0.5,
            ),
            warn_size,
        );
        painter.rect(
            warn_rect,
            egui::CornerRadius::same(10),
            egui::Color32::from_rgba_unmultiplied(69, 20, 27, 238),
            egui::Stroke::new(1.0, SELECTION_BORDER_COLOR),
            egui::StrokeKind::Inside,
        );
        painter.galley(
            warn_rect.min + egui::vec2(12.0, 6.0),
            warn_galley,
            egui::Color32::from_rgb(254, 202, 202),
        );
    }
}

fn paint_coordinate_badge(
    painter: &egui::Painter,
    selection: egui::Rect,
    screen: egui::Rect,
    preview: &PhysicalSelectionPreview,
) {
    let badge_galley = painter.layout_no_wrap(
        preview.format_badge_text(),
        egui::FontId::monospace(13.0),
        egui::Color32::WHITE,
    );
    let badge_size = badge_galley.size() + egui::vec2(20.0, 10.0);
    let badge_pos = clamp_badge_position(selection, screen, badge_size);
    let badge_rect = egui::Rect::from_min_size(badge_pos, badge_size);

    painter.rect(
        badge_rect,
        egui::CornerRadius::same(8),
        egui::Color32::from_rgba_unmultiplied(24, 27, 34, 240),
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 48),
        ),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        badge_rect.min + egui::vec2(10.0, 5.0),
        badge_galley,
        egui::Color32::WHITE,
    );
}

/// Render the fullscreen transparent region-selection viewport and process pointer/keyboard input.
///
/// `background` is the frozen fullscreen snapshot of the active monitor
/// (option B, taken when the selector opened), painted over the whole
/// viewport so the desktop/video underneath appears fully visible. `None`
/// keeps the previous behavior (opaque black window, no background).
pub fn render_region_selector_ui(
    ui: &mut egui::Ui,
    state: &mut RegionSelectorState,
    background: Option<&egui::TextureHandle>,
) -> RegionSelectorOutcome {
    let screen = ui.max_rect();
    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);

    let (cancel_requested, switch_monitor, events) = ui.input(|i| {
        let esc_or_close = i.viewport().close_requested()
            || i.key_pressed(egui::Key::Escape)
            || i.pointer.button_pressed(egui::PointerButton::Secondary)
            || i.pointer.button_clicked(egui::PointerButton::Secondary);
        let tab = i.key_pressed(egui::Key::Tab);
        (esc_or_close, tab, i.events.clone())
    });

    if cancel_requested {
        return state.handle_event(RegionSelectorEvent::CancelRequested);
    }
    if switch_monitor {
        let _ = state.cycle_monitor();
    }

    let mut outcome = RegionSelectorOutcome::Pending;
    for ev in &events {
        match ev {
            egui::Event::Key {
                key: egui::Key::Escape,
                pressed: true,
                ..
            } => {
                return state.handle_event(RegionSelectorEvent::CancelRequested);
            }
            egui::Event::PointerButton {
                button: egui::PointerButton::Secondary,
                pressed: true,
                ..
            } => {
                return state.handle_event(RegionSelectorEvent::CancelRequested);
            }
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                ..
            } => {
                outcome = state.handle_event(RegionSelectorEvent::PointerPressed(*pos));
            }
            egui::Event::PointerMoved(pos) => {
                outcome = state.handle_event(RegionSelectorEvent::PointerMoved(*pos));
            }
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                ..
            } => {
                outcome = state.handle_event(RegionSelectorEvent::PointerReleased(*pos));
                if !matches!(outcome, RegionSelectorOutcome::Pending) {
                    return outcome;
                }
            }
            _ => {}
        }
    }

    let painter = ui.painter();

    // Option-B background: the window itself is opaque black on this PC
    // (no alpha in the WGL pixel format), so the frozen fullscreen
    // snapshot of the active monitor is painted as the background — the
    // desktop/video underneath appears fully visible. The snapshot is in
    // physical pixels and the viewport is logical points (physical /
    // scale), so stretching the texture over the whole viewport rect
    // keeps the same logical-to-physical mapping as the drag math.
    if let Some(texture) = background {
        painter.image(
            texture.id(),
            screen,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }

    let cutout = state
        .selection_rect_logical()
        .map(|rect| rect.intersect(screen))
        .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0);

    // Fully transparent: no dimming veil — the desktop/video underneath
    // stays visible so the user can align the selection exactly. Only
    // the selection border, the coordinate badge, and the prompt pill
    // are painted on top of the snapshot background.
    //
    if let Some(sel_rect) = cutout {
        paint_dashed_selection_border(painter, sel_rect);
        if let Some(preview) = state.physical_preview() {
            paint_coordinate_badge(painter, sel_rect, screen, &preview);
        }
    }

    paint_top_prompt_pill(painter, screen, state);
    outcome
}

#[cfg(test)]
mod tests {
    use super::{
        build_region_viewport_builder, clamp_badge_position, fullscreen_region_for_monitor,
        logical_drag_to_physical_preview, logical_drag_to_physical_region,
        render_region_selector_ui, select_active_monitor_index, PhysicalSelectionPreview,
        RegionSelectorEvent, RegionSelectorOutcome, RegionSelectorState, REGION_PROMPT_TITLE,
        SELECTION_BORDER_COLOR,
    };
    use crate::capture::{CaptureError, MonitorInfo};
    use crate::config::{CaptureRegion, WindowPosition};
    use eframe::egui;

    fn make_monitor(
        id: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        scale_factor: f32,
        is_primary: bool,
    ) -> MonitorInfo {
        MonitorInfo {
            id: id.to_string(),
            name: id.to_string(),
            x,
            y,
            width,
            height,
            scale_factor,
            is_primary,
        }
    }

    fn collect_shapes_recursive<'a>(shape: &'a egui::Shape, out: &mut Vec<&'a egui::Shape>) {
        out.push(shape);
        if let egui::Shape::Vec(children) = shape {
            for child in children {
                collect_shapes_recursive(child, out);
            }
        }
    }

    #[test]
    fn logical_drag_to_physical_maps_100_125_150_175_dpi_scales_accurately() {
        // 100% DPI (1920x1080)
        let mon_100 = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true);
        assert_eq!(
            logical_drag_to_physical_region(
                &mon_100,
                egui::pos2(420.0, 1020.0),
                egui::pos2(1700.0, 1080.0),
            ),
            Ok(CaptureRegion {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 420,
                y: 1020,
                width: 1280,
                height: 60,
            })
        );

        // 125% DPI (1920x1080 physical == 1536x864 logical), dragged bottom-right -> top-left
        let mon_125 = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.25, true);
        assert_eq!(
            logical_drag_to_physical_region(
                &mon_125,
                egui::pos2(1000.0, 600.0),
                egui::pos2(200.0, 500.0),
            ),
            Ok(CaptureRegion {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 250,
                y: 625,
                width: 1000,
                height: 125,
            })
        );

        // 150% DPI (1920x1080 physical == 1280x720 logical)
        let mon_150 = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.5, true);
        assert_eq!(
            logical_drag_to_physical_region(
                &mon_150,
                egui::pos2(200.0, 600.0),
                egui::pos2(1000.0, 660.0),
            ),
            Ok(CaptureRegion {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 300,
                y: 900,
                width: 1200,
                height: 90,
            })
        );

        // 175% DPI edge clamp + negative-offset secondary monitor
        let mon_175 = make_monitor(r"\\.\DISPLAY2", -1920, 100, 1920, 1080, 1.75, false);
        let max_lx = 1920.0 / 1.75 + 2.0;
        let max_ly = 1080.0 / 1.75 + 2.0;
        assert_eq!(
            logical_drag_to_physical_region(
                &mon_175,
                egui::pos2(100.0, 100.0),
                egui::pos2(max_lx, max_ly),
            ),
            Ok(CaptureRegion {
                monitor_id: r"\\.\DISPLAY2".to_string(),
                x: -1745,
                y: 275,
                width: 1745,
                height: 905,
            })
        );

        let preview = logical_drag_to_physical_preview(
            &mon_150,
            egui::pos2(200.0, 600.0),
            egui::pos2(1000.0, 660.0),
        );
        assert_eq!(
            preview,
            Some(PhysicalSelectionPreview {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 300,
                y: 900,
                width: 1200,
                height: 90,
            })
        );
    }

    #[test]
    fn region_selector_accepts_20x16_minimum_and_rejects_smaller_clicks_with_hint() {
        let mon = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true);
        let mut state = match RegionSelectorState::new(vec![mon.clone()], 0) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };

        // 19 x 16 physical -> rejected
        let _ = state.handle_event(RegionSelectorEvent::PointerPressed(egui::pos2(
            100.0, 100.0,
        )));
        let _ = state.handle_event(RegionSelectorEvent::PointerMoved(egui::pos2(119.0, 116.0)));
        assert_eq!(
            state.handle_event(RegionSelectorEvent::PointerReleased(egui::pos2(
                119.0, 116.0
            ))),
            RegionSelectorOutcome::Pending
        );
        assert!(
            state
                .hint_message()
                .is_some_and(|msg| msg.contains("20") && msg.contains("16")),
            "expected Vietnamese minimum-size hint, got {:?}",
            state.hint_message()
        );
        assert_eq!(state.selection_rect_logical(), None);

        // 20 x 15 physical -> rejected
        let _ = state.handle_event(RegionSelectorEvent::PointerPressed(egui::pos2(
            100.0, 100.0,
        )));
        assert_eq!(
            state.handle_event(RegionSelectorEvent::PointerReleased(egui::pos2(
                120.0, 115.0
            ))),
            RegionSelectorOutcome::Pending
        );
        assert!(state.hint_message().is_some());

        // Exact 20 x 16 physical -> accepted and clears hint
        let _ = state.handle_event(RegionSelectorEvent::PointerPressed(egui::pos2(
            100.0, 100.0,
        )));
        assert_eq!(
            state.handle_event(RegionSelectorEvent::PointerReleased(egui::pos2(
                120.0, 116.0
            ))),
            RegionSelectorOutcome::Confirmed(CaptureRegion {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 100,
                y: 100,
                width: 20,
                height: 16,
            })
        );
        assert_eq!(state.hint_message(), None);

        // At 150% DPI: 12x10 logical (18x15 physical) rejected, 14x11 logical (21x17 physical) accepted
        let mon_150 = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.5, true);
        let mut scaled_state = match RegionSelectorState::new(vec![mon_150], 0) {
            Ok(s) => s,
            Err(err) => panic!("expected scaled state init to succeed: {err}"),
        };
        let _ =
            scaled_state.handle_event(RegionSelectorEvent::PointerPressed(egui::pos2(50.0, 50.0)));
        assert_eq!(
            scaled_state.handle_event(RegionSelectorEvent::PointerReleased(egui::pos2(62.0, 60.0))),
            RegionSelectorOutcome::Pending
        );
        assert!(scaled_state.hint_message().is_some());

        let _ =
            scaled_state.handle_event(RegionSelectorEvent::PointerPressed(egui::pos2(50.0, 50.0)));
        assert_eq!(
            scaled_state.handle_event(RegionSelectorEvent::PointerReleased(egui::pos2(64.0, 61.0))),
            RegionSelectorOutcome::Confirmed(CaptureRegion {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 75,
                y: 75,
                width: 21,
                height: 17,
            })
        );
    }

    #[test]
    fn region_selector_esc_and_right_click_cancel_idle_and_mid_drag() {
        let mon = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true);
        let mut idle_state = match RegionSelectorState::new(vec![mon.clone()], 0) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };
        assert_eq!(
            idle_state.handle_event(RegionSelectorEvent::CancelRequested),
            RegionSelectorOutcome::Cancelled
        );

        let mut dragging_state = match RegionSelectorState::new(vec![mon], 0) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };
        let _ = dragging_state.handle_event(RegionSelectorEvent::PointerPressed(egui::pos2(
            300.0, 800.0,
        )));
        let _ = dragging_state
            .handle_event(RegionSelectorEvent::PointerMoved(egui::pos2(900.0, 880.0)));
        assert!(dragging_state.selection_rect_logical().is_some());
        assert_eq!(
            dragging_state.handle_event(RegionSelectorEvent::CancelRequested),
            RegionSelectorOutcome::Cancelled
        );
        assert_eq!(dragging_state.selection_rect_logical(), None);
    }

    #[test]
    fn physical_preview_formats_exact_badge_and_clamps_near_screen_edges() {
        let preview = PhysicalSelectionPreview {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 420,
            y: 1020,
            width: 1280,
            height: 60,
        };
        assert_eq!(preview.format_badge_text(), "1280 × 60 px · 420, 1020");

        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1080.0));
        let badge_size = egui::vec2(200.0, 28.0);
        let near_corner =
            egui::Rect::from_min_max(egui::pos2(1650.0, 1000.0), egui::pos2(1910.0, 1075.0));
        let clamped = clamp_badge_position(near_corner, screen, badge_size);
        assert!(clamped.x >= screen.left() + 8.0);
        assert!(clamped.y >= screen.top() + 8.0);
        assert!(clamped.x + badge_size.x <= screen.right() - 8.0);
        assert!(clamped.y + badge_size.y <= screen.bottom() - 8.0);
    }

    #[test]
    fn select_active_monitor_and_cycle_monitor_handle_multi_monitor_layouts() {
        let monitors = vec![
            make_monitor(r"\\.\DISPLAY2", -1920, 0, 1920, 1080, 1.0, false),
            make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.5, true),
        ];

        assert_eq!(
            select_active_monitor_index(
                &monitors,
                Some(&WindowPosition {
                    x: -800.0,
                    y: 200.0
                }),
                1.0
            ),
            0
        );
        assert_eq!(
            select_active_monitor_index(
                &monitors,
                Some(&WindowPosition { x: 200.0, y: 200.0 }),
                1.5
            ),
            1
        );
        // Outside all monitor rects -> fallback to primary monitor (index 1)
        assert_eq!(
            select_active_monitor_index(
                &monitors,
                Some(&WindowPosition {
                    x: 9000.0,
                    y: 9000.0
                }),
                1.0
            ),
            1
        );

        let mut state = match RegionSelectorState::new(monitors, 0) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };
        assert_eq!(state.monitor_count(), 2);
        assert_eq!(
            state.active_monitor().map(|m| m.id.as_str()),
            Some(r"\\.\DISPLAY2")
        );
        assert!(state.cycle_monitor());
        assert_eq!(
            state.active_monitor().map(|m| m.id.as_str()),
            Some(r"\\.\DISPLAY1")
        );
        assert!(state.cycle_monitor());
        assert_eq!(
            state.active_monitor().map(|m| m.id.as_str()),
            Some(r"\\.\DISPLAY2")
        );
    }

    #[test]
    fn build_region_viewport_builder_configures_transparent_borderless_always_on_top_monitor_bounds(
    ) {
        let mon = make_monitor(r"\\.\DISPLAY1", 1920, 0, 1920, 1080, 1.5, false);
        let vb = build_region_viewport_builder(&mon);
        assert_eq!(vb.decorations, Some(false));
        assert_eq!(vb.transparent, Some(true));
        assert_eq!(vb.resizable, Some(false));
        assert_eq!(vb.taskbar, Some(false));
        assert_eq!(vb.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!(vb.clamp_size_to_monitor_size, Some(false));
        assert_eq!(vb.position, Some(egui::pos2(1280.0, 0.0)));
        assert_eq!(vb.inner_size, Some(egui::vec2(1280.0, 720.0)));
    }

    #[test]
    fn fullscreen_region_for_monitor_covers_monitor_bounds() {
        let mon = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true);
        assert_eq!(
            fullscreen_region_for_monitor(&mon),
            CaptureRegion {
                monitor_id: r"\\.\DISPLAY1".to_string(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        );
        let secondary = make_monitor(r"\\.\DISPLAY2", -1920, 100, 1920, 1080, 1.5, false);
        assert_eq!(
            fullscreen_region_for_monitor(&secondary),
            CaptureRegion {
                monitor_id: r"\\.\DISPLAY2".to_string(),
                x: -1920,
                y: 100,
                width: 1920,
                height: 1080,
            }
        );
    }

    /// Whether the textured `mesh` quad covers `rect` (within rounding).
    fn mesh_covers_rect(mesh: &egui::Mesh, rect: egui::Rect) -> bool {
        if mesh.vertices.is_empty() {
            return false;
        }
        let mut min = egui::Pos2::new(f32::INFINITY, f32::INFINITY);
        let mut max = egui::Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
        for v in &mesh.vertices {
            min.x = min.x.min(v.pos.x);
            min.y = min.y.min(v.pos.y);
            max.x = max.x.max(v.pos.x);
            max.y = max.y.max(v.pos.y);
        }
        (min.x - rect.left()).abs() < 0.01
            && (min.y - rect.top()).abs() < 0.01
            && (max.x - rect.right()).abs() < 0.01
            && (max.y - rect.bottom()).abs() < 0.01
    }

    #[test]
    fn render_region_selector_ui_paints_snapshot_background_fullscreen() {
        let mon = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true);
        let mut state = match RegionSelectorState::new(vec![mon], 0) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1920.0, 1080.0));
        let texture = ctx.load_texture(
            "test_snapshot",
            egui::ColorImage::new([4, 4], vec![egui::Color32::RED; 16]),
            egui::TextureOptions::NEAREST,
        );

        let full_output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                let outcome = render_region_selector_ui(ui, &mut state, Some(&texture));
                assert_eq!(outcome, RegionSelectorOutcome::Pending);
            },
        );
        let mut all_shapes = Vec::new();
        for clipped in &full_output.shapes {
            collect_shapes_recursive(&clipped.shape, &mut all_shapes);
        }
        // egui 0.35 has no `Shape::Image`: `Painter::image` emits a
        // textured `Shape::Mesh` quad, so assert on the mesh.
        assert!(
            all_shapes.iter().any(|s| matches!(
                s,
                egui::Shape::Mesh(mesh) if mesh.texture_id == texture.id()
                    && mesh_covers_rect(mesh, screen_rect)
            )),
            "expected a fullscreen snapshot mesh covering the viewport"
        );

        // No background: no Image shape at all.
        let mut state_no_bg = match RegionSelectorState::new(
            vec![make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true)],
            0,
        ) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };
        let no_bg_output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                let outcome = render_region_selector_ui(ui, &mut state_no_bg, None);
                assert_eq!(outcome, RegionSelectorOutcome::Pending);
            },
        );
        let mut no_bg_shapes = Vec::new();
        for clipped in &no_bg_output.shapes {
            collect_shapes_recursive(&clipped.shape, &mut no_bg_shapes);
        }
        assert!(
            !no_bg_shapes.iter().any(|s| matches!(
                s,
                egui::Shape::Mesh(mesh) if mesh.texture_id == texture.id()
            )),
            "no snapshot mesh may be painted without a snapshot background"
        );
    }

    #[test]
    fn render_region_selector_ui_draws_transparent_red_dashed_box_and_handles_esc_cancel() {
        let mon = make_monitor(r"\\.\DISPLAY1", 0, 0, 1920, 1080, 1.0, true);
        let mut state = match RegionSelectorState::new(vec![mon], 0) {
            Ok(s) => s,
            Err(err) => panic!("expected state init to succeed: {err}"),
        };

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1920.0, 1080.0));

        // Frame 1: pointer press + drag to (800, 900)
        let drag_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            events: vec![
                egui::Event::PointerMoved(egui::pos2(200.0, 820.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(200.0, 820.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerMoved(egui::pos2(800.0, 900.0)),
            ],
            ..Default::default()
        };
        let full_output = ctx.run_ui(drag_input, |ui| {
            let outcome = render_region_selector_ui(ui, &mut state, None);
            assert_eq!(outcome, RegionSelectorOutcome::Pending);
        });

        let mut all_shapes = Vec::new();
        for clipped in &full_output.shapes {
            collect_shapes_recursive(&clipped.shape, &mut all_shapes);
        }
        assert!(
            all_shapes.iter().any(|s| matches!(
                s,
                egui::Shape::LineSegment { stroke, .. } if stroke.color == SELECTION_BORDER_COLOR
            )),
            "expected red #EF4444 dashed border LineSegment shapes during drag"
        );
        // Fully transparent: the old dim veil color (rgba 8,10,14,150)
        // must never be painted — the desktop/video underneath stays visible.
        assert!(
            !all_shapes.iter().any(|s| matches!(
                s,
                egui::Shape::Rect(r) if r.fill
                    == egui::Color32::from_rgba_unmultiplied(8, 10, 14, 150)
            )),
            "no dimmed backdrop Rect shapes may be painted"
        );
        let _ = CaptureError::MonitorUnavailable {
            monitor_id: REGION_PROMPT_TITLE.to_string(),
        };

        // Frame 2: Esc key cancels selection cleanly
        let esc_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut cancel_outcome = RegionSelectorOutcome::Pending;
        let _ = ctx.run_ui(esc_input, |ui| {
            cancel_outcome = render_region_selector_ui(ui, &mut state, None);
        });
        assert_eq!(cancel_outcome, RegionSelectorOutcome::Cancelled);

        // Frame 3: Right-click while dragging also cancels cleanly
        let rmb_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            events: vec![
                egui::Event::PointerButton {
                    pos: egui::pos2(200.0, 820.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerMoved(egui::pos2(600.0, 900.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(600.0, 900.0),
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let mut rmb_outcome = RegionSelectorOutcome::Pending;
        let _ = ctx.run_ui(rmb_input, |ui| {
            rmb_outcome = render_region_selector_ui(ui, &mut state, None);
        });
        assert_eq!(rmb_outcome, RegionSelectorOutcome::Cancelled);
        assert_eq!(state.selection_rect_logical(), None);
    }
}
