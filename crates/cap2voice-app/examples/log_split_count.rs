//! Count how many lines of a real subtitle log the merge fix (speaker
//! split + ICU4X sentence segmentation) would now split into several
//! utterances. Diagnostic tool for the 2026-11-25 merge fix — not part of
//! the app.
//!
//! Usage: `cargo run --release --example log_split_count -- <subtitles-YYYY-MM-DD.log>`
//! Reads `[YYYY-MM-DD HH:MM:SS] <chinese> : <vietnamese>` lines, applies
//! `split_line_into_utterances` to each Chinese text, and prints totals
//! plus every line that splits.

use cap2voice_app::pipeline::{split_line_into_utterances, split_speakers};
use std::env;
use std::fs;
use std::process::ExitCode;

fn chinese_part(line: &str) -> Option<&str> {
    // `[2026-11-25 00:12:34] <chinese> : <vietnamese>` — the separator
    // is the first ` : ` after the timestamp bracket.
    let after_bracket = line.split_once("] ")?.1;
    let (chinese, _) = after_bracket.split_once(" : ")?;
    Some(chinese.trim())
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: log_split_count <subtitles-YYYY-MM-DD.log>");
        return ExitCode::from(2);
    };
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("cannot read {path}: {err}");
            return ExitCode::from(1);
        }
    };
    let mut total = 0usize;
    let mut long_lines = 0usize;
    let mut split_lines = 0usize;
    let mut split_long_lines = 0usize;
    let mut utterances = 0usize;
    let mut speaker_splits = 0usize;
    for line in content.lines() {
        let Some(chinese) = chinese_part(line) else {
            continue;
        };
        if chinese.is_empty() {
            continue;
        }
        total += 1;
        let is_long = chinese.chars().count() > 25;
        if is_long {
            long_lines += 1;
        }
        if split_speakers(chinese).len() > 1 {
            speaker_splits += 1;
        }
        let pieces = split_line_into_utterances(chinese);
        utterances += pieces.len();
        if pieces.len() > 1 {
            split_lines += 1;
            if is_long {
                split_long_lines += 1;
            }
            println!("SPLIT ({} pieces): {chinese}", pieces.len());
            for piece in &pieces {
                println!("    -> {piece}");
            }
        }
    }
    println!("total lines: {total}");
    println!("long lines (>25 chars): {long_lines}");
    println!("lines that now split: {split_lines} (of which long: {split_long_lines})");
    println!("total utterances after split: {utterances}");
    println!("lines split by the speaker layer alone: {speaker_splits}");
    ExitCode::SUCCESS
}
