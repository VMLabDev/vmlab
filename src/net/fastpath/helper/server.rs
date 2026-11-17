//! The helper's side: one connection, one daemon, one loop.
//!
//! The helper holds CAP_BPF and CAP_NET_ADMIN, so whatever it accepts is
//! what any program running as the user can do through it. It therefore
//! checks every request before any kernel call: the operation must be in
//! the closed vocabulary, an engine must be one this connection loaded, a
//! devmap slot may only point at a tap this helper created (never at a host
//! interface), MACs must be unicast, and passed descriptors must be the
//! unix sockets the sockmap tier expects. The kernel work itself sits behind
//! [`Kernel`] so these checks are tested without privileges.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{AddressFamily, SocketType};

use super::proto::{
    self, HOST_TAG, MAX_ENGINES, MAX_FDS, MAX_MACS, MAX_PORTS, Reply, Request, XDP_FLAG_HOST,
    XDP_FLAG_ISOLATED,
};

/// The privileged operations, one method each.
pub(crate) trait Kernel {
    type Xdp;
    type Sockmap;
    fn xdp_load(&mut self) -> Result<Self::Xdp, String>;
    /// Create a tap; returns its name, ifindex and queues.
    fn tap_create(&mut self, queue_pairs: Option<u16>)
    -> Result<(String, u32, Vec<OwnedFd>), String>;
    fn xdp_attach(&mut self, xdp: &mut Self::Xdp, ifname: &str) -> Result<(), String>;
    fn xdp_port(
        &mut self,
        xdp: &mut Self::Xdp,
        tag: u32,
        ifindex: u32,
        flags: u32,
    ) -> Result<(), String>;
    fn xdp_unport(&mut self, xdp: &mut Self::Xdp, ifindex: u32);
    fn xdp_mac(&mut self, xdp: &mut Self::Xdp, mac: [u8; 6], tag: Option<u32>)
    -> Result<(), String>;
    fn xdp_stats(&mut self, xdp: &Self::Xdp) -> [u64; 4];
    fn sockmap_load(&mut self) -> Result<Self::Sockmap, String>;
    fn sockmap_stream(
        &mut self,
        sockmap: &mut Self::Sockmap,
        id: u64,
        sock: BorrowedFd<'_>,
    ) -> Result<(), String>;
    fn sockmap_tx(
        &mut self,
        sockmap: &mut Self::Sockmap,
        id: u64,
        kernel_end: BorrowedFd<'_>,
        user_end: BorrowedFd<'_>,
    ) -> Result<u64, String>;
    fn sockmap_mac(
        &mut self,
        sockmap: &mut Self::Sockmap,
        mac: [u8; 6],
        id: Option<u64>,
    ) -> Result<(), String>;
    fn sockmap_unregister(
        &mut self,
        sockmap: &mut Self::Sockmap,
        id: u64,
        tx_cookie: u64,
        macs: &[[u8; 6]],
    );
    fn sockmap_stats(&mut self, sockmap: &Self::Sockmap) -> (u64, u64);
    fn sockmap_debug(&mut self, sockmap: &Self::Sockmap) -> String;
}

/// A tap this helper created. The helper keeps one queue open until the
/// daemon releases the tap, so its ifindex cannot name another device
/// while a devmap slot may point at it.
struct OwnedTap {
    _queue: OwnedFd,
}

struct XdpSlot<X> {
    engine: X,
    taps: HashMap<u32, OwnedTap>,
}

enum Slot<K: Kernel> {
    Xdp(XdpSlot<K::Xdp>),
    Sockmap(K::Sockmap),
}

pub(crate) struct Server<K: Kernel> {
    kernel: K,
    slots: HashMap<u32, Slot<K>>,
    next: u32,
}

/// A reply and the descriptors riding it.
type Answer = (Reply, Vec<OwnedFd>);

fn refused(reason: impl Into<String>) -> Answer {
    (
        Reply::Refused {
            reason: reason.into(),
        },
        Vec::new(),
    )
}

fn failed(reason: String) -> Answer {
    (Reply::Failed { reason }, Vec::new())
}

fn reply(reply: Reply) -> Answer {
    (reply, Vec::new())
}

fn unicast(mac: &[u8; 6]) -> bool {
    mac[0] & 1 == 0
}

/// The passed descriptor is a unix socket of `kind`.
fn unix_socket(fd: BorrowedFd<'_>, kind: SocketType) -> bool {
    matches!(rustix::net::sockopt::socket_domain(fd), Ok(AddressFamily::UNIX))
        && matches!(rustix::net::sockopt::socket_type(fd), Ok(t) if t == kind)
}

impl<K: Kernel> Server<K> {
    pub(crate) fn new(kernel: K) -> Self {
        Server {
            kernel,
            slots: HashMap::new(),
            next: 1,
        }
    }

    /// Answer one record. Never panics on input and never makes a kernel
    /// call for a request it refuses.
    pub(crate) fn handle(&mut self, bytes: &[u8], fds: Vec<OwnedFd>) -> Answer {
        let request: Request = match serde_json::from_slice(bytes) {
            Ok(request) => request,
            Err(error) => return refused(format!("not a fast-path request: {error}")),
        };
        if fds.len() != request.fds() {
            return refused(format!(
                "this request carries {} descriptors, not {}",
                request.fds(),
                fds.len()
            ));
        }
        self.dispatch(request, fds)
    }

    fn insert(&mut self, slot: Slot<K>) -> Answer {
        let engine = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.slots.insert(engine, slot);
        reply(Reply::Engine { engine })
    }

    fn xdp(&mut self, engine: u32) -> Result<(&mut K, &mut XdpSlot<K::Xdp>), Answer> {
        match self.slots.get_mut(&engine) {
            Some(Slot::Xdp(slot)) => Ok((&mut self.kernel, slot)),
            _ => Err(refused(format!("no xdp engine {engine} on this connection"))),
        }
    }

    fn sockmap(
        &mut self,
        engine: u32,
    ) -> Result<(&mut K, &mut K::Sockmap), Answer> {
        match self.slots.get_mut(&engine) {
            Some(Slot::Sockmap(slot)) => Ok((&mut self.kernel, slot)),
            _ => Err(refused(format!(
                "no sockmap engine {engine} on this connection"
            ))),
        }
    }

    fn dispatch(&mut self, request: Request, fds: Vec<OwnedFd>) -> Answer {
        macro_rules! get {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(r) => return r,
                }
            };
        }
        match request {
            Request::XdpLoad {} | Request::SockmapLoad {} if self.slots.len() >= MAX_ENGINES => {
                refused(format!("this connection already holds {MAX_ENGINES} engines"))
            }
            Request::XdpLoad {} => match self.kernel.xdp_load() {
                Ok(engine) => self.insert(Slot::Xdp(XdpSlot {
                    engine,
                    taps: HashMap::new(),
                })),
                Err(e) => failed(e),
            },
            Request::SockmapLoad {} => match self.kernel.sockmap_load() {
                Ok(engine) => self.insert(Slot::Sockmap(engine)),
                Err(e) => failed(e),
            },
            Request::XdpTap {
                engine,
                queue_pairs,
            } => {
                let (kernel, slot) = get!(self.xdp(engine));
                if let Some(pairs) = queue_pairs
                    && !(1..=MAX_FDS as u16).contains(&pairs)
                {
                    return refused(format!("a tap takes 1..={MAX_FDS} queue pairs"));
                }
                if slot.taps.len() >= MAX_PORTS as usize {
                    return refused(format!("engine {engine} already holds {MAX_PORTS} taps"));
                }
                let (name, ifindex, queues) = match kernel.tap_create(queue_pairs) {
                    Ok(t) => t,
                    Err(e) => return failed(e),
                };
                let kept = match queues.first().map(|q| q.as_fd().try_clone_to_owned()) {
                    Some(Ok(fd)) => fd,
                    Some(Err(e)) => return failed(format!("keeping a queue of {name}: {e}")),
                    None => return failed(format!("{name} came back with no queue")),
                };
                if let Err(e) = kernel.xdp_attach(&mut slot.engine, &name) {
                    return failed(e);
                }
                slot.taps.insert(ifindex, OwnedTap { _queue: kept });
                (Reply::Tap { name, ifindex }, queues)
            }
            Request::XdpPort {
                engine,
                ifindex,
                tag,
                flags,
            } => {
                let (kernel, slot) = get!(self.xdp(engine));
                if !slot.taps.contains_key(&ifindex) {
                    return refused(format!(
                        "ifindex {ifindex} is not a tap engine {engine} created"
                    ));
                }
                if tag >= MAX_PORTS {
                    return refused(format!("tag {tag} is past the {MAX_PORTS} devmap slots"));
                }
                if flags & !(XDP_FLAG_ISOLATED | XDP_FLAG_HOST) != 0 {
                    return refused(format!("unknown port flags {flags:#x}"));
                }
                if (tag == HOST_TAG) != (flags & XDP_FLAG_HOST != 0) {
                    return refused("only the host tap takes slot 0, and it takes only slot 0");
                }
                match kernel.xdp_port(&mut slot.engine, tag, ifindex, flags) {
                    Ok(()) => reply(Reply::Done),
                    Err(e) => failed(e),
                }
            }
            Request::XdpRelease { engine, ifindex } => {
                let (kernel, slot) = get!(self.xdp(engine));
                if slot.taps.remove(&ifindex).is_none() {
                    return refused(format!(
                        "ifindex {ifindex} is not a tap engine {engine} created"
                    ));
                }
                kernel.xdp_unport(&mut slot.engine, ifindex);
                reply(Reply::Done)
            }
            Request::XdpMac { engine, mac, tag } => {
                let (kernel, slot) = get!(self.xdp(engine));
                if !unicast(&mac) {
                    return refused("only a unicast MAC enters the forwarding table");
                }
                if let Some(tag) = tag
                    && (tag == HOST_TAG || tag >= MAX_PORTS)
                {
                    return refused(format!("tag {tag} is not a guest slot"));
                }
                match kernel.xdp_mac(&mut slot.engine, mac, tag) {
                    Ok(()) => reply(Reply::Done),
                    Err(e) => failed(e),
                }
            }
            Request::XdpStats { engine } => {
                let (kernel, slot) = get!(self.xdp(engine));
                reply(Reply::XdpStats {
                    counters: kernel.xdp_stats(&slot.engine),
                })
            }
            Request::SockmapStream { engine, id } => {
                let (kernel, slot) = get!(self.sockmap(engine));
                if !unix_socket(fds[0].as_fd(), SocketType::STREAM) {
                    return refused("only a unix stream socket joins the sockmap");
                }
                match kernel.sockmap_stream(slot, id, fds[0].as_fd()) {
                    Ok(()) => reply(Reply::Done),
                    Err(e) => failed(e),
                }
            }
            Request::SockmapTx { engine, id } => {
                let (kernel, slot) = get!(self.sockmap(engine));
                if !fds
                    .iter()
                    .all(|fd| unix_socket(fd.as_fd(), SocketType::DGRAM))
                {
                    return refused("a TX loopback is two unix datagram sockets");
                }
                match kernel.sockmap_tx(slot, id, fds[0].as_fd(), fds[1].as_fd()) {
                    Ok(cookie) => reply(Reply::Cookie { cookie }),
                    Err(e) => failed(e),
                }
            }
            Request::SockmapMac { engine, mac, id } => {
                let (kernel, slot) = get!(self.sockmap(engine));
                if !unicast(&mac) {
                    return refused("only a unicast MAC enters the forwarding table");
                }
                match kernel.sockmap_mac(slot, mac, id) {
                    Ok(()) => reply(Reply::Done),
                    Err(e) => failed(e),
                }
            }
            Request::SockmapUnregister {
                engine,
                id,
                tx_cookie,
                macs,
            } => {
                let (kernel, slot) = get!(self.sockmap(engine));
                if macs.len() > MAX_MACS {
                    return refused(format!("at most {MAX_MACS} MACs per unregister"));
                }
                kernel.sockmap_unregister(slot, id, tx_cookie, &macs);
                reply(Reply::Done)
            }
            Request::SockmapStats { engine } => {
                let (kernel, slot) = get!(self.sockmap(engine));
                let (frames, bytes) = kernel.sockmap_stats(slot);
                reply(Reply::SockmapStats { frames, bytes })
            }
            Request::SockmapDebug { engine } => {
                let (kernel, slot) = get!(self.sockmap(engine));
                reply(Reply::Debug {
                    text: kernel.sockmap_debug(slot),
                })
            }
            Request::Unload { engine } => match self.slots.remove(&engine) {
                Some(_) => reply(Reply::Done),
                None => refused(format!("no engine {engine} on this connection")),
            },
        }
    }
}

/// Serve one daemon until it closes its end.
pub(crate) fn serve<K: Kernel>(sock: OwnedFd, kernel: K) -> std::io::Result<()> {
    let mut server = Server::new(kernel);
    loop {
        let (reply, out) = match proto::recv(sock.as_fd()) {
            Ok(None) => return Ok(()),
            Ok(Some((bytes, fds))) => server.handle(&bytes, fds),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                refused(format!("not a fast-path request: {error}"))
            }
            Err(error) => return Err(error),
        };
        let borrowed: Vec<BorrowedFd<'_>> = out.iter().map(|fd| fd.as_fd()).collect();
        proto::send(sock.as_fd(), &reply, &borrowed)?;
    }
}

/// The real kernel: aya and the tap crate.
pub(crate) struct Aya;

impl Kernel for Aya {
    type Xdp = super::super::engine::XdpEngine;
    type Sockmap = super::super::engine::SockmapEngine;

    fn xdp_load(&mut self) -> Result<Self::Xdp, String> {
        Self::Xdp::load().map_err(|e| format!("{e:#}"))
    }

    fn tap_create(
        &mut self,
        queue_pairs: Option<u16>,
    ) -> Result<(String, u32, Vec<OwnedFd>), String> {
        use super::super::engine::{create_tap, ifindex};
        let (name, queues) = create_tap(queue_pairs).map_err(|e| format!("{e:#}"))?;
        let ifindex = ifindex(&name).map_err(|e| format!("{e:#}"))?;
        Ok((name, ifindex, queues))
    }

    fn xdp_attach(&mut self, xdp: &mut Self::Xdp, ifname: &str) -> Result<(), String> {
        xdp.attach(ifname).map_err(|e| format!("{e:#}"))
    }

    fn xdp_port(
        &mut self,
        xdp: &mut Self::Xdp,
        tag: u32,
        ifindex: u32,
        flags: u32,
    ) -> Result<(), String> {
        xdp.set_port(tag, ifindex, flags)
            .map_err(|e| format!("{e:#}"))
    }

    fn xdp_unport(&mut self, xdp: &mut Self::Xdp, ifindex: u32) {
        xdp.remove_port(ifindex);
    }

    fn xdp_mac(
        &mut self,
        xdp: &mut Self::Xdp,
        mac: [u8; 6],
        tag: Option<u32>,
    ) -> Result<(), String> {
        match tag {
            Some(tag) => xdp.insert_mac(mac, tag).map_err(|e| format!("{e:#}")),
            None => {
                xdp.remove_mac(mac);
                Ok(())
            }
        }
    }

    fn xdp_stats(&mut self, xdp: &Self::Xdp) -> [u64; 4] {
        xdp.stats()
    }

    fn sockmap_load(&mut self) -> Result<Self::Sockmap, String> {
        Self::Sockmap::load().map_err(|e| format!("{e:#}"))
    }

    fn sockmap_stream(
        &mut self,
        sockmap: &mut Self::Sockmap,
        id: u64,
        sock: BorrowedFd<'_>,
    ) -> Result<(), String> {
        sockmap
            .register_stream(id, sock)
            .map_err(|e| format!("{e:#}"))
    }

    fn sockmap_tx(
        &mut self,
        sockmap: &mut Self::Sockmap,
        id: u64,
        kernel_end: BorrowedFd<'_>,
        user_end: BorrowedFd<'_>,
    ) -> Result<u64, String> {
        sockmap
            .register_tx(id, kernel_end, user_end)
            .map_err(|e| format!("{e:#}"))
    }

    fn sockmap_mac(
        &mut self,
        sockmap: &mut Self::Sockmap,
        mac: [u8; 6],
        id: Option<u64>,
    ) -> Result<(), String> {
        match id {
            Some(id) => sockmap.insert_mac(mac, id).map_err(|e| format!("{e:#}")),
            None => {
                sockmap.remove_mac(mac);
                Ok(())
            }
        }
    }

    fn sockmap_unregister(
        &mut self,
        sockmap: &mut Self::Sockmap,
        id: u64,
        tx_cookie: u64,
        macs: &[[u8; 6]],
    ) {
        sockmap.unregister(id, tx_cookie, macs);
    }

    fn sockmap_stats(&mut self, sockmap: &Self::Sockmap) -> (u64, u64) {
        sockmap.stats()
    }

    fn sockmap_debug(&mut self, sockmap: &Self::Sockmap) -> String {
        sockmap.debug_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixDatagram, UnixStream};

    /// Records every kernel call; taps are datagram socket stand-ins.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        next_ifindex: u32,
    }

    impl Kernel for Fake {
        type Xdp = ();
        type Sockmap = ();
        fn xdp_load(&mut self) -> Result<(), String> {
            self.calls.push("xdp_load".into());
            Ok(())
        }
        fn tap_create(&mut self, q: Option<u16>) -> Result<(String, u32, Vec<OwnedFd>), String> {
            self.next_ifindex += 1;
            self.calls.push(format!("tap_create {q:?}"));
            let n = q.unwrap_or(1);
            let queues = (0..n)
                .map(|_| UnixDatagram::pair().unwrap().0.into())
                .collect();
            Ok((format!("vmfp{}", self.next_ifindex), 100 + self.next_ifindex, queues))
        }
        fn xdp_attach(&mut self, _: &mut (), name: &str) -> Result<(), String> {
            self.calls.push(format!("attach {name}"));
            Ok(())
        }
        fn xdp_port(&mut self, _: &mut (), tag: u32, ifindex: u32, flags: u32) -> Result<(), String> {
            self.calls.push(format!("port {tag} {ifindex} {flags}"));
            Ok(())
        }
        fn xdp_unport(&mut self, _: &mut (), ifindex: u32) {
            self.calls.push(format!("unport {ifindex}"));
        }
        fn xdp_mac(&mut self, _: &mut (), _: [u8; 6], tag: Option<u32>) -> Result<(), String> {
            self.calls.push(format!("xdp_mac {tag:?}"));
            Ok(())
        }
        fn xdp_stats(&mut self, _: &()) -> [u64; 4] {
            [1, 2, 3, 4]
        }
        fn sockmap_load(&mut self) -> Result<(), String> {
            self.calls.push("sockmap_load".into());
            Ok(())
        }
        fn sockmap_stream(&mut self, _: &mut (), id: u64, _: BorrowedFd<'_>) -> Result<(), String> {
            self.calls.push(format!("stream {id}"));
            Ok(())
        }
        fn sockmap_tx(
            &mut self,
            _: &mut (),
            id: u64,
            _: BorrowedFd<'_>,
            _: BorrowedFd<'_>,
        ) -> Result<u64, String> {
            self.calls.push(format!("tx {id}"));
            Ok(77)
        }
        fn sockmap_mac(&mut self, _: &mut (), _: [u8; 6], id: Option<u64>) -> Result<(), String> {
            self.calls.push(format!("sockmap_mac {id:?}"));
            Ok(())
        }
        fn sockmap_unregister(&mut self, _: &mut (), id: u64, _: u64, _: &[[u8; 6]]) {
            self.calls.push(format!("unregister {id}"));
        }
        fn sockmap_stats(&mut self, _: &()) -> (u64, u64) {
            (5, 6)
        }
        fn sockmap_debug(&mut self, _: &()) -> String {
            "debug".into()
        }
    }

    fn ask(server: &mut Server<Fake>, request: &Request, fds: Vec<OwnedFd>) -> Answer {
        server.handle(&serde_json::to_vec(request).unwrap(), fds)
    }

    fn is_refused(reply: &Reply) -> bool {
        matches!(reply, Reply::Refused { .. })
    }

    fn load_xdp(server: &mut Server<Fake>) -> u32 {
        match ask(server, &Request::XdpLoad {}, vec![]).0 {
            Reply::Engine { engine } => engine,
            other => panic!("{other:?}"),
        }
    }

    fn tap(server: &mut Server<Fake>, engine: u32) -> u32 {
        match ask(
            server,
            &Request::XdpTap {
                engine,
                queue_pairs: None,
            },
            vec![],
        ) {
            (Reply::Tap { ifindex, .. }, fds) => {
                assert_eq!(fds.len(), 1);
                ifindex
            }
            other => panic!("{:?}", other.0),
        }
    }

    #[test]
    fn foreign_operations_are_refused_without_a_kernel_call() {
        let mut server = Server::new(Fake::default());
        for raw in [
            &br#"{"op":"exec","argv":["/bin/sh"]}"#[..],
            br#"{"op":"xdp_attach","engine":1,"ifname":"eth0"}"#,
            br#"{"op":"prog_load","bytes":"AAAA"}"#,
            br#"{"op":"xdp_load","object":"/tmp/x.o"}"#,
            b"\x00\x01garbage",
            b"",
        ] {
            let (reply, fds) = server.handle(raw, vec![]);
            assert!(is_refused(&reply), "{raw:?}: {reply:?}");
            assert!(fds.is_empty());
        }
        assert!(server.kernel.calls.is_empty(), "{:?}", server.kernel.calls);
    }

    #[test]
    fn a_devmap_slot_only_points_at_a_tap_this_helper_created() {
        let mut server = Server::new(Fake::default());
        let engine = load_xdp(&mut server);
        let own = tap(&mut server, engine);
        // ifindex 1 is the loopback; 2 is usually the host NIC.
        for foreign in [1, 2, own + 1] {
            let (reply, _) = ask(
                &mut server,
                &Request::XdpPort {
                    engine,
                    ifindex: foreign,
                    tag: 1,
                    flags: 0,
                },
                vec![],
            );
            assert!(is_refused(&reply), "ifindex {foreign}: {reply:?}");
        }
        let (reply, _) = ask(
            &mut server,
            &Request::XdpPort {
                engine,
                ifindex: own,
                tag: 1,
                flags: 0,
            },
            vec![],
        );
        assert_eq!(reply, Reply::Done);
        // Released, it is foreign again.
        let release = Request::XdpRelease {
            engine,
            ifindex: own,
        };
        assert_eq!(ask(&mut server, &release, vec![]).0, Reply::Done);
        assert!(is_refused(&ask(&mut server, &release, vec![]).0));
        let (reply, _) = ask(
            &mut server,
            &Request::XdpPort {
                engine,
                ifindex: own,
                tag: 1,
                flags: 0,
            },
            vec![],
        );
        assert!(is_refused(&reply));
    }

    #[test]
    fn slots_flags_and_macs_are_bounded() {
        let mut server = Server::new(Fake::default());
        let engine = load_xdp(&mut server);
        let own = tap(&mut server, engine);
        let port = |tag, flags| Request::XdpPort {
            engine,
            ifindex: own,
            tag,
            flags,
        };
        for bad in [
            port(MAX_PORTS, 0),
            port(1, 4),
            port(HOST_TAG, 0),
            port(1, XDP_FLAG_HOST),
            Request::XdpMac {
                engine,
                mac: [0xff; 6],
                tag: Some(1),
            },
            Request::XdpMac {
                engine,
                mac: [0x01, 0, 0x5e, 0, 0, 1],
                tag: Some(1),
            },
            Request::XdpMac {
                engine,
                mac: [2, 0, 0, 0, 0, 1],
                tag: Some(HOST_TAG),
            },
            Request::XdpTap {
                engine,
                queue_pairs: Some(0),
            },
            Request::XdpTap {
                engine,
                queue_pairs: Some(32),
            },
        ] {
            assert!(is_refused(&ask(&mut server, &bad, vec![]).0), "{bad:?}");
        }
        assert_eq!(
            ask(&mut server, &port(HOST_TAG, XDP_FLAG_HOST), vec![]).0,
            Reply::Done
        );
        assert_eq!(
            ask(&mut server, &port(5, XDP_FLAG_ISOLATED), vec![]).0,
            Reply::Done
        );
    }

    #[test]
    fn engines_are_per_connection_and_typed() {
        let mut server = Server::new(Fake::default());
        let xdp = load_xdp(&mut server);
        let sockmap = match ask(&mut server, &Request::SockmapLoad {}, vec![]).0 {
            Reply::Engine { engine } => engine,
            other => panic!("{other:?}"),
        };
        assert!(is_refused(&ask(&mut server, &Request::XdpStats { engine: sockmap }, vec![]).0));
        assert!(is_refused(&ask(&mut server, &Request::SockmapStats { engine: xdp }, vec![]).0));
        assert!(is_refused(&ask(&mut server, &Request::XdpStats { engine: 999 }, vec![]).0));
        assert_eq!(
            ask(&mut server, &Request::XdpStats { engine: xdp }, vec![]).0,
            Reply::XdpStats {
                counters: [1, 2, 3, 4]
            }
        );
        assert_eq!(ask(&mut server, &Request::Unload { engine: xdp }, vec![]).0, Reply::Done);
        assert!(is_refused(&ask(&mut server, &Request::XdpStats { engine: xdp }, vec![]).0));
        for _ in server.slots.len()..MAX_ENGINES {
            load_xdp(&mut server);
        }
        assert!(is_refused(&ask(&mut server, &Request::XdpLoad {}, vec![]).0));
    }

    #[test]
    fn sockmap_takes_only_the_unix_sockets_it_expects() {
        let mut server = Server::new(Fake::default());
        let engine = match ask(&mut server, &Request::SockmapLoad {}, vec![]).0 {
            Reply::Engine { engine } => engine,
            other => panic!("{other:?}"),
        };
        let stream = Request::SockmapStream { engine, id: 1 };
        let tx = Request::SockmapTx { engine, id: 1 };
        let dgram = || -> OwnedFd { UnixDatagram::pair().unwrap().0.into() };
        let unix_stream = || -> OwnedFd { UnixStream::pair().unwrap().0.into() };
        let udp = || -> OwnedFd { std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into() };
        let file = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };

        // Wrong count, wrong kind, wrong family, not a socket.
        assert!(is_refused(&ask(&mut server, &stream, vec![]).0));
        assert!(is_refused(&ask(&mut server, &stream, vec![unix_stream(), unix_stream()]).0));
        assert!(is_refused(&ask(&mut server, &stream, vec![dgram()]).0));
        assert!(is_refused(&ask(&mut server, &stream, vec![file()]).0));
        assert!(is_refused(&ask(&mut server, &tx, vec![dgram(), udp()]).0));
        assert!(is_refused(&ask(&mut server, &tx, vec![unix_stream(), dgram()]).0));
        assert!(is_refused(&ask(&mut server, &Request::SockmapStats { engine }, vec![dgram()]).0));
        assert_eq!(server.kernel.calls, vec!["sockmap_load"]);

        assert_eq!(ask(&mut server, &stream, vec![unix_stream()]).0, Reply::Done);
        assert_eq!(
            ask(&mut server, &tx, vec![dgram(), dgram()]).0,
            Reply::Cookie { cookie: 77 }
        );
        assert!(is_refused(
            &ask(
                &mut server,
                &Request::SockmapMac {
                    engine,
                    mac: [0xff; 6],
                    id: Some(1)
                },
                vec![]
            )
            .0
        ));
    }

    #[test]
    fn the_loop_answers_refusals_and_ends_with_its_peer() {
        let (daemon, helper) = proto::pair().unwrap();
        let worker = std::thread::spawn(move || serve(helper, Fake::default()));
        rustix::net::send(&daemon, br#"{"op":"exec"}"#, rustix::net::SendFlags::empty()).unwrap();
        let (bytes, _) = proto::recv(daemon.as_fd()).unwrap().unwrap();
        assert!(is_refused(&serde_json::from_slice(&bytes).unwrap()));
        proto::send(daemon.as_fd(), &Request::XdpLoad {}, &[]).unwrap();
        let (bytes, _) = proto::recv(daemon.as_fd()).unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Reply>(&bytes).unwrap(),
            Reply::Engine { engine: 1 }
        );
        drop(daemon);
        worker.join().unwrap().unwrap();
    }
}
