//! Script execution: provision scripts (`fn main(lab: Lab)`), event
//! handlers (`fn handle(event: Event, lab: Lab)`), and ad-hoc `vmlab script`.
//! The wscript VM is synchronous — scripts run on blocking threads; host
//! methods bridge back into tokio via the runtime handle carried in each
//! script object.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use tokio_util::sync::CancellationToken;
use wscript::{Vm, VmConfig};

use super::{EventData, LabHandle, ScriptEntry, SegmentHandle};
use crate::labd::lab::LabRuntime;
use crate::labd::record::RunId;

/// The instructions a script may run without a host call. It ends a script
/// that spins and never hands control back to the host, where a cancelled
/// run stops. The count starts again when a host call returns
/// ([`super::cancel::alive`]), so it is no limit on the work of a script that
/// keeps reaching the host (docs/reviews/wscript-2.md F17).
const SCRIPT_FUEL: u64 = 10_000_000;

/// The bytes of values one script run may hold at one time. The instruction
/// budget does not limit memory: one instruction can allocate any amount, and
/// a failed allocation ends the lab daemon (docs/reviews/wscript-2.md F13).
/// `exec` returns up to 64 MiB, so a script can keep several outputs.
const SCRIPT_MEMORY: u64 = 1024 * 1024 * 1024;

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
    let result = run_blocking(
        finalise_on.script_shutdown.child_token(),
        token,
        move || {
            let print = output.clone();
            let lab = LabHandle {
                runtime,
                rt,
                output,
                ref_base,
                owner,
                run,
            };
            execute(&source, &name, ScriptEntry::Main, (lab,), print)
        },
    )
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
        let print = output.clone();
        let lab = LabHandle {
            runtime,
            rt,
            output,
            ref_base,
            owner: None,
            run,
        };
        execute(&source, &name, ScriptEntry::Handler, (event, lab), print)
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

fn execute(
    source: &str,
    name: &str,
    entry: ScriptEntry,
    args: impl wscript::IntoArgs,
    output: OutputSink,
) -> Result<()> {
    let ctx = super::context();
    let unit = ctx
        .compile(source)
        .map_err(|e| anyhow!("{name}: {}", compile_error(e)))?;
    super::cancel::check();
    let config = VmConfig {
        out: Box::new(move |text, newline| {
            let mut chunk = text.to_owned();
            if newline {
                chunk.push('\n');
            }
            output(chunk);
        }),
        fuel: Some(SCRIPT_FUEL),
        refuel: Some(Box::new(super::cancel::alive)),
        memory: Some(SCRIPT_MEMORY),
        ..VmConfig::default()
    };
    let mut vm = Vm::with_config(&ctx, config);
    vm.call_unit::<_, ()>(&unit, entry.name(), args)
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

pub(super) fn compile_error(e: wscript::Error) -> String {
    match e {
        wscript::Error::Compile(diags) => {
            let msgs: Vec<String> = diags.iter().map(render_diag).collect();
            msgs.join("; ")
        }
        other => other.to_string(),
    }
}

fn render_diag(d: &wscript::Diagnostic) -> String {
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
            let mut s = limit_fault(&r.message).unwrap_or_else(|| r.message.trim_end().to_string());
            if !r.trace.is_empty() {
                const TRACE_FRAMES: usize = 12;
                let mut chain = r
                    .trace
                    .iter()
                    .take(TRACE_FRAMES)
                    .map(|f| f.function.as_str())
                    .collect::<Vec<_>>()
                    .join(" <- ");
                if r.trace.len() > TRACE_FRAMES {
                    chain.push_str(&format!(" <- … {} more", r.trace.len() - TRACE_FRAMES));
                }
                s.push_str(&format!(" (at {chain})"));
            }
            s
        }
        other => other.to_string(),
    }
}

/// The VM's fault for one of vmlab's two limits on a script run, in words
/// that say what the limit is, how large it is and that it is vmlab's. The
/// VM's own (`fuel exhausted`) name neither.
fn limit_fault(message: &str) -> Option<String> {
    if message == "fuel exhausted" {
        return Some(format!(
            "the script ran {} instructions without a host call, which is vmlab's limit \
             for a script that does not end (the count starts again at each host call)",
            grouped(SCRIPT_FUEL)
        ));
    }
    if message.starts_with("memory limit of ") {
        return Some(format!(
            "memory limit exceeded: the script holds more than {} MiB of values, which is \
             vmlab's limit for one script run",
            SCRIPT_MEMORY / (1024 * 1024)
        ));
    }
    None
}

/// `n` with a comma between each three digits.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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

    #[test]
    fn card_861_a_recursion_fault_has_a_short_trace() {
        let msg = fault_of("fn f(n: int) -> int { f(n + 1) }\nfn main() { let x = f(0) }");
        assert!(msg.contains("stack overflow"), "{msg}");
        assert!(msg.len() < 1000, "fault length: {}", msg.len());
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

    /// Set in the child process of
    /// `a_script_that_allocates_without_end_faults`.
    const GROWING_VALUE_CHILD_ENV: &str = "VMLAB_TEST_GROWING_VALUE_CHILD";

    /// A script that doubles a string in a loop asks for 1 TiB in about 400
    /// instructions, far inside the instruction budget, and a failed
    /// allocation is an abort of the lab daemon
    /// (docs/reviews/wscript-2.md F13, card #1086). The run has to end with
    /// a fault that names the memory limit. It runs in a child process so
    /// that an abort is an exit status, and under a 4 GiB limit on the
    /// address space so that a run without the fix cannot take the memory
    /// of the host.
    #[test]
    fn a_script_that_allocates_without_end_faults() {
        const SCRIPT: &str = r#"
fn main() {
    let s = "x"
    let i = 0
    while i < 40 {
        s = s + s
        i = i + 1
    }
}
"#;
        if std::env::var_os(GROWING_VALUE_CHILD_ENV).is_some() {
            let limit = 4 * 1024 * 1024 * 1024;
            rustix::process::setrlimit(
                rustix::process::Resource::As,
                rustix::process::Rlimit {
                    current: Some(limit),
                    maximum: Some(limit),
                },
            )
            .unwrap();
            let result = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(|| execute(SCRIPT, "grow.ws", ScriptEntry::Main, (), Arc::new(|_| {})))
                .unwrap()
                .join()
                .unwrap();
            let error = format!("{:#}", result.expect_err("the script ran to its end"));
            assert!(error.contains("memory limit"), "{error}");
            return;
        }
        // libtest names a test without the crate.
        let (_, module) = module_path!().split_once("::").unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::a_script_that_allocates_without_end_faults"),
                "--nocapture",
            ])
            .env(GROWING_VALUE_CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "the script run ended its process, or without the fault: {}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
    }

    // -------------------------------------------------------- memory limit
    //
    // The memory limit of the vendored VM (`vendor/wscript-vm/src/mem.rs`,
    // card #1086). That crate's own tests need the compiler as a
    // dev-dependency, which a crate outside the workspace cannot have, so
    // the limit is tested here. The limits are small so that no test holds
    // much.

    const MIB: u64 = 1024 * 1024;

    /// Run `main() -> int` of `src` under a memory limit of `limit` bytes
    /// (none for `None`): its result, or the VM's fault message.
    fn run_limited(src: &str, limit: Option<u64>) -> Result<i64, String> {
        let ctx = super::super::context();
        let unit = ctx
            .compile(src)
            .unwrap_or_else(|e| panic!("{}", compile_error(e)));
        let config = VmConfig {
            memory: limit,
            ..VmConfig::default()
        };
        let mut vm = Vm::with_config(&ctx, config);
        vm.call_unit::<_, i64>(&unit, "main", ())
            .map_err(|e| match e {
                wscript::Error::Runtime(r) => r.message,
                other => other.to_string(),
            })
    }

    /// `src` must be stopped by a limit of 1 MiB.
    fn stopped_at_one_mib(src: &str) {
        let fault = run_limited(src, Some(MIB)).expect_err("the script ran to its end");
        assert_eq!(fault, format!("memory limit of {MIB} bytes exceeded"));
    }

    /// Doubling a string asks for any amount in a few instructions. The
    /// concatenation that does not fit is refused before it allocates.
    #[test]
    fn memory_limit_stops_a_doubling_string() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let s = "x"
    let i = 0
    while i < 40 {
        s = s + s
        i = i + 1
    }
    i
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// Without a limit the VM counts nothing: the string doubles to 1 MiB and
    /// the run ends.
    #[test]
    fn memory_none_is_no_limit() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let s = "x"
    let i = 0
    while i < 20 {
        s = s + s
        i = i + 1
    }
    s.bytes_len()
}
"#;
        assert_eq!(run_limited(SCRIPT, None), Ok(1048576));
    }

    /// A list that grows one element at a time is stopped too: each `push` is
    /// small, the list is not.
    #[test]
    fn memory_limit_stops_a_growing_list() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let l = [0]
    while true { l.push(1) }
    0
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// A list of fresh strings is stopped by what the strings take, not by the
    /// size of the list alone.
    #[test]
    fn memory_limit_counts_what_a_list_holds() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(1024)
    let l = [""]
    while true { l.push(big + "x") }
    0
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// The limit is on what the script holds, not on what it has allocated in
    /// total: 4000 strings of 16 KiB, each dropped at the next pass, are 64 MiB
    /// of allocation under a limit of 1 MiB.
    #[test]
    fn memory_limit_does_not_count_dropped_values() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(1024)
    let n = 0
    let i = 0
    while i < 4000 {
        let t = big + "x"
        n = n + t.bytes_len()
        i = i + 1
    }
    n
}
"#;
        assert_eq!(run_limited(SCRIPT, Some(MIB)), Ok(65540000));
    }

    /// The same value in many places is counted once: 10,000 references to one
    /// string of 16 KiB are a list of 10,000 slots, not 160 MiB.
    #[test]
    fn memory_limit_counts_a_shared_value_once() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(1024)
    let l = [big]
    let i = 0
    while i < 10000 {
        l.push(big)
        i = i + 1
    }
    l.len()
}
"#;
        assert_eq!(run_limited(SCRIPT, Some(MIB)), Ok(10001));
    }

    /// What `map` collects from its callback is in no register until `map`
    /// returns. It is counted while `map` runs.
    #[test]
    fn memory_limit_counts_what_map_collects() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(1024)
    let l = [0]
    let i = 0
    while i < 1000 {
        l.push(i)
        i = i + 1
    }
    let all = l.map(|x| big + "x")
    all.len()
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// `fold` keeps one value, however many its callback returned: joining 2000
    /// lines into 128 KiB returns 128 MiB of strings in total and holds one.
    #[test]
    fn memory_limit_does_not_count_what_fold_dropped() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let line = "0123456789abcdef".repeat(4)
    let l = [line]
    let i = 1
    while i < 2000 {
        l.push(line)
        i = i + 1
    }
    let all = l.fold("", |acc, x| acc + x)
    all.bytes_len()
}
"#;
        assert_eq!(run_limited(SCRIPT, Some(MIB)), Ok(128000));
    }

    /// A pad to a width the script names is refused before it allocates.
    #[test]
    fn memory_limit_stops_a_pad_to_any_width() {
        const SCRIPT: &str = r#"
fn main() -> int {
    "x".pad_left(1000000000000, " ").bytes_len()
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// A list of 1000 references to one string of 16 KiB is counted as the list
    /// and the string once. Its `join` is 16 MiB, built inside one builtin: each
    /// piece has to fit before it is appended (review of card #1086, F1).
    #[test]
    fn memory_limit_stops_a_join_of_one_string_many_times() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(1024)
    let l = [big]
    let i = 1
    while i < 1000 {
        l.push(big)
        i = i + 1
    }
    l.join("").bytes_len()
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// `replace` makes each match as long as its replacement: 16 KiB of matches
    /// times 16 KiB is 256 MiB in one call. It is refused before it allocates
    /// (review of card #1086, F1).
    #[test]
    fn memory_limit_stops_a_replace_that_multiplies() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "x".repeat(16384)
    big.replace("x", "y".repeat(16384)).bytes_len()
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// `str` of the same list writes the string 1000 times, like `join`.
    #[test]
    fn memory_limit_stops_a_rendering_of_one_string_many_times() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(1024)
    let l = [big]
    let i = 1
    while i < 1000 {
        l.push(big)
        i = i + 1
    }
    str(l).bytes_len()
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// `chars` makes a list slot for each byte of an ASCII string: 128 KiB
    /// become some MiB. It is refused before it allocates.
    #[test]
    fn memory_limit_stops_chars_of_a_long_string() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let big = "0123456789abcdef".repeat(8192)
    big.chars().len()
}
"#;
        stopped_at_one_mib(SCRIPT);
    }

    /// The builtins that ask the limit first are not stopped when the result
    /// fits: a join, a replace and a rendering of 64 KiB each under 1 MiB.
    #[test]
    fn memory_limit_lets_a_result_that_fits_through() {
        const SCRIPT: &str = r#"
fn main() -> int {
    let line = "0123456789abcdef".repeat(4)
    let l = [line]
    let i = 1
    while i < 1000 {
        l.push(line)
        i = i + 1
    }
    let joined = l.join("")
    let replaced = joined.replace("0", "zero")
    joined.bytes_len() + replaced.bytes_len() + str(l).bytes_len()
}
"#;
        // 64,000 joined. 4000 matches of "0" grow by 3 bytes each. The
        // rendering is 1000 strings and 999 of ", " in brackets.
        assert_eq!(
            run_limited(SCRIPT, Some(MIB)),
            Ok(64000 + 76000 + 64000 + 1998 + 2)
        );
    }

    /// A chain of structs nested to any depth is walked without recursion,
    /// and is stopped when it is too long.
    #[test]
    fn memory_limit_walks_a_deep_value() {
        const SCRIPT: &str = r#"
struct Node { next: Option[Node] }

fn main() -> int {
    let cur = Node { next: None }
    while true { cur = Node { next: Some(cur) } }
    0
}
"#;
        let fault = run_limited(SCRIPT, Some(64 * MIB)).expect_err("the script ran to its end");
        assert_eq!(
            fault,
            format!("memory limit of {} bytes exceeded", 64 * MIB)
        );
    }
}
