//! ConPTY terminals: CreatePseudoConsole hosting PowerShell, with its pipes
//! handed to the spawner seam as a running process. Works from a
//! session-0 SYSTEM service — ConPTY does not need an interactive session
//! (Win32-OpenSSH runs exactly this way).

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::logon::MintedLogon;
use super::port::wide;
use super::proc::{Owned, agent_env_block, create_as_user, env_block};
use crate::spawn::{Spawned, TerminalSpec, command_line};

/// Windows-side MOTD, written down the ConPTY input? No — ConPTY input is
/// keystrokes. The banner is printed by prepending an echo to the command
/// line instead; PowerShell's own banner already names the host, so keep it
/// to the SYSTEM warning.
const DEFAULT_SHELL: &str = "powershell.exe -NoLogo";

/// ConPTY, resolved at run time rather than imported.
///
/// A static import is a load-time dependency: a binary that names
/// `CreatePseudoConsole` will not start at all on a Windows without it, and
/// the API arrived in Windows 10 1809. That floor would be the *agent's*
/// floor — no exec, no file transfer, no metrics on Server 2012 R2 or
/// Windows 7 — for the sake of one feature. Resolved through
/// `GetProcAddress`, the agent runs everywhere and only terminals are
/// missing where the API is.
mod api {
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{COORD, HPCON};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

    pub struct ConPty {
        pub create: unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32,
        pub resize: unsafe extern "system" fn(HPCON, COORD) -> i32,
        pub close: unsafe extern "system" fn(HPCON),
    }

    /// `Some` on Windows 10 1809 and later, `None` on everything older.
    pub fn conpty() -> Option<&'static ConPty> {
        static API: OnceLock<Option<ConPty>> = OnceLock::new();
        API.get_or_init(|| {
            // SAFETY: kernel32 is loaded in every process; a null return is
            // handled, and each symbol is checked before it is transmuted.
            unsafe {
                let k32 = GetModuleHandleW(super::wide("kernel32.dll").as_ptr());
                if k32.is_null() {
                    return None;
                }
                let create = GetProcAddress(k32, c"CreatePseudoConsole".as_ptr() as *const u8)?;
                let resize = GetProcAddress(k32, c"ResizePseudoConsole".as_ptr() as *const u8)?;
                let close = GetProcAddress(k32, c"ClosePseudoConsole".as_ptr() as *const u8)?;
                Some(ConPty {
                    create: std::mem::transmute(create),
                    resize: std::mem::transmute(resize),
                    close: std::mem::transmute(close),
                })
            }
        })
        .as_ref()
    }
}

/// What a guest too old for ConPTY says when a terminal is opened on it.
pub const NO_CONPTY: &str = "this guest has no ConPTY (Windows 10 1809 / Server 2019 and later),      so interactive terminals are unavailable; exec, file transfer and the rest of the agent work";

/// The pseudoconsole handle, shared with the resize hook.

struct Pty(HPCON);
// SAFETY: ResizePseudoConsole/ClosePseudoConsole are callable from any
// thread; we serialize destruction via Arc.
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

/// Start a shell on a fresh pseudoconsole and wrap it as a seam
/// [`Spawned`]: `input` is the ConPTY's keystroke pipe, `output` its VT
/// stream, and `wait` closes the pseudoconsole once the shell exits — which
/// is what ends the output pipe.
pub fn spawn(spec: TerminalSpec, logon: Option<&MintedLogon>) -> std::io::Result<Spawned> {
    let TerminalSpec {
        command,
        cols,
        rows,
        env,
    } = spec;
    let cmdline = match command {
        Some(argv) if !argv.is_empty() => command_line(&argv),
        _ => DEFAULT_SHELL.to_string(),
    };

    // in pipe: host keystrokes → ConPTY; out pipe: ConPTY VT output → host.
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;

    let size = COORD {
        X: cols.max(2) as i16,
        Y: rows.max(2) as i16,
    };
    let conpty = api::conpty().ok_or_else(|| std::io::Error::other(NO_CONPTY))?;
    let mut hpc: HPCON = 0;
    // SAFETY: fresh pipe handles; ConPTY duplicates what it needs.
    let hr = unsafe { (conpty.create)(size, in_read.0, out_write.0, 0, &mut hpc) };
    if hr != S_OK || hpc == 0 {
        return Err(std::io::Error::other(format!(
            "CreatePseudoConsole failed: 0x{hr:08x}"
        )));
    }
    let pty = Arc::new(Pty(hpc));
    // ConPTY holds its own references to these ends now.
    drop(in_read);
    drop(out_write);

    let process = match spawn_with_conpty(&cmdline, pty.0, logon, &env) {
        Ok(v) => v,
        Err(e) => {
            // SAFETY: hpc came from CreatePseudoConsole above.
            unsafe { (conpty.close)(pty.0) };
            return Err(e);
        }
    };
    let process = Arc::new(process);

    // SAFETY: both are fresh pipe handles we own; File assumes them.
    let input = unsafe { File::from_raw_handle(in_write.take() as _) };
    let output = unsafe { File::from_raw_handle(out_read.take() as _) };

    let resize_pty = pty.clone();
    let kill_process = process.clone();
    Ok(Spawned {
        input: Box::new(input),
        output: Box::new(output),
        errors: None,
        resize: Some(Box::new(move |cols, rows| {
            let size = COORD {
                X: cols.max(2) as i16,
                Y: rows.max(2) as i16,
            };
            // SAFETY: live HPCON until the session's reaper closes it.
            unsafe { (conpty.resize)(resize_pty.0, size) };
        })),
        kill: Box::new(move || {
            // SAFETY: live process handle held by the Arc.
            unsafe { TerminateProcess(kill_process.0, 137) };
        }),
        wait: Box::new(move || {
            // SAFETY: live process handle.
            let code = unsafe {
                WaitForSingleObject(process.0, INFINITE);
                let mut code: u32 = 127;
                GetExitCodeProcess(process.0, &mut code);
                code as i32
            };
            // Closing the pseudoconsole tears down conhost and closes the
            // output pipe, ending the caller's output pump.
            // SAFETY: single close of the HPCON we created.
            unsafe { (conpty.close)(pty.0) };
            code
        }),
    })
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

/// Start the shell on the pseudoconsole — `CreateProcessW` as the agent, or
/// `CreateProcessAsUserW` where the channel resolved to a declared logon
/// (PRD §19.2). The pseudoconsole attribute is the same either way; only the
/// token, the environment and the starting directory differ.
fn spawn_with_conpty(
    cmdline: &str,
    hpc: HPCON,
    logon: Option<&MintedLogon>,
    env: &[(String, String)],
) -> std::io::Result<Owned> {
    // SAFETY: textbook STARTUPINFOEXW attribute-list dance; every pointer
    // lives across the create call.
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

        // Both branches under the spawn lock: `CreateProcessAsUserW`
        // inherits every inheritable handle in the process, so a terminal
        // opening while an exec is wiring its pipes would swallow them.
        let _spawn = super::proc::spawn_lock();
        let spawned = match logon {
            Some(logon) => {
                // The environment must come from the *loaded* profile, or
                // every editor that writes under `$HOME` scribbles into
                // `C:\Users\Default`.
                match env_block(logon, env) {
                    Ok(env) => create_as_user(
                        logon,
                        cmdline,
                        &env,
                        logon.home.as_deref(),
                        &si.StartupInfo,
                        EXTENDED_STARTUPINFO_PRESENT,
                    ),
                    Err(e) => Err(e),
                }
            }
            None => {
                let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
                let mut cmd = wide(cmdline);
                // The agent identity has no profile to load, so it simply
                // inherits the agent's environment — a block is built only
                // when the open carried overrides, keeping the common case
                // a null pointer rather than a copy of our own environment.
                let block = (!env.is_empty()).then(|| agent_env_block(env));
                let flags = EXTENDED_STARTUPINFO_PRESENT
                    | block.as_ref().map_or(0, |_| CREATE_UNICODE_ENVIRONMENT);
                let ok = CreateProcessW(
                    std::ptr::null(),
                    cmd.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    flags,
                    block.as_ref().map_or(std::ptr::null(), |b| b.as_ptr()),
                    std::ptr::null(),
                    &si.StartupInfo,
                    &mut pi,
                );
                if ok == 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    CloseHandle(pi.hThread);
                    Ok(Owned(pi.hProcess))
                }
            }
        };
        DeleteProcThreadAttributeList(attrs);
        spawned
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
