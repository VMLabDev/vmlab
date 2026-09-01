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
                    what: format!("forward {} → \"{}\"", fwd.host_port, fwd.vm),
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
                    host: HostBinding::Port(fwd.host_port),
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
                    host: HostBinding::Port(port.host_port),
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
        let HostBinding::Port(p) = r.host;
        claims.entry(p).or_default().push(i);
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
}
