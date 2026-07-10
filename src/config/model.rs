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
    pub segments: Vec<Segment>,
    pub vms: Vec<Vm>,
    pub containers: Vec<Container>,
    pub provisions: Vec<Provision>,
    pub handlers: Vec<Handler>,
    pub records: Vec<DnsRecord>,
    pub sinkholes: Vec<SinkholeRule>,
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
    /// `<arch>/<name>[@<version>]` — local store.
    Store {
        arch: String,
        name: String,
        version: Option<String>,
    },
    /// OCI registry reference, e.g. `ghcr.io/owner/name:version`; arch comes
    /// from the VM's explicit `arch` attribute (§6.4).
    Registry { reference: String },
}

impl std::fmt::Display for TemplateRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateRef::Scratch => write!(f, "scratch"),
            TemplateRef::Store {
                arch,
                name,
                version,
            } => match version {
                Some(v) => write!(f, "{arch}/{name}@{v}"),
                None => write!(f, "{arch}/{name}"),
            },
            TemplateRef::Registry { reference } => write!(f, "{reference}"),
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
    pub display: Option<String>,
    pub firmware: Option<Firmware>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    pub qemu_args: Vec<String>,
    pub gpu: Option<Gpu>,
    pub nics: Vec<Nic>,
    pub extra_disks: Vec<DiskBlock>,
    pub shares: Vec<Share>,
    pub media: Vec<Media>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    Ovmf,
    Seabios,
}

#[derive(Debug, Clone)]
pub struct Nic {
    pub span: Span,
    pub segment: Option<String>,
    pub nat: bool,
    pub ip: Option<Ipv4Addr>,
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

#[derive(Debug, Clone)]
pub struct Share {
    pub span: Span,
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
    pub smb1: bool,
    pub name: String,
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

#[derive(Debug, Clone)]
pub struct Gpu {
    pub mode: GpuMode,
    pub address: Option<String>,
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
    /// Entrypoint override (exec form); `None` = image default.
    pub entrypoint: Option<Vec<String>>,
    /// Cmd override (exec form); `None` = image default.
    pub command: Option<Vec<String>>,
    pub workdir: Option<String>,
    /// `uid[:gid]` or a user/group name resolved against the image.
    pub user: Option<String>,
    /// Micro-VM vCPUs; default 1 at runtime.
    pub cpus: Option<u32>,
    /// Micro-VM RAM in bytes; default 256 MiB at runtime.
    pub memory: Option<u64>,
    /// VM or container names — the namespaces are unified.
    pub depends_on: Vec<String>,
    pub restart: RestartPolicy,
    pub nics: Vec<Nic>,
    pub env: Vec<EnvVar>,
    pub volumes: Vec<Volume>,
    pub ports: Vec<PortMap>,
    pub healthcheck: Option<Healthcheck>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    No,
    OnFailure,
    Always,
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

#[derive(Debug, Clone)]
pub struct Provision {
    pub script: PathBuf,
    pub vms: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Handler {
    pub event: String,
    pub run: PathBuf,
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
    pub source: TemplateSource,
    pub media: Vec<Media>,
    pub provisions: Vec<Provision>,
    pub nics: Vec<Nic>,
    pub extra_disks: Vec<DiskBlock>,
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

/// Parse a `template =` value (PRD §6.2/§6.4/§6.5).
pub fn parse_template_ref(s: &str) -> Result<TemplateRef, String> {
    if s == "scratch" {
        return Ok(TemplateRef::Scratch);
    }
    // Registry references contain a registry host (dot or :port before the
    // first slash) — e.g. ghcr.io/owner/name:1.2.
    if let Some((first, _)) = s.split_once('/')
        && (first.contains('.') || first.contains(':') || first == "localhost")
    {
        return Ok(TemplateRef::Registry {
            reference: s.to_string(),
        });
    }
    let Some((arch, rest)) = s.split_once('/') else {
        return Err(format!(
            "malformed template reference `{s}`: expected `<arch>/<name>[@<version>]` — arch is \
             always explicit (PRD §6.2)"
        ));
    };
    if arch.is_empty() || rest.is_empty() {
        return Err(format!("malformed template reference `{s}`"));
    }
    if !KNOWN_ARCHES.contains(&arch) {
        return Err(format!(
            "unknown arch `{arch}` in template reference `{s}` (known: {})",
            KNOWN_ARCHES.join(", ")
        ));
    }
    let (name, version) = match rest.split_once('@') {
        Some((n, v)) => {
            if v.is_empty() {
                return Err(format!("malformed template reference `{s}`: empty version"));
            }
            (n, Some(v.to_string()))
        }
        None => (rest, None),
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!("malformed template reference `{s}`"));
    }
    Ok(TemplateRef::Store {
        arch: arch.to_string(),
        name: name.to_string(),
        version,
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
                version: Some("26100.1".into())
            }
        );
        assert_eq!(
            parse_template_ref("aarch64/linux-router").unwrap(),
            TemplateRef::Store {
                arch: "aarch64".into(),
                name: "linux-router".into(),
                version: None
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
}
