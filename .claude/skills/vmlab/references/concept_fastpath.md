# Network fast path (eBPF)

_Optional kernel-accelerated tiers above the userspace switch: AF_XDP (picked by auto) and sockmap (explicit-only); probed at startup, falling back to userspace._

The baseline network fabric is a userspace L2 switch — always available,
rootless, no capabilities. On supported Linux hosts two eBPF tiers can
accelerate it: **afxdp** (tap netdevs with a per-segment XDP program
forwarding known unicast in-kernel) and **sockmap** (sk_skb programs splicing
QEMU's existing stream sockets in-kernel; kernel ≥ 5.15). Both need `CAP_BPF`
+ `CAP_NET_ADMIN`, and afxdp uses `/dev/net/tun`. Selection is empirical: each
daemon probes the real mechanism once at startup and silently degrades to the
userspace switch on any failure, so labs behave identically either way.


```console
vmlab fastpath        # which tier is active, and why the others are not
```

The mode comes from the `fastpath` host-config knob, overridden by
`VMLAB_FASTPATH` (`auto` | `off` | `sockmap` | `afxdp`). `auto` considers only
afxdp — sockmap measured slower than the userspace switch on typical hosts, so
it is explicit-only. The web console's topbar shows the active tier as a
badge (`GET /api/fastpath`). In containers, grant `/dev/net/tun`, `CAP_BPF`
and `CAP_NET_ADMIN` to enable acceleration — see
[Run vmlab in a container](../references/process_run_in_container.md).


## Related

- [Networking model](../references/concept_networking.md)

- [Running vmlab in a container](../references/concept_containers.md)

- [Host config](../references/concept_host_config.md)

[← Back to SKILL.md](../SKILL.md)
