# N8: passed TAP queues and optional vhost-net backends

## Ownership and launch

The daemon creates TAPs using vmlab's existing helper. vmlab-hv receives owned
descriptors over its control socket with SCM_RIGHTS. It never opens /dev/net/tun,
creates an interface, or requires CAP_NET_ADMIN. The daemon retains the TapNic
handle so its existing XDP-map cleanup still follows machine lifetime.

A new machine attachment identifies a NIC that awaits passed descriptors and its
queue-pair count. Launch remains paused until every required NIC is installed.
An attach command names the configured NIC MAC and carries one TAP descriptor per
pair, followed by an optional matching vector of vhost-net descriptors. Reject
wrong counts, duplicate installation, unexpected descriptors and installation
after resume. Descriptor reception uses CLOEXEC and associates rights with the
exact request boundary. Existing protocol clients and raw stream attachments
remain compatible.

## Queue and datapath contract

One vhost-net file owns two backend rings, RX0 and TX1. Multiqueue therefore needs
one vhost file per virtio queue pair, not one file for the whole NIC. The daemon
creates IFF_MULTI_QUEUE and IFF_VNET_HDR TAP queues on the same interface and opens an equal number
of /dev/vhost-net files when available. If opening the optional kernel backend is
unavailable, userspace consumes the TAP queues directly. Configuration errors
after installing kernel backends are reported rather than silently downgraded.

For userspace TAP, reads and writes preserve packet boundaries and carry a
virtio-net header. Each virtio TX pair maps to its TAP queue. The worker drains
all TAP RX queues and steers packets to active guest pairs, including before MQ
activation. The control queue remains in
userspace. For vhost, configure owner, negotiated backend features, guest-memory
table, ring sizes, GPA ring addresses, ring bases and kick/call eventfds before
attaching each TAP queue. Global queue 2p/2p+1 maps to local ring 0/1 in backend p.
Never let the userspace worker read a TAP owned by vhost.

Probe TAP offload capabilities before guest negotiation, then disable receive
offloads until activation. At activation, enable only the guest's negotiated
receive offloads. VHOST_GET_FEATURES describes ring capabilities, not the TAP's
network offloads. Intersect ring features separately and leave the vhost-owned
header feature unset because the TAP already supplies its header. Modern headers
are 12 bytes even without mergeable buffers. Inactive kernel-backed TAP queues
are detached with TUNSETQUEUE until MQ enables their guest pair.

The AF_XDP host punt carries raw Ethernet. Guest TAP metadata is consumed by the
kernel rather than encoded as Ethernet bytes. TAPs and punt buffers are sized
for maximal packets. Software segmentation at raw switch egress preserves
compatibility with vnet stream ports.

Kernel-source review established that generic XDP preserves GSO and partial
checksums and bypasses normal transmit validation. A raw punt therefore loses
the metadata needed to finish those packets. AF_XDP attachments use TapRaw:
checksum, TSO and USO are not advertised to the guest, and TUN receive offloads
stay disabled from creation. MQ and vhost ring acceleration remain available.
This matches the existing raw QEMU TAP contract and makes both known redirects
and raw punts carry fully checksummed packets. Ordinary Tap attachments retain
offloads for a kernel network path that performs transmit validation. Stream
TSO/USO passthrough is unchanged. Supporting GSO over AF_XDP would require
metadata-aware punts and destination receive-capability handling.

## Lifecycle and snapshots

Reset and snapshot quiesce first detach vhost backends and read their ring bases.
That operation synchronizes kernel DMA before RAM is copied. Restore reinstalls
the saved bases. Resume reattaches the retained descriptors only after queue and
memory state is restored. Activation during restore must not restart kernel DMA
before resume_snapshot. Drain late call eventfds into the saved interrupt state.
Passed descriptors are process resources, not snapshot bytes. Closing the device stops its interrupt worker before dropping eventfds,
kernel handles and TAP queues.

## Verification

Exercise SCM_RIGHTS ownership and framing over real Unix socket pairs, including
fragmented requests, pipelined requests and unexpected/excess rights. Exercise
TAP packet semantics with packet sockets and kernel setup through a fake ioctl
seam, then prove actual attachment through S1 and an isolated Linux/Windows lab.
Run the kernel fastpath e2e scenario for both hypervisors. Record any unavailable
live prerequisite without claiming the corresponding check passed.

## Sources

- Linux TAP queue API: https://docs.kernel.org/networking/tuntap.html
- Linux vhost-net queue ownership: https://github.com/torvalds/linux/blob/master/drivers/vhost/net.c
- vhost ioctl contract: https://github.com/torvalds/linux/blob/master/include/uapi/linux/vhost.h
- virtio networking and MQ: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html
- Local integration audit: /home/wil/dev/vmlab-hypervisor-wt/net-gso-mq/.scratch/research/virtio-net-offload-multiqueue-and-tap-vhost-inte/20261103-164835-f7c9/findings/q02-device.md
- Generic XDP review: /home/wil/dev/vmlab-hypervisor-wt/net-gso-mq/.scratch/research/virtio-net-offload-multiqueue-and-tap-vhost-inte/20261103-164835-f7c9/findings/q04-kernel-xdp.md
