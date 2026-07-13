//! Opening and reading/writing the vioserial port on Windows.
//!
//! The virtio-win vioserial driver exposes each port two ways: a DosDevices
//! symlink named after the QEMU `virtserialport` name (open
//! `\\.\Global\vmlab.agent.0` directly — the same mechanism qemu-ga and the
//! SPICE agent use), and a device interface under GUID
//! `{6FDE7547-1B65-48AE-B628-80BE62016026}` whose ports answer
//! `IOCTL_GET_INFORMATION` with their name — the fallback when the symlink
//! is missing (locale/driver quirks).
//!
//! The driver enforces **one open handle per port**. Reader and writer
//! threads share that one handle, which therefore must be OVERLAPPED —
//! synchronous handles serialize I/O on the file object, so a blocking read
//! would starve writes. Each caller brings its own OVERLAPPED + event.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::CreateEventW;

use vmlab_agent_proto::PORT_NAME;

/// vioserial port device-interface class (virtio-win `vioser.h`
/// GUID_VIOSERIAL_PORT {6FDE7547-1B65-48AE-B628-80BE62016026}).
const GUID_VIOSERIAL_PORT: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x6FDE7547,
    data2: 0x1B65,
    data3: 0x48AE,
    data4: [0xB6, 0x28, 0x80, 0xBE, 0x62, 0x01, 0x60, 0x26],
};

/// vioser.h `IOCTL_GET_INFORMATION`:
/// CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS).
const IOCTL_GET_INFORMATION: u32 = (0x22 << 16) | (0x800 << 2);

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The one shared port handle. Closed when the last clone drops.
struct PortHandle(HANDLE);
// SAFETY: the handle is used only through OVERLAPPED I/O calls that are safe
// to issue concurrently from multiple threads.
unsafe impl Send for PortHandle {}
unsafe impl Sync for PortHandle {}

impl Drop for PortHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.0) };
    }
}

/// One side of the port (its own OVERLAPPED event; the handle is shared).
pub struct PortIo {
    handle: Arc<PortHandle>,
    event: HANDLE,
}
// SAFETY: the event is owned by this side exclusively.
unsafe impl Send for PortIo {}

impl PortIo {
    fn new(handle: Arc<PortHandle>) -> std::io::Result<Self> {
        // SAFETY: plain event creation, manual-reset, non-signaled.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { handle, event })
    }

    fn finish(&self, ov: &mut OVERLAPPED, pending: bool) -> std::io::Result<usize> {
        let mut done: u32 = 0;
        if pending {
            // SAFETY: ov/event live for the duration; bWait blocks until done.
            let ok = unsafe { GetOverlappedResult(self.handle.0, ov, &mut done, 1) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(done as usize)
    }
}

impl Drop for PortIo {
    fn drop(&mut self) {
        // SAFETY: we own the event.
        unsafe { CloseHandle(self.event) };
    }
}

impl Read for PortIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: buffer and OVERLAPPED outlive the call + wait.
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.event;
            let mut done: u32 = 0;
            let ok = ReadFile(
                self.handle.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut done,
                &mut ov,
            );
            if ok != 0 {
                return Ok(done as usize);
            }
            if GetLastError() != ERROR_IO_PENDING {
                return Err(std::io::Error::last_os_error());
            }
            self.finish(&mut ov, true)
        }
    }
}

impl Write for PortIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: buffer and OVERLAPPED outlive the call + wait.
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.event;
            let mut done: u32 = 0;
            let ok = WriteFile(
                self.handle.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut done,
                &mut ov,
            );
            if ok != 0 {
                return Ok(done as usize);
            }
            if GetLastError() != ERROR_IO_PENDING {
                return Err(std::io::Error::last_os_error());
            }
            self.finish(&mut ov, true)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn open_path(path: &str) -> std::io::Result<Arc<PortHandle>> {
    // SAFETY: CreateFileW with a NUL-terminated wide path; share mode 0
    // (vioserial ports are exclusive-open anyway).
    let h = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Arc::new(PortHandle(h)))
}

/// Ask an open vioserial port for its name (vioser.h VIRTIO_PORT_INFO:
/// `UINT Id; BOOLEAN OutVqFull, HostConnected, GuestConnected; CHAR Name[]`).
fn port_name(handle: &PortHandle) -> Option<String> {
    let mut buf = [0u8; 512];
    let mut got: u32 = 0;
    // SAFETY: buffered ioctl into a stack buffer.
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_GET_INFORMATION,
            std::ptr::null(),
            0,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || got < 8 {
        return None;
    }
    let name = &buf[7..got as usize];
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Some(String::from_utf8_lossy(&name[..end]).into_owned())
}

/// Enumerate vioserial device interfaces and open the one whose
/// IOCTL-reported name matches. The exclusive-open rule means probing a
/// port that some other software holds fails — those are skipped.
fn open_by_enumeration(want: &str) -> Option<Arc<PortHandle>> {
    // SAFETY: standard SetupDi enumeration sequence; all buffers are local.
    unsafe {
        let devs = SetupDiGetClassDevsW(
            &GUID_VIOSERIAL_PORT,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        // HDEVINFO's invalid value is -1 (it is not a HANDLE in windows-sys).
        if devs == -1isize {
            return None;
        }
        let mut found = None;
        for index in 0.. {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(
                devs,
                std::ptr::null(),
                &GUID_VIOSERIAL_PORT,
                index,
                &mut iface,
            ) == 0
            {
                break;
            }
            // Detail data = { cbSize: u32, DevicePath: [u16] } — grab it
            // into a raw buffer and read the path from offset 4.
            let mut needed: u32 = 0;
            SetupDiGetDeviceInterfaceDetailW(
                devs,
                &iface,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            );
            if needed == 0 || needed > 4096 {
                continue;
            }
            let mut detail = vec![0u8; needed as usize];
            // cbSize is the size of the *fixed* part of the struct.
            let cb_size: u32 = 4 + std::mem::size_of::<u16>() as u32;
            detail[..4].copy_from_slice(&cb_size.to_ne_bytes());
            if SetupDiGetDeviceInterfaceDetailW(
                devs,
                &iface,
                detail.as_mut_ptr() as *mut _,
                needed,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
            {
                continue;
            }
            let path_u16: Vec<u16> = detail[4..]
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let path = String::from_utf16_lossy(&path_u16);
            let Ok(handle) = open_path(&path) else {
                continue; // busy: someone else's port
            };
            if port_name(&handle).as_deref() == Some(want) {
                found = Some(handle);
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(devs);
        found
    }
}

/// Open the agent port, retrying until the device exists (the service can
/// start before the vioserial driver binds). A busy port means another
/// agent instance is serving: exit quietly so double-starts are harmless.
pub fn open_port() -> (PortIo, PortIo) {
    const ERROR_BUSY: i32 = 170; // ERROR_BUSY as io error raw code
    loop {
        let by_name = open_path(&format!("\\\\.\\Global\\{PORT_NAME}"));
        let handle = match by_name {
            Ok(h) => Some(h),
            Err(e)
                if e.raw_os_error() == Some(ERROR_BUSY)
                    || e.raw_os_error() == Some(32 /* SHARING_VIOLATION */)
                    || e.raw_os_error() == Some(5 /* ACCESS_DENIED (held) */) =>
            {
                eprintln!("vmlab-agent: port {PORT_NAME} busy (another instance is serving)");
                std::process::exit(0);
            }
            Err(_) => open_by_enumeration(PORT_NAME),
        };
        if let Some(handle) = handle {
            match (PortIo::new(handle.clone()), PortIo::new(handle)) {
                (Ok(r), Ok(w)) => return (r, w),
                _ => eprintln!("vmlab-agent: event creation failed"),
            }
        } else {
            eprintln!("vmlab-agent: waiting for port {PORT_NAME}");
        }
        thread::sleep(Duration::from_secs(2));
    }
}
