# Review: vmlab-hv virtio devices and the virtio-pci transport

Card #379, 2026-11-21. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

All paths are relative to `hv/crates/`. Line numbers are those of commit
`47dabad7` (`next` at the start of the card). `devices/` stands for
`vmlab-hv-devices/src/`.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. Three facts about the `virtio-queue` 0.18.0 crate carry several
findings. They were checked in the crate source on helios
(`~/.cargo/registry/src/*/virtio-queue-0.18.0/src`):

- `Queue::pop_descriptor_chain` logs with `error!` and returns `None` when
  `avail.idx` is more than the queue size ahead of `next_avail`
  (`queue.rs:564-577`, `698`).
- `Queue::enable_notification` returns `true` whenever `avail.idx` differs
  from `next_avail` (`queue.rs:479-491`). It does not apply the check above.
- `Reader::new` and `Writer::new` put no upper limit on the total length of a
  chain (`descriptor_utils.rs:158-190`, `266-298`). Descriptors may alias the
  same guest memory. A chain walks an indirect table whether or not
  `VIRTIO_F_INDIRECT_DESC` was negotiated, and an indirect table holds up to
  65535 descriptors (`chain.rs:119-148`). So the guest chooses the total
  length of a chain freely, far above its RAM size.

Severity words are those of the card: data loss, crash of the VM or host
process, security against a hostile guest, wrong result, cosmetic.
"Confidence" says how sure the reading is.

## What was read and what was not

Read in full, without the test modules:

- `devices/virtio.rs`, `devices/virtio/pci.rs`,
  `devices/virtio/transitional.rs`, `devices/virtio/worker.rs`
- `devices/virtio/block.rs`
- `devices/virtio/net.rs`, `net/worker.rs`, `net/buffer.rs`, `net/tap.rs`
- `devices/virtio/console.rs`, `devices/virtio/sound/mod.rs`
- `devices/virtio/fs/mod.rs`, `fs/device.rs`, `fs/fuse/mod.rs`,
  `fs/fuse/server/mod.rs`, `fs/fuse/server/sync_io.rs`
- `vmlab-hv-vmm/src/pci.rs`, and `KvmDoorbells` in `vmlab-hv-vmm/src/kvm.rs`

Read in part:

- `fs/passthrough.rs` (lines 1-1830, 1940-2070, 2100-2384),
  `fs/vhost_user.rs` (1-1000), `fs/fuse/transport.rs` (1-760),
  `fs/fuse/file_traits.rs`, `fs/fuse/file_buf.rs`, `fs/fuse/abi/fuse_abi.rs`,
  `fs/fuse/filesystem/` (targeted reads)
- `net/checksum.rs` (the three constructors), `vmlab-hv-proto/src/net.rs`
  (header decode, validate, layout parse, flow hash)
- `devices/virtio/gpu/`: `mod.rs`, `copy.rs`, `compose.rs`, `snapshot.rs`,
  `vblank.rs`, `pacer.rs`, `teardown.rs` and `renderer.rs` lines 1-1926 in
  full. `scanout.rs` (96-325, 455-984) and `lost.rs` (1-120) in part.
  `modifiers.rs` and `vga_tests.rs` were not read.

Not read:

- `net/attach.rs`, `net/tap_integration_tests.rs`, `transitional_tests.rs`
- `fs/fuse/server/guest_log.rs`, `fs/fuse/abi/virtio_fs.rs`
- every test module, apart from the names of fixtures and helpers
- `vmlab-hv-vmm/src/snapshot/` and `reset.rs`. So the order in which the VMM
  saves guest memory, transport state and device state was not checked.
- `crate::audio` and the display code that the sound and GPU devices call
- There is no vsock, rng or balloon device in the tree.

## Findings, worst first

### F1. virtio-fs: a rename is not recorded, so a create at the old name re-points the renamed inode

- Where: `devices/virtio/fs/passthrough.rs:617-640` (`rename`) and
  `1004-1016` (`entry_for`).
- Fault: `rename` only calls `renameat2`. The moved inode keeps its `path`,
  `name` and `parent`, and `by_path` still maps the old path to it. When the
  guest later looks up or creates the old name, `entry_for` finds that stale
  entry and overwrites the inode's descriptor (`state.fd = fd`). The node id
  the guest holds for the renamed file now opens the new file, and the create
  returns the same node id for both names. `readlink` (`516-522`) and
  `unlink` use the same stale fields.
- Reach: an ordinary guest. `mv a b` followed by `echo x > a`, or the
  write-temporary-then-rename save pattern used twice.
- Severity: data loss. Reads and writes of one file reach the other.
- Proof: test `rename_then_create_at_old_name_keeps_the_renamed_inode` in the
  passthrough tests, with the helpers `scratch()`, `lookup()`, `ctx()`,
  `name()` and `SinkWriter`. Look up `hello.txt` as X. Call
  `fs.rename(ROOT_ID, "hello.txt", ROOT_ID, "moved", 0)`. Call `fs.create`
  for `hello.txt` and write `new` into it on the host. Assert that `fs.open(X)`
  and `fs.read` return the old content and that the created entry has a node
  id other than X. Today the read returns `new` and the id is X.
- Confidence: high.
- Fixed in card #391. `rename` moves the inode, and every known inode below
  it, to the new path in `by_path`, and gives the moved inode its new parent
  descriptor and name. A replaced destination loses the name, and
  `RENAME_EXCHANGE` swaps both subtrees. The proposed test failed before the
  fix (the create returned X) and passes after it. Not fixed here: `unlink`
  and `rmdir` still leave the removed name in `by_path`, and the inode a
  rename replaced keeps its old `path` for the snapshot.

### F2. virtio-fs: after a restore, a handle whose file is gone accepts writes and reports success

- Fixed in #392. Restored file handles without a reopened file return
  `ESTALE` on read, write and fsync.
- Where: `devices/virtio/fs/passthrough.rs:352-357`, `1285-1298`
  (`reopen_dead`), `1124-1138` (`file_handle`).
- Fault: the handle is reopened on `/dev/null` with `O_RDWR`. The comments
  say that I/O on it "must fail loudly" and line 233 promises `ESTALE`, but
  `file_handle` never looks at `inode.dead` and the code contains no
  `ESTALE`. A write returns its full length and the bytes are dropped. A
  read returns end of file. `fsync` succeeds.
- Reach: any file that was open and then unlinked before the snapshot. With
  F1, also any file renamed after its lookup, because restore walks the stale
  path.
- Severity: data loss, silent.
- Proof: test `restored_handle_on_a_vanished_file_fails_io` with `scratch()`
  and `SliceReader`. Create `log.txt` and keep the handle. `save_state()`.
  Remove the file on the host. `restore_state()` into a new `PassthroughFs`.
  Assert that `write` returns `ESTALE`. Today it returns `Ok(len)`. A second
  test `restored_handle_follows_a_guest_rename` does open, `fs.rename`, save,
  restore, write, and asserts that the bytes are in the renamed file.
- Confidence: high.

### F3. virtio-fs: host `O_APPEND` is kept while `WRITEBACK_CACHE` is negotiated

- Where: `devices/virtio/fs/passthrough.rs:1253-1263` (`host_open_flags`
  keeps `O_APPEND`), `396-410` (`init` asks for `WRITEBACK_CACHE`), and the
  write path `749-768` into `pwritev64` at `fs/fuse/file_traits.rs:431`.
- Fault: on Linux a `pwritev` on an `O_APPEND` descriptor ignores the offset
  and writes at the end of the file. With the writeback cache the guest
  kernel owns the append offset and writes whole dirty pages at their real
  offset. A page that the guest writes back twice is appended twice. The
  trait documentation at `fs/fuse/filesystem/sync_io.rs:286-292` says the
  server must ignore `O_APPEND` in this mode. The existing test
  `append_uses_host_eof_instead_of_stale_guest_offset` pins the opposite, so
  the choice was deliberate and conflicts with the negotiated flag.
- Severity: data loss (duplicated data in appended files).
- Proof: test `append_handle_under_writeback_honours_the_write_offset`. After
  `fs.init(FsOptions::all())`, create `f`, open it `O_WRONLY|O_APPEND`, write
  `a\n` at offset 0, then write `a\nb\n` at offset 0. Assert that the file
  is `a\nb\n`. Today it is `a\na\nb\n`.
- Confidence: high for the server side. Medium that a Linux guest sends the
  second write for `echo a >> f; echo b >> f`. An end-to-end run in a guest
  settles it.
- Fixed in card #398. OPEN and CREATE clear host `O_APPEND` when writeback
  is negotiated, so writes honor the guest offset. Snapshots now record
  the negotiated setting.

### F4. virtio-sound: `PCM_INFO` allocates the guest's `size` field with no limit

- Where: `devices/virtio/sound/mod.rs:279-303`.
- Fault: `size` is a 32-bit field of the request. It is checked only for
  zero. For each requested stream the reply grows by
  `out.resize(out.len() + size - PCM_INFO_LEN, 0)`. One 16-byte control
  request with `size = 0xFFFF_FFFF` and `count = 2` makes the device
  zero-fill about 8 GiB under the device lock, before the reply is cut to
  the writable descriptor (`drain_control`, `776`).
- Severity: security against a hostile guest. The host process runs out of
  memory, or aborts on the failed allocation, from one request.
- Proof: test `pcm_info_with_a_huge_size_is_refused_or_bounded` in
  `sound/tests.rs`, with `Guest::new(PacedHost::with_script(vec![]), true)`,
  `negotiate` and `setup_queues`. Send `R_PCM_INFO` with `start_id 0`,
  `count 2`, `size 0x1000_0000`. Assert that the status is `S_BAD_MSG`, or
  that `guest.device.lock().control(&request)` returns at most
  `4 + 2 * PCM_INFO_LEN` bytes. Today it returns more than 512 MiB.
- Confidence: high.
- Fixed in card #393. A `size` above `PCM_INFO_SIZE_MAX` (256) is answered
  `S_BAD_MSG` before anything is allocated. A smaller `size` above the
  structure's 32 bytes is still zero-padded. The proposed test failed before
  the fix (a 512 MiB reply) and passes after it.

### F5. A descriptor chain is copied into host memory whole, with no limit (block, sound)

- Where:
  - `devices/virtio/block.rs:698-711`. `VIRTIO_BLK_T_OUT` resizes
    `io_buffer` to `reader.available_bytes()` and reads the chain into it
    before the range check against the disk size.
  - `devices/virtio/block.rs:653-670`. The slow path of `VIRTIO_BLK_T_IN`
    resizes `io_buffer` to the writable length. Here the range check comes
    first, so the limit is the disk size, which is not a memory limit.
  - `devices/virtio/sound/mod.rs:537-538` (tx: `read_to_end` before any
    check, also when the bytes are thrown away), `768-769` (control:
    `read_to_end`, though no request is longer than 24 bytes), `686-687`
    (rx: `vec![0u8; frames * frame_bytes]` from the writable length).
- Fault: the guest chooses the chain length (see "How the review was done").
  `seg_max` and the stream's `buffer_bytes` are advice to the driver and are
  not enforced. `io_buffer` is also never shrunk, so the largest request
  stays allocated. virtio-net and the console cap the same thing
  (`MAX_FRAME`, `MAX_TX_STAGE`).
- Severity: security against a hostile guest. Host memory exhaustion and the
  end of the VM process.
- Proof:
  - `write_larger_than_the_disk_is_refused_before_it_is_buffered` in the
    block tests, with `Fixture` and `desc()`. A 4096-byte `MemDisk`. One
    chain: the 16-byte header (`T_OUT`, sector 0), 8 readable descriptors
    that all point at the same 64 KiB of `buf`, and the status byte. Assert
    status `S_IOERR` and `f.block.io_buffer.capacity() <= 4096`. Today the
    capacity is 512 KiB.
  - `tx_message_larger_than_the_buffer_is_not_copied_whole` in
    `sound/tests.rs`: `start_streams()`, `advance(period_time())`, then
    `guest.submit(VQ_TX, ...)` with a 4-byte header, 32 descriptors that
    alias one 32 KiB region, and the 8-byte status. Assert
    `host.pcm().len() <= 4096` (the `buffer_bytes` of `start_streams`) or
    `S_IO_ERR`. Today about 1 MiB reaches the sink.
  - Twins `rx_message_larger_than_a_period_pulls_one_period` and
    `control_request_longer_than_any_request_is_bad_msg`.
- Confidence: high.
- Fixed in card #393.
  - Block: `VIRTIO_BLK_T_OUT` checks the range against the disk before it
    copies anything. Both scatter-gather paths then move a request through
    `io_buffer` in pieces of at most `IO_CHUNK` (1 MiB), so the buffer no
    longer follows the chain length or the disk size. A write that fails in
    the image partway can leave the earlier pieces written, with `S_IOERR`.
  - Sound tx: a message with more PCM than the stream's `buffer_bytes`, or
    than `PCM_MESSAGE_MAX` (1 MiB), completes with `S_IO_ERR`. A message
    that is not played is not copied at all.
  - Sound rx: one message is filled with at most one `period_bytes`, and at
    most `PCM_MESSAGE_MAX`.
  - Sound control: a request longer than 24 bytes (`SET_PARAMS`, the longest
    one served) is answered `S_BAD_MSG` without being read.
  - The four proposed tests and `large_requests_are_staged_in_bounded_pieces`
    (block, both directions) failed before the fix and pass after it.
  - Not fixed here: `set_params` still accepts any `buffer_bytes` and
    `period_bytes`. The two caps above bound what that can cost.

### F6. An `avail.idx` far ahead makes the queue drain loop spin forever (block, virtio-fs, GPU)

- Fixed in #399. The three drain loops check `avail_idx_in_range`
  (`devices/virtio.rs`) before each pass and stop on an index more than the
  queue size ahead. The test `avail_idx_far_ahead_does_not_spin` in the
  block, virtio-fs and GPU tests fails before the fix (`queue_notify` does
  not return in two seconds) and passes after it. The device is not marked
  `DEVICE_NEEDS_RESET`: it ignores the queue until the index is valid again,
  as virtio-net does.
- Where: `devices/virtio/block.rs:445-476`,
  `devices/virtio/fs/device.rs:137-166`,
  `devices/virtio/gpu/mod.rs:1274-1307`.
- Fault: the loop drains with `pop_descriptor_chain` and repeats while
  `enable_notification` returns `true`. When the guest writes an `avail.idx`
  that is more than the queue size ahead of `next_avail`, the pop returns
  `None` and consumes nothing, and `enable_notification` returns `true`. The
  loop never ends. Each pass also writes one `error!` line ("Iterator
  error"), so the host log grows without limit.
  - Block runs behind `QueueWorker`. The worker thread spins and holds the
    device mutex. A vCPU that reads the device configuration, writes status
    0, or any snapshot `quiesce` then waits forever (`worker.rs:234-243`,
    `296-300`).
  - virtio-fs and the GPU serve the kick on the vCPU thread. That vCPU never
    returns to its run loop, with the device lock held.
  - The loop ends only when the guest writes a valid index again.
  - virtio-net has the guard: `q.iter()` errors set `invalid` and leave the
    loop (`net.rs:1232-1237`, `1603-1608`). Sound and the console do not
    loop on `enable_notification` this way.
- Severity: security against a hostile guest. A hung VM that the host cannot
  pause or snapshot, a busy host CPU, and a log that fills the disk.
- Proof: test `avail_idx_far_ahead_does_not_spin` in the block tests.
  `Fixture::new(Box::new(MemDisk::new(4096)))`, write `0x8000u16` to
  `f.avail + 2`, run `f.block.queue_notify(0)` on a thread and assert that it
  returns within two seconds. The same test for virtio-fs with the
  `Fixture` of `fs/device.rs` (`f.fs.queue_notify(1)`), and for the GPU with
  the helpers of `gpu/tests.rs`.
- Confidence: high. Both crate facts were read in the source.

### F7. virtio-fs: every directory handle caches the whole directory, with no limit

- Where: `devices/virtio/fs/passthrough.rs:1142-1169` (`dir_entries`),
  `1549-1611` (`read_dir`), and the `.to_vec()` at `851` and `889`.
- Fault: a `READDIR` at offset 0 stores the complete listing in the handle.
  Every `READDIR` and `READDIRPLUS` call clones the complete listing again
  under the session lock. A guest fills a directory with some hundred
  thousand entries, opens 4096 directory handles and reads each one time.
  That pins tens of GiB in the VM process. Each call is also linear in the
  directory size, so a listing is quadratic.
- Severity: security against a hostile guest (host memory). For an ordinary
  guest it is slow listing of large directories.
- Proof: test `directory_caches_are_bounded_across_handles` with `scratch()`
  (the handle limit is 32 under test). Create 2000 files, `opendir` and
  `readdir` at offset 0 on 32 handles, sum `entries.len()` over
  `HandleKind::Dir` in `fs.inner.lock().handles`, and assert a stated budget.
  Today the sum is 32 x 2000.
- Confidence: high for the mechanism. The effect depends on the memory limit
  of the process.
- Fixed in card #400. A directory handle holds no entries: it holds one
  directory descriptor, opened by the first `READDIR`, and the cookie the
  stream stands at. Each request reads bounded batches with `getdents64`
  (at most 64 KiB per call) and serves the host's `d_off` as the cookie, so
  a request costs what it serves and a listing is linear. A request at
  another cookie seeks. Offset 0 still rewinds. A handle counts against the
  session limit as before, so the descriptors are bounded by it. The test
  `directory_caches_are_bounded_across_handles` fails before the fix
  (`32 handles hold 64096 directory entries`) and passes after it.
  - Changed for the guest: a cookie is now the host's `d_off` and not an
    index. After a snapshot restore a handle resumes by seeking to the
    cookie the guest sends.
  - Not fixed here: F13.

### F8. virtio-net: one frame larger than the guest's buffers stalls receive forever without mergeable buffers

- Where: `devices/virtio/net.rs:1369-1398` (`place_rx_frames`) and
  `1095-1130` (`place_vnet_rx_frames`).
- Fault: without `VIRTIO_NET_F_MRG_RXBUF` a frame must fit one chain. When
  it does not, the chain is returned with length 0 and the frame stays at
  the front of `rx_pending` (`if written == 0 { break; }`, and
  `Receive::Retry` puts the packet back in the vnet path). The next buffer
  the guest posts gets the same treatment. The frame never leaves, every
  receive buffer comes back empty, and the socket is not read while
  `rx_pending` is not empty (`rx_may_read`, `608-611`). The mergeable path
  has the guard for this case (`972-977`: "Drop it and return the buffers so
  a misconfigured driver cannot stall the worker").
- Reach: a guest driver that does not negotiate mergeable buffers (iPXE on
  the transitional NIC, old drivers). The frame comes from the segment, so
  another guest on the same switch can send it. The wire format accepts
  frames up to 65550 bytes.
- Severity: wrong result that another guest can cause. The NIC receives
  nothing more until the link drops.
- Proof: test `oversized_frame_without_mergeable_buffers_is_dropped` in the
  net tests, with `Fixture`, `TestVq` and `wait_for`. Negotiate without
  `F_MRG_RXBUF`. Post two 256-byte receive buffers. Send a 1000-byte frame
  and then a 60-byte frame from the test switch socket. Post one more
  buffer. Assert that the 60-byte frame arrives. Today each buffer comes
  back with used length 0 and nothing arrives.
- Confidence: high.

### F9. virtio-gpu: chains held for a vblank or a fence have no upper limit

- Where: `devices/virtio/gpu/mod.rs:1278-1285` (vblank waits) and
  `1287-1297` (fences).
- Fault: `queue_notify` pushes one `vblank::Wait`, with a heap `Vec`, for
  each vblank-wait chain and one `PendingFence` for each fenced chain. It
  does not complete them and it has no cap. A guest can publish the same
  head again and again, so the queue size does not limit the count. The
  loop holds the device lock, and the vblank clock (`vblank.rs:182`) and the
  fence thread (`mod.rs:350`) need that lock to drain the lists. A second
  vCPU that keeps advancing `avail.idx` keeps the loop running, and the
  lists grow until the host process has no memory left.
- Reach: a 3D device (`mod.rs:678`) and a guest that acknowledges feature
  bit 23. The guest controls both.
- Severity: security against a hostile guest (host memory).
- Proof: test `held_vblank_waits_are_bounded_by_the_queue_size` in
  `gpu/tests.rs`. `QueueFixture::new(FEATURES)` with `vblank::Clock::idle()`
  installed as the existing vblank test does. 40 times:
  `f.put(0, &vblank_wait(0), 48); f.notify();`. Assert
  `f.gpu.state.lock().vblank_waits.len() <= 16` and that the surplus chains
  are in the used ring. Today the length is 40. A fence twin pushes
  `PendingFence` the way `a_lost_host_gpu_answers_every_fence_the_device_holds`
  does.
- Confidence: high that the limit is missing. Medium that it grows to an
  out-of-memory state in practice.

### F10. virtio-gpu: guest backing entries have no total cap in 3D mode

- Where: `devices/virtio/gpu/renderer.rs:1098-1101`, `1518-1534`, and blob
  create at `1669-1681`.
- Fault: the 2D path caps the entries over all resources
  (`gpu/mod.rs:1005-1006`, test `backing_entries_are_bounded_across_resources`).
  The renderer path checks only `count > MAX_ENTRIES` for one resource. 4096
  resources with 16384 entries each cost about 1 GiB of `RutabagaIovec`,
  1 GiB more in `HostResource.backing`, and the copy that virglrenderer
  keeps.
- Severity: security against a hostile guest (host memory, some GiB).
- Proof: test `renderer_backing_entries_are_bounded_across_resources` in the
  `renderer.rs` tests, gated on `VMLAB_HV_TEST_RENDER_NODE` like
  `venus_blobs_return_their_descriptors_and_stop_at_the_budget`. Create two
  resources, attach `MAX_ENTRIES` one-page entries to the first and one
  entry to the second, and expect `Err(INVALID)` for the second. Today it
  succeeds. The test needs a render node, so it does not run in
  `just ci::check`.
- Confidence: high.

### F11. virtio-fs: a device reset does not end the FUSE session

- Where: `devices/virtio/fs/device.rs:283-291` (`reset` clears only the
  queue state), `fs/passthrough.rs:396-422` (`init` keeps the session,
  `destroy` is empty).
- Fault: after a guest reboot the new kernel knows none of the old node ids
  and handles and cannot forget or release them. Up to `SESSION_LIMIT`
  (4096) inodes and 4096 handles stay pinned, with the disk space of deleted
  files. When the old boot filled the table, the new boot gets `ENFILE` on
  each new lookup or open until the VM process restarts.
- Severity: wrong result, and a resource leak across guest reboots.
- Proof: test `reset_and_reinit_start_a_fresh_session` with the `Fixture` of
  `fs/device.rs` (the limit is 32 under test). Send `LOOKUP` for 31 files.
  Call `f.fs.reset()`, activate again as `Fixture::over` does, send `INIT`,
  then `LOOKUP` a new file and expect `error == 0`. Today it is `-ENFILE`.
- Confidence: high.
- Fixed in card #402. Device reset ends the FUSE session, releases its inodes
  and handles, and keeps the pinned share root for the next guest session.

### F12. vhost-user-fs: a completion interrupt can be lost across a snapshot

- Where: `devices/virtio/fs/vhost_user.rs:457-470`, `622-651` (save),
  `720-732` (restore).
- Fault: `call_worker` returns as soon as `stop` is set and does not serve
  call descriptors that were readable in the same poll. `save_state` stops
  the worker first and then sends `GET_VRING_BASE`. That is the moment at
  which virtiofsd finishes requests in flight and signals the call
  descriptor. The interrupt reaches only the machine that keeps running,
  through the restarted worker. The restore path arms the vrings and
  notifies no queue. A restored guest with a request in flight at save time
  waits forever.
- Severity: wrong result (a hung file request after a restore).
- Proof: test `restore_into_an_active_device_notifies_every_queue` with
  `FakeBackend::new(..).serve()` and the recording interrupt of the tests.
  Activate, `restore_state` with an active section, and assert that
  `notify_queue` was called for both queues. A second test
  `save_delivers_calls_raised_during_get_vring_base`: the fake writes the
  call eventfd while it handles `GET_VRING_BASE`, and the interrupt must be
  recorded before `save_state` returns.
- Confidence: medium. Both code facts are confirmed. Where the VMM saves the
  transport's interrupt state relative to this section was not read.
- Fixed in card #416. `save_state` delivers the calls that are readable
  after `GET_VRING_BASE` before it re-arms the rings, and a restore of an
  active section notifies every armed queue (also when the section is applied
  by the next `activate`). The two proof tests and
  `restored_state_applied_at_activation_notifies_every_queue` fail before
  the fix and pass after it. The save test checks the order (the interrupt
  comes before the re-arm), because the restarted worker would otherwise
  deliver the call a moment after `save_state` returns.

### F13. virtio-fs: `READDIRPLUS` leaks a lookup reference and hides entries when the inode table is full

- Where: `devices/virtio/fs/passthrough.rs:895-919`.
- Fault: `session.resolve` raises `lookups` before `add_entry` runs. When
  `add_entry` returns `Ok(0)` (buffer full) or an error, the loop leaves
  without undoing it. The guest never saw the entry and never sends
  `FORGET`. The inode and its two `O_PATH` descriptors stay for the session
  and count against `SESSION_LIMIT`. Also `Err(_) => continue` swallows the
  `ENFILE` of `entry_for`, so files vanish from listings with no error when
  4096 inodes are live.
- Severity: wrong result.
- Proof: test `readdirplus_does_not_leak_the_entry_that_did_not_fit` with
  `scratch()`. `opendir(ROOT_ID)`, call `fs.readdirplus(..)` with an
  `add_entry` that returns `Ok(len)` one time and then `Ok(0)`, and assert
  that the path of the second entry is absent from
  `fs.inner.lock().by_path`. Test
  `readdirplus_reports_a_full_inode_table`: create 40 files and assert that
  the call returns `ENFILE` or lists all 40.
- Confidence: high.

### F14. virtio-net on a TAP: one failed packet write or read latches `tap_error` for the life of the process

- Where: `devices/virtio/net/worker.rs:74-76` and `175-177` set it.
  `devices/virtio/net.rs:361-364` and `1989-1991` read it.
  `net.rs:1882-1910` (`reset`) does not clear it. No code clears it.
- Fault: any error from `tap.send` or `tap.receive` other than `WouldBlock`
  and `Interrupted` sets `tap_error`. From then on the link reads as down
  and every `save_state` fails with "TAP backend: ...", so the machine can
  take no snapshot. A guest reboot does not recover it. The guest can cause
  a write error: `prepare_vnet_tx` validates the header with
  `Header::validate`, which does not check the `hdr_len` field, and the
  kernel's TUN driver refuses a packet whose `hdr_len` is larger than the
  packet with `EINVAL`. (The kernel behaviour is from memory of
  `tun_get_user` and was not run.)
- Severity: wrong result that a hostile guest can cause: no snapshot and no
  network until the VM process restarts.
- Proof: test `a_failed_tap_write_does_not_outlive_a_device_reset` with
  `tap::test_backend` and a TAP stand-in whose write fails (a socket whose
  peer is closed). Transmit one packet, assert that `save_state` fails, call
  `reset()` and activate again, and assert that `save_state` succeeds and
  the link is up. Today both still fail. For the guest trigger, run in
  `tap_integration_tests.rs` with a real TAP: transmit one frame with
  `hdr_len = 0xFFFF` and `gso_type = 0`.
- Confidence: high that the error latches. Medium for the guest trigger.

### F15. virtio-console: an in-place restore does not tell the guest the live host connection state

- Where: `devices/virtio/console.rs:1072-1095` (`restore_state`) and `312`.
- Fault: `restore_state` replaces `pending` with the snapshot's content. The
  worker then only corrects its flag (`state.host_open = clients[i].is_some()`).
  No `PORT_OPEN` event is queued for the difference. A snapshot taken with
  no client and restored while a client is connected leaves the guest with
  the host side closed, and Linux blocks writes on such a port. The reverse
  case leaves the guest writing into nothing.
- Severity: wrong result (the agent port can hang after a restore).
- Proof: test `restore_reannounces_the_live_host_connection` with
  `Fixture::new()` and `device_ready(8, ..)`. `save_state()` with no client.
  Connect a `UnixStream` to `agent_sock` and `wait_for` `host_open`. Then
  `quiesce`, `restore_state`, `resume_snapshot`, `post_ctrl_rx`, kick queue
  2. Assert that `ctrl_delivered()` gains `(2, EV_PORT_OPEN, 1, [])`. Today
  nothing arrives.
- Confidence: medium. The code path is confirmed. Whether vmlab always
  reconnects after a restore, which would hide it, was not checked.

### F16. virtio-console: a second host client on a busy port makes the worker spin

- Where: `devices/virtio/console.rs:322-328` and `380`.
- Fault: every listener is polled with `POLLIN` at all times, but `accept`
  is skipped while the port has a client. With a connection waiting in the
  backlog, `poll` returns at once on every pass.
- Severity: wrong result. One host CPU is busy until the first client
  leaves. The trigger is on the host side, not in the guest.
- Proof: test `second_client_in_the_backlog_does_not_spin_the_worker` with
  `Fixture::new()`. Connect two `UnixStream`s to `agent_sock`, `wait_for`
  `host_open`, read the worker thread's CPU clock (`pthread_getcpuclockid`)
  across a 300 ms sleep, and assert less than 50 ms.
- Confidence: high.

### F17. virtio-sound: restored stream parameters are not validated, and `advance` then divides by zero

- Where: `devices/virtio/sound/mod.rs:1132-1135` (restore) with `880-885`.
- Fault: `restore_state` takes `period_bytes` and `channels` as stored.
  `advance` computes `due.min(period + due % period)` with
  `period = period_frames()`. A `period_bytes` of 0, or of less than one
  frame, panics on a started stream.
- Severity: crash of the host process, from a damaged or crafted snapshot
  only. `SET_PARAMS` validates the same values on the live path.
- Proof: test `restore_rejects_a_zero_period`: `start_streams()`,
  `save_state()`, zero the 4 bytes of `period_bytes` of stream 0,
  `restore_state` must return `Err`. Today it returns `Ok` and
  `guest.advance(period_time())` panics.
- Confidence: high.

### F18. virtio-console: control traffic stalls or is lost when `pending` is full

- Where: `devices/virtio/console.rs:690-693` with `473-477` and `127-135`.
- Fault: `process_ctrl_tx` stops when the room in `pending` is below
  `sh.ports.len().max(2)`. A later kick of queue 2 only runs
  `flush_pending`, so the unread messages of the control transmit queue wait
  until the guest kicks queue 3 again. While `pending` is full,
  `enqueue_ctrl` drops host events ("control queue full, dropping"), so a
  connect or disconnect in that window never reaches the guest.
- Reach: only a guest that withholds control receive buffers.
- Severity: wrong result.
- Proof: test `control_tx_resumes_when_control_rx_buffers_arrive` with
  `Fixture::new()`. Connect a client. With no `post_ctrl_rx`, call
  `send_ctrl(1, EV_PORT_READY, 1)` 14 times and wait until
  `vqs[3].used_idx` stops below 14. Then `post_ctrl_rx` 24 buffers and call
  `queue_notify(2)` only. Assert that `vqs[3].used_idx` reaches 14. Today it
  does not move.
- Confidence: medium to high.

### F19. virtio-fs: `SETATTR` with a size ignores the supplied handle

- Where: `devices/virtio/fs/passthrough.rs:480-482`, `494-496`, `1479-1489`.
- Fault: the handle is validated and then dropped. The truncation reopens
  the inode `O_WRONLY`. So `ftruncate` on a writable descriptor of a file
  whose mode forbids writing (created `0444`, opened `O_RDWR` at create)
  fails with `EACCES` when the VM process is not root.
- Severity: wrong result.
- Proof: test `ftruncate_through_a_handle_ignores_the_file_mode` with
  `scratch()`: `fs.create(.., CreateIn { flags: O_RDWR, mode: 0o400, .. })`,
  then `fs.setattr(ino, attr { st_size: 0 }, Some(handle), SetattrValid::SIZE)`
  must be `Ok`. Skip the test when it runs as root.
- Confidence: high.

### F20. virtio-pci: the driver can change the feature set of a live device

- Where: `devices/virtio/pci.rs:723-730` (`GUEST_FEATURE` writes are
  accepted in every state) and `590-597` (each status write that carries
  `FEATURES_OK` calls `device.ack_features` again).
- Fault: after `DRIVER_OK` the guest writes other driver features and
  writes the status again. The device gets a new feature set while its
  queues run. The queues keep the `EVENT_IDX` setting of the activation.
  virtio-net switches the header length between 10 and 12 bytes
  (`net.rs:199-205`, `1781-1786`) and moves the control queue index
  (`net.rs:1214-1218`) in the middle of traffic. The block device changes
  whether its read walker accepts indirect tables (`block.rs:573`). VIRTIO
  1.2 section 3.1.1 fixes the feature set at `FEATURES_OK`. No memory-safety
  effect was found: each device bounds its own accesses.
- Severity: wrong result, for a guest that breaks the protocol.
- Proof: test `features_are_fixed_after_features_ok` in the `pci.rs` tests
  with `Guest::new()` and `negotiate_and_activate(0)`. Then write
  `GUEST_FEATURE_SELECT = 0`, `GUEST_FEATURE = 0` and the same status again.
  Assert that the device still has the first set (for block:
  `save_state()` bytes 5..13, the acknowledged features, are unchanged).
  Today they change.
- Confidence: high.

### F21. PCI root: a queue doorbell stays registered while memory decoding is off

- Where: `vmlab-hv-vmm/src/pci.rs:519-526` and `759-766`. The address given
  to `set_bar_address` depends on `bar.addr` and `bar.sizing` only.
  `devices/virtio/pci.rs:459-491` registers the ioeventfds from it.
- Fault: `decode_mmio` (`631-633`) skips a function whose command register
  has memory decoding off, on a q35 root. The ioeventfd is not removed in
  that state. A guest write to the notify address still kicks the queue
  inside KVM, while every other register of the BAR is not decoded. The
  kernel path also matches only the exact address `base + 4 * queue`, while
  `bar_write` accepts each offset of the notify page (`pci.rs:1054-1059`).
  That second difference is harmless.
- Severity: wrong result (the device decodes an address it should not).
- Proof: test `doorbells_follow_the_memory_enable_bit` in the
  `vmlab-hv-vmm/src/pci.rs` tests with `FakePci` on a `PciRoot::q35` root.
  Program BAR 0, set command bit 1, then clear it. Assert that the last
  entry of `placements` for the BAR is `None`. Today it is `Some(addr)`.
- Confidence: high.

### F22. virtio-gpu: a fenced release after the loss of the host GPU is answered with an error

- Where: `devices/virtio/gpu/mod.rs:1124-1135`.
- Fault: `gpu/lost.rs:8-11` and `renderer.rs:460-469` say that a release
  answers `OK_NODATA` after a loss. `chain()` then calls `renderer.fence()`,
  which returns `UNSPEC` when the GPU is lost (`renderer.rs:450`), and
  writes that over the answer (`Err(error) => kind = error`). The same
  happens to `GET_DISPLAY_INFO` and `GET_EDID` when they carry the fence
  flag.
- Severity: wrong result.
- Proof: test `a_fenced_release_after_a_loss_is_still_ok`, gated on
  `VMLAB_HV_TEST_RENDER_NODE`. Build a `Gpu`, `enable_3d`, activate it with
  the `QueueFixture` layout, create a resource, call
  `gpu.lose_host_gpu("test")`, `put` a `command(0x102, &[1, 0])` with the
  fence flag set, `notify`, and assert `NODATA` in the response header.
- Confidence: high for the code. Whether real drivers fence a release was
  not checked.

### F23. virtio-console: used length is reported for chains the device only read

- Where: `devices/virtio/console.rs:601`, `615`, `714`.
- Fault: transmit and control transmit chains are completed with the number
  of bytes read. The used length counts bytes the device wrote (VIRTIO 1.2
  section 2.7.8.2), which is 0 here. `reader.available_bytes() as u32` also
  truncates. virtio-net does the same for transmit chains
  (`net.rs:1645`, `1679`).
- Severity: cosmetic.
- Proof: test `transmit_chains_complete_with_zero_used_length`: as
  `guest_output_reaches_the_connected_client`, then assert
  `vqs[5].used_entry(&mem, 0).1 == 0`.
- Confidence: high.

### F24. virtio-gpu: a response buffer too small for the payload gets no error header

- Where: `devices/virtio/gpu/mod.rs:1138-1140`.
- Fault: the command has run. When the writable part holds the 24-byte
  header but not the payload, `chain()` returns used length 0 and writes
  nothing.
- Severity: cosmetic.
- Proof: test `a_short_response_buffer_gets_an_error_header`:
  `QueueFixture::new(FEATURES)`, `f.put(0, &display_info(), 24)`,
  `f.notify()`, and assert `f.used_entry(0) == (0, 24)` with an error code
  in the header. Today it is `(0, 0)` and the buffer is untouched.
- Confidence: high.

## Notes

These have no practical test, or were not confirmed.

- N1. `QueueWorker` (`devices/virtio/worker.rs:224-243`) panics the calling
  thread with `expect` when its worker thread is gone. The worker ends on a
  panic in the wrapped device's `queue_notify` or on an `epoll_wait` error
  (`138-143`). Then the next reset or snapshot barrier panics a vCPU or the
  control thread. No panic was found in `Block::queue_notify`, so no test.
- N2. `Block::activate` (`block.rs:800-806`) and the same code in
  `fs/device.rs:267`, `394` and `sound/mod.rs:942` ignore a failed
  `Queue::try_from(state)` and run with the queue the transport built, which
  starts at index 0. A damaged snapshot then replays or skips requests. The
  console returns an error for the same case, but after it replaced part of
  its state (`console.rs:1080-1090`).
- N3. A descriptor that crosses two guest-memory regions is refused by
  `get_slice` in virtio-net (`net.rs:1628`, `net/buffer.rs:34`) and
  virtio-fs (`fs/fuse/transport.rs:446-457`). The packet or request is
  dropped with used length 0. The block device handles the case. It
  needs a buffer that lies across the boundary of two regions that touch.
  Whether the machine has such regions was not checked.
- N4. `apply_common_write` (`pci.rs:773-778`) accepts `queue_enable = 1`
  after `DRIVER_OK`. The register then reads 1, but the device never gets
  the queue. Linux enables all queues before `DRIVER_OK`.
- N5. `status_write` (`pci.rs:580-611`) builds the status from the written
  value only. A write that is not zero clears `DEVICE_NEEDS_RESET` after a
  failed activation, and the device then shows `DRIVER_OK` without being
  active.
- N6. When the guest turns MSI-X off while a vector is pending in the PBA,
  or while `isr` is set, no INTx is raised for it (`pci.rs:810-822`).
- N7. `PciRoot::config_write` holds the root lock while it waits for the
  device lock (`vmlab-hv-vmm/src/pci.rs:513-526`). A device that is slow in
  `bar_write`, for example a block reset that waits for disk I/O, stops
  every other PCI configuration access and every MMIO decode for that time.
- N8. The guest can place a BAR on any address. A notify region placed on
  another device's registers takes the writes to those addresses in KVM.
  Only the guest's own devices are affected.
- N9. virtio-fs, from the read of `passthrough.rs` and `vhost_user.rs`: `linkat` with `AT_EMPTY_PATH` needs `CAP_DAC_READ_SEARCH`
  on host kernels before about 6.10 (`passthrough.rs:1411-1419`).
  `Fs::reset` in `vhost_user.rs:602-610` disables the vrings and never
  stops them with `GET_VRING_BASE`. A failed `GET_VRING_BASE` in
  `save_state` (`636-651`) leaves the live device stopped. A snapshot with
  no root inode is accepted, and the next restore panics at
  `inodes[&ROOT_ID]` (`passthrough.rs:252`). `getattr` with a handle does
  not check that the handle belongs to the inode (`456-460`). The guest can
  create setuid files and symlinks with any target on the host share. The
  server does not follow them, but host tools that read the share might.
- N10. virtio-sound: playback pacing can be passed by the guest against
  itself with a `period_bytes` of one frame (`tx_credit.saturating_sub`).
  For an rx buffer whose capacity is not a multiple of the frame size the
  status sits beyond the reported used length (`sound/mod.rs:712`, `839`).
  `reset` clears `paused` (`971`). Sound has no `quiesce`, so whether
  `advance` can run between the save of guest memory and the save of the
  device state depends on the VMM code that was not read.
- N11. virtio-console: `process_tx` drops the lock between collecting the
  output and appending it to `tx_stage` (`console.rs:633-643`). A reset in
  that gap lets bytes from before the reset reach the client. It has no
  deterministic test.
- N12. virtio-net: in `receive_merged_header` (`net.rs:983-998`) a write
  error after chains were popped returns without publishing them. The guest
  loses those buffers until it resets the device. The trigger is a buffer
  that becomes invalid between two reads of the same descriptor.
- N13. virtio-gpu, not confirmed or without a host GPU to test on:
  `RESOURCE_UNMAP_BLOB` calls `mapper.unmap` before `gpu.unmap`, and a
  failure of the second leaves `mapped` set, so a later reset never settles
  and `Backend::drop` aborts the process (`renderer.rs:1802-1813`,
  `224-229`). Fences of destroyed contexts may never signal
  (`renderer.rs:941`), and then every `synchronize()` waits its full 5 s
  under the device lock (`1183-1203`). `present_copy` writes into a blob's
  `host_map` without a check that the mapping is writable (`1248-1249`).
  `lease.retire()` waits with no timeout under the device lock
  (`476-481`, `908-911`). Commands are not gated on the negotiated
  features, apart from bit 23 and EDID. `SET_SCANOUT_BLOB` accepts a stride
  up to `size / height`, and `readback` then allocates `stride * height`
  (`1737-1748`, `1080`). The trace probe in `present_copy` uses unchecked
  guest values before `copy::run` validates them, with the display trace on
  only (`1288-1298`).

## Other work seen

- `devices/virtio.rs:7-8` still describes `fs` as "the vhost-user-fs
  front-end whose queues an external virtiofsd serves". The in-process
  passthrough server is the one the product uses.
