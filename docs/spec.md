# Spec: cap2voice — V1 (MVP)

> **Status:** Approved — 2026-11-19 (Danh via delegation to Muse; all 12 assumptions in §11 confirmed as-is)
> **D1+A5 amendment — 2026-11-23:** The temporary Python server is **gone**. Translation and TTS are now native in-process Rust (§3.8 rewritten): a selectable engine (Helsinki-NLP `opus-mt-zh-vi` ONNX, lazy-loaded, or offline Vietphrase) and a native Edge-TTS WebSocket client. §3.8's `AC-SRV-1..5`, the `429`/`RateLimited` path, and the server-port/`GET /health` UI are superseded; `AC-TR-2`'s mock-HTTP-server wording is superseded by native-engine error tests. The rest of this spec stands.
> **Piper amendment — 2026-11-24:** Piper TTS (voice Ngọc Huyền) is a second, selectable TTS engine beside Edge-TTS (§3.4, §3.6, §3.8): on-demand download in Settings → `Giọng đọc`, saved under `%APPDATA%\cap2voice\voices\piper\`, never bundled into the binary or repo. Edge-TTS stays the default. **Voice selection lives ONLY in Settings from now on** — the voice dropdown on the main overlay window is removed (§3.5, `AC-UI-4`).
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
  - Displays a fullscreen window covering the active display (or virtual desktop across monitors) with a top-center pill prompt: `"Kéo chuột để chọn vùng phụ đề"` and secondary hint `"Nhấn Esc để huỷ"`. Nothing is dimmed: the desktop/video underneath stays fully visible so the user can align the selection exactly over the burned-in subtitle text. Transparency mechanism (option B, 2026-11-24): the window itself is NOT transparent on this PC — eframe/glow cannot create a transparent window here ("Cannot create transparent window: the GL config does not support it"; the WGL pixel format has no alpha on the AMD Radeon, and winit 0.30 uses DWM blur-behind, not `WS_EX_LAYERED`). Instead, when the selector opens it captures a FROZEN fullscreen snapshot of the active monitor with the existing `xcap` backend and paints that snapshot as the window background, so the desktop/video appears fully visible. The snapshot is frozen at open time, not a live feed (intended design); it is re-taken when the user switches monitors with Tab. The selector window is excluded from screen capture (`WDA_EXCLUDEFROMCAPTURE`) so it never appears in the app's own captures. If the snapshot capture fails, the selector still opens with no background (opaque black) and a notice is shown after selection.
  - Dragging with the left mouse button draws a dashed red rectangle (`#EF4444`); only the rectangle border, the live coordinate badge, and the top prompt pill are painted — everything else shows the frozen snapshot background (visually the desktop/video underneath).
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
- `AC-CAP-1`: Clicking `Chọn vùng` opens a fullscreen window whose background shows the desktop/video underneath as a frozen snapshot taken at open time (no dimming — the video/desktop stays visible; the window itself is not transparent on this PC, see §3.1 option B), shows `"Kéo chuột để chọn vùng phụ đề"`, draws a red dashed rectangle while dragging, and updates the live `<W> × <H> px · <X>, <Y>` badge.
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
- **Sentence buffering (A2 — merge fix 2026-11-25):**
  - The translation unit is a *sentence*, not an OCR frame. Stable lines from the hold pass through THREE layers before translation:
    - *Layer 0 — geometric speaker split* (`extract_chinese_subtitle_lines_with_decisions`, `ocr/filter.rs`, applied before the speaker layer): the OCR engine returns each `TextBlock` with its box (`left/top/right/bottom`) and per-character boxes (`CharDetail`); geometry decides before characters do. Within one visual row, blocks whose horizontal gap exceeds `0.5x` the row's median character width stay separate lines, while blocks that nearly abut are joined into one line (one sentence the engine happened to return as two boxes). A single block is further split at large gaps between its character boxes. Median character width is measured from the CJK character boxes when present, else block width / CJK count, else block height (CJK glyphs are square). Threshold `SPEAKER_GAP_CHAR_WIDTHS = 0.5`, calibrated 2026-11-25 on the real WeChat OCR engine (probe image, 36 px font, 960x540): normal inter-character gaps 0-5 px (0-0.16x), a same-sentence 12 px gap (0.33x) stayed one block, a 26 px block gap (0.79x) and an 82 px gap (2.3x) came back as separate blocks. Character boxes can overlap slightly (a negative gap counts as touching, never a split). A line geometry judged to be one sentence carries no space for the character speaker rules to re-split on (sub-threshold spaces inside a block are dropped when the line is rebuilt from its character boxes); blocks without usable character boxes keep their text as-is and the character rules decide instead. Every split/join decision is recorded for the debug log (see the `GEO` lines below).
    - *Layer 2 — speaker split* (`split_speakers`, applied first in `emit_held_lines`): one OCR line is split into speaker utterances on (a) whitespace between two CJK clusters (Chinese uses no spaces between words, so this is almost always a speaker change or a line break OCR collapsed into one line) and (b) a dash (`-`, `－`, `–`, `—`) between two CJK clusters, or at the start of the second piece. Each piece is a separate utterance (own translation/log/TTS). Guardrails: no split when a neighbour is not CJK (numbers/dates like `9 日` or `3.5`, CJK next to Latin/digits), or when either adjacent CJK cluster is shorter than 2 characters (a lone interjection such as `是 啊` stays one line); punctuation between the cluster and the separator is ignored.
    - *Layer 1 — sentence segmentation* (`SentenceBuffer::drain_completed`, ICU4X `SentenceSegmenter`, UAX #29): sentence boundaries come from the segmenter, not from a hand-written punctuation list — every segment except the last is a complete sentence, and the last segment is complete only when the segmenter itself breaks after it (see `segment_is_complete`). This handles closing quotes/brackets after terminal punctuation (`他说"你好。"` is one sentence), `...`, and ASCII terminal punctuation, and does NOT split a decimal point (`3.5元` stays one fragment). One explicit exception: the segmenter does NOT break after U+2026 HORIZONTAL ELLIPSIS (verified by probe 2026-11-25: `等一下…… X`, `等一下…… 再见`, and `等一下……你好` all stay one segment), so a trailing `…` is treated as terminal by an explicit check in `segment_is_complete` — preserving the old list's behavior for that one character instead of silently losing it. The crate is `icu_segmenter` with the `compiled_data` feature; binary cost measured after the 2026-11-25 build — see the release-binary note at the end of §3.5.
    - *Buffer rule* (per fragment, after the speaker split): the `SentenceBuffer` decides instead of appending blindly (the old append-always behavior merged consecutive subtitles with no terminal punctuation and no blank gap into one long line — real log 2026-11-25: 33/383 lines merged, e.g. `啊 下个月啊到金泽住一晚吗哦 挺好的嘛…`):
      - *Extends* (the buffered text is a strict prefix of the new fragment, or vice versa — progressive reveal / re-read with more text): REPLACE the buffer with the longer reading.
      - *Highly similar* (normalized Levenshtein similarity ≥ the configured `dedupe_threshold`, same metric and normalization as the dedupe): REPLACE the buffer with the freshest reading (same subtitle re-read with OCR misread(s)).
      - *Otherwise* (a new subtitle began — hard cut): FLUSH the buffer as a completed sentence first, then push the fragment fresh.
  - A sentence completes on terminal punctuation (UAX #29), after `blank_reset_frames` blank frames, after 1.2 s with no new fragment, or at the 10 s max-age safety valve. Two visual lines in one frame stay separate utterances (the second line flushes the first before starting).
  - Known limits, stated plainly: (1) a sentence split across two subtitle cards whose second card shares no prefix and no high similarity with the first (e.g. `你好，我是` then `小明。`) is indistinguishable from a new subtitle and is treated as one — the first card completes on its own. Each half still translates correctly on its own. (2) Two sentences stuck together with NO signal at all — no space, no dash, no punctuation (e.g. `大家就开心地享受吧好的好的那就再见了`) — cannot be split by either layer and stay one line (the blind case). (3) OCR sometimes misreads `-` or inserts spurious spaces; a single sentence split at a spurious space becomes two utterances, each still translating on its own.
  - *Future directions, NOT implemented:* (a) using the per-block confidence `rate` to pick the best reading when the engine returns overlapping variants; (b) using overlapping boxes to distinguish a continuing sentence from a new subtitle. Both are separate inferences Danh has not approved; they are noted here only as follow-ups, not built.
  - **Raw-OCR debug log (toggle in Settings → Chung, default OFF):** when `debug_logging` is ON, the pipeline writes `%APPDATA%\cap2voice\logs\ocr-debug-YYYY-MM-DD.log` (same directory and daily rotation as the subtitle log) with one line per OCR call plus one line per pipeline decision: `[14:40:46.353] OCR blocks=1 | [192,33-363,71 r=1.00] "你好，我是" | lines=["你好，我是"]` (raw blocks before dedupe/hold/buffer; `blocks=0` means OCR saw nothing; blocks with `lines=[]` means the CJK filter dropped everything; no line at all means the pixel-diff gate skipped the frame), `PIXEL-DIFF skipped N frames` (one aggregate line per static run), `DEDUPE dropped "<text>" sim=0.97`, `HOLD replaced "<old>" with "<new>"`, `SPEAKER split "<line>" into ["a", "b"]`, `BUFFER <Extended|KeptLonger|ReplacedSimilar|NewSubtitle|Started> frag="<text>" flushed=[...] buf="<text>"`, `BUFFER flush(blank|timeout) "<text>"`, `GEO split|joined block-gap|char-gap gap=82px thr=20px boxes=[95,85-212,128] [294,86-410,129] "你好吗" | "我很好"` (one line per geometric split/join decision: the gap judged, the threshold, the boxes on either side). The toggle takes effect on the next pipeline start (the logger is built at start). When OFF, no debug file is created and the pipeline pays nothing but a `None` check per decision. Only OCR text is logged — never audio or translations.
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
- `AC-OCR-8` (merge fix): replaying the merged sequences from the real 2026-11-25 log through the buffer splits them — L14 (`啊 下个月啊到金泽住一晚吗` / `哦 挺好的嘛` / `是 是呀虽然我也很想去 但有点难呢`), L73, L119 come out as separate completed lines; L116 splits into its two speaker pieces (`他很温柔` / `下次也让爸爸见见他`) via the speaker layer; L126 identical re-reads collapse to a single line instead of appending. Unit tests pin all five cases.
- `AC-OCR-9` (merge fix): a progressive reveal (`明天` → `明天早上见。`) still accumulates into one completed sentence, and a highly similar re-read (one OCR misread in 10 chars) still replaces the buffer; all pre-existing A2 tests pass except the three that pinned the old append-always behavior, which are updated with comments stating why.
- `AC-OCR-10` (OCR debug log): with the Settings → Chung debug toggle ON, a live run writes `ocr-debug-YYYY-MM-DD.log` next to the subtitle log with one line per OCR call in the documented format (including `blocks=0` lines for OCR misses) plus one line per pixel-diff run, dedupe drop, hold replacement, speaker split, buffer decision, and geometric split/join decision (`GEO` lines); with the toggle OFF (default) no debug file is created. The file stays absent while the pipeline is idle (pixel-diff skips).
- `AC-OCR-11` (ICU4X sentence segmentation, UAX #29): `他说"你好。"` completes as one sentence (no stranded `"` preliminary), `等一下……` completes as one sentence (via the explicit trailing-… check — the segmenter itself does not break after U+2026, verified by probe), `3.5元` does NOT split at the decimal point, and `Hello. World` still splits at the ASCII period. Unit tests pin all four cases.
- `AC-OCR-12` (speaker split): the speaker rules split `你先睡吧 不用等我吃饭了` and `你好吗 - 我很好` into two utterances each, keep `9 日`, `3.5 元`, `是 啊`, `Hello 你好`, and `你好 123` as one line (guardrails), and leave the no-signal case `大家就开心地享受吧好的好的那就再见了` as one line (the blind case, documented, not hidden). Unit tests pin all cases.
- `AC-OCR-13` (geometric speaker split): two blocks on one row 80 px apart (threshold 20 px) stay two lines, two blocks 12 px apart (threshold 18 px) join into one line, two blocks on two rows stay two lines, one block split at a 91 px character gap (threshold 19 px) becomes two lines, one block with a 10 px character gap (`明天早上 见。`) stays one line with the space dropped, and a block without character boxes keeps its text (character rules decide). Unit tests pin all cases; the probe-image measurements (0.33x same-sentence gap vs 0.79x speaker gap) justify the 0.5x threshold.

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

- Rust defines an async `TtsEngine` trait: `async fn synthesize(&self, text: &str, voice: &str, rate: &str) -> Result<TtsSynthesis { audio, format, retries }, TtsError>`, where `format` is `AudioFormat` (`Mp3` for Edge-TTS, `Wav` for Piper) and travels with the bytes through the cache and the audio queue.
- V1 implements the `TtsEngine` trait with a native Rust Edge-TTS WebSocket client (`wss://api.msedgeservices.com/tts/cognitiveservices/websocket/v1`), receiving raw MP3 bytes. No HTTP server or Python is involved.
- Default voice: `vi-VN-NamMinhNeural`. Built-in selectable Vietnamese voices: `vi-VN-NamMinhNeural` (Nam — mặc định) and `vi-VN-HoaiMyNeural` (Nữ).
- **Selectable TTS engine (`SwitchableTtsEngine`, `voice.engine` in `config.toml`, default `edge`) — Piper, 2026-11-24:** beside Edge-TTS, the app offers Piper (VITS, offline) with the Vietnamese voice Ngọc Huyền. The engine and voice are chosen ONLY in Settings → `Giọng đọc` (the overlay has no voice selector since 2026-11-24); switching the engine resets the voice to that engine's default so an Edge voice id is never paired with Piper (or vice versa). Chinese click-to-replay always uses Edge-TTS (`zh-CN-XiaoxiaoNeural`) even when Piper is the active Vietnamese engine, because Piper ships Vietnamese voices only. Piper runs fully offline, so it has no 429 retry; the Edge-TTS retry (429/5xx/network, backoff 500 ms/1 s/2 s, max 3) applies to Edge-TTS only.
- **Piper voice download (on demand):** the 63.5 MB Ngọc Huyền model (`ngochuyen.onnx` + `tokens.txt`) is downloaded from Hugging Face only when Danh clicks `Tải giọng` in Settings → `Giọng đọc`, with a progress bar; files land in `%APPDATA%\cap2voice\voices\piper\ngochuyen\` (override `CAP2VOICE_PIPER_VOICES_DIR`). The model is never bundled into the binary or committed to git. The voice registry (`PIPER_VOICES` in `tts/piper.rs`) is designed so later voices from the same repo (Mạnh Dũng, Minh Quang, …) can be added as one registry entry each.
- **License note (Piper voices):** the voice source is an unofficial mirror (`doof-ferb/nghitts-copy` on Hugging Face) of NGHI-TTS; metadata says Apache-2.0 but the voices are named after real people. Personal/hobby use only for Danh — not for commercial use or redistribution.
- Audio playback uses `rodio` `0.23` on a dedicated background audio worker thread:
  - **Non-overlapping FIFO queue:** One sentence plays at a time; incoming sentences wait in queue until the current sentence finishes.
  - **Bounded queue depth (max 3 pending items):** When the queue already has 3 unplayed items and a new item arrives, drop the **oldest unplayed** item (never interrupt the currently playing clip) so speech stays synchronized with fast dialogue.
  - **Volume & Mute:** Volume (`0.0..=1.0`, default `0.75`) adjusts `rodio::Sink` gain in real time. Toggling Mute or clicking `Dừng` immediately stops active playback and clears the pending queue.
  - **Auto-read toggle (`Tự động đọc phụ đề`):** When OFF (default ON), no TTS synthesis is requested and no audio plays; OCR, translation, overlay display, and logging continue normally.
  - **Failure visibility:** If TTS synthesis fails (network offline / Edge-TTS error) or audio output device fails to open, show a non-blocking warning in the overlay status bar while continuing to translate and display subtitles.

**Acceptance Criteria:**
- `AC-TTS-1`: With `Tự động đọc phụ đề` ON and unmuted, each newly translated Vietnamese subtitle is read aloud clearly using `vi-VN-NamMinhNeural` on the PC's default audio output.
- `AC-TTS-2`: Switching voice to `vi-VN-HoaiMyNeural` (in Settings → `Giọng đọc` — the ONLY place to change voices since 2026-11-24; the overlay has no voice selector) causes the very next subtitle to play in the female voice.
- `AC-TTS-8` (Piper): With the TTS engine set to Piper in Settings, the app downloads the Ngọc Huyền model on demand into `%APPDATA%\cap2voice\voices\piper\ngochuyen\` with visible progress, then reads the next Vietnamese subtitle aloud with the Piper voice (WAV, 22,050 Hz) — a new cache file appears, distinct from Edge-TTS output — and click-to-replay on the Vietnamese line replays with Piper.
- `AC-TTS-9` (Piper): Switching the TTS engine back to Edge-TTS restores Edge-TTS speech on the next subtitle; on a fresh config the engine is Edge-TTS.
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
  - **Fixed size (2026-11-25):** the overlay window is a FIXED size — it NEVER resizes with the subtitle content. It starts at the default `620 × 204` px, or at the user's saved size (`config.display.overlay_size`) when present, and only changes when the user drags the resize grip at the bottom-right corner (minimum `460 × 140` px, maximum `1200 × 600` px). This supersedes the 24/11 2-line fix, which grew the window to ~259 px for two-line subtitles — both lines still display, just smaller when needed (see the fit rule below). (Note: at 204 px the body budget is ~74 px; one history pair above a two-line current measures ~78 px at the 12 px floor, so the fit keeps it only because the inter-block gaps were tightened to 2 px — see the fit rule. Two full history pairs need a taller window; the user drags the grip taller for that, and the fit drops the oldest pair first when it still does not fit.) The grip-chosen size persists in `config.toml` (`display.overlay_size`, debounced 400 ms, also saved on clean exit) and survives restarts. This supersedes the 24/11 2-line fix, which grew the window to ~259 px for two-line subtitles — both lines still display, just smaller when needed (see the fit rule below).
  - Draggable anywhere on empty panel background to reposition freely; last `(x, y)` window position persisted in `config.toml`.
  - **Capture self-exclusion:** Apply Windows `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` on Windows 10 2004+ so even if the user drags the overlay over the capture region, `xcap` captures the underlying video rather than the overlay. Also warn if the API call fails and bounding boxes overlap.
- **Typography & Font Chain:**
  - Register a composite `egui::FontDefinitions` fallback chain using Windows system fonts so both Simplified/Traditional Chinese (`Microsoft YaHei` / `SimSun`) and full Vietnamese diacritics (`Segoe UI` / `Arial`) render cleanly with zero missing-glyph boxes (`□`).
  - Top line: Chinese original text in white (`#FFFFFF`), size = `base_font_size × 1.35` (default ~24 px).
  - Bottom line: Vietnamese translation in green (`#4ADE80`, configurable), size = `base_font_size` (default `18 px`).
  - Both lines wrap cleanly inside the panel width without clipping.
  - **Fit-to-window (2026-11-25, `fit_subtitle_blocks` in `ui/overlay.rs`):** every frame, ALL visible lines (history pairs + current pinyin / Chinese / Vietnamese) share one uniform scale chosen so they fit the window's content area (`available_height` minus a 65 px reserve for the separator + action bar + grip). Inter-block gaps are kept tight (2 px) so one history pair above a two-line current fits the ~74 px body budget at the 12 px floor (measured 78 px with 4 px gaps, 74 px with 2 px gaps). The scale is found by binary search over the real egui text layout (`ui.painter().layout` at the candidate size), so a long single line counts at its wrapped height, not its character count. The scale never goes below the readable floor: Vietnamese `12 px` (the `font_size_px` config clamp floor, i.e. scale `12 / font_size_px`). When even the floor does not fit, content drops in this order — oldest history pair first (both its lines together, so a Vietnamese line never survives without its Chinese line), then the pinyin line, then the leading lines of the current Chinese line, then the leading lines of the current Vietnamese line; a block that lost leading lines shows a `…` prefix on its first remaining line. A block's last line never drops, and the last remaining line overall never drops: if nothing fits, that one line renders (clipped) and the window still never grows. After dropping, the survivors are scaled back up to fill the budget.
- **Subtitle history (2026-11-25, opt-in, default OFF):**
  - Settings → `Hiển thị`: `Hiện phụ đề trước đó` dropdown — `Tắt` / `1 dòng` / `2 dòng` (saved as `display.history_lines`, `0`–`2`; values above 2 are clamped to 2 where used). Applies on save, no restart needed; the Settings preview shows a dimmed sample history pair when the setting is on.
  - Each history entry is one previously COMPLETED subtitle pair (Chinese line + Vietnamese line) as it was shown on the overlay, taken from the pipeline's own record (the last two completed pairs, kept in `SharedPipelineState` and published in `PipelineSnapshot.history`, most-recent-first). A pair enters history when its translation succeeds (`record_completed_for_history`, in emission order by `seq`, so a translation resolving out of order never moves history backwards); a repeated line never duplicates itself into history. Recording happens at translation-complete time, not at emit time, and does not require the pair to still be on screen — the earlier emit-time design lost pairs emitted while the previous translation was still in flight, and dropped whole multi-line groups. History lines show NO pinyin (to save height) and are view-only (no click-to-replay).
  - Display order: oldest history pair on top, newer pair below it, current subtitle at the bottom. History text renders dimmed at 55% of the current lines' alpha (`HISTORY_DIM_ALPHA`), clearly dimmer than the current lines.
  - History counts toward the fit: the whole content (history + current) must fit the fixed window, shrinking uniformly when history is on. The window never changes size when history appears or disappears.
- **Click-to-replay:** clicking the pinyin or Chinese line replays it in Chinese via Edge-TTS (`zh-CN-XiaoxiaoNeural`); clicking the Vietnamese line replays it in Vietnamese via the CURRENT TTS engine (Piper when Piper is active, Edge-TTS otherwise). Both are explicit user actions, so they play even while muted, and both go through the TTS cache first. (Vietnamese click-to-replay did not exist before the Piper slice — only Chinese replay did — and was added here so `AC-TTS-8` is verifiable.)
- **Sentence assembly:** what the overlay shows as one line is one assembled sentence: consecutive subtitles are split or joined by the layers in §3.2 (geometric split + speaker split + ICU4X sentence segmentation + `SentenceBuffer` rule, merge fix 2026-11-25), never two hard-cut subtitles glued together.
- **Controls (matching Mockup 1 & `brief.md` §6):** icon buttons with Vietnamese hover tooltips (vector glyphs, no text labels; the Settings sidebar tabs keep text labels because they are navigation, not actions):
  - Compact top header strip (or hoverable corner controls): app status indicator dot + status text (`Đang chờ` / `Đang dịch` / error message), Settings gear icon button (tooltip `Cài đặt`), and Close icon button (tooltip `Đóng` — hides to tray when close-to-tray is enabled).
  - Bottom action bar:
    - `Chọn vùng` icon button (region icon, tooltip `Chọn vùng`)
    - `Bắt đầu` icon button (play icon, tooltip `Bắt đầu`; disabled while running; if clicked with no saved region, prompts user to select a region first or opens region selector)
    - `Dừng` icon button (stop icon, tooltip `Dừng`; disabled while idle)
    - Speaker mute/unmute icon button (tooltip `Tắt tiếng` / `Bật tiếng`)
    - **No voice selector on the overlay (since 2026-11-24):** voice selection — both the TTS engine choice and the voice choice — lives ONLY in Settings → `Giọng đọc`. The overlay shows the current voice in the settings summary panel but offers no control to change it.
- **In-place subtitle window (optional mode, default OFF):**
  - Settings → `Hiển thị`: `Hiển thị phụ đề tại vị trí gốc trên video` checkbox (default OFF) + `Độ mờ nền phụ đề tại chỗ` slider (`0%`–`100%`, default `70%`), saved in `config.toml` as `display.inplace_subtitle` / `display.inplace_bg_opacity`.
  - When ON, an ADDITIONAL separate borderless, fully transparent, click-through window appears exactly over the capture region (position + size = region), showing only the translated subtitle text on a background rectangle of the chosen opacity that covers the original subtitle underneath. The main floating overlay stays visible and usable at all times.
  - Renders the Vietnamese line (plus the Chinese line above it when `show_chinese_line` is on, plus pinyin when enabled); font size scales down to fit the region height (never larger than `font_size_px`).
  - Follows the region when the user re-selects it; appears/disappears immediately when toggled (no restart).
  - The text window is excluded from capture (`WDA_EXCLUDEFROMCAPTURE`) so it never hides the original subtitle from OCR; if WDA fails, a Vietnamese warning is shown in the overlay.
  - When OFF, the text window is hidden and everything behaves as before.

- **Release-binary size (merge fix, 2026-11-25):** `cap2voice-app.exe` measured `48,132,608` bytes after the merge fix (ICU4X `icu_segmenter` with `compiled_data` + geometric speaker split + speaker rules + debug-log toggle), vs `48,013,824` bytes at option B (`f8fca7c`) — an increase of `118,784` bytes (`+0.25%`), far inside the 15%-per-slice budget. The ICU4X sentence-break tables cost only ~96 KB over the pre-ICU4X merge-fix build (`48,036,864` bytes); the compiled sentence data is compact.
- **Release-binary size (fixed-size overlay + history, 2026-11-25):** `cap2voice-app.exe` measured `48,179,712` bytes after this slice (fixed-size overlay, uniform text fit with real egui layout, grip size persistence, subtitle history with race-free recording), vs `48,132,608` bytes after the merge fix — an increase of `47,104` bytes (`+0.098%`), far inside the 15%-per-slice budget.
**Acceptance Criteria:**
- `AC-UI-1`: Overlay floats borderless and always-on-top over both a local media player (e.g. VLC/MPV) and a maximized browser video (e.g. Chrome/Edge on Bilibili/YouTube).
- `AC-UI-2`: Chinese line renders in white above Vietnamese line in green; verified visually via real-PC screenshot with mixed Chinese + Vietnamese diacritics (e.g. `"夜深了，雨还在下。"` / `"Đêm đã khuya, mưa vẫn còn rơi."`) showing zero missing-glyph tofu boxes.
- `AC-UI-3`: Dragging the overlay moves it smoothly and persists its coordinates across app restarts.
- `AC-UI-4`: All 4 control-bar elements (`Chọn vùng`, `Bắt đầu`, `Dừng` icon buttons, speaker mute icon button) plus the `Cài đặt` gear and `Đóng` icon buttons work as specified and reflect live running/muted state; every icon button shows a Vietnamese tooltip on hover. (The voice dropdown was removed from the overlay on 2026-11-24; voice selection lives ONLY in Settings → `Giọng đọc`.)
- `AC-UI-5`: Network, engine, or OCR errors render visibly inside the overlay status banner in Vietnamese and clear automatically once a subsequent request succeeds.
- `AC-UI-6`: Placing the overlay partially over the captured subtitle region does not cause an OCR feedback loop (verified via `WDA_EXCLUDEFROMCAPTURE` on Windows 10/11).
- `AC-UI-7` (in-place mode): With the mode ON, a separate transparent window sits exactly over the capture region (window rect == region rect) showing the translated line on the chosen-opacity background while the main overlay stays visible and usable; toggling OFF hides the text window immediately; re-selecting the region moves the text window to the new region.
- `AC-UI-8` (resizable overlay): Dragging the bottom-right grip changes the overlay window width/height within `460 × 140` – `1200 × 600` px and the layout reflows without clipping buttons or subtitle lines; the grip-chosen size persists across app restarts (`display.overlay_size` in `config.toml`).
- `AC-UI-9` (fixed size + fit, 2026-11-25): The overlay window rect does NOT change when the subtitle goes from 1 line to 2 lines (or back) — measured via window-rect queries across video segments. A 1-line subtitle renders at the full configured size; a 2-line subtitle shrinks uniformly (same scale for Chinese, pinyin, and Vietnamese) and stays inside the fixed window; many lines hit the 12 px floor and drop oldest-first with a `…` marker instead of growing the window.
- `AC-UI-10` (subtitle history, 2026-11-25): With the setting at `1 dòng`, after two sentences have passed the previous completed pair shows dimmed above the current subtitle; at `2 dòng` two previous pairs show (oldest on top); at `Tắt` no history shows. History lines are not clickable (no replay). The window rect stays fixed in all three states, and the text shrinks uniformly when history is on.

---

### 3.6 Settings Window & Configuration Persistence (`settings-ui` + `config`)

Reference mockup: `docs/mockups/media-generation-settings-tray-*.webp`.

- **Window Layout:**
  - Standard decorated window titled `Cài đặt` (opened via overlay gear button or tray `Cài đặt`), dark theme matching Mockup 3.
  - Left navigation sidebar with 5 sections (exact labels from `AGENTS.md` and Mockup 3):
    1. `Chung` (General):
       - `Tốc độ chụp` dropdown (`5 khung/giây`, `10 khung/giây` default, `15 khung/giây`)
       - `Vùng phụ đề` summary (`<x>, <y> · <w> × <h> px` or `"Chưa chọn vùng"`) + `Chọn lại vùng` icon button (tooltip `Chọn lại vùng`)
       - WeChat OCR paths (`wxocr.dll` & Weixin directory — auto-detected status + editable path inputs) + `Kiểm tra WeChat OCR` icon button (tooltip `Kiểm tra WeChat OCR`)
       - Subtitle log directory + enable logging toggle (default ON)
       - Debug OCR log toggle (default OFF): when ON, writes `ocr-debug-YYYY-MM-DD.log` (raw OCR per frame + pixel-diff / dedupe / hold / speaker / buffer decisions) into the log directory; takes effect on the next pipeline start (see §3.2)
       - Checkbox: `Ẩn xuống khay hệ thống khi đóng cửa sổ` (default checked)
       - Checkbox: `Khởi động cùng Windows` (default unchecked)
       - Checkbox: `Khởi động ở chế độ ẩn xuống khay` (default unchecked)
    2. `Dịch` (Translation):
       - Engine selector (`Helsinki (mô hình AI)` / `Vietphrase (từ điển)`, default Helsinki) + live engine status line; switching applies immediately without restart
       - Fuzzy deduplication threshold slider (`0.80`–`0.99`, default `0.93`)
       - Pixel-diff sensitivity threshold slider (`0.5%`–`10.0%`, default `2.0%`)
    3. `Giọng đọc` (Voice / TTS) — the ONLY place to change the TTS engine or voice (since 2026-11-24):
       - `Engine giọng đọc` dropdown (`Edge-TTS (trực tuyến)` default, `Piper (offline)`); switching the engine resets the voice to that engine's default and applies live on save, without restart
       - `Giọng đọc` dropdown — engine-dependent: Edge-TTS offers `vi-VN-NamMinhNeural` (default) / `vi-VN-HoaiMyNeural`; Piper offers `Ngọc Huyền` (default, first voice)
       - Piper voice download: when Piper is selected and the voice is not yet downloaded, a `Tải giọng` button with progress bar downloads the model on demand into `%APPDATA%\cap2voice\voices\piper\<voice-id>\`; a downloaded voice shows `Giọng đã tải — sẵn sàng dùng (offline).`
       - `Âm lượng` slider (`0%`–`100%`, default `75%`)
       - Speech rate selector (`-20%`, `-10%`, `0% (Mặc định)`, `+10%`, `+20%`)
       - Toggle: `Tự động đọc phụ đề` (default ON)
       - `Nghe thử` icon button (tooltip `Nghe thử`, plays a short Vietnamese sample sentence via the DRAFT engine/voice — Piper preview uses the downloaded model, Edge-TTS preview uses the network)
    4. `Hiển thị` (Display):
       - `Cỡ chữ phụ đề` slider (`12 px`–`40 px`, default `18 px`)
       - Overlay background opacity slider (`30%`–`100%`, default `85%`)
       - Toggles: show Chinese line (default ON), show Vietnamese line (default ON), always-on-top (default ON)
       - Subtitle history dropdown `Hiện phụ đề trước đó` (`Tắt` default / `1 dòng` / `2 dòng`): shows 1–2 previously completed pairs, dimmed, above the current subtitle; applies on save, no restart; see §3.5
       - In-place subtitle mode: `Hiển thị phụ đề tại vị trí gốc trên video` checkbox (default OFF) + `Độ mờ nền phụ đề tại chỗ` slider (`0%`–`100%`, default `70%`); see §3.5
       - Text color pickers/presets (defaults: Chinese `#FFFFFF`, Vietnamese `#4ADE80`)
    5. `Phím tắt` (Hotkeys):
       - Configurable global shortcuts with defaults:
         - Start / Stop translation: `Ctrl+Alt+S`
         - Select region: `Ctrl+Alt+R`
         - Show / Hide overlay: `Ctrl+Alt+H`
         - Mute / Unmute TTS: `Ctrl+Alt+M`
       - `Khôi phục mặc định` icon button (tooltip `Khôi phục mặc định`) to reset hotkeys.
  - Footer buttons: `Huỷ` icon button (tooltip `Huỷ`, discards unsaved draft edits and closes Settings) and `Lưu thay đổi` icon button (tooltip `Lưu thay đổi`, validates, saves atomically to `%APPDATA%\cap2voice\config.toml`, applies live to running app).
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
- **TTS — Piper offline engine (`PiperTtsEngine`, 2026-11-24):** `sherpa-onnx` `2.0.3` + `piper-phonemize` `1.6.1` Rust crates, `shared` feature (DLLs shipped beside the exe). Implementation direction (a) — sherpa-onnx with external phonemization — was chosen and is documented in `tts/piper.rs`: sherpa-onnx v2 dropped espeak-ng, so the text is phonemized FIRST with piper-phonemize (espeak-ng `vi` voice) and the resulting Unicode codepoints are passed as `GenerationConfig::phoneme_codepoints` with empty text. Direction (c) (Piper directly via the app's existing `ort`) was evaluated and rejected: the exact VITS token sequence could not be reverse-engineered reliably (length-based matching was inconclusive across test sentences). Direction (b) (pin sherpa v1) was rejected: v1 is unmaintained and its README is the trap that fails with "phoneme_codepoints is empty" on v2. Both crates' Windows static libs are built with `/MT` while the app uses `/MD`, so the `shared` (DLL) feature is required — static linking would fail with `LNK2038`. The sherpa `onnxruntime.dll` shipped beside the exe does not conflict with `ort`'s runtime (verified live: Helsinki still translates after the DLLs land). Output is WAV PCM16 mono 22,050 Hz, encoded in `tts/piper.rs`; Edge rate strings map to Piper speed (`+10%` → 1.1, clamped `0.5`–`2.0`). `GenerationConfig.silence_scale` is set to `1.0` explicitly (the Rust crate defaults it to `0.2`; the reference run Danh approved used the C++ default `1.0`). Model + `tokens.txt` per voice under `%APPDATA%\cap2voice\voices\piper\<voice-id>\` (override `CAP2VOICE_PIPER_VOICES_DIR`); `espeak-ng-data` (~7 MB) is embedded in the binary by piper-phonemize (no feature to disable it) — the exe grows ~7 MB, justified as required offline phonemization data. Voice source is an unofficial mirror (`doof-ferb/nghitts-copy`, NGHI-TTS); personal/hobby use only, not for commercial use or redistribution. **Download robustness (fixed 2026-11-24 after the first live run stalled at ~3 MB with no error):** the HTTP client uses a per-read timeout of 60 s (a stalled connection fails visibly instead of hanging forever; no global timeout, a 63.5 MB download on a slow link legitimately takes minutes); progress events are sent with `try_send` so a full channel can never stall the download thread; the app schedules repaints while a download is in flight so the progress bar drains and updates live.
- **Audio format:** `AudioFormat` (`Mp3` / `Wav`) travels with the audio bytes through `TtsSynthesis`, the TTS cache (filenames carry the extension, `.wav` for Piper), and the `rodio` queue (`symphonia-wav` + `symphonia-pcm` features — WAV decoding needs the PCM codec, `symphonia-wav` alone fails the probe with "format not recognized"); a format-hint mismatch is rejected at decode time.

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
