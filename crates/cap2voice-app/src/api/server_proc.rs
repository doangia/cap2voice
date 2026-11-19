//! Managed local Python API server lifecycle (`uvicorn`) bound to a Windows
//! Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.

use super::http_client::{build_local_http_client, check_health_with_client};
use super::ServerProcessError;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Loopback bind address enforced for the temporary Python API server.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// Maximum duration to wait for `GET /health` to return `200 OK` after spawn.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval between `GET /health` probes during startup.
pub const DEFAULT_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Maximum wait time for child reaping during `stop()` / `Drop`.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

const MAX_STDERR_BYTES: usize = 16 * 1024;

/// Configuration used to launch `ServerProcess`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProcessConfig {
    pub host: String,
    pub port: u16,
    pub server_dir: PathBuf,
    pub python_exe: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub health_poll_interval: Duration,
    pub shutdown_grace: Duration,
}

impl ServerProcessConfig {
    pub fn for_port(port: u16, server_dir: PathBuf) -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port,
            server_dir,
            python_exe: None,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            health_poll_interval: DEFAULT_HEALTH_POLL_INTERVAL,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    pub fn with_python_exe(mut self, python_exe: PathBuf) -> Self {
        self.python_exe = Some(python_exe);
        self
    }

    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn with_health_poll_interval(mut self, interval: Duration) -> Self {
        self.health_poll_interval = interval;
        self
    }

    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// Locate the `server/` directory containing `server.py` by checking `start_dir`
/// and walking ancestor directories.
pub fn discover_server_dir(start_dir: &Path) -> Result<PathBuf, ServerProcessError> {
    for ancestor in start_dir.ancestors() {
        if ancestor.join("server.py").is_file() {
            return Ok(ancestor.to_path_buf());
        }
        let nested = ancestor.join("server");
        if nested.join("server.py").is_file() {
            return Ok(nested);
        }
    }
    Err(ServerProcessError::ServerDirNotFound {
        searched: start_dir.display().to_string(),
    })
}

/// Locate `server/.venv/Scripts/python.exe` (preferred) or a system `python` on `PATH`.
pub fn discover_python_executable(server_dir: &Path) -> Result<PathBuf, ServerProcessError> {
    let path_var = std::env::var_os("PATH");
    discover_python_executable_with_path(server_dir, path_var.as_deref())
}

/// Locate Python executable checking `server_dir/.venv` first, then directories in `path_var`.
pub fn discover_python_executable_with_path(
    server_dir: &Path,
    path_var: Option<&OsStr>,
) -> Result<PathBuf, ServerProcessError> {
    let win_venv = server_dir.join(".venv").join("Scripts").join("python.exe");
    if win_venv.is_file() {
        return Ok(win_venv);
    }

    let unix_venv = server_dir.join(".venv").join("bin").join("python");
    if unix_venv.is_file() {
        return Ok(unix_venv);
    }

    if let Some(paths) = path_var {
        #[cfg(windows)]
        let candidates = ["python.exe", "py.exe"];
        #[cfg(not(windows))]
        let candidates = ["python3", "python"];

        for dir in std::env::split_paths(paths) {
            for bin in candidates {
                let candidate = dir.join(bin);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(ServerProcessError::PythonNotFound {
        searched: format!("{}, PATH", win_venv.display()),
    })
}

fn spawn_stderr_drain(mut stderr: ChildStderr, buf: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 2048];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
                    guard.extend_from_slice(&chunk[..n]);
                    if guard.len() > MAX_STDERR_BYTES {
                        let excess = guard.len() - MAX_STDERR_BYTES;
                        guard.drain(..excess);
                    }
                }
            }
        }
    })
}

#[cfg(windows)]
mod win_job {
    use super::ServerProcessError;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FALSE, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub(super) struct JobObjectGuard(HANDLE);

    // SAFETY: Win32 Job Object kernel handles can safely be stored across threads and dropped/closed
    // by the thread owning `ServerProcess`.
    unsafe impl Send for JobObjectGuard {}
    // SAFETY: Access only mutates or drops via `&mut self` or atomic kernel handle operations.
    unsafe impl Sync for JobObjectGuard {}

    impl JobObjectGuard {
        pub(super) fn create_kill_on_close() -> Result<Self, ServerProcessError> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(ServerProcessError::JobObjectFailed {
                    action: "CreateJobObjectW",
                    code: unsafe { GetLastError() },
                });
            }
            let guard = Self(handle);

            // SAFETY: Plain C POD limit structure zeroed before setting `LimitFlags`.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let info_len =
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .unwrap_or(u32::MAX);

            let ok = unsafe {
                SetInformationJobObject(
                    guard.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    info_len,
                )
            };
            if ok == FALSE {
                return Err(ServerProcessError::JobObjectFailed {
                    action: "SetInformationJobObject",
                    code: unsafe { GetLastError() },
                });
            }

            Ok(guard)
        }

        pub(super) fn assign_child(&self, child: &Child) -> Result<(), ServerProcessError> {
            let proc_handle = child.as_raw_handle() as HANDLE;
            let ok = unsafe { AssignProcessToJobObject(self.0, proc_handle) };
            if ok == FALSE {
                return Err(ServerProcessError::JobObjectFailed {
                    action: "AssignProcessToJobObject",
                    code: unsafe { GetLastError() },
                });
            }
            Ok(())
        }

        pub(super) fn terminate(&self) {
            if !self.0.is_null() {
                unsafe {
                    TerminateJobObject(self.0, 1);
                }
            }
        }
    }

    impl Drop for JobObjectGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
                self.0 = std::ptr::null_mut();
            }
        }
    }
}

/// Handle owning the managed Python FastAPI subprocess and Windows Job Object.
pub struct ServerProcess {
    port: u16,
    base_url: String,
    pid: u32,
    shutdown_grace: Duration,
    child: Option<Child>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<JoinHandle<()>>,
    #[cfg(windows)]
    job: Option<win_job::JobObjectGuard>,
}

impl ServerProcess {
    /// Start `uvicorn server:app` bound to `127.0.0.1:<port>` without a console window,
    /// assign it to a `KILL_ON_JOB_CLOSE` Job Object, and wait for `GET /health`.
    pub async fn start(config: ServerProcessConfig) -> Result<Self, ServerProcessError> {
        if !config.server_dir.join("server.py").is_file() {
            return Err(ServerProcessError::ServerDirNotFound {
                searched: config.server_dir.display().to_string(),
            });
        }

        let python_exe = match &config.python_exe {
            Some(path) => {
                if !path.is_file() {
                    return Err(ServerProcessError::SpawnFailed {
                        program: path.display().to_string(),
                        detail: "file Python được chỉ định không tồn tại".to_string(),
                    });
                }
                path.clone()
            }
            None => discover_python_executable(&config.server_dir)?,
        };

        #[cfg(windows)]
        let job = win_job::JobObjectGuard::create_kill_on_close()?;

        let mut cmd = Command::new(&python_exe);
        cmd.args(["-m", "uvicorn", "server:app", "--app-dir"])
            .arg(&config.server_dir)
            .args(["--host", &config.host, "--port", &config.port.to_string()])
            .current_dir(&config.server_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|err| ServerProcessError::SpawnFailed {
            program: python_exe.display().to_string(),
            detail: err.to_string(),
        })?;

        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let mut stderr_thread = child
            .stderr
            .take()
            .map(|pipe| spawn_stderr_drain(pipe, Arc::clone(&stderr_buf)));

        #[cfg(windows)]
        if let Err(job_err) = job.assign_child(&child) {
            if let Ok(Some(status)) = child.try_wait() {
                if let Some(handle) = stderr_thread.take() {
                    let _ = handle.join();
                }
                let stderr =
                    String::from_utf8_lossy(&stderr_buf.lock().unwrap_or_else(|e| e.into_inner()))
                        .into_owned();
                return Err(ServerProcessError::ExitedEarly {
                    exit_code: status.code(),
                    stderr,
                });
            }
            let _ = child.kill();
            let _ = child.wait();
            if let Some(handle) = stderr_thread.take() {
                let _ = handle.join();
            }
            return Err(job_err);
        }

        let base_url = config.base_url();
        let pid = child.id();
        let mut server = Self {
            port: config.port,
            base_url: base_url.clone(),
            pid,
            shutdown_grace: config.shutdown_grace,
            child: Some(child),
            stderr_buf,
            stderr_thread,
            #[cfg(windows)]
            job: Some(job),
        };

        let probe_timeout = Duration::from_secs(1);
        let client = build_local_http_client(probe_timeout, probe_timeout)
            .map_err(ServerProcessError::HttpClientInit)?;

        let deadline = Instant::now() + config.startup_timeout;

        loop {
            if let Some(child) = server.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    if let Some(handle) = server.stderr_thread.take() {
                        let _ = handle.join();
                    }
                    let stderr = server.stderr_snapshot();
                    let _ = server.child.take();
                    return Err(ServerProcessError::ExitedEarly {
                        exit_code: status.code(),
                        stderr,
                    });
                }
            }

            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            let req_timeout = remaining.min(probe_timeout).max(Duration::from_millis(50));

            let last_error = match check_health_with_client(&client, &base_url, req_timeout).await {
                Ok(_) => return Ok(server),
                Err(err) => err.to_string(),
            };

            let now_after = Instant::now();
            if now_after >= deadline {
                return Err(ServerProcessError::StartupTimeout {
                    port: config.port,
                    timeout_ms: u64::try_from(config.startup_timeout.as_millis())
                        .unwrap_or(u64::MAX),
                    detail: last_error,
                });
            }

            let sleep_dur = deadline
                .saturating_duration_since(now_after)
                .min(config.health_poll_interval);
            tokio::time::sleep(sleep_dur).await;
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn stderr_snapshot(&self) -> String {
        let guard = self.stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    }

    /// Terminate the managed Python server cleanly within `shutdown_grace`.
    pub fn stop(&mut self) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }

        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let deadline = Instant::now() + self.shutdown_grace;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }

        #[cfg(windows)]
        let _ = self.job.take();
        let _ = self.child.take();

        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        discover_python_executable_with_path, discover_server_dir, ServerProcessConfig,
        ServerProcessError,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "cap2voice_srv_{label}_{}_{}_{}",
                std::process::id(),
                nanos,
                seq
            ));
            let Ok(()) = std::fs::create_dir_all(&dir) else {
                panic!("failed to create temp dir {}", dir.display());
            };
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn touch_file(path: &Path) {
        if let Some(parent) = path.parent() {
            let Ok(()) = std::fs::create_dir_all(parent) else {
                panic!("failed creating {}", parent.display());
            };
        }
        let Ok(()) = std::fs::write(path, b"stub") else {
            panic!("failed writing {}", path.display());
        };
    }

    #[test]
    fn discover_python_prefers_venv_scripts_over_path_fallback() {
        let temp = TempDirGuard::new("py_prio");
        let server_dir = temp.path().join("server");
        let path_bin = temp.path().join("sys_bin");
        let venv_py = server_dir.join(r".venv\Scripts\python.exe");
        let sys_py = path_bin.join("python.exe");

        touch_file(&sys_py);
        let Ok(path_env) = std::env::join_paths([path_bin.as_path()]) else {
            panic!("join_paths failed");
        };

        // Without .venv, falls back to PATH python.exe.
        assert_eq!(
            discover_python_executable_with_path(&server_dir, Some(&path_env)),
            Ok(sys_py.clone())
        );

        // Once .venv exists, it strictly takes priority over PATH python.exe.
        touch_file(&venv_py);
        assert_eq!(
            discover_python_executable_with_path(&server_dir, Some(&path_env)),
            Ok(venv_py)
        );

        // Missing both returns explicit PythonNotFound error mentioning server/.venv.
        let empty_server = temp.path().join("empty_server");
        let err =
            discover_python_executable_with_path(&empty_server, Some(OsString::new().as_os_str()));
        assert!(matches!(
            err,
            Err(ServerProcessError::PythonNotFound { .. })
        ));
        if let Err(err) = err {
            assert!(err.to_string().contains("server/.venv"));
        }
    }

    #[test]
    fn discover_server_dir_walks_ancestors_to_find_server_py() {
        let temp = TempDirGuard::new("srv_dir");
        let expected_server = temp.path().join("server");
        touch_file(&expected_server.join("server.py"));
        let nested = temp.path().join(r"target\release");
        let Ok(()) = std::fs::create_dir_all(&nested) else {
            panic!("failed creating nested dir");
        };

        assert_eq!(discover_server_dir(&nested), Ok(expected_server.clone()));
        assert_eq!(discover_server_dir(&expected_server), Ok(expected_server));

        let orphan_temp = TempDirGuard::new("srv_orphan");
        let missing = orphan_temp.path().join("orphan_dir");
        let Ok(()) = std::fs::create_dir_all(&missing) else {
            panic!("failed creating orphan dir");
        };
        assert!(matches!(
            discover_server_dir(&missing),
            Err(ServerProcessError::ServerDirNotFound { .. })
        ));
    }

    #[test]
    fn server_process_config_builds_loopback_base_url() {
        let cfg = ServerProcessConfig::for_port(18765, PathBuf::from(r"E:\repo\server"));
        assert_eq!(cfg.base_url(), "http://127.0.0.1:18765");
    }
}
