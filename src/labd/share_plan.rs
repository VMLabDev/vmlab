//! The **share plan**: which of a lab's shared folders ride virtiofs, which
//! fall back to the bundled SMB server, which segments need a gateway rule to
//! reach it, and which host port it takes (PRD §7.5, §18).
//!
//! All of that used to be decided inside the routine that started smbd, so the
//! rules could only be exercised by bringing a lab up against a real host —
//! one with a virtiofsd, or deliberately without one. The invariant that
//! *a share is served by exactly one transport* was stated as a comment, in a
//! different module from the code that depended on it. Here it is
//! [`SharePlan::placements`], and a test asserts it.
//!
//! Nothing here does I/O. The two facts that are genuinely host state —
//! whether a virtiofsd exists, and whether a localhost port is free — arrive
//! as a bool and as a [`PortProbe`].

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use crate::config::model::{Lab, Share, ShareTransport};
use crate::labd::container::resolve_volume_hosts;
use crate::labd::network::nic_segment_name;
use crate::labd::plan::Skip;

/// The localhost port range the bundled `smbd` walks. Another lab's smbd — or
/// an orphan from an unclean daemon death — may hold the earlier ones.
pub const SMB_PORT_BASE: u16 = 14450;
pub const SMB_PORT_TRIES: u16 = 10;

/// Whether a localhost port can be taken. The gateway DNAT hides the number
/// from guests, so any free one will do — but "free" is host state, and the
/// plan does not go looking for it itself.
pub trait PortProbe {
    fn is_free(&self, port: u16) -> bool;
}

/// The real probe: bind it and see.
pub struct BindProbe;

impl PortProbe for BindProbe {
    fn is_free(&self, port: u16) -> bool {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
    }
}

/// How a share reaches its guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A vhost-user-fs device, served by a per-share virtiofsd at machine
    /// start. No network involved.
    Virtiofs,
    /// The lab's bundled `smbd`, reached at the segment gateway.
    Smb,
}

/// One share riding virtiofs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtiofsShare {
    pub machine: String,
    /// Index into the machine's declared `share {}` / `volume {}` blocks.
    pub index: usize,
    pub name: String,
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
}

/// What `smbd` exports for one machine, and the gateway it reaches them at.
#[derive(Debug, Clone)]
pub struct SmbExport {
    pub machine: String,
    pub gateway: Ipv4Addr,
    pub shares: Vec<Share>,
}

/// The bundled SMB server, present only when something actually needs it.
#[derive(Debug, Clone)]
pub struct SmbPlan {
    /// The free localhost ports to try, in range order, first choice first.
    /// Never empty — a range with nothing free is a [`SharePlanError`].
    ///
    /// More than one, because a port free when the plan was computed can be
    /// taken by the time `smbd` binds it, and because `smbd` can fail to come
    /// up for reasons that have nothing to do with the port. The executor
    /// works down the list.
    pub host_ports: Vec<u16>,
    pub exports: Vec<SmbExport>,
    /// Segments needing `gateway:445 → 127.0.0.1:<the port smbd took>`, so a
    /// guest mounting `\\<gateway>\<share>` reaches the local smbd through NAT.
    pub gateway_segments: Vec<String>,
    /// Containers whose volumes mount over CIFS, with the gateway their
    /// cinit mounts from.
    pub volume_gateways: Vec<(String, Ipv4Addr)>,
}

/// Every shared folder in a lab, placed on a transport.
#[derive(Debug, Clone)]
pub struct SharePlan {
    pub virtiofs: Vec<VirtiofsShare>,
    pub smb: Option<SmbPlan>,
    pub skipped: Vec<Skip>,
}

/// One share and the transport carrying it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub machine: String,
    pub share: String,
    pub transport: Transport,
}

impl SharePlan {
    /// Every share the plan places, and what carries it — the plan's central
    /// invariant made inspectable: exactly one entry per declared share.
    pub fn placements(&self) -> Vec<Placement> {
        let mut out: Vec<Placement> = self
            .virtiofs
            .iter()
            .map(|v| Placement {
                machine: v.machine.clone(),
                share: v.name.clone(),
                transport: Transport::Virtiofs,
            })
            .chain(self.smb.iter().flat_map(|s| {
                s.exports.iter().flat_map(|e| {
                    e.shares.iter().map(|sh| Placement {
                        machine: e.machine.clone(),
                        share: sh.name.clone(),
                        transport: Transport::Smb,
                    })
                })
            }))
            .collect();
        out.sort_by(|a, b| (&a.machine, &a.share).cmp(&(&b.machine, &b.share)));
        out
    }
}

/// Why a lab's shares cannot be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharePlanError {
    /// Two shares would answer to one name. One `smbd` serves the whole lab,
    /// and one virtio-fs tag namespace serves one machine, so a repeat is a
    /// share the author will never reach.
    Collision {
        name: String,
        claimants: Vec<String>,
    },
    /// Every port in the range is taken.
    NoFreePort { base: u16, tries: u16 },
}

impl std::fmt::Display for SharePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SharePlanError::Collision { name, claimants } => write!(
                f,
                "two shares are both named \"{name}\" ({}) — a share name must be unique, \
                 rename one",
                claimants.join(" and ")
            ),
            SharePlanError::NoFreePort { base, tries } => write!(
                f,
                "no free localhost port for the share server in {base}..{} — another lab's \
                 smbd (or an orphan from an unclean daemon exit) is holding them",
                base + tries
            ),
        }
    }
}

impl std::error::Error for SharePlanError {}

/// What the plan needs to know that is not in the lab file.
pub struct ShareInputs<'a> {
    pub lab: &'a Lab,
    /// The lab root, for resolving relative share host paths.
    pub root: &'a Path,
    /// `$HOME`, for resolving `~`-prefixed ones. `None` = leave them alone.
    pub home: Option<&'a Path>,
    /// Does this host have a virtiofsd at all?
    pub host_virtiofsd: bool,
    /// Per VM: does its resolved profile say the guest mounts virtiofs
    /// natively? A VM absent from the map is assumed not to.
    pub guest_virtiofs: &'a BTreeMap<String, bool>,
    /// Each segment's service address — where a guest reaches the lab's
    /// gateway services. A segment absent from the map has no gateway.
    pub gateways: &'a BTreeMap<String, Ipv4Addr>,
}

/// The half-built plan, so the two machine kinds can hand their SMB exports
/// to one routine instead of writing the same "no nic → skip, no gateway →
/// skip, export, remember the segment" block out twice.
#[derive(Default)]
struct Building {
    virtiofs: Vec<VirtiofsShare>,
    exports: Vec<SmbExport>,
    gateway_segments: Vec<String>,
    volume_gateways: Vec<(String, Ipv4Addr)>,
    skipped: Vec<Skip>,
}

impl Building {
    /// Export `shares` for `machine` at its first NIC's segment gateway, or
    /// record why they cannot be served.
    ///
    /// `what` names the machine the way a skip should read it; `nic_why` is
    /// what to say when it has no NIC, which differs by kind because a VM's
    /// share and a container's volume are declared differently.
    fn export(
        &mut self,
        inputs: &ShareInputs,
        machine: &str,
        nics: &[crate::config::model::Nic],
        shares: Vec<Share>,
        what: String,
        nic_why: &str,
    ) -> Option<Ipv4Addr> {
        // Validated (§5.1): a machine with a non-virtiofs share has a NIC. A
        // segment with no gateway cannot carry the DNAT, so say so rather
        // than exporting shares nothing can reach.
        let Some(segment) = nics.first().map(nic_segment_name) else {
            self.skipped.push(Skip {
                what,
                why: nic_why.to_string(),
            });
            return None;
        };
        let Some(gateway) = inputs.gateways.get(segment).copied() else {
            self.skipped.push(Skip {
                what,
                why: format!("segment \"{segment}\" has no gateway to serve them at"),
            });
            return None;
        };
        self.exports.push(SmbExport {
            machine: machine.to_string(),
            gateway,
            shares,
        });
        if !self.gateway_segments.iter().any(|s| s == segment) {
            self.gateway_segments.push(segment.to_string());
        }
        Some(gateway)
    }
}

/// Work out where every share in the lab goes.
pub fn plan(inputs: &ShareInputs, ports: &dyn PortProbe) -> Result<SharePlan, SharePlanError> {
    let mut b = Building::default();

    // -- VM `share {}` blocks --------------------------------------------
    for vm in &inputs.lab.vms {
        if vm.shares.is_empty() {
            continue;
        }
        let guest_ok = inputs
            .guest_virtiofs
            .get(&vm.name)
            .copied()
            .unwrap_or(false);
        let mut smb_shares: Vec<Share> = Vec::new();
        for (i, share) in vm.shares.iter().enumerate() {
            let host = resolve_share_host(inputs.root, inputs.home, &share.host);
            match transport_of(share.transport, inputs.host_virtiofsd, guest_ok) {
                Transport::Virtiofs => b.virtiofs.push(VirtiofsShare {
                    machine: vm.name.clone(),
                    index: i,
                    name: share.name.clone(),
                    host,
                    guest: share.guest.clone(),
                    readonly: share.readonly,
                }),
                Transport::Smb => {
                    let mut share = share.clone();
                    share.host = host;
                    smb_shares.push(share);
                }
            }
        }
        if smb_shares.is_empty() {
            continue;
        }
        b.export(
            inputs,
            &vm.name,
            &vm.nics,
            smb_shares,
            format!("vm \"{}\": smb shares", vm.name),
            "no nic — an smb share is reachable only over a segment",
        );
    }

    // -- container `volume {}` blocks ------------------------------------
    // A container's volumes all ride the same transport: cinit mounts CIFS
    // when the host has no virtiofsd, and vhost-user-fs devices otherwise
    // (PRD §18). There is no per-guest capability question — the micro-VM's
    // init supports both.
    for c in &inputs.lab.containers {
        if c.volumes.is_empty() {
            continue;
        }
        let hosts = resolve_volume_hosts(c, inputs.root);
        if container_volume_transport(inputs.host_virtiofsd) == Transport::Virtiofs {
            for (i, ((name, host, readonly), vol)) in hosts.iter().zip(&c.volumes).enumerate() {
                b.virtiofs.push(VirtiofsShare {
                    machine: c.name.clone(),
                    index: i,
                    name: name.clone(),
                    host: host.clone(),
                    guest: vol.target.clone(),
                    readonly: *readonly,
                });
            }
            continue;
        }
        // The guest target rides along for smb.conf comments only; read it
        // from the config (1:1 with the resolved exports) so the plan does
        // not depend on the image being pulled yet.
        let shares = hosts
            .iter()
            .zip(&c.volumes)
            .map(|((name, host, readonly), vol)| Share {
                span: (0, 0),
                host: host.clone(),
                guest: vol.target.clone(),
                readonly: *readonly,
                smb1: false,
                name: name.clone(),
                transport: ShareTransport::Smb,
            })
            .collect();
        if let Some(gateway) = b.export(
            inputs,
            &c.name,
            &c.nics,
            shares,
            format!("container \"{}\": volumes", c.name),
            "no nic — a cifs volume is reachable only over a segment",
        ) {
            b.volume_gateways.push((c.name.clone(), gateway));
        }
    }

    check_collisions(&b.virtiofs, &b.exports)?;

    let smb = if b.exports.is_empty() {
        None
    } else {
        Some(SmbPlan {
            host_ports: free_ports(ports)?,
            exports: b.exports,
            gateway_segments: b.gateway_segments,
            volume_gateways: b.volume_gateways,
        })
    };
    Ok(SharePlan {
        virtiofs: b.virtiofs,
        smb,
        skipped: b.skipped,
    })
}

/// Which transport a container's volumes ride, all together.
///
/// Containers declare no per-volume transport and the micro-VM's init mounts
/// either, so there is no per-guest capability question: the whole set
/// follows the host (PRD §18).
///
/// A vhost-user-fs device cannot hotplug, so this is decided again at machine
/// start rather than read off the plan. Both sites call *this*, which is what
/// keeps the plan's placement and what the machine actually attaches from
/// drifting apart — the same arrangement [`transport_of`] gives VM shares.
pub fn container_volume_transport(host_virtiofsd: bool) -> Transport {
    if host_virtiofsd {
        Transport::Virtiofs
    } else {
        Transport::Smb
    }
}

/// Which transport one declared VM share takes.
///
/// An explicit `transport = "virtiofs"` always rides virtiofs — a host with
/// no virtiofsd errors at machine start rather than silently degrading. `smb`
/// always rides smbd. `auto` takes virtiofs only when the host can serve it
/// *and* the guest can mount it, so SMB is the fallback and never the
/// preference.
pub fn transport_of(declared: ShareTransport, host_virtiofsd: bool, guest_ok: bool) -> Transport {
    match declared {
        ShareTransport::Smb => Transport::Smb,
        ShareTransport::Virtiofs => Transport::Virtiofs,
        ShareTransport::Auto if host_virtiofsd && guest_ok => Transport::Virtiofs,
        ShareTransport::Auto => Transport::Smb,
    }
}

/// A share's host path as the server sees it: `~` against `$HOME`, relative
/// paths against the lab root. smbd's cwd is not the lab's, so a literal
/// `./shared` would canonicalize to `/shared` and fail every tree connect.
pub fn resolve_share_host(root: &Path, home: Option<&Path>, host: &Path) -> PathBuf {
    if let Ok(rest) = host.strip_prefix("~")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    if host.is_relative() {
        return root.join(host);
    }
    host.to_path_buf()
}

/// One `smbd` serves the whole lab, so an export name repeated across VMs is
/// a share the author will never reach. Virtio-fs tags are per machine, so
/// those only collide within one.
fn check_collisions(
    virtiofs: &[VirtiofsShare],
    exports: &[SmbExport],
) -> Result<(), SharePlanError> {
    let mut claims: BTreeMap<(Option<&str>, &str), Vec<String>> = BTreeMap::new();
    for v in virtiofs {
        claims
            .entry((Some(v.machine.as_str()), v.name.as_str()))
            .or_default()
            .push(format!("\"{}\" virtiofs", v.machine));
    }
    for e in exports {
        for s in &e.shares {
            claims
                .entry((None, s.name.as_str()))
                .or_default()
                .push(format!("\"{}\" smb", e.machine));
        }
    }
    for ((_, name), claimants) in claims {
        if claimants.len() > 1 {
            return Err(SharePlanError::Collision {
                name: name.to_string(),
                claimants,
            });
        }
    }
    Ok(())
}

/// Every free port in the range, in order, or a diagnosable failure. Walking
/// upward rather than taking an ephemeral port keeps the number predictable
/// for anyone reading the smb.conf.
fn free_ports(ports: &dyn PortProbe) -> Result<Vec<u16>, SharePlanError> {
    let free: Vec<u16> = (SMB_PORT_BASE..SMB_PORT_BASE + SMB_PORT_TRIES)
        .filter(|p| ports.is_free(*p))
        .collect();
    if free.is_empty() {
        return Err(SharePlanError::NoFreePort {
            base: SMB_PORT_BASE,
            tries: SMB_PORT_TRIES,
        });
    }
    Ok(free)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::container::volume_share_name;

    /// Every port free — the ordinary host.
    struct AllFree;
    impl PortProbe for AllFree {
        fn is_free(&self, _: u16) -> bool {
            true
        }
    }

    /// Every port taken — a host already running labs.
    struct NoneFree;
    impl PortProbe for NoneFree {
        fn is_free(&self, _: u16) -> bool {
            false
        }
    }

    /// The first `n` ports of the range are held by someone else.
    struct Held(u16);
    impl PortProbe for Held {
        fn is_free(&self, port: u16) -> bool {
            port >= SMB_PORT_BASE + self.0
        }
    }

    fn lab_of(src: &str) -> Lab {
        crate::config::load_lab_source(src, "<test>", std::path::Path::new("/lab"))
            .expect("parse")
            .lab
    }

    struct Host {
        virtiofsd: bool,
        guest_virtiofs: BTreeMap<String, bool>,
        gateways: BTreeMap<String, Ipv4Addr>,
    }

    impl Host {
        /// A host with a virtiofsd, one segment "lan", and every named guest
        /// able to mount virtiofs.
        fn with_virtiofsd(guests: &[&str]) -> Self {
            Host {
                virtiofsd: true,
                guest_virtiofs: guests.iter().map(|g| (g.to_string(), true)).collect(),
                gateways: BTreeMap::from([("lan".to_string(), Ipv4Addr::new(10, 0, 0, 1))]),
            }
        }

        /// The same host with no virtiofsd on it.
        fn bare() -> Self {
            Host {
                virtiofsd: false,
                guest_virtiofs: BTreeMap::new(),
                gateways: BTreeMap::from([("lan".to_string(), Ipv4Addr::new(10, 0, 0, 1))]),
            }
        }
    }

    fn plan_on(lab: &Lab, host: &Host, ports: &dyn PortProbe) -> Result<SharePlan, SharePlanError> {
        plan(
            &ShareInputs {
                lab,
                root: Path::new("/lab"),
                home: Some(Path::new("/home/dev")),
                host_virtiofsd: host.virtiofsd,
                guest_virtiofs: &host.guest_virtiofs,
                gateways: &host.gateways,
            },
            ports,
        )
    }

    /// One VM, three shares, one of each transport setting.
    fn mixed() -> Lab {
        lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "web" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./auto"  guest = "/mnt/auto"  name = "auto" }
    share { host = "./vfs"   guest = "/mnt/vfs"   name = "vfs"  transport = "virtiofs" }
    share { host = "./cifs"  guest = "/mnt/cifs"  name = "cifs" transport = "smb" }
  }
}"#,
        )
    }

    /// The invariant the old comment asserted and nothing checked: each
    /// declared share is placed exactly once, on exactly one transport.
    #[test]
    fn every_share_is_served_by_exactly_one_transport() {
        for host in [Host::with_virtiofsd(&["web"]), Host::bare()] {
            let p = plan_on(&mixed(), &host, &AllFree).unwrap();
            let placed = p.placements();
            let names: Vec<&str> = placed.iter().map(|p| p.share.as_str()).collect();
            assert_eq!(
                names,
                ["auto", "cifs", "vfs"],
                "every share placed once, virtiofsd={}",
                host.virtiofsd
            );
        }
    }

    /// `auto` prefers virtiofs, `smb` never takes it, `virtiofs` always does.
    #[test]
    fn transport_selection_follows_the_declaration_then_the_host() {
        let p = plan_on(&mixed(), &Host::with_virtiofsd(&["web"]), &AllFree).unwrap();
        let by_name: BTreeMap<String, Transport> = p
            .placements()
            .into_iter()
            .map(|pl| (pl.share, pl.transport))
            .collect();
        assert_eq!(by_name["auto"], Transport::Virtiofs);
        assert_eq!(by_name["vfs"], Transport::Virtiofs);
        assert_eq!(by_name["cifs"], Transport::Smb, "an explicit smb stays smb");
    }

    /// SMB is a fallback, never a preference: `auto` lands on it only when
    /// virtiofs is unavailable — and each half of "available" is enough on
    /// its own to force it.
    #[test]
    fn auto_falls_back_to_smb_only_when_virtiofs_is_unavailable() {
        let lab = mixed();
        let cases = [
            (true, true, Transport::Virtiofs),
            (false, true, Transport::Smb), // no virtiofsd on the host
            (true, false, Transport::Smb), // the guest cannot mount it
            (false, false, Transport::Smb),
        ];
        for (host_has, guest_ok, want) in cases {
            let host = Host {
                virtiofsd: host_has,
                guest_virtiofs: BTreeMap::from([("web".to_string(), guest_ok)]),
                gateways: BTreeMap::from([("lan".to_string(), Ipv4Addr::new(10, 0, 0, 1))]),
            };
            let p = plan_on(&lab, &host, &AllFree).unwrap();
            let auto = p
                .placements()
                .into_iter()
                .find(|p| p.share == "auto")
                .unwrap();
            assert_eq!(auto.transport, want, "host={host_has} guest={guest_ok}");
        }
    }

    /// An explicit `transport = "virtiofs"` rides virtiofs even with no
    /// virtiofsd on the host: the machine start errors, rather than silently
    /// degrading to a transport the author declined.
    #[test]
    fn an_explicit_virtiofs_share_never_falls_back() {
        let p = plan_on(&mixed(), &Host::bare(), &AllFree).unwrap();
        let vfs = p
            .placements()
            .into_iter()
            .find(|p| p.share == "vfs")
            .unwrap();
        assert_eq!(vfs.transport, Transport::Virtiofs);
    }

    /// VM shares and container volumes are both accounted for, so neither
    /// kind can be silently dropped.
    #[test]
    fn vm_shares_and_container_volumes_are_both_placed() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "web" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./src" guest = "/mnt/src" name = "src" transport = "smb" }
  }
  container "cache" { image = "redis" nic { segment = "lan" }
    volume { host = "./data" target = "/data" }
  }
}"#,
        );
        // No virtiofsd: both kinds land on the one smbd.
        let p = plan_on(&lab, &Host::bare(), &AllFree).unwrap();
        let placed = p.placements();
        let machines: Vec<&str> = placed.iter().map(|pl| pl.machine.as_str()).collect();
        assert_eq!(machines, ["cache", "web"]);
        let smb = p.smb.as_ref().unwrap();
        assert_eq!(
            smb.volume_gateways,
            [("cache".to_string(), Ipv4Addr::new(10, 0, 0, 1))]
        );

        // With a virtiofsd the volume rides it and only the VM's explicit
        // smb share needs a server.
        let p = plan_on(&lab, &Host::with_virtiofsd(&["web"]), &AllFree).unwrap();
        assert_eq!(p.virtiofs.len(), 1);
        assert_eq!(p.virtiofs[0].machine, "cache");
        assert_eq!(p.virtiofs[0].guest, "/data");
        let smb = p.smb.as_ref().unwrap();
        assert_eq!(smb.exports.len(), 1);
        assert_eq!(smb.exports[0].machine, "web");
        assert!(smb.volume_gateways.is_empty());
    }

    /// A lab whose every share rides virtiofs needs no server at all.
    #[test]
    fn a_lab_with_nothing_on_smb_starts_no_server() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "web" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./src" guest = "/mnt/src" name = "src" transport = "virtiofs" }
  }
}"#,
        );
        let p = plan_on(&lab, &Host::with_virtiofsd(&["web"]), &NoneFree).unwrap();
        assert!(p.smb.is_none(), "no exports, so no port needed either");
        assert_eq!(p.virtiofs.len(), 1);
    }

    /// Only the segments that actually carry an export get the 445 rule.
    #[test]
    fn only_sharing_segments_get_a_gateway_rule() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan"  { subnet = "10.0.0.0/24" }
  segment "dmz"  { subnet = "10.0.1.0/24" }
  segment "idle" { subnet = "10.0.2.0/24" }
  vm "web" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./a" guest = "/mnt/a" name = "a" transport = "smb" } }
  vm "app" { template = "x86_64/t" nic { segment = "dmz" }
    share { host = "./b" guest = "/mnt/b" name = "b" transport = "smb" } }
  vm "bare" { template = "x86_64/t" nic { segment = "idle" } }
}"#,
        );
        let host = Host {
            virtiofsd: false,
            guest_virtiofs: BTreeMap::new(),
            gateways: BTreeMap::from([
                ("lan".to_string(), Ipv4Addr::new(10, 0, 0, 1)),
                ("dmz".to_string(), Ipv4Addr::new(10, 0, 1, 1)),
                ("idle".to_string(), Ipv4Addr::new(10, 0, 2, 1)),
            ]),
        };
        let p = plan_on(&lab, &host, &AllFree).unwrap();
        assert_eq!(p.smb.unwrap().gateway_segments, ["lan", "dmz"]);
    }

    /// A machine sharing over a segment with no gateway is named, not
    /// silently left without its shares.
    #[test]
    fn a_segment_without_a_gateway_is_reported() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "web" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./a" guest = "/mnt/a" name = "a" transport = "smb" } }
}"#,
        );
        let host = Host {
            virtiofsd: false,
            guest_virtiofs: BTreeMap::new(),
            gateways: BTreeMap::new(), // the lab is not up
        };
        let p = plan_on(&lab, &host, &AllFree).unwrap();
        assert!(p.smb.is_none());
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].what.contains("web"), "{:#?}", p.skipped);
        assert!(p.skipped[0].why.contains("no gateway"), "{:#?}", p.skipped);
    }

    /// Port selection is a decision over data, not a walk over live sockets.
    #[test]
    fn the_server_takes_the_first_free_port_in_the_range() {
        let lab = mixed();
        let host = Host::bare();
        let all = plan_on(&lab, &host, &AllFree).unwrap().smb.unwrap();
        assert_eq!(all.host_ports[0], SMB_PORT_BASE);
        assert_eq!(all.host_ports.len(), SMB_PORT_TRIES as usize);

        let held = plan_on(&lab, &host, &Held(3)).unwrap().smb.unwrap();
        assert_eq!(
            held.host_ports[0],
            SMB_PORT_BASE + 3,
            "three earlier ports held by another lab"
        );
    }

    /// A port free at plan time can be taken by the time smbd binds it, and
    /// smbd can fail for reasons that are nothing to do with the port — so
    /// the plan offers the executor every free port, not just the best one.
    #[test]
    fn the_plan_carries_a_fallback_for_every_free_port() {
        let plan = plan_on(&mixed(), &Host::bare(), &Held(8)).unwrap();
        assert_eq!(
            plan.smb.unwrap().host_ports,
            [SMB_PORT_BASE + 8, SMB_PORT_BASE + 9]
        );
    }

    /// And a full range fails with something an operator can act on, rather
    /// than leaving the shares quietly unmounted.
    #[test]
    fn a_full_port_range_fails_diagnosably() {
        let err = plan_on(&mixed(), &Host::bare(), &NoneFree).unwrap_err();
        assert_eq!(
            err,
            SharePlanError::NoFreePort {
                base: SMB_PORT_BASE,
                tries: SMB_PORT_TRIES,
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("14450..14460"), "{msg}");
        assert!(msg.contains("smbd"), "{msg}");
    }

    /// Two VMs both exporting a share called "data" would produce one
    /// smb.conf with two `[data]` sections: one of the authors never reaches
    /// their folder. Caught at plan time, naming both.
    #[test]
    fn two_shares_with_one_name_collide() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./x" guest = "/mnt/x" name = "data" transport = "smb" } }
  vm "b" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./y" guest = "/mnt/y" name = "data" transport = "smb" } }
}"#,
        );
        let err = plan_on(&lab, &Host::bare(), &AllFree).unwrap_err();
        assert_eq!(
            err,
            SharePlanError::Collision {
                name: "data".into(),
                claimants: vec!["\"a\" smb".into(), "\"b\" smb".into()],
            }
        );
        assert!(err.to_string().contains("rename one"), "{err}");
    }

    /// Within one machine two virtiofs shares would share a mount tag, which
    /// is the same defect one layer down.
    #[test]
    fn two_virtiofs_shares_with_one_name_collide() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./x" guest = "/mnt/x" name = "data" transport = "virtiofs" }
    share { host = "./y" guest = "/mnt/y" name = "data" transport = "virtiofs" } }
}"#,
        );
        let err = plan_on(&lab, &Host::with_virtiofsd(&["a"]), &AllFree).unwrap_err();
        assert!(matches!(err, SharePlanError::Collision { .. }), "{err:?}");
    }

    /// The same name on two different machines' virtiofs devices is fine —
    /// tags are per machine.
    #[test]
    fn one_virtiofs_name_per_machine_does_not_collide() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./x" guest = "/mnt/x" name = "data" transport = "virtiofs" } }
  vm "b" { template = "x86_64/t" nic { segment = "lan" }
    share { host = "./y" guest = "/mnt/y" name = "data" transport = "virtiofs" } }
}"#,
        );
        let p = plan_on(&lab, &Host::with_virtiofsd(&["a", "b"]), &AllFree).unwrap();
        assert_eq!(p.virtiofs.len(), 2);
    }

    /// smbd's cwd is not the lab's, so exported paths are absolute by the
    /// time the plan hands them over.
    #[test]
    fn host_paths_are_resolved_for_the_server() {
        let root = Path::new("/lab");
        let home = Some(Path::new("/home/dev"));
        assert_eq!(
            resolve_share_host(root, home, Path::new("./shared")),
            PathBuf::from("/lab/./shared")
        );
        assert_eq!(
            resolve_share_host(root, home, Path::new("~/docs")),
            PathBuf::from("/home/dev/docs")
        );
        assert_eq!(
            resolve_share_host(root, home, Path::new("/srv/data")),
            PathBuf::from("/srv/data")
        );
        assert_eq!(
            resolve_share_host(root, None, Path::new("~/docs")),
            PathBuf::from("/lab/~/docs"),
            "no $HOME to resolve against leaves it relative to the lab"
        );
    }

    /// A container volume's export name is machine-scoped, so two containers
    /// mounting `/data` never collide.
    #[test]
    fn container_volume_names_are_machine_scoped() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" }
  container "a" { image = "redis" nic { segment = "lan" }
    volume { host = "./d" target = "/data" } }
  container "b" { image = "redis" nic { segment = "lan" }
    volume { host = "./d" target = "/data" } }
}"#,
        );
        let p = plan_on(&lab, &Host::bare(), &AllFree).unwrap();
        let placed = p.placements();
        let names: Vec<&str> = placed.iter().map(|p| p.share.as_str()).collect();
        assert_eq!(
            names,
            [
                volume_share_name("a", 0).as_str(),
                volume_share_name("b", 0).as_str()
            ]
        );
    }
}
