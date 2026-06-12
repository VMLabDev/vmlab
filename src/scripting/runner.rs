//! Script execution: provision scripts (`fn main(lab: Lab)`), event
//! handlers (`fn handle(event: Event, lab: Lab)`), and ad-hoc `vmlab run`.
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

// Placeholder impls completed by the rules-engine wiring pass; kept here so
// the module registration compiles independently.
impl SegmentHandle {
    pub(crate) fn rule_block(
        &self,
        _cidr: &str,
        _proto: Option<&str>,
        _port: Option<i64>,
    ) -> Result<i64, String> {
        Err("network rules are not wired for this segment yet".into())
    }

    pub(crate) fn rule_remove(&self, _rule_id: i64) -> Result<bool, String> {
        Err("network rules are not wired for this segment yet".into())
    }

    pub(crate) fn rule_redirect(&self, _from: &str, _to: &str) -> Result<i64, String> {
        Err("network rules are not wired for this segment yet".into())
    }

    pub(crate) fn add_forward(
        &self,
        _host_port: i64,
        _vm: &str,
        _guest_port: i64,
    ) -> Result<i64, String> {
        Err("runtime port forwards are not wired yet".into())
    }

    pub(crate) fn route_to(&self, _other: &str, _enable: bool) -> Result<(), String> {
        Err("inter-segment routing is not wired yet".into())
    }

    pub(crate) fn rules_json(&self) -> Result<String, String> {
        Err("network rules are not wired yet".into())
    }
}
