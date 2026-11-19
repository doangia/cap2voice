# AGENTS.md — cap2voice

This file configures AI coding agents (Antigravity, OpenCode, Codex, Claude Code)
working on this repository. It is project-specific: the reusable workflow skills
come from the `agent-skills` plugin (Addy Osmani), they are NOT pasted in here.
Read the skill when its intent matches; do not re-implement its steps from memory.

User-facing communication is in Vietnamese. Code, comments, and technical docs are
in English. The user (Danh) is the orchestrator and approves every gate below.

## Project overview

cap2voice is a Windows desktop app that watches videos with hardcoded Chinese
subtitles, OCRs the subtitle region in real time, translates it to Vietnamese,
shows both lines (Chinese above, Vietnamese below) on a floating overlay, and
reads the Vietnamese line aloud (TTS). It works with any video source (local
file, player, browser video) because it only observes the screen — it is NOT a
video player and never touches video files or the browser.

Pipeline: select subtitle region → capture region at ~10 fps → skip unchanged
frames (pixel diff) → OCR with wcocr-rs (WeChat mmmojo engine, local) → keep
Chinese text only → fuzzy dedupe vs previous line → translate zh→vi →
show 2 lines on overlay → TTS the Vietnamese line → log each `Chinese : Vietnamese`
pair to file.

## Architecture

```text
cap2voice/
├── brief.md              # project vision + decisions (source of truth for intent)
├── docs/
│   ├── spec.md           # written from brief.md, user-approved (Step 1 gate)
│   └── mockups/          # UI reference images (3 screens)
├── crates/
│   ├── cap2voice-app/    # egui app: UI, capture, pipeline, tray
│   └── wcocr-rs/         # WeChat OCR as a library crate (refactor from C ABI first)
├── server/               # TEMPORARY Python API server (localhost)
│   ├── server.py         # POST /translate, POST /tts, GET /health
│   └── requirements.txt
├── tasks/                # plan + todo (created by planning skill)
├── CONSTRAINTS.md        # quality bar (created by constraints skill)
└── AGENTS.md             # this file
```

- UI: `eframe` 0.37 (egui), borderless always-on-top overlay window.
- Capture: `xcap` 0.9.10; fallback `windows-capture` 2.0.1 if xcap is too slow on the real PC.
- OCR: `wcocr-rs` (internal crate). Current code exports a C ABI
  (`wechat_ocr` / `stop_ocr`); the first refactor is to use it as a direct Rust crate.
- HTTP: `reqwest` 0.13 (Rust app → Python server). Audio: `rodio` 0.23.
- Python server (temporary, later replaced by native Rust): FastAPI,
  `POST /translate` (Google Translate, zh→vi), `POST /tts` (Edge-TTS → mp3,
  default voice `vi-VN-NamMinhNeural`). The Rust app starts/stops the server
  itself; the user never runs it by hand.
- No crate `edge-tts` exists: native Rust TTS later means calling the Edge-TTS
  websocket protocol directly.

## Workflow — mandatory, spec-first

Every task runs the lifecycle. Never write code before the spec and plan gates pass.

| Phase | Skill (Antigravity) | Skill (OpenCode) | Gate / output |
|---|---|---|---|
| Interview | `/agent-skills:interview-me` | `interview-me` | Stop at 95% confidence; only an explicit "yes" counts |
| Spec | `/agent-skills:spec-driven-development` | `spec-driven-development` | `docs/spec.md` with acceptance criteria per feature; user approves |
| Constraints | `/agent-skills:constraint-driven-development` | `constraint-driven-development` | `CONSTRAINTS.md` (quality bar); do not ship below it |
| Plan | `/agent-skills:planning-and-task-breakdown` | `planning-and-task-breakdown` | `tasks/plan.md` + `tasks/todo.md`; each task ≤ ~5 files, with "done means" + how to verify on the real PC |
| Build | `/agent-skills:incremental-implementation` + `/agent-skills:test-driven-development` | `incremental-implementation` + `test-driven-development` | One thin slice at a time, RED→GREEN→REFACTOR, one commit per slice |
| Debug | `/agent-skills:debugging-and-error-recovery` | `debugging-and-error-recovery` | Stop-the-line: no new features while a failure is open |
| Test | `/agent-skills:test-driven-development` | `test-driven-development` | See Verification below |
| Review | `/agent-skills:code-review-and-quality` | `code-review-and-quality` | 5-axis review; all Critical/Required findings resolved |
| Ship | `/agent-skills:shipping-and-launch` | `shipping-and-launch` | GO/NO-GO + rollback plan; user gives the final GO |

- Antigravity: invoke skills by their namespaced form
  (`/agent-skills:spec-driven-development`). Short wrappers (`/spec`, `/build`)
  may not resolve in the slash catalog; do not rely on them.
- OpenCode has no slash commands: the agent maps intent to skills internally
  and must follow the same lifecycle (DEFINE → PLAN → BUILD → VERIFY → REVIEW
  → SHIP). "Too small for a skill" is not a reason to skip the workflow.
- Skills are the how, personas are the who, the user is the orchestrator.
  Personas never invoke other personas. The only multi-persona pattern allowed
  is parallel fan-out with a merge step (used by ship: code-reviewer,
  security-auditor, test-engineer in parallel, then one merged report).
- If a step contradicts this file, the skill wins for its own phase; this file
  wins on project scope, safety rules, and verification.

## Non-negotiable rules

- **Verify on the real PC with real data.** Build on the VM first, then test
  on the PC with a real video that has hardcoded Chinese subtitles. Never
  report a task done based on the builder's word — check each spec point
  yourself and record the result in the test log.
- **No publish / push / deploy / delete without an explicit "go"** from the
  user for that exact action. One "go" covers only that action, not the next.
- **Do not modify original files on the PC before approval.** Proposals that
  touch real files go out as a separate draft with a reason per change, for
  veto. New files (spec, plan, logs) are fine to write.
- **WeChat OCR engine is personal-use only.** It runs through the Weixin
  install on the PC (`C:\Program Files\Tencent\Weixin\4.0.2.28`, OCR plugin
  8022). Never distribute the engine or its binaries.
- **Network failures are reported, never silent.** Google Translate (free
  endpoint) and Edge-TTS need network. If the network is down, the app must
  say so clearly instead of silently not translating.
- **Do not build out-of-MVP scope in the first round:** local translation
  model (`Helsinki-NLP/opus-mt-zh-vi`), native Rust TTS, pinyin on the Chinese
  line, click-to-replay Chinese audio, translation history table, browser
  extension. Those are recorded in `brief.md` §8 for later.

## Build, test, verify

```bash
cargo test                          # Rust workspace tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
# Python server (if tests exist)
python -m pytest server/
```

- Every ticket states "done means" and how to verify it on the real PC.
  Run the check, paste the actual output into the test log; a passing claim
  without output does not count.
- UI checks need a screenshot or a live run on the PC — a successful compile
  is not a UI verification.
- Known risks to verify during build (from `brief.md` §9): xcap capture speed
  on the real PC; Google Translate quality on short spoken subtitle lines;
  Edge-TTS websocket protocol (native Rust step only); free Translate endpoint
  rate limits; tray-icon crate behavior and close-to-tray on Windows.

## Conventions

- Rust 2021 edition, `cargo fmt` clean, `clippy -D warnings` clean.
- Small commits, one slice per commit, message in English (`feat:`, `fix:`,
  `test:`, `docs:`). No secrets, API keys, or credentials in the repo — ever.
- Overlay text: Chinese line white, Vietnamese line green (see
  `docs/mockups/`). Settings window: sidebar Chung / Dịch / Giọng đọc /
  Hiển thị / Phím tắt. Tray right-click menu: Hiện cửa sổ / Bắt đầu dịch /
  Dừng / Cài đặt / Thoát. Closing the window minimizes to tray and keeps
  translating in the background; only Thoát quits.
- Log format: one `Chinese : Vietnamese` pair per line, with timestamp.
