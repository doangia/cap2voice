//! Vector-drawn icon buttons (no font dependency).
//!
//! Every glyph is painted with `egui` shapes, so an icon can never render
//! as a tofu box (`□`) when a font lacks the code point. Used for the
//! overlay action bar and the Settings action buttons; each button pairs
//! with `.on_hover_text(...)` at the call site.

use eframe::egui;

/// Icon glyphs available as buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiIcon {
    /// Right-pointing play triangle (`Bắt đầu` / `Nghe thử`).
    Play,
    /// Filled square (`Dừng`).
    Stop,
    /// Speaker with sound waves (unmuted).
    Speaker,
    /// Speaker with an X (muted).
    SpeakerMuted,
    /// Three sliders (`Cài đặt`).
    Settings,
    /// Four corner brackets (`Chọn vùng`).
    Region,
    /// X (`Đóng` / `Huỷ`).
    Close,
    /// Checkmark (`Lưu thay đổi` / `Kiểm tra`).
    Check,
    /// Trash can (`Xóa bộ nhớ đệm`).
    Trash,
    /// Circular arrow (`Khôi phục mặc định`).
    Reset,
}

/// Square icon-button side in logical points (matches the overlay action
/// bar height).
pub const ICON_BUTTON_SIZE: f32 = 28.0;

/// Smaller square side for inline "Đóng" buttons in overlay info panels.
pub const ICON_BUTTON_SIZE_SMALL: f32 = 22.0;

/// Icon button widget: a rounded dark button (same look as the overlay
/// `action_button`) with a vector-drawn glyph. Tooltips are attached by
/// the caller via `.on_hover_text(...)` on the returned `Response`.
pub struct IconButton {
    icon: UiIcon,
    size: f32,
    base_fill: egui::Color32,
}

impl IconButton {
    pub fn new(icon: UiIcon) -> Self {
        Self {
            icon,
            size: ICON_BUTTON_SIZE,
            base_fill: egui::Color32::from_rgba_unmultiplied(40, 44, 52, 220),
        }
    }

    pub fn small(icon: UiIcon) -> Self {
        Self {
            icon,
            size: ICON_BUTTON_SIZE_SMALL,
            base_fill: egui::Color32::from_rgba_unmultiplied(40, 44, 52, 220),
        }
    }

    /// Use a custom base fill (e.g. the accent blue for the primary
    /// Save action); hover/pressed states shade it automatically.
    pub fn filled(mut self, color: egui::Color32) -> Self {
        self.base_fill = color;
        self
    }
}

fn arc_points(center: egui::Pos2, radius: f32, start_deg: f32, end_deg: f32) -> Vec<egui::Pos2> {
    const STEPS: usize = 12;
    (0..=STEPS)
        .map(|i| {
            let t = start_deg + (end_deg - start_deg) * i as f32 / STEPS as f32;
            let rad = t.to_radians();
            egui::pos2(center.x + radius * rad.cos(), center.y + radius * rad.sin())
        })
        .collect()
}

fn draw_icon(painter: &egui::Painter, icon: UiIcon, rect: egui::Rect, color: egui::Color32) {
    let pad = rect.width() * 0.22;
    let l = rect.left() + pad;
    let r = rect.right() - pad;
    let t = rect.top() + pad;
    let b = rect.bottom() - pad;
    let cx = rect.center().x;
    let cy = rect.center().y;
    let stroke = egui::Stroke::new(1.8, color);
    match icon {
        UiIcon::Play => {
            painter.add(egui::Shape::convex_polygon(
                vec![egui::pos2(l, t), egui::pos2(l, b), egui::pos2(r, cy)],
                color,
                egui::Stroke::NONE,
            ));
        }
        UiIcon::Stop => {
            painter.rect_filled(
                egui::Rect::from_min_max(egui::pos2(l, t), egui::pos2(r, b)),
                egui::CornerRadius::same(2),
                color,
            );
        }
        UiIcon::Speaker | UiIcon::SpeakerMuted => {
            let body = vec![
                egui::pos2(l, cy - 3.5),
                egui::pos2(l + 5.0, cy - 3.5),
                egui::pos2(l + 9.0, cy - 7.5),
                egui::pos2(l + 9.0, cy + 7.5),
                egui::pos2(l + 5.0, cy + 3.5),
                egui::pos2(l, cy + 3.5),
            ];
            painter.add(egui::Shape::convex_polygon(body, color, egui::Stroke::NONE));
            if icon == UiIcon::Speaker {
                for radius in [4.5, 8.0] {
                    painter.add(egui::Shape::line(
                        arc_points(egui::pos2(l + 9.0, cy), radius, -55.0, 55.0),
                        stroke,
                    ));
                }
            } else {
                let x0 = l + 12.0;
                let x1 = r;
                let y0 = cy - 5.0;
                let y1 = cy + 5.0;
                painter.line_segment([egui::pos2(x0, y0), egui::pos2(x1, y1)], stroke);
                painter.line_segment([egui::pos2(x0, y1), egui::pos2(x1, y0)], stroke);
            }
        }
        UiIcon::Settings => {
            for (i, y_frac) in [0.28, 0.5, 0.72].iter().enumerate() {
                let y = rect.top() + rect.height() * y_frac;
                painter.line_segment(
                    [egui::pos2(l, y), egui::pos2(r, y)],
                    egui::Stroke::new(1.6, color),
                );
                let knob_x = l + (r - l) * [0.32, 0.68, 0.45][i];
                painter.circle_filled(egui::pos2(knob_x, y), 2.6, color);
            }
        }
        UiIcon::Region => {
            let arm = rect.width() * 0.26;
            let corners = [
                (
                    egui::pos2(l, t + arm),
                    egui::pos2(l, t),
                    egui::pos2(l + arm, t),
                ),
                (
                    egui::pos2(r - arm, t),
                    egui::pos2(r, t),
                    egui::pos2(r, t + arm),
                ),
                (
                    egui::pos2(r, b - arm),
                    egui::pos2(r, b),
                    egui::pos2(r - arm, b),
                ),
                (
                    egui::pos2(l + arm, b),
                    egui::pos2(l, b),
                    egui::pos2(l, b - arm),
                ),
            ];
            for (a, b, c) in corners {
                painter.line_segment([a, b], stroke);
                painter.line_segment([b, c], stroke);
            }
        }
        UiIcon::Close => {
            painter.line_segment([egui::pos2(l, t), egui::pos2(r, b)], stroke);
            painter.line_segment([egui::pos2(r, t), egui::pos2(l, b)], stroke);
        }
        UiIcon::Check => {
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(l, cy),
                    egui::pos2(cx - 1.0, b - 1.0),
                    egui::pos2(r, t + 1.0),
                ],
                egui::Stroke::new(2.2, color),
            ));
        }
        UiIcon::Trash => {
            let lid_y = t + 1.5;
            painter.line_segment(
                [egui::pos2(l + 1.0, lid_y), egui::pos2(r - 1.0, lid_y)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(cx - 2.5, lid_y), egui::pos2(cx + 2.5, lid_y)],
                egui::Stroke::new(2.0, color),
            );
            painter.line_segment(
                [egui::pos2(cx, t - 1.0), egui::pos2(cx, lid_y)],
                egui::Stroke::new(1.6, color),
            );
            let body_top = lid_y + 3.0;
            painter.line_segment(
                [egui::pos2(l + 1.5, body_top), egui::pos2(r - 1.5, body_top)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(l + 1.5, body_top), egui::pos2(l + 3.5, b)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(r - 1.5, body_top), egui::pos2(r - 3.5, b)],
                stroke,
            );
            painter.line_segment([egui::pos2(l + 3.5, b), egui::pos2(r - 3.5, b)], stroke);
            painter.line_segment(
                [
                    egui::pos2(cx - 2.0, body_top + 2.5),
                    egui::pos2(cx - 2.0, b - 2.5),
                ],
                egui::Stroke::new(1.4, color),
            );
            painter.line_segment(
                [
                    egui::pos2(cx + 2.0, body_top + 2.5),
                    egui::pos2(cx + 2.0, b - 2.5),
                ],
                egui::Stroke::new(1.4, color),
            );
        }
        UiIcon::Reset => {
            painter.add(egui::Shape::line(
                arc_points(egui::pos2(cx, cy), rect.width() * 0.28, -60.0, 225.0),
                stroke,
            ));
            let tip = egui::pos2(
                cx + rect.width() * 0.28 * (225.0f32.to_radians().cos()),
                cy + rect.width() * 0.28 * (225.0f32.to_radians().sin()),
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    tip,
                    egui::pos2(tip.x - 1.0, tip.y - 6.0),
                    egui::pos2(tip.x + 5.5, tip.y - 1.5),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
    }
}

impl egui::Widget for IconButton {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(self.size, self.size), egui::Sense::click());
        let painter = ui.painter();
        let enabled = response.enabled();
        let bg = if !enabled {
            self.base_fill.gamma_multiply(0.55)
        } else if response.is_pointer_button_down_on() {
            self.base_fill.gamma_multiply(0.7)
        } else if response.hovered() {
            self.base_fill.gamma_multiply(1.35)
        } else {
            self.base_fill
        };
        painter.rect_filled(rect, egui::CornerRadius::same(8), bg);
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(8),
            egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
            ),
            egui::StrokeKind::Inside,
        );
        let icon_color = if enabled {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(120, 126, 138)
        };
        draw_icon(painter, self.icon, rect.shrink(2.0), icon_color);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::{IconButton, UiIcon};
    use eframe::egui;

    const ALL_ICONS: [UiIcon; 10] = [
        UiIcon::Play,
        UiIcon::Stop,
        UiIcon::Speaker,
        UiIcon::SpeakerMuted,
        UiIcon::Settings,
        UiIcon::Region,
        UiIcon::Close,
        UiIcon::Check,
        UiIcon::Trash,
        UiIcon::Reset,
    ];

    #[test]
    fn every_icon_renders_shapes_headlessly_without_panic() {
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 120.0));
        for icon in ALL_ICONS {
            let input = egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                ui.add(IconButton::new(icon));
            });
            assert!(
                !output.shapes.is_empty(),
                "expected shapes for icon {icon:?}"
            );
        }
    }

    #[test]
    fn icon_button_registers_click_at_its_center_and_ignores_click_when_disabled() {
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0));
        let rect_cell = std::cell::Cell::new(egui::Rect::NOTHING);
        let input0 = egui::RawInput {
            screen_rect: Some(screen_rect),
            ..Default::default()
        };
        let _ = ctx.run_ui(input0, |ui| {
            let resp = ui.add(IconButton::new(UiIcon::Play));
            rect_cell.set(resp.rect);
        });
        let center = rect_cell.get().center();
        assert!(center.x > 0.0 && center.y > 0.0, "button rect: {center:?}");

        let click_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            events: vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = ctx.run_ui(click_input, |ui| {
            let resp = ui.add(IconButton::new(UiIcon::Play));
            assert!(resp.clicked(), "expected click at center {center:?}");
        });

        let disabled_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            events: vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = ctx.run_ui(disabled_input, |ui| {
            let resp = ui.add_enabled(false, IconButton::new(UiIcon::Play));
            assert!(
                !resp.clicked(),
                "disabled icon button must not report a click"
            );
        });
    }
}
