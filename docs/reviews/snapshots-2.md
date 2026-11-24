# Second review: snapshots (online, offline, restore)

Card #947. A second bug-hunting pass over the snapshot paths of vmlab and
vmlab-hv. It lists faults and makes no fix. Reviewed at `next` commit
`fec7ca65`. All paths are from the repository root, and all line numbers are
from that commit.

The first pass is `docs/reviews/snapshots.md` (card #381, findings F1 to F11,
notes N1 to N10). All eleven of its findings are marked fixed. This document
continues its numbering, so a code names one finding across both: F12 to F21
and N11 to N17 are new here. A finding of the first pass is named again only
where its fix is incomplete, and the text says which.

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**.

## How the findings were checked

Every finding is from reading the code. No vmlab code was run for this
review. Each finding names the test that would prove it. A finding whose test
fails to reproduce is wrong and should be dropped. No test below uses a
wall-clock limit: where two operations must overlap, the test blocks one of
them on a channel.

One thing was run, on helios through `stc run card-947`, in a scratch
directory that was removed afterwards. The first pass left open how
`qemu-img` resolves an all-digit snapshot name (its F2, "Not checked").
With `qemu-img` 11.1.3: `snapshot -c 2`, then `snapshot -c b`, gives the
table `id 1 / tag 2` and `id 2 / tag b`. `snapshot -d 2` then removed the
snapshot **tagged** `2` and kept `b`. So the fallback image tool
(`src/template/qimg.rs:381-394`) resolves a name as a name where one
matches, and vmlab checks that the name exists before each apply and delete
(`src/labd/vm.rs:3090-3097`, `:2195-2203`). I found no fault there.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F12 | data loss | A freeze-point restore replaces a machine's disks with no reservation against a start or a destroy of that machine (F1's fix does not cover this path) |
| F13 | data loss | The lab-wide restore's check before the first rewind asks the records only, so a machine-level refusal still stops it half way (F4 fix incomplete) |
| F14 | wrong result | An online restore that is refused has already unplugged the machine's host USB devices and marked its workspace as rewound, so a guest tree that was never rewound is overwritten from the host |
| F15 | wrong result | On a running vmlab-hv machine, deleting a snapshot that one disk lacks removes it from the other disks, returns an error and keeps the record |
| F16 | wrong result | A freeze-point restore rewinds the disks and leaves the UEFI variables and the TPM state at the present (F10 fix incomplete) |
| F17 | wrong result | A container's offline restore and a freeze-point restore stop the machine before they check that the restore can be done (F3 fix incomplete) |
| F18 | wrong result | The recovery snapshot of an offline restore outlives a daemon that dies during it, on every disk, and no vmlab command lists or removes it |
| F19 | wrong result | A destroy that crosses an offline capture leaves a snapshot record for disks that are gone, and a destroy never clears the pending captures |
| F20 | wrong result | A capture that the backend refuses before any work still leaves a pending entry: a VM that never started, and every refusal vmlab-hv makes itself (F7 fix incomplete) |
| F21 | cosmetic | A user snapshot may be named `vmlab-freeze-hold`, after which freeze and freeze-point restore refuse the machine and blame an interrupted run |

### F12. A freeze-point restore is not reserved against a start or a destroy (data loss)

- Where: `src/labd/lab.rs:4147-4148` (the freeze-point branch),
  `:4149-4163` (the ordinary branch, which is admitted through the start
  gate), `src/labd/worktree.rs:269-296` (`restore_freeze_point`),
  `src/freeze/plan.rs:502-577` (the steps), `src/labd/vm.rs:1421-1428`
  (`boot_inner`).
- Why it is a fault: the ordinary restore runs under two reservations. The
  lab takes a ticket and `self.starts.admit(m.name(), ticket)`, which holds
  the machine's lock of the start gate for the whole rewind
  (`src/labd/start_gate.rs:14-18`: every start and every destroy takes that
  lock). The machine takes its snapshot gate, which `boot_inner` also tries
  (the fix of F1). The freeze-point branch takes neither. It calls
  `restore_freeze_point`, which stops the machine and then runs a plan that
  creates a hold snapshot on each disk, builds a fresh overlay, exports and
  commits every later snapshot into it, and renames the old overlay away and
  the fresh one into place. From the stop to the last rename the machine is
  `Stopped` and nothing says it is busy. The lab daemon serves requests
  concurrently, so a `vmlab up` or `vm start` in that window is admitted by
  the start gate, passes the snapshot gate, and boots on the overlay that the
  plan is still reading (`Step::Export` of each carried snapshot) and is about
  to rename to `.old` and remove. The guest then writes to a file that the
  plan deletes in its cleanup, or two writers hold one qcow2, which is F1's
  damage. A `vm destroy` in the same window removes the directory under the
  plan.
- I did not find a lock on this path. `CLAUDE.md` says freeze-point restore
  runs "under the state lock", but `restore_freeze_point` takes none, and a
  start does not need the state lock to reach `boot_inner`. If a caller above
  `restore_to_its_end` holds a lock that a start also takes, this finding is
  wrong.
- Proof: lifecycle test in `src/labd/lifecycle_tests.rs`,
  `a_freeze_point_restore_excludes_a_concurrent_start`: a frozen lab with one
  machine, an image tool whose `export_overlay` (or `snapshot_create`) blocks
  on a channel, the restore of the freeze point started, then `start` of the
  machine. Expected: the start refuses or waits until the restore is over.
  Today it launches the hypervisor. The same test with `destroy_machine` in
  place of `start`: expected that it waits.

### F13. The lab-wide restore still stops half way on a machine-level refusal (data loss)

- First pass: F4, fixed in #389 by running `check_restore` for every machine
  before any rewind. That check reads the records, the pin, the hypervisor
  and the workspace. It does not ask the machines. The fix is incomplete.
- Where: `src/labd/lab.rs:4031-4045` (`restore_all`), `:4217-4273`
  (`check_restore`), and the refusals that come later:
  `src/labd/vm.rs:2599-2601` and `src/labd/container.rs:1604-1606` (the
  online refusal), `src/labd/vm.rs:3090-3097` (a disk lacks the snapshot),
  `src/labd/vm.rs:2153` and `src/labd/container.rs:1283` (an offline
  snapshot operation is in progress), `src/freeze/plan.rs:464-500` (the
  freeze-point refusals), and vmlab-hv's own
  (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:684-707`, `:895-900`).
- Why it is a fault: each of these refuses one machine after the machines
  before it in the loop were rewound, and `restore_all` returns at the first
  error. The lab is then at two points in time, which is what F4 described.
  Plain ways to get there:
  - an `extra_disk` was added to one VM after the lab-wide snapshot. Its
    record exists, so `check_restore` passes. Its restore refuses with
    "snapshot is missing" on the new disk.
  - a machine's online snapshot can no longer be loaded: a virtiofs share or
    a `gpu {}` was added (the online refusal), or any other change of the
    machine description (the digest check of vmlab-hv, first-pass N3).
  - a freeze point that one machine's overlay cannot restore because it holds
    an online snapshot taken since.
- Proof: test beside `restore_offline_disks`'s tests in `src/labd/vm.rs`,
  with two real VM directories under one fake lab,
  `a_lab_wide_restore_asks_every_machine_before_any_moves`: snapshot `s`
  recorded on `a` and `b`, a second disk of `b` that has no `s`, data written
  to `a` after the snapshot, then the lab-wide restore. Expected: an error
  and `a`'s disk unchanged. Today `a` is rewound. With the fake machine of
  `src/labd/lab.rs:5358`: give it a "can this restore run" answer that fails
  for `b`, and assert no `restore` call on `a`.

### F14. A refused online restore has already unplugged USB and ordered a re-seed (wrong result)

- Where: `src/labd/lab.rs:4123-4125` (`usb_release_for`), `:4136-4139`
  (`mark_rewound`), `:4155-4159` (the call into the machine), and the refusal
  it meets there: `src/labd/vm.rs:2597-2601`, `src/labd/container.rs:1602-1606`.
- Why it is a fault: `online_snapshot_refusal` is a check that needs nothing
  to have moved. Its own comment says "Refused before anything moves". But it
  sits inside `Machine::restore`, and the lab has done two things before it
  calls that:
  - every host USB device of the machine was detached. The restore is then
    refused and the guest has lost its devices for nothing. The user has to
    attach each one again.
  - the workspace was marked as rewound. The comment at `:4127-4135` accepts
    this for a rewind that fails half way, because nobody can tell then
    whether the guest moved. A refusal is not that case: the guest did not
    move. The re-seed still runs when the syncer comes back (`:4178-4196`
    says so in an event). A re-seed is host-only and carries the host's tree
    onto the guest. With `--discard-guest-changes` the pre-flight flush was
    skipped (`src/labd/workspace/syncer.rs:616-618`), so guest work that the
    host never saw is overwritten, and the snapshot the user gave it up for
    was not restored. The work is lost and the guest is where it was.
  The same holds for a refusal of the start gate (`:4153-4161`).
- Proof: test in `src/labd/lab.rs` beside the restore tests,
  `a_refused_online_restore_changes_nothing`: a fake machine with one USB
  attachment and an online record, whose `restore` returns the online
  refusal (or a new "can restore" answer that refuses). Expected after the
  restore: the attachment is still there, and the workspace ledger has no
  re-seed note. Today the device is detached and the note is set.

### F15. A running delete of a snapshot one disk lacks is half done and keeps the record (wrong result)

- Where: `hv/crates/vmlab-hv-vmm/src/snapshot.rs:1010-1030` (`delete`),
  `src/labd/hypervisor/vmlab_hv/control.rs:472-483`, `src/labd/vm.rs:2178-2187`,
  `src/labd/lab.rs:4363-4370`.
- Why it is a fault: vmlab-hv's delete walks every writable disk, deletes the
  snapshot where it finds it, and returns `snapshot_not_found` when any disk
  had none. The lab accepts that answer only when there is no record (the
  pending cleanup of F7). With a record it returns the error and keeps the
  record. So for a VM that got an `extra_disk` after the snapshot was taken,
  `vmlab snapshot delete s` on the running machine removes `s` from the old
  disks, prints an error, and leaves `s` in `vmlab snapshot list`. Every
  later delete finds nothing on any disk and fails the same way. A later
  `vmlab snapshot restore s` is refused, because the disks no longer hold it.
  The record goes away only when the user stops the machine and deletes
  again: the offline branch skips a disk that lacks the name
  (`src/labd/vm.rs:2195-2203`). The two branches of one verb disagree, and the
  running one destroys the snapshot while it reports that it did not.
- Proof: test in `src/labd/lab.rs` beside
  `pending_capture_is_durable_and_cannot_be_replaced_or_restored`,
  `a_delete_that_finds_no_snapshot_on_one_disk_removes_the_record`: a fake
  machine with a record `s` whose `delete_snapshot` answers with
  `CommandError::not_found`, as vmlab-hv does. Expected: `Ok` and no record.
  Today: the error, and the record stays. Lower down, in
  `hv/crates/vmlab-hv-vmm/src/snapshot_tests.rs`: two writable disks, `s` on
  the first alone, `delete(handle, "s")`, then list the first disk. This
  shows that the delete was done before the error was returned.

### F16. A freeze-point restore leaves the UEFI variables and the TPM at the present (wrong result)

- First pass: F10, fixed in #437 for the offline restore of an ordinary
  snapshot. A freeze point is restored through another path, which the fix
  did not touch.
- Where: `src/labd/worktree.rs:269-296`, `src/freeze/plan.rs:458-577` (disk
  steps only), `src/freeze/plan.rs:179-190` (`CARRIED`: what a freeze copies
  beside the disks).
- Why it is a fault: a freeze copies `OVMF_VARS.fd`, `OVMF_VARS.boot` and
  `tpm-state/` of each machine into `.vmlab/freeze/<n>/<machine>/`, so the
  state at the freeze point exists. `restore_freeze_point` plans disk steps
  from `disk_facts` and nothing else. After it the disks are at the freeze
  point and the variables and the TPM are at the present, which is the
  mismatch F10 described: boot entries, secure-boot keys and whatever the
  guest sealed to the TPM since the freeze no longer match the disk. The PRD
  says a freeze point "resets a lab" (§21.3).
- Proof: lifecycle test `a_freeze_point_restore_returns_the_firmware_variables`:
  an OVMF VM directory with a VARS file, a freeze, a changed byte in the VARS
  file, the restore of the freeze point, then compare with the copy under
  the freeze. Today the changed byte stays. The same with a file under
  `tpm-state/`.

### F17. Two restores stop the machine before they check (wrong result)

- First pass: F3, fixed in #388: "Restore checks each disk before stopping
  the VM". The fix is in the VM's offline branch only.
- Where: `src/labd/container.rs:1282-1295` (`restore_offline`: the stop
  ladder, then `snapshot_apply` with no check), `src/labd/worktree.rs:275-292`
  (`restore_freeze_point`: the stop, then `disk_facts` and every refusal of
  `plan_point_restore`).
- Why it is a fault: a restore that cannot be done should leave a running
  machine running. A container whose scratch disk lacks the recorded snapshot
  is stopped and then gets the image tool's error. A freeze-point restore
  that must refuse (the overlay holds an online snapshot, a disk was not part
  of the freeze, a `vmlab-freeze-hold` is on a disk) stops the machine
  first and refuses second. In both the machine stays stopped and nothing was
  restored. In a lab-wide restore this is one more way into F13.
- Proof: for the container, a lifecycle test with the fake hypervisor,
  `a_container_restore_that_cannot_apply_leaves_it_running`: a running
  container, an offline record `s`, a scratch disk without `s`, the restore.
  Expected: an error and the power state `Running`. For the freeze point:
  a fake machine in state `Running` and disks for which
  `plan_point_restore` refuses, then assert that `stop` was not called.

### F18. The recovery snapshot of an offline restore outlives a dead daemon (wrong result)

- Where: `src/labd/vm.rs:3099-3104` (the name and the capture),
  `:3107-3145` (the only place that deletes it).
- Why it is a fault: this is F9 again, on the offline path that the fix of F3
  added. Every offline restore first creates `vmlab-restore-<32 hex digits>`
  on every disk and deletes it at the end of `apply_offline_snapshot`. When
  the lab daemon dies in between (a kill, a host crash, an out-of-memory
  kill), the name stays in every image. Nothing reads it again: a search of
  the repository for `vmlab-restore-` finds line 3100 only. It is in no
  record, so `vmlab snapshot list` does not show it, and `vmlab snapshot
  delete` answers that there is no such snapshot. Every cluster it shares
  stays copy-on-write for good, so the image grows by everything the guest
  rewrites. vmlab-hv's own leftover got a start-up sweep and a reserved
  prefix in #421. This one has neither, and its name is inside the grammar of
  user names (`validate_snapshot_name`, `:3176-3185`), so a sweep by prefix
  could also remove a user's snapshot. A freeze-point restore that runs later
  carries the leftover into the fresh overlay as if it were a user's
  (`src/freeze/plan.rs:529-548`).
- Proof: unit test beside `restore_offline_disks`'s tests,
  `an_interrupted_offline_restore_leaves_no_recovery_snapshot`: one qcow2 with
  snapshot `s`, run `snapshot_offline_disks(disks, "vmlab-restore-0")` as the
  restore does and stop there (the daemon died), then run what a daemon runs
  for a machine at start-up and list. Expected: no `vmlab-restore-*` entry.
  Today there is no such start-up step, so the entry stays.

### F19. A destroy crosses a capture, and never clears the pending ones (wrong result)

- Where: `src/labd/lab.rs:3913-4002` (`snapshot`: no hold on the start gate),
  `:1766-1806` (`destroy_machine`: holds the start gate, clears `snapshots`
  at `:1803`), `src/labd/container.rs:1697-1701`.
- Why it is a fault, in two parts:
  - A restore is held against a destroy of its machine by the start gate
    (`:4149-4163`). A capture is not. An offline capture does its disk work
    in a task of its own (`finish_offline_snapshot`,
    `src/labd/machine.rs:81-91`). A `vm destroy` of the same machine at that
    moment stops nothing (the machine is stopped), removes the machine's
    directory, and clears the records. The image tool finishes on files that
    are unlinked, the capture returns `Ok`, and `snapshot` then inserts the
    record (`:3965-3977`) for a machine that has no disks. After the next
    `up` the list shows a snapshot that the new disks do not hold.
  - `destroy_machine` clears `snapshots` and leaves `pending_snapshots`. A
    name that was pending before the destroy stays taken for the machine that
    `up` builds next: a capture under it is refused with "already exists or
    is pending cleanup", and the list of a fresh machine shows an
    "interrupted capture".
- Proof: tests in `src/labd/lab.rs` with the fake machine:
  `a_destroy_waits_for_a_capture_under_way` (the fake's `snapshot` blocks on a
  channel, `destroy_machine` is started, the channel is released, and the
  state must hold no record of the name), and
  `a_destroy_clears_the_pending_captures` (a state with one pending name,
  `destroy_machine`, then `pending_snapshots` must be empty).

### F20. Refusals that still leave a pending entry (wrong result)

- First pass: F7, fixed in #420: "Preflight refusals clear the durable
  capture intent". Two groups of refusals are still classed as an attempt.
- Where: `src/labd/vm.rs:2088-2098` (the stopped branch maps every error to
  `Attempted`), `:3034-3041` and `:3147-3158` (what fails first),
  `:2079-2086` (the running branch maps every answer of the hypervisor to
  `Attempted`), `hv/crates/vmlab-hv-vmm/src/snapshot.rs:337-389` (what
  vmlab-hv refuses before it stops the guest), `src/labd/lab.rs:3955-3963`.
- Why it is a fault:
  - a VM that was never started has no disk files, because they are created
    at the first start (`ensure_disks`, `src/labd/vm.rs:830-881`).
    `vmlab snapshot take` on it fails in `image_info` of the first disk, before anything was written.
    The container path refuses this case by name (`container.rs:1209-1216`).
    The VM path reports a failed attempt and keeps the pending entry.
  - on a running vmlab-hv machine, `save` refuses a reserved name, an
    assigned VFIO device, an attached host USB device, an accelerated GPU, a
    name that is too long and a name that a disk already holds, all before
    `stop_for_snapshot`. Each comes back as `unsupported` and is kept as a
    pending entry.
  In each case the name is then taken, a second `take` says "already exists
  or is pending cleanup", and the user has to run a delete for a capture that
  never began. For the last refusal (a disk already holds the name, which no
  record knows) the delete that the list asks for removes that older
  snapshot. The comment at `lab.rs:3959-3960` says the code must not do that
  blindly, and then the user is told to.
- Proof: test in `src/labd/lab.rs`,
  `a_capture_of_a_never_started_vm_leaves_no_pending_entry`: a real
  `VmInstance` directory with no disk file, `lab.snapshot`, then assert an
  error and an empty `pending_snapshots`. For vmlab-hv: with the fake
  protocol server of `src/labd/hypervisor/fake.rs` answering `snapshot_save`
  with `unsupported`, assert the same.

### F21. `vmlab-freeze-hold` can be a user's snapshot (cosmetic)

- Where: `src/labd/vm.rs:3176-3185` (`validate_snapshot_name`: no reserved
  names), `src/freeze/plan.rs:31`, `:262-268`, `:481-487`.
- Why it is a fault: the name `vmlab-freeze-hold` is inside the grammar, so
  `vmlab snapshot take vmlab-freeze-hold` works. From then on `vmlab worktree
  freeze` and the restore of a freeze point refuse that machine and say that
  the disk still holds the snapshot that an interrupted run left behind. Nothing was interrupted. vmlab-hv solved the same clash for
  its checkpoint with a prefix outside the grammar
  (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:308-310`).
- Proof: unit test beside `validate_snapshot_name`,
  `a_name_vmlab_keeps_for_itself_is_refused`: `validate_snapshot_name`
  on `vmlab-freeze-hold` must fail. Today it passes.

## Notes

These have no test that I can name with confidence, or they are choices and
not faults.

- N11. `LabRuntime::delete_snapshot` holds the lab's state lock from its
  first line to its last (`src/labd/lab.rs:4319-4383`), over the call into
  the machine. On a running vmlab-hv machine that call waits for the one
  command connection (`src/labd/hypervisor/vmlab_hv/control.rs:207-220`),
  which an online save or load of that machine holds for as long as it runs.
  Every request that needs the state waits with it. I did not list which
  requests those are.
- N12. vmlab-hv's `delete` does not take the snapshot-operation lock that
  `save` and `load` take (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:1010`
  against `:343` and `:677`). vmlab's single command connection serialises
  the three today. A second client of the control socket could delete a
  snapshot between the disks of a save or a load.
- N13. A capture whose caller goes away is not run to its end, as a restore
  is (`src/labd/lab.rs:4076-4091` against `:3913`). A cancelled provision
  drops the future. The backend finishes the snapshot, no record is written,
  and the name is listed as an interrupted capture. A delete removes it. No
  data is lost.
- N14. After a container's online load, a failed `Resync` to cinit fails the
  whole restore with `?` (`src/labd/container.rs:1253-1255`), though the
  guest was restored and resumed. The lab then says the restore failed, emits
  no `snapshot.restored`, and does not set the clock.
- N15. The lab-wide restore resumes each online machine as soon as it is
  loaded (`src/labd/lab.rs:4039-4043`). The first machines run at the
  snapshot's time and talk on the lab network to machines that are still at
  the present. The PRD says capture times are uncoordinated (§7.3). It does
  not say that of the restore.
- N16. A snapshot taken before a VM's first start holds no variable store,
  and its directory under `snapshot-state/` says so. A restore of it removes
  the VM's store (`src/labd/snapshot_state.rs:176-184`). This is the stated
  rule. With F20's first case fixed it can be reached. Today such a capture
  fails.
- N17. First-pass N4 still holds, and I can add to it: both online restores
  boot a stopped machine through the normal `start`
  (`src/labd/vm.rs:2604-2606`, `src/labd/container.rs:1611-1613`). For a
  container this runs the image's entrypoint against the host volumes before
  the load. Volume contents are host state and do not roll back (§18), so
  what that short run writes there stays.

## What I read

- `docs/reviews/snapshots.md` in full, and the code each of its fixes named.
- `src/labd/lab.rs`: `snapshot`, `snapshot_all`, `restore_all`, `restore`,
  `restore_to_its_end`, `check_restore`, `set_clock_after_restore`,
  `delete_snapshot`, `snapshots`, `ActiveCapture`, and `destroy_machine`.
- `src/labd/vm.rs`: `online_snapshot_refusal`, `take_snapshot`,
  `load_snapshot`, `drop_snapshot`, the `Machine::restore` impl,
  `capture_offline`, `restore_offline`, `restore_offline_disks`,
  `apply_offline_snapshot`, `snapshot_offline_disks`,
  `validate_snapshot_name`, `all_disk_paths`, the head of `boot_inner`.
- `src/labd/container.rs`: the snapshot section, `restore`,
  `forget_artefacts`. `src/labd/machine.rs`: `SnapshotCaptureError`,
  `snapshot_guard`, `finish_offline_snapshot`.
- `src/labd/snapshot_state.rs` up to `copy_tree`.
- `src/labd/worktree.rs`: `restore_freeze_point`, `take_refusal`.
  `src/freeze/plan.rs`: `plan_point_restore`, `CARRIED`, the uses of
  `HOLD_SNAPSHOT`.
- `src/labd/start_gate.rs`: the module text and the function list.
  `src/labd/usb.rs`: `usb_release_for`.
  `src/labd/workspace/syncer.rs`: `before_restore`.
- `src/labd/hypervisor/vmlab_hv/control.rs`: `HvSnapshots`, `send`, `call`,
  `refused`. `src/labd/state.rs`: `hypervisor_refusal`.
- `hv/crates/vmlab-hv-vmm/src/snapshot.rs`: the head, `remove_stale_checkpoints`,
  `save`, `save_locked`, `capture_and_write`, `stream_ram_out`,
  `save_kernel_state`, `load`, `load_locked`, `load_checkpoint_name`,
  `delete_checkpoint`, `restore_with_rollback`, `restore`, `delete`. In
  `vm.rs`: `begin_snapshot`, `stop_for_snapshot`, `finish_snapshot`.
- `hv/crates/vmlab-disk/src/qcow2/snapshot.rs`: the lookups by name and
  `snapshot_create_impl`. `src/template/qimg.rs`: the snapshot functions of
  both image tools.

## What I did not read

- Everything the first pass lists as not read and that is not named above.
  In particular the save and restore of the single devices, the stream codec
  (`snapshot/format.rs`), `reset.rs`, and the qcow2 refcount code.
- The workspace bracket in depth: `src/labd/workspace/bracket.rs`,
  `reseed.rs`, `before_capture`, `suspend`, `resume` and `mark_rewound`. F14
  rests on the comments in `lab.rs` about what a re-seed does, and on
  `before_restore`.
- Freeze and thaw themselves (`plan_freeze`, `plan_thaw`,
  `src/freeze/steps.rs`, `inspect.rs`) and the freeze clone. F12 and F16 are
  about the restore of a freeze point only.
- The QEMU adapter (`src/labd/hypervisor/qemu.rs`) and the QMP snapshot jobs.
  QEMU runs aarch64 and riscv64 guests only now.
- The CLI (`src/cli/lab.rs`) and the wscript bindings beyond their call
  sites, and how the protocol server treats a client that disconnects. N13
  rests on the comment at `lab.rs:4069-4075`.
- `restore_kernel_state` and the rest of `snapshot.rs` after `delete`.
- The existing tests, except for the names used as anchors above. A finding
  here may already have a test that I did not see.
