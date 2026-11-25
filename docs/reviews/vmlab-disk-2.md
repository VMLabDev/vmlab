# Second review: vmlab-disk and qcow2

Card #984. A second bug-hunting pass over `hv/crates/vmlab-disk` (qcow2 read
and write, backing chains, internal snapshots, recovery, the image operations
that replace `qemu-img`) and over the vmlab code that calls it. It lists faults
and makes no fix. Reviewed at `next` commit `84fc31e0`. All paths are from the
repository root, and all line numbers are from that commit.

The first pass is `docs/reviews/vmlab-disk.md` (card #531, findings F1 to F11,
notes N1 to N12). All eleven of its findings are marked fixed. This document
continues its numbering, so a code names one finding across both: F12 to F21
and N13 to N22 are new here. A finding of the first pass is named again only
where its fix is incomplete, and the text says which.

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**.

## How the findings were checked

Findings F12 to F18, F20 and F21 were reproduced on helios with the
`vmlab-disk` binary of commit `84fc31e0` (debug build) against `qemu-img` and
`qemu-io` 11.1.3. F19 is from the code, with one QEMU run that shows how QEMU
reads the result. Each finding gives the commands that showed it and the test
that should hold it. The commands ran in a scratch directory under the build
cache of the card, which was removed afterwards. No file of the repository
changed. A finding whose test fails to reproduce is wrong and should be
dropped. No test below uses a wall-clock limit. The entries under "Notes" are
from reading, except where one says what was measured.

Two random comparisons were also run, to look for faults that reading missed.
Both are described under "What I read and what I did not". They found F13 and
F20 again and nothing else.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F12 | wrong result | A backing chain of more than 9 files is refused, and nothing limits how often a lab is frozen |
| F13 | wrong result | Refcount recovery refuses an image whose last compressed cluster ends inside the last sector of the file, so the image cannot be opened for writing (F7 fix incomplete) |
| F14 | data loss | `convert` deletes an existing destination before it knows that the source opens |
| F15 | wrong result | `resize` to 2 EiB or more succeeds without growing the L1 table, and QEMU then refuses the image |
| F16 | wrong result | `create` writes a backing file name of more than 1023 bytes, which no open accepts |
| F17 | wrong result | A safe rebase with a relative backing path compares against one file and records a name that resolves to another |
| F18 | wrong result | `create` and `convert` accept a size that is not a multiple of 512, and the last part of a sector is cut off |
| F19 | cosmetic | An online snapshot of a version 2 image writes zero-flag entries, which QEMU reports as corruption |
| F20 | cosmetic | Applying a snapshot with a shorter L1 table leaks the clusters of the longer table |
| F21 | cosmetic | A failed `create` leaves a file at the destination, and `up` takes an existing file as the machine's disk |

F14 has the strongest severity word and is third, because no vmlab caller
gives it a destination that holds data which is not about to be replaced. F12
stops a freeze, or a machine from starting, after a number of freezes that a
user can reach. F13 stops a writable open and needs an image that QEMU wrote
in a way that is not common.

### F12. A backing chain of more than 9 files is refused (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:32` (`MAX_BACKING_DEPTH`),
  `:222-228` (`open_depth`), `src/freeze/plan.rs:548-563` (a freeze puts a new
  overlay on the disk it froze), `src/freeze/inspect.rs:60-62` (vmlab's own
  walk follows 64 files), `hv/crates/vmlab-hv-vmm/src/factory.rs:43`.
- Why it is a fault: the limit exists to stop a loop of backing files. It is
  8 levels under the top image, which is 9 files. QEMU has no such limit.
  Every `vmlab worktree freeze` adds one file to the chain of each disk of
  the main lab: the disk becomes the frozen file of the generation and a new
  overlay takes its place. A machine disk starts with 2 files (base and
  disk) or 3 (base, variant overlay and disk). So the seventh or eighth
  freeze of one lab fails at its overlay step with "backing chain deeper
  than 8". No vmlab code counts the chain before the freeze starts, so
  the user learns the limit from a failed step and a rollback. With
  `VMLAB_IMAGE_TOOL=qemu-img` the freeze succeeds, because `qemu-img` builds
  the longer chain. vmlab-hv opens every disk with vmlab-disk, so that machine
  then fails to start with the same message, and `thaw` is the only way back.
- Shown by: `vmlab-disk create l0.qcow2 --size 1048576`, then
  `vmlab-disk clone l<n-1>.qcow2 l<n>.qcow2` for n from 1. The clone `l9`
  exits 1 with `backing chain deeper than 8`. `vmlab-disk read l8.qcow2 0 4`
  works. Two more overlays made with `qemu-img create -b` on `l8.qcow2`:
  `qemu-io -r -c "read -v 0 4"` of the top one prints the data of `l0`, and
  `vmlab-disk read` of it exits 1.
- Proof: unit test in `hv/crates/vmlab-disk/src/ops.rs`,
  `a_chain_of_twelve_overlays_opens`: `create_qcow2` and eleven
  `create_linked_clone` calls, then a read of the top image. Expected: the
  data of the first file. Today: the tenth file cannot be made. A loop must
  still be refused: a test beside it gives two images each other as backing
  and expects an error, which a set of visited files (device and inode)
  gives without a depth limit. When the limit stays, the test is a plan test
  in `src/freeze/plan.rs`: a freeze of a disk whose chain is at the limit is
  refused before the first step, with a message that names `thaw`.

### F13. Recovery refuses a compressed cluster that ends the file (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/recovery.rs:154-160` (`count_view`,
  the compressed branch) with `:121-132` (`count_range` refuses a range past
  the end of the file), `hv/crates/vmlab-disk/src/qcow2/mod.rs:231-239`
  (recovery runs on every writable open of a version 2 image and of a dirty
  version 3 image). The read path at `mod.rs:706-719` has the fix of F7.
- Why it is a fault: this is the cause of F7 in a second place. A compressed
  descriptor counts whole sectors, and QEMU writes only the compressed bytes.
  When the compressed cluster is the last thing in the file, the file ends
  inside the last sector. Card #562 made the read stop at the end of the file.
  Recovery still counts the range up to the end of the last sector, and
  `count_range` returns "qcow2 reference beyond end of file" for it. The image
  then cannot be opened for writing: no start, no snapshot operation, no
  rebase. A read-only open works. No data is lost, and QEMU opens and writes
  the image. The image is in this state when its newest allocation is a
  compressed cluster that QEMU wrote through its block layer: `qemu-io`, or
  the target of a backup job with compression. `qemu-img convert -c` pads the
  end of the file to a sector (run: three images, each a multiple of 512
  bytes long, and each opens for writing), so its images are not affected.
  The image must also be version 2, or version 3 with the dirty bit. vmlab
  opens a template base read-only, so this needs such an image as a disk that
  is written: a disk that a user brings to a machine, or the `vmlab-disk`
  CLI.
- Shown by: `qemu-img create -f qcow2 -o compat=0.10 b.qcow2 64M`,
  `qemu-io -c "write -c -P 3 0 64k" b.qcow2` (file length 327759),
  `vmlab-disk read b.qcow2 0 16` prints 16 bytes of `0x03`,
  `vmlab-disk write b.qcow2 65536 aa` fails with `unsupported: qcow2
  reference beyond end of file`. `qemu-io -c "write -P 9 65536 512"` writes
  to the same image. The same failure for a version 3 image with header byte
  79 set to 1 (the dirty bit).
- Proof: S2 test in `hv/crates/vmlab-disk/tests/s2_qemuimg.rs`,
  `a_dirty_image_that_ends_in_a_compressed_cluster_opens`: the two commands
  above, then `Qcow2Image::open(path, false)`, one `write_at` to another
  cluster and `flush`. Expected: success, `qemu-img check` exits 0, and the
  first cluster still reads 64 KiB of `0x03`. Today: the open fails. The same
  test for a version 3 image with the dirty bit set in the header.

### F14. `convert` deletes the destination before the source opens (data loss)

- Where: `hv/crates/vmlab-disk/src/ops.rs:354-369` (the destination is
  removed), `:373-374` and `:384` (the source is opened after that).
- Why it is a fault: card #543 (F11) made `convert` refuse a destination that
  is one of the images it reads. For every other destination the order is
  still: remove the destination, then open the source. When the source does
  not exist, is not readable, has a header that vmlab-disk refuses (a
  compression type, an external data file, a chain over the limit of F12), or
  has no snapshot of the given name, `convert` returns the error and the old
  destination is gone. `qemu-img convert` opens the source first and leaves
  the destination as it was. In vmlab the destinations are a new working disk
  and a new overlay, so today this loses a file only for the `vmlab-disk` CLI
  and for a caller that converts over a file it wants to keep on failure. The
  first review's open point under F11 (an interrupted convert leaves a part
  of an image) has the same cause: the image is written in place.
- Shown by: `vmlab-disk create keep.qcow2 --size 1048576`, a write,
  `vmlab-disk convert nosuch.qcow2 keep.qcow2` exits 1 with `No such file or
  directory`, and `keep.qcow2` no longer exists. The same with
  `vmlab-disk convert l0.qcow2 keep2.qcow2 --snapshot nosuch`: exit 1 with
  `snapshot not found: nosuch`, and `keep2.qcow2` is gone.
- Proof: unit tests in `ops.rs`,
  `convert_from_a_missing_source_keeps_the_destination` and
  `convert_of_a_missing_snapshot_keeps_the_destination`: a destination made
  with the `patterned` helper, the failing convert, then `first_bytes(dest)`.
  Expected: the pattern. Today: the file is gone.

### F15. `resize` to 2 EiB or more does not grow the L1 table (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:1438-1444`
  (`ensure_l1_covers`, the cast to `u32` at `:1439`), `:1345-1364`
  (`resize_grow`), `hv/crates/vmlab-disk/src/qcow2/refcount.rs:427-432` (the
  limit of card #555, which the cast runs before).
- Why it is a fault: `ensure_l1_covers` computes the number of L1 entries as
  a 64-bit value and casts it to `u32` before it compares it with the limit.
  With 64 KiB clusters one entry covers 512 MiB, so a size of 2^61 bytes
  needs 2^32 entries, which the cast turns into 0. The function then finds
  the L1 table long enough. `resize_grow` writes the new size into the header
  and returns success. The image now says 2 EiB with an L1 table for the old
  size. `qemu-img info` refuses it with "Image is too big". vmlab-disk reads
  zeros from the new area and fails every write to it with "write past L1
  coverage". A size between the limit (2 PiB) and 2^61 is refused as card
  #555 intends. Above 2^61 the entry count wraps, so a size is accepted when
  the low 32 bits of the count are within the limit. This is the part of the first review's N6 that card #555
  left open on the resize path. A template's disk size comes from the lab
  file, so the value needs a user error of this size.
- Shown by: `qemu-img create -f qcow2 r.qcow2 1M`,
  `vmlab-disk resize r.qcow2 2305843009213693952` exits 0,
  `vmlab-disk info r.qcow2` reports that virtual size, `qemu-img info
  r.qcow2` fails with `Image is too big`, and `vmlab-disk write r.qcow2
  1073741824 aa` fails with `write past L1 coverage`. A resize to 2^62 also
  exits 0. A resize of a new image to 2^60 exits 1 with the limit message.
- Proof: unit test in `qcow2/mod.rs`,
  `a_resize_past_the_l1_limit_is_refused_for_every_size`: for the sizes
  `1 << 60`, `1 << 61` and `1 << 62` on a `testsupport::blank` image,
  `resize_grow` returns an error and `size()` is unchanged. Today the second
  and third calls succeed.

### F16. `create` writes a backing name that no open accepts (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:1164-1175` (`create` checks
  the name against the cluster only), `:445-450` (`parse` refuses a name over
  1023 bytes), `:1286` (`set_backing` has the check),
  `hv/crates/vmlab-disk/src/ops.rs:86-96`.
- Why it is a fault: the format limits the backing file name to 1023 bytes.
  `parse` and `set_backing` hold that limit. `create` only checks that the
  name fits in the header cluster, which is 64 KiB. A path on Linux can have
  4095 bytes. `create_linked_clone` makes the path absolute, so a store or a
  lab directory deep in the file system gives a clone whose create succeeds
  and whose every open then fails with "backing file name too long". The
  machine's disk exists and cannot be used. `qemu-img create` refuses such a
  name.
- Shown by: a copy of a qcow2 image at a path of 1173 bytes,
  `vmlab-disk clone <that path> lc.qcow2` exits 0, and
  `vmlab-disk info lc.qcow2` fails with `not a qcow2 image: backing file name
  too long (1173)`. The same for `vmlab-disk create --backing`.
- Proof: unit test in `ops.rs`,
  `a_clone_of_a_backing_path_over_the_name_limit_is_refused`: a base image
  under nested directories with a path of 1100 bytes, then
  `create_linked_clone`. Expected: an error, and no file at the destination.
  Today: success, and `Qcow2Image::open` of the clone fails.

### F17. A safe rebase resolves a relative backing path in two ways (wrong result)

- Where: `hv/crates/vmlab-disk/src/ops.rs:117-144` (`rebase_safe`: the new
  backing is opened at `:123` with the path as given, and the same string is
  recorded at `:141-142`), `hv/crates/vmlab-disk/src/qcow2/mod.rs:1638-1648`
  (`resolve_backing` resolves a recorded name against the directory of the
  image).
- Why it is a fault: `rebase_safe` opens the new backing file with the path
  as the caller gave it, which the operating system resolves against the
  working directory. It compares the image with that file and writes the
  clusters that differ. Then it records the same string in the header. Every
  later open resolves the string against the directory of the image. When the
  image is not in the working directory, these are two different files. The
  image was made consistent with one file and reads through the other: a
  cluster that the two files do not share reads wrong data without an error,
  and when the second file does not exist the image cannot be opened.
  `qemu-img rebase` resolves the name against the directory of the image in
  both places. The freeze step gives an absolute path
  (`src/freeze/plan.rs:558-563`), so this is a fault of the operation and of
  the CLI today.
- Shown by: `old.qcow2` and `new.qcow2` in the working directory,
  `sub/new.qcow2` with other data, `sub/top.qcow2` with the absolute path of
  `old.qcow2` as backing. `vmlab-disk rebase sub/top.qcow2 --backing
  new.qcow2 --format qcow2` exits 0. `vmlab-disk info sub/top.qcow2` then
  reports `"backing-filename": "new.qcow2"` and `"full-backing-filename":
  "sub/new.qcow2"`, the file that the rebase never read.
- Proof: unit test in `ops.rs`,
  `a_safe_rebase_onto_a_relative_path_reads_the_same_content_after`: the
  four images above, the first cluster of `old.qcow2` equal to that of the
  `new.qcow2` beside it and different from `sub/new.qcow2`, then
  `rebase_safe(sub/top.qcow2, Path::new("new.qcow2"), ..)` with the parent
  directory as working directory, and a read of the first cluster through a
  new open. Expected: the content from before the rebase, or an error from
  the rebase that names the relative path. Today: the content of
  `sub/new.qcow2`. The test changes the working directory, so it must run
  in a child process or hold a lock that other tests of the crate share.

### F18. A size that is not a multiple of 512 loses the end of the disk (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:1104-1138` (`create` writes
  any size into the header), `hv/crates/vmlab-disk/src/ops.rs:385-390`
  (`convert` of a raw source takes the file length as the size),
  `hv/crates/vmlab-hv-devices/src/virtio/block.rs:400` (the capacity is the
  size divided by 512, rounded down).
- Why it is a fault: QEMU counts a disk in sectors of 512 bytes.
  `qemu-img create` and `qemu-img convert` round a size up to the next
  sector, and the last sector of a raw source is filled with zeros.
  vmlab-disk writes the byte count as it is. QEMU reads the header size of
  such an image rounded down, and so does the virtio-blk device of vmlab-hv.
  So for a raw source whose length is not a multiple of 512, the guest of a
  `qemu-img` conversion sees the last bytes of the source and the guest of a
  vmlab-disk conversion does not. The two image tools of
  `src/template/qimg.rs` give disks of different sizes for the same source.
  The sources that vmlab converts today are whole sectors (FAT volumes that
  vmlab builds, disk images), so this needs an odd source file or the CLI.
- Shown by: a raw file of 1000 bytes of `U`, `vmlab-disk convert odd.raw
  v.qcow2` exits 0, `vmlab-disk info v.qcow2` reports a virtual size of
  1000, `qemu-img info v.qcow2` reports `virtual size: 512 B`, and `qemu-img
  convert -O raw` of it gives 512 bytes. `qemu-img convert -O qcow2` of the
  same source gives an image of 1024 bytes. `qemu-img create -f qcow2
  q.qcow2 1000` also makes 1024 bytes, and `vmlab-disk create --size 1000`
  makes 1000.
- Proof: S2 test `convert_of_a_raw_file_with_a_partial_last_sector`: a raw
  file of 1000 bytes of `U`, `ops::convert` to qcow2, then `qemu-img convert
  -O raw` of the result. Expected: 1024 bytes, the 1000 bytes of the source
  and 24 zeros, as `qemu-img convert` of the same source gives. Today: 512
  bytes. Run it on both image tools (`on_both_tools!` in
  `src/template/qimg.rs`) with `image_info` after `convert_to_qcow2`:
  expected is the same `virtual_size` from both.

### F19. An online snapshot of a version 2 image writes zero-flag entries (cosmetic)

- Where: `hv/crates/vmlab-disk/src/qcow2/snapshot.rs:343` (`punch_vmstate`
  sets `ZERO_BIT` for every version), reached from `:224-226`
  (`snapshot_create_impl`) and from `discard_vmstate`
  (`hv/crates/vmlab-disk/src/qcow2/mod.rs:1582-1586`). `zero_cluster` at
  `mod.rs:1418-1422` has the version check that this path lacks.
- Why it is a fault: bit 0 of an L2 entry is the zero flag from version 3
  on. In a version 2 image it must be 0. After the vmstate of an online
  snapshot is in the snapshot, `punch_vmstate` replaces the vmstate entries
  of the active layer with the zero flag, also in a version 2 image. Card
  #563 gave `zero_cluster` a version check for the same reason. vmlab-disk
  reads such an entry as zeros in every version, so vmlab sees no effect.
  QEMU treats the entry as corruption when it reads the cluster. The entries
  are in the vmstate area, past the end of the disk, where a guest read never
  goes, and `qemu-img check` does not report them. So the effect is an image
  that is outside the format, for a disk that came from QEMU as version 2
  (`compat=0.10`) and had an online snapshot under vmlab-hv.
- Shown by: from the code. How QEMU reads such an entry was run: a
  `compat=0.10` image with two written clusters, the second L2 entry set to
  1 by hand, `qemu-io -r -c "read -P 0 64k 64k"` prints `Image is corrupt:
  Zero cluster entry found in pre-v3 image` and fails with an I/O error.
  `qemu-img check` of that image exits 0.
- Proof: S2 test
  `an_online_snapshot_of_a_version_2_image_writes_no_zero_flag`: an image
  from `qemu-img create -o compat=0.10`, `write_vmstate(0, &[0x77; 4096])`,
  `snapshot_create("s", 4096, 0)` and `flush`, then the test reads the file:
  the L1 entry at the index of the vmstate area, and every entry of the L2
  table it points at. Expected: no entry has bit 0. Today: the first entry
  is 1. The snapshot must still hold the vmstate: `snapshot_apply("s")` and
  `read_vmstate` give the 4096 bytes.

### F20. Applying a snapshot with a shorter L1 leaks the longer table (cosmetic)

- Where: `hv/crates/vmlab-disk/src/qcow2/refcount.rs:433-446`
  (`ensure_l1_size`, the branch for a shorter table sets `l1_size` and keeps
  the region), `:448` (the cluster count of the old region is computed from
  `l1_size`), `hv/crates/vmlab-disk/src/qcow2/snapshot.rs:449-451`
  (`snapshot_apply_impl`).
- Why it is a fault: `snapshot_apply` installs the L1 table of the snapshot
  in the region of the active table. When the snapshot's table has fewer
  entries, `ensure_l1_size` zeroes the rest and lowers `l1_size`. The region
  on disk keeps its clusters, and their number is computed from `l1_size`
  from then on. When the old table had more clusters than the new entry
  count needs, the clusters past the first ones keep a refcount of 1 and
  nothing refers to them. `qemu-img check` reports them as leaked and exits
  3. A later growth of the table frees only the clusters that the lower
  count gives. No data is wrong. One L1 cluster holds 8192 entries with
  64 KiB clusters, so this needs a disk that was grown past 4 TiB after the
  snapshot, or an image with small clusters.
- Shown by: `vmlab-disk create s.qcow2 --size 1048576`, a write,
  `vmlab-disk snapshot s.qcow2 --create a`, `vmlab-disk resize s.qcow2
  8796093022208`, `vmlab-disk snapshot s.qcow2 --apply a`, and `qemu-img
  check s.qcow2` exits 3 with `Leaked cluster 9 refcount=1 reference=0`. The
  random comparison found the same on an image with 512-byte clusters after
  a resize of 1 MiB.
- Proof: S2 test `applying_a_snapshot_from_before_a_large_grow_leaks_nothing`:
  the commands above through `Qcow2Image`, then `qemu-img check`. Expected:
  exit 0. Today: exit 3.

### F21. A failed `create` leaves a file that `up` takes as the disk (cosmetic)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:1121-1125` (the file is
  made), `:1167-1171` and `:1229` (two errors after that), `:1596-1602`
  (`Drop` writes the image out), `src/labd/vm.rs:844-850` (an existing
  primary disk is taken as built).
- Why it is a fault: `Qcow2Image::create` makes the file first. It can still
  fail after that: the backing file cannot be opened (a format that
  vmlab-disk refuses, a chain at the limit of F12, no read permission), or a
  write fails (a full file system). The file stays. For a failure in
  `open_backing` the `Drop` of the image writes a complete overlay, which
  works once the cause is gone. For a failed write the file can be a run of
  zeros with no header. The error goes to the caller, and the next `vmlab
  up` finds the primary disk, takes the machine as built and writes the
  clone-source record. With a file that has no header the start then fails
  in vmlab-hv with "bad magic", and it does so until the user destroys the
  machine: a disk that a failed create left is never made again.
  `qemu-img create` opens the backing file before it makes the image. The
  image tool of vmlab removes an existing destination before each create
  (`src/template/qimg.rs:432-459`), so a retry of the same call works, and
  only the `exists` test of `up` is misled. I did not make a write fail, so
  the file of zeros is from the code.
- Shown by: in the chain of F12, `vmlab-disk clone l8.qcow2 l9.qcow2` exits
  1 and `l9.qcow2` exists afterwards.
- Proof: unit test in `ops.rs`,
  `a_failed_create_leaves_no_file`: `create_qcow2` with a backing path that
  is a qcow2 image with an unknown incompatible feature bit set in its
  header. Expected: an error and no file at the destination. Today: the file
  exists.

## Notes

These have no proving test yet. Each one is from reading, except where it
says what was measured. The numbering continues the first review's notes.

- N13. A version 2 image grows at every writable open. This adds a
  measurement to N12. Recovery puts the new refcount table and blocks at the
  end of the file (`qcow2/recovery.rs:64-79`) and does not look for free
  clusters first. Measured on an empty `compat=0.10` image of 64 GiB: six
  `vmlab-disk write` calls of one byte at offset 0 gave the file lengths
  393216, 524288, 655360, 786432, 917504 and 1048576, which is 128 KiB for
  each open. The old tables become free clusters and a later allocation uses
  them, so the growth has a bound only when the guest allocates as much as
  the opens add. For a file of 500 GiB one open adds about 16 MiB.
- N14. `flush_metadata` marks each table clean after its write and before the
  sync (`qcow2/mod.rs:963-977`, `:985-995`). When `sync_all` fails, the
  tables are clean and the kernel has dropped the error state of the pages.
  A second flush then writes nothing, both syncs succeed, and the guest gets
  a good status for a flush whose metadata may not be on the disk. A test
  needs a file layer that fails one sync, which the crate does not have (as
  for N1). One guest flush also makes three `sync_all` calls (`:977`, `:995`
  and `:1499`), also when nothing changed.
- N15. `copy_own_layer` copies a cluster with the zero flag as a cluster of
  written zeros (`ops.rs:469-482`, `qcow2/mod.rs:1314-1329`). `export_overlay`
  and `commit` use it. A disk that ran under QEMU can hold many zero-flag
  clusters (WRITE_ZEROES, F1). The result of a freeze export or a thaw is
  right and can be much larger than the source. It is a cost.
- N16. A rewrite of the snapshot table changes entries it did not create.
  `parse_table` reads the id and the name through `from_utf8_lossy`
  (`qcow2/snapshot.rs:130-131`), so a name that is not UTF-8 comes back with
  replacement characters and another length. It keeps only the first 16
  bytes of the extra data (`:121-127`), and `serialize` writes an icount of 0
  (`:66-69`), so the icount of a record and replay snapshot and any later
  field are lost at the first create or delete. `qemu-img snapshot -c` also
  writes an icount of 0 (run: both rows of `qemu-img snapshot -l` show 0).
- N17. QEMU lets two snapshots have the same name (run: `qemu-img snapshot
  -c x` twice exits 0 both times). `snapshot_apply`, `snapshot_delete` and
  `install_snapshot_view` take the first entry with the name
  (`qcow2/snapshot.rs:162-165`, `:424-427`, `:471-474`). The check of the
  hypervisor (`hv/crates/vmlab-hv-vmm/src/snapshot.rs:746-748`) does the
  same. vmlab-disk refuses to create a second one, so this needs a disk that
  came from QEMU with such a table.
- N18. A writable open of a crafted image can grow the refcount table
  without a limit. `set_cluster_refcount` calls `ensure_refcount_table_len`
  for any cluster index (`qcow2/refcount.rs:165-170`, `:182-195`), and the
  index comes from an L2 entry in `walk_refcounts` (`:503-512`). An entry
  with a host offset near 2^55 in an image with 512-byte clusters and 1-bit
  refcounts asks for a table of gigabytes. The limit of card #555 is checked
  in `parse` only. Recovery refuses an offset past the end of the file, so
  this needs a clean version 3 image and a snapshot operation. No vmlab path
  opens an image for writing that it did not make, so I found no way to
  reach it.
- N19. A snapshot without a recorded disk size keeps the current size when
  it is applied (the fix of F4). After `resize_grow` that size is the new
  one, and the L1 table of the snapshot is from the old one. The vmstate of
  the snapshot is then inside the disk (the cause of F5), and a backing file
  longer than the old size shows through the new area. QEMU refuses to
  resize a version 2 image that has snapshots for this reason, and
  `resize_grow` (`qcow2/mod.rs:1345-1380`) has no such check. It needs a
  snapshot entry of an old QEMU, a resize and an apply.
- N20. `resize_grow` zeroes the tail of the last old cluster only when the
  backing file is longer than the old size now (`qcow2/mod.rs:1366-1372`).
  A cluster that was copied from a longer backing file before a rebase onto
  a shorter one keeps those bytes past the old size, and a grow then shows
  them at the start of the new area.
- N21. A snapshot change that fails after its first step poisons the image
  (`qcow2/snapshot.rs:79-85`, `qcow2/mod.rs:1088-1093`). Every later write
  and flush of the running machine then fails until the process ends, and
  `Drop` writes nothing, so guest writes since the last flush are lost as in
  a power cut. This is the design, and it keeps the image recoverable. I
  found no test in `hv/crates/vmlab-hv-vmm` for what the machine does with a
  poisoned disk after a failed online snapshot or delete.
- N22. `info` walks the backing chain with `probe_format` for each file
  (`ops.rs:205-229`), and `open_backing` uses the recorded format
  (`qcow2/mod.rs:510-519`). For an image that records `raw` for a backing
  file whose first bytes are the qcow2 magic, the two disagree: the open
  reads the file as raw, and `info` reports it as qcow2 and follows the
  backing name inside it. This is one more caller for N8.

## What I read and what I did not

Read in full:

- `hv/crates/vmlab-disk/src/qcow2/mod.rs` without its unit tests,
  `refcount.rs`, `snapshot.rs`, `recovery.rs`.
- `hv/crates/vmlab-disk/src/ops.rs`, `lib.rs`, `raw.rs`,
  `src/bin/vmlab-disk.rs`, `DESIGN.md`.
- `docs/reviews/vmlab-disk.md`, and the list of commits to the crate since
  its commit `aa29533e`.
- `fuzz/fuzz_targets/qcow2.rs`.

Read in part:

- `src/template/qimg.rs`: the trait and the vmlab-disk adapter.
- `src/template/store.rs`: `qcow2_backing_file`, `verify_base`,
  `ensure_variant_backing`, `verify_variants`. `src/template/build.rs`:
  `seed_working_disk` and `refuse_backed_source` (the fix of F2). The store
  and the crate agree on when a header names a backing file.
- `src/freeze/plan.rs`: `freeze_disk` and the checks before it.
  `src/freeze/steps.rs`: `run_step`. `src/freeze/inspect.rs`: `chain`.
- `src/labd/vm.rs:835-862`: how the primary disk is made.
- `hv/crates/vmlab-hv-vmm/src/snapshot.rs`: the calls into `DiskImage` for
  save, load, the rollback checkpoint and delete.
  `hv/crates/vmlab-hv-vmm/src/factory.rs`: `open_image`.
- `hv/crates/vmlab-hv-devices/src/virtio/block.rs`: the capacity and the
  discard feature. The device still offers no WRITE_ZEROES, and AHCI has no
  TRIM, so N2 of the first review stands as it is.

Not read:

- `hv/crates/vmlab-disk/tests/s2_qemuimg.rs` and the unit tests of
  `qcow2/mod.rs`. A finding here may already have a test that I did not see.
  The named tests are new names.
- `src/labd/worktree.rs`, `src/labd/freeze_clone.rs` and the thaw part of
  `src/freeze/plan.rs`: the order and the undo of the steps.
- `src/supervisor/backing.rs` and `src/supervisor/store.rs`.
- The IDE, AHCI and NVMe device models beyond a search for discard.
- The snapshot paths of `src/labd/`. `docs/reviews/snapshots.md` and
  `docs/reviews/snapshots-2.md` cover them.

Done besides reading, all on helios through `stc run card-984`, in scratch
directories that were removed:

- The commands under "Shown by", with a debug build of the `vmlab-disk`
  binary of commit `84fc31e0`.
- A random comparison through the CLI: 200 runs. Each run starts from a
  `qemu-img create` image and makes up to 45 operations: writes with
  vmlab-disk, plain, compressed and zero writes with `qemu-io`, snapshot
  create, apply and delete with either tool, and `vmlab-disk resize`. After
  each operation `qemu-img check` must exit 0. After every fifth, QEMU's view
  and vmlab-disk's view (`vmlab-disk convert`) must equal a byte model. The
  images: default, `compat=0.10`, cluster sizes 512, 4096 and 2 MiB, refcount
  widths 8 and 64, lazy refcounts, a size that is not a cluster multiple, and
  a raw backing file. It found F13 in four runs, each a version 2 image
  after a compressed write (the last 140 runs made no compressed write on
  version 2, so that those runs go on), and F20 in three runs, each the image
  with 512-byte clusters. It found nothing else.
- A random comparison through the public API of the crate, in one process:
  160 runs. Each run makes up to 120 operations on one open image:
  `write_at`, `discard`, `flush`, a close and a new open, `write_vmstate` and
  `snapshot_create` with a vmstate, `write_vmstate` and `discard_vmstate`,
  `snapshot_apply` with `read_vmstate`, and `snapshot_delete`. The content
  must equal a byte model, the vmstate must read back, `qemu-img check` must
  exit 0 at every close, and QEMU's view must equal the model at the end.
  The images: default, `compat=0.10`, a raw backing file, and a cluster size
  of 4096. It found nothing.

Not done:

- No test was added and no code changed.
- No power-loss test (N1, N14) and no run of the fuzz targets.
- No freeze was run against the chain limit. F12 names the freeze step that
  fails from the plan code and from the `clone` that failed in the scratch
  directory.
- No crafted image was put through `vmlab template pull` or `import`.
