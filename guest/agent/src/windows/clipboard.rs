//! Clipboard bridging from a session-0 SYSTEM service.
//!
//! The service cannot touch the user clipboard directly (wrong window
//! station), so it spawns *itself* as `--clipboard-helper` into the active
//! console session (`WTSGetActiveConsoleSessionId` + `WTSQueryUserToken` +
//! `CreateProcessAsUserW`) and talks to it over a named pipe:
//!
//! - helper → service: `{"clip": "<text>"}` on every clipboard change
//!   (AddClipboardFormatListener) and in reply to a `get`; `{"set_ok": true}`
//!   or `{"failed": "<why>"}` in reply to a request.
//! - service → helper: `{"set": "<text>"}` / `{"get": true}`.
//!
//! The lines themselves, and the host replies they become, are
//! `crate::clipboard::helper`.
//!
//! With no user logged on there is no helper, and clipboard calls answer
//! [`AgentMsg::ClipboardFailed`] naming why (`crate::clipboard`). A request
//! that finds no helper tries to start one on the spot rather than waiting for
//! the spawner's next round, so the first copy after a logon works.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};

use super::port::wide;
use crate::clipboard::{self as reply, NO_DESKTOP_LOGON, helper};
use crate::mux::Mux;

const PIPE_PATH: &str = "\\\\.\\pipe\\vmlab-agent-clipboard";

/// How long a request waits for a helper it just started to connect.
const HELPER_CONNECT: Duration = Duration::from_secs(5);

/// The service side: latest connected helper's pipe (write half).
struct State {
    helper: Mutex<Option<File>>,
    mux: Mutex<Option<Mux>>,
}

static STATE: OnceLock<Arc<State>> = OnceLock::new();

fn state() -> &'static Arc<State> {
    STATE.get_or_init(|| {
        Arc::new(State {
            helper: Mutex::new(None),
            mux: Mutex::new(None),
        })
    })
}

/// Start the service-side manager: the pipe server plus the helper spawner.
pub fn start(mux: &Mux) {
    *state().mux.lock().unwrap() = Some(mux.clone());
    thread::spawn(pipe_server);
    thread::spawn(helper_spawner);
}

/// Hand a set to the helper. Its `set_ok`/`failed` comes back through the
/// pipe server; a refusal before it ever reaches one is answered here. Off
/// the control thread, because starting a helper can take seconds.
pub fn set(mux: &Mux, text: String) {
    let mux = mux.clone();
    thread::spawn(move || {
        if let Err(e) = ensure_helper().and_then(|()| send_to_helper(&helper::set_request(&text))) {
            reply::answer_set(&mux, Err(e));
        }
    });
}

/// Ask the helper for the clipboard; its `clip`/`failed` comes back through
/// the pipe server.
pub fn get(mux: &Mux) {
    let mux = mux.clone();
    thread::spawn(move || {
        if let Err(e) = ensure_helper().and_then(|()| send_to_helper(&helper::get_request())) {
            reply::answer_get(&mux, Err(e));
        }
    });
}

fn helper_connected() -> bool {
    state().helper.lock().unwrap().is_some()
}

/// A connected helper, or why there cannot be one: start one into the
/// console session if none is connected, and give it a moment to dial in.
fn ensure_helper() -> Result<(), String> {
    if helper_connected() {
        return Ok(());
    }
    spawn_helper()?;
    let deadline = Instant::now() + HELPER_CONNECT;
    while Instant::now() < deadline {
        if helper_connected() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "a user is logged on, but the clipboard helper did not start in their session \
         within {}s",
        HELPER_CONNECT.as_secs()
    ))
}

fn send_to_helper(line: &str) -> Result<(), String> {
    let mut guard = state().helper.lock().unwrap();
    if let Some(pipe) = guard.as_mut()
        && send_line(pipe, line).is_ok()
    {
        return Ok(());
    }
    *guard = None;
    Err(
        "the clipboard helper in the desktop session went away (the user may have \
         logged off)"
            .to_string(),
    )
}

/// One whole line in one write, so the helper's change reports and its
/// replies — written from two threads — never interleave mid-line.
fn send_line(pipe: &mut File, line: &str) -> std::io::Result<()> {
    pipe.write_all(format!("{line}\n").as_bytes())?;
    pipe.flush()
}

/// Serve one helper at a time on the named pipe; forward its clipboard
/// reports to the host.
fn pipe_server() {
    loop {
        // SAFETY: create + block-accept one duplex byte-stream pipe instance.
        let pipe = unsafe {
            const PIPE_ACCESS_DUPLEX: u32 = 3;
            const PIPE_TYPE_BYTE: u32 = 0;
            let sa: *const SECURITY_ATTRIBUTES = std::ptr::null();
            let h = CreateNamedPipeW(
                wide(PIPE_PATH).as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE,
                1,
                64 * 1024,
                64 * 1024,
                0,
                sa,
            );
            if h == INVALID_HANDLE_VALUE {
                thread::sleep(Duration::from_secs(10));
                continue;
            }
            if ConnectNamedPipe(h, std::ptr::null_mut()) == 0 {
                CloseHandle(h);
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            h
        };
        // SAFETY: fresh connected pipe handle, ownership moves to File.
        let file = unsafe {
            use std::os::windows::io::FromRawHandle;
            File::from_raw_handle(pipe as _)
        };
        let write_half = match file.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        *state().helper.lock().unwrap() = Some(write_half);

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // helper gone (logoff)
                Ok(_) => {
                    if let Some(msg) = helper::reply_msg(&line)
                        && let Some(mux) = state().mux.lock().unwrap().clone()
                    {
                        mux.send_ctrl(&msg);
                    }
                }
            }
        }
        *state().helper.lock().unwrap() = None;
    }
}

/// Keep a helper alive in the active console session while a user is
/// logged on.
fn helper_spawner() {
    loop {
        if !helper_connected() {
            // Nobody logged on is the normal idle state, not a fault; the
            // reason is for a request to report, not for this loop.
            let _ = spawn_helper();
        }
        thread::sleep(Duration::from_secs(15));
    }
}

/// Start a helper into the active console session, or say why there is no
/// one there to start it for.
fn spawn_helper() -> Result<(), String> {
    // SAFETY: token query + CreateProcessAsUserW with our own exe path.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == 0xFFFF_FFFF {
            // No console session at all (mid-switch); same answer for the user.
            return Err(NO_DESKTOP_LOGON.to_string());
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if WTSQueryUserToken(session, &mut token) == 0 {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(reply::token_failure(code));
        }
        let exe = std::env::current_exe().unwrap_or_default();
        let mut cmd = wide(&format!("\"{}\" --clipboard-helper", exe.display()));
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let started = CreateProcessAsUserW(
            token,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            // The agent is a console-subsystem exe; without this the helper
            // gets a visible console window on the user's desktop.
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        ) != 0;
        let err = std::io::Error::last_os_error();
        CloseHandle(token);
        if !started {
            return Err(format!(
                "cannot start the clipboard helper in the desktop session ({err})"
            ));
        }
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        Ok(())
    }
}

// ---- the helper process (`vmlab-agent --clipboard-helper`) ----------------

/// Entry point for the helper: bridge the user-session clipboard to the
/// service over the named pipe. Exits when the pipe closes.
pub fn helper_main() {
    // SAFETY: client open of the service's pipe.
    let pipe = unsafe {
        CreateFileW(
            wide(PIPE_PATH).as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        std::process::exit(1);
    }
    // SAFETY: fresh handle, ownership moves to File.
    let file = unsafe {
        use std::os::windows::io::FromRawHandle;
        File::from_raw_handle(pipe as _)
    };
    let Ok(mut write_half) = file.try_clone() else {
        std::process::exit(1);
    };

    // Watch for clipboard changes on a message-only window.
    {
        let Ok(mut change_tx) = file.try_clone() else {
            std::process::exit(1);
        };
        thread::spawn(move || clipboard_watch(&mut change_tx));
    }

    // Serve set/get requests from the service.
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => std::process::exit(0), // service gone
            Ok(_) => {
                let reply = match helper::parse_request(&line) {
                    Some(helper::Request::Set(text)) => match clip::set_text(&text) {
                        Ok(()) => helper::set_ok(),
                        Err(e) => helper::failed(&e),
                    },
                    Some(helper::Request::Get) => match clip::get_text() {
                        Ok(text) => helper::clip(&text),
                        Err(e) => helper::failed(&e),
                    },
                    None => continue,
                };
                let _ = send_line(&mut write_half, &reply);
            }
        }
    }
}

/// Message-only window with AddClipboardFormatListener; every change ships
/// the new text to the service.
fn clipboard_watch(pipe: &mut File) {
    use windows_sys::Win32::System::DataExchange::AddClipboardFormatListener;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE,
    };
    // SAFETY: message-only window on this thread + classic message loop.
    unsafe {
        let hwnd = CreateWindowExW(
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
        );
        if hwnd.is_null() || AddClipboardFormatListener(hwnd) == 0 {
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, hwnd, 0, 0) > 0 {
            if msg.message == WM_CLIPBOARDUPDATE
                && let Ok(text) = clip::get_text()
            {
                let _ = send_line(pipe, &helper::clip(&text));
            }
            DispatchMessageW(&msg);
        }
    }
}

/// Raw clipboard text access (helper runs in the user session, so plain
/// OpenClipboard works).
mod clip {
    use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };

    const CF_UNICODETEXT: u32 = 13;

    /// Whoever holds the clipboard open blocks everyone else; say so.
    fn busy() -> String {
        format!(
            "the clipboard is held open by another program ({})",
            std::io::Error::last_os_error()
        )
    }

    /// The clipboard's text; empty when it holds none (empty, or only
    /// non-text formats).
    pub fn get_text() -> Result<String, String> {
        // SAFETY: standard open/get/lock/unlock/close sequence.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err(busy());
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            let mut text = String::new();
            if !handle.is_null() {
                let ptr = GlobalLock(handle as HGLOBAL) as *const u16;
                if !ptr.is_null() {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    GlobalUnlock(handle as HGLOBAL);
                }
            }
            CloseClipboard();
            Ok(text)
        }
    }

    pub fn set_text(text: &str) -> Result<(), String> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: movable global alloc handed to the clipboard on success
        // (the system owns it afterwards); freed on any failure path.
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
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                let e = busy();
                GlobalFree(mem);
                return Err(e);
            }
            EmptyClipboard();
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
