# Second review: USB passthrough and emulated USB

Card #1064, 2026-11-26. A second bug-hunting pass over host USB passthrough
and the emulated USB controllers and devices. It lists faults and makes no
fix. Reviewed at `next` commit `d50db6ee`. All line numbers are from that
commit. `usb/` stands for `hv/crates/vmlab-hv-devices/src/usb/`, and `vmm/`
for `hv/crates/vmlab-hv-vmm/src/`. Other paths are from the repository root.

The first pass is `docs/reviews/usb.md` (card #616, findings F1 to F13, notes
N1 to N11). This document continues its numbering, so a code names one finding
across both: F14 to F21 and N12 to N23 are new here. A finding of the first
pass is named again only where its fix is incomplete, and the text says which.

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**.

## How the findings were checked

Every finding is from reading the code. Five were also run on helios, with
temporary tests applied to the checkout of the branch through
`stc run card-1064`, run with `cargo test`, and removed again. No probe test
is committed. The probes used the existing test harnesses (`Driver` in
`usb/tests.rs`, `FakeUsbFs`, `fake_sysfs`, `refused` in
`src/config/hypervisor.rs`) and printed what the code does today:

- F14: `plan_attach` over `host_mounts` with a fake disk `sdb1` (`8:17`) and
  the mount line `36 35 0:45 / /mnt/stick rw,relatime shared:1 - btrfs
  /dev/sdb1 rw,space_cache`. The attach plan came back `Ok` without `--force`.
- F15: `HostUsbDevice::transfer(0x81, ..)`, the fake completes the URB with
  the four bytes `abcd`, then `endpoint_dropped(0x81)`. The next transfers
  answered `Nak`, `Nak`, and after one more fake interrupt `Ack([])`. The four
  bytes never came out. The same held when `reap` had already filed the
  completion before the drop.
- F16: Configure Endpoint for one bulk IN endpoint with Max Burst Size 0, 3,
  4 and 15, and with MaxPStreams 1. Completion codes: 1, 1, 5, 5, and 1.
- F19: `vm_refusals` for `profile = "windows-xp"`, `"windows-9x"` and
  `"windows-legacy"` with `usb = true`. Each list was empty.
- F20: a fake sysfs device whose `product` is `X<ESC>[2J<LF>fake 1-1 row`.
  `scan` kept the string unchanged, `HostDevice::describe` returned it
  unchanged, and `render_list` printed the ESC byte and three lines for one
  device.

F17, F18 and F21 were not run. Each finding names the test that would prove
it. A finding whose test does not reproduce is wrong and should be dropped.
No test below uses a wall-clock limit.

Three findings rest on behaviour of Linux that was stated from memory and not
checked against a kernel tree or a live device:

- F14: btrfs reports an anonymous device number (`0:N`) in
  `/proc/self/mountinfo`, not the `major:minor` of the block device.
- F16: the xHCI driver of a Linux guest writes `bMaxBurst` of the SuperSpeed
  endpoint companion descriptor into the Max Burst Size field
  (`xhci_endpoint_init`), and many USB 3 storage devices report 15.
- F18: one usbdevfs isochronous packet descriptor is one service interval,
  and a high-bandwidth endpoint carries up to three packets of
  `wMaxPacketSize` in one interval.

### What was read

- All of `usb/host/mod.rs` (the non-test part), `usb/host/device.rs`,
  `usb/host/fs.rs` (the non-test part), `usb/host/descriptor.rs` (the
  non-test part), all of `usb/xhci.rs`, `usb/ring.rs`, `usb/state.rs`,
  `usb/mod.rs`, `usb/uhci.rs`, `usb/tablet.rs`, `usb/keyboard.rs`, and
  `usb/README.md:60-190`.
- `vmm/vm.rs:420-600` and `1362-1372`, `vmm/control.rs:160-215`, the xHCI part
  of `vmm/builder.rs` (`325-345`, `436-447`, `1436-1487`, `1716-1725`), the
  two snapshot refusals in `vmm/snapshot.rs`, and the exit path of
  `hv/crates/vmlab-hv/src/main.rs:326-385` with `Vm::wait`.
- All of `src/usb/mod.rs`, `src/labd/usb.rs` and `src/cli/usb.rs`. The USB
  parts of `src/labd/vm.rs` (`teardown`, the Kill rung,
  `usb_return_before_kill`, `usb_ready`, `usb_attach`, `usb_detach`), the
  hot-plug half of `src/labd/hypervisor/vmlab_hv/control.rs` (`send`, `call`,
  `HvHotplug`, the event reader), `usb_list` in `src/supervisor/mod.rs`, and
  where `usb = true` goes in `src/labd/hypervisor/vmlab_hv/describe.rs` and
  `src/config/hypervisor.rs`.
- The test harnesses named above, to write the probes.

### What was not read

- `usb/host/sys.rs` and `usb/host/types.rs` beyond the endpoint descriptor
  helpers. The layout of the usbdevfs structures was not compared with the
  kernel headers.
- `usb/host/fake.rs` beyond what the probes needed, and the existing tests
  except to see which cases they cover.
- The QEMU adapter (`src/labd/hypervisor/qemu.rs`, `src/qemu/cmdline.rs`).
  QEMU runs only aarch64 and riscv64 guests now.
- The wscript bindings in `src/scripting/mod.rs`, the manual pages, and the
  e2e scenario for USB.
- Whether every strong reference to the VM is gone when `vmlab-hv` exits
  (N12). That needs a run, not a reading.
- Nothing was tried with a real USB device. helios has no `dummy_hcd`.

## The state of the first pass

All thirteen findings carry a fix mark. This pass found three fixes
incomplete: F2 (see F14), F5 (see F17 and N12) and F9 (see F21). N1 of the
first pass still holds: `vmm/vm.rs:504-510` stores `hostport` and nothing
compares it.

## Findings, worst first

The order is by the damage and by how many users meet the fault. F20 is the
one security finding and stands low because it only changes what a terminal
shows.

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F14 | data loss | A USB disk with a mounted btrfs filesystem is taken from the host without `--force`: the mount lookup compares device numbers, and btrfs shows none of the disk's (F2 fix incomplete) |
| F15 | wrong result | Stop Endpoint throws away a host transfer that had already completed: bytes the device delivered are lost, and an OUT payload that went through is reported as not sent |
| F16 | wrong result | The xHCI refuses an endpoint with Max Burst Size 4 or more, so a Linux guest cannot configure most SuperSpeed storage devices, and it accepts a streams endpoint it does not implement |
| F17 | wrong result | `stop --force` waits without a limit behind a `usb attach` or `usb detach` that the hypervisor does not answer, and never reaches the kill (F5 fix incomplete) |
| F18 | wrong result | An isochronous TD longer than one packet is cut into several usbdevfs packets, so a high-bandwidth isochronous IN endpoint (a webcam) gets no usable data |
| F19 | wrong result | `usb = true` on a `pc` machine passes validation and the machine then fails to start in the vmlab-hv builder |
| F20 | security | The strings of a USB device are printed raw: a hostile device puts terminal escape sequences and forged rows into `vmlab usb list`, the refusals and the events |
| F21 | wrong result | SET_INTERFACE leaves the URBs of the old alternate setting in flight, and the next TD on that endpoint address gets their cancelled result as a stall (F9 fix incomplete) |

### F14. A mounted btrfs disk is taken without `--force` (data loss)

- Where: `src/usb/mod.rs:505-527` (`host_mounts`: field three of each
  mountinfo line, `major:minor`, is compared with the `dev` files of the block
  devices below the USB device), used by `plan_attach` at
  `src/usb/mod.rs:672-677`.
- Why it is a fault: the fix of F2 finds a mount by the device number of the
  block device. A btrfs mount has an anonymous device number in mountinfo
  (`0:45`), and the block device appears only in the mount source after the
  ` - ` separator (`btrfs /dev/sdb1`). So no line matches, `host_mounts`
  returns nothing, and the attach disconnects `usb-storage` under a mounted,
  writable filesystem. That is the data loss F2 named. btrfs is the default
  filesystem of Fedora and openSUSE, and external backup disks often carry
  it. Any other filesystem that mounts under an anonymous device number has
  the same gap (ZFS, bcachefs). The first pass listed swap, raw opens, network
  adapters and non-boot HID as not covered. btrfs is not on that list.
- Proof: unit test `a_mounted_btrfs_filesystem_is_refused_without_force` in
  `src/usb/tests.rs`, beside
  `plan_attach_refuses_a_mounted_storage_device_without_force`: the fake disk
  `sdb1` with `dev` `8:17`, and the mount table line quoted under "How the
  findings were checked". `plan_mounted(.., force = false)` must return the
  mounted refusal that names `/mnt/stick`. Today it returns `Ok` (run on
  helios).
- How bad: data loss on the host, for one family of filesystems.

### F15. Stop Endpoint throws away a completed host transfer (wrong result)

- Where: `usb/host/mod.rs:903-918` (`endpoint_dropped`: an `InFlight` entry
  is marked discarded with no reap first, and a `Done` entry is removed),
  `usb/host/mod.rs:393` (`reap` drops the completion of a discarded endpoint
  whatever its status), and the caller `usb/xhci.rs:717-759` (Stop Endpoint
  reports the TRB at the dequeue pointer as Stopped with its whole length as
  the residual, line 755, then calls `drop_endpoints`).
- Why it is a fault: a URB that the host kernel has completed stays unseen
  until the next call of `control` or `transfer` reaps it, which is up to one
  poll (1 ms) later. When the guest issues Stop Endpoint in that time, the
  backend treats the URB as still in flight, marks the endpoint discarded,
  and the next reap throws the completion away. For an IN endpoint the device
  sent the bytes and the host acknowledged them on the wire, so they are gone
  from the stream: the guest is told that nothing was transferred. For an OUT
  endpoint the payload reached the device, the guest is told it did not, and
  a guest that sends it again delivers it twice. A real controller reports
  the transferred length in the Stopped event, and Linux and libusb hand the
  partial data to the caller. A guest program that reads a serial adapter or
  a HID device with a timeout cancels a transfer on every timeout, so it runs
  into the window regularly: an FTDI adapter sends a packet every 16 ms even
  when idle. The comment at `usb/host/mod.rs:564-566` states the discard of
  an uncollected completion as intended. It is right for a reset and a
  configuration change, and wrong for Stop Endpoint and Set TR Dequeue
  Pointer.
- Proof: unit test `a_completed_urb_survives_stop_endpoint` in
  `usb/host/mod.rs` with `FakeUsbFs`: `transfer(0x81, &[], 64)` answers
  `Nak`, `fake.complete_with(Completion::Data(b"abcd".to_vec()))`,
  `fake.run_irq()`, `endpoint_dropped(0x81)`. Then either the drop reports
  the four bytes to its caller or the next `transfer(0x81, ..)` returns
  `Ack(b"abcd")`. Today the bytes never come out (run on helios, also with
  the completion already filed). Second test in `usb/tests.rs`, through MMIO
  with a backend that completes on the second call: Stop Endpoint on a NAKed
  bulk IN endpoint whose backend has the data ready must produce a Transfer
  Event with the data in guest memory and the right residual, before or in
  place of the Stopped event.
- How bad: wrong result, with silent loss or duplication of device data in
  the guest. The host is not affected.

### F16. An endpoint with Max Burst Size 4 or more is refused (wrong result)

- Where: `usb/xhci.rs:811` and `usb/xhci.rs:884`
  (`(ctx[1] >> 10) & 31 != 0` answers TRB Error).
- Why it is a fault: bits 10 to 14 of dword 0 of an endpoint context are
  MaxPStreams. The code tests the same bits of dword 1, which are bits 2 to 6
  of Max Burst Size (dword 1 bits 8 to 15). The comment at `usb/xhci.rs:1197`
  and `usb/README.md:111` say that streams are not supported, so the test was
  meant for dword 0. Two things follow. First, Configure Endpoint fails with
  TRB Error for every endpoint whose Max Burst Size is 4 or more. A Linux
  guest writes `bMaxBurst` of a SuperSpeed endpoint there, and USB 3 flash
  drives and disk enclosures commonly report 15. Such a device lands on a
  SuperSpeed root port (`vmm/vm.rs:437-441`), enumerates, and then cannot be
  configured: the guest's driver gets the error for its Configure Endpoint
  command and the disk never appears. High-speed and full-speed
  devices are not affected, because their Max Burst Size is at most 2.
  Second, an endpoint context with MaxPStreams set is accepted, and its
  dequeue pointer, which then points at a stream context array, is walked as
  a transfer ring. HCCPARAMS1 advertises no streams (`usb/xhci.rs:1198`), so
  only a guest that ignores the capability gets there. No test configures an
  endpoint with a burst: the existing ones write a Max Burst Size of 0.
- Proof: unit test `a_superspeed_burst_endpoint_is_configured` in
  `usb/tests.rs`: after `enumerate` and SET_CONFIGURATION, Configure Endpoint
  with one bulk IN context whose dword 1 is
  `(6 << 3) | (15 << 8) | (1024 << 16)`. The completion code must be 1. Today
  it is 5 (run on helios, for 4 and for 15). Companion test
  `a_streams_endpoint_is_refused`: the same context with burst 0 and dword 0
  `1 << 10` must not complete with 1. Today it does.
- How bad: wrong result. USB 3 storage on a USB 3 port is the common case of
  passthrough, and it does not work in a Linux guest.

### F17. A forced stop waits behind a pending attach (wrong result)

- Where: `src/labd/vm.rs:2831-2845` (`usb_attach` holds the lock on
  `self.usb` across `hotplug.attach_usb(plan).await`), `src/labd/vm.rs:2847-2872`
  (`usb_detach` the same across `hotplug.detach`),
  `src/labd/hypervisor/vmlab_hv/control.rs:207-220` (`send` has no time
  limit), and `src/labd/vm.rs:2011-2012` (`usb_return_before_kill` starts with
  `self.usb.lock().await`, before its 5 s limit begins), called at
  `src/labd/vm.rs:1980-1982` ahead of `proc.kill()`.
- Why it is a fault: the fix of F5 gave the detaches before the kill a limit
  of 5 s together, so that "a hypervisor that does not answer is still
  killed". The limit covers the detach commands and not the lock taken before
  them. A `vmlab usb attach` whose `device_add` has not come back holds that
  lock. `device_add` does not come back while vmlab-hv opens the device, and
  the open waits for the host driver to let go of the interface
  (`usb/host/mod.rs:224-227`), which for a failing USB disk can take very long
  or for ever. It also does not come back from a hypervisor that is hung. In
  both cases `vmlab vm stop --force`, and a graceful stop that ends at the
  Kill rung, wait on the lock and never kill the process. A failing device
  and a hung hypervisor are the two cases a forced stop exists for. The
  existing test `a_forced_stop_kills_a_hypervisor_that_never_answers_the_detach`
  has no attach in flight.
- Proof: test `a_forced_stop_kills_behind_an_attach_that_never_returns` in
  `src/labd/lifecycle_tests.rs`, on the paused tokio clock, with a fake
  `Hotplug` whose `attach_usb` never completes: start `usb_attach` in a task,
  then call the forced stop. The stop must return and the fake must have seen
  the kill. Today the stop future never completes, which the paused clock
  shows without a wall-clock limit (a virtual `timeout` of any length
  elapses).
- How bad: wrong result. A forced stop that hangs. The way out today is to
  kill vmlab-hv by hand, which leaves the device without a host driver.

### F18. A high-bandwidth isochronous TD is cut into packets (wrong result)

- Where: `usb/xhci.rs:1063-1067` (the Max Packet Size field of the endpoint
  context is passed as the packet size), `usb/host/mod.rs:891-897`, and
  `usb/host/device.rs:126-154` (`new_isochronous` makes
  `buffer / packet_size` packet descriptors).
- Why it is a fault: one isochronous TD of xHCI is the data of one service
  interval. For a high-bandwidth high-speed endpoint that is up to three
  packets, 3072 bytes, and for SuperSpeed up to 48 packets, while Max Packet
  Size stays 1024. The backend turns a 3072-byte TD into a URB of three
  descriptors of 1024 bytes. To the host kernel each descriptor is one
  service interval of at most 1024 bytes, so the host asks the device for one
  transaction in an interval in which the device sends three. The device data
  does not fit and the packets end in errors, which `iso_data` skips
  (`usb/host/device.rs:210`). The guest gets empty or cut frames. The existing
  test `p4_isochronous_transfer_reaches_a_host_backend` uses a TD of exactly
  one packet. USB webcams above low resolutions and capture devices use
  high-bandwidth endpoints.
- Proof: unit test `one_isochronous_td_is_one_usbdevfs_packet` in
  `usb/host/mod.rs`, with fake descriptors whose isochronous IN endpoint has
  `wMaxPacketSize` `0x1400` (1024 bytes, three per microframe):
  `transfer_iso(0x83, &[], 3072, 1024)`, then read the submitted URB from the
  fake. It must carry one packet descriptor of length 3072. Today it carries
  three of 1024. The fake needs an accessor for the packet descriptors of a
  submitted URB.
- How bad: wrong result, for one class of devices. Not run, and it rests on
  the usbdevfs behaviour stated above.

### F19. `usb = true` on a `pc` machine fails at start (wrong result)

- Where: `src/config/hypervisor.rs:325-420` (`hardware_refusals` has no
  entry for `usb` on a `pc` machine),
  `src/labd/hypervisor/vmlab_hv/describe.rs:678-679` (a `pc` machine with
  `usb = true` gets `usb: Some(slot)` in its description), and
  `vmm/builder.rs:444-446` ("host USB passthrough requires the q35 xHCI").
- Why it is a fault: what vmlab-hv cannot run is refused from one list,
  `vmlab_refusals`, which validation and the lab daemon both read. A VM on a
  `pc` profile (`windows-xp`, `windows-9x`, `windows-legacy`, `templeos`)
  with `usb = true`, set on the VM or inherited from a profile, is not on the
  list. `vmlab validate` passes, `up` composes the description, and
  vmlab-hv refuses the whole machine at build with a message that names no
  vmlab setting. The other `pc` limits (virtio-net, cirrus on q35,
  `isa-serial`) are on the list with the setting to change.
- Proof: unit test `usb_passthrough_on_the_pc_machine_is_refused_by_name` in
  `src/config/hypervisor.rs`, beside `a_modern_vm_on_q35_is_refused_nothing`:
  `refused("... profile = \"windows-xp\" usb = true")` must hold one entry
  that names `usb = true`. Today it is empty for the three profiles tried
  (run on helios). The builder half is by reading.
- How bad: wrong result. A late and unclear refusal. Nothing is lost.

### F20. Device strings are printed raw (security)

- Where: `src/usb/mod.rs:197-203` (`read_device` trims the ends of
  `manufacturer`, `product` and `serial` and keeps every other byte),
  `src/usb/mod.rs:129-142` (`describe`), `src/cli/usb.rs:91-143`
  (`render_list`), and every message built from `describe`: the refusals of
  `plan_attach` (`src/usb/mod.rs:659-670`, `619-631`, `464-481`), the
  `attached` and `detached` lines of the CLI, the daemon log
  (`src/labd/vm.rs:2031-2036`) and the `product` field of the `usb.*` events
  (`src/labd/usb.rs:87-99`).
- Why it is a fault: the three strings come from the string descriptors of
  the device, so whoever made or reprogrammed the device chooses them. sysfs
  hands them on as UTF-8 with no filter (stated from memory, not checked). An
  ESC byte in `product` reaches the operator's terminal through
  `vmlab usb list`, where it can clear the screen, move the cursor or set the
  window title, and a line break forges a second row, for example one that
  shows another device as free or as attached to a VM. The same text goes
  into the refusal a user reads before they decide on `--force`. A USB device
  is the one input of this area that comes from outside the host, and the
  listing is what a user runs first after plugging one in.
- Proof: unit test `device_strings_reach_no_terminal_raw` in
  `src/usb/tests.rs`: a fake sysfs device whose `product` is
  `"X\u{1b}[2J\nfake 1-1 row"`. `scan` and then `describe` must return no
  control character, and `render_list` over the listing must print one line
  for the device. Today the ESC byte is kept and the table has three lines
  for one device (run on helios).
- How bad: security against a hostile USB device, of low weight: it changes
  what the operator sees, and runs nothing.

### F21. SET_INTERFACE leaves the old URBs in flight (wrong result)

- Where: `usb/host/mod.rs:605-622` (`set_interface` starts the job and drops
  nothing), against `usb/host/mod.rs:590-595` (`set_config` drops every
  transfer first, with the reason in its comment), and `usb/host/mod.rs:411`
  (every status that is not success is filed as `Stall`).
- Why it is a fault: the fix of F9 discards the in-flight URBs before a
  SET_CONFIGURATION, because the kernel cancels them and `reap` would file
  each cancelled URB as a stall on the endpoint of the new configuration.
  SET_INTERFACE has the same effect in the kernel for the endpoints of that
  interface, and here nothing is discarded. The cancelled completion stays
  filed under the endpoint address, and the first TD the guest queues on that
  address in the new alternate setting gets `Stall` and halts the endpoint.
  A Linux guest drops the old endpoints with Configure Endpoint before it
  sends SET_INTERFACE, which goes through `endpoint_dropped`, so it does not
  see this. A guest that sends the request first does.
- Proof: unit test `set_interface_forgets_in_flight_work` in
  `usb/host/mod.rs`, beside `set_config_forgets_in_flight_work`: bulk IN in
  flight on interface 0, SET_INTERFACE(0, alt 1) through the `control`
  helper, let the fake complete the old URB as cancelled, then
  `transfer(0x81, ..)` must answer `Nak` and not `Stall`. A probe on helios
  showed that the old URB survives the request and that its result is given
  to the next transfer. The cancelled status was not produced, because the
  fake does not cancel on `set_interface`.
- How bad: wrong result in a guest that is not Linux. Low.

## Notes

These have no proving test that was thought through to the end, are not
established, or are not faults against the specification.

- N12. Not established: whether a clean exit of vmlab-hv gives the devices
  back. The fix of F5 covers the Kill rung only. On `quit`, on a guest
  power-off and on a signal, nothing detaches: `Vm::wait`
  (`vmm/vm.rs:1651-1681`) flushes disks, resets VFIO devices and shuts the TPM
  down by name, and does nothing for USB. The device returns to its host
  driver only if the `Drop` of `HostUsbDevice` runs
  (`usb/host/mod.rs:929-939`), which needs every strong reference to the VM
  and to the xHCI to be gone before `main` returns. The code uses weak
  references where it was checked, and no test covers it. If one strong
  reference outlives `run`, every normal `vmlab down` with a device attached
  leaves that device without a driver, which is F5 on the common path. Test
  to settle it: in `vmm/control.rs`, build a machine with `usb`, attach a fake
  backend whose `Drop` sets a flag, send `quit`, run `Vm::wait`, drop the
  handle and the control server, and assert the flag.
- N13. `transfers` checks that the slot is enabled and the endpoint is
  running or stopped (`usb/xhci.rs:929`), and takes whatever device is on the
  slot's root port (`usb/xhci.rs:1045-1051`). It does not check that the port
  is enabled or that the device is the one the slot was addressed for. After
  a detach the next poll stalls each queued TD, so the window is small. An
  interrupt endpoint that waits for its service interval is skipped by that
  poll (`usb/xhci.rs:1024-1026`). A second device attached to the same port
  within that interval, before the guest disabled the slot, receives the TD
  that the guest queued for the first device. A unit test with two recording
  backends can show it. No realistic sequence of `vmlab usb` commands was
  found that is fast enough.
- N14. The lab daemon reads `device_deleted` only inside a detach it asked
  for (`src/labd/hypervisor/vmlab_hv/control.rs:101-105`, `421-438`). When
  vmlab-hv drops a lost device by itself (`vmm/vm.rs:577-596`), the tracking
  is corrected by the sysfs sweep alone. A device that vmlab-hv marks lost
  while sysfs still shows the same bus and device number stays listed as
  attached for ever, and a second attach of it is refused as "already
  attached". No way to reach that state without an unplug was found.
- N15. Every URB status that is not success becomes `Stall`
  (`usb/host/mod.rs:411`): babble, a transaction error and a timeout all
  halt the endpoint with a Stall Error, where a controller reports Babble
  Detected or USB Transaction Error. An OUT transfer is acknowledged with the
  whole TD length whatever `actual_length` says (`usb/host/mod.rs:408`,
  `usb/xhci.rs:1088`).
- N16. While a host transfer is in flight, each poll walks the TD again and
  copies its OUT data out of guest memory again before the backend answers
  `Nak` (`usb/xhci.rs:1027-1044`). A 1 MiB OUT TD on a slow device is copied
  once for each millisecond it takes, and once more for each doorbell write.
  F11 bounded the ring reads and left the data buffers outside the limit by
  design.
- N17. A device with one configuration is taken to be in it without a
  question (`usb/host/device.rs:400-406`). When the host has it at
  configuration 0, the open fails at the first claim. If the claims are
  skipped, the guest's first SET_CONFIGURATION sets nothing, because the
  value equals the assumed one (`usb/host/mod.rs:587-588`).
- N18. A `SetConfig` job whose claim of interface k fails gives back the
  interfaces before k and reports none as claimed
  (`usb/host/mod.rs:139-156`). The interfaces from k on were released by the
  same job and have no host driver. The detach reconnects the claimed list
  only (`usb/host/mod.rs:468-479`), so they stay without one when the
  configuration is still the one found at the open.
- N19. The lab daemon has one command connection for each VM, and a command
  holds it until vmlab-hv answers
  (`src/labd/hypervisor/vmlab_hv/control.rs:207-220`). `device_add` answers
  after the open and the claims, and `device_del` after the device is back on
  the host, both of which block in the kernel. During that time no other
  command of that VM is served. F17 is the case where this hangs a stop.
- N20. One host transfer needs one poll to be collected, so an isochronous
  endpoint passes at most one TD for each millisecond. A high-speed endpoint
  with an interval of 125 microseconds gets an eighth of its packets. Isochronous OUT
  answers `Stall` (`usb/host/mod.rs:887-890`) and halts an endpoint type that
  cannot halt. `usb/README.md:134` states the missing OUT support.
- N21. Stop Endpoint on a halted endpoint makes it stopped
  (`usb/xhci.rs:735`), where the specification asks for a Context State
  Error. Address Device is accepted for a slot that is already addressed or
  configured (`usb/xhci.rs:820-863`). Both hurt only the guest that does it.
- N22. HCRST drops the endpoints and does not call `reset` of the devices
  (`usb/xhci.rs:1256-1269`), where the machine reset does
  (`usb/xhci.rs:1662-1664`). The tablet and the keyboard keep their
  configuration and their queued reports across an HCRST.
- N23. A byte write to an odd offset of a UHCI port register is taken as a
  word write whose value is that byte (`usb/uhci.rs:324`). A write of 0x02 to
  offset 0x11, the high byte of PORTSC1, meant as port reset, clears the
  connect-change bit and the port-enable bit. The module comment gives QEMU's
  decoding as the model for byte reads. Whether QEMU decodes byte writes the
  same way was not checked against the oracle file.
