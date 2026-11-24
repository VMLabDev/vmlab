//! The virtio-pci (modern) transport and the virtio devices.
//!
//! [`pci`] wraps any [`VirtioDevice`](crate::VirtioDevice) in a VIRTIO 1.2
//! §4.1 virtio-pci transport: one 64-bit memory BAR carries the common
//! configuration, notification, ISR status, device-specific configuration and
//! MSI-X structures, and legacy INTx is available for guests that prefer it.
//! [`block`] is virtio-blk over the disk-format seam, and [`fs`] is the
//! vhost-user-fs front-end whose queues an external virtiofsd serves.

pub mod block;
pub mod console;
pub mod fs;
pub mod gpu;
pub mod net;
pub mod pci;
pub mod sound;
pub mod worker;

/// Whether the driver's `avail.idx` is one the queue can hold: at most the
/// queue size ahead of the next chain the device takes. A driver that writes
/// an index further ahead has broken the ring. No chain can be taken from it,
/// while `enable_notification` still reports that buffers are waiting, so a
/// drain loop that repeats on that answer must check this first and stop.
pub(crate) fn avail_idx_in_range<M>(queue: &virtio_queue::Queue, mem: &M) -> bool
where
    M: vm_memory::GuestMemory + ?Sized,
{
    use virtio_queue::QueueT as _;
    queue
        .avail_idx(mem, std::sync::atomic::Ordering::Acquire)
        .is_ok_and(|idx| idx.0.wrapping_sub(queue.next_avail()) <= queue.size())
}

/// `VIRTQ_AVAIL_F_NO_INTERRUPT` (VIRTIO 1.2 section 2.7.6).
const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Whether the device interrupts the driver for the buffers it just used.
/// With `EVENT_IDX` the queue's own `used_event` test decides. Without it
/// `Queue::needs_notification` always answers yes, so this reads the flag
/// the driver sets in `avail.flags` to turn interrupts off. Call it only
/// after the device used a buffer.
pub(crate) fn used_needs_interrupt(
    queue: &mut virtio_queue::Queue,
    mem: &crate::GuestMemory,
) -> bool {
    use virtio_queue::QueueT as _;
    use vm_memory::Bytes as _;
    if !queue.needs_notification(mem).unwrap_or(false) {
        return false;
    }
    queue.event_idx_enabled()
        || mem
            .load::<u16>(
                vm_memory::GuestAddress(queue.avail_ring()),
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok_and(|flags| u16::from_le(flags) & AVAIL_F_NO_INTERRUPT == 0)
}

#[cfg(test)]
mod interrupt_tests {
    use virtio_queue::{Queue, QueueT as _};
    use vm_memory::{Bytes as _, GuestAddress};

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;

    fn queue(event_idx: bool) -> Queue {
        let mut queue = Queue::new(16).unwrap();
        queue.try_set_size(16).unwrap();
        queue
            .try_set_desc_table_address(GuestAddress(DESC))
            .unwrap();
        queue
            .try_set_avail_ring_address(GuestAddress(AVAIL))
            .unwrap();
        queue.try_set_used_ring_address(GuestAddress(USED)).unwrap();
        queue.set_event_idx(event_idx);
        queue.set_ready(true);
        queue
    }

    // Review virtio-devices-2, F10.
    #[test]
    fn used_buffers_respect_avail_no_interrupt() {
        let mem = crate::GuestMemory::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let mut classic = queue(false);
        classic.add_used(&mem, 0, 0).unwrap();
        assert!(super::used_needs_interrupt(&mut classic, &mem));
        mem.write_obj(super::AVAIL_F_NO_INTERRUPT.to_le(), GuestAddress(AVAIL))
            .unwrap();
        classic.add_used(&mem, 0, 0).unwrap();
        assert!(!super::used_needs_interrupt(&mut classic, &mem));

        // With EVENT_IDX the flag means nothing: `used_event` decides.
        mem.write_obj(0u16, GuestAddress(USED + 2)).unwrap();
        let mut event_idx = queue(true);
        event_idx.add_used(&mem, 0, 0).unwrap();
        assert!(super::used_needs_interrupt(&mut event_idx, &mem));
    }
}

/// Test doubles shared by the virtio device and transport tests: an
/// in-memory [`vmlab_disk::DiskImage`] and interrupt sinks that record what
/// they delivered.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use vmlab_disk::{DiskImage, Error, Result, SnapshotInfo};

    /// Run `f` on its own thread and give its result, or `None` when it has
    /// not returned after two seconds. For a call that a fault makes spin
    /// forever: the test fails instead of hanging the run.
    pub(crate) fn returns_in_time<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).ok()
    }

    /// An in-memory disk image with a deterministic byte pattern. Content
    /// and discard log are shared by handle so tests can observe them after
    /// the disk is boxed into a device.
    pub(crate) struct MemDisk {
        data: Arc<Mutex<Vec<u8>>>,
        read_only: bool,
        discards: Arc<Mutex<Vec<(u64, u64)>>>,
    }

    impl MemDisk {
        /// Writable disk of `len` bytes; byte i is `(i % 251) as u8`.
        pub fn new(len: usize) -> Self {
            Self {
                data: Arc::new(Mutex::new((0..len).map(|i| (i % 251) as u8).collect())),
                read_only: false,
                discards: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Read-only disk of `len` bytes with the same pattern.
        pub fn new_read_only(len: usize) -> Self {
            let mut d = Self::new(len);
            d.read_only = true;
            d
        }

        /// Shared view of the disk content.
        pub fn content(&self) -> Arc<Mutex<Vec<u8>>> {
            Arc::clone(&self.data)
        }

        /// Shared log of discard hints.
        pub fn discard_log(&self) -> Arc<Mutex<Vec<(u64, u64)>>> {
            Arc::clone(&self.discards)
        }
    }

    impl DiskImage for MemDisk {
        fn size(&self) -> u64 {
            self.data.lock().len() as u64
        }

        fn read_only(&self) -> bool {
            self.read_only
        }

        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let data = self.data.lock();
            let end = offset
                .checked_add(buf.len() as u64)
                .ok_or_else(|| Error::Unsupported("offset overflow".into()))?;
            if end > data.len() as u64 {
                return Err(Error::Unsupported("read past end".into()));
            }
            buf.copy_from_slice(&data[offset as usize..end as usize]);
            Ok(())
        }

        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
            if self.read_only {
                return Err(Error::ReadOnly);
            }
            let mut data = self.data.lock();
            let end = offset
                .checked_add(buf.len() as u64)
                .ok_or_else(|| Error::Unsupported("offset overflow".into()))?;
            if end > data.len() as u64 {
                return Err(Error::Unsupported("write past end".into()));
            }
            data[offset as usize..end as usize].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }

        fn discard(&mut self, offset: u64, len: u64) -> Result<()> {
            if self.read_only {
                return Err(Error::ReadOnly);
            }
            self.discards.lock().push((offset, len));
            Ok(())
        }

        fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
            Ok(Vec::new())
        }

        fn snapshot_create(
            &mut self,
            _name: &str,
            _vm_state_size: u64,
            _vm_clock_nsec: u64,
        ) -> Result<()> {
            Err(Error::Unsupported("memdisk snapshots".into()))
        }

        fn snapshot_apply(&mut self, _name: &str) -> Result<SnapshotInfo> {
            Err(Error::Unsupported("memdisk snapshots".into()))
        }

        fn snapshot_delete(&mut self, _name: &str) -> Result<()> {
            Err(Error::Unsupported("memdisk snapshots".into()))
        }

        fn write_vmstate(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
            Err(Error::Unsupported("memdisk vmstate".into()))
        }

        fn read_vmstate(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<()> {
            Err(Error::Unsupported("memdisk vmstate".into()))
        }
    }

    /// Records delivered MSI/MSI-X messages.
    #[derive(Default)]
    pub(crate) struct RecMsi {
        pub messages: Mutex<Vec<(u64, u32)>>,
    }

    impl crate::MsiSink for RecMsi {
        fn signal(&self, msg: crate::MsiMessage) {
            self.messages.lock().push((msg.address, msg.data));
        }
    }

    /// Records the level of a legacy interrupt line.
    #[derive(Default)]
    pub(crate) struct RecIrq {
        pub level: Mutex<bool>,
    }

    impl crate::IrqLine for RecIrq {
        fn set_level(&self, high: bool) {
            *self.level.lock() = high;
        }
    }
}
