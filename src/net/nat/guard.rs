//! The host guard: which of the host's own addresses a NAT flow may not
//! land on (PRD §9.7, §21.4, ADR-0019).
//!
//! **Every engine** refuses the host's loopback, `127.0.0.0/8` and
//! `0.0.0.0/8` ([`is_loopback`]): a service bound to loopback is bound there
//! to be reachable from the host only, and a guest's flow leaves the engine
//! as a connection from the host itself. An engine with no guard refuses all
//! of it. A segment's guard ([`HostGuard::loopback`]) admits what the lab
//! put there: its own host services, and the target of a `redirect` the
//! operator declared onto loopback (the [`Grant`]).
//!
//! **Worktree isolation** ([`HostGuard::new`]) refuses more.
//!
//! Labs of one worktree family run clones of the same guests, so no NAT flow
//! may land on the host itself: loopback, the unspecified address, and every
//! address on a host interface. Otherwise a worktree guest reaches the main
//! lab's port forwards through the host's LAN address, and its own clone
//! answers.
//!
//! The decision is [`refuses`], pure over the host's addresses and the ports
//! the lab exposes. [`HostGuard`] feeds it: it re-reads the interface
//! addresses at decision time, at most once per [`REFRESH`], so an address
//! that appears after the lab started (a VPN, a DHCP renewal) is covered from
//! the next new flow on. The engine asks only for new flows: a TCP SYN, a UDP
//! datagram, an ICMP echo.
//!
//! The one exception is the lab's own host services, the TCP ports a gateway
//! `redirect` rewrites onto `127.0.0.1` ([`crate::labd::netservices`]): the
//! lab's `smbd` behind gateway:445 above all. Those stay reachable on
//! `127.0.0.1` exactly, by TCP, and nothing else on the host does.
//!
//! The NAT engine is IPv4 only, so `::1` and the host's IPv6 addresses are
//! never a NAT destination to begin with.

use crate::sync::LockRecover;
use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How stale the host's address list may be when a new flow is decided.
pub const REFRESH: Duration = Duration::from_secs(1);

/// What a new flow is, as far as the guard cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    Tcp { port: u16 },
    Udp { port: u16 },
    Icmp,
}

/// Whether `dst` is the host's loopback as a connect reaches it:
/// `127.0.0.0/8`, or `0.0.0.0/8`, which the kernel delivers to the host
/// itself. No engine lets a guest's flow land there on its own (§9.7).
pub fn is_loopback(dst: Ipv4Addr) -> bool {
    dst.is_loopback() || dst.octets()[0] == 0
}

/// Whether `dst` is the host itself: loopback, `0.0.0.0/8`, or one of
/// `host_addrs` (every address and broadcast address on a host interface).
pub fn is_host_address(dst: Ipv4Addr, host_addrs: &HashSet<Ipv4Addr>) -> bool {
    is_loopback(dst) || host_addrs.contains(&dst)
}

/// Whether a new flow to `dst` is refused, given the host's addresses the
/// guard covers: all of them under worktree isolation, none (loopback
/// alone) outside it.
/// `exposed` are the lab's own host services, TCP ports on `127.0.0.1`.
pub fn refuses(
    dst: Ipv4Addr,
    kind: FlowKind,
    host_addrs: &HashSet<Ipv4Addr>,
    exposed: &[u16],
) -> bool {
    if let FlowKind::Tcp { port } = kind
        && dst == Ipv4Addr::LOCALHOST
        && exposed.contains(&port)
    {
        return false;
    }
    is_host_address(dst, host_addrs)
}

/// Every IPv4 address, and broadcast address, on the host's interfaces.
pub fn host_addresses() -> HashSet<Ipv4Addr> {
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        tracing::warn!("worktree isolation: cannot list the host's interface addresses");
        return HashSet::new();
    };
    addrs
        .flat_map(|ifa| [ifa.address, ifa.broadcast])
        .flatten()
        .filter_map(|a| a.as_sockaddr_in().map(|sin| sin.ip()))
        .collect()
}

/// Where the guard reads the host's addresses from: [`host_addresses`],
/// replaced only by tests.
pub type AddressSource = Box<dyn Fn() -> HashSet<Ipv4Addr> + Send + Sync>;

/// What the lab itself pointed at the host: asked for a new flow before it
/// is refused, and a `true` admits it. Outside isolation this is the target
/// of a declared `redirect` (§9.9).
pub type Grant = Box<dyn Fn(Ipv4Addr, FlowKind) -> bool + Send + Sync>;

/// The guard one NAT engine consults.
pub struct HostGuard {
    read: AddressSource,
    /// `None` under worktree isolation, which grants nothing.
    grant: Option<Grant>,
    cache: Mutex<Option<(Instant, Arc<HashSet<Ipv4Addr>>)>>,
    /// The lab's own host services on this segment, shared with the
    /// segment's services so a port exposed later is admitted from then on.
    exposed: Arc<Mutex<Vec<u16>>>,
}

impl HostGuard {
    /// The guard of worktree isolation (§21.4): every host address.
    pub fn new(exposed: Arc<Mutex<Vec<u16>>>) -> Self {
        Self::with_source(exposed, Box::new(host_addresses))
    }

    /// The guard of a segment outside isolation (§9.7): the host's loopback
    /// only, less the lab's `exposed` host services and whatever `grant`
    /// admits. The host's interface addresses stay reachable, which is how a
    /// guest reaches a host-side service.
    pub fn loopback(exposed: Arc<Mutex<Vec<u16>>>, grant: Grant) -> Self {
        Self {
            grant: Some(grant),
            ..Self::with_source(exposed, Box::new(HashSet::new))
        }
    }

    /// A guard that refuses nothing, for the engine tests whose "internet"
    /// is a listener on the test host's loopback.
    #[cfg(test)]
    pub fn open() -> Self {
        Self::loopback(Arc::default(), Box::new(|_, _| true))
    }

    pub fn with_source(exposed: Arc<Mutex<Vec<u16>>>, read: AddressSource) -> Self {
        Self {
            read,
            grant: None,
            cache: Mutex::new(None),
            exposed,
        }
    }

    /// The host's addresses, re-read when the last read is older than
    /// [`REFRESH`].
    fn addresses(&self) -> Arc<HashSet<Ipv4Addr>> {
        let mut cache = self.cache.lock_recover();
        match &*cache {
            Some((at, addrs)) if at.elapsed() < REFRESH => addrs.clone(),
            _ => {
                let addrs = Arc::new((self.read)());
                *cache = Some((Instant::now(), addrs.clone()));
                addrs
            }
        }
    }

    /// Whether the engine must refuse a new flow to `dst`.
    pub fn refuses(&self, dst: Ipv4Addr, kind: FlowKind) -> bool {
        let refused = refuses(dst, kind, &self.addresses(), &self.exposed.lock_recover())
            && !self.grant.as_ref().is_some_and(|grant| grant(dst, kind));
        if refused {
            tracing::debug!(%dst, ?kind, "nat: refusing a flow to a host address");
        }
        refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

    fn host() -> HashSet<Ipv4Addr> {
        [
            LAN,
            Ipv4Addr::new(192, 168, 1, 255),
            Ipv4Addr::new(172, 17, 0, 1),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn loopback_unspecified_and_interface_addresses_are_the_host() {
        for dst in [
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(127, 4, 5, 6),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(0, 1, 2, 3),
            LAN,
            Ipv4Addr::new(192, 168, 1, 255),
            Ipv4Addr::new(172, 17, 0, 1),
        ] {
            assert!(is_host_address(dst, &host()), "{dst}");
        }
        for dst in [
            Ipv4Addr::new(192, 168, 1, 21),
            Ipv4Addr::new(1, 1, 1, 1),
            Ipv4Addr::new(10, 213, 0, 1),
        ] {
            assert!(!is_host_address(dst, &host()), "{dst}");
        }
    }

    #[test]
    fn every_protocol_to_the_host_is_refused() {
        for kind in [
            FlowKind::Tcp { port: 8080 },
            FlowKind::Udp { port: 53 },
            FlowKind::Icmp,
        ] {
            assert!(refuses(LAN, kind, &host(), &[]), "{kind:?}");
            assert!(refuses(Ipv4Addr::LOCALHOST, kind, &host(), &[]), "{kind:?}");
            assert!(
                !refuses(Ipv4Addr::new(93, 184, 216, 34), kind, &host(), &[]),
                "{kind:?}"
            );
        }
    }

    /// The lab's `smbd` behind gateway:445 is a NAT flow to `127.0.0.1`, and
    /// it is the only one that survives: same port by UDP, the same port on
    /// another host address, or another loopback port, all stay refused.
    #[test]
    fn an_exposed_host_service_is_reachable_on_loopback_by_tcp_only() {
        let exposed = [44445];
        let tcp = FlowKind::Tcp { port: 44445 };
        assert!(!refuses(Ipv4Addr::LOCALHOST, tcp, &host(), &exposed));
        assert!(refuses(LAN, tcp, &host(), &exposed));
        assert!(refuses(Ipv4Addr::new(127, 0, 0, 2), tcp, &host(), &exposed));
        assert!(refuses(
            Ipv4Addr::LOCALHOST,
            FlowKind::Udp { port: 44445 },
            &host(),
            &exposed
        ));
        assert!(refuses(
            Ipv4Addr::LOCALHOST,
            FlowKind::Tcp { port: 44446 },
            &host(),
            &exposed
        ));
    }

    /// An address the host gains after the lab started is refused once the
    /// cache is older than [`REFRESH`], with no restart.
    #[test]
    fn a_new_interface_address_is_covered_after_a_refresh() {
        let current = Arc::new(Mutex::new(HashSet::from([LAN])));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = {
            let current = current.clone();
            let reads = reads.clone();
            Box::new(move || {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                current.lock_recover().clone()
            })
        };
        let guard = HostGuard::with_source(Arc::default(), source);
        let vpn = Ipv4Addr::new(100, 64, 0, 7);
        assert!(guard.refuses(LAN, FlowKind::Icmp));
        assert!(!guard.refuses(vpn, FlowKind::Icmp));
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1, "cached");

        current.lock_recover().insert(vpn);
        *guard.cache.lock_recover() =
            Some((Instant::now() - REFRESH, Arc::new(HashSet::from([LAN]))));
        assert!(guard.refuses(vpn, FlowKind::Tcp { port: 22 }));
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// Outside isolation the guard covers loopback and `0.0.0.0/8` alone: a
    /// host interface address stays reachable, an exposed service too, and
    /// the grant admits exactly what it names.
    #[test]
    fn the_loopback_guard_refuses_loopback_and_nothing_else() {
        let granted = (Ipv4Addr::LOCALHOST, FlowKind::Udp { port: 5353 });
        let guard = HostGuard::loopback(
            Arc::new(Mutex::new(vec![44445])),
            Box::new(move |dst, kind| (dst, kind) == granted),
        );
        let tcp = FlowKind::Tcp { port: 5432 };
        for dst in [
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(127, 4, 5, 6),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(0, 1, 2, 3),
        ] {
            for kind in [tcp, FlowKind::Udp { port: 53 }, FlowKind::Icmp] {
                assert!(guard.refuses(dst, kind), "{dst} {kind:?}");
            }
        }
        assert!(!guard.refuses(LAN, tcp), "a host interface address");
        assert!(!guard.refuses(Ipv4Addr::new(1, 1, 1, 1), tcp));
        assert!(!guard.refuses(Ipv4Addr::LOCALHOST, FlowKind::Tcp { port: 44445 }));
        assert!(!guard.refuses(granted.0, granted.1));
        assert!(guard.refuses(Ipv4Addr::LOCALHOST, FlowKind::Udp { port: 5354 }));
    }

    /// The real interface list holds at least loopback's own address.
    #[test]
    fn the_host_lists_its_loopback_address() {
        assert!(host_addresses().contains(&Ipv4Addr::LOCALHOST));
    }
}
