//! Per-lab network assembly (PRD §9): one switch per segment, subnet
//! allocation from the host pool, NIC listener sockets for QEMU stream
//! netdevs. Gateway services (DHCP/DNS/NAT) attach per segment.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ipnet::Ipv4Net;

use crate::config::model::{Lab, MacAddr, Segment};
use crate::net::dhcp::DhcpConfig;
use crate::net::dns::DnsZone;
use crate::net::fastpath::{self, FastpathTier, NicAttachment, SegmentXdp};
use crate::net::gateway::{Gateway, GatewayConfig, gateway_mac};
use crate::net::router::{Leg, Router};
use crate::net::switch::{PortClass, Switch};
use crate::proto::vocab::GlobalMember;

/// Name of the built-in per-lab NAT segment (`nic { nat = true }`, §9.7).
pub const NAT_SEGMENT: &str = "nat";

/// Default auto-allocation pool (PRD §9.4); /24s carved out of it.
pub const DEFAULT_POOL: &str = "10.213.0.0/16";

pub struct SegmentNet {
    pub name: String,
    pub switch: Arc<Switch>,
    pub subnet: Ipv4Net,
    /// Router address advertised to guests. In dedicated gateway mode this
    /// belongs to the selected VM/container NIC.
    pub gateway_ip: Ipv4Addr,
    /// Address retained by the daemon for DHCP/DNS/SMB. Normally identical
    /// to `gateway_ip`; moved to a free host address when a machine owns it.
    pub service_ip: Ipv4Addr,
    /// Declared config (None for the built-in NAT segment).
    pub config: Option<Segment>,
    /// NAT egress on (declared `nat = true`, or the built-in segment).
    pub nat: bool,
    pub dhcp: bool,
    /// Supervisor-owned shared segment (PRD §9.2): no local gateway; bridged
    /// to the supervisor's global switch via a trunk.
    pub global: bool,
    /// Cross-host peer (`connect { host }`), forwarded to the supervisor.
    pub peer: Option<String>,
    /// This lab's NICs on a global segment, for the supervisor's DNS; filled
    /// by [`LabNetwork::wire_gateways`] once MACs are settled.
    pub global_members: Vec<GlobalMember>,
    /// Gateway service (ARP/ICMP/DHCP/DNS + uplink seam), wired by
    /// [`LabNetwork::wire_gateways`].
    pub gateway: Option<crate::net::gateway::GatewayHandle>,
    /// NAT + L3 rule services (PRD §9.6–§9.9), wired alongside the gateway.
    pub services: Option<Arc<super::netservices::SegmentServices>>,
    listeners: Vec<tokio::task::JoinHandle<()>>,
    /// The afxdp-tier datapath (host tap + XDP program), created lazily on
    /// the first tap NIC when that tier is active.
    xdp: Option<Arc<SegmentXdp>>,
}

/// Default MTU for NAT segments. The guest↔gateway link is an in-memory UNIX
/// socket and NAT terminates TCP, so a jumbo MTU here cuts per-frame overhead
/// with no fragmentation risk (see `net::nat`). Plain L2 and cross-host global
/// segments stay at the classic 1500 — jumbo there would need end-to-end
/// agreement across bridged/peer topologies, so it's opt-in via `mtu`.
pub const JUMBO_MTU: u16 = 9000;
pub const STANDARD_MTU: u16 = 1500;

impl SegmentNet {
    /// The MTU advertised to guests on this segment: an explicit `mtu` attribute
    /// wins; otherwise jumbo on NAT segments and 1500 everywhere else.
    pub fn effective_mtu(&self) -> u16 {
        self.config
            .as_ref()
            .and_then(|c| c.mtu)
            .unwrap_or(if self.nat { JUMBO_MTU } else { STANDARD_MTU })
    }

    /// Listen on a unix socket for one VM NIC; QEMU connects to it.
    pub async fn listen_nic(&mut self, sock: &Path, isolated: bool) -> Result<()> {
        let handle = self
            .switch
            .listen_unix(sock, PortClass::Guest { isolated })
            .await
            .with_context(|| format!("listening on {}", sock.display()))?;
        self.listeners.push(handle);
        Ok(())
    }

    /// Attach one NIC: a tap on the afxdp fast path when that tier is active
    /// (falling back per NIC on any failure), else the stream-socket listener
    /// QEMU connects to.
    ///
    /// `tap_ok` is the machine's ability to consume a pre-opened tap fd, not a
    /// preference — a container micro-VM's argv carries no descriptors, so it
    /// passes `false` and always gets a stream socket.
    pub async fn attach_nic(
        &mut self,
        sock: &Path,
        mac: MacAddr,
        isolated: bool,
        tap_ok: bool,
    ) -> Result<NicAttachment> {
        if tap_ok && fastpath::tier() == FastpathTier::AfXdp {
            match self.attach_tap(mac, isolated) {
                Ok(att) => return Ok(att),
                Err(error) => tracing::warn!(
                    segment = %self.name,
                    "tap fast path failed ({error:#}); nic falls back to a stream socket"
                ),
            }
        }
        self.listen_nic(sock, isolated).await?;
        Ok(NicAttachment::Stream {
            sock: sock.to_path_buf(),
        })
    }

    fn attach_tap(&mut self, mac: MacAddr, isolated: bool) -> Result<NicAttachment> {
        let xdp = match &self.xdp {
            Some(x) => x.clone(),
            None => {
                let x = SegmentXdp::new(&self.name, self.effective_mtu())?;
                self.xdp = Some(x.clone());
                x
            }
        };
        Ok(NicAttachment::Tap(xdp.add_nic(
            &self.switch,
            mac,
            isolated,
        )?))
    }
}

pub struct LabNetwork {
    pub segments: HashMap<String, SegmentNet>,
    /// Daemon inter-segment routing (§9.6): every lab-local segment's
    /// gateway, and the pairs connected by `routes_to` or a script.
    pub router: Arc<Router>,
}

impl LabNetwork {
    /// Build switches and allocate subnets for every declared segment, plus
    /// the built-in NAT segment when any NIC uses `nat = true`.
    pub fn build(lab: &Lab) -> Result<LabNetwork> {
        let pool: Ipv4Net = DEFAULT_POOL.parse().expect("valid pool");
        let declared: Vec<Ipv4Net> = lab.segments.iter().filter_map(|s| s.subnet).collect();

        let mut auto = pool
            .subnets(24)
            .expect("pool splits into /24s")
            .filter(|c| {
                !declared
                    .iter()
                    .any(|d| d.contains(&c.network()) || c.contains(&d.network()))
            });
        let mut alloc_auto = || -> Result<Ipv4Net> {
            auto.next()
                .ok_or_else(|| anyhow::anyhow!("auto-subnet pool exhausted"))
        };

        let mut segments = HashMap::new();
        for seg in &lab.segments {
            let subnet = match seg.subnet {
                Some(s) => s,
                None => alloc_auto()?,
            };
            let gateway_ip = crate::config::validate::gateway_ip(subnet);
            let service_ip = segment_service_ip(lab, &seg.name, subnet, gateway_ip)?;
            segments.insert(
                seg.name.clone(),
                SegmentNet {
                    name: seg.name.clone(),
                    switch: Switch::new(format!("{}/{}", lab.name, seg.name)),
                    subnet,
                    gateway_ip,
                    service_ip,
                    config: Some(seg.clone()),
                    nat: seg.nat,
                    dhcp: seg.dhcp,
                    global: seg.global,
                    peer: seg.connect.as_ref().map(|c| c.host.clone()),
                    global_members: Vec::new(),
                    gateway: None,
                    services: None,
                    listeners: Vec::new(),
                    xdp: None,
                },
            );
        }

        let needs_nat_segment = machine_nics(lab).any(|(_, nics)| nics.iter().any(|n| n.nat));
        if needs_nat_segment {
            if segments.contains_key(NAT_SEGMENT) {
                bail!(
                    "a declared segment is named \"{NAT_SEGMENT}\" while `nic {{ nat = true }}` is \
                     also used — rename the segment (the name is reserved for the built-in NAT \
                     segment, PRD §9.7)"
                );
            }
            let subnet = alloc_auto()?;
            segments.insert(
                NAT_SEGMENT.to_string(),
                SegmentNet {
                    name: NAT_SEGMENT.to_string(),
                    switch: Switch::new(format!("{}/{}", lab.name, NAT_SEGMENT)),
                    subnet,
                    gateway_ip: crate::config::validate::gateway_ip(subnet),
                    service_ip: crate::config::validate::gateway_ip(subnet),
                    config: None,
                    nat: true,
                    dhcp: true,
                    global: false,
                    peer: None,
                    global_members: Vec::new(),
                    gateway: None,
                    services: None,
                    listeners: Vec::new(),
                    xdp: None,
                },
            );
        }

        Ok(LabNetwork {
            segments,
            router: Router::new(),
        })
    }

    pub fn segment_mut(&mut self, name: &str) -> Option<&mut SegmentNet> {
        self.segments.get_mut(name)
    }

    /// Bridge each global segment to the supervisor's global switch (PRD
    /// §9.2): ask the supervisor to attach (creating the shared segment on
    /// first use), then connect this segment's local switch to the returned
    /// trunk socket. The supervisor runs the shared DHCP/DNS.
    pub async fn attach_globals(&mut self, lab: &str) -> anyhow::Result<()> {
        let supervisor_sock = crate::paths::supervisor_socket();
        for seg in self.segments.values_mut() {
            if !seg.global {
                continue;
            }
            let client = crate::proto::client::SupClient::connect(&supervisor_sock)
                .await
                .context("connecting to supervisor for global segment")?;
            let resp = client
                .send(crate::proto::SupRequest::GlobalAttach {
                    name: seg.name.clone(),
                    subnet: seg.config.as_ref().and_then(|c| c.subnet),
                    peer: seg.peer.clone(),
                    lab: Some(lab.to_string()),
                    members: seg.global_members.clone(),
                })
                .await
                .map_err(|e| anyhow::anyhow!("global.attach: {e}"))?;
            let trunk_sock = std::path::PathBuf::from(
                resp["socket"]
                    .as_str()
                    .context("malformed global.attach response")?,
            );
            let stream = tokio::net::UnixStream::connect(&trunk_sock)
                .await
                .with_context(|| format!("connecting global trunk {}", trunk_sock.display()))?;
            let _port = seg
                .switch
                .add_stream_port(stream, crate::net::switch::PortClass::Service)
                .await;
            tracing::info!("bridged global segment \"{}\" to supervisor", seg.name);
        }
        Ok(())
    }

    /// Detach this lab's global segments from the supervisor (on shutdown).
    pub async fn detach_globals(&self, lab: &str) {
        let names: Vec<String> = self
            .segments
            .values()
            .filter(|s| s.global)
            .map(|s| s.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        if let Ok(client) =
            crate::proto::client::SupClient::connect(&crate::paths::supervisor_socket()).await
        {
            for name in names {
                let _ = client
                    .send(crate::proto::SupRequest::GlobalDetach {
                        name,
                        lab: Some(lab.to_string()),
                    })
                    .await;
            }
        }
    }

    /// Phase 2: attach a gateway service to every segment (PRD §9.4–§9.6).
    /// Called after VM MACs are settled so static IPs become DHCP
    /// reservations keyed on the persisted MAC, and a lease→DNS sync task
    /// keeps `<vm>.<lab>.<suffix>` registrations current.
    pub fn wire_gateways(
        &mut self,
        lab: &Lab,
        macs_by_vm: &HashMap<String, Vec<MacAddr>>,
        host: &crate::config::host::HostConfig,
    ) {
        let upstream = host.upstream_resolver();
        let router = self.router.clone();

        for seg in self.segments.values_mut() {
            if seg.global {
                // Global segments are gatewayed by the supervisor; the lab
                // daemon only bridges its local switch over a trunk, and
                // tells the supervisor which of its NICs sit there so the
                // shared DNS can name them (§9.2).
                seg.global_members = global_members(lab, &seg.name, macs_by_vm);
                continue;
            }
            let gw_mac = gateway_mac(&lab.name, &seg.name);
            let mtu = seg.effective_mtu();
            let router_ip = machine_nics(lab)
                .flat_map(|(_, nics)| nics)
                .find(|nic| nic.gateway && nic_segment_name(nic) == seg.name)
                .and_then(|nic| nic.ip)
                .unwrap_or(seg.gateway_ip);

            // -- DHCP -------------------------------------------------------
            let seg_dns = seg
                .config
                .as_ref()
                .map(|c| c.dns.clone())
                .unwrap_or_default();
            let dns_enabled = !seg_dns.declared || seg_dns.enabled;
            let dhcp = if seg.dhcp {
                let mut cfg = DhcpConfig::new(seg.subnet, seg.service_ip, gw_mac);
                // Dedicated gateway mode gives the router address to a
                // VM/container. The daemon keeps DHCP/DNS on `service_ip`.
                cfg.router = router_ip;
                // DNS option (§9.5): segment override > daemon gateway;
                // suppressed entirely with `dns { enabled = false }`.
                cfg.dns_server = if !dns_enabled {
                    None
                } else {
                    Some(seg_dns.server.unwrap_or(seg.service_ip))
                };
                cfg.domain = Some(format!("{}.{}", lab.name, host.dns_suffix));
                cfg.mtu = mtu;
                if let Some(c) = &seg.config {
                    cfg.routes = c.routes.iter().map(|r| (r.dest, r.via)).collect();
                }
                // Static IPs → reservations keyed on the persisted MAC (§9.4).
                // Containers attach like VMs, so they get the same treatment.
                for (mname, nics) in machine_nics(lab) {
                    for (i, nic) in nics.iter().enumerate() {
                        if nic_segment_name(nic) == seg.name
                            && let Some(ip) = nic.ip
                            && let Some(mac) = macs_by_vm.get(mname).and_then(|m| m.get(i))
                        {
                            cfg.reservations.insert(*mac, ip);
                        }
                    }
                }
                Some(cfg)
            } else {
                None
            };

            // -- DNS zone ---------------------------------------------------
            let dns = if dns_enabled {
                let mut zone = DnsZone::new(&host.dns_suffix);
                // Static-IP guests resolve immediately; dynamic leases are
                // synced by the task below.
                for (mname, nics) in machine_nics(lab) {
                    for nic in nics {
                        if nic_segment_name(nic) == seg.name
                            && let Some(ip) = nic.ip
                        {
                            zone.register(&format!("{mname}.{}", lab.name), ip);
                            zone.register(mname, ip);
                        }
                    }
                }
                for rec in lab
                    .records
                    .iter()
                    .chain(seg.config.iter().flat_map(|c| c.records.iter()))
                {
                    zone.set_static(&rec.name, rec.ip);
                }
                for sink in lab
                    .sinkholes
                    .iter()
                    .chain(seg.config.iter().flat_map(|c| c.sinkholes.iter()))
                {
                    zone.add_sinkhole(&sink.pattern, sink.mode);
                }
                Some(zone)
            } else {
                None
            };

            let handle = Gateway::spawn(
                &seg.switch,
                GatewayConfig {
                    segment_name: seg.name.clone(),
                    lab_name: lab.name.clone(),
                    gw_ip: seg.service_ip,
                    gw_mac,
                    dhcp,
                    dns,
                    upstream_dns: upstream,
                },
            );

            // Lease → DNS registration sync (§9.5 auto-registration).
            if dns_enabled {
                spawn_lease_dns_sync(&handle, &lab.name, macs_by_vm);
            }

            // NAT + L3 rule services on the same switch (§9.6–§9.9). Declared
            // routes' block/redirect/forward rules are pre-installed.
            let services = super::netservices::SegmentServices::install(
                &seg.switch,
                &handle,
                seg.nat,
                mtu,
                (&router, &seg.name),
            );
            if let Some(cfg) = &seg.config {
                super::netservices::preinstall_rules(&services, cfg, lab);
            }

            // The segment as the router sees it; connected below, once
            // every segment is registered.
            router.register(Leg {
                name: seg.name.clone(),
                subnet: seg.subnet,
                gw_ip: seg.service_ip,
                mtu,
                port: handle.l3_port(),
                rules: services.rules.clone(),
                dhcp: handle.dhcp_server(),
                declared_routes: seg
                    .config
                    .iter()
                    .flat_map(|c| c.routes.iter().map(|r| (r.dest, r.via)))
                    .collect(),
            });

            seg.gateway = Some(handle);
            seg.services = Some(services);
        }

        // Declared pairs (§9.6). Validation has refused a global segment on
        // either side and an undeclared name, so every pair connects.
        for (a, b) in declared_route_pairs(lab) {
            if let Err(error) = router.connect(a, b) {
                tracing::warn!("routes_to: {error}");
            }
        }
    }

    /// Connect (`enable`) or disconnect `a` and `b` at runtime — a script's
    /// `seg.route_to()` / `seg.unroute_to()` (§9.6). Guests learn of the
    /// change with their next lease.
    pub fn set_route(&self, a: &str, b: &str, enable: bool) -> Result<(), String> {
        for name in [a, b] {
            match self.segments.get(name) {
                None => return Err(format!("no segment named \"{name}\" in this lab")),
                Some(seg) if seg.global => {
                    return Err(format!(
                        "segment \"{name}\" is global — daemon inter-segment routing is \
                         lab-local (PRD §9.6)"
                    ));
                }
                Some(_) => {}
            }
        }
        if enable {
            self.router.connect(a, b)
        } else {
            self.router.disconnect(a, b)
        }
    }
}

/// Every `routes_to` pair the lab declares, as (declaring segment, target).
/// Declaring a pair on both sides lists it twice; connecting is idempotent.
fn declared_route_pairs(lab: &Lab) -> impl Iterator<Item = (&str, &str)> {
    lab.segments.iter().flat_map(|seg| {
        seg.routes_to
            .iter()
            .map(move |target| (seg.name.as_str(), target.as_str()))
    })
}

/// The NICs `lab` puts on the global segment `segment`, by persisted MAC.
fn global_members(
    lab: &Lab,
    segment: &str,
    macs_by_vm: &HashMap<String, Vec<MacAddr>>,
) -> Vec<GlobalMember> {
    let mut members = Vec::new();
    for (mname, nics) in machine_nics(lab) {
        for (i, nic) in nics.iter().enumerate() {
            if nic_segment_name(nic) == segment
                && let Some(mac) = macs_by_vm.get(mname).and_then(|m| m.get(i))
            {
                members.push(GlobalMember {
                    machine: mname.to_string(),
                    mac: *mac,
                    ip: nic.ip,
                });
            }
        }
    }
    members
}

/// Keep `<vm>.<lab>.<suffix>` (and the short `<vm>` alias) registered for
/// every DHCP lease, matching leases back to VMs via their persisted MACs.
fn spawn_lease_dns_sync(
    gateway: &crate::net::gateway::GatewayHandle,
    lab_name: &str,
    macs_by_vm: &HashMap<String, Vec<MacAddr>>,
) {
    let Some(zone) = gateway.dns_zone() else {
        return;
    };
    let leases_handle = gateway.leases_probe();
    let mac_to_vm: HashMap<MacAddr, String> = macs_by_vm
        .iter()
        .flat_map(|(vm, macs)| macs.iter().map(move |m| (*m, vm.clone())))
        .collect();
    let lab = lab_name.to_string();
    tokio::spawn(async move {
        let mut known: HashMap<MacAddr, Ipv4Addr> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Some(leases) = leases_handle() else { break };
            for (mac, ip) in leases {
                if known.get(&mac) == Some(&ip) {
                    continue;
                }
                if let Some(vm) = mac_to_vm.get(&mac)
                    && let Ok(mut z) = zone.lock()
                {
                    z.register(&format!("{vm}.{lab}"), ip);
                    z.register(vm, ip);
                    known.insert(mac, ip);
                }
            }
        }
    });
}

/// Segment a NIC attaches to: its declared segment, or the built-in NAT
/// Pick the daemon's service address. A dedicated machine gateway owns the
/// traditional first host address, so DHCP/DNS/SMB move to the first otherwise
/// unused host. The DHCP allocator excludes this address automatically.
fn segment_service_ip(
    lab: &Lab,
    segment: &str,
    subnet: Ipv4Net,
    gateway_ip: Ipv4Addr,
) -> Result<Ipv4Addr> {
    let delegated = machine_nics(lab)
        .flat_map(|(_, nics)| nics)
        .any(|nic| nic.gateway && nic_segment_name(nic) == segment);
    if !delegated {
        return Ok(gateway_ip);
    }
    let used: HashSet<Ipv4Addr> = machine_nics(lab)
        .flat_map(|(_, nics)| nics)
        .filter(|nic| nic_segment_name(nic) == segment)
        .filter_map(|nic| nic.ip)
        .collect();
    subnet
        .hosts()
        .find(|ip| *ip != gateway_ip && !used.contains(ip))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "segment \"{segment}\" has no free address for built-in DHCP/DNS services"
            )
        })
}

/// All machine NICs in the lab, `(name, nics)` per machine — VMs and
/// containers attach to segments identically, so network assembly treats
/// them uniformly.
fn machine_nics(lab: &Lab) -> impl Iterator<Item = (&str, &[crate::config::model::Nic])> {
    lab.machines().map(|m| (m.name(), m.nics()))
}

/// Segment a NIC attaches to: its declared segment, or the built-in NAT
/// segment for `nat = true`.
pub fn nic_segment_name(nic: &crate::config::model::Nic) -> &str {
    if nic.nat {
        NAT_SEGMENT
    } else {
        nic.segment.as_deref().expect("validated nic")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_lab_source;
    use std::path::Path;

    fn lab(src: &str) -> Lab {
        load_lab_source(src, "<t>", Path::new("/tmp")).unwrap().lab
    }

    #[test]
    fn declared_and_auto_subnets() {
        let l = lab(r#"import <vmlab.wcl>
lab "l" {
  segment "corp" { subnet = "10.50.0.0/24" }
  segment "dmz" { }
  vm "a" { template = "x86_64/t" nic { nat = true } }
}"#);
        let net = LabNetwork::build(&l).unwrap();
        assert_eq!(net.segments["corp"].subnet.to_string(), "10.50.0.0/24");
        assert_eq!(net.segments["corp"].gateway_ip.to_string(), "10.50.0.1");
        assert_eq!(net.segments["corp"].service_ip.to_string(), "10.50.0.1");
        // dmz auto-allocated from the pool.
        let dmz = net.segments["dmz"].subnet;
        assert!(dmz.to_string().starts_with("10.213."));
        // Built-in NAT segment exists and got its own /24.
        let nat = &net.segments[NAT_SEGMENT];
        assert!(nat.nat);
        assert_ne!(nat.subnet, dmz);
    }

    #[test]
    fn delegated_gateway_moves_daemon_services_off_router_address() {
        let l = lab(r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.50.0.0/24" }
  vm "router" {
    template = "x86_64/t"
    nic { segment = "lan" ip = "10.50.0.1" gateway = true }
  }
  container "reserved" { image = "alpine" nic { segment = "lan" ip = "10.50.0.2" } }
}"#);
        let net = LabNetwork::build(&l).unwrap();
        let lan = &net.segments["lan"];
        assert_eq!(lan.gateway_ip.to_string(), "10.50.0.1");
        assert_eq!(lan.service_ip.to_string(), "10.50.0.3");
    }

    #[test]
    fn declared_subnet_inside_pool_not_reallocated() {
        let l = lab(r#"import <vmlab.wcl>
lab "l" {
  segment "a" { subnet = "10.213.0.0/24" }
  segment "b" { }
  vm "x" { template = "x86_64/t" nic { segment = "a" } }
}"#);
        let net = LabNetwork::build(&l).unwrap();
        assert_eq!(net.segments["a"].subnet.to_string(), "10.213.0.0/24");
        assert_ne!(net.segments["b"].subnet.to_string(), "10.213.0.0/24");
    }

    /// `routes_to` on one side connects the pair both ways (§9.6), and a
    /// script can connect and disconnect a pair — never a global segment.
    #[tokio::test]
    async fn routes_to_connects_the_pair_and_scripts_toggle_it() {
        let l = lab(r#"import <vmlab.wcl>
lab "l" {
  segment "a" { subnet = "10.60.0.0/24" routes_to = ["b"] }
  segment "b" { subnet = "10.60.1.0/24" }
  segment "c" { subnet = "10.60.2.0/24" }
  segment "g" { subnet = "10.60.9.0/24" global = true }
  vm "x" { template = "x86_64/t" nic { segment = "a" } }
}"#);
        let mut net = LabNetwork::build(&l).unwrap();
        net.wire_gateways(
            &l,
            &HashMap::new(),
            &crate::config::host::HostConfig::default(),
        );

        assert!(net.router.connected("b", "a"));
        assert!(!net.router.connected("a", "c"));
        let a: Ipv4Net = "10.60.0.0/24".parse().unwrap();
        let b: Ipv4Net = "10.60.1.0/24".parse().unwrap();
        assert_eq!(
            net.router.dhcp_routes("a"),
            vec![(b, "10.60.0.1".parse().unwrap())]
        );
        assert_eq!(
            net.router.dhcp_routes("b"),
            vec![(a, "10.60.1.1".parse().unwrap())]
        );

        net.set_route("c", "b", true).unwrap();
        assert!(net.router.connected("b", "c"));
        net.set_route("b", "c", false).unwrap();
        assert!(!net.router.connected("c", "b"));

        let e = net.set_route("a", "g", true).unwrap_err();
        assert!(e.contains("\"g\" is global"), "{e}");
        let e = net.set_route("a", "nope", true).unwrap_err();
        assert!(e.contains("\"nope\""), "{e}");
    }

    #[test]
    fn reserved_nat_name_conflict() {
        let l = lab(r#"import <vmlab.wcl>
lab "l" {
  segment "nat" { }
  vm "a" { template = "x86_64/t" nic { nat = true } }
}"#);
        assert!(LabNetwork::build(&l).is_err());
    }
}
