//! The per-user supervisor `vmlabd` (PRD §3): lab lifecycle, lab registry,
//! global segments, template store writes, host watchdogs, event
//! aggregation. Auto-started by the CLI; runs in the foreground (the CLI
//! detaches it into its own process group).

pub mod global;
pub mod registry;
pub mod templates;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::proto::server::{Handler, Server, Streamer};
use crate::proto::{Event, client::Client};
use global::GlobalSegments;
use registry::{LabEntry, LabState, Registry};

pub struct Supervisor {
    registry: Mutex<Registry>,
    server_events: tokio::sync::OnceCell<tokio::sync::broadcast::Sender<Event>>,
    globals: Arc<GlobalSegments>,
    /// Per-lab locks serialising `ensure_lab`: without this, concurrent
    /// `lab.ensure` calls (a status poll plus an `up`, say) would each spawn a
    /// daemon and pre-pull the same templates in parallel.
    ensure_locks: Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// In-flight template builds/pushes (web Templates page, PRD §6).
    template_ops: templates::TemplateOps,
}

/// Entry point for `vmlab __supervisord`.
pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async())
}

async fn run_async() -> Result<()> {
    let runtime_dir = crate::paths::runtime_dir();
    crate::paths::ensure_dir(&runtime_dir)?;
    crate::paths::ensure_dir(&crate::paths::state_dir())?;

    let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
    let supervisor = Arc::new(Supervisor {
        registry: Mutex::new(Registry::load()),
        server_events: tokio::sync::OnceCell::new(),
        globals: GlobalSegments::new(host_cfg.dns_suffix.clone(), host_cfg.psk.clone()),
        ensure_locks: Mutex::new(std::collections::HashMap::new()),
        template_ops: templates::TemplateOps::default(),
    });

    // Long-lived background tasks register here so the `shutdown` command
    // can cancel and join them deterministically.
    let tasks = Arc::new(crate::lifecycle::TaskGroup::new());

    let sock = crate::paths::supervisor_socket();
    let handler: Arc<dyn Handler> = Arc::new(SupervisorHandler {
        sup: supervisor.clone(),
        tasks: tasks.clone(),
    });
    let server = Server::bind(&sock, handler)
        .await
        .with_context(|| format!("binding {}", sock.display()))?;
    let _ = supervisor.server_events.set(server.events.clone());

    // Cross-host trunk listener (PRD §9.2): enabled when a PSK is configured.
    // Bind on a fixed port (the peer addresses it as host:port).
    if let Some(psk) = &host_cfg.psk {
        let bind: std::net::SocketAddr = ([0, 0, 0, 0], 13947).into();
        global::spawn_peer_listener(supervisor.globals.clone(), bind, psk.clone());
    }

    tracing::info!("vmlabd listening on {}", sock.display());
    supervisor.adopt_existing_labs().await;

    // Disk-space watchdog on the template store's filesystem (PRD §8.1).
    let store_dir = crate::paths::data_dir();
    crate::paths::ensure_dir(&store_dir)?;
    let sup_wd = supervisor.clone();
    let watchdog = crate::config::host::spawn_disk_watchdog(
        store_dir.clone(),
        host_cfg.disk_low_percent,
        std::time::Duration::from_secs(60),
        tasks.cancel_token(),
        move |free| {
            sup_wd.emit(Event::new(
                "host.disk_low",
                "",
                serde_json::json!({"path": store_dir, "free_percent": free}),
            ));
        },
    );
    tasks.adopt("disk-watchdog", watchdog);

    // Run until killed; the `shutdown` command exits the process directly.
    futures::future::pending::<()>().await;
    drop(server);
    Ok(())
}

impl Supervisor {
    fn emit(&self, event: Event) {
        if let Some(tx) = self.server_events.get() {
            let _ = tx.send(event);
        }
    }

    /// Reconnect registry entries from a previous supervisor run: lab
    /// daemons survive a supervisor restart; dead ones are marked failed.
    async fn adopt_existing_labs(self: &Arc<Self>) {
        let entries: Vec<LabEntry> = self.registry.lock().await.labs().to_vec();
        for entry in entries {
            if entry.state != LabState::Running {
                continue;
            }
            let sock = crate::paths::lab_socket(&entry.name);
            match Client::connect(&sock).await {
                Ok(client) => {
                    if client.call("ping", Value::Null).await.is_ok() {
                        self.watch_lab_events(entry.name.clone()).await;
                        continue;
                    }
                    self.mark_crashed(&entry.name).await;
                }
                Err(_) => self.mark_crashed(&entry.name).await,
            }
        }
    }

    async fn mark_crashed(&self, lab: &str) {
        let mut reg = self.registry.lock().await;
        reg.set_state(lab, LabState::Failed);
        reg.save();
        drop(reg);
        self.emit(Event::new("lab.daemon_crashed", lab, Value::Null));
        tracing::warn!("lab daemon for {lab} is gone; marked failed");
    }

    /// The per-lab `ensure` lock, created on first use.
    async fn ensure_lock(&self, lab: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.ensure_locks.lock().await;
        locks
            .entry(lab.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Spawn the lab daemon for `name` if it isn't running; wait until its
    /// control socket answers. Returns the socket path.
    async fn ensure_lab(self: &Arc<Self>, name: &str, root: PathBuf) -> Result<PathBuf, String> {
        // Serialise per lab: a status poll and an `up` arriving together must
        // not both spawn the daemon and pre-pull templates in parallel.
        let lock = self.ensure_lock(name).await;
        let _guard = lock.lock().await;

        let sock = crate::paths::lab_socket(name);
        {
            let reg = self.registry.lock().await;
            if let Some(entry) = reg.get(name)
                && entry.state == LabState::Running
                && let Ok(c) = Client::connect(&sock).await
                && c.call("ping", Value::Null).await.is_ok()
            {
                return Ok(sock);
            }
        }

        // Pre-pull any registry templates the lab needs before the daemon
        // boots, streaming download progress to the UI's event feed (issue
        // #1). The daemon's own build re-resolves and pulls as a fallback, so
        // a failure here only costs the progress display, never the `up`.
        self.prepull_templates(name, &root).await;

        crate::paths::ensure_dir(sock.parent().expect("lab socket has parent"))
            .map_err(|e| e.to_string())?;
        let exe = crate::paths::self_exe().map_err(|e| e.to_string())?;
        let log_path = crate::paths::state_dir().join(format!("labd-{name}.log"));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
        let log_err = log.try_clone().map_err(|e| e.to_string())?;

        let child = tokio::process::Command::new(&exe)
            .arg("__labd")
            .arg("--lab")
            .arg(name)
            .arg("--root")
            .arg(&root)
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .spawn()
            .map_err(|e| format!("cannot spawn lab daemon: {e}"))?;
        let pid = child.id().unwrap_or_default();

        {
            let mut reg = self.registry.lock().await;
            reg.upsert(LabEntry {
                name: name.to_string(),
                root: root.clone(),
                pid,
                state: LabState::Running,
            });
            reg.save();
        }

        // Reap on exit: an exit we didn't ask for is a crash (PRD §3 — no
        // silent restart; mark failed + event).
        let sup = self.clone();
        let lab_name = name.to_string();
        tokio::spawn(async move {
            let mut child = child;
            let status = child.wait().await;
            let expected = {
                let reg = sup.registry.lock().await;
                reg.get(&lab_name)
                    .map(|e| e.state == LabState::Stopping)
                    .unwrap_or(true)
            };
            if expected {
                let mut reg = sup.registry.lock().await;
                reg.remove(&lab_name);
                reg.save();
            } else {
                tracing::warn!("lab daemon {lab_name} exited unexpectedly: {status:?}");
                sup.mark_crashed(&lab_name).await;
            }
        });

        // Wait for the control socket to come up. The daemon binds only
        // after `LabRuntime::build`, which on a first `up` may pull missing
        // templates from an OCI registry (PRD §6.4) — that can take minutes,
        // so a short fixed window is wrong. Instead allow a generous deadline
        // but bail immediately if the reaper marks the daemon Failed (or it
        // vanishes), so a genuine startup crash still reports fast.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1800);
        loop {
            if let Ok(c) = Client::connect(&sock).await
                && c.call("ping", Value::Null).await.is_ok()
            {
                self.watch_lab_events(name.to_string()).await;
                return Ok(sock);
            }
            {
                let reg = self.registry.lock().await;
                match reg.get(name) {
                    Some(entry) if entry.state == LabState::Failed => {
                        return Err(format!("lab daemon for {name} failed during startup"));
                    }
                    None => return Err(format!("lab daemon for {name} exited during startup")),
                    _ => {}
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("lab daemon for {name} did not come up"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Resolve and download every registry-backed template the lab references
    /// that isn't already cached, emitting `template.pull.{start,progress,done,
    /// error}` events as it goes so the web UI can show a download bar instead
    /// of an indefinite spinner (issue #1). Best-effort: config errors and pull
    /// failures are left for the lab daemon's build to surface properly.
    async fn prepull_templates(self: &Arc<Self>, lab: &str, root: &std::path::Path) {
        let config = match crate::config::load_lab_root(root) {
            Ok(c) => c,
            Err(_) => return,
        };
        let store = crate::template::TemplateStore::new(crate::paths::template_store_dir());
        for vm in &config.lab.vms {
            let crate::config::model::TemplateRef::Registry { reference } = &vm.template else {
                continue;
            };
            let Some(arch) = vm.arch.clone() else {
                continue;
            };
            let vm_name = vm.name.clone();
            let reference = reference.clone();
            self.emit(Event::new(
                "template.pull.start",
                lab,
                json!({"vm": vm_name, "reference": reference, "arch": arch}),
            ));

            let sup = self.clone();
            let lab_s = lab.to_string();
            let vm_s = vm_name.clone();
            let ref_s = reference.clone();
            let mut progress = move |p: crate::oci::PullProgress| {
                let percent = p
                    .bytes_done
                    .saturating_mul(100)
                    .checked_div(p.bytes_total)
                    .unwrap_or(0) as u32;
                sup.emit(Event::new(
                    "template.pull.progress",
                    lab_s.clone(),
                    json!({
                        "vm": vm_s,
                        "reference": ref_s,
                        "chunk": p.chunk,
                        "chunks": p.chunks,
                        "bytes_done": p.bytes_done,
                        "bytes_total": p.bytes_total,
                        "percent": percent,
                    }),
                ));
            };
            let result =
                crate::oci::ensure_registry_template(&reference, &arch, &store, &mut progress)
                    .await;
            drop(progress);

            match result {
                Ok(_) => self.emit(Event::new(
                    "template.pull.done",
                    lab,
                    json!({"vm": vm_name, "reference": reference}),
                )),
                Err(e) => self.emit(Event::new(
                    "template.pull.error",
                    lab,
                    json!({"vm": vm_name, "reference": reference, "error": format!("{e:#}")}),
                )),
            }
        }
    }

    /// Forward a lab daemon's events into the host-wide aggregate stream
    /// (PRD §8.2).
    async fn watch_lab_events(self: &Arc<Self>, lab: String) {
        let sock = crate::paths::lab_socket(&lab);
        let sup = self.clone();
        tokio::spawn(async move {
            let Ok(client) = Client::connect(&sock).await else {
                return;
            };
            let Ok(mut rx) = client.subscribe().await else {
                return;
            };
            while let Some(ev) = rx.recv().await {
                sup.emit(ev);
            }
        });
    }

    async fn release_lab(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let sock = crate::paths::lab_socket(name);
        {
            let mut reg = self.registry.lock().await;
            if reg.get(name).is_none() {
                return Ok(());
            }
            reg.set_state(name, LabState::Stopping);
            reg.save();
        }
        // A live daemon shuts down gracefully; its reaper drops the entry on
        // exit. A daemon that's already gone (e.g. a crashed/Failed lab) has no
        // reaper watching it, so remove the entry here — otherwise it would be
        // stuck in Stopping forever.
        if let Ok(client) = Client::connect(&sock).await {
            let _ = client.call("shutdown", Value::Null).await;
        } else {
            // The daemon is gone and can't have stopped its VMs. Reap any QEMU
            // processes it orphaned, then drop the registry entry.
            let killed = crate::qemu::process::kill_lab_orphans(name);
            if killed > 0 {
                tracing::warn!("reaped {killed} orphaned QEMU process(es) for lab {name}");
            }
            let mut reg = self.registry.lock().await;
            reg.remove(name);
            reg.save();
        }
        Ok(())
    }

    /// Restart a lab daemon so it re-reads `vmlab.wcl` from disk (the web UI's
    /// "reload" after editing the config). Stop the current daemon, wait for it
    /// to fully exit, then spawn a fresh one. Returns the new control socket.
    ///
    /// The caller is responsible for ensuring the lab is down (no running VMs):
    /// a restart drops the daemon's in-memory state, so a fresh daemon cannot
    /// re-adopt VMs the old one left running.
    async fn restart_lab(self: &Arc<Self>, name: &str, root: PathBuf) -> Result<PathBuf, String> {
        let registered = { self.registry.lock().await.get(name).is_some() };
        if registered {
            self.release_lab(name).await?;
            // Wait for the old daemon to fully exit before re-spawning. On a
            // clean shutdown the reaper removes the registry entry; a daemon
            // that was already dead was removed by `release_lab` directly.
            // Without this, `ensure_lab` could see the still-alive old daemon
            // (state Running + socket up) and hand back the stale socket.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if self.registry.lock().await.get(name).is_none() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!("lab daemon for {name} did not stop in time"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        self.ensure_lab(name, root).await
    }
}

/// The `lab` + `root` argument pair shared by lab-addressed commands.
fn lab_root_args(args: &Value) -> Result<(String, PathBuf), String> {
    let lab = args["lab"].as_str().ok_or("missing lab")?.to_string();
    let root = PathBuf::from(args["root"].as_str().ok_or("missing root")?);
    Ok((lab, root))
}

/// Wrapper giving command handlers access to `Arc<Supervisor>` (needed for
/// the tasks they spawn).
struct SupervisorHandler {
    sup: Arc<Supervisor>,
    /// The daemon's background tasks, cancelled + joined on `shutdown`.
    tasks: Arc<crate::lifecycle::TaskGroup>,
}

#[async_trait::async_trait]
impl Handler for SupervisorHandler {
    async fn handle(&self, cmd: &str, args: Value, _stream: &Streamer) -> Result<Value, String> {
        let sup = &self.sup;
        match cmd {
            "ping" => Ok(json!("pong")),
            "version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
            "status" => {
                let reg = sup.registry.lock().await;
                Ok(serde_json::to_value(reg.labs()).map_err(|e| e.to_string())?)
            }
            // Spawn (or find) the lab daemon for a lab; returns its socket.
            "lab.ensure" => {
                let name = args["name"].as_str().ok_or("missing name")?.to_string();
                let root = PathBuf::from(args["root"].as_str().ok_or("missing root")?);
                let sock = sup.ensure_lab(&name, root).await?;
                Ok(json!({"socket": sock}))
            }
            // Stop a lab daemon (after `down`/`destroy`).
            "lab.release" => {
                let name = args["name"].as_str().ok_or("missing name")?;
                sup.release_lab(name).await?;
                Ok(json!(true))
            }
            // Restart a lab daemon so it re-reads its config (web "reload").
            "lab.restart" => {
                let name = args["name"].as_str().ok_or("missing name")?.to_string();
                let root = PathBuf::from(args["root"].as_str().ok_or("missing root")?);
                let sock = sup.restart_lab(&name, root).await?;
                Ok(json!({"socket": sock}))
            }
            // Global segments (PRD §9.2): attach returns the trunk socket.
            "global.attach" => {
                let name = args["name"].as_str().ok_or("missing name")?;
                let subnet = match args["subnet"].as_str() {
                    Some(s) => Some(s.parse().map_err(|_| format!("bad subnet `{s}`"))?),
                    None => None,
                };
                let peer = args["peer"].as_str().map(String::from);
                let sock = sup
                    .globals
                    .attach(name, subnet, peer)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                Ok(json!({"socket": sock}))
            }
            "global.detach" => {
                let name = args["name"].as_str().ok_or("missing name")?;
                sup.globals.detach(name).await;
                Ok(json!(true))
            }
            "global.list" => {
                let list = sup.globals.list().await;
                Ok(json!(
                    list.into_iter()
                        .map(|(n, s, r)| json!({"name": n, "subnet": s, "refcount": r}))
                        .collect::<Vec<_>>()
                ))
            }
            // Template operations for the web Templates page (PRD §6). All
            // take `lab` + `root` like `lab.ensure`, so the supervisor works
            // for labs it never started.
            "template.list" => {
                let (lab, root) = lab_root_args(&args)?;
                templates::list(lab, root, sup.template_ops.clone()).await
            }
            "template.remote" => {
                let (_, root) = lab_root_args(&args)?;
                let template = args["template"].as_str().ok_or("missing template")?;
                templates::remote(root, template.to_string()).await
            }
            "template.build" => {
                let (lab, root) = lab_root_args(&args)?;
                let template = args["template"].as_str().ok_or("missing template")?;
                templates::start_build(sup.clone(), lab, root, template.to_string()).await
            }
            "template.push" => {
                let (lab, root) = lab_root_args(&args)?;
                let template = args["template"].as_str().ok_or("missing template")?;
                let version = args["version"].as_str().map(String::from);
                templates::start_push(sup.clone(), lab, root, template.to_string(), version).await
            }
            "template.op_status" => {
                let lab = args["lab"].as_str().ok_or("missing lab")?;
                Ok(sup.template_ops.status(lab))
            }
            "shutdown" => {
                tracing::info!("supervisor shutdown requested");
                let sup = sup.clone();
                let tasks = self.tasks.clone();
                tokio::spawn(async move {
                    let names: Vec<String> = {
                        let reg = sup.registry.lock().await;
                        reg.labs().iter().map(|l| l.name.clone()).collect()
                    };
                    for name in names {
                        let _ = sup.release_lab(&name).await;
                    }
                    // Cancel + join the supervisor's background tasks
                    // (disk watchdog) so exit is deterministic.
                    tasks.shutdown(std::time::Duration::from_secs(5)).await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    std::process::exit(0);
                });
                Ok(json!(true))
            }
            _ => Err(format!("unknown command `{cmd}`")),
        }
    }
}
