//! Separate transparent subtitle window shown exactly over the capture
//! region when in-place mode is ON (Settings → Hiển thị,
//! `display.inplace_subtitle`). It is an ADDITIONAL window: the floating
//! overlay stays visible and usable at all times.
//!
//! The window is borderless, fully transparent, click-through
//! (`mouse_passthrough`), and excluded from screen capture (`WDA`) so the
//! OCR pipeline keeps seeing the original video subtitle underneath —
//! without the exclusion the background rectangle would hide the original
//! subtitle from the capture and the pipeline would stall on the old line.

use crate::config::{AppConfig, CaptureRegion};
use crate::ui::fonts::parse_hex_color;
use crate::ui::overlay::{CHINESE_FONT_SCALE, PINYIN_FONT_SCALE, PINYIN_TEXT_COLOR};
use eframe::egui;

/// Window title of the in-place subtitle viewport (also the `FindWindowW`
/// key used to apply `WDA_EXCLUDEFROMCAPTURE` to it).
pub const INPLACE_WINDOW_TITLE: &str = "cap2voice-inplace-subtitle";

/// Viewport ID used by `egui::Context::show_viewport_immediate` for the
/// in-place subtitle window.
pub fn inplace_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("cap2voice_inplace_subtitle")
}

/// Convert a physical capture region to logical viewport position + size.
pub fn region_to_logical_rect(region: &CaptureRegion, scale: f32) -> (egui::Pos2, egui::Vec2) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (
        egui::pos2(region.x as f32 / scale, region.y as f32 / scale),
        egui::vec2(region.width as f32 / scale, region.height as f32 / scale),
    )
}

/// Resolve the logical rect for a region: look the region's monitor up
/// in `monitors` for its DPI scale, falling back to `fallback_scale`
/// (usually `ctx.pixels_per_point()`) when the monitor is unknown.
pub fn region_logical_rect_with_monitors(
    region: &CaptureRegion,
    monitors: &[crate::capture::MonitorInfo],
    fallback_scale: f32,
) -> (egui::Pos2, egui::Vec2) {
    let scale = monitors
        .iter()
        .find(|m| m.id == region.monitor_id)
        .map(|m| m.scale_factor)
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(fallback_scale);
    region_to_logical_rect(region, scale)
}
pub fn build_inplace_viewport_builder(pos: egui::Pos2, size: egui::Vec2) -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_title(INPLACE_WINDOW_TITLE)
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_taskbar(false)
        .with_active(false)
        .with_mouse_passthrough(true)
        .with_window_level(egui::WindowLevel::AlwaysOnTop)
        .with_clamp_size_to_monitor_size(false)
        .with_position(pos)
        .with_inner_size(size)
}

/// Scale factor (`<= 1.0`) so the subtitle lines fit the region height at
/// most `font_size_px`. `chinese_lines` / `pinyin_lines` /
/// `vietnamese_lines` are the numbers of lines actually shown (0 hides
/// that part); a 2-line frame counts 2 Chinese + 2 Vietnamese lines, so
/// the font shrinks to fit them all instead of overflowing the region
/// (2-line bug fix). The pinyin line is a single space-joined line even
/// for multi-line Chinese, so it is 0 or 1.
pub fn inplace_font_scale(
    region_height_logical: f32,
    font_size_px: f32,
    chinese_lines: u32,
    pinyin_lines: u32,
    vietnamese_lines: u32,
) -> f32 {
    let base = font_size_px.clamp(12.0, 40.0);
    let mut needed = 0.0f32;
    let mut lines = 0u32;
    if pinyin_lines > 0 {
        needed += base * CHINESE_FONT_SCALE * PINYIN_FONT_SCALE * pinyin_lines as f32;
        lines += pinyin_lines;
    }
    if chinese_lines > 0 {
        needed += base * CHINESE_FONT_SCALE * chinese_lines as f32;
        lines += chinese_lines;
    }
    if vietnamese_lines > 0 {
        needed += base * vietnamese_lines as f32;
        lines += vietnamese_lines;
    }
    if lines == 0 {
        return 1.0;
    }
    // 4 px gap between lines plus ~8 px vertical padding.
    needed += (lines.saturating_sub(1)) as f32 * 4.0 + 8.0;
    if !region_height_logical.is_finite() || region_height_logical <= 0.0 {
        return 1.0;
    }
    (region_height_logical / needed).clamp(0.2, 1.0)
}

/// Render the in-place subtitle content. Returns `true` when anything was
/// drawn. Draws nothing when both lines are empty, so the window stays
/// fully invisible while idle.
pub fn render_inplace_subtitle_ui(
    ui: &mut egui::Ui,
    chinese_line: &str,
    pinyin_line: &str,
    vietnamese_line: &str,
    config: &AppConfig,
) -> bool {
    let show_zh = config.display.show_chinese_line && !chinese_line.is_empty();
    let show_py = show_zh && config.display.show_pinyin_line && !pinyin_line.is_empty();
    let show_vi = config.display.show_vietnamese_line && !vietnamese_line.is_empty();
    if !show_zh && !show_vi {
        return false;
    }

    let full = ui.max_rect();
    let alpha = (config.display.inplace_bg_opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    ui.painter().rect_filled(
        full,
        egui::CornerRadius::ZERO,
        egui::Color32::from_rgba_unmultiplied(20, 22, 26, alpha),
    );

    let zh_lines = if show_zh {
        chinese_line.matches('\n').count() as u32 + 1
    } else {
        0
    };
    let py_lines = if show_py { 1 } else { 0 };
    let vi_lines = if show_vi {
        vietnamese_line.matches('\n').count() as u32 + 1
    } else {
        0
    };
    let scale = inplace_font_scale(
        full.height(),
        config.display.font_size_px,
        zh_lines,
        py_lines,
        vi_lines,
    );
    let vi_size = config.display.font_size_px.clamp(12.0, 40.0) * scale;
    let zh_size = vi_size * CHINESE_FONT_SCALE;
    let zh_color =
        parse_hex_color(&config.display.chinese_color_hex).unwrap_or(egui::Color32::WHITE);
    let vi_color = parse_hex_color(&config.display.vietnamese_color_hex)
        .unwrap_or(egui::Color32::from_rgb(74, 222, 128));

    ui.vertical_centered(|ui| {
        ui.set_min_width(ui.available_width());
        if show_py {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(pinyin_line)
                        .size(zh_size * PINYIN_FONT_SCALE)
                        .color(PINYIN_TEXT_COLOR),
                )
                .wrap(),
            );
            ui.add_space(2.0);
        }
        if show_zh {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(chinese_line)
                        .size(zh_size)
                        .color(zh_color),
                )
                .wrap(),
            );
        }
        if show_vi {
            if show_zh {
                ui.add_space(4.0);
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(vietnamese_line)
                        .size(vi_size)
                        .color(vi_color),
                )
                .wrap(),
            );
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::{
        build_inplace_viewport_builder, inplace_font_scale, region_logical_rect_with_monitors,
        region_to_logical_rect, render_inplace_subtitle_ui, INPLACE_WINDOW_TITLE,
    };
    use crate::capture::MonitorInfo;
    use crate::config::{AppConfig, CaptureRegion};
    use eframe::egui;

    fn test_region() -> CaptureRegion {
        CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 200,
            y: 380,
            width: 560,
            height: 140,
        }
    }

    #[test]
    fn region_to_logical_rect_divides_by_monitor_scale() {
        assert_eq!(
            region_to_logical_rect(&test_region(), 1.0),
            (egui::pos2(200.0, 380.0), egui::vec2(560.0, 140.0))
        );
        assert_eq!(
            region_to_logical_rect(&test_region(), 1.5),
            (
                egui::pos2(200.0 / 1.5, 380.0 / 1.5),
                egui::vec2(560.0 / 1.5, 140.0 / 1.5)
            )
        );
        // Invalid scale falls back to 1.0 instead of producing infinities.
        assert_eq!(
            region_to_logical_rect(&test_region(), 0.0),
            (egui::pos2(200.0, 380.0), egui::vec2(560.0, 140.0))
        );
        let negative = CaptureRegion {
            monitor_id: "DISPLAY2".to_string(),
            x: -1745,
            y: 275,
            width: 1745,
            height: 905,
        };
        assert_eq!(
            region_to_logical_rect(&negative, 1.75),
            (
                egui::pos2(-1745.0 / 1.75, 275.0 / 1.75),
                egui::vec2(1745.0 / 1.75, 905.0 / 1.75)
            )
        );
    }

    #[test]
    fn region_logical_rect_with_monitors_uses_monitor_scale_and_falls_back() {
        let monitors = vec![
            MonitorInfo {
                id: r"\\.\DISPLAY1".to_string(),
                name: "DISPLAY1".to_string(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 1.25,
                is_primary: true,
            },
            MonitorInfo {
                id: r"\\.\DISPLAY2".to_string(),
                name: "DISPLAY2".to_string(),
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
                scale_factor: 1.5,
                is_primary: false,
            },
        ];
        assert_eq!(
            region_logical_rect_with_monitors(&test_region(), &monitors, 1.0),
            (
                egui::pos2(200.0 / 1.25, 380.0 / 1.25),
                egui::vec2(560.0 / 1.25, 140.0 / 1.25)
            )
        );
        // Unknown monitor id falls back to the given scale.
        let unknown = CaptureRegion {
            monitor_id: "DISPLAY9".to_string(),
            x: 100,
            y: 100,
            width: 400,
            height: 100,
        };
        assert_eq!(
            region_logical_rect_with_monitors(&unknown, &monitors, 2.0),
            (egui::pos2(50.0, 50.0), egui::vec2(200.0, 50.0))
        );
    }

    #[test]
    fn build_inplace_viewport_builder_is_borderless_transparent_click_through_on_top() {
        let vb = build_inplace_viewport_builder(egui::pos2(200.0, 380.0), egui::vec2(560.0, 140.0));
        assert_eq!(vb.title.as_deref(), Some(INPLACE_WINDOW_TITLE));
        assert_eq!(vb.decorations, Some(false));
        assert_eq!(vb.transparent, Some(true));
        assert_eq!(vb.resizable, Some(false));
        assert_eq!(vb.taskbar, Some(false));
        assert_eq!(vb.active, Some(false));
        assert_eq!(vb.mouse_passthrough, Some(true));
        assert_eq!(vb.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!(vb.clamp_size_to_monitor_size, Some(false));
        assert_eq!(vb.position, Some(egui::pos2(200.0, 380.0)));
        assert_eq!(vb.inner_size, Some(egui::vec2(560.0, 140.0)));
    }

    #[test]
    fn inplace_font_scale_never_enlarges_and_shrinks_for_short_regions() {
        // Tall region: full size.
        assert!((inplace_font_scale(140.0, 18.0, 1, 1, 1) - 1.0).abs() < 1e-6);
        // Short region (60 px): 3 lines at 18 px need ~62 px -> scale < 1.
        let s = inplace_font_scale(60.0, 18.0, 1, 1, 1);
        assert!((0.2..1.0).contains(&s), "scale for 60px region: {s}");
        // Very short region clamps at 0.2 rather than vanishing.
        assert!((inplace_font_scale(10.0, 18.0, 1, 1, 1) - 0.2).abs() < 1e-6);
        // One line only needs less height, so it stays full size longer.
        assert!((inplace_font_scale(60.0, 18.0, 0, 0, 1) - 1.0).abs() < 1e-6);
        // No lines: neutral scale.
        assert!((inplace_font_scale(60.0, 18.0, 0, 0, 0) - 1.0).abs() < 1e-6);
        // Two-line frame needs more height than one line at the same size:
        // at 100 px a single line stays full size but 2+2 lines shrink.
        assert!((inplace_font_scale(100.0, 18.0, 1, 1, 1) - 1.0).abs() < 1e-6);
        let s2 = inplace_font_scale(100.0, 18.0, 2, 1, 2);
        assert!((0.2..1.0).contains(&s2), "scale for two-line frame: {s2}");
    }

    #[test]
    fn render_inplace_subtitle_ui_draws_lines_and_stays_empty_when_idle() {
        let ctx = egui::Context::default();
        let cfg = AppConfig::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(560.0, 140.0));

        let mut drew = false;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                drew = render_inplace_subtitle_ui(
                    ui,
                    "夜深了，雨还在下。",
                    "yè shēn le, yǔ hái zài xià。",
                    "Đêm đã khuya, mưa vẫn còn rơi.",
                    &cfg,
                );
            },
        );
        assert!(drew);
        assert!(!output.shapes.is_empty());

        let mut idle_drew = true;
        let idle_output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| {
                idle_drew = render_inplace_subtitle_ui(ui, "", "", "", &cfg);
            },
        );
        assert!(!idle_drew);
        assert!(
            idle_output.shapes.is_empty(),
            "idle in-place window must paint nothing"
        );
    }
}
