//! Starting a process *as* a minted logon (PRD §19.2).
//!
//! `std::process::Command` cannot do this — it has no way to pass a token —
//! so the piped-stdio shape the seam wants is built by hand here, and the
//! ConPTY shape borrows [`env_block`] and [`Owned`] from it.
//!
//! Two things the environment must carry, both of which are silent when
//! missed: the block comes from `CreateEnvironmentBlock` **against the
//! loaded profile**, so `USERPROFILE` is the developer's own directory
//! rather than `C:\Users\Default`; and the working directory defaults to
//! that same profile, so an attached shell starts where a logged-in user's
//! would.

use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};

use super::logon::MintedLogon;
use super::port::wide;
use crate::spawn::{ProcessSpec, Spawned, command_line};

/// A handle we own and close exactly once.
pub struct Owned(pub HANDLE);
// SAFETY: raw handle owned exclusively; Win32 handles have no thread affinity.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own it.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The user's environment block, as `CreateProcessAsUserW` wants it.
pub struct EnvBlock {
    /// `K=V\0K=V\0\0` in UTF-16. Owned by us — the Win32 block is copied out
    /// and destroyed immediately, so overrides can be merged in.
    block: Vec<u16>,
}

impl EnvBlock {
    pub fn as_ptr(&self) -> *const core::ffi::c_void {
        self.block.as_ptr() as *const _
    }
}

/// The environment a process started in `session` gets: the profile's own
/// block with the caller's overrides applied on top.
pub fn env_block(logon: &MintedLogon, overrides: &[(String, String)]) -> std::io::Result<EnvBlock> {
    let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: out param; destroyed below once copied.
    if unsafe { CreateEnvironmentBlock(&mut raw, logon.token(), 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Win32 promises a double-null-terminated UTF-16 block.
    let mut vars = unsafe { read_block(raw as *const u16) };
    // SAFETY: the block we were just handed, freed once.
    unsafe { DestroyEnvironmentBlock(raw) };

    for (key, value) in overrides {
        let prefix = format!("{key}=").to_lowercase();
        vars.retain(|v| !v.to_lowercase().starts_with(&prefix));
        vars.push(format!("{key}={value}"));
    }

    let mut block: Vec<u16> = Vec::new();
    for var in vars {
        block.extend(var.encode_utf16());
        block.push(0);
    }
    block.push(0);
    Ok(EnvBlock { block })
}

/// Copy a `K=V\0K=V\0\0` block out of Win32-owned memory.
///
/// # Safety
/// `ptr` must point at a double-null-terminated UTF-16 environment block.
unsafe fn read_block(ptr: *const u16) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = ptr;
    loop {
        let mut len = 0;
        // SAFETY: the block is null-terminated per entry.
        while unsafe { *cursor.add(len) } != 0 {
            len += 1;
        }
        if len == 0 {
            return out; // the second null: end of block
        }
        // SAFETY: `len` units before the terminator.
        out.push(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(cursor, len)
        }));
        // SAFETY: step past this entry and its terminator.
        cursor = unsafe { cursor.add(len + 1) };
    }
}

/// Serialises creating inheritable pipe ends with the spawn that consumes
/// them.
///
/// `CreateProcessAsUserW` with `bInheritHandles` inherits **every**
/// inheritable handle in the process, so two channels opening at once would
/// each hand the other's pipe ends to their child — and a stdout that a
/// stranger holds open never reaches EOF, which surfaces as an exec that
/// finishes but never ends. Holding this across create-then-spawn means no
/// other inheritable end exists while the spawn runs.
///
/// Every caller of [`create_as_user`] must hold it, not just the one that
/// makes the pipes: a terminal opening mid-exec inherits the exec's ends
/// just as readily.
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the spawn lock. See [`SPAWN`].
pub fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start `spec` as `session` with piped stdio — the exec shape of the seam.
pub fn spawn_piped(logon: &MintedLogon, spec: ProcessSpec) -> std::io::Result<Spawned> {
    if spec.argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    }
    let env = env_block(logon, &spec.env)?;
    let cwd = spec.cwd.clone().or_else(|| logon.home.clone());

    let guard = spawn_lock();
    let (stdin_r, stdin_w) = pipe(PipeEnd::Read)?;
    let (stdout_r, stdout_w) = pipe(PipeEnd::Write)?;
    let (stderr_r, stderr_w) = pipe(PipeEnd::Write)?;

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = stdin_r.0;
    si.hStdOutput = stdout_w.0;
    si.hStdError = stderr_w.0;

    let pi = create_as_user(
        logon,
        &command_line(&spec.argv),
        &env,
        cwd.as_deref(),
        &si,
        0,
    )?;
    // The child holds its own copies now.
    drop(stdin_r);
    drop(stdout_w);
    drop(stderr_w);
    drop(guard);

    // SAFETY: fresh pipe ends we own; File assumes them.
    let input = unsafe { File::from_raw_handle(take(stdin_w) as _) };
    let output = unsafe { File::from_raw_handle(take(stdout_r) as _) };
    let errors = unsafe { File::from_raw_handle(take(stderr_r) as _) };

    let process = Arc::new(pi);
    let kill = process.clone();
    Ok(Spawned {
        input: Box::new(input),
        output: Box::new(output),
        errors: Some(Box::new(errors)),
        resize: None,
        kill: Box::new(move || {
            // SAFETY: live process handle held by the Arc.
            unsafe { TerminateProcess(kill.0, 137) };
        }),
        wait: Box::new(move || wait_for(process.0)),
    })
}

/// Run one command line as `session` and wait up to `timeout_ms` for it,
/// inheriting nothing and discarding its output.
///
/// The share-credential injection is the only caller. It takes a command
/// line rather than an argv because that is what it holds — the `Run` value
/// the SMB mount plan wrote — and running it verbatim is what the `Run` key
/// itself would do. The wait is bounded because it happens while the logon
/// cache is locked: a command that hung would wedge every later attach, and
/// a share that cannot be authenticated must not stop a developer attaching.
pub fn run_and_wait(logon: &MintedLogon, cmdline: &str, timeout_ms: u32) -> std::io::Result<i32> {
    let env = env_block(logon, &[])?;
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let guard = spawn_lock();
    let pi = create_as_user(logon, cmdline, &env, logon.home.as_deref(), &si, 0)?;
    drop(guard);
    // SAFETY: live process handle, killed if it outstays the timeout.
    unsafe {
        if WaitForSingleObject(pi.0, timeout_ms) != WAIT_OBJECT_0 {
            TerminateProcess(pi.0, 1);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out",
            ));
        }
        let mut code: u32 = 127;
        GetExitCodeProcess(pi.0, &mut code);
        Ok(code as i32)
    }
}

/// `CreateProcessAsUserW` with the seam's shared flags, handing back the
/// process handle (the thread handle is closed immediately — nothing here
/// resumes or waits on a thread).
///
/// `extra_flags` is what the caller's startup info implies and this cannot
/// see — the ConPTY shape passes `EXTENDED_STARTUPINFO_PRESENT`.
pub fn create_as_user(
    logon: &MintedLogon,
    cmdline: &str,
    env: &EnvBlock,
    cwd: Option<&str>,
    si: &STARTUPINFOW,
    extra_flags: u32,
) -> std::io::Result<Owned> {
    let mut cmd = wide(cmdline);
    let cwd = cwd.map(wide);
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer lives across the call; `bInheritHandles` is TRUE
    // because the stdio handles in `si` were created inheritable.
    let ok = unsafe {
        CreateProcessAsUserW(
            logon.token(),
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | extra_flags,
            env.as_ptr(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: our handle, closed once; the process handle is kept.
    unsafe { CloseHandle(pi.hThread) };
    Ok(Owned(pi.hProcess))
}

fn wait_for(process: HANDLE) -> i32 {
    // SAFETY: live process handle.
    unsafe {
        WaitForSingleObject(process, INFINITE);
        let mut code: u32 = 127;
        GetExitCodeProcess(process, &mut code);
        code as i32
    }
}

/// Which end of a new pipe the *child* inherits; the other end is marked
/// non-inheritable so the child cannot hold its own read end open and stall
/// our EOF.
enum PipeEnd {
    Read,
    Write,
}

fn pipe(child_end: PipeEnd) -> std::io::Result<(Owned, Owned)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: out params, with attributes that live across the call.
    if unsafe { CreatePipe(&mut read, &mut write, &sa, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ours = match child_end {
        PipeEnd::Read => write,
        PipeEnd::Write => read,
    };
    // SAFETY: a handle we own, before it has been given to anyone.
    unsafe { SetHandleInformation(ours, HANDLE_FLAG_INHERIT, 0) };
    Ok((Owned(read), Owned(write)))
}

/// Hand a handle out of its owner without closing it.
fn take(owned: Owned) -> HANDLE {
    let raw = owned.0;
    std::mem::forget(owned);
    raw
}
