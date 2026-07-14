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

/// Optional controls supplied by an interactive build caller.
#[derive(Default)]
pub struct BuildControl {
    pub console_ready: Option<ConsoleReady>,
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
    // specialize/OOBE on this boot, and QGA answers while that is still
    // running — sealing then would capture a half-specialized image.
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

    // Synthesize a one-VM scratch lab for the build.
    let lab_name = format!("build-{}", def.name);
    let lab_wcl = synth_lab(def, &lab_name, build_vm, cdrom.as_deref(), root)?;
    std::fs::write(work.join("vmlab.wcl"), &lab_wcl)?;

    let labfile = crate::config::load_lab_source(&lab_wcl, "<build>", work)
        .map_err(|e| anyhow::anyhow!("internal build lab invalid: {e:?}"))?;

    // Build the runtime; then pre-seed the working disk before `up` creates
    // the (otherwise blank) scratch disk.
    let (events_tx, _) = tokio::sync::broadcast::channel(256);
    let event_log = Arc::new(crate::labd::events::EventLog::new(&lab_name, events_tx)?);
    let runtime = cancelable(
        &run.control.cancel,
        crate::labd::lab::LabRuntime::build(labfile, event_log, profiles),
    )
    .await?;

    // Bake the vmlab-agent right after boot, before any provision script —
    // a final sysprep/generalize provision may shut the guest down, so
    // post-provision would be too late. The verified asset version lands in
    // the sealed metadata below.
    let agent_version: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    {
        let wants_agent = def.agent;
        let arch = def.arch.clone();
        let agent_version = agent_version.clone();
        *runtime.pre_provision.write().expect("pre_provision lock") =
            Some(Arc::new(move |vm, out| {
                let arch = arch.clone();
                let agent_version = agent_version.clone();
                Box::pin(async move {
                    let log = move |s: String| out(s);
                    let version =
                        super::agent_install::install(&vm, wants_agent, &arch, &log).await?;
                    *agent_version.lock().expect("agent_version lock") = version;
                    Ok(())
                })
            }));
    }

    let vm = runtime.vm(build_vm)?;

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
        log("sealing: graceful shutdown\n".to_string());
        vm.stop(false).await.context("build VM did not shut down")?;
        vm.wait_state(
            crate::labd::vm::PowerState::Stopped,
            std::time::Duration::from_secs(120),
        )
        .await
    };
    if let Err(e) = booted.await {
        let _ = runtime.down(&[], true).await;
        return Err(e);
    }

    if run.control.cancel.is_cancelled() {
        bail!("build cancelled");
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
    runtime.events.emit(
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
fn synth_lab(
    def: &TemplateDef,
    lab_name: &str,
    vm: &str,
    cdrom: Option<&Path>,
    root: &Path,
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
    // relative to the original file's root.
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
    writeln!(s, "  }}").unwrap();
    // Build provision scripts run against the single build VM (§10.4).
    for p in &def.provisions {
        let script = root.join(&p.script);
        writeln!(s, "  provision \"{}\" {{ }}", script.display()).unwrap();
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
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root")).unwrap();
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
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root")).unwrap();
        assert!(wcl.contains("nic { nat = true }"), "{wcl}");
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
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root")).unwrap();
        crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
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
        let wcl = synth_lab(&d, "build-t", "build", None, Path::new("/root")).unwrap();
        assert!(!wcl.contains("firstboot.ws"), "{wcl}");
    }
}
