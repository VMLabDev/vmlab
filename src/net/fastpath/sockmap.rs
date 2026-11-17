//! The sockmap fast-path tier: sk_skb verdict programs splice known
//! guest→guest unicast between the existing QEMU stream sockets in-kernel;
//! everything with semantic weight (broadcast, gateway-bound, unknown MACs,
//! isolated ports) `SK_PASS`es into the switch's ordinary reader tasks.
//!
//! One [`Engine`] (a fresh program + map load) exists per switch, so
//! cross-segment leakage is structurally impossible: a program instance can
//! only redirect into its own load's maps.
//!
//! # The single-writer invariant
//!
//! A kernel egress redirect transmits via the target socket's psock
//! backlog. If the userspace writer task also wrote that socket, the two
//! writers would interleave mid-frame and corrupt the stream. So once a
//! port is offloaded, only the kernel writes its QEMU socket: userspace
//! egress goes through a per-port `SOCK_DGRAM` loopback pair whose kernel
//! end sits in `TX_HASH` with a trivial redirect verdict — dgram sends are
//! atomic, and the backlog serializes all sources.
//!
//! The programs and maps are held by the `vmlab-fastpath` helper
//! (`helper/`), which alone has CAP_BPF and CAP_NET_ADMIN; [`Engine`] is the
//! daemon's handle on one loaded object there.

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::net::UnixDatagram;
use tokio::net::unix::OwnedWriteHalf;
use tracing::{debug, warn};

pub(super) use super::helper::client::RemoteSockmap as Engine;
use crate::config::model::MacAddr;
use crate::net::switch::PortId;
use crate::sync::LockRecover;

/// Egress handle for one offloaded port: each frame goes out as a single
/// atomic dgram (`[4-byte BE length][frame]`) that the kernel splices into
/// the QEMU socket.
pub struct PortTx {
    tx_user: Arc<UnixDatagram>,
}

impl PortTx {
    pub async fn send_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(4 + frame.len());
        buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        buf.extend_from_slice(frame);
        self.tx_user.send(&buf).await.map(|_| ())
    }
}

struct OffloadPort {
    tx_cookie: u64,
    /// Keeps the kernel end of the TX loopback (and its sockmap entry)
    /// alive for the port's lifetime.
    _tx_kernel: UnixDatagram,
    /// Parked, never written: the kernel owns this socket's send path.
    /// Dropping it on removal is what EOFs QEMU (matching the userspace
    /// path's teardown semantics).
    write_half: Option<OwnedWriteHalf>,
}

/// Per-switch sockmap offload state. `None` (from [`Self::for_segment`])
/// whenever the daemon's fast-path tier isn't sockmap — the switch then
/// runs today's pure-userspace path with zero behavioural change.
pub struct SegmentOffload {
    engine: Engine,
    ports: Mutex<std::collections::HashMap<u64, OffloadPort>>,
}

impl SegmentOffload {
    pub fn for_segment(name: &str) -> Option<Arc<SegmentOffload>> {
        if super::tier() != super::FastpathTier::Sockmap {
            return None;
        }
        match Engine::load() {
            Ok(engine) => Some(Arc::new(SegmentOffload {
                engine,
                ports: Mutex::new(std::collections::HashMap::new()),
            })),
            Err(error) => {
                // The tier probe passed at startup, so this is unexpected —
                // but never fatal: the segment simply stays userspace.
                warn!(
                    segment = name,
                    "sockmap offload unavailable ({error:#}); using userspace switching"
                );
                None
            }
        }
    }

    /// Register an accepted QEMU stream socket for kernel splicing and
    /// return the egress handle its writer task must use instead of the
    /// socket. Call before any bytes flow (VMs start paused until their
    /// ports attach, so the socket is still silent here — the BPF framing
    /// state machine starts at a true frame boundary).
    pub fn add_port(&self, id: PortId, stream: &tokio::net::UnixStream) -> Result<PortTx> {
        let pid = id.raw();
        self.engine.register_stream(pid, stream)?;
        let (tx_user, tx_kernel) = UnixDatagram::pair().context("tx loopback pair")?;
        let tx_cookie = match self.engine.register_tx(pid, &tx_kernel, &tx_user) {
            Ok(c) => c,
            Err(e) => {
                // Never leave a half-offloaded port behind: stream-redirect
                // without the TX loopback would reintroduce the two-writer
                // corruption this design exists to prevent.
                self.engine.unregister(pid, 0, &[]);
                return Err(e);
            }
        };
        let tx_user = Arc::new(tx_user);
        self.ports.lock_recover().insert(
            pid,
            OffloadPort {
                tx_cookie,
                _tx_kernel: tx_kernel,
                write_half: None,
            },
        );
        debug!(port = %id, "port offloaded to sockmap fast path");
        Ok(PortTx { tx_user })
    }

    /// Park the socket's write half for the port's lifetime (see
    /// [`OffloadPort::write_half`]).
    pub fn adopt_write_half(&self, id: PortId, half: OwnedWriteHalf) {
        if let Some(port) = self.ports.lock_recover().get_mut(&id.raw()) {
            port.write_half = Some(half);
        }
    }

    /// Mirror a userspace MAC-table change. `port` is the MAC's new home:
    /// if it is an offloaded port the kernel entry follows it; otherwise
    /// (service port, isolated guest, plain stream fallback) any stale
    /// kernel entry is removed so those frames keep passing to userspace.
    pub fn relearn(&self, mac: MacAddr, port: PortId) {
        let offloaded = self.ports.lock_recover().contains_key(&port.raw());
        if offloaded {
            if let Err(error) = self.engine.insert_mac(mac.0, port.raw()) {
                debug!(%port, %error, "mac_map insert failed");
            }
        } else {
            self.engine.remove_mac(mac.0);
        }
    }

    /// Tear down a port's kernel state and drop its parked write half
    /// (which EOFs QEMU). `purged` is the MACs the switch just unlearned.
    pub fn remove_port(&self, id: PortId, purged: &[MacAddr]) {
        let Some(port) = self.ports.lock_recover().remove(&id.raw()) else {
            // Not offloaded (service port, fallback port): still scrub any
            // MACs that might have pointed at it defensively.
            for mac in purged {
                self.engine.remove_mac(mac.0);
            }
            return;
        };
        let macs: Vec<[u8; 6]> = purged.iter().map(|m| m.0).collect();
        self.engine.unregister(id.raw(), port.tx_cookie, &macs);
        debug!(port = %id, "offloaded port removed");
    }

    /// Kernel-forwarded (frames, bytes) for this segment.
    pub fn stats(&self) -> (u64, u64) {
        self.engine.stats()
    }
}
