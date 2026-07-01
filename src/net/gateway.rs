//! The per-segment gateway service (PRD §9.4–§9.6).
//!
//! One gateway attaches to each segment's switch as a [`PortClass::Service`]
//! channel port. It owns the segment's first usable IP and a deterministic
//! locally-administered MAC ([`gateway_mac`]), and:
//!
//! - answers ARP requests for its IP,
//! - replies to ICMP echo addressed to it,
//! - serves DHCP (UDP 67) and DNS (UDP 53) when enabled,
//! - hands every other IPv4 frame addressed to the gateway MAC (guests
//!   routing off-segment traffic through their default gateway) to a
//!   pluggable uplink handler — the NAT / inter-segment routing engine,
//!   wired in later via [`GatewayHandle::set_uplink`].

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::debug;

use crate::config::model::MacAddr;
use crate::net::dhcp::{DHCP_SERVER_PORT, DhcpConfig, DhcpServer};
use crate::net::dns::{DNS_PORT, DnsAction, DnsServer, DnsZone, forward_upstream};
use crate::net::frame::{
    ArpOp, ArpView, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthView, IPPROTO_ICMP, IPPROTO_UDP, Ipv4View,
    UdpView, arp_reply_build, eth_build, icmp_echo_reply_for,
};
use crate::net::switch::{ChannelPort, PortClass, Switch};

/// How long the gateway waits for an upstream DNS resolver.
const UPSTREAM_DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// Uplink handler: receives every frame the gateway routes off-segment.
pub type UplinkFn = Box<dyn Fn(Bytes) + Send + Sync>;

type SharedUplink = Arc<Mutex<Option<UplinkFn>>>;

/// Deterministic locally-administered gateway MAC for a lab/segment pair:
/// fixed `52:56` prefix (local bit set, multicast bit clear) plus four bytes
/// folded from an FNV-1a hash of `lab/segment`.
pub fn gateway_mac(lab: &str, segment: &str) -> MacAddr {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in lab.as_bytes().iter().chain(b"/").chain(segment.as_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let x = h.to_be_bytes();
    MacAddr([
        0x52,
        0x56,
        x[0] ^ x[4],
        x[1] ^ x[5],
        x[2] ^ x[6],
        x[3] ^ x[7],
    ])
}

/// Configuration for one segment's gateway.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub segment_name: String,
    pub lab_name: String,
    /// The gateway's address: the segment's first usable IP.
    pub gw_ip: Ipv4Addr,
    /// See [`gateway_mac`].
    pub gw_mac: MacAddr,
    /// DHCP service; `None` disables it (PRD §9.4 `dhcp = false`). The
    /// gateway forces `gateway`/`server_mac` to its own identity.
    pub dhcp: Option<DhcpConfig>,
    /// DNS service; `None` disables it.
    pub dns: Option<DnsZone>,
    /// Default upstream resolver, applied to the zone when it has none.
    pub upstream_dns: Option<SocketAddr>,
}

/// Handle to a running gateway. Dropping it aborts the gateway task.
pub struct GatewayHandle {
    gw_ip: Ipv4Addr,
    gw_mac: MacAddr,
    tx: mpsc::Sender<Bytes>,
    dhcp: Option<Arc<Mutex<DhcpServer>>>,
    dns: Option<Arc<Mutex<DnsZone>>>,
    uplink: SharedUplink,
    task: tokio::task::JoinHandle<()>,
}

impl GatewayHandle {
    pub fn gw_ip(&self) -> Ipv4Addr {
        self.gw_ip
    }

    pub fn gw_mac(&self) -> MacAddr {
        self.gw_mac
    }

    /// Current DHCP leases; empty when DHCP is disabled. (Production syncs
    /// leases through [`Self::leases_probe`]; the gateway tests assert
    /// through this.)
    #[allow(dead_code)]
    pub fn dhcp_leases(&self) -> Vec<(MacAddr, Ipv4Addr)> {
        self.dhcp
            .as_ref()
            .map(|d| d.lock().expect("dhcp lock poisoned").leases())
            .unwrap_or_default()
    }

    /// Shared DNS zone for runtime mutation (register/sinkhole, PRD §9.9);
    /// `None` when DNS is disabled.
    pub fn dns_zone(&self) -> Option<Arc<Mutex<DnsZone>>> {
        self.dns.clone()
    }

    /// Detached lease accessor for background tasks (lease→DNS sync): the
    /// closure returns the current leases, or `None` once DHCP is gone.
    pub fn leases_probe(&self) -> impl Fn() -> Option<Vec<(MacAddr, Ipv4Addr)>> + Send + 'static {
        let dhcp = self.dhcp.clone();
        move || {
            dhcp.as_ref()
                .map(|d| d.lock().expect("dhcp lock poisoned").leases())
        }
    }

    /// Install (or replace) the uplink handler that receives off-segment
    /// frames — the NAT / inter-segment routing seam.
    pub fn set_uplink(&self, handler: UplinkFn) {
        *self.uplink.lock().expect("uplink lock poisoned") = Some(handler);
    }

    /// Send a frame out the gateway port into the switch (the NAT return
    /// path). Returns `false` if the port queue is full or closed.
    /// Production code uses the detached [`Self::injector`]; the gateway
    /// tests inject through the handle directly.
    #[allow(dead_code)]
    pub fn inject(&self, frame: Bytes) -> bool {
        self.tx.try_send(frame).is_ok()
    }

    /// A detached injector closure (the NAT engine's return path) that
    /// outlives borrows of the handle.
    pub fn injector(&self) -> impl Fn(Bytes) -> bool + Send + Sync + 'static {
        let tx = self.tx.clone();
        move |frame: Bytes| tx.try_send(frame).is_ok()
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The gateway service constructor (see module docs).
pub struct Gateway;

impl Gateway {
    /// Attach a gateway to `switch` as a service port and start its task.
    pub fn spawn(switch: &Arc<Switch>, config: GatewayConfig) -> GatewayHandle {
        let GatewayConfig {
            segment_name,
            lab_name,
            gw_ip,
            gw_mac,
            dhcp,
            dns,
            upstream_dns,
        } = config;

        let ChannelPort { id, tx, rx } = switch.add_channel_port(PortClass::Service);

        let dhcp = dhcp.map(|mut c| {
            c.gateway = gw_ip;
            c.server_mac = gw_mac;
            Arc::new(Mutex::new(DhcpServer::new(c)))
        });
        let dns_zone = dns.map(|mut z| {
            if z.upstream.is_none() {
                z.upstream = upstream_dns;
            }
            Arc::new(Mutex::new(z))
        });
        let uplink: SharedUplink = Arc::new(Mutex::new(None));

        let task = GatewayTask {
            gw_ip,
            gw_mac,
            segment: format!("{lab_name}/{segment_name}"),
            tx: tx.clone(),
            dhcp: dhcp.clone(),
            dns: dns_zone.clone().map(DnsServer::shared),
            uplink: Arc::clone(&uplink),
        };
        debug!(segment = %task.segment, port = %id, ip = %gw_ip, mac = %gw_mac, "gateway spawned");
        let join = tokio::spawn(task.run(rx));

        GatewayHandle {
            gw_ip,
            gw_mac,
            tx,
            dhcp,
            dns: dns_zone,
            uplink,
            task: join,
        }
    }
}

struct GatewayTask {
    gw_ip: Ipv4Addr,
    gw_mac: MacAddr,
    segment: String,
    tx: mpsc::Sender<Bytes>,
    dhcp: Option<Arc<Mutex<DhcpServer>>>,
    dns: Option<DnsServer>,
    uplink: SharedUplink,
}

impl GatewayTask {
    async fn run(self, mut rx: mpsc::Receiver<Bytes>) {
        while let Some(frame) = rx.recv().await {
            self.handle_frame(&frame);
        }
        debug!(segment = %self.segment, "gateway port closed, task exiting");
    }

    fn handle_frame(&self, frame: &Bytes) {
        let Some(eth) = EthView::parse(frame) else {
            return;
        };
        match eth.ethertype() {
            ETHERTYPE_ARP => self.handle_arp(eth.payload()),
            ETHERTYPE_IPV4 => self.handle_ipv4(&eth, frame),
            _ => {}
        }
    }

    fn handle_arp(&self, payload: &[u8]) {
        let Some(arp) = ArpView::parse(payload) else {
            return;
        };
        if arp.op() == ArpOp::Request && arp.tpa() == self.gw_ip {
            self.send(arp_reply_build(
                self.gw_mac,
                self.gw_ip,
                arp.sha(),
                arp.spa(),
            ));
        }
    }

    fn handle_ipv4(&self, eth: &EthView<'_>, frame: &Bytes) {
        let Some(ip) = Ipv4View::parse(eth.payload()) else {
            return;
        };

        // DHCP and DNS first: DHCP arrives broadcast (DISCOVER/REQUEST) or
        // unicast (renewals), so match on the UDP port alone.
        if ip.proto() == IPPROTO_UDP
            && let Some(udp) = UdpView::parse(ip.payload())
        {
            if udp.dst_port() == DHCP_SERVER_PORT {
                if let Some(dhcp) = &self.dhcp {
                    let reply = dhcp.lock().expect("dhcp lock poisoned").handle(frame);
                    if let Some(reply) = reply {
                        self.send(reply);
                    }
                }
                return;
            }
            if udp.dst_port() == DNS_PORT && ip.dst() == self.gw_ip {
                if let Some(dns) = &self.dns {
                    self.handle_dns(dns.handle(frame));
                }
                return;
            }
        }

        if ip.dst() == self.gw_ip {
            // ICMP echo to the gateway itself is answered here. Other
            // gateway-addressed traffic falls through to the uplink: TCP
            // replies to engine-originated flows (port forwards, §9.8) and
            // gateway-terminated services land in the NAT engine.
            if ip.proto() == IPPROTO_ICMP {
                if let Some(reply) = icmp_echo_reply_for(eth.payload()) {
                    self.send(eth_build(
                        eth.src_mac(),
                        self.gw_mac,
                        ETHERTYPE_IPV4,
                        &reply,
                    ));
                }
                return;
            }
        }

        // Addressed to the gateway MAC but not its IP: a guest routing
        // off-segment traffic through us. Hand it to the uplink.
        if eth.dst_mac() == self.gw_mac {
            let uplink = self.uplink.lock().expect("uplink lock poisoned");
            match uplink.as_ref() {
                Some(f) => f(frame.clone()),
                None => {
                    debug!(segment = %self.segment, dst = %ip.dst(), "no uplink attached, frame dropped");
                }
            }
        }
    }

    fn handle_dns(&self, action: DnsAction) {
        match action {
            DnsAction::Reply(f) => self.send(f),
            DnsAction::ForwardUpstream {
                upstream,
                raw_query,
                respond,
                ..
            } => {
                let tx = self.tx.clone();
                let segment = self.segment.clone();
                tokio::spawn(async move {
                    match forward_upstream(upstream, &raw_query, UPSTREAM_DNS_TIMEOUT).await {
                        Some(resp) => {
                            let frame = respond.build_frame(&resp);
                            let _ = tx.send(Bytes::from(frame)).await;
                        }
                        None => {
                            debug!(segment = %segment, %upstream, "upstream DNS query failed");
                        }
                    }
                });
            }
            DnsAction::Ignore => {}
        }
    }

    fn send(&self, frame: Vec<u8>) {
        if self.tx.try_send(Bytes::from(frame)).is_err() {
            debug!(segment = %self.segment, "gateway port queue full, frame dropped");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ipnet::Ipv4Net;
    use tokio::time::timeout;

    use super::*;
    use crate::net::frame::{
        ICMP_ECHO_REPLY, ICMP_ECHO_REQUEST, IPPROTO_ICMP, IcmpView, MAC_BROADCAST,
        arp_request_build, icmp_build, ipv4_build, udp_build,
    };

    const GUEST_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 0x0A]);
    const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 50);

    fn subnet() -> Ipv4Net {
        "10.213.1.0/24".parse().unwrap()
    }

    fn gw_ip() -> Ipv4Addr {
        Ipv4Addr::new(10, 213, 1, 1)
    }

    fn config() -> GatewayConfig {
        let gw_mac = gateway_mac("lab", "seg");
        GatewayConfig {
            segment_name: "seg".into(),
            lab_name: "lab".into(),
            gw_ip: gw_ip(),
            gw_mac,
            dhcp: Some(DhcpConfig::new(subnet(), gw_ip(), gw_mac)),
            dns: Some(DnsZone::new("vmlab.internal")),
            upstream_dns: None,
        }
    }

    async fn recv(rx: &mut mpsc::Receiver<Bytes>) -> Bytes {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for frame")
            .expect("port closed")
    }

    /// A minimal DHCP DISCOVER broadcast frame from `mac`.
    fn discover_frame(mac: MacAddr) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        b[0] = 1; // BOOTREQUEST
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        b[28..34].copy_from_slice(&mac.0);
        b[236..240].copy_from_slice(&[99, 130, 83, 99]);
        b.extend_from_slice(&[53, 1, 1]); // DISCOVER
        b.push(255);
        let udp = udp_build(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &b);
        let ip = ipv4_build(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            IPPROTO_UDP,
            64,
            &udp,
            1,
        );
        crate::net::frame::eth_build(MAC_BROADCAST, mac, ETHERTYPE_IPV4, &ip)
    }

    #[test]
    fn gateway_mac_is_deterministic_local_and_unicast() {
        let a = gateway_mac("lab", "seg");
        assert_eq!(a, gateway_mac("lab", "seg"));
        assert_ne!(a, gateway_mac("lab", "other"));
        assert_ne!(a, gateway_mac("other", "seg"));
        // Prefix-shift collisions ("ab"/"c" vs "a"/"bc") are separated too.
        assert_ne!(gateway_mac("ab", "c"), gateway_mac("a", "bc"));
        assert_eq!(a.0[0], 0x52);
        assert_eq!(a.0[1], 0x56);
        assert_eq!(a.0[0] & 0x01, 0, "multicast bit must be clear");
        assert_ne!(a.0[0] & 0x02, 0, "local bit must be set");
    }

    #[tokio::test]
    async fn dhcp_discover_gets_offer_through_switch() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let mut guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        guest
            .tx
            .send(Bytes::from(discover_frame(GUEST_MAC)))
            .await
            .unwrap();
        let reply = recv(&mut guest.rx).await;
        let eth = EthView::parse(&reply).unwrap();
        assert_eq!(eth.src_mac(), gw.gw_mac());
        assert_eq!(eth.dst_mac(), GUEST_MAC);
        let ip = Ipv4View::parse(eth.payload()).unwrap();
        assert_eq!(ip.src(), gw.gw_ip());
        let udp = UdpView::parse(ip.payload()).unwrap();
        assert_eq!((udp.src_port(), udp.dst_port()), (67, 68));
        let p = udp.payload();
        assert_eq!(p[0], 2, "BOOTREPLY");
        let yiaddr = Ipv4Addr::new(p[16], p[17], p[18], p[19]);
        assert!(subnet().contains(&yiaddr));
        assert_ne!(yiaddr, gw.gw_ip());
        // Options carry msg-type OFFER and the router.
        let mut opts = std::collections::HashMap::new();
        let mut i = 240;
        while i < p.len() && p[i] != 255 {
            if p[i] == 0 {
                i += 1;
                continue;
            }
            let len = usize::from(p[i + 1]);
            opts.insert(p[i], p[i + 2..i + 2 + len].to_vec());
            i += 2 + len;
        }
        assert_eq!(opts[&53], vec![2u8]); // OFFER
        assert_eq!(opts[&3], gw.gw_ip().octets().to_vec()); // router
        // The lease shows up in status.
        assert_eq!(gw.dhcp_leases(), vec![(GUEST_MAC, yiaddr)]);
    }

    #[tokio::test]
    async fn arp_request_for_gateway_ip_is_answered() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let mut guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        let req = arp_request_build(GUEST_MAC, GUEST_IP, gw.gw_ip());
        guest.tx.send(Bytes::from(req)).await.unwrap();
        let reply = recv(&mut guest.rx).await;
        let eth = EthView::parse(&reply).unwrap();
        assert_eq!(eth.dst_mac(), GUEST_MAC);
        let arp = ArpView::parse(eth.payload()).unwrap();
        assert_eq!(arp.op(), ArpOp::Reply);
        assert_eq!(arp.sha(), gw.gw_mac());
        assert_eq!(arp.spa(), gw.gw_ip());
        assert_eq!(arp.tha(), GUEST_MAC);
        assert_eq!(arp.tpa(), GUEST_IP);
    }

    #[tokio::test]
    async fn ping_gateway_gets_echo_reply() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let mut guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        let echo = icmp_build(ICMP_ECHO_REQUEST, 0, [0xAB, 0xCD, 0, 1], b"ping!");
        let ipp = ipv4_build(GUEST_IP, gw.gw_ip(), IPPROTO_ICMP, 64, &echo, 7);
        let frame = eth_build(gw.gw_mac(), GUEST_MAC, ETHERTYPE_IPV4, &ipp);
        guest.tx.send(Bytes::from(frame)).await.unwrap();

        let reply = recv(&mut guest.rx).await;
        let eth = EthView::parse(&reply).unwrap();
        assert_eq!(eth.dst_mac(), GUEST_MAC);
        assert_eq!(eth.src_mac(), gw.gw_mac());
        let ip = Ipv4View::parse(eth.payload()).unwrap();
        assert_eq!(ip.src(), gw.gw_ip());
        assert_eq!(ip.dst(), GUEST_IP);
        let icmp = IcmpView::parse(ip.payload()).unwrap();
        assert_eq!(icmp.icmp_type(), ICMP_ECHO_REPLY);
        assert_eq!(icmp.rest(), [0xAB, 0xCD, 0, 1]);
        assert_eq!(icmp.payload(), b"ping!");
    }

    #[tokio::test]
    async fn dns_query_answered_for_runtime_registered_name() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let mut guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        let web = Ipv4Addr::new(10, 213, 1, 77);
        gw.dns_zone()
            .expect("dns enabled")
            .lock()
            .unwrap()
            .register("web", web);

        let q = {
            let mut q = vec![0x77, 0x01, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
            q.extend_from_slice(&crate::net::dns::encode_qname("web.vmlab.internal"));
            q.extend_from_slice(&[0, 1, 0, 1]); // A IN
            q
        };
        let udp = udp_build(GUEST_IP, gw.gw_ip(), 33000, 53, &q);
        let ipp = ipv4_build(GUEST_IP, gw.gw_ip(), IPPROTO_UDP, 64, &udp, 9);
        let frame = eth_build(gw.gw_mac(), GUEST_MAC, ETHERTYPE_IPV4, &ipp);
        guest.tx.send(Bytes::from(frame)).await.unwrap();

        let reply = recv(&mut guest.rx).await;
        let eth = EthView::parse(&reply).unwrap();
        assert_eq!(eth.dst_mac(), GUEST_MAC);
        let ip = Ipv4View::parse(eth.payload()).unwrap();
        let udp = UdpView::parse(ip.payload()).unwrap();
        assert_eq!((udp.src_port(), udp.dst_port()), (53, 33000));
        let m = udp.payload();
        assert_eq!(&m[..2], &[0x77, 0x01]);
        assert_ne!(m[2] & 0x80, 0); // QR
        assert_eq!(m[3] & 0x0F, 0); // NOERROR
        assert_eq!(u16::from_be_bytes([m[6], m[7]]), 1); // one answer
        assert_eq!(&m[m.len() - 4..], &web.octets()); // rdata
    }

    #[tokio::test]
    async fn off_segment_frame_via_gw_mac_reaches_uplink() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        let (utx, mut urx) = mpsc::unbounded_channel::<Bytes>();
        gw.set_uplink(Box::new(move |f| {
            let _ = utx.send(f);
        }));

        let udp = udp_build(GUEST_IP, Ipv4Addr::new(8, 8, 8, 8), 5555, 443, b"hello");
        let ipp = ipv4_build(
            GUEST_IP,
            Ipv4Addr::new(8, 8, 8, 8),
            IPPROTO_UDP,
            64,
            &udp,
            21,
        );
        let frame = Bytes::from(eth_build(gw.gw_mac(), GUEST_MAC, ETHERTYPE_IPV4, &ipp));
        guest.tx.send(frame.clone()).await.unwrap();

        let got = timeout(Duration::from_secs(2), urx.recv())
            .await
            .expect("timed out waiting for uplink frame")
            .expect("uplink channel closed");
        assert_eq!(got, frame);
    }

    /// TCP addressed to the gateway IP itself must reach the uplink — it
    /// carries replies to engine-originated flows (port forwards, §9.8)
    /// and DNAT'd gateway services (§7.5). Only ICMP echo is terminated
    /// by the gateway.
    #[tokio::test]
    async fn gw_ip_tcp_reaches_uplink() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        let (utx, mut urx) = mpsc::unbounded_channel::<Bytes>();
        gw.set_uplink(Box::new(move |f| {
            let _ = utx.send(f);
        }));

        let tcp = crate::net::frame::tcp_build(
            GUEST_IP,
            gw.gw_ip(),
            crate::net::frame::TcpFields {
                src_port: 5555,
                dst_port: 445,
                seq: 1,
                ack: 0,
                flags: 0x02, // SYN
                window: 65535,
                options: &[],
            },
            &[],
        );
        let ipp = ipv4_build(GUEST_IP, gw.gw_ip(), 6, 64, &tcp, 22);
        let frame = Bytes::from(eth_build(gw.gw_mac(), GUEST_MAC, ETHERTYPE_IPV4, &ipp));
        guest.tx.send(frame.clone()).await.unwrap();

        let got = timeout(Duration::from_secs(2), urx.recv())
            .await
            .expect("gw-addressed TCP never reached the uplink")
            .expect("uplink channel closed");
        assert_eq!(got, frame);
    }

    #[tokio::test]
    async fn inject_sends_frame_out_the_gateway_port() {
        let sw = Switch::new("seg".into());
        let gw = Gateway::spawn(&sw, config());
        let mut guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        // Teach the switch the guest's MAC.
        guest
            .tx
            .send(Bytes::from(eth_build(
                MAC_BROADCAST,
                GUEST_MAC,
                ETHERTYPE_IPV4,
                b"hello",
            )))
            .await
            .unwrap();

        // NAT return path: inject a frame addressed to the guest.
        let frame = Bytes::from(eth_build(
            GUEST_MAC,
            gw.gw_mac(),
            ETHERTYPE_IPV4,
            b"return traffic",
        ));
        assert!(gw.inject(frame.clone()));
        assert_eq!(recv(&mut guest.rx).await, frame);
    }

    #[tokio::test]
    async fn disabled_services_stay_silent_but_arp_works() {
        let sw = Switch::new("seg".into());
        let mut cfg = config();
        cfg.dhcp = None;
        cfg.dns = None;
        let gw = Gateway::spawn(&sw, cfg);
        let mut guest = sw.add_channel_port(PortClass::Guest { isolated: false });

        assert!(gw.dns_zone().is_none());
        assert!(gw.dhcp_leases().is_empty());

        // DHCP DISCOVER goes unanswered...
        guest
            .tx
            .send(Bytes::from(discover_frame(GUEST_MAC)))
            .await
            .unwrap();
        // ...but ARP still works (also proves the discover produced nothing,
        // since ordering on the port is FIFO).
        let req = arp_request_build(GUEST_MAC, GUEST_IP, gw.gw_ip());
        guest.tx.send(Bytes::from(req)).await.unwrap();
        let reply = recv(&mut guest.rx).await;
        let arp = ArpView::parse(EthView::parse(&reply).unwrap().payload()).unwrap();
        assert_eq!(arp.op(), ArpOp::Reply);
        assert_eq!(arp.sha(), gw.gw_mac());
    }
}
