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

/// Generous receive timeout: passing tests return as soon as the frame
/// arrives, so this only bounds genuinely-failing runs — and the tests that
/// do real socket work (e.g. `vtcp_rst_on_connect_refused`) have flaked on
/// a shorter value under heavy parallel build load.
const WAIT: Duration = Duration::from_secs(15);

/// An engine whose "internet" is the test host's loopback: its guard is
/// open, because every listener these tests dial is bound there.
fn engine() -> (Arc<NatEngine>, mpsc::Receiver<Bytes>) {
    let (tx, rx) = mpsc::channel(256);
    let engine = NatEngine::new(NatConfig::new(GW_IP, GW_MAC), tx);
    engine.set_guard(Arc::new(super::HostGuard::open()));
    (engine, rx)
}

/// [`engine`] with a custom pinger.
fn engine_with_pinger(p: Arc<dyn Pinger>) -> (Arc<NatEngine>, mpsc::Receiver<Bytes>) {
    let (engine, rx) = unguarded(p);
    engine.set_guard(Arc::new(super::HostGuard::open()));
    (engine, rx)
}

/// An engine as [`NatEngine::with_pinger`] makes it, with no guard set.
fn unguarded(p: Arc<dyn Pinger>) -> (Arc<NatEngine>, mpsc::Receiver<Bytes>) {
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
    window: u16,
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
            window: t.window(),
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
    guest_tcp_window(engine, sport, dst, seq, ack, flags, 65535, payload, options).await;
}

/// [`guest_tcp`] advertising `window` instead of a full 64 KiB.
#[allow(clippy::too_many_arguments)]
async fn guest_tcp_window(
    engine: &Arc<NatEngine>,
    sport: u16,
    dst: (Ipv4Addr, u16),
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
    options: &[u8],
) {
    let f = tcp_frame(sport, dst, seq, ack, flags, window, payload, options);
    engine.handle_frame(f).await;
}

/// One guest TCP segment as the ethernet frame the gateway MAC receives.
#[allow(clippy::too_many_arguments)]
fn tcp_frame(
    sport: u16,
    dst: (Ipv4Addr, u16),
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
    options: &[u8],
) -> Bytes {
    let seg = frame::tcp_build(
        GUEST_IP,
        dst.0,
        TcpFields {
            src_port: sport,
            dst_port: dst.1,
            seq,
            ack,
            flags,
            window,
            options,
        },
        payload,
    )
    .unwrap();
    let pkt = frame::ipv4_build(GUEST_IP, dst.0, IPPROTO_TCP, 64, &seg, 7).unwrap();
    Bytes::from(frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt))
}

async fn guest_udp(engine: &Arc<NatEngine>, sport: u16, dst: (Ipv4Addr, u16), payload: &[u8]) {
    let seg = frame::udp_build(GUEST_IP, dst.0, sport, dst.1, payload).unwrap();
    let pkt = frame::ipv4_build(GUEST_IP, dst.0, IPPROTO_UDP, 64, &seg, 8).unwrap();
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
    let pkt = frame::ipv4_build(GUEST_IP, dst, IPPROTO_ICMP, 64, &m, 9).unwrap();
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

/// A segment arriving ahead of the stream is held and stitched in when the hole
/// fills — the guest must not have to resend it. Delivered 1-3-2, the host sees
/// one ordered stream and the ACK jumps past all three the moment 2 lands.
#[tokio::test]
async fn vtcp_reassembles_out_of_order_segments() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dst_addr = listener.local_addr().unwrap();
    let sink = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        received
    });

    let dst = (Ipv4Addr::LOCALHOST, dst_addr.port());
    let (engine, mut rx) = engine();
    let sport = 40_020;
    let isn = 7_000u32;

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
    let synack = recv_tcp(&mut rx).await;
    let g_ack = synack.seq.wrapping_add(1);
    let base = isn.wrapping_add(1);
    guest_tcp(&engine, sport, dst, base, g_ack, TCP_ACK, b"", &[]).await;

    let (a, b, c) = ([b'a'; 100], [b'b'; 100], [b'c'; 100]);
    let send = async |seq: u32, payload: &[u8]| {
        guest_tcp(
            &engine,
            sport,
            dst,
            seq,
            g_ack,
            TCP_ACK | TCP_PSH,
            payload,
            &[],
        )
        .await;
    };

    // 1: in order.
    send(base, &a).await;
    let ack = recv_tcp(&mut rx).await;
    assert_eq!(ack.ack, base.wrapping_add(100));

    // 3: ahead of the stream — dup-ACK, and (the fix) buffered rather than
    // discarded.
    send(base.wrapping_add(200), &c).await;
    let ack = recv_tcp(&mut rx).await;
    assert_eq!(
        ack.ack,
        base.wrapping_add(100),
        "a future segment must not advance the ack"
    );

    // 2: fills the hole. The ack must now cover segment 3 as well, which only
    // holds if it was still buffered.
    send(base.wrapping_add(100), &b).await;
    let ack = recv_tcp(&mut rx).await;
    assert_eq!(
        ack.ack,
        base.wrapping_add(300),
        "the buffered segment was stitched in without a retransmit"
    );

    let fin_seq = base.wrapping_add(300);
    guest_tcp(
        &engine,
        sport,
        dst,
        fin_seq,
        g_ack,
        TCP_ACK | TCP_FIN,
        b"",
        &[],
    )
    .await;
    loop {
        if recv_tcp(&mut rx).await.ack == fin_seq.wrapping_add(1) {
            break;
        }
    }

    let received = timeout(Duration::from_secs(10), sink)
        .await
        .expect("host never saw the stream")
        .expect("sink task failed");
    let mut expected = Vec::new();
    expected.extend_from_slice(&a);
    expected.extend_from_slice(&b);
    expected.extend_from_slice(&c);
    assert_eq!(received, expected, "bytes arrive in stream order");
}

/// More future segments than the per-flow buffer allows must not grow it without
/// bound: the excess is dropped like before, and the flow still completes once
/// the guest retransmits.
#[tokio::test]
async fn vtcp_out_of_order_buffer_is_bounded() {
    const SEGMENT: usize = 1400;
    // Two windows' worth of future segments, none of them contiguous with the
    // stream until the very end.
    const FUTURE_SEGMENTS: usize = (2 * 0xFFFF) / SEGMENT;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dst_addr = listener.local_addr().unwrap();
    let sink = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        received.len()
    });

    let dst = (Ipv4Addr::LOCALHOST, dst_addr.port());
    let (engine, mut rx) = engine();
    let sport = 40_021;
    let isn = 8_000u32;

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
    let synack = recv_tcp(&mut rx).await;
    let g_ack = synack.seq.wrapping_add(1);
    let base = isn.wrapping_add(1);
    guest_tcp(&engine, sport, dst, base, g_ack, TCP_ACK, b"", &[]).await;

    let payload = vec![b'x'; SEGMENT];
    // Skip segment 0 entirely, then flood the future.
    for i in 1..=FUTURE_SEGMENTS {
        guest_tcp(
            &engine,
            sport,
            dst,
            base.wrapping_add((i * SEGMENT) as u32),
            g_ack,
            TCP_ACK | TCP_PSH,
            &payload,
            &[],
        )
        .await;
        let _ = recv_tcp(&mut rx).await; // dup-ACK each time
    }

    // Now the missing first segment. Whatever is still buffered gets stitched
    // in; the rest the guest would retransmit. Either way the flow lives and the
    // ack advances past the hole.
    guest_tcp(
        &engine,
        sport,
        dst,
        base,
        g_ack,
        TCP_ACK | TCP_PSH,
        &payload,
        &[],
    )
    .await;
    let ack = recv_tcp(&mut rx).await;
    assert!(
        ack.ack != base && seq_after(ack.ack, base),
        "ack advanced past the filled hole (got {}, base {base})",
        ack.ack
    );
    let delivered = ack.ack.wrapping_sub(base) as usize;
    assert!(
        delivered <= (FUTURE_SEGMENTS + 1) * SEGMENT,
        "never acks more than was actually sent"
    );

    // Close from the guest at whatever the engine has acked, then confirm the
    // host received exactly that much.
    let fin_seq = ack.ack;
    guest_tcp(
        &engine,
        sport,
        dst,
        fin_seq,
        g_ack,
        TCP_ACK | TCP_FIN,
        b"",
        &[],
    )
    .await;
    loop {
        if recv_tcp(&mut rx).await.ack == fin_seq.wrapping_add(1) {
            break;
        }
    }
    let received = timeout(Duration::from_secs(10), sink)
        .await
        .expect("host never saw the stream")
        .expect("sink task failed");
    assert_eq!(received, delivered, "acked bytes all reached the host");
}

/// `a` strictly after `b` in sequence space.
fn seq_after(a: u32, b: u32) -> bool {
    a != b && b.wrapping_sub(a) >= 0x8000_0000
}

#[tokio::test]
async fn vtcp_passive_bulk_upload_is_complete_and_stall_free() {
    const SIZE: usize = 10 * 1024 * 1024;
    const SEGMENT: usize = 1400;
    const WINDOW_SEGMENTS: usize = 32;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dst = listener.local_addr().unwrap();
    let sink = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::with_capacity(SIZE);
        stream.read_to_end(&mut received).await.unwrap();
        received
    });

    let expected: Vec<u8> = (0..SIZE)
        .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
        .collect();
    let (engine, mut rx) = engine();
    let sport = 40_010;
    let guest_isn = 10_000u32;

    guest_tcp(
        &engine,
        sport,
        (Ipv4Addr::LOCALHOST, dst.port()),
        guest_isn,
        0,
        TCP_SYN,
        b"",
        &[2, 4, 0x05, 0xB4],
    )
    .await;
    let synack = recv_tcp(&mut rx).await;
    let server_ack = synack.seq.wrapping_add(1);
    let mut guest_seq = guest_isn.wrapping_add(1);
    guest_tcp(
        &engine,
        sport,
        (Ipv4Addr::LOCALHOST, dst.port()),
        guest_seq,
        server_ack,
        TCP_ACK,
        b"",
        &[],
    )
    .await;

    for window in expected.chunks(SEGMENT * WINDOW_SEGMENTS) {
        for payload in window.chunks(SEGMENT) {
            guest_tcp(
                &engine,
                sport,
                (Ipv4Addr::LOCALHOST, dst.port()),
                guest_seq,
                server_ack,
                TCP_ACK | TCP_PSH,
                payload,
                &[],
            )
            .await;
            guest_seq = guest_seq.wrapping_add(payload.len() as u32);
        }

        loop {
            let ack = recv_tcp(&mut rx).await;
            assert!(ack.payload.is_empty());
            if ack.ack == guest_seq {
                break;
            }
        }
    }

    guest_tcp(
        &engine,
        sport,
        (Ipv4Addr::LOCALHOST, dst.port()),
        guest_seq,
        server_ack,
        TCP_ACK | TCP_FIN,
        b"",
        &[],
    )
    .await;
    guest_seq = guest_seq.wrapping_add(1);
    loop {
        let ack = recv_tcp(&mut rx).await;
        if ack.ack == guest_seq {
            break;
        }
    }

    let received = timeout(Duration::from_secs(30), sink)
        .await
        .expect("bulk upload stalled")
        .expect("sink task failed");
    assert_eq!(received, expected);
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

/// Complete an engine-originated handshake whose SYN-ACK advertises
/// `window`; returns (gw endpoint, guest seq, first engine data seq).
async fn guest_accept_active_window(
    engine: &Arc<NatEngine>,
    rx: &mut mpsc::Receiver<Bytes>,
    guest_port: u16,
    guest_isn: u32,
    window: u16,
) -> ((Ipv4Addr, u16), u32, u32) {
    let syn = recv_tcp(rx).await;
    assert_eq!(syn.flags & (TCP_SYN | TCP_ACK), TCP_SYN);
    #[rustfmt::skip]
    guest_tcp_window(
        engine, guest_port, syn.src, guest_isn, syn.seq.wrapping_add(1),
        TCP_SYN | TCP_ACK, window, b"", &[2, 4, 0x05, 0xB4],
    )
    .await;
    let ack = recv_tcp(rx).await;
    assert_eq!(ack.flags & (TCP_SYN | TCP_ACK), TCP_ACK);
    (syn.src, guest_isn.wrapping_add(1), syn.seq.wrapping_add(1))
}

/// Every engine→guest segment until the engine has been quiet for `quiet`.
async fn drain_tcp(rx: &mut mpsc::Receiver<Bytes>, quiet: Duration) -> Vec<GTcp> {
    let mut out = Vec::new();
    while let Ok(t) = timeout(quiet, recv_tcp(rx)).await {
        out.push(t);
    }
    out
}

/// Issue #136. A guest window that is not a whole number of segments
/// (65535 over a 1460-byte MSS) used to end every window with a full MSS
/// past its right edge. Windows acked that excess without ever shrinking
/// its window and buffered a fast download until it ran out of memory.
/// The engine must stop at the edge, and send nothing into a sliver
/// smaller than a segment.
#[tokio::test]
async fn vtcp_never_sends_past_the_guest_window() {
    const WND: u16 = 4000; // two 1460-byte segments and a 1080-byte sliver
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let open = engine.open_tcp_to_guest(GUEST_IP, 8082);
    let guest = guest_accept_active_window(&engine, &mut rx, 8082, 5000, WND);
    let (stream, (gw, g_seq, base)) = tokio::join!(open, guest);
    let mut stream = stream.expect("active open succeeds");

    const TOTAL: usize = 20_000;
    stream.write_all(&[0x5A; TOTAL]).await.unwrap();
    let mut acked = base;
    let mut delivered = 0usize;
    while delivered < TOTAL {
        let burst = drain_tcp(&mut rx, Duration::from_millis(300)).await;
        let mut end = acked;
        for t in &burst {
            assert!(
                !t.payload.is_empty(),
                "only data while the window is open: {t:?}"
            );
            assert_eq!(t.seq, end, "segments are contiguous");
            end = t.seq.wrapping_add(t.payload.len() as u32);
            let used = end.wrapping_sub(acked);
            assert!(
                used <= u32::from(WND),
                "segment ends {used} bytes into a {WND}-byte window"
            );
        }
        let sent = end.wrapping_sub(acked) as usize;
        assert_eq!(
            sent,
            2920.min(TOTAL - delivered),
            "two full segments per window, no runt"
        );
        delivered += sent;
        acked = end;
        guest_tcp_window(&engine, 8082, gw, g_seq, acked, TCP_ACK, WND, b"", &[]).await;
    }
}

/// A guest that closes its window must be probed, or a lost window update
/// leaves the flow silent until the idle reset. When it reopens, data
/// resumes from where it stopped.
#[tokio::test]
async fn vtcp_probes_a_closed_guest_window() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);

    let open = engine.open_tcp_to_guest(GUEST_IP, 8083);
    let guest = guest_accept_active_window(&engine, &mut rx, 8083, 6000, 0);
    let (stream, (gw, g_seq, base)) = tokio::join!(open, guest);
    let mut stream = stream.expect("active open succeeds");

    stream
        .write_all(b"held behind a closed window")
        .await
        .unwrap();
    let probe = recv_tcp(&mut rx).await;
    assert!(
        probe.payload.is_empty(),
        "nothing is sent into a zero window: {probe:?}"
    );
    assert_eq!(probe.flags, TCP_ACK);
    assert_eq!(
        probe.seq,
        base.wrapping_sub(1),
        "probe sits one byte behind the window"
    );
    let again = recv_tcp(&mut rx).await;
    assert_eq!(again.seq, probe.seq, "unanswered, it is probed again");

    // The answer to a probe reopens the window: data follows at once.
    guest_tcp_window(&engine, 8083, gw, g_seq, base, TCP_ACK, 65535, b"", &[]).await;
    let data = loop {
        let t = recv_tcp(&mut rx).await;
        if !t.payload.is_empty() {
            break t;
        }
    };
    assert_eq!(data.seq, base);
    assert_eq!(data.payload, b"held behind a closed window");
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

    let (to_guest, mut from_guest) = engine.udp_bind_guest_flow(GUEST_IP, 9999).unwrap();
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

/// Code review F8: with every gateway port taken for one guest port, the
/// next bind spun forever holding the table lock. It must fail after one
/// pass, and a flow that ends must give its port back.
#[tokio::test]
async fn udp_bind_guest_flow_fails_when_the_range_is_exhausted_and_reuses_a_freed_port() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);
    engine.limit_ephemeral_ports(2);

    let (first, _first_rx) = engine.udp_bind_guest_flow(GUEST_IP, 9999).unwrap();
    let (_second, _second_rx) = engine.udp_bind_guest_flow(GUEST_IP, 9999).unwrap();
    first.send(b"one".to_vec()).await.unwrap();
    let first_port = recv_udp(&mut rx).await.src.1;

    // On a detached thread with a deadline, so a spin fails the test
    // instead of hanging it (the runtime would wait for a blocking task).
    let (done, refused) = std::sync::mpsc::channel();
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            let _ = done.send(engine.udp_bind_guest_flow(GUEST_IP, 9999).is_err());
        });
    }
    let refused = refused
        .recv_timeout(WAIT)
        .expect("allocation must not spin on an exhausted range");
    assert!(refused, "a third flow on a two-port range must be refused");

    // A different guest port has the whole range to itself.
    assert!(engine.udp_bind_guest_flow(GUEST_IP, 9998).is_ok());

    // Dropping a flow's sender frees its port, once its task has noticed.
    drop(first);
    let deadline = tokio::time::Instant::now() + WAIT;
    let (reused, _reused_rx) = loop {
        if let Ok(flow) = engine.udp_bind_guest_flow(GUEST_IP, 9999) {
            break flow;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the freed port never came back"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    reused.send(b"again".to_vec()).await.unwrap();
    assert_eq!(recv_udp(&mut rx).await.src.1, first_port);
}

/// A UDP forward maps each remote peer to its own gateway port. Peers that
/// go quiet must expire and give the port back, or enough distinct peers
/// would exhaust the range for good.
#[tokio::test]
async fn udp_port_forward_expires_idle_peers_and_reuses_their_port() {
    let (engine, mut rx) = engine();
    engine.learn_mac(GUEST_IP, GUEST_MAC);
    engine.limit_ephemeral_ports(1);
    let idle = Duration::from_millis(200);

    let fwd_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let fwd_addr = fwd_sock.local_addr().unwrap();
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            PortForwarder::serve_udp_with_idle(fwd_sock, engine, GUEST_IP, 53, idle).await;
        });
    }

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    a.connect(fwd_addr).await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    b.connect(fwd_addr).await.unwrap();

    a.send(b"from a").await.unwrap();
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.payload, b"from a");
    let port = u.src.1;

    // The only port is A's: B's datagram is dropped, and the forward lives on.
    b.send(b"too early").await.unwrap();
    assert!(
        timeout(idle / 2, rx.recv()).await.is_err(),
        "a peer without a port must not reach the guest"
    );

    // A goes quiet past the expiry; its port is B's now, both ways.
    tokio::time::sleep(idle * 3).await;
    b.send(b"from b").await.unwrap();
    let u = recv_udp(&mut rx).await;
    assert_eq!(u.payload, b"from b");
    assert_eq!(u.src.1, port);
    guest_udp(&engine, 53, u.src, b"to b").await;
    let mut buf = [0u8; 16];
    let n = timeout(WAIT, b.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"to b");
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

// ---------------------------------------------------------------------------
// Flow-table hygiene under churn
// ---------------------------------------------------------------------------

/// Many distinct guest flows fill the UDP table once each (repeat traffic
/// reuses the entry), and idle expiry drains it back to empty — the table
/// must not grow without bound under connection churn.
#[tokio::test]
async fn udp_flow_table_reuses_entries_and_expires_idle_ones() {
    use crate::sync::LockRecover;

    let (engine, _rx) = engine();
    // A local sink so every flow's connect/send succeeds.
    let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = (
        Ipv4Addr::new(127, 0, 0, 1),
        sink.local_addr().unwrap().port(),
    );

    const FLOWS: u16 = 32;
    for i in 0..FLOWS {
        guest_udp(&engine, 20_000 + i, dst, b"hello").await;
    }
    assert_eq!(engine.udp_flows.lock_recover().len(), usize::from(FLOWS));

    // Repeat traffic on existing flows must reuse, not duplicate.
    for i in 0..FLOWS {
        guest_udp(&engine, 20_000 + i, dst, b"again").await;
    }
    assert_eq!(engine.udp_flows.lock_recover().len(), usize::from(FLOWS));

    // Fast-forward past the idle window (flows track tokio time): every
    // reader task's timeout fires, sees the idle flow, and removes it.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        if engine.udp_flows.lock_recover().is_empty() {
            break;
        }
    }
    assert_eq!(
        engine.udp_flows.lock_recover().len(),
        0,
        "idle flows drained"
    );
}

// ---------------------------------------------------------------------------
// Worktree isolation (§21.4)
// ---------------------------------------------------------------------------

/// An engine under worktree isolation, with `host` as the host's interface
/// addresses and `exposed` as the lab's own host services.
fn guarded(
    pinger: Arc<dyn Pinger>,
    host: Vec<Ipv4Addr>,
    exposed: Vec<u16>,
) -> (Arc<NatEngine>, mpsc::Receiver<Bytes>) {
    let (engine, rx) = unguarded(pinger);
    let host: std::collections::HashSet<Ipv4Addr> = host.into_iter().collect();
    engine.set_guard(Arc::new(super::HostGuard::with_source(
        Arc::new(std::sync::Mutex::new(exposed)),
        Box::new(move || host.clone()),
    )));
    (engine, rx)
}

fn stub_pinger() -> Arc<StubPinger> {
    Arc::new(StubPinger {
        reachable: true,
        calls: AtomicUsize::new(0),
    })
}

/// A SYN to a listening loopback port is answered with RST, and the
/// listener never sees a connection.
#[tokio::test]
async fn isolation_resets_tcp_to_the_host() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dst = (Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port());
    let (engine, mut rx) = guarded(stub_pinger(), Vec::new(), Vec::new());

    guest_tcp(&engine, 40100, dst, 777, 0, TCP_SYN, b"", &[]).await;
    let t = recv_tcp(&mut rx).await;
    assert_ne!(t.flags & TCP_RST, 0, "refused SYN is reset");
    assert_eq!(t.ack, 778, "the reset acks the SYN");
    assert_eq!(t.src, dst);
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "the host listener was reached"
    );
}

/// The same refusal for a host interface address, not just loopback: the
/// path a worktree guest would take to the main lab's forward.
#[tokio::test]
async fn isolation_resets_tcp_to_a_host_interface_address() {
    let lan = Ipv4Addr::new(192, 0, 2, 44);
    let (engine, mut rx) = guarded(stub_pinger(), vec![lan], Vec::new());
    guest_tcp(&engine, 40101, (lan, 8080), 1, 0, TCP_SYN, b"", &[]).await;
    let t = recv_tcp(&mut rx).await;
    assert_ne!(t.flags & TCP_RST, 0);
    assert!(
        engine.tcp_flows.lock().unwrap().is_empty(),
        "no flow opened"
    );
}

/// The lab's own `smbd` port on 127.0.0.1 stays reachable.
#[tokio::test]
async fn isolation_admits_an_exposed_host_service() {
    let addr = tcp_echo_server().await;
    let dst = (Ipv4Addr::LOCALHOST, addr.port());
    let (engine, mut rx) = guarded(stub_pinger(), Vec::new(), vec![addr.port()]);
    guest_tcp(&engine, 40102, dst, 1000, 0, TCP_SYN, b"", &[]).await;
    let t = recv_tcp(&mut rx).await;
    assert_eq!(t.flags & (TCP_SYN | TCP_ACK), TCP_SYN | TCP_ACK, "{t:?}");
}

/// A datagram to the host is dropped: no flow, no reply.
#[tokio::test]
async fn isolation_drops_udp_to_the_host() {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = (Ipv4Addr::LOCALHOST, sock.local_addr().unwrap().port());
    let (engine, mut rx) = guarded(stub_pinger(), Vec::new(), Vec::new());
    guest_udp(&engine, 50100, dst, b"hello").await;
    let mut buf = [0u8; 16];
    assert!(
        timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
            .await
            .is_err(),
        "the datagram reached the host"
    );
    assert!(engine.udp_flows.lock().unwrap().is_empty());
    assert!(
        timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err()
    );
}

/// An echo to the host is dropped without a probe; one elsewhere is not.
#[tokio::test]
async fn isolation_drops_icmp_to_the_host() {
    let lan = Ipv4Addr::new(192, 0, 2, 45);
    let pinger = stub_pinger();
    let (engine, mut rx) = guarded(pinger.clone(), vec![lan], Vec::new());
    guest_icmp_echo(&engine, lan, 1, b"x").await;
    guest_icmp_echo(&engine, Ipv4Addr::LOCALHOST, 2, b"x").await;
    assert!(
        timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "no frame emitted"
    );
    assert_eq!(pinger.calls.load(Ordering::SeqCst), 0, "no probe ran");

    guest_icmp_echo(&engine, Ipv4Addr::new(192, 0, 2, 46), 3, b"x").await;
    recv_frame(&mut rx).await;
    assert_eq!(pinger.calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// The host's loopback outside isolation (networking review F5, card #460)
// ---------------------------------------------------------------------------

/// An engine nobody gave a guard still refuses the host's loopback: a SYN to
/// a listening `127.0.0.1` port is answered with RST and the listener
/// accepts nothing. A service bound to loopback is bound there to be
/// reachable from the host only.
#[tokio::test]
async fn nat_refuses_loopback_without_isolation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dst = (Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port());
    let (tx, mut rx) = mpsc::channel(256);
    let engine = NatEngine::new(NatConfig::new(GW_IP, GW_MAC), tx);

    guest_tcp(&engine, 40200, dst, 777, 0, TCP_SYN, b"", &[]).await;
    let t = recv_tcp(&mut rx).await;
    assert_ne!(t.flags & TCP_RST, 0, "the SYN is reset, got {t:?}");
    assert_eq!(t.ack, 778, "the reset acks the SYN");
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "the host's loopback listener was reached"
    );
    assert!(
        engine.tcp_flows.lock().unwrap().is_empty(),
        "no flow opened"
    );
}

/// The same for the rest of `127.0.0.0/8`, for `0.0.0.0`, and for UDP and
/// ICMP, which are dropped: no flow, no probe, no reply.
#[tokio::test]
async fn nat_refuses_every_loopback_address_and_protocol_without_isolation() {
    let pinger = stub_pinger();
    let (engine, mut rx) = unguarded(pinger.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // A connect to 0.0.0.0 reaches a listener on loopback.
    for (i, dst) in [Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(127, 0, 0, 2)]
        .into_iter()
        .enumerate()
    {
        guest_tcp(
            &engine,
            40210 + i as u16,
            (dst, port),
            1,
            0,
            TCP_SYN,
            b"",
            &[],
        )
        .await;
        let t = recv_tcp(&mut rx).await;
        assert_ne!(t.flags & TCP_RST, 0, "{dst}: {t:?}");
    }
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "the host's loopback listener was reached"
    );

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_port = sock.local_addr().unwrap().port();
    guest_udp(&engine, 50200, (Ipv4Addr::LOCALHOST, udp_port), b"hello").await;
    guest_udp(&engine, 50201, (Ipv4Addr::UNSPECIFIED, udp_port), b"hello").await;
    let mut buf = [0u8; 16];
    assert!(
        timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
            .await
            .is_err(),
        "the datagram reached the host"
    );
    assert!(engine.udp_flows.lock().unwrap().is_empty());

    guest_icmp_echo(&engine, Ipv4Addr::LOCALHOST, 1, b"x").await;
    assert!(
        timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "no frame emitted"
    );
    assert_eq!(pinger.calls.load(Ordering::SeqCst), 0, "no probe ran");
}

// ---------------------------------------------------------------------------
// Limits on what guests can make the engine hold (networking review F3)
// ---------------------------------------------------------------------------

/// The address of the `n`th flooding guest.
fn guest_n(n: u16) -> Ipv4Addr {
    Ipv4Addr::new(10, 213, 1 + (n >> 8) as u8, n as u8)
}

async fn udp_from(engine: &Arc<NatEngine>, guest: Ipv4Addr, sport: u16, dst: (Ipv4Addr, u16)) {
    let seg = frame::udp_build(guest, dst.0, sport, dst.1, b"x").unwrap();
    let pkt = frame::ipv4_build(guest, dst.0, IPPROTO_UDP, 64, &seg, 8).unwrap();
    let f = frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt);
    engine.handle_frame(Bytes::from(f)).await;
    // The gateway task awaits its queue between two frames.
    tokio::task::yield_now().await;
}

async fn syn_from(engine: &Arc<NatEngine>, guest: Ipv4Addr, sport: u16, dst: (Ipv4Addr, u16)) {
    let seg = frame::tcp_build(
        guest,
        dst.0,
        TcpFields {
            src_port: sport,
            dst_port: dst.1,
            seq: 1000,
            ack: 0,
            flags: TCP_SYN,
            window: 65535,
            options: &[],
        },
        &[],
    )
    .unwrap();
    let pkt = frame::ipv4_build(guest, dst.0, IPPROTO_TCP, 64, &seg, 7).unwrap();
    let f = frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt);
    engine.handle_frame(Bytes::from(f)).await;
}

async fn echo_from(engine: &Arc<NatEngine>, guest: Ipv4Addr, dst: Ipv4Addr) {
    let m = frame::icmp_build(ICMP_ECHO_REQUEST, 0, [0, 1, 0, 1], b"x");
    let pkt = frame::ipv4_build(guest, dst, IPPROTO_ICMP, 64, &m, 9).unwrap();
    let f = frame::eth_build(GW_MAC, GUEST_MAC, ETHERTYPE_IPV4, &pkt);
    engine.handle_frame(Bytes::from(f)).await;
}

fn udp_key(guest: Ipv4Addr, sport: u16, dst: (Ipv4Addr, u16)) -> super::FlowKey {
    super::FlowKey {
        guest_ip: guest,
        guest_port: sport,
        dst_ip: dst.0,
        dst_port: dst.1,
    }
}

/// One guest that sends 5 000 datagrams on 5 000 flows holds no more host
/// sockets than its share, the newest flow replaces the oldest, and no
/// number of guests takes the table past its size.
#[tokio::test]
async fn udp_flows_are_bounded() {
    use super::udp::{MAX_UDP_FLOWS, MAX_UDP_FLOWS_PER_GUEST};
    use crate::sync::LockRecover;

    let (engine, _rx) = engine();
    // A sink that never answers: every flow stays up for its idle time.
    let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = (
        Ipv4Addr::new(127, 0, 0, 1),
        sink.local_addr().unwrap().port(),
    );

    const FLOOD: u16 = 5_000;
    for i in 0..FLOOD {
        udp_from(&engine, GUEST_IP, 10_000 + i, dst).await;
    }
    {
        let table = engine.udp_flows.lock_recover();
        assert!(
            table.len() <= MAX_UDP_FLOWS_PER_GUEST,
            "one guest holds {} UDP flows, over its limit of {MAX_UDP_FLOWS_PER_GUEST}",
            table.len()
        );
        assert!(
            table.contains_key(&udp_key(GUEST_IP, 10_000 + FLOOD - 1, dst)),
            "the newest flow is served"
        );
        assert!(
            !table.contains_key(&udp_key(GUEST_IP, 10_000, dst)),
            "the oldest flow made room for it"
        );
    }

    // A second guest gets flows of its own beside the first one's.
    let other = guest_n(1);
    udp_from(&engine, other, 10_000, dst).await;
    assert!(
        engine
            .udp_flows
            .lock_recover()
            .contains_key(&udp_key(other, 10_000, dst))
    );

    // Forty guests, each inside its own share, do not pass the table's size.
    for n in 2..42 {
        for i in 0..125 {
            udp_from(&engine, guest_n(n), 10_000 + i, dst).await;
        }
    }
    let held = engine.udp_flows.lock_recover().len();
    assert!(
        held <= MAX_UDP_FLOWS,
        "the engine holds {held} UDP flows, over its limit of {MAX_UDP_FLOWS}"
    );
}

/// The same with SYNs: one guest holds no more connecting flows than its
/// share, all guests together no more than the table's size, and a flow
/// that ends gives its slot back.
#[tokio::test]
async fn tcp_flows_are_bounded() {
    use super::{MAX_TCP_FLOWS, MAX_TCP_FLOWS_PER_GUEST};
    use crate::sync::LockRecover;

    let (engine, mut rx) = engine();
    // Each failed connect answers its guest with a RST: read them away.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    // A loopback port nothing listens on.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let dst = (Ipv4Addr::new(127, 0, 0, 1), port);

    let mut peak = 0;
    for i in 0..5_000 {
        syn_from(&engine, GUEST_IP, 10_000 + i, dst).await;
        peak = peak.max(engine.tcp_flows.lock_recover().len());
    }
    assert!(
        peak <= MAX_TCP_FLOWS_PER_GUEST,
        "one guest held {peak} TCP flows, over its limit of {MAX_TCP_FLOWS_PER_GUEST}"
    );

    let mut peak = 0;
    for n in 1..41 {
        for i in 0..125 {
            syn_from(&engine, guest_n(n), 10_000 + i, dst).await;
            peak = peak.max(engine.tcp_flows.lock_recover().len());
        }
    }
    assert!(
        peak <= MAX_TCP_FLOWS,
        "the engine held {peak} TCP flows, over its limit of {MAX_TCP_FLOWS}"
    );

    // Every connect is refused, so every flow ends and frees its slot.
    timeout(WAIT, async {
        while !engine.tcp_flows.lock_recover().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the refused flows end");
    syn_from(&engine, GUEST_IP, 9_000, dst).await;
    assert_eq!(
        engine.tcp_flows.lock_recover().len(),
        1,
        "a guest that was at its limit opens a flow again"
    );
}

/// A pinger that never answers and records how many probes ran at once.
#[derive(Default)]
struct HoldingPinger {
    running: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait]
impl Pinger for HoldingPinger {
    async fn ping(&self, _dst: Ipv4Addr) -> bool {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        std::future::pending().await
    }
}

/// Echo requests to 5 000 addresses start no more `ping` calls at once than
/// the guest's share, and all guests together no more than the limit.
#[tokio::test]
async fn ping_probes_are_bounded() {
    use super::icmp::{MAX_PROBES, MAX_PROBES_PER_GUEST};

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    let pinger = Arc::new(HoldingPinger::default());
    let (engine, _rx) = engine_with_pinger(pinger.clone());

    for i in 0..5_000u16 {
        let dst = Ipv4Addr::new(198, 18, (i >> 8) as u8, i as u8);
        echo_from(&engine, GUEST_IP, dst).await;
    }
    settle().await;
    let peak = pinger.peak.load(Ordering::SeqCst);
    assert!(peak > 0, "a probe ran");
    assert!(
        peak <= MAX_PROBES_PER_GUEST,
        "one guest ran {peak} probes at once, over its limit of {MAX_PROBES_PER_GUEST}"
    );

    for n in 1..41 {
        for i in 0..100u16 {
            let dst = Ipv4Addr::new(198, 19, n as u8, i as u8);
            echo_from(&engine, guest_n(n), dst).await;
        }
    }
    settle().await;
    let peak = pinger.peak.load(Ordering::SeqCst);
    assert!(
        peak <= MAX_PROBES,
        "the engine ran {peak} probes at once, over its limit of {MAX_PROBES}"
    );
    assert!(
        peak > MAX_PROBES_PER_GUEST,
        "a second guest probes while the first is at its limit"
    );
}

// ---------------------------------------------------------------------------
// A stalled flow (review F4)
// ---------------------------------------------------------------------------

/// How long a call may take before the test says it is blocked. A call
/// behind a stalled flow never returned at all, so the length only has to
/// outlast a loaded test host.
const BLOCKED: Duration = Duration::from_secs(5);
/// A full segment at the 1460-byte MSS these tests negotiate.
const FULL_SEGMENT: usize = 1460;

/// Where a fake guest's frames go in: straight into the engine, or into a
/// switch port with a gateway behind it.
enum Wire {
    Engine(Arc<NatEngine>),
    Port(mpsc::Sender<Bytes>),
}

impl Wire {
    async fn send(&self, frame: Bytes) {
        match self {
            Wire::Engine(engine) => engine.handle_frame(frame).await,
            Wire::Port(tx) => tx.send(frame).await.expect("switch port closed"),
        }
    }
}

/// The guest's end of one flow it opened.
struct Upload {
    sport: u16,
    dst: (Ipv4Addr, u16),
    /// The sequence number of the first data byte.
    start: u32,
    /// The first byte the engine has not acknowledged.
    seq: u32,
    ack: u32,
}

impl Upload {
    /// The first `len` bytes of the stream the guest uploads: each byte
    /// follows from its place, so a lost, repeated or moved byte shows.
    fn stream(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) & 0xff) as u8).collect()
    }

    /// The full data segment that starts `ahead` segments past `seq`.
    fn segment(&self, ahead: u32) -> Bytes {
        let seq = self.seq.wrapping_add(ahead * FULL_SEGMENT as u32);
        let at = seq.wrapping_sub(self.start) as usize;
        let payload: Vec<u8> = (at..at + FULL_SEGMENT)
            .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
            .collect();
        tcp_frame(
            self.sport,
            self.dst,
            seq,
            self.ack,
            TCP_ACK | TCP_PSH,
            65535,
            &payload,
            &[],
        )
    }

    /// How many bytes the engine has acknowledged.
    fn acked(&self) -> usize {
        self.seq.wrapping_sub(self.start) as usize
    }
}

/// Next TCP frame of the flow to `dst`, skipping every other frame.
async fn recv_flow(rx: &mut mpsc::Receiver<Bytes>, dst: (Ipv4Addr, u16)) -> GTcp {
    loop {
        let t = recv_tcp(rx).await;
        if t.src == dst {
            return t;
        }
    }
}

/// Complete the guest's handshake for a flow to `dst`.
async fn open_upload(
    wire: &Wire,
    rx: &mut mpsc::Receiver<Bytes>,
    sport: u16,
    dst: (Ipv4Addr, u16),
) -> Upload {
    let isn = 50_000u32;
    let syn = tcp_frame(sport, dst, isn, 0, TCP_SYN, 65535, b"", &[2, 4, 0x05, 0xB4]);
    wire.send(syn).await;
    let synack = recv_flow(rx, dst).await;
    assert_eq!(synack.flags & (TCP_SYN | TCP_ACK), TCP_SYN | TCP_ACK);
    let up = Upload {
        sport,
        dst,
        start: isn.wrapping_add(1),
        seq: isn.wrapping_add(1),
        ack: synack.seq.wrapping_add(1),
    };
    wire.send(tcp_frame(sport, dst, up.seq, up.ack, TCP_ACK, 65535, b"", &[]))
        .await;
    up
}

/// Upload full segments in order, each after the acknowledgement of the one
/// before, until the engine stops taking them: it closes the guest's window,
/// or three seconds pass without an acknowledgement of the whole segment
/// (the engine before the fix, which never closed the window). Against a
/// host that does not read, that is the moment its socket buffers are full.
/// Leaves `up.seq` at the first byte the engine has not acknowledged, and
/// returns the last frame of the flow it saw.
async fn upload_until_stalled(
    wire: &Wire,
    rx: &mut mpsc::Receiver<Bytes>,
    up: &mut Upload,
) -> Option<GTcp> {
    let start = up.seq;
    let mut last = None;
    loop {
        assert!(
            up.seq.wrapping_sub(start) < 256 << 20,
            "a host that reads nothing took 256 MiB"
        );
        let end = up.seq.wrapping_add(FULL_SEGMENT as u32);
        wire.send(up.segment(0)).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while up.seq != end {
            match tokio::time::timeout_at(deadline, recv_flow(rx, up.dst)).await {
                Ok(t) => {
                    if seq_after(t.ack, up.seq) {
                        up.seq = t.ack;
                    }
                    let closed = t.window == 0;
                    last = Some(t);
                    if closed {
                        return last;
                    }
                }
                Err(_) => return last,
            }
        }
    }
}

/// A listener that accepts connections and never reads from them. The task
/// holds each accepted stream open for as long as the test runs.
async fn host_that_never_reads() -> (Ipv4Addr, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    (Ipv4Addr::LOCALHOST, port)
}

/// Review F4: a flow whose host end does not read must not hold up any
/// other frame. The guest fills the host's socket buffers, then sends 200
/// more segments: every `handle_frame` call returns, and a second flow
/// still opens. Before the fix the flow task sat in `write_all`, its
/// channel filled after 64 segments, and the next call never returned.
#[tokio::test]
async fn a_stalled_flow_does_not_block_other_frames() {
    let dst = host_that_never_reads().await;
    let (engine, mut rx) = engine();
    let wire = Wire::Engine(engine.clone());
    let mut up = open_upload(&wire, &mut rx, 40_100, dst).await;
    upload_until_stalled(&wire, &mut rx, &mut up).await;

    // The engine's answers are read all the while, so that only the host's
    // socket can be what a call waits for.
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if seen_tx.send(frame).is_err() {
                return;
            }
        }
    });

    for i in 0..200 {
        timeout(BLOCKED, engine.handle_frame(up.segment(i)))
            .await
            .unwrap_or_else(|_| {
                panic!("handle_frame is blocked behind the stalled flow at segment {i}")
            });
    }

    // Another flow of the same guest opens while the first one is stalled.
    let echo = tcp_echo_server().await;
    let other = (Ipv4Addr::LOCALHOST, echo.port());
    let syn = tcp_frame(40_101, other, 9_000, 0, TCP_SYN, 65535, b"", &[]);
    timeout(BLOCKED, engine.handle_frame(syn))
        .await
        .expect("a SYN is blocked behind the stalled flow");
    let opened = timeout(WAIT, async {
        loop {
            let frame = seen_rx.recv().await.expect("engine output closed");
            let eth = EthView::parse(&frame).unwrap();
            let ip = Ipv4View::parse(eth.payload()).unwrap();
            let tcp = TcpView::parse(ip.payload()).unwrap();
            if (ip.src(), tcp.src_port()) == other && tcp.flags() & TCP_SYN != 0 {
                return;
            }
        }
    })
    .await;
    assert!(opened.is_ok(), "no SYN-ACK for a second flow");
}

/// Review F4 at the gateway: with one flow stalled and 200 more of its
/// segments sent, the gateway still answers an ARP request for its own
/// address. Before the fix the gateway task waited in the uplink for the
/// flow's channel, and with it ARP, DHCP, DNS and every other flow.
#[tokio::test]
async fn a_stalled_flow_does_not_block_the_gateway() {
    use crate::net::gateway::{Gateway, GatewayConfig};
    use crate::net::switch::{PortClass, Switch};

    let dst = host_that_never_reads().await;
    let switch = Switch::new("seg".into());
    let gateway = Gateway::spawn(
        &switch,
        GatewayConfig {
            segment_name: "seg".into(),
            lab_name: "lab".into(),
            gw_ip: GW_IP,
            gw_mac: GW_MAC,
            dhcp: None,
            dns: None,
            upstream_dns: None,
        },
    );
    // The engine behind the gateway, wired as `spawn_nat` wires it.
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(1024);
    let engine = NatEngine::new(NatConfig::new(GW_IP, GW_MAC), out_tx);
    engine.set_guard(Arc::new(super::HostGuard::open()));
    let to_segment = gateway.sender();
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if to_segment.send(frame).await.is_err() {
                return;
            }
        }
    });
    gateway.set_uplink(Arc::new(move |frame: Bytes| {
        let engine = engine.clone();
        Box::pin(async move { engine.handle_frame(frame).await })
    }));

    let mut guest = switch.add_channel_port(PortClass::Guest { isolated: false });
    let wire = Wire::Port(guest.tx.clone());
    let mut up = open_upload(&wire, &mut guest.rx, 40_110, dst).await;
    upload_until_stalled(&wire, &mut guest.rx, &mut up).await;
    for i in 0..200 {
        wire.send(up.segment(i)).await;
    }
    // Let the switch hand all of them to the gateway before the request.
    tokio::time::sleep(Duration::from_millis(200)).await;

    wire.send(Bytes::from(frame::arp_request_build(
        GUEST_MAC, GUEST_IP, GW_IP,
    )))
    .await;
    let answered = timeout(BLOCKED, async {
        loop {
            let frame = guest.rx.recv().await.expect("guest port closed");
            let eth = EthView::parse(&frame).unwrap();
            if eth.ethertype() == frame::ETHERTYPE_ARP && eth.src_mac() == GW_MAC {
                return;
            }
        }
    })
    .await;
    assert!(
        answered.is_ok(),
        "the gateway answers no ARP request while one flow is stalled"
    );
}

/// A listener that accepts one connection, reads nothing until it is told
/// to, and then reads to the end. Returns its address, the sender that
/// tells it, and the task that yields what it read.
async fn host_that_reads_when_told() -> (
    (Ipv4Addr, u16),
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<u8>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tell, told) = tokio::sync::oneshot::channel();
    let sink = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = told.await;
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        received
    });
    ((Ipv4Addr::LOCALHOST, port), tell, sink)
}

/// A host that does not read closes the guest's window instead of blocking
/// anything, and the flow goes on when the host reads again: the window
/// reopens without a probe from the guest, and the host gets every byte the
/// engine acknowledged, in order, and then the guest's FIN.
#[tokio::test]
async fn a_stalled_flow_closes_the_window_and_resumes_when_the_host_reads() {
    let (dst, tell, sink) = host_that_reads_when_told().await;
    let (engine, mut rx) = engine();
    let wire = Wire::Engine(engine.clone());
    let mut up = open_upload(&wire, &mut rx, 40_120, dst).await;
    let last = upload_until_stalled(&wire, &mut rx, &mut up)
        .await
        .expect("the engine answered the upload");
    assert_eq!(last.window, 0, "a full buffer is a closed window");
    assert_eq!(last.ack, up.seq);

    tell.send(()).unwrap();
    let total = up.acked() + 200 * FULL_SEGMENT;
    let mut window = last.window;
    while up.acked() < total {
        // The guest keeps to the window it is told, and sends no probe.
        while usize::from(window) < FULL_SEGMENT {
            window = recv_flow(&mut rx, dst).await.window;
        }
        let end = up.seq.wrapping_add(FULL_SEGMENT as u32);
        wire.send(up.segment(0)).await;
        while up.seq != end {
            let t = recv_flow(&mut rx, dst).await;
            if seq_after(t.ack, up.seq) {
                up.seq = t.ack;
            }
            window = t.window;
        }
    }

    let fin = tcp_frame(up.sport, dst, up.seq, up.ack, TCP_ACK | TCP_FIN, 65535, b"", &[]);
    wire.send(fin).await;
    let received = timeout(WAIT, sink)
        .await
        .expect("the host never saw the end of the upload")
        .unwrap();
    assert_eq!(received.len(), up.acked());
    assert!(received == Upload::stream(up.acked()), "the upload arrived changed");
}

/// The guest's FIN behind buffered data: the host gets the data first and
/// the end of the stream after it, not a stream cut short.
#[tokio::test]
async fn a_fin_behind_a_stalled_flow_reaches_the_host_after_the_data() {
    let (dst, tell, sink) = host_that_reads_when_told().await;
    let (engine, mut rx) = engine();
    let wire = Wire::Engine(engine.clone());
    let mut up = open_upload(&wire, &mut rx, 40_130, dst).await;
    upload_until_stalled(&wire, &mut rx, &mut up).await;

    let fin = tcp_frame(up.sport, dst, up.seq, up.ack, TCP_ACK | TCP_FIN, 65535, b"", &[]);
    wire.send(fin).await;
    loop {
        let t = recv_flow(&mut rx, dst).await;
        if t.ack == up.seq.wrapping_add(1) {
            break;
        }
    }

    tell.send(()).unwrap();
    let received = timeout(WAIT, sink)
        .await
        .expect("the host never saw the end of the upload")
        .unwrap();
    assert_eq!(received.len(), up.acked());
    assert!(received == Upload::stream(up.acked()), "the upload arrived changed");
}
