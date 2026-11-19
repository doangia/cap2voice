//! Pre-OCR grayscale downsampled pixel-diff gate.

use super::CapturedFrame;

/// Configuration controlling pre-OCR pixel change detection.
#[derive(Debug, Clone, PartialEq)]
pub struct PixelDiffConfig {
    /// Minimum fraction of sampled pixels that must differ to trigger OCR (default `0.02`).
    pub changed_ratio_threshold: f32,
    /// Minimum grayscale luminance delta (`0..=255`) for a sampled pixel to count as changed.
    pub luma_delta_threshold: u8,
    /// Spatial sampling stride on large frames (`2` = sample every 2nd pixel per axis).
    pub downsample_step: u32,
}

impl Default for PixelDiffConfig {
    fn default() -> Self {
        Self {
            changed_ratio_threshold: 0.02,
            luma_delta_threshold: 16,
            downsample_step: 2,
        }
    }
}

/// Stateful pixel-difference gate comparing incoming frames against the last frame sent to OCR.
#[derive(Debug, Clone)]
pub struct PixelDiffGate {
    config: PixelDiffConfig,
    last_accepted: Option<(u32, u32, Vec<u8>)>,
}

impl PixelDiffGate {
    pub fn new(config: PixelDiffConfig) -> Self {
        Self {
            config,
            last_accepted: None,
        }
    }

    pub fn with_threshold(changed_ratio_threshold: f32) -> Self {
        Self::new(PixelDiffConfig {
            changed_ratio_threshold,
            ..PixelDiffConfig::default()
        })
    }

    /// Clear previous frame memory so the next captured frame always runs OCR.
    pub fn reset(&mut self) {
        self.last_accepted = None;
    }

    /// Return `true` when `frame` changed enough relative to the last OCR'd frame.
    pub fn should_run_ocr(&mut self, frame: &CapturedFrame) -> bool {
        let current = downsample_luma(frame, self.config.downsample_step);
        if current.is_empty() {
            return false;
        }

        if let Some((prev_w, prev_h, prev_luma)) = &self.last_accepted {
            if *prev_w == frame.width && *prev_h == frame.height && prev_luma.len() == current.len()
            {
                let delta = self.config.luma_delta_threshold;
                let changed = current
                    .iter()
                    .zip(prev_luma.iter())
                    .filter(|(a, b)| a.abs_diff(**b) > delta)
                    .count();
                let ratio = changed as f32 / current.len() as f32;
                if ratio < self.config.changed_ratio_threshold {
                    return false;
                }
            }
        }

        self.last_accepted = Some((frame.width, frame.height, current));
        true
    }
}

fn downsample_luma(frame: &CapturedFrame, configured_step: u32) -> Vec<u8> {
    let expected_len = (frame.width as usize)
        .saturating_mul(frame.height as usize)
        .saturating_mul(4);
    if frame.width == 0 || frame.height == 0 || frame.rgba.len() < expected_len {
        return Vec::new();
    }

    let stride = if frame.width >= 64 && frame.height >= 16 {
        configured_step.max(1) as usize
    } else {
        1
    };
    let w = frame.width as usize;
    let h = frame.height as usize;
    let est = (w / stride + 1).saturating_mul(h / stride + 1);
    let mut luma = Vec::with_capacity(est);

    for y in (0..h).step_by(stride) {
        let row_base = y * w * 4;
        for x in (0..w).step_by(stride) {
            let idx = row_base + x * 4;
            let r = u32::from(frame.rgba[idx]);
            let g = u32::from(frame.rgba[idx + 1]);
            let b = u32::from(frame.rgba[idx + 2]);
            let y_val = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
            luma.push(y_val);
        }
    }
    luma
}

#[cfg(test)]
mod tests {
    use super::{PixelDiffConfig, PixelDiffGate};
    use crate::capture::CapturedFrame;

    fn solid_frame(width: u32, height: u32, r: u8, g: u8, b: u8) -> CapturedFrame {
        let pixel_count = (width as usize) * (height as usize);
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for _ in 0..pixel_count {
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
        let Ok(frame) = CapturedFrame::new(width, height, rgba) else {
            panic!("failed creating test frame {width}x{height}");
        };
        frame
    }

    fn paint_rect(frame: &mut CapturedFrame, rx: u32, ry: u32, rw: u32, rh: u32, rgb: [u8; 3]) {
        for y in ry..(ry + rh).min(frame.height) {
            for x in rx..(rx + rw).min(frame.width) {
                let idx = ((y * frame.width + x) * 4) as usize;
                frame.rgba[idx] = rgb[0];
                frame.rgba[idx + 1] = rgb[1];
                frame.rgba[idx + 2] = rgb[2];
                frame.rgba[idx + 3] = 255;
            }
        }
    }

    #[test]
    fn first_frame_always_bypasses_gate_even_if_blank() {
        let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
        let blank = solid_frame(1280, 60, 0, 0, 0);
        assert!(gate.should_run_ocr(&blank));
    }

    #[test]
    fn identical_static_frames_skip_at_least_99_percent_after_first() {
        let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
        let frame = solid_frame(1280, 60, 18, 22, 30);

        assert!(gate.should_run_ocr(&frame));
        let mut skipped = 0usize;
        let repeats = 200usize;
        for _ in 0..repeats {
            if !gate.should_run_ocr(&frame) {
                skipped += 1;
            }
        }
        let skip_ratio = skipped as f32 / repeats as f32;
        assert!(
            skip_ratio >= 0.99,
            "expected >= 99% static frame suppression, got {:.2}%",
            skip_ratio * 100.0
        );
    }

    #[test]
    fn compression_noise_below_luma_threshold_is_skipped() {
        let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
        let f1 = solid_frame(320, 60, 40, 40, 40);
        let f2 = solid_frame(320, 60, 45, 45, 45);

        assert!(gate.should_run_ocr(&f1));
        assert!(!gate.should_run_ocr(&f2));
    }

    #[test]
    fn subtitle_text_change_triggers_ocr_immediately() {
        let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
        let base = solid_frame(1280, 60, 16, 16, 20);
        let mut with_sub = base.clone();
        // ~4.7% of pixels change to bright subtitle text (> 2% threshold).
        paint_rect(&mut with_sub, 340, 15, 600, 6, [255, 255, 255]);

        assert!(gate.should_run_ocr(&base));
        assert!(!gate.should_run_ocr(&base));
        assert!(gate.should_run_ocr(&with_sub));
        assert!(!gate.should_run_ocr(&with_sub));
    }

    #[test]
    fn gradual_fade_in_triggers_against_last_ocrd_frame() {
        let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
        let f0 = solid_frame(128, 32, 20, 20, 20);
        let f1 = solid_frame(128, 32, 30, 30, 30);
        let f2 = solid_frame(128, 32, 42, 42, 42);

        assert!(gate.should_run_ocr(&f0));
        // +10 luma is below 16 -> skipped, reference remains f0.
        assert!(!gate.should_run_ocr(&f1));
        // +22 luma relative to f0 exceeds 16 -> triggers immediately.
        assert!(gate.should_run_ocr(&f2));
    }

    #[test]
    fn dimension_change_and_reset_force_ocr() {
        let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
        let small = solid_frame(200, 40, 30, 30, 30);
        let resized = solid_frame(240, 40, 30, 30, 30);

        assert!(gate.should_run_ocr(&small));
        assert!(gate.should_run_ocr(&resized));
        assert!(!gate.should_run_ocr(&resized));

        gate.reset();
        assert!(gate.should_run_ocr(&resized));
    }
}
