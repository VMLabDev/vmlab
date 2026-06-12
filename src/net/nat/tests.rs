//! NAT engine integration tests. A fake guest crafts ethernet frames into
//! [`NatEngine::handle_frame`] and parses frames off the output channel —
//! i.e. the tests implement a tiny TCP peer of their own and exercise the
//! engine over real loopback sockets.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::config::model::MacAddr;
use crate::net::frame::{
    self, ETHERTYPE_IPV4, EthView, ICMP_DEST_UNREACHABLE, ICMP_ECHO_REPLY, ICMP_ECHO_REQUEST,
    IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP, IcmpView, Ipv4View, TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST,
    TCP_SYN, TcpFields, TcpView, UdpView,
};

use super::{NatConfig, NatEngine, Pinger, PortForwarder};

const GW_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 1);
const GW_MAC: MacAddr = MacAddr([0x02, 0xAA, 0, 0, 0, 0x01]);
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 10);
const GUEST_MAC: MacAddr = MacAddr([0x02, 0xBB, 0, 0, 0, 0x10]);

const WAIT: Duration = Duration::from_secs(3);

fn engine() -> (Arc<NatEngine>, mpsc::Receiver<Bytes>) {
    let (tx, rx) = mpsc::channel(256);
    (NatEngine::new(NatConfig::new(GW_IP, GW_MAC), tx), rx)
}

fn engine_with_pinger(p: Arc<dyn Pinger>) -> (Arc<NatEngine>, mpsc::Receiver<Bytes>) {
    let (tx, rx) = mpsc::channel(256);
    (
        NatEngine::with_pinger(NatConfig::new(GW_IP, GW_MAC), tx, p),
        rx,
    )
}

// ---------------------------------------------------------------------------
// Fake-guest helpers
// ---------------------------------------------------------------------------

/// A parsed engine→guest TCP frame.
#[derive(Debug, Clone)]
struct GTcp {
    src: (Ipv4Addr, u16),
    dst: (Ipv4Addr, u16),
    seq: u32,
    ack: u32,
    flags: u8,
    options: Vec<u8>,
    payload: Vec<u8>,
}

/// A parsed engine→guest UDP frame.
#[derive(Debug, Clone)]
struct GUdp {
    src: (Ipv4Addr, u16),
    dst: (Ipv4Addr, u16),
    payload: Vec<u8>,
}

async fn recv_frame(rx: &mut mpsc::Receiver<Bytes>) -> Bytes {
    timeout(WAIT, rx.recv())
        .await
        .expect("timed out waiting for an output frame")
        .expect("engine output channel closed")
}

/// Next TCP frame off the output (skipping anything else), with checksum
/// and addressing assertions.
async fn recv_tcp(rx: &mut mpsc::Receiver<Bytes>) -> GTcp {
    loop {
        let f = recv_frame(rx).await;
        let eth = EthView::parse(&f).expect("eth parses");
        assert_eq!(eth.src_mac(), GW_MAC, "frames are sourced from the gw mac");
        if eth.ethertype() != ETHERTYPE_IPV4 {
            continue;
        }
        let ip = Ipv4View::parse(eth.payload()).expect("ipv4 parses");
        assert!(ip.checksum_valid(), "ip checksum");
        if ip.proto() != IPPROTO_TCP {
            continue;
        }
        let t = TcpView::parse(ip.payload()).expect("tcp parses");
        assert!(t.checksum_valid(ip.src(), ip.dst()), "tcp checksum");
        return GTcp {
            src: (ip.src(), t.src_port()),
            dst: (ip.dst(), t.dst_port()),
            seq: t.seq(),
            ack: t.ack(),
            flags: t.flags(),
            options: t.options().to_vec(),
            payload: t.payload().to_vec(),
        };
    }
}

async fn recv_udp(rx: &mut mpsc::Receiver<Bytes>) -> GUdp {
    loop {
        let f = recv_frame(rx).await;
        let eth = EthView::parse(&f).expect("eth parses");
        if eth.ethertype() != ETHERTYPE_IPV4 {
            continue;
        }
        let ip = Ipv4View::parse(eth.payload()).expect("ipv4 parses");
        if ip.proto() != IPPROTO_UDP {
            continue;
        }
        let u = UdpView::parse(ip.payload()).expect("udp parses");
        assert!(u.checksum_valid(ip.src(), ip.dst()), "udp checksum");
        return GUdp {
            src: (ip.src(), u.src_port()),
            dst: (ip.dst(), u.dst_port()),
            payload: u.payload().to_vec(),
        };
    }
}

/// Send one guest TCP segment into the engine.
#[allow(clippy::too_many_arguments)]
async fn guest_tcp(
    engine: &Arc<NatEngine>,
    sport: u16,
    dst: (Ipv4Addr, u16),
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
    options: &[u8],
) {
    let seg = frame::tcp_build(
        GUEST_IP,
        dst.0,
        TcpFields {
            src_port: sport,
            dst_port: dst.1,
            seq,
            ack,
            flags,
            window: 65535,
            options,
        },
        payload,
    );
    let pkt = frame::ipv4_build(GUEST_IP, dst.0, IPPROTO_TCP, 64, &seg, 7);
    let f = frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt);
    engine.handle_frame(Bytes::from(f)).await;
}

async fn guest_udp(engine: &Arc<NatEngine>, sport: u16, dst: (Ipv4Addr, u16), payload: &[u8]) {
    let seg = frame::udp_build(GUEST_IP, dst.0, sport, dst.1, payload);
    let pkt = frame::ipv4_build(GUEST_IP, dst.0, IPPROTO_UDP, 64, &seg, 8);
    let f = frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt);
    engine.handle_frame(Bytes::from(f)).await;
}

async fn guest_icmp_echo(engine: &Arc<NatEngine>, dst: Ipv4Addr, id: u16, body: &[u8]) {
    let m = frame::icmp_build(
        ICMP_ECHO_REQUEST,
        0,
        [(id >> 8) as u8, id as u8, 0, 1],
        body,
    );
    let pkt = frame::ipv4_build(GUEST_IP, dst, IPPROTO_ICMP, 64, &m, 9);
    let f = frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt);
    engine.handle_frame(Bytes::from(f)).await;
}

/// Spawn a loopback TCP echo server; returns its address.
async fn tcp_echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

// ---------------------------------------------------------------------------
// vTCP passive (guest → world)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vtcp_passive_echo_roundtrip() {
    let addr = tcp_echo_server().await;
    let dst = (Ipv4Addr::new(127, 0, 0, 1), addr.port());
    let (engine, mut rx) = engine();
    let sport = 40000;
    let isn = 1000u32;

    // SYN →
    guest_tcp(
        &engine,
        sport,
        dst,
        isn,
        0,
        TCP_SYN,
        b"",
        &[2, 4, 0x05, 0xB4],
    )
    .await;
    // ← SYN-ACK
    let synack = recv_tcp(&mut rx).await;
    assert_eq!(synack.flags & (TCP_SYN | TCP_ACK), TCP_SYN | TCP_ACK);
    assert_eq!(synack.src, dst, "src is the original destination");
    assert_eq!(synack.dst, (GUEST_IP, sport));
    assert_eq!(synack.ack, isn.wrapping_add(1));
    // MSS clamp advertised: mtu(1500) - 40 = 1460.
    assert_eq!(&synack.options[..4], &[2, 4, 0x05, 0xB4]);
    let s_isn = synack.seq;

    // ACK → (established)
    let mut seq = isn + 1;
    let mut g_ack = s_isn.wrapping_add(1);
    guest_tcp(&engine, sport, dst, seq, g_ack, TCP_ACK, b"", &[]).await;

    // data → ; ← ACK + ← echoed data
    let msg = b"hello vtcp";
    guest_tcp(&engine, sport, dst, seq, g_ack, TCP_ACK | TCP_PSH, msg, &[]).await;
    seq += msg.len() as u32;
    let mut got_ack = false;
    let mut echoed: Option<GTcp> = None;
    while !(got_ack && echoed.is_some()) {
        let t = recv_tcp(&mut rx).await;
        if t.payload.is_empty() {
            assert_eq!(t.ack, seq, "cumulative ack covers the data");
            got_ack = true;
        } else {
            assert_eq!(t.payload, msg, "echo payload");
            assert_eq!(t.seq, g_ack, "first data byte follows the syn-ack");
            echoed = Some(t);
        }
    }
    let echoed = echoed.unwrap();
    g_ack = echoed.seq.wrapping_add(echoed.payload.len() as u32);
    guest_tcp(&engine, sport, dst, seq, g_ack, TCP_ACK, b"", &[]).await;

    // FIN → ; ← ACK of FIN; server closes → ← FIN; ACK → .
    guest_tcp(&engine, sport, dst, seq, g_ack, TCP_ACK | TCP_FIN, b"", &[]).await;
    seq += 1;
    let mut got_fin_ack = false;
    let mut got_fin = false;
    while !(got_fin_ack && got_fin) {
        let t = recv_tcp(&mut rx).await;
        if t.flags & TCP_FIN != 0 {
            assert_eq!(t.seq, g_ack, "engine fin in sequence");
            g_ack = t.seq.wrapping_add(1);
            guest_tcp(&engine, sport, dst, seq, g_ack, TCP_ACK, b"", &[]).await;
            got_fin = true;
        } else if t.ack == seq {
            got_fin_ack = true;
        }
    }
}

#[tokio::test]
async fn vtcp_rst_on_connect_refused() {
    // Bind then drop to find a (very probably still) closed port.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let dst = (Ipv4Addr::new(127, 0, 0, 1), closed);
    let (engine, mut rx) = engine();

    guest_tcp(&engine, 40001, dst, 5555, 0, TCP_SYN, b"", &[]).await;
    let t = recv_tcp(&mut rx).await;
    assert_ne!(t.flags & TCP_RST, 0, "refused connect resets the guest");
    assert_eq!(t.ack, 5556, "rst acks the syn");
    assert_eq!(t.src, dst);
}

#[tokio::test]
async fn vtcp_unknown_flow_gets_rst() {
    let (engine, mut rx) = engine();
    let dst = (Ipv4Addr::new(192, 0, 2, 7), 80);
    // A naked ACK for a flow the engine has never seen.
    guest_tcp(&engine, 40002, dst, 123, 9876, TCP_ACK, b"", &[]).await;
    let t = recv_tcp(&mut rx).await;
    assert_ne!(t.flags & TCP_RST, 0);
    assert_eq!(t.seq, 9876, "rst seq taken from the segment's ack");
}

// ---------------------------------------------------------------------------
// UDP NAT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn udp_roundtrip_via_loopback_echo() {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(&buf[..n], from).await;
        }
    });
    let dst = (Ipv4Addr::new(127, 0, 0, 1), addr.port());
    let (engine, mut rx) = engine();

    guest_udp(&engine, 50000, dst, b"dns-ish query").await;
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.src, dst, "reply translated back from the original dst");
    assert_eq!(u.dst, (GUEST_IP, 50000));
    assert_eq!(u.payload, b"dns-ish query");

    // Same flow again — the socket is reused, replies keep flowing.
    guest_udp(&engine, 50000, dst, b"second").await;
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.payload, b"second");
}

// ---------------------------------------------------------------------------
// Active open (engine → guest)
// ---------------------------------------------------------------------------

/// Complete the guest side of an engine-originated handshake; returns
/// (gw endpoint, guest seq, guest ack) ready for data exchange.
async fn guest_accept_active(
    engine: &Arc<NatEngine>,
    rx: &mut mpsc::Receiver<Bytes>,
    guest_port: u16,
    guest_isn: u32,
) -> ((Ipv4Addr, u16), u32, u32) {
    let syn = recv_tcp(rx).await;
    assert_eq!(syn.flags & (TCP_SYN | TCP_ACK), TCP_SYN);
    assert_eq!(
        syn.src.0, GW_IP,
        "active open originates from the gateway ip"
    );
    assert_eq!(syn.dst, (GUEST_IP, guest_port));
    let gw = syn.src;
    // SYN-ACK →
    guest_tcp(
        engine,
        guest_port,
        gw,
        guest_isn,
        syn.seq.wrapping_add(1),
        TCP_SYN | TCP_ACK,
        b"",
        &[2, 4, 0x05, 0xB4],
    )
    .await;
    // ← ACK
    let ack = recv_tcp(rx).await;
    assert_eq!(ack.flags & (TCP_SYN | TCP_ACK), TCP_ACK);
    assert_eq!(ack.ack, guest_isn.wrapping_add(1));
    (gw, guest_isn.wrapping_add(1), syn.seq.wrapping_add(1))
}

#[tokio::test]
async fn active_open_handshake_and_data() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let open = engine.open_tcp_to_guest(GUEST_IP, 8080);
    let guest = guest_accept_active(&engine, &mut rx, 8080, 70000);
    let (stream, (gw, mut g_seq, mut g_ack)) = tokio::join!(open, guest);
    let mut stream = stream.expect("active open succeeds");

    // Engine-side write surfaces as a data segment to the guest.
    stream.write_all(b"ping").await.unwrap();
    let data = recv_tcp(&mut rx).await;
    assert_eq!(data.payload, b"ping");
    assert_eq!(data.seq, g_ack);
    g_ack = g_ack.wrapping_add(4);
    guest_tcp(&engine, 8080, gw, g_seq, g_ack, TCP_ACK, b"", &[]).await;

    // Guest data surfaces on the GuestStream.
    guest_tcp(
        &engine,
        8080,
        gw,
        g_seq,
        g_ack,
        TCP_ACK | TCP_PSH,
        b"pong",
        &[],
    )
    .await;
    g_seq = g_seq.wrapping_add(4);
    let mut buf = [0u8; 16];
    let n = timeout(WAIT, stream.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"pong");
    // And gets acked.
    let ack = recv_tcp(&mut rx).await;
    assert_eq!(ack.ack, g_seq);
}

#[tokio::test]
async fn active_open_rst_means_refused() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let open = engine.open_tcp_to_guest(GUEST_IP, 81);
    let guest = async {
        let syn = recv_tcp(&mut rx).await;
        guest_tcp(
            &engine,
            81,
            syn.src,
            0,
            syn.seq.wrapping_add(1),
            TCP_RST | TCP_ACK,
            b"",
            &[],
        )
        .await;
    };
    let (res, ()) = tokio::join!(open, guest);
    assert!(res.is_err(), "guest RST fails the open");
}

#[tokio::test]
async fn retransmits_unacked_data_within_budget() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let open = engine.open_tcp_to_guest(GUEST_IP, 8081);
    let guest = guest_accept_active(&engine, &mut rx, 8081, 90000);
    let (stream, (gw, g_seq, g_ack)) = tokio::join!(open, guest);
    let mut stream = stream.expect("active open succeeds");

    stream.write_all(b"lost?").await.unwrap();
    let first = recv_tcp(&mut rx).await;
    assert_eq!(first.payload, b"lost?");

    // Don't ACK: the 1 s RTO must retransmit the same segment.
    let started = std::time::Instant::now();
    let retx = recv_tcp(&mut rx).await;
    let elapsed = started.elapsed();
    assert_eq!(retx.seq, first.seq, "same sequence number");
    assert_eq!(retx.payload, b"lost?", "same payload");
    assert!(
        elapsed >= Duration::from_millis(500) && elapsed <= Duration::from_millis(2500),
        "retransmit after ~1s RTO, got {elapsed:?}"
    );

    // ACK it so the flow quiesces.
    let end = first.seq.wrapping_add(first.payload.len() as u32);
    guest_tcp(&engine, 8081, gw, g_seq, end, TCP_ACK, b"", &[]).await;
    let _ = g_ack;
}

// ---------------------------------------------------------------------------
// Port forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tcp_port_forward_end_to_end() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fwd_addr = listener.local_addr().unwrap();
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            PortForwarder::serve_tcp(listener, engine, GUEST_IP, 80).await;
        });
    }

    // The "guest web server": completes the handshake, echoes one request.
    let guest = tokio::spawn(async move {
        let (gw, mut g_seq, mut g_ack) = guest_accept_active(&engine, &mut rx, 80, 31337).await;
        // Expect the client's request bytes.
        let req = recv_tcp(&mut rx).await;
        assert_eq!(req.payload, b"GET /");
        g_ack = g_ack.wrapping_add(req.payload.len() as u32);
        guest_tcp(&engine, 80, gw, g_seq, g_ack, TCP_ACK, b"", &[]).await;
        guest_tcp(
            &engine,
            80,
            gw,
            g_seq,
            g_ack,
            TCP_ACK | TCP_PSH,
            b"200 OK",
            &[],
        )
        .await;
        g_seq = g_seq.wrapping_add(6);
        // Swallow the ack for our data.
        let ack = recv_tcp(&mut rx).await;
        assert_eq!(ack.ack, g_seq);
    });

    let mut client = TcpStream::connect(fwd_addr).await.unwrap();
    client.write_all(b"GET /").await.unwrap();
    let mut buf = [0u8; 16];
    let n = timeout(WAIT, client.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"200 OK");
    guest.await.unwrap();
}

#[tokio::test]
async fn udp_port_forward_end_to_end() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let fwd_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let fwd_addr = fwd_sock.local_addr().unwrap();
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            PortForwarder::serve_udp(fwd_sock, engine, GUEST_IP, 53).await;
        });
    }

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(fwd_addr).await.unwrap();
    client.send(b"query").await.unwrap();

    // Guest sees the datagram from gw_ip:eph and answers it.
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.src.0, GW_IP);
    assert_eq!(u.dst, (GUEST_IP, 53));
    assert_eq!(u.payload, b"query");
    guest_udp(&engine, 53, u.src, b"answer").await;

    let mut buf = [0u8; 16];
    let n = timeout(WAIT, client.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"answer");
}

// ---------------------------------------------------------------------------
// Engine-originated UDP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn udp_bind_guest_flow_roundtrip() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let (to_guest, mut from_guest) = engine.udp_bind_guest_flow(GUEST_IP, 9999);
    to_guest.send(b"hi guest".to_vec()).await.unwrap();
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.src.0, GW_IP);
    assert_eq!(u.dst, (GUEST_IP, 9999));
    assert_eq!(u.payload, b"hi guest");

    // The guest replies to gw_ip:eph; it lands on the receiver.
    guest_udp(&engine, 9999, u.src, b"hi engine").await;
    let got = timeout(WAIT, from_guest.recv()).await.unwrap().unwrap();
    assert_eq!(got, b"hi engine");
}

#[tokio::test]
async fn udp_to_guest_one_shot() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);
    engine.udp_to_guest(GUEST_IP, 7000, b"beep").await;
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.src.0, GW_IP);
    assert_eq!(u.dst, (GUEST_IP, 7000));
    assert_eq!(u.payload, b"beep");
}

// ---------------------------------------------------------------------------
// ICMP (stubbed pinger; no real subprocess)
// ---------------------------------------------------------------------------

struct StubPinger {
    reachable: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl Pinger for StubPinger {
    async fn ping(&self, _dst: Ipv4Addr) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.reachable
    }
}

#[tokio::test]
async fn icmp_echo_reply_synthesized_and_cached() {
    let pinger = Arc::new(StubPinger {
        reachable: true,
        calls: AtomicUsize::new(0),
    });
    let (engine, mut rx) = engine_with_pinger(pinger.clone());
    let dst = Ipv4Addr::new(192, 0, 2, 50);

    guest_icmp_echo(&engine, dst, 0xBEEF, b"payload-1").await;
    let f = recv_frame(&mut rx).await;
    let eth = EthView::parse(&f).unwrap();
    assert_eq!(eth.dst_mac(), GUEST_MAC);
    let ip = Ipv4View::parse(eth.payload()).unwrap();
    assert_eq!(ip.src(), dst, "reply appears to come from the pinged host");
    assert_eq!(ip.dst(), GUEST_IP);
    let icmp = IcmpView::parse(ip.payload()).unwrap();
    assert_eq!(icmp.icmp_type(), ICMP_ECHO_REPLY);
    assert!(icmp.checksum_valid());
    assert_eq!(icmp.rest(), [0xBE, 0xEF, 0, 1], "id/seq mirrored");
    assert_eq!(icmp.payload(), b"payload-1");

    // Second echo hits the 10 s cache: reply arrives, probe count stays 1.
    guest_icmp_echo(&engine, dst, 0xBEEF, b"payload-2").await;
    let f = recv_frame(&mut rx).await;
    let ip = Ipv4View::parse(EthView::parse(&f).unwrap().payload()).unwrap();
    let icmp = IcmpView::parse(ip.payload()).unwrap();
    assert_eq!(icmp.payload(), b"payload-2");
    assert_eq!(
        pinger.calls.load(Ordering::SeqCst),
        1,
        "cached, no second probe"
    );
}

#[tokio::test]
async fn icmp_unreachable_when_probe_fails() {
    let pinger = Arc::new(StubPinger {
        reachable: false,
        calls: AtomicUsize::new(0),
    });
    let (engine, mut rx) = engine_with_pinger(pinger);
    let dst = Ipv4Addr::new(192, 0, 2, 51);

    guest_icmp_echo(&engine, dst, 7, b"x").await;
    let f = recv_frame(&mut rx).await;
    let ip = Ipv4View::parse(EthView::parse(&f).unwrap().payload()).unwrap();
    assert_eq!(ip.src(), GW_IP, "unreachable originates from the gateway");
    assert_eq!(ip.dst(), GUEST_IP);
    let icmp = IcmpView::parse(ip.payload()).unwrap();
    assert_eq!(icmp.icmp_type(), ICMP_DEST_UNREACHABLE);
    assert_eq!(icmp.code(), 1, "host unreachable");
    assert!(icmp.checksum_valid());
}

#[tokio::test]
async fn icmp_echo_to_gateway_is_ignored() {
    // The gateway answers pings to itself elsewhere; the NAT engine must
    // not double-reply.
    let pinger = Arc::new(StubPinger {
        reachable: true,
        calls: AtomicUsize::new(0),
    });
    let (engine, mut rx) = engine_with_pinger(pinger.clone());
    guest_icmp_echo(&engine, GW_IP, 1, b"x").await;
    assert!(
        timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "no frame emitted"
    );
    assert_eq!(pinger.calls.load(Ordering::SeqCst), 0);
}
