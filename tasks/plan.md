# Implementation Plan: cap2voice — V1 (MVP)

**Source spec:** `docs/spec.md` (V1 approved as-is; all 12 assumptions in §11 confirmed)  
**Quality bar:** `CONSTRAINTS.md`  
**Task checklist target:** `tasks/todo.md`  
**Workflow contract:** `AGENTS.md`

---

## 1. Overview

Build `cap2voice` V1 (MVP), a Windows 10/11 pure-Rust desktop application (`eframe`/egui) that observes a user-selected screen region over any video player or browser, skips unchanged frames via pixel-diff, runs local WeChat 4.x OCR (`wcocr-rs`), filters Chinese text, fuzzy-deduplicates repeated lines (`0.93`), translates zh→vi and synthesizes Vietnamese TTS (`vi-VN-NamMinhNeural`) through a Rust-managed temporary local Python FastAPI server, displays both lines on a borderless always-on-top overlay (white Chinese above green Vietnamese), speaks Vietnamese sequentially via `rodio`, logs `Chinese : Vietnamese` pairs to UTF-8 daily files, and supports a 5-tab Settings window plus close-to-tray background operation.

Implementation proceeds in **14 small vertical slices (each ≤ 5 files)** organized across **5 Phases** with **5 mandatory Checkpoints**, front-loading the highest-risk technical unknowns (`wcocr-rs` Rust refactor, temporary Python server contract, real-PC `xcap` capture latency, and Windows Job Object cleanup) so any failure surfaces immediately.

---

## 2. Dependency Graph

Dependencies are strictly one-way (no cycles), matching the approved Capability Map in `docs/spec.md` §1:

```text
Phase 1 (Foundations & High-Risk Proofs)
├── Task 1: Workspace + wcocr-rs mojo/proto core
│      └── Task 2: wcocr-rs WeChatOcr typed API + timeout + real-PC OCR proof
└── Task 3: Python FastAPI server (/health, /translate, /tts) + pytest/mypy

Phase 2 (App Core, Capture Risk Proof & Managed Server)
├── Task 4: cap2voice-app scaffold + config.toml + Windows autostart
│      ├── Task 5: Screen capture (xcap) + pixel-diff gate + real-PC latency bench
│      └── Task 6: Rust HTTP clients (Translator/TtsEngine) + ServerProcess (Job Object)

Phase 3 (Headless End-to-End Pipeline & Audio)
├── Task 7: OCR adapter + WeChat path auto-discovery + CJK filter + fuzzy deduper  (depends: 2, 4)
├── Task 8: rodio AudioPlayer + bounded FIFO queue (max 3 pending)                 (depends: 4)
└── Task 9: Async PipelineRunner + sequence guard + daily UTF-8 subtitle logger    (depends: 5, 6, 7, 8)

Phase 4 (Desktop UI — Matching All 3 Mockups)
├── Task 10: eframe App shell + CJK/VN fonts + floating 2-line overlay + WDA       (depends: 9)
├── Task 11: Fullscreen red-dashed region selector + DPI alignment                 (depends: 5, 10)
└── Task 12: 5-tab Settings window (Chung/Dịch/Giọng đọc/Hiển thị/Phím tắt)       (depends: 4, 10, 11)

Phase 5 (System Tray, Global Hotkeys & V1 Release Gate)
├── Task 13: Windows tray icon (5 items) + close-to-tray + global hotkeys          (depends: 10, 12)
└── Task 14: 15-min real-video E2E verification (local + browser) + test log       (depends: 13)
```

---

## 3. Architecture Decisions

1. **Pure Rust internal `wcocr-rs` (`rlib` only):** Drop the `cdylib` and C ABI (`wechat_ocr`/`stop_ocr`) from `C:\Users\doanv\wechat-ocr-rs`, add `thiserror`-typed `OcrError`, and replace the unbounded `Condvar::wait_while` with `wait_timeout_while` (`3000 ms` default) so a stalled WeChat worker never freezes the app.
2. **Trait boundaries for V2 replaceability:** Keep `FrameCapturer`, `OcrEngine`, `TextDeduplicator`, `Translator`, and `TtsEngine` as Rust traits. Swapping `xcap` → `windows-capture` (if real-PC p95 > 50 ms) or swapping the temporary Python API server → native Rust in V2 requires zero UI, pipeline, or tray rewrites.
3. **Fail-fast hardware proof in Phase 1–2:** Benchmark `xcap` latency (100 cropped frames) and prove `wcocr-rs` + Python server Job Object cleanup on Danh's real PC before building any GUI.
4. **Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`):** Assign the spawned `uvicorn` Python child process to an OS Job Object so even a crash or `Stop-Process -Force` of `cap2voice-app.exe` automatically reaps the child process with zero zombies.
5. **Overlay capture self-exclusion (`WDA_EXCLUDEFROMCAPTURE`):** Call `SetWindowDisplayAffinity` on the overlay `HWND` (Windows 10 2004+) so dragging the overlay over the subtitle region cannot trigger an OCR feedback loop.
6. **Composite Windows font chain in `egui`:** Load system fonts (`Segoe UI` + `Microsoft YaHei`) into `egui::FontDefinitions` so Simplified/Traditional Chinese and full Vietnamese diacritics render crisply with zero tofu (`□`) boxes.
7. **Sequence-guarded async translation + bounded FIFO TTS queue:** Tag each emitted Chinese subtitle with a monotonic `u64` sequence number so late translations never overwrite newer lines; cap unplayed TTS queue depth at `3` (dropping oldest unplayed item, never interrupting active speech) to prevent audio lag.
8. **Honest failure reporting:** Never swallow network, `429`, OCR, config, or audio errors. Corrupt `config.toml` is backed up to `config.toml.bak` and surfaced in UI; network errors clear stale Vietnamese text and show explicit Vietnamese error banners.

---

## 4. Phased Task Index & Checkpoints

Detailed per-task acceptance criteria ("done means"), exact automated commands, real-PC verification steps, and touched file lists live in `tasks/todo.md`. Every task touches `<= 5` files.

### Phase 1: Foundations & High-Risk Proofs
- [ ] **Task 1:** Workspace root & `wcocr-rs` mojo/proto core (`M`, 5 files)
- [ ] **Task 2:** `wcocr-rs` typed `WeChatOcr` API with request timeout & real-PC OCR proof (`S`, 3 files)
- [ ] **Task 3:** Temporary local Python FastAPI server (`/health`, `/translate`, `/tts`) (`M`, 3 files)

### Checkpoint 1: Local OCR & API Server Verified on Real PC
- [ ] `cargo fmt`, `cargo clippy -D warnings`, `cargo test -p wcocr-rs`, `mypy --strict server/server.py`, `pytest server/ -v` all pass
- [ ] Real-PC `wcocr-rs` OCR against `test-zh-custom.png` returns `errcode == 0`
- [ ] Live Python server translates Chinese → Vietnamese and produces playable MP3 for both Vietnamese voices; network-down error tested
- [ ] Review test output with Danh before Phase 2

### Phase 2: App Core, Capture Latency Proof & Managed Server
- [ ] **Task 4:** `cap2voice-app` scaffold, `config.toml` atomic persistence & Windows autostart helper (`M`, 5 files)
- [ ] **Task 5:** Screen capture (`xcap` 0.9.10), pixel-diff gate & real-PC latency benchmark (`M`, 4 files)
- [ ] **Task 6:** Rust HTTP clients (`Translator`/`TtsEngine`) & managed `ServerProcess` with Windows Job Object (`M`, 4 files)

### Checkpoint 2: Capture Budget & Process Lifecycle Verified on Real PC
- [ ] `xcap` real-PC cropped capture meets `<= 30 ms` median / `<= 50 ms` p95 (if exceeded, pause and ask Danh to approve `windows-capture` `2.0.1` swap)
- [ ] DPI alignment (`125%`/`150%`) spot-checked via saved crop PNG
- [ ] Graceful exit AND forced kill (`Stop-Process -Force`) leave `0` orphaned Python processes
- [ ] Review benchmark & lifecycle results with Danh before Phase 3

### Phase 3: End-to-End Headless Pipeline & Audio Engine
- [ ] **Task 7:** OCR adapter, WeChat 4.x path auto-discovery, CJK filter & fuzzy deduplicator (`0.93`) (`M`, 3 files)
- [ ] **Task 8:** `rodio` 0.23 `AudioPlayer` & bounded non-overlapping FIFO queue (max 3 pending) (`S`, 2 files)
- [ ] **Task 9:** Async `PipelineRunner`, sequence guard & daily UTF-8 subtitle logger (`M`, 3 files)

### Checkpoint 3: Full Headless Pipeline Verified on Real Hardcoded-Subtitle Video
- [ ] All workspace Rust & Python automated tests pass
- [ ] 60-second real video test on Danh's PC produces accurate `[timestamp] Chinese : Vietnamese` lines in `%APPDATA%\cap2voice\logs\`, sequential non-overlapping Vietnamese audio, and zero OCR calls when video is paused
- [ ] Review real translated log pairs with Danh before building GUI

### Phase 4: Desktop UI (Overlay, Region Selector, Settings)
- [ ] **Task 10:** `eframe` 0.37 app shell, CJK+Vietnamese font chain & floating 2-line overlay with `WDA_EXCLUDEFROMCAPTURE` (`M`, 4 files)
- [ ] **Task 11:** Fullscreen red-dashed region selector (`Chọn vùng`) with live physical coordinate badge (`S`, 2 files)
- [ ] **Task 12:** 5-tab Settings window (`Chung`, `Dịch`, `Giọng đọc`, `Hiển thị`, `Phím tắt`) with draft `Huỷ` / `Lưu thay đổi` (`S`, 2 files)

### Checkpoint 4: All 3 Mockup Screens Verified Live on Real PC
- [ ] Real-PC screenshots confirm visual & functional match to `docs/mockups/` (`overlay-main`, `region-select`, `settings-tray`)
- [ ] CJK + Vietnamese diacritics render cleanly with zero tofu boxes
- [ ] Dragging overlay over captured region causes zero OCR feedback loop
- [ ] Review screenshots and live UX with Danh before Phase 5

### Phase 5: System Tray, Global Hotkeys & V1 Release Gate
- [ ] **Task 13:** Windows system tray icon (exact 5 menu items), close-to-tray background mode & global hotkeys (`M`, 4 files)
- [ ] **Task 14:** 15-minute real-video E2E verification (local player + browser) & `docs/test-log-v1.md` sign-off (`S`, 2 files)

### Checkpoint 5: V1 Release Gate Complete
- [ ] All 50 `AC-*` acceptance criteria verified with pasted command output and real-PC screenshots in `docs/test-log-v1.md`
- [ ] `CONSTRAINTS.md` floor & enforced checks 100% green
- [ ] Ready for `/agent-skills:code-review-and-quality` and `/agent-skills:shipping-and-launch` (Danh gives final GO)

---

## 5. Parallelization Opportunities

- **Safe to parallelize (independent modules / zero shared files):**
  - Task 1+2 (`crates/wcocr-rs/`) and Task 3 (`server/`) have zero file overlap and can run in parallel during Phase 1.
  - Task 5 (`capture/`) and Task 6 (`api/`) can run in parallel once Task 4 (`config.rs` + `cap2voice-app/Cargo.toml`) lands.
  - Task 7 (`ocr/`) and Task 8 (`audio/`) can run in parallel during Phase 3.
- **Must be sequential:**
  - Task 1 → Task 2 (shared `wcocr-rs` crate).
  - Task 4 → Tasks 5, 6, 7, 8 (shared `cap2voice-app/Cargo.toml` and `AppConfig`).
  - Tasks 5, 6, 7, 8 → Task 9 (pipeline wires all four modules).
  - Task 9 → Task 10 → Tasks 11, 12 → Task 13 → Task 14 (UI viewports & event loop share `ui/mod.rs`).

---

## 6. Risks and Mitigations

| Risk | Impact | Likelihood | Mitigation & Early Detection |
|---|---|---|---|
| `xcap` capture latency exceeds `50 ms` p95 on target PC | High | Medium | Benchmarked early in **Task 5** (Checkpoint 2); isolated behind `FrameCapturer` trait for clean swap to `windows-capture` `2.0.1` after user approval |
| Windows DPI scaling (`125%`/`150%`) shifts captured box off subtitle | High | Medium | Store monitor ID + physical pixel rect; verify visually in **Task 5** and **Task 11** by saving cropped PNG on scaled display |
| Google Translate free endpoint rate-limits (`429`) or returns awkward colloquial translations | Medium | Medium | Pixel-diff + `0.93` dedupe minimize requests; explicit `429` banner + 2 s backoff in **Task 3/6/9**; benchmark quality on real drama in **Checkpoint 3 & Task 14** |
| `eframe` 0.37 close-to-tray event loop stops ticking when window hidden | High | Medium | Intercept `CloseRequested` with `ViewportCommand::CancelClose` + `Visible(false)`, wake UI loop from tray/hotkey event channel; verified live in **Task 13** |
| `wcocr-rs` temp PNG write adds latency or file-lock contention | Low | Low | Reuse single `%TEMP%\cap2voice\frame.png` serialized by OCR worker; optionally benchmark `pic_data` in **Task 2** if disk I/O > 10 ms |
| Missing CJK or Vietnamese diacritic glyphs in default `egui` font | High | High | Explicit composite Windows font loader (`Segoe UI` + `Microsoft YaHei`) built in **Task 10** and verified by real-PC screenshot |
| Orphaned Python `uvicorn` process after crash or Task Manager kill | High | Medium | Attach child process to Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) in **Task 6**; test both `Thoát` and `Stop-Process -Force` |
| Overlay dragged over subtitle box creates OCR feedback loop | Medium | High | Apply `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` in **Task 10** and verify live |

---

## 7. Open Questions

- All 12 specification assumptions in `docs/spec.md` §11 have been confirmed as-is by Danh via delegation to Muse.
- Two empirical build-time decisions will be resolved with measured real-PC data rather than guessing:
  1. **Task 2:** Confirm whether reused temp file `%TEMP%\cap2voice\frame.png` is fast enough or whether `pic_data` in-memory protobuf works reliably on WeChat 4.x.
  2. **Task 5 (Checkpoint 2):** Confirm whether `xcap` 0.9.10 meets `<= 50 ms` p95 on Danh's PC; if not, request approval to switch to `windows-capture` `2.0.1`.
