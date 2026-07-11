//! Privileged fast-path integration tests: they exercise the real kernel
//! mechanisms, so they need CAP_BPF + CAP_NET_ADMIN and are `#[ignore]`d
//! for the normal suite. Run them with `just fastpath-test`, which invokes
//! this binary under sudo twice — once per tier — because the tier is a
//! per-process singleton:
//!
//! ```text
//! VMLAB_FASTPATH=sockmap <test-bin> fastpath_sockmap --ignored
//! VMLAB_FASTPATH=afxdp   <test-bin> fastpath_afxdp   --ignored
//! ```
//!
//! Each test still guards on the selected tier and skips gracefully, so an
//! unprivileged `--ignored` run reports skips instead of failures.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::{FastpathMode, FastpathTier, init};
use crate::config::model::MacAddr;
use crate::net::frame::{ETHERTYPE_IPV4, MAC_BROADCAST, eth_build};
use crate::net::framing::read_frame;
use crate::net::switch::{HookAction, PortClass, Switch};

const MAC_A: MacAddr = MacAddr([0x52, 0x54, 0xee, 0x00, 0x00, 0x0a]);
const MAC_B: MacAddr = MacAddr([0x52, 0x54, 0xee, 0x00, 0x00, 0x0b]);
const MAC_C: MacAddr = MacAddr([0x52, 0x54, 0xee, 0x00, 0x00, 0x0c]);
const MAC_SVC: MacAddr = MacAddr([0x52, 0x54, 0xee, 0x00, 0x00, 0x99]);

fn tier_is(want: FastpathTier) -> bool {
    let got = init(FastpathMode::Auto);
    if got != want {
        eprintln!(
            "SKIPPING: fast-path tier is {} (want {}) — probe status: {}",
            got.as_str(),
            want.as_str(),
            super::status_json(),
        );
        return false;
    }
    true
}

/// A fake QEMU: the daemon half is a switch stream port, ours speaks the
/// length-prefixed framing.
async fn guest_port(
    sw: &std::sync::Arc<Switch>,
    isolated: bool,
) -> (
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
) {
    let (qemu, daemon) = UnixStream::pair().unwrap();
    sw.add_stream_port(daemon, PortClass::Guest { isolated })
        .await;
    qemu.into_split()
}

async fn recv_frame(read: &mut tokio::net::unix::OwnedReadHalf) -> Bytes {
    timeout(Duration::from_secs(2), read_frame(read))
        .await
        .expect("timed out waiting for frame")
        .expect("read failed")
        .expect("unexpected EOF")
}

/// Send one frame the way QEMU's stream netdev does: prefix + payload in a
/// single write (one skb). `framing::write_frame`'s two writes would arrive
/// as two skbs, which the verdict rightly passes to userspace — the tests
/// must produce the aligned single-skb traffic the fast path targets.
async fn send_qemu_framed(write: &mut tokio::net::unix::OwnedWriteHalf, frame: &[u8]) {
    use tokio::io::AsyncWriteExt;
    let mut buf = (frame.len() as u32).to_be_bytes().to_vec();
    buf.extend_from_slice(frame);
    write.write_all(&buf).await.unwrap();
}

/// Teach the switch (and through it the kernel MAC map) a port's MAC.
async fn announce(write: &mut tokio::net::unix::OwnedWriteHalf, mac: MacAddr) {
    let f = eth_build(MAC_BROADCAST, mac, ETHERTYPE_IPV4, b"announce");
    send_qemu_framed(write, &f).await;
    // Give the learn + kernel mirror a moment to land.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
#[ignore = "needs CAP_BPF + CAP_NET_ADMIN — run via `just fastpath-test`"]
async fn fastpath_sockmap_forwards_in_kernel() {
    if !tier_is(FastpathTier::Sockmap) {
        return;
    }
    let sw = Switch::new("fp-sockmap-forward".into());
    let (mut a_read, mut a_write) = guest_port(&sw, false).await;
    let (mut b_read, mut b_write) = guest_port(&sw, false).await;
    announce(&mut a_write, MAC_A).await;
    announce(&mut b_write, MAC_B).await;
    // Drain the announce floods.
    recv_frame(&mut a_read).await;
    recv_frame(&mut b_read).await;

    // Known unicast splices in-kernel: delivered, and counted as offloaded.
    for i in 0..100u32 {
        let f = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, &i.to_be_bytes());
        send_qemu_framed(&mut a_write, &f).await;
        assert_eq!(recv_frame(&mut b_read).await, Bytes::from(f));
    }
    let stats = sw.stats();
    assert!(
        stats.frames_offloaded >= 99,
        "expected kernel-spliced frames, got {stats:?}"
    );

    // And the reply direction too.
    let f = eth_build(MAC_A, MAC_B, ETHERTYPE_IPV4, b"reply");
    send_qemu_framed(&mut b_write, &f).await;
    assert_eq!(recv_frame(&mut a_read).await, Bytes::from(f));
}

#[tokio::test]
#[ignore = "needs CAP_BPF + CAP_NET_ADMIN — run via `just fastpath-test`"]
async fn fastpath_sockmap_gateway_frames_reach_hook_and_service() {
    if !tier_is(FastpathTier::Sockmap) {
        return;
    }
    let sw = Switch::new("fp-sockmap-hook".into());
    let hook_hits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hits = hook_hits.clone();
    sw.set_ingress_hook(Box::new(move |_, _, frame| {
        if frame.len() >= 6 && frame[0..6] == MAC_SVC.0 {
            hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        HookAction::Pass
    }));
    let (mut a_read, mut a_write) = guest_port(&sw, false).await;
    let mut svc = sw.add_channel_port(PortClass::Service);
    announce(&mut a_write, MAC_A).await;
    // Teach the switch the service MAC (like the gateway's ARP would).
    svc.tx
        .send(Bytes::from(eth_build(
            MAC_BROADCAST,
            MAC_SVC,
            ETHERTYPE_IPV4,
            b"svc",
        )))
        .await
        .unwrap();
    recv_frame(&mut a_read).await;

    // Gateway-addressed traffic is never in the kernel MAC map, so it must
    // pass to userspace, traverse the hook, and reach the service port —
    // exactly the L3-rules contract.
    let f = eth_build(MAC_SVC, MAC_A, ETHERTYPE_IPV4, b"to gateway");
    send_qemu_framed(&mut a_write, &f).await;
    let got = timeout(Duration::from_secs(2), svc.rx.recv())
        .await
        .expect("timed out")
        .expect("service port closed");
    assert_eq!(got, Bytes::from(f));
    assert_eq!(hook_hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(sw.stats().frames_offloaded, 0);
}

#[tokio::test]
#[ignore = "needs CAP_BPF + CAP_NET_ADMIN — run via `just fastpath-test`"]
async fn fastpath_sockmap_isolated_ports_stay_userspace() {
    if !tier_is(FastpathTier::Sockmap) {
        return;
    }
    let sw = Switch::new("fp-sockmap-isolated".into());
    let (_c_read, mut c_write) = guest_port(&sw, true).await;
    let (mut b_read, mut b_write) = guest_port(&sw, false).await;
    announce(&mut c_write, MAC_C).await;
    announce(&mut b_write, MAC_B).await;

    // Isolated guest → guest is blocked by the userspace matrix, and the
    // kernel never learned the isolated port, so nothing can splice around
    // that rule.
    let f = eth_build(MAC_B, MAC_C, ETHERTYPE_IPV4, b"must not arrive");
    send_qemu_framed(&mut c_write, &f).await;
    let got = timeout(Duration::from_millis(300), read_frame(&mut b_read)).await;
    assert!(got.is_err(), "isolated frame leaked: {got:?}");
    assert_eq!(sw.stats().frames_offloaded, 0);
}

#[tokio::test]
#[ignore = "needs CAP_BPF + CAP_NET_ADMIN — run via `just fastpath-test`"]
async fn fastpath_sockmap_port_close_cleans_up() {
    if !tier_is(FastpathTier::Sockmap) {
        return;
    }
    let sw = Switch::new("fp-sockmap-close".into());
    let (mut a_read, mut a_write) = guest_port(&sw, false).await;
    let (mut b_read, mut b_write) = guest_port(&sw, false).await;
    announce(&mut a_write, MAC_A).await;
    announce(&mut b_write, MAC_B).await;
    recv_frame(&mut a_read).await;
    recv_frame(&mut b_read).await;
    let f = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, b"pre-close");
    send_qemu_framed(&mut a_write, &f).await;
    recv_frame(&mut b_read).await;

    // Close "QEMU B": the port and its kernel state must go away, so
    // traffic to MAC_B becomes an unknown-unicast flood in userspace
    // (reaching a later port) instead of a redirect into a dead socket.
    drop(b_read);
    drop(b_write);
    for _ in 0..100 {
        if sw.stats().ports == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sw.stats().ports, 1, "port not removed on EOF");

    let (mut d_read, _d_write) = guest_port(&sw, false).await;
    let f = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, b"post-close flood");
    send_qemu_framed(&mut a_write, &f).await;
    assert_eq!(recv_frame(&mut d_read).await, Bytes::from(f));
}

/// The single-writer regression test: kernel splices and daemon egress
/// (through the TX loopback) hammer the same QEMU socket concurrently; the
/// byte stream must stay a perfect concatenation of intact frames.
#[tokio::test]
#[ignore = "needs CAP_BPF + CAP_NET_ADMIN — run via `just fastpath-test`"]
async fn fastpath_sockmap_single_writer_integrity() {
    if !tier_is(FastpathTier::Sockmap) {
        return;
    }
    const PER_SOURCE: u64 = 20_000;
    let sw = Switch::new("fp-sockmap-integrity".into());
    let (mut b_read, mut b_write) = guest_port(&sw, false).await;
    let (mut a_read, mut a_write) = guest_port(&sw, false).await;
    let svc = sw.add_channel_port(PortClass::Service);
    announce(&mut a_write, MAC_A).await;
    announce(&mut b_write, MAC_B).await;
    recv_frame(&mut a_read).await;
    recv_frame(&mut b_read).await;
    // Drain B's copy of A/svc announces in the collector below instead.

    // Collector: parse B's stream; every frame must be whole and carry one
    // of the two source markers.
    let collector = tokio::spawn(async move {
        let (mut from_kernel, mut from_service) = (0u64, 0u64);
        loop {
            match timeout(Duration::from_secs(5), read_frame(&mut b_read)).await {
                Ok(Ok(Some(frame))) => {
                    assert!(frame.len() >= 22, "runt frame in stream");
                    match &frame[14..16] {
                        b"K:" => from_kernel += 1,
                        b"S:" => from_service += 1,
                        _ => {} // announce floods
                    }
                    if from_kernel == PER_SOURCE && from_service == PER_SOURCE {
                        return (from_kernel, from_service);
                    }
                }
                Ok(other) => panic!("stream broke mid-test: {other:?}"),
                Err(_) => return (from_kernel, from_service),
            }
        }
    });

    // Kernel path: guest A floods unicast at B.
    let kernel_writer = tokio::spawn(async move {
        for i in 0..PER_SOURCE {
            let mut payload = b"K:".to_vec();
            payload.extend_from_slice(&i.to_be_bytes());
            payload.resize(64, 0);
            let f = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, &payload);
            send_qemu_framed(&mut a_write, &f).await;
        }
        a_write
    });
    // Userspace path: a service port unicasts at B through the TX loopback.
    // Pace it just enough that B's 512-slot egress queue doesn't overflow
    // (drops are legal ethernet but would make the count assertion moot).
    let svc_tx = svc.tx.clone();
    let service_writer = tokio::spawn(async move {
        for i in 0..PER_SOURCE {
            let mut payload = b"S:".to_vec();
            payload.extend_from_slice(&i.to_be_bytes());
            payload.resize(64, 0);
            let f = eth_build(MAC_B, MAC_SVC, ETHERTYPE_IPV4, &payload);
            svc_tx.send(Bytes::from(f)).await.unwrap();
            if i % 128 == 0 {
                tokio::task::yield_now().await;
            }
        }
    });

    let _a_write = kernel_writer.await.unwrap();
    service_writer.await.unwrap();
    let (from_kernel, from_service) = collector.await.unwrap();
    assert_eq!(from_kernel, PER_SOURCE, "kernel-path frames lost/corrupted");
    assert!(
        from_service >= PER_SOURCE * 9 / 10,
        "service-path frames lost: {from_service}/{PER_SOURCE}"
    );
    let _ = svc;
}

#[tokio::test]
#[ignore = "needs CAP_BPF + CAP_NET_ADMIN — run via `just fastpath-test`"]
async fn fastpath_afxdp_switch_semantics_hold() {
    if !tier_is(FastpathTier::AfXdp) {
        return;
    }
    use super::afxdp::SegmentXdp;
    let sw = Switch::new("fp-afxdp-semantics".into());
    let hook_hits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hits = hook_hits.clone();
    sw.set_ingress_hook(Box::new(move |_, _, frame| {
        if frame.len() >= 6 && frame[0..6] == MAC_SVC.0 {
            hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        HookAction::Pass
    }));

    let seg = SegmentXdp::new("fp-afxdp-semantics", 1500).unwrap();
    let nic_a = seg.add_nic(&sw, MAC_A, false).unwrap();
    let nic_b = seg.add_nic(&sw, MAC_B, false).unwrap();
    let nic_c = seg.add_nic(&sw, MAC_C, true).unwrap(); // isolated
    let a = nic_a.io().unwrap();
    let b = nic_b.io().unwrap();
    let c = nic_c.io().unwrap();
    let mut svc = sw.add_channel_port(PortClass::Service);

    // Known unicast forwards tap-to-tap in-kernel.
    let f = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, b"in-kernel");
    a.send(&f).await.unwrap();
    recv_tap_expect(&b, &f).await;
    assert!(
        seg.stats()[0] >= 1,
        "no in-kernel forwards: {:?}",
        seg.stats()
    );

    // Service-addressed traffic punts, traverses the hook, reaches the port.
    svc.tx
        .send(Bytes::from(eth_build(
            MAC_BROADCAST,
            MAC_SVC,
            ETHERTYPE_IPV4,
            b"svc",
        )))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let f = eth_build(MAC_SVC, MAC_A, ETHERTYPE_IPV4, b"to service");
    a.send(&f).await.unwrap();
    loop {
        let got = timeout(Duration::from_secs(2), svc.rx.recv())
            .await
            .expect("timed out waiting at service port")
            .expect("service port closed");
        if got == f.clone() {
            break;
        }
    }
    assert!(hook_hits.load(std::sync::atomic::Ordering::Relaxed) >= 1);

    // Isolated tap → guest is punted and then blocked by the switch matrix
    // (its MAC is never in the kernel table, so no in-kernel bypass).
    let before = seg.stats()[0];
    let f = eth_build(MAC_B, MAC_C, ETHERTYPE_IPV4, b"must not arrive");
    c.send(&f).await.unwrap();
    let leaked = recv_tap_matches(&b, &f, Duration::from_millis(300)).await;
    assert!(!leaked, "isolated frame leaked through the fast path");
    assert_eq!(
        seg.stats()[0],
        before,
        "isolated frame was kernel-forwarded"
    );
}

async fn recv_tap_expect(io: &super::afxdp::TapIo, want: &[u8]) {
    assert!(
        recv_tap_matches(io, want, Duration::from_secs(2)).await,
        "expected frame never arrived"
    );
}

/// Read a tap until `want` arrives (true) or the timeout passes (false),
/// skipping unrelated host-stack chatter.
async fn recv_tap_matches(io: &super::afxdp::TapIo, want: &[u8], wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match tokio::time::timeout_at(deadline, io.recv(&mut buf)).await {
            Ok(Ok(n)) if &buf[..n] == want => return true,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("tap read failed: {e}"),
            Err(_) => return false,
        }
    }
}

/// A/B throughput smoke (`just fastpath-bench`): floods unicast frames
/// between two stream ports for a fixed window and reports the *delivered*
/// rate. Time-boxed rather than frame-counted on purpose: the userspace
/// switch drops on a full egress queue (legal ethernet), so waiting for an
/// exact frame count would never finish under flood — drops just lower the
/// reported rate instead. Run once with `VMLAB_FASTPATH=off` and once with
/// `=sockmap` to compare.
#[tokio::test]
#[ignore = "benchmark — run via `just fastpath-bench`"]
async fn fastpath_bench_ab() {
    const WINDOW: Duration = Duration::from_secs(5);
    const SIZE: usize = 1400;
    let tier = init(FastpathMode::Auto);
    // Make a degraded run diagnosable: the whole point is comparing tiers.
    eprintln!("probe status: {}", super::status_json());
    let sw = Switch::new("fp-bench".into());
    let (mut a_read, mut a_write) = guest_port(&sw, false).await;
    let (mut b_read, mut b_write) = guest_port(&sw, false).await;
    announce(&mut a_write, MAC_A).await;
    announce(&mut b_write, MAC_B).await;
    recv_frame(&mut a_read).await;
    recv_frame(&mut b_read).await;

    // Reader: count until the stream goes idle after the writer stops.
    let reader = tokio::spawn(async move {
        let mut n = 0u64;
        loop {
            match timeout(Duration::from_secs(2), read_frame(&mut b_read)).await {
                Ok(Ok(Some(_))) => n += 1,
                _ => return n, // idle, EOF, or error: the window is over
            }
        }
    });

    let frame = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, &vec![0xBE; SIZE - 14]);
    let start = Instant::now();
    let mut sent = 0u64;
    while start.elapsed() < WINDOW {
        send_qemu_framed(&mut a_write, &frame).await;
        sent += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    drop(a_write); // EOF ends the port; the reader exits on idle

    let received = timeout(Duration::from_secs(30), reader)
        .await
        .expect("reader wedged")
        .unwrap();
    let stats = sw.stats();
    println!(
        "tier={} size={SIZE} window={secs:.2}s sent={sent} delivered={received} \
         rate={:.0} frames/s ({:.1} MiB/s) dropped={} offloaded={}",
        tier.as_str(),
        received as f64 / secs,
        received as f64 * SIZE as f64 / secs / (1024.0 * 1024.0),
        sent - received,
        stats.frames_offloaded,
    );
}
