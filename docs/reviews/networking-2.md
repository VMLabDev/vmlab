# Second review: networking (lab networks, port forwards, trunks, fast path)

Card #891. A second bug-hunting pass over the lab network stack. It lists
faults and makes no fix. Reviewed at `next` commit `b9fd1de4`. All paths are
from the repository root, and all line numbers are from that commit.

The first pass is `docs/reviews/networking.md` (card #439, findings F1 to F20,
notes N1 to N10). This document continues its numbering, so a code names one
finding across both: F21 to F33 and N11 to N18 are new here. A finding of the
first pass is named again only where its fix is incomplete or brought a new
fault in, and the text says which.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped. No test below uses a wall-clock limit: where
it has to show that a frame does not arrive, it sends a second frame after it
and reads that one, or it reads a counter or a table.

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**. A finding that lets a guest or
a peer take a service away from others is filed under security, and its text
says that the effect is a denial of service.

Who the attacker is, per finding:

- **guest**: code running inside a VM or container on a lab segment. It can
  send any ethernet frame on its NIC.
- **peer**: another host on a cross-host trunk. An *authenticated* peer holds
  the PSK. An *on-path* peer can change the trunk's TCP stream, which carries
  plain frames after the handshake (`src/supervisor/trunk_auth.rs:1-3`).
- **LAN host**: a machine that reaches a port forward declared with a `bind`
  other than loopback.
- **local process**: a process of the user that runs vmlab.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F21 | security (guest) | A DNS reply is sent from the ethernet destination of its query: a guest makes the gateway port claim any machine's MAC, and the switch then drops that machine's frames for good (brought in by the fix of F9) |
| F22 | security (guest, peer) | A MAC learned on a trunk port is never given back: one frame from another lab or host with a guest's source MAC cuts that guest off for good (brought in by the fix of F9) |
| F23 | security (guest) | The vTCP out-of-order queue holds 65 535 one-byte segments per flow and is scanned in full for each one it takes: a guest keeps the lab daemon's worker threads busy |
| F24 | security (peer, local process) | One frame of 65 551 to 65 554 bytes ends the writer task of every vmlab-hv NIC it is delivered to, and those NICs receive nothing until their machine restarts |
| F25 | wrong result | The accepting half of a cross-host segment has no DHCP and no DNS after the dialing host leaves, until its own labs detach (brought in by the fix of F13) |
| F26 | security (guest) | `isolated = true` does not hold on a global segment: the NIC exchanges frames with every guest of the other labs and hosts |
| F27 | security (guest) | A guest that takes a DHCP lease for an address another machine uses without a lease makes the gateway drop that machine's frames (brought in by the fix of F9) |
| F28 | security (LAN host, local process) | A TCP port forward takes connections without limit: each is a descriptor and a task of the lab daemon |
| F29 | security (guest) | A guest on a non-isolated port still speaks as the gateway to the other guests: ARP for the gateway address and DHCP server replies are flooded as sent (F9 fix incomplete) |
| F30 | security (guest) | On a global segment the gateway's MAC is not held: the supervisor's switch has only service ports, and `bind_mac` holds a MAC against guest ports alone (F9 fix incomplete) |
| F31 | wrong result | NAT dials an address of its own segment or of another segment of the lab on the host's network (F18 fix covers the gateway address only) |
| F32 | wrong result | A SYN on a live vTCP flow gets no answer: a guest that reuses a port pair after a reboot waits out the idle timeout |
| F33 | wrong result | A sinkholed name is reached through a CNAME: the upstream's answer is relayed without a look at the names in it |

### F21. A DNS reply claims the MAC its query was sent to (security, guest)

- Where: `src/net/dns.rs:484-490` (`server_mac: eth.dst_mac()`), `:388-403`
  (`build_frame` uses it as the ethernet source), `src/net/gateway.rs:491-496`
  (the gateway answers a query by its UDP port and IP destination, whatever
  its ethernet destination), `src/net/switch.rs:633-655` (a frame from the
  gateway's service port teaches the switch its source MAC), and `:614-623`
  (a guest frame whose source MAC a service port holds is dropped).
- Why it is a fault: every other frame the gateway originates carries the
  gateway MAC as its source. A DNS reply carries the ethernet destination of
  the query. A guest chooses that field. The switch floods a query to an
  unknown unicast MAC, so the gateway receives it and answers from that MAC.
  The switch learns the MAC on the gateway's port, which is a service port.
  From then on the rule of the F9 fix drops every frame the real owner of
  that MAC sends, because "a MAC a service port holds is not a guest's to
  send from". Nothing ages the entry out: it stays until 4 096 other MACs
  were learned on the gateway port or the lab daemon restarts. The owner
  loses DHCP, DNS, NAT and every neighbour.
  The MAC has to be unknown to the switch when the query is sent. The
  attacker gets there by itself: it sends one frame with the victim's MAC as
  source, which moves the MAC to its own port (a move between two guest
  ports is allowed), then 256 frames with new source MACs, which push the
  victim's MAC out of its port's 256 entries (`MAX_MACS_PER_GUEST_PORT`,
  `:122`). The victim wins the MAC back only if it sends a frame between
  those 257 frames and the query. `isolated = true` on the attacker's NIC
  does not help: all of this is traffic between the guest and the gateway.
  A query to the broadcast MAC is answered from the broadcast MAC, which no
  switch learns and a guest's stack may drop.
- Proof: test `a_dns_reply_comes_from_the_gateway_mac` in
  `src/net/gateway.rs`. Wire a gateway with a DNS zone that holds one name
  and two guest ports A and B. From A, send a query for that name with the
  ethernet destination `X`, a unicast MAC that nothing uses. Read the reply
  on A. Assert its ethernet source is the gateway MAC. Today it is `X`.
  Second part: from B send a broadcast frame with source `X`, then one with
  B's own MAC. Assert A reads the frame from `X` first. Today A reads only
  the second, and `stats().frames_dropped` went up by one.

### F22. A MAC learned on a trunk is never given back (security, guest and peer)

- Where: `src/net/switch.rs:633-655` (a frame that enters on a service port
  moves its source MAC there from any port), `:614-623` (the guard that then
  drops the guest), `src/labd/network.rs:726-728` (a lab's trunk to the
  supervisor is a service port), and `src/supervisor/global.rs:723` and
  `:925` (every port of a global segment's switch is a service port).
- Why it is a fault: before the F9 fix a MAC moved to the port that sent it
  last, so a machine won its MAC back with its next frame. The fix made a
  MAC on a service port final for guests. It did not limit how a MAC gets
  onto a service port. On a global segment the lab's switch has one service
  port that it does not control: the trunk, behind which are the guests of
  every other lab and host. One frame that arrives over the trunk with the
  source MAC of a local guest moves that MAC to the trunk port, and every
  later frame of the local guest is dropped. A guest of another lab sends
  that frame when its own lab's switch has not yet seen the victim's MAC on
  its trunk (a lab that came up after the victim's last broadcast: DHCP
  renewals and most traffic to the gateway are unicast). The supervisor's
  switch passes it on, because there every port is a service port and the
  guard never applies. An on-path peer of a cross-host trunk writes the
  frame into the plain TCP stream at any time, and an authenticated peer
  sends it as a matter of course. The victim is cut off until the trunk
  port has learned 4 096 other MACs or its lab daemon restarts.
- Proof: test `a_service_port_does_not_keep_a_guests_mac` in
  `src/net/switch.rs`. Guest ports A and B, and one service channel port T.
  A sends a broadcast from MAC `X`. T sends a broadcast with source `X`. A
  sends a second broadcast from `X`, then B's reader is given a marker frame
  from T. Assert B reads A's second broadcast before the marker. Today B
  reads the marker only, and `frames_dropped` went up by one.

### F23. The vTCP out-of-order queue: 65 535 entries, scanned per entry (security, guest)

- Where: `src/net/nat/vtcp.rs:659-678` (`queue_ooo` limits the queue in
  bytes, `OOO_MAX_BYTES`, and not in segments), `:620-628` (any segment
  ahead of `rcv_nxt` is queued, however far ahead), and `:699-731`
  (`drain_ooo`: each turn of the loop walks the whole map for stale entries
  and then takes one segment).
- Why it is a fault: a guest sends 65 534 segments of one byte each at
  `rcv_nxt + 1`, `rcv_nxt + 2` and so on. Each becomes a map entry with its
  own allocation, a few megabytes per flow, and one guest address may hold
  128 flows (`MAX_TCP_FLOWS_PER_GUEST`). The guest then sends the one byte
  at `rcv_nxt`. `drain_ooo` takes the queued segments one per turn, and each
  turn filters all remaining entries, so the call does about 65 535² / 2,
  two thousand million, entry visits without yielding. That is seconds of
  one tokio worker thread for one small frame. The guest repeats it on each
  of its flows, with about 4 MiB of frames per round, and the lab daemon's
  worker threads stay busy: QMP, agent channels, the other segments' gateways
  and the control socket are served late or not at all. The receive window
  does not prevent it, because the queue takes a segment anywhere in the
  sequence space ahead of `rcv_nxt`.
- Proof: test `the_out_of_order_queue_is_bounded_in_segments` in the `unit`
  module of `src/net/nat/vtcp.rs`. Build a passive `Flow`, call
  `queue_ooo(rcv_nxt + 1 + i, &[0])` for `i` in `0..60_000`, and assert
  `flow.ooo.len()` is at most a stated limit of segments. Today it is
  60 000. A second assertion for the cost: count the entries `drain_ooo`
  visits (a counter behind `cfg(test)`) after the byte at `rcv_nxt` arrives,
  and assert it is linear in the queue length.

### F24. One oversized frame ends a vmlab-hv NIC's writer (security, peer and local process)

- Where: `src/net/framing.rs:69` (`MAX_FRAME_LEN` is 65 536 + 14 + 4 for a
  raw port), `hv/crates/vmlab-hv-proto/src/net.rs:10` and `:88` (a frame in
  vnet framing may be 65 536 + 14 bytes, and `validate` refuses a longer
  one), `src/net/framing.rs:107` (`write_packet` validates on the way out),
  `src/net/switch.rs:319-328` (the writer task of a stream port ends on the
  first write error), and `:572-590` (`deliver` checks no length).
- Why it is a fault: a raw port accepts a frame four bytes longer than a
  vnet port can be sent. `deliver` queues such a frame for a vnet port, the
  port's writer task fails in `validate`, logs at debug level and ends. The
  port stays in the switch with a closed queue, so every later frame for
  that NIC is dropped, while the NIC still sends. The machine has to be
  restarted to receive again. A broadcast frame does this to every vmlab-hv
  virtio-net NIC of the segment at once. Raw ports that can carry such a
  frame: a cross-host trunk (`read_frame` in
  `src/supervisor/global.rs:943`), so an authenticated or on-path peer, and
  the unix sockets of a global segment and of a NIC, so a local process. I
  did not check whether a guest behind QEMU's stream netdev can make its
  device emit a frame that long. The more general fault is that one frame a
  port cannot be sent ends the port and is not dropped by itself.
- Proof: test `an_oversized_frame_does_not_end_a_vnet_ports_writer` in
  `src/net/switch.rs`, next to the vnet tests at `:1229`. One raw stream
  port and one stream port with `FramingMode::Vnet { queue_pairs: 2 }`.
  Write a broadcast frame of 65 552 bytes to the raw port, then a broadcast
  frame of 60 bytes. Assert the vnet port's peer reads the second frame.
  Today that read never completes. For an assertion that fails at once:
  `stats().frames_dropped` is 1 after the first frame. Today it is 0,
  because the frame was queued and the writer died on it.

### F25. The accepting half loses DHCP and DNS when the dialer leaves (wrong result)

- Where: `src/supervisor/global.rs:869-879` (`accept_peer` takes the
  segment's own services down and aborts its dialer), `:880` (it takes the
  peer's subnet), `:888-903` (the waiter only clears the slot and gives the
  reference back when the bridge ends), `:139-144` and `:169-176` (the
  `upstream` owner is kept across a disconnect), and `:575-590`
  (`start_dialer` does nothing while `services` is `None`).
- Why it is a fault: the fix of F13 (card #816) makes the accepting
  supervisor bridge to the dialer's gateway. It does so by dropping its own
  gateway, DHCP server and DNS zone, and nothing ever starts them again.
  When the lab on the dialing host goes down, that host destroys its segment
  and its dialer, and the trunk closes. The accepting host still has its own
  labs on the segment, so the segment stays, with no gateway, with the other
  host's subnet, and with no dialer of its own. Its guests get no DHCP
  answer and no DNS answer until every local lab has detached and one
  attaches again. A plain `vmlab down` on one host therefore takes the
  shared segment's services away from the other. Before the fix that host
  kept serving. The comment at `:142-143` shows the kept owner is meant for
  a peer that comes back. A peer that does not is the common case.
  The same code lets an authenticated peer do this to any global segment of
  the host on purpose, also one that declares no `connect`: it dials the
  segment's name with a subnet of its choice and hangs up.
- Proof: test `a_segment_serves_again_after_its_upstream_peer_leaves` in
  `src/supervisor/global.rs`, built like `a_bridged_segment_has_one_subnet`
  (`:1492`). Attach a lab to `wan` on B. Attach `wan` on A with `peer` set
  to B and wait until both are connected. Detach on A and wait for B's
  `segment.peer.down` event. Write a DHCP DISCOVER on B's trunk socket and
  assert an OFFER comes back. Today none does. The assertion without a
  wait: B's segment has `services.is_some()` after the event.

### F26. Port isolation ends at a trunk (security, guest)

- Where: `src/net/switch.rs:178-184` (`delivery_allowed`: an isolated guest
  port exchanges frames with service ports), `src/labd/network.rs:726-728`
  (the trunk of a global segment is a service port), and
  `src/supervisor/global.rs:723` (so is every port of the supervisor's
  switch).
- Why it is a fault: PRD §9.1 says of `isolated = true`: the NIC reaches the
  gateway services, NAT, port forwards and daemon routing, "but never
  neighbouring guests. Works on any segment, built-in or declared." On a
  global segment the isolated NIC's frames go to the trunk, as to any
  service port. The supervisor's switch floods or forwards them to the
  other labs' trunks and to cross-host peers, and each lab's switch delivers
  a frame from its trunk to all its guests, isolated or not. So an isolated
  NIC on a global segment talks to every guest of that segment outside its
  own lab daemon, in both directions. Only the guests of its own lab are
  kept from it. Nothing in validation refuses `isolated` on a global
  segment or warns about it.
- Proof: test `an_isolated_nic_reaches_no_guest_behind_a_trunk` in
  `src/net/switch.rs`. Two switches joined as a lab's switch and the
  supervisor's are: a channel service port on each, with a task that copies
  frames both ways. An isolated guest port on the first sends a broadcast. A
  guest port on the second is then sent a marker frame from a service port.
  Assert the guest port reads the marker first. Today it reads the
  isolated port's broadcast first.

### F27. A lease for an address in use makes the gateway drop its user (security, guest)

- Where: `src/net/dhcp.rs:348-353` (a client with no lease is granted any
  address that `requestable` allows), `:443-449` (`requestable` knows
  reservations, leases and declines, not the addresses in use),
  `:323-325` (an OFFER holds its address as a lease for 60 s), `:276-282`
  (`leased_to_another`), and `src/net/gateway.rs:440-444` and `:473-475`
  (an ARP or IPv4 frame whose source address is leased to another MAC is
  dropped).
- Why it is a fault: the fix of F9 treats a lease as proof of who owns an
  address. An address that a machine uses without a lease has no such
  proof: an address set inside the guest and not declared on its `nic`, a
  second address on a NIC, a virtual address that moves between cluster
  nodes, or an address whose lease ran out while the guest was paused. A
  guest sends one DHCP REQUEST that names such an address. It gets an ACK,
  and from then on the gateway drops every ARP and IPv4 frame the real user
  sends from that address: no DNS, no NAT, no routing, no answer to ARP for
  the gateway. Before the fix the two machines had an address conflict, and
  the gateway served both. `isolated = true` does not prevent it. The same
  happens without intent for 60 seconds whenever the pool's next free
  address is one in such use and a new client sends a DISCOVER, and for the
  whole lease when that client does not probe the address before it takes
  it.
- Proof: test `a_request_cannot_take_an_address_another_mac_uses` in
  `src/net/gateway.rs`. MAC S sends an ARP request for the gateway with the
  sender address `X`, a pool address with no lease, so the gateway holds `X`
  at S in its neighbour table. MAC B sends a DHCP REQUEST with requested
  address `X`. Assert the reply is a NAK. Today it is an ACK. Second part:
  S sends an echo request to the gateway from `X`, then B sends one from its
  own address. Assert the first echo reply read on S's port answers S.
  Today S gets none.

### F28. A TCP port forward has no limit on connections (security, LAN host and local process)

- Where: `src/net/nat/forward.rs:73-94` (one task per accepted connection,
  no count), `src/net/nat/mod.rs:523-544` (an active open takes one of
  16 384 gateway ports per guest port), and `:173-176` (the limits of the
  F3 fix count guest-opened flows only, as its text says).
- Why it is a fault: the fix of F3 bounded what a guest makes the engine
  hold. The other side of the engine is open. Each connection to a forwarded
  port is a host socket, a task, a 64 KiB pipe and a vTCP flow for as long
  as the peer keeps it, or for 6 seconds when the guest does not answer.
  The only limit is the ephemeral range, 16 384 flows per guest port, which
  is far past the descriptor limit of the lab daemon (nothing in `src/`
  raises `RLIMIT_NOFILE`). At that limit the daemon cannot open a QMP
  socket, an agent socket or a disk, as in F3. Whoever reaches the forward
  can do it: a local process for the default loopback bind, and any host
  that reaches the address for a forward declared with `bind = "0.0.0.0"`.
  The trunk listener got such a limit in the fix of F11, and the forward
  listener got only the pause after a failed `accept`.
- Proof: test `forwarded_connections_are_bounded` in
  `src/net/nat/tests.rs`. Spawn a TCP forward to a guest address that never
  answers. Open 2 000 connections to it. Assert
  `engine.tcp_flows.lock().len()` is at most a stated limit after each one.
  Today it reaches 2 000, or the test process reaches its own descriptor
  limit first.

### F29. A guest still answers as the gateway (security, guest)

- Where: `src/net/switch.rs:657-703` (ARP and DHCP frames from a guest port
  are flooded as sent), `src/net/gateway.rs:432-461` (the gateway checks an
  ARP against the leases for its own table only), and
  `src/labd/netservices.rs:68-99` (the only ingress hook looks at IPv4 to
  the gateway MAC).
- Why it is a fault: F9 was that a guest takes over the gateway's traffic.
  Its fix holds the gateway's MAC in the switch. It does not hold the
  gateway's address. A guest on a non-isolated port broadcasts an ARP reply
  that names the gateway's address at its own MAC, and the other guests
  send their DNS queries and all their off-segment traffic to it. A guest
  that answers a DHCP DISCOVER faster than the gateway (a frame from UDP
  port 67) hands out its own address as router and DNS server. vmlab knows
  both facts that would refuse these frames: the gateway's address, and
  that no guest port is the segment's DHCP server unless the lab says so
  (`nic.gateway`). The effect is that of F9 item 1 for every guest that is
  not isolated. An isolated port cannot do it, and cannot be its victim.
- Proof: test `a_guest_cannot_answer_arp_for_the_gateway_address` in
  `src/net/gateway.rs`. A gateway and two guest ports A and B. A sends a
  broadcast ARP reply with the gateway's address as sender address and its
  own MAC, then a broadcast marker frame. Assert the first frame B reads is
  the marker. Today it is the forged ARP. The same shape with a DHCP OFFER
  from A (source port 67).

### F30. The gateway's MAC is not held on a global segment (security, guest)

- Where: `src/net/switch.rs:400-411` (`bind_mac`), `:614-623` (the guard
  applies to a frame from a guest port only), and
  `src/supervisor/global.rs:723` and `:925` (a global segment's switch has
  service ports only).
- Why it is a fault: F9 item 1 named the global segment as the worst case:
  "the victims are in other labs and on other hosts". The fix does not
  reach it. In the supervisor's switch a frame from any trunk with the
  gateway's MAC as source moves that MAC to the trunk, as between two
  service ports "a MAC still moves". The only protection is the switch of
  the sending guest's own lab, which holds the gateway's MAC on its trunk
  port once a frame from the gateway has crossed that trunk. A guest that
  sends from the gateway's MAC before that (as its first frame after its
  lab came up, without a DHCP exchange before it), or after it pushed the
  entry out of the trunk port's 4 096 with replies it provoked (F21),
  receives the traffic of every lab on the segment to the gateway until the
  gateway next sends a frame.
- Proof: test `a_bound_mac_is_held_against_a_service_port` in
  `src/net/switch.rs`. Service ports G, T1 and T2. `bind_mac(G, mac)`. T1
  sends a frame with source `mac`. T2 sends a frame to `mac`. Assert G
  reads it. Today T1's reader does.

### F31. NAT dials lab addresses on the host's network (wrong result)

- Where: `src/net/nat/mod.rs:333` (only a SYN to `gw_ip` is refused),
  `:355-383` (UDP), `:385-402` (an echo request starts a `ping` on the
  host), and `:98-106` (`NatConfig` holds no subnet).
- Why it is a fault: the fix of F18 stops a SYN to the gateway's own
  address from being dialed by the host. Every other address the host does
  not own is still dialed: an address of the guest's own segment sent to
  the gateway MAC, and any address of another segment of the lab that is
  not connected with `routes_to`. The lab's subnets exist in the lab
  daemon's memory only, so the host's connect leaves by its default route.
  A guest on segment `a` that dials a machine on the unconnected segment
  `b` is therefore connected to whatever holds that private address on the
  host's own network, or waits 10 seconds for nothing, where PRD §9.6 says
  the two segments are isolated. The router returns such a frame as "not
  for a connected peer" (`src/net/router.rs:210-212`) and the engine cannot
  tell it from internet egress.
- Proof: test `nat_does_not_dial_an_address_of_the_lab` in
  `src/net/nat/tests.rs`. Give the engine the lab's subnets (the test needs
  that field to exist). Send a guest SYN to an address of the segment's own
  subnet other than `gw_ip`. Assert the flow table is empty and a RST is on
  the output channel when `handle_frame` returns, as the F18 test does.
  Today the table holds one flow and no RST is sent.

### F32. A SYN on a live flow gets no answer (wrong result)

- Where: `src/net/nat/vtcp.rs:586-653` (`on_segment`: a segment with SYN
  and without ACK and payload changes nothing and is not acknowledged), and
  `src/net/nat/mod.rs:313-327` (a SYN for a key that has a flow goes to that
  flow).
- Why it is a fault: a guest that lost its side of a connection without a
  FIN or RST reaching the engine (a reset or restored VM, a `vm restart`)
  can pick the same source port for the same destination again. Its SYN
  reaches the old flow, which ignores it. A TCP peer answers such a SYN
  with an ACK of what it expects, the guest answers that with a RST, and
  the next SYN opens a new connection. Here the guest retransmits its SYN
  into silence until the old flow's idle timeout of 5 minutes, and the old
  flow keeps its host socket and its slot that long. The same holds for a
  port forward's flow.
- Proof: test `a_syn_on_a_live_flow_is_answered` in `src/net/nat/tests.rs`,
  on a paused clock. Complete a guest handshake to a host listener. Send a
  SYN on the same four values with a new sequence number. Assert the next
  frame on the output channel is an ACK or a RST for that flow. Today the
  next frame is the idle reset, 300 seconds of test clock later.

### F33. A sinkhole is passed through a CNAME (wrong result)

- Where: `src/net/gateway.rs:550-555` (the upstream's answer goes to the
  guest as received), and `src/net/dns.rs:245-262` (the sinkhole is looked
  up for the question's name only).
- Why it is a fault: PRD §9.9 sinkholes a name, and the fix of F17 made
  that hold for every query type. It holds for the name in the question
  only. A query for a name outside the pattern whose answer is a CNAME to a
  name inside it comes back from the upstream resolver with the whole
  chain and the final addresses, and the guest connects. Vendors put
  telemetry and update hosts behind such aliases, so a lab that sinkholes
  `*.telemetry.example.com` to watch a client fail can see it succeed
  through `alias.vendor.example`.
- Proof: test `an_upstream_answer_through_a_sinkholed_name_is_not_relayed`
  in `src/net/gateway.rs`, with a UDP socket on loopback as the upstream.
  Sinkhole `*.blocked.test` as NXDOMAIN. Query `alias.open.test`, and let
  the upstream answer with `alias.open.test CNAME a.blocked.test` and
  `a.blocked.test A 192.0.2.1`. Assert the frame the guest reads holds no
  A record for `192.0.2.1`. Today it is the upstream's answer unchanged.

## Notes

These have no test that proves them here, or they are not faults by
themselves.

- N11. The host's own frames reach a guest behind an afxdp tap. The tap is
  an interface of the host that is up (`tap/src/lib.rs:136-150`), and
  nothing turns IPv6 off on it. XDP filters what the guest sends, not what
  the host's stack sends: router solicitations and multicast listener
  reports from the tap's link-local address, and whatever a host service
  sends on every interface (mDNS with the host's name, a DHCP client of the
  host's network manager). The guest can answer none of it. A test needs
  the capabilities of `src/net/fastpath/priv_tests.rs`: create a tap
  through the helper and assert `/proc/net/if_inet6` has no line for it. I
  did not run it.
- N12. A tap is up before its filter is attached
  (`src/net/fastpath/helper/server.rs:224-235`). No queue descriptor has
  left the helper by then, so only N11 applies in that window.
- N13. An authenticated peer can make the supervisor hold segments without
  limit: each accepted name is a switch, a listener, a socket file and
  tasks (`src/supervisor/global.rs:859-862`). A subnet of `0.0.0.0/0` on
  the first of them makes every later global segment of the host fail the
  overlap check (`:1048-1050`, `:681-703`).
- N14. The source address of a routed packet is not checked against the
  segment it comes from (`src/net/router.rs:198-261`). A guest on one
  segment sends a packet into a connected segment with a source address of
  that segment, or of any other network.
- N15. Packets waiting for ARP are limited to 256 next hops
  (`src/net/gateway.rs:297-302`). A guest on a connected segment that sends
  to 256 addresses nobody holds, every 3 seconds, keeps that table full, and
  a routed packet for a host the gateway has not yet heard from is dropped
  without an ARP request.
- N16. An active open that the guest does not answer fails after 6 seconds
  with "active open task died" (`src/net/nat/vtcp.rs:908-911`, `:297`). The
  comment says the caller's 10 second timeout reports it.
- N17. A guest that sends an MSS option of 0 in its SYN gets a flow that
  never sends it data and probes its window once a second for as long as
  the guest answers (`src/net/nat/vtcp.rs:374`, `:503-511`). It harms only
  itself, and holds one of its own slots.
- N18. A query for a name under the lab's own suffix that the zone does not
  hold is forwarded to the host's upstream resolver
  (`src/net/dns.rs:284-288`), which so learns names of machines that are
  mistyped or not yet up. The UDP forward's receive loop repeats a failing
  `recv_from` without a pause (`src/net/nat/forward.rs:143-149`), which the
  fix of F11 changed for the TCP loop only. Per-machine SMB redirects are
  granted to the whole segment: the guard admits any guest to every
  machine's server port on `127.0.0.1`
  (`src/labd/netservices.rs:113-129`, `src/net/rules.rs:516-524`). What
  that is worth depends on the servers' authentication, which I did not
  read.

## First-pass fixes I checked

Read against the code and found complete as described, apart from what the
findings above say: F1 (`check_segment_name`), F3 (the four limits), F4
(`try_send` and the `to_host` buffer; the NAT output channel is drained by
its own task, so the gateway task cannot block on it for good), F5 (the
loopback guard and its grant), F6 (`BoundedMap` and the per-port MAC limit),
F7 (the connection table limits and sweep), F8 (`chaddr` against the
ethernet source), F10 (fragments), F11 (`PreAuth`), F14 (subnet reuse), F17
(sinkhole per query type), F18 (for the gateway address), F19 (long
options), F20 (`set_lease_records`).

## What I read and what I did not

Read in full, without the test modules:

- `src/net/switch.rs`, `src/net/framing.rs`, `src/net/gateway.rs`,
  `src/net/dhcp.rs` (from line 60), `src/net/dns.rs` (from line 30),
  `src/net/rules.rs`, `src/net/router.rs`, `src/net/bounded.rs`,
  `src/net/accept.rs`
- `src/net/nat/mod.rs`, `src/net/nat/vtcp.rs` (all of it, which the first
  pass read in part), `src/net/nat/udp.rs`, `src/net/nat/icmp.rs`,
  `src/net/nat/forward.rs`, `src/net/nat/guard.rs`
- `src/labd/netservices.rs` (to line 330), `src/labd/network.rs`
- `src/supervisor/trunk_auth.rs`

Read in part:

- `src/supervisor/global.rs`: the trunk table, `PreAuth`, the holders,
  `attach`, `detach`, `accept_peer`, the bridge, the dialer and the
  listener. Not `Directory` (`:330-480`) or `spawn_dns_sync`.
- `src/net/frame.rs`: the IPv4, UDP, TCP and ICMP views and the ICMP
  builders. Not the ethernet and ARP views, `tcp_build` or the checksum
  code.
- `hv/crates/vmlab-hv-proto/src/net.rs`: `Header::decode`, `validate`,
  `from_metadata` and `segments`. Not `Layout` or `flow_hash`.
- `src/net/fastpath/engine.rs` (the XDP half and the rtnetlink attach of
  the F2 fix), `src/net/fastpath/helper/server.rs` (the `XdpTap`, `XdpPort`,
  `XdpRelease` and `XdpMac` requests), `tap/src/lib.rs` (the bring-up).
- `src/labd/forward_plan.rs` and `src/labd/isolation.rs`: the bind address
  of a forward only.
- `docs/vmlab-prd.md`: §9.1 and the lines on `isolated`.

Not read:

- `src/net/fastpath/afxdp.rs`, `mod.rs`, `sockmap.rs`, `probe.rs`,
  `stub.rs`, `helper/client.rs`, `helper/proto.rs`, `priv_tests.rs`, and
  everything under `ebpf/` but the verdict lines of
  `ebpf/xdp-switch/src/main.rs`. The first pass read most of these. I did
  not check the fix of F2 beyond the attach call.
- `src/labd/machine.rs`, `src/labd/lab.rs` and `src/labd/mod.rs`: how NICs
  are attached at a start, how forwards are installed, and the global
  attach call sites. So the fixes of F12, F15 and F16 are checked only as
  far as `global.rs` and `network.rs` show them, and the fix of F13 only on
  the accepting side.
- The plans in `src/labd/forward_plan.rs` and `src/labd/isolation.rs`
  beyond the lines named above, and `src/smb/`.
- The virtio-net, e1000 and pcnet devices of vmlab-hv and the QEMU netdev
  arguments.
- Every test module, and the e2e `fastpath` scenario.
