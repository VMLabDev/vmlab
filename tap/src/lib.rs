//! TAP device creation (Linux, `/dev/net/tun`).
//!
//! Everything vmlab's afxdp fast path needs and nothing more: create a
//! non-persistent `IFF_TAP | IFF_NO_PI` device with a kernel-assigned name,
//! set its MTU, bring it up, and hand back the (nonblocking, cloexec) queue
//! fd. The device disappears when the last fd — including dups held by a
//! QEMU child — closes, which is exactly the crash-safe lifetime the daemon
//! wants. Needs CAP_NET_ADMIN; callers treat failure as "fall back".

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;

/// `_IOW('T', 202, int)` — same value on every Linux arch vmlab targets.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;

/// A created TAP device: the kernel-assigned name and its queue fd.
pub struct Tap {
    pub name: String,
    pub fd: OwnedFd,
}

/// Multiple queues of one TAP, using 12-byte virtio-net headers.
pub struct VnetTap {
    pub name: String,
    pub queues: Vec<OwnedFd>,
}

/// Create raw AF_XDP queues with 12-byte virtio headers and offloads disabled.
pub fn create_vnet(pattern: &str, mtu: u32, queue_pairs: u16) -> io::Result<VnetTap> {
    create_vnet_with(pattern, queue_pairs, |name| {
        let tap = create_queue(name, mtu, libc::IFF_MULTI_QUEUE | libc::IFF_VNET_HDR)?;
        configure_vnet(&tap.fd)?;
        Ok(tap)
    })
}

fn create_vnet_with(
    pattern: &str,
    queue_pairs: u16,
    mut open: impl FnMut(&str) -> io::Result<Tap>,
) -> io::Result<VnetTap> {
    if !(1..=31).contains(&queue_pairs) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TAP needs 1..=31 queue pairs",
        ));
    }
    let first = open(pattern)?;
    let name = first.name;
    let mut queues = Vec::with_capacity(usize::from(queue_pairs));
    queues.push(first.fd);
    for _ in 1..queue_pairs {
        let queue = open(&name)?;
        if queue.name != name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TAP queue belongs to another interface",
            ));
        }
        queues.push(queue.fd);
    }
    Ok(VnetTap { name, queues })
}

fn configure_vnet(fd: &OwnedFd) -> io::Result<()> {
    configure_vnet_with(
        |size| {
            // SAFETY: the fd is an open TAP queue and size is live through ioctl.
            #[allow(unsafe_code)]
            let result = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETVNETHDRSZ, &size) };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        },
        |flags| {
            // SAFETY: TUNSETOFFLOAD takes the mask by value, not a pointer.
            #[allow(unsafe_code)]
            let result = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETOFFLOAD, flags) };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        },
    )
}

fn configure_vnet_with(
    set_header: impl FnOnce(libc::c_int) -> io::Result<()>,
    set_offloads: impl FnOnce(u32) -> io::Result<()>,
) -> io::Result<()> {
    set_header(12)?;
    set_offloads(0)
}

/// Create a TAP device from `pattern` (e.g. `"vmfp%d"` — the kernel picks
/// the number), set `mtu`, and bring it up.
pub fn create(pattern: &str, mtu: u32) -> io::Result<Tap> {
    create_queue(pattern, mtu, 0)
}

fn create_queue(pattern: &str, mtu: u32, extra_flags: libc::c_int) -> io::Result<Tap> {
    let tun = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/dev/net/tun")?;

    #[allow(unsafe_code)]
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    let bytes = pattern.as_bytes();
    if bytes.len() >= ifr.ifr_name.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("tap name pattern `{pattern}` too long"),
        ));
    }
    for (dst, src) in ifr.ifr_name.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    #[allow(unsafe_code)]
    unsafe {
        ifr.ifr_ifru.ifru_flags = (libc::IFF_TAP | libc::IFF_NO_PI | extra_flags) as libc::c_short;
        if libc::ioctl(tun.as_raw_fd(), TUNSETIFF, &mut ifr) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let name = ifr_name(&ifr);

    // MTU + IFF_UP via a scratch dgram socket.
    let sock = ScratchSocket::new()?;
    #[allow(unsafe_code)]
    unsafe {
        ifr.ifr_ifru.ifru_mtu = mtu as libc::c_int;
        if libc::ioctl(sock.0, libc::SIOCSIFMTU, &ifr) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(sock.0, libc::SIOCGIFFLAGS, &mut ifr) < 0 {
            return Err(io::Error::last_os_error());
        }
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(sock.0, libc::SIOCSIFFLAGS, &ifr) < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(Tap {
        name,
        fd: OwnedFd::from(tun),
    })
}

fn ifr_name(ifr: &libc::ifreq) -> String {
    ifr.ifr_name
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8 as char)
        .collect()
}

/// RAII AF_INET dgram socket for the SIOCSIF* ioctls.
struct ScratchSocket(libc::c_int);

impl ScratchSocket {
    fn new() -> io::Result<Self> {
        #[allow(unsafe_code)]
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }
}

impl Drop for ScratchSocket {
    fn drop(&mut self) {
        #[allow(unsafe_code)]
        unsafe {
            libc::close(self.0)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiqueue_uses_the_first_queues_interface_name() {
        let mut names = Vec::new();
        let tap = create_vnet_with("vmfp%d", 3, |name| {
            names.push(name.to_owned());
            let (fd, _) = std::os::unix::net::UnixDatagram::pair()?;
            Ok(Tap {
                name: "vmfp17".into(),
                fd: fd.into(),
            })
        })
        .unwrap();
        assert_eq!(names, ["vmfp%d", "vmfp17", "vmfp17"]);
        assert_eq!(tap.name, "vmfp17");
        assert_eq!(tap.queues.len(), 3);
    }

    #[test]
    fn invalid_queue_counts_do_not_open_a_device() {
        for count in [0, 32, u16::MAX] {
            assert!(
                create_vnet_with("vmfp%d", count, |_| panic!("invalid count opened TAP")).is_err()
            );
        }
    }

    #[test]
    fn vnet_queues_keep_offloads_disabled() {
        let mut sizes = Vec::new();
        let mut masks = Vec::new();
        configure_vnet_with(
            |size| {
                sizes.push(size);
                Ok(())
            },
            |mask| {
                masks.push(mask);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(sizes, [12]);
        assert_eq!(masks, [0]);
    }

    #[test]
    fn a_queue_creation_error_stops_the_attachment() {
        let mut opened = 0;
        let error = create_vnet_with("vmfp%d", 3, |_| {
            opened += 1;
            if opened == 2 {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            let (fd, _) = std::os::unix::net::UnixDatagram::pair()?;
            Ok(Tap {
                name: "vmfp17".into(),
                fd: fd.into(),
            })
        })
        .err()
        .unwrap();
        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        assert_eq!(opened, 2);
    }

    #[test]
    fn vnet_configuration_errors_are_reported() {
        let error = configure_vnet_with(
            |_| Err(io::Error::from_raw_os_error(libc::ENOTTY)),
            |_| panic!("offloads configured after a header failure"),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOTTY));
        for errno in [libc::EINVAL, libc::EIO] {
            let mut attempts = 0;
            let error = configure_vnet_with(
                |size| {
                    assert_eq!(size, 12);
                    Ok(())
                },
                |mask| {
                    attempts += 1;
                    assert_eq!(mask, 0);
                    Err(io::Error::from_raw_os_error(errno))
                },
            )
            .unwrap_err();
            assert_eq!(attempts, 1);
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn create_needs_net_admin_or_succeeds() {
        // Privileged: a real tap with a kernel-assigned name that vanishes
        // when `t` drops. Otherwise a clean io::Error, and which one is an
        // environment fact, not a bug: EPERM without CAP_NET_ADMIN, ENODEV
        // when the tun driver is not registered (kernel upgraded in place,
        // so /lib/modules has nothing to load), ENOENT with no device node
        // at all. Asserting EPERM alone made this test fail on a host that
        // simply had not rebooted.
        match create("vmfptest%d", 1500) {
            Ok(t) => {
                assert!(t.name.starts_with("vmfptest"));
                assert!(t.fd.as_raw_fd() >= 0);
            }
            Err(e) => {
                let errno = e.raw_os_error();
                assert!(
                    matches!(
                        errno,
                        Some(libc::EPERM)
                            | Some(libc::EACCES)
                            | Some(libc::ENODEV)
                            | Some(libc::ENOENT)
                    ),
                    "unexpected tap failure: {e}"
                );
            }
        }
    }

    #[test]
    fn overlong_pattern_rejected() {
        assert!(create("waytoolongtapname%d", 1500).is_err());
    }
}
