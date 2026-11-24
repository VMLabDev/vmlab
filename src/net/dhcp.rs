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
//! segment declares `routes {}` — with the default route through the router
//! appended, since a client given option 121 ignores option 3. An option's
//! length field is one byte: routes past 255 bytes go out as several options
//! 121 split on route boundaries, which a client joins (RFC 3396), and a
//! domain past 255 bytes is not sent.
//!
//! A guest writes every byte of its frames, so the server takes a client's
//! identity from the frame and not from the payload alone: a message whose
//! `chaddr` is not its ethernet source is ignored, a DECLINE quarantines only
//! the address its client holds and only for one lease time, and an OFFER
//! holds its address for [`OFFER_HOLD`], the lease proper starting at the ACK.

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

/// How many declined addresses stay quarantined (review F6). A client can
/// take a lease and decline it without end, so past this a new decline
/// replaces the quarantine that ends first, and that address can be offered
/// again.
const MAX_DECLINED: usize = 1024;
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

/// How long an OFFER keeps its address for a client that has not sent the
/// REQUEST yet. A DISCOVER costs the pool one address for this long and not
/// for the whole lease, so addresses nobody confirmed come back by
/// themselves.
const OFFER_HOLD: Duration = Duration::from_secs(60);

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
    /// Addresses quarantined by client DECLINEs (duplicate detection), each
    /// with the time its quarantine ends. Only an address the declining
    /// client held gets here, and an entry lasts one lease time. A client
    /// can take a new lease and decline it again, so the map holds
    /// [`MAX_DECLINED`] (review F6).
    declined: HashMap<Ipv4Addr, Instant>,
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
        warn_on_long_domain(&config);
        Self {
            config,
            leases: HashMap::new(),
            declined: HashMap::new(),
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
        // A client speaks for its own NIC only. The segment has no relay
        // agent, so a `chaddr` that is not the frame's source is one guest
        // naming another machine: it could release, decline or re-select that
        // machine's lease, and take one address per invented MAC.
        if req.chaddr != eth.src_mac() {
            debug!(
                src = %eth.src_mac(),
                chaddr = %req.chaddr,
                "ignoring DHCP message whose chaddr is not its ethernet source"
            );
            return None;
        }
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

    /// Change the served configuration in place, keeping the lease table:
    /// a supervisor-owned global segment learns its reservations and MTU as
    /// labs attach and detach, long after the server started (PRD §9.2).
    pub fn reconfigure(&mut self, f: impl FnOnce(&mut DhcpConfig)) {
        let domain = self.config.domain.clone();
        f(&mut self.config);
        if self.config.domain != domain {
            warn_on_long_domain(&self.config);
        }
    }

    /// The configuration being served.
    #[cfg(test)]
    pub fn config(&self) -> &DhcpConfig {
        &self.config
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

    /// Replace the option-121 routes offered from now on. A lease already
    /// held keeps the routes it was given until its client renews.
    pub fn set_routes(&mut self, routes: Vec<(Ipv4Net, Ipv4Addr)>) {
        self.config.routes = routes;
    }

    /// The option-121 routes currently offered.
    #[cfg(test)]
    pub fn routes(&self) -> &[(Ipv4Net, Ipv4Addr)] {
        &self.config.routes
    }

    /// The valid lease held by `mac`, if any.
    pub fn lease_of(&self, mac: MacAddr) -> Option<Ipv4Addr> {
        let now = Instant::now();
        self.leases
            .get(&mac)
            .filter(|l| l.expiry > now)
            .map(|l| l.ip)
    }

    /// Whether `ip` is under a valid lease and `mac` does not hold it: the
    /// address then has an owner, and it is not `mac`. The gateway checks a
    /// frame's source against this (review F9). An honest frame costs one
    /// lookup; only a mismatch scans the table.
    pub fn leased_to_another(&self, ip: Ipv4Addr, mac: MacAddr) -> bool {
        if self.lease_of(mac) == Some(ip) {
            return false;
        }
        let now = Instant::now();
        self.leases.values().any(|l| l.ip == ip && l.expiry > now)
    }

    /// The address the lab daemon may take `mac`'s NIC to hold, when its
    /// guest says the NIC carries `claimed` (docs/reviews/containers.md F1).
    /// A guest writes that report, so it decides nothing this server can
    /// decide: a NIC with a valid lease holds the leased address, whatever
    /// its guest says. A NIC with no lease (a static address, or a lease
    /// table lost with the daemon that kept it) is believed, as a frame from
    /// it is, unless `claimed` is this server's own address, or is leased or
    /// reserved to another NIC. `None` is a claim to refuse.
    pub fn vouched(&self, mac: MacAddr, claimed: Ipv4Addr) -> Option<Ipv4Addr> {
        if let Some(leased) = self.lease_of(mac) {
            return Some(leased);
        }
        let reserved_to_another = self
            .config
            .reservations
            .iter()
            .any(|(m, ip)| *ip == claimed && *m != mac);
        if claimed == self.config.gateway
            || reserved_to_another
            || self.leased_to_another(claimed, mac)
        {
            return None;
        }
        Some(claimed)
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
        // The offer holds the address only until the REQUEST is due. A
        // client that already has this address keeps the time it has.
        if self.lease_of(req.chaddr) != Some(ip) {
            self.hold(req.chaddr, ip, OFFER_HOLD);
        }
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

    /// Quarantine the address a client declines, for one lease time. A
    /// client declines only what it holds: a DECLINE with no lease, or one
    /// that names another address, quarantines nothing.
    fn on_decline(&mut self, req: &BootpRequest) {
        let Some(held) = self.lease_of(req.chaddr) else {
            debug!(mac = %req.chaddr, "ignoring DHCP decline from a client with no lease");
            return;
        };
        if req.requested_ip.is_some_and(|ip| ip != held) {
            debug!(
                mac = %req.chaddr,
                %held,
                "ignoring DHCP decline of an address the client does not hold"
            );
            return;
        }
        self.leases.remove(&req.chaddr);
        warn!(mac = %req.chaddr, ip = %held, "DHCP decline; address quarantined");
        let now = Instant::now();
        // A full set gives up the quarantine that ends first (review F6).
        if !self.declined.contains_key(&held) && self.declined.len() >= MAX_DECLINED {
            self.declined.retain(|_, until| *until > now);
            if self.declined.len() >= MAX_DECLINED
                && let Some(first) = self
                    .declined
                    .iter()
                    .min_by_key(|(_, until)| **until)
                    .map(|(ip, _)| *ip)
            {
                self.declined.remove(&first);
            }
        }
        let until = now + Duration::from_secs(u64::from(self.config.lease_secs));
        self.declined.insert(held, until);
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
            .chain(self.declined.keys().copied())
            .collect();
        self.config.subnet.hosts().find(|h| !taken.contains(h))
    }

    /// Whether `ip` is an address of the subnet a client can hold.
    fn in_pool(&self, ip: Ipv4Addr) -> bool {
        self.config.subnet.contains(&ip)
            && ip != self.config.subnet.network()
            && ip != self.config.subnet.broadcast()
            && ip != self.config.gateway
    }

    /// May a client with no reservation and no lease claim `ip`?
    fn requestable(&self, ip: Ipv4Addr) -> bool {
        let now = Instant::now();
        self.in_pool(ip)
            && !self.config.reservations.values().any(|&r| r == ip)
            && !self.declined.contains_key(&ip)
            && !self.leases.values().any(|l| l.ip == ip && l.expiry > now)
    }

    /// Give `mac` a full lease of `ip`, as an ACK does. Visible to the crate
    /// for the tests that need a lease table without a client.
    pub(crate) fn grant(&mut self, mac: MacAddr, ip: Ipv4Addr) {
        self.hold(
            mac,
            ip,
            Duration::from_secs(u64::from(self.config.lease_secs)),
        );
    }

    /// Give `ip` to `mac` for `time`, replacing what it held.
    fn hold(&mut self, mac: MacAddr, ip: Ipv4Addr, time: Duration) {
        let expiry = Instant::now() + time;
        self.leases.insert(mac, Lease { ip, expiry });
    }

    fn purge_expired(&mut self) {
        let now = Instant::now();
        self.leases.retain(|_, l| l.expiry > now);
        self.declined.retain(|_, until| *until > now);
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
            // A domain that does not fit one option is not a DNS name: it
            // is left out (review F19; `new` and `reconfigure` say so).
            if let Some(domain) = cfg.domain.as_deref().filter(|d| domain_fits(d)) {
                opt(&mut b, OPT_DOMAIN, domain.as_bytes());
            }
            if cfg.mtu != 1500 {
                opt(&mut b, OPT_MTU, &cfg.mtu.to_be_bytes());
            }
            if !cfg.routes.is_empty() {
                // RFC 3442: a client that receives option 121 ignores option
                // 3, so the default route rides here too unless a route
                // already claims it.
                let mut routes = cfg.routes.clone();
                if !routes.iter().any(|(net, _)| net.prefix_len() == 0) {
                    routes.push((Ipv4Net::default(), cfg.router));
                }
                classless_route_options(&mut b, &routes);
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

/// The longest value one option carries: its length field is one byte.
const OPT_MAX_LEN: usize = 255;

/// Append a TLV option. A value longer than [`OPT_MAX_LEN`] is written as
/// several options of the same code, which a client joins in order
/// (RFC 3396), so the length byte is always the length of what follows it.
fn opt(out: &mut Vec<u8>, code: u8, val: &[u8]) {
    if val.is_empty() {
        out.extend_from_slice(&[code, 0]);
        return;
    }
    for part in val.chunks(OPT_MAX_LEN) {
        out.push(code);
        // `chunks` yields at most `OPT_MAX_LEN` bytes, which fits the byte.
        out.push(part.len() as u8);
        out.extend_from_slice(part);
    }
}

/// Whether `domain` fits the one option 15 a client reads. A DNS name is at
/// most 253 bytes, so a longer one is no domain a client could use.
fn domain_fits(domain: &str) -> bool {
    domain.len() <= OPT_MAX_LEN
}

/// Warn about a domain that option 15 cannot carry (review F19).
fn warn_on_long_domain(config: &DhcpConfig) {
    if let Some(domain) = config.domain.as_deref().filter(|d| !domain_fits(d)) {
        warn!(
            subnet = %config.subnet,
            len = domain.len(),
            "DHCP domain is longer than {OPT_MAX_LEN} bytes, option 15 is not sent"
        );
    }
}

/// Append option 121 for `routes`. More routes than one option holds go out
/// as several options 121, which a client joins in order (RFC 3396; RFC 3442
/// makes this option one a client must join). Each one ends on a route
/// boundary, so every option is a valid route list by itself too.
fn classless_route_options(out: &mut Vec<u8>, routes: &[(Ipv4Net, Ipv4Addr)]) {
    let mut part: Vec<u8> = Vec::new();
    for route in routes {
        // One route is at most 9 bytes, so it always fits an empty option.
        let encoded = encode_classless_routes(std::slice::from_ref(route));
        if part.len() + encoded.len() > OPT_MAX_LEN {
            opt(out, OPT_CLASSLESS_ROUTES, &part);
            part.clear();
        }
        part.extend_from_slice(&encoded);
    }
    if !part.is_empty() {
        opt(out, OPT_CLASSLESS_ROUTES, &part);
    }
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

    /// A global segment learns its reservations and MTU after its server
    /// started (PRD §9.2): the next exchange serves them, leases kept.
    #[test]
    fn reconfigure_serves_a_new_reservation_and_mtu() {
        let mut s = DhcpServer::new(config());
        let before = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(before.yiaddr, Ipv4Addr::new(10, 213, 1, 2));
        assert!(!before.opts.contains_key(&OPT_MTU));
        let other = parse_reply(&s.handle(&discover(mac(2))).unwrap()).yiaddr;

        let reserved = Ipv4Addr::new(10, 213, 1, 77);
        s.reconfigure(|c| {
            c.reservations.insert(mac(1), reserved);
            c.mtu = 1400;
        });
        let after = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(after.yiaddr, reserved);
        assert_eq!(after.opts[&OPT_MTU], 1400u16.to_be_bytes().to_vec());
        let ack = parse_reply(&s.handle(&request(mac(1), reserved)).unwrap());
        assert_eq!(ack.opts[&OPT_MSG_TYPE], vec![DHCP_ACK]);
        assert_eq!(s.lease_of(mac(2)), Some(other), "other leases survive");
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
    fn a_quarantine_ends_after_one_lease_time() {
        let mut s = DhcpServer::new(config());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        let decline = client_frame(
            mac(1),
            DHCP_DECLINE,
            &[(OPT_REQUESTED_IP, &offer.yiaddr.octets())],
            Ipv4Addr::UNSPECIFIED,
            false,
        );
        s.handle(&decline);
        let left = s.declined[&offer.yiaddr] - Instant::now();
        assert!(left <= Duration::from_secs(3600), "quarantine is {left:?}");
        // Once its time is over the address is offered again.
        *s.declined.get_mut(&offer.yiaddr).unwrap() = Instant::now();
        let next = parse_reply(&s.handle(&discover(mac(2))).unwrap());
        assert_eq!(next.yiaddr, offer.yiaddr);
    }

    /// `frame` as another NIC would send it: the ethernet source replaced,
    /// `chaddr` left as the sender wrote it.
    fn sent_by(mut frame: Vec<u8>, src: MacAddr) -> Vec<u8> {
        frame[6..12].copy_from_slice(&src.0);
        frame
    }

    #[test]
    fn a_decline_without_a_lease_quarantines_nothing() {
        let mut s = DhcpServer::new(config());
        let first = Ipv4Addr::new(10, 213, 1, 2);
        let decline = client_frame(
            mac(9),
            DHCP_DECLINE,
            &[(OPT_REQUESTED_IP, &first.octets())],
            Ipv4Addr::UNSPECIFIED,
            false,
        );
        assert!(s.handle(&decline).is_none());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(offer.yiaddr, first);
    }

    #[test]
    fn a_decline_of_an_address_the_client_does_not_hold_quarantines_nothing() {
        let mut s = DhcpServer::new(config());
        let held = parse_reply(&s.handle(&discover(mac(1))).unwrap()).yiaddr;
        let other = Ipv4Addr::new(10, 213, 1, 3);
        let decline = client_frame(
            mac(1),
            DHCP_DECLINE,
            &[(OPT_REQUESTED_IP, &other.octets())],
            Ipv4Addr::UNSPECIFIED,
            false,
        );
        assert!(s.handle(&decline).is_none());
        assert_eq!(s.lease_of(mac(1)), Some(held));
        let next = parse_reply(&s.handle(&discover(mac(2))).unwrap());
        assert_eq!(next.yiaddr, other);
    }

    #[test]
    fn a_message_that_speaks_for_another_mac_is_ignored() {
        let mut s = DhcpServer::new(config());
        let held = parse_reply(&s.handle(&discover(mac(1))).unwrap()).yiaddr;
        s.handle(&request(mac(1), held)).unwrap();

        // RELEASE, DECLINE and a REQUEST that selects another server, each
        // with the victim's MAC in `chaddr` and the attacker's on the frame.
        let release = client_frame(mac(1), DHCP_RELEASE, &[], held, false);
        assert!(s.handle(&sent_by(release, mac(2))).is_none());
        let decline = client_frame(
            mac(1),
            DHCP_DECLINE,
            &[(OPT_REQUESTED_IP, &held.octets())],
            Ipv4Addr::UNSPECIFIED,
            false,
        );
        assert!(s.handle(&sent_by(decline, mac(2))).is_none());
        let elsewhere = client_frame(
            mac(1),
            DHCP_REQUEST,
            &[
                (OPT_REQUESTED_IP, &held.octets()),
                (OPT_SERVER_ID, &[192, 168, 0, 1]),
            ],
            Ipv4Addr::UNSPECIFIED,
            false,
        );
        assert!(s.handle(&sent_by(elsewhere, mac(2))).is_none());
        assert_eq!(s.leases(), vec![(mac(1), held)]);

        // Nor does a DISCOVER take an address for a MAC that did not send it.
        assert!(s.handle(&sent_by(discover(mac(3)), mac(2))).is_none());
        assert_eq!(s.leases(), vec![(mac(1), held)]);
        // The address is still usable: nothing was quarantined.
        let again = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        assert_eq!(again.yiaddr, held);
    }

    #[test]
    fn an_offer_holds_its_address_briefly_until_the_request() {
        let mut s = DhcpServer::new(config());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        let hold = s.leases[&mac(1)].expiry - Instant::now();
        assert!(
            hold <= Duration::from_secs(60),
            "an unanswered offer holds its address for {hold:?}"
        );
        // The client is still told the full lease time, and gets it on ACK.
        assert_eq!(offer.opts[&OPT_LEASE_TIME], 3600u32.to_be_bytes().to_vec());
        s.handle(&request(mac(1), offer.yiaddr)).unwrap();
        let held = s.leases[&mac(1)].expiry - Instant::now();
        assert!(held > Duration::from_secs(3500), "lease is {held:?}");
    }

    #[test]
    fn a_rediscover_does_not_shorten_a_confirmed_lease() {
        let mut s = DhcpServer::new(config());
        let offer = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        s.handle(&request(mac(1), offer.yiaddr)).unwrap();
        s.handle(&discover(mac(1))).unwrap();
        let held = s.leases[&mac(1)].expiry - Instant::now();
        assert!(held > Duration::from_secs(3500), "lease is {held:?}");
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

    /// Every option of a reply in wire order, walked strictly: each length
    /// has to stay inside the payload, and the walk has to end on the END
    /// option as the payload's last byte.
    fn walk_options(frame: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let eth = EthView::parse(frame).unwrap();
        let ip = Ipv4View::parse(eth.payload()).unwrap();
        let udp = UdpView::parse(ip.payload()).unwrap();
        let p = udp.payload();
        let mut opts = Vec::new();
        let mut i = BOOTP_MIN_LEN;
        loop {
            let code = *p.get(i).expect("the options end without an END option");
            if code == OPT_END {
                assert_eq!(i, p.len() - 1, "bytes follow the END option");
                return opts;
            }
            if code == OPT_PAD {
                i += 1;
                continue;
            }
            let len = usize::from(*p.get(i + 1).expect("an option has no length byte"));
            let val = p
                .get(i + 2..i + 2 + len)
                .expect("an option's length passes the end of the reply");
            opts.push((code, val.to_vec()));
            i += 2 + len;
        }
    }

    /// Review F19: option 121 for more routes than one option holds. The
    /// reply stays a well-formed option list, and the options 121 joined in
    /// order (RFC 3396) are every route with the default route last.
    #[test]
    fn many_routes_make_a_well_formed_reply() {
        let via = Ipv4Addr::new(10, 213, 1, 254);
        let mut cfg = config();
        cfg.routes = (0..40u8)
            .map(|n| {
                (
                    Ipv4Net::new(Ipv4Addr::new(192, 168, 7, n), 32).unwrap(),
                    via,
                )
            })
            .collect();
        let mut expected = cfg.routes.clone();
        expected.push((Ipv4Net::default(), GW));
        let expected = encode_classless_routes(&expected);
        assert!(expected.len() > 255, "the routes have to pass one option");

        let mut s = DhcpServer::new(cfg);
        let reply = s.handle(&discover(mac(1))).expect("offer expected");
        let opts = walk_options(&reply);

        let of = |code: u8| -> Vec<&Vec<u8>> {
            opts.iter()
                .filter(|(c, _)| *c == code)
                .map(|(_, v)| v)
                .collect()
        };
        assert_eq!(of(OPT_MSG_TYPE), vec![&vec![DHCP_OFFER]]);
        // Nothing but the routes is in the reply twice, and no code appears
        // that the server does not serve.
        for (code, _) in &opts {
            assert!(
                [
                    OPT_MSG_TYPE,
                    OPT_SERVER_ID,
                    OPT_LEASE_TIME,
                    OPT_SUBNET_MASK,
                    OPT_ROUTER,
                    OPT_DNS,
                    OPT_DOMAIN,
                    OPT_CLASSLESS_ROUTES,
                ]
                .contains(code),
                "the reply has an option {code} the server does not serve"
            );
            if *code != OPT_CLASSLESS_ROUTES {
                assert_eq!(of(*code).len(), 1, "option {code} is in the reply twice");
            }
        }
        let parts = of(OPT_CLASSLESS_ROUTES);
        assert!(parts.len() > 1);
        // Each option is whole routes: 1 length byte, 4 of a /32, 4 router.
        for part in &parts {
            assert!(!part.is_empty());
            assert_eq!(
                part.len() % 9,
                if part.ends_with(&[0, 10, 213, 1, 1]) {
                    5
                } else {
                    0
                }
            );
        }
        let joined: Vec<u8> = parts.into_iter().flatten().copied().collect();
        assert_eq!(joined, expected);
    }

    /// Review F19: a domain that does not fit option 15 is left out, and the
    /// reply stays a well-formed option list.
    #[test]
    fn a_domain_longer_than_one_option_is_left_out() {
        let mut cfg = config();
        cfg.domain = Some("a".repeat(300));
        let mut s = DhcpServer::new(cfg);
        let reply = s.handle(&discover(mac(1))).expect("offer expected");
        let opts = walk_options(&reply);
        assert!(
            opts.iter()
                .any(|(c, v)| *c == OPT_MSG_TYPE && v == &[DHCP_OFFER])
        );
        assert!(opts.iter().all(|(c, _)| *c != OPT_DOMAIN));
        assert!(opts.iter().any(|(c, _)| *c == OPT_ROUTER));
    }

    /// RFC 3442: a client that receives option 121 ignores option 3, so the
    /// default route has to ride option 121 too or the guest loses it.
    #[test]
    fn option_121_carries_the_default_route() {
        let mut cfg = config();
        cfg.router = Ipv4Addr::new(10, 213, 1, 5);
        cfg.routes = vec![(net("10.0.0.0/8"), Ipv4Addr::new(10, 213, 1, 254))];
        let mut s = DhcpServer::new(cfg);
        let r = parse_reply(&s.handle(&discover(mac(1))).unwrap());
        let expected: Vec<u8> = vec![
            8, 10, 10, 213, 1, 254, // 10/8 via .254
            0, 10, 213, 1, 5, // default via the router
        ];
        assert_eq!(r.opts[&OPT_CLASSLESS_ROUTES], expected);
        assert_eq!(r.opts[&OPT_ROUTER], vec![10, 213, 1, 5]);
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

    /// Review F6: a client that takes and declines 5 000 addresses in turn
    /// quarantines no more than [`MAX_DECLINED`], and a decline of an
    /// address it does not hold takes no slot.
    #[test]
    fn the_declined_set_is_bounded() {
        let decline = |ip: Ipv4Addr| {
            client_frame(
                mac(1),
                DHCP_DECLINE,
                &[(OPT_REQUESTED_IP, &ip.octets())],
                Ipv4Addr::UNSPECIFIED,
                false,
            )
        };
        // A /8, so that the pool is larger than the limit.
        let cfg = DhcpConfig::new(net("10.0.0.0/8"), Ipv4Addr::new(10, 0, 0, 1), GW_MAC);
        let mut s = DhcpServer::new(cfg);
        let mut last = Ipv4Addr::UNSPECIFIED;
        for _ in 0..5_000u32 {
            last = parse_reply(&s.handle(&discover(mac(1))).unwrap()).yiaddr;
            s.handle(&decline(last));
        }
        assert!(
            s.declined.len() <= MAX_DECLINED,
            "{} addresses are quarantined, over the limit of {MAX_DECLINED}",
            s.declined.len()
        );
        assert_eq!(
            s.declined.len(),
            MAX_DECLINED,
            "a decline still quarantines"
        );
        assert!(
            s.declined.contains_key(&last),
            "the newest decline is quarantined"
        );

        // An address the client does not hold takes no slot.
        let mut s = DhcpServer::new(config());
        for i in 0..1_000u32 {
            s.handle(&decline(Ipv4Addr::from(0xC612_0000 + i)));
        }
        assert_eq!(s.declined.len(), 0);
    }

    /// What the lab daemon may take a NIC to hold (containers review F1):
    /// its lease whatever its guest claims, and with no lease the claim,
    /// unless the address is another NIC's or the server's own.
    #[test]
    fn a_claim_never_outranks_a_lease() {
        let mut c = config();
        let reserved = Ipv4Addr::new(10, 213, 1, 50);
        c.reservations.insert(mac(3), reserved);
        let mut s = DhcpServer::new(c);
        let victim = Ipv4Addr::new(10, 213, 1, 10);
        let rogue = Ipv4Addr::new(10, 213, 1, 11);
        s.grant(mac(1), victim);
        s.grant(mac(2), rogue);

        assert_eq!(s.vouched(mac(1), victim), Some(victim));
        // A NIC with a lease holds the lease, whatever is claimed for it.
        assert_eq!(s.vouched(mac(2), victim), Some(rogue));
        assert_eq!(s.vouched(mac(2), GW), Some(rogue));

        // A NIC with no lease: a free address is believed.
        let free = Ipv4Addr::new(10, 213, 1, 200);
        assert_eq!(s.vouched(mac(4), free), Some(free));
        // Another NIC's lease, another NIC's reservation and the server's
        // own address are not.
        assert_eq!(s.vouched(mac(4), victim), None);
        assert_eq!(s.vouched(mac(4), reserved), None);
        assert_eq!(s.vouched(mac(4), GW), None);
        // The reservation's own NIC may claim it before it has the lease.
        assert_eq!(s.vouched(mac(3), reserved), Some(reserved));
    }
}
