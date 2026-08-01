//! The **forward plan**: every host→guest port forward a lab's machines
//! require, worked out as one value.
//!
//! Three near-identical routines used to install these — one for segment
//! `forward {}` blocks, one for container `port {}` blocks, one for `web {}`
//! pages — each resolving a machine its own way, each taking its lease
//! address, each priming a hardware address and installing a rule. Their
//! differences were incidental: what a forward *is* does not depend on which
//! block declared it.
//!
//! Here they are one plan. Lease resolution is the only genuinely runtime
//! input and it arrives as data, so the plan is computable with no network:
//! tests build a lab and a lease table and assert on the rules.
//!
//! Two things the old routines did silently, this says out loud: a forward
//! whose machine has no lease is [`Skip`]ped with a reason rather than
//! dropped, and two forwards claiming one host port are reported as a
//! [`HostPortConflict`] at plan time rather than discovered at bind time.

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
    /// The loopback forward backing a proxied `web {}` page.
    WebPage { page: String },
}

impl ForwardSource {
    /// How the source names itself in a skip reason or a conflict.
    pub fn describe(&self) -> String {
        match self {
            ForwardSource::Declared => "forward".to_string(),
            ForwardSource::ContainerPort => "container port".to_string(),
            ForwardSource::WebPage { page } => format!("web page \"{page}\""),
        }
    }
}

/// Where a forward listens on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBinding {
    /// A declared port on every interface.
    Port(u16),
    /// An ephemeral loopback port: the executor binds `127.0.0.1:0` and reads
    /// the number back, because the web-page proxy needs it before the
    /// forward starts. Ephemeral bindings cannot collide, so they take no
    /// part in [`HostPortConflict`] detection.
    Ephemeral,
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

/// Two or more forwards claiming one host port. Config validation already
/// rejects this within a lab file; the plan catches whatever reaches it
/// anyway — a partial `up`, or a rule composed from more than one source —
/// before the loser fails at bind time with nothing to name it.
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

/// What the plan needs to know that is not in the lab file.
pub struct ForwardInputs<'a> {
    pub lab: &'a Lab,
    /// Plan only for these machines; empty means the whole lab.
    pub machines: &'a [String],
    /// The address each machine's lease holds. Absent = no lease yet.
    pub leases: &'a HashMap<String, Ipv4Addr>,
    /// Each machine's first hardware address, for NAT priming.
    pub macs: &'a HashMap<String, MacAddr>,
}

impl ForwardInputs<'_> {
    fn in_scope(&self, machine: &str) -> bool {
        self.machines.is_empty() || self.machines.iter().any(|m| m == machine)
    }

    /// The segment a machine's forwards ride: its first NIC's. `None` when it
    /// has no NIC — validation requires one for ports and web pages, so this
    /// only fires on a lab that got past validation with neither.
    fn first_segment(&self, machine: &str) -> Option<&str> {
        let nics = self
            .lab
            .vms
            .iter()
            .find(|v| v.name == machine)
            .map(|v| &v.nics)
            .or_else(|| {
                self.lab
                    .containers
                    .iter()
                    .find(|c| c.name == machine)
                    .map(|c| &c.nics)
            })?;
        nics.first().map(nic_segment_name)
    }

    fn knows(&self, machine: &str) -> bool {
        self.lab.vms.iter().any(|v| v.name == machine)
            || self.lab.containers.iter().any(|c| c.name == machine)
    }
}

/// Work out every forward the lab requires.
///
/// Order is declaration order: segment `forward {}` blocks first, then
/// container `port {}` blocks, then `web {}` pages — VMs before containers
/// within each.
pub fn plan(inputs: &ForwardInputs) -> ForwardPlan {
    let mut plan = ForwardPlan::default();

    // Segment `forward {}` blocks name their own segment and their target.
    for seg in &inputs.lab.segments {
        for fwd in &seg.forwards {
            if !inputs.in_scope(&fwd.vm) {
                continue;
            }
            if !inputs.knows(&fwd.vm) {
                plan.skipped.push(Skip {
                    what: format!("forward {} → \"{}\"", fwd.host_port, fwd.vm),
                    why: "no such vm or container in the lab".into(),
                });
                continue;
            }
            push(
                &mut plan,
                inputs,
                &fwd.vm,
                seg.name.clone(),
                HostBinding::Port(fwd.host_port),
                fwd.guest_port,
                fwd.proto,
                ForwardSource::Declared,
            );
        }
    }

    // Container `port {}` blocks land on the container's own segment.
    for c in &inputs.lab.containers {
        if !inputs.in_scope(&c.name) {
            continue;
        }
        for port in &c.ports {
            let Some(segment) =
                segment_or_skip(&mut plan, inputs, &c.name, &ForwardSource::ContainerPort)
            else {
                continue;
            };
            push(
                &mut plan,
                inputs,
                &c.name,
                segment,
                HostBinding::Port(port.host_port),
                port.container_port,
                port.proto,
                ForwardSource::ContainerPort,
            );
        }
    }

    // `web {}` pages take an ephemeral loopback port the console proxies to.
    let web_pages = inputs
        .lab
        .vms
        .iter()
        .map(|v| (&v.name, &v.web))
        .chain(inputs.lab.containers.iter().map(|c| (&c.name, &c.web)));
    for (machine, pages) in web_pages {
        if !inputs.in_scope(machine) {
            continue;
        }
        for page in pages {
            let source = ForwardSource::WebPage {
                page: page.name.clone(),
            };
            let Some(segment) = segment_or_skip(&mut plan, inputs, machine, &source) else {
                continue;
            };
            push(
                &mut plan,
                inputs,
                machine,
                segment,
                HostBinding::Ephemeral,
                page.port,
                Proto::Tcp,
                source,
            );
        }
    }

    plan.conflicts = conflicts(&plan.rules);
    plan
}

/// The single forward backing one declared web page, or why there isn't one.
/// The console asks for these one at a time, so it gets the one rule rather
/// than the whole plan.
pub fn web_page(inputs: &ForwardInputs, machine: &str, page: &str) -> Result<ForwardRule, String> {
    if !inputs.knows(machine) {
        return Err(format!(
            "no machine \"{machine}\" in lab \"{}\"",
            inputs.lab.name
        ));
    }
    let scope = [machine.to_string()];
    let plan = plan(&ForwardInputs {
        lab: inputs.lab,
        machines: &scope,
        leases: inputs.leases,
        macs: inputs.macs,
    });
    let wanted = ForwardSource::WebPage {
        page: page.to_string(),
    };
    if let Some(rule) = plan.rules.into_iter().find(|r| r.source == wanted) {
        return Ok(rule);
    }
    // Not planned: either the page is not declared, or something stopped it.
    let declared = declares_page(inputs.lab, machine, page);
    if !declared {
        return Err(format!("no web page \"{page}\" on \"{machine}\""));
    }
    Err(plan
        .skipped
        .iter()
        .find(|s| s.what.contains(&format!("web page \"{page}\"")))
        .map(|s| s.why.clone())
        .unwrap_or_else(|| format!("web page \"{page}\" on \"{machine}\" cannot be forwarded")))
}

fn declares_page(lab: &Lab, machine: &str, page: &str) -> bool {
    let pages = lab
        .vms
        .iter()
        .find(|v| v.name == machine)
        .map(|v| &v.web)
        .or_else(|| {
            lab.containers
                .iter()
                .find(|c| c.name == machine)
                .map(|c| &c.web)
        });
    pages.is_some_and(|ps| ps.iter().any(|p| p.name == page))
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
#[allow(clippy::too_many_arguments)]
fn push(
    plan: &mut ForwardPlan,
    inputs: &ForwardInputs,
    machine: &str,
    segment: String,
    host: HostBinding,
    guest_port: u16,
    proto: Proto,
    source: ForwardSource,
) {
    let Some(guest_ip) = inputs.leases.get(machine).copied() else {
        plan.skipped.push(Skip {
            what: format!("\"{machine}\": {}", source.describe()),
            why: "no lease — is it running and ready?".into(),
        });
        return;
    };
    plan.rules.push(ForwardRule {
        machine: machine.to_string(),
        segment,
        host,
        guest_ip,
        guest_port,
        proto,
        source,
        prime_mac: inputs.macs.get(machine).copied(),
    });
}

/// Host ports claimed more than once, in port order.
fn conflicts(rules: &[ForwardRule]) -> Vec<HostPortConflict> {
    let mut by_port: std::collections::BTreeMap<u16, Vec<String>> = Default::default();
    for r in rules {
        if let HostBinding::Port(p) = r.host {
            by_port
                .entry(p)
                .or_default()
                .push(format!("{}: {}", r.machine, r.source.describe()));
        }
    }
    by_port
        .into_iter()
        .filter(|(_, claimants)| claimants.len() > 1)
        .map(|(host_port, claimants)| HostPortConflict {
            host_port,
            claimants,
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

    /// A lab drawing a forward from all three sources at once.
    fn every_source() -> Lab {
        lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    forward { host_port = 8080 to = "web:80" }
  }
  vm "web" { template = "x86_64/t" nic { segment = "lan" }
    web "admin" { port = 9000 }
  }
  container "cache" { image = "redis" nic { segment = "lan" }
    port { host = 6379 container = 6379 }
  }
}"#,
        )
    }

    struct Observed {
        leases: HashMap<String, Ipv4Addr>,
        macs: HashMap<String, MacAddr>,
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

    fn inputs<'a>(lab: &'a Lab, obs: &'a Observed, machines: &'a [String]) -> ForwardInputs<'a> {
        ForwardInputs {
            lab,
            machines,
            leases: &obs.leases,
            macs: &obs.macs,
        }
    }

    /// One plan, all three sources — the whole point of collapsing the three
    /// routines. Nothing may be missed because it was declared elsewhere.
    #[test]
    fn every_source_lands_in_one_plan() {
        let lab = every_source();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs, &[]));
        let sources: Vec<&ForwardSource> = p.rules.iter().map(|r| &r.source).collect();
        assert_eq!(
            sources,
            vec![
                &ForwardSource::Declared,
                &ForwardSource::ContainerPort,
                &ForwardSource::WebPage {
                    page: "admin".into()
                },
            ],
            "{p:#?}"
        );
        assert!(p.skipped.is_empty(), "{:#?}", p.skipped);
    }

    /// Every rule carries what installing it needs, from whichever source.
    #[test]
    fn a_rule_carries_its_segment_lease_and_priming_mac() {
        let lab = every_source();
        let obs = leased(&[("web", 10), ("cache", 11)]);
        let p = plan(&inputs(&lab, &obs, &[]));
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

    /// A web page binds an ephemeral loopback port, so the console can read
    /// the number back before the forward starts.
    #[test]
    fn a_web_page_binds_an_ephemeral_port() {
        let lab = every_source();
        let obs = leased(&[("web", 10)]);
        let rule = web_page(&inputs(&lab, &obs, &[]), "web", "admin").unwrap();
        assert_eq!(rule.host, HostBinding::Ephemeral);
        assert_eq!(rule.guest_port, 9000);
        assert_eq!(rule.proto, Proto::Tcp);
        assert_eq!(rule.segment, "lan");
    }

    /// A forward for a machine with no lease is named, not dropped.
    #[test]
    fn a_missing_lease_is_reported_not_skipped_silently() {
        let lab = every_source();
        let obs = leased(&[("cache", 11)]); // web never got a lease
        let p = plan(&inputs(&lab, &obs, &[]));
        assert_eq!(
            p.rules.len(),
            1,
            "only the container port is installable: {p:#?}"
        );
        let reasons: Vec<&str> = p.skipped.iter().map(|s| s.why.as_str()).collect();
        assert_eq!(reasons.len(), 2, "the declared forward and the web page");
        assert!(reasons.iter().all(|r| r.contains("no lease")), "{p:#?}");
        assert!(p.skipped.iter().any(|s| s.what.contains("web page")));
    }

    /// And the console gets the same reason when it asks for the one page.
    #[test]
    fn a_web_page_without_a_lease_says_why() {
        let lab = every_source();
        let obs = leased(&[]);
        let err = web_page(&inputs(&lab, &obs, &[]), "web", "admin").unwrap_err();
        assert!(err.contains("no lease"), "{err}");
    }

    #[test]
    fn an_undeclared_web_page_is_named() {
        let lab = every_source();
        let obs = leased(&[("web", 10)]);
        let err = web_page(&inputs(&lab, &obs, &[]), "web", "nope").unwrap_err();
        assert!(err.contains("no web page \"nope\""), "{err}");
        let err = web_page(&inputs(&lab, &obs, &[]), "ghost", "admin").unwrap_err();
        assert!(err.contains("no machine \"ghost\""), "{err}");
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
        let p = plan(&inputs(&lab, &obs, &[]));
        assert_eq!(p.conflicts.len(), 1, "{p:#?}");
        assert_eq!(p.conflicts[0].host_port, 8080);
        assert_eq!(
            p.conflicts[0].claimants,
            ["a: forward".to_string(), "b: container port".to_string()]
        );
    }

    /// Ephemeral bindings never collide, however many pages a lab declares.
    #[test]
    fn ephemeral_web_forwards_never_conflict() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true }
  vm "a" { template = "x86_64/t" nic { segment = "lan" }
    web "one" { port = 80 }
    web "two" { port = 80 }
  }
}"#,
        );
        let obs = leased(&[("a", 10)]);
        let p = plan(&inputs(&lab, &obs, &[]));
        assert_eq!(p.rules.len(), 2);
        assert!(p.conflicts.is_empty(), "{p:#?}");
    }

    /// A scoped plan — what a single container's readiness recomputes —
    /// leaves everyone else's forwards alone rather than reporting them all
    /// as leaseless.
    #[test]
    fn a_scoped_plan_ignores_other_machines() {
        let lab = every_source();
        let obs = leased(&[("cache", 11)]);
        let p = plan(&inputs(&lab, &obs, &["cache".to_string()]));
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
        let p = plan(&inputs(&lab, &obs, &[]));
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
        let p = plan(&inputs(&lab, &obs, &["cache".to_string()]));
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].prime_mac, None);
    }
}
