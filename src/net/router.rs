//! Daemon inter-segment routing (PRD §9.6, mechanism 2).
//!
//! The lab daemon forwards L3 between two of its own segments, and only
//! between a pair someone connected — `routes_to` in the lab file or
//! `seg.route_to()` from a script. Segments are isolated otherwise.
//!
//! - **Bidirectional.** A pair is unordered: connecting `a` to `b` routes
//!   both ways, and declaring it on either side (or both) is the same thing.
//! - **Where it sits.** Each segment's gateway hands the router every frame
//!   a guest addressed to the gateway MAC, before NAT or host services see
//!   it. A packet for a connected peer's subnet leaves the peer's gateway
//!   ([`L3Port`]) toward the destination, TTL decremented; anything else is
//!   handed back for NAT. Gateway-addressed frames punt to userspace on
//!   every fast-path tier, so routing is tier-invariant.
//! - **No NAT.** A routed packet keeps its source address; NAT stays the
//!   internet-egress path.
//! - **Rules.** A segment's `block`/`redirect` rules already ran on the
//!   packet at its switch ingress — the segment it is leaving. The segment
//!   it enters un-DNATs replies to its own redirects, as NAT output does.
//! - **Guests learn it by DHCP.** Each side of a pair offers the other's
//!   subnet via its own gateway in option 121 ([`Router::dhcp_routes`]);
//!   a change reaches leases granted after it.
//!
//! Lab-local only: global segments have no daemon gateway here and are never
//! registered.

use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use bytes::Bytes;
use ipnet::Ipv4Net;

use crate::net::dhcp::DhcpServer;
use crate::net::frame::{
    ETHERTYPE_IPV4, EthView, ICMP_DEST_UNREACHABLE, ICMP_ECHO_REQUEST, IPPROTO_ICMP, IcmpView,
    Ipv4View, icmp_build, icmp_echo_reply_for, internet_checksum, ipv4_build,
};
use crate::net::gateway::L3Port;
use crate::net::rules::{RuleSet, Verdict};
use crate::sync::LockRecover;

/// ICMP time exceeded (RFC 792), code 0: TTL exceeded in transit.
const ICMP_TIME_EXCEEDED: u8 = 11;
/// Destination unreachable code 4: fragmentation needed and DF set.
const ICMP_FRAG_NEEDED: u8 = 4;

/// One segment as the router sees it.
pub struct Leg {
    pub name: String,
    pub subnet: Ipv4Net,
    /// The daemon's address on the segment — what option 121 routes via.
    pub gw_ip: Ipv4Addr,
    pub mtu: u16,
    /// Where packets routed onto this segment leave from.
    pub port: L3Port,
    /// The segment's L3 rules: the return half of its redirects applies to
    /// packets routed onto it.
    pub rules: Arc<Mutex<RuleSet>>,
    /// Its DHCP server, `None` with `dhcp = false`.
    pub dhcp: Option<Arc<Mutex<DhcpServer>>>,
    /// The `route {}` blocks it declares, which option 121 always carries.
    pub declared_routes: Vec<(Ipv4Net, Ipv4Addr)>,
}

/// The lab's routing table: its segments and the pairs connected.
#[derive(Default)]
pub struct Router {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    legs: HashMap<String, Arc<Leg>>,
    /// Connected pairs, each stored once with the smaller name first.
    pairs: BTreeSet<(String, String)>,
}

impl Inner {
    fn peers_of<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Arc<Leg>> + 'a {
        self.pairs.iter().filter_map(move |(a, b)| {
            let other = if a == name {
                b
            } else if b == name {
                a
            } else {
                return None;
            };
            self.legs.get(other)
        })
    }
}

/// What to do with one frame a guest sent its gateway.
enum Decision {
    /// Not for a connected peer: NAT and host services take it.
    NotRouted,
    /// Emit `packet` on `to`, toward its destination.
    Forward { to: Arc<Leg>, packet: Vec<u8> },
    /// Answer the sender on its own segment: an echo reply from a peer's
    /// gateway, or an ICMP error.
    Answer { on: Arc<Leg>, packet: Vec<u8> },
    /// For a peer, but not deliverable; dropped without an answer.
    Drop,
}

impl Router {
    pub fn new() -> Arc<Router> {
        Arc::new(Router::default())
    }

    /// Add (or replace) a segment. Its DHCP routes are brought up to date
    /// with whatever it is already connected to.
    pub fn register(&self, leg: Leg) {
        let name = leg.name.clone();
        let mut inner = self.write();
        inner.legs.insert(name.clone(), Arc::new(leg));
        refresh_dhcp(&inner, &name);
    }

    /// Connect `a` and `b` (§9.6): route between them, both ways, and offer
    /// each one's subnet to the other's new leases. Connecting a pair
    /// already connected changes nothing.
    pub fn connect(&self, a: &str, b: &str) -> Result<(), String> {
        self.set(a, b, true)
    }

    /// Take the pair apart again. A pair not connected is not an error.
    pub fn disconnect(&self, a: &str, b: &str) -> Result<(), String> {
        self.set(a, b, false)
    }

    fn set(&self, a: &str, b: &str, connected: bool) -> Result<(), String> {
        if a == b {
            return Err(format!("segment \"{a}\" cannot route to itself"));
        }
        let mut inner = self.write();
        for name in [a, b] {
            if !inner.legs.contains_key(name) {
                return Err(format!("no lab-local segment named \"{name}\" to route to"));
            }
        }
        let key = if a < b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        };
        let changed = if connected {
            inner.pairs.insert(key)
        } else {
            inner.pairs.remove(&key)
        };
        if changed {
            refresh_dhcp(&inner, a);
            refresh_dhcp(&inner, b);
        }
        Ok(())
    }

    /// Whether `a` and `b` are connected.
    #[allow(dead_code)]
    pub fn connected(&self, a: &str, b: &str) -> bool {
        self.read().peers_of(a).any(|p| p.name == b)
    }

    /// The option-121 routes `segment` offers: its declared `route {}`
    /// blocks, then each connected peer's subnet via its own gateway. A
    /// declared route to a peer's subnet wins over the daemon's. (The DHCP
    /// servers are kept current directly; the router tests assert through
    /// this.)
    #[allow(dead_code)]
    pub fn dhcp_routes(&self, segment: &str) -> Vec<(Ipv4Net, Ipv4Addr)> {
        dhcp_routes_of(&self.read(), segment)
    }

    /// Route one frame `from`'s gateway received. `Some` hands the frame
    /// back untouched — it is not for a connected peer — and `None` means
    /// the router took it.
    pub async fn route(&self, from: &str, frame: Bytes) -> Option<Bytes> {
        let reply_to = EthView::parse(&frame).map(|eth| eth.src_mac());
        match self.decide(from, &frame) {
            Decision::NotRouted => Some(frame),
            Decision::Forward { to, packet } => {
                let dst = Ipv4View::parse(&packet).map(|ip| ip.dst())?;
                to.port.send_ip(dst, packet).await;
                None
            }
            Decision::Answer { on, packet } => {
                if let Some(mac) = reply_to {
                    on.port.send_to(mac, &packet).await;
                }
                None
            }
            Decision::Drop => None,
        }
    }

    fn decide(&self, from: &str, frame: &[u8]) -> Decision {
        let Some(eth) = EthView::parse(frame).filter(|e| e.ethertype() == ETHERTYPE_IPV4) else {
            return Decision::NotRouted;
        };
        let Some(ip) = Ipv4View::parse(eth.payload()) else {
            return Decision::NotRouted;
        };
        let inner = self.read();
        let Some(origin) = inner.legs.get(from) else {
            return Decision::NotRouted;
        };
        let dst = ip.dst();
        let Some(peer) = inner.peers_of(from).find(|p| p.subnet.contains(&dst)) else {
            return Decision::NotRouted;
        };
        // Ethernet padding is not part of the packet.
        let packet = &eth.payload()[..usize::from(ip.total_len()).min(eth.payload().len())];

        if dst == peer.gw_ip {
            // The peer's gateway answers a ping, as its own guests see it.
            return match icmp_echo_reply_for(packet) {
                Some(reply) => Decision::Answer {
                    on: origin.clone(),
                    packet: reply,
                },
                None => Decision::Drop,
            };
        }
        if dst == peer.subnet.network() || dst == peer.subnet.broadcast() {
            return Decision::Drop;
        }
        if ip.ttl() <= 1 {
            return icmp_error(origin, packet, ICMP_TIME_EXCEEDED, 0, [0; 4]);
        }
        if packet.len() > usize::from(peer.mtu) {
            if ip.dont_fragment() {
                let [hi, lo] = peer.mtu.to_be_bytes();
                return icmp_error(
                    origin,
                    packet,
                    ICMP_DEST_UNREACHABLE,
                    ICMP_FRAG_NEEDED,
                    [0, 0, hi, lo],
                );
            }
            // The fabric does not fragment; a sender that allowed it is
            // told nothing, as a lossy link would tell it nothing.
            tracing::debug!(from, to = %peer.name, len = packet.len(), "routed packet over the peer MTU, dropped");
            return Decision::Drop;
        }

        let mut packet = packet.to_vec();
        decrement_ttl(&mut packet);
        // A reply to one of the peer's own redirects is un-DNATed on the
        // way in, exactly as NAT output is.
        let verdict = peer.rules.lock_recover().eval_return(&packet);
        if let Verdict::Rewrite(rewritten) = verdict {
            packet = rewritten;
        }
        Decision::Forward {
            to: peer.clone(),
            packet,
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }
}

fn dhcp_routes_of(inner: &Inner, segment: &str) -> Vec<(Ipv4Net, Ipv4Addr)> {
    let Some(leg) = inner.legs.get(segment) else {
        return Vec::new();
    };
    let mut routes = leg.declared_routes.clone();
    let mut peers: Vec<Ipv4Net> = inner.peers_of(segment).map(|p| p.subnet).collect();
    peers.sort();
    for subnet in peers {
        if !routes.iter().any(|(dest, _)| *dest == subnet) {
            routes.push((subnet, leg.gw_ip));
        }
    }
    routes
}

fn refresh_dhcp(inner: &Inner, segment: &str) {
    if let Some(dhcp) = inner.legs.get(segment).and_then(|l| l.dhcp.as_ref()) {
        dhcp.lock_recover()
            .set_routes(dhcp_routes_of(inner, segment));
    }
}

/// An ICMP error from `origin`'s gateway back to the packet's sender —
/// never about an ICMP error (RFC 1122 §3.2.2), so only for echo requests
/// among ICMP messages.
fn icmp_error(
    origin: &Arc<Leg>,
    packet: &[u8],
    icmp_type: u8,
    code: u8,
    rest: [u8; 4],
) -> Decision {
    let Some(ip) = Ipv4View::parse(packet) else {
        return Decision::Drop;
    };
    if ip.proto() == IPPROTO_ICMP
        && IcmpView::parse(ip.payload()).is_none_or(|i| i.icmp_type() != ICMP_ECHO_REQUEST)
    {
        return Decision::Drop;
    }
    let quote = &packet[..(ip.header_len() + 8).min(packet.len())];
    let icmp = icmp_build(icmp_type, code, rest, quote);
    match ipv4_build(origin.gw_ip, ip.src(), IPPROTO_ICMP, 64, &icmp, ip.id()) {
        Some(packet) => Decision::Answer {
            on: origin.clone(),
            packet,
        },
        None => Decision::Drop,
    }
}

/// One hop: TTL down by one and the header checksum recomputed.
fn decrement_ttl(packet: &mut [u8]) {
    let Some(header_len) = Ipv4View::parse(packet).map(|ip| ip.header_len()) else {
        return;
    };
    packet[8] = packet[8].saturating_sub(1);
    packet[10..12].copy_from_slice(&[0, 0]);
    let csum = internet_checksum(&packet[..header_len]);
    packet[10..12].copy_from_slice(&csum.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;
    use crate::config::model::{BlockRule, HostPort, L4Proto, MacAddr, RedirectRule};
    use crate::net::dhcp::DhcpConfig;
    use crate::net::frame::{
        ArpOp, ArpView, ETHERTYPE_ARP, ICMP_ECHO_REPLY, IPPROTO_TCP, TCP_RST, TCP_SYN, TcpFields,
        TcpView, arp_reply_build, arp_request_build, eth_build, icmp_build, tcp_build,
    };
    use crate::net::gateway::{Gateway, GatewayConfig, GatewayHandle, gateway_mac};
    use crate::net::switch::{ChannelPort, PortClass, Switch};

    const MAC_A: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 0x0A]);
    const MAC_B: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 0x0B]);
    const IP_A: Ipv4Addr = Ipv4Addr::new(10, 1, 0, 10);
    const IP_B: Ipv4Addr = Ipv4Addr::new(10, 2, 0, 20);
    const IP_C: Ipv4Addr = Ipv4Addr::new(10, 3, 0, 30);

    /// One segment: its switch, gateway, rules, and a guest port on it.
    struct Seg {
        switch: Arc<Switch>,
        gateway: GatewayHandle,
        rules: Arc<Mutex<RuleSet>>,
        guest: ChannelPort,
    }

    fn subnet(name: &str) -> Ipv4Net {
        match name {
            "a" => "10.1.0.0/24",
            "b" => "10.2.0.0/24",
            _ => "10.3.0.0/24",
        }
        .parse()
        .unwrap()
    }

    /// Build segment `name` on `router`, its uplink wired through the router
    /// with NAT standing in as a channel that records what fell through.
    fn segment(
        router: &Arc<Router>,
        name: &str,
        mtu: u16,
    ) -> (Seg, mpsc::UnboundedReceiver<Bytes>) {
        let switch = Switch::new(name.into());
        let net = subnet(name);
        let gw_ip = crate::config::validate::gateway_ip(net);
        let gw_mac = gateway_mac("lab", name);
        let gateway = Gateway::spawn(
            &switch,
            GatewayConfig {
                segment_name: name.into(),
                lab_name: "lab".into(),
                gw_ip,
                gw_mac,
                dhcp: Some(DhcpConfig::new(net, gw_ip, gw_mac)),
                dns: None,
                upstream_dns: None,
            },
        );
        let rules = Arc::new(Mutex::new(RuleSet::new()));
        router.register(Leg {
            name: name.into(),
            subnet: net,
            gw_ip,
            mtu,
            port: gateway.l3_port(),
            rules: rules.clone(),
            dhcp: gateway.dhcp_server(),
            declared_routes: Vec::new(),
        });
        let (nat_tx, nat_rx) = mpsc::unbounded_channel();
        let r = router.clone();
        let seg_name = name.to_string();
        gateway.set_uplink(Arc::new(move |frame| {
            let r = r.clone();
            let seg_name = seg_name.clone();
            let nat_tx = nat_tx.clone();
            Box::pin(async move {
                if let Some(frame) = r.route(&seg_name, frame).await {
                    let _ = nat_tx.send(frame);
                }
            })
        }));
        let guest = switch.add_channel_port(PortClass::Guest { isolated: false });
        (
            Seg {
                switch,
                gateway,
                rules,
                guest,
            },
            nat_rx,
        )
    }

    async fn recv(rx: &mut mpsc::Receiver<Bytes>) -> Bytes {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a frame")
            .expect("port closed")
    }

    /// Announce a guest on its segment the way a booting guest does: an ARP
    /// request for its gateway, which the gateway overhears and answers.
    async fn announce(seg: &mut Seg, mac: MacAddr, ip: Ipv4Addr) {
        let req = arp_request_build(mac, ip, seg.gateway.gw_ip());
        seg.guest.tx.send(Bytes::from(req)).await.unwrap();
        let reply = recv(&mut seg.guest.rx).await;
        assert_eq!(EthView::parse(&reply).unwrap().ethertype(), ETHERTYPE_ARP);
    }

    fn syn(src: Ipv4Addr, dst: Ipv4Addr, port: u16, ttl: u8) -> Vec<u8> {
        let tcp = tcp_build(
            src,
            dst,
            TcpFields {
                src_port: 40000,
                dst_port: port,
                seq: 1,
                ack: 0,
                flags: TCP_SYN,
                window: 65535,
                options: &[],
            },
            &[],
        )
        .unwrap();
        ipv4_build(src, dst, IPPROTO_TCP, ttl, &tcp, 7).unwrap()
    }

    /// A guest's frame to its gateway MAC.
    async fn send(seg: &Seg, mac: MacAddr, packet: &[u8]) {
        let frame = eth_build(seg.gateway.gw_mac(), mac, ETHERTYPE_IPV4, packet);
        seg.guest.tx.send(Bytes::from(frame)).await.unwrap();
    }

    #[tokio::test]
    async fn a_connected_pair_routes_both_ways_keeping_the_source() {
        let router = Router::new();
        let (mut a, _) = segment(&router, "a", 1500);
        let (mut b, _) = segment(&router, "b", 1500);
        // Declared on one side, connected both ways.
        router.connect("b", "a").unwrap();
        announce(&mut a, MAC_A, IP_A).await;
        announce(&mut b, MAC_B, IP_B).await;

        send(&a, MAC_A, &syn(IP_A, IP_B, 80, 64)).await;
        let got = recv(&mut b.guest.rx).await;
        let eth = EthView::parse(&got).unwrap();
        assert_eq!(eth.src_mac(), b.gateway.gw_mac(), "leaves b's gateway");
        assert_eq!(eth.dst_mac(), MAC_B);
        let ip = Ipv4View::parse(eth.payload()).unwrap();
        assert_eq!(ip.src(), IP_A, "no NAT: the source is the client's own");
        assert_eq!(ip.dst(), IP_B);
        assert_eq!(ip.ttl(), 63, "one hop");
        assert!(ip.checksum_valid());

        send(&b, MAC_B, &syn(IP_B, IP_A, 22, 64)).await;
        let got = recv(&mut a.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&got).unwrap().payload()).unwrap();
        assert_eq!((ip.src(), ip.dst()), (IP_B, IP_A));
    }

    #[tokio::test]
    async fn an_unknown_next_hop_is_asked_for_with_arp_and_released_on_the_answer() {
        let router = Router::new();
        let (a, _) = segment(&router, "a", 1500);
        let (mut b, _) = segment(&router, "b", 1500);
        router.connect("a", "b").unwrap();
        // b's guest has never spoken: the gateway does not know its MAC.
        let mut a = a;
        announce(&mut a, MAC_A, IP_A).await;
        send(&a, MAC_A, &syn(IP_A, IP_B, 80, 64)).await;

        let ask = recv(&mut b.guest.rx).await;
        let arp = ArpView::parse(EthView::parse(&ask).unwrap().payload()).unwrap();
        assert_eq!(arp.op(), ArpOp::Request);
        assert_eq!(arp.tpa(), IP_B);
        assert_eq!(arp.spa(), b.gateway.gw_ip());

        let answer = arp_reply_build(MAC_B, IP_B, b.gateway.gw_mac(), b.gateway.gw_ip());
        b.guest.tx.send(Bytes::from(answer)).await.unwrap();
        let got = recv(&mut b.guest.rx).await;
        let eth = EthView::parse(&got).unwrap();
        assert_eq!(eth.dst_mac(), MAC_B);
        assert_eq!(Ipv4View::parse(eth.payload()).unwrap().src(), IP_A);
    }

    #[tokio::test]
    async fn an_unconnected_segment_is_not_routed_to() {
        let router = Router::new();
        let (a, mut nat_a) = segment(&router, "a", 1500);
        let (_b, _) = segment(&router, "b", 1500);
        let (mut c, _) = segment(&router, "c", 1500);
        router.connect("a", "b").unwrap();
        announce(&mut c, MacAddr([2, 0, 0, 0, 0, 0x0C]), IP_C).await;

        // a -> c: not a pair, so it falls through to a's NAT, untouched.
        let packet = syn(IP_A, IP_C, 80, 64);
        send(&a, MAC_A, &packet).await;
        let fell = timeout(Duration::from_secs(2), nat_a.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(EthView::parse(&fell).unwrap().payload(), &packet[..]);
        assert!(
            timeout(Duration::from_millis(200), c.guest.rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn disconnecting_stops_routing_and_the_dhcp_route() {
        let router = Router::new();
        let (a, mut nat_a) = segment(&router, "a", 1500);
        let (_b, _) = segment(&router, "b", 1500);
        router.connect("a", "b").unwrap();
        assert!(router.connected("b", "a"));
        router.disconnect("b", "a").unwrap();
        assert!(!router.connected("a", "b"));
        assert!(router.dhcp_routes("a").is_empty());
        assert!(
            a.gateway
                .dhcp_server()
                .unwrap()
                .lock()
                .unwrap()
                .routes()
                .is_empty()
        );

        send(&a, MAC_A, &syn(IP_A, IP_B, 80, 64)).await;
        assert!(
            timeout(Duration::from_secs(2), nat_a.recv())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn option_121_offers_each_peer_via_the_segments_own_gateway() {
        let router = Router::new();
        let (a, _) = segment(&router, "a", 1500);
        let (b, _) = segment(&router, "b", 1500);
        let (_c, _) = segment(&router, "c", 1500);
        router.connect("a", "b").unwrap();
        router.connect("a", "c").unwrap();

        let gw_a = a.gateway.gw_ip();
        assert_eq!(
            router.dhcp_routes("a"),
            vec![(subnet("b"), gw_a), (subnet("c"), gw_a)]
        );
        assert_eq!(
            router.dhcp_routes("b"),
            vec![(subnet("a"), b.gateway.gw_ip())]
        );
        // And the DHCP server offers exactly that from now on.
        assert_eq!(
            a.gateway.dhcp_server().unwrap().lock().unwrap().routes(),
            &[(subnet("b"), gw_a), (subnet("c"), gw_a)]
        );
    }

    #[tokio::test]
    async fn a_declared_route_keeps_its_place_and_wins_over_the_daemons() {
        let router = Router::new();
        let (_b, _) = segment(&router, "b", 1500);
        let switch = Switch::new("a".into());
        let gw = Gateway::spawn(
            &switch,
            GatewayConfig {
                segment_name: "a".into(),
                lab_name: "lab".into(),
                gw_ip: Ipv4Addr::new(10, 1, 0, 1),
                gw_mac: gateway_mac("lab", "a"),
                dhcp: None,
                dns: None,
                upstream_dns: None,
            },
        );
        let via_vm = Ipv4Addr::new(10, 1, 0, 254);
        let elsewhere: Ipv4Net = "10.99.0.0/16".parse().unwrap();
        router.register(Leg {
            name: "a".into(),
            subnet: subnet("a"),
            gw_ip: gw.gw_ip(),
            mtu: 1500,
            port: gw.l3_port(),
            rules: Arc::new(Mutex::new(RuleSet::new())),
            dhcp: None,
            declared_routes: vec![(elsewhere, via_vm), (subnet("b"), via_vm)],
        });
        router.connect("a", "b").unwrap();
        assert_eq!(
            router.dhcp_routes("a"),
            vec![(elsewhere, via_vm), (subnet("b"), via_vm)]
        );
    }

    #[tokio::test]
    async fn connect_refuses_itself_and_unknown_segments_by_name() {
        let router = Router::new();
        let (_a, _) = segment(&router, "a", 1500);
        assert!(router.connect("a", "a").unwrap_err().contains("\"a\""));
        let e = router.connect("a", "g").unwrap_err();
        assert!(e.contains("\"g\""), "{e}");
    }

    #[tokio::test]
    async fn the_peer_gateway_answers_a_ping() {
        let router = Router::new();
        let (mut a, _) = segment(&router, "a", 1500);
        let (b, _) = segment(&router, "b", 1500);
        router.connect("a", "b").unwrap();
        let echo = icmp_build(ICMP_ECHO_REQUEST, 0, [0, 1, 0, 1], b"hi");
        let packet = ipv4_build(IP_A, b.gateway.gw_ip(), IPPROTO_ICMP, 64, &echo, 3).unwrap();
        send(&a, MAC_A, &packet).await;
        let got = recv(&mut a.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&got).unwrap().payload()).unwrap();
        assert_eq!((ip.src(), ip.dst()), (b.gateway.gw_ip(), IP_A));
        assert_eq!(
            IcmpView::parse(ip.payload()).unwrap().icmp_type(),
            ICMP_ECHO_REPLY
        );
    }

    #[tokio::test]
    async fn an_expiring_ttl_is_answered_with_time_exceeded() {
        let router = Router::new();
        let (mut a, _) = segment(&router, "a", 1500);
        let (_b, _) = segment(&router, "b", 1500);
        router.connect("a", "b").unwrap();
        send(&a, MAC_A, &syn(IP_A, IP_B, 80, 1)).await;
        let got = recv(&mut a.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&got).unwrap().payload()).unwrap();
        assert_eq!((ip.src(), ip.dst()), (a.gateway.gw_ip(), IP_A));
        assert_eq!(
            IcmpView::parse(ip.payload()).unwrap().icmp_type(),
            ICMP_TIME_EXCEEDED
        );
    }

    #[tokio::test]
    async fn a_packet_over_the_peer_mtu_with_df_is_told_the_mtu() {
        let router = Router::new();
        let (mut a, _) = segment(&router, "a", 9000);
        let (_b, _) = segment(&router, "b", 1400);
        router.connect("a", "b").unwrap();
        let echo = icmp_build(ICMP_ECHO_REQUEST, 0, [0, 1, 0, 1], &[0u8; 2000]);
        let mut packet = ipv4_build(IP_A, IP_B, IPPROTO_ICMP, 64, &echo, 3).unwrap();
        packet[6] = 0x40; // DF
        packet[10..12].copy_from_slice(&[0, 0]);
        let csum = internet_checksum(&packet[..20]);
        packet[10..12].copy_from_slice(&csum.to_be_bytes());
        send(&a, MAC_A, &packet).await;
        let got = recv(&mut a.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&got).unwrap().payload()).unwrap();
        let icmp = IcmpView::parse(ip.payload()).unwrap();
        assert_eq!(
            (icmp.icmp_type(), icmp.code()),
            (ICMP_DEST_UNREACHABLE, ICMP_FRAG_NEEDED)
        );
        assert_eq!(u16::from_be_bytes([icmp.rest()[2], icmp.rest()[3]]), 1400);
    }

    /// D4: the leaving segment's `block` rule stops one port across the
    /// route and leaves the rest routed. The rule runs in the switch's
    /// ingress hook, as installed by `netservices`; the hook here is the
    /// same rule engine at the same place.
    #[tokio::test]
    async fn the_leaving_segments_block_rule_holds_across_the_route() {
        let router = Router::new();
        let (mut a, _) = segment(&router, "a", 1500);
        let (mut b, _) = segment(&router, "b", 1500);
        router.connect("a", "b").unwrap();
        announce(&mut a, MAC_A, IP_A).await;
        announce(&mut b, MAC_B, IP_B).await;
        a.rules.lock().unwrap().add_block(BlockRule {
            cidr: subnet("b"),
            proto: Some(L4Proto::Tcp),
            port: Some(8082),
            span: (0, 0),
        });
        let hook_rules = a.rules.clone();
        let gw_mac = a.gateway.gw_mac();
        a.switch.set_ingress_hook(Box::new(move |_, _, frame| {
            use crate::net::switch::HookAction;
            let eth = EthView::parse(frame).unwrap();
            if eth.dst_mac() != gw_mac || eth.ethertype() != ETHERTYPE_IPV4 {
                return HookAction::Pass;
            }
            match hook_rules.lock().unwrap().eval(eth.payload()) {
                Verdict::Drop { reply } => HookAction::Inject {
                    forward: None,
                    reply: reply
                        .map(|ip| {
                            vec![Bytes::from(eth_build(
                                eth.src_mac(),
                                gw_mac,
                                ETHERTYPE_IPV4,
                                &ip,
                            ))]
                        })
                        .unwrap_or_default(),
                },
                _ => HookAction::Pass,
            }
        }));

        send(&a, MAC_A, &syn(IP_A, IP_B, 8082, 64)).await;
        let rst = recv(&mut a.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&rst).unwrap().payload()).unwrap();
        assert_ne!(TcpView::parse(ip.payload()).unwrap().flags() & TCP_RST, 0);
        assert!(
            timeout(Duration::from_millis(200), b.guest.rx.recv())
                .await
                .is_err()
        );

        send(&a, MAC_A, &syn(IP_A, IP_B, 8081, 64)).await;
        let got = recv(&mut b.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&got).unwrap().payload()).unwrap();
        assert_eq!(TcpView::parse(ip.payload()).unwrap().dst_port(), 8081);
    }

    /// A redirect on `a` aimed into `b` is routed, and `b`'s answer is
    /// un-DNATed on the way back into `a`.
    #[tokio::test]
    async fn a_redirect_into_the_peer_is_undone_on_the_reply() {
        let router = Router::new();
        let (mut a, _) = segment(&router, "a", 1500);
        let (mut b, _) = segment(&router, "b", 1500);
        router.connect("a", "b").unwrap();
        announce(&mut a, MAC_A, IP_A).await;
        announce(&mut b, MAC_B, IP_B).await;
        let virt = Ipv4Addr::new(192, 0, 2, 10);
        a.rules.lock().unwrap().add_redirect(RedirectRule {
            from: HostPort {
                ip: virt,
                port: Some(80),
            },
            to: HostPort {
                ip: IP_B,
                port: Some(8080),
            },
            proto: None,
            span: (0, 0),
        });
        // The ingress hook's rewrite, applied by hand.
        let Verdict::Rewrite(dnat) = a.rules.lock().unwrap().eval(&syn(IP_A, virt, 80, 64)) else {
            panic!("redirect did not match");
        };
        send(&a, MAC_A, &dnat).await;
        let got = recv(&mut b.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&got).unwrap().payload()).unwrap();
        assert_eq!(ip.dst(), IP_B);

        let tcp = tcp_build(
            IP_B,
            IP_A,
            TcpFields {
                src_port: 8080,
                dst_port: 40000,
                seq: 9,
                ack: 2,
                flags: TCP_SYN | 0x10,
                window: 65535,
                options: &[],
            },
            &[],
        )
        .unwrap();
        send(
            &b,
            MAC_B,
            &ipv4_build(IP_B, IP_A, IPPROTO_TCP, 64, &tcp, 8).unwrap(),
        )
        .await;
        let back = recv(&mut a.guest.rx).await;
        let ip = Ipv4View::parse(EthView::parse(&back).unwrap().payload()).unwrap();
        assert_eq!(
            ip.src(),
            virt,
            "the reply comes from the address the client dialled"
        );
        assert_eq!(TcpView::parse(ip.payload()).unwrap().src_port(), 80);
    }
}
