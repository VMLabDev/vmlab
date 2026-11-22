//! L3 filter / redirect rule engine (PRD §9.9).
//!
//! Pure, synchronous rule evaluation, called from the switch ingress hook /
//! gateway for every guest-originated IPv4 packet. Two rule layers:
//!
//! - **redirect** (DNAT): traffic to `X[:port]` is rewritten to `Y[:port]`.
//!   A NAT-style translation entry is recorded so [`RuleSet::eval_return`]
//!   can rewrite replies from the redirect target back to the original
//!   destination. Entries expire after 5 minutes idle.
//! - **block**: drop, answering with a TCP RST (correct seq/ack so the guest
//!   fails fast) or an ICMP unreachable for UDP/other protocols.
//!
//! Evaluation order (PRD §9.9): redirect rules are consulted before block
//! rules — a packet matching both is redirected, not dropped. Within a layer
//! the most specific match wins:
//!
//! - redirects: `ip:port` beats `ip` (port-less);
//! - blocks: longest prefix first, then a rule with a port beats one
//!   without, then a rule with a protocol beats one without.
//!
//! Remaining ties are broken by insertion (declaration) order.
//!
//! **Fragments.** The rule set keeps no reassembly state, so the port of an
//! IP fragment is not known: a later fragment has no L4 header, and a first
//! one can be cut short of it. A TCP or UDP fragment is therefore dropped,
//! with no reply, when a rule *with a port* covers its destination address
//! and protocol, whatever port the whole datagram has. Otherwise the
//! datagram would pass the rule in pieces and be put together again by its
//! destination. Port-less rules match a fragment as they match any packet.
//! No fragment is tracked or rewritten on the return path.
//!
//! Reply packets synthesized for blocks are sourced from the *blocked
//! destination address* (the host the guest believed it was talking to);
//! the rule set is gateway-agnostic and has no address of its own.
//!
//! Forward-direction translation is purely rule-driven: removing a redirect
//! immediately stops new rewrites ([`RuleSet::remove`] restores `Pass`),
//! while established return-path entries linger until they idle out.

use crate::sync::LockRecover;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::model::{BlockRule, L4Proto, RedirectRule};
use crate::net::frame::{
    self, ICMP_ECHO_REPLY, ICMP_ECHO_REQUEST, IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP, IcmpView,
    Ipv4View, TCP_ACK, TCP_RST, TcpFields, TcpView, UdpView,
};

/// Idle expiry for redirect translation (return-path) entries.
const CONN_IDLE: Duration = Duration::from_secs(300);
/// How often `eval` drops idle entries from the translation table. Not on
/// every packet: the walk is over the whole table (review F7).
const CONN_SWEEP: Duration = Duration::from_secs(1);
/// How many translation entries the table holds (review F7). A guest picks
/// the source address and port an entry is keyed on. Past the limit a new
/// connection replaces the entry idle the longest.
const MAX_CONNS: usize = 4096;
/// How many of [`MAX_CONNS`] one guest address may hold, so that one guest
/// cannot push out the connections of the others. Past it a new connection
/// of that guest replaces its own entry idle the longest.
const MAX_CONNS_PER_GUEST: usize = 1024;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Opaque handle to an installed rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct RuleId(pub u64);

/// Which layer a rule belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    Redirect,
    Block,
}

/// Verdict of [`RuleSet::eval`] / [`RuleSet::eval_return`] for one packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Forward unchanged.
    Pass,
    /// Drop the packet; `reply` (when present) is a full IPv4 packet to send
    /// back toward the source (TCP RST or ICMP unreachable).
    Drop { reply: Option<Vec<u8>> },
    /// Forward the rewritten (DNAT'd / un-DNAT'd) IPv4 packet instead.
    Rewrite(Vec<u8>),
}

/// Display entry for `vmlab net rules`.
#[derive(Debug, Clone, Serialize)]
pub struct RuleEntry {
    pub id: RuleId,
    pub kind: RuleKind,
    /// One-line human description.
    pub description: String,
    /// Block: matched CIDR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    /// Block: matched destination port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Redirect: original destination `ip[:port]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Redirect: rewrite target `ip[:port]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Protocol filter (both kinds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proto: Option<L4Proto>,
}

// ---------------------------------------------------------------------------
// Connection (return-path) tracking
// ---------------------------------------------------------------------------

/// Key of a translation entry, viewed from the *reply* direction:
/// `src` is the redirect target, `dst` the original guest. For ICMP echo,
/// the echo identifier stands in for a port (`sport` = 0, `dport` = id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConnKey {
    src: Ipv4Addr,
    sport: u16,
    dst: Ipv4Addr,
    dport: u16,
    proto: u8,
}

#[derive(Debug, Clone, Copy)]
struct RevVal {
    /// Original destination the guest addressed.
    orig_ip: Ipv4Addr,
    /// Original destination port (None for ICMP).
    orig_port: Option<u16>,
    last: Instant,
}

// ---------------------------------------------------------------------------
// Packet metadata
// ---------------------------------------------------------------------------

struct Meta {
    l4: Option<L4Proto>,
    /// Destination port for TCP/UDP (rule matching).
    dst_port: Option<u16>,
    /// (sport, dport) used for connection tracking. For ICMP echo requests
    /// this is `(id, 0)`, for replies `(0, id)`; `None` for untrackable
    /// protocols/messages.
    conn_ports: Option<(u16, u16)>,
    /// The packet is an IP fragment: its ports are unknown, so both fields
    /// above are `None`.
    fragment: bool,
}

impl Meta {
    fn of(ip: &Ipv4View<'_>) -> Self {
        if ip.more_fragments() || ip.frag_offset() != 0 {
            // Bytes at the L4 offset of a later fragment are payload, and a
            // first fragment can end inside the header. Read neither.
            return Self {
                l4: match ip.proto() {
                    IPPROTO_TCP => Some(L4Proto::Tcp),
                    IPPROTO_UDP => Some(L4Proto::Udp),
                    IPPROTO_ICMP => Some(L4Proto::Icmp),
                    _ => None,
                },
                dst_port: None,
                conn_ports: None,
                fragment: true,
            };
        }
        match ip.proto() {
            IPPROTO_TCP => {
                let p = TcpView::parse(ip.payload());
                Self {
                    l4: Some(L4Proto::Tcp),
                    dst_port: p.map(|t| t.dst_port()),
                    conn_ports: p.map(|t| (t.src_port(), t.dst_port())),
                    fragment: false,
                }
            }
            IPPROTO_UDP => {
                let p = UdpView::parse(ip.payload());
                Self {
                    l4: Some(L4Proto::Udp),
                    dst_port: p.map(|u| u.dst_port()),
                    conn_ports: p.map(|u| (u.src_port(), u.dst_port())),
                    fragment: false,
                }
            }
            IPPROTO_ICMP => {
                let conn_ports = IcmpView::parse(ip.payload()).and_then(|i| {
                    let id = u16::from_be_bytes([i.rest()[0], i.rest()[1]]);
                    match i.icmp_type() {
                        ICMP_ECHO_REQUEST => Some((id, 0)),
                        ICMP_ECHO_REPLY => Some((0, id)),
                        _ => None,
                    }
                });
                Self {
                    l4: Some(L4Proto::Icmp),
                    dst_port: None,
                    conn_ports,
                    fragment: false,
                }
            }
            _ => Self {
                l4: None,
                dst_port: None,
                conn_ports: None,
                fragment: false,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// RuleSet
// ---------------------------------------------------------------------------

/// Ordered set of L3 rules plus the redirect translation table.
///
/// Mutation (`add_*`, `remove`) takes `&mut self`; evaluation takes `&self`
/// (the translation table sits behind an internal mutex so the switch's
/// ingress hook can call `eval` through a shared reference).
#[derive(Default)]
pub struct RuleSet {
    next_id: u64,
    redirects: Vec<(RuleId, RedirectRule)>,
    blocks: Vec<(RuleId, BlockRule)>,
    conns: Mutex<Conns>,
}

/// The redirect translation table: reply-direction key to what the reply's
/// source is rewritten back to. In a key, `dst` is the guest.
#[derive(Default)]
struct Conns {
    table: HashMap<ConnKey, RevVal>,
    /// When idle entries were last dropped.
    swept: Option<Instant>,
}

impl Conns {
    /// Drop idle entries, at most once per [`CONN_SWEEP`].
    fn sweep(&mut self, now: Instant) {
        if self
            .swept
            .is_some_and(|at| now.duration_since(at) < CONN_SWEEP)
        {
            return;
        }
        self.swept = Some(now);
        self.table
            .retain(|_, v| now.duration_since(v.last) < CONN_IDLE);
    }

    /// Track a connection. A new one at the guest's limit replaces that
    /// guest's entry idle the longest, and in a full table the entry idle
    /// the longest of all. The walk this takes is over at most
    /// [`MAX_CONNS`] entries, and only a new connection pays it.
    fn track(&mut self, key: ConnKey, val: RevVal) {
        if let Some(held) = self.table.get_mut(&key) {
            *held = val;
            return;
        }
        let of_guest = self.table.keys().filter(|k| k.dst == key.dst).count();
        let only = if of_guest >= MAX_CONNS_PER_GUEST {
            Some(key.dst)
        } else if self.table.len() >= MAX_CONNS {
            None
        } else {
            self.table.insert(key, val);
            return;
        };
        let oldest = self
            .table
            .iter()
            .filter(|(k, _)| only.is_none_or(|g| k.dst == g))
            .min_by_key(|(_, v)| v.last)
            .map(|(k, _)| *k);
        if let Some(oldest) = oldest {
            self.table.remove(&oldest);
        }
        self.table.insert(key, val);
    }
}

impl RuleSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc_id(&mut self) -> RuleId {
        self.next_id += 1;
        RuleId(self.next_id)
    }

    pub fn add_block(&mut self, rule: BlockRule) -> RuleId {
        let id = self.alloc_id();
        self.blocks.push((id, rule));
        id
    }

    pub fn add_redirect(&mut self, rule: RedirectRule) -> RuleId {
        let id = self.alloc_id();
        self.redirects.push((id, rule));
        id
    }

    /// Remove a rule by id. Returns whether anything was removed. Existing
    /// return-path translation entries are left to idle out so in-flight
    /// connections keep receiving rewritten replies.
    pub fn remove(&mut self, id: RuleId) -> bool {
        let before = self.redirects.len() + self.blocks.len();
        self.redirects.retain(|(rid, _)| *rid != id);
        self.blocks.retain(|(rid, _)| *rid != id);
        self.redirects.len() + self.blocks.len() != before
    }

    /// All rules in evaluation order (redirects first, then blocks; each in
    /// insertion order).
    pub fn list(&self) -> Vec<RuleEntry> {
        let mut out = Vec::with_capacity(self.redirects.len() + self.blocks.len());
        for (id, r) in &self.redirects {
            let from = host_port_str(r.from.ip, r.from.port);
            let to = host_port_str(r.to.ip, r.to.port);
            let proto_s = r
                .proto
                .map(|p| format!(" {}", proto_name(p)))
                .unwrap_or_default();
            out.push(RuleEntry {
                id: *id,
                kind: RuleKind::Redirect,
                description: format!("redirect {from} -> {to}{proto_s}"),
                cidr: None,
                port: None,
                from: Some(from),
                to: Some(to),
                proto: r.proto,
            });
        }
        for (id, b) in &self.blocks {
            let proto_s = b
                .proto
                .map(|p| format!(" {}", proto_name(p)))
                .unwrap_or_default();
            let port_s = b.port.map(|p| format!(" port {p}")).unwrap_or_default();
            out.push(RuleEntry {
                id: *id,
                kind: RuleKind::Block,
                description: format!("block {}{proto_s}{port_s}", b.cidr),
                cidr: Some(b.cidr.to_string()),
                port: b.port,
                from: None,
                to: None,
                proto: b.proto,
            });
        }
        out
    }

    /// Evaluate a guest-originated IPv4 packet. Non-IPv4 / unparseable input
    /// passes (it is not this layer's job to police framing).
    pub fn eval(&self, ipv4_packet: &[u8]) -> Verdict {
        let Some(ip) = Ipv4View::parse(ipv4_packet) else {
            return Verdict::Pass;
        };
        self.conns.lock_recover().sweep(Instant::now());
        let meta = Meta::of(&ip);

        // A fragment's port is unknown: one that a port rule could cover is
        // dropped rather than let past the rule (see the module docs). No
        // reply: an error is never sent for a later fragment (RFC 1122).
        if meta.fragment && self.port_rule_covers(&ip, &meta) {
            return Verdict::Drop { reply: None };
        }

        // Layer 1: redirects.
        if let Some(rule) = self.best_redirect(&ip, &meta) {
            let new_ip = rule.to.ip;
            let new_port = meta.dst_port.map(|orig| rule.to.port.unwrap_or(orig));
            // Record the return-path translation entry (reply-direction key).
            if let Some((sport, dport)) = meta.conn_ports {
                let (rev_sport, rev_dport) = match ip.proto() {
                    // TCP/UDP replies come from the rewritten dst port.
                    IPPROTO_TCP | IPPROTO_UDP => (new_port.unwrap_or(dport), sport),
                    // ICMP echo reply carries the request's id as dport.
                    _ => (0, sport.max(dport)),
                };
                let key = ConnKey {
                    src: new_ip,
                    sport: rev_sport,
                    dst: ip.src(),
                    dport: rev_dport,
                    proto: ip.proto(),
                };
                let val = RevVal {
                    orig_ip: ip.dst(),
                    orig_port: meta.dst_port,
                    last: Instant::now(),
                };
                self.conns.lock_recover().track(key, val);
            }
            if let Some(out) = rewrite_packet(ipv4_packet, &ip, None, Some((new_ip, new_port))) {
                return Verdict::Rewrite(out);
            }
            return Verdict::Pass;
        }

        // Layer 2: blocks.
        if self.best_block(&ip, &meta).is_some() {
            let total = usize::from(ip.total_len());
            let reply = block_reply(&ipv4_packet[..total], &ip);
            return Verdict::Drop { reply };
        }

        Verdict::Pass
    }

    /// Evaluate a packet *sourced from a redirect target*: if it matches a
    /// live translation entry, rewrite its source back to the original
    /// destination the guest addressed. Otherwise pass.
    pub fn eval_return(&self, ipv4_packet: &[u8]) -> Verdict {
        let Some(ip) = Ipv4View::parse(ipv4_packet) else {
            return Verdict::Pass;
        };
        let meta = Meta::of(&ip);
        let Some((sport, dport)) = meta.conn_ports else {
            return Verdict::Pass;
        };
        let key = ConnKey {
            src: ip.src(),
            sport,
            dst: ip.dst(),
            dport,
            proto: ip.proto(),
        };
        let rev = {
            let mut conns = self.conns.lock_recover();
            match conns.table.get_mut(&key) {
                Some(v) if v.last.elapsed() < CONN_IDLE => {
                    v.last = Instant::now();
                    Some(*v)
                }
                _ => None,
            }
        };
        match rev
            .and_then(|v| rewrite_packet(ipv4_packet, &ip, Some((v.orig_ip, v.orig_port)), None))
        {
            Some(out) => Verdict::Rewrite(out),
            None => Verdict::Pass,
        }
    }

    /// Whether some redirect rewrites traffic of `proto` onto `ip`, at
    /// `port` when the rule names one: the question the NAT engine's guard
    /// asks before it refuses a flow to the host's loopback, since a
    /// declared `redirect` onto it is the operator's own grant (§9.7).
    /// `port` is `None` for a protocol without ports.
    pub fn redirects_onto(&self, ip: Ipv4Addr, proto: L4Proto, port: Option<u16>) -> bool {
        self.redirects.iter().any(|(_, r)| {
            r.to.ip == ip
                && r.proto.is_none_or(|p| p == proto)
                // A port-less rule keeps the port the guest dialled, so it
                // reaches every port of its target.
                && r.to.port.or(r.from.port).is_none_or(|p| port == Some(p))
        })
    }

    /// Number of live translation entries (diagnostics; asserted by tests).
    #[allow(dead_code)]
    pub fn conn_count(&self) -> usize {
        self.conns.lock_recover().table.len()
    }

    /// Expiry with an explicit idle threshold, so tests can drain the table
    /// without waiting out the real `CONN_IDLE`.
    #[cfg(test)]
    fn expire_conns_older_than(&self, idle: Duration) {
        let mut conns = self.conns.lock_recover();
        conns.table.retain(|_, v| v.last.elapsed() < idle);
    }

    /// Whether a rule with a port covers the destination address and
    /// protocol of this packet, leaving the port aside. Only TCP and UDP
    /// have ports, so no port rule covers another protocol.
    fn port_rule_covers(&self, ip: &Ipv4View<'_>, meta: &Meta) -> bool {
        if !matches!(meta.l4, Some(L4Proto::Tcp | L4Proto::Udp)) {
            return false;
        }
        let proto_ok = |p: Option<L4Proto>| p.is_none_or(|p| meta.l4 == Some(p));
        self.redirects
            .iter()
            .any(|(_, r)| r.from.port.is_some() && r.from.ip == ip.dst() && proto_ok(r.proto))
            || self
                .blocks
                .iter()
                .any(|(_, b)| b.port.is_some() && b.cidr.contains(&ip.dst()) && proto_ok(b.proto))
    }

    /// Most specific matching redirect: `ip:port` beats `ip`; ties go to the
    /// earliest-inserted rule.
    fn best_redirect(&self, ip: &Ipv4View<'_>, meta: &Meta) -> Option<&RedirectRule> {
        let mut best: Option<(&RedirectRule, bool)> = None;
        for (_, r) in &self.redirects {
            if r.from.ip != ip.dst() {
                continue;
            }
            if let Some(p) = r.proto
                && meta.l4 != Some(p)
            {
                continue;
            }
            if let Some(port) = r.from.port
                && meta.dst_port != Some(port)
            {
                continue;
            }
            let spec = r.from.port.is_some();
            // Strictly-greater specificity replaces; equal keeps the earlier.
            if best.is_none_or(|(_, b)| spec && !b) {
                best = Some((r, spec));
            }
        }
        best.map(|(r, _)| r)
    }

    /// Most specific matching block: longest prefix, then with-port beats
    /// without, then with-proto beats without; ties to insertion order.
    fn best_block(&self, ip: &Ipv4View<'_>, meta: &Meta) -> Option<&BlockRule> {
        let mut best: Option<(&BlockRule, (u8, bool, bool))> = None;
        for (_, b) in &self.blocks {
            if !b.cidr.contains(&ip.dst()) {
                continue;
            }
            if let Some(p) = b.proto
                && meta.l4 != Some(p)
            {
                continue;
            }
            if let Some(port) = b.port
                && meta.dst_port != Some(port)
            {
                continue;
            }
            let spec = (b.cidr.prefix_len(), b.port.is_some(), b.proto.is_some());
            if best.is_none_or(|(_, s)| spec > s) {
                best = Some((b, spec));
            }
        }
        best.map(|(b, _)| b)
    }
}

fn proto_name(p: L4Proto) -> &'static str {
    match p {
        L4Proto::Tcp => "tcp",
        L4Proto::Udp => "udp",
        L4Proto::Icmp => "icmp",
    }
}

fn host_port_str(ip: Ipv4Addr, port: Option<u16>) -> String {
    match port {
        Some(p) => format!("{ip}:{p}"),
        None => ip.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Packet rewriting
// ---------------------------------------------------------------------------

/// Rewrite source and/or destination (ip, optional port) of an IPv4 packet,
/// recomputing the IP header checksum and the TCP/UDP checksum (over the new
/// pseudo-header). A UDP packet that opted out of checksums (field 0) stays
/// opted out. ICMP checksums do not cover the IP addresses, so they are left
/// untouched. The result is clipped to `total_len` (ethernet padding shed).
fn rewrite_packet(
    pkt: &[u8],
    ip: &Ipv4View<'_>,
    new_src: Option<(Ipv4Addr, Option<u16>)>,
    new_dst: Option<(Ipv4Addr, Option<u16>)>,
) -> Option<Vec<u8>> {
    let total = usize::from(ip.total_len());
    let hl = ip.header_len();
    let mut out = pkt[..total].to_vec();

    if let Some((a, _)) = new_src {
        out[12..16].copy_from_slice(&a.octets());
    }
    if let Some((a, _)) = new_dst {
        out[16..20].copy_from_slice(&a.octets());
    }
    let sp = new_src.and_then(|(_, p)| p);
    let dp = new_dst.and_then(|(_, p)| p);
    if matches!(ip.proto(), IPPROTO_TCP | IPPROTO_UDP) && out.len() >= hl + 4 {
        if let Some(p) = sp {
            out[hl..hl + 2].copy_from_slice(&p.to_be_bytes());
        }
        if let Some(p) = dp {
            out[hl + 2..hl + 4].copy_from_slice(&p.to_be_bytes());
        }
    }

    let src = Ipv4Addr::new(out[12], out[13], out[14], out[15]);
    let dst = Ipv4Addr::new(out[16], out[17], out[18], out[19]);
    match ip.proto() {
        IPPROTO_TCP if out.len() >= hl + 18 => {
            out[hl + 16..hl + 18].copy_from_slice(&[0, 0]);
            let c = frame::l4_checksum(src, dst, IPPROTO_TCP, &out[hl..])?;
            out[hl + 16..hl + 18].copy_from_slice(&c.to_be_bytes());
        }
        IPPROTO_UDP if out.len() >= hl + 8 => {
            let had_csum = out[hl + 6] != 0 || out[hl + 7] != 0;
            if had_csum {
                out[hl + 6..hl + 8].copy_from_slice(&[0, 0]);
                let c = match frame::l4_checksum(src, dst, IPPROTO_UDP, &out[hl..])? {
                    0 => 0xFFFF,
                    c => c,
                };
                out[hl + 6..hl + 8].copy_from_slice(&c.to_be_bytes());
            }
        }
        _ => {}
    }

    out[10..12].copy_from_slice(&[0, 0]);
    let c = frame::internet_checksum(&out[..hl]);
    out[10..12].copy_from_slice(&c.to_be_bytes());
    Some(out)
}

// ---------------------------------------------------------------------------
// Block replies
// ---------------------------------------------------------------------------

/// Synthesize the fail-fast reply for a blocked packet: TCP gets an RST with
/// RFC 793 reset sequence rules, UDP an ICMP port-unreachable, everything
/// else an ICMP host-unreachable. Returns `None` for packets that must not
/// be answered (e.g. an incoming RST, or unparseable L4).
fn block_reply(pkt: &[u8], ip: &Ipv4View<'_>) -> Option<Vec<u8>> {
    match ip.proto() {
        IPPROTO_TCP => {
            let tcp = TcpView::parse(ip.payload())?;
            if tcp.is_rst() {
                return None;
            }
            let (seq, ack, flags) = if tcp.is_ack() {
                (tcp.ack(), 0, TCP_RST)
            } else {
                let advance =
                    tcp.payload().len() as u32 + u32::from(tcp.is_syn()) + u32::from(tcp.is_fin());
                (0, tcp.seq().wrapping_add(advance), TCP_RST | TCP_ACK)
            };
            let seg = frame::tcp_build(
                ip.dst(),
                ip.src(),
                TcpFields {
                    src_port: tcp.dst_port(),
                    dst_port: tcp.src_port(),
                    seq,
                    ack,
                    flags,
                    window: 0,
                    options: &[],
                },
                &[],
            )?;
            frame::ipv4_build(ip.dst(), ip.src(), IPPROTO_TCP, 64, &seg, 0)
        }
        IPPROTO_UDP => {
            let icmp = frame::icmp_unreachable_for(pkt, 3); // port unreachable
            frame::ipv4_build(ip.dst(), ip.src(), IPPROTO_ICMP, 64, &icmp, 0)
        }
        _ => {
            let icmp = frame::icmp_unreachable_for(pkt, 1); // host unreachable
            frame::ipv4_build(ip.dst(), ip.src(), IPPROTO_ICMP, 64, &icmp, 0)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::frame::{ICMP_DEST_UNREACHABLE, icmp_build, ipv4_build, tcp_build, udp_build};
    use ipnet::Ipv4Net;

    const GUEST: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 10);
    const DST_A: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const DST_B: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const TARGET: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 99);

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn block(cidr: &str, proto: Option<L4Proto>, port: Option<u16>) -> BlockRule {
        BlockRule {
            cidr: net(cidr),
            proto,
            port,
            span: (0, 0),
        }
    }

    fn redirect(
        from: Ipv4Addr,
        from_port: Option<u16>,
        to: Ipv4Addr,
        to_port: Option<u16>,
        proto: Option<L4Proto>,
    ) -> RedirectRule {
        use crate::config::model::HostPort;
        RedirectRule {
            from: HostPort {
                ip: from,
                port: from_port,
            },
            to: HostPort {
                ip: to,
                port: to_port,
            },
            proto,
            span: (0, 0),
        }
    }

    fn tcp_pkt(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, flags: u8) -> Vec<u8> {
        let seg = tcp_build(
            src,
            dst,
            TcpFields {
                src_port: sport,
                dst_port: dport,
                seq: 1000,
                ack: 0,
                flags,
                window: 65535,
                options: &[],
            },
            b"",
        )
        .unwrap();
        ipv4_build(src, dst, IPPROTO_TCP, 64, &seg, 42).unwrap()
    }

    fn udp_pkt(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, body: &[u8]) -> Vec<u8> {
        let seg = udp_build(src, dst, sport, dport, body).unwrap();
        ipv4_build(src, dst, IPPROTO_UDP, 64, &seg, 43).unwrap()
    }

    fn icmp_echo_pkt(src: Ipv4Addr, dst: Ipv4Addr, id: u16, reply: bool) -> Vec<u8> {
        let t = if reply {
            ICMP_ECHO_REPLY
        } else {
            ICMP_ECHO_REQUEST
        };
        let m = icmp_build(t, 0, [(id >> 8) as u8, id as u8, 0, 1], b"pingpayload");
        ipv4_build(src, dst, IPPROTO_ICMP, 64, &m, 44).unwrap()
    }

    fn assert_checksums_ok(pkt: &[u8]) {
        let ip = Ipv4View::parse(pkt).expect("rewritten packet parses");
        assert!(ip.checksum_valid(), "ip checksum");
        match ip.proto() {
            IPPROTO_TCP => {
                let t = TcpView::parse(ip.payload()).unwrap();
                assert!(t.checksum_valid(ip.src(), ip.dst()), "tcp checksum");
            }
            IPPROTO_UDP => {
                let u = UdpView::parse(ip.payload()).unwrap();
                assert!(u.checksum_valid(ip.src(), ip.dst()), "udp checksum");
            }
            IPPROTO_ICMP => {
                let i = IcmpView::parse(ip.payload()).unwrap();
                assert!(i.checksum_valid(), "icmp checksum");
            }
            _ => {}
        }
    }

    // --- pass-through & basics ---

    #[test]
    fn empty_ruleset_passes() {
        let rs = RuleSet::new();
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        assert_eq!(rs.eval(&p), Verdict::Pass);
        assert_eq!(rs.eval_return(&p), Verdict::Pass);
        assert_eq!(rs.eval(b"garbage"), Verdict::Pass);
    }

    #[test]
    fn remove_restores_pass() {
        let mut rs = RuleSet::new();
        let id = rs.add_block(block("192.0.2.0/24", None, None));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        assert!(matches!(rs.eval(&p), Verdict::Drop { .. }));
        assert!(rs.remove(id));
        assert_eq!(rs.eval(&p), Verdict::Pass);
        assert!(!rs.remove(id), "double remove is false");

        let rid = rs.add_redirect(redirect(DST_A, None, TARGET, None, None));
        assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
        assert!(rs.remove(rid));
        assert_eq!(rs.eval(&p), Verdict::Pass);
    }

    #[test]
    fn list_describes_rules() {
        let mut rs = RuleSet::new();
        let b = rs.add_block(block("10.0.0.0/8", Some(L4Proto::Tcp), Some(445)));
        let r = rs.add_redirect(redirect(
            DST_A,
            Some(80),
            TARGET,
            Some(8080),
            Some(L4Proto::Tcp),
        ));
        let entries = rs.list();
        assert_eq!(entries.len(), 2);
        // Redirects listed first (evaluation order).
        assert_eq!(entries[0].id, r);
        assert_eq!(entries[0].kind, RuleKind::Redirect);
        assert_eq!(entries[0].from.as_deref(), Some("192.0.2.1:80"));
        assert_eq!(entries[0].to.as_deref(), Some("10.213.0.99:8080"));
        assert_eq!(
            entries[0].description,
            "redirect 192.0.2.1:80 -> 10.213.0.99:8080 tcp"
        );
        assert_eq!(entries[1].id, b);
        assert_eq!(entries[1].kind, RuleKind::Block);
        assert_eq!(entries[1].cidr.as_deref(), Some("10.0.0.0/8"));
        assert_eq!(entries[1].port, Some(445));
        assert_eq!(entries[1].description, "block 10.0.0.0/8 tcp port 445");
        // Serializable for `vmlab net rules`.
        serde_json::to_string(&entries).unwrap();
    }

    // --- precedence matrix ---

    #[test]
    fn redirect_beats_block() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.1/32", None, None));
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        match rs.eval(&p) {
            Verdict::Rewrite(out) => {
                assert_eq!(Ipv4View::parse(&out).unwrap().dst(), TARGET);
            }
            v => panic!("expected rewrite, got {v:?}"),
        }
    }

    #[test]
    fn redirect_ip_port_beats_ip_only_regardless_of_order() {
        for flip in [false, true] {
            let mut rs = RuleSet::new();
            let general = redirect(DST_A, None, TARGET, None, None);
            let specific = redirect(DST_A, Some(80), DST_B, Some(8080), None);
            if flip {
                rs.add_redirect(general.clone());
                rs.add_redirect(specific.clone());
            } else {
                rs.add_redirect(specific);
                rs.add_redirect(general);
            }
            let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
            match rs.eval(&p) {
                Verdict::Rewrite(out) => {
                    let ip = Ipv4View::parse(&out).unwrap();
                    assert_eq!(ip.dst(), DST_B, "flip={flip}");
                    let t = TcpView::parse(ip.payload()).unwrap();
                    assert_eq!(t.dst_port(), 8080);
                }
                v => panic!("expected rewrite, got {v:?}"),
            }
            // Port 443 only matches the ip-only rule.
            let p = tcp_pkt(GUEST, DST_A, 5000, 443, frame::TCP_SYN);
            match rs.eval(&p) {
                Verdict::Rewrite(out) => {
                    let ip = Ipv4View::parse(&out).unwrap();
                    assert_eq!(ip.dst(), TARGET);
                    // Port preserved when `to` has no port.
                    assert_eq!(TcpView::parse(ip.payload()).unwrap().dst_port(), 443);
                }
                v => panic!("expected rewrite, got {v:?}"),
            }
        }
    }

    /// What a redirect is asked for the NAT guard: its target address, in
    /// its protocol, at the port a rewritten packet arrives on.
    #[test]
    fn redirects_onto_names_exactly_what_a_rule_rewrites_to() {
        let lo = Ipv4Addr::LOCALHOST;
        let gw = Ipv4Addr::new(10, 0, 0, 1);
        let mut rs = RuleSet::new();
        assert!(!rs.redirects_onto(lo, L4Proto::Tcp, Some(8080)));

        // from gw:445 to lo:44445, any protocol.
        let id = rs.add_redirect(redirect(gw, Some(445), lo, Some(44445), None));
        assert!(rs.redirects_onto(lo, L4Proto::Tcp, Some(44445)));
        assert!(rs.redirects_onto(lo, L4Proto::Udp, Some(44445)));
        assert!(!rs.redirects_onto(lo, L4Proto::Tcp, Some(445)));
        assert!(!rs.redirects_onto(lo, L4Proto::Icmp, None));
        assert!(!rs.redirects_onto(Ipv4Addr::new(127, 0, 0, 2), L4Proto::Tcp, Some(44445)));
        assert!(rs.remove(id));
        assert!(!rs.redirects_onto(lo, L4Proto::Tcp, Some(44445)));

        // from gw:53 to lo, udp: the port the guest dialled is kept.
        rs.add_redirect(redirect(gw, Some(53), lo, None, Some(L4Proto::Udp)));
        assert!(rs.redirects_onto(lo, L4Proto::Udp, Some(53)));
        assert!(!rs.redirects_onto(lo, L4Proto::Udp, Some(54)));
        assert!(!rs.redirects_onto(lo, L4Proto::Tcp, Some(53)));

        // A port-less rule reaches every port of its target, and ICMP.
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(gw, None, lo, None, None));
        assert!(rs.redirects_onto(lo, L4Proto::Tcp, Some(1)));
        assert!(rs.redirects_onto(lo, L4Proto::Icmp, None));
    }

    #[test]
    fn redirect_tie_broken_by_insertion_order() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, Some(80), TARGET, Some(1111), None));
        rs.add_redirect(redirect(DST_A, Some(80), TARGET, Some(2222), None));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        match rs.eval(&p) {
            Verdict::Rewrite(out) => {
                let ip = Ipv4View::parse(&out).unwrap();
                assert_eq!(TcpView::parse(ip.payload()).unwrap().dst_port(), 1111);
            }
            v => panic!("expected rewrite, got {v:?}"),
        }
    }

    #[test]
    fn block_longest_prefix_wins() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", Some(L4Proto::Udp), None));
        rs.add_block(block("192.0.2.1/32", Some(L4Proto::Tcp), None));
        // TCP to .1 → matches only the /32 (the /24 is udp-only).
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        assert!(matches!(rs.eval(&p), Verdict::Drop { .. }));
        // TCP to .2 → no rule matches (the /24 is udp-only).
        let p = tcp_pkt(GUEST, DST_B, 5000, 80, frame::TCP_SYN);
        assert_eq!(rs.eval(&p), Verdict::Pass);
    }

    #[test]
    fn block_specificity_port_beats_proto_beats_bare() {
        // All three match; longest prefix is equal so with-port wins.
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", None, None));
        rs.add_block(block("192.0.2.0/24", Some(L4Proto::Tcp), None));
        rs.add_block(block("192.0.2.0/24", None, Some(80)));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        assert!(matches!(rs.eval(&p), Verdict::Drop { .. }));

        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", Some(L4Proto::Tcp), Some(80)));
        rs.add_block(block("192.0.2.1/32", None, None));
        // UDP to .1: only the /32 bare rule can match — longest prefix path.
        let p = udp_pkt(GUEST, DST_A, 5000, 53, b"q");
        assert!(matches!(rs.eval(&p), Verdict::Drop { .. }));
    }

    #[test]
    fn block_port_and_proto_filters() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", Some(L4Proto::Tcp), Some(445)));
        // Wrong port passes.
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        assert_eq!(rs.eval(&p), Verdict::Pass);
        // Wrong proto passes.
        let p = udp_pkt(GUEST, DST_A, 5000, 445, b"x");
        assert_eq!(rs.eval(&p), Verdict::Pass);
        // Exact match drops.
        let p = tcp_pkt(GUEST, DST_A, 5000, 445, frame::TCP_SYN);
        assert!(matches!(rs.eval(&p), Verdict::Drop { .. }));
        // A port-bearing rule can never match a portless protocol.
        let p = icmp_echo_pkt(GUEST, DST_A, 7, false);
        assert_eq!(rs.eval(&p), Verdict::Pass);
    }

    // --- fragments ---

    /// Split a full IPv4 packet (20-byte header) into fragments of at most
    /// `chunk` payload bytes each (a multiple of 8), as a sender would.
    fn fragments(pkt: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        assert_eq!(chunk % 8, 0);
        let ip = Ipv4View::parse(pkt).unwrap();
        let payload = ip.payload();
        let count = payload.len().div_ceil(chunk);
        payload
            .chunks(chunk)
            .enumerate()
            .map(|(i, part)| {
                let mut f = ipv4_build(ip.src(), ip.dst(), ip.proto(), 64, part, ip.id()).unwrap();
                let more = if i + 1 < count { 0x2000 } else { 0 };
                let field = more | (i * chunk / 8) as u16;
                f[6..8].copy_from_slice(&field.to_be_bytes());
                f[10..12].copy_from_slice(&[0, 0]);
                let c = frame::internet_checksum(&f[..20]);
                f[10..12].copy_from_slice(&c.to_be_bytes());
                f
            })
            .collect()
    }

    #[test]
    fn a_fragment_does_not_evade_a_port_block() {
        let mut rs = RuleSet::new();
        rs.add_block(block("10.0.1.0/24", Some(L4Proto::Udp), Some(53)));
        let dst = Ipv4Addr::new(10, 0, 1, 5);
        let whole = udp_pkt(GUEST, dst, 5000, 53, &[0xAB; 2000]);
        let frags = fragments(&whole, 1480);
        assert_eq!(frags.len(), 2);
        let first = Ipv4View::parse(&frags[0]).unwrap();
        assert!(first.more_fragments() && first.checksum_valid());
        assert_eq!(Ipv4View::parse(&frags[1]).unwrap().frag_offset(), 185);
        for f in &frags {
            assert_eq!(rs.eval(f), Verdict::Drop { reply: None });
        }
    }

    #[test]
    fn a_fragment_does_not_evade_a_port_redirect() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, Some(53), TARGET, Some(5353), None));
        // A TCP first fragment too short to hold the header, and its tail.
        let whole = tcp_pkt(GUEST, DST_A, 5000, 53, frame::TCP_SYN);
        for f in &fragments(&whole, 8) {
            assert_eq!(rs.eval(f), Verdict::Drop { reply: None });
        }
        assert_eq!(rs.conn_count(), 0);
    }

    #[test]
    fn a_fragment_passes_where_no_port_rule_covers_it() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.1/32", Some(L4Proto::Tcp), Some(445)));
        rs.add_redirect(redirect(DST_B, Some(53), TARGET, None, Some(L4Proto::Udp)));
        // Another protocol than the rule names.
        let udp = udp_pkt(GUEST, DST_A, 5000, 445, &[0; 2000]);
        // Another address than any rule names.
        let other = udp_pkt(GUEST, Ipv4Addr::new(192, 0, 2, 9), 5000, 53, &[0; 2000]);
        // ICMP has no ports, so a port rule never covered it.
        let icmp = ipv4_build(GUEST, DST_A, IPPROTO_ICMP, 64, &[0; 2000], 45).unwrap();
        for whole in [udp, other, icmp] {
            for f in &fragments(&whole, 1480) {
                assert_eq!(rs.eval(f), Verdict::Pass);
            }
        }
    }

    #[test]
    fn a_fragment_is_never_tracked_or_rewritten_on_return() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, Some(53), TARGET, Some(5353), None));
        let q = udp_pkt(GUEST, DST_A, 5000, 53, b"q");
        assert!(matches!(rs.eval(&q), Verdict::Rewrite(_)));
        // A whole reply is rewritten, the same reply in fragments is not:
        // only its first fragment has the ports, and a fragment's checksum
        // cannot be redone.
        let reply = udp_pkt(TARGET, GUEST, 5353, 5000, &[0; 64]);
        assert!(matches!(rs.eval_return(&reply), Verdict::Rewrite(_)));
        for f in &fragments(&reply, 32) {
            assert_eq!(rs.eval_return(f), Verdict::Pass);
        }
    }

    // --- DNAT rewrite correctness ---

    #[test]
    fn dnat_tcp_rewrite_checksums_valid() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(
            DST_A,
            Some(80),
            TARGET,
            Some(8080),
            Some(L4Proto::Tcp),
        ));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        let Verdict::Rewrite(out) = rs.eval(&p) else {
            panic!("expected rewrite");
        };
        let ip = Ipv4View::parse(&out).unwrap();
        assert_eq!(ip.src(), GUEST);
        assert_eq!(ip.dst(), TARGET);
        let t = TcpView::parse(ip.payload()).unwrap();
        assert_eq!(t.src_port(), 5000);
        assert_eq!(t.dst_port(), 8080);
        assert_eq!(t.seq(), 1000);
        assert_checksums_ok(&out);
    }

    #[test]
    fn dnat_udp_rewrite_checksums_valid() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, Some(5353), None));
        let p = udp_pkt(GUEST, DST_A, 4000, 53, b"dns query");
        let Verdict::Rewrite(out) = rs.eval(&p) else {
            panic!("expected rewrite");
        };
        let ip = Ipv4View::parse(&out).unwrap();
        assert_eq!(ip.dst(), TARGET);
        let u = UdpView::parse(ip.payload()).unwrap();
        assert_eq!(u.dst_port(), 5353);
        assert_eq!(u.payload(), b"dns query");
        assert_checksums_ok(&out);
    }

    #[test]
    fn dnat_udp_zero_checksum_stays_zero() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));
        let mut p = udp_pkt(GUEST, DST_A, 4000, 53, b"q");
        let hl = Ipv4View::parse(&p).unwrap().header_len();
        p[hl + 6..hl + 8].copy_from_slice(&[0, 0]); // opt out of udp checksum
        let Verdict::Rewrite(out) = rs.eval(&p) else {
            panic!("expected rewrite");
        };
        let ip = Ipv4View::parse(&out).unwrap();
        assert!(ip.checksum_valid());
        assert_eq!(UdpView::parse(ip.payload()).unwrap().checksum(), 0);
    }

    #[test]
    fn dnat_sheds_ethernet_padding() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));
        let mut p = udp_pkt(GUEST, DST_A, 4000, 53, b"q");
        let total = p.len();
        p.extend_from_slice(&[0u8; 14]); // simulated eth min-frame padding
        let Verdict::Rewrite(out) = rs.eval(&p) else {
            panic!("expected rewrite");
        };
        assert_eq!(out.len(), total);
        assert_checksums_ok(&out);
    }

    #[test]
    fn dnat_icmp_echo() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));
        let p = icmp_echo_pkt(GUEST, DST_A, 0x1234, false);
        let Verdict::Rewrite(out) = rs.eval(&p) else {
            panic!("expected rewrite");
        };
        let ip = Ipv4View::parse(&out).unwrap();
        assert_eq!(ip.dst(), TARGET);
        assert_checksums_ok(&out);
    }

    // --- return traffic ---

    #[test]
    fn return_traffic_rewritten_tcp() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, Some(80), TARGET, Some(8080), None));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
        assert_eq!(rs.conn_count(), 1);

        // Reply from the redirect target.
        let reply = tcp_pkt(TARGET, GUEST, 8080, 5000, frame::TCP_SYN | frame::TCP_ACK);
        let Verdict::Rewrite(out) = rs.eval_return(&reply) else {
            panic!("expected return rewrite");
        };
        let ip = Ipv4View::parse(&out).unwrap();
        assert_eq!(ip.src(), DST_A, "source restored to original dst");
        assert_eq!(ip.dst(), GUEST);
        let t = TcpView::parse(ip.payload()).unwrap();
        assert_eq!(t.src_port(), 80, "source port restored");
        assert_eq!(t.dst_port(), 5000);
        assert_checksums_ok(&out);

        // Unrelated traffic from the target passes untouched.
        let other = tcp_pkt(TARGET, GUEST, 9999, 5000, frame::TCP_ACK);
        assert_eq!(rs.eval_return(&other), Verdict::Pass);
    }

    #[test]
    fn return_traffic_rewritten_udp_and_icmp() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));

        let p = udp_pkt(GUEST, DST_A, 4000, 53, b"q");
        assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
        let reply = udp_pkt(TARGET, GUEST, 53, 4000, b"a");
        let Verdict::Rewrite(out) = rs.eval_return(&reply) else {
            panic!("expected udp return rewrite");
        };
        assert_eq!(Ipv4View::parse(&out).unwrap().src(), DST_A);
        assert_checksums_ok(&out);

        let p = icmp_echo_pkt(GUEST, DST_A, 0x42, false);
        assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
        let reply = icmp_echo_pkt(TARGET, GUEST, 0x42, true);
        let Verdict::Rewrite(out) = rs.eval_return(&reply) else {
            panic!("expected icmp return rewrite");
        };
        assert_eq!(Ipv4View::parse(&out).unwrap().src(), DST_A);
        assert_checksums_ok(&out);
        // Wrong echo id passes.
        let reply = icmp_echo_pkt(TARGET, GUEST, 0x43, true);
        assert_eq!(rs.eval_return(&reply), Verdict::Pass);
    }

    // --- block reply shape ---

    #[test]
    fn block_tcp_syn_gets_rst_ack() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", None, None));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_SYN);
        let Verdict::Drop { reply: Some(r) } = rs.eval(&p) else {
            panic!("expected drop with reply");
        };
        let ip = Ipv4View::parse(&r).unwrap();
        assert_eq!(ip.src(), DST_A);
        assert_eq!(ip.dst(), GUEST);
        let t = TcpView::parse(ip.payload()).unwrap();
        assert!(t.is_rst() && t.is_ack());
        assert_eq!(t.src_port(), 80);
        assert_eq!(t.dst_port(), 5000);
        // SYN with seq 1000 and no payload → ack = 1001.
        assert_eq!(t.ack(), 1001);
        assert_eq!(t.seq(), 0);
        assert_checksums_ok(&r);
    }

    #[test]
    fn block_tcp_ack_gets_rst_at_ack() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", None, None));
        let seg = tcp_build(
            GUEST,
            DST_A,
            TcpFields {
                src_port: 5000,
                dst_port: 80,
                seq: 1000,
                ack: 7777,
                flags: frame::TCP_ACK,
                window: 65535,
                options: &[],
            },
            b"data",
        )
        .unwrap();
        let p = ipv4_build(GUEST, DST_A, IPPROTO_TCP, 64, &seg, 1).unwrap();
        let Verdict::Drop { reply: Some(r) } = rs.eval(&p) else {
            panic!("expected drop with reply");
        };
        let t = TcpView::parse(Ipv4View::parse(&r).unwrap().payload()).unwrap();
        assert!(t.is_rst() && !t.is_ack());
        assert_eq!(t.seq(), 7777, "rst seq taken from incoming ack");
    }

    #[test]
    fn block_does_not_answer_rst() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", None, None));
        let p = tcp_pkt(GUEST, DST_A, 5000, 80, frame::TCP_RST);
        assert_eq!(rs.eval(&p), Verdict::Drop { reply: None });
    }

    #[test]
    fn block_udp_gets_port_unreachable() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", None, None));
        let p = udp_pkt(GUEST, DST_A, 5000, 53, b"query");
        let Verdict::Drop { reply: Some(r) } = rs.eval(&p) else {
            panic!("expected drop with reply");
        };
        let ip = Ipv4View::parse(&r).unwrap();
        assert_eq!(ip.src(), DST_A);
        assert_eq!(ip.dst(), GUEST);
        assert_eq!(ip.proto(), IPPROTO_ICMP);
        let i = IcmpView::parse(ip.payload()).unwrap();
        assert_eq!(i.icmp_type(), ICMP_DEST_UNREACHABLE);
        assert_eq!(i.code(), 3, "port unreachable");
        // Quotes the original IP header + 8 bytes.
        assert_eq!(&i.payload()[..20], &p[..20]);
        assert_checksums_ok(&r);
    }

    #[test]
    fn block_icmp_gets_host_unreachable() {
        let mut rs = RuleSet::new();
        rs.add_block(block("192.0.2.0/24", Some(L4Proto::Icmp), None));
        let p = icmp_echo_pkt(GUEST, DST_A, 9, false);
        let Verdict::Drop { reply: Some(r) } = rs.eval(&p) else {
            panic!("expected drop with reply");
        };
        let ip = Ipv4View::parse(&r).unwrap();
        let i = IcmpView::parse(ip.payload()).unwrap();
        assert_eq!(i.icmp_type(), ICMP_DEST_UNREACHABLE);
        assert_eq!(i.code(), 1, "host unreachable");
        assert_checksums_ok(&r);
    }

    /// The translation table drains under connection churn instead of
    /// growing forever, and expiry does not disturb fresh flows.
    #[test]
    fn conn_table_drains_under_churn() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));

        for sport in 0..200u16 {
            let p = tcp_pkt(GUEST, DST_A, 10_000 + sport, 80, frame::TCP_SYN);
            assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
        }
        assert_eq!(rs.conn_count(), 200);

        // Everything idles out.
        rs.expire_conns_older_than(Duration::ZERO);
        assert_eq!(rs.conn_count(), 0);

        // A fresh flow re-tracks and its return path still translates.
        let p = tcp_pkt(GUEST, DST_A, 6000, 80, frame::TCP_SYN);
        assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
        assert_eq!(rs.conn_count(), 1);
        let reply = tcp_pkt(TARGET, GUEST, 80, 6000, frame::TCP_ACK);
        assert!(matches!(rs.eval_return(&reply), Verdict::Rewrite(_)));
    }

    fn guest_n(n: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 213, 7, n)
    }

    /// Review F7: 200 000 redirected packets that differ in source port
    /// leave no more entries than one guest's share, and a packet that
    /// matches no rule does not pay for the size of the table.
    #[test]
    fn eval_cost_does_not_grow_with_the_connection_table() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));

        for i in 0..200_000u32 {
            let p = tcp_pkt(GUEST, DST_A, i as u16, 1 + (i >> 16) as u16, frame::TCP_SYN);
            assert!(matches!(rs.eval(&p), Verdict::Rewrite(_)));
            // Checked on the way, not at the end: without a limit every one
            // of these calls walks the whole table.
            assert!(
                rs.conn_count() <= MAX_CONNS_PER_GUEST,
                "one guest holds {} entries, over its limit of {MAX_CONNS_PER_GUEST}",
                rs.conn_count()
            );
        }
        assert_eq!(rs.conn_count(), MAX_CONNS_PER_GUEST);

        // The newest connection is tracked: its reply is rewritten.
        let last = 199_999u32;
        let reply = tcp_pkt(
            TARGET,
            GUEST,
            1 + (last >> 16) as u16,
            last as u16,
            frame::TCP_ACK,
        );
        assert!(matches!(rs.eval_return(&reply), Verdict::Rewrite(_)));

        // A packet no rule matches: 1 000 of them with the table at its
        // limit. One walk of the table per second is all they may pay.
        let other = tcp_pkt(GUEST, Ipv4Addr::new(192, 0, 2, 99), 1, 80, frame::TCP_SYN);
        let start = Instant::now();
        for _ in 0..1_000 {
            assert!(matches!(rs.eval(&other), Verdict::Pass));
        }
        let took = start.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "1 000 calls took {took:?} with {} entries",
            rs.conn_count()
        );
    }

    /// A guest at its limit replaces its own entry idle the longest and
    /// leaves the entries of another guest alone, and no number of guests
    /// takes the table past its size.
    #[test]
    fn a_guest_at_its_limit_does_not_push_out_another_guest() {
        let mut rs = RuleSet::new();
        rs.add_redirect(redirect(DST_A, None, TARGET, None, None));

        let quiet = tcp_pkt(guest_n(1), DST_A, 4000, 80, frame::TCP_SYN);
        assert!(matches!(rs.eval(&quiet), Verdict::Rewrite(_)));
        for sport in 0..(2 * MAX_CONNS_PER_GUEST as u16) {
            rs.eval(&tcp_pkt(guest_n(2), DST_A, sport, 80, frame::TCP_SYN));
        }
        assert_eq!(rs.conn_count(), MAX_CONNS_PER_GUEST + 1);
        let reply = tcp_pkt(TARGET, guest_n(1), 80, 4000, frame::TCP_ACK);
        assert!(
            matches!(rs.eval_return(&reply), Verdict::Rewrite(_)),
            "the quiet guest's connection is still tracked"
        );
        // The busy guest's first connection made room for its later ones.
        let reply = tcp_pkt(TARGET, guest_n(2), 80, 0, frame::TCP_ACK);
        assert!(matches!(rs.eval_return(&reply), Verdict::Pass));

        for n in 3..10 {
            for sport in 0..MAX_CONNS_PER_GUEST as u16 {
                rs.eval(&tcp_pkt(guest_n(n), DST_A, sport, 80, frame::TCP_SYN));
            }
        }
        assert_eq!(rs.conn_count(), MAX_CONNS);
    }
}
