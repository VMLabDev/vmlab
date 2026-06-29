//! The per-user supervisor `vmlabd` (PRD §3): lab lifecycle, lab registry,
//! global segments, template store writes, host watchdogs, event
//! aggregation. Auto-started by the CLI; runs in the foreground (the CLI
//! detaches it into its own process group).

pub mod global;
pub mod registry;

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
    });

    let sock = crate::paths::supervisor_socket();
    let handler: Arc<dyn Handler> = Arc::new(SupervisorHandler(supervisor.clone()));
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
    crate::config::host::spawn_disk_watchdog(
        store_dir.clone(),
        host_cfg.disk_low_percent,
        std::time::Duration::from_secs(60),
        move |free| {
            sup_wd.emit(Event::new(
                "host.disk_low",
                "",
                serde_json::json!({"path": store_dir, "free_percent": free}),
            ));
        },
    );

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

    /// Spawn the lab daemon for `name` if it isn't running; wait until its
    /// control socket answers. Returns the socket path.
    async fn ensure_lab(self: &Arc<Self>, name: &str, root: PathBuf) -> Result<PathBuf, String> {
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

/// Wrapper giving command handlers access to `Arc<Supervisor>` (needed for
/// the tasks they spawn).
struct SupervisorHandler(Arc<Supervisor>);

#[async_trait::async_trait]
impl Handler for SupervisorHandler {
    async fn handle(&self, cmd: &str, args: Value, _stream: &Streamer) -> Result<Value, String> {
        let sup = &self.0;
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
            "shutdown" => {
                tracing::info!("supervisor shutdown requested");
                let sup = sup.clone();
                tokio::spawn(async move {
                    let names: Vec<String> = {
                        let reg = sup.registry.lock().await;
                        reg.labs().iter().map(|l| l.name.clone()).collect()
                    };
                    for name in names {
                        let _ = sup.release_lab(&name).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    std::process::exit(0);
                });
                Ok(json!(true))
            }
            _ => Err(format!("unknown command `{cmd}`")),
        }
    }
}
