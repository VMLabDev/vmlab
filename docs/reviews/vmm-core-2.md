# Second review: the vmlab-hv VMM core and the legacy PC devices

Card #932, 2026-11-24. This is a fault list. It changes no code. The first
review of this area is `docs/reviews/vmm-core.md` (card #580). This document
lists faults that the first pass missed and faults that its fixes brought in.
It does not repeat a first-pass finding, except where the fix is incomplete,
and then it says so.

All paths are relative to `hv/crates/`. Line numbers are those of commit
`cee41195` (`next` at the start of the card). `devices/` stands for
`vmlab-hv-devices/src/`.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it.

First the diff of every fix commit that the first review names was read
against the code around it: `4d010783` (F1, F2), `eedbbb1a` (F4, F5),
`7a227be0` (F3), `a4e76a68` (F7), `883a802f` (F6), `733d0deb` (F8),
`38397089` (F9), `b5eb0308` (F10), `48a196f5` (F12) and `b91a09e0` (F11,
F13, F14). Then the files that the first pass did not read, or read only in
part, were read with the same severity words: data loss, crash of the VMM
process, security against a hostile guest or peer, wrong result, cosmetic.

## What was read and what was not

Read in full, without the test modules:

- the ten fix diffs above, and in the files they touch the functions around
  each change: `vmlab-hv-vmm/src/snapshot.rs` (`load_locked` to the end of
  `restore`, `restore_kernel_state`, `decode_msrs`), `snapshot/format.rs`
  (`decode_header`, `read_section`, `decode_ram_regions`), `q35.rs` (`Pam`,
  `attach_firmware_memory`, `Mch`), `pm.rs`, `vm.rs` 300-420, 860-1300 and
  1390-1600, `bus.rs`, `pci.rs` 700-930, `smm/fwcfg.rs` 95-200, `reset.rs`,
  `memory.rs` 1-200
- `vmlab-hv-vmm/src/hyperv.rs` (without tests), `vcpu.rs` 180-420 (the exit
  dispatch), `macos.rs` 160-300, `chipset.rs` 395-560
- `devices/`: `chipset/smbus.rs`, `chipset/hpet.rs`, `chipset/rtc.rs`,
  `chipset/lpc.rs` (non-test part), `chipset/ps2.rs` 500-722, `applesmc.rs`,
  `floppy/mod.rs` 280-1043, `chipset/isa.rs` 270-350, `tpm/mod.rs` 150-360
  (only the MMIO write path, for F2)

Read in part, only to follow one path: `builder.rs` (the bus registrations
and `machine_digest`), `devices/virtio/fs/vhost_user.rs` (`set_mem_table`),
`devices/virtio/gpu/` (the resource limits, for the vmstate allowance of the
first F1 fix), `devices/display/cirrus/blit.rs` (`within_registers`).

Not read: `vm.rs` 1-120, `debug.rs`, `kvm.rs`, `a20.rs`, `clock.rs`,
`coalesced.rs`, `diagnostics/`, `control.rs`, `devices/display/virtio_vga.rs`,
`devices/serial.rs`, `vmlab-hv-firmware/src/fw_cfg.rs` beyond its use in
`smm/fwcfg.rs`, and every test module beyond helper names. The first review
read most of these in full.

## Findings, worst first

### F1. A macOS-policy guest grows the logged MSR set and the machine log without limit

- Where: `vmlab-hv-vmm/src/macos.rs:251-277` (`answer`) and `229-246`
  (`user_msr`), with the set at `vm.rs:128` and its snapshot section at
  `snapshot.rs:587`.
- Fault: with `msr_policy = macos` the VM enables
  `KVM_CAP_X86_USER_SPACE_MSR` for the unknown-MSR reason
  (`macos.rs:209-227`), so every RDMSR or WRMSR of an index that KVM does not
  know exits to user space. `answer` inserts each new index into
  `shared.ignored_msrs`, a `BTreeSet<u32>`, and returns a log line for it,
  which `user_msr` writes to stderr. Neither has a limit. The set holds up to
  2^32 entries, and the vmlab adapter sends vmlab-hv's stderr to the
  machine's log file on disk (`src/labd/hypervisor/vmlab_hv/mod.rs:419-421`).
  The set is also written into every snapshot as JSON, so the vmstate grows
  with it.
- Reach: a ring-0 guest on a machine with the macOS profile: loop RDMSR over
  every index that is not in `MACOS_MSRS` and that KVM does not handle. Each
  access raises #GP in the guest, which the guest catches. About 70 bytes of
  log per index, so a full sweep writes about 280 GB of log and holds tens of
  GB of set in the VMM.
- Severity: security against a hostile guest: exhaustion of host disk (the
  log) and host memory (the set), which affects the host and other VMs.
  Limited to machines with `msr_policy = macos`.
- Proof: test `unhandled_msr_log_is_bounded` in the `macos.rs` test module.
  Call `answer(MsrPolicy::Macos, &mut seen, index, MsrExitReason::Unknown,
  false)` for 100 000 distinct indices outside `MACOS_MSRS` (for example
  `0x1000_0000..0x1001_86a0`). Assert that `seen.len()` stays at or below a
  fixed cap and that the number of returned log lines does too. Today both
  are 100 000.
- Confidence: high for the code. The disk numbers are an estimate from the
  line format.

### F2. The F12 fix puts every q35 PCI MMIO access behind the chipset MMIO bus lock, which a TPM command holds

- Where: `vmlab-hv-vmm/src/vm.rs:1131-1134` (`in_pci_window`, changed by
  `48a196f5`), with `vm.rs:1064` and `1109` (`self.mmio_bus.lock().read` and
  `.write` hold the bus guard across the device call) and
  `devices/tpm/mod.rs:157-168` (`go` runs the TPM command through
  `backend.execute`, a blocking round trip to swtpm, inside the MMIO write).
- Fault: before the fix, a q35 MMIO exit to a PCI BAR went to
  `PciRoot::read_mmio`/`write_mmio` without the `mmio_bus` lock. Now
  `in_pci_window` takes `self.mmio_bus.lock()` on every MMIO exit of a
  machine with an MCH, before the PCI decode. The TPM TIS is on `mmio_bus`
  (`builder.rs:1548-1553`), and a guest's TPM_STS.go makes the vCPU wait for
  swtpm while it holds the bus guard. A TPM2_CreatePrimary with an RSA key
  can take seconds in swtpm. During that time every other vCPU's access to
  any PCI BAR (AHCI, xHCI, e1000, HDA, a display BAR that is not mapped,
  MSI-X tables) stops at the lock. The HPET and the PAM range already shared
  that lock, so this is new only for PCI devices.
- Reach: any q35 machine with a TPM. Windows with BitLocker or measured boot,
  or Linux with `tpm2-tools`, does it without intent.
- Severity: wrong result (latency): all vCPUs of the VM stop doing device
  I/O for the length of a TPM command, which can trip guest driver timeouts.
  No crash, no deadlock: the guard in `in_pci_window` is dropped before the
  PCI root lock is taken.
- Proof: test `chipset_mmio_device_call_does_not_block_pci_bar_access` next
  to `bar_over_chipset_mmio_does_not_hide_the_chipset_device` in the `vm.rs`
  tests, with the `q35_with_function` harness. Hold `shared.mmio_bus.lock()`
  in the test thread, as a device call in progress does. From a second
  thread call `shared.mmio_read` on the function's BAR and send the result
  on a channel. The test thread then waits on that channel while it still
  holds the guard. With the fault the second thread never returns, so the
  test does not finish. A fix that decodes the chipset ranges from a copy
  outside the lock (they are fixed after build) passes. Like the other
  `vm.rs` dispatch tests it needs KVM.
- Confidence: high for the lock order. The swtpm duration depends on the
  command and the host.

### F3. Device writes to a write-protected PAM bank are read back by devices

- Where: `vmlab-hv-vmm/src/q35.rs:196-245` (`map_device_bank`) and the
  device view built by the F4 fix (`eedbbb1a`).
- Fault: a bank whose write enable is clear is mapped into the device view
  with `MAP_PRIVATE` (`q35.rs:206-210`). A device write there lands in a
  copy-on-write page. The shadow and the CPU view stay unchanged, which is
  what F4 asked for, but the next device read of the same address returns the
  dropped write, not the shadow. On the chipset the write is not stored
  anywhere, so a device reads back what the CPU reads. The private page
  stays until the bank's attribute changes or a restore or reset runs
  `refresh_device_view`, so the result also depends on later PAM writes.
- Reach: a ring-0 guest: set PAM bank 12 read-only, DMA a sector to 0xf0000
  (ISA DMA or any PCI bus master), then DMA from 0xf0000. No guest is known
  to do this.
- Severity: wrong result, low. The guest affects only its own devices.
- Proof: test `device_write_to_read_only_shadow_is_not_read_back` in the
  `q35.rs` test module, with `pc_with_isa_dma()` and `backing` as in
  `device_view_of_shadow_ram_follows_the_pam_attributes`. Set bank 12 to
  read-write (`mch.config_write(0x58 / 4, 1, &[0x30])`), write `[0x22; 512]`
  to 0xf0000 through `ram.device_mem`, set it read-only (`&[0x10]`), write
  `[0x44; 512]` through `ram.device_mem`, and assert
  `backing(&ram.device_mem, 0xf0000) == [0x22; 512]`. Today it is
  `[0x44; 512]`.
- Confidence: high.

## Notes

Suspicions without a proving test, behaviour that was checked and is not a
fault, and the state of the first-pass findings.

- N-FIXES. The fixes of F1 to F14 were checked against their findings. No
  fix is incomplete for the fault its finding describes, apart from the
  parts the first review already names as open: the VFIO IOMMU mapping of
  the shadow (F4) and the unsaved `legacy_reset_control` (F10). F15, F16,
  F17 and F18 have no fix and are unchanged on `next`.
- N-F1-ALLOWANCE. The bound of the F1 fix allows 1 GiB of sections beside
  RAM (`snapshot.rs:817`). The largest sections found are the Cirrus VRAM
  (4 MiB), the standard VGA VRAM and the virtio-gpu 2D resources (256 MiB of
  pixels at most, `devices/virtio/gpu/mod.rs:101`), so a machine cannot save
  a stream that its own load refuses. A new device with more state needs the
  allowance raised, or its own rollback checkpoint becomes unloadable.
- N-PAM-WO. In the device view a write-only bank (write enable set, read
  enable clear) maps the shadow shared, so a device reads the shadow where
  the CPU reads the ROM (`q35.rs:204-205`). The comment says this is
  deliberate. Whether the 82441FX and the Q35 MCH forward bus-master reads
  of such a bank to the ROM was not checked against a datasheet.
- N-MSR-MERGE. `restore_kernel_state` adds the saved `ignored-msrs` to the
  live set (`snapshot.rs:1204`) instead of replacing it, so after a load the
  machine keeps the indices of the machine it replaced. It only suppresses a
  log line. A test needs KVM.
- N-LOWPORT. Since the F11 fix, a port below 0x1000 that no chipset handler
  and no `io_bus` device takes goes through `PciRoot::dispatch_io_*`, a scan
  of every function under the root lock (`vm.rs:911-913`). On a
  direct-kernel machine, which has no chipset, Linux's `outb_p` delay writes
  to port 0x80 take that path. This is cost, not a wrong result.
- N-IOBUS. `IoBus::read` and `write` hold the bus lock across the device
  call, and the IDE data ports, the floppy ports and fw_cfg (a DMA transfer
  of a whole kernel image) are on `io_bus`. So a vCPU's legacy port access
  waits behind another vCPU's disk read or fw_cfg DMA. This was so before the
  fixes and is the port-I/O side of F2 and of the first review's F18.
- N-SMBBA. On pc a guest can set the PIIX4 SMBus base to 0 (or over another
  fixed port) and enable the host (`devices/chipset/lpc.rs:102-105`). The
  chipset then takes ports 0x00-0x0f, the ISA DMA controller
  (`chipset.rs:399-404`, `466-473`), before the ISA decode. The guest breaks
  only itself, as with the first review's N-PMBASE.
- N-HPET-PULSE. An edge-triggered HPET timer pulses its route low and high
  (`devices/chipset/hpet.rs:95-97`). If a level-triggered timer of the same
  HPET is asserted on the same GSI, the pulse lowers that level until
  `update_irqs` raises it again at the end of `advance`. The two timers on
  one IOAPIC pin with different trigger modes are a guest misconfiguration.
- N-VHOST-USER. The device view's PAM mapping has no file offset, so
  `set_mem_table` leaves 0xc0000-0xfffff out of the vhost-user table
  (`devices/virtio/fs/vhost_user.rs:330-339`). The comment there says the RAM
  region below covers that range, but on firmware machines low RAM stops at
  0xa0000. A virtio-fs request with a buffer in the BIOS area fails in the
  backend. This was so before the F4 fix too.
- Checked and not a fault: the floppy restore after the F2 fix (every opcode
  and operation pair, the PIO buffer on an empty drive, the drive position
  bounds), `finish_read_id` after the F6 fix, the PIIX4 and ICH9 PM window
  after the F7 fix, the F8 bound against every RAM region, the pending reset
  of F9 (its section, the rollback checkpoint, and `exit_on_reset`, which the
  machine digest covers), the PM section of F10 against what the write path
  can store, the CF8/CFC clip of F14, the fw_cfg DMA high half of F13, the
  SMBus, RTC, HPET, PS/2 and Apple SMC snapshot restores, and the Hyper-V
  SynIC page clear on the device view.

## Other work seen

- The vmstate section allowance of N-F1-ALLOWANCE is a constant with no test
  that ties it to the largest device section.
