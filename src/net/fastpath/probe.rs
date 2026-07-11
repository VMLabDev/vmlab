//! Empirical fast-path probes, run once per daemon by [`super::init`].
//!
//! Nothing here inspects kernel versions or capability bits: each probe
//! drives the real mechanism end-to-end over throwaway sockets and either
//! proves it works on this host or reports why it doesn't. That's what
//! keeps the PRD's rootless guarantee intact with zero special-casing —
//! on an unprivileged daemon the first BPF syscall fails with EPERM and
//! the tier quietly stays userspace.

use std::io::{Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::sockmap::Engine;
use crate::config::model::MacAddr;
use crate::net::frame::{ETHERTYPE_IPV4, MAC_BROADCAST, eth_build};

const MAC_A: MacAddr = MacAddr([0x52, 0x54, 0xfa, 0x57, 0x00, 0x0a]);
const MAC_B: MacAddr = MacAddr([0x52, 0x54, 0xfa, 0x57, 0x00, 0x0b]);
const MAC_UNKNOWN: MacAddr = MacAddr([0x52, 0x54, 0xfa, 0x57, 0x00, 0xff]);
const TIMEOUT: Duration = Duration::from_millis(500);

/// Decorate permission failures with the one actionable fix.
fn hint(msg: String) -> String {
    if msg.contains("EPERM") || msg.contains("Operation not permitted") {
        format!("{msg} — run the vmlab daemons with CAP_BPF + CAP_NET_ADMIN to enable")
    } else {
        msg
    }
}

/// Validate the sockmap tier: load + attach the programs, then prove the
/// three load-bearing behaviours on unix socketpairs standing in for QEMU
/// connections — egress redirect (guest→guest splice), `SK_PASS` fall-
/// through (unknown destinations must reach the daemon), and the dgram
/// TX-loopback path (daemon egress). Any kernel where one of these is
/// missing or broken fails closed.
pub(super) fn sockmap() -> Result<(), String> {
    sockmap_impl().map_err(|e| hint(format!("{e:#}")))
}

fn sockmap_impl() -> Result<()> {
    let engine = Engine::load()?;

    // Two "QEMU connections": the daemon-side halves go into the sockmap.
    let (qemu_a, daemon_a) = UnixStream::pair().context("socketpair")?;
    let (qemu_b, daemon_b) = UnixStream::pair().context("socketpair")?;
    engine
        .register_stream(1, &daemon_a)
        .context("adding a unix stream socket to the sockmap")?;
    engine
        .register_stream(2, &daemon_b)
        .context("adding a unix stream socket to the sockmap")?;
    engine.insert_mac(MAC_A.0, 1)?;
    engine.insert_mac(MAC_B.0, 2)?;

    // 1. Known unicast A→B must splice in-kernel: written into QEMU A's
    //    side, readable on QEMU B's side (validates *egress* redirect on
    //    af_unix, the historically shakiest piece). On failure, work out
    //    where the frame went — it pinpoints whether the verdict passed it
    //    (program/map problem) or the kernel accepted the redirect and then
    //    dropped it (af_unix egress redirect unsupported on this kernel).
    let spliced = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, b"fastpath probe: splice");
    send_framed(&qemu_a, &spliced)?;
    let got = match recv_framed(&qemu_b) {
        Ok(got) => got,
        Err(e) => {
            let (frames, _) = engine.stats();
            let disposition = if recv_framed(&daemon_a).is_ok() {
                "the verdict SK_PASSed it to the daemon instead of redirecting"
            } else if frames > 0 {
                "the verdict redirected it but the kernel dropped it — af_unix \
                 egress redirect appears unsupported on this kernel"
            } else {
                "it vanished before the redirect counter — likely dropped by \
                 the verdict/sockmap layer"
            };
            return Err(e.context(format!(
                "kernel splice of known unicast (egress redirect); {disposition}"
            )));
        }
    };
    if got != spliced {
        bail!("kernel splice corrupted the frame");
    }
    let (frames, bytes) = engine.stats();
    if frames != 1 || bytes == 0 {
        bail!("fast-path counters not accounting (frames={frames}, bytes={bytes})");
    }

    // 2. Unknown destination must SK_PASS to the daemon side untouched.
    let passed = eth_build(MAC_UNKNOWN, MAC_A, ETHERTYPE_IPV4, b"fastpath probe: pass");
    send_framed(&qemu_a, &passed)?;
    let got = recv_framed(&daemon_a).context("SK_PASS of unknown destination")?;
    if got != passed {
        bail!("SK_PASS altered the byte stream");
    }

    // 3. Daemon egress through the TX loopback: one dgram in, spliced out
    //    of QEMU B's socket.
    let (tx_user, tx_kernel) = UnixDatagram::pair().context("dgram pair")?;
    engine
        .register_tx(2, &tx_kernel)
        .context("adding a unix dgram socket to the sockmap")?;
    let injected = eth_build(MAC_B, MAC_UNKNOWN, ETHERTYPE_IPV4, b"fastpath probe: tx");
    let mut buf = (injected.len() as u32).to_be_bytes().to_vec();
    buf.extend_from_slice(&injected);
    tx_user.send(&buf).context("tx loopback send")?;
    let got = recv_framed(&qemu_b).context("TX-loopback splice (daemon egress)")?;
    if got != injected {
        bail!("TX-loopback splice corrupted the frame");
    }

    Ok(())
}

/// Validate the afxdp tier by driving the real datapath end-to-end on
/// throwaway resources: a [`SegmentXdp`] with two probe NICs on a real
/// switch, exercising the in-kernel tap-to-tap forward (standard + jumbo
/// frames) and the full punt→switch→inject round trip. Runs on its own
/// thread + single-thread runtime so it works from any caller (the daemons
/// call [`super::init`] inside a runtime; unit tests call it without one).
pub(super) fn afxdp() -> Result<(), String> {
    let handle = std::thread::Builder::new()
        .name("fastpath-probe".into())
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("probe runtime: {e}"))?;
            rt.block_on(async { afxdp_impl().await.map_err(|e| hint(format!("{e:#}"))) })
        })
        .map_err(|e| format!("probe thread: {e}"))?;
    handle
        .join()
        .map_err(|_| "probe thread panicked".to_string())?
}

async fn afxdp_impl() -> Result<()> {
    use super::afxdp::SegmentXdp;
    use crate::net::switch::Switch;

    // Jumbo-capable taps so one probe covers standard and jumbo frames.
    const MTU: u16 = 9000;
    // The probe switch has no offload of its own: the tier is still
    // undecided while we run, so `fastpath::tier()` reads Userspace.
    let switch = Switch::new("fastpath-probe".into());
    let seg = SegmentXdp::new("fastpath-probe", MTU)?;
    let nic_a = seg.add_nic(&switch, MAC_A, false)?;
    let nic_b = seg.add_nic(&switch, MAC_B, false)?;
    let a = nic_a.io()?;
    let b = nic_b.io()?;

    // 1. Known unicast A→B must forward tap-to-tap in-kernel.
    let fwd = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, &[0xA5; 1400]);
    a.send(&fwd).await.context("tap send")?;
    recv_expect(&b, &fwd)
        .await
        .context("in-kernel tap-to-tap forward")?;
    if seg.stats()[0] == 0 {
        anyhow::bail!("forward counter not accounting");
    }

    // 2. A jumbo frame takes the same path (generic XDP must not bypass it).
    let jumbo = eth_build(MAC_B, MAC_A, ETHERTYPE_IPV4, &[0x5A; 8900]);
    a.send(&jumbo).await.context("tap send (jumbo)")?;
    recv_expect(&b, &jumbo)
        .await
        .context("in-kernel forward of a jumbo frame")?;

    // 3. Broadcast must punt to the daemon, flood through the userspace
    //    switch, and inject back out B's tap — the whole bridge round trip.
    let bcast = eth_build(
        MAC_BROADCAST,
        MAC_A,
        ETHERTYPE_IPV4,
        b"fastpath probe: punt",
    );
    a.send(&bcast).await.context("tap send (broadcast)")?;
    recv_expect(&b, &bcast)
        .await
        .context("punt/inject round trip via the userspace switch")?;

    Ok(())
}

/// Read frames off a tap until the expected one arrives (the host stack may
/// emit its own chatter — IPv6 ND and friends — out any UP tap).
async fn recv_expect(io: &super::afxdp::TapIo, want: &[u8]) -> Result<()> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = tokio::time::timeout_at(deadline, io.recv(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("no frame arrived within {TIMEOUT:?}"))?
            .context("tap read")?;
        if &buf[..n] == want {
            return Ok(());
        }
    }
}

fn send_framed(sock: &UnixStream, frame: &[u8]) -> Result<()> {
    let mut s = sock;
    s.write_all(&(frame.len() as u32).to_be_bytes())?;
    s.write_all(frame)?;
    Ok(())
}

fn recv_framed(sock: &UnixStream) -> Result<Vec<u8>> {
    sock.set_read_timeout(Some(TIMEOUT))?;
    let mut s = sock;
    let mut prefix = [0u8; 4];
    s.read_exact(&mut prefix)
        .context("no frame arrived within the probe timeout")?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len > crate::net::framing::MAX_FRAME_LEN {
        bail!("implausible frame length {len}");
    }
    let mut frame = vec![0u8; len];
    s.read_exact(&mut frame).context("truncated frame")?;
    Ok(frame)
}
