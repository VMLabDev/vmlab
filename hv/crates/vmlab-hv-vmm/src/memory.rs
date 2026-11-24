//! Guest RAM: anonymous private mmaps, or one shared memfd when the machine
//! has vhost-user-fs shares (virtiofsd maps the same memfd later).

use std::fs::File;
use std::os::fd::FromRawFd;
use std::sync::Arc;

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{FileOffset, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmlab_hv_devices::{GuestMemory, GuestRegion};

use crate::error::VmmError;
use crate::layout::MemLayout;

type MmapRegion = vm_memory::MmapRegion<vmlab_hv_devices::WriteDiscard>;

/// Built guest memory plus what backs it.
pub struct GuestRam {
    pub mem: GuestMemory,
    /// Guest memory as a device model sees it: `mem` on a machine without
    /// firmware. On a firmware machine it holds no SMRAM, and its
    /// 0xc0000-0xfffff follows the PAM write enables (`q35::Pam`). Every
    /// access made for the guest outside SMM goes through this view.
    pub device_mem: GuestMemory,
    layout: MemLayout,
    /// The memfd backing RAM, when shared; kept open so the mapping lives.
    _file: Option<File>,
    pub(crate) firmware_rom: Option<GuestMemory>,
    pub(crate) a20: Option<Arc<parking_lot::Mutex<crate::a20::A20>>>,
}

impl GuestRam {
    /// Anonymous private RAM.
    pub fn anonymous(layout: MemLayout) -> Result<Self, VmmError> {
        let mem = mmap_anonymous(&layout)
            .map_err(|e| VmmError::Memory(format!("allocating guest RAM: {e}")))?;
        dont_dump(&mem)?;
        Ok(GuestRam {
            device_mem: mem.clone(),
            mem,
            layout,
            _file: None,
            firmware_rom: None,
            a20: None,
        })
    }

    /// RAM backed by one shared memfd, so a vhost-user backend can map the
    /// guest's memory.
    pub fn shared_memfd(layout: MemLayout) -> Result<Self, VmmError> {
        let file = create_memfd().map_err(VmmError::Memory)?;
        let total: u64 = layout
            .regions
            .iter()
            .map(|(base, size)| base + size)
            .max()
            .unwrap();
        // The memfd must cover the high region's offset too; its size is the
        // top of RAM.
        file.set_len(total)
            .map_err(|e| VmmError::Memory(format!("sizing memfd: {e}")))?;
        let mem = mmap_regions(&layout, &file)
            .map_err(|e| VmmError::Memory(format!("mapping guest RAM: {e}")))?;
        dont_dump(&mem)?;
        Ok(GuestRam {
            device_mem: mem.clone(),
            mem,
            layout,
            _file: Some(file),
            firmware_rom: None,
            a20: None,
        })
    }

    pub fn layout(&self) -> &MemLayout {
        &self.layout
    }

    pub fn pc(layout: MemLayout, vm: Arc<VmFd>) -> Result<Self, VmmError> {
        let mut ram = Self::shared_memfd(layout)?;
        let file = ram
            ._file
            .as_ref()
            .unwrap()
            .try_clone()
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        ram.a20 = Some(Arc::new(parking_lot::Mutex::new(crate::a20::A20::new(
            file,
            vm,
            &ram.layout.regions,
        )?)));
        Ok(ram)
    }
}

/// Exclude every guest-RAM mapping, including CPU-only aliases, from host cores.
pub(crate) fn dont_dump(mem: &GuestMemory) -> Result<(), VmmError> {
    for region in mem.iter() {
        // SAFETY: each region owns this page-aligned mapping for its full length.
        if unsafe {
            libc::madvise(
                region.as_ptr().cast(),
                region.len() as usize,
                libc::MADV_DONTDUMP,
            )
        } != 0
        {
            return Err(VmmError::Memory(format!(
                "MADV_DONTDUMP: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

fn mmap_regions(
    layout: &MemLayout,
    file: &File,
) -> Result<GuestMemory, vm_memory::mmap::FromRangesError> {
    let ranges = layout.regions.iter().map(|(base, size)| {
        let file_offset = FileOffset::new(file.try_clone().expect("clone memfd"), *base);
        (GuestAddress(*base), *size as usize, Some(file_offset))
    });
    GuestMemory::from_ranges_with_files(ranges)
}

/// The page size KVM maps guest RAM with when the host backs it with
/// transparent huge pages.
const HUGE_PAGE: usize = 2 << 20;

/// KVM maps a memslot with 2 MiB pages only when its host address and its
/// guest address are equal modulo 2 MiB.
fn huge_page_congruent(host: *mut u8, base: u64) -> bool {
    (host as u64 ^ base) & (HUGE_PAGE as u64 - 1) == 0
}

/// Anonymous RAM, one mapping per region, each placed so KVM can use 2 MiB
/// pages for it. q35's main low region starts at guest address 1 MiB, and an
/// address the kernel chooses is not congruent with that: every page of low
/// RAM then costs its own nested page fault (card #71, D28 G1).
fn mmap_anonymous(layout: &MemLayout) -> Result<GuestMemory, String> {
    let regions = layout
        .regions
        .iter()
        .map(|&(base, size)| {
            let mapping = map_congruent(base, size as usize).map_err(|e| e.to_string())?;
            GuestRegion::new(mapping, GuestAddress(base))
                .ok_or_else(|| format!("RAM region {base:#x}+{size:#x} wraps the address space"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    GuestMemory::from_regions(regions).map_err(|e| e.to_string())
}

/// One anonymous mapping for the region at guest address `base`. vm-memory
/// owns only a mapping it makes itself, at an address the kernel picks, so
/// the address is steered: see [`map_into_hole`]. A region the steering
/// cannot place keeps a plain mapping, which is correct and only slower.
fn map_congruent(base: u64, size: usize) -> Result<MmapRegion, vm_memory::mmap::MmapRegionError> {
    let plain = MmapRegion::new(size)?;
    if size < HUGE_PAGE || huge_page_congruent(plain.as_ptr(), base) {
        return Ok(plain);
    }
    // A mapping that landed elsewhere stays mapped until the end: it fills
    // the gap that took it, so the next attempt cannot fall into it again.
    let mut misplaced = Vec::new();
    for _ in 0..4 {
        match map_into_hole(base, size) {
            Some(mapping) if huge_page_congruent(mapping.as_ptr(), base) => return Ok(mapping),
            Some(mapping) => misplaced.push(mapping),
            None => break,
        }
    }
    Ok(plain)
}

/// Reserve a span 2 MiB longer than the region, open a hole of exactly the
/// region's size at the congruent address inside it, and map: the kernel
/// places a mapping in the highest gap that fits, which is that hole unless
/// another gap or another thread takes it. The caller checks the address.
fn map_into_hole(base: u64, size: usize) -> Option<MmapRegion> {
    let span = size.checked_add(HUGE_PAGE)?;
    // SAFETY: a fresh anonymous reservation that nothing else refers to.
    let start = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            span,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if start == libc::MAP_FAILED {
        return None;
    }
    let start = start as usize;
    let lead = (base as usize).wrapping_sub(start) & (HUGE_PAGE - 1);
    let hole = start + lead;
    let tail = span - lead - size;
    // SAFETY: the three ranges partition the reservation made above, which
    // only this function knows. The hole is unmapped before the mapping is
    // made and the two guards after it, so the hole is the gap between them.
    let mapping = unsafe {
        libc::munmap(hole as *mut libc::c_void, size);
        let mapping = MmapRegion::new(size);
        if lead != 0 {
            libc::munmap(start as *mut libc::c_void, lead);
        }
        if tail != 0 {
            libc::munmap((hole + size) as *mut libc::c_void, tail);
        }
        mapping
    };
    mapping.ok()
}

pub(crate) fn create_memfd() -> Result<File, String> {
    const MFD_CLOEXEC: u32 = 0x0001;
    const MFD_ALLOW_SEALING: u32 = 0x0002;
    // SAFETY: memfd_create(2) with a NUL-terminated name; the returned fd is
    // either a valid file descriptor or -1 with errno set.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"vmlab-hv-ram".as_ptr(),
            MFD_CLOEXEC | MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd is a fresh, owned file descriptor from the syscall above.
    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

/// Register every RAM region as a KVM memslot.
pub fn register_memslots(vm: &Arc<VmFd>, ram: &GuestRam) -> Result<(), VmmError> {
    if let Some(a20) = &ram.a20 {
        return a20.lock().register();
    }
    register_address_space(vm, ram, 0)
}

pub fn register_smm_memslots(vm: &Arc<VmFd>, ram: &GuestRam) -> Result<(), VmmError> {
    register_address_space(vm, ram, 1 << 16)
}

fn register_address_space(
    vm: &Arc<VmFd>,
    ram: &GuestRam,
    address_space: u32,
) -> Result<(), VmmError> {
    for (slot, &(base, size)) in ram.layout.regions.iter().enumerate() {
        let region = kvm_userspace_memory_region {
            slot: slot as u32 | address_space,
            guest_phys_addr: base,
            memory_size: size,
            userspace_addr: ram
                .mem
                .get_host_address(GuestAddress(base))
                .map_err(|e| VmmError::Memory(e.to_string()))? as u64,
            flags: 0,
        };
        // SAFETY: this is the documented safe wrapper for KVM_SET_USER_MEMORY_REGION.
        unsafe {
            vm.set_user_memory_region(region)
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION", e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::Bytes;

    #[test]
    fn guest_ram_is_excluded_from_host_core_dumps() {
        let memories = [
            GuestRam::anonymous(MemLayout::new(8).unwrap()).unwrap(),
            GuestRam::shared_memfd(MemLayout::new(8).unwrap()).unwrap(),
        ];
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        for memory in memories {
            for region in memory.mem.iter() {
                let address = region.as_ptr() as u64;
                let mut selected = false;
                let mut excluded = false;
                for line in smaps.lines() {
                    let mut fields = line.split_whitespace();
                    let first = fields.next().unwrap_or_default();
                    if let Some((start, end)) = first.split_once('-')
                        && let (Ok(start), Ok(end)) =
                            (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
                    {
                        selected = (start..end).contains(&address);
                    }
                    if selected && first == "VmFlags:" {
                        excluded = fields.any(|flag| flag == "dd");
                        break;
                    }
                }
                assert!(excluded, "guest mapping must carry MADV_DONTDUMP");
            }
        }
    }

    #[test]
    fn anonymous_ram_is_congruent_with_its_guest_address_modulo_2_mib() {
        // q35's low region starts at 1 MiB and has an odd size in MiB, the
        // shape the kernel's own placement leaves without huge pages.
        let layout = MemLayout::q35(8192).unwrap();
        let ram = GuestRam::anonymous(layout.clone()).unwrap();
        let regions: Vec<_> = ram
            .mem
            .iter()
            .map(|region| (region.start_addr().0, region.len()))
            .collect();
        assert_eq!(regions, layout.regions);
        for region in ram.mem.iter().filter(|r| r.len() >= HUGE_PAGE as u64) {
            let base = region.start_addr().0;
            assert!(
                huge_page_congruent(region.as_ptr(), base),
                "region {base:#x} is mapped at {:p}",
                region.as_ptr()
            );
        }
        // The steered mapping is ordinary RAM: both ends take a write.
        let (base, size) = layout.regions[1];
        ram.mem.write_slice(&[0x5a], GuestAddress(base)).unwrap();
        ram.mem
            .write_slice(&[0xa5], GuestAddress(base + size - 1))
            .unwrap();
        let mut byte = [0];
        ram.mem
            .read_slice(&mut byte, GuestAddress(base + size - 1))
            .unwrap();
        assert_eq!(byte, [0xa5]);
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
    fn registering_ram_ignores_device_regions_in_the_dma_view() {
        if !std::path::Path::new("/dev/kvm").exists() {
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let mut ram = GuestRam::anonymous(MemLayout::q35(8).unwrap()).unwrap();
        register_memslots(&kvm.vm, &ram).unwrap();
        let aperture = Arc::new(
            vmlab_hv_devices::GuestRegion::from_range(GuestAddress(0xa0000), 0x20000, None)
                .unwrap(),
        );
        let region = kvm_userspace_memory_region {
            slot: 128,
            guest_phys_addr: 0xa0000,
            memory_size: 0x20000,
            userspace_addr: aperture.as_ptr() as u64,
            flags: 0,
        };
        // SAFETY: aperture owns the mapping until after the VM is dropped.
        unsafe { kvm.vm.set_user_memory_region(region) }.unwrap();
        ram.mem = ram.mem.insert_region(aperture).unwrap();
        register_memslots(&kvm.vm, &ram).unwrap();
    }

    #[test]
    fn anonymous_ram_round_trips() {
        let layout = MemLayout::new(8).unwrap();
        let ram = GuestRam::anonymous(layout).unwrap();
        ram.mem.write_slice(b"hello", GuestAddress(0)).unwrap();
        let mut buf = [0u8; 5];
        ram.mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn shared_memfd_ram_round_trips_across_the_hole() {
        let layout = MemLayout::new(4096).unwrap();
        let ram = GuestRam::shared_memfd(layout).unwrap();
        // Write into the high region through a second mapping of the same
        // memfd, as vhost-user would see it.
        let low = &ram.mem;
        low.write_slice(b"low!", GuestAddress(0)).unwrap();
        low.write_slice(b"high", GuestAddress(0x1_0000_0000 + 0x1000))
            .unwrap();
        let mut buf = [0u8; 5];
        low.read_slice(&mut buf, GuestAddress(0x1_0000_0000 + 0x1000))
            .unwrap();
        assert_eq!(&buf[..4], b"high");
        assert_eq!(buf[4], 0);
    }

    #[test]
    fn anonymous_regions_dont_alias() {
        let layout = MemLayout::new(4096).unwrap();
        let ram = GuestRam::anonymous(layout).unwrap();
        ram.mem
            .write_slice(&7u64.to_le_bytes(), GuestAddress(0))
            .unwrap();
        let mut buf = [0u8; 8];
        ram.mem
            .read_slice(&mut buf, GuestAddress(0x1_0000_0000))
            .unwrap();
        assert_eq!(buf, [0; 8]);
    }

    #[test]
    fn memfd_size_covers_high_region() {
        // The memfd length equals the top of RAM, so the high region's file
        // offset (up to 4 GiB in) is inside the file.
        let layout = MemLayout::new(4096).unwrap();
        let ram = GuestRam::shared_memfd(layout).unwrap();
        let f = ram._file.as_ref().unwrap();
        assert_eq!(f.metadata().unwrap().len(), 0x1_4000_0000);
    }
}
