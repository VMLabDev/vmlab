//! JSON DTOs for the parsed lab model, consumed by the web visual editor.
//! Every block-backed entity carries its source byte span (`[start, end]`) —
//! that span is the block's address for [`super::edit_ops`] operations
//! against the same source revision.

use serde::Serialize;

use super::model::{
    BlockRule, Connect, DiskBlock, DnsRecord, Firmware, Forward, Gpu, GpuMode, Handler, HostPort,
    L4Proto, Lab, LabFile, Media, MediaKind, Nic, Provision, RedirectRule, Route, Segment,
    SegmentDns, Share, SinkholeMode, SinkholeRule, Span, TemplateDef, Vm,
};

#[derive(Serialize)]
pub struct LabModelDto {
    pub lab: LabDto,
    /// `template {}` blocks living in the same file — surfaced so the editor
    /// knows they exist (it never edits them).
    pub templates: Vec<TemplateSummaryDto>,
}

impl From<&LabFile> for LabModelDto {
    fn from(f: &LabFile) -> Self {
        Self {
            lab: LabDto::from(&f.lab),
            templates: f.templates.iter().map(TemplateSummaryDto::from).collect(),
        }
    }
}

#[derive(Serialize)]
pub struct LabDto {
    pub name: String,
    pub span: Span,
    pub gui: Option<bool>,
    pub segments: Vec<SegmentDto>,
    pub vms: Vec<VmDto>,
    pub provisions: Vec<ProvisionDto>,
    pub handlers: Vec<HandlerDto>,
    pub records: Vec<DnsRecordDto>,
    pub sinkholes: Vec<SinkholeDto>,
}

impl From<&Lab> for LabDto {
    fn from(l: &Lab) -> Self {
        Self {
            name: l.name.clone(),
            span: l.span,
            gui: l.gui,
            segments: l.segments.iter().map(SegmentDto::from).collect(),
            vms: l.vms.iter().map(VmDto::from).collect(),
            provisions: l.provisions.iter().map(ProvisionDto::from).collect(),
            handlers: l.handlers.iter().map(HandlerDto::from).collect(),
            records: l.records.iter().map(DnsRecordDto::from).collect(),
            sinkholes: l.sinkholes.iter().map(SinkholeDto::from).collect(),
        }
    }
}

#[derive(Serialize)]
pub struct SegmentDto {
    pub name: String,
    pub span: Span,
    pub subnet: Option<String>,
    pub global: bool,
    pub dhcp: bool,
    pub nat: bool,
    pub mtu: Option<u16>,
    pub routes_to: Vec<String>,
    pub dns: SegmentDnsDto,
    pub connect: Option<ConnectDto>,
    pub routes: Vec<RouteDto>,
    pub records: Vec<DnsRecordDto>,
    pub forwards: Vec<ForwardDto>,
    pub block_rules: Vec<BlockRuleDto>,
    pub redirect_rules: Vec<RedirectRuleDto>,
    pub sinkholes: Vec<SinkholeDto>,
}

impl From<&Segment> for SegmentDto {
    fn from(s: &Segment) -> Self {
        Self {
            name: s.name.clone(),
            span: s.span,
            subnet: s.subnet.map(|n| n.to_string()),
            global: s.global,
            dhcp: s.dhcp,
            nat: s.nat,
            mtu: s.mtu,
            routes_to: s.routes_to.clone(),
            dns: SegmentDnsDto::from(&s.dns),
            connect: s.connect.as_ref().map(ConnectDto::from),
            routes: s.routes.iter().map(RouteDto::from).collect(),
            records: s.records.iter().map(DnsRecordDto::from).collect(),
            forwards: s.forwards.iter().map(ForwardDto::from).collect(),
            block_rules: s.block_rules.iter().map(BlockRuleDto::from).collect(),
            redirect_rules: s.redirect_rules.iter().map(RedirectRuleDto::from).collect(),
            sinkholes: s.sinkholes.iter().map(SinkholeDto::from).collect(),
        }
    }
}

#[derive(Serialize)]
pub struct SegmentDnsDto {
    /// Whether a `dns {}` block is present in the source at all.
    pub declared: bool,
    /// Block address when declared.
    pub span: Option<Span>,
    pub server: Option<String>,
    pub enabled: bool,
}

impl From<&SegmentDns> for SegmentDnsDto {
    fn from(d: &SegmentDns) -> Self {
        Self {
            declared: d.declared,
            span: d.span,
            server: d.server.map(|ip| ip.to_string()),
            enabled: d.enabled,
        }
    }
}

#[derive(Serialize)]
pub struct ConnectDto {
    pub host: String,
    pub span: Span,
}

impl From<&Connect> for ConnectDto {
    fn from(c: &Connect) -> Self {
        Self {
            host: c.host.clone(),
            span: c.span,
        }
    }
}

#[derive(Serialize)]
pub struct RouteDto {
    pub dest: String,
    pub via: String,
    pub span: Span,
}

impl From<&Route> for RouteDto {
    fn from(r: &Route) -> Self {
        Self {
            dest: r.dest.to_string(),
            via: r.via.to_string(),
            span: r.span,
        }
    }
}

#[derive(Serialize)]
pub struct DnsRecordDto {
    pub name: String,
    pub ip: String,
    pub span: Span,
}

impl From<&DnsRecord> for DnsRecordDto {
    fn from(r: &DnsRecord) -> Self {
        Self {
            name: r.name.clone(),
            ip: r.ip.to_string(),
            span: r.span,
        }
    }
}

/// NOTE: the schema writes forwards as `to = "vm:port"`; the model (and this
/// DTO) splits that into `vm` + `guest_port`. Edits must send the schema
/// form back (`set_field to "vm:port"`).
#[derive(Serialize)]
pub struct ForwardDto {
    pub host_port: u16,
    pub vm: String,
    pub guest_port: u16,
    pub proto: super::model::Proto,
    pub span: Span,
}

impl From<&Forward> for ForwardDto {
    fn from(f: &Forward) -> Self {
        Self {
            host_port: f.host_port,
            vm: f.vm.clone(),
            guest_port: f.guest_port,
            proto: f.proto,
            span: f.span,
        }
    }
}

#[derive(Serialize)]
pub struct BlockRuleDto {
    pub cidr: String,
    pub proto: Option<L4Proto>,
    pub port: Option<u16>,
    pub span: Span,
}

impl From<&BlockRule> for BlockRuleDto {
    fn from(b: &BlockRule) -> Self {
        Self {
            cidr: b.cidr.to_string(),
            proto: b.proto,
            port: b.port,
            span: b.span,
        }
    }
}

fn host_port_string(hp: &HostPort) -> String {
    match hp.port {
        Some(p) => format!("{}:{p}", hp.ip),
        None => hp.ip.to_string(),
    }
}

#[derive(Serialize)]
pub struct RedirectRuleDto {
    pub from: String,
    pub to: String,
    pub proto: Option<L4Proto>,
    pub span: Span,
}

impl From<&RedirectRule> for RedirectRuleDto {
    fn from(r: &RedirectRule) -> Self {
        Self {
            from: host_port_string(&r.from),
            to: host_port_string(&r.to),
            proto: r.proto,
            span: r.span,
        }
    }
}

#[derive(Serialize)]
pub struct SinkholeDto {
    pub pattern: String,
    pub mode: SinkholeMode,
    pub span: Span,
}

impl From<&SinkholeRule> for SinkholeDto {
    fn from(s: &SinkholeRule) -> Self {
        Self {
            pattern: s.pattern.clone(),
            mode: s.mode,
            span: s.span,
        }
    }
}

#[derive(Serialize)]
pub struct VmDto {
    pub name: String,
    pub span: Span,
    /// Rendered template reference (`x86_64/name@ver`, `scratch`, or an OCI
    /// reference) — exactly what `template = "…"` accepts.
    pub template: String,
    pub template_span: Span,
    pub arch: Option<String>,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    /// Bytes.
    pub memory: Option<u64>,
    /// Bytes (scratch VMs only).
    pub disk: Option<u64>,
    pub cdrom: Option<String>,
    pub floppy: Option<String>,
    pub depends_on: Vec<String>,
    pub nested: bool,
    pub gui: Option<bool>,
    pub display: Option<String>,
    pub firmware: Option<Firmware>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    pub qemu_args: Vec<String>,
    pub gpu: Option<GpuDto>,
    pub nics: Vec<NicDto>,
    pub extra_disks: Vec<DiskDto>,
    pub shares: Vec<ShareDto>,
    pub media: Vec<MediaDto>,
}

impl From<&Vm> for VmDto {
    fn from(v: &Vm) -> Self {
        Self {
            name: v.name.clone(),
            span: v.span,
            template: v.template.to_string(),
            template_span: v.template_span,
            arch: v.arch.clone(),
            profile: v.profile.clone(),
            cpus: v.cpus,
            memory: v.memory,
            disk: v.disk,
            cdrom: v.cdrom.as_ref().map(|p| p.display().to_string()),
            floppy: v.floppy.as_ref().map(|p| p.display().to_string()),
            depends_on: v.depends_on.clone(),
            nested: v.nested,
            gui: v.gui,
            display: v.display.clone(),
            firmware: v.firmware,
            tpm: v.tpm,
            secure_boot: v.secure_boot,
            qemu_args: v.qemu_args.clone(),
            gpu: v.gpu.as_ref().map(GpuDto::from),
            nics: v.nics.iter().map(NicDto::from).collect(),
            extra_disks: v.extra_disks.iter().map(DiskDto::from).collect(),
            shares: v.shares.iter().map(ShareDto::from).collect(),
            media: v.media.iter().map(MediaDto::from).collect(),
        }
    }
}

#[derive(Serialize)]
pub struct GpuDto {
    pub mode: GpuMode,
    pub address: Option<String>,
    pub span: Span,
}

impl From<&Gpu> for GpuDto {
    fn from(g: &Gpu) -> Self {
        Self {
            mode: g.mode,
            address: g.address.clone(),
            span: g.span,
        }
    }
}

#[derive(Serialize)]
pub struct NicDto {
    pub span: Span,
    pub segment: Option<String>,
    pub nat: bool,
    pub ip: Option<String>,
    pub mac: Option<String>,
    pub isolated: bool,
}

impl From<&Nic> for NicDto {
    fn from(n: &Nic) -> Self {
        Self {
            span: n.span,
            segment: n.segment.clone(),
            nat: n.nat,
            ip: n.ip.map(|ip| ip.to_string()),
            mac: n.mac.map(|m| m.to_string()),
            isolated: n.isolated,
        }
    }
}

#[derive(Serialize)]
pub struct DiskDto {
    pub name: String,
    pub span: Span,
    /// Bytes.
    pub size: Option<u64>,
    pub from: Option<String>,
}

impl From<&DiskBlock> for DiskDto {
    fn from(d: &DiskBlock) -> Self {
        Self {
            name: d.name.clone(),
            span: d.span,
            size: d.size,
            from: d.from.as_ref().map(|p| p.display().to_string()),
        }
    }
}

#[derive(Serialize)]
pub struct ShareDto {
    pub span: Span,
    pub host: String,
    pub guest: String,
    pub readonly: bool,
    pub smb1: bool,
    /// Share name — derived from the guest path when not declared.
    pub name: String,
}

impl From<&Share> for ShareDto {
    fn from(s: &Share) -> Self {
        Self {
            span: s.span,
            host: s.host.display().to_string(),
            guest: s.guest.clone(),
            readonly: s.readonly,
            smb1: s.smb1,
            name: s.name.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct MediaDto {
    pub span: Span,
    pub kind: MediaKind,
    pub from: String,
    pub label: Option<String>,
}

impl From<&Media> for MediaDto {
    fn from(m: &Media) -> Self {
        Self {
            span: m.span,
            kind: m.kind,
            from: m.from.display().to_string(),
            label: m.label.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct ProvisionDto {
    pub script: String,
    pub vms: Vec<String>,
    pub span: Span,
}

impl From<&Provision> for ProvisionDto {
    fn from(p: &Provision) -> Self {
        Self {
            script: p.script.display().to_string(),
            vms: p.vms.clone(),
            span: p.span,
        }
    }
}

#[derive(Serialize)]
pub struct HandlerDto {
    pub event: String,
    pub run: String,
    pub span: Span,
}

impl From<&Handler> for HandlerDto {
    fn from(h: &Handler) -> Self {
        Self {
            event: h.event.clone(),
            run: h.run.display().to_string(),
            span: h.span,
        }
    }
}

#[derive(Serialize)]
pub struct TemplateSummaryDto {
    pub name: String,
    pub span: Span,
    pub arch: String,
    pub version: String,
}

impl From<&TemplateDef> for TemplateSummaryDto {
    fn from(t: &TemplateDef) -> Self {
        Self {
            name: t.name.clone(),
            span: t.span,
            arch: t.arch.clone(),
            version: t.version.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::load_lab_source;

    const SRC: &str = r#"import <vmlab.wcl>

lab "dto-lab" {
  segment "corp" {
    subnet = "10.9.0.0/24"
    nat    = true
    dns { server = "10.9.0.53" }
    forward { host_port = 8080 to = "web01:80" }
    redirect { from = "1.2.3.4:443" to = "10.9.0.5:8443" }
    block { cidr = "0.0.0.0/0" proto = "udp" }
  }

  vm "web01" {
    template = "x86_64/linux-modern@1.0"
    memory   = 2GiB
    firmware = "ovmf"
    nic { segment = "corp" ip = "10.9.0.5" mac = "52:54:00:aa:bb:cc" }
    disk "data" { size = 10GiB }
    share { host = "./files" guest = "C:/files" }
    media { kind = "iso" from = "./extra/" }
  }

  provision "scripts/setup.ws" { vms = ["web01"] }
  on "vm.crashed" { run = "scripts/dump.ws" }
}
"#;

    #[test]
    fn dto_serializes_the_full_tree() {
        let lf = load_lab_source(SRC, "<test>", Path::new("/tmp")).unwrap();
        let dto = LabModelDto::from(&lf);
        let v = serde_json::to_value(&dto).unwrap();

        assert_eq!(v["lab"]["name"], "dto-lab");
        // Spans are [start, end] pairs on every block.
        assert!(v["lab"]["span"][1].as_u64().unwrap() > 0);

        let seg = &v["lab"]["segments"][0];
        assert_eq!(seg["subnet"], "10.9.0.0/24");
        assert_eq!(seg["nat"], true);
        assert_eq!(seg["dns"]["declared"], true);
        assert_eq!(seg["dns"]["server"], "10.9.0.53");
        assert_eq!(seg["forwards"][0]["vm"], "web01");
        assert_eq!(seg["forwards"][0]["guest_port"], 80);
        assert_eq!(seg["redirect_rules"][0]["from"], "1.2.3.4:443");
        assert_eq!(seg["block_rules"][0]["proto"], "udp");

        let vm = &v["lab"]["vms"][0];
        assert_eq!(vm["template"], "x86_64/linux-modern@1.0");
        assert_eq!(vm["memory"].as_u64(), Some(2 << 30));
        assert_eq!(vm["firmware"], "ovmf");
        assert_eq!(vm["nics"][0]["mac"], "52:54:00:aa:bb:cc");
        assert_eq!(vm["extra_disks"][0]["name"], "data");
        assert_eq!(vm["extra_disks"][0]["size"].as_u64(), Some(10 << 30));
        assert_eq!(vm["shares"][0]["guest"], "C:/files");
        assert_eq!(vm["media"][0]["kind"], "iso");

        assert_eq!(v["lab"]["provisions"][0]["vms"][0], "web01");
        assert_eq!(v["lab"]["handlers"][0]["event"], "vm.crashed");
    }
}
