//! Port forwarding (PRD §9.8): host listeners proxied into guests through
//! the NAT engine's active-open primitives. Works without privileges for
//! any host port the daemon user may bind.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::NatEngine;

/// Spawners tying host sockets to guest endpoints via a [`NatEngine`].
pub struct PortForwarder;

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
    /// inside a task nobody awaits.
    pub fn spawn_tcp_forward(
        listen: SocketAddr,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> std::io::Result<JoinHandle<()>> {
        let listener = TcpListener::from_std(bind_tcp(listen)?)?;
        Ok(tokio::spawn(Self::serve_tcp(
            listener, engine, guest_ip, guest_port,
        )))
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
    ) -> std::io::Result<JoinHandle<()>> {
        let sock = std::net::UdpSocket::bind(listen)?;
        sock.set_nonblocking(true)?;
        let sock = UdpSocket::from_std(sock)?;
        Ok(tokio::spawn(Self::serve_udp(
            sock, engine, guest_ip, guest_port,
        )))
    }

    /// As [`Self::spawn_udp_forward`] but on an already-bound socket.
    pub async fn serve_udp(
        sock: UdpSocket,
        engine: Arc<NatEngine>,
        guest_ip: Ipv4Addr,
        guest_port: u16,
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
                    let (to_guest, mut from_guest) =
                        engine.udp_bind_guest_flow(guest_ip, guest_port);
                    let sock = sock.clone();
                    tokio::spawn(async move {
                        while let Some(payload) = from_guest.recv().await {
                            let _ = sock.send_to(&payload, from).await;
                        }
                    });
                    remotes.insert(from, to_guest.clone());
                    to_guest
                }
            };
            let _ = tx.send(buf[..n].to_vec()).await;
        }
    }
}
