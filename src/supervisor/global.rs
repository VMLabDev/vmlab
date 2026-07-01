//! Supervisor-owned global segments (PRD §9.2): shared L2 switches that
//! span labs (and hosts). Created on first attach, destroyed on last detach.
//! The supervisor runs the shared segment's DHCP/DNS so registrations span
//! labs coherently. Lab daemons attach via segment trunks (unix sockets);
//! the *same* frame-forwarding trunk protocol over TCP bridges two
//! supervisors for cross-host segments — one mechanism, two transports.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ipnet::Ipv4Net;
use tokio::sync::Mutex;

use crate::net::dhcp::DhcpConfig;
use crate::net::dns::DnsZone;
use crate::net::framing::{read_frame, write_frame};
use crate::net::gateway::{Gateway, GatewayConfig, GatewayHandle, gateway_mac};
use crate::net::switch::{ChannelPort, PortClass, Switch};

/// Global segments use a distinct pool from per-lab segments to avoid
/// collisions when both appear on one host.
const GLOBAL_POOL: &str = "10.214.0.0/16";

struct GlobalSeg {
    switch: Arc<Switch>,
    #[allow(dead_code)]
    gateway: GatewayHandle,
    subnet: Ipv4Net,
    refcount: usize,
    sock: PathBuf,
    listener: tokio::task::JoinHandle<()>,
    /// Cross-host peer bridges (host:port → task).
    peers: Vec<tokio::task::JoinHandle<()>>,
}

pub struct GlobalSegments {
    segs: Mutex<HashMap<String, GlobalSeg>>,
    next_index: Mutex<u32>,
    dns_suffix: String,
    psk: Option<String>,
}

impl GlobalSegments {
    pub fn new(dns_suffix: String, psk: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            segs: Mutex::new(HashMap::new()),
            next_index: Mutex::new(0),
            dns_suffix,
            psk,
        })
    }

    fn global_dir() -> PathBuf {
        crate::paths::runtime_dir().join("global")
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

        let dir = Self::global_dir();
        std::fs::create_dir_all(&dir)?;
        let sock = dir.join(format!("{name}.sock"));
        let listener = switch
            .listen_unix(&sock, PortClass::Service)
            .await
            .with_context(|| format!("listening on {}", sock.display()))?;

        let mut peers = Vec::new();
        if let Some(peer) = peer {
            // Cross-host: dial the remote supervisor and bridge over TCP.
            let psk = self.psk.clone().ok_or_else(|| {
                anyhow::anyhow!("cross-host segment needs a `psk` in host config")
            })?;
            peers.push(spawn_tcp_peer_dialer(
                switch.clone(),
                peer,
                name.to_string(),
                psk,
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
                peers,
            },
        );
        tracing::info!("global segment \"{name}\" created on {subnet}");
        Ok(sock)
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
                for p in seg.peers {
                    p.abort();
                }
                let _ = std::fs::remove_file(&seg.sock);
                tracing::info!("global segment \"{name}\" destroyed");
            }
        }
    }

    pub async fn list(&self) -> Vec<(String, String, usize)> {
        self.segs
            .lock()
            .await
            .iter()
            .map(|(n, s)| (n.clone(), s.subnet.to_string(), s.refcount))
            .collect()
    }

    /// Accept an inbound cross-host peer trunk (after PSK auth) and bridge it
    /// onto the named global segment, creating the segment if necessary.
    pub async fn accept_peer(
        self: &Arc<Self>,
        name: &str,
        stream: tokio::net::TcpStream,
    ) -> Result<()> {
        // Ensure the segment exists (peer attach counts as a reference).
        let _ = self.attach(name, None, None).await?;
        let segs = self.segs.lock().await;
        let seg = segs.get(name).expect("just attached");
        bridge_tcp_to_switch(seg.switch.clone(), stream);
        Ok(())
    }
}

/// Bridge a TCP stream (cross-host trunk) onto a switch via a channel port:
/// frames from the switch are written to TCP; frames from TCP are injected.
fn bridge_tcp_to_switch(switch: Arc<Switch>, stream: tokio::net::TcpStream) {
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

    // TCP → switch.
    let sw = switch.clone();
    tokio::spawn(async move {
        let mut read_half = read_half;
        while let Ok(Some(frame)) = read_frame(&mut read_half).await {
            if tx.try_send(frame).is_err() {
                tracing::debug!("global trunk ingress queue full");
            }
        }
        sw.remove_port(id);
    });
}

/// Dial a remote supervisor's trunk TCP port, authenticate with the PSK, and
/// bridge the segment. Reconnects on failure.
fn spawn_tcp_peer_dialer(
    switch: Arc<Switch>,
    peer: String,
    segment: String,
    psk: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match dial_peer(&peer, &segment, &psk).await {
                Ok(stream) => {
                    tracing::info!("cross-host trunk to {peer} for \"{segment}\" up");
                    bridge_tcp_to_switch(switch.clone(), stream);
                    // bridge_tcp_to_switch owns the stream; wait before any
                    // reconnect attempt (it returns immediately, spawning
                    // tasks). Sleep long; if the bridge dies the port is
                    // removed but we don't auto-reconnect here to avoid
                    // duplicate ports — a future enhancement.
                    return;
                }
                Err(e) => {
                    tracing::warn!("cross-host trunk to {peer} failed: {e}; retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

async fn dial_peer(peer: &str, segment: &str, psk: &str) -> Result<tokio::net::TcpStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(peer)
        .await
        .with_context(|| format!("connecting to peer {peer}"))?;
    // Simple PSK handshake: send a line `VMLABTRUNK1 <segment> <psk>\n`; the
    // peer replies `OK\n` or `NO\n`. No certificate machinery in v1 (§9.2).
    let hello = format!("VMLABTRUNK1 {segment} {psk}\n");
    stream.write_all(hello.as_bytes()).await?;
    let mut buf = [0u8; 3];
    stream.read_exact(&mut buf).await?;
    if &buf != b"OK\n" && &buf[..2] != b"OK" {
        bail!("peer {peer} rejected the trunk (bad PSK or segment)");
    }
    Ok(stream)
}

/// Listen for inbound cross-host peer trunks. Called by the supervisor at
/// startup when a trunk port is configured.
pub fn spawn_peer_listener(
    globals: Arc<GlobalSegments>,
    bind: std::net::SocketAddr,
    psk: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("cross-host trunk listener bind {bind} failed: {e}");
                return;
            }
        };
        tracing::info!("cross-host trunk listener on {bind}");
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
                if stream.write_all(b"OK\n").await.is_err() {
                    return;
                }
                if let Err(e) = globals.accept_peer(&segment, stream).await {
                    tracing::warn!("accepting peer trunk for {segment}: {e}");
                }
            });
        }
    })
}
