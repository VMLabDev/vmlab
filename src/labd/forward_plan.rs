//! The **forward plan**: every host→guest port forward a lab's machines
//! require, worked out as one value.
//!
//! Two near-identical routines used to install these — one for segment
//! `forward {}` blocks, one for container `port {}` blocks — each resolving
//! a machine its own way, each taking its lease address, each priming a
//! hardware address and installing a rule. Their differences were
//! incidental: what a forward *is* does not depend on which block declared
//! it.
//!
//! Here they are one plan. Lease resolution is the only genuinely runtime
//! input and it arrives as data, so the plan is computable with no network:
//! tests build a lab and a lease table and assert on the rules.
//!
//! Two things the old routines did silently, this says out loud: a forward
//! whose machine has no lease is [`Skip`]ped with a reason rather than
//! dropped, and two forwards claiming one host port are settled here as a
//! [`HostPortConflict`] — the first claimant keeps the port, the rest are
//! dropped naming the winner — rather than all being installed and the
//! losers failing at bind time with nothing to say why.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use crate::config::model::{Lab, MacAddr, Proto};
use crate::labd::network::nic_segment_name;
use crate::labd::plan::Skip;

/// Which declaration a forward came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardSource {
    /// A segment `forward {}` block (PRD §9.8).
    Declared,
    /// A container `port {}` block — sugar for the same machinery (PRD §18).
    ContainerPort,
}

impl ForwardSource {
    /// How the source names itself in a skip reason or a conflict.
    pub fn describe(&self) -> String {
        match self {
            ForwardSource::Declared => "forward".to_string(),
            ForwardSource::ContainerPort => "container port".to_string(),
        }
    }
}

/// Where a forward listens on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBinding {
    /// A declared port on every interface.
    Port(u16),
    /// No port declared: vmlab binds a free one on every interface when it
    /// installs the forward, preferring the one it held last time
    /// ([`sticky_bind`]).
    Auto,
}

impl HostBinding {
    fn from_declared(port: Option<u16>) -> Self {
        port.map_or(HostBinding::Auto, HostBinding::Port)
    }

    /// Whether vmlab picks the port.
    pub fn is_auto(&self) -> bool {
        matches!(self, HostBinding::Auto)
    }
}

/// One forward to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRule {
    pub machine: String,
    /// The segment whose NAT engine carries the forward.
    pub segment: String,
    pub host: HostBinding,
    pub guest_ip: Ipv4Addr,
    pub guest_port: u16,
    pub proto: Proto,
    pub source: ForwardSource,
    /// Prime the NAT engine with this hardware address before installing.
    /// A machine that never originates egress (an idle nginx, say) is
    /// otherwise unreachable: the engine would broadcast the SYN and the
    /// guest's TCP stack drops broadcast-framed segments.
    pub prime_mac: Option<MacAddr>,
}

/// Two or more forwards claiming one host port, in plan order. Config
/// validation already rejects this within a lab file; the plan catches
/// whatever reaches it anyway — a partial `up`, or a rule composed from more
/// than one source. The first claimant keeps the port; every other is dropped
/// from [`ForwardPlan::rules`] and appears in `skipped` naming the winner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPortConflict {
    pub host_port: u16,
    /// Every claimant, in plan order: `"<machine>: <source>"`.
    pub claimants: Vec<String>,
}

/// Every forward a lab requires, and everything left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForwardPlan {
    pub rules: Vec<ForwardRule>,
    pub skipped: Vec<Skip>,
    pub conflicts: Vec<HostPortConflict>,
}

/// What the running network knows about the lab's machines: which address
/// each one's lease holds, and the hardware address that lease sits behind.
/// Gathering it is the caller's job — the plan itself touches no network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Absent = no lease yet.
    pub leases: HashMap<String, Ipv4Addr>,
    /// The hardware address of the NIC holding the lease, for NAT priming.
    pub macs: HashMap<String, MacAddr>,
}

/// What the plan needs to know that is not in the lab file.
pub struct ForwardInputs<'a> {
    pub lab: &'a Lab,
    pub observed: &'a Observed,
}

impl ForwardInputs<'_> {
    /// The segment a machine's forwards ride: its first NIC's. `None` when it
    /// has no NIC — validation requires one for ports, so this only fires on
    /// a lab that got past validation without one.
    fn first_segment(&self, machine: &str) -> Option<&str> {
        self.lab
            .machine(machine)?
            .nics()
            .first()
            .map(nic_segment_name)
    }
}

/// One forward before its runtime address is known — everything the
/// declaration itself decides.
struct Draft {
    machine: String,
    segment: String,
    host: HostBinding,
    guest_port: u16,
    proto: Proto,
    source: ForwardSource,
}

/// Work out every forward the lab requires. `scope` narrows it to those
/// machines; empty means the whole lab.
///
/// Order is declaration order: segment `forward {}` blocks first, then
/// container `port {}` blocks.
pub fn plan(inputs: &ForwardInputs, scope: &[String]) -> ForwardPlan {
    let mut plan = ForwardPlan::default();
    let in_scope = |machine: &str| scope.is_empty() || scope.iter().any(|m| m == machine);

    // Segment `forward {}` blocks name their own segment and their target.
    for seg in &inputs.lab.segments {
        for fwd in &seg.forwards {
            if !in_scope(&fwd.vm) {
                continue;
            }
            if inputs.lab.machine(&fwd.vm).is_none() {
                plan.skipped.push(Skip {
                    what: match fwd.host_port {
                        Some(p) => format!("forward {p} → \"{}\"", fwd.vm),
                        None => format!("forward (auto) → \"{}\"", fwd.vm),
                    },
                    why: "no such vm or container in the lab".into(),
                });
                continue;
            }
            push(
                &mut plan,
                inputs,
                Draft {
                    machine: fwd.vm.clone(),
                    segment: seg.name.clone(),
                    host: HostBinding::from_declared(fwd.host_port),
                    guest_port: fwd.guest_port,
                    proto: fwd.proto,
                    source: ForwardSource::Declared,
                },
            );
        }
    }

    // Container `port {}` blocks land on the container's own segment.
    for c in &inputs.lab.containers {
        if !in_scope(&c.name) {
            continue;
        }
        for port in &c.ports {
            let source = ForwardSource::ContainerPort;
            let Some(segment) = segment_or_skip(&mut plan, inputs, &c.name, &source) else {
                continue;
            };
            push(
                &mut plan,
                inputs,
                Draft {
                    machine: c.name.clone(),
                    segment,
                    host: HostBinding::from_declared(port.host_port),
                    guest_port: port.container_port,
                    proto: port.proto,
                    source,
                },
            );
        }
    }

    resolve_conflicts(&mut plan);
    plan
}

/// The machine's first NIC's segment, recording a skip when it has none.
fn segment_or_skip(
    plan: &mut ForwardPlan,
    inputs: &ForwardInputs,
    machine: &str,
    source: &ForwardSource,
) -> Option<String> {
    match inputs.first_segment(machine) {
        Some(s) => Some(s.to_string()),
        None => {
            plan.skipped.push(Skip {
                what: format!("\"{machine}\": {}", source.describe()),
                why: "needs a nic to reach it over".into(),
            });
            None
        }
    }
}

/// Add one rule, or record why it cannot be planned.
fn push(plan: &mut ForwardPlan, inputs: &ForwardInputs, draft: Draft) {
    let Some(guest_ip) = inputs.observed.leases.get(&draft.machine).copied() else {
        plan.skipped.push(Skip {
            what: format!("\"{}\": {}", draft.machine, draft.source.describe()),
            why: "no lease — is it running and ready?".into(),
        });
        return;
    };
    let prime_mac = inputs.observed.macs.get(&draft.machine).copied();
    plan.rules.push(ForwardRule {
        machine: draft.machine,
        segment: draft.segment,
        host: draft.host,
        guest_ip,
        guest_port: draft.guest_port,
        proto: draft.proto,
        source: draft.source,
        prime_mac,
    });
}

/// Settle every host port claimed more than once: the first claimant in plan
/// order keeps it, the rest are dropped with a reason naming the winner.
///
/// Dropping them is the point. Installing all of them and letting the losers
/// fail at bind time is what this used to do, and a bind failure names
/// neither the winner nor the fact that there was a contest.
fn resolve_conflicts(plan: &mut ForwardPlan) {
    let mut claims: std::collections::BTreeMap<u16, Vec<usize>> = Default::default();
    for (i, r) in plan.rules.iter().enumerate() {
        // An automatic port is picked free at install time; it claims
        // nothing a plan could contest.
        if let HostBinding::Port(p) = r.host {
            claims.entry(p).or_default().push(i);
        }
    }
    let mut dropped: Vec<usize> = Vec::new();
    for (host_port, claimants) in claims {
        if claimants.len() < 2 {
            continue;
        }
        let describe = |i: &usize| {
            let r = &plan.rules[*i];
            format!("{}: {}", r.machine, r.source.describe())
        };
        let winner = describe(&claimants[0]);
        for loser in &claimants[1..] {
            plan.skipped.push(Skip {
                what: describe(loser),
                why: format!("host port {host_port} is already claimed by {winner}"),
            });
            dropped.push(*loser);
        }
        plan.conflicts.push(HostPortConflict {
            host_port,
            claimants: claimants.iter().map(describe).collect(),
        });
    }
    let mut i = 0;
    plan.rules.retain(|_| {
        let keep = !dropped.contains(&i);
        i += 1;
        keep
    });
}

/// What a forward installed earlier is recognised by when its machine's
/// forwards are installed again (a restart takes a new lease, so the rule's
/// guest address is not part of it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardKey {
    pub machine: String,
    pub segment: String,
    pub source: ForwardSource,
    pub guest_port: u16,
    pub proto: Proto,
}

impl ForwardRule {
    pub fn key(&self) -> ForwardKey {
        ForwardKey {
            machine: self.machine.clone(),
            segment: self.segment.clone(),
            source: self.source.clone(),
            guest_port: self.guest_port,
            proto: self.proto,
        }
    }
}

/// The ports to try binding for one rule, in order. A declared port is the
/// only candidate — it is exactly what the lab file asked for. An automatic
/// rule tries the port it held the last time it was installed (`previous`),
/// so a machine restart keeps the address a user already has, then falls
/// back to port 0: whatever the kernel hands out.
pub fn sticky_bind(host: HostBinding, previous: Option<u16>) -> Vec<u16> {
    match host {
        HostBinding::Port(p) => vec![p],
        HostBinding::Auto => match previous {
            Some(p) if p != 0 => vec![p, 0],
            _ => vec![0],
        },
    }
}

/// A forward the lab daemon installed, as it tracks it per machine: enough
/// to tear it down, to report it in `vmlab status`, and to recognise its
/// rule the next time the machine's forwards are installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledForward {
    pub key: ForwardKey,
    /// The segment services' forward id.
    pub id: u64,
    /// The host port actually bound.
    pub host_port: u16,
    /// Whether vmlab picked `host_port` (the declaration omitted it).
    pub auto: bool,
    pub guest_ip: Ipv4Addr,
}

/// The ports to try for each of `rules`, in order, given what the same
/// machine had installed before (ADR-0003: decided before any bind).
///
/// An automatic rule whose key matches an automatic forward installed last
/// time asks for that port first ([`sticky_bind`]). Each previous forward
/// is matched at most once, in order, so two identical automatic rules keep
/// a port each rather than both asking for the first one's.
pub fn bind_candidates(previous: &[InstalledForward], rules: &[&ForwardRule]) -> Vec<Vec<u16>> {
    let mut used = vec![false; previous.len()];
    rules
        .iter()
        .map(|rule| {
            let mut held = None;
            if rule.host.is_auto() {
                let key = rule.key();
                if let Some(i) = previous
                    .iter()
                    .enumerate()
                    .position(|(i, p)| !used[i] && p.auto && p.key == key)
                {
                    used[i] = true;
                    held = Some(previous[i].host_port);
                }
            }
            sticky_bind(rule.host, held)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab_of(src: &str) -> Lab {
        crate::config::load_lab_source(src, "<test>", std::path::Path::new("/tmp"))
            .expect("parse")
            .lab
    }

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 0, 0, last)
    }

    fn mac(last: u8) -> MacAddr {
        MacAddr([0x52, 0x54, 0, 0, 0, last])
    }

    /// A lab drawing a forward from both sources at once.
    fn every_source() -> Lab {
        lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { host_port = 8080 to = "web:80" }
  }
  vm "web" { template = "x86_64/t" nic { segment = "lan" } }
  container "cache" { image = "redis" nic { segment = "lan" }
    port { host = 6379 container = 6379 }
  }
}"#,
        )
    }

    fn leased(pairs: &[(&str, u8)]) -> Observed {
        Observed {
            leases: pairs.iter().map(|(n, l)| (n.to_string(), ip(*l))).collect(),
            macs: pairs
                .iter()
                .map(|(n, l)| (n.to_string(), mac(*l)))
                .collect(),
        }
    }

    fn inputs<'a>(lab: &'a Lab, observed: &'a Observed) -> ForwardInputs<'a> {
        ForwardInputs { lab, observed }
    }

    /// One plan, both sources — the whole point of collapsing the two
    /// routines. Nothing may be missed because it was declared elsewhere.
    #[test]
    fn every_source_lands_in_one_plan() {
        let lab = every_source();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        let sources: Vec<&ForwardSource> = p.rules.iter().map(|r| &r.source).collect();
        assert_eq!(
            sources,
            vec![&ForwardSource::Declared, &ForwardSource::ContainerPort],
            "{p:#?}"
        );
        assert!(p.skipped.is_empty(), "{:#?}", p.skipped);
    }

    /// Every rule carries what installing it needs, from whichever source.
    #[test]
    fn a_rule_carries_its_segment_lease_and_priming_mac() {
        let lab = every_source();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        let declared = &p.rules[0];
        assert_eq!(declared.machine, "web");
        assert_eq!(declared.segment, "lan");
        assert_eq!(declared.host, HostBinding::Port(8080));
        assert_eq!(declared.guest_ip, ip(10));
        assert_eq!(declared.guest_port, 80);
        assert_eq!(declared.prime_mac, Some(mac(10)));

        let port = &p.rules[1];
        assert_eq!(port.machine, "cache");
        assert_eq!(port.host, HostBinding::Port(6379));
        assert_eq!(port.guest_port, 6379);
        assert_eq!(port.prime_mac, Some(mac(11)));
    }

    /// A forward for a machine with no lease is named, not dropped.
    #[test]
    fn a_missing_lease_is_reported_not_skipped_silently() {
        let lab = every_source();
        let obs = leased(&[("cache", 11)]); // web never got a lease
        let p = plan(&inputs(&lab, &obs), &[]);
        assert_eq!(
            p.rules.len(),
            1,
            "only the container port is installable: {p:#?}"
        );
        let reasons: Vec<&str> = p.skipped.iter().map(|s| s.why.as_str()).collect();
        assert_eq!(reasons.len(), 1, "the declared forward");
        assert!(reasons.iter().all(|r| r.contains("no lease")), "{p:#?}");
    }

    /// Two machines claiming one host port collide. Caught here, with both
    /// claimants named, rather than as a bind failure with neither.
    #[test]
    fn two_claims_on_one_host_port_conflict() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { host_port = 8080 to = "a:80" }
  }
  vm "a" { template = "x86_64/t" nic { segment = "lan" } }
  container "b" { image = "nginx" nic { segment = "lan" }
    port { host = 8080 container = 80 }
  }
}"#,
        );
        let obs = leased(&[("a", 10), ("b", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        assert_eq!(p.conflicts.len(), 1, "{p:#?}");
        assert_eq!(p.conflicts[0].host_port, 8080);
        assert_eq!(
            p.conflicts[0].claimants,
            ["a: forward".to_string(), "b: container port".to_string()]
        );
    }

    /// A scoped plan — what a single container's readiness recomputes —
    /// leaves everyone else's forwards alone rather than reporting them all
    /// as leaseless.
    #[test]
    fn a_scoped_plan_ignores_other_machines() {
        let lab = every_source();
        let obs = leased(&[("cache", 11)]);
        let p = plan(&inputs(&lab, &obs), &["cache".to_string()]);
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].machine, "cache");
        assert!(p.skipped.is_empty(), "{:#?}", p.skipped);
    }

    /// A declared forward pointing at nothing is announced. The old routine
    /// resolved it against VMs, then containers, then silently gave up.
    #[test]
    fn a_forward_to_an_unknown_machine_is_announced() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { host_port = 8080 to = "ghost:80" }
  }
  vm "a" { template = "x86_64/t" nic { segment = "lan" } }
}"#,
        );
        let obs = leased(&[("a", 10)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        assert!(p.rules.is_empty());
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].what.contains("ghost"), "{:#?}", p.skipped);
        assert!(p.skipped[0].why.contains("no such vm"), "{:#?}", p.skipped);
    }

    /// A machine with a lease but no recorded hardware address still gets its
    /// forward — priming is an optimisation, not a precondition.
    #[test]
    fn a_rule_without_a_mac_is_still_planned() {
        let lab = every_source();
        let obs = Observed {
            leases: HashMap::from([("cache".to_string(), ip(11))]),
            macs: HashMap::new(),
        };
        let p = plan(&inputs(&lab, &obs), &["cache".to_string()]);
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].prime_mac, None);
    }

    /// Every rule primes, whatever kind of machine it targets. A VM that
    /// never originates egress is as unreachable as a container that does
    /// not — the engine broadcasts the SYN either way, and a guest TCP stack
    /// drops broadcast-framed segments whichever guest it is.
    #[test]
    fn a_forward_to_a_vm_primes_the_nat_engine_too() {
        let lab = every_source();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        let declared = p
            .rules
            .iter()
            .find(|r| r.source == ForwardSource::Declared)
            .unwrap();
        assert_eq!(declared.machine, "web", "a vm target");
        assert_eq!(declared.prime_mac, Some(mac(10)));
        assert!(
            p.rules.iter().all(|r| r.prime_mac.is_some()),
            "no rule is left unprimed: {p:#?}"
        );
    }

    /// A lab whose forwards leave their host ports out.
    fn auto_lab() -> Lab {
        lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { to = "web:80" }
    forward { to = "web:443" }
  }
  vm "web" { template = "x86_64/t" nic { segment = "lan" } }
  container "cache" { image = "redis" nic { segment = "lan" }
    port { container = 6379 }
  }
}"#,
        )
    }

    /// An omitted host port plans as `Auto`, from either source.
    #[test]
    fn an_omitted_host_port_plans_as_auto() {
        let lab = auto_lab();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        assert_eq!(p.rules.len(), 3, "{p:#?}");
        assert!(
            p.rules.iter().all(|r| r.host == HostBinding::Auto),
            "{p:#?}"
        );
    }

    /// Automatic ports claim nothing, so any number of them never conflict.
    #[test]
    fn auto_rules_never_conflict() {
        let lab = auto_lab();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        assert!(p.conflicts.is_empty(), "{p:#?}");
        assert!(p.skipped.is_empty(), "{p:#?}");
    }

    /// Fixed ports beside automatic ones still contest, and only they do.
    #[test]
    fn fixed_ports_still_conflict_among_auto_ones() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { host_port = 8080 to = "a:80" }
    forward { to = "a:81" }
  }
  vm "a" { template = "x86_64/t" nic { segment = "lan" } }
  container "b" { image = "nginx" nic { segment = "lan" }
    port { container = 82 }
    port { host = 8080 container = 80 }
  }
}"#,
        );
        let obs = leased(&[("a", 10), ("b", 11)]);
        let p = plan(&inputs(&lab, &obs), &[]);
        assert_eq!(p.conflicts.len(), 1, "{p:#?}");
        assert_eq!(p.conflicts[0].host_port, 8080);
        assert_eq!(p.rules.len(), 3, "both auto rules and the winner: {p:#?}");
        assert_eq!(
            p.rules.iter().filter(|r| r.host.is_auto()).count(),
            2,
            "{p:#?}"
        );
    }

    /// A declared port is tried alone, with or without a history.
    #[test]
    fn sticky_bind_leaves_a_declared_port_alone() {
        assert_eq!(sticky_bind(HostBinding::Port(8080), None), vec![8080]);
        assert_eq!(
            sticky_bind(HostBinding::Port(8080), Some(40000)),
            vec![8080]
        );
    }

    /// An automatic port tries what it held last time, then any free port.
    #[test]
    fn sticky_bind_prefers_the_previous_auto_port() {
        assert_eq!(sticky_bind(HostBinding::Auto, None), vec![0]);
        assert_eq!(sticky_bind(HostBinding::Auto, Some(40000)), vec![40000, 0]);
        assert_eq!(sticky_bind(HostBinding::Auto, Some(0)), vec![0]);
    }

    /// The key a re-install recognises a rule by survives a new lease.
    #[test]
    fn a_rule_key_ignores_the_lease() {
        let lab = auto_lab();
        let first = plan(&inputs(&lab, &leased(&[("web", 10)])), &["web".into()]);
        let again = plan(&inputs(&lab, &leased(&[("web", 20)])), &["web".into()]);
        let k1: Vec<ForwardKey> = first.rules.iter().map(ForwardRule::key).collect();
        let k2: Vec<ForwardKey> = again.rules.iter().map(ForwardRule::key).collect();
        assert_eq!(k1, k2);
        assert_ne!(k1[0], k1[1], "two guest ports, two keys");
    }

    fn installed(rule: &ForwardRule, host_port: u16) -> InstalledForward {
        InstalledForward {
            key: rule.key(),
            id: 1,
            host_port,
            auto: rule.host.is_auto(),
            guest_ip: rule.guest_ip,
        }
    }

    /// A restart re-plans with a new lease; each automatic rule asks for the
    /// port it held before, then any.
    #[test]
    fn a_reinstall_asks_for_each_auto_rule_s_previous_port() {
        let lab = auto_lab();
        let before = plan(&inputs(&lab, &leased(&[("web", 10)])), &["web".into()]);
        let previous = vec![
            installed(&before.rules[0], 40001),
            installed(&before.rules[1], 40002),
        ];
        let after = plan(&inputs(&lab, &leased(&[("web", 20)])), &["web".into()]);
        let rules: Vec<&ForwardRule> = after.rules.iter().collect();
        assert_eq!(
            bind_candidates(&previous, &rules),
            vec![vec![40001, 0], vec![40002, 0]]
        );
    }

    /// The first install has no history, and a fixed rule never borrows one.
    #[test]
    fn a_first_install_binds_any_port_and_fixed_rules_their_own() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { host_port = 8080 to = "a:80" }
    forward { to = "a:81" }
  }
  vm "a" { template = "x86_64/t" nic { segment = "lan" } }
}"#,
        );
        let p = plan(&inputs(&lab, &leased(&[("a", 10)])), &[]);
        let rules: Vec<&ForwardRule> = p.rules.iter().collect();
        assert_eq!(bind_candidates(&[], &rules), vec![vec![8080], vec![0]]);
        let previous = vec![installed(&p.rules[0], 8080), installed(&p.rules[1], 40000)];
        assert_eq!(
            bind_candidates(&previous, &rules),
            vec![vec![8080], vec![40000, 0]]
        );
    }

    /// Two identical automatic rules keep a port each.
    #[test]
    fn identical_auto_rules_each_keep_their_own_port() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { to = "a:80" }
    forward { to = "a:80" }
  }
  vm "a" { template = "x86_64/t" nic { segment = "lan" } }
}"#,
        );
        let p = plan(&inputs(&lab, &leased(&[("a", 10)])), &[]);
        let rules: Vec<&ForwardRule> = p.rules.iter().collect();
        let previous = vec![installed(&p.rules[0], 40001), installed(&p.rules[1], 40002)];
        assert_eq!(
            bind_candidates(&previous, &rules),
            vec![vec![40001, 0], vec![40002, 0]]
        );
    }
}
