//! Helsinki translator parity: the Rust ONNX translator must produce the
//! SAME outputs as the Python no-past greedy run on the 25-line A5
//! benchmark set (the greedy outputs differ from beam search on 7 lines;
//! the fixture is the greedy run, which is what this implementation does).
//!
//! Requires the real model: set `CAP2VOICE_HELSINKI_MODEL_DIR` to the
//! directory holding `encoder_model.onnx`, `decoder_model.onnx` and
//! `tokenizer.json` (or use the default
//! `%APPDATA%\cap2voice\models\opus-mt-zh-vi`).
//! Ignored by default — run explicitly on the PC:
//! `cargo test --test helsinki_benchmark -- --ignored --nocapture`
//! (model load takes seconds; the 25 lines take ~2 s total).

use cap2voice_app::translate::helsinki::{default_model_dir, HelsinkiTranslator};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Deserialize)]
struct Row {
    zh: String,
    vi: String,
}

fn expected_rows() -> Vec<Row> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("helsinki_expected_greedy.json");
    let text = std::fs::read_to_string(&path).expect("read helsinki_expected_greedy.json");
    serde_json::from_str(&text).expect("parse helsinki_expected_greedy.json")
}

#[test]
#[ignore]
fn helsinki_matches_python_greedy_on_benchmark_lines() {
    let dir = match std::env::var("CAP2VOICE_HELSINKI_MODEL_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => default_model_dir().expect("default model dir"),
    };
    let load_start = Instant::now();
    let mut translator = HelsinkiTranslator::load(&dir).expect("load Helsinki model");
    println!("model load: {:?}", load_start.elapsed());
    let rows = expected_rows();
    assert_eq!(rows.len(), 25);
    let mut failures: Vec<String> = Vec::new();
    let mut total_ms = 0f64;
    for row in &rows {
        let t = Instant::now();
        let got = translator
            .translate_blocking(&row.zh)
            .expect("translate line");
        total_ms += t.elapsed().as_secs_f64() * 1000.0;
        if got != row.vi {
            failures.push(format!(
                "{:?}: expected {:?}, got {:?}",
                row.zh, row.vi, got
            ));
        }
    }
    println!(
        "25 lines in {:.0} ms total (median budget 275 ms/line)",
        total_ms
    );
    assert!(
        failures.is_empty(),
        "{} / {} lines differ from Python greedy:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
}
