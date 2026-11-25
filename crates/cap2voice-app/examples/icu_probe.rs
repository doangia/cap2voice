//! Probe `icu_segmenter` (UAX #29) on the strings the merge-fix tests
//! care about, and print the breakpoint indices + segments. One-off
//! diagnostic (2026-11-25) — not part of the app.
//!
//! Usage: `icu_probe.exe` (no args; the cases are baked in).

use icu_segmenter::options::SentenceBreakInvariantOptions;
use icu_segmenter::SentenceSegmenter;

fn main() {
    let segmenter = SentenceSegmenter::new(SentenceBreakInvariantOptions::default());
    let cases = [
        "等一下…… X",
        "等一下……",
        "你好。 X",
        "Hello. X",
        "等一下... X",
        "他说\"你好。\" X",
        "3.5元 X",
        "你好。我很好。你呢？ X",
        "等一下…… 再见",
        "等一下……你好",
    ];
    for text in cases {
        let breaks: Vec<usize> = segmenter.segment_str(text).collect();
        let segments: Vec<&str> = breaks
            .windows(2)
            .map(|pair| &text[pair[0]..pair[1]])
            .collect();
        println!("{text:?} -> breaks={breaks:?} segments={segments:?}");
    }
}
