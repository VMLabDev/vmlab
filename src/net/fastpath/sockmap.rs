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

use std::os::fd::{AsFd, AsRawFd as _};
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use aya::maps::{HashMap as BpfHashMap, MapData, PerCpuArray, SockHash};
use aya::programs::SkSkb;
use tokio::net::UnixDatagram;
use tokio::net::unix::OwnedWriteHalf;
use tracing::{debug, warn};

use crate::config::model::MacAddr;
use crate::net::switch::PortId;
use crate::sync::LockRecover;

const OBJ: &[u8] = include_bytes!("bpf/fastpath_sockmap.bpf.o");

/// `SOCK_STATE` map value: the BPF side's `SockState` (repr(C): `u64`
/// port_id, then the 12-byte framing state, padded to 24). The kernel
/// self-primes entries (keyed by the *sender* cookie, which userspace
/// cannot query for QEMU's sockets); userspace only reads the leading
/// port id when scanning entries to delete on port removal — so a byte
/// array avoids needing an `unsafe impl aya::Pod` in this crate.
const SOCK_STATE_LEN: usize = 24;

fn socket_cookie(fd: impl AsFd) -> Result<u64> {
    rustix::net::sockopt::socket_cookie(fd).context("SO_COOKIE")
}

/// One loaded fastpath_sockmap object: programs attached, typed map handles
/// taken. Shared by [`SegmentOffload`] and the startup probe.
pub(super) struct Engine {
    /// Owns the loaded programs (and thus their attachments); maps have
    /// been taken out into the typed handles below.
    _ebpf: aya::Ebpf,
    maps: Mutex<Maps>,
}

struct Maps {
    port_hash: SockHash<MapData, u64>,
    tx_hash: SockHash<MapData, u64>,
    mac_map: BpfHashMap<MapData, [u8; 6], u64>,
    sock_state: BpfHashMap<MapData, u64, [u8; SOCK_STATE_LEN]>,
    tx_target: BpfHashMap<MapData, u64, u64>,
    fp_stats: PerCpuArray<MapData, [u64; 2]>,
    fp_debug: PerCpuArray<MapData, u64>,
}

impl Engine {
    /// Load the embedded object and attach both verdict programs. Needs
    /// CAP_BPF + CAP_NET_ADMIN; any failure (perms, verifier, old kernel)
    /// surfaces here.
    pub(super) fn load() -> Result<Engine> {
        let mut ebpf = aya::Ebpf::load(OBJ).context("loading fastpath_sockmap.bpf.o")?;
        let mut take = |name: &str| -> Result<aya::maps::Map> {
            ebpf.take_map(name)
                .ok_or_else(|| anyhow!("map {name} missing from object"))
        };
        let port_hash = SockHash::try_from(take("PORT_HASH")?)?;
        let tx_hash = SockHash::try_from(take("TX_HASH")?)?;
        let mac_map = BpfHashMap::try_from(take("MAC_MAP")?)?;
        let sock_state = BpfHashMap::try_from(take("SOCK_STATE")?)?;
        let tx_target = BpfHashMap::try_from(take("TX_TARGET")?)?;
        let fp_stats = PerCpuArray::try_from(take("FP_STATS")?)?;
        let fp_debug = PerCpuArray::try_from(take("FP_DEBUG")?)?;

        let attach = |ebpf: &mut aya::Ebpf, name: &str, fd| -> Result<()> {
            let prog: &mut SkSkb = ebpf
                .program_mut(name)
                .ok_or_else(|| anyhow!("program {name} missing from object"))?
                .try_into()?;
            prog.load().with_context(|| format!("loading {name}"))?;
            prog.attach(fd)
                .with_context(|| format!("attaching {name}"))?;
            Ok(())
        };
        attach(&mut ebpf, "verdict_guest", port_hash.fd())?;
        attach(&mut ebpf, "verdict_tx", tx_hash.fd())?;

        Ok(Engine {
            _ebpf: ebpf,
            maps: Mutex::new(Maps {
                port_hash,
                tx_hash,
                mac_map,
                sock_state,
                tx_target,
                fp_stats,
                fp_debug,
            }),
        })
    }

    /// Register a QEMU-facing stream socket as offloaded port `id`. The
    /// per-stream state is self-primed by the verdict program (keyed by
    /// the sender cookie only the kernel sees), so this is just the
    /// sockmap membership that makes the verdict run at all.
    pub(super) fn register_stream(&self, id: u64, sock: impl AsFd) -> Result<()> {
        let mut maps = self.maps.lock_recover();
        maps.port_hash
            .insert(id, sock.as_fd().as_raw_fd(), 0)
            .context("port_hash insert")?;
        Ok(())
    }

    /// Register a TX loopback: `map_sock` (the kernel end) joins the
    /// sockmap so the TX verdict runs on its ingress; `cookie_sock` (the
    /// `tx_user` end the daemon writes) is the *sender* the kernel
    /// attributes those dgrams to, hence the TX_TARGET key.
    pub(super) fn register_tx(
        &self,
        id: u64,
        map_sock: impl AsFd,
        cookie_sock: impl AsFd,
    ) -> Result<u64> {
        let cookie = socket_cookie(cookie_sock.as_fd())?;
        let mut maps = self.maps.lock_recover();
        maps.tx_target
            .insert(cookie, id, 0)
            .context("tx_target insert")?;
        if let Err(e) = maps.tx_hash.insert(id, map_sock.as_fd().as_raw_fd(), 0) {
            let _ = maps.tx_target.remove(&cookie);
            return Err(anyhow::Error::new(e).context("tx_hash insert"));
        }
        Ok(cookie)
    }

    pub(super) fn insert_mac(&self, mac: [u8; 6], id: u64) -> Result<()> {
        Ok(self.maps.lock_recover().mac_map.insert(mac, id, 0)?)
    }

    pub(super) fn remove_mac(&self, mac: [u8; 6]) {
        // Absent is fine: most MACs never belonged to an offloaded port.
        let _ = self.maps.lock_recover().mac_map.remove(&mac);
    }

    /// Best-effort cleanup of everything a port registered. Closed sockets
    /// are auto-removed by the kernel, so failures here are non-events.
    /// SOCK_STATE entries are keyed by cookies only the kernel saw, so
    /// they're found by scanning for the port id in the value.
    pub(super) fn unregister(&self, id: u64, tx_cookie: u64, macs: &[[u8; 6]]) {
        let mut maps = self.maps.lock_recover();
        let _ = maps.port_hash.remove(&id);
        let _ = maps.tx_hash.remove(&id);
        let _ = maps.tx_target.remove(&tx_cookie);
        for mac in macs {
            let _ = maps.mac_map.remove(mac);
        }
        let stale: Vec<u64> = maps
            .sock_state
            .iter()
            .filter_map(|entry| entry.ok())
            .filter(|(_, v)| v[..8] == id.to_ne_bytes())
            .map(|(k, _)| k)
            .collect();
        for cookie in stale {
            let _ = maps.sock_state.remove(&cookie);
        }
    }

    /// Kernel-forwarded (frames, bytes), summed across CPUs.
    pub(super) fn stats(&self) -> (u64, u64) {
        let maps = self.maps.lock_recover();
        match maps.fp_stats.get(&0, 0) {
            Ok(values) => values.iter().fold((0, 0), |(f, b), v| (f + v[0], b + v[1])),
            Err(_) => (0, 0),
        }
    }

    /// The verdict programs' branch counters (see FP_DEBUG in the BPF
    /// source), summed across CPUs — probe/field diagnosis only.
    pub(super) fn debug_counters(&self) -> String {
        let maps = self.maps.lock_recover();
        let read = |i: u32| -> u64 {
            maps.fp_debug
                .get(&i, 0)
                .map(|v| v.iter().sum())
                .unwrap_or(0)
        };
        format!(
            "invoked={} no_state={} passthrough={} aligned={} classify_pass={} \
             redirect={} redirect_drop={} tx_invoked={}",
            read(0),
            read(1),
            read(2),
            read(3),
            read(4),
            read(5),
            read(6),
            read(7),
        )
    }
}

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
