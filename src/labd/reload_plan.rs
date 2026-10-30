//! The **reload plan** (PRD §7.1): what a running lab daemon does with a lab
//! file that changed under it, worked out in full before anything is rebuilt.
//!
//! A daemon reads `vmlab.wcl` when it starts and cannot be replaced while a
//! machine runs or a download is in flight — a fresh daemon cannot adopt a
//! live QEMU or a transfer. So it reads the file again and diffs it, machine
//! by machine, against what it holds. This module is that diff, as a value
//! (ADR-0003): it takes the declaration the daemon runs, the one on disk, and
//! what the daemon observes of each machine it holds — the block it was built
//! from, whether it is running, whether a download for it is in flight — and
//! performs no I/O.
//!
//! The network is the one thing a reload never touches. Segments are switches,
//! gateways and DHCP reservations assembled once, when the daemon starts, so
//! any change to them — or to a NIC's place on them — refuses the whole reload
//! and applies nothing.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::declaration_digest as digest;
use crate::config::model::{Lab, MachineCfg, Segment};
use crate::labd::network::{NAT_SEGMENT, nic_segment_name};
use crate::status::PendingReason;

/// One machine the daemon holds, as the plan needs to see it.
#[derive(Debug, Clone, Copy)]
pub struct Held<'a> {
    /// The declaration it was built from — not necessarily the daemon's
    /// current one: a machine left pending by an earlier reload still runs
    /// an older block.
    pub built: MachineCfg<'a>,
    /// Not stopped: running, starting, stopping or suspended. A machine in
    /// any of those has a process its rebuild would orphan.
    pub live: bool,
    /// A download for it is in flight; binding the result into a machine
    /// rebuilt under it would bind it into the wrong one.
    pub pulling: bool,
}

/// What a reload does to one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Stopped and changed: rebuild it from the new block in place.
    Rebuild,
    /// New in the file: build it and add it to the lab.
    Add,
    /// Gone from the file and stopped: drop it. Its disks stay.
    Drop,
    /// Changed or gone, but it cannot be rebuilt now.
    Pending(PendingReason),
}

/// A reload worked out in full.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadPlan {
    /// Every machine the reload acts on, by name. A machine whose block is
    /// unchanged and that runs what the file declares is absent.
    pub machines: BTreeMap<String, Action>,
    /// The `on {}` handlers changed.
    pub handlers: bool,
    /// The machines whose forwards — a segment's `forward {}` blocks naming
    /// them, or a container's `port {}` blocks — changed. A running one has
    /// them reinstalled; any other installs them when it next boots.
    pub forwards: BTreeSet<String>,
}

/// Everything a reload must refuse on: each change the network the daemon
/// assembled cannot take while the lab runs, in words naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkChanges(pub Vec<String>);

impl std::fmt::Display for NetworkChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "vmlab.wcl changes the lab's network, which cannot change while the lab runs: {}. \
             Nothing was reloaded. Run `vmlab down`, then this command again, to apply the \
             edit (or undo it)",
            self.0.join("; ")
        )
    }
}

/// Plan a reload of `new` into a daemon running `old`, holding `held`.
///
/// `segments` names the segments the daemon's network was assembled with,
/// the built-in NAT segment included when it was — the one fact about the
/// running network a NIC's change depends on that the declaration does not
/// carry.
pub fn plan(
    old: &Lab,
    new: &Lab,
    held: &BTreeMap<String, Held<'_>>,
    segments: &BTreeSet<String>,
) -> Result<ReloadPlan, NetworkChanges> {
    let changes = network_changes(old, new, held, segments);
    if !changes.is_empty() {
        return Err(NetworkChanges(changes));
    }
    let mut machines = BTreeMap::new();
    for (name, h) in held {
        let action = match new.machine(name) {
            None if h.pulling => Some(Action::Pending(PendingReason::Downloading)),
            None if h.live => Some(Action::Pending(PendingReason::Removed)),
            None => Some(Action::Drop),
            Some(now) if block(now) == block(h.built) => None,
            Some(_) if h.pulling => Some(Action::Pending(PendingReason::Downloading)),
            Some(_) if h.live => Some(Action::Pending(PendingReason::Running)),
            Some(_) => Some(Action::Rebuild),
        };
        if let Some(action) = action {
            machines.insert(name.clone(), action);
        }
    }
    for m in new.machines() {
        if !held.contains_key(m.name()) {
            machines.insert(m.name().to_string(), Action::Add);
        }
    }
    let forwards = old
        .machines()
        .chain(new.machines())
        .map(|m| m.name())
        .filter(|name| forwards_of(old, name) != forwards_of(new, name))
        .map(str::to_string)
        .collect();
    Ok(ReloadPlan {
        machines,
        handlers: digest(&old.handlers) != digest(&new.handlers),
        forwards,
    })
}

/// Whether `held` runs what `now` declares, as a reload compares them: a
/// machine the file has dropped does not.
pub fn runs_declared(held: MachineCfg<'_>, now: Option<MachineCfg<'_>>) -> bool {
    now.is_some_and(|now| block(now) == block(held))
}

/// Every forward `lab` declares to `machine`, as one comparable value.
fn forwards_of(lab: &Lab, machine: &str) -> String {
    let segment: Vec<(&str, &crate::config::model::Forward)> = lab
        .segments
        .iter()
        .flat_map(|s| s.forwards.iter().map(move |f| (s.name.as_str(), f)))
        .filter(|(_, f)| f.vm == machine)
        .collect();
    let ports = match lab.machine(machine) {
        Some(MachineCfg::Container(c)) => digest(&c.ports),
        _ => String::new(),
    };
    format!("{}{ports}", digest(&segment))
}

/// A machine's block as a reload compares it. A container's `port {}`
/// blocks are left out: they are forwards, which a reload reinstalls for a
/// running machine as it does a segment's (§7.1), so a change to them alone
/// leaves the container running what it declares.
fn block(m: MachineCfg<'_>) -> String {
    match m {
        MachineCfg::Vm(vm) => digest(vm),
        MachineCfg::Container(c) => {
            let mut c = c.clone();
            c.ports.clear();
            digest(&c)
        }
    }
}

/// Every change `new` makes to the network the daemon assembled from `old`.
fn network_changes(
    old: &Lab,
    new: &Lab,
    held: &BTreeMap<String, Held<'_>>,
    segments: &BTreeSet<String>,
) -> Vec<String> {
    let mut changes = Vec::new();
    if old.name != new.name {
        changes.push(format!(
            "the lab's name (\"{}\" to \"{}\")",
            old.name, new.name
        ));
    }
    for seg in &old.segments {
        match new.segments.iter().find(|s| s.name == seg.name) {
            None => changes.push(format!("segment \"{}\" removed", seg.name)),
            Some(now) => {
                let fields = segment_fields(seg, now);
                if !fields.is_empty() {
                    changes.push(format!(
                        "segment \"{}\" changed ({})",
                        seg.name,
                        fields.join(", ")
                    ));
                }
            }
        }
    }
    for seg in &new.segments {
        if !old.segments.iter().any(|s| s.name == seg.name) {
            changes.push(format!("segment \"{}\" added", seg.name));
        }
    }
    if digest(&old.records) != digest(&new.records) {
        changes.push("the lab's DNS records".to_string());
    }
    if digest(&old.sinkholes) != digest(&new.sinkholes) {
        changes.push("the lab's DNS sinkholes".to_string());
    }
    let global: BTreeSet<&str> = new
        .segments
        .iter()
        .filter(|s| s.global)
        .map(|s| s.name.as_str())
        .collect();
    for m in new.machines() {
        let name = m.name();
        match held.get(name) {
            Some(h) => {
                let (was, now) = (h.built.nics(), m.nics());
                for i in 0..was.len().max(now.len()) {
                    match (was.get(i), now.get(i)) {
                        (Some(_), None) => changes.push(format!("\"{name}\": nic {i} removed")),
                        (None, Some(_)) => changes.push(format!("\"{name}\": nic {i} added")),
                        (Some(a), Some(b)) => {
                            let mut fields = Vec::new();
                            if nic_segment_name(a) != nic_segment_name(b) {
                                fields.push("segment");
                            }
                            if a.ip != b.ip {
                                fields.push("static ip");
                            }
                            if a.mac != b.mac {
                                fields.push("mac");
                            }
                            if a.gateway != b.gateway {
                                fields.push("gateway");
                            }
                            if !fields.is_empty() {
                                changes.push(format!(
                                    "\"{name}\": nic {i} changed ({})",
                                    fields.join(", ")
                                ));
                            }
                        }
                        (None, None) => {}
                    }
                }
            }
            // A machine joining the lab joins it on a dynamic lease: a
            // static address is a DHCP reservation, a gateway NIC moves the
            // segment's router, and a global segment's membership is the
            // supervisor's — all fixed when the network was assembled.
            None => {
                for (i, nic) in m.nics().iter().enumerate() {
                    let seg = nic_segment_name(nic);
                    if !segments.contains(seg) {
                        changes.push(if seg == NAT_SEGMENT {
                            format!(
                                "\"{name}\": nic {i} uses the built-in NAT segment, \
                                 which this lab's network was assembled without"
                            )
                        } else {
                            format!("\"{name}\": nic {i} is on segment \"{seg}\", which is not assembled")
                        });
                    } else if nic.ip.is_some() {
                        changes.push(format!("\"{name}\": nic {i} declares a static ip"));
                    } else if nic.gateway {
                        changes.push(format!("\"{name}\": nic {i} is its segment's gateway"));
                    } else if global.contains(seg) {
                        changes.push(format!(
                            "\"{name}\": nic {i} joins global segment \"{seg}\""
                        ));
                    }
                }
            }
        }
    }
    changes
}

/// The attributes of a segment that differ, by the names a lab file gives
/// them. Its `forward {}` blocks are not among them: a reload reinstalls
/// forwards live.
fn segment_fields(a: &Segment, b: &Segment) -> Vec<&'static str> {
    let mut fields = Vec::new();
    let mut check = |name: &'static str, differs: bool| {
        if differs {
            fields.push(name);
        }
    };
    check("subnet", a.subnet != b.subnet);
    check("global", a.global != b.global);
    check("dhcp", a.dhcp != b.dhcp);
    check("nat", a.nat != b.nat);
    check("mtu", a.mtu != b.mtu);
    check("routes_to", a.routes_to != b.routes_to);
    check("dns", digest(&a.dns) != digest(&b.dns));
    check("connect", digest(&a.connect) != digest(&b.connect));
    check("route", digest(&a.routes) != digest(&b.routes));
    check("record", digest(&a.records) != digest(&b.records));
    check("block", digest(&a.block_rules) != digest(&b.block_rules));
    check(
        "redirect",
        digest(&a.redirect_rules) != digest(&b.redirect_rules),
    );
    check("sinkhole", digest(&a.sinkholes) != digest(&b.sinkholes));
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_lab_source;

    fn lab(src: &str) -> Lab {
        let dir = std::path::Path::new("/tmp");
        load_lab_source(
            &format!("import <vmlab.wcl>\nlab \"t\" {{\n{src}\n}}"),
            "<test>",
            dir,
        )
        .unwrap_or_else(|e| panic!("{e:?}"))
        .lab
    }

    const BASE: &str = r#"
  segment "corp" { subnet = "10.50.0.0/24" }
  container "a" { image = "a:1" nic { segment = "corp" } }
  container "b" { image = "b:1" nic { segment = "corp" } }
"#;

    /// Hold every machine of `built`, live when named in `live`, pulling
    /// when named in `pulling`.
    fn held<'a>(built: &'a Lab, live: &[&str], pulling: &[&str]) -> BTreeMap<String, Held<'a>> {
        built
            .machines()
            .map(|m| {
                (
                    m.name().to_string(),
                    Held {
                        built: m,
                        live: live.contains(&m.name()),
                        pulling: pulling.contains(&m.name()),
                    },
                )
            })
            .collect()
    }

    fn segs(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn reload(old: &str, new: &str, live: &[&str], pulling: &[&str]) -> ReloadPlan {
        let (old, new) = (lab(old), lab(new));
        plan(&old, &new, &held(&old, live, pulling), &segs(&["corp"])).expect("no network change")
    }

    fn refused(old: &str, new: &str) -> Vec<String> {
        let (old, new) = (lab(old), lab(new));
        plan(&old, &new, &held(&old, &["a"], &[]), &segs(&["corp"]))
            .expect_err("a network change")
            .0
    }

    fn actions(plan: &ReloadPlan) -> Vec<(&str, Action)> {
        plan.machines
            .iter()
            .map(|(n, a)| (n.as_str(), *a))
            .collect()
    }

    #[test]
    fn an_unchanged_file_does_nothing() {
        let plan = reload(BASE, BASE, &["a"], &[]);
        assert_eq!(plan, ReloadPlan::default());
    }

    #[test]
    fn a_stopped_changed_machine_is_rebuilt_and_a_running_one_is_pending() {
        let new = BASE
            .replace("\"a:1\"", "\"a:2\"")
            .replace("\"b:1\"", "\"b:2\"");
        let plan = reload(BASE, &new, &["a"], &[]);
        assert_eq!(
            actions(&plan),
            [
                ("a", Action::Pending(PendingReason::Running)),
                ("b", Action::Rebuild)
            ]
        );
    }

    #[test]
    fn a_download_in_flight_leaves_its_machine_pending() {
        let new = BASE.replace("\"b:1\"", "\"b:2\"");
        let plan = reload(BASE, &new, &[], &["b"]);
        assert_eq!(
            actions(&plan),
            [("b", Action::Pending(PendingReason::Downloading))]
        );
    }

    #[test]
    fn added_machines_are_built_and_removed_ones_dropped_once_stopped() {
        let new = r#"
  segment "corp" { subnet = "10.50.0.0/24" }
  container "a" { image = "a:1" nic { segment = "corp" } }
  container "c" { image = "c:1" nic { segment = "corp" } }
"#;
        let plan = reload(BASE, new, &[], &[]);
        assert_eq!(actions(&plan), [("b", Action::Drop), ("c", Action::Add)]);
        let plan = reload(BASE, new, &["b"], &[]);
        assert_eq!(
            actions(&plan),
            [
                ("b", Action::Pending(PendingReason::Removed)),
                ("c", Action::Add)
            ]
        );
    }

    /// A machine an earlier reload left pending runs an older block than the
    /// daemon's declaration: a file that leaves it alone still rebuilds it
    /// once it has stopped, and keeps it pending while it runs.
    #[test]
    fn a_machine_pending_from_an_earlier_reload_is_compared_by_what_it_runs() {
        let older = lab(BASE);
        let current = lab(&BASE.replace("\"a:1\"", "\"a:2\""));
        let mut h = held(&current, &[], &[]);
        h.get_mut("a").unwrap().built = older.machine("a").unwrap();
        let plan = plan(&current, &current, &h, &segs(&["corp"])).unwrap();
        assert_eq!(actions(&plan), [("a", Action::Rebuild)]);

        h.get_mut("a").unwrap().live = true;
        let plan = super::plan(&current, &current, &h, &segs(&["corp"])).unwrap();
        assert_eq!(
            actions(&plan),
            [("a", Action::Pending(PendingReason::Running))]
        );
    }

    /// Forwards and container ports are reinstalled live, so a change to them
    /// alone neither rebuilds nor pends a machine, and handlers are swapped.
    #[test]
    fn forwards_ports_and_handlers_leave_the_machines_alone() {
        let new = r#"
  segment "corp" {
    subnet = "10.50.0.0/24"
    forward { host_port = 8080 to = "a:80" }
  }
  container "a" { image = "a:1" nic { segment = "corp" } port { host = 8081 container = 80 } }
  container "b" { image = "b:1" nic { segment = "corp" } }
  on "vm.ready" { run = "ready.ws" }
"#;
        let plan = reload(BASE, new, &["a", "b"], &[]);
        assert!(plan.machines.is_empty(), "{plan:?}");
        assert!(plan.handlers);
        assert_eq!(plan.forwards, segs(&["a"]));
    }

    #[test]
    fn every_segment_change_is_refused_by_name() {
        let cases = [
            (r#"subnet = "10.51.0.0/24""#, "subnet"),
            ("nat = true", "nat"),
            ("dhcp = false", "dhcp"),
            ("mtu = 9000", "mtu"),
            ("global = true", "global"),
            (r#"dns { server = "10.50.0.9" }"#, "dns"),
            (
                r#"route { dest = "10.60.0.0/24" via = "10.50.0.254" }"#,
                "route",
            ),
        ];
        for (attr, field) in cases {
            let new = BASE.replace(
                r#"subnet = "10.50.0.0/24" }"#,
                &format!(r#"subnet = "10.50.0.0/24" {attr} }}"#),
            );
            let new = if field == "subnet" {
                BASE.replace("10.50.0.0/24", "10.51.0.0/24")
            } else {
                new
            };
            let why = refused(BASE, &new);
            assert_eq!(
                why,
                [format!("segment \"corp\" changed ({field})")],
                "{attr}"
            );
        }
    }

    #[test]
    fn adding_or_removing_a_segment_is_refused() {
        let added = format!("{BASE}\n  segment \"dmz\" {{ }}");
        assert_eq!(refused(BASE, &added), ["segment \"dmz\" added"]);
        let old = format!("{BASE}\n  segment \"dmz\" {{ }}");
        assert_eq!(refused(&old, BASE), ["segment \"dmz\" removed"]);
    }

    #[test]
    fn a_nic_change_is_a_network_change_even_on_a_stopped_machine() {
        let two = format!("{BASE}\n  segment \"dmz\" {{ }}");
        let moved = two.replace(
            r#"container "b" { image = "b:1" nic { segment = "corp" } }"#,
            r#"container "b" { image = "b:1" nic { segment = "dmz" } }"#,
        );
        assert_eq!(refused(&two, &moved), ["\"b\": nic 0 changed (segment)"]);
        let pinned = BASE.replace(
            r#"container "b" { image = "b:1" nic { segment = "corp" } }"#,
            r#"container "b" { image = "b:1" nic { segment = "corp" ip = "10.50.0.20" mac = "52:54:00:00:00:01" } }"#,
        );
        assert_eq!(
            refused(BASE, &pinned),
            ["\"b\": nic 0 changed (static ip, mac)"]
        );
        let second = BASE.replace(
            r#"container "b" { image = "b:1" nic { segment = "corp" } }"#,
            r#"container "b" { image = "b:1" nic { segment = "corp" } nic { segment = "corp" } }"#,
        );
        assert_eq!(refused(BASE, &second), ["\"b\": nic 1 added"]);
    }

    /// A machine joining a running lab takes a dynamic lease on a segment
    /// already assembled; anything the assembly would have had to know about
    /// is a network change.
    #[test]
    fn an_added_machine_must_fit_the_assembled_network() {
        let fits =
            format!("{BASE}\n  container \"c\" {{ image = \"c:1\" nic {{ segment = \"corp\" }} }}");
        let plan = reload(BASE, &fits, &["a"], &[]);
        assert_eq!(actions(&plan), [("c", Action::Add)]);

        let fixed = format!(
            "{BASE}\n  container \"c\" {{ image = \"c:1\" nic {{ segment = \"corp\" ip = \"10.50.0.30\" }} }}"
        );
        assert_eq!(refused(BASE, &fixed), ["\"c\": nic 0 declares a static ip"]);

        let natted =
            format!("{BASE}\n  container \"c\" {{ image = \"c:1\" nic {{ nat = true }} }}");
        let why = refused(BASE, &natted);
        assert!(why[0].contains("built-in NAT segment"), "{why:?}");
    }

    #[test]
    fn lab_dns_records_are_network() {
        let new = format!("{BASE}\n  record {{ name = \"x.example\" ip = \"10.50.0.9\" }}");
        assert_eq!(refused(BASE, &new), ["the lab's DNS records"]);
    }
}
