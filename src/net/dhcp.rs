//! DHCP server state machine for segment gateways (PRD §9.4).
//!
//! Sans-I/O: [`DhcpServer::handle`] consumes a raw ethernet frame and
//! returns the raw ethernet reply frame (or `None`), so the whole protocol
//! is unit-testable without sockets. The gateway service ([`super::gateway`])
//! wires it to a switch port.
//!
//! Served options: subnet mask (1), router (3), DNS (6, omitted when
//! unconfigured), domain (15), lease time (51), message type (53), server
//! identifier (54), and RFC 3442 classless static routes (121) when the
//! segment declares `routes {}`.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use ipnet::Ipv4Net;
use tracing::{debug, warn};

use crate::config::model::MacAddr;
use crate::net::frame::{
    ETHERTYPE_IPV4, EthView, IPPROTO_UDP, Ipv4View, MAC_BROADCAST, UdpView, eth_build, ipv4_build,
    udp_build,
};

/// UDP port the server listens on.
pub const DHCP_SERVER_PORT: u16 = 67;
/// UDP port clients send from and receive on.
pub const DHCP_CLIENT_PORT: u16 = 68;

pub const DHCP_DISCOVER: u8 = 1;
pub const DHCP_OFFER: u8 = 2;
pub const DHCP_REQUEST: u8 = 3;
pub const DHCP_DECLINE: u8 = 4;
pub const DHCP_ACK: u8 = 5;
pub const DHCP_NAK: u8 = 6;
pub const DHCP_RELEASE: u8 = 7;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
/// BOOTP fixed header (236 bytes) plus the magic cookie (4 bytes).
const BOOTP_MIN_LEN: usize = 240;
/// The broadcast bit in the BOOTP `flags` field.
const FLAG_BROADCAST: u16 = 0x8000;

const OPT_PAD: u8 = 0;
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_DOMAIN: u8 = 15;
const OPT_MTU: u8 = 26;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_CLASSLESS_ROUTES: u8 = 121;
const OPT_END: u8 = 255;

/// Configuration of one segment's DHCP service.
#[derive(Debug, Clone)]
pub struct DhcpConfig {
    pub subnet: Ipv4Net,
    /// The daemon gateway service's IP — used as the DHCP server identifier
    /// and excluded from the dynamic pool.
    pub gateway: Ipv4Addr,
    /// Default router advertised with DHCP option 3. Usually the daemon
    /// gateway, but may be a designated VM/container NIC on the segment.
    pub router: Ipv4Addr,
    /// The MAC the server's reply frames are sent from (the gateway MAC).
    pub server_mac: MacAddr,
    /// DNS server pushed via option 6; `None` omits the option entirely.
    pub dns_server: Option<Ipv4Addr>,
    /// Domain suffix pushed via option 15.
    pub domain: Option<String>,
    /// Classless static routes pushed via option 121 (PRD §9.6 mechanism 1).
    pub routes: Vec<(Ipv4Net, Ipv4Addr)>,
    /// Static reservations keyed on client MAC; always win over the dynamic
    /// pool and may sit outside it (PRD §9.4).
    pub reservations: HashMap<MacAddr, Ipv4Addr>,
    pub lease_secs: u32,
    /// Interface MTU pushed via option 26. Lets Linux guests raise their NIC
    /// MTU to match a jumbo segment (Windows generally ignores this and relies
    /// on virtio-net `host_mtu`).
    pub mtu: u16,
}

impl DhcpConfig {
    /// A config with no DNS/domain/routes/reservations and the default
    /// 3600-second lease.
    pub fn new(subnet: Ipv4Net, gateway: Ipv4Addr, server_mac: MacAddr) -> Self {
        Self {
            subnet,
            gateway,
            router: gateway,
            server_mac,
            dns_server: None,
            domain: None,
            routes: Vec::new(),
            reservations: HashMap::new(),
            lease_secs: 3600,
            mtu: 1500,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Lease {
    ip: Ipv4Addr,
    expiry: Instant,
}

/// The DHCP server state machine: a lease table over a [`DhcpConfig`].
#[derive(Debug)]
pub struct DhcpServer {
    config: DhcpConfig,
    leases: HashMap<MacAddr, Lease>,
    /// Addresses quarantined by client DECLINEs (duplicate detection).
    declined: HashSet<Ipv4Addr>,
}

/// A parsed inbound BOOTP/DHCP request.
#[derive(Debug, Clone, Copy)]
struct BootpRequest {
    xid: u32,
    flags: u16,
    ciaddr: Ipv4Addr,
    giaddr: Ipv4Addr,
    chaddr: MacAddr,
    msg_type: u8,
    requested_ip: Option<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
}

impl DhcpServer {
    pub fn new(config: DhcpConfig) -> Self {
        Self {
            config,
            leases: HashMap::new(),
            declined: HashSet::new(),
        }
    }

    /// Process one ethernet frame. Returns the full ethernet reply frame for
    /// DISCOVER/REQUEST that warrant one (OFFER/ACK/NAK); `None` for
    /// non-DHCP traffic, RELEASE, DECLINE, and pool exhaustion.
    pub fn handle(&mut self, eth_frame: &[u8]) -> Option<Vec<u8>> {
        let eth = EthView::parse(eth_frame)?;
        if eth.ethertype() != ETHERTYPE_IPV4 {
            return None;
        }
        let ip = Ipv4View::parse(eth.payload())?;
        if ip.proto() != IPPROTO_UDP {
            return None;
        }
        let udp = UdpView::parse(ip.payload())?;
        if udp.src_port() != DHCP_CLIENT_PORT || udp.dst_port() != DHCP_SERVER_PORT {
            return None;
        }
        let req = parse_bootp(udp.payload())?;
        self.purge_expired();
        match req.msg_type {
            DHCP_DISCOVER => self.on_discover(&req),
            DHCP_REQUEST => self.on_request(&req),
            DHCP_RELEASE => {
                if self.leases.remove(&req.chaddr).is_some() {
                    debug!(mac = %req.chaddr, "DHCP release");
                }
                None
            }
            DHCP_DECLINE => {
                self.on_decline(&req);
                None
            }
            other => {
                debug!(msg_type = other, mac = %req.chaddr, "ignoring DHCP message");
                None
            }
        }
    }

    /// Current (unexpired) leases, sorted by address, for status display.
    pub fn leases(&self) -> Vec<(MacAddr, Ipv4Addr)> {
        let now = Instant::now();
        let mut v: Vec<(MacAddr, Ipv4Addr)> = self
            .leases
            .iter()
            .filter(|(_, l)| l.expiry > now)
            .map(|(m, l)| (*m, l.ip))
            .collect();
        v.sort_by_key(|(_, ip)| u32::from(*ip));
        v
    }

    /// The valid lease held by `mac`, if any.
    pub fn lease_of(&self, mac: MacAddr) -> Option<Ipv4Addr> {
        let now = Instant::now();
        self.leases
            .get(&mac)
            .filter(|l| l.expiry > now)
            .map(|l| l.ip)
    }

    // -- message handling -----------------------------------------------------

    fn on_discover(&mut self, req: &BootpRequest) -> Option<Vec<u8>> {
        let Some(ip) = self.ip_for(req.chaddr) else {
            warn!(
                subnet = %self.config.subnet,
                mac = %req.chaddr,
                "DHCP pool exhausted, no offer"
            );
            return None;
        };
        self.grant(req.chaddr, ip);
        debug!(mac = %req.chaddr, %ip, "DHCP offer");
        self.build_reply(req, DHCP_OFFER, Some(ip))
    }

    fn on_request(&mut self, req: &BootpRequest) -> Option<Vec<u8>> {
        // SELECTING with a different server: the client chose someone else.
        if let Some(sid) = req.server_id
            && sid != self.config.gateway
        {
            self.leases.remove(&req.chaddr);
            debug!(mac = %req.chaddr, server = %sid, "client selected another DHCP server");
            return None;
        }
        let requested = req
            .requested_ip
            .or_else(|| (!req.ciaddr.is_unspecified()).then_some(req.ciaddr));
        let entitled = self
            .config
            .reservations
            .get(&req.chaddr)
            .copied()
            .or_else(|| self.lease_of(req.chaddr));
        let granted = match (requested, entitled) {
            (None, Some(e)) => Some(e),
            (Some(r), Some(e)) => (r == e).then_some(e),
            (Some(r), None) => self.requestable(r).then_some(r),
            (None, None) => None,
        };
        match granted {
            Some(ip) => {
                self.grant(req.chaddr, ip);
                debug!(mac = %req.chaddr, %ip, "DHCP ack");
                self.build_reply(req, DHCP_ACK, Some(ip))
            }
            None => {
                debug!(mac = %req.chaddr, ?requested, "DHCP nak");
                self.build_reply(req, DHCP_NAK, None)
            }
        }
    }

    fn on_decline(&mut self, req: &BootpRequest) {
        let declined = self
            .leases
            .remove(&req.chaddr)
            .map(|l| l.ip)
            .or(req.requested_ip);
        if let Some(ip) = declined {
            warn!(mac = %req.chaddr, %ip, "DHCP decline; address quarantined");
            self.declined.insert(ip);
        }
    }

    // -- allocation -------------------------------------------------------------

    /// The address this client should get: reservation, then existing valid
    /// lease, then a fresh allocation from the dynamic pool.
    fn ip_for(&self, mac: MacAddr) -> Option<Ipv4Addr> {
        if let Some(&ip) = self.config.reservations.get(&mac) {
            return Some(ip);
        }
        if let Some(ip) = self.lease_of(mac) {
            return Some(ip);
        }
        self.alloc()
    }

    /// First free address in the dynamic pool: subnet hosts minus network/
    /// broadcast (already excluded by `hosts()`), gateway, reservations,
    /// valid leases, and quarantined addresses.
    fn alloc(&self) -> Option<Ipv4Addr> {
        let now = Instant::now();
        let taken: HashSet<Ipv4Addr> = self
            .leases
            .values()
            .filter(|l| l.expiry > now)
            .map(|l| l.ip)
            .chain(self.config.reservations.values().copied())
            .chain(std::iter::once(self.config.gateway))
            .chain(self.declined.iter().copied())
            .collect();
        self.config.subnet.hosts().find(|h| !taken.contains(h))
    }

    /// May a client with no reservation and no lease claim `ip`?
    fn requestable(&self, ip: Ipv4Addr) -> bool {
        let now = Instant::now();
        self.config.subnet.contains(&ip)
            && ip != self.config.subnet.network()
            && ip != self.config.subnet.broadcast()
            && ip != self.config.gateway
            && !self.config.reservations.values().any(|&r| r == ip)
            && !self.declined.contains(&ip)
            && !self.leases.values().any(|l| l.ip == ip && l.expiry > now)
    }

    fn grant(&mut self, mac: MacAddr, ip: Ipv4Addr) {
        let expiry = Instant::now() + Duration::from_secs(u64::from(self.config.lease_secs));
        self.leases.insert(mac, Lease { ip, expiry });
    }

    fn purge_expired(&mut self) {
        let now = Instant::now();
        self.leases.retain(|_, l| l.expiry > now);
    }

    // -- reply construction -------------------------------------------------------

    fn build_reply(
        &self,
        req: &BootpRequest,
        msg_type: u8,
        yiaddr: Option<Ipv4Addr>,
    ) -> Option<Vec<u8>> {
        let cfg = &self.config;
        let yiaddr = yiaddr.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let ciaddr = if msg_type == DHCP_ACK {
            req.ciaddr
        } else {
            Ipv4Addr::UNSPECIFIED
        };

        let mut b = Vec::with_capacity(320);
        b.push(BOOTREPLY);
        b.push(1); // htype: ethernet
        b.push(6); // hlen
        b.push(0); // hops
        b.extend_from_slice(&req.xid.to_be_bytes());
        b.extend_from_slice(&[0, 0]); // secs
        b.extend_from_slice(&req.flags.to_be_bytes());
        b.extend_from_slice(&ciaddr.octets());
        b.extend_from_slice(&yiaddr.octets());
        b.extend_from_slice(&cfg.gateway.octets()); // siaddr
        b.extend_from_slice(&req.giaddr.octets());
        b.extend_from_slice(&req.chaddr.0);
        b.extend_from_slice(&[0u8; 10]); // chaddr padding
        b.extend_from_slice(&[0u8; 64]); // sname
        b.extend_from_slice(&[0u8; 128]); // file
        b.extend_from_slice(&MAGIC_COOKIE);

        opt(&mut b, OPT_MSG_TYPE, &[msg_type]);
        opt(&mut b, OPT_SERVER_ID, &cfg.gateway.octets());
        if msg_type != DHCP_NAK {
            opt(&mut b, OPT_LEASE_TIME, &cfg.lease_secs.to_be_bytes());
            opt(&mut b, OPT_SUBNET_MASK, &cfg.subnet.netmask().octets());
            opt(&mut b, OPT_ROUTER, &cfg.router.octets());
            if let Some(dns) = cfg.dns_server {
                opt(&mut b, OPT_DNS, &dns.octets());
            }
            if let Some(domain) = &cfg.domain {
                opt(&mut b, OPT_DOMAIN, domain.as_bytes());
            }
            if cfg.mtu != 1500 {
                opt(&mut b, OPT_MTU, &cfg.mtu.to_be_bytes());
            }
            if !cfg.routes.is_empty() {
                opt(
                    &mut b,
                    OPT_CLASSLESS_ROUTES,
                    &encode_classless_routes(&cfg.routes),
                );
            }
        }
        b.push(OPT_END);

        // NAKs are always broadcast (the client has no usable address);
        // otherwise honour the client's broadcast flag.
        let broadcast =
            msg_type == DHCP_NAK || req.flags & FLAG_BROADCAST != 0 || yiaddr.is_unspecified();
        let (dst_mac, dst_ip) = if broadcast {
            (MAC_BROADCAST, Ipv4Addr::BROADCAST)
        } else {
            (req.chaddr, yiaddr)
        };
        let udp = udp_build(cfg.gateway, dst_ip, DHCP_SERVER_PORT, DHCP_CLIENT_PORT, &b)?;
        let ip = ipv4_build(
            cfg.gateway,
            dst_ip,
            IPPROTO_UDP,
            64,
            &udp,
            (req.xid & 0xFFFF) as u16,
        )?;
        Some(eth_build(dst_mac, cfg.server_mac, ETHERTYPE_IPV4, &ip))
    }
}

/// Append a TLV option.
fn opt(out: &mut Vec<u8>, code: u8, val: &[u8]) {
    debug_assert!(val.len() <= 255, "DHCP option too long");
    out.push(code);
    out.push(val.len() as u8);
    out.extend_from_slice(val);
}

/// RFC 3442 classless-static-routes encoding: for each route, the prefix
/// length, the `ceil(len/8)` significant octets of the destination, then the
/// four router octets.
pub fn encode_classless_routes(routes: &[(Ipv4Net, Ipv4Addr)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (net, via) in routes {
        let plen = net.prefix_len();
        out.push(plen);
        let significant = usize::from(plen).div_ceil(8);
        out.extend_from_slice(&net.network().octets()[..significant]);
        out.extend_from_slice(&via.octets());
    }
    out
}

fn ipv4_from(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

/// Parse a BOOTP/DHCP request (the UDP payload). Returns `None` for replies,
/// non-ethernet hardware, missing cookie, or any truncation.
fn parse_bootp(p: &[u8]) -> Option<BootpRequest> {
    if p.len() < BOOTP_MIN_LEN {
        return None;
    }
    if p[0] != BOOTREQUEST || p[1] != 1 || p[2] != 6 {
        return None;
    }
    if p[236..240] != MAGIC_COOKIE {
        return None;
    }
    let xid = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    let flags = u16::from_be_bytes([p[10], p[11]]);
    let ciaddr = ipv4_from(&p[12..16]);
    let giaddr = ipv4_from(&p[24..28]);
    let mut chaddr = [0u8; 6];
    chaddr.copy_from_slice(&p[28..34]);

    let mut msg_type = None;
    let mut requested_ip = None;
    let mut server_id = None;
    let mut i = 240;
    while i < p.len() {
        match p[i] {
            OPT_PAD => i += 1,
            OPT_END => break,
            code => {
                let len = usize::from(*p.get(i + 1)?);
                let end = i + 2 + len;
                let val = p.get(i + 2..end)?;
                match code {
                    OPT_MSG_TYPE if len == 1 => msg_type = Some(val[0]),
                    OPT_REQUESTED_IP if len == 4 => requested_ip = Some(ipv4_from(val)),
                    OPT_SERVER_ID if len == 4 => server_id = Some(ipv4_from(val)),
                    _ => {}
                }
                i = end;
            }
        }
    }
    Some(BootpRequest {
        xid,
        flags,
        ciaddr,
        giaddr,
        chaddr: MacAddr(chaddr),
        msg_type: msg_type?,
        requested_ip,
        server_id,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GW: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 1);
    const GW_MAC: MacAddr = MacAddr([0x52, 0x56, 0, 0, 0, 1]);
    const XID: u32 = 0x4142_4344;

    fn mac(n: u8) -> MacAddr {
        MacAddr([0x02, 0, 0, 0, 0, n])
    }

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn config() -> DhcpConfig {
        let mut c = DhcpConfig::new(net("10.213.1.0/24"), GW, GW_MAC);
        c.dns_server = Some(GW);
        c.domain = Some("vmlab.internal".into());
        c
    }

    /// Build a full client→server ethernet frame carrying a DHCP message.
    fn client_frame(
        mac: MacAddr,
        msg_type: u8,
        extra_opts: &[(u8, &[u8])],
        ciaddr: Ipv4Addr,
        broadcast_flag: bool,
    ) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        b[0] = BOOTREQUEST;
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&XID.to_be_bytes());
        if broadcast_flag {
            b[10] = 0x80;
        }
        b[12..16].copy_from_slice(&ciaddr.octets());
        b[28..34].copy_from_slice(&mac.0);
        b[236..240].copy_from_slice(&MAGIC_COOKIE);
        b.extend_from_slice(&[OPT_MSG_TYPE, 1, msg_type]);
        for (code, val) in extra_opts {
            b.push(*code);
            b.push(val.len() as u8);
            b.extend_from_slice(val);
        }
        b.push(OPT_END);
        let src_ip = if ciaddr.is_unspecified() {
            Ipv4Addr::UNSPECIFIED
        } else {
            ciaddr
        };
        let udp = udp_build(src_ip, Ipv4Addr::BROADCAST, 68, 67, &b).unwrap();
        let ip = ipv4_build(src_ip, Ipv4Addr::BROADCAST, IPPROTO_UDP, 64, &udp, 1).unwrap();
        eth_build(MAC_BROADCAST, mac, ETHERTYPE_IPV4, &ip)
    }

    fn discover(mac: MacAddr) -> Vec<u8> {
        client_frame(mac, DHCP_DISCOVER, &[], Ipv4Addr::UNSPECIFIED, false)
    }

    fn request(mac: MacAddr, ip: Ipv4Addr) -> Vec<u8> {
        client_frame(
            mac,
            DHCP_REQUEST,
            &[
                (OPT_REQUESTED_IP, &ip.octets()),
                (OPT_SERVER_ID, &GW.octets()),
            ],
            Ipv4Addr::UNSPECIFIED,
            false,
        )
    }

    /// Parsed server reply for assertions.
    struct Reply {
        eth_dst: MacAddr,
        eth_src: MacAddr,
        ip_dst: Ipv4Addr,
        xid: u32,
        yiaddr: Ipv4Addr,
        chaddr: [u8; 6],
        opts: HashMap<u8, Vec<u8>>,
    }

    fn parse_reply(frame: &[u8]) -> Reply {
        let eth = EthView::parse(frame).unwrap();
        assert_eq!(eth.ethertype(), ETHERTYPE_IPV4);
        let ip = Ipv4View::parse(eth.payload()).unwrap();
        assert!(ip.checksum_valid());
        let udp = UdpView::parse(ip.payload()).unwrap();
        assert!(udp.checksum_valid(ip.src(), ip.dst()));
        assert_eq!(udp.src_port(), 67);
        assert_eq!(udp.dst_port(), 68);
        let p = udp.payload();
        assert_eq!(p[0], BOOTREPLY);
        assert_eq!(&p[236..240], &MAGIC_COOKIE);
        let mut chaddr = [0u8; 6];
        chaddr.copy_from_slice(&p[28..34]);
        let mut opts = HashMap::new();
        let mut i = 240;
        while i < p.len() && p[i] != OPT_END {
            if p[i] == OPT_PAD {
                i += 1;
                continue;
            }
            let len = usize::from(p[i + 1]);
            opts.insert(p[i], p[i + 2..i + 2 + len].to_vec());
            i += 2 + len;
        }
        // Replies always come from the configured gateway (the server id).
        assert_eq!(ip.src().octets().to_vec(), opts[&OPT_SERVER_ID]);
        Reply {
            eth_dst: eth.dst_mac(),
            eth_src: eth.src_mac(),
            ip_dst: ip.dst(),
            xid: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
            yiaddr: ipv4_from(&p[16..20]),
            chaddr,
            opts,
        }
    }

    #[test]
    fn discover_gets_offer_with_options() {
        let mut s = DhcpServer::new(config());
        let reply = s.handle(&discover(mac(1))).expect("offer expected");
        let r = parse_reply(&reply);
        assert_eq!(r.opts[&OPT_MSG_TYPE], vec![DHCP_OFFER]);
        assert_eq!(r.yiaddr, Ipv4Addr::new(10, 213, 1, 2)); // first free host
        assert_eq!(r.xid, XID);
        assert_eq!(r.chaddr, mac(1).0);
        // Unicast addressing (no broadcast flag).
        assert_eq!(r.eth_dst, mac(1));
        assert_eq!(r.eth_src, GW_MAC);
        assert_eq!(r.ip_dst, r.yiaddr);
        // Options.
        assert_eq!(r.opts[&OPT_SUBNET_MASK], vec![255, 255, 255, 0]);
        assert_eq!(r.opts[&OPT_ROUTER], GW.octets().to_vec());
        assert_eq!(r.opts[&OPT_DNS], GW.octets().to_vec());
        assert_eq!(r.opts[&OPT_DOMAIN], b"vmlab.internal".to_vec());
        assert_eq!(r.opts[&OPT_LEASE_TIME], 3600u32.to_be_bytes().to_vec());
        assert_eq!(r.opts[&OPT_SERVER_ID], GW.octets().to_vec());
        assert!(!r.opts.contains_key(&OPT_CLASSLESS_ROUTES));
        // Default MTU is 1500 — option 26 is omitted.
        assert!(!r.opts.contains_key(&OPT_MTU));
    }

    #[test]
    fn external_router_does_not_replace_dhcp_server_identity() {
        let router = Ipv4Addr::new(10, 213, 1, 254);
        let mut cfg = config();
        cfg.router = router;
        let mut server = DhcpServer::new(cfg);
        let reply = parse_reply(&server.handle(&discover(mac(1))).expect("offer expected"));
        assert_eq!(reply.opts[&OPT_ROUTER], router.octets().to_vec());
        assert_eq!(reply.opts[&OPT_SERVER_ID], GW.octets().to_vec());
        assert_eq!(reply.eth_src, GW_MAC);
    }

    #[test]
    fn jumbo_mtu_sets_option_26() {
        let mut cfg = config();
        cfg.mtu = 9000;
        let mut s = DhcpServer::new(cfg);
        let r = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(r.opts[&OPT_MTU], 9000u16.to_be_bytes().to_vec());
    }

    #[test]
    fn dns_option_omitted_when_unconfigured() {
        let mut cfg = config();
        cfg.dns_server = None;
        cfg.domain = None;
        let mut s = DhcpServer::new(cfg);
        let r = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert!(!r.opts.contains_key(&OPT_DNS));
        assert!(!r.opts.contains_key(&OPT_DOMAIN));
    }

    #[test]
    fn request_acks_and_records_lease() {
        let mut s = DhcpServer::new(config());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        let ack = parse_reply(&s.handle(&request(mac(1), offer.yiaddr)).unwrap());
        assert_eq!(ack.opts[&OPT_MSG_TYPE], vec![DHCP_ACK]);
        assert_eq!(ack.yiaddr, offer.yiaddr);
        assert_eq!(s.leases(), vec![(mac(1), offer.yiaddr)]);
        assert_eq!(s.lease_of(mac(1)), Some(offer.yiaddr));
        assert_eq!(s.lease_of(mac(2)), None);

        // Renewal: REQUEST with ciaddr set and no option 50.
        let renew = client_frame(mac(1), DHCP_REQUEST, &[], offer.yiaddr, false);
        let ack2 = parse_reply(&s.handle(&renew).unwrap());
        assert_eq!(ack2.opts[&OPT_MSG_TYPE], vec![DHCP_ACK]);
        assert_eq!(ack2.yiaddr, offer.yiaddr);
    }

    #[test]
    fn broadcast_flag_honoured() {
        let mut s = DhcpServer::new(config());
        let f = client_frame(mac(1), DHCP_DISCOVER, &[], Ipv4Addr::UNSPECIFIED, true);
        let r = parse_reply(&s.handle(&f).unwrap());
        assert_eq!(r.eth_dst, MAC_BROADCAST);
        assert_eq!(r.ip_dst, Ipv4Addr::BROADCAST);
    }

    #[test]
    fn reservation_wins_even_outside_pool() {
        let outside = Ipv4Addr::new(192, 168, 99, 5);
        let mut cfg = config();
        cfg.reservations.insert(mac(7), outside);
        let mut s = DhcpServer::new(cfg);
        let offer = parse_reply(&s.handle(&discover(mac(7))).unwrap());
        assert_eq!(offer.yiaddr, outside);
        let ack = parse_reply(&s.handle(&request(mac(7), outside)).unwrap());
        assert_eq!(ack.opts[&OPT_MSG_TYPE], vec![DHCP_ACK]);
        assert_eq!(ack.yiaddr, outside);
        // A reserved client requesting something else is NAKed.
        let nak = parse_reply(
            &s.handle(&request(mac(7), Ipv4Addr::new(10, 213, 1, 9)))
                .unwrap(),
        );
        assert_eq!(nak.opts[&OPT_MSG_TYPE], vec![DHCP_NAK]);
    }

    #[test]
    fn dynamic_allocation_skips_gateway_and_reservations() {
        let mut cfg = DhcpConfig::new(net("10.0.0.0/29"), Ipv4Addr::new(10, 0, 0, 1), GW_MAC);
        cfg.reservations.insert(mac(9), Ipv4Addr::new(10, 0, 0, 2));
        let mut s = DhcpServer::new(cfg);
        // .0 network, .1 gateway, .2 reserved => first dynamic is .3.
        let r = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(r.yiaddr, Ipv4Addr::new(10, 0, 0, 3));
        // And the next client gets .4.
        let r = parse_reply(&s.handle(&discover(mac(2))).unwrap());
        assert_eq!(r.yiaddr, Ipv4Addr::new(10, 0, 0, 4));
    }

    #[test]
    fn nak_on_taken_or_invalid_request() {
        let mut s = DhcpServer::new(config());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        s.handle(&request(mac(1), offer.yiaddr)).unwrap();

        // Another client requests the same (taken) address.
        let nak = parse_reply(&s.handle(&request(mac(2), offer.yiaddr)).unwrap());
        assert_eq!(nak.opts[&OPT_MSG_TYPE], vec![DHCP_NAK]);
        assert_eq!(nak.yiaddr, Ipv4Addr::UNSPECIFIED);
        // NAKs are broadcast.
        assert_eq!(nak.eth_dst, MAC_BROADCAST);
        assert_eq!(nak.ip_dst, Ipv4Addr::BROADCAST);
        // NAKs carry no lease options.
        assert!(!nak.opts.contains_key(&OPT_LEASE_TIME));

        // A stale request for an off-subnet address.
        let nak = parse_reply(
            &s.handle(&request(mac(3), Ipv4Addr::new(172, 16, 0, 5)))
                .unwrap(),
        );
        assert_eq!(nak.opts[&OPT_MSG_TYPE], vec![DHCP_NAK]);
        // Requesting the gateway itself is refused.
        let nak = parse_reply(&s.handle(&request(mac(4), GW)).unwrap());
        assert_eq!(nak.opts[&OPT_MSG_TYPE], vec![DHCP_NAK]);
        // Neither mac(2) nor the others were granted anything.
        assert_eq!(s.leases(), vec![(mac(1), offer.yiaddr)]);
    }

    #[test]
    fn free_request_for_unleased_pool_address_is_acked() {
        let mut s = DhcpServer::new(config());
        let want = Ipv4Addr::new(10, 213, 1, 42);
        let ack = parse_reply(&s.handle(&request(mac(1), want)).unwrap());
        assert_eq!(ack.opts[&OPT_MSG_TYPE], vec![DHCP_ACK]);
        assert_eq!(ack.yiaddr, want);
        assert_eq!(s.lease_of(mac(1)), Some(want));
    }

    #[test]
    fn release_frees_and_pool_exhaustion_warns() {
        // /30: hosts .1 (gateway) and .2 — a one-address pool.
        let cfg = DhcpConfig::new(net("10.9.0.0/30"), Ipv4Addr::new(10, 9, 0, 1), GW_MAC);
        let mut s = DhcpServer::new(cfg);
        let a = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(a.yiaddr, Ipv4Addr::new(10, 9, 0, 2));
        // Pool exhausted for a second client: no offer at all.
        assert!(s.handle(&discover(mac(2))).is_none());
        // First client releases; second can now allocate.
        let release = client_frame(mac(1), DHCP_RELEASE, &[], a.yiaddr, false);
        assert!(s.handle(&release).is_none());
        assert!(s.leases().is_empty());
        let b = parse_reply(&s.handle(&discover(mac(2))).unwrap());
        assert_eq!(b.yiaddr, Ipv4Addr::new(10, 9, 0, 2));
    }

    #[test]
    fn decline_quarantines_address() {
        let mut s = DhcpServer::new(config());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 213, 1, 2));
        let decline = client_frame(
            mac(1),
            DHCP_DECLINE,
            &[(OPT_REQUESTED_IP, &offer.yiaddr.octets())],
            Ipv4Addr::UNSPECIFIED,
            false,
        );
        assert!(s.handle(&decline).is_none());
        // The declined address is skipped from now on.
        let next = parse_reply(&s.handle(&discover(mac(2))).unwrap());
        assert_eq!(next.yiaddr, Ipv4Addr::new(10, 213, 1, 3));
    }

    #[test]
    fn same_mac_rediscover_keeps_its_address() {
        let mut s = DhcpServer::new(config());
        let first = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        s.handle(&request(mac(1), first.yiaddr)).unwrap();
        let again = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(again.yiaddr, first.yiaddr);
    }

    #[test]
    fn option_121_encoding_hand_verified() {
        let routes = vec![
            (net("10.0.0.0/8"), Ipv4Addr::new(10, 213, 1, 254)),
            (net("192.168.50.0/24"), Ipv4Addr::new(10, 213, 1, 253)),
            (net("0.0.0.0/0"), Ipv4Addr::new(10, 213, 1, 1)),
        ];
        // RFC 3442: prefix len, ceil(len/8) destination octets, router.
        let expected: Vec<u8> = vec![
            8, 10, 10, 213, 1, 254, // 10/8 via .254
            24, 192, 168, 50, 10, 213, 1, 253, // 192.168.50/24 via .253
            0, 10, 213, 1, 1, // default via .1
        ];
        assert_eq!(encode_classless_routes(&routes), expected);

        // And it rides option 121 in an OFFER.
        let mut cfg = config();
        cfg.routes = routes;
        let mut s = DhcpServer::new(cfg);
        let r = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(r.opts[&OPT_CLASSLESS_ROUTES], expected);
    }

    #[test]
    fn non_dhcp_frames_ignored() {
        let mut s = DhcpServer::new(config());
        // ARP frame.
        let arp = crate::net::frame::arp_request_build(mac(1), Ipv4Addr::UNSPECIFIED, GW);
        assert!(s.handle(&arp).is_none());
        // UDP to a different port.
        let udp = udp_build(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 123, b"ntp?").unwrap();
        let ip = ipv4_build(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            IPPROTO_UDP,
            64,
            &udp,
            1,
        )
        .unwrap();
        let f = eth_build(MAC_BROADCAST, mac(1), ETHERTYPE_IPV4, &ip);
        assert!(s.handle(&f).is_none());
        // Truncated BOOTP payload.
        let udp = udp_build(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            68,
            67,
            &[1, 1, 6, 0],
        )
        .unwrap();
        let ip = ipv4_build(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            IPPROTO_UDP,
            64,
            &udp,
            1,
        )
        .unwrap();
        let f = eth_build(MAC_BROADCAST, mac(1), ETHERTYPE_IPV4, &ip);
        assert!(s.handle(&f).is_none());
        // A BOOTP *reply* must not be answered (eth 14 + ip 20 + udp 8 = 42).
        let mut frame = discover(mac(1));
        frame[42] = BOOTREPLY;
        assert!(s.handle(&frame).is_none());
        // Truncation sweep never panics.
        let f = discover(mac(2));
        for n in 0..f.len() {
            let _ = s.handle(&f[..n]);
        }
    }

    /// Hostile option TLVs must never panic the parser: lengths running past
    /// the buffer, zero-length options, a missing END, and every possible
    /// single-byte corruption of a valid DISCOVER.
    #[test]
    fn hostile_options_never_panic() {
        let mut s = DhcpServer::new(config());

        // An option whose declared length runs past the end of the message.
        let mut b = vec![0u8; 240];
        b[0] = BOOTREQUEST;
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&XID.to_be_bytes());
        b[28..34].copy_from_slice(&mac(3).0);
        b[236..240].copy_from_slice(&MAGIC_COOKIE);
        b.extend_from_slice(&[OPT_MSG_TYPE, 200, DHCP_DISCOVER]); // len 200, 1 byte present
        let udp = udp_build(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &b).unwrap();
        let ip = ipv4_build(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            IPPROTO_UDP,
            64,
            &udp,
            1,
        )
        .unwrap();
        let f = eth_build(MAC_BROADCAST, mac(3), ETHERTYPE_IPV4, &ip);
        let _ = s.handle(&f);

        // Zero-length options and no END terminator.
        let mut b2 = b[..240].to_vec();
        b2.extend_from_slice(&[OPT_MSG_TYPE, 0, 42, 0, 42, 0]);
        let udp = udp_build(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &b2).unwrap();
        let ip = ipv4_build(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            IPPROTO_UDP,
            64,
            &udp,
            1,
        )
        .unwrap();
        let f = eth_build(MAC_BROADCAST, mac(3), ETHERTYPE_IPV4, &ip);
        let _ = s.handle(&f);

        // Single-byte corruption sweep over a valid DISCOVER: any reply that
        // does come back must still be a parseable frame.
        let good = discover(mac(4));
        for i in 0..good.len() {
            let mut f = good.clone();
            f[i] ^= 0xFF;
            if let Some(reply) = s.handle(&f) {
                assert!(EthView::parse(&reply).is_some());
            }
        }
    }
}
