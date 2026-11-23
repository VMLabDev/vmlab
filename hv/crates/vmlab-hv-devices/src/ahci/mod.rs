//! ICH9 SATA AHCI function for q35 at 00:1f.2.
//!
//! Register and DMA layouts follow Intel AHCI 1.3.1, sections 3-5 and 10.
//! ATA commands follow T13/1410D, and optical commands follow T10 MMC-4.
//! PCI identity and capabilities match the q35 hardware captures. Port
//! multipliers and power management are not advertised. Disk contents belong
//! to the VMM snapshot.
//!
//! A command that reads, writes or flushes a backing image runs on the
//! controller's worker thread and completes with an interrupt, so the vCPU
//! that set its PxCI bit returns to the guest at once. A vCPU held in a host
//! `fsync` cannot answer an IPI, and macOS panics on one that goes unanswered.
//! Every other command completes inside the register write. A port runs one
//! command at a time, tagged NCQ commands included, in slot order.
//!
//! The register state sits behind an inner lock the worker takes only to
//! start and to finish a command, never across the I/O. What cancels a
//! command list (a cleared PxCMD.ST, a COMRESET, an HBA reset) discards the
//! completion of the command in flight and waits for its I/O to end before
//! the register write returns. A snapshot, a restore and a machine reset
//! wait until no command is outstanding, so a saved state never holds one.

#[cfg(test)]
mod tests;

use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex, MutexGuard};
use vm_memory::{Bytes, GuestAddress, GuestMemory as _, Permissions};
use vmlab_disk::DiskImage;

use crate::ata::{AtaDisk, Drive, Payload, Transport, atapi::Optical};
use crate::{
    BarInfo, BarKind, BusDevice, DeviceError, GuestMemory, IrqLine, MsiMessage, MsiSink, PciDevice,
    PciSnapshot, Snapshot,
};

const PORTS: usize = 6;
const REGS: usize = 0x1000;
const CAP: u32 = (1 << 31) | (1 << 30) | (1 << 20) | (1 << 18) | (31 << 8) | 5;
const DHRS: u32 = 1;
const PSS: u32 = 2;
const DSS: u32 = 4;
const HBFS: u32 = 1 << 29;
const TFES: u32 = 1 << 30;

fn port_base(port: usize) -> usize {
    0x100 + port * 0x80
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

pub use crate::ata::MediaState;

/// The most one image read or guest-memory copy moves at a time.
const CHUNK: usize = 64 * 1024;

/// A drive's backing image as the drive and the worker both hold it. The
/// worker locks it for the length of one command's I/O and holds no other
/// lock meanwhile. Size and writability never change under an open image, so
/// the register path reads them here and never waits on that lock.
#[derive(Clone)]
struct SharedImage {
    image: Arc<Mutex<Box<dyn DiskImage>>>,
    size: u64,
    read_only: bool,
}

impl SharedImage {
    fn new(image: Box<dyn DiskImage>) -> Self {
        Self {
            size: image.size(),
            read_only: image.read_only(),
            image: Arc::new(Mutex::new(image)),
        }
    }
}

impl DiskImage for SharedImage {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        self.image.lock().read_at(offset, buf)
    }
    fn read_into(
        &mut self,
        offset: u64,
        len: usize,
        target: &mut dyn vmlab_disk::ReadTarget,
    ) -> vmlab_disk::Result<()> {
        self.image.lock().read_into(offset, len, target)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        self.image.lock().write_at(offset, buf)
    }
    fn flush(&mut self) -> vmlab_disk::Result<()> {
        self.image.lock().flush()
    }
    fn discard(&mut self, offset: u64, len: u64) -> vmlab_disk::Result<()> {
        self.image.lock().discard(offset, len)
    }
    fn snapshots(&self) -> vmlab_disk::Result<Vec<vmlab_disk::SnapshotInfo>> {
        self.image.lock().snapshots()
    }
    fn snapshot_create(
        &mut self,
        name: &str,
        vm_state_size: u64,
        vm_clock_nsec: u64,
    ) -> vmlab_disk::Result<()> {
        self.image
            .lock()
            .snapshot_create(name, vm_state_size, vm_clock_nsec)
    }
    fn snapshot_apply(&mut self, name: &str) -> vmlab_disk::Result<vmlab_disk::SnapshotInfo> {
        self.image.lock().snapshot_apply(name)
    }
    fn snapshot_delete(&mut self, name: &str) -> vmlab_disk::Result<()> {
        self.image.lock().snapshot_delete(name)
    }
    fn write_vmstate(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        self.image.lock().write_vmstate(offset, buf)
    }
    fn discard_vmstate(&mut self, len: u64) -> vmlab_disk::Result<()> {
        self.image.lock().discard_vmstate(len)
    }
    fn read_vmstate(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        self.image.lock().read_vmstate(offset, buf)
    }
}

/// What the worker does to the backing image for one command.
enum Io {
    Read {
        offset: u64,
        len: usize,
    },
    Write {
        offset: u64,
        data: Vec<u8>,
        flush: bool,
    },
    Flush,
}

/// What a command resolved to inside the register write.
enum Exec {
    /// The whole response, with no image I/O left to do.
    Bytes(Vec<u8>),
    /// Image I/O for the worker.
    Io(Io),
}

/// The parsed command header and table of one slot.
struct Request {
    slot: usize,
    header_addr: u64,
    cfis: [u8; 20],
    /// Address, length and interrupt-on-completion of each PRD entry.
    prds: Vec<(u64, usize, bool)>,
}

/// How a command ended: the task-file status and error, the byte count for
/// the command header, and whether a fully used PRD asked for an interrupt.
struct Done {
    status: u8,
    error: u8,
    transferred: usize,
    ioc: bool,
}

impl Done {
    fn error(error: u8) -> Self {
        Self {
            status: 0x51,
            error,
            transferred: 0,
            ioc: false,
        }
    }
}

/// One command on its way through the worker.
struct Job {
    port: usize,
    /// The port's generation when the command started. A completion whose
    /// generation is no longer the port's is discarded.
    generation: u64,
    request: Request,
    io: Io,
    image: SharedImage,
}

enum Message {
    Job(Box<Job>),
    Stop,
}

/// Walk a PRD table for `expected` bytes, handing each piece to `chunk` as
/// (guest address, bytes already moved, length). `chunk` answers `Ok(false)`
/// for a device error, which ends the walk, and `Err` for a DMA fault. The
/// result is the bytes moved and whether a fully used PRD had its interrupt
/// bit set.
fn walk(
    prds: &[(u64, usize, bool)],
    expected: usize,
    mut chunk: impl FnMut(u64, usize, usize) -> Result<bool, ()>,
) -> Result<(usize, bool), ()> {
    let mut copied = 0;
    let mut ioc = false;
    for &(address, len, interrupt) in prds {
        let n = len.min(expected - copied);
        if n == 0 {
            break;
        }
        let mut done = 0;
        let mut failed = false;
        while done < n {
            let count = (n - done).min(CHUNK);
            if !chunk(address + done as u64, copied, count)? {
                failed = true;
                break;
            }
            copied += count;
            done += count;
        }
        if done == len && interrupt {
            ioc = true;
        }
        if copied == expected || failed {
            break;
        }
    }
    Ok((copied, ioc))
}

impl Job {
    /// The image I/O and the guest-memory copy of one command. Runs on the
    /// worker with no controller lock held. `Err` is a DMA fault.
    fn perform(&self, memory: &GuestMemory) -> Result<Done, ()> {
        let mut image = self.image.image.lock();
        match &self.io {
            Io::Read { offset, len } => {
                let mut buffer = vec![0; CHUNK.min(*len)];
                let mut failed = false;
                let (transferred, ioc) = walk(&self.request.prds, *len, |address, copied, n| {
                    if image
                        .read_at(offset + copied as u64, &mut buffer[..n])
                        .is_err()
                    {
                        failed = true;
                        return Ok(false);
                    }
                    memory
                        .write_slice(&buffer[..n], GuestAddress(address))
                        .map_err(|_| ())?;
                    Ok(true)
                })?;
                Ok(Done {
                    status: if failed { 0x51 } else { 0x50 },
                    error: if failed { 0x40 } else { 0 },
                    transferred,
                    ioc,
                })
            }
            Io::Write {
                offset,
                data,
                flush,
            } => {
                if image.write_at(*offset, data).is_err() || (*flush && image.flush().is_err()) {
                    return Ok(Done::error(0x40));
                }
                let (transferred, ioc) = walk(&self.request.prds, data.len(), |_, _, _| Ok(true))?;
                Ok(Done {
                    status: 0x50,
                    error: 0,
                    transferred,
                    ioc,
                })
            }
            Io::Flush => Ok(if image.flush().is_err() {
                Done::error(0x40)
            } else {
                Done {
                    status: 0x50,
                    error: 0,
                    transferred: 0,
                    ioc: false,
                }
            }),
        }
    }
}

fn worker(shared: &Shared, memory: &GuestMemory, queue: &mpsc::Receiver<Message>) {
    while let Ok(Message::Job(job)) = queue.recv() {
        let outcome = job.perform(memory);
        shared.core.lock().complete(&job, outcome);
        shared.done.notify_all();
    }
}

struct Shared {
    core: Mutex<Core>,
    /// Signalled after every completion, discarded ones included.
    done: Condvar,
}

/// An AHCI PCI function with six independently attachable SATA devices.
pub struct Ahci {
    id: String,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// The controller's registers and drives, behind [`Shared::core`].
struct Core {
    memory: GuestMemory,
    irq: Arc<dyn IrqLine>,
    msi: Arc<dyn MsiSink>,
    config: [u8; 256],
    regs: [u8; REGS],
    drives: [Option<Drive>; PORTS],
    /// The image of each drive that has one, for the worker.
    images: [Option<SharedImage>; PORTS],
    index: u32,
    msi_pending: u32,
    /// A port with a command on the worker starts no other until it ends.
    busy: [bool; PORTS],
    generation: [u64; PORTS],
    /// Cancelled commands whose I/O the worker has not finished.
    stale: usize,
    jobs: mpsc::Sender<Message>,
}

fn power_on_config() -> [u8; 256] {
    let mut config = [0; 256];
    config[..4].copy_from_slice(&0x2922_8086u32.to_le_bytes());
    config[6] = 0x10;
    config[8..12].copy_from_slice(&0x0106_0102u32.to_le_bytes());
    config[14] = 0x80;
    config[0x20] = 1;
    config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
    config[0x34] = 0x80;
    config[0x3c] = 10;
    config[0x3d] = 1;
    config[0x80..0x84].copy_from_slice(&[5, 0xa8, 0x80, 0]);
    config[0x90] = 0x40;
    config[0x92] = 0x3f;
    config[0xa8..0xb0].copy_from_slice(&[0x12, 0, 0x10, 0, 0x48, 0, 0, 0]);
    config
}

impl Ahci {
    /// Register BAR4 as 32 bytes of I/O and BAR5 as 4 KiB of MMIO.
    /// `irq` is PCI INTx pin A, routed by the machine's PCI root.
    pub fn new(
        id: impl Into<String>,
        memory: GuestMemory,
        irq: Arc<dyn IrqLine>,
        msi: Arc<dyn MsiSink>,
    ) -> Self {
        let (jobs, queue) = mpsc::channel();
        let mut core = Core {
            memory: memory.clone(),
            irq,
            msi,
            config: power_on_config(),
            regs: [0; REGS],
            drives: std::array::from_fn(|_| None),
            images: std::array::from_fn(|_| None),
            index: 0,
            msi_pending: 0,
            busy: [false; PORTS],
            generation: [0; PORTS],
            stale: 0,
            jobs,
        };
        core.reset();
        let shared = Arc::new(Shared {
            core: Mutex::new(core),
            done: Condvar::new(),
        });
        let worker = std::thread::Builder::new()
            .name("ahci".into())
            .spawn({
                let shared = shared.clone();
                move || worker(&shared, &memory, &queue)
            })
            .expect("spawn the AHCI worker thread");
        Self {
            id: id.into(),
            shared,
            worker: Some(worker),
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.shared.core.lock()
    }

    /// The controller with no command outstanding: every command the guest
    /// issued has completed and no cancelled one is still doing I/O. The
    /// caller has stopped the vCPUs, or accepts commands issued after this.
    fn idle(&self) -> MutexGuard<'_, Core> {
        let mut core = self.core();
        while core.stale != 0 || core.busy.contains(&true) {
            self.shared.done.wait(&mut core);
        }
        core
    }

    /// End a register write: wait for the I/O of every command it cancelled.
    fn drained(&self, mut core: MutexGuard<'_, Core>) {
        while core.stale != 0 {
            self.shared.done.wait(&mut core);
        }
    }

    /// Wait until no command is outstanding.
    pub fn wait_idle(&self) {
        drop(self.idle());
    }

    /// Attach an ATA disk before the guest starts. A running port is rejected.
    pub fn attach_disk(
        &mut self,
        port: usize,
        image: Box<dyn DiskImage>,
    ) -> Result<(), DeviceError> {
        if image.size() < 512 || !image.size().is_multiple_of(512) || image.size() / 512 >= 1 << 48
        {
            return Err(DeviceError::Other(
                "ATA disk size must be a nonzero multiple of 512 bytes within LBA48".into(),
            ));
        }
        let image = SharedImage::new(image);
        self.core().attach(
            port,
            Drive::Disk(AtaDisk {
                image: Box::new(image.clone()),
                write_cache: true,
                transfer_mode: 0x22,
                ncq_error: [0; 24],
            }),
            Some(image),
        )
    }

    /// Attach a removable CD-ROM drive, optionally with read-only ISO media.
    pub fn attach_cdrom(
        &mut self,
        port: usize,
        image: Option<Box<dyn DiskImage>>,
    ) -> Result<(), DeviceError> {
        Optical::validate_media(image.as_deref())?;
        let image = image.map(SharedImage::new);
        let media = image
            .clone()
            .map(|image| Box::new(image) as Box<dyn DiskImage>);
        self.core()
            .attach(port, Drive::Cdrom(Optical::new(media)), image)
    }

    /// Replace optical media and close the tray. Guest removal locks apply.
    pub fn insert_media(
        &mut self,
        port: usize,
        image: Box<dyn DiskImage>,
    ) -> Result<(), DeviceError> {
        let image = SharedImage::new(image);
        let mut core = self.core();
        core.optical_mut(port)?
            .insert_media(Box::new(image.clone()))?;
        core.images[port] = Some(image);
        Ok(())
    }

    /// Open the tray and remove the host image. Guest removal locks apply.
    pub fn eject_media(&mut self, port: usize) -> Result<Option<Box<dyn DiskImage>>, DeviceError> {
        let mut core = self.core();
        let image = core.optical_mut(port)?.eject_media()?;
        core.forget_removed_image(port);
        Ok(image)
    }

    /// Remove the host image whatever the guest locked, leaving the tray
    /// closed and the drive empty.
    pub fn force_eject_media(
        &mut self,
        port: usize,
    ) -> Result<Option<Box<dyn DiskImage>>, DeviceError> {
        let mut core = self.core();
        let image = core.optical_mut(port)?.force_eject();
        core.forget_removed_image(port);
        Ok(image)
    }

    pub fn media_state(&self, port: usize) -> Result<MediaState, DeviceError> {
        match self.core().drives.get(port) {
            Some(Some(Drive::Cdrom(optical))) => Ok(optical.media_state()),
            _ => Err(DeviceError::Other("AHCI port is not a CD-ROM".into())),
        }
    }

    /// Access an ATA backing image for the VMM's disk snapshot transaction,
    /// once no command is outstanding.
    pub fn with_disk<R>(
        &mut self,
        port: usize,
        access: impl FnOnce(&mut dyn DiskImage) -> R,
    ) -> Option<R> {
        match self.idle().drives.get_mut(port) {
            Some(Some(Drive::Disk(disk))) => Some(access(disk.image.as_mut())),
            _ => None,
        }
    }
}

impl Drop for Ahci {
    fn drop(&mut self) {
        // The worker ends the command it is in, then leaves.
        let _ = self.core().jobs.send(Message::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Core {
    fn attach(
        &mut self,
        port: usize,
        drive: Drive,
        image: Option<SharedImage>,
    ) -> Result<(), DeviceError> {
        if port >= PORTS
            || self.get32(port_base(port) + 0x18) & 0x11 != 0
            || self.drives[port].is_some()
        {
            return Err(DeviceError::Other(
                "AHCI port is invalid, running or occupied".into(),
            ));
        }
        self.drives[port] = Some(drive);
        self.images[port] = image;
        self.reset_port(port);
        Ok(())
    }

    fn optical_mut(&mut self, port: usize) -> Result<&mut Optical, DeviceError> {
        match self.drives.get_mut(port) {
            Some(Some(Drive::Cdrom(optical))) => Ok(optical),
            _ => Err(DeviceError::Other("AHCI port is not a CD-ROM".into())),
        }
    }

    fn forget_removed_image(&mut self, port: usize) {
        if matches!(&self.drives[port], Some(Drive::Cdrom(optical)) if optical.image.is_none()) {
            self.images[port] = None;
        }
    }

    /// Cancel the command a port has on the worker. Its completion is
    /// discarded, and the register write that got here waits for its I/O.
    fn cancel(&mut self, port: usize) {
        if self.busy[port] {
            self.busy[port] = false;
            self.generation[port] += 1;
            self.stale += 1;
        }
    }

    /// The worker's end of a command: apply its result and start the next.
    fn complete(&mut self, job: &Job, outcome: Result<Done, ()>) {
        let port = job.port;
        if job.generation != self.generation[port] {
            self.stale -= 1;
            return;
        }
        self.busy[port] = false;
        let result = match outcome {
            Ok(mut done) => {
                if done.error != 0
                    && matches!(job.io, Io::Read { .. })
                    && let Some(Drive::Cdrom(optical)) = self.drives[port].as_mut()
                {
                    optical.sense = [3, 0x11, 0];
                    done.error = 0x34;
                }
                self.finish(port, &job.request, done)
            }
            Err(()) => Err(()),
        };
        if self.settle(port, job.request.slot, result) {
            self.run(port);
        }
        self.update_irq();
    }

    fn get32(&self, offset: usize) -> u32 {
        le32(&self.regs[offset..])
    }
    fn put32(&mut self, offset: usize, value: u32) {
        self.regs[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn address(&self, offset: usize) -> u64 {
        le64(&self.regs[offset..])
    }

    fn reset(&mut self) {
        self.regs.fill(0);
        self.put32(0, CAP);
        self.put32(4, 1 << 31);
        self.put32(0xc, 0x3f);
        self.put32(0x10, 0x0001_0300);
        for port in 0..PORTS {
            self.reset_port(port);
        }
        self.index = 0;
        self.msi_pending = 0;
        self.irq.set_level(false);
    }

    fn reset_port(&mut self, port: usize) {
        self.cancel(port);
        let base = port_base(port);
        self.regs[base..base + 0x80].fill(0);
        let signature = self.drives[port].as_ref().map(Drive::signature);
        self.put32(base + 0x20, if signature.is_some() { 0x50 } else { 0x7f });
        self.put32(base + 0x24, signature.unwrap_or(0xffff_ffff));
        self.put32(base + 0x28, if signature.is_some() { 0x113 } else { 0 });
        self.put32(base + 0x18, 6);
        if let Some(Drive::Cdrom(optical)) = self.drives[port].as_mut() {
            optical.locked = false;
            optical.sense = [0; 3];
        }
        if let Some(Drive::Disk(disk)) = self.drives[port].as_mut() {
            disk.ncq_error = [0; 24];
        }
    }

    fn update_irq(&mut self) {
        let pending = (0..PORTS).fold(0, |mask, port| {
            let base = port_base(port);
            mask | (u32::from(self.get32(base + 0x10) & self.get32(base + 0x14) != 0) << port)
        });
        // Global IS is sticky until the guest acknowledges it.
        self.put32(8, self.get32(8) | pending);
        let active = self.get32(8) != 0 && self.get32(4) & 2 != 0;
        let enabled = self.config[0x82] & 1 != 0;
        self.config[6] = 0x10 | if active && !enabled { 8 } else { 0 };
        self.irq
            .set_level(active && !enabled && self.config[5] & 4 == 0);
        let pending = if active && enabled { self.get32(8) } else { 0 };
        if pending & !self.msi_pending != 0 {
            self.msi.signal(MsiMessage {
                address: le64(&self.config[0x84..]),
                data: u16::from_le_bytes([self.config[0x8c], self.config[0x8d]]) as u32,
            });
        }
        self.msi_pending = pending;
    }

    fn dma_read(&self, address: u64, bytes: &mut [u8]) -> Result<(), ()> {
        self.memory
            .read_slice(bytes, GuestAddress(address))
            .map_err(|_| ())
    }
    fn dma_write(&self, address: u64, bytes: &[u8]) -> Result<(), ()> {
        self.memory
            .write_slice(bytes, GuestAddress(address))
            .map_err(|_| ())
    }

    fn fis(
        &mut self,
        port: usize,
        status: u8,
        error: u8,
        source: &[u8; 20],
        pio: Option<usize>,
    ) -> Result<(), ()> {
        let base = port_base(port);
        self.put32(base + 0x20, status as u32 | ((error as u32) << 8));
        if self.get32(base + 0x18) & 0x10 == 0 || self.config[4] & 4 == 0 {
            return Ok(());
        }
        let fb = self.address(base + 8);
        let mut fis = *source;
        fis[0] = 0x34;
        fis[1] = 0x40;
        fis[2] = status;
        fis[3] = error;
        fis[14..].fill(0);
        self.dma_write(fb.checked_add(0x40).ok_or(())?, &fis)?;
        if let Some(count) = pio {
            fis[0] = 0x5f;
            fis[1] = 0x60;
            fis[2] = 0x58;
            fis[15] = status;
            fis[16..18].copy_from_slice(&(count.min(65534) as u16).to_le_bytes());
            self.dma_write(fb.checked_add(0x20).ok_or(())?, &fis)?;
        }
        Ok(())
    }

    fn initial_fis(&mut self, port: usize) {
        if self.config[4] & 4 == 0 {
            return;
        }
        let Some(drive) = &self.drives[port] else {
            return;
        };
        let sig = drive.signature().to_le_bytes();
        let mut fis = [0; 20];
        fis[4] = sig[1];
        fis[5] = sig[2];
        fis[6] = sig[3];
        fis[12] = sig[0];
        if self.fis(port, 0x50, 1, &fis, None).is_err() {
            self.port_interrupt(port, HBFS);
        } else {
            self.port_interrupt(port, DHRS);
        }
    }

    fn port_interrupt(&mut self, port: usize, flags: u32) {
        let offset = port_base(port) + 0x10;
        self.put32(offset, self.get32(offset) | flags);
        if flags & (TFES | HBFS) != 0 {
            let cmd = port_base(port) + 0x18;
            self.put32(cmd, self.get32(cmd) & !(1 << 15));
        }
    }

    fn run(&mut self, port: usize) {
        let base = port_base(port);
        if self.busy[port]
            || self.config[4] & 4 == 0
            || self.get32(4) & (1 << 31) == 0
            || self.get32(base + 0x18) & 0x8011 != 0x8011
            || self.get32(base + 0x28) & 0xf != 3
        {
            return;
        }
        for slot in 0..32 {
            if self.get32(base + 0x38) & (1 << slot) == 0 {
                continue;
            }
            self.put32(
                base + 0x18,
                (self.get32(base + 0x18) & !0x1f00) | ((slot as u32) << 8),
            );
            let result = match self.command(port, slot) {
                Ok(Some(job)) => {
                    // The worker finishes this slot, then runs the port again.
                    self.busy[port] = true;
                    let _ = self.jobs.send(Message::Job(job));
                    return;
                }
                Ok(None) => Ok(()),
                Err(()) => Err(()),
            };
            if !self.settle(port, slot, result) {
                break;
            }
        }
    }

    /// Close a slot after its command. False means the command list halted.
    fn settle(&mut self, port: usize, slot: usize, result: Result<(), ()>) -> bool {
        let base = port_base(port);
        if result.is_err() {
            self.port_interrupt(port, HBFS);
        }
        // A task-file or host-bus error halts this command list until ST is cleared.
        if self.get32(base + 0x18) & (1 << 15) == 0 {
            return false;
        }
        self.put32(base + 0x38, self.get32(base + 0x38) & !(1 << slot));
        true
    }

    /// Parse the command in a slot and run it. A command with image I/O
    /// comes back as a job for the worker, any other is complete on return.
    fn command(&mut self, port: usize, slot: usize) -> Result<Option<Box<Job>>, ()> {
        let base = port_base(port);
        let header_addr = self
            .address(base)
            .checked_add((slot * 32) as u64)
            .ok_or(())?;
        let mut header = [0; 32];
        self.dma_read(header_addr, &mut header)?;
        if !self.memory.check_range(
            GuestAddress(self.address(base + 8)),
            256,
            Permissions::Write,
        ) {
            return Err(());
        }
        self.dma_write(header_addr.checked_add(4).ok_or(())?, &0u32.to_le_bytes())?;
        let flags = u16::from_le_bytes([header[0], header[1]]);
        if flags & 0xf000 != 0 || flags & 0x1f != 5 {
            return Err(());
        }
        let table = le64(&header[8..]);
        if table & 0x7f != 0 {
            return Err(());
        }
        let mut cfis = [0; 20];
        self.dma_read(table, &mut cfis)?;
        if cfis[0] != 0x27 || cfis[1] & 0xf != 0 {
            return Err(());
        }
        if cfis[1] & 0x80 == 0 {
            if cfis[15] & 4 == 0 {
                self.initial_fis(port);
            } else {
                self.put32(base + 0x20, 0x80);
            }
            return Ok(());
        }
        let count = u16::from_le_bytes([header[2], header[3]]) as usize;
        let mut prds = Vec::with_capacity(count);
        let mut capacity = 0usize;
        for index in 0..count {
            let mut bytes = [0; 16];
            self.dma_read(
                table.checked_add(0x80 + (index * 16) as u64).ok_or(())?,
                &mut bytes,
            )?;
            let address = le64(&bytes);
            let raw = le32(&bytes[12..]);
            let len = (raw as usize & 0x3f_ffff) + 1;
            if address & 1 != 0
                || !len.is_multiple_of(2)
                || !self
                    .memory
                    .check_range(GuestAddress(address), len, Permissions::ReadWrite)
            {
                return Err(());
            }
            capacity = capacity.checked_add(len).ok_or(())?;
            prds.push((address, len, raw & (1 << 31) != 0));
        }
        let write = flags & 0x40 != 0;
        let queued = matches!(cfis[2], 0x60 | 0x61);
        if queued && usize::from(cfis[12] >> 3) != slot {
            return Err(());
        }
        let mut input = Vec::new();
        // ATA write lengths are limited by the sector count, never by guest PRDT capacity.
        if write && matches!(cfis[2], 0x35 | 0xca | 0x30 | 0x34 | 0x61) {
            let (_, length) = Self::ata_range(&cfis);
            if length > capacity {
                return Err(());
            }
            input.resize(length, 0);
            let mut copied = 0;
            for &(address, len, _) in &prds {
                let n = len.min(length - copied);
                self.dma_read(address, &mut input[copied..copied + n])?;
                copied += n;
                if copied == length {
                    break;
                }
            }
        }
        let mut packet = [0; 16];
        if cfis[2] == 0xa0 {
            self.dma_read(table.checked_add(0x40).ok_or(())?, &mut packet)?;
        }
        let input_len = input.len();
        let request = Request {
            slot,
            header_addr,
            cfis,
            prds,
        };
        let done = match self.execute(port, &cfis, flags, &packet, input) {
            Err(error) => Done::error(error),
            Ok(Exec::Bytes(bytes)) => {
                if !write && bytes.len() > capacity {
                    return Err(());
                }
                let expected = if write { input_len } else { bytes.len() };
                let memory = &self.memory;
                let (transferred, ioc) = walk(&request.prds, expected, |address, copied, n| {
                    if !write {
                        memory
                            .write_slice(&bytes[copied..copied + n], GuestAddress(address))
                            .map_err(|_| ())?;
                    }
                    Ok(true)
                })?;
                Done {
                    status: 0x50,
                    error: 0,
                    transferred,
                    ioc,
                }
            }
            Ok(Exec::Io(io)) => {
                if matches!(io, Io::Read { len, .. } if len > capacity) {
                    return Err(());
                }
                match self.images[port].clone() {
                    Some(image) => {
                        return Ok(Some(Box::new(Job {
                            port,
                            generation: self.generation[port],
                            request,
                            io,
                            image,
                        })));
                    }
                    None => Done::error(4),
                }
            }
        };
        self.finish(port, &request, done)?;
        Ok(None)
    }

    /// Report a finished command to the guest: the byte count, the FIS, the
    /// task file and the port's interrupt bits.
    fn finish(&mut self, port: usize, request: &Request, done: Done) -> Result<(), ()> {
        let base = port_base(port);
        let Request {
            slot,
            header_addr,
            cfis,
            ..
        } = *request;
        let Done {
            status,
            error,
            transferred,
            ioc,
        } = done;
        let queued = matches!(cfis[2], 0x60 | 0x61);
        self.dma_write(
            header_addr.checked_add(4).ok_or(())?,
            &(transferred as u32).to_le_bytes(),
        )?;
        if queued {
            if error != 0
                && let Some(Drive::Disk(disk)) = self.drives[port].as_mut()
            {
                disk.ncq_error[..20].copy_from_slice(&cfis);
                disk.ncq_error[20] = status;
                disk.ncq_error[21] = error;
            }
            self.put32(base + 0x20, u32::from(status) | (u32::from(error) << 8));
            let active = 1u32 << slot;
            self.put32(base + 0x34, self.get32(base + 0x34) & !active);
            if self.get32(base + 0x18) & 0x10 != 0 {
                let mut sdb = [0xa1, 0x40, status, error, 0, 0, 0, 0];
                sdb[4..].copy_from_slice(&active.to_le_bytes());
                self.dma_write(self.address(base + 8).checked_add(0x58).ok_or(())?, &sdb)?;
            }
            self.port_interrupt(
                port,
                8 | if error != 0 { TFES } else { 0 } | if ioc { DSS } else { 0 },
            );
            return Ok(());
        }
        let pio = matches!(cfis[2], 0xec | 0xa1 | 0x20 | 0x24 | 0x2f)
            || (cfis[2] == 0xa0 && cfis[3] & 1 == 0);
        let mut source = cfis;
        if cfis[2] == 0xa0 {
            source[12] = 3; // ATAPI command complete: C/D and I/O.
            source[5] = transferred as u8;
            source[6] = (transferred >> 8) as u8;
        }
        self.fis(
            port,
            status,
            error,
            &source,
            (pio && error == 0).then_some(transferred),
        )?;
        self.port_interrupt(
            port,
            DHRS | if error != 0 { TFES } else { 0 }
                | if pio && error == 0 { PSS } else { 0 }
                | if ioc { DSS } else { 0 },
        );
        Ok(())
    }

    fn ata_range(fis: &[u8; 20]) -> (u64, usize) {
        let queued = matches!(fis[2], 0x60 | 0x61);
        let ext = queued || matches!(fis[2], 0x25 | 0x35 | 0x24 | 0x34);
        let mut lba = fis[4] as u64 | ((fis[5] as u64) << 8) | ((fis[6] as u64) << 16);
        let count = if ext {
            lba |= (fis[8] as u64) << 24 | (fis[9] as u64) << 32 | (fis[10] as u64) << 40;
            let raw = u16::from_le_bytes(if queued {
                [fis[3], fis[11]]
            } else {
                [fis[12], fis[13]]
            }) as usize;
            if raw == 0 { 65536 } else { raw }
        } else {
            lba |= ((fis[7] & 0xf) as u64) << 24;
            if fis[12] == 0 { 256 } else { fis[12] as usize }
        };
        (lba * 512, count * 512)
    }

    fn execute(
        &mut self,
        port: usize,
        fis: &[u8; 20],
        flags: u16,
        packet: &[u8; 16],
        input: Vec<u8>,
    ) -> Result<Exec, u8> {
        let drive = self.drives[port].as_mut().ok_or(4u8)?;
        let write = flags & 0x40 != 0;
        match fis[2] {
            0xec if matches!(drive, Drive::Disk(_)) && !write => {
                Ok(Exec::Bytes(drive.identify(port, Transport::Sata)))
            }
            0xa1 if matches!(drive, Drive::Cdrom(_)) && !write => {
                Ok(Exec::Bytes(drive.identify(port, Transport::Sata)))
            }
            0xa0 if flags & 0x20 != 0 && (!write || matches!(packet[0], 0x2a | 0xaa)) => {
                match drive {
                    Drive::Cdrom(optical) => Ok(match optical.packet(packet)? {
                        Payload::Bytes(bytes) => Exec::Bytes(bytes),
                        Payload::Read { offset, len } => Exec::Io(Io::Read { offset, len }),
                    }),
                    _ => Err(4),
                }
            }
            0x25 | 0xc8 | 0x20 | 0x24 | 0x35 | 0xca | 0x30 | 0x34 | 0x60 | 0x61 => {
                let expected_write = matches!(fis[2], 0x35 | 0xca | 0x30 | 0x34 | 0x61);
                if write != expected_write {
                    return Err(4);
                }
                let Drive::Disk(disk) = drive else {
                    return Err(4);
                };
                let (offset, len) = Self::ata_range(fis);
                if offset
                    .checked_add(len as u64)
                    .is_none_or(|end| end > disk.image.size())
                {
                    return Err(0x10);
                }
                if !write {
                    return Ok(Exec::Io(Io::Read { offset, len }));
                }
                if disk.image.read_only() || input.len() != len {
                    return Err(4);
                }
                Ok(Exec::Io(Io::Write {
                    offset,
                    data: input,
                    flush: !disk.write_cache || (fis[2] == 0x61 && fis[7] & 0x80 != 0),
                }))
            }
            0xe7 | 0xea => match drive {
                Drive::Disk(_) => Ok(Exec::Io(Io::Flush)),
                Drive::Cdrom(_) => Err(4),
            },
            0x2f if !write && fis[4] == 0x10 && fis[5..7] == [0, 0] && fis[12..14] == [1, 0] => {
                let Drive::Disk(disk) = drive else {
                    return Err(4);
                };
                let mut log = vec![0; 512];
                if disk.ncq_error[20] == 0 {
                    log[0] = 0x80;
                } else {
                    log[0] = disk.ncq_error[12] >> 3;
                    log[2..4].copy_from_slice(&disk.ncq_error[20..22]);
                    log[4..11].copy_from_slice(&disk.ncq_error[4..11]);
                    log[12] = disk.ncq_error[3];
                    log[13] = disk.ncq_error[11];
                }
                log[511] =
                    0u8.wrapping_sub(log.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
                Ok(Exec::Bytes(log))
            }
            0xef => {
                drive.set_features(fis[3], fis[12])?;
                Ok(Exec::Bytes(Vec::new()))
            }
            _ => Err(4),
        }
    }

    fn write_reg(&mut self, offset: usize, value: u32, mask: u32) {
        let old = self.get32(offset);
        let merged = (old & !mask) | (value & mask);
        match offset {
            4 => {
                if value & mask & 1 != 0 {
                    self.reset();
                } else {
                    self.put32(4, (merged & 2) | (1 << 31));
                }
            }
            8 => {
                // An acknowledgement ends what the last MSI signalled.
                // Whatever IS holds after it sends a new MSI below: a port
                // whose PxIS is still pending sets its bit again, and the
                // bits of other ports the guest did not acknowledge stay
                // set. storahci serves one port per interrupt and
                // acknowledges only that port's bit, and two MSIs sent
                // before the handler ran reach the guest as one. Without
                // the new MSI the other port waited for the next unrelated
                // completion.
                self.put32(8, old & !(value & mask));
                self.msi_pending = 0;
            }
            0x100..=0x3ff => {
                let port = (offset - 0x100) / 0x80;
                let base = port_base(port);
                match offset - base {
                    0 | 4 if self.get32(base + 0x18) & 1 == 0 => self.put32(
                        offset,
                        if offset == base {
                            merged & !0x3ff
                        } else {
                            merged
                        },
                    ),
                    8 | 12 if self.get32(base + 0x18) & 0x10 == 0 => self.put32(
                        offset,
                        if offset == base + 8 {
                            merged & !0xff
                        } else {
                            merged
                        },
                    ),
                    0x10 | 0x30 => self.put32(offset, old & !(value & mask)),
                    0x14 => self.put32(offset, merged & 0xfdc0_00ff),
                    0x18 => {
                        let mut cmd = (merged & 0x0100_0011) | 6;
                        if cmd & 1 != 0 {
                            cmd |= if old & 1 == 0 { 1 << 15 } else { old & 0x9f00 };
                        }
                        if cmd & 0x10 != 0 {
                            cmd |= 1 << 14;
                        }
                        self.put32(offset, cmd);
                        if value & mask & 8 != 0 {
                            self.put32(base + 0x20, self.get32(base + 0x20) & !0x88);
                        }
                        if cmd & 1 == 0 {
                            self.cancel(port);
                            self.put32(base + 0x38, 0);
                            self.put32(base + 0x34, 0);
                        }
                        if old & 0x10 == 0 && cmd & 0x10 != 0 && self.config[4] & 4 != 0 {
                            self.initial_fis(port);
                        }
                    }
                    0x2c => {
                        self.put32(offset, merged & 0xfff);
                        if merged & 0xf == 1 {
                            self.cancel(port);
                            self.put32(base + 0x28, u32::from(self.drives[port].is_some()));
                            self.put32(base + 0x38, 0);
                            self.put32(base + 0x34, 0);
                        } else if old & 0xf == 1 && merged & 0xf == 0 {
                            self.put32(
                                base + 0x28,
                                if self.drives[port].is_some() {
                                    0x113
                                } else {
                                    0
                                },
                            );
                            self.put32(
                                base + 0x24,
                                self.drives[port]
                                    .as_ref()
                                    .map_or(0xffff_ffff, Drive::signature),
                            );
                            self.initial_fis(port);
                            self.put32(base + 0x30, self.get32(base + 0x30) | (1 << 16));
                            self.port_interrupt(port, (1 << 22) | (1 << 6));
                        }
                    }
                    0x34 | 0x38 => self.put32(offset, old | (value & mask)),
                    _ => {}
                }
                self.run(port);
            }
            _ => {}
        }
        self.update_irq();
    }
}

impl Core {
    fn read(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|a| self.regs.get(a as usize))
                .copied()
                .unwrap_or(0);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut index = 0;
        while index < data.len() {
            let Some(address) = offset
                .checked_add(index as u64)
                .filter(|a| *a < REGS as u64)
            else {
                break;
            };
            let shift = (address as usize & 3) * 8;
            let n = (4 - (address as usize & 3)).min(data.len() - index);
            let mut value = 0;
            let mut mask = 0;
            for j in 0..n {
                value |= (data[index + j] as u32) << (shift + j * 8);
                mask |= 0xff << (shift + j * 8);
            }
            self.write_reg(address as usize & !3, value, mask);
            index += n;
        }
    }
}

impl Core {
    fn config_read(&self, reg: usize) -> u32 {
        if reg >= 64 {
            return 0;
        }
        let raw = le32(&self.config[reg * 4..]);
        match reg {
            8 => (raw & !0x1f) | 1,
            9 => raw & !0xfff,
            _ => raw,
        }
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let old_master = self.config[4] & 4;
        let Some(start) = (reg as u64)
            .checked_mul(4)
            .and_then(|a| a.checked_add(offset))
        else {
            return;
        };
        for (i, &byte) in data.iter().enumerate() {
            let Some(address) = start.checked_add(i as u64) else {
                break;
            };
            match address {
                4 => self.config[4] = byte & 7,
                5 => self.config[5] = byte & 5,
                0x20..=0x27 | 0x3c | 0x84..=0x8d => self.config[address as usize] = byte,
                0x82 => self.config[0x82] = 0x80 | (byte & 1),
                0x92 => self.config[0x92] = byte & 0x3f,
                _ => {}
            }
        }
        for port in 0..PORTS {
            if old_master == 0
                && self.config[4] & 4 != 0
                && self.get32(port_base(port) + 0x18) & 0x10 != 0
            {
                self.initial_fis(port);
            }
            self.run(port);
        }
        self.update_irq();
    }
    fn bar_read(&self, bar: u8, offset: u64, data: &mut [u8]) {
        match (bar, offset) {
            (5, _) => self.read(offset, data),
            (4, 0x10) if data.len() == 4 => data.copy_from_slice(&self.index.to_le_bytes()),
            (4, 0x14) => self.read(self.index as u64, data),
            _ => data.fill(0),
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        match (bar, offset) {
            (5, _) => self.write(offset, data),
            (4, 0x10) if data.len() == 4 => self.index = le32(data) & 0xffc,
            (4, 0x14) => self.write(self.index as u64, data),
            _ => {}
        }
    }
}

impl Core {
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"AHC3".to_vec();
        state.extend(self.config);
        state.extend(self.regs);
        state.extend(self.index.to_le_bytes());
        for drive in &self.drives {
            let mut bytes = [0; 48];
            match drive {
                None => {}
                Some(Drive::Disk(disk)) => {
                    bytes[0] = 1;
                    bytes[1] = u8::from(disk.image.read_only());
                    bytes[2] = u8::from(disk.write_cache);
                    bytes[3] = disk.transfer_mode;
                    bytes[8..16].copy_from_slice(&disk.image.size().to_le_bytes());
                    bytes[24..48].copy_from_slice(&disk.ncq_error);
                }
                Some(Drive::Cdrom(optical)) => {
                    bytes[0] = 2;
                    bytes[1] = u8::from(optical.tray_open);
                    bytes[2] = u8::from(optical.locked);
                    bytes[3] = u8::from(optical.attention);
                    bytes[4] = optical.event;
                    bytes[5..8].copy_from_slice(&optical.sense);
                    bytes[8..16].copy_from_slice(
                        &optical.image.as_ref().map_or(0, |i| i.size()).to_le_bytes(),
                    );
                    bytes[16..24].copy_from_slice(&optical.generation.to_le_bytes());
                    bytes[24] = optical.transfer_mode;
                }
            }
            state.extend(bytes);
        }
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let invalid =
            || DeviceError::State("AHCI snapshot version, topology or media mismatch".into());
        if state.len() != 4 + 256 + REGS + 4 + PORTS * 48 || &state[..4] != b"AHC3" {
            return Err(invalid());
        }
        let config: [u8; 256] = state[4..260].try_into().unwrap();
        let regs: [u8; REGS] = state[260..260 + REGS].try_into().unwrap();
        let index = le32(&state[260 + REGS..]);
        if config[..4] != self.config[..4]
            || le32(&regs) != CAP
            || le32(&regs[0xc..]) != 0x3f
            || index >= REGS as u32
            || index & 3 != 0
        {
            return Err(invalid());
        }
        let ports = &state[264 + REGS..];
        for (port, drive) in self.drives.iter().enumerate() {
            let bytes = &ports[port * 48..port * 48 + 48];
            let valid = match drive {
                None => bytes[0] == 0,
                Some(Drive::Disk(disk)) => {
                    bytes[0] == 1
                        && bytes[1] == u8::from(disk.image.read_only())
                        && bytes[2] <= 1
                        && matches!(bytes[3], 0 | 1 | 0x08..=0x0c | 0x20..=0x22 | 0x40..=0x45)
                        && le64(&bytes[8..]) == disk.image.size()
                }
                Some(Drive::Cdrom(optical)) => {
                    bytes[0] == 2
                        && bytes[1..4].iter().all(|b| *b <= 1)
                        && le64(&bytes[8..]) == optical.image.as_ref().map_or(0, |i| i.size())
                        && le64(&bytes[16..]) == optical.generation
                        && matches!(bytes[24], 0 | 1 | 0x08..=0x0c | 0x20..=0x22 | 0x40..=0x45)
                }
            };
            if !valid {
                return Err(invalid());
            }
        }
        self.config = config;
        self.regs = regs;
        self.index = index;
        for (port, drive) in self.drives.iter_mut().enumerate() {
            if let Some(Drive::Disk(disk)) = drive {
                let bytes = &ports[port * 48..port * 48 + 48];
                disk.write_cache = bytes[2] != 0;
                disk.transfer_mode = bytes[3];
                disk.ncq_error.copy_from_slice(&bytes[24..48]);
            }
            if let Some(Drive::Cdrom(optical)) = drive {
                let bytes = &ports[port * 48..port * 48 + 48];
                optical.tray_open = bytes[1] != 0;
                optical.locked = bytes[2] != 0;
                optical.attention = bytes[3] != 0;
                optical.event = bytes[4];
                optical.sense.copy_from_slice(&bytes[5..8]);
                optical.transfer_mode = bytes[24];
            }
        }
        self.msi_pending = 0;
        self.update_irq();
        Ok(())
    }
}

impl BusDevice for Ahci {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.core().read(offset, data);
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut core = self.core();
        core.write(offset, data);
        self.drained(core);
    }
}

impl PciDevice for Ahci {
    fn config_read(&mut self, reg: usize) -> u32 {
        self.core().config_read(reg)
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let mut core = self.core();
        core.config_write(reg, offset, data);
        self.drained(core);
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 4,
                size: 32,
                kind: BarKind::Io,
            },
            BarInfo {
                index: 5,
                size: 4096,
                kind: BarKind::Mmio32,
            },
        ]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        self.core().bar_read(bar, offset, data);
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        let mut core = self.core();
        core.bar_write(bar, offset, data);
        self.drained(core);
    }
}

impl Snapshot for Ahci {
    fn quiesce(&mut self) {
        self.wait_idle();
    }
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.idle().save_state()
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.idle().restore_state(state)
    }
}

impl crate::Resettable for Ahci {
    fn reset(&mut self) -> Result<(), DeviceError> {
        let mut core = self.idle();
        core.config = power_on_config();
        for drive in core.drives.iter_mut().flatten() {
            match drive {
                Drive::Disk(disk) => {
                    disk.write_cache = true;
                    disk.transfer_mode = 0x22;
                }
                Drive::Cdrom(optical) => optical.transfer_mode = Optical::new(None).transfer_mode,
            }
        }
        core.reset();
        Ok(())
    }
}

impl PciSnapshot for Ahci {
    fn quiesce(&mut self) {
        self.wait_idle();
    }
    fn snapshot_id(&self) -> String {
        self.state_id()
    }
    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.save_state()
    }
    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok(Vec::new())
    }
    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.restore_state(state)
    }
    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.is_empty() {
            Ok(())
        } else {
            Err(DeviceError::State("unexpected AHCI wrapped state".into()))
        }
    }
}
