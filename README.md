# cap2voice

Windows desktop app that watches videos with hardcoded Chinese subtitles,
OCRs the subtitle region in real time, translates it to Vietnamese, shows
both lines on a floating overlay, and reads the Vietnamese line aloud (TTS).

> Status: early development — spec phase. See `brief.md` (Vietnamese) for
> the full vision and decisions.

## How it works

1. Drag to select the subtitle region on screen.
2. Capture that region (~10 fps), skip unchanged frames.
3. OCR with `wcocr-rs` (WeChat OCR engine, local, no GPU needed).
4. Translate zh → vi, show both lines, TTS the Vietnamese line.
5. Log every `Chinese : Vietnamese` pair.

Works with any video source (local file, player, browser) because it only
observes the screen. It is not a video player.

## Architecture

- Rust desktop app (`eframe`/egui), screen capture (`xcap`), OCR (`wcocr-rs`)
- Native in-process translation (selectable engine: Helsinki-NLP `opus-mt-zh-vi`
  ONNX or offline Vietphrase) and TTS (native Edge-TTS WebSocket client,
  voice `vi-VN-NamMinhNeural`); no Python, no local server
- Settings window + system tray (close to tray, keep translating in background)

See `brief.md` and `docs/spec.md` for details, and `AGENTS.md` for the agent
workflow (spec-first: spec → plan → build → test → review → ship).

## Notes

- The OCR engine is WeChat's (mmmojo), used through the Weixin install on
  the user's PC. It is personal use only and is NOT distributed with this
  repo — the app loads it from the local WeChat installation at runtime.
- Translation runs fully offline (local ONNX model / dictionaries). Only TTS needs network (Edge-TTS).
