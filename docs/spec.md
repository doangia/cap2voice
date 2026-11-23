# Spec: cap2voice — V1 (MVP)

> **Status:** Approved — 2026-11-19 (Danh via delegation to Muse; all 12 assumptions in §11 confirmed as-is)
> **D1+A5 amendment — 2026-11-23:** The temporary Python server is **gone**. Translation and TTS are now native in-process Rust (§3.8 rewritten): a selectable engine (Helsinki-NLP `opus-mt-zh-vi` ONNX, lazy-loaded, or offline Vietphrase) and a native Edge-TTS WebSocket client. §3.8's `AC-SRV-1..5`, the `429`/`RateLimited` path, and the server-port/`GET /health` UI are superseded; `AC-TR-2`'s mock-HTTP-server wording is superseded by native-engine error tests. The rest of this spec stands.
> **Version:** V1 (MVP)  
> **Date:** 2026-11-19  
> **Owner / Approver:** Danh  
> **Sources of Truth:** `brief.md`, `AGENTS.md`, `docs/mockups/`  
> **Target Platform:** Windows 10 / Windows 11 (x86_64)

---

## 1. Objective

`cap2voice` is a Windows desktop application that helps Chinese-language learners watch videos containing **hardcoded (burned-in) Chinese subtitles** from any video source (local files, standalone media players, or browser streams).

- The app **only observes a user-selected screen region** — it is **not** a video player and never reads video files or inspects browser tabs/DOM.
- In real time, it captures the subtitle region, skips visually unchanged frames, runs local WeChat OCR (`wcocr-rs`), filters to Chinese text, deduplicates repeated subtitle lines, translates Chinese → Vietnamese, displays both lines (Chinese above in white, Vietnamese below in green) on a borderless always-on-top floating overlay, reads the Vietnamese translation aloud via TTS, and logs each `Chinese : Vietnamese` pair to disk.
- Translation and TTS run **natively in-process** (D1+A5, 2026-11-23): a selectable translation engine — Helsinki-NLP `opus-mt-zh-vi` ONNX model (lazy-loaded in the background) or the offline Vietphrase dictionary engine — and a native Rust Edge-TTS WebSocket client. No Python, no local HTTP server, no port. Dictionaries live in `%APPDATA%\cap2voice\vietphrase\data`, the model in `%APPDATA%\cap2voice\models\opus-mt-zh-vi`.

### End-to-End Pipeline

```text
Screen (any player / browser)
   │  1. Capture user-selected region (~10 fps via xcap)
   ▼
Pixel-Diff Gate
   │  2. Skip frame if pixel change < threshold
   ▼
wcocr-rs (internal Rust library → WeChat 4.x mmmojo IPC)
   │  3. OCR cropped frame → sort lines → keep CJK (+ digits/punctuation)
   ▼
Fuzzy Deduplicator (threshold 0.93 + blank-frame reset)
   │  4. Emit only when new Chinese line differs from previous line
   ▼
Native translation engines (in-process, §3.8)
   ├─ Helsinki-NLP opus-mt-zh-vi (ONNX, lazy-loaded) — selectable, default
   └─ Vietphrase (offline dictionaries) — selectable, fallback while Helsinki loads
Native Edge-TTS client (WebSocket, §3.8)
   └─ vi → MP3, default vi-VN-NamMinhNeural
   ▼
Floating egui Overlay (Chinese white + Vietnamese green)
   ├─ rodio Audio Player (non-overlapping bounded FIFO queue)
   └─ Subtitle Logger (`[timestamp] Chinese : Vietnamese` → UTF-8 log file)
```

### Capability & Module Map

Each capability maps to a single responsibility with one-way dependencies (no cycles). Interfaces live at the provider boundary.

| Module ID | Responsibility | Depends On |
|---|---|---|
| `config` | Typed settings model, TOML load/save, validation, Windows autostart registry helper | — |
| `ocr` | `wcocr-rs` pure Rust crate (WeChat 4.x mmmojo), path auto-detection, CJK filtering, line assembly | — |
| `capture` | Region coordinates (DPI/monitor-aware), screen capture (`xcap` only — no fallback backend), pixel-diff gate | `config` |
| `translate` | Selectable in-process engines: Helsinki-NLP `opus-mt-zh-vi` ONNX (lazy background load) + offline Vietphrase dictionaries; per-engine translation cache | `config` |
| `tts` | Native Rust Edge-TTS WebSocket client (no HTTP server, no Python) | `config` |
| `pipeline` | Capture loop orchestration, fuzzy deduplicator, `Translator` + `TtsEngine` trait clients, `rodio` FIFO audio player, UTF-8 subtitle logger | `config`, `capture`, `ocr`, `translate`, `tts` |
| `overlay-ui` | `eframe`/`egui` borderless always-on-top 2-line overlay, region-selector fullscreen overlay, CJK+Vietnamese font chain, visible error/status banner | `config`, `pipeline` |
| `settings-ui` | 5-tab Settings window (`Chung`, `Dịch`, `Giọng đọc`, `Hiển thị`, `Phím tắt`) with draft state (`Huỷ` / `Lưu thay đổi`) | `config`, `overlay-ui` |
| `tray` | Windows system tray icon, 5-item context menu, close-to-tray background mode, clean shutdown trigger | `overlay-ui`, `settings-ui`, `pipeline` |

**Build Order:** `config`, `ocr`, `translate`, `tts` → `capture` → `pipeline` → `overlay-ui` → `settings-ui`, `tray`.

---

## 2. Tech Stack

| Layer | Technology | Version / Notes |
|---|---|---|
| OS & Toolchain | Windows 10/11 x64 (`x86_64-pc-windows-msvc`), Rust 2021 edition | Verified locally: `rustc 1.93.1`, `cargo 1.93.1` |
| GUI & Overlay | `eframe` / `egui` | `eframe` `0.35` (pure Rust, no Tauri, no WebView) |
| Screen Capture | `xcap` | `xcap` `0.9.10` (only backend; measured on the real PC: p50 15.45 ms / p95 17.38 ms, well under the 30/50 ms budget — no fallback exists) |
| Local OCR | `wcocr-rs` (internal workspace crate, refactored from `C:\Users\doanv\wechat-ocr-rs`) | `libloading` `0.8`, `prost` `0.13`, `serde` `1`; WeChat 4.x `mmmojo_64.dll` + `wxocr.dll` (plugin 8022, Weixin `4.0.2.28` — user's PC, personal-use only) |
| Translation engines | Helsinki-NLP `opus-mt-zh-vi` via `ort` ONNX (`2.0.0-rc.15`) + `tokenizers`; offline Vietphrase rule engine (Rust port) | Model + dictionaries in `%APPDATA%\cap2voice` (not committed) |
| TTS | Native Edge-TTS WebSocket client (`tokio-tungstenite`) | Direct `wss://` to Microsoft Edge-TTS; no server, no Python |
| Audio Playback | `rodio` (MP3 decoder enabled) | `rodio` `0.23` |
| System Tray & Hotkeys | `tray-icon` (with `muda` menu), `global-hotkey` | Latest compatible stable versions on crates.io |
| Config / Logging / Errors | `serde`, `toml`, `chrono`, `thiserror`, `tracing`, `image`, `windows-sys` | Explicit typed errors; display affinity via `windows-sys` |

---

## 3. Feature Specifications & Acceptance Criteria

All user-facing UI labels are in Vietnamese (matching `AGENTS.md`, `brief.md`, and `docs/mockups/`). All acceptance criteria are verifiable via automated tests and/or live real-PC runs.

### 3.1 Screen Region Selection & Capture (`capture`)

Reference mockup: `docs/mockups/media-generation-region-select-*.webp`.

- **Region Selector UX:**
  - Triggered from the overlay (`Chọn vùng`), Settings (`Chọn lại vùng`), or hotkey.
  - Displays a fullscreen translucent dimmed overlay covering the active display (or virtual desktop across monitors) with a top-center pill prompt: `"Kéo chuột để chọn vùng phụ đề"` and secondary hint `"Nhấn Esc để huỷ"`.
  - Dragging with the left mouse button draws a dashed red rectangle (`#EF4444`) whose interior remains clear/undimmed so the user can align directly over the burned-in subtitle text.
  - A live dark badge near the selection corner displays physical pixel size and origin coordinates (e.g., `1280 × 60 px · 420, 1020`).
  - Releasing the mouse saves the region if width ≥ `20 px` and height ≥ `16 px`; smaller accidental clicks are rejected with a brief hint and do not overwrite the existing region.
  - Pressing `Esc` or right-clicking cancels selection and restores the previous saved region.
- **Coordinate & DPI Correctness:**
  - Stores monitor identifier plus physical pixel rectangle `(x, y, width, height)` in `config.toml` so capture remains accurate under Windows DPI scaling (`100%`, `125%`, `150%`, `175%`) and multi-monitor layouts.
- **Capture Loop & Pixel-Diff Gate:**
  - Captures only the selected monitor/region at the configured FPS (default `10` fps; selectable `5`, `10`, `15` fps) behind a `FrameCapturer` trait implemented with `xcap` `0.9.10`.
  - Computes a fast grayscale downsampled pixel-difference metric between the current frame and the last frame sent to OCR. If the changed-pixel ratio is below `pixel_diff_threshold` (default `0.02`, i.e. 2% of pixels changing by > 16 luminance levels), skips OCR completely.
  - When capture starts (`Bắt đầu`), the first captured frame always bypasses the pixel-diff gate so an already-visible subtitle is recognized immediately.

**Acceptance Criteria:**
- `AC-CAP-1`: Clicking `Chọn vùng` dims the screen, shows `"Kéo chuột để chọn vùng phụ đề"`, draws a red dashed rectangle with clear inner cut-out while dragging, and updates the live `<W> × <H> px · <X>, <Y>` badge.
- `AC-CAP-2`: Releasing a drag ≥ `20 × 16 px` persists the physical coordinates to config and displays them in Settings (`Vùng phụ đề`); pressing `Esc` cancels without modifying the previously saved region.
- `AC-CAP-3`: On a Windows display at `125%` or `150%` DPI scaling, a captured PNG of the selected region matches the visual pixels inside the drawn red box within ±2 px.
- `AC-CAP-4`: On a paused video frame (static pixels), pixel-diff skips ≥ 99% of frames after the initial frame (zero redundant OCR calls while static).
- `AC-CAP-5`: When the subtitle text changes on screen, pixel-diff triggers OCR on the next capture tick (≤ 150 ms at 10 fps).
- `AC-CAP-6`: Median cropped-frame capture latency on the target PC is ≤ 30 ms (p95 ≤ 50 ms). Measured on the real PC: p50 15.45 ms / p95 17.38 ms (Task 5). There is no fallback backend; if `xcap` ever exceeds budget, evaluating another capture library is a new engineering decision, not a swap to existing code.
- `AC-CAP-7`: If the saved monitor is disconnected or coordinates fall out of bounds, capture fails explicitly with a Vietnamese UI warning prompting the user to re-select the region (never panics or captures garbage).

---

### 3.2 OCR, Chinese Filtering, Deduplication & Logging (`ocr` + `pipeline`)

- **`wcocr-rs` Internal Crate Refactor:**
  - Copy/migrate `C:\Users\doanv\wechat-ocr-rs` into `crates/wcocr-rs` as a pure Rust internal library crate (`crate-type = ["rlib"]`).
  - Remove the C ABI compatibility layer (`wechat_ocr`, `stop_ocr`, `cdylib`, global `OnceLock` singleton) per project unification rules; keep `wcocr-cli` only if useful as a manual smoke binary or move into `examples/`.
  - Expose a clean typed Rust API:
    - `WeChatOcr::new(wxocr_dll: &Path, weixin_dir: &Path) -> Result<Self, OcrError>`
    - `WeChatOcr::wait_connection(&self, timeout: Duration) -> Result<(), OcrError>`
    - `WeChatOcr::ocr_file(&self, img_path: &Path, timeout: Duration) -> Result<OcrResult, OcrError>`
    - Typed `OcrError` enum (`InvalidPath`, `DllLoadFailed`, `StartFailed`, `HandshakeTimeout`, `NotSupported`, `RequestTimeout`, `SendFailed`, `InvalidImagePath`).
  - Enforce a per-request timeout (default `3000 ms`) on the `Condvar` wait so a stalled WeChat OCR worker returns `Err(OcrError::RequestTimeout)` instead of deadlocking the pipeline.
  - Because `mmmojo_64.dll` intentionally remains loaded for process lifetime, instantiate `WeChatOcr` lazily once when needed and shut down cleanly via `Drop` on application exit.
- **WeChat Path Discovery:**
  - Auto-detects the latest `wxocr.dll` under `%APPDATA%\Tencent\xwechat\XPlugin\plugins\WeChatOcr\*\extracted\wxocr.dll` (e.g. plugin `8022`) and latest Weixin 4.x folder under `C:\Program Files\Tencent\Weixin\*` (e.g. `4.0.2.28`), with manual overrides in Settings.
  - If either path is missing or invalid, surfaces a clear Vietnamese error message in the overlay and Settings — never fails silently.
- **Image Transfer:**
  - Baseline V1 writes the cropped frame as a PNG to a single reused temp file (`%TEMP%\cap2voice\frame.png`) before calling `ocr_file`. (See Assumption #8 regarding optional `pic_data` in-memory optimization.)
- **Text Sorting & Chinese Filtering:**
  - Sort OCR `TextBlock`s top-to-bottom (grouping lines whose vertical centers are within half a line height) and left-to-right within each line, then join into a single subtitle string.
  - Filter characters to retain CJK Unified Ideographs (`U+4E00..=U+9FFF`), CJK Extension A (`U+3400..=U+4DBF`), CJK symbols/punctuation (`U+3000..=U+303F`), fullwidth punctuation (`U+FF00..=U+FFEF`), ASCII digits (`0–9`), and standard punctuation (`.,!?;:'"-()`).
  - Discard OCR results containing **zero** CJK ideographs (ignores pure logos, timestamps, watermarks, or English-only UI elements).
- **Fuzzy Deduplication:**
  - Compare the filtered Chinese string (stripped of whitespace/punctuation for comparison) against the last emitted Chinese line using a `TextDeduplicator` trait.
  - Default V1 implementation: normalized Levenshtein similarity (`1.0 - edit_distance / max(len_a, len_b)`). If similarity ≥ `dedupe_threshold` (default `0.93`), skip translation/TTS/logging.
  - If `blank_reset_frames` (default `5` consecutive OCR frames with no CJK text) occur, clear the last-emitted deduplication memory so a repeated line later in the video is not suppressed.
- **Subtitle Pair Logging:**
  - For every translated subtitle pair, immediately append one UTF-8 line to `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log` (path configurable in Settings) and flush:
    ```text
    [2026-11-19 21:45:12] 夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi.
    ```

**Acceptance Criteria:**
- `AC-OCR-1`: `crates/wcocr-rs` compiles cleanly as an `rlib` with zero C ABI exports (`#[no_mangle] extern "C"` removed) and passes `cargo clippy --all-targets -- -D warnings`.
- `AC-OCR-2`: On the real PC with Weixin `4.0.2.28` + plugin `8022`, `WeChatOcr::ocr_file` recognizes Chinese text from fixture subtitle images (`test-zh-custom.png`) with `errcode == 0` and returns structured `OcrResult` blocks.
- `AC-OCR-3`: Missing/invalid `wxocr.dll` or Weixin directory returns `Err(OcrError::InvalidPath)` and displays an actionable Vietnamese error banner in the UI.
- `AC-OCR-4`: A mocked/simulated hung OCR worker returns `Err(OcrError::RequestTimeout)` within `timeout + 100 ms` without freezing the UI thread.
- `AC-OCR-5`: Unit tests prove CJK filtering keeps Chinese + numbers + punctuation, removes Latin noise (e.g. `"Bilibili 1080P 你好世界 2026"` → `"1080 你好世界 2026"` or strips isolated Latin tokens cleanly — specifically retains digits adjacent/within subtitle and drops Latin letters), and returns `None` when zero CJK characters exist.
- `AC-OCR-6`: Unit tests prove deduplicator skips pairs with similarity ≥ `0.93`, passes pairs below `0.93`, and resets memory after 5 consecutive blank/non-CJK frames.
- `AC-OCR-7`: Each translated subtitle writes exactly one `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` UTF-8 line to the daily log file, readable immediately without waiting for app shutdown.

---

### 3.3 Translation (`translate`)

- Rust defines an async `Translator` trait: `async fn translate(&self, zh: &str) -> Result<String, TranslateError>`.
- V1 implements the `Translator` trait with two selectable in-process engines (§3.8): Helsinki-NLP `opus-mt-zh-vi` (ONNX, lazy-loaded) and offline Vietphrase. The selected engine is stored in `config.toml` (`translation.engine = "helsinki" | "vietphrase"`, default Helsinki) and can be switched live in Settings → `Dịch` without restarting.
- Runs asynchronously off the UI thread. If a newer Chinese subtitle arrives while a previous translation is in flight, sequence numbers ensure stale responses never overwrite newer subtitles on screen.
- **Explicit Error Handling (non-negotiable):**
  - Network unreachable / DNS error / upstream `502`/`503`: show visible error banner `"Mất kết nối mạng — không thể dịch"`, keep displaying the latest Chinese OCR line, do **not** display fake text or reuse the previous Vietnamese translation under a new Chinese line (clear or mark Vietnamese line as `[Lỗi dịch — kiểm tra kết nối mạng]`).
  - Translation is offline-capable (Helsinki ONNX runs locally; Vietphrase dictionaries are local), so there is no upstream rate limit: no `429` is produced. The `TranslateError::RateLimited` variant and any rate-limit handling were removed in D1+A5 and must not be presented as a live path.
  - No engine available (model + dictionaries missing): show `"Không có engine dịch khả dụng — kiểm tra model/từ điển"` and block pipeline start; the overlay header shows the engine status (`Đang tải model dịch…` / `Lỗi engine dịch`).

**Acceptance Criteria:**
- `AC-TR-1`: Sending a real Chinese subtitle line (`"夜深了，雨还在下。"`) returns a non-empty Vietnamese translation displayed beneath the Chinese line within 1.5 s under normal broadband conditions.
- `AC-TR-2`: Automated tests verify the native engines' error mapping (invalid input, engine unavailable, timeout) to typed `TranslateError` variants, and the Helsinki/Vietphrase 25-sentence benchmark fixtures stay green.
- `AC-TR-3`: Disconnecting network on the real PC causes the overlay to display the explicit Vietnamese network-error banner within 1 translation attempt (never silently stuck on stale Vietnamese text).
- `AC-TR-4`: Out-of-order responses from slow requests (sequence `N` completing after sequence `N+1`) are discarded and never overwrite subtitle `N+1`.
- `AC-TR-5`: UI remains responsive (buttons clickable, overlay draggable) while a slow translation request is waiting.

---

### 3.4 Text-to-Speech & Audio Playback (`tts`)

- Rust defines an async `TtsEngine` trait: `async fn synthesize(&self, text: &str, voice: &str, rate: &str) -> Result<Vec<u8>, TtsError>`.
- V1 implements the `TtsEngine` trait with a native Rust Edge-TTS WebSocket client (`wss://api.msedgeservices.com/tts/cognitiveservices/websocket/v1`), receiving raw MP3 bytes. No HTTP server or Python is involved.
- Default voice: `vi-VN-NamMinhNeural`. Built-in selectable Vietnamese voices: `vi-VN-NamMinhNeural` (Nam — mặc định) and `vi-VN-HoaiMyNeural` (Nữ).
- Audio playback uses `rodio` `0.23` on a dedicated background audio worker thread:
  - **Non-overlapping FIFO queue:** One sentence plays at a time; incoming sentences wait in queue until the current sentence finishes.
  - **Bounded queue depth (max 3 pending items):** When the queue already has 3 unplayed items and a new item arrives, drop the **oldest unplayed** item (never interrupt the currently playing clip) so speech stays synchronized with fast dialogue.
  - **Volume & Mute:** Volume (`0.0..=1.0`, default `0.75`) adjusts `rodio::Sink` gain in real time. Toggling Mute or clicking `Dừng` immediately stops active playback and clears the pending queue.
  - **Auto-read toggle (`Tự động đọc phụ đề`):** When OFF (default ON), no TTS synthesis is requested and no audio plays; OCR, translation, overlay display, and logging continue normally.
  - **Failure visibility:** If TTS synthesis fails (network offline / Edge-TTS error) or audio output device fails to open, show a non-blocking warning in the overlay status bar while continuing to translate and display subtitles.

**Acceptance Criteria:**
- `AC-TTS-1`: With `Tự động đọc phụ đề` ON and unmuted, each newly translated Vietnamese subtitle is read aloud clearly using `vi-VN-NamMinhNeural` on the PC's default audio output.
- `AC-TTS-2`: Switching voice to `vi-VN-HoaiMyNeural` (in overlay dropdown or Settings) causes the very next subtitle to play in the female voice.
- `AC-TTS-3`: Rapidly feeding 5 subtitle lines plays them strictly sequentially without audio overlap; unit tests on the queue verify bounded capacity (max 3 pending) drops oldest unplayed items first.
- `AC-TTS-4`: Clicking `Dừng` or the overlay speaker mute button halts currently playing audio within 200 ms and empties queued items.
- `AC-TTS-5`: Adjusting the `Âm lượng` slider changes playback loudness linearly (`0%` = silent, `75%` default, `100%` max).
- `AC-TTS-6`: With `Tự động đọc phụ đề` OFF, zero TTS synthesis requests are issued while subtitles still translate and display.
- `AC-TTS-7`: When Edge-TTS fails (network error / timeout), a visible TTS warning appears in the overlay without crashing or halting translation.

---

### 3.5 Floating Subtitle Overlay (`overlay-ui`)

Reference mockup: `docs/mockups/media-generation-overlay-main-*.webp`.

- **Window Properties:**
  - Borderless (`with_decorations(false)`), always-on-top (`with_always_on_top()`), transparent window chrome with rounded dark semi-transparent panel (configurable opacity, default `0.85`).
  - Draggable anywhere on empty panel background to reposition freely; last `(x, y)` window position persisted in `config.toml`.
  - **Capture self-exclusion:** Apply Windows `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` on Windows 10 2004+ so even if the user drags the overlay over the capture region, `xcap` captures the underlying video rather than the overlay. Also warn if the API call fails and bounding boxes overlap.
- **Typography & Font Chain:**
  - Register a composite `egui::FontDefinitions` fallback chain using Windows system fonts so both Simplified/Traditional Chinese (`Microsoft YaHei` / `SimSun`) and full Vietnamese diacritics (`Segoe UI` / `Arial`) render cleanly with zero missing-glyph boxes (`□`).
  - Top line: Chinese original text in white (`#FFFFFF`), size = `base_font_size × 1.35` (default ~24 px).
  - Bottom line: Vietnamese translation in green (`#4ADE80`, configurable), size = `base_font_size` (default `18 px`).
  - Both lines wrap cleanly inside the panel width without clipping.
- **Controls (matching Mockup 1 & `brief.md` §6):**
  - Compact top header strip (or hoverable corner controls): app status indicator dot + status text (`Đang chờ` / `Đang dịch` / error message), Settings gear button (`Cài đặt`), and Close button (`X` — hides to tray when close-to-tray is enabled).
  - Bottom action bar:
    - `Chọn vùng` button
    - `Bắt đầu` button (disabled while running; if clicked with no saved region, prompts user to select a region first or opens region selector)
    - `Dừng` button (disabled while idle)
    - Speaker mute/unmute icon button
    - Voice selector dropdown (`vi-VN-NamMinhNeural` / `vi-VN-HoaiMyNeural`)

**Acceptance Criteria:**
- `AC-UI-1`: Overlay floats borderless and always-on-top over both a local media player (e.g. VLC/MPV) and a maximized browser video (e.g. Chrome/Edge on Bilibili/YouTube).
- `AC-UI-2`: Chinese line renders in white above Vietnamese line in green; verified visually via real-PC screenshot with mixed Chinese + Vietnamese diacritics (e.g. `"夜深了，雨还在下。"` / `"Đêm đã khuya, mưa vẫn còn rơi."`) showing zero missing-glyph tofu boxes.
- `AC-UI-3`: Dragging the overlay moves it smoothly and persists its coordinates across app restarts.
- `AC-UI-4`: All 5 control-bar elements (`Chọn vùng`, `Bắt đầu`, `Dừng`, speaker mute button, voice dropdown) plus `Cài đặt` and `X` work as specified and reflect live running/muted/voice state.
- `AC-UI-5`: Network, engine, or OCR errors render visibly inside the overlay status banner in Vietnamese and clear automatically once a subsequent request succeeds.
- `AC-UI-6`: Placing the overlay partially over the captured subtitle region does not cause an OCR feedback loop (verified via `WDA_EXCLUDEFROMCAPTURE` on Windows 10/11).

---

### 3.6 Settings Window & Configuration Persistence (`settings-ui` + `config`)

Reference mockup: `docs/mockups/media-generation-settings-tray-*.webp`.

- **Window Layout:**
  - Standard decorated window titled `Cài đặt` (opened via overlay gear button or tray `Cài đặt`), dark theme matching Mockup 3.
  - Left navigation sidebar with 5 sections (exact labels from `AGENTS.md` and Mockup 3):
    1. `Chung` (General):
       - `Tốc độ chụp` dropdown (`5 khung/giây`, `10 khung/giây` default, `15 khung/giây`)
       - `Vùng phụ đề` summary (`<x>, <y> · <w> × <h> px` or `"Chưa chọn vùng"`) + `Chọn lại vùng` button
       - WeChat OCR paths (`wxocr.dll` & Weixin directory — auto-detected status + editable path inputs)
       - Subtitle log directory + enable logging toggle (default ON)
       - Checkbox: `Ẩn xuống khay hệ thống khi đóng cửa sổ` (default checked)
       - Checkbox: `Khởi động cùng Windows` (default unchecked)
       - Checkbox: `Khởi động ở chế độ ẩn xuống khay` (default unchecked)
    2. `Dịch` (Translation):
       - Engine selector (`Helsinki (mô hình AI)` / `Vietphrase (từ điển)`, default Helsinki) + live engine status line; switching applies immediately without restart
       - Fuzzy deduplication threshold slider (`0.80`–`0.99`, default `0.93`)
       - Pixel-diff sensitivity threshold slider (`0.5%`–`10.0%`, default `2.0%`)
    3. `Giọng đọc` (Voice / TTS):
       - `Giọng đọc` dropdown (`vi-VN-NamMinhNeural` default, `vi-VN-HoaiMyNeural`)
       - `Âm lượng` slider (`0%`–`100%`, default `75%`)
       - Speech rate selector (`-20%`, `-10%`, `0% (Mặc định)`, `+10%`, `+20%`)
       - Toggle: `Tự động đọc phụ đề` (default ON)
       - `Nghe thử` button (plays a short Vietnamese sample sentence via native Edge-TTS)
    4. `Hiển thị` (Display):
       - `Cỡ chữ phụ đề` slider (`12 px`–`40 px`, default `18 px`)
       - Overlay background opacity slider (`30%`–`100%`, default `85%`)
       - Toggles: show Chinese line (default ON), show Vietnamese line (default ON), always-on-top (default ON)
       - Text color pickers/presets (defaults: Chinese `#FFFFFF`, Vietnamese `#4ADE80`)
    5. `Phím tắt` (Hotkeys):
       - Configurable global shortcuts with defaults:
         - Start / Stop translation: `Ctrl+Alt+S`
         - Select region: `Ctrl+Alt+R`
         - Show / Hide overlay: `Ctrl+Alt+H`
         - Mute / Unmute TTS: `Ctrl+Alt+M`
       - `Khôi phục mặc định` button to reset hotkeys.
  - Footer buttons: `Huỷ` (discards unsaved draft edits and closes Settings) and `Lưu thay đổi` (validates, saves atomically to `%APPDATA%\cap2voice\config.toml`, applies live to running app).
- **Persistence & Safety:**
  - Config stored at `%APPDATA%\cap2voice\config.toml` via atomic temp-file + rename write.
  - On corrupt/invalid TOML at startup: report the parse error visibly in UI, preserve the broken file by copying to `config.toml.bak`, and load explicit documented defaults only after alerting the user — never silently wipe user config.
  - `Khởi động cùng Windows`: adds/removes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice` pointing to the current executable only when explicitly toggled and saved.

**Acceptance Criteria:**
- `AC-SET-1`: All 5 sidebar tabs (`Chung`, `Dịch`, `Giọng đọc`, `Hiển thị`, `Phím tắt`) open cleanly with functional controls and zero placeholder/stub widgets.
- `AC-SET-2`: Editing settings and clicking `Huỷ` reverts all draft changes without modifying `config.toml` or active overlay behavior.
- `AC-SET-3`: Editing settings and clicking `Lưu thay đổi` updates `config.toml` atomically, applies immediately to the live overlay/pipeline (font size, volume, voice, FPS, opacity), and persists across app restarts.
- `AC-SET-4`: Clicking `Chọn lại vùng` inside Settings launches the region selector and updates the displayed coordinates in the Settings draft.
- `AC-SET-5`: Enabling `Khởi động cùng Windows` + `Lưu thay đổi` creates the `HKCU\...\Run\cap2voice` entry; unchecking + saving removes it cleanly.
- `AC-SET-6`: Corrupting `config.toml` triggers a visible Vietnamese warning and creates `config.toml.bak` rather than silently overwriting or crashing.
- `AC-SET-7`: Registered global hotkeys trigger Start/Stop, Region Select, Show/Hide, and Mute even when focus is inside a video player or browser.

---

### 3.7 System Tray & Background Operation (`tray`)

Reference mockup: `docs/mockups/media-generation-settings-tray-*.webp`.

- Creates a Windows notification area icon on startup using `tray-icon` with tooltip `cap2voice`.
- Right-click context menu contains the exact 5 items (per `AGENTS.md`, `brief.md` §6, Mockup 3):
  1. `Hiện cửa sổ` — shows and focuses the overlay window.
  2. `Bắt đầu dịch` — starts the capture/OCR/translate/TTS pipeline (disabled if already running).
  3. `Dừng` — stops the pipeline and clears TTS queue (disabled if idle).
  4. `Cài đặt` — opens/focuses the Settings window.
  5. `Thoát` — stops pipeline, destroys tray icon, and exits the application cleanly.
- Left double-click (or single-click) on tray icon restores and brings the overlay to the foreground.
- **Close-to-tray behavior:** When `Ẩn xuống khay hệ thống khi đóng cửa sổ` is checked (default), clicking `X` on the overlay hides the window without stopping translation or TTS playback. When unchecked, clicking `X` performs a full graceful exit identical to `Thoát`.

**Acceptance Criteria:**
- `AC-TRAY-1`: Launching `cap2voice` displays the tray icon in the Windows notification area; right-clicking shows all 5 exact Vietnamese menu items (`Hiện cửa sổ`, `Bắt đầu dịch`, `Dừng`, `Cài đặt`, `Thoát`).
- `AC-TRAY-2`: With translation active and close-to-tray checked, clicking `X` on the overlay hides the window while OCR, translation, TTS audio, and subtitle logging continue uninterrupted in the background.
- `AC-TRAY-3`: Selecting `Hiện cửa sổ` (or clicking the tray icon) restores the overlay displaying the latest translated subtitle.
- `AC-TRAY-4`: `Bắt đầu dịch`, `Dừng`, and `Cài đặt` from the tray menu execute their respective actions while the overlay is hidden.
- `AC-TRAY-5`: Selecting `Thoát` stops playback, removes the tray icon with no ghost icon left behind, and exits cleanly with code `0`.

---

### 3.8 Native Translation & TTS Engines (`translate` + `tts`) — D1+A5 (2026-11-23)

The temporary Python server (`server/`, `AC-SRV-1..6`) was **removed in D1+A5**. Translation and TTS are now native in-process Rust; the app needs no Python at runtime and opens no local port. The `AC-SRV-*` criteria are superseded and kept only as a record of the removed design.

- **Translation — selectable engine (`SwitchableTranslator`, `translation.engine` in `config.toml`, default `helsinki`):**
  - **Helsinki-NLP `opus-mt-zh-vi`** (ONNX, `ort` `2.0.0-rc.15`, greedy decoding, max 64 new tokens): model files in `%APPDATA%\cap2voice\models\opus-mt-zh-vi` (override `CAP2VOICE_HELSINKI_MODEL_DIR`). Loads lazily in a background thread after startup so the overlay is interactive before the ~3 s load finishes; the overlay header shows `Đang tải model dịch…` meanwhile.
  - **Vietphrase** (Rust port of the dictionary rule engine): dictionaries in `%APPDATA%\cap2voice\vietphrase\data` (override `CAP2VOICE_VP_DATA_DIR`). Offline, correct on proper names.
  - **Fallback:** while the Helsinki model is still loading, sentences translate via Vietphrase; each fallback is counted (`helsinki_fallbacks`) and shown in Settings → `Dịch`. Switching engines in Settings applies live, without restart.
  - **Cache:** per-engine cache keys (`helsinki|…` / `vietphrase|…`) so the same sentence cached under one engine is never served as the other engine's output.
- **TTS — native Edge-TTS WebSocket client (`EdgeTtsEngine`):** direct `wss://` connection to Microsoft Edge-TTS, SSML chunks ≤ 4096 bytes, MP3 bytes returned. Default voice `vi-VN-NamMinhNeural`; Chinese click-to-replay uses `zh-CN-XiaoxiaoNeural`. No API key; network failure surfaces as a visible non-blocking warning, never a silent stall.

**Acceptance Criteria (replacing `AC-SRV-1..6`):**
- `AC-NAT-1`: With no Python on `PATH` and `server/` deleted, the app builds, starts, translates, and speaks normally.
- `AC-NAT-2`: Switching the engine in Settings → `Dịch` changes the translation of the same Chinese line on the next subtitle (Helsinki vs Vietphrase outputs differ as benchmarked), without restart.
- `AC-NAT-3`: With Helsinki selected, the overlay is usable (region selectable, pipeline startable) before the model finishes loading; the header shows the loading status and translations fall back to Vietphrase meanwhile.
- `AC-NAT-4`: Exiting via tray `Thoát` — AND force-killing `cap2voice-app.exe` via `Stop-Process -Force` — leaves zero lingering `cap2voice-app.exe` processes.
- ~~`AC-SRV-1`–`AC-SRV-6`~~ — superseded (Python server removed 2026-11-23).

---

## 4. Commands

> **Windows SSH Note (verified on target PC):** `cargo`/`rustc` shims in `%USERPROFILE%\.cargo\bin` can fail under SSH sessions. Prepend the concrete toolchain directory when running over SSH:
> ```powershell
> $env:PATH = "C:\Users\doanv\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;$env:PATH"
> ```

### Rust Workspace
```powershell
# Format check
cargo fmt --all -- --check

# Lint (warnings are errors)
cargo clippy --workspace --all-targets -- -D warnings

# Run all unit & integration tests
cargo test --workspace

# Build debug / release
cargo build --workspace
cargo build --workspace --release

# Run desktop application (dev)
cargo run -p cap2voice-app
```


## 5. Project Structure

```text
cap2voice/
├── AGENTS.md                       # Agent rules & workflow contract
├── brief.md                        # Project vision & settled decisions
├── CONSTRAINTS.md                  # Quality bar (created in Constraints phase)
├── Cargo.toml                      # Rust workspace manifest
├── Cargo.lock                      # Locked dependency versions
├── docs/
│   ├── spec.md                     # This specification (V1 MVP)
│   └── mockups/                    # Approved UI reference images (3 screens)
├── tasks/
│   ├── plan.md                     # Implementation plan (created after spec approval)
│   └── todo.md                     # Ordered task checklist
├── crates/
│   ├── wcocr-rs/                   # Internal Rust library crate for WeChat 4.x OCR
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs              # Public Rust API & OcrError (no C ABI)
│   │       ├── mojo.rs             # Runtime mmmojo_64.dll FFI & lifecycle
│   │       ├── ocr.rs              # WeChatOcr session, handshake, timed OCR calls
│   │       ├── proto.rs            # Hand-written prost structs (ocr_common, wx4)
│   │       └── util.rs             # UTF-16 & Windows path helpers
│   └── cap2voice-app/              # Windows desktop app
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs             # Entrypoint, logging init, eframe bootstrap
│           ├── config.rs           # AppConfig, TOML persistence, autostart registry
│           ├── capture/
│           │   ├── mod.rs          # FrameCapturer trait, region rect, DPI helpers
│           │   ├── xcap_impl.rs    # xcap 0.9.10 capturer
│           │   └── pixel_diff.rs   # Pre-OCR pixel change detector
│           ├── ocr/
│           │   ├── mod.rs          # OcrEngine trait, WeChat path auto-discovery
│           │   └── filter.rs       # Line sorting, CJK filtering, fuzzy deduplicator
│           ├── api/
│           │   └── mod.rs          # Translator & TtsEngine traits + error types
│           ├── translate/          # SwitchableTranslator: Helsinki ONNX + Vietphrase
│           └── tts/                # Native Edge-TTS WebSocket client
│           ├── audio/
│           │   └── mod.rs          # rodio 0.23 playback worker & bounded FIFO queue
│           ├── pipeline/
│           │   ├── mod.rs          # End-to-end async worker, sequence guard, state bus
│           │   └── logger.rs       # Daily UTF-8 `Chinese : Vietnamese` log writer
│           └── ui/
│               ├── mod.rs          # Root eframe::App, viewport routing, fonts
│               ├── overlay.rs      # Floating 2-line subtitle overlay + WDA affinity
│               ├── region.rs       # Fullscreen red-dashed region selector
│               ├── settings.rs     # 5-tab Settings window (draft + Save/Cancel)
│               ├── tray.rs         # Windows tray icon & 5-item context menu
│               └── hotkeys.rs      # Global hotkey registration & dispatch
```

### Runtime User Data Paths (Windows)
- Configuration: `%APPDATA%\cap2voice\config.toml`
- Subtitle logs: `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log`
- Temporary OCR frame buffer: `%TEMP%\cap2voice\frame.png`
- Helsinki ONNX model: `%APPDATA%\cap2voice\models\opus-mt-zh-vi` (override `CAP2VOICE_HELSINKI_MODEL_DIR`)
- Vietphrase dictionaries: `%APPDATA%\cap2voice\vietphrase\data` (override `CAP2VOICE_VP_DATA_DIR`)

---

## 6. Code Style

### Conventions
- **Languages:** All source code, identifiers, comments, commit messages, and technical docs are in **English**. User-facing UI strings are in **Vietnamese** and centralized as constants.
- **Zero silent fallbacks / zero `unwrap()` in runtime paths:** Return typed `Result<T, E>` using `thiserror`; surface every network, OCR, config, or audio failure explicitly to the UI status state and structured `tracing` logs.
- **Strict typing:** Explicit types across Rust (no untyped `dict` payloads or `Any` anywhere).
- **Modular boundaries:** Keep UI rendering (`ui/`), capture/OCR/network workers (`pipeline/`, `api/`), and config (`config.rs`) strictly decoupled through channels and traits so components remain unit-testable without a GUI or network.
- **Formatting & Linting:** `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` must pass cleanly on every commit.

### Representative Rust Example
```rust
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TranslateError {
    #[error("input text is empty")]
    EmptyInput,
    #[error("no translation engine available: {0}")]
    EngineUnavailable(String),
    #[error("network or upstream translation failure: {0}")]
    Upstream(String),
}

pub trait Translator: Send + Sync {
    fn translate<'a>(
        &'a self,
        chinese: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, TranslateError>> + Send + 'a>>;
}

/// Returns normalized similarity in `[0.0, 1.0]` for Chinese subtitle deduplication.
pub fn normalized_similarity(a: &str, b: &str) -> f32 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein(&a_chars, &b_chars);
    1.0 - (dist as f32 / max_len as f32)
}
```

---

## 7. Testing Strategy

Testing follows `AGENTS.md` and TDD (RED → GREEN → REFACTOR): every functional change includes unit/integration tests asserting exact values (no "self-answering" mocks that return X and assert X without executing business logic), and every bug fix adds a regression test named after the bug scenario.

| Level | Tooling & Location | Scope | Runs On |
|---|---|---|---|
| **Rust Unit Tests** | `cargo test --workspace` (`#[cfg(test)]` modules & `tests/`) | Pixel-diff gate, CJK character filter, OCR line ordering, fuzzy deduplicator (`0.93` threshold + blank reset), log formatter, TOML config round-trip & corrupt-file backup, bounded TTS FIFO queue, stale-sequence dropping, native Helsinki/Vietphrase + Edge-TTS error mapping, 25-sentence benchmark fixtures | VM & Real PC |
| **Real-PC Integration Tests** | `cargo test --workspace -- --ignored` (hardware/WeChat-tagged tests) | `wcocr-rs` direct Rust OCR on Chinese subtitle PNG fixtures, `xcap` cropped capture latency benchmark (100 frames p50/p95), native Helsinki/Vietphrase + Edge-TTS live tests, forced-kill orphan check | Real PC (mandatory) |
| **Real-Video E2E Verification** | Live run on Danh's Windows PC + recorded test log + screenshots | Real Chinese drama / Bilibili video with hardcoded subtitles (both local player & browser), verifying every `AC-*` item, DPI alignment, Vietnamese/CJK font rendering, audio non-overlap, close-to-tray background run, and explicit network-offline error reporting | Real PC (mandatory before ticket sign-off) |

### Build-Time Risk Register (from `brief.md` §9)

| Risk | Verification Check | Mitigation / Contingency |
|---|---|---|
| `xcap` capture latency on real PC | Benchmark 100 cropped captures on target display | Measured p50 15.45 ms / p95 17.38 ms (Task 5), well under budget; no fallback backend exists |
| Vietphrase offline translation quality on short spoken subtitles | 15+ min real drama test + inspect log pairs | Deduplication (`0.93`) + pixel-diff minimize request volume; evaluate DeepL in V2 |
| `wcocr-rs` temp-file I/O vs in-memory `pic_data` | Measure OCR latency over 50 consecutive subtitle changes | Start with single reused `%TEMP%\cap2voice\frame.png`; test `pic_data` if disk I/O is measurable |
| `tray-icon` + `eframe` 0.35 close-to-tray event loop on Windows | Live test clicking `X` during active playback + restoring via tray | Intercept `CloseRequested` via `ViewportCommand::CancelClose` + `Visible(false)`; wake event loop on tray messages |
| Windows DPI scaling (`125%` / `150%`) & multi-monitor region offset | Draw box on scaled display, save captured frame, compare visually | Normalize logical egui points ↔ physical monitor pixels via monitor scale factor |
| CJK + Vietnamese font rendering in `egui` | Live screenshot with Traditional/Simplified Chinese + full Vietnamese diacritics | Load Windows system fonts (`Segoe UI` + `Microsoft YaHei`) into `egui::FontDefinitions` |

---

## 8. Boundaries

### Always
- Follow the spec-first lifecycle (`SPEC → CONSTRAINTS → PLAN → BUILD → VERIFY → REVIEW → SHIP`).
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` before marking any slice complete.
- Write tests alongside every feature change and regression tests for every bug fix, asserting concrete values.
- Report all network, engine, OCR, audio, and config failures explicitly in the UI and structured logs.
- Verify UI and pipeline behavior on the real Windows PC with real hardcoded-subtitle video, saving actual output and screenshots to the test log.
- Keep commits atomic (one vertical slice per commit, English conventional messages: `feat:`, `fix:`, `test:`, `docs:`).

### Ask First
- Any Git state-modifying operation (`commit`, `push`, `pull`, `merge`, `rebase`, `reset`, branch creation/deletion, tagging) or publish/deploy/delete action — requires an explicit "go" for that exact action.
- Modifying existing original files outside newly created project artifacts.
- Adding new major crates/dependencies (including any new capture backend — there is no `windows-capture` fallback to swap to).
- Changing the `Translator`/`TtsEngine` trait contracts, `config.toml` schema, or `CONSTRAINTS.md` quality thresholds.

### Never
- Bundle, redistribute, or commit WeChat OCR binaries/DLLs (`wxocr.dll`, `mmmojo_64.dll`, `Weixin.exe`). WeChat OCR is strictly personal-use via the user's existing local Weixin installation.
- Commit secrets, API keys, tokens, or credentials.
- Silently swallow network/OCR/TTS errors, fabricate placeholder translations, or add hidden fallback chains that mask failures.
- Implement out-of-MVP (V2+) features during V1 build cycles.
- Leave orphaned processes or ghost tray icons after shutdown or crash.
- Touch video files, inject scripts into browsers, or disable/delete failing tests to make CI green.

---

## 9. Success Criteria (V1 Release Gate)

V1 is ready for user acceptance and packaging when **all** conditions below pass with recorded evidence:

1. **Automated Quality Gate:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` all exit with code `0` and zero skipped tests.
2. **Real-Video End-to-End Run (Local + Browser):** Running against a real hardcoded-Chinese-subtitle video in both a local player and a web browser for ≥ 15 continuous minutes:
   - End-to-end latency from subtitle appearance on screen to Vietnamese text appearing on the overlay is ≤ 2.0 s median under normal broadband.
   - Zero crashes, zero UI freezes, zero overlapping TTS audio clips, and zero redundant OCR calls during paused frames.
3. **Visual & UX Fidelity:** Screenshots on the real PC confirm all 3 mockup views (`overlay-main`, `region-select`, `settings-tray`) match approved layout, colors (Chinese white, Vietnamese green), CJK + Vietnamese typography, and exact Vietnamese labels.
4. **Tray & Lifecycle Integrity:** Closing the overlay hides it to tray while background translation + TTS continue; `Hiện cửa sổ` restores it; `Thoát` and forced kill both leave zero lingering `cap2voice-app.exe` processes.
5. **Failure Honesty:** Disconnecting internet mid-playback surfaces the clear Vietnamese network-error banner within 1 subtitle cycle and recovers cleanly once connectivity returns; missing WeChat OCR path surfaces a clear setup error.
   > **Note (2026-11-21, Danh):** the network-disconnect check in this item is skipped for V1 ("task 14 không cần ngắt mạng"). `AC-TR-3` and the offline part of `AC-TTS-7` are recorded as skipped in `docs/test-log-v1.md`, not as verified.
6. **Log Integrity:** The generated `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log` file contains accurate timestamped `Chinese : Vietnamese` pairs with zero mojibake.

---

## 10. Out of Scope (Deferred — V2+ Backlog)

The following items from `brief.md` §8 and `AGENTS.md` are explicitly excluded from V1 and must not be implemented in this cycle:

~~- **Local offline translation model (`Helsinki-NLP/opus-mt-zh-vi`, ~300 MB):** Deferred to V2 to avoid shipping heavy ML inference runtimes in V1.~~ — **DONE in Phase 2 (D1+A5, 2026-11-23):** selectable Helsinki engine, ONNX via `ort`.
- **DeepL API translation backend:** Dropped by Danh 2026-11-21 (A4 cancelled); not planned.
~~- **Native Rust Edge-TTS (direct WebSocket protocol) & Python server removal:** Deferred to V2; V1 isolates the Python server behind `Translator`/`TtsEngine` traits for a clean drop-in swap later.~~ — **DONE in Phase 2 (D1+A5, 2026-11-23):** native Edge-TTS client, Python server deleted.
~~- **Pinyin display above the Chinese line & click-to-replay Chinese audio:** Deferred to V2 language-learning enhancements.~~ — **DONE in Phase 2 (D2, 2026-11-22):** pinyin line + click-to-replay.
- **Searchable translation history table UI:** Deferred to V2 (V1 provides UTF-8 log files on disk).
- **Browser extension (direct frame capture):** Deferred to V2+ (V1 is screen-observation only).
- **WeChat 3.x OCR support (`WeChatOCR.exe`, `wx3` protos):** Deferred (`wcocr-rs` supports WeChat 4.x `wxocr.dll` only).

---

## 11. Open Questions / Assumptions

Per instructions, reasonable defaults were chosen for all underspecified details so this specification is complete and reviewable. Please confirm or override any item before Phase 2 (Constraints & Planning):

1. **Fuzzy Deduplication Algorithm (`0.93` threshold):** `brief.md` references stroke-based fuzzy matching (`0.93`) from `pijuskri/bilibili-caption-project`. **Assumption:** V1 uses normalized character-level Levenshtein similarity (`>= 0.93`) behind a `TextDeduplicator` trait (zero external stroke-table bloat, fast and deterministic). If visually similar WeChat OCR confusions (e.g. `门` vs `内`) trigger noticeable duplicate translations on real videos, we can swap in stroke-based distance without changing callers.
2. **TTS Queue Overflow Policy:** `brief.md` specifies queued non-overlapping playback. **Assumption:** Bounded FIFO queue with max **3** pending unplayed items; when full, the oldest *unplayed* item is dropped (never cutting off the currently spoken sentence) so speech cannot drift minutes behind rapid dialogue.
3. **Blank-Screen / Subtitle Disappearance Behavior:** **Assumption:** After 5 consecutive non-CJK/empty OCR frames (~0.5 s at 10 fps), the deduplicator resets its last-line memory (so a repeated phrase later in the scene triggers again), while the overlay retains the last translated pair on screen until the next subtitle arrives (rather than flashing empty).
4. **Font Size Hierarchy (`Cỡ chữ phụ đề`):** Mockup 1 shows larger Chinese text over smaller Vietnamese text, while Mockup 3 shows one `Cỡ chữ phụ đề` slider. **Assumption:** The slider sets the Vietnamese base size (`12–40 px`, default `18 px`) and the Chinese line scales automatically at `1.35×` (default ~`24 px`).
5. **Settings Control Placement Across 5 Tabs:** Mockup 3 renders several key controls together for visual reference alongside the 5-tab sidebar (`Chung`, `Dịch`, `Giọng đọc`, `Hiển thị`, `Phím tắt`). **Assumption:** Controls are organized logically across all 5 tabs as detailed in §3.6 so no tab is an empty stub.
6. **Default Global Hotkeys:** Not specified in `brief.md`. **Assumption:** Defaults are `Ctrl+Alt+S` (Start/Stop), `Ctrl+Alt+R` (Select Region), `Ctrl+Alt+H` (Show/Hide Overlay), and `Ctrl+Alt+M` (Mute/Unmute TTS), all editable/disableable in Settings → `Phím tắt`.
7. ~~**Python Environment & Local Server Port**~~ — **Superseded by D1+A5 (2026-11-23):** no Python, no local port. Model/dictionary locations are `%APPDATA%\cap2voice\models\opus-mt-zh-vi` and `%APPDATA%\cap2voice\vietphrase\data` (env overrides `CAP2VOICE_HELSINKI_MODEL_DIR` / `CAP2VOICE_VP_DATA_DIR`).
8. **WeChat OCR Image Input (`pic_path` vs `pic_data`):** Inspected `wcocr-rs/src/ocr.rs` passes `pic_path` (proven on WeChat 4.x) while `pic_data` in `wx4::ParseOcrReqMessage` is untested. **Assumption:** V1 uses a single reused temp file (`%TEMP%\cap2voice\frame.png`) as the proven path and optionally benchmarks `pic_data` in-memory during OCR implementation.
9. **Subtitle Log Path & Format:** **Assumption:** Daily UTF-8 append logs at `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log` with format `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>`, flushed per line, configurable directory in Settings → `Chung`.
10. **Offline Vietphrase Translation:** **Assumption (updated 2026-11-23, D1+A5):** Vietphrase is a Rust port of the GPL-3.0 `duongden/script-vietphrase-translator` userscript, running in-process with dictionaries in `%APPDATA%\cap2voice\vietphrase\data` (outside git). It replaced the Google Translate free endpoint on 2026-11-20 because Google returned HTTP `429` for ~2 days, and the Python server that hosted it was deleted in D1+A5. There is no upstream network call and no rate limit; when the dictionaries are missing/unloadable the Vietphrase engine is simply unavailable (Helsinki still works).
11. **Overlay Self-Capture Exclusion:** Dragging the floating overlay over the selected subtitle box would create an OCR feedback loop if captured. **Assumption:** V1 applies Windows `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` (Windows 10 2004+) to exclude the overlay from screen capture automatically, plus warns if the call is unsupported and regions overlap.
12. **Start With Windows Registry Scope:** **Assumption:** `Khởi động cùng Windows` defaults to **unchecked** and, when enabled by the user, writes only to per-user `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\cap2voice` (requires no UAC admin elevation).
