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
- [ ] `WeChatOcr` returns `Err(OcrError::InvalidPath)` when `wxocr.dll` or Weixin directory is missing (`AC-OCR-3`).
- [ ] Unit test simulating an unresponsive task slot verifies `Err(OcrError::RequestTimeout)` returns within `timeout + 100 ms` (`AC-OCR-4`).
- [ ] `#[ignore]` real-PC integration test recognizes Chinese lines from `test-zh-custom.png` via local Weixin `4.0.2.28` + plugin `8022` with `errcode == 0` (`AC-OCR-1`, `AC-OCR-2`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p wcocr-rs`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `cargo test -p wcocr-rs -- --ignored --nocapture` with `WXOCR=%APPDATA%\Tencent\xwechat\XPlugin\plugins\WeChatOcr\8022\extracted\wxocr.dll` and `WXDIR=C:\Program Files\Tencent\Weixin\4.0.2.28`; paste recognized Chinese text blocks and elapsed OCR ms into the test log (also note whether `pic_data` in-memory works or stays deferred to temp file).

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
- [ ] `GET /health` returns `200 OK` with `{"status":"ok","version":"0.1.0","translator":"google-free","tts":"edge-tts"}` (`AC-SRV-1`).
- [ ] `POST /translate` translates Chinese → Vietnamese (`200`), rejects empty/whitespace input (`400`), and maps upstream `429` / network errors to structured `429` / `502` JSON (`AC-SRV-2`, `AC-TR-1`).
- [ ] `POST /tts` returns `audio/mpeg` MP3 bytes for valid Vietnamese text + voice/rate, rejects empty text or invalid voice format (`400`), and maps Edge-TTS failures to `502`/`503` (`AC-SRV-3`).

**Verification:**
- [ ] Static types pass: `mypy --strict server/server.py`
- [ ] Automated tests pass: `python -m pytest server/ -v` (with mocked `httpx` and `edge_tts` covering happy paths + `400`/`429`/`502` failures)
- [ ] Real PC check: Create `server/.venv`, install `server/requirements.txt`, start `uvicorn` on `127.0.0.1:18765`, call live `GET /health`, `POST /translate` with `"夜深了，雨还在下。"`, and `POST /tts` for both `vi-VN-NamMinhNeural` and `vi-VN-HoaiMyNeural` (play resulting MP3 to confirm intelligible Vietnamese speech), then disconnect network and confirm `502`/`503` JSON response; stop server cleanly.

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
- [ ] `AppConfig` serializes/deserializes cleanly via TOML with exact defaults from `docs/spec.md` (`10 fps`, `vi-VN-NamMinhNeural`, volume `0.75`, font `18 px`, opacity `0.85`, dedupe `0.93`, pixel-diff `0.02`, port `18765`, close-to-tray `true`, autostart `false`, default hotkeys `Ctrl+Alt+S/R/H/M`) (`AC-SET-3`).
- [ ] Corrupt TOML triggers typed `ConfigLoadWarning`, copies broken file to `config.toml.bak`, and never silently destroys user data (`AC-SET-6`).
- [ ] Windows autostart helper writes and deletes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice` cleanly (`AC-SET-5`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app config`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run ignored Windows registry test on Danh's PC verifying `Get-ItemProperty HKCU:\Software\Microsoft\Windows\CurrentVersion\Run` adds then removes `cap2voice` without requiring admin/UAC, and confirm atomic write + `.bak` creation in a temp `%APPDATA%` path.

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
- [ ] Unit tests prove `PixelDiffGate` bypasses on frame 1, skips `>= 99%` identical static frames (`AC-CAP-4`), and triggers immediately on subtitle pixel change (`AC-CAP-5`).
- [ ] Disconnected monitor or out-of-bounds region returns explicit `CaptureError::MonitorUnavailable` / `OutOfBounds` without panicking (`AC-CAP-7`).
- [ ] Real-PC 100-frame benchmark logs p50/p95 capture latency (`<= 30 ms` median / `<= 50 ms` p95 target — `AC-CAP-6`) and saves a verification PNG crop matching physical coordinates (`AC-CAP-3`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app capture`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check (FAIL-FAST GATE): Run `cargo test -p cap2voice-app --test capture_bench -- --ignored --nocapture` on Danh's display (including scaled DPI check); inspect saved crop PNG and record p50/p95 ms in test log. If p95 > `50 ms`, stop and ask Danh to approve switching `FrameCapturer` to `windows-capture` `2.0.1`.

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
- [ ] Mock HTTP server tests verify exact JSON payloads and typed mapping of `400`, `429`, `502`/`503`, timeout, and connection refused (`AC-TR-2`).
- [ ] `ServerProcess::start` launches Python server invisibly and passes `GET /health` within `10 s`, or returns clear setup diagnostics if Python/deps missing (`AC-SRV-4`, `AC-SRV-6`).
- [ ] Dropping `ServerProcess` OR force-killing the parent process terminates the Python child via Windows Job Object with zero orphans (`AC-SRV-5`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app api`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `cargo test -p cap2voice-app --test api_client_tests -- --ignored --nocapture` to spawn real `server/server.py`, verify no console window flashes, verify `GET /health` 200, then verify `Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'server:app' }` returns empty after both graceful drop and forced parent kill.

**Dependencies:** Task 3, Task 4 (can run in parallel with Task 5)

**Files likely touched:**
- `crates/cap2voice-app/src/api/mod.rs`
- `crates/cap2voice-app/src/api/http_client.rs`
- `crates/cap2voice-app/src/api/server_proc.rs`
- `crates/cap2voice-app/tests/api_client_tests.rs`

**Estimated scope:** Medium (4 files)

---

## Checkpoint 2: After Tasks 4–6 (Capture Budget & Process Lifecycle)
- [ ] All automated Rust + Python checks pass (`CONSTRAINTS.md` §2)
- [ ] Real-PC `xcap` p50/p95 benchmark meets budget (or `windows-capture` contingency approved)
- [ ] Zero orphaned `python.exe` server processes verified under normal stop and `Stop-Process -Force`
- [ ] Review Checkpoint 2 evidence with Danh before Phase 3

---

## Phase 3: End-to-End Headless Pipeline & Audio Engine

### Task 7: OCR Adapter, WeChat Path Auto-Discovery, CJK Filter & Fuzzy Deduplicator

**Description:** Wrap `wcocr-rs` behind `OcrEngine` using reused `%TEMP%\cap2voice\frame.png`, auto-detect Weixin 4.x and `WeChatOcr` plugin paths with clear Vietnamese errors on missing install, sort `TextBlock`s top-to-bottom / left-to-right, filter to CJK + digits + punctuation (returning `None` on zero CJK chars), and implement `TextDeduplicator` with normalized Levenshtein (`>= 0.93`) plus 5-frame blank reset.

**Done means (Acceptance criteria):**
- [ ] Path auto-discovery finds latest `wxocr.dll` and Weixin 4.x folder, respects Settings overrides, and returns explicit error when missing (`AC-OCR-3`).
- [ ] Unit tests verify top-to-bottom/left-to-right line sorting, CJK+digit retention with Latin noise removal, and `None` when zero CJK ideographs exist (`AC-OCR-5`).
- [ ] Unit tests verify fuzzy deduplicator suppresses similarity `>= 0.93`, emits `< 0.93`, and resets memory after 5 consecutive blank/non-CJK frames (`AC-OCR-6`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app ocr`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `cargo test -p cap2voice-app --test ocr_filter_tests -- --ignored --nocapture` confirming auto-discovery resolves `C:\Program Files\Tencent\Weixin\4.0.2.28` and plugin `8022` `wxocr.dll` and extracts filtered Chinese lines from real fixture subtitle image.

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
- [ ] `SubtitleLogger` appends and flushes exact UTF-8 `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` lines immediately (`AC-OCR-7`).
- [ ] Out-of-order translation response (`seq N` arriving after `seq N+1`) is discarded and never overwrites `N+1` (`AC-TR-4`).
- [ ] With `auto_read == false`, zero `TtsEngine::synthesize` calls are made while translation and logging continue (`AC-TTS-6`).
- [ ] Translation/TTS/network errors clear stale Vietnamese text and emit explicit Vietnamese UI status errors (`AC-TR-3`, `AC-TTS-7`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app pipeline` (mocked capturer, OCR, slow/out-of-order translator, mock TTS)
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Run `#[ignore]` headless 60-second smoke test over a real Chinese hardcoded-subtitle video playing on Danh's PC; inspect generated `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log` for accurate `Chinese : Vietnamese` pairs, confirm sequential Vietnamese speech, and confirm paused frames produce zero OCR calls.

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
- [ ] Automated tests pass: `cargo test --workspace` (including font-loader & UI state-reducer unit tests)
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo build --release`
- [ ] Real PC check: Run `cap2voice-app.exe` over VLC/MPV AND Chrome video with hardcoded Chinese subtitles; capture screenshot proving white Chinese + green Vietnamese rendering; drag overlay directly over subtitle region to confirm no OCR feedback loop; switch voice dropdown to `vi-VN-HoaiMyNeural` and confirm audible voice change; disconnect network to verify error banner.

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
- [ ] Clicking `Chọn vùng` dims the screen, shows prompt pill, draws red dashed box with clear interior, and updates live `<W> × <H> px · <X>, <Y>` badge (`AC-CAP-1`).
- [ ] Releasing drag `>= 20 × 16 px` saves physical monitor coordinates to `AppConfig`; smaller clicks are rejected; `Esc` or right-click cancels without overwriting previous region (`AC-CAP-2`).
- [ ] Unit tests verify logical-to-physical DPI coordinate math at `100%`, `125%`, `150%`, `175%` scaling (`AC-CAP-3`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app region`
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Test `Chọn vùng` on Danh's monitor at Windows DPI scaling (`125%` or `150%`), capture screenshot of active red-dashed selection, verify captured PNG matches the subtitle box within ±2 px, and verify `Esc` preserves prior saved coordinates.

**Dependencies:** Task 5, Task 10

**Files likely touched:**
- `crates/cap2voice-app/src/ui/region.rs`
- `crates/cap2voice-app/src/ui/mod.rs`

**Estimated scope:** Small (2 files)

---

### Task 12: 5-Tab Settings Window (`Cài đặt`) with Draft Save/Cancel

**Description:** Implement the decorated dark `Cài đặt` window matching Mockup 3 (`settings-tray`) with all 5 functional sidebar tabs (`Chung`, `Dịch`, `Giọng đọc`, `Hiển thị`, `Phím tắt` — zero stubs), draft editing (`Huỷ` discards; `Lưu thay đổi` validates, persists atomically, applies live, and updates Windows autostart registry), `Chọn lại vùng`, `Kiểm tra kết nối` (`/health`), and `Nghe thử` (`/tts`).

**Done means (Acceptance criteria):**
- [ ] All 5 sidebar tabs render complete functional controls matching `docs/spec.md` §3.6 (`AC-SET-1`).
- [ ] `Huỷ` reverts draft edits without touching `config.toml` or live state (`AC-SET-2`); `Lưu thay đổi` saves atomically, applies live (font size, opacity, voice, volume, FPS, auto-read), and persists across restarts (`AC-SET-3`, `AC-TTS-2`, `AC-TTS-5`, `AC-TTS-6`).
- [ ] `Chọn lại vùng` opens the region selector and updates the draft coordinates (`AC-SET-4`); `Khởi động cùng Windows` toggles the `HKCU\...\Run\cap2voice` entry on save (`AC-SET-5`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app settings` (draft commit/discard & validation unit tests)
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Real PC check: Open `Cài đặt`, capture screenshots of all 5 tabs; test `Huỷ` vs `Lưu thay đổi` + restart persistence; test `Chọn lại vùng`, `Kiểm tra kết nối`, `Nghe thử`, and `Khởi động cùng Windows` registry toggle.

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
- [ ] Tray icon appears on launch with right-click menu containing exact 5 items: `Hiện cửa sổ`, `Bắt đầu dịch`, `Dừng`, `Cài đặt`, `Thoát` (`AC-TRAY-1`).
- [ ] With close-to-tray enabled, clicking `X` hides overlay while OCR, translation, TTS playback, and subtitle logging continue uninterrupted; `Hiện cửa sổ` / tray click restores overlay showing latest line (`AC-TRAY-2`, `AC-TRAY-3`, `AC-TRAY-4`).
- [ ] `Thoát` stops pipeline, kills Python server (0 orphans), removes tray icon with zero ghost icon, and exits `0` (`AC-TRAY-5`, `AC-SRV-5`).
- [ ] Global hotkeys trigger Start/Stop, Select Region, Show/Hide, and Mute even when Chrome or VLC has foreground focus (`AC-SET-7`).

**Verification:**
- [ ] Automated tests pass: `cargo test -p cap2voice-app` (hotkey parser & close-to-tray state transitions)
- [ ] Build & lint pass: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo build --release`
- [ ] Real PC check: Screenshot right-click tray menu; start live video translation → click `X` → confirm overlay hides while Vietnamese TTS keeps speaking and log grows → trigger `Ctrl+Alt+M` & `Ctrl+Alt+S` while Chrome is focused → restore via `Hiện cửa sổ` → click `Thoát` and verify zero ghost tray icon and zero `python.exe` server processes.

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

**Done means (Acceptance criteria):**
- [ ] All automated checks pass: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `mypy --strict server/server.py`, `python -m pytest server/ -v`, and `CONSTRAINTS.md` floor review.
- [ ] ≥ 15-minute continuous real-video run (local player + browser) confirms `<= 2.0 s` median subtitle-to-overlay latency, zero crashes/freezes/overlapping TTS, `>= 99%` paused-frame OCR skip, and clean UTF-8 `subtitles-YYYY-MM-DD.log` (`AC-TR-1`, `AC-TTS-1`, `AC-OCR-7`).
- [ ] Mid-run network disconnect + reconnect confirms visible Vietnamese error banner and clean recovery (`AC-TR-3`, `AC-TTS-7`); `Stop-Process -Force` confirms zero orphaned Python processes (`AC-SRV-5`).
- [ ] `docs/test-log-v1.md` records pasted actual command output and real-PC screenshot paths mapped to all 50 `AC-*` items, and `docs/run-guide.md` documents local build/run prerequisites (WeChat 4.x + Python `.venv`).

**Verification:**
- [ ] Automated quality gate passes with actual output pasted into `docs/test-log-v1.md`.
- [ ] Real PC check: Full 15-minute live video walkthrough + screenshots + network-offline test + forced-kill process check verified by Danh.

**Dependencies:** Task 13

**Files likely touched:**
- `docs/test-log-v1.md`
- `docs/run-guide.md`

**Estimated scope:** Small (2 files)

---

## Checkpoint 5: Final V1 Release Gate (After Task 14)
- [ ] All 50 `AC-*` items in `docs/spec.md` verified with evidence in `docs/test-log-v1.md`
- [ ] All `CONSTRAINTS.md` floor & numbered gates green
- [ ] Ready for `/agent-skills:code-review-and-quality` and `/agent-skills:shipping-and-launch` (Danh gives final GO)
