//! Template builds (PRD §6.1): create a working qcow2, boot it per the
//! template's hardware, run wscript build provision scripts, seal, and move the
//! image + metadata into the store. A failed build leaves nothing behind.
//!
//! A build is modelled as a one-VM `scratch` lab whose primary disk is
//! pre-seeded from the source, so it reuses the entire lab runtime
//! (lifecycle, networking, the wscript build scripts). The build runs
//! in-process — no daemon — and seals by flattening the working disk into
//! the store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::meta::TemplateMeta;
use super::store::TemplateStore;
use crate::config::model::{ArtefactSource, TemplateDef, TemplateSource};
use crate::scripting::OutputSink;

/// Called once the build VM's VNC socket is accepting connections.
pub type ConsoleReady = Arc<dyn Fn(PathBuf) + Send + Sync>;

/// Called for every structured event the synthetic build lab emits whose
/// kind starts with `playbook.` — the config-weave step stream (§10.4). The
/// supervisor forwards these as `template.op.step` so build UIs can render
/// per-step progress instead of opaque log lines.
pub type BuildEvent = Arc<dyn Fn(crate::proto::Event) + Send + Sync>;

/// Optional controls supplied by an interactive build caller.
#[derive(Default)]
pub struct BuildControl {
    pub console_ready: Option<ConsoleReady>,
    pub on_event: Option<BuildEvent>,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Build `def` (from a parsed lab/template file rooted at `root`) and install
/// the result into `store`. `log` streams progress. The build version is
/// auto-incremented (PRD §6.4) unless `version_override` pins it.
pub async fn build_template(
    def: &TemplateDef,
    root: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: OutputSink,
    version_override: Option<&str>,
    control: BuildControl,
) -> Result<TemplateMeta> {
    let version = match version_override {
        Some(v) => v.to_string(),
        None => cancelable(&control.cancel, next_version(def, store, &log)).await?,
    };
    log(format!("building {}/{}@{}\n", def.arch, def.name, version));

    if store.exists(&def.arch, &def.name, Some(&version)) {
        bail!(
            "{}/{}@{} already in the store — remove it first or pick another version",
            def.arch,
            def.name,
            version
        );
    }

    // Working area: a throwaway lab root under the artefact cache. Removed on
    // both success and failure, so nothing leaks.
    let work = build_workdir(def);
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let guard = WorkdirGuard(work.clone());

    let result = run_build(
        def,
        root,
        &work,
        store,
        profiles,
        &log,
        BuildRun {
            version: &version,
            control,
        },
    )
    .await;
    drop(guard); // always clean up the workdir
    result
}

/// Pick the next build version (PRD §6.4). The declared `version` is a fixed
/// prefix (the upstream/OS identity); vmlab appends a trailing build counter,
/// `<declared>.<N>`, where N is the highest existing `<declared>.<N>` plus one,
/// or 0 if none exist yet. Existing builds come from the template's registry
/// tags when it has a `registry` (falling back to the local store), so the
/// counter continues across machines. Changing the declared prefix (e.g. a new
/// Windows build number) restarts the counter at `.0`.
async fn next_version(
    def: &TemplateDef,
    store: &TemplateStore,
    log: &OutputSink,
) -> Result<String> {
    let mut existing: Vec<String> = Vec::new();
    let mut source = "fresh";

    let mut from_registry = false;
    if let Some(repo) = &def.registry {
        match list_registry_versions(repo).await {
            Ok(tags) => {
                from_registry = true;
                existing = tags;
                source = "registry";
            }
            Err(e) => log(format!(
                "warning: could not read registry tags from {repo} ({e:#}); \
                 falling back to the local store\n"
            )),
        }
    }

    // Fall back to the local store only when the registry wasn't consulted.
    if !from_registry && let Ok(local) = store.versions_of(&def.arch, &def.name) {
        existing = local;
        source = "local store";
    }

    let next = super::store::next_subbuild(&def.version, &existing);
    log(format!(
        "auto-version: {next} (prefix {}, {source})\n",
        def.version
    ));
    Ok(next)
}

/// Fetch the concrete version tags published under `repo` (excludes moving
/// aliases like `latest` / `latest-prerelease`, which do not start with a
/// digit).
async fn list_registry_versions(repo: &str) -> Result<Vec<String>> {
    let registry = crate::oci::Registry::new(repo)?;
    let tags = registry.list_tags().await?;
    Ok(tags
        .into_iter()
        .filter(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .collect())
}

struct WorkdirGuard(PathBuf);
impl Drop for WorkdirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn build_workdir(def: &TemplateDef) -> PathBuf {
    super::artefact::cache_dir()
        .parent()
        .unwrap_or(&super::artefact::cache_dir())
        .join("builds")
        .join(format!("{}-{}-{}", def.arch, def.name, def.version))
}

async fn run_build(
    def: &TemplateDef,
    root: &Path,
    work: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: &OutputSink,
    run: BuildRun<'_>,
) -> Result<TemplateMeta> {
    let version = run.version;
    let disk_size = def.disk.unwrap_or(20 << 30);
    let build_vm = "build";

    // Resolve the source into the working primary disk. A layered source's
    // embedded first-boot provision must gate the build boot exactly as it
    // gates a clone (PRD §6.1): a sysprep-generalized Windows source replays
    // specialize/OOBE on this boot, and the agent answers while that is
    // still running — sealing then would capture a half-specialized image.
    let mut source_first_boot: Option<String> = None;
    let (cdrom, seed_disk): (Option<PathBuf>, SeedDisk) = match &def.source {
        TemplateSource::Iso(src) => {
            let iso = cancelable(&run.control.cancel, resolve_artefact(src, root, log)).await?;
            (Some(iso), SeedDisk::Blank(disk_size))
        }
        TemplateSource::Qcow2(src) => {
            let img = cancelable(&run.control.cancel, resolve_artefact(src, root, log)).await?;
            (None, SeedDisk::CopyFrom(img))
        }
        TemplateSource::Template { from, .. } => {
            let arch_name_ver = match from {
                crate::config::model::TemplateRef::Store {
                    arch,
                    name,
                    version,
                } => (arch.clone(), name.clone(), version.clone()),
                _ => bail!("layered build source must be a store reference"),
            };
            let resolved = store
                .resolve(
                    &arch_name_ver.0,
                    &arch_name_ver.1,
                    arch_name_ver.2.as_deref(),
                )
                .context("resolving layered build source")?;
            source_first_boot = resolved.meta.first_boot_script.clone();
            (None, SeedDisk::CopyFrom(resolved.disk_path))
        }
        TemplateSource::Scratch { .. } => (None, SeedDisk::Blank(disk_size)),
    };

    // Stage the VMLAB bootstrap ISO (agent binaries + install scripts): the
    // guest's own unattended install runs it, so the agent exists before any
    // host channel does. Skipped when the template opts out or the profile
    // has no agent channel (vintage guests would just carry a dead ISO).
    let profile_channel = profiles
        .get(def.profile.as_deref().unwrap_or("linux-generic"))
        .map(|p| p.agent_channel)
        .unwrap_or(true);
    let wants_agent = def.agent && profile_channel;
    let staged: Option<Arc<super::bootstrap::StagedGuestIso>> = if wants_agent {
        Some(Arc::new(super::bootstrap::stage_guest_iso_dir(
            work, &def.arch,
        )?))
    } else {
        None
    };

    // Synthesize a one-VM scratch lab for the build.
    let lab_name = format!("build-{}", def.name);
    let lab_wcl = synth_lab(
        def,
        &lab_name,
        build_vm,
        cdrom.as_deref(),
        root,
        staged.as_ref().map(|s| s.dir.as_path()),
        true,
    )?;
    std::fs::write(work.join("vmlab.wcl"), &lab_wcl)?;

    let labfile = crate::config::load_lab_source(&lab_wcl, "<build>", work)
        .map_err(|e| anyhow::anyhow!("internal build lab invalid: {e:?}"))?;

    // Build the runtime; then pre-seed the working disk before `up` creates
    // the (otherwise blank) scratch disk.
    let (events_tx, _) = tokio::sync::broadcast::channel::<crate::proto::Event>(256);
    // Bridge the synthetic lab's structured events out to the caller: the
    // playbook engine narrates step progress as `playbook.op.*` (incl. the
    // raw config-weave ndjson on `playbook.op.step`), which would otherwise
    // be discarded with this receiver.
    if let Some(on_event) = run.control.on_event.clone() {
        let mut rx = events_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev.event.starts_with("playbook.") {
                            on_event(ev);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
    let event_log = Arc::new(crate::labd::events::EventLog::new(&lab_name, events_tx)?);
    let runtime = cancelable(
        &run.control.cancel,
        crate::labd::lab::LabRuntime::build(labfile, event_log, profiles),
    )
    .await?;

    // Verify the vmlab-agent before the build seals. The install itself is
    // guest-driven from the VMLAB ISO staged above; this side only waits for
    // the agent's handshake and records the verified version. HOW the wait
    // runs depends on the source:
    //
    // - Layered/qcow2: the image boots an installed OS whose unattended hook
    //   (or an already-baked agent) answers promptly (bounded by a layered
    //   source's first-boot pass) — verify as a blocking pre-provision hook,
    //   before any provision script.
    // - ISO/scratch: the provisions themselves drive the installer from the
    //   first keystroke, so a blocking hook deadlocks — the agent only
    //   exists once the unattended installer has laid down the OS. Verify
    //   concurrently instead: watch for the handshake in the background.
    //   Installers that power off from a live environment (subiquity) never
    //   hand one over — those builds get a verification boot below.
    let vm = runtime.vm(build_vm)?;
    let agent_version: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    let verify_concurrent = matches!(
        def.source,
        TemplateSource::Iso(_) | TemplateSource::Scratch { .. }
    );
    let verify_task: Option<tokio::task::JoinHandle<Result<Option<String>>>> = if verify_concurrent
    {
        let vm = vm.clone();
        let wants_agent = def.agent;
        let staged = staged.clone();
        let out = log.clone();
        Some(tokio::spawn(async move {
            // Wait for `up` to actually start the VM, capped so a build that
            // dies before boot doesn't strand this task.
            let started = tokio::time::Instant::now();
            while vm.state().await == crate::labd::vm::PowerState::Stopped {
                if started.elapsed() > std::time::Duration::from_secs(600) {
                    bail!("build VM never started");
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let log = move |s: String| out(s);
            super::agent_install::verify(
                &vm,
                wants_agent,
                staged.as_deref(),
                std::time::Duration::from_secs(3600),
                &log,
            )
            .await
        }))
    } else {
        let wants_agent = def.agent;
        let staged = staged.clone();
        let agent_version = agent_version.clone();
        *runtime.pre_provision.write().expect("pre_provision lock") =
            Some(Arc::new(move |vm, out| {
                let staged = staged.clone();
                let agent_version = agent_version.clone();
                Box::pin(async move {
                    let log = move |s: String| out(s);
                    let version = super::agent_install::verify(
                        &vm,
                        wants_agent,
                        staged.as_deref(),
                        std::time::Duration::from_secs(600),
                        &log,
                    )
                    .await?;
                    *agent_version.lock().expect("agent_version lock") = version;
                    Ok(())
                })
            }));
        None
    };

    // Carry the layered source's first-boot provision onto the build VM.
    // The build seeds the working disk directly (below) instead of cloning
    // through the store, so the script would otherwise be lost — and `up()`
    // runs it before the agent bake and any build provisions.
    if let Some(script) = source_first_boot {
        let parts = vm.template();
        vm.set_template(crate::labd::vm::TemplateParts {
            resolved: parts.resolved.clone(),
            backing: parts.backing.clone(),
            disk_size: parts.disk_size,
            first_boot_script: Some(script),
            agent_version: parts.agent_version.clone(),
        });
    }

    let disk0 = vm.dirs.primary_disk();
    std::fs::create_dir_all(disk0.parent().unwrap())?;
    match &seed_disk {
        SeedDisk::Blank(size) => {
            cancelable(
                &run.control.cancel,
                super::qimg::create_blank(&disk0, *size),
            )
            .await?;
        }
        SeedDisk::CopyFrom(src) => {
            log(format!("seeding working disk from {}\n", src.display()));
            // Flatten/copy into a standalone working qcow2 (resized up to the
            // requested disk size if larger).
            cancelable(
                &run.control.cancel,
                super::qimg::convert_to_qcow2(src, &disk0),
            )
            .await?;
            if def.disk.is_some() {
                let info = super::qimg::image_info(&disk0).await?;
                if disk_size > info.virtual_size {
                    super::qimg::resize(&disk0, disk_size).await?;
                }
            }
        }
    }

    // Boot + run build provision scripts (PRD §6.1, §10.4).
    log("booting build VM\n".to_string());
    // `gui = true` builds get a viewer once QEMU creates the VNC socket;
    // up() below blocks through provisioning, so this watches concurrently.
    if def.gui {
        crate::viewer::open_when_ready(vm.dirs.vnc_sock());
    }
    let console_watch = run.control.console_ready.map(|ready| {
        let sock = vm.dirs.vnc_sock();
        tokio::spawn(async move {
            loop {
                if sock.exists() {
                    ready(sock);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
    });
    let up_result = tokio::select! {
        result = runtime.up(&[], log.clone()) => Some(result),
        () = run.control.cancel.cancelled() => None,
    };
    if let Some(watch) = console_watch {
        watch.abort();
    }
    // Through graceful shutdown as one fallible step: a failed boot,
    // provision or seal must stop the build VM (QEMU/swtpm outlive the CLI
    // otherwise), not just delete its workdir.
    let booted = async {
        match up_result {
            Some(result) => result.context("build boot/provision failed")?,
            None => bail!("build cancelled"),
        }
        // In a live-handshake build (Windows FirstLogonCommands run while
        // the VM is still up) the concurrent verify finished minutes ago —
        // give a near-complete one a short grace. An installer that powered
        // the VM off from a throwaway live session (subiquity) can never
        // complete it: the agent is installed into the target but has not
        // run yet, so abort and boot the installed system for verification
        // below.
        let mut verify_boot = false;
        if let Some(task) = verify_task {
            let grace = if vm.state().await == crate::labd::vm::PowerState::Stopped {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs(60)
            };
            let abort = task.abort_handle();
            match tokio::time::timeout(grace, task).await {
                Ok(joined) => {
                    let version = joined
                        .map_err(|e| anyhow::anyhow!("agent verify task panicked: {e}"))??;
                    *agent_version.lock().expect("agent_version lock") = version;
                }
                Err(_) => {
                    abort.abort();
                    verify_boot = wants_agent;
                }
            }
        }
        log("sealing: graceful shutdown\n".to_string());
        vm.stop(false).await.context("build VM did not shut down")?;
        vm.wait_state(
            crate::labd::vm::PowerState::Stopped,
            std::time::Duration::from_secs(120),
        )
        .await?;
        Ok(verify_boot)
    };
    let needs_verify_boot = match booted.await {
        Ok(v) => v,
        Err(e) => {
            let _ = runtime.down(&[], true).await;
            return Err(e);
        }
    };

    if run.control.cancel.is_cancelled() {
        bail!("build cancelled");
    }

    // Free the build runtime's sockets/watchers before any verification
    // boot reuses the same lab dirs; keep the event log for the final emit.
    let events = runtime.events.clone();
    drop(runtime);

    // Boot the installed system once — same workdir and disk, no installer
    // media, no provisions — verify the agent handshake, and shut down
    // again. Only reached when the installer could not hand a live
    // handshake (see above); this also means every sealed ISO build was
    // verified against the *installed* OS, never the live installer
    // session.
    if needs_verify_boot {
        log("agent: verification boot (installer sealed without a live handshake)\n".to_string());
        let verify_wcl = synth_lab(def, &lab_name, build_vm, None, root, None, false)?;
        let labfile = crate::config::load_lab_source(&verify_wcl, "<verify>", work)
            .map_err(|e| anyhow::anyhow!("internal verification lab invalid: {e:?}"))?;
        let (events_tx, _) = tokio::sync::broadcast::channel::<crate::proto::Event>(256);
        let event_log = Arc::new(crate::labd::events::EventLog::new(&lab_name, events_tx)?);
        let runtime2 = cancelable(
            &run.control.cancel,
            crate::labd::lab::LabRuntime::build(labfile, event_log, profiles),
        )
        .await?;
        let vm2 = runtime2.vm(build_vm)?;
        let verified = async {
            let up = tokio::select! {
                result = runtime2.up(&[], log.clone()) => Some(result),
                () = run.control.cancel.cancelled() => None,
            };
            match up {
                Some(result) => result.context("verification boot failed")?,
                None => bail!("build cancelled"),
            }
            let vlog = {
                let out = log.clone();
                move |s: String| out(s)
            };
            let version = super::agent_install::verify(
                vm2,
                def.agent,
                staged.as_deref(),
                std::time::Duration::from_secs(900),
                &vlog,
            )
            .await?;
            log("sealing: graceful shutdown (verification boot)\n".to_string());
            vm2.stop(false)
                .await
                .context("verification boot VM did not shut down")?;
            vm2.wait_state(
                crate::labd::vm::PowerState::Stopped,
                std::time::Duration::from_secs(120),
            )
            .await?;
            Ok::<_, anyhow::Error>(version)
        }
        .await;
        match verified {
            Ok(version) => *agent_version.lock().expect("agent_version lock") = version,
            Err(e) => {
                let _ = runtime2.down(&[], true).await;
                return Err(e);
            }
        }
    }

    // Seal: flatten the working disk into a staging dir, then install.
    let staging = work.join("staging");
    std::fs::create_dir_all(&staging)?;
    let sealed = staging.join("disk.qcow2");
    log("flattening sealed image\n".to_string());
    super::qimg::convert_to_qcow2(&disk0, &sealed).await?;

    let info = super::qimg::image_info(&sealed).await?;
    let sha = super::store::sha256_file(&sealed).context("hashing sealed image")?;
    // Embed the first-boot provision script (run on first instantiation, before
    // ready). It rides in the metadata, so the file is read relative to the
    // template root and baked in here (PRD §6.1).
    let first_boot_script = match &def.first_boot {
        Some(path) => {
            let full = root.join(path);
            Some(
                std::fs::read_to_string(&full)
                    .with_context(|| format!("reading first-boot script {}", full.display()))?,
            )
        }
        None => None,
    };
    let meta = TemplateMeta {
        name: def.name.clone(),
        arch: def.arch.clone(),
        version: version.to_string(),
        profile: def.profile.clone(),
        cpus: def.cpus,
        memory: def.memory,
        disk: Some(info.virtual_size),
        firmware: def.firmware.map(|f| match f {
            crate::config::model::Firmware::Ovmf => "ovmf".into(),
            crate::config::model::Firmware::Seabios => "seabios".into(),
        }),
        tpm: def.tpm,
        secure_boot: def.secure_boot,
        display: def.display.clone(),
        created: chrono::Utc::now(),
        origin: source_origin(&def.source),
        registry: def.registry.clone(),
        sha256: Some(sha),
        first_boot_script,
        agent_version: agent_version.lock().expect("agent_version lock").clone(),
    };

    store
        .install(&staging, &meta, false)
        .context("installing into the store")?;
    log(format!(
        "installed {}/{}@{}\n",
        meta.arch, meta.name, meta.version
    ));
    events.emit(
        "template.built",
        serde_json::json!({
            "arch": meta.arch, "name": meta.name, "version": meta.version,
        }),
    );
    Ok(meta)
}

enum SeedDisk {
    Blank(u64),
    CopyFrom(PathBuf),
}

struct BuildRun<'a> {
    version: &'a str,
    control: BuildControl,
}

async fn cancelable<T, E>(
    cancel: &tokio_util::sync::CancellationToken,
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T>
where
    E: Into<anyhow::Error>,
{
    tokio::select! {
        result = future => result.map_err(Into::into),
        () = cancel.cancelled() => bail!("build cancelled"),
    }
}

async fn resolve_artefact(src: &ArtefactSource, root: &Path, log: &OutputSink) -> Result<PathBuf> {
    let log = log.clone();
    // A local `path` source is relative to the template dir (like media /
    // provision paths), but the build runs from a separate work dir — rebase
    // relative paths onto `root` so QEMU can find them.
    let rebased = match src {
        ArtefactSource::Path { path, span } if path.is_relative() => Some(ArtefactSource::Path {
            path: root.join(path),
            span: *span,
        }),
        _ => None,
    };
    let src = rebased.as_ref().unwrap_or(src);
    super::artefact::resolve(src, move |m| log(format!("{m}\n"))).await
}

fn source_origin(source: &TemplateSource) -> Option<String> {
    match source {
        TemplateSource::Iso(ArtefactSource::Url { url, .. })
        | TemplateSource::Qcow2(ArtefactSource::Url { url, .. }) => Some(url.clone()),
        TemplateSource::Template { from, .. } => Some(from.to_string()),
        _ => None,
    }
}

/// Render the synthetic build lab. The build VM is a `scratch` VM (so there
/// is no template layer); its disk is pre-seeded after the runtime builds.
/// `guest_iso` attaches the VMLAB bootstrap ISO folder as extra media.
/// `with_steps = false` renders the verification-boot variant: no media at
/// all and no provisions/playbooks — just boot the installed disk.
fn synth_lab(
    def: &TemplateDef,
    lab_name: &str,
    vm: &str,
    cdrom: Option<&Path>,
    root: &Path,
    guest_iso: Option<&Path>,
    with_steps: bool,
) -> Result<String> {
    use std::fmt::Write;
    let mut s = String::from("import <vmlab.wcl>\n\n");
    writeln!(s, "lab \"{lab_name}\" {{").unwrap();
    writeln!(s, "  vm \"{vm}\" {{").unwrap();
    writeln!(s, "    template = \"scratch\"").unwrap();
    writeln!(s, "    arch     = \"{}\"", def.arch).unwrap();
    let profile = def.profile.as_deref().unwrap_or("linux-generic");
    writeln!(s, "    profile  = \"{profile}\"").unwrap();
    // Bare integers: `disk`/`memory` are std.ByteSize in the schema, which
    // takes byte counts or size literals — never quoted strings.
    let disk = def.disk.unwrap_or(20 << 30);
    writeln!(s, "    disk     = {disk}").unwrap();
    if let Some(cpus) = def.cpus {
        writeln!(s, "    cpus     = {cpus}").unwrap();
    }
    if let Some(mem) = def.memory {
        writeln!(s, "    memory   = {mem}").unwrap();
    }
    if let Some(c) = cdrom {
        writeln!(s, "    cdrom    = \"{}\"", c.display()).unwrap();
    }
    if def.gui {
        writeln!(s, "    gui      = true").unwrap();
    }
    // Template-declared NICs carry over. The synthetic lab declares no
    // segments, so only NAT NICs make sense here — segment references are
    // rewritten to NAT. Builds with no NICs declared get internet egress by
    // default (agent/package install).
    if def.nics.is_empty() {
        writeln!(s, "    nic {{ nat = true }}").unwrap();
    } else {
        for n in &def.nics {
            let mut attrs = String::from("nat = true");
            if let Some(mac) = &n.mac {
                write!(attrs, " mac = \"{mac}\"").unwrap();
            }
            writeln!(s, "    nic {{ {attrs} }}").unwrap();
        }
    }
    // Media (driver/answer-file ISOs/floppies, §6.3) carry over, resolved
    // relative to the original file's root. The verification boot carries
    // none: the whole point is booting the installed disk alone.
    if with_steps {
        for m in &def.media {
            let kind = match m.kind {
                crate::config::model::MediaKind::Iso => "iso",
                crate::config::model::MediaKind::Floppy => "floppy",
            };
            let from = root.join(&m.from);
            write!(
                s,
                "    media {{ kind = \"{kind}\" from = \"{}\"",
                from.display()
            )
            .unwrap();
            if let Some(l) = &m.label {
                write!(s, " label = \"{l}\"").unwrap();
            }
            writeln!(s, " }}").unwrap();
        }
        if let Some(gi) = guest_iso {
            writeln!(
                s,
                "    media {{ kind = \"iso\" from = \"{}\" label = \"VMLAB\" }}",
                gi.display()
            )
            .unwrap();
        }
    }
    writeln!(s, "  }}").unwrap();
    // Build provision scripts and playbooks run against the single build VM
    // (§10.4). The lab runtime interleaves them in the synthetic file's
    // declaration order, so emit them ordered by their spans in the original
    // template definition. Paths are rebased absolute: the synthetic lab's
    // root is the throwaway work dir, not the template root.
    enum Step<'a> {
        Provision(&'a crate::config::model::Provision),
        Playbook(&'a crate::config::model::Playbook),
    }
    let mut steps: Vec<(usize, Step)> = if with_steps {
        def.provisions
            .iter()
            .map(|p| (p.span.0, Step::Provision(p)))
            .chain(def.playbooks.iter().map(|p| (p.span.0, Step::Playbook(p))))
            .collect()
    } else {
        Vec::new()
    };
    steps.sort_by_key(|(at, _)| *at);
    for (_, step) in steps {
        match step {
            Step::Provision(p) => {
                let script = root.join(&p.script);
                writeln!(s, "  provision \"{}\" {{ }}", script.display()).unwrap();
            }
            Step::Playbook(p) => {
                let dir = root.join(&p.path);
                writeln!(
                    s,
                    "  playbook \"{}\" {{ play = \"{}\" vms = [\"{vm}\"] }}",
                    dir.display(),
                    p.play
                )
                .unwrap();
            }
        }
    }
    writeln!(s, "}}").unwrap();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::synth_lab;
    use std::path::Path;

    fn def(source: &str) -> crate::config::model::TemplateDef {
        let tf = crate::config::load_template_source(source, "<test>", Path::new("/root")).unwrap();
        tf.templates.into_iter().next().unwrap()
    }

    /// A template-declared NIC must reach the synthetic build lab (it used
    /// to be silently dropped, booting the build VM with `-nic none`).
    #[test]
    fn declared_nic_carries_into_build_lab() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  nic { nat = true }\n",
            "}\n"
        ));
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root"), None, true).unwrap();
        assert!(wcl.contains("nic { nat = true }"), "{wcl}");
    }

    #[test]
    fn no_nics_defaults_to_nat() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root"), None, true).unwrap();
        assert!(wcl.contains("nic { nat = true }"), "{wcl}");
    }

    /// Template playbooks reach the synthetic build lab as `playbook` blocks
    /// targeting the build VM, path rebased absolute, and interleaved with
    /// provisions in declaration order (the up-queue replays that order).
    #[test]
    fn playbooks_carry_into_build_lab_in_declaration_order() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  provision \"a.ws\" { }\n",
            "  playbook \"pb\" { play = \"baseline\" }\n",
            "  provision \"b.ws\" { }\n",
            "}\n"
        ));
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root"), None, true).unwrap();
        assert!(
            wcl.contains("playbook \"/root/pb\" { play = \"baseline\" vms = [\"build\"] }"),
            "{wcl}"
        );
        let a = wcl.find("/root/a.ws").expect("provision a");
        let pb = wcl.find("/root/pb").expect("playbook");
        let b = wcl.find("/root/b.ws").expect("provision b");
        assert!(a < pb && pb < b, "declaration order lost:\n{wcl}");
        crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
    }

    /// The synthetic build lab must satisfy the lab schema — `disk`/`memory`
    /// are std.ByteSize and must render as bare integers, not quoted strings
    /// (quoted values broke every build after the ByteSize migration).
    #[test]
    fn build_lab_parses_against_the_schema() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  memory = 2GiB\n",
            "  disk   = 20GiB\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root"), None, true).unwrap();
        crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
    }

    /// The VMLAB bootstrap ISO folder rides in as extra media; the
    /// verification-boot variant (`with_steps = false`) drops all media and
    /// steps so the installed disk boots alone.
    #[test]
    fn guest_iso_media_and_verification_variant() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  media { kind = \"iso\" from = \"./cloudinit/\" label = \"CIDATA\" }\n",
            "  provision \"a.ws\" { }\n",
            "}\n"
        ));
        let wcl = synth_lab(
            &d,
            "build-t",
            "build",
            None,
            Path::new("/root"),
            Some(Path::new("/work/guest-iso")),
            true,
        )
        .unwrap();
        assert!(
            wcl.contains("media { kind = \"iso\" from = \"/work/guest-iso\" label = \"VMLAB\" }"),
            "{wcl}"
        );
        assert!(wcl.contains("CIDATA"), "{wcl}");
        crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));

        let verify = synth_lab(
            &d,
            "build-t",
            "build",
            None,
            Path::new("/root"),
            None,
            false,
        )
        .unwrap();
        assert!(!verify.contains("media"), "{verify}");
        assert!(!verify.contains("provision"), "{verify}");
        assert!(verify.contains("nic { nat = true }"), "{verify}");
        crate::config::load_lab_source(&verify, "<verify>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("verification lab must parse: {e:?}\n{verify}"));
    }

    /// `first_boot` parses to the script path; it is build-time-only, so the
    /// synthetic build lab must NOT replay it (first-boot runs at instantiation,
    /// not during the build).
    #[test]
    fn first_boot_parses_and_is_not_in_build_lab() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  first_boot = \"scripts/firstboot.ws\"\n",
            "}\n"
        ));
        assert_eq!(
            d.first_boot.as_deref(),
            Some(Path::new("scripts/firstboot.ws"))
        );
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root"), None, true).unwrap();
        assert!(!wcl.contains("firstboot.ws"), "{wcl}");
    }
}
