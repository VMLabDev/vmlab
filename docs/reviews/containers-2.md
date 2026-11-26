# Review: containers and the micro-VM runtime, second pass

Card #1091, 2026-11-26. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

The first pass is `docs/reviews/containers.md` (card #579). Its findings F1
to F8 are fixed. This pass reads the code as it is on `next` now, looks for
faults the first pass missed and for faults its fixes brought in, and does
not repeat a first-pass finding.

Paths are relative to the repo root. Line numbers are those of commit
`ae5f4088` (`next` at the start of the card). Finding codes start at G1 so
they do not collide with the first pass. Notes are M1 to M11, because the
first pass used N1 to N8.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. No proposed test depends on a wall-clock limit: where a test has
to wait, it waits for an event (an exit, a closed channel, a file), and the
assertion is about state, not about time. "Confidence" says how sure the
reading is.

Severity words are those of the first pass: data loss, crash, security
against a hostile guest or peer, wrong result, cosmetic.

The frame of the first pass still holds: the micro-VM is the boundary
(PRD §18), and the workload is root of the micro-VM in all but its PID and
mount namespaces. No finding of this pass crosses that boundary. Nine of the
ten are wrong results an ordinary lab reaches, and one is a leak in the lab
daemon.

## What was read and what was not

Read in full, without the test modules:

- `guest/cinit/src/main.rs`, `ctl.rs`, `container.rs`, `mounts.rs`,
  `util.rs`, `users.rs`, `health.rs`, `net.rs`, `reap.rs`, `sig.rs`,
  `cmdline.rs`
- `guest/cinit-proto/src/lib.rs`
- `src/labd/container_ctl.rs`
- `src/labd/container.rs` lines 140-1840 (volumes, the spec merge, the start,
  the ctl watcher, readiness, the exit monitor, teardown, the stop ladder,
  snapshots, the `Machine` glue)
- `src/qemu/container.rs` (the argv builder)
- `src/guest_asset.rs`
- `src/oci/image/model.rs`

Read in part:

- `src/oci/image/pull.rs` lines 54-335 (the two entry points,
  `pull_with_transport`, `resolve_platform`, `load_cached`)
- `src/labd/machine_build.rs` lines 295-372 (`build_container`)
- `src/labd/hypervisor/vmlab_hv/describe.rs` lines 150-210 (the container
  description: disks, NICs, ports, shares)
- `hv/crates/vmlab-hv/src/devices.rs` lines 183-225 and
  `hv/crates/vmlab-hv-devices/src/virtio/fs/vhost_user.rs` around line 148
  (the tag limit only)
- `src/qemu/virtiofsd.rs` lines 295-320 (`mount_tag`)
- `src/net/dhcp.rs` lines 88-125, 250-300, 380-560 (the lease table, the
  allocation, the reply options), `src/net/dns.rs` lines 186-195,
  `src/labd/network.rs` lines 800-840 (the lease to DNS sync),
  `src/net/router.rs` lines 265-290
- `guest/build-asset.sh` lines 190-222 (the udhcpc hook)
- `src/config/validate.rs` lines 440-575 (the container rules),
  `src/config/extract.rs` lines 745-815 and 850-895
- `src/labd/machine.rs` lines 795-895, `src/scripting/mod.rs` lines 925-945
- the fix commits of the first pass (`aa2f580b`, `c0eb1652`, `1934f22f`,
  `2aa410e5`, `46984f5d`, `6f020163`, `1d193418`, `cb0eb481`), as they sit in
  the files above

Not read:

- `src/oci/image/flatten.rs`, `cache.rs` and `reference.rs`, and
  `pull.rs` past its entry points. `docs/reviews/templates-registry.md` and
  `templates-registry-2.md` read them.
- `guest/agent`, the container half included (`unix/container.rs`,
  `unix/shepherd.rs`). `docs/reviews/guest-agents-2.md` read
  `unix/container.rs` in full.
- `src/labd/container.rs` lines 1-140 (the directory layout) and the test
  module, and `src/labd/lifecycle_tests.rs` apart from the names of the
  tests the first-pass fixes added.
- `src/labd/lab.rs`: the forward install, the destroy verbs and the snapshot
  restore of a container. The F1 fix was not read again.
- `src/labd/hypervisor/vmlab_hv/control.rs` and the rest of `hv/`. Nothing in
  the TCG crates and nothing about a GPU, as the card says.
- `guest/build-asset.sh` apart from the udhcpc hook, and the initramfs
  content.
- The e2e container scenarios.
- The busybox source. G2 and G4 rest on what `udhcpc` does with `-q` and
  with option 121, from memory of busybox, not checked against the pinned
  1.37.0 source.

## Summary

| # | Severity | Finding |
|---|----------|---------|
| G1 | crash (leak) | The ctl watcher task keeps the channel it waits on open, so it never ends: every container run leaks one task, one socket and the container instance in the lab daemon |
| G2 | wrong result | cinit's DHCP client exits once bound and never renews, so a container that runs longer than its lease loses its DNS name |
| G3 | wrong result | A volume's virtiofs tag is `vol-<container>-<n>` with no length cut, so a container whose name has 31 characters or more cannot start with a volume |
| G4 | wrong result | The micro-VM's DHCP hook ignores option 121, so a container gets none of the segment's `routes {}` |
| G5 | wrong result | An `image =` that names a multi-arch index by digest asks the registry at every daemon start, and fails offline with the image in the cache |
| G6 | wrong result | The health verdict of a run survives its stop, so `is_healthy()` answers true for a stopped container |
| G7 | wrong result | No healthcheck runs during the start period, so a container that is healthy at once is ready only when the whole period is over |
| G8 | wrong result | A relative `workdir` is taken: the launch fails, or the workload starts with its working directory outside its root |
| G9 | wrong result | The QEMU argv of a container does not double a comma in a path (the VM argv does) |
| G10 | cosmetic | `has_healthcheck` reads the lab file only, so a container gated by its image's `HEALTHCHECK` reports no healthcheck |

## Findings

### G1. The ctl watcher never ends: one task, one socket and the instance leak per run (crash)

- Where: `src/labd/container.rs:952-984` (`spawn_ctl_watcher`),
  `src/labd/container_ctl.rs:43-55` (`CtlInner` owns the broadcast sender
  and the write half), `:76-90`, `:138-142`.
- Fault: the watcher task ends on `Err(_) => return`, with the comment
  "channel gone: QEMU exited". A tokio broadcast receiver reports `Closed`
  only when every sender is dropped. The only sender is `CtlInner::events`,
  and the watcher holds a `CtlHandle` clone (`let ctl = ctl.clone()`, used
  for `send_spec`), which is an `Arc<CtlInner>`. The task therefore keeps
  alive the sender it waits on. When the micro-VM exits, the reader task
  ends at EOF and drops its `Arc`, and `teardown` clears `self.ctl`, but the
  watcher's clone remains, and `rx.recv()` waits for ever. Each run of each
  container leaves behind: one parked task, the `Arc<ContainerInstance>` it
  captured (so a rebuilt lab keeps the old instance), the callbacks, and the
  ctl socket's file descriptor, because `OwnedWriteHalf` inside `CtlInner`
  holds the stream open.
- Severity: crash, slowly. The lab daemon loses one descriptor per container
  start for its whole life. A wscript that restarts a container in a loop, or
  a long-lived lab that is stopped and started many times, reaches the
  descriptor limit, and after that every `open`, `accept` and `connect` of
  the daemon fails. Below the limit it is a memory leak.
- Confidence: high for the leak (it follows from the ownership). The count of
  starts needed depends on the daemon's descriptor limit.
- Proving test: in `src/labd/container_ctl.rs` tests, name it
  `a_subscriber_that_holds_the_handle_sees_the_channel_close`. Start a
  unix-socket server, connect a `CtlHandle`, take `rx = handle.subscribe()`
  and keep `handle` alive as the watcher does. Close the server side. Wait
  for the reader's end through `wait_exited`, which returns "ctl channel
  closed" (an event, not a time). Then assert that `rx.try_recv()` is
  `Err(TryRecvError::Closed)`. Today it is `Err(TryRecvError::Empty)`: the
  channel never closes while the subscriber holds the handle. A second proof
  in `src/labd/lifecycle_tests.rs` with the fake hypervisor: start a
  container, stop it, and assert that `Arc::strong_count` of the instance is
  back at its value before the start.

### G2. The DHCP lease is never renewed, so the container loses its DNS name (wrong result)

- Where: `guest/cinit/src/net.rs:46-62` (`udhcpc -f -q -n`),
  `src/net/dhcp.rs:117` (`lease_secs: 3600`), `:253-262` (`leases` lists
  only unexpired leases), `src/labd/network.rs:812-836`
  (`spawn_lease_dns_sync`), `src/net/dns.rs:186-195` (`set_lease_records`
  replaces the whole set).
- Fault: cinit runs busybox `udhcpc` with `-q`, which makes it exit as soon
  as it holds a lease. No process of the micro-VM renews it. The segment's
  DHCP server gives a lease of 3600 seconds. Every two seconds the lab
  daemon replaces the zone's lease records with the names of the leases that
  are still valid. One hour after its boot, a container with a dynamic
  address is no longer in `leases()`, and `<container>` and
  `<container>.<lab>` stop resolving for every machine of the lab, while the
  container still runs and still holds the address. Two guards lapse with
  the lease: `leased_to_another` (`src/net/dhcp.rs:290-296`, the gateway's
  source check of networking review F9) no longer protects the address, and
  `alloc` may give it to a machine that starts later (`:442-457` passes over
  an address another MAC was seen to use only "while any other is free").
- Severity: wrong result. A lab whose containers run longer than one hour
  loses name resolution toward them. A VM guest renews, so only containers
  show it.
- Confidence: high for the host side. Medium for `-q` (see "Not read"): if
  busybox kept a renewing child after `-q`, cinit's `status()` would not
  return and the boot would hang, and it does not.
- Proving test: no wall-clock form exists for the hour, so prove the cause.
  In `guest/cinit/src/net.rs`, move the argv into `udhcpc_argv(ifname)` and
  test `the_dhcp_client_stays_to_renew`: assert the argv has no `-q` and that
  `dhcp_up` does not wait for the client's exit (it should wait for the
  hook's `<ifname>.ip` file instead). Add an e2e scenario for the effect: an
  idle container, then `vmlab exec <c> -- /.vmlab/busybox pidof udhcpc` must
  exit 0 (an idle exec session sees the micro-VM's process list, first-pass
  N6). Today it exits 1.
- For the fix: a client that stays also re-runs the hook on `renew`, which
  rewrites the address and the default route. The reaper already parks a
  stale udhcpc exit (`guest/cinit/src/reap.rs:61`).

### G3. A long container name makes the virtiofs tag longer than 36 bytes (wrong result)

- Where: `src/labd/container.rs:151-153` (`volume_share_name`), `:651-654`
  (`spec_for_guest`: the tag is the share name), `:758-772`
  (`start_virtiofsds`: the device gets the share name),
  `src/qemu/container.rs:262`, `src/labd/hypervisor/vmlab_hv/describe.rs:195-197`,
  `hv/crates/vmlab-hv-devices/src/virtio/fs/vhost_user.rs:148-151`,
  `src/qemu/virtiofsd.rs:302-320` (`mount_tag`, used only by
  `src/labd/vm.rs:1274`).
- Fault: a virtio-fs mount tag is at most 36 bytes. A VM's share goes
  through `mount_tag`, which cuts a long name and adds a hash. A container's
  volume does not: its tag is `vol-<container>-<index>`. A container name is
  a DNS label of up to 63 characters (`src/config/validate.rs:455`). With 31
  characters or more and one volume, the tag has 37 bytes. vmlab-hv refuses the device ("share tag ... virtio-fs tags are at
  most 36"), and QEMU refuses the property the same way. The start fails
  after the virtiofsd daemons are up, with a message about a tag the author
  never wrote. There is no fallback to CIFS, because the transport was
  chosen before.
- Severity: wrong result. A valid lab file does not start, and validation
  says nothing.
- Confidence: high.
- Proving test: in `src/labd/container.rs` tests, name it
  `a_long_container_name_keeps_its_volume_tags_mountable`. Build an instance
  whose name has 40 characters and which declares two volumes, set
  `virtiofs_active`, call `spec_for_guest`, and assert that every `tag` is
  at most 36 bytes, that the two tags differ, and that they equal the tags
  `start_virtiofsds` hands the hypervisor (the fake records them). Today the
  tags have 46 bytes.

### G4. The micro-VM's DHCP hook ignores option 121 (wrong result)

- Where: `guest/build-asset.sh:195-218` (the hook: `$ip`, `$subnet`,
  `$router`, `$dns` only), `src/net/dhcp.rs:548-557` (option 121 with the
  default route in it), `src/net/router.rs:270-285` (`dhcp_routes_of`: the
  segment's declared routes first).
- Fault: PRD §9.5 has the segment's DHCP server push "classless static
  routes (option 121) from the segment's `routes {}` declarations", and the
  server does. The hook cinit gives udhcpc applies the address and one
  default route from `$router`, and never reads `$staticroutes`. A container
  on a segment that declares `routes { ... via <a router VM> }` gets none of
  those routes. Its traffic for such a destination goes to the segment
  gateway, which does not own the route, where a VM guest on the same
  segment sends it to the declared next hop. A route to a daemon-routed peer
  segment is not affected: its next hop is the gateway, which the default
  route reaches.
- Severity: wrong result. A container cannot reach a network that the lab
  file routes through a machine.
- Confidence: medium. The hook is certain. That busybox exports the option
  as `$staticroutes` is from memory.
- Proving test: an e2e scenario: segment `a` with
  `routes { "10.99.0.0/24" via <ip of a VM> }`, an idle container on `a`,
  then `vmlab exec <c> -- /.vmlab/busybox ip route`. Assert that the output
  has a line for `10.99.0.0/24 via <that ip>`. Today it has only the
  connected route and the default route. A faster form: a shell test that
  runs the hook with a stub `busybox` on `PATH`, `staticroutes="10.99.0.0/24
  10.0.0.5"` and event `bound`, and asserts a `route add` for that pair.
  The hook calls `/bin/busybox` by absolute path, so the stub needs the path
  to be a variable first.

### G5. An index digest in `image =` needs the registry at every daemon start (wrong result)

- Where: `src/labd/machine_build.rs:316-324` (`build_container`: a reference
  that carries `@` is used as written, "a reference already carrying a
  digest equals its pin"), `src/oci/image/pull.rs:54-60`
  (`cached_container_image`), `:96-109` (the digest selector), `:149-156`
  and `:171-176` (the cache is keyed by the platform manifest's digest).
- Fault: the digest a registry shows for a multi-arch image is the digest of
  its index. `image = "docker.io/library/nginx@sha256:<index>"` is the usual
  way to pin one. The pull resolves the index to the platform manifest and
  installs the image under the platform manifest's digest. Nothing records
  that the index digest led there: `record_ref` runs only for a tag. So
  `cached_container_image` looks the index digest up in the cache, misses,
  and every lab daemon start queues a pull. The pull misses the cache the
  same way and asks the registry for the index. Online that costs one
  request per start. Offline it fails with "resolving ...", although the
  image is installed, the comment at `:99` says "content-addressed: fully
  offline", and the pinned platform digest is in the lab's state
  (`image_digest`) and is not used, because the reference has an `@`.
  A tag has an offline fallback (`:113-126`). A digest, the stricter pin,
  has none.
- Severity: wrong result. `up` of a digest-pinned container fails without
  network.
- Confidence: high.
- Proving test: in `src/oci/image/pull.rs` tests, beside
  `pull_resolves_platform_flattens_and_goes_offline`, name it
  `an_index_digest_resolves_offline_after_its_first_pull`. Seed the fake
  registry with an index and its platform image, pull `repo@<index digest>`,
  take the registry offline, then assert that `cached_container_image` for
  the same reference returns the image and that `pull_with_transport`
  succeeds. Today the first returns `None` and the second fails.

### G6. The health verdict of a run survives its stop (wrong result)

- Where: `src/labd/container.rs:836-837` (`healthy` is cleared only at the
  next start), `:1063-1086` (the exit monitor clears `agent_up`, `started`
  and `ready`, not `healthy`), `:578-580`, `src/labd/machine.rs:822-827`
  (`is_healthy`), `src/scripting/mod.rs:939-945`.
- Fault: `Machine::is_healthy` returns the latest verdict when there is one,
  and readiness otherwise. After a container with a healthcheck stops or
  crashes, `healthy` still holds `Some(true)` from the run that ended. A
  wscript `m.is_healthy()` then answers true for a machine that is not
  running, until its next start. The status label is not affected: it reads
  the verdict only in the `Running` state (`src/status.rs:185`).
  `status_detail` still carries the stale `health` in its JSON.
- Severity: wrong result. A script that waits for `is_healthy()` after a
  crash, or checks it before it uses a service, goes on against a dead
  container.
- Confidence: high.
- Proving test: in `src/labd/lifecycle_tests.rs`, beside
  `a_healthcheck_gates_readiness`, name it
  `a_stopped_container_reports_no_health`. Start a container with a
  healthcheck on the fake hypervisor, send `health: true` on its ctl, wait
  for readiness, stop it, wait for the `Stopped` state, and assert that
  `health()` is `None` and `is_healthy()` is false. Today they are
  `Some(true)` and true.

### G7. No healthcheck runs during the start period (wrong result)

- Where: `guest/cinit/src/health.rs:104` (`thread::sleep` of the whole
  `start_period_secs` before the first check),
  `guest/cinit-proto/src/lib.rs:93-95`, `src/labd/container.rs:1028-1038`
  (readiness waits for the first healthy report).
- Fault: the module says it mirrors the OCI and compose semantics. There, the
  start period is a time in which a failed probe does not count toward
  `retries`, and probes do run: the first pass inside the period makes the
  container healthy at once. cinit sleeps through the period and runs
  nothing. A database image with `HEALTHCHECK --start-period=120s` that
  answers after three seconds is `ready` after two minutes under vmlab.
  Readiness gates `up`, the dependents (`depends_on`) and the provisions, so
  each such container adds its whole start period to every `up`.
- Severity: wrong result (a wait that the image's author meant as an upper
  limit is taken as a fixed delay).
- Confidence: medium. The Docker behaviour is from memory of its
  documentation, not checked against its source here.
- Proving test: move the loop's decisions out of the thread into a value,
  for example `HealthState { fails, state }` with
  `on_result(pass, in_start_period) -> Option<bool>`, and a schedule function
  for the delay before the first check. In `guest/cinit/src/health.rs`
  tests: `a_pass_inside_the_start_period_reports_healthy` (a pass with
  `in_start_period = true` returns `Some(true)`),
  `a_failure_inside_the_start_period_is_not_counted` (`retries` failures
  inside the period return `None`), and
  `the_first_check_does_not_wait_for_the_start_period` (the first delay does
  not depend on `start_period_secs`). The third fails today by
  construction: the delay is the period.

### G8. A relative `workdir` is taken (wrong result)

- Where: `src/config/extract.rs:750` (`workdir` is any string; a volume
  `target` is checked at `:880`), `src/labd/container.rs:243-246` (the
  image's `WorkingDir` as it is), `guest/cinit/src/container.rs:146`,
  `:155`, `:199-201`, `:250-255`, `guest/cinit/src/health.rs:51`.
- Fault: the launch calls `chroot(ROOTFS)` and then `chdir(workdir)` with no
  `chdir("/")` between them. After `chroot` the working directory is still
  the initramfs root, outside the new root. An absolute workdir resolves
  inside the new root. A relative one resolves against the initramfs. With
  `workdir = "app"` cinit first creates `/rootfs/app` (`rootfs_dir` drops
  the difference), then the child's `chdir("app")` looks for `/app` of the
  initramfs, fails, and the container ends with "container launch failed:
  chdir to workdir" and code 127. With a name the initramfs has, such as
  `workdir = "etc"` or `"lib/modules"`, the `chdir` succeeds and the
  workload starts with its working directory in the micro-VM's own tree,
  outside its root. Docker refuses a relative working directory. The
  healthcheck child takes the same path.
- Severity: wrong result. It is not a boundary fault: first-pass N1 already
  says a root workload can leave its root.
- Confidence: high.
- Proving test: in `src/config/validate.rs` tests, name it
  `a_relative_container_workdir_is_refused`: a lab with
  `container "c" { image = "nginx" workdir = "app" }` must give an issue
  that names `workdir`. Today it loads clean. In
  `guest/cinit/src/container.rs` tests, name it
  `an_exec_identity_refuses_a_relative_workdir`: assert that
  `ExecIdentity::new(Some("etc"), None)` is an error, which covers an image
  whose `WorkingDir` is relative. Today it is `Ok`.

### G9. The QEMU argv of a container does not double a comma in a path (wrong result)

- Where: `src/qemu/container.rs:160` (the serial log), `:166` (QMP), `:180`
  and `:188` (the two chardevs), `:210` and `:217` (the two blockdevs),
  `:232` (each netdev), `:257` (each virtiofs chardev);
  `src/qemu/cmdline.rs:113-117` (the VM argv's helper).
- Fault: QEMU splits an option value at each comma and reads `,,` as one.
  The VM argv doubles the commas of every path
  (`a_comma_in_a_path_is_doubled_in_every_option`, `src/qemu/cmdline.rs:862`).
  The container argv writes each path with `display()`. A lab whose
  directory or state directory has a comma in its path gives QEMU a broken
  `-blockdev`, `-chardev`, `-qmp`, `-serial` and `-netdev`, and the
  micro-VM does not start, with QEMU's parse error as the only message.
- Severity: wrong result, on a host where containers run on QEMU: aarch64
  and riscv64 (§22.4). On x86_64 the description goes to vmlab-hv as JSON
  and is not affected.
- Confidence: high.
- Proving test: in `src/qemu/container.rs` tests, name it
  `a_comma_in_a_container_path_is_doubled_in_every_option`, the copy of the
  VM test: build the argv with every path under `/tmp/with,comma/` and
  assert that no value has `with,comma` without the doubled form. Today
  every path option fails it.

### G10. `has_healthcheck` reads the lab file only (cosmetic)

- Where: `src/labd/container.rs:1729-1731` (`self.cfg.healthcheck.is_some()`),
  `:252-261` and `:1024-1027` (the spec's healthcheck, which is the lab's or
  else the image's, gates readiness), `src/labd/machine.rs:889`
  (`Capabilities::healthcheck`).
- Fault: a container without a `healthcheck {}` block whose image has a
  `HEALTHCHECK` runs that check, waits for it before it is ready, and
  reports its verdicts. Its capabilities say `healthcheck: no`
  (`vmlab machine capabilities`, `src/cli/machine.rs:60`), and the comment
  on `is_healthy` names that flag as "how a caller learns whether the answer
  means anything".
- Severity: cosmetic (one flag of a report).
- Confidence: high.
- Proving test: in `src/labd/container.rs` tests, name it
  `an_image_healthcheck_counts_as_a_healthcheck`: an instance from a config
  with no `healthcheck` and an image config with `Test: ["CMD", "/check"]`
  must answer `has_healthcheck() == true`. Today it answers false.

## Notes

These have no proving test without a wall-clock limit, or are design points
rather than faults.

- M1. The exit monitor reads `last_exit` as soon as the emulator process is
  gone (`src/labd/container.rs:1069-1070`). The value gets there through two
  tasks: the ctl reader, then the ctl watcher (`:978`). Nothing makes the
  monitor wait for them. What keeps the order today is cinit's 200 ms sleep
  in `power_off` (`guest/cinit/src/util.rs:33`) after `exited` is written. On
  a loaded host a clean exit with code 0 can be classified `Crashed` with no
  exit code, and `container.crashed` is emitted. A fix would let the monitor
  wait for the reader's EOF before it reads the code. A test for it is a race
  against the scheduler, so it is a note.
- M2. The readiness task of a run ends when it sees a state other than
  `Running`, and it looks every 250 ms (`src/labd/container.rs:999-1037`). A
  run that never got ready, followed by a stop and a start that reaches
  `Running` between two looks, leaves the old task alive in the new run. It
  then fires the old run's `on_ready` when the new run is ready, so
  `container.ready` is emitted twice. The agent poller has the same shape
  with one second. vmlab-hv boots a micro-VM in less than a second.
- M3. `restore_online` asks cinit for a replay (`:1266-1268`) and does not
  clear `healthy` first. A snapshot taken before the first health report
  replays no `health` event, so the host keeps the verdict of the time
  after the snapshot.
- M4. The record of the image a scratch layer was made over is written with
  `std::fs::write` (`:872`, `:876`), not by a rename. A crash between the
  create and the write leaves an empty file, and every later start refuses
  with a message that names an empty digest. The way out is still the one
  the message gives.
- M5. The workload's supplementary groups are its primary group only
  (`guest/cinit/src/container.rs:159`), as the comment at `:136` says. An
  image whose `USER` relies on a second group in `/etc/group` runs without
  it. Docker adds those groups.
- M6. `parse_passwd` skips a row with fewer than seven fields
  (`guest/cinit/src/users.rs:31`). A hand-written `/etc/passwd` row without
  the shell field makes `user = "<name>"` fail with "not found", and the
  micro-VM powers off.
- M7. Each NIC's `bound` event adds a default route through its own router
  (`guest/build-asset.sh:209-213`), and every segment's DHCP server names a
  router. A container with two NICs has two default routes of the same
  metric. Which one the kernel takes was not checked.
- M8. The hook does not read `$domain`, so `/etc/resolv.conf` has no
  `search` line (`guest/cinit/src/net.rs:79-100`). Short names resolve
  because the zone registers them. A name under the segment's domain suffix
  written without it does not.
- M9. In idle mode a `stop` powers the micro-VM off at once
  (`guest/cinit/src/main.rs:183-186`). The processes that exec sessions
  started get no signal first. `sync` runs before the power-off, so the
  scratch layer is consistent.
- M10. A container that declares volumes must have a NIC
  (`src/config/validate.rs:514-523`), because a volume can ride CIFS. On a
  host with virtiofsd the volumes need no network, and the rule still
  refuses the lab.
- M11. A `stop` command that arrives while cinit is between the spec and the
  dispatcher (the mounts and up to 18 seconds of DHCP per NIC without a
  server) waits in the channel (`guest/cinit/src/ctl.rs:181-205`) and is
  acted on only after the workload has started. The host's rung 1 can run
  out of its `grace + 15` seconds in that window and go on to the agent and
  the kill. The result is the same stop, by a harder rung.
