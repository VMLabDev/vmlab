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

/// A created TAP device: the kernel-assigned name and its queue fd.
pub struct Tap {
    pub name: String,
    pub fd: OwnedFd,
}

/// Create a TAP device from `pattern` (e.g. `"vmfp%d"` — the kernel picks
/// the number), set `mtu`, and bring it up.
pub fn create(pattern: &str, mtu: u32) -> io::Result<Tap> {
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
        ifr.ifr_ifru.ifru_flags = (libc::IFF_TAP | libc::IFF_NO_PI) as libc::c_short;
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
    fn create_needs_net_admin_or_succeeds() {
        // Unprivileged: a clean io::Error (EPERM). Privileged: a real tap
        // with a kernel-assigned name that vanishes when `t` drops.
        match create("vmfptest%d", 1500) {
            Ok(t) => {
                assert!(t.name.starts_with("vmfptest"));
                assert!(t.fd.as_raw_fd() >= 0);
            }
            Err(e) => {
                assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e}");
            }
        }
    }

    #[test]
    fn overlong_pattern_rejected() {
        assert!(create("waytoolongtapname%d", 1500).is_err());
    }
}
