//! HTTP contract tests against a local mock server and real-PC `ServerProcess`
//! lifecycle + Windows Job Object forced-kill verification.

use cap2voice_app::api::{
    check_health, discover_python_executable_with_path, HealthError, HealthStatus, HttpTranslator,
    HttpTtsEngine, ServerProcess, ServerProcessConfig, ServerProcessError, TranslateError,
    TranslateRequest, Translator, TtsEngine, TtsError, TtsRequest,
};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
struct MockResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    delay: Duration,
}

impl MockResponse {
    fn json(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json",
            body: body.into().into_bytes(),
            delay: Duration::ZERO,
        }
    }

    fn text(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain",
            body: body.into().into_bytes(),
            delay: Duration::ZERO,
        }
    }

    fn mp3(bytes: Vec<u8>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            content_type: "audio/mpeg",
            body: bytes,
            delay: Duration::ZERO,
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];

    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, val)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(len) = val.trim().parse::<usize>() {
                    content_length = len;
                }
            }
        }
    }

    while buf.len() < header_end + content_length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }

    let body_end = (header_end + content_length).min(buf.len());
    Some(RecordedRequest {
        method,
        path,
        body: buf[header_end..body_end].to_vec(),
    })
}

async fn spawn_mock_server(
    responses: Vec<MockResponse>,
) -> (String, Arc<Mutex<Vec<RecordedRequest>>>, JoinHandle<()>) {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else {
        panic!("failed binding mock TcpListener");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("failed getting local addr");
    };
    let expected_count = responses.len();
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let recorded = Arc::new(Mutex::new(Vec::new()));

    let q_clone = Arc::clone(&queue);
    let r_clone = Arc::clone(&recorded);
    let handle = tokio::spawn(async move {
        for _ in 0..expected_count {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            if let Some(req) = read_http_request(&mut stream).await {
                r_clone.lock().unwrap_or_else(|e| e.into_inner()).push(req);
            }
            let next_resp = q_clone
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            let Some(resp) = next_resp else {
                break;
            };
            if !resp.delay.is_zero() {
                tokio::time::sleep(resp.delay).await;
            }
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp.status,
                resp.reason,
                resp.content_type,
                resp.body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&resp.body).await;
            let _ = stream.shutdown().await;
        }
    });

    (format!("http://{addr}"), recorded, handle)
}

fn unused_loopback_port() -> u16 {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        panic!("failed binding ephemeral TCP port");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("failed reading ephemeral port");
    };
    addr.port()
}

fn closed_base_url() -> String {
    let addr = SocketAddr::from(([127, 0, 0, 1], unused_loopback_port()));
    format!("http://{addr}")
}

#[tokio::test]
async fn http_translator_sends_exact_json_and_parses_success_response() {
    let (base_url, recorded, handle) = spawn_mock_server(vec![MockResponse::json(
        200,
        "OK",
        r#"{"text":"Đêm đã khuya, mưa vẫn còn rơi."}"#,
    )])
    .await;

    let Ok(translator) =
        HttpTranslator::with_base_url(base_url, Duration::from_secs(1), Duration::from_secs(2))
    else {
        panic!("HttpTranslator init failed");
    };

    let res = translator.translate("  夜深了，雨还在下。  ").await;
    assert_eq!(res, Ok("Đêm đã khuya, mưa vẫn còn rơi.".to_string()));

    let _ = handle.await;
    let reqs = recorded.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/translate");
    let parsed: Result<TranslateRequest, _> = serde_json::from_slice(&reqs[0].body);
    assert_eq!(
        parsed.ok(),
        Some(TranslateRequest {
            text: "夜深了，雨还在下。".to_string(),
        })
    );
}

#[tokio::test]
async fn http_translator_maps_empty_400_429_502_503_invalid_timeout_and_refused() {
    let (base_url, _recorded, handle) = spawn_mock_server(vec![
        MockResponse::json(
            400,
            "Bad Request",
            r#"{"error":"invalid_input","detail":"text must not be empty"}"#,
        ),
        MockResponse::json(
            429,
            "Too Many Requests",
            r#"{"error":"rate_limited","detail":"Google Translate rate limit reached"}"#,
        ),
        MockResponse::json(
            502,
            "Bad Gateway",
            r#"{"error":"upstream_error","detail":"translation upstream unreachable"}"#,
        ),
        MockResponse::json(
            503,
            "Service Unavailable",
            r#"{"error":"upstream_error","detail":"service busy"}"#,
        ),
        MockResponse::json(200, "OK", r#"{"text":"   "}"#),
        MockResponse::text(200, "OK", "not-json"),
        MockResponse::json(200, "OK", r#"{"text":"quá hạn"}"#)
            .with_delay(Duration::from_millis(300)),
    ])
    .await;

    let Ok(translator) = HttpTranslator::with_base_url(
        base_url.clone(),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ) else {
        panic!("HttpTranslator init failed");
    };

    assert_eq!(
        translator.translate("   \t ").await,
        Err(TranslateError::EmptyInput)
    );
    assert_eq!(
        translator.translate("你好").await,
        Err(TranslateError::InvalidInput {
            detail: "text must not be empty".to_string(),
        })
    );
    assert_eq!(
        translator.translate("你好").await,
        Err(TranslateError::RateLimited {
            detail: "Google Translate rate limit reached".to_string(),
        })
    );
    assert_eq!(
        translator.translate("你好").await,
        Err(TranslateError::Upstream {
            status: 502,
            detail: "translation upstream unreachable".to_string(),
        })
    );
    assert_eq!(
        translator.translate("你好").await,
        Err(TranslateError::Upstream {
            status: 503,
            detail: "service busy".to_string(),
        })
    );
    assert!(matches!(
        translator.translate("你好").await,
        Err(TranslateError::InvalidResponse { .. })
    ));
    assert!(matches!(
        translator.translate("你好").await,
        Err(TranslateError::InvalidResponse { .. })
    ));

    let Ok(short_timeout) =
        HttpTranslator::with_base_url(base_url, Duration::from_secs(1), Duration::from_millis(60))
    else {
        panic!("short timeout HttpTranslator init failed");
    };
    assert_eq!(
        short_timeout.translate("你好").await,
        Err(TranslateError::Timeout { timeout_ms: 60 })
    );

    let _ = handle.await;

    let Ok(refused_client) = HttpTranslator::with_base_url(
        closed_base_url(),
        Duration::from_secs(4),
        Duration::from_secs(4),
    ) else {
        panic!("refused HttpTranslator init failed");
    };
    assert!(matches!(
        refused_client.translate("你好").await,
        Err(TranslateError::ServerUnavailable { .. })
    ));
}

#[tokio::test]
async fn http_tts_engine_sends_exact_json_and_maps_mp3_and_error_variants() {
    let sample_mp3 = b"ID3\x04\x00fake-mp3-payload".to_vec();
    let (base_url, recorded, handle) = spawn_mock_server(vec![
        MockResponse::mp3(sample_mp3.clone()),
        MockResponse::json(
            400,
            "Bad Request",
            r#"{"error":"invalid_input","detail":"unsupported voice: bad-voice"}"#,
        ),
        MockResponse::json(
            502,
            "Bad Gateway",
            r#"{"error":"upstream_error","detail":"Edge-TTS failed"}"#,
        ),
        MockResponse::json(
            503,
            "Service Unavailable",
            r#"{"error":"upstream_error","detail":"Edge-TTS returned no audio data"}"#,
        ),
        MockResponse::json(200, "OK", r#"{"unexpected":true}"#),
        MockResponse::mp3(Vec::new()),
        MockResponse::mp3(vec![1, 2, 3]).with_delay(Duration::from_millis(300)),
    ])
    .await;

    let Ok(tts) = HttpTtsEngine::with_base_url(
        base_url.clone(),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ) else {
        panic!("HttpTtsEngine init failed");
    };

    assert_eq!(
        tts.synthesize("   ", "vi-VN-NamMinhNeural", "+0%").await,
        Err(TtsError::EmptyInput)
    );

    let audio = tts
        .synthesize("  Xin chào  ", "vi-VN-HoaiMyNeural", "+10%")
        .await;
    assert_eq!(audio, Ok(sample_mp3));

    assert_eq!(
        tts.synthesize("Xin chào", "bad-voice", "+0%").await,
        Err(TtsError::InvalidInput {
            detail: "unsupported voice: bad-voice".to_string(),
        })
    );
    assert_eq!(
        tts.synthesize("Xin chào", "vi-VN-NamMinhNeural", "+0%")
            .await,
        Err(TtsError::Upstream {
            status: 502,
            detail: "Edge-TTS failed".to_string(),
        })
    );
    assert_eq!(
        tts.synthesize("Xin chào", "vi-VN-NamMinhNeural", "+0%")
            .await,
        Err(TtsError::Upstream {
            status: 503,
            detail: "Edge-TTS returned no audio data".to_string(),
        })
    );
    assert!(matches!(
        tts.synthesize("Xin chào", "vi-VN-NamMinhNeural", "+0%")
            .await,
        Err(TtsError::InvalidResponse { .. })
    ));
    assert!(matches!(
        tts.synthesize("Xin chào", "vi-VN-NamMinhNeural", "+0%")
            .await,
        Err(TtsError::InvalidResponse { .. })
    ));

    let Ok(short_tts) =
        HttpTtsEngine::with_base_url(base_url, Duration::from_secs(1), Duration::from_millis(60))
    else {
        panic!("short timeout HttpTtsEngine init failed");
    };
    assert_eq!(
        short_tts
            .synthesize("Xin chào", "vi-VN-NamMinhNeural", "+0%")
            .await,
        Err(TtsError::Timeout { timeout_ms: 60 })
    );

    let _ = handle.await;

    {
        let reqs = recorded.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/tts");
        let parsed: Result<TtsRequest, _> = serde_json::from_slice(&reqs[0].body);
        assert_eq!(
            parsed.ok(),
            Some(TtsRequest {
                text: "Xin chào".to_string(),
                voice: "vi-VN-HoaiMyNeural".to_string(),
                rate: "+10%".to_string(),
            })
        );
    }

    let Ok(refused_tts) = HttpTtsEngine::with_base_url(
        closed_base_url(),
        Duration::from_secs(4),
        Duration::from_secs(4),
    ) else {
        panic!("refused HttpTtsEngine init failed");
    };
    assert!(matches!(
        refused_tts
            .synthesize("Xin chào", "vi-VN-NamMinhNeural", "+0%")
            .await,
        Err(TtsError::ServerUnavailable { .. })
    ));
}

#[tokio::test]
async fn check_health_verifies_schema_and_maps_error_variants() {
    let (base_url, _recorded, handle) = spawn_mock_server(vec![
        MockResponse::json(
            200,
            "OK",
            r#"{"status":"ok","version":"0.1.0","translator":"google-free","tts":"edge-tts"}"#,
        ),
        MockResponse::json(
            200,
            "OK",
            r#"{"status":"degraded","version":"0.1.0","translator":"google-free","tts":"edge-tts"}"#,
        ),
        MockResponse::text(503, "Service Unavailable", "starting"),
        MockResponse::json(
            200,
            "OK",
            r#"{"status":"ok","version":"0.1.0","translator":"google-free","tts":"edge-tts"}"#,
        )
        .with_delay(Duration::from_millis(300)),
    ])
    .await;

    assert_eq!(
        check_health(&base_url, Duration::from_secs(1)).await,
        Ok(HealthStatus {
            status: "ok".to_string(),
            version: "0.1.0".to_string(),
            translator: "google-free".to_string(),
            tts: "edge-tts".to_string(),
        })
    );
    assert!(matches!(
        check_health(&base_url, Duration::from_secs(1)).await,
        Err(HealthError::InvalidResponse { .. })
    ));
    assert_eq!(
        check_health(&base_url, Duration::from_secs(1)).await,
        Err(HealthError::UnexpectedStatus {
            status: 503,
            detail: "starting".to_string(),
        })
    );
    assert_eq!(
        check_health(&base_url, Duration::from_millis(60)).await,
        Err(HealthError::Timeout { timeout_ms: 60 })
    );

    let _ = handle.await;

    assert!(matches!(
        check_health(&closed_base_url(), Duration::from_secs(4)).await,
        Err(HealthError::ServerUnavailable { .. })
    ));
}

#[tokio::test]
async fn server_process_reports_clear_diagnostics_when_dir_or_python_missing() {
    let temp = std::env::temp_dir().join(format!("cap2voice_missing_srv_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    let Ok(()) = std::fs::create_dir_all(&temp) else {
        panic!("failed creating temp dir");
    };

    let cfg = ServerProcessConfig::for_port(unused_loopback_port(), temp.clone());
    let dir_err = ServerProcess::start(cfg).await;
    assert!(matches!(
        dir_err,
        Err(ServerProcessError::ServerDirNotFound { .. })
    ));

    let Ok(()) = std::fs::write(temp.join("server.py"), b"# stub") else {
        panic!("failed writing stub server.py");
    };
    let py_missing = discover_python_executable_with_path(&temp, Some(OsStr::new("")));
    assert!(matches!(
        py_missing,
        Err(ServerProcessError::PythonNotFound { .. })
    ));

    let bad_py_cfg = ServerProcessConfig::for_port(unused_loopback_port(), temp.clone())
        .with_python_exe(temp.join("nonexistent_python.exe"));
    let spawn_err = ServerProcess::start(bad_py_cfg).await;
    let _ = std::fs::remove_dir_all(&temp);
    assert!(matches!(
        spawn_err,
        Err(ServerProcessError::SpawnFailed { .. })
    ));
}

fn is_pid_running(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let mut cmd = Command::new("tasklist");
    cmd.args(["/FI", &filter, "/FO", "CSV", "/NH"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let Ok(out) = cmd.output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let quoted_pid = format!(",\"{pid}\",");
    stdout.contains(&quoted_pid)
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_pid_running(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !is_pid_running(pid)
}

#[tokio::test]
#[ignore = "spawns real Python server/server.py on Windows PC to verify health, graceful drop, and Job Object forced-kill cleanup"]
async fn real_pc_server_cleanup_on_drop_and_forced_parent_kill() {
    let server_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("server");
    let port1 = unused_loopback_port();
    let started = Instant::now();
    let cfg = ServerProcessConfig::for_port(port1, server_dir.clone());
    let server = match ServerProcess::start(cfg).await {
        Ok(srv) => srv,
        Err(err) => panic!("ServerProcess::start failed on real PC: {err}"),
    };
    let ready_ms = started.elapsed().as_millis();
    let py_pid_graceful = server.pid();
    let base1 = server.base_url().to_string();

    let health = match check_health(&base1, Duration::from_secs(2)).await {
        Ok(h) => h,
        Err(err) => panic!("GET /health failed right after ServerProcess::start: {err}"),
    };
    assert_eq!(health.status, "ok");
    assert_eq!(health.version, "0.1.0");
    assert_eq!(health.translator, "google-free");
    assert_eq!(health.tts, "edge-tts");
    assert!(is_pid_running(py_pid_graceful));
    println!("real-PC ServerProcess ready in {ready_ms} ms (pid={py_pid_graceful}, url={base1})");

    drop(server);
    assert!(
        wait_for_pid_exit(py_pid_graceful, Duration::from_secs(3)),
        "expected graceful Drop to stop Python server pid={py_pid_graceful}"
    );
    assert!(matches!(
        check_health(&base1, Duration::from_secs(4)).await,
        Err(HealthError::ServerUnavailable { .. })
    ));
    println!("graceful Drop reaped pid={py_pid_graceful}");

    // Forced-kill verification: spawn helper test binary holding ServerProcess and TerminateProcess it.
    let port2 = unused_loopback_port();
    let pid_file: PathBuf =
        std::env::temp_dir().join(format!("cap2voice_hold_pid_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);

    let Ok(self_exe) = std::env::current_exe() else {
        panic!("failed resolving test binary path");
    };
    let mut helper = match Command::new(&self_exe)
        .args([
            "--exact",
            "helper_hold_server_until_killed",
            "--ignored",
            "--nocapture",
        ])
        .env("CAP2VOICE_HOLD_SERVER_PORT", port2.to_string())
        .env("CAP2VOICE_HOLD_SERVER_PID_FILE", &pid_file)
        .env("CAP2VOICE_HOLD_SERVER_DIR", &server_dir)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => panic!("failed spawning forced-kill helper process: {err}"),
    };

    let deadline = Instant::now() + Duration::from_secs(12);
    let py_pid_forced = loop {
        if let Ok(raw) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = raw.trim().parse::<u32>() {
                break pid;
            }
        }
        if Instant::now() >= deadline {
            let _ = helper.kill();
            let _ = helper.wait();
            let _ = std::fs::remove_file(&pid_file);
            panic!("timed out waiting for helper subprocess to write Python server PID");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let base2 = format!("http://127.0.0.1:{port2}");
    assert_eq!(
        check_health(&base2, Duration::from_secs(2))
            .await
            .map(|h| h.status),
        Ok("ok".to_string())
    );
    assert!(is_pid_running(py_pid_forced));

    // Terminate parent helper abruptly (bypasses Drop, tests JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE).
    let _ = helper.kill();
    let _ = helper.wait();
    let _ = std::fs::remove_file(&pid_file);

    assert!(
        wait_for_pid_exit(py_pid_forced, Duration::from_secs(3)),
        "Job Object failed to kill child Python server pid={py_pid_forced} after parent TerminateProcess"
    );
    assert!(matches!(
        check_health(&base2, Duration::from_secs(4)).await,
        Err(HealthError::ServerUnavailable { .. })
    ));
    println!("forced parent kill cleanly terminated Job Object child pid={py_pid_forced}");
}

#[tokio::test]
#[ignore = "helper subprocess invoked by real_pc_server_cleanup_on_drop_and_forced_parent_kill"]
async fn helper_hold_server_until_killed() {
    let Some(port_os) = std::env::var_os("CAP2VOICE_HOLD_SERVER_PORT") else {
        return;
    };
    let Some(pid_file_os) = std::env::var_os("CAP2VOICE_HOLD_SERVER_PID_FILE") else {
        return;
    };
    let Some(server_dir_os) = std::env::var_os("CAP2VOICE_HOLD_SERVER_DIR") else {
        return;
    };

    let Ok(port) = port_os.to_string_lossy().parse::<u16>() else {
        panic!("invalid CAP2VOICE_HOLD_SERVER_PORT");
    };
    let cfg = ServerProcessConfig::for_port(port, PathBuf::from(server_dir_os));
    let _server = match ServerProcess::start(cfg).await {
        Ok(srv) => srv,
        Err(err) => panic!("helper failed starting ServerProcess: {err}"),
    };
    let pid_text = _server.pid().to_string();
    let Ok(()) = std::fs::write(PathBuf::from(pid_file_os), pid_text) else {
        panic!("helper failed writing PID file");
    };

    tokio::time::sleep(Duration::from_secs(60)).await;
}
