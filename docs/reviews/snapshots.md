# Review: snapshots (online, offline, restore)

Card #381. A bug-hunting review of the snapshot paths of vmlab and vmlab-hv.
It lists faults and makes no fix. Reviewed at `next` commit `25de93a8`. All
paths are from the repository root, and all line numbers are from that commit.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped.

Severity words: **data loss**, **crash** (of the VM or a host process),
**security** (against a hostile guest), **wrong result**, **cosmetic**.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | data loss | An offline snapshot operation and a start of the same machine can write one qcow2 at the same time (fixed in #386) |
| F2 | data loss | Snapshot lookup prefers the qcow2 id over the name, so an all-digit name can address another snapshot |
| F3 | data loss | An offline restore of a multi-disk VM rewinds disk by disk with no check first and no way back |
| F4 | data loss | A lab-wide restore rewinds machines one by one and stops at the first that fails |
| F5 | wrong result | A container whose online restore fails stays paused |
| F6 | wrong result | virtio-net restores the framing position of a host stream that has moved on |
| F7 | wrong result | A capture that failed leaves a pending entry that a running vmlab-hv machine, or any container, cannot clear |
| F8 | wrong result | A failed vmlab-hv save leaves the snapshot on the earlier disks and the vmstate clusters in the image |
| F9 | wrong result | A vmlab-hv process killed during a load leaves its rollback checkpoint on every disk for good |
| F10 | wrong result | An offline snapshot does not hold the UEFI variables or the TPM state |
| F11 | wrong result | The micro-VM's ACPI PM1a block is in no snapshot section |

### F1. Offline snapshot operations do not exclude a start (data loss)

- Fixed in #386. A per-machine reservation excludes boot during offline
  capture, restore and deletion for VMs and containers.
- Where: `src/labd/vm.rs:1808-1829` (`take_snapshot`), `:1881-1891` (offline
  branch of `load_snapshot`), `:1896-1920` (`drop_snapshot`),
  `src/labd/vm.rs:1264-1270` (`boot_inner`), `src/labd/container.rs:1126-1135`,
  `:1189-1209`, and `hv/crates/vmlab-disk/src/qcow2/mod.rs` (no file lock on
  open).
- Why it is a fault: each of the three operations reads `power_state()` once,
  sees `Stopped`, and then runs `qimg::snapshot_create`, `snapshot_apply` or
  `snapshot_delete` on the disks. Nothing holds the state between the read and
  the end of the disk work. `boot_inner` only checks that the state is
  `Stopped` and sets `Starting`. The lab daemon serves requests concurrently
  (`src/proto/server.rs:100` serves each connection on its own task, and
  `src/labd/mod.rs:1211-1239` are plain awaits). So
  `vmlab snapshot take` (or `restore`, or `delete`) and `vmlab up` or
  `vm start` for the same stopped machine can overlap. With the default image
  tool the snapshot code and vmlab-hv then both hold the qcow2 open for
  writing, each with its own cached L1, L2 and refcount tables.
  `vmlab-disk` takes no lock on the file (a search of the crate for `flock`,
  `F_OFD_SETLK` and `LOCK_EX` finds nothing), so neither side notices. The
  second flush overwrites the first writer's metadata. QEMU with `qemu-img`
  is protected by QEMU's own image locking. vmlab-hv is not.
- Proof:
  1. `vmlab-disk` unit test `a_second_writable_open_of_one_image_is_refused`:
     open one qcow2 with `Qcow2Image::open(path, false)` twice. Today both
     opens succeed.
  2. Lifecycle test `an_offline_capture_excludes_a_concurrent_start` in
     `src/labd/lifecycle_tests.rs`: an image tool whose `snapshot_create`
     blocks on a channel, `lab.snapshot("a", "s")` started, then `start("a")`.
     The start must refuse or wait. Today it launches the hypervisor.

### F2. Lookup by id before name (data loss)

- Where: `hv/crates/vmlab-disk/src/qcow2/snapshot.rs:147-151`
  (`load_snapshot_l1`), `:399-403` (`snapshot_apply_impl`), `:447-451`
  (`snapshot_delete_impl`), `:182-186` (`snapshot_create_impl`), and
  `src/labd/vm.rs:2670-2678` (`validate_snapshot_name`).
- Why it is a fault: vmlab addresses a snapshot by its name only, and
  `validate_snapshot_name` allows an all-digit name. `vmlab-disk` resolves the
  string as a qcow2 **id** first and as a name second. Its own
  `next_snapshot_id` (`:531-547`) keeps ids and names apart, but QEMU and
  `qemu-img` assign ids as "highest plus one" with no such care. Take an image
  whose table was written by QEMU: snapshot `2` (id `1`), then snapshot `b`
  (id `2`). This is what a VM on `hypervisor = "qemu"`, or one that has moved
  from QEMU to vmlab-hv (§22.3, "offline snapshots carry over"), holds.
  `vmlab snapshot delete 2` on the stopped machine then removes snapshot `b`
  from the disk and the record of `2` from the state. `b` is lost and its
  record stays. `vmlab snapshot restore 2` applies `b` with no message.
  The callers disagree as well: `load_checkpoint_name`
  (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:637-639`) takes the first entry
  whose name **or** id matches in table order, and `drop_snapshot`
  (`src/labd/vm.rs:1910-1915`) checks the name alone before it calls a delete
  that prefers the id.
  A second effect needs no QEMU: on an image with snapshots `a` and `b`,
  `vmlab snapshot take 2` is refused with `snapshot "2" already exists`,
  because `:182` also compares the new name with the existing ids.
- Proof: S2 test in `hv/crates/vmlab-disk/tests/s2_qemuimg.rs`,
  `a_name_wins_over_another_snapshots_id`: `qemu-img snapshot -c 2`, write a
  marker, `qemu-img snapshot -c b`, then `ops::snapshot_delete(path, "2")` and
  list. Expected: `b` is still there. If `qemu-img` refuses the name `2`,
  build the table through `snapshot_create` with ids forced by a test helper.
  Unit test `a_digit_name_is_not_refused_for_an_unrelated_id`: create `a`,
  `b`, then `2`.
- Not checked: whether QEMU's `snapshot-save` job accepts an all-digit tag.
  The S2 test settles the `qemu-img` half.

### F3. Offline restore of several disks has no check and no way back (data loss)

- Where: `src/labd/vm.rs:1881-1891`.
- Why it is a fault: the offline branch stops the machine and then runs
  `snapshot_apply` on each path of `all_disk_paths()` in turn. It does not
  first check that every disk has the snapshot. The capture path does
  (`snapshot_offline_disks`, `:2641-2668`, checks first and rolls back). When
  the second disk fails, the first is already rewound, the error goes to the
  user, and the machine is left with disk 0 at the snapshot and disk 1 at the
  present. The state of disk 0 since the snapshot is gone, and the machine has
  been stopped. A plain way to get there: take the snapshot, then add an
  `extra_disk` to the VM, then restore. The new disk has no such snapshot.
  This is the card's case "restore into a VM with different devices".
- Proof: unit test beside `snapshot_offline_disks`'s tests
  (`src/labd/vm.rs:2777`), `an_offline_restore_checks_every_disk_first`: two
  qcow2 files, snapshot `s` on the first only, data written to the first after
  it, then the offline restore of `s`. Expected: an error and the first disk
  unchanged. Today the first disk is rewound.

### F4. A lab-wide restore stops half way (data loss)

- Where: `src/labd/mod.rs:1226-1231`.
- Why it is a fault: `snapshot.restore` with no machine loops over the
  machines and returns at the first error. The lab-wide capture
  (`snapshot_all`, `src/labd/lab.rs:3429-3453`) rolls back. The restore cannot
  roll back, so it has to check first, and it does not. A machine that lacks
  the snapshot (taken per machine, or added to the lab later), a record that
  is pending, locked by a freeze, bound to the other hypervisor, or a halted
  workspace without `--discard-guest-changes` each stop the loop after the
  machines before it were rewound. The lab is then at two points in time and
  the command reports only the error.
- Proof: test in `src/labd/lab.rs` beside
  `missing_snapshot_returns_not_found_without_calling_the_backend`
  (`:4620`), `a_lab_wide_restore_refuses_before_any_machine_moves`: fake
  machines `a` and `b`, snapshot `s` recorded on `a` only, the lab-wide
  restore. Expected: an error and no `restore` call on `a`. Today `a` is
  restored.

### F5. A failed online restore leaves a container paused (wrong result)

- Where: `src/labd/container.rs:1145-1149`.
- Why it is a fault: `restore_online` pauses the micro-VM and then calls
  `snapshots.load(...)` with `?`. On an error it returns without a resume.
  vmlab-hv puts a machine back in the run state it had when the load began
  (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:612-621`), and that state is
  paused, because vmlab paused it first. The container stays frozen until
  someone stops it. The VM path has the resume
  (`src/labd/vm.rs:1867-1871`, "A failed load must not strand a previously
  running VM").
- Proof: lifecycle test with the fake adapter (`src/labd/hypervisor/fake.rs`),
  `a_failed_container_load_resumes_the_container`: make `load` fail, call
  restore, and assert that the last control call is `resume`.

### F6. virtio-net restores the position of a stream that has moved (wrong result)

- Where: `hv/crates/vmlab-hv-devices/src/virtio/net.rs:2021-2033` (save) and
  `:2160-2163` (restore).
- Why it is a fault: in stream mode the device talks to the lab switch over a
  length-prefixed byte stream. The snapshot holds `tx_out` (bytes not yet
  written, which after a short write starts in the middle of a frame,
  `:1717-1728`) and `rx` (a partly read prefix or body). Both are positions in
  a stream that the snapshot does not hold. A load brings them back onto the
  live socket, which is somewhere else: at a frame boundary, in another frame,
  or a new connection of a new process. The old tail of `tx_out` is sent
  again, and the switch reads it as a length prefix. The bytes that the device
  had read of the current inbound frame are dropped, and the rest of that
  frame is read as a prefix. Both directions lose framing. It ends when a
  bogus length is over `MAX_FRAME` and the link is dropped (`:830`, `:589`).
  Until then the guest and the switch get frames made of other frames'
  bytes. The same happens with a saved `Idle` over a live partial read.
- Proof: unit test in `virtio/net.rs` next to the tests at `:3430-3470`,
  `a_load_does_not_resume_a_frame_the_stream_no_longer_holds`: set
  `rx = RxRead::Body { got: half }` and a non-empty `tx_out`, save, clear both
  as a later moment would, restore, then send one whole frame from the peer.
  Expected: the guest gets exactly that frame, and the peer reads nothing
  before the next frame the guest sends. S1 version: take a snapshot under
  `iperf` load, load it, and ping.

### F7. A pending capture that cannot be cleared (wrong result)

- Where: `src/labd/lab.rs:3364-3386` (the intent, and it is kept on every
  error), `:3705-3708` (delete), `src/labd/vm.rs:1808-1809`,
  `src/labd/container.rs:1198-1209`,
  `hv/crates/vmlab-hv-vmm/src/snapshot.rs:859-875`.
- Why it is a fault: `LabRuntime::snapshot` writes the name into
  `pending_snapshots` before the machine is asked, and keeps it on any error.
  Many errors come before anything touched a disk: an invalid name (the name
  is validated only inside `take_snapshot`), a suspended machine, a machine
  that is starting, the virtiofs, GPU and USB refusals, and a container that
  never started. The name is then taken: a second capture says "already
  exists or is pending cleanup" and the list tells the user to delete it. The
  delete goes to the machine:
  - On a running vmlab-hv machine `snapshot_delete` returns
    `snapshot_not_found` when any writable disk lacks the name. The error
    comes back and the pending entry stays. It also stays after a real partial
    save (F8): the first delete cleans the disks that had it and still returns
    the error for the one that did not, and the next delete finds nothing.
  - A stopped container calls `qimg::snapshot_delete` with no check that the
    snapshot exists (`container.rs:1205`). The VM path has that check
    (`vm.rs:1910-1915`). A container whose capture was refused can so never
    clear the entry, running or stopped.
- Proof: tests in `src/labd/lab.rs` beside
  `pending_capture_is_durable_and_cannot_be_replaced_or_restored` (`:4645`):
  `a_refused_capture_leaves_no_pending_entry` (fake machine whose `snapshot`
  fails before any work), and `a_pending_entry_is_cleared_when_no_disk_holds_it`
  (fake machine whose `delete_snapshot` answers as vmlab-hv does for a missing
  name). For the container: `drop_snapshot` on a scratch disk with no such
  snapshot must return `Ok`.

### F8. A failed vmlab-hv save cleans nothing (wrong result)

- Where: `hv/crates/vmlab-hv-vmm/src/snapshot.rs:313-352` (`save`) and
  `:377-405` (`capture_and_write`).
- Why it is a fault: `capture_and_write` first streams the whole vmstate into
  the active layer of the vmstate disk, and then creates the internal
  snapshot on each writable disk in turn. The name is not checked on any disk
  before the stream is written. When `snapshot_create` fails on a disk (the
  name or id exists there, the disk is full, an I/O error), `save` returns
  the error and does nothing else:
  - the disks before the failing one keep a snapshot of that name, with the
    vmstate when the vmstate disk was among them. The load path has a cleanup
    for its own checkpoint (`delete_checkpoint`, `:666-682`). The save path
    has none.
  - when the failing disk is the vmstate disk, or comes before it, the
    vmstate clusters stay mapped in the active layer above the disk size. They
    are as large as the guest's used RAM. A later snapshot punches only its
    own length (`hv/crates/vmlab-disk/src/qcow2/snapshot.rs:272-322`), and its
    L1 is cloned before the punch (`:199`), so a longer old tail stays
    allocated and is shared into every later snapshot.
  The first case feeds F7.
- Proof: test in `hv/crates/vmlab-hv-vmm/src/snapshot_tests.rs`,
  `a_failed_save_leaves_no_snapshot_and_no_vmstate`: a machine with two
  writable qcow2 disks, a snapshot `s` made on the second alone with
  `vmlab_disk::ops::snapshot_create`, then `save(handle, "s")`. Expected: an
  error, no `s` on the first disk, and the first image no larger than before
  apart from a constant. One disk is enough for the leak: save `s` two times
  and compare the file length after the refused second save.

### F9. A rollback checkpoint outlives a killed load (wrong result)

- Where: `hv/crates/vmlab-hv-vmm/src/snapshot.rs:627-664`
  (`load_checkpoint_name`) and `:684-718`.
- Why it is a fault: every load first saves the running machine as
  `vmlab-hv-rollback-<pid>-<n>` on every writable disk, with a full vmstate,
  and deletes it at the end. When the process dies in between (a `kill`, a
  host crash, the `request_exit` at `:706`, a stop from the lab daemon while
  the load runs), the checkpoint stays in the images. Nothing reads that name
  again: a search of the repository finds it at `:656` only. The lab daemon's
  records do not hold it, so `vmlab snapshot list` and `delete` cannot see or
  remove it. The image keeps a full RAM image per interrupted load, and every
  cluster the checkpoint shares stays copy-on-write.
- Proof: S1 test in `hv/crates/vmlab-hv-tests/tests/s1.rs`,
  `a_new_process_removes_a_stale_rollback_checkpoint`: start a load, kill the
  process once the checkpoint is in the snapshot table (poll
  `ops::snapshot_list`), start the machine again, and list. Expected: no
  `vmlab-hv-rollback-*` entry. Today it stays.

### F10. An offline snapshot holds the disks only (wrong result)

- Where: `src/labd/vm.rs:1826-1828` and `:2641-2668`
  (`snapshot_offline_disks`), `:1889-1891`, and the files the capture leaves
  out: `ovmf_vars()` (`:857`, `:1315-1317`) and `tpm_state()` (`:244`).
- Why it is a fault: an OVMF VM keeps its variable store in a file of its own,
  and a VM with a TPM keeps the TPM's permanent state in a directory of its
  own. An offline capture and an offline restore touch `all_disk_paths()` and
  nothing else. After a restore the disks are at the snapshot and the
  variables and the TPM are at the present. The boot entries, the secure-boot
  keys and whatever the guest sealed to the TPM no longer match the disk. A
  Windows guest that turned BitLocker on after the snapshot, or cleared the
  TPM after it, boots into recovery or not at all. An online snapshot does
  hold both (`hv/crates/vmlab-hv-devices/src/pflash/mod.rs:250-259`,
  `tpm/mod.rs:452-459`), so the two kinds differ in what a restore gives back.
- The PRD says "Offline (VM powered off): disk state only" (§7.3). So this is
  the PRD's own gap and not a slip in the code. It is listed because the card
  asks for state that is not saved, and because the user cannot see it.
- Proof: lifecycle test `an_offline_restore_returns_the_firmware_variables`:
  an OVMF VM directory with a VARS file, an offline capture, a changed byte in
  the VARS file, an offline restore, then compare. Today the changed byte
  stays. The same with a file under `tpm_state()`.

### F11. The micro-VM's PM1a block is in no section (wrong result)

- Where: `hv/crates/vmlab-hv-vmm/src/pm.rs:47-53` (`sts`, `en`, `cnt`),
  `hv/crates/vmlab-hv-vmm/src/builder.rs:1399-1404` (registered on the I/O bus
  for a machine without firmware, never pushed to `snap_nodes`).
- Why it is a fault: `PmDevice` has no `Snapshot` impl. On a q35 or pc machine
  the LPC holds these registers and saves them
  (`hv/crates/vmlab-hv-devices/src/chipset/lpc.rs:367-380`). On a direct-kernel
  machine, which is every container micro-VM, a load keeps the registers of
  the moment of the load. Two effects: a power-button status that was pending
  when the load began stays set with the SCI asserted, so the restored guest
  gets a power-button event it never had and shuts down. And a load into a
  machine whose guest has not yet written `PM1_EN` leaves the restored guest
  with the button disabled, so `powerdown` does nothing.
- Proof: S1 test `a_load_restores_the_power_button_state`: a micro-VM with an
  ACPI-aware kernel, `snapshot_save`, `pause`, `powerdown`, `snapshot_load`,
  `resume`. Expected: the guest keeps running. Unit test beside
  `power_button_sets_status_and_sci_when_enabled` (`pm.rs:194`): save, press,
  restore, assert `sts` and the SCI level.

## Notes

These have no test that I can name with confidence, or they are choices and
not faults.

- N1. `restore_kernel_state` adds the saved `ignored-msrs` to the live set
  and does not replace it (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:1038-1041`).
  After a load the set is the union of both moments. I did not read how the
  set changes what the guest sees.
- N2. A load reads the whole vmstate into one buffer
  (`snapshot.rs:749-753`), after it has written a full checkpoint. For a
  guest with much non-zero RAM the process needs about that much extra host
  memory for the time of the load.
- N3. The machine digest is checked after the disks were switched
  (`snapshot.rs:732-762`). The rollback checkpoint covers it, but a load that
  must refuse costs one full save and one full load. Any change of the
  machine description makes every online snapshot of the VM unrestorable, and
  `vmlab snapshot list` says so only for a change of hypervisor.
- N4. An online restore of a stopped machine boots it from the present disks
  first (`src/labd/vm.rs:2301-2303`, `container.rs:1496-1498`). That guest
  runs and is on the network until the load. I did not read whether `start`
  waits for readiness or runs provisions there.
- N5. On QEMU a failed `snapshot-load` is followed by a resume
  (`src/labd/vm.rs:1867-1871`) of a machine whose state QEMU does not
  promise. vmlab-hv rolls back. QEMU has no such step.
- N6. AHCI refuses a load when the optical medium differs
  (`hv/crates/vmlab-hv-devices/src/ahci/mod.rs:906-912`). A snapshot taken
  after `eject_media` cannot be loaded into a new process, which starts with
  the medium in. I did not check whether a lab VM ever ejects.
- N7. `KVM_GET_XSAVE` with the fixed 4096-byte area, `KVM_GET_SREGS` and no
  nested state (`snapshot.rs:548-553`). A guest with AMX state, or a guest
  that runs its own hypervisor, would lose state. vmlab-hv offers neither as
  far as I read.
- N8. A version 2 qcow2 has no dirty bit (`qcow2/mod.rs:950-952`), so the
  refcount rebuild after a crash inside a snapshot operation does not run for
  it. I did not check whether vmlab can produce a version 2 image.
- N9. `snapshot_apply_impl` writes the new active L1 in place
  (`qcow2/snapshot.rs:424-438`, `mod.rs:921-925`). A crash inside that write
  on an L1 of more than one cluster leaves a mixed table. QEMU does the same.
- N10. A container's `drop_snapshot` treats every state other than `Running`
  as stopped (`container.rs:1199-1207`). The VM path names `Suspended` as a
  live process. I did not check whether a container can be suspended.

## What I read

- `hv/crates/vmlab-hv-vmm/src/snapshot.rs` in full. In `vm.rs`: the snapshot
  lock, `stop_for_snapshot`, `finish_snapshot`, `wait_quiesced`, the PCI BAR
  and SMI helpers, `eject_media`, `powerdown`. In `builder.rs`: the snapshot
  registry and every `snap_nodes` and `snap_disks` push. `pm.rs`,
  `chipset.rs` (`ChipsetRegisters`), `pci.rs` (BAR state, config address),
  `control.rs` (the three commands).
- `hv/crates/vmlab-disk/src/qcow2/snapshot.rs` and `recovery.rs` in full. In
  `qcow2/mod.rs`: `flush_metadata`, the `DiskImage` snapshot methods,
  `vmstate_base`. `ops.rs`: the snapshot functions.
- Device state: the `Snapshot` and `PciSnapshot` traits, the virtio-pci
  transport, virtio-block, virtio-net, the virtio-fs passthrough session,
  AHCI, pflash, the TPM front end and its swtpm blobs, the LPC.
- `src/labd/lab.rs`: `snapshot`, `snapshot_all`, `restore`,
  `delete_snapshot`, `snapshots`. `src/labd/vm.rs`: `take_snapshot`,
  `load_snapshot`, `drop_snapshot`, `restore`, `snapshot_offline_disks`,
  `validate_snapshot_name`, the head of `boot_inner`.
  `src/labd/container.rs`: the snapshot section and `restore`.
  `src/labd/mod.rs`: the snapshot requests.
  `src/labd/hypervisor/vmlab_hv/control.rs`: `HvSnapshots` and `call`.
  `src/qmp/client.rs`: the three snapshot jobs. `src/template/qimg.rs`: both
  image tools' snapshot functions. PRD §7.3.

## What I did not read

- The save and restore of these devices: virtio-console, virtio-sound,
  virtio-gpu and the scanout, vhost-user-fs, the virtio-fs device around the
  passthrough session, IDE, floppy, e1000, pcnet, HDA, UHCI, xHCI and the USB
  keyboard and tablet, the std-VGA, Cirrus and virtio-vga displays, the RTC,
  HPET, PS/2, the ISA block, SMBus, AppleSMC, the serial port, fw_cfg, the
  SMM devices, the A20 gate, the q35 MCH and the KVM RTC. For these I only
  checked that the builder registers them.
- `snapshot/format.rs` (the stream codec) and `snapshot/audit.rs`.
- `reset.rs`, which reuses the kernel-state save for a guest reset.
- The TAP and vhost-net halves of virtio-net.
- The qcow2 refcount code (`refcount.rs`) and the cluster allocator.
- Worktree freeze, thaw, freeze-point restore and the freeze clone
  (`src/freeze/`, `src/labd/worktree.rs`, `src/labd/freeze_clone.rs`), and
  `export_overlay`.
- The workspace syncer's bracket around capture and restore
  (`src/labd/workspace/bracket.rs`, `reseed.rs`).
- The CLI (`src/cli/lab.rs`), the wscript snapshot calls, and the state file
  (`src/labd/state.rs`) apart from the fields named above.
- The existing tests, except for the names used as anchors above. A finding
  here may already have a test that I did not see.
- QEMU's own behaviour. Where a finding leans on it (F2, N5) the text says so.
