# aarch64 `virt`: the fw_cfg files of QEMU

The reference side of the normalised hwref diff for aarch64 (phase 3 D9,
phase 3d card C11). Each directory is one QEMU 11.1.3 `-machine virt`
configuration, read over qtest with no firmware and no guest running. So the
ACPI tables are as QEMU builds them: pointers relative to the file, checksums
zero, and the loader program that a firmware runs to finish them.

Recapture, from `hv/`, in about five seconds:

```sh
python3 scripts/hwref/virt_fwcfg.py --output docs/hwref/virt-fwcfg
python3 scripts/hwref/virt_fwcfg.py --list
```

It needs `qemu-system-aarch64` and AAVMF. QEMU is run as a black box. No QEMU
source is read.

## Layout

- `capture.json`: the argv, the numeric items, and the directory with the
  selector, the size and a sha256 of each file.
- `fwcfg/<name>`: each file. A file of size 0 is an empty file.
- `acpi/<signature>`: the tables of `etc/acpi/tables`. That file is these
  tables in the order of `acpi_tables`, then zeros up to 131072 bytes. It is
  not stored a second time.

## Configurations

| Name | `-cpu` | vCPUs | Memory | Why |
|---|---|---|---|---|
| `smp1-2g`, `smp2-2g`, `smp4-2g`, `smp8-8g` | `max` | 1, 2, 4, 8 | 2 GiB, 8 GiB | The CPU count, up to the limit of a GICv2. |
| `smp2-512m`, `smp2-1g`, `smp2-1536m`, `smp2-4g`, `smp2-16g`, `smp2-32g`, `smp2-64g` | `max` | 2 | 512 MiB to 64 GiB | The memory size, across the SMBIOS extended-size boundary at 32 GiB. |
| `smp2-2g-uuid` | `max` | 2 | 2 GiB | `-uuid`, for the byte order in SMBIOS and in the numeric item. |
| `a53-smp2-2g` | `cortex-a53` | 2 | 2 GiB | Another CPU model with a PMU. |
| `a53-nopmu-smp1-2g`, `a53-nopmu-smp2-2g`, `a53-nopmu-smp4-2g` | `cortex-a53,pmu=off` | 1, 2, 4 | 2 GiB | QEMU's own tables for a CPU without a PMU. |

`-cpu max` is what vmlab's QEMU path passes. The devices are those of a
`linux-modern` VM: virtio-serial, virtio-blk, virtio-gpu and xHCI on PCI.

## What the captures show

- F1. The ACPI tables depend on the CPU count and on nothing else that was
  varied. Memory, the UUID and the CPU model change no ACPI byte.
- F2. A CPU without a PMU changes one field: `Performance Interrupt GSIV` of
  each GICC structure in the MADT is 0 in place of 23. This is the difference
  V5 of the phase 3d design, and the one rule of
  `scripts/hwref/approved-differences-virt.json`.
- F3. The CPU model changes no fw_cfg file (`a53-smp2-2g` equals `smp2-2g`).
  The difference G2 of the design does not show at this level.
- F4. SMBIOS depends on the CPU count, the memory size and the UUID. There
  is no type 0 structure: AAVMF supplies it.
- F5. The tables are DSDT, FACP, APIC, PPTT, GTDT, MCFG, SPCR, DBG2, IORT and
  XSDT. The XSDT of QEMU 11.1.3 lists the PPTT twice. AAVMF installs it once
  (the guest's XSDT has eight entries). vmlab-hv presents the same file.
- F6. The FADT says PSCI over HVC (ARM boot flags 3) and hardware-reduced
  ACPI. Each GICC names the GICV and GICH frames at `0x08040000` and
  `0x08030000`, also on a machine without EL2.
- F7. The DSDT declares the PL011 (`COM0`), fw_cfg (`FWCF`), 32 virtio-mmio
  transports (`VR00` to `VR31`, `LNRO0005`, at `0x0a000000` with GSIV 48 to
  79), four PCI interrupt links, the PCI root (`PCI0`), the GED at
  `0x09080000` on GSIV 41 and the power button. A kernel probes each
  virtio-mmio transport, so the machine must answer there as QEMU's empty
  transports do.
- F8. The numeric items: the signature, the feature bitmap 3 (DMA), the UUID
  in RFC 4122 order and the CPU count. The RAM size, `max_cpus`, `nographic`
  and `boot_menu` read as zero.

## The candidate side

`vmlab_hv_firmware::virt` generates the same files, and
`crates/vmlab-hv-firmware/tests/virt_hwref.rs` is the diff. It runs in
`just ci::check` and needs no QEMU:

- With the PMU interrupt of the captured CPU, the generated files equal all
  16 captures byte for byte.
- Without a PMU, which is what vmlab-hv presents, the files equal the three
  `a53-nopmu-*` captures byte for byte, and differ from the others in F2 only.
  The allow-list covers that field with both values, and nothing else.

Nothing needs normalising at this level: no pointer is relocated and no
checksum is filled before a firmware runs.

`docs/hwref/virt-boot/debian-13-a53-nopmu-smp1.json` is a Debian 13 boot on
QEMU with `-cpu cortex-a53,pmu=off`, the machine whose files vmlab-hv
reproduces. The kernel boots with ACPI and no device tree, finds PSCI 1.1,
the GICv2m frame, the SPCR console and the DMI identity, logs
`No ACPI PMU IRQ for CPU0`, and reaches systemd and the vmlab agent. Make it
again with `python3 scripts/hwref/virt_boot.py --output DIR`.
