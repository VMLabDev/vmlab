//! Outbound UDP NAT: one connected, ephemeral host socket per guest flow
//! (guest_ip, guest_port, dst_ip, dst_port), a reader task translating
//! replies back into frames, 60 s idle expiry. DNS to arbitrary servers
//! works through this with zero configuration.

use crate::sync::LockRecover;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout};
use tracing::{debug, trace};

use crate::config::model::MacAddr;

use super::{FlowKey, NatEngine};

/// Flows idle longer than this are torn down.
const UDP_IDLE: Duration = Duration::from_secs(60);

/// How many outbound UDP flows the engine holds at once (review F3): each
/// is a host socket, a task and a 64 KiB buffer. A new flow past the limit
/// replaces the one idle the longest, so a table full of abandoned flows
/// never refuses a live one.
pub(super) const MAX_UDP_FLOWS: usize = 256;
/// How many of [`MAX_UDP_FLOWS`] one guest address may hold. Past it a new
/// flow replaces that guest's own oldest, not another guest's.
pub(super) const MAX_UDP_FLOWS_PER_GUEST: usize = 128;

/// A live outbound UDP flow.
#[derive(Clone)]
pub(super) struct UdpFlow {
    sock: Arc<UdpSocket>,
    last_used: Arc<StdMutex<Instant>>,
}

/// Handle one guest→world datagram: reuse or create the flow socket and
/// forward the payload.
pub(super) async fn handle_outbound(
    engine: &Arc<NatEngine>,
    key: FlowKey,
    guest_mac: MacAddr,
    payload: &[u8],
) {
    let existing = engine.udp_flows.lock_recover().get(&key).cloned();
    let flow = match existing {
        Some(f) => f,
        None => {
            let sock = match UdpSocket::bind(("0.0.0.0", 0)).await {
                Ok(s) => s,
                Err(e) => {
                    debug!(?key, error = %e, "nat: udp bind failed");
                    return;
                }
            };
            if let Err(e) = sock.connect((key.dst_ip, key.dst_port)).await {
                debug!(?key, error = %e, "nat: udp connect failed");
                return;
            }
            let flow = UdpFlow {
                sock: Arc::new(sock),
                last_used: Arc::new(StdMutex::new(Instant::now())),
            };
            engine.udp_flows.lock_recover().insert(key, flow.clone());
            spawn_reader(engine.clone(), key, guest_mac, flow.clone());
            trace!(?key, "nat: udp flow created");
            flow
        }
    };
    *flow.last_used.lock_recover() = Instant::now();
    let _ = flow.sock.send(payload).await;
}

/// Reader task: world→guest datagrams become frames; exits (and removes the
/// flow) after 60 s without traffic in either direction.
fn spawn_reader(engine: Arc<NatEngine>, key: FlowKey, guest_mac: MacAddr, flow: UdpFlow) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            match timeout(UDP_IDLE, flow.sock.recv(&mut buf)).await {
                Ok(Ok(n)) => {
                    *flow.last_used.lock_recover() = Instant::now();
                    engine
                        .send_udp_to_guest(
                            guest_mac,
                            (key.dst_ip, key.dst_port),
                            (key.guest_ip, key.guest_port),
                            &buf[..n],
                        )
                        .await;
                }
                Ok(Err(e)) => {
                    // ICMP errors surface here on connected sockets
                    // (e.g. port unreachable); just drop the flow.
                    debug!(?key, error = %e, "nat: udp recv error");
                    break;
                }
                Err(_elapsed) => {
                    let idle = flow.last_used.lock_recover().elapsed();
                    if idle >= UDP_IDLE {
                        trace!(?key, "nat: udp flow idle, expiring");
                        break;
                    }
                }
            }
        }
        engine.udp_flows.lock_recover().remove(&key);
    });
}
