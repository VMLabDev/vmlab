//! Semantic validation (PRD §5.1): everything that can be caught without
//! touching QEMU. Runs after schema checking and extraction.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use super::model::*;
use super::{Issue, IssueList};

/// Host facilities the validator consults. The CLI wires the real template
/// store and wscript compiler; tests substitute fakes.
pub trait ValidationContext {
    fn template_exists(&self, arch: &str, name: &str, version: Option<&str>) -> bool;
    fn profile_exists(&self, name: &str) -> bool;
    /// Compile-check a wscript script at an absolute path.
    fn check_script(&self, path: &Path) -> Result<(), String>;
}

/// Validate a parsed lab file. Returns every problem found (never short-
/// circuits — the goal is one complete report).
pub fn validate(file: &LabFile, ctx: &dyn ValidationContext) -> IssueList {
    let mut issues = IssueList::new();
    let lab = &file.lab;

    check_dns_label(&lab.name, lab.span, "lab name", &mut issues);

    // -- segments -------------------------------------------------------
    let mut seg_names: HashMap<&str, Span> = HashMap::new();
    for seg in &lab.segments {
        if seg_names.insert(&seg.name, seg.span).is_some() {
            issues.push(Issue::at(
                seg.span,
                format!("duplicate segment \"{}\"", seg.name),
            ));
        }
        check_dns_label(&seg.name, seg.span, "segment name", &mut issues);
        for other in &lab.segments {
            if !std::ptr::eq(seg, other)
                && let (Some(a), Some(b)) = (seg.subnet, other.subnet)
                && seg.name <= other.name
                && (a.contains(&b.network()) || b.contains(&a.network()))
            {
                issues.push(Issue::at(
                    seg.span,
                    format!(
                        "segments \"{}\" ({a}) and \"{}\" ({b}) have overlapping subnets",
                        seg.name, other.name
                    ),
                ));
            }
        }
        for target in &seg.routes_to {
            if !lab.segments.iter().any(|s| &s.name == target) {
                issues.push(Issue::at(
                    seg.span,
                    format!(
                        "segment \"{}\" routes_to undeclared segment \"{target}\"",
                        seg.name
                    ),
                ));
            }
        }
        for fwd in &seg.forwards {
            if !machine_exists(lab, &fwd.vm) {
                issues.push(Issue::at(
                    fwd.span,
                    format!("forward references undefined vm/container \"{}\"", fwd.vm),
                ));
            }
        }
        for s in &seg.sinkholes {
            if s.pattern.is_empty() {
                issues.push(Issue::at(s.span, "empty sinkhole pattern"));
            }
        }
    }

    // -- duplicate forward host ports across the lab ----------------------
    // Container `port` blocks compile into the same forward machinery, so
    // they share the uniqueness space with segment forwards.
    let mut fwd_ports: HashMap<u16, Span> = HashMap::new();
    for seg in &lab.segments {
        for fwd in &seg.forwards {
            if fwd_ports.insert(fwd.host_port, fwd.span).is_some() {
                issues.push(Issue::at(
                    fwd.span,
                    format!("duplicate forward host_port {}", fwd.host_port),
                ));
            }
        }
    }
    for c in &lab.containers {
        for p in &c.ports {
            if fwd_ports.insert(p.host_port, p.span).is_some() {
                issues.push(Issue::at(
                    p.span,
                    format!("duplicate forward host port {}", p.host_port),
                ));
            }
        }
    }

    // -- VMs --------------------------------------------------------------
    // VMs and containers share one name namespace (they share DNS, forwards,
    // and dependency waves).
    let mut machine_names: HashSet<&str> = HashSet::new();
    let mut static_ips: HashMap<Ipv4Addr, Span> = HashMap::new();
    let mut macs: HashMap<MacAddr, Span> = HashMap::new();
    for vm in &lab.vms {
        if !machine_names.insert(&vm.name) {
            issues.push(Issue::at(
                vm.span,
                format!(
                    "duplicate name \"{}\" — VM and container names share one namespace",
                    vm.name
                ),
            ));
        }
        check_dns_label(&vm.name, vm.span, "vm name", &mut issues);
        check_vm_template(file, vm, ctx, &mut issues);
        check_vm_hardware(file, vm, ctx, &mut issues);
        check_nics(lab, &vm.nics, &mut static_ips, &mut macs, &mut issues);

        for dep in &vm.depends_on {
            if !machine_exists(lab, dep) {
                issues.push(Issue::at(
                    vm.span,
                    format!(
                        "vm \"{}\" depends_on undefined vm/container \"{dep}\"",
                        vm.name
                    ),
                ));
            }
        }

        if !vm.shares.is_empty() && vm.nics.is_empty() {
            issues.push(Issue::at(
                vm.span,
                format!(
                    "vm \"{}\" declares shares but has no NICs — shares are reachable only over \
                     a segment (PRD §7.5)",
                    vm.name
                ),
            ));
        }
        for share in &vm.shares {
            let host = file.root.join(&share.host);
            if !host.is_dir() {
                issues.push(Issue::at(
                    share.span,
                    format!(
                        "share host path {} is not a directory",
                        share.host.display()
                    ),
                ));
            }
            if share.name.is_empty() {
                issues.push(Issue::at(
                    share.span,
                    format!(
                        "cannot derive a share name from guest path `{}` — set `name`",
                        share.guest
                    ),
                ));
            }
        }
        for m in &vm.media {
            check_media(&file.root, m, &mut issues);
        }
        for d in &vm.extra_disks {
            check_disk_block(&file.root, d, &mut issues);
        }
        if let Some(gpu) = &vm.gpu
            && gpu.mode == GpuMode::Passthrough
            && gpu.address.is_none()
        {
            issues.push(Issue::at(
                gpu.span,
                "gpu passthrough requires `address = \"<host PCI address>\"` (PRD §5.2)",
            ));
        }
        for path in [&vm.cdrom, &vm.floppy].into_iter().flatten() {
            if !file.root.join(path).is_file() {
                issues.push(Issue::at(
                    vm.span,
                    format!(
                        "vm \"{}\": attachment {} does not exist",
                        vm.name,
                        path.display()
                    ),
                ));
            }
        }
    }

    // -- containers ---------------------------------------------------------
    for c in &lab.containers {
        if !machine_names.insert(&c.name) {
            issues.push(Issue::at(
                c.span,
                format!(
                    "duplicate name \"{}\" — VM and container names share one namespace",
                    c.name
                ),
            ));
        }
        check_dns_label(&c.name, c.span, "container name", &mut issues);
        check_nics(lab, &c.nics, &mut static_ips, &mut macs, &mut issues);

        for dep in &c.depends_on {
            if !machine_exists(lab, dep) {
                issues.push(Issue::at(
                    c.span,
                    format!(
                        "container \"{}\" depends_on undefined vm/container \"{dep}\"",
                        c.name
                    ),
                ));
            }
        }

        if !c.ports.is_empty() && c.nics.is_empty() {
            issues.push(Issue::at(
                c.span,
                format!(
                    "container \"{}\" declares ports but has no NICs — forwards need a segment \
                     to reach the container over",
                    c.name
                ),
            ));
        }

        if !c.volumes.is_empty() && c.nics.is_empty() {
            issues.push(Issue::at(
                c.span,
                format!(
                    "container \"{}\" declares volumes but has no NICs — volumes mount over \
                     the network from the segment gateway (PRD §18)",
                    c.name
                ),
            ));
        }

        for v in &c.volumes {
            if let VolumeSource::Host(host) = &v.source {
                let path = file.root.join(host);
                if !path.is_dir() {
                    issues.push(Issue::at(
                        v.span,
                        format!("volume host path {} is not a directory", host.display()),
                    ));
                }
            }
        }
    }

    check_dependency_cycles(lab, &mut issues);

    // -- scripts ------------------------------------------------------------
    let mut scripts: Vec<(&PathBuf, Span)> = Vec::new();
    for p in &lab.provisions {
        scripts.push((&p.script, p.span));
        for vm in &p.vms {
            if !machine_exists(lab, vm) {
                issues.push(Issue::at(
                    p.span,
                    format!("provision scopes undefined vm/container \"{vm}\""),
                ));
            }
        }
    }
    for h in &lab.handlers {
        scripts.push((&h.run, h.span));
        if !EVENT_NAMES.contains(&h.event.as_str()) {
            issues.push(Issue::at(
                h.span,
                format!(
                    "unknown event \"{}\" (known: {})",
                    h.event,
                    EVENT_NAMES.join(", ")
                ),
            ));
        }
    }
    for t in &file.templates {
        for p in &t.provisions {
            scripts.push((&p.script, p.span));
        }
        if let Some(fb) = &t.first_boot {
            scripts.push((fb, t.span));
        }
    }
    for (script, span) in scripts {
        let path = file.root.join(script);
        if !path.is_file() {
            issues.push(Issue::at(
                span,
                format!("script {} does not exist", script.display()),
            ));
        } else if let Err(e) = ctx.check_script(&path) {
            issues.push(Issue::at(span, format!("{}: {e}", script.display())));
        }
    }

    // -- template definitions -----------------------------------------------
    let mut tdefs: HashSet<(&str, &str, &str)> = HashSet::new();
    for t in &file.templates {
        if !tdefs.insert((&t.arch, &t.name, &t.version)) {
            issues.push(Issue::at(
                t.span,
                format!(
                    "duplicate template definition {}/{}@{}",
                    t.arch, t.name, t.version
                ),
            ));
        }
        if t.version.is_empty() {
            issues.push(Issue::at(t.span, "template version must not be empty"));
        }
        if let Some(p) = &t.profile
            && !ctx.profile_exists(p)
        {
            issues.push(Issue::at(t.span, format!("unknown profile \"{p}\"")));
        }
        match &t.source {
            TemplateSource::Template {
                from:
                    TemplateRef::Store {
                        arch,
                        name,
                        version,
                    },
                span,
            } => {
                if !ctx.template_exists(arch, name, version.as_deref()) {
                    issues.push(Issue::at(
                        *span,
                        format!(
                            "layered build source {arch}/{name}{} not in the template store",
                            version
                                .as_ref()
                                .map(|v| format!("@{v}"))
                                .unwrap_or_default()
                        ),
                    ));
                }
            }
            TemplateSource::Iso(a) | TemplateSource::Qcow2(a) => {
                if let ArtefactSource::Path { path, span } = a
                    && !file.root.join(path).is_file()
                {
                    issues.push(Issue::at(
                        *span,
                        format!("source file {} does not exist", path.display()),
                    ));
                }
            }
            TemplateSource::Scratch { span } if t.disk.is_none() => {
                issues.push(Issue::at(
                    *span,
                    format!("scratch-built template \"{}\" requires `disk`", t.name),
                ));
            }
            _ => {}
        }
        for m in &t.media {
            check_media(&file.root, m, &mut issues);
        }
        for d in &t.extra_disks {
            check_disk_block(&file.root, d, &mut issues);
        }
    }

    issues
}

fn check_vm_template(file: &LabFile, vm: &Vm, ctx: &dyn ValidationContext, issues: &mut IssueList) {
    match &vm.template {
        TemplateRef::Scratch => {
            // §6.5: scratch demands explicit arch, profile, and disk.
            for (missing, what) in [
                (vm.arch.is_none(), "`arch`"),
                (vm.profile.is_none(), "`profile`"),
                (vm.disk.is_none(), "`disk`"),
            ] {
                if missing {
                    issues.push(Issue::at(
                        vm.template_span,
                        format!("scratch vm \"{}\" requires {what} (PRD §6.5)", vm.name),
                    ));
                }
            }
            if let Some(arch) = &vm.arch
                && !KNOWN_ARCHES.contains(&arch.as_str())
            {
                issues.push(Issue::at(
                    vm.span,
                    format!("unknown arch `{arch}` (known: {})", KNOWN_ARCHES.join(", ")),
                ));
            }
        }
        TemplateRef::Store {
            arch,
            name,
            version,
        } => {
            if let Some(vm_arch) = &vm.arch
                && vm_arch != arch
            {
                issues.push(Issue::at(
                    vm.span,
                    format!(
                        "vm \"{}\" sets arch = \"{vm_arch}\" but its template is {arch}/{name}",
                        vm.name
                    ),
                ));
            }
            if !ctx.template_exists(arch, name, version.as_deref()) {
                let local_def = file
                    .templates
                    .iter()
                    .any(|t| &t.arch == arch && &t.name == name);
                let hint = if local_def {
                    " (defined in this file — run `vmlab template build` first)"
                } else {
                    ""
                };
                issues.push(Issue::at(
                    vm.template_span,
                    format!(
                        "template {arch}/{name}{} not in the template store{hint}",
                        version
                            .as_ref()
                            .map(|v| format!("@{v}"))
                            .unwrap_or_default()
                    ),
                ));
            }
            if vm.disk.is_some() {
                issues.push(Issue::at(
                    vm.span,
                    format!(
                        "vm \"{}\": `disk` sets the primary disk size for scratch VMs only — \
                         clones inherit the template's disk (PRD §6.5); use `disk \"name\" {{}}` \
                         blocks for additional disks",
                        vm.name
                    ),
                ));
            }
        }
        TemplateRef::Registry { reference } => {
            if vm.arch.is_none() {
                issues.push(Issue::at(
                    vm.template_span,
                    format!(
                        "registry template `{reference}` requires an explicit `arch` (PRD §6.4)"
                    ),
                ));
            }
        }
    }
}

fn check_vm_hardware(
    _file: &LabFile,
    vm: &Vm,
    ctx: &dyn ValidationContext,
    issues: &mut IssueList,
) {
    if let Some(p) = &vm.profile
        && !ctx.profile_exists(p)
    {
        issues.push(Issue::at(vm.span, format!("unknown profile \"{p}\"")));
    }
}

/// `name` resolves against the unified VM + container namespace.
fn machine_exists(lab: &Lab, name: &str) -> bool {
    lab.vms.iter().any(|v| v.name == name) || lab.containers.iter().any(|c| c.name == name)
}

fn check_nics(
    lab: &Lab,
    nics: &[Nic],
    static_ips: &mut HashMap<Ipv4Addr, Span>,
    macs: &mut HashMap<MacAddr, Span>,
    issues: &mut IssueList,
) {
    for nic in nics {
        let seg = match (&nic.segment, nic.nat) {
            (Some(_), true) => {
                issues.push(Issue::at(
                    nic.span,
                    "nic declares both `segment` and `nat = true` — `nat = true` is the shorthand \
                     for the built-in NAT segment; pick one (PRD §9.7)",
                ));
                continue;
            }
            (None, false) => {
                issues.push(Issue::at(
                    nic.span,
                    "nic needs `segment = \"...\"` or `nat = true` (a machine with no nic blocks \
                     is air-gapped — an empty nic is meaningless)",
                ));
                continue;
            }
            (Some(name), false) => {
                let Some(seg) = lab.segments.iter().find(|s| &s.name == name) else {
                    issues.push(Issue::at(
                        nic.span,
                        format!("nic references undeclared segment \"{name}\""),
                    ));
                    continue;
                };
                Some(seg)
            }
            (None, true) => None, // built-in NAT segment
        };

        if let Some(ip) = nic.ip {
            match seg {
                None => issues.push(Issue::at(
                    nic.span,
                    "static `ip` is not supported on the built-in NAT segment — declare a \
                     segment with a subnet instead",
                )),
                Some(seg) => match seg.subnet {
                    None => issues.push(Issue::at(
                        nic.span,
                        format!(
                            "static ip {ip} on segment \"{}\" which has no declared subnet — \
                             deterministic addresses need `subnet = ...`",
                            seg.name
                        ),
                    )),
                    Some(net) => {
                        if !net.contains(&ip) {
                            issues.push(Issue::at(
                                nic.span,
                                format!(
                                    "static ip {ip} is outside segment \"{}\" subnet {net}",
                                    seg.name
                                ),
                            ));
                        } else if ip == net.network()
                            || ip == net.broadcast()
                            || ip == gateway_ip(net)
                        {
                            issues.push(Issue::at(
                                nic.span,
                                format!(
                                    "static ip {ip} collides with a reserved address on {net} \
                                     (network/broadcast/gateway {})",
                                    gateway_ip(net)
                                ),
                            ));
                        }
                    }
                },
            }
            if let Some(_prev) = static_ips.insert(ip, nic.span) {
                issues.push(Issue::at(nic.span, format!("duplicate static ip {ip}")));
            }
        }
        if let Some(mac) = nic.mac
            && macs.insert(mac, nic.span).is_some()
        {
            issues.push(Issue::at(nic.span, format!("duplicate MAC {mac}")));
        }
    }
}

/// The daemon claims the first usable address of every segment as its
/// gateway (DHCP/DNS/NAT/share endpoint).
pub fn gateway_ip(net: ipnet::Ipv4Net) -> Ipv4Addr {
    let base = u32::from(net.network());
    Ipv4Addr::from(base + 1)
}

fn check_dependency_cycles(lab: &Lab, issues: &mut IssueList) {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Visiting,
        Done,
    }
    // Dependency waves span VMs and containers, so cycles are detected over
    // the unified graph.
    fn visit<'a>(
        name: &'a str,
        deps: &HashMap<&'a str, &'a [String]>,
        state: &mut HashMap<&'a str, State>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match state.get(name) {
            Some(State::Done) => return None,
            Some(State::Visiting) => {
                let start = stack.iter().position(|n| *n == name).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(name.to_string());
                return Some(cycle);
            }
            None => {}
        }
        state.insert(name, State::Visiting);
        stack.push(name);
        if let Some(names) = deps.get(name) {
            for dep in names.iter() {
                if let Some(cycle) = visit(dep, deps, state, stack) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        state.insert(name, State::Done);
        None
    }

    let mut deps: HashMap<&str, &[String]> = HashMap::new();
    let mut roots: Vec<(&str, Span)> = Vec::new();
    for vm in &lab.vms {
        deps.insert(&vm.name, &vm.depends_on);
        roots.push((&vm.name, vm.span));
    }
    for c in &lab.containers {
        deps.insert(&c.name, &c.depends_on);
        roots.push((&c.name, c.span));
    }

    let mut state = HashMap::new();
    for (name, span) in roots {
        let mut stack = Vec::new();
        if let Some(cycle) = visit(name, &deps, &mut state, &mut stack) {
            issues.push(Issue::at(
                span,
                format!("dependency cycle: {}", cycle.join(" -> ")),
            ));
            return; // one cycle report is enough to act on
        }
    }
}

fn check_media(root: &Path, m: &Media, issues: &mut IssueList) {
    if !root.join(&m.from).is_dir() {
        issues.push(Issue::at(
            m.span,
            format!("media source folder {} does not exist", m.from.display()),
        ));
    }
}

fn check_disk_block(root: &Path, d: &DiskBlock, issues: &mut IssueList) {
    match (&d.size, &d.from) {
        (None, None) => issues.push(Issue::at(
            d.span,
            format!("disk \"{}\" needs `size` and/or `from`", d.name),
        )),
        _ => {
            if let Some(from) = &d.from
                && !root.join(from).is_dir()
            {
                issues.push(Issue::at(
                    d.span,
                    format!(
                        "disk \"{}\" source folder {} does not exist",
                        d.name,
                        from.display()
                    ),
                ));
            }
        }
    }
}

/// RFC-1035-ish label check for names that become DNS labels
/// (`<vm>.<lab>.<suffix>`, §9.5).
fn check_dns_label(name: &str, span: Span, what: &str, issues: &mut IssueList) {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        issues.push(Issue::at(
            span,
            format!(
                "{what} \"{name}\" must be a DNS label (letters, digits, hyphens; max 63 chars) — \
                 it becomes part of guest hostnames (PRD §9.5)"
            ),
        ));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::load_lab_source;

    /// Context where everything exists and compiles.
    pub struct Permissive;
    impl ValidationContext for Permissive {
        fn template_exists(&self, _: &str, _: &str, _: Option<&str>) -> bool {
            true
        }
        fn profile_exists(&self, _: &str) -> bool {
            true
        }
        fn check_script(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    fn lab(src: &str) -> LabFile {
        let tmp = std::env::temp_dir();
        load_lab_source(src, "<test>", &tmp).expect("source should parse")
    }

    fn errs(src: &str) -> Vec<String> {
        validate(&lab(src), &Permissive)
            .into_iter()
            .map(|i| i.message)
            .collect()
    }

    fn assert_err(src: &str, needle: &str) {
        let es = errs(src);
        assert!(
            es.iter().any(|m| m.contains(needle)),
            "expected error containing {needle:?}, got: {es:#?}"
        );
    }

    /// Collect issues whether they surface at extraction or validation —
    /// some structural container rules (volume shape, image syntax) are
    /// reported while extracting.
    fn assert_any_err(src: &str, needle: &str) {
        let tmp = std::env::temp_dir();
        let es: Vec<String> = match load_lab_source(src, "<test>", &tmp) {
            Ok(f) => validate(&f, &Permissive)
                .into_iter()
                .map(|i| i.message)
                .collect(),
            Err(e) => e.issues.into_iter().map(|i| i.message).collect(),
        };
        assert!(
            es.iter().any(|m| m.contains(needle)),
            "expected error containing {needle:?}, got: {es:#?}"
        );
    }

    #[test]
    fn undeclared_segment() {
        assert_err(
            "import <vmlab.wcl>\nlab \"l\" { vm \"a\" { template = \"x86_64/t\" nic { segment = \"nope\" } } }",
            "undeclared segment",
        );
    }

    #[test]
    fn static_ip_outside_subnet() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "s" ip = "10.2.0.5" } }
}"#,
            "outside segment",
        );
    }

    #[test]
    fn duplicate_static_ips_and_macs() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "s" ip = "10.1.1.10" } }
  vm "b" { template = "x86_64/t" nic { segment = "s" ip = "10.1.1.10" } }
}"#,
            "duplicate static ip",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { }
  vm "a" { template = "x86_64/t" nic { segment = "s" mac = "52:54:00:00:00:01" } }
  vm "b" { template = "x86_64/t" nic { segment = "s" mac = "52:54:00:00:00:01" } }
}"#,
            "duplicate MAC",
        );
    }

    #[test]
    fn dependency_cycle() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" depends_on = ["b"] }
  vm "b" { template = "x86_64/t" depends_on = ["a"] }
}"#,
            "dependency cycle",
        );
    }

    #[test]
    fn scratch_requirements() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "scratch" } }"#,
        );
        for needle in ["`arch`", "`profile`", "`disk`"] {
            assert!(
                es.iter().any(|m| m.contains(needle)),
                "missing {needle} in {es:#?}"
            );
        }
    }

    #[test]
    fn missing_template_in_store() {
        struct NoTemplates;
        impl ValidationContext for NoTemplates {
            fn template_exists(&self, _: &str, _: &str, _: Option<&str>) -> bool {
                false
            }
            fn profile_exists(&self, _: &str) -> bool {
                true
            }
            fn check_script(&self, _: &Path) -> Result<(), String> {
                Ok(())
            }
        }
        let f = lab("import <vmlab.wcl>\nlab \"l\" { vm \"a\" { template = \"x86_64/win\" } }");
        let es = validate(&f, &NoTemplates);
        assert!(
            es.iter()
                .any(|i| i.message.contains("not in the template store"))
        );
    }

    #[test]
    fn nat_and_segment_conflict() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { }
  vm "a" { template = "x86_64/t" nic { segment = "s" nat = true } }
}"#,
            "pick one",
        );
    }

    #[test]
    fn missing_script() {
        assert_err(
            "import <vmlab.wcl>\nlab \"l\" { vm \"a\" { template = \"x86_64/t\" }\n  provision \"no/such/script.ws\" { } }",
            "does not exist",
        );
    }

    #[test]
    fn shares_need_nics() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" share { host = "." guest = "/mnt/x" } }
}"#,
            "no NICs",
        );
    }

    #[test]
    fn unknown_event() {
        assert_err(
            "import <vmlab.wcl>\nlab \"l\" { vm \"a\" { template = \"x86_64/t\" }\n  on \"vm.exploded\" { run = \"x.ws\" } }",
            "unknown event",
        );
    }

    #[test]
    fn container_vm_name_collision() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" }
  container "a" { image = "nginx:1.27" }
}"#,
            "share one namespace",
        );
    }

    #[test]
    fn container_cross_kind_cycle() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" depends_on = ["c"] }
  container "c" { image = "nginx" depends_on = ["a"] }
}"#,
            "dependency cycle",
        );
    }

    #[test]
    fn container_deps_resolve_across_kinds() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" depends_on = ["c"] nic { segment = "s" } }
  container "c" { image = "nginx:1.27" nic { segment = "s" ip = "10.1.1.20" } }
}"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");
    }

    #[test]
    fn forward_to_container_resolves() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { forward { host_port = 18080 to = "c:80" } }
  container "c" { image = "nginx" nic { segment = "s" } }
}"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");
    }

    #[test]
    fn container_port_collides_with_forward() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { forward { host_port = 18080 to = "c:80" } }
  vm "v" { template = "x86_64/t" nic { segment = "s" } }
  container "c" { image = "nginx" nic { segment = "s" } port { host = 18080 container = 80 } }
}"#,
            "duplicate forward host port",
        );
    }

    #[test]
    fn container_ports_need_nics() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "nginx" port { host = 18080 container = 80 } }
}"#,
            "no NICs",
        );
    }

    #[test]
    fn airgapped_container_is_valid() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "alpine" command = ["sleep", "infinity"] }
}"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");
    }

    #[test]
    fn container_volume_and_env_rules() {
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "nginx" volume { target = "/data" } }
}"#,
            "volume needs",
        );
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "nginx" volume { host = "x" name = "y" target = "/data" } }
}"#,
            "pick one",
        );
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "nginx" volume { name = "data" target = "relative/path" } }
}"#,
            "absolute path",
        );
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "nginx" volume { host = "no/such/dir" target = "/data" } }
}"#,
            "not a directory",
        );
    }

    #[test]
    fn container_bad_image_and_restart() {
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "UPPER/Case" } }"#,
            "lowercase",
        );
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "nginx" restart = "sometimes" } }"#,
            "`restart` must be one of",
        );
    }

    #[test]
    fn container_events_bindable() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  container "c" { image = "nginx" }
  on "container.crashed" { run = "h.ws" }
}"#,
        );
        // The handler script does not exist, but the event name must be known.
        assert!(
            !es.iter().any(|m| m.contains("unknown event")),
            "container.crashed should be bindable: {es:#?}"
        );
    }

    #[test]
    fn clean_lab_validates() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "s" ip = "10.1.1.10" } }
  vm "b" { template = "x86_64/t" depends_on = ["a"] nic { nat = true } }
}"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");
    }
}
