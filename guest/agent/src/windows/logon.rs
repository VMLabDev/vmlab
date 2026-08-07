//! Minting a declared logon on Windows (PRD §19.2).
//!
//! The agent runs as `LocalSystem` and logs the declared account on itself.
//! Four requirements, each of which silently breaks something if missed —
//! all four settled against a live offline domain (a Server 2025 DC plus a
//! domain-joined member), not inferred:
//!
//! 1. **`LOGON32_LOGON_NETWORK_CLEARTEXT`.** It yields a *real* initial TGT
//!    and genuine network credentials, unlike the identity-without-
//!    credentials a key-authenticated Windows sshd produces — the finding
//!    that moved the SSH server to the host (§19.3). `BATCH` and `SERVICE`
//!    are refused outright (1385), and `INTERACTIVE` is refused **on a
//!    domain controller**, where "log on locally" is not granted to ordinary
//!    users: choosing it would quietly make "the DC is my dev machine"
//!    impossible.
//! 2. **[`LoadUserProfileW`] before spawning.** It *creates* the profile on
//!    demand for a never-logged-on domain user. Skip it and `USERPROFILE` is
//!    `C:\Users\Default` — shared, wrong, and silent, with every editor that
//!    writes under `$HOME` scribbling into it.
//! 3. **[`AdjustTokenPrivileges`].** SYSTEM holds `SeAssignPrimaryToken` and
//!    `SeIncreaseQuota` **present but disabled**; `CreateProcessAsUserW`
//!    fails until the agent enables them.
//! 4. **The linked token** where the account has one, for `elevated`.
//!
//! And one thing rides along: the lab's share credential is written into
//! every logon before anything spawns (see [`inject_share_credential`]).

use std::sync::OnceLock;
use std::time::Instant;

use vmlab_agent_proto::Logon;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, ImpersonateLoggedOnUser,
    LOGON32_LOGON_NETWORK_CLEARTEXT, LOGON32_PROVIDER_DEFAULT, LUID_AND_ATTRIBUTES, LogonUserW,
    LookupPrivilegeValueW, RevertToSelf, SE_ASSIGNPRIMARYTOKEN_NAME, SE_BACKUP_NAME,
    SE_INCREASE_QUOTA_NAME, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ALL_ACCESS, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TokenElevationType, TokenLinkedToken, TokenPrimary,
};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    GetUserProfileDirectoryW, LoadUserProfileW, PROFILEINFOW, UnloadUserProfile,
};

use super::port::wide;
use crate::logon::{Held, LogonCache, LogonKey};
use crate::spawn::{Adopted, Adopter, Identity};

/// `TokenElevationType` values: the account has no split token, this token
/// is the full one, or this token is the filtered one.
const ELEVATION_DEFAULT: TOKEN_ELEVATION_TYPE = 1;
const ELEVATION_FULL: TOKEN_ELEVATION_TYPE = 2;
const ELEVATION_LIMITED: TOKEN_ELEVATION_TYPE = 3;

/// A live logon: the primary token to spawn with, and the profile loaded
/// against it.
///
/// Dropping it is the whole of §19.2's lifetime rule at the bottom end —
/// the profile is unloaded here, or the user's registry hive stays mounted
/// for the machine's life.
pub struct MintedLogon {
    token: HANDLE,
    profile: HANDLE,
    /// The user's own profile directory, which is where a shell starts.
    pub home: Option<String>,
}

// SAFETY: a Win32 handle is process-wide and has no thread affinity; the
// cache serialises handing it out and this type never mutates.
unsafe impl Send for MintedLogon {}
unsafe impl Sync for MintedLogon {}

impl MintedLogon {
    pub fn token(&self) -> HANDLE {
        self.token
    }
}

impl Drop for MintedLogon {
    fn drop(&mut self) {
        // SAFETY: both handles are ours and closed exactly once. The profile
        // must be unloaded before the token it was loaded against is closed.
        unsafe {
            UnloadUserProfile(self.token, self.profile);
            CloseHandle(self.token);
        }
    }
}

/// The agent's logon cache — one per agent, which is one per machine, which
/// is why §19.2's "never survives the machine stopping" needs no code.
pub struct Logons {
    cache: LogonCache<MintedLogon>,
}

impl Logons {
    pub fn new() -> Logons {
        Logons {
            cache: LogonCache::new(),
        }
    }

    /// The live logon for `identity`, minted if the cache has none.
    /// `Identity::Agent` has no logon at all — that is §19.2's floor.
    pub fn resolve(
        &self,
        identity: &Identity,
    ) -> std::io::Result<Option<std::sync::Arc<Held<MintedLogon>>>> {
        let Identity::Declared(logon) = identity else {
            return Ok(None);
        };
        let held = self.cache.get_or_mint(
            LogonKey::new(&logon.user, &logon.secret),
            Instant::now(),
            || mint(logon),
        )?;
        Ok(Some(held))
    }

    /// Drop logons nothing holds and nothing has taken lately. Run on a
    /// timer by [`start_sweeper`].
    pub fn sweep(&self) {
        self.cache.sweep(Instant::now());
    }
}

/// Start the background sweep that drops idle logons — and with them
/// unloads their profile hives.
pub fn start_sweeper(logons: std::sync::Arc<Logons>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(crate::logon::SWEEP_INTERVAL);
            logons.sweep();
        }
    });
}

/// Log `logon` on, load its profile, and hand it the lab's share
/// credential. Roughly 97 ms, paid once per (account, secret).
fn mint(logon: &Logon) -> std::io::Result<MintedLogon> {
    enable_spawn_privileges()?;

    let (domain, user) = split_account(&logon.user);
    let wide_user = wide(user);
    let wide_domain = domain.map(wide);
    let wide_secret = wide(&logon.secret);
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: three wide strings bound above, one out param.
    let ok = unsafe {
        LogonUserW(
            wide_user.as_ptr(),
            wide_domain
                .as_ref()
                .map_or(std::ptr::null(), |d| d.as_ptr()),
            wide_secret.as_ptr(),
            LOGON32_LOGON_NETWORK_CLEARTEXT,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if ok == 0 {
        // §19.2: failure is loud and names the account. Falling back to the
        // agent identity would leave commands mysteriously running as
        // SYSTEM and writing into `systemprofile`.
        let e = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            e.kind(),
            format!("cannot log on as `{}`: {e}", logon.user),
        ));
    }
    let token = match apply_elevation(token, logon.elevated) {
        Ok(t) => t,
        Err(e) => {
            // SAFETY: our token, not yet handed anywhere.
            unsafe { CloseHandle(token) };
            return Err(e);
        }
    };

    // The profile *creates* itself here for a never-logged-on domain user.
    let mut info: PROFILEINFOW = unsafe { std::mem::zeroed() };
    let mut username = wide(user);
    info.dwSize = std::mem::size_of::<PROFILEINFOW>() as u32;
    info.dwFlags = PI_NOUI;
    info.lpUserName = username.as_mut_ptr();
    // SAFETY: `info` and the name buffer outlive the call.
    if unsafe { LoadUserProfileW(token, &mut info) } == 0 {
        let e = std::io::Error::last_os_error();
        // SAFETY: our token, nothing loaded against it.
        unsafe { CloseHandle(token) };
        return Err(std::io::Error::new(
            e.kind(),
            format!("cannot load the profile for `{}`: {e}", logon.user),
        ));
    }

    let minted = MintedLogon {
        token,
        profile: info.hProfile,
        home: profile_dir(token),
    };
    // Before anything else is spawned in this logon — §7.5's correction.
    inject_share_credential(&minted);
    Ok(minted)
}

/// `PI_NOUI`: never show a progress dialog. There is no desktop to show it
/// on, and a blocked dialog would hang the mint.
const PI_NOUI: u32 = 0x0000_0001;

/// Split `DOMAIN\user`, `user@domain` or a bare local account.
///
/// The UPN form is left whole: `LogonUserW` takes a UPN with a null domain,
/// and splitting it would produce a NetBIOS name the DNS domain is not.
fn split_account(account: &str) -> (Option<&str>, &str) {
    match account.split_once('\\') {
        Some((domain, user)) => (Some(domain), user),
        None => (None, account),
    }
}

/// Swap in the account's linked token where the caller's `elevated` asks for
/// the half it is not already holding.
///
/// Without this a **local** admin would land filtered under
/// `LocalAccountTokenFilterPolicy` while a **domain** admin would not — an
/// invisible distinction to declare against (§19.2). An account with no
/// split token has nothing to swap and is used as it is.
fn apply_elevation(token: HANDLE, elevated: bool) -> std::io::Result<HANDLE> {
    let mut kind: TOKEN_ELEVATION_TYPE = ELEVATION_DEFAULT;
    let mut len: u32 = 0;
    // SAFETY: out params sized for a TOKEN_ELEVATION_TYPE.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            &mut kind as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut len,
        )
    };
    if ok == 0 {
        return Ok(token); // no elevation split to reason about
    }
    let wants_swap = match kind {
        ELEVATION_LIMITED => elevated,
        ELEVATION_FULL => !elevated,
        _ => false,
    };
    if !wants_swap {
        return Ok(token);
    }

    let mut linked: TOKEN_LINKED_TOKEN = unsafe { std::mem::zeroed() };
    // SAFETY: out param sized for a TOKEN_LINKED_TOKEN.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenLinkedToken,
            &mut linked as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut len,
        )
    };
    if ok == 0 || linked.LinkedToken.is_null() {
        return Ok(token);
    }
    // The linked token comes back as an impersonation token; only a primary
    // token can start a process.
    let mut primary: HANDLE = std::ptr::null_mut();
    // SAFETY: `linked.LinkedToken` is live until we close it below.
    let ok = unsafe {
        DuplicateTokenEx(
            linked.LinkedToken,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            IMPERSONATION_LEVEL,
            TokenPrimary,
            &mut primary,
        )
    };
    // SAFETY: we own the linked handle whether or not the duplicate worked.
    unsafe { CloseHandle(linked.LinkedToken) };
    if ok == 0 {
        return Ok(token);
    }
    // SAFETY: the original token is ours and now superseded.
    unsafe { CloseHandle(token) };
    Ok(primary)
}

/// The impersonation level the linked token is duplicated at before being
/// turned into a primary token.
const IMPERSONATION_LEVEL: windows_sys::Win32::Security::SECURITY_IMPERSONATION_LEVEL =
    windows_sys::Win32::Security::SecurityImpersonation;

/// Enable the privileges the two calls this module makes need. SYSTEM
/// *holds* all four, but present-but-disabled: without this every spawn
/// fails with 1314 and every profile load with 1307, both of which read as
/// permission problems on a principal that has permission.
///
/// §19.2 names the first pair, which is what `CreateProcessAsUserW` needs;
/// the second pair is `LoadUserProfileW`'s own documented requirement, so
/// enabling them is part of requirement 2 rather than a fifth requirement.
///
/// Once per process — the agent's own token does not change.
fn enable_spawn_privileges() -> std::io::Result<()> {
    static DONE: OnceLock<Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        let mut process_token: HANDLE = std::ptr::null_mut();
        // SAFETY: pseudo-handle in, real handle out.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut process_token,
            )
        } == 0
        {
            return Err(format!(
                "cannot open the agent's own token: {}",
                std::io::Error::last_os_error()
            ));
        }
        let result = [
            SE_ASSIGNPRIMARYTOKEN_NAME,
            SE_INCREASE_QUOTA_NAME,
            SE_RESTORE_NAME,
            SE_BACKUP_NAME,
        ]
        .iter()
        .try_for_each(|name| enable_privilege(process_token, *name));
        // SAFETY: our handle, closed once.
        unsafe { CloseHandle(process_token) };
        result
    })
    .clone()
    .map_err(std::io::Error::other)
}

fn enable_privilege(token: HANDLE, name: windows_sys::core::PCWSTR) -> Result<(), String> {
    let mut privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: unsafe { std::mem::zeroed() },
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: one LUID out, then one privilege in; both live across the call.
    unsafe {
        if LookupPrivilegeValueW(std::ptr::null(), name, &mut privileges.Privileges[0].Luid) == 0 {
            return Err(format!(
                "cannot look up a privilege the agent needs to spawn: {}",
                std::io::Error::last_os_error()
            ));
        }
        // AdjustTokenPrivileges reports success even when it enabled nothing,
        // so the last error is the real answer.
        AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_SUCCESS as i32) {
            return Err(format!(
                "cannot enable a privilege the agent needs to spawn: {err}"
            ));
        }
    }
    Ok(())
}

/// The user's profile directory, which is where an attached shell starts and
/// what `USERPROFILE` in the spawned environment says.
fn profile_dir(token: HANDLE) -> Option<String> {
    let mut len: u32 = 0;
    // SAFETY: a size query (which fails with ERROR_INSUFFICIENT_BUFFER),
    // then a fetch into a buffer of that size.
    unsafe {
        GetUserProfileDirectoryW(token, std::ptr::null_mut(), &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserProfileDirectoryW(token, buf.as_mut_ptr(), &mut len) == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(&buf)
            .trim_end_matches('\0')
            .to_string();
        (!s.is_empty()).then_some(s)
    }
}

// ---- §7.5's correction ----------------------------------------------------

/// Where the SMB mount plan records the lab's share credential.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "vmlab-shares";

/// Write the lab's share credential into a freshly minted logon.
///
/// **A correction to §7.5, not an addition.** The agent's own mounts run as
/// SYSTEM and land in the global DOS-device namespace, so every session
/// *sees* the drive letters while each logon authenticates separately. The
/// existing fix is an `HKLM\…\Run` hook — and a facade logon never fires
/// one, because a `Run` key needs a desktop session and
/// `NETWORK_CLEARTEXT` + `CreateProcessAsUserW` is not that. Without this an
/// attached developer lands in exactly the documented failure: `Z:` is
/// visible and opening it says the password is wrong.
///
/// The credential is read back out of that same `Run` value rather than
/// carried on the wire: the mount plan rewrites it on every mount, so a
/// rotated credential heals here with no host plumbing, and a restored
/// snapshot needs no re-send. A machine with no SMB share has no value and
/// nothing to do — virtiofs mounts through a service-owned global device
/// with no credential.
///
/// Best-effort by design: a share that cannot be authenticated must not stop
/// a developer attaching. The failure it prevents is visible; the failure it
/// would cause is total.
fn inject_share_credential(logon: &MintedLogon) {
    let Some(command) = run_key_value() else {
        return;
    };
    let _ = super::proc::run_and_wait(logon, &command, INJECT_TIMEOUT_MS);
}

/// How long the credential injection may take before it is abandoned. It is
/// one `cmdkey` write against the local credential store, so anything near
/// this is a hang rather than slowness — and the attach must not wait on it.
const INJECT_TIMEOUT_MS: u32 = 15_000;

fn run_key_value() -> Option<String> {
    let key = wide(RUN_KEY);
    let val = wide(RUN_VALUE);
    let mut len: u32 = 0;
    // SAFETY: size query then fetch into a matching wide buffer.
    unsafe {
        if RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        if RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut len,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let s = String::from_utf16_lossy(&buf)
            .trim_end_matches('\0')
            .to_string();
        (!s.is_empty()).then_some(s)
    }
}

// ---- reading as the logon -------------------------------------------------

/// The thread is impersonating a logon; dropping this reverts it.
struct Impersonated {
    /// Keeps the token alive for as long as the thread is wearing it.
    _held: std::sync::Arc<Held<MintedLogon>>,
}

impl Adopted for Impersonated {}

impl Drop for Impersonated {
    fn drop(&mut self) {
        // SAFETY: paired with the ImpersonateLoggedOnUser that built this.
        unsafe { RevertToSelf() };
    }
}

/// Lend `held`'s identity to whichever thread calls the adopter.
pub fn adopter_for(held: std::sync::Arc<Held<MintedLogon>>) -> Adopter {
    Box::new(move || {
        // SAFETY: a live primary token; reverted when the guard drops.
        if unsafe { ImpersonateLoggedOnUser(held.token()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Box::new(Impersonated {
            _held: held.clone(),
        }) as Box<dyn Adopted>)
    })
}
