//! Integration tests for the public `wcocr_rs::WeChatOcr` API.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wcocr_rs::{OcrError, WeChatOcr};

#[test]
fn wechat_ocr_new_rejects_missing_wxocr_dll() {
    let missing_dll = Path::new(r"Z:\nonexistent_cap2voice\wxocr.dll");
    let existing_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    let err = WeChatOcr::new(missing_dll, existing_dir)
        .err()
        .unwrap_or_else(|| panic!("expected InvalidPath for missing wxocr.dll"));

    assert_eq!(
        err,
        OcrError::InvalidPath {
            path: missing_dll.to_path_buf(),
            expected: "wxocr.dll file",
        }
    );
}

#[test]
fn wechat_ocr_new_rejects_missing_weixin_dir() {
    let existing_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let missing_dir = Path::new(r"Z:\nonexistent_cap2voice\Weixin\4.0.2.28");

    let err = WeChatOcr::new(&existing_file, missing_dir)
        .err()
        .unwrap_or_else(|| panic!("expected InvalidPath for missing Weixin directory"));

    assert_eq!(
        err,
        OcrError::InvalidPath {
            path: missing_dir.to_path_buf(),
            expected: "Weixin directory",
        }
    );
}

#[test]
#[ignore = "requires local WeChat 4.x OCR installation on Windows PC"]
fn wechat_ocr_real_pc_recognizes_chinese_subtitle_fixture() {
    let wxocr = std::env::var_os("WXOCR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let appdata = std::env::var_os("APPDATA").unwrap_or_default();
            PathBuf::from(appdata)
                .join(r"Tencent\xwechat\XPlugin\plugins\WeChatOcr\8022\extracted\wxocr.dll")
        });
    let wxdir = std::env::var_os("WXDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Tencent\Weixin\4.0.2.28"));
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("test-zh-custom.png");

    let init_started = Instant::now();
    let Ok(ocr) = WeChatOcr::new(&wxocr, &wxdir) else {
        panic!("WeChatOcr::new failed");
    };
    let Ok(()) = ocr.wait_connection(Duration::from_secs(5)) else {
        panic!("wait_connection failed");
    };
    let handshake_ms = init_started.elapsed().as_millis();

    let ocr_started = Instant::now();
    let res = match ocr.ocr_file(&fixture, Duration::from_secs(3)) {
        Ok(res) => res,
        Err(e) => panic!("ocr_file failed on {}: {e}", fixture.display()),
    };
    let ocr_ms = ocr_started.elapsed().as_millis();

    println!(
        "real-PC OCR: handshake_ms={handshake_ms}, ocr_ms={ocr_ms}, errcode={}, size={}x{}, blocks={}",
        res.errcode,
        res.width,
        res.height,
        res.ocr_response.len()
    );
    for (idx, block) in res.ocr_response.iter().enumerate() {
        println!(
            "  block[{idx}] rate={:.3} box=({:.1},{:.1},{:.1},{:.1}) text={}",
            block.rate, block.left, block.top, block.right, block.bottom, block.text
        );
    }

    assert_eq!(res.errcode, 0);
    assert!(
        !res.ocr_response.is_empty(),
        "expected non-empty OCR response blocks"
    );

    let joined: String = res
        .ocr_response
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("夜深了"),
        "expected '夜深了' in recognized text:\n{joined}"
    );
    assert!(
        joined.contains("明日再议"),
        "expected '明日再议' in recognized text:\n{joined}"
    );
    assert!(
        joined.contains("离开这里"),
        "expected '离开这里' in recognized text:\n{joined}"
    );

    let missing_img = Path::new(r"Z:\nonexistent_cap2voice\missing.png");
    let bad_img_err = ocr
        .ocr_file(missing_img, Duration::from_secs(3))
        .err()
        .unwrap_or_else(|| panic!("expected InvalidImagePath for nonexistent image"));
    assert!(
        matches!(bad_img_err, OcrError::InvalidImagePath { .. }),
        "expected InvalidImagePath, got {bad_img_err:?}"
    );
}
