//! Walk a parsed WCL document into the typed model. Structural legality
//! (unknown fields, wrong types) is the schema's job; here we convert values
//! and report anything the schema cannot express as positioned issues.

use std::path::{Path, PathBuf};

use wcl_lang::{Block, Document, Value};

use super::model::*;
use super::{Issue, IssueList};

pub fn extract_lab_file(doc: &Document, root: &Path, issues: &mut IssueList) -> Option<LabFile> {
    let mut labs = Vec::new();
    let mut templates = Vec::new();
    for block in doc.blocks() {
        match block.kind() {
            "lab" => {
                if let Some(lab) = extract_lab(&block, issues) {
                    labs.push(lab);
                }
            }
            "template" => {
                if let Some(t) = extract_template(&block, issues) {
                    templates.push(t);
                }
            }
            _ => {} // schema already rejected unknown kinds
        }
    }
    match labs.len() {
        0 => {
            issues.push(Issue::new("no `lab` block found in vmlab.wcl"));
            None
        }
        1 => Some(LabFile {
            root: root.to_path_buf(),
            lab: labs.remove(0),
            templates,
        }),
        _ => {
            issues.push(Issue::at(
                labs[1].span,
                "multiple `lab` blocks in one file — a lab file defines exactly one lab",
            ));
            None
        }
    }
}

/// Extract only template definitions (for dedicated template files).
pub fn extract_template_file(doc: &Document, issues: &mut IssueList) -> TemplateFile {
    let mut templates = Vec::new();
    for block in doc.blocks() {
        if block.kind() == "template"
            && let Some(t) = extract_template(&block, issues)
        {
            templates.push(t);
        }
    }
    TemplateFile { templates }
}

fn span_of(b: &Block) -> Span {
    let s = b.span();
    (s.start, s.end)
}

fn label_name(b: &Block, what: &str, issues: &mut IssueList) -> Option<String> {
    match b.labels() {
        Ok(labels) => match labels.first() {
            Some(Value::Utf8(s)) | Some(Value::Ascii(s)) | Some(Value::Identifier(s)) => {
                Some(s.clone())
            }
            _ => {
                issues.push(Issue::at(
                    span_of(b),
                    format!("{what} requires a name label"),
                ));
                None
            }
        },
        Err(e) => {
            issues.push(Issue::at(
                span_of(b),
                format!("cannot evaluate {what} label: {e}"),
            ));
            None
        }
    }
}

// ---- field readers ---------------------------------------------------------

fn raw_value(b: &Block, name: &str, issues: &mut IssueList) -> Option<(Value, Span)> {
    let field = b.field(name)?;
    let span = (field.span().start, field.span().end);
    match field.value() {
        Ok(Value::None) => None,
        Ok(v) => Some((v.clone(), span)),
        Err(e) => {
            issues.push(Issue::at(span, format!("cannot evaluate `{name}`: {e}")));
            None
        }
    }
}

fn get_str(b: &Block, name: &str, issues: &mut IssueList) -> Option<(String, Span)> {
    let (v, span) = raw_value(b, name, issues)?;
    match v {
        Value::Utf8(s) | Value::Ascii(s) | Value::Identifier(s) => Some((s, span)),
        other => {
            issues.push(Issue::at(
                span,
                format!("`{name}` must be a string, got {other:?}"),
            ));
            None
        }
    }
}

fn get_bool(b: &Block, name: &str, issues: &mut IssueList) -> Option<bool> {
    let (v, span) = raw_value(b, name, issues)?;
    match v {
        Value::Bool(x) => Some(x),
        other => {
            issues.push(Issue::at(
                span,
                format!("`{name}` must be a bool, got {other:?}"),
            ));
            None
        }
    }
}

fn get_int(b: &Block, name: &str, issues: &mut IssueList) -> Option<(i64, Span)> {
    let (v, span) = raw_value(b, name, issues)?;
    let n = match v {
        Value::I64(n) => Some(n),
        Value::I32(n) => Some(n as i64),
        Value::U32(n) => Some(n as i64),
        Value::U64(n) => i64::try_from(n).ok(),
        Value::I8(n) => Some(n as i64),
        Value::I16(n) => Some(n as i64),
        Value::U8(n) => Some(n as i64),
        Value::U16(n) => Some(n as i64),
        _ => None,
    };
    match n {
        Some(n) => Some((n, span)),
        None => {
            issues.push(Issue::at(span, format!("`{name}` must be an integer")));
            None
        }
    }
}

fn get_str_list(b: &Block, name: &str, issues: &mut IssueList) -> Vec<String> {
    let Some((v, span)) = raw_value(b, name, issues) else {
        return Vec::new();
    };
    match v {
        Value::List(items) => {
            let mut out = Vec::new();
            for item in items.iter() {
                match item {
                    Value::Utf8(s) | Value::Ascii(s) | Value::Identifier(s) => out.push(s.clone()),
                    other => issues.push(Issue::at(
                        span,
                        format!("`{name}` must be a list of strings, found {other:?}"),
                    )),
                }
            }
            out
        }
        other => {
            issues.push(Issue::at(
                span,
                format!("`{name}` must be a list, got {other:?}"),
            ));
            Vec::new()
        }
    }
}

/// Parse a string field through `parse`, reporting failures as issues.
fn get_parsed<T>(
    b: &Block,
    name: &str,
    issues: &mut IssueList,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Option<(T, Span)> {
    let (s, span) = get_str(b, name, issues)?;
    match parse(&s) {
        Ok(v) => Some((v, span)),
        Err(e) => {
            issues.push(Issue::at(span, e));
            None
        }
    }
}

/// Read a `std.ByteSize` field — a non-negative integer count of bytes
/// (unit suffixes like `8GiB` are resolved to bytes by wcl).
fn get_size(b: &Block, name: &str, issues: &mut IssueList) -> Option<u64> {
    let (n, span) = get_int(b, name, issues)?;
    match u64::try_from(n) {
        Ok(v) => Some(v),
        Err(_) => {
            issues.push(Issue::at(span, format!("`{name}` must not be negative")));
            None
        }
    }
}

fn get_path(b: &Block, name: &str, issues: &mut IssueList) -> Option<PathBuf> {
    get_str(b, name, issues).map(|(s, _)| PathBuf::from(s))
}

fn get_enum<T: Copy>(
    b: &Block,
    name: &str,
    table: &[(&str, T)],
    issues: &mut IssueList,
) -> Option<T> {
    let (s, span) = get_str(b, name, issues)?;
    match table.iter().find(|(k, _)| *k == s) {
        Some((_, v)) => Some(*v),
        None => {
            let allowed: Vec<&str> = table.iter().map(|(k, _)| *k).collect();
            issues.push(Issue::at(
                span,
                format!("`{name}` must be one of {}, got `{s}`", allowed.join(", ")),
            ));
            None
        }
    }
}

// ---- block extractors ------------------------------------------------------

fn extract_lab(b: &Block, issues: &mut IssueList) -> Option<Lab> {
    let name = label_name(b, "lab", issues)?;
    let mut lab = Lab {
        name,
        span: span_of(b),
        gui: get_bool(b, "gui", issues),
        segments: Vec::new(),
        vms: Vec::new(),
        provisions: Vec::new(),
        handlers: Vec::new(),
        records: Vec::new(),
        sinkholes: Vec::new(),
    };
    for child in b.blocks() {
        match child.kind() {
            "segment" => {
                if let Some(s) = extract_segment(&child, issues) {
                    lab.segments.push(s);
                }
            }
            "vm" => {
                if let Some(v) = extract_vm(&child, issues) {
                    lab.vms.push(v);
                }
            }
            "provision" => {
                if let Some(p) = extract_provision(&child, issues) {
                    lab.provisions.push(p);
                }
            }
            "on" => {
                if let Some(h) = extract_handler(&child, issues) {
                    lab.handlers.push(h);
                }
            }
            "record" => {
                if let Some(r) = extract_record(&child, issues) {
                    lab.records.push(r);
                }
            }
            "sinkhole" => {
                if let Some(s) = extract_sinkhole(&child, issues) {
                    lab.sinkholes.push(s);
                }
            }
            _ => {}
        }
    }
    Some(lab)
}

fn extract_segment(b: &Block, issues: &mut IssueList) -> Option<Segment> {
    let name = label_name(b, "segment", issues)?;
    let mut seg = Segment {
        name,
        span: span_of(b),
        subnet: get_parsed(b, "subnet", issues, |s| {
            s.parse::<ipnet::Ipv4Net>()
                .map_err(|_| format!("malformed subnet `{s}` (expected CIDR like 10.50.0.0/24)"))
        })
        .map(|(v, _)| v),
        global: get_bool(b, "global", issues).unwrap_or(false),
        dhcp: get_bool(b, "dhcp", issues).unwrap_or(true),
        nat: get_bool(b, "nat", issues).unwrap_or(false),
        mtu: get_int(b, "mtu", issues).and_then(|(n, span)| match u16::try_from(n) {
            Ok(m) if m >= 576 => Some(m),
            _ => {
                issues.push(Issue::at(
                    span,
                    format!("`mtu` must be between 576 and 65535, got {n}"),
                ));
                None
            }
        }),
        routes_to: get_str_list(b, "routes_to", issues),
        dns: SegmentDns {
            server: None,
            enabled: true,
            declared: false,
            span: None,
        },
        connect: None,
        routes: Vec::new(),
        records: Vec::new(),
        forwards: Vec::new(),
        block_rules: Vec::new(),
        redirect_rules: Vec::new(),
        sinkholes: Vec::new(),
    };
    for child in b.blocks() {
        match child.kind() {
            "dns" => {
                seg.dns = SegmentDns {
                    server: get_parsed(&child, "server", issues, |s| {
                        s.parse().map_err(|_| format!("malformed IP `{s}`"))
                    })
                    .map(|(v, _)| v),
                    enabled: get_bool(&child, "enabled", issues).unwrap_or(true),
                    declared: true,
                    span: Some(span_of(&child)),
                };
            }
            "connect" => {
                let host = get_str(&child, "host", issues);
                if let Some((host, _)) = host {
                    seg.connect = Some(Connect {
                        host,
                        span: span_of(&child),
                    });
                }
            }
            "route" => {
                let dest = get_parsed(&child, "dest", issues, |s| {
                    s.parse::<ipnet::Ipv4Net>()
                        .map_err(|_| format!("malformed CIDR `{s}`"))
                });
                let via = get_parsed(&child, "via", issues, |s| {
                    s.parse().map_err(|_| format!("malformed IP `{s}`"))
                });
                if let (Some((dest, _)), Some((via, _))) = (dest, via) {
                    seg.routes.push(Route {
                        dest,
                        via,
                        span: span_of(&child),
                    });
                }
            }
            "record" => {
                if let Some(r) = extract_record(&child, issues) {
                    seg.records.push(r);
                }
            }
            "forward" => {
                if let Some(f) = extract_forward(&child, issues) {
                    seg.forwards.push(f);
                }
            }
            "block" => {
                let cidr = get_parsed(&child, "cidr", issues, |s| {
                    s.parse::<ipnet::Ipv4Net>()
                        .map_err(|_| format!("malformed CIDR `{s}`"))
                });
                let proto = if child.field("proto").is_some() {
                    get_enum(
                        &child,
                        "proto",
                        &[
                            ("tcp", L4Proto::Tcp),
                            ("udp", L4Proto::Udp),
                            ("icmp", L4Proto::Icmp),
                        ],
                        issues,
                    )
                } else {
                    None
                };
                let port = get_int(&child, "port", issues).and_then(|(n, span)| {
                    u16::try_from(n).ok().or_else(|| {
                        issues.push(Issue::at(span, format!("port {n} out of range")));
                        None
                    })
                });
                if let Some((cidr, _)) = cidr {
                    seg.block_rules.push(BlockRule {
                        cidr,
                        proto,
                        port,
                        span: span_of(&child),
                    });
                }
            }
            "redirect" => {
                let from = get_parsed(&child, "from", issues, parse_host_port);
                let to = get_parsed(&child, "to", issues, parse_host_port);
                let proto = if child.field("proto").is_some() {
                    get_enum(
                        &child,
                        "proto",
                        &[("tcp", L4Proto::Tcp), ("udp", L4Proto::Udp)],
                        issues,
                    )
                } else {
                    None
                };
                if let (Some((from, _)), Some((to, _))) = (from, to) {
                    seg.redirect_rules.push(RedirectRule {
                        from,
                        to,
                        proto,
                        span: span_of(&child),
                    });
                }
            }
            "sinkhole" => {
                if let Some(s) = extract_sinkhole(&child, issues) {
                    seg.sinkholes.push(s);
                }
            }
            _ => {}
        }
    }
    Some(seg)
}

fn extract_record(b: &Block, issues: &mut IssueList) -> Option<DnsRecord> {
    let name = get_str(b, "name", issues);
    let ip = get_parsed(b, "ip", issues, |s| {
        s.parse().map_err(|_| format!("malformed IP `{s}`"))
    });
    match (name, ip) {
        (Some((name, _)), Some((ip, _))) => Some(DnsRecord {
            name,
            ip,
            span: span_of(b),
        }),
        _ => None,
    }
}

fn extract_sinkhole(b: &Block, issues: &mut IssueList) -> Option<SinkholeRule> {
    let pattern = get_str(b, "pattern", issues)?;
    let mode = if b.field("mode").is_some() {
        get_enum(
            b,
            "mode",
            &[
                ("nxdomain", SinkholeMode::Nxdomain),
                ("zero", SinkholeMode::Zero),
            ],
            issues,
        )
        .unwrap_or(SinkholeMode::Nxdomain)
    } else {
        SinkholeMode::Nxdomain
    };
    Some(SinkholeRule {
        pattern: pattern.0,
        mode,
        span: span_of(b),
    })
}

fn extract_forward(b: &Block, issues: &mut IssueList) -> Option<Forward> {
    let span = span_of(b);
    let host_port = get_int(b, "host_port", issues).and_then(|(n, s)| {
        u16::try_from(n).ok().or_else(|| {
            issues.push(Issue::at(s, format!("host_port {n} out of range")));
            None
        })
    })?;
    let (to, to_span) = get_str(b, "to", issues)?;
    let Some((vm, port_s)) = to.split_once(':') else {
        issues.push(Issue::at(
            to_span,
            format!("`to` must be \"vm:port\", got `{to}`"),
        ));
        return None;
    };
    let Ok(guest_port) = port_s.parse::<u16>() else {
        issues.push(Issue::at(
            to_span,
            format!("malformed guest port in `{to}`"),
        ));
        return None;
    };
    let proto = if b.field("proto").is_some() {
        get_enum(
            b,
            "proto",
            &[
                ("tcp", Proto::Tcp),
                ("udp", Proto::Udp),
                ("both", Proto::Both),
            ],
            issues,
        )
        .unwrap_or(Proto::Tcp)
    } else {
        Proto::Tcp
    };
    Some(Forward {
        host_port,
        vm: vm.to_string(),
        guest_port,
        proto,
        span,
    })
}

fn extract_nic(b: &Block, issues: &mut IssueList) -> Nic {
    Nic {
        span: span_of(b),
        segment: get_str(b, "segment", issues).map(|(s, _)| s),
        nat: get_bool(b, "nat", issues).unwrap_or(false),
        ip: get_parsed(b, "ip", issues, |s| {
            s.parse().map_err(|_| format!("malformed IP `{s}`"))
        })
        .map(|(v, _)| v),
        mac: get_parsed(b, "mac", issues, |s| s.parse()).map(|(v, _)| v),
        isolated: get_bool(b, "isolated", issues).unwrap_or(false),
    }
}

fn extract_share(b: &Block, issues: &mut IssueList) -> Option<Share> {
    let host = get_path(b, "host", issues)?;
    let (guest, _) = get_str(b, "guest", issues)?;
    let name = match get_str(b, "name", issues) {
        Some((n, _)) => n,
        None => derive_share_name(&guest),
    };
    Some(Share {
        span: span_of(b),
        host,
        guest,
        readonly: get_bool(b, "readonly", issues).unwrap_or(false),
        smb1: get_bool(b, "smb1", issues).unwrap_or(false),
        name,
    })
}

/// Derive an SMB share name from the guest mount path: alphanumeric runs
/// joined by `_`, e.g. `/mnt/src` → `mnt_src`, `D:\data` → `d_data`.
pub fn derive_share_name(guest: &str) -> String {
    let mut out = String::new();
    let mut last_sep = true;
    for c in guest.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            out.push('_');
            last_sep = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn extract_media(b: &Block, issues: &mut IssueList) -> Option<Media> {
    let kind = get_enum(
        b,
        "kind",
        &[("iso", MediaKind::Iso), ("floppy", MediaKind::Floppy)],
        issues,
    )?;
    let from = get_path(b, "from", issues)?;
    Some(Media {
        span: span_of(b),
        kind,
        from,
        label: get_str(b, "label", issues).map(|(s, _)| s),
    })
}

fn extract_disk_block(b: &Block, issues: &mut IssueList) -> Option<DiskBlock> {
    let name = label_name(b, "disk", issues)?;
    Some(DiskBlock {
        name,
        span: span_of(b),
        size: get_size(b, "size", issues),
        from: get_path(b, "from", issues),
    })
}

fn extract_gpu(b: &Block, issues: &mut IssueList) -> Option<Gpu> {
    let mode = get_enum(
        b,
        "mode",
        &[
            ("passthrough", GpuMode::Passthrough),
            ("virgl", GpuMode::Virgl),
            ("vulkan", GpuMode::Vulkan),
        ],
        issues,
    )?;
    Some(Gpu {
        mode,
        address: get_str(b, "address", issues).map(|(s, _)| s),
        span: span_of(b),
    })
}

fn extract_provision(b: &Block, issues: &mut IssueList) -> Option<Provision> {
    let script = label_name(b, "provision", issues)?;
    Some(Provision {
        script: PathBuf::from(script),
        vms: get_str_list(b, "vms", issues),
        span: span_of(b),
    })
}

fn extract_handler(b: &Block, issues: &mut IssueList) -> Option<Handler> {
    let event = label_name(b, "on", issues)?;
    let (run, _) = get_str(b, "run", issues)?;
    Some(Handler {
        event,
        run: PathBuf::from(run),
        span: span_of(b),
    })
}

fn extract_firmware(b: &Block, issues: &mut IssueList) -> Option<Firmware> {
    if b.field("firmware").is_some() {
        get_enum(
            b,
            "firmware",
            &[("ovmf", Firmware::Ovmf), ("seabios", Firmware::Seabios)],
            issues,
        )
    } else {
        None
    }
}

fn extract_vm(b: &Block, issues: &mut IssueList) -> Option<Vm> {
    let name = label_name(b, "vm", issues)?;
    let span = span_of(b);
    let (template, template_span) = match get_str(b, "template", issues) {
        Some((s, tspan)) => match parse_template_ref(&s) {
            Ok(t) => (t, tspan),
            Err(e) => {
                issues.push(Issue::at(tspan, e));
                return None;
            }
        },
        None => {
            issues.push(Issue::at(
                span,
                format!("vm \"{name}\" is missing required `template`"),
            ));
            return None;
        }
    };
    let mut vm = Vm {
        name,
        span,
        template,
        template_span,
        arch: get_str(b, "arch", issues).map(|(s, _)| s),
        profile: get_str(b, "profile", issues).map(|(s, _)| s),
        cpus: get_int(b, "cpus", issues).and_then(|(n, s)| {
            u32::try_from(n).ok().filter(|&c| c > 0).or_else(|| {
                issues.push(Issue::at(
                    s,
                    format!("cpus must be a positive integer, got {n}"),
                ));
                None
            })
        }),
        memory: get_size(b, "memory", issues),
        disk: get_size(b, "disk", issues),
        cdrom: get_path(b, "cdrom", issues),
        floppy: get_path(b, "floppy", issues),
        depends_on: get_str_list(b, "depends_on", issues),
        nested: get_bool(b, "nested", issues).unwrap_or(false),
        gui: get_bool(b, "gui", issues),
        display: get_str(b, "display", issues).map(|(s, _)| s),
        firmware: extract_firmware(b, issues),
        tpm: get_bool(b, "tpm", issues),
        secure_boot: get_bool(b, "secure_boot", issues),
        qemu_args: get_str_list(b, "qemu_args", issues),
        gpu: None,
        nics: Vec::new(),
        extra_disks: Vec::new(),
        shares: Vec::new(),
        media: Vec::new(),
    };
    for child in b.blocks() {
        match child.kind() {
            "nic" => vm.nics.push(extract_nic(&child, issues)),
            "gpu" => vm.gpu = extract_gpu(&child, issues),
            "disk" => {
                if let Some(d) = extract_disk_block(&child, issues) {
                    vm.extra_disks.push(d);
                }
            }
            "share" => {
                if let Some(s) = extract_share(&child, issues) {
                    vm.shares.push(s);
                }
            }
            "media" => {
                if let Some(m) = extract_media(&child, issues) {
                    vm.media.push(m);
                }
            }
            _ => {}
        }
    }
    Some(vm)
}

fn extract_template(b: &Block, issues: &mut IssueList) -> Option<TemplateDef> {
    let name = label_name(b, "template", issues)?;
    let span = span_of(b);
    let Some((arch, arch_span)) = get_str(b, "arch", issues) else {
        issues.push(Issue::at(
            span,
            format!("template \"{name}\" is missing required `arch`"),
        ));
        return None;
    };
    if !KNOWN_ARCHES.contains(&arch.as_str()) {
        issues.push(Issue::at(
            arch_span,
            format!("unknown arch `{arch}` (known: {})", KNOWN_ARCHES.join(", ")),
        ));
        return None;
    }
    let Some((version, _)) = get_str(b, "version", issues) else {
        issues.push(Issue::at(
            span,
            format!("template \"{name}\" is missing required `version`"),
        ));
        return None;
    };
    let mut source = None;
    let mut media = Vec::new();
    let mut provisions = Vec::new();
    let mut nics = Vec::new();
    let mut extra_disks = Vec::new();
    for child in b.blocks() {
        match child.kind() {
            "source" => source = extract_source(&child, issues),
            "media" => {
                if let Some(m) = extract_media(&child, issues) {
                    media.push(m);
                }
            }
            "provision" => {
                if let Some(p) = extract_provision(&child, issues) {
                    provisions.push(p);
                }
            }
            "nic" => nics.push(extract_nic(&child, issues)),
            "disk" => {
                if let Some(d) = extract_disk_block(&child, issues) {
                    extra_disks.push(d);
                }
            }
            _ => {}
        }
    }
    let Some(source) = source else {
        issues.push(Issue::at(
            span,
            format!("template \"{name}\" is missing a `source` block"),
        ));
        return None;
    };
    Some(TemplateDef {
        name,
        span,
        arch,
        version,
        registry: get_str(b, "registry", issues).map(|(s, _)| s),
        profile: get_str(b, "profile", issues).map(|(s, _)| s),
        cpus: get_int(b, "cpus", issues).and_then(|(n, _)| u32::try_from(n).ok()),
        memory: get_size(b, "memory", issues),
        disk: get_size(b, "disk", issues),
        display: get_str(b, "display", issues).map(|(s, _)| s),
        firmware: extract_firmware(b, issues),
        tpm: get_bool(b, "tpm", issues),
        secure_boot: get_bool(b, "secure_boot", issues),
        nested: get_bool(b, "nested", issues).unwrap_or(false),
        gui: get_bool(b, "gui", issues).unwrap_or(false),
        qemu_args: get_str_list(b, "qemu_args", issues),
        first_boot: get_path(b, "first_boot", issues),
        source,
        media,
        provisions,
        nics,
        extra_disks,
    })
}

fn extract_source(b: &Block, issues: &mut IssueList) -> Option<TemplateSource> {
    let kind = label_name(b, "source", issues)?;
    let span = span_of(b);
    let path = get_path(b, "path", issues);
    let url = get_str(b, "url", issues).map(|(s, _)| s);
    let sha256 = get_str(b, "sha256", issues).map(|(s, _)| s);
    let artefact = |issues: &mut IssueList| -> Option<ArtefactSource> {
        match (path.clone(), url.clone()) {
            (Some(p), None) => Some(ArtefactSource::Path { path: p, span }),
            (None, Some(u)) => match sha256.clone() {
                Some(h) => Some(ArtefactSource::Url {
                    url: u,
                    sha256: h,
                    span,
                }),
                None => {
                    issues.push(Issue::at(
                        span,
                        "URL sources require `sha256 = ...` (PRD §6.1)",
                    ));
                    None
                }
            },
            (Some(_), Some(_)) => {
                issues.push(Issue::at(
                    span,
                    "source has both `path` and `url` — pick one",
                ));
                None
            }
            (None, None) => {
                issues.push(Issue::at(span, "source requires `path` or `url`"));
                None
            }
        }
    };
    match kind.as_str() {
        "iso" => artefact(issues).map(TemplateSource::Iso),
        "qcow2" => artefact(issues).map(TemplateSource::Qcow2),
        "template" => {
            let (from, fspan) = get_str(b, "from", issues)?;
            match parse_template_ref(&from) {
                Ok(t @ TemplateRef::Store { .. }) => {
                    Some(TemplateSource::Template { from: t, span })
                }
                Ok(_) => {
                    issues.push(Issue::at(
                        fspan,
                        "layered builds take a local store reference `<arch>/<name>[@<version>]`",
                    ));
                    None
                }
                Err(e) => {
                    issues.push(Issue::at(fspan, e));
                    None
                }
            }
        }
        "scratch" => Some(TemplateSource::Scratch { span }),
        other => {
            issues.push(Issue::at(
                span,
                format!("unknown source kind `{other}` (expected iso, qcow2, template, scratch)"),
            ));
            None
        }
    }
}
