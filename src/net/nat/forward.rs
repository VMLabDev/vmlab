//! Port forwarding (PRD §9.8): host listeners proxied into guests through
//! the NAT engine's active-open primitives. Works without privileges for
//! any host port the daemon user may bind.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::NatEngine;

/// Spawners tying host sockets to guest endpoints via a [`NatEngine`].
pub struct PortForwarder;

/// A UDP forward's remote peer that sends nothing and gets nothing back for
/// this long is dropped, with its engine flow and gateway port. The same
/// expiry as an outbound UDP flow.
const UDP_PEER_IDLE: Duration = Duration::from_secs(60);

/// A non-blocking TCP listener on `listen`, bound the way
/// [`TcpListener::bind`] binds one (`SO_REUSEADDR`, so a port whose last
/// connections sit in `TIME_WAIT` can be listened on again) but
/// synchronously, so the caller learns whether it got the port.
fn bind_tcp(listen: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(
        Domain::for_address(listen),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&listen.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

impl PortForwarder {
    /// Listen on `listen`; every accepted TCP connection becomes a vTCP
    /// active open to `guest_ip:guest_port` with bytes copied both ways.
    ///
    /// The bind happens here, before the task is spawned, so a port that is
    /// already taken is the caller's error to report rather than a warning
    /// inside a task nobody awaits. Returns the address actually bound,
    /// which differs from `listen` when it asks for port 0.
    pub fn spawn_tcp_forward(
        listen: SocketAddr,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> std::io::Result<(JoinHandle<()>, SocketAddr)> {
        let listener = TcpListener::from_std(bind_tcp(listen)?)?;
        let bound = listener.local_addr()?;
        let handle = tokio::spawn(Self::serve_tcp(listener, engine, guest_ip, guest_port));
        Ok((handle, bound))
    }

    /// As [`Self::spawn_tcp_forward`] but on an already-bound listener
    /// (lets callers bind port 0 and learn the port first).
    pub async fn serve_tcp(
        listener: TcpListener,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) {
        info!(addr = ?listener.local_addr().ok(), %guest_ip, guest_port, "tcp forward up");
        loop {
            let (mut sock, peer) = match listener.accept().await {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "tcp forward: accept failed");
                    continue;
                }
            };
            let engine = engine.clone();
            tokio::spawn(async move {
                match engine.open_tcp_to_guest(guest_ip, guest_port).await {
                    Ok(mut guest) => {
                        let _ = tokio::io::copy_bidirectional(&mut sock, &mut guest).await;
                    }
                    Err(e) => {
                        debug!(%peer, error = %e, "tcp forward: guest open failed");
                        // Dropping `sock` closes the host side.
                    }
                }
            });
        }
    }

    /// Listen for UDP on `listen`; each distinct remote source address is
    /// mapped to its own engine-side guest flow so replies return to the
    /// right peer. Binds before spawning, as [`Self::spawn_tcp_forward`].
    pub fn spawn_udp_forward(
        listen: SocketAddr,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> std::io::Result<(JoinHandle<()>, SocketAddr)> {
        let sock = std::net::UdpSocket::bind(listen)?;
        sock.set_nonblocking(true)?;
        let sock = UdpSocket::from_std(sock)?;
        let bound = sock.local_addr()?;
        let handle = tokio::spawn(Self::serve_udp(sock, engine, guest_ip, guest_port));
        Ok((handle, bound))
    }

    /// As [`Self::spawn_udp_forward`] but on an already-bound socket.
    pub async fn serve_udp(
        sock: UdpSocket,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) {
        Self::serve_udp_with_idle(sock, engine, guest_ip, guest_port, UDP_PEER_IDLE).await
    }

    /// [`Self::serve_udp`] with the peer idle expiry given, so a test need
    /// not wait a minute.
    ///
    /// Each remote peer gets one task that carries both directions over its
    /// own engine flow and ends after `idle` without traffic either way.
    /// Ending drops the flow, which frees its gateway port, so peers that
    /// come and go do not exhaust the ephemeral range.
    pub(super) async fn serve_udp_with_idle(
        sock: UdpSocket,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
        idle: Duration,
    ) {
        info!(addr = ?sock.local_addr().ok(), %guest_ip, guest_port, "udp forward up");
        let sock = Arc::new(sock);
        let mut remotes: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>> = HashMap::new();
        let mut buf = vec![0u8; 65535];
        loop {
            let (n, from) = match sock.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "udp forward: recv failed");
                    continue;
                }
            };
            let tx = match remotes.get(&from) {
                Some(tx) if !tx.is_closed() => tx.clone(),
                _ => {
                    // Forget the peers whose task has ended before adding one.
                    remotes.retain(|_, tx| !tx.is_closed());
                    let (to_guest, from_guest) =
                        match engine.udp_bind_guest_flow(guest_ip, guest_port) {
                            Ok(flow) => flow,
                            Err(e) => {
                                warn!(%from, error = %e, "udp forward: datagram dropped");
                                continue;
                            }
                        };
                    let (to_peer, from_peer) = mpsc::channel(64);
                    tokio::spawn(Self::udp_peer(
                        sock.clone(),
                        from,
                        from_peer,
                        to_guest,
                        from_guest,
                        idle,
                    ));
                    remotes.insert(from, to_peer.clone());
                    to_peer
                }
            };
            let _ = tx.send(buf[..n].to_vec()).await;
        }
    }

    /// One remote peer of a UDP forward: its datagrams to the guest, the
    /// guest's replies back to it, until `idle` passes with neither.
    async fn udp_peer(
        sock: Arc<UdpSocket>,
        peer: SocketAddr,
        mut from_peer: mpsc::Receiver<Vec<u8>>,
        to_guest: mpsc::Sender<Vec<u8>>,
        mut from_guest: mpsc::Receiver<Vec<u8>>,
        idle: Duration,
    ) {
        loop {
            tokio::select! {
                got = from_peer.recv() => match got {
                    Some(payload) => {
                        if to_guest.send(payload).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                got = from_guest.recv() => match got {
                    Some(payload) => {
                        let _ = sock.send_to(&payload, peer).await;
                    }
                    None => break,
                },
                () = tokio::time::sleep(idle) => {
                    debug!(%peer, "udp forward: peer idle, expiring");
                    break;
                }
            }
        }
    }
}
