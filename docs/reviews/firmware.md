# Review: firmware (OVMF, SeaBIOS, fw_cfg, iPXE)

Card #726, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Line numbers are those of commit `1122c1ab` (`next` at the start of the
card). `hv/` paths are below `hv/crates/`. Other paths are relative to the
repository root.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it.

Two findings rest on behaviour outside this repository that was not checked
here:

- F3 rests on how Debian's `AAVMF_CODE.secboot.fd` and QEMU's `virt` machine
  treat the variable flash. It is stated from what the reviewer knows of
  edk2's ArmVirtQemu and was not checked against the pinned package.
- F8 rests on iPXE ignoring `src/config/local/` in git. `build.py` itself
  depends on that: it copies headers there and then refuses untracked files,
  so a second run could not pass otherwise.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic.

### What was read

- The build and pin scripts: `guest/build-firmware.sh`, `build-ovmf.sh`,
  `build-seabios.sh`, `build-secureboot.sh`, `fetch-firmware.sh`,
  `enroll-secureboot.py`, `guest/ipxe/build.py`, `boot.py`, `upstream.json`,
  `SHA256SUMS.hv`, `config/`, the five patches in `guest/ovmf/`, the two
  configs and the patch in `guest/seabios/`, `guest/secureboot/README.md` and
  `microsoft-2023.json`, `guest/OVMF.md`, `guest/SECUREBOOT.md`,
  `guest/ipxe/README.md`, and the firmware part of `guest/package.sh`.
- `hv/vmlab-hv-firmware/src/fw_cfg.rs`, `platform.rs`, `loader.rs`,
  `smbios.rs`, `lib.rs`, and `acpi.rs:1-120`.
- `hv/vmlab-hv-vmm/src/smm/fwcfg.rs`, `loader.rs:1-330` (direct kernel
  boot), and the firmware parts of `builder.rs` (`:100-240`, `:770-835`,
  `:1560-1680`), `vm.rs` (`:1055-1075`, `:1268-1290`), `reset.rs:210-240` and
  `q35.rs:16-27`.
- `hv/vmlab-hv-devices/src/pflash/mod.rs` and
  `hv/vmlab-hv/src/serial_log.rs:1-90`.
- `src/qemu/firmware.rs:1-640`, the firmware parts of `src/labd/vm.rs`
  (`:236-270`, `:860-930`, `:1340-1400`), `src/labd/snapshot_state.rs:1-225`,
  `src/labd/freeze_clone.rs:525-546`, the fw_cfg and boot parts of
  `src/labd/hypervisor/vmlab_hv/describe.rs` (`:385-430`, `:590-625`,
  `:780-830`), and the pflash lines of `src/qemu/cmdline.rs`.
- The earlier reviews, to avoid a second report of the same fault:
  `vmm-core.md` (F4, F5 and F13 cover SMRAM, the SMRAM lock and the stale DMA
  high address of fw_cfg), `snapshots.md` (the variable store in offline
  snapshots), `templates-registry.md` and `template-build.md` (the VARS file
  a template carries), `containers.md` (the serial log cap).

### What was not read

- `hv/vmlab-hv-firmware/src/pc.rs`, the rest of `acpi.rs`, and every test
  under `hv/vmlab-hv-firmware/tests/` and `hv/vmlab-hv/tests/fw_cfg.rs`.
- `hv/vmlab-hv-vmm/src/smm.rs` and `smm/tests.rs`, the vCPU side of the SMM
  state that decides `in_smm` for a flash write, and `pflash/tests.rs`.
- `guest/FIRMWARE-TESTS.md`, `guest/SEABIOS.md`, `guest/logo/`,
  `guest/tests/`, `guest/ovmf/test-genfw-reproducible.py`, and the entries of
  `guest/secureboot/debian-inventory.json` beyond the certificate subjects.
- The tests of `src/qemu/firmware.rs` (from line 640), `src/freeze/plan.rs`,
  and how a template build carries `secure_boot_vars`.
- The binary images themselves. No firmware was built, unpacked or searched
  for keys. "Secrets or keys in the images" was reviewed from the build
  inputs only.
- No guest was booted.

## Summary

| Code | Severity | Fault |
| --- | --- | --- |
| F1 | security against a hostile guest | The firmware debug port (0x402) writes the serial log file past its size cap (fixed, card #736) |
| F2 | security, wrong result | `secure_boot = true` on a VM that already has a variable store boots the secure-boot firmware on that store, blank or guest-written |
| F3 | security inside the guest | aarch64 secure boot keeps its variable store on flash the guest OS can write |
| F4 | wrong result | A read-only VARS template gives a read-only per-VM store, and the VM cannot start |
| F5 | wrong result | A first-start VARS copy that fails leaves a partial store that blocks every later start |
| F6 | wrong result | An x86 VM whose store is not the 4 MiB layout is selected a firmware vmlab-hv then refuses |
| F7 | crash (debug builds), cosmetic | Direct kernel boot subtracts the initrd size from the top of low RAM without a check |
| F8 | wrong result | The iPXE build keeps stale headers in `config/local` of a cached checkout |
| F9 | cosmetic | The recorded iPXE ROM hashes and the firmware build reports are never compared with a build |

No finding lets a guest change another lab's firmware state. The variable
store is one file per VM (`.vmlab/vms/<vm>/OVMF_VARS.fd`). A destroy removes
it with the VM directory (`src/labd/lab.rs:1624`), an offline snapshot keeps
a copy per snapshot, and a guest reset keeps it, as a real machine does. The
only writable fw_cfg file is `etc/smi/requested-features`, and a reset puts
fw_cfg back to its power-on state (`hv/vmlab-hv-vmm/src/reset.rs:12-24`).

## Findings

### F1. The firmware debug port writes the serial log past its cap

- Where: `hv/vmlab-hv-vmm/src/builder.rs:800-811`,
  `hv/vmlab-hv-vmm/src/q35.rs:16-27`, against
  `hv/vmlab-hv/src/serial_log.rs:21-27` and `:71-80`.
- Fault: every firmware machine registers `DebugPort` on I/O port 0x402. It
  holds its own append handle on `desc.serial_log` and writes each guest byte
  to it. The size cap of the serial log lives in `CappedLog`, which is the
  UART's handle on the same file. `CappedLog` reads the file size, so it
  counts the debug port's bytes, but it runs that check only after `cap / 16`
  bytes were written through `CappedLog` itself. A guest that writes to port
  0x402 and stays silent on the UART never triggers the check, and the file
  grows without a limit. The comment at `serial_log.rs:21-25` says the second
  writer's bytes "are under the cap too". That holds only while the UART also
  writes.
- Reach: any guest code that can do port I/O on a VM with SeaBIOS or OVMF,
  with a RELEASE firmware as well as a DEBUG one. `rep outsb` to 0x402 moves
  a page per exit. The file is in the lab directory, so a full disk stops
  that lab and every other user of the filesystem.
- Severity: security against a hostile guest (the host disk fills).
- Proof: unit test `the_debug_port_cannot_pass_the_serial_log_cap`. Open
  `CappedLog` on a path with a cap of 64 KiB, open `DebugPort` on the same
  path as the builder does (create, append), write 1 MiB through
  `DebugPort::write` and nothing through `CappedLog`. Assert that the file is
  not longer than the cap plus `cap / 16`. Today it is 1 MiB. The two types
  are in different crates (`vmlab-hv` and `vmlab-hv-vmm`), so the test goes in
  `vmlab-hv`, or the debug port takes its sink from the device factory.
- Confidence: high.
- Fixed by card #736. `CappedLog` moved to
  `hv/crates/vmlab-hv-vmm/src/serial_log.rs`, and `DebugPort` holds its own
  `CappedLog` on the serial log (`DebugPort::open`, `q35.rs`) where it held a
  plain append handle. Each writer now runs the size check on its own bytes,
  so the file passes the cap by at most `cap / 16` for each of the two
  writers. The proof test is in `serial_log.rs`: on `next` the file reached
  1,048,576 bytes under a cap of 65,536.

### F2. Turning secure boot on later boots the old variable store

- Where: `src/labd/vm.rs:886-896` and `:1365-1374`,
  `src/qemu/firmware.rs:470-495` (`lookup_for_vars`).
- Fault: a VM's `OVMF_VARS.fd` is copied once and never replaced. When it
  exists, `uefi_firmware` selects the CODE build by the pairing of the
  present config and checks the store only by its size. Every x86_64 store is
  540,672 bytes, blank or enrolled. So a VM first started with
  `secure_boot = false` and later given `secure_boot = true` gets
  `OVMF_CODE_4M.secboot.fd` on the store it already has. That store is the
  blank template, which leaves the firmware in setup mode, where it verifies
  nothing. `src/qemu/firmware.rs:17-20` says this combination "is refused
  unless the machine opts in with `secure_boot_keys = "setup"`". Here it is
  not refused and nothing is logged. The machine description carries
  `secure_boot: true`, and status shows a secure-boot VM.
- The hostile case: while `secure_boot` is false the pflash is opened with
  `secure = false` (`hv/vmlab-hv-devices/src/pflash/mod.rs:92-95`), so guest
  ring 0 can program the whole store, its own PK, KEK and db included. The
  later secure-boot firmware trusts those keys. The same holds for a VM that
  went from true to false and back.
- Reach: an operator who hardens an existing VM by a config edit. A template
  clone is not affected at its first start. A changed `secure_boot_keys` or
  `secure_boot_vars` on an existing VM is ignored in the same way, which the
  comment at `vm.rs:887-888` states as intended. The fault is that the first
  switch to secure boot is treated like any later start.
- Severity: security (secure boot is shown as on and enforces nothing, or
  enforces keys the guest chose), and a wrong result.
- Proof: lifecycle test
  `turning_secure_boot_on_does_not_boot_an_unenrolled_store` in
  `src/labd/lifecycle_tests.rs`, with the fake hypervisor and a fake firmware
  layout through `Sources`: start a VM with `firmware = "ovmf"` and
  `secure_boot = false`, stop it, set `secure_boot = true`, start it again.
  Assert that the start is refused by name, or that the store now equals the
  enrolled template. Today the start succeeds, the launch has
  `secure_boot = true`, and the store's bytes are the blank template's.
- Confidence: high for the path. Which of the two outcomes is right is the
  owner's decision: a store the VM wrote without secure boot holds its boot
  entries.

### F3. aarch64 secure boot keeps its variables on guest-writable flash

- Where: `src/qemu/cmdline.rs:504-523`, with the pair from
  `src/qemu/firmware.rs:189-190` and `guest/fetch-firmware.sh:56-60`.
- Fault: on aarch64 the VARS file is a plain `-drive if=pflash,format=raw`.
  The machine gets no secure-world flash, and the pinned Debian
  `AAVMF_CODE.secboot.fd` has, to the reviewer's knowledge, no standalone MM
  variable driver. So the flash that holds PK, KEK, db and dbx is an ordinary
  memory-mapped NOR to the guest OS. On x86_64 the same store is written only
  from SMM (`pflash/mod.rs:92-95`, `-DSMM_REQUIRE=TRUE` in
  `guest/build-ovmf.sh:106`). `secure_boot = true` is accepted for aarch64
  with no word about the difference.
- Reach: guest kernel code on an aarch64 VM with `secure_boot = true`. It can
  rewrite its key databases, and the change outlives every reset. It reaches
  no other VM.
- Severity: security inside the guest (the OS can turn off its own secure
  boot for good). No host impact.
- Proof: steps, on a host with QEMU for aarch64. Boot an aarch64 Linux guest
  with `secure_boot = true`. In the guest, as kernel code or through
  `/dev/mem` with lockdown off, send the NOR program command to an erased
  byte of the VARS flash (unit 1, at 0x0400_0000 on `virt`) and read it back.
  Stop the VM and compare `OVMF_VARS.fd` with its copy from before. Expected:
  the byte is unchanged. If the file changed, the finding holds.
- Confidence: medium. It rests on the first point of "How the review was
  done". If the pinned AAVMF does protect its store, this finding is void.

### F4. A read-only VARS template gives a store the VM cannot open

- Where: `src/labd/vm.rs:1369-1374`, against
  `src/labd/freeze_clone.rs:536-542` and
  `hv/vmlab-hv-devices/src/pflash/mod.rs:52`.
- Fault: the first-start copy is `std::fs::copy`, which also copies the
  permission bits of the template. A template without the owner write bit
  (a firmware directory in a read-only store, a `secure_boot_vars` file kept
  mode 0444) gives a per-VM store without it. `Pflash::open` opens the store
  read-write and fails with "Permission denied". The copy stays, so every
  later start fails the same way. The clone path of a worktree lab had the
  same case and sets the write bit (`freeze_clone.rs:536-542`). This path
  does not.
- Severity: wrong result (the VM does not start, and the message names no
  cause).
- Proof: lifecycle test `the_vars_copy_is_writable_whatever_the_template_is`:
  a fake firmware layout whose VARS template is mode 0444, one start with the
  fake hypervisor, then assert `mode & 0o200 != 0` on `dirs.ovmf_vars()`.
  Today the mode is 0444.
- Confidence: high for the copy. The bundled files are installed 0644, so the
  reach is an override directory or a user file.

### F5. A failed first copy leaves a partial store

- Where: `src/labd/vm.rs:1369-1374` and `:886-896`.
- Fault: `std::fs::copy` writes the destination in place. When it fails part
  way (a full disk, an I/O error on the template), a short or empty
  `OVMF_VARS.fd` stays. The next start finds a store, skips the copy, and
  `lookup_for_vars` fails on its size with "it was created under a different
  firmware build", which is false. The VM does not start until the file is
  removed by hand. `snapshot_state.rs` and `freeze_clone.rs` write the same
  file through a temporary name and a rename.
- Severity: wrong result.
- Proof: lifecycle test `a_failed_vars_copy_leaves_no_store`: a fake firmware
  layout whose VARS template is a symlink to `/proc/self/mem` (a regular file
  to `stat`, an error on the first read), one start that fails, then assert
  that `dirs.ovmf_vars()` does not exist. Then put a real template in place
  and assert that a second start succeeds. Today the first start leaves an
  empty file and the second fails.
- Confidence: medium. The path is certain. That `/proc/self/mem` fails after
  the destination is created was not run.

### F6. An x86 store of another layout is selected and then refused

- Where: `src/qemu/firmware.rs:470-495`, against
  `hv/vmlab-hv-devices/src/pflash/mod.rs:52-57` and
  `hv/vmlab-hv-vmm/src/q35.rs:682`.
- Fault: `lookup_for_vars` exists so that a VM created under another
  firmware, "the host's 2 MiB OVMF", keeps the CODE build of its store. It
  returns that host pair for an x86_64 VM. Every x86 VM now runs on vmlab-hv
  (§22.4), whose flash accepts one layout only: a 0x37c000-byte CODE and a
  0x84000-byte VARS. The start then fails inside vmlab-hv with "OVMF requires
  0x37c000-byte CODE and 0x84000-byte VARS". The refusal list of
  `src/config/hypervisor.rs` does not hold this case, so it is not reported
  before the start, and no message says what to do with the old store. The
  same happens on a host without the bundle where `lookup` falls through to a
  2 MiB host pair for a new VM.
- Severity: wrong result (a late, low-level refusal for a case the lookup
  was written to keep working).
- Proof: unit test `a_store_of_another_layout_is_refused_by_name` beside the
  describe tests: a fake layout with a 131,072-byte host VARS pair and a VM
  directory with a 131,072-byte `OVMF_VARS.fd`, hypervisor `vmlab`. Assert
  that validation or the start plan fails with a message that names the
  store and the supported layout. Today `lookup_for_vars` returns the pair
  and the description is composed.
- Confidence: high.

### F7. Direct kernel boot does not check the initrd against low RAM

- Where: `hv/vmlab-hv-vmm/src/loader.rs:87`.
- Fault: `(low_ram_top - bytes.len() as u64) & !0xFFF` underflows when the
  initrd is larger than low RAM. A debug build panics. A release build wraps,
  the wrapped address passes the `addr < kernel_end` check, and the load
  fails later with "writing initrd" and an invalid guest address. The message
  written for this case at `:89-93` is never reached.
- Reach: a container micro-VM with little memory and a large initramfs. The
  inputs are the host's, not the guest's.
- Severity: crash in debug builds, cosmetic in release (a wrong message).
- Proof: unit test `an_initrd_larger_than_low_ram_is_refused_by_name` in
  `loader.rs`: the existing `tiny_bzimage`, 128 MiB of memory and
  `low_ram_top` of 128 MiB, an initrd file of 129 MiB. Assert an
  `Err(VmmError::Kernel)` whose text contains "does not fit". Today the test
  panics on the subtraction.
- Confidence: high.

### F8. The iPXE build keeps stale local headers

- Where: `guest/ipxe/build.py:64-80`.
- Fault: the build refuses a cached checkout with tracked changes or with
  untracked files, but it lists untracked files with `--exclude-standard`,
  and `src/config/local/` is ignored by iPXE's own git rules. The build
  copies today's headers into that directory and removes nothing. A header
  that an earlier revision of `guest/ipxe/config/` had, and that was since
  removed or renamed, stays in `guest/.cache/ipxe/upstream` and is compiled
  in. `ipxe-source.tar` is made from the repository's headers, so it does not
  hold the stale one, and the ROM no longer matches its "complete
  corresponding source".
- Reach: a build host that keeps `guest/.cache` or `hv/target/ipxe` between
  builds, which helios does.
- Severity: wrong result (a ROM that is not a function of the pinned inputs).
- Proof: steps. After one `python3 guest/ipxe/build.py --profile hv`, write
  `src/config/local/settings.h` with the one line `#error stale` into the
  cached checkout and run the build again. Expected: the build removes the
  file or refuses the checkout by name. Today the compile fails on the
  `#error`, or, with a harmless stale define, succeeds with it built in. A
  unit test of the same is possible once the checkout check is a function.
- Confidence: medium, see the second point of "How the review was done".

### F9. Recorded firmware hashes are never compared with a build

- Where: `guest/ipxe/SHA256SUMS.hv`, `guest/ipxe/build.py:113-115`,
  `guest/build-ovmf.sh:137-156`, `guest/build-seabios.sh:46-51`.
- Fault: the inputs are pinned well. Each archive and package has a SHA256,
  iPXE has a commit, and the Microsoft inputs have a manifest. The outputs
  are not. `SHA256SUMS.hv` is the only output record in the repository, and
  nothing reads it: `build.py` writes a fresh `SHA256SUMS` and prints it.
  `BUILDINFO`, `SEABIOS-BUILDINFO` and `IPXE-SHA256SUMS` are written by the
  build they describe. The documents call the builds reproducible
  (`guest/ipxe/README.md:1`, `guest/OVMF.md`), and no check would notice a
  change of the bytes from a toolchain update.
- Severity: cosmetic (a claim without a check). It is not a pin fault.
- Proof: steps. Run `just ci::hv-ipxe-build` on helios and then
  `cd $VMLAB_TARGET_ROOT/ipxe && sha256sum -c <repo>/guest/ipxe/SHA256SUMS.hv`.
  The README names GCC 16.2.1 and binutils 2.47 for the recorded hashes. If
  the check fails, the record is stale today. If it passes, the fault is only
  that no recipe runs it.
- Confidence: high that nothing compares. Not run.

## Notes

These have no proving test, or are not faults.

- N1. The default x86_64 secure-boot store keeps Debian's PK and KEK
  ("Debian UEFI Secure Boot (PK/KEK key)", `guest/secureboot/README.md`).
  Whoever holds that private key can sign a KEK or db update that every
  vmlab VM with the default store accepts. The review did not find out who
  holds it or whether Debian discarded it. vmlab holds none
  (`guest/secureboot/README.md`, "Service an existing VM"). No private key is
  in the repository's build inputs.
- N2. The bundled OVMF runs unsigned PCI option ROMs under secure boot. This
  is documented for iPXE (`guest/SECUREBOOT.md`, `guest/ipxe/README.md`). It
  holds for the ROM of a passed-through card too, which the documents do not
  say.
- N3. The aarch64 secure-boot template is Debian's as fetched. Only the
  x86_64 one gets the current Microsoft dbx (`guest/fetch-firmware.sh:160-162`).
- N4. `EDK2_COMMIT` in `build-ovmf.sh:11` and `COMMIT` in
  `build-seabios.sh:8` are labels. The archive hash is what is verified.
- N5. `build-ovmf.sh:24-33` and `build-seabios.sh:14-18` move a download into
  the cache before they check its hash. A bad download stays and fails every
  later run until it is removed by hand. `fetch-firmware.sh:104` says so in
  its message. The other two do not.
- N6. A fw_cfg DMA read takes its length from the guest, up to 4 GiB, and
  fills guest memory in 4 KiB steps on the vCPU thread with the device locked
  (`fw_cfg.rs:241-252`, `smm/fwcfg.rs:193-207`). The guest stalls only
  itself. QEMU behaves the same.
- N7. `SmiFwCfg` accepts byte writes through the data port into
  `etc/smi/requested-features` (`smm/fwcfg.rs:147`). QEMU removed data-port
  writes. The inner `FwCfg` ignores them, so the two copies of the bytes can
  differ. No effect was found.
- N8. With `secure = true`, each flash write from outside SMM sets the
  program-error and protect bits of the status register, and they stay set
  until a clear-status command from SMM (`pflash/mod.rs:92-95`). edk2's flash
  driver does not read the status after a write, so no effect was found.
- N9. The fw_cfg snapshot state is JSON of every file's bytes
  (`fw_cfg.rs:314-316`), about four bytes of text per byte, and the reset
  path keeps one more copy. With a splash image this is some megabytes per
  machine.
- N10. vmlab-hv gives every VM an all-zero SMBIOS system UUID.
  `SmbiosConfig::uuid` has no source in the machine description
  (`hv/vmlab-hv-vmm/src/builder.rs:1626-1630`). The QEMU argv passed no
  `-uuid` either, so this is not a regression. Clones cannot be told apart
  by UUID.
- N11. `Pflash::restore_state` writes the whole store of an online snapshot
  over the VARS file in place (`pflash/mod.rs:281`). A host crash in that
  write leaves a mixed store. The file is not locked against a second
  opener.
- N12. A worktree freeze copies the guest-written store, and every clone in
  another worktree starts from it (`src/freeze/plan.rs:183`). This is the
  design, as for the disks.
- N13. The bundled OVMF does not apply the host's boot order to the guest's
  persistent `BootOrder` (`guest/ovmf/boot-without-waits.patch`). A guest can
  therefore fix what it boots from across resets, and a changed `boot` in the
  lab file does not override it. This is the documented design
  (`guest/OVMF.md`, PRD §5.2).
- N14. Both builds set `PcdUninstallMemAttrProtocol`, Debian's workaround for
  old shims. It removes the memory attribute protocol that boot loaders use
  to enforce non-executable data.
- N15. `guest/OVMF.md` ("vmlab-hv firmware inputs") still says the vmlab-hv
  crates are pinned to revision `450de8c`. They are in this workspace since
  ADR-0022.
- N16. `secure_boot` with the DEBUG CODE variants is allowed. The debug text
  goes to the serial log, which F1 is about.
