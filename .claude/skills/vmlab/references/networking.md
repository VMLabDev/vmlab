# Networking

The lab daemon contains a complete userspace network stack: switching, DHCP,
DNS, NAT, port forwarding, and traffic filtering and redirection. It needs no
root, no tap devices, no bridges and no host network configuration, which is
what makes WSL 2 a first-class host.

## Segments and the switch

A **segment** is a virtual L2 switch. Every machine NIC on it attaches as a
port over a QEMU stream-socket netdev, which is a unix socket in the runtime
directory, and the daemon does MAC-learning frame forwarding between the ports
of one segment. Because the daemon sees every frame, DHCP, DNS, routing,
filtering and redirection are implemented as participants on the switch rather
than as external services.

Segments are lab-scoped by default: `corp` in one lab and `corp` in another are
different wires. A machine reaches a segment only through a `nic` block, and a
machine with none is air-gapped. Any NIC may set `isolated = true`, after which
the switch drops frames between that port and other guests, in the
private-VLAN style. An isolated NIC still reaches the segment's gateway
services, its NAT, its forwards and its shares, but never a neighbour.

Throughput is a stated non-goal. The fabric proxies every flow in process, so
it will not approach tap or bridge speeds; the eBPF fast path is the optional
remedy.

## The connectivity ladder

Connectivity is climbed by declaration, one rung at a time:

1. **No `nic` block.** No network hardware at all. The agent still works, over
   virtio-serial.
2. **`nic { nat = true }`.** The NIC joins the lab's built-in NAT segment:
   DHCP, DNS and internet egress are on, and nothing needs declaring. It is one
   shared segment per lab, so machines using the shorthand can reach each other
   unless a NIC also sets `isolated = true`.
3. **A declared segment.** `nic { segment = "corp" }` on a segment you
   declared, with the subnet, DNS, NAT, routes and rules you choose. A declared
   segment is isolated from the internet unless it sets `nat = true`.

The `nic {}` block itself is documented in vm.md.

## Addressing and DHCP

DHCP is on for every segment unless it sets `dhcp = false`. A segment with no
`subnet` is allocated a `/24` from a host-wide pool, `10.213.0.0/16` by default
and overridable with `subnet_pool` in the host configuration (host-profiles.md).
A declared subnet is honoured. The daemon holds the subnet's first usable
address as the gateway and serves leases from there.

A NIC with a static `ip` becomes a **DHCP reservation** keyed on the NIC's MAC,
which vmlab generates once and persists in `.vmlab/` unless the block fixes a
`mac`. The guest keeps plain DHCP configuration and still lands on a
deterministic address, and a static IP may sit outside the dynamic pool. The
lease carries the gateway, the DNS server, the domain suffix and, when the
segment declares `route` blocks, classless static routes as DHCP option 121.

Turn DHCP off for a segment where a lab machine should own addressing: a domain
controller, a pfSense VM, a dnsmasq experiment. A NIC may also declare
`gateway = true` to take over the segment's router role. It must own the
subnet's first usable address, and the daemon moves its own DHCP, DNS and SMB
services to another free address on the segment.

## DNS

The daemon answers DNS on each segment's gateway address. Every guest NIC
auto-registers as `<machine>.<lab>.<suffix>`, with a short `<machine>.<suffix>`
alias where that is unambiguous within the segment. The suffix defaults to
`vmlab.internal`, chosen to avoid the `.local` mDNS collision, and is
overridable with `dns_suffix` in the host configuration. Containers register
the same way as VMs.

Static entries are `record` blocks, per segment or lab-wide, with wildcards
allowed in the name. Queries nothing in the lab answers are forwarded upstream,
to the host's own resolver unless `dns_upstream` names another, so a guest on a
NAT segment gets working public DNS for free. A segment's `dns` child hands out
a different server over DHCP, which an Active Directory lab needs so the DC
owns resolution, or sets `enabled = false` to suppress the DHCP option
entirely. `vmlab dns` prints the zones the lab's segments currently serve.

## Internet egress

Egress is a userspace NAT attached as a port on the switch. Guest TCP and UDP
flows are terminated in the daemon and proxied over ordinary host sockets, so
no privilege is needed anywhere. ICMP echo is degraded by design: unprivileged
ICMP sockets are unavailable, so reachability is probed with the system `ping`
binary and a reply synthesised from the result. A guest's `ping` tells you
whether a host is reachable, and nothing about round-trip time.

Because the NAT terminates flows on the host, anything a guest addresses
off-segment reaches the host's own address space. That is how a guest reaches a
host-side service such as a package mirror or a licence server, and it is the
answer to the reverse tunnel the SSH facade refuses (logins-and-ssh.md).

## Port forwards

A `forward` block on a segment, or a `port` block on a container
(containers.md), makes the daemon listen on a host port and proxy TCP, UDP or
both into the segment. This is the host-to-guest path for RDP, SSH and web UIs.
It works identically under WSL 2, where Windows-side access rides WSL's own
localhost forwarding.

Every forward a lab needs is worked out as one **forward plan** before any is
installed, with lease resolution as the only runtime input. A forward whose
machine has no lease yet is skipped with a reason rather than dropped, and
installed once the lease arrives. Two forwards claiming one host port are
settled in the plan: the first claimant keeps the port and the rest are
dropped, naming the winner, rather than all being installed and the losers
failing at bind time. Scripts can add forwards at runtime with
`Segment.forward`.

## Guest routes and inter-segment routing

Multi-segment topologies are routed through a machine. Give a router VM a NIC
on each segment, and declare `route` blocks on the segments whose guests should
know about the other side. Each route is pushed to every guest at lease time as
DHCP option 121, so a firewall or router lab needs no guest configuration.

A segment's `routes_to` list names other segments the daemon itself should
forward L3 traffic to, always an explicit opt-in per pair. Validation checks
that every target is a declared segment. The daemon's own forwarding engine
behind that field is not yet wired, and the script verbs `route_to` and
`unroute_to` answer with an error saying so. Route through a machine for now.

## Filtering and redirection

Two enforcement layers are declared in the lab file and mutable at runtime from
wscript. Runtime mutation is a first-class lab scenario: block the DC and watch
the client fail over. There is no `vmlab net` command; static rules belong in
`vmlab.wcl` and dynamic ones in scripts.

**DNS rules** are `sinkhole` blocks, per segment or lab-wide. A sinkhole
answers a name pattern with NXDOMAIN, or with `0.0.0.0` when its mode is
`zero`, and wildcards such as `*.telemetry.example.com` are supported. A
`record` overrides a name to an address of your choosing. Both are only
effective for guests using the segment's DNS.

**L3 rules** run at the switch on every guest-originated IPv4 packet addressed
to the gateway. A `block` rule drops traffic to or from a CIDR, optionally
scoped by protocol and port, and answers with a TCP reset or an ICMP
unreachable so the guest fails fast instead of hanging. A `redirect` rule is
DNAT: traffic to one `ip[:port]` is rewritten to another, and the daemon keeps
the connection state to rewrite the return path.

Redirect rules are evaluated before block rules, so a packet matching both is
redirected, not dropped. Within a layer the most specific match wins: an
`ip:port` redirect beats a port-less one, and among blocks the longest prefix
wins, then a rule with a port, then a rule with a protocol. Remaining ties go
to declaration order. Removing a redirect stops new rewrites at once, while
established return-path entries linger until they idle out.

From a script, `Segment.block`, `block_port`, `redirect`, `dns_set` and
`dns_sinkhole` each return a rule id, and `unblock` and `dns_clear` remove one.
`rules` lists what is currently in force. The full API is in
wscript-lab-api.md.

## Global segments and cross-host trunks

A segment declared `global = true` is owned by the supervisor rather than the
lab daemon. It is created on first attach, destroyed on last detach, and shared
by every lab on the host that declares the same name. Each lab daemon attaches
over a **trunk**, a frame-forwarding connection on a unix socket, and the
supervisor runs the shared segment's DHCP and DNS so registrations span labs
coherently. Machines in different labs on a global segment resolve each other's
names.

The same trunk protocol over TCP is the whole cross-host story. A global
segment with a `connect { host = "peer:port" }` child is bridged to the
same-named segment on another host's supervisor, with the two supervisors
authenticating by the pre-shared key both set in `psk` in their host
configuration and listening on `trunk_port`. VMs stay local; only the wire
spans hosts. `connect` on a segment that is not global is a validation error.

Tip — declare `connect` on one side: the supervisor keeps at most one trunk per
remote host per segment, so both sides declaring `connect` to each other is
safe on a plain network: the dialer stands down while an inbound trunk from
that address is active. A NAT'd or multi-homed peer can defeat the address
match, so on such topologies declare `connect` on one side only.

## The eBPF fast path

The netdev attachment is designed so a faster backend can be substituted per
segment without changing lab semantics, and two opt-in kernel tiers use that
seam.

- **afxdp** attaches NICs as tap devices with a per-segment XDP program that
  forwards known unicast between two non-isolated guest ports in-kernel;
  everything else punts to the daemon and crosses the userspace switch as
  before.
- **sockmap** keeps the stream sockets and splices known guest-to-guest unicast
  between them in-kernel. It is functionally validated but measured slower than
  the userspace fabric, so `auto` never selects it.
- **userspace** is the fabric as described, and always the fallback.

Both kernel tiers need `CAP_BPF` and `CAP_NET_ADMIN`, and each daemon proves a
tier works on its host, loading the programs and pushing frames through
throwaway taps or sockets, before using it. An unprivileged or WSL 2 daemon
degrades to userspace silently. The gateway MAC and the service and trunk ports
never enter the kernel forwarding tables, so DHCP, DNS, NAT, rules and forwards
behave identically on every tier. Force a tier with `fastpath` in the host
configuration or the `VMLAB_FASTPATH` environment variable.

# segment {} and its child blocks

Value types: `utf8` is a quoted string. `bool` is `true` or `false`. `i64` is
an integer. `ByteSize` is an integer with a unit, such as `8GiB` or `512MiB`.
`Duration` is an integer with a unit, such as `10s`. `list<utf8>` is a
bracketed list of strings, such as `["dc01", "dc02"]`.

The schema rejects unknown fields, wrong types and missing required child
blocks as the file is parsed. `vmlab validate` then runs the semantic rules
listed under each entry, reports every problem it finds in one pass, and every
other verb runs the same checks before it touches a machine.

## segment {}

One virtual L2 switch owned by the lab daemon, or by the supervisor when
`global = true`. Machines attach to it with `nic {}` blocks. A segment with no
fields gets an auto-allocated subnet, DHCP and DNS from the daemon, and no
internet egress.

```wcl
segment "<name>" {
  subnet    = "10.50.0.0/24"
  global    = false
  dhcp      = true
  nat       = false
  mtu       = 1500
  routes_to = ["other"]
  dns      { … }
  connect  { … }
  route    { … }
  record   { … }
  forward  { … }
  block    { … }
  redirect { … }
  sinkhole { … }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Segment name, a DNS label, unique per lab; the inline block label. |
| `subnet` | utf8 | auto | IPv4 CIDR. Auto-allocated as a /24 from the host pool if omitted. |
| `global` | bool | `false` | Owned by the supervisor and shared across labs. |
| `dhcp` | bool | `true` | Enable DHCP on this segment. |
| `nat` | bool | `false` | Enable NAT internet egress for this segment. |
| `mtu` | i64 | 9000 or 1500 | Link MTU, 576 to 65535. Default is jumbo (9000) on a `nat` or `global` segment, else 1500. |
| `routes_to` | list<utf8> | none | Names of other segments the daemon routes to. Inter-segment routing is opt-in per segment. |
| `dns {}` | child | none | DNS service override: hand out another server, or opt out. |
| `connect {}` | child | none | Cross-host segment peer over TCP, authenticated by the PSK from host config. |
| `route {}` | children | none | Guest routes pushed via DHCP option 121. |
| `record {}` | children | none | Static DNS entries for this segment; wildcards allowed. |
| `forward {}` | children | none | Host-to-guest port forwards. |
| `block {}` | children | none | L3 block rules at the switch. |
| `redirect {}` | children | none | L3 DNAT redirect rules. |
| `sinkhole {}` | children | none | DNS sinkhole rules. |

The daemon claims the first usable address of the subnet as the segment
gateway. DHCP, DNS, NAT and shared folders are all served there. The host pool
the automatic /24 comes from is `subnet_pool` in the host configuration file
(host-profiles.md).

Validation enforces these rules:

- The name is a DNS label and no other segment in the lab has it.
- `subnet` is a well-formed CIDR, and no two declared subnets overlap.
- `mtu` is between 576 and 65535.
- Every name in `routes_to` is a segment declared in this lab.
- A `connect {}` child requires `global = true`; on a lab-local segment it
  would be ignored, so it is refused.
- A segment with a machine gateway (a `nic` with `gateway = true`) cannot also
  set `nat = true`, and cannot be `global`.

```wcl
# examples/ad-lab/vmlab.wcl
segment "corp" {
  subnet = "10.50.0.0/24"
  // Hand out the DC as DNS instead of the daemon (AD owns DNS).
  dns { server = "10.50.0.10" }
  route { dest = "10.60.0.0/24" via = "10.50.0.254" }
}
```

## dns {}

Overrides the DNS server a segment hands out over DHCP. Use it when a guest,
such as a domain controller, owns name resolution for the segment. At most one
per segment.

```wcl
dns {
  server  = "10.50.0.10"
  enabled = true
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `server` | utf8 | the daemon | IPv4 address of the DNS server to hand out via DHCP instead of the daemon. |
| `enabled` | bool | `true` | Hand out a DNS server at all. `false` suppresses the DHCP DNS option. |

`server` must parse as an IPv4 address. The daemon's own resolver keeps
answering on the gateway address either way; the block only changes what guests
are told to use.

## connect {}

Bridges this segment to the same segment on another host. The two supervisors
tunnel L2 frames over TCP, authenticated by the pre-shared key both hosts set
in their host config. At most one per segment.

```wcl
connect { host = "helios:13947" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host` | utf8 | required | Remote supervisor as `host[:port]` to bridge this segment with. |

Validation requires the segment to be `global = true` and `host` to be
non-empty. The port defaults to the remote's `trunk_port`, which is 13947
unless its host config changes it. Set `psk` in the host configuration file on
both sides (host-profiles.md).

```wcl
# examples/peer-a/vmlab.wcl
segment "wan" {
  subnet = "10.99.0.0/24"
  global = true
  connect { host = "127.0.0.1:13948" }   // side B's trunk_port
}
```

## route {}

A static route pushed to every guest on the segment at lease time, as DHCP
option 121. This is how a router VM becomes the path to another segment.

```wcl
route { dest = "10.60.0.0/24" via = "10.50.0.254" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `dest` | utf8 | required | Destination CIDR, for example `10.60.0.0/24`. |
| `via` | utf8 | required | Gateway IPv4 address the route points at. |

`dest` must parse as a CIDR and `via` as an IPv4 address. A guest with
`dhcp = false` on its segment never receives the route.

## record {}

A static DNS entry. Inside a `segment {}` it answers on that segment; inside
`lab {}` it answers on every segment of the lab.

```wcl
record { name = "*.internal" ip = "10.50.0.10" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 | required | DNS name to resolve. Wildcards are allowed, for example `*.internal`. |
| `ip` | utf8 | required | IPv4 address the name resolves to. |

`ip` must parse as an IPv4 address. Records are only seen by guests using the
segment's DNS; a segment whose `dns {}` hands out another server bypasses them.

## forward {}

A host-to-guest port forward. The daemon listens on the host port and proxies
into the segment, which is the path for RDP, SSH and web UIs from the host.

```wcl
forward { host_port = 13389 to = "dc01:3389" proto = "tcp" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host_port` | i64 | required | Host port to listen on, 1 to 65535. Unique across the lab. |
| `to` | utf8 | required | Target as `vm:port`. The machine must be declared in this lab. |
| `proto` | utf8 | `tcp` | Protocol: `tcp`, `udp` or `both`. |

Validation requires `to` to have the form `name:port` with a numeric port, the
name to be a VM or container in this lab, and `host_port` to be unused by every
other `forward` and every container `port {}` in the lab, since both compile
into the same forward machinery.

```wcl
# examples/mixed-lab/vmlab.wcl
segment "lan" {
  subnet = "10.70.0.0/24"
  nat = true  # apt needs egress
  forward {
    host_port = 18080
    to = "nix01:80"
  }  # host → nginx
}
```

## block {}

An L3 rule that drops traffic to or from a CIDR at the switch, answering with
ICMP unreachable or TCP RST where it can so guests fail fast.

```wcl
block { cidr = "203.0.113.0/24" proto = "tcp" port = 443 }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `cidr` | utf8 | required | IPv4 CIDR to drop traffic to and from. |
| `proto` | utf8 | any | Protocol to scope the rule: `tcp`, `udp` or `icmp`. |
| `port` | i64 | any | Port to scope the rule, 1 to 65535. Requires `proto`. |

`cidr` must parse as a CIDR. Redirect rules are evaluated before block rules;
within a layer the most specific match wins, and ties go to declaration order.
Scripts can add and remove rules at runtime (automation.md).

## redirect {}

An L3 DNAT rule. Traffic to one destination is rewritten to another, and the
daemon keeps the connection state to rewrite the return path.

```wcl
redirect { from = "10.50.0.10:443" to = "10.50.0.99:8443" proto = "tcp" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `from` | utf8 | required | Match destination as `ip[:port]`. |
| `to` | utf8 | required | Rewrite destination to `ip[:port]`. |
| `proto` | utf8 | any | Protocol to scope the rule: `tcp` or `udp`. |

Both addresses must parse as an IPv4 address with an optional numeric port. A
rule without a port matches every port.

## sinkhole {}

A DNS sinkhole. Names matching the pattern get NXDOMAIN, or resolve to 0.0.0.0
in `zero` mode. Inside a `segment {}` it applies there; inside `lab {}` it
applies to every segment.

```wcl
sinkhole { pattern = "*.telemetry.example.com" mode = "nxdomain" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `pattern` | utf8 | required | DNS name pattern to sink; wildcards allowed. |
| `mode` | utf8 | `nxdomain` | Response: `nxdomain`, or `zero` to resolve to 0.0.0.0. |

Validation rejects an empty pattern. Like records, a sinkhole is only effective
for guests that use the segment's DNS.

# vmlab dns

`vmlab dns` prints the DNS zones the current lab's segments serve: every exact
record, wildcard and sinkhole, in the order the resolver consults them. It is
the thing to read when a guest cannot resolve a peer.

```sh
vmlab dns [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--json` | Emit the raw JSON instead of a table. |
| `-h`, `--help` | Print help. |

## What it prints

The verb starts the lab daemon if none is running and asks it for the DNS
table. Segments with no local zone, global segments and segments with
`dns { enabled = false }`, serve nothing and are not listed; with no serving
segment at all the verb prints `no segment in this lab serves DNS`.

Each serving segment gets a heading `segment "<name>" — zone <suffix>` and a
table of `NAME`, `IP` and `KIND`. Exact records come first, sorted by name,
with kind `static` for a record the lab file declares and `dynamic` for one a
DHCP lease registered; then wildcards with kind `wildcard`; then sinkholes with
`-` for the address and kind `sinkhole/<mode>`, since a sinkhole answers with
nothing and `NXDOMAIN` and `0.0.0.0` fail differently in a guest. A zone with
no rules prints `(no records)`.

With `--json` the daemon's reply is printed verbatim as pretty JSON, an object
with a `segments` array whose entries carry `segment` and a `zone` with
`suffix`, `records`, `wildcards` and `sinkholes`.

## Examples

```sh
vmlab dns
```

```sh
segment "corp" — zone vmlab.internal
  NAME                              IP          KIND
  client01.ad-lab.vmlab.internal    10.10.0.50  dynamic
  dc01.ad-lab.vmlab.internal        10.10.0.10  static
  *.corp.example                    10.10.0.10  wildcard
  *.telemetry.example.com           -           sinkhole/nxdomain
```

Feed the table to a script:

```sh
vmlab dns --json | jq '.segments[].zone.records[]'
```

## Exit status

Exit status is 0 when the table was printed. A lab directory that cannot be
found, or a daemon that could not be started or answered with a failure, exits
1 (`failed`). Exit 5 (`conflict`) means the supervisor tracks a lab with this
name from another directory. A usage error exits 2.

# vmlab fastpath

`vmlab fastpath` shows which network fast-path tier the supervisor selected for
switch traffic, and why the tiers it skipped were unavailable. The tiers are
the substitutable backends of the userspace fabric.

```sh
vmlab fastpath
```

| Option | Meaning |
| --- | --- |
| `-h`, `--help` | Print help. |

The command starts the supervisor if it is not running, like every other verb,
because the answer is the probe result of the daemon that will carry the
traffic. It prints one line naming the tier and the mode it was selected under,
then one line per skipped tier with the reason.

The tier is one of `afxdp`, tap devices with in-kernel XDP forwarding;
`sockmap`, kernel socket splicing on the stream-socket ports; or `userspace`,
the plain switch that is always available. The mode is the `fastpath` key in
the host configuration (host-profiles.md), overridden by the `VMLAB_FASTPATH`
environment variable: `auto` probes `afxdp` and otherwise falls back to
`userspace`, `off` never uses a kernel path, and `sockmap` or `afxdp` probe
only that tier. `auto` never picks `sockmap`, because it measures slower than
the userspace fabric, and the reasons say so. A forced tier whose probe fails
degrades to `userspace` rather than stopping the daemon. A vmlab built without
the `ebpf` feature reports both kernel tiers unavailable for that reason.

```sh
$ vmlab fastpath
network fast path: userspace (mode auto)
  afxdp unavailable: vmlab was built without the `ebpf` feature
  sockmap unavailable: not used in auto mode: af_unix kernel splicing measures slower than the userspace fabric (psock backlog workqueue); force with `fastpath = "sockmap"` to evaluate it
```

Exit status is 0 on success. The command discards the protocol error code and
exits 1 for any failure, including a supervisor that does not come up.
