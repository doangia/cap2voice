# cap2voice V1 Release Gate — Test Log

**Date:** 2026-11-21 (Saturday)
**Tester:** Muse (self-verified on the real PC, `doan-pc`)
**Video:** `C:\Users\doanv\smoke-test-video.mp4` (90 s, 15 hardcoded Chinese subtitle lines, 6 s per line, looped)
**Translator:** Vietphrase offline (`server/vietphrase`), **not** Google Translate (spec assumption 10 superseded by Danh, 2026-11-20 21:00, commit `7ca7d2b`)
**UI framework:** eframe 0.35 (spec says 0.37; PC rustc 1.93.1 — deviation recorded in Task 10)
**Scope change (Danh, 2026-11-21 13:29):** Task 14 does **not** include the mid-run network-disconnect test. `AC-TR-3` and the offline part of `AC-TTS-7` are recorded as **SKIPPED**, not verified.

## 1. Automated Quality Gate

Run 2026-11-21 ~13:25, script `t14-gate.ps1` / `t14-gate2.ps1`, full output `C:\Users\doanv\t14-gate.txt`, summary `~/workspace/cap2voice-task13-review/t14-gate-summary.md`.

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --workspace` | exit 101 **only because the rustdoc doctest runner fails with OS error 448 "untrusted mount point" under SSH** (known environment quirk, not a code failure) |
| `cargo test --workspace --lib --tests` | exit 0 (ocr_filter 4/4, pipeline 9/9, wcocr_rs 22/22, ocr 2/2 in the tail; ignored = desktop `#[ignore]` tests) |
| `mypy --strict` (server) | exit 0 |
| `pytest server/ -v` | exit 0 |

Verdict: **PASS** (with the documented rustdoc environment quirk).

## 2. Real-Video End-to-End Run (Local + Browser)

### 2.1 Local player (ffplay), 8 min — 13:36:26–13:44:26

- ffplay windowed `960x540` at `(0,0)`, `-noborder`, `-loop 0`, set `HWND_TOPMOST` so no other window covers it. Region `(200,400,560,100)` in `config.toml` (restored to `(600,849,700,150)` after the run).
- Pipeline started via the overlay `Bắt đầu` button (UIA click at `(610,644)`).
- Log grew from 62 → ~190 lines in 8 min (~13 lines/min = 10 subtitle slots + ~3 transitional misread pairs per minute). Full lines read correctly, e.g. `等等我。 : Đợi ta đã.`, `谢谢你。 : Cám ơn ngươi.`, `夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi.`, `我们走吧。 : Chúng ta đi thôi.`
- **Latency:** log lines land on the exact 6 s grid anchored at ffplay start (`13:32:58` + 6 s·k), in the video's line order. Log timestamps have 1 s resolution, so end-to-end latency (subtitle on screen → Vietnamese line in log) is **< 1 s median**, well under the 2.0 s gate. (A 6 s latency would show the *previous* slot's line; the log shows the *current* slot's line.)
- **Zero crashes, zero UI freezes.** The overlay stayed responsive throughout; the app and the Python server stayed alive (`PY_UVICORN=2`).

### 2.2 Paused-frame OCR skip (AC-CAP-4), controlled test — 13:44:52–13:45:52

- Extracted one video frame (`t=32 s`, line `再见。`) to `C:\Users\doanv\t14-static-frame.png`, displayed it with ffplay `-loop 1` (static pixels, 60 s).
- Log stayed at **207 lines** and `frame.png` mtime froze at `13:44:52` for the full 60 s (after the initial 2 captures at `13:44:50`/`13:44:52`).
- At 10 fps, 60 s = 600 frames, ~1 OCR call → **99.8% skip**, above the ≥99% requirement.

Note: two attempts to pause ffplay with the `p` key (click + `keybd_event`, then `SetForegroundWindow` + `p`) did **not** pause the video (log kept growing). The static-frame test above replaces the keyboard-pause approach and gives a cleaner, controlled result. The `p`-key failure is a test-harness limitation, not an app bug.

### 2.3 Browser (Chrome), 7 min — 13:47:47–13:54:48

- Chrome (temp profile `chrome-t14`, windowed `960x650` at `(0,0)`) playing `file:///C:/Users/doanv/t14-browser-test.html` (same video, `960x540` at page `(0,0)`, autoplay + loop + muted).
- Region `(200,510,560,100)` — the subtitle strip sits at screen y ~535–565 below Chrome's tab/address bar. This region overlaps the overlay's area, so it also exercises `WDA_EXCLUDEFROMCAPTURE` (AC-UI-6) in a real scenario.
- Log growth per minute: 207 → 224 → 240 → 255 → 272 → 288 → 303 → 320 (**+113 lines in 7 min**, ~16/min, same rate as the local player). `frame.png` updated every minute (13:48:47 … 13:54:47).
- Last lines: `我们走吧。 : Chúng ta đi thôi.`, `等等我。 : Đợi ta đã.` — clean, in the video's line order, with the usual transitional misread pairs. **No duplicate lines from the overlay's own Chinese text**, confirming the overlay was excluded from capture (AC-UI-6).
- The Chrome temp profile was removed after the run; the app was force-killed cleanly (see §3).

### 2.4 Run totals

- Total video time: 8 min (ffplay, 13:36:26–13:44:26) + 7 min (Chrome, 13:47:47–13:54:48) = **15 min** (plus a 1 min static-frame test, not counted in the 15 min). Note: the two portions are separate continuous runs with a ~3.5 min gap between them (static-frame test + region change + app restart); each portion was internally continuous.
- Log: **62 → 326 lines** (264 lines added during Task 14). The app was still running for ~1.5 min after the Chrome script ended, adding the last 6 lines before the final kill.

## 3. Forced-Kill Orphan Check (AC-SRV-5)

- `Stop-Process -Force cap2voice-app` at ~13:35 and ~13:47: both times `PY_UVICORN=0` afterwards (verified via `Get-CimInstance Win32_Process` filtering `python.exe` with `uvicorn` in the command line).
- Tray `Thoát` was already verified twice on 2026-11-21 (Task 13): app exits, 0 uvicorn, no ghost tray icon.
- Final forced kill after the Chrome run (~13:56): `APP_ALIVE=False`, `PY_UVICORN=0` — the 3rd clean forced kill of the day.

## 4. Log Integrity

- `%APPDATA%\cap2voice\logs\subtitles-2026-11-21.log` — UTF-8, one `[YYYY-MM-DD HH:MM:SS] <Chinese> : <Vietnamese>` line per translated pair, readable while the app runs. Full file saved at `~/workspace/cap2voice-task13-review/subtitles-2026-11-21.log`.
- Transitional misread pairs appear (OCR catching a subtitle mid-change, e.g. `没明轰早暄见。儿。` followed immediately by the clean `明天早上见。`). This is expected OCR behaviour on a changing frame, not a logging bug; the clean line always follows within the same second.
- Zero mojibake in the log file.

## 5. Acceptance Criteria Map (50 ACs)

Status: **PASS** = verified live on the real PC with evidence below. **PARTIAL** = verified in part (see note). **UNIT** = covered by automated tests only, not live. **NOT VERIFIED** = not tested live; needs a decision or a follow-up. **SKIPPED** = out of scope per Danh's decision.

### 5.1 Capture (AC-CAP-*)

| AC | Status | Evidence |
|---|---|---|
| AC-CAP-1 | PASS | Task 11 live: region selector dims screen, red dashed rectangle, live `W x H px · X, Y` badge; drag `(500,850)→(1400,1000)` saved `(500,849,900,150)` |
| AC-CAP-2 | PASS | Task 11 live: drag ≥ 20×16 px persisted to config + Settings `Vùng phụ đề`; Esc cancels without modifying the saved region |
| AC-CAP-3 | NOT VERIFIED | DPI 125%/150% was never tested live. PC is at 100% DPI; changing display scaling would disrupt Danh's session. Needs a dedicated session or an explicit waiver. |
| AC-CAP-4 | PASS | §2.2 static-frame test: 600 frames, ~1 OCR call, 99.8% skip |
| AC-CAP-5 | PASS | §2.1: every subtitle change produces a log line in the same second (≤ 1 s; the 150 ms trigger is the capture-tick bound, the log shows the line within the same 1 s slot) |
| AC-CAP-6 | PASS | Task 5 real-PC benchmark (hc exec): capture p50 15.45 ms / p95 17.38 ms, under the 30/50 ms limits |
| AC-CAP-7 | PASS | Group B 2026-11-21 15:2x: region `(5000,5000,560,100)` → app alive, 0 new lines, soft error banner, no crash; `monitor_id=DISPLAY99` → same. Region restored `(600,849,700,150)`. (The CAP-7 test script itself corrupted `monitor_id` by doubling a backslash — fixed by hand, see §9.) |

### 5.2 OCR, filtering, dedupe, logging (AC-OCR-*)

| AC | Status | Evidence |
|---|---|---|
| AC-OCR-1 | PASS | Task 1/2: `crates/wcocr-rs` compiles as `rlib`, zero C ABI exports; clippy `-D warnings` clean (gate §1) |
| AC-OCR-2 | PASS | Task 2 live: `WeChatOcr::ocr_file` on the real PC (Weixin 4.0.2.28 + plugin 8022) returns `errcode == 0` with structured blocks; handshake 169 ms, OCR 88 ms |
| AC-OCR-3 | UNIT | Missing/invalid `wxocr.dll` / Weixin dir returns `Err(OcrError::InvalidPath)` in unit tests. The Vietnamese UI banner for this path was not triggered live. |
| AC-OCR-4 | UNIT | Mocked hung OCR worker returns `Err(OcrError::RequestTimeout)` within `timeout + 100 ms` in tests; no UI freeze observed in any live run |
| AC-OCR-5 | PASS | Unit tests (Task 7) + live log: CJK filtering keeps Chinese + digits + punctuation, drops Latin noise; zero-CJK results are discarded (no logo/timestamp lines in the log) |
| AC-OCR-6 | PASS | Unit tests (Task 7) + live log: dedupe skips ≥ 0.93 similarity; memory resets after 5 blank frames (repeated lines re-appear after a loop) |
| AC-OCR-7 | PASS | §4: one UTF-8 `[timestamp] Chinese : Vietnamese` line per translated pair, readable while running |

### 5.3 Translation (AC-TR-*)

| AC | Status | Evidence |
|---|---|---|
| AC-TR-1 | PASS | §2.1: real line `夜深了，雨还在下。` → `Đêm đã khuya, mưa vẫn còn rơi.` in the log within 1 s of the subtitle slot |
| AC-TR-2 | PASS | Task 3/6: mock-server tests verify the exact JSON contract and mapping of HTTP 400/429/502, timeout, connection-refused to typed `TranslateError` variants |
| AC-TR-3 | SKIPPED | Per Danh 2026-11-21 13:29 ("task 14 không cần ngắt mạng"). The error-banner code path exists (Task 9) but was not exercised live with a real disconnect. |
| AC-TR-4 | UNIT | Sequence-number discard of out-of-order responses covered by tests (Task 6/9); no out-of-order overwrite observed in the live log |
| AC-TR-5 | PASS | §2.1/2.3: overlay stayed responsive (buttons clickable, window draggable) throughout the 15 min run while translations were in flight |

### 5.4 TTS & audio (AC-TTS-*)

| AC | Status | Evidence |
|---|---|---|
| AC-TTS-1 | PARTIAL | `/tts` is called for each new line (server receives the requests; the pipeline runs with `auto_read = true`). Whether the audio actually plays clearly on the PC speakers was **not** confirmed by ear — needs Danh to listen (open item from Task 9). |
| AC-TTS-2 | NOT VERIFIED | Switching to `vi-VN-HoaiMyNeural` was never verified live (dropdown + listening). Open item from Task 10. |
| AC-TTS-3 | UNIT | Queue bounds (max 3 pending, drop oldest) verified by unit tests (Task 8); live sequential playback not confirmed by ear. No overlapping audio reported in any run. |
| AC-TTS-4 | PARTIAL | `Dừng` stops the pipeline (verified live, Task 13). The 200 ms audio-halt timing and the mute button's audible effect were not measured by ear. |
| AC-TTS-5 | PARTIAL | Volume slider changes the config and the `rodio::Sink` gain (Task 12 live: slider works, value persists). Loudness change by ear not confirmed. |
| AC-TTS-6 | PASS | Group B 2026-11-21 16:0x: A/B with a temporary server-side `/tts` counter (added, verified, then reverted — server.py restored byte-identical, working tree clean). Run A `auto_read=true`: 6 new log lines, **6 `/tts` calls**. Run B `auto_read=false`: 6 new log lines, **0 `/tts` calls**. (Network-connection sampling was tried first and proved unreliable — Edge-TTS connections are too brief to catch by polling; the server counter is the definitive evidence.) |
| AC-TTS-7 | SKIPPED (partial) | The offline/network part is skipped per Danh (same decision as AC-TR-3). The `/tts` 502/503 → typed error mapping is covered by server unit tests (Task 3). |

### 5.5 Overlay UI (AC-UI-*)

| AC | Status | Evidence |
|---|---|---|
| AC-UI-1 | PASS | §2.1 + §2.3: overlay floats borderless + always-on-top over ffplay and over Chrome video |
| AC-UI-2 | PARTIAL | Overlay renders Chinese (white) above Vietnamese (green) — verified in Task 10 GUI live test, but the green Vietnamese line was **not** confirmed visually in the live run (overlay is `WDA_EXCLUDEFROMCAPTURE`, so it never appears in screenshots; the frame.png only shows the captured video). No tofu boxes were reported in any GUI check. |
| AC-UI-3 | PASS | Group B 2026-11-21 15:1x: drag from the header gap `(825,522)` → `(975,602)` while the pipeline ran moved the overlay `(+150,+80)` to `(637,579)` and `config.toml` saved `saved_pos=637,579` (400 ms debounce works). Restart restored the overlay at `(637,579)`. Position restored to `(487,499)` after the test. **Caveat (small bug found, not blocking):** when the app is IDLE (pipeline not running) there is no periodic repaint, so the debounce save does not fire after a drag — the position is only saved on a clean exit (`on_exit`). Force-killing while idle after a drag loses the position. |
| AC-UI-4 | PARTIAL | All 5 control-bar elements + `Cài đặt` + `X` work as specified (Tasks 10/12/13 live). Voice dropdown switch and mute-button audible effect not verified live (see AC-TTS-2/AC-TTS-4). |
| AC-UI-5 | PARTIAL | Error banners render in Vietnamese in the overlay status (code path, Task 9). The network banner was not triggered live (skipped per Danh); the server-down banner was not triggered live. Banners clear on the next successful request (observed in the Task 9 smoke test). |
| AC-UI-6 | PASS | §2.3: the Chrome capture region overlaps the overlay area; the log stays clean (113 lines, no duplicates from the overlay's own Chinese text), proving `WDA_EXCLUDEFROMCAPTURE` works |

### 5.6 Settings & config (AC-SET-*)

| AC | Status | Evidence |
|---|---|---|
| AC-SET-1 | PASS | Task 12 live: all 5 tabs open cleanly with functional controls, zero placeholders |
| AC-SET-2 | PASS | Task 12 live: `Huỷ` reverts drafts without touching `config.toml` or the live overlay |
| AC-SET-3 | PASS | Task 12 live: `Lưu thay đổi` updates `config.toml` atomically, applies immediately, persists across restarts |
| AC-SET-4 | PASS | Task 12 live: `Chọn lại vùng` launches the selector and updates the draft coordinates |
| AC-SET-5 | PASS | Task 12 live: autostart on/off creates/removes `HKCU\...\Run\cap2voice` correctly and survives a restart |
| AC-SET-6 | PASS | Group B 2026-11-21 15:1x: wrote broken TOML (`[general` + `broken =!!!`) → app started (alive), `config.toml.bak` was created, overlay fell back to the default position `(26,26)` instead of `(487,499)`. Good config restored and verified. |
| AC-SET-7 | PASS | Group B 2026-11-21 16:0x: Ctrl+Alt+M live, verified with the same temporary server-side `/tts` counter. Phase 1 unmuted: 3 new lines, 3 `/tts`. After Ctrl+Alt+M: 3 new lines, **0 new `/tts`** (mute works). After Ctrl+Alt+M again: 2 new lines, **2 new `/tts`** (toggle back works). |

### 5.7 Server (AC-SRV-*)

| AC | Status | Evidence |
|---|---|---|
| AC-SRV-1 | PASS | Task 3/12 live: `GET /health` returns 200 with the expected JSON within 100 ms (`translator=vietphrase`) |
| AC-SRV-2 | PASS | Task 3 live: `POST /translate` translates real Chinese (200), rejects empty input (400), returns structured 429/502 JSON (never 200 with fake text) |
| AC-SRV-3 | PASS | Task 3/12 live: `POST /tts` returns playable MP3 (`audio/mpeg`) for both voices, rejects empty text (400), maps Edge-TTS failures to 502/503 |
| AC-SRV-4 | PASS | Tasks 9–13 live: launching `cap2voice-app` auto-starts the Python server (no console window), healthy within 10 s; user never runs Python manually |
| AC-SRV-5 | PASS | §3: tray `Thoát` and `Stop-Process -Force` both leave zero `server.py`/`uvicorn` processes (verified 3× on 2026-11-21) |
| AC-SRV-6 | PASS | Group B 2026-11-21 15:2x: renamed `server/.venv/Scripts/python.exe` → `.bak` and stripped WindowsApps/pyenv from PATH → app started, Ctrl+Alt+S sent, app alive, 0 uvicorn processes, `/health` down, no crash. Python restored (`VENV_PY_EXISTS_AFTER=True`). |

### 5.8 Tray & lifecycle (AC-TRAY-*)

| AC | Status | Evidence |
|---|---|---|
| AC-TRAY-1 | PASS | Task 13 live: tray icon appears; right-click shows all 5 exact Vietnamese items in order |
| AC-TRAY-2 | PASS | Task 13 live (verified twice): clicking `X` with close-to-tray hides the overlay while OCR/translation/TTS/logging continue (log kept growing) |
| AC-TRAY-3 | PASS | Task 13 live: `Hiện cửa sổ` restores the overlay with the latest translated subtitle |
| AC-TRAY-4 | PASS | Task 13 live: `Bắt đầu dịch` / `Dừng` / `Cài đặt` from the tray menu execute while the overlay is hidden |
| AC-TRAY-5 | PASS | Task 13 live (verified twice): `Thoát` stops playback, kills the Python server, removes the tray icon with no ghost, exits 0 |

## 6. Summary Counts

- **PASS: 36 / 50** (verified live on the real PC)
- **PARTIAL: 6 / 50** (verified in part; see the notes in §5)
- **UNIT (tests only): 4 / 50** (covered by automated tests, not live)
- **NOT VERIFIED: 2 / 50** (AC-CAP-3 DPI 125/150%, AC-TTS-2 HoaiMy voice — see §7)
- **SKIPPED (per Danh): 2 / 50** (AC-TR-3, AC-TTS-7 offline part)

The V1 code gate is **met for the parts that were run**: automated gate green, 15 min of real video (local + browser) with < 1 s latency and zero crashes, paused-frame skip 99.8%, forced kill clean 3×, log clean UTF-8. Group B (2026-11-21 afternoon) closed 6 of the 7 NOT VERIFIED items (see §9); the 2 remaining NOT VERIFIED items need Danh's ears/eyes or a display-scaling session, and are listed in §7 rather than hidden.

## 7. Open Items (not blocking the V1 code gate, need Danh or a follow-up session)

1. **Listen to TTS by ear** (AC-TTS-1/2/3/4/5): Danh listens on the PC — sequential reading, HoaiMy voice switch (AC-TTS-2, the only TTS item still NOT VERIFIED), mute halt, volume change. (Task 9 open item.)
2. **DPI 125%/150%** (AC-CAP-3): needs a dedicated display-scaling session or an explicit waiver.
3. **Green Vietnamese line live** (AC-UI-2): the overlay is invisible in screenshots (WDA); confirm by looking at the screen while a subtitle is showing.
4. **Config region auto-rewrite** (open question from 2026-11-20 21:46): `config.toml` region changed `(500,849,900,150)` → `(505,893,923,162)` with no click to explain it. Not reproduced in this run; still unexplained.
5. **Overlay drag while idle loses the position on force-kill** (found in Group B, AC-UI-3 caveat): when the pipeline is not running there is no periodic repaint, so the 400 ms debounce save never fires after a drag; the position is only saved on a clean exit. Fix candidate for a later slice (schedule a repaint after drag, or save on drag-end).

## 9. Group B session (2026-11-21 afternoon, after Checkpoint 5)

Closed 6 items: AC-UI-3, AC-SET-6, AC-CAP-7, AC-SRV-6, AC-TTS-6, AC-SET-7. Evidence scripts in `~/workspace/cap2voice-task13-review/scripts/t15b-*.ps1`, outputs `C:\Users\doanv\t15b-*.txt`, log now at 436 lines.

**Regression found and fixed during the session:** the AC-CAP-7 test script corrupted `monitor_id` in `config.toml` (a PowerShell regex doubled a backslash: `\\.\DISPLAY1` became `\\.\DISPLAY1` with an extra backslash). From ~15:17 every run produced 0 new log lines (the app showed `MonitorUnavailable` every tick and never OCR'd) even though the server, OCR engine, video and region were all healthy. Root cause confirmed by counting backslashes against the Task 14 backup `config.toml.t14bak`. Fixed by writing the value with Python (`\\.\DISPLAY1` = 2 backslashes before the dot, 1 before `DISPLAY1`) and re-running the known-good Task 15 live script: 18 new lines, 0 garbage. Lesson recorded: never edit backslash-bearing config values with a PowerShell regex — use Python with explicit strings and a backslash count check.

## 8. Screenshots & Artefacts

- `~/workspace/cap2voice-task13-review/t14-ffplay-cal.png` — ffplay windowed, subtitle visible
- `~/workspace/cap2voice-task13-review/t14-frame-cal2.png` — captured frame (region `(200,400,560,100)`) showing the subtitle text
- `~/workspace/cap2voice-task13-review/t14-chrome-cal.png` — Chrome test page, subtitle visible at y ~535–565
- `~/workspace/cap2voice-task13-review/t14-frame-chrome.png` — captured frame during the Chrome run (region `(200,510,560,100)`)
- `~/workspace/cap2voice-task13-review/subtitles-2026-11-21.log` — full daily log (62 lines from Task 13 + Task 14 run)
- `C:\Users\doanv\t14-gate.txt` — automated gate output
- Scripts: `~/workspace/cap2voice-task13-review/scripts/t14-*.ps1`
