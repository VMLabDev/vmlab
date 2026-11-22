# Review: networking (lab networks, port forwards, trunks, XDP fast path)

Card #439. A bug-hunting review of the lab network stack of vmlab. It lists
faults and makes no fix. Reviewed at `next` commit `7e09781f`. All paths are
from the repository root, and all line numbers are from that commit.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped.

Severity words: **data loss**, **crash** (of the VM or a host process),
**security** (against a hostile guest or peer), **wrong result**,
**cosmetic**. A finding that lets a guest or a peer exhaust a host process is
filed under security, and its text says that the effect is a denial of
service.

Who the attacker is, per finding:

- **guest**: code running inside a VM or container on a lab segment. It can
  send any ethernet frame on its NIC.
- **peer**: another host on a cross-host trunk. An *authenticated* peer holds
  the PSK. An *unauthenticated* peer can only reach the trunk TCP port.
- **local user**: an unprivileged account on the host that can execute
  `vmlab-fastpath`.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | security (peer) | A trunk peer's segment name becomes a socket path: it can delete and replace any `*.sock` file of the supervisor's user (fixed in card #466) |
| F2 | security (guest, local user) | A tap from `vmlab-fastpath` outlives its XDP filter after `unload`, a helper exit or a daemon crash (fixed in card #467) |
| F3 | security (guest) | NAT has no limit on TCP flows, UDP flows or `ping` subprocesses (fixed in card #476) |
| F4 | security (guest) | One stalled NAT TCP flow blocks the gateway task of the whole segment (fixed in card #477) |
| F5 | security (guest) | Outside worktree isolation a guest on a NAT segment reaches the host's loopback services (fixed in card #460) |
| F6 | security (guest) | Tables learned from guest frames have no size limit: switch MACs, gateway neighbours, NAT MACs, DHCP declines, ping cache |
| F7 | security (guest) | The redirect connection table has no limit and is scanned in full for every gateway-bound packet |
| F8 | security (guest) | DHCP trusts `chaddr`: any guest can starve the pool, quarantine it for the daemon's life, and drop another machine's lease (fixed in card #501) |
| F9 | security (guest) | Nothing binds a port to its MAC or IP: a guest, isolated or not, takes over the gateway's or another guest's traffic and its port forwards (fixed in card #502) |
| F10 | security (guest) | IP fragments evade port-specific `block` and `redirect` rules on routed traffic (fixed in card #478) |
| F11 | security (peer) | The trunk listener takes unlimited unauthenticated connections and spins on an `accept` error (fixed in card #479) |
| F12 | wrong result | A dialed trunk bridge outlives its segment, and the trunk never forms again |
| F13 | wrong result | A cross-host segment gets a second subnet, DHCP server and gateway on the accepting host, with the same gateway MAC |
| F14 | wrong result | A global subnet index is never reused: the 257th global segment of a supervisor's life is refused |
| F15 | wrong result | A global attach that fails on the PSK leaves a reference, a gateway and a listener behind |
| F16 | wrong result | Every machine start leaves one NIC listener task and its descriptor behind |
| F17 | wrong result | A DNS sinkhole does not cover HTTPS, SVCB, CNAME or any type but A, AAAA and ANY |
| F18 | wrong result | A guest's TCP connection to its gateway address on an unserved port is dialed out on the host's own network |
| F19 | wrong result | A DHCP option longer than 255 bytes is written with a truncated length |
| F20 | wrong result | A lab segment's DNS never drops the name of a lease that ended |

### F1. A trunk segment name is a filesystem path (security, peer)

- Where: `src/supervisor/trunk_auth.rs:66-70` (`server`), and
  `src/supervisor/global.rs:702` (`accept_peer`), `:595` (`attach`), `:653`
  (`detach`), and `src/net/switch.rs:384-392` (`listen_unix_with_config`).
- Why it is a fault: the trunk handshake returns the segment name the peer
  sent. The only checks are a length of 1 to 128 bytes and valid UTF-8.
  `accept_peer` calls `attach(name, ..)`, and `attach` builds the trunk socket
  path with `self.sock_dir.join(format!("{name}.sock"))`. `listen_unix` then
  removes whatever file is at that path and binds a listening socket there.
  A lab file's segment name passes `check_dns_label`
  (`src/config/validate.rs:119`). A name from a peer passes nothing. So an
  authenticated peer that sends `../vmlabd` makes the supervisor delete its
  own control socket (`src/paths.rs:96-98`) and replace it with a switch
  trunk socket. Every later `vmlab` command then connects to a switch port. A
  name that starts with `/` replaces the base directory, because
  `Path::join` with an absolute path does that. The peer can therefore delete
  and replace any file named `<anything>.sock` that the supervisor's user can
  unlink: a lab daemon's `control.sock`, a VM's QMP or agent socket. When the
  trunk closes, `detach` unlinks the path again.
- Proof: test `a_peer_cannot_name_a_segment_outside_the_socket_directory` in
  `src/supervisor/global.rs`. Start one `instance("k")`. Create a regular file
  `<tempdir>/victim.sock` beside the instance's `sock_dir`. Connect a
  `TcpStream` to the listener and run `trunk_auth::client(&mut s,
  "../victim", "k")`. Read the 3-byte answer. Assert that the answer is not
  `OK\n` and that `<tempdir>/victim.sock` is still a regular file. Today the
  answer is `OK\n` and the file is a socket.
- Fixed in card #466: `check_segment_name` in `src/supervisor/global.rs`
  refuses a name that is not one file name (empty, over 128 bytes, `.`, `..`,
  or holding `/` or a control character). The trunk listener answers `NO\n`
  to such a name, and `attach` refuses it for every caller. The proof test
  failed before the fix and passes after it.

### F2. A fast-path tap outlives its XDP filter (security, guest and local user)

- Where: `src/net/fastpath/helper/server.rs:353-356` (`Request::Unload`),
  `:205-237` (`Request::XdpTap` returns the queue descriptors), `:366`
  (`serve` returns when the connection closes), `tap/src/lib.rs:136-150` (the
  tap is brought up), `src/net/fastpath/engine.rs:57-66` (`attach`), and
  `src/net/fastpath/afxdp.rs:328-338` (the hypervisor gets a dup of the queue
  descriptor).
- Why it is a fault: `ebpf/xdp-switch/src/main.rs:9` states the invariant:
  "The fall-through is XDP_DROP: guest frames never touch the host stack".
  That holds only while the program is attached. A tap is non-persistent and
  lives as long as any descriptor on it is open. The helper hands the queue
  descriptors to its caller, and the daemon hands a dup to QEMU or vmlab-hv.
  The XDP attachment belongs to the helper's `aya::Ebpf` object. `Unload`
  drops that object while the caller still holds the tap descriptors, and a
  helper exit closes it. The tap is then an ordinary interface of the host
  that is up, has no filter, and (with default sysctls) has an IPv6
  link-local address. Two ways to get there:
  1. A lab daemon dies (crash, `SIGKILL`, OOM) while a VM runs. The helper
     sees its socket close and exits. The VM process keeps its tap
     descriptor. Every frame the guest sends now enters the host's network
     stack. The guest reaches host services that listen on `::` or
     `0.0.0.0`, without NAT, without L3 rules and without the worktree host
     guard.
  2. A local user starts `vmlab-fastpath` with a `SOCK_SEQPACKET` socket on
     stdin, sends `xdp_load` and `xdp_tap`, keeps the descriptor from the
     reply, and sends `unload` or closes the socket. That user now holds an
     unfiltered tap, which the kernel otherwise gives only to
     `CAP_NET_ADMIN`. The helper's own rule
     (`src/net/fastpath/helper/server.rs:3-9`) is that what it accepts is
     what any program of the user can do.
- Assumption to confirm: that dropping the `aya::Ebpf` object, or the helper
  process ending, detaches the program. The module comment
  `src/net/fastpath/helper/mod.rs:5-7` says the attachments live as long as
  the daemon does. The test below decides it.
- Proof: test `a_tap_is_closed_to_the_host_stack_after_unload` in
  `src/net/fastpath/priv_tests.rs` (it needs the capabilities those tests
  already need). Load an engine through the helper client, take a tap, read
  the tap's link-local address from `getifaddrs`, send `unload`, then write
  an ICMPv6 neighbour solicitation for that address into the queue
  descriptor. Assert that no neighbour advertisement can be read back within
  one second. Today the host answers. Repeat with the helper process killed
  instead of `unload`.
- Fixed in card #467. The assumption held: aya attaches through a `bpf_link`,
  which ends when the helper drops the engine or exits. `XdpEngine::attach`
  (`src/net/fastpath/engine.rs`) now sets the program on the tap through
  rtnetlink (`RTM_SETLINK`, `IFLA_XDP`, generic mode). The device then holds
  the program and its maps until the tap is destroyed, so `unload`, a helper
  exit and a daemon crash all leave the filter in place. The proof tests are
  `fastpath_afxdp_tap_is_closed_to_the_host_stack_after_unload` and
  `fastpath_afxdp_tap_is_closed_to_the_host_stack_after_the_helper_is_killed`
  (the `fastpath_afxdp_` prefix is what `just fastpath-test` selects by). They
  run the helper as a real process and read the address from
  `/proc/net/if_inet6`, which also says when duplicate address detection is
  over. They need CAP_BPF and CAP_NET_ADMIN, so `just ci::check` does not run
  them.

### F3. NAT has no limit on flows or on `ping` subprocesses (security, guest)

- Where: `src/net/nat/mod.rs:236-241` (a SYN inserts a flow and spawns a
  task), `src/net/nat/vtcp.rs:237` (one host `connect` per SYN, 10 s),
  `src/net/nat/udp.rs:41-55` and `:70` (one host socket, one task and one
  65535-byte buffer per UDP flow, 60 s idle), `src/net/nat/mod.rs:293-297`
  and `src/net/nat/icmp.rs:126-131` (one `ping` process per destination not
  in the cache), and `src/net/gateway.rs:476` with `src/net/dns.rs:625` (one
  task and one host UDP socket per forwarded DNS query, 3 s).
- Why it is a fault: none of the four tables has a size limit, and nothing
  counts flows per guest. A guest that sends UDP datagrams to distinct
  destination ports creates one host socket and 64 KiB of buffer for each,
  held for 60 seconds. At 10 000 datagrams a second that is 600 000 sockets
  and about 39 GiB. The lab daemon reaches `RLIMIT_NOFILE` long before that
  (nothing in `src/` raises it), and from then on it cannot open a QMP
  socket, an agent socket or a disk. A SYN flood to distinct ports does the
  same with TCP sockets. An echo request to each address of a /16 starts one
  `ping` process each, because the cache only stops a second probe of the
  same address. DNS queries for names outside the zone do the same with
  3-second sockets. The effect is a denial of service of the lab daemon and
  of every machine it runs, from one guest.
- Proof: test `udp_flows_are_bounded` in `src/net/nat/tests.rs`. Send 5 000
  UDP frames from one guest address to 5 000 distinct destination ports
  through `handle_frame`. Assert `engine.udp_flows.lock().len()` is at most a
  stated limit. Today it is 5 000 (or the bind fails at the descriptor
  limit). The same shape with SYNs (`tcp_flows`), and with a counting
  `Pinger` stub and 5 000 echo destinations (assert a limit on concurrent
  `ping` calls).
- Fixed in card #476. Each table has a limit per guest address and a limit
  per engine, so one guest cannot take every slot of its segment. Guest-opened
  TCP flows: 128 per guest and 256 in all (`MAX_TCP_FLOWS` in
  `src/net/nat/mod.rs`). A SYN past the limit is dropped, and the guest's
  retransmission retries it. Flows the engine opens toward a guest (port
  forwards) are not counted. UDP flows: 128 per guest and 256 in all
  (`MAX_UDP_FLOWS` in `src/net/nat/udp.rs`). A new flow past the limit
  replaces the flow idle the longest, of that guest or of the table, so a
  table full of abandoned flows does not refuse a live one. `ping` probes in
  flight: 8 per guest and 32 in all (`MAX_PROBES` in `src/net/nat/icmp.rs`).
  An echo request past the limit is dropped. Forwarded DNS queries in flight:
  64 per gateway (`MAX_DNS_FORWARDS` in `src/net/gateway.rs`), with no share
  per guest. A query past the limit is dropped. The proof tests
  `udp_flows_are_bounded`, `tcp_flows_are_bounded` and
  `ping_probes_are_bounded` in `src/net/nat/tests.rs` and
  `forwarded_dns_queries_are_bounded` in `src/net/gateway.rs` failed before
  the fix (1 833 UDP flows where the descriptor limit stopped the test, 5 000
  TCP flows, 3 050 probes at once, 200 forwards) and pass after it. Not
  changed: the limits count per segment, the lab daemon still does not raise
  `RLIMIT_NOFILE`, and a guest that sends from many source addresses (F9)
  has the engine's limit and not one guest's.

### F4. One stalled TCP flow blocks the whole gateway (security, guest)

- Where: `src/net/gateway.rs:457` (the gateway task awaits the uplink),
  `src/net/nat/mod.rs:229-234` (`handle_tcp` awaits `tx.send(seg)` on a
  64-slot channel), `src/net/nat/vtcp.rs:566` and `:660` (the flow task
  awaits `wr.write_all` to the host socket), and `:237` (the flow task reads
  nothing from its channel during the host `connect`, up to 10 s).
- Why it is a fault: the gateway is one task per segment. It handles ARP,
  DHCP, DNS and every NAT, routed and port-forward frame in order. A flow
  task that waits in `write_all` (the remote end does not read) or in
  `connect` (the destination does not answer) does not drain its channel.
  After 64 more segments for that flow, `tx.send(seg).await` blocks, and with
  it the gateway task. The idle timeout does not fire, because the flow task
  is inside `write_all` and not in its `select!`. A guest does this on
  purpose: it connects to a server that never reads, sends data past the
  advertised window until the host socket buffer is full, then sends 65 more
  segments. From then on no guest on the segment gets a DHCP answer, a DNS
  answer or any NAT traffic, and no port forward into the segment works. A
  well-behaved guest that uploads to a slow server can hit the same state
  for a short time through its retransmissions.
- Proof: test `a_stalled_flow_does_not_block_other_frames` in
  `src/net/nat/tests.rs`. A host listener accepts and never reads. Complete
  the guest handshake, then feed segments with `handle_frame` until one call
  does not return within a second (wrap each call in `timeout`). Assert that
  every `handle_frame` call returns within a second. Today a call blocks
  after the host socket buffer plus 64 segments. A second form at the
  gateway: with the flow stalled, send an ARP request for the gateway and
  assert a reply within a second.
- Fixed in card #477. `handle_tcp` hands a segment to its flow with
  `try_send` and never waits: a segment that finds the flow's channel full
  is dropped, and the guest sends it again. The flow task no longer waits
  for the host's socket outside its `select!`. Guest data waits in a buffer
  of one window per flow (`Flow::to_host` in `src/net/nat/vtcp.rs`), and the
  window the guest is told is the room left in it, where it was a constant
  64 KiB before. A host that does not read therefore closes the guest's
  window, the flow sends a window update when the host reads again, and the
  guest's FIN reaches the host after the buffered data. The channel grew
  from 64 to 256 segments (`SEGMENT_QUEUE`), which is more than a guest
  inside its window has in flight, so only a guest that ignores the window
  loses segments there. The wait during the host `connect` is covered by
  the same `try_send`. The proof test failed before the fix
  (`handle_frame` blocked at segment 64) and passes after it. The second
  form is `a_stalled_flow_does_not_block_the_gateway`, which got no ARP
  reply before the fix. Two more tests cover the new path: the window
  closes and reopens with every acknowledged byte delivered in order, and a
  FIN behind buffered data. Not changed: `handle_udp` still awaits the
  64-slot channel of an engine-originated UDP flow (`guest_udp`), and the
  gateway still awaits the engine's output channel when it resets an
  unknown flow.

### F5. A NAT guest reaches the host's loopback (security, guest)

- Where: `src/net/nat/vtcp.rs:236-237` (`TcpStream::connect((key.dst_ip,
  key.dst_port))`), `src/net/nat/udp.rs:48`, `src/net/nat/mod.rs:183-185`
  (the guard is set only under worktree isolation), and
  `src/labd/netservices.rs:103-107`.
- Why it is a fault: on a segment with `nat = true` every frame to the
  gateway MAC is admitted, and the engine connects to the destination address
  as written. A guest that sends a SYN for `127.0.0.1:5432` to the gateway
  MAC is connected to the host's own loopback port 5432. A service that binds
  loopback does so to be reachable from the host only: a database, a
  development server, an admin port. Any guest with NAT egress reaches all of
  them, as a connection from the host itself. The same holds for
  `127.0.0.0/8`, `0.0.0.0` and UDP. `HostGuard`
  (`src/net/nat/guard.rs`) refuses exactly these destinations, but it is
  installed only in a frozen worktree family. PRD §9.7 does not say that a
  guest may reach the host's loopback, and §9.8 offers `bind = "127.0.0.1"`
  to keep a service "off the LAN", which shows that loopback is treated as a
  boundary elsewhere.
- Proof: test `nat_refuses_loopback_without_isolation` in
  `src/net/nat/tests.rs`. Bind a listener on `127.0.0.1:0`. With no guard
  set, send a guest SYN for that address and port. Assert the guest gets a
  RST and the listener accepts nothing. Today the guest gets a SYN-ACK and
  the listener accepts.
- Fixed in card #460. Every NAT engine refuses a new flow to `127.0.0.0/8`
  and `0.0.0.0/8`: a SYN gets a RST, UDP and ICMP are dropped. An engine with
  no guard refuses all of it (`NatEngine::refused`, `guard::is_loopback`). A
  segment outside isolation gets `HostGuard::loopback`, which admits two
  things on loopback: the lab's own host services (its `smbd` behind
  gateway:445) and the target of a `redirect` the lab declares onto loopback
  (`RuleSet::redirects_onto`), since that rule is the operator's grant. The
  host's interface addresses stay reachable outside isolation, as PRD §9.7
  now says. The proof test failed before the fix (the guest got a SYN-ACK,
  flags 18) and passes after it.

### F6. Tables learned from guest frames have no limit (security, guest)

- Where: `src/net/switch.rs:543-550` (`inner.macs`, one entry per source
  MAC), `src/net/gateway.rs:390-396` (`Neighbours::table`, one entry per ARP
  sender address), `src/net/nat/mod.rs:205` (`macs`, one entry per IPv4
  source address), `src/net/dhcp.rs:283-292` (`declined`, one entry per
  declined address), and `src/net/nat/icmp.rs:111-113` (`cache`, one entry
  per echo destination, never removed).
- Why it is a fault: each table grows by one entry for each new value of a
  field the guest writes freely, and no entry is ever aged out or counted.
  A guest that sends frames with random source MACs adds an entry per frame
  to the switch table (2^46 possible unicast MACs). ARP frames with random
  sender addresses do the same to the gateway (2^32 entries). IPv4 frames
  with random source addresses do the same to the NAT engine. The lab daemon
  grows until the host kills it, and all its machines go with it. On a global
  segment the process that grows is the supervisor, which every lab on the
  host depends on. In the sockmap tier every new source MAC is also a BPF
  map update (`relearn`).
- Proof: test `the_mac_table_is_bounded` in `src/net/switch.rs`. Ingress
  100 000 frames with distinct unicast source MACs on one guest port. Assert
  `inner.macs.len()` is at most a stated limit. Today it is 100 000. The same
  shape for the gateway (100 000 ARP requests, `neighbours.table.len()`), for
  `NatEngine::macs`, and for `DhcpServer::declined` (see F8).

### F7. The redirect connection table: no limit, full scan per packet (security, guest)

- Where: `src/net/rules.rs:279` (`eval` calls `expire_conns`), `:372-377`
  (`retain` over the whole table), `:306` (one entry per redirected flow),
  and `src/labd/netservices.rs:76-80` (called from the switch ingress hook
  under the rule set's mutex).
- Why it is a fault: every guest IPv4 frame to the gateway MAC runs `eval`,
  and `eval` first walks the whole connection table to drop idle entries. A
  packet that matches a redirect adds an entry keyed on the source address
  and source port the guest chose, kept for five minutes. A guest that sends
  packets with changing source ports to a redirected address therefore makes
  each later packet of every guest cost one pass over all entries. With a
  million entries, which takes seconds to create, the switch's ingress path
  does a million-entry scan per frame while holding the hook lock, and the
  segment stops forwarding in practice. Memory grows without limit as well.
  A lab with a shared folder has such a redirect: the gateway port for the
  lab's `smbd` is one (`src/net/nat/guard.rs:15-18`).
- Proof: test `eval_cost_does_not_grow_with_the_connection_table` in
  `src/net/rules.rs`. Add one redirect. Call `eval` with 200 000 packets that
  differ in source port. Assert `conn_count()` is at most a stated limit, and
  that 1 000 further `eval` calls of a packet that matches no rule finish
  within a fixed time. Today `conn_count()` is 200 000 and each call scans
  all of them.

### F8. DHCP trusts `chaddr` (security, guest)

- Where: `src/net/dhcp.rs:479-480` (`chaddr` read from the payload, never
  compared with the ethernet source or the ingress port), `:232-243`
  (`on_discover` grants a full lease), `:283-292` (`on_decline`), `:166-171`
  (`DHCP_RELEASE`), and `:247-253` (a REQUEST that names another server
  removes the lease).
- Why it is a fault: one guest can speak for any client.
  1. Quarantine for the daemon's life: a DECLINE with a `chaddr` that has no
     lease and a `requested_ip` of the guest's choice puts that address into
     `declined`. Nothing checks that the address is in the subnet, and
     nothing ever removes an entry. 253 frames take every dynamic address of
     a /24 out of the pool until the lab daemon restarts. More frames grow
     the set without limit (F6).
  2. Starvation: each DISCOVER with a new `chaddr` is granted a lease for the
     full 3600 seconds at the offer, before any REQUEST. 253 frames empty
     the pool for an hour, and the guest can repeat them.
  3. Dropping another machine's lease: a RELEASE, or a REQUEST with a foreign
     server identifier, that carries the victim's MAC as `chaddr` removes the
     victim's lease. The attacker then REQUESTs that address for itself. The
     victim's DNS name still points at the address (F20), and a port forward
     to the victim that is installed again follows the lease table.
- Proof: tests in `src/net/dhcp.rs`.
  `a_decline_without_a_lease_quarantines_nothing`: send a DECLINE with an
  unknown `chaddr` and `requested_ip` set to the first pool address, then a
  DISCOVER from another MAC. Assert the offer is the first pool address.
  Today it is the second, and after 253 such DECLINEs there is no offer.
  `a_release_from_another_port_keeps_the_lease`: this one needs the ingress
  source MAC in the `handle` call, so it is a gateway test in
  `src/net/gateway.rs`: lease as MAC A, send a RELEASE with `chaddr` A in a
  frame whose ethernet source is B, assert `dhcp_leases()` still holds A.
- Fixed in card #501, in `src/net/dhcp.rs`. `handle` ignores a message whose
  `chaddr` is not the ethernet source of its frame, so a guest speaks for its
  own NIC only (the segments have no relay agent). A DECLINE quarantines only
  the address its client holds: one with no lease, or one that names another
  address, changes nothing. A quarantine ends after one lease time, so the
  set is no larger than the leases of one lease time. An OFFER holds its
  address for 60 seconds (`OFFER_HOLD`) and the full lease starts at the ACK.
  The client is still told the full lease time in the OFFER. Five proof tests
  failed before the fix and pass after it: the two named above,
  `a_decline_of_an_address_the_client_does_not_hold_quarantines_nothing`,
  `a_message_that_speaks_for_another_mac_is_ignored` and
  `an_offer_holds_its_address_briefly_until_the_request`.
  - Not fixed here: a guest that also forges the ethernet source of its
    frames still speaks for that MAC, and can still take one address per
    forged MAC with a DISCOVER and a REQUEST. Binding a port to its MAC is F9.

### F9. No port is bound to its MAC or IP (security, guest)

- Where: `src/net/switch.rs:543-550` (a source MAC moves to whatever port
  last sent it, including the gateway's MAC), `src/net/gateway.rs:390-396`
  (any ARP rewrites the neighbour entry of any address),
  `src/net/nat/mod.rs:205` (any frame rewrites the MAC of its IPv4 source
  address), and `src/net/nat/vtcp.rs:263` (a port forward sends its SYN to
  that MAC).
- Why it is a fault: vmlab knows each NIC's MAC (it persists them) and each
  lease, but uses neither to check a frame.
  1. A guest that sends one frame with the gateway's MAC as source owns that
     MAC in the switch table. Frames of the other guests to the gateway (DNS,
     DHCP renewals, all NAT traffic) are then delivered to that guest, or
     dropped if it is an isolated port. This repeats as often as the guest
     sends. On a global segment the victims are in other labs and on other
     hosts.
  2. A guest that sends one IPv4 frame to the gateway MAC with the victim's
     address as source becomes the target of every port forward to the
     victim: the engine sends the SYN of the host's connection to the
     attacker's MAC, and the attacker answers as the victim. A user who
     connects to a forwarded SSH or RDP port then talks to the wrong machine.
     `isolated = true` does not prevent it, because the traffic is between
     the guest and the gateway. PRD §9.1 says an isolated NIC can reach
     port forwards, not that it can take another machine's.
  3. An ARP with the victim's address does the same for traffic routed in
     from another segment (§9.6).
  In the afxdp tier the kernel table is a static projection of the configured
  MACs, so guest-to-guest unicast there is not affected. The userspace paths
  above are the same in every tier.
- Proof: test `a_guest_cannot_take_the_gateway_mac` in `src/net/switch.rs`:
  a service port sends a frame from MAC G, guest port A sends a frame with
  source G, guest port B sends a frame to G. Assert the service port receives
  it. Today port A does. Test `a_forward_goes_to_the_machine_it_names` in
  `src/net/nat/tests.rs`: `learn_mac(victim_ip, victim_mac)`, then
  `handle_frame` with a frame from `attacker_mac` with source `victim_ip`,
  then `open_tcp_to_guest(victim_ip, 22)`. Assert the SYN's destination MAC
  is `victim_mac`. Today it is `attacker_mac`.
- Fixed in card #502, in three places, each with the knowledge vmlab already
  had.
  1. The switch drops a guest port's frame whose source MAC a service port
     holds, and leaves the MAC where it is. `Gateway::spawn` binds the
     gateway MAC to its port with `Switch::bind_mac` before any frame, so a
     guest cannot claim it first. A MAC learned behind a trunk is held the
     same way. A MAC still moves between two guest ports and between two
     service ports.
  2. The NAT engine keeps the address the lab daemon bound with `learn_mac`
     (the MAC of the NIC that holds the lease, set when a forward is
     installed): a frame from another MAC no longer changes where a forward's
     SYN goes. An address nobody bound is still learned from frames.
  3. The gateway drops an ARP or an IPv4 frame whose source address is under
     a DHCP lease of another MAC (`DhcpServer::leased_to_another`), before
     the neighbour table, DNS, NAT and routing see it.
  Both proof tests failed before the change and pass after it, with
  `a_bound_mac_is_held_before_its_port_speaks` in `src/net/switch.rs` and
  `an_arp_cannot_claim_a_leased_address` and
  `a_leased_source_address_needs_its_holders_mac` in `src/net/gateway.rs`.
  Not covered: an address with no lease and no forward (a static one) has no
  owner vmlab knows, so the last frame or ARP still wins for it. Two guests on
  non-isolated ports can still take each other's MAC in the switch, because a
  MAC that moves between guest ports is also what a restarted machine and a
  failover address do. The lease check is only as strong as the lease, which
  the fix of F8 in card #501 now ties to the ethernet source of the DHCP
  message.

### F10. IP fragments evade port rules on routed traffic (security, guest)

- Where: `src/net/rules.rs:140-180` (`Meta::of` parses a TCP or UDP header
  from every packet and never reads `frag_offset` or `more_fragments`,
  `src/net/frame.rs:296-303`), `:381-406` and `:408-430` (a rule with a port
  does not match when `dst_port` is `None`), and `src/net/router.rs:198-260`
  (fragments are forwarded unchanged).
- Why it is a fault: a `block` or `redirect` rule with a port matches only
  when the packet parses as a full L4 header with that port. The first
  fragment of a UDP datagram fails `UdpView::parse`, because the UDP length
  field is larger than the fragment (`src/net/frame.rs:388-391`), so
  `dst_port` is `None` and the rule is skipped. A first TCP fragment shorter
  than 20 bytes of TCP header fails `TcpView::parse` the same way. Later
  fragments carry no header, and their first bytes are read as ports that do
  not match. The router forwards all fragments to the peer segment, where
  the destination reassembles them. So between two segments joined with
  `routes_to`, a guest reaches a port that a `block` rule names by sending
  the datagram in fragments. The NAT engine drops such packets itself, so
  internet egress is not affected.
- Proof: test `a_fragment_does_not_evade_a_port_block` in
  `src/net/rules.rs`. Add `block { cidr = 10.0.1.0/24 proto = udp port = 53
  }`. Build a UDP datagram to `10.0.1.5:53` of 2 000 bytes and split it into
  two fragments (the first with the more-fragments flag). Assert `eval` of
  each fragment is `Verdict::Drop`. Today both are `Verdict::Pass`.
- Fixed in card #478. `Meta::of` reads no port from a fragment, and
  `RuleSet::eval` drops a TCP or UDP fragment, with no reply, when a rule
  with a port covers its destination address and protocol
  (`port_rule_covers` in `src/net/rules.rs`). The rule set keeps no
  reassembly state, so this drops every fragmented datagram to such an
  address and protocol, also one to a port that no rule names. A fragment is
  no longer tracked or rewritten on the return path. Tests
  `a_fragment_does_not_evade_a_port_block` and
  `a_fragment_does_not_evade_a_port_redirect` failed before and pass now.

### F11. The trunk listener before authentication (security, peer)

- Where: `src/supervisor/global.rs:901-917` (`bind_peer_listener`), and
  `src/supervisor/mod.rs:154-156` (bound on `0.0.0.0` whenever a `psk` is
  set).
- Why it is a fault: every accepted connection gets a task and is held for
  up to 5 seconds (`trunk_auth::TIMEOUT`) before the peer has proved
  anything. There is no limit on connections in total or per address. Anyone
  who can reach the trunk port can hold the supervisor at its descriptor
  limit by opening connections and staying silent. At that limit
  `listener.accept()` fails at once with `EMFILE`, and the loop's answer is
  `continue` with no pause, so the supervisor also spins at full CPU. While
  it is at the limit the supervisor cannot accept a CLI or lab daemon
  connection on its control socket, and cannot start a lab. The port forward
  listener has the same loop shape with a `warn!` in it
  (`src/net/nat/forward.rs:72-79`), which also writes one log line per
  iteration.
- Proof: steps, because the descriptor limit is per process. Run a test
  binary as a child with `RLIMIT_NOFILE` set to 64. In it, bind the listener
  with `bind_peer_listener`, open 80 client connections that send nothing,
  and measure the CPU time of the process over one second
  (`getrusage`). Assert it is under 100 ms and that a connection made after
  the first batch timed out still gets the `VMLABTRUNK2` greeting. Today the
  accept loop uses the whole second.
- Fixed in card #479. `bind_peer_listener` counts the connections that have
  not finished the handshake in `PreAuth` (`src/supervisor/global.rs`): at
  most 32 in total and 8 from one address. A connection past a limit is
  closed at once, with no task and no greeting, and a slot comes back when
  the handshake ends either way. A refused peer dials again after its redial
  delay. After a failed `accept` the loop waits 100 ms and logs the first
  failure of a run only (`AcceptErrors`, `src/net/accept.rs`), and the port
  forward listener in `src/net/nat/forward.rs` does the same. Tests in
  `src/supervisor/global.rs`:
  `silent_connections_do_not_take_the_process_to_its_descriptor_limit` (the
  proof above, with the 80 connections made by the parent, because they
  would count against the child's own limit),
  `a_listener_at_the_descriptor_limit_waits` (a child with three free
  descriptors, which reaches `EMFILE` under the new limits too),
  `a_silent_address_holds_only_its_share_of_the_listener`,
  `silent_connections_are_limited_in_total` and
  `an_authenticated_connection_gives_its_slot_back`. Not changed: the limits
  are per address, so an attacker with five addresses still holds all 32
  slots for as long as it keeps opening connections.

### F12. A dialed bridge outlives its segment (wrong result)

- Where: `src/supervisor/global.rs:649-651` (`detach` aborts the dialer
  task), `:835-841` (the dialer task awaits the bridge's `JoinHandle`),
  `:732-759` (`bridge_tcp_to_switch`), and `:63-65` (the comment that states
  the opposite).
- Why it is a fault: aborting a task that awaits a `JoinHandle` drops the
  handle. Dropping a `JoinHandle` detaches the task, it does not stop it. So
  after the last lab detaches from a segment that dialed a peer, the bridge
  tasks keep running. They hold the TCP connection, the old `Switch` and its
  port. The remote supervisor still counts the trunk as active. When a lab
  attaches again, the supervisor makes a new segment and a new dialer. The
  remote refuses the new dial with `NO\n`, because it already has a trunk
  from that address (`peer_slot_free`). The old connection never ends by
  itself, because keepalives are answered. So after one `vmlab down` and
  `vmlab up` on the dialing host, the two halves of the segment are cut off
  from each other until one supervisor restarts. Each cycle also leaks one
  connection, two tasks and one switch.
- Proof: extend `two_instances_bridge_and_reconnect` in
  `src/supervisor/global.rs`, or add
  `a_trunk_forms_again_after_the_dialer_detaches`: attach on A with `peer`,
  wait until both are connected, `a.globals.detach("wan", None)`, attach on A
  again with the same `peer`, then write a broadcast frame on A's trunk
  socket and read it on B's. Today the read times out, and
  `wait_connected(&a.globals, "wan", true)` never returns.

### F13. A cross-host segment has two subnets, DHCP servers and gateways (wrong result)

- Where: `src/supervisor/global.rs:702` (`accept_peer` attaches with no
  subnet), `:566-592` (every `attach` that creates a segment allocates a
  subnet and spawns a gateway with DHCP and DNS), and `:572`
  (`gateway_mac("__global", name)`, the same MAC on every host).
- Why it is a fault: the dialing host's segment has the subnet its lab
  declared, or the next free index of its own pool. The accepting host
  creates its half with the next free index of *its* pool, without knowing
  the other side's choice. Both halves then run a DHCP server, and a
  DISCOVER is a broadcast that crosses the trunk, so both answer.
  - When the subnets differ, guests on one wire get addresses, gateways and
    DNS servers from two subnets, depending on which answer arrives first.
  - When the subnets are the same, the two gateways have the same IP and the
    same MAC. The MAC moves between the local gateway port and the trunk
    port in each switch with every frame either gateway sends, so a guest's
    frames reach one gateway or the other by timing. The two lease tables
    are separate, so a host that joins later offers addresses the other
    side has already leased.
  PRD §9.2 says the supervisor runs the shared segment's DHCP and DNS "so
  registrations span labs coherently", and that a wire can span hosts. It
  does not say which supervisor serves a wire that spans two.
- Proof: test `a_bridged_segment_has_one_subnet` in
  `src/supervisor/global.rs`. On B, attach and detach a segment `other`
  first, so B's pool index is 1. On A, attach `wan` with `peer` set to B.
  Wait until connected. Assert `a.globals.list()[0]["subnet"] ==
  b.globals.list()[0]["subnet"]`. Today A has `10.214.0.0/24` and B has
  `10.214.1.0/24`. Second assertion: a DHCP DISCOVER written on A's trunk
  socket gets exactly one OFFER. Today it gets two.

### F14. A global subnet index is never reused (wrong result)

- Where: `src/supervisor/global.rs:519-532` (`alloc_subnet`).
- Why it is a fault: `next_index` only counts up. Destroying a segment gives
  nothing back. The pool `10.214.0.0/16` has 256 /24 subnets, so the 257th
  creation of a global segment without a declared subnet fails with "global
  subnet pool exhausted", even when no segment exists at that moment. A
  global segment is destroyed at the last detach and created again at the
  next attach, so a lab with one global segment reaches this after 256
  `up` and `down` cycles within one supervisor life. An authenticated trunk
  peer reaches it in seconds by naming 256 segments. The lab daemon logs the
  failure as a warning and continues (`src/labd/mod.rs:134-148`), so the
  visible result is a lab whose global NICs have no DHCP and no neighbours. A
  declared subnet is also not checked against the allocated ones, so a
  declared `10.214.0.0/24` and the first automatic segment overlap.
- Proof: test `a_destroyed_segment_gives_its_subnet_back` in
  `src/supervisor/global.rs`: 257 times, `attach("s", None, None, None)` then
  `detach("s", None)`. Assert every attach is `Ok`. Today the 257th is an
  error.

### F15. A failed global attach leaves state behind (wrong result)

- Where: `src/supervisor/global.rs:550-566` (existing segment: `refcount +=
  1` and `join` run before `require_psk()?`), and `:579-609` (new segment:
  `Gateway::spawn`, `spawn_dns_sync` and `listen_unix` run before
  `require_psk()?`).
- Why it is a fault: `require_psk` fails when the host config has no `psk`
  and the attach names a `connect` peer. On an existing segment the function
  has by then counted the reference and admitted the lab's members and
  reservations. The caller got an error, so it never detaches. The segment
  can never reach a count of zero and is never destroyed, and the lab's
  names stay in its DNS. On a new segment the gateway handle is dropped (its
  task is aborted), but the DNS sync task, the unix listener task, the
  socket file and the switch stay, and the subnet index is spent (F14). The
  early return after a failed `listen_unix` (`:596-599`) leaves the gateway
  and DNS sync task of that attempt the same way.
- Proof: test `a_refused_attach_leaves_nothing` in
  `src/supervisor/global.rs`. Build `GlobalSegments::new_at` with `psk =
  None`. `attach("wan", None, None, None)` succeeds. `attach("wan", None,
  Some("127.0.0.1:1".into()), None)` fails. `detach("wan", None)` once.
  Assert `list()` is empty. Today it holds `wan` with `refcount` 1. Second
  case: on a fresh instance the failing attach alone, then assert the file
  `<sock_dir>/wan.sock` does not exist. Today it exists.

### F16. Every machine start leaves a NIC listener behind (wrong result)

- Where: `src/labd/network.rs:89-109` (`listen_nic` pushes the task handle
  into `listeners`), `:64` (the field), `src/net/switch.rs:384-411`
  (`listen_unix_with_config`), and `src/labd/machine.rs:900-927`
  (`attach_all_nics` runs at every start).
- Why it is a fault: each start of a machine binds a new listener for each
  NIC socket path. `listen_unix_with_config` unlinks the old socket file, but
  the old listener task is never aborted: nothing outside the tests reads
  `listeners` (the only `drain` is at `src/labd/network.rs:757`, in a test).
  The old task waits in `accept` on a socket nobody can reach, and holds its
  descriptor and a reference to the switch until the daemon ends. A lab
  daemon that restarts machines in a loop (a scripted test run, `vm restart`
  in CI) loses one descriptor per NIC per start. With the common soft limit
  of 1024 descriptors a four-NIC machine reaches it after about 250
  restarts, and the daemon then fails as in F3. The same leftover exists
  when `attach_all_nics` fails at the second NIC: the first NIC's listener
  stays.
- Proof: test `a_second_attach_replaces_the_first_listener` in
  `src/labd/network.rs`, next to the existing attach test at `:721-760`:
  call `attach_nic` twice with the same socket path. Assert the segment
  holds one listener, or that the first handle `is_finished()`. Today it
  holds two live ones.

### F17. A sinkhole covers only A, AAAA and ANY (wrong result)

- Where: `src/net/dns.rs:515-518` (any other query type is forwarded
  upstream without looking at the zone's answer), and `:479-484` (the same
  for a non-query opcode or a class other than IN).
- Why it is a fault: PRD §9.9 says a DNS rule sinkholes "a name". The code
  applies the rule to three query types. A query for the HTTPS (65) or SVCB
  (64) record of a sinkholed name goes to the upstream resolver and comes
  back with the real answer, which can carry address hints. Current browsers
  and operating systems ask for HTTPS records before or with A. CNAME, TXT,
  SRV and MX go the same way. So a lab that sinkholes
  `*.telemetry.example.com` to watch a client fail can see it succeed. The
  same branch forwards every non-address query for the lab's own names
  (`dc01.lab.vmlab.internal`) to the host's upstream resolver, which tells
  that resolver the lab's machine names.
- Proof: test `a_sinkhole_covers_every_query_type` in `src/net/dns.rs`: a
  zone with an upstream and `add_sinkhole("*.blocked.test", Nxdomain)`. Send
  a query of type 65 for `a.blocked.test`. Assert the action is a `Reply`
  with rcode NXDOMAIN. Today it is `ForwardUpstream`.

### F18. A connection to the gateway address is dialed on the host's network (wrong result)

- Where: `src/net/gateway.rs:434-449` (TCP to the gateway's own IP falls
  through to the uplink), `src/net/nat/mod.rs:236-241` (a SYN with no flow
  is a passive open whatever its destination), and `src/net/nat/vtcp.rs:237`.
- Why it is a fault: a guest's SYN to its gateway address on a port that
  nothing serves (say `10.213.0.1:22`) has no flow in the table, so the
  engine treats it as internet egress and calls
  `TcpStream::connect(("10.213.0.1", 22))` on the host. The host does not
  own that address. The connection leaves through the host's default route.
  On a network where that private address exists (a company network that
  uses `10.213.0.0/16`), the guest is connected to that unrelated machine
  while it believes it talks to its gateway. Elsewhere the guest waits 10
  seconds for a RST where it should get one at once. UDP to the gateway
  address is handled (`handle_udp` returns), TCP is not.
- Proof: test `a_syn_to_the_gateway_itself_is_reset` in
  `src/net/nat/tests.rs`: send a guest SYN to `gw_ip:22` with no flow.
  Assert a RST arrives on the output channel within 100 ms. Today nothing
  arrives until the host connect fails or times out.

### F19. A DHCP option longer than 255 bytes (wrong result)

- Where: `src/net/dhcp.rs:437-442` (`opt`: a `debug_assert!`, then
  `val.len() as u8`), `:405-409` (option 121), and `:391-393` (option 15).
- Why it is a fault: an option's length field is one byte. Option 121 takes
  up to 9 bytes per route, so 29 routes of a /32 (or more of shorter
  prefixes) pass 255 bytes. The count is the segment's `routes {}` plus one
  per `routes_to` peer plus the default route. A release build then writes
  the low byte of the length and all of the value, so the client reads the
  rest of the value as further options, and usually loses the default route
  with it. A debug build panics in the gateway task. The domain option has
  the same shape with a long lab name and DNS suffix. Nothing in validation
  limits the number of routes.
- Proof: test `many_routes_make_a_well_formed_reply` in `src/net/dhcp.rs`:
  a config with 40 /32 routes, one DISCOVER. Parse the reply's options and
  assert the walk ends exactly at option 255 and finds option 53. Today a
  debug build panics, and a release build fails the walk.

### F20. A lab segment's DNS keeps the name of an ended lease (wrong result)

- Where: `src/labd/network.rs:626-656` (`spawn_lease_dns_sync` only calls
  `register`).
- Why it is a fault: the sync task adds a name when it sees a new lease and
  never removes one. A machine whose lease was released or has expired keeps
  resolving to its last address. When DHCP gives that address to another
  machine, `<old>.<lab>.<suffix>` resolves to the new machine. The global
  segment's sync does unregister (`Directory::sync`,
  `src/supervisor/global.rs:382-392`), so the two kinds of segment differ.
  With F8 a guest can cause this on purpose.
- Proof: test `a_released_lease_leaves_dns` in `src/labd/network.rs`: wire a
  gateway as the test at `:825` does, lease an address to a named MAC, wait
  for the name to resolve, send a RELEASE, wait two sync periods. Assert the
  name no longer resolves to that address. Today it still does.

## Notes

These have no test that proves them, or they are not faults by themselves.

- N1. PSK guessing offline. In the trunk handshake the client proves itself
  first (`src/supervisor/trunk_auth.rs:44-51`). A host that a supervisor
  dials (a wrong `connect` address, or a host that took the address) gets an
  HMAC over known values under the PSK without knowing the PSK, and can test
  guesses against it at any rate. A short PSK does not survive that. The
  module comment already says the frames themselves are plaintext and need a
  trusted network.
- N2. The worktree host guard fails open. `host_addresses`
  (`src/net/nat/guard.rs:64-69`) returns an empty set when `getifaddrs`
  fails, and logs a warning. The guard then refuses only loopback and
  `0.0.0.0/8`, and a worktree guest reaches the host's LAN addresses.
- N3. Multicast under the guard. `refuses` does not cover `224.0.0.0/4`. A
  UDP datagram from a worktree guest to a multicast group is sent by the
  host, and the host's own listeners on that group receive it. Replies come
  from a unicast address and the connected socket drops them, so this is one
  way only.
- N4. The helper gives any local user kernel objects without a quota. Each
  `vmlab-fastpath` process allows 64 engines of 256 taps
  (`src/net/fastpath/helper/proto.rs`), and nothing limits the number of
  helper processes a user starts. This is the design of F21 as PRD §9.1.1
  describes it. F2 is the part that breaks a stated invariant.
- N5. `XdpRelease` leaves the devmap slot. `XdpEngine::remove_port`
  (`src/net/fastpath/engine.rs:80-82`) removes `PORT_CONF[ifindex]` and not
  `PORTS[tag]`. While the hypervisor still holds the tap, a frame the daemon
  injects with that tag still reaches the old tap. The tag is reused by the
  next NIC, which overwrites the slot.
- N6. A unix NIC listener ends on the first `accept` error
  (`src/net/switch.rs:404-407`). After one `EMFILE` the socket file still
  exists, but no hypervisor can connect that NIC again until the machine is
  attached again.
- N7. DHCP leases live only in memory. If a lab daemon is restarted while
  its guests run, the new daemon can lease an address a running guest still
  holds, until that guest renews and gets a NAK. I did not check whether a
  daemon restart with running guests is possible.
- N8. The UDP port forward serves all remotes from one loop
  (`src/net/nat/forward.rs:129-175`). One remote whose 64-slot channel is
  full blocks datagrams from every other remote. A LAN host that sends from
  16 384 source ports uses every ephemeral gateway port for that guest port
  for 60 seconds.
- N9. `Ipv4View::parse` (`src/net/frame.rs:249-265`) does not verify the
  header checksum, and no caller does. A corrupt header is processed as
  written.
- N10. The global segment's lease check in `Directory::admit` reads leases
  once (`src/supervisor/global.rs:419`). A lease granted between that read
  and `sync_dhcp` can collide with a reservation admitted in the same call.

## What I read and what I did not

Read in full, without the test modules:

- `src/supervisor/trunk_auth.rs`, `src/supervisor/global.rs`
- `src/net/switch.rs`, `src/net/framing.rs`, `src/net/gateway.rs`,
  `src/net/dhcp.rs`, `src/net/dns.rs`, `src/net/rules.rs`,
  `src/net/router.rs`
- `src/net/nat/mod.rs`, `src/net/nat/guard.rs`, `src/net/nat/udp.rs`,
  `src/net/nat/icmp.rs`, `src/net/nat/forward.rs`
- `src/labd/network.rs`, `src/labd/netservices.rs`
- `src/net/fastpath/helper/mod.rs`, `helper/proto.rs`, `helper/server.rs`,
  `src/net/fastpath/engine.rs`, `src/net/fastpath/afxdp.rs`,
  `src/net/fastpath/mod.rs`, `src/fastpath_helper.rs`
- `ebpf/xdp-switch/src/main.rs`

Read in part:

- `src/net/nat/vtcp.rs`: the passive open, the active open, `on_segment` and
  the proxy loop. Not the sequence arithmetic, the retransmission code or
  the out-of-order queue.
- `src/net/frame.rs`: the `parse` functions. Not the builders or the
  checksum code.
- `ebpf/fastpath-logic/src/lib.rs`: `classify_frame` and
  `classify_xdp_guest`. Not `step_skb`, the stream framing state machine of
  the sockmap tier.
- `tap/src/lib.rs`: the creation flags only.
- `src/labd/machine.rs` (`attach_all_nics`), `src/labd/lab.rs`
  (`install_forwards`, the first 70 lines), `src/labd/mod.rs` (the global
  attach and teardown call sites), `src/paths.rs` (the socket paths).

Not read:

- `src/labd/forward_plan.rs` and `src/labd/isolation.rs`: the plans for port
  forwards and worktree isolation as values. The faults above are in what
  runs the plans.
- `src/net/fastpath/sockmap.rs`, `probe.rs`, `stub.rs`, `helper/client.rs`,
  `priv_tests.rs`, and `ebpf/fastpath-sockmap/src/main.rs`. `auto` never
  selects the sockmap tier.
- The virtio-net, e1000 and pcnet devices of vmlab-hv
  (`hv/crates/vmlab-hv-devices/src/nic/`) and the QEMU netdev arguments
  (`src/qemu/cmdline.rs`). `docs/reviews/virtio-devices.md` covers
  virtio-net.
- `src/smb/` and the gateway redirect that serves it, beyond the rule
  engine it uses.
- Every test module, and the e2e `fastpath` scenario.
