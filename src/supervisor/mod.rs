//! The per-user supervisor `vmlabd` (PRD §3): lab lifecycle, lab registry,
//! global segments, template store writes, host watchdogs, event
//! aggregation. Auto-started by the CLI; runs in the foreground (the CLI
//! detaches it into its own process group).

pub mod global;
pub mod registry;
pub mod store;
pub mod templates;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::proto::client::LabClient;
use crate::proto::server::{Handler, Server, Streamer};
use crate::proto::{CommandError, Event, LabRequest, SupRequest};
use global::GlobalSegments;
use registry::{LabEntry, LabState, Registry};

pub struct Supervisor {
    registry: Mutex<Registry>,
    events: tokio::sync::broadcast::Sender<Event>,
    globals: Arc<GlobalSegments>,
    /// Per-lab locks serialising `ensure_lab`: without this, concurrent
    /// `lab.ensure` calls (a status poll plus an `up`, say) would each spawn
    /// the same daemon in parallel.
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
    // Private: this directory holds every control and VNC socket on the host.
    crate::paths::ensure_private_dir(&runtime_dir)?;
    crate::paths::ensure_dir(&crate::paths::state_dir())?;

    let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
    // Select the network fast-path tier (PRD §9.1) before any switch exists —
    // the global segments this daemon hosts pick it up from here.
    crate::net::fastpath::init(host_cfg.fastpath);
    // One aggregate event channel, created before the socket binds so the
    // global-segment trunks can emit through it from day one.
    let (events_tx, _) = tokio::sync::broadcast::channel::<Event>(1024);
    let supervisor = Arc::new(Supervisor {
        registry: Mutex::new(Registry::load()),
        events: events_tx.clone(),
        globals: GlobalSegments::new(
            host_cfg.dns_suffix.clone(),
            host_cfg.psk.clone(),
            events_tx.clone(),
        ),
        ensure_locks: Mutex::new(std::collections::HashMap::new()),
        template_ops: templates::TemplateOps::default(),
    });

    // Long-lived background tasks register here so the `shutdown` command
    // can cancel and join them deterministically.
    let tasks = Arc::new(crate::lifecycle::TaskGroup::new());

    let sock = crate::paths::supervisor_socket();
    let handler: Arc<dyn Handler<SupRequest>> = Arc::new(SupervisorHandler {
        sup: supervisor.clone(),
        tasks: tasks.clone(),
    });
    let server = Server::bind_with_events(&sock, handler, events_tx)
        .await
        .with_context(|| format!("binding {}", sock.display()))?;

    // Cross-host trunk listener (PRD §9.2): enabled when a PSK is configured.
    // The port comes from host config (`trunk_port`, default 13947) so two
    // instances on one machine can listen side by side; peers address it as
    // `host:port` in their segment `connect {}` blocks.
    if let Some(psk) = &host_cfg.psk {
        let bind: std::net::SocketAddr = ([0, 0, 0, 0], host_cfg.trunk_port).into();
        match global::bind_peer_listener(supervisor.globals.clone(), bind, psk.clone()).await {
            Ok((_addr, _task)) => {}
            Err(e) => tracing::error!("{e:#}"),
        }
    }

    tracing::info!("vmlabd listening on {}", sock.display());
    supervisor.adopt_existing_labs().await;

    // A build that was in flight when the last supervisor died is not resumed
    // (ADR-0010) and its working disk is nobody's — the workdir guard is a
    // `Drop`, and a killed process runs none. Nothing owns a build at this
    // point, so anything still here is a leftover.
    match crate::template::build::sweep_build_workdirs() {
        0 => {}
        swept => {
            tracing::info!("cleared {swept} build working director(y|ies) left by a previous run")
        }
    }

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

    // A termination signal runs the same teardown as the `shutdown` command,
    // so a `systemctl stop` or host shutdown releases the lab daemons (which
    // in turn stop their VMs) instead of leaving them running unmanaged.
    crate::lifecycle::termination_signal().await;
    tracing::info!("vmlabd caught a termination signal; releasing labs");
    teardown(&supervisor, &tasks).await;
    drop(server);
    Ok(())
}

/// Release every lab daemon (each stops its own machines) and join the
/// supervisor's background tasks.
async fn teardown(sup: &Arc<Supervisor>, tasks: &crate::lifecycle::TaskGroup) {
    let names: Vec<String> = {
        let reg = sup.registry.lock().await;
        reg.labs().iter().map(|l| l.name.clone()).collect()
    };
    for name in names {
        let _ = sup.release_lab(&name).await;
    }
    tasks.shutdown(std::time::Duration::from_secs(5)).await;
}

impl Supervisor {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Reconnect registry entries from a previous supervisor run: lab
    /// daemons survive a supervisor restart; dead ones are marked failed.
    ///
    /// Also the one place stale entries are pruned. A `failed` entry is kept so
    /// the user sees that the lab crashed, but once its lab file is gone the
    /// entry is only noise in `vmlab status` and the web lab list — those
    /// accumulate forever otherwise.
    async fn adopt_existing_labs(self: &Arc<Self>) {
        let entries: Vec<LabEntry> = self.registry.lock().await.labs().to_vec();
        for entry in entries {
            if entry.state != LabState::Running {
                if !entry.root.join(crate::paths::LAB_FILE).is_file() {
                    tracing::info!(
                        "dropping registry entry for {} — {} is gone",
                        entry.name,
                        entry.root.display()
                    );
                    let mut reg = self.registry.lock().await;
                    reg.remove(&entry.name);
                    reg.save();
                }
                continue;
            }
            let sock = crate::paths::lab_socket(&entry.name);
            match LabClient::connect(&sock).await {
                Ok(client) => {
                    if client.send(LabRequest::Ping {}).await.is_ok() {
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
    async fn ensure_lab(
        self: &Arc<Self>,
        name: &str,
        root: PathBuf,
    ) -> Result<PathBuf, CommandError> {
        let root = registry::canonical_root(&root)?;
        // Serialise per lab: a status poll and an `up` arriving together must
        // not both spawn the daemon in parallel.
        let lock = self.ensure_lock(name).await;
        let _guard = lock.lock().await;

        self.ensure_lab_locked(name, root).await
    }

    /// The already-serialised half of [`Self::ensure_lab`], also used after a
    /// restart has kept the same name lock across identity check and release.
    async fn ensure_lab_locked(
        self: &Arc<Self>,
        name: &str,
        root: PathBuf,
    ) -> Result<PathBuf, CommandError> {
        let sock = crate::paths::lab_socket(name);
        {
            let reg = self.registry.lock().await;
            reg.check_name(name, &root)?;
            if let Some(entry) = reg.get(name)
                && entry.state == LabState::Running
                && let Ok(c) = LabClient::connect(&sock).await
                && c.send(LabRequest::Ping {}).await.is_ok()
            {
                return Ok(sock);
            }
        }

        // Note: templates are NOT pre-pulled here. The daemon's build binds
        // cached templates offline and defers missing ones; the lab daemon
        // downloads them at up/start/`pull` time, streaming the same
        // `template.pull.*` progress events through its event log (which
        // `watch_lab_events` below forwards into the aggregate feed).

        crate::paths::ensure_private_dir(sock.parent().expect("lab socket has parent"))
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
            })?;
            reg.save();
        }

        // Reap on exit: an exit we didn't ask for is a crash (PRD §3 — no
        // silent restart; mark failed + event).
        let sup = self.clone();
        let lab_name = name.to_string();
        tokio::spawn(async move {
            let mut child = child;
            let status = child.wait().await;
            // Clean exit code = the daemon completed its own teardown, whether
            // we asked for it (`Stopping`) or a signal did. Only a non-zero or
            // signalled exit is a crash.
            let exited_cleanly = status.as_ref().is_ok_and(|s| s.success());
            let expected = exited_cleanly || {
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

        // Wait for the control socket to come up. Build is fully offline now
        // (missing templates defer to pull-on-up), so startup is quick; the
        // deadline only covers slow hosts. Bail immediately if the reaper
        // marks the daemon Failed (or it vanishes), so a genuine startup
        // crash still reports fast.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if let Ok(c) = LabClient::connect(&sock).await
                && c.send(LabRequest::Ping {}).await.is_ok()
            {
                self.watch_lab_events(name.to_string()).await;
                return Ok(sock);
            }
            {
                let reg = self.registry.lock().await;
                match reg.get(name) {
                    Some(entry) if entry.state == LabState::Failed => {
                        return Err(CommandError::failed(format!(
                            "lab daemon for {name} failed during startup"
                        )));
                    }
                    None => {
                        return Err(CommandError::failed(format!(
                            "lab daemon for {name} exited during startup"
                        )));
                    }
                    _ => {}
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(CommandError::failed(format!(
                    "lab daemon for {name} did not come up"
                )));
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
            let Ok(client) = LabClient::connect(&sock).await else {
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
        // Captured before the entry can be removed — the orphan reaper needs it
        // to recognise this lab's smbd.
        let root;
        {
            let mut reg = self.registry.lock().await;
            let Some(entry) = reg.get(name) else {
                return Ok(());
            };
            root = entry.root.clone();
            reg.set_state(name, LabState::Stopping);
            reg.save();
        }
        // A live daemon shuts down gracefully; its reaper drops the entry on
        // exit. A daemon that's already gone (e.g. a crashed/Failed lab) has no
        // reaper watching it, so remove the entry here — otherwise it would be
        // stuck in Stopping forever.
        if let Ok(client) = LabClient::connect(&sock).await {
            let _ = client.send(LabRequest::Shutdown {}).await;
        } else {
            // The daemon is gone and can't have stopped anything it owned. Reap
            // the QEMU processes AND the helpers it orphaned (swtpm, virtiofsd,
            // smbd — an orphaned smbd holds its port against the next `up`),
            // then drop the registry entry.
            let killed = crate::qemu::process::kill_lab_orphans(name, Some(&root));
            if killed > 0 {
                tracing::warn!("reaped {killed} orphaned process(es) for lab {name}");
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
    async fn restart_lab(
        self: &Arc<Self>,
        name: &str,
        root: PathBuf,
    ) -> Result<PathBuf, CommandError> {
        let root = registry::canonical_root(&root)?;
        let lock = self.ensure_lock(name).await;
        let _guard = lock.lock().await;

        let registered = {
            let registry = self.registry.lock().await;
            registry.check_name(name, &root)?;
            registry.get(name).is_some()
        };
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
                    return Err(CommandError::failed(format!(
                        "lab daemon for {name} did not stop in time"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        self.ensure_lab_locked(name, root).await
    }
}

/// Wrapper giving command handlers access to `Arc<Supervisor>` (needed for
/// the tasks they spawn).
struct SupervisorHandler {
    sup: Arc<Supervisor>,
    /// The daemon's background tasks, cancelled + joined on `shutdown`.
    tasks: Arc<crate::lifecycle::TaskGroup>,
}

#[async_trait::async_trait]
impl Handler<SupRequest> for SupervisorHandler {
    async fn handle(&self, req: SupRequest, _stream: &Streamer) -> Result<Value, CommandError> {
        let sup = &self.sup;
        match req {
            SupRequest::Ping {} => Ok(json!("pong")),
            SupRequest::Version {} => Ok(json!(env!("CARGO_PKG_VERSION"))),
            // Which network fast-path tier this daemon selected (PRD §9.1),
            // plus why the skipped kernel tiers were unavailable.
            SupRequest::FastPath {} => Ok(crate::net::fastpath::status_json()),
            SupRequest::Status {} => {
                let reg = sup.registry.lock().await;
                serde_json::to_value(reg.labs()).map_err(|e| CommandError::internal(e.to_string()))
            }
            // Spawn (or find) the lab daemon for a lab; returns its socket.
            SupRequest::LabEnsure { name, root } => {
                let sock = sup.ensure_lab(&name, root).await?;
                Ok(json!({"socket": sock}))
            }
            // Stop a lab daemon (after `down`/`destroy`).
            SupRequest::LabRelease { name } => {
                sup.release_lab(&name).await?;
                Ok(json!(true))
            }
            // Restart a lab daemon so it re-reads its config (web "reload").
            SupRequest::LabRestart { name, root } => {
                let sock = sup.restart_lab(&name, root).await?;
                Ok(json!({"socket": sock}))
            }
            // Global segments (PRD §9.2): attach returns the trunk socket.
            SupRequest::GlobalAttach { name, subnet, peer } => {
                let sock = sup.globals.attach(&name, subnet, peer).await?;
                Ok(json!({"socket": sock}))
            }
            SupRequest::GlobalDetach { name } => {
                sup.globals.detach(&name).await;
                Ok(json!(true))
            }
            SupRequest::GlobalList {} => Ok(json!(sup.globals.list().await)),
            // Template operations for the web Templates page (PRD §6). All
            // take `lab` + `root` like `lab.ensure`, so the supervisor works
            // for labs it never started.
            SupRequest::TemplateList { lab, root, file } => {
                templates::list(lab, root, file, sup.template_ops.clone()).await
            }
            SupRequest::TemplateRemote {
                lab: _,
                root,
                template,
                arch,
            } => templates::remote(root, template, arch).await,
            SupRequest::TemplateBuild {
                lab,
                root,
                template,
                arch,
                version,
                file,
            } => {
                templates::start_build(sup.clone(), lab, root, template, arch, version, file).await
            }
            SupRequest::TemplateStopBuild {
                lab,
                arch,
                template,
            } => templates::stop_build(sup.clone(), lab, arch, template),
            SupRequest::TemplatePush {
                lab,
                root,
                template,
                arch,
                version,
            } => templates::start_push(sup.clone(), lab, root, template, arch, version).await,
            SupRequest::TemplateOpStatus { lab } => Ok(sup.template_ops.status(&lab)),
            SupRequest::TemplateConsolePath {
                lab,
                arch,
                template,
            } => {
                let path = sup.template_ops.console_path(&lab, &arch, &template)?;
                Ok(json!(path.to_string_lossy()))
            }
            // The template store and its registries (PRD §3, §6): the
            // supervisor is the only thing that opens either.
            SupRequest::StoreList { remote } => store::list(remote).await,
            SupRequest::StoreRemove { reference, force } => store::remove(reference, force).await,
            SupRequest::StorePrune {
                filter,
                keep,
                apply,
                force,
            } => store::prune(filter, keep, apply, force).await,
            SupRequest::StoreExport { reference, out } => store::export(reference, out).await,
            SupRequest::StoreImport { archive, overwrite } => {
                store::import(archive, overwrite).await
            }
            SupRequest::StorePull {
                target,
                arch,
                overwrite,
            } => store::pull(target, arch, overwrite).await,
            SupRequest::StorePush {
                reference,
                target,
                source,
                prerelease,
                lab,
            } => store::push(sup.clone(), reference, target, source, prerelease, lab).await,
            SupRequest::StoreStopPush {
                lab,
                arch,
                template,
            } => store::stop_push(sup.clone(), lab, arch, template),
            SupRequest::RegistrySearch {
                query,
                namespace,
                arch,
                containers,
            } => store::search(query, namespace, arch, containers).await,
            SupRequest::RegistryLogin {
                registry,
                username,
                password,
            } => store::login(registry, username, password).await,
            SupRequest::RegistryNamespaces {} => store::namespaces(),
            SupRequest::RegistryNamespaceAdd { namespace, use_for } => {
                store::namespace_add(namespace, use_for)
            }
            SupRequest::RegistryNamespaceRemove { namespace } => store::namespace_remove(namespace),
            SupRequest::Shutdown {} => {
                tracing::info!("supervisor shutdown requested");
                let sup = sup.clone();
                let tasks = self.tasks.clone();
                // Spawned so this command's response reaches the caller before
                // the process goes away.
                tokio::spawn(async move {
                    teardown(&sup, &tasks).await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    std::process::exit(0);
                });
                Ok(json!(true))
            }
        }
    }
}
