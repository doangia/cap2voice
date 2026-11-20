//! Procedural tray icon (32x32 RGBA, no asset file to ship).
//!
//! The icon is a dark-blue rounded square with two subtitle lines: a white
//! upper line (Chinese) and a green lower line (Vietnamese), matching the
//! overlay's two-line look.

use tray_icon::{BadIcon, Icon};

/// Tray icon edge length in pixels.
pub const TRAY_ICON_SIZE: u32 = 32;
/// Corner radius of the rounded-square background.
pub const TRAY_ICON_RADIUS: u32 = 7;

/// Background blue (`#1A56DB`).
pub const TRAY_ICON_BG: [u8; 4] = [0x1A, 0x56, 0xDB, 0xFF];
/// Upper subtitle line, white (Chinese line).
pub const TRAY_ICON_LINE_ZH: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
/// Lower subtitle line, green `#4ADE80` (Vietnamese line).
pub const TRAY_ICON_LINE_VI: [u8; 4] = [0x4A, 0xDE, 0x80, 0xFF];

fn in_rounded_rect(x: u32, y: u32, size: u32, radius: u32) -> bool {
    let (x, y, size, radius) = (x as i32, y as i32, size as i32, radius as i32);
    let inside_x = x >= radius && x < size - radius;
    let inside_y = y >= radius && y < size - radius;
    if inside_x || inside_y {
        return x >= 0 && x < size && y >= 0 && y < size;
    }
    let (cx, cy) = if x < radius && y < radius {
        (radius, radius)
    } else if x >= size - radius && y < radius {
        (size - radius - 1, radius)
    } else if x < radius && y >= size - radius {
        (radius, size - radius - 1)
    } else {
        (size - radius - 1, size - radius - 1)
    };
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= radius * radius
}

/// RGBA bytes for the 32x32 tray icon, row-major, top to bottom.
pub fn tray_icon_rgba() -> Vec<u8> {
    let size = TRAY_ICON_SIZE;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            if !in_rounded_rect(x, y, size, TRAY_ICON_RADIUS) {
                continue;
            }
            let px: [u8; 4] = if (7..25).contains(&x) && (11..14).contains(&y) {
                TRAY_ICON_LINE_ZH
            } else if (7..25).contains(&x) && (19..22).contains(&y) {
                TRAY_ICON_LINE_VI
            } else {
                TRAY_ICON_BG
            };
            let off = ((y * size + x) * 4) as usize;
            rgba[off..off + 4].copy_from_slice(&px);
        }
    }
    rgba
}

/// Build the `tray-icon` icon from the procedural RGBA buffer.
pub fn build_tray_icon() -> Result<Icon, BadIcon> {
    Icon::from_rgba(tray_icon_rgba(), TRAY_ICON_SIZE, TRAY_ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let off = ((y * TRAY_ICON_SIZE + x) * 4) as usize;
        [rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3]]
    }

    #[test]
    fn tray_icon_rgba_has_expected_length() {
        assert_eq!(
            tray_icon_rgba().len(),
            (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize
        );
    }

    #[test]
    fn tray_icon_corners_are_transparent() {
        let rgba = tray_icon_rgba();
        for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31)] {
            assert_eq!(pixel(&rgba, x, y)[3], 0, "corner ({x}, {y})");
        }
    }

    #[test]
    fn tray_icon_center_is_opaque_blue() {
        let rgba = tray_icon_rgba();
        assert_eq!(pixel(&rgba, 16, 16), TRAY_ICON_BG);
    }

    #[test]
    fn tray_icon_has_white_and_green_subtitle_lines() {
        let rgba = tray_icon_rgba();
        let mut white = 0;
        let mut green = 0;
        for y in 0..TRAY_ICON_SIZE {
            for x in 0..TRAY_ICON_SIZE {
                match pixel(&rgba, x, y) {
                    p if p == TRAY_ICON_LINE_ZH => white += 1,
                    p if p == TRAY_ICON_LINE_VI => green += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(white, 18 * 3, "white line pixels");
        assert_eq!(green, 18 * 3, "green line pixels");
    }

    #[test]
    fn build_tray_icon_accepts_the_rgba_buffer() {
        assert!(build_tray_icon().is_ok());
    }
}
