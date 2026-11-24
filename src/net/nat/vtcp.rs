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
//! Simplifications, all backed by the PRD's performance non-goal: go-back-N
//! retransmission of the first unacked segment on a fixed 1 s RTO with at most
//! 5 tries, no TIME_WAIT, no window scaling, no SACK, RST on anything
//! unexpected.
//!
//! Host→guest data never runs past the right edge of the guest's advertised
//! window, and a closed window is probed every RTO until it reopens. Sending a
//! full MSS whenever any window remained overran it once per window on a slow
//! reader: a Windows guest acked the excess while still advertising a full
//! window, buffered a fast download until it ran out of memory, and the flow
//! fell to one retransmit per RTO.
//!
//! Guest→host data waits in the flow's own buffer of one window
//! ([`Flow::to_host`]) and the flow task never waits for the host's socket
//! outside its `select!`. The window advertised to the guest is the room left
//! in that buffer, so a host that reads slowly closes the guest's window, as
//! a real TCP peer's would. Awaiting the write in line let one host that did
//! not read stop the flow task from draining its channel, and the gateway
//! task of the whole segment waited behind that channel (review F4).
//!
//! Segments arriving ahead of `rcv_nxt` ARE buffered (up to one window per
//! flow) and stitched in when the hole fills. Dropping them and dup-ACKing —
//! the original behaviour — cost the guest its entire in-flight window for
//! every single loss, and the userspace switch drops frames whenever a port's
//! egress queue fills, so bulk guest→host transfers collapsed into a
//! retransmit crawl.

use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep, sleep_until, timeout};
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
/// Largest receive window advertised to the guest (64 KiB - 1; no scaling),
/// and so the most guest data a flow holds for a host that has not taken it.
const WINDOW: u16 = 0xFFFF;
/// Guest segments a flow's channel holds for its task. [`NatEngine`] drops a
/// segment that finds it full and never waits (review F4), so it is sized
/// for what a guest inside its window can have in flight: one window in
/// segments of the smallest MSS (65535 / 536 = 123), and as many again for
/// the acknowledgements of a transfer the other way.
pub(super) const SEGMENT_QUEUE: usize = 256;
/// In-memory pipe capacity backing [`GuestStream`].
const DUPLEX_CAPACITY: usize = 64 * 1024;
/// Guest data held per flow while waiting for a missing segment. One advertised
/// window's worth: past that the guest cannot legally have more in flight, so a
/// bigger buffer would only hold data we will never be able to stitch up.
const OOO_MAX_BYTES: usize = WINDOW as usize;
/// Segments held per flow while waiting for a missing segment (review
/// networking-2 F23). The byte limit alone let a guest queue one window of
/// one-byte segments, 65 535 map entries with an allocation each. A guest
/// inside its window has at most one window in segments of the smallest MSS
/// in flight (65535 / 536 = 123), so this is twice that. Past it the segment
/// farthest ahead is dropped, and the guest sends it again.
const OOO_MAX_SEGMENTS: usize = 256;

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
pub(super) fn rst_for(ip: &Ipv4View<'_>, tcp: &TcpView<'_>, ip_id: u16) -> Option<Vec<u8>> {
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
/// table by the caller). The task holds `slot`, the flow's place under the
/// engine's limit, until it ends.
pub(super) fn spawn_passive(
    engine: Arc<NatEngine>,
    key: FlowKey,
    guest_mac: MacAddr,
    syn: Segment,
    rx: mpsc::Receiver<Segment>,
    slot: super::PassiveSlot,
) {
    tokio::spawn(async move {
        // Given back when the task ends, however it ends.
        let _slot = slot;
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
    let (tx, rx) = mpsc::channel(SEGMENT_QUEUE);
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
    /// Guest segments that arrived ahead of `rcv_nxt`, keyed by sequence
    /// number, held until the hole in front of them fills.
    ooo: BTreeMap<u32, Vec<u8>>,
    /// Bytes currently held in `ooo`, capped at [`OOO_MAX_BYTES`].
    ooo_bytes: usize,
    /// Queue entries [`Flow::drain_ooo`] has looked at, for the test of its
    /// cost.
    #[cfg(test)]
    ooo_visits: usize,
    retries: u32,
    rto_at: Option<Instant>,
    last_activity: Instant,
    our_fin_sent: bool,
    guest_fin_rcvd: bool,
    /// Guest data accepted and acknowledged that the host side has not
    /// taken yet, at most [`WINDOW`] bytes.
    to_host: VecDeque<u8>,
    /// The guest's FIN arrived while `to_host` held data: the host side is
    /// shut down once the buffer is written out.
    host_fin_pending: bool,
    /// The window in the last segment sent to the guest.
    advertised: AtomicU16,
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
            ooo: BTreeMap::new(),
            ooo_bytes: 0,
            #[cfg(test)]
            ooo_visits: 0,
            retries: 0,
            rto_at: None,
            last_activity: Instant::now(),
            our_fin_sent: false,
            guest_fin_rcvd: false,
            to_host: VecDeque::new(),
            host_fin_pending: false,
            advertised: AtomicU16::new(WINDOW),
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
            ooo: BTreeMap::new(),
            ooo_bytes: 0,
            #[cfg(test)]
            ooo_visits: 0,
            retries: 0,
            rto_at: None,
            last_activity: Instant::now(),
            our_fin_sent: false,
            guest_fin_rcvd: false,
            to_host: VecDeque::new(),
            host_fin_pending: false,
            advertised: AtomicU16::new(WINDOW),
        }
    }

    fn mss_clamp(&self) -> u16 {
        self.engine.config().mtu.saturating_sub(40).max(536)
    }

    /// Room left for guest data, which is the receive window the guest is
    /// told. Accepting a byte moves `rcv_nxt` up and the room down by one,
    /// so the window's right edge never moves back.
    fn rcv_window(&self) -> u16 {
        WINDOW.saturating_sub(self.to_host.len().min(usize::from(WINDOW)) as u16)
    }

    /// Emit one segment toward the guest.
    async fn emit(&self, seq: u32, ack: u32, flags: u8, payload: &[u8], options: &[u8]) {
        let window = self.rcv_window();
        self.advertised.store(window, Ordering::Relaxed);
        let seg = frame::tcp_build(
            self.key.dst_ip,
            self.key.guest_ip,
            TcpFields {
                src_port: self.key.dst_port,
                dst_port: self.key.guest_port,
                seq,
                ack,
                flags,
                window,
                options,
            },
            payload,
        );
        let pkt = seg.and_then(|seg| {
            frame::ipv4_build(
                self.key.dst_ip,
                self.key.guest_ip,
                IPPROTO_TCP,
                64,
                &seg,
                self.engine.next_ip_id(),
            )
        });
        let Some(pkt) = pkt else { return };
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

    /// How many new bytes may go to the guest now: never past the right edge
    /// of its advertised window, at most one MSS, and nothing at all while
    /// what is left is a sliver smaller than a segment the window could
    /// otherwise take (sender-side silly-window avoidance, RFC 1122
    /// 4.2.3.4). Sending a full MSS whenever *any* window remained overran
    /// it on every slow-reader cycle; a receiver that discards the excess
    /// stalls the flow for an RTO per window.
    fn send_allowance(&self) -> usize {
        let usable = self.peer_wnd.saturating_sub(self.in_flight()) as usize;
        let segment = self.mss.min(self.peer_wnd as usize);
        if usable == 0 || usable < segment {
            0
        } else {
            usable.min(self.mss)
        }
    }

    /// Probe a closed window: an ACK one byte behind `snd_nxt` lies outside
    /// the guest's window, so it answers with an ACK carrying the current
    /// one (RFC 9293 3.8.6.1). Without it a lost window update would leave
    /// the flow silent until the idle reset.
    async fn send_window_probe(&self) {
        trace!(key = ?self.key, "nat: vtcp zero-window probe");
        self.emit(
            self.snd_nxt.wrapping_sub(1),
            self.rcv_nxt,
            TCP_ACK,
            &[],
            &[],
        )
        .await;
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
                    self.accept(fresh, *wr_open);
                    // This segment may have filled a hole: take whatever of
                    // the out-of-order queue is now contiguous.
                    self.drain_ooo(*wr_open);
                    // Hand it to the host now if its socket takes it, so the
                    // ACK below still advertises a full window.
                    if !self.flush_to_host(wr, wr_open).await {
                        return false;
                    }
                }
                need_ack = true; // fresh data or pure duplicate: (re-)ACK
            } else {
                // Ahead of rcv_nxt: hold it until the hole fills. Dropping it
                // (and only dup-ACKing) cost the guest its whole in-flight
                // window per loss — with the switch dropping frames whenever a
                // port queue fills, that collapsed bulk guest→host transfers
                // to a retransmit crawl.
                self.queue_ooo(seg.seq, &seg.payload);
                need_ack = true;
            }
        }

        if seg.is_fin() {
            let fin_seq = seg.seq.wrapping_add(payload_len);
            if fin_seq == self.rcv_nxt && !self.guest_fin_rcvd {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                self.guest_fin_rcvd = true;
                if *wr_open {
                    if self.to_host.is_empty() {
                        let _ = wr.shutdown().await;
                        *wr_open = false;
                    } else {
                        // The host gets the buffered data first.
                        self.host_fin_pending = true;
                    }
                }
            }
            need_ack = true;
        }

        if need_ack {
            self.send_ack().await;
        }
        true
    }

    /// Hold a segment that arrived ahead of `rcv_nxt`. Overlapping or duplicate
    /// entries are replaced by the longer one. A segment that starts past the
    /// largest window is not held: the guest cannot legally have sent it, and
    /// the queue's order relies on every entry being that near `rcv_nxt`.
    /// When the queue is over [`OOO_MAX_BYTES`] or [`OOO_MAX_SEGMENTS`] the
    /// entry farthest ahead is dropped (the new one, if that is it),
    /// degrading to the previous drop-and-dup-ACK behaviour rather than
    /// growing without bound. The entries nearest the hole are the ones that
    /// are stitched in first, so they are the ones worth keeping.
    fn queue_ooo(&mut self, seq: u32, payload: &[u8]) {
        if payload.len() > OOO_MAX_BYTES
            || seq.wrapping_sub(self.rcv_nxt) as usize >= OOO_MAX_BYTES
        {
            return;
        }
        match self.ooo.get(&seq) {
            Some(existing) if existing.len() >= payload.len() => return,
            Some(existing) => self.ooo_bytes -= existing.len(),
            None => {}
        }
        self.ooo.insert(seq, payload.to_vec());
        self.ooo_bytes += payload.len();
        while self.ooo_bytes > OOO_MAX_BYTES || self.ooo.len() > OOO_MAX_SEGMENTS {
            let Some(farthest) = self.ooo_farthest() else {
                break;
            };
            if let Some(dropped) = self.ooo.remove(&farthest) {
                self.ooo_bytes -= dropped.len();
            }
        }
    }

    /// The queued segment to drop when the queue is over a limit: the one
    /// farthest ahead of `rcv_nxt`. The map is ordered by the plain number,
    /// so that is the largest key, or across the wrap of the sequence number
    /// the largest key below `rcv_nxt`. An entry left at or behind `rcv_nxt`
    /// (one that waits for room in the window) is dropped only when nothing
    /// is ahead of it. One or two lookups, whatever the queue holds.
    fn ooo_farthest(&self) -> Option<u32> {
        let wrapped = self
            .ooo
            .range(..self.rcv_nxt)
            .next_back()
            .map(|(seq, _)| *seq)
            .filter(|&seq| !seq_le(seq, self.rcv_nxt));
        wrapped.or_else(|| self.ooo.keys().next_back().copied())
    }

    /// The queued segment that starts at or before `rcv_nxt` and nearest to
    /// it, if there is one: the largest key up to `rcv_nxt`, or across the
    /// wrap the largest key of all. One lookup, whatever the queue holds.
    fn ooo_at_or_before_rcv_nxt(&self) -> Option<u32> {
        self.ooo
            .range(..=self.rcv_nxt)
            .next_back()
            .or_else(|| self.ooo.iter().next_back())
            .map(|(seq, _)| *seq)
            .filter(|&seq| seq_le(seq, self.rcv_nxt))
    }

    /// Take in-order guest bytes: as many as the advertised window has room
    /// for. The rest is not acknowledged, so the guest sends it again once
    /// the window reopens. Returns how many bytes were taken. With the host
    /// side already shut down the bytes are acknowledged and discarded.
    fn accept(&mut self, fresh: &[u8], wr_open: bool) -> usize {
        let n = if wr_open {
            let n = fresh.len().min(usize::from(self.rcv_window()));
            self.to_host.extend(&fresh[..n]);
            n
        } else {
            fresh.len()
        };
        self.rcv_nxt = self.rcv_nxt.wrapping_add(n as u32);
        n
    }

    /// Take every queued segment that is now contiguous with `rcv_nxt`,
    /// trimming any overlap, for as long as the window has room. A segment
    /// that does not fit whole stays queued for the next call.
    ///
    /// Each turn looks up one entry and removes it, so a call costs the
    /// entries it takes and not the queue's length for each of them (review
    /// networking-2 F23).
    fn drain_ooo(&mut self, wr_open: bool) {
        while let Some(seq) = self.ooo_at_or_before_rcv_nxt() {
            #[cfg(test)]
            {
                self.ooo_visits += 1;
            }
            let payload = self.ooo.remove(&seq).expect("key just found");
            self.ooo_bytes -= payload.len();
            let behind = self.rcv_nxt.wrapping_sub(seq) as usize;
            if behind >= payload.len() {
                // It ends at or before rcv_nxt: already taken. Forget it.
                continue;
            }
            // It extends past rcv_nxt: it continues the stream.
            let fresh = &payload[behind..];
            if self.accept(fresh, wr_open) < fresh.len() {
                // No room for all of it: keep it for when the host has
                // taken some of the buffer.
                self.ooo_bytes += payload.len();
                self.ooo.insert(seq, payload);
                return;
            }
        }
    }

    /// Write buffered guest data to the host side for as long as it takes
    /// the data at once, and never wait for it: the flow's `select!` waits,
    /// together with everything else the flow has to answer. Returns `false`
    /// when the flow must die (the proxied peer is gone; the guest is reset).
    async fn flush_to_host<W: AsyncWrite + Unpin>(
        &mut self,
        wr: &mut W,
        wr_open: &mut bool,
    ) -> bool {
        while *wr_open && !self.to_host.is_empty() {
            let chunk = self.to_host.as_slices().0;
            let wrote = std::future::poll_fn(|cx| {
                Poll::Ready(match Pin::new(&mut *wr).poll_write(cx, chunk) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            match wrote {
                None => return true,
                Some(result) => {
                    if !self.wrote_to_host(result, wr, wr_open).await {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// The host side took `result` bytes of `to_host` (or failed): drop
    /// them, and shut the host side down once a pending guest FIN has
    /// nothing in front of it. Returns `false` when the flow must die.
    async fn wrote_to_host<W: AsyncWrite + Unpin>(
        &mut self,
        result: std::io::Result<usize>,
        wr: &mut W,
        wr_open: &mut bool,
    ) -> bool {
        match result {
            Ok(n) if n > 0 => {
                self.to_host.drain(..n.min(self.to_host.len()));
            }
            // Proxied peer is gone: reset the guest.
            _ => {
                self.send_rst().await;
                return false;
            }
        }
        if self.to_host.is_empty() && self.host_fin_pending {
            self.host_fin_pending = false;
            let _ = wr.shutdown().await;
            *wr_open = false;
        }
        true
    }

    /// After the host took buffered data: stitch in what was waiting for
    /// room, and tell the guest when its window has grown by a segment or
    /// is whole again (receiver-side silly-window avoidance, RFC 1122
    /// 4.2.3.3). Without the update a guest that filled the window would
    /// only learn of the room from its own zero-window probe.
    async fn after_host_write(&mut self, wr_open: bool) {
        let before = self.rcv_nxt;
        self.drain_ooo(wr_open);
        let window = self.rcv_window();
        let advertised = self.advertised.load(Ordering::Relaxed);
        let grown = usize::from(window.saturating_sub(advertised));
        if self.rcv_nxt != before || grown >= self.mss || (grown > 0 && window == WINDOW) {
            self.send_ack().await;
        }
    }

    fn closed_both_ways(&self) -> bool {
        self.guest_fin_rcvd
            && self.our_fin_sent
            && self.snd_una == self.snd_nxt
            && self.to_host.is_empty()
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
            let allowance = self.send_allowance();
            let sending = !rd_done && !self.our_fin_sent;
            let can_read = sending && allowance > 0 && self.unacked.len() < 64;
            // Nothing in flight to retransmit and no room to send: only a
            // probe will hear about the window reopening.
            let window_closed = sending && allowance == 0 && self.unacked.is_empty();
            let rto_deadline = self.rto_at.unwrap_or_else(|| Instant::now() + RTO);
            let idle_deadline = self.last_activity + IDLE_TIMEOUT;
            let host_owed = wr_open && !self.to_host.is_empty();

            tokio::select! {
                seg = rx.recv() => {
                    let Some(seg) = seg else { return };
                    if !self.on_segment(seg, &mut wr, &mut wr_open).await {
                        return;
                    }
                }
                wrote = wr.write(self.to_host.as_slices().0), if host_owed => {
                    if !self.wrote_to_host(wrote, &mut wr, &mut wr_open).await
                        || !self.flush_to_host(&mut wr, &mut wr_open).await
                    {
                        return;
                    }
                    self.after_host_write(wr_open).await;
                }
                read = rd.read(&mut buf[..allowance]), if can_read => {
                    match read {
                        Ok(0) | Err(_) => {
                            rd_done = true;
                            self.our_fin_sent = true;
                            self.send_data(Vec::new(), true).await;
                        }
                        Ok(n) => self.send_data(buf[..n].to_vec(), false).await,
                    }
                }
                _ = sleep(RTO), if window_closed => {
                    self.send_window_probe().await;
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

    /// A passive flow whose guest's next byte is `rcv_nxt`, on an engine
    /// nobody reads from.
    fn flow_at(rcv_nxt: u32) -> Flow {
        let gw_ip = Ipv4Addr::new(10, 0, 0, 1);
        let (tx, _rx) = mpsc::channel(1);
        let engine = NatEngine::new(
            super::super::NatConfig::new(gw_ip, MacAddr([2, 0, 0, 0, 0, 1])),
            tx,
        );
        let key = FlowKey {
            guest_ip: Ipv4Addr::new(10, 0, 0, 2),
            guest_port: 40000,
            dst_ip: Ipv4Addr::new(192, 0, 2, 1),
            dst_port: 80,
        };
        let syn = Segment {
            seq: rcv_nxt.wrapping_sub(1),
            ack: 0,
            flags: TCP_SYN,
            window: 65535,
            payload: Vec::new(),
            mss: None,
        };
        Flow::new(engine, key, MacAddr([2, 0, 0, 0, 0, 2]), &syn)
    }

    /// Review networking-2 F23. A guest that sends one window of one-byte
    /// segments ahead of the hole made the queue hold one entry for each,
    /// and the byte that filled the hole then cost a scan of the whole queue
    /// per entry taken.
    #[tokio::test]
    async fn the_out_of_order_queue_is_bounded_in_segments() {
        let rcv_nxt = 1000;
        let mut flow = flow_at(rcv_nxt);
        for i in 0..60_000u32 {
            flow.queue_ooo(rcv_nxt + 1 + i, &[0]);
            assert!(
                flow.ooo.len() <= OOO_MAX_SEGMENTS,
                "the queue holds {} segments, over its limit of {OOO_MAX_SEGMENTS}",
                flow.ooo.len()
            );
        }
        // What is kept is what is stitched in first: the segments nearest
        // the hole.
        let queued = flow.ooo.len();
        assert_eq!(queued, OOO_MAX_SEGMENTS);
        assert_eq!(flow.ooo.keys().next(), Some(&(rcv_nxt + 1)));
        assert_eq!(flow.ooo_bytes, queued);

        // The byte at rcv_nxt arrives, as `on_segment` takes it.
        assert_eq!(flow.accept(&[0], true), 1);
        flow.drain_ooo(true);
        assert_eq!(flow.rcv_nxt, rcv_nxt + 1 + queued as u32);
        assert!(flow.ooo.is_empty());
        assert_eq!(flow.ooo_bytes, 0);
        assert_eq!(flow.to_host.len(), 1 + queued);
        assert!(
            flow.ooo_visits <= queued + 1,
            "taking {queued} segments looked at {} queue entries",
            flow.ooo_visits
        );
    }

    /// The same cost for a queue of entries that are behind `rcv_nxt` when
    /// the hole fills: one large segment covers every small one, and each is
    /// forgotten at one look.
    #[tokio::test]
    async fn stale_out_of_order_segments_are_forgotten_at_one_look_each() {
        let rcv_nxt = 1000;
        let mut flow = flow_at(rcv_nxt);
        for i in 0..100u32 {
            flow.queue_ooo(rcv_nxt + 1 + 2 * i, &[0]);
        }
        flow.queue_ooo(rcv_nxt + 300, b"tail");
        assert_eq!(flow.accept(&[7; 300], true), 300);
        flow.drain_ooo(true);
        assert_eq!(flow.rcv_nxt, rcv_nxt + 304);
        assert!(flow.ooo.is_empty());
        assert_eq!(flow.ooo_bytes, 0);
        assert_eq!(flow.to_host.len(), 304);
        assert!(flow.ooo_visits <= 101, "{} looks", flow.ooo_visits);
    }

    /// The queue's order is sequence order from `rcv_nxt`, across the wrap
    /// of the sequence number too.
    #[tokio::test]
    async fn the_out_of_order_queue_is_stitched_across_the_sequence_wrap() {
        let rcv_nxt = u32::MAX - 2;
        let mut flow = flow_at(rcv_nxt);
        // Ahead of the hole on both sides of the wrap, queued last first.
        flow.queue_ooo(rcv_nxt.wrapping_add(7), b"hi");
        flow.queue_ooo(rcv_nxt.wrapping_add(4), b"def");
        flow.queue_ooo(rcv_nxt.wrapping_add(1), b"abc");
        // The farthest ahead is the one past the wrap, not the largest key.
        assert_eq!(flow.ooo_farthest(), Some(rcv_nxt.wrapping_add(7)));
        // Far past the largest window: not held.
        flow.queue_ooo(rcv_nxt.wrapping_add(70_000), b"x");
        assert_eq!(flow.ooo.len(), 3);

        assert_eq!(flow.accept(b"_", true), 1);
        flow.drain_ooo(true);
        assert_eq!(flow.rcv_nxt, rcv_nxt.wrapping_add(9));
        assert!(flow.ooo.is_empty());
        let taken: Vec<u8> = flow.to_host.iter().copied().collect();
        assert_eq!(taken, b"_abcdefhi");
    }
}
