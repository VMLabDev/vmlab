//! A virtio-gpu PCI function without VGA: 1af4:1050, class 03 80, as QEMU's
//! `virtio-gpu-pci` presents it. It is the display of the aarch64 `virt`
//! machine, which decodes no legacy VGA ports or aperture.
//!
//! The function is the modern transport of `virtio::pci` around the 2D
//! [`Gpu`], with two registers of the header as QEMU's function has them:
//! the class (display, other) with revision 1, and the subsystem
//! 1af4:1100. A guest driver of a modern-only virtio function wants a
//! subsystem ID of 0x40 or more (VIRTIO 1.2, 4.1.2.1), so the type in the
//! low byte that the transport writes would leave AAVMF's driver unbound.
//!
//! The device serves its queues on a worker thread
//! ([`crate::virtio::worker::QueueWorker`]): a 2D command copies and
//! converts pixels and does not hold the vCPU that kicked.

use std::sync::Arc;

use super::{Frame, Mode, PixelFormat};
use crate::virtio::gpu::{Cursor, DisplaySink, Gpu};
use crate::virtio::pci::VirtioPciDevice;
use crate::{BarInfo, DeviceError, GuestMemory, IrqLine, MsiSink, PciDevice, PciSnapshot};

/// The class code (display controller, other) and revision 1 (register 2).
const CLASS_REVISION: u32 = 0x0380_0001;
/// The subsystem vendor and ID (register 11).
const SUBSYSTEM: u32 = 0x1100_1af4;

/// A 2D virtio GPU as one PCI function.
pub struct VirtioGpu {
    gpu: Gpu,
    transport: VirtioPciDevice,
}

impl VirtioGpu {
    pub fn new(
        id: impl Into<String>,
        mem: GuestMemory,
        msi: Option<Arc<dyn MsiSink>>,
        intx: Option<Arc<dyn IrqLine>>,
    ) -> Result<Self, DeviceError> {
        let id = id.into();
        let gpu = Gpu::new(format!("{id}.gpu"));
        let worker = crate::virtio::worker::QueueWorker::new(Box::new(gpu.clone()))?;
        let transport = VirtioPciDevice::new(id, Box::new(worker), mem, msi, intx);
        Ok(Self { gpu, transport })
    }

    pub fn set_display_sink(&self, sink: Arc<dyn DisplaySink>) {
        self.gpu.set_display_sink(sink);
    }

    pub fn poll_scanout(&self) -> bool {
        self.gpu.poll_scanout()
    }

    /// Whether the guest shows a scanout. Until its driver sets one the
    /// screen is off: there is no VGA to fall back to.
    pub fn scanout_active(&self) -> bool {
        self.gpu.scanout_active().unwrap_or(false)
    }

    /// The scanout, or a black 640x480 frame while none is set.
    pub fn frame(&self) -> Frame {
        self.gpu.frame().unwrap_or_else(|| {
            let mode = Mode {
                width: 640,
                height: 480,
                stride: 640 * 4,
                format: PixelFormat::Xrgb8888,
            };
            Frame {
                mode,
                pixels: vec![0; (mode.stride * mode.height) as usize],
                dirty: false,
                damage: Vec::new(),
            }
        })
    }

    pub fn cursor(&self) -> Cursor {
        self.gpu.cursor()
    }

    pub fn scanout_generation(&self) -> u64 {
        self.gpu.scanout_generation()
    }

    pub fn request_size(&self, width: u32, height: u32) -> Result<(), DeviceError> {
        self.gpu.request_size(width, height)
    }
}

impl PciDevice for VirtioGpu {
    fn set_bar_address(&mut self, bar: u8, address: Option<u64>) {
        self.transport.set_bar_address(bar, address);
    }

    fn config_read(&mut self, reg: usize) -> u32 {
        match reg {
            2 => CLASS_REVISION,
            11 => SUBSYSTEM,
            _ => self.transport.config_read(reg),
        }
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        // Both registers are read-only.
        if !matches!(reg, 2 | 11) {
            self.transport.config_write(reg, offset, data);
        }
    }

    fn bars(&self) -> Vec<BarInfo> {
        self.transport.bars()
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        self.transport.bar_read(bar, offset, data);
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        self.transport.bar_write(bar, offset, data);
    }
}

impl crate::Resettable for VirtioGpu {
    fn reset(&mut self) -> Result<(), DeviceError> {
        crate::Resettable::reset(&mut self.transport)
    }
}

impl PciSnapshot for VirtioGpu {
    fn quiesce(&mut self) {
        PciSnapshot::quiesce(&mut self.transport);
    }

    fn resume_snapshot(&mut self) {
        PciSnapshot::resume_snapshot(&mut self.transport);
    }

    fn snapshot_id(&self) -> String {
        self.transport.snapshot_id()
    }

    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.transport.save_function_state()
    }

    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.transport.save_wrapped_state()
    }

    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.transport.restore_function_state(state)
    }

    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.transport.restore_wrapped_state(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> VirtioGpu {
        let mem = GuestMemory::from_ranges(&[(vm_memory::GuestAddress(0), 1 << 20)]).unwrap();
        VirtioGpu::new("gpu", mem, None, None).unwrap()
    }

    /// The header is QEMU's `virtio-gpu-pci`: 1af4:1050, class 03 80,
    /// revision 1, subsystem 1af4:1100, which AAVMF's modern virtio driver
    /// binds; and the guest cannot write the class or the subsystem.
    #[test]
    fn the_header_is_a_modern_virtio_gpu_function() {
        let mut gpu = device();
        assert_eq!(gpu.config_read(0), 0x1050_1af4);
        assert_eq!(gpu.config_read(2), 0x0380_0001);
        assert_eq!(gpu.config_read(11), 0x1100_1af4);
        gpu.config_write(11, 0, &[0; 4]);
        gpu.config_write(2, 0, &[0xff; 4]);
        assert_eq!(gpu.config_read(11), 0x1100_1af4);
        assert_eq!(gpu.config_read(2), 0x0380_0001);
        assert!(!gpu.bars().is_empty());
    }

    /// Before the guest's driver sets a scanout the screen is off, and the
    /// frame is black.
    #[test]
    fn no_scanout_is_an_inactive_black_screen() {
        let gpu = device();
        assert!(!gpu.scanout_active());
        let frame = gpu.frame();
        assert_eq!((frame.mode.width, frame.mode.height), (640, 480));
        assert!(frame.pixels.iter().all(|byte| *byte == 0));
    }
}
