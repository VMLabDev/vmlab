//! Walk a parsed WCL document into the typed model. Structural legality
//! (unknown fields, wrong types) is the schema's job; here we convert values
//! and report anything the schema cannot express as positioned issues.
//!
//! Field access, coercion, spans and issue wording all come from
//! [`super::block`] (ADR-0006); what is left here is the field mapping —
//! which block carries which field, and what the typed model calls it.

use std::path::{Path, PathBuf};

use wcl_lang::{Block, Document};

use super::block::{Reader, Spanned, Unspan, span_of};
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

// ---- block extractors ------------------------------------------------------

fn extract_lab(b: &Block, issues: &mut IssueList) -> Option<Lab> {
    let mut r = Reader::new(b, issues);
    // Read the label, but keep reading: a nameless block should not swallow
    // the diagnostics for everything else wrong inside it.
    let name = r.label();
    let mut lab = Lab {
        name: String::new(),
        span: r.span(),
        gui: r.bool("gui").unspan(),
        segments: Vec::new(),
        vms: Vec::new(),
        containers: Vec::new(),
        handlers: Vec::new(),
        records: Vec::new(),
        sinkholes: Vec::new(),
    };
    for child in r.children() {
        match child.kind() {
            "segment" => {
                if let Some(s) = extract_segment(&child, r.issues()) {
                    lab.segments.push(s);
                }
            }
            "vm" => {
                if let Some(v) = extract_vm(&child, r.issues()) {
                    lab.vms.push(v);
                }
            }
            "container" => {
                if let Some(c) = extract_container(&child, r.issues()) {
                    lab.containers.push(c);
                }
            }
            "on" => {
                if let Some(h) = extract_handler(&child, r.issues()) {
                    lab.handlers.push(h);
                }
            }
            "record" => {
                if let Some(rec) = extract_record(&child, r.issues()) {
                    lab.records.push(rec);
                }
            }
            "sinkhole" => {
                if let Some(s) = extract_sinkhole(&child, r.issues()) {
                    lab.sinkholes.push(s);
                }
            }
            _ => {}
        }
    }
    Some(Lab { name: name?, ..lab })
}

fn extract_segment(b: &Block, issues: &mut IssueList) -> Option<Segment> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let mut seg = Segment {
        name: String::new(),
        span: r.span(),
        subnet: r.parse_as("subnet", "CIDR").unspan(),
        global: r.bool("global").unspan().unwrap_or(false),
        dhcp: r.bool("dhcp").unspan().unwrap_or(true),
        nat: r.bool("nat").unspan().unwrap_or(false),
        mtu: r.int_in("mtu", 576, u16::MAX as i64).unspan(),
        routes_to: r.string_list("routes_to"),
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
    for child in r.children() {
        match child.kind() {
            "dns" => {
                let mut c = Reader::new(&child, r.issues());
                seg.dns = SegmentDns {
                    server: c.parse_as("server", "IP address").unspan(),
                    enabled: c.bool("enabled").unspan().unwrap_or(true),
                    declared: true,
                    span: Some(c.span()),
                };
            }
            "connect" => {
                let mut c = Reader::new(&child, r.issues());
                if let Some(host) = c.required_string("host") {
                    seg.connect = Some(Connect {
                        host: host.value,
                        span: c.span(),
                    });
                }
            }
            "route" => {
                let mut c = Reader::new(&child, r.issues());
                let dest = c.required("dest", |c, n| c.parse_as(n, "CIDR"));
                let via = c.required("via", |c, n| c.parse_as(n, "IP address"));
                if let (Some(dest), Some(via)) = (dest, via) {
                    seg.routes.push(Route {
                        dest: dest.value,
                        via: via.value,
                        span: c.span(),
                    });
                }
            }
            "record" => {
                if let Some(rec) = extract_record(&child, r.issues()) {
                    seg.records.push(rec);
                }
            }
            "forward" => {
                if let Some(f) = extract_forward(&child, r.issues()) {
                    seg.forwards.push(f);
                }
            }
            "block" => {
                let mut c = Reader::new(&child, r.issues());
                let cidr = c.required("cidr", |c, n| c.parse_as(n, "CIDR"));
                let proto = c.keyword("proto", L4_PROTOS).unspan();
                let port = c.port("port").unspan();
                if let Some(cidr) = cidr {
                    seg.block_rules.push(BlockRule {
                        cidr: cidr.value,
                        proto,
                        port,
                        span: c.span(),
                    });
                }
            }
            "redirect" => {
                let mut c = Reader::new(&child, r.issues());
                let from = c.required("from", |c, n| c.parsed(n, parse_host_port));
                let to = c.required("to", |c, n| c.parsed(n, parse_host_port));
                let proto = c
                    .keyword("proto", &[("tcp", L4Proto::Tcp), ("udp", L4Proto::Udp)])
                    .unspan();
                if let (Some(from), Some(to)) = (from, to) {
                    seg.redirect_rules.push(RedirectRule {
                        from: from.value,
                        to: to.value,
                        proto,
                        span: c.span(),
                    });
                }
            }
            "sinkhole" => {
                if let Some(s) = extract_sinkhole(&child, r.issues()) {
                    seg.sinkholes.push(s);
                }
            }
            _ => {}
        }
    }
    Some(Segment { name: name?, ..seg })
}

const L4_PROTOS: &[(&str, L4Proto)] = &[
    ("tcp", L4Proto::Tcp),
    ("udp", L4Proto::Udp),
    ("icmp", L4Proto::Icmp),
];

const PROTOS: &[(&str, Proto)] = &[
    ("tcp", Proto::Tcp),
    ("udp", Proto::Udp),
    ("both", Proto::Both),
];

fn extract_record(b: &Block, issues: &mut IssueList) -> Option<DnsRecord> {
    let mut r = Reader::new(b, issues);
    let name = r.required_string("name");
    let ip = r.required("ip", |r, n| r.parse_as(n, "IP address"));
    match (name, ip) {
        (Some(name), Some(ip)) => Some(DnsRecord {
            name: name.value,
            ip: ip.value,
            span: r.span(),
        }),
        _ => None,
    }
}

fn extract_sinkhole(b: &Block, issues: &mut IssueList) -> Option<SinkholeRule> {
    let mut r = Reader::new(b, issues);
    let pattern = r.required_string("pattern")?;
    let mode = r
        .keyword(
            "mode",
            &[
                ("nxdomain", SinkholeMode::Nxdomain),
                ("zero", SinkholeMode::Zero),
            ],
        )
        .unspan()
        .unwrap_or(SinkholeMode::Nxdomain);
    Some(SinkholeRule {
        pattern: pattern.value,
        mode,
        span: r.span(),
    })
}

fn extract_forward(b: &Block, issues: &mut IssueList) -> Option<Forward> {
    let mut r = Reader::new(b, issues);
    let span = r.span();
    let host_port = r.required("host_port", |r, n| r.port(n))?.value;
    let to = r.required_string("to")?;
    let Some((vm, port_s)) = to.value.split_once(':') else {
        r.issue_at(
            to.span,
            format!("`to` must be \"vm:port\", got `{}`", to.value),
        );
        return None;
    };
    let Ok(guest_port) = port_s.parse::<u16>() else {
        r.issue_at(to.span, format!("malformed guest port in `{}`", to.value));
        return None;
    };
    let proto = r.keyword("proto", PROTOS).unspan().unwrap_or(Proto::Tcp);
    Some(Forward {
        host_port,
        vm: vm.to_string(),
        guest_port,
        proto,
        span,
    })
}

fn extract_nic(b: &Block, issues: &mut IssueList) -> Nic {
    let mut r = Reader::new(b, issues);
    Nic {
        span: r.span(),
        segment: r.string("segment").unspan(),
        nat: r.bool("nat").unspan().unwrap_or(false),
        ip: r.parse_as("ip", "IP address").unspan(),
        gateway: r.bool("gateway").unspan().unwrap_or(false),
        mac: r.parse_as("mac", "MAC address").unspan(),
        isolated: r.bool("isolated").unspan().unwrap_or(false),
    }
}

fn extract_share(b: &Block, issues: &mut IssueList) -> Option<Share> {
    let mut r = Reader::new(b, issues);
    let span = r.span();
    let host = r.required_path("host")?;
    let guest = r.required_string("guest")?;
    let name = match r.string("name") {
        Some(n) => n.value,
        None => derive_share_name(&guest.value),
    };
    let smb1 = r.bool("smb1").unspan().unwrap_or(false);
    let transport = r
        .keyword(
            "transport",
            &[
                ("auto", ShareTransport::Auto),
                ("virtiofs", ShareTransport::Virtiofs),
                ("smb", ShareTransport::Smb),
            ],
        )
        .unspan()
        .unwrap_or(ShareTransport::Auto);
    if smb1 && transport == ShareTransport::Virtiofs {
        r.issue(
            "`smb1 = true` conflicts with `transport = \"virtiofs\"` — SMB1 guests have no \
             virtiofs client",
        );
    }
    Some(Share {
        span,
        host: host.value,
        guest: guest.value,
        readonly: r.bool("readonly").unspan().unwrap_or(false),
        smb1,
        name,
        transport,
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
    let mut r = Reader::new(b, issues);
    let kind = r.required("kind", |r, n| {
        r.keyword(n, &[("iso", MediaKind::Iso), ("floppy", MediaKind::Floppy)])
    })?;
    let from = r.required_path("from")?;
    Some(Media {
        span: r.span(),
        kind: kind.value,
        from: from.value,
        label: r.string("label").unspan(),
    })
}

fn extract_web(b: &Block, issues: &mut IssueList) -> Option<WebPage> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let span = r.span();
    let port = r.required("port", |r, n| r.port(n));
    let path = match r.string("path") {
        Some(p) if !p.value.is_empty() => {
            if p.value.starts_with('/') {
                p.value
            } else {
                format!("/{}", p.value)
            }
        }
        _ => "/".to_string(),
    };
    // The auth block's diagnostics name the page they are about; an
    // unnamed page still gets them, under the placeholder.
    let page = name.clone().unwrap_or_else(|| "?".to_string());
    let mut auth = None;
    let mut auth_span = None;
    for child in r.children() {
        if child.kind() == "auth" {
            auth_span = Some(span_of(&child));
            auth = extract_web_auth(&child, &page, r.issues());
        }
    }
    Some(WebPage {
        name: name?,
        span,
        port: port?.value,
        path,
        auth,
        auth_span,
    })
}

/// Parse a `web`'s nested `auth {}` block into the typed method enum,
/// enforcing per-method required fields and flagging fields the chosen
/// method ignores (local-consistency checks, like `extract_share`).
fn extract_web_auth(b: &Block, page: &str, issues: &mut IssueList) -> Option<WebAuth> {
    #[derive(Clone, Copy)]
    enum Method {
        Basic,
        Bearer,
        Header,
        Ntlm,
        Form,
    }
    let mut r = Reader::new(b, issues);
    let method = r
        .symbol(
            "method",
            &[
                ("basic", Method::Basic),
                ("bearer", Method::Bearer),
                ("header", Method::Header),
                ("ntlm", Method::Ntlm),
                ("form", Method::Form),
            ],
        )?
        .value;
    // Fields each method uses; anything else set is a mistake worth flagging.
    let used: &[&str] = match method {
        Method::Basic => &["method", "username", "password"],
        Method::Bearer => &["method", "token"],
        Method::Header => &["method", "header", "value"],
        Method::Ntlm => &["method", "username", "password", "domain"],
        Method::Form => &[
            "method",
            "username",
            "password",
            "login_path",
            "login_method",
            "login_body",
            "login_content_type",
            "fail_redirect",
        ],
    };
    const ALL: &[&str] = &[
        "username",
        "password",
        "domain",
        "token",
        "header",
        "value",
        "login_path",
        "login_method",
        "login_body",
        "login_content_type",
        "fail_redirect",
    ];
    let method_name = match method {
        Method::Basic => "basic",
        Method::Bearer => "bearer",
        Method::Header => "header",
        Method::Ntlm => "ntlm",
        Method::Form => "form",
    };
    for f in ALL {
        if r.has(f) && !used.contains(f) {
            r.issue(format!(
                "web page `{page}`: field `{f}` is not used by auth method `:{method_name}`"
            ));
        }
    }
    fn req(r: &mut Reader, field: &str, page: &str, method: &str) -> Option<String> {
        match r.string(field) {
            Some(s) if !s.value.is_empty() => Some(s.value),
            _ => {
                r.issue(format!(
                    "web page `{page}`: auth method `:{method}` requires `{field}`"
                ));
                None
            }
        }
    }
    match method {
        Method::Basic => Some(WebAuth::Basic {
            username: req(&mut r, "username", page, method_name)?,
            password: req(&mut r, "password", page, method_name)?,
        }),
        Method::Bearer => Some(WebAuth::Bearer {
            token: req(&mut r, "token", page, method_name)?,
        }),
        Method::Header => Some(WebAuth::Header {
            name: req(&mut r, "header", page, method_name)?,
            value: req(&mut r, "value", page, method_name)?,
        }),
        Method::Ntlm => Some(WebAuth::Ntlm {
            username: req(&mut r, "username", page, method_name)?,
            password: req(&mut r, "password", page, method_name)?,
            domain: r.string("domain").unspan(),
        }),
        Method::Form => {
            let login_method = match r.string("login_method").unspan() {
                None => "POST".to_string(),
                Some(m) => {
                    let up = m.to_ascii_uppercase();
                    if up != "GET" && up != "POST" {
                        r.issue(format!(
                            "web page `{page}`: `login_method` must be GET or POST, got `{m}`"
                        ));
                    }
                    up
                }
            };
            let login_content_type = r
                .string("login_content_type")
                .unspan()
                .unwrap_or_else(|| "application/x-www-form-urlencoded".to_string());
            Some(WebAuth::Form {
                username: req(&mut r, "username", page, method_name)?,
                password: req(&mut r, "password", page, method_name)?,
                login_path: req(&mut r, "login_path", page, method_name)?,
                login_method,
                login_body: req(&mut r, "login_body", page, method_name)?,
                login_content_type,
                fail_redirect: r.string("fail_redirect").unspan(),
            })
        }
    }
}

fn extract_disk_block(b: &Block, issues: &mut IssueList) -> Option<DiskBlock> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let size = r.size("size").unspan();
    let from = r.path("from").unspan();
    Some(DiskBlock {
        name: name?,
        span: r.span(),
        size,
        from,
    })
}

fn extract_gpu(b: &Block, issues: &mut IssueList) -> Option<Gpu> {
    let mut r = Reader::new(b, issues);
    let mode = r.required("mode", |r, n| {
        r.keyword(
            n,
            &[
                ("passthrough", GpuMode::Passthrough),
                ("virgl", GpuMode::Virgl),
                ("vulkan", GpuMode::Vulkan),
            ],
        )
    })?;
    Some(Gpu {
        mode: mode.value,
        address: r.string("address").unspan(),
        span: r.span(),
    })
}

fn extract_provision(b: &Block, issues: &mut IssueList) -> Option<Provision> {
    let mut r = Reader::new(b, issues);
    let script = r.label()?;
    Some(Provision {
        script: PathBuf::from(script),
        span: r.span(),
    })
}

fn extract_playbook(b: &Block, issues: &mut IssueList) -> Option<Playbook> {
    let mut r = Reader::new(b, issues);
    let path = r.label();
    let play = r.required_string("play");
    let span = r.span();
    let mut vars = Vec::new();
    for child in r.children() {
        if child.kind() == "var"
            && let Some(v) = extract_playbook_var(&child, r.issues())
        {
            vars.push(v);
        }
    }
    Some(Playbook {
        path: PathBuf::from(path?),
        play: play?.value,
        vars,
        span,
    })
}

fn extract_playbook_var(b: &Block, issues: &mut IssueList) -> Option<PlaybookVar> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let value = r.required_string("value");
    Some(PlaybookVar {
        name: name?,
        value: value?.value,
        span: r.span(),
    })
}

fn extract_handler(b: &Block, issues: &mut IssueList) -> Option<Handler> {
    let mut r = Reader::new(b, issues);
    let event = r.label();
    let run = r.required_path("run");
    let targets = r.string_list("targets");
    Some(Handler {
        event: event?,
        run: run?.value,
        targets,
        span: r.span(),
    })
}

const FIRMWARES: &[(&str, Firmware)] = &[("ovmf", Firmware::Ovmf), ("seabios", Firmware::Seabios)];

fn extract_vm(b: &Block, issues: &mut IssueList) -> Option<Vm> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let span = r.span();
    let template = r.required("template", |r, n| r.parsed(n, parse_template_ref));
    let arch = r.string("arch").unspan();
    let profile = r.string("profile").unspan();
    let cpus = r.int_at_least("cpus", 1).unspan();
    let memory = r.size("memory").unspan();
    let disk = r.size("disk").unspan();
    let cdrom = r.path("cdrom").unspan();
    let floppy = r.path("floppy").unspan();
    let depends_on = r.string_list("depends_on");
    let nested = r.bool("nested").unspan().unwrap_or(false);
    let gui = r.bool("gui").unspan();
    let display = r.string("display").unspan();
    let firmware = r.keyword("firmware", FIRMWARES).unspan();
    let tpm = r.bool("tpm").unspan();
    let secure_boot = r.bool("secure_boot").unspan();
    let qemu_args = r.string_list("qemu_args");
    let mut gpu = None;
    let mut nics = Vec::new();
    let mut extra_disks = Vec::new();
    let mut shares = Vec::new();
    let mut media = Vec::new();
    let mut web = Vec::new();
    let mut provisions = Vec::new();
    let mut playbooks = Vec::new();
    for child in r.children() {
        match child.kind() {
            "nic" => nics.push(extract_nic(&child, r.issues())),
            "gpu" => gpu = extract_gpu(&child, r.issues()),
            "disk" => {
                if let Some(d) = extract_disk_block(&child, r.issues()) {
                    extra_disks.push(d);
                }
            }
            "share" => {
                if let Some(s) = extract_share(&child, r.issues()) {
                    shares.push(s);
                }
            }
            "media" => {
                if let Some(m) = extract_media(&child, r.issues()) {
                    media.push(m);
                }
            }
            "web" => {
                if let Some(w) = extract_web(&child, r.issues()) {
                    web.push(w);
                }
            }
            "provision" => {
                if let Some(p) = extract_provision(&child, r.issues()) {
                    provisions.push(p);
                }
            }
            "playbook" => {
                if let Some(p) = extract_playbook(&child, r.issues()) {
                    playbooks.push(p);
                }
            }
            _ => {}
        }
    }
    let template = template?;
    Some(Vm {
        name: name?,
        span,
        template: template.value,
        template_span: template.span,
        arch,
        profile,
        cpus,
        memory,
        disk,
        cdrom,
        floppy,
        depends_on,
        nested,
        gui,
        display,
        firmware,
        tpm,
        secure_boot,
        qemu_args,
        gpu,
        nics,
        extra_disks,
        shares,
        media,
        web,
        provisions,
        playbooks,
    })
}

fn extract_container(b: &Block, issues: &mut IssueList) -> Option<Container> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let span = r.span();
    let image = r.required("image", |r, n| r.parsed(n, parse_image_ref));
    let mode = r
        .symbol(
            "mode",
            &[
                ("workload", ContainerMode::Workload),
                ("idle", ContainerMode::Idle),
            ],
        )
        .unspan()
        .unwrap_or_default();
    let entrypoint = r.opt_string_list("entrypoint");
    let command = r.opt_string_list("command");
    let workdir = r.string("workdir").unspan();
    let user = r.string("user").unspan();
    let profile = r.string("profile").unspan();
    let cpus = r.int_at_least("cpus", 1).unspan();
    let memory = r.size("memory").unspan();
    let depends_on = r.string_list("depends_on");
    let mut nics = Vec::new();
    let mut env = Vec::new();
    let mut volumes = Vec::new();
    let mut ports = Vec::new();
    let mut healthcheck = None;
    let mut web = Vec::new();
    let mut provisions = Vec::new();
    let mut playbooks = Vec::new();
    for child in r.children() {
        match child.kind() {
            "nic" => nics.push(extract_nic(&child, r.issues())),
            "env" => {
                if let Some(e) = extract_env(&child, r.issues()) {
                    env.push(e);
                }
            }
            "volume" => {
                if let Some(v) = extract_volume(&child, r.issues()) {
                    volumes.push(v);
                }
            }
            "port" => {
                if let Some(p) = extract_port(&child, r.issues()) {
                    ports.push(p);
                }
            }
            "healthcheck" => healthcheck = extract_healthcheck(&child, r.issues()),
            "web" => {
                if let Some(w) = extract_web(&child, r.issues()) {
                    web.push(w);
                }
            }
            "provision" => {
                if let Some(p) = extract_provision(&child, r.issues()) {
                    provisions.push(p);
                }
            }
            "playbook" => {
                if let Some(p) = extract_playbook(&child, r.issues()) {
                    playbooks.push(p);
                }
            }
            _ => {}
        }
    }
    let image = image?;
    Some(Container {
        name: name?,
        span,
        image: image.value,
        image_span: image.span,
        mode,
        entrypoint,
        command,
        workdir,
        user,
        profile,
        cpus,
        memory,
        depends_on,
        nics,
        env,
        volumes,
        ports,
        healthcheck,
        web,
        provisions,
        playbooks,
    })
}

fn extract_env(b: &Block, issues: &mut IssueList) -> Option<EnvVar> {
    let mut r = Reader::new(b, issues);
    let name = r.required_string("name")?;
    if name.value.is_empty() || name.value.contains('=') {
        r.issue_at(
            name.span,
            format!("malformed environment variable name `{}`", name.value),
        );
        return None;
    }
    let value = r.required_string("value")?;
    Some(EnvVar {
        name: name.value,
        value: value.value,
        span: r.span(),
    })
}

fn extract_volume(b: &Block, issues: &mut IssueList) -> Option<Volume> {
    let mut r = Reader::new(b, issues);
    let span = r.span();
    let host = r.path("host").unspan();
    let name = r.string("name").unspan();
    let source = match (host, name) {
        (Some(h), None) => VolumeSource::Host(h),
        (None, Some(n)) => {
            let ok =
                !n.is_empty() && n != "." && n != ".." && !n.contains('/') && !n.contains('\\');
            if !ok {
                r.issue(format!(
                    "malformed volume name `{n}` — it becomes a directory name"
                ));
                return None;
            }
            VolumeSource::Named(n)
        }
        (Some(_), Some(_)) => {
            r.issue("volume has both `host` and `name` — pick one");
            return None;
        }
        (None, None) => {
            r.issue("volume needs `host = ...` (bind mount) or `name = ...` (named volume)");
            return None;
        }
    };
    let target = r.required_string("target")?;
    if !target.value.starts_with('/') {
        r.issue_at(
            target.span,
            format!(
                "volume target `{}` must be an absolute path inside the container",
                target.value
            ),
        );
        return None;
    }
    Some(Volume {
        source,
        target: target.value,
        read_only: r.bool("read_only").unspan().unwrap_or(false),
        span,
    })
}

fn extract_port(b: &Block, issues: &mut IssueList) -> Option<PortMap> {
    let mut r = Reader::new(b, issues);
    let span = r.span();
    let host_port = r.required("host", |r, n| r.port(n))?.value;
    let container_port = r.required("container", |r, n| r.port(n))?.value;
    let proto = r.keyword("proto", PROTOS).unspan().unwrap_or(Proto::Tcp);
    Some(PortMap {
        host_port,
        container_port,
        proto,
        span,
    })
}

fn extract_healthcheck(b: &Block, issues: &mut IssueList) -> Option<Healthcheck> {
    let mut r = Reader::new(b, issues);
    let span = r.span();
    let command = r.string_list("command");
    if command.is_empty() {
        r.issue("healthcheck requires a non-empty `command`");
        return None;
    }
    /// A duration that must be greater than zero, else the default.
    fn positive_dur(r: &mut Reader, name: &str, default_secs: u64) -> std::time::Duration {
        match r.duration(name).unspan() {
            Some(d) if d.is_zero() => {
                r.issue(format!("`{name}` must be greater than zero"));
                std::time::Duration::from_secs(default_secs)
            }
            Some(d) => d,
            None => std::time::Duration::from_secs(default_secs),
        }
    }
    let interval = positive_dur(&mut r, "interval", 10);
    let timeout = positive_dur(&mut r, "timeout", 5);
    let start_period = r
        .duration("start_period")
        .unspan()
        .unwrap_or(std::time::Duration::from_secs(10));
    let retries = r.int_at_least("retries", 1).unspan().unwrap_or(3);
    Some(Healthcheck {
        command,
        interval,
        timeout,
        retries,
        start_period,
        span,
    })
}

fn extract_template(b: &Block, issues: &mut IssueList) -> Option<TemplateDef> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let span = r.span();
    let arch = r.required("arch", |r, n| r.one_of(n, KNOWN_ARCHES));
    let version = r.required_string("version");
    let mut source = None;
    let mut media = Vec::new();
    let mut provisions = Vec::new();
    let mut playbooks = Vec::new();
    let mut nics = Vec::new();
    let mut extra_disks = Vec::new();
    for child in r.children() {
        match child.kind() {
            "source" => source = extract_source(&child, r.issues()),
            "media" => {
                if let Some(m) = extract_media(&child, r.issues()) {
                    media.push(m);
                }
            }
            "provision" => {
                if let Some(p) = extract_provision(&child, r.issues()) {
                    provisions.push(p);
                }
            }
            "playbook" => {
                if let Some(p) = extract_playbook(&child, r.issues()) {
                    playbooks.push(p);
                }
            }
            "nic" => nics.push(extract_nic(&child, r.issues())),
            "disk" => {
                if let Some(d) = extract_disk_block(&child, r.issues()) {
                    extra_disks.push(d);
                }
            }
            _ => {}
        }
    }
    let registry = r.string("registry").unspan();
    let profile = r.string("profile").unspan();
    // `cpus` only guards its sign: what makes a count valid is §5.1's job.
    let cpus = r.int_at_least("cpus", 0).unspan();
    let memory = r.size("memory").unspan();
    let disk = r.size("disk").unspan();
    let display = r.string("display").unspan();
    let firmware = r.keyword("firmware", FIRMWARES).unspan();
    let tpm = r.bool("tpm").unspan();
    let secure_boot = r.bool("secure_boot").unspan();
    let nested = r.bool("nested").unspan().unwrap_or(false);
    let gui = r.bool("gui").unspan().unwrap_or(false);
    let qemu_args = r.string_list("qemu_args");
    let first_boot = r.path("first_boot").unspan();
    let agent = r.bool("agent").unspan().unwrap_or(true);
    // A missing `source` block is a schema error already (`@child("source")`
    // is not optional); saying so again here would just double the report.
    Some(TemplateDef {
        name: name?,
        span,
        arch: arch?.value,
        version: version?.value,
        source: source?,
        registry,
        profile,
        cpus,
        memory,
        disk,
        display,
        firmware,
        tpm,
        secure_boot,
        nested,
        gui,
        qemu_args,
        first_boot,
        agent,
        media,
        provisions,
        playbooks,
        nics,
        extra_disks,
    })
}

fn extract_source(b: &Block, issues: &mut IssueList) -> Option<TemplateSource> {
    let mut r = Reader::new(b, issues);
    let kind = r.label()?;
    let span = r.span();
    let path = r.path("path").unspan();
    let url = r.string("url").unspan();
    let sha256 = r.string("sha256").unspan();
    let artefact = |r: &mut Reader| -> Option<ArtefactSource> {
        match (path.clone(), url.clone()) {
            (Some(p), None) => Some(ArtefactSource::Path { path: p, span }),
            (None, Some(u)) => match sha256.clone() {
                Some(h) => Some(ArtefactSource::Url {
                    url: u,
                    sha256: h,
                    span,
                }),
                None => {
                    r.issue("URL sources require `sha256 = ...` (PRD §6.1)");
                    None
                }
            },
            (Some(_), Some(_)) => {
                r.issue("source has both `path` and `url` — pick one");
                None
            }
            (None, None) => {
                r.issue("source requires `path` or `url`");
                None
            }
        }
    };
    match kind.as_str() {
        "iso" => artefact(&mut r).map(TemplateSource::Iso),
        "qcow2" => artefact(&mut r).map(TemplateSource::Qcow2),
        "template" => {
            let Spanned {
                value: from,
                span: from_span,
            } = r.required("from", |r, n| r.parsed(n, parse_template_ref))?;
            match from {
                t @ TemplateRef::Store { .. } => Some(TemplateSource::Template { from: t, span }),
                _ => {
                    r.issue_at(
                        from_span,
                        "layered builds take a local store reference `<arch>/<name>[@<version>]`",
                    );
                    None
                }
            }
        }
        "scratch" => Some(TemplateSource::Scratch { span }),
        other => {
            r.issue(format!(
                "unknown source kind `{other}` (expected iso, qcow2, template, scratch)"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract `body` as the contents of one lab, returning what survived
    /// alongside every issue raised. Goes through the real schema, so what
    /// the schema rejects shows up here too — which is the point: these
    /// tests are about the field mapping, and the mapping's job stops where
    /// the schema's begins.
    fn lab(body: &str) -> (Option<Lab>, Vec<String>) {
        let src = format!("import <vmlab.wcl>\nlab \"l\" {{\n{body}\n}}\n");
        let doc = super::super::open(&src, "<test>", Some(Path::new("/tmp")))
            .unwrap_or_else(|e| panic!("{body} should parse: {e:?}"));
        let mut issues = super::super::schema_issues(&doc);
        let block = doc.blocks().next().expect("the lab block");
        let lab = extract_lab(&block, &mut issues);
        (lab, issues.iter().map(|i| i.message.clone()).collect())
    }

    /// Fields the schema declares required but does not itself check for
    /// (wcl enforces required *child blocks* only). Before ADR-0006 each of
    /// these silently dropped the block it was in and the file still loaded.
    #[test]
    fn a_missing_required_field_is_reported_rather_than_dropped() {
        let cases = [
            (
                "vm \"a\" { template = \"x86_64/t\" media { } }",
                "missing required field `kind`",
            ),
            (
                "vm \"a\" { template = \"x86_64/t\" media { kind = \"iso\" } }",
                "missing required field `from`",
            ),
            (
                "vm \"a\" { template = \"x86_64/t\" share { guest = \"/mnt/x\" } }",
                "missing required field `host`",
            ),
            (
                "record { ip = \"10.0.0.1\" }",
                "missing required field `name`",
            ),
            ("record { name = \"a\" }", "missing required field `ip`"),
            ("on \"vm.crashed\" { }", "missing required field `run`"),
            (
                "vm \"a\" { template = \"x86_64/t\" playbook \"p\" { } }",
                "missing required field `play`",
            ),
            (
                "segment \"s\" { route { via = \"10.0.0.1\" } }",
                "missing required field `dest`",
            ),
            (
                "container \"c\" { image = \"nginx\" port { container = 80 } }",
                "missing required field `host`",
            ),
        ];
        for (body, expected) in cases {
            let (_, issues) = lab(body);
            assert!(
                issues.iter().any(|m| m == expected),
                "{body}: expected `{expected}`, got {issues:?}"
            );
        }
    }

    /// One pass, every mistake — a lab author is not fixing one per run.
    #[test]
    fn every_mistake_is_reported_in_one_pass() {
        let (_, issues) = lab("segment \"s\" { subnet = \"10.1\" mtu = 100 }\n\
             vm \"a\" { template = \"x86_64/t\" cpus = 0 }");
        for expected in [
            "malformed CIDR `10.1`",
            "`mtu` must be between 576 and 65535, got 100",
            "`cpus` must be at least 1, got 0",
        ] {
            assert!(
                issues.iter().any(|m| m == expected),
                "missing `{expected}` in {issues:?}"
            );
        }
    }

    /// A block that cannot be built still reports everything else wrong
    /// inside it: the mistake that sinks it does not hide its neighbours.
    #[test]
    fn a_block_that_cannot_be_built_still_reports_what_is_inside_it() {
        // No `template`, so the VM is dropped — but its NIC, its share and
        // its own bad `cpus` are all still reported.
        let (_, issues) =
            lab("vm \"a\" { cpus = 0 nic { ip = \"10.50\" } share { guest = \"/mnt/x\" } }");
        for expected in [
            "missing required field `template`",
            "`cpus` must be at least 1, got 0",
            "malformed IP address `10.50`",
            "missing required field `host`",
        ] {
            assert!(
                issues.iter().any(|m| m == expected),
                "missing `{expected}` in {issues:?}"
            );
        }
    }

    /// The same, for the label: a nameless block does not swallow the
    /// diagnostics for everything it contains.
    #[test]
    fn a_nameless_block_still_reports_what_is_inside_it() {
        let (lab_out, issues) = lab("segment { mtu = 100 }");
        assert!(lab_out.is_some(), "the lab itself is still built");
        assert!(
            issues
                .iter()
                .any(|m| m == "`segment` requires a name label"),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|m| m == "`mtu` must be between 576 and 65535, got 100"),
            "{issues:?}"
        );
    }

    /// Each issue points at the text that caused it, not at the block that
    /// happens to contain it.
    #[test]
    fn an_issue_points_at_the_field_that_caused_it() {
        let src = "import <vmlab.wcl>\nlab \"l\" { segment \"s\" { mtu = 100 } }\n";
        let doc = super::super::open(src, "<test>", Some(Path::new("/tmp"))).unwrap();
        let mut issues = super::super::schema_issues(&doc);
        let block = doc.blocks().next().unwrap();
        extract_lab(&block, &mut issues);
        let span = issues[0].span.expect("positioned");
        assert_eq!(&src[span.offset()..span.offset() + span.len()], "mtu = 100");
    }
}
