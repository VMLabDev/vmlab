//! Supervisor-owned global segments (PRD §9.2): shared L2 switches that
//! span labs (and hosts). Created on first attach, destroyed on last detach.
//! The supervisor runs the shared segment's DHCP/DNS so registrations span
//! labs coherently. Lab daemons attach via segment trunks (unix sockets);
//! the *same* frame-forwarding trunk protocol over TCP bridges two
//! supervisors for cross-host segments — one mechanism, two transports.
//!
//! Cross-host trunks are tracked per (segment, remote IP) in a [`TrunkTable`]:
//! at most one trunk per remote host. That single-slot rule is what prevents
//! a broadcast storm when both sides declare `connect {}` to each other (two
//! parallel Service ports would re-flood every broadcast back and forth
//! forever — the switch floods broadcasts to all other ports, Service→Service
//! included). The dialer stands down while an inbound trunk from the same IP
//! is active, the listener refuses a second trunk from an IP it already has,
//! and jittered retries break the simultaneous-dial tie. Known limitation:
//! a NAT'd or multi-homed peer can defeat the IP match, so one-sided
//! `connect` remains the documented safe form for exotic topologies.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ipnet::Ipv4Net;
use serde_json::json;
use tokio::sync::Mutex;

use crate::net::dhcp::DhcpConfig;
use crate::net::dns::DnsZone;
use crate::net::framing::{read_frame, write_frame};
use crate::net::gateway::{Gateway, GatewayConfig, GatewayHandle, gateway_mac};
use crate::net::switch::{ChannelPort, PortClass, Switch};
use crate::proto::Event;

/// Global segments use a distinct pool from per-lab segments to avoid
/// collisions when both appear on one host.
const GLOBAL_POOL: &str = "10.214.0.0/16";

/// Default trunk TCP port, appended when a `connect { host }` has no `:port`.
const DEFAULT_TRUNK_PORT: u16 = 13947;

/// Retry / stand-down cadence for the dialer loop.
#[cfg(not(test))]
const REDIAL_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const REDIAL_DELAY: Duration = Duration::from_millis(150);

/// Jitter ceiling for the dialer retry (breaks simultaneous-dial ties).
#[cfg(not(test))]
const JITTER_MS: u32 = 2000;
#[cfg(test)]
const JITTER_MS: u32 = 100;

/// One cross-host trunk slot.
enum TrunkState {
    /// Our dialer is mid-handshake to this IP.
    Dialing,
    /// A live bridge exists (dialed by us or accepted from them).
    Active {
        direction: &'static str, // "dial" | "accept"
        addr: SocketAddr,
        since: chrono::DateTime<chrono::Utc>,
        /// Abort handle for an inbound bridge (dial bridges are owned by the
        /// dialer loop, which aborts with the dialer task itself).
        task: Option<tokio::task::AbortHandle>,
    },
}

/// Per-segment trunk bookkeeping, shared by the dialer loop, the listener's
/// accept path, `list()`, and segment teardown. Emits `segment.peer.up` /
/// `segment.peer.down` transition events on the supervisor's aggregate
/// stream (host-scoped: `lab` is empty; the UI keys on the segment name).
struct TrunkTable {
    segment: String,
    events: tokio::sync::broadcast::Sender<Event>,
    slots: std::sync::Mutex<HashMap<IpAddr, TrunkState>>,
}

impl TrunkTable {
    fn new(segment: &str, events: tokio::sync::broadcast::Sender<Event>) -> Arc<Self> {
        Arc::new(Self {
            segment: segment.to_string(),
            events,
            slots: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Reserve a dial slot for `ip`. `false` = a trunk to/from that host
    /// already exists (or is being dialed) — the caller stands down.
    fn try_reserve_dial(&self, ip: IpAddr) -> bool {
        let mut slots = self.slots.lock().expect("trunk slots");
        if slots.contains_key(&ip) {
            return false;
        }
        slots.insert(ip, TrunkState::Dialing);
        true
    }

    /// Whether an inbound trunk from `ip` may be accepted (no existing slot).
    /// Reserves the slot on success so a racing second accept is refused.
    fn try_reserve_accept(&self, addr: SocketAddr, task: tokio::task::AbortHandle) -> bool {
        let mut slots = self.slots.lock().expect("trunk slots");
        if slots.contains_key(&addr.ip()) {
            return false;
        }
        slots.insert(
            addr.ip(),
            TrunkState::Active {
                direction: "accept",
                addr,
                since: chrono::Utc::now(),
                task: Some(task),
            },
        );
        drop(slots);
        self.emit_up(addr, "accept");
        true
    }

    /// Flip a reserved dial slot to active (handshake succeeded).
    fn activate_dial(&self, addr: SocketAddr) {
        let mut slots = self.slots.lock().expect("trunk slots");
        slots.insert(
            addr.ip(),
            TrunkState::Active {
                direction: "dial",
                addr,
                since: chrono::Utc::now(),
                task: None,
            },
        );
        drop(slots);
        self.emit_up(addr, "dial");
    }

    /// Drop the slot for `ip`; emits `segment.peer.down` if it was active.
    fn clear(&self, ip: IpAddr) {
        let removed = self.slots.lock().expect("trunk slots").remove(&ip);
        if let Some(TrunkState::Active {
            direction, addr, ..
        }) = removed
        {
            let _ = self.events.send(Event::new(
                "segment.peer.down",
                "",
                json!({"segment": self.segment, "peer": addr.to_string(), "direction": direction}),
            ));
            tracing::info!(
                "cross-host trunk {addr} for \"{}\" down ({direction})",
                self.segment
            );
        }
    }

    fn emit_up(&self, addr: SocketAddr, direction: &'static str) {
        let _ = self.events.send(Event::new(
            "segment.peer.up",
            "",
            json!({"segment": self.segment, "peer": addr.to_string(), "direction": direction}),
        ));
        tracing::info!(
            "cross-host trunk {addr} for \"{}\" up ({direction})",
            self.segment
        );
    }

    fn has(&self, ip: IpAddr) -> bool {
        self.slots.lock().expect("trunk slots").contains_key(&ip)
    }

    fn connected(&self) -> bool {
        self.slots
            .lock()
            .expect("trunk slots")
            .values()
            .any(|s| matches!(s, TrunkState::Active { .. }))
    }

    fn peers_json(&self) -> Vec<serde_json::Value> {
        self.slots
            .lock()
            .expect("trunk slots")
            .values()
            .filter_map(|s| match s {
                TrunkState::Active {
                    direction,
                    addr,
                    since,
                    ..
                } => Some(json!({
                    "addr": addr.to_string(),
                    "direction": direction,
                    "since": since.to_rfc3339(),
                })),
                TrunkState::Dialing => None,
            })
            .collect()
    }

    /// Abort every inbound bridge and emit down for every active trunk
    /// (segment teardown).
    fn teardown(&self) {
        let slots: Vec<(IpAddr, TrunkState)> = {
            let mut map = self.slots.lock().expect("trunk slots");
            map.drain().collect()
        };
        for (_, state) in slots {
            if let TrunkState::Active {
                direction,
                addr,
                task,
                ..
            } = state
            {
                if let Some(task) = task {
                    task.abort();
                }
                let _ = self.events.send(Event::new(
                    "segment.peer.down",
                    "",
                    json!({"segment": self.segment, "peer": addr.to_string(), "direction": direction}),
                ));
            }
        }
    }
}

struct GlobalSeg {
    switch: Arc<Switch>,
    #[allow(dead_code)]
    gateway: GatewayHandle,
    subnet: Ipv4Net,
    refcount: usize,
    sock: PathBuf,
    listener: tokio::task::JoinHandle<()>,
    /// Outbound dialer loop (present once some attach declared `connect`).
    dialer: Option<tokio::task::JoinHandle<()>>,
    /// Cross-host trunk slots (dialer + inbound accepts + list()).
    trunks: Arc<TrunkTable>,
}

pub struct GlobalSegments {
    segs: Mutex<HashMap<String, GlobalSeg>>,
    next_index: Mutex<u32>,
    dns_suffix: String,
    psk: Option<String>,
    events: tokio::sync::broadcast::Sender<Event>,
    /// Where segment trunk unix sockets live (tests inject a tempdir so two
    /// in-process instances don't collide).
    sock_dir: PathBuf,
}

impl GlobalSegments {
    pub fn new(
        dns_suffix: String,
        psk: Option<String>,
        events: tokio::sync::broadcast::Sender<Event>,
    ) -> Arc<Self> {
        Self::new_at(
            crate::paths::runtime_dir().join("global"),
            dns_suffix,
            psk,
            events,
        )
    }

    fn new_at(
        sock_dir: PathBuf,
        dns_suffix: String,
        psk: Option<String>,
        events: tokio::sync::broadcast::Sender<Event>,
    ) -> Arc<Self> {
        Arc::new(Self {
            segs: Mutex::new(HashMap::new()),
            next_index: Mutex::new(0),
            dns_suffix,
            psk,
            events,
            sock_dir,
        })
    }

    async fn alloc_subnet(&self, declared: Option<Ipv4Net>) -> Result<Ipv4Net> {
        if let Some(d) = declared {
            return Ok(d);
        }
        let pool: Ipv4Net = GLOBAL_POOL.parse().expect("valid global pool");
        let mut idx = self.next_index.lock().await;
        let subnet = pool
            .subnets(24)
            .expect("pool splits")
            .nth(*idx as usize)
            .ok_or_else(|| anyhow::anyhow!("global subnet pool exhausted"))?;
        *idx += 1;
        Ok(subnet)
    }

    /// Attach to (creating if needed) the global segment `name`. Returns the
    /// unix socket the caller's lab daemon connects its trunk to.
    pub async fn attach(
        self: &Arc<Self>,
        name: &str,
        subnet: Option<Ipv4Net>,
        peer: Option<String>,
    ) -> Result<PathBuf> {
        let mut segs = self.segs.lock().await;
        if let Some(seg) = segs.get_mut(name) {
            seg.refcount += 1;
            // A later lab may be the one declaring `connect` — start the
            // dialer on an already-existing segment too.
            if let Some(peer) = peer
                && seg.dialer.is_none()
            {
                let psk = self.require_psk()?;
                seg.dialer = Some(spawn_tcp_peer_dialer(
                    seg.switch.clone(),
                    peer,
                    psk,
                    seg.trunks.clone(),
                ));
            }
            return Ok(seg.sock.clone());
        }

        let subnet = self.alloc_subnet(subnet).await?;
        let gw_ip = Ipv4Addr::from(u32::from(subnet.network()) + 1);
        let switch = Switch::new(format!("global/{name}"));
        let gw_mac = gateway_mac("__global", name);

        let mut dhcp = DhcpConfig::new(subnet, gw_ip, gw_mac);
        dhcp.dns_server = Some(gw_ip);
        dhcp.domain = Some(format!("{name}.{}", self.dns_suffix));
        let zone = DnsZone::new(&self.dns_suffix);

        let gateway = Gateway::spawn(
            &switch,
            GatewayConfig {
                segment_name: name.to_string(),
                lab_name: "__global".to_string(),
                gw_ip,
                gw_mac,
                dhcp: Some(dhcp),
                dns: Some(zone),
                upstream_dns: None,
            },
        );

        std::fs::create_dir_all(&self.sock_dir)?;
        let sock = self.sock_dir.join(format!("{name}.sock"));
        let listener = switch
            .listen_unix(&sock, PortClass::Service)
            .await
            .with_context(|| format!("listening on {}", sock.display()))?;

        let trunks = TrunkTable::new(name, self.events.clone());
        let mut dialer = None;
        if let Some(peer) = peer {
            let psk = self.require_psk()?;
            dialer = Some(spawn_tcp_peer_dialer(
                switch.clone(),
                peer,
                psk,
                trunks.clone(),
            ));
        }

        segs.insert(
            name.to_string(),
            GlobalSeg {
                switch,
                gateway,
                subnet,
                refcount: 1,
                sock: sock.clone(),
                listener,
                dialer,
                trunks,
            },
        );
        tracing::info!("global segment \"{name}\" created on {subnet}");
        Ok(sock)
    }

    fn require_psk(&self) -> Result<String> {
        self.psk
            .clone()
            .ok_or_else(|| anyhow::anyhow!("cross-host segment needs a `psk` in host config"))
    }

    /// Detach; destroys the segment when the last lab leaves.
    pub async fn detach(self: &Arc<Self>, name: &str) {
        let mut segs = self.segs.lock().await;
        if let Some(seg) = segs.get_mut(name) {
            seg.refcount = seg.refcount.saturating_sub(1);
            if seg.refcount == 0
                && let Some(seg) = segs.remove(name)
            {
                seg.listener.abort();
                if let Some(d) = seg.dialer {
                    d.abort();
                }
                seg.trunks.teardown();
                let _ = std::fs::remove_file(&seg.sock);
                tracing::info!("global segment \"{name}\" destroyed");
            }
        }
    }

    /// Segment inventory for `global.list`: name, subnet, refcount, and the
    /// live cross-host trunk state (drives the web UI's peer LED).
    pub async fn list(&self) -> Vec<serde_json::Value> {
        self.segs
            .lock()
            .await
            .iter()
            .map(|(n, s)| {
                json!({
                    "name": n,
                    "subnet": s.subnet.to_string(),
                    "refcount": s.refcount,
                    "peer_connected": s.trunks.connected(),
                    "peers": s.trunks.peers_json(),
                })
            })
            .collect()
    }

    /// Cheap pre-check for the listener: is there room for an inbound trunk
    /// from `ip` on segment `name`? (A missing segment says yes — accepting
    /// creates it.) The authoritative check is the atomic reserve inside
    /// [`accept_peer`]; this just lets the listener answer `NO\n` up front.
    pub async fn peer_slot_free(&self, name: &str, ip: IpAddr) -> bool {
        match self.segs.lock().await.get(name) {
            Some(seg) => !seg.trunks.has(ip),
            None => true,
        }
    }

    /// Accept an inbound cross-host peer trunk (after PSK auth) and bridge it
    /// onto the named global segment, creating the segment if necessary.
    /// Refuses (returns `false`) when a trunk to/from that remote host
    /// already exists — the single-slot rule that stops mutual-connect from
    /// creating a broadcast loop.
    pub async fn accept_peer(
        self: &Arc<Self>,
        name: &str,
        stream: tokio::net::TcpStream,
    ) -> Result<bool> {
        let remote = stream.peer_addr().context("peer_addr")?;
        // The inbound trunk counts as a segment reference until it dies.
        let _ = self.attach(name, None, None).await?;
        let (switch, trunks) = {
            let segs = self.segs.lock().await;
            let seg = segs.get(name).expect("just attached");
            (seg.switch.clone(), seg.trunks.clone())
        };
        tune_trunk_socket(&stream);
        let bridge = bridge_tcp_to_switch(switch, stream);
        if !trunks.try_reserve_accept(remote, bridge.abort_handle()) {
            bridge.abort();
            self.detach(name).await;
            return Ok(false);
        }
        // Waiter: on bridge death clear the slot (emitting peer.down) and
        // release the segment reference.
        let me = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let _ = bridge.await;
            trunks.clear(remote.ip());
            me.detach(&name).await;
        });
        Ok(true)
    }
}

/// Bridge a TCP stream (cross-host trunk) onto a switch via a channel port:
/// frames from the switch are written to TCP; frames from TCP are injected.
/// The returned handle completes when the trunk dies (read EOF/error), after
/// the port has been removed from the switch.
fn bridge_tcp_to_switch(
    switch: Arc<Switch>,
    stream: tokio::net::TcpStream,
) -> tokio::task::JoinHandle<()> {
    let ChannelPort { id, tx, mut rx } = switch.add_channel_port(PortClass::Service);
    let (read_half, write_half) = stream.into_split();

    // Switch → TCP.
    let mut write_half = write_half;
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write_frame(&mut write_half, &frame).await.is_err() {
                break;
            }
        }
    });

    // TCP → switch; owns the trunk lifetime.
    let sw = switch.clone();
    tokio::spawn(async move {
        let mut read_half = read_half;
        while let Ok(Some(frame)) = read_frame(&mut read_half).await {
            if tx.try_send(frame).is_err() {
                tracing::debug!("global trunk ingress queue full");
            }
        }
        sw.remove_port(id);
    })
}

/// Keepalive-tune a trunk socket so a silently dead peer surfaces as a read
/// error in ~25s (10s idle + 3×5s probes) instead of hanging forever — the
/// framed-L2 protocol itself has no heartbeat.
fn tune_trunk_socket(stream: &tokio::net::TcpStream) {
    use socket2::{SockRef, TcpKeepalive};
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    let sock = SockRef::from(stream);
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        tracing::debug!("trunk keepalive: {e}");
    }
    let _ = stream.set_nodelay(true);
}

/// `host[:port]` → a dialable address string (default trunk port appended).
fn peer_addr_string(peer: &str) -> String {
    if peer.contains(':') {
        peer.to_string()
    } else {
        format!("{peer}:{DEFAULT_TRUNK_PORT}")
    }
}

/// Sub-second jitter so two supervisors dialing each other don't retry in
/// lockstep forever (the tie-breaker for the simultaneous-dial race).
fn dial_jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis(u64::from(nanos % JITTER_MS))
}

/// Dial a remote supervisor's trunk TCP port, authenticate with the PSK, and
/// bridge the segment. The loop owns the whole trunk lifecycle: it redials
/// after failures AND after an established bridge dies (one bridge per
/// iteration — never duplicate ports), and stands down while an inbound
/// trunk from the same host is active (see the module docs' storm guard).
fn spawn_tcp_peer_dialer(
    switch: Arc<Switch>,
    peer: String,
    psk: String,
    trunks: Arc<TrunkTable>,
) -> tokio::task::JoinHandle<()> {
    let target = peer_addr_string(&peer);
    tokio::spawn(async move {
        loop {
            // Re-resolve each round (DNS may change); the resolved IP is the
            // dedupe key, so it must match what the connection will use.
            let addr = match tokio::net::lookup_host(&target).await {
                Ok(mut addrs) => match addrs.next() {
                    Some(a) => a,
                    None => {
                        tracing::warn!("cross-host trunk: {target} resolves to nothing");
                        tokio::time::sleep(REDIAL_DELAY).await;
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!("cross-host trunk: resolving {target}: {e}");
                    tokio::time::sleep(REDIAL_DELAY).await;
                    continue;
                }
            };
            if !trunks.try_reserve_dial(addr.ip()) {
                // An inbound trunk from that host is already active (or a
                // dial is in flight) — one trunk is enough.
                tokio::time::sleep(REDIAL_DELAY).await;
                continue;
            }
            match dial_peer(addr, &trunks.segment, &psk).await {
                Ok(stream) => {
                    tune_trunk_socket(&stream);
                    let bridge = bridge_tcp_to_switch(switch.clone(), stream);
                    trunks.activate_dial(addr);
                    // Wait for the bridge to die, then loop around to redial.
                    let _ = bridge.await;
                    trunks.clear(addr.ip());
                }
                Err(e) => {
                    trunks.clear(addr.ip()); // was Dialing → no down event
                    tracing::warn!(
                        "cross-host trunk to {target} for \"{}\" failed: {e:#}; retrying",
                        trunks.segment
                    );
                }
            }
            tokio::time::sleep(REDIAL_DELAY + dial_jitter()).await;
        }
    })
}

async fn dial_peer(addr: SocketAddr, segment: &str, psk: &str) -> Result<tokio::net::TcpStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connecting to peer {addr}"))?;
    // Simple PSK handshake: send a line `VMLABTRUNK1 <segment> <psk>\n`; the
    // peer replies `OK\n` or `NO\n`. No certificate machinery in v1 (§9.2).
    let hello = format!("VMLABTRUNK1 {segment} {psk}\n");
    stream.write_all(hello.as_bytes()).await?;
    let mut buf = [0u8; 3];
    stream.read_exact(&mut buf).await?;
    if &buf != b"OK\n" && &buf[..2] != b"OK" {
        bail!("peer {addr} rejected the trunk (bad PSK, or it already has this trunk)");
    }
    Ok(stream)
}

/// Bind the inbound cross-host trunk listener and serve it in a background
/// task. Binding happens before the task spawns so callers (and tests, which
/// bind `127.0.0.1:0`) see bind errors — and the real listen address.
pub async fn bind_peer_listener(
    globals: Arc<GlobalSegments>,
    bind: std::net::SocketAddr,
    psk: String,
) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cross-host trunk listener bind {bind}"))?;
    let addr = listener.local_addr()?;
    tracing::info!("cross-host trunk listener on {addr}");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let globals = globals.clone();
            let psk = psk.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                // Read the hello line.
                for _ in 0..256 {
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    if byte[0] == b'\n' {
                        break;
                    }
                    line.push(byte[0]);
                }
                let hello = String::from_utf8_lossy(&line);
                let parts: Vec<&str> = hello.split_whitespace().collect();
                if parts.len() != 3 || parts[0] != "VMLABTRUNK1" || parts[2] != psk {
                    let _ = stream.write_all(b"NO\n").await;
                    return;
                }
                let segment = parts[1].to_string();
                // Single-slot rule: refuse up front when a trunk to/from this
                // host already exists (mutual connect, duplicate dial). The
                // atomic reserve inside accept_peer catches the race window.
                if let Ok(remote) = stream.peer_addr()
                    && !globals.peer_slot_free(&segment, remote.ip()).await
                {
                    let _ = stream.write_all(b"NO\n").await;
                    return;
                }
                if stream.write_all(b"OK\n").await.is_err() {
                    return;
                }
                match globals.accept_peer(&segment, stream).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::debug!(
                            "refused duplicate trunk for \"{segment}\" (single-slot rule)"
                        );
                    }
                    Err(e) => tracing::warn!("accepting peer trunk for {segment}: {e:#}"),
                }
            });
        }
    });
    Ok((addr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::frame::{ETHERTYPE_IPV4, MAC_BROADCAST, eth_build};
    use crate::net::framing::{read_frame, write_frame};
    use bytes::Bytes;
    use tokio::time::{Duration, sleep, timeout};

    struct Instance {
        globals: Arc<GlobalSegments>,
        addr: SocketAddr,
        events: tokio::sync::broadcast::Receiver<Event>,
        _dir: tempfile::TempDir,
    }

    /// A supervisor-shaped half: GlobalSegments + a live trunk listener on
    /// 127.0.0.1:0, sockets in a private tempdir.
    async fn instance(psk: &str) -> Instance {
        let dir = tempfile::tempdir().unwrap();
        let (tx, events) = tokio::sync::broadcast::channel(64);
        let globals = GlobalSegments::new_at(
            dir.path().join("global"),
            "test.internal".into(),
            Some(psk.into()),
            tx,
        );
        let (addr, _task) =
            bind_peer_listener(globals.clone(), "127.0.0.1:0".parse().unwrap(), psk.into())
                .await
                .unwrap();
        Instance {
            globals,
            addr,
            events,
            _dir: dir,
        }
    }

    /// A lab-daemon-shaped trunk client on the segment's unix socket.
    async fn trunk_client(sock: &std::path::Path) -> tokio::net::UnixStream {
        tokio::net::UnixStream::connect(sock).await.unwrap()
    }

    fn bcast(tag: &[u8]) -> Bytes {
        Bytes::from(eth_build(
            MAC_BROADCAST,
            crate::config::model::MacAddr([0x02, 0, 0, 0, 0, 0x42]),
            ETHERTYPE_IPV4,
            tag,
        ))
    }

    async fn wait_event(rx: &mut tokio::sync::broadcast::Receiver<Event>, name: &str) -> Event {
        timeout(Duration::from_secs(10), async {
            loop {
                let ev = rx.recv().await.expect("event channel open");
                if ev.event == name {
                    return ev;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {name}"))
    }

    async fn wait_connected(g: &Arc<GlobalSegments>, name: &str, want: bool) {
        for _ in 0..200 {
            let list = g.list().await;
            let got = list
                .iter()
                .find(|e| e["name"] == name)
                .map(|e| e["peer_connected"].as_bool().unwrap_or(false))
                .unwrap_or(false);
            if got == want {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("segment {name} never reached peer_connected={want}");
    }

    #[tokio::test]
    async fn two_instances_bridge_and_reconnect() {
        let mut a = instance("s3kr1t").await;
        let mut b = instance("s3kr1t").await;

        // A dials B; B's listener creates its side of the segment on accept.
        let a_sock = a
            .globals
            .attach("wan", None, Some(format!("127.0.0.1:{}", b.addr.port())))
            .await
            .unwrap();
        let up_a = wait_event(&mut a.events, "segment.peer.up").await;
        assert_eq!(up_a.data["segment"], "wan");
        assert_eq!(up_a.data["direction"], "dial");
        let up_b = wait_event(&mut b.events, "segment.peer.up").await;
        assert_eq!(up_b.data["direction"], "accept");
        wait_connected(&a.globals, "wan", true).await;
        wait_connected(&b.globals, "wan", true).await;
        let listed = a.globals.list().await;
        assert_eq!(listed[0]["peers"][0]["direction"], "dial");

        // L2 passthrough: a broadcast injected on A's trunk socket arrives at
        // a client on B's (and only once).
        let b_sock = {
            let segs = b.globals.segs.lock().await;
            segs.get("wan")
                .expect("accept created the segment")
                .sock
                .clone()
        };
        let mut a_client = trunk_client(&a_sock).await;
        let mut b_client = trunk_client(&b_sock).await;
        let f = bcast(b"hello-b");
        write_frame(&mut a_client, &f).await.unwrap();
        let got = timeout(Duration::from_secs(5), read_frame(&mut b_client))
            .await
            .expect("frame crossed the trunk")
            .unwrap()
            .expect("stream open");
        assert_eq!(got, f);
        // …and back the other way.
        let f2 = bcast(b"hello-a");
        write_frame(&mut b_client, &f2).await.unwrap();
        let got2 = timeout(Duration::from_secs(5), read_frame(&mut a_client))
            .await
            .expect("frame crossed back")
            .unwrap()
            .expect("stream open");
        assert_eq!(got2, f2);

        // Kill B's side entirely: A must emit down and then redial into the
        // fresh accept, coming back up without duplicate ports.
        b.globals.detach("wan").await;
        wait_event(&mut a.events, "segment.peer.down").await;
        wait_event(&mut a.events, "segment.peer.up").await;
        wait_connected(&a.globals, "wan", true).await;
        wait_connected(&b.globals, "wan", true).await;
    }

    #[tokio::test]
    async fn mutual_dial_converges_to_one_trunk() {
        let mut a = instance("s3kr1t").await;
        let mut b = instance("s3kr1t").await;

        // Both sides declare connect at each other — exactly what the web
        // UI's remote-vmlab node writes on both canvases.
        let a_sock = a
            .globals
            .attach("wan", None, Some(format!("127.0.0.1:{}", b.addr.port())))
            .await
            .unwrap();
        let _ = b
            .globals
            .attach("wan", None, Some(format!("127.0.0.1:{}", a.addr.port())))
            .await
            .unwrap();
        wait_event(&mut a.events, "segment.peer.up").await;
        wait_event(&mut b.events, "segment.peer.up").await;
        // Let the losing dialer take a few more swings at the storm guard.
        sleep(Duration::from_millis(600)).await;

        // Exactly one active trunk per side.
        for g in [&a.globals, &b.globals] {
            let list = g.list().await;
            let peers = list[0]["peers"].as_array().unwrap();
            assert_eq!(peers.len(), 1, "single-slot rule violated: {list:?}");
        }

        // The storm test: a broadcast from A arrives at B exactly once (two
        // parallel trunks would loop it forever).
        let b_sock = {
            let segs = b.globals.segs.lock().await;
            segs.get("wan").unwrap().sock.clone()
        };
        let mut a_client = trunk_client(&a_sock).await;
        let mut b_client = trunk_client(&b_sock).await;
        let f = bcast(b"once");
        write_frame(&mut a_client, &f).await.unwrap();
        let got = timeout(Duration::from_secs(5), read_frame(&mut b_client))
            .await
            .expect("broadcast crossed")
            .unwrap()
            .expect("stream open");
        assert_eq!(got, f);
        let extra = timeout(Duration::from_millis(400), read_frame(&mut b_client)).await;
        assert!(
            extra.is_err(),
            "broadcast duplicated across parallel trunks"
        );
    }

    #[tokio::test]
    async fn bad_psk_rejected() {
        let mut a = instance("right").await;
        let b = instance("wrong").await;
        let _ = a
            .globals
            .attach("wan", None, Some(format!("127.0.0.1:{}", b.addr.port())))
            .await
            .unwrap();
        // The dial must never come up; give it a few retry rounds.
        let up = timeout(Duration::from_millis(800), async {
            wait_event(&mut a.events, "segment.peer.up").await
        })
        .await;
        assert!(up.is_err(), "trunk came up despite a PSK mismatch");
        let list = b.globals.list().await;
        assert!(list.is_empty(), "rejected peer still created the segment");
    }
}
