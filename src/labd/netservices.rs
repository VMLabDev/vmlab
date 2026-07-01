//! Wire the NAT engine and L3 rule engine into a segment's switch and
//! gateway (PRD §9.6–§9.9). Phase 3 of network assembly, after gateways.
//!
//! - **NAT**: the gateway's uplink hands off-segment frames to the engine;
//!   the engine's output is injected back through the gateway port.
//! - **Rules**: a switch ingress hook evaluates guest→world IPv4 packets
//!   (those addressed to the gateway MAC) — `block` drops with a synthesised
//!   RST/ICMP reply back to the guest, `redirect` DNATs in place.

use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::config::model::MacAddr;
use crate::net::frame::{ETHERTYPE_IPV4, EthView, eth_build};
use crate::net::gateway::GatewayHandle;
use crate::net::nat::{NatConfig, NatEngine};
use crate::net::rules::{RuleSet, Verdict};
use crate::net::switch::{HookAction, PortClass, Switch};

/// Per-segment network services available for runtime mutation from scripts
/// and the CLI (PRD §9.9).
pub struct SegmentServices {
    pub rules: Arc<Mutex<RuleSet>>,
    pub nat: Option<Arc<NatEngine>>,
    /// Active runtime port forwards: rule id → task handle.
    pub forwards: Mutex<Vec<(u64, tokio::task::JoinHandle<()>)>>,
    pub next_forward_id: std::sync::atomic::AtomicU64,
}

impl SegmentServices {
    /// Install NAT (when the segment has egress) and the L3 rule hook on the
    /// switch. `gateway` is this segment's gateway handle.
    pub fn install(
        switch: &Arc<Switch>,
        gateway: &GatewayHandle,
        nat_enabled: bool,
        mtu: u16,
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
                let rs = hook_rules.lock().expect("ruleset poisoned");
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

        let nat = if nat_enabled {
            Some(spawn_nat(switch, gateway, gw_mac, mtu, rules.clone()))
        } else {
            None
        };

        Arc::new(SegmentServices {
            rules,
            nat,
            forwards: Mutex::new(Vec::new()),
            next_forward_id: std::sync::atomic::AtomicU64::new(1),
        })
    }
}

/// Build the NAT engine. Its output (frames toward guests) is injected back
/// through the gateway port; the gateway's uplink feeds it off-segment
/// frames. No extra switch port is needed — the gateway already has one.
fn spawn_nat(
    _switch: &Arc<Switch>,
    gateway: &GatewayHandle,
    gw_mac: MacAddr,
    mtu: u16,
    rules: Arc<Mutex<RuleSet>>,
) -> Arc<NatEngine> {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Bytes>(1024);
    let mut cfg = NatConfig::new(gateway.gw_ip(), gw_mac);
    cfg.mtu = mtu;
    let engine = NatEngine::new(cfg, out_tx);

    // Forward NAT output into the switch via the gateway port. Replies
    // sourced from a redirect target are un-NAT'd first (§9.9) — the guest
    // expects them from the address it originally dialled.
    let inject = gateway.injector();
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let frame = {
                let rewritten = EthView::parse(&frame)
                    .filter(|eth| eth.ethertype() == ETHERTYPE_IPV4)
                    .and_then(|eth| {
                        let verdict = {
                            let rs = rules.lock().expect("ruleset poisoned");
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
            if !inject(frame) {
                tracing::debug!("NAT output dropped: gateway port full/closed");
            }
        }
    });

    // The gateway hands every off-segment frame to the engine. The uplink
    // closure is synchronous; handle_frame is async, so spawn it.
    let engine_uplink = engine.clone();
    gateway.set_uplink(Box::new(move |frame: Bytes| {
        let e = engine_uplink.clone();
        tokio::spawn(async move {
            e.handle_frame(frame).await;
        });
    }));
    engine
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
    let mut rs = services.rules.lock().expect("ruleset poisoned");
    for b in &seg.block_rules {
        rs.add_block(b.clone());
    }
    for r in &seg.redirect_rules {
        rs.add_redirect(r.clone());
    }
}

impl SegmentServices {
    /// Spawn a host→guest port forward (PRD §9.8). Requires NAT on the
    /// segment (the engine originates the guest-side TCP/UDP). Returns a
    /// forward id usable with [`SegmentServices::remove_forward`].
    pub fn add_forward(
        &self,
        host_addr: std::net::SocketAddr,
        guest_ip: std::net::Ipv4Addr,
        guest_port: u16,
        proto: crate::config::model::Proto,
    ) -> Result<u64, String> {
        let engine = self
            .nat
            .as_ref()
            .ok_or("port forwarding requires NAT/egress on the segment")?
            .clone();
        use crate::config::model::Proto;
        use crate::net::nat::PortForwarder;
        let handle = match proto {
            Proto::Udp => PortForwarder::spawn_udp_forward(host_addr, engine, guest_ip, guest_port),
            // "both" forwards TCP (the common case); a second call can add UDP.
            _ => PortForwarder::spawn_tcp_forward(host_addr, engine, guest_ip, guest_port),
        };
        let id = self
            .next_forward_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.forwards
            .lock()
            .expect("forwards poisoned")
            .push((id, handle));
        Ok(id)
    }

    /// Tear down a forward spawned by [`Self::add_forward`]. Declared
    /// forwards live for the lab's lifetime, so nothing calls this yet; it
    /// completes the add/remove contract for dynamic (scripted) forwards.
    #[allow(dead_code)]
    pub fn remove_forward(&self, id: u64) -> bool {
        let mut fwds = self.forwards.lock().expect("forwards poisoned");
        if let Some(pos) = fwds.iter().position(|(fid, _)| *fid == id) {
            let (_, handle) = fwds.remove(pos);
            handle.abort();
            true
        } else {
            false
        }
    }
}
