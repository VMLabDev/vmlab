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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

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

/// The one shared port handle. Logically closed at most once (poisoning or
/// the last clone dropping); `closed` also gates new I/O so nobody issues a
/// syscall on a handle value the OS may have recycled.
struct PortHandle {
    raw: HANDLE,
    closed: AtomicBool,
}
// SAFETY: the handle is used only through OVERLAPPED I/O calls that are safe
// to issue concurrently from multiple threads.
unsafe impl Send for PortHandle {}
unsafe impl Sync for PortHandle {}

impl PortHandle {
    /// Idempotent close. Runs on a disposable thread when called from
    /// `poison`: an in-flight request the vioserial driver cannot cancel
    /// makes IRP_MJ_CLEANUP (and therefore CloseHandle) block until the host
    /// finally drains it, and that must never hang an agent thread.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            // SAFETY: first (and only) close of a handle we own.
            unsafe { CloseHandle(self.raw) };
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for PortHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// The replaceable port connection shared by the reader and writer sides.
///
/// An online snapshot restore can leave the restored port connection dead
/// while the device itself stays healthy (a sibling port keeps working):
/// pending writes never complete and new I/O sees nothing. qemu-ga
/// recovers by reopening its port on channel errors; this is the same
/// policy. Either side may `poison` the current handle (cancel everything,
/// close it on a disposable thread); only the reader `reopen`s, before its
/// next read.
struct PortShared {
    handle: RwLock<Arc<PortHandle>>,
    poisoned: AtomicBool,
}

impl PortShared {
    fn current(&self) -> Arc<PortHandle> {
        self.handle.read().expect("port handle lock").clone()
    }

    /// Abandon the current connection: cancel every pending I/O on it, mark
    /// it closed for new I/O, and close it off-thread (see
    /// [`PortHandle::close`]). The reader reopens on its next pass.
    fn poison(&self, h: &Arc<PortHandle>) {
        if !h.closed.swap(true, Ordering::SeqCst) {
            // SAFETY: cancel all of this process's pending I/O on the handle;
            // the raw value is still valid — the swap above is what retired
            // it from new use, and the actual CloseHandle happens below.
            unsafe { CancelIoEx(h.raw, std::ptr::null()) };
            let h = h.clone();
            thread::spawn(move || {
                // SAFETY: first close was claimed by the swap; do it here.
                unsafe { CloseHandle(h.raw) };
            });
        }
        self.poisoned.store(true, Ordering::SeqCst);
    }

    /// Replace a poisoned handle with a freshly opened one, retrying until
    /// the driver lets us back in (a stuck cleanup on the old handle keeps
    /// the port exclusive until the host drains it — each host reconnect
    /// attempt helps flush it through).
    fn reopen(&self) {
        let mut guard = self.handle.write().expect("port handle lock");
        if !self.poisoned.load(Ordering::SeqCst) {
            return; // someone else already reopened
        }
        let mut waited = 0u64;
        loop {
            let by_name = open_path(&format!("\\\\.\\Global\\{PORT_NAME}"));
            let fresh = match by_name {
                Ok(h) => Some(h),
                Err(_) => open_by_enumeration(PORT_NAME),
            };
            if let Some(h) = fresh {
                eprintln!("vmlab-agent: reopened port {PORT_NAME}");
                *guard = h;
                self.poisoned.store(false, Ordering::SeqCst);
                return;
            }
            if waited.is_multiple_of(30) {
                eprintln!("vmlab-agent: waiting to reopen port {PORT_NAME}");
            }
            thread::sleep(Duration::from_secs(2));
            waited += 2;
        }
    }
}

/// How long a port write may pend before the connection is declared dead.
/// The host drains the channel continuously when attached, so a stuck write
/// means a detached/desynced host — not backpressure.
const WRITE_STALL: u32 = 15_000;
/// Reader wake-up cadence: each cycle re-checks for poisoning. Not a
/// deadline — an idle healthy port just re-waits.
const READ_CYCLE: u32 = 60_000;
/// Grace for a cancelled request to come back before it is abandoned.
const CANCEL_GRACE: u32 = 2_000;

/// One side of the port (its own OVERLAPPED event; the handle is shared).
pub struct PortIo {
    shared: Arc<PortShared>,
    event: HANDLE,
    /// I/O staging buffer, heap-owned so an uncancelable request can be
    /// abandoned (leaked) without freeing memory the driver may still write.
    staging: Box<[u8]>,
}
// SAFETY: the event is owned by this side exclusively.
unsafe impl Send for PortIo {}

/// Outcome of waiting for one overlapped request.
enum IoWait {
    Done(usize),
    Failed(std::io::Error),
    /// Cancelled-but-never-completed: the caller must leak everything the
    /// request references (staging buffer, OVERLAPPED, event).
    Abandoned,
}

impl PortIo {
    fn new(shared: Arc<PortShared>) -> std::io::Result<Self> {
        Ok(Self {
            shared,
            event: new_event()?,
            staging: vec![0u8; 64 * 1024].into_boxed_slice(),
        })
    }

    /// Wait for a pending request with a per-cycle timeout. `stall_after`
    /// cycles of `cycle_ms` with no completion poisons the connection and
    /// cancels the request; a request that survives even cancellation is
    /// abandoned.
    fn wait_io(
        &mut self,
        h: &Arc<PortHandle>,
        ov: &mut OVERLAPPED,
        cycle_ms: u32,
        stall_cycles: u32,
    ) -> IoWait {
        let mut cycles = 0u32;
        loop {
            // SAFETY: event is valid; ov outlives the request.
            let wait = unsafe { WaitForSingleObject(self.event, cycle_ms) };
            if wait == WAIT_OBJECT_0 {
                let mut done: u32 = 0;
                // SAFETY: request has signalled; collect without blocking.
                let ok = unsafe { GetOverlappedResult(h.raw, ov, &mut done, 0) };
                return if ok != 0 {
                    IoWait::Done(done as usize)
                } else {
                    IoWait::Failed(std::io::Error::last_os_error())
                };
            }
            cycles += 1;
            let poisoned = h.is_closed();
            if !poisoned && cycles < stall_cycles {
                continue; // idle, healthy: keep waiting
            }
            // Stalled (or externally poisoned): tear this connection down.
            self.shared.poison(h);
            // SAFETY: cancel just this request, then give it a moment.
            unsafe { CancelIoEx(h.raw, ov) };
            let wait = unsafe { WaitForSingleObject(self.event, CANCEL_GRACE) };
            if wait == WAIT_OBJECT_0 {
                let mut done: u32 = 0;
                // SAFETY: completed (with data or as cancelled); collect it.
                unsafe { GetOverlappedResult(h.raw, ov, &mut done, 0) };
                return IoWait::Failed(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "port I/O stalled; connection reset",
                ));
            }
            return IoWait::Abandoned;
        }
    }

    /// Leak everything a still-pending request references and re-arm this
    /// side with a fresh event + staging buffer.
    fn abandon(&mut self, h: Arc<PortHandle>, ov: Box<OVERLAPPED>) {
        eprintln!("vmlab-agent: abandoning a port request the driver would not cancel");
        Box::leak(ov);
        let staging = std::mem::replace(&mut self.staging, vec![0u8; 64 * 1024].into_boxed_slice());
        Box::leak(staging);
        std::mem::forget(h); // keep the file object referenced forever
        // The old event stays with the leaked request.
        self.event = new_event().expect("event creation");
    }
}

fn new_event() -> std::io::Result<HANDLE> {
    // SAFETY: plain event creation, manual-reset, non-signaled.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(event)
}

impl Drop for PortIo {
    fn drop(&mut self) {
        // SAFETY: we own the event.
        unsafe { CloseHandle(self.event) };
    }
}

impl Read for PortIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.shared.poisoned.load(Ordering::SeqCst) {
            // Reader owns recovery: replace the poisoned connection first.
            self.shared.reopen();
        }
        let h = self.shared.current();
        if h.is_closed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "port connection is being reset",
            ));
        }
        let want = buf.len().min(self.staging.len());
        // SAFETY: staging buffer and OVERLAPPED are heap-owned and leaked if
        // the request cannot be cancelled; event is ours.
        let mut ov: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        ov.hEvent = self.event;
        let mut done: u32 = 0;
        // SAFETY: see above.
        let ok = unsafe {
            ReadFile(
                h.raw,
                self.staging.as_mut_ptr(),
                want as u32,
                &mut done,
                &mut *ov,
            )
        };
        let n = if ok != 0 {
            done as usize
        } else {
            // SAFETY: just called ReadFile.
            if unsafe { GetLastError() } != ERROR_IO_PENDING {
                return Err(std::io::Error::last_os_error());
            }
            match self.wait_io(&h, &mut ov, READ_CYCLE, u32::MAX) {
                IoWait::Done(n) => n,
                IoWait::Failed(e) => return Err(e),
                IoWait::Abandoned => {
                    self.abandon(h, ov);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "port read abandoned; connection reset",
                    ));
                }
            }
        };
        buf[..n].copy_from_slice(&self.staging[..n]);
        Ok(n)
    }
}

impl Write for PortIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let h = self.shared.current();
        if h.is_closed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "port connection is being reset",
            ));
        }
        let want = buf.len().min(self.staging.len());
        self.staging[..want].copy_from_slice(&buf[..want]);
        // SAFETY: staging buffer and OVERLAPPED are heap-owned and leaked if
        // the request cannot be cancelled; event is ours.
        let mut ov: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        ov.hEvent = self.event;
        let mut done: u32 = 0;
        // SAFETY: see above.
        let ok = unsafe {
            WriteFile(
                h.raw,
                self.staging.as_ptr(),
                want as u32,
                &mut done,
                &mut *ov,
            )
        };
        if ok != 0 {
            return Ok(done as usize);
        }
        // SAFETY: just called WriteFile.
        if unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(std::io::Error::last_os_error());
        }
        // One stall cycle: a write that pends WRITE_STALL is dead (see
        // WRITE_STALL) — poison so the reader reopens.
        match self.wait_io(&h, &mut ov, WRITE_STALL, 1) {
            IoWait::Done(n) => Ok(n),
            IoWait::Failed(e) => Err(e),
            IoWait::Abandoned => {
                self.abandon(h, ov);
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "port write abandoned; connection reset",
                ))
            }
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
    Ok(Arc::new(PortHandle {
        raw: h,
        closed: AtomicBool::new(false),
    }))
}

/// Ask an open vioserial port for its name (vioser.h VIRTIO_PORT_INFO:
/// `UINT Id; BOOLEAN OutVqFull, HostConnected, GuestConnected; CHAR Name[]`).
fn port_name(handle: &PortHandle) -> Option<String> {
    let mut buf = [0u8; 512];
    let mut got: u32 = 0;
    // SAFETY: buffered ioctl into a stack buffer.
    let ok = unsafe {
        DeviceIoControl(
            handle.raw,
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
            let shared = Arc::new(PortShared {
                handle: RwLock::new(handle),
                poisoned: AtomicBool::new(false),
            });
            match (PortIo::new(shared.clone()), PortIo::new(shared)) {
                (Ok(r), Ok(w)) => return (r, w),
                _ => eprintln!("vmlab-agent: event creation failed"),
            }
        } else {
            eprintln!("vmlab-agent: waiting for port {PORT_NAME}");
        }
        thread::sleep(Duration::from_secs(2));
    }
}
