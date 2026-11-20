//! Composite Windows system font loader (`Segoe UI` + `Microsoft YaHei`)
//! and color/opacity helpers for crisp CJK + Vietnamese subtitle rendering.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Font identifier registered for Vietnamese diacritics and UI controls.
pub const FONT_NAME_VIETNAMESE: &str = "cap2voice_vietnamese";

/// Font identifier registered for Simplified/Traditional Chinese subtitles.
pub const FONT_NAME_CJK: &str = "cap2voice_cjk";

/// Ordered Windows font filenames supporting full Vietnamese diacritics.
pub const VIETNAMESE_FONT_CANDIDATES: &[&str] = &["segoeui.ttf", "arial.ttf", "tahoma.ttf"];

/// Ordered Windows font filenames supporting Simplified and Traditional Chinese.
pub const CJK_FONT_CANDIDATES: &[&str] = &["msyh.ttc", "msyhl.ttc", "simsun.ttc", "simhei.ttf"];

/// Summary returned after probing and registering system fonts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontLoadReport {
    pub vietnamese_font: Option<PathBuf>,
    pub cjk_font: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// Parse a `#RRGGBB` hex string into an opaque `egui::Color32`.
pub fn parse_hex_color(hex: &str) -> Option<Color32> {
    let trimmed = hex.trim();
    let digits = trimmed.strip_prefix('#')?;
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let g = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let b = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

/// Convert normalized opacity (`0.0..=1.0`) to an 8-bit alpha channel.
pub fn opacity_to_alpha(opacity: f32) -> u8 {
    if !opacity.is_finite() {
        return 255;
    }
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn load_first_font(
    fonts_dir: &Path,
    candidates: &[&str],
    role_label: &str,
    warnings: &mut Vec<String>,
) -> Option<(PathBuf, Vec<u8>)> {
    let mut read_error: Option<String> = None;
    for filename in candidates {
        let path = fonts_dir.join(filename);
        if !path.is_file() {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => return Some((path, bytes)),
            Ok(_) => {
                read_error = Some(format!("Tệp font {role_label} {} bị rỗng", path.display()));
            }
            Err(err) => {
                read_error = Some(format!(
                    "Không thể đọc font {role_label} ({}): {err}",
                    path.display()
                ));
            }
        }
    }

    if let Some(err_msg) = read_error {
        warnings.push(err_msg);
    } else {
        warnings.push(format!(
            "Không tìm thấy font hệ thống cho {role_label} trong {} (đã thử: {})",
            fonts_dir.display(),
            candidates.join(", ")
        ));
    }
    None
}

/// Build `FontDefinitions` prioritizing a Vietnamese font and a CJK fallback font
/// discovered inside `fonts_dir`.
pub fn build_composite_font_definitions(fonts_dir: &Path) -> (FontDefinitions, FontLoadReport) {
    let mut defs = FontDefinitions::default();
    let mut report = FontLoadReport::default();
    let mut priority_ids: Vec<String> = Vec::with_capacity(2);

    if let Some((path, bytes)) = load_first_font(
        fonts_dir,
        VIETNAMESE_FONT_CANDIDATES,
        "tiếng Việt",
        &mut report.warnings,
    ) {
        defs.font_data.insert(
            FONT_NAME_VIETNAMESE.to_string(),
            Arc::new(FontData::from_owned(bytes)),
        );
        report.vietnamese_font = Some(path);
        priority_ids.push(FONT_NAME_VIETNAMESE.to_string());
    }

    if let Some((path, bytes)) = load_first_font(
        fonts_dir,
        CJK_FONT_CANDIDATES,
        "tiếng Trung (CJK)",
        &mut report.warnings,
    ) {
        defs.font_data.insert(
            FONT_NAME_CJK.to_string(),
            Arc::new(FontData::from_owned(bytes)),
        );
        report.cjk_font = Some(path);
        priority_ids.push(FONT_NAME_CJK.to_string());
    }

    if !priority_ids.is_empty() {
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let entry = defs.families.entry(family).or_default();
            for (idx, font_id) in priority_ids.iter().enumerate() {
                entry.insert(idx, font_id.clone());
            }
        }
    }

    (defs, report)
}

/// Resolve the Windows system fonts folder (`%WINDIR%\Fonts`).
pub fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("WINDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

/// Discover Windows system fonts in `%WINDIR%\Fonts` and install them onto `ctx`.
pub fn install_windows_fonts(ctx: &egui::Context) -> FontLoadReport {
    let (defs, report) = build_composite_font_definitions(&windows_fonts_dir());
    ctx.set_fonts(defs);
    report
}

#[cfg(test)]
mod tests {
    use super::{
        build_composite_font_definitions, opacity_to_alpha, parse_hex_color, FontLoadReport,
        FONT_NAME_CJK, FONT_NAME_VIETNAMESE,
    };
    use eframe::egui::{self, Color32, FontFamily, FontId};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "cap2voice_fonts_{label}_{}_{}_{}",
                std::process::id(),
                nanos,
                seq
            ));
            let Ok(()) = std::fs::create_dir_all(&dir) else {
                panic!("failed creating temp dir {}", dir.display());
            };
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_hex_color_parses_valid_rgb_and_rejects_malformed_inputs() {
        assert_eq!(
            parse_hex_color("#FFFFFF"),
            Some(Color32::from_rgb(255, 255, 255))
        );
        assert_eq!(
            parse_hex_color("#4ADE80"),
            Some(Color32::from_rgb(74, 222, 128))
        );
        assert_eq!(
            parse_hex_color("#4ade80"),
            Some(Color32::from_rgb(74, 222, 128))
        );
        assert_eq!(parse_hex_color("4ADE80"), None);
        assert_eq!(parse_hex_color("#GGG000"), None);
        assert_eq!(parse_hex_color("#FFF"), None);
        assert_eq!(opacity_to_alpha(0.85), 217);
        assert_eq!(opacity_to_alpha(0.0), 0);
        assert_eq!(opacity_to_alpha(1.0), 255);
    }

    #[test]
    fn build_composite_font_definitions_prioritizes_vietnamese_then_cjk_and_warns_when_missing() {
        let temp = TempDirGuard::new("fonts_order");
        let segoe = temp.path().join("segoeui.ttf");
        let msyh = temp.path().join("msyh.ttc");
        let Ok(()) = std::fs::write(&segoe, b"stub-segoe") else {
            panic!("failed writing stub segoeui.ttf");
        };
        let Ok(()) = std::fs::write(&msyh, b"stub-msyh") else {
            panic!("failed writing stub msyh.ttc");
        };

        let (defs, report) = build_composite_font_definitions(temp.path());
        assert_eq!(report.vietnamese_font.as_ref(), Some(&segoe));
        assert_eq!(report.cjk_font.as_ref(), Some(&msyh));
        assert!(report.warnings.is_empty());

        let prop = defs.families.get(&FontFamily::Proportional);
        assert!(prop.is_some());
        if let Some(list) = prop {
            assert_eq!(list.first().map(String::as_str), Some(FONT_NAME_VIETNAMESE));
            assert_eq!(list.get(1).map(String::as_str), Some(FONT_NAME_CJK));
        }
        let mono = defs.families.get(&FontFamily::Monospace);
        assert!(mono.is_some());
        if let Some(list) = mono {
            assert_eq!(list.first().map(String::as_str), Some(FONT_NAME_VIETNAMESE));
            assert_eq!(list.get(1).map(String::as_str), Some(FONT_NAME_CJK));
        }

        let empty = TempDirGuard::new("fonts_missing");
        let (_empty_defs, empty_report) = build_composite_font_definitions(empty.path());
        assert_eq!(
            empty_report,
            FontLoadReport {
                vietnamese_font: None,
                cjk_font: None,
                warnings: empty_report.warnings.clone(),
            }
        );
        assert_eq!(empty_report.warnings.len(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_system_fonts_cover_chinese_and_vietnamese_glyphs_without_tofu() {
        let ctx = egui::Context::default();
        let fonts_dir = super::windows_fonts_dir();
        let (defs, report) = build_composite_font_definitions(&fonts_dir);
        assert!(
            report.vietnamese_font.is_some(),
            "expected Windows Vietnamese font in {}: {:?}",
            fonts_dir.display(),
            report.warnings
        );
        assert!(
            report.cjk_font.is_some(),
            "expected Windows CJK font in {}: {:?}",
            fonts_dir.display(),
            report.warnings
        );
        assert!(
            report.warnings.is_empty(),
            "warnings: {:?}",
            report.warnings
        );

        ctx.set_fonts(defs);
        let zh_sample = "夜深了，雨还在下。";
        let vi_sample = "Đêm đã khuya, mưa vẫn còn rơi. ă â đ ê ô ơ ư ế ề ệ ử ữ ợ";

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let zh_galley = ui.fonts_mut(|f| {
                f.layout_no_wrap(
                    zh_sample.to_string(),
                    FontId::proportional(24.0),
                    Color32::WHITE,
                )
            });
            let vi_galley = ui.fonts_mut(|f| {
                f.layout_no_wrap(
                    vi_sample.to_string(),
                    FontId::proportional(18.0),
                    Color32::GREEN,
                )
            });
            assert!(zh_galley.size().x > 40.0);
            assert!(vi_galley.size().x > 40.0);

            for ch in zh_sample
                .chars()
                .chain(vi_sample.chars())
                .filter(|c| !c.is_whitespace())
            {
                let has_glyph = ui.fonts_mut(|f| f.has_glyph(&FontId::proportional(20.0), ch));
                assert!(has_glyph, "missing glyph (tofu) for character '{ch}'");
            }
        });
    }
}
