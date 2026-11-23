//! Userspace NAT engine — internet egress for guests (PRD §9.7) and the
//! host→guest path behind port forwards (PRD §9.8).
//!
//! The segment gateway hands the engine full ethernet frames whose
//! destination MAC is the gateway and whose destination IP is non-local
//! ([`NatEngine::handle_frame`]); the engine answers on its `output` channel
//! with full ethernet frames for the gateway to inject back into the switch.
//! No privileges are required anywhere: guest flows are *terminated* in
//! process and proxied over ordinary host sockets. Throughput is an explicit
//! non-goal (PRD §1.2); the implementation chooses simplicity and
//! correctness over speed everywhere.
//!
//! Per protocol:
//!
//! - **TCP** — a minimal in-process TCP responder ("vTCP", [`vtcp`]). A
//!   guest SYN triggers a host [`tokio::net::TcpStream`] connect to the
//!   original destination; on success the guest-side handshake completes
//!   and bytes are proxied both ways. Out-of-order segments are held for
//!   reassembly, ACKs are cumulative, unacked host→guest data retransmits
//!   on a fixed 1 s RTO (5 tries), host→guest data stays inside the
//!   guest's advertised window (a closed one is probed every RTO), MSS is
//!   clamped to `mtu - 40`, and a fixed 64 KiB receive window is
//!   advertised. Idle flows are reset after 5 minutes.
//! - **UDP** — per-flow connected host sockets with a 60 s idle expiry;
//!   replies are translated back into frames ([`udp`]).
//! - **ICMP echo** — *degraded by design*: unprivileged ICMP sockets are
//!   not available without extra capabilities or the `nix` socket feature,
//!   so reachability is probed by spawning the system `ping` binary (which
//!   is setuid/cap-enabled on practically every distro) and synthesizing
//!   the echo reply (or an ICMP host-unreachable) from the result. Results
//!   are cached for 10 s per destination so a flood ping does not become a
//!   subprocess storm. RTT seen by the guest is therefore meaningless; only
//!   reachability is faithful. See [`icmp`].
//!
//! For port forwards and gateway-terminated services the engine can also
//! *originate* connections toward guests: [`NatEngine::open_tcp_to_guest`]
//! performs an active vTCP open (SYN from `gw_ip:ephemeral`) and returns a
//! [`GuestStream`] (`AsyncRead + AsyncWrite`); [`NatEngine::udp_to_guest`]
//! and [`NatEngine::udp_bind_guest_flow`] do the same for UDP datagrams.
//! [`PortForwarder`] ties host listeners to those primitives.
//!
//! What a guest can make the engine hold is limited, per guest address and
//! per engine: guest-opened TCP flows ([`MAX_TCP_FLOWS`], a SYN past the
//! limit is dropped), UDP flows (a new one past the limit replaces the one
//! idle the longest, see [`udp`]) and `ping` probes in flight (see
//! [`icmp`]). Without the limits one guest takes every descriptor of the
//! lab daemon.
//!
//! The engine never lets a guest's flow land on the host's loopback
//! (`127.0.0.0/8`, `0.0.0.0/8`) on its own: a SYN is answered with RST, a
//! UDP datagram or an ICMP echo is dropped. A segment's [`HostGuard`] admits
//! the lab's own host services and declared `redirect` targets there, and
//! under worktree isolation (PRD §21.4) refuses every host address
//! ([`guard`]).
//!
//! Frames toward a guest need the guest's MAC. The lab daemon binds the
//! address of a forward to the MAC of the NIC that holds its lease with
//! [`NatEngine::learn_mac`], and no frame changes a bound address (review
//! F9). An address nobody bound is learned from the frames the engine sees.
//! Unknown guests fall back to the L2 broadcast address, which the switch
//! floods.

mod forward;
pub mod guard;
mod icmp;
mod udp;
mod vtcp;

#[cfg(test)]
mod tests;

pub use forward::PortForwarder;
pub use guard::HostGuard;
pub use icmp::{Pinger, SubprocessPinger};
pub use vtcp::{FlowKey, GuestStream};

use crate::sync::LockRecover;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::config::model::MacAddr;
use crate::net::bounded::BoundedMap;
use crate::net::frame::{
    self, ETHERTYPE_IPV4, EthView, ICMP_ECHO_REQUEST, IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP,
    IcmpView, Ipv4View, MAC_BROADCAST, TcpView, UdpView,
};

/// Static engine configuration.
#[derive(Debug, Clone, Copy)]
pub struct NatConfig {
    /// The segment gateway address — the engine's own L3 identity for
    /// active opens and ICMP errors it originates.
    pub gw_ip: Ipv4Addr,
    /// The gateway MAC, used as the source of every emitted frame.
    pub gw_mac: MacAddr,
    /// Segment MTU; vTCP advertises an MSS of `mtu - 40`.
    pub mtu: u16,
}

impl NatConfig {
    /// Config with the default 1500-byte MTU.
    pub fn new(gw_ip: Ipv4Addr, gw_mac: MacAddr) -> Self {
        Self {
            gw_ip,
            gw_mac,
            mtu: 1500,
        }
    }
}

/// (guest_ip, guest_port, gw_port) identifying an engine-bound UDP flow.
type GuestUdpKey = (Ipv4Addr, u16, u16);

/// The two ends of an engine-bound UDP flow: payloads toward the guest, and
/// the guest's replies.
pub type GuestUdpFlow = (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>);

/// The userspace NAT engine. Always `Arc`-shared; flow tasks hold clones.
pub struct NatEngine {
    cfg: NatConfig,
    /// Full ethernet frames toward the guest (the gateway injects them).
    output: mpsc::Sender<Bytes>,
    /// Live vTCP flows; the value feeds guest segments to the flow task.
    tcp_flows: StdMutex<HashMap<FlowKey, mpsc::Sender<vtcp::Segment>>>,
    /// How many guest-opened vTCP flows each guest address holds; the sum
    /// is held under [`MAX_TCP_FLOWS`]. Engine-originated flows (port
    /// forwards) are in `tcp_flows` but not here: no guest opens them.
    tcp_passive: StdMutex<HashMap<Ipv4Addr, usize>>,
    /// Outbound UDP NAT flows.
    udp_flows: StdMutex<HashMap<FlowKey, udp::UdpFlow>>,
    /// Engine-bound UDP flows for [`Self::udp_bind_guest_flow`], keyed
    /// (guest_ip, guest_port, gw_port); the value receives guest payloads.
    guest_udp: StdMutex<HashMap<GuestUdpKey, mpsc::Sender<Vec<u8>>>>,
    /// Guest IP → MAC mappings, bound or seen ([`GuestMacs`]).
    macs: StdMutex<GuestMacs>,
    icmp: icmp::IcmpState,
    pinger: Arc<dyn Pinger>,
    /// What of the host a new flow may not reach: the segment's guard, or
    /// worktree isolation's (§21.4). Unset, the engine refuses the host's
    /// loopback outright.
    guard: std::sync::OnceLock<Arc<HostGuard>>,
    ip_id: AtomicU16,
    eph_counter: AtomicU32,
    /// How many ports of the ephemeral range are handed out: all of it,
    /// except in tests that need to exhaust it.
    eph_span: AtomicU32,
}

/// Where the engine sends what it originates toward a guest address.
struct GuestMacs {
    /// Told by the lab daemon ([`NatEngine::learn_mac`]), which knows the
    /// NIC that holds the lease. Only another binding replaces an entry,
    /// and no guest frame adds or removes one (review F9).
    bound: HashMap<Ipv4Addr, MacAddr>,
    /// The source MAC of the last frame from each address without a
    /// binding. Any frame replaces it. A guest writes the source address
    /// freely, so this table holds [`MAX_GUEST_MACS`] (review F6).
    seen: BoundedMap<Ipv4Addr, MacAddr>,
}

/// How many guest-opened vTCP flows the engine holds at once (review F3):
/// each is a host socket and a task, and a lab daemon that runs out of
/// descriptors can open no QMP socket, agent socket or disk. A SYN past the
/// limit is dropped, so the guest's own retransmission retries it.
pub(super) const MAX_TCP_FLOWS: usize = 256;
/// How many of [`MAX_TCP_FLOWS`] one guest address may hold, so that one
/// guest cannot take every slot of its segment.
pub(super) const MAX_TCP_FLOWS_PER_GUEST: usize = 128;

/// One guest-opened vTCP flow's place under [`MAX_TCP_FLOWS`], held by the
/// flow task for as long as it runs.
pub(super) struct PassiveSlot {
    engine: Arc<NatEngine>,
    guest: Ipv4Addr,
}

impl Drop for PassiveSlot {
    fn drop(&mut self) {
        let mut held = self.engine.tcp_passive.lock_recover();
        if let Some(n) = held.get_mut(&self.guest) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.remove(&self.guest);
            }
        }
    }
}

/// How many guest addresses the engine keeps a seen MAC for (review F6).
/// Past this a new one replaces the oldest. A flow a guest opens carries the
/// MAC of its own frames. Only a flow the engine opens toward a guest (a
/// port forward) reads this table, and it broadcasts when the address has
/// neither a binding nor an entry here.
pub(super) const MAX_GUEST_MACS: usize = 1024;

/// The ephemeral range 49152..=65535.
const EPHEMERAL_BASE: u16 = 49152;
const EPHEMERAL_PORTS: u32 = 16384;

impl NatEngine {
    /// New engine using the subprocess `ping` reachability probe.
    pub fn new(cfg: NatConfig, output: mpsc::Sender<Bytes>) -> Arc<Self> {
        Self::with_pinger(cfg, output, Arc::new(SubprocessPinger::default()))
    }

    /// New engine with a custom [`Pinger`] (tests stub the subprocess out).
    pub fn with_pinger(
        cfg: NatConfig,
        output: mpsc::Sender<Bytes>,
        pinger: Arc<dyn Pinger>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            output,
            tcp_flows: StdMutex::new(HashMap::new()),
            tcp_passive: StdMutex::new(HashMap::new()),
            udp_flows: StdMutex::new(HashMap::new()),
            guest_udp: StdMutex::new(HashMap::new()),
            macs: StdMutex::new(GuestMacs {
                bound: HashMap::new(),
                seen: BoundedMap::new(MAX_GUEST_MACS),
            }),
            icmp: icmp::IcmpState::new(),
            pinger,
            guard: std::sync::OnceLock::new(),
            ip_id: AtomicU16::new(1),
            eph_counter: AtomicU32::new(0),
            eph_span: AtomicU32::new(EPHEMERAL_PORTS),
        })
    }

    pub fn config(&self) -> &NatConfig {
        &self.cfg
    }

    /// Give the engine its guard: from now on every new flow `guard`
    /// refuses is reset or dropped, and nothing else is. Set once; a second
    /// call is ignored.
    pub fn set_guard(&self, guard: Arc<HostGuard>) {
        let _ = self.guard.set(guard);
    }

    /// Whether a new flow to `dst` is refused: by the guard, and with none
    /// set, whenever `dst` is the host's loopback.
    fn refused(&self, dst: Ipv4Addr, kind: guard::FlowKind) -> bool {
        match self.guard.get() {
            Some(guard) => guard.refuses(dst, kind),
            None => guard::is_loopback(dst),
        }
    }

    /// Bind `ip` to `mac`: every connection the engine originates toward
    /// `ip` (a port forward) goes to `mac` from now on, whatever MAC a guest
    /// frame carries that address from (review F9). The caller is the lab
    /// daemon, which knows the NIC that holds the lease. A later call
    /// replaces the binding.
    pub fn learn_mac(&self, ip: Ipv4Addr, mac: MacAddr) {
        self.macs.lock_recover().bound.insert(ip, mac);
    }

    /// Note the MAC a frame from `ip` came from, unless `ip` is bound.
    fn see_mac(&self, ip: Ipv4Addr, mac: MacAddr) {
        let mut macs = self.macs.lock_recover();
        if !macs.bound.contains_key(&ip) {
            macs.seen.insert(ip, mac);
        }
    }

    /// Entry point: one guest→world IPv4 ethernet frame (dst MAC = gateway).
    /// Non-IPv4 and unparseable frames are silently dropped.
    pub async fn handle_frame(self: &Arc<Self>, eth_frame: Bytes) {
        let Some(eth) = EthView::parse(&eth_frame) else {
            return;
        };
        if eth.ethertype() != ETHERTYPE_IPV4 {
            return;
        }
        let src_mac = eth.src_mac();
        let Some(ip) = Ipv4View::parse(eth.payload()) else {
            return;
        };
        self.see_mac(ip.src(), src_mac);

        match ip.proto() {
            IPPROTO_TCP => self.handle_tcp(&ip, src_mac).await,
            IPPROTO_UDP => self.handle_udp(&ip, src_mac).await,
            IPPROTO_ICMP => self.handle_icmp(eth.payload(), &ip, src_mac).await,
            other => {
                trace!(proto = other, "nat: dropping unsupported protocol");
            }
        }
    }

    async fn handle_tcp(self: &Arc<Self>, ip: &Ipv4View<'_>, src_mac: MacAddr) {
        let Some(tcp) = TcpView::parse(ip.payload()) else {
            return;
        };
        let key = FlowKey {
            guest_ip: ip.src(),
            guest_port: tcp.src_port(),
            dst_ip: ip.dst(),
            dst_port: tcp.dst_port(),
        };
        let seg = vtcp::Segment::from_view(&tcp);
        let tx = self.tcp_flows.lock_recover().get(&key).cloned();
        if let Some(tx) = tx {
            // Never wait for a flow task: the caller is the gateway task of
            // the whole segment, and one flow that fell behind would stop
            // ARP, DHCP, DNS and every other flow with it (review F4).
            // `try_send` keeps arrival order. Backpressure is the flow's
            // advertised window, which closes as its host side falls
            // behind, and the channel holds more than a guest inside that
            // window has in flight. So only a guest that ignores the window
            // loses a segment here, and it sends that one again.
            if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(seg) {
                trace!(?key, "nat: flow queue full, segment dropped");
            }
            return;
        }
        // A SYN to the gateway's own address never opens a flow (review
        // F18). The host does not own that address, so a connect to it
        // would leave by the host's default route. What the gateway serves
        // over TCP is a flow the engine opened itself, found above, or a
        // `redirect` rewritten to another address before it gets here.
        let opens = seg.is_syn() && !seg.is_ack() && key.dst_ip != self.cfg.gw_ip;
        if opens && !self.refused(key.dst_ip, guard::FlowKind::Tcp { port: key.dst_port }) {
            let Some(slot) = self.claim_passive_tcp(key.guest_ip) else {
                // Dropped, not reset: the guest's SYN retransmission finds
                // the slot a finished flow gave back.
                debug!(?key, "nat: tcp flow limit reached, SYN dropped");
                return;
            };
            debug!(?key, "nat: passive vtcp open");
            let (tx, rx) = mpsc::channel(vtcp::SEGMENT_QUEUE);
            self.tcp_flows.lock_recover().insert(key, tx);
            vtcp::spawn_passive(self.clone(), key, src_mac, seg, rx, slot);
        } else if !seg.is_rst() {
            // Anything unexpected for an unknown flow, or a SYN to the
            // gateway address or a host address the engine refuses: reset
            // it.
            if let Some(rst) = vtcp::rst_for(ip, &tcp, self.next_ip_id()) {
                self.send_frame(src_mac, &rst).await;
            }
        }
    }

    async fn handle_udp(self: &Arc<Self>, ip: &Ipv4View<'_>, src_mac: MacAddr) {
        let Some(udp_seg) = UdpView::parse(ip.payload()) else {
            return;
        };
        if ip.dst() == self.cfg.gw_ip {
            // Reply to an engine-originated flow (udp_bind_guest_flow).
            let key = (ip.src(), udp_seg.src_port(), udp_seg.dst_port());
            let tx = self.guest_udp.lock_recover().get(&key).cloned();
            if let Some(tx) = tx {
                let _ = tx.send(udp_seg.payload().to_vec()).await;
            }
            return;
        }
        if self.refused(
            ip.dst(),
            guard::FlowKind::Udp {
                port: udp_seg.dst_port(),
            },
        ) {
            return;
        }
        let key = FlowKey {
            guest_ip: ip.src(),
            guest_port: udp_seg.src_port(),
            dst_ip: ip.dst(),
            dst_port: udp_seg.dst_port(),
        };
        udp::handle_outbound(self, key, src_mac, udp_seg.payload()).await;
    }

    async fn handle_icmp(self: &Arc<Self>, raw: &[u8], ip: &Ipv4View<'_>, src_mac: MacAddr) {
        let Some(icmp_msg) = IcmpView::parse(ip.payload()) else {
            return;
        };
        if icmp_msg.icmp_type() != ICMP_ECHO_REQUEST || ip.dst() == self.cfg.gw_ip {
            // Echo to the gateway itself is answered by the gateway, not
            // here; everything but echo requests is dropped.
            return;
        }
        if self.refused(ip.dst(), guard::FlowKind::Icmp) {
            return;
        }
        let pkt = raw[..usize::from(ip.total_len())].to_vec();
        let engine = self.clone();
        tokio::spawn(async move {
            icmp::handle_echo(engine, src_mac, pkt).await;
        });
    }

    // -- active open / engine-originated traffic ------------------------------

    /// Originate a TCP connection toward a guest (port forwards, PRD §9.8;
    /// gateway-terminated services, §7.5). Completes when the guest answers
    /// SYN-ACK; fails with RST/timeout (10 s) otherwise.
    pub async fn open_tcp_to_guest(
        self: &Arc<Self>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> Result<GuestStream> {
        vtcp::open_active(self.clone(), guest_ip, guest_port).await
    }

    /// One-shot UDP datagram to a guest from `gw_ip:<ephemeral>`. Replies
    /// (if any) are not routed anywhere; use [`Self::udp_bind_guest_flow`]
    /// for request/response exchanges. (Exercised by the NAT tests; the
    /// port-forward paths all use the bound-flow variant.)
    #[allow(dead_code)]
    pub async fn udp_to_guest(
        self: &Arc<Self>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
        payload: &[u8],
    ) {
        let src_port = self.next_ephemeral();
        let mac = self.guest_mac(guest_ip);
        self.send_udp_to_guest(
            mac,
            (self.cfg.gw_ip, src_port),
            (guest_ip, guest_port),
            payload,
        )
        .await;
    }

    /// Bind a bidirectional UDP exchange with a guest port: payloads sent to
    /// the returned `Sender` become datagrams `gw_ip:eph → guest:port`;
    /// datagrams the guest sends back to `gw_ip:eph` arrive on the returned
    /// `Receiver`. Dropping the `Sender` tears the flow down and frees its
    /// gateway port.
    ///
    /// Fails when every ephemeral port already carries a flow to this guest
    /// port: one pass over the range, never a spin under the table lock.
    pub fn udp_bind_guest_flow(
        self: &Arc<Self>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> Result<GuestUdpFlow> {
        use std::collections::hash_map::Entry;
        let (to_guest_tx, mut to_guest_rx) = mpsc::channel::<Vec<u8>>(64);
        let (from_guest_tx, from_guest_rx) = mpsc::channel::<Vec<u8>>(64);
        let gw_port = {
            let mut table = self.guest_udp.lock_recover();
            let mut free = None;
            for _ in 0..self.eph_span.load(Ordering::Relaxed) {
                let p = self.next_ephemeral();
                if let Entry::Vacant(slot) = table.entry((guest_ip, guest_port, p)) {
                    slot.insert(from_guest_tx);
                    free = Some(p);
                    break;
                }
            }
            match free {
                Some(p) => p,
                None => bail!("no free ephemeral port for a UDP flow to {guest_ip}:{guest_port}"),
            }
        };
        let engine = self.clone();
        tokio::spawn(async move {
            while let Some(payload) = to_guest_rx.recv().await {
                let mac = engine.guest_mac(guest_ip);
                engine
                    .send_udp_to_guest(
                        mac,
                        (engine.cfg.gw_ip, gw_port),
                        (guest_ip, guest_port),
                        &payload,
                    )
                    .await;
            }
            engine
                .guest_udp
                .lock_recover()
                .remove(&(guest_ip, guest_port, gw_port));
        });
        Ok((to_guest_tx, from_guest_rx))
    }

    // -- internals shared with submodules -------------------------------------

    pub(crate) fn guest_mac(&self, ip: Ipv4Addr) -> MacAddr {
        let macs = self.macs.lock_recover();
        macs.bound
            .get(&ip)
            .or_else(|| macs.seen.get(&ip))
            .copied()
            .unwrap_or(MAC_BROADCAST)
    }

    fn next_ip_id(&self) -> u16 {
        self.ip_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Next port from the 49152..=65535 ephemeral range (round-robin).
    fn next_ephemeral(&self) -> u16 {
        let n = self.eph_counter.fetch_add(1, Ordering::Relaxed);
        EPHEMERAL_BASE + (n % self.eph_span.load(Ordering::Relaxed)) as u16
    }

    /// Hand out only the first `ports` ports of the ephemeral range, so a
    /// test can exhaust it.
    #[cfg(test)]
    fn limit_ephemeral_ports(&self, ports: u32) {
        assert!((1..=EPHEMERAL_PORTS).contains(&ports));
        self.eph_span.store(ports, Ordering::Relaxed);
    }

    /// Reserve an ephemeral gateway port + flow-table slot for an active
    /// vTCP open.
    fn alloc_active_tcp_flow(
        &self,
        guest_ip: Ipv4Addr,
        guest_port: u16,
        tx: mpsc::Sender<vtcp::Segment>,
    ) -> Result<FlowKey> {
        use std::collections::hash_map::Entry;
        let mut table = self.tcp_flows.lock_recover();
        for _ in 0..self.eph_span.load(Ordering::Relaxed) {
            let key = FlowKey {
                guest_ip,
                guest_port,
                dst_ip: self.cfg.gw_ip,
                dst_port: self.next_ephemeral(),
            };
            if let Entry::Vacant(slot) = table.entry(key) {
                slot.insert(tx.clone());
                return Ok(key);
            }
        }
        bail!("no free ephemeral port for active open to {guest_ip}:{guest_port}");
    }

    /// Take a slot for one guest-opened vTCP flow, or `None` when `guest`
    /// or the engine is at its limit. The slot is given back when dropped.
    fn claim_passive_tcp(self: &Arc<Self>, guest: Ipv4Addr) -> Option<PassiveSlot> {
        let mut held = self.tcp_passive.lock_recover();
        if held.values().sum::<usize>() >= MAX_TCP_FLOWS {
            return None;
        }
        let of_guest = held.entry(guest).or_insert(0);
        if *of_guest >= MAX_TCP_FLOWS_PER_GUEST {
            return None;
        }
        *of_guest += 1;
        Some(PassiveSlot {
            engine: self.clone(),
            guest,
        })
    }

    fn remove_tcp_flow(&self, key: &FlowKey) {
        self.tcp_flows.lock_recover().remove(key);
    }

    /// Wrap an IPv4 packet in ethernet (src = gateway MAC) and emit it.
    async fn send_frame(&self, dst_mac: MacAddr, ipv4_packet: &[u8]) {
        let f = frame::eth_build(dst_mac, self.cfg.gw_mac, ETHERTYPE_IPV4, ipv4_packet);
        let _ = self.output.send(Bytes::from(f)).await;
    }

    async fn send_udp_to_guest(
        &self,
        dst_mac: MacAddr,
        src: (Ipv4Addr, u16),
        dst: (Ipv4Addr, u16),
        payload: &[u8],
    ) {
        let pkt = frame::udp_build(src.0, dst.0, src.1, dst.1, payload).and_then(|seg| {
            frame::ipv4_build(src.0, dst.0, IPPROTO_UDP, 64, &seg, self.next_ip_id())
        });
        let Some(pkt) = pkt else { return };
        self.send_frame(dst_mac, &pkt).await;
    }
}
