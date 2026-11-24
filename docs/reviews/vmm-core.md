# Review: the vmlab-hv VMM core and the legacy PC devices

Card #580, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

All paths are relative to `hv/crates/`. Line numbers are those of commit
`c6123728` (`next` at the start of the card). `devices/` stands for
`vmlab-hv-devices/src/`.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. The reading was split three ways (the VMM dispatch, PCI and
interrupt paths, then the VM, reset, SMM and snapshot paths, then the legacy
devices),
and the VGA and Cirrus code was read separately. Every finding below was then
checked again against the code at the cited lines.

One fact carries several findings: the workspace sets no `overflow-checks`,
so an arithmetic overflow panics in debug and test builds and wraps in the
release build. The PIT and the PIC are KVM's in-kernel devices and are not in
this crate, except the port 0x61 estimate of PIT channel 2
(`vmlab-hv-vmm/src/chipset.rs:419-440`).

Severity words are those of the card: data loss, crash of the VMM process,
security against a hostile guest or peer, wrong result, cosmetic. A crash
reachable only through a changed snapshot file is ranked above a guest that
reaches the same crash only in a debug build. "Security inside the guest"
means a guest OS getting past a boundary of its own firmware (SMM), with no
effect on the host or other VMs. No finding lets a guest affect the host or
another VM in a release build.

## What was read and what was not

Read in full, without the test modules:

- `vmlab-hv-vmm/src/`: `vcpu.rs`, `bus.rs`, `pci.rs`, `memory.rs`,
  `kvm.rs`, `chipset.rs`, `q35.rs`, `layout.rs`, `e820.rs`, `reset.rs`,
  `smm.rs`, `smm/fwcfg.rs`, `a20.rs`, `pm.rs`, `coalesced.rs`, `pc.rs`,
  `clock.rs`, `snapshot.rs`, `snapshot/format.rs`
- `vmlab-hv-firmware/src/fw_cfg.rs`
- `devices/`: `floppy/mod.rs`, `chipset/isa.rs`, `serial.rs`,
  `serial/socket.rs`, `display/mod.rs`, `display/cirrus/mod.rs`,
  `display/cirrus/blit.rs`

Read in part:

- `vmlab-hv-vmm/src/vm.rs` (120-400, 556-1530), `builder.rs` (540-1000,
  1120-1175, 1385-1425, 1640-1700), `vfio.rs` (60-140, 381-415)
- `devices/chipset/rtc.rs` (1-420), `chipset/ps2.rs` (1-620, without the key
  table tail), `chipset/lpc.rs` (1-420), `chipset/hpet.rs` (1-280)
- the test modules, only for helper names: `floppy/tests.rs`,
  `cirrus/tests.rs`, `rtc.rs`, `lpc.rs`, `pci.rs`, `snapshot_tests.rs`

Not read: `vm.rs` 1-120 and after 1530, `chipset/smbus.rs` (only the
caller in `chipset.rs`), `display/virtio_vga.rs`, `debug.rs`, `hyperv.rs`,
`macos.rs`, `kvm_tests.rs` and `tests/pc_chipset.rs` beyond their test lists,
and the IDE, AHCI and USB devices on the same buses.

## Findings, worst first

### F1. A crafted snapshot makes the restore allocate without limit

- Where: `vmlab-hv-vmm/src/snapshot/format.rs:322-323`
  (`decode_ram_regions`), `snapshot.rs:1073-1074` (`decode_msrs`) and
  `snapshot.rs:822` (`load`). Arithmetic in the same decoder at
  `format.rs:338` and `345`.
- Fault: `decode_ram_regions` reads a `u32` count and calls
  `Vec::with_capacity(count)` for 16-byte entries before it reads them, so
  `0xffff_ffff` asks for 64 GiB. `decode_msrs` does the same, and it runs
  inside `restore_kernel_state`, after guest RAM has been overwritten, so a
  failure there also loses the old state. `load` allocates
  `vec![0u8; vmstate_size]` from the qcow2 snapshot table. A failed
  allocation aborts the process and returns no error. In the
  decoder, `covered += reader.u64()? * PAGE_SIZE` and `base + covered` are
  unchecked and overflow in debug builds.
- Reach: a snapshot file that is corrupt or was changed. The guest cannot
  write the vmstate area.
- Severity: crash of the VMM process, in every build.
- Proof: test `decode_ram_regions_rejects_a_huge_region_count` in the
  `format.rs` test module: a stream holding only `u32::MAX`, decoded with
  `decode_ram_regions(&mut Reader::new(&v), |_, _| Ok(()))`, must return
  `Corrupt`. Today it aborts. The same for `decode_msrs(&u32::MAX.to_le_bytes())`,
  and a stream with a zero-run of `u64::MAX` pages for the overflow.
- Confidence: high for the code. Whether the allocation aborts or is
  granted lazily depends on the host's overcommit setting. The overflow and
  the `decode_msrs` path do not.
- Fixed by card #612. `decode_ram_regions` refuses a region count that the
  rest of the stream cannot hold, a region that ends past the address space,
  and a run that overflows or passes the end of its region. It refuses the
  run before it writes the pages. `decode_msrs` refuses a count that is not
  the section's length in 12-byte entries. `restore` bounds the snapshot
  table's vmstate size by what the machine can have saved (its RAM, nine
  bytes of record header for each page, and 1 GiB for the sections) and
  returns an error when the allocator refuses the buffer
  (`vmstate_buffer`). On the build host the 64 GiB request was granted
  lazily, so before the fix the count tests failed on the error text and
  not by an abort. The overflow test panicked. Tests:
  `decode_ram_regions_rejects_a_huge_region_count`,
  `decode_ram_regions_rejects_a_run_that_overflows`,
  `decode_ram_regions_refuses_an_overrunning_data_run_before_it_writes`,
  `decode_msrs_rejects_a_huge_entry_count` and
  `load_rejects_a_vmstate_size_no_machine_of_this_size_can_have_saved`.

### F2. Floppy: a restored command whose operation does not match its opcode panics the host

- Where: `devices/floppy/mod.rs:935-945` (`restore_state`), with the panics
  at `646` (`run`), `761` and `775` (`start_pio`, `pio_sector_done`).
- Fault: restore checks a saved `Execution` only for
  `command.len() == command_length(command[0])`, the PIO buffer size and
  position, and the phase. It does not check that `operation` belongs to the
  opcode in `command[0]`, or that the drive holds a medium. `operation: Read`
  with `command: [0x0a, 0]` (READ ID's opcode, two bytes) passes. The next
  `dma_ready()` reaches `run()`, which reads `command[2]..command[6]` and
  indexes out of bounds. `operation: Format` or `ReadId` with a 512-byte
  `pio` at position 511 panics at `775` (`command[6]`) on the next FIFO read.
  `operation: Read` with a valid nine-byte command and a PIO buffer on an
  empty drive passes the media check (both sides are `None`) and panics at
  `761` on `media.as_ref().unwrap()`.
- Reach: a snapshot file. The guest does not choose these fields.
- Severity: crash of the VMM process, in every build.
- Proof: test `restore_rejects_execution_inconsistent_with_opcode` in
  `floppy/tests.rs`, built like `restore_rejects_pio_position_at_end_of_buffer`.
  Restore each of the three states (the third on `Machine::new("empty")`).
  Assert `restore_state` returns `Err`. Today it returns `Ok`, and the next
  `dma_ready()` or `io_read(5)` panics.
- Confidence: high.
- Fixed by card #612. `data_operation` is the one table from an opcode to
  its operation. `execute` starts a command by it, and `restore_state`
  refuses a saved command whose operation is not the one its opcode starts.
  A sector in the PIO buffer is accepted only for a read, a write or a
  verify on a drive that holds a medium. Tests:
  `restore_rejects_execution_inconsistent_with_opcode` and
  `restore_keeps_a_command_that_matches_its_opcode`.

### F3. Cirrus: a restored waiting BitBLT has unchecked geometry

- Where: `devices/display/cirrus/mod.rs:858-867` (`restore_state`), with
  `cirrus/blit.rs:157-168` (`source_row_bytes`) and `cirrus/mod.rs:377-398`
  (`feed`).
- Fault: restore checks a saved `Blit` only for `system_source()`,
  `row < height` and `pending.len() < source_row_bytes()`. `width`,
  `height`, `dst_pitch`, `src_pitch`, `dst` and `skip` are `usize` fields
  taken from JSON without limits, while `Blit::latch` can only produce
  `width <= 8192`, `height <= 2048`, 13-bit pitches and 22-bit addresses.
  With `width = usize::MAX`, `source_row_bytes` calls `next_multiple_of(4)`,
  which panics in debug builds. With `width = 1 << 40`, the check passes and
  every byte the guest then writes to the aperture is pushed onto `pending`
  until 1 TiB is buffered, and the row then walks 2^40 pixels.
- Reach: a snapshot file, then an ordinary guest that writes the aperture.
- Severity: crash of the VMM process (debug panic, or host memory exhaustion
  in any build).
- Proof: test `restore_rejects_blit_geometry_outside_the_registers` in
  `cirrus/tests.rs`. Take the state of
  `snapshot_restores_extended_state_and_a_waiting_blit`, decode the JSON
  tail, set `blit.width` to `1 << 40` (and in a second case to
  `usize::MAX`), and assert `restore_state` returns `Err`. Today the first
  returns `Ok` and the second panics in the test build.
- Confidence: high.
- Fixed by card #640. `Blit::within_registers` holds the limits of the
  registers `Blit::latch` reads: a width of 1 to 8192, a height of 1 to
  2048, 13-bit pitches, 22-bit addresses and a left clip of at most 7.
  `restore_state` refuses a waiting BitBLT outside them, before it computes
  `source_row_bytes`. Test:
  `restore_rejects_blit_geometry_outside_the_registers`.

### F4. ISA DMA (and every device on `ram.mem`) writes SMRAM and write-protected shadow RAM

- Where: `vmlab-hv-vmm/src/builder.rs:830-838` and `1132`, and
  `devices/chipset/isa.rs:321-336` (`dma_transfer`).
- Fault: on firmware machines the builder inserts the PAM shadow region
  (0xc0000-0xfffff) into `ram.mem`, and without `secure_smm` the SMRAM
  backing at 0xa0000 too. That `ram.mem` is the memory of the ISA DMA
  controller and of the PCI devices. `dma_transfer` writes it with
  `write_slice` without asking the MCH. So a DMA write lands in SMRAM whether
  D_OPEN is set or D_LCK is set, and in shadow RAM whether its PAM bank is
  write-enabled or not. The CPU outside SMM sees VGA memory at 0xa0000 at the
  same time. On the chipsets modelled, DMA cycles decode as CPU cycles do.
- Reach: a ring-0 guest: program channel 2 at page 0x0a, address 0x0000,
  mode 0x46, and issue a floppy READ. The `secure_smm` (OVMF secure boot)
  machine keeps SMRAM out of `ram.mem` and is not affected for SMRAM.
- Severity: security inside the guest (an OS can change a locked SeaBIOS SMM
  handler or a read-only BIOS shadow), and a wrong result (DMA meant for the
  VGA window lands in hidden memory).
- Proof: builder-level test `isa_dma_does_not_write_closed_smram` in
  `vmlab-hv-vmm`. Build a `pc` machine with a floppy, DMA-read one sector to
  0xa0000 with SMRAM closed, and assert that the SMRAM backing
  (`Pam::smram_region`) does not hold the sector. A second case with PAM bank
  12 read-only and address 0xf0000 asserts the shadow is unchanged.
- Confidence: high for the path. Whether SeaBIOS on this machine keeps live
  SMM code in that region was not checked.
- Fixed by card #611. Device models get a second view of guest memory,
  `GuestRam::device_mem`, and `ram.mem` stays the complete view for the
  snapshot and the debugger. The device view holds no SMRAM, open or closed.
  Its 0xc0000-0xfffff is `Pam`'s device mapping, remapped per bank when a PAM
  attribute changes: the shadow shared while the bank's write enable is set,
  otherwise a private copy-on-write mapping of what the CPU reads there, so a
  device write does not reach the shadow. The ISA DMA controller, the PCI
  devices, fw_cfg and the Hyper-V hypercall path use it. A passed-through
  device (vfio) no longer gets SMRAM, but its IOMMU mapping is made once and
  still holds the shadow writable whatever PAM says: that part is open. The
  proof is at the seam the builder wires memory through
  (`q35::attach_firmware_memory`), without KVM:
  `isa_dma_does_not_write_closed_smram`,
  `isa_dma_does_not_write_read_only_shadow_ram` and
  `device_view_of_shadow_ram_follows_the_pam_attributes` in `q35.rs`.

### F5. Non-secure q35 and pc: the SMRAM lock (D_LCK) is not enforced

- Where: `vmlab-hv-vmm/src/q35.rs:466` (`0x9d => 0x7f`), `q35.rs:452`
  (pc: `0x40..=0xff => 0xff` covers 0x72), and `Pam::update` at
  `q35.rs:235-256`.
- Fault: without `secure_smm` the MCH has no `SmramControl`, and SMRAMC is a
  plain masked byte. The guest can clear D_LCK (bit 4) after setting it, and
  `Pam::update` opens the SMRAM alias from D_OPEN (bit 6) alone, whatever
  D_LCK says. On the chipset, D_LCK makes SMRAMC read-only until reset and
  forces D_OPEN off. The `secure_smm` path does this correctly
  (`smm.rs:104-110`).
- Reach: a ring-0 guest on a non-secure firmware machine: write 0x1a to
  SMRAMC (lock), then 0x4a (open), then read or write 0xa0000.
- Severity: security inside the guest (an OS can read and change SMRAM
  after the firmware locked it). No host impact. Together with F4 it is the
  second way into the same memory.
- Proof: test `smram_lock_holds_until_reset` in the `q35.rs` test module:
  `mch.config_write(0x9c / 4, 1, &[0x1a])`, then `&[0x4a]`. Assert that
  config 0x9d still has bit 4 set and bit 6 clear, and that
  `pam.smram_open` is false. The same for pc through 0x72.
- Confidence: high.
- Fixed by card #611. Once D_LCK is set, `Mch::config_write` drops writes to
  SMRAMC (0x9d on q35, 0x72 on pc) and to ESMRAMC on q35, and setting D_LCK
  clears D_OPEN. `Pam::update` opens the alias only for D_OPEN without
  D_LCK, which also covers a restored state. A reset restores the power-on
  state and unlocks. QEMU's i440fx does not model the lock, so the pc byte
  now differs from the qtest capture on purpose. Tests:
  `smram_lock_holds_until_reset` and `restored_smram_lock_keeps_smram_closed`
  in `q35.rs`, `i440fx_smram_uses_72h_and_locks_until_reset` in
  `tests/pc_chipset.rs`.

### F6. Floppy: READ ID after an out-of-range sector adds 1 to 255

- Where: `devices/floppy/mod.rs:601-602` (`finish_read_id`), with the
  sector set at `561` (`start`).
- Fault: the implied seek of a data command stores the guest's sector byte
  `r` unclamped (`sect: r`). `run()` refuses `sect > sectors` at `670` but
  leaves `at.sect` at 255. A following READ ID computes `at.sect + 1` on a
  `u8`. Restore does not check `drives[].at.sect` either.
- Reach: a guest with port access to 0x3f5. With a 1.44 MB medium at rate 0:
  write `0x46,0,0,0,0xff,2,18,0x1b,0xff`, read the seven result bytes, write
  `0x4a,0`, then advance `READ_ID_NS`.
- Severity: crash of the VMM process in debug and test builds (overflow
  check). The release profile has no `overflow-checks`, so there the reported
  sector wraps to 0: wrong result.
- Proof: test `read_id_after_an_out_of_range_sector_does_not_overflow` in
  `floppy/tests.rs`, with `Machine::new("floppy")` and `m.out(...)` as above.
  Today it panics with "attempt to add with overflow".
- Confidence: high.

### F7. PIIX4 PM window: 128 ports over a 64-aligned base, and a subtraction that wraps at 0xffff

- Where: `devices/chipset/lpc.rs:120-123` (`pm_base`), `194-195`
  (`read_port`) and `222-223` (`write_port`).
- Fault: on PIIX4 the PM base is masked with `0xffc0` (64-byte aligned), but
  both port paths claim `port.wrapping_sub(base) < 128` and then compute
  `port - base` without wrapping. With PMBA = 0xffc0 and PMREGMISC bit 0
  set (config 0x40-0xff is writable, `lpc.rs:330`), any access to ports
  0x0000-0x003f takes the branch and evaluates `0x0000 - 0xffc0`. With a
  normal base the LPC also claims base+0x40..base+0x7f, 64 ports that are
  not PIIX4's.
- Reach: a guest on the `pc` machine (PIIX4 at 00:01.3,
  `builder.rs:895`). `chipset.io_read`/`io_write` see every port that the
  VGA path does not take (`vm.rs:844-867`), so the LPC is asked first.
- Severity: crash of the VMM process in debug and test builds. In release the
  LPC swallows the ISA DMA ports 0x00-0x3f (reads 0, writes dropped): a guest
  harming only itself, wrong result.
- Proof: test `piix4_pm_window_does_not_wrap_or_overclaim` in the `lpc.rs`
  test module. Build a PIIX4 `Lpc`, write `0xffc1` to config 0x40 and 1 to
  0x80, then assert `!read_port(0x0000, ..)`. Also, with PMBA = 0xb000,
  assert `!read_port(0xb040, ..)`.
- Confidence: high.
- Fixed by card #641. `Lpc::pm_offset` is the one test both port paths use:
  the block is 64 ports on PIIX4 and 128 on ICH9, the same as the alignment
  of its base, and the offset comes from a checked subtraction, so a port
  below the base is never in the block. Tests:
  `piix4_pm_window_does_not_wrap_or_overclaim` and
  `ich9_pm_window_keeps_128_ports` in `chipset/lpc.rs`.

### F8. pc: a VGA framebuffer BAR placed over low RAM ends the VM

- Where: `vmlab-hv-vmm/src/vm.rs:1080-1084` (`sync_vga_mapping`), with
  `layout.rs:48-56` (`MemLayout::pc`).
- Fault: the framebuffer slot is accepted when
  `base >= 0x8000_0000 && base + size <= 0xfec0_0000`. That bound fits q35,
  whose low RAM stops at 2 GiB, but `MemLayout::pc` keeps low RAM up to
  3 GiB. On a pc machine with more than 2 GiB, a BAR0 at 0x8000_0000 passes
  the filter, `KVM_SET_USER_MEMORY_REGION` fails on the overlap with the RAM
  slot, and the write path calls `request_exit(Error)`. The old mapping is
  already removed at that point.
- Reach: a guest on pc with 3 GiB of RAM and a VGA that the VMM maps
  directly: write BAR0 = 0x8000_0000 and set memory decode through
  0xcf8/0xcfc.
- Severity: the guest stops its own VM with an error (guest-caused crash of
  the VM, not of other VMs). Linux with `pci=realloc` can do it without
  intent.
- Proof: KVM test `pc_vga_bar_over_low_ram_is_not_mapped` next to the
  other `sync_vga_mapping` tests: build a pc machine with 3 GiB and stdvga,
  program BAR0 as above, and assert the VM keeps running and the framebuffer
  is served through the BAR callbacks. Today the VM exits with "mapping VGA
  framebuffer".
- Confidence: high for the bound. The exit path was traced by reading.

### F9. A guest reset that is pending when a snapshot load starts runs after the load

- Where: `vmlab-hv-vmm/src/vm.rs:328-331` (`service_reset`) and
  `snapshot.rs:658` (`load`), which never clears `reset_pending`.
- Fault: `service_reset` returns early when `snapshot_op.try_lock()` fails.
  If the guest writes 0xcf9, 0x92 or 0xfe to 0x64 just before a load takes
  the lock, `reset_pending` stays set through the load, and the events
  thread then resets the machine the user has just restored. The other
  order is also wrong: a save taken while the reset is pending records a
  machine whose reset request is lost (the vCPUs are parked, the LPC
  `reset_control` is saved with bit 2 set, so the guest's retry through
  0xcf9 does nothing), and that snapshot restores to a guest waiting for a
  reset that never comes.
- Reach: a guest reset at the moment of a load or save. It needs no hostile
  guest, only timing.
- Severity: wrong result. The running state the user restored is lost to a
  reboot.
- Proof: test `load_clears_a_pending_guest_reset` in `snapshot_tests.rs`
  with the `machine(...)` helper: set `reset_pending` (a crate-visible
  hook), call `snapshot::load`, then run `service_reset`, and assert no
  `Event::Reset` is emitted and the restored state is intact. For the save
  side: set `reset_pending`, save, load into a fresh machine, and assert
  that the reset still happens or the save was refused.
- Confidence: medium. The window is real in the code. The test needs a
  hook to set the flag, because driving it from the guest is a race.
- Fixed by card #790. The pending reset is machine state: a save writes it
  as the vmstate section `reset-pending`, and a load replaces the flag with
  the recorded value (none in an older snapshot). So a load drops the reset
  of the machine it replaces, a failed load gets it back from the rollback
  checkpoint, and a snapshot saved with a reset pending restores to a
  machine that is reset. Tests `load_clears_a_pending_guest_reset` and
  `save_carries_a_pending_guest_reset` hold the snapshot-op lock themselves
  to fix the order. They need KVM, so the merge bar does not run them.

### F10. The direct-kernel ACPI PM block is not in snapshots

- Where: `vmlab-hv-vmm/src/builder.rs:1396-1404`. The `PmDevice` is
  registered on the I/O bus and is never pushed to `snap_nodes`, and
  `save_kernel_state` does not cover it.
- Fault: after a load, PM1a_STS, PM1a_EN and PM1a_CNT keep the live values
  of the process that loads. The guest enables PWRBTN once at boot. When the
  loading process has EN = 0 (a fresh process, or one whose guest had not
  yet booted that far), `powerdown` (`vm.rs:1722-1729`,
  `press_power_button`) sets the status bit and never raises the SCI, so the
  guest does not shut down. `legacy_reset_control` (port 0xcf9 without a
  chipset) is not saved either. That one is harmless.
- Reach: any direct-kernel (non-firmware) machine restored into another
  process or after a reboot.
- Severity: wrong result (graceful shutdown stops working after a load).
- Proof: test `snapshot_preserves_pm1_enable` in `snapshot_tests.rs` with
  `machine(...)`: pause, `shared.io_write(0x602, &[0, 1])`, save,
  `io_write(0x602, &[0, 0])`, load, and assert `io_read(0x602)` of two
  bytes returns `0x0100`. Today it returns 0.
- Confidence: high.

### F11. I/O BARs placed below 0x1000 are never dispatched

- Where: `vmlab-hv-vmm/src/vm.rs:868` (`dispatch_io_read`) and `934`
  (`io_write`), gate `port >= IO_ALLOC_BASE` with `IO_ALLOC_BASE = 0x1000`
  (`layout.rs:27`). The root itself would decode it: `pci.rs:723-749`
  (`find_bar`) and `pci.rs:826-833` (`bar_placement`).
- Fault: the shipped DSDT gives `PCI0._CRS` the I/O ranges 0x0000-0x0cf7 and
  0x0d00-0xffff. An OS that moves an I/O BAR into 0x0d00-0x0fff gets a
  device that reads 0xff and drops writes, while `set_bar_address` has told
  the device it is placed.
- Reach: a guest that reassigns I/O resources (Linux `pci=realloc`, Windows
  resource rebalancing).
- Severity: wrong result (a device that stops working).
- Proof: test `io_bar_programmed_below_0x1000_is_dispatched` with the
  `VmShared` harness of the `vm.rs` tests: a fake PCI function with an I/O BAR
  of 0x20 bytes, BAR = 0x0d01 and command = 1, then `shared.io_write(0x0d04,
  &[1])`. Assert the function's `bar_write` saw it.
- Confidence: high.

### F12. q35: an expansion ROM or BAR placed over chipset MMIO takes the access

- Where: `vmlab-hv-vmm/src/vm.rs:990` and `1027` (`in_pci_window` is true
  for every address once the MCH exists), and `pci.rs:661-675` (the ROM
  decode has no `base != 0` check, unlike BARs at `686`).
- Fault: PCI decode runs before `mmio_bus`, with no check that the address is
  in the PCI hole. A ROM enabled at 0xc0000 (value `0x000c0001`) takes the
  writes that should reach `Pam::write` (`q35.rs:290`) for a shadow bank in
  write-only mode, and a BAR at 0xfed00000 hides the HPET.
- Reach: a guest that programs a ROM or BAR there.
- Severity: wrong result, the guest harming itself.
- Proof: test `rom_bar_does_not_hide_pam_shadow_writes` with the `vm.rs`
  harness and the PAM on `mmio_bus`: enable a ROM at 0xc0000, set PAM bank 0
  to write-only, `mmio_write(0xc0000, &[0x42])`, and assert the shadow holds
  0x42.
- Confidence: medium. The order was traced. That a firmware or OS places a
  ROM there was not seen.

### F13. fw_cfg SMI negotiation keeps a stale DMA high address

- Where: `vmlab-hv-vmm/src/smm/fwcfg.rs:126-131` and `145`, against
  `vmlab-hv-firmware/src/fw_cfg.rs:299-304`.
- Fault: the wrapper `SmiFwCfg` stores `dma_high` on a write to offset 4 and
  never clears it. The inner `FwCfg` clears its `dma_address` to 0 after
  each trigger, as QEMU does. After one 64-bit transfer, a later write of
  only the low half makes the inner device run the descriptor at `0:low`
  while the wrapper reads header and completion at `high:low`, so the SMI
  feature negotiation it tracks no longer matches what the guest wrote.
- Reach: a guest that writes only the low half after a 64-bit transfer.
  edk2 and SeaBIOS write both halves.
- Severity: wrong result, low. The guest affects only its own negotiation.
- Proof: test `fw_cfg_low_half_dma_after_a_64_bit_one` in `smm/tests.rs`,
  with guest memory at 0 and at 4 GiB: one 64-bit DMA write, then a
  descriptor at a low address that selects requested-features and writes
  `[1, 0, ...]`, triggered with `write(8, &D.to_be_bytes())` alone. Read
  features-ok and assert `broadcast()`. Today it is false.
- Confidence: high.

### F14. A four-byte write at 0xcfd-0xcff spills into the next register, and past offset 255

- Where: `vmlab-hv-vmm/src/pci.rs:919` (`PciConfigIo::write`).
- Fault: `offset = reg * 4 + (offset - 4)` and the whole access goes to
  `config_write`, which splits it at the dword boundary and writes the next
  register too. With reg 63 it writes register 64, in extended space that
  CF8/CFC cannot address. Reads at the same ports stop at the dword
  (`copy_into` pads with 0xff, `pci.rs:904`), so reads and writes disagree.
- Reach: a guest doing an unaligned wide OUT to 0xcfd-0xcff.
- Severity: wrong result, low. No known guest does it.
- Proof: test `cfc_write_does_not_cross_the_dword` in the `pci.rs` test
  module: `cfg.write(0, &0x8000_08fcu32.to_le_bytes())`,
  `cfg.write(7, &[1, 2, 3, 4])`, and assert the fake function's config log
  has no register 64 entry.
- Confidence: high.

### F15. The high dword of a 64-bit BAR is not aligned to the BAR size

- Where: `vmlab-hv-vmm/src/pci.rs:625` (`write_high_dword`).
- Fault: the low-dword path masks with `!(size - 1)` (`pci.rs:610`), the
  high-dword path does not. For a BAR larger than 4 GiB (the virtio-vga blob
  aperture, up to 256 GiB, or VFIO) a write of the low then the high dword
  leaves bits set that hardware hardwires to zero. Reads return them, and
  `set_bar_address` passes the misaligned base to the device. The result
  depends on the order of the two writes.
- Reach: a guest that writes an unaligned high dword.
- Severity: wrong result, low.
- Proof: test `high_dword_of_a_large_bar_is_size_aligned`: a fake function
  with an `Mmio64` BAR of `0x4_0000_0000` bytes, low dword 0x0c, high dword 3.
  Assert the high dword reads 0 and the placement is aligned.
- Confidence: high.

### F16. pc: every SMBus port write turns the controller back on

- Where: `vmlab-hv-vmm/src/chipset.rs:466-473` (`io_write`).
- Fault: before each write to the SMBus I/O window the chipset calls
  `smbus.config_write(1, 0, &[1])` and `smbus.config_write(0x40 / 4, 0,
  &[1])`. This undoes a guest's clear of I/O enable or HST_EN on the next
  access. `io_read` does not do it, so reads and writes see two states.
- Reach: a guest that disables the SMBus host and then touches its ports.
- Severity: wrong result, low.
- Proof: test `smbus_write_keeps_a_cleared_enable` in the `chipset.rs`
  tests: clear the enable bits through config space, write a port in the
  window, and assert config 0x04 bit 0 and 0x40 bit 0 stay clear.
- Confidence: high.

### F17. RTC reset clears the SET bit

- Where: `devices/chipset/rtc.rs:69` (`Resettable::reset`).
- Fault: `self.cmos[0x0b] &= !0xf8` clears SET (bit 7) as well as PIE, AIE,
  UIE and SQWE. The MC146818 RESET pin clears 0x78 and leaves SET. A reset
  during a time-set lets the clock run without `commit_calendar`, so the next
  tick overwrites the half-written time.
- Reach: a guest that resets during a time-set.
- Severity: wrong result, low.
- Proof: test `reset_keeps_the_set_bit` in the `rtc.rs` test module:
  `put(0x0b, 0x82)`, `reset()`, assert `get(0x0b) & 0x80 != 0`.
- Confidence: medium. The datasheet behaviour is from the MC146818
  description. Whether QEMU's reset keeps SET was not checked against the
  oracle.

### F18. Port-I/O BAR service holds the PCI root lock across the device call

- Where: `vmlab-hv-vmm/src/pci.rs:709-720` and `742-744` (`find_bar`
  returns the device guard while the root guard taken at `vm.rs:869` and
  `935` is held).
- Fault: the module doc (`pci.rs:16-17`) says device calls run outside the
  root lock, and MMIO does that (`PciRoot::read_mmio`). Port I/O does not.
  A slow I/O BAR (a passed-through device's I/O region) stalls every other
  vCPU's config cycles and MMIO decode. No handler that takes `pci_root`
  again was found, so this is latency, not a deadlock.
- Severity: wrong result (latency), low.
- Proof: copy `blocked_gpu_bar_does_not_delay_another_pci_function`
  (`pci.rs:1095`) with an I/O BAR blocked in `bar_write`, and assert that a
  second thread's `PciRoot::read_mmio` to another function returns while the
  first is blocked (wait on a channel, no time limit). Today it does not
  return until the first is released.
- Confidence: high.

## Notes

Suspicions without a proving test, and behaviour that was checked and is
not a fault.

- N-PM. `vmlab-hv-vmm/src/pm.rs:99-127`: a one-byte write to a PM1
  register is widened to 16 bits with the other byte zero
  (`read_u16`). A byte write to the high byte of PM1_CNT (offset 5) clears
  SCI_EN, and a byte write to offset 2 clears `EN_PWRBTN`. The device is used
  only on direct-kernel machines (`builder.rs:1396-1404`), whose Linux
  guests use word accesses from the FADT, so no guest is known to be hit.
  Test: `pm_byte_write_keeps_the_other_byte`.
- N-PMBASE. A guest can move the ICH9 PM base over the PCI configuration
  ports (PMBASE 0x0c80 covers 0xcf8-0xcff) or over the RTC, because
  `chipset.io_read`/`io_write` (`vm.rs:863-866`) ask the LPC before the I/O
  bus. That breaks only the guest itself, as a decode conflict does on real
  hardware.
- N-WRAP. `bus.rs`: a multi-byte port access that runs past the end of a
  region is given to the first device whole, with offsets beyond its length.
  The RTC masks with `& 7`, so `outw 0x77` writes its second byte to the
  index register 0x70. QEMU splits such accesses. Wrong result only, no guest
  is known to do this.
- N-DMA. ISA DMA does not wrap at the 64 KiB page and its decrement mode
  covers the block just below the address. Both are deliberate, to match QEMU
  11.1.2 (`isa.rs:283-288`, tests at `639` and `658`), and are not faults.
- N-VGA. The VGA core and Cirrus were read in full for guest-reachable
  panics. Every VRAM access in rendering goes through `get()`, a mask, or
  `read_obj(..).unwrap_or(0)`. Register indexes are `u8` into 256-byte banks.
  The DAC component is at most 2 and is validated on restore. The cursor
  pattern addresses stay below 4 MiB for every SR13 value. DISPI values are
  range-checked on write and on restore. A BitBLT whose span leaves video
  memory uses a full-memory window that wraps. No fault was found there.
  Two behaviours that are not faults: legacy VGA renders only the first
  256 KiB (`frame`, `display/mod.rs:786-790`), so a mode-13h start address
  near the top of plane memory reads zeros. Changing the DISPI depth while
  enabled can describe a frame larger than VRAM, which renders as black.

- N-IRQ. The wired-OR state of shared lines, `IrqHub.sources` and
  `Source.level` (`vmlab-hv-vmm/src/chipset.rs:24-61`) and
  `PirqRouter.sources`/`levels` (`q35.rs:585-657`), is neither saved nor
  reset. It stays right only if every INTx device drives its line again on
  restore and reset. The serial port does (`last_irq_level = None`). The
  other devices were not checked. A stale high level would hide the next
  assertion. Test: snapshot with a level-triggered line high, restore into
  a fresh process, lower it from the device, and assert KVM sees it low.
- N-SLOTS. VFIO direct maps (`vfio.rs:381`) and the virtio-vga blob
  aperture (`vm.rs:1087-1094`, only `base >= 4 GiB`) take the guest's BAR
  address without checking it against RAM above 4 GiB. KVM refuses the
  overlapping slot, so the result is a VM exit as in F8, not corruption.
- N-TSEG. The SMRAM slot is sized once, but a guest can select a 1, 2 or
  8 MiB TSEG in ESMRAMC before D_LCK. Not traced to a fault.
- N-CPUID. `ResetState::reset` restores CPUID through `restore_kernel_state`
  (`snapshot.rs:1173`) after `KVM_RUN`. Some host kernels since 5.16 refuse
  a changed `KVM_SET_CPUID2` after the first run once the guest has set
  CR4.OSXSAVE, and the reset then ends the VM. Depends on the host kernel.
- N-SMRAM-RESTORE. A changed snapshot can restore `Smram` open flags that
  disagree with a locked `SmramControl`. The next owned config write
  repairs it.
- N-PC-RESTORE. `ChipsetRegisters::restore_state` on pc writes `fast_a20`
  before it validates the SMI bytes, so a refused restore is partial.
- N-PIT. Port 0x61 and the HPET legacy callback do `get_pit2`, change,
  `set_pit2` (`chipset.rs:516-519`, `256-262`), which can undo another
  vCPU's PIT programming made in between.
- N-FLOPPY-N. The floppy always moves 512-byte sectors and ignores the size
  code N and DTL. QEMU sizes the transfer from `128 << min(N, 7)`. The DMA
  count bounds it, no crash.
- N-HPET. HPET routes are not checked against the advertised route mask
  (`hpet.rs:109`). A guest can pulse any GSI 0-31 through its own wired-OR
  source: spurious interrupts to itself only.
- N-A20. The PS/2 output port and port 0x92 each set the A20 gate directly,
  the last writer wins, as in QEMU. Not a fault. The snapshot order (PS/2
  before A20, `builder.rs:915` and `1693`) restores the saved gate.
- N-SERIAL. The serial log sink grows while the guest writes, as in QEMU.
  Disk use is a host policy question.

## Other work seen

- The I/O BAR allocator of a root without firmware can overflow `u16`
  (`pci.rs:147`, `io_next += size as u16`). Host configuration only.
- 64-bit BARs share one `sizing` flag for both dwords (`pci.rs:602`,
  `622`, `836-853`). Linux's sizing order hides it.
- Each MMIO exit on q35 scans all 256 PCI functions under the root lock
  when the four-entry cache misses (`pci.rs:676-703`).
