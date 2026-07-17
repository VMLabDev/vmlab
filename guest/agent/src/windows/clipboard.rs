//! Clipboard bridging from a session-0 SYSTEM service.
//!
//! The service cannot touch the user clipboard directly (wrong window
//! station), so it spawns *itself* as `--clipboard-helper` into the active
//! console session (`WTSGetActiveConsoleSessionId` + `WTSQueryUserToken` +
//! `CreateProcessAsUserW`) and talks to it over a named pipe:
//!
//! - helper → service: `{"clip": "<text>"}` on every clipboard change
//!   (AddClipboardFormatListener) and in reply to a `get`.
//! - service → helper: `{"set": "<text>"}` / `{"get": true}`.
//!
//! With no user logged on there is no helper, and clipboard calls answer
//! with an explanatory error.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

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

use vmlab_agent_proto::AgentMsg;

use super::port::wide;
use crate::mux::Mux;

const PIPE_PATH: &str = "\\\\.\\pipe\\vmlab-agent-clipboard";

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

pub fn set(mux: &Mux, text: String) {
    let line = serde_json::json!({ "set": text }).to_string();
    if !send_to_helper(&line) {
        mux.send_error(None, "clipboard: no interactive user session");
    }
}

pub fn get(mux: &Mux) {
    if !send_to_helper(&serde_json::json!({ "get": true }).to_string()) {
        mux.send_error(None, "clipboard: no interactive user session");
    }
    // The helper's `clip` reply flows back through the pipe server below.
}

fn send_to_helper(line: &str) -> bool {
    let mut guard = state().helper.lock().unwrap();
    if let Some(pipe) = guard.as_mut()
        && writeln!(pipe, "{line}").and_then(|()| pipe.flush()).is_ok()
    {
        return true;
    }
    *guard = None;
    false
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
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim())
                        && let Some(text) = v["clip"].as_str()
                        && let Some(mux) = state().mux.lock().unwrap().clone()
                    {
                        mux.send_ctrl(&AgentMsg::Clipboard {
                            text: text.to_string(),
                        });
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
        if state().helper.lock().unwrap().is_none() {
            spawn_helper();
        }
        thread::sleep(Duration::from_secs(15));
    }
}

fn spawn_helper() {
    // SAFETY: token query + CreateProcessAsUserW with our own exe path.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == 0xFFFF_FFFF {
            return; // no console session (nobody logged on)
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if WTSQueryUserToken(session, &mut token) == 0 {
            return;
        }
        let exe = std::env::current_exe().unwrap_or_default();
        let mut cmd = wide(&format!("\"{}\" --clipboard-helper", exe.display()));
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessAsUserW(
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
        ) != 0
        {
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
        }
        CloseHandle(token);
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
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if let Some(text) = v["set"].as_str() {
                    clip::set_text(text);
                } else if v["get"].as_bool() == Some(true) {
                    let text = clip::get_text().unwrap_or_default();
                    let _ = writeln!(write_half, "{}", serde_json::json!({ "clip": text }));
                    let _ = write_half.flush();
                }
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
                && let Some(text) = clip::get_text()
            {
                let _ = writeln!(pipe, "{}", serde_json::json!({ "clip": text }));
                let _ = pipe.flush();
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

    pub fn get_text() -> Option<String> {
        // SAFETY: standard open/get/lock/unlock/close sequence.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            let text = if handle.is_null() {
                None
            } else {
                let ptr = GlobalLock(handle as HGLOBAL) as *const u16;
                if ptr.is_null() {
                    None
                } else {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    GlobalUnlock(handle as HGLOBAL);
                    Some(s)
                }
            };
            CloseClipboard();
            text
        }
    }

    pub fn set_text(text: &str) {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: movable global alloc handed to the clipboard on success
        // (the system owns it afterwards); freed on any failure path.
        unsafe {
            let bytes = wide.len() * 2;
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if mem.is_null() {
                return;
            }
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                GlobalFree(mem);
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(mem);
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                GlobalFree(mem);
                return;
            }
            EmptyClipboard();
            if SetClipboardData(CF_UNICODETEXT, mem as _).is_null() {
                GlobalFree(mem);
            }
            CloseClipboard();
        }
    }
}
