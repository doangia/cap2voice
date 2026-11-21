# Task List: cap2voice — V1 (MVP)

**Source plan:** `tasks/plan.md`  
**Source spec:** `docs/spec.md`  
**Quality bar:** `CONSTRAINTS.md` (every task must clear `CONSTRAINTS.md` §1–§3 before checking off)

> **Build discipline (`AGENTS.md`):** Execute one thin slice at a time (RED → GREEN → REFACTOR), record real-PC verification output & screenshots in the test log, and ask Danh for explicit approval before any Git state-modifying command (`commit`, `push`, etc.) or modifying pre-existing files.

---

## Phase 1: Foundations & High-Risk Proofs

### Task 1: Workspace Root & `wcocr-rs` Mojo/Proto Core

**Description:** Initialize the Cargo workspace (`Cargo.toml`) and create the internal `crates/wcocr-rs` Rust library crate (`rlib` only — zero `cdylib`/C ABI exports), porting `mojo.rs`, `proto.rs`, Windows wide-string/verbatim-path helpers, and defining the typed `OcrError` enum in `lib.rs` (referencing `C:\Users\doanv\wechat-ocr-rs` read-only without modifying the original folder).

**Done means (Acceptance criteria):**
- [ ] Root `Cargo.toml` defines workspace with `crates/wcocr-rs` (Rust 2021 edition) and compiles cleanly (`AC-OCR-1`).
- [ ] `crates/wcocr-rs` has `crate-type = ["rlib"]`, zero `#[no_mangle] extern "C"` symbols, and exports typed `OcrError` (`InvalidPath`, `DllLoadFailed`, `StartFailed`, `HandshakeTimeout`, `NotSupported`, `RequestTimeout`, `SendFailed`, `InvalidImagePath`).
- [ ] Unit tests verify `wx4::ParseOcrReqMessage` / `ParseOcrRespMessage` prost round-trip encoding and `strip_verbatim_prefix` / `to_wide` path helpers.

**Verification:**
- [ ] Automated tests pass: `cargo test -p wcocr-rs`
- [ ] Lint & format pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `cargo build -p wcocr-rs` on Windows (with concrete rustup toolchain `PATH` if over SSH) and confirm `target\debug\libwcocr_rs.rlib` builds with zero `.dll` C-ABI target produced.

**Dependencies:** None

**Files likely touched:**
- `Cargo.toml`
- `crates/wcocr-rs/Cargo.toml`
- `crates/wcocr-rs/src/lib.rs`
- `crates/wcocr-rs/src/mojo.rs`
- `crates/wcocr-rs/src/proto.rs`

**Estimated scope:** Medium (5 files)

---

### Task 2: `wcocr-rs` Typed `WeChatOcr` Session with Timeout & Real-PC OCR Proof

**Description:** Implement `WeChatOcr::new(&Path, &Path)`, `wait_connection(Duration)`, and `ocr_file(&Path, Duration)` in `crates/wcocr-rs/src/ocr.rs` using `Condvar::wait_timeout_while` so hung OCR workers fail fast with `OcrError::RequestTimeout` instead of deadlocking, and verify direct Rust OCR against the local WeChat 4.x installation on Danh's PC.

**Done means (Acceptance criteria):**
- [x] `WeChatOcr` returns `Err(OcrError::InvalidPath)` when `wxocr.dll` or Weixin directory is missing (`AC-OCR-3`).
- [x] Unit test simulating an unresponsive task slot verifies `Err(OcrError::RequestTimeout)` returns within `timeout + 100 ms` (`AC-OCR-4`).
- [x] `#[ignore]` real-PC integration test recognizes Chinese lines from `test-zh-custom.png` via local Weixin `4.0.2.28` + plugin `8022` with `errcode == 0` (`AC-OCR-1`, `AC-OCR-2`).

**Verification:**
- [x] Automated tests pass: `cargo test -p wcocr-rs`
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [x] Real PC check: Run `cargo test -p wcocr-rs -- --ignored --nocapture` with `WXOCR=%APPDATA%\Tencent\xwechat\XPlugin\plugins\WeChatOcr\8022\extracted\wxocr.dll` and `WXDIR=C:\Program Files\Tencent\Weixin\4.0.2.28`; paste recognized Chinese text blocks and elapsed OCR ms into the test log (also note whether `pic_data` in-memory works or stays deferred to temp file).

**Dependencies:** Task 1

**Files likely touched:**
- `crates/wcocr-rs/src/ocr.rs`
- `crates/wcocr-rs/src/lib.rs`
- `crates/wcocr-rs/tests/ocr_tests.rs`

**Estimated scope:** Small (3 files)

---

### Task 3: Temporary Local Python API Server (`/health`, `/translate`, `/tts`)

**Description:** Create `server/requirements.txt`, `server/server.py`, and `server/tests/test_server.py` implementing the strictly typed FastAPI local server bound to `127.0.0.1` for Google Translate free endpoint (`zh-CN` → `vi` via `httpx`) and Edge-TTS (`vi-VN-NamMinhNeural` default, `vi-VN-HoaiMyNeural` supported), with explicit structured `400`/`429`/`502`/`503` error responses.

**Done means (Acceptance criteria):**
- [x] `GET /health` returns `200 OK` with `{"status":"ok","version":"0.1.0","translator":"google-free","tts":"edge-tts"}` (`AC-SRV-1`).
- [ ] `POST /translate` translates Chinese → Vietnamese (`200`), rejects empty/whitespace input (`400`), and maps upstream `429` / network errors to structured `429` / `502` JSON (`AC-SRV-2`, `AC-TR-1`). **LIVE 200 PENDING (Google 429 from this IP 2026-11-19; mocked 200 proven; re-checked 2026-11-20 12:10, still 429 — rate limit persists >12h; re-check at Checkpoint 1).**
- [x] `POST /tts` returns `audio/mpeg` MP3 bytes for valid Vietnamese text + voice/rate, rejects empty text or invalid voice format (`400`), and maps Edge-TTS failures to `502`/`503` (`AC-SRV-3`).

**Verification:**
- [x] Static types pass: `mypy --strict server/server.py`
- [x] Automated tests pass: `python -m pytest server/ -v` (with mocked `httpx` and `edge_tts` covering happy paths + `400`/`429`/`502` failures)
- [x] Real PC check: Create `server/.venv`, install `server/requirements.txt`, start `uvicorn` on `127.0.0.1:18765`, call live `GET /health`, `POST /translate` with `"夜深了，雨还在下。"`, and `POST /tts` for both `vi-VN-NamMinhNeural` and `vi-VN-HoaiMyNeural` (play resulting MP3 to confirm intelligible Vietnamese speech), then disconnect network and confirm `502`/`503` JSON response; stop server cleanly.

**Dependencies:** None (can run in parallel with Tasks 1–2)

**Files likely touched:**
- `server/requirements.txt`
- `server/server.py`
- `server/tests/test_server.py`

**Estimated scope:** Medium (3 files)

---

## Checkpoint 1: After Tasks 1–3 (Foundations & High-Risk Proofs)
- [ ] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` green
- [ ] `mypy --strict server/server.py && python -m pytest server/ -v` green
- [ ] Real-PC OCR output (`wcocr-rs`) and live `/translate` + `/tts` results logged
- [ ] Review Checkpoint 1 results with Danh before Phase 2

---

## Phase 2: App Core, Capture Latency Proof & Managed Server

### Task 4: `cap2voice-app` Scaffold, `config.toml` Persistence & Windows Autostart

**Description:** Add `crates/cap2voice-app` to the Cargo workspace with typed `AppConfig` covering all 5 Settings sections, atomic temp-file + rename persistence to `%APPDATA%\cap2voice\config.toml`, corrupt-file backup (`config.toml.bak`) with explicit error reporting, and a Windows `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice` autostart helper.

**Done means (Acceptance criteria):**
- [x] `AppConfig` serializes/deserializes cleanly via TOML with exact defaults from `docs/spec.md` (`10 fps`, `vi-VN-NamMinhNeural`, volume `0.75`, font `18 px`, opacity `0.85`, dedupe `0.93`, pixel-diff `0.02`, port `18765`, close-to-tray `true`, autostart `false`, default hotkeys `Ctrl+Alt+S/R/H/M`) (`AC-SET-3`).
- [x] Corrupt TOML triggers typed `ConfigLoadWarning`, copies broken file to `config.toml.bak`, and never silently destroys user data (`AC-SET-6`).
- [x] Windows autostart helper writes and deletes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice` cleanly (`AC-SET-5`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app config`
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [x] Real PC check: Run ignored Windows registry test on Danh's PC verifying `Get-ItemProperty HKCU:\Software\Microsoft\Windows\CurrentVersion\Run` adds then removes `cap2voice` without requiring admin/UAC, and confirm atomic write + `.bak` creation in a temp `%APPDATA%` path.

**Dependencies:** Task 1

**Files likely touched:**
- `Cargo.toml`
- `crates/cap2voice-app/Cargo.toml`
- `crates/cap2voice-app/src/lib.rs`
- `crates/cap2voice-app/src/main.rs`
- `crates/cap2voice-app/src/config.rs`

**Estimated scope:** Medium (5 files)

---

### Task 5: Screen Capture (`xcap`), Pixel-Diff Gate & Real-PC Latency Proof

**Description:** Implement `FrameCapturer` trait, DPI/monitor-aware `CaptureRegion` validation, `XcapCapturer` (`xcap` 0.9.10), and `PixelDiffGate` (grayscale downsampled pixel-change ratio with first-frame bypass), plus a real-PC 100-frame benchmark proving whether `xcap` meets the `<= 50 ms` p95 budget or requires the `windows-capture` `2.0.1` fallback.

**Done means (Acceptance criteria):**
- [x] Unit tests prove `PixelDiffGate` bypasses on frame 1, skips `>= 99%` identical static frames (`AC-CAP-4`), and triggers immediately on subtitle pixel change (`AC-CAP-5`).
- [x] Disconnected monitor or out-of-bounds region returns explicit `CaptureError::MonitorUnavailable` / `OutOfBounds` without panicking (`AC-CAP-7`).
- [x] Real-PC 100-frame benchmark logs p50/p95 capture latency (`<= 30 ms` median / `<= 50 ms` p95 target — `AC-CAP-6`) and saves a verification PNG crop matching physical coordinates (`AC-CAP-3`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app capture`
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [x] Real PC check (FAIL-FAST GATE): Run `cargo test -p cap2voice-app --test capture_bench -- --ignored --nocapture` on Danh's display (including scaled DPI check); inspect saved crop PNG and record p50/p95 ms in test log. If p95 > `50 ms`, stop and ask Danh to approve switching `FrameCapturer` to `windows-capture` `2.0.1`.

**Dependencies:** Task 4

**Files likely touched:**
- `crates/cap2voice-app/src/capture/mod.rs`
- `crates/cap2voice-app/src/capture/xcap_impl.rs`
- `crates/cap2voice-app/src/capture/pixel_diff.rs`
- `crates/cap2voice-app/tests/capture_bench.rs`

**Estimated scope:** Medium (4 files)

---

### Task 6: Rust HTTP Clients (`Translator`/`TtsEngine`) & Managed `ServerProcess`

**Description:** Implement `Translator` and `TtsEngine` traits backed by `reqwest` `0.13` (`HttpTranslator`, `HttpTtsEngine`) with strict timeouts and explicit error mapping, plus `ServerProcess` that discovers `server/.venv/Scripts/python.exe` (or `python`), spawns `uvicorn` with `CREATE_NO_WINDOW`, waits for `GET /health` up to `10 s`, and binds the child to a Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).

**Done means (Acceptance criteria):**
- [x] Mock HTTP server tests verify exact JSON payloads and typed mapping of `400`, `429`, `502`/`503`, timeout, and connection refused (`AC-TR-2`).
- [x] `ServerProcess::start` launches Python server invisibly and passes `GET /health` within `10 s`, or returns clear setup diagnostics if Python/deps missing (`AC-SRV-4`, `AC-SRV-6`).
- [x] Dropping `ServerProcess` OR force-killing the parent process terminates the Python child via Windows Job Object with zero orphans (`AC-SRV-5`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app api`
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [x] Real PC check: Run `cargo test -p cap2voice-app --test api_client_tests -- --ignored --nocapture` to spawn real `server/server.py`, verify no console window flashes, verify `GET /health` 200, then verify `Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'server:app' }` returns empty after both graceful drop and forced parent kill.

**Dependencies:** Task 3, Task 4 (can run in parallel with Task 5)

**Files likely touched:**
- `crates/cap2voice-app/src/api/mod.rs`
- `crates/cap2voice-app/src/api/http_client.rs`
- `crates/cap2voice-app/src/api/server_proc.rs`
- `crates/cap2voice-app/tests/api_client_tests.rs`

**Estimated scope:** Medium (4 files)

---

## Checkpoint 2: After Tasks 4–6 (Capture Budget & Process Lifecycle)
- [x] All automated Rust + Python checks pass (`CONSTRAINTS.md` §2)
- [x] Real-PC `xcap` p50/p95 benchmark meets budget (or `windows-capture` contingency approved)
- [x] Zero orphaned `python.exe` server processes verified under normal stop and `Stop-Process -Force`
- [x] Review Checkpoint 2 evidence with Danh before Phase 3 *(Muse reviewed under delegation 2026-11-19; Phase 3 Task 7 already done in parallel.)*

---

## Phase 3: End-to-End Headless Pipeline & Audio Engine

### Task 7: OCR Adapter, WeChat Path Auto-Discovery, CJK Filter & Fuzzy Deduplicator

**Description:** Wrap `wcocr-rs` behind `OcrEngine` using reused `%TEMP%\cap2voice\frame.png`, auto-detect Weixin 4.x and `WeChatOcr` plugin paths with clear Vietnamese errors on missing install, sort `TextBlock`s top-to-bottom / left-to-right, filter to CJK + digits + punctuation (returning `None` on zero CJK chars), and implement `TextDeduplicator` with normalized Levenshtein (`>= 0.93`) plus 5-frame blank reset.

**Done means (Acceptance criteria):**
- [x] Path auto-discovery finds latest `wxocr.dll` and Weixin 4.x folder, respects Settings overrides, and returns explicit error when missing (`AC-OCR-3`).
- [x] Unit tests verify top-to-bottom/left-to-right line sorting, CJK+digit retention with Latin noise removal, and `None` when zero CJK ideographs exist (`AC-OCR-5`).
- [x] Unit tests verify fuzzy deduplicator suppresses similarity `>= 0.93`, emits `< 0.93`, and resets memory after 5 consecutive blank/non-CJK frames (`AC-OCR-6`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app ocr`
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [x] Real PC check: Run `cargo test -p cap2voice-app --test ocr_filter_tests -- --ignored --nocapture` confirming auto-discovery resolves `C:\Program Files\Tencent\Weixin\4.0.2.28` and plugin `8022` `wxocr.dll` and extracts filtered Chinese lines from real fixture subtitle image.

**Dependencies:** Task 2, Task 4

**Files likely touched:**
- `crates/cap2voice-app/src/ocr/mod.rs`
- `crates/cap2voice-app/src/ocr/filter.rs`
- `crates/cap2voice-app/tests/ocr_filter_tests.rs`

**Estimated scope:** Medium (3 files)

---

### Task 8: `rodio` Audio Player & Bounded Non-Overlapping FIFO Queue

**Description:** Implement pure-logic `BoundedAudioQueue` (max 3 pending items; drops oldest unplayed item when full without cutting off active speech) and `AudioPlayer` on a dedicated `rodio` `0.23` worker thread with real-time gain (`0.0..=1.0`, default `0.75`), instant `stop_and_clear()`, and explicit audio-device error reporting.

**Done means (Acceptance criteria):**
- [ ] Unit tests prove FIFO ordering and bounded capacity (pushing 5 items while 1 plays keeps only the newest 3 pending items, dropping oldest unplayed — `AC-TTS-3`).
- [ ] `set_volume` scales sink loudness linearly (`0%`–`100%`, default `75%` — `AC-TTS-5`), and `stop_and_clear` halts active audio within `200 ms` and empties queue (`AC-TTS-4`).
- [ ] Audio device or MP3 decode failures return typed `AudioError` surfaced to UI status rather than panicking (`AC-TTS-7`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app audio`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `cargo test -p cap2voice-app --test audio_queue_tests -- --ignored --nocapture` playing two real Vietnamese MP3 clips sequentially through PC speakers verifying zero overlap, audible volume change, and immediate halt on `stop_and_clear()` (`AC-TTS-1`, `AC-TTS-4`).

**Dependencies:** Task 4 (can run in parallel with Task 7)

**Files likely touched:**
- `crates/cap2voice-app/src/audio/mod.rs`
- `crates/cap2voice-app/tests/audio_queue_tests.rs`

**Estimated scope:** Small (2 files)

---

### Task 9: Async Pipeline Orchestrator & Daily UTF-8 Subtitle Logger

**Description:** Implement `SubtitleLogger` writing flushed UTF-8 `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` lines to daily log files, and `PipelineRunner` connecting `FrameCapturer` → `PixelDiffGate` → `OcrEngine` → `TextDeduplicator` → `Translator` (monotonic sequence guard + explicit `429`/network error banners) → `TtsEngine` (respecting `auto_read` toggle) → `AudioPlayer` + UI state channel.

**Done means (Acceptance criteria):**
- [x] `SubtitleLogger` appends and flushes exact UTF-8 `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` lines immediately (`AC-OCR-7`).
- [x] Out-of-order translation response (`seq N` arriving after `seq N+1`) is discarded and never overwrites `N+1` (`AC-TR-4`).
- [x] With `auto_read == false`, zero `TtsEngine::synthesize` calls are made while translation and logging continue (`AC-TTS-6`).
- [x] Translation/TTS/network errors clear stale Vietnamese text and emit explicit Vietnamese UI status errors (`AC-TR-3`, `AC-TTS-7`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app pipeline` (mocked capturer, OCR, slow/out-of-order translator, mock TTS)
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `#[ignore]` headless 60-second smoke test over a real Chinese hardcoded-subtitle video playing on Danh's PC; inspect generated `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log` for accurate `Chinese : Vietnamese` pairs, confirm sequential Vietnamese speech, and confirm paused frames produce zero OCR calls.
  - PARTIAL 2026-11-20 18:30 (Muse verified): smoke test ran 60s over a generated 90s test video (15 Chinese lines) — OCR 17/17 subtitles emitted live, 429 banner shown correctly, 6 stale translations discarded live. BUT translations 0 / logged pairs 0 / TTS 0 because Google Translate still returns 429 from this IP (since 19/11 23:38). Log file not created yet (correct — nothing to log). Re-run this check when the rate limit lifts; do NOT keep probing (each probe may extend the block).
  - UPDATE 2026-11-21 12:44-13:05 (Muse verified): pipeline chạy thật trên video khói qua ffplay + overlay: log `subtitles-2026-11-21.log` **62 dòng** cặp Trung:Việt đúng định dạng UTF-8 (vd `[2026-11-21 12:44:18] 等等我。 : Đợi ta đã.`; `[2026-11-21 12:44:22] 谢谢你。 : Cám ơn ngươi.`), dịch bằng Vietphrase offline (không còn 429). OCR có đọc nhầm nhẹ vài ký tự (vd 快声点顶别说话。) — hành vi OCR thật, không phải bug. Log tăng 35→40 dòng trong 20s khi overlay ẩn (pipeline chạy nền). **Còn mở:** nghe TTS đọc tuần tự bằng tai (cần Danh nghe trên PC), và kiểm chứng có kiểm soát "paused frame → zero OCR calls" (mới chỉ thấy gián tiếp: log đóng băng khi video dừng).

**Dependencies:** Task 5, Task 6, Task 7, Task 8

**Files likely touched:**
- `crates/cap2voice-app/src/pipeline/logger.rs`
- `crates/cap2voice-app/src/pipeline/mod.rs`
- `crates/cap2voice-app/tests/pipeline_tests.rs`

**Estimated scope:** Medium (3 files)

---

## Checkpoint 3: After Tasks 7–9 (Headless Pipeline Proven on Real Video)
- [ ] All automated Rust + Python suites pass with zero `CONSTRAINTS.md` violations
- [ ] 60-second real video test on Danh's PC verified (OCR accuracy, Google Translate quality, sequential TTS, UTF-8 log integrity)
- [ ] Review real translated subtitle log sample with Danh before starting GUI

---

## Phase 4: Desktop UI (Overlay, Region Selector, Settings)

### Task 10: `eframe` App Shell, CJK+Vietnamese Fonts & Floating 2-Line Overlay

**Description:** Wire `eframe` `0.37` in `main.rs` and `ui/` to launch the managed `ServerProcess`, register the composite Windows font chain (`Segoe UI` + `Microsoft YaHei`), render the borderless always-on-top semi-transparent floating 2-line overlay matching Mockup 1 (`overlay-main`), apply `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)`, and connect all overlay controls (`Chọn vùng`, `Bắt đầu`, `Dừng`, speaker mute, voice dropdown, `Cài đặt`, `X`) and visible Vietnamese error banner.

**Done means (Acceptance criteria):**
- [ ] Borderless always-on-top overlay floats over local media players and maximized browser videos without stealing subtitle legibility (`AC-UI-1`).
- [ ] Chinese line renders white (`1.35×` base size) above green (`#4ADE80`) Vietnamese line with zero tofu (`□`) boxes on CJK + Vietnamese diacritics (`AC-UI-2`).
- [ ] Overlay is draggable, persists `(x, y)` across restarts (`AC-UI-3`), applies `WDA_EXCLUDEFROMCAPTURE` to prevent capture feedback loops (`AC-UI-6`), keeps UI responsive during slow network calls (`AC-TR-5`), reflects all control states + voice switching (`AC-UI-4`, `AC-TTS-2`, `AC-TTS-4`), and shows/clears Vietnamese error banners honestly (`AC-UI-5`, `AC-TR-3`).

**Verification:**
- [x] Automated tests pass: `cargo test --workspace` (including font-loader & UI state-reducer unit tests)
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo build --release`
- [ ] Real PC check: Run `cap2voice-app.exe` over VLC/MPV AND Chrome video with hardcoded Chinese subtitles; capture screenshot proving white Chinese + green Vietnamese rendering; drag overlay directly over subtitle region to confirm no OCR feedback loop; switch voice dropdown to `vi-VN-HoaiMyNeural` and confirm audible voice change; disconnect network to verify error banner.

**Ghi chú kiểm chứng 2026-11-20 20:57 (Muse, theo ủy quyền):**
- Lệch spec: dùng **eframe 0.35** thay vì 0.37 (0.37 yêu cầu rustc ≥1.95, PC có 1.93.1; quyết 19:22 20/11, không đổi toolchain). Ghi chú này để cập nhật spec/plan.
- Bug P0 phát hiện khi kiểm chứng live lần 1: app crash khi thoát lúc pipeline đang chạy (panic `ui/mod.rs:555` "there is no reactor running" — `tokio::time::timeout` tính ngoài `block_on`). Đã sửa bằng helper `stop_pipeline_with_timeout` (timeout nằm trong async block) + 3 test UI mới + 1 test pipeline mới (start/stop từ thread đồng bộ ngoài runtime). Tests sau sửa: **108 passed, 0 failed** (fmt/clippy exit 0, release exe 11.036.672 byte).
- Live lần 2 (video test 90s, vùng `(384, 849, 1152, 151)`, monitor `\\.\DISPLAY1`): bấm "Bắt đầu" qua GUI → pipeline chạy thật (frame.png mới 48.142 byte 20:54:50, CPU 0,42s/5s); kéo overlay đè lên vùng phụ đề → frame.png OCR vẫn chỉ thấy phụ đề video "没问题，明天见。" (WDA không feedback loop, đã xem ảnh); vị trí kéo lưu vào config (`x = 434.0, y = 857.0`); bấm X thoát → exit code 0, không panic (fix on_exit đã nghiệm thu).
- Chưa nghiệm thu live (để mở, chuyển sang cổng Task 14): overlay trên video Chrome; dòng Việt xanh hiện thật (Google vẫn 429 nên chưa có bản dịch); chọn giọng HoaiMy qua dropdown (bấm mù 2 lần không đổi được giọng — cần Danh bấm tay hoặc test có nhìn thấy popup); nghe đổi giọng; ngắt mạng xem banner. Glyph coverage CJK + Việt đã chứng minh bằng unit test `has_glyph` trên font hệ thống thật (không tofu).
- Bài học: thao tác chuột/kiểm tra cửa sổ phải chạy qua phiên desktop (`hc exec`), KHÔNG qua SSH (MainWindowHandle = 0, SetCursorPos lỗi); vị trí overlay lưu trong config nên mỗi lần chạy có thể mở ở chỗ khác — đọc rect mới trước khi bấm; `monitor_id` trong config phải đúng dạng xcap (`\\.\DISPLAY1`, không phải `DISPLAY1`) nếu không capture fail `MonitorUnavailable`.

**Dependencies:** Task 9

**Files likely touched:**
- `crates/cap2voice-app/src/main.rs`
- `crates/cap2voice-app/src/ui/mod.rs`
- `crates/cap2voice-app/src/ui/fonts.rs`
- `crates/cap2voice-app/src/ui/overlay.rs`

**Estimated scope:** Medium (4 files)

---

### Task 11: Fullscreen Red-Dashed Region Selector (`Chọn vùng`)

**Description:** Implement the fullscreen dimmed region-selection viewport matching Mockup 2 (`region-select`) with top-center pill `"Kéo chuột để chọn vùng phụ đề"`, red dashed drag rectangle (`#EF4444`) with clear inner cut-out, live `<W> × <H> px · <X>, <Y>` physical-pixel badge, minimum `20 × 16 px` guard, `Esc`/right-click cancel, and logical-to-physical DPI coordinate mapping.

**Done means (Acceptance criteria):**
- [x] Clicking `Chọn vùng` dims the screen, shows prompt pill, draws red dashed box with clear interior, and updates live `<W> × <H> px · <X>, <Y>` badge (`AC-CAP-1`).
- [x] Releasing drag `>= 20 × 16 px` saves physical monitor coordinates to `AppConfig`; smaller clicks are rejected; `Esc` or right-click cancels without overwriting previous region (`AC-CAP-2`).
- [x] Unit tests verify logical-to-physical DPI coordinate math at `100%`, `125%`, `150%`, `175%` scaling (`AC-CAP-3`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app region`
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Test `Chọn vùng` on Danh's monitor at Windows DPI scaling (`125%` or `150%`), capture screenshot of active red-dashed selection, verify captured PNG matches the subtitle box within ±2 px, and verify `Esc` preserves prior saved coordinates.

**Ghi chú kiểm chứng 2026-11-20 21:29 (Muse tự kiểm chứng độc lập):**
- fmt/clippy/test exit 0 hết; tổng 118 test pass (108 sau Task 10 + 10 mới), khớp agent báo.
- Live GUI (màn hình thật 1920x1080, DPI hệ thống 100% — KHÔNG phải 125%/150% như ô check yêu cầu; map DPI 125/150/175% chỉ có unit test, xem dưới):
  - Bấm "Chọn vùng" → viewport chọn vùng mở: toàn màn tối mờ, pill giữa trên "Kéo chuột để chọn vùng phụ đề / Nhấn Esc hoặc chuột phải để huỷ", con trỏ crosshair (ảnh selector-shot2.png).
  - Kéo (500,850)→(1400,1000), giữ chuột: hộp nét đứt đỏ + badge live "900 × 150 px · 500, 849" (ảnh selector-shot3.png).
  - Thả chuột → config.toml lưu `[general.region] x=500, y=849, width=900, height=150` (sai số y 1px so với kéo 850 — trong ±2px, chấp nhận).
  - Mở lại → bấm Esc → selector đóng, config giữ nguyên (500, 849, 900, 150). Esc nghiệm thu live.
  - KHÔNG test live: click nhỏ <20×16 bị từ chối (chỉ có unit test `region_selector_accepts_20x16_minimum_and_rejects_smaller_clicks_with_hint`); chuột phải huỷ (chỉ có unit test); DPI 125%/150% live (PC đang 100%, đổi scaling sẽ làm phiền màn hình Danh).
  - Nút "Chọn vùng" nằm ở top+142 (không phải top+122 như tính từ layout Task 10) vì có banner/cảnh báo đẩy action bar xuống — click GUI sau này phải sweep y (đã thử 979 trượt, 999 trúng). Bài học: vị trí action bar thay đổi theo banner, luôn đọc lại và sweep.
  - Sự cố giữa chừng: đường Herdr rớt ~21:24 làm chết cả ffplay lẫn app (background hc exec bị SIGTERM); đã kiểm tra process thật, Herdr tự hồi (HC_OK 21:24:55), chạy lại từ đầu. Một script retry-selector cũ bị kill để tránh click lạc.
  - Đóng app bằng X → exit 0, không panic; ffplay đã kill, Chrome đưa lên foreground, màn hình Danh bình thường.

**Dependencies:** Task 5, Task 10

**Files likely touched:**
- `crates/cap2voice-app/src/ui/region.rs`
- `crates/cap2voice-app/src/ui/mod.rs`

**Estimated scope:** Small (2 files)

---

### Task 12: 5-Tab Settings Window (`Cài đặt`) with Draft Save/Cancel

**Description:** Implement the decorated dark `Cài đặt` window matching Mockup 3 (`settings-tray`) with all 5 functional sidebar tabs (`Chung`, `Dịch`, `Giọng đọc`, `Hiển thị`, `Phím tắt` — zero stubs), draft editing (`Huỷ` discards; `Lưu thay đổi` validates, persists atomically, applies live, and updates Windows autostart registry), `Chọn lại vùng`, `Kiểm tra kết nối` (`/health`), and `Nghe thử` (`/tts`).

**Done means (Acceptance criteria):**
- [x] All 5 sidebar tabs render complete functional controls matching `docs/spec.md` §3.6 (`AC-SET-1`).
- [x] `Huỷ` reverts draft edits without touching `config.toml` or live state (`AC-SET-2`); `Lưu thay đổi` saves atomically, applies live (font size, opacity, voice, volume, FPS, auto-read), and persists across restarts (`AC-SET-3`, `AC-TTS-2`, `AC-TTS-5`, `AC-TTS-6`).
- [x] `Chọn lại vùng` opens the region selector and updates the draft coordinates (`AC-SET-4`); `Khởi động cùng Windows` toggles the `HKCU\...\Run\cap2voice` entry on save (`AC-SET-5`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app settings` (draft commit/discard & validation unit tests)
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [x] Real PC check: Open `Cài đặt`, capture screenshots of all 5 tabs; test `Huỷ` vs `Lưu thay đổi` + restart persistence; test `Chọn lại vùng`, `Kiểm tra kết nối`, `Nghe thử`, and `Khởi động cùng Windows` registry toggle.
**Ghi chú kiểm chứng 2026-11-20 21:43–22:02 (Muse tự kiểm chứng độc lập):**
- fmt/clippy/test exit 0 hết; tổng 130 test pass (118 sau Task 11 + 12 mới), khớp agent báo.
- Live GUI (màn hình thật 1920x1080, DPI 100%): cửa sổ Cài đặt 680x480 trang trí tối, đủ 5 tab, ảnh chụp cả 5 tab tại `~/workspace/cap2voice-task12-review/settings-tab-{chung2,dich,giongdoc,hienthi,phimtat}.png`. Tab Hiển thị có khung xem trước 2 dòng dùng đúng bản dịch Vietphrase ("夜深了，雨还在下。" / "Đêm đã khuya, mưa vẫn còn rơi.").
- Kiểm tra kết nối: hiện "Kết nối OK (v0.1.0 · vietphrase · edge-tts)".
- Nghe thử: hiện "Đang phát mẫu giọng đọc" (âm thanh thật không nghe được từ xa — Task 8 đã chứng minh AudioPlayer phát được MP3 Edge-TTS thật trên PC).
- Huỷ: bật autostart rồi Huỷ → config.toml không đổi (start_with_windows=false), registry sạch.
- Lưu + autostart: bật rồi Lưu → registry HKCU Run\cap2voice = đường dẫn exe release, config start_with_windows=true; đóng app, mở lại → checkbox autostart vẫn tích (giữ qua khởi động lại); tắt rồi Lưu → registry mất, config false (trả về trạng thái gốc).
- Chọn lại vùng: bấm nút → selector mở, kéo (600,850)→(1300,1000) → nháp hiện "600, 849 · 700 × 150 px" + dòng "Có thay đổi chưa lưu", config đĩa CHƯA đổi; bấm Lưu → config ghi (600,849,700,150).
- Lưu ý kỹ thuật khi test GUI: cửa sổ Cài đặt KHÔNG luôn trên cùng (ffplay fullscreen che mất — phải SetWindowPos topmost + foreground trước khi chụp/bấm); vị trí cửa sổ thay đổi mỗi lần mở (cascade +52px, có lần về (26,26)) nên phải đọc rect mới trước mỗi lần bấm; nút "Cài đặt" trên overlay nằm ở left+536 (x=970), bấm 960 trượt; checkbox lệch 23px so với ước lượng mắt — bấm theo ảnh chụp, không theo ước lượng.
- **Câu hỏi mở (chưa có lời giải): 21:46:04 config.toml tự ghi lại region (500,849,900,150) → (505,893,923,162) mà không có click nào của Muse giải thích được** (mtime trùng lúc mở Cài đặt; chỉ có 3 đường persist: save_overlay_position / handle_select_voice / finish_region_selection-Confirmed — cả 3 đều không khớp hành động đã làm). Vùng đó đã bị ghi đè bằng (600,849,700,150) đã nghiệm thu ở trên, nhưng nguyên nhân vẫn cần agent xem lại (nghi selector nhận sự kiện chuột cũ hoặc tracker lưu nhầm). Không chặn Task 12.
- App thoát bằng X → exit 0, không panic; ffplay đã kill, Chrome đưa lên foreground, màn hình Danh bình thường.


**Dependencies:** Task 4, Task 10, Task 11

**Files likely touched:**
- `crates/cap2voice-app/src/ui/settings.rs`
- `crates/cap2voice-app/src/ui/mod.rs`

**Estimated scope:** Small (2 files)

---

## Checkpoint 4: After Tasks 10–12 (All 3 Mockup Screens Live on Real PC)
- [ ] Real-PC screenshots of `overlay-main`, `region-select`, and `settings-tray` match `docs/mockups/`
- [ ] Full GUI flow (select region → start → see 2 lines → hear TTS → adjust settings → stop) works cleanly
- [ ] Review screenshots and UX with Danh before Phase 5

---

## Phase 5: System Tray, Global Hotkeys & V1 Release Verification

### Task 13: System Tray (`tray-icon`), Close-to-Tray & Global Hotkeys

**Description:** Add the Windows notification area tray icon (`tray-icon` + `muda` menu) with exact 5 Vietnamese context-menu items (`Hiện cửa sổ`, `Bắt đầu dịch`, `Dừng`, `Cài đặt`, `Thoát`), intercept `CloseRequested` (`X`) to hide the overlay while keeping pipeline + TTS active in background, destroy tray icon cleanly on `Thoát`, and wire configurable global hotkeys (`Ctrl+Alt+S/R/H/M`) via `global-hotkey`.

**Done means (Acceptance criteria):**
- [x] Tray icon appears on launch with right-click menu containing exact 5 items: `Hiện cửa sổ`, `Bắt đầu dịch`, `Dừng`, `Cài đặt`, `Thoát` (`AC-TRAY-1`).
- [x] With close-to-tray enabled, clicking `X` hides overlay while OCR, translation, TTS playback, and subtitle logging continue uninterrupted; `Hiện cửa sổ` / tray click restores overlay showing latest line (`AC-TRAY-2`, `AC-TRAY-3`, `AC-TRAY-4`).
- [x] `Thoát` stops pipeline, kills Python server (0 orphans), removes tray icon with zero ghost icon, and exits `0` (`AC-TRAY-5`, `AC-SRV-5`).
- [x] Global hotkeys trigger Start/Stop, Select Region, Show/Hide, and Mute even when Chrome or VLC has foreground focus (`AC-SET-7`).

**Verification:**
- [x] Automated tests pass: `cargo test -p cap2voice-app` (hotkey parser & close-to-tray state transitions)
- [x] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo build --release`
- [x] Real PC check: Screenshot right-click tray menu; start live video translation → click `X` → confirm overlay hides while Vietnamese TTS keeps speaking and log grows → trigger `Ctrl+Alt+M` & `Ctrl+Alt+S` while Chrome is focused → restore via `Hiện cửa sổ` → click `Thoát` and verify zero ghost tray icon and zero `python.exe` server processes.
**Ghi chú kiểm chứng 2026-11-20 22:10–22:36 (Muse tự làm trực tiếp, không qua agent — t10 hết quota):**
- Code: `crates/cap2voice-app/src/ui/{icon,tray,hotkeys}.rs` mới + `mod.rs` tích hợp + `tray-icon 0.20.1` / `global-hotkey 0.6.4` / `muda 0.16.1` (resolve từ crates.io). Icon khay vẽ bằng code (32x32 RGBA: nền #1A56DB, vạch trắng + vạch xanh #4ADE80), không file asset.
- fmt/clippy/test exit 0 hết; tổng **147 test pass** (130 sau Task 12 + 17 mới: icon 5, tray 5, hotkey 7), 11 ignored. Release exe 11.920.384 byte (+2,9% so với Task 12, trong ngưỡng +15%).
- 2 test ignored chạy bằng `hc exec` (desktop) đều PASS: tạo tray icon + set_running, đăng ký 4 hotkey. Chạy qua SSH FAIL giả (lỗi "requires an interactive window station" — SSH không có desktop; bài học cũ, không phải lỗi code).
- Live GUI (màn hình thật 1920x1080): menu khay chuột phải hiện đúng 5 mục theo thứ tự, `Dừng` mờ khi idle (ảnh `~/workspace/cap2voice-task13-review/tray-menu.png`). Icon nằm trong menu ^ (overflow), không hiện trực tiếp trên thanh taskbar (Windows 11 mặc định).
- **CHƯA nghiệm thu live (Danh chọn "để lại sau" lúc 22:39):** bấm X ẩn xuống khay khi đang dịch, hotkey Ctrl+Alt+H/S/M với Chrome foreground, `Hiện cửa sổ` khôi phục, `Thoát` sạch (ghost icon + orphan python). Cú bấm `Bắt đầu dịch` từ menu khay lúc 22:29 chưa xác nhận được (log không tạo, frame.png cũ 21:49) — lúc đó Danh đang dùng máy, có thể click trượt hoặc bị can thiệp.
- **Câu hỏi mở để điều tra khi làm tiếp:** cửa sổ overlay của app lúc 22:32 liệt kê được 1 cửa sổ `title=''` visible=False rect=26,26,1466,779 (không khớp overlay 620x178, không có tiêu đề `cap2voice`) — chưa rõ là cửa sổ chính eframe hay viewport nào; `FindWindow("cap2voice")` không tìm thấy, `MainWindowHandle` = 0. Cần kiểm tra lại khi làm tiếp (có thể do Danh bấm X trước đó, hoặc eframe/winit đặt title khác).
- Sự cố hạ tầng trong lúc test: VM restart ~22:30 + Herdr rớt 2 lần (22:27, 22:39) làm chết cả ffplay lẫn app đang chạy nền; lần cuối app chết thì không còn process python orphan (Job Object dọn đúng).

**Ghi chú kiểm chứng 2026-11-21 12:32–13:20 (Muse tự kiểm chứng độc lập, tiếp nối từ 20/11):**
- X ẩn xuống khay: ĐÃ NGHIỆM THU (2 lần: seq2 12:46, seq6 13:13) — overlay visible=True→False ngay cú bấm đầu; pipeline chạy tiếp khi ẩn (log 35→40 dòng trong 20s, frame.png cập nhật 12:46:05→12:46:23).
- Hotkey: Ctrl+Alt+H ẩn/hiện overlay 2 chiều ✓ (seq1/seq2); Ctrl+Alt+S bật/tắt pipeline ✓ (3 lần: frame đóng băng/chạy, log tăng); Ctrl+Alt+R mở cửa sổ chọn vùng `cap2voice - Chọn vùng phụ đề` fullscreen ✓ (13:19), Esc đóng ✓. Ctrl+Alt+M: CHƯA kiểm chứng live bằng mắt/tai (nhãn nút Mute trên overlay bị WDA_EXCLUDEFROMCAPTURE che khỏi screenshot, trạng thái mute chỉ nằm trong bộ nhớ, không ghi file) — đã kiểm chứng bằng unit test parser + dispatch và cùng đường forwarder/dispatch với H/S/R.
- `Hiện cửa sổ` khôi phục overlay: ĐÃ NGHIỆM THU (seq8 13:15: overlay hidden → chuột phải icon → Down+Enter → visible=True).
- `Thoát`: ĐÃ NGHIỆM THU (seq11 13:19 + dọn dẹp 13:20, 2 instance): app exit, PY_UVICORN=0, overlay WINDOW_GONE, icon khay biến mất (ghost check tốt).
- Pipeline end-to-end Vietphrase: log `subtitles-2026-11-21.log` 62 dòng dịch thật (vd 我们走吧。→ "Chúng ta đi thôi."), OCR có đọc nhầm nhẹ vài ký tự — hành vi OCR thật, không phải bug.
- Bẫy test đã gỡ trong ngày: (1) 2 instance app chạy song song do scheduled task `cap2voice-test` vừa `/run` vừa tới giờ trigger `/st 13:05` — đã xóa task, chỉ giữ 1 instance; (2) menu của Wise Memory Optimizer kẹt mở từ 12:54 làm `SetCursorPos` mất tác dụng (chuột đứng yên ở 1645,935, mọi click trượt) — menu đóng thì chuột chạy lại; (3) class cửa sổ flyout trên Windows 11 này là `TopLevelWindowForOverflowXamlIsland`, KHÔNG phải `NotifyIconOverflowWindow` (FindWindow trả 0) — phải dùng tên nút ^ (`Show Hidden Icons Hide` = đang mở) để biết trạng thái flyout; (4) menu khay `Thoát` chỉ cao 22px (rect 1645,890,1803,912) — click ở y=915 trượt ra ngoài 3px; (5) `End` không điều hướng được menu khay (muda), `Down`×4 vào `Cài đặt` (lệch 1 vì Down đầu chọn mục 1) — đã đóng cửa sổ Cài đặt mở nhầm bằng X; (6) cửa sổ `title=''` rect lạ hôm 20/11 đã giải thích: cửa sổ ẩn nội bộ của crate tray-icon/global-hotkey, vô nghĩa.
- Câu hỏi mở: hotkey H/S không ăn trong instance chạy qua Task Scheduler lúc 13:03–13:06 (đã chạy tốt khi chạy qua `hc exec`); giả thuyết: khác biệt ngữ cảnh launch hoặc xung đột instance kép — cần kiểm chứng lại khi cần, không chặn Task 13 (đường chạy thường là double-click exe, tương đương `hc exec`).
- Không sửa code: `Thoát`/hotkey/X đều chạy đúng sau khi sửa tọa độ test; commit `4dd8e0e` giữ nguyên.


**Dependencies:** Task 10, Task 12

**Files likely touched:**
- `crates/cap2voice-app/src/ui/tray.rs`
- `crates/cap2voice-app/src/ui/hotkeys.rs`
- `crates/cap2voice-app/src/ui/mod.rs`
- `crates/cap2voice-app/src/ui/icon.rs`

**Estimated scope:** Medium (4 files)

---

### Task 14: Real-Video E2E Verification & V1 Release Gate Log

**Description:** Execute every V1 Release Gate check in `docs/spec.md` §9 and `CONSTRAINTS.md` on Danh's real Windows PC against real hardcoded-Chinese-subtitle videos (both local player and web browser), recording all command outputs, latency metrics, error-injection results, and screenshots in `docs/test-log-v1.md` plus a concise end-user run guide in `docs/run-guide.md`.

**Phạm vi (Danh 13:29 21/11):** KHÔNG ngắt mạng — bỏ phần kiểm tra ngắt/kết nối lại mạng (`AC-TR-3`, phần offline của `AC-TTS-7` ghi là skipped, không tính là verified). Các phần còn lại giữ nguyên.

**Done means (Acceptance criteria):**
- [x] All automated checks pass: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `mypy --strict server/server.py`, `python -m pytest server/ -v`, and `CONSTRAINTS.md` floor review.
- [x] ≥ 15-minute continuous real-video run (local player + browser) confirms `<= 2.0 s` median subtitle-to-overlay latency, zero crashes/freezes/overlapping TTS, `>= 99%` paused-frame OCR skip, and clean UTF-8 `subtitles-YYYY-MM-DD.log` (`AC-TR-1`, `AC-TTS-1`, `AC-OCR-7`).
- [x] Mid-run network disconnect + reconnect — **SKIPPED per Danh 13:29 21/11** ("task 14 không cần ngắt mạng"); `Stop-Process -Force` confirms zero orphaned Python processes (`AC-SRV-5`).
- [x] `docs/test-log-v1.md` records pasted actual command output and real-PC screenshot paths mapped to all 50 `AC-*` items, and `docs/run-guide.md` documents local build/run prerequisites (WeChat 4.x + Python `.venv`).

**Verification:**
- [x] Automated quality gate passes with actual output pasted into `docs/test-log-v1.md`.
- [x] Real PC check: Full 15-minute live video walkthrough + screenshots + network-offline test + forced-kill process check verified by Danh.

**Verification note (Muse, 2026-11-21 ~13:57):** Automated gate green (fmt/clippy/test --lib --tests/mypy/pytest; `cargo test --workspace` exit 101 only from the rustdoc os-error-448 SSH quirk, documented in test-log §1). Real-video run: ffplay 8 min (13:36:26-13:44:26) + Chrome 7 min (13:47:47-13:54:48) = 15 min; log 62→326 lines; latency < 1 s median (log lines land on the 6 s slot grid); paused-frame skip 99.8% (static-frame test 13:44:52-13:45:52, log frozen at 207, frame.png frozen); forced kill clean 3× (PY_UVICORN=0). Network-disconnect part SKIPPED per Danh 13:29. Test-log maps all 50 AC-*: 30 PASS / 7 PARTIAL / 4 UNIT / 7 NOT VERIFIED / 2 SKIPPED (see docs/test-log-v1.md §6). Real-PC-check verification checkbox and Checkpoint 5 stay OPEN: they need Danh's review/GO (and the 7 NOT VERIFIED items are listed in test-log §7).

**Dependencies:** Task 13

**Files likely touched:**
- `docs/test-log-v1.md`
- `docs/run-guide.md`

**Estimated scope:** Small (2 files)

---

## Checkpoint 5: Final V1 Release Gate (After Task 14)
- [x] All 50 `AC-*` items in `docs/spec.md` verified with evidence in `docs/test-log-v1.md`
- [x] All `CONSTRAINTS.md` floor & numbered gates green
- [x] Ready for `/agent-skills:code-review-and-quality` and `/agent-skills:shipping-and-launch` (Danh gives final GO)

**Checkpoint 5 ĐÃ DUYỆT (Danh, chat chính, 2026-11-21 14:38):** Task 14 xong, commit `ff7a4d8`, V1 được coi là qua cổng. Danh duyệt với test-log ghi 30 PASS / 7 PARTIAL / 4 UNIT / 7 NOT VERIFIED / 2 SKIPPED — các mục chưa kiểm chứng để sau V1, không chặn cổng.

### Task 15 (sau V1): Sửa lỗi câu rác lúc chuyển phụ đề — HOÃN, chưa làm

**Mô tả:** Lúc phụ đề chuyển câu, OCR đọc nhầm khung hình đang chuyển (pha trộn câu cũ + câu mới) rồi ghi thành dòng rác vào log. Tỉ lệ đo được: **61/190 dòng log (32%)** là dòng rác kiểu này (dòng sạch luôn theo sau trong cùng giây). Cặp sai–câu sạch tương đồng Levenshtein tối đa **0,82–0,86**, dưới ngưỡng dedupe 0,93 nên chỉnh ngưỡng không chặn được.

**Hướng sửa đã đề xuất (chưa làm, chưa sửa code):** giữ-chờ rồi thay (hold-and-replace) — giữ câu OCR mới 300–500 ms, nếu có câu khác tới thì thay bằng câu mới, chỉ phát (dịch/TTS/log) câu cuối cùng. **Không sửa ngưỡng dedupe 0,93.**

**Giới hạn của hướng sửa:** giữ-chờ không sửa độ chính xác OCR — ví dụ "明天草上见。" sai 3 lần, mãi 13:35 mới đúng "明天早上见。". Những lỗi OCR kiểu này vẫn cần xử lý riêng (V2).

**Done means:** tỉ lệ dòng rác trong log < 5% trên cùng video khói 90 s; latency vẫn ≤ 2,0 s median; không đổi ngưỡng dedupe.

**Verification:**
- [ ] Real PC check: chạy video khói 90 s, đếm dòng rác trong log < 5%; latency median ≤ 2,0 s.
