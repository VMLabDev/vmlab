//! Clipboard bridging from a session-0 SYSTEM service.
//!
//! The service starts its own executable on the console user's winsta0\default.
//! Two one-way pipes avoid synchronous duplex reads blocking request writes.
//! The helper owns a message-only window and serves requests on its message
//! thread. Wire lines and startup policy live in crate::clipboard.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, HWND, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    PIPE_ACCESS_OUTBOUND,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_REJECT_REMOTE_CLIENTS,
};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    GetExitCodeProcess, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};

use super::port::wide;
use super::proc::Owned;
use crate::clipboard::{self as reply, NO_DESKTOP_LOGON, helper};
use crate::mux::Mux;

const HELPER_CONNECT: Duration = Duration::from_secs(5);

fn pipe_path(direction: &str) -> String {
    format!(
        r"\\.\pipe\vmlab-agent-clipboard-{}-{direction}",
        std::process::id()
    )
}

struct Connected {
    pid: u32,
    pipe: File,
}

struct Child {
    process: Owned,
    pid: u32,
    session: u32,
}

impl Child {
    fn exit_code(&self) -> Result<Option<u32>, String> {
        // SAFETY: this process handle stays owned while queried.
        match unsafe { WaitForSingleObject(self.process.0, 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
                    return Err(os_failure("clipboard helper GetExitCodeProcess"));
                }
                Ok(Some(code))
            }
            WAIT_FAILED => Err(os_failure("clipboard helper WaitForSingleObject")),
            _ => Ok(None),
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: only our helper is terminated on logoff, session change,
        // or failed startup. This releases its pipe and clipboard window.
        unsafe { TerminateProcess(self.process.0, 0) };
    }
}

struct State {
    helper: Mutex<Option<Connected>>,
    child: Mutex<Option<Child>>,
    expected_pid: AtomicU32,
    pipe_error: Mutex<Option<String>>,
    mux: Mutex<Option<Mux>>,
}

static STATE: OnceLock<Arc<State>> = OnceLock::new();

fn state() -> &'static Arc<State> {
    STATE.get_or_init(|| {
        Arc::new(State {
            helper: Mutex::new(None),
            child: Mutex::new(None),
            expected_pid: AtomicU32::new(0),
            pipe_error: Mutex::new(None),
            mux: Mutex::new(None),
        })
    })
}

pub fn start(mux: &Mux) {
    *state().mux.lock().unwrap() = Some(mux.clone());
    thread::spawn(pipe_server);
    thread::spawn(helper_spawner);
}

pub fn set(mux: &Mux, text: String) {
    let mux = mux.clone();
    thread::spawn(move || {
        if let Err(e) = ensure_helper().and_then(|()| send_to_helper(&helper::set_request(&text))) {
            reply::answer_set(&mux, Err(e));
        }
    });
}

pub fn get(mux: &Mux) {
    let mux = mux.clone();
    thread::spawn(move || {
        if let Err(e) = ensure_helper().and_then(|()| send_to_helper(&helper::get_request())) {
            reply::answer_get(&mux, Err(e));
        }
    });
}

fn helper_connected(pid: u32) -> bool {
    state()
        .helper
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|h| h.pid == pid)
}

fn ensure_helper() -> Result<(), String> {
    // Serialize first-logon requests with the background launch, including
    // the connection wait, so only one helper can claim the pipes.
    let mut child = state().child.lock().unwrap();
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    let mut token: HANDLE = std::ptr::null_mut();
    let user = if session == u32::MAX {
        Err(NO_DESKTOP_LOGON.to_string())
    } else if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        Err(reply::token_failure(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    } else {
        Ok(Owned(token))
    };
    let token = match user {
        Ok(token) => token,
        Err(e) => {
            state().expected_pid.store(0, Ordering::SeqCst);
            *state().helper.lock().unwrap() = None;
            *child = None;
            return Err(e);
        }
    };
    let reuse = match child.as_ref() {
        Some(c) => helper::reuse_process(session, c.session, c.exit_code()?),
        None => false,
    };
    if !reuse {
        state().expected_pid.store(0, Ordering::SeqCst);
        *state().helper.lock().unwrap() = None;
        *child = None;
        *child = Some(spawn_helper(session, token.0)?);
    }
    let running = child.as_ref().unwrap();
    let deadline = Instant::now() + HELPER_CONNECT;
    loop {
        if let Some(e) = state().pipe_error.lock().unwrap().clone() {
            *child = None;
            return Err(e);
        }
        match helper::startup(
            helper_connected(running.pid),
            running.exit_code()?,
            Instant::now() >= deadline,
        ) {
            Ok(true) => return Ok(()),
            Ok(false) => thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                let error = format!(
                    "clipboard helper PID {} in console session {session}: {e}",
                    running.pid
                );
                state().expected_pid.store(0, Ordering::SeqCst);
                *state().helper.lock().unwrap() = None;
                *child = None;
                return Err(error);
            }
        }
    }
}

fn send_to_helper(line: &str) -> Result<(), String> {
    let mut guard = state().helper.lock().unwrap();
    if let Some(connected) = guard.as_mut() {
        match send_line(&mut connected.pipe, line) {
            Ok(()) => return Ok(()),
            Err(e) => {
                *guard = None;
                return Err(format!(
                    "cannot send a clipboard request to the desktop helper: {e}"
                ));
            }
        }
    }
    Err(
        "the clipboard helper in the desktop session went away (the user may have logged off)"
            .into(),
    )
}

fn send_line(pipe: &mut File, line: &str) -> std::io::Result<()> {
    pipe.write_all(format!("{line}\n").as_bytes())
}

fn os_failure(stage: &str) -> String {
    format!("{stage}: {}", std::io::Error::last_os_error())
}

fn create_pipe(direction: &str, access: u32) -> Result<File, String> {
    // Default SYSTEM pipe security gives ordinary desktop users only read.
    // Interactive clients get data access, never FILE_CREATE_PIPE_INSTANCE;
    // the server also authenticates the exact helper PID on both pipes.
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(helper::PIPE_SECURITY).as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(os_failure("clipboard pipe security descriptor"));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let raw = CreateNamedPipeW(
            wide(&pipe_path(direction)).as_ptr(),
            access | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        );
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor);
        if raw == INVALID_HANDLE_VALUE {
            return Err(format!("cannot create clipboard {direction} pipe: {error}"));
        }
        Ok(File::from_raw_handle(raw as _))
    }
}

fn connect_pipe(pipe: &File) -> Result<u32, String> {
    // SAFETY: a live synchronous named-pipe server handle.
    unsafe {
        let raw = pipe.as_raw_handle() as HANDLE;
        let connected = ConnectNamedPipe(raw, std::ptr::null_mut()) != 0;
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        helper::pipe_connected(connected, code)?;
        let mut pid = 0;
        if GetNamedPipeClientProcessId(raw, &mut pid) == 0 {
            return Err(os_failure("clipboard GetNamedPipeClientProcessId"));
        }
        if pid == 0 || pid != state().expected_pid.load(Ordering::SeqCst) {
            return Err(format!(
                "clipboard pipe rejected unexpected client PID {pid}"
            ));
        }
        Ok(pid)
    }
}

fn serve_helper() -> Result<(), String> {
    let requests = create_pipe("requests", PIPE_ACCESS_OUTBOUND)?;
    let replies = create_pipe("replies", PIPE_ACCESS_INBOUND)?;
    *state().pipe_error.lock().unwrap() = None;
    let pid = connect_pipe(&requests)?;
    if connect_pipe(&replies)? != pid {
        return Err("clipboard pipes have different clients".into());
    }
    *state().helper.lock().unwrap() = Some(Connected {
        pid,
        pipe: requests,
    });

    let mut reader = BufReader::new(replies);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if pid == state().expected_pid.load(Ordering::SeqCst)
                    && let Some(msg) = helper::reply_msg(&line)
                    && let Some(mux) = state().mux.lock().unwrap().clone()
                {
                    mux.send_ctrl(&msg);
                }
            }
        }
    }
    let mut connected = state().helper.lock().unwrap();
    if connected.as_ref().is_some_and(|h| h.pid == pid) {
        *connected = None;
    }
    Ok(())
}

fn pipe_server() {
    loop {
        if let Err(e) = serve_helper() {
            eprintln!("vmlab-agent: {e}");
            *state().pipe_error.lock().unwrap() = Some(e);
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn helper_spawner() {
    loop {
        if let Err(e) = ensure_helper()
            && e != NO_DESKTOP_LOGON
        {
            eprintln!("vmlab-agent: {e}");
        }
        thread::sleep(Duration::from_secs(15));
    }
}

fn spawn_helper(session: u32, token: HANDLE) -> Result<Child, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate clipboard helper executable: {e}"))?;
    let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut cmd = wide(&crate::spawn::command_line(&[
        exe.to_string_lossy().into_owned(),
        "--clipboard-helper".into(),
        pipe_path("requests"),
    ]));
    let mut desktop = wide("winsta0\\default");
    // SAFETY: the interactive user's profile is already loaded. Buffers and
    // the environment block outlive CreateProcessAsUserW.
    unsafe {
        let mut environment = std::ptr::null_mut();
        if CreateEnvironmentBlock(&mut environment, token, 0) == 0 {
            return Err(os_failure("clipboard helper CreateEnvironmentBlock"));
        }
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let started = CreateProcessAsUserW(
            token,
            application.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            environment,
            std::ptr::null(),
            &si,
            &mut pi,
        ) != 0;
        let error = std::io::Error::last_os_error();
        DestroyEnvironmentBlock(environment);
        if !started {
            return Err(format!(
                "cannot start clipboard helper {} in console session {session} on winsta0\\default (CreateProcessAsUserW: {error})",
                exe.display()
            ));
        }
        let child = Child {
            process: Owned(pi.hProcess),
            pid: pi.dwProcessId,
            session,
        };
        let thread = Owned(pi.hThread);
        state().expected_pid.store(child.pid, Ordering::SeqCst);
        if ResumeThread(thread.0) == u32::MAX {
            return Err(os_failure("clipboard helper ResumeThread"));
        }
        Ok(child)
    }
}

// ---- the user-session helper ---------------------------------------------

pub fn helper_main(request_path: &str) -> ! {
    if let Err(e) = run_helper(request_path) {
        eprintln!("vmlab-agent clipboard helper: {e}");
        // Preserve Win32 failures for the service's process-exit diagnostic.
        std::process::exit(e.raw_os_error().unwrap_or(1));
    }
    std::process::exit(0);
}

fn open_pipe(path: &str, access: u32) -> std::io::Result<File> {
    let deadline = Instant::now() + HELPER_CONNECT;
    loop {
        // SAFETY: open the local pipe named by the parent service.
        let raw = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                access,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw != INVALID_HANDLE_VALUE {
            return Ok(unsafe { File::from_raw_handle(raw as _) });
        }
        let e = std::io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(c) if c == ERROR_FILE_NOT_FOUND as i32 || c == ERROR_PIPE_BUSY as i32)
            || Instant::now() >= deadline
        {
            return Err(e);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn run_helper(request_path: &str) -> std::io::Result<()> {
    use windows_sys::Win32::System::DataExchange::AddClipboardFormatListener;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, KillTimer,
        MSG, PostMessageW, SetTimer, WM_APP, WM_CLIPBOARDUPDATE, WM_QUIT, WM_TIMER,
    };

    // Connect only after the desktop window and listener are ready.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide("vmlab-clip").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0) };
        }
    }
    let _window = Window(hwnd);
    if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let request_pipe = open_pipe(request_path, helper::PIPE_READ)?;
    let reply_path = request_path.strip_suffix("-requests").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid clipboard pipe path",
        )
    })?;
    let mut replies = open_pipe(&format!("{reply_path}-replies"), helper::PIPE_WRITE)?;

    let (tx, rx) = mpsc::channel();
    let target = hwnd as usize;
    thread::spawn(move || {
        let mut reader = BufReader::new(request_pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(request) = helper::parse_request(&line)
                        && (tx.send(request).is_err()
                            || unsafe { PostMessageW(target as HWND, WM_APP, 0, 0) } == 0)
                    {
                        break;
                    }
                }
            }
        }
        // The message thread exits when the service closes its request pipe.
        unsafe { PostMessageW(target as HWND, WM_QUIT, 0, 0) };
    });

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let status = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if status == 0 {
            return Ok(());
        }
        if status < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if msg.message == WM_APP {
            while let Ok(request) = rx.try_recv() {
                let response = match request {
                    helper::Request::Set(text) => match clip::set_text(hwnd, &text) {
                        Ok(()) => helper::set_ok(),
                        Err(e) => helper::failed(&e),
                    },
                    helper::Request::Get => match clip::get_text() {
                        Ok(text) => helper::clip(&text),
                        Err(e) => helper::failed(&e),
                    },
                };
                send_line(&mut replies, &response)?;
            }
        } else if msg.message == WM_CLIPBOARDUPDATE {
            // OLE owners may still be publishing delayed formats when this
            // notification arrives. Reading immediately can make their
            // OleFlushClipboard (including PowerShell Set-Clipboard) fail.
            if unsafe { SetTimer(hwnd, 1, 50, None) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        } else if msg.message == WM_TIMER && msg.wParam == 1 {
            unsafe { KillTimer(hwnd, 1) };
            if let Ok(text) = clip::get_text() {
                send_line(&mut replies, &helper::clip(&text))?;
            }
        } else {
            unsafe { DispatchMessageW(&msg) };
        }
    }
}

mod clip {
    use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };

    const CF_UNICODETEXT: u32 = 13;

    fn busy() -> String {
        format!(
            "cannot open the desktop clipboard (another program may hold it): {}",
            std::io::Error::last_os_error()
        )
    }

    pub fn get_text() -> Result<String, String> {
        // SAFETY: standard open/get/lock/unlock/close sequence.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err(busy());
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            let result = if handle.is_null() {
                Ok(String::new())
            } else {
                let ptr = GlobalLock(handle as HGLOBAL) as *const u16;
                if ptr.is_null() {
                    Err(format!(
                        "cannot lock clipboard text: {}",
                        std::io::Error::last_os_error()
                    ))
                } else {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    GlobalUnlock(handle as HGLOBAL);
                    Ok(text)
                }
            };
            CloseClipboard();
            result
        }
    }

    pub fn set_text(owner: HWND, text: &str) -> Result<(), String> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: movable allocation transfers to the clipboard on success.
        // Failures free it, and every opened clipboard closes.
        unsafe {
            let bytes = wide.len() * 2;
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if mem.is_null() {
                return Err(format!("cannot allocate {bytes} bytes for the clipboard"));
            }
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                GlobalFree(mem);
                return Err("cannot lock the clipboard buffer".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(mem);
            // NULL would make EmptyClipboard leave no owner, causing
            // SetClipboardData to fail even in the correct session.
            if OpenClipboard(owner) == 0 {
                let e = busy();
                GlobalFree(mem);
                return Err(e);
            }
            if EmptyClipboard() == 0 {
                let e = format!(
                    "EmptyClipboard failed ({})",
                    std::io::Error::last_os_error()
                );
                CloseClipboard();
                GlobalFree(mem);
                return Err(e);
            }
            let placed = !SetClipboardData(CF_UNICODETEXT, mem as _).is_null();
            let err = std::io::Error::last_os_error();
            if !placed {
                GlobalFree(mem);
            }
            CloseClipboard();
            if placed {
                Ok(())
            } else {
                Err(format!("SetClipboardData failed ({err})"))
            }
        }
    }
}
