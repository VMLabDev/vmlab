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
//!   and bytes are proxied both ways. Out-of-order segments are dropped
//!   (the guest retransmits), ACKs are cumulative, unacked host→guest data
//!   retransmits on a fixed 1 s RTO (5 tries), MSS is clamped to
//!   `mtu - 40`, and a fixed 64 KiB receive window is advertised. Idle
//!   flows are reset after 5 minutes.
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
//! Frames toward a guest need the guest's MAC: it is learned from every
//! frame the engine sees, and the gateway can prime the table via
//! [`NatEngine::learn_mac`] (it sees ARP/DHCP). Unknown guests fall back to
//! the L2 broadcast address, which the switch floods.

mod forward;
mod icmp;
mod udp;
mod vtcp;

#[cfg(test)]
mod tests;

pub use forward::PortForwarder;
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

/// The userspace NAT engine. Always `Arc`-shared; flow tasks hold clones.
pub struct NatEngine {
    cfg: NatConfig,
    /// Full ethernet frames toward the guest (the gateway injects them).
    output: mpsc::Sender<Bytes>,
    /// Live vTCP flows; the value feeds guest segments to the flow task.
    tcp_flows: StdMutex<HashMap<FlowKey, mpsc::Sender<vtcp::Segment>>>,
    /// Outbound UDP NAT flows.
    udp_flows: StdMutex<HashMap<FlowKey, udp::UdpFlow>>,
    /// Engine-bound UDP flows for [`Self::udp_bind_guest_flow`], keyed
    /// (guest_ip, guest_port, gw_port); the value receives guest payloads.
    guest_udp: StdMutex<HashMap<GuestUdpKey, mpsc::Sender<Vec<u8>>>>,
    /// Learned guest IP → MAC mappings.
    macs: StdMutex<HashMap<Ipv4Addr, MacAddr>>,
    icmp: icmp::IcmpState,
    pinger: Arc<dyn Pinger>,
    ip_id: AtomicU16,
    eph_counter: AtomicU32,
}

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
            udp_flows: StdMutex::new(HashMap::new()),
            guest_udp: StdMutex::new(HashMap::new()),
            macs: StdMutex::new(HashMap::new()),
            icmp: icmp::IcmpState::new(),
            pinger,
            ip_id: AtomicU16::new(1),
            eph_counter: AtomicU32::new(0),
        })
    }

    pub fn config(&self) -> &NatConfig {
        &self.cfg
    }

    /// Prime the IP → MAC table (the gateway sees ARP/DHCP and knows best).
    pub fn learn_mac(&self, ip: Ipv4Addr, mac: MacAddr) {
        self.macs.lock_recover().insert(ip, mac);
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
        self.learn_mac(ip.src(), src_mac);

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
            // Preserve arrival order and backpressure. Deliberately dropping
            // here turns a busy in-process proxy into apparent network loss
            // and can poison protocols such as SMB under sustained writes.
            let _ = tx.send(seg).await;
            return;
        }
        if seg.is_syn() && !seg.is_ack() {
            debug!(?key, "nat: passive vtcp open");
            let (tx, rx) = mpsc::channel(64);
            self.tcp_flows.lock_recover().insert(key, tx);
            vtcp::spawn_passive(self.clone(), key, src_mac, seg, rx);
        } else if !seg.is_rst() {
            // Anything unexpected for an unknown flow: reset it.
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
    /// `Receiver`. Dropping the `Sender` tears the flow down.
    pub fn udp_bind_guest_flow(
        self: &Arc<Self>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
        let (to_guest_tx, mut to_guest_rx) = mpsc::channel::<Vec<u8>>(64);
        let (from_guest_tx, from_guest_rx) = mpsc::channel::<Vec<u8>>(64);
        let gw_port = {
            let mut table = self.guest_udp.lock_recover();
            let port = loop {
                let p = self.next_ephemeral();
                if !table.contains_key(&(guest_ip, guest_port, p)) {
                    break p;
                }
            };
            table.insert((guest_ip, guest_port, port), from_guest_tx);
            port
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
        (to_guest_tx, from_guest_rx)
    }

    // -- internals shared with submodules -------------------------------------

    fn guest_mac(&self, ip: Ipv4Addr) -> MacAddr {
        self.macs
            .lock_recover()
            .get(&ip)
            .copied()
            .unwrap_or(MAC_BROADCAST)
    }

    fn next_ip_id(&self) -> u16 {
        self.ip_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Next port from the 49152..=65535 ephemeral range (round-robin).
    fn next_ephemeral(&self) -> u16 {
        let n = self.eph_counter.fetch_add(1, Ordering::Relaxed);
        49152 + (n % 16384) as u16
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
        for _ in 0..16384 {
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
