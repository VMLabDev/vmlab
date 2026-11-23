//! The kernel side of both fast-path tiers, as only `vmlab-fastpath` runs
//! it: loading the embedded BPF objects, creating tap devices, attaching
//! XDP, and writing the maps. Every call here needs CAP_BPF and
//! CAP_NET_ADMIN, which is why it lives behind the helper's request
//! protocol (`helper/`) instead of in the daemons (F21).

use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};

use anyhow::{Context, Result, anyhow};
use aya::maps::{HashMap as BpfHashMap, MapData, PerCpuArray, SockHash, xdp::DevMap};
use aya::programs::{SkSkb, Xdp};

const XDP_OBJ: &[u8] = include_bytes!("bpf/xdp_switch.bpf.o");
const SOCKMAP_OBJ: &[u8] = include_bytes!("bpf/fastpath_sockmap.bpf.o");

/// Linux tun caps the MTU at 65535 minus the Ethernet header.
pub(super) const MAX_TAP_MTU: u32 = u16::MAX as u32 - 14;

/// The loaded xdp_switch object for one segment: program and typed maps.
pub(crate) struct XdpEngine {
    ebpf: aya::Ebpf,
    mac_table: BpfHashMap<MapData, [u8; 6], u32>,
    ports: DevMap<MapData>,
    port_conf: BpfHashMap<MapData, u32, [u32; 2]>,
    stats: PerCpuArray<MapData, u64>,
}

impl XdpEngine {
    pub(super) fn load() -> Result<XdpEngine> {
        let mut ebpf = aya::Ebpf::load(XDP_OBJ).context("loading xdp_switch.bpf.o")?;
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
            ebpf,
            mac_table,
            ports,
            port_conf,
            stats,
        })
    }

    /// Attach the program to a tap in generic (SKB) mode — native XDP on tun
    /// silently bypasses large frames.
    ///
    /// The attachment belongs to the tap, not to this engine: it is made
    /// through rtnetlink, so the device holds the program (and through it
    /// the maps) until the device itself is destroyed. A `bpf_link`, which
    /// is what aya makes, ends with its descriptor: an `unload`, or the end
    /// of this process, would leave the tap up and unfiltered in the hands
    /// of whoever still holds one of its queues, and every frame written
    /// there would enter the host's network stack.
    pub(super) fn attach(&mut self, ifname: &str) -> Result<()> {
        let ifindex = ifindex(ifname)?;
        let prog: &mut Xdp = self
            .ebpf
            .program_mut("xdp_switch")
            .expect("loaded above")
            .try_into()?;
        let fd = prog.fd().context("xdp_switch is not loaded")?;
        attach_until_the_device_goes(ifindex, fd.as_fd())
            .with_context(|| format!("attaching xdp_switch to {ifname} (skb mode)"))?;
        Ok(())
    }

    /// Register a tap: devmap slot and per-ifindex config.
    pub(super) fn set_port(&mut self, tag: u32, ifindex: u32, flags: u32) -> Result<()> {
        self.port_conf
            .insert(ifindex, [tag, flags], 0)
            .context("port_conf insert")?;
        self.ports
            .set(tag, ifindex, None, 0)
            .context("devmap set")?;
        Ok(())
    }

    pub(super) fn remove_port(&mut self, ifindex: u32) {
        // The devmap slot is purged by the kernel when the tap is destroyed
        // (and overwritten on tag reuse); only PORT_CONF needs cleanup.
        let _ = self.port_conf.remove(&ifindex);
    }

    pub(super) fn insert_mac(&mut self, mac: [u8; 6], tag: u32) -> Result<()> {
        Ok(self.mac_table.insert(mac, tag, 0)?)
    }

    pub(super) fn remove_mac(&mut self, mac: [u8; 6]) {
        let _ = self.mac_table.remove(&mac);
    }

    /// (forwarded, punted, dropped, injected), summed across CPUs.
    pub(super) fn stats(&self) -> [u64; 4] {
        let mut out = [0u64; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            if let Ok(values) = self.stats.get(&(i as u32), 0) {
                *slot = values.iter().sum();
            }
        }
        out
    }
}

// rtnetlink, as `include/uapi/linux/{netlink,rtnetlink,if_link}.h` number it.
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const RTM_SETLINK: u16 = 19;
const NLA_F_NESTED: u16 = 1 << 15;
const IFLA_XDP: u16 = 43;
const IFLA_XDP_FD: u16 = 1;
const IFLA_XDP_FLAGS: u16 = 3;
/// Fail rather than replace a program the device already carries.
const XDP_FLAGS_UPDATE_IF_NOEXIST: u32 = 1;
const XDP_FLAGS_SKB_MODE: u32 = 1 << 1;

/// The `RTM_SETLINK` message that sets a device's generic-mode XDP program:
/// `nlmsghdr`, `ifinfomsg`, then `IFLA_XDP { FD, FLAGS }`. Every length here
/// is a multiple of four, so no attribute needs padding.
fn xdp_setlink_request(ifindex: u32, prog_fd: i32) -> Vec<u8> {
    let attr = |kind: u16, value: [u8; 4]| {
        let mut attr = Vec::with_capacity(8);
        attr.extend_from_slice(&8u16.to_ne_bytes());
        attr.extend_from_slice(&kind.to_ne_bytes());
        attr.extend_from_slice(&value);
        attr
    };
    let nested = [
        attr(IFLA_XDP_FD, prog_fd.to_ne_bytes()),
        attr(
            IFLA_XDP_FLAGS,
            (XDP_FLAGS_UPDATE_IF_NOEXIST | XDP_FLAGS_SKB_MODE).to_ne_bytes(),
        ),
    ]
    .concat();

    let mut message = Vec::with_capacity(52);
    // nlmsghdr: length (written last), type, flags, sequence, port id.
    message.extend_from_slice(&[0; 4]);
    message.extend_from_slice(&RTM_SETLINK.to_ne_bytes());
    message.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
    message.extend_from_slice(&1u32.to_ne_bytes());
    message.extend_from_slice(&0u32.to_ne_bytes());
    // ifinfomsg: family, padding and type, then the index, flags and change.
    message.extend_from_slice(&[0; 4]);
    message.extend_from_slice(&ifindex.to_ne_bytes());
    message.extend_from_slice(&[0; 8]);
    message.extend_from_slice(&(4 + nested.len() as u16).to_ne_bytes());
    message.extend_from_slice(&(IFLA_XDP | NLA_F_NESTED).to_ne_bytes());
    message.extend_from_slice(&nested);
    let len = message.len() as u32;
    message[..4].copy_from_slice(&len.to_ne_bytes());
    message
}

/// The kernel's answer to a request sent with `NLM_F_ACK`: an `NLMSG_ERROR`
/// message whose first field after the header is zero or a negated errno.
fn netlink_ack(reply: &[u8]) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let (Some(kind), Some(code)) = (
        reply.get(4..6).and_then(|b| b.try_into().ok()),
        reply.get(16..20).and_then(|b| b.try_into().ok()),
    ) else {
        return Err(Error::new(ErrorKind::InvalidData, "short netlink reply"));
    };
    if u16::from_ne_bytes(kind) != NLMSG_ERROR {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "netlink did not acknowledge",
        ));
    }
    match i32::from_ne_bytes(code) {
        0 => Ok(()),
        code => Err(Error::from_raw_os_error(code.saturating_neg())),
    }
}

/// Set `prog` as the device's XDP program over rtnetlink. From here on the
/// device owns the attachment: closing `prog`, or this process ending,
/// leaves it in place, and only the device's own destruction removes it.
fn attach_until_the_device_goes(ifindex: u32, prog: BorrowedFd<'_>) -> std::io::Result<()> {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socket_with};
    let sock = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::io::write(&sock, &xdp_setlink_request(ifindex, prog.as_raw_fd()))?;
    let mut reply = [0u8; 4096];
    let len = rustix::io::read(&sock, &mut reply)?;
    netlink_ack(&reply[..len])
}

/// `SOCK_STATE` map value: the BPF side's `SockState` (repr(C): `u64`
/// port_id, then the 12-byte framing state, padded to 24). Only the leading
/// port id is read, when scanning entries to delete on port removal.
const SOCK_STATE_LEN: usize = 24;

pub(super) fn socket_cookie(fd: BorrowedFd<'_>) -> Result<u64> {
    rustix::net::sockopt::socket_cookie(fd).context("SO_COOKIE")
}

/// One loaded fastpath_sockmap object: programs attached, typed map handles
/// taken.
pub(crate) struct SockmapEngine {
    /// Owns the loaded programs (and thus their attachments).
    _ebpf: aya::Ebpf,
    port_hash: SockHash<MapData, u64>,
    tx_hash: SockHash<MapData, u64>,
    mac_map: BpfHashMap<MapData, [u8; 6], u64>,
    sock_state: BpfHashMap<MapData, u64, [u8; SOCK_STATE_LEN]>,
    tx_target: BpfHashMap<MapData, u64, u64>,
    fp_stats: PerCpuArray<MapData, [u64; 2]>,
    fp_debug: PerCpuArray<MapData, u64>,
}

impl SockmapEngine {
    /// Load the embedded object and attach both verdict programs; any
    /// failure (perms, verifier, old kernel) surfaces here.
    pub(super) fn load() -> Result<SockmapEngine> {
        let mut ebpf = aya::Ebpf::load(SOCKMAP_OBJ).context("loading fastpath_sockmap.bpf.o")?;
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

        Ok(SockmapEngine {
            _ebpf: ebpf,
            port_hash,
            tx_hash,
            mac_map,
            sock_state,
            tx_target,
            fp_stats,
            fp_debug,
        })
    }

    /// Register a QEMU-facing stream socket as offloaded port `id`.
    pub(super) fn register_stream(&mut self, id: u64, sock: BorrowedFd<'_>) -> Result<()> {
        self.port_hash
            .insert(id, sock.as_raw_fd(), 0)
            .context("port_hash insert")?;
        Ok(())
    }

    /// Register a TX loopback: `map_sock` (the kernel end) joins the
    /// sockmap; `cookie_sock` (the end the daemon writes) is the sender the
    /// kernel attributes those dgrams to, hence the TX_TARGET key.
    pub(super) fn register_tx(
        &mut self,
        id: u64,
        map_sock: BorrowedFd<'_>,
        cookie_sock: BorrowedFd<'_>,
    ) -> Result<u64> {
        let cookie = socket_cookie(cookie_sock)?;
        self.tx_target
            .insert(cookie, id, 0)
            .context("tx_target insert")?;
        if let Err(e) = self.tx_hash.insert(id, map_sock.as_raw_fd(), 0) {
            let _ = self.tx_target.remove(&cookie);
            return Err(anyhow::Error::new(e).context("tx_hash insert"));
        }
        Ok(cookie)
    }

    pub(super) fn insert_mac(&mut self, mac: [u8; 6], id: u64) -> Result<()> {
        Ok(self.mac_map.insert(mac, id, 0)?)
    }

    pub(super) fn remove_mac(&mut self, mac: [u8; 6]) {
        let _ = self.mac_map.remove(&mac);
    }

    /// Best-effort cleanup of everything a port registered. SOCK_STATE
    /// entries are keyed by cookies only the kernel saw, so they are found
    /// by scanning for the port id in the value.
    pub(super) fn unregister(&mut self, id: u64, tx_cookie: u64, macs: &[[u8; 6]]) {
        let _ = self.port_hash.remove(&id);
        let _ = self.tx_hash.remove(&id);
        let _ = self.tx_target.remove(&tx_cookie);
        for mac in macs {
            let _ = self.mac_map.remove(mac);
        }
        let stale: Vec<u64> = self
            .sock_state
            .iter()
            .filter_map(|entry| entry.ok())
            .filter(|(_, v)| v[..8] == id.to_ne_bytes())
            .map(|(k, _)| k)
            .collect();
        for cookie in stale {
            let _ = self.sock_state.remove(&cookie);
        }
    }

    /// Kernel-forwarded (frames, bytes), summed across CPUs.
    pub(super) fn stats(&self) -> (u64, u64) {
        match self.fp_stats.get(&0, 0) {
            Ok(values) => values.iter().fold((0, 0), |(f, b), v| (f + v[0], b + v[1])),
            Err(_) => (0, 0),
        }
    }

    /// The verdict programs' branch counters (FP_DEBUG in the BPF source).
    pub(super) fn debug_counters(&self) -> String {
        let read = |i: u32| -> u64 {
            self.fp_debug
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

/// Create a `vmfp%d` tap: one raw queue, or `queue_pairs` vnet queues.
pub(super) fn create_tap(queue_pairs: Option<u16>) -> Result<(String, Vec<OwnedFd>)> {
    let opened = match queue_pairs {
        None => vmlab_tap::create("vmfp%d", MAX_TAP_MTU).map(|tap| (tap.name, vec![tap.fd])),
        Some(pairs) => {
            vmlab_tap::create_vnet("vmfp%d", MAX_TAP_MTU, pairs).map(|tap| (tap.name, tap.queues))
        }
    };
    opened.map_err(|error| {
        anyhow!(
            "creating a tap: {error} — {}",
            diagnose_tap_failure(error.raw_os_error(), &TapEnv::probe())
        )
    })
}

pub(super) fn ifindex(name: &str) -> Result<u32> {
    nix::net::if_::if_nametoindex(name).with_context(|| format!("ifindex of {name}"))
}

/// What the host looks like to the tap layer. Split out so the diagnosis
/// below is a pure function of it (and therefore testable off a real host).
struct TapEnv {
    /// `/dev/net/tun` is present.
    node: bool,
    /// Running kernel release, and whether its module tree is installed.
    /// `None` when the release could not be read.
    kernel: Option<(String, bool)>,
}

impl TapEnv {
    fn probe() -> TapEnv {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|release| {
                let release = release.trim().to_string();
                let modules = std::path::Path::new("/lib/modules").join(&release).is_dir();
                (release, modules)
            });
        TapEnv {
            node: std::path::Path::new("/dev/net/tun").exists(),
            kernel,
        }
    }
}

/// Say what a tap-creation errno actually means for this host.
///
/// The raw errno is actively misleading here. `ENODEV` renders as "No such
/// device" while the device node is sitting right there in `/dev/net/tun`:
/// it means the `tun` driver is not registered, not that the node is
/// missing. Worse, the usual remedy (`modprobe tun`) also fails when the
/// running kernel's module tree has been removed by an in-place kernel
/// upgrade — nothing short of a reboot fixes that, and nothing in the errno
/// hints at it.
fn diagnose_tap_failure(errno: Option<i32>, env: &TapEnv) -> String {
    match errno {
        Some(nix::libc::ENODEV) if env.node => match &env.kernel {
            Some((release, false)) => format!(
                "the tun driver is not loaded and cannot be: the running kernel ({release}) has \
                 no module tree under /lib/modules, so it was upgraded in place. Reboot into the \
                 installed kernel, then the fast path selects itself"
            ),
            _ => "the tun driver is not loaded. Run `modprobe tun` on the host (not inside the \
                  container) and restart vmlab"
                .into(),
        },
        Some(nix::libc::ENODEV) | Some(nix::libc::ENOENT) => {
            "/dev/net/tun is missing. In a container, pass `--device /dev/net/tun`; on a host, \
             load the tun driver with `modprobe tun`"
                .into()
        }
        Some(nix::libc::EPERM) | Some(nix::libc::EACCES) => {
            "creating a tap needs CAP_NET_ADMIN on the vmlab-fastpath helper. Grant it with \
             `install.sh --fastpath`; in a container, add `--cap-add NET_ADMIN` (the fast path \
             also needs `--cap-add BPF`)"
                .into()
        }
        _ => "the fast path stays off; the userspace fabric carries the lab unchanged".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(node: bool, kernel: Option<(&str, bool)>) -> TapEnv {
        TapEnv {
            node,
            kernel: kernel.map(|(r, m)| (r.to_string(), m)),
        }
    }

    /// Node present, driver unregistered, running kernel's modules deleted
    /// by an in-place upgrade. `modprobe` cannot help, so the diagnosis must
    /// say "reboot" and name the release.
    #[test]
    fn enodev_with_no_module_tree_blames_the_missing_reboot() {
        let msg = diagnose_tap_failure(
            Some(nix::libc::ENODEV),
            &env(true, Some(("7.1.1-2-cachyos", false))),
        );
        assert!(msg.contains("7.1.1-2-cachyos"), "{msg}");
        assert!(msg.contains("Reboot"), "{msg}");
        assert!(!msg.contains("modprobe"), "modprobe cannot fix this: {msg}");
    }

    #[test]
    fn enodev_with_a_module_tree_points_at_modprobe() {
        let msg =
            diagnose_tap_failure(Some(nix::libc::ENODEV), &env(true, Some(("7.1.5-1", true))));
        assert!(msg.contains("modprobe tun"), "{msg}");
        assert!(!msg.contains("Reboot"), "{msg}");
    }

    /// No node at all is the container-without-`--device` case, which reports
    /// the same ENODEV on some runtimes — the node check is what separates them.
    #[test]
    fn missing_node_points_at_the_device_flag() {
        for errno in [nix::libc::ENODEV, nix::libc::ENOENT] {
            let msg = diagnose_tap_failure(Some(errno), &env(false, Some(("7.1.5-1", true))));
            assert!(msg.contains("--device /dev/net/tun"), "{errno}: {msg}");
        }
    }

    #[test]
    fn permission_failures_name_the_capability_and_the_helper() {
        for errno in [nix::libc::EPERM, nix::libc::EACCES] {
            let msg = diagnose_tap_failure(Some(errno), &env(true, Some(("7.1.5-1", true))));
            assert!(msg.contains("NET_ADMIN"), "{errno}: {msg}");
            assert!(msg.contains("vmlab-fastpath"), "{errno}: {msg}");
        }
    }

    /// An unclassified failure must still reassure rather than alarm: the lab
    /// works, it is just not accelerated.
    #[test]
    fn unknown_errno_falls_back_to_the_reassuring_line() {
        let msg = diagnose_tap_failure(Some(nix::libc::EIO), &env(true, None));
        assert!(msg.contains("userspace fabric"), "{msg}");
    }

    /// The attach request, byte for byte: a wrong length or a missing
    /// nesting flag is refused by the kernel only on a privileged host.
    #[test]
    fn the_setlink_request_carries_the_program_and_the_skb_mode() {
        let message = xdp_setlink_request(0x0102_0304, 7);
        let mut want = Vec::new();
        want.extend_from_slice(&52u32.to_ne_bytes());
        want.extend_from_slice(&19u16.to_ne_bytes()); // RTM_SETLINK
        want.extend_from_slice(&5u16.to_ne_bytes()); // REQUEST | ACK
        want.extend_from_slice(&1u32.to_ne_bytes());
        want.extend_from_slice(&0u32.to_ne_bytes());
        want.extend_from_slice(&[0; 4]);
        want.extend_from_slice(&0x0102_0304u32.to_ne_bytes());
        want.extend_from_slice(&[0; 8]);
        want.extend_from_slice(&20u16.to_ne_bytes());
        want.extend_from_slice(&(43u16 | 0x8000).to_ne_bytes()); // IFLA_XDP, nested
        want.extend_from_slice(&8u16.to_ne_bytes());
        want.extend_from_slice(&1u16.to_ne_bytes()); // IFLA_XDP_FD
        want.extend_from_slice(&7i32.to_ne_bytes());
        want.extend_from_slice(&8u16.to_ne_bytes());
        want.extend_from_slice(&3u16.to_ne_bytes()); // IFLA_XDP_FLAGS
        want.extend_from_slice(&3u32.to_ne_bytes()); // UPDATE_IF_NOEXIST | SKB_MODE
        assert_eq!(message, want);
    }

    #[test]
    fn a_netlink_reply_is_an_ack_or_the_kernels_errno() {
        let reply = |kind: u16, code: i32| {
            let mut reply = Vec::new();
            reply.extend_from_slice(&36u32.to_ne_bytes());
            reply.extend_from_slice(&kind.to_ne_bytes());
            reply.extend_from_slice(&[0; 10]);
            reply.extend_from_slice(&code.to_ne_bytes());
            reply.extend_from_slice(&[0; 16]);
            reply
        };
        assert!(netlink_ack(&reply(NLMSG_ERROR, 0)).is_ok());
        let refused = netlink_ack(&reply(NLMSG_ERROR, -nix::libc::EPERM)).unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(nix::libc::EPERM));
        let busy = netlink_ack(&reply(NLMSG_ERROR, -nix::libc::EBUSY)).unwrap_err();
        assert_eq!(busy.raw_os_error(), Some(nix::libc::EBUSY));
        // Not an answer to the request: never taken for success.
        assert!(netlink_ack(&reply(3, 0)).is_err());
        assert!(netlink_ack(&reply(NLMSG_ERROR, 0)[..19]).is_err());
        assert!(netlink_ack(&[]).is_err());
    }
}
