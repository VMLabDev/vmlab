# Second review: vmlab-hv virtio devices and the virtio-pci transport

Card #890, 2026-11-24. This is a fault list. It changes no code. The first
review of this area is `docs/reviews/virtio-devices.md` (card #379). This
document lists faults that the first pass missed and faults that its fixes
left open. It does not repeat a finding of the first pass, except where the
fix is incomplete, and then it says so.

All paths are relative to `hv/crates/`. Line numbers are those of commit
`b9fd1de4` (`next` at the start of the card). `devices/` stands for
`vmlab-hv-devices/src/`.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. The three facts about the `virtio-queue` 0.18.0 crate that the
first review checked in the crate source are used here as given: a chain may
walk an indirect table of up to 65535 descriptors whether or not the feature
was negotiated, descriptors may alias guest memory, and `Reader::new` puts no
limit on the total length of a chain.

Two more facts carry findings and were not checked in a source on this card.
They are from memory, and the finding says so where it depends on them:

- `Queue::disable_notification` sets `VRING_USED_F_NO_NOTIFY` when
  `EVENT_IDX` is off, and does nothing when it is on. The comments at
  `devices/virtio/net.rs:900-901` and `devices/virtio/block.rs:444-448`
  agree with this.
- The Linux FUSE client, with the writeback cache on, reads a page through
  the file handle that writes it (`fuse_write_begin`), and it sends `FORGET`
  only when it drops an inode from its own caches.

Severity words are those of the first review: data loss, crash of the VM or
host process, security against a hostile guest, wrong result, cosmetic.
"Confidence" says how sure the reading is.

## What was read and what was not

Read in full, without the test modules:

- `devices/virtio.rs`, `devices/virtio/pci.rs`,
  `devices/virtio/transitional.rs`, `devices/virtio/worker.rs`
- `devices/virtio/block.rs`
- `devices/virtio/net.rs`, `net/worker.rs`, `net/buffer.rs`, `net/tap.rs`,
  `net/attach.rs`
- `devices/virtio/console.rs`, `devices/virtio/sound/mod.rs`
- `devices/virtio/fs/device.rs`, `fs/passthrough.rs`
- `vmlab-hv-proto/src/net.rs` (the header, the layout parser, segmentation,
  the flow hash)
- `vmlab-hv-vmm/src/pci.rs` lines 30-855 (the root: BARs, decoding, the
  cache of decoded addresses, save and restore of the BAR state)

Read in part:

- `devices/virtio/gpu/mod.rs` (lines 500-1470: holding of chains, the 2D
  commands, `chain`, `queue_notify`, reset) and `gpu/snapshot.rs` (restore,
  lines 120-283)
- `fs/fuse/server/sync_io.rs` (lines 14-1040: the dispatcher and each
  request decoder), `fs/fuse/server/mod.rs` (the server state and
  `get_message_body`)
- `net/checksum.rs` (`transmit` only)
- `vmlab-hv-vmm/src/snapshot.rs` and `reset.rs`, only for the order in which
  device sections and the PCI root's BAR section are restored
- `devices/audio/`: only the ring sizes and `write_period` of the PulseAudio
  and PipeWire sinks, for F6
- `vmlab-hv/src/devices.rs`: only how the block and virtio-fs devices are
  built

Not read:

- `fs/vhost_user.rs`, `fs/fuse/transport.rs`, `fs/fuse/file_traits.rs`,
  `fs/fuse/file_buf.rs`, `fs/fuse/abi/`, `fs/fuse/filesystem/`,
  `fs/fuse/server/guest_log.rs`
- `gpu/renderer.rs`, `gpu/scanout.rs`, `gpu/lost.rs`, `gpu/copy.rs`,
  `gpu/compose.rs`, `gpu/vblank.rs`, `gpu/pacer.rs`, `gpu/teardown.rs`,
  `gpu/modifiers.rs`. The 3D path of the GPU was not reviewed on this card.
- `net/checksum.rs` apart from `transmit`, `net/tap_integration_tests.rs`
- every test module, apart from the names of fixtures and helpers
- `devices/display/virtio_vga.rs`, which wraps the GPU

## Findings, worst first

### F1. virtio-fs: `unlink` and `rmdir` are not recorded, so a create at the removed name re-points the removed inode (the fix of the first review's F1 is incomplete)

- Where: `devices/virtio/fs/passthrough.rs:657-669` (`unlink`, `rmdir`) and
  `1132-1144` (`entry_for`).
- Fault: the first review's F1 was fixed for `rename` (card #391). Its fix
  note says that `unlink` and `rmdir` still leave the removed name in
  `by_path`. That is the same fault through another door. `unlink` only
  calls `unlinkat`. The inode keeps its entry in `by_path` for as long as
  the guest holds it: an open handle, a second hard link, or a `FORGET` that
  has not come yet. When the guest then creates or looks up that name,
  `entry_for` finds the stale entry, replaces the inode's descriptor
  (`state.fd = fd`) and returns the old node id for the new file. The guest
  now has one inode for two host files. Its page cache and its attributes
  for the removed file are served for the new one. With the writeback cache
  the guest writes dirty pages through any writable handle of that inode, so
  pages of one file can reach the other.
- Reach: an ordinary guest. One process keeps a log open while another
  removes the file and creates it again (`tail -f log` and a rotation by
  `rm` and `touch`). `ln a b; rm a; echo new > a` reaches it too, because the
  inode lives on through `b`.
- Severity: data loss. Reads and writes of one file reach the other.
- Proof: test `unlink_then_create_at_old_name_keeps_the_open_inode` in the
  passthrough tests, with `scratch()`, `lookup()`, `ctx()` and `name()`.
  Look up `hello.txt` as X and open it. Call `fs.unlink(ROOT_ID, "hello.txt")`.
  Call `fs.create` for `hello.txt`. Assert that the created entry has a node
  id other than X, and that `fs.getattr(X, None)` reports `st_nlink == 0`
  (the removed file). Today the id is X and `getattr` describes the new
  file. A twin `rmdir_then_mkdir_keeps_the_held_directory` does the same
  with a directory that has an open directory handle.
- Confidence: high for the server. Medium for the mixing of written pages in
  the guest, which depends on the client fact above.

### F2. virtio-blk: `T_DISCARD` serves every range of a chain, with no limit, under the device lock

- Where: `devices/virtio/block.rs:734-762` (`discard`), with the advertised
  `max_discard_seg = 1` at `394-395` and the worker at
  `devices/virtio/worker.rs:124-130`.
- Fault: `discard` loops `while reader.available_bytes() > 0` and calls the
  image once for each 16-byte range. The configuration tells the driver
  that a request has one range, but nothing enforces it. The guest chooses
  the readable length of a chain: descriptors may alias one buffer, and each
  one may be as long as guest memory. A chain of 254 descriptors over 1 GiB
  each holds about 1.7e10 ranges. With an indirect table it is far more.
  Every range that fits the disk is one call into the image. The request
  does not end in any time that matters. The worker thread holds the device
  mutex for all of it, so a vCPU that reads the device configuration or
  writes the status, and every snapshot `quiesce`, waits behind it
  (`worker.rs:272-276`, `296-300`).
- This is the hang of the first review's F6 by another route. The fix of F6
  bounded the loop on a broken `avail.idx`. The fix of F5 bounded the bytes
  a request stages. Neither bounds the number of ranges.
- Severity: security against a hostile guest. A hung VM that the host cannot
  pause or snapshot, and one busy host CPU.
- Proof: test `discard_serves_at_most_the_advertised_segments` in the block
  tests, with `Fixture`, `desc()` and `MemDisk::discard_log()`. A 1 MiB
  `MemDisk`, with `VIRTIO_BLK_F_DISCARD` acknowledged. One chain: the 16-byte header (`T_DISCARD`), 8 readable
  descriptors that all point at the same 4 KiB buffer filled with the range
  `(sector 0, 1 sector, flags 0)`, and the status byte. Assert status
  `S_IOERR` or `S_UNSUPP`, or that the discard log holds at most one entry.
  Today the status is `S_OK` and the log holds 2048 entries.
- Confidence: high.

### F3. virtio-fs: a write-only handle is opened write-only on the host while the writeback cache is negotiated

- Where: `devices/virtio/fs/passthrough.rs:1507-1522` (`host_open_flags`),
  `1247-1248` (`open_file`), `774` (`create`), `440` (`init` asks for
  `WRITEBACK_CACHE`), and the read path `820-825`.
- Fault: with the writeback cache the guest kernel fills a page before it
  writes part of it, and it sends that `READ` on the handle of the file that
  is being written. So the server must be able to read through a handle the
  guest opened `O_WRONLY`. `host_open_flags` clears `O_APPEND` for this mode
  (the fix of the first review's F3) but keeps the access mode. The host
  descriptor is write-only, and `read` on it fails with `EBADF`. The guest's
  `write` then fails. libfuse's `passthrough_ll` and virtiofsd open such a
  handle `O_RDWR` for this reason.
- Reach: an ordinary Linux guest. `echo x >> /share/file` on a file that the
  guest has not read yet and whose size is not a multiple of the page size:
  the shell opens it `O_WRONLY|O_APPEND`. A program that rewrites bytes in
  the middle of a file through a write-only descriptor reaches it too.
- Severity: wrong result. The write fails with an error; no data is lost
  silently.
- Proof: test `write_only_handle_under_writeback_serves_reads` in the
  passthrough tests. After `fs.init(FsOptions::all())`, look up `hello.txt`,
  `fs.open` it with `O_WRONLY`, and call `fs.read` on that handle with
  `SinkWriter`. Assert that it returns the file's bytes. Today it returns
  `EBADF`. An end-to-end step settles the guest side: write a 10-byte file
  on the host, then run `echo more >> file` in the guest.
- Confidence: high for the server. Medium to high for the guest: the kernel
  behaviour is from memory of `fuse_write_begin` and was not run.

### F4. virtio-fs: a share stops answering after the guest has looked at 4096 files

- Where: `devices/virtio/fs/passthrough.rs:49` (`SESSION_LIMIT`), `1146-1148`
  (`entry_for`), `766-768` (`create`), `1318-1320` (`READDIRPLUS`).
- Fault: every inode the guest was given stays in the session until the
  guest sends `FORGET`. The table holds 4096 inodes and nothing is evicted.
  The Linux client sends `FORGET` only when it drops the inode from its own
  caches, which it does under memory pressure, not when the one-second entry
  timeout ends. So after the guest has looked up 4096 files of one share,
  every lookup of another file, every create and every `READDIRPLUS` returns
  `ENFILE`. The first review's F11 and F13 named this limit for a guest
  reboot and for a listing. The limit itself is the fault for an ordinary
  guest: `ls -l` in a directory of 5000 files, `find`, `git status` or a
  build over a source tree reach it.
- Severity: wrong result. The share fails with "Too many open files in
  system" until the guest drops its caches or the device is reset.
- Proof: test `a_tree_larger_than_the_inode_table_can_be_walked` with
  `scratch()` (the limit is 32 under test). Create 40 files on the host.
  Look up each one in turn and send no `FORGET`. Assert that every lookup
  returns `Ok`. Today the 32nd fails with `ENFILE`. The fix has to give up
  descriptors of inodes that have no open handle and reopen them by path on
  the next use, so the test also reads the first file again at the end.
- Confidence: high for the server. Medium to high for the guest behaviour
  (see "How the review was done").

### F5. virtio-net: the transmit queue stalls when the switch connection drops while output is held back

- Where: `devices/virtio/net.rs:1642-1646`, `1718-1720` and `1733`
  (`process_tx_queue` stops and does not re-arm the kick while `tx_out`
  holds `MAX_TX_STAGE`), `486-504` (the worker drops the link on a failed
  flush or on `POLLHUP` and does not serve the queue), `511-531` (reconnect).
- Fault: when the switch does not read, `tx_out` fills to 1 MiB. The device
  then leaves the next chain in the queue (`go_to_previous_position`), sets
  `blocked`, and does not call `enable_notification`: it relies on `POLLOUT`
  to come back. If the connection dies in that state, `drop_link` clears
  `tx_out` and the worker goes on to reconnect. Nothing serves the transmit
  queue again. With `EVENT_IDX` the guest does not kick, because the
  `avail_event` it sees is the old one. Without `EVENT_IDX` the
  `NO_NOTIFY` flag is still set. The chains that wait are never completed,
  the guest's transmit ring stays full, and the NIC sends nothing after the
  link is up again. A kick of another queue of the same NIC ends the stall,
  because all queues share one wakeup, but receive kicks are suppressed in
  the same way most of the time.
- The path inside `process_tx_queue` is right: a failed flush there loops
  once more and drops the waiting chains (`1728-1735`). Only the three
  `drop_link` calls of the worker loop miss it.
- Reach: the lab daemon that runs the switch stops or restarts while a
  guest sends at a rate that fills 1 MiB.
- Severity: wrong result. No transmit until the guest resets the device.
- Proof: test `transmit_resumes_after_the_link_drops_under_backpressure` in
  the net tests, with `Fixture`, `TestVq` and `wait_for`. Connect a switch
  socket and do not read from it. Post transmit chains and kick until
  `tx_out.len() >= MAX_TX_STAGE` and the used index of queue 1 stays behind
  its avail index. Close the switch socket and accept the device's new
  connection. Send no kick. Assert with `wait_for` that the used index
  reaches the avail index. Today it does not move.
- Confidence: medium to high. The control flow is confirmed. The behaviour
  of `disable_notification` is from memory (see above).

### F6. virtio-sound: a period larger than the host sink's ring loses audio

- Where: `devices/virtio/sound/mod.rs:100-102` (`queued_target`), `530-536`
  (the pacing test) and `563-580` (the push), with the ring sizes at
  `devices/audio/pulse.rs:39` (1024 frames), `devices/audio/pipewire.rs:22`
  and `147` (4096 frames) and the overflow rule at
  `devices/audio/ring.rs:61-66`.
- Fault: a tx message is played when the sink holds less than
  `queued_target`, which is one guest period or 256 frames, whichever is
  more. Then the whole message goes into the sink in pieces of 512 frames,
  one after the other. The device never asks how much room the sink has.
  The sinks keep a ring of fixed size and drop the oldest audio when it is
  full. `SET_PARAMS` accepts any `period_bytes`, so a guest period of 2048
  frames, which is about 43 ms at 48 kHz, puts 2048 frames into a ring of
  1024 on the PulseAudio sink: half of every period is dropped. On the
  PipeWire sink the same happens from 4096 frames. The first review's F5
  note says that the two caps bound what `set_params` can cost in memory.
  They do not bound it against the ring.
- Reach: an ordinary guest whose audio stack picks a long period. ALSA
  programs that ask for the default buffer do.
- Severity: wrong result (gaps in playback).
- Proof: test `a_period_larger_than_the_sink_ring_is_not_dropped` in
  `sound/tests.rs`. A `HostAudio` whose sink is an
  `SpscRing::with_capacity_bytes(4 * 256 * frame_bytes)` that nobody reads,
  and that records the return value of `push_drop_oldest`. Set a period of
  2048 frames, start the stream, advance one period time, submit one period
  on `VQ_TX`. Assert that the sink dropped 0 bytes: the device holds what
  does not fit, or completes the message when the ring has taken all of it.
  Today the sink reports 4096 dropped bytes (1024 frames of S16 stereo).
- Confidence: high for the mechanism. Medium for how often a real guest
  picks such a period.

### F7. virtio-fs: the umask of the host process is applied to every file and directory the guest creates

- Where: `devices/virtio/fs/passthrough.rs:777-782` (`create`), `650`
  (`mkdir`), `627` (`mknod`), with the ignored `_umask` arguments at `614`
  and `640`.
- Fault: the server does not negotiate `DONT_MASK`, so the guest kernel has
  applied the guest's umask to `mode` already. `openat`, `mkdirat` and
  `mknodat` then apply the umask of the vmlab-hv process a second time. No
  code in the hypervisor sets that umask, so it is the one the lab daemon
  was started with, usually 022. A guest user with umask 002 creates a file
  with mode 0664 and gets 0644. `mkdir -m 777` gets 0755. Group-shared
  directories in the share lose the group write bit.
- Severity: wrong result.
- Proof: test `created_nodes_keep_the_mode_the_guest_sent` with `scratch()`.
  `fs.create` with `mode: 0o666`, `fs.mkdir` with `0o777`, and assert that
  the returned attributes carry those permission bits. Skip the test when
  the `Umask:` line of `/proc/self/status` is 0000. On helios (umask 022)
  the modes are 0644 and 0755 today.
- Confidence: high.

### F8. virtio-pci: `FEATURES_OK` is still accepted after `DRIVER_OK` (the fix of the first review's F20 is incomplete)

- Where: `devices/virtio/pci.rs:570-574` (`set_driver_features`), `599-609`
  (`status_write`), `639` (`activate` runs with no accepted set).
- Fault: the fix of F20 (card #428) fixes the feature set once it was
  accepted. It does not cover a driver that never sent `FEATURES_OK` before
  `DRIVER_OK`. The device is then activated with the set 0 and `negotiated`
  stays `None`. The guest can now write `GUEST_FEATURE` (accepted, because
  no set is fixed) and then a status with `FEATURES_OK`. `status_write`
  accepts the set and calls `device.ack_features` on the live device. This
  is the fault of F20: virtio-net changes its header length and the index of
  its control queue in the middle of traffic, while the queues keep the
  `EVENT_IDX` setting of the activation.
- Severity: wrong result, for a guest that breaks the protocol. No
  memory-safety effect was found.
- Proof: test `features_cannot_be_accepted_after_driver_ok` in the `pci.rs`
  tests with `Guest::new()`. Write the status `ACKNOWLEDGE|DRIVER`, set up
  and enable queue 0, write `ACKNOWLEDGE|DRIVER|DRIVER_OK`. Then write
  `GUEST_FEATURE_SELECT = 0`, `GUEST_FEATURE = 1 << 28` and the status with
  `FEATURES_OK` added. Assert that the status reads without `FEATURES_OK`
  and that bytes 5..13 of the block device's `save_state()` are still 0.
  Today the status has `FEATURES_OK` and the bytes carry the new set.
- Confidence: high.

### F9. virtio-pci: a one-byte write to the low byte of the MSI-X message control turns MSI-X off

- Where: `devices/virtio/pci.rs:943-953` (the modern `config_write`). The
  transitional path at `devices/virtio/transitional.rs:212-243` is right.
- Fault: the two bytes of the message control are gathered into
  `[0; 2]`. A write that carries only the byte at 0x42 leaves the high byte
  at 0, and `msix_control_write` then clears the enable bit and the function
  mask, which live in the high byte. The low byte holds only the read-only
  table size. The transitional path starts from the current value and has no
  such effect. The PCI root passes a one-byte guest write through as one
  byte (`vmlab-hv-vmm/src/pci.rs:514-548`).
- Reach: a guest that saves and restores configuration space one byte at a
  time. The write to 0x43 that follows repairs the bits, so the visible
  effect is a short time on INTx. Linux writes the word.
- Severity: wrong result, small.
- Proof: test `a_byte_write_to_msix_control_keeps_the_enable_bit` in the
  `pci.rs` tests. Enable MSI-X with a two-byte write at 0x42. Call
  `config_write(16, 2, &[0])`. Assert that bit 31 of `config_read(16)` is
  still set. Today it is clear.
- Confidence: high.

### F10. Console, sound, virtio-fs and GPU interrupt a driver that turned interrupts off, and the console interrupts for nothing

- Where: `devices/virtio/console.rs:578-580`, `648-650`, `748-750`,
  `867-869`. `devices/virtio/sound/mod.rs:623-630`, `772-779`, `830-837`.
  `devices/virtio/fs/device.rs:173-180`. `devices/virtio/gpu/mod.rs:549-553`,
  `813-818`, `1377-1382`. For contrast, `devices/virtio/block.rs:487-496`
  and `devices/virtio/net.rs:899-909` do the check.
- Fault: without `EVENT_IDX`, `Queue::needs_notification` always answers
  yes. It does not read `VRING_AVAIL_F_NO_INTERRUPT`. Block and net read the
  flag themselves. These four devices do not. The console does not offer
  `EVENT_IDX` at all, so it is always in this case: a Linux guest turns
  interrupts off on the console transmit queues and still gets one for each
  write. The console also asks `needs_notification` when it used no buffer
  (`drain_rx_stage` with an empty stage, at `578`), so each kick of a
  receive queue is answered with an interrupt for nothing.
- Severity: cosmetic (extra interrupts and VM exits).
- Proof: test `completions_respect_avail_no_interrupt` in the console tests,
  as `guest_output_reaches_the_connected_client`, with `1u16` written to the
  avail flags of queue 5 first. Assert that the recording interrupt has no
  entry for queue 5. A twin `a_kick_without_work_raises_no_interrupt` kicks
  queue 4 with no staged bytes and asserts no entry for queue 4.
- Confidence: high.

## Notes

These have no practical test, or were not confirmed.

- N1. virtio-net on a userspace TAP: a packet read from the TAP that fails
  `Header::validate` is treated as a failed backend. `tap.receive` returns
  the error (`net/tap.rs:654-668`) and `process_tap` latches `tap_error`
  (`net/worker.rs:73-76`), which takes the link down and refuses a snapshot
  until the next reset. The first review's F14 named the write side. A
  packet the parser does not accept should be dropped. No packet that the
  kernel hands over and the parser refuses was found, so there is no test.
- N2. virtio-console: each kick is one message on an unbounded channel
  (`console.rs:945-949`). The net device and `QueueWorker` coalesce kicks.
  A guest that kicks from many vCPUs faster than the worker serves makes the
  channel grow. The length of a `std::sync::mpsc` channel cannot be read, so
  there is no test.
- N3. Lists sized by a chain's descriptor count. `ReceiveWriter::new` keeps
  one slice for each writable descriptor (`net/buffer.rs:26-37`), and
  `receive_merged_header` holds up to a queue size of them
  (`net.rs:956-982`): 256 chains of 65535 zero-length descriptors are about
  400 MiB for the time of one frame. A vblank wait keeps the list of the
  writable descriptors of its chain (`gpu/mod.rs:732-736`), and the queue
  holds up to 256 waits. Both are freed again, so this is a short peak and
  not a leak.
- N4. virtio-net reads the negotiated features from an atomic before it
  takes the lock (`net.rs:429`, `460`). A worker that is descheduled between
  the two, across `ack_features` and the activation, places one frame with
  the header length of the old set. It has no deterministic test.
- N5. A virtio reset leaves the pending bits of the MSI-X PBA
  (`pci.rs:539-548`). A vector that was pending and masked before the reset
  fires once when the new driver unmasks it.
- N6. `status_write` builds the status from the written value
  (`pci.rs:589-631`). A write that clears `DRIVER_OK` without a reset leaves
  the device active. A snapshot taken then restores a device that is not
  activated, because restore derives the activation from `DRIVER_OK`
  (`pci.rs:1387`). This is next to the first review's N5.
- N7. `restore_transport` takes the saved `negotiated` set without checking
  it against the offered features (`pci.rs:1276-1278`, `1382-1384`). Block
  and sound apply restored queue states one by one and return on the first
  that fails, with the earlier ones applied (`block.rs:910-912`,
  `sound/mod.rs:1143-1146`). Both need a damaged snapshot.
- N8. The command register keeps the INTx-disable bit (`pci.rs:936`), but
  `deliver` raises the line whatever it says (`pci.rs:192-194`). A guest
  that polls with INTx disabled and without MSI-X keeps the shared line
  asserted until it reads the ISR.
- N9. virtio-console: `enqueue_ctrl` replaces a pending `PORT_OPEN` of the
  same port (`console.rs:129-136`). When the guest posts no control receive
  buffers, a close followed by a new connection reaches it as "open", and
  the guest never learns that the first client left.
- N10. virtio-fs serves each request on the thread that delivered the kick,
  under the transport's lock (`fs/device.rs:285-287`,
  `vmlab-hv/src/devices.rs:209-220`). Block runs behind `QueueWorker`. A
  slow host operation, such as `fsync` on a slow disk, stops that vCPU and
  every other access to the device for its time.
- N11. virtio-fs passes `O_NOATIME` and `O_DIRECT` of the guest to the host
  open (`passthrough.rs:1513-1520`). `O_NOATIME` fails with `EPERM` for a
  file the host process does not own. `READDIR` reports the host's inode
  number for an entry the session does not know (`passthrough.rs:950-954`),
  and the lookup then reports another.
- N12. virtio-net delivers a frame of length 0 from the switch to the guest
  as a packet that holds only the header (`net.rs:659-661`, `841-843`).
- N13. 2D `TRANSFER_TO_HOST` walks the backing list from its start for each
  row (`gpu/mod.rs:1025-1033`, `1209-1233`). With 16384 entries and 2160
  rows that is some ten million steps for one command, under the device
  lock.
- N14. `InterruptState::new` asserts fewer than 64 queues (`pci.rs:150-153`),
  and a transitional block device gets one queue for each vector but one
  (`vmlab-hv/src/devices.rs:108-110`). `Block::with_num_queues` accepts up to
  256. The transitional constructor refuses more than 64 vectors first, so
  no machine description that reaches the assert was found.

## Other work seen

- The first review's text for F6 says that virtio-fs and the GPU serve the
  kick on the vCPU thread. The GPU now runs behind `QueueWorker`
  (`devices/display/virtio_vga.rs:75`).
