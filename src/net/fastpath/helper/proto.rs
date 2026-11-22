//! The request protocol between a daemon and its `vmlab-fastpath` helper.
//!
//! One JSON message per `SOCK_SEQPACKET` record, descriptors riding the
//! same record as `SCM_RIGHTS`. Strictly request then reply: the daemon
//! holds a lock across each exchange, so no ids are needed. The request
//! vocabulary is closed: every variant below is one fast-path operation,
//! and anything else fails to parse and is refused.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use serde::{Deserialize, Serialize};

/// The largest record either side sends or accepts.
pub(crate) const MAX_MESSAGE: usize = 64 * 1024;
/// A vnet tap's queues (at most 31) are the largest descriptor set.
pub(crate) const MAX_FDS: usize = 31;
/// Devmap capacity (must match the BPF `PORTS` definition).
pub(crate) const MAX_PORTS: u32 = 256;
/// How many loaded objects one helper holds at once (one per segment).
pub(crate) const MAX_ENGINES: usize = 64;
/// The most MACs one sockmap unregister may name.
pub(crate) const MAX_MACS: usize = 4096;

/// See `ebpf/fastpath-logic`: flag bits of the `PORT_CONF` value and the
/// host tap's fixed devmap slot.
pub(crate) const XDP_FLAG_ISOLATED: u32 = 1;
pub(crate) const XDP_FLAG_HOST: u32 = 2;
pub(crate) const HOST_TAG: u32 = 0;

/// Every operation the helper performs. `engine` names an object an
/// earlier `*_load` returned on this connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    /// Load one xdp_switch object (one per segment). The load requests are
    /// empty struct variants so that `deny_unknown_fields` refuses extra
    /// fields on them too.
    XdpLoad {},
    /// Create a `vmfp%d` tap, attach the engine's program to it and return
    /// its queues: one raw queue, or `queue_pairs` vnet queues.
    XdpTap {
        engine: u32,
        queue_pairs: Option<u16>,
    },
    /// Give a tap this engine created a devmap slot and its flags.
    XdpPort {
        engine: u32,
        ifindex: u32,
        tag: u32,
        flags: u32,
    },
    /// Forget a tap: drop its port config and the helper's queue.
    XdpRelease {
        engine: u32,
        ifindex: u32,
    },
    /// Point a unicast MAC at a slot (`tag`), or remove it (`None`).
    XdpMac {
        engine: u32,
        mac: [u8; 6],
        tag: Option<u32>,
    },
    XdpStats {
        engine: u32,
    },
    /// Load one fastpath_sockmap object (one per switch).
    SockmapLoad {},
    /// Offload the unix stream socket passed with the request as port `id`.
    SockmapStream {
        engine: u32,
        id: u64,
    },
    /// Register a TX loopback: the kernel end, then the end the daemon
    /// writes, both unix datagram sockets passed with the request.
    SockmapTx {
        engine: u32,
        id: u64,
    },
    /// Point a unicast MAC at an offloaded port (`id`), or remove it.
    SockmapMac {
        engine: u32,
        mac: [u8; 6],
        id: Option<u64>,
    },
    SockmapUnregister {
        engine: u32,
        id: u64,
        tx_cookie: u64,
        macs: Vec<[u8; 6]>,
    },
    SockmapStats {
        engine: u32,
    },
    SockmapDebug {
        engine: u32,
    },
    /// Drop an engine and everything it holds. Its program stays attached
    /// to each tap it filtered until that tap is destroyed.
    Unload {
        engine: u32,
    },
}

impl Request {
    /// How many descriptors this request carries.
    pub(crate) fn fds(&self) -> usize {
        match self {
            Request::SockmapStream { .. } => 1,
            Request::SockmapTx { .. } => 2,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Reply {
    Done,
    Engine {
        engine: u32,
    },
    /// The tap's queues ride this reply.
    Tap {
        name: String,
        ifindex: u32,
    },
    XdpStats {
        counters: [u64; 4],
    },
    Cookie {
        cookie: u64,
    },
    SockmapStats {
        frames: u64,
        bytes: u64,
    },
    Debug {
        text: String,
    },
    /// Not a fast-path operation, or not one this connection may make.
    Refused {
        reason: String,
    },
    /// A permitted operation the kernel failed.
    Failed {
        reason: String,
    },
}

/// Send one record with its descriptors.
pub(crate) fn send(
    sock: BorrowedFd<'_>,
    message: &impl Serialize,
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors",
        ));
    }
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message too long",
        ));
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !ancillary.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors",
        ));
    }
    loop {
        match sendmsg(
            sock,
            &[IoSlice::new(&bytes)],
            &mut ancillary,
            SendFlags::NOSIGNAL,
        ) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Receive one record: `None` when the peer has gone. A truncated record
/// or descriptor set is an `InvalidData` error, its descriptors closed.
pub(crate) fn recv(sock: BorrowedFd<'_>) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut bytes = vec![0u8; MAX_MESSAGE + 1];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let received = loop {
        match recvmsg(
            sock,
            &mut [IoSliceMut::new(&mut bytes)],
            &mut ancillary,
            RecvFlags::CMSG_CLOEXEC,
        ) {
            Ok(received) => break received,
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let mut fds = Vec::new();
    for message in ancillary.drain() {
        match message {
            RecvAncillaryMessage::ScmRights(rights) => fds.extend(rights),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected ancillary message",
                ));
            }
        }
    }
    if received.flags.contains(ReturnFlags::CTRUNC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated descriptors",
        ));
    }
    if received.flags.contains(ReturnFlags::TRUNC) || received.bytes > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too long",
        ));
    }
    if received.bytes == 0 && fds.is_empty() {
        return Ok(None);
    }
    bytes.truncate(received.bytes);
    Ok(Some((bytes, fds)))
}

/// A connected `SOCK_SEQPACKET` pair, both ends close-on-exec.
pub(crate) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    Ok(socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn requests_round_trip_and_nothing_else_parses() {
        let all = [
            Request::XdpLoad {},
            Request::XdpTap {
                engine: 1,
                queue_pairs: Some(2),
            },
            Request::XdpPort {
                engine: 1,
                ifindex: 7,
                tag: 1,
                flags: 0,
            },
            Request::XdpRelease {
                engine: 1,
                ifindex: 7,
            },
            Request::XdpMac {
                engine: 1,
                mac: [2, 0, 0, 0, 0, 1],
                tag: None,
            },
            Request::XdpStats { engine: 1 },
            Request::SockmapLoad {},
            Request::SockmapStream { engine: 2, id: 3 },
            Request::SockmapTx { engine: 2, id: 3 },
            Request::SockmapMac {
                engine: 2,
                mac: [2, 0, 0, 0, 0, 1],
                id: Some(3),
            },
            Request::SockmapUnregister {
                engine: 2,
                id: 3,
                tx_cookie: 9,
                macs: vec![],
            },
            Request::SockmapStats { engine: 2 },
            Request::SockmapDebug { engine: 2 },
            Request::Unload { engine: 2 },
        ];
        for request in all {
            let text = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        }
        for foreign in [
            r#"{"op":"exec","argv":["sh"]}"#,
            r#"{"op":"xdp_attach","engine":1,"ifname":"eth0"}"#,
            r#"{"op":"xdp_load","path":"/tmp/evil.o"}"#,
            r#"{"op":"xdp_port","engine":1,"ifindex":2,"tag":1,"flags":0,"ifname":"eth0"}"#,
            r#"{"engine":1}"#,
            "not json",
        ] {
            assert!(
                serde_json::from_str::<Request>(foreign).is_err(),
                "{foreign} parsed"
            );
        }
    }

    #[test]
    fn records_keep_their_boundaries_and_descriptors() {
        let (a, b) = pair().unwrap();
        let (x, _y) = std::os::unix::net::UnixDatagram::pair().unwrap();
        send(
            a.as_fd(),
            &Request::SockmapStream { engine: 1, id: 2 },
            &[x.as_fd()],
        )
        .unwrap();
        send(a.as_fd(), &Request::XdpLoad {}, &[]).unwrap();
        let (first, fds) = recv(b.as_fd()).unwrap().unwrap();
        assert_eq!(fds.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Request>(&first).unwrap(),
            Request::SockmapStream { engine: 1, id: 2 }
        );
        let (second, fds) = recv(b.as_fd()).unwrap().unwrap();
        assert!(fds.is_empty());
        assert_eq!(
            serde_json::from_slice::<Request>(&second).unwrap(),
            Request::XdpLoad {}
        );
        drop(a);
        assert!(recv(b.as_fd()).unwrap().is_none());
    }

    #[test]
    fn an_oversized_record_is_refused_not_truncated() {
        let (a, b) = pair().unwrap();
        let huge = vec![b'x'; MAX_MESSAGE + 10];
        rustix::net::send(&a, &huge, SendFlags::empty()).unwrap();
        let error = recv(b.as_fd()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
