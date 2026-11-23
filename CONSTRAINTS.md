# Constraints — cap2voice (V1 MVP)

> **Last reviewed:** 2026-11-23 by Muse (acting for Danh per delegation; D1+A5: Python server removed, rows updated)  
> **Source spec:** `docs/spec.md` (V1 approved)  
> **Workflow contract:** `AGENTS.md`  
> **Platform scope:** Windows 10/11 native desktop app (Rust `eframe`/egui, native in-process translation + TTS; no Python at runtime). Web-only dimensions (Lighthouse, axe-core browser URL audits) are intentionally omitted because `cap2voice` has no web UI URL.

---

## 1. Floor (Always Enforced, Zero Setup Required)

Every change in this repository must obey the floor. Violating any rule below blocks the task immediately:

- **No new checker suppressions:** No new `#[allow(clippy::...)]`, `#[allow(warnings)]`, `#[allow(dead_code)]`, `# noqa`, `# type: ignore`, `gitleaks:allow`, or `nosemgrep` without a tracked row in the Exceptions table below.
- **No unimplemented stubs or silent error swallowing:** No `todo!()`, `unimplemented!()`, `panic!("not implemented")`, `.unwrap()` / `.expect(...)` in non-test production paths, `except Exception: pass`, bare `except:`, or `Err(_) => {}` that hides network, OCR, TTS, audio, or config failures. (The `.unwrap()` / `.expect(` ban is enforced in CI by `scripts/check_no_unwrap.py`, job `no-unwrap`.)
- **No weakened, skipped, or deleted tests:** No `@pytest.mark.skip`, no `#[ignore]` except on explicitly labeled real-PC hardware/WeChat tests, no deleted test files, and no removed assertions from surviving tests without an explicit reason in the commit message and user approval.
- **No self-answering tests:** Tests must execute real business logic and assert concrete values (outputs, serialized bytes, HTTP status/bodies, file contents) — never mock returning `X` and immediately assert `X`.
- **No secrets or WeChat OCR binaries in source:** Never commit API keys, credentials, `.env`, `wxocr.dll`, `mmmojo*.dll`, `WeChatOCR*`, or `Weixin/` files. WeChat OCR is strictly personal-use loaded at runtime from the local PC installation.
- **No out-of-MVP scope creep:** Never implement V2+ deferred items (`docs/spec.md` §10: history table UI, browser extension, WeChat 3.x — Helsinki model, native Rust Edge-TTS, and pinyin moved out of §10 when Phase 2 delivered them) inside V1 tasks.
- **No weakening of this file:** `CONSTRAINTS.md` must never have a threshold lowered, a rule removed, or an unapproved exception added to make a failing change pass.

---

## 2. Enforced with Numbers

Every row below names a concrete, executable command verified against the toolchain installed on this Windows PC (`rustc 1.93.1`, `clippy 0.1.93`, `rustfmt 1.8.0`) and `.github/workflows/ci.yml`.

> **SSH note:** Over SSH on Windows, prepend `C:\Users\doanv\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin` to `PATH` before running `cargo` commands.

| Dimension | Rule | Rationale | Checked By | Runs At | Opinion Source |
|---|---|---|---|---|---|
| Rust formatting | `<= 0` formatting diffs | Eliminates noisy diffs and matches CI gate | `cargo fmt --all -- --check` | every edit, task end, CI | External (`rustfmt`) |
| Rust lint & safety | `<= 0` compiler or clippy warnings | Catches bug patterns, dead code, and type/ownership pitfalls before runtime | `cargo clippy --workspace --all-targets -- -D warnings` | every edit, task end, CI | External (`rustc` + `clippy`) |
| Rust unit & integration suite | `>= 100%` tests passing (`<= 0` failures) | Verifies pixel-diff, CJK filter, dedupe, config, audio queue, native engine error mapping, pipeline | `cargo test --workspace` | task end, CI | Project suite |
| Forbidden WeChat binaries & secrets in git tree | `<= 0` matched files or leaked secret patterns | Prevents redistributing proprietary WeChat DLLs or committing credentials | `git ls-files` check against `(mmmojo\|wxocr\|WeChatOCR\|\.env\|\.pem\|\.key)` + diff scan | task end, review | External (repo policy) |
| Real-PC capture latency | `<= 30 ms` median and `<= 50 ms` p95 per cropped frame (100 frames) | Leaves `>= 50 ms` headroom within a `100 ms` (10 fps) frame budget; triggers `windows-capture` fallback if breached | `cargo test -p cap2voice-app --test capture_bench -- --ignored --nocapture` | Task 5 gate, review, ship | External (real hardware) |
| Real-PC OCR request timeout bound | `<= 3100 ms` worst-case timeout return (`3000 ms` configured timeout) | Guarantees a stalled WeChat mmmojo worker can never deadlock the pipeline or UI | `cargo test -p wcocr-rs timeout` | task end, CI | Project suite |
| Zero orphaned app processes | `<= 0` lingering `cap2voice-app.exe` processes after exit or forced kill | Prevents zombie app processes holding capture/overlay resources or leaking RAM on Windows (no child processes exist anymore — D1+A5 removed the Python server) | `Get-CimInstance Win32_Process` check | Task 13 gate, ship | External (Windows OS) |
| End-to-end subtitle latency (real video) | `<= 2000 ms` median from subtitle appearance to Vietnamese overlay display | Keeps subtitles readable alongside live spoken dialogue | Real-PC timed run on hardcoded-subtitle video recorded in `docs/test-log-v1.md` | Checkpoint 3, Task 14 (ship) | External (real video) |
| Paused-frame OCR suppression | `>= 99%` static frames skipped after frame 1 | Prevents unnecessary CPU/OCR load and Google Translate bursts on paused video | `cargo test -p cap2voice-app pixel_diff` + real-PC paused-video log count | Task 5 gate, Task 14 (ship) | Project suite + real PC |

---

## 3. Lifecycle Placement & Time Budgets

Do not stall the edit loop with slow checks. Run each check at the cheapest stage that still catches regressions promptly:

| Phase | What Runs | Target Budget | Behavior on Failure |
|---|---|---|---|
| **BUILD (per edit)** | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, floor scan on changed files | `<= 15 s` | **Block** — fix before continuing |
| **VERIFY (task end)** | BUILD checks + `cargo test --workspace` + task-specific real-PC check when required by `tasks/todo.md` | `<= 90 s` (automated); real-PC run as needed | **Block** — task cannot be checked off |
| **REVIEW (checkpoint / PR)** | VERIFY checks + 5-move floor guard review on full branch diff (`CONSTRAINTS.md` diff, deleted/weakened tests, new suppressions, stubs, unapproved exceptions) + real-PC UI screenshots | Minutes | **Block** on any Critical/Required finding |
| **SHIP (V1 release)** | Full CI green (`.github/workflows/ci.yml`) + 15-min real-video E2E run (local player + browser) + network-offline failure test + orphan-process forced-kill test | Pre-release gate | **NO-GO** until all pass and user gives explicit GO |

---

## 4. Measured, Not Yet Enforced (Ratchets)

Baselines below were recorded as the project grew; tighten into §2 once tooling is installed — never let a recorded metric regress beyond `0.5%` tolerance:

| Metric | Today (Baseline) | Target / Direction | Planned Checker (Install Needed) |
|---|---|---|---|
| Rust changed-line test coverage | `TBD (measure after Task 4)` | `>= 80%` on changed lines; project coverage must not fall | `cargo llvm-cov --workspace --lcov` (`cargo install cargo-llvm-cov`) |
| Rust dependency advisories | `TBD (measure after Task 4)` | `<= 0` high/critical advisories | `cargo audit` (`cargo install cargo-audit`) or `osv-scanner` in CI |
| Secret scanning (full tree) | `TBD (CI adoption)` | `<= 0` findings (always with `--redact`) | `gitleaks dir . --redact --no-banner` (CI or local install) |
| Release binary size (`cap2voice-app.exe`) | `TBD (measure after Task 10)` | Must not grow unexpectedly (`<= +15%` per slice without reason) | `Get-Item target\release\cap2voice-app.exe` |
| End-to-end RAM usage (15-min run) | `TBD (measure at Task 14)` | Stable plateau (no unbounded memory growth) | Windows Process WorkingSet sampling during 15-min video run |

---

## 5. Guarding the Bar (Diff Review Protocol)

At every checkpoint and review, inspect `git diff` against the branch base for the five cheap-road-to-green moves:

1. **Threshold moved:** Any rule removed or numeric bound loosened in `CONSTRAINTS.md`.
2. **Test eased:** Added `.skip` / unjustified `#[ignore]`, deleted test files, or stripped `assert!` / `assert_eq!` lines.
3. **Checker silenced:** New `#[allow(...)]`, `# noqa`, `# type: ignore`, or scanner disable comments.
4. **Work left unfinished:** New `todo!()`, `unimplemented!()`, `TODO`, empty `Err(_) => {}`, or bare `except: pass`.
5. **Unapproved exception:** Any new row added to §6 without explicit approval from Danh.

Tightening a constraint is silent and welcome; loosening any constraint must be loud and explicitly approved.

---

## 6. Exceptions

Any temporary waiver must be explicitly approved by Danh, have a named owner, a concrete tracking reason, and an expiry `<= 90 days` from grant.

| ID | Rule | Path | Reason | Owner | Expires |
|---|---|---|---|---|---|
| *(none)* | — | — | — | — | — |
