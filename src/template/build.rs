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
use super::store::{ResolvedTemplate, TemplateStore};
use crate::config::model::{ArtefactSource, Firmware, TemplateDef, TemplateRef, TemplateSource};
use crate::labd::machine::Machine;
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

/// Where every build's working directory lives.
pub fn builds_dir() -> PathBuf {
    crate::paths::data_dir().join("cache").join("builds")
}

fn build_workdir(def: &TemplateDef) -> PathBuf {
    builds_dir().join(format!("{}-{}-{}", def.arch, def.name, def.version))
}

/// Delete every build working directory, returning how many went.
///
/// [`WorkdirGuard`] clears one up when its build ends, but it is a `Drop` and
/// a killed process runs none: a supervisor that is SIGKILLed — or that exits
/// through `process::exit` on `shutdown` — leaves its in-flight builds' disks
/// behind, several gigabytes each. The supervisor sweeps at startup, when by
/// definition it owns no build, so a leftover survives at most until the next
/// time the daemon comes up (ADR-0010).
pub fn sweep_build_workdirs() -> usize {
    sweep_workdirs_in(&builds_dir())
}

fn sweep_workdirs_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir() && std::fs::remove_dir_all(e.path()).is_ok())
        .count()
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

    // A layered build boots the hardware its source recorded, under whatever
    // the block restates (ADR-0009), so the source's metadata has to be in
    // hand before anything asks what this build boots on.
    let mut layered = resolve_layered_source(def, store)?;
    let hw = EffectiveHardware::resolve(def, layered.as_ref().map(|r| &r.meta));

    // That hardware decides how the build VM boots, so resolve it before the
    // first expensive step: the resolver refuses combinations that cannot work
    // (secure boot without UEFI, §5.2), and downloading a multi-gigabyte ISO
    // first only to refuse at boot helps nobody.
    check_build_hardware(def, &hw, build_vm, root, profiles)?;

    // Resolve the source into the working primary disk. A layered source's
    // embedded first-boot provision must gate the build boot exactly as it
    // gates a clone (PRD §6.1): a sysprep-generalized Windows source replays
    // specialize/OOBE on this boot, and the agent answers while that is
    // still running — sealing then would capture a half-specialized image.
    let mut source_first_boot: Option<crate::scripting::EmbeddedWscript> = None;
    let (cdrom, seed_disk): (Option<PathBuf>, SeedDisk) = match &def.source {
        TemplateSource::Iso(src) => {
            let iso = cancelable(&run.control.cancel, resolve_artefact(src, root, log)).await?;
            (Some(iso), SeedDisk::Blank(disk_size))
        }
        TemplateSource::Qcow2(src) => {
            let img = cancelable(&run.control.cancel, resolve_artefact(src, root, log)).await?;
            (None, SeedDisk::CopyFrom(img))
        }
        TemplateSource::Template { .. } => {
            let resolved = layered
                .take()
                .context("no store entry resolved for a layered build source")?;
            source_first_boot = resolved.meta.first_boot();
            (None, SeedDisk::CopyFrom(resolved.disk_path))
        }
        TemplateSource::Scratch { .. } => (None, SeedDisk::Blank(disk_size)),
    };

    let wants_agent = wants_agent(def, &hw, profiles);
    let staged: Option<Arc<super::bootstrap::StagedGuestIso>> = if wants_agent {
        Some(Arc::new(super::bootstrap::stage_guest_iso_dir(
            work, &def.arch,
        )?))
    } else {
        None
    };

    // Synthesize a one-VM scratch lab for the build.
    let lab_name = build_lab_name(def);
    let lab_wcl = synth_lab(
        def,
        &hw,
        &lab_name,
        build_vm,
        root,
        BuildBoot::Install {
            cdrom: cdrom.as_deref(),
            guest_iso: staged.as_ref().map(|s| s.dir.as_path()),
        },
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
    // A build's provisions are the installer, and the agent readiness means
    // only exists once they have run — so a build never gates a provision
    // step on readiness. The verification below is what holds the agent
    // contract instead, and it is the one thing that must not be skipped.
    runtime
        .provisions_wait_ready
        .store(false, std::sync::atomic::Ordering::Relaxed);
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
                &(vm.clone() as Arc<dyn Machine>),
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
            Some(Arc::new(move |machine, out| {
                let staged = staged.clone();
                let agent_version = agent_version.clone();
                Box::pin(async move {
                    let log = move |s: String| out(s);
                    let version = super::agent_install::verify(
                        &machine,
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
    if let Some(first_boot) = source_first_boot {
        let parts = vm.template();
        vm.set_template(crate::labd::vm::TemplateParts {
            resolved: parts.resolved.clone(),
            backing: parts.backing.clone(),
            disk_size: parts.disk_size,
            first_boot: Some(first_boot),
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
        let verify_wcl = synth_lab(def, &hw, &lab_name, build_vm, root, BuildBoot::Bare)?;
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
                &(vm2.clone() as Arc<dyn Machine>),
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
    let meta = seal_meta(
        def,
        &hw,
        version,
        SealedImage {
            disk: info.virtual_size,
            sha256: sha,
            first_boot_script,
            agent_version: agent_version.lock().expect("agent_version lock").clone(),
        },
    );

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

/// The synthetic lab a build runs in, named after the template it builds.
fn build_lab_name(def: &TemplateDef) -> String {
    format!("build-{}", def.name)
}

/// Effective build hardware: what a build boots on and seals, the template
/// block over the source template's recorded metadata (ADR-0009).
///
/// Those two layers and no more. It is deliberately not the §5.2 chain — it
/// names a profile but takes nothing *from* one. That layer stays live,
/// resolved by [`crate::qemu::resolve`] over the rendered lab, so an image
/// still picks up a later edit to the profile it names. A field neither layer
/// declares stays `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EffectiveHardware {
    profile: Option<String>,
    cpus: Option<u32>,
    memory: Option<u64>,
    firmware: Option<Firmware>,
    tpm: Option<bool>,
    secure_boot: Option<bool>,
    display: Option<String>,
}

impl EffectiveHardware {
    /// Merge the template block over `source` — the recorded metadata of the
    /// template a layered build layers on, `None` for every other source kind.
    fn resolve(def: &TemplateDef, source: Option<&TemplateMeta>) -> Self {
        let (source_firmware, source_secure_boot) = source_firmware(source);
        Self {
            profile: def
                .profile
                .clone()
                .or_else(|| source.and_then(|m| m.profile.clone())),
            cpus: def.cpus.or_else(|| source.and_then(|m| m.cpus)),
            memory: def.memory.or_else(|| source.and_then(|m| m.memory)),
            firmware: def.firmware.or(source_firmware),
            tpm: def.tpm.or_else(|| source.and_then(|m| m.tpm)),
            secure_boot: def.secure_boot.or(source_secure_boot),
            display: def
                .display
                .clone()
                .or_else(|| source.and_then(|m| m.display.clone())),
        }
    }

    /// The profile the build resolves against. `linux-generic` is vmlab's
    /// default layer, below the profile rather than a substitute for it: it
    /// applies only when neither the block nor the source names one, and it is
    /// never recorded in the sealed metadata.
    fn profile_or_default(&self) -> &str {
        self.profile.as_deref().unwrap_or("linux-generic")
    }
}

/// What a source template contributes to the firmware/secure-boot pair.
///
/// The two only mean anything together (ADR-0009), so the source offers them
/// together or not at all. Metadata records a firmware as free text, so a store
/// entry can name a spelling this build cannot read: that firmware is dropped
/// — the lab schema names exactly two, and an unknown one would fail the
/// synthetic lab's own validation — and the secure boot beside it goes with it,
/// rather than being left hanging over whatever firmware the profile floor
/// happens to supply. A source recording secure boot and no firmware at all is
/// a different case and inherits normally: its profile carries over too, and
/// that is where its firmware came from.
///
/// Values the block declares are unaffected — it gets what it asked for.
fn source_firmware(source: Option<&TemplateMeta>) -> (Option<Firmware>, Option<bool>) {
    let Some(meta) = source else {
        return (None, None);
    };
    match meta.firmware.as_deref() {
        Some(recorded) => match Firmware::parse(recorded) {
            Some(f) => (Some(f), meta.secure_boot),
            None => (None, None),
        },
        None => (None, meta.secure_boot),
    }
}

/// Resolve a layered build's source through the store.
///
/// Runs before the hardware pre-flight, because the source's recorded hardware
/// is one of the layers the build boots on (ADR-0009) — a pair inherited from
/// it has to reach the pre-flight, not arrive after it. It reads local
/// metadata only; the expensive artefact fetch stays behind the gate.
fn resolve_layered_source(
    def: &TemplateDef,
    store: &TemplateStore,
) -> Result<Option<ResolvedTemplate>> {
    let TemplateSource::Template { from, .. } = &def.source else {
        return Ok(None);
    };
    let TemplateRef::Store {
        arch,
        name,
        version,
    } = from
    else {
        bail!("layered build source must be a store reference");
    };
    store
        .resolve(arch, name, version.as_deref())
        .context("resolving layered build source")
        .map(Some)
}

/// Whether the build stages the VMLAB bootstrap ISO (agent binaries + install
/// scripts), which the guest's own unattended install runs so the agent exists
/// before any host channel does. Skipped when the template opts out or the
/// effective profile has no agent channel — a layered build inherits its
/// source's profile, and a vintage guest would just carry a dead ISO.
fn wants_agent(
    def: &TemplateDef,
    hw: &EffectiveHardware,
    profiles: &crate::profiles::ProfileSet,
) -> bool {
    let channel = profiles
        .get(hw.profile_or_default())
        .map(|p| p.agent_channel)
        .unwrap_or(true);
    def.agent && channel
}

/// Resolve the hardware the build VM will boot on, and fail if it cannot.
///
/// It resolves the rendered build lab rather than the `TemplateDef` directly,
/// so what is checked is exactly what boots: same synthetic VM, same §5.2
/// chain (no template layer — the build VM is `scratch`), same refusals.
fn check_build_hardware(
    def: &TemplateDef,
    hw: &EffectiveHardware,
    build_vm: &str,
    root: &Path,
    profiles: &crate::profiles::ProfileSet,
) -> Result<()> {
    // Hardware-only probe: the bare variant carries none of the build-time
    // attachments (extra disks, media, steps), and none of them can change the
    // hardware this resolves. Its own source label, so an issue reported
    // against the render names the render that produced it.
    let lab_name = build_lab_name(def);
    let wcl = synth_lab(def, hw, &lab_name, build_vm, root, BuildBoot::Bare)?;
    let labfile = crate::config::load_lab_source(&wcl, "<preflight>", root)
        .map_err(|e| anyhow::anyhow!("internal build lab invalid: {e:?}"))?;
    let vm = labfile
        .lab
        .vms
        .first()
        .context("internal build lab has no VM")?;
    crate::qemu::resolve::resolve_vm(vm, None, profiles).with_context(|| {
        format!(
            "template \"{}/{}\" cannot build: the hardware it declares does not resolve",
            def.arch, def.name
        )
    })?;
    Ok(())
}

/// Facts about the sealed image that only exist once the build has run.
struct SealedImage {
    /// Virtual size of the flattened disk, in bytes.
    disk: u64,
    /// Hex SHA-256 of the flattened disk.
    sha256: String,
    first_boot_script: Option<String>,
    agent_version: Option<String>,
}

/// The metadata a finished build installs alongside its disk.
///
/// Hardware comes from the merged build hardware, not the block alone: a
/// layered build records what it inherited, so a chain of rebuilds ends with
/// the hardware the first one recorded instead of losing a layer each time
/// (ADR-0009). Nothing profile-derived is recorded — a field neither the block
/// nor the source declared stays absent, and the profile it names stays a live
/// layer for whoever clones this template.
fn seal_meta(
    def: &TemplateDef,
    hw: &EffectiveHardware,
    version: &str,
    image: SealedImage,
) -> TemplateMeta {
    TemplateMeta {
        name: def.name.clone(),
        arch: def.arch.clone(),
        version: version.to_string(),
        profile: hw.profile.clone(),
        cpus: hw.cpus,
        memory: hw.memory,
        disk: Some(image.disk),
        firmware: hw.firmware.map(|f| f.as_str().to_string()),
        tpm: hw.tpm,
        secure_boot: hw.secure_boot,
        display: hw.display.clone(),
        created: chrono::Utc::now(),
        origin: source_origin(&def.source),
        registry: def.registry.clone(),
        sha256: Some(image.sha256),
        first_boot_script: image.first_boot_script,
        agent_version: image.agent_version,
        wscript_surface: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
    }
}

fn source_origin(source: &TemplateSource) -> Option<String> {
    match source {
        TemplateSource::Iso(ArtefactSource::Url { url, .. })
        | TemplateSource::Qcow2(ArtefactSource::Url { url, .. }) => Some(url.clone()),
        TemplateSource::Template { from, .. } => Some(from.to_string()),
        _ => None,
    }
}

/// Quote a value for a WCL string literal. Every path, label and name below
/// comes from a template definition (or a work-dir path), so a `"` or `\` in
/// one would otherwise produce a file that fails to parse — a confusing error a
/// long way from its cause.
fn wcl_str(value: impl std::fmt::Display) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What a rendered build lab boots with — the only thing that differs between
/// the renders below, all of which describe the same machine.
enum BuildBoot<'a> {
    /// The build proper: everything the build attaches *to do the build with*
    /// — the template's extra disks and media, the installer's `cdrom`, the
    /// VMLAB bootstrap ISO folder in `guest_iso`, and its provisions and
    /// playbooks.
    Install {
        cdrom: Option<&'a Path>,
        guest_iso: Option<&'a Path>,
    },
    /// The installed disk alone, with none of those attachments. Both the
    /// hardware pre-flight and the verification boot render this: neither can
    /// be changed by what the build attached, and the verification boot's whole
    /// point is proving the sealed image boots by itself.
    Bare,
}

/// Render the synthetic build lab. The build VM is a `scratch` VM (so there
/// is no template layer); its disk is pre-seeded after the runtime builds.
///
/// Hardware comes from `hw`, never from `def` directly: a layered build's
/// source contributes to it (ADR-0009), and the emitter needs one source of
/// truth for the values the pre-flight will check.
fn synth_lab(
    def: &TemplateDef,
    hw: &EffectiveHardware,
    lab_name: &str,
    vm: &str,
    root: &Path,
    boot: BuildBoot<'_>,
) -> Result<String> {
    use std::fmt::Write;
    let (cdrom, guest_iso) = match boot {
        BuildBoot::Install { cdrom, guest_iso } => (cdrom, guest_iso),
        BuildBoot::Bare => (None, None),
    };
    let with_steps = matches!(boot, BuildBoot::Install { .. });
    // Destructured rather than read field by field: this emitter has drifted
    // behind the block three times now, each repaired by hand (ADR-0009's
    // discharged note). An exhaustive binding makes the next *hardware* field a
    // compile error here instead of a template that builds on hardware it never
    // chose. It does not make the whole emitter exhaustive — the fields it
    // translates rather than copies are still enumerated below, which is the
    // debt that record leaves open.
    let EffectiveHardware {
        // Rendered below through its default floor, never raw.
        profile: _,
        cpus,
        memory,
        firmware,
        tpm,
        secure_boot,
        display,
    } = hw;
    let mut s = String::from("import <vmlab.wcl>\n\n");
    writeln!(s, "lab {} {{", wcl_str(lab_name)).unwrap();
    writeln!(s, "  vm {} {{", wcl_str(vm)).unwrap();
    writeln!(s, "    template = \"scratch\"").unwrap();
    writeln!(s, "    arch     = {}", wcl_str(&def.arch)).unwrap();
    writeln!(s, "    profile  = {}", wcl_str(hw.profile_or_default())).unwrap();
    // Bare integers: `disk`/`memory` are std.ByteSize in the schema, which
    // takes byte counts or size literals — never quoted strings.
    let disk = def.disk.unwrap_or(20 << 30);
    writeln!(s, "    disk     = {disk}").unwrap();
    if let Some(cpus) = cpus {
        writeln!(s, "    cpus     = {cpus}").unwrap();
    }
    if let Some(mem) = memory {
        writeln!(s, "    memory   = {mem}").unwrap();
    }
    if let Some(c) = cdrom {
        writeln!(s, "    cdrom    = {}", wcl_str(c.display())).unwrap();
    }
    if def.gui {
        writeln!(s, "    gui      = true").unwrap();
    }
    // §5.2 hardware the build resolved is hardware for the build VM (PRD §6.1:
    // "boot per template hardware"), so it has to be rendered here — the four
    // that also reach the metadata are the inheritance layer for VMs cloning
    // the template, which is a different job. The build VM is `scratch`, so it
    // has no template layer of its own (§6.5): these land on the vm block,
    // with the profile as the floor beneath them.
    if let Some(d) = display {
        writeln!(s, "    display  = {}", wcl_str(d)).unwrap();
    }
    if let Some(f) = firmware {
        writeln!(s, "    firmware = {}", wcl_str(f.as_str())).unwrap();
    }
    if let Some(tpm) = tpm {
        writeln!(s, "    tpm      = {tpm}").unwrap();
    }
    if let Some(sb) = secure_boot {
        writeln!(s, "    secure_boot = {sb}").unwrap();
    }
    if def.nested {
        writeln!(s, "    nested   = true").unwrap();
    }
    if !def.qemu_args.is_empty() {
        let args: Vec<String> = def.qemu_args.iter().map(wcl_str).collect();
        writeln!(s, "    qemu_args = [{}]", args.join(", ")).unwrap();
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
                write!(attrs, " mac = {}", wcl_str(mac)).unwrap();
            }
            writeln!(s, "    nic {{ {attrs} }}").unwrap();
        }
    }
    // Everything the build attaches *to do the build with* — extra disks,
    // media, steps — carries over here, with folder paths resolved relative to
    // the original file's root. The verification boot carries none of it: the
    // whole point is booting the installed disk alone.
    if with_steps {
        // Template-declared disks are "additional disks attached during the
        // build" (schema) — build-time scratch a provision can write to, gone
        // once the primary disk is sealed and moved into the store (§6.1).
        // `from` folders rebase absolute against the template root, like media.
        for d in &def.extra_disks {
            write!(s, "    disk {} {{", wcl_str(&d.name)).unwrap();
            if let Some(size) = d.size {
                write!(s, " size = {size}").unwrap();
            }
            if let Some(from) = &d.from {
                write!(s, " from = {}", wcl_str(root.join(from).display())).unwrap();
            }
            writeln!(s, " }}").unwrap();
        }
        // Media: driver/answer-file ISOs and floppies built from folders (§6.3).
        for m in &def.media {
            let kind = match m.kind {
                crate::config::model::MediaKind::Iso => "iso",
                crate::config::model::MediaKind::Floppy => "floppy",
            };
            let from = root.join(&m.from);
            write!(
                s,
                "    media {{ kind = {} from = {}",
                wcl_str(kind),
                wcl_str(from.display())
            )
            .unwrap();
            if let Some(l) = &m.label {
                write!(s, " label = {}", wcl_str(l)).unwrap();
            }
            writeln!(s, " }}").unwrap();
        }
        if let Some(gi) = guest_iso {
            writeln!(
                s,
                "    media {{ kind = \"iso\" from = {} label = \"VMLAB\" }}",
                wcl_str(gi.display())
            )
            .unwrap();
        }
    }
    // Build provision scripts and playbooks run against the single build VM
    // (§10.4), so they are emitted inside its block — the lab runtime
    // interleaves a machine's steps in declaration order, so keep them ordered
    // by their spans in the original template definition. Paths are rebased
    // absolute: the synthetic lab's root is the throwaway work dir, not the
    // template root.
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
                writeln!(s, "    provision {} {{ }}", wcl_str(script.display())).unwrap();
            }
            Step::Playbook(p) => {
                let dir = root.join(&p.path);
                write!(
                    s,
                    "    playbook {} {{ play = {}",
                    wcl_str(dir.display()),
                    wcl_str(&p.play),
                )
                .unwrap();
                for v in &p.vars {
                    write!(
                        s,
                        " var {} {{ value = {} }}",
                        wcl_str(&v.name),
                        wcl_str(&v.value)
                    )
                    .unwrap();
                }
                writeln!(s, " }}").unwrap();
            }
        }
    }
    writeln!(s, "  }}").unwrap();
    writeln!(s, "}}").unwrap();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::{
        BuildBoot, EffectiveHardware, SealedImage, seal_meta, sweep_workdirs_in, synth_lab, wcl_str,
    };
    use crate::template::meta::TemplateMeta;
    use std::path::Path;

    /// A supervisor that was killed mid-build leaves its working disk behind
    /// — the guard that would have removed it is a `Drop`. The next one to
    /// start owns no build, so everything it finds there is a leftover.
    #[test]
    fn a_sweep_clears_every_leftover_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing to sweep, and a directory that was never created, are both
        // fine rather than errors.
        assert_eq!(sweep_workdirs_in(dir.path()), 0);
        assert_eq!(sweep_workdirs_in(&dir.path().join("never-made")), 0);

        for name in ["x86_64-base-1.0", "aarch64-base-2.0"] {
            let work = dir.path().join(name);
            std::fs::create_dir_all(work.join("nested")).unwrap();
            std::fs::write(work.join("disk.qcow2"), "pretend").unwrap();
        }
        // A stray file is not a working directory and is left alone.
        std::fs::write(dir.path().join("stray"), "").unwrap();

        assert_eq!(sweep_workdirs_in(dir.path()), 2);
        assert!(!dir.path().join("x86_64-base-1.0").exists());
        assert!(dir.path().join("stray").is_file());
    }

    fn def(source: &str) -> crate::config::model::TemplateDef {
        let tf = crate::config::load_template_source(source, "<test>", Path::new("/root")).unwrap();
        tf.templates.into_iter().next().unwrap()
    }

    /// The build render, with the VMLAB bootstrap ISO folder attached or not.
    fn install(guest_iso: Option<&Path>) -> BuildBoot<'_> {
        BuildBoot::Install {
            cdrom: None,
            guest_iso,
        }
    }

    /// Render the build lab for a template with no source template under it.
    fn render(d: &crate::config::model::TemplateDef) -> String {
        synth_lab(
            d,
            &hw(d),
            "build-t",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap()
    }

    /// Build hardware for a template with no source template under it — every
    /// source kind but a layered one.
    fn hw(def: &crate::config::model::TemplateDef) -> EffectiveHardware {
        EffectiveHardware::resolve(def, None)
    }

    /// A store entry's recorded metadata, hardware only: what a layered build
    /// layers on.
    fn source_meta() -> TemplateMeta {
        TemplateMeta {
            name: "win11".into(),
            arch: "x86_64".into(),
            version: "26100.1".into(),
            profile: None,
            cpus: None,
            memory: None,
            disk: None,
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            created: "2026-01-02T03:04:05Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
        }
    }

    /// The same, with every hardware field recorded — what rebuilding a real
    /// Windows 11 template layers on.
    fn source_meta_full() -> TemplateMeta {
        TemplateMeta {
            profile: Some("windows-11".into()),
            cpus: Some(4),
            memory: Some(8 << 30),
            firmware: Some("ovmf".into()),
            tpm: Some(true),
            secure_boot: Some(true),
            display: Some("virtio-vga".into()),
            ..source_meta()
        }
    }

    /// A layered template block naming `x86_64/win11` as its source, plus
    /// whatever `body` declares of its own.
    fn layered(body: &str) -> crate::config::model::TemplateDef {
        def(&format!(
            "import <vmlab.wcl>\n\
             template \"t\" {{ arch = \"x86_64\" version = \"1\"\n{body}\
             \x20 source \"template\" {{ from = \"x86_64/win11@26100.1\" }}\n\
             }}\n"
        ))
    }

    #[test]
    fn wcl_strings_escape_quotes_and_backslashes() {
        assert_eq!(wcl_str("plain"), "\"plain\"");
        assert_eq!(wcl_str(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(wcl_str(r"C:\drivers"), r#""C:\\drivers""#);
        assert_eq!(wcl_str("two\nlines"), "\"two\\nlines\"");
    }

    /// A media label containing a quote used to emit a lab file that failed to
    /// parse, a long way from the value that caused it.
    #[test]
    fn a_quote_in_a_label_still_produces_a_parseable_lab() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  media { kind = \"iso\" from = \"drivers\" label = \"say \\\"hi\\\"\" }\n",
            "}\n"
        ));
        let wcl = render(&d);
        assert!(wcl.contains(r#"label = "say \"hi\"""#), "{wcl}");
        crate::config::load_lab_source(&wcl, "<synth>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic lab must parse: {e:?}\n{wcl}"));
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
        let wcl = render(&d);
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
        let wcl = render(&d);
        assert!(wcl.contains("nic { nat = true }"), "{wcl}");
    }

    /// Template steps reach the synthetic build lab nested inside the build
    /// VM, paths rebased absolute, in declaration order (the up-queue replays
    /// that order), with each playbook's variables carried along.
    #[test]
    fn playbooks_carry_into_build_lab_in_declaration_order() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  provision \"a.ws\" { }\n",
            "  playbook \"pb\" { play = \"baseline\" var \"tz\" { value = \"UTC\" } }\n",
            "  provision \"b.ws\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        assert!(
            wcl.contains(
                "playbook \"/root/pb\" { play = \"baseline\" var \"tz\" { value = \"UTC\" } }"
            ),
            "{wcl}"
        );
        let a = wcl.find("/root/a.ws").expect("provision a");
        let pb = wcl.find("/root/pb").expect("playbook");
        let b = wcl.find("/root/b.ws").expect("provision b");
        assert!(a < pb && pb < b, "declaration order lost:\n{wcl}");
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let build = &lf.lab.vms[0];
        assert_eq!(build.name, "build");
        assert_eq!(build.provisions.len(), 2);
        assert_eq!(build.playbooks.len(), 1);
        assert_eq!(build.playbooks[0].vars[0].name, "tz");
        assert_eq!(build.playbooks[0].vars[0].value, "UTC");
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
        let wcl = render(&d);
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
            &hw(&d),
            "build-t",
            "build",
            Path::new("/root"),
            install(Some(Path::new("/work/guest-iso"))),
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
            &hw(&d),
            "build-t",
            "build",
            Path::new("/root"),
            BuildBoot::Bare,
        )
        .unwrap();
        assert!(!verify.contains("media"), "{verify}");
        assert!(!verify.contains("provision"), "{verify}");
        assert!(verify.contains("nic { nat = true }"), "{verify}");
        crate::config::load_lab_source(&verify, "<verify>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("verification lab must parse: {e:?}\n{verify}"));
    }

    /// `disk {}` blocks on a `template {}` are "additional disks attached
    /// during the build" (schema doc string). They used to be validated and
    /// then dropped, so the build VM booted with its primary disk alone.
    /// `from` is a folder relative to the template's root, so it has to be
    /// rebased absolute — the synthetic lab's root is the throwaway work dir.
    #[test]
    fn declared_disks_carry_into_build_lab() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  disk \"data\"    { size = 10GiB }\n",
            "  disk \"drivers\" { from = \"./drivers/\" }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let disks = &lf.lab.vms[0].extra_disks;
        assert_eq!(disks.len(), 2, "{wcl}");
        assert_eq!(disks[0].name, "data");
        assert_eq!(disks[0].size, Some(10 << 30));
        assert_eq!(disks[0].from, None);
        assert_eq!(disks[1].name, "drivers");
        assert_eq!(disks[1].size, None);
        assert_eq!(
            disks[1].from.as_deref(),
            Some(Path::new("/root/drivers")),
            "`from` must be rebased against the template root:\n{wcl}"
        );

        // The verification boot proves the sealed image boots alone, so it
        // carries no extra disks — the same reason it carries no media.
        let verify = synth_lab(
            &d,
            &hw(&d),
            "build-t",
            "build",
            Path::new("/root"),
            BuildBoot::Bare,
        )
        .unwrap();
        let vf = crate::config::load_lab_source(&verify, "<verify>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("verification lab must parse: {e:?}\n{verify}"));
        assert!(vf.lab.vms[0].extra_disks.is_empty(), "{verify}");
    }

    /// A disk sized *and* sourced from a folder (the schema allows both:
    /// `@one_of(["size", "from"], exclusive = false)`) carries both fields.
    #[test]
    fn a_disk_with_both_size_and_from_carries_both() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  disk \"payload\" { size = 2GiB from = \"payload\" }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let disk = &lf.lab.vms[0].extra_disks[0];
        assert_eq!(disk.size, Some(2 << 30));
        assert_eq!(disk.from.as_deref(), Some(Path::new("/root/payload")));
    }

    /// Every §5.2 hardware attribute a `template {}` block declares is a
    /// setting *for the build VM* (schema doc strings, PRD §6.1 "boot per
    /// template hardware"). They used to be written to the template metadata
    /// only — `nested`/`qemu_args` nowhere at all — so the build booted the
    /// profile's hardware whatever the block said.
    #[test]
    fn declared_hardware_reaches_the_build_vm() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  profile     = \"linux-modern\"\n",
            "  display     = \"std\"\n",
            "  firmware    = \"seabios\"\n",
            "  tpm         = false\n",
            "  secure_boot = false\n",
            "  nested      = true\n",
            "  qemu_args   = [\"-device\", \"weird-thing\"]\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let build = &lf.lab.vms[0];
        assert_eq!(build.display.as_deref(), Some("std"));
        assert_eq!(
            build.firmware,
            Some(crate::config::model::Firmware::Seabios)
        );
        assert_eq!(build.tpm, Some(false));
        assert_eq!(build.secure_boot, Some(false));
        assert!(build.nested);
        assert_eq!(build.qemu_args, ["-device", "weird-thing"]);
    }

    /// …and the resolved hardware really is the template's, not the profile
    /// floor underneath it: `linux-modern` is OVMF, the block said SeaBIOS.
    #[test]
    fn declared_hardware_wins_over_the_profile_floor() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  profile  = \"linux-modern\"\n",
            "  firmware = \"seabios\"\n",
            "  display  = \"std\"\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root")).unwrap();
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        // No template layer: the build VM is `scratch` (§6.5).
        let resolved = crate::qemu::resolve::resolve_vm(&lf.lab.vms[0], None, &profiles).unwrap();
        assert_eq!(
            resolved.firmware,
            Some(crate::profiles::FirmwareKind::Seabios)
        );
        assert_eq!(resolved.display_device.as_deref(), Some("VGA"));
    }

    /// Now that both values are live for the build, the pair that cannot work
    /// together is refused up front instead of booting a VM whose secure boot
    /// is dropped on the floor (§5.2, the #14/#19 shape on the build path).
    #[test]
    fn secure_boot_on_a_seabios_template_refuses_the_build() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  firmware    = \"seabios\"\n",
            "  secure_boot = true\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let err = super::check_build_hardware(&d, &hw(&d), "build", Path::new("/root"), &profiles)
            .expect_err("secure boot under SeaBIOS must refuse the build");
        let report = format!("{err:#}");
        assert!(report.contains("x86_64/t"), "{report}");
        assert!(report.contains("secure boot needs UEFI"), "{report}");
    }

    /// The happy path resolves — the pre-flight must not refuse an ordinary
    /// build (it runs before every one of them).
    #[test]
    fn ordinary_hardware_passes_the_pre_flight() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  profile  = \"linux-modern\"\n",
            "  firmware = \"seabios\"\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        super::check_build_hardware(&d, &hw(&d), "build", Path::new("/root"), &profiles).unwrap();
    }

    /// The verification boot re-renders the same VM with no media and no
    /// steps — it still has to boot the hardware the build was sealed on.
    #[test]
    fn declared_hardware_reaches_the_verification_boot() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  firmware  = \"ovmf\"\n",
            "  nested    = true\n",
            "  qemu_args = [\"-smbios\", \"type=1\"]\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = synth_lab(
            &d,
            &hw(&d),
            "build-t",
            "build",
            Path::new("/root"),
            BuildBoot::Bare,
        )
        .unwrap();
        let lf = crate::config::load_lab_source(&wcl, "<verify>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("verification lab must parse: {e:?}\n{wcl}"));
        let build = &lf.lab.vms[0];
        assert_eq!(build.firmware, Some(crate::config::model::Firmware::Ovmf));
        assert!(build.nested);
        assert_eq!(build.qemu_args, ["-smbios", "type=1"]);
    }

    /// A `qemu_args` entry with a quote or backslash has to survive the round
    /// trip through the rendered lab file (the same hazard `wcl_str` exists
    /// for on labels and paths).
    #[test]
    fn qemu_args_are_escaped() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  qemu_args = [\"-fw_cfg\", \"name=opt/x,string=say \\\"hi\\\"\"]\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        assert_eq!(
            lf.lab.vms[0].qemu_args,
            ["-fw_cfg", "name=opt/x,string=say \"hi\""]
        );
    }

    /// A template that declares no hardware renders no hardware: the build
    /// VM falls through to the profile floor (§5.2), as before.
    #[test]
    fn undeclared_hardware_is_not_rendered() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let build = &lf.lab.vms[0];
        assert!(build.display.is_none(), "{wcl}");
        assert!(build.firmware.is_none(), "{wcl}");
        assert!(build.tpm.is_none(), "{wcl}");
        assert!(build.secure_boot.is_none(), "{wcl}");
        assert!(!build.nested, "{wcl}");
        assert!(build.qemu_args.is_empty(), "{wcl}");
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
        let wcl = render(&d);
        assert!(!wcl.contains("firstboot.ws"), "{wcl}");
    }

    // ---- layered builds: the source template's recorded hardware ----------

    /// A layered build's source is the layer beneath the block, so a block
    /// that restates nothing inherits everything the source recorded
    /// (ADR-0009). Rebuilding a Windows 11 template used to boot the installed
    /// disk on `linux-generic` — SeaBIOS, no TPM — which an OVMF-installed
    /// guest does not survive.
    #[test]
    fn a_silent_block_inherits_the_sources_hardware() {
        let hw = EffectiveHardware::resolve(&layered(""), Some(&source_meta_full()));
        assert_eq!(hw.profile.as_deref(), Some("windows-11"));
        assert_eq!(hw.cpus, Some(4));
        assert_eq!(hw.memory, Some(8 << 30));
        assert_eq!(hw.firmware, Some(crate::config::model::Firmware::Ovmf));
        assert_eq!(hw.tpm, Some(true));
        assert_eq!(hw.secure_boot, Some(true));
        assert_eq!(hw.display.as_deref(), Some("virtio-vga"));
    }

    /// Where both layers declare a value the block wins — the same precedence
    /// a VM cloning that template gets (§5.2).
    #[test]
    fn the_block_wins_over_the_source() {
        let d = layered(concat!(
            "  profile     = \"linux-generic\"\n",
            "  cpus        = 2\n",
            "  memory      = 2GiB\n",
            "  firmware    = \"seabios\"\n",
            "  tpm         = false\n",
            "  secure_boot = false\n",
            "  display     = \"std\"\n",
        ));
        let hw = EffectiveHardware::resolve(&d, Some(&source_meta_full()));
        assert_eq!(hw.profile.as_deref(), Some("linux-generic"));
        assert_eq!(hw.cpus, Some(2));
        assert_eq!(hw.memory, Some(2 << 30));
        assert_eq!(hw.firmware, Some(crate::config::model::Firmware::Seabios));
        assert_eq!(hw.tpm, Some(false));
        assert_eq!(hw.secure_boot, Some(false));
        assert_eq!(hw.display.as_deref(), Some("std"));
    }

    /// A field neither layer declares stays absent: the profile beneath is a
    /// live layer, not something the merge freezes (ADR-0009).
    #[test]
    fn a_field_neither_layer_declares_stays_absent() {
        let hw = EffectiveHardware::resolve(&layered(""), Some(&source_meta()));
        assert_eq!(hw, EffectiveHardware::default());
        // …and the profile floor is applied at render time only.
        assert_eq!(hw.profile_or_default(), "linux-generic");
    }

    /// Metadata stores a firmware as free text, so a store entry can name one
    /// this build has no spelling for. It is dropped rather than rendered —
    /// the lab schema names exactly two, and an unknown one would fail the
    /// synthetic lab's own validation — and it takes the secure boot beside it
    /// with it. Keeping that alone would leave a secure-boot demand hanging
    /// over whatever firmware the profile floor supplies, which is the pair
    /// separating that ADR-0009 says to watch for: under `linux-generic` the
    /// pre-flight would refuse a build whose source demonstrably ran UEFI.
    #[test]
    fn an_unreadable_recorded_firmware_takes_its_secure_boot_with_it() {
        let meta = TemplateMeta {
            firmware: Some("uefi".into()),
            secure_boot: Some(true),
            ..source_meta()
        };
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        assert_eq!(hw.firmware, None);
        assert_eq!(hw.secure_boot, None);
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        super::check_build_hardware(&d, &hw, "build", Path::new("/root"), &profiles).unwrap();
    }

    /// The block's own values are never collateral: it gets the secure boot it
    /// asked for whatever the source recorded beside its unreadable firmware.
    #[test]
    fn the_blocks_own_secure_boot_survives_an_unreadable_source_firmware() {
        let meta = TemplateMeta {
            firmware: Some("uefi".into()),
            secure_boot: Some(true),
            ..source_meta()
        };
        let d = layered("  firmware = \"ovmf\"\n  secure_boot = true\n");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        assert_eq!(hw.firmware, Some(crate::config::model::Firmware::Ovmf));
        assert_eq!(hw.secure_boot, Some(true));
    }

    /// A source recording secure boot and no firmware at all is a different
    /// case: its profile carries over too, and that is where its firmware came
    /// from — so the demand still means something and is inherited.
    #[test]
    fn secure_boot_recorded_without_a_firmware_is_still_inherited() {
        let meta = TemplateMeta {
            profile: Some("linux-modern".into()), // OVMF
            secure_boot: Some(true),
            ..source_meta()
        };
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        assert_eq!(hw.firmware, None);
        assert_eq!(hw.secure_boot, Some(true));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        super::check_build_hardware(&d, &hw, "build", Path::new("/root"), &profiles).unwrap();
    }

    /// The inherited hardware has to reach the build VM, not just the merge:
    /// render it, parse it back, and read it off the VM that will boot.
    #[test]
    fn inherited_hardware_reaches_the_build_vm() {
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&source_meta_full()));
        let wcl = synth_lab(
            &d,
            &hw,
            "build-t",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap();
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let build = &lf.lab.vms[0];
        assert_eq!(build.profile.as_deref(), Some("windows-11"));
        assert_eq!(build.cpus, Some(4));
        assert_eq!(build.memory, Some(8 << 30));
        assert_eq!(build.firmware, Some(crate::config::model::Firmware::Ovmf));
        assert_eq!(build.tpm, Some(true));
        assert_eq!(build.secure_boot, Some(true));
        assert_eq!(build.display.as_deref(), Some("virtio-vga"));
    }

    /// A build that names no profile in either layer still renders the default
    /// floor, as before.
    #[test]
    fn no_profile_in_either_layer_still_renders_linux_generic() {
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&source_meta()));
        let wcl = synth_lab(
            &d,
            &hw,
            "build-t",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap();
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root")).unwrap();
        assert_eq!(lf.lab.vms[0].profile.as_deref(), Some("linux-generic"));
    }

    /// Inherited OVMF and secure boot travel together and pass the pre-flight:
    /// the merge runs before it, so what is checked is what will boot.
    #[test]
    fn inherited_ovmf_and_secure_boot_pass_the_pre_flight() {
        let mut meta = source_meta();
        meta.firmware = Some("ovmf".into());
        meta.secure_boot = Some(true);
        meta.tpm = Some(true);
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        super::check_build_hardware(&d, &hw, "build", Path::new("/root"), &profiles).unwrap();
    }

    /// …and an inherited pair that cannot work is refused by the pre-flight,
    /// before any artefact is downloaded — not at boot with secure boot
    /// silently dropped.
    #[test]
    fn inherited_secure_boot_under_seabios_refuses_the_build() {
        let mut meta = source_meta();
        meta.firmware = Some("seabios".into());
        meta.secure_boot = Some(true);
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let err = super::check_build_hardware(&d, &hw, "build", Path::new("/root"), &profiles)
            .expect_err("secure boot under SeaBIOS must refuse the build");
        let report = format!("{err:#}");
        assert!(report.contains("x86_64/t"), "{report}");
        assert!(report.contains("secure boot needs UEFI"), "{report}");
    }

    /// The sealed template records what it inherited, so a chain of layered
    /// rebuilds keeps the profile the first one recorded instead of losing a
    /// layer each time.
    #[test]
    fn sealed_metadata_records_the_inherited_hardware() {
        // The source records everything but the memory, which the block
        // declares itself.
        let meta = TemplateMeta {
            memory: None,
            ..source_meta_full()
        };
        let d = layered("  memory = 8GiB\n");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        let sealed = seal_meta(
            &d,
            &hw,
            "1.0",
            SealedImage {
                disk: 64 << 30,
                sha256: "ab".repeat(32),
                first_boot_script: None,
                agent_version: None,
            },
        );
        assert_eq!(sealed.profile.as_deref(), Some("windows-11"));
        assert_eq!(sealed.cpus, Some(4));
        assert_eq!(sealed.memory, Some(8 << 30)); // the block's own
        assert_eq!(sealed.firmware.as_deref(), Some("ovmf"));
        assert_eq!(sealed.tpm, Some(true));
        assert_eq!(sealed.secure_boot, Some(true));
        assert_eq!(sealed.display.as_deref(), Some("virtio-vga"));
        assert_eq!(sealed.disk, Some(64 << 30));
        assert_eq!(sealed.origin.as_deref(), Some("x86_64/win11@26100.1"));
        assert_eq!(
            sealed.wscript_surface,
            Some(crate::scripting::WSCRIPT_SURFACE_VERSION)
        );
    }

    /// Nothing profile-derived is frozen into the image: a field neither the
    /// block nor the source declared stays `None`, so the template still picks
    /// up a later edit to the profile it names (ADR-0009).
    #[test]
    fn sealed_metadata_freezes_no_profile_derived_value() {
        let mut meta = source_meta();
        meta.profile = Some("linux-modern".into()); // OVMF, virtio-vga, 2 cpus
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&meta));
        let sealed = seal_meta(
            &d,
            &hw,
            "1.0",
            SealedImage {
                disk: 20 << 30,
                sha256: "cd".repeat(32),
                first_boot_script: None,
                agent_version: None,
            },
        );
        assert_eq!(sealed.profile.as_deref(), Some("linux-modern"));
        assert_eq!(sealed.cpus, None);
        assert_eq!(sealed.memory, None);
        assert_eq!(sealed.firmware, None);
        assert_eq!(sealed.tpm, None);
        assert_eq!(sealed.secure_boot, None);
        assert_eq!(sealed.display, None);
    }

    /// The bootstrap ISO decision follows the *effective* profile: a layered
    /// build from a source whose profile has no agent channel must not stage
    /// an ISO the guest can never answer over.
    #[test]
    fn the_agent_iso_decision_follows_the_inherited_profile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vintage.wcl"),
            "import <vmlab-profile.wcl>\n\n\
             profile \"vintage\" {\n  description = \"no virtio-serial\"\n  \
             agent_channel = false\n}\n",
        )
        .unwrap();
        let profiles = crate::profiles::ProfileSet::load(dir.path()).unwrap();

        let mut meta = source_meta();
        meta.profile = Some("vintage".into());
        let d = layered("");
        assert!(!super::wants_agent(
            &d,
            &EffectiveHardware::resolve(&d, Some(&meta)),
            &profiles
        ));
        // The block's own profile still wins, so restating a modern one puts
        // the ISO back.
        let d = layered("  profile = \"linux-modern\"\n");
        assert!(super::wants_agent(
            &d,
            &EffectiveHardware::resolve(&d, Some(&meta)),
            &profiles
        ));
        // …as does a source that records nothing (the default floor).
        let d = layered("");
        assert!(super::wants_agent(&d, &hw(&d), &profiles));
    }
}
