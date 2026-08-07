//! Semantic validation (PRD §5.1): everything that can be caught without
//! touching QEMU. Runs after schema checking and extraction.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use super::model::*;
use super::{Issue, IssueList};
use crate::profiles::Profile;
use crate::qemu::resolve::{
    Layer, default_profile, effective_profile_name, resolve_firmware, vm_arch,
};
use crate::template::TemplateMeta;

/// Host facilities the validator consults. The CLI wires the real template
/// store and wscript compiler; tests substitute fakes.
///
/// The lookups return the template's recorded hardware and the profile
/// itself rather than a bare "it exists", because §5.1 checks that reason
/// about hardware have to resolve it first — a VM's firmware or secure boot
/// may have been inherited from either layer (§5.2).
pub trait ValidationContext {
    /// Recorded metadata for a template in the store, if it is there.
    fn template_meta(&self, arch: &str, name: &str, version: Option<&str>) -> Option<TemplateMeta>;
    /// A named guest OS profile, if it is known.
    fn profile(&self, name: &str) -> Option<Profile>;
    /// Run a container's micro-VM hardware through the real resolver
    /// (declaration > profile) and report what it says. Delegated rather
    /// than restated: precedence has one implementation, and the message a
    /// missing layer produces is the message the user should read
    /// (ADR-0008). A permissive context may answer `Ok`.
    fn check_container_hardware(&self, container: &Container) -> Result<(), String>;
    /// Compile-check a wscript script at an absolute path.
    fn check_script(&self, path: &Path) -> Result<(), String>;

    fn template_exists(&self, arch: &str, name: &str, version: Option<&str>) -> bool {
        self.template_meta(arch, name, version).is_some()
    }
    fn profile_exists(&self, name: &str) -> bool {
        self.profile(name).is_some()
    }
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
        // Cross-host peering rides the supervisor's shared switch, so a
        // connect {} on a lab-local segment would be silently ignored.
        if let Some(c) = &seg.connect {
            if !seg.global {
                issues.push(Issue::at(
                    c.span,
                    format!(
                        "segment \"{}\" declares connect {{ }} but is not global — cross-host \
                         peering requires `global = true` (PRD §9.2)",
                        seg.name
                    ),
                ));
            }
            if c.host.trim().is_empty() {
                issues.push(Issue::at(
                    c.span,
                    format!("segment \"{}\": connect host must not be empty", seg.name),
                ));
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
    let mut segment_gateways: HashMap<String, Span> = HashMap::new();
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
        check_nics(
            lab,
            &vm.nics,
            &mut static_ips,
            &mut macs,
            &mut segment_gateways,
            &mut issues,
        );

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

        // virtiofs is a vhost-user device, so it needs no guest networking. Every
        // other transport lands on SMB, which is reachable only over a segment
        // (PRD §7.5) — and `auto` can still fall back to it at VM start.
        if vm.nics.is_empty()
            && vm
                .shares
                .iter()
                .any(|s| s.transport != ShareTransport::Virtiofs)
        {
            issues.push(Issue::at(
                vm.span,
                format!(
                    "vm \"{}\" declares shares but has no NICs — SMB shares are reachable only \
                     over a segment (PRD §7.5); set `transport = \"virtiofs\"` to share without \
                     one",
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
        check_web_pages(
            "vm",
            &vm.name,
            &vm.web,
            vm.nics.is_empty(),
            vm.span,
            &mut issues,
        );
        // The family a VM's logins are judged against is the §5.2 resolved
        // profile, so a lab that names its profile only on the template still
        // gets the Windows rules.
        check_logins(
            "vm",
            &vm.name,
            &vm.logins,
            login_family(effective_profile_name(vm, TemplateLayer::of(vm, ctx).meta()).as_deref()),
            &mut issues,
        );
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

        // Micro-VM hardware, through the same chain as a VM's. An unknown
        // profile is reported here rather than by the resolver's own error,
        // so it reads like every other unknown-profile issue.
        if let Some(p) = &c.profile
            && !ctx.profile_exists(p)
        {
            issues.push(Issue::at(c.span, format!("unknown profile \"{p}\"")));
        } else if let Err(msg) = ctx.check_container_hardware(c) {
            issues.push(Issue::at(c.span, msg));
        }

        if c.mode == ContainerMode::Idle {
            if c.entrypoint.is_some() || c.command.is_some() {
                issues.push(Issue::at(
                    c.span,
                    format!(
                        "idle container \"{}\" cannot declare `entrypoint` or `command`",
                        c.name
                    ),
                ));
            }
            if c.healthcheck.is_some() {
                issues.push(Issue::at(
                    c.span,
                    format!("idle container \"{}\" cannot declare a healthcheck", c.name),
                ));
            }
        }
        check_nics(
            lab,
            &c.nics,
            &mut static_ips,
            &mut macs,
            &mut segment_gateways,
            &mut issues,
        );

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

        check_web_pages(
            "container",
            &c.name,
            &c.web,
            c.nics.is_empty(),
            c.span,
            &mut issues,
        );
        // A container's guest is the OCI image inside a Linux micro-VM, so
        // its family is fixed regardless of the profile it names — the
        // `container` profile carries micro-VM size, not a guest OS.
        check_logins(
            "container",
            &c.name,
            &c.logins,
            LoginFamily::Linux,
            &mut issues,
        );

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
    check_dev_defaults(lab, &mut issues);

    // -- per-machine configuration steps ------------------------------------
    // `provision`/`playbook` blocks live inside the vm/container they
    // configure; the machine is the target, so there is nothing to
    // cross-reference here beyond the folders and scripts themselves.
    let mut scripts: Vec<(&PathBuf, Span)> = Vec::new();
    for vm in &lab.vms {
        for p in &vm.provisions {
            scripts.push((&p.script, p.span));
        }
        // config-weave ships guest binaries only for x86_64; reject machines
        // whose arch is statically known to differ. Unknown archs (registry
        // templates without `arch`) are caught by the daemon's preflight.
        let arch = match &vm.template {
            TemplateRef::Store { arch, .. } => Some(arch.as_str()),
            _ => vm.arch.as_deref(),
        };
        for p in &vm.playbooks {
            check_playbook(p, &file.root, &mut issues);
            if let Some(arch) = arch
                && arch != "x86_64"
            {
                issues.push(Issue::at(
                    p.span,
                    format!(
                        "playbook {} runs on \"{}\" ({arch}) — config-weave ships binaries only for x86_64",
                        p.path.display(),
                        vm.name
                    ),
                ));
            }
        }
    }
    for c in &lab.containers {
        for p in &c.provisions {
            scripts.push((&p.script, p.span));
        }
        for p in &c.playbooks {
            check_playbook(p, &file.root, &mut issues);
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
        let target_kind = if h.event.starts_with("vm.") {
            Some("vm")
        } else if h.event.starts_with("container.") {
            Some("container")
        } else if h.event.starts_with("snapshot.") {
            Some("machine")
        } else {
            None
        };
        if !h.targets.is_empty() && target_kind.is_none() {
            issues.push(Issue::at(
                h.span,
                format!(
                    "event \"{}\" is lab-wide and cannot declare targets",
                    h.event
                ),
            ));
        }
        for target in &h.targets {
            if !machine_exists(lab, target) {
                issues.push(Issue::at(
                    h.span,
                    format!("event handler targets undefined vm/container \"{target}\""),
                ));
            } else if target_kind == Some("vm") && !lab.vms.iter().any(|vm| vm.name == *target) {
                issues.push(Issue::at(
                    h.span,
                    format!(
                        "event \"{}\" can target only VMs, not \"{target}\"",
                        h.event
                    ),
                ));
            } else if target_kind == Some("container")
                && !lab
                    .containers
                    .iter()
                    .any(|container| container.name == *target)
            {
                issues.push(Issue::at(
                    h.span,
                    format!(
                        "event \"{}\" can target only containers, not \"{target}\"",
                        h.event
                    ),
                ));
            }
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
        // Build playbooks run on the synthetic "build" VM, and config-weave
        // is x86_64-only (§10.4).
        for p in &t.playbooks {
            check_playbook(p, &file.root, &mut issues);
            if t.arch != "x86_64" {
                issues.push(Issue::at(
                    p.span,
                    format!(
                        "playbook {} on a {} template — config-weave ships binaries only for x86_64",
                        p.path.display(),
                        t.arch
                    ),
                ));
            }
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
    check_secure_boot(vm, ctx, issues);
}

/// Secure boot exists only under UEFI: with SeaBIOS the cmdline builder has
/// no pflash to load a secboot OVMF into, so `secure_boot = true` would be
/// read nowhere and the VM would boot without it, silently. Either value may
/// have been inherited, so this resolves the §5.2 chain and names the layer
/// each side came from.
fn check_secure_boot(vm: &Vm, ctx: &dyn ValidationContext, issues: &mut IssueList) {
    let layer = TemplateLayer::of(vm, ctx);
    // The arch only decides which firmware applies when no layer names one;
    // a VM without one is already an error.
    let Some(arch) = vm_arch(vm) else { return };
    let profile_name = effective_profile_name(vm, layer.meta());
    let profile = match &profile_name {
        // An unknown profile name is reported on its own; without the
        // profile there is no floor to resolve against.
        Some(name) => match ctx.profile(name) {
            Some(p) => p,
            None => return,
        },
        None => default_profile(),
    };

    let choice = resolve_firmware(vm, layer.meta(), &profile, &arch);
    if !choice.secure_boot_unsupported() {
        return;
    }
    // With the template layer unknown, only a conflict the vm block decided
    // by itself is certain — nothing below it can override the vm block.
    let certain = layer.is_known()
        || (choice.firmware_layer == Layer::Vm && choice.secure_boot_layer == Layer::Vm);
    if !certain {
        return;
    }
    issues.push(Issue::at(
        vm.span,
        choice.conflict_message(&vm.name, profile_name.as_deref()),
    ));
}

/// What validation knows about a VM's template layer — §5.2's middle layer,
/// which it can only sometimes see.
enum TemplateLayer {
    /// A `scratch` VM has no template layer at all (§6.5).
    Absent,
    /// Boxed: the metadata dwarfs the other two variants.
    Known(Box<TemplateMeta>),
    /// The layer exists but its contents do not: a registry template is not
    /// pulled at validate time, and a store template that is missing is
    /// reported by `check_vm_template`. Either could have supplied any
    /// hardware value, so nothing below the vm block can be trusted.
    Unknown,
}

impl TemplateLayer {
    fn of(vm: &Vm, ctx: &dyn ValidationContext) -> Self {
        match &vm.template {
            TemplateRef::Scratch => Self::Absent,
            TemplateRef::Store {
                arch,
                name,
                version,
            } => match ctx.template_meta(arch, name, version.as_deref()) {
                Some(meta) => Self::Known(Box::new(meta)),
                None => Self::Unknown,
            },
            TemplateRef::Registry { .. } => Self::Unknown,
        }
    }

    fn meta(&self) -> Option<&TemplateMeta> {
        match self {
            Self::Known(meta) => Some(meta.as_ref()),
            Self::Absent | Self::Unknown => None,
        }
    }

    fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// The guest OS family a machine's `login {}` blocks are judged against.
///
/// Three-valued where its siblings are two-valued, and deliberately so. Both
/// of §19.2's family rules are statements about a *known* family — "the agent
/// is SYSTEM, so a passwordless logon is impossible" and "root is root" — and
/// neither is a claim vmlab can make about `custom`, whose whole contract is
/// that nothing is assumed (§5.3), or about a VM whose profile is only
/// knowable once its registry template is pulled. Those machines are left to
/// fail loudly at attach time (§19.2) rather than rejected here on a guess.
///
/// Not the same question as [`crate::labd::guest_os::guest_os_of`] (which
/// picks a config-weave binary) or [`crate::smb::guest_os_hint`] (which picks
/// a share mount command); both answer `Linux` for everything they cannot
/// place, which is the right default for *doing* something and the wrong one
/// for *rejecting* something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginFamily {
    Windows,
    Linux,
    /// No profile resolved, or one that names no family.
    Unknown,
}

fn login_family(profile: Option<&str>) -> LoginFamily {
    match profile {
        Some(p) if p.starts_with("windows") => LoginFamily::Windows,
        Some(p) if p.starts_with("linux") => LoginFamily::Linux,
        _ => LoginFamily::Unknown,
    }
}

/// §19.2's rules for a machine's identities — the ones WCL's own schema
/// validation cannot see, because each reads a second declaration: the
/// machine's resolved profile, or another `login` block beside it. The two
/// family rules and the one-default rule are §5.1's; the label-uniqueness
/// rule is the same "unique per machine" guard every other labelled child
/// block carries, and here it is what keeps the SSH selector addressable.
fn check_logins(
    kind: &str,
    machine: &str,
    logins: &[Login],
    family: LoginFamily,
    issues: &mut IssueList,
) {
    let mut labels: HashSet<&str> = HashSet::new();
    let mut default: Option<&Login> = None;
    for login in logins {
        // A Windows agent runs as LocalSystem and mints the logon with
        // `LogonUser`, so there is no credential-free route to the account —
        // every one of them is the S4U logon §19.3 already disqualified.
        if family == LoginFamily::Windows && login.password.is_none() {
            issues.push(Issue::at(
                login.span,
                format!(
                    "{kind} \"{machine}\": login \"{}\" has no `password` — a Windows guest has no \
                     credential-free logon (PRD §19.2)",
                    login.label
                ),
            ));
        }
        // Elevation is a Windows concept: it selects the linked token. On
        // Linux root is root, and a non-root user is not elevatable without
        // sudo — so the field could only be read nowhere.
        if family == LoginFamily::Linux && login.elevated.is_some() {
            issues.push(Issue::at(
                login.span,
                format!(
                    "{kind} \"{machine}\": login \"{}\" declares `elevated`, which is Windows-only \
                     (PRD §19.2)",
                    login.label
                ),
            ));
        }
        // The label is the SSH username selector (§19.2), so two of them on
        // one machine is an identity that cannot be addressed.
        if !labels.insert(&login.label) {
            issues.push(Issue::at(
                login.span,
                format!(
                    "{kind} \"{machine}\": duplicate login \"{}\" — the label is what an SSH \
                     username selects an identity by (PRD §19.2)",
                    login.label
                ),
            ));
        }
        if login.default == Some(true) {
            if let Some(first) = default {
                issues.push(Issue::at(
                    login.span,
                    format!(
                        "{kind} \"{machine}\": logins \"{}\" and \"{}\" both set `default = true` \
                         — a machine has one default identity (PRD §19.2)",
                        first.label, login.label
                    ),
                ));
            } else {
                default = Some(login);
            }
        }
    }
}

/// Structural checks shared by every `playbook {}` block, wherever it is
/// declared: the play is named, the folder is a real playbook, and the
/// variables can survive the trip to config-weave's `--var KEY=VALUE`.
fn check_playbook(p: &Playbook, root: &Path, issues: &mut IssueList) {
    if p.play.is_empty() {
        issues.push(Issue::at(
            p.span,
            format!("playbook {} has an empty play name", p.path.display()),
        ));
    }
    let dir = root.join(&p.path);
    if !dir.is_dir() {
        issues.push(Issue::at(
            p.span,
            format!("playbook {} is not a directory", p.path.display()),
        ));
    } else if !dir.join("playbook.wcl").is_file() {
        issues.push(Issue::at(
            p.span,
            format!("playbook {} has no playbook.wcl", p.path.display()),
        ));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for var in &p.vars {
        // config-weave binds each override as a `let <name> = …`, so the name
        // has to be a WCL identifier or the run fails inside the guest.
        if !is_wcl_identifier(&var.name) {
            issues.push(Issue::at(
                var.span,
                format!(
                    "playbook variable \"{}\" is not a valid identifier (letters, digits and \
                     underscores, not starting with a digit)",
                    var.name
                ),
            ));
        }
        if !seen.insert(var.name.as_str()) {
            issues.push(Issue::at(
                var.span,
                format!(
                    "playbook {} play {} sets variable \"{}\" twice",
                    p.path.display(),
                    p.play,
                    var.name
                ),
            ));
        }
    }
}

/// Mirrors config-weave's own `is_identifier` gate on `--var` keys.
fn is_wcl_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    segment_gateways: &mut HashMap<String, Span>,
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

        if nic.gateway {
            match seg {
                None => issues.push(Issue::at(
                    nic.span,
                    "`gateway = true` requires a declared segment and cannot be used with the \
                     built-in NAT interface",
                )),
                Some(segment) => {
                    if nic.ip.is_none() {
                        issues.push(Issue::at(
                            nic.span,
                            format!(
                                "gateway NIC on segment \"{}\" needs a static `ip`",
                                segment.name
                            ),
                        ));
                    }
                    if let (Some(ip), Some(net)) = (nic.ip, segment.subnet)
                        && ip != gateway_ip(net)
                    {
                        issues.push(Issue::at(
                            nic.span,
                            format!(
                                "gateway NIC on segment \"{}\" must use the segment router address {}",
                                segment.name,
                                gateway_ip(net)
                            ),
                        ));
                    }
                    if segment.nat {
                        issues.push(Issue::at(
                            nic.span,
                            format!(
                                "segment \"{}\" has a machine gateway, so built-in `nat` must be disabled",
                                segment.name
                            ),
                        ));
                    }
                    if segment.global {
                        issues.push(Issue::at(
                            nic.span,
                            format!(
                                "machine gateways are not supported on global segment \"{}\"",
                                segment.name
                            ),
                        ));
                    }
                    if segment_gateways
                        .insert(segment.name.clone(), nic.span)
                        .is_some()
                    {
                        issues.push(Issue::at(
                            nic.span,
                            format!("segment \"{}\" has more than one gateway NIC", segment.name),
                        ));
                    }
                }
            }
        }

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
                            || (ip == gateway_ip(net) && !nic.gateway)
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
/// Validate a machine's declared web pages: names are DNS labels (they
/// become URL path segments), unique per machine, and pages need a NIC (the
/// proxy reaches them over a segment forward).
fn check_web_pages(
    kind: &str,
    machine: &str,
    web: &[WebPage],
    nics_empty: bool,
    machine_span: Span,
    issues: &mut IssueList,
) {
    if !web.is_empty() && nics_empty {
        issues.push(Issue::at(
            machine_span,
            format!(
                "{kind} \"{machine}\" declares web pages but has no NICs — the proxy reaches \
                 them over a segment forward"
            ),
        ));
    }
    let mut seen: std::collections::HashMap<&str, Span> = std::collections::HashMap::new();
    for page in web {
        check_dns_label(&page.name, page.span, "web page name", issues);
        if seen.insert(&page.name, page.span).is_some() {
            issues.push(Issue::at(
                page.span,
                format!(
                    "duplicate web page \"{}\" on {kind} \"{machine}\"",
                    page.name
                ),
            ));
        }
    }
}

/// A lab has at most one default dev machine (§19.1).
///
/// This is the *only* `@dev` rule §5.1 owns. The decorator's own errors — an
/// undeclared `@dve`, a wrong-typed or unknown argument, `@dev` on a `nic {}`,
/// a repeated `@dev @dev` — come from WCL, which validates an instance
/// decorator against its declaration. What WCL cannot see is this one, because
/// it spans two machine blocks: the same class as the duplicate-static-IP rule
/// above. The message names both machines, since the point is that the reader
/// has to choose between them.
fn check_dev_defaults(lab: &Lab, issues: &mut IssueList) {
    let mut declared = lab
        .machines()
        .filter_map(|m| m.dev().filter(|d| d.default).map(|d| (m.name(), d.span)));
    let Some((first, _)) = declared.next() else {
        return;
    };
    for (name, span) in declared {
        issues.push(Issue::at(
            span,
            format!(
                "\"{name}\" and \"{first}\" both declare `@dev(default = true)` — a lab has at \
                 most one default dev machine (PRD §19.1)"
            ),
        ));
    }
}

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

    /// A template that records no hardware of its own — the store entry
    /// exists, so every layer below it decides.
    pub(crate) fn blank_meta(arch: &str, name: &str, version: Option<&str>) -> TemplateMeta {
        TemplateMeta {
            name: name.to_string(),
            arch: arch.to_string(),
            version: version.unwrap_or("1").to_string(),
            profile: None,
            cpus: None,
            memory: None,
            disk: None,
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            created: chrono::Utc::now(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
        }
    }

    /// Context where everything exists and compiles. Templates record no
    /// hardware and profiles assume nothing, so resolved-hardware checks
    /// see a blank slate.
    pub struct Permissive;
    impl ValidationContext for Permissive {
        fn template_meta(
            &self,
            arch: &str,
            name: &str,
            version: Option<&str>,
        ) -> Option<TemplateMeta> {
            Some(blank_meta(arch, name, version))
        }
        fn profile(&self, name: &str) -> Option<Profile> {
            Some(Profile {
                name: name.to_string(),
                ..Profile::default()
            })
        }
        /// Permissive by name and by nature: hardware resolution has its own
        /// tests, and the container fixtures below are about other rules.
        /// `container_without_a_size_layer_is_rejected` uses a real one.
        fn check_container_hardware(&self, _: &Container) -> Result<(), String> {
            Ok(())
        }
        fn check_script(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    /// Context backed by the real shipped profiles, with the store
    /// template's recorded hardware supplied by the test — what the
    /// resolved-hardware checks need to resolve against (§5.2).
    struct Hardware {
        profiles: crate::profiles::ProfileSet,
        meta: Option<TemplateMeta>,
    }

    impl Hardware {
        /// A store template that records nothing.
        fn blank() -> Self {
            Self::with_meta(blank_meta("x86_64", "t", None))
        }
        fn with_meta(meta: TemplateMeta) -> Self {
            Self {
                profiles: crate::profiles::ProfileSet::shipped().expect("shipped profiles"),
                meta: Some(meta),
            }
        }
        /// No such template in the store — the template layer is unknown.
        fn missing_template() -> Self {
            Self {
                profiles: crate::profiles::ProfileSet::shipped().expect("shipped profiles"),
                meta: None,
            }
        }
    }

    impl ValidationContext for Hardware {
        fn template_meta(&self, _: &str, _: &str, _: Option<&str>) -> Option<TemplateMeta> {
            self.meta.clone()
        }
        fn profile(&self, name: &str) -> Option<Profile> {
            self.profiles.get(name).cloned()
        }
        fn check_container_hardware(&self, container: &Container) -> Result<(), String> {
            crate::qemu::resolve_container(container, "x86_64", &self.profiles)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        }
        fn check_script(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    /// Validation messages against a real-profile context.
    fn hw_errs(ctx: &Hardware, src: &str) -> Vec<String> {
        validate(&lab(src), ctx)
            .into_iter()
            .map(|i| i.message)
            .collect()
    }

    fn assert_secure_boot_conflict(ctx: &Hardware, src: &str, needles: &[&str]) {
        let es = hw_errs(ctx, src);
        let found = es
            .iter()
            .find(|m| m.contains("secure boot needs UEFI"))
            .unwrap_or_else(|| panic!("expected a secure-boot conflict, got: {es:#?}"));
        for needle in needles {
            assert!(found.contains(needle), "missing {needle:?} in {found:?}");
        }
    }

    fn assert_no_secure_boot_conflict(ctx: &Hardware, src: &str) {
        let es = hw_errs(ctx, src);
        assert!(
            !es.iter().any(|m| m.contains("secure boot needs UEFI")),
            "unexpected secure-boot conflict: {es:#?}"
        );
    }

    /// The reported combination: SeaBIOS from one layer, secure boot from
    /// another. Each side names where it came from, since either may have
    /// been inherited.
    #[test]
    fn secure_boot_on_seabios_is_rejected_whichever_layer_it_came_from() {
        // Profile supplies SeaBIOS, VM block asks for secure boot.
        assert_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-legacy" secure_boot = true } }"#,
            [
                "secure_boot = true (from the vm block)",
                "profile \"windows-legacy\"",
            ]
            .as_slice(),
        );

        // VM block supplies SeaBIOS, profile floor asks for secure boot.
        assert_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-11" firmware = "seabios" } }"#,
            [
                "secure_boot = true (from profile \"windows-11\")",
                "firmware = \"seabios\" (from the vm block)",
            ]
            .as_slice(),
        );

        // The template records SeaBIOS; the VM asks for secure boot.
        let mut meta = blank_meta("x86_64", "t", None);
        meta.firmware = Some("seabios".into());
        assert_secure_boot_conflict(
            &Hardware::with_meta(meta),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" secure_boot = true } }"#,
            ["firmware = \"seabios\" (from the template)"].as_slice(),
        );

        // The template records secure boot on top of a SeaBIOS profile.
        let mut meta = blank_meta("x86_64", "t", None);
        meta.profile = Some("windows-legacy".into());
        meta.secure_boot = Some(true);
        assert_secure_boot_conflict(
            &Hardware::with_meta(meta),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" } }"#,
            [
                "secure_boot = true (from the template)",
                "profile \"windows-legacy\"",
            ]
            .as_slice(),
        );

        // Scratch VMs have no template layer at all (§6.5).
        assert_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "scratch" arch = "x86_64" profile = "windows-legacy"
  disk = 10GiB secure_boot = true } }"#,
            ["profile \"windows-legacy\""].as_slice(),
        );
    }

    /// Unset firmware is SeaBIOS on x86 (the QEMU default), so it swallows
    /// secure boot just as silently as naming SeaBIOS does.
    #[test]
    fn secure_boot_without_any_firmware_is_rejected_on_x86() {
        assert_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "custom" secure_boot = true } }"#,
            ["no firmware is set", "SeaBIOS"].as_slice(),
        );
        // Non-x86 has no SeaBIOS to fall back to, so firmware defaults to
        // UEFI and secure boot stands.
        assert_no_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "aarch64/t" profile = "custom" secure_boot = true } }"#,
        );
    }

    #[test]
    fn uefi_secure_boot_and_plain_seabios_validate() {
        // The whole point of windows-11: OVMF plus secure boot.
        assert_no_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-11" } }"#,
        );
        // SeaBIOS with secure boot switched off at the VM is fine.
        assert_no_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-11" firmware = "seabios"
  secure_boot = false } }"#,
        );
    }

    /// With the template layer unavailable, only a conflict written on the
    /// VM block itself is certain — the template could have supplied either
    /// value.
    #[test]
    fn unknown_template_layer_reports_only_vm_block_conflicts() {
        let registry = r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "ghcr.io/acme/win:1" arch = "x86_64" profile = "windows-legacy"
  secure_boot = true } }"#;
        assert_no_secure_boot_conflict(&Hardware::blank(), registry);

        assert_secure_boot_conflict(
            &Hardware::blank(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "ghcr.io/acme/win:1" arch = "x86_64" firmware = "seabios"
  secure_boot = true } }"#,
            ["from the vm block"].as_slice(),
        );

        // Same for a store template that is not there: the missing template
        // is the error to fix, not a hardware conflict inferred without it.
        assert_no_secure_boot_conflict(
            &Hardware::missing_template(),
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-legacy" secure_boot = true } }"#,
        );
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

    /// §19.1's one cross-block rule: two default dev machines, named. Both
    /// kinds carry `@dev`, so the pair may straddle them.
    #[test]
    fn two_default_dev_machines_are_rejected_naming_both() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  @dev(default = true) vm "dev01" { template = "x86_64/t" }
  @dev(default = true) container "buildbox" { image = "sdk:9.0" }
}"#;
        let es = errs(src);
        let found = es
            .iter()
            .find(|m| m.contains("default dev machine"))
            .unwrap_or_else(|| panic!("expected a duplicate-default error, got: {es:#?}"));
        assert!(
            found.contains("dev01") && found.contains("buildbox"),
            "{found}"
        );

        // The issue points at the second decorator, not at the machine block.
        let second = src.rfind("@dev(default = true)").unwrap();
        let issue = validate(&lab(src), &Permissive)
            .into_iter()
            .find(|i| i.message.contains("default dev machine"))
            .expect("the issue");
        assert_eq!(issue.span.map(|s| s.offset()), Some(second));
    }

    /// §19.4's bottom rung: **`validate` says nothing about agent
    /// capability.** It is a config check with no side effects, and the only
    /// statically available signal is the template's sealed `agent_version` —
    /// a free-form string, so comparing it is *inference*, which the
    /// capability doctrine rejects, and it would be `validate`'s first
    /// guest-content check. A dev machine on a template that records an
    /// ancient agent, or none at all, validates clean; the failure lands at
    /// `up` (a warning) and at attach (hard).
    #[test]
    fn validate_says_nothing_about_agent_capability() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  @dev(default = true) vm "dev01" { template = "x86_64/t" }
}"#;
        for agent_version in [None, Some("agent=ancient".to_string())] {
            let ctx = Hardware::with_meta(TemplateMeta {
                agent_version,
                ..blank_meta("x86_64", "t", None)
            });
            let issues: Vec<String> = hw_errs(&ctx, src);
            assert!(
                issues.is_empty(),
                "validate grew a guest-content check: {issues:#?}"
            );
        }
    }

    /// Everything else about `@dev` is legal: any number of dev machines,
    /// none or one declaring the default, and a bare `@dev` on either kind.
    #[test]
    fn dev_machines_are_otherwise_unconstrained() {
        for lab_src in [
            r#"@dev vm "dev01" { template = "x86_64/t" }"#,
            r#"@dev vm "dev01" { template = "x86_64/t" }
               @dev container "buildbox" { image = "sdk:9.0" }"#,
            r#"@dev(default = true, workspace = "./src") vm "dev01" { template = "x86_64/t" }
               @dev container "buildbox" { image = "sdk:9.0" }"#,
        ] {
            let es = errs(&format!("import <vmlab.wcl>\nlab \"l\" {{\n{lab_src}\n}}"));
            assert!(es.is_empty(), "{lab_src} → {es:#?}");
        }
    }

    #[test]
    fn machine_gateway_is_unique_static_and_disables_segment_nat() {
        let valid = r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "router" {
    template = "x86_64/t"
    nic { segment = "s" ip = "10.1.1.1" gateway = true }
    nic { nat = true }
  }
}"#;
        assert!(errs(valid).is_empty(), "valid machine gateway was rejected");

        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "s" ip = "10.1.1.1" gateway = true } }
  container "b" { image = "alpine" nic { segment = "s" ip = "10.1.1.1" gateway = true } }
}"#,
            "more than one gateway",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" nat = true }
  vm "a" { template = "x86_64/t" nic { segment = "s" ip = "10.1.1.1" gateway = true } }
}"#,
            "built-in `nat` must be disabled",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "s" gateway = true } }
}"#,
            "needs a static `ip`",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  vm "a" { template = "x86_64/t" nic { segment = "s" ip = "10.1.1.10" gateway = true } }
}"#,
            "must use the segment router address 10.1.1.1",
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
            fn template_meta(&self, _: &str, _: &str, _: Option<&str>) -> Option<TemplateMeta> {
                None
            }
            fn profile(&self, name: &str) -> Option<Profile> {
                Some(Profile {
                    name: name.to_string(),
                    ..Profile::default()
                })
            }
            fn check_container_hardware(&self, _: &Container) -> Result<(), String> {
                Ok(())
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
            "import <vmlab.wcl>\nlab \"l\" { vm \"a\" { template = \"x86_64/t\"\n  provision \"no/such/script.ws\" { } } }",
            "does not exist",
        );
    }

    #[test]
    fn shares_need_nics() {
        // Default transport is `auto`, which can still land on SMB at VM start.
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" share { host = "." guest = "/mnt/x" } }
}"#,
            "no NICs",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t"
    share { host = "." guest = "/mnt/x" transport = "smb" } }
}"#,
            "no NICs",
        );
    }

    #[test]
    fn virtiofs_shares_do_not_need_nics() {
        // vhost-user-fs is a local device — no segment involved (PRD §7.5).
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t"
    share { host = "." guest = "/mnt/x" transport = "virtiofs" } }
}"#,
        );
        assert!(
            !es.iter().any(|m| m.contains("no NICs")),
            "virtiofs share should not require a NIC, got: {es:#?}"
        );
    }

    #[test]
    fn web_pages_need_nics_and_unique_names() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" web "ui" { port = 80 } } }"#,
            "no NICs",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" nic { nat = true }
  web "ui" { port = 80 } web "ui" { port = 81 } } }"#,
            "duplicate web page",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" nic { nat = true } web "Bad Name" { port = 80 } } }"#,
            "must be a DNS label",
        );
    }

    /// §19.2's first rule: the Windows agent is LocalSystem and mints the
    /// logon with a credential, so a passwordless `login` on a Windows-family
    /// profile has no route to the account at all.
    #[test]
    fn a_windows_login_needs_a_password() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-server"
  login "dev" { user = "PROBE\\dev" } } }"#,
            "login \"dev\" has no `password`",
        );
        // The family resolves through §5.2, so a lab that names its profile
        // only on the template is judged the same way.
        let from_template = Hardware::with_meta(TemplateMeta {
            profile: Some("windows-11".into()),
            ..blank_meta("x86_64", "t", None)
        });
        assert!(
            hw_errs(
                &from_template,
                r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" login "dev" { user = "dev" } } }"#,
            )
            .iter()
            .any(|m| m.contains("has no `password`")),
            "the template's profile decides the family too"
        );
        // A password makes it legal, and nothing else about the login does.
        assert!(
            errs(
                r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "windows-server"
  login "dev" { user = "PROBE\\dev" password = "vmlab123!" } } }"#
            )
            .is_empty(),
            "a declared secret is all the rule wants"
        );
    }

    /// §19.2's second rule. Elevation selects a Windows linked token; on Linux
    /// root is root, so the field could only be read nowhere.
    #[test]
    fn elevated_is_windows_only() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "linux-modern"
  login "dev" { user = "dev" elevated = false } } }"#,
            "declares `elevated`, which is Windows-only",
        );
        // A container's guest is Linux whatever profile it names — the
        // `container` profile is micro-VM size, not a guest OS.
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "nginx:1" profile = "container"
  login "dev" { user = "dev" elevated = true } } }"#,
            "container \"c\": login \"dev\" declares `elevated`",
        );
        // Its absence is not the error — a Linux login without the field is
        // ordinary, and needs no password either.
        assert!(
            errs(
                r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "linux-modern"
  login "dev" { user = "dev" } } }"#
            )
            .is_empty(),
            "a plain Linux login is legal"
        );
    }

    /// §19.2's third rule, which names both offenders — the point of the
    /// message is to show the author the pair they have to choose between.
    #[test]
    fn one_machine_has_one_default_login() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "linux-modern"
  login "dev"   { user = "dev"   default = true }
  login "admin" { user = "admin" default = true } } }"#,
        );
        let named = es
            .iter()
            .find(|m| m.contains("default = true"))
            .unwrap_or_else(|| panic!("expected a duplicate-default error, got: {es:#?}"));
        assert!(named.contains("\"dev\""), "{named}");
        assert!(named.contains("\"admin\""), "{named}");
        // One `default = true` among several is the ordinary case.
        assert!(
            errs(
                r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "linux-modern"
  login "dev"   { user = "dev" default = true }
  login "admin" { user = "admin" } } }"#
            )
            .is_empty()
        );
    }

    /// The label is what an SSH username selects an identity by, so two of
    /// them on one machine name an identity nothing can address.
    #[test]
    fn login_labels_are_unique_per_machine() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" profile = "linux-modern"
  login "dev" { user = "dev" }
  login "dev" { user = "root" } } }"#,
            "duplicate login \"dev\"",
        );
    }

    /// Both family rules are claims about a *known* family. A profile that
    /// names none — `custom`, or a registry template whose profile is not
    /// knowable until it is pulled — is left to fail loudly at attach time
    /// (§19.2) rather than rejected here on a guess.
    #[test]
    fn an_unclassifiable_profile_triggers_neither_family_rule() {
        for profile in ["profile = \"custom\"", ""] {
            let src = format!(
                r#"import <vmlab.wcl>
lab "l" {{ vm "a" {{ template = "x86_64/t" {profile}
  login "dev" {{ user = "dev" elevated = true }} }} }}"#
            );
            assert!(
                errs(&src).is_empty(),
                "{profile:?} classifies no family, so neither rule may fire: {:#?}",
                errs(&src)
            );
        }
    }

    /// Web-page extract issues (port range, defaults, auth method rules)
    /// surface at parse.
    #[test]
    fn web_page_extract_rules() {
        let parse_err = |src: &str| {
            load_lab_source(src, "<test>", &std::env::temp_dir())
                .expect_err("source should be rejected")
                .issues
                .iter()
                .map(|i| i.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        };
        assert!(
            parse_err(
                r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" nic { nat = true } web "ui" { port = 99999 } } }"#
            )
            .contains("`port` must be between 1 and 65535, got 99999"),
        );
        // ntlm requires username+password; a stray token is flagged.
        let err = parse_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" nic { nat = true }
  web "ui" { port = 80 auth { method = :ntlm token = "x" } } } }"#,
        );
        assert!(err.contains("requires `username`"), "{err}");
        assert!(err.contains("not used by auth method `:ntlm`"), "{err}");
        // form requires login_path/login_body; bad login_method flagged.
        let err = parse_err(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" nic { nat = true }
  web "ui" { port = 80 auth { method = :form username = "u" password = "p" login_method = "PUT" } } } }"#,
        );
        assert!(err.contains("login_method` must be GET or POST"), "{err}");
        assert!(err.contains("requires `login_path`"), "{err}");
    }

    #[test]
    fn web_page_defaults_and_auth_parse() {
        let f = lab(r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" nic { nat = true }
  web "grafana" { port = 3000 }
  web "iis" { port = 80 path = "start" auth { method = :basic username = "admin" password = "pw" } } } }"#);
        let vm = &f.lab.vms[0];
        assert_eq!(vm.web.len(), 2);
        assert_eq!(vm.web[0].path, "/"); // default
        assert_eq!(vm.web[1].path, "/start"); // /-prefixed
        assert!(vm.web[0].auth.is_none());
        assert!(matches!(
            &vm.web[1].auth,
            Some(crate::config::model::WebAuth::Basic { username, .. }) if username == "admin"
        ));
    }

    /// Extract-stage share issues: bad transport values and the
    /// smb1/virtiofs conflict surface at parse, not validation.
    #[test]
    fn share_transport_parses_and_rejects_conflicts() {
        let parse_err = |src: &str| {
            load_lab_source(src, "<test>", &std::env::temp_dir())
                .expect_err("source should be rejected")
                .issues
                .iter()
                .map(|i| i.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        };
        let err = parse_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" {
    template = "x86_64/t"
    nic { nat = true }
    share { host = "." guest = "/mnt/x" transport = "nfs" }
  }
}"#,
        );
        assert!(
            err.contains("`transport` must be one of auto, virtiofs, smb, got `nfs`"),
            "{err}"
        );
        let err = parse_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" {
    template = "x86_64/t"
    nic { nat = true }
    share { host = "." guest = "/mnt/x" smb1 = true transport = "virtiofs" }
  }
}"#,
        );
        assert!(err.contains("conflicts"), "{err}");
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
    fn idle_container_rules() {
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "alpine" mode = :idle } }"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");

        for (extra, expected) in [
            (r#"entrypoint = ["/bin/sh"]"#, "entrypoint"),
            (r#"command = ["sleep", "infinity"]"#, "command"),
            (r#"healthcheck { command = ["true"] }"#, "healthcheck"),
        ] {
            assert_any_err(
                &format!(
                    "import <vmlab.wcl>\nlab \"l\" {{ container \"c\" {{ image = \"alpine\" mode = :idle {extra} }} }}"
                ),
                expected,
            );
        }
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

    /// A container's micro-VM size has no defensible default — what it needs
    /// depends on its image — so no layer supplying it is a validation error
    /// rather than a hardcoded guess the container silently OOMs under.
    #[test]
    fn container_without_a_size_layer_is_rejected() {
        let f = lab(r#"import <vmlab.wcl>
lab "l" { container "c" { image = "nginx" } }"#);
        let es = validate(&f, &Hardware::blank());
        let msgs: Vec<&str> = es.iter().map(|i| i.message.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("no `cpus`")),
            "expected a missing-hardware issue, got {msgs:?}"
        );
        // The message names both places the value can come from.
        let m = msgs.iter().find(|m| m.contains("no `cpus`")).unwrap();
        assert!(m.contains("declare `cpus`"), "{m}");
        assert!(m.contains("`profile`"), "{m}");
    }

    /// …and a container naming a profile that supplies one is fine, as is
    /// one declaring the values itself.
    #[test]
    fn container_sized_by_profile_or_declaration_validates() {
        for src in [
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "nginx" profile = "container" } }"#,
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "nginx" cpus = 2 memory = 512MiB } }"#,
        ] {
            let es = validate(&lab(src), &Hardware::blank());
            assert!(es.is_empty(), "{src}\n{es:?}");
        }
    }

    #[test]
    fn container_unknown_profile_is_rejected() {
        let f = lab(r#"import <vmlab.wcl>
lab "l" { container "c" { image = "nginx" profile = "no-such-profile" } }"#);
        let es = validate(&f, &Hardware::blank());
        assert!(
            es.iter().any(|i| i.message.contains("unknown profile")),
            "{es:?}"
        );
    }

    #[test]
    fn container_bad_image() {
        assert_any_err(
            r#"import <vmlab.wcl>
lab "l" { container "c" { image = "UPPER/Case" } }"#,
            "lowercase",
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
    fn event_handler_targets_match_event_machine_kind() {
        let source = r#"import <vmlab.wcl>
lab "l" {
  vm "v" { template = "x86_64/t" }
  container "c" { image = "alpine" }
  on "vm.ready" { run = "missing.ws" targets = ["c"] }
  on "lab.up" { run = "missing.ws" targets = ["v"] }
}"#;
        let errors = errs(source);
        assert!(
            errors
                .iter()
                .any(|message| message.contains("can target only VMs"))
        );
        assert!(
            errors
                .iter()
                .any(|message| message.contains("lab-wide and cannot declare targets"))
        );
    }

    #[test]
    fn connect_requires_global() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" connect { host = "otherhost:13947" } }
}"#,
            "requires `global = true`",
        );
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { global = true connect { host = "" } }
}"#,
            "connect host must not be empty",
        );
        // Global + a host: clean.
        let es = errs(
            r#"import <vmlab.wcl>
lab "l" {
  segment "s" { global = true connect { host = "otherhost:13947" } }
}"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");
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

    /// Validate against a root that actually contains `playbooks/base/playbook.wcl`.
    fn errs_with_playbook_dir(src: &str) -> (Vec<String>, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("playbooks/base")).unwrap();
        std::fs::write(root.path().join("playbooks/base/playbook.wcl"), "").unwrap();
        let f = load_lab_source(src, "<test>", root.path()).expect("source should parse");
        let es = validate(&f, &Permissive)
            .into_iter()
            .map(|i| i.message)
            .collect();
        (es, root)
    }

    #[test]
    fn playbook_missing_dir() {
        assert_err(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" playbook "no/such/pb" { play = "base" } }
}"#,
            "is not a directory",
        );
    }

    #[test]
    fn playbook_missing_playbook_wcl() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pb")).unwrap();
        let f = load_lab_source(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" playbook "pb" { play = "base" } }
}"#,
            "<test>",
            root.path(),
        )
        .expect("source should parse");
        let es: Vec<String> = validate(&f, &Permissive)
            .into_iter()
            .map(|i| i.message)
            .collect();
        assert!(
            es.iter().any(|m| m.contains("has no playbook.wcl")),
            "expected playbook.wcl error, got: {es:#?}"
        );
    }

    #[test]
    fn playbook_non_x86_64_machine() {
        let (es, _root) = errs_with_playbook_dir(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" {
    template = "aarch64/t"
    playbook "playbooks/base" { play = "base" }
  }
}"#,
        );
        assert!(
            es.iter()
                .any(|m| m.contains("binaries only for x86_64") && m.contains("aarch64")),
            "expected arch error, got: {es:#?}"
        );
    }

    /// Variable names become `let` bindings inside config-weave, so a name it
    /// could not bind is caught here rather than in the guest.
    #[test]
    fn playbook_var_name_and_duplicate_rules() {
        let (es, _root) = errs_with_playbook_dir(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" {
    template = "x86_64/t"
    playbook "playbooks/base" {
      play = "base"
      var "new-name" { value = "A" }
      var "domain"   { value = "corp.example.com" }
      var "domain"   { value = "other.example.com" }
    }
  }
}"#,
        );
        assert!(
            es.iter()
                .any(|m| m.contains("\"new-name\" is not a valid identifier")),
            "expected identifier error, got: {es:#?}"
        );
        assert!(
            es.iter()
                .any(|m| m.contains("sets variable \"domain\" twice")),
            "expected duplicate-var error, got: {es:#?}"
        );
    }

    /// Template playbooks reach the build VM and may carry variables too.
    #[test]
    fn template_playbook_vars_validate() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("scripts")).unwrap();
        std::fs::write(root.path().join("scripts/install.ws"), "").unwrap();
        std::fs::create_dir_all(root.path().join("pb")).unwrap();
        std::fs::write(root.path().join("pb/playbook.wcl"), "").unwrap();
        let f = load_lab_source(
            r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/t" } }
template "t" {
  arch = "x86_64"
  version = "1"
  source "scratch" { }
  disk = 10GiB
  provision "scripts/install.ws" { }
  playbook "pb" { play = "base" var "1bad" { value = "x" } }
}"#,
            "<test>",
            root.path(),
        )
        .expect("source should parse");
        let es: Vec<String> = validate(&f, &Permissive)
            .into_iter()
            .map(|i| i.message)
            .collect();
        assert!(
            es.iter()
                .any(|m| m.contains("\"1bad\" is not a valid identifier")),
            "expected identifier error, got: {es:#?}"
        );
    }

    #[test]
    fn playbook_clean() {
        let (es, _root) = errs_with_playbook_dir(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" {
    template = "x86_64/t"
    playbook "playbooks/base" {
      play = "base"
      var "domain"   { value = "corp.example.com" }
      var "new_name" { value = "A" }
    }
  }
  vm "b" { template = "aarch64/t" }
}"#,
        );
        assert!(es.is_empty(), "expected clean validation, got: {es:#?}");
    }
}
