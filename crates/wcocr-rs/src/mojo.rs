//! Minimal Rust port of `mojocall.{h,cpp}`: load WeChat's
//! `mmmojo_64.dll` at runtime, resolve its C API with `libloading`,
//! and drive a Mojo environment that spawns the OCR subprocess.
//!
//! `mmmojo` itself remains a black box: this module reproduces the
//! exact FFI calls made by the reference C++ wrapper in the same order.

use crate::OcrError;
use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// `MMMojoInfoMethod` from `mmmojo.h`.
pub mod method {
    use std::ffi::c_int;

    pub const NONE: c_int = 0;
    pub const PUSH: c_int = 1;
    pub const PULL_REQ: c_int = 2;
    pub const PULL_RESP: c_int = 3;
    pub const SHARED: c_int = 4;
}

/// `MMMojoEnvironmentCallbackType` from `mmmojo.h`.
pub mod callback_type {
    use std::ffi::c_int;

    pub const USER_DATA: c_int = 0;
    pub const READ_PUSH: c_int = 1;
    pub const READ_PULL: c_int = 2;
    pub const READ_SHARED: c_int = 3;
    pub const REMOTE_CONNECT: c_int = 4;
    pub const REMOTE_DISCONNECT: c_int = 5;
    pub const REMOTE_PROCESS_LAUNCHED: c_int = 6;
    pub const REMOTE_PROCESS_LAUNCH_FAILED: c_int = 7;
    pub const REMOTE_MOJO_ERROR: c_int = 8;
}

/// `MMMojoEnvironmentInitParamType` from `mmmojo.h`.
pub mod init_param {
    use std::ffi::c_int;

    pub const HOST_PROCESS: c_int = 0;
    pub const LOOP_START_THREAD: c_int = 1;
    pub const EXE_PATH: c_int = 2;
    pub const LOG_PATH: c_int = 3;
    pub const LOG_TO_STDERR: c_int = 4;
    pub const ADD_NUM_MESSAGEPIPE: c_int = 5;
    pub const SET_DISCONNECT_HANDLERS: c_int = 6;
}

/// Connection state mirroring `CMojoCall::MJC_*` plus `Inited` after OCR handshake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Failed = -1,
    Pending = 0,
    Connected = 1,
    Inited = 2,
}

/// Callbacks delivered by the Mojo layer on `mmmojo`'s internal threads.
pub trait MojoHandler: Send + Sync {
    fn on_read_push(&self, request_id: u32, data: &[u8]);
    fn on_read_pull(&self, request_id: u32, data: &[u8]) {
        let _ = (request_id, data);
    }
    fn on_read_shared(&self, request_id: u32, data: &[u8]) {
        let _ = (request_id, data);
    }
    fn on_remote_connect(&self, connected: bool);
    fn on_remote_disconnect(&self) {}
    fn on_process_launched(&self) {}
    fn on_process_launch_failed(&self, error_code: c_int) {
        let _ = error_code;
    }
    fn on_remote_error(&self, data: &[u8]) {
        let _ = data;
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Synchronized Mojo connection state with timed condition-variable waiting.
pub struct ConnectionState {
    state: Mutex<State>,
    cv: Condvar,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionState {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::Pending),
            cv: Condvar::new(),
        }
    }

    pub fn get(&self) -> State {
        *lock_unpoisoned(&self.state)
    }

    pub fn set(&self, state: State) {
        let mut guard = lock_unpoisoned(&self.state);
        *guard = state;
        self.cv.notify_all();
    }

    pub fn wait_until(&self, timeout: Duration, want_inited: bool) -> State {
        let done = |s: &State| {
            if want_inited {
                *s == State::Inited || *s == State::Failed
            } else {
                *s != State::Pending
            }
        };

        let guard = lock_unpoisoned(&self.state);
        let guard = match self.cv.wait_timeout_while(guard, timeout, |s| !done(s)) {
            Ok((guard, _timed_out)) => guard,
            Err(poisoned) => poisoned.into_inner().0,
        };
        *guard
    }
}

type CreateEnvFn = unsafe extern "C" fn() -> *mut c_void;
type SetCallbacksFn = unsafe extern "C" fn(*mut c_void, c_int, ...);
type SetInitParamsFn = unsafe extern "C" fn(*mut c_void, c_int, ...);
type AppendSwitchFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const u16);
type EnvFn = unsafe extern "C" fn(*mut c_void);
type InitMojoFn = unsafe extern "C" fn(c_int, *const *const c_char);
type CreateWriteInfoFn = unsafe extern "C" fn(c_int, bool, u32) -> *mut c_void;
type GetWriteReqFn = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type SendWriteInfoFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;
type RemoveWriteInfoFn = unsafe extern "C" fn(*mut c_void);
type GetReadReqFn = unsafe extern "C" fn(*const c_void, *mut u32) -> *const c_void;
type RemoveReadInfoFn = unsafe extern "C" fn(*const c_void);

/// The 14 `mmmojo_64.dll` exports resolved dynamically at startup.
struct MojoFuncs {
    create_env: CreateEnvFn,
    set_callbacks: SetCallbacksFn,
    set_init_params: SetInitParamsFn,
    append_switch: AppendSwitchFn,
    start_env: EnvFn,
    stop_env: EnvFn,
    remove_env: EnvFn,
    init_mojo: InitMojoFn,
    create_write_info: CreateWriteInfoFn,
    get_write_req: GetWriteReqFn,
    send_write_info: SendWriteInfoFn,
    remove_write_info: RemoveWriteInfoFn,
    get_read_req: GetReadReqFn,
    remove_read_info: RemoveReadInfoFn,
}

impl MojoFuncs {
    unsafe fn load_symbol<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, libloading::Error> {
        let sym: Symbol<'_, T> = unsafe { lib.get(name)? };
        Ok(*sym)
    }

    fn load(lib: &Library) -> Result<Self, libloading::Error> {
        unsafe {
            Ok(Self {
                create_env: Self::load_symbol(lib, b"CreateMMMojoEnvironment\0")?,
                set_callbacks: Self::load_symbol(lib, b"SetMMMojoEnvironmentCallbacks\0")?,
                set_init_params: Self::load_symbol(lib, b"SetMMMojoEnvironmentInitParams\0")?,
                append_switch: Self::load_symbol(lib, b"AppendMMSubProcessSwitchNative\0")?,
                start_env: Self::load_symbol(lib, b"StartMMMojoEnvironment\0")?,
                stop_env: Self::load_symbol(lib, b"StopMMMojoEnvironment\0")?,
                remove_env: Self::load_symbol(lib, b"RemoveMMMojoEnvironment\0")?,
                init_mojo: Self::load_symbol(lib, b"InitializeMMMojo\0")?,
                create_write_info: Self::load_symbol(lib, b"CreateMMMojoWriteInfo\0")?,
                get_write_req: Self::load_symbol(lib, b"GetMMMojoWriteInfoRequest\0")?,
                send_write_info: Self::load_symbol(lib, b"SendMMMojoWriteInfo\0")?,
                remove_write_info: Self::load_symbol(lib, b"RemoveMMMojoWriteInfo\0")?,
                get_read_req: Self::load_symbol(lib, b"GetMMMojoReadInfoRequest\0")?,
                remove_read_info: Self::load_symbol(lib, b"RemoveMMMojoReadInfo\0")?,
            })
        }
    }
}

/// Shared state behind `MojoCall` kept at a stable address for C callbacks.
struct MojoInner {
    funcs: MojoFuncs,
    /// Never unloaded because stopping the mmmojo environment does not join its background threads.
    _lib: Library,
    env: Mutex<*mut c_void>,
    conn: ConnectionState,
    handler: Arc<dyn MojoHandler>,
}

/// Runtime-loaded `mmmojo_64.dll` environment handle.
#[derive(Clone)]
pub struct MojoCall {
    inner: Arc<MojoInner>,
}

// SAFETY: `Library` is retained purely to keep the DLL mapped; every FFI call uses copied
// function pointers, `env` is protected by a `Mutex`, and callbacks only access synchronized state.
unsafe impl Send for MojoInner {}
// SAFETY: Same invariants as `Send`; shared access from callback threads is Mutex/Condvar-guarded.
unsafe impl Sync for MojoInner {}
// SAFETY: `MojoCall` wraps an `Arc<MojoInner>` whose inner state is `Send + Sync`.
unsafe impl Send for MojoCall {}
// SAFETY: `MojoCall` wraps an `Arc<MojoInner>` whose inner state is `Send + Sync`.
unsafe impl Sync for MojoCall {}

fn read_payload(funcs: &MojoFuncs, info: *const c_void) -> &[u8] {
    if info.is_null() {
        return &[];
    }
    unsafe {
        let mut size: u32 = 0;
        let ptr = (funcs.get_read_req)(info, &mut size);
        if ptr.is_null() || size == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(ptr.cast::<u8>(), size as usize)
        }
    }
}

extern "C" fn on_read_push(request_id: u32, info: *const c_void, user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    let data = read_payload(&inner.funcs, info).to_vec();
    inner.handler.on_read_push(request_id, &data);
    unsafe { (inner.funcs.remove_read_info)(info) };
}

extern "C" fn on_read_pull(request_id: u32, info: *const c_void, user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    let data = read_payload(&inner.funcs, info).to_vec();
    inner.handler.on_read_pull(request_id, &data);
    unsafe { (inner.funcs.remove_read_info)(info) };
}

extern "C" fn on_read_shared(request_id: u32, info: *const c_void, user_data: *mut c_void) {
    // Mirrors the C++ wrapper: `RemoveMMMojoReadInfo` is intentionally omitted on shared reads.
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    let data = read_payload(&inner.funcs, info).to_vec();
    inner.handler.on_read_shared(request_id, &data);
}

extern "C" fn on_connect(is_connected: bool, user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    inner.conn.set(if is_connected {
        State::Connected
    } else {
        State::Failed
    });
    inner.handler.on_remote_connect(is_connected);
}

extern "C" fn on_disconnect(user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    inner.handler.on_remote_disconnect();
    inner.conn.set(State::Failed);
}

extern "C" fn on_process_launched(user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    inner.handler.on_process_launched();
}

extern "C" fn on_process_launch_failed(error_code: c_int, user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    inner.handler.on_process_launch_failed(error_code);
    if inner.conn.get() == State::Pending {
        inner.conn.set(State::Failed);
    }
}

extern "C" fn on_error(errorbuf: *const c_void, errorsize: c_int, user_data: *mut c_void) {
    let Some(inner) = (unsafe { user_data.cast::<MojoInner>().as_ref() }) else {
        return;
    };
    let data = match usize::try_from(errorsize) {
        Ok(len) if !errorbuf.is_null() && len > 0 => unsafe {
            std::slice::from_raw_parts(errorbuf.cast::<u8>(), len)
        },
        _ => &[],
    };
    inner.handler.on_remote_error(data);
}

unsafe fn register_callbacks(funcs: &MojoFuncs, env: *mut c_void, user_data: *mut c_void) {
    unsafe {
        (funcs.set_callbacks)(env, callback_type::USER_DATA, user_data);
        (funcs.set_callbacks)(
            env,
            callback_type::READ_PUSH,
            on_read_push as extern "C" fn(u32, *const c_void, *mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::READ_PULL,
            on_read_pull as extern "C" fn(u32, *const c_void, *mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::READ_SHARED,
            on_read_shared as extern "C" fn(u32, *const c_void, *mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::REMOTE_CONNECT,
            on_connect as extern "C" fn(bool, *mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::REMOTE_DISCONNECT,
            on_disconnect as extern "C" fn(*mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::REMOTE_PROCESS_LAUNCHED,
            on_process_launched as extern "C" fn(*mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::REMOTE_PROCESS_LAUNCH_FAILED,
            on_process_launch_failed as extern "C" fn(c_int, *mut c_void),
        );
        (funcs.set_callbacks)(
            env,
            callback_type::REMOTE_MOJO_ERROR,
            on_error as extern "C" fn(*const c_void, c_int, *mut c_void),
        );
    }
}

impl MojoCall {
    /// Load `mmmojo_64.dll` from `wcdir` and initialize the Mojo library.
    pub fn load(wcdir: &Path, handler: Arc<dyn MojoHandler>) -> Result<Self, OcrError> {
        let dll = wcdir.join("mmmojo_64.dll");
        let lib = unsafe { Library::new(&dll) }.map_err(|e| OcrError::DllLoadFailed {
            path: dll.clone(),
            detail: e.to_string(),
        })?;
        let funcs = MojoFuncs::load(&lib).map_err(|e| OcrError::DllLoadFailed {
            path: dll,
            detail: format!("cannot resolve mmmojo exports: {e}"),
        })?;
        unsafe { (funcs.init_mojo)(0, std::ptr::null()) };
        Ok(Self {
            inner: Arc::new(MojoInner {
                funcs,
                _lib: lib,
                env: Mutex::new(std::ptr::null_mut()),
                conn: ConnectionState::new(),
                handler,
            }),
        })
    }

    /// Create the Mojo environment, register callbacks/params/switches, and start it.
    pub fn start(&self, exe_path_wide: &[u16], switches: &[(Vec<u8>, Vec<u16>)]) -> bool {
        const HOST_PROCESS_TRUE: c_int = 1;
        let inner = &self.inner;
        let mut env_guard = lock_unpoisoned(&inner.env);
        if !(*env_guard).is_null() {
            return true;
        }
        unsafe {
            let env = (inner.funcs.create_env)();
            if env.is_null() {
                return false;
            }
            let user_data = Arc::as_ptr(&self.inner).cast::<c_void>().cast_mut();
            register_callbacks(&inner.funcs, env, user_data);
            // C promotes `true` in variadic calls to `int`; Rust does not, so pass explicit `c_int`.
            (inner.funcs.set_init_params)(env, init_param::HOST_PROCESS, HOST_PROCESS_TRUE);
            (inner.funcs.set_init_params)(env, init_param::EXE_PATH, exe_path_wide.as_ptr());
            for (name, value) in switches {
                (inner.funcs.append_switch)(env, name.as_ptr().cast::<c_char>(), value.as_ptr());
            }
            *env_guard = env;
            drop(env_guard);
            (inner.funcs.start_env)(env);
        }
        true
    }

    /// Stop and destroy the Mojo environment.
    pub fn stop(&self) {
        let inner = &self.inner;
        let env = {
            let mut env_guard = lock_unpoisoned(&inner.env);
            std::mem::replace(&mut *env_guard, std::ptr::null_mut())
        };
        if !env.is_null() {
            unsafe {
                (inner.funcs.stop_env)(env);
                (inner.funcs.remove_env)(env);
            }
        }
    }

    /// Serialize and send one protobuf payload over Mojo IPC.
    pub fn send(&self, data: &[u8], method: c_int, sync: bool, request_id: u32) -> bool {
        let inner = &self.inner;
        let env = *lock_unpoisoned(&inner.env);
        if env.is_null() {
            return false;
        }
        unsafe {
            let write_info = (inner.funcs.create_write_info)(method, sync, request_id);
            if write_info.is_null() {
                return false;
            }
            if !data.is_empty() {
                let req = (inner.funcs.get_write_req)(write_info, data.len());
                if req.is_null() {
                    (inner.funcs.remove_write_info)(write_info);
                    return false;
                }
                std::ptr::copy_nonoverlapping(data.as_ptr(), req.cast::<u8>(), data.len());
                return (inner.funcs.send_write_info)(env, write_info);
            }
            (inner.funcs.remove_write_info)(write_info);
        }
        false
    }

    /// Return the current Mojo connection state.
    pub fn state(&self) -> State {
        self.inner.conn.get()
    }

    /// Update the Mojo connection state and wake any waiters.
    pub fn set_state(&self, state: State) {
        self.inner.conn.set(state);
    }

    /// Wait up to `timeout` until connection reaches `Inited` (when `want_inited`) or leaves `Pending`.
    pub fn wait_connection(&self, timeout: Duration, want_inited: bool) -> State {
        self.inner.conn.wait_until(timeout, want_inited)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionState, MojoCall, MojoHandler, State};
    use crate::OcrError;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct NoopHandler;

    impl MojoHandler for NoopHandler {
        fn on_read_push(&self, _request_id: u32, _data: &[u8]) {}
        fn on_remote_connect(&self, _connected: bool) {}
    }

    #[test]
    fn connection_state_starts_pending_and_times_out() {
        let state = ConnectionState::new();
        assert_eq!(state.get(), State::Pending);

        let started = Instant::now();
        let observed = state.wait_until(Duration::from_millis(25), true);
        let elapsed = started.elapsed();

        assert_eq!(observed, State::Pending);
        assert!(
            elapsed >= Duration::from_millis(15),
            "expected wait_until to wait near timeout, returned after {elapsed:?}"
        );
    }

    #[test]
    fn connection_state_wakes_immediately_when_set_to_inited() {
        let state = Arc::new(ConnectionState::new());
        let writer = Arc::clone(&state);

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            writer.set(State::Inited);
        });

        let started = Instant::now();
        let observed = state.wait_until(Duration::from_secs(2), true);
        let elapsed = started.elapsed();

        let _ = handle.join();
        assert_eq!(observed, State::Inited);
        assert!(
            elapsed < Duration::from_millis(500),
            "expected wake immediately, took {elapsed:?}"
        );
    }

    #[test]
    fn connection_state_distinguishes_connected_from_inited() {
        let state = ConnectionState::new();
        state.set(State::Connected);

        assert_eq!(
            state.wait_until(Duration::from_millis(50), false),
            State::Connected
        );
        assert_eq!(
            state.wait_until(Duration::from_millis(25), true),
            State::Connected
        );
    }

    #[test]
    fn connection_state_wakes_immediately_on_failed() {
        let state = Arc::new(ConnectionState::new());
        let writer = Arc::clone(&state);

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            writer.set(State::Failed);
        });

        let started = Instant::now();
        let observed = state.wait_until(Duration::from_secs(2), true);
        let elapsed = started.elapsed();

        let _ = handle.join();
        assert_eq!(observed, State::Failed);
        assert!(
            elapsed < Duration::from_millis(500),
            "expected wake immediately on failure, took {elapsed:?}"
        );
    }

    #[test]
    fn mojo_call_load_missing_directory_returns_dll_load_failed() {
        let handler: Arc<dyn MojoHandler> = Arc::new(NoopHandler);
        let err = MojoCall::load(Path::new(r"Z:\nonexistent_cap2voice_wcdir"), handler)
            .err()
            .unwrap_or_else(|| panic!("expected DllLoadFailed for missing directory"));

        match err {
            OcrError::DllLoadFailed { path, detail } => {
                assert!(
                    path.ends_with("mmmojo_64.dll"),
                    "unexpected dll path: {}",
                    path.display()
                );
                assert!(!detail.is_empty(), "expected non-empty load error detail");
            }
            other => panic!("expected OcrError::DllLoadFailed, got {other:?}"),
        }
    }
}
