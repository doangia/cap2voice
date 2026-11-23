//! Vietphrase port parity: the Rust port must produce the SAME outputs as
//! the Python translator on the 25-line A5 benchmark set.
//!
//! Requires the real dictionaries: set `CAP2VOICE_VP_DATA_DIR` to the
//! directory holding the five dictionary files (or use the default
//! `%APPDATA%\cap2voice\vietphrase\data`).
//! Ignored by default — run explicitly on the PC:
//! `cargo test --test vietphrase_benchmark -- --ignored --nocapture`

use cap2voice_app::translate::vietphrase::{load_dictionary_set, VietPhraseTranslator};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct Row {
    zh: String,
    vi: String,
}

fn expected_rows() -> Vec<Row> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("vietphrase_expected.json");
    let text = std::fs::read_to_string(&path).expect("read vietphrase_expected.json");
    serde_json::from_str(&text).expect("parse vietphrase_expected.json")
}

#[test]
#[ignore]
fn vietphrase_matches_python_on_benchmark_lines() {
    let dir = match std::env::var("CAP2VOICE_VP_DATA_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => cap2voice_app::translate::vietphrase::default_dict_dir().expect("default dict dir"),
    };
    let dicts = load_dictionary_set(&dir).expect("load dictionaries");
    let translator = VietPhraseTranslator::new(dicts, None);
    let rows = expected_rows();
    assert_eq!(rows.len(), 25);
    let mut failures: Vec<String> = Vec::new();
    for row in &rows {
        let got = translator.translate(&row.zh);
        if got != row.vi {
            failures.push(format!(
                "{:?}: expected {:?}, got {:?}",
                row.zh, row.vi, got
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} / {} lines differ from Python:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
}
