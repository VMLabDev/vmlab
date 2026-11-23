# Review: vmlab-disk and qcow2

Card #531. A bug-hunting review of `hv/crates/vmlab-disk` (qcow2 read and
write, backing chains, internal snapshots, recovery, the image operations that
replace `qemu-img`) and of the vmlab code that calls it. It lists faults and
makes no fix. Reviewed at `next` commit `aa29533e`. All paths are from the
repository root, and all line numbers are from that commit.

Findings F1 to F11 were reproduced on helios with the `vmlab-disk` binary of
that commit (debug build) against `qemu-img` and `qemu-io`. Each finding gives
the commands that showed it and the test that should hold it. The commands ran
in a scratch directory and no file of the repository changed. A finding whose
test fails to reproduce is wrong and should be dropped. The entries under
"Notes" are from reading only.

Severity words: **data loss**, **crash** (of a host process), **security**
(against a hostile image, guest or peer), **wrong result**, **cosmetic**.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | data loss | A write to a cluster that QEMU zeroed in place keeps the zero flag, so the written data reads back as zeros (fixed, #542) |
| F2 | security | A qcow2 template build source that names a host file as its backing file is flattened into the build disk (fixed, card #541) |
| F3 | data loss | The refcount range of a compressed cluster is up to 511 bytes too long, so a write to a compressed image fails or takes a reference from the next cluster (fixed, card #549) |
| F4 | data loss | A snapshot entry with less than 16 bytes of extra data is rewritten with disk size 0, and applying it makes the disk 0 bytes (fixed, card #556) |
| F5 | wrong result | `resize` leaves vmstate clusters in the active layer, and the grown disk shows them as guest data (fixed, card #563) |
| F6 | wrong result | Refcount recovery refuses an image whose QEMU-written snapshot table is the end of the file (fixed, card #576) |
| F7 | wrong result | A compressed cluster at the end of the file cannot be read (fixed, card #562) |
| F8 | wrong result | `convert -B` and `commit` fail when the disk size is not a multiple of the cluster size |
| F9 | crash | The L1 size and the snapshot count of a header have no upper limit, so a small crafted image takes gigabytes of memory (fixed, card #555) |
| F10 | crash | Two header fields of a crafted image panic the process (fixed, card #555) |
| F11 | data loss | `convert` with the same path for source and destination deletes the source |

### F1. A write to a preallocated zero cluster is lost (data loss)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:829-844` (`write_bytes`, the
  in-place branch), with `:44-54` (`ReadSource::from_entry`).
- Why it is a fault: an L2 entry can have the zero flag (bit 0) and a host
  offset at the same time. QEMU writes this form when a guest sends
  WRITE_ZEROES without unmap to an allocated cluster: it keeps the cluster and
  sets the flag. `write_bytes` takes the in-place branch for every entry that
  is not compressed, has a host offset and has the copied bit. It writes the
  new bytes to the host cluster and leaves the entry as it is. The zero flag
  stays set. `from_entry` tests the zero flag before the host offset, so every
  later read of the cluster returns zeros. The write reports success. The old
  bytes of the host cluster are also not cleared, so the entry would be wrong
  for a partial write even without the flag.
  vmlab-disk itself never writes this form of entry. A disk that ran under
  QEMU has it: a Linux guest zeroes ranges with WRITE_ZEROES (the lazy inode
  table initialisation of ext4, `fallocate` zero range, `mkfs`). §22.3
  migrates existing QEMU machines to vmlab-hv at the next start, with the same
  disk. After the migration a guest write to such a cluster is lost without
  an error.
- Shown by:
  `qemu-img create -f qcow2 z.qcow2 1M`,
  `qemu-io -c "write -P 0x11 0 64k" -c "write -z 0 64k" z.qcow2` (the L2 entry
  is then `0x8000000000050001`),
  `vmlab-disk write z.qcow2 0 aabbccdd` (exit 0),
  `vmlab-disk read z.qcow2 0 16` prints 16 zero bytes. `qemu-io` reads zeros
  from the image too.
- Proof: S2 test in `hv/crates/vmlab-disk/tests/s2_qemuimg.rs`,
  `a_write_to_a_cluster_qemu_zeroed_in_place_reads_back`: the two `qemu-io`
  writes above, then `write_at(512, &[0xaa; 512])` and `flush` through
  `Qcow2Image`, then read the whole cluster with `qemu-io` and with
  `read_at`. Expected: zeros, then 512 bytes of `0xaa` at offset 512, then
  zeros. Today: all zeros.
- Fixed in card #542. A write to an owned cluster with the zero flag fills the
  whole host cluster, zeros around the new bytes, and then rewrites the entry
  without the flag. The test also writes a whole cluster, and it failed with
  all zeros before the fix.

### F2. A build source with a host file as backing is read into the disk (security)

- Where: `src/template/build.rs:1054-1057` and `:1219-1226` (a
  `TemplateSource::Qcow2` source goes to `convert_to_qcow2` unchecked),
  `hv/crates/vmlab-disk/src/ops.rs:343-390` (`convert`),
  `hv/crates/vmlab-disk/src/qcow2/mod.rs:475-496` (`open_backing`),
  `:1491-1501` (`resolve_backing`), `:1503-1512` (`probe_format`).
- Why it is a fault: the store refuses a pulled or imported base that has a
  backing file (`src/template/store.rs:1053-1066`). The build path has no such
  check. A template definition can name a qcow2 image as its source, and
  `resolve_artefact` resolves it from a local path or a URL
  (`src/template/build.rs:1468-1482`). `convert` opens the source with its backing
  chain. `open_backing` takes an absolute backing name as it is, and opens it
  as raw when the header says `raw` or when the header gives no format and the
  file does not start with the qcow2 magic. So a source image whose header
  names `/home/<user>/.ssh/id_ed25519` as backing gets the bytes of that file
  copied into the working disk at the offsets that the image leaves
  unallocated. The operating system inside the image is from the same
  publisher. It reads the bytes from its disk during the build and can send
  them out through NAT. The `qemu-img` tool does the same, so the check belongs
  in the build, before the convert, for both tools.
- Shown by: `echo host-secret-line > secret.txt`,
  `truncate -s 65536 secret.txt`,
  `qemu-img create -f qcow2 -b $PWD/secret.txt -F raw evil.qcow2 1M`,
  `vmlab-disk convert evil.qcow2 flat.qcow2` (exit 0),
  `vmlab-disk read flat.qcow2 0 16` prints `host-secret-line`. The same result
  with a header that has the backing name and no format extension.
- Proof: build test in `src/template/build.rs`,
  `a_qcow2_source_with_a_backing_file_is_refused`: a source image made with
  `create_linked_clone` over a second file, given to the code that seeds the
  working disk. Expected: an error that names the backing file, and no working
  disk. Today: the working disk holds the backing file's content. Run it on
  both image tools (`on_both_tools!`).
- Fixed by card #541: `seed_working_disk` in `src/template/build.rs` calls
  `refuse_backed_source` before the convert for a `TemplateSource::Qcow2`
  source. A layered source (a store disk, which may be a variant overlay on
  its base) is not checked. The proof test failed on both tools before the
  fix and passes after it.

### F3. Compressed clusters get a refcount range that is too long (data loss)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:877-893`
  (`adjust_compressed_refcounts`, the end at `:882`),
  `hv/crates/vmlab-disk/src/qcow2/recovery.rs:139-143` (`count_view`). The
  read path at `mod.rs:673-674` has the right end.
- Why it is a fault: a compressed descriptor gives a host offset and a count
  of further 512-byte sectors. The data ends at the end of the last sector:
  `(offset / 512 + sectors + 1) * 512`. The read path uses that. The two
  refcount paths use `offset + (sectors + 1) * 512`, which is later by
  `offset % 512` bytes. When the right end is on a cluster boundary and the
  offset is not sector-aligned, the range covers one cluster more than QEMU
  counted. This is the case for about one compressed cluster in 128 with
  64 KiB clusters. A write, a discard or `punch_vmstate` over such a cluster
  then decrements the refcount of the next host cluster, which belongs to
  other data. Either that refcount is already at its true value and the next
  decrement fails with "refcount underflow", or the cluster reaches 0 while it
  is in use and the allocator hands it out again. In recovery the same sum
  counts one cluster too many, and it refuses an image when the too-long range
  passes the end of the file.
  vmlab makes no compressed images, so this needs an image from `qemu-img
  convert -c` (an imported or older template disk) that is opened for writing.
- Shown by: `base64 -w0 /dev/urandom | head -c 67108864 > src.raw`,
  `qemu-img convert -c -O qcow2 src.raw c.qcow2`,
  `vmlab-disk bench c.qcow2 write 67108864` fails with `refcount underflow at
  cluster 81`, and `qemu-img check c.qcow2` then reports errors ("Data may be
  corrupted, or further writes to the image may corrupt it"). With
  `-o compat=0.10` on the convert, `vmlab-disk write c4.qcow2 0 aa` fails with
  `qcow2 reference beyond end of file`.
- Proof: S2 test `overwriting_a_qemu_compressed_image_keeps_refcounts`:
  `qemu-img convert -c` of 64 MiB of base64 text, then `write_at` over every
  cluster and `flush`. Expected: no error, `qemu-img check` exits 0, and
  `qemu-img compare` against the written data is identical. Today: the write
  fails. Unit test beside it: for a descriptor with offset `0x1ff37` and 0
  further sectors in a 64 KiB image, `adjust_compressed_refcounts(entry, 1)`
  must change cluster 1 only. Today it changes clusters 1 and 2.
- Fixed in card #549. `adjust_compressed_refcounts` and `count_view` end the
  range at the end of the last sector, as the read path does. The unit test is
  `compressed_refcounts_end_at_the_last_sector` in
  `hv/crates/vmlab-disk/src/qcow2/mod.rs` (the descriptor moved to clusters
  10 and 11, past the metadata). The S2 test also runs the convert with
  `compat=0.10`. Before the fix the unit test failed with the next cluster
  changed and the S2 test with `refcount underflow at cluster 81`.

### F4. A snapshot entry without extra data comes back with disk size 0 (data loss)

- Where: `hv/crates/vmlab-disk/src/qcow2/snapshot.rs:57-60` (`serialize`
  writes 24 bytes of extra data with `disk_size.unwrap_or(0)`), `:109-112`
  (the parser gives `None` below 16 bytes), `:431-435` (`snapshot_apply_impl`
  takes the size), `hv/crates/vmlab-disk/src/qcow2/mod.rs:1260-1268`
  (`install_snapshot_view`).
- Why it is a fault: the format allows a snapshot entry with 0 or 8 bytes of
  extra data (version 2 images of old QEMU). The parser keeps "no disk size"
  for it. Every snapshot create and delete rewrites the whole table, and the
  rewrite gives each entry 24 bytes of extra data, with 0 for an absent disk
  size. From then on the entry says that the disk was 0 bytes long. Applying
  that snapshot sets the virtual size to 0 and writes it into the header. The
  guest's data is still in the clusters, and no vmlab command sets the size
  back. `convert --snapshot` of such an entry writes an empty image.
- Shown by: `qemu-img create -f qcow2 ex.qcow2 1M`, a write, `qemu-img
  snapshot -c a ex.qcow2`, then the entry rewritten by hand with extra data
  size 0 (`qemu-img snapshot -l` still lists it),
  `vmlab-disk snapshot ex.qcow2 --create b`,
  `vmlab-disk snapshot ex.qcow2 --apply a` (exit 0), and `vmlab-disk info
  ex.qcow2` reports `"virtual-size": 0`.
- Proof: unit test in `qcow2/snapshot.rs`,
  `a_snapshot_without_a_recorded_disk_size_keeps_the_size_through_a_rewrite`:
  create snapshot `a`, write its entry again with extra data size 0, create
  snapshot `b`, apply `a`. Expected: `size()` is unchanged and the first
  cluster reads the data of `a`. Today: `size()` is 0.
- Fixed in card #556. `serialize` writes 8 bytes of extra data (the large
  vmstate size only) for an entry that records no disk size, so the disk size
  stays absent through every rewrite and applying the snapshot keeps the
  current size. An entry with a disk size keeps its 24 bytes.

### F5. Growing an image shows vmstate as disk data (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:1272-1287` (`resize_grow`),
  `:516-519` (`vmstate_base`), `hv/crates/vmlab-disk/src/ops.rs:315-335`.
- Why it is a fault: vmstate is stored as guest data in the first L1 entry
  after the disk. The active layer holds vmstate after `write_vmstate`, and
  after every `snapshot_apply` of a snapshot that has vmstate (the design note
  says so). `resize_grow` only sets the new size and extends the L1 table. The
  L1 entries that held vmstate are now inside the disk, so the new area of the
  disk reads the old vmstate (guest memory and device state) and not zeros. A
  second effect of the same cause: when the image has a backing file that is
  longer than the old size, the new area reads the backing file's data.
  `qemu-img resize` zeroes the new area. In vmlab only the template build
  calls `resize` (`src/template/build.rs:1229-1232`), on a disk that was just
  converted, so today this needs the `vmlab-disk` CLI or a new caller.
- Shown by: `qemu-img create -f qcow2 v.qcow2 1M`,
  `qemu-io -c "write -b -P 0x77 0 64k" v.qcow2` (64 KiB of vmstate),
  `vmlab-disk resize v.qcow2 1073741824` (exit 0),
  `vmlab-disk read v.qcow2 536870912 16` prints 16 bytes of `0x77`.
- Proof: unit test in `qcow2/mod.rs`,
  `a_grown_image_reads_zeros_where_vmstate_was`: `write_vmstate(0, &[0x77;
  4096])`, `resize_grow(2 * l2_span())`, `read_at(l2_span(), ..)`. Expected:
  zeros. Today: `0x77`. A second test with a backing image twice the size of
  the overlay: grow the overlay to the backing size and read the new area.
  Expected: zeros.
- Fixed in card #563. `resize_grow` unmaps every active-layer cluster from
  the first cluster boundary at or after the old size (the vmstate, whose
  address moves with the size) and releases its host clusters. When the
  backing file is longer than the old size, it zeroes the new area up to the
  backing size: written zeros for the tail of the last old cluster, and the
  zero flag (written zeros on version 2) for each whole cluster. Proof tests
  `a_grown_image_reads_zeros_where_vmstate_was` and
  `a_grown_overlay_reads_zeros_where_its_longer_backing_has_data` in
  `qcow2/mod.rs` failed before the fix (`0x77` and `0x66`) and pass after.

### F6. Recovery refuses a QEMU snapshot table at the end of the file (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/recovery.rs:37-46` (the table's
  length is counted with each entry padded to 8 bytes) and `:106-117`
  (`count_range` refuses a range past the end of the file),
  `hv/crates/vmlab-disk/src/qcow2/mod.rs:225-233` (recovery runs on every
  writable open of a version 2 image and of a dirty version 3 image).
- Why it is a fault: QEMU does not write the padding after the last entry of
  the snapshot table. When the table is the last thing in the file, the file
  ends up to 7 bytes before the padded end. `parse_table` checks the unpadded
  length and accepts the image. Recovery counts the padded length and returns
  "qcow2 reference beyond end of file". The image then cannot be opened for
  writing: no start, no snapshot operation, no rebase. A version 2 image is in
  this state right after `qemu-img snapshot -c` or `savevm`. A version 3 image
  is in this state when the process that had it open was killed after its
  first write and before its first new cluster. Both are disks that a machine
  brings from QEMU with its snapshots (§22.3). No data is lost. QEMU opens
  and writes both images.
- Shown by: `qemu-img create -f qcow2 -o compat=0.10 b.qcow2 64M`, an 8 MiB
  write, `qemu-img snapshot -c q1 b.qcow2` (file length 8781891, table at
  8781824, entry 67 bytes), `vmlab-disk write b.qcow2 0 aa` fails with
  `unsupported: qcow2 reference beyond end of file`. The same for a version 3
  image with the dirty bit set by hand. `qemu-io` writes to both.
- Proof: S2 test `a_dirty_image_with_a_qemu_snapshot_table_at_the_end_opens`:
  create the image and the snapshot with `qemu-img`, set incompatible-feature
  bit 0 in the header, then `Qcow2Image::open(path, false)`, one `write_at`
  and `flush`. Expected: success and `qemu-img check` exits 0. Today: the open
  fails. The same test with `-o compat=0.10` and no bit set.
- Fixed by card #576. Recovery counts the snapshot table through the last
  entry's payload, including alignment padding between entries. Both S2
  cases failed at writable open before the fix.

### F7. A compressed cluster at the end of the file cannot be read (wrong result)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:673-676`
  (`read_compressed_cluster`) with `:535-538` (`read_file` uses
  `read_exact_at`).
- Why it is a fault: the descriptor counts whole sectors, and the compressed
  data may end inside the last one. The read asks for every byte up to the end
  of the last sector. QEMU writes only the compressed bytes, so when the
  cluster is the last thing in the file, the file ends inside that sector and
  `read_exact_at` fails with "failed to fill whole buffer". QEMU treats the
  missing bytes as zeros. The guest gets an I/O error for that cluster, and
  `convert` of the image fails. The 64 MiB image of F3 did not end in a
  compressed cluster, so not every `convert -c` image has this.
- Shown by: `qemu-img create -f qcow2 b.qcow2 64M`,
  `qemu-io -c "write -c -P 3 0 64k" b.qcow2` (file length 327759, not a
  multiple of 512), `vmlab-disk read b.qcow2 0 16` fails with `io: failed to
  fill whole buffer`. `qemu-io -c "read -P 3 0 64k"` reads it.
- Proof: S2 test `a_compressed_cluster_at_the_end_of_the_file_reads`: the two
  commands above, then `read_at(0, ..)` of one cluster. Expected: 64 KiB of
  `0x03`. Today: an error.
- Fixed in card #562. The read of a compressed cluster stops at the end of
  the file and leaves the rest of the last sector as zeros, as QEMU does.

### F8. `convert -B` and `commit` fail on a size that is not a cluster multiple (wrong result)

- Where: `hv/crates/vmlab-disk/src/ops.rs:411-424` (`copy_own_layer`, the
  read at `:420`) and `:456-482` (`commit`, the read at `:476` and the write
  after it).
- Why it is a fault: both walk the clusters that the top image holds and read
  one whole cluster at each. When the virtual size is not a multiple of the
  cluster size, the last cluster passes the end of the disk, and `read_at`
  refuses it with "read beyond end of image". `copy_full` and `rebase_safe`
  cut the last chunk to the size, these two do not. So `export_overlay` and
  `commit`, the two steps of a worktree freeze and thaw
  (`src/freeze/steps.rs:175-177`), fail for a disk with data in its last
  cluster when the size is not a multiple of 64 KiB. Sizes like that come from
  imported images, for example an old disk image sized by cylinders. `qemu-img`
  handles both. `commit` has a second gap in the same lines: data of the top
  image past the end of the backing image is skipped without an error, where
  `qemu-img commit` grows the backing image.
- Shown by: a raw file of 1049088 bytes of `U`,
  `vmlab-disk convert odd.raw base.qcow2` (exit 0),
  `vmlab-disk convert base.qcow2 ov.qcow2 --backing $PWD/base.qcow2
  --backing-format qcow2` fails with `read beyond end of image base.qcow2
  (offset 1048576, len 65536)`. An overlay on `base.qcow2` with one byte
  written at 1048576: `vmlab-disk commit top.qcow2` fails with the same
  message. `qemu-img commit -d` of the same overlay exits 0.
- Proof: S2 tests `commit_of_an_image_with_a_partial_last_cluster` and
  `convert_with_backing_of_an_image_with_a_partial_last_cluster`: the steps
  above with a size of 1 MiB + 512 bytes. Expected: success, and `qemu-img
  compare` of the result with the top image is identical. Today: both fail.

### F9. Header counts have no upper limit (crash)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:285-290` and `:378-411`
  (`l1_size` and `refcount_table_clusters` are checked against the file length
  only), `hv/crates/vmlab-disk/src/qcow2/snapshot.rs:87-88` (`nb_snapshots`),
  `hv/crates/vmlab-disk/src/qcow2/recovery.rs:25`.
- Why it is a fault: the three counts are 32-bit header fields, and the only
  limit is that the range they give is inside the file. A file can be long and
  hold almost nothing: a sparse file on disk, and a run of zeros in a registry
  layer that compresses to a few megabytes. The open then allocates and reads
  `l1_size * 8` bytes (up to 32 GiB), and `parse_table` allocates one entry
  per snapshot and reads each one (40 bytes of zeros is a valid empty entry).
  QEMU refuses an L1 table over 32 MiB, a refcount table over 8 MiB and more
  than 65536 snapshots. The virtual size is not compared with the L1 size
  either. The open runs in the lab daemon and the supervisor (`info_header`
  for a pulled disk, `src/supervisor/store.rs:429`), so a crafted template can
  push a daemon into the OOM killer. Nothing here writes outside a buffer.
- Shown by: a 1 MiB `qemu-img` image with header `l1_size` set to `0x04000000`
  and the file extended to 1 GiB (sparse): `vmlab-disk info` exits 0 with a
  peak resident size of 1027 MiB. With `nb_snapshots` set to `0x00800000` and
  the file extended by 320 MiB: `vmlab-disk info` runs for 22.7 s with a peak
  of 2028 MiB. `qemu-img info` refuses both ("Active L1 table too large",
  "Snapshot table too large").
- Proof: unit tests in `qcow2/mod.rs`,
  `an_l1_table_larger_than_the_format_limit_is_refused` and
  `a_snapshot_count_larger_than_the_format_limit_is_refused`: the two crafted
  headers above on a `testsupport::blank` image. Expected:
  `Qcow2Image::open(path, true)` and `open_header` return an error. Today:
  both succeed. No time or memory measurement is needed for the test.
- Fixed in card #555. `parse` refuses an `l1_size` over 4194304 entries
  (32 MiB), a refcount table over 8 MiB and more than 65536 snapshots,
  QEMU's limits, before it allocates anything. The snapshot table parser and
  refcount recovery refuse a snapshot L1 over the same limit. `create`,
  `ensure_l1_size` and `snapshot_create` refuse to write an image past them.
  The two proof tests failed before the fix (`open` succeeded) and pass
  after it.

### F10. Two crafted header fields panic the process (crash)

- Where: `hv/crates/vmlab-disk/src/qcow2/mod.rs:421-423` (the backing name
  range, `start + backing_file_size` is not a checked sum) and `:1191-1197`
  (`set_backing` copies `header_length` bytes from a buffer of one cluster).
- Why it is a fault: `backing_file_offset` is a 64-bit header field. With a
  value near `u64::MAX` the sum overflows. A build with overflow checks panics
  at the sum, and a build without them gets an end before the start and panics
  at the slice. This is in `parse`, so every open of the image panics,
  read-only ones too. `header_length` is checked for a minimum and for a
  multiple of 8 (`:299`) and not against the cluster size, so the first rebase
  of an image with a larger value panics at the slice. A panic in the lab
  daemon's blocking task comes back as a failed operation, and in vmlab-hv it
  ends the machine's process.
- Shown by: header bytes 8-19 set to offset `0xfffffffffffffff8` and size 16:
  `vmlab-disk info` panics with "attempt to add with overflow". Header bytes
  100-103 set to `0x00100000`: `vmlab-disk rebase hl.qcow2 --backing .. --unsafe`
  panics with "range end index 1048576 out of range for slice of length 65536"
  at `mod.rs:1197`.
- Proof: unit tests in `qcow2/mod.rs`,
  `a_backing_name_offset_near_the_end_of_u64_is_an_error` (`open` returns
  `Err`) and `a_header_length_beyond_the_cluster_is_an_error` (`open` or
  `set_backing` returns `Err`). Today both panic. The qcow2 fuzz target of
  `docs/reviews/fuzz-coverage.md` (P26) should reach the first one.
- Fixed in card #555. `parse` computes the end of the backing name with a
  checked sum and refuses a `header_length` larger than the cluster.
  `set_backing` returns an error when the extensions do not fit after the
  header, which a header length near the cluster size still allowed. The two
  proof tests and a third,
  `a_header_length_that_leaves_no_room_for_extensions_is_an_error`, panicked
  before the fix and pass after it.

### F11. `convert` onto its own source deletes the source (data loss)

- Where: `hv/crates/vmlab-disk/src/ops.rs:350-352`.
- Why it is a fault: `convert` removes an existing destination before it
  opens the source. With one path for both, or with a destination that is a
  second name of the source or of the backing file, the image is gone before
  anything was read, and the call returns "No such file or directory". No
  vmlab caller passes equal paths today (`src/template/qimg.rs:494`, `:520`),
  so this is a fault of the operation and of the CLI. A convert that is
  interrupted also leaves a part of an image at the destination path, because
  the image is written in place and not to a temporary name.
- Shown by: `vmlab-disk convert same.qcow2 same.qcow2` exits 1, and
  `same.qcow2` no longer exists.
- Proof: unit test in `ops.rs`, `convert_onto_the_source_is_refused`: after
  the call the source still exists and reads the same data. Today the file is
  gone.
- Fixed in card #543. `convert` refuses, before it removes anything, a
  destination that is the same file (device and inode, so under any name) as
  the source, as the `backing` argument, or as a file of either backing
  chain. The proof test failed before the fix (`the source was deleted`) and
  passes after it, with `convert_onto_a_second_name_of_the_source_is_refused`
  and `convert_onto_a_backing_file_is_refused` beside it. A hard link or a
  symlink to the source is refused too, although replacing that name alone
  loses no data.
  - Not fixed here: `convert` still writes the image in place, so an
    interrupted convert leaves a part of an image at the destination path.

## Notes

These have no proving test yet. Each one is from reading.

- N1. Write order on power loss. `load_l2` writes an evicted dirty L2 table at
  once (`qcow2/mod.rs:567-575`), with no sync of the data clusters that the
  table points at. `flush_metadata` syncs data before mappings (`:918`), an
  eviction does not. After a power loss the table can be on disk and the data
  cluster not. For a copy-on-write of a cluster that a snapshot shares, the
  cluster then reads garbage where it held flushed data before. The cache has
  64 tables, so this needs writes over more than 32 GiB of address space
  between two flushes with 64 KiB clusters. A test needs a file layer that
  records the order of writes and syncs, which the crate does not have.
- N2. `discard` sets the L2 entry to 0 (`qcow2/mod.rs:1379`, `:1385`). In an
  overlay the cluster then reads the backing file's data, which is the
  template's old content and not zeros. virtio-blk leaves the content after a
  discard undefined, and the device does not offer WRITE_ZEROES
  (`hv/crates/vmlab-hv-devices/src/virtio/block.rs:396`), so no guest contract
  is broken today. A device that maps WRITE_ZEROES with unmap to `discard`
  would be wrong.
- N3. `old_snapshot_table_clusters` (`qcow2/snapshot.rs:526-530`) sizes the
  table on disk with `padded_size`, which assumes 24 bytes of extra data per
  entry (`:43-45`). For a table with other extra data sizes the count can be
  one cluster off, and `rewrite_snapshot_table` then decrements a cluster that
  is not part of the table, or misses one. It needs a table that ends near a
  cluster boundary, about 900 snapshots with 64 KiB clusters.
- N4. `flush_metadata` resets the free-cluster scan to cluster 0
  (`qcow2/mod.rs:940`). The first allocation after every flush reads refcount
  blocks from the start of the image until it has 4096 free clusters. On a
  large full image this is a read of every refcount block per guest flush.
  It is a cost, not a wrong result.
- N5. The crate still takes no lock on an image (no `flock` or OFD lock in the
  crate). Two writable opens of one file overwrite each other's metadata. This
  is F1 of `docs/reviews/snapshots.md`, closed there with a reservation in the
  lab daemon and not in the crate. The `vmlab-disk` CLI on a running machine's
  disk has no protection.
- N6. The virtual size is never checked. `Qcow2Image::create` casts the L1
  entry count to `u32` (`qcow2/mod.rs:1053`). `vmstate_base` multiplies without
  a check (`:516-519`), so for a size within one L2 span of `u64::MAX` it
  wraps to 0 in a build without overflow checks, and `write_vmstate` would
  write the vmstate over the start of the disk. Such a size is not usable as a
  disk, so this needs a crafted header and an online snapshot.
- N7. `ensure_l1_covers(0)` subtracts 1 from 0 (`qcow2/mod.rs:1292`). It is
  reached by `write_vmstate(0, &[])` on an image of size 0.
- N8. `probe_format` decides by the first four bytes (`qcow2/mod.rs:1503-1512`),
  and `info`, `info_header`, `resize`, `convert` and `commit` use it. A raw
  disk that a guest can write could be given a qcow2 header by that guest.
  vmlab-hv gets every format from the machine description
  (`hv/crates/vmlab-hv-vmm/src/factory.rs:40-43`), and the raw disks it opens
  for a guest are read-only, so I found no path from a guest to a probe.
- N9. After `snapshot_apply` of a snapshot with vmstate the active layer keeps
  the references to the vmstate clusters. A later snapshot with a smaller or
  no vmstate punches only its own range (`qcow2/snapshot.rs:271-326`), so the
  rest stays allocated in the active layer and in every later snapshot until
  the image is deleted. QEMU does the same. It is lost space and the cause
  of F5.
- N10. `snapshot_apply_impl` calls `ensure_l1_size` (`qcow2/snapshot.rs:428`),
  which zeroes the tail of the active L1 on disk at once when the snapshot's
  L1 is shorter (`qcow2/refcount.rs:428-439`). The rest of the apply reaches
  the disk at the flush. A kill between the two leaves the old active layer
  without its last L1 entries. The dirty bit starts a recovery at the next
  open, so the image is consistent, and the lost entries were about to be
  replaced by the apply.
- N11. `create_linked_clone` records the backing format `qcow2` for every
  backing file (`ops.rs:91-95`). The store accepts a raw base
  (`src/template/store.rs:1055`). A clone of a raw base then fails at its
  first open with "bad magic". I did not check whether a raw base can reach
  this call.
- N12. Every writable open of a version 2 image, and of a dirty version 3
  image, builds a new refcount table at the end of the file
  (`qcow2/recovery.rs:49-64`). vmlab-hv sets the dirty bit at the first write
  and clears it only on a clean close (`qcow2/mod.rs:952-977`), so every
  killed machine pays a full walk of all L2 tables at its next start. The old
  tables become free clusters and are used again.

## What I read and what I did not

Read in full:

- `hv/crates/vmlab-disk/src/qcow2/mod.rs`, `refcount.rs`, `snapshot.rs`,
  `recovery.rs`, with the unit tests in `mod.rs`.
- `hv/crates/vmlab-disk/src/ops.rs`, `lib.rs`, `raw.rs`, and the command
  table of `src/bin/vmlab-disk.rs`.
- `hv/crates/vmlab-disk/DESIGN.md`.

Read in part:

- `src/template/qimg.rs`: the two tools and which `ops` call each verb makes.
- `src/template/build.rs`: the seed of the working disk and the resize.
- `src/template/store.rs`: `qcow2_backing_file` and `verify_base`.
- `hv/crates/vmlab-hv-vmm/src/factory.rs` and
  `hv/crates/vmlab-hv-devices/src/virtio/block.rs`: how a disk is opened and
  what discard does.
- The list of callers of `qimg::` in `src/`.

Not read:

- `hv/crates/vmlab-disk/tests/s2_qemuimg.rs`. A finding here may already have
  a test that I did not see. The named tests are new names.
- `src/freeze/`, `src/labd/worktree.rs` and `src/labd/freeze_clone.rs`: the
  order and the undo of freeze, thaw and clone steps. Only the two calls named
  in F8.
- `src/labd/vm.rs` and `src/labd/container.rs`: the snapshot paths.
  `docs/reviews/snapshots.md` covers them.
- `src/supervisor/backing.rs` and `src/supervisor/store.rs` beyond one call.
- The snapshot code of `hv/crates/vmlab-hv-vmm` that writes and reads vmstate
  through `DiskImage`.
- The IDE and AHCI disk devices.

Not done:

- No test was added and no code changed. The experiments used the debug build
  of the `vmlab-disk` binary, `qemu-img` and `qemu-io` on helios.
- No power-loss test (N1) and no run of the fuzz targets.
- No crafted image was put through `vmlab template pull` or `import`. F9 and
  F10 name the open that such an image reaches, from the code.
