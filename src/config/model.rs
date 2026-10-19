//! Typed lab configuration model, extracted from a parsed `vmlab.wcl`
//! document (PRD §5). Spans reference byte offsets into the source file for
//! diagnostics.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};

pub type Span = (usize, usize);

/// A parsed lab file: one lab plus any template definitions that live
/// alongside it.
#[derive(Debug, Clone)]
pub struct LabFile {
    /// Directory containing `vmlab.wcl`; relative paths resolve against it.
    pub root: PathBuf,
    pub lab: Lab,
    pub templates: Vec<TemplateDef>,
}

/// A standalone template file (`vmlab template build -f templates.wcl`) may
/// contain templates with no lab.
#[derive(Debug, Clone)]
pub struct TemplateFile {
    pub templates: Vec<TemplateDef>,
}

#[derive(Debug, Clone)]
pub struct Lab {
    pub name: String,
    pub span: Span,
    /// Default for all VMs: open a VNC viewer on `up` (§11).
    pub gui: Option<bool>,
    /// Default for all VMs: refresh an out-of-date guest agent on `up`
    /// (§19.4). `None` = the floor, which is on.
    pub agent_update: Option<bool>,
    pub segments: Vec<Segment>,
    pub vms: Vec<Vm>,
    pub containers: Vec<Container>,
    pub handlers: Vec<Handler>,
    pub records: Vec<DnsRecord>,
    pub sinkholes: Vec<SinkholeRule>,
}

/// One machine's declaration, whichever kind it is.
///
/// A machine is anything a lab boots and attaches to a segment, and VMs and
/// containers share one name namespace. Most of what the daemon asks a
/// declaration for — its NICs, its web pages, what it depends on, the
/// configuration steps it declares — is the same question for both kinds, and
/// asking it used to mean writing out the "look in `vms`, then look in
/// `containers`" walk again.
#[derive(Debug, Clone, Copy)]
pub enum MachineCfg<'a> {
    Vm(&'a Vm),
    Container(&'a Container),
}

impl<'a> MachineCfg<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            MachineCfg::Vm(v) => &v.name,
            MachineCfg::Container(c) => &c.name,
        }
    }

    pub fn nics(&self) -> &'a [Nic] {
        match self {
            MachineCfg::Vm(v) => &v.nics,
            MachineCfg::Container(c) => &c.nics,
        }
    }

    pub fn depends_on(&self) -> &'a [String] {
        match self {
            MachineCfg::Vm(v) => &v.depends_on,
            MachineCfg::Container(c) => &c.depends_on,
        }
    }

    pub fn provisions(&self) -> &'a [Provision] {
        match self {
            MachineCfg::Vm(v) => &v.provisions,
            MachineCfg::Container(c) => &c.provisions,
        }
    }

    pub fn playbooks(&self) -> &'a [Playbook] {
        match self {
            MachineCfg::Vm(v) => &v.playbooks,
            MachineCfg::Container(c) => &c.playbooks,
        }
    }

    /// The identities this machine declares, in declaration order (§19.2).
    /// Empty on a machine that declares none, which attaches as the agent
    /// identity.
    pub fn logins(&self) -> &'a [Login] {
        match self {
            MachineCfg::Vm(v) => &v.logins,
            MachineCfg::Container(c) => &c.logins,
        }
    }

    /// The `@dev` decorator this machine carries, if any (§19.1). `None` on
    /// an ordinary machine, which is most of them.
    pub fn dev(&self) -> Option<&'a DevDecl> {
        match self {
            MachineCfg::Vm(v) => v.dev.as_ref(),
            MachineCfg::Container(c) => c.dev.as_ref(),
        }
    }
}

/// A machine's `@dev` decorator, as written (§19.1). Every argument is
/// optional; a bare `@dev` is a complete dev machine, and the
/// unset arguments resolve `@dev` > profile > floor in [`crate::dev`].
#[derive(Debug, Clone)]
pub struct DevDecl {
    /// Span of the decorator itself, so a diagnostic points at `@dev(…)`
    /// rather than at the whole machine block.
    pub span: Span,
    /// `default = true`: the lab file's choice of default dev machine. Note
    /// that a lone `@dev` machine is the default implicitly, so this being
    /// `false` does not mean "not the default" — [`crate::dev`] decides.
    pub default: bool,
    /// Host directory whose contents sync into the workspace, relative to
    /// the lab root (§19.6). `None` = no workspace, and nothing to sync.
    pub workspace: Option<PathBuf>,
    /// Guest path the workspace lands at. `None` = inherit from the profile,
    /// then the floor.
    pub workspace_guest: Option<String>,
}

impl Lab {
    /// Every machine in declaration order — VMs then containers, the order
    /// `status` and the `up` waves both present them in.
    pub fn machines(&self) -> impl Iterator<Item = MachineCfg<'_>> {
        self.vms
            .iter()
            .map(MachineCfg::Vm)
            .chain(self.containers.iter().map(MachineCfg::Container))
    }

    /// One machine by name. There is one namespace to look in, so a name that
    /// answers here is not also a machine of the other kind.
    pub fn machine(&self, name: &str) -> Option<MachineCfg<'_>> {
        self.machines().find(|m| m.name() == name)
    }

    /// Whether `up` may refresh `machine`'s guest agent (§19.4): the `vm`
    /// block's `agent_update`, else the `lab` block's, else on. A container
    /// declares none — its agent ships with the host and is never refreshed
    /// whatever this says.
    pub fn agent_update_for(&self, machine: &str) -> bool {
        let own = match self.machine(machine) {
            Some(MachineCfg::Vm(vm)) => vm.agent_update,
            _ => None,
        };
        own.or(self.agent_update).unwrap_or(true)
    }
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub name: String,
    pub span: Span,
    pub subnet: Option<Ipv4Net>,
    pub global: bool,
    pub dhcp: bool,
    pub nat: bool,
    /// Segment MTU. `None` means "use the default" (jumbo on NAT/global
    /// segments, 1500 otherwise) — see the network assembly. The guest↔gateway
    /// path is an in-memory UNIX socket, so a jumbo MTU here cuts per-frame
    /// overhead with no fragmentation risk.
    pub mtu: Option<u16>,
    pub routes_to: Vec<String>,
    pub dns: SegmentDns,
    pub connect: Option<Connect>,
    pub routes: Vec<Route>,
    pub records: Vec<DnsRecord>,
    pub forwards: Vec<Forward>,
    pub block_rules: Vec<BlockRule>,
    pub redirect_rules: Vec<RedirectRule>,
    pub sinkholes: Vec<SinkholeRule>,
}

#[derive(Debug, Clone, Default)]
pub struct SegmentDns {
    /// DNS server handed out via DHCP instead of the daemon gateway.
    pub server: Option<Ipv4Addr>,
    /// `false` suppresses the DNS option entirely.
    pub enabled: bool,
    /// Whether a `dns {}` block was declared at all.
    pub declared: bool,
    /// Span of the declared `dns {}` block (None when not declared) — the
    /// visual editor's block address.
    pub span: Option<Span>,
}

#[derive(Debug, Clone)]
pub struct Connect {
    pub host: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub dest: Ipv4Net,
    pub via: Ipv4Addr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct DnsRecord {
    /// May contain a leading wildcard label (`*.telemetry.example.com`).
    pub name: String,
    pub ip: Ipv4Addr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
    Both,
}

#[derive(Debug, Clone)]
pub struct Forward {
    pub host_port: u16,
    pub vm: String,
    pub guest_port: u16,
    pub proto: Proto,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum L4Proto {
    Tcp,
    Udp,
    Icmp,
}

#[derive(Debug, Clone)]
pub struct BlockRule {
    pub cidr: Ipv4Net,
    pub proto: Option<L4Proto>,
    pub port: Option<u16>,
    pub span: Span,
}

/// `ip[:port]` endpoint in a redirect rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPort {
    pub ip: Ipv4Addr,
    pub port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct RedirectRule {
    pub from: HostPort,
    pub to: HostPort,
    pub proto: Option<L4Proto>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SinkholeMode {
    Nxdomain,
    Zero,
}

#[derive(Debug, Clone)]
pub struct SinkholeRule {
    pub pattern: String,
    pub mode: SinkholeMode,
    pub span: Span,
}

/// Template reference as written in config (PRD §6.2, §6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRef {
    /// `scratch` — blank disk, no backing image (§6.5).
    Scratch,
    /// `<arch>/<name>[@<version>][#<variant>]` — local store.
    Store {
        arch: String,
        name: String,
        version: Option<String>,
        /// `None`: the template's default variant, or its base when it has
        /// no variants (§6.6).
        variant: Option<VariantRef>,
    },
    /// OCI registry reference, e.g. `ghcr.io/owner/name:version`; arch comes
    /// from the VM's explicit `arch` attribute (§6.4). `reference` is the
    /// registry reference alone, without the `#<variant>` suffix.
    Registry {
        reference: String,
        variant: Option<VariantRef>,
    },
}

/// The `#<variant>` suffix of a template reference: which disk of a template
/// version it selects (§6.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantRef {
    /// `#base` — the unsealed base disk; build contexts only when the
    /// template has variants.
    Base,
    /// `#<name>` — one sealed variant.
    Named(String),
}

impl VariantRef {
    /// The name as written after `#`.
    pub fn as_str(&self) -> &str {
        match self {
            VariantRef::Base => BASE_VARIANT,
            VariantRef::Named(n) => n,
        }
    }
}

impl std::fmt::Display for VariantRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The reserved `#base` selector: names a template version's base disk and is
/// never a variant's name (§6.6).
pub const BASE_VARIANT: &str = "base";

/// A variant name is `[a-z0-9][a-z0-9-]{0,31}` (§6.6). `base` passes here —
/// it is reserved for variant *names* by [`check_variant_name`], and means
/// the base disk in a reference.
pub fn variant_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Whether `s` can name a variant: the §6.6 grammar, and not `base`.
pub fn check_variant_name(s: &str) -> Result<(), String> {
    if s == BASE_VARIANT {
        return Err(format!(
            "`{BASE_VARIANT}` is reserved — it names the template's base disk, not a variant"
        ));
    }
    if !variant_name_ok(s) {
        return Err(format!(
            "malformed variant name `{s}`: a variant name is 1-32 characters of lowercase \
             letters, digits and `-`, starting with a letter or digit"
        ));
    }
    Ok(())
}

/// Split a trailing `#<variant>` off a template reference.
pub(crate) fn split_variant(s: &str) -> Result<(&str, Option<VariantRef>), String> {
    let Some((head, v)) = s.split_once('#') else {
        return Ok((s, None));
    };
    if v.is_empty() {
        return Err(format!(
            "malformed template reference `{s}`: empty variant after `#`"
        ));
    }
    if v == BASE_VARIANT {
        return Ok((head, Some(VariantRef::Base)));
    }
    if !variant_name_ok(v) {
        return Err(format!(
            "malformed template reference `{s}`: variant `{v}` must be 1-32 characters of \
             lowercase letters, digits and `-`, starting with a letter or digit"
        ));
    }
    Ok((head, Some(VariantRef::Named(v.to_string()))))
}

impl TemplateRef {
    /// The `#<variant>` suffix, if the reference carries one.
    pub fn variant(&self) -> Option<&VariantRef> {
        match self {
            TemplateRef::Scratch => None,
            TemplateRef::Store { variant, .. } | TemplateRef::Registry { variant, .. } => {
                variant.as_ref()
            }
        }
    }
}

impl std::fmt::Display for TemplateRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateRef::Scratch => return write!(f, "scratch"),
            TemplateRef::Store {
                arch,
                name,
                version,
                ..
            } => match version {
                Some(v) => write!(f, "{arch}/{name}@{v}")?,
                None => write!(f, "{arch}/{name}")?,
            },
            TemplateRef::Registry { reference, .. } => write!(f, "{reference}")?,
        }
        match self.variant() {
            Some(v) => write!(f, "#{v}"),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Vm {
    pub name: String,
    pub span: Span,
    pub template: TemplateRef,
    pub template_span: Span,
    pub arch: Option<String>,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    /// Bytes.
    pub memory: Option<u64>,
    /// Primary disk size in bytes — scratch VMs only.
    pub disk: Option<u64>,
    pub cdrom: Option<PathBuf>,
    pub floppy: Option<PathBuf>,
    pub depends_on: Vec<String>,
    pub nested: bool,
    /// Open a VNC viewer on `up` (§11); None = inherit the lab default.
    pub gui: Option<bool>,
    /// Refresh an out-of-date guest agent on `up` (§19.4); None = inherit
    /// the lab default.
    pub agent_update: Option<bool>,
    /// Wake the guest the moment it suspends to RAM (ACPI S3). Runtime
    /// policy the lab daemon applies, not hardware: the guest still sees S3
    /// advertised, so it is declared on the VM alone and never resolved
    /// through template or profile (ADR-0008 resolves hardware).
    pub prevent_sleep: bool,
    /// The guest clock (§7.3): runtime policy plus the RTC's start time, so
    /// declared on the machine alone like `prevent_sleep`.
    pub clock: Clock,
    pub display: Option<String>,
    /// QEMU `-cpu` model (§5.2); None = inherit template→profile, and with
    /// no layer naming one the builder's accelerator default applies.
    pub cpu_model: Option<String>,
    /// QEMU accelerator (§5.2); None = inherit template→profile, and with no
    /// layer naming one KVM is used when usable, TCG otherwise.
    pub accel: Option<AccelMode>,
    pub firmware: Option<Firmware>,
    pub tpm: Option<bool>,
    /// A hot-plug xHCI controller for host USB passthrough (§5.2.1); None =
    /// inherit the profile's, and with neither saying, off.
    pub usb: Option<bool>,
    pub secure_boot: Option<bool>,
    pub qemu_args: Vec<String>,
    pub gpu: Option<Gpu>,
    pub nics: Vec<Nic>,
    pub extra_disks: Vec<DiskBlock>,
    pub shares: Vec<Share>,
    pub media: Vec<Media>,
    /// Declared identities, in declaration order (§19.2).
    pub logins: Vec<Login>,
    /// Configuration steps for this VM, in declaration order (§10).
    pub provisions: Vec<Provision>,
    pub playbooks: Vec<Playbook>,
    /// The `@dev` decorator on this VM, if it carries one (§19.1).
    pub dev: Option<DevDecl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    Ovmf,
    Seabios,
}

impl Firmware {
    /// The schema spelling — the one string every surface that writes a
    /// firmware back out (rendered lab files, template metadata) names it by.
    pub fn as_str(self) -> &'static str {
        match self {
            Firmware::Ovmf => "ovmf",
            Firmware::Seabios => "seabios",
        }
    }

    /// The inverse of [`Firmware::as_str`], for reading a spelling back off a
    /// surface that stores it as free text (template metadata). A spelling
    /// this build does not know is `None` rather than an error: the reader is
    /// a store entry someone else may have written.
    pub fn parse(s: &str) -> Option<Firmware> {
        [Firmware::Ovmf, Firmware::Seabios]
            .into_iter()
            .find(|f| f.as_str() == s)
    }
}

/// The accelerator a machine asks for (§5.2). Undeclared, vmlab picks KVM
/// when it is usable and falls back to TCG; declared, the choice is the
/// machine's — `tcg` even where KVM would work (a guest KVM cannot run
/// reliably, such as Windows 95 on an AMD host), `kvm` with no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccelMode {
    Kvm,
    Tcg,
}

impl AccelMode {
    /// The schema spelling, which every surface that writes one back out
    /// (rendered lab files, template metadata) names it by.
    pub fn as_str(self) -> &'static str {
        match self {
            AccelMode::Kvm => "kvm",
            AccelMode::Tcg => "tcg",
        }
    }

    /// The inverse of [`AccelMode::as_str`]. A spelling this build does not
    /// know is `None`, as for [`Firmware::parse`]: the reader is a store
    /// entry someone else may have written.
    pub fn parse(s: &str) -> Option<AccelMode> {
        [AccelMode::Kvm, AccelMode::Tcg]
            .into_iter()
            .find(|a| a.as_str() == s)
    }
}

/// The `accel =` keyword table, shared by every block that declares one.
pub const ACCELS: &[(&str, AccelMode)] = &[("kvm", AccelMode::Kvm), ("tcg", AccelMode::Tcg)];

#[derive(Debug, Clone)]
pub struct Nic {
    pub span: Span,
    pub segment: Option<String>,
    pub nat: bool,
    pub ip: Option<Ipv4Addr>,
    /// Give this NIC the segment router identity (the first usable address).
    pub gateway: bool,
    pub mac: Option<MacAddr>,
    pub isolated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MacAddr(pub [u8; 6]);

impl std::fmt::Display for MacAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl std::str::FromStr for MacAddr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split([':', '-']).collect();
        if parts.len() != 6 {
            return Err(format!("malformed MAC address `{s}`"));
        }
        let mut b = [0u8; 6];
        for (i, p) in parts.iter().enumerate() {
            b[i] = u8::from_str_radix(p, 16).map_err(|_| format!("malformed MAC address `{s}`"))?;
        }
        Ok(MacAddr(b))
    }
}

#[derive(Debug, Clone)]
pub struct DiskBlock {
    pub name: String,
    pub span: Span,
    pub size: Option<u64>,
    pub from: Option<PathBuf>,
}

/// How a share reaches the guest. `Auto` picks virtiofs when the host has a
/// virtiofsd and the guest profile advertises the client, else SMB — decided
/// at VM start (vhost-user-fs devices cannot hotplug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareTransport {
    #[default]
    Auto,
    Virtiofs,
    Smb,
}

#[derive(Debug, Clone)]
pub struct Share {
    pub span: Span,
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
    pub smb1: bool,
    pub name: String,
    pub transport: ShareTransport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Iso,
    Floppy,
}

#[derive(Debug, Clone)]
pub struct Media {
    pub span: Span,
    pub kind: MediaKind,
    pub from: PathBuf,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuMode {
    Passthrough,
    Virgl,
    Vulkan,
}

impl GpuMode {
    /// Whether the guest's scanout is a host GL texture (egl-headless) rather
    /// than a 2D surface — the modes QMP `screendump` cannot capture.
    pub fn gl_display(self) -> bool {
        matches!(self, GpuMode::Virgl | GpuMode::Vulkan)
    }
}

#[derive(Debug, Clone)]
pub struct Gpu {
    pub mode: GpuMode,
    /// The host GPU by name (§5.2), declared in the host config rather than
    /// here because a PCI address is a fact about one machine: the device
    /// itself under `passthrough`, the GPU that renders under
    /// `virgl`/`vulkan`. `None` takes the host's default.
    pub host_gpu: Option<String>,
    pub span: Span,
}

/// An OCI container declared in the lab, run in a micro-VM (PRD §18).
#[derive(Debug, Clone)]
pub struct Container {
    pub name: String,
    pub span: Span,
    /// Image reference as written; normalised (docker.io shorthand etc.) by
    /// the OCI image layer at pull time.
    pub image: ImageRef,
    pub image_span: Span,
    /// Workload mode runs the OCI process; idle mode keeps only the
    /// micro-VM and guest agent running for exec-driven use.
    pub mode: ContainerMode,
    /// The guest clock (§7.3), as for a VM.
    pub clock: Clock,
    /// Entrypoint override (exec form); `None` = image default.
    pub entrypoint: Option<Vec<String>>,
    /// Cmd override (exec form); `None` = image default.
    pub command: Option<Vec<String>>,
    pub workdir: Option<String>,
    /// `uid[:gid]` or a user/group name resolved against the image.
    pub user: Option<String>,
    /// Guest profile supplying micro-VM hardware defaults (§5.3). Containers
    /// have no template layer, so this is the only inheritance they have.
    pub profile: Option<String>,
    /// Micro-VM vCPUs; falls back to the profile (`qemu::resolve_container`).
    pub cpus: Option<u32>,
    /// Micro-VM RAM in bytes; falls back to the profile.
    pub memory: Option<u64>,
    /// VM or container names — the namespaces are unified.
    pub depends_on: Vec<String>,
    pub nics: Vec<Nic>,
    pub env: Vec<EnvVar>,
    pub volumes: Vec<Volume>,
    pub ports: Vec<PortMap>,
    pub healthcheck: Option<Healthcheck>,
    /// Declared identities, in declaration order (§19.2).
    pub logins: Vec<Login>,
    /// Configuration steps for this container, in declaration order (§10).
    pub provisions: Vec<Provision>,
    pub playbooks: Vec<Playbook>,
    /// The `@dev` decorator on this container, if it carries one (§19.1).
    pub dev: Option<DevDecl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerMode {
    #[default]
    Workload,
    Idle,
}

#[derive(Debug, Clone)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    pub span: Span,
}

/// Where a container mount's data lives on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeSource {
    /// Bind mount of a host path (relative paths resolve against the lab root).
    Host(PathBuf),
    /// Named volume kept under the lab dir; shared across containers by name,
    /// retained until lab destroy.
    Named(String),
}

#[derive(Debug, Clone)]
pub struct Volume {
    pub source: VolumeSource,
    /// Absolute mount path inside the container.
    pub target: String,
    pub read_only: bool,
    pub span: Span,
}

/// Host→container port forward — sugar for the segment forward machinery.
#[derive(Debug, Clone)]
pub struct PortMap {
    pub host_port: u16,
    pub container_port: u16,
    pub proto: Proto,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Healthcheck {
    /// Probe command (exec form) run inside the container; exit 0 = healthy.
    pub command: Vec<String>,
    pub interval: std::time::Duration,
    pub timeout: std::time::Duration,
    pub retries: u32,
    pub start_period: std::time::Duration,
    pub span: Span,
}

/// OCI container image reference as written in config; syntax-checked here,
/// normalised (docker.io shorthand, default tag) by the OCI image layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub reference: String,
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reference)
    }
}

/// A labelled identity declared on a machine (PRD §19.2): the account a
/// surface attaches *as*, its secret, and whether the session is elevated.
///
/// Declared on the machine and not on `@dev`, because `exec`, `shell` and
/// `cp` reach every machine — an unmarked machine needs an identity too. The secret
/// is plaintext by decision, not by omission: the account exists because the
/// lab's own provisioning created it, so the string already sits in the
/// provision script beside it (§19.2, §1.2).
#[derive(Debug, Clone)]
pub struct Login {
    /// What `--user` and `as_login` select this identity by. Carrying the
    /// *label* rather than the account is what lets one account be declared
    /// twice at different elevation.
    pub label: String,
    /// The guest account, e.g. `PROBE\dev` or `dev`.
    pub user: String,
    /// The account's password. Absent is legal only where a credential-free
    /// logon exists — never on Windows, where the agent is SYSTEM (§19.2).
    pub password: Option<String>,
    /// Whether the session is elevated. `None` is "not declared", which is
    /// what §19.2's Linux rule is about. Deliberately not resolved here: the
    /// default is true on Windows and nothing at all on Linux, so only a
    /// caller that knows the guest family can fold it in.
    pub elevated: Option<bool>,
    /// Whether this is the machine's default identity. `None` is "not
    /// declared"; [`default_login`] folds in the lone-login case.
    pub default: Option<bool>,
    pub span: Span,
}

/// A machine's default identity: the login marked `default = true`, or — where
/// none is — the only login it declares.
///
/// The implicit case matches `@dev`'s shape (§19.1/§19.2): a lone declaration
/// never has to meet the concept. More than one `default = true` is a §5.1
/// error, so the first is taken here rather than the ambiguity being repeated.
pub fn default_login(logins: &[Login]) -> Option<&Login> {
    logins
        .iter()
        .find(|l| l.default == Some(true))
        .or(match logins {
            [only] => Some(only),
            _ => None,
        })
}

/// wscript run once during `up`, after the machine that declares it is ready.
#[derive(Debug, Clone)]
pub struct Provision {
    pub script: PathBuf,
    pub span: Span,
}

/// config-weave playbook assignment: apply `play` from the playbook folder at
/// `path` to the machine that declares it — on `up`, and on demand afterwards.
#[derive(Debug, Clone)]
pub struct Playbook {
    /// Playbook folder, relative to the lab root.
    pub path: PathBuf,
    pub play: String,
    /// Ceiling on each config-weave invocation; every run after a reboot gets
    /// the full value again.
    pub timeout: std::time::Duration,
    /// `--var name=value` overrides for this machine's run, in declaration
    /// order.
    pub vars: Vec<PlaybookVar>,
    pub span: Span,
}

impl Playbook {
    /// `timeout` when the block leaves it out.
    pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);
}

/// One variable override passed to config-weave as `--var name=value`.
#[derive(Debug, Clone)]
pub struct PlaybookVar {
    pub name: String,
    /// Passed through verbatim; config-weave parses it as a WCL expression
    /// where it can and falls back to a string.
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Handler {
    pub event: String,
    pub run: PathBuf,
    /// Optional VM/container names. Empty binds the event lab-wide.
    pub targets: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TemplateDef {
    pub name: String,
    pub span: Span,
    pub arch: String,
    pub version: String,
    /// Full OCI repository this template publishes to (host/owner/[group/]name).
    /// When set, `build` bumps the version against the registry's tags and
    /// `push` defaults its target here (PRD §6.4).
    pub registry: Option<String>,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub disk: Option<u64>,
    pub display: Option<String>,
    /// QEMU `-cpu` model for the build VM, recorded for clones (§5.2).
    pub cpu_model: Option<String>,
    /// QEMU accelerator for the build VM, recorded for clones (§5.2).
    pub accel: Option<AccelMode>,
    pub firmware: Option<Firmware>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    pub nested: bool,
    /// Watch the build VM in QEMU's own window (§11).
    pub gui: bool,
    pub qemu_args: Vec<String>,
    /// Wscript script (path relative to the template root) run the first time a VM
    /// is instantiated from this template, before it is reported ready (§6.1).
    pub first_boot: Option<PathBuf>,
    /// Bake the vmlab-agent service into the image (default true; the escape
    /// hatch for guests where the install step misbehaves).
    pub agent: bool,
    pub source: TemplateSource,
    pub media: Vec<Media>,
    pub provisions: Vec<Provision>,
    /// config-weave playbooks applied to the build VM, interleaved with
    /// provisions in declaration order (§10.4); steps stream as structured
    /// build progress.
    pub playbooks: Vec<Playbook>,
    pub nics: Vec<Nic>,
    pub extra_disks: Vec<DiskBlock>,
    /// Sealed variants built on the base after it seals, in declaration
    /// order (§6.6). Empty for a base-only template.
    pub variants: Vec<TemplateVariantDef>,
}

impl TemplateDef {
    /// The variant a reference with no `#` selects once built: the one
    /// declaring `default = true`, or a lone variant. `None` for a base-only
    /// template, and for declarations §5.1 refuses (several defaults, or
    /// several variants and no default).
    pub fn default_variant(&self) -> Option<&TemplateVariantDef> {
        match self.variants.as_slice() {
            [] => None,
            [only] if only.default != Some(false) => Some(only),
            several => {
                let mut defaults = several.iter().filter(|v| v.default == Some(true));
                match (defaults.next(), defaults.next()) {
                    (Some(d), None) => Some(d),
                    _ => None,
                }
            }
        }
    }

    /// The variant declared as `name`.
    pub fn variant(&self, name: &str) -> Option<&TemplateVariantDef> {
        self.variants.iter().find(|v| v.name == name)
    }
}

/// One `variant "<name>" {}` inside a template definition (§6.6): the steps
/// that turn the sealed base into this variant, and the hardware its build
/// boot and its clones run on where it differs from the base's.
#[derive(Debug, Clone)]
pub struct TemplateVariantDef {
    pub name: String,
    pub span: Span,
    /// `default = ...` as declared; `None` when absent.
    pub default: Option<bool>,
    pub description: Option<String>,
    /// First-boot wscript for clones of this variant, relative to the
    /// template root; `None` inherits the template's `first_boot`.
    pub first_boot: Option<PathBuf>,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub display: Option<String>,
    pub cpu_model: Option<String>,
    pub accel: Option<AccelMode>,
    pub firmware: Option<Firmware>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    /// Replaces the template's `qemu_args` for this variant's build boot.
    pub qemu_args: Option<Vec<String>>,
    pub media: Vec<Media>,
    pub provisions: Vec<Provision>,
    pub playbooks: Vec<Playbook>,
    /// Replaces the template's NICs for this variant's build boot when any
    /// are declared.
    pub nics: Vec<Nic>,
}

#[derive(Debug, Clone)]
pub enum TemplateSource {
    Iso(ArtefactSource),
    Qcow2(ArtefactSource),
    /// Layered build from an existing template.
    Template {
        from: TemplateRef,
        span: Span,
    },
    Scratch {
        span: Span,
    },
}

/// Local path or URL+hash artefact (§6.1).
#[derive(Debug, Clone)]
pub enum ArtefactSource {
    Path {
        path: PathBuf,
        span: Span,
    },
    Url {
        url: String,
        sha256: String,
        span: Span,
    },
}

/// Known event names bindable with `on` (§8.1).
pub const EVENT_NAMES: &[&str] = &[
    "vm.starting",
    "vm.ready",
    "vm.stopped",
    "vm.crashed",
    "vm.suspended",
    "vm.woken",
    "container.starting",
    "container.ready",
    "container.stopped",
    "container.crashed",
    "container.unhealthy",
    "lab.up",
    "lab.down",
    "lab.daemon_crashed",
    "snapshot.created",
    "snapshot.restored",
    "template.built",
    "playbook.applied",
    "playbook.failed",
    "host.disk_low",
];

/// Architectures with a `qemu-system-<arch>` emulator vmlab will drive.
pub const KNOWN_ARCHES: &[&str] = &[
    "x86_64",
    // Display-only alias for 32-bit x86 guests; runs on the x86_64 emulator.
    "x86",
    "aarch64",
    "riscv64",
    "loongarch64",
    "s390x",
    "ppc64",
];

/// Parse a `cpu_model =` value (§5.2): the QEMU `-cpu` model a machine runs
/// as, e.g. `pentium3`. Any name is accepted — QEMU owns the catalogue and
/// refuses one it does not know at start — but a blank one would hand QEMU
/// an empty `-cpu`, so it is refused here, wherever the field is declared.
/// What a machine's guest clock does (§7.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Clock {
    /// Runs from the host's time, and is set to it again after an online
    /// snapshot restore, which would otherwise resume it at capture time.
    #[default]
    Host,
    /// Runs from the host's time, but a restored guest keeps the time it was
    /// captured at.
    Snapshot,
    /// Starts at this instant on every boot and is never set by vmlab.
    Fixed(chrono::DateTime<chrono::Utc>),
}

impl Clock {
    /// Whether an online restore sets the guest clock to the host's time.
    pub fn syncs_on_restore(self) -> bool {
        self == Clock::Host
    }

    /// QEMU `-rtc base=` for a fixed clock: UTC, without a zone suffix.
    pub fn rtc_base(self) -> Option<String> {
        match self {
            Clock::Fixed(at) => Some(at.format("%Y-%m-%dT%H:%M:%S").to_string()),
            Clock::Host | Clock::Snapshot => None,
        }
    }
}

/// Parse a `clock =` value: `host`, `snapshot`, or an RFC 3339 date-time.
pub fn parse_clock(s: &str) -> Result<Clock, String> {
    match s {
        "host" => Ok(Clock::Host),
        "snapshot" => Ok(Clock::Snapshot),
        _ => chrono::DateTime::parse_from_rfc3339(s)
            .map(|at| Clock::Fixed(at.with_timezone(&chrono::Utc)))
            .map_err(|_| {
                format!(
                    "`clock` must be `host`, `snapshot`, or an RFC 3339 date-time such as \
                     `2020-01-01T00:00:00Z`, not `{s}`"
                )
            }),
    }
}

pub fn parse_cpu_model(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        return Err(
            "`cpu_model` must name a QEMU CPU model (e.g. \"pentium3\"), not be empty".to_string(),
        );
    }
    Ok(s.to_string())
}

/// Parse a `template =` value (PRD §6.2/§6.4/§6.5).
pub fn parse_template_ref(s: &str) -> Result<TemplateRef, String> {
    if s == "scratch" {
        return Ok(TemplateRef::Scratch);
    }
    let whole = s;
    let (s, variant) = split_variant(s)?;
    // Registry references contain a registry host (dot or :port before the
    // first slash) — e.g. ghcr.io/owner/name:1.2.
    if let Some((first, _)) = s.split_once('/')
        && (first.contains('.') || first.contains(':') || first == "localhost")
    {
        return Ok(TemplateRef::Registry {
            reference: s.to_string(),
            variant,
        });
    }
    let Some((arch, rest)) = s.split_once('/') else {
        return Err(format!(
            "malformed template reference `{whole}`: expected \
             `<arch>/<name>[@<version>][#<variant>]` — arch is always explicit (PRD §6.2)"
        ));
    };
    if arch.is_empty() || rest.is_empty() {
        return Err(format!("malformed template reference `{whole}`"));
    }
    if !KNOWN_ARCHES.contains(&arch) {
        return Err(format!(
            "unknown arch `{arch}` in template reference `{whole}` (known: {})",
            KNOWN_ARCHES.join(", ")
        ));
    }
    let (name, version) = match rest.split_once('@') {
        Some((n, v)) => {
            if v.is_empty() {
                return Err(format!(
                    "malformed template reference `{whole}`: empty version"
                ));
            }
            (n, Some(v.to_string()))
        }
        None => (rest, None),
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!("malformed template reference `{whole}`"));
    }
    Ok(TemplateRef::Store {
        arch: arch.to_string(),
        name: name.to_string(),
        version,
        variant,
    })
}

/// Parse an `image =` value: `[host/]repo[:tag][@sha256:<hex>]`, docker
/// shorthand allowed (`nginx`, `owner/app:v1`). The reference is kept as
/// written — the OCI image layer normalises shorthand (docker.io, `latest`)
/// at pull time; this only rejects syntax that can never resolve.
pub fn parse_image_ref(s: &str) -> Result<ImageRef, String> {
    if s.is_empty() {
        return Err("empty image reference".to_string());
    }
    if s.chars().any(char::is_whitespace) {
        return Err(format!(
            "malformed image reference `{s}`: contains whitespace"
        ));
    }
    let (rest, digest) = match s.split_once('@') {
        Some((r, d)) => (r, Some(d)),
        None => (s, None),
    };
    if let Some(d) = digest {
        let ok = d
            .strip_prefix("sha256:")
            .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
        if !ok {
            return Err(format!(
                "malformed digest in image reference `{s}` (expected `@sha256:<64 hex chars>`)"
            ));
        }
    }
    if rest.is_empty() {
        return Err(format!(
            "malformed image reference `{s}`: missing repository"
        ));
    }
    // A registry host is a first path segment with a dot or :port (or
    // `localhost`) — the same detection rule as template references.
    let path = match rest.split_once('/') {
        Some((first, p)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            let host_ok = !first.is_empty()
                && first
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'));
            if !host_ok {
                return Err(format!(
                    "malformed registry host `{first}` in image reference `{s}`"
                ));
            }
            p
        }
        _ => rest,
    };
    let (repo, tag) = match path.rsplit_once(':') {
        Some((r, t)) => (r, Some(t)),
        None => (path, None),
    };
    if repo.is_empty() {
        return Err(format!(
            "malformed image reference `{s}`: missing repository"
        ));
    }
    for part in repo.split('/') {
        let ok = !part.is_empty()
            && part.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if !ok {
            return Err(format!(
                "malformed image reference `{s}`: repository names are lowercase \
                 (letters, digits, `.`, `_`, `-`)"
            ));
        }
    }
    if let Some(t) = tag {
        let ok = !t.is_empty()
            && t.len() <= 128
            && !t.starts_with(['.', '-'])
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !ok {
            return Err(format!("malformed tag `{t}` in image reference `{s}`"));
        }
    }
    Ok(ImageRef {
        reference: s.to_string(),
    })
}

/// Parse `ip[:port]`.
pub fn parse_host_port(s: &str) -> Result<HostPort, String> {
    let (ip_s, port) = match s.rsplit_once(':') {
        Some((ip, p)) => {
            let port: u16 = p.parse().map_err(|_| format!("malformed port in `{s}`"))?;
            (ip, Some(port))
        }
        None => (s, None),
    };
    let ip: Ipv4Addr = ip_s.parse().map_err(|_| format!("malformed IP in `{s}`"))?;
    Ok(HostPort { ip, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_refs() {
        assert_eq!(parse_template_ref("scratch").unwrap(), TemplateRef::Scratch);
        assert_eq!(
            parse_template_ref("x86_64/windows-11@26100.1").unwrap(),
            TemplateRef::Store {
                arch: "x86_64".into(),
                name: "windows-11".into(),
                version: Some("26100.1".into()),
                variant: None,
            }
        );
        assert_eq!(
            parse_template_ref("aarch64/linux-router").unwrap(),
            TemplateRef::Store {
                arch: "aarch64".into(),
                name: "linux-router".into(),
                version: None,
                variant: None,
            }
        );
        assert!(matches!(
            parse_template_ref("ghcr.io/wil/win11:26100.1").unwrap(),
            TemplateRef::Registry { .. }
        ));
        // Archless references are malformed (§6.2).
        assert!(parse_template_ref("windows-11").is_err());
        assert!(parse_template_ref("bogusarch/win").is_err());
        assert!(parse_template_ref("x86_64/win@").is_err());
    }

    /// `#<variant>` on store and registry refs (§6.6): `#base` is its own
    /// selector, a bare `#` and a malformed name are refused, and display
    /// round-trips the text.
    #[test]
    fn template_refs_with_variants() {
        let store = |s: &str| match parse_template_ref(s).unwrap() {
            TemplateRef::Store {
                version, variant, ..
            } => (version, variant),
            other => panic!("{s}: not a store ref: {other:?}"),
        };
        assert_eq!(
            store("x86_64/windows-11#win3d"),
            (None, Some(VariantRef::Named("win3d".into())))
        );
        assert_eq!(
            store("x86_64/windows-11@26100.1#standard"),
            (
                Some("26100.1".into()),
                Some(VariantRef::Named("standard".into()))
            )
        );
        assert_eq!(
            store("x86_64/windows-11#base"),
            (None, Some(VariantRef::Base))
        );
        assert_eq!(store("x86_64/windows-11"), (None, None));
        // The grammar's edges: one char, 32 chars, digits first, hyphens.
        assert!(parse_template_ref("x86_64/w#a").is_ok());
        assert!(parse_template_ref(&format!("x86_64/w#{}", "a".repeat(32))).is_ok());
        assert!(parse_template_ref("x86_64/w#3d-gpu").is_ok());

        for bad in [
            "x86_64/w#",
            "x86_64/w@1#",
            "x86_64/w@#v",
            "x86_64/#v",
            "x86_64/w#Win3d",
            "x86_64/w#-lead",
            "x86_64/w#under_score",
            "x86_64/w#a#b",
            "x86_64/w#has.dot",
            "ghcr.io/o/w:1#",
            "ghcr.io/o/w:1#BAD",
        ] {
            assert!(parse_template_ref(bad).is_err(), "expected `{bad}` to fail");
        }
        let long = format!("x86_64/w#{}", "a".repeat(33));
        assert!(parse_template_ref(&long).is_err(), "33 chars");
        let err = parse_template_ref("x86_64/w#").unwrap_err();
        assert!(err.contains("empty variant"), "{err}");

        // Registry plus variant: the reference stays bare, the variant apart.
        assert_eq!(
            parse_template_ref("ghcr.io/o/win11:26100.1#win3d").unwrap(),
            TemplateRef::Registry {
                reference: "ghcr.io/o/win11:26100.1".into(),
                variant: Some(VariantRef::Named("win3d".into())),
            }
        );
        assert_eq!(
            parse_template_ref("ghcr.io/o/win11:1#base").unwrap(),
            TemplateRef::Registry {
                reference: "ghcr.io/o/win11:1".into(),
                variant: Some(VariantRef::Base),
            }
        );

        for text in [
            "x86_64/windows-11",
            "x86_64/windows-11#win3d",
            "x86_64/windows-11@26100.1#standard",
            "x86_64/windows-11#base",
            "ghcr.io/o/win11:26100.1#win3d",
            "localhost:5000/o/w@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef#extra",
        ] {
            assert_eq!(parse_template_ref(text).unwrap().to_string(), text);
        }
    }

    /// A variant *name* follows the ref grammar and may not be `base`.
    #[test]
    fn variant_names() {
        assert!(check_variant_name("standard").is_ok());
        assert!(check_variant_name("win3d").is_ok());
        let err = check_variant_name("base").unwrap_err();
        assert!(err.contains("reserved"), "{err}");
        assert!(check_variant_name("").is_err());
        assert!(check_variant_name("Standard").is_err());
        assert!(check_variant_name(&"a".repeat(33)).is_err());
    }

    #[test]
    fn image_refs() {
        for ok in [
            "nginx",
            "nginx:1.27",
            "owner/app:v1",
            "ghcr.io/owner/app:2.0",
            "localhost:5000/dev/img",
            "registry.example.com:8443/a/b/c:latest",
            "alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "ghcr.io/o/a:v1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert!(parse_image_ref(ok).is_ok(), "expected `{ok}` to parse");
        }
        for bad in [
            "",
            "has space",
            "UPPER/case",
            "nginx:",
            "nginx:.badtag",
            "img@sha256:short",
            "img@md5:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "ghcr.io/",
        ] {
            assert!(parse_image_ref(bad).is_err(), "expected `{bad}` to fail");
        }
    }

    #[test]
    fn macs() {
        let m: MacAddr = "52:54:00:ab:cd:ef".parse().unwrap();
        assert_eq!(m.to_string(), "52:54:00:ab:cd:ef");
        assert!("52:54:00".parse::<MacAddr>().is_err());
    }

    #[test]
    fn host_ports() {
        let hp = parse_host_port("10.0.0.1:80").unwrap();
        assert_eq!(hp.port, Some(80));
        assert_eq!(parse_host_port("10.0.0.1").unwrap().port, None);
        assert!(parse_host_port("nope:80").is_err());
    }

    /// `clock` is `host`, `snapshot`, or an RFC 3339 instant, which is kept
    /// in UTC and given to QEMU without a zone (§7.3).
    #[test]
    fn clocks() {
        assert_eq!(parse_clock("host").unwrap(), Clock::Host);
        assert_eq!(parse_clock("snapshot").unwrap(), Clock::Snapshot);
        let fixed = parse_clock("2020-01-01T10:00:00+10:00").unwrap();
        assert_eq!(fixed.rtc_base().as_deref(), Some("2020-01-01T00:00:00"));
        assert!(!fixed.syncs_on_restore());
        assert!(Clock::Host.syncs_on_restore());
        assert!(!Clock::Snapshot.syncs_on_restore());
        assert_eq!(Clock::Host.rtc_base(), None);
        assert_eq!(Clock::default(), Clock::Host);
        let err = parse_clock("2020-01-01").unwrap_err();
        assert!(err.contains("RFC 3339"), "{err}");
        assert!(parse_clock("Host").is_err());
    }
}
