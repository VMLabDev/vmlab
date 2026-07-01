//! DNS server state machine for segment gateways (PRD §9.5, §9.9).
//!
//! [`DnsZone`] is the mutable name database: auto-registered guest records,
//! static entries (with `*.`-wildcard support), and sinkhole rules. Every
//! mutating rule insertion returns a `u64` rule id so runtime mutation (PRD
//! §9.9) can remove individual entries again.
//!
//! [`DnsServer`] is sans-I/O: [`DnsServer::handle`] consumes a raw ethernet
//! frame and returns a [`DnsAction`] — either a complete reply frame, a
//! request to forward the query upstream (the gateway performs the socket
//! I/O via [`forward_upstream`] and rebuilds the reply frame from the
//! returned [`DnsRespondCtx`]), or ignore.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::debug;

use crate::config::model::{MacAddr, SinkholeMode};
use crate::net::frame::{
    ETHERTYPE_IPV4, EthView, IPPROTO_UDP, Ipv4View, UdpView, eth_build, ipv4_build, udp_build,
};

/// UDP port the server answers on.
pub const DNS_PORT: u16 = 53;

pub const QTYPE_A: u16 = 1;
pub const QTYPE_AAAA: u16 = 28;
pub const QTYPE_ANY: u16 = 255;
pub const QCLASS_IN: u16 = 1;

pub const RCODE_NOERROR: u8 = 0;
pub const RCODE_NXDOMAIN: u8 = 3;
pub const RCODE_NOTIMP: u8 = 4;

/// TTL on every answer we synthesise; short, because records mutate at
/// runtime.
pub const ANSWER_TTL: u32 = 60;

// ---------------------------------------------------------------------------
// Zone
// ---------------------------------------------------------------------------

/// Result of a zone lookup, in precedence order: sinkhole verdicts first,
/// then exact records, then wildcards, then forward-if-upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsAnswer {
    A(Ipv4Addr),
    Nxdomain,
    /// Sinkholed to 0.0.0.0.
    Zero,
    /// No local answer; an upstream is configured — forward the query.
    Forward,
}

#[derive(Debug, Clone)]
struct WildcardRecord {
    id: u64,
    pattern: String,
    ip: Ipv4Addr,
}

#[derive(Debug, Clone)]
struct SinkholeEntry {
    id: u64,
    pattern: String,
    mode: SinkholeMode,
}

/// One segment's name database. All names are stored and matched lowercase
/// without a trailing dot.
#[derive(Debug, Clone)]
pub struct DnsZone {
    /// Lab DNS suffix (default `vmlab.internal`); [`DnsZone::register`]
    /// appends it to bare names.
    pub suffix: String,
    /// Upstream resolver for unmatched queries; `None` means NXDOMAIN.
    pub upstream: Option<SocketAddr>,
    /// Exact FQDN → address (auto-registrations and exact static entries).
    records: HashMap<String, Ipv4Addr>,
    /// `*.suffix` static entries.
    wildcards: Vec<WildcardRecord>,
    sinkholes: Vec<SinkholeEntry>,
    /// Rule id → exact-record name, for removal of static exact entries.
    rule_names: HashMap<u64, String>,
    next_id: u64,
}

/// Lowercase, no trailing dot.
fn normalize(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Does `pattern` match `name`? A leading `*.` label matches one or more
/// extra labels; anything else is an exact match.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix("*.") {
        name.len() > rest.len() + 1
            && name.ends_with(rest)
            && name.as_bytes()[name.len() - rest.len() - 1] == b'.'
    } else {
        pattern == name
    }
}

/// Match specificity: more labels beat fewer; exact beats wildcard.
fn specificity(pattern: &str) -> (usize, bool) {
    (pattern.split('.').count(), !pattern.starts_with("*."))
}

impl DnsZone {
    pub fn new(suffix: impl Into<String>) -> Self {
        Self {
            suffix: normalize(&suffix.into()),
            upstream: None,
            records: HashMap::new(),
            wildcards: Vec::new(),
            sinkholes: Vec::new(),
            rule_names: HashMap::new(),
            next_id: 0,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Fully qualify a name against the zone suffix (no-op when it already
    /// carries it).
    fn fqdn(&self, name: &str) -> String {
        let n = normalize(name);
        if n == self.suffix || n.ends_with(&format!(".{}", self.suffix)) {
            n
        } else {
            format!("{n}.{}", self.suffix)
        }
    }

    /// Auto-register a guest record (PRD §9.5). Bare names get the zone
    /// suffix appended; replaces any previous registration of the name.
    pub fn register(&mut self, name: &str, ip: Ipv4Addr) {
        let fqdn = self.fqdn(name);
        self.records.insert(fqdn, ip);
    }

    /// Remove an auto-registered record. Returns whether it existed.
    /// ([`Self::register`]'s inverse; nothing unregisters today — records
    /// die with the zone — but the pair keeps the API symmetric.)
    #[allow(dead_code)]
    pub fn unregister(&mut self, name: &str) -> bool {
        let fqdn = self.fqdn(name);
        self.records.remove(&fqdn).is_some()
    }

    /// Add a static entry — exact name or `*.`-wildcard pattern, taken
    /// verbatim (no suffix appending). Returns the rule id for later
    /// removal.
    pub fn set_static(&mut self, pattern: &str, ip: Ipv4Addr) -> u64 {
        let id = self.alloc_id();
        let p = normalize(pattern);
        if p.starts_with("*.") {
            self.wildcards.push(WildcardRecord { id, pattern: p, ip });
        } else {
            self.records.insert(p.clone(), ip);
            self.rule_names.insert(id, p);
        }
        id
    }

    /// Add a sinkhole rule (PRD §9.9). Returns the rule id.
    pub fn add_sinkhole(&mut self, pattern: &str, mode: SinkholeMode) -> u64 {
        let id = self.alloc_id();
        self.sinkholes.push(SinkholeEntry {
            id,
            pattern: normalize(pattern),
            mode,
        });
        id
    }

    /// Remove a rule previously added by [`set_static`] or
    /// [`add_sinkhole`]. Returns whether anything was removed.
    ///
    /// [`set_static`]: DnsZone::set_static
    /// [`add_sinkhole`]: DnsZone::add_sinkhole
    pub fn remove_rule(&mut self, id: u64) -> bool {
        if let Some(name) = self.rule_names.remove(&id) {
            return self.records.remove(&name).is_some();
        }
        let before = self.wildcards.len() + self.sinkholes.len();
        self.wildcards.retain(|w| w.id != id);
        self.sinkholes.retain(|s| s.id != id);
        before != self.wildcards.len() + self.sinkholes.len()
    }

    /// Resolve a query name. Precedence: sinkhole (most-specific pattern
    /// wins, declaration order breaks ties) > exact record > wildcard >
    /// forward-if-upstream-else-NXDOMAIN.
    pub fn lookup(&self, qname: &str) -> DnsAnswer {
        let name = normalize(qname);

        let mut best: Option<(&SinkholeEntry, (usize, bool))> = None;
        for s in &self.sinkholes {
            if pattern_matches(&s.pattern, &name) {
                let k = specificity(&s.pattern);
                if best.is_none_or(|(_, bk)| k > bk) {
                    best = Some((s, k));
                }
            }
        }
        if let Some((s, _)) = best {
            return match s.mode {
                SinkholeMode::Nxdomain => DnsAnswer::Nxdomain,
                SinkholeMode::Zero => DnsAnswer::Zero,
            };
        }

        if let Some(&ip) = self.records.get(&name) {
            return DnsAnswer::A(ip);
        }

        let mut best: Option<(&WildcardRecord, (usize, bool))> = None;
        for w in &self.wildcards {
            if pattern_matches(&w.pattern, &name) {
                let k = specificity(&w.pattern);
                if best.is_none_or(|(_, bk)| k > bk) {
                    best = Some((w, k));
                }
            }
        }
        if let Some((w, _)) = best {
            return DnsAnswer::A(w.ip);
        }

        if self.upstream.is_some() {
            DnsAnswer::Forward
        } else {
            DnsAnswer::Nxdomain
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Everything needed to wrap a DNS payload back into an ethernet frame for
/// the client, captured from the query frame.
#[derive(Debug, Clone, Copy)]
pub struct DnsRespondCtx {
    pub client_mac: MacAddr,
    pub server_mac: MacAddr,
    pub client_ip: Ipv4Addr,
    pub server_ip: Ipv4Addr,
    pub client_port: u16,
}

impl DnsRespondCtx {
    /// Wrap a raw DNS message into the full server→client ethernet frame.
    pub fn build_frame(&self, dns_payload: &[u8]) -> Vec<u8> {
        let udp = udp_build(
            self.server_ip,
            self.client_ip,
            DNS_PORT,
            self.client_port,
            dns_payload,
        );
        let ip = ipv4_build(self.server_ip, self.client_ip, IPPROTO_UDP, 64, &udp, 0);
        eth_build(self.client_mac, self.server_mac, ETHERTYPE_IPV4, &ip)
    }
}

/// Verdict of [`DnsServer::handle`] for one frame.
#[derive(Debug)]
pub enum DnsAction {
    /// Send this complete ethernet frame back out the port.
    Reply(Vec<u8>),
    /// Forward `raw_query` to `upstream`; wrap whatever comes back with
    /// `respond.build_frame(...)`.
    ForwardUpstream {
        upstream: SocketAddr,
        /// The query's DNS transaction id (also embedded in `raw_query`;
        /// production forwards the raw bytes, tests assert on this).
        #[allow(dead_code)]
        query_id: u16,
        raw_query: Vec<u8>,
        respond: DnsRespondCtx,
    },
    /// Not a DNS query for us (or malformed): do nothing.
    Ignore,
}

/// The DNS protocol engine over a shared [`DnsZone`].
pub struct DnsServer {
    zone: Arc<Mutex<DnsZone>>,
}

impl DnsServer {
    /// Own a fresh zone (tests; production wraps the gateway's shared zone).
    #[allow(dead_code)]
    pub fn new(zone: DnsZone) -> Self {
        Self {
            zone: Arc::new(Mutex::new(zone)),
        }
    }

    /// Wrap an already-shared zone (the gateway hands the same `Arc` out for
    /// runtime mutation).
    pub fn shared(zone: Arc<Mutex<DnsZone>>) -> Self {
        Self { zone }
    }

    /// Process one ethernet frame addressed to UDP port 53.
    pub fn handle(&self, eth_frame: &[u8]) -> DnsAction {
        let Some(eth) = EthView::parse(eth_frame) else {
            return DnsAction::Ignore;
        };
        if eth.ethertype() != ETHERTYPE_IPV4 {
            return DnsAction::Ignore;
        }
        let Some(ip) = Ipv4View::parse(eth.payload()) else {
            return DnsAction::Ignore;
        };
        if ip.proto() != IPPROTO_UDP {
            return DnsAction::Ignore;
        }
        let Some(udp) = UdpView::parse(ip.payload()) else {
            return DnsAction::Ignore;
        };
        if udp.dst_port() != DNS_PORT {
            return DnsAction::Ignore;
        }
        let msg = udp.payload();
        if msg.len() < 12 || msg[2] & 0x80 != 0 {
            return DnsAction::Ignore; // truncated, or a response
        }
        let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
        if qdcount != 1 {
            return DnsAction::Ignore;
        }
        let Some((qname, off)) = parse_qname(msg, 12) else {
            return DnsAction::Ignore;
        };
        let Some(rest) = msg.get(off..off + 4) else {
            return DnsAction::Ignore;
        };
        let qtype = u16::from_be_bytes([rest[0], rest[1]]);
        let qclass = u16::from_be_bytes([rest[2], rest[3]]);
        let question_end = off + 4;

        let respond = DnsRespondCtx {
            client_mac: eth.src_mac(),
            server_mac: eth.dst_mac(),
            client_ip: ip.src(),
            server_ip: ip.dst(),
            client_port: udp.src_port(),
        };
        let reply = |payload: Vec<u8>| DnsAction::Reply(respond.build_frame(&payload));
        let nxdomain = || reply(build_response(msg, question_end, RCODE_NXDOMAIN, None));

        let (answer, upstream) = {
            let zone = self.zone.lock().expect("dns zone lock poisoned");
            (zone.lookup(&qname), zone.upstream)
        };

        let opcode = (msg[2] >> 3) & 0x0F;
        if opcode != 0 || qclass != QCLASS_IN {
            return match upstream {
                Some(u) => forward_action(u, msg, respond),
                None => reply(build_response(msg, question_end, RCODE_NOTIMP, None)),
            };
        }

        debug!(name = %qname, qtype, ?answer, "DNS query");
        match qtype {
            QTYPE_A | QTYPE_ANY => match answer {
                DnsAnswer::A(a) => reply(build_response(msg, question_end, RCODE_NOERROR, Some(a))),
                DnsAnswer::Zero => reply(build_response(
                    msg,
                    question_end,
                    RCODE_NOERROR,
                    Some(Ipv4Addr::UNSPECIFIED),
                )),
                DnsAnswer::Nxdomain => nxdomain(),
                DnsAnswer::Forward => match upstream {
                    Some(u) => forward_action(u, msg, respond),
                    None => nxdomain(),
                },
            },
            // Known names have no AAAA records: empty NOERROR so clients
            // fall back to A. Unknown names take the same path as A.
            QTYPE_AAAA => match answer {
                DnsAnswer::A(_) | DnsAnswer::Zero => {
                    reply(build_response(msg, question_end, RCODE_NOERROR, None))
                }
                DnsAnswer::Nxdomain => nxdomain(),
                DnsAnswer::Forward => match upstream {
                    Some(u) => forward_action(u, msg, respond),
                    None => nxdomain(),
                },
            },
            // Anything else we don't serve locally.
            _ => match upstream {
                Some(u) => forward_action(u, msg, respond),
                None => reply(build_response(msg, question_end, RCODE_NOTIMP, None)),
            },
        }
    }
}

fn forward_action(upstream: SocketAddr, msg: &[u8], respond: DnsRespondCtx) -> DnsAction {
    DnsAction::ForwardUpstream {
        upstream,
        query_id: u16::from_be_bytes([msg[0], msg[1]]),
        raw_query: msg.to_vec(),
        respond,
    }
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// Parse an uncompressed question name starting at `off`; returns the
/// lowercased dotted name and the offset just past the terminating zero
/// label. Compression pointers are rejected (queries never need them).
fn parse_qname(msg: &[u8], mut off: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    loop {
        let len = usize::from(*msg.get(off)?);
        if len == 0 {
            off += 1;
            break;
        }
        if len > 63 {
            return None; // compression pointer or malformed length
        }
        let label = msg.get(off + 1..off + 1 + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        for &b in label {
            name.push(char::from(b.to_ascii_lowercase()));
        }
        if name.len() > 255 {
            return None;
        }
        off += 1 + len;
    }
    Some((name, off))
}

/// Encode a dotted name as DNS labels (query-building helper for the DNS
/// and gateway tests; production only parses queries).
#[cfg(test)]
pub fn encode_qname(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    for label in name.trim_end_matches('.').split('.') {
        debug_assert!(label.len() <= 63, "DNS label too long");
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Build a response message echoing the query's id and question. `answer`
/// adds one A record (name compressed as a pointer to offset 12).
fn build_response(
    query: &[u8],
    question_end: usize,
    rcode: u8,
    answer: Option<Ipv4Addr>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(question_end + 16);
    out.extend_from_slice(&query[..2]); // id
    // QR=1, opcode copied, AA=1, TC=0, RD copied.
    out.push(0x80 | (query[2] & 0x78) | 0x04 | (query[2] & 0x01));
    // RA=1, Z=0, RCODE.
    out.push(0x80 | (rcode & 0x0F));
    out.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    out.extend_from_slice(&u16::from(answer.is_some()).to_be_bytes()); // ancount
    out.extend_from_slice(&0u16.to_be_bytes()); // nscount
    out.extend_from_slice(&0u16.to_be_bytes()); // arcount
    out.extend_from_slice(&query[12..question_end]); // question echoed
    if let Some(ip) = answer {
        out.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question name
        out.extend_from_slice(&QTYPE_A.to_be_bytes());
        out.extend_from_slice(&QCLASS_IN.to_be_bytes());
        out.extend_from_slice(&ANSWER_TTL.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&ip.octets());
    }
    out
}

// ---------------------------------------------------------------------------
// Upstream forwarding I/O
// ---------------------------------------------------------------------------

/// Forward a raw DNS query to an upstream resolver over UDP and return the
/// raw response, or `None` on timeout, I/O error, or a mismatched reply id.
pub async fn forward_upstream(
    upstream: SocketAddr,
    raw_query: &[u8],
    timeout: Duration,
) -> Option<Vec<u8>> {
    let bind: SocketAddr = if upstream.is_ipv4() {
        "0.0.0.0:0".parse().expect("static addr")
    } else {
        "[::]:0".parse().expect("static addr")
    };
    let sock = tokio::net::UdpSocket::bind(bind).await.ok()?;
    sock.connect(upstream).await.ok()?;
    sock.send(raw_query).await.ok()?;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(timeout, sock.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    buf.truncate(n);
    if buf.len() < 2 || raw_query.len() < 2 || buf[..2] != raw_query[..2] {
        debug!(%upstream, "upstream DNS reply id mismatch");
        return None;
    }
    Some(buf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 9]);
    const SERVER_MAC: MacAddr = MacAddr([0x52, 0x56, 0, 0, 0, 1]);
    const CLIENT_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 50);
    const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 1);
    const CLIENT_PORT: u16 = 40000;

    fn ip(n: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 213, 1, n)
    }

    fn upstream() -> SocketAddr {
        "192.0.2.1:53".parse().unwrap()
    }

    fn build_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&id.to_be_bytes());
        q.extend_from_slice(&[0x01, 0x00]); // RD set
        q.extend_from_slice(&1u16.to_be_bytes()); // qd
        q.extend_from_slice(&0u16.to_be_bytes()); // an
        q.extend_from_slice(&0u16.to_be_bytes()); // ns
        q.extend_from_slice(&0u16.to_be_bytes()); // ar
        q.extend_from_slice(&encode_qname(name));
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&QCLASS_IN.to_be_bytes());
        q
    }

    fn query_frame(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let q = build_query(id, name, qtype);
        let udp = udp_build(CLIENT_IP, SERVER_IP, CLIENT_PORT, DNS_PORT, &q);
        let ipp = ipv4_build(CLIENT_IP, SERVER_IP, IPPROTO_UDP, 64, &udp, 3);
        eth_build(SERVER_MAC, CLIENT_MAC, ETHERTYPE_IPV4, &ipp)
    }

    /// (id, rcode, ancount, answer = (rtype, ttl, addr)).
    fn parse_reply(frame: &[u8]) -> (u16, u8, u16, Option<(u16, u32, Ipv4Addr)>) {
        let eth = EthView::parse(frame).unwrap();
        assert_eq!(eth.ethertype(), ETHERTYPE_IPV4);
        assert_eq!(eth.dst_mac(), CLIENT_MAC);
        assert_eq!(eth.src_mac(), SERVER_MAC);
        let ipp = Ipv4View::parse(eth.payload()).unwrap();
        assert!(ipp.checksum_valid());
        assert_eq!(ipp.src(), SERVER_IP);
        assert_eq!(ipp.dst(), CLIENT_IP);
        let udp = UdpView::parse(ipp.payload()).unwrap();
        assert!(udp.checksum_valid(ipp.src(), ipp.dst()));
        assert_eq!(udp.src_port(), DNS_PORT);
        assert_eq!(udp.dst_port(), CLIENT_PORT);
        let m = udp.payload();
        let id = u16::from_be_bytes([m[0], m[1]]);
        assert_ne!(m[2] & 0x80, 0, "QR must be set on responses");
        let rcode = m[3] & 0x0F;
        let ancount = u16::from_be_bytes([m[6], m[7]]);
        // Skip the echoed question.
        let mut off = 12;
        while m[off] != 0 {
            off += 1 + usize::from(m[off]);
        }
        off += 1 + 4;
        let answer = (ancount == 1).then(|| {
            assert_eq!(&m[off..off + 2], &[0xC0, 0x0C], "compression pointer");
            let rtype = u16::from_be_bytes([m[off + 2], m[off + 3]]);
            assert_eq!(u16::from_be_bytes([m[off + 4], m[off + 5]]), QCLASS_IN);
            let ttl = u32::from_be_bytes([m[off + 6], m[off + 7], m[off + 8], m[off + 9]]);
            assert_eq!(u16::from_be_bytes([m[off + 10], m[off + 11]]), 4);
            (
                rtype,
                ttl,
                Ipv4Addr::new(m[off + 12], m[off + 13], m[off + 14], m[off + 15]),
            )
        });
        (id, rcode, ancount, answer)
    }

    // -- zone -----------------------------------------------------------------

    #[test]
    fn register_appends_suffix_and_unregister_removes() {
        let mut z = DnsZone::new("vmlab.internal");
        z.register("dc01.ad-lab", ip(10));
        assert_eq!(z.lookup("dc01.ad-lab.vmlab.internal"), DnsAnswer::A(ip(10)));
        // Already-qualified names are not double-suffixed.
        z.register("web.ad-lab.vmlab.internal", ip(11));
        assert_eq!(z.lookup("web.ad-lab.vmlab.internal"), DnsAnswer::A(ip(11)));
        assert!(z.unregister("dc01.ad-lab"));
        assert!(!z.unregister("dc01.ad-lab"));
        assert_eq!(z.lookup("dc01.ad-lab.vmlab.internal"), DnsAnswer::Nxdomain);
    }

    #[test]
    fn lookup_precedence_matrix() {
        let mut z = DnsZone::new("vmlab.internal");
        z.upstream = Some(upstream());

        // Nothing matches: forward (upstream set).
        assert_eq!(z.lookup("unknown.example.com"), DnsAnswer::Forward);

        // Wildcard.
        z.set_static("*.example.com", ip(1));
        assert_eq!(z.lookup("a.example.com"), DnsAnswer::A(ip(1)));
        // The wildcard does not match its own apex.
        assert_eq!(z.lookup("example.com"), DnsAnswer::Forward);
        // ...but matches deeper names.
        assert_eq!(z.lookup("x.y.example.com"), DnsAnswer::A(ip(1)));

        // Exact beats wildcard.
        let exact_id = z.set_static("a.example.com", ip(2));
        assert_eq!(z.lookup("a.example.com"), DnsAnswer::A(ip(2)));
        assert_eq!(z.lookup("b.example.com"), DnsAnswer::A(ip(1)));

        // Sinkhole beats exact.
        let sink_id = z.add_sinkhole("a.example.com", SinkholeMode::Nxdomain);
        assert_eq!(z.lookup("a.example.com"), DnsAnswer::Nxdomain);

        // Removing rules by id restores the lower-precedence answers.
        assert!(z.remove_rule(sink_id));
        assert_eq!(z.lookup("a.example.com"), DnsAnswer::A(ip(2)));
        assert!(z.remove_rule(exact_id));
        assert_eq!(z.lookup("a.example.com"), DnsAnswer::A(ip(1)));
        assert!(!z.remove_rule(9999));
    }

    #[test]
    fn no_upstream_means_nxdomain() {
        let z = DnsZone::new("vmlab.internal");
        assert_eq!(z.lookup("whatever.example.com"), DnsAnswer::Nxdomain);
    }

    #[test]
    fn sinkhole_most_specific_wins_then_declaration_order() {
        let mut z = DnsZone::new("vmlab.internal");
        z.add_sinkhole("*.example.com", SinkholeMode::Zero);
        z.add_sinkhole("*.telemetry.example.com", SinkholeMode::Nxdomain);
        assert_eq!(z.lookup("x.telemetry.example.com"), DnsAnswer::Nxdomain);
        assert_eq!(z.lookup("x.example.com"), DnsAnswer::Zero);
        // Exact sinkhole beats a same-label-count wildcard.
        z.add_sinkhole("x.example.com", SinkholeMode::Nxdomain);
        assert_eq!(z.lookup("x.example.com"), DnsAnswer::Nxdomain);

        // Equal specificity: first declared wins.
        let mut z2 = DnsZone::new("vmlab.internal");
        z2.add_sinkhole("*.tied.com", SinkholeMode::Zero);
        z2.add_sinkhole("*.tied.com", SinkholeMode::Nxdomain);
        assert_eq!(z2.lookup("a.tied.com"), DnsAnswer::Zero);
    }

    #[test]
    fn case_insensitive_everywhere() {
        let mut z = DnsZone::new("VMLab.Internal");
        z.register("DC01.Ad-Lab", ip(10));
        z.add_sinkhole("*.Telemetry.Example.COM", SinkholeMode::Zero);
        assert_eq!(z.lookup("dc01.ad-lab.vmlab.internal"), DnsAnswer::A(ip(10)));
        assert_eq!(
            z.lookup("DC01.AD-LAB.VMLAB.INTERNAL."),
            DnsAnswer::A(ip(10))
        );
        assert_eq!(z.lookup("ping.TELEMETRY.example.com"), DnsAnswer::Zero);
    }

    // -- server / wire format ----------------------------------------------------

    #[test]
    fn hand_built_a_query_round_trip() {
        let server = DnsServer::new({
            let mut z = DnsZone::new("vmlab.internal");
            z.register("dc01.ad-lab", ip(10));
            z
        });
        // Hand-built query for "dc01.ad-lab.vmlab.internal" A IN, id 0x1234.
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.push(4);
        q.extend_from_slice(b"dc01");
        q.push(6);
        q.extend_from_slice(b"ad-lab");
        q.push(5);
        q.extend_from_slice(b"vmlab");
        q.push(8);
        q.extend_from_slice(b"internal");
        q.push(0);
        q.extend_from_slice(&[0, 1, 0, 1]); // A IN
        let udp = udp_build(CLIENT_IP, SERVER_IP, CLIENT_PORT, DNS_PORT, &q);
        let ipp = ipv4_build(CLIENT_IP, SERVER_IP, IPPROTO_UDP, 64, &udp, 3);
        let frame = eth_build(SERVER_MAC, CLIENT_MAC, ETHERTYPE_IPV4, &ipp);

        let DnsAction::Reply(reply) = server.handle(&frame) else {
            panic!("expected a reply");
        };
        let (id, rcode, ancount, answer) = parse_reply(&reply);
        assert_eq!(id, 0x1234);
        assert_eq!(rcode, RCODE_NOERROR);
        assert_eq!(ancount, 1);
        assert_eq!(answer, Some((QTYPE_A, ANSWER_TTL, ip(10))));
    }

    #[test]
    fn nxdomain_reply_has_flags_and_no_answers() {
        let server = DnsServer::new(DnsZone::new("vmlab.internal"));
        let DnsAction::Reply(reply) =
            server.handle(&query_frame(7, "nope.vmlab.internal", QTYPE_A))
        else {
            panic!("expected a reply");
        };
        let (id, rcode, ancount, answer) = parse_reply(&reply);
        assert_eq!(id, 7);
        assert_eq!(rcode, RCODE_NXDOMAIN);
        assert_eq!(ancount, 0);
        assert!(answer.is_none());
    }

    #[test]
    fn zero_sinkhole_answers_0000_nxdomain_sinkhole_refuses() {
        let mut z = DnsZone::new("vmlab.internal");
        z.add_sinkhole("*.ads.example.com", SinkholeMode::Zero);
        z.add_sinkhole("*.telemetry.example.com", SinkholeMode::Nxdomain);
        let server = DnsServer::new(z);

        let DnsAction::Reply(reply) = server.handle(&query_frame(1, "x.ads.example.com", QTYPE_A))
        else {
            panic!("expected a reply");
        };
        let (_, rcode, _, answer) = parse_reply(&reply);
        assert_eq!(rcode, RCODE_NOERROR);
        assert_eq!(answer, Some((QTYPE_A, ANSWER_TTL, Ipv4Addr::UNSPECIFIED)));

        let DnsAction::Reply(reply) =
            server.handle(&query_frame(2, "x.telemetry.example.com", QTYPE_A))
        else {
            panic!("expected a reply");
        };
        let (_, rcode, ancount, _) = parse_reply(&reply);
        assert_eq!(rcode, RCODE_NXDOMAIN);
        assert_eq!(ancount, 0);
    }

    #[test]
    fn aaaa_for_known_name_is_empty_noerror() {
        let mut z = DnsZone::new("vmlab.internal");
        z.register("web", ip(20));
        let server = DnsServer::new(z);
        let DnsAction::Reply(reply) =
            server.handle(&query_frame(3, "web.vmlab.internal", QTYPE_AAAA))
        else {
            panic!("expected a reply");
        };
        let (_, rcode, ancount, _) = parse_reply(&reply);
        assert_eq!(rcode, RCODE_NOERROR);
        assert_eq!(ancount, 0);
        // Unknown AAAA without upstream takes the A path: NXDOMAIN.
        let DnsAction::Reply(reply) =
            server.handle(&query_frame(4, "gone.vmlab.internal", QTYPE_AAAA))
        else {
            panic!("expected a reply");
        };
        let (_, rcode, _, _) = parse_reply(&reply);
        assert_eq!(rcode, RCODE_NXDOMAIN);
    }

    #[test]
    fn any_query_answers_like_a() {
        let mut z = DnsZone::new("vmlab.internal");
        z.register("web", ip(20));
        let server = DnsServer::new(z);
        let DnsAction::Reply(reply) =
            server.handle(&query_frame(5, "web.vmlab.internal", QTYPE_ANY))
        else {
            panic!("expected a reply");
        };
        let (_, rcode, _, answer) = parse_reply(&reply);
        assert_eq!(rcode, RCODE_NOERROR);
        assert_eq!(answer, Some((QTYPE_A, ANSWER_TTL, ip(20))));
    }

    #[test]
    fn other_qtypes_notimp_without_upstream_forward_with() {
        const QTYPE_MX: u16 = 15;
        let server = DnsServer::new(DnsZone::new("vmlab.internal"));
        let DnsAction::Reply(reply) = server.handle(&query_frame(6, "example.com", QTYPE_MX))
        else {
            panic!("expected a reply");
        };
        let (_, rcode, ancount, _) = parse_reply(&reply);
        assert_eq!(rcode, RCODE_NOTIMP);
        assert_eq!(ancount, 0);

        let mut z = DnsZone::new("vmlab.internal");
        z.upstream = Some(upstream());
        let server = DnsServer::new(z);
        let frame = query_frame(0xBEEF, "example.com", QTYPE_MX);
        match server.handle(&frame) {
            DnsAction::ForwardUpstream {
                upstream: u,
                query_id,
                raw_query,
                respond,
            } => {
                assert_eq!(u, upstream());
                assert_eq!(query_id, 0xBEEF);
                assert_eq!(raw_query, build_query(0xBEEF, "example.com", QTYPE_MX));
                assert_eq!(respond.client_mac, CLIENT_MAC);
                assert_eq!(respond.server_mac, SERVER_MAC);
                assert_eq!(respond.client_ip, CLIENT_IP);
                assert_eq!(respond.server_ip, SERVER_IP);
                assert_eq!(respond.client_port, CLIENT_PORT);
            }
            other => panic!("expected ForwardUpstream, got {other:?}"),
        }
        // Unknown A queries also forward when upstream is set.
        assert!(matches!(
            server.handle(&query_frame(8, "unknown.example.com", QTYPE_A)),
            DnsAction::ForwardUpstream { .. }
        ));
    }

    #[test]
    fn respond_ctx_builds_client_frame() {
        let ctx = DnsRespondCtx {
            client_mac: CLIENT_MAC,
            server_mac: SERVER_MAC,
            client_ip: CLIENT_IP,
            server_ip: SERVER_IP,
            client_port: CLIENT_PORT,
        };
        let payload = build_response(
            &build_query(9, "x.example.com", QTYPE_A),
            12 + 15 + 4,
            0,
            None,
        );
        let frame = ctx.build_frame(&payload);
        let (id, rcode, ancount, _) = parse_reply(&frame);
        assert_eq!(id, 9);
        assert_eq!(rcode, RCODE_NOERROR);
        assert_eq!(ancount, 0);
    }

    #[test]
    fn ignores_non_dns_and_malformed() {
        let server = DnsServer::new(DnsZone::new("vmlab.internal"));
        // Wrong UDP port.
        let udp = udp_build(CLIENT_IP, SERVER_IP, 1000, 123, b"x");
        let ipp = ipv4_build(CLIENT_IP, SERVER_IP, IPPROTO_UDP, 64, &udp, 1);
        let f = eth_build(SERVER_MAC, CLIENT_MAC, ETHERTYPE_IPV4, &ipp);
        assert!(matches!(server.handle(&f), DnsAction::Ignore));
        // A DNS *response* (QR set) is ignored.
        let mut q = build_query(1, "a.b", QTYPE_A);
        q[2] |= 0x80;
        let udp = udp_build(CLIENT_IP, SERVER_IP, CLIENT_PORT, DNS_PORT, &q);
        let ipp = ipv4_build(CLIENT_IP, SERVER_IP, IPPROTO_UDP, 64, &udp, 1);
        let f = eth_build(SERVER_MAC, CLIENT_MAC, ETHERTYPE_IPV4, &ipp);
        assert!(matches!(server.handle(&f), DnsAction::Ignore));
        // Truncation sweep never panics.
        let f = query_frame(2, "sweep.example.com", QTYPE_A);
        for n in 0..f.len() {
            let _ = server.handle(&f[..n]);
        }
    }

    // -- upstream forwarding I/O -------------------------------------------------

    #[tokio::test]
    async fn forward_upstream_round_trips() {
        let fake = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = fake.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, peer) = fake.recv_from(&mut buf).await.unwrap();
            let mut resp = buf[..n].to_vec();
            resp[2] |= 0x80; // mark response
            fake.send_to(&resp, peer).await.unwrap();
        });
        let q = build_query(0xABCD, "example.com", QTYPE_A);
        let resp = forward_upstream(addr, &q, Duration::from_secs(2))
            .await
            .expect("response expected");
        assert_eq!(&resp[..2], &q[..2]);
        assert_ne!(resp[2] & 0x80, 0);
    }

    #[tokio::test]
    async fn forward_upstream_times_out() {
        // A bound socket that never answers.
        let mute = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = mute.local_addr().unwrap();
        let q = build_query(1, "example.com", QTYPE_A);
        assert!(
            forward_upstream(addr, &q, Duration::from_millis(100))
                .await
                .is_none()
        );
    }
}
