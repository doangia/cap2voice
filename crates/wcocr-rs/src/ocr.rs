//! Typed Rust session wrapper for WeChat 4.x OCR (`wxocr.dll` + `mmmojo_64.dll`).

use crate::mojo::{method, MojoCall, MojoHandler, State};
use crate::proto::{ocr_common, wx4};
use crate::{strip_verbatim_prefix, to_wide, OcrError};
use prost::Message;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Request IDs for the WeChat 4.x OCR Mojo component.
const HAND_SHAKE: u32 = 10001;
const REQ_OCR: u32 = 10010;
const RESP_OCR: u32 = 10011;
const MAX_TASK_ID: u64 = i32::MAX as u64;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CharDetail {
    pub chars: String,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TextBlock {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub rate: f32,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<CharDetail>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OcrResult {
    pub errcode: i32,
    pub imgpath: String,
    pub width: i32,
    pub height: i32,
    pub ocr_response: Vec<TextBlock>,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

type SwitchArg = (Vec<u8>, Vec<u16>);

struct LaunchPlan {
    exe_to_launch: String,
    switches: Vec<SwitchArg>,
}

fn build_launch_plan(exe: &Path, wcdir: &Path) -> LaunchPlan {
    let exe_str = exe.to_string_lossy().replace('/', "\\");
    let mut wcdir_str = wcdir.to_string_lossy().replace('/', "\\");
    let mut switches = vec![
        (b"no-sandbox\0".to_vec(), to_wide("")),
        (b"user-lib-dir\0".to_vec(), to_wide(&wcdir_str)),
    ];

    let is_dll = Path::new(&exe_str)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"));
    if !is_dll {
        return LaunchPlan {
            exe_to_launch: exe_str,
            switches,
        };
    }

    // WeChat 4.x launches `Weixin.exe` from the parent folder with plugin switches.
    if !wcdir_str.ends_with('\\') {
        wcdir_str.push('\\');
    }
    let exe_to_launch = format!("{wcdir_str}..\\weixin.exe");
    let exe_path = PathBuf::from(&exe_str);
    let app_name = exe_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let app_path = exe_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    switches.push((b"type\0".to_vec(), to_wide(&app_name)));
    switches.push((b"app-path\0".to_vec(), to_wide(&app_path)));
    LaunchPlan {
        exe_to_launch,
        switches,
    }
}

struct TaskSlot {
    imgpath: String,
    result: Mutex<Option<OcrResult>>,
    cv: Condvar,
}

/// Shared OCR state backing `WeChatOcr` and acting as its `MojoHandler`.
/// `Drop` on `WeChatOcr` breaks the `OcrInner -> MojoCall -> handler -> OcrInner` cycle.
struct OcrInner {
    mojo: Mutex<Option<MojoCall>>,
    tasks: Mutex<HashMap<u64, Arc<TaskSlot>>>,
    not_supported: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl OcrInner {
    fn new() -> Self {
        Self {
            mojo: Mutex::new(None),
            tasks: Mutex::new(HashMap::new()),
            not_supported: AtomicBool::new(false),
            last_error: Mutex::new(None),
        }
    }

    fn record_error(&self, message: String) {
        *lock_unpoisoned(&self.last_error) = Some(message);
    }

    fn set_mojo_state(&self, state: State) {
        let mojo = lock_unpoisoned(&self.mojo).clone();
        if let Some(mojo) = mojo {
            mojo.set_state(state);
        }
    }

    fn register_task(&self, imgpath: String) -> Result<(u64, Arc<TaskSlot>), OcrError> {
        let mut tasks = lock_unpoisoned(&self.tasks);
        let Some(task_id) = (2u64..=MAX_TASK_ID).find(|id| !tasks.contains_key(id)) else {
            return Err(OcrError::SendFailed(
                "too many in-flight OCR tasks".to_string(),
            ));
        };
        let slot = Arc::new(TaskSlot {
            imgpath,
            result: Mutex::new(None),
            cv: Condvar::new(),
        });
        tasks.insert(task_id, Arc::clone(&slot));
        Ok((task_id, slot))
    }

    fn complete_task(&self, task_id: u64, mut res: OcrResult) {
        let slot = lock_unpoisoned(&self.tasks).remove(&task_id);
        if let Some(slot) = slot {
            res.imgpath = slot.imgpath.clone();
            let mut guard = lock_unpoisoned(&slot.result);
            *guard = Some(res);
            slot.cv.notify_all();
        }
    }

    fn wait_task_slot(
        &self,
        task_id: u64,
        slot: &TaskSlot,
        timeout: Duration,
    ) -> Result<OcrResult, OcrError> {
        let mut guard = lock_unpoisoned(&slot.result);
        guard = match slot.cv.wait_timeout_while(guard, timeout, |r| r.is_none()) {
            Ok((guard, _timed_out)) => guard,
            Err(poisoned) => poisoned.into_inner().0,
        };
        if let Some(res) = guard.take() {
            return Ok(res);
        }
        drop(guard);

        let removed = lock_unpoisoned(&self.tasks).remove(&task_id);
        if removed.is_none() {
            if let Some(res) = lock_unpoisoned(&slot.result).take() {
                return Ok(res);
            }
        }
        Err(OcrError::RequestTimeout {
            timeout_ms: duration_ms(timeout),
        })
    }

    fn handle_handshake(&self, data: &[u8]) {
        match wx4::OcrSupportMessage::decode(data) {
            Ok(msg) => {
                let supported = msg.supported.unwrap_or(false);
                if supported {
                    self.not_supported.store(false, Ordering::Release);
                    self.set_mojo_state(State::Inited);
                } else {
                    self.not_supported.store(true, Ordering::Release);
                    self.record_error(
                        "WeChat OCR plugin reported OCR is not supported".to_string(),
                    );
                    self.set_mojo_state(State::Failed);
                }
            }
            Err(e) => {
                self.record_error(format!(
                    "bad OCR handshake message: {e} (len={})",
                    data.len()
                ));
                self.set_mojo_state(State::Failed);
            }
        }
    }

    fn handle_ocr_response(&self, data: &[u8]) {
        let resp = match wx4::ParseOcrRespMessage::decode(data) {
            Ok(resp) => resp,
            Err(e) => {
                self.record_error(format!("bad OCR response message: {e}"));
                return;
            }
        };
        let task_id = resp.task_id.unwrap_or(0);
        let mut res = OcrResult {
            errcode: resp.err_code.unwrap_or(-1),
            imgpath: String::new(),
            width: 0,
            height: 0,
            ocr_response: Vec::new(),
        };
        if let Some(info) = resp.res {
            res.errcode = resp.err_code.unwrap_or(0);
            res.width = info
                .img_width
                .and_then(|w| i32::try_from(w).ok())
                .unwrap_or(0);
            res.height = info
                .img_height
                .and_then(|h| i32::try_from(h).ok())
                .unwrap_or(0);
            for line in &info.lines {
                res.ocr_response.push(map_line(line));
            }
        }
        self.complete_task(task_id, res);
    }
}

impl MojoHandler for OcrInner {
    fn on_read_push(&self, request_id: u32, data: &[u8]) {
        match request_id {
            HAND_SHAKE => self.handle_handshake(data),
            RESP_OCR => self.handle_ocr_response(data),
            _ => {}
        }
    }

    fn on_remote_connect(&self, _connected: bool) {
        // State transition is updated directly by `MojoCall`.
    }

    fn on_remote_error(&self, data: &[u8]) {
        let msg = String::from_utf8_lossy(data).into_owned();
        self.record_error(format!("mmmojo remote error: {msg}"));
    }
}

fn point_coord(
    p: &Option<ocr_common::Point>,
    getter: impl Fn(&ocr_common::Point) -> Option<f32>,
) -> f32 {
    p.as_ref().and_then(getter).unwrap_or(0.0)
}

fn map_line(line: &ocr_common::OcrResultLine) -> TextBlock {
    let mut tb = TextBlock {
        left: line.left,
        top: line.top,
        right: line.right,
        bottom: line.bottom,
        rate: line.rate,
        text: line.text.clone(),
        details: Vec::new(),
    };
    for block in &line.blocks {
        if block.chars.is_empty() {
            continue;
        }
        let Some(cb) = &block.char_box else {
            continue;
        };
        let left = point_coord(&cb.bottomleft, |p| p.x).min(point_coord(&cb.topleft, |p| p.x));
        let top = point_coord(&cb.topleft, |p| p.y).min(point_coord(&cb.topright, |p| p.y));
        let right = point_coord(&cb.topright, |p| p.x).max(point_coord(&cb.bottomright, |p| p.x));
        let bottom =
            point_coord(&cb.bottomright, |p| p.y).max(point_coord(&cb.bottomleft, |p| p.y));
        tb.details.push(CharDetail {
            chars: block.chars.clone(),
            left,
            top,
            right,
            bottom,
        });
    }
    tb
}

/// Active WeChat 4.x OCR session communicating over `mmmojo_64.dll`.
pub struct WeChatOcr {
    inner: Arc<OcrInner>,
}

impl WeChatOcr {
    /// Validate paths, load `mmmojo_64.dll` from `weixin_dir`, and launch the OCR subprocess.
    pub fn new(wxocr_dll: &Path, weixin_dir: &Path) -> Result<Self, OcrError> {
        if !wxocr_dll.is_file() {
            return Err(OcrError::InvalidPath {
                path: wxocr_dll.to_path_buf(),
                expected: "wxocr.dll file",
            });
        }
        if !weixin_dir.is_dir() {
            return Err(OcrError::InvalidPath {
                path: weixin_dir.to_path_buf(),
                expected: "Weixin directory",
            });
        }

        let inner = Arc::new(OcrInner::new());
        let handler: Arc<dyn MojoHandler> = Arc::clone(&inner) as Arc<dyn MojoHandler>;
        let mojo = MojoCall::load(weixin_dir, handler)?;
        *lock_unpoisoned(&inner.mojo) = Some(mojo.clone());

        let plan = build_launch_plan(wxocr_dll, weixin_dir);
        if !mojo.start(&to_wide(&plan.exe_to_launch), &plan.switches) {
            mojo.set_state(State::Failed);
            let _ = lock_unpoisoned(&inner.mojo).take();
            return Err(OcrError::StartFailed(
                "failed to start mmmojo environment".to_string(),
            ));
        }

        Ok(Self { inner })
    }

    /// Wait up to `timeout` for the WeChat OCR plugin handshake (`State::Inited`).
    pub fn wait_connection(&self, timeout: Duration) -> Result<(), OcrError> {
        let mojo = lock_unpoisoned(&self.inner.mojo).clone();
        let Some(mojo) = mojo else {
            return Err(OcrError::StartFailed("OCR session is closed".to_string()));
        };

        match mojo.wait_connection(timeout, true) {
            State::Inited => Ok(()),
            State::Failed => {
                if self.inner.not_supported.load(Ordering::Acquire) {
                    return Err(OcrError::NotSupported);
                }
                let detail = lock_unpoisoned(&self.inner.last_error)
                    .clone()
                    .unwrap_or_else(|| "OCR subprocess failed during handshake".to_string());
                Err(OcrError::StartFailed(detail))
            }
            State::Pending | State::Connected => Err(OcrError::HandshakeTimeout {
                timeout_ms: duration_ms(timeout),
            }),
        }
    }

    /// Run synchronous OCR on an image file on disk with a bounded `timeout`.
    pub fn ocr_file(&self, img_path: &Path, timeout: Duration) -> Result<OcrResult, OcrError> {
        let abs = std::fs::canonicalize(img_path).map_err(|e| OcrError::InvalidImagePath {
            path: img_path.to_path_buf(),
            detail: e.to_string(),
        })?;
        let resolved = strip_verbatim_prefix(&abs);
        let img_utf8 = resolved.to_string_lossy().into_owned();
        let display_path = img_path.to_string_lossy().into_owned();
        self.send_ocr_request(Some(img_utf8), None, display_path, timeout)
    }

    fn send_ocr_request(
        &self,
        pic_path: Option<String>,
        pic_data: Option<Vec<u8>>,
        slot_label: String,
        timeout: Duration,
    ) -> Result<OcrResult, OcrError> {
        let deadline = Instant::now() + timeout;
        let conn_budget = deadline.saturating_duration_since(Instant::now());
        self.wait_connection(conn_budget)?;

        let (task_id, slot) = self.inner.register_task(slot_label)?;
        let req = wx4::ParseOcrReqMessage {
            task_id: Some(task_id),
            pic_path,
            xx3: None,
            xx4: None,
            pic_data,
            rt: Some(wx4::ReqType {
                t1: Some(true),
                t2: Some(true),
                t3: Some(false),
            }),
        };
        let encoded = req.encode_to_vec();

        let mojo = lock_unpoisoned(&self.inner.mojo).clone();
        let sent = mojo.is_some_and(|m| m.send(&encoded, method::PUSH, false, REQ_OCR));
        if !sent {
            lock_unpoisoned(&self.inner.tasks).remove(&task_id);
            return Err(OcrError::SendFailed(
                "failed to send OCR request over mmmojo".to_string(),
            ));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        self.inner.wait_task_slot(task_id, &slot, remaining)
    }
}

impl Drop for WeChatOcr {
    fn drop(&mut self) {
        // Take `MojoCall` out first to break the `OcrInner -> MojoCall -> handler -> OcrInner` cycle.
        let mojo = lock_unpoisoned(&self.inner.mojo).take();
        if let Some(mojo) = mojo {
            mojo.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_launch_plan, lock_unpoisoned, CharDetail, OcrInner, OcrResult, SwitchArg, TaskSlot,
        TextBlock, WeChatOcr, HAND_SHAKE, RESP_OCR,
    };
    use crate::mojo::MojoHandler;
    use crate::proto::{ocr_common, wx4};
    use crate::OcrError;
    use prost::Message;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    impl OcrInner {
        fn register_task_with_id(&self, task_id: u64, imgpath: String) -> Arc<TaskSlot> {
            let slot = Arc::new(TaskSlot {
                imgpath,
                result: std::sync::Mutex::new(None),
                cv: std::sync::Condvar::new(),
            });
            lock_unpoisoned(&self.tasks).insert(task_id, Arc::clone(&slot));
            slot
        }
    }

    fn decode_switch_pairs(switches: &[SwitchArg]) -> Vec<(String, String)> {
        switches
            .iter()
            .map(|(k, v)| {
                let key_bytes = k.strip_suffix(&[0]).unwrap_or(k.as_slice());
                let key = String::from_utf8_lossy(key_bytes).into_owned();
                let val_units = v.split(|&u| u == 0).next().unwrap_or(&[]);
                let val = String::from_utf16_lossy(val_units);
                (key, val)
            })
            .collect()
    }

    #[test]
    fn build_launch_plan_for_wxocr_dll_uses_parent_weixin_exe_and_plugin_switches() {
        let exe = Path::new(
            r"C:\Users\doanv\AppData\Roaming\Tencent\xwechat\XPlugin\plugins\WeChatOcr\8022\extracted\wxocr.dll",
        );
        let wcdir = Path::new(r"C:\Program Files\Tencent\Weixin\4.0.2.28");

        let plan = build_launch_plan(exe, wcdir);
        assert_eq!(
            plan.exe_to_launch,
            r"C:\Program Files\Tencent\Weixin\4.0.2.28\..\weixin.exe"
        );

        let pairs = decode_switch_pairs(&plan.switches);
        assert_eq!(
            pairs,
            vec![
                ("no-sandbox".to_string(), String::new()),
                (
                    "user-lib-dir".to_string(),
                    r"C:\Program Files\Tencent\Weixin\4.0.2.28".to_string(),
                ),
                ("type".to_string(), "wxocr".to_string()),
                (
                    "app-path".to_string(),
                    r"C:\Users\doanv\AppData\Roaming\Tencent\xwechat\XPlugin\plugins\WeChatOcr\8022\extracted"
                        .to_string(),
                ),
            ]
        );
    }

    #[test]
    fn on_read_push_handshake_supported_true_sets_inited() {
        let inner = OcrInner::new();
        inner.not_supported.store(true, Ordering::Release);
        let payload = wx4::OcrSupportMessage {
            supported: Some(true),
        }
        .encode_to_vec();

        inner.on_read_push(HAND_SHAKE, &payload);
        assert!(!inner.not_supported.load(Ordering::Acquire));
        assert!(lock_unpoisoned(&inner.last_error).is_none());
    }

    #[test]
    fn on_read_push_handshake_supported_false_records_not_supported() {
        let inner = OcrInner::new();
        let payload = wx4::OcrSupportMessage {
            supported: Some(false),
        }
        .encode_to_vec();

        inner.on_read_push(HAND_SHAKE, &payload);
        assert!(inner.not_supported.load(Ordering::Acquire));
    }

    #[test]
    fn on_read_push_ocr_response_completes_waiting_task_slot_with_mapped_blocks() {
        let inner = Arc::new(OcrInner::new());
        let slot = inner.register_task_with_id(7, "frame.png".to_string());

        let resp_bytes = wx4::ParseOcrRespMessage {
            task_id: Some(7),
            err_code: Some(0),
            res: Some(wx4::OcrResultInfo {
                lines: vec![ocr_common::OcrResultLine {
                    line_box: None,
                    text: "夜深了，雨还在下。".to_string(),
                    rate: 0.97,
                    blocks: vec![ocr_common::OcrResultChar {
                        char_box: Some(ocr_common::Box {
                            topleft: Some(ocr_common::Point {
                                x: Some(12.0),
                                y: Some(22.0),
                            }),
                            topright: Some(ocr_common::Point {
                                x: Some(34.0),
                                y: Some(20.0),
                            }),
                            bottomright: Some(ocr_common::Point {
                                x: Some(36.0),
                                y: Some(48.0),
                            }),
                            bottomleft: Some(ocr_common::Point {
                                x: Some(10.0),
                                y: Some(50.0),
                            }),
                        }),
                        chars: "夜".to_string(),
                    }],
                    left: 10.0,
                    top: 20.0,
                    right: 220.0,
                    bottom: 50.0,
                    unknown_0: None,
                    box10: None,
                }],
                img_width: Some(900),
                img_height: Some(260),
                cpu_report: None,
                time_used: Some(15),
            }),
            rt: None,
            qrcode: None,
            mmfg: None,
        }
        .encode_to_vec();

        let inner_bg = Arc::clone(&inner);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            inner_bg.on_read_push(RESP_OCR, &resp_bytes);
        });

        let result = inner.wait_task_slot(7, &slot, Duration::from_secs(2));
        let _ = handle.join();

        let expected = OcrResult {
            errcode: 0,
            imgpath: "frame.png".to_string(),
            width: 900,
            height: 260,
            ocr_response: vec![TextBlock {
                left: 10.0,
                top: 20.0,
                right: 220.0,
                bottom: 50.0,
                rate: 0.97,
                text: "夜深了，雨还在下。".to_string(),
                details: vec![CharDetail {
                    chars: "夜".to_string(),
                    left: 10.0,
                    top: 20.0,
                    right: 36.0,
                    bottom: 50.0,
                }],
            }],
        };

        assert_eq!(result, Ok(expected));
        assert!(!lock_unpoisoned(&inner.tasks).contains_key(&7));
    }

    #[test]
    fn task_slot_wait_returns_request_timeout_within_bound() {
        let inner = OcrInner::new();
        let slot = inner.register_task_with_id(11, "hung.png".to_string());

        let started = Instant::now();
        let result = inner.wait_task_slot(11, &slot, Duration::from_millis(100));
        let elapsed = started.elapsed();

        assert_eq!(result, Err(OcrError::RequestTimeout { timeout_ms: 100 }));
        assert!(
            elapsed >= Duration::from_millis(80) && elapsed <= Duration::from_millis(250),
            "expected wait to return near 100 ms timeout, elapsed {elapsed:?}"
        );
        assert!(
            !lock_unpoisoned(&inner.tasks).contains_key(&11),
            "timed-out task slot must be removed from map"
        );
    }

    #[test]
    fn task_slot_wait_3000ms_timeout_returns_within_3100ms_bound() {
        let inner = OcrInner::new();
        let slot = inner.register_task_with_id(13, "hung_3s.png".to_string());

        let started = Instant::now();
        let result = inner.wait_task_slot(13, &slot, Duration::from_millis(3000));
        let elapsed = started.elapsed();

        assert_eq!(result, Err(OcrError::RequestTimeout { timeout_ms: 3000 }));
        assert!(
            elapsed >= Duration::from_millis(2950) && elapsed <= Duration::from_millis(3100),
            "expected 3000 ms timeout within <= 3100 ms bound, elapsed {elapsed:?}"
        );
        assert!(
            !lock_unpoisoned(&inner.tasks).contains_key(&13),
            "timed-out task slot must be removed from map"
        );
    }

    #[test]
    #[ignore = "requires local WeChat 4.x OCR installation on Windows PC"]
    fn real_pc_probe_pic_data_support() {
        let wxocr = std::env::var_os("WXOCR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let appdata = std::env::var_os("APPDATA").unwrap_or_default();
                PathBuf::from(appdata)
                    .join(r"Tencent\xwechat\XPlugin\plugins\WeChatOcr\8022\extracted\wxocr.dll")
            });
        let wxdir = std::env::var_os("WXDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Tencent\Weixin\4.0.2.28"));

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("test-zh-custom.png");

        let Ok(png_bytes) = std::fs::read(&fixture) else {
            panic!("failed to read fixture {}", fixture.display());
        };
        let Ok(ocr) = WeChatOcr::new(&wxocr, &wxdir) else {
            panic!("failed to init WeChatOcr");
        };
        let started = Instant::now();
        let res = ocr.send_ocr_request(
            None,
            Some(png_bytes),
            "pic_data_probe".to_string(),
            Duration::from_millis(2000),
        );
        let elapsed_ms = started.elapsed().as_millis();
        match res {
            Ok(out) if out.errcode == 0 => {
                println!(
                    "pic_data probe result: SUPPORTED (elapsed={elapsed_ms} ms, errcode={}, blocks={})",
                    out.errcode,
                    out.ocr_response.len()
                );
                for b in &out.ocr_response {
                    println!("  pic_data block: {}", b.text);
                }
            }
            Ok(out) => {
                println!(
                    "pic_data probe result: REJECTED_BY_ENGINE (errcode={}, elapsed={elapsed_ms} ms, blocks={})",
                    out.errcode,
                    out.ocr_response.len()
                );
            }
            Err(e) => {
                println!("pic_data probe result: UNSUPPORTED/ERROR after {elapsed_ms} ms: {e}");
            }
        }
    }
}
