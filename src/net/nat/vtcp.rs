//! vTCP — the minimal in-process TCP peer that terminates guest flows.
//!
//! One tokio task per flow owns all state; [`super::NatEngine::handle_frame`]
//! only routes parsed segments into the task's channel. Both directions are
//! supported by the same core:
//!
//! - **passive** (guest→world NAT): guest SYN → host `TcpStream::connect`
//!   to the original destination → SYN-ACK → proxy.
//! - **active** ([`super::NatEngine::open_tcp_to_guest`]): the engine sends
//!   the SYN from `gw_ip:ephemeral` and hands the caller a [`GuestStream`]
//!   (one end of an in-memory duplex pipe; the flow task drives the other).
//!
//! State machine (per flow task):
//!
//! ```text
//! passive:  CONNECTING → SYN_RECEIVED → ESTABLISHED → (FIN exchange) → gone
//! active:   SYN_SENT   ───────────────↗
//! ```
//!
//! Simplifications, all backed by the PRD's performance non-goal: in-order
//! acceptance only (out-of-order segments are dropped and dup-ACKed; the
//! peer retransmits), go-back-N retransmission of the first unacked segment
//! on a fixed 1 s RTO with at most 5 tries, no TIME_WAIT, no window
//! scaling, no SACK, no zero-window probes (a flow stuck on a closed guest
//! window falls to the 5-minute idle reset), RST on anything unexpected.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};
use tracing::{debug, trace};

use crate::config::model::MacAddr;
use crate::net::frame::{
    self, IPPROTO_TCP, Ipv4View, TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN, TcpFields, TcpView,
};

use super::NatEngine;

/// Retransmission timeout (fixed; no RTT estimation).
const RTO: Duration = Duration::from_secs(1);
/// Retransmissions before the flow is reset.
const MAX_RETRIES: u32 = 5;
/// Idle flows are RST and dropped after this.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Host-side connect timeout for passive opens.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Guest-side handshake timeout for active opens.
const ACTIVE_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Fixed receive window advertised to the guest (64 KiB - 1; no scaling).
const WINDOW: u16 = 0xFFFF;
/// In-memory pipe capacity backing [`GuestStream`].
const DUPLEX_CAPACITY: usize = 64 * 1024;

/// Identity of a vTCP flow as seen from the guest side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub guest_ip: Ipv4Addr,
    pub guest_port: u16,
    /// Original destination (for active opens: the gateway IP).
    pub dst_ip: Ipv4Addr,
    /// Original destination port (for active opens: the gateway ephemeral).
    pub dst_port: u16,
}

/// A parsed guest TCP segment, routed from `handle_frame` to a flow task.
#[derive(Debug, Clone)]
pub(super) struct Segment {
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: Vec<u8>,
    /// MSS option value, when present (SYN segments).
    pub mss: Option<u16>,
}

impl Segment {
    pub fn from_view(tcp: &TcpView<'_>) -> Self {
        Self {
            seq: tcp.seq(),
            ack: tcp.ack(),
            flags: tcp.flags(),
            window: tcp.window(),
            payload: tcp.payload().to_vec(),
            mss: parse_mss(tcp.options()),
        }
    }

    pub fn is_syn(&self) -> bool {
        self.flags & TCP_SYN != 0
    }
    pub fn is_ack(&self) -> bool {
        self.flags & TCP_ACK != 0
    }
    pub fn is_rst(&self) -> bool {
        self.flags & TCP_RST != 0
    }
    pub fn is_fin(&self) -> bool {
        self.flags & TCP_FIN != 0
    }
}

fn parse_mss(mut opts: &[u8]) -> Option<u16> {
    while let [kind, rest @ ..] = opts {
        match kind {
            0 => return None, // end of options
            1 => opts = rest, // NOP
            2 => {
                // MSS: kind 2, len 4, 2 value bytes.
                return match rest {
                    [4, hi, lo, ..] => Some(u16::from_be_bytes([*hi, *lo])),
                    _ => None,
                };
            }
            _ => {
                let [len, ..] = rest else { return None };
                let skip = usize::from(*len);
                if skip < 2 || skip > opts.len() {
                    return None;
                }
                opts = &opts[skip..];
            }
        }
    }
    None
}

/// `true` when `a <= b` in sequence-number space.
fn seq_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

/// Build the RFC 793 reset answering an unexpected segment. Returns a full
/// IPv4 packet from the segment's destination back to its source.
pub(super) fn rst_for(ip: &Ipv4View<'_>, tcp: &TcpView<'_>, ip_id: u16) -> Vec<u8> {
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
    );
    frame::ipv4_build(ip.dst(), ip.src(), IPPROTO_TCP, 64, &seg, ip_id)
}

// ---------------------------------------------------------------------------
// GuestStream
// ---------------------------------------------------------------------------

/// Byte stream of an engine-originated TCP connection to a guest
/// ([`super::NatEngine::open_tcp_to_guest`]). Dropping it closes the
/// connection (FIN, then RST if the guest misbehaves).
pub struct GuestStream {
    inner: DuplexStream,
}

impl AsyncRead for GuestStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for GuestStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Flow spawning
// ---------------------------------------------------------------------------

/// Spawn the flow task answering a guest SYN (already inserted in the flow
/// table by the caller).
pub(super) fn spawn_passive(
    engine: Arc<NatEngine>,
    key: FlowKey,
    guest_mac: MacAddr,
    syn: Segment,
    rx: mpsc::Receiver<Segment>,
) {
    tokio::spawn(async move {
        let dst = (key.dst_ip, key.dst_port);
        match timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await {
            Ok(Ok(stream)) => {
                let mut flow = Flow::new(engine.clone(), key, guest_mac, &syn);
                flow.run_passive(stream, rx).await;
            }
            other => {
                debug!(?key, ?other, "nat: connect failed, resetting guest");
                // Connect refused / timed out: RST the guest's SYN.
                let flow = Flow::new(engine.clone(), key, guest_mac, &syn);
                flow.emit(0, syn.seq.wrapping_add(1), TCP_RST | TCP_ACK, &[], &[])
                    .await;
            }
        }
        engine.remove_tcp_flow(&key);
    });
}

/// Engine-originated connection to a guest. Resolves once the guest answers
/// SYN-ACK; RST or 10 s of silence fail it.
pub(super) async fn open_active(
    engine: Arc<NatEngine>,
    guest_ip: Ipv4Addr,
    guest_port: u16,
) -> Result<GuestStream> {
    let (tx, rx) = mpsc::channel(64);
    let key = engine.alloc_active_tcp_flow(guest_ip, guest_port, tx)?;
    let guest_mac = engine.guest_mac(guest_ip);
    let (ours, theirs) = tokio::io::duplex(DUPLEX_CAPACITY);
    let (est_tx, est_rx) = oneshot::channel();

    let task_engine = engine.clone();
    tokio::spawn(async move {
        let mut flow = Flow::new_active(task_engine.clone(), key, guest_mac);
        flow.run_active(theirs, rx, est_tx).await;
        task_engine.remove_tcp_flow(&key);
    });

    match timeout(ACTIVE_OPEN_TIMEOUT, est_rx).await {
        Ok(Ok(Ok(()))) => Ok(GuestStream { inner: ours }),
        Ok(Ok(Err(e))) => Err(anyhow!(e)).context("active open refused"),
        Ok(Err(_)) => Err(anyhow!("active open task died")),
        Err(_) => {
            engine.remove_tcp_flow(&key);
            Err(anyhow!(
                "active open to {guest_ip}:{guest_port} timed out after {ACTIVE_OPEN_TIMEOUT:?}"
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Flow core
// ---------------------------------------------------------------------------

/// A host→guest segment awaiting acknowledgement.
struct TxSeg {
    seq: u32,
    data: Vec<u8>,
    fin: bool,
}

impl TxSeg {
    fn end(&self) -> u32 {
        self.seq
            .wrapping_add(self.data.len() as u32)
            .wrapping_add(u32::from(self.fin))
    }
}

struct Flow {
    engine: Arc<NatEngine>,
    key: FlowKey,
    guest_mac: MacAddr,
    /// Our next send sequence number.
    snd_nxt: u32,
    /// Oldest unacknowledged sequence number.
    snd_una: u32,
    /// Next sequence number expected from the guest.
    rcv_nxt: u32,
    /// Guest's advertised receive window (honored for sending).
    peer_wnd: u32,
    /// Effective MSS: min(our clamp, guest's SYN option).
    mss: usize,
    unacked: VecDeque<TxSeg>,
    retries: u32,
    rto_at: Option<Instant>,
    last_activity: Instant,
    our_fin_sent: bool,
    guest_fin_rcvd: bool,
}

impl Flow {
    /// Passive flow seeded from the guest's SYN.
    fn new(engine: Arc<NatEngine>, key: FlowKey, guest_mac: MacAddr, syn: &Segment) -> Self {
        let clamp = usize::from(engine.config().mtu.saturating_sub(40)).max(536);
        let isn: u32 = rand::random();
        Self {
            engine,
            key,
            guest_mac,
            snd_nxt: isn,
            snd_una: isn,
            rcv_nxt: syn.seq.wrapping_add(1),
            peer_wnd: u32::from(syn.window),
            mss: syn.mss.map_or(clamp, |m| clamp.min(usize::from(m))),
            unacked: VecDeque::new(),
            retries: 0,
            rto_at: None,
            last_activity: Instant::now(),
            our_fin_sent: false,
            guest_fin_rcvd: false,
        }
    }

    /// Active flow; sequence state is filled in during the handshake.
    fn new_active(engine: Arc<NatEngine>, key: FlowKey, guest_mac: MacAddr) -> Self {
        let clamp = usize::from(engine.config().mtu.saturating_sub(40)).max(536);
        let isn: u32 = rand::random();
        Self {
            engine,
            key,
            guest_mac,
            snd_nxt: isn,
            snd_una: isn,
            rcv_nxt: 0,
            peer_wnd: 0,
            mss: clamp,
            unacked: VecDeque::new(),
            retries: 0,
            rto_at: None,
            last_activity: Instant::now(),
            our_fin_sent: false,
            guest_fin_rcvd: false,
        }
    }

    fn mss_clamp(&self) -> u16 {
        self.engine.config().mtu.saturating_sub(40).max(536)
    }

    /// Emit one segment toward the guest.
    async fn emit(&self, seq: u32, ack: u32, flags: u8, payload: &[u8], options: &[u8]) {
        let seg = frame::tcp_build(
            self.key.dst_ip,
            self.key.guest_ip,
            TcpFields {
                src_port: self.key.dst_port,
                dst_port: self.key.guest_port,
                seq,
                ack,
                flags,
                window: WINDOW,
                options,
            },
            payload,
        );
        let pkt = frame::ipv4_build(
            self.key.dst_ip,
            self.key.guest_ip,
            IPPROTO_TCP,
            64,
            &seg,
            self.engine.next_ip_id(),
        );
        self.engine.send_frame(self.guest_mac, &pkt).await;
    }

    async fn send_ack(&self) {
        self.emit(self.snd_nxt, self.rcv_nxt, TCP_ACK, &[], &[])
            .await;
    }

    async fn send_rst(&self) {
        self.emit(self.snd_nxt, self.rcv_nxt, TCP_RST | TCP_ACK, &[], &[])
            .await;
    }

    /// Queue and transmit data (and/or FIN) toward the guest.
    async fn send_data(&mut self, data: Vec<u8>, fin: bool) {
        let mut flags = TCP_ACK;
        if !data.is_empty() {
            flags |= TCP_PSH;
        }
        if fin {
            flags |= TCP_FIN;
        }
        self.emit(self.snd_nxt, self.rcv_nxt, flags, &data, &[])
            .await;
        let seg = TxSeg {
            seq: self.snd_nxt,
            data,
            fin,
        };
        self.snd_nxt = seg.end();
        self.unacked.push_back(seg);
        if self.rto_at.is_none() {
            self.rto_at = Some(Instant::now() + RTO);
        }
    }

    fn in_flight(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una)
    }

    /// Process an acknowledgement; drops fully-acked segments and rearms
    /// the RTO.
    fn on_ack(&mut self, seg: &Segment) {
        if !seg.is_ack() {
            return;
        }
        self.peer_wnd = u32::from(seg.window);
        let ack = seg.ack;
        // Acceptable: snd_una < ack <= snd_nxt.
        if ack.wrapping_sub(self.snd_una) == 0
            || ack.wrapping_sub(self.snd_una) > self.snd_nxt.wrapping_sub(self.snd_una)
        {
            return;
        }
        self.snd_una = ack;
        while let Some(front) = self.unacked.front() {
            if seq_le(front.end(), self.snd_una) {
                self.unacked.pop_front();
            } else {
                break;
            }
        }
        self.retries = 0;
        self.rto_at = if self.unacked.is_empty() {
            None
        } else {
            Some(Instant::now() + RTO)
        };
    }

    /// Retransmit the oldest unacked segment. Returns `false` when the flow
    /// must die (retry budget exhausted).
    async fn on_rto(&mut self) -> bool {
        self.retries += 1;
        if self.retries > MAX_RETRIES {
            debug!(key = ?self.key, "nat: vtcp retransmit budget exhausted");
            self.send_rst().await;
            return false;
        }
        if let Some(seg) = self.unacked.front() {
            let mut flags = TCP_ACK;
            if !seg.data.is_empty() {
                flags |= TCP_PSH;
            }
            if seg.fin {
                flags |= TCP_FIN;
            }
            let (seq, data) = (seg.seq, seg.data.clone());
            trace!(key = ?self.key, seq, "nat: vtcp retransmit");
            self.emit(seq, self.rcv_nxt, flags, &data, &[]).await;
        }
        self.rto_at = Some(Instant::now() + RTO);
        true
    }

    /// Accept in-order payload/FIN from the guest, writing payload into the
    /// proxied stream. Returns `false` when the flow must die.
    async fn on_segment<W: AsyncWrite + Unpin>(
        &mut self,
        seg: Segment,
        wr: &mut W,
        wr_open: &mut bool,
    ) -> bool {
        if seg.is_rst() {
            debug!(key = ?self.key, "nat: guest RST");
            return false;
        }
        self.last_activity = Instant::now();
        self.on_ack(&seg);

        let mut need_ack = false;
        let payload_len = seg.payload.len() as u32;

        if !seg.payload.is_empty() {
            if seq_le(seg.seq, self.rcv_nxt) {
                let off = self.rcv_nxt.wrapping_sub(seg.seq);
                if off < payload_len {
                    // New bytes (possibly a retransmission overlapping old
                    // data; accept the tail).
                    let fresh = &seg.payload[off as usize..];
                    if *wr_open && wr.write_all(fresh).await.is_err() {
                        // Proxied peer is gone: reset the guest.
                        self.send_rst().await;
                        return false;
                    }
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(fresh.len() as u32);
                }
                need_ack = true; // fresh data or pure duplicate: (re-)ACK
            } else {
                // Out of order (future): drop, dup-ACK; the guest resends.
                need_ack = true;
            }
        }

        if seg.is_fin() {
            let fin_seq = seg.seq.wrapping_add(payload_len);
            if fin_seq == self.rcv_nxt && !self.guest_fin_rcvd {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                self.guest_fin_rcvd = true;
                if *wr_open {
                    let _ = wr.shutdown().await;
                    *wr_open = false;
                }
            }
            need_ack = true;
        }

        if need_ack {
            self.send_ack().await;
        }
        true
    }

    fn closed_both_ways(&self) -> bool {
        self.guest_fin_rcvd && self.our_fin_sent && self.snd_una == self.snd_nxt
    }

    // -- passive ---------------------------------------------------------

    /// Host connect succeeded: complete the guest handshake, then proxy.
    async fn run_passive(&mut self, stream: TcpStream, mut rx: mpsc::Receiver<Segment>) {
        // SYN-ACK consumes one sequence number.
        let isn = self.snd_nxt;
        let mss = self.mss_clamp().to_be_bytes();
        let opts = [2, 4, mss[0], mss[1]];
        self.emit(isn, self.rcv_nxt, TCP_SYN | TCP_ACK, &[], &opts)
            .await;
        self.snd_nxt = isn.wrapping_add(1);
        self.rto_at = Some(Instant::now() + RTO);

        // SYN_RECEIVED: wait for the guest's ACK of our ISN.
        let first = loop {
            let deadline = self.rto_at.unwrap_or_else(|| Instant::now() + RTO);
            tokio::select! {
                seg = rx.recv() => {
                    let Some(seg) = seg else { return };
                    if seg.is_rst() {
                        return;
                    }
                    if seg.is_syn() && !seg.is_ack() {
                        // Duplicate SYN: re-answer.
                        self.emit(isn, self.rcv_nxt, TCP_SYN | TCP_ACK, &[], &opts).await;
                        continue;
                    }
                    if seg.is_ack() && seg.ack == self.snd_nxt {
                        self.snd_una = self.snd_nxt;
                        self.peer_wnd = u32::from(seg.window);
                        self.retries = 0;
                        self.rto_at = None;
                        break Some(seg); // may carry data already
                    }
                    // Unacceptable ACK in SYN_RECEIVED: reset.
                    self.send_rst().await;
                    return;
                }
                _ = sleep_until(deadline) => {
                    self.retries += 1;
                    if self.retries > MAX_RETRIES {
                        self.send_rst().await;
                        return;
                    }
                    self.emit(isn, self.rcv_nxt, TCP_SYN | TCP_ACK, &[], &opts).await;
                    self.rto_at = Some(Instant::now() + RTO);
                }
            }
        };
        debug!(key = ?self.key, "nat: vtcp established (passive)");
        self.run_established(stream, rx, first).await;
    }

    // -- active ------------------------------------------------------------

    async fn run_active(
        &mut self,
        stream: DuplexStream,
        mut rx: mpsc::Receiver<Segment>,
        est_tx: oneshot::Sender<Result<(), String>>,
    ) {
        let isn = self.snd_nxt;
        let mss = self.mss_clamp().to_be_bytes();
        let opts = [2, 4, mss[0], mss[1]];
        self.emit(isn, 0, TCP_SYN, &[], &opts).await;
        self.snd_nxt = isn.wrapping_add(1);
        self.rto_at = Some(Instant::now() + RTO);

        // SYN_SENT: wait for SYN-ACK.
        loop {
            let deadline = self.rto_at.unwrap_or_else(|| Instant::now() + RTO);
            tokio::select! {
                seg = rx.recv() => {
                    let Some(seg) = seg else { return };
                    if seg.is_rst() {
                        let _ = est_tx.send(Err("connection refused by guest".into()));
                        return;
                    }
                    if seg.is_syn() && seg.is_ack() && seg.ack == self.snd_nxt {
                        self.snd_una = self.snd_nxt;
                        self.rcv_nxt = seg.seq.wrapping_add(1);
                        self.peer_wnd = u32::from(seg.window);
                        if let Some(m) = seg.mss {
                            self.mss = self.mss.min(usize::from(m));
                        }
                        self.retries = 0;
                        self.rto_at = None;
                        self.send_ack().await;
                        break;
                    }
                    // Anything else in SYN_SENT is unexpected: reset.
                    self.send_rst().await;
                    let _ = est_tx.send(Err("unexpected segment during handshake".into()));
                    return;
                }
                _ = sleep_until(deadline) => {
                    self.retries += 1;
                    if self.retries > MAX_RETRIES {
                        return; // caller's 10 s timeout reports the error
                    }
                    self.emit(isn, 0, TCP_SYN, &[], &opts).await;
                    self.rto_at = Some(Instant::now() + RTO);
                }
            }
        }
        debug!(key = ?self.key, "nat: vtcp established (active)");
        if est_tx.send(Ok(())).is_err() {
            // Caller gave up (timeout) — tear down politely.
            self.send_rst().await;
            return;
        }
        self.run_established(stream, rx, None).await;
    }

    // -- established -------------------------------------------------------

    /// Proxy loop: guest segments ⇄ host/duplex byte stream.
    async fn run_established<S>(
        &mut self,
        stream: S,
        mut rx: mpsc::Receiver<Segment>,
        pending: Option<Segment>,
    ) where
        S: AsyncRead + AsyncWrite,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let mut wr_open = true;
        let mut rd_done = false;
        let mut buf = vec![0u8; self.mss.max(536)];

        if let Some(seg) = pending
            && !self.on_segment(seg, &mut wr, &mut wr_open).await
        {
            return;
        }

        loop {
            if self.closed_both_ways() {
                trace!(key = ?self.key, "nat: vtcp closed cleanly");
                return;
            }
            let can_read = !rd_done
                && !self.our_fin_sent
                && self.in_flight() < self.peer_wnd
                && self.unacked.len() < 64;
            let rto_deadline = self.rto_at.unwrap_or_else(|| Instant::now() + RTO);
            let idle_deadline = self.last_activity + IDLE_TIMEOUT;

            tokio::select! {
                seg = rx.recv() => {
                    let Some(seg) = seg else { return };
                    if !self.on_segment(seg, &mut wr, &mut wr_open).await {
                        return;
                    }
                }
                read = rd.read(&mut buf), if can_read => {
                    match read {
                        Ok(0) | Err(_) => {
                            rd_done = true;
                            self.our_fin_sent = true;
                            self.send_data(Vec::new(), true).await;
                        }
                        Ok(n) => {
                            let take = n.min(self.mss);
                            self.send_data(buf[..take].to_vec(), false).await;
                            if take < n {
                                // Shouldn't happen (buf is mss-sized), but
                                // never silently drop bytes.
                                self.send_data(buf[take..n].to_vec(), false).await;
                            }
                        }
                    }
                }
                _ = sleep_until(rto_deadline), if self.rto_at.is_some() => {
                    if !self.on_rto().await {
                        return;
                    }
                }
                _ = sleep_until(idle_deadline) => {
                    debug!(key = ?self.key, "nat: vtcp idle timeout, resetting");
                    self.send_rst().await;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn mss_option_parsing() {
        assert_eq!(parse_mss(&[2, 4, 0x05, 0xB4]), Some(1460));
        assert_eq!(parse_mss(&[1, 1, 2, 4, 0x21, 0x00]), Some(0x2100));
        assert_eq!(parse_mss(&[]), None);
        assert_eq!(parse_mss(&[0, 2, 4, 1, 1]), None, "EOL stops the scan");
        // Unknown option skipped (kind 3 = window scale, len 3).
        assert_eq!(parse_mss(&[3, 3, 7, 2, 4, 0x01, 0x00]), Some(256));
        // Malformed length never panics.
        assert_eq!(parse_mss(&[5, 0]), None);
        assert_eq!(parse_mss(&[2, 4]), None);
    }

    #[test]
    fn seq_compare_wraps() {
        assert!(seq_le(1, 2));
        assert!(seq_le(2, 2));
        assert!(!seq_le(3, 2));
        assert!(seq_le(u32::MAX, 5), "wraparound");
        assert!(!seq_le(5, u32::MAX));
    }
}
