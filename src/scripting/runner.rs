//! Script execution: provision scripts (`fn main(lab: Lab)`), event
//! handlers (`fn handle(event: Event, lab: Lab)`), and ad-hoc `vmlab script`.
//! The wisp VM is synchronous — scripts run on blocking threads; host
//! methods bridge back into tokio via the runtime handle carried in each
//! script object.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use wisp::Vm;

use super::{EventData, LabHandle, SegmentHandle};
use crate::labd::lab::LabRuntime;

/// Where script log output goes (lab log + live CLI stream, PRD §10.1).
pub type OutputSink = Arc<dyn Fn(String) + Send + Sync>;

impl SegmentHandle {
    pub(crate) fn with_zone<T>(
        &self,
        f: impl FnOnce(&mut crate::net::dns::DnsZone) -> T,
    ) -> Result<T, String> {
        self.rt.block_on(async {
            let net = self.runtime.network.lock().await;
            let seg = net
                .segments
                .get(&self.segment)
                .ok_or_else(|| format!("segment {} is gone", self.segment))?;
            let zone = seg
                .gateway
                .as_ref()
                .and_then(|g| g.dns_zone())
                .ok_or_else(|| format!("segment {} has DNS disabled", self.segment))?;
            let mut z = zone.lock().map_err(|_| "zone lock poisoned".to_string())?;
            Ok(f(&mut z))
        })
    }
}

/// Run a script file's `main(lab)` against the lab. Blocking errors out of
/// the script fail the run (and therefore `vmlab up`, PRD §10.3).
pub async fn run_script_file(
    runtime: Arc<LabRuntime>,
    script: &Path,
    output: OutputSink,
) -> Result<()> {
    let source =
        std::fs::read_to_string(script).with_context(|| format!("reading {}", script.display()))?;
    let name = script.display().to_string();
    run_script_source(runtime, source, &name, script_dir(script), None, output).await
}

/// Run a `main(lab)` script from in-memory source (a template's embedded
/// first-boot provision, or a file via [`run_script_file`]). `ref_base` roots
/// relative reference-image/screenshot paths; `target_vm`, when set, is the VM
/// the script reaches with `lab.this_vm()`.
pub async fn run_script_source(
    runtime: Arc<LabRuntime>,
    source: String,
    name: &str,
    ref_base: std::path::PathBuf,
    target_vm: Option<String>,
    output: OutputSink,
) -> Result<()> {
    let name = name.to_string();
    let ref_base = Arc::new(ref_base);
    let rt = tokio::runtime::Handle::current();
    let out_err = output.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let ctx = super::context();
        let unit = ctx
            .compile(&source)
            .map_err(|e| anyhow!("{name}: {}", compile_error(e)))?;
        let mut vm = Vm::new(&ctx);
        let lab = LabHandle {
            runtime,
            rt,
            output,
            ref_base,
            first_boot_vm: target_vm,
        };
        vm.call_unit::<_, ()>(&unit, "main", (lab,))
            .map_err(|e| anyhow!("{name}: {}", run_error(e)))
    })
    .await
    .map_err(|e| anyhow!("script thread panicked: {e}"))?;
    if let Err(e) = &result {
        out_err(format!("script failed: {e:#}\n"));
    }
    result
}

/// Run an event handler script's `handle(event, lab)`. Handler failures are
/// logged, never fatal (PRD §8.2).
pub async fn run_event_handler(
    runtime: Arc<LabRuntime>,
    script: &Path,
    event: EventData,
    output: OutputSink,
) {
    let Ok(source) = std::fs::read_to_string(script) else {
        tracing::warn!("handler script {} unreadable", script.display());
        return;
    };
    let name = script.display().to_string();
    let ref_base = Arc::new(script_dir(script));
    let rt = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let ctx = super::context();
        let unit = ctx
            .compile(&source)
            .map_err(|e| anyhow!("{name}: {}", compile_error(e)))?;
        let mut vm = Vm::new(&ctx);
        let lab = LabHandle {
            runtime,
            rt,
            output,
            ref_base,
            first_boot_vm: None,
        };
        vm.call_unit::<_, ()>(&unit, "handle", (event, lab))
            .map_err(|e| anyhow!("{name}: {}", run_error(e)))
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("event handler failed: {e:#}"),
        Err(e) => tracing::warn!("event handler thread panicked: {e}"),
    }
}

/// Directory a script file lives in — the base for its relative reference-image
/// and screenshot paths. Falls back to `.` for a bare filename.
fn script_dir(script: &Path) -> std::path::PathBuf {
    script
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn compile_error(e: wisp::Error) -> String {
    match e {
        wisp::Error::Compile(diags) => {
            let msgs: Vec<String> = diags.iter().map(render_diag).collect();
            msgs.join("; ")
        }
        other => other.to_string(),
    }
}

pub(crate) fn render_diag(d: &wisp::Diagnostic) -> String {
    match &d.help {
        Some(h) => format!("{} [{}] (help: {h})", d.message, d.code),
        None => format!("{} [{}]", d.message, d.code),
    }
}

fn run_error(e: wisp::Error) -> String {
    match e {
        wisp::Error::Runtime(r) => {
            let mut s = r.message.clone();
            if !r.trace.is_empty() {
                s.push_str(&format!(" (at {})", r.trace.join(" <- ")));
            }
            s
        }
        other => other.to_string(),
    }
}

/// Runtime mutation of a segment's L3 rules and forwards (PRD §9.9), bridged
/// from the wisp `Segment` handle into the lab daemon.
impl SegmentHandle {
    fn with_services<T>(
        &self,
        f: impl FnOnce(&Arc<crate::labd::netservices::SegmentServices>) -> Result<T, String>,
    ) -> Result<T, String> {
        self.rt.block_on(async {
            let net = self.runtime.network.lock().await;
            let seg = net
                .segments
                .get(&self.segment)
                .ok_or_else(|| format!("segment {} is gone", self.segment))?;
            let services = seg
                .services
                .as_ref()
                .ok_or_else(|| format!("segment {} has no network services", self.segment))?;
            f(services)
        })
    }

    pub(crate) fn rule_block(
        &self,
        cidr: &str,
        proto: Option<&str>,
        port: Option<i64>,
    ) -> Result<i64, String> {
        use crate::config::model::{BlockRule, L4Proto};
        let net: ipnet::Ipv4Net = cidr
            .parse()
            .or_else(|_| cidr.parse::<std::net::Ipv4Addr>().map(|ip| ip.into()))
            .map_err(|_| format!("malformed CIDR/IP `{cidr}`"))?;
        let proto = match proto {
            None => None,
            Some("tcp") => Some(L4Proto::Tcp),
            Some("udp") => Some(L4Proto::Udp),
            Some("icmp") => Some(L4Proto::Icmp),
            Some(o) => return Err(format!("unknown proto `{o}`")),
        };
        let port = match port {
            None => None,
            Some(p) => Some(u16::try_from(p).map_err(|_| format!("port {p} out of range"))?),
        };
        let rule = BlockRule {
            cidr: net,
            proto,
            port,
            span: (0, 0),
        };
        self.with_services(|s| {
            let mut rs = s.rules.lock().map_err(|_| "ruleset poisoned".to_string())?;
            Ok(rs.add_block(rule).0 as i64)
        })
    }

    pub(crate) fn rule_remove(&self, rule_id: i64) -> Result<bool, String> {
        self.with_services(|s| {
            let mut rs = s.rules.lock().map_err(|_| "ruleset poisoned".to_string())?;
            Ok(rs.remove(crate::net::rules::RuleId(rule_id as u64)))
        })
    }

    pub(crate) fn rule_redirect(&self, from: &str, to: &str) -> Result<i64, String> {
        use crate::config::model::{RedirectRule, parse_host_port};
        let from = parse_host_port(from)?;
        let to = parse_host_port(to)?;
        let rule = RedirectRule {
            from,
            to,
            proto: None,
            span: (0, 0),
        };
        self.with_services(|s| {
            let mut rs = s.rules.lock().map_err(|_| "ruleset poisoned".to_string())?;
            Ok(rs.add_redirect(rule).0 as i64)
        })
    }

    pub(crate) fn add_forward(
        &self,
        host_port: i64,
        vm: &str,
        guest_port: i64,
    ) -> Result<i64, String> {
        let host_port =
            u16::try_from(host_port).map_err(|_| "host_port out of range".to_string())?;
        let guest_port =
            u16::try_from(guest_port).map_err(|_| "guest_port out of range".to_string())?;
        // Resolve the target VM's leased IP on this segment.
        let guest_ip = self.rt.block_on(async {
            self.runtime
                .vm(vm)
                .map_err(|e| format!("{e:#}"))?
                .guest_ip(None)
                .await
                .map_err(|e| format!("{e:#}"))
        })?;
        let guest_ip: std::net::Ipv4Addr = guest_ip
            .parse()
            .map_err(|_| format!("vm {vm} has no IPv4 lease yet"))?;
        let host_addr = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, host_port));
        self.with_services(|s| {
            s.add_forward(
                host_addr,
                guest_ip,
                guest_port,
                crate::config::model::Proto::Tcp,
            )
            .map(|id| id as i64)
        })
    }

    pub(crate) fn route_to(&self, _other: &str, _enable: bool) -> Result<(), String> {
        // Daemon inter-segment routing: explicit opt-in per pair (§9.6).
        Err("inter-segment routing is not yet available from scripts".into())
    }

    pub(crate) fn rules_json(&self) -> Result<String, String> {
        self.with_services(|s| {
            let rs = s.rules.lock().map_err(|_| "ruleset poisoned".to_string())?;
            serde_json::to_string(&rs.list()).map_err(|e| e.to_string())
        })
    }
}
