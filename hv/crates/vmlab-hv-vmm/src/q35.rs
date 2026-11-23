//! Q35 host bridge, ROM aliases and programmable memory attributes.

use std::sync::Arc;

use kvm_bindings::{KVM_MEM_READONLY, kvm_userspace_memory_region};
use kvm_ioctls::VmFd;
use parking_lot::Mutex;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vmlab_hv_devices::{BarInfo, BusDevice, DeviceError, PciDevice, Snapshot};

use crate::error::VmmError;

const ALIAS: u64 = 0xc0000;
const ALIAS_SIZE: usize = 0x40000;

/// The firmware debug port (0x402): each byte the guest writes is appended to
/// the serial log, under that log's size cap.
pub struct DebugPort(crate::serial_log::CappedLog);

impl DebugPort {
    /// Opens the port's own capped handle on the serial log at `path`.
    pub fn open(path: &std::path::Path, cap: u64) -> std::io::Result<Self> {
        crate::serial_log::CappedLog::open(path, cap).map(Self)
    }
}

impl BusDevice for DebugPort {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(0xe9);
    }
    fn write(&mut self, _: u64, data: &[u8]) {
        use std::io::Write;
        if let Err(e) = self.0.write_all(data) {
            log::warn!("firmware debug log: {e}");
        }
    }
}

/// PAM0 controls F0000-FFFFF; PAM1-6 each control two 16 KiB banks.
pub fn pam_bank(index: usize) -> (u64, usize, usize, u8) {
    if index == 12 {
        (0xf0000, 0x10000, 0x90, 4)
    } else {
        (
            ALIAS + index as u64 * 0x4000,
            0x4000,
            0x91 + index / 2,
            (index % 2 * 4) as u8,
        )
    }
}

/// CPU mappings select ROM or RAM independently from write permission. Writes
/// to write-only shadow RAM exit through the read-only ROM memslot.
///
/// Device models reach 0xc0000-0xfffff through a second mapping, the device
/// view, which follows the same attributes: a device write must not land in
/// a bank the guest write-protected. A bank with its write enable set maps the
/// shadow shared, so reads and writes are the shadow's (a write-only bank
/// therefore reads the shadow and not the ROM). Any other bank maps what the
/// CPU reads there, shadow or ROM, privately: a device write goes to a
/// copy-on-write page that only the device view sees and that the next change
/// of the bank drops.
pub struct Pam {
    shadow: GuestMemoryMmap,
    rom: GuestMemoryMmap,
    /// The memfd behind `shadow` (offset 0) and `rom` (offset `ALIAS_SIZE`),
    /// so the device view can map either.
    backing: std::fs::File,
    device: GuestMemoryMmap,
    vm: Option<Arc<VmFd>>,
    first_slot: u32,
    attributes: [u8; 13],
    smram: GuestMemoryMmap,
    smram_open: bool,
    isolated_smram: bool,
    a20_enabled: bool,
}

impl Pam {
    pub fn shadow_region(&self) -> Arc<vm_memory::GuestRegionMmap> {
        Arc::new(
            vm_memory::GuestRegionMmap::with_arc(
                self.shadow.iter().next().unwrap().get_mmap(),
                GuestAddress(ALIAS),
            )
            .unwrap(),
        )
    }
    /// 0xc0000-0xfffff as device models see it.
    pub fn device_region(&self) -> Arc<vm_memory::GuestRegionMmap> {
        Arc::new(
            vm_memory::GuestRegionMmap::with_arc(
                self.device.iter().next().unwrap().get_mmap(),
                GuestAddress(ALIAS),
            )
            .unwrap(),
        )
    }
    /// The legacy SMRAM backing. It belongs in the complete memory view the
    /// snapshot and the debugger use, and never in the one devices get.
    pub fn smram_region(&self) -> Arc<vm_memory::GuestRegionMmap> {
        Arc::new(
            vm_memory::GuestRegionMmap::with_arc(
                self.smram.iter().next().unwrap().get_mmap(),
                GuestAddress(0xa0000),
            )
            .unwrap(),
        )
    }
    pub fn new(image: &[u8], vm: Option<Arc<VmFd>>, first_slot: u32) -> Result<Self, VmmError> {
        Self::build(image, vm, first_slot, false)
    }

    pub fn with_isolated_smram(
        image: &[u8],
        vm: Arc<VmFd>,
        first_slot: u32,
    ) -> Result<Self, VmmError> {
        Self::build(image, Some(vm), first_slot, true)
    }

    fn build(
        image: &[u8],
        vm: Option<Arc<VmFd>>,
        first_slot: u32,
        isolated_smram: bool,
    ) -> Result<Self, VmmError> {
        let backing = crate::memory::create_memfd().map_err(VmmError::Memory)?;
        backing
            .set_len(2 * ALIAS_SIZE as u64)
            .map_err(|e| VmmError::Memory(format!("sizing the PAM memfd: {e}")))?;
        let file_backed = |offset: u64| {
            let file = backing
                .try_clone()
                .map_err(|e| VmmError::Memory(e.to_string()))?;
            GuestMemoryMmap::from_ranges_with_files([(
                GuestAddress(ALIAS),
                ALIAS_SIZE,
                Some(vm_memory::FileOffset::new(file, offset)),
            )])
            .map_err(|e| VmmError::Memory(e.to_string()))
        };
        let shadow = file_backed(0)?;
        let rom = file_backed(ALIAS_SIZE as u64)?;
        let device = GuestMemoryMmap::from_ranges(&[(GuestAddress(ALIAS), ALIAS_SIZE)])
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        rom.write_slice(&vec![0xff; ALIAS_SIZE], GuestAddress(ALIAS))
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        let n = image.len().min(0x20000);
        rom.write_slice(&image[image.len() - n..], GuestAddress(0x100000 - n as u64))
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        let smram = GuestMemoryMmap::from_ranges(&[(GuestAddress(0xa0000), 0x20000)])
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        crate::memory::dont_dump(&shadow)?;
        crate::memory::dont_dump(&smram)?;
        let pam = Self {
            shadow,
            rom,
            backing,
            device,
            vm,
            first_slot,
            attributes: [0; 13],
            smram,
            smram_open: false,
            isolated_smram,
            a20_enabled: true,
        };
        for bank in 0..13 {
            pam.map_bank(bank, false)?;
        }
        pam.refresh_device_view()?;
        if let Some(vm) = &pam.vm
            && !isolated_smram
        {
            let region = kvm_userspace_memory_region {
                slot: (1 << 16) | (first_slot + 13),
                guest_phys_addr: 0xa0000,
                memory_size: 0x20000,
                userspace_addr: pam
                    .smram
                    .get_host_address(GuestAddress(0xa0000))
                    .map_err(|e| VmmError::Memory(e.to_string()))?
                    as u64,
                flags: 0,
            };
            // SAFETY: SMRAM backing remains owned by Pam for the VM lifetime.
            unsafe { vm.set_user_memory_region(region) }
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(SMRAM)", e))?;
        }
        Ok(pam)
    }

    /// Map one bank of the device view from its attributes.
    fn map_device_bank(&self, index: usize) -> Result<(), VmmError> {
        use std::os::fd::AsRawFd;
        let (base, size, _, _) = pam_bank(index);
        let attr = self.attributes[index];
        let host = self
            .device
            .get_host_address(GuestAddress(base))
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        let within = base - ALIAS;
        let (sharing, offset) = if attr & 2 != 0 {
            (libc::MAP_SHARED, within)
        } else if attr & 1 != 0 {
            (libc::MAP_PRIVATE, within)
        } else {
            (libc::MAP_PRIVATE, ALIAS_SIZE as u64 + within)
        };
        // SAFETY: the target is one whole, page-aligned bank inside the
        // mapping `self.device` owns, and MAP_FIXED replaces those pages
        // atomically: a device thread copying there reads or writes the old
        // pages or the new ones. The memfd covers the offset and outlives
        // the mapping, which `self.device` unmaps when it drops.
        let mapped = unsafe {
            libc::mmap(
                host.cast(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                sharing | libc::MAP_FIXED | libc::MAP_NORESERVE,
                self.backing.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(VmmError::Memory(format!(
                "mapping PAM bank {index} for devices: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: the range is the mapping made above. A new mapping does
        // not inherit the advice of the one it replaced.
        if unsafe { libc::madvise(mapped, size, libc::MADV_DONTDUMP) } != 0 {
            return Err(VmmError::Memory(format!(
                "MADV_DONTDUMP: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Map every bank of the device view again. A bank that is not writable
    /// holds private pages, which neither see a shadow replaced under them
    /// (a restore, a reset) nor forget what a device wrote to them.
    fn refresh_device_view(&self) -> Result<(), VmmError> {
        (0..13).try_for_each(|bank| self.map_device_bank(bank))
    }

    fn map_bank(&self, index: usize, replace: bool) -> Result<(), VmmError> {
        let Some(vm) = &self.vm else { return Ok(()) };
        let (base, size, _, _) = pam_bank(index);
        let attr = self.attributes[index];
        let backing = if attr & 1 != 0 {
            &self.shadow
        } else {
            &self.rom
        };
        let ptr = backing
            .get_host_address(GuestAddress(base))
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        let region = kvm_userspace_memory_region {
            slot: self.first_slot + index as u32,
            guest_phys_addr: base,
            memory_size: size as u64,
            userspace_addr: ptr as u64,
            flags: if attr == 3 { 0 } else { KVM_MEM_READONLY },
        };
        for address_space in [0, 1 << 16] {
            let region = kvm_userspace_memory_region {
                slot: region.slot | address_space,
                ..region
            };
            if replace {
                // KVM does not permit toggling KVM_MEM_READONLY on a live slot.
                // Remove the old mapping before installing its new permissions.
                let removed = kvm_userspace_memory_region {
                    memory_size: 0,
                    ..region
                };
                // SAFETY: a zero-size slot removes the corresponding mapping;
                // both old and new backing allocations remain alive throughout.
                unsafe { vm.set_user_memory_region(removed) }
                    .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(remove PAM)", e))?;
            }
            // SAFETY: both backing mmaps outlive these slots and each bank is
            // page aligned, disjoint from RAM and the other PAM banks.
            unsafe { vm.set_user_memory_region(region) }
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(PAM)", e))?;
        }
        if !self.a20_enabled {
            let alias = kvm_userspace_memory_region {
                slot: self.first_slot + 40 + index as u32,
                guest_phys_addr: base + (1 << 20),
                ..region
            };
            if replace {
                // SAFETY: removing the alias preserves both retained backings.
                unsafe {
                    vm.set_user_memory_region(kvm_userspace_memory_region {
                        memory_size: 0,
                        ..alias
                    })
                }
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(remove A20 PAM)", e))?;
            }
            // SAFETY: the alias has identical lifetime and permissions to PAM.
            unsafe { vm.set_user_memory_region(alias) }
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(A20 PAM)", e))?;
        }
        Ok(())
    }

    pub fn set_a20(&mut self, enabled: bool) -> Result<(), VmmError> {
        if enabled == self.a20_enabled {
            return Ok(());
        }
        self.a20_enabled = enabled;
        if enabled {
            if let Some(vm) = &self.vm {
                for index in 0..13 {
                    // SAFETY: removing slots leaves their backing owned by Pam.
                    unsafe {
                        vm.set_user_memory_region(kvm_userspace_memory_region {
                            slot: self.first_slot + 40 + index,
                            memory_size: 0,
                            ..Default::default()
                        })
                    }
                    .map_err(|e| {
                        VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(remove A20 PAM)", e)
                    })?;
                }
            }
        } else {
            for index in 0..13 {
                self.map_bank(index, false)?;
            }
        }
        Ok(())
    }

    fn update(&mut self, config: &[u8; 4096]) -> Result<(), VmmError> {
        // D_OPEN counts only while D_LCK is clear. The register write path
        // keeps the two apart, and a restored state is held to the same rule.
        let open = config[0x9d] & 0x50 == 0x40;
        if !self.isolated_smram && open != self.smram_open {
            if let Some(vm) = &self.vm {
                let region = kvm_userspace_memory_region {
                    slot: self.first_slot + 13,
                    guest_phys_addr: 0xa0000,
                    memory_size: if open { 0x20000 } else { 0 },
                    userspace_addr: self
                        .smram
                        .get_host_address(GuestAddress(0xa0000))
                        .map_err(|e| VmmError::Memory(e.to_string()))?
                        as u64,
                    flags: 0,
                };
                // SAFETY: opening or closing the non-SMM alias does not
                // change the lifetime of the permanent SMM backing.
                unsafe { vm.set_user_memory_region(region) }
                    .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(SMRAM alias)", e))?;
            }
            self.smram_open = open;
        }
        for i in 0..13 {
            let (_, _, reg, shift) = pam_bank(i);
            let attr = (config[reg] >> shift) & 3;
            if attr != self.attributes[i] {
                self.attributes[i] = attr;
                self.map_bank(i, true)?;
                self.map_device_bank(i)?;
            }
        }
        Ok(())
    }
}

impl BusDevice for Pam {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let addr = ALIAS + offset + i as u64;
            let bank = if addr >= 0xf0000 {
                12
            } else {
                ((addr - ALIAS) / 0x4000) as usize
            };
            *byte = 0xff;
            if addr >= 0x100000 {
                continue;
            }
            let memory = if self.attributes[bank] & 1 != 0 {
                &self.shadow
            } else {
                &self.rom
            };
            let _ = memory.read_slice(std::slice::from_mut(byte), GuestAddress(addr));
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, byte) in data.iter().enumerate() {
            let addr = ALIAS + offset + i as u64;
            if addr >= 0x100000 {
                break;
            }
            let bank = if addr >= 0xf0000 {
                12
            } else {
                ((addr - ALIAS) / 0x4000) as usize
            };
            if self.attributes[bank] & 2 != 0 {
                let _ = self
                    .shadow
                    .write_slice(std::slice::from_ref(byte), GuestAddress(addr));
            }
        }
    }
}

/// Put the firmware memory below 1 MiB into guest memory. The complete view
/// (`ram.mem`: snapshots, the debugger) gets the PAM shadow, and the legacy
/// SMRAM backing unless SMRAM is isolated. The device view (`ram.device_mem`)
/// gets `Pam`'s device mapping of the shadow and no SMRAM at all: outside SMM
/// 0xa0000-0xbffff is the VGA window, which is not memory a device can reach,
/// so a device access there fails as one to an unmapped address does.
///
/// Returns the complete view without SMRAM, for a passed-through device.
pub(crate) fn attach_firmware_memory(
    ram: &mut crate::memory::GuestRam,
    pam: &Pam,
    secure_smm: bool,
) -> Result<GuestMemoryMmap, VmmError> {
    ram.device_mem = ram
        .device_mem
        .insert_region(pam.device_region())
        .map_err(|e| VmmError::Memory(e.to_string()))?;
    ram.mem = ram
        .mem
        .insert_region(pam.shadow_region())
        .map_err(|e| VmmError::Memory(e.to_string()))?;
    let without_smram = ram.mem.clone();
    if !secure_smm {
        ram.mem = ram
            .mem
            .insert_region(pam.smram_region())
            .map_err(|e| VmmError::Memory(e.to_string()))?;
    }
    Ok(without_smram)
}

struct SmmWiring {
    control: Arc<Mutex<crate::smm::SmramControl>>,
    memory: Arc<Mutex<crate::smm::Smram>>,
}

pub struct Mch {
    config: [u8; 4096],
    pub pam: Arc<Mutex<Pam>>,
    /// Mapping errors cannot be returned through PciDevice; the run loop checks
    /// this latch immediately after configuration writes.
    pub error: Option<String>,
    smm: Option<SmmWiring>,
    pc: bool,
}

impl Mch {
    pub fn new(pam: Arc<Mutex<Pam>>) -> Self {
        let mut config = [0; 4096];
        config[..4].copy_from_slice(&0x29c0_8086u32.to_le_bytes());
        config[0xb] = 0x06;
        config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
        config[0x50..0x52].copy_from_slice(&64u16.to_le_bytes());
        config[0x9d] = 0x02;
        config[0x9e] = 0x38;
        Self {
            config,
            pam,
            error: None,
            smm: None,
            pc: false,
        }
    }

    pub fn i440fx(pam: Arc<Mutex<Pam>>, ram_bytes: u64) -> Self {
        let mut bridge = Self::new(pam);
        bridge.config.fill(0);
        bridge.config[..4].copy_from_slice(&0x1237_8086u32.to_le_bytes());
        bridge.config[8] = 2;
        bridge.config[0xb] = 6;
        bridge.config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
        bridge.config[0x72] = 2;
        bridge.config[0x57] = (ram_bytes / (8 << 20)).min(255) as u8;
        bridge.pc = true;
        bridge
    }

    fn update_pam(&mut self) -> Result<(), VmmError> {
        if self.pc {
            let mut config = [0; 4096];
            config[0x90..0x97].copy_from_slice(&self.config[0x59..0x60]);
            config[0x9d] = self.config[0x72];
            self.pam.lock().update(&config)
        } else {
            self.pam.lock().update(&self.config)
        }
    }

    pub fn with_smm(
        mut self,
        control: Arc<Mutex<crate::smm::SmramControl>>,
        memory: Arc<Mutex<crate::smm::Smram>>,
    ) -> Self {
        self.smm = Some(SmmWiring { control, memory });
        self
    }

    /// PCIEXBAR enable, base and length (256/128/64 MiB encodings).
    pub fn ecam_window(&self) -> Option<(u64, u64)> {
        if self.pc {
            return None;
        }
        let value = u64::from_le_bytes(self.config[0x60..0x68].try_into().unwrap());
        if value & 1 == 0 {
            return None;
        }
        let size = match (value >> 1) & 3 {
            0 => 1 << 28,
            1 => 1 << 27,
            2 => 1 << 26,
            _ => return None,
        };
        Some((value & 0x0000_000f_ffff_ffff & !(size - 1), size))
    }

    /// Read diagnostic registers without waiting for SMRAM configuration locks.
    pub(crate) fn diagnostic_smram(&self) -> Option<[u32; 3]> {
        let control = match &self.smm {
            Some(smm) => Some(smm.control.try_lock()?),
            None => None,
        };
        Some([0x50, 0x9c, 0xac].map(|offset| {
            u32::from_le_bytes(std::array::from_fn(|i| {
                control
                    .as_ref()
                    .and_then(|c| c.read_config(offset + i))
                    .unwrap_or(self.config[offset + i])
            }))
        }))
    }
}

impl PciDevice for Mch {
    fn config_read(&mut self, reg: usize) -> u32 {
        if reg >= 64 {
            return u32::MAX;
        }
        if let Some(smm) = &self.smm {
            let control = smm.control.lock();
            let bytes = std::array::from_fn(|i| {
                control
                    .read_config(reg * 4 + i)
                    .unwrap_or(self.config[reg * 4 + i])
            });
            return u32::from_le_bytes(bytes);
        }
        self.config
            .get(reg * 4..reg * 4 + 4)
            .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
            .unwrap_or(u32::MAX)
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let start = reg * 4 + offset as usize;
        for (index, byte) in data.iter().enumerate() {
            let pos = start + index;
            if let Some(smm) = &self.smm {
                let mut control = smm.control.lock();
                if control.write_config(pos, *byte) {
                    let mut memory = smm.memory.lock();
                    if let Err(e) = memory
                        .set_region_open(0, control.default_smbase_open())
                        .and_then(|()| memory.set_region_open(1, control.open()))
                        .and_then(|()| memory.set_region_open(2, control.tseg_open()))
                    {
                        self.error = Some(e.to_string());
                    }
                    continue;
                }
            }
            let mask = if self.pc {
                match pos {
                    4 => 0x07,
                    5 => 0x05,
                    0x0c | 0x3c | 0x40..=0xff => 0xff,
                    _ => 0,
                }
            } else {
                match pos {
                    4 => 0x07,
                    5 => 0x05,
                    0x3c => 0xff,
                    0x52 => 0xff,
                    0x60 => 0x07,
                    0x63 => 0xfc,
                    0x64 => 0x0f,
                    0x90 => 0x30,
                    0x91..=0x96 => 0x33,
                    0x9d => 0x7f,
                    0x9e => 0x07,
                    0xa0..=0xb1 => 0xff,
                    _ => 0,
                }
            };
            // D_LCK (bit 4 of SMRAMC) makes SMRAMC, and on q35 ESMRAMC,
            // read-only until a reset, and setting it clears D_OPEN. QEMU's
            // i440fx does not model the lock. The 82441FX does, and this
            // follows the chip.
            let smramc = if self.pc { 0x72 } else { 0x9d };
            let locked = self.config[smramc] & 0x10 != 0;
            if locked && (pos == smramc || (!self.pc && pos == 0x9e)) {
                continue;
            }
            if pos < self.config.len() {
                self.config[pos] = (self.config[pos] & !mask) | (byte & mask);
                if pos == smramc && self.config[pos] & 0x10 != 0 {
                    self.config[pos] &= !0x40;
                }
            }
        }
        if let Err(e) = self.update_pam() {
            self.error = Some(e.to_string());
        }
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![]
    }
    fn bar_read(&mut self, _: u8, _: u64, data: &mut [u8]) {
        data.fill(0xff);
    }
    fn bar_write(&mut self, _: u8, _: u64, _: &[u8]) {}
}

impl Snapshot for Mch {
    fn state_id(&self) -> String {
        if self.pc { "i440fx" } else { "q35-mch" }.into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = self.config.to_vec();
        let pam = self.pam.lock();
        let mut shadow = vec![0; ALIAS_SIZE];
        pam.shadow
            .read_slice(&mut shadow, GuestAddress(ALIAS))
            .map_err(|e| DeviceError::Memory(e.to_string()))?;
        bytes.extend(shadow);
        Ok(bytes)
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        if bytes.len() != 4096 + ALIAS_SIZE {
            return Err(DeviceError::State("invalid MCH state length".into()));
        }
        self.config.copy_from_slice(&bytes[..4096]);
        let pam = self.pam.lock();
        pam.shadow
            .write_slice(&bytes[4096..], GuestAddress(ALIAS))
            .map_err(|e| DeviceError::Memory(e.to_string()))?;
        drop(pam);
        self.update_pam()
            .map_err(|e| DeviceError::State(e.to_string()))?;
        // The shadow changed under the private pages of the device view.
        self.pam
            .lock()
            .refresh_device_view()
            .map_err(|e| DeviceError::State(e.to_string()))
    }
}

/// Register the immutable high ROM independently of RAM and PAM aliases.
/// The returned mmap must be retained for the lifetime of the VM.
pub fn map_bios(vm: &VmFd, image: &[u8], slot: u32) -> Result<GuestMemoryMmap, VmmError> {
    if image.len() < 0x10000 || !image.len().is_multiple_of(0x10000) || image.len() > 0x400000 {
        return Err(VmmError::InvalidMachine(
            "BIOS size must be a multiple of 64 KiB in 64 KiB..=4 MiB".into(),
        ));
    }
    let base = (1u64 << 32) - image.len() as u64;
    map_rom(vm, image, slot, base)
}

/// Executable immutable CODE backing; VARS remains on the flash MMIO path.
pub fn map_pflash_code(vm: &VmFd, image: &[u8], slot: u32) -> Result<GuestMemoryMmap, VmmError> {
    if image.len() != vmlab_hv_devices::pflash::CODE_SIZE {
        return Err(VmmError::InvalidMachine("invalid OVMF CODE length".into()));
    }
    map_rom(vm, image, slot, vmlab_hv_devices::pflash::CODE_BASE)
}

fn map_rom(vm: &VmFd, image: &[u8], slot: u32, base: u64) -> Result<GuestMemoryMmap, VmmError> {
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(base), image.len())])
        .map_err(|e| VmmError::Memory(e.to_string()))?;
    memory
        .write_slice(image, GuestAddress(base))
        .map_err(|e| VmmError::Memory(e.to_string()))?;
    let region = kvm_userspace_memory_region {
        slot,
        guest_phys_addr: base,
        memory_size: image.len() as u64,
        userspace_addr: memory.iter().next().unwrap().as_ptr() as u64,
        flags: KVM_MEM_READONLY,
    };
    // SAFETY: the returned mmap owns the page-aligned, populated ROM backing.
    for address_space in [0, 1 << 16] {
        let region = kvm_userspace_memory_region {
            slot: region.slot | address_space,
            ..region
        };
        // SAFETY: both address spaces refer to the returned immutable mmap.
        unsafe { vm.set_user_memory_region(region) }
            .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(BIOS)", e))?;
    }
    Ok(memory)
}

/// Captured _PRT: slots 0..24 rotate E-H, slots 25..29 and 31 use A-D,
/// and slot 30 uses E-H. Pin is PCI's one-based INTA..INTD.
pub fn pirq(slot: u8, pin: u8) -> Option<u8> {
    if slot >= 32 || !(1..=4).contains(&pin) {
        return None;
    }
    Some(if slot < 25 {
        4 + ((slot + pin - 1) & 3)
    } else if slot == 30 {
        4 + pin - 1
    } else {
        pin - 1
    })
}

pub fn intx_gsi(slot: u8, pin: u8) -> Option<u32> {
    pirq(slot, pin).map(|p| 16 + u32::from(p))
}

/// Level aggregation for shared PIRQ inputs. IOAPIC routing is fixed at
/// 16..23; LPC routing bytes independently select the legacy PIC input.
pub struct PirqRouter {
    pc: bool,
    outputs: [Arc<dyn vmlab_hv_devices::IrqLine>; 24],
    routes: [u8; 8],
    sources: std::collections::BTreeMap<(u8, u8, u8), bool>,
    levels: [bool; 24],
}

impl PirqRouter {
    pub fn new(outputs: [Arc<dyn vmlab_hv_devices::IrqLine>; 24]) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            pc: false,
            outputs,
            routes: [0x80; 8],
            sources: Default::default(),
            levels: [false; 24],
        }))
    }

    pub fn pc(outputs: [Arc<dyn vmlab_hv_devices::IrqLine>; 24]) -> Arc<Mutex<Self>> {
        let router = Self::new(outputs);
        router.lock().pc = true;
        router
    }

    /// Called by the LPC when PIRQ A-D (0x60..63) or E-H (0x68..6b) change.
    pub fn set_route(&mut self, pirq: usize, value: u8) {
        if let Some(route) = self.routes.get_mut(pirq) {
            *route = value;
            self.update();
        }
    }

    pub fn line(
        router: &Arc<Mutex<Self>>,
        address: vmlab_hv_proto::machine::PciAddress,
        pin: u8,
    ) -> Result<Arc<dyn vmlab_hv_devices::IrqLine>, String> {
        if address.bus != 0 || address.function >= 8 || pirq(address.device, pin).is_none() {
            return Err("invalid INTx source address or pin".into());
        }
        Ok(Arc::new(PirqLine {
            router: router.clone(),
            source: (address.device, address.function, pin),
        }))
    }

    fn update(&mut self) {
        let mut levels = [false; 24];
        for (&(slot, _, pin), &active) in &self.sources {
            if !active {
                continue;
            }
            let p = if self.pc {
                ((slot.wrapping_add(pin).wrapping_sub(2)) & 3) as usize
            } else {
                pirq(slot, pin).unwrap() as usize
            };
            if !self.pc {
                levels[16 + p] = true;
            }
            let route = self.routes[p];
            if route & 0x80 == 0 && matches!(route & 15, 3..=7 | 9..=12 | 14..=15) {
                levels[(route & 15) as usize] = true;
            }
        }
        for (i, level) in levels.into_iter().enumerate() {
            if level != self.levels[i] {
                self.outputs[i].set_level(level);
                self.levels[i] = level;
            }
        }
    }
}

struct PirqLine {
    router: Arc<Mutex<PirqRouter>>,
    source: (u8, u8, u8),
}

impl vmlab_hv_devices::IrqLine for PirqLine {
    fn set_level(&self, high: bool) {
        let mut router = self.router.lock();
        router.sources.insert(self.source, high);
        router.update();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mch() -> Mch {
        let pam = Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap();
        Mch::new(Arc::new(Mutex::new(pam)))
    }

    /// A `pc` machine's memory and host bridge as the builder wires them,
    /// with the ISA DMA controller on the device view. No KVM.
    fn pc_with_isa_dma() -> (
        crate::memory::GuestRam,
        Mch,
        vmlab_hv_devices::chipset::isa::IsaDevices,
    ) {
        let layout = crate::layout::MemLayout::pc(16).unwrap();
        let mut ram = crate::memory::GuestRam::anonymous(layout.clone()).unwrap();
        let pam = Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap();
        attach_firmware_memory(&mut ram, &pam, false).unwrap();
        let mch = Mch::i440fx(Arc::new(Mutex::new(pam)), layout.ram);
        let mut isa = vmlab_hv_devices::chipset::isa::IsaDevices::new();
        isa.set_memory(ram.device_mem.clone());
        (ram, mch, isa)
    }

    /// Program DMA channel 2 for a device-to-memory transfer of `count`
    /// bytes at `page:address`, as a floppy READ does, and move `data`.
    fn floppy_dma_read(
        isa: &mut vmlab_hv_devices::chipset::isa::IsaDevices,
        page: u8,
        address: u16,
        data: &mut [u8],
    ) {
        let count = (data.len() - 1) as u16;
        isa.io_write(0x0a, 4 | 2);
        isa.io_write(0x0b, 0x46);
        isa.io_write(0x0c, 0);
        isa.io_write(4, address as u8);
        isa.io_write(4, (address >> 8) as u8);
        isa.io_write(0x0c, 0);
        isa.io_write(5, count as u8);
        isa.io_write(5, (count >> 8) as u8);
        isa.io_write(0x81, page);
        isa.io_write(0x0a, 2);
        let progress = isa.dma_transfer(2, data);
        assert_eq!(progress.bytes, data.len());
        assert!(progress.terminal_count);
    }

    fn backing(memory: &GuestMemoryMmap, address: u64) -> [u8; 512] {
        let mut bytes = [0; 512];
        memory
            .read_slice(&mut bytes, GuestAddress(address))
            .unwrap();
        bytes
    }

    #[test]
    fn isa_dma_does_not_write_closed_smram() {
        let (_ram, mch, mut isa) = pc_with_isa_dma();
        assert!(!mch.pam.lock().smram_open);
        floppy_dma_read(&mut isa, 0x0a, 0, &mut [0xd7; 512]);
        assert_eq!(
            backing(&mch.pam.lock().smram, 0xa0000),
            [0; 512],
            "a DMA write to 0xa0000 reached SMRAM while it was closed"
        );
    }

    #[test]
    fn isa_dma_does_not_write_read_only_shadow_ram() {
        let (_ram, mut mch, mut isa) = pc_with_isa_dma();
        // PAM0 on pc is 0x59 bits 4-5: bank 12, 0xf0000-0xfffff.
        mch.config_write(0x58 / 4, 1, &[0x30]);
        floppy_dma_read(&mut isa, 0x0f, 0, &mut [0x11; 512]);
        assert_eq!(
            backing(&mch.pam.lock().shadow, 0xf0000),
            [0x11; 512],
            "DMA reaches a write-enabled bank"
        );
        mch.config_write(0x58 / 4, 1, &[0x10]);
        floppy_dma_read(&mut isa, 0x0f, 0, &mut [0xd7; 512]);
        assert_eq!(
            backing(&mch.pam.lock().shadow, 0xf0000),
            [0x11; 512],
            "a DMA write reached a read-only bank"
        );
        // Bank 0, 0xc0000, never enabled: it reads the ROM and takes no write.
        floppy_dma_read(&mut isa, 0x0c, 0, &mut [0xd7; 512]);
        assert_eq!(backing(&mch.pam.lock().shadow, 0xc0000), [0; 512]);
    }

    #[test]
    fn device_view_of_shadow_ram_follows_the_pam_attributes() {
        let (ram, mut mch, _isa) = pc_with_isa_dma();
        let device = ram.device_mem.clone();
        assert_eq!(backing(&device, 0xf0000), [0x5a; 512], "ROM at reset");
        mch.config_write(0x58 / 4, 1, &[0x30]);
        device
            .write_slice(&[0x22; 512], GuestAddress(0xf0000))
            .unwrap();
        assert_eq!(backing(&mch.pam.lock().shadow, 0xf0000), [0x22; 512]);
        mch.config_write(0x58 / 4, 1, &[0x10]);
        assert_eq!(backing(&device, 0xf0000), [0x22; 512], "read-only RAM");
        mch.config_write(0x58 / 4, 1, &[0]);
        assert_eq!(backing(&device, 0xf0000), [0x5a; 512], "ROM again");
        // A restore replaces the shadow under a read-only bank.
        mch.config_write(0x58 / 4, 1, &[0x10]);
        let state = mch.save_state().unwrap();
        mch.config_write(0x58 / 4, 1, &[0x30]);
        mch.pam.lock().write(0x30000, &[0x33; 512]);
        mch.config_write(0x58 / 4, 1, &[0x10]);
        assert_eq!(backing(&device, 0xf0000), [0x33; 512]);
        mch.restore_state(&state).unwrap();
        assert_eq!(backing(&device, 0xf0000), [0x22; 512]);
    }

    #[test]
    fn smram_lock_holds_until_reset() {
        for pc in [false, true] {
            let pam = Arc::new(Mutex::new(Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap()));
            let mut mch = if pc {
                Mch::i440fx(pam.clone(), 128 << 20)
            } else {
                Mch::new(pam.clone())
            };
            let reset = mch.save_state().unwrap();
            let (reg, offset, at) = if pc {
                (0x70 / 4, 2, 0x72)
            } else {
                (0x9c / 4, 1, 0x9d)
            };
            mch.config_write(reg, offset, &[0x4a]);
            assert!(pam.lock().smram_open, "pc={pc}: open before the lock");
            mch.config_write(reg, offset, &[0x1a]);
            assert_eq!(mch.config[at] & 0x50, 0x10, "pc={pc}: lock closes");
            assert!(!pam.lock().smram_open, "pc={pc}");
            for value in [0x4a, 0x0a, 0x5a, 0xff, 0] {
                mch.config_write(reg, offset, &[value]);
                assert_eq!(
                    mch.config[at] & 0x50,
                    0x10,
                    "pc={pc}: D_LCK set and D_OPEN clear after writing {value:#x}"
                );
                assert!(!pam.lock().smram_open, "pc={pc}: {value:#x}");
            }
            if !pc {
                let esmramc = mch.config[0x9e];
                mch.config_write(0x9c / 4, 2, &[!esmramc]);
                assert_eq!(mch.config[0x9e], esmramc, "ESMRAMC is locked too");
            }
            // A machine reset restores the power-on state, which unlocks.
            mch.restore_state(&reset).unwrap();
            mch.config_write(reg, offset, &[0x4a]);
            assert!(pam.lock().smram_open, "pc={pc}: reset unlocks");
        }
    }

    #[test]
    fn restored_smram_lock_keeps_smram_closed() {
        let pam = Arc::new(Mutex::new(Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap()));
        let mut mch = Mch::new(pam.clone());
        let mut state = mch.save_state().unwrap();
        state[0x9d] = 0x5a;
        mch.restore_state(&state).unwrap();
        assert!(!pam.lock().smram_open);
    }

    fn captured_i440fx_config() -> Vec<u8> {
        let capture: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/hwref/pc-chipset-reset.json"))
                .unwrap();
        let hex = capture["chipset_reset_config_hex"]["00.0"]
            .as_str()
            .unwrap();
        decode_config(hex)
    }

    fn decode_config(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len(), 512);
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|byte| u8::from_str_radix(std::str::from_utf8(byte).unwrap(), 16).unwrap())
            .collect()
    }

    fn assert_i440fx_config(bridge: &mut Mch, expected: &[u8]) {
        for (reg, bytes) in expected.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(
                bridge.config_read(reg),
                u32::from_le_bytes(*bytes),
                "i440fx configuration offset {:#04x}",
                reg * 4
            );
        }
    }

    #[test]
    fn i440fx_complete_reset_config_matches_qtest_and_restores() {
        let pam = Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap();
        let mut bridge = Mch::i440fx(Arc::new(Mutex::new(pam)), 128 << 20);
        let expected = captured_i440fx_config();
        assert_i440fx_config(&mut bridge, &expected);
        let reset_state = bridge.save_state().unwrap();
        for reg in 0..64 {
            bridge.config_write(reg, 0, &[0xff; 4]);
        }
        assert_ne!(bridge.config[..256], expected);
        assert!(bridge.error.is_none());
        bridge.restore_state(&reset_state).unwrap();
        assert_i440fx_config(&mut bridge, &expected);
        assert!(bridge.error.is_none());
        assert_eq!(bridge.ecam_window(), None);
    }

    #[test]
    fn i440fx_complete_config_write_replay_matches_qtest() {
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/hwref/pc-chipset-write-masks.json"
        ))
        .unwrap();
        let pam = Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap();
        let mut bridge = Mch::i440fx(Arc::new(Mutex::new(pam)), 128 << 20);
        let mut phases = 0;
        let mut differences = Vec::new();
        for phase in capture["chipset_config_write_replay"].as_array().unwrap() {
            if phase["bdf"] != "00.0" {
                continue;
            }
            let value = u32::try_from(phase["written_dword"].as_u64().unwrap()).unwrap();
            for reg in 0..64 {
                bridge.config_write(reg, 0, &value.to_le_bytes());
            }
            let mut expected = decode_config(phase["config_hex"].as_str().unwrap());
            // QEMU keeps every write to SMRAMC. Here the first phase sets
            // D_LCK, which clears D_OPEN and holds the byte (card #611).
            expected[0x72] = 0xbf;
            for (offset, expected) in expected.iter().enumerate() {
                let actual = (bridge.config_read(offset / 4) >> (offset % 4 * 8)) as u8;
                if actual != *expected {
                    differences.push(format!(
                        "written {value:08x}, offset {offset:02x}: {actual:02x} != {expected:02x}"
                    ));
                }
            }
            assert!(bridge.error.is_none());
            phases += 1;
        }
        assert_eq!(phases, 2);
        assert!(differences.is_empty(), "{}", differences.join("\n"));
    }

    #[test]
    fn i440fx_has_legacy_pam_offsets_and_writable_smram() {
        let pam = Pam::new(&vec![0x5a; 0x40000], None, 0).unwrap();
        let mut bridge = Mch::i440fx(Arc::new(Mutex::new(pam)), 256 << 20);
        assert_eq!(bridge.config_read(0), 0x1237_8086);
        assert_eq!(bridge.config_read(1), 0);
        assert_eq!(bridge.config_read(2), 0x0600_0002);
        assert_eq!(bridge.config[0x57], 0x20);
        bridge.config_write(0x58 / 4, 1, &[0x30]);
        bridge.pam.lock().write(0x30000, &[0x42]);
        let mut byte = [0];
        bridge.pam.lock().read(0x30000, &mut byte);
        assert_eq!(byte, [0x42]);
        let state = bridge.save_state().unwrap();
        bridge.config_write(0x70 / 4, 2, &[0x48]);
        assert_eq!(bridge.config[0x72], 0x48);
        assert!(bridge.pam.lock().smram_open);
        bridge.config_write(0x70 / 4, 2, &[0x40]);
        assert_eq!(bridge.config[0x72], 0x40);
        assert!(bridge.pam.lock().smram_open);
        bridge.restore_state(&state).unwrap();
        assert_eq!(bridge.config[0x72], 2);
        assert!(!bridge.pam.lock().smram_open);
        assert_eq!(bridge.ecam_window(), None);
    }

    #[test]
    fn ovmf_can_disable_integrated_graphics_in_ggc() {
        let mut mch = mch();
        mch.config_write(0x50 / 4, 2, &[2]);
        assert_eq!(mch.config_read(0x50 / 4) >> 16 & 0xff, 2);
    }

    #[test]
    fn host_bridge_pci_command_reset_mask_and_interrupt_line_match_qemu() {
        let mut mch = mch();
        assert_eq!(mch.config_read(1) & 0xffff, 0);
        mch.config_write(1, 0, &[0xff; 2]);
        assert_eq!(mch.config_read(1) & 0xffff, 0x507);
        mch.config_write(0x3c / 4, 0, &[0xff]);
        assert_eq!(mch.config_read(0x3c / 4) & 0xff, 0xff);
    }

    #[test]
    fn identity_memory_bounds_and_read_only_registers() {
        let mut mch = mch();
        assert_eq!(mch.config_read(0), 0x29c0_8086);
        assert_eq!(mch.config_read(2), 0x0600_0000);
        assert_eq!(mch.config_read(0x2c / 4), 0x1100_1af4);
        assert_eq!(mch.config_read(0xb0 / 4), 0);
        assert_eq!(mch.config_read(0xa0 / 4), 0);
        mch.config_write(0xb0 / 4, 0, &0x8000u16.to_le_bytes());
        assert_eq!(mch.config_read(0xb0 / 4), 0x8000);
        mch.config_write(0, 0, &[0; 4]);
        assert_eq!(mch.config_read(0), 0x29c0_8086);
        assert_eq!(mch.config[0x9d], 2);
        assert_eq!(mch.config[0x9e], 0x38);
    }

    #[test]
    fn pciexbar_tracks_firmware_programming_and_length() {
        let mut mch = mch();
        assert_eq!(mch.ecam_window(), None);
        for base in [0xb000_0000u32, 0xe000_0000] {
            mch.config_write(0x60 / 4, 0, &(base | 1).to_le_bytes());
            assert_eq!(mch.ecam_window(), Some((base as u64, 0x1000_0000)));
        }
        mch.config_write(0x60 / 4, 0, &0xe800_0003u32.to_le_bytes());
        assert_eq!(mch.ecam_window(), Some((0xe800_0000, 0x800_0000)));
        mch.config_write(0x60 / 4, 0, &[0]);
        assert_eq!(mch.ecam_window(), None);
    }

    #[test]
    fn pam_read_write_selection_and_snapshot() {
        let mut mch = mch();
        let mut byte = [0];
        mch.pam.lock().read(0x30000, &mut byte);
        assert_eq!(byte, [0x5a]);
        mch.pam.lock().write(0x30000, &[0x11]);
        mch.config_write(0x90 / 4, 0, &[0x20]);
        mch.pam.lock().write(0x30000, &[0x42]);
        mch.pam.lock().read(0x30000, &mut byte);
        assert_eq!(byte, [0x5a], "write-only shadow retains ROM reads");
        mch.config_write(0x90 / 4, 0, &[0x10]);
        mch.pam.lock().write(0x30000, &[0xff]);
        mch.pam.lock().read(0x30000, &mut byte);
        assert_eq!(byte, [0x42], "read-only shadow rejects writes");
        let state = mch.save_state().unwrap();
        mch.config_write(0x90 / 4, 0, &[0x30]);
        mch.pam.lock().write(0x30000, &[0xff]);
        mch.restore_state(&state).unwrap();
        mch.pam.lock().read(0x30000, &mut byte);
        assert_eq!(byte, [0x42]);
        assert!(mch.restore_state(&state[..20]).is_err());
    }

    #[test]
    fn pam_banks_cover_the_aperture_without_overlap() {
        let mut end = ALIAS;
        for i in 0..13 {
            let (base, size, _, _) = pam_bank(i);
            assert_eq!(base, end);
            end += size as u64;
        }
        assert_eq!(end, 0x100000);
    }

    #[test]
    fn routing_matches_all_captured_prt_entries() {
        let first = [
            20, 21, 22, 23, 20, 21, 22, 23, 20, 21, 22, 23, 20, 21, 22, 23, 20, 21, 22, 23, 20, 21,
            22, 23, 20, 16, 16, 16, 16, 16, 20, 16,
        ];
        for slot in 0..32 {
            for pin in 1..=4 {
                let base = if first[slot as usize] >= 20 { 20 } else { 16 };
                assert_eq!(
                    intx_gsi(slot, pin),
                    Some(base + (first[slot as usize] - base + pin as u32 - 1) % 4)
                );
            }
        }
        assert_eq!(pirq(32, 1), None);
        assert_eq!(pirq(1, 0), None);
    }

    #[test]
    fn shared_pirq_levels_and_lpc_route_changes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Line(AtomicBool);
        impl vmlab_hv_devices::IrqLine for Line {
            fn set_level(&self, high: bool) {
                self.0.store(high, Ordering::SeqCst);
            }
        }
        let lines: [Arc<Line>; 24] =
            std::array::from_fn(|_| Arc::new(Line(AtomicBool::new(false))));
        let router = PirqRouter::new(std::array::from_fn(|i| {
            lines[i].clone() as Arc<dyn vmlab_hv_devices::IrqLine>
        }));
        let address = |device| vmlab_hv_proto::machine::PciAddress {
            bus: 0,
            device,
            function: 0,
        };
        let a = PirqRouter::line(&router, address(2), 1).unwrap();
        let b = PirqRouter::line(&router, address(6), 1).unwrap();
        router.lock().set_route(6, 10);
        a.set_level(true);
        b.set_level(true);
        a.set_level(false);
        assert!(lines[22].0.load(Ordering::SeqCst));
        assert!(lines[10].0.load(Ordering::SeqCst));
        router.lock().set_route(6, 11);
        assert!(!lines[10].0.load(Ordering::SeqCst));
        assert!(lines[11].0.load(Ordering::SeqCst));
        router.lock().set_route(6, 0x80);
        assert!(!lines[11].0.load(Ordering::SeqCst));
        assert!(lines[22].0.load(Ordering::SeqCst));
        b.set_level(false);
        assert!(!lines[22].0.load(Ordering::SeqCst));
    }
}
