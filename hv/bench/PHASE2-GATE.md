# Phase-2 performance gate (HV-D28)

**Verdict: D28 remains open. Round 7 met six of ten core bars. Card #71 then
closed G1 (0.9770). Card #72 reran G2 and did not close it (1.0182). Card #73
closed G6 (1.6307). Card #98 merged the ioeventfd wake of the virtio-blk
worker (FD6) and reran both: G2 1.0240, G6 1.5416. Card #74 closed G7
(1.1434). Card #97 moved the virtio-gpu 2D commands off the vCPU threads
(FD5) and reran G2: 0.9361, with a boot time that did not move outside its
spread.**

## Round 7 follow-up: virtio-gpu 2D commands off the vCPU thread (card #97, 2026-11-19)

The 2D commands of virtio-gpu no longer run on a vCPU thread, and a flush
no longer composes a frame. The vCPU threads spend 2.0 seconds less of a
Windows boot in the host. The boot time did not move outside its spread:
the medians of six alternating boots are 91.83 seconds without the change
and 91.52 with it. The compose time did not shrink. It moved to a frame
thread, which says that Windows does not flush in bursts within a frame.

### Change

- The queues of the 2D device are served by a `QueueWorker`, as those of
  virtio-blk are. The transport registers the queue eventfds with KVM on
  the notify region of BAR2, so a kick does not exit to userspace. With a
  renderer (`gpu = "virgl"` or `"venus"`) nothing changes: the queues are
  served on the thread that kicks.
- A flush records its damage and asks a frame thread for a frame. That
  thread composes and publishes at most one frame per 16 ms, which holds
  every flush before it and carries the timestamp of the newest. A lone
  flush is published at once. SET_SCANOUT and RESOURCE_UNREF publish where
  they are served, as before.
- A kick no longer asks the GPU whether its scanout is native. That
  question takes the device lock, which the worker holds while it serves a
  batch. This is the probable cause of what card #72 saw with a worker
  (vCPU CPU time fell, wall time did not): the vCPU that kicked slept on
  the lock until the compose was over. A unit test holds the worker inside
  a command and checks that a kick and a transport read return. The wait
  itself was not measured in a boot.

### Measurements

All runs were on helios in one window with no build, check or other
project VM: QEMU 11.1.3, CPUs 16 to 23, governor performance, 24 GiB
scopes, Windows 10 19045.2006.10, fresh overlay per boot, the bundled OVMF
pair, one-second readiness probes and the sync-latency probe of card #96
(3.6 to 6.7 ms before every boot). The vmlab-hv binary of `0885e0cd` has
SHA-256
`eaf49e949df25f891debed8b3afe2a5d63c3cfacdeee842ebaae6db45d06bb11`, the
base binary of `7352c514` (`next` before the card)
`3d82fc7d9f1d0cedc5f666ab5c534853074c03a074c48b9271f4343a672a8446`, which
is the binary card #98 measured, and the iPXE ROM
`4f0b06896c9457499631ee07d0209aa3416fc82111ea3149cd86a78332a61268`. The
monitor took 362 samples from 04:50:46 to 05:20:51 UTC on 2026-11-19:
one-minute load 1.49 to 3.47, five-minute 2.05 to 2.88, and I/O pressure
(`/proc/pressure/io`, some, 10 s) 43% to 75%, as in cards #73 and #98.
Scripts, logs and raw results are on helios under
`/home/vmlab/src/stc/logs/d28-card-97/`.

- M1, gate rerun with the method of FD7: `bench/run.py --arch x86_64
  --guest windows-10 --repeats 3`, three QEMU repeats and then three
  vmlab-hv repeats.

  | Bar | Workload | QEMU samples | QEMU median | vmlab-hv samples | vmlab-hv median | hv/qemu | Verdict |
  |---|---|---|---:|---|---:|---:|---|
  | G2 | Windows 10 boot (s) | 102.82, 98.12, 94.92 | 98.12 | 102.95, 91.85, 89.38 | 91.85 | 0.9361 | PASS |

  The ratio is under 1.00 for the first time, but three samples per side
  with spreads of 7.9 and 13.6 seconds do not separate it from card #98's
  1.0240: M2 shows no change of the boot time. Both aggregate records are
  appended to [`history/x86_64.jsonl`](history/x86_64.jsonl). The runs
  used a scratch copy of the tree, so the tool recorded no commit, and the
  records carry `0885e0cd` by hand.
- M2, Windows 10 boot, base against the change, six rounds of one boot
  each, alternating, with the order reversed every round (`measure.sh`):

  | Binary | Boot samples (s) | Median |
  |---|---|---:|
  | Base `7352c514` | 92.23, 91.43, 95.41, 91.27, 89.10, 97.19 | 91.83 |
  | Change `0885e0cd` | 91.22, 95.90, 91.49, 102.70, 91.54, 91.24 | 91.52 |

  The medians differ by 0.3 seconds, and the samples spread over 8.1
  seconds for the base and 11.5 for the change. The change was faster in three rounds and slower
  in three. This does not resolve an effect of 1.4 seconds.
- M3, CPU time of the vmlab-hv threads over the boots of M2, from
  `/proc/<pid>/task/*/stat` sampled every second (`cpusample.py`). Host
  user time is user time less guest time.

  | Binary | vCPU threads, host user (s) | vCPU threads, system (s) | GPU queue worker (s) | GPU frame thread (s) | Process, host (s) |
  |---|---|---|---|---|---|
  | Base | 9.80, 9.80, 9.58, 9.93, 9.74, 9.72 | 9.01, 9.14, 9.04, 9.12, 8.95, 8.96 | none | none | 23.57, 23.48, 23.24, 23.58, 23.20, 23.21 |
  | Change | 7.65, 7.41, 7.85, 9.66, 7.83, 7.75 | 9.34, 9.55, 9.18, 9.16, 9.29, 9.11 | 0.18, 0.17, 0.16, 0.16, 0.17, 0.17 | 1.61, 1.59, 1.49, 1.68, 1.54, 1.62 | 23.42, 23.50, 23.24, 25.42, 23.46, 23.29 |

  The vCPU threads' host user time falls from a median of 9.77 seconds to
  7.79, by 2.0 seconds, in five of six boots. Their system time rises by
  0.2 seconds. The fourth boot of the change is its slow one (102.70
  seconds): the vCPU threads used 1.9 seconds more there. The first
  vmlab-hv boot of M1 shows the same pair (102.95 seconds, 9.62), and QEMU
  has such a boot too (102.82), so this is a mode of the guest's boot and
  not of the change. The three boots of M1 measured the same thread times
  as M2 (queue worker 0.16, frame thread 1.54 to 1.58).
- M4, what did not happen. The frame thread uses 1.5 to 1.7 seconds per
  boot, about what the commands cost on the vCPU threads before (1.4
  seconds in card #72, for about 2,400 queue notifications). One compose
  per frame interval saves little here, so most flushes still get a frame
  of their own. The spacing of the flushes was not measured. A compose
  still copies the whole frame twice and compares all of it.
  The process uses the same host CPU time as before (23.2 to 23.6
  seconds).

### Verification

`just ci::check` holds on the card's last commit. Four unit tests cover
the change: the flushes of one frame interval publish one composed frame
with the union of their damage, a flush is answered before its frame is
composed, a kick returns while the worker serves a command, and the
doorbells sit on the notify region of BAR2 and follow it. The merge bar
ignores the tests that need KVM, so these ran alone on helios at
`0885e0cd`: `kvm_tests` (11 passed) and `snapshot_tests` (24 passed) of
`vmlab-hv-vmm`, and `viewer_input_and_resize_reach_guest_via_s1` of
`vmlab-hv-viewer`, which boots Ubuntu on a 2D virtio-vga and checks its
frames, a resize, input and a reset through the display socket.

### Follow-up drafts

These are drafts. No card exists for them.

- FD16: make a 2D compose cost what its damage costs. It copies the
  frame twice (the previous pixels and the cached frame) and compares
  every tile, where the flush rectangles bound what can have changed.
  Worth up to 1.5 seconds of host CPU per Windows boot on the frame
  thread, and no boot time.
- FD17: G2 moves in two modes about 10 seconds apart on both backends
  (M3), which is most of its spread. Finding what the guest does in the
  slow boots would make the bar resolve smaller effects.

### G2 boot milestones (card #121)

Card #97's logs localize much of the spread to the first boot, before
the host observes the specialize marker. The table uses whole-second
wall-clock log timestamps from CPU release, while the total is the
runner's monotonic measurement from launch.

| Card #97 run | Total (s) | Release to marker (s) | Marker to OOBE done (s) | OOBE done to ready (s) |
|---|---:|---:|---:|---:|
| QEMU gate, fastest | 94.92 | 59 | 27 | 9 |
| QEMU gate, slowest | 102.82 | 63 | 29 | 11 |
| vmlab-hv gate, fastest | 89.38 | 56 | 25 | 8 |
| vmlab-hv gate, slowest | 102.95 | 64 | 29 | 10 |
| vmlab-hv A/B head, fastest | 91.22 | 56 | 24 | 11 |
| vmlab-hv A/B head, slowest | 102.70 | 67 | 26 | 10 |

The slow vmlab-hv A/B boot loses about 11 seconds by the marker. The
slow QEMU gate boot loses about four seconds by that point and the rest
after it. These samples do not establish a single binary mode selector.
The guest has no NIC, and the prior disk measurements show variable sync
costs. The logs cannot attribute the pre-marker delay to disk caching,
CPU placement, a guest timer, or offline servicing.

G2 now records monotonic readiness milestones for each repeat alongside
its total, host sync probe, and load. The benchmark retains the existing
guest configuration and readiness sequence so future runs can compare
the spread by phase without changing the parity metric.


## Round 7 follow-up: G7 UDP transmit (card #74, 2026-11-19)

G7 now passes: vmlab-hv sends 35958.04 Mbit/s against QEMU's 31447.98, a
ratio of 1.1434. Round 7 measured 25763.87 against 35457.47 (0.7266).

### Cause

Each kick of a virtio-net transmit queue was a user-space MMIO exit on the
vCPU thread. QEMU takes the same kick in the kernel, through an ioeventfd.

The workload sends one 65000-byte datagram per `sendto`, and the guest
kicks once per datagram on both backends: a 12-second send on vmlab-hv
had 738,000 transmit interrupts for 762,000 datagrams. The sending vCPU
limits the rate. One 12-second send on each backend, with the sender alone
on its guest CPU:

| | QEMU 11.1.3 | vmlab-hv at `0c65636d` |
|---|---:|---:|
| Rate (Mbit/s) | 39711 | 33037 |
| Sender's guest CPU in user and system time | 79% | 95% |
| That CPU time per datagram (microseconds) | 10.4 | 15.0 |
| Device thread CPU, user plus system (s) | 7.8 | 5.3 |

vmlab-hv takes the checksum off the guest (N6) and its device thread uses
less CPU than QEMU's main loop, yet the guest paid 4.6 microseconds more
for each datagram. A user-space instruction sample of the vmlab-hv process
(`bench/ipsample.c`, 3 seconds, 2697 samples, 455 unresolved) puts 64% of
the samples in the kick's exit path on the vCPU thread: the vCPU run loop, the
chipset clock step with its HPET, RTC and interrupt-line updates, the PCI
window and BAR decode, and `queue_notify` with its write to the worker's
wake socket. `process_tx`, the whole transmit copy, has 13%.

### Change

virtio-net on a switch stream offers one event for all its queues, and
its worker polls it. The event is registered as a KVM ioeventfd at each
queue's notify address while the driver has the device activated. A
refused registration leaves the write to the old path. TAP devices are
unchanged. The commit that was measured is
`42425590b7132f6b4e1e8dd8f3b5d78064547df0`.

That commit had its own seam for the registration, kept by the PCI root.
Card #98 merged an equal seam for virtio-blk first (below), and the merge
of `next` into this card left one: virtio-net returns its event from
`VirtioDevice::queue_events`, and the transport registers it through
`QueueDoorbells`, as it does for a `QueueWorker`. The seam of `42425590`
is removed (`VirtioDevice::notify_event`, `PciDevice::notify_events` and
`take_notify_change`, `NotifyEvent`, `NotifyRegistrar` and
`PciRoot::set_notify_registrar`), so this card changes no shared
interface. One difference follows: the PCI root of `42425590` removed a
registration while the function's memory-decode bit was off, and the
merged seam keeps it at the programmed BAR address. The numbers below
are from `42425590` and were not measured again after the merge.

### Diagnostic cohorts

Before the rerun, 24 diagnostic sends of 12 seconds ran on a shared but
lightly loaded host (one-minute load 1.1 to 5.1), three per fresh-overlay
boot, with the backends rotated. "fix" is the change before its commit.
These are not gate records.

The spread of every gate round has one source on both backends: where the
guest scheduler runs the sender. The transmit interrupt arrives on one
guest CPU. A sender on the other CPU has a CPU to itself. A sender on the
interrupt's CPU shares it with the completion softirq. A send is "mixed"
when the sender moved during the 12 seconds.

| Sender runs on | QEMU (Mbit/s) | vmlab-hv at `0c65636d` | vmlab-hv with the fix |
|---|---:|---:|---:|
| The other CPU | 41399 (n=2) | 32930 (n=3) | 44509 (n=1) |
| Both (mixed) | 34085 (n=5) | 26673 (n=3) | 35959 (n=2) |
| The interrupt's CPU | 29849 (n=2) | 24882 (n=3) | 30197 (n=3) |
| Guest CPU per datagram, same order (microseconds) | 13.0, 15.5, 17.3 | 16.0, 19.7, 21.0 | 12.2, 14.8, 17.2 |

Values are medians. The last row counts both guest CPUs with softirq
time. In each placement the round-7 code is at 0.78 to 0.83 times QEMU
and the fix at 1.01 to 1.08.

### Findings

- F1, the kick was the cost. With the ioeventfd the guest spends 3.8 to
  4.9 microseconds less CPU on each datagram, in every placement.
- F2, G7 cannot place a ratio within 30% from three samples. A sample is
  near 30, 35 or 43 Gbit/s by the sender's placement, on both backends,
  so two backends at parity can measure anywhere from 0.70 to 1.43. This
  explains QEMU's samples from 28812 to 43560 in rounds 2 to 7. The rerun below
  has one high sample on each side.
- F3, boot is not slower. Registration happens at DRIVER_OK, not while
  firmware sizes BARs. The diagnostic boots took 6.44 and 6.46 seconds
  with the fix and 6.56 and 6.58 without.

### Quiet-window rerun

The rerun used round 7's method on helios from
`42425590b7132f6b4e1e8dd8f3b5d78064547df0`: `bench/run.py --arch x86_64
--guest ubuntu-26.04 --repeats 3`, all three QEMU repeats and then all
three vmlab-hv repeats, each on a fresh overlay, CPUs 16 to 23, governor
performance, 24 GiB scopes with swap disabled, Ubuntu 26.04.3. QEMU was
11.1.3. The release vmlab-hv binary SHA-256 was
`bc8b50acc1b116271719017c4b582449351948a3180bf7ff491cb448b19bf26c` and the
iPXE ROM SHA-256 was
`1fcb174044dacabd6fc16d92eecee65ca2f92dfd5262f3a963f85da92b6ef101`.

The card ran alone on the board. No project VM ran before or after. No
build or check ran during the window. The monitor took 125 samples at
five-second intervals from 02:29:34 to 02:39:54 UTC. The one-minute load
ranged from 0.29 to 1.91, the five-minute load from 0.40 to 1.26 and the
fifteen-minute load from 0.87 to 1.11. The opticlang pause was not
confirmed. Logs, hashes, the diagnostic scripts and their raw results
remain on helios under `/home/vmlab/src/stc/logs/d28-card-74/`. The rerun
is in `gate2`.

| Bar | Workload | QEMU samples | QEMU median | vmlab-hv samples | vmlab-hv median | hv/qemu | Verdict |
|---|---|---|---:|---|---:|---:|---|
| G7 | UDP TX (Mbit/s) | 42039.68, 29887.45, 31447.98 | 31447.98 | 35810.52, 35958.04, 44971.56 | 35958.04 | 1.1434 | PASS |

The lowest vmlab-hv sample is above the QEMU median.

This was the second attempt. The first, 18 minutes earlier (`gate1`),
stopped in the QEMU cohort before any vmlab-hv sample: in QEMU's second
repeat the host peer (`bench/netpeer`) died during the UDP receive blast,
and the runner failed on its next request to the peer. QEMU's two
transmit samples until then were 37982.92 and 38712.39. The attempt
appended no record. Its cause was not found.

The same runs measure the other single-VM workloads. Their cards own the
verdicts (#71, #73). The values are recorded here because the change is
in the PCI root, which every device's BAR writes pass through:

| Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Card #71's rerun |
|---|---|---:|---:|---:|---:|
| G1 | Ubuntu boot (s) | 6.67 | 6.50 | 0.9745 | 0.9770 |
| G3 | Sequential read (MiB/s) | 20451.32 | 20321.24 | 0.9936 | 1.1350 |
| G4 | Sequential write (MiB/s) | 1431.34 | 4060.75 | 2.8370 | 2.0602 |
| G5 | Random read 4 KiB (IOPS) | 50584.50 | 68432.80 | 1.3528 | 1.3944 |
| G6 | Random write 4 KiB (IOPS) | 18875.40 | 17136.90 | 0.9079 | 0.9845 |
| G8 | UDP RX (Mbit/s) | 85294.26 | 109125.82 | 1.2794 | 1.2651 |

The G3 ratio moved because QEMU's median rose from 18473.58 to 20451.32.
vmlab-hv measured 20967.17 then and 20321.24 now. Idle host CPU was 1.07%
of one core on QEMU and 2.18% on vmlab-hv. All 16 aggregate records and
their samples are appended to [`history/x86_64.jsonl`](history/x86_64.jsonl).
The vmlab-hv records are marked dirty because the QEMU run had appended
to that tracked file. Windows boot (G2) and the guest-to-guest pair were
not run.

### Verification

After the merge, unit tests cover virtio-net on the merged seam. The
worker transmits on a count written to the event, with no `queue_notify`
call, and a TAP device offers no event. The transitional transport
registers the one event at both queues' notify addresses from DRIVER_OK
until reset. The tests of card #98 cover the transport and the KVM
registration.

The three S1 NIC tests on booted guests
(`nic_dhcp_exchange_with_test_switch`,
`nic_traffic_survives_snapshot_load`,
`nic_vnet_multiqueue_traffic_survives_snapshot_load`) need KVM, so they
ran on helios outside the merge bar. They passed at `42425590` on the seam
that is now removed, and again at `eb71a71b` on the merged one. They
assert traffic, not that the kick takes the ioeventfd.

### Follow-up drafts

These are drafts. No card exists for them.

- FD14: G7 needs another method to resolve a ratio. Either pin the sender
  in the guest (`taskset`) away from the interrupt's CPU, or take more
  than three sends and alternate the backends.
- FD15: `bench/netpeer` died once during a QEMU receive blast. The runner
  keeps none of the peer's standard error, so the cause is not recorded.

## Round 7 follow-up: virtio-blk kicks through an ioeventfd (card #98, 2026-11-19)

The queue notification of virtio-blk no longer exits to userspace. The
predicted gain of up to 0.5 seconds of a Windows boot did not appear: the
boot time did not move outside its spread, and the vCPU threads spend one
second more in the kernel per boot. On Ubuntu, sequential reads gained 4%
and random writes about 13%. Random reads and the boot did not change.

### Change

Each queue of a device behind `QueueWorker` has an eventfd, and a kick is a
write to it. The virtio-pci transport registers the eventfds with KVM as
ioeventfds of length zero on the modern notify addresses, from DRIVER_OK
until a reset, and moves them with the BAR. KVM then signals the eventfd
on the guest's write and the vCPU continues in the guest. A kick that still
reaches the transport (a legacy port write, or a refused registration)
writes the same eventfd from the vCPU thread. The worker sleeps in an
edge-triggered epoll set and never reads the counters, so a wake costs it
one system call, as the channel did before. A barrier and a snapshot
quiesce first serve the kicks that were written before them.

### Measurements

All runs were on helios in one window with no build, check or other
project VM: QEMU 11.1.3, CPUs 16 to 23, governor performance, 24 GiB scopes
with swap disabled, Ubuntu 26.04.3 and Windows 10 19045.2006.10, fresh
overlay per boot. The vmlab-hv binary of `2d090d3d` has SHA-256
`3d82fc7d9f1d0cedc5f666ab5c534853074c03a074c48b9271f4343a672a8446`, the
base binary of `fea9ea9a` (`next` before the card)
`8c07ee045f4bcdd5bd1b73fa77d5112b143208fb17ffa8f1654aab22afef74dc`, and the
iPXE ROM `fec3e63efe0ad55e6ef2d00bf93c9d7bc969d3b515f9d2f77a9e6b6b85749d52`.
The host's I/O pressure (`/proc/pressure/io`, some, 10 s) read 44% to 80%
during the window, as in card #73. Scripts, logs and raw results are on
helios under `/home/vmlab/src/stc/logs/d28-card-98/`.

- M1, the kicks come from the kernel. A KVM test writes to a registered
  notify address from a vCPU: the eventfd counts one and the vCPU reports
  no MMIO exit. In the 20,000 random writes of the block sequence the
  vmlab-hv process made 20,028 write system calls with the change and
  20,027 without, so no vCPU thread wrote an eventfd.
- M2, Ubuntu block sequence, base against the change, alternating, four
  rounds, one fresh VM each (`ab2.sh`, medians):

  | Workload | Base `fea9ea9a` | Change `2d090d3d` | Change/base |
  |---|---:|---:|---:|
  | Boot (s) | 6.53 | 6.50 | 0.995 |
  | Sequential read (MiB/s) | 20,454 | 21,334 | 1.043 |
  | Sequential write (MiB/s) | 3,719 | 3,646 | 0.980 |
  | Random read 4 KiB (IOPS) | 68,867 | 68,289 | 0.992 |
  | Random write 4 KiB (IOPS) | 29,870 | 33,896 | 1.135 |

  The random-read samples overlap (68,172 to 69,498 against 65,970 to
  68,990). The random-write samples of the change are 22,638 to 35,377
  against 28,328 to 31,916. A first version of the worker used `poll` and
  a read of the eventfd per wake. It lost 4% on random reads (median
  66,327 in the same four rounds), which is why the worker uses
  edge-triggered epoll.
- M3, gate rerun, round 7's method: `bench/run.py --arch x86_64 --repeats
  3`, three QEMU repeats and then three vmlab-hv repeats, for
  `ubuntu-26.04` and then `windows-10`. The Windows boots use the
  one-second readiness probes and the sync-latency probe of card #96
  (3.6 to 4.0 ms before every boot). The monitor took 366 samples from
  03:23:02 to 03:53:28 UTC on 2026-11-19, which also covers M4's first
  three rounds: one-minute load 0.63 to 2.71, five-minute 1.05 to 2.08,
  fifteen-minute 1.43 to 1.85.

  | Bar | Workload | QEMU samples | QEMU median | vmlab-hv samples | vmlab-hv median | hv/qemu | Verdict |
  |---|---|---|---:|---|---:|---:|---|
  | G2 | Windows 10 boot (s) | 89.51, 89.40, 107.37 | 89.51 | 87.43, 96.06, 91.66 | 91.66 | 1.0240 | FAIL |
  | G6 | Random write 4 KiB (IOPS) | 19435.8, 15998.1, 17386.6 | 17386.60 | 26802.0, 25581.8, 28542.5 | 26802.00 | 1.5416 | PASS |

  G2 is where card #72 left it (1.0182), which the operator accepted as
  parity. The same runs measure the other single-VM workloads:

  | Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Card #73 |
  |---|---|---:|---:|---:|---:|
  | G1 | Ubuntu boot (s) | 6.62 | 6.51 | 0.9834 | 0.9771 |
  | G3 | Sequential read (MiB/s) | 18720.72 | 21882.80 | 1.1689 | 1.0995 |
  | G4 | Sequential write (MiB/s) | 1500.64 | 2713.68 | 1.8083 | 2.6741 |
  | G5 | Random read 4 KiB (IOPS) | 50534.70 | 66951.60 | 1.3249 | 1.3356 |
  | G7 | UDP TX (Mbit/s) | 37083.91 | 29631.40 | 0.7990 | 0.8652 |
  | G8 | UDP RX (Mbit/s) | 85427.29 | 114464.90 | 1.3399 | 1.2550 |

  Idle host CPU was 1.17% of one core on QEMU and 2.30% on vmlab-hv. G7
  is under its bar of 0.80 in this run. The card does not touch the
  network path: QEMU's samples are 36,530 to 41,824 against 31,342 at the
  median in card #73, and vmlab-hv's 24,843 to 30,817 against 27,116. All
  18 aggregate records are appended to
  [`history/x86_64.jsonl`](history/x86_64.jsonl). The vmlab-hv and Windows
  records are marked dirty because the QEMU run had appended to that
  tracked file.
- M4, Windows 10 boot, base against the change, alternating, six rounds of
  one boot each (`gate.sh` part 3 and `pairs2.sh`):

  | Binary | Boot samples (s) | Median |
  |---|---|---:|
  | Base `fea9ea9a` | 99.23, 94.68, 93.62, 95.90, 85.76, 90.89 | 94.15 |
  | Change `2d090d3d` | 101.57, 90.33, 91.82, 90.15, 98.69, 88.73 | 91.08 |

  The medians differ by 3.1 seconds in favour of the change, and the
  samples of each binary spread over 13 seconds. The change was faster in
  four rounds and slower in two. This does not resolve an effect of 0.5
  seconds. The monitor of the last three rounds took 118 samples from
  03:54:23 to 04:04:08 UTC: one-minute load 0.85 to 2.97.
- M5, CPU time of the vmlab-hv threads over the last three rounds of M4,
  from `/proc/<pid>/task/*/stat` sampled every second until the process
  ends (`cpusample.py`). Host user time is user time less guest time.

  | Binary | vCPU threads, host user (s) | vCPU threads, system (s) | Disk worker, user + system (s) |
  |---|---|---|---|
  | Base | 11.27, 10.02, 10.43 | 7.61, 7.54, 7.76 | 2.15, 2.10, 2.13 |
  | Change | 10.38, 11.61, 10.16 | 8.64, 8.62, 8.56 | 2.11, 2.04, 2.17 |

  The vCPU threads' user time, where the 0.55 seconds of MMIO exits of
  card #72's F4 would show, does not separate (the samples spread over 1.6
  seconds). Their system time is 0.8 to 1.1 seconds higher with the change
  in every round. The disk worker is unchanged. The cause of the added
  system time is not established. One candidate: the guest now reaches
  its idle loop before a short request completes, where the exit to
  userspace used to outlast the request, so each such request adds a halt
  exit and KVM's halt polling to the vCPU thread.

### Verification

`just ci::check` holds on the card's last commit. The merge bar ignores
the tests that need KVM, so these ran alone on helios at `2d090d3d`:
the two KVM doorbell tests, `kvm_tests` and `snapshot_tests` of
`vmlab-hv-vmm` (37 passed), and `vm_restore` of `vmlab-hv-tests` (2
passed), which boots Ubuntu
and restores a snapshot ten times during fsync writes to a virtio-blk
disk.

### Follow-up drafts

These are drafts. No card exists for them.

- FD12: find where the added second of vCPU system time of a Windows boot
  goes (M5), with KVM's halt-poll and exit statistics for both binaries.
  If it is halt polling, the kick could stay synchronous for a queue
  whose requests complete from the page cache.
- FD13: G7 measured 0.7990 here and 0.8652 in card #73 with the same
  network code. Three repeats do not hold that bar to 0.80.

## Round 7 follow-up: G6 4 KiB random writes (card #73, 2026-11-19)

G6 now passes: vmlab-hv writes 29,592.9 IOPS against QEMU's 18,147.4, a
ratio of 1.6307. Round 7 measured 13,200.5 against 17,723.0.

### Cause

The gate runs the random write after it removes the 512 MiB file of the
sequential write. The Ubuntu template mounts `/` with `discard` (checked
in the guest), so the removal reaches the disk as discards of up to 8,192
clusters. QEMU ignores discards with
this argv. vmlab-hv frees the clusters. A freed cluster must not be handed
out before the flush that makes the removal of its mapping durable, and
since `30d3ba7a` (2026-11-10, between rounds 6 and 7) such *retired*
clusters stayed in the qcow2 free map, where every allocation stepped over
all of them, lowest first, with one hash lookup each. The random write
allocates about 11,600 new clusters and sends no flush until its end.

- F1, the walk. With the gate's order, 36% of the user-space instruction
  samples of the vmlab-hv process (`bench/ipsample.c`, all threads) were in
  that hash lookup. The disk worker used 0.57 to 0.59 seconds of user CPU
  for the 20,000 writes. With the fix it uses 0.02 to 0.03.
- F2, the order decides. On a fresh VM that runs only the random write,
  vmlab-hv at `34e39e6c` wrote 31,131 IOPS. After the gate's earlier block
  workloads the same binary wrote 13,643 to 16,568 in six runs. QEMU wrote
  15,580 alone and 8,581 to 18,106 after the earlier workloads.
- F3, not the cause: the queue notification (FD6, card #98). The wake of
  the disk worker from the vCPU thread is 6% to 8% of the same samples with
  and without the walk. Card #98 stays open. The round-7 binary and the
  binary of `next` measured the same (5,082 to 15,513 against 15,332 to
  16,568, three runs each on a loaded host), so the changes of card #71
  did not move G6 either. Its rerun of 0.9845 and round 7's 0.7448 are one
  state: the walk put vmlab-hv at about QEMU's rate, and host noise decided
  the side of 0.80.

### Fix

Retired clusters stay out of the free map. The scan that fills the map
skips them, and the flush clears the list and resets the scan as before,
which then finds them as ordinary holes. The order of allocation is
unchanged. The scan also reads the cached refcount block in place: it ran
on every allocation and copied the 64 KiB block each time. A unit test
discards 32 clusters and checks that none is reused or in the map before
the flush, and that the lowest is reused after it.

Diagnostic runs of the gate's block sequence on the shared host, one VM at
a time, QEMU, fix, base in turn:

| Build | Random write 4 KiB (IOPS) | Disk worker user CPU (s) |
|---|---|---|
| QEMU 11.1.3 | 16,601.5, 10,913.8 | n/a |
| vmlab-hv at `34e39e6c` | 16,447.7, 13,642.9 | 0.58, 0.59 |
| vmlab-hv with the fix | 30,226.0, 28,523.0 | 0.03, 0.02 |

### Quiet-window rerun

The rerun used round 7's method on helios from
`2850c4543db2d7407f972ca5b0abc7af464c8e2e`: `bench/run.py --arch x86_64
--guest ubuntu-26.04 --repeats 3`, all three QEMU repeats and then all
three vmlab-hv repeats, each on a fresh overlay, CPUs 16 to 23, governor
performance, 24 GiB scopes with swap disabled, Ubuntu 26.04.3. QEMU was
11.1.3. The release vmlab-hv binary SHA-256 was
`3f91d729c8c029972686ce55b5f45cb3b215b0a275036e17a23aa89d3f6cb72a` and the
iPXE ROM was
`9bfcf6535d787e2083d65ee201d414669c7493e7dbdf52953a9513b6481af8a0`.

The card ran alone on the board. No project VM ran before or after. No
build or check ran during the window. The monitor took 125 samples at
five-second intervals from 01:04:17 to 01:14:38 UTC on 2026-11-19. The
one-minute load ranged from 0.67 to 2.19, the five-minute load from 1.14
to 1.49 and the fifteen-minute load from 1.43 to 1.68. The opticlang pause
was not confirmed. The host's I/O pressure (`/proc/pressure/io`, some)
read 67% to 83% with no VM running, before the window. Logs, hashes, the
diagnostic scripts and their raw results remain on helios under
`/home/vmlab/src/stc/logs/d28-card-73/`.

| Bar | Workload | QEMU samples | QEMU median | vmlab-hv samples | vmlab-hv median | hv/qemu | Verdict |
|---|---|---|---:|---|---:|---:|---|
| G6 | Random write 4 KiB (IOPS) | 11914.1, 18147.4, 18315.6 | 18147.40 | 32167.5, 10732.9, 29592.9 | 29592.90 | 1.6307 | PASS |

One repeat of each backend is slow. QEMU's first repeat also wrote
sequentially at 176 MiB/s against 1,500 and 1,636 in the other two. The
second vmlab-hv repeat has no other slow workload, and its cause is not
established. The verdict uses the medians, as every round does.

The same runs measure the other single-VM workloads. Their cards own the
verdicts:

| Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Round 7 |
|---|---|---:|---:|---:|---:|
| G1 | Ubuntu boot (s) | 6.55 | 6.40 | 0.9771 | 1.0500 |
| G3 | Sequential read (MiB/s) | 18670.33 | 20527.83 | 1.0995 | 1.1071 |
| G4 | Sequential write (MiB/s) | 1500.12 | 4011.44 | 2.6741 | 2.6338 |
| G5 | Random read 4 KiB (IOPS) | 51404.30 | 68653.40 | 1.3356 | 1.3979 |
| G7 | UDP TX (Mbit/s) | 31342.44 | 27116.01 | 0.8652 | 0.7266 |
| G8 | UDP RX (Mbit/s) | 85529.15 | 107336.73 | 1.2550 | 1.2490 |

Idle host CPU was 1.08% of one core on QEMU and 2.05% on vmlab-hv. All 16
aggregate records and their samples are appended to
[`history/x86_64.jsonl`](history/x86_64.jsonl). The vmlab-hv records are
marked dirty because the QEMU run had appended to that tracked file.
Windows boot (G2) and the guest-to-guest pair were not run.

### Follow-up drafts

These are drafts. No card exists for them.

- FD9: a write that allocates a cluster writes the whole 64 KiB cluster.
  The 20,000 random writes carry 82 MB of guest data. vmlab-hv wrote
  758 MB to the host file for them (`write_bytes` of `/proc/<pid>/io`,
  11,571 clusters), QEMU 90 to 102 MB: QEMU zeroes a new cluster through
  the host file system and writes only the guest's bytes. The cost did not
  show in G6 after the fix. It is about nine times the dirty page cache
  for a guest that writes small blocks to new space.
- FD10: vmlab-hv acts on guest discards where QEMU, with vmlab's argv,
  ignores them. A discarded cluster's mapping is cleared, so a later read
  of it returns the backing file's content, not zeros and not the last
  written data. VIRTIO leaves the content after a discard undefined. Decide
  whether the cluster should read as zeros (the qcow2 zero flag), as QEMU's
  `discard=unmap` does for an image with a backing file.
- FD11: QEMU's random write fell from 30,948 IOPS in round 5 to 17,723 in
  round 7 on the same template, and its sequential write from 4,769 MiB/s
  in round 6 to 1,480. QEMU went from 11.1.2 to 11.1.3 in between. The
  cause is not established. The I/O pressure reading above is a place to
  start.

## Round 7 follow-up: G2 Windows 10 boot (card #72, 2026-11-19)

G2 still fails: vmlab-hv boots Windows 10 in 96.69 seconds against QEMU's
94.96, a ratio of 1.0182. Round 7 measured 103.11 against 89.93. No runtime
change is merged. The round-7 ratio of 1.1466 was not reproduced with any
binary, including round 7's own.

### Rerun

The rerun used round 7's method on helios from
`e10780873d648d8a34fb437af22d9cb3262744c7`: `bench/run.py --arch x86_64
--guest windows-10 --repeats 3`, all three QEMU repeats and then all three
vmlab-hv repeats, each on a fresh overlay, CPUs 16 to 23, governor
performance, 24 GiB scopes with swap disabled, Windows 10 19045.2006.10.
QEMU was 11.1.3. The firmware was the plain OVMF pair of round 7 (same
SHA-256), named with `VMLAB_BENCH_OVMF_DIR`, which this card adds because
the default path is absent on helios. The release vmlab-hv binary SHA-256
was `8a497fba60ab23d3c44c0ed7b39fe5eb154eeea5c9c80204b57fd7ab0fe12824`.
That revision used fdatasync where the qcow2 flush uses fsync. The change
measured as no change (F3 below) and was reverted after the rerun.

The card ran alone on the board. No project VM ran before or after. No
build or check ran during the window. The monitor took 119 samples at
five-second intervals from 22:17:45 to 22:27:35 UTC on 2026-11-18. The
one-minute load ranged from 1.29 to 3.79, the five-minute load from 1.72
to 2.45 and the fifteen-minute load from 1.71 to 2.07. The booting guest
itself accounts for one to three of that. The opticlang pause was not
confirmed. Logs, hashes, the diagnostic scripts and their raw results
remain on helios under `/home/vmlab/src/stc/logs/d28-card-72.r8b/`.

| Bar | Workload | QEMU samples | QEMU median | vmlab-hv samples | vmlab-hv median | hv/qemu | Verdict |
|---|---|---|---:|---|---:|---:|---|
| G2 | Windows 10 boot (s) | 94.96, 97.76, 88.33 | 94.96 | 96.69, 98.61, 95.92 | 96.69 | 1.0182 | FAIL |

Both aggregate records are appended to
[`history/x86_64.jsonl`](history/x86_64.jsonl).

### Diagnostic cohorts

Before the rerun, 71 diagnostic boots ran the same readiness sequence
(`run.wait_x86_ready`) one at a time, with the backend order rotated each
round. They also recorded the guest's own event log, per-thread CPU time
and, in some boots, KVM statistics. These are not gate records.

| Cohort | Backend | Boots to agent ready (s) | Median | Launch to end of specialize, median (s) | vCPU CPU there, median (s) |
|---|---|---|---:|---:|---:|
| 1 | QEMU 11.1.3 | 83.32, 89.30, 89.73, 90.65, 94.21 | 89.73 | 53.6 | 46.8 |
| 1 | vmlab-hv at `9572cdba` | 89.43, 91.38, 92.69, 94.38, 97.11 | 92.69 | 54.7 | 48.8 |
| 1 | vmlab-hv at round 7, `82bec6b5` | 89.23, 90.23, 91.23, 91.73, 93.34 | 91.23 | 54.4 | 50.5 |
| 2 | QEMU 11.1.3 | 83.23, 87.43, 88.46, 96.85 | 87.94 | 52.8 | 47.1 |
| 2 | vmlab-hv at `9572cdba` | 88.32, 96.88, 97.56, 97.88 | 97.22 | 55.0 | 49.1 |
| 2 | vmlab-hv with fdatasync | 87.97, 88.52, 89.49, 89.50 | 89.00 | 55.1 | 49.5 |
| 3 | QEMU 11.1.3 | 84.26, 88.59, 89.26, 96.93 | 88.93 | 52.6 | 47.0 |
| 3 | vmlab-hv with fdatasync | 91.45, 93.99, 97.19, 97.57 | 95.59 | 55.2 | 48.9 |

The round-7 binary measured 1.017 times QEMU in cohort 1, and the binary
of `next` 1.033. "End of specialize" is the guest's event 1074 from
winlogon, on the guest clock, which starts from the host's.

### Findings

- F1, the result moves in steps of about five seconds. The readiness
  sequence probes every 5 seconds while the agent answers and every 8
  while it does not, in three stages (marker, OOBE, reboot). The samples
  of all backends fall near 84, 89, 94, 98 and 103 seconds. A backend that
  is two seconds behind lands on the same step or on the next one, so
  three samples cannot place a ratio within 5%.
- F2, a quarter of the boot is host sync time, and it is not steady. One
  boot sends 8,500 to 9,700 flushes. QEMU answered them with 5,250 to
  5,970 fdatasync calls that took 23.8 to 28.7 seconds. vmlab-hv ran 5,950
  to 6,170 syncs that reached the disk, 22.9 to 33.8 seconds. The time of
  1,500 in-place 4 KiB sync writes in a Linux guest ranged from 5.9 to
  14.5 seconds between runs of one binary minutes apart.
- F3, the sync path is at parity. Those 1,500 writes cost 1,500 host
  syncs on both backends, and 1,500 appending writes about 3,600 on both:
  QEMU orders refcounts and data before mappings with a second sync, as
  the qcow2 flush here does. The times overlap: QEMU 5.9 to 9.0 seconds,
  vmlab-hv 5.9 to 14.5 with fsync and 6.1 to 12.0 with fdatasync. Three
  candidate changes were built and measured, and none is merged.
  Answering the flushes of one batch with one sync did nothing: Windows
  queued no request behind any of 8,714 flushes. Dropping the third sync
  of each flush removed 8,700 calls that cost 0.016 seconds together, and
  fdatasync in place of fsync changed no time. Ignoring guest discards,
  as QEMU does with this argv, removed about 160 of 1,280 second syncs
  and showed no boot change.
- F4, vmlab-hv reaches the end of the specialize pass 1.1 to 2.6 seconds
  after QEMU, by the cohort medians, and its vCPU threads have used 1.9
  to 2.4 seconds more CPU by then. This difference repeats in all three
  cohorts. Its cause is not established. KVM exit counts are close to
  QEMU's, 2.31 million against 2.28 million in one boot each, except that
  vmlab-hv has 200,000 more MMIO exits. A vCPU thread spends 3.0 seconds
  of one boot outside `KVM_RUN` after an exit. 1.4 seconds are about
  2,400 virtio-gpu queue notifications of 0.58 ms each: the 2D transfer
  and flush commands run on the vCPU thread, and each flush composes and
  compares the whole frame. 0.55 seconds are 150,000 MMIO writes of 2 to
  10 microseconds, 120,000 of them virtio-blk queue notifications. QEMU
  serves both off the vCPU thread, through an ioeventfd and its main
  loop. Serving the GPU queue on a worker thread was tried and is not
  merged: the vCPU thread's CPU time in MMIO writes fell from 2.0 to 0.65
  seconds, but its wall time stayed at 1.3 seconds, for a reason that was
  not found, and the four boots (83.64 to 95.09) do not separate from the
  others.
- F5, checked and equal on both backends: the guest's CPU topology and
  model, the PnP device list, and the parallel port registers, which the
  guest polls 410,000 times a boot. Checked and cheap in vmlab-hv: the
  flushes that follow no write (3,894 of 8,722, 0.015 seconds together)
  and the configuration writes through ECAM (2,254, 0.034 seconds
  together behind the all-vCPU barrier).

### Follow-ups

FD5, FD6 and FD7 are cards now. FD8 is a decision that waits for the
operator.

- FD5 (card #97): serve virtio-gpu 2D commands off the vCPU thread, and compose a
  frame once per published frame, not once per flush command. Up to 1.4 seconds of a
  Windows boot.
- FD6 (card #98): wake the virtio-blk worker from an ioeventfd, not from a userspace
  MMIO exit. Up to 0.5 seconds of a Windows boot, and every block
  workload.
- FD7 (card #96): G2 cannot resolve 2% as it is measured. Either probe readiness
  every second in the benchmark, or alternate the backends and take more
  than three repeats, and record a host sync-latency probe beside each
  boot.
- FD8: the owner accepted G1 at 1.003 in round 6. G2 at 1.02, with a
  spread of 9 seconds on the QEMU side, is a candidate for the same
  decision. No card: the lead asked the operator, and the answer is open.

## Round 7 follow-up: G1 Ubuntu boot (card #71, 2026-11-18)

G1 now passes: vmlab-hv boots Ubuntu in 6.36 seconds against QEMU's 6.51,
a ratio of 0.9770. Round 7 measured 6.93 against 6.60.

### Where the time went

Guest serial output, per-thread CPU time, `/proc/<pid>/io` and KVM's own
vCPU statistics (`KVM_GET_STATS_FD` for vmlab-hv, QMP `query-stats` for
QEMU) split the round-7 boot into phases. The Linux kernel and userspace
were not slower: the agent started at 4.85 seconds of guest time under
vmlab-hv and 5.03 under QEMU. The whole gap sat before the first kernel
line on the serial port: 1.86 to 2.00 seconds under vmlab-hv, 1.46 to 1.56
under QEMU. SeaBIOS took the same time on both. GRUB's 88 MiB of disk reads
took 1.07 seconds against 0.80, and the kernel's decompression 0.27 against
0.16.

GRUB reads through SeaBIOS in about 5,500 calls of 16 KiB. Each call enters
SeaBIOS's 32-bit code through four SMIs and through legacy mode switches.
Three causes were found there. No single one closes the gap.

- F1, RTC index port. A read of port 0x70 returned the CMOS index. QEMU
  returns 0xff, checked in a guest on both backends. SeaBIOS reads the port
  before each legacy mode switch, and a clear bit 7 makes it mask NMI and
  restore it. That was 20 port exits for each disk call against QEMU's 12.
  The index ports (0x70, 0x72, 0x74, 0x76) now read 0xff.
- F2, 4 KiB nested pages for low RAM. q35's main low region starts at guest
  address 1 MiB and had its own anonymous mapping at an address the kernel
  chose. KVM uses 2 MiB pages for a memslot only when the host and guest
  addresses are equal modulo 2 MiB, so the low 2 GiB was mapped 4 KiB at a
  time: about 3,270 nested page faults per 100 ms during GRUB against 7
  under QEMU, 22,000 for the 88 MiB. Decompression touches fresh pages the
  same way. Each anonymous region is now placed congruent with its guest address. The
  mapping stays owned by vm-memory. A region that cannot be placed keeps
  its plain mapping.
- F3, SMI kicks. Every SMI signalled every vCPU thread, although without
  broadcast the only target is the vCPU that raised it. The halted second
  vCPU left `KVM_RUN` 22,000 times and spent 1.1 seconds of host kernel
  time during GRUB. An SMI whose only target is its issuer now signals no
  thread.

Diagnostic boots after each step, one to three per build, on the shared
host. One QEMU boot slowed by statistics polling is left out:

| Build | First kernel line (s) | Boot to agent ready (s) |
|---|---|---|
| QEMU 11.1.3 | 1.46 to 1.56 | 6.56 to 6.78 |
| vmlab-hv at round 7 | 1.86 to 2.00 | 6.85 to 6.99 |
| F3 only | 1.81 to 1.87 | 6.79 to 6.86 |
| F3 and F1 | 1.66 to 1.75 | 6.67 to 6.80 |
| F3, F1 and F2 | 1.41 to 1.47 | 6.39 to 6.48 |

### Quiet-window rerun

The rerun used round 7's method on helios from
`45558cb1811443e4e3fc726991eb20a80425412c`: `bench/run.py --arch x86_64
--guest ubuntu-26.04 --repeats 3`, all three QEMU repeats and then all
three vmlab-hv repeats, each on a fresh overlay, CPUs 16 to 23, governor
performance, 24 GiB scopes with swap disabled, Ubuntu 26.04.3. QEMU was
11.1.3. The release vmlab-hv binary SHA-256 was
`5b99c245498fe33920e636aab2fddd1347c7ca56edee82ca1c12b5e50f9154a8` and the
iPXE ROM, rebuilt from this revision, was
`66308ed2100e05385a600baae371e9e69675e4bd0b287f4e57bfce04c4e8b727`.

The card ran alone on the board. No project VM ran before or after. No
build or check ran during the window. The monitor took 125 samples at
five-second intervals from 17:35:15 to 17:45:35 UTC. The one-minute load
ranged from 0.58 to 1.68, the five-minute load from 1.00 to 1.49 and the
fifteen-minute load from 1.09 to 1.23. The opticlang pause was not
confirmed. Logs, hashes and the diagnostic scripts remain on helios under
`/home/vmlab/src/stc/logs/d28-card-71.r8a/`.

| Bar | Workload | QEMU samples | QEMU median | vmlab-hv samples | vmlab-hv median | hv/qemu | Verdict |
|---|---|---|---:|---|---:|---:|---|
| G1 | Ubuntu boot (s) | 6.51, 6.54, 6.48 | 6.51 | 6.38, 6.36, 6.34 | 6.36 | 0.9770 | PASS |

Every vmlab-hv sample is below every QEMU sample.

The same runs measure the other single-VM workloads. Their cards own the
verdicts (#73, #74). The values are recorded here because F2 changes how
all low guest RAM is mapped:

| Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Round 7 |
|---|---|---:|---:|---:|---:|
| G3 | Sequential read (MiB/s) | 18473.58 | 20967.17 | 1.1350 | 1.1071 |
| G4 | Sequential write (MiB/s) | 1488.03 | 3065.61 | 2.0602 | 2.6338 |
| G5 | Random read 4 KiB (IOPS) | 50452.60 | 70349.60 | 1.3944 | 1.3979 |
| G6 | Random write 4 KiB (IOPS) | 17263.90 | 16996.50 | 0.9845 | 0.7448 |
| G7 | UDP TX (Mbit/s) | 36694.70 | 25056.39 | 0.6828 | 0.7266 |
| G8 | UDP RX (Mbit/s) | 86395.98 | 109299.92 | 1.2651 | 1.2490 |

Idle host CPU was 0.93% of one core on QEMU and 1.97% on vmlab-hv. All 16
aggregate records and their samples are appended to
[`history/x86_64.jsonl`](history/x86_64.jsonl). The vmlab-hv records are
marked dirty because the QEMU run had appended to that tracked file.
Windows boot (G2) and the guest-to-guest pair were not run.

### Verification

Unit tests cover the three changes: the index ports read 0xff and keep the
selected register, a q35 layout's anonymous regions are congruent with
their guest addresses, and an SMI for its issuer alone interrupts no
registered vCPU thread while one raised by another vCPU does. The SMI test
needs KVM. It ran on helios outside the merge bar, which ignores KVM tests.

### Follow-up drafts

These are drafts. No card exists for them.

- FD1: RAM backed by the shared memfd (a machine with virtio-fs shares, and
  every pc machine) still gets 4 KiB nested pages. It needs shmem huge
  pages, which depend on the host's `shmem_enabled` setting and on
  `MADV_HUGEPAGE`.
- FD2: anonymous guest RAM is not marked `MADV_HUGEPAGE`. QEMU marks it. On
  a host with transparent huge pages set to `madvise`, vmlab-hv would get
  none. helios runs `always`.
- FD3: vmlab-hv answers ports 0x72 to 0x77 with an extended CMOS bank and
  aliases. QEMU's q35 reads 0xff on all six.
- FD4: each SMI still pays one extra `KVM_RUN` with `immediate_exit` to
  complete the APMC write, about 22,000 during this boot.

## Round 7 full quiet-host window (2026-11-18)

Card #32 ran the full x86_64 KVM suite on helios from
`82bec6b52c422a8313849f0496fdca4b989ad400`. The board reserved the window
for this card. Admission found zero project VMs at the start and after
the run. The log records host load. The opticlang pause was unconfirmed.

Each backend completed three fresh-overlay repeats for Ubuntu 26.04.3
(full and guest-to-guest) and Windows 10 19045.2006.10 (standard variant,
boot only). Within each configuration all QEMU repeats ran before the
vmlab-hv repeats. Single-VM runs used CPUs 16–23 and the TCP pair used
16–19 and 20–23. The governor was set to performance and VM scopes had a
24 GiB cap with swap disabled. QEMU was 11.1.3. The release vmlab-hv binary
SHA-256 was `a7eea151afd10d8e589a854052691f53d34fdfa710989a95154f3f8218fc3ae9`.
The iPXE ROM SHA-256 was
`d16c126333662d3cd6127d9ef27c463a04de263d7edbde200441c2c58bf4153e`.

The first Windows attempts stopped before VM launch because the runner's
default OVMF path was absent. Both retries used the same plain OVMF pair
from the published v0.46.1-alpha guest bundle retained by the q35 soak.
The CODE SHA-256 was
`29ae4bcaea971a66df26493f294a786986dcce482e3109e45634b951c362cf26`
and the VARS SHA-256 was
`5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e`.
All six measured configurations and `bench/report.py --arch x86_64`
completed successfully.

The monitor took 298 samples at five-second intervals from 10:43:08 to
11:07:54 UTC. Load averages (1, 5, and 15 minutes) started at
1.38, 1.27, and 2.12 and ended at 1.87, 1.79, and 1.78. The sampled
ranges were 0.45–3.15, 0.90–2.13, and 1.53–2.16. The runner logs and both
load logs remain on helios under
`/home/vmlab/src/stc/logs/d28-vml-13.n9E4VicG/`.

Boot requires a median ratio at most 1.00. Throughput requires at least
0.80. Verdicts use the ratios before rounding.

| Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Verdict |
|---|---|---:|---:|---:|---|
| G1 | Ubuntu boot (s) | 6.60 | 6.93 | 1.0500 | FAIL (card #71) |
| G2 | Windows 10 boot (s) | 89.93 | 103.11 | 1.1466 | FAIL (card #72) |
| G3 | Sequential read (MiB/s) | 15505.99 | 17166.19 | 1.1071 | PASS |
| G4 | Sequential write (MiB/s) | 1480.02 | 3898.07 | 2.6338 | PASS |
| G5 | Random read 4 KiB (IOPS) | 50750.00 | 70941.20 | 1.3979 | PASS |
| G6 | Random write 4 KiB (IOPS) | 17723.00 | 13200.50 | 0.7448 | FAIL (card #73, closed above) |
| G7 | UDP TX (Mbit/s) | 35457.47 | 25763.87 | 0.7266 | FAIL (card #74) |
| G8 | UDP RX (Mbit/s) | 86199.69 | 107667.66 | 1.2490 | PASS |
| G9 | TCP A to B (Mbit/s) | 26851.93 | 31155.99 | 1.1603 | PASS |
| G10 | TCP B to A (Mbit/s) | 26498.21 | 31063.02 | 1.1723 | PASS |

Idle host CPU was 0.95% of one core on QEMU and 1.93% on vmlab-hv.
D28 sets no idle CPU threshold. The QEMU sequential-write median
fell from 4,769.18 MiB/s in round 6 to 1,480.02 MiB/s here, so the wider
passing margin reflects a lower QEMU baseline.

All 22 aggregate records and their raw samples are appended to
[`history/x86_64.jsonl`](history/x86_64.jsonl). The runner marks later
configurations dirty because earlier configurations had already appended
to that tracked file. Both backends used the source revision above.

## Round 6 owner decisions (2026-11-02)

The owner accepts G1 at parity: Ubuntu boot at 6.61 seconds versus QEMU's
6.59 seconds is within noise. Round 6 does not pursue further boot changes.
The round-5 table below retains its strict numerical verdict for traceability.

UDP TX and sequential-write regressions must be investigated before the
network optimizations. If TCP B to A does not recover with UDP TX, the owner
accepts its round-5 0.7974x result as noise-level. This exception does not
change the UDP TX or sequential-write bars. The default remains unchanged
until the remaining work and the authorized quiet-window comparison finish.

## Round 6 development checkpoint (2026-11-02)

This is shared-host evidence, not a quiet-window verdict. G4 remains
unresolved. No default, workload size, flush policy, or D28 threshold changed.
The owner scheduled the full merge gate after the focused quiet window.

### Historical commit comparisons

The quiet-window ratio changes do not establish raw HV slowdowns. Between
rounds 4 and 5, HV UDP TX rises from 23.38 to 24.50 Gbit/s and sequential
write rises from 3843.93 to 3986.11 MiB/s. QEMU rises more: 30.94 to 38.83
Gbit/s and 4793.65 to 5337.45 MiB/s. The lower ratios remain failed bars;
they do not, by themselves, identify a regression caused by an HV commit.

Frozen release binaries cover round 4, queue-wake coalescing, firmware
configuration I/O, mergeable receive buffers, and round 5. Each workload
runs alone in three rotated cohorts. UDP uses three 12-second sends per
fresh overlay. Writes use seven fresh 512 MiB files per overlay, each with
the unchanged O_DIRECT writes and timed final fdatasync.

The write column below uses only the first write in each fresh overlay.
Later writes have warmer mappings and must not conceal the cold-write gap.
Every sample, including the later writes, is retained in the profile.

| Revision | UDP TX median (Gbit/s) | First sequential write median (MiB/s) |
|---|---:|---:|
| round 4, `41e53e4` | 24.21 | 3776.81 |
| wake coalescing, `e9333a9` | 23.89 | - |
| firmware I/O, `2bec7a2` | - | 3650.99 |
| mergeable RX, `a3ddb7b` | 24.25 | 3790.71 |
| round 5, `95ea79d` | 28.01 | 3571.90 |

UDP's round-5 regression does not reproduce in this cohort. Cold writes
remain 5.4% below round 4. The parent/child samples around block interrupt
suppression overlap and reverse direction in one cohort. These runs do not
establish either suspect as the cause. No speculative block or firmware
revert was made. This is an unresolved performance finding, not a claimed
fix or an owner-approved exception for G4.

### Retained network changes

N1 receive DMA reads the frame length separately and readv fills validated
guest spans. A short body is captured in the existing owned decoder state.
The header and used entries are published only after the complete packet.
Three rotated RX cohorts improve the median from 79.03 to 104.43 Gbit/s.
The RX-only confirmation measures 77.03 to 98.81 Gbit/s. The per-packet
direct-TX candidate is rejected: 24.42 versus 31.35 Gbit/s.

N2 batches transmit spans in writev, keeps descriptor ownership until the
unsent suffix is owned, and uses 512 KiB buffered RX chunks. UDP TX rises
from 23.22 to 25.20 Gbit/s across three rotated cohorts. A paired RX check
measures 74.86 to 100.78 Gbit/s. EVENT_IDX, POLLOUT continuation, ring wrap,
bounded staging, and legacy interrupt suppression remain covered by tests.

N5 requests 4 MiB SO_SNDBUF/SO_RCVBUF. Its UDP median falls from 26.66 to
25.05 Gbit/s, so the change is removed. Host socket sysctls are unchanged.

N6 offers CSUM and GUEST_CSUM. TX completes the seeded checksum during its
owned payload copy; the wire still carries bare Ethernet. Buffered RX
combines checksum calculation and placement. Direct RX validates its DMA
spans before setting DATA_VALID. Invalid checksums, unsupported formats,
and fragments requiring reassembly retain flags=0. No TSO, USO, or MQ is
offered. IPv4/IPv6, TCP/UDP, VLAN, odd splits, malformed metadata, and
short-write ownership have device coverage.

| N6 copy-path prototype | Before (Gbit/s) | After (Gbit/s) |
|---|---:|---:|
| UDP TX | 27.32 | 27.36 |
| TCP A to B | 25.17 | 34.71 |
| TCP B to A | 23.55 | 33.22 |

These are three interleaved cohorts. A later two-byte writev checksum-patch
candidate is also tested and rejected: UDP falls from 29.82 to 27.63 Gbit/s
in all three paired comparisons. The final source retains the copy path.
The prototype and final rebuild have distinct hashes; the final rebuild
has its own correctness and focused-runner smoke checks. Prototype timings
are not labelled as final-binary quiet measurements.

Linux ethtool reports generic TX checksum offload enabled under N6 and
fixed off before it. ethtool -S is captured but exposes no checksum counters.
Separate, untimed 32 MiB random TCP transfers match SHA-256 in both
directions. Guest probes then show TX checksum-helper calls disappearing.
A mixed QEMU/HV check also matches bytes and digests: QEMU records 348 TX
checksum-helper calls and 633 RX GRO checksum calls; N6 records zero for
both. Probes are installed only after timed transfers.

### Correctness and focused-window preparation

The final copy-based rebuild passes all 42 NIC tests and all 15 core S1
tests. S1 asserts both checksum features, validates nonzero on-wire UDP
checksums through the test switch, compares exact echo bytes, and snapshots
with UDP traffic active. The reset check passes four reboot methods, two
resets each, preserving the NIC stream and control connection. All 23 active
S2 tests and the explicit 1 GiB S2 check pass. Formatting, changed-crate
Clippy with warnings denied, and all 103 benchmark tests pass. Full
`just ci::check` remains for the owner's post-window merge gate.

`bench/phase2_window.py` prepares exactly G4, G7, G9, and G10 on both
backends, interleaved for three repeats. It performs no builds, boot bars,
read/random/RX/idle workloads, or CI. Preflight checks prepared assets;
the run reserves four admission locks and monitors VM ownership, memory
limits, and load. Only a complete cohort appends the eight target history
records. Its single-VM and guest-pair paths pass live smoke checks on both
backends without appending history. Allow 6-8 minutes for the authorized
window.

The raw diagnostics, source drivers, binary hashes, feature queries,
counter probes, negative control, rejected candidates, and verification
receipts are in `bench/profiles/phase2-round6-20261102.json`. Development
does not append benchmark history. Every VM uses the shared host count
check and a 24 GiB, zero-swap scope. Android and physical PCI/USB devices
are untouched. Temporary VM directories are removed after each run.

Runtime traits, the control protocol, switch framing, and VLNET v1 snapshot
layout are unchanged. The test-only SwitchStats adds udp_echoes. The default
is unchanged. The focused-window results below supersede the round-5 G4, G7,
G9, and G10 measurements; other bars retain their last full-suite verdicts.

### Round 6 authorized focused quiet window

The owner opened the window with other project work paused and no project VMs
running. The focused runner completed all three interleaved repeats on both
backends in 5m42s. It ran only sequential write (G4), UDP TX (G7), and TCP
guest-to-guest in both directions (G9/G10). No full suite, build, or CI ran.
The tested HV binary is the final copy-based N6 build, SHA-256
`93a214688c3289c59dd25be6893fc414e46b3883ebb0d54f7d538afc2322e818`.

| Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Required | Verdict |
|---|---|---:|---:|---:|---:|---|
| G4 | sequential write (MiB/s) | 4769.18 | 3859.40 | 0.8092 | >= 0.800 | PASS |
| G7 | UDP TX (Mbit/s) | 29361.04 | 25417.98 | 0.8657 | >= 0.800 | PASS |
| G9 | TCP A to B (Mbit/s) | 28519.17 | 32916.90 | 1.1542 | >= 0.800 | PASS |
| G10 | TCP B to A (Mbit/s) | 26809.29 | 33511.79 | 1.2500 | >= 0.800 | PASS |

Samples are retained in `bench/history/x86_64.jsonl`; the runner's complete
JSON and logs remain under `bench/run/`. Sender and receiver byte counts
matched for every TCP transfer. The three UDP sends completed without sender
errors. The host had 32 logical CPUs, performance governor, 24 GiB VM memory
caps with swap disabled, and the unchanged Ubuntu 26.04.3 template. Each load
entry is the host's 1-, 5-, and 15-minute load average at run start and end.

| Suite | Repeat | Backend | Sequential write | UDP TX | TCP A to B | TCP B to A | Load start -> end |
|---|---:|---|---:|---:|---:|---:|---|
| single | 1 | QEMU | 4769.18 | 40983.75 | - | - | 1.48, 3.38, 7.01 -> 1.72, 3.28, 6.88 |
| single | 1 | vmlab-hv | 3859.40 | 34597.70 | - | - | 1.72, 3.28, 6.88 -> 1.74, 3.18, 6.77 |
| single | 2 | vmlab-hv | 3683.14 | 25417.98 | - | - | 1.74, 3.18, 6.77 -> 1.46, 3.03, 6.64 |
| single | 2 | QEMU | 4731.02 | 29361.04 | - | - | 1.46, 3.03, 6.64 -> 1.41, 2.91, 6.53 |
| single | 3 | QEMU | 4865.58 | 28812.22 | - | - | 1.41, 2.91, 6.53 -> 1.58, 2.85, 6.43 |
| single | 3 | vmlab-hv | 3889.63 | 25042.77 | - | - | 1.58, 2.85, 6.43 -> 1.70, 2.80, 6.33 |
| g2g | 1 | QEMU | - | - | 31138.51 | 26809.29 | 1.70, 2.80, 6.33 -> 2.54, 2.88, 6.23 |
| g2g | 1 | vmlab-hv | - | - | 31204.22 | 32486.28 | 2.54, 2.88, 6.23 -> 3.31, 3.04, 6.14 |
| g2g | 2 | vmlab-hv | - | - | 32916.90 | 33511.79 | 3.31, 3.04, 6.14 -> 3.33, 3.07, 6.04 |
| g2g | 2 | QEMU | - | - | 28519.17 | 27443.33 | 3.33, 3.07, 6.04 -> 3.46, 3.11, 5.94 |
| g2g | 3 | QEMU | - | - | 26436.00 | 26630.34 | 3.46, 3.11, 5.94 -> 2.95, 3.03, 5.79 |
| g2g | 3 | vmlab-hv | - | - | 33636.92 | 35103.53 | 2.95, 3.03, 5.79 -> 3.08, 3.06, 5.70 |

The 15-minute load average declined from 7.01 to 5.70 over the window.
Each backend's result remains the median of its three interleaved samples;
no repeat was discarded. The complete result JSON also records template and
binary hashes, backend versions, workload counts, and per-second VM/load
monitoring.

## Round 5 authorized final quiet bench window (2026-11-02)

The final measurement passes six of ten core bars. G1, G4, G7, and G10 fail.
The default remains unchanged. This window supersedes the round-4 verdict
and the development sections' pending-window statements below.

The owner authorized the window with the other project SIGSTOPped, every
other session idle, no project VMs, and 68 GiB free in /tmp. Both backends
ran from source `a1eb3ec`. The tested HV binary's SHA-256 is
`eae67f98cdf14d89a3d19b86a02b808e07fff4dc9dcaa5c573021d641d8d388f`.

Both backends ran the actual `just bench-run-kvm` recipes for Ubuntu
26.04 and Windows 10, plus `just bench-run-g2g` for Ubuntu. Each has three
fresh-overlay repeats, alternating QEMU then vmlab-hv per repeat. Recipe
dependencies finish before either backend starts its first VM. Runtime
source, firmware, workload sizes, flushes, readiness, repeat counts, and
D28 thresholds stay unchanged.

All six benchmark commands and `just bench-report x86_64` exit 0. The
report prints the failed bars; its exit status does not enforce D28.
All 22 new aggregate records and their raw samples are appended to
`bench/history/x86_64.jsonl`. The earlier supplementary Server 2025
result is historical and is not included in these ten bars.

### Full bar table

Values are medians. Boot requires hv/qemu <= 1.0000. Throughput requires
hv/qemu >= 0.8000. Neither rounding nor spread changes the verdict.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Result |
|---|---|---:|---:|---:|---|
| G1 | Ubuntu boot (s) | 6.59 | 6.61 | 1.0030 | FAIL |
| G2 | Windows 10 boot (s) | 85.46 | 79.13 | 0.9259 | PASS |
| G3 | Sequential read (MiB/s) | 17197.94 | 14990.46 | 0.8716 | PASS |
| G4 | Sequential write (MiB/s) | 5337.45 | 3986.11 | 0.7468 | FAIL |
| G5 | Random read 4 KiB (iops) | 49844.60 | 68271.20 | 1.3697 | PASS |
| G6 | Random write 4 KiB (iops) | 30947.90 | 36018.80 | 1.1639 | PASS |
| G7 | UDP TX (Mbit/s) | 38828.77 | 24502.28 | 0.6310 | FAIL |
| G8 | UDP RX (Mbit/s) | 86589.76 | 82292.74 | 0.9504 | PASS |
| G9 | TCP A to B (Mbit/s) | 26395.45 | 21429.40 | 0.8119 | PASS |
| G10 | TCP B to A (Mbit/s) | 26695.35 | 21286.79 | 0.7974 | FAIL |

G2 and G9 now pass. G1 remains 0.02 seconds slower. G10 is below the floor
at 0.7974, while G4 and G7 have larger deficits. These measurements do not
identify the cause of the remaining gaps. Idle host CPU has no D28 bar:
QEMU measures 1.03% and vmlab-hv 2.57% of one core.

### Windows readiness and TCP checks

Windows 10 samples are 86.16, 85.46, and 85.30 seconds for QEMU, and
81.17, 79.13, and 78.84 for vmlab-hv. The unchanged marker, OOBE, AppX,
and guest-reboot readiness sequence completes. Existing boot-ID probes
change from `0x9` to `0xa` across all six requested reboots. Capturing
those return values adds no guest commands.

All 12 TCP transfers have matching sender and receiver byte counts.
Payload hashes were not collected. All 12 single-VM repeats exit cleanly;
the g2g teardown also stops every owned VM and peer.

### Load averages per run

Each triple contains the one-, five-, and fifteen-minute load averages.
Start observations are taken after the interleaving wait. Rolling averages
include preceding benchmark work. The original history environment's
pre-wait observations are retained separately.

| Run | Start load [1, 5, 15 min] | End load [1, 5, 15 min] |
|---|---|---|
| ubuntu qemu r1 | [0.34, 2.24, 3.79] | [0.55, 1.85, 3.49] |
| ubuntu vmlab-hv r1 | [0.55, 1.85, 3.49] | [0.52, 1.50, 3.19] |
| ubuntu qemu r2 | [0.52, 1.50, 3.19] | [0.57, 1.28, 2.94] |
| ubuntu vmlab-hv r2 | [0.57, 1.28, 2.94] | [0.48, 1.11, 2.70] |
| ubuntu qemu r3 | [0.48, 1.11, 2.70] | [0.62, 1.06, 2.53] |
| ubuntu vmlab-hv r3 | [0.62, 1.06, 2.53] | [0.60, 0.97, 2.34] |
| windows10 qemu r1 | [0.60, 0.97, 2.34] | [1.34, 1.15, 2.27] |
| windows10 vmlab-hv r1 | [1.34, 1.15, 2.27] | [1.85, 1.36, 2.25] |
| windows10 qemu r2 | [1.85, 1.36, 2.25] | [1.43, 1.36, 2.17] |
| windows10 vmlab-hv r2 | [1.43, 1.36, 2.17] | [2.05, 1.57, 2.17] |
| windows10 qemu r3 | [2.05, 1.57, 2.17] | [2.48, 1.79, 2.19] |
| windows10 vmlab-hv r3 | [2.48, 1.79, 2.19] | [1.75, 1.72, 2.13] |
| g2g qemu r1 | [1.75, 1.72, 2.13] | [2.36, 1.88, 2.17] |
| g2g vmlab-hv r1 | [2.36, 1.88, 2.17] | [3.05, 2.11, 2.23] |
| g2g qemu r2 | [3.05, 2.11, 2.23] | [3.05, 2.23, 2.27] |
| g2g vmlab-hv r2 | [3.05, 2.23, 2.27] | [3.02, 2.33, 2.30] |
| g2g qemu r3 | [3.02, 2.33, 2.30] | [3.12, 2.45, 2.34] |
| g2g vmlab-hv r3 | [3.12, 2.45, 2.34] | [2.90, 2.48, 2.36] |

The one-second monitor records 1361 observations, a one-minute load range
of 0.31 to 3.41, at most two owned VMs, and no foreign project VM. Android
is excluded. Every observed VM has memory.max 25769803776 and
memory.swap.max 0. The coordinator holds all four shared admission locks;
the harness also checks capacity before every launch.

Single VMs use CPUs 16-23, and g2g uses 16-19 plus 20-23. The governor is
performance. The existing Ananicy policy remains unchanged: QEMU is moved
to nice 9 while vmlab-hv inherits nice -4. No profiler or exit-recorder
override is active.

### Evidence and cleanup

`bench/profiles/phase2-round5-final-20261102.json` contains the complete
bar table, all records and samples, load triples, monitor, command logs,
firmware and binary hashes, existing boot-ID probes, TCP counters, and
coordinator sources. The measurement job has a clean exit-0 receipt.
There are no discarded attempts or resource failures in this window.

Every owned temporary VM directory is removed as its run finishes, with
logs retained on disk. The minimum observed /tmp free space is
71086792704 bytes, about 66.2 GiB. The empty temporary root and the
measurement job are removed after completion. No project VM remains.

`just ci::check` is deliberately not run in this window, as instructed.
The orchestrator owns the subsequent integration gate. Later history
dirty flags reflect appended history, not runtime edits. The window changes
no default, threshold, shared interface, or snapshot format. Nothing is
merged or pushed.

## Round 5 development diagnostics (2026-11-02)

These are shared-host diagnostics on main `41e53e4`, not a new D28 verdict.
The round-4 quiet-window table remains the gate result. No history record,
threshold, firmware, guest workload, or default changed.

### Windows readiness across reset

The large Windows 10 gap includes a benchmark-client defect. Sysprep can
reboot after the specialize marker appears. The host's agent socket stays
open, but an outstanding command belongs to the previous guest boot. The
old client waits its full 90-second exec timeout before reconnecting.
Windows' own OOBE log finishes much earlier than the host's next probe.

An unchanged main binary reproduced a 156.74-second boot. A later QEMU
boot reproduced the same failure at 157.36 seconds, including an explicitly
recorded 90.09-second OOBE-probe timeout. Other boots of both binaries avoid
the race. This does not identify a VMM regression between `6cceef4` and main.

The runner now observes QMP or S1 reset events and cancels commands from the
previous agent generation. It reconnects and repeats the same probe. The
marker, OOBE, AppX, final guest reboot, and boot-ID checks remain. A marker
already observed is remembered if reset interrupts its deletion, so retrying
the deletion cannot lose the readiness proof.

Interleaved fresh-overlay boots used frozen VMM binaries. The old and fixed
columns are complete readiness times, not Hello-only measurements.

| Backend | Old readiness samples (s) | Fixed readiness samples (s) |
|---|---|---|
| QEMU | 157.36 | 92.92 |
| vmlab-hv main | 86.15, 85.67 | 83.83, 83.00 |

The old main samples in this cohort avoided the race; the earlier 156.74
capture remains separate. A controlled S1 reproduction on one Windows VM
scheduled a reboot during the same long command. The old client timed out
after 90.09 seconds; the fixed client cancelled after 3.36 seconds. S1 reset
events and BootId transitions `0xa -> 0xb -> 0xc` prove both guest reboots.
Six socket and readiness regressions pass, including both control protocols.
The reset-cancellation and marker-deletion regressions failed before their
corrections.

Server 2025 also completed the unchanged readiness sequence with the fixed
client: QEMU 76.98 seconds and vmlab-hv 71.18 seconds. Those samples shared
the host with other diagnostics and do not replace its round-4 result.

### Windows hardware and phase evidence

| ID | Hypothesis | Evidence |
|---|---|---|
| H1 | Coarse userspace HLT or synthetic timers | Both use KVM's irqchip, LAPIC, halt handling, and Hyper-V timers. Host halt polling is 200 us. PID-filtered HLT and scheduler traces record runnable-to-scheduled latency separately from the intended halt duration. |
| H2 | Missing Hyper-V enlightenments | Installed CPUID matches across QEMU, `6cceef4`, and main: feature leaf `0xaff/0x30/0/0x108`, recommendations `0xe24`, spinlock limit `0x1fff`. TLB flush, IPI, SynIC, timers, and AVIC are present. Neither reference launch enables stimer-direct. |
| H3 | Expensive reset or firmware rerun | Typical final reset-to-agent intervals are about eight seconds on both current backends. OVMF BDS, guest kernel-start events, marker, OOBE, AppX, and agent timestamps are retained for each captured boot. |
| H5 | Trapped framebuffer writes | Both register an 8 MiB dirty-logged mapping at `0x80000000`. Complete MMIO-page histograms in the second observer cohort contain no exits in that mapping. Legacy VGA-aperture exits are separate. |

Windows 10 and Server 2025 use the same configured hardware: four vCPUs,
8192 MiB, installed OVMF, enlightenments, virtio-blk, virtio-vga, tablet,
and no NIC. Their images and provisioning work differ.

Guest kernel-start events are calibrated against a post-boot guest/host
clock observation, with the RPC interval retained as uncertainty. These
bound boot-manager and kernel/service startup phases; they do not measure
kernel-only CPU time. The initial all-exit main trace overran by 166526
records and is marked incomplete. The replacement HLT-filtered captures
have no overruns. Raw captures are retained as lossless gzip files.

One complete 157.36-second QEMU capture contains 483609 HLT exits; a complete
83.00-second fixed-main capture contains 282281. Their HLT-blocked vCPU
runnable-to-scheduled p95 is 3 us on both backends, with p99 of 12 and 8 us.
The different capture durations prevent interpreting those totals as an
exit-rate improvement. This scheduler interval is not timer-expiration to
the first resumed guest instruction.

`bench/profiles/phase2-round5-windows-20261102.json` contains the complete
samples, failed probes, guest logs, phase markers, installed CPUID and
memory-registration evidence, trace summaries, losses, compressed-capture
hashes, and diagnostic sources. Credential-related guest log lines are
omitted from the committed copy; original local diagnostic logs remain.

### Network queue service and G1

H4, the offload hypothesis, was checked inside the guests. CSUM,
GUEST_CSUM, HOST_TSO4/6, and GUEST_TSO4/6 are disabled on both backends with
the stream-socket attachment. QEMU removes those capabilities when its
peer has no virtio header. MRG_RXBUF is offered by QEMU and remains absent
from vmlab-hv; this pre-existing difference was not identified as the TX
cause. Negotiated bits and complete ethtool output are retained.

The NIC now coalesces worker wakes, rearms before processing work, and
services both queues without enqueuing a command for each kick. This
removes duplicate queue walks. Lifecycle barriers remain ordered, and
backpressure, POLLOUT continuation, EVENT_IDX, and snapshot bytes remain
unchanged. Coalescing socket writes alone did not improve UDP and was not
retained as a standalone fix.

The following are medians of three fresh-overlay boots per configuration.
UDP uses the median of three unchanged 12-second sends within each boot.
Each TCP direction runs once per fresh pair. The cohorts alternate QEMU,
baseline, and candidate with rotating order.

| Workload | QEMU (Mbit/s) | Baseline (Mbit/s) | Coalesced queue service (Mbit/s) |
|---|---:|---:|---:|
| UDP TX | 28113.15 | 22724.03 | 25099.14 |
| TCP A to B | 24552.06 | 13616.11 | 17382.59 |
| TCP B to A | 29395.08 | 16006.86 | 18886.95 |

The distributions overlap. UDP VM-process CPU seconds per GiB increased;
no CPU-efficiency gain is claimed. TCP remains below 80% of QEMU in this
cohort, and G9 is not closed. All 18 TCP transfers have matching sender and
receiver byte counts. The new wake regression fails before the fix with
200 socket bytes instead of one. All 24 network unit tests and the S1 NIC
test pass, including all 900 echoes across save and restore.

UDP used CPUs 8-15 while Windows diagnostics used 16-23. TCP used 16-19 and
20-23 and overlapped the final UDP cohort and CI. Every VM used the shared
four-slot admission checks and a 24 GiB, zero-swap scope. The host's
Ananicy rule moves QEMU to nice 9 and best-effort I/O priority 7; vmlab-hv
inherits nice -4. This policy is recorded, not changed. These conditions
exclude a quiet-host verdict.

G1 remains unresolved. A startup profile pair reaches first kernel output
at 1.81 seconds under QEMU and 2.35 under baseline vmlab-hv. The requested
one-second leaf samples contain 329 and 626 samples, with 176 and 170
unresolved. Run-loop synchronization, mutex contention, and platform clock
service remain distributed costs. This does not establish one sufficient
safe change, and no boot-time fix is claimed. Memory-map quiescence and
timer synchronization were preserved.

`bench/profiles/phase2-round5-network-20261102.json` retains every sample,
CPU counter, feature query, failed setup attempt, discarded wake-only
candidate, startup profile, TCP byte check, and driver source. Rust shared
traits, the control protocol, and snapshot formats are unchanged. The
benchmark agent client adds an optional reset monitor; other callers keep
their existing behavior.

### Initial correctness gate

`just ci::check` passed after the fixes: formatting, clippy with warnings
denied, and 998 tests, including S1 and S2 coverage. Six slow, manual, or
live-session diagnostics remain ignored. All 89 benchmark-harness tests passed
separately. Physical USB and VFIO opt-ins were unset; real-device paths
were not exercised. Owned temporary VM overlays were removed; diagnostic
logs and compressed traces remain available.

The initial pass leaves G1 and G9 open. G2 and G7 need confirmation in the
next authorized quiet window. The default remains unchanged. READY FOR BENCH WINDOW.

### Owner-requested G1/G9 follow-up

The owner requested another focused pass before the quiet window. The
following measurements remain shared-host diagnostics. The strict D28
verdict and the default are unchanged.

F1, ECAM service during SeaBIOS and GRUB boot, accounts for a large part
of the pre-kernel gap. vmlab-hv completes firmware/ROM loading, ACPI
construction, and vCPU creation within about 0.1 seconds. QEMU also reaches
its first KVM exit in about 0.1 seconds. Captured fw_cfg DMA service is
about 0.1 ms in vmlab-hv. Neither accounts for the original gap. The disk's
ECAM page receives about 44771 accesses. Its observed userspace service
interval totals 0.516 seconds before the first correction and 0.100 after.
Those intervals include scheduling and observer overhead, not only device
CPU time.

ECAM reads now call each configuration dword once, including unaligned
reads, instead of repeating a side-effecting read for every returned byte.
Writes through virtio's indirect configuration window no longer stop every
other vCPU, recheck VGA mappings, or resynchronize platform timers and PIRQs for
each access. The exception covers only the captured net, block, and
console identities and bytes 0x88 through 0x97. All other configuration
writes retain the memory-update barrier. PAM, SMRAM, VGA, A20, and VFIO
mapping rules are preserved. Both new regressions failed before the fix.

The initial interleaved candidate reduces the median launch-to-first-kernel
interval from 2.251 to 1.851 seconds; QEMU measures 1.614. Full readiness
changes only from 8.16 to 8.12 seconds in that cohort. Unmapped ROM reads
remain visible in the observer. QEMU registers ioeventfds, but the large
firmware disk cost appears in configuration-window accesses; it is not a
measurement of Linux's steady-state BAR notification path.

F2, console input before guest open, explains the remaining large readiness
penalty. vmlab-hv previously consumed host bytes when receive buffers
existed, even if the guest had not opened the named port. The kernel can
discard that early Hello. QEMU's `virtio_serial_guest_ready` gates input on
guest-open state. The device now applies that backpressure and delivers
staged data when the guest opens the port. The socket regression fails
before the correction. No benchmark timing or readiness rule changed.

| Console cohort | Full readiness median (s) | Samples (s) |
|---|---:|---|
| QEMU | 6.80 | 6.84, 6.80, 6.69 |
| Before guest-open correction | 8.13 | 8.13, 8.12, 8.13 |
| After guest-open correction | 6.90 | 6.92, 6.90, 6.80 |

F3, mergeable network receive buffers, closes the remaining observed
feature difference relevant to RX allocation. CSUM, GUEST_CSUM,
HOST_TSO4/6, and GUEST_TSO4/6 remain off on both stream backends. MRG_RXBUF
is now offered and negotiated, as in QEMU. One header describes each
packet; all receive chains are copied before one used-index publication.
If capacity is insufficient, the packet and original available position
remain pending, while the refill event targets the end of the inspected
buffers. This avoids repeatedly retrying the same insufficient buffers.

Tests cover split headers, multi-buffer packets, no DMA or partial
publication before refill, short and inaccessible buffers, ring wrap,
and interrupt events crossing the whole batch. The first two mergeable
RX regressions failed before implementation. The S1 snapshot test now
asserts MRG_RXBUF and EVENT_IDX negotiation and passes all 900 normal and
60 unfragmented jumbo echoes across save and restore.

| Three-pair TCP cohort | QEMU (Mbit/s) | Before MRG_RXBUF | After MRG_RXBUF | After/QEMU |
|---|---:|---:|---:|---:|
| A to B | 26029.85 | 20914.20 | 22752.00 | 0.8741 |
| B to A | 26578.61 | 22166.90 | 23007.16 | 0.8656 |

All 18 cohort transfers and four later combined-build transfers have
matching sender/receiver byte counts. Payload hashes were not collected.
The quiet-window G9 result remains unmeasured. TCP CPU efficiency was not
measured in this cohort.

F4, legacy block interrupt suppression, removes unnecessary completion
interrupts while firmware polls the ring. Without EVENT_IDX, the block
device previously ignored the driver's NO_INTERRUPT flag. It now respects
that flag; EVENT_IDX still takes precedence when negotiated. The polling
regression fails before the fix. The observer records 22290 pre-kernel
KVM_IRQ_LINE calls before this change and six after; QEMU records zero.

| Final interleaved boot cohort | First kernel median (s) | Full readiness median (s) | Full readiness samples (s) |
|---|---:|---:|---|
| QEMU | 1.636 | 6.75 | 6.76, 6.75, 6.73 |
| Before block interrupt correction | 1.871 | 6.81 | 6.80, 6.81, 6.89 |
| All corrections | 1.792 | 6.76 | 6.72, 6.78, 6.76 |

The final full-readiness ratio is 1.0015. G1 is still above the strict bar
in this shared-host cohort; neither rounding nor a noise allowance makes
it pass. First-kernel output is sampled every 5 ms. Host setup, firmware
execution, first kernel output, and full readiness remain separate metrics.
The benchmark clock includes the systemd scope launch. The observer also
timestamps VMM exec through its loader constructor, vCPU creation, and
the first KVM exits.

Boot diagnostics use CPUs 8-15; TCP uses 16-19 and 20-23. Some cohorts
overlap on those separate physical-core groups, with brief builds and
unit checks also recorded. Full CI starts after every timed cohort ends.
The existing Ananicy priority difference remains unchanged. Each VM uses
the shared four-slot admission checks and its own 24 GiB, zero-swap scope.

`bench/profiles/phase2-round5-followup-20261102.json` retains all cohorts,
feature queries, observer records, source drivers, binary hashes, failures,
and validation receipts. The first observer lacked mmap64 interception;
two early captures stopped on UART setup rather than kernel text. These
captures are marked unsuitable for port-cost conclusions. A wrapped Herdr
exit marker also prevented its build-status parser from reading one
successful build; the displayed EXIT=0 and build log are retained.

No Rust trait, control protocol, switch framing, or snapshot layout changed.
The guest-visible change is MRG_RXBUF support, matching the reference.
The runtime changes are ready for the authorized quiet-window comparison.

The final `just ci::check` passes formatting, clippy with warnings denied,
and 1007 Rust tests, including S1 and S2. Six slow, manual, or live-session
diagnostics remain ignored. All 89 benchmark tests and the final release
build pass. Physical USB and VFIO opt-ins remain unset; no real devices
were exercised. Owned temporary benchmark VM directories were removed.

READY FOR BENCH WINDOW. G1 still needs a strict quiet-host result; G9's
shared-host improvement does not change the existing gate verdict.

## Round 4 authorized quiet bench window (2026-11-02)

CI was deliberately not run in this window, as instructed by the owner.
The orchestrator reserves the combined merge gate. The benchmark harness's
83 unit tests passed before timed measurements began.

The owner reported other projects SIGSTOPped, no project VMs running, and
every other session idle. Measurements use branch source `55301ef`, which
contains main `a8fdf05` through merge `c551327` and the test-only fixture fix
`19676ee`. The only new harness change registers Server 2025 as a boot-only
Windows guest. Runtime code, thresholds, readiness phases, and workload
sizes stayed fixed throughout the measurements.

Both backends ran the actual `just bench-run-kvm` recipes for Ubuntu,
Windows 10, and Server 2025, plus `just bench-run-g2g` for Ubuntu. Each
configuration has three fresh-overlay repeats. A scratch coordinator
alternated QEMU then vmlab-hv for each repeat, while preserving the original
recipes and three-sample aggregation. All eight benchmark commands exited
0. `just bench-report x86_64` also exited 0 and applied the unchanged D28
bounds. All 24 aggregate records and their samples are appended to
`bench/history/x86_64.jsonl`.

### Full bar table

Values are medians. Boot requires hv/qemu <= 1.0000; throughput requires
hv/qemu >= 0.8000. S25 is supplementary and does not replace Windows 10 G2.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Result |
|---|---|---:|---:|---:|---|
| G1 | Ubuntu boot (s) | 6.54 | 8.12 | 1.2416 | FAIL |
| G2 | Windows 10 boot (s) | 85.31 | 157.22 | 1.8429 | FAIL |
| G3 | sequential read (MiB/s) | 19671.17 | 16572.80 | 0.8425 | PASS |
| G4 | sequential write (MiB/s) | 4793.65 | 3843.93 | 0.8019 | PASS |
| G5 | random read 4 KiB (IOPS) | 50002.90 | 69281.10 | 1.3855 | PASS |
| G6 | random write 4 KiB (IOPS) | 30354.80 | 35546.80 | 1.1710 | PASS |
| G7 | UDP TX (Mbit/s) | 30936.27 | 23378.52 | 0.7557 | FAIL |
| G8 | UDP RX (Mbit/s) | 86595.75 | 91521.51 | 1.0569 | PASS |
| G9 | guest TCP A to B (Mbit/s) | 27822.22 | 21686.65 | 0.7795 | FAIL |
| G10 | guest TCP B to A (Mbit/s) | 26524.78 | 21818.77 | 0.8226 | PASS |
| S25 | Server 2025 boot (s) | 59.09 | 54.91 | 0.9293 | PASS |

G4 has little margin above the floor. No spread or noise allowance changes
the verdict. Idle host CPU, which has no D28 bar, was 0.95% for QEMU and
2.55% for vmlab-hv, measured as a percentage of one core.

### Server 2025 boot

The shipped template was `26100.1742.8`. Both backends used q35, 4 vCPUs,
8192 MiB, the same installed OVMF pair, Hyper-V enlightenments, virtio-blk,
virtio-vga, USB tablet, and no network interface. Readiness uses the existing
Windows specialize-marker, user-OOBE, AppX-settle, and in-guest reboot
sequence. It is not an early Hello-only measurement. The existing boot-ID
probes changed from 8 to 9 across all six requested reboots; retaining those
return values added no guest probes.

| Backend | Repeat 1 (s) | Repeat 2 (s) | Repeat 3 (s) | Median (s) |
|---|---:|---:|---:|---:|
| QEMU | 60.46 | 59.09 | 58.44 | 59.09 |
| vmlab-hv | 54.99 | 54.91 | 54.86 | 54.91 |

This profile does not reproduce the reported loaded-soak gap of 674 s
versus 418 s. That earlier observation is context, not a matched baseline
for this cohort. These measurements do not establish that any difference
from the soak was caused solely by host load or by a particular patch.

### Per-run host load and limits

The table records one-minute load at the actual start and end of each
repeat, after its interleaving wait. Full one-, five-, and fifteen-minute
values are retained per run in the evidence JSON. Rolling averages include
the preceding benchmark's own work.

| Configuration | Repeat 1 start->end | Repeat 2 start->end | Repeat 3 start->end |
|---|---|---|---|
| Ubuntu QEMU | 0.43->0.52 | 0.61->0.75 | 0.63->0.87 |
| Ubuntu vmlab-hv | 0.52->0.61 | 0.75->0.63 | 0.87->0.84 |
| Windows 10 QEMU | 0.20->1.76 | 1.41->1.27 | 1.80->2.51 |
| Windows 10 vmlab-hv | 1.76->1.41 | 1.27->1.80 | 2.51->1.55 |
| g2g QEMU | 1.55->2.36 | 2.20->2.50 | 2.95->3.34 |
| g2g vmlab-hv | 2.36->2.20 | 2.50->2.95 | 3.34->3.51 |
| Server 2025 QEMU | 3.51->3.00 | 2.79->2.22 | 2.40->2.35 |
| Server 2025 vmlab-hv | 3.00->2.79 | 2.22->2.40 | 2.35->2.19 |

The one-second monitor has 1949 observations for the successful cohorts,
with one-minute load ranging from 0.20 to 3.82. It observed at most two
project VMs, only for g2g, and no foreign project VM. Every observed VM had
memory.max 25769803776 and memory.swap.max 0. The coordinator reserved all
four shared admission locks; the original harness still checked actual
host capacity before every launch. Android was excluded from the count.
Single VMs used CPUs 16-23; pairs used 16-19 and 20-23. The governor was
performance. No debug recorder override or LD_PRELOAD profiler was active.
All 18 single-VM repeats exited cleanly, and every g2g transfer had exact
sender/receiver byte equality.

### Interrupted attempts and evidence

An initial recipe dependency rebuilt the HV binary for 3.62 s while the
first QEMU boot was starting. That attempt was interrupted before any
aggregate records and retained as invalid. The replacement cohort used a
prebuilt binary. Its SHA-256 is
`ec76f5838098ff2db24c51bf3c736337785bc1494047477e8d046aadac546764`.

After Ubuntu completed, the 94 GiB /tmp tmpfs filled during the first HV
Windows 10 repeat. The monitor hit ENOSPC and left a truncated final line.
That entire Windows comparison was invalidated before aggregate records.
The orchestrator removed 239 stale, unopened scratch directories from
finished sessions and restored 68 GiB free. Windows 10 was restarted with
three fresh repeats per backend, followed by g2g and Server 2025 on /tmp.
The complete Ubuntu cohort predates the resource failure and was retained.
The failed-attempt images and logs were not manually deleted or moved;
subsequent single-VM runs used the harness's normal overlay cleanup.

Herdr could not recover the final completed pane marker, and its waiter
raised a no-marker error. Individual command receipts show all eight
benchmarks and the report exited 0. The complete dataset, clean single-VM
exits, reboot checks, and memory caps were validated independently. This
runner issue is retained in the execution record, not presented as a clean
top-level receipt.

`bench/profiles/phase2-round4-final-20261102.json` contains the full bar
table, all samples and aggregate records, per-repeat load triples, monitor,
command logs, firmware and binary hashes, coordinator sources, and both
interrupted attempts. The g2g environment's pre-wait load is retained;
the coordinator observations supply its actual per-run start load. Dirty
flags in later history records reflect the appended history file, not
runtime source edits. All owned VMs, peers, and measurement jobs were
stopped after recording the results. Nothing was pushed or merged into
main. No threshold, default, flush requirement, or repeat count changed.

The earlier sections below are historical. This cohort supersedes their
gate verdicts and pending-window statements.

## Round 4 main integration before the bench window

The first post-merge CI attempt exited 101 on a pre-existing CPU-policy
fixture race. The test started a guest, immediately paused it, then changed
CPUID. KVM rejects changed CPUID after the first KVM_RUN. An isolated rerun
passed, but a 150 ms scheduling delay reproduced EINVAL at SET_CPUID2.
Commit `19676ee` starts that fixture paused, retains the delay, and asserts
the initial paused state. All original CPUID, MSR-log, TSC rollback, and
changed-frequency rejection assertions remain. Production code is unchanged
by that fix. All 202 active VMM unit tests passed before the full rerun.

The full `just ci::check` rerun exited 0 on `19676ee`: 987 tests passed,
zero failed, and six manual diagnostics remained ignored across 65 result
groups. Formatting and workspace clippy with `-D warnings` passed. The
suite ran with RUST_TEST_THREADS=1 and the 1800-second child-process
deadline, inside the 24 GiB, zero-swap scope. The ignored diagnostics were
not run explicitly. Physical USB/VFIO opt-ins remained unset.

The requested main `a8fdf05` was merged as `c551327`. Its one conflict was
in the vCPU post-KVM_RUN hooks. The resolution records opted-in exit
metadata first, then drains coalesced VGA writes before dispatch or
parking. VGA delivery remains on machine-events, while rendering stays
on the incoming display-frames worker. The halted-guest VGA S1 check also
passed with the exit recorder enabled. All 83 benchmark unit tests passed.

The release binary was rebuilt from `c551327`. The only source change
between that runtime and validated `19676ee` is the snapshot test fixture.
`bench/profiles/phase2-round4-main-a8fdf05-20261101.json` records both
commits, the release binary hash, complete validation logs, the initial
failure and its reproduction, and the kernel reference for the CPUID rule.

No post-merge D28 cohort has run. The timings below and the original
round-4 JSON describe the pre-merge runtime and remain historical evidence.
The display/audio-clock change and diagnostic hooks require fresh QEMU
and vmlab-hv measurements in the owner's authorized quiet bench window.
No threshold or default changed, and nothing was pushed.

## Round 4 development diagnostics

The round-4 changes start at `379c1ca`. These are shared-host diagnostics,
not final D28 measurements. The round-2 quiet-host verdict above remains
unchanged. Final validation waits for the owner's authorized bench window.
No threshold, readiness phase, workload, default, or firmware was changed.

### G7 historical comparison and notification progress

The first complete interleaved cohort ran QEMU, historical `8bcbf1b`,
baseline `379c1ca`, and two notification candidates three times each.
QEMU's UDP TX median was 30553.57 Mbit/s. Historical and baseline medians
were 18557.61 and 15294.84 Mbit/s, with overlapping ranges. Source review
identified added per-exit locks, but the measurements assign no regression
commit. Host load exceeded 230 during this investigation.

The initial TX-suppression and EVENT_IDX candidates timed out. One saved
guest remained responsive with its sender blocked in `sock_alloc_send_pskb`.
Source review found a lost continuation: an independent wakeup flush could
empty staging after TX left guest kicks suppressed. The next poll then had
neither POLLOUT interest nor a kick to service pending descriptors. The
extracted wakeup regression failed before the correction and passed after
TX service was added after that flush. The precise interleaving was not
observed inside the saved timed-out VM.

The corrected cohort repeated every backend and candidate, interleaved,
with fresh overlays. All twelve runs completed with validated peer bytes.

| Candidate | UDP TX samples, Mbit/s | Median |
|---|---|---:|
| QEMU | 30982.15, 35051.78, 37601.39 | 35051.78 |
| Baseline | 22710.89, 26522.83, 23471.66 | 23471.66 |
| Corrected net notifications | 22554.35, 22573.95, 19050.46 | 22554.35 |
| Corrected net plus PCI decode cache | 22528.34, 28737.59, 22731.14 | 22731.14 |

These distributions do not establish a UDP speedup or close G7.

### Retained changes and profiles

The read-only QEMU comparison covers `hw/net/virtio-net.c`, notification
ioeventfds in `hw/virtio/virtio-pci.c`, block iovecs and merging in
`hw/block/virtio-blk.c`, VGA coalescing, PCI ROM backing, and flash ROMD.
The evidence JSON records the reference revision and each conclusion.
IOeventfd, direct ROM mapping, and VARS ROMD remain unimplemented candidates,
not established causes or claimed fixes. No QEMU code was copied or translated.

- F1. The NIC suppresses kicks while draining, rearms with the rust-vmm
  recheck protocol, and offers EVENT_IDX. Backpressure retains a POLLOUT
  continuation, including after wakeup flushes. Malformed available indices
  terminate processing instead of spinning. RX rearms on buffer shortage
  and retries an already-published buffer after a short descriptor.
- F2. Four exact-address PCI MMIO decode entries avoid rescanning ROMs,
  BARs, and command registers on repeated notifications. Configuration,
  topology, ROM attachment, and restore invalidate the entries. Values
  remain device reads and writes. In the baseline UDP leaf profile,
  `find_rom`, `find_bar`, and virtio PCI `config_read` accounted for 7.37%,
  5.46%, and 5.23% of samples. None appears in the cached profile's top 40.
  The unit reproduction changes 34 configuration reads to 2.
- F3. Modern machines batch legacy VGA writes through KVM's coalesced
  MMIO ring. Consumers serialize delivery before slot release, drain before
  register accesses and vCPU parking, and poll for writes from halted
  guests. Software-A20 machines keep their original access path. The ring
  drains into ordinary VGA state before snapshot, so the snapshot format
  is unchanged.

The profiles retain unresolved samples and DSO metadata. The baseline net
profile has 11853 samples, 2960 unresolved; the cached profile has 11979,
2936 unresolved. These are leaf IP counts, not call counts or elapsed-time
fractions. QEMU, baseline, and corrected cache candidates also have raw
`strace -c -f` tables. Tracing heavily changes throughput, so traced rates
are not gate results or an isolated cache comparison.

Three fresh-pair TCP repeats completed for every backend, with exact
sender and receiver byte equality. The combined net/cache candidate has
A-to-B and B-to-A medians of 16020.84 and 16941.49 Mbit/s. Baseline medians
are 9973.36 and 9470.04; QEMU medians are 20022.21 and 23453.85. The
combined ratios are 0.800 and 0.722. The comparison cannot separate the
net policy from the cache, and does not close G10.

### Boot and block evidence

The three-repeat interleaved Ubuntu cohort compares QEMU, corrected
net/cache, and the same candidate plus VGA batching. First-kernel medians
are 1.811, 2.686, and 2.486 seconds. Agent-ready medians are 8.20, 12.13,
and 8.27 seconds. Four-second readiness variation remains visible in the
raw samples, so these medians do not establish a quiet-host G1 pass.

The exit-counter observer initially mistook the additional ring mapping
for vCPU 0's `kvm_run` mapping. That coalesced profile is invalid and
retained as such. After correcting the offset filter, all three backends
were repeated. Legacy VGA MMIO exits were 18014 for QEMU, 138448 before
batching, and 14172 after. Total MMIO exits fell from 221958 to 96937.
The corrected single-repeat agent-ready times were 8.17, 8.18, and 8.29
seconds. Exit reduction is established; its isolated readiness gain is not.

Windows received an exit-count comparison on unchanged installed OVMF,
with QEMU at 175.87 seconds and baseline at 133.75 seconds. Some small
compilation overlapped this diagnostic. OVMF VARS reads remain a candidate
for future investigation, not a demonstrated G2 cause or a change here.

Block profiles and syscall tables compare the existing 8 GiB direct-read
path in both hypervisors. Profiled medians were 9751.86 MiB/s for QEMU
and 13923.05 for baseline. The traced medians were 10574.68 and 9502.56.
Their disagreement reinforces the measurement limit. No disk worker,
flush, memory-validation, or cache-policy change was justified by this
evidence. G3 and G9 margin still need the final cohort.

### Method and retained failures

Single VMs used CPUs 16-23; pairs used 16-19 and 20-23. Every launch
checked host capacity and used the shared slot locks. All VMs ran inside
24 GiB, zero-swap scopes, with at most four project VMs host-wide and
the owner's Android emulator excluded. The requested boot hold was
honored until `GATE WINDOW CLOSED`. Other sessions' processes were not
stopped. Diagnostic runs do not append records to benchmark history.

The first baseline executable was renamed, which prevented PID discovery.
It was reaped and the comparison restarted with the counted `vmlab-hv`
basename. The initial strace driver treated its own requested SIGINT
detach, exit -2, as failure. Those command failures and tables remain in
the evidence. Corrected runs accept only an explicitly requested detach.
The rejected candidate's UDP timeout is separate from that driver error.

`bench/profiles/phase2-round4-20261101.json` retains every diagnostic
record, failed candidate, cohort, binary and firmware hash, raw profile,
syscall table, exit counter, and driver source. It explicitly rejects a
quiet-host claim. G1, G2, G7, and G10 remain open pending the final cohort;
the historical G7 regression commit remains unassigned.

### Pre-merge correctness and completion

`just ci::check` exited 0 on the pre-merge runtime, inside the 24 GiB,
zero-swap scope with RUST_TEST_THREADS=1 and a 1800-second child-process
deadline. Its 64 result groups report 945 passing tests, zero failures,
and three ignored tests. Formatting and workspace clippy with `-D warnings`
passed. The expanded S1 NIC check negotiated EVENT_IDX and delivered all
900 echoes before save, after save, and after load. The halted-guest VGA
S1 check and all four ring tests passed, including concurrent consumers.
Firmware, reset, snapshot, and snapshot-during-fsync checks passed.
All 23 active S2 checks passed against qemu-img.

The three ignored tests were the 1 GiB S2 throughput baseline, the 4 GiB
zero-fill diagnostic, and live PipeWire loopback. They were not run
explicitly this round. All 83 benchmark unit tests also passed; their
existing mock ResourceWarnings remain. Physical USB/VFIO opt-ins were
unset, so real-device paths were not exercised. No host device was claimed,
rebound, or reset.

No scoped `todo!()` or `unimplemented!()` remains. No shared Rust trait,
control protocol, or snapshot-format signature changed. The net feature
offer now includes EVENT_IDX. Runtime fixes are separate commits on
`wp/p2-perf4`; the performance branch was not merged into main or pushed.
All owned measurement VMs,
peers, profilers, and CI jobs were reaped after recording their results.
Final quiet-host D28 validation remains unfinished by the task's explicit
bench-window instruction. The default stays unchanged.

## Round 3 authorized bench window

The requested runs completed, but external compiler work prevents a
quiet-host claim. The owner opened the window with no other project VMs
running and other sessions holding new boots. Main `fb507f3` was merged
as `4f5984de636a193930f7da123a131a67915cffe9` before building and testing.
That commit is the measured runtime source; no runtime code changed
during the cohort. The imported harness and benchmark default permits
four project VMs. The timed cohort reserved all four shared slot locks
and used at most two VMs, only for guest-to-guest transfers.

All six benchmark configurations exited 0: both backends' full Ubuntu
suite, Windows boot suite, and Ubuntu guest-to-guest suite, with three
fresh-overlay repeats each. All 22 aggregate records and every sample
are appended to `bench/history/x86_64.jsonl`. No bar, repeat, readiness
phase, flush requirement, or workload was dropped or relaxed.

The following are **observed medians under interference**, not a new
quiet-host D28 verdict. Three meet their numerical thresholds. The
round 2 cohort remains the latest valid quiet-host comparison.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Required | Observed result |
|---|---|---:|---:|---:|---|---|
| G1 | Ubuntu boot (s) | 6.74 | 8.29 | 1.230 | <= 1.000 | FAIL |
| G2 | Windows boot (s) | 90.71 | 151.37 | 1.669 | <= 1.000 | FAIL |
| G3 | sequential read (MiB/s) | 16993.81 | 12824.73 | 0.755 | >= 0.800 | FAIL |
| G4 | sequential write (MiB/s) | 5159.52 | 3390.91 | 0.657 | >= 0.800 | FAIL |
| G5 | random read 4 KiB (IOPS) | 48261.50 | 64689.30 | 1.340 | >= 0.800 | PASS |
| G6 | random write 4 KiB (IOPS) | 31083.10 | 33009.30 | 1.062 | >= 0.800 | PASS |
| G7 | UDP TX (Mbit/s) | 38711.78 | 14207.56 | 0.367 | >= 0.800 | FAIL |
| G8 | UDP RX (Mbit/s) | 73887.71 | 66092.34 | 0.894 | >= 0.800 | PASS |
| G9 | guest TCP A to B (Mbit/s) | 26025.66 | 20705.88 | 0.796 | >= 0.800 | FAIL |
| G10 | guest TCP B to A (Mbit/s) | 26502.41 | 19761.46 | 0.746 | >= 0.800 | FAIL |

### Host observations and validity

The VM-count monitor observed only this cohort's VMs, at most two at
once. Every observed VM had memory.max 25769803776 and memory.swap.max
0. The governor was `performance`; single VMs used CPUs 16-23 and
pairs used 16-19 plus 20-23. Ubuntu remained 2 vCPUs/4 GiB and Windows
4 vCPUs/8 GiB, with the existing firmware, templates, and MTU 65520.

During vmlab-hv's Ubuntu samples, the one-minute host load reached
30.42. Live rustc, rust-lld, and clippy-driver processes were observed;
a saved process/cwd check identified builds in `/home/wil/dev/wcl` and
`/home/wil/dev/opticlang-wt`. They were not this worktree's processes
and were not stopped. A request to pause external builds and restart
the full cohort was sent while measurements continued; no answer had
arrived when the cohort finished. This invalidates a quiet-host claim,
not the recorded byte counters or timestamps. The data cannot establish
an isolated regression or gain from the net changes.

The low samples remain: vmlab-hv UDP TX was 22029.34, 10737.13,
14207.56 Mbit/s; its TCP B-to-A samples were 19761.46, 7816.78,
20344.47 Mbit/s. Windows readiness was 98.52, 151.37, 151.37 seconds.
A rerun needs other CPU-heavy builds and tests held, as well as new VM
boots. No selective rerun has replaced these measurements.

### Verification and execution records

`just ci::check` exited 0 on the merged source, inside the 24 GiB,
zero-swap scope with RUST_TEST_THREADS=1 and a 1800-second harness
process deadline. It took 1627.20 seconds and reported 896 passing
tests, zero failures, and three ignored tests. Formatting and workspace
clippy with `-D warnings` passed. CI finished before timed benchmarks
began; no acceptance/test VM overlapped a timed benchmark VM.

The ignored tests were the 1 GiB S2 throughput baseline, the 4 GiB
zero-fill diagnostic, and live PipeWire loopback. They were not run
explicitly in this window. Physical USB/VFIO opt-ins were unset, so
those real-device paths were not exercised. No host device was claimed,
rebound, or reset. All 83 benchmark unit tests also passed; their
existing mock ResourceWarnings remain.

Two setup failures are retained. An unsupported `--version` metadata
probe stopped the first runner before any VM launch. The next attempt
used retained paths longer than Unix sockets accept; all six benchmark
commands failed without aggregate records, then its full CI command
passed. The corrected cohort used a short `/tmp/vp3-final-*` root and
reran every benchmark configuration. Its command exit codes are all 0;
the earlier batch's nonzero status is not treated as success.

`bench/profiles/phase2-round3-final-20261101.json` contains the full bar
table, all 22 records, command statuses, logs, scope observations,
firmware/binary hashes, CI counts, and external-build evidence. It marks
`quiet_host_claim_supported: false`. Full runtime artifacts remain in
`/tmp/vp3-final-3oxwzw1w`; driver records are under
`.scratch/perf3/final-quiet.k6FYYL` and
`.scratch/perf3/final-quiet.fhsPvS`. All owned VMs and jobs were stopped
after recording results. No shared protocol, trait, or snapshot-format
signature changed. Quiet-host D28 validation, G7's regression commit,
and G10's cause remain unfinished; the thresholds stay unchanged.

## Round 3 development diagnostics

These are shared-host diagnostics on 2026-11-01, not a new D28 verdict.
The round 2 final cohort below remains the latest gate result. No gate
threshold, workload, CPU pin, template, cache policy, or readiness condition
has changed. A final quiet-host run requires the owner's bench window.

### G7 investigation

Historical binaries were built from isolated source trees and target
directories, with the same installed firmware and rebuilt iPXE ROM.
Each diagnostic boot used a fresh overlay and the normal Ubuntu profile:
2 vCPUs, 4 GiB, CPUs 16-23, MTU 65520, 65000-byte UDP payloads, and
12-second sends. All samples, delivery counters, load averages, binary
hashes, command statuses, and incomplete attempts are retained in
`bench/profiles/phase2-round3-20261101.json`.

The initial three-send medians were 28882.25 Mbit/s at `8bcbf1b`,
23083.97 at `fe0915b`, and 22586.79 at `6cceef4`. This initially placed
the loss before the phase-4a merge. Repeated five-send cohorts did not
support a reliable bisect: `8bcbf1b` fell to 22030.51, `6e1a493` ranged
from 18229.06 to 30122.47 across boots, and `6cceef4` reached 27932.50.
The adjacent `6e1a493` and `27a43c8` distributions also overlap. No
regression commit is assigned from these measurements.

Four network changes have independent correctness or allocation evidence.
The table records consecutive diagnostic cohorts, not isolated causal
estimates. Each before/after cell is the median of five sends in one boot.

| Change | Commit | Before / after UDP TX (Mbit/s) | Evidence |
|---|---|---|---|
| C11: avoid empty-queue interrupts | `ba17a4e` | 22281.22 / 23303.73 | Empty TX and unavailable RX-buffer regressions fail before and pass after |
| C12: offer indirect descriptors | `ccc4288` | 23303.73 / 23630.37 | A two-leg indirect TX chain produces the exact framed packet and used completion |
| C13: remove TX staging copies | `b3704f5` | 24618.83 / 23593.81 | A backpressured socket preserves every byte and unsent suffix across save/restore |
| C14: honor classic interrupt suppression | `4448937` | 22313.87 / 22758.85 | Suppressed TX/RX completions raised an IRQ before the fix; suppression and reenable now pass |

The sample ranges overlap, and C13's measured median decreased. None of
these cohorts proves an isolated throughput gain. In the C14 comparison,
QEMU's median was 28470.70 Mbit/s and the C14 binary's was 22758.85;
their shared-host ratio is not a final gate result. Delivery counters
validate the traffic rather than accepting guest send rates alone.

The device's previous notification helper did not implement
`VRING_AVAIL_F_NO_INTERRUPT` when EVENT_IDX was disabled. The fix observes
the driver's flag after used-ring publication and preserves EVENT_IDX
semantics. Empty drains no longer produce completion interrupts. The
socket worker still bounds queued output and retains unsent data; queue
reset, quiescence, framing, and snapshot layouts are unchanged.

### Boot investigation

An installed-firmware pair reached first kernel output at 1.70 seconds
under QEMU and 2.98 under the C14 binary. Agent readiness was 6.97 and
12.22 seconds. A separate diagnostic SeaBIOS build reached the boot
sector at 0.83 seconds under vmlab-hv and first kernel output at 2.80.
That debug firmware is not used for gate measurements.

A three-second host instruction sampler found repeated platform timer
updates and PCI/VGA synchronization in the boot path. Leaf instruction
samples are not wall-time measurements; unresolved samples remain in
the evidence. A targeted regression reproduced an unrelated MMIO read
waiting for the RTC lock. An experimental clock change avoided platform
timer synchronization for unrelated MMIO while retaining HPET and audio
updates, periodic timer service, and pause/resume behavior.

The installed-firmware comparison retained all three boots of each
configuration. Readiness was 12.22, 8.18, 8.17 seconds before the clock
change; 8.21, 8.30, 12.12 afterward; and 6.73, 6.80, 6.95 for QEMU.
The medians, 8.18 / 8.30 / 6.80 seconds, show no boot-time improvement
from this change and do not meet G1. Early auxiliary NIC-feature probes
matched no sysfs entry because they assumed a particular hexadecimal
width. Later probes compare numeric device IDs; an empty early field
does not mean no features were negotiated.

The experiment's UDP TX medians were 23695.32 before and 22686.61 Mbit/s
afterward; QEMU measured 31736.33 in that cohort. It is not retained:
neither timing diagnostic improved, and
there is no reason to change clock synchronization for this gate without
evidence of a gain. Its patch, tests, binary hash, and measurements remain
in the evidence as the `mmio` candidate. The final code keeps the existing
platform-clock behavior.

The Windows diagnostic retained two boots of each vmlab-hv candidate:
93.32 and 94.75 seconds with C14, 113.51 and 85.76 with the discarded
clock experiment. One QEMU boot took 167.57 seconds. These include the
unchanged specialize, OOBE, AppX-settle, and in-guest reboot sequence.
The small, noisy shared-host set is not a G2 median-of-three verdict.
No Windows-specific fix is claimed.

### Verification and remaining work

All 15 network tests, the release build, workspace clippy with
`-D warnings`, and 83 benchmark tests pass. The S1 NIC test passes through
traffic before snapshot, after save, and after restore with all four net
changes. All 23 active S2 integration tests pass; the ignored 1 GiB test
has not been rerun in round 3. Benchmark tests still print their existing
mock socket/file ResourceWarnings.

The MMIO lock regression fails before the candidate and passes afterward.
HPET progress, pause/resume freezing, and audio pre-access service pass
their positive controls. The S1 fixed-base clock and both clock snapshot
roundtrips pass with the candidate binary, including SMP CMOS restore.
Two later diagnostic sequences were rejected by the per-launch VM-count
guard before starting their next VM. Their nonzero command statuses and
completed samples are retained; the scratch runners now wait and retry
only this admission error. An initial historical build failed because a
shared target directory reused incompatible dependency metadata. Isolated
target directories fixed that build setup without changing historical
sources. The initial interrupt-mask test also needed a borrow-check
correction before its intended failing assertion could run.

Every diagnostic VM uses the shared host admission checks and a
24 GiB, zero-swap scope. Live cgroup limits and CPU topology are recorded.
CPUs 16-23 are one SMT sibling of each of eight physical cores, not eight
complete physical cores. The pinning is unchanged. No host PCI or USB
device was claimed, rebound, or reset. Real-device opt-in tests remain
for the owner to run with an explicitly designated device.

QEMU's userspace virtio-net, stream socket, block submission, and x86
firmware paths were read as behavioral references. The reference checkout
was read-only; no QEMU code was copied or translated. No shared trait,
control protocol, or snapshot-format signature has changed.

The G10 diagnostic waited for both host VM slots without reserving an
idle slot. It produced no transfer samples and was stopped before any
pair launch when preparing the required bench-window handoff. G10 has
not been remeasured or diagnosed in this round.

At the development handoff, G7's regression commit, G10's cause, the
final G1/G2/G7/G10 verdicts, and full CI were unresolved. The authorized
window above subsequently supplied full CI and all requested measurements.
Quiet-host validation remains unresolved because of external CPU work.

## Round 2 final run

The final window opened with the owner's confirmation that no other
project VM was running and every other session was holding new boots.
Local main `a0b61f4` (including phase 4a) was merged in `a99edda` before
building and measuring. The measured commit is `872fab1d147233a46851cdc817ee32510c64f6ab`.
The later dirty markers reflect appended benchmark history; runtime
sources did not change during this cohort.

Both backends completed three repeats of the full Ubuntu suite, Windows
boot suite, and guest-to-guest suite on 2026-11-01 (Australia/Brisbane).
The 22 aggregate records in `bench/history/x86_64.jsonl` retain every
sample and load average. `just bench-report x86_64` gives the same
verdicts. No bar, workload, or repeat was dropped or relaxed.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Required | Verdict |
|---|---|---|---|---|---|---|
| G1 | Ubuntu boot to agent ready (s) | 6.70 | 8.29 | 1.237 | <= 1.000 | FAILS |
| G2 | Windows boot to agent ready (s) | 108.11 | 109.68 | 1.015 | <= 1.000 | FAILS |
| G3 | sequential read (MiB/s) | 17579.97 | 15061.59 | 0.857 | >= 0.800 | meets |
| G4 | sequential write (MiB/s) | 815.32 | 1921.45 | 2.357 | >= 0.800 | meets |
| G5 | random read 4 KiB (IOPS) | 50640.50 | 67013.70 | 1.323 | >= 0.800 | meets |
| G6 | random write 4 KiB (IOPS) | 20150.00 | 19088.00 | 0.947 | >= 0.800 | meets |
| G7 | UDP TX (Mbit/s) | 41107.46 | 23870.07 | 0.581 | >= 0.800 | FAILS |
| G8 | UDP RX (Mbit/s) | 86108.37 | 75773.06 | 0.880 | >= 0.800 | meets |
| G9 | guest TCP A to B (Mbit/s) | 26011.68 | 20866.66 | 0.802 | >= 0.800 | meets |
| G10 | guest TCP B to A (Mbit/s) | 26610.76 | 20640.87 | 0.776 | >= 0.800 | FAILS |

All throughput workloads use Ubuntu 26.04.3; Windows is 19045.2006.9.
Idle CPU has no D28 bar: QEMU 1.13%, vmlab-hv 2.85% of one core.

G3 now meets the read bar. G1 improves from round 1's 12.23 seconds to
8.29 seconds, but still misses QEMU by 1.59 seconds. G2 fails its strict
median bar despite overlapping samples: QEMU 179.54, 108.11, 106.33;
vmlab-hv 195.25, 104.69, 109.68 seconds. This is not a statistically
established Windows regression relative to round 1's different baseline.

UDP TX samples are QEMU 37211.44, 41107.46, 43560.06 and vmlab-hv
29595.84, 23870.07, 23343.36 Mbit/s. G10's vmlab-hv samples are
11028.92, 20640.87, 21526.57 Mbit/s. The slow first sample is retained;
no cause is assigned without a profile. G9's 0.802x median leaves little
margin, and its first sample is below 80% of QEMU's median. These are
median verdicts, not claims that every repeat passes.

### Final-window environment

The host is helios, with 32 logical CPUs, the performance governor,
QEMU 11.1.2, and vmlab-hv 0.1.0. Single VMs use CPUs 16-23; pairs use
16-19 and 20-23. Ubuntu uses 2 vCPUs and 4 GiB; Windows uses 4 vCPUs
and 8 GiB. The Ubuntu NIC MTU is 65520. Templates and disk cache/flush
policy are unchanged from round 1.

Both host-wide slot locks were reserved for the benchmark cohort. No
project VM was present before any configuration, and the per-launch
count check remained enabled. Each VM ran in a scope with MemoryMax=24G
and MemorySwapMax=0. A live QEMU scope readback confirmed memory.max
25769803776 and memory.swap.max 0. The owner's Android emulator was
excluded and untouched. No physical PCI/USB device was claimed.

The one-minute load averages below include the measured VMs. Five- and
fifteen-minute averages, including decaying build load, remain in history.
No competing project VM was observed. A process CPU sample before the
fresh cohort found no heavy external CPU consumer.

| Run | Configuration | Repeat 1 | Repeat 2 | Repeat 3 |
|---|---|---|---|---|
| E1 | qemu Ubuntu | 1.55 to 0.97 | 0.97 to 0.72 | 0.72 to 4.29 |
| E2 | vmlab-hv Ubuntu | 4.29 to 1.85 | 1.85 to 0.73 | 0.73 to 0.90 |
| E3 | qemu Windows | 0.90 to 1.21 | 1.21 to 3.03 | 3.03 to 2.48 |
| E4 | vmlab-hv Windows | 2.48 to 1.22 | 1.22 to 2.07 | 2.07 to 2.39 |
| E5 | qemu guest pair | 2.39 to 2.82 | 2.82 to 2.94 | 2.94 to 3.14 |
| E6 | vmlab-hv guest pair | 3.14 to 6.42 | 6.42 to 5.36 | 5.36 to 5.09 |

An earlier attempt was rejected before appending results: QEMU remained
in QMP prelaunch despite the client reporting resume. The client had
treated socket chunks as complete replies and had not correlated command
IDs or checked errors. Commit `872fab1` reads buffered JSON lines,
matches IDs, rejects command errors, and verifies running state. Three
socketpair regressions fail before the fix and pass afterward. The
complete cohort above was restarted after that fix and a cooldown.

### Final-window verification

The timed suites and report completed with exit 0. `just ci::check`
also exited 0 with RUST_TEST_THREADS=1 inside the 24 GiB, zero-swap
scope: formatting, workspace clippy with -D warnings, and 890 reported
passing tests. This includes all 36 binary acceptance tests, all 379
active device tests, all 23 active S2 tests, all 14 core S1 tests, and
both filesystem restore tests. Ubuntu and Windows snapshot/reset,
Windows Server TPM, legacy-PC acceptance, and snapshot during fsync
writes passed. Guest-write, flush, and snapshot ordering were not
weakened for throughput.

CI normally ignores three tests. The 1 GiB S2 integrity/throughput test
and the zero-fill byte-verification diagnostic were run explicitly and
passed. The live PipeWire loopback test remains ignored. Physical USB
and VFIO testing was not enabled; their opt-in variables were unset.
Optional Sail-oracle feature tests were not enabled.

All 83 benchmark unit tests pass, including the QMP regressions and
main's additional coverage. Existing mock socket/file ResourceWarnings
remain. PIE and non-PIE sampler symbol-resolution self-tests pass with
compiler warnings treated as errors. No scoped todo!() or
unimplemented!() remains.

Herdr lost the outer completion marker for the main job and its wait
helper raised a TypeError. The runner independently saved each child
exit status and complete logs: CI exited 0 after 1597.48 seconds, and no
project VM remained. An auxiliary job had the same tracking issue;
those two explicit tests were rerun with normal job arguments and a
confirmed job exit 0. No test failure was hidden by the wrapper issue.

`bench/profiles/phase2-round2-final-20261101.json` records the command
statuses, timestamps, scope readback, firmware hashes, and auxiliary
checks. Full logs remain under `.scratch/perf2/final-quiet/`. QEMU C
source was not read. Only the authorized local-main merge was made;
there was no push or default change. G1, G2, G7, and G10 remain
unfinished, and work stops with evidence under the brief's failed-bar
condition instead of changing the bars.

## Round 2 development diagnostics (historical)

| Change | Commit | Diagnostic evidence |
|---|---|---|
| C5: iPXE POST timeout | `06b3f3c` | Agent readiness 12.31 to 8.19 s with the original VMM |
| C6: direct guest-buffer reads | `6e1a493` | Read-path series 9583.51 to 13998.15 MiB/s before the final table cache |
| C7: unchanged IRQ-level cache | `27a43c8` | Sampled Source share 6.08% to 1.26%; no isolated wall-rate claim |
| C8: test-hypervisor VM counting | `5cbbd87` | Reproduced omission; regression passes with the count fix |
| C9: direct sparse-zero fills | `422b67a` | Development zero probe 18052.31 to 47731.18 MiB/s; every byte verified |
| C10: correlated QMP resume | `872fab1` | Three regressions fail before and pass after; complete fresh cohort retained |

The initial round-2 CI run failed waiting 600 seconds for a shared VM slot
in Windows Server TPM acceptance. The VMM never launched for that test.
The swtpm helper's watchdog also expired during the wait. Ubuntu snapshot
and in-process reset acceptance, and Windows 10 snapshot and reset
acceptance, passed earlier in that run. Full CI must be rerun on the final
code; the failed run stopped before the remaining test crates.

At this development checkpoint, the quiet-host gate was still pending.
The round-1 table below is historical; the final result is above. No
diagnostic appends benchmark history or changes a D28 bar. Raw timings,
samples, and load averages are in
`bench/profiles/phase2-round2-20261031.json`.

The Ubuntu 26.04.3 template explicitly selects SeaBIOS. File observations
at 2 ms intervals give this process-spawn timeline. Process exec was
observed after 20-26 ms, and both backends acknowledged resume after
about 0.12 seconds.

| Diagnostic | First kernel message (s) | Agent ready (s) |
|---|---|---|
| QEMU baseline | 1.60 | 6.69 |
| vmlab-hv baseline | 6.40 | 12.31 |
| vmlab-hv with the iPXE POST delay removed | 2.51 | 8.19 |

The installed firmware emits no debugcon output. A separate SeaBIOS
rel-1.17.0 build with DEBUG_LEVEL=8 and DEBUG_IO enabled, used identically
on both backends, isolates a 3.92-second pause in the legacy iPXE POST
handler. Its default ROM_BANNER_TIMEOUT is four seconds. The firmware fix
sets that POST timeout to zero, while retaining the network-boot shell
timeout and EFI_DOWNGRADE_UX for OVMF's native network-boot entries.
It does not remove the NIC ROM or skip device initialization.

Restricting PCI quiescence did not improve agent readiness in its isolated
diagnostic, so that candidate was removed. Existing PAM, SMM, framebuffer,
and snapshot synchronization remains in place. A thin-LTO probe also
showed no material gain and did not change the release profile.

The block-read diagnostics moved from a 9583.51 MiB/s median to
13998.15 MiB/s before the final descriptor-table cache. Coalescing alone
reached about 10 GiB/s. Cached guest-region lookup reached about 11.6 GiB/s.
The next gain came from reading file extents directly into validated
guest buffers, with bounded descriptor walking and fixed-size preadv
batches. These are shared-host samples, not a G3 pass verdict.

The first 8 GiB contains 5.895 GiB of zero extents. Filling those validated
guest spans in place raises the last diagnostic read median to
14443.48 MiB/s. A separate 4 GiB guest-memory zero-fill probe measures
18052.31 MiB/s through the buffered target and 47731.18 MiB/s in place,
with every byte verified. Its test is normally ignored and was run
explicitly as `zero_fill_throughput` in the devices crate's release tests.

An IRQ-source cache avoids taking the shared hub lock for unchanged
levels. Aliases share the cache, and transitions still perform serialized
wired-OR delivery before publishing the cached level. Its regression
fails before the change and passes afterward. The source's sampled share
falls from 6.08% to 1.26%; differing host loads prevent an isolated
throughput claim.

The last development diagnostic boot takes 12.23 seconds at a starting
one-minute host load of 5.16. All samples are retained. This was not a
gate verdict. The final comparison above reruns both backends instead
of comparing development samples with round 1.

The disk seam adds `ReadTarget` and the default `DiskImage::read_into` method.
Raw images, qcow2, and `SharedDisk` implement or forward the direct path.
Buffered callers remain unchanged. S2 checks compare both read paths
across backing, compressed, zero, unaligned, and snapshot reads. The
normally ignored 1 GiB S2 test also passes. Guest writes and flush handling
are unchanged. Final-window verification is recorded above.

Final-code formatting, workspace clippy with `-D warnings`, all 24 active
block tests, and all 67 benchmark tests pass. The zero-fill diagnostic
passes when explicitly enabled. The benchmark tests still emit socket
and file ResourceWarnings from unchanged test code. ELF sampler
self-tests pass. No scoped `todo!()` or `unimplemented!()` remains.

The benchmark's process-count check now includes `vmlab-hv-test-*`, the
executables the Rust harness uses, matching its VM admission check. A
regression reproduces the previous omission while retaining the Android
emulator exclusion. Development diagnostics also held a shared test-slot
lock, checked host capacity, and launched every VM in the 24 GiB scope
with swap disabled. No host PCI or USB device was claimed. No merge,
push, or default change was performed.

## Round 1 full run

These are medians of three repeats per configuration on 2026-10-31, from
`bench/history/x86_64.jsonl`. Both backends ran the full Ubuntu suite,
Windows boot suite, and guest-to-guest suite through `just bench-run-kvm`
and `just bench-run-g2g`. `just bench-report x86_64` reports the same
verdicts. The bars remain boot <= QEMU and throughput >= 80% of QEMU.

| Bar | Guest | Workload | QEMU | vmlab-hv | hv/qemu | Bar | Verdict |
|---|---|---|---|---|---|---|---|
| G1 | ubuntu-26.04 | boot to agent ready (s) | 6.73 | 12.23 | 1.817 | <= 1.000 | FAILS |
| G2 | windows-10 | boot to agent ready (s) | 171.86 | 119.78 | 0.697 | <= 1.000 | meets |
| G3 | ubuntu-26.04 | sequential read (MiB/s) | 19541.84 | 9560.08 | 0.489 | >= 0.800 | FAILS |
| G4 | ubuntu-26.04 | sequential write (MiB/s) | 495.25 | 3654.79 | 7.380 | >= 0.800 | meets |
| G5 | ubuntu-26.04 | random read 4 KiB (IOPS) | 50386.00 | 62425.70 | 1.239 | >= 0.800 | meets |
| G6 | ubuntu-26.04 | random write 4 KiB (IOPS) | 10475.10 | 22621.30 | 2.160 | >= 0.800 | meets |
| G7 | ubuntu-26.04 | UDP TX (Mbit/s) | 36398.85 | 31162.01 | 0.856 | >= 0.800 | meets |
| G8 | ubuntu-26.04 | UDP RX (Mbit/s) | 86691.53 | 75662.23 | 0.873 | >= 0.800 | meets |
| G9 | ubuntu-26.04 | guest TCP A to B (Mbit/s) | 26312.27 | 21313.36 | 0.810 | >= 0.800 | meets |
| G10 | ubuntu-26.04 | guest TCP B to A (Mbit/s) | 26602.37 | 21445.48 | 0.806 | >= 0.800 | meets |

Idle CPU has no D28 bar: QEMU 1.00% and vmlab-hv 2.60% of one core.

Windows and writes still vary widely on a quiet host. Windows samples are
108.44, 188.60, 171.86 seconds for QEMU and 188.91, 103.39, 119.78 seconds
for vmlab-hv. Sequential writes range from 328.32 to 1736.24 MiB/s for
QEMU and 1684.48 to 3947.11 MiB/s for vmlab-hv. All samples are retained.

The guest-to-guest margins are small. G9 passes at 0.810x and G10 at
0.806x; one G10 sample is below 80% of QEMU's median. These are median
verdicts, not a claim that every sample passes.

### Host and reproducibility

The measured tree is `251187d41` on `wp/p2-perf`. The recorded dirty
marker comes from appended benchmark history; Rust sources did not change
during the final run. QEMU is 11.1.2 and vmlab-hv reports 0.1.0.

The host has 32 logical CPUs and the performance governor. Single VMs use
pin 16-23; the guest pair uses 16-19 and 20-23. Templates are Ubuntu
26.04.3 and Windows 19045.2006.9, standard variant. Ubuntu uses 2 vCPUs
and 4 GiB; Windows uses 4 vCPUs and 8 GiB. The Ubuntu NIC MTU is 65520.

Other project sessions deferred VM boots and heavy work for the final
window. Before each configuration, both existing test-slot locks were
held and no project VM was running. Each launch also checked the
host-wide two-VM limit. The guest pair used exactly two slots.
Every VM ran under `MemoryMax=24G` and `MemorySwapMax=0`. The owner's
Android emulator was excluded from the VM count and left untouched.

The table records one-minute load averages at each repeat's start and end.
The history retains all three load averages at both endpoints. The higher
guest-pair load includes the two measured VMs.

| Run | Configuration | Repeat 1 | Repeat 2 | Repeat 3 |
|---|---|---|---|---|
| E1 | qemu Ubuntu | 1.68 to 0.68 | 0.68 to 0.56 | 0.56 to 0.55 |
| E2 | vmlab-hv Ubuntu | 0.59 to 0.57 | 0.57 to 0.57 | 0.57 to 0.71 |
| E3 | qemu Windows | 0.71 to 1.59 | 1.59 to 1.06 | 1.06 to 1.08 |
| E4 | vmlab-hv Windows | 0.99 to 1.31 | 1.31 to 1.92 | 1.92 to 1.69 |
| E5 | qemu guest pair | 1.69 to 2.82 | 2.82 to 3.36 | 3.36 to 3.03 |
| E6 | vmlab-hv guest pair | 3.03 to 2.58 | 2.58 to 2.50 | 2.50 to 2.35 |

### QEMU cache mode

The benchmark uses vmlab's blockdev argv without a cache override.
Black-box QMP `query-block` on this argv reports
`writeback=true, direct=false, no-flush=false`. Thus the host page cache
is enabled and guest flush requests are honored. The disk crate retains
the same buffered-I/O and flush semantics. No unsafe cache mode was used.

## Round 1 changes

The following before/after figures are single-repeat diagnostics on a
shared host, not the final gate evidence above.

- C1. `4036c5c` skips unchanged chipset IRQ source levels before scanning
  the wired-OR inputs. Ubuntu boot moved from 16.22 to 12.19 seconds.
  The corrected block profile's `Source::set_level` share moved from
  40.13% to 10.56%. Shared-line delivery semantics are unchanged.
- C2. `8047d0d` advertises 254 data segments, indirect descriptors,
  and a 256-entry block queue. Installed QEMU device help confirms the
  same queue default and indirect-descriptor support. Sequential reads
  moved from 275.14 to 9846.36 MiB/s. Device and transport tests cover
  scatter-gather data, status, queue validation, snapshot, and reset.
- C3. `0b75d36` parses network payloads with slice copies, removes
  pending-frame clones, and avoids redundant TX copies. UDP RX moved
  from 11574.96 to 74564.50 Mbit/s. A new fragmented/jumbo/empty-frame
  test also reproduces and fixes the previous empty-frame panic.
- C4. `c2f6977` coalesces adjacent qcow2 mappings and uses positional
  I/O; `d47c0d9` reuses block request scratch. Neither demonstrated an
  isolated sequential-read throughput gain. The final read profile
  explains why further disk mapping work alone cannot close G3.

The first current-branch Windows diagnostic was already 119.42 seconds,
rather than the stale 307.12-second result. Reset-in-place predates this
work; it is not credited to these changes. Metadata caches also already
existed. No refcount batching or durability relaxation was added to
obtain the random-write result.

The benchmark client now allocates distinct channel IDs across execs and
reconnects. This fixes a reproduced QEMU run failure after readiness.
Admission failures preserve their original error and reap peers and
partially started pairs. Guest-to-guest labels now identify the actual
sender and receiver; historical records retain their old, swapped keys.
New records for both backends use the corrected labels.

## Round 1 failures and profiles (historical)

The sampler is committed as `bench/ipsample.c`. It resolves ELF load
segments and symbol sizes, supports dynamic DSO symbols, requests 1 kHz
sampling, and requests kernel/guest exclusion. PIE and non-PIE
symbol-resolution tests pass. Unresolved samples remain visible and are
not assigned to a nearby symbol.

The old sampler used mapping start minus file offset as the load bias.
This executable's text segment differs by 4096 bytes, which displaced
symbol attribution. In particular, the old `fire_config` diagnosis was
wrong. Source inspection confirms that config notifications occur on link
transitions, not per packet. Notification semantics were left intact.

### G1: Ubuntu boot

The final medians are 12.23 seconds versus 6.73 seconds, a 5.50-second
gap. Kernel serial output in the diagnostic boots reports a kvm-clock
startup offset near 6.0 seconds under vmlab-hv and 1.4 seconds under
QEMU. Much of the remaining gap precedes the kernel's normal boot log.

`bench/profiles/phase2-20261031-boot.txt` retains 1825 samples from an
approximately six-second window after control-socket resume. Of those,
496 are unresolved. The largest resolved leaves are `run_vcpu` at
12.60%, `should_run` at 5.10%, and chipset clock advancement at 4.05%.
Condition-variable waiting, notification, and lock-bucket operations are
also visible. This is distributed startup/exit and synchronization cost;
the leaf profile does not establish one sufficient, safe fix.

G1 remains unfinished. Removing mapping quiescence or timer
synchronization without replacement would risk reset, SMM, and snapshot
correctness. No such shortcut was made.

### G3: sequential block reads

The final median is 9560.08 MiB/s. The required rate is 15633.47 MiB/s,
80% of QEMU's 19541.84 MiB/s.

`bench/profiles/phase2-20261031-seqread.txt` retains 5584 samples while
repeating the same 8 GiB direct guest read for a long enough profile.
This diagnostic does not append gate results. There are 2598 unresolved
samples. Two guest-memory slice iterator functions account for 13.14%
and 10.01%; descriptor-chain iteration accounts for 3.99%, guest-memory
`read_slice` for 3.98%, and writer construction for 2.63%.
Qcow2 `read_bytes` accounts for 0.50%.

The remaining read path spends substantially more resolved samples on
descriptor/address handling than on qcow2 reads. G3 needs further work
on that request path. Coalescing mappings and reusing scratch did not
meet the bar. Guest-memory validation and snapshot tracking were not
bypassed to raise the number.

These are leaf-IP histograms, not call counts or wall-time breakdowns.
Unresolved code limits the attribution. No claim assigns those samples
to allocation, copying, or storage I/O without evidence.

## Round 1 correctness and verification (historical)

`just ci::check` passed with `RUST_TEST_THREADS=1`, inside the 24 GiB
scope. This includes firmware acceptance, S1 snapshot/reset and network
restore checks, and snapshot during fsync writes. All 23 active S2 tests
passed against qemu-img; the normally ignored 1 GiB S2 integrity and
throughput test was also run explicitly and passed. All 333 device unit
tests and all 67 benchmark tests passed.

Guest flushes still reach `DiskImage::flush` and durable host sync.
Metadata cache, refcount flush, and snapshot ordering were not weakened.
No shared Rust trait or control protocol type changed. No scoped
`todo!()` or `unimplemented!()` remains. No host PCI/USB binding or reset
operation was performed. Measurement VMs, peers, and profiler processes
were reaped. No merge, rebase, default flip, or push was performed.

Round 1 stopped with G1 and G3 measured and profiled. The round-2 result
above supersedes that verdict. D28 remains the gate for any future
default change.
