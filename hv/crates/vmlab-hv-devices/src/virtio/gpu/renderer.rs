// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.
//
// Adapted from crosvm devices/src/virtio/gpu/{mod.rs,virtio_gpu.rs},
// revision ad804a6ecf1d3c4337f18b56092fc54d8ca574fa.

use super::*;
use rutabaga_gfx::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use zerocopy::IntoBytes;

pub const APERTURE_SIZE: u64 = 1 << 32;

/// The VMM keeps each host mapping registered until `unmap` returns.
/// Addresses and sizes are page aligned and relative to the shared-memory BAR.
pub trait BlobMapper: Send + Sync {
    fn size(&self) -> u64;
    fn set_base(&self, base: Option<u64>) -> Result<(), DeviceError>;
    fn map(&self, id: u32, offset: u64, host: u64, size: u64) -> Result<(), DeviceError>;
    fn unmap(&self, id: u32) -> Result<(), DeviceError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuMode {
    Virgl,
    Venus,
}

/// Descriptors held per live host resource: virglrenderer keeps the blob's
/// descriptor and rutabaga a duplicate, until the guest unreferences it.
const RESOURCE_DESCRIPTORS: u64 = 2;
/// Descriptors left for the rest of the process: disks, sockets, contexts,
/// the control and display clients.
const RESERVED_DESCRIPTORS: u64 = 256;

/// How many host resources the process's descriptor limit can pay for. A guest
/// that reaches it gets an error, where an exhausted table ends the VM.
fn resource_budget(descriptor_limit: u64) -> usize {
    (descriptor_limit.saturating_sub(RESERVED_DESCRIPTORS) / RESOURCE_DESCRIPTORS)
        .min(MAX_RESOURCES as u64) as usize
}

fn descriptor_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only the supplied structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 1024;
    }
    limit.rlim_cur
}

type Task = Box<dyn FnOnce(&mut Backend) + Send>;
type Ring = (u32, u8);

// Public virglrenderer.h export-query ABI. The query does not request fds.
#[repr(C)]
#[derive(Default)]
struct ExportQuery {
    stype: u32,
    version: u32,
    size: u32,
    resource: u32,
    num_fds: u32,
    export_fds: u32,
    fourcc: u32,
    pad: u32,
    fds: [i32; 4],
    strides: [u32; 4],
    offsets: [u32; 4],
    modifier: u64,
}

unsafe extern "C" {
    fn virgl_renderer_execute(args: *mut std::ffi::c_void, size: u32) -> i32;
}

fn export_info(id: u32, fallback: Resource3DInfo) -> Resource3DInfo {
    let mut query = ExportQuery {
        stype: 1,
        size: std::mem::size_of::<ExportQuery>() as u32,
        resource: id,
        ..Default::default()
    };
    // SAFETY: this matches the public C ABI and runs only on the renderer thread.
    if unsafe { virgl_renderer_execute((&mut query as *mut ExportQuery).cast(), query.size) } != 0
        || query.num_fds == 0
        || query.fourcc == 0
    {
        return fallback;
    }
    Resource3DInfo {
        drm_fourcc: query.fourcc,
        strides: query.strides,
        offsets: query.offsets,
        modifier: query.modifier,
        ..fallback
    }
}

#[derive(Clone)]
pub(super) struct Renderer {
    worker: Arc<Worker>,
    pub capsets: u32,
    completed: Arc<Mutex<BTreeMap<Ring, u64>>>,
    mapper: Arc<dyn BlobMapper>,
    direct: Arc<Mutex<Vec<std::sync::Weak<vmlab_hv_display::BufferLease>>>>,
}

struct Worker {
    sender: Option<mpsc::Sender<Task>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Backend) -> Result<T, u32> + Send + 'static,
    ) -> Result<T, u32> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or(0x1200u32)?
            .send(Box::new(move |backend| {
                let _ = tx.send(f(backend));
            }))
            .map_err(|_| 0x1200u32)?;
        rx.recv().map_err(|_| 0x1200u32)?
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct HostResource {
    width: u32,
    height: u32,
    format: u32,
    size: u64,
    mapped: bool,
    // Renderer iovecs borrow this allocation until detach or unref.
    memory: Option<GuestMemory>,
    scanout_info: Option<Resource3DInfo>,
    exported: Option<Arc<std::os::fd::OwnedFd>>,
    export_info: Option<Resource3DInfo>,
}

/// A fence on the renderer's timeline: the guest's, or one the host made.
struct TimelineFence {
    guest: Option<RutabagaFence>,
    /// The ring the fence was created on, which its callback names.
    ring: Ring,
    signal: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
struct Timeline {
    next: u64,
    pending: BTreeMap<u64, TimelineFence>,
}

impl Timeline {
    fn reserve(&mut self) -> Option<u64> {
        // virgl's global fence ABI carries only 32 bits. Drain the old epoch
        // before reusing IDs so coalesced callbacks cannot retire newer work.
        if self.next == u32::MAX as u64 {
            if !self.pending.is_empty() {
                return None;
            }
            self.next = 0;
        }
        self.next += 1;
        Some(self.next)
    }

    /// A callback retired `id` on `ring`. Virgl may coalesce callbacks on
    /// a ring, so every earlier fence of that ring retires with it. Returns
    /// the guest fences among them.
    fn signal(&mut self, ring: Ring, id: u64) -> Vec<RutabagaFence> {
        let ids: Vec<_> = self
            .pending
            .range(..=id)
            .filter(|(_, fence)| fence.ring == ring)
            .map(|(&id, _)| id)
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let fence = self.pending.remove(&id).unwrap();
                fence.signal.store(true, std::sync::atomic::Ordering::SeqCst);
                fence.guest
            })
            .collect()
    }
}

struct Backend {
    gpu: Rutabaga,
    resources: BTreeMap<u32, HostResource>,
    contexts: BTreeSet<u32>,
    mapper: Arc<dyn BlobMapper>,
    budget: usize,
    budget_reported: bool,
    scanout: Option<(u32, Rect)>,
    mode: GpuMode,
    scanout_copy: Option<super::scanout::Scanout>,
    node: PathBuf,
    timeline: Arc<Mutex<Timeline>>,
    /// The context rings the guest has fenced: every host queue that can
    /// hold work for a frame. An ordered flip fences each of them.
    rings: BTreeSet<Ring>,
    direct_frames: u64,
    fallback_frames: u64,
    external_frames: u64,
    readbacks: u64,
    generation: u64,
    damage: Vec<Rect>,
    pending_frame: bool,
    scanout_dirty: bool,
}

impl Drop for Backend {
    fn drop(&mut self) {
        for (&id, resource) in &self.resources {
            if resource.mapped {
                // A failed unregister must not leave KVM referencing freed host pages.
                if let Err(error) = self.mapper.unmap(id) {
                    log::warn!("GPU mapping teardown: {error}");
                    std::process::abort();
                }
                let _ = self.gpu.unmap(id);
            }
            let _ = self.gpu.unref_resource(id);
        }
        self.scanout_copy.take();
        self.gpu.force_ctx_0();
    }
}

pub(super) fn ring(flags: u32, ctx: u32, index: u8) -> Ring {
    if flags & 2 != 0 { (ctx, index) } else { (0, 0) }
}

impl Renderer {
    pub fn new(
        mode: GpuMode,
        node: PathBuf,
        mapper: Arc<dyn BlobMapper>,
    ) -> Result<Self, DeviceError> {
        Self::with_budget(mode, node, mapper, resource_budget(descriptor_limit()))
    }

    fn with_budget(
        mode: GpuMode,
        node: PathBuf,
        mapper: Arc<dyn BlobMapper>,
        budget: usize,
    ) -> Result<Self, DeviceError> {
        if budget < MAX_RESOURCES {
            log::warn!(
                "the descriptor limit of {} allows {budget} GPU resources of {MAX_RESOURCES}",
                descriptor_limit()
            );
        }
        // Validate explicitly: virglrenderer otherwise falls back to another GPU.
        let node = std::fs::canonicalize(node)?;
        if !node.starts_with("/dev/dri")
            || !node
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        {
            return Err(DeviceError::Other(
                "render_node must name a DRM render node".into(),
            ));
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)?;
        let (sender, receiver) = mpsc::channel::<Task>();
        let (ready, initialized) = mpsc::sync_channel(1);
        let completed = Arc::new(Mutex::new(BTreeMap::<Ring, u64>::new()));
        let done = completed.clone();
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let callbacks = timeline.clone();
        let worker_mapper = mapper.clone();
        let join = std::thread::Builder::new()
            .name("virtio-gpu-renderer".into())
            .spawn(move || {
                let mask = match mode {
                    GpuMode::Virgl => (1 << RUTABAGA_CAPSET_VIRGL) | (1 << RUTABAGA_CAPSET_VIRGL2),
                    GpuMode::Venus => {
                        (1 << RUTABAGA_CAPSET_VIRGL)
                            | (1 << RUTABAGA_CAPSET_VIRGL2)
                            | (1 << RUTABAGA_CAPSET_VENUS)
                    }
                };
                let handler = RutabagaFenceHandler::new(move |f: RutabagaFence| {
                    let retired = callbacks
                        .lock()
                        .signal(ring(f.flags, f.ctx_id, f.ring_idx), f.fence_id);
                    for guest in retired {
                        let mut completed = done.lock();
                        let value = completed
                            .entry(ring(guest.flags, guest.ctx_id, guest.ring_idx))
                            .or_default();
                        *value = (*value).max(guest.fence_id);
                    }
                });
                let gpu = RutabagaBuilder::new(mask, handler)
                    .set_default_component(RutabagaComponentType::VirglRenderer)
                    .set_use_egl(true)
                    .set_use_surfaceless(false)
                    .set_use_vulkan(mode == GpuMode::Venus)
                    .set_use_external_blob(mode == GpuMode::Venus)
                    .set_use_render_server(mode == GpuMode::Venus)
                    .set_rutabaga_paths(Some(vec![RutabagaPath {
                        path: node.clone(),
                        path_type: RUTABAGA_PATH_TYPE_GPU,
                    }]))
                    .build();
                let gpu = match gpu {
                    Ok(gpu) => gpu,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                if gpu.get_num_capsets() == 0 {
                    let _ = ready.send(Err(
                        "virglrenderer failed initialization; refusing rutabaga's 2D fallback"
                            .into(),
                    ));
                    return;
                }
                if mode == GpuMode::Venus
                    && !gpu
                        .get_capset(RUTABAGA_CAPSET_VENUS, 0)
                        .is_ok_and(|caps| caps.get(..4).is_some_and(|version| version != [0; 4]))
                {
                    let _ = ready.send(Err("virglrenderer returned no usable Venus capset".into()));
                    return;
                }
                let _ = ready.send(Ok(gpu.get_num_capsets()));
                let mut backend = Backend {
                    gpu,
                    resources: BTreeMap::new(),
                    contexts: BTreeSet::new(),
                    mapper: worker_mapper,
                    budget,
                    budget_reported: false,
                    scanout: None,
                    mode,
                    scanout_copy: None,
                    node,
                    timeline,
                    rings: BTreeSet::new(),
                    direct_frames: 0,
                    fallback_frames: 0,
                    external_frames: 0,
                    readbacks: 0,
                    generation: 0,
                    damage: Vec::new(),
                    pending_frame: false,
                    scanout_dirty: true,
                };
                loop {
                    match receiver.recv_timeout(Duration::from_millis(2)) {
                        Ok(task) => {
                            backend.gpu.force_ctx_0();
                            task(&mut backend);
                            backend.gpu.force_ctx_0();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    backend.gpu.event_poll();
                }
            })?;
        let capsets = initialized
            .recv()
            .map_err(|e| DeviceError::Other(e.to_string()))?
            .map_err(|e| DeviceError::Other(format!("initializing virtio-gpu renderer: {e}")))?;
        Ok(Self {
            worker: Arc::new(Worker {
                sender: Some(sender),
                join: Some(join),
            }),
            capsets,
            completed,
            mapper,
            direct: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn set_base(&self, base: Option<u64>) -> Result<(), DeviceError> {
        self.mapper.set_base(base)
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Backend) -> Result<T, u32> + Send + 'static,
    ) -> Result<T, u32> {
        self.worker.call(f)
    }

    pub fn command(&self, mem: &GuestMemory, request: &[u8]) -> Result<(u32, Vec<u8>), u32> {
        let kind = request
            .get(..4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()));
        if matches!(
            kind,
            Some(0x102 | 0x105 | 0x107 | 0x201 | 0x203 | 0x205 | 0x207 | 0x209)
        ) {
            // Wait on the caller, leaving the renderer free for client readback.
            let leases = std::mem::take(&mut *self.direct.lock());
            for lease in leases.into_iter().filter_map(|lease| lease.upgrade()) {
                if !lease.try_retire() {
                    log::debug!("scanout_wait_for_release");
                    lease.retire();
                }
            }
        }
        let mem = mem.clone();
        let request = request.to_vec();
        self.call(move |b| {
            let ctx = request
                .get(16..20)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()));
            let result = b.command(&mem, &request);
            if let Err(error) = &result {
                log::warn!(
                    "GPU command failed: type={:#x} ctx={} live={} bytes={} response={error:#x}",
                    kind.unwrap_or_default(),
                    ctx.unwrap_or_default(),
                    ctx.is_some_and(|ctx| b.contexts.contains(&ctx)),
                    request.len()
                );
            }
            result
        })
    }

    pub fn fence(&self, flags: u32, id: u64, ctx: u32, index: u8) -> Result<(), u32> {
        self.call(move |b| {
            b.fence(Some(RutabagaFence {
                flags,
                fence_id: id,
                ctx_id: ctx,
                ring_idx: index,
            }))
            .map(|_| ())
        })
    }

    /// The host wait of an ordered flip, [`Backend::order`].
    pub fn order(&self) -> Result<(), u32> {
        self.call(Backend::order)
    }

    /// Return when the renderer has run every task queued before this
    /// call, among them the console's readback of a published frame.
    pub fn settle(&self) -> Result<(), u32> {
        self.call(|_| Ok(()))
    }

    /// Wait until the display has stopped reading every published frame
    /// but the newest one, and lend those no more.
    pub fn retire_previous(&self) {
        let older: Vec<_> = {
            let mut direct = self.direct.lock();
            direct.retain(|lease| lease.strong_count() != 0);
            let live: Vec<_> = direct.iter().filter_map(|lease| lease.upgrade()).collect();
            live.split_last()
                .map(|(_, older)| older.to_vec())
                .unwrap_or_default()
        };
        for lease in older {
            lease.retire();
        }
    }

    pub fn complete(&self, flags: u32, id: u64, ctx: u32, index: u8) -> bool {
        self.completed
            .lock()
            .get(&ring(flags, ctx, index))
            .is_some_and(|done| *done >= id)
    }

    pub fn has_contexts(&self) -> bool {
        self.call(|b| Ok(!b.contexts.is_empty())).unwrap_or(true)
    }

    pub fn frame(&self) -> Result<Option<Frame>, u32> {
        self.dma_frame(true)?.map(cpu_frame).transpose()
    }

    pub fn mode(&self) -> Option<Mode> {
        self.call(|b| {
            Ok(b.scanout.map(|(_, rect)| Mode {
                width: rect.w,
                height: rect.h,
                stride: rect.w * 4,
                format: PixelFormat::Xrgb8888,
            }))
        })
        .ok()
        .flatten()
    }

    pub fn cursor_resource(&self, id: u32) -> Result<Resource, u32> {
        self.call(move |b| {
            let resource = b.resources.get(&id).ok_or(RESOURCE)?;
            if resource.width != 64 || resource.height != 64 || !valid_format(resource.format) {
                return Err(INVALID);
            }
            let format = resource.format;
            let mut pixels = vec![0; 64 * 64 * 4];
            host(b.gpu.transfer_read(
                0,
                id,
                Transfer3D::new_2d(0, 0, 64, 64, 0),
                Some(std::io::IoSliceMut::new(&mut pixels)),
            ))?;
            Ok(Resource {
                width: 64,
                height: 64,
                format,
                pixels,
                backing: vec![],
            })
        })
    }

    pub fn dma_frame(&self, cpu: bool) -> Result<Option<super::DmaFrame>, u32> {
        let mut frame = self.call(move |b| {
            let Some((id, rect)) = b.scanout else {
                return Ok(None);
            };
            b.synchronize()?;
            let resource = b.resources.get_mut(&id).ok_or(RESOURCE)?;
            let mut info = if let Some(scanout) = resource.scanout_info {
                scanout
            } else if let Some(info) = resource.export_info {
                info
            } else {
                export_info(id, host(b.gpu.resource3d_info(id))?)
            };
            let (width, height) = if let Some(scanout) = resource.scanout_info {
                (scanout.width, scanout.height)
            } else {
                (resource.width, resource.height)
            };
            if resource.exported.is_none() {
                let handle = host(b.gpu.export_blob(id))?;
                let handle = handle.as_mesa_handle().ok_or(INVALID)?;
                if handle.handle_type != RUTABAGA_HANDLE_TYPE_MEM_DMABUF {
                    return Err(INVALID);
                }
                use magma_gpu::util::AsRawDescriptor;
                use std::os::fd::FromRawFd;
                // SAFETY: duplicate the live rutabaga descriptor into independent ownership.
                let raw = unsafe {
                    libc::fcntl(
                        handle.os_handle.as_raw_descriptor(),
                        libc::F_DUPFD_CLOEXEC,
                        0,
                    )
                };
                if raw < 0 {
                    return Err(0x1200);
                }
                // SAFETY: fcntl returned a new owned descriptor.
                resource.exported =
                    Some(Arc::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }));
            }
            let fd = resource.exported.as_ref().unwrap().clone();
            use std::os::fd::AsRawFd;
            if b.scanout_copy.is_none() {
                b.scanout_copy = Some(super::scanout::Scanout::new(&b.node)?);
            }
            let copy = b.scanout_copy.as_mut().unwrap();
            if resource.export_info.is_none() {
                // Blob scanouts carry their own layout. Probing an implicit Vulkan
                // allocation through GBM can change NVIDIA's buffer interpretation.
                if resource.scanout_info.is_none() {
                    info = copy.metadata(fd.as_raw_fd(), info, width, height);
                }
                resource.export_info = Some(info);
            }
            let frame = if copy.direct(info, width, height, rect) {
                b.direct_frames += 1;
                super::DmaFrame {
                    fd: fd.clone(),
                    acquire: None,
                    width,
                    height,
                    stride: info.strides[0],
                    fourcc: info.drm_fourcc,
                    modifier: info.modifier,
                    pixels: None,
                    buffers: Arc::new(vec![fd]),
                    buffer: 0,
                    lease: vmlab_hv_display::BufferLease::new(),
                    direct: true,
                    reader: None,
                    snapshot: false,
                }
            } else {
                let frame = copy.capture(fd.as_raw_fd(), info, width, height, rect, false)?;
                b.fallback_frames += 1;
                if copy.external_only(info) {
                    b.external_frames += 1;
                }
                frame
            };
            b.pending_frame = false;
            b.damage.clear();
            b.scanout_dirty = false;
            b.log_stats();
            Ok(Some(frame))
        })?;
        if let Some(frame) = frame.as_mut() {
            if frame.direct {
                let mut direct = self.direct.lock();
                direct.retain(|lease| lease.strong_count() != 0);
                direct.push(Arc::downgrade(&frame.lease));
            }
            let worker = Arc::downgrade(&self.worker);
            let fd = frame.fd.clone();
            let (width, height, stride, fourcc, modifier) = (
                frame.width,
                frame.height,
                frame.stride,
                frame.fourcc,
                frame.modifier,
            );
            let capture = move |b: &mut Backend| {
                use std::os::fd::AsRawFd;
                b.readbacks += 1;
                let pixels = readback(
                    b.scanout_copy.as_mut().ok_or(0x1200u32)?,
                    fd.as_raw_fd(),
                    Resource3DInfo {
                        drm_fourcc: fourcc,
                        modifier,
                        strides: [stride, 0, 0, 0],
                        ..Default::default()
                    },
                    width,
                    height,
                )?;
                b.log_stats();
                Ok(pixels)
            };
            if cpu {
                let pin = frame.lease.acquire().ok_or(0x1200u32)?;
                let result = Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
                let destination = result.clone();
                // Readback runs on the renderer after export, without delaying
                // dma-buf publication. Its lease excludes guest writes meanwhile.
                self.worker
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(Box::new(move |backend| {
                        let pixels = capture(backend);
                        drop(pin);
                        *destination.0.lock().unwrap() = Some(pixels);
                        destination.1.notify_all();
                    }))
                    .map_err(|_| 0x1200u32)?;
                frame.snapshot = true;
                frame.reader = Some(Arc::new(move || {
                    let mut pixels = result.0.lock().unwrap();
                    while pixels.is_none() {
                        pixels = result.1.wait(pixels).unwrap();
                    }
                    pixels.as_ref().unwrap().clone()
                }));
            } else {
                frame.reader = Some(Arc::new(move || {
                    worker.upgrade().ok_or(0x1200u32)?.call(capture.clone())
                }));
            }
        }
        Ok(frame)
    }

    pub fn take_damage(&self) -> Result<Option<(u64, Vec<Rect>)>, u32> {
        self.call(|b| Ok(b.pending_frame.then(|| (b.generation, b.damage.clone()))))
    }

    pub fn reset(&self) -> Result<(), u32> {
        let leases = std::mem::take(&mut *self.direct.lock());
        for lease in leases.into_iter().filter_map(|lease| lease.upgrade()) {
            lease.retire();
        }
        self.call(|b| {
            log::debug!("GPU renderer reset: contexts={:?}", b.contexts);
            let ids: Vec<_> = b.resources.keys().copied().collect();
            for id in ids {
                b.unref(id)?;
            }
            for id in std::mem::take(&mut b.contexts) {
                host(b.gpu.destroy_context(id))?;
            }
            b.scanout = None;
            b.pending_frame = false;
            b.damage.clear();
            b.rings.clear();
            b.timeline.lock().pending.clear();
            Ok(())
        })?;
        self.completed.lock().clear();
        Ok(())
    }
}

fn host<T>(result: RutabagaResult<T>) -> Result<T, u32> {
    result.map_err(|error| {
        log::warn!("virtio-gpu: {error}");
        0x1200
    })
}

fn cpu_frame(frame: super::DmaFrame) -> Result<Frame, u32> {
    let mut pixels = frame.readback().map_err(|_| 0x1200u32)?;
    if matches!(
        frame.fourcc,
        vmlab_hv_display::protocol::XBGR8888 | vmlab_hv_display::protocol::ABGR8888
    ) {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
    }
    Ok(Frame {
        mode: Mode {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            format: PixelFormat::Xrgb8888,
        },
        pixels,
        dirty: true,
        damage: vec![crate::display::Damage {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        }],
    })
}

fn readback(
    copy: &mut super::scanout::Scanout,
    fd: i32,
    info: Resource3DInfo,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, u32> {
    let pixels = copy.readback(fd, info, width, height)?;
    let source_stride = pixels.len() / height as usize;
    let mut output = vec![0; info.strides[0] as usize * height as usize];
    for (source, destination) in pixels
        .chunks(source_stride)
        .zip(output.chunks_mut(info.strides[0] as usize))
    {
        destination[..width as usize * 4].copy_from_slice(&source[..width as usize * 4]);
        if matches!(
            info.drm_fourcc,
            vmlab_hv_display::protocol::XBGR8888 | vmlab_hv_display::protocol::ABGR8888
        ) {
            for pixel in destination[..width as usize * 4].as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
    }
    Ok(output)
}

fn iovecs(mem: &GuestMemory, d: &mut Decoder<'_>, count: usize) -> Result<Vec<RutabagaIovec>, u32> {
    if count > MAX_ENTRIES {
        return Err(INVALID);
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let address = d.u64()?;
        let length = d.u32()? as usize;
        d.u32()?;
        if length == 0 {
            return Err(INVALID);
        }
        // Split at guest-memory region boundaries rather than assuming contiguous host RAM.
        vm_memory::GuestMemory::get_slices(
            mem,
            GuestAddress(address),
            length,
            Permissions::ReadWrite,
        )
        .map_err(|_| INVALID)?
        .try_for_each(|slice| {
            let slice = slice.map_err(|_| INVALID)?;
            result.push(RutabagaIovec {
                base: slice.ptr_guard_mut().as_ptr().cast(),
                len: slice.len(),
            });
            Ok::<_, u32>(())
        })?;
    }
    Ok(result)
}

impl Backend {
    fn log_stats(&self) {
        log::debug!(
            "scanout_export {}",
            serde_json::json!({ "node": self.node, "direct": self.direct_frames, "fallback": self.fallback_frames, "readbacks": self.readbacks,
                "external_only": self.external_frames,
                "source": self.scanout.and_then(|(id, _)| self.resources.get(&id)?.export_info).map(|info| serde_json::json!({ "modifier": info.modifier, "fourcc": info.drm_fourcc })) })
        );
    }
    fn fence(
        &mut self,
        guest: Option<RutabagaFence>,
    ) -> Result<Arc<std::sync::atomic::AtomicBool>, u32> {
        let fence = match guest {
            Some(guest) => {
                let ring = ring(guest.flags, guest.ctx_id, guest.ring_idx);
                if ring.0 != 0 {
                    self.rings.insert(ring);
                }
                guest
            }
            None => RutabagaFence {
                flags: 1,
                fence_id: 0,
                ctx_id: 0,
                ring_idx: 0,
            },
        };
        self.create_fence(fence, guest)
    }

    /// Put `fence` on the timeline under a fresh ID. `guest` is the fence
    /// the guest waits for, `None` for one only the host waits for.
    fn create_fence(
        &mut self,
        mut fence: RutabagaFence,
        guest: Option<RutabagaFence>,
    ) -> Result<Arc<std::sync::atomic::AtomicBool>, u32> {
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start = std::time::Instant::now();
        let id = loop {
            let mut timeline = self.timeline.lock();
            if let Some(id) = timeline.reserve() {
                timeline.pending.insert(
                    id,
                    TimelineFence {
                        guest,
                        ring: ring(fence.flags, fence.ctx_id, fence.ring_idx),
                        signal: signal.clone(),
                    },
                );
                break id;
            }
            drop(timeline);
            self.gpu.event_poll();
            if start.elapsed() > Duration::from_secs(5) {
                return Err(0x1200);
            }
            std::thread::yield_now();
        };
        fence.fence_id = id;
        if let Err(error) = host(self.gpu.create_fence(fence)) {
            self.timeline.lock().pending.remove(&id);
            return Err(error);
        }
        Ok(signal)
    }

    fn synchronize(&mut self) -> Result<(), u32> {
        self.fence(None)?;
        let signals: Vec<_> = self
            .timeline
            .lock()
            .pending
            .values()
            .map(|fence| fence.signal.clone())
            .collect();
        let start = std::time::Instant::now();
        while signals
            .iter()
            .any(|signal| !signal.load(std::sync::atomic::Ordering::SeqCst))
        {
            self.gpu.event_poll();
            if start.elapsed() > Duration::from_secs(5) {
                return Err(0x1200);
            }
            std::thread::yield_now();
        }
        Ok(())
    }
    /// Wait until the host GPU has finished all work it was given for this
    /// guest: a fence on every context ring the guest has fenced, whatever
    /// process the work came from, and then everything on the timeline.
    /// A guest sees only its own fences, so it cannot make this wait.
    fn order(&mut self) -> Result<(), u32> {
        for (ctx, ring_idx) in self.rings.clone() {
            let fence = RutabagaFence {
                flags: 3,
                fence_id: 0,
                ctx_id: ctx,
                ring_idx,
            };
            if self.create_fence(fence, None).is_err() {
                // The ring's queue is gone: nothing more can run on it.
                self.rings.remove(&(ctx, ring_idx));
            }
        }
        self.synchronize()
    }

    /// Refuse a resource the descriptor limit cannot pay for.
    fn admit_resource(&mut self) -> Result<(), u32> {
        if self.resources.len() < self.budget {
            return Ok(());
        }
        if !self.budget_reported {
            self.budget_reported = true;
            log::warn!(
                "GPU resource limit reached: the guest holds {} resources",
                self.resources.len()
            );
        }
        Err(if self.budget < MAX_RESOURCES {
            OUT_OF_MEMORY
        } else {
            INVALID
        })
    }

    fn unref(&mut self, id: u32) -> Result<(), u32> {
        let r = self.resources.get(&id).ok_or(RESOURCE)?;
        if r.mapped {
            self.mapper.unmap(id).map_err(|_| 0x1200u32)?;
            host(self.gpu.unmap(id))?;
        }
        host(self.gpu.unref_resource(id))?;
        self.resources.remove(&id);
        if self.scanout.is_some_and(|(scanout, _)| scanout == id) {
            self.scanout = None;
        }
        Ok(())
    }

    fn command(&mut self, mem: &GuestMemory, request: &[u8]) -> Result<(u32, Vec<u8>), u32> {
        let previous = self.scanout;
        let mut d = Decoder::new(request);
        let kind = d.u32()?;
        d.u32()?;
        d.u64()?;
        let ctx = d.u32()?;
        d.u32()?;
        if matches!(kind, 0x200 | 0x201) {
            log::debug!("GPU context command: type={kind:#x} ctx={ctx}");
        }
        let mut out = Vec::new();
        let response = match kind {
            0x108 => {
                let (id, version, size) = host(self.gpu.get_capset_info(d.u32()?))?;
                d.u32()?;
                for value in [id, version, size, 0] {
                    out.extend_from_slice(&value.to_le_bytes());
                }
                0x1102
            }
            0x109 => {
                let id = d.u32()?;
                let version = d.u32()?;
                out = host(self.gpu.get_capset(id, version))?;
                0x1103
            }
            0x200 => {
                let len = d.u32()? as usize;
                let init = d.u32()?;
                let name = d.take(64)?;
                if ctx == 0 || len > 64 || self.contexts.len() >= MAX_RESOURCES {
                    return Err(INVALID);
                }
                let capset = init & 0xff;
                if !(matches!(
                    (self.mode, capset),
                    (GpuMode::Virgl, 0..=2) | (GpuMode::Venus, 0..=2 | 4)
                )) || init & !0xff != 0
                {
                    return Err(INVALID);
                }
                let name = std::str::from_utf8(&name[..len]).map_err(|_| INVALID)?;
                log::debug!("GPU context create: ctx={ctx} init={init:#x} name={name:?}");
                host(self.gpu.create_context(ctx, init, Some(name)))?;
                self.contexts.insert(ctx);
                NODATA
            }
            0x201 => {
                host(self.gpu.destroy_context(ctx))?;
                self.contexts.remove(&ctx);
                self.rings.retain(|&(owner, _)| owner != ctx);
                NODATA
            }
            0x202 | 0x203 => {
                let id = d.u32()?;
                d.u32()?;
                if self.contexts.contains(&ctx) && self.resources.contains_key(&id) {
                    if kind == 0x202 {
                        host(self.gpu.context_attach_resource(ctx, id))?;
                    } else {
                        host(self.gpu.context_detach_resource(ctx, id))?;
                    }
                }
                // Guest GEM file operations can precede context creation or outlive
                // context/resource teardown. Match virglrenderer's no-op handling.
                NODATA
            }
            0x101 | 0x204 => {
                let id = d.u32()?;
                let create = if kind == 0x101 {
                    ResourceCreate3D {
                        target: 2,
                        format: d.u32()?,
                        bind: 2,
                        width: d.u32()?,
                        height: d.u32()?,
                        depth: 1,
                        array_size: 1,
                        last_level: 0,
                        nr_samples: 0,
                        flags: 0,
                    }
                } else {
                    ResourceCreate3D {
                        target: d.u32()?,
                        format: d.u32()?,
                        bind: d.u32()?,
                        width: d.u32()?,
                        height: d.u32()?,
                        depth: d.u32()?,
                        array_size: d.u32()?,
                        last_level: d.u32()?,
                        nr_samples: d.u32()?,
                        flags: d.u32()?,
                    }
                };
                if id == 0 {
                    return Err(INVALID);
                }
                self.admit_resource()?;
                host(self.gpu.resource_create_3d(id, create))?;
                let backing = if create.bind & (1 << 18) != 0 {
                    if self.scanout_copy.is_none() {
                        match super::scanout::Scanout::new(&self.node) {
                            Ok(copy) => self.scanout_copy = Some(copy),
                            Err(error) => {
                                let _ = host(self.gpu.unref_resource(id));
                                return Err(error);
                            }
                        }
                    }
                    match self.scanout_copy.as_ref().unwrap().resource(id, create) {
                        Ok(backing) => backing,
                        Err(error) => {
                            let _ = host(self.gpu.unref_resource(id));
                            return Err(error);
                        }
                    }
                } else {
                    None
                };
                self.resources.insert(
                    id,
                    HostResource {
                        width: create.width,
                        height: create.height,
                        format: create.format,
                        size: 0,
                        mapped: false,
                        memory: None,
                        scanout_info: None,
                        exported: backing.as_ref().map(|(fd, _)| fd.clone()),
                        export_info: backing.map(|(_, info)| info),
                    },
                );
                NODATA
            }
            0x102 => {
                let id = d.u32()?;
                d.u32()?;
                self.unref(id)?;
                NODATA
            }
            0x106 => {
                let id = d.u32()?;
                let count = d.u32()? as usize;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                log::debug!(
                    "GPU attach backing: id={id} entries={count} backed={}",
                    resource.memory.is_some()
                );
                if resource.memory.is_some() {
                    return Err(INVALID);
                }
                let vecs = iovecs(mem, &mut d, count)?;
                host(self.gpu.attach_backing(id, vecs))?;
                resource.memory = Some(mem.clone());
                NODATA
            }
            0x107 => {
                let id = d.u32()?;
                d.u32()?;
                host(self.gpu.detach_backing(id))?;
                self.resources.get_mut(&id).ok_or(RESOURCE)?.memory = None;
                NODATA
            }
            0x105 | 0x205 | 0x206 => {
                let x = d.u32()?;
                let y = d.u32()?;
                let (z, w, h, depth) = if kind == 0x105 {
                    (0, d.u32()?, d.u32()?, 1)
                } else {
                    (d.u32()?, d.u32()?, d.u32()?, d.u32()?)
                };
                let offset = d.u64()?;
                let id = d.u32()?;
                let level = d.u32()?;
                let (stride, layer_stride) = if kind == 0x105 {
                    (0, 0)
                } else {
                    (d.u32()?, d.u32()?)
                };
                let transfer = Transfer3D {
                    x,
                    y,
                    z,
                    w,
                    h,
                    d: depth,
                    level,
                    stride,
                    layer_stride,
                    offset,
                };
                if kind == 0x206 {
                    host(self.gpu.transfer_read(ctx, id, transfer, None))?;
                } else {
                    host(self.gpu.transfer_write(ctx, id, transfer, None))?;
                }
                NODATA
            }
            0x207 => {
                let size = d.u32()? as usize;
                d.u32()?;
                if size > 16 * 1024 * 1024 || !size.is_multiple_of(4) {
                    return Err(INVALID);
                }
                // virglrenderer requires dword alignment even for an empty stream.
                let mut command: Vec<u32> = d
                    .take(size)?
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|word| u32::from_ne_bytes(*word))
                    .collect();
                // Windows uses empty context-zero submissions for global fences
                // and vblank notifications. The transport still queues their fence.
                if ctx != 0 || size != 0 {
                    host(
                        self.gpu
                            .submit_command(ctx, command.as_mut_slice().as_mut_bytes(), &[]),
                    )?;
                }
                NODATA
            }
            0x103 => {
                let rect = d.rect()?;
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                let id = d.u32()?;
                if id == 0 {
                    self.scanout = None;
                } else {
                    let r = self.resources.get(&id).ok_or(RESOURCE)?;
                    if !valid_size(rect.w, rect.h)
                        || !rect.fits(r.width, r.height)
                        || !valid_format(r.format)
                    {
                        return Err(INVALID);
                    }
                    self.scanout = Some((id, rect));
                }
                NODATA
            }
            0x104 => {
                let rect = d.rect()?;
                log::debug!("guest flush {},{},{}x{}", rect.x, rect.y, rect.w, rect.h);
                let id = d.u32()?;
                d.u32()?;
                let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                let (width, height) = resource
                    .scanout_info
                    .map(|info| (info.width, info.height))
                    .unwrap_or((resource.width, resource.height));
                if !rect.fits(width, height) {
                    return Err(INVALID);
                }
                if let Some((scanout_id, scanout)) = self.scanout
                    && scanout_id == id
                    && rect.overlaps(scanout)
                {
                    let x = rect.x.max(scanout.x);
                    let y = rect.y.max(scanout.y);
                    let damage = Rect {
                        x: x - scanout.x,
                        y: y - scanout.y,
                        w: (rect.x + rect.w).min(scanout.x + scanout.w) - x,
                        h: (rect.y + rect.h).min(scanout.y + scanout.h) - y,
                    };
                    if self.damage.len() < 256 {
                        self.damage.push(damage);
                    } else {
                        self.damage = vec![Rect {
                            x: 0,
                            y: 0,
                            w: scanout.w,
                            h: scanout.h,
                        }];
                    }
                    self.pending_frame = true;
                }
                // Virgl flush is ordered by the following fence. Venus blob flush
                // does not use the optional rutabaga resource_flush operation.
                NODATA
            }
            0x10c => {
                let id = d.u32()?;
                let blob_mem = d.u32()?;
                let blob_flags = d.u32()?;
                let count = d.u32()? as usize;
                let blob_id = d.u64()?;
                let size = d.u64()?;
                if id == 0 || size == 0 || size > self.mapper.size() {
                    return Err(INVALID);
                }
                self.admit_resource()?;
                let vecs = iovecs(mem, &mut d, count)?;
                // An empty Vec still has a non-null pointer. Passing it through
                // the C API would mark a host-only blob as already guest-backed.
                let vecs = (!vecs.is_empty()).then_some(vecs);
                let memory = vecs.as_ref().map(|_| mem.clone());
                log::debug!("GPU create blob: id={id} ctx={ctx} memory={blob_mem} entries={count}");
                host(self.gpu.resource_create_blob(
                    ctx,
                    id,
                    ResourceCreateBlob {
                        blob_mem,
                        blob_flags,
                        blob_id,
                        size,
                    },
                    vecs,
                    None,
                ))?;
                self.resources.insert(
                    id,
                    HostResource {
                        width: 0,
                        height: 0,
                        format: 0,
                        size,
                        mapped: false,
                        memory,
                        scanout_info: None,
                        exported: None,
                        export_info: None,
                    },
                );
                NODATA
            }
            0x10d => {
                let rect = d.rect()?;
                if d.u32()? != 0 {
                    return Err(SCANOUT);
                }
                let id = d.u32()?;
                let width = d.u32()?;
                let height = d.u32()?;
                let format = d.u32()?;
                d.u32()?;
                let mut strides = [0; 4];
                let mut offsets = [0; 4];
                for stride in &mut strides {
                    *stride = d.u32()?;
                }
                for offset in &mut offsets {
                    *offset = d.u32()?;
                }
                if id == 0 {
                    self.scanout = None;
                } else {
                    if !valid_size(width, height)
                        || !rect.fits(width, height)
                        || !valid_format(format)
                        || strides[0] < width * 4
                        || strides[1..] != [0; 3]
                        || offsets[1..] != [0; 3]
                    {
                        return Err(INVALID);
                    }
                    let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                    if (offsets[0] as u64)
                        .checked_add(strides[0] as u64 * height as u64)
                        .is_none_or(|end| end > resource.size)
                    {
                        return Err(INVALID);
                    }
                    let drm_fourcc = match format {
                        1 => u32::from_le_bytes(*b"AR24"),
                        2 => u32::from_le_bytes(*b"XR24"),
                        67 => u32::from_le_bytes(*b"AB24"),
                        134 => u32::from_le_bytes(*b"XB24"),
                        _ => return Err(INVALID),
                    };
                    resource.scanout_info = Some(Resource3DInfo {
                        width,
                        height,
                        drm_fourcc,
                        strides,
                        offsets,
                        modifier: super::scanout::INVALID_MODIFIER,
                    });
                    resource.export_info = None;
                    self.scanout = Some((id, rect));
                }
                NODATA
            }
            0x208 => {
                let id = d.u32()?;
                d.u32()?;
                let offset = d.u64()?;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                let size = resource.size;
                if resource.mapped
                    || offset & 4095 != 0
                    || size & 4095 != 0
                    || offset
                        .checked_add(size)
                        .is_none_or(|end| end > self.mapper.size())
                {
                    return Err(INVALID);
                }
                let info = host(self.gpu.map_info(id))?;
                let mapping = host(self.gpu.map(id))?;
                if mapping.size < size || mapping.ptr & 4095 != 0 {
                    let _ = self.gpu.unmap(id);
                    return Err(INVALID);
                }
                if let Err(error) = self.mapper.map(id, offset, mapping.ptr, size) {
                    let _ = self.gpu.unmap(id);
                    log::warn!("GPU blob mapping: {error}");
                    return Err(0x1200);
                }
                resource.mapped = true;
                out.extend_from_slice(&(info & RUTABAGA_MAP_CACHE_MASK).to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                0x1106
            }
            0x209 => {
                let id = d.u32()?;
                d.u32()?;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !resource.mapped {
                    return Err(INVALID);
                }
                self.mapper.unmap(id).map_err(|_| 0x1200u32)?;
                host(self.gpu.unmap(id))?;
                resource.mapped = false;
                NODATA
            }
            _ => return Err(INVALID),
        };
        if previous != self.scanout {
            self.generation = self.generation.wrapping_add(1);
        }
        if matches!(kind, 0x105 | 0x107 | 0x201 | 0x203 | 0x205 | 0x207 | 0x209) {
            self.scanout_dirty = true;
        }
        if (matches!(kind, 0x103 | 0x10d) && (previous != self.scanout || self.scanout_dirty))
            || (kind == 0x102 && previous != self.scanout)
        {
            self.pending_frame = true;
            self.damage = self
                .scanout
                .map(|(_, r)| {
                    vec![Rect {
                        x: 0,
                        y: 0,
                        w: r.w,
                        h: r.h,
                    }]
                })
                .unwrap_or_default();
        }
        Ok((response, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_frame_preserves_red_and_blue_for_every_scanout_format() {
        for (fourcc, bytes) in [
            (*b"AR24", [19, 37, 211, 255]),
            (*b"XR24", [19, 37, 211, 255]),
            (*b"AB24", [211, 37, 19, 255]),
            (*b"XB24", [211, 37, 19, 255]),
        ] {
            let fd = Arc::new(std::fs::File::open("/dev/null").unwrap().into());
            let frame = cpu_frame(super::super::DmaFrame {
                fd: Arc::clone(&fd),
                acquire: None,
                width: 1,
                height: 1,
                stride: 4,
                fourcc: u32::from_le_bytes(fourcc),
                modifier: 0,
                pixels: Some(Arc::new(bytes.to_vec())),
                buffers: Arc::new(vec![fd]),
                buffer: 0,
                lease: vmlab_hv_display::BufferLease::new(),
                direct: true,
                reader: None,
                snapshot: false,
            })
            .unwrap();
            assert_eq!(frame.pixels, [19, 37, 211, 255], "fourcc={fourcc:?}");
        }
    }

    #[test]
    fn resource_budget_follows_the_descriptor_limit() {
        // The default soft limit pays for fewer resources than the device allows.
        assert_eq!(resource_budget(1024), 384);
        assert_eq!(resource_budget(4096), 1920);
        assert_eq!(resource_budget(1 << 20), MAX_RESOURCES);
        assert_eq!(resource_budget(u64::MAX), MAX_RESOURCES);
        assert_eq!(resource_budget(0), 0);
        assert_eq!(resource_budget(RESERVED_DESCRIPTORS), 0);
    }

    struct TestMapper;
    impl BlobMapper for TestMapper {
        fn size(&self) -> u64 {
            APERTURE_SIZE
        }
        fn set_base(&self, _: Option<u64>) -> Result<(), DeviceError> {
            Ok(())
        }
        fn map(&self, _: u32, _: u64, _: u64, _: u64) -> Result<(), DeviceError> {
            Ok(())
        }
        fn unmap(&self, _: u32) -> Result<(), DeviceError> {
            Ok(())
        }
    }

    fn request(kind: u32, ctx: u32, body: &[u32]) -> Vec<u8> {
        let mut request = vec![0; 24];
        request[..4].copy_from_slice(&kind.to_le_bytes());
        request[16..20].copy_from_slice(&ctx.to_le_bytes());
        for word in body {
            request.extend_from_slice(&word.to_le_bytes());
        }
        request
    }

    /// The descriptors of blob memory this process holds. Other tests of the
    /// binary open and close files of other kinds meanwhile.
    fn open_descriptors() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| {
                let target = target.to_string_lossy();
                target.contains("vkr-shmem") || target.contains("dmabuf")
            })
            .count()
    }

    /// One Venus context that makes, maps, unmaps and unreferences `blobs`
    /// shared-memory blobs, as a guest program does from start to exit.
    fn guest_program(renderer: &Renderer, mem: &GuestMemory, ctx: u32, blobs: u32) -> usize {
        let mut create = vec![7, 4];
        create.extend_from_slice(&[0; 16]);
        let mut create = request(0x200, ctx, &create);
        create[32..39].copy_from_slice(b"fd-test");
        renderer.command(mem, &create).unwrap();
        for id in 1..=blobs {
            // HOST3D memory, mappable, blob 0: a shared-memory allocation of the context.
            let blob = [id, 2, 1, 0, 0, 0, 4096, 0];
            renderer.command(mem, &request(0x10c, ctx, &blob)).unwrap();
            let map = [id, 0, (id - 1) * 4096, 0];
            renderer.command(mem, &request(0x208, ctx, &map)).unwrap();
        }
        let held = open_descriptors();
        for id in 1..=blobs {
            renderer
                .command(mem, &request(0x209, ctx, &[id, 0]))
                .unwrap();
            renderer
                .command(mem, &request(0x102, ctx, &[id, 0]))
                .unwrap();
        }
        renderer.command(mem, &request(0x201, ctx, &[])).unwrap();
        held
    }

    #[test]
    #[cfg_attr(
        vmlab_ci_no_host_assets,
        ignore = "requires an owner-selected DRM render node"
    )]
    fn venus_blobs_return_their_descriptors_and_stop_at_the_budget() {
        let Some(node) = std::env::var_os("VMLAB_HV_TEST_RENDER_NODE") else {
            eprintln!("SKIP blob descriptors: set VMLAB_HV_TEST_RENDER_NODE");
            return;
        };
        const BLOBS: u32 = 32;
        let renderer = Renderer::with_budget(
            GpuMode::Venus,
            PathBuf::from(node),
            Arc::new(TestMapper),
            BLOBS as usize,
        )
        .unwrap();
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
        // The first program pays for what the renderer opens once.
        guest_program(&renderer, &mem, 1, BLOBS);
        let idle = open_descriptors();
        for round in 0..16 {
            let held = guest_program(&renderer, &mem, 2 + round, BLOBS);
            assert!(
                held >= idle + BLOBS as usize,
                "round {round}: {BLOBS} live blobs hold {held} descriptors, idle is {idle}"
            );
            assert_eq!(
                open_descriptors(),
                idle,
                "round {round}: descriptors outlived their blobs"
            );
        }

        // At the budget the guest gets an error and the host opens nothing.
        let mut create = vec![7, 4];
        create.extend_from_slice(&[0; 16]);
        renderer
            .command(&mem, &request(0x200, 99, &create))
            .unwrap();
        for id in 1..=BLOBS {
            let blob = [id, 2, 1, 0, 0, 0, 4096, 0];
            renderer.command(&mem, &request(0x10c, 99, &blob)).unwrap();
        }
        let full = open_descriptors();
        let over = [BLOBS + 1, 2, 1, 0, 0, 0, 4096, 0];
        assert_eq!(
            renderer.command(&mem, &request(0x10c, 99, &over)),
            Err(OUT_OF_MEMORY)
        );
        let classic = [BLOBS + 1, 2, 1, 2, 64, 64, 1, 1, 0, 0, 0];
        assert_eq!(
            renderer.command(&mem, &request(0x204, 99, &classic)),
            Err(OUT_OF_MEMORY)
        );
        assert_eq!(open_descriptors(), full);
        renderer
            .command(&mem, &request(0x102, 99, &[1, 0]))
            .unwrap();
        renderer.command(&mem, &request(0x10c, 99, &over)).unwrap();
        renderer.reset().unwrap();
        assert_eq!(open_descriptors(), idle);
    }

    #[test]
    fn a_host_fence_on_a_context_ring_retires_only_with_that_ring() {
        let mut timeline = Timeline::default();
        let signal = || Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guest = RutabagaFence {
            flags: 3,
            fence_id: 7,
            ctx_id: 5,
            ring_idx: 1,
        };
        let (global, own, other) = (signal(), signal(), signal());
        for (ring, guest, signal) in [
            ((0, 0), None, &global),
            ((5, 1), None, &own),
            ((5, 1), Some(guest), &other),
        ] {
            let id = timeline.reserve().unwrap();
            timeline.pending.insert(
                id,
                TimelineFence {
                    guest,
                    ring,
                    signal: signal.clone(),
                },
            );
        }
        let done = |signal: &Arc<std::sync::atomic::AtomicBool>| {
            signal.load(std::sync::atomic::Ordering::SeqCst)
        };

        // A callback of the global timeline does not retire ring work.
        assert!(timeline.signal((0, 0), 3).is_empty());
        assert!(done(&global) && !done(&own) && !done(&other));
        // Nor does a callback of another ring of the same context.
        assert!(timeline.signal((5, 0), 3).is_empty());
        assert!(!done(&own));
        // The ring's callback retires the host fence and hands back the
        // guest's fence of the same ring.
        let retired = timeline.signal((5, 1), 3);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].fence_id, 7);
        assert!(done(&own) && done(&other));
        assert!(timeline.pending.is_empty());
    }

    #[test]
    fn fence_ids_do_not_wrap_while_old_callbacks_are_pending() {
        let mut timeline = Timeline {
            next: u32::MAX as u64 - 1,
            ..Default::default()
        };
        let last = timeline.reserve().unwrap();
        assert_eq!(last, u32::MAX as u64);
        timeline.pending.insert(
            last,
            TimelineFence {
                guest: None,
                ring: (0, 0),
                signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        assert_eq!(timeline.reserve(), None);
        timeline.pending.clear();
        assert_eq!(timeline.reserve(), Some(1));
        assert_eq!(timeline.reserve(), Some(2));
    }
}
