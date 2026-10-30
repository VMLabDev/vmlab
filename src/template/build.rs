//! Template builds (PRD §6.1): create a working qcow2, boot it per the
//! template's hardware, run wscript build provision scripts, seal, and move the
//! image + metadata into the store. A failed build leaves nothing behind.
//!
//! A build is modelled as a one-VM `scratch` lab whose primary disk is
//! pre-seeded from the source, so it reuses the entire lab runtime
//! (lifecycle, networking, the wscript build scripts). The build runs
//! in-process — no daemon — and seals by flattening the working disk into
//! the store. While it runs, that lab's control socket is served for
//! inspection and input ([`crate::labd::build_lab`]), so the machine verbs
//! reach the build VM by its [`BuildMachine`] reference.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::meta::TemplateMeta;
use super::store::{ResolvedTemplate, TemplateStore};
use crate::config::model::{
    ArtefactSource, Firmware, TemplateDef, TemplateRef, TemplateSource, TemplateVariantDef,
};
use crate::config::wcl_str;
use crate::labd::machine::Machine;
use crate::qemu::resolve::KeyStore;
use crate::scripting::OutputSink;

/// A build VM as the machine verbs address it, `<lab>/<vm>` (PRD §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildMachine {
    pub lab: String,
    pub vm: String,
}

impl BuildMachine {
    /// The `[lab/]vm` reference `vmlab vm screenshot` and its kin take.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.lab, self.vm)
    }

    /// The log line a build prints once its VM can be inspected: the
    /// reference, and the verbs that reach it.
    fn announcement(&self) -> String {
        let r = self.reference();
        format!(
            "build VM is `{r}`: `vmlab vm screenshot {r} <file.png>`, \
             `vmlab console {r}`, `vmlab vm sendkeys {r} <chord>`\n"
        )
    }
}

/// Called with the build VM once its display is up and its lab's socket is
/// served, and with `None` once neither is any longer.
pub type Inspectable = Arc<dyn Fn(Option<&BuildMachine>) + Send + Sync>;

/// Called for every structured event the synthetic build lab emits whose
/// kind starts with `playbook.` — the config-weave step stream (§10.4). The
/// supervisor forwards these as `template.op.step` so build UIs can render
/// per-step progress instead of opaque log lines.
pub type BuildEvent = Arc<dyn Fn(crate::proto::Event) + Send + Sync>;

/// Asked before a variant's overlay is replaced (§6.6): why the given disks
/// cannot go — the clones backed by them — or `None` when nothing leans on
/// them. A clone's data is a delta against the overlay it was made from, so
/// replacing that overlay underneath it would corrupt it.
pub type InUse = Arc<
    dyn Fn(
            Vec<PathBuf>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// Called as each variant's build starts, with the variant's name, so a
/// build UI can say which one is running.
pub type VariantStarted = Arc<dyn Fn(&str) + Send + Sync>;

/// Which build of a definition [`build_template`] runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildTarget<'a> {
    /// Pin this version instead of auto-incrementing; with `variants`, the
    /// installed version to build them onto.
    pub version: Option<&'a str>,
    /// Build only these variants onto an installed version (§6.6).
    pub variants: Option<&'a [String]>,
}

/// Optional controls supplied by an interactive build caller.
#[derive(Default)]
pub struct BuildControl {
    pub inspectable: Option<Inspectable>,
    pub on_event: Option<BuildEvent>,
    pub on_variant: Option<VariantStarted>,
    pub in_use: Option<InUse>,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Build `def` (from a parsed lab/template file rooted at `root`) and install
/// the result into `store`. `log` streams progress. The build version is
/// auto-incremented (PRD §6.4) unless `version_override` pins it.
///
/// A definition with variants (§6.6) seals and installs the base, then builds
/// each variant on it. With `target.variants`, nothing but those variants is
/// built, onto the newest installed version or the one `target.version`
/// pins. Either way the answer is the version's metadata as installed.
pub async fn build_template(
    def: &TemplateDef,
    root: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: OutputSink,
    target: BuildTarget<'_>,
    control: BuildControl,
) -> Result<TemplateMeta> {
    let BuildTarget {
        version: version_override,
        variants: only_variants,
    } = target;
    // The declaration is checked before anything is fetched or booted: a
    // base that builds for an hour and then has no default to install its
    // variants under helps nobody.
    let order = plan_variants(def, only_variants)?;
    if only_variants.is_some() {
        return build_onto_installed(
            def,
            root,
            store,
            profiles,
            &log,
            version_override,
            &order,
            &control,
        )
        .await;
    }
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
            control: &control,
        },
    )
    .await;
    drop(guard); // always clean up the workdir
    let meta = result?;
    if order.is_empty() {
        return Ok(meta);
    }
    build_variants(
        def,
        root,
        store,
        profiles,
        &log,
        &meta.version,
        &order,
        &control,
    )
    .await?;
    Ok(store
        .installed(&meta.arch, &meta.name, &meta.version)?
        .map(|v| v.meta)
        .unwrap_or(meta))
}

/// The variants a build builds, in the order it builds them (§6.6), after
/// checking the declaration the way §5.1 does — the build reads its file
/// without the lab validator, so it cannot assume it was checked.
///
/// The default variant goes first and the rest follow in declaration order.
/// A failed variant stops the build with every earlier one installed, and a
/// version holding two variants but not its default is invalid metadata — so
/// the default must already be in place before any other lands beside it.
///
/// `only` restricts the build to the variants named (`--variant`); each must
/// be declared.
fn plan_variants<'d>(
    def: &'d TemplateDef,
    only: Option<&[String]>,
) -> Result<Vec<&'d TemplateVariantDef>> {
    let reference = format!("{}/{}", def.arch, def.name);
    if def.variants.is_empty() {
        if only.is_some() {
            bail!("template {reference} declares no variants, so `--variant` has nothing to build");
        }
        return Ok(Vec::new());
    }
    let mut seen = std::collections::HashSet::new();
    for v in &def.variants {
        crate::config::model::check_variant_name(&v.name)
            .map_err(|why| anyhow::anyhow!("template {reference} variant \"{}\": {why}", v.name))?;
        if !seen.insert(v.name.as_str()) {
            bail!("template {reference} declares variant \"{}\" twice", v.name);
        }
    }
    let default = def.default_variant().with_context(|| {
        format!(
            "template {reference} declares {} variants but not exactly one with default = true — \
             mark the one a reference with no `#` selects",
            def.variants.len()
        )
    })?;
    let wanted = |v: &TemplateVariantDef| only.is_none_or(|names| names.contains(&v.name));
    if let Some(names) = only {
        if names.is_empty() {
            bail!("`--variant` names no variant");
        }
        for n in names {
            if def.variant(n).is_none() {
                bail!(
                    "template {reference} declares no variant `{n}` (its variants: {})",
                    def.variants
                        .iter()
                        .map(|v| v.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(std::iter::once(default)
        .filter(|d| wanted(d))
        .chain(
            def.variants
                .iter()
                .filter(|v| v.name != default.name && wanted(v)),
        )
        .collect())
}

/// `--variant`: build only `order` onto an installed version — the newest,
/// or the one `version` pins — leaving the base and every other variant as
/// they are.
#[allow(clippy::too_many_arguments)]
async fn build_onto_installed(
    def: &TemplateDef,
    root: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: &OutputSink,
    version: Option<&str>,
    order: &[&TemplateVariantDef],
    control: &BuildControl,
) -> Result<TemplateMeta> {
    let base = store
        .resolve_version(&def.arch, &def.name, version)
        .with_context(|| {
            format!(
                "cannot build variants of {}/{}: its base is not in the store — build the template \
             first",
                def.arch, def.name
            )
        })?;
    check_default_survives(def, &base.meta, order)?;
    build_variants(
        def,
        root,
        store,
        profiles,
        log,
        &base.meta.version,
        order,
        control,
    )
    .await?;
    Ok(store
        .installed(&def.arch, &def.name, &base.meta.version)?
        .map(|v| v.meta)
        .unwrap_or(base.meta))
}

/// Refuse a `--variant` build that would leave a version with several
/// variants and no default (§6.6): one adding a variant beside another while
/// the declared default is in neither the store nor the build.
fn check_default_survives(
    def: &TemplateDef,
    installed: &TemplateMeta,
    order: &[&TemplateVariantDef],
) -> Result<()> {
    let Some(default) = def.default_variant() else {
        return Ok(());
    };
    let mut after: Vec<&str> = installed.variants.iter().map(|v| v.name.as_str()).collect();
    for v in order {
        if !after.contains(&v.name.as_str()) {
            after.push(&v.name);
        }
    }
    // A block is installed default only when it is the definition's default,
    // and taking the default clears it elsewhere; so the store's default
    // moves only by building the declared one. Rebuilding the store's
    // default while the definition names another would leave none.
    if let Some(current) = installed.default_variant()
        && current.name != default.name
        && !order.iter().any(|v| v.name == default.name)
        && after.len() > 1
    {
        bail!(
            "{}/{}@{} has default variant `{}` but the definition names `{}` — include it to move \
             the default: `--variant {},{}`",
            installed.arch,
            installed.name,
            installed.version,
            current.name,
            default.name,
            default.name,
            order
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    if after.len() > 1 && !after.contains(&default.name.as_str()) {
        bail!(
            "{}/{}@{} would hold variants {} but not its default `{}`, which is not installed — \
             include it: `--variant {},{}`",
            installed.arch,
            installed.name,
            installed.version,
            after.join(", "),
            default.name,
            default.name,
            order
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    Ok(())
}

/// Build `order` onto `<arch>/<name>@<version>` one at a time. The first
/// failure stops it, naming the variant; the base and every variant
/// installed before it stay installed.
#[allow(clippy::too_many_arguments)]
async fn build_variants(
    def: &TemplateDef,
    root: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: &OutputSink,
    version: &str,
    order: &[&TemplateVariantDef],
    control: &BuildControl,
) -> Result<()> {
    let default = def
        .default_variant()
        .context("a template with variants has a default (checked by the plan)")?
        .name
        .clone();
    let total = order.len();
    for (i, v) in order.iter().enumerate() {
        if control.cancel.is_cancelled() {
            bail!("build cancelled");
        }
        let base = store
            .installed(&def.arch, &def.name, version)?
            .with_context(|| format!("{}/{}@{version} left the store", def.arch, def.name))?;
        let reference = format!("{}#{}", base.reference(), v.name);
        log(format!(
            "building variant {reference} ({}/{total})\n",
            i + 1
        ));
        if let Some(started) = &control.on_variant {
            started(&v.name);
        }
        // A rebuild replaces the overlay, and a clone of it is a delta
        // against the bytes being replaced: refuse before booting anything.
        let installed = base.variant_disk(&v.name);
        if installed.is_file() {
            refuse_in_use(control, &installed, &base.reference(), &v.name).await?;
        }
        let work = builds_dir().join(format!(
            "{}-{}-{}-variant-{}",
            def.arch, def.name, def.version, v.name
        ));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
        let guard = WorkdirGuard(work.clone());
        let built = build_variant(
            def,
            v,
            root,
            &work,
            store,
            profiles,
            log,
            VariantRun {
                base: &base,
                default: &default,
                control,
            },
        )
        .await;
        drop(guard);
        built.with_context(|| {
            format!(
                "variant `{}` failed to build — the base and every variant installed before it \
                 are kept; rebuild it with `vmlab template build {} --variant {}`",
                v.name, def.name, v.name
            )
        })?;
        log(format!("installed {reference}\n"));
    }
    Ok(())
}

struct VariantRun<'a> {
    /// The installed version the variant is built on.
    base: &'a super::store::TemplateVersion,
    /// The variant the definition marks as default.
    default: &'a str,
    control: &'a BuildControl,
}

/// Build one variant (§6.6): boot a linked overlay on the sealed base with
/// the variant's effective hardware, run its steps, shut down gracefully,
/// point the overlay's backing file at `../../disk.qcow2`, hash it, and
/// install it with its block.
#[allow(clippy::too_many_arguments)]
async fn build_variant(
    def: &TemplateDef,
    v: &TemplateVariantDef,
    root: &Path,
    work: &Path,
    store: &TemplateStore,
    profiles: &crate::profiles::ProfileSet,
    log: &OutputSink,
    run: VariantRun<'_>,
) -> Result<()> {
    let base = run.base;
    let control = run.control;
    let base_sha = base.meta.sha256.clone().with_context(|| {
        format!(
            "{} records no sha256 for its base disk, so no variant built on it could be vouched for",
            base.reference()
        )
    })?;
    let build_vm = "build";
    let hw = EffectiveHardware::for_variant(v, &base.meta).locate_vars(Some(&base.dir));
    let spec = BuildSpec::variant(def, v, base.meta.disk);
    check_spec_hardware(def, &spec, &hw, build_vm, root, profiles)?;

    let installed = base.variant_disk(&v.name);

    let wants_agent = wants_agent(def, &hw, profiles);
    let staged: Option<Arc<super::bootstrap::StagedGuestIso>> = if wants_agent {
        Some(Arc::new(super::bootstrap::stage_guest_iso_dir(
            work, &def.arch,
        )?))
    } else {
        None
    };

    let lab_name = build_lab_name(def);
    // No bootstrap ISO: the base already carries the agent it was sealed
    // with, and the staged copy is only what its handshake is checked
    // against.
    let lab_wcl = synth_spec_lab(
        &spec,
        &hw,
        &lab_name,
        build_vm,
        root,
        BuildBoot::Install {
            cdrom: None,
            guest_iso: None,
        },
    )?;
    std::fs::write(work.join("vmlab.wcl"), &lab_wcl)?;
    let labfile = crate::config::load_lab_source(&lab_wcl, "<variant build>", work)
        .map_err(|e| anyhow::anyhow!("internal variant build lab invalid: {e:?}"))?;
    let (events_tx, _) = tokio::sync::broadcast::channel::<crate::proto::Event>(256);
    bridge_playbook_events(&events_tx, control.on_event.clone());
    let event_log = Arc::new(crate::labd::events::EventLog::new(&lab_name, events_tx)?);
    let runtime = cancelable(
        &control.cancel,
        crate::labd::lab::LabRuntime::build(labfile, event_log, profiles),
    )
    .await?;
    let vm = runtime.vm(build_vm)?;
    runtime
        .provisions_wait_ready
        .store(false, std::sync::atomic::Ordering::Relaxed);
    runtime
        .agent_updates
        .store(false, std::sync::atomic::Ordering::Relaxed);
    // The base boots an installed OS whose agent answers promptly, as a
    // layered source does: verify it before the variant's first step.
    let agent_version: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    {
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
                        Some(std::time::Duration::from_secs(600)),
                        &log,
                    )
                    .await?;
                    *agent_version.lock().expect("agent_version lock") = version;
                    Ok(())
                })
            }));
    }
    // The build boot of a variant runs no first-boot script — not the
    // variant's, not the template's. The base was sealed without being
    // generalized (no sysprep), so it has no machine identity left to
    // specialize; the first-boot script belongs to the variant's clones,
    // which are. The scratch build VM carries none, and nothing sets one.

    let disk0 = vm.dirs.primary_disk();
    std::fs::create_dir_all(disk0.parent().expect("a disk has a parent"))?;
    cancelable(
        &control.cancel,
        super::qimg::create_linked_clone(&base.disk_path, &disk0),
    )
    .await?;

    log_build_gpu(&spec, &def.arch, log);
    log(format!("booting variant build VM ({})\n", v.name));
    if def.gui {
        crate::viewer::open_when_ready(vm.dirs.vnc_sock());
    }
    let inspection = Inspection::open(&runtime, &vm, control.inspectable.clone(), log).await?;
    let up_result = tokio::select! {
        result = runtime.up(&[], log.clone()) => Some(result),
        () = control.cancel.cancelled() => None,
    };
    let sealed = async {
        match up_result {
            Some(result) => result.context("variant build boot/provision failed")?,
            None => bail!("build cancelled"),
        }
        log("sealing variant: graceful shutdown\n".to_string());
        vm.stop(false)
            .await
            .context("variant build VM did not shut down")?;
        vm.wait_state(
            crate::labd::vm::PowerState::Stopped,
            std::time::Duration::from_secs(120),
        )
        .await?;
        Ok(())
    };
    if let Err(e) = sealed.await {
        let _ = runtime.down(&[], true).await;
        return Err(e);
    }
    if control.cancel.is_cancelled() {
        bail!("build cancelled");
    }
    let events = runtime.events.clone();
    drop(inspection);
    drop(runtime);

    // The overlay was written against the base by its absolute path; the
    // store keeps it beside the base and must survive being moved, so the
    // reference becomes relative. `-u`: the bytes behind it are the same.
    super::qimg::rebase_unsafe(&disk0, super::store::VARIANT_BACKING).await?;
    let sha = super::store::sha256_file(&disk0).context("hashing the variant overlay")?;
    let first_boot = v.first_boot.as_ref().or(def.first_boot.as_ref());
    let first_boot_script = match first_boot {
        Some(path) => {
            let full = root.join(path);
            Some(
                std::fs::read_to_string(&full)
                    .with_context(|| format!("reading first-boot script {}", full.display()))?,
            )
        }
        None => None,
    };
    let mut meta = variant_meta(
        v,
        VariantImage {
            sha256: sha,
            base_sha256: base_sha,
            first_boot_script,
            agent_version: agent_version.lock().expect("agent_version lock").clone(),
        },
    );
    // Checked again at the swap (first by the caller, before the boot): a
    // clone made while the variant was building is backed by the overlay
    // about to be replaced.
    if installed.is_file() {
        refuse_in_use(control, &installed, &base.reference(), &v.name).await?;
    }
    // The store's own installer takes a staged `variants/<name>/disk.qcow2`,
    // checks the digest, the base and the relative backing, moves it in and
    // merges the block — overlay first, block after. A default block takes
    // the default from whichever variant held it.
    meta.default = v.name == run.default;
    install_built_variant(store, base, &disk0, &work.join("variant-staging"), &meta)
        .context("installing the variant into the store")?;
    events.emit(
        "template.built",
        serde_json::json!({
            "arch": base.meta.arch, "name": base.meta.name, "version": base.meta.version,
            "variant": v.name,
        }),
    );
    Ok(())
}

/// Stage a built overlay as `<staging>/variants/<name>/disk.qcow2` and hand
/// it to the store, which verifies its digest, its base and its relative
/// backing file before moving it in and merging `meta` into `template.wcl`.
/// A refusal leaves the store as it was.
fn install_built_variant(
    store: &TemplateStore,
    base: &super::store::TemplateVersion,
    overlay: &Path,
    staging: &Path,
    meta: &super::meta::VariantMeta,
) -> Result<TemplateMeta> {
    let staged = staging.join(super::meta::variant_disk_rel(&meta.name));
    std::fs::create_dir_all(staged.parent().expect("a disk has a parent"))?;
    std::fs::rename(overlay, &staged).with_context(|| format!("staging {}", overlay.display()))?;
    store.add_variants(
        &base.meta.arch,
        &base.meta.name,
        &base.meta.version,
        staging,
        std::slice::from_ref(meta),
    )
}

/// Refuse to replace `disk` while the caller's in-use check names a clone
/// backed by it.
async fn refuse_in_use(
    control: &BuildControl,
    disk: &Path,
    reference: &str,
    variant: &str,
) -> Result<()> {
    if let Some(in_use) = &control.in_use
        && let Some(why) = in_use(vec![disk.to_path_buf()]).await
    {
        bail!(
            "refusing to rebuild {reference}#{variant}: {why} — rebuilding replaces the disk a \
             clone is a delta against; destroy those machines first"
        );
    }
    Ok(())
}

/// Facts about a built variant overlay that only exist once it has run.
struct VariantImage {
    sha256: String,
    base_sha256: String,
    first_boot_script: Option<String>,
    agent_version: Option<String>,
}

/// The block a built variant installs (§6.6). Hardware is what the variant
/// block declares and nothing else — an absent field inherits the base's at
/// resolution, so a variant follows the base it was built on. `default` is
/// left for the store to set from the definition at install.
fn variant_meta(v: &TemplateVariantDef, image: VariantImage) -> super::meta::VariantMeta {
    super::meta::VariantMeta {
        name: v.name.clone(),
        default: false,
        created: chrono::Utc::now(),
        sha256: image.sha256,
        base_sha256: image.base_sha256,
        description: v.description.clone(),
        first_boot_script: image.first_boot_script,
        agent_version: image.agent_version,
        wscript_surface: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
        profile: v.profile.clone(),
        cpus: v.cpus,
        memory: v.memory,
        firmware: v.firmware.map(|f| f.as_str().to_string()),
        tpm: v.tpm,
        secure_boot: v.secure_boot,
        secure_boot_keys: v.secure_boot_keys,
        display: v.display.clone(),
        cpu_model: v.cpu_model.clone(),
        hyperv: v.hyperv,
        apple_smc: v.apple_smc,
        input: v.input,
        tsc: v.tsc,
        accel: v.accel,
    }
}

/// Forward the synthetic build lab's `playbook.*` events — config-weave's
/// step stream — to the caller, who would otherwise never see them.
fn bridge_playbook_events(
    events_tx: &tokio::sync::broadcast::Sender<crate::proto::Event>,
    on_event: Option<BuildEvent>,
) {
    let Some(on_event) = on_event else { return };
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

/// A build lab served for inspection (PRD §6.1): its control socket bound,
/// and a watch that announces the VM once QEMU's display socket exists.
/// Dropping it closes the socket and tells the caller the VM is gone.
struct Inspection {
    _server: Option<crate::labd::build_lab::BuildLabServer>,
    watch: tokio::task::JoinHandle<()>,
    announced: Arc<std::sync::atomic::AtomicBool>,
    inspectable: Option<Inspectable>,
}

impl Inspection {
    /// Serve `runtime`'s lab and watch for `vm`'s display. A build whose lab
    /// name a running daemon already holds is not inspectable by name, and
    /// says so rather than taking the socket from that lab.
    async fn open(
        runtime: &Arc<crate::labd::lab::LabRuntime>,
        vm: &crate::labd::vm::VmInstance,
        inspectable: Option<Inspectable>,
        log: &OutputSink,
    ) -> Result<Self> {
        let machine = BuildMachine {
            lab: runtime.config().lab.name.clone(),
            vm: vm.name().to_string(),
        };
        let server = crate::labd::build_lab::serve(runtime).await?;
        let announced = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sock = vm.dirs.vnc_sock();
        let watch = {
            let (announced, inspectable, log) =
                (announced.clone(), inspectable.clone(), log.clone());
            let served = server.is_some();
            tokio::spawn(async move {
                while !sock.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if !served {
                    log(format!(
                        "lab `{}` is already running under its own daemon, so this build's \
                         VM cannot be inspected by name\n",
                        machine.lab
                    ));
                    return;
                }
                log(machine.announcement());
                announced.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(ready) = &inspectable {
                    ready(Some(&machine));
                }
            })
        };
        Ok(Self {
            _server: server,
            watch,
            announced,
            inspectable,
        })
    }
}

impl Drop for Inspection {
    fn drop(&mut self) {
        self.watch.abort();
        if self.announced.load(std::sync::atomic::Ordering::SeqCst)
            && let Some(gone) = &self.inspectable
        {
            gone(None);
        }
    }
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
    let hw = EffectiveHardware::resolve(def, layered.as_ref().map(|r| &r.meta))
        .locate_vars(layered.as_ref().map(|r| r.dir.as_path()));

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
    bridge_playbook_events(&events_tx, run.control.on_event.clone());
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
    // The build installs and verifies the agent it seals; `up` refreshing a
    // base template's agent over it mid-build would seal the wrong one.
    runtime
        .agent_updates
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let agent_version: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    let verify_concurrent = matches!(
        def.source,
        TemplateSource::Iso(_) | TemplateSource::Scratch { .. }
    );
    let verify_task: Option<AbortOnDrop<Result<Option<String>>>> = if verify_concurrent {
        let vm = vm.clone();
        let wants_agent = def.agent;
        let staged = staged.clone();
        let out = log.clone();
        // No deadline of its own: the provisions are the installer, and they
        // bound this wait. The build's deadline starts when they return.
        Some(AbortOnDrop(tokio::spawn(async move {
            while vm.state().await == crate::labd::vm::PowerState::Stopped {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let log = move |s: String| out(s);
            super::agent_install::verify(
                &(vm.clone() as Arc<dyn Machine>),
                wants_agent,
                staged.as_deref(),
                None,
                &log,
            )
            .await
        })))
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
                        Some(std::time::Duration::from_secs(600)),
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
            variant: parts.variant.clone(),
            template_vars: parts.template_vars.clone(),
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
    log_build_gpu(&BuildSpec::base(def), &def.arch, log);
    log("booting build VM\n".to_string());
    // `gui = true` builds get a viewer once QEMU creates the VNC socket;
    // up() below blocks through provisioning, so this watches concurrently.
    if def.gui {
        crate::viewer::open_when_ready(vm.dirs.vnc_sock());
    }
    let inspection = Inspection::open(&runtime, &vm, run.control.inspectable.clone(), log).await?;
    let up_result = tokio::select! {
        result = runtime.up(&[], log.clone()) => Some(result),
        () = run.control.cancel.cancelled() => None,
    };
    // Through graceful shutdown as one fallible step: a failed boot,
    // provision or seal must stop the build VM (QEMU/swtpm outlive the CLI
    // otherwise), not just delete its workdir.
    let booted = async {
        match up_result {
            Some(result) => result.context("build boot/provision failed")?,
            None => bail!("build cancelled"),
        }
        // In a live-handshake build (Windows FirstLogonCommands run while
        // the VM is still up) the background wait finished minutes ago, or
        // finishes within the grace below. An installer that powered the VM
        // off from a throwaway live session (subiquity) can never complete
        // it: the agent is installed into the target but has not run yet, so
        // boot the installed system for verification below.
        let mut verify_boot = false;
        if let Some(mut task) = verify_task {
            let stopped = vm.state().await == crate::labd::vm::PowerState::Stopped;
            let waited = match tokio::time::timeout(post_provision_grace(stopped), &mut task.0)
                .await
            {
                Ok(joined) => {
                    match joined.map_err(|e| anyhow::anyhow!("agent verify task panicked: {e}"))? {
                        Ok(version) => BackgroundWait::Answered(version),
                        Err(e) => BackgroundWait::Failed(e),
                    }
                }
                Err(_) => BackgroundWait::Pending,
            };
            match settle_background_wait(waited, wants_agent) {
                AgentSettle::Verified(version) => {
                    *agent_version.lock().expect("agent_version lock") = version;
                }
                AgentSettle::VerificationBoot => verify_boot = true,
                AgentSettle::Fail(e) => return Err(e),
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
    drop(inspection);
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
        let _inspection =
            Inspection::open(&runtime2, &vm2, run.control.inspectable.clone(), log).await?;
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
                Some(VERIFICATION_BOOT_AGENT_WAIT),
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
    let first_boot_script = match base_first_boot(def) {
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

    // A pre-enrolled key store is sealed beside the disk, so every clone's
    // variable store starts from it (§5.2) — the file the build booted with,
    // not the build VM's own store, which holds the build's boot entries.
    if let Some(KeyStore::File(path)) = &hw.key_store {
        let from = root.join(path);
        std::fs::copy(&from, staging.join(super::store::VARS_FILE))
            .with_context(|| format!("copying secure_boot_vars {}", from.display()))?;
    }
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

/// The first-boot script the base's own metadata embeds: the template's
/// `first_boot` — except on a template with variants, whose base is unsealed
/// and never cloned by a lab; there the template's `first_boot` is each
/// variant's to inherit (§6.6).
fn base_first_boot(def: &TemplateDef) -> Option<&PathBuf> {
    def.first_boot.as_ref().filter(|_| def.variants.is_empty())
}

enum SeedDisk {
    Blank(u64),
    CopyFrom(PathBuf),
}

struct BuildRun<'a> {
    version: &'a str,
    control: &'a BuildControl,
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

/// The synthetic lab a build runs in, named after the template it builds —
/// architecture included, like the working directory beside it.
///
/// A short name is not unique: `windows-vista` is one template built for
/// `x86_64` and another for `x86`, which is the whole point of a store ref
/// carrying an arch. Two such builds running at once took the same lab name
/// and therefore the same runtime directory, so they shared one set of QMP,
/// agent and VNC sockets — the second QEMU to start rebinds them and the
/// first build loses its control channel and hangs out its budget. Their
/// disks were always separate; only this name collided (2026-09-03).
fn build_lab_name(def: &TemplateDef) -> String {
    format!("build-{}-{}", def.arch, def.name)
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
    /// The key store the build's secure boot starts from (§5.2). A file —
    /// the block's `secure_boot_vars`, relative to the template root, or a
    /// source template's stored VARS once [`Self::locate_vars`] has found it
    /// — is sealed beside the disk for the clones.
    key_store: Option<KeyStore>,
    display: Option<String>,
    cpu_model: Option<String>,
    hyperv: Option<bool>,
    apple_smc: Option<bool>,
    input: Option<crate::config::model::InputDevices>,
    tsc: Option<crate::config::model::TscMode>,
    accel: Option<crate::config::model::AccelMode>,
}

impl EffectiveHardware {
    /// Merge the template block over `source` — the recorded metadata of the
    /// template a layered build layers on, `None` for every other source kind.
    fn resolve(def: &TemplateDef, source: Option<&TemplateMeta>) -> Self {
        Self::declared(def).over(source)
    }

    /// A variant's build hardware (§6.6): what the variant block declares over
    /// what its sealed base recorded — the same two-layer merge, with the base
    /// as the source. `secure_boot = false` on the variant reaches its build
    /// boot, not just its metadata.
    fn for_variant(v: &TemplateVariantDef, base: &TemplateMeta) -> Self {
        Self {
            profile: v.profile.clone(),
            cpus: v.cpus,
            memory: v.memory,
            firmware: v.firmware,
            tpm: v.tpm,
            secure_boot: v.secure_boot,
            key_store: v.secure_boot_keys.map(KeyStore::of_keys),
            display: v.display.clone(),
            cpu_model: v.cpu_model.clone(),
            hyperv: v.hyperv,
            apple_smc: v.apple_smc,
            input: v.input,
            tsc: v.tsc,
            accel: v.accel,
        }
        .over(Some(base))
    }

    /// Exactly what a template block declares, nothing inherited.
    fn declared(def: &TemplateDef) -> Self {
        Self {
            profile: def.profile.clone(),
            cpus: def.cpus,
            memory: def.memory,
            firmware: def.firmware,
            tpm: def.tpm,
            secure_boot: def.secure_boot,
            key_store: def
                .secure_boot_vars
                .clone()
                .map(KeyStore::File)
                .or(def.secure_boot_keys.map(KeyStore::of_keys)),
            display: def.display.clone(),
            cpu_model: def.cpu_model.clone(),
            hyperv: def.hyperv,
            apple_smc: def.apple_smc,
            input: def.input,
            tsc: def.tsc,
            accel: def.accel,
        }
    }

    /// Point a key store inherited from the source template at the VARS
    /// file the source carries in `dir`, its version directory.
    fn locate_vars(mut self, dir: Option<&Path>) -> Self {
        if let (Some(KeyStore::Template), Some(dir)) = (&self.key_store, dir) {
            self.key_store = Some(KeyStore::File(dir.join(super::store::VARS_FILE)));
        }
        self
    }

    /// This layer over `source`'s recorded hardware: each field this layer
    /// leaves unset is taken from the source, the firmware pair as a pair.
    fn over(self, source: Option<&TemplateMeta>) -> Self {
        let (source_firmware, source_secure_boot) = source_firmware(source);
        // The key store goes with the firmware pair: dropped beside a
        // firmware this build cannot read, inherited otherwise.
        let firmware_dropped = source
            .and_then(|m| m.firmware.as_deref())
            .is_some_and(|f| Firmware::parse(f).is_none());
        let source_key_store = source
            .filter(|_| !firmware_dropped)
            .and_then(|m| m.secure_boot_keys)
            .map(KeyStore::of_keys);
        Self {
            profile: self
                .profile
                .or_else(|| source.and_then(|m| m.profile.clone())),
            cpus: self.cpus.or_else(|| source.and_then(|m| m.cpus)),
            memory: self.memory.or_else(|| source.and_then(|m| m.memory)),
            firmware: self.firmware.or(source_firmware),
            tpm: self.tpm.or_else(|| source.and_then(|m| m.tpm)),
            secure_boot: self.secure_boot.or(source_secure_boot),
            key_store: self.key_store.or(source_key_store),
            display: self
                .display
                .or_else(|| source.and_then(|m| m.display.clone())),
            cpu_model: self
                .cpu_model
                .or_else(|| source.and_then(|m| m.cpu_model.clone())),
            hyperv: self.hyperv.or_else(|| source.and_then(|m| m.hyperv)),
            apple_smc: self.apple_smc.or_else(|| source.and_then(|m| m.apple_smc)),
            input: self.input.or_else(|| source.and_then(|m| m.input)),
            tsc: self.tsc.or_else(|| source.and_then(|m| m.tsc)),
            accel: self.accel.or_else(|| source.and_then(|m| m.accel)),
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
        variant,
    } = from
    else {
        bail!("layered build source must be a store reference");
    };
    // A layered `from` is a build context: `#base` names the base (§6.6).
    let mut resolved = store
        .resolve(
            arch,
            name,
            version.as_deref(),
            variant.as_ref(),
            crate::template::store::Purpose::Build,
        )
        .context("resolving layered build source")?;
    // `#base` seeds from the unsealed base of a template with variants, which
    // was never generalized: there is nothing for a first-boot script to
    // specialize, and on such a template the base carries none anyway. A
    // base-only template's `#base` is the same — the explicit spelling asks
    // for the disk alone. No `#`, or `#<variant>`, runs the selected disk's own
    // (effective) script.
    if matches!(variant, Some(crate::config::model::VariantRef::Base)) {
        resolved.meta.first_boot_script = None;
    }
    // The source's first-boot provision runs on the build boot; refuse a
    // surface this host cannot compile before anything is fetched or booted.
    if let Some(script) = resolved.meta.first_boot() {
        crate::scripting::ensure_wscript_surface_supported(
            &from.to_string(),
            script.surface_version,
        )
        .map_err(anyhow::Error::msg)?;
    }
    Ok(Some(resolved))
}

/// How long an installer-driven build still listens for the agent once its
/// provisions have returned with the VM up: an agent they installed answers
/// within it, the legacy tier's slow handshake included.
const POST_PROVISION_AGENT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long the verification boot waits for the installed system's agent.
const VERIFICATION_BOOT_AGENT_WAIT: std::time::Duration = std::time::Duration::from_secs(900);

/// A spawned task aborted when its handle is dropped. The background agent
/// wait has no deadline of its own, so every way out of a build must end it.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What the background agent wait of an ISO or scratch build came to, read
/// once the provisions have returned and [`post_provision_grace`] has passed.
#[derive(Debug)]
enum BackgroundWait {
    /// The agent answered (the version verified; `None` when skipped).
    Answered(Option<String>),
    /// The wait failed for a reason more waiting would not cure.
    Failed(anyhow::Error),
    /// The agent had not answered.
    Pending,
}

/// How a build settles its agent contract after the provisions.
#[derive(Debug)]
enum AgentSettle {
    Verified(Option<String>),
    /// Boot the installed disk once more, with no media and no provisions,
    /// and verify there.
    VerificationBoot,
    Fail(anyhow::Error),
}

/// The deadline an installer-driven build puts on its agent, which starts
/// when the provisions return, never at boot: an install runs as long as its
/// provisions take (Windows 9x Setup, a macOS install, past an hour), and a
/// wait timed from boot gave up under them. A VM still up gets
/// [`POST_PROVISION_AGENT_WAIT`] to answer; a VM its provisions powered off
/// gets none, since nothing is left running to answer.
fn post_provision_grace(vm_stopped: bool) -> std::time::Duration {
    if vm_stopped {
        std::time::Duration::ZERO
    } else {
        POST_PROVISION_AGENT_WAIT
    }
}

/// What the build does with its background agent wait (ADR-0003): an agent
/// that answered is the verified one; one that has not answered, whether the
/// VM is off or the grace ran out, is verified on a fresh boot of the
/// installed disk. Only a wait that failed outright fails the build.
fn settle_background_wait(wait: BackgroundWait, wants_agent: bool) -> AgentSettle {
    match wait {
        BackgroundWait::Answered(version) => AgentSettle::Verified(version),
        BackgroundWait::Failed(e) => AgentSettle::Fail(e),
        BackgroundWait::Pending if wants_agent => AgentSettle::VerificationBoot,
        BackgroundWait::Pending => AgentSettle::Verified(None),
    }
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
        .map(|p| p.agent_transport.has_channel())
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
    check_spec_hardware(def, &BuildSpec::base(def), hw, build_vm, root, profiles)
}

fn check_spec_hardware(
    def: &TemplateDef,
    spec: &BuildSpec<'_>,
    hw: &EffectiveHardware,
    build_vm: &str,
    root: &Path,
    profiles: &crate::profiles::ProfileSet,
) -> Result<()> {
    // Hardware-only probe: the bare render carries none of the build-time
    // attachments (extra disks, media, steps), and none of them can change the
    // hardware this resolves. Its own source label, so an issue reported
    // against the render names the render that produced it.
    let lab_name = build_lab_name(def);
    let wcl = synth_spec_lab(spec, hw, &lab_name, build_vm, root, BuildBoot::Bare)?;
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

/// Say which host GPU the build VM's `gpu {}` gets (§6.1), the way
/// `status -v` names a lab VM's: the same pick the lab daemon makes at the
/// start, run here beforehand so the line is in the build log whatever the
/// boot then does. A `host_gpu` this host lacks warns here as a lab's does.
/// A pick this host refuses is left to the start, which refuses with the
/// same words; nothing is said for a build with no `gpu {}`.
fn log_build_gpu(spec: &BuildSpec<'_>, arch: &str, log: &OutputSink) {
    let Some(gpu) = spec.gpu else {
        return;
    };
    let Ok(host) = crate::config::host::HostConfig::load_default() else {
        return;
    };
    let plan = crate::qemu::hostgpu::render_plan(
        Some(gpu),
        Some(arch),
        &host,
        &crate::qemu::hostgpu::Sysfs::host(),
        &crate::paths::data_dir(),
    );
    let Ok(plan) = plan else {
        return;
    };
    if let Some(warning) = &plan.warning {
        log(format!("warning: build VM: {warning}\n"));
    }
    log(gpu_log_line(gpu, &plan));
}

/// The build log's one line about the build VM's GPU: the mode asked for and
/// the host GPU the plan picked by name, `host_gpu=<name>` as `status -v`
/// spells it — or that nothing was picked, when the host config declares no
/// GPU for a paravirtual mode to render on.
fn gpu_log_line(
    gpu: &crate::config::model::Gpu,
    plan: &crate::qemu::hostgpu::RenderPlan,
) -> String {
    let mut line = format!("build VM gpu: mode={}", gpu.mode.as_str());
    match &plan.name {
        Some(name) => {
            use std::fmt::Write;
            write!(line, " host_gpu={name}").unwrap();
            if let Some(g) = &plan.gpu {
                write!(line, " render_node={}", g.render_node.display()).unwrap();
            }
            if let Some(b) = &plan.qemu_binary {
                write!(line, " qemu={}", b.display()).unwrap();
            }
        }
        None => line.push_str(" host_gpu=(none declared; QEMU's default render node)"),
    }
    line.push('\n');
    line
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
        secure_boot_keys: hw.key_store.as_ref().map(KeyStore::recorded),
        display: hw.display.clone(),
        cpu_model: hw.cpu_model.clone(),
        hyperv: hw.hyperv,
        apple_smc: hw.apple_smc,
        input: hw.input,
        tsc: hw.tsc,
        accel: hw.accel,
        created: chrono::Utc::now(),
        origin: source_origin(&def.source),
        registry: def.registry.clone(),
        sha256: Some(image.sha256),
        first_boot_script: image.first_boot_script,
        agent_version: image.agent_version,
        wscript_surface: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
        variants_expected: !def.variants.is_empty(),
        variants: Vec::new(),
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
    synth_spec_lab(&BuildSpec::base(def), hw, lab_name, vm, root, boot)
}

/// What a build VM is rendered from besides its hardware: the template's own
/// attachments and steps for the base build, a variant's for a variant build
/// (§6.6). Both describe one machine of one architecture.
struct BuildSpec<'a> {
    arch: &'a str,
    /// The primary disk's size. A variant build boots an overlay that already
    /// has the base's size, so this only has to agree with it.
    disk: u64,
    gui: bool,
    nested: bool,
    qemu_args: &'a [String],
    /// The build VM's `gpu {}` (§6.1): build-time hardware like `qemu_args`,
    /// resolved on the building host, never part of [`EffectiveHardware`]
    /// and so never sealed into the metadata — a clone's GPU is the lab's
    /// choice.
    gpu: Option<&'a crate::config::model::Gpu>,
    nics: &'a [crate::config::model::Nic],
    extra_disks: &'a [crate::config::model::DiskBlock],
    media: &'a [crate::config::model::Media],
    provisions: &'a [crate::config::model::Provision],
    playbooks: &'a [crate::config::model::Playbook],
}

impl<'a> BuildSpec<'a> {
    /// The base build: everything the template block declares.
    fn base(def: &'a TemplateDef) -> Self {
        Self {
            arch: &def.arch,
            disk: def.disk.unwrap_or(20 << 30),
            gui: def.gui,
            nested: def.nested,
            qemu_args: &def.qemu_args,
            gpu: def.gpu.as_ref(),
            nics: &def.nics,
            extra_disks: &def.extra_disks,
            media: &def.media,
            provisions: &def.provisions,
            playbooks: &def.playbooks,
        }
    }

    /// A variant's build on the sealed base, `base_disk` bytes big: the
    /// variant's own media and steps, its `qemu_args`, `gpu {}` and NICs
    /// where it declares them and the template's where it does not. The template's
    /// extra disks are build-time scratch for the base install and do not
    /// carry over; neither does its media — the installer's, by and large.
    fn variant(def: &'a TemplateDef, v: &'a TemplateVariantDef, base_disk: Option<u64>) -> Self {
        Self {
            arch: &def.arch,
            disk: base_disk.or(def.disk).unwrap_or(20 << 30),
            gui: def.gui,
            nested: def.nested,
            qemu_args: v.qemu_args.as_deref().unwrap_or(&def.qemu_args),
            gpu: v.gpu.as_ref().or(def.gpu.as_ref()),
            nics: if v.nics.is_empty() {
                &def.nics
            } else {
                &v.nics
            },
            extra_disks: &[],
            media: &v.media,
            provisions: &v.provisions,
            playbooks: &v.playbooks,
        }
    }
}

fn synth_spec_lab(
    def: &BuildSpec<'_>,
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
        key_store,
        display,
        cpu_model,
        hyperv,
        apple_smc,
        input,
        tsc,
        accel,
    } = hw;
    let mut s = String::from("import <vmlab.wcl>\n\n");
    writeln!(s, "lab {} {{", wcl_str(lab_name)).unwrap();
    writeln!(s, "  vm {} {{", wcl_str(vm)).unwrap();
    writeln!(s, "    template = \"scratch\"").unwrap();
    writeln!(s, "    arch     = {}", wcl_str(def.arch)).unwrap();
    writeln!(s, "    profile  = {}", wcl_str(hw.profile_or_default())).unwrap();
    // Bare integers: `disk`/`memory` are std.ByteSize in the schema, which
    // takes byte counts or size literals — never quoted strings.
    writeln!(s, "    disk     = {}", def.disk).unwrap();
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
    if let Some(c) = cpu_model {
        writeln!(s, "    cpu_model = {}", wcl_str(c)).unwrap();
    }
    if let Some(h) = hyperv {
        writeln!(s, "    hyperv   = {h}").unwrap();
    }
    if let Some(a) = apple_smc {
        writeln!(s, "    apple_smc = {a}").unwrap();
    }
    if let Some(i) = input {
        writeln!(s, "    input = {}", wcl_str(i.as_str())).unwrap();
    }
    if let Some(t) = tsc {
        writeln!(s, "    tsc = {}", wcl_str(t.as_str())).unwrap();
    }
    if let Some(a) = accel {
        writeln!(s, "    accel = {}", wcl_str(a.as_str())).unwrap();
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
    match key_store {
        None => {}
        Some(KeyStore::Microsoft) => writeln!(s, "    secure_boot_keys = \"microsoft\"").unwrap(),
        Some(KeyStore::Setup) => writeln!(s, "    secure_boot_keys = \"setup\"").unwrap(),
        Some(KeyStore::File(path)) => writeln!(
            s,
            "    secure_boot_vars = {}",
            wcl_str(root.join(path).display())
        )
        .unwrap(),
        Some(KeyStore::Template) => {
            bail!("the source template records a pre-enrolled VARS file the build cannot locate")
        }
    }
    if def.nested {
        writeln!(s, "    nested   = true").unwrap();
    }
    // A build is unattended, so nobody is there to wake a guest that idles
    // into S3 — a Windows client edition does, mid-install — and a sleeping
    // build VM looks exactly like a hung one. Every render keeps it awake.
    writeln!(s, "    prevent_sleep = true").unwrap();
    if !def.qemu_args.is_empty() {
        let args: Vec<String> = def.qemu_args.iter().map(wcl_str).collect();
        writeln!(s, "    qemu_args = [{}]", args.join(", ")).unwrap();
    }
    // The build VM's GPU (§6.1) lands on the vm block as a lab VM's would, so
    // the lab daemon picks the host GPU — by `host_gpu`, else the host's
    // default — with the render node, loader filter, PCI-filter layer and
    // the host config's emulator and environment that choice entails (§5.2).
    // Build-time only: `EffectiveHardware` does not carry it, so the sealed
    // metadata cannot.
    if let Some(gpu) = def.gpu {
        let mut attrs = format!("mode = {}", wcl_str(gpu.mode.as_str()));
        if let Some(name) = &gpu.host_gpu {
            write!(attrs, " host_gpu = {}", wcl_str(name)).unwrap();
        }
        writeln!(s, "    gpu {{ {attrs} }}").unwrap();
    }
    // Template-declared NICs carry over. The synthetic lab declares no
    // segments, so only NAT NICs make sense here — segment references are
    // rewritten to NAT. Builds with no NICs declared get internet egress by
    // default (agent/package install).
    if def.nics.is_empty() {
        writeln!(s, "    nic {{ nat = true }}").unwrap();
    } else {
        for n in def.nics {
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
        for d in def.extra_disks {
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
        for m in def.media {
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
                if let Some(d) = &p.guest_dir {
                    write!(s, " guest_dir = {}", wcl_str(d)).unwrap();
                }
                if let Some(b) = &p.guest_bin {
                    write!(s, " guest_bin = {}", wcl_str(b)).unwrap();
                }
                for v in &p.vars {
                    write!(
                        s,
                        " var {} {{ value = {}{} }}",
                        wcl_str(&v.name),
                        wcl_str(&v.value),
                        if v.secret { " secret = true" } else { "" }
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
        BuildBoot, EffectiveHardware, SealedImage, build_lab_name, seal_meta, sweep_workdirs_in,
        synth_lab, wcl_str,
    };
    use crate::config::model::SecureBootKeys;
    use crate::qemu::resolve::KeyStore;
    use crate::template::meta::TemplateMeta;
    use crate::template::store::{DISK_FILE, TemplateStore};
    use std::path::Path;

    /// Two builds of one short name — the same template for two
    /// architectures — must not share a lab, because the lab name is what
    /// picks the runtime directory holding the QMP, agent and VNC sockets.
    /// When they collided the second QEMU rebound them and the first build
    /// sat there until its budget ran out.
    #[test]
    fn a_build_lab_is_named_per_architecture() {
        let x64 = def(r#"import <vmlab.wcl>
               template "windows-vista" { arch = "x86_64" version = "1" profile = "windows-legacy"
                 disk = 40GiB source "iso" { path = "/i.iso" } }"#);
        let x86 = def(r#"import <vmlab.wcl>
               template "windows-vista" { arch = "x86" version = "1" profile = "windows-legacy"
                 disk = 40GiB source "iso" { path = "/i.iso" } }"#);
        assert_eq!(build_lab_name(&x64), "build-x86_64-windows-vista");
        assert_eq!(build_lab_name(&x86), "build-x86-windows-vista");
        assert_ne!(build_lab_name(&x64), build_lab_name(&x86));
    }

    #[test]
    fn a_build_machine_announces_the_verbs_that_reach_it() {
        let m = super::BuildMachine {
            lab: "build-x86_64-alpine".into(),
            vm: "build".into(),
        };
        assert_eq!(m.reference(), "build-x86_64-alpine/build");
        let line = m.announcement();
        for verb in [
            "vmlab vm screenshot build-x86_64-alpine/build",
            "vmlab console build-x86_64-alpine/build",
            "vmlab vm sendkeys build-x86_64-alpine/build",
        ] {
            assert!(line.contains(verb), "{line}");
        }
    }

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
            secure_boot_keys: None,
            display: None,
            cpu_model: None,
            hyperv: None,
            apple_smc: None,
            input: None,
            tsc: None,
            accel: None,
            created: "2026-01-02T03:04:05Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            variants_expected: false,
            variants: Vec::new(),
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
            cpu_model: Some("pentium3".into()),
            accel: Some(crate::config::model::AccelMode::Kvm),
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

    /// A template playbook's secret variable stays one in the build VM's
    /// block (#183), or the build's run would show its value.
    #[test]
    fn a_secret_variable_stays_secret_in_the_build_lab() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  playbook \"pb\" { play = \"b\"\n",
            "    var \"tz\" { value = \"UTC\" }\n",
            "    var \"pw\" { value = \"s3cr3t\" secret = true }\n",
            "  }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let vars = &lf.lab.vms[0].playbooks[0].vars;
        assert!(!vars[0].secret, "{wcl}");
        assert!(vars[1].secret, "{wcl}");
    }

    /// A template playbook's `guest_dir` and `guest_bin` (#169) reach the
    /// build VM's block — the build is where a read-only image needs them.
    #[test]
    fn playbook_guest_paths_carry_into_build_lab() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "  playbook \"pb\" { play = \"b\" guest_dir = \"/var/opt/w\" guest_bin = \"/usr/bin/config-weave\" }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
            .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"));
        let pb = &lf.lab.vms[0].playbooks[0];
        assert_eq!(pb.guest_dir.as_deref(), Some("/var/opt/w"));
        assert_eq!(pb.guest_bin.as_deref(), Some("/usr/bin/config-weave"));
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

    /// A build is unattended, so its VM keeps itself awake — a guest that
    /// idles into S3 mid-install stalls the build exactly like a hang. Every
    /// render, the verification boot included, declares it.
    #[test]
    fn the_build_vm_never_sleeps() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86_64\" version = \"1\"\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        for boot in [install(None), BuildBoot::Bare] {
            let wcl = synth_lab(&d, &hw(&d), "build-t", "build", Path::new("/root"), boot).unwrap();
            let lab = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root"))
                .unwrap_or_else(|e| panic!("synthetic build lab must parse: {e:?}\n{wcl}"))
                .lab;
            assert!(lab.vms[0].prevent_sleep, "{wcl}");
        }
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

    /// A template's `cpu_model` is hardware for its build VM too: Windows 9x
    /// Setup has to see the period CPU while it installs, not only the clones.
    #[test]
    fn a_declared_cpu_model_reaches_the_build_vm() {
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86\" version = \"1\"\n",
            "  profile   = \"windows-9x\"\n",
            "  cpu_model = \"pentium3\"\n",
            "  disk      = 1GiB\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root")).unwrap();
        assert_eq!(lf.lab.vms[0].cpu_model.as_deref(), Some("pentium3"));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let resolved = crate::qemu::resolve::resolve_vm(&lf.lab.vms[0], None, &profiles).unwrap();
        assert_eq!(resolved.cpu_model.as_deref(), Some("pentium3"));
    }

    /// A template's `hyperv` reaches its build VM and its sealed metadata;
    /// left unset, the build VM resolves the profile's like any VM (§5.2).
    #[test]
    fn hyperv_reaches_the_build_vm_and_the_metadata() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let build = |extra: &str| {
            let d = def(&format!(
                "import <vmlab.wcl>\ntemplate \"t\" {{ arch = \"x86_64\" version = \"1\"\n  \
                 profile = \"windows-server\"\n  disk = 1GiB\n{extra}  source \"scratch\" {{ }}\n}}\n"
            ));
            let wcl = render(&d);
            let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root")).unwrap();
            let vm = lf.lab.vms[0].clone();
            let resolved = crate::qemu::resolve::resolve_vm(&vm, None, &profiles).unwrap();
            (
                vm.hyperv,
                resolved.hyperv,
                EffectiveHardware::resolve(&d, None).hyperv,
            )
        };
        assert_eq!(build(""), (None, true, None));
        assert_eq!(
            build("  hyperv = false\n"),
            (Some(false), false, Some(false))
        );
    }

    /// A `cpu_model` inherited from a layered source meets the block's own
    /// `nested` at the build's pre-flight, which refuses the pair (§5.2).
    #[test]
    fn nested_over_an_inherited_cpu_model_refuses_the_build() {
        let d = layered("  nested = true\n");
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let hw = EffectiveHardware::resolve(&d, Some(&source_meta_full()));
        let err = super::check_build_hardware(&d, &hw, "build", Path::new("/root"), &profiles)
            .expect_err("nested over a named CPU model must refuse the build");
        let report = format!("{err:#}");
        assert!(report.contains("cpu_model = \"pentium3\""), "{report}");
        assert!(report.contains("nested = true"), "{report}");
    }

    /// A template's `accel` is hardware for its build VM too: Windows 95
    /// Setup has to install under TCG on an AMD host, not only its clones.
    #[test]
    fn a_declared_accel_reaches_the_build_vm() {
        use crate::config::model::AccelMode;
        let d = def(concat!(
            "import <vmlab.wcl>\n",
            "template \"t\" { arch = \"x86\" version = \"1\"\n",
            "  profile = \"windows-9x\"\n",
            "  accel   = \"tcg\"\n",
            "  disk    = 1GiB\n",
            "  source \"scratch\" { }\n",
            "}\n"
        ));
        let wcl = render(&d);
        let lf = crate::config::load_lab_source(&wcl, "<build>", Path::new("/root")).unwrap();
        assert_eq!(lf.lab.vms[0].accel, Some(AccelMode::Tcg));
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let resolved = crate::qemu::resolve::resolve_vm(&lf.lab.vms[0], None, &profiles).unwrap();
        assert_eq!(
            crate::qemu::plan_accel(&resolved, true).unwrap(),
            crate::qemu::AccelPlan::Tcg
        );
    }

    /// A `tcg` inherited from a layered source meets the block's own
    /// `nested` at the build's pre-flight, which refuses the pair (§5.2).
    #[test]
    fn nested_over_an_inherited_tcg_refuses_the_build() {
        let d = layered("  nested = true\n");
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let source = TemplateMeta {
            accel: Some(crate::config::model::AccelMode::Tcg),
            ..source_meta()
        };
        let hw = EffectiveHardware::resolve(&d, Some(&source));
        let err = super::check_build_hardware(&d, &hw, "build", Path::new("/root"), &profiles)
            .expect_err("nested over TCG must refuse the build");
        let report = format!("{err:#}");
        assert!(
            report.contains("nested = true but accel = \"tcg\""),
            "{report}"
        );
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
        assert_eq!(hw.cpu_model.as_deref(), Some("pentium3"));
        assert_eq!(hw.accel, Some(crate::config::model::AccelMode::Kvm));
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
            "  cpu_model   = \"pentium2\"\n",
            "  accel       = \"tcg\"\n",
        ));
        let hw = EffectiveHardware::resolve(&d, Some(&source_meta_full()));
        assert_eq!(hw.profile.as_deref(), Some("linux-generic"));
        assert_eq!(hw.cpus, Some(2));
        assert_eq!(hw.memory, Some(2 << 30));
        assert_eq!(hw.firmware, Some(crate::config::model::Firmware::Seabios));
        assert_eq!(hw.tpm, Some(false));
        assert_eq!(hw.secure_boot, Some(false));
        assert_eq!(hw.display.as_deref(), Some("std"));
        assert_eq!(hw.cpu_model.as_deref(), Some("pentium2"));
        assert_eq!(hw.accel, Some(crate::config::model::AccelMode::Tcg));
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

    fn sealed_image() -> SealedImage {
        SealedImage {
            disk: 64 << 30,
            sha256: "ab".repeat(32),
            first_boot_script: None,
            agent_version: None,
        }
    }

    /// A block's key store reaches the build VM and the sealed metadata:
    /// `setup` as itself, a `secure_boot_vars` file rendered rooted and
    /// recorded as `vars` (the file is sealed beside the disk).
    #[test]
    fn a_key_store_reaches_the_build_and_the_metadata() {
        let d = def(r#"import <vmlab.wcl>
               template "t" { arch = "x86_64" version = "1" disk = 10GiB firmware = "ovmf"
                 secure_boot = true secure_boot_keys = "setup" source "scratch" {} }"#);
        assert!(
            render(&d).contains("secure_boot_keys = \"setup\""),
            "{}",
            render(&d)
        );
        let sealed = seal_meta(&d, &hw(&d), "1", sealed_image());
        assert_eq!(sealed.secure_boot_keys, Some(SecureBootKeys::Setup));

        let d = def(r#"import <vmlab.wcl>
               template "t" { arch = "x86_64" version = "1" disk = 10GiB firmware = "ovmf"
                 secure_boot = true secure_boot_vars = "keys/v.fd" source "scratch" {} }"#);
        let wcl = render(&d);
        assert!(
            wcl.contains("secure_boot_vars = \"/root/keys/v.fd\""),
            "{wcl}"
        );
        let sealed = seal_meta(&d, &hw(&d), "1", sealed_image());
        assert_eq!(sealed.secure_boot_keys, Some(SecureBootKeys::Vars));

        // Nothing declared: nothing rendered, nothing recorded.
        let d = def(r#"import <vmlab.wcl>
               template "t" { arch = "x86_64" version = "1" disk = 10GiB firmware = "ovmf"
                 secure_boot = true source "scratch" {} }"#);
        assert!(!render(&d).contains("secure_boot_"), "{}", render(&d));
        assert_eq!(
            seal_meta(&d, &hw(&d), "1", sealed_image()).secure_boot_keys,
            None
        );
    }

    /// A layered build inherits its source's stored VARS: located in the
    /// source's version directory, booted, and recorded again as `vars`. A
    /// block naming its own key store wins over it.
    #[test]
    fn a_layered_build_inherits_the_sources_vars_file() {
        let source = TemplateMeta {
            secure_boot_keys: Some(SecureBootKeys::Vars),
            ..source_meta_full()
        };
        let d = layered("");
        let hw = EffectiveHardware::resolve(&d, Some(&source))
            .locate_vars(Some(Path::new("/store/x86_64/win11/26100.1")));
        assert_eq!(
            hw.key_store,
            Some(KeyStore::File(
                "/store/x86_64/win11/26100.1/OVMF_VARS.fd".into()
            ))
        );
        let wcl = synth_lab(
            &d,
            &hw,
            "build-t",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap();
        assert!(
            wcl.contains("secure_boot_vars = \"/store/x86_64/win11/26100.1/OVMF_VARS.fd\""),
            "{wcl}"
        );
        assert_eq!(
            seal_meta(&d, &hw, "1", sealed_image()).secure_boot_keys,
            Some(SecureBootKeys::Vars)
        );

        let d = layered("  secure_boot_keys = \"microsoft\"\n");
        let hw = EffectiveHardware::resolve(&d, Some(&source))
            .locate_vars(Some(Path::new("/store/x86_64/win11/26100.1")));
        assert_eq!(hw.key_store, Some(KeyStore::Microsoft));

        // Unlocated, the render refuses rather than booting Microsoft's keys.
        let hw = EffectiveHardware::resolve(&layered(""), Some(&source));
        assert!(
            synth_lab(
                &d,
                &hw,
                "build-t",
                "build",
                Path::new("/root"),
                install(None)
            )
            .is_err()
        );
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
        assert_eq!(sealed.cpu_model.as_deref(), Some("pentium3"));
        assert_eq!(sealed.accel, Some(crate::config::model::AccelMode::Kvm));
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
        assert_eq!(sealed.cpu_model, None);
        assert_eq!(sealed.accel, None);
    }

    /// A layered source whose first-boot provision targets a newer wscript
    /// surface is refused by name before anything is fetched or booted: the
    /// build boot would run that script.
    #[test]
    fn a_layered_source_from_a_newer_surface_is_refused_before_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let store = TemplateStore::new(dir.path().join("templates"));
        std::fs::create_dir_all(store.root()).unwrap();
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        std::fs::write(staging.path().join(DISK_FILE), b"").unwrap();
        let mut meta = source_meta();
        meta.first_boot_script = Some("use vmlab\nfn main(lab: Lab) {}\n".into());
        meta.wscript_surface = Some(6.into());
        store.install(&staging.keep(), &meta, false).unwrap();

        let err = super::resolve_layered_source(&layered(""), &store).unwrap_err();
        assert_eq!(
            err.to_string(),
            "template `x86_64/win11@26100.1` records wscript surface 6, newer than this host's \
             5; upgrade vmlab to use it"
        );

        meta.wscript_surface = Some(crate::scripting::WSCRIPT_SURFACE_VERSION);
        meta.version = "26100.2".into();
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        std::fs::write(staging.path().join(DISK_FILE), b"").unwrap();
        store.install(&staging.keep(), &meta, false).unwrap();
        let d = def("import <vmlab.wcl>\n\
             template \"t\" { arch = \"x86_64\" version = \"1\"\n\
             \x20 source \"template\" { from = \"x86_64/win11@26100.2\" }\n\
             }\n");
        assert!(super::resolve_layered_source(&d, &store).unwrap().is_some());
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
             agent_transport = \"none\"\n}\n",
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

    // ---- variants (§6.6) ----------------------------------------------------

    /// A template with variants, as SPEC.md's Windows example declares it.
    fn with_variants(variants: &str) -> crate::config::model::TemplateDef {
        def(&format!(
            "import <vmlab.wcl>\n\
             template \"win\" {{ arch = \"x86_64\" version = \"1\" disk = 1GiB\n\
             \x20 first_boot = \"scripts/firstboot.ws\"\n\
             \x20 source \"iso\" {{ path = \"/i.iso\" }}\n\
             \x20 media {{ kind = \"iso\" from = \"/drivers\" }}\n\
             \x20 disk \"scratch\" {{ size = 1GiB }}\n\
             \x20 provision \"scripts/install.ws\" {{ }}\n\
             {variants}\n\
             }}\n"
        ))
    }

    fn names(order: &[&crate::config::model::TemplateVariantDef]) -> Vec<String> {
        order.iter().map(|v| v.name.clone()).collect()
    }

    /// The default is built first, the rest in declaration order — a failure
    /// then never leaves two variants installed without their default.
    #[test]
    fn variants_build_default_first_then_in_declaration_order() {
        let d = with_variants(
            r#"variant "win3d" { } variant "standard" { default = true } variant "extra" { }"#,
        );
        let order = super::plan_variants(&d, None).unwrap();
        assert_eq!(names(&order), ["standard", "win3d", "extra"]);
        // Base-only: nothing to plan.
        let plain = def(r#"import <vmlab.wcl>
               template "p" { arch = "x86_64" version = "1" source "iso" { path = "/i.iso" } }"#);
        assert!(super::plan_variants(&plain, None).unwrap().is_empty());
    }

    /// `--variant` builds only what it names, each of which must be declared,
    /// and a definition without variants has nothing for it.
    #[test]
    fn only_the_named_variants_are_targeted() {
        let d = with_variants(
            r#"variant "standard" { default = true } variant "win3d" { } variant "extra" { }"#,
        );
        let only = ["extra".to_string(), "standard".to_string()];
        let order = super::plan_variants(&d, Some(&only)).unwrap();
        assert_eq!(names(&order), ["standard", "extra"]);
        let only = ["win3d".to_string()];
        assert_eq!(
            names(&super::plan_variants(&d, Some(&only)).unwrap()),
            ["win3d"]
        );

        let err = super::plan_variants(&d, Some(&["nope".to_string()])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "template x86_64/win declares no variant `nope` (its variants: standard, win3d, extra)"
        );
        let plain = def(r#"import <vmlab.wcl>
               template "p" { arch = "x86_64" version = "1" source "iso" { path = "/i.iso" } }"#);
        let err = super::plan_variants(&plain, Some(&["a".to_string()])).unwrap_err();
        assert!(err.to_string().contains("declares no variants"), "{err}");
    }

    /// The build reads its file without the lab validator, so it refuses a
    /// declaration §5.1 would have.
    #[test]
    fn the_build_refuses_a_declaration_without_one_default() {
        let d = with_variants(r#"variant "a" { } variant "b" { }"#);
        let err = super::plan_variants(&d, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("not exactly one with default = true"),
            "{err}"
        );
        let d = with_variants(r#"variant "base" { }"#);
        assert!(
            super::plan_variants(&d, None)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        let d = with_variants(r#"variant "a" { default = true } variant "a" { }"#);
        assert!(
            super::plan_variants(&d, None)
                .unwrap_err()
                .to_string()
                .contains("twice")
        );
    }

    /// A `--variant` build that would leave several variants installed and
    /// none of them the default is refused before anything boots.
    #[test]
    fn a_targeted_build_keeps_a_default_installed() {
        let d = with_variants(
            r#"variant "standard" { default = true } variant "win3d" { } variant "extra" { }"#,
        );
        let mut installed = source_meta();
        let order =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| s.to_string()).collect() };

        // Nothing installed: one variant alone is its own default.
        let only = order(&["win3d"]);
        let plan = super::plan_variants(&d, Some(&only)).unwrap();
        super::check_default_survives(&d, &installed, &plan).unwrap();

        // win3d installed, adding extra without standard: refused.
        installed.variants = vec![crate::template::meta::VariantMeta {
            name: "win3d".into(),
            default: false,
            created: "2026-01-02T03:04:05Z".parse().unwrap(),
            sha256: "cd".repeat(32),
            base_sha256: "ab".repeat(32),
            description: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            profile: None,
            cpus: None,
            memory: None,
            firmware: None,
            tpm: None,
            secure_boot: None,
            secure_boot_keys: None,
            display: None,
            cpu_model: None,
            hyperv: None,
            apple_smc: None,
            input: None,
            tsc: None,
            accel: None,
        }];
        let only = order(&["extra"]);
        let plan = super::plan_variants(&d, Some(&only)).unwrap();
        let err = super::check_default_survives(&d, &installed, &plan).unwrap_err();
        assert!(
            err.to_string().contains("`--variant standard,extra`"),
            "{err}"
        );
        // …and allowed with the default in the build, or rebuilding win3d alone.
        let only = order(&["extra", "standard"]);
        let plan = super::plan_variants(&d, Some(&only)).unwrap();
        super::check_default_survives(&d, &installed, &plan).unwrap();
        let only = order(&["win3d"]);
        let plan = super::plan_variants(&d, Some(&only)).unwrap();
        super::check_default_survives(&d, &installed, &plan).unwrap();

        // The store's default is `win3d` beside `standard`, and the definition
        // names `standard`: rebuilding `win3d` alone would leave no default.
        let mut standard = installed.variants[0].clone();
        standard.name = "standard".into();
        installed.variants[0].default = true;
        installed.variants.push(standard);
        let only = order(&["win3d"]);
        let plan = super::plan_variants(&d, Some(&only)).unwrap();
        let err = super::check_default_survives(&d, &installed, &plan).unwrap_err();
        assert!(
            err.to_string().contains("include it to move the default"),
            "{err}"
        );
        let only = order(&["win3d", "standard"]);
        let plan = super::plan_variants(&d, Some(&only)).unwrap();
        super::check_default_survives(&d, &installed, &plan).unwrap();
    }

    /// The base of a template with variants embeds no first-boot script: the
    /// template's `first_boot` is the variants' to inherit.
    #[test]
    fn a_base_with_variants_embeds_no_first_boot() {
        let d = with_variants(r#"variant "standard" { }"#);
        assert_eq!(super::base_first_boot(&d), None);
        let plain = def(r#"import <vmlab.wcl>
               template "p" { arch = "x86_64" version = "1" first_boot = "fb.ws"
                 source "iso" { path = "/i.iso" } }"#);
        assert_eq!(
            super::base_first_boot(&plain).map(|p| p.display().to_string()),
            Some("fb.ws".into())
        );
    }

    /// A definition that declares variants seals a base marked
    /// `variants_expected`, so no lab clones it before (or without) a
    /// variant; one that declares none does not.
    #[test]
    fn a_base_with_variants_is_marked_variant_bearing() {
        let image = || SealedImage {
            disk: 8 << 30,
            sha256: "ab".repeat(32),
            first_boot_script: None,
            agent_version: None,
        };
        let d = with_variants(r#"variant "standard" { }"#);
        let hw = EffectiveHardware::resolve(&d, None);
        let sealed = seal_meta(&d, &hw, "1", image());
        assert!(sealed.variants_expected);
        assert!(sealed.to_wcl().contains("variants_expected = true"));
        let err = sealed
            .select_variant(None, crate::template::store::Purpose::Lab)
            .unwrap_err();
        assert!(err.contains("variants are not built yet"), "{err}");
        let plain = def(r#"import <vmlab.wcl>
               template "p" { arch = "x86_64" version = "1"
                 source "iso" { path = "/i.iso" } }"#);
        let hw = EffectiveHardware::resolve(&plain, None);
        let sealed = seal_meta(&plain, &hw, "1", image());
        assert!(!sealed.variants_expected);
        assert_eq!(
            sealed.select_variant(None, crate::template::store::Purpose::Lab),
            Ok(None)
        );
    }

    /// The sealed base an OVMF + Secure Boot Windows build records.
    fn secure_base() -> TemplateMeta {
        let mut m = source_meta();
        m.profile = Some("windows-11".into());
        m.firmware = Some("ovmf".into());
        m.secure_boot = Some(true);
        m.tpm = Some(true);
        m.memory = Some(4 << 30);
        m.disk = Some(64 << 30);
        m.sha256 = Some("ab".repeat(32));
        m
    }

    /// A variant boots its build on the base's hardware under its own
    /// overrides: `secure_boot = false` reaches the build VM, the rest is the
    /// base's.
    #[test]
    fn a_variant_builds_on_its_effective_hardware() {
        let d = with_variants(r#"variant "win3d" { secure_boot = false cpus = 4 }"#);
        let v = &d.variants[0];
        let base = secure_base();
        let hw = EffectiveHardware::for_variant(v, &base);
        assert_eq!(hw.secure_boot, Some(false));
        assert_eq!(hw.firmware, Some(crate::config::model::Firmware::Ovmf));
        assert_eq!(hw.tpm, Some(true));
        assert_eq!(hw.cpus, Some(4));
        assert_eq!(hw.memory, Some(4 << 30));
        assert_eq!(hw.profile.as_deref(), Some("windows-11"));

        let spec = super::BuildSpec::variant(&d, v, base.disk);
        let wcl = super::synth_spec_lab(
            &spec,
            &hw,
            "build-x86_64-win",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap();
        assert!(wcl.contains("secure_boot = false"), "{wcl}");
        assert!(wcl.contains("firmware = \"ovmf\""), "{wcl}");
        assert!(
            wcl.contains(&format!("disk     = {}", 64u64 << 30)),
            "{wcl}"
        );
        // The base install's media, scratch disk and steps stay with the base.
        assert!(!wcl.contains("/drivers"), "{wcl}");
        assert!(!wcl.contains("disk \"scratch\""), "{wcl}");
        assert!(!wcl.contains("install.ws"), "{wcl}");
        crate::config::load_lab_source(&wcl, "<test>", Path::new("/root")).unwrap();

        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        super::check_spec_hardware(&d, &spec, &hw, "build", Path::new("/root"), &profiles)
            .map_err(|e| format!("{e:#}"))
            .unwrap();
    }

    /// A variant's own steps run in declaration order, and its `qemu_args`
    /// and NICs replace the template's where it declares them.
    #[test]
    fn a_variant_build_renders_its_own_steps() {
        let d = def(r#"import <vmlab.wcl>
               template "win" { arch = "x86_64" version = "1" qemu_args = ["-base"]
                 nic { mac = "52:54:00:00:00:01" }
                 source "iso" { path = "/i.iso" }
                 variant "a" { default = true provision "a1.ws" { } playbook "pb" { play = "p" } provision "a2.ws" { } }
                 variant "b" { qemu_args = ["-b"] nic { mac = "52:54:00:00:00:02" } }
               }"#);
        let a = super::BuildSpec::variant(&d, &d.variants[0], Some(1 << 30));
        let wcl =
            super::synth_spec_lab(&a, &hw(&d), "l", "build", Path::new("/root"), install(None))
                .unwrap();
        let (p1, pb, p2) = (
            wcl.find("/root/a1.ws").unwrap(),
            wcl.find("/root/pb").unwrap(),
            wcl.find("/root/a2.ws").unwrap(),
        );
        assert!(p1 < pb && pb < p2, "{wcl}");
        assert!(wcl.contains("\"-base\"") && wcl.contains(":01"), "{wcl}");
        let b = super::BuildSpec::variant(&d, &d.variants[1], Some(1 << 30));
        let wcl =
            super::synth_spec_lab(&b, &hw(&d), "l", "build", Path::new("/root"), install(None))
                .unwrap();
        assert!(wcl.contains("\"-b\"") && !wcl.contains("-base"), "{wcl}");
        assert!(wcl.contains(":02") && !wcl.contains(":01"), "{wcl}");
    }

    /// A template's `gpu {}` is the build VM's (§6.1): the rendered build lab
    /// carries it on the vm block as a lab VM would write it, so the lab
    /// daemon picks the host GPU for the build as it does for a lab; a
    /// variant's replaces the template's for its own build, and a variant
    /// declaring none builds on the template's.
    #[test]
    fn a_build_gpu_reaches_the_build_vm_and_a_variants_replaces_it() {
        let d = def(r#"import <vmlab.wcl>
               template "win" { arch = "x86_64" version = "1"
                 gpu { mode = "virgl" }
                 source "iso" { path = "/i.iso" }
                 variant "plain" { default = true }
                 variant "win3d" { gpu { mode = "vulkan" host_gpu = "AMD1" } }
               }"#);
        let base = render(&d);
        assert!(base.contains("    gpu { mode = \"virgl\" }\n"), "{base}");
        crate::config::load_lab_source(&base, "<test>", Path::new("/root")).unwrap();

        let plain = super::BuildSpec::variant(&d, &d.variants[0], Some(1 << 30));
        let wcl = super::synth_spec_lab(
            &plain,
            &hw(&d),
            "l",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap();
        assert!(wcl.contains("gpu { mode = \"virgl\" }"), "{wcl}");

        let win3d = super::BuildSpec::variant(&d, &d.variants[1], Some(1 << 30));
        let wcl = super::synth_spec_lab(
            &win3d,
            &hw(&d),
            "l",
            "build",
            Path::new("/root"),
            install(None),
        )
        .unwrap();
        assert!(
            wcl.contains("    gpu { mode = \"vulkan\" host_gpu = \"AMD1\" }\n"),
            "{wcl}"
        );
        assert!(!wcl.contains("virgl"), "{wcl}");
        let lf = crate::config::load_lab_source(&wcl, "<test>", Path::new("/root")).unwrap();
        let gpu = lf.lab.vms[0].gpu.as_ref().expect("the build VM's gpu");
        assert_eq!(gpu.mode, crate::config::model::GpuMode::Vulkan);
        assert_eq!(gpu.host_gpu.as_deref(), Some("AMD1"));
        // The profile's own display stays: the GL device line is the `gpu`'s
        // to emit at the start, not a `display` the metadata would record.
        assert!(!wcl.contains("display"), "{wcl}");
    }

    /// A build's `gpu {}` is never sealed into the metadata — not the base's
    /// block, not a variant's — because a clone's GPU is the lab's choice
    /// (§6.1); and so a clone of the variant gets the profile's plain display.
    #[test]
    fn a_build_gpu_is_not_recorded_for_clones() {
        let d = def(r#"import <vmlab.wcl>
               template "win" { arch = "x86_64" version = "1" profile = "windows-11"
                 gpu { mode = "virgl" }
                 source "iso" { path = "/i.iso" }
                 variant "win3d" { secure_boot = false gpu { mode = "vulkan" host_gpu = "AMD1" } }
               }"#);
        let hw = EffectiveHardware::resolve(&d, None);
        let mut sealed = seal_meta(
            &d,
            &hw,
            "1",
            SealedImage {
                disk: 8 << 30,
                sha256: "ab".repeat(32),
                first_boot_script: None,
                agent_version: None,
            },
        );
        sealed.variants.push(super::variant_meta(
            &d.variants[0],
            super::VariantImage {
                sha256: "cd".repeat(32),
                base_sha256: "ab".repeat(32),
                first_boot_script: None,
                agent_version: None,
            },
        ));
        let wcl = sealed.to_wcl();
        assert!(!wcl.contains("gpu"), "{wcl}");
        assert!(!wcl.contains("AMD1"), "{wcl}");
        assert!(!wcl.contains("display"), "{wcl}");
        let eff = sealed.effective(sealed.variants.first());
        assert_eq!(eff.display, None);
        assert_eq!(eff.secure_boot, Some(false));
    }

    /// The build log names the host GPU the build VM's `gpu {}` picked, as
    /// `status -v` does for a lab VM: `host_gpu=<name>` with the render node
    /// it resolved to, the host's default when the block names none, and a
    /// name this host lacks falls back with a warning rather than failing.
    #[test]
    fn the_build_log_names_the_picked_host_gpu() {
        let sys = crate::qemu::hostgpu::testing::fake_host();
        let host = crate::config::host::HostConfig::parse(
            r#"import <vmlab-host.wcl>
gpu "AMD1"    { address = "0000:72:00.0" }
gpu "Nvidia1" { address = "0000:01:00.0"  default = true }
"#,
            "<host>",
        )
        .unwrap();
        let plan = |gpu: &crate::config::model::Gpu| {
            crate::qemu::hostgpu::render_plan(
                Some(gpu),
                Some("x86_64"),
                &host,
                &crate::qemu::hostgpu::Sysfs::new(sys.path()),
                Path::new("/data"),
            )
            .unwrap()
        };
        let d = def(r#"import <vmlab.wcl>
               template "win" { arch = "x86_64" version = "1"
                 source "iso" { path = "/i.iso" }
                 variant "amd" { gpu { mode = "vulkan" host_gpu = "AMD1" } }
                 variant "default" { gpu { mode = "virgl" } }
                 variant "gone" { gpu { mode = "vulkan" host_gpu = "Nope" } }
               }"#);
        let gpu = |i: usize| d.variants[i].gpu.as_ref().unwrap();
        assert_eq!(
            super::gpu_log_line(gpu(0), &plan(gpu(0))),
            "build VM gpu: mode=vulkan host_gpu=AMD1 render_node=/dev/dri/renderD129\n"
        );
        assert_eq!(
            super::gpu_log_line(gpu(1), &plan(gpu(1))),
            "build VM gpu: mode=virgl host_gpu=Nvidia1 render_node=/dev/dri/renderD128\n"
        );
        let fallen = plan(gpu(2));
        assert!(
            fallen
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("Nope")),
            "{fallen:?}"
        );
        assert_eq!(
            super::gpu_log_line(gpu(2), &fallen),
            "build VM gpu: mode=vulkan host_gpu=Nvidia1 render_node=/dev/dri/renderD128\n"
        );
        // A host config declaring no GPU leaves QEMU's own render node.
        let none = crate::qemu::hostgpu::render_plan(
            Some(gpu(1)),
            Some("x86_64"),
            &crate::config::host::HostConfig::default(),
            &crate::qemu::hostgpu::Sysfs::new(sys.path()),
            Path::new("/data"),
        )
        .unwrap();
        assert_eq!(
            super::gpu_log_line(gpu(1), &none),
            "build VM gpu: mode=virgl host_gpu=(none declared; QEMU's default render node)\n"
        );
    }

    /// A variant's block records what it declares and nothing it inherits,
    /// so it follows the base it was built on.
    #[test]
    fn a_variant_block_records_only_its_overrides() {
        let d = with_variants(r#"variant "win3d" { secure_boot = false description = "3D" }"#);
        let m = super::variant_meta(
            &d.variants[0],
            super::VariantImage {
                sha256: "cd".repeat(32),
                base_sha256: "ab".repeat(32),
                first_boot_script: Some("fb".into()),
                agent_version: Some("agent=x".into()),
            },
        );
        assert_eq!(m.secure_boot, Some(false));
        assert_eq!(
            (m.firmware.as_ref(), m.tpm, m.memory, m.profile.as_ref()),
            (None, None, None, None)
        );
        assert_eq!(m.base_sha256, "ab".repeat(32));
        assert_eq!(m.description.as_deref(), Some("3D"));
        assert_eq!(m.first_boot_script.as_deref(), Some("fb"));
        assert_eq!(
            m.wscript_surface,
            Some(crate::scripting::WSCRIPT_SURFACE_VERSION)
        );
        // And a clone of it resolves to the base's hardware with the override.
        let mut base = secure_base();
        base.variants = vec![m];
        let eff = base.effective(base.variants.first());
        assert_eq!(
            (eff.secure_boot, eff.firmware.as_deref(), eff.tpm),
            (Some(false), Some("ovmf"), Some(true))
        );
    }

    fn have_qemu_img() -> bool {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qemu-img").is_file())
        })
    }

    fn qemu_img(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("qemu-img")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `x86_64/win11@26100.1` with a real base and two real overlays:
    /// `standard` (default, its own first-boot script at a surface this host
    /// does not have) and `extra`. The base records a first-boot script too.
    fn variant_store() -> (tempfile::TempDir, TemplateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = TemplateStore::new(tmp.path().join("templates"));
        let dir = store.root().join("x86_64/win11/26100.1");
        std::fs::create_dir_all(&dir).unwrap();
        qemu_img(&dir, &["create", "-q", "-f", "qcow2", DISK_FILE, "16M"]);
        let base_sha = crate::template::store::sha256_file(&dir.join(DISK_FILE)).unwrap();
        let mut m = source_meta();
        m.sha256 = Some(base_sha.clone());
        m.first_boot_script = Some("base script".into());
        m.wscript_surface = Some(crate::scripting::WSCRIPT_SURFACE_VERSION);
        for (name, default) in [("standard", true), ("extra", false)] {
            let vdir = dir.join("variants").join(name);
            std::fs::create_dir_all(&vdir).unwrap();
            qemu_img(
                &vdir,
                &[
                    "create",
                    "-q",
                    "-f",
                    "qcow2",
                    "-b",
                    "../../disk.qcow2",
                    "-F",
                    "qcow2",
                    DISK_FILE,
                ],
            );
            m.variants.push(crate::template::meta::VariantMeta {
                name: name.into(),
                default,
                created: "2026-01-02T03:04:05Z".parse().unwrap(),
                sha256: "cd".repeat(32),
                base_sha256: base_sha.clone(),
                description: None,
                first_boot_script: Some(format!("{name} script")),
                agent_version: None,
                wscript_surface: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
                profile: None,
                cpus: None,
                memory: (name == "extra").then_some(2 << 30),
                firmware: None,
                tpm: None,
                secure_boot: None,
                secure_boot_keys: None,
                display: None,
                cpu_model: None,
                hyperv: None,
                apple_smc: None,
                input: None,
                tsc: None,
                accel: None,
            });
        }
        m.write_to(&dir.join(crate::template::meta::META_FILE))
            .unwrap();
        (tmp, store)
    }

    fn layered_from(from: &str) -> crate::config::model::TemplateDef {
        def(&format!(
            "import <vmlab.wcl>\n\
             template \"t\" {{ arch = \"x86_64\" version = \"1\"\n\
             \x20 source \"template\" {{ from = \"{from}\" }}\n\
             }}\n"
        ))
    }

    /// A layered `from` is a build context (§6.6): no `#` seeds from the
    /// default variant and runs its first-boot script, `#<variant>` from that
    /// one, and `#base` from the base with no first-boot script at all.
    #[test]
    fn a_layered_build_seeds_from_the_selected_disk() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let seed = |from: &str| {
            super::resolve_layered_source(&layered_from(from), &store)
                .unwrap()
                .unwrap()
        };

        let r = seed("x86_64/win11");
        assert!(
            r.disk_path
                .ends_with("26100.1/variants/standard/disk.qcow2"),
            "{}",
            r.disk_path.display()
        );
        assert_eq!(
            r.meta.first_boot().map(|f| f.source).as_deref(),
            Some("standard script")
        );

        let r = seed("x86_64/win11#extra");
        assert!(r.disk_path.ends_with("variants/extra/disk.qcow2"));
        assert_eq!(
            r.meta.memory,
            Some(2 << 30),
            "the variant's effective hardware"
        );

        let r = seed("x86_64/win11@26100.1#base");
        assert!(
            r.disk_path.ends_with("26100.1/disk.qcow2"),
            "{}",
            r.disk_path.display()
        );
        assert!(r.variant.is_none());
        assert!(
            r.meta.first_boot().is_none(),
            "#base runs no first-boot script"
        );
    }

    /// The agent deadline of an installer-driven build starts when its
    /// provisions return: a VM still up is given the post-provision wait, a
    /// VM they powered off none (V5).
    #[test]
    fn the_post_provision_grace_depends_only_on_the_vm_being_up() {
        assert_eq!(
            super::post_provision_grace(false),
            super::POST_PROVISION_AGENT_WAIT
        );
        assert_eq!(super::post_provision_grace(true), std::time::Duration::ZERO);
    }

    /// An agent that has not answered by the end of the provisions sends the
    /// build to a verification boot — the VM powered off or not — and never
    /// fails it there and then. Only a wait that failed outright does (V5).
    #[test]
    fn an_unanswered_background_wait_settles_on_a_verification_boot() {
        use super::{AgentSettle, BackgroundWait, settle_background_wait};
        assert!(matches!(
            settle_background_wait(BackgroundWait::Pending, true),
            AgentSettle::VerificationBoot
        ));
        assert!(matches!(
            settle_background_wait(BackgroundWait::Pending, false),
            AgentSettle::Verified(None)
        ));
        assert!(matches!(
            settle_background_wait(BackgroundWait::Answered(Some("v1".into())), true),
            AgentSettle::Verified(Some(v)) if v == "v1"
        ));
        assert!(matches!(
            settle_background_wait(BackgroundWait::Failed(anyhow::anyhow!("no ISO")), true),
            AgentSettle::Fail(e) if e.to_string() == "no ISO"
        ));
    }

    /// The background wait has no deadline, so a build that leaves early —
    /// a failed boot or provision — must not strand it.
    #[tokio::test]
    async fn dropping_the_background_wait_aborts_it() {
        let task = super::AbortOnDrop(tokio::spawn(std::future::pending::<()>()));
        let abort = task.0.abort_handle();
        drop(task);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    /// The surface check reads the selected variant's script: a default whose
    /// script targets a newer surface refuses the layered build, while
    /// `#base` — which runs no script — does not.
    #[test]
    fn the_layered_surface_check_reads_the_selected_variant() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let path = store
            .root()
            .join("x86_64/win11/26100.1")
            .join(crate::template::meta::META_FILE);
        let mut m = TemplateMeta::read_from(&path).unwrap();
        m.variants[0].wscript_surface = Some(99.into());
        m.write_to(&path).unwrap();
        let err = super::resolve_layered_source(&layered_from("x86_64/win11"), &store).unwrap_err();
        assert!(
            err.to_string().contains("records wscript surface 99"),
            "{err}"
        );
        assert!(super::resolve_layered_source(&layered_from("x86_64/win11#extra"), &store).is_ok());
        assert!(super::resolve_layered_source(&layered_from("x86_64/win11#base"), &store).is_ok());
    }

    /// Seeding from a variant flattens overlay and base into one standalone
    /// working disk.
    #[tokio::test]
    async fn seeding_from_a_variant_flattens_the_chain() {
        if !have_qemu_img() {
            return;
        }
        let (tmp, store) = variant_store();
        let r = super::resolve_layered_source(&layered_from("x86_64/win11"), &store)
            .unwrap()
            .unwrap();
        let seeded = tmp.path().join("seeded.qcow2");
        crate::template::qimg::convert_to_qcow2(&r.disk_path, &seeded)
            .await
            .unwrap();
        let out = std::process::Command::new("qemu-img")
            .args(["info", "--output=json"])
            .arg(&seeded)
            .output()
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(info.get("backing-filename").is_none(), "{info}");
        assert_eq!(info["virtual-size"], 16 << 20);
    }

    /// A built overlay the way `build_variant` leaves one: written against
    /// the base by absolute path, then rebased `-u` to `../../disk.qcow2`.
    fn built_overlay(base: &Path, scratch: &Path, file: &str) -> std::path::PathBuf {
        let base = base.canonicalize().unwrap();
        qemu_img(
            scratch,
            &[
                "create",
                "-q",
                "-f",
                "qcow2",
                "-b",
                base.to_str().unwrap(),
                "-F",
                "qcow2",
                file,
            ],
        );
        qemu_img(
            scratch,
            &[
                "rebase",
                "-u",
                "-b",
                "../../disk.qcow2",
                "-F",
                "qcow2",
                file,
            ],
        );
        scratch.join(file)
    }

    fn built_block(
        name: &str,
        overlay: &Path,
        base_sha: &str,
        default: bool,
    ) -> crate::template::meta::VariantMeta {
        let d = with_variants(&format!(r#"variant "{name}" {{ memory = 3GiB }}"#));
        let mut m = super::variant_meta(
            &d.variants[0],
            super::VariantImage {
                sha256: crate::template::store::sha256_file(overlay).unwrap(),
                base_sha256: base_sha.into(),
                first_boot_script: None,
                agent_version: None,
            },
        );
        m.default = default;
        m
    }

    /// A rebuilt variant replaces its overlay and its block together and
    /// leaves the other variant alone; a new default takes the default over;
    /// one built on another base is refused with the store unchanged.
    #[test]
    fn a_built_variant_installs_atomically() {
        if !have_qemu_img() {
            return;
        }
        use crate::template::store::sha256_file;
        let (tmp, store) = variant_store();
        let base = store
            .installed("x86_64", "win11", "26100.1")
            .unwrap()
            .unwrap();
        let base_sha = base.meta.sha256.clone().unwrap();
        let standard_before = sha256_file(&base.variant_disk("standard")).unwrap();
        let work = |n: &str| {
            let w = tmp.path().join(n);
            std::fs::create_dir_all(&w).unwrap();
            w
        };

        let w = work("extra");
        let overlay = built_overlay(&base.disk_path, &w, "disk0.qcow2");
        let block = built_block("extra", &overlay, &base_sha, false);
        let digest = block.sha256.clone();
        let meta =
            super::install_built_variant(&store, &base, &overlay, &w.join("staging"), &block)
                .unwrap();
        let extra = meta.variant("extra").unwrap();
        assert_eq!(
            (extra.sha256.as_str(), extra.memory),
            (digest.as_str(), Some(3 << 30))
        );
        assert_eq!(meta.default_variant().unwrap().name, "standard");
        assert_eq!(sha256_file(&base.variant_disk("extra")).unwrap(), digest);
        assert_eq!(
            sha256_file(&base.variant_disk("standard")).unwrap(),
            standard_before,
            "the other variant is untouched"
        );
        let extra_ref = crate::config::model::VariantRef::Named("extra".into());
        let r = store
            .resolve(
                "x86_64",
                "win11",
                None,
                Some(&extra_ref),
                crate::template::store::Purpose::Lab,
            )
            .unwrap();
        assert_eq!(r.meta.memory, Some(3 << 30));

        // A new variant that is the definition's default takes it over.
        let w = work("third");
        let overlay = built_overlay(&base.disk_path, &w, "disk0.qcow2");
        let block = built_block("third", &overlay, &base_sha, true);
        let meta =
            super::install_built_variant(&store, &base, &overlay, &w.join("staging"), &block)
                .unwrap();
        assert_eq!(meta.default_variant().unwrap().name, "third");
        assert!(!meta.variant("standard").unwrap().default);

        // Built on another base: refused, nothing moves.
        let wcl_path = base.dir.join(crate::template::meta::META_FILE);
        let wcl = std::fs::read_to_string(&wcl_path).unwrap();
        let w = work("stale");
        let overlay = built_overlay(&base.disk_path, &w, "disk0.qcow2");
        let block = built_block("extra", &overlay, &"ef".repeat(32), false);
        assert!(
            super::install_built_variant(&store, &base, &overlay, &w.join("staging"), &block)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&wcl_path).unwrap(), wcl);
        assert_eq!(sha256_file(&base.variant_disk("extra")).unwrap(), digest);
    }
}
