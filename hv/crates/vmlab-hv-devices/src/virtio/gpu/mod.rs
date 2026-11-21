//! Virtio-gpu, VIRTIO 1.2 section 5.7, with optional virglrenderer acceleration.
//!
//! `Gpu` clones share state. Keep a clone for frame and cursor publication
//! while the PCI transport owns another clone. The default 2D mode needs no GPU.

mod compose;
mod copy;
mod lost;
mod modifiers;
mod pacer;
mod renderer;
mod scanout;
pub use scanout::DmaFrame;
mod snapshot;
mod teardown;
pub use renderer::{APERTURE_SIZE, BlobMapper, GpuMode};

pub enum ScanoutPixels {
    Dma(DmaFrame),
    Shm(ShmFrame),
    Inactive,
    Vga,
}

#[derive(Clone)]
pub struct ShmFrame {
    pub mode: Mode,
    pub pixels: Arc<Vec<u8>>,
    pub damage: Vec<crate::display::Damage>,
}

pub struct ScanoutUpdate {
    pub generation: u64,
    pub timestamp_ns: u64,
    pub damage: Vec<crate::display::Damage>,
    pub pixels: ScanoutPixels,
}

pub trait DisplaySink: Send + Sync {
    fn needs_cpu(&self) -> bool;
    fn update(&self, frame: ScanoutUpdate);
}
#[cfg(test)]
mod tests;

use crate::display::{Frame, Mode, PixelFormat};
use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::Arc,
};
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemory as _, Permissions};

const FEATURES: u64 = (1 << 32) | (1 << 29) | (1 << 28) | (1 << ORDERED_FLIP_FEATURE) | (1 << 1);
/// vmlab's private feature bit: the host-ordered flip. The bits up to 23
/// belong to the device type and VIRTIO 1.2 defines 0 to 5, so the top one
/// is the furthest from the next standard bit. Only vmlab's viogpu3d driver
/// acks it (`guest/win3d/docs/flip.md`). A driver that does not know it
/// keeps `RESOURCE_FLUSH` and its own CPU copy of a Blt present. The bit
/// enables both of vmlab's commands, [`CMD_ORDERED_FLIP`] and
/// [`CMD_PRESENT_COPY`].
pub const ORDERED_FLIP_FEATURE: u32 = 23;
/// The first command of [`ORDERED_FLIP_FEATURE`]. It takes the place of
/// `RESOURCE_FLUSH` for a flip: header, scanout ID, resource ID, the
/// rectangle (x, y, width, height) and the guest's frame number, all
/// little-endian, 56 bytes. It is answered `OK_NODATA` once every fence on
/// the renderer's timeline has signalled, the writers the kernel tracks on
/// the scanout buffer are done, the frame is published to the display, the
/// console has read it, and no display reader holds an older frame. Outside the 2D, 3D and cursor ranges of VIRTIO 1.2.
pub const CMD_ORDERED_FLIP: u32 = 0x0f00;
/// The second command of [`ORDERED_FLIP_FEATURE`]: the copy of a present of
/// the Blt kind, run on the host in place of the guest driver's CPU copy
/// (`copy.rs` has the layout). It is answered `OK_NODATA` once every fence
/// on the renderer's timeline has signalled, the writers and readers the
/// kernel tracks on either resource are done, and the pixels are in the
/// destination. The queue serves no later command before that, so the
/// fence of the present, which follows it, signals after the copy.
pub const CMD_PRESENT_COPY: u32 = 0x0f01;
/// vmlab's private event of [`ORDERED_FLIP_FEATURE`], in `events_read`: the
/// host GPU is lost (`lost.rs`). The top bit, the furthest from the
/// standard `VIRTIO_GPU_EVENT_DISPLAY` (bit 0).
pub const EVENT_DEVICE_LOST: u32 = 1 << 31;
const UNSPEC: u32 = 0x1200;
const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_RESOURCES: usize = 4096;
const MAX_ENTRIES: usize = 16384;
const INVALID: u32 = 0x1205;
const OUT_OF_MEMORY: u32 = 0x1201;
const RESOURCE: u32 = 0x1203;
const SCANOUT: u32 = 0x1202;
const NODATA: u32 = 0x1100;
const EXPORT_BUSY: u32 = 0x12ff;

/// Cursor pixels are little-endian premultiplied ARGB8888 (B, G, R, A).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub pixels: Vec<u8>,
    pub x: u32,
    pub y: u32,
    pub visible: bool,
    /// Changes on shape, position, visibility, reset and restore.
    pub generation: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn fits(self, width: u32, height: u32) -> bool {
        self.w > 0
            && self.h > 0
            && self.x.checked_add(self.w).is_some_and(|v| v <= width)
            && self.y.checked_add(self.h).is_some_and(|v| v <= height)
    }
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: u32,
    pixels: Vec<u8>,
    backing: Vec<(u64, u32)>,
}

impl Resource {
    fn pixel(&self, offset: usize) -> [u8; 4] {
        let p = &self.pixels[offset..offset + 4];
        match self.format {
            1 => [p[0], p[1], p[2], p[3]],
            2 => [p[0], p[1], p[2], 255],
            3 => [p[3], p[2], p[1], p[0]],
            4 => [p[3], p[2], p[1], 255],
            67 => [p[2], p[1], p[0], p[3]],
            68 => [p[1], p[2], p[3], 255],
            121 => [p[1], p[2], p[3], p[0]],
            134 => [p[2], p[1], p[0], 255],
            _ => unreachable!("validated resource format"),
        }
    }
}

struct State {
    renderer: Option<renderer::Renderer>,
    fences: Vec<PendingFence>,
    current_fence: Option<(u32, u64, u32, u8)>,
    acked: u64,
    preferred: (u32, u32),
    events: u32,
    resources: BTreeMap<u32, Resource>,
    scanout: Option<(u32, Rect)>,
    native: bool,
    scanout_generation: u64,
    dirty: bool,
    compose: compose::Compose,
    flushes: Vec<Rect>,
    cursor: Cursor,
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Queue>,
    pending: Vec<Queue>,
    display: Option<Arc<dyn DisplaySink>>,
    retry_display: bool,
    display_timestamp: u64,
    /// Publishes 2D flushes off the thread that serves the queue. `None`
    /// until a display sink is set: a flush then publishes where it stands.
    pacer: Option<Arc<pacer::Pacer>>,
    /// The command timestamp of the newest 2D flush that no published
    /// frame holds yet.
    flush_pending: Option<u64>,
    last_publish: Option<std::time::Instant>,
    /// Ordered flips served since the last reset. The first is logged, so
    /// that a log shows which flip a guest uses.
    ordered_flips: u64,
    /// Why the host GPU is lost, from the loss on (`lost.rs`). It
    /// outlives a device reset: a host GPU does not come back.
    lost: Option<String>,
    /// Set with `lost`, and read without the device lock.
    lost_flag: lost::Flag,
    /// The render workers of the guest's Venus contexts (`teardown.rs`).
    workers: teardown::Workers,
    /// The last reset has not confirmed that the renderer and its workers
    /// stopped: the transport keeps the status nonzero.
    unsettled: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(pacer) = &self.pacer {
            pacer.close();
        }
    }
}

/// One output and two queues. `enable_3d` adds virgl, Venus and blob resources.
#[derive(Clone)]
pub struct Gpu {
    id: String,
    state: Arc<Mutex<State>>,
}

impl Gpu {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: Arc::new(Mutex::new(State {
                renderer: None,
                fences: Vec::new(),
                current_fence: None,
                acked: 0,
                preferred: (1280, 800),
                events: 0,
                resources: BTreeMap::new(),
                scanout: None,
                native: false,
                scanout_generation: 0,
                dirty: true,
                compose: compose::Compose::default(),
                flushes: Vec::new(),
                cursor: Cursor::default(),
                mem: None,
                interrupt: None,
                queues: Vec::new(),
                pending: Vec::new(),
                display: None,
                retry_display: false,
                display_timestamp: 0,
                pacer: None,
                flush_pending: None,
                last_publish: None,
                ordered_flips: 0,
                lost: None,
                lost_flag: lost::Flag::default(),
                workers: teardown::Workers::default(),
                unsettled: false,
            })),
        }
    }

    pub fn native(&self) -> bool {
        self.state.lock().native
    }

    pub fn enable_3d(
        &self,
        mode: GpuMode,
        node: std::path::PathBuf,
        mapper: Arc<dyn BlobMapper>,
    ) -> Result<(), DeviceError> {
        let renderer = renderer::Renderer::new(mode, node, mapper)?;
        let flag = renderer.lost();
        let mut watch = renderer.watch();
        {
            let mut s = self.state.lock();
            s.lost_flag = flag.clone();
            s.renderer = Some(renderer);
        }
        let state = Arc::downgrade(&self.state);
        std::thread::Builder::new()
            .name("gpu-loss-watch".into())
            .spawn(move || {
                while state.strong_count() != 0 && !flag.get() {
                    if let Some(reason) = watch.check() {
                        // Set the flag before the device lock is taken: a
                        // command that waits on the lost host GPU can hold
                        // the lock, and the flag ends its wait.
                        flag.set();
                        if let Some(state) = state.upgrade() {
                            lose(&state, reason);
                        }
                        return;
                    }
                    std::thread::sleep(lost::INTERVAL);
                }
            })?;
        let state = Arc::downgrade(&self.state);
        std::thread::Builder::new()
            .name("gpu-fence-completion".into())
            .spawn(move || {
                while let Some(state) = state.upgrade() {
                    state.lock().complete_fences();
                    drop(state);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })?;
        Ok(())
    }

    /// Treat the host GPU as lost (`lost.rs`). The loss watch calls this
    /// when it sees the render server end or the render node go; a test
    /// calls it to simulate a loss. A second call changes nothing.
    pub fn lose_host_gpu(&self, reason: impl Into<String>) {
        lose(&self.state, reason.into());
    }

    /// Why the host GPU is lost, once it is.
    pub fn host_gpu_lost(&self) -> Option<String> {
        self.state.lock().lost.clone()
    }

    pub fn has_3d_contexts(&self) -> bool {
        self.state
            .lock()
            .renderer
            .as_ref()
            .is_some_and(|r| r.has_contexts())
    }

    pub fn dma_frame(&self) -> Result<Option<DmaFrame>, DeviceError> {
        self.state.lock().renderer.as_ref().map_or(Ok(None), |r| {
            r.dma_frame(true)
                .map_err(|e| DeviceError::Other(format!("GPU scanout capture failed: {e:#x}")))
        })
    }

    /// Publish scanout changes to `sink`. From here on a 2D flush only
    /// records its damage, and a frame thread composes and publishes at
    /// most one frame per [`pacer::FRAME_INTERVAL`].
    pub fn set_display_sink(&self, sink: Arc<dyn DisplaySink>) {
        let mut s = self.state.lock();
        s.display = Some(sink);
        if let Some(timestamp_ns) = s.flush_pending {
            s.publish_scanout(timestamp_ns);
        }
        if s.pacer.is_none() {
            match pacer::Pacer::spawn(&self.id, Arc::downgrade(&self.state)) {
                Ok(pacer) => s.pacer = Some(pacer),
                // Without the thread every flush publishes its own frame.
                Err(error) => log::warn!("{}: no frame thread: {error}", self.id),
            }
        }
    }

    pub fn poll_scanout(&self) -> bool {
        !self.state.lock().native
    }

    pub fn set_aperture_base(&self, base: Option<u64>) -> Result<(), DeviceError> {
        if let Some(renderer) = &self.state.lock().renderer {
            renderer.set_base(base)?;
        }
        Ok(())
    }

    pub fn cursor(&self) -> Cursor {
        self.state.lock().cursor.clone()
    }

    /// Set the preferred EDID/display-info size and signal DISPLAY configuration change.
    pub fn request_size(&self, width: u32, height: u32) -> Result<(), DeviceError> {
        if !valid_size(width, height) {
            return Err(DeviceError::Other("invalid GPU preferred size".into()));
        }
        let mut s = self.state.lock();
        if s.preferred != (width, height) {
            s.preferred = (width, height);
            s.events |= 1;
            if let Some(irq) = &s.interrupt {
                irq.notify_config();
            }
        }
        Ok(())
    }

    /// `None` means VGA compatibility is active. Disabled native scanouts are black.
    pub fn mode(&self) -> Option<Mode> {
        self.state.lock().mode()
    }

    pub fn scanout_active(&self) -> Option<bool> {
        let state = self.state.lock();
        state.native.then(|| {
            state.scanout.is_some()
                || state
                    .renderer
                    .as_ref()
                    .is_some_and(|renderer| renderer.mode().is_some())
        })
    }

    pub fn frame(&self) -> Option<Frame> {
        let mut s = self.state.lock();
        if let Some(renderer) = s.renderer.as_ref().filter(|_| s.lost.is_none()) {
            match renderer.frame() {
                Ok(Some(frame)) => return Some(frame),
                Ok(None) => {}
                Err(error) => {
                    log::warn!("GPU readback failed: {error:#x}");
                    return None;
                }
            }
        }
        s.frame_2d().map(|frame| Frame {
            mode: frame.mode,
            pixels: frame.pixels.as_ref().clone(),
            dirty: !frame.damage.is_empty(),
            damage: frame.damage,
        })
    }

    /// Runtime surface generation, including same-size replacement and reset.
    pub fn scanout_generation(&self) -> u64 {
        self.state.lock().scanout_generation
    }
}

fn lose(state: &Mutex<State>, reason: String) {
    state.lock().lose(reason);
}

impl State {
    /// The loss of the host GPU, as `lost.rs` describes it.
    fn lose(&mut self, reason: String) {
        if self.lost.is_some() {
            return;
        }
        log::error!(
            "virtio-gpu: the host GPU is lost: {reason}. The device answers its {} pending fences \
             and refuses further GPU work; the guest is {}",
            self.fences.len(),
            if self.acked & (1 << ORDERED_FLIP_FEATURE) != 0 {
                "told through the device-lost event"
            } else {
                "not told: its driver has not acked the vmlab feature bit"
            }
        );
        self.lost_flag.set();
        self.lost = Some(reason);
        self.release_fences();
        self.raise_lost();
    }

    /// Answer every fence the device holds. Their responses are written.
    fn release_fences(&mut self) {
        let fences = std::mem::take(&mut self.fences);
        let Some(mem) = &self.mem else { return };
        let mut notify = std::collections::BTreeSet::new();
        for fence in fences {
            let queue = &mut self.queues[fence.queue as usize];
            if queue.add_used(mem, fence.head, fence.len).is_ok()
                && queue.needs_notification(mem).unwrap_or(false)
            {
                notify.insert(fence.queue);
            }
        }
        if let Some(irq) = &self.interrupt {
            for queue in notify {
                irq.notify_queue(queue);
            }
        }
    }

    /// Put [`EVENT_DEVICE_LOST`] in `events_read`, for a driver that acked
    /// the vmlab bit, and raise a configuration interrupt.
    fn raise_lost(&mut self) {
        if self.lost.is_none() || self.acked & (1 << ORDERED_FLIP_FEATURE) == 0 {
            return;
        }
        self.events |= EVENT_DEVICE_LOST;
        if let Some(irq) = &self.interrupt {
            irq.notify_config();
        }
    }

    fn frame_2d(&mut self) -> Option<ShmFrame> {
        let mode = self.mode()?;
        let scanout = self.scanout.map(|(id, rect)| (&self.resources[&id], rect));
        let frame = self.compose.frame(mode, scanout, &self.flushes, self.dirty);
        self.flushes.clear();
        self.dirty = false;
        Some(frame)
    }

    /// Hand the scanout to the display sink. False when nothing was
    /// published: no sink, no new frame, or a failed export.
    fn publish_scanout(&mut self, timestamp_ns: u64) -> bool {
        let Some(sink) = self.display.clone() else {
            return false;
        };
        // This frame holds every flush that came before it.
        self.flush_pending = None;
        self.last_publish = Some(std::time::Instant::now());
        let mut damage = Vec::new();
        let pixels = if let Some(renderer) = &self.renderer {
            let Ok(Some((generation, rectangles))) = renderer.take_damage() else {
                return false;
            };
            self.scanout_generation = generation;
            damage = rectangles
                .into_iter()
                .map(|r| crate::display::Damage {
                    x: r.x,
                    y: r.y,
                    width: r.w,
                    height: r.h,
                })
                .collect();
            vmlab_hv_display::trace::write(
                "publish",
                format_args!("ts={timestamp_ns} cpu={}", sink.needs_cpu() as u8),
            );
            match renderer.dma_frame(sink.needs_cpu()) {
                Ok(Some(frame)) => ScanoutPixels::Dma(frame),
                Ok(None) => ScanoutPixels::Inactive,
                Err(error) => {
                    self.retry_display = error == EXPORT_BUSY;
                    self.display_timestamp = timestamp_ns;
                    if error != EXPORT_BUSY {
                        log::warn!("GPU flush export failed: {error:#x}");
                    }
                    return false;
                }
            }
        } else if !self.native {
            ScanoutPixels::Vga
        } else if self.scanout.is_none() {
            ScanoutPixels::Inactive
        } else {
            let Some(frame) = self.frame_2d() else {
                return false;
            };
            ScanoutPixels::Shm(frame)
        };
        self.retry_display = false;
        sink.update(ScanoutUpdate {
            generation: self.scanout_generation,
            timestamp_ns,
            damage,
            pixels,
        });
        true
    }

    /// [`CMD_ORDERED_FLIP`]: the flush of a flip, published before the
    /// answer. The queue holds every later command until then, so the
    /// guest's vsync, which a later command raises, follows the publish.
    fn ordered_flip(
        &mut self,
        mem: &GuestMemory,
        request: &[u8],
        d: &mut Decoder<'_>,
    ) -> Result<(u32, Vec<u8>), u32> {
        if self.acked & (1 << ORDERED_FLIP_FEATURE) == 0 {
            return Err(INVALID);
        }
        if d.u32()? != 0 {
            return Err(SCANOUT);
        }
        let id = d.u32()?;
        let rect = d.rect()?;
        let frame = d.u64()?;
        // The RESOURCE_FLUSH that the flip takes the place of, without a fence.
        let mut flush = request[..24].to_vec();
        flush[..4].copy_from_slice(&0x104u32.to_le_bytes());
        flush[4..8].fill(0);
        for word in [rect.x, rect.y, rect.w, rect.h, id, 0] {
            flush.extend_from_slice(&word.to_le_bytes());
        }
        if let Some(renderer) = &self.renderer {
            // Host GPU work that a fence of the guest does not cover can
            // still write the new frame. Wait for what the host can see.
            if let Err(error) = renderer.order() {
                log::warn!("ordered flip {frame}: host GPU wait failed: {error:#x}");
            }
            renderer.command(mem, &flush)?;
        } else {
            self.command(mem, 0, &flush)?;
        }
        let start = monotonic_ns();
        let published = self.publish_scanout(monotonic_ns());
        if published && let Some(renderer) = &self.renderer {
            // The console's readback of this frame runs on the renderer.
            if let Err(error) = renderer.settle() {
                log::warn!("ordered flip {frame}: readback wait failed: {error:#x}");
            }
            renderer.retire_previous();
        }
        vmlab_hv_display::trace::write_at(
            start,
            "flip",
            format_args!(
                "frame={frame} res={id} published={} us={}",
                published as u8,
                (monotonic_ns() - start) / 1000
            ),
        );
        if self.ordered_flips == 0 {
            log::info!("virtio-gpu: the guest flips through the host-ordered flip (frame {frame})");
        }
        self.ordered_flips += 1;
        log::trace!("ordered flip {frame}: published resource {id}");
        Ok((NODATA, Vec::new()))
    }
}

impl State {
    /// A 2D flush changed the scanout at `timestamp_ns`. Leave the compose
    /// to the frame thread, which runs it once for all the flushes of one
    /// frame interval.
    fn schedule_flush(&mut self, timestamp_ns: u64) {
        let Some(pacer) = self.pacer.clone() else {
            self.publish_scanout(timestamp_ns);
            return;
        };
        self.flush_pending = Some(timestamp_ns);
        pacer.request(self.frame_due(std::time::Instant::now()));
    }

    /// When the next frame may be published.
    fn frame_due(&self, now: std::time::Instant) -> std::time::Instant {
        self.last_publish
            .map_or(now, |last| (last + pacer::FRAME_INTERVAL).max(now))
    }

    /// The frame thread's turn: publish the pending flushes, or return the
    /// time to come back at when a frame was published too recently.
    fn publish_pending(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        let timestamp_ns = self.flush_pending?;
        let due = self.frame_due(now);
        if due > now {
            return Some(due);
        }
        self.publish_scanout(timestamp_ns);
        None
    }
}

fn valid_size(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= 3840 && h <= 2160
}
fn valid_format(f: u32) -> bool {
    matches!(f, 1..=4 | 67 | 68 | 121 | 134)
}

impl State {
    fn complete_fences(&mut self) {
        if self.lost.is_some() {
            self.release_fences();
            return;
        }
        if self.retry_display {
            self.publish_scanout(self.display_timestamp);
        }
        let Some(renderer) = &self.renderer else {
            return;
        };
        let Some(mem) = &self.mem else { return };
        self.fences.retain(|fence| {
            if !renderer.complete(fence.flags, fence.id, fence.ctx, fence.ring) {
                return true;
            }
            let queue = &mut self.queues[fence.queue as usize];
            if queue.add_used(mem, fence.head, fence.len).is_ok()
                && queue.needs_notification(mem).unwrap_or(false)
                && let Some(irq) = &self.interrupt
            {
                irq.notify_queue(fence.queue);
            }
            false
        });
    }

    fn mode(&self) -> Option<Mode> {
        if !self.native {
            return None;
        }
        if let Some(renderer) = &self.renderer
            && let Some(mode) = renderer.mode()
        {
            return Some(mode);
        }
        let (width, height) = self
            .scanout
            .map(|(_, r)| (r.w, r.h))
            .unwrap_or(self.preferred);
        Some(Mode {
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Xrgb8888,
        })
    }

    fn command(
        &mut self,
        mem: &GuestMemory,
        q: usize,
        request: &[u8],
    ) -> Result<(u32, Vec<u8>), u32> {
        let mut d = Decoder::new(request);
        let kind = d.u32()?;
        d.take(20)?;
        if self.lost.is_some() && q == 0 && !matches!(kind, 0x100 | 0x10a) && !lost::releases(kind)
        {
            // The host GPU is gone (`lost.rs`).
            return Err(UNSPEC);
        }
        if q == 0 && kind == CMD_ORDERED_FLIP {
            return self.ordered_flip(mem, request, &mut d);
        }
        if q == 0 && kind == CMD_PRESENT_COPY {
            if self.acked & (1 << ORDERED_FLIP_FEATURE) == 0 {
                return Err(INVALID);
            }
            let copy = copy::decode(&request[24..])?;
            // A present copy exists only between renderer resources.
            let renderer = self.renderer.as_ref().ok_or(INVALID)?;
            return renderer.present_copy(copy).map(|()| (NODATA, Vec::new()));
        }
        if q == 0
            && !matches!(kind, 0x100 | 0x10a)
            && let Some(renderer) = &self.renderer
        {
            let result = renderer.command(mem, request)?;
            if kind == 0x200 {
                // CTX_CREATE: a Venus context has a worker of its own.
                self.workers.note(renderer.servers());
            }
            if matches!(kind, 0x103 | 0x10d) {
                self.native = true;
            }
            return Ok(result);
        }
        if q == 1 {
            if !matches!(kind, 0x300 | 0x301) {
                return Err(INVALID);
            }
            return self
                .cursor_command(kind, &mut d)
                .map(|()| (NODATA, Vec::new()));
        }
        let mut out = Vec::new();
        let response = match kind {
            0x100 => {
                out.resize(16 * 24, 0);
                out[8..12].copy_from_slice(&self.preferred.0.to_le_bytes());
                out[12..16].copy_from_slice(&self.preferred.1.to_le_bytes());
                out[16..20].copy_from_slice(&1u32.to_le_bytes());
                0x1101
            }
            0x10a => {
                if self.acked & 2 == 0 {
                    return Err(INVALID);
                }
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                d.u32()?;
                out.resize(1032, 0);
                let edid = edid(self.preferred.0, self.preferred.1);
                out[..4].copy_from_slice(&(edid.len() as u32).to_le_bytes());
                out[8..8 + edid.len()].copy_from_slice(&edid);
                0x1104
            }
            0x101 => {
                let id = d.u32()?;
                let format = d.u32()?;
                let width = d.u32()?;
                let height = d.u32()?;
                if id == 0
                    || self.resources.contains_key(&id)
                    || !valid_format(format)
                    || !valid_size(width, height)
                {
                    return Err(INVALID);
                }
                let len = width as usize * height as usize * 4;
                if self.resources.len() >= MAX_RESOURCES
                    || self
                        .resources
                        .values()
                        .map(|r| r.pixels.len())
                        .sum::<usize>()
                        + len
                        > MAX_BYTES
                {
                    return Err(0x1201);
                }
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        pixels: vec![0; len],
                        backing: Vec::new(),
                    },
                );
                NODATA
            }
            0x102 => {
                let id = d.u32()?;
                d.u32()?;
                self.resources.remove(&id).ok_or(RESOURCE)?;
                if self.scanout.is_some_and(|(i, _)| i == id) {
                    self.scanout = None;
                    self.scanout_generation = self.scanout_generation.wrapping_add(1);
                    self.dirty = true;
                    self.compose = compose::Compose::default();
                    self.flushes.clear();
                }
                NODATA
            }
            0x103 => {
                let previous = (self.native, self.scanout);
                let rect = d.rect()?;
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                let id = d.u32()?;
                if id == 0 {
                    self.scanout = None;
                } else {
                    let r = self.resources.get(&id).ok_or(RESOURCE)?;
                    if !rect.fits(r.width, r.height) {
                        return Err(INVALID);
                    }
                    self.scanout = Some((id, rect));
                }
                self.native = true;
                if previous != (self.native, self.scanout) {
                    self.scanout_generation = self.scanout_generation.wrapping_add(1);
                    self.compose = compose::Compose::default();
                    self.flushes.clear();
                }
                self.dirty = true;
                NODATA
            }
            0x104 => {
                let rect = d.rect()?;
                let id = d.u32()?;
                d.u32()?;
                let r = self.resources.get(&id).ok_or(RESOURCE)?;
                if !rect.fits(r.width, r.height) {
                    return Err(INVALID);
                }
                if self
                    .scanout
                    .is_some_and(|(i, s)| i == id && rect.overlaps(s))
                {
                    self.dirty = true;
                    // Bound queued damage even if the guest flushes faster than
                    // the host samples. A full update includes all pending pixels.
                    if self.flushes.len() < 256 {
                        self.flushes.push(rect);
                    } else if let Some((_, scanout)) = self.scanout {
                        self.flushes.clear();
                        self.flushes.push(scanout);
                    }
                }
                NODATA
            }
            0x105 => {
                let rect = d.rect()?;
                let offset = d.u64()?;
                let id = d.u32()?;
                d.u32()?;
                let r = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !rect.fits(r.width, r.height) || r.backing.is_empty() {
                    return Err(INVALID);
                }
                let stride = r.width as usize * 4;
                // Read the entire rectangle before changing the resource on a failed DMA.
                let mut pixels = vec![0; rect.w as usize * rect.h as usize * 4];
                for row in 0..rect.h as usize {
                    let source = offset.checked_add((row * stride) as u64).ok_or(INVALID)?;
                    read_backing(
                        mem,
                        &r.backing,
                        source,
                        &mut pixels[row * rect.w as usize * 4..(row + 1) * rect.w as usize * 4],
                    )?;
                }
                for row in 0..rect.h as usize {
                    let dest = (rect.y as usize + row) * stride + rect.x as usize * 4;
                    let src = row * rect.w as usize * 4;
                    r.pixels[dest..dest + rect.w as usize * 4]
                        .copy_from_slice(&pixels[src..src + rect.w as usize * 4]);
                }
                NODATA
            }
            0x106 => {
                let id = d.u32()?;
                let n = d.u32()? as usize;
                let total: usize = self.resources.values().map(|r| r.backing.len()).sum();
                if n == 0 || n > MAX_ENTRIES.saturating_sub(total) {
                    return Err(INVALID);
                }
                let r = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !r.backing.is_empty() {
                    return Err(INVALID);
                }
                let mut backing = Vec::with_capacity(n);
                for _ in 0..n {
                    let addr = d.u64()?;
                    let len = d.u32()?;
                    d.u32()?;
                    if len == 0
                        || !mem.check_range(GuestAddress(addr), len as usize, Permissions::Read)
                    {
                        return Err(INVALID);
                    }
                    backing.push((addr, len));
                }
                r.backing = backing;
                NODATA
            }
            0x107 => {
                let id = d.u32()?;
                d.u32()?;
                self.resources.get_mut(&id).ok_or(RESOURCE)?.backing.clear();
                NODATA
            }
            _ => return Err(INVALID),
        };
        Ok((response, out))
    }

    fn cursor_command(&mut self, kind: u32, d: &mut Decoder<'_>) -> Result<(), u32> {
        if d.u32()? != 0 {
            return Err(SCANOUT);
        }
        let x = d.u32()?;
        let y = d.u32()?;
        d.u32()?;
        let id = d.u32()?;
        let hot_x = d.u32()?;
        let hot_y = d.u32()?;
        d.u32()?;
        if kind == 0x300 {
            if id == 0 {
                self.cursor.visible = false;
            } else {
                let accelerated = self
                    .renderer
                    .as_ref()
                    .map(|renderer| renderer.cursor_resource(id))
                    .transpose()?;
                let r = accelerated
                    .as_ref()
                    .or_else(|| self.resources.get(&id))
                    .ok_or(RESOURCE)?;
                if r.width != 64 || r.height != 64 || hot_x >= 64 || hot_y >= 64 {
                    return Err(INVALID);
                }
                let mut pixels = Vec::with_capacity(64 * 64 * 4);
                for off in (0..r.pixels.len()).step_by(4) {
                    let mut p = r.pixel(off);
                    for c in 0..3 {
                        p[c] = (u16::from(p[c]) * u16::from(p[3]) / 255) as u8;
                    }
                    pixels.extend_from_slice(&p);
                }
                self.cursor.width = 64;
                self.cursor.height = 64;
                self.cursor.hot_x = hot_x;
                self.cursor.hot_y = hot_y;
                self.cursor.pixels = pixels;
                self.cursor.visible = true;
            }
        }
        self.cursor.x = x;
        self.cursor.y = y;
        self.cursor.generation = self.cursor.generation.wrapping_add(1);
        Ok(())
    }

    fn chain(&mut self, mem: &GuestMemory, q: usize, chain: DescriptorChain<&GuestMemory>) -> u32 {
        let timestamp_ns = monotonic_ns();
        self.current_fence = None;
        let Ok(mut reader) = chain.clone().reader(mem) else {
            return 0;
        };
        let len = reader.available_bytes();
        if !(24..=16 * 1024 * 1024 + 32).contains(&len) {
            return 0;
        }
        let mut request = vec![0; len];
        if reader.read_exact(&mut request).is_err() {
            return 0;
        }
        let Ok(mut writer) = chain.writer(mem) else {
            return 0;
        };
        // Cursor drivers commonly provide no response buffer.
        if q == 0 && writer.available_bytes() < 24 {
            return 0;
        }
        let (mut kind, payload) = match self.command(mem, q, &request) {
            Ok(response) => response,
            Err(error) => (error, Vec::new()),
        };
        let mut response = vec![0; 24];
        response[..4].copy_from_slice(&kind.to_le_bytes());
        if request[4] & 1 != 0 {
            response[4..24].copy_from_slice(&request[4..24]);
            if let Some(renderer) = &self.renderer {
                let flags = u32::from_le_bytes(request[4..8].try_into().unwrap());
                let id = u64::from_le_bytes(request[8..16].try_into().unwrap());
                let ctx = u32::from_le_bytes(request[16..20].try_into().unwrap());
                match renderer.fence(flags, id, ctx, request[20]) {
                    Err(error) => kind = error,
                    Ok(()) => self.current_fence = Some((flags, id, ctx, request[20])),
                }
            }
        }
        response[..4].copy_from_slice(&kind.to_le_bytes());
        response.extend_from_slice(&payload);
        if writer.available_bytes() < response.len() {
            return 0;
        }
        if writer.write_all(&response).is_err() {
            return 0;
        }
        if kind < 0x1200 {
            match u32::from_le_bytes(request[..4].try_into().unwrap()) {
                // A guest flushes many times in one frame. The renderer
                // keeps its own damage, so its flushes publish here.
                0x104 if self.renderer.is_none() => self.schedule_flush(timestamp_ns),
                0x102..=0x104 | 0x10d => {
                    self.publish_scanout(timestamp_ns);
                }
                _ => {}
            }
        }
        response.len() as u32
    }
}

fn read_backing(
    mem: &GuestMemory,
    backing: &[(u64, u32)],
    mut offset: u64,
    mut dst: &mut [u8],
) -> Result<(), u32> {
    for &(addr, len) in backing {
        if offset >= u64::from(len) {
            offset -= u64::from(len);
            continue;
        }
        let n = dst.len().min((u64::from(len) - offset) as usize);
        mem.read_slice(
            &mut dst[..n],
            GuestAddress(addr.checked_add(offset).ok_or(INVALID)?),
        )
        .map_err(|_| INVALID)?;
        dst = &mut dst[n..];
        if dst.is_empty() {
            return Ok(());
        }
        offset = 0;
    }
    Err(INVALID)
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        16
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[256, 64]
    }
    fn features(&self) -> u64 {
        FEATURES
            | if self.state.lock().renderer.is_some() {
                1 | (1 << 3) | (1 << 4)
            } else {
                0
            }
    }
    fn ack_features(&mut self, features: u64) {
        let supported = self.features();
        let mut s = self.state.lock();
        s.acked = features & supported;
        // A host GPU lost before this negotiation is still lost.
        s.raise_lost();
    }
    fn config_len(&self) -> u64 {
        16
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let s = self.state.lock();
        let mut config = [0; 16];
        config[..4].copy_from_slice(&s.events.to_le_bytes());
        config[8..12].copy_from_slice(&1u32.to_le_bytes());
        if let Some(renderer) = &s.renderer {
            config[12..16].copy_from_slice(&renderer.capsets.to_le_bytes());
        }
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|v| config.get(v as usize))
                .copied()
                .unwrap_or(0);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let mut clear = [0; 4];
        for (i, byte) in data.iter().enumerate() {
            if let Some(pos) = offset.checked_add(i as u64).filter(|v| (4..8).contains(v)) {
                clear[pos as usize - 4] = *byte;
            }
        }
        self.state.lock().events &= !u32::from_le_bytes(clear);
    }
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        mut queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        let mut s = self.state.lock();
        if queues.len() != 2 || queues.iter().any(|q| q.ready() && !q.is_valid(&mem)) {
            return Err(DeviceError::Other("invalid GPU queues".into()));
        }
        if !s.pending.is_empty() {
            queues = std::mem::take(&mut s.pending);
        }
        if queues.iter().any(|q| q.ready() && !q.is_valid(&mem))
            || s.resources.values().any(|r| {
                r.backing
                    .iter()
                    .any(|&(a, n)| !mem.check_range(GuestAddress(a), n as usize, Permissions::Read))
            })
        {
            return Err(DeviceError::Other(
                "invalid restored GPU memory ranges".into(),
            ));
        }
        s.mem = Some(mem);
        s.interrupt = Some(interrupt);
        s.queues = queues;
        Ok(())
    }
    fn queue_notify(&mut self, index: u16) {
        let mut s = self.state.lock();
        let index = index as usize;
        let Some(mem) = s.mem.clone() else {
            return;
        };
        if index >= s.queues.len() || !s.queues[index].ready() {
            return;
        }
        let mut used = false;
        loop {
            while let Some(chain) = s.queues[index].pop_descriptor_chain(&mem) {
                let head = chain.head_index();
                let len = s.chain(&mem, index, chain);
                if let Some((flags, id, ctx, ring)) = s.current_fence.take() {
                    s.fences.push(PendingFence {
                        queue: index as u16,
                        head,
                        len,
                        flags,
                        id,
                        ctx,
                        ring,
                    });
                    continue;
                }
                if s.queues[index].add_used(&mem, head, len).is_err() {
                    return;
                }
                used = true;
            }
            if !s.queues[index].enable_notification(&mem).unwrap_or(false) {
                break;
            }
        }
        if used
            && s.queues[index].needs_notification(&mem).unwrap_or(false)
            && let Some(irq) = &s.interrupt
        {
            irq.notify_queue(index as u16);
        }
    }
    fn reset(&mut self) {
        let mut s = self.state.lock();
        let patience = s.workers.patience;
        s.unsettled = !s.teardown(patience);
        s.fences.clear();
        s.flush_pending = None;
        s.retry_display = false;
        s.acked = 0;
        s.ordered_flips = 0;
        s.events = 0;
        s.resources.clear();
        s.scanout = None;
        s.native = false;
        s.scanout_generation = s.scanout_generation.wrapping_add(1);
        s.dirty = true;
        s.compose = compose::Compose::default();
        s.flushes.clear();
        let generation = s.cursor.generation.wrapping_add(1);
        s.cursor = Cursor {
            generation,
            ..Cursor::default()
        };
        s.mem = None;
        s.interrupt = None;
        s.queues.clear();
        s.pending.clear();
        if let Some(sink) = &s.display {
            sink.update(ScanoutUpdate {
                generation: s.scanout_generation,
                timestamp_ns: monotonic_ns(),
                damage: Vec::new(),
                pixels: ScanoutPixels::Vga,
            });
        }
    }
    fn reset_settled(&mut self) -> bool {
        let mut s = self.state.lock();
        if s.unsettled && s.teardown(std::time::Duration::ZERO) {
            log::info!("GPU reset: the renderer and its workers have stopped");
            s.unsettled = false;
        }
        !s.unsettled
    }
}

impl State {
    /// Stop the renderer and wait at most `wait` for the workers of the
    /// guest's contexts to exit. False, logged, when either is not done.
    fn teardown(&mut self, wait: std::time::Duration) -> bool {
        let mut done = true;
        if let Some(renderer) = &self.renderer {
            if let Err(error) = renderer.reset() {
                if !self.unsettled {
                    log::error!("GPU reset failed: {error:#x}; the device status stays nonzero");
                }
                done = false;
            }
            // A worker the start of this reset did not know yet.
            self.workers.note(renderer.servers());
        }
        if let Err(running) = self.workers.wait(wait) {
            if !self.unsettled {
                log::error!(
                    "GPU reset: render workers {running:?} have not exited; the device status stays nonzero"
                );
            }
            done = false;
        }
        done
    }
}

struct PendingFence {
    queue: u16,
    head: u16,
    len: u32,
    flags: u32,
    id: u64,
    ctx: u32,
    ring: u8,
}

fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the clock is valid and time points to writable timespec storage.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

struct Decoder<'a> {
    bytes: &'a [u8],
}
impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], u32> {
        if n > self.bytes.len() {
            return Err(INVALID);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, u32> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn rect(&mut self) -> Result<Rect, u32> {
        Ok(Rect {
            x: self.u32()?,
            y: self.u32()?,
            w: self.u32()?,
            h: self.u32()?,
        })
    }
}

/// QEMU's default monitor, captured through the Ubuntu guest's DRM connector.
pub(crate) fn edid(w: u32, h: u32) -> [u8; 256] {
    let mut e =
        *include_bytes!("../../../../../docs/hwref/ubuntu-26.04-display/guest/drm/Virtual-1.edid");
    if (w, h) == (1280, 800) {
        return e;
    }
    // Size requests replace the preferred detailed timing; the monitor identity
    // and secondary modes remain those of the captured monitor.
    let hb = 160u32;
    let vb = 35u32;
    let clock = ((w + hb) * (h + vb) * 60 / 10000) as u16;
    e[54..56].copy_from_slice(&clock.to_le_bytes());
    e[56] = w as u8;
    e[57] = hb as u8;
    e[58] = ((w >> 8) << 4) as u8;
    e[59] = h as u8;
    e[60] = vb as u8;
    e[61] = ((h >> 8) << 4) as u8;
    e[62..66].copy_from_slice(&[48, 32, 0x35, 0]);
    e[71] = 0x1a;
    e[127] = 0u8.wrapping_sub(e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    e
}
