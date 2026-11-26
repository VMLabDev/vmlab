//! Device models for vmlab-hv.
//!
//! Devices are written against the traits in this file and know nothing about
//! KVM: the VMM routes guest port I/O and MMIO to [`BusDevice`]s, PCI
//! configuration to [`PciDevice`]s, and wires interrupts in through
//! [`IrqLine`] and [`MsiSink`]. Virtio devices implement [`VirtioDevice`] and
//! sit behind the virtio-pci transport. Every device that holds guest-visible
//! state implements [`Snapshot`].

use std::sync::Arc;

pub mod ahci;
pub mod applesmc;
pub mod ata;
pub mod audio;
pub mod chipset;
pub mod display;
pub mod floppy;
pub mod ged;
pub mod gic;
pub mod hda;
pub mod ide;
pub mod nic;
pub mod pflash;
pub mod pl011;
pub mod pl031;
pub mod serial;
pub mod tpm;
pub mod usb;
pub mod vdisplay;
pub mod vfio;
pub mod virtio;

#[cfg(test)]
mod qtest_oracle;
#[cfg(test)]
mod virt_oracle;

/// Guest physical memory as devices see it.
pub type GuestMemory = vm_memory::GuestMemoryMmap<WriteDiscard>;

/// One region of [`GuestMemory`].
pub type GuestRegion = vm_memory::GuestRegionMmap<WriteDiscard>;

/// A slice of one region of [`GuestMemory`].
pub type GuestSlice<'a> =
    vm_memory::VolatileSlice<'a, vm_memory::bitmap::RefSlice<'a, WriteDiscard>>;

/// What a region of [`GuestMemory`] does after each write that vm-memory
/// makes to it. This is vm-memory's dirty-bitmap seam, and it is the only
/// place that sees a device write: devices copy straight into the mapping.
///
/// Guest RAM under KVM does nothing here. A region armed with
/// [`WriteDiscard::arm`] is one where a device write may land in a private
/// copy-on-write page (the write-protected PAM banks of the chipset, which
/// store a device write nowhere). After each write there the pages are
/// handed back with `MADV_DONTNEED`: a private page loses the write and reads
/// what it maps again, a shared page loses nothing.
///
/// Code that writes through a raw pointer calls `bitmap().mark_dirty` after
/// the write, as vm-memory asks. A write the kernel makes for another process
/// (vhost) does not come past here.
#[derive(Default)]
pub struct WriteDiscard {
    discard: std::sync::OnceLock<(usize, usize)>,
    watch: std::sync::OnceLock<WriteWatch>,
}

/// What [`WriteDiscard::watch`] calls after each write to a region: the
/// offset of the write in the region, and its length.
pub type WriteWatch = Box<dyn Fn(usize, usize) + Send + Sync>;

impl std::fmt::Debug for WriteDiscard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteDiscard")
            .field("discard", &self.discard.get())
            .field("watched", &self.watch.get().is_some())
            .finish()
    }
}

impl WriteDiscard {
    const PAGE: usize = 4096;

    /// Discard, from now on, the private pages a write leaves in the `len`
    /// bytes mapped at `host`.
    ///
    /// # Safety
    /// `host` is the page-aligned start of the mapping this value tracks,
    /// `len` its length, and every page of it is a file mapping (shared, or
    /// private and never meant to keep a write) for as long as the region
    /// lives.
    pub unsafe fn arm(&self, host: *mut u8, len: usize) {
        let _ = self.discard.set((host as usize, len));
    }

    /// Call `watch`, from now on, after each write that vm-memory makes to
    /// the region. An emulated CPU keeps the write generations of its guest
    /// memory with it: a device write into translated code must end that
    /// code. `false`: the region has a watch already, and keeps it.
    pub fn watch(&self, watch: WriteWatch) -> bool {
        self.watch.set(watch).is_ok()
    }
}

impl<'a> vm_memory::bitmap::WithBitmapSlice<'a> for WriteDiscard {
    type S = vm_memory::bitmap::RefSlice<'a, Self>;
}

impl vm_memory::bitmap::Bitmap for WriteDiscard {
    fn mark_dirty(&self, offset: usize, len: usize) {
        if let Some(watch) = self.watch.get() {
            watch(offset, len);
        }
        let Some(&(host, size)) = self.discard.get() else {
            return;
        };
        // vm-memory checks the range before the write. The clamp keeps a
        // caller's wrong range inside the mapping all the same.
        let start = offset.min(size) & !(Self::PAGE - 1);
        let end = offset
            .saturating_add(len)
            .min(size)
            .next_multiple_of(Self::PAGE)
            .min(size);
        if start >= end {
            return;
        }
        // SAFETY: the range is whole pages inside the mapping `arm` was
        // given, whose pages are all file mappings. MADV_DONTNEED keeps the
        // mapping and drops only private copies, so a thread that reads or
        // writes there at the same time sees the old page or the new one.
        if unsafe { libc::madvise((host + start) as *mut _, end - start, libc::MADV_DONTNEED) } != 0
        {
            log::warn!(
                "discarding a device write: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    fn dirty_at(&self, _: usize) -> bool {
        false
    }

    fn slice_at(&self, offset: usize) -> vm_memory::bitmap::RefSlice<'_, Self> {
        vm_memory::bitmap::RefSlice::new(self, offset)
    }
}

impl vm_memory::bitmap::NewBitmap for WriteDiscard {
    fn with_len(_: usize) -> Self {
        Self::default()
    }
}

/// A level-triggered legacy interrupt line (e.g. the UART's IRQ 4).
pub trait IrqLine: Send + Sync {
    fn set_level(&self, high: bool);
}

/// One MSI or MSI-X message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

/// Delivers MSI messages to the guest.
pub trait MsiSink: Send + Sync {
    fn signal(&self, msg: MsiMessage);
}

/// A device on the port-I/O or MMIO bus. `offset` is relative to the base the
/// device was registered at.
pub trait BusDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]);
    fn write(&mut self, offset: u64, data: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    Mmio32,
    Mmio64 { prefetchable: bool },
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarInfo {
    pub index: u8,
    pub size: u64,
    pub kind: BarKind,
}

/// A PCI function. The VMM's PCI root owns config-space routing and BAR
/// placement; the device owns the contents of its config space and BARs.
pub trait PciDevice: Send {
    /// Notify a direct-mapped device after root-owned BAR placement changes.
    fn set_bar_address(&mut self, _bar: u8, _address: Option<u64>) {}
    /// A fatal asynchronous or config-space host error, if any.
    fn take_error(&mut self) -> Option<String> {
        None
    }
    /// Read a 32-bit config register (`reg` is the dword index).
    fn config_read(&mut self, reg: usize) -> u32;
    /// Write `data` at byte `offset` within dword `reg`.
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]);
    fn bars(&self) -> Vec<BarInfo>;
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]);
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]);
}

/// Interrupt delivery a virtio transport gives its device.
pub trait VirtioInterrupt: Send + Sync {
    fn notify_queue(&self, queue: u16);
    fn notify_config(&self);
}

/// A virtio device behind a transport. Virtio devices hold guest-visible
/// state (queues, features), so they implement [`Snapshot`]; the transport
/// saves its own state and the VMM snapshots the device through it.
pub trait VirtioDevice: Snapshot + Send {
    /// Virtio device id (2 = block, 1 = net, 3 = console, 26 = fs).
    fn device_type(&self) -> u32;
    /// Maximum size of each queue; the length is the queue count.
    fn queue_max_sizes(&self) -> &[u16];
    fn features(&self) -> u64;
    fn ack_features(&mut self, features: u64);
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);
    /// Bytes of device-specific configuration readable at offset 0. The
    /// virtio-pci transport reports this as the device-configuration
    /// capability length.
    fn config_len(&self) -> u64 {
        0
    }

    /// The driver set DRIVER_OK: start using the queues.
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<virtio_queue::Queue>,
    ) -> Result<(), DeviceError>;
    /// The driver kicked queue `index`.
    fn queue_notify(&mut self, index: u16);
    /// One eventfd per queue, for a device that serves its queues off the
    /// calling thread: a write to the event at `index` is the same kick as
    /// `queue_notify(index)`. The transport hands them to
    /// [`QueueDoorbells`] so the kernel can deliver a kick itself.
    fn queue_events(&self) -> Option<Vec<Arc<vmm_sys_util::eventfd::EventFd>>> {
        None
    }
    /// The driver reset the device. Returns to the pre-activation state.
    fn reset(&mut self);
    /// Whether the last [`reset`](Self::reset) has ended every use of the
    /// guest memory and the queues it had, work on other threads and
    /// processes included. A driver frees that memory when it reads device
    /// status zero (VIRTIO 1.2 §2.4), so the transport shows
    /// `DEVICE_NEEDS_RESET` in its place until this is true. It asks after
    /// the reset and again at each driver read of the transport, and the
    /// device may finish its reset in those calls. A device whose reset
    /// stops everything before it returns keeps the default.
    fn reset_settled(&mut self) -> bool {
        true
    }
}

/// A guest memory address whose writes kick one queue.
#[derive(Clone)]
pub struct QueueDoorbell {
    /// Guest-physical address of the queue's notify register. A write of
    /// any length and value is a kick.
    pub address: u64,
    pub event: Arc<vmm_sys_util::eventfd::EventFd>,
}

/// Kernel delivery of queue kicks (KVM's ioeventfd), which the VMM gives a
/// virtio transport. A registered write signals its event in the kernel and
/// does not reach the transport's `bar_write`.
pub trait QueueDoorbells: Send {
    /// Replace every registration this object made with `doorbells`. An
    /// empty slice removes them all. On an error none is left registered.
    fn set(&mut self, doorbells: &[QueueDoorbell]) -> std::io::Result<()>;
}

/// Guest-visible state a device saves into and restores from a snapshot.
/// `Send` because the VMM keeps snapshot handles in its shared state.
pub trait Snapshot: Send {
    /// Stop asynchronous guest-memory access until `resume_snapshot`.
    /// Synchronous devices have no work to stop once vCPUs are parked.
    fn quiesce(&mut self) {}
    fn resume_snapshot(&mut self) {}
    /// Stable id of this device instance in the machine, used as the section key.
    fn state_id(&self) -> String;
    fn save_state(&self) -> Result<Vec<u8>, DeviceError>;
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError>;
}

/// Return volatile guest-visible state to power-on defaults without replacing
/// host connections, attached media, or persistent storage. The VMM calls this
/// only after every vCPU has stopped.
pub trait Resettable: Send {
    fn reset(&mut self) -> Result<(), DeviceError>;
}

/// Layered snapshot access to a PCI function: the function's own state and
/// the state of the device it wraps, as separate sections. The VMM saves
/// and restores the function's state first. A virtio transport activates
/// the wrapped device only after its own state and queue indices have landed.
pub trait PciSnapshot: Resettable {
    fn quiesce(&mut self) {}
    fn resume_snapshot(&mut self) {}
    /// Stable id of this function instance, keying its snapshot sections.
    fn snapshot_id(&self) -> String;
    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError>;
    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError>;
    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError>;
    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError>;
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("guest memory: {0}")]
    Memory(String),
    #[error("snapshot state: {0}")]
    State(String),
    #[error("{0}")]
    Other(String),
}
