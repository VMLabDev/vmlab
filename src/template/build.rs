//! Template builds (PRD §6.1): create a working qcow2, boot it per the
//! template's hardware, run wisp build provision scripts, seal, and move the
//! image + metadata into the store. A failed build leaves nothing behind.
//!
//! A build is modelled as a one-VM `scratch` lab whose primary disk is
//! pre-seeded from the source, so it reuses the entire lab runtime
//! (lifecycle, networking, the wisp build scripts). The build runs
//! in-process — no daemon — and seals by flattening the working disk into
//! the store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::meta::TemplateMeta;
use super::store::TemplateStore;
use crate::config::model::{ArtefactSource, TemplateDef, TemplateSource};
use crate::scripting::OutputSink;

/// Build `def` (from a parsed lab/template file rooted at `root`) and install
/// the result into `store`. `log` streams progress.
pub async fn build_template(
    def: &TemplateDef,
    root: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: OutputSink,
) -> Result<TemplateMeta> {
    log(format!(
        "building {}/{}@{}\n",
        def.arch, def.name, def.version
    ));

    if store.exists(&def.arch, &def.name, Some(&def.version)) {
        bail!(
            "{}/{}@{} already in the store — remove it first or bump the version",
            def.arch,
            def.name,
            def.version
        );
    }

    // Working area: a throwaway lab root under the artefact cache. Removed on
    // both success and failure, so nothing leaks.
    let work = build_workdir(def);
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let guard = WorkdirGuard(work.clone());

    let result = run_build(def, root, &work, store, profiles, &log).await;
    drop(guard); // always clean up the workdir
    result
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
) -> Result<TemplateMeta> {
    let disk_size = def.disk.unwrap_or(20 << 30);
    let build_vm = "build";

    // Resolve the source into the working primary disk.
    let (cdrom, seed_disk): (Option<PathBuf>, SeedDisk) = match &def.source {
        TemplateSource::Iso(src) => {
            let iso = resolve_artefact(src, log).await?;
            (Some(iso), SeedDisk::Blank(disk_size))
        }
        TemplateSource::Qcow2(src) => {
            let img = resolve_artefact(src, log).await?;
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
    let runtime = crate::labd::lab::LabRuntime::build(labfile, event_log, profiles).await?;

    let vm = runtime.vm(build_vm)?;
    let disk0 = vm.dirs.primary_disk();
    std::fs::create_dir_all(disk0.parent().unwrap())?;
    match &seed_disk {
        SeedDisk::Blank(size) => {
            super::qimg::create_blank(&disk0, *size).await?;
        }
        SeedDisk::CopyFrom(src) => {
            log(format!("seeding working disk from {}\n", src.display()));
            // Flatten/copy into a standalone working qcow2 (resized up to the
            // requested disk size if larger).
            super::qimg::convert_to_qcow2(src, &disk0).await?;
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
    runtime
        .up(&[], log.clone())
        .await
        .context("build boot/provision failed")?;

    log("sealing: graceful shutdown\n".to_string());
    vm.stop(false).await.context("build VM did not shut down")?;
    vm.wait_state(
        crate::labd::vm::PowerState::Stopped,
        std::time::Duration::from_secs(120),
    )
    .await?;

    // Seal: flatten the working disk into a staging dir, then install.
    let staging = work.join("staging");
    std::fs::create_dir_all(&staging)?;
    let sealed = staging.join("disk.qcow2");
    log("flattening sealed image\n".to_string());
    super::qimg::convert_to_qcow2(&disk0, &sealed).await?;

    let info = super::qimg::image_info(&sealed).await?;
    let sha = super::store::sha256_file(&sealed).context("hashing sealed image")?;
    let meta = TemplateMeta {
        name: def.name.clone(),
        arch: def.arch.clone(),
        version: def.version.clone(),
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
        sha256: Some(sha),
    };

    store
        .install(&staging, &meta, false)
        .context("installing into the store")?;
    log(format!(
        "installed {}/{}@{}\n",
        def.arch, def.name, def.version
    ));
    runtime.events.emit(
        "template.built",
        serde_json::json!({
            "arch": def.arch, "name": def.name, "version": def.version,
        }),
    );
    Ok(meta)
}

enum SeedDisk {
    Blank(u64),
    CopyFrom(PathBuf),
}

async fn resolve_artefact(src: &ArtefactSource, log: &OutputSink) -> Result<PathBuf> {
    let log = log.clone();
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
    let disk = def.disk.unwrap_or(20 << 30);
    writeln!(s, "    disk     = \"{}\"", disk).unwrap();
    if let Some(cpus) = def.cpus {
        writeln!(s, "    cpus     = {cpus}").unwrap();
    }
    if let Some(mem) = def.memory {
        writeln!(s, "    memory   = \"{mem}\"").unwrap();
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
}
