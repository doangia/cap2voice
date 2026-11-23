//! Live Edge-TTS integration test (ignored by default): synthesise real
//! audio from the Edge service with both voices in use and check the MP3
//! output. Requires network. Run explicitly on the PC:
//! `cargo test --test edge_tts_live -- --ignored --nocapture`
//! The MP3 files are written next to the test for the by-ear check.

use cap2voice_app::api::TtsEngine;
use cap2voice_app::tts::EdgeTtsEngine;
use std::path::PathBuf;

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("tts-live")
}

#[tokio::test]
#[ignore]
async fn edge_tts_synthesises_vietnamese_and_chinese() {
    let engine = EdgeTtsEngine::new();
    let dir = out_dir();
    std::fs::create_dir_all(&dir).expect("create tts-live dir");

    let vi = engine
        .synthesize("Xin chào, tôi là Tiểu Minh.", "vi-VN-NamMinhNeural", "+0%")
        .await
        .expect("Vietnamese synthesis")
        .audio;
    assert!(!vi.is_empty(), "Vietnamese audio is empty");
    assert!(
        vi.len() > 1000,
        "Vietnamese audio suspiciously short: {}",
        vi.len()
    );
    assert_eq!(vi[0], 0xff, "MP3 must start with the frame sync 0xFF");
    assert!(
        vi[1] == 0xfb || vi[1] == 0xf3 || vi[1] == 0xf2,
        "MP3 second sync byte unexpected: {:#x}",
        vi[1]
    );
    std::fs::write(dir.join("vi-xin-chao.mp3"), &vi).expect("write vi mp3");

    let zh = engine
        .synthesize("你好", "zh-CN-XiaoxiaoNeural", "+0%")
        .await
        .expect("Chinese synthesis")
        .audio;
    assert!(!zh.is_empty(), "Chinese audio is empty");
    assert!(
        zh.len() > 500,
        "Chinese audio suspiciously short: {}",
        zh.len()
    );
    assert_eq!(zh[0], 0xff, "MP3 must start with the frame sync 0xFF");
    std::fs::write(dir.join("zh-ni-hao.mp3"), &zh).expect("write zh mp3");
}
