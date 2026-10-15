//! Preparing the guest to serve a shared app over RDP RemoteApp (PRD §20.1,
//! §20.3, ADR-0016): refuse an edition with no Remote Desktop host, then
//! turn Remote Desktop on, allow unlisted programs, let the account log on
//! remotely and start `TermService` — each written only where it differs,
//! so a prepared guest is left exactly as it was.
//!
//! The decisions are `crate::rdp`'s; this module only reads and writes. The
//! agent runs as LocalSystem, which holds the rights for every step.

use std::time::{Duration, Instant};

use vmlab_agent_proto::Logon;
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MEMBER_IN_ALIAS,
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SUCCESS, GetLastError,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    LG_INCLUDE_INDIRECT, LOCALGROUP_MEMBERS_INFO_0, LOCALGROUP_USERS_INFO_0, MAX_PREFERRED_LENGTH,
    NetApiBufferFree, NetLocalGroupAddMembers, NetUserGetLocalGroups,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, LookupAccountNameW, LookupAccountSidW, PSID, SECURITY_MAX_SID_SIZE,
    SID_NAME_USE, WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid,
    WinBuiltinRemoteDesktopUsersSid,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_HANDLE,
    SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS,
    StartServiceW,
};

use super::port::wide;
use crate::rdp::{SETTINGS, check_edition, differs, lookup_name, needs_remote_desktop_users};

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const TERM_SERVICE: &str = "TermService";
/// How long a started `TermService` has to reach running.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// Make the guest able to serve a RemoteApp session for `logon`'s account,
/// changing only what differs.
pub fn prepare(logon: &Logon) -> Result<(), String> {
    prepare_reporting(logon).map(|_| ())
}

/// [`prepare`], returning what it changed — empty on a guest already
/// prepared. The hidden `--rdp-prepare` mode prints it.
pub fn prepare_reporting(logon: &Logon) -> Result<Vec<String>, String> {
    let edition = reg_sz(CURRENT_VERSION, "EditionID")
        .map_err(|e| format!("reading the Windows edition: {e}"))?;
    check_edition(&edition)?;

    let mut changed = Vec::new();
    for s in SETTINGS {
        let current = reg_dword(s.key, s.value).map_err(|e| format!("{}: {e}", s.step))?;
        if differs(current, s.wanted) {
            set_reg_dword(s.key, s.value, s.wanted).map_err(|e| format!("{}: {e}", s.step))?;
            changed.push(format!(r"set HKLM\{}\{} = {}", s.key, s.value, s.wanted));
        }
    }

    let step = format!("letting `{}` log on remotely", logon.user);
    if let Some(group) = grant_remote_logon(&logon.user).map_err(|e| format!("{step}: {e}"))? {
        changed.push(format!("added `{}` to {group}", logon.user));
    }

    if start_term_service().map_err(|e| format!("starting {TERM_SERVICE}: {e}"))? {
        changed.push(format!("started {TERM_SERVICE}"));
    }
    Ok(changed)
}

/// Add the account to Remote Desktop Users unless it is already there or
/// an administrator. Returns the group's name when it added it.
fn grant_remote_logon(account: &str) -> Result<Option<String>, String> {
    let sid = account_sid(&lookup_name(account))?;
    // The canonical `DOMAIN\user`, which the group query wants whatever form
    // the lab file wrote.
    let qualified = sid_name(sid.as_ptr() as PSID)?;
    let admins = well_known_name(WinBuiltinAdministratorsSid)?;
    let rdp_users = well_known_name(WinBuiltinRemoteDesktopUsersSid)?;
    let groups = local_groups(&qualified)?;
    if !needs_remote_desktop_users(&groups, &admins, &rdp_users) {
        return Ok(None);
    }
    let member = LOCALGROUP_MEMBERS_INFO_0 {
        lgrmi0_sid: sid.as_ptr() as PSID,
    };
    let group = wide(&rdp_users);
    // SAFETY: one level-0 entry whose SID outlives the call.
    let rc = unsafe {
        NetLocalGroupAddMembers(
            std::ptr::null(),
            group.as_ptr(),
            0,
            &member as *const _ as *const u8,
            1,
        )
    };
    match rc {
        0 => Ok(Some(rdp_users)),
        ERROR_MEMBER_IN_ALIAS => Ok(None),
        rc => Err(format!("adding it to {rdp_users}: {}", win32(rc))),
    }
}

/// Start `TermService` unless it runs, then wait for it to. Returns whether
/// it had to be started.
fn start_term_service() -> Result<bool, String> {
    // SAFETY: handles are checked and closed by `Sc`.
    let scm = Sc(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) });
    if scm.0.is_null() {
        return Err(format!("opening the service manager: {}", last_error()));
    }
    let name = wide(TERM_SERVICE);
    let svc =
        Sc(unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_QUERY_STATUS | SERVICE_START) });
    if svc.0.is_null() {
        return Err(format!("opening the service: {}", last_error()));
    }
    if service_state(&svc)? == SERVICE_RUNNING {
        return Ok(false);
    }
    // SAFETY: an open service handle and no arguments.
    if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
        let e = unsafe { GetLastError() };
        if e != ERROR_SERVICE_ALREADY_RUNNING {
            return Err(win32(e));
        }
    }
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let state = service_state(&svc)?;
        if state == SERVICE_RUNNING {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "not running {}s after starting it (state {state})",
                START_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a handle this guard owns.
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn service_state(svc: &Sc) -> Result<u32, String> {
    // SAFETY: plain out-struct.
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { QueryServiceStatus(svc.0, &mut status) } == 0 {
        return Err(format!("querying its state: {}", last_error()));
    }
    Ok(status.dwCurrentState)
}

/// The account's SID, as bytes sized by the lookup.
fn account_sid(name: &str) -> Result<Vec<u8>, String> {
    let wname = wide(name);
    let mut sid_len = 0u32;
    let mut dom_len = 0u32;
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: a size query, then a fetch into buffers of the sizes it gave.
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wname.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut dom_len,
            &mut use_,
        );
        let e = GetLastError();
        if e != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!("looking up the account: {}", win32(e)));
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut dom = vec![0u16; dom_len as usize];
        if LookupAccountNameW(
            std::ptr::null(),
            wname.as_ptr(),
            sid.as_mut_ptr() as PSID,
            &mut sid_len,
            dom.as_mut_ptr(),
            &mut dom_len,
            &mut use_,
        ) == 0
        {
            return Err(format!("looking up the account: {}", last_error()));
        }
        Ok(sid)
    }
}

/// `DOMAIN\name` for a SID, as this guest names it.
fn sid_name(sid: PSID) -> Result<String, String> {
    let (domain, name) = lookup_sid(sid)?;
    Ok(if domain.is_empty() {
        name
    } else {
        format!(r"{domain}\{name}")
    })
}

/// A builtin group's name in this guest's language.
fn well_known_name(kind: WELL_KNOWN_SID_TYPE) -> Result<String, String> {
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut len = sid.len() as u32;
    // SAFETY: a buffer of the maximum SID size.
    if unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            sid.as_mut_ptr() as PSID,
            &mut len,
        )
    } == 0
    {
        return Err(format!("building a builtin group's SID: {}", last_error()));
    }
    Ok(lookup_sid(sid.as_mut_ptr() as PSID)?.1)
}

fn lookup_sid(sid: PSID) -> Result<(String, String), String> {
    let mut name_len = 0u32;
    let mut dom_len = 0u32;
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: a size query, then a fetch into buffers of the sizes it gave.
    unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut dom_len,
            &mut use_,
        );
        let e = GetLastError();
        if e != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!("naming a SID: {}", win32(e)));
        }
        let mut name = vec![0u16; name_len as usize];
        let mut dom = vec![0u16; dom_len as usize];
        if LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            dom.as_mut_ptr(),
            &mut dom_len,
            &mut use_,
        ) == 0
        {
            return Err(format!("naming a SID: {}", last_error()));
        }
        Ok((
            String::from_utf16_lossy(&dom[..dom_len as usize]),
            String::from_utf16_lossy(&name[..name_len as usize]),
        ))
    }
}

/// The local groups the account is in, directly or through a domain group.
fn local_groups(qualified: &str) -> Result<Vec<String>, String> {
    let user = wide(qualified);
    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: the API allocates `buf`; it is freed below on every path.
    let rc = unsafe {
        NetUserGetLocalGroups(
            std::ptr::null(),
            user.as_ptr(),
            0,
            LG_INCLUDE_INDIRECT,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
        )
    };
    if rc != 0 {
        if !buf.is_null() {
            unsafe { NetApiBufferFree(buf as *const _) };
        }
        return Err(format!("reading its local groups: {}", win32(rc)));
    }
    let groups = (0..read as usize)
        .map(|i| {
            // SAFETY: `read` level-0 entries.
            let entry = unsafe { &*(buf as *const LOCALGROUP_USERS_INFO_0).add(i) };
            pwstr(entry.lgrui0_name)
        })
        .collect();
    unsafe { NetApiBufferFree(buf as *const _) };
    Ok(groups)
}

fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated wide string the API returned.
    unsafe {
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

fn reg_sz(key: &str, value: &str) -> Result<String, String> {
    let k = wide(key);
    let v = wide(value);
    let mut len = 0u32;
    // SAFETY: a size query, then a fetch into a wide buffer of that size.
    unsafe {
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        );
        if rc != ERROR_SUCCESS {
            return Err(format!(r"HKLM\{key}\{value}: {}", win32(rc)));
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut len,
        );
        if rc != ERROR_SUCCESS {
            return Err(format!(r"HKLM\{key}\{value}: {}", win32(rc)));
        }
        Ok(String::from_utf16_lossy(&buf)
            .trim_end_matches('\0')
            .to_string())
    }
}

/// A DWORD, or `None` where the key or the value does not exist.
fn reg_dword(key: &str, value: &str) -> Result<Option<u32>, String> {
    let k = wide(key);
    let v = wide(value);
    let mut out = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: a DWORD out-param.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut out as *mut u32 as *mut _,
            &mut len,
        )
    };
    match rc {
        ERROR_SUCCESS => Ok(Some(out)),
        ERROR_FILE_NOT_FOUND => Ok(None),
        rc => Err(format!(r"reading HKLM\{key}\{value}: {}", win32(rc))),
    }
}

/// Write a DWORD, creating its key if it does not exist yet — the policy
/// key, on a guest no policy has touched.
fn set_reg_dword(key: &str, value: &str, data: u32) -> Result<(), String> {
    let k = wide(key);
    let v = wide(value);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: out-handle closed below.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!(r"opening HKLM\{key}: {}", win32(rc)));
    }
    // SAFETY: an open key and four bytes of DWORD.
    let rc = unsafe {
        let rc = RegSetValueExW(
            hkey,
            v.as_ptr(),
            0,
            REG_DWORD,
            &data as *const u32 as *const u8,
            std::mem::size_of::<u32>() as u32,
        );
        RegCloseKey(hkey);
        rc
    };
    if rc != ERROR_SUCCESS {
        return Err(format!(r"writing HKLM\{key}\{value}: {}", win32(rc)));
    }
    Ok(())
}

/// A Win32 error as the system words it, code included.
fn win32(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

fn last_error() -> String {
    // SAFETY: reads the calling thread's last-error value.
    win32(unsafe { GetLastError() })
}
