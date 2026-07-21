//! Windows net_info / os_info / shutdown: GetAdaptersAddresses for NICs,
//! the CurrentVersion registry key for OS identity (GetVersionExW lies under
//! manifest compatibility, the registry does not), and
//! InitiateSystemShutdownExW with the SeShutdownPrivilege enabled — the
//! LocalSystem service account holds the privilege but it starts disabled.

use std::net::{Ipv4Addr, Ipv6Addr};

use vmlab_agent_proto::{NetInterface, OsInfo, ShutdownMode};

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, HANDLE, LUID};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::Shutdown::{
    InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
};
use windows_sys::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW, GetNativeSystemInfo,
    PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM64, PROCESSOR_ARCHITECTURE_INTEL,
    SYSTEM_INFO,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::port::wide;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;

/// All adapters with their MACs and unicast addresses, loopback excluded.
pub fn net_info() -> Result<Vec<NetInterface>, String> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut len: u32 = 16 * 1024;
    let mut buf: Vec<u8>;
    // Size-then-fetch loop: the table can grow between the two calls.
    loop {
        buf = vec![0u8; len as usize];
        // SAFETY: buffer of `len` bytes; the API writes a linked list into it.
        let rc = unsafe {
            GetAdaptersAddresses(
                0, // AF_UNSPEC
                flags,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut len,
            )
        };
        match rc {
            ERROR_SUCCESS => break,
            ERROR_BUFFER_OVERFLOW => continue, // len was updated
            e => return Err(format!("GetAdaptersAddresses failed: {e}")),
        }
    }

    let mut out = Vec::new();
    let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: walking the linked lists the API laid out inside `buf`; all
    // pointers point into the buffer or API-owned strings.
    unsafe {
        while !adapter.is_null() {
            let a = &*adapter;
            adapter = a.Next;
            if a.IfType == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            let mac_len = a.PhysicalAddressLength as usize;
            let mac = (mac_len > 0).then(|| {
                a.PhysicalAddress[..mac_len]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":")
            });
            let mut ipv4 = Vec::new();
            let mut ipv6 = Vec::new();
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let sa = (*ua).Address.lpSockaddr;
                if !sa.is_null() {
                    match (*sa).sa_family {
                        AF_INET => {
                            let sin = &*(sa as *const SOCKADDR_IN);
                            ipv4.push(
                                Ipv4Addr::from(sin.sin_addr.S_un.S_addr.to_ne_bytes()).to_string(),
                            );
                        }
                        AF_INET6 => {
                            let sin6 = &*(sa as *const SOCKADDR_IN6);
                            ipv6.push(Ipv6Addr::from(sin6.sin6_addr.u.Byte).to_string());
                        }
                        _ => {}
                    }
                }
                ua = (*ua).Next;
            }
            out.push(NetInterface {
                name: pwstr_to_string(a.FriendlyName),
                mac,
                ipv4,
                ipv6,
            });
        }
    }
    Ok(out)
}

/// OS identity from HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion.
pub fn os_info() -> Result<OsInfo, String> {
    let name = reg_str("ProductName").unwrap_or_else(|| "Windows".to_string());
    // DisplayVersion (e.g. "24H2") exists from Win10 20H2; ReleaseId before.
    let version = reg_str("DisplayVersion")
        .or_else(|| reg_str("ReleaseId"))
        .unwrap_or_default();
    let major = reg_dword("CurrentMajorVersionNumber").unwrap_or(10);
    let minor = reg_dword("CurrentMinorVersionNumber").unwrap_or(0);
    let build = reg_str("CurrentBuildNumber").unwrap_or_else(|| "0".to_string());
    Ok(OsInfo {
        id: "windows".to_string(),
        name,
        version,
        kernel: format!("{major}.{minor}.{build}"),
        arch: native_arch(),
        hostname: hostname(),
    })
}

/// Force a system shutdown/reboot. Halt maps to powerdown (Windows has no
/// distinct halt). `force = TRUE` matches QGA semantics: hung apps must not
/// wedge the lab's stop ladder.
pub fn shutdown(mode: ShutdownMode) -> Result<(), String> {
    enable_shutdown_privilege()?;
    let reboot = matches!(mode, ShutdownMode::Reboot);
    // SAFETY: plain API call; wide strings outlive it.
    let ok = unsafe {
        InitiateSystemShutdownExW(
            std::ptr::null(),
            std::ptr::null(),
            0,
            1, // force
            reboot as i32,
            SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
        )
    };
    if ok == 0 {
        return Err(format!(
            "InitiateSystemShutdownExW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn enable_shutdown_privilege() -> Result<(), String> {
    // SAFETY: standard token-privilege dance on our own process token.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return Err("OpenProcessToken failed".to_string());
        }
        let mut luid: LUID = std::mem::zeroed();
        let name = wide("SeShutdownPrivilege");
        if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) == 0 {
            return Err("LookupPrivilegeValueW failed".to_string());
        }
        let mut tp: TOKEN_PRIVILEGES = std::mem::zeroed();
        tp.PrivilegeCount = 1;
        tp.Privileges[0].Luid = luid;
        tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        if AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) == 0
        {
            return Err("AdjustTokenPrivileges failed".to_string());
        }
    }
    Ok(())
}

fn native_arch() -> String {
    // SAFETY: sized struct out-param.
    let info: SYSTEM_INFO = unsafe {
        let mut info = std::mem::zeroed();
        GetNativeSystemInfo(&mut info);
        info
    };
    // SAFETY: the anonymous union's wProcessorArchitecture is always valid.
    let arch = unsafe { info.Anonymous.Anonymous.wProcessorArchitecture };
    match arch {
        PROCESSOR_ARCHITECTURE_AMD64 => "x86_64",
        PROCESSOR_ARCHITECTURE_ARM64 => "aarch64",
        PROCESSOR_ARCHITECTURE_INTEL => "i686",
        _ => "unknown",
    }
    .to_string()
}

fn hostname() -> String {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: wide buffer out-param with in/out length.
    let ok = unsafe {
        GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut len)
    };
    if ok == 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

fn reg_str(value: &str) -> Option<String> {
    let key = wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let val = wide(value);
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

fn reg_dword(value: &str) -> Option<u32> {
    let key = wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let val = wide(value);
    let mut out: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: DWORD out-param.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut out as *mut u32 as *mut _,
            &mut len,
        )
    };
    (rc == ERROR_SUCCESS).then_some(out)
}

fn pwstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: NUL-terminated wide string owned by the adapters buffer.
    unsafe {
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}
