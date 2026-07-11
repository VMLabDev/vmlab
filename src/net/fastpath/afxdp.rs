//! The afxdp fast-path tier: VM NICs ride tap devices (QEMU `-netdev
//! tap,fd=`) and a per-segment XDP program (SKB/generic mode — native XDP on
//! tun silently bypasses large frames) forwards known non-isolated unicast
//! tap-to-tap in-kernel. Everything with semantic weight punts to the daemon
//! as `[4-byte BE tag][frame]` over a host tap, entering the userspace
//! switch through an ordinary channel port — so learning, flooding,
//! isolation, the L3-rules hook, gateway/DHCP/DNS/NAT, and trunks all run
//! unchanged. Daemon egress to a tap NIC takes the same tagged path back.
//!
//! The daemon is the sole map writer: `MAC_TABLE` is a static projection of
//! the configured (persisted) NIC MACs, never a learning cache. A guest
//! using an unconfigured MAC just punts — identical semantics, slower path.
//!
//! Tap lifetime is fd lifetime: the daemon holds one queue fd per NIC and
//! QEMU a dup of it, so the device vanishes when both are gone — VM stop,
//! daemon crash + orphan reap, whatever order. Nothing to garbage-collect.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use aya::maps::{HashMap as BpfHashMap, MapData, PerCpuArray, xdp::DevMap};
use aya::programs::{Xdp, XdpMode};
use bytes::Bytes;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::model::MacAddr;
use crate::net::switch::{PortClass, Switch};
use crate::sync::LockRecover;

const OBJ: &[u8] = include_bytes!("bpf/xdp_switch.bpf.o");

/// See `ebpf/fastpath-logic`: flag bits of the `PORT_CONF` value and the
/// host tap's fixed devmap slot.
const XDP_FLAG_ISOLATED: u32 = 1;
const XDP_FLAG_HOST: u32 = 2;
const HOST_TAG: u32 = 0;
const TAG_LEN: usize = 4;

/// Devmap capacity (must match the BPF `PORTS` definition).
const MAX_PORTS: u32 = 256;

/// Extra host-tap MTU headroom for the 4-byte punt tag (rounded up).
const HOST_TAP_SLACK: u32 = 64;

/// Async I/O over a nonblocking tap queue fd.
pub(super) struct TapIo(AsyncFd<OwnedFd>);

impl TapIo {
    pub(super) fn new(fd: OwnedFd) -> Result<TapIo> {
        Ok(TapIo(AsyncFd::new(fd).context("registering tap fd")?))
    }

    /// Read one packet (tap semantics: one frame per read).
    pub(super) async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.0.readable().await?;
            match guard.try_io(|fd| {
                rustix::io::read(fd.get_ref(), &mut *buf).map_err(std::io::Error::from)
            }) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }

    /// Write one packet (a full write or an error; taps never short-write).
    pub(super) async fn send(&self, buf: &[u8]) -> std::io::Result<()> {
        loop {
            let mut guard = self.0.writable().await?;
            match guard
                .try_io(|fd| rustix::io::write(fd.get_ref(), buf).map_err(std::io::Error::from))
            {
                Ok(res) => return res.map(|_| ()),
                Err(_would_block) => continue,
            }
        }
    }
}

/// The loaded xdp_switch object for one segment: program + typed maps.
struct XdpEngine {
    /// Kept for per-tap attaches (`program_mut` needs `&mut`).
    ebpf: Mutex<aya::Ebpf>,
    maps: Mutex<XdpMaps>,
}

struct XdpMaps {
    mac_table: BpfHashMap<MapData, [u8; 6], u32>,
    ports: DevMap<MapData>,
    port_conf: BpfHashMap<MapData, u32, [u32; 2]>,
    stats: PerCpuArray<MapData, u64>,
}

impl XdpEngine {
    fn load() -> Result<XdpEngine> {
        let mut ebpf = aya::Ebpf::load(OBJ).context("loading xdp_switch.bpf.o")?;
        let mut take = |name: &str| -> Result<aya::maps::Map> {
            ebpf.take_map(name)
                .ok_or_else(|| anyhow!("map {name} missing from object"))
        };
        let mac_table = BpfHashMap::try_from(take("MAC_TABLE")?)?;
        let ports = DevMap::try_from(take("PORTS")?)?;
        let port_conf = BpfHashMap::try_from(take("PORT_CONF")?)?;
        let stats = PerCpuArray::try_from(take("STATS")?)?;
        {
            let prog: &mut Xdp = ebpf
                .program_mut("xdp_switch")
                .ok_or_else(|| anyhow!("program xdp_switch missing from object"))?
                .try_into()?;
            prog.load().context("loading xdp_switch")?;
        }
        Ok(XdpEngine {
            ebpf: Mutex::new(ebpf),
            maps: Mutex::new(XdpMaps {
                mac_table,
                ports,
                port_conf,
                stats,
            }),
        })
    }

    /// Attach the program to a tap in generic (SKB) mode. The link lives as
    /// long as the program (= this engine); the kernel also detaches it when
    /// the device is destroyed.
    fn attach(&self, ifname: &str) -> Result<()> {
        let mut ebpf = self.ebpf.lock_recover();
        let prog: &mut Xdp = ebpf
            .program_mut("xdp_switch")
            .expect("loaded above")
            .try_into()?;
        prog.attach(ifname, XdpMode::Skb)
            .with_context(|| format!("attaching xdp_switch to {ifname} (skb mode)"))?;
        Ok(())
    }

    /// Register a tap: devmap slot + per-ifindex config.
    fn set_port(&self, tag: u32, ifindex: u32, flags: u32) -> Result<()> {
        let mut maps = self.maps.lock_recover();
        maps.port_conf
            .insert(ifindex, [tag, flags], 0)
            .context("port_conf insert")?;
        maps.ports
            .set(tag, ifindex, None, 0)
            .context("devmap set")?;
        Ok(())
    }

    fn remove_port(&self, ifindex: u32) {
        // The devmap slot is purged by the kernel when the tap is destroyed
        // (and overwritten on tag reuse); only PORT_CONF needs cleanup.
        let _ = self.maps.lock_recover().port_conf.remove(&ifindex);
    }

    fn insert_mac(&self, mac: [u8; 6], tag: u32) -> Result<()> {
        Ok(self.maps.lock_recover().mac_table.insert(mac, tag, 0)?)
    }

    fn remove_mac(&self, mac: [u8; 6]) {
        let _ = self.maps.lock_recover().mac_table.remove(&mac);
    }

    /// (forwarded, punted, dropped, injected), summed across CPUs.
    fn stats(&self) -> [u64; 4] {
        let maps = self.maps.lock_recover();
        let mut out = [0u64; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            if let Ok(values) = maps.stats.get(&(i as u32), 0) {
                *slot = values.iter().sum();
            }
        }
        out
    }
}

struct NicState {
    ifindex: u32,
    mac: [u8; 6],
    /// Clone of the NIC's switch-port ingress sender; punted frames from the
    /// host tap enter the switch through it. Dropping it (removal) detaches
    /// the switch port.
    ingress_tx: mpsc::Sender<Bytes>,
}

/// The afxdp datapath for one segment: host tap, XDP program instance, and
/// the bridge between punted frames and the userspace switch.
pub struct SegmentXdp {
    name: String,
    engine: XdpEngine,
    host_tap: Arc<TapIo>,
    /// Egress funnel: (tag, frame) pairs written to the host tap by one
    /// writer task (a tap write is one packet, so the funnel serializes).
    writer_tx: mpsc::Sender<(u32, Bytes)>,
    nics: Mutex<std::collections::HashMap<u32, NicState>>,
    /// Tag allocator: freed tags are reused (the devmap has MAX_PORTS slots).
    next_tag: AtomicU32,
    free_tags: Mutex<Vec<u32>>,
    mtu: u16,
}

impl SegmentXdp {
    /// Build the segment datapath: create the host tap, load + attach the
    /// XDP program, spawn the bridge tasks. Must run inside the daemon's
    /// tokio runtime. Needs CAP_NET_ADMIN + CAP_BPF.
    pub fn new(segment: &str, mtu: u16) -> Result<Arc<SegmentXdp>> {
        let tap = vmlab_tap::create("vmfp%d", u32::from(mtu) + HOST_TAP_SLACK)
            .context("creating host tap")?;
        let engine = XdpEngine::load()?;
        let ifindex = ifindex(&tap.name)?;
        engine.attach(&tap.name)?;
        engine.set_port(HOST_TAG, ifindex, XDP_FLAG_HOST)?;

        let (writer_tx, mut writer_rx) = mpsc::channel::<(u32, Bytes)>(512);
        let host_tap = Arc::new(TapIo::new(tap.fd)?);
        let me = Arc::new(SegmentXdp {
            name: segment.to_string(),
            engine,
            host_tap: host_tap.clone(),
            writer_tx,
            nics: Mutex::new(std::collections::HashMap::new()),
            next_tag: AtomicU32::new(HOST_TAG + 1),
            free_tags: Mutex::new(Vec::new()),
            mtu,
        });

        // Reader: host tap → (tag-routed) switch ingress.
        let reader = me.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; usize::from(reader.mtu) + 14 + HOST_TAP_SLACK as usize];
            loop {
                let n = match reader.host_tap.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(error) => {
                        debug!(segment = %reader.name, %error, "host tap reader exiting");
                        break;
                    }
                };
                if n < TAG_LEN {
                    continue;
                }
                let tag = u32::from_be_bytes(buf[..TAG_LEN].try_into().expect("4 bytes"));
                let frame = Bytes::copy_from_slice(&buf[TAG_LEN..n]);
                let tx = reader
                    .nics
                    .lock_recover()
                    .get(&tag)
                    .map(|nic| nic.ingress_tx.clone());
                match tx {
                    // Ethernet semantics: full queue drops the frame.
                    Some(tx) => {
                        let _ = tx.try_send(frame);
                    }
                    None => debug!(segment = %reader.name, tag, "punt for unknown tag dropped"),
                }
            }
        });

        // Writer: switch egress funnel → host tap.
        let name = segment.to_string();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            while let Some((tag, frame)) = writer_rx.recv().await {
                buf.clear();
                buf.extend_from_slice(&tag.to_be_bytes());
                buf.extend_from_slice(&frame);
                if let Err(error) = host_tap.send(&buf).await {
                    debug!(segment = %name, %error, "host tap writer exiting");
                    break;
                }
            }
        });

        Ok(me)
    }

    /// Attach one VM NIC: create its tap, wire the XDP maps, and register a
    /// channel port on the switch for the punt path. Returns the RAII
    /// attachment carrying the fd QEMU will inherit.
    pub fn add_nic(
        self: &Arc<Self>,
        switch: &Arc<Switch>,
        mac: MacAddr,
        isolated: bool,
    ) -> Result<TapNic> {
        let tag = self.alloc_tag()?;
        let tap = match vmlab_tap::create("vmfp%d", u32::from(self.mtu)) {
            Ok(t) => t,
            Err(e) => {
                self.free_tags.lock_recover().push(tag);
                return Err(e).context("creating nic tap");
            }
        };
        let result = (|| -> Result<()> {
            let ifindex = ifindex(&tap.name)?;
            self.engine.attach(&tap.name)?;
            let flags = if isolated { XDP_FLAG_ISOLATED } else { 0 };
            self.engine.set_port(tag, ifindex, flags)?;
            if !isolated {
                self.engine.insert_mac(mac.0, tag)?;
            }

            let port = switch.add_channel_port(PortClass::Guest { isolated });
            let mut egress_rx = port.rx;
            self.nics.lock_recover().insert(
                tag,
                NicState {
                    ifindex,
                    mac: mac.0,
                    ingress_tx: port.tx,
                },
            );
            // Egress pump: frames the switch sends this NIC → tagged host-tap
            // writes. Ends when the switch drops the port (removal).
            let writer_tx = self.writer_tx.clone();
            tokio::spawn(async move {
                while let Some(frame) = egress_rx.recv().await {
                    if writer_tx.send((tag, frame)).await.is_err() {
                        break;
                    }
                }
            });
            Ok(())
        })();
        if let Err(e) = result {
            self.remove_nic(tag);
            return Err(e);
        }

        debug!(segment = %self.name, tag, tap = %tap.name, %mac, "nic on afxdp fast path");
        Ok(TapNic {
            segment: self.clone(),
            tag,
            name: tap.name,
            fd: tap.fd,
        })
    }

    fn alloc_tag(&self) -> Result<u32> {
        if let Some(tag) = self.free_tags.lock_recover().pop() {
            return Ok(tag);
        }
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        if tag >= MAX_PORTS {
            bail!("segment {} exhausted its {MAX_PORTS} tap slots", self.name);
        }
        Ok(tag)
    }

    fn remove_nic(&self, tag: u32) {
        let nic = self.nics.lock_recover().remove(&tag);
        if let Some(nic) = nic {
            self.engine.remove_mac(nic.mac);
            self.engine.remove_port(nic.ifindex);
        }
        self.free_tags.lock_recover().push(tag);
    }

    /// (forwarded, punted, dropped, injected) kernel counters.
    pub fn stats(&self) -> [u64; 4] {
        self.engine.stats()
    }
}

/// One VM NIC attached to the afxdp fast path. RAII: dropping detaches the
/// switch port + XDP maps and closes the daemon's queue fd — once QEMU's dup
/// is gone too, the kernel destroys the tap.
pub struct TapNic {
    segment: Arc<SegmentXdp>,
    tag: u32,
    name: String,
    fd: OwnedFd,
}

impl TapNic {
    /// A fresh dup of the queue fd for a QEMU child to inherit.
    pub fn qemu_fd(&self) -> std::io::Result<OwnedFd> {
        self.fd.as_fd().try_clone_to_owned()
    }

    /// The tap's queue fd (probe use).
    pub(super) fn io(&self) -> Result<TapIo> {
        TapIo::new(self.fd.as_fd().try_clone_to_owned()?)
    }
}

impl Drop for TapNic {
    fn drop(&mut self) {
        debug!(segment = %self.segment.name, tag = self.tag, tap = %self.name, "tap nic detached");
        self.segment.remove_nic(self.tag);
    }
}

fn ifindex(name: &str) -> Result<u32> {
    nix::net::if_::if_nametoindex(name)
        .with_context(|| format!("ifindex of {name}"))
        .map_err(|e| {
            warn!("{e:#}");
            e
        })
}
