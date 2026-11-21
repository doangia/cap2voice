//! Screen capture abstraction, DPI/monitor coordinate validation, and frame diffing.

pub mod pixel_diff;
pub mod xcap_impl;

pub use pixel_diff::{PixelDiffConfig, PixelDiffGate};
pub use xcap_impl::XcapCapturer;

use crate::config::CaptureRegion;
use std::path::Path;
use thiserror::Error;

/// Minimum allowed subtitle capture region dimensions in physical pixels.
pub const MIN_REGION_WIDTH: u32 = 20;
pub const MIN_REGION_HEIGHT: u32 = 16;

/// Metadata describing an active OS display monitor.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    pub id: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
}

/// Monitor-local cropped pixel rectangle (`x, y >= 0` relative to monitor top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Captured RGBA frame buffer (`4 * width * height` bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Typed screen capture errors surfaced explicitly to the pipeline and UI.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureError {
    #[error("vùng phụ đề quá nhỏ ({width}x{height} px; tối thiểu {MIN_REGION_WIDTH}x{MIN_REGION_HEIGHT} px)")]
    InvalidRegion { width: u32, height: u32 },

    #[error("không tìm thấy màn hình '{monitor_id}' — hãy chọn lại vùng phụ đề")]
    MonitorUnavailable { monitor_id: String },

    #[error(
        "vùng phụ đề ({x}, {y}, {width}x{height}) nằm ngoài màn hình '{monitor_id}' ({mon_x}, {mon_y}, {mon_width}x{mon_height}) — hãy chọn lại vùng"
    )]
    OutOfBounds {
        monitor_id: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        mon_x: i32,
        mon_y: i32,
        mon_width: u32,
        mon_height: u32,
    },

    #[error("kích thước buffer ảnh chụp không hợp lệ ({width}x{height}, len={actual_len})")]
    InvalidFrameBuffer {
        width: u32,
        height: u32,
        actual_len: usize,
    },

    #[error("lỗi chụp màn hình: {0}")]
    Backend(String),

    #[error("lỗi ghi ảnh chụp ra {path}: {detail}")]
    Io { path: String, detail: String },
}

/// Trait abstracting screen region capture.
///
/// The only production implementation is `xcap`; there is no `windows-capture`
/// fallback. The trait exists so tests can substitute a fake capturer.
pub trait FrameCapturer: Send + Sync {
    fn list_monitors(&self) -> Result<Vec<MonitorInfo>, CaptureError>;
    fn capture_region(&self, region: &CaptureRegion) -> Result<CapturedFrame, CaptureError>;
}

impl CapturedFrame {
    /// Construct a validated RGBA frame buffer.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, CaptureError> {
        let actual_len = rgba.len();
        if width == 0 || height == 0 {
            return Err(CaptureError::InvalidFrameBuffer {
                width,
                height,
                actual_len,
            });
        }
        let expected_len = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|px| px.checked_mul(4));
        if expected_len != Some(actual_len) {
            return Err(CaptureError::InvalidFrameBuffer {
                width,
                height,
                actual_len,
            });
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// Encode and write this RGBA frame to a PNG file on disk.
    pub fn save_png(&self, path: &Path) -> Result<(), CaptureError> {
        let Some(img) = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
        else {
            return Err(CaptureError::InvalidFrameBuffer {
                width: self.width,
                height: self.height,
                actual_len: self.rgba.len(),
            });
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| CaptureError::Io {
                    path: parent.display().to_string(),
                    detail: e.to_string(),
                })?;
            }
        }
        img.save(path).map_err(|e| CaptureError::Io {
            path: path.display().to_string(),
            detail: e.to_string(),
        })
    }
}

/// Validate minimum physical capture region dimensions.
pub fn validate_region_dimensions(width: u32, height: u32) -> Result<(), CaptureError> {
    if width < MIN_REGION_WIDTH || height < MIN_REGION_HEIGHT {
        return Err(CaptureError::InvalidRegion { width, height });
    }
    Ok(())
}

/// Validate `region` dimensions and resolve its monitor-local physical crop rectangle.
pub fn resolve_monitor_crop(
    region: &CaptureRegion,
    monitors: &[MonitorInfo],
) -> Result<(usize, LocalCropRect), CaptureError> {
    validate_region_dimensions(region.width, region.height)?;

    let Some((idx, monitor)) = monitors
        .iter()
        .enumerate()
        .find(|(_, m)| m.id == region.monitor_id)
    else {
        return Err(CaptureError::MonitorUnavailable {
            monitor_id: region.monitor_id.clone(),
        });
    };

    let rx0 = i64::from(region.x);
    let ry0 = i64::from(region.y);
    let rx1 = rx0 + i64::from(region.width);
    let ry1 = ry0 + i64::from(region.height);

    let mx0 = i64::from(monitor.x);
    let my0 = i64::from(monitor.y);
    let mx1 = mx0 + i64::from(monitor.width);
    let my1 = my0 + i64::from(monitor.height);

    let make_oob = || CaptureError::OutOfBounds {
        monitor_id: region.monitor_id.clone(),
        x: region.x,
        y: region.y,
        width: region.width,
        height: region.height,
        mon_x: monitor.x,
        mon_y: monitor.y,
        mon_width: monitor.width,
        mon_height: monitor.height,
    };

    if rx0 < mx0 || ry0 < my0 || rx1 > mx1 || ry1 > my1 {
        return Err(make_oob());
    }

    let local_x = u32::try_from(rx0 - mx0).map_err(|_| make_oob())?;
    let local_y = u32::try_from(ry0 - my0).map_err(|_| make_oob())?;

    Ok((
        idx,
        LocalCropRect {
            x: local_x,
            y: local_y,
            width: region.width,
            height: region.height,
        },
    ))
}

fn round_non_negative_u32(value: f32) -> Result<u32, CaptureError> {
    if !value.is_finite() || value < 0.0 || value > u32::MAX as f32 {
        return Err(CaptureError::InvalidRegion {
            width: 0,
            height: 0,
        });
    }
    Ok(value.round() as u32)
}

/// Convert logical UI drag bounds relative to a monitor into a physical `CaptureRegion`.
pub fn logical_rect_to_physical(
    monitor: &MonitorInfo,
    logical_x: f32,
    logical_y: f32,
    logical_width: f32,
    logical_height: f32,
) -> Result<CaptureRegion, CaptureError> {
    if !monitor.scale_factor.is_finite() || monitor.scale_factor <= 0.0 {
        return Err(CaptureError::Backend(format!(
            "invalid monitor scale factor {}",
            monitor.scale_factor
        )));
    }
    if !logical_x.is_finite()
        || !logical_y.is_finite()
        || !logical_width.is_finite()
        || !logical_height.is_finite()
    {
        return Err(CaptureError::InvalidRegion {
            width: 0,
            height: 0,
        });
    }

    let scale = monitor.scale_factor;
    let width = round_non_negative_u32(logical_width * scale)?;
    let height = round_non_negative_u32(logical_height * scale)?;
    validate_region_dimensions(width, height)?;

    let rel_x_rounded = (logical_x * scale).round();
    let rel_y_rounded = (logical_y * scale).round();
    if rel_x_rounded < i32::MIN as f32
        || rel_x_rounded > i32::MAX as f32
        || rel_y_rounded < i32::MIN as f32
        || rel_y_rounded > i32::MAX as f32
    {
        return Err(CaptureError::InvalidRegion { width, height });
    }
    let rel_x = rel_x_rounded as i32;
    let rel_y = rel_y_rounded as i32;

    let region = CaptureRegion {
        monitor_id: monitor.id.clone(),
        x: monitor.x.saturating_add(rel_x),
        y: monitor.y.saturating_add(rel_y),
        width,
        height,
    };
    let _ = resolve_monitor_crop(&region, std::slice::from_ref(monitor))?;
    Ok(region)
}

#[cfg(test)]
mod tests {
    use super::{
        logical_rect_to_physical, resolve_monitor_crop, CaptureError, CapturedFrame, LocalCropRect,
        MonitorInfo,
    };
    use crate::config::CaptureRegion;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn sample_monitors() -> Vec<MonitorInfo> {
        vec![
            MonitorInfo {
                id: r"\\.\DISPLAY1".to_string(),
                name: "Primary".to_string(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 1.5,
                is_primary: true,
            },
            MonitorInfo {
                id: r"\\.\DISPLAY2".to_string(),
                name: "Secondary Left".to_string(),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 1.0,
                is_primary: false,
            },
        ]
    }

    #[test]
    fn resolve_monitor_crop_maps_primary_and_secondary_negative_offset_monitors() {
        let monitors = sample_monitors();

        let primary_region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 320,
            y: 960,
            width: 1280,
            height: 60,
        };
        assert_eq!(
            resolve_monitor_crop(&primary_region, &monitors),
            Ok((
                0,
                LocalCropRect {
                    x: 320,
                    y: 960,
                    width: 1280,
                    height: 60,
                }
            ))
        );

        let left_region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY2".to_string(),
            x: -1600,
            y: 980,
            width: 1280,
            height: 60,
        };
        assert_eq!(
            resolve_monitor_crop(&left_region, &monitors),
            Ok((
                1,
                LocalCropRect {
                    x: 320,
                    y: 980,
                    width: 1280,
                    height: 60,
                }
            ))
        );
    }

    #[test]
    fn resolve_monitor_crop_rejects_disconnected_monitor() {
        let monitors = sample_monitors();
        let region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY99".to_string(),
            x: 100,
            y: 100,
            width: 200,
            height: 40,
        };
        assert_eq!(
            resolve_monitor_crop(&region, &monitors),
            Err(CaptureError::MonitorUnavailable {
                monitor_id: r"\\.\DISPLAY99".to_string(),
            })
        );
    }

    #[test]
    fn resolve_monitor_crop_rejects_out_of_bounds_region() {
        let monitors = sample_monitors();
        let beyond_right = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 1800,
            y: 900,
            width: 200,
            height: 60,
        };
        assert!(matches!(
            resolve_monitor_crop(&beyond_right, &monitors),
            Err(CaptureError::OutOfBounds { .. })
        ));

        let before_left = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: -10,
            y: 100,
            width: 100,
            height: 40,
        };
        assert!(matches!(
            resolve_monitor_crop(&before_left, &monitors),
            Err(CaptureError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn resolve_monitor_crop_rejects_region_smaller_than_minimum() {
        let monitors = sample_monitors();
        let tiny = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 100,
            y: 100,
            width: 19,
            height: 16,
        };
        assert_eq!(
            resolve_monitor_crop(&tiny, &monitors),
            Err(CaptureError::InvalidRegion {
                width: 19,
                height: 16,
            })
        );
    }

    #[test]
    fn logical_rect_to_physical_scales_accurately_across_windows_dpi_factors() {
        let cases = [
            (
                1.0f32, 0, 0, 100.0f32, 200.0f32, 400.0f32, 40.0f32, 100, 200, 400, 40,
            ),
            (
                1.25f32, 0, 0, 100.0f32, 200.0f32, 400.0f32, 40.0f32, 125, 250, 500, 50,
            ),
            (
                1.5f32, 1920, 0, 100.0f32, 200.0f32, 400.0f32, 40.0f32, 2070, 300, 600, 60,
            ),
            (
                1.75f32, 0, 0, 100.0f32, 200.0f32, 400.0f32, 40.0f32, 175, 350, 700, 70,
            ),
        ];

        for (scale, mon_x, mon_y, lx, ly, lw, lh, ex, ey, ew, eh) in cases {
            let monitor = MonitorInfo {
                id: r"\\.\DISPLAY1".to_string(),
                name: "Scaled".to_string(),
                x: mon_x,
                y: mon_y,
                width: 3840,
                height: 2160,
                scale_factor: scale,
                is_primary: true,
            };
            let region = logical_rect_to_physical(&monitor, lx, ly, lw, lh);
            assert_eq!(
                region,
                Ok(CaptureRegion {
                    monitor_id: r"\\.\DISPLAY1".to_string(),
                    x: ex,
                    y: ey,
                    width: ew,
                    height: eh,
                })
            );
        }
    }

    #[test]
    fn captured_frame_validates_buffer_size_and_saves_png_roundtrip() {
        assert_eq!(
            CapturedFrame::new(4, 4, vec![0; 10]),
            Err(CaptureError::InvalidFrameBuffer {
                width: 4,
                height: 4,
                actual_len: 10,
            })
        );

        let mut rgba = Vec::with_capacity(2 * 2 * 4);
        rgba.extend_from_slice(&[255, 0, 0, 255]);
        rgba.extend_from_slice(&[0, 255, 0, 255]);
        rgba.extend_from_slice(&[0, 0, 255, 255]);
        rgba.extend_from_slice(&[255, 255, 255, 255]);

        let Ok(frame) = CapturedFrame::new(2, 2, rgba.clone()) else {
            panic!("valid 2x2 RGBA buffer should construct CapturedFrame");
        };

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir: PathBuf = std::env::temp_dir().join(format!(
            "cap2voice_frame_test_{}_{}_{}",
            std::process::id(),
            nanos,
            seq
        ));
        let png_path = dir.join("crop.png");

        assert_eq!(frame.save_png(&png_path), Ok(()));
        let Ok(loaded) = image::open(&png_path) else {
            panic!("saved PNG {} should open cleanly", png_path.display());
        };
        let loaded_rgba = loaded.to_rgba8();
        assert_eq!(loaded_rgba.dimensions(), (2, 2));
        assert_eq!(loaded_rgba.as_raw().as_slice(), rgba.as_slice());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
