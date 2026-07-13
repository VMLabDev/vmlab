//! ConPTY terminal sessions: CreatePseudoConsole hosting PowerShell, pipes
//! pumped to/from the agent channel. Works from a session-0 SYSTEM service —
//! ConPTY does not need an interactive session (Win32-OpenSSH runs exactly
//! this way).

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use super::port::wide;
use crate::mux::{Input, Mux, pump_out};

/// Windows-side MOTD, written down the ConPTY input? No — ConPTY input is
/// keystrokes. The banner is printed by prepending an echo to the command
/// line instead; PowerShell's own banner already names the host, so keep it
/// to the SYSTEM warning.
const DEFAULT_SHELL: &str = "powershell.exe -NoLogo";

/// The pseudoconsole handle, shared with the resize hook.
struct Pty(HPCON);
// SAFETY: ResizePseudoConsole/ClosePseudoConsole are callable from any
// thread; we serialize destruction via Arc.
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

struct OwnedHandle(HANDLE);
// SAFETY: raw handle owned exclusively.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own it.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn open_terminal(mux: &Mux, id: u32, cols: u16, rows: u16, command: Option<Vec<String>>) {
    match spawn(mux, id, cols, rows, command) {
        Ok(()) => {}
        Err(e) => mux.send_error(Some(id), format!("terminal: {e}")),
    }
}

fn spawn(
    mux: &Mux,
    id: u32,
    cols: u16,
    rows: u16,
    command: Option<Vec<String>>,
) -> std::io::Result<()> {
    let cmdline = match command {
        Some(argv) if !argv.is_empty() => argv
            .iter()
            .map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => DEFAULT_SHELL.to_string(),
    };

    // in pipe: host keystrokes → ConPTY; out pipe: ConPTY VT output → host.
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;

    let size = COORD {
        X: cols.max(2) as i16,
        Y: rows.max(2) as i16,
    };
    let mut hpc: HPCON = 0;
    // SAFETY: fresh pipe handles; ConPTY duplicates what it needs.
    let hr = unsafe { CreatePseudoConsole(size, in_read.0, out_write.0, 0, &mut hpc) };
    if hr != S_OK || hpc == 0 {
        return Err(std::io::Error::other(format!(
            "CreatePseudoConsole failed: 0x{hr:08x}"
        )));
    }
    let pty = Arc::new(Pty(hpc));
    // ConPTY holds its own references to these ends now.
    drop(in_read);
    drop(out_write);

    let (process, thread_h) = match spawn_with_conpty(&cmdline, pty.0) {
        Ok(v) => v,
        Err(e) => {
            // SAFETY: hpc came from CreatePseudoConsole above.
            unsafe { ClosePseudoConsole(pty.0) };
            return Err(e);
        }
    };
    drop(thread_h);
    let process = Arc::new(process);

    let done = Arc::new(AtomicBool::new(false));
    let resize_pty = pty.clone();
    let kill_done = done.clone();
    let kill_process = process.clone();
    let Some((input, credit)) = mux.register(
        id,
        Some(Box::new(move |cols, rows| {
            let size = COORD {
                X: cols.max(2) as i16,
                Y: rows.max(2) as i16,
            };
            // SAFETY: live HPCON until the session's reaper closes it.
            unsafe { ResizePseudoConsole(resize_pty.0, size) };
        })),
        Some(Box::new(move || {
            if !kill_done.load(Ordering::SeqCst) {
                // SAFETY: live process handle held by the Arc.
                unsafe { TerminateProcess(kill_process.0, 137) };
            }
        })),
    ) else {
        // SAFETY: our handles.
        unsafe {
            TerminateProcess(process.0, 137);
            ClosePseudoConsole(pty.0);
        }
        return Ok(());
    };
    mux.send_ctrl(&AgentMsg::Opened { id });

    // Input pump: host bytes → ConPTY input pipe.
    {
        let mux = mux.clone();
        // SAFETY: in_write is a fresh pipe handle we own; File assumes it.
        let mut writer = unsafe { File::from_raw_handle(in_write.take() as _) };
        thread::spawn(move || {
            use std::io::Write;
            let mut window = RecvWindow::default();
            for input in input {
                match input {
                    Input::Bytes(b) => {
                        let _ = writer.write_all(&b);
                        if let Some(grant) = window.recv(b.len()) {
                            mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                        }
                    }
                    Input::Eof => {}
                }
            }
        });
    }

    // Output pump: ConPTY output pipe → host.
    let out_pump = {
        let (mux, credit) = (mux.clone(), credit.clone());
        // SAFETY: out_read is a fresh pipe handle we own; File assumes it.
        let reader = unsafe { File::from_raw_handle(out_read.take() as _) };
        thread::spawn(move || pump_out(&mux, id, FrameKind::Data, &credit, reader))
    };

    // Reaper.
    let mux = mux.clone();
    thread::spawn(move || {
        // SAFETY: live process handle.
        let code = unsafe {
            WaitForSingleObject(process.0, INFINITE);
            let mut code: u32 = 127;
            GetExitCodeProcess(process.0, &mut code);
            code as i32
        };
        done.store(true, Ordering::SeqCst);
        // Closing the pseudoconsole tears down conhost and closes the output
        // pipe, ending the pump.
        // SAFETY: single close of the HPCON we created.
        unsafe { ClosePseudoConsole(pty.0) };
        let _ = out_pump.join();
        mux.send_ctrl(&AgentMsg::Exited { id, code });
        mux.remove_finished(id);
    });
    Ok(())
}

struct PipeEnd(HANDLE, bool);
impl PipeEnd {
    /// Hand the handle over (caller now owns closing it).
    fn take(mut self) -> HANDLE {
        self.1 = false;
        self.0
    }
}
impl Drop for PipeEnd {
    fn drop(&mut self) {
        if self.1 {
            // SAFETY: unclaimed pipe end.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: out params only.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((PipeEnd(read, true), PipeEnd(write, true)))
}

/// CreateProcessW with the PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE attribute.
fn spawn_with_conpty(cmdline: &str, hpc: HPCON) -> std::io::Result<(OwnedHandle, OwnedHandle)> {
    // SAFETY: textbook STARTUPINFOEXW attribute-list dance; every pointer
    // lives across the CreateProcessW call.
    unsafe {
        let mut attr_size: usize = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size];
        let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attrs, 1, 0, &mut attr_size) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            let e = std::io::Error::last_os_error();
            DeleteProcThreadAttributeList(attrs);
            return Err(e);
        }

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let mut cmd = wide(cmdline);
        let ok = CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        );
        let err = std::io::Error::last_os_error();
        DeleteProcThreadAttributeList(attrs);
        if ok == 0 {
            return Err(err);
        }
        Ok((OwnedHandle(pi.hProcess), OwnedHandle(pi.hThread)))
    }
}

/// Kill an arbitrary process by pid (exec sessions).
pub fn kill_process(pid: u32) {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE};
    // SAFETY: open-then-terminate; a null handle is checked.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 137);
            CloseHandle(h);
        }
    }
}
