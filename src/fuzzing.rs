//! Entry points for the cargo-fuzz targets under `fuzz/` (`just fuzz`).
//!
//! The parsers of guest and registry input live in private modules, so each
//! target's body is here and the fuzz crate only forwards its bytes. Compiled
//! under `--cfg fuzzing` alone, which cargo-fuzz sets; no release build has
//! this module. The list of parsers and their targets is
//! `docs/reviews/fuzz-coverage.md`.

use std::net::Ipv4Addr;

use ipnet::Ipv4Net;

use crate::config::model::{BlockRule, HostPort, MacAddr, RedirectRule, SinkholeMode};
use crate::net::dhcp::{DhcpConfig, DhcpServer};
use crate::net::dns::{DnsAction, DnsServer, DnsZone};
use crate::net::frame::{
    ArpView, EthView, IcmpView, Ipv4View, TcpView, UdpView, icmp_echo_reply_for,
    icmp_unreachable_for,
};
use crate::net::rules::{RuleSet, Verdict};
use crate::oci;

const GW: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 1);
const GW_MAC: MacAddr = MacAddr([0x52, 0x56, 0, 0, 0, 1]);

/// One ethernet frame as a guest NIC sends it, through every sans-I/O
/// consumer of the userspace fabric: the header views with each accessor, the
/// ICMP reply builders, the L3 rules, the DHCP server and the DNS server. The
/// NAT engine is left out on purpose: it opens host sockets to the frame's
/// destination.
///
/// Every reply the servers build must parse again and carry valid checksums,
/// and every packet the rules rewrite must still be an IPv4 packet.
pub fn net_frame(data: &[u8]) {
    views(data);

    // A /29 pool, so a few frames exhaust it and the reuse paths run.
    let subnet = Ipv4Net::new(GW, 29).expect("a /29 is a valid prefix");
    let mut config = DhcpConfig::new(subnet, GW, GW_MAC);
    config.dns_server = Some(GW);
    config.domain = Some("vmlab.internal".into());
    config.routes = vec![(
        Ipv4Net::new(Ipv4Addr::new(10, 213, 2, 0), 24).expect("a /24 is a valid prefix"),
        GW,
    )];
    config
        .reservations
        .insert(MacAddr([0x02, 0, 0, 0, 0, 9]), Ipv4Addr::new(10, 213, 1, 5));
    let mut dhcp = DhcpServer::new(config);

    let mut zone = DnsZone::new("vmlab.internal");
    zone.register("web", Ipv4Addr::new(10, 213, 1, 5));
    zone.set_static("*.apps.vmlab.internal", Ipv4Addr::new(10, 213, 1, 6));
    zone.add_sinkhole("*.blocked.test", SinkholeMode::Nxdomain);
    zone.upstream = Some("192.0.2.1:53".parse().expect("a literal socket address"));
    let dns = DnsServer::new(zone);

    // One block and one redirect for each shape a rule takes: a whole
    // network, and one address with and without a port.
    let mut rules = RuleSet::new();
    rules.add_block(BlockRule {
        cidr: Ipv4Net::new(Ipv4Addr::new(10, 213, 9, 0), 24).expect("a /24 is a valid prefix"),
        proto: None,
        port: None,
        span: (0, 0),
    });
    rules.add_block(BlockRule {
        cidr: Ipv4Net::new(Ipv4Addr::new(0, 0, 0, 0), 1).expect("a /1 is a valid prefix"),
        proto: None,
        port: Some(25),
        span: (0, 0),
    });
    for (from, to) in [(None, None), (Some(80), Some(8080)), (Some(53), None)] {
        rules.add_redirect(RedirectRule {
            from: HostPort {
                ip: Ipv4Addr::new(10, 213, 1, 7),
                port: from,
            },
            to: HostPort {
                ip: Ipv4Addr::new(10, 213, 1, 5),
                port: to,
            },
            proto: None,
            span: (0, 0),
        });
    }

    // The input is a sequence of frames, each behind a 2-byte length, so the
    // DHCP lease table sees DISCOVER, REQUEST, DECLINE and RELEASE in a row.
    let mut rest = data;
    while let [a, b, tail @ ..] = rest {
        let len = usize::from(u16::from_be_bytes([*a, *b])).min(tail.len());
        let (frame, next) = tail.split_at(len);
        rest = next;
        views(frame);
        if let Some(reply) = dhcp.handle(frame) {
            assert_reply(&reply);
        }
        match dns.handle(frame) {
            DnsAction::Reply(reply) => assert_reply(&reply),
            DnsAction::ForwardUpstream {
                raw_query, respond, ..
            } => {
                // What the gateway does with the upstream's answer; the
                // guest's own query stands in for it.
                if let Some(reply) = respond.build_frame(&raw_query) {
                    assert_reply(&reply);
                }
            }
            DnsAction::Ignore => {}
        }
        let _ = dhcp.leases();
        if let Some(eth) = EthView::parse(frame) {
            for verdict in [rules.eval(eth.payload()), rules.eval_return(eth.payload())] {
                match verdict {
                    Verdict::Rewrite(packet) => {
                        Ipv4View::parse(&packet).expect("a rewritten packet is IPv4");
                    }
                    Verdict::Drop {
                        reply: Some(packet),
                    } => {
                        let ip = Ipv4View::parse(&packet).expect("a block reply is IPv4");
                        assert!(ip.checksum_valid(), "block reply IPv4 checksum");
                    }
                    Verdict::Drop { reply: None } | Verdict::Pass => {}
                }
            }
        }
    }
}

/// Walk one frame through each header view and call every accessor.
fn views(frame: &[u8]) {
    let Some(eth) = EthView::parse(frame) else {
        return;
    };
    let _ = (eth.dst_mac(), eth.src_mac(), eth.ethertype());
    if let Some(arp) = ArpView::parse(eth.payload()) {
        let _ = (arp.op(), arp.sha(), arp.spa(), arp.tha(), arp.tpa());
    }
    let packet = eth.payload();
    let _ = icmp_echo_reply_for(packet);
    let _ = icmp_unreachable_for(packet, 1);
    let Some(ip) = Ipv4View::parse(packet) else {
        return;
    };
    let _ = (ip.version(), ip.ihl(), ip.total_len(), ip.id(), ip.flags());
    let _ = (ip.dont_fragment(), ip.more_fragments(), ip.frag_offset());
    let _ = (ip.ttl(), ip.proto(), ip.header_checksum(), ip.options());
    let _ = ip.checksum_valid();
    let (src, dst) = (ip.src(), ip.dst());
    if let Some(udp) = UdpView::parse(ip.payload()) {
        let _ = (udp.src_port(), udp.dst_port(), udp.len(), udp.is_empty());
        let _ = (udp.checksum(), udp.payload(), udp.checksum_valid(src, dst));
    }
    if let Some(tcp) = TcpView::parse(ip.payload()) {
        let _ = (tcp.src_port(), tcp.dst_port(), tcp.seq(), tcp.ack());
        let _ = (tcp.data_offset(), tcp.flags(), tcp.window(), tcp.checksum());
        let _ = (tcp.is_fin(), tcp.is_syn(), tcp.is_rst(), tcp.is_psh());
        let _ = (tcp.is_ack(), tcp.is_urg(), tcp.options(), tcp.payload());
        let _ = tcp.checksum_valid(src, dst);
    }
    if let Some(icmp) = IcmpView::parse(ip.payload()) {
        let _ = (icmp.icmp_type(), icmp.code(), icmp.checksum(), icmp.rest());
        let _ = (icmp.payload(), icmp.checksum_valid());
    }
}

/// A frame vmlab built itself: it must be a well-formed IPv4/UDP frame.
fn assert_reply(frame: &[u8]) {
    let eth = EthView::parse(frame).expect("a reply is an ethernet frame");
    let ip = Ipv4View::parse(eth.payload()).expect("a reply carries IPv4");
    assert!(ip.checksum_valid(), "reply IPv4 checksum");
    let udp = UdpView::parse(ip.payload()).expect("a reply carries UDP");
    assert!(udp.checksum_valid(ip.src(), ip.dst()), "reply UDP checksum");
}

/// One document a registry serves, through each parser of registry text: the
/// manifest or index with what a pull reads from it, the template config blob
/// with its conversion to store metadata, the container image config, a
/// `Www-Authenticate` challenge, and the two reference grammars.
pub fn oci_document(data: &[u8]) {
    use oci::manifest::{ManifestOrIndex, parse_manifest_or_index};

    match parse_manifest_or_index(data) {
        Ok(ManifestOrIndex::Manifest(manifest)) => {
            let _ = manifest.is_vmlab_template();
            let _ = manifest.whole_digest();
            let _ = manifest.layers_in_order();
            let _ = manifest.variants();
            let _ = manifest.variant_layout();
        }
        Ok(ManifestOrIndex::Index(index)) => {
            let _ = index.single();
            let _ = index.manifest_for_arch("x86_64");
            let _ = index.resolve(None);
            let _ = index.resolve(Some("aarch64"));
        }
        Err(_) => {}
    }

    if let Ok(config) = oci::config_blob::TemplateConfig::from_json(data) {
        let _ = config.vars();
        let again = config.to_json().expect("a parsed config serialises");
        oci::config_blob::TemplateConfig::from_json(&again).expect("and parses again");
        if let Ok(base) = config.clone().into_meta(None) {
            let _ = config.clone().into_variant(&base);
        }
        let _ = config.into_meta(None);
    }
    let _ = serde_json::from_slice::<oci::image::model::ImageConfig>(data);

    if let Ok(text) = std::str::from_utf8(data) {
        let _ = oci::auth::parse_bearer_challenge(text);
        if let Ok(reference) = oci::reference::Reference::parse(text) {
            let _ = reference.canonical();
        }
        let _ = oci::reference::with_version_tag(text, "1.2.3");
        let _ = oci::image::reference::parse_image_reference(text);
    }
}
