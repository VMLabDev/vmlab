# Review: containers and the micro-VM runtime

Card #579, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Paths are relative to the repo root. Line numbers are those of commit
`872022fd` (`next` at the start of the card).

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. "Confidence" says how sure the reading is.

The card asks first what a container image or a process in the container can
reach of the micro-VM and the host, and how a failed start is cleaned up.
Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic.

One fact frames the security findings. The workload is not confined inside
the micro-VM. cinit starts it with `CLONE_NEWPID | CLONE_NEWNS` and a
`chroot` (`guest/cinit/src/container.rs:257-263`, `:245`), as root unless the
image or the lab names a user, with every capability, in the micro-VM's own
network namespace, with the micro-VM's devtmpfs and a writable sysfs mounted
in its root (`guest/cinit/src/mounts.rs:195-240`). There is no capability
drop, no `no_new_privs`, no seccomp and no `pivot_root` anywhere in
`guest/cinit` or in the agent's container half. PRD §18 makes the micro-VM
the boundary, so this is not a finding by itself (see N1). It means that the
host must treat everything the micro-VM says as said by the workload: the
addresses its interfaces carry, its ctl events and its agent answers.

## What was read and what was not

Read in full, without the test modules:

- `guest/cinit/src/main.rs`, `mounts.rs`, `util.rs`, `container.rs`,
  `users.rs`, `health.rs`, `net.rs`, `cmdline.rs`, `reap.rs`, `sig.rs`,
  `ctl.rs`
- `guest/agent/src/unix/container.rs`
- `src/labd/container_ctl.rs` lines 1-200
- `src/labd/container.rs` lines 100-1265 and 1600-1660 (spec merge, volumes,
  start, readiness, exit monitor, teardown, stop ladder, snapshots, logs,
  addresses)

Read in part:

- `src/qemu/container.rs` (the argv: drives, ports, virtiofs devices, serial)
- `src/labd/hypervisor/vmlab_hv/describe.rs` lines 95-200 (the container
  description)
- `src/qemu/virtiofsd.rs` lines 338-410 (`spawn`), `src/qemu/process.rs`
  around line 150 (`kill_on_drop`)
- `src/labd/vm.rs` lines 1131-1190 (`start_virtiofsds`) and 2733-2742
  (`tail_file`)
- `src/labd/machine_build.rs` lines 295-360 (`build_container`)
- `src/labd/lab.rs` lines 1040-1110 (the SMB start), 1975-2045 (forward
  observations and install), 2995-3010
- `src/labd/forward_plan.rs` lines 300-335, `src/net/nat/mod.rs` lines
  255-275, `src/labd/netservices.rs` lines 265-290
- `src/smb/config.rs` lines 40-160
- `src/oci/image/pull.rs` lines 160-280, `src/oci/image/cache.rs` (the lock
  and install functions only), `src/oci/image/flatten.rs` lines 1-140
- `src/config/extract.rs` lines 850-880 (volume names and targets)
- PRD §18

Not read:

- The layer flatten past its entry point. `docs/reviews/templates-registry.md`
  F6, F9 and F17 cover it, and the flatten never unpacks onto the host: it
  streams a tar into `sqfstar`, and only the guest kernel reads the result.
- `guest/agent` apart from `unix/container.rs`: the file session, terminals,
  exec and logins are in `docs/reviews/guest-agents.md`.
- `src/labd/container.rs` lines 1265-1600 and 1660-2239 (the `Machine`
  trait glue, snapshots online, tests), `src/labd/lifecycle_tests.rs`.
- `guest/build-asset.sh` and the initramfs content.
- The vmlab-hv side of the micro-VM (`hv/`) apart from the serial log open.
- The e2e container features.

## Summary

| # | Severity | Finding |
|---|----------|---------|
| F1 | security (peer) | A port forward and the NAT engine's MAC binding follow the address the guest claims, so a container takes over another machine's forwards (fixed, card #614) |
| F2 | crash (hang) | A ctl write has no timeout, so a micro-VM that stops reading its ctl port hangs the stop of that machine (fixed, card #642) |
| F3 | wrong result | Changing `image =` keeps the old image's writable layer over the new image |
| F4 | wrong result, security | The console log grows without bound and `logs` reads all of it, and fails on one byte that is not UTF-8 (fixed, card #643) |
| F5 | wrong result | A start that fails on its second virtiofsd leaves the first one running |
| F6 | wrong result | An image `StopSignal` of a real-time signal powers the micro-VM off before the workload starts |
| F7 | wrong result | A FIFO at `/.vmlab/busybox` hangs PID 1 at boot |
| F8 | wrong result | A `workdir` the image lacks fails the launch, where Docker creates it |

## Findings

### F1. A forward and the NAT MAC binding follow the address the guest claims (security)

- Where: `src/labd/lab.rs:1975-2010` (`forward_observations`),
  `src/labd/lab.rs:2033-2035` (`learn_mac` before `add_forward`),
  `src/labd/container.rs:1643-1665` (`guest_ips`: the agent's interface
  list, else cinit's `net_up`), `src/labd/vm_agent.rs:3031-3051`
  (`ipv4_by_mac`), `src/net/nat/mod.rs:259-267` (`learn_mac`).
- Fault: the lab daemon plans every forward against `observed.leases`, which
  is filled from `Machine::guest_ips`: what the guest's agent reports its
  interfaces carry, and for a container the `net_up` line cinit sends as a
  fallback. It never asks the segment's DHCP server, which holds the real
  lease (`src/net/dhcp.rs:256` `lease_of`, `:268` `leased_to_another`). The
  install then calls `learn_mac(guest_ip, mac)`. The comment on `learn_mac`
  says "the caller is the lab daemon, which knows the NIC that holds the
  lease", but the address it binds is the guest's claim. A binding is
  `bound`, so it beats what frames show (`see_mac`), and "a later call
  replaces the binding".
- Why it is reachable from a container: the workload is root with
  `CAP_NET_ADMIN` in the micro-VM's own network namespace (see "How the
  review was done"). An entrypoint that runs `ip addr flush dev eth0; ip addr
  add <peer address>/24 dev eth0` before readiness makes the agent report the
  peer's address on the container's MAC. Readiness then installs the
  container's `port {}` forwards against that address and binds the peer's
  address to the container's MAC in the segment's NAT engine. Every
  connection the engine originates toward the peer from then on, which is
  every host forward to the peer's own `forward {}` and `port {}` rules, goes
  to the container. The forwards are installed again after each restart of
  the container, so a restart wins back a binding the peer's own install
  replaced. A VM guest can do the same through its agent.
- Severity: security against a hostile peer. A container intercepts
  connections the host user makes to another machine of the lab.
- Confidence: high for the plan and the binding. The order in which a
  peer's own install and the container's install run decides who holds the
  binding at a given time.
- Proving test: in `src/labd/lab.rs` tests, with two fake machines on one
  NAT segment, `victim` leased 10.0.0.10 by the segment DHCP server and
  `rogue` leased 10.0.0.11, make `rogue`'s `guest_ips` answer `10.0.0.10`.
  Install the forwards of `rogue` and then connect through a forward of
  `victim`. Name it `a_forward_targets_the_dhcp_lease_not_the_guest_claim`.
  It fails today: `rogue`'s rule carries `guest_ip = 10.0.0.10`, and the
  engine's binding for 10.0.0.10 is `rogue`'s MAC. A smaller form in
  `forward_observations`: assert that an address the guest reports and the
  DHCP server leased to another MAC is not recorded.
- Open question for the fix: whether a claimed address equal to the
  segment's service address makes the engine originate toward its own
  redirect rules (the SMB DNAT to the host's loopback). Not followed.
- Fixed in card #614. `forward_observations` asks the segment's DHCP server
  (`DhcpServer::vouched`, `src/net/dhcp.rs`) which address the NIC holds. A
  NIC with a valid lease is recorded with the leased address, whatever its
  guest reports. A NIC with no lease (a static address, or a lease table lost
  with a restarted lab daemon) is recorded with the reported address, unless
  that address is leased or reserved to another NIC or is the segment's
  service address: then nothing is recorded, a warning names the machine, and
  its forwards are skipped with "no lease". The open question is closed that
  way: a claim of the service address is refused. Proof:
  `a_forward_targets_the_dhcp_lease_not_the_guest_claim` in `src/labd/lab.rs`
  and `a_claim_never_outranks_a_lease` in `src/net/dhcp.rs`. Not covered: a
  global segment, whose DHCP server is the supervisor's, and an address
  nobody holds a lease or a reservation for, which is believed as a frame
  from it is (`Gateway::may_claim`).

### F2. A ctl write has no timeout, so a silent micro-VM hangs its stop (crash)

- Where: `src/labd/container_ctl.rs:130-140` (`CtlHandle::send`: the writer
  mutex held across `write_all` and `flush`, no timeout),
  `src/labd/container.rs:883-887` (every `boot` event answered with
  `send_spec`), `src/labd/container.rs:1052-1063` (stop rung 1 awaits
  `ctl.send(Stop)` with no timeout), `:1185` (resync).
- Fault: the host writes to the ctl socket and waits until the bytes are
  taken. When the guest does not read its `vmlab.ctl.0` port, virtio-serial
  flow control stops QEMU reading the chardev socket, and the socket buffer
  fills. A guest that floods `boot` lines makes the ctl watcher send the
  whole spec for each one until the buffer is full. The watcher's write then
  blocks forever while it holds the writer mutex. A later stop takes rung 1,
  waits for the same mutex, and never reaches the `wait_exit` with its
  timeout, rung 2 or the kill. `vmlab vm stop` on that container, and a
  `down` that reaches it, hang. Only `--force` (and the forced pass of
  `stop_for_exit`, which runs only after `down` returns an error) go past it.
- Who can do it: anyone who controls the micro-VM's cinit. The workload as
  root can, because it keeps every capability (it can leave the chroot and
  write cinit's memory through the micro-VM's `/proc`).
- Severity: crash (a hang of the stop path) caused by a hostile guest.
- Confidence: high for the missing timeout and the shared mutex. The flood
  needs control of cinit or of the ctl port, which is a step past the
  workload.
- Proving test: in `src/labd/container_ctl.rs` tests, a unix-socket server
  that never reads and writes `{"event":"boot","proto_version":N}` lines.
  Connect a `CtlHandle`, run the watcher's answer (a `send` of a large
  `Spec`) until it blocks, then call `send(Stop)` in a second task. Name it
  `a_ctl_peer_that_never_reads_does_not_block_a_stop`. Assert that the second
  `send` returns an error. Today it never returns, and the test runner's own
  timeout ends the test.
- Fixed in card #642. `CtlHandle::send` has one deadline (`SEND_TIMEOUT`,
  5 s) for the wait on the writer mutex and for the write and flush. A write
  that runs out of time marks the writer as cut short, because part of its
  line may be on the wire, and every later `send` on that handle fails at
  once. The ctl watcher's blocked answer therefore releases the mutex, stop
  rung 1 gets an error instead of waiting, and the stop goes on to the agent
  shutdown and the kill. Events are still read after a cut-short write.
  Proof: `a_ctl_peer_that_never_reads_does_not_block_a_stop` in
  `src/labd/container_ctl.rs`, on a paused clock. It never returned before
  the fix.

### F3. Changing `image =` keeps the old writable layer over the new image (wrong result)

- Where: `src/labd/machine_build.rs:317-320` (the pin is dropped when the
  `image =` line changes), `src/labd/container.rs:793-796` (the scratch
  qcow2 is created only when absent), `guest/cinit/src/mounts.rs:104-133`
  (the scratch upper layer is used as found).
- Fault: PRD §18 says the writable layer "is only valid against the same
  rootfs", and a snapshot restore under another pin is refused
  (`src/labd/lab.rs:3804-3815`). A plain start is not. After the lab file
  moves `image = "postgres:15"` to `postgres:16`, the next `up` pulls the
  new image and boots it with the old `scratch.qcow2`. Every file the old
  run changed or created in the upper layer shadows the new image's file,
  and every whiteout of a deleted file hides the new image's file at that
  path. A package upgrade the old run made shadows the new image's
  libraries. The scratch also keeps the old snapshots, which then can never
  be restored.
- Severity: wrong result (the container runs a mix of two images).
- Confidence: high. No code removes or refuses `scratch.qcow2` when the pin
  changes. `forget_artefacts` (`src/labd/container.rs:1631-1637`) runs only
  with the local directory gone.
- Proving test: a lifecycle test in `src/labd/lifecycle_tests.rs` with the
  fake hypervisor. Start a container bound to digest A, stop it, write a
  marker into its `scratch.qcow2`, rebuild the machine with a different
  `image =` line and digest B, start it. Name it
  `a_changed_image_does_not_reuse_the_old_scratch_layer`. Assert that the
  start either made a new scratch disk or refused with a message naming the
  change. Today the marker survives.

### F4. The console log grows without bound, and `logs` reads all of it (wrong result, security)

- Where: `src/qemu/container.rs:155-161` (`-serial file:`),
  `src/labd/container.rs:1261-1263` and `src/labd/vm.rs:2733-2742`
  (`tail_file`).
- Fault: the container's stdout and stderr go to the console log with no
  size limit and no rotation. `tail_file` answers `vmlab container logs` and
  the machine's console request by reading the whole file into one `String`
  with `read_to_string` and then splitting it into lines. Two results
  follow. A log that is not UTF-8, which one byte of binary or Latin-1
  output makes it, fails the call with "stream did not contain valid UTF-8",
  so the logs of that container cannot be read through vmlab at all. A
  workload that writes without pause fills the host disk that holds the lab,
  and each `logs` call then holds the whole file in the lab daemon's memory.
- Severity: wrong result (the logs disappear on one bad byte), and security
  against a hostile guest (host disk and lab daemon memory). The same
  `tail_file` serves VM serial logs (`src/labd/vm.rs:2697`).
- Confidence: high.
- Proving test: in `src/labd/vm.rs` tests, write `b"first\n\xffsecond\nthird\n"`
  to a temporary file and call `tail_file(path, 2)`. Name it
  `tail_file_survives_bytes_that_are_not_utf8`. Assert `Ok` with the last two
  lines (the bad byte replaced). Today it returns `Err`. For the size, a
  second test `tail_file_reads_only_the_end` over a sparse file of 8 GiB with
  three lines at its end, asserting the result: it shows the whole-file read
  as an allocation failure or a very long run.
- Fixed in card #643. `tail_file` reads the end of the file through
  `logs::tail_with_len` (at most 4 MiB, bytes that are not UTF-8 replaced),
  and `logs --follow` reads what the file gained by offset
  (`read_log_since`, at most 1 MiB a poll) where it read the whole file every
  half second. vmlab-hv writes the serial log through `CappedLog`
  (`hv/crates/vmlab-hv/src/serial_log.rs`): past 64 MiB the file is cut to
  its newest half. Both proving tests are in `src/labd/vm.rs`, the cap's in
  `serial_log.rs`. Not covered: a container on QEMU (an aarch64 or riscv64
  host), whose `-serial file:` has no size limit. Its reads are bounded, its
  file is not.

### F5. A start that fails on its second virtiofsd leaves the first one running (wrong result)

- Where: `src/labd/container.rs:694-713` (`start_virtiofsds`), the same
  shape in `src/labd/vm.rs:1131-1190`, `src/labd/container.rs:1008-1025`
  (`teardown`), `src/qemu/process.rs:150` (`kill_on_drop(false)`).
- Fault: each started daemon is pushed to a local `procs` vector, and the
  vector is stored in `self.virtiofsd` only after the loop. When the second
  `start_virtiofsd` fails, the `?` returns with the first daemon only in the
  dropped local vector. `start_attempt` then calls `teardown`, which drains
  `self.virtiofsd`, now empty. A dropped `Proc` does not kill its child. The
  first virtiofsd keeps running and exporting its host directory on a socket
  path that the next start unlinks and binds again
  (`src/qemu/virtiofsd.rs:352-355`), so each failed start adds one daemon
  that nothing stops.
- Severity: wrong result (leaked host processes that hold a volume
  directory open), on every host where one volume's virtiofsd fails to start,
  for example a bad host path on the second volume.
- Confidence: high.
- Proving test: in `src/labd/lifecycle_tests.rs`, give the fake hypervisor a
  script field that fails only the n-th `start_virtiofsd` (today
  `virtiofsd_fails` fails every call, `src/labd/hypervisor/fake.rs:335`).
  Start a container with two volumes that fails on the second, and assert
  that the helper process of the first is killed. Name it
  `a_failed_second_virtiofsd_stops_the_first`. The same test for a VM with
  two virtiofs shares.

### F6. A real-time `StopSignal` powers the micro-VM off before the workload starts (wrong result)

- Where: `guest/cinit/src/sig.rs:9-23`, `guest/cinit/src/main.rs:219-222`.
- Fault: `parse_signal` knows only the names and numbers in nix's `Signal`
  enum, which has no real-time signals. Images built for systemd set
  `STOPSIGNAL SIGRTMIN+3`, and Docker accepts that and numeric values such as
  `37`. cinit parses the stop signal before it spawns the workload, and an
  error there is fatal: cinit prints it on the console and powers off. The
  host sees no `exited` and reports the container crashed, with no reason in
  the event.
- Severity: wrong result (an ordinary image never starts).
- Confidence: high.
- Proving test: in `guest/cinit/src/sig.rs` tests, name it
  `parses_realtime_signals`: assert that `parse_signal("SIGRTMIN+3")`,
  `parse_signal("RTMIN+3")` and `parse_signal("37")` return a signal number
  (the type must change from `Signal` to a raw number for this). Today all
  three return `Err`.

### F7. A FIFO at `/.vmlab/busybox` hangs PID 1 at boot (wrong result)

- Where: `guest/cinit/src/mounts.rs:247-277` (`install_shell_fallback`,
  line 255 `fs::copy`).
- Fault: F56 made `write_in_root` open with `O_NONBLOCK` and refuse anything
  that is not a regular file (`guest/cinit/src/util.rs:132-147`), so that a
  link onto a FIFO or a device cannot block PID 1. `install_shell_fallback`
  resolves its target inside the rootfs but then writes it with `fs::copy`,
  which opens the target for writing with no such guard. A FIFO at
  `/.vmlab/busybox`, shipped in the image or left in the scratch layer by an
  earlier run, blocks cinit in `open` forever, before networking, the
  volumes and the workload. A device node there is written with the busybox
  bytes.
- Severity: wrong result. The container never starts and `up` waits for its
  readiness. The image or the earlier run harms only its own container.
- Confidence: high.
- Proving test: move the body of `install_shell_fallback` behind a root
  argument, then in `guest/cinit/src/mounts.rs` tests create a rootfs with
  `/.vmlab/busybox` made by `mkfifo` and call it. Name it
  `shell_fallback_refuses_a_fifo`. Assert that it returns an error. Today it
  blocks, and the test runner's own timeout ends the test.

### F8. A `workdir` the image lacks fails the launch (wrong result)

- Where: `guest/cinit/src/container.rs:154-157` (`ExecIdentity::apply`),
  `:248-250`.
- Fault: the launch `chdir`s to the workdir after the chroot and dies with
  "container launch failed: chdir to workdir" (exit 127) when it does not
  exist. Docker creates a missing working directory for `docker run -w` and
  for a compose `working_dir`. A lab `workdir = "/srv/app"` over an image
  without that directory therefore runs under Docker and not under vmlab.
  The healthcheck uses the same identity and fails the same way.
- Severity: wrong result.
- Confidence: medium (the Docker behaviour is from memory of its runtime,
  not checked against its source here).
- Proving test: an e2e feature in `e2e/features.py`: a container from
  `alpine` with `workdir = "/srv/missing"` and `command = ["pwd"]`. Assert
  that the console log shows `/srv/missing` and the exit code is 0. Today
  the log shows the launch failure and the container is reported crashed.

## Notes

These have no proving test, or are design points rather than faults.

- N1. The workload is root of the micro-VM in all but its PID and mount
  namespaces (see "How the review was done"). `chroot` instead of
  `pivot_root` lets a root workload leave its root, the micro-VM's
  `/dev/vport*`, `/dev/vda` and `/dev/vdb` are in its `/dev`, and it shares
  the network namespace with the agent and udhcpc. PRD §18 makes the
  micro-VM the boundary, so this is a design choice. It is the reason F1 and
  F2 need no further step. Dropping `CAP_NET_ADMIN`, `CAP_SYS_PTRACE` and
  `CAP_SYS_ADMIN` from the bounding set of a workload that did not ask for
  them would be a separate decision for the PRD.
- N2. On the CIFS fallback the container receives the lab's single SMB
  credential (`src/smb/config.rs:55-61`, a documented deviation from
  PRD §7.5). Root in the micro-VM can then mount every share of the lab, the
  VMs' shared folders and other containers' read-write volumes included.
  `read only = yes` on the server keeps a read-only volume read-only.
- N3. A healthcheck that times out is killed by its process group
  (`guest/cinit/src/health.rs:64-72`). A check that starts its own session
  leaves its descendants running, one set per timed-out run.
- N4. `start_attempt` sets the state to `Stopped` before `teardown`
  (`src/labd/container.rs:854-857`), and the exit monitor does it in the other
  order (`:999-1001`). A start that could begin between the two would have
  its new QEMU and virtiofsd handles taken and killed by the old teardown.
  The start gate probably keeps a second start out for the whole of the
  first. Not checked.
- N5. `container_shell` and `container_home`
  (`guest/agent/src/unix/container.rs:162-167`, `:217-232`) test paths by
  prefixing the rootfs, so an image symlink is followed on the micro-VM's
  side. They only decide which shell and home to offer. The exec itself
  happens inside the chroot. Cosmetic.
- N6. In idle mode an exec session only chroots (`setns_pid` is 0), so it
  runs in the micro-VM's PID namespace and its `/proc` shows cinit and the
  agent. Cosmetic.
- N7. The reaper keeps 64 unclaimed exits (`guest/cinit/src/reap.rs:21`). A
  workload that orphans more than 64 children between a healthcheck's
  `fork` and its `subscribe` can push the check's exit out, and the check
  then reads as a timeout. The window is a few instructions long.
- N8. A failed `qemu-img create` of the scratch disk that leaves a partial
  file makes every later start fail, because the start creates the disk only
  when the path is absent (`src/labd/container.rs:793`).
