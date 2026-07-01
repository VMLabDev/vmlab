//! Ethernet / ARP / IPv4 / UDP / TCP / ICMP frame views and builders.
//!
//! Views are zero-copy wrappers over `&[u8]` that validate length and basic
//! structure on construction; every parser returns `Option` and never panics
//! on truncated or malformed input. Builders return owned `Vec<u8>` with all
//! checksums computed.

use std::net::Ipv4Addr;

use crate::config::model::MacAddr;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// Recognised for completeness; the fabric is IPv4-only (tests use this to
/// exercise the "not ours" paths).
#[allow(dead_code)]
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

pub const IPPROTO_ICMP: u8 = 1;
pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;

/// Length of an ethernet header (no 802.1Q tag).
pub const ETH_HEADER_LEN: usize = 14;

/// The all-ones broadcast MAC.
pub const MAC_BROADCAST: MacAddr = MacAddr([0xff; 6]);

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;
pub const TCP_URG: u8 = 0x20;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn mac_at(buf: &[u8], off: usize) -> MacAddr {
    let mut m = [0u8; 6];
    m.copy_from_slice(&buf[off..off + 6]);
    MacAddr(m)
}

fn ipv4_at(buf: &[u8], off: usize) -> Ipv4Addr {
    Ipv4Addr::new(buf[off], buf[off + 1], buf[off + 2], buf[off + 3])
}

/// RFC 1071 internet checksum (one's-complement sum of 16-bit big-endian
/// words, odd trailing byte padded with zero), returned complemented and
/// ready to write into a header.
pub fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    if let &[last] = chunks.remainder() {
        sum += u32::from(u16::from_be_bytes([last, 0]));
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Internet checksum over the IPv4 pseudo-header (src, dst, zero, proto,
/// length) followed by the full L4 segment. Used by UDP, TCP and (without
/// effect of the pseudo-header) verified against zero for valid segments.
pub fn l4_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, segment: &[u8]) -> u16 {
    let mut buf = Vec::with_capacity(12 + segment.len());
    buf.extend_from_slice(&src.octets());
    buf.extend_from_slice(&dst.octets());
    buf.push(0);
    buf.push(proto);
    let len = u16::try_from(segment.len()).expect("l4 segment too large");
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(segment);
    internet_checksum(&buf)
}

// ---------------------------------------------------------------------------
// Ethernet
// ---------------------------------------------------------------------------

/// View over an ethernet II frame.
#[derive(Debug, Clone, Copy)]
pub struct EthView<'a> {
    buf: &'a [u8],
}

impl<'a> EthView<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        (buf.len() >= ETH_HEADER_LEN).then_some(Self { buf })
    }

    pub fn dst_mac(&self) -> MacAddr {
        mac_at(self.buf, 0)
    }

    pub fn src_mac(&self) -> MacAddr {
        mac_at(self.buf, 6)
    }

    pub fn ethertype(&self) -> u16 {
        u16_at(self.buf, 12)
    }

    pub fn payload(&self) -> &'a [u8] {
        &self.buf[ETH_HEADER_LEN..]
    }
}

/// Build an ethernet II frame.
pub fn eth_build(dst: MacAddr, src: MacAddr, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH_HEADER_LEN + payload.len());
    f.extend_from_slice(&dst.0);
    f.extend_from_slice(&src.0);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

// ---------------------------------------------------------------------------
// ARP (IPv4 over ethernet only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArpOp {
    Request,
    Reply,
}

const ARP_LEN: usize = 28;

/// View over an ARP packet (the ethernet payload) for IPv4-over-ethernet:
/// htype 1, ptype 0x0800, hlen 6, plen 4. Anything else fails to parse.
#[derive(Debug, Clone, Copy)]
pub struct ArpView<'a> {
    buf: &'a [u8],
}

// Each header view exposes its complete field surface, consumers or not.
#[allow(dead_code)]
impl<'a> ArpView<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < ARP_LEN {
            return None;
        }
        if u16_at(buf, 0) != 1 || u16_at(buf, 2) != ETHERTYPE_IPV4 || buf[4] != 6 || buf[5] != 4 {
            return None;
        }
        if !matches!(u16_at(buf, 6), 1 | 2) {
            return None;
        }
        Some(Self { buf })
    }

    pub fn op(&self) -> ArpOp {
        match u16_at(self.buf, 6) {
            1 => ArpOp::Request,
            _ => ArpOp::Reply,
        }
    }

    /// Sender hardware address.
    pub fn sha(&self) -> MacAddr {
        mac_at(self.buf, 8)
    }

    /// Sender protocol address.
    pub fn spa(&self) -> Ipv4Addr {
        ipv4_at(self.buf, 14)
    }

    /// Target hardware address.
    pub fn tha(&self) -> MacAddr {
        mac_at(self.buf, 18)
    }

    /// Target protocol address.
    pub fn tpa(&self) -> Ipv4Addr {
        ipv4_at(self.buf, 24)
    }
}

fn arp_build(op: u16, sha: MacAddr, spa: Ipv4Addr, tha: MacAddr, tpa: Ipv4Addr) -> Vec<u8> {
    let mut p = Vec::with_capacity(ARP_LEN);
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    p.push(6);
    p.push(4);
    p.extend_from_slice(&op.to_be_bytes());
    p.extend_from_slice(&sha.0);
    p.extend_from_slice(&spa.octets());
    p.extend_from_slice(&tha.0);
    p.extend_from_slice(&tpa.octets());
    p
}

/// Build a complete ethernet frame carrying an ARP request (who-has `tpa`),
/// broadcast at L2. The fabric never sends requests (MACs are learned
/// passively); tests use this to synthesise guest ARP traffic.
#[cfg(test)]
pub fn arp_request_build(sha: MacAddr, spa: Ipv4Addr, tpa: Ipv4Addr) -> Vec<u8> {
    let arp = arp_build(1, sha, spa, MacAddr([0; 6]), tpa);
    eth_build(MAC_BROADCAST, sha, ETHERTYPE_ARP, &arp)
}

/// Build a complete ethernet frame carrying an ARP reply (`spa` is-at `sha`),
/// unicast to `tha`.
pub fn arp_reply_build(sha: MacAddr, spa: Ipv4Addr, tha: MacAddr, tpa: Ipv4Addr) -> Vec<u8> {
    let arp = arp_build(2, sha, spa, tha, tpa);
    eth_build(tha, sha, ETHERTYPE_ARP, &arp)
}

// ---------------------------------------------------------------------------
// IPv4
// ---------------------------------------------------------------------------

const IPV4_MIN_HEADER_LEN: usize = 20;

/// View over an IPv4 packet (the ethernet payload). Handles header options
/// (IHL > 5) and trailing buffer padding (ethernet minimum-frame padding):
/// `payload()` honours both the header length and `total_len`.
#[derive(Debug, Clone, Copy)]
pub struct Ipv4View<'a> {
    buf: &'a [u8],
}

// Each header view exposes its complete field surface, consumers or not.
#[allow(dead_code)]
impl<'a> Ipv4View<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < IPV4_MIN_HEADER_LEN {
            return None;
        }
        if buf[0] >> 4 != 4 {
            return None;
        }
        let header_len = usize::from(buf[0] & 0x0F) * 4;
        if header_len < IPV4_MIN_HEADER_LEN || header_len > buf.len() {
            return None;
        }
        let total_len = usize::from(u16_at(buf, 2));
        if total_len < header_len || total_len > buf.len() {
            return None;
        }
        Some(Self { buf })
    }

    pub fn version(&self) -> u8 {
        self.buf[0] >> 4
    }

    pub fn ihl(&self) -> u8 {
        self.buf[0] & 0x0F
    }

    pub fn header_len(&self) -> usize {
        usize::from(self.ihl()) * 4
    }

    pub fn total_len(&self) -> u16 {
        u16_at(self.buf, 2)
    }

    pub fn id(&self) -> u16 {
        u16_at(self.buf, 4)
    }

    /// The three flag bits (reserved, DF, MF).
    pub fn flags(&self) -> u8 {
        self.buf[6] >> 5
    }

    pub fn dont_fragment(&self) -> bool {
        self.flags() & 0b010 != 0
    }

    pub fn more_fragments(&self) -> bool {
        self.flags() & 0b001 != 0
    }

    /// Fragment offset in 8-byte units.
    pub fn frag_offset(&self) -> u16 {
        u16_at(self.buf, 6) & 0x1FFF
    }

    pub fn ttl(&self) -> u8 {
        self.buf[8]
    }

    pub fn proto(&self) -> u8 {
        self.buf[9]
    }

    pub fn header_checksum(&self) -> u16 {
        u16_at(self.buf, 10)
    }

    pub fn src(&self) -> Ipv4Addr {
        ipv4_at(self.buf, 12)
    }

    pub fn dst(&self) -> Ipv4Addr {
        ipv4_at(self.buf, 16)
    }

    /// Header options (empty when IHL == 5).
    pub fn options(&self) -> &'a [u8] {
        &self.buf[IPV4_MIN_HEADER_LEN..self.header_len()]
    }

    /// L4 payload, bounded by `total_len` (ignores ethernet padding).
    pub fn payload(&self) -> &'a [u8] {
        &self.buf[self.header_len()..usize::from(self.total_len())]
    }

    /// Verify the header checksum.
    pub fn checksum_valid(&self) -> bool {
        internet_checksum(&self.buf[..self.header_len()]) == 0
    }
}

/// Build an IPv4 packet (no options, IHL 5) with the header checksum
/// computed. Flags and fragment offset are zero.
pub fn ipv4_build(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    proto: u8,
    ttl: u8,
    payload: &[u8],
    id: u16,
) -> Vec<u8> {
    let total_len =
        u16::try_from(IPV4_MIN_HEADER_LEN + payload.len()).expect("ipv4 payload too large");
    let mut p = Vec::with_capacity(IPV4_MIN_HEADER_LEN + payload.len());
    p.push(0x45); // version 4, IHL 5
    p.push(0); // DSCP/ECN
    p.extend_from_slice(&total_len.to_be_bytes());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // flags + fragment offset
    p.push(ttl);
    p.push(proto);
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let csum = internet_checksum(&p);
    p[10..12].copy_from_slice(&csum.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

const UDP_HEADER_LEN: usize = 8;

/// View over a UDP datagram (the IPv4 payload).
#[derive(Debug, Clone, Copy)]
pub struct UdpView<'a> {
    buf: &'a [u8],
}

// Each header view exposes its complete field surface, consumers or not.
#[allow(dead_code)]
impl<'a> UdpView<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < UDP_HEADER_LEN {
            return None;
        }
        let len = usize::from(u16_at(buf, 4));
        if len < UDP_HEADER_LEN || len > buf.len() {
            return None;
        }
        Some(Self { buf })
    }

    pub fn src_port(&self) -> u16 {
        u16_at(self.buf, 0)
    }

    pub fn dst_port(&self) -> u16 {
        u16_at(self.buf, 2)
    }

    /// The UDP length field (header + payload).
    pub fn len(&self) -> u16 {
        u16_at(self.buf, 4)
    }

    pub fn is_empty(&self) -> bool {
        usize::from(self.len()) == UDP_HEADER_LEN
    }

    pub fn checksum(&self) -> u16 {
        u16_at(self.buf, 6)
    }

    pub fn payload(&self) -> &'a [u8] {
        &self.buf[UDP_HEADER_LEN..usize::from(self.len())]
    }

    /// Verify the checksum against the IPv4 pseudo-header. A zero checksum
    /// (transmitter opted out, legal on IPv4) is accepted.
    pub fn checksum_valid(&self, src: Ipv4Addr, dst: Ipv4Addr) -> bool {
        if self.checksum() == 0 {
            return true;
        }
        l4_checksum(src, dst, IPPROTO_UDP, &self.buf[..usize::from(self.len())]) == 0
    }
}

/// Build a UDP datagram with the pseudo-header checksum computed (a computed
/// checksum of zero is transmitted as 0xFFFF per RFC 768).
pub fn udp_build(
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let len = u16::try_from(UDP_HEADER_LEN + payload.len()).expect("udp payload too large");
    let mut p = Vec::with_capacity(UDP_HEADER_LEN + payload.len());
    p.extend_from_slice(&src_port.to_be_bytes());
    p.extend_from_slice(&dst_port.to_be_bytes());
    p.extend_from_slice(&len.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(payload);
    let csum = match l4_checksum(src_ip, dst_ip, IPPROTO_UDP, &p) {
        0 => 0xFFFF,
        c => c,
    };
    p[6..8].copy_from_slice(&csum.to_be_bytes());
    p
}

// ---------------------------------------------------------------------------
// TCP
// ---------------------------------------------------------------------------

const TCP_MIN_HEADER_LEN: usize = 20;
const TCP_MAX_OPTIONS_LEN: usize = 40;

/// View over a TCP segment (the IPv4 payload).
#[derive(Debug, Clone, Copy)]
pub struct TcpView<'a> {
    buf: &'a [u8],
}

// Each header view exposes its complete field surface, consumers or not.
#[allow(dead_code)]
impl<'a> TcpView<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < TCP_MIN_HEADER_LEN {
            return None;
        }
        let data_offset = usize::from(buf[12] >> 4) * 4;
        if data_offset < TCP_MIN_HEADER_LEN || data_offset > buf.len() {
            return None;
        }
        Some(Self { buf })
    }

    pub fn src_port(&self) -> u16 {
        u16_at(self.buf, 0)
    }

    pub fn dst_port(&self) -> u16 {
        u16_at(self.buf, 2)
    }

    pub fn seq(&self) -> u32 {
        u32_at(self.buf, 4)
    }

    pub fn ack(&self) -> u32 {
        u32_at(self.buf, 8)
    }

    pub fn data_offset(&self) -> usize {
        usize::from(self.buf[12] >> 4) * 4
    }

    /// Raw flag byte (CWR/ECE bits included).
    pub fn flags(&self) -> u8 {
        self.buf[13]
    }

    pub fn is_fin(&self) -> bool {
        self.flags() & TCP_FIN != 0
    }

    pub fn is_syn(&self) -> bool {
        self.flags() & TCP_SYN != 0
    }

    pub fn is_rst(&self) -> bool {
        self.flags() & TCP_RST != 0
    }

    pub fn is_psh(&self) -> bool {
        self.flags() & TCP_PSH != 0
    }

    pub fn is_ack(&self) -> bool {
        self.flags() & TCP_ACK != 0
    }

    pub fn is_urg(&self) -> bool {
        self.flags() & TCP_URG != 0
    }

    pub fn window(&self) -> u16 {
        u16_at(self.buf, 14)
    }

    pub fn checksum(&self) -> u16 {
        u16_at(self.buf, 16)
    }

    /// Options bytes (empty when data offset == 5 words).
    pub fn options(&self) -> &'a [u8] {
        &self.buf[TCP_MIN_HEADER_LEN..self.data_offset()]
    }

    pub fn payload(&self) -> &'a [u8] {
        &self.buf[self.data_offset()..]
    }

    /// Verify the checksum against the IPv4 pseudo-header.
    pub fn checksum_valid(&self, src: Ipv4Addr, dst: Ipv4Addr) -> bool {
        l4_checksum(src, dst, IPPROTO_TCP, self.buf) == 0
    }
}

/// Header fields for [`tcp_build`].
#[derive(Debug, Clone, Copy)]
pub struct TcpFields<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    /// Flag bits ([`TCP_SYN`] | [`TCP_ACK`] | ...).
    pub flags: u8,
    pub window: u16,
    /// Raw options; padded with zero (end-of-options) to a 4-byte boundary,
    /// truncated past 40 bytes.
    pub options: &'a [u8],
}

/// Build a TCP segment with the pseudo-header checksum computed.
pub fn tcp_build(
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    fields: TcpFields<'_>,
    payload: &[u8],
) -> Vec<u8> {
    let opts = &fields.options[..fields.options.len().min(TCP_MAX_OPTIONS_LEN)];
    let opts_padded = opts.len().div_ceil(4) * 4;
    let header_len = TCP_MIN_HEADER_LEN + opts_padded;
    let mut p = Vec::with_capacity(header_len + payload.len());
    p.extend_from_slice(&fields.src_port.to_be_bytes());
    p.extend_from_slice(&fields.dst_port.to_be_bytes());
    p.extend_from_slice(&fields.seq.to_be_bytes());
    p.extend_from_slice(&fields.ack.to_be_bytes());
    p.push(((header_len / 4) as u8) << 4);
    p.push(fields.flags);
    p.extend_from_slice(&fields.window.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(&[0, 0]); // urgent pointer
    p.extend_from_slice(opts);
    p.resize(header_len, 0); // option padding (end-of-options)
    p.extend_from_slice(payload);
    let csum = l4_checksum(src_ip, dst_ip, IPPROTO_TCP, &p);
    p[16..18].copy_from_slice(&csum.to_be_bytes());
    p
}

// ---------------------------------------------------------------------------
// ICMP
// ---------------------------------------------------------------------------

const ICMP_HEADER_LEN: usize = 8;

pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_DEST_UNREACHABLE: u8 = 3;
pub const ICMP_ECHO_REQUEST: u8 = 8;

/// View over an ICMP message (the IPv4 payload).
#[derive(Debug, Clone, Copy)]
pub struct IcmpView<'a> {
    buf: &'a [u8],
}

// Each header view exposes its complete field surface, consumers or not.
#[allow(dead_code)]
impl<'a> IcmpView<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        (buf.len() >= ICMP_HEADER_LEN).then_some(Self { buf })
    }

    pub fn icmp_type(&self) -> u8 {
        self.buf[0]
    }

    pub fn code(&self) -> u8 {
        self.buf[1]
    }

    pub fn checksum(&self) -> u16 {
        u16_at(self.buf, 2)
    }

    /// The 4 "rest of header" bytes (identifier/sequence for echo, unused
    /// for unreachable, ...).
    pub fn rest(&self) -> [u8; 4] {
        [self.buf[4], self.buf[5], self.buf[6], self.buf[7]]
    }

    pub fn payload(&self) -> &'a [u8] {
        &self.buf[ICMP_HEADER_LEN..]
    }

    /// Verify the checksum (over the whole ICMP message).
    pub fn checksum_valid(&self) -> bool {
        internet_checksum(self.buf) == 0
    }
}

/// Build an ICMP message with the checksum computed.
pub fn icmp_build(icmp_type: u8, code: u8, rest: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(ICMP_HEADER_LEN + payload.len());
    p.push(icmp_type);
    p.push(code);
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(&rest);
    p.extend_from_slice(payload);
    let csum = internet_checksum(&p);
    p[2..4].copy_from_slice(&csum.to_be_bytes());
    p
}

/// Given a full IPv4 packet containing an ICMP echo request, build the full
/// IPv4 echo-reply packet (src/dst swapped, identifier/sequence and payload
/// echoed). Returns `None` if the input is not a well-formed echo request.
pub fn icmp_echo_reply_for(request_ipv4_packet: &[u8]) -> Option<Vec<u8>> {
    let ip = Ipv4View::parse(request_ipv4_packet)?;
    if ip.proto() != IPPROTO_ICMP {
        return None;
    }
    let icmp = IcmpView::parse(ip.payload())?;
    if icmp.icmp_type() != ICMP_ECHO_REQUEST || icmp.code() != 0 {
        return None;
    }
    let reply = icmp_build(ICMP_ECHO_REPLY, 0, icmp.rest(), icmp.payload());
    Some(ipv4_build(
        ip.dst(),
        ip.src(),
        IPPROTO_ICMP,
        64,
        &reply,
        ip.id(),
    ))
}

/// Build an ICMP destination-unreachable message (type 3, the given code)
/// quoting the original packet's IP header plus the first 8 payload bytes,
/// per RFC 792. The return value is the ICMP message (an IPv4 *payload*);
/// the caller wraps it in its own IPv4 packet. Tolerates truncated input by
/// quoting whatever is available.
pub fn icmp_unreachable_for(original_ipv4: &[u8], code: u8) -> Vec<u8> {
    let quote_len = match Ipv4View::parse(original_ipv4) {
        Some(ip) => (ip.header_len() + 8).min(original_ipv4.len()),
        None => original_ipv4.len().min(28),
    };
    icmp_build(
        ICMP_DEST_UNREACHABLE,
        code,
        [0; 4],
        &original_ipv4[..quote_len],
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(n: u8) -> MacAddr {
        MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, n])
    }

    const IP_A: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 10);
    const IP_B: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 1);

    // --- internet checksum ---

    #[test]
    fn checksum_known_vector() {
        // Classic IPv4 header example, checksum field zeroed. Hand-computed:
        // 4500+003C+1C46+4000+4006+0000+AC10+0A63+AC10+0A0C, with carries
        // folded, sums to 0x4E19; one's complement is 0xB1E6.
        let header: [u8; 20] = [
            0x45, 0x00, 0x00, 0x3C, 0x1C, 0x46, 0x40, 0x00, 0x40, 0x06, 0x00, 0x00, 0xAC, 0x10,
            0x0A, 0x63, 0xAC, 0x10, 0x0A, 0x0C,
        ];
        assert_eq!(internet_checksum(&header), 0xB1E6);
        // With the checksum filled in, the sum over the header complements
        // to zero.
        let mut filled = header;
        filled[10..12].copy_from_slice(&0xB1E6u16.to_be_bytes());
        assert_eq!(internet_checksum(&filled), 0);
    }

    #[test]
    fn checksum_odd_length() {
        // Words: 0x0102, 0x0300 (pad) => sum 0x0402, complement 0xFBFD.
        assert_eq!(internet_checksum(&[0x01, 0x02, 0x03]), 0xFBFD);
        assert_eq!(internet_checksum(&[0xFF]), !0xFF00);
    }

    #[test]
    fn checksum_empty_and_carry_fold() {
        assert_eq!(internet_checksum(&[]), 0xFFFF);
        // 0xFFFF + 0x0001 folds the carry: sum = 0x0001, complement 0xFFFE.
        assert_eq!(internet_checksum(&[0xFF, 0xFF, 0x00, 0x01]), 0xFFFE);
    }

    // --- ethernet ---

    #[test]
    fn eth_roundtrip() {
        let f = eth_build(mac(1), mac(2), ETHERTYPE_IPV4, b"hello");
        let v = EthView::parse(&f).unwrap();
        assert_eq!(v.dst_mac(), mac(1));
        assert_eq!(v.src_mac(), mac(2));
        assert_eq!(v.ethertype(), ETHERTYPE_IPV4);
        assert_eq!(v.payload(), b"hello");
    }

    #[test]
    fn eth_truncated_is_none() {
        let f = eth_build(mac(1), mac(2), ETHERTYPE_ARP, b"");
        for n in 0..ETH_HEADER_LEN {
            assert!(EthView::parse(&f[..n]).is_none(), "len {n} parsed");
        }
        assert!(EthView::parse(&f).is_some());
    }

    // --- ARP ---

    #[test]
    fn arp_request_roundtrip() {
        let f = arp_request_build(mac(1), IP_A, IP_B);
        let eth = EthView::parse(&f).unwrap();
        assert_eq!(eth.dst_mac(), MAC_BROADCAST);
        assert_eq!(eth.src_mac(), mac(1));
        assert_eq!(eth.ethertype(), ETHERTYPE_ARP);
        let arp = ArpView::parse(eth.payload()).unwrap();
        assert_eq!(arp.op(), ArpOp::Request);
        assert_eq!(arp.sha(), mac(1));
        assert_eq!(arp.spa(), IP_A);
        assert_eq!(arp.tha(), MacAddr([0; 6]));
        assert_eq!(arp.tpa(), IP_B);
    }

    #[test]
    fn arp_reply_roundtrip() {
        let f = arp_reply_build(mac(2), IP_B, mac(1), IP_A);
        let eth = EthView::parse(&f).unwrap();
        assert_eq!(eth.dst_mac(), mac(1));
        let arp = ArpView::parse(eth.payload()).unwrap();
        assert_eq!(arp.op(), ArpOp::Reply);
        assert_eq!(arp.sha(), mac(2));
        assert_eq!(arp.spa(), IP_B);
        assert_eq!(arp.tha(), mac(1));
        assert_eq!(arp.tpa(), IP_A);
    }

    #[test]
    fn arp_rejects_malformed() {
        let f = arp_request_build(mac(1), IP_A, IP_B);
        let arp = &f[ETH_HEADER_LEN..];
        for n in 0..arp.len() {
            assert!(ArpView::parse(&arp[..n]).is_none(), "len {n} parsed");
        }
        // Wrong ptype (IPv6).
        let mut bad = arp.to_vec();
        bad[2..4].copy_from_slice(&ETHERTYPE_IPV6.to_be_bytes());
        assert!(ArpView::parse(&bad).is_none());
        // Bogus opcode.
        let mut bad = arp.to_vec();
        bad[6..8].copy_from_slice(&9u16.to_be_bytes());
        assert!(ArpView::parse(&bad).is_none());
    }

    // --- IPv4 ---

    #[test]
    fn ipv4_roundtrip_and_checksum() {
        let p = ipv4_build(IP_A, IP_B, IPPROTO_UDP, 64, b"payload", 0x1234);
        let v = Ipv4View::parse(&p).unwrap();
        assert_eq!(v.version(), 4);
        assert_eq!(v.ihl(), 5);
        assert_eq!(v.header_len(), 20);
        assert_eq!(v.total_len(), 27);
        assert_eq!(v.id(), 0x1234);
        assert_eq!(v.flags(), 0);
        assert!(!v.dont_fragment());
        assert!(!v.more_fragments());
        assert_eq!(v.frag_offset(), 0);
        assert_eq!(v.ttl(), 64);
        assert_eq!(v.proto(), IPPROTO_UDP);
        assert_eq!(v.src(), IP_A);
        assert_eq!(v.dst(), IP_B);
        assert!(v.options().is_empty());
        assert_eq!(v.payload(), b"payload");
        assert!(v.checksum_valid());
    }

    #[test]
    fn ipv4_corrupt_checksum_detected() {
        let mut p = ipv4_build(IP_A, IP_B, IPPROTO_TCP, 64, b"x", 1);
        p[8] = p[8].wrapping_add(1); // mutate TTL without fixing checksum
        let v = Ipv4View::parse(&p).unwrap();
        assert!(!v.checksum_valid());
    }

    #[test]
    fn ipv4_options_aware_payload_offset() {
        // Hand-build a header with IHL=6 (4 bytes of options).
        let mut p = vec![
            0x46,
            0x00,
            0x00,
            0x1C, // version/ihl, tos, total_len = 28
            0x00,
            0x01,
            0x00,
            0x00, // id, flags/frag
            0x40,
            IPPROTO_UDP,
            0x00,
            0x00, // ttl, proto, checksum
            10,
            213,
            0,
            10, // src
            10,
            213,
            0,
            1, // dst
            0x01,
            0x01,
            0x01,
            0x00, // options (NOPs + EOL)
        ];
        let csum = internet_checksum(&p);
        p[10..12].copy_from_slice(&csum.to_be_bytes());
        p.extend_from_slice(b"data"); // 4 payload bytes => total 28
        let v = Ipv4View::parse(&p).unwrap();
        assert_eq!(v.ihl(), 6);
        assert_eq!(v.header_len(), 24);
        assert_eq!(v.options(), &[0x01, 0x01, 0x01, 0x00]);
        assert_eq!(v.payload(), b"data");
        assert!(v.checksum_valid());
    }

    #[test]
    fn ipv4_honours_total_len_with_padding() {
        // Ethernet pads short frames; the view must clip to total_len.
        let mut p = ipv4_build(IP_A, IP_B, IPPROTO_UDP, 64, b"ab", 7);
        p.extend_from_slice(&[0u8; 18]); // trailing pad
        let v = Ipv4View::parse(&p).unwrap();
        assert_eq!(v.payload(), b"ab");
    }

    #[test]
    fn ipv4_rejects_malformed() {
        let p = ipv4_build(IP_A, IP_B, IPPROTO_UDP, 64, b"payload", 9);
        for n in 0..IPV4_MIN_HEADER_LEN {
            assert!(Ipv4View::parse(&p[..n]).is_none(), "len {n} parsed");
        }
        // total_len larger than the buffer (truncated packet).
        assert!(Ipv4View::parse(&p[..p.len() - 1]).is_none());
        // Wrong version nibble.
        let mut bad = p.clone();
        bad[0] = 0x65;
        assert!(Ipv4View::parse(&bad).is_none());
        // IHL below minimum.
        let mut bad = p.clone();
        bad[0] = 0x44;
        assert!(Ipv4View::parse(&bad).is_none());
        // IHL pointing past the buffer.
        let mut bad = p;
        bad[0] = 0x4F;
        bad.truncate(20);
        assert!(Ipv4View::parse(&bad).is_none());
    }

    // --- UDP ---

    #[test]
    fn udp_roundtrip_with_pseudo_header_checksum() {
        let d = udp_build(IP_A, IP_B, 5353, 53, b"query");
        let v = UdpView::parse(&d).unwrap();
        assert_eq!(v.src_port(), 5353);
        assert_eq!(v.dst_port(), 53);
        assert_eq!(usize::from(v.len()), d.len());
        assert_eq!(v.payload(), b"query");
        assert_ne!(v.checksum(), 0, "checksum must be computed");
        assert!(v.checksum_valid(IP_A, IP_B));
        // Wrong pseudo-header (different src ip) must fail verification.
        assert!(!v.checksum_valid(IP_B, IP_B));
        // Corrupt payload must fail verification.
        let mut bad = d.clone();
        *bad.last_mut().unwrap() ^= 0xFF;
        assert!(!UdpView::parse(&bad).unwrap().checksum_valid(IP_A, IP_B));
    }

    #[test]
    fn udp_zero_checksum_accepted() {
        let mut d = udp_build(IP_A, IP_B, 1, 2, b"x");
        d[6..8].copy_from_slice(&[0, 0]);
        assert!(UdpView::parse(&d).unwrap().checksum_valid(IP_A, IP_B));
    }

    #[test]
    fn udp_rejects_malformed() {
        let d = udp_build(IP_A, IP_B, 1, 2, b"abc");
        for n in 0..UDP_HEADER_LEN {
            assert!(UdpView::parse(&d[..n]).is_none(), "len {n} parsed");
        }
        // Length field exceeding the buffer.
        assert!(UdpView::parse(&d[..d.len() - 1]).is_none());
        // Length field below header size.
        let mut bad = d;
        bad[4..6].copy_from_slice(&3u16.to_be_bytes());
        assert!(UdpView::parse(&bad).is_none());
    }

    // --- TCP ---

    #[test]
    fn tcp_roundtrip_flags_and_options() {
        let fields = TcpFields {
            src_port: 49152,
            dst_port: 443,
            seq: 0xDEADBEEF,
            ack: 0x01020304,
            flags: TCP_SYN | TCP_ACK,
            window: 65000,
            options: &[0x02, 0x04, 0x05, 0xB4, 0x01], // MSS + NOP (5 bytes, padded to 8)
        };
        let s = tcp_build(IP_A, IP_B, fields, b"data");
        let v = TcpView::parse(&s).unwrap();
        assert_eq!(v.src_port(), 49152);
        assert_eq!(v.dst_port(), 443);
        assert_eq!(v.seq(), 0xDEADBEEF);
        assert_eq!(v.ack(), 0x01020304);
        assert!(v.is_syn() && v.is_ack());
        assert!(!v.is_fin() && !v.is_rst() && !v.is_psh() && !v.is_urg());
        assert_eq!(v.window(), 65000);
        assert_eq!(v.data_offset(), 28);
        assert_eq!(
            v.options(),
            &[0x02, 0x04, 0x05, 0xB4, 0x01, 0x00, 0x00, 0x00]
        );
        assert_eq!(v.payload(), b"data");
        assert!(v.checksum_valid(IP_A, IP_B));
        // Note: swapping src/dst is checksum-neutral (one's-complement sums
        // commute), so use a genuinely different address to test mismatch.
        assert!(!v.checksum_valid(Ipv4Addr::new(192, 168, 0, 1), IP_B));
    }

    #[test]
    fn tcp_no_options_and_all_flags() {
        let fields = TcpFields {
            src_port: 1,
            dst_port: 2,
            seq: 0,
            ack: 0,
            flags: TCP_FIN | TCP_RST | TCP_PSH | TCP_URG,
            window: 0,
            options: &[],
        };
        let s = tcp_build(IP_A, IP_B, fields, b"");
        let v = TcpView::parse(&s).unwrap();
        assert_eq!(v.data_offset(), 20);
        assert!(v.options().is_empty());
        assert!(v.payload().is_empty());
        assert!(v.is_fin() && v.is_rst() && v.is_psh() && v.is_urg());
        assert!(!v.is_syn() && !v.is_ack());
        assert!(v.checksum_valid(IP_A, IP_B));
    }

    #[test]
    fn tcp_rejects_malformed() {
        let fields = TcpFields {
            src_port: 1,
            dst_port: 2,
            seq: 1,
            ack: 0,
            flags: TCP_SYN,
            window: 100,
            options: &[],
        };
        let s = tcp_build(IP_A, IP_B, fields, b"abc");
        for n in 0..TCP_MIN_HEADER_LEN {
            assert!(TcpView::parse(&s[..n]).is_none(), "len {n} parsed");
        }
        // Data offset below minimum.
        let mut bad = s.clone();
        bad[12] = 4 << 4;
        assert!(TcpView::parse(&bad).is_none());
        // Data offset past the buffer.
        let mut bad = s;
        bad[12] = 15 << 4;
        bad.truncate(20);
        assert!(TcpView::parse(&bad).is_none());
    }

    // --- ICMP ---

    #[test]
    fn icmp_roundtrip() {
        let m = icmp_build(ICMP_ECHO_REQUEST, 0, [0xAB, 0xCD, 0x00, 0x01], b"ping-data");
        let v = IcmpView::parse(&m).unwrap();
        assert_eq!(v.icmp_type(), ICMP_ECHO_REQUEST);
        assert_eq!(v.code(), 0);
        assert_eq!(v.rest(), [0xAB, 0xCD, 0x00, 0x01]);
        assert_eq!(v.payload(), b"ping-data");
        assert!(v.checksum_valid());
        let mut bad = m;
        bad[5] ^= 0xFF;
        assert!(!IcmpView::parse(&bad).unwrap().checksum_valid());
        // Odd-length ICMP payload still checksums correctly.
        let odd = icmp_build(ICMP_ECHO_REQUEST, 0, [0; 4], b"odd");
        assert!(IcmpView::parse(&odd).unwrap().checksum_valid());
    }

    #[test]
    fn icmp_echo_reply_for_request() {
        let echo = icmp_build(ICMP_ECHO_REQUEST, 0, [0x12, 0x34, 0x00, 0x07], b"abcdefgh");
        let req = ipv4_build(IP_A, IP_B, IPPROTO_ICMP, 64, &echo, 42);
        let reply = icmp_echo_reply_for(&req).unwrap();
        let ip = Ipv4View::parse(&reply).unwrap();
        assert_eq!(ip.src(), IP_B);
        assert_eq!(ip.dst(), IP_A);
        assert_eq!(ip.proto(), IPPROTO_ICMP);
        assert!(ip.checksum_valid());
        let icmp = IcmpView::parse(ip.payload()).unwrap();
        assert_eq!(icmp.icmp_type(), ICMP_ECHO_REPLY);
        assert_eq!(icmp.rest(), [0x12, 0x34, 0x00, 0x07]);
        assert_eq!(icmp.payload(), b"abcdefgh");
        assert!(icmp.checksum_valid());
    }

    #[test]
    fn icmp_echo_reply_rejects_non_echo() {
        // Not ICMP at all.
        let udp = udp_build(IP_A, IP_B, 1, 2, b"x");
        let p = ipv4_build(IP_A, IP_B, IPPROTO_UDP, 64, &udp, 1);
        assert!(icmp_echo_reply_for(&p).is_none());
        // ICMP but not an echo request.
        let m = icmp_build(ICMP_ECHO_REPLY, 0, [0; 4], b"");
        let p = ipv4_build(IP_A, IP_B, IPPROTO_ICMP, 64, &m, 1);
        assert!(icmp_echo_reply_for(&p).is_none());
        // Truncated input.
        assert!(icmp_echo_reply_for(&[0x45, 0x00]).is_none());
    }

    #[test]
    fn icmp_unreachable_quotes_header_plus_8() {
        let udp = udp_build(IP_A, IP_B, 1000, 2000, b"0123456789abcdef");
        let orig = ipv4_build(IP_A, IP_B, IPPROTO_UDP, 64, &udp, 5);
        let m = icmp_unreachable_for(&orig, 3); // port unreachable
        let v = IcmpView::parse(&m).unwrap();
        assert_eq!(v.icmp_type(), ICMP_DEST_UNREACHABLE);
        assert_eq!(v.code(), 3);
        assert!(v.checksum_valid());
        // Quoted data: 20-byte IP header + first 8 payload bytes.
        assert_eq!(v.payload(), &orig[..28]);
        // Truncated original must not panic and quotes what exists.
        let m = icmp_unreachable_for(&orig[..10], 1);
        assert_eq!(IcmpView::parse(&m).unwrap().payload(), &orig[..10]);
    }

    // --- fuzz-ish truncation sweep ---

    #[test]
    fn truncation_sweep_never_panics() {
        let fields = TcpFields {
            src_port: 80,
            dst_port: 12345,
            seq: 1,
            ack: 2,
            flags: TCP_ACK,
            window: 1024,
            options: &[0x01, 0x01],
        };
        let tcp = tcp_build(IP_A, IP_B, fields, b"body");
        let ip = ipv4_build(IP_A, IP_B, IPPROTO_TCP, 64, &tcp, 99);
        let frame = eth_build(mac(1), mac(2), ETHERTYPE_IPV4, &ip);
        for n in 0..=frame.len() {
            let b = &frame[..n];
            let _ = EthView::parse(b);
            let _ = ArpView::parse(b);
            let _ = Ipv4View::parse(b);
            let _ = UdpView::parse(b);
            let _ = TcpView::parse(b);
            let _ = IcmpView::parse(b);
            let _ = icmp_echo_reply_for(b);
            let _ = icmp_unreachable_for(b, 0);
        }
        // Same sweep over garbage.
        let garbage: Vec<u8> = (0..128u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(11))
            .collect();
        for n in 0..=garbage.len() {
            let b = &garbage[..n];
            let _ = EthView::parse(b);
            let _ = ArpView::parse(b);
            let _ = Ipv4View::parse(b);
            let _ = UdpView::parse(b);
            let _ = TcpView::parse(b);
            let _ = IcmpView::parse(b);
        }
    }
}
