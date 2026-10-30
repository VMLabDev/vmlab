//! Wire the NAT engine and L3 rule engine into a segment's switch and
//! gateway (PRD §9.6–§9.9). Phase 3 of network assembly, after gateways.
//!
//! - **Routing**: the gateway's uplink offers every off-segment frame to the
//!   lab's inter-segment router first (§9.6); a frame for a connected peer
//!   never reaches NAT.
//! - **NAT**: the gateway's uplink hands the remaining off-segment frames to
//!   the engine; the engine's output is injected back through the gateway
//!   port.
//! - **Rules**: a switch ingress hook evaluates guest→world IPv4 packets
//!   (those addressed to the gateway MAC) — `block` drops with a synthesised
//!   RST/ICMP reply back to the guest, `redirect` DNATs in place.

use crate::sync::LockRecover;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::config::model::MacAddr;
use crate::net::frame::{ETHERTYPE_IPV4, EthView, IPPROTO_TCP, Ipv4View, TcpView, eth_build};
use crate::net::gateway::GatewayHandle;
use crate::net::nat::{HostGuard, NatConfig, NatEngine};
use crate::net::router::Router;
use crate::net::rules::{RuleSet, Verdict};
use crate::net::switch::{HookAction, PortClass, Switch};

/// Per-segment network services available for runtime mutation from scripts
/// and the CLI (PRD §9.9).
pub struct SegmentServices {
    pub rules: Arc<Mutex<RuleSet>>,
    /// The engine giving this segment egress — `None` without `nat`, even
    /// though a segment without egress still runs one for its host services.
    pub nat: Option<Arc<NatEngine>>,
    /// Host loopback TCP ports a guest may reach through a `redirect`
    /// rewriting a gateway address onto one, on a segment without egress or
    /// under worktree isolation (§21.4). Unused on a segment with egress
    /// outside isolation, which reaches every port anyway.
    host_services: Arc<Mutex<Vec<u16>>>,
    /// Active runtime port forwards: rule id → task handle.
    pub forwards: Mutex<Vec<(u64, tokio::task::JoinHandle<()>)>>,
    pub next_forward_id: std::sync::atomic::AtomicU64,
}

impl SegmentServices {
    /// Install NAT (when the segment has egress) and the L3 rule hook on the
    /// switch. `gateway` is this segment's gateway handle; `router` routes
    /// what it can of the gateway's off-segment frames, as `segment`'s.
    /// `isolated` puts the engine under worktree isolation (§21.4): it
    /// refuses every host address but the services this segment exposes.
    pub fn install(
        switch: &Arc<Switch>,
        gateway: &GatewayHandle,
        nat_enabled: bool,
        isolated: bool,
        mtu: u16,
        router: (&Arc<Router>, &str),
    ) -> Arc<SegmentServices> {
        let rules = Arc::new(Mutex::new(RuleSet::new()));
        let gw_mac = gateway.gw_mac();

        // L3 rules: evaluate every guest frame addressed to the gateway MAC
        // (i.e. routed off-segment). Replies travel back out the ingress
        // port wrapped in ethernet (gateway → guest).
        let hook_rules = rules.clone();
        switch.set_ingress_hook(Box::new(move |_port, class, frame| {
            if !matches!(class, PortClass::Guest { .. }) {
                return HookAction::Pass;
            }
            let Some(eth) = EthView::parse(frame) else {
                return HookAction::Pass;
            };
            if eth.dst_mac() != gw_mac || eth.ethertype() != ETHERTYPE_IPV4 {
                return HookAction::Pass;
            }
            let guest_mac = eth.src_mac();
            let ipv4 = eth.payload();
            let verdict = {
                let rs = hook_rules.lock_recover();
                rs.eval(ipv4)
            };
            match verdict {
                Verdict::Pass => HookAction::Pass,
                Verdict::Drop { reply } => {
                    let replies = reply
                        .map(|ip| vec![wrap_eth(gw_mac, guest_mac, &ip)])
                        .unwrap_or_default();
                    HookAction::Inject {
                        forward: None,
                        reply: replies,
                    }
                }
                Verdict::Rewrite(ip) => {
                    HookAction::Replace(eth_build(gw_mac, guest_mac, ETHERTYPE_IPV4, &ip))
                }
            }
        }));

        // A segment without egress still needs the engine for the host
        // services it is offered — the lab's `smbd` behind gateway:445 above
        // all, or a share on an isolated segment could never mount. Its
        // uplink admits those and nothing else.
        let host_services = Arc::new(Mutex::new(Vec::new()));
        let admit = (!nat_enabled).then(|| host_services.clone());
        let engine = spawn_nat(switch, gateway, gw_mac, mtu, rules.clone(), admit, router);
        if isolated {
            engine.set_guard(Arc::new(HostGuard::new(host_services.clone())));
        }
        let nat = nat_enabled.then_some(engine);

        Arc::new(SegmentServices {
            rules,
            nat,
            host_services,
            forwards: Mutex::new(Vec::new()),
            next_forward_id: std::sync::atomic::AtomicU64::new(1),
        })
    }
}

/// Build the NAT engine. Its output (frames toward guests) is injected back
/// through the gateway port; the gateway's uplink feeds it off-segment
/// frames. No extra switch port is needed — the gateway already has one.
///
/// `admit` is `None` for a segment with egress. Otherwise only frames to a
/// host loopback TCP port it lists reach the engine; the rest are dropped,
/// as they were before a segment without egress had an engine at all.
///
/// Every frame is offered to `router` first: one for a connected peer
/// segment is routed there and never reaches the engine (§9.6).
fn spawn_nat(
    _switch: &Arc<Switch>,
    gateway: &GatewayHandle,
    gw_mac: MacAddr,
    mtu: u16,
    rules: Arc<Mutex<RuleSet>>,
    admit: Option<Arc<Mutex<Vec<u16>>>>,
    (router, segment): (&Arc<Router>, &str),
) -> Arc<NatEngine> {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Bytes>(1024);
    let mut cfg = NatConfig::new(gateway.gw_ip(), gw_mac);
    cfg.mtu = mtu;
    let engine = NatEngine::new(cfg, out_tx);

    // Forward NAT output into the switch via the gateway port. Replies
    // sourced from a redirect target are un-NAT'd first (§9.9) — the guest
    // expects them from the address it originally dialled.
    let gateway_tx = gateway.sender();
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let frame = {
                let rewritten = EthView::parse(&frame)
                    .filter(|eth| eth.ethertype() == ETHERTYPE_IPV4)
                    .and_then(|eth| {
                        let verdict = {
                            let rs = rules.lock_recover();
                            rs.eval_return(eth.payload())
                        };
                        match verdict {
                            Verdict::Rewrite(ip) => {
                                Some(wrap_eth(eth.src_mac(), eth.dst_mac(), &ip))
                            }
                            _ => None,
                        }
                    });
                rewritten.unwrap_or(frame)
            };
            if gateway_tx.send(frame).await.is_err() {
                tracing::debug!("NAT output stopped: gateway port closed");
                break;
            }
        }
    });

    // The gateway awaits every off-segment frame in arrival order. This is
    // essential for vTCP: spawning one task per frame reorders bulk streams.
    let engine_uplink = engine.clone();
    let router = router.clone();
    let segment: Arc<str> = segment.into();
    gateway.set_uplink(Arc::new(move |frame: Bytes| {
        let e = engine_uplink.clone();
        let router = router.clone();
        let segment = segment.clone();
        let admit = admit.clone();
        Box::pin(async move {
            let Some(frame) = router.route(&segment, frame).await else {
                return;
            };
            let admitted = admit
                .as_ref()
                .is_none_or(|ports| is_host_service(&frame, &ports.lock_recover()));
            if admitted {
                e.handle_frame(frame).await;
            }
        })
    }));
    engine
}

/// Whether an off-segment frame is TCP to one of `ports` on the host's
/// loopback — where a `redirect` has already rewritten it to (§9.9).
fn is_host_service(frame: &[u8], ports: &[u16]) -> bool {
    EthView::parse(frame)
        .filter(|eth| eth.ethertype() == ETHERTYPE_IPV4)
        .and_then(|eth| Ipv4View::parse(eth.payload()))
        .filter(|ip| ip.dst().is_loopback() && ip.proto() == IPPROTO_TCP)
        .and_then(|ip| TcpView::parse(ip.payload()))
        .is_some_and(|tcp| ports.contains(&tcp.dst_port()))
}

fn wrap_eth(src: MacAddr, dst: MacAddr, ipv4: &[u8]) -> Bytes {
    Bytes::from(eth_build(dst, src, ETHERTYPE_IPV4, ipv4))
}

/// Install the segment's declared `block {}` / `redirect {}` rules (PRD
/// §9.9). Declared `forward {}` rules are wired separately by the lab
/// runtime (they need the guest's leased IP, known only at start).
pub fn preinstall_rules(
    services: &Arc<SegmentServices>,
    seg: &crate::config::model::Segment,
    _lab: &crate::config::model::Lab,
) {
    let mut rs = services.rules.lock_recover();
    for b in &seg.block_rules {
        rs.add_block(b.clone());
    }
    for r in &seg.redirect_rules {
        rs.add_redirect(r.clone());
    }
}

impl SegmentServices {
    /// Let guests reach host loopback TCP `port` even when the segment has
    /// no egress, or runs under worktree isolation. The caller installs the
    /// `redirect` that rewrites a gateway address onto it; this only stops
    /// the missing egress, or isolation's host guard, dropping it.
    pub fn expose_host_service(&self, port: u16) {
        let mut ports = self.host_services.lock_recover();
        if !ports.contains(&port) {
            ports.push(port);
        }
    }

    /// Prime the NAT engine's IP → MAC table. Forwards to a guest that has
    /// never originated egress would otherwise send the SYN in a broadcast
    /// frame — which the guest's TCP stack discards (`pkt_type != HOST`).
    /// labd knows the lease MAC, so it seeds the table when installing a
    /// forward.
    pub fn learn_mac(&self, ip: std::net::Ipv4Addr, mac: crate::config::model::MacAddr) {
        if let Some(engine) = self.nat.as_ref() {
            engine.learn_mac(ip, mac);
        }
    }

    /// Spawn a host→guest port forward (PRD §9.8). Requires NAT on the
    /// segment (the engine originates the guest-side TCP/UDP). Returns a
    /// forward id usable with [`SegmentServices::remove_forward`], and the
    /// host port actually bound (the kernel's pick when `host_addr` asks for
    /// port 0).
    pub fn add_forward(
        &self,
        host_addr: std::net::SocketAddr,
        guest_ip: std::net::Ipv4Addr,
        guest_port: u16,
        proto: crate::config::model::Proto,
    ) -> Result<(u64, u16), String> {
        let engine = self
            .nat
            .as_ref()
            .ok_or("port forwarding requires NAT/egress on the segment")?
            .clone();
        use crate::config::model::Proto;
        use crate::net::nat::PortForwarder;
        let (handle, bound) = match proto {
            Proto::Udp => PortForwarder::spawn_udp_forward(host_addr, engine, guest_ip, guest_port),
            // "both" forwards TCP (the common case); a second call can add UDP.
            _ => PortForwarder::spawn_tcp_forward(host_addr, engine, guest_ip, guest_port),
        }
        .map_err(|e| listen_error(host_addr, &e))?;
        let id = self
            .next_forward_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.forwards.lock_recover().push((id, handle));
        Ok((id, bound.port()))
    }

    /// Tear down a forward spawned by [`Self::add_forward`]. Declared
    /// segment forwards live for the lab's lifetime; container `port {}`
    /// forwards are removed and re-installed when a restart changes the
    /// lease.
    ///
    /// Returns once the listener is closed, so a forward re-installed on the
    /// same host port straight after can bind it. Aborting a task only asks
    /// for it to stop: a rebind issued before the runtime gets round to
    /// dropping the old listener fails with the port still in use.
    pub async fn remove_forward(&self, id: u64) -> bool {
        let handle = {
            let mut fwds = self.forwards.lock_recover();
            match fwds.iter().position(|(fid, _)| *fid == id) {
                Some(pos) => fwds.remove(pos).1,
                None => return false,
            }
        };
        handle.abort();
        let _ = handle.await;
        true
    }
}

/// Why a forward could not listen on `addr`. A `bind` naming an address the
/// host does not have is the one failure a lab file causes on its own, so it
/// says so rather than leaving it to the kernel's wording (§9.8).
fn listen_error(addr: std::net::SocketAddr, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AddrNotAvailable {
        return format!(
            "cannot listen on host {addr}: the host has no address {} (forward `bind`)",
            addr.ip()
        );
    }
    format!("cannot listen on host {addr}: {e}")
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;
    use crate::config::model::{HostPort, RedirectRule};
    use crate::net::frame::{TcpFields, ipv4_build, tcp_build};
    use crate::net::gateway::{Gateway, GatewayConfig, gateway_mac};

    const GUEST_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 0x0A]);
    const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 50);
    const GW_IP: Ipv4Addr = Ipv4Addr::new(10, 213, 1, 1);

    /// A segment's services, its gateway (which stops when dropped), and
    /// one guest port on it.
    fn segment(
        nat: bool,
    ) -> (
        Arc<SegmentServices>,
        GatewayHandle,
        crate::net::switch::ChannelPort,
    ) {
        segment_isolated(nat, false)
    }

    /// [`segment`], under worktree isolation when `isolated`.
    fn segment_isolated(
        nat: bool,
        isolated: bool,
    ) -> (
        Arc<SegmentServices>,
        GatewayHandle,
        crate::net::switch::ChannelPort,
    ) {
        let sw = Switch::new("seg".into());
        let gw_mac = gateway_mac("lab", "seg");
        let gw = Gateway::spawn(
            &sw,
            GatewayConfig {
                segment_name: "seg".into(),
                lab_name: "lab".into(),
                gw_ip: GW_IP,
                gw_mac,
                dhcp: None,
                dns: None,
                upstream_dns: None,
            },
        );
        let services =
            SegmentServices::install(&sw, &gw, nat, isolated, 1500, (&Router::new(), "seg"));
        let guest = sw.add_channel_port(PortClass::Guest { isolated: false });
        (services, gw, guest)
    }

    /// Redirect gateway:445 onto `port` on the host's loopback, the way the
    /// lab runtime points a share's segment at its `smbd`.
    fn redirect_445(services: &SegmentServices, port: u16) {
        services.rules.lock_recover().add_redirect(RedirectRule {
            from: HostPort {
                ip: GW_IP,
                port: Some(445),
            },
            to: HostPort {
                ip: Ipv4Addr::LOCALHOST,
                port: Some(port),
            },
            proto: None,
            span: (0, 0),
        });
    }

    /// A guest's SYN to `dst`:`port`.
    fn syn(dst: Ipv4Addr, port: u16) -> Bytes {
        let tcp = tcp_build(
            GUEST_IP,
            dst,
            TcpFields {
                src_port: 40000,
                dst_port: port,
                seq: 1,
                ack: 0,
                flags: 0x02,
                window: 65535,
                options: &[],
            },
            &[],
        )
        .unwrap();
        let ip = ipv4_build(GUEST_IP, dst, IPPROTO_TCP, 64, &tcp, 1).unwrap();
        Bytes::from(eth_build(
            gateway_mac("lab", "seg"),
            GUEST_MAC,
            ETHERTYPE_IPV4,
            &ip,
        ))
    }

    async fn accepted(listener: &tokio::net::TcpListener) -> bool {
        tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .is_ok()
    }

    /// A share on a segment without `nat` must still reach the lab's `smbd`:
    /// gateway:445 is redirected to a host loopback port, and the segment's
    /// missing egress does not drop it.
    #[tokio::test]
    async fn a_segment_without_egress_reaches_an_exposed_host_service() {
        let smbd = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = smbd.local_addr().unwrap().port();
        let (services, _gw, guest) = segment(false);
        assert!(services.nat.is_none(), "no egress engine without nat");
        redirect_445(&services, port);
        services.expose_host_service(port);

        guest.tx.send(syn(GW_IP, 445)).await.unwrap();
        assert!(
            accepted(&smbd).await,
            "gateway:445 never reached the host service"
        );
    }

    /// Without egress, a loopback port nobody exposed stays unreachable even
    /// through a redirect — the segment gains the services it is given, not
    /// a way out.
    #[tokio::test]
    async fn a_segment_without_egress_reaches_nothing_else() {
        let other = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = other.local_addr().unwrap().port();
        let (services, _gw, guest) = segment(false);
        redirect_445(&services, port);

        guest.tx.send(syn(GW_IP, 445)).await.unwrap();
        assert!(!accepted(&other).await, "an unexposed port was reached");
    }

    /// Under worktree isolation a segment with egress still reaches the
    /// lab's `smbd` behind gateway:445 (§21.4: a gateway service, not a NAT
    /// connection the guest chose)...
    #[tokio::test]
    async fn an_isolated_segment_reaches_its_smbd_through_the_gateway() {
        let smbd = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = smbd.local_addr().unwrap().port();
        let (services, _gw, guest) = segment_isolated(true, true);
        redirect_445(&services, port);
        services.expose_host_service(port);

        guest.tx.send(syn(GW_IP, 445)).await.unwrap();
        assert!(accepted(&smbd).await, "gateway:445 never reached smbd");
    }

    /// ...and nothing else on the host: a listener on loopback, which the
    /// same segment reaches without isolation, is answered with RST.
    #[tokio::test]
    async fn an_isolated_segment_reaches_no_host_port() {
        let other = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = other.local_addr().unwrap().port();

        let (_services, _gw, guest) = segment_isolated(true, false);
        guest.tx.send(syn(Ipv4Addr::LOCALHOST, port)).await.unwrap();
        assert!(
            accepted(&other).await,
            "egress reaches the host without isolation"
        );

        let (_services, _gw, mut guest) = segment_isolated(true, true);
        guest.tx.send(syn(Ipv4Addr::LOCALHOST, port)).await.unwrap();
        assert!(!accepted(&other).await, "isolation let a host port through");
        let reply = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let frame = guest.rx.recv().await.expect("guest port open");
                let tcp = EthView::parse(&frame)
                    .and_then(|eth| Ipv4View::parse(eth.payload()).map(|ip| ip.payload().to_vec()))
                    .and_then(|p| TcpView::parse(&p).map(|t| t.flags()));
                if let Some(flags) = tcp {
                    return flags;
                }
            }
        })
        .await
        .expect("the refused SYN got no reply");
        assert_ne!(reply & crate::net::frame::TCP_RST, 0, "flags {reply:#x}");
    }

    /// A free loopback port, released for the test to claim.
    fn free_port() -> std::net::SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    /// Re-installing a forward on the port it was just removed from binds
    /// every time. Removal used to only abort the listener's task, and on a
    /// multi-threaded runtime the new bind sometimes ran before the old
    /// listener was dropped: the new forward died with the port in use, and
    /// nothing said so (#144).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_forward_reinstalled_on_its_own_port_binds() {
        let (services, _gw, _guest) = segment(true);
        for _ in 0..50 {
            let addr = free_port();
            let (first, _) = services
                .add_forward(addr, GUEST_IP, 80, crate::config::model::Proto::Tcp)
                .unwrap();
            assert!(services.remove_forward(first).await);
            let (second, bound) = services
                .add_forward(addr, GUEST_IP, 80, crate::config::model::Proto::Tcp)
                .expect("the removed forward's port is free again");
            assert_eq!(bound, addr.port());
            tokio::net::TcpStream::connect(addr)
                .await
                .expect("the re-installed forward is listening");
            services.remove_forward(second).await;
        }
    }

    /// A host port something else holds is an error from `add_forward`
    /// itself, for the caller to report, not a warning inside a spawned task.
    #[tokio::test]
    async fn a_forward_on_a_taken_port_is_an_error() {
        let (services, _gw, _guest) = segment(true);
        let holder = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let addr = holder.local_addr().unwrap();
        let err = services
            .add_forward(addr, GUEST_IP, 80, crate::config::model::Proto::Tcp)
            .expect_err("the port is taken");
        assert!(err.contains(&addr.port().to_string()), "{err}");
    }

    /// Port 0 asks the kernel for a free port, and the forward reports the
    /// one it got, listening there.
    #[tokio::test]
    async fn a_forward_on_port_zero_reports_its_bound_port() {
        let (services, _gw, _guest) = segment(true);
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (id, bound) = services
            .add_forward(addr, GUEST_IP, 80, crate::config::model::Proto::Tcp)
            .unwrap();
        assert_ne!(bound, 0);
        tokio::net::TcpStream::connect(("127.0.0.1", bound))
            .await
            .expect("the forward listens on the port it reported");
        services.remove_forward(id).await;
    }

    /// A `bind` the host lacks is named as such; anything else keeps the
    /// kernel's words.
    #[test]
    fn a_listen_failure_names_an_address_the_host_lacks() {
        let addr: std::net::SocketAddr = "192.0.2.7:18490".parse().unwrap();
        let missing = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
        assert_eq!(
            listen_error(addr, &missing),
            "cannot listen on host 192.0.2.7:18490: the host has no address 192.0.2.7 \
             (forward `bind`)"
        );
        let held = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        assert!(listen_error(addr, &held).starts_with("cannot listen on host 192.0.2.7:18490: "));
    }
}
