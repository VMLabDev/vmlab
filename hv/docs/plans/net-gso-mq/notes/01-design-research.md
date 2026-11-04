---
slug: net-gso-mq
kind: research
created: 2026-11-02
for: N6 (GSO/TSO passthrough), N7 (multi-queue)
---

# Design research: GSO/TSO passthrough (N6) and multi-queue (N7)

Read-only research answering the seven questions in the task brief. Every
code claim carries `path:line`; paths relative to `/home/wil/dev/vmlab` (the
fabric), `/home/wil/dev/vmlab-hypervisor` (this repo), or
`/home/wil/dev/qemu-vmlab` (QEMU reference). Driver and spec claims cite
upstream URLs. Two subagents gathered the services-stack and guest-driver
evidence; everything they report was spot-checked against the cited lines.

## 1. vmlab's userspace switch today

### Wire format and frame budget
- Every VM NIC attaches over a Unix **stream** socket speaking QEMU's
  `-netdev stream` framing: a 4-byte big-endian length prefix followed by one
  Ethernet frame (`src/net/framing.rs:1-14`). `MAX_FRAME_LEN = 65536 + 14 + 4
  = 65554` (`framing.rs:14`); a maximal GSO frame (14 + 65535 = 65549 bytes)
  **already fits** the codec, by design ("64 KiB of L3 payload + ethernet
  header + slack", `framing.rs:12-14`). Read rejects larger prefixes
  (`framing.rs:33-39`), write rejects larger frames (`framing.rs:44-53`).
- The switch itself is pure L2: `ingress()` parses only the 14-byte Ethernet
  header, drops runts (`frame.len() < ETH_HEADER_LEN`,
  `src/net/switch.rs:409-413`), learns the source MAC (`switch.rs:428-435`),
  and forwards known unicast or floods (`switch.rs:443-484`). **There is no
  maximum-frame check anywhere in the switch** — a 64 KiB frame crosses it
  untouched today. Per-port egress is a 512-frame mpsc channel; a full queue
  drops the frame (`PORT_QUEUE_CAPACITY`, `switch.rs:26`; `try_send` drop at
  `switch.rs:454-458, 472-476`).
- vmlab-hv's device agrees: `MAX_FRAME = 65536 + 14`
  (`crates/vmlab-hv-devices/src/virtio/net.rs:71`) and TX drops anything
  above it (`net.rs:749`).

### Ports and (non-)negotiation
- Ports are stream ports (hypervisor NICs) or in-process channel ports
  (services). `add_stream_port` starts deframing immediately on accept
  (`switch.rs:195, 248-263`); `listen_unix` binds one socket path per
  attachment with a fixed `PortClass::{Guest{isolated}, Service}`
  (`switch.rs:317-347`). The daemon wires per-NIC listeners as Guest ports
  (`src/labd/network.rs:90-93`) and services as channel ports
  (`src/net/gateway.rs:313`, `src/labd/netservices.rs:372-373`).
- **Nothing is negotiated on the wire.** No hello, version, or capability
  exchange exists anywhere in `src/net/` — `PortClass` isolation is the only
  per-port metadata (`switch.rs:45-56`), and the sockmap fast-path
  registration is a silent best-effort kernel attach, not a handshake
  (`switch.rs:203-213`). Any N6/N7 capability scheme is greenfield. But the
  daemon **owns both ends' launch**: it knows per machine whether the peer is
  QEMU or vmlab-hv (D27 `hypervisor` field) and creates the listener, so a
  per-port mode can also be set out-of-band at attach time. See §5 and Open
  Questions.

### Kernel fast paths (interaction with N6)
- Sockmap tier: the BPF framing state machine accepts exactly the same
  `MAX_FRAME_LEN = 65554` (`ebpf/fastpath-logic/src/lib.rs:26-33`) and only
  inspects the length prefix and Ethernet MACs
  (`ebpf/fastpath-sockmap/src/main.rs:128-172`), so whole 64 KiB frames pass
  in-kernel unchanged. Egress rides one atomic dgram `[4-byte BE len][frame]`
  (`src/net/fastpath/sockmap.rs:214-226`) — 64 KiB fits under the default
  `net.core.wmem_max` (~208 KiB), a host-dependent bound worth an explicit
  check. **Caveat:** registration happens "before any bytes flow"
  (`sockmap.rs:273-275`); a first-frame hello from the peer would break the
  "socket is still silent" assumption and needs rework or an opt-out for
  vnet-mode ports.
- afxdp tier: taps are created at segment MTU (+64 slack)
  (`src/net/fastpath/afxdp.rs:211, 295`) and the punt reader buffer is
  `mtu + 14 + 64` (`afxdp.rs:238`) — GSO frames larger than the segment MTU
  cannot cross the punt path. This tier replaces the stream netdev with a
  pre-opened tap fd for QEMU (`src/qemu/cmdline.rs:113-118`), where QEMU's
  tap backend *does* have vnet hdr, so guest offloads are already live there
  for QEMU; the machine description's `NicAttachment` has only `Stream`
  (`crates/vmlab-hv-proto/src/machine.rs:267-270`), so vmlab-hv does not ride
  this tier yet. Treat afxdp + vnet framing as a later compatibility item.

### Where a large segment would need segmenting; checksums
The services stack was scouted end to end (`src/net/frame.rs`, `nat/`,
`gateway.rs`, `dns.rs`, `dhcp.rs`, `router.rs`):

- **Checksums are never verified anywhere in production code.** Every
  `checksum_valid` call site is in tests. Ingress paths trust guest checksums
  structurally: `NatEngine::handle_frame` (`src/net/nat/mod.rs:182-202`),
  gateway (`src/net/gateway.rs:374-446`), DNS (`src/net/dns.rs:424-470`),
  DHCP (`src/net/dhcp.rs:157-184`), router (`src/net/router.rs:202-260`),
  vTCP (`src/net/nat/vtcp.rs:543+`). Consequence: **NEEDS_CSUM (partial
  checksum) passthrough is already safe through the whole fabric** — nothing
  on the host path consumes the checksum field except the rewrite points
  below.
- Checksums are **computed** only when the daemon builds frames from scratch
  (`ipv4_build` `frame.rs:363-364`, `udp_build` `frame.rs:446-451`,
  `tcp_build` `frame.rs:591-592`, `icmp_build` `frame.rs:655-656`), or fully
  recomputed on rewrite: the L3-rules DNAT rewrite (`src/net/rules.rs:488-508`)
  and the router's TTL decrement (`router.rs:324-332`). No incremental update
  anywhere. `l4_checksum` allocates a full segment copy and returns `None`
  above 65535 (`frame.rs:86-96`) — fine for per-segment use after
  segmentation (each piece ≤ MTU), unusable for summing a 64 KiB L4
  pseudo-packet in one go (it fits, barely: ≤65535, but at a full copy).
- **A 64 KiB TCP segment already survives ingress everywhere.** The parsers
  are zero-copy views bounded by `total_len`; vTCP accepts a 64 KiB segment
  whole (its fixed 0xFFFF window covers it; `vtcp.rs:71, 105-115, 613-616`)
  and never validates received segments against the MSS it advertised. The
  real >MTU break points are all **emit-side**:
  - vTCP clamps MSS to `max(mtu-40, 536)` and advertises it on every SYN
    (`vtcp.rs:336, 360, 383, 678-681, 734-737`); host reads are one MSS
    (`vtcp.rs:800`) — the daemon never emits a TCP segment larger than
    MTU−40. So the NAT path *terminates* large segments naturally: a guest
    TSO segment arrives whole, vTCP streams the payload to a host
    `TcpStream` (`vtcp.rs:212-215`), and the host stack segments. **No
    segmentation is needed for NAT TCP**; only the guest→host checksum
    story changes (vTCP ignores it, so nothing is needed there either).
  - UDP NAT forwards payload datagrams to per-flow host sockets
    (`src/net/nat/udp.rs:31-68`); replies rebuild via `udp_build`, which
    returns `None` (silent drop) above 16-bit lengths (`nat/mod.rs:421-425`).
    USO-sized guest datagrams up to 65535 pass; larger cannot exist on the
    wire anyway (IP total length is 16-bit).
  - Router PMTU: a routed packet larger than the peer leg's MTU is dropped,
    with ICMP frag-needed only when DF is set — "the fabric does not
    fragment" (`router.rs:232-247`). N6 must segment before inter-segment
    routing or keep TSO off that path.
  - DHCP advertises the segment MTU via option 26 when ≠1500
    (`dhcp.rs:394-395`) — the only MTU signaling toward guests today. DNS
    forwards raw query bytes and rebuilds replies (`dns.rs:516-525,
    358-372`); both are small-frame protocols, unaffected by GSO.
  - The rules hook's `rewrite_packet` recomputes full L4 checksums
    (`rules.rs:488-499`). On a partial-checksum TSO frame this computes a
    checksum as if the frame were one giant packet — harmless while the
    frame is only ever consumed by vTCP (payload only), wrong if the same
    frame is re-emitted toward a raw peer. Rule: **compute checksums at the
    segmentation boundary only** (see §2, §7).

### What a per-frame virtio-net header would touch (vmlab side)
`framing.rs` (codec), `switch.rs` (`add_stream_port` reader/writer, ingress
dispatch), the sockmap BPF framing state machine and its dgram egress
(`ebpf/fastpath-logic`, `sockmap.rs:214-226`), the probe
(`src/net/fastpath/probe.rs:224-227`), `bench/netpeer.c` (the D28 peer), and
a new segmentation service at raw-mode port egress. Channel-port services
would keep receiving bare Ethernet frames — the header belongs to the
port-socket layer, not the switch fabric, if we scope it that way (recommended;
see §5).

## 2. QEMU's stream netdev and virtio-net offloads

- `net_stream_info` registers only `receive` and `cleanup` — no
  `has_vnet_hdr`, no `set_vnet_hdr_len` (`net/stream.c:98-103`). Therefore
  `qemu_has_vnet_hdr()` is false for a stream peer (`net/net.c:535-542`), and
  the socket read state is initialized with `vnet_hdr=false`
  (`net/stream_data.c:120, 174`; the parameter at `net/net.c:2123-2127`).
  The stream write path is a plain `[htonl(size)][frame]` writev
  (`net/stream_data.c:31-68`). **A stream netdev cannot carry a vnet header,
  full stop** (the intake's "check" is settled).
- Consequence in the device: `peer_test_vnet_hdr` (`hw/net/virtio-net.c:625-633`,
  called at realize `:4147-4151`) sets `host_hdr_len = 0`, and
  `virtio_net_get_features` **strips every offload bit from the offer** when
  the peer lacks vnet hdr: CSUM, HOST_TSO4/6, HOST_ECN, GUEST_CSUM,
  GUEST_TSO4/6, GUEST_ECN, HOST_USO, GUEST_USO4/6, tunnel-GSO, HASH_REPORT
  (`virtio-net.c:3163-3186`) plus GUEST/HOST_UFO (`:3188-3191`). There is no
  software-segmentation fallback for virtio-net on this path — the guest
  simply never negotiates the features, so every QEMU guest on vmlab's switch
  runs **software checksums and MTU-sized frames today** (the device property
  defaults are all on, `virtio-net.c:4334-4403`; the backend strips them).
  The bench record confirms it from the measurement side: "Guest checksum
  and TSO offloads remain disabled on both stream backends"
  (`bench/profiles/phase2-round5-followup-20261102.json`, findings list).
- RX from the socket: QEMU prepends a zeroed header with `GSO_NONE`
  (`receive_header`, `virtio-net.c:1716-1736`) — flags 0, so the guest stack
  checksums everything in software, exactly like vmlab-hv's device does
  (§3).
- MQ in QEMU: the `mq`/`rss`/`hash` properties default **off**
  (`virtio-net.c:4374-4378`) and vmlab's argv passes none
  (`src/qemu/cmdline.rs:758-762`: `-device virtio-net-pci,netdev=netN,mac=…`
  plus optional `host_mtu=`), so the QEMU reference is single queue pair
  regardless.
- What a QEMU peer therefore needs from N6: **the switch must translate**.
  Toward a QEMU port, a TSO/USO frame with a vnet header must be segmented
  into MTU-sized frames with fully computed checksums (QEMU will not fix
  them, and its guest verifies in software). Toward the switch from QEMU,
  nothing changes: bare frames, already ≤MTU. QEMU's queue layout when MQ is
  ever enabled is `rq0,sq0,rq1,sq1,…,ctrlvq-last` (`virtio_net_add_queue`
  `virtio-net.c:3025-3050`, `virtio_net_del_queue :3060-3069`, ctrl vq of
  size 64 added last `:3103`), matching the spec layout in §4 — useful if
  vmlab ever launches QEMU with `mq=on` for parity testing.

## 3. vmlab-hv's virtio-net today, and what N6/N7 change

State of `crates/vmlab-hv-devices/src/virtio/net.rs`:

- **Features offered**: `MAC | STATUS | MRG_RXBUF | INDIRECT_DESC | EVENT_IDX
  | VERSION_1`, plus `MTU` when mtu ≠ 1500 (`net.rs:833-840`). No CSUM, no
  TSO/USO, no MQ, **no CTRL_VQ** — which N7 makes a hard prerequisite (the
  spec requires CTRL_VQ for MQ; §4). `max_virtqueue_pairs` in config space is
  hard-coded to 1 (`net.rs:854`); queue count is fixed at 2
  (`net.rs:825-827, 873-878`).
- **Framing to the socket**: TX strips the 10/12-byte `virtio_net_hdr`
  without inspecting it (`hdr_len` by negotiation, `net.rs:129-137`; strip
  and frame in `process_tx`, `net.rs:706-790`), stages framed bytes up to
  1 MiB (`MAX_TX_STAGE`, `net.rs:73-75`) and flushes under POLLOUT
  (`flush_tx`, `net.rs:794-816`). RX deframes into `rx_pending`
  (`read_frames`, `net.rs:441-502`) and places frames with a **zeroed
  header** (`flags = 0`, i.e. "checksum not validated"), mergeable or not
  (`place_rx_frames` `net.rs:606-704`, `receive_merged` `net.rs:540-588`,
  `num_buffers` handled correctly). One worker thread per NIC runs a
  `poll(2)` loop over {wake pipe, switch socket} (`run`, `net.rs:231-365`);
  the vCPU path only sends wakeups. Link state, reconnect with 1 s retry,
  and config-change IRQs are handled (`net.rs:394-432`).
- **N6 changes**:
  1. Offer `CSUM | GUEST_CSUM | HOST_TSO4/6 | GUEST_TSO4/6 | (HOST_USO,
     GUEST_USO4/6)`. TX: stop discarding the header — parse
     `flags/gso_type/gso_size/csum_start/csum_offset`, and either pass the
     header through to the switch (vnet-mode port) or segment+checksum
     locally. RX: fill a real header — mark `DATA_VALID` after computing the
     checksum during the existing copy (honest: the device *has* validated),
     or pass through a peer's header on g2g, and emit GSO-sized segments to
     guests that negotiated GUEST_TSO/USO.
  2. The header math must respect `hdr_len` per negotiated feature set
     (`net.rs:129-137` already parameterizes 10 vs 12).
  3. Queue size 256 (`net.rs:63`) and MRG_RXBUF chains already support
     64 KiB packets; `receive_merged` handles multi-buffer large frames
     (`net.rs:540-588`). Non-mergeable guests get a single-chain guarantee
     (`place_rx_frames` `net.rs:654-680`) — a 64 KiB RX segment then needs
     one ≥64 KiB buffer from the driver (Linux big-packets mode does this;
     netkvm's buffer sizing is driver-side, §4).
- **N7 changes**: `queue_max_sizes`/`activate`/`reset` all assume 2 queues
  (`net.rs:825-827, 873-878, 901-911`); the `Shared.queues` vector, the
  worker's single RX/TX processing functions (`place_rx_frames` takes RXQ,
  `process_tx` takes TXQ constants `net.rs:67-69`), and `queue_notify`
  (wakes on index ≤ TXQ only, `net.rs:895-899`) must generalize to N pairs.
  A control vq handler (VQ_PAIRS_SET, RSS_CONFIG/HASH_CONFIG if offered) and
  per-queue `notify_queue` already exist as plumbing
  (`irq.notify_queue(queue)` at `net.rs:698, 775`).

## 4. Guest drivers and the spec (what they negotiate by default)

Scout-verified against torvalds/linux master (2026-11), virtio-win master,
and the VIRTIO 1.2 CS01 / 1.3 sources. Full citations in the scout report;
essentials:

- **Linux virtio_net** offers the full modern set by default
  (`drivers/net/virtio_net.c` `features[]`): CSUM, GUEST_CSUM, HOST/GUEST
  TSO4/6+ECN, HOST_USO, GUEST_USO4/6, MRG_RXBUF, STATUS, CTRL_VQ, CTRL_RX,
  CTRL_VLAN, GUEST_ANNOUNCE, MQ, CTRL_MAC_ADDR, CTRL_GUEST_OFFLOADS, RSS,
  HASH_REPORT, MTU, SPEED_DUPLEX. Module params `csum=true`, `gso=true` —
  TSO is used whenever offered. Header semantics
  (`include/linux/virtio_net.h`): RX `NEEDS_CSUM` → CHECKSUM_PARTIAL (guest
  finishes it), `DATA_VALID` → CHECKSUM_UNNECESSARY (guest skips
  verification), neither → software verify. Without HOST_TSO the stack
  segments in software; without GUEST_TSO no GRO_HW. `RXCSUM` is set
  unconditionally but only *means* something with GUEST_CSUM.
  - **Queues**: `max_queue_pairs = 1` unless MQ or RSS is negotiated;
    `curr_queue_pairs = min(max_virtqueue_pairs, num_online_cpus())` —
    "Enable multiqueue by default". vq layout `rxq2vq(i)=2i, txq2vq(i)=2i+1`,
    ctrl last. With RSS the driver programs all supported hash types, a
    random Toeplitz key, round-robin indirection; with MQ only, the spec's
    automatic steering applies. ethtool can resize channels live.
  - Missing CTRL_VQ makes the driver reject MQ/RSS/CTRL_RX/etc. entirely
    (`virtnet_validate` fails the device).
- **Windows netkvm** (virtio-win master): registry defaults enable every
  offload — `*IPChecksumOffloadIPv4=3`, `*TCP/UDPChecksumOffload*=3`,
  `*LsoV1/V2IPv4/6=1`, `*RSS=1`, `*NumRssQueues=16`, `*RscIPv4/6=1`,
  `*UsoIPv4/6=1` (`ParaNdis_Common.cpp` `defaultConfiguration`). TX
  checksum/LSO/USO are kept only if CSUM / HOST_TSO4/6 / HOST_USO are acked;
  RX RSC is implemented **on top of GUEST_TSO4/6** (+RSC_EXT), and GUEST_TSO
  is refused without GUEST_CSUM. RX: `DATA_VALID` → trusted; `NEEDS_CSUM` →
  driver computes; neither → software verify per NDIS flags. MQ:
  `nHardwareQueues = max_virtqueue_pairs` iff CTRL_VQ+MQ, queue pairs =
  min(that, CPU count, (MSI-X vectors−1)/2) — **no MSI-X ⇒ 1 queue pair**.
  With device RSS it sends RSS_CONFIG (requires key ≥ 40 bytes, indirection
  table ≥ 128 entries, IPv4 hash support, else falls back); without device
  RSS it still advertises NDIS RSS but steers in driver software on a single
  queue. Caveat: netkvm master defaults `MergeableBuffers=0` (does not ack
  MRG_RXBUF; released builds vary) — vmlab-hv's non-mergeable RX path
  (`net.rs:654-680`) must stay correct, and 64 KiB RX segments need the
  driver's own big buffers. USO needs NDIS 6.83 (Win10 1903+).
- **Spec (VIRTIO 1.2 §5.1, 1.3 unchanged here)**: TX with `gso_type ≠ NONE`
  requires NEEDS_CSUM + `gso_size`; the packet carries only the
  pseudo-header checksum. RX GSO frames MUST carry NEEDS_CSUM + gso_size;
  `DATA_VALID` asserts the device validated. Every GUEST_TSO/USO requires
  GUEST_CSUM; every HOST_TSO/USO requires CSUM; MQ/RSS/HASH_REPORT/CTRL_RX
  require CTRL_VQ; RSS and HASH_REPORT are not mutually exclusive. MQ layout
  `receiveq1, transmitq1, …, controlq` with `max_virtqueue_pairs` in config
  iff MQ or RSS offered; only queue pair 1 is used until
  VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET (u16 pairs) or RSS_CONFIG; after reset the
  device MUST receive only on receiveq1; MQ-only devices SHOULD steer
  receive after transmit on the matching queue (automatic steering).
  RSS_CONFIG carries hash_types, indirection table (power-of-two entries),
  unclassified_queue, max_tx_vq, key; config space must expose
  `rss_max_key_size ≥ 40`, `rss_max_indirection_table_length ≥ 128`,
  `supported_hash_types`. Canonical: virtio-v1.2-cs01 and virtio-v1.3 HTML at
  docs.oasis-open.org; sources at github.com/oasis-tcs/virtio-spec.

## 5. N7 design options

**O1 — one stream connection per queue pair.** Each pair gets its own
socket + worker thread; natural per-queue parallelism, per-queue backpressure,
and affinity. But the switch models a connection as a *port*: N connections
from one NIC would be N ports sharing one MAC, and MAC learning flaps between
them (`switch.rs:428-435` relearns on every ingress), so today replies would
land on whichever queue last transmitted — actually compatible with automatic
steering by luck, but broadcast/flood would duplicate to all N ports and
isolation/quese counters fragment. It needs a first-class "port group" concept
in the switch. The proto would grow `sockets: Vec<PathBuf>` per NIC
(`machine.rs:267-270` today: one).

**O2 — one connection, queue-id multiplexed.** Extend the N6 vnet framing
with a small per-frame record (e.g. `[u32 BE length][u8 queue][vnet hdr +
frame]`, or fold the queue into a new header). One port per NIC keeps MAC
learning, isolation, flooding, and sockmap registration intact; the switch
stays L2-dumb and vmlab-hv steers RX itself. Cost: the single socket is a
shared serialization point for all queues' socket I/O — but each queue pair's
*worker thread* still runs its own ring processing; the socket write path can
be sharded by writev batching per queue with a small lock, and the 1 MiB
staging bound (`net.rs:73`) becomes per-queue.

**Recommendation: O2**, riding the N6 framing change (one wire-format
revision instead of two), with one worker thread per queue pair and a shared
socket writer. O1 only if profiling shows the shared socket write lock
matters — it is measurable later without protocol change (the queue byte is
already there).

**RX steering without RSS**: MQ-only automatic steering (hash the flow, steer
to the queue the flow last transmitted on — a small Toeplitz or even
src/dst-hash map per port in vmlab-hv) satisfies the spec and both drivers;
Linux also does driver-side TX queue selection by CPU. **Offering RSS** buys
Windows real RSS and Linux explicit tables, but adds `rss_max_key_size /
indirection_table_length / supported_hash_types` config fields, RSS_CONFIG
command handling, and Toeplitz in the device. Netkvm falls back to software
RSS on one queue without it, which forfeits most of N7's Windows value, so
RSS should be phase 2 of N7. **Affinity**: workers per queue pair, each
pinned near the corresponding vCPU's housekeeping CPU if the VMM already
assigns one (the bench pins `16-23` for 2-vCPU guests,
`bench/history/x86_64.jsonl` env). IRQ affinity hints are the guest driver's
job (Linux `virtnet_set_affinity`).

**Switch must do (N7)**: nothing for O2 beyond the framing extension, since
steering is device-side. For O1 it needs port groups + per-queue MAC
handling.

## 6. Snapshot and reset implications

- **N6**: the `VLNET` v1 section (`net.rs:916-1055`) stores raw framed bytes
  (`tx_out`), partial RX read state, raw pending frames, and 2 queue states.
  Frames saved in vnet mode embed headers, so the section needs a version
  bump (v2) recording the port mode; restore must reconcile the saved mode
  with the live connection's negotiated mode (the socket survives snapshots
  and resets, `net.rs:909-910`, so the mode is connection state, not guest
  state). Guest `reset()` zeroes `negotiated` and drops queues
  (`net.rs:901-911`) — in vnet mode the device must then synthesize zero
  headers on RX (like QEMU's `receive_header` else-branch,
  `virtio-net.c:1729-1735`) and parse TX headers as absent until features
  return; simplest is to fix the *wire* header length at 12 bytes for vnet
  mode regardless of guest negotiation.
- **N7**: the section already stores a queue count with per-queue states
  (`net.rs:972-981`) but restore enforces exactly 2 (`net.rs:1017-1022`) —
  generalize to the negotiated pair count and additionally persist
  `curr_queue_pairs` (VQ_PAIRS_SET state) and, if RSS lands, the key and
  indirection table. Reset must collapse to base queues per the spec
  (receive only on receiveq1 after reset). Both D32 (same-major restore) and
  the in-place restore flow (`net.rs:142-145`) are preserved by versioning
  once, in the same bump as N6 if they land together.

## 7. Expected gains per D28 bar, order, and first steps

Ground truth for the bars: `bench/PHASE2-GATE.md:43-47` and
`phase2-round5-final-20261102.json:157-283` — G7 UDP TX 0.63x (24.5 vs 38.8
Gbit/s), G8 UDP RX 0.95x, G9 TCP A→B 0.81x, G10 TCP B→A 0.80x. **The bench
forces `host_mtu=65520` on both backends** (`bench/README.md:90-92`) —
frames are already ~64 KiB on the bars. The intake's "40x more frames" cost
is real but lives at the product-default MTU 1500, which no current bar
measures. Estimates:

- **G7 UDP TX (0.63x)**: at 65 500-byte datagrams one frame is one datagram;
  N6 does not change frame count. The gap is vmlab-hv's per-frame host path
  (copy into `tx_out` staging + framing-vec allocations) versus QEMU's
  zero-copy writev (`net/stream_data.c:31-68`); that is batching/copy work,
  not offload work. N6's checksum effect: today the *guest* fully
  checksums each datagram; with CSUM+GUEST_CSUM the guest stops and (since
  nothing in the fabric verifies, §1) nobody starts — freeing guest vCPU for
  the pump. Estimate **+0-15%**, honest central ~5%. If hv must complete
  checksums for a verifying peer it would *lose* here — keep completion
  lazy (only at segmentation, and only toward QEMU ports).
- **G8 UDP RX (0.95x)**: the peer's UDP checksums are almost certainly 0
  (opt-out) or the guest could not sustain 82 Gbit/s verifying; GUEST_CSUM
  changes little. Estimate **≈0**.
- **G9/G10 TCP g2g (0.81x/0.80x)**: both guests software-checksum every
  64 KiB segment (TX full checksum + RX verify ≈ 2 passes over ~2.6 GB/s).
  N6 passthrough with DATA_VALID-after-compute in hv removes both guest
  passes at the cost of one host pass folded into an existing copy.
  Estimate **+5-20%**, likely enough to clear 0.85-0.90x. N7 adds little:
  single flow, single stream — maybe +0-10% from kick/IRQ spreading across
  the 2 vCPUs.
- **At default MTU 1500 (unmeasured today)**: N6 is the difference between
  ~43 frames and 1 frame per 64 KiB of bulk TCP — expected multiples (2-10x)
  for both backends, and it is what lets the bench eventually drop the
  `host_mtu=65520` forced deviation and measure the configuration users
  actually run. **N7's real payoff (parallel flows, multi-vCPU guests,
  Windows RSS) is not covered by any current bar** — add a multi-flow or
  n-stream g2g workload before building it, or its gain is invisible to the
  gate.

**Recommended order: N6 first, N7 second.** N6's framing revision is the
substrate N7's queue-id rides on (§5), and N6 is what removes the 40x frame
tax at the default MTU. N7 needs CTRL_VQ — the larger, riskier build — and
has no bar that proves it yet.

- **N6 minimal first step** (device-only, no fabric change, one PR): offer
  `CSUM | GUEST_CSUM` in `net.rs:833-840`; on TX complete the checksum
  during the existing `tx_out` copy (pseudo-header is already in the frame);
  on RX compute the checksum during the existing `place_rx_frames` copy and
  set `VIRTIO_NET_HDR_F_DATA_VALID`. Frames on the socket stay bare
  Ethernet; old peers unaffected; guest software-checksum work disappears in
  both directions. Verify with guest `ethtool -S`/`ethtool -k` and the D28
  net bars. Then: vnet framing negotiation per port + TSO/USO passthrough
  g2g + segmentation toward QEMU ports (switch-side, `gso_size`-sized, full
  checksum per segment, IP id increment per segment).
- **N7 minimal first step**: CTRL_VQ + MQ (offer `max_virtqueue_pairs =
  vCPUs`, honor VQ_PAIRS_SET, RX automatic steering by flow hash), O2
  framing with the queue byte, one worker per pair. RSS_CONFIG after that,
  driven by a Windows measurement.

## Open questions for the owner

1. **Negotiation locus**: is out-of-band port-mode configuration acceptable
   (the daemon knows each port's hypervisor and sets vnet mode at attach),
   or must the wire protocol self-describe (client hello at connect)? The
   latter forces a decision on hello-vs-QEMU-compatibility (a reserved
   EtherType frame QEMU would forward to its guest, or a length-prefix
   sentinel QEMU would reject) and reworks the sockmap "silent socket"
   registration assumption (`sockmap.rs:273-275`).
2. **Bar coverage**: may the bench add (a) a default-MTU (1500) net workload
   and (b) a multi-flow/parallel g2g workload, so N6's frame-count win and
   N7's parallelism win are visible to D28? Without them N7 cannot pass a
   bar it moves.
3. **USO scope**: support UDP L4 GSO (HOST/GUEST_USO4/6) in the first N6
   cut, or TCP-only first? Linux uses USO when offered; netkvm needs
   Win10 1903+; the switch-side segmenter needs a UDP variant either way.
4. **MRG_RXBUF on Windows**: netkvm master defaults `MergeableBuffers=0` —
   do we care to confirm the shipped virtio-win build's default before
   sizing RX buffers for 64 KiB non-mergeable segments?
5. **vTCP large RX**: should the daemon's TCP emit also grow (GSO-sized
   segments with NEEDS_CSUM toward offload guests, window and clamp raised),
   or is guest RX left MSS-clamped in the first cut? Left-clamped is the
   recommended first step; guest→host and g2g still win.
6. **afxdp tier**: when vmlab-hv grows a tap attachment, must the vnet
   framing and the afxdp tier interoperate (tap MTU and punt buffer sized
   for GSO), or is the tier QEMU-only until then?
