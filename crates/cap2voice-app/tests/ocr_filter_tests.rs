//! Integration tests for WeChat path auto-discovery, OCR adapter error mapping,
//! CJK text filtering, and fuzzy deduplication.

use cap2voice_app::capture::CapturedFrame;
use cap2voice_app::config::GeneralConfig;
use cap2voice_app::ocr::{
    discover_weixin_dir_in, discover_wxocr_in, extract_chinese_subtitle, resolve_wechat_paths,
    resolve_wechat_paths_from_config, resolve_wechat_paths_with_roots, FuzzyDeduplicator,
    OcrAdapterError, OcrEngine, OcrError, TextBlock, TextDeduplicator, WeChatOcrAdapter,
    WeChatOcrPaths,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
            "cap2voice_ocr_{label}_{}_{}_{}",
            std::process::id(),
            nanos,
            seq
        ));
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("failed to create temp dir {}", dir.display());
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

fn touch_file(path: &Path) {
    if let Some(parent) = path.parent() {
        let Ok(()) = std::fs::create_dir_all(parent) else {
            panic!("failed creating parent dir {}", parent.display());
        };
    }
    let Ok(()) = std::fs::write(path, b"stub") else {
        panic!("failed writing {}", path.display());
    };
}

fn block(left: f32, top: f32, right: f32, bottom: f32, text: &str) -> TextBlock {
    TextBlock {
        left,
        top,
        right,
        bottom,
        rate: 0.99,
        text: text.to_string(),
        details: Vec::new(),
    }
}

#[test]
fn discover_wechat_paths_picks_highest_numeric_versions_and_skips_incomplete_dirs() {
    let temp = TempDirGuard::new("discover");
    let plugins_root = temp.path().join("WeChatOcr");
    let weixin_root = temp.path().join("Weixin");

    touch_file(&plugins_root.join(r"8022\extracted\wxocr.dll"));
    touch_file(&plugins_root.join(r"10001\extracted\wxocr.dll"));
    let Ok(()) = std::fs::create_dir_all(plugins_root.join(r"10002\extracted")) else {
        panic!("failed creating incomplete plugin dir");
    };

    touch_file(&weixin_root.join(r"4.0.2.28\mmmojo_64.dll"));
    touch_file(&weixin_root.join(r"4.0.10.1\mmmojo_64.dll"));
    let Ok(()) = std::fs::create_dir_all(weixin_root.join("4.0.99.0")) else {
        panic!("failed creating incomplete weixin dir");
    };
    touch_file(&weixin_root.join("Weixin.exe"));

    let expected_wxocr = plugins_root.join(r"10001\extracted\wxocr.dll");
    let expected_wxdir = weixin_root.join("4.0.10.1");

    assert_eq!(discover_wxocr_in(&plugins_root), Ok(expected_wxocr.clone()));
    assert_eq!(
        discover_weixin_dir_in(&weixin_root),
        Ok(expected_wxdir.clone())
    );
    assert_eq!(
        resolve_wechat_paths_with_roots(None, None, Some(&plugins_root), Some(&weixin_root)),
        Ok(WeChatOcrPaths {
            wxocr_dll: expected_wxocr,
            weixin_dir: expected_wxdir,
        })
    );
}

#[test]
fn resolve_wechat_paths_respects_manual_overrides_and_reports_vietnamese_errors_when_missing() {
    let temp = TempDirGuard::new("overrides");
    let custom_dll = temp.path().join(r"custom\wxocr.dll");
    let custom_dir = temp.path().join("custom_weixin");
    touch_file(&custom_dll);
    touch_file(&custom_dir.join("mmmojo_64.dll"));

    let missing_root = temp.path().join("does_not_exist");
    assert_eq!(
        resolve_wechat_paths_with_roots(
            Some(&custom_dll),
            Some(&custom_dir),
            Some(&missing_root),
            Some(&missing_root),
        ),
        Ok(WeChatOcrPaths {
            wxocr_dll: custom_dll.clone(),
            weixin_dir: custom_dir.clone(),
        })
    );

    let missing_dll = temp.path().join("missing_wxocr.dll");
    let dll_err = resolve_wechat_paths_with_roots(
        Some(&missing_dll),
        Some(&custom_dir),
        Some(&missing_root),
        Some(&missing_root),
    );
    assert_eq!(
        dll_err,
        Err(OcrAdapterError::InvalidConfiguredPath {
            kind: "wxocr.dll",
            path: missing_dll.clone(),
        })
    );
    if let Err(err) = dll_err {
        assert!(err.to_string().contains("không hợp lệ"));
    }

    let incomplete_dir = temp.path().join("empty_weixin");
    let Ok(()) = std::fs::create_dir_all(&incomplete_dir) else {
        panic!("failed creating incomplete dir");
    };
    let dir_err = resolve_wechat_paths_with_roots(
        Some(&custom_dll),
        Some(&incomplete_dir),
        Some(&missing_root),
        Some(&missing_root),
    );
    assert_eq!(
        dir_err,
        Err(OcrAdapterError::InvalidConfiguredPath {
            kind: "thư mục Weixin 4.x (thiếu mmmojo_64.dll)",
            path: incomplete_dir,
        })
    );

    let cfg = GeneralConfig {
        wxocr_dll_path: Some(missing_dll.clone()),
        weixin_dir_path: Some(custom_dir),
        ..GeneralConfig::default()
    };
    assert_eq!(
        resolve_wechat_paths_from_config(&cfg),
        Err(OcrAdapterError::InvalidConfiguredPath {
            kind: "wxocr.dll",
            path: missing_dll,
        })
    );

    let missing_wxocr =
        resolve_wechat_paths_with_roots(None, None, Some(&missing_root), Some(&missing_root));
    assert_eq!(
        missing_wxocr,
        Err(OcrAdapterError::WxocrNotFound {
            searched: missing_root.display().to_string(),
        })
    );
    if let Err(err) = missing_wxocr {
        assert!(err.to_string().contains("Không tìm thấy wxocr.dll"));
    }

    let valid_plugins = temp.path().join("plugins_only");
    touch_file(&valid_plugins.join(r"8022\extracted\wxocr.dll"));
    let missing_wxdir =
        resolve_wechat_paths_with_roots(None, None, Some(&valid_plugins), Some(&missing_root));
    assert_eq!(
        missing_wxdir,
        Err(OcrAdapterError::WeixinDirNotFound {
            searched: missing_root.display().to_string(),
        })
    );
    if let Err(err) = missing_wxdir {
        assert!(err.to_string().contains("Không tìm thấy thư mục Weixin"));
    }
}

#[test]
fn ocr_filter_and_fuzzy_dedupe_pipeline_sequence() {
    let mut deduper = FuzzyDeduplicator::default();

    // Frame 1: unsorted blocks + watermark noise -> emit clean Chinese subtitle.
    let frame1_blocks = vec![
        block(20.0, 10.0, 120.0, 30.0, "YouTube HD"),
        block(260.0, 82.0, 420.0, 106.0, "甲乙丙丁戊"),
        block(100.0, 80.0, 250.0, 104.0, "一二三四五六七八九十"),
    ];
    let sub1 = extract_chinese_subtitle(&frame1_blocks);
    assert_eq!(sub1.as_deref(), Some("一二三四五六七八九十 甲乙丙丁戊"));
    assert_eq!(
        deduper.process(sub1.as_deref()),
        Some("一二三四五六七八九十 甲乙丙丁戊".to_string())
    );

    // Frame 2: 1-character OCR glitch (14/15 = 0.933 >= 0.93) -> suppressed.
    let frame2_blocks = vec![block(
        100.0,
        80.0,
        420.0,
        104.0,
        "一二三四五六七八九十 甲乙丙丁己",
    )];
    let sub2 = extract_chinese_subtitle(&frame2_blocks);
    assert_eq!(deduper.process(sub2.as_deref()), None);

    // Frame 3: genuinely new subtitle -> emitted.
    let frame3_blocks = vec![block(100.0, 80.0, 400.0, 104.0, "夜深了，雨还在下。")];
    let sub3 = extract_chinese_subtitle(&frame3_blocks);
    assert_eq!(
        deduper.process(sub3.as_deref()),
        Some("夜深了，雨还在下。".to_string())
    );

    // 5 consecutive frames containing only Latin watermark / digits (zero CJK).
    let logo_only = vec![block(20.0, 10.0, 150.0, 30.0, "CCTV-6 1080P")];
    for _ in 0..5 {
        let filtered = extract_chinese_subtitle(&logo_only);
        assert_eq!(filtered, None);
        assert_eq!(deduper.process(filtered.as_deref()), None);
    }

    // Same line returning after >= 5 blank frames must emit again.
    let sub8 = extract_chinese_subtitle(&frame3_blocks);
    assert_eq!(
        deduper.process(sub8.as_deref()),
        Some("夜深了，雨还在下。".to_string())
    );
}

#[test]
fn wechat_ocr_adapter_returns_invalid_path_error_without_panicking() {
    let missing_dll = PathBuf::from(r"Z:\nonexistent_cap2voice\wxocr.dll");
    let missing_dir = PathBuf::from(r"Z:\nonexistent_cap2voice\Weixin\4.0.2.28");
    let adapter = WeChatOcrAdapter::new(WeChatOcrPaths {
        wxocr_dll: missing_dll.clone(),
        weixin_dir: missing_dir,
    });
    let Ok(frame) = CapturedFrame::new(24, 24, vec![255u8; 24 * 24 * 4]) else {
        panic!("expected valid CapturedFrame");
    };

    let err = adapter.recognize_frame(&frame);
    assert_eq!(
        err,
        Err(OcrAdapterError::Engine(OcrError::InvalidPath {
            path: missing_dll,
            expected: "wxocr.dll file",
        }))
    );
}

#[test]
#[ignore = "requires local WeChat 4.x OCR installation on Windows PC"]
fn real_pc_auto_discovery_and_ocr_filter_on_chinese_fixture() {
    let paths = match resolve_wechat_paths(None, None) {
        Ok(paths) => paths,
        Err(err) => panic!("auto-discovery failed on real PC: {err}"),
    };
    println!(
        "auto-discovered wxocr_dll={} weixin_dir={}",
        paths.wxocr_dll.display(),
        paths.weixin_dir.display()
    );
    assert!(
        paths.wxocr_dll.is_file(),
        "expected wxocr.dll file at {}",
        paths.wxocr_dll.display()
    );
    assert!(
        paths.weixin_dir.join("mmmojo_64.dll").is_file(),
        "expected mmmojo_64.dll inside {}",
        paths.weixin_dir.display()
    );

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("wcocr-rs")
        .join("tests")
        .join("fixtures")
        .join("test-zh-custom.png");
    let img = match image::open(&fixture) {
        Ok(img) => img.to_rgba8(),
        Err(err) => panic!("failed opening fixture {}: {err}", fixture.display()),
    };
    let (w, h) = img.dimensions();
    let frame = match CapturedFrame::new(w, h, img.into_raw()) {
        Ok(frame) => frame,
        Err(err) => panic!("invalid fixture frame buffer: {err}"),
    };

    let adapter = WeChatOcrAdapter::new(paths);
    let started = Instant::now();
    let res = match adapter.recognize_frame(&frame) {
        Ok(res) => res,
        Err(err) => panic!("WeChatOcrAdapter::recognize_frame failed: {err}"),
    };
    let elapsed_ms = started.elapsed().as_millis();

    println!(
        "adapter OCR elapsed_ms={elapsed_ms}, temp_frame={}, errcode={}, blocks={}",
        adapter.temp_frame_path().display(),
        res.errcode,
        res.ocr_response.len()
    );
    for (idx, b) in res.ocr_response.iter().enumerate() {
        println!(
            "  block[{idx}] box=({:.1},{:.1},{:.1},{:.1}) text={}",
            b.left, b.top, b.right, b.bottom, b.text
        );
    }

    assert_eq!(res.errcode, 0);
    assert!(!res.ocr_response.is_empty());

    let filtered = match extract_chinese_subtitle(&res.ocr_response) {
        Some(text) => text,
        None => panic!("expected non-empty filtered Chinese text"),
    };
    println!("filtered Chinese subtitle: {filtered}");

    assert!(filtered.contains("夜深了"), "got: {filtered}");
    assert!(filtered.contains("明日再议"), "got: {filtered}");
    assert!(filtered.contains("离开这里"), "got: {filtered}");
    assert!(
        !filtered.chars().any(|c| c.is_ascii_alphabetic()),
        "expected no ASCII Latin letters in filtered output: {filtered}"
    );

    let mut deduper = FuzzyDeduplicator::default();
    assert_eq!(deduper.process(Some(&filtered)), Some(filtered.clone()));
    assert_eq!(deduper.process(Some(&filtered)), None);
}
