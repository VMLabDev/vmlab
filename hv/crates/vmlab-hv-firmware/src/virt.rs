//! Firmware inputs of the aarch64 `virt` machine: the ACPI tables, SMBIOS and
//! the fw_cfg files that AAVMF reads (phase 3d, F4 and card C11).
//!
//! What the tables say is pinned by `docs/hwref/virt-fwcfg`: the fw_cfg files
//! of QEMU 11.1.3 `-machine virt`, read over qtest with no guest running
//! (`scripts/hwref/virt_fwcfg.py`). `tests/virt_hwref.rs` holds this module
//! against every captured configuration.
//!
//! Sources read (phase 3d DESIGN.md, "Sources"): S6, the captured files of a
//! running QEMU, and S7, the ACPI 6.3 specification (FADT, MADT, GTDT, MCFG,
//! PPTT, the table header and the AML package encoding), the Microsoft SPCR
//! and DBG2 table documents, the Arm IORT document (DEN 0049, revision E),
//! DMTF DSP0134 3.0 (SMBIOS), and QEMU's interface documents
//! `docs/specs/fw_cfg.rst` and the firmware table-loader record layout that
//! `loader.rs` already encodes. No QEMU source was read.

pub mod acpi;
pub mod smbios;

/// Guest-physical addresses and interrupts of `virt` that the tables name.
/// The machine model must place its devices at the same values.
pub mod map {
    /// GICv2 distributor.
    pub const GICD_BASE: u64 = 0x0800_0000;
    /// GICv2 CPU interface.
    pub const GICC_BASE: u64 = 0x0801_0000;
    /// GICv2m MSI frame, with its shared peripheral interrupts.
    pub const V2M_BASE: u64 = 0x0802_0000;
    pub const V2M_SPI_BASE: u16 = 80;
    pub const V2M_SPI_COUNT: u16 = 64;
    /// The hypervisor control and virtual CPU interfaces. QEMU names both in
    /// every GICC structure, also on a machine without EL2.
    pub const GICH_BASE: u64 = 0x0803_0000;
    pub const GICV_BASE: u64 = 0x0804_0000;
    /// PL011, and its interrupt as a GSIV (SPI 1).
    pub const UART_BASE: u64 = 0x0900_0000;
    pub const UART_SIZE: u32 = 0x1000;
    pub const UART_GSIV: u32 = 33;
    /// fw_cfg over MMIO.
    pub const FW_CFG_BASE: u64 = 0x0902_0000;
    /// Start of RAM.
    pub const RAM_BASE: u64 = 0x4000_0000;
    /// PCI ECAM, 256 buses.
    pub const ECAM_BASE: u64 = 0x40_1000_0000;
    /// The generic timer interrupts as GSIVs (PPI 13, 14, 11 and 10).
    pub const TIMER_SECURE_EL1_GSIV: u32 = 29;
    pub const TIMER_NON_SECURE_EL1_GSIV: u32 = 30;
    pub const TIMER_VIRTUAL_GSIV: u32 = 27;
    pub const TIMER_EL2_GSIV: u32 = 26;
    /// The PMU overflow interrupt that QEMU's `-cpu max` presents (PPI 7).
    /// vmlab-hv presents no PMU (V5), so its tables carry 0 here.
    pub const QEMU_PMU_GSIV: u32 = 23;
}

/// The GICv2 limits a `virt` machine to eight CPUs.
pub const MAX_VCPUS: u8 = 8;
/// The largest RAM below the high memory regions of `virt`.
pub const MAX_RAM: u64 = 255 << 30;

/// What of a `virt` machine its firmware inputs depend on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub vcpus: u8,
    /// Bytes of RAM at `map::RAM_BASE`, a whole number of MiB.
    pub ram_size: u64,
    /// The machine UUID in RFC 4122 byte order.
    pub uuid: [u8; 16],
    /// The GSIV of the PMU overflow interrupt in each GICC structure. `None`
    /// is a CPU without a PMU, which is what vmlab-hv presents (V5).
    pub performance_interrupt: Option<u32>,
    /// OpenFirmware device paths in boot order, for the `bootorder` file.
    pub bootorder: Vec<String>,
}

impl Default for Platform {
    fn default() -> Self {
        Self {
            vcpus: 2,
            ram_size: 2 << 30,
            uuid: [0; 16],
            performance_interrupt: None,
            bootorder: Vec::new(),
        }
    }
}

impl Platform {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.vcpus == 0 || self.vcpus > MAX_VCPUS {
            return Err(format!("virt requires 1 <= vcpus <= {MAX_VCPUS}"));
        }
        if self.ram_size == 0 || !self.ram_size.is_multiple_of(1 << 20) || self.ram_size > MAX_RAM {
            return Err("virt RAM must be a whole number of MiB up to 255 GiB".into());
        }
        Ok(())
    }
}

/// The fixed numeric fw_cfg items of `virt`: the UUID and the CPU count.
/// QEMU leaves the RAM size, the maximum CPU count and the rest unset.
pub fn numeric_items(platform: &Platform) -> Result<Vec<(u16, Vec<u8>)>, String> {
    platform.validate()?;
    Ok(vec![
        (0x02, platform.uuid.to_vec()),
        (0x05, u16::from(platform.vcpus).to_le_bytes().to_vec()),
    ])
}

/// Every fw_cfg file of `virt`, in directory order (sorted by name).
pub fn files(platform: &Platform) -> Result<Vec<(String, Vec<u8>)>, String> {
    let acpi = acpi::generate(platform)?;
    let smbios = smbios::generate(platform)?;
    let mut bootorder = Vec::new();
    for path in &platform.bootorder {
        if !path.starts_with('/') || !path.is_ascii() || path.contains(['\n', '\r', '\0']) {
            return Err("boot paths must be absolute NUL-free ASCII without newlines".into());
        }
        if !bootorder.is_empty() {
            bootorder.push(b'\n');
        }
        bootorder.extend_from_slice(path.as_bytes());
    }
    if !bootorder.is_empty() {
        bootorder.push(0);
    }
    let files = vec![
        ("bios-geometry".to_string(), Vec::new()),
        ("bootorder".into(), bootorder),
        (crate::acpi::RSDP_FILE.into(), acpi.rsdp),
        (crate::acpi::TABLES_FILE.into(), acpi.tables),
        // No boot-failure timeout: the firmware waits for ever.
        ("etc/boot-fail-wait".into(), u32::MAX.to_le_bytes().to_vec()),
        ("etc/smbios/smbios-anchor".into(), smbios.anchor),
        ("etc/smbios/smbios-tables".into(), smbios.tables),
        ("etc/table-loader".into(), acpi.loader),
        // Without a TPM the event log is an empty file.
        ("etc/tpm/log".into(), Vec::new()),
    ];
    debug_assert!(files.windows(2).all(|pair| pair[0].0 < pair[1].0));
    Ok(files)
}
