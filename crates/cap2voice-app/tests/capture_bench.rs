//! Capture contract tests and real-PC `xcap` 100-frame latency benchmark.

use cap2voice_app::capture::{
    CaptureError, FrameCapturer, PixelDiffConfig, PixelDiffGate, XcapCapturer,
};
use cap2voice_app::config::CaptureRegion;
use std::path::PathBuf;
use std::time::Instant;

#[test]
fn xcap_capturer_rejects_too_small_region_before_touching_monitor() {
    let capturer = XcapCapturer::new();
    let region = CaptureRegion {
        monitor_id: "ANY".to_string(),
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    };

    assert_eq!(
        capturer.capture_region(&region),
        Err(CaptureError::InvalidRegion {
            width: 10,
            height: 10,
        })
    );
}

#[test]
fn xcap_capturer_rejects_disconnected_monitor_id() {
    let capturer = XcapCapturer::new();
    let region = CaptureRegion {
        monitor_id: r"\\.\DISPLAY_DOES_NOT_EXIST_999".to_string(),
        x: 0,
        y: 0,
        width: 200,
        height: 40,
    };

    assert_eq!(
        capturer.capture_region(&region),
        Err(CaptureError::MonitorUnavailable {
            monitor_id: r"\\.\DISPLAY_DOES_NOT_EXIST_999".to_string(),
        })
    );
}

#[test]
#[ignore = "benchmarks real xcap 100-frame capture latency on Windows PC"]
fn real_pc_xcap_100_frame_capture_latency_benchmark() {
    let capturer = XcapCapturer::new();
    let Ok(monitors) = capturer.list_monitors() else {
        panic!("failed enumerating Windows monitors via xcap");
    };
    assert!(
        !monitors.is_empty(),
        "expected at least 1 connected monitor"
    );

    for (idx, m) in monitors.iter().enumerate() {
        println!(
            "monitor[{idx}] id={} name='{}' origin=({},{}) size={}x{} scale={} primary={}",
            m.id, m.name, m.x, m.y, m.width, m.height, m.scale_factor, m.is_primary
        );
    }

    let monitor = monitors
        .iter()
        .find(|m| m.is_primary)
        .unwrap_or(&monitors[0]);

    let crop_w = 1280u32.min(monitor.width.saturating_sub(40)).max(200);
    let crop_h = 60u32.min(monitor.height.saturating_sub(40)).max(32);
    let offset_x = i32::try_from((monitor.width - crop_w) / 2).unwrap_or(0);
    let offset_y = i32::try_from(monitor.height.saturating_sub(crop_h + 60)).unwrap_or(0);

    let region = CaptureRegion {
        monitor_id: monitor.id.clone(),
        x: monitor.x + offset_x,
        y: monitor.y + offset_y,
        width: crop_w,
        height: crop_h,
    };
    println!("benchmark crop region: {region:?}");

    for _ in 0..3 {
        if let Err(err) = capturer.capture_region(&region) {
            panic!("warm-up capture_region failed: {err}");
        }
    }

    let mut gate = PixelDiffGate::new(PixelDiffConfig::default());
    let mut capture_ms = Vec::with_capacity(100);
    let mut diff_us = Vec::with_capacity(100);
    let mut last_frame = None;

    for i in 0..100 {
        let t0 = Instant::now();
        let frame = match capturer.capture_region(&region) {
            Ok(frame) => frame,
            Err(err) => panic!("benchmark capture_region failed at frame {i}: {err}"),
        };
        capture_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        let t1 = Instant::now();
        let _ = gate.should_run_ocr(&frame);
        diff_us.push(t1.elapsed().as_secs_f64() * 1_000_000.0);
        last_frame = Some(frame);
    }

    let Some(sample) = last_frame else {
        panic!("expected at least one captured frame");
    };
    let verify_path: PathBuf = std::env::temp_dir()
        .join("cap2voice")
        .join("task5_crop_verify.png");
    assert_eq!(sample.save_png(&verify_path), Ok(()));
    println!("saved crop verification PNG to: {}", verify_path.display());

    capture_ms.sort_by(f64::total_cmp);
    diff_us.sort_by(f64::total_cmp);

    let min_ms = capture_ms[0];
    let p50_ms = capture_ms[49];
    let p95_ms = capture_ms[94];
    let max_ms = capture_ms[99];
    let mean_ms = capture_ms.iter().sum::<f64>() / capture_ms.len() as f64;
    let diff_p50_us = diff_us[49];

    println!(
        "xcap 100-frame latency ({crop_w}x{crop_h}): min={min_ms:.2} ms, p50={p50_ms:.2} ms, p95={p95_ms:.2} ms, max={max_ms:.2} ms, mean={mean_ms:.2} ms | pixel_diff p50={diff_p50_us:.1} us"
    );

    assert!(
        p50_ms <= 30.0,
        "xcap median capture latency {p50_ms:.2} ms exceeds 30.0 ms CONSTRAINTS.md budget"
    );
    assert!(
        p95_ms <= 50.0,
        "xcap p95 capture latency {p95_ms:.2} ms exceeds 50.0 ms CONSTRAINTS.md budget"
    );
}
