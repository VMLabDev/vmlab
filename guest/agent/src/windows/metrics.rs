//! Windows metrics sampling: GetSystemTimes for CPU, GlobalMemoryStatusEx
//! for memory, fixed logical drives for disk usage.

use vmlab_agent_proto::DiskUsage;

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

use super::port::wide;

/// Cumulative (busy, total) 100ns units from GetSystemTimes. Note: kernel
/// time includes idle time, so busy = kernel + user - idle.
pub type CpuSample = (u64, u64);

fn filetime_u64(ft: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

pub fn cpu_sample() -> CpuSample {
    // SAFETY: three out-params.
    unsafe {
        let mut idle = std::mem::zeroed();
        let mut kernel = std::mem::zeroed();
        let mut user = std::mem::zeroed();
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return (0, 0);
        }
        let (idle, kernel, user) = (
            filetime_u64(&idle),
            filetime_u64(&kernel),
            filetime_u64(&user),
        );
        let total = kernel + user;
        (total.saturating_sub(idle), total)
    }
}

pub fn cpu_pct(prev: &CpuSample, cur: &CpuSample) -> f32 {
    let busy = cur.0.saturating_sub(prev.0) as f32;
    let total = cur.1.saturating_sub(prev.1) as f32;
    if total <= 0.0 {
        0.0
    } else {
        (100.0 * busy / total).clamp(0.0, 100.0)
    }
}

/// (used, total) bytes of physical memory.
pub fn mem_sample() -> (u64, u64) {
    // SAFETY: sized struct out-param.
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return (0, 0);
        }
        (
            status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
            status.ullTotalPhys,
        )
    }
}

pub fn disk_sample() -> Vec<DiskUsage> {
    const DRIVE_FIXED: u32 = 3;
    let mut out = Vec::new();
    // SAFETY: bitmask query + per-drive stat calls with wide strings.
    unsafe {
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let wroot = wide(&root);
            if GetDriveTypeW(wroot.as_ptr()) != DRIVE_FIXED {
                continue;
            }
            let mut free: u64 = 0;
            let mut total: u64 = 0;
            let mut total_free: u64 = 0;
            if GetDiskFreeSpaceExW(wroot.as_ptr(), &mut free, &mut total, &mut total_free) == 0
                || total == 0
            {
                continue;
            }
            out.push(DiskUsage {
                mount: root,
                used: total.saturating_sub(total_free),
                total,
            });
        }
    }
    out
}
