//! Probe the real WeChat OCR engine on a synthetic image and dump the raw
//! result (blocks + per-character boxes) as JSON. Used once to calibrate
//! the geometric speaker-split threshold in `ocr/filter.rs` (merge fix,
//! 2026-11-25) — not part of the app.
//!
//! Usage (on the PC, WeChat OCR spawns its own `Weixin.exe` host):
//! `ocr_gap_probe.exe <image.png>` — prints the `OcrResult` JSON, which
//! includes every block's box/text and every `CharDetail` box.

use cap2voice_app::ocr::resolve_wechat_paths;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use wcocr_rs::WeChatOcr;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(image) = args.next() else {
        eprintln!("usage: ocr_gap_probe <image.png>");
        return ExitCode::from(2);
    };
    let image = PathBuf::from(image);
    if !image.is_file() {
        eprintln!("no such file: {}", image.display());
        return ExitCode::from(2);
    }
    let paths = match resolve_wechat_paths(None, None) {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("cannot resolve WeChat OCR paths: {err}");
            return ExitCode::from(1);
        }
    };
    let ocr = match WeChatOcr::new(&paths.wxocr_dll, &paths.weixin_dir) {
        Ok(ocr) => ocr,
        Err(err) => {
            eprintln!("cannot start WeChat OCR: {err}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = ocr.wait_connection(Duration::from_secs(30)) {
        eprintln!("WeChat OCR handshake failed: {err}");
        return ExitCode::from(1);
    }
    match ocr.ocr_file(&image, Duration::from_secs(30)) {
        Ok(result) => match serde_json::to_string_pretty(&result) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("cannot serialize OCR result: {err}");
                ExitCode::from(1)
            }
        },
        Err(err) => {
            eprintln!("OCR failed: {err}");
            ExitCode::from(1)
        }
    }
}
