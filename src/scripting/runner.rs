//! Script execution: provision scripts (`fn main(lab: Lab)`), event
//! handlers (`fn handle(event: Event, lab: Lab)`), and ad-hoc `vmlab script`.
//! The wscript VM is synchronous — scripts run on blocking threads; host
//! methods bridge back into tokio via the runtime handle carried in each
//! script object.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use tokio_util::sync::CancellationToken;
use wscript::Vm;

use super::{EventData, LabHandle, SegmentHandle};
use crate::labd::lab::LabRuntime;
use crate::labd::record::RunId;

// Bound bytecode execution for every entry point, including scripts that
// never hand control back to the host. Time spent waiting on a host call
// does not count against this budget.
const SCRIPT_FUEL: u64 = 10_000_000;

/// Where script log output goes (lab log + live CLI stream, PRD §10.1).
pub type OutputSink = Arc<dyn Fn(String) + Send + Sync>;

/// The VM a script belongs to — the one it reaches with `lab.this_vm()`.
/// Set for a `provision {}` block declared inside a `vm {}` and for a
/// template's first-boot script; absent for handlers and `vmlab script`.
#[derive(Debug, Clone)]
pub struct ScriptOwner {
    pub vm: String,
    /// The script is that VM's template first-boot provision, which runs
    /// before the VM is allowed to reach full readiness — so `is_ready` /
    /// `wait_ready` on it mean agent-level readiness instead.
    pub first_boot: bool,
    /// Cancels the run — `up` hands each provision one,
    /// so the first failed step stops the others (§7.2). See
    /// [`super::cancel`].
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

impl ScriptOwner {
    /// The same owner, its run cancelled when `token` is.
    pub fn cancelled_by(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// A machine-scoped `provision {}` block: the VM is fully up already.
    pub fn provision(vm: impl Into<String>) -> Self {
        Self {
            vm: vm.into(),
            first_boot: false,
            cancel: None,
        }
    }

    /// A template first-boot provision.
    pub fn first_boot(vm: impl Into<String>) -> Self {
        Self {
            vm: vm.into(),
            first_boot: true,
            cancel: None,
        }
    }
}

impl SegmentHandle {
    pub(crate) fn with_zone<T>(
        &self,
        f: impl FnOnce(&mut crate::net::dns::DnsZone) -> T,
    ) -> Result<T, String> {
        super::cancel::block_on(&self.rt, async {
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
/// `owner`, when set, is the VM the script reaches with `lab.this_vm()` — the
/// machine whose `provision {}` block declared it.
pub async fn run_script_file(
    runtime: Arc<LabRuntime>,
    script: &Path,
    owner: Option<ScriptOwner>,
    run: Option<RunId>,
    disconnect: Option<CancellationToken>,
    output: OutputSink,
) -> Result<()> {
    let source =
        std::fs::read_to_string(script).with_context(|| format!("reading {}", script.display()))?;
    let name = script.display().to_string();
    run_main(
        runtime,
        source,
        (&name, script_dir(script)),
        owner,
        run,
        disconnect,
        output,
    )
    .await
}

/// Run a `main(lab)` script from in-memory source (a template's embedded
/// first-boot provision, or a file via [`run_script_file`]). `ref_base` roots
/// relative reference-image/screenshot paths; `owner`, when set, is the VM the
/// script reaches with `lab.this_vm()`.
pub async fn run_script_source(
    runtime: Arc<LabRuntime>,
    source: String,
    name: &str,
    ref_base: std::path::PathBuf,
    owner: Option<ScriptOwner>,
    run: Option<RunId>,
    output: OutputSink,
) -> Result<()> {
    run_main(runtime, source, (name, ref_base), owner, run, None, output).await
}

async fn run_main(
    runtime: Arc<LabRuntime>,
    source: String,
    (name, ref_base): (&str, std::path::PathBuf),
    owner: Option<ScriptOwner>,
    run: Option<RunId>,
    disconnect: Option<CancellationToken>,
    output: OutputSink,
) -> Result<()> {
    let name = name.to_string();
    let ref_base = Arc::new(ref_base);
    let rt = tokio::runtime::Handle::current();
    let out_err = output.clone();
    // The run every recording this script starts belongs to (D7): a caller
    // that already holds one (`vmlab script --record`) passes it, so the
    // recordings it started before the script are finalised with the
    // script's own.
    let run = run.unwrap_or_else(|| runtime.recordings.new_run());
    let finalise_on = runtime.clone();
    let token = disconnect.or_else(|| owner.as_ref().and_then(|o| o.cancel.clone()));
    let result = run_blocking(finalise_on.script_shutdown.child_token(), token, move || {
        let lab = LabHandle {
            runtime,
            rt,
            output,
            ref_base,
            owner,
            run,
        };
        execute(&source, &name, "main", (lab,))
    })
    .await;
    // Whatever the script recorded and did not stop is finalised here, on
    // success and on failure alike (D7).
    finalise_on.record_finalise_run(run).await;
    // A cancelled run says nothing here: the step that failed first says
    // why, and this one only stopped.
    if let Err(e) = &result
        && !e.is::<super::cancel::ScriptCancelled>()
    {
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
    let run = runtime.recordings.new_run();
    let finalise_on = runtime.clone();
    let result = run_blocking(finalise_on.script_shutdown.child_token(), None, move || {
        let lab = LabHandle {
            runtime,
            rt,
            output,
            ref_base,
            owner: None,
            run,
        };
        execute(&source, &name, "handle", (event, lab))
    })
    .await;
    finalise_on.record_finalise_run(run).await;
    if let Err(e) = result
        && !e.is::<super::cancel::ScriptCancelled>()
    {
        tracing::warn!("event handler failed: {e:#}");
    }
}

async fn run_blocking(
    token: CancellationToken,
    request_cancel: Option<CancellationToken>,
    script: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    let _cancel_on_drop = token.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        let _entered = super::cancel::enter(token, request_cancel);
        super::cancel::check();
        let result = script();
        super::cancel::check();
        result
    })
    .await
    .map_err(|e| match e.try_into_panic() {
        Ok(payload) if payload.is::<super::cancel::Cancelled>() => {
            anyhow::Error::new(super::cancel::ScriptCancelled)
        }
        Ok(_) => anyhow!("script thread panicked"),
        Err(e) => anyhow!("script thread panicked: {e}"),
    })?
}

fn execute(source: &str, name: &str, entry: &str, args: impl wscript::IntoArgs) -> Result<()> {
    let ctx = super::context();
    let unit = ctx
        .compile(source)
        .map_err(|e| anyhow!("{name}: {}", compile_error(e)))?;
    super::cancel::check();
    let mut vm = Vm::new(&ctx);
    vm.set_fuel(Some(SCRIPT_FUEL));
    vm.call_unit::<_, ()>(&unit, entry, args)
        .map_err(|e| anyhow!("{name}: {}", run_error(e)))
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

fn compile_error(e: wscript::Error) -> String {
    match e {
        wscript::Error::Compile(diags) => {
            let msgs: Vec<String> = diags.iter().map(render_diag).collect();
            msgs.join("; ")
        }
        other => other.to_string(),
    }
}

pub(crate) fn render_diag(d: &wscript::Diagnostic) -> String {
    match &d.help {
        Some(h) => format!("{} [{}] (help: {h})", d.message, d.code),
        None => format!("{} [{}]", d.message, d.code),
    }
}

fn run_error(e: wscript::Error) -> String {
    match e {
        wscript::Error::Runtime(r) => {
            // A fault can end in a payload's own newline (an `expect` on an
            // `Err` carrying a command's stderr); the trace stays on its line.
            let mut s = r.message.trim_end().to_string();
            if !r.trace.is_empty() {
                let chain = r
                    .trace
                    .iter()
                    .map(|f| f.function.as_str())
                    .collect::<Vec<_>>()
                    .join(" <- ");
                s.push_str(&format!(" (at {chain})"));
            }
            s
        }
        other => other.to_string(),
    }
}

/// Runtime mutation of a segment's L3 rules and forwards (PRD §9.9), bridged
/// from the wscript `Segment` handle into the lab daemon.
impl SegmentHandle {
    fn with_services<T>(
        &self,
        f: impl FnOnce(&Arc<crate::labd::netservices::SegmentServices>) -> Result<T, String>,
    ) -> Result<T, String> {
        super::cancel::block_on(&self.rt, async {
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
        // The address the segment's DHCP server leased to the VM's NIC on
        // this segment, not the one its guest reports
        // (docs/reviews/wscript.md F1).
        let (guest_ip, guest_mac) = super::cancel::block_on(
            &self.rt,
            self.runtime.script_forward_target(&self.segment, vm),
        )?;
        use crate::config::model::Proto;
        use crate::freeze::ports;
        // Under worktree isolation a forward binds loopback only (§21.4),
        // and a fixed port is shifted by the lab's port slot as a declared
        // one is (§21.7): chosen, bound and recorded under the family
        // record's lock.
        let bind = crate::labd::isolation::forward_bind(self.runtime.isolation.as_ref());
        let family = match &self.runtime.isolation {
            Some(isolation) if host_port != 0 => {
                let mut locked =
                    ports::lock(&isolation.family_local).map_err(|e| format!("{e:#}"))?;
                let mut record = locked.load().map_err(|e| format!("{e:#}"))?;
                let key = ports::PortKey {
                    worktree: isolation.port_owner().map(str::to_string),
                    machine: vm.to_string(),
                    source: "script".into(),
                    guest_port,
                    proto: Proto::Tcp,
                    declared: host_port,
                };
                let slot = record.slot(key.worktree.as_deref());
                let port = record.choose(&key, slot, |p| ports::host_free(bind, p, Proto::Tcp))?;
                record.record(key, port);
                Some((locked, record, port))
            }
            _ => None,
        };
        let port = family.as_ref().map_or(host_port, |(_, _, port)| *port);
        let host_addr = std::net::SocketAddr::from((bind, port));
        let id = self.with_services(|s| {
            let id = s
                .add_forward(host_addr, guest_ip, guest_port, Proto::Tcp)
                .map(|(id, _)| id as i64)?;
            // As a declared forward does: the engine frames what it sends to
            // this address to the NIC that holds it.
            if let Some(mac) = guest_mac {
                s.learn_mac(guest_ip, mac);
            }
            Ok(id)
        })?;
        if let Some((locked, record, _)) = family {
            locked.save(&record).map_err(|e| format!("{e:#}"))?;
            if port != host_port {
                self.runtime.events.emit(
                    "forward.installed",
                    serde_json::json!({
                        "what": format!("\"{vm}\": script forward"),
                        "bind": bind.to_string(),
                        "host_port": port,
                        "auto": false,
                        "declared_port": host_port,
                        "guest": format!("{guest_ip}:{guest_port}"),
                    }),
                );
            }
        }
        Ok(id)
    }

    /// Connect (`enable`) or disconnect this segment and `other` for daemon
    /// inter-segment routing (§9.6): both ways, lab-local segments only.
    pub(crate) fn route_to(&self, other: &str, enable: bool) -> Result<(), String> {
        super::cancel::block_on(&self.rt, async {
            let net = self.runtime.network.lock().await;
            net.set_route(&self.segment, other, enable)
        })
    }

    pub(crate) fn rules_json(&self) -> Result<String, String> {
        self.with_services(|s| {
            let rs = s.rules.lock().map_err(|_| "ruleset poisoned".to_string())?;
            serde_json::to_string(&rs.list()).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `main()` of `src` under vmlab's script context and return the
    /// failure as `vmlab up` prints it after `script failed: <name>: `.
    fn fault_of(src: &str) -> String {
        let ctx = super::super::context();
        let unit = ctx
            .compile(src)
            .unwrap_or_else(|e| panic!("{}", compile_error(e)));
        let mut vm = Vm::new(&ctx);
        let err = vm
            .call_unit::<_, ()>(&unit, "main", ())
            .expect_err("script should fault");
        run_error(err)
    }

    /// A provision that stops on `expect` names its own step and the `Err`
    /// that caused it (#170), not the step alone.
    #[test]
    fn result_expect_reports_the_err_payload() {
        let msg = fault_of(
            r#"
fn setup() -> Result[int, string] {
    Err("powershell: access denied\n")
}

fn main() {
    let n = setup().expect("install-apps failed")
}
"#,
        );
        assert_eq!(
            msg,
            "install-apps failed: powershell: access denied (at main)"
        );
    }

    /// `None` has no payload, so `expect` on it reports the message alone.
    #[test]
    fn option_expect_reports_the_message() {
        let msg = fault_of(
            r#"
fn find() -> Option[string] {
    None
}

fn main() {
    let c = find().expect("no config found")
}
"#,
        );
        assert_eq!(msg, "no config found (at main)");
    }

    /// Set in the child process of `a_deep_value_faults_instead_of_aborting`.
    const DEEP_VALUE_CHILD_ENV: &str = "VMLAB_TEST_DEEP_VALUE_CHILD";

    /// Freeing a value nested far deeper than the native stack is high must
    /// not end the process (docs/reviews/wscript.md F2, card #666): the
    /// process is the lab daemon. A stack overflow is an abort, not a panic,
    /// so the script runs in a child process and the abort is its exit
    /// status. 300,000 levels overflow a 2 MiB stack unless a level costs
    /// under 7 bytes of it.
    #[test]
    fn a_deep_value_faults_instead_of_aborting() {
        const SCRIPT: &str = r#"
struct Node { next: Option[Node] }

fn main() {
    let cur = Node { next: None }
    let i = 0
    while i < 300000 {
        cur = Node { next: Some(cur) }
        i = i + 1
    }
}
"#;
        if std::env::var_os(DEEP_VALUE_CHILD_ENV).is_some() {
            // The stack a script has in the daemon: tokio's blocking
            // threads, which `run_script_source` runs it on, get 2 MiB.
            std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(|| {
                    let ctx = super::super::context();
                    let unit = ctx
                        .compile(SCRIPT)
                        .unwrap_or_else(|e| panic!("{}", compile_error(e)));
                    let mut vm = Vm::new(&ctx);
                    // With or without a fault: the run only has to return.
                    let _ = vm.call_unit::<_, ()>(&unit, "main", ());
                    drop(vm);
                })
                .unwrap()
                .join()
                .unwrap();
            return;
        }
        // libtest names a test without the crate.
        let (_, module) = module_path!().split_once("::").unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::a_deep_value_faults_instead_of_aborting"),
                "--nocapture",
            ])
            .env(DEEP_VALUE_CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "the script run ended its process: {}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
    }
}
