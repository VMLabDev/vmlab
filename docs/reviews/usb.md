# Review: USB passthrough and emulated USB

Card #616, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Line numbers are those of commit `47a8f7c2` (`next` at the start of the
card). `usb/` stands for `hv/crates/vmlab-hv-devices/src/usb/`, and `vmm/` for
`hv/crates/vmlab-hv-vmm/src/`. Other paths are relative to the repository
root.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it.

Two findings (F1, F5) rest on how Linux usbdevfs behaves. That behaviour is
stated from the kernel's `drivers/usb/core/devio.c` as the reviewer knows it.
It was not checked against a kernel tree or a live device in this review:

- `USBDEVFS_DISCONNECT_CLAIM` releases whatever driver is bound to the
  interface and then claims it. usbfs itself is such a driver (`usbfs`), so a
  claim another process made through usbfs is released too, unless the caller
  sets `USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER` with the name `usbfs`.
- Closing a usbdevfs file releases its interface claims. It does not bind a
  kernel driver again. Only `USBDEVFS_CONNECT`, a write to the `drivers_probe`
  file of sysfs, or a replug does that.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic.

### What was read

- `src/usb/mod.rs`, `src/labd/usb.rs`, and the USB parts of `src/labd/vm.rs`
  (`usb_ready`, `usb_attach`, `usb_detach`, `teardown`, `stop_ladder`, the
  snapshot refusal), `src/labd/lab.rs` (the release before a restore),
  `src/labd/mod.rs` (the three wire commands), `src/labd/stop_plan.rs`, and
  `src/supervisor/mod.rs` (`usb_list`).
- The hot-plug half of both adapters: `src/labd/hypervisor/qemu.rs:345-359`
  and `src/labd/hypervisor/vmlab_hv/control.rs:403-440`.
- `usb/host/mod.rs`, `usb/host/device.rs`, `usb/host/fs.rs`,
  `usb/host/descriptor.rs`, all of `usb/xhci.rs`, `usb/ring.rs`,
  `usb/state.rs`, `usb/mod.rs`, `usb/README.md`, `usb/uhci.rs:1-630`, and the
  device half of `usb/tablet.rs` (lines 150-343).
- `vmm/vm.rs:374-530` and `1270-1280` (attach, detach, lost devices, the
  poll), `vmm/control.rs:25-45` and `160-215` (`device_add`, `device_del`),
  and the `DeviceAdd` and `DeviceDel` commands of `vmlab-hv-proto`.
- PRD §5.2.1.

### What was not read

- `usb/keyboard.rs` except its snapshot code, `usb/host/sys.rs` and
  `usb/host/types.rs` except the items named in the findings,
  `usb/host/fake.rs` except its interface, and the snapshot half of
  `usb/uhci.rs` (lines 630-881).
- The existing tests (`usb/tests.rs`, `usb/uhci/tests.rs`, the tests of
  `src/usb`), except to see which cases they cover.
- `src/cli/usb.rs` and the wscript bindings in `src/scripting/mod.rs`. Only
  the names of the script functions were checked.
- `src/qemu/cmdline.rs` beyond the lines that add the xHCI, the keyboard and
  the tablet. QEMU's own `usb-host`, xHCI and HID code was not read. QEMU now
  runs only aarch64 and riscv64 guests, so on QEMU the passthrough path is
  vmlab's `device_add` arguments and nothing more.
- How the vmlab-hv process drops its devices on a clean exit (`quit`, guest
  power-off). F2 assumes only that a `SIGKILL` runs no destructor.
- The udev and ACL state of `/dev/bus/usb` on a real host. vmlab changes no
  permission there. It only tests access (`src/usb/mod.rs:432-435`) and prints
  the two fixes.

## Findings

### F1. An attach takes a device away from another VM that holds it

- Status: fixed by card #662. `claim_interface` now sends
  `USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER` with the name `usbfs`, so the
  kernel answers EBUSY for an interface another usbdevfs user holds, and the
  attach fails with an error that says so. Proof tests:
  `a_claim_leaves_the_claim_of_another_usbfs_user_alone` (`usb/host/fs.rs`),
  `a_claim_another_usbfs_user_holds_is_refused_by_name` (`usb/host/mod.rs`)
  and `open_refuses_a_device_another_usbfs_user_holds` (`usb_loopback.rs`,
  which needs root and a `dummy_hcd` gadget and skips without them). The
  kernel test was not run: the build host has no `dummy_hcd`. The holes in
  vmlab's own "who holds it" list stay as they are. They now end in the
  kernel's refusal and not in a taken device. The second proposed test (two
  labs, no supervisor) was not written: the refusal comes from the kernel,
  and the fake adapter has none.
- Where: `usb/host/fs.rs:137-145` (`claim_interface` sends
  `USBDEVFS_DISCONNECT_CLAIM` with `flags: 0`). The constant
  `USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER` exists at `usb/host/sys.rs:209`
  and nothing uses it. The check that should come first is
  `src/labd/usb.rs:109-135` (`usb_held`) and `src/supervisor/mod.rs:276-310`
  (`usb_list`).
- Why it is a fault: with `flags: 0` the kernel releases any driver on the
  interface, and a claim made by another vmlab-hv process is such a driver.
  So the second process gets the device and the first VM loses it in the
  middle of its transfers. The only guard is vmlab's own "who holds it" list,
  and that list has holes by design. When the supervisor is not running or
  does not answer in 5 s, `usb_held` falls back to this lab's devices alone
  (`src/labd/usb.rs:114-126`). When a lab daemon does not answer the
  supervisor in 3 s, its devices list as free
  (`src/supervisor/mod.rs:299-303`). The comment at
  `src/supervisor/mod.rs:273-275` says such an attach "is refused by QEMU,
  which cannot claim it". That holds for QEMU and does not hold for vmlab-hv,
  which now runs every x86 guest. The same call also takes the device from
  any other host program that uses it through libusb. The first VM is not
  told: its backend does not report the device lost, and `vmlab usb list`
  still names it as the holder. A guest that was writing to a USB disk loses
  the writes in flight.
- How to prove it: as root on a host with `dummy_hcd` and a gadget (the setup
  of `hv/crates/vmlab-hv-tests/tests/usb_loopback.rs`), open the device node
  in the test, claim interface 0 with `USBDEVFS_CLAIMINTERFACE`, then call
  `HostUsbDevice::open` on the same bus and address. The test
  `open_refuses_a_device_another_usbfs_user_holds` expects an error that
  names the claim, and expects the first claim to work afterwards. Today the
  open succeeds. Second test, without a kernel: stop the supervisor, run two
  labs, attach one device to a VM in each. The second attach must be refused.
- How bad: security against a peer (another lab on the host), and data loss
  in the VM that held the device.

### F2. A lab can take a mounted USB disk away from the host

- Where: `src/usb/mod.rs:476-482` (`plan_attach`, the only refusal by device
  kind) and `src/usb/mod.rs:38-45` (`host_input`).
- Why it is a fault: the one protection for the host is the refusal of a HID
  boot keyboard or mouse. A USB disk whose filesystem the host has mounted, a
  USB network adapter that carries the host's connection, and a HID device
  without the boot protocol all attach without `--force`. The claim
  disconnects the host driver at once (`usb/host/mod.rs:194-209`), so the
  host filesystem loses the device under its dirty pages. PRD §5.2.1 asks for
  the HID refusal only, so this is a gap in the specification as much as in
  the code. A wscript can do the same with `usb_attach`, and a script can
  pass the HID refusal too with `usb_attach_force`.
- How to prove it: steps on a host with a USB stick. Mount it, write a large
  file without `sync`, run `vmlab usb attach <vm> <selector>`. Expected: a
  refusal that names the mount point and asks for `--force`. Today the attach
  succeeds and the host logs I/O errors for the mount. Unit test for the fix:
  `plan_attach_refuses_a_mounted_storage_device_without_force` in
  `src/usb/tests.rs`, with a fake sysfs tree that gives the device a block
  child and a mount table that names it.
- How bad: data loss on the host.
- Fixed by card #663, for the mounted disk. `plan_attach` refuses a device
  that carries a filesystem the host has mounted unless `--force` is given,
  and names each block device and mount point. `host_mounts` finds the block
  devices below the USB device in sysfs, with their partitions and the
  mappings in their `holders/`, and looks each up by `major:minor` in
  `/proc/self/mountinfo`. The proof test failed before the refusal was added
  and passes with it. Not covered: a disk in use without a mount (swap, a raw
  device a process has open), a USB network adapter, and a HID device without
  the boot protocol.

### F3. A guest stops its VMM with one UHCI queue head that nests itself

- Status: fixed by card #664. `queue` no longer calls itself for a nested
  queue head. It follows the element link in its own loop, so `MAX_ELEMENTS`
  bounds the nesting together with the TDs of one call. Proof test:
  `a_queue_head_nested_in_itself_stays_bounded` (`usb/uhci/tests.rs`), which
  also covers two queue heads nested in each other. Before the fix it ended
  with a stack overflow (SIGABRT) in a debug build.
- Where: `usb/uhci.rs:396-409` (`queue`), line 406.
- Why it is a fault: a queue head whose element link has the Q bit set makes
  `queue` call itself on the linked queue head. The `MAX_ELEMENTS` limit at
  line 397 counts the turns of one call, not the depth of the calls. A queue
  head whose element link points to itself therefore recurses without an
  end. In a debug build the thread overflows its stack and the process dies
  with a signal. In a release build the compiler may turn the call into a
  jump, and then the thread loops for ever while it holds the controller. The
  thread is the machine-events thread or a vCPU thread (`vmm/vm.rs:931`,
  `1474`). A process that dies this way writes no qcow2 metadata back, which
  `src/labd/vm.rs:1798-1801` names as a cause of an unbootable disk. The
  existing test `cyclic_schedules_and_bad_pointers_stay_bounded`
  (`usb/uhci/tests.rs:207`) covers a horizontal link to itself and a TD that
  links to itself. It does not cover this case.
- How to prove it: unit test `a_queue_head_nested_in_itself_stays_bounded` in
  `usb/uhci/tests.rs`. Write a queue head at address A with the horizontal
  link 1 (terminate) and the element link `A | 2`, point the frame list at
  `A | 2`, enable bus mastering and Run, and call `advance(1_000_000)`. The
  call must return. Today it does not.
- How bad: crash of the VM by its guest, with possible loss of the guest's
  own disk state. Only machines with the PIIX3 UHCI (the legacy `pc`
  profiles) are affected. The host is not affected.

### F4. Blocking usbdevfs calls run under the xHCI lock

- Where: `usb/host/device.rs:389-418` (`get_active_configuration`, a
  synchronous `USBDEVFS_CONTROL` with a 5000 ms timeout), called on guest
  requests from `usb/host/mod.rs:248`, `270` and `321`. Also the synchronous
  `set_configuration`, `set_interface`, `clear_halt` and claim ioctls reached
  from `intercept_control` (`usb/host/mod.rs:350-446`), and the whole
  `HostUsbDevice::open` at `vmm/vm.rs:429-433`, which runs while `attach_usb`
  holds the controller lock.
- Why it is a fault: `usb/README.md:88` states the contract "Backend calls
  must not block". These calls block for as long as the device or the kernel
  takes. A device with more than one configuration that does not answer
  GET_CONFIGURATION holds the caller for 5 s on every
  GET_DESCRIPTOR(Configuration) and every SET_INTERFACE of the guest. The
  caller is a vCPU thread or the machine-events thread, and it holds the
  xHCI lock. During that time no other vCPU can touch the controller, the
  tablet takes no pointer input, the serial ports and the chipset do not
  advance (`vmm/vm.rs:1270-1280`), and `device_del` waits. A hostile guest
  repeats the request and keeps its VM in that state. A hostile or broken
  USB device does the same to any guest. The claim inside `open` can block
  for long too, because disconnecting `usb-storage` waits for the disk to go
  away.
- How to prove it: unit test
  `a_guest_request_makes_no_synchronous_control_transfer` in
  `usb/host/mod.rs`. Give `FakeUsbFs` descriptors with two configurations and
  count calls to `UsbFs::control`. Send GET_DESCRIPTOR(Configuration) and
  SET_INTERFACE through `UsbDevice::control`. Expected count: 0. Today it is
  1 for each request. For the open: a test in `vmm/control.rs` whose opener
  waits on a channel, and a second thread that calls `detach_lost_usb` or
  takes the controller lock. The second thread must finish before the opener
  is released.
- How bad: the VM hangs (the crash class), caused by a hostile guest or a
  hostile USB device. The host is not affected.
- Status: fixed by card #668. A guest request makes no blocking usbdevfs
  call on its own thread. The backend reads the active configuration once
  at the open and follows it through SET_CONFIGURATION, so
  GET_DESCRIPTOR(Configuration) and SET_INTERFACE send no GET_CONFIGURATION.
  SETCONFIGURATION with its release and claim, SETINTERFACE and CLEAR_HALT
  run on a thread of their own (`Job` in `usb/host/mod.rs`) while the request
  is NAKed. During that time the backend makes no usbdevfs call, because the
  kernel serialises the ioctls of one device, so the other transfers of that
  device are NAKed and a discard waits. The device speed is read at the open.
  `attach_usb` opens the host device before it takes the controller lock and
  drops a refused device after it releases the lock. Tests:
  `a_guest_request_makes_no_synchronous_control_transfer`,
  `a_blocked_usbdevfs_call_holds_no_caller` and two more in
  `usb/host/mod.rs`, `opening_a_host_device_does_not_hold_the_controller` in
  `vmm/control.rs`. Not changed: the drop of a backend (detach, host unplug)
  still blocks its caller, the control thread or the machine-events thread,
  with no controller lock held. The statement that the kernel serialises
  usbdevfs ioctls on the device lock is from `devio.c` as the worker knows
  it, not checked against a kernel tree.

### F5. A forced stop leaves the host device without a driver

- Where: `usb/host/mod.rs:581-588` (the `Drop` that issues
  `USBDEVFS_CONNECT`), `src/labd/vm.rs:1813-1820` (the Kill rung),
  `src/labd/stop_plan.rs:94-96` (a forced stop is the Kill rung alone),
  `src/labd/vm.rs:1719` (`teardown` only clears the list).
- Why it is a fault: the device goes back to the host only when the backend
  is dropped. A `SIGKILL` runs no destructor. The kernel then releases the
  claims on close and binds no driver. The lab daemon detaches nothing before
  it kills, so every `vmlab ... stop --force`, every graceful stop that ends
  at the Kill rung, `kill_lab_orphans` (`src/qemu/process.rs:579`), and every
  crash of vmlab-hv (F3 is one) leaves each attached device on the host with
  no driver until it is replugged. A guest reaches the Kill rung by itself
  when it ignores the shutdown request. vmlab reports the device as free
  again, which is true for vmlab and useless for the host. PRD §5.2.1 says "A
  VM stop releases everything it held".
- How to prove it: lifecycle test in `src/labd/lifecycle_tests.rs` with the
  fake adapter: attach a device, call `stop(force = true)`, and assert that
  the fake `Hotplug` saw a `detach` for the device before the process was
  killed. Today it sees none. On a real host: attach a USB stick, run the
  forced stop, and read `/sys/bus/usb/devices/<port>:1.0/driver`. Expected:
  a link to `usb-storage`. Today the link is absent.
- How bad: wrong result. The host loses the use of the device until a
  replug. No data of the host is lost by this alone.

### F6. A guest returns the device to the host without a configuration

- Where: `usb/host/mod.rs:238-261` (`set_config`) and `216-227`
  (`release_and_reconnect`).
- Why it is a fault: SET_CONFIGURATION with value 0 from the guest releases
  every claim, sets configuration 0 on the real device and leaves
  `claimed_interfaces` empty. On detach `release_and_reconnect` walks that
  empty list, so it reconnects nothing, and nothing puts the configuration
  back. A device in configuration 0 has no interfaces, so no host driver
  binds until a replug. The same holds for a guest that chooses another
  configuration than the host had: the host gets the device back in the
  guest's configuration. The backend does not record the configuration it
  found at open.
- How to prove it: unit test
  `detach_restores_the_configuration_found_at_open` in `usb/host/mod.rs`.
  With `FakeUsbFs`: open (configuration 1), send SET_CONFIGURATION(0), drop
  the device. Expected: the fake's `set_configurations` ends with 1 and its
  reconnects name interfaces 0 and 1. Today `set_configurations` is `[0]` and
  there is no reconnect.
- How bad: security against a hostile guest, of low weight: the guest denies
  the host a device after the detach. Otherwise wrong result.

### F7. The filter for standard requests is passed with one changed bit

- Where: `usb/host/mod.rs:350-446` (`intercept_control`), the match at lines
  376-445 and its last arm `_ => None`.
- Why it is a fault: SET_ADDRESS, SET_CONFIGURATION and SET_INTERFACE are
  kept from the device only when the recipient bits and the direction bit of
  `bmRequestType` have the expected value. Every other combination falls to
  `_ => None` and is sent to the real device as a raw control URB
  (`usb/host/mod.rs:472-491`). Examples: `bmRequestType` 0x01 or 0x80 with
  `bRequest` 5 or 9. Many device firmwares decode a standard request by
  `bRequest` alone. Such a device then changes its address or configuration
  and the host kernel does not know. After SET_ADDRESS the device no longer
  answers at the address the kernel uses, so it is dead for the host until a
  replug or a port reset. usbdevfs does not filter these requests.
- How to prove it: unit test
  `standard_requests_with_odd_recipient_or_direction_never_reach_the_device`
  in `usb/host/mod.rs`, beside `intercepted_requests_never_reach_usbdevfs`
  (line 730). For `bRequest` 5, 9 and 11 and each `bmRequestType` in 0x00 to
  0x03 and 0x80 to 0x83, call `control` and assert that
  `fake.submitted_payloads()` stays empty. Today each case outside the
  expected pair submits one URB.
- How bad: security against a hostile guest, of low weight: it needs a device
  that ignores the recipient bits, and the damage is the loss of that device
  for the host until a replug.

### F8. Stopping the control endpoint does not cancel its transfer

- Where: `usb/xhci.rs:683` and `usb/xhci.rs:701-703`; the receiving side is
  `usb/host/mod.rs:565-570`.
- Why it is a fault: the controller turns an endpoint context index into a
  USB address with `(ep / 2) | 0x80 if ep is odd`. For the control endpoint
  (index 1) that gives 0x80. The host backend keeps the control transfer
  under key 0 (`usb/host/mod.rs:464-469`, `489`). So `endpoint_dropped(0x80)`
  finds nothing, the URB is not discarded, and its completion stays filed
  under endpoint 0. The next control TD, which is a different request, gets
  the old answer (`usb/host/mod.rs:464-466`). The Reset Device command skips
  the control endpoint in the same way: its loop starts at index 2. Linux
  guests do exactly this sequence after a control timeout: Stop Endpoint, Set
  TR Dequeue Pointer, next request. The comment at `usb/xhci.rs:676-677` says
  this must not happen.
- How to prove it: unit test
  `stopping_the_control_endpoint_discards_its_transfer` in `usb/tests.rs`,
  through MMIO with a recording `UsbDevice`: start a control IN transfer that
  NAKs, issue Stop Endpoint and Set TR Dequeue Pointer for endpoint index 1,
  and assert that the backend saw `endpoint_dropped(0)`. Today it sees 0x80.
  Second test in `usb/host/mod.rs`: after `endpoint_dropped` for the control
  endpoint as the controller sends it, complete the old URB and assert that a
  new, different request gets `Nak` and not the old data.
- How bad: wrong result in the guest (an answer to another request).

### F9. Backend transfers outlive the slot, the configuration and a reset

- Where: `usb/xhci.rs:638-641` (Disable Slot), `718-730` (deconfigure),
  `803-806` (endpoints dropped by Configure Endpoint), `1152-1158` (port
  reset), `1167-1179` (HCRST), `1559-1570` (machine reset). On the backend
  side `usb/host/mod.rs:238-261` (`set_config`) and `449-454` (`reset`).
- Why it is a fault: `endpoint_dropped` is sent only for Stop Endpoint, Reset
  Endpoint, Set TR Dequeue Pointer and Reset Device. Every other way a guest
  ends an endpoint leaves the URB in flight and its entry in `pending`.
  `reset` of the host backend does nothing, and `set_config` clears neither
  `pending` nor `discarded`. After a guest reboot the first TD on such an
  endpoint waits for the old URB and then gets its result: old data, or a
  stall, because `set_config` released the interface and the kernel
  cancelled the URB, which `reap` files as `Stall` (`usb/host/mod.rs:160`).
  The old data can come from before the reboot.
- How to prove it: unit test `every_endpoint_teardown_reaches_the_backend` in
  `usb/tests.rs` with a recording `UsbDevice`: put a transfer in flight on a
  bulk endpoint, then for each of Disable Slot, Configure Endpoint with the
  drop flag, Configure Endpoint with DC, a port reset and HCRST, assert that
  the backend saw `endpoint_dropped` for that endpoint. Today it sees none.
  Second test in `usb/host/mod.rs`: bulk IN in flight, SET_CONFIGURATION(1),
  `run_irq`, then the next `transfer(0x81, ..)` must be `Nak` and not
  `Stall`.
- How bad: wrong result in the guest.

### F10. A detach that fails drops the tracking while the device stays held

- Where: `src/labd/vm.rs:2571-2591` (`usb_detach`), with the wait at
  `src/labd/hypervisor/vmlab_hv/control.rs:429-438` and the sweep at
  `src/labd/usb.rs:261-268`.
- Why it is a fault: the attachment is taken out of the list before the
  hypervisor is asked. PRD §5.2.1 wants that for a hypervisor that "no longer
  knows the device", and both adapters already turn `DeviceNotFound` into
  success. Every other failure also drops it: a control connection error, a
  missing capability, or no `device_deleted` within 10 s. The hypervisor then
  still holds the device. vmlab lists it as free, `usb detach` says the VM
  holds no such device, a new attach to the same VM fails with "id already
  attached" (`vmm/vm.rs:420-425`), and an attach to another VM runs into F1.
  The unplug sweep also emits `usb.detached` when its `device_del` failed.
- How to prove it: test `a_failed_detach_keeps_the_attachment` in
  `src/labd/lifecycle_tests.rs` with the fake adapter set to fail `detach`
  with an error that is not `DeviceNotFound`. After `usb_detach` returns the
  error, `usb_attached` must still list the device. Today the list is empty.
- How bad: wrong result.

### F11. One poll does unbounded work for the guest

- Where: `usb/xhci.rs:569-595` (`poll`) and `847-893` (`transfers`);
  `usb/uhci.rs:330-409` (`advance`, `walk`, `queue`).
- Why it is a fault: `usb/xhci.rs:567-568` says "No guest-controlled
  traversal can monopolize the VMM thread". The limits are per endpoint and
  per ring, and nothing limits one poll. One call to `transfers` handles up
  to 256 TDs of up to 256 TRBs, with up to 256 Link TRBs for each pop
  (`usb/ring.rs:72`). TDs of Event Data or No Op TRBs without the IOC flag
  complete without a device and without an event, so the event limit at
  `usb/xhci.rs:849` never stops them, and a ring that links back to itself
  without the toggle bit never runs dry. `poll` does this for all 31
  endpoints of every slot, on each 1 ms tick and on each doorbell write,
  under the controller lock. That is millions of guest memory reads for each
  poll. UHCI is the same: 2048 queue heads of 2048 TDs for each frame, and
  128 frames for each `advance`.
- How to prove it: unit test `one_poll_handles_a_bounded_number_of_tds` in
  `usb/tests.rs`. Configure 30 endpoints on the tablet's slot, give each a
  one-TRB ring of a No Op TRB followed by a Link TRB to itself without the
  toggle bit, ring all the doorbells, then count the guest memory reads of
  one `poll()` (a counting `GuestMemory` region, or a counter on `Ring::pop`).
  Expected: at most a fixed number for the whole controller, for example
  256. Today the count is 30 times 256 TDs.
- How bad: a hostile guest holds its own device thread and slows its own VM,
  the pointer input and `device_del` included. No effect on other VMs beyond
  the CPU time.

### F12. `wTotalLength` is trusted when the configuration descriptor is cut out

- Where: `usb/host/mod.rs:312-319` (`config_descriptor_filtered`) and
  `usb/host/descriptor.rs:122-250` (`parse_usbfs_descriptors`).
- Why it is a fault: the parser checks each descriptor's `bLength` against
  the data, and it never checks `wTotalLength` of a configuration against
  the data. `config_descriptor_filtered` then slices
  `raw[offset..offset + wTotalLength]` and panics when that passes the end.
  The value comes from the USB device. On a real kernel the usbfs file is
  built from the same field, so the two normally agree, and that is why this
  is low. `docs/reviews/fuzz-coverage.md` lists the USB setup packets (P16) and no
  row for this parser was found there by a search.
- How to prove it: unit test
  `a_total_length_past_the_data_is_refused_at_parse` in
  `usb/host/descriptor.rs`: take `MASS_STORAGE`, set bytes 20 and 21 to
  0xFF, and expect `parse_usbfs_descriptors` to return an error. Today it
  returns `Ok`, and a GET_DESCRIPTOR(Configuration) through `control` on a
  device built from it panics.
- How bad: crash of the VM, reachable only when the kernel hands out
  descriptors that disagree with themselves.

### F13. Alternate settings of an old configuration are applied to the new one

- Where: `usb/host/mod.rs:238-261` (`set_config`), `279-310`
  (`rebuild_endpoints`), `325-347`.
- Why it is a fault: `alt_settings` is filled by SET_INTERFACE and never
  cleared. SET_CONFIGURATION puts every interface of the real device back to
  alternate setting 0, and `rebuild_endpoints` then reads the endpoint table
  from the stale alternate setting. After a guest reboot a webcam or an audio
  device has an endpoint table that does not match the device. When the new
  configuration has no such alternate setting, `rebuild_endpoints` returns an
  error after the claims succeeded, and the guest's SET_CONFIGURATION stalls.
- How to prove it: unit test `set_configuration_resets_alternate_settings` in
  `usb/host/mod.rs`: SET_INTERFACE(1, alt 1), then SET_CONFIGURATION(1), then
  assert that the endpoint table is that of alternate setting 0 (a transfer
  on an endpoint that only alternate setting 1 has must stall, and one that
  only alternate setting 0 has must be submitted).
- How bad: wrong result in the guest.

## Notes

These have no proving test, or are not faults against the specification.

- N1. `hostport` is not used by vmlab-hv. `vmm/control.rs:187-200` passes
  only the bus and the address to the opener, and `vmm/vm.rs:444-450` stores
  the port without a check. PRD §5.2.1 says the attach is pinned by bus, port
  and address. The address alone is unique until the kernel reuses device
  numbers (after 127 plug-ins on the bus), so a wrong device needs a replug
  race across that wrap. No test without a real bus.
- N2. Two labs can pass the "already attached" check at the same time. The
  lock `UsbHost::op` (`src/labd/usb.rs:43`) is per lab. With F1 fixed the
  kernel refuses the second claim, so this is only a second, less clear error
  message.
- N3. `usb_detach` and `usb_release_for` do not take `UsbHost::op`
  (`src/labd/usb.rs:180-207`, `228-240`), so a detach can run beside an attach
  and beside the sweep. The per-machine list has its own lock, and no wrong
  state was found.
- N4. The lab daemon holds `UsbHost::op` while it waits up to 5 s for the
  supervisor (`src/labd/usb.rs:159-161`). The supervisor asks the labs one
  after another with 3 s each (`src/supervisor/mod.rs:288-303`). Two labs
  that do not answer make the supervisor slower than the caller's 5 s, and
  the caller then drops the whole answer and sees every other lab's devices
  as free. That is one more way into F1.
- N5. `claim_interfaces` claims the numbers 0 to `bNumInterfaces - 1`
  (`usb/host/mod.rs:195`), and the parser refuses a configuration whose
  interface numbers are not exactly those (`usb/host/descriptor.rs:233-240`).
  A device with other interface numbers cannot be attached. The USB
  specification asks for numbers from 0 without gaps, so only broken devices
  are affected.
- N6. After a detach `USBDEVFS_CONNECT` is sent for every interface that was
  claimed (`usb/host/mod.rs:216-227`). An interface that had no host driver
  before the attach, because the user unbound it, gets one afterwards.
- N7. No port reset is sent to the device when it goes back to the host
  (`usb/host/device.rs:444-447`). The host gets the device in the state the
  guest left it: halted endpoints, alternate settings, and whatever the guest
  wrote to it. A device with writable firmware can come back changed. That is
  a property of USB passthrough, and the manual should say it.
- N8. `reap` drops the completions it already collected when
  `poll_transfers` returns an error for a later one
  (`usb/host/mod.rs:135-139`, `usb/host/device.rs:252-254`). Those endpoints
  then stay `InFlight` for ever. The error needs a URB the kernel returns and
  the backend does not know, which should not happen.
- N9. An isochronous TD lets the guest choose the packet size
  (`usb/xhci.rs:979-980`). A 1 MiB TD with packet size 1 makes
  `Transfer::new_isochronous` allocate about 12 MiB of packet descriptors
  (`usb/host/device.rs:134-143`) before the kernel refuses the URB. The
  allocation is freed at once.
- N10. The script functions `usb_attach` and `usb_attach_force` are on every
  machine handle (`src/scripting/mod.rs:1257-1267`). Whether a script
  embedded in a template from a registry can reach them on a clone with
  `usb = true` was not checked. If it can, a template can take host devices,
  the keyboard included.
- N11. `src/usb/mod.rs:1-10` and `src/labd/usb.rs:9-16` still describe QEMU
  as the hypervisor of this path. vmlab-hv reports a lost device itself
  (`vmm/vm.rs:511-530`), and the 2 s sysfs sweep then finds the attachment
  gone on the hypervisor side and relies on `DeviceNotFound`. Cosmetic.
