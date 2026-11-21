# cap2voice V1 — Run Guide

How to run cap2voice on Windows. Written from the real-PC verification (Task 14, 2026-11-21).

## 1. Prerequisites

| Requirement | Notes |
|---|---|
| Windows 10 2004+ / Windows 11 | `WDA_EXCLUDEFROMCAPTURE` needs Windows 10 2004+ |
| Weixin 4.x installed | e.g. `C:\Program Files\Tencent\Weixin\4.0.2.28`. The app auto-detects the latest version. |
| WeChat OCR plugin | `wxocr.dll` under `%APPDATA%\Tencent\xwechat\XPlugin\plugins\WeChatOcr\<id>\extracted\` (e.g. plugin `8022`). Auto-detected; override in Settings if needed. |
| Python 3.11+ with the server venv | The app starts its own Python server automatically; you never run Python manually. The server lives at `server/` in the install folder with a `.venv`. |
| Vietphrase data (29 MB, GPL-3.0) | Downloaded on first run if missing (`server/vietphrase/data/`). The server returns `503` with the fetch command until the data is present. Data is **not** shipped in the repo. |
| A video with hardcoded Chinese subtitles | Local player (ffplay/VLC/MPV) or a browser (Chrome/Edge). The app captures the subtitle strip from the screen — it does not read video files. |

## 2. First Run

1. Launch `cap2voice-app.exe`. The tray icon appears and the Python server starts automatically (no console window); it is healthy within ~10 s.
2. Click **Chọn vùng** (or press `Ctrl+Alt+R`). The screen dims; drag a rectangle around the subtitle strip of your video. Release to save. `Esc` cancels without changing the saved region.
3. Click **Bắt đầu** (or `Ctrl+Alt+S`). The overlay shows the Chinese line (white) above the Vietnamese translation (green). Translation appears within ~1 s of the subtitle changing.
4. Subtitle pairs are logged to `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log`, one line per pair:
   `[2026-11-21 21:45:12] 夜深了，雨还在下。 : Đêm đã khuya, mưa vẫn còn rơi.`

## 3. Controls

| Control | What it does |
|---|---|
| `Bắt đầu` / `Dừng` | Start / stop the pipeline (capture + OCR + translate + TTS + logging) |
| `Chọn vùng` | Re-select the subtitle region |
| Speaker icon | Mute / unmute TTS (hotkey `Ctrl+Alt+M`) |
| Voice dropdown | `vi-VN-NamMinhNeural` (male, default) / `vi-VN-HoaiMyNeural` (female) |
| `Cài đặt` (gear) | 5 tabs: Chung, Dịch, Giọng đọc, Hiển thị, Phím tắt. Edit, then `Lưu thay đổi` (applies immediately) or `Huỷ` (reverts the draft). |
| `X` | Hides the overlay to the tray (if close-to-tray is on); translation keeps running in the background |
| Tray menu | `Hiện cửa sổ`, `Bắt đầu dịch`, `Dừng`, `Cài đặt`, `Thoát` |
| `Ctrl+Alt+H` | Show / hide the overlay |
| `Ctrl+Alt+R` | Open the region selector |

## 4. Settings Worth Knowing

- **Region** (`[general.region]` in `config.toml`): the physical pixel rectangle that is captured. If you move the video window or change the player, re-select the region.
- **FPS** (default 10): capture rate. Lower it (5) on a slow machine; raise it (15) for fast dialogue.
- **Pixel-diff threshold** (default 0.02): frames that change less than this are not sent to OCR. On a paused video the app makes ~0 OCR calls (verified: 99.8% skip over 60 s of static frames).
- **Dedupe threshold** (default 0.93): a new OCR line that is ≥ 93% similar to the last emitted line is skipped. After 5 consecutive frames with no Chinese text, the memory resets so a repeated line is translated again.
- **Auto-read** (`Tự động đọc phụ đề`, default ON): when OFF, no TTS requests are made; translation, overlay and logging continue.
- **Autostart** (`Khởi động cùng Windows`, default OFF): creates/removes the `HKCU\...\Run\cap2voice` entry.
- **Server port** (default 18765): the local translation/TTS server on `127.0.0.1`.

## 5. Files

| File | Purpose |
|---|---|
| `%APPDATA%\cap2voice\config.toml` | All settings (region, voice, volume, font size, hotkeys, autostart). Corrupting it triggers a Vietnamese warning and a `config.toml.bak` is created. |
| `%APPDATA%\cap2voice\logs\subtitles-YYYY-MM-DD.log` | Daily subtitle log (UTF-8). |
| `%TEMP%\cap2voice\frame.png` | The last frame sent to OCR (reused temp file). Useful to check what the app is actually capturing. |
| `server/vietphrase/data/` | Vietphrase dictionaries (29 MB, GPL-3.0, not in git). |

## 6. Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| Overlay shows `Server dịch/TTS chưa sẵn sàng` | The Python server is still starting (wait ~10 s) or failed. Check Settings → `Kiểm tra kết nối`. |
| `Server dịch/TTS` 503 on first run | Vietphrase data missing. Run the fetch command the server message shows, then restart the app. |
| No translation, log not growing | The region is wrong (video moved / player changed). Press `Ctrl+Alt+R` and re-select. Check `%TEMP%\cap2voice\frame.png` — it shows exactly what the app captures. |
| Wrong / garbled text in the log | OCR misread a transitional frame (subtitle changing). The clean line follows within a second. If it persists, the region is cutting the text — make it taller. |
| Translation is stiff / literal | Vietphrase is an offline dictionary translator; it reads correctly but stiffly (`ngươi`, `tái nghị`). A better engine (e.g. DeepL) is a V2 candidate. |
| App exits but a `python.exe` stays behind | Should not happen (the server is tied to the app via a Job Object). Kill it with `Stop-Process -Name python -Force` and report it. |
| Hotkeys don't work when the app was launched from Task Scheduler | Known open question (2026-11-21): hotkeys work when launched by double-click / `hc exec` but did not fire in one Task Scheduler instance. Launch by double-click as a workaround. |

## 7. Known Limitations (V1)

- No network-disconnect banner test was run for V1 (skipped by Danh's decision, 2026-11-21). If the network drops, the overlay is designed to show `Mất kết nối mạng — không thể dịch` and keep the last Chinese line, but this was not verified live.
- DPI 125%/150% was not verified live; the app was only tested at 100% DPI.
- The mute button's audible effect and the HoaiMy voice switch were not verified by ear; they need a listening check on the PC.
- Translation quality is Vietphrase-level (readable, stiff). Sentence-boundary handling (splitting a sentence across subtitle changes) is a V2 item.
