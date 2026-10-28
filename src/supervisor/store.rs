//! The template store and the OCI registries it talks to, as supervisor
//! commands (PRD §3, §6).
//!
//! PRD §3 gives the supervisor "serialised writes to the template store
//! (pulls, builds, imports …; reads are lock-free)". This module is that
//! ownership in code: every `store.*` and `registry.*` command lands here, and
//! nothing above the supervisor opens the store itself. The store's own
//! `flock` still does the serialising — routing is about there being one
//! implementation, not about safety.
//!
//! Reads route for the same reason and take no lock, exactly as they did when
//! the CLI ran them in-process.
//!
//! **Failures answer with [`ErrorCode::Failed`]**, the code for "attempted,
//! and it failed". That is deliberate rather than lazy: these commands
//! replaced in-process CLI work whose every failure exited 1, and `Failed` is
//! the code that still exits 1. A wrongly *shaped* request is still an
//! `invalid_argument` — the wire decoder answers that before a handler runs.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::Supervisor;
use super::backing;
use super::templates::op_sink;
use crate::config::model::{TemplateRef, VariantRef, parse_template_ref};
use crate::proto::{CommandError, Event};
use crate::template::TemplateStore;
use crate::template::meta::TemplateMeta;
use crate::template::store_view::{
    InstalledVersion, PrunePlan, PushStarted, RemoteStatus, StoreEntry, StoredVersion,
    TemplateSummary, VariantSummary,
};

/// Serialise a typed answer, which cannot realistically fail — every view in
/// `store_view` is plain data.
fn answer<T: serde::Serialize>(value: T) -> Result<Value, CommandError> {
    serde_json::to_value(value).map_err(|e| CommandError::internal(e.to_string()))
}

fn store() -> TemplateStore {
    TemplateStore::new(crate::paths::template_store_dir())
}

/// A local store reference, split.
struct StoreRef {
    arch: String,
    name: String,
    version: Option<String>,
    variant: Option<VariantRef>,
}

/// Split `<arch>/<name>[@<version>][#<variant>]`, refusing anything that is
/// not a local store reference.
fn parse_store_ref(reference: &str) -> Result<StoreRef> {
    match parse_template_ref(reference).map_err(|e| anyhow!(e))? {
        TemplateRef::Store {
            arch,
            name,
            version,
            variant,
        } => Ok(StoreRef {
            arch,
            name,
            version,
            variant,
        }),
        _ => bail!("expected a local store reference `<arch>/<name>[@<version>]`"),
    }
}

/// Split a reference that names a whole version, refusing a `#<variant>`.
fn parse_version_ref(reference: &str) -> Result<(String, String, Option<String>)> {
    let r = parse_store_ref(reference)?;
    if let Some(v) = r.variant {
        bail!(
            "`{reference}` names one variant (`#{v}`); this command works on a whole version — \
             drop the `#{v}`"
        );
    }
    Ok((r.arch, r.name, r.version))
}

fn disk_path(store: &TemplateStore, t: &TemplateMeta) -> PathBuf {
    store
        .root()
        .join(&t.arch)
        .join(&t.name)
        .join(&t.version)
        .join(crate::template::store::DISK_FILE)
}

/// The base disk, then every variant overlay, of one listed version.
fn disks(store: &TemplateStore, t: &TemplateMeta) -> Vec<PathBuf> {
    let base = disk_path(store, t);
    let dir = base.parent().map(PathBuf::from).unwrap_or_default();
    let mut out = vec![base];
    out.extend(
        t.variants
            .iter()
            .map(|v| dir.join(crate::template::meta::variant_disk_rel(&v.name))),
    );
    out
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Everything one version occupies: the base and its variant overlays.
fn disk_size(store: &TemplateStore, t: &TemplateMeta) -> u64 {
    disks(store, t).iter().map(|d| file_size(d)).sum()
}

/// A listed version's variants as `template list` shows them: the default
/// marked, a stale one flagged, each overlay's own size.
fn variant_rows(store: &TemplateStore, t: &TemplateMeta) -> Vec<VariantSummary> {
    let default = t.default_variant().map(|d| d.name.clone());
    let dir = disk_path(store, t)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    t.variants
        .iter()
        .map(|v| VariantSummary {
            name: v.name.clone(),
            default: default.as_deref() == Some(v.name.as_str()),
            stale: t.is_stale(v),
            description: v.description.clone(),
            created: v.created.to_rfc3339(),
            sha256: v.sha256.clone(),
            size: file_size(&dir.join(crate::template::meta::variant_disk_rel(&v.name))),
        })
        .collect()
}

/// `store.list`: every template in the store with its disk size, and — when
/// asked — whether that exact version and architecture is already published.
pub async fn list(remote: bool) -> Result<Value, CommandError> {
    let (templates, sizes, variants) = tokio::task::spawn_blocking(move || -> Result<_> {
        let store = store();
        let templates = store.list()?;
        let sizes: Vec<u64> = templates
            .iter()
            .map(|t| file_size(&disk_path(&store, t)))
            .collect();
        let variants: Vec<Vec<VariantSummary>> =
            templates.iter().map(|t| variant_rows(&store, t)).collect();
        Ok((templates, sizes, variants))
    })
    .await
    .map_err(|e| e.to_string())??;

    let statuses: Vec<RemoteStatus> = if remote {
        use futures::StreamExt as _;
        // Owned triples first: the futures outlive the borrow of `templates`.
        let asked: Vec<(Option<String>, String, String)> = templates
            .iter()
            .map(|t| (t.registry.clone(), t.version.clone(), t.arch.clone()))
            .collect();
        futures::stream::iter(
            asked
                .into_iter()
                .map(|(registry, version, arch)| registry_status(registry, version, arch)),
        )
        .buffered(8)
        .collect()
        .await
    } else {
        Vec::new()
    };

    let rows: Vec<StoreEntry> = templates
        .iter()
        .enumerate()
        .map(|(i, t)| StoreEntry {
            meta: TemplateSummary::from(t),
            size: sizes[i],
            remote: statuses.get(i).copied(),
            variants: variants[i].clone(),
        })
        .collect();
    answer(rows)
}

/// Whether the registry a template names already carries this exact version
/// and architecture.
async fn registry_status(registry: Option<String>, version: String, arch: String) -> RemoteStatus {
    let Some(reg) = registry else {
        return RemoteStatus::Local;
    };
    let Ok(r) = crate::oci::Registry::new(&reg) else {
        return RemoteStatus::Unknown;
    };
    match r.index_arches(&version).await {
        Ok(arches) if arches.contains(&arch) => RemoteStatus::Published,
        _ => RemoteStatus::Missing,
    }
}

/// `store.remove`: drop one exact version — the base and every variant — or,
/// for `<arch>/<name>@<version>#<variant>`, that one variant (§6.6).
///
/// Refused, unless forced, while a clone in a registered lab is backed by a
/// disk the removal would delete: the base or a variant overlay for a
/// version, the overlay alone for a variant.
pub async fn remove(reference: String, force: bool) -> Result<Value, CommandError> {
    let r = parse_store_ref(&reference).map_err(|e| format!("{e:#}"))?;
    let in_use = if force {
        Vec::new()
    } else {
        clones_by_backing().await
    };
    let removed = tokio::task::spawn_blocking(move || -> Result<StoredVersion> {
        let StoreRef {
            arch,
            name,
            version,
            variant,
        } = r;
        let version = version.ok_or_else(|| {
            anyhow!("specify the exact version to remove, e.g. {arch}/{name}@<version>")
        })?;
        let variant = variant.map(|v| v.as_str().to_string());
        store().remove(
            &arch,
            &name,
            &version,
            variant.as_deref(),
            force,
            &|doomed| backed_clones_reason(&in_use, doomed),
        )?;
        Ok(StoredVersion {
            arch,
            name,
            version,
            variant,
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    answer(removed)
}

/// Why `doomed` cannot be deleted: the clones (`lab/vm`) backed by one of
/// them. `None` when none is.
pub(super) fn backed_clones_reason(
    clones: &[(PathBuf, String)],
    doomed: &[PathBuf],
) -> Option<String> {
    let doomed: HashSet<PathBuf> = doomed
        .iter()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.clone()))
        .collect();
    // A machine reaches a doomed disk through several of its own disks, or
    // a base through its variant: name it once.
    let mut named = HashSet::new();
    let users: Vec<&str> = clones
        .iter()
        .filter(|(backing, _)| doomed.contains(backing))
        .map(|(_, who)| who.as_str())
        .filter(|who| named.insert(*who))
        .collect();
    match users.as_slice() {
        [] => None,
        [one] => Some(format!("the clone of machine {one} is backed by it")),
        many => Some(format!(
            "the clones of machines {} are backed by it",
            many.join(", ")
        )),
    }
}

/// `store.prune`: per `<arch>/<name>` family, keep the `keep` newest builds
/// and drop the rest.
///
/// The answer is always the plan — what would go, what is being kept back
/// because a clone still leans on it, and how many bytes are involved — with
/// `applied` saying whether it was carried out. A caller that wants a dry run
/// asks for the plan and stops there, which is one code path rather than two.
pub async fn prune(
    filter: Option<String>,
    keep: usize,
    apply: bool,
    force: bool,
) -> Result<Value, CommandError> {
    if keep == 0 {
        return Err(CommandError::failed(
            "keep must be >= 1 (use `store.remove` to remove specific versions)",
        ));
    }
    let store = store();
    let templates = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.list()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;

    let candidates = superseded(templates, filter.as_deref(), keep);

    // In-use protection: store disks currently backing a clone are skipped
    // unless forced.
    let in_use: HashSet<PathBuf> = if force || candidates.is_empty() {
        HashSet::new()
    } else {
        clones_by_backing()
            .await
            .into_iter()
            .map(|(backing, _)| backing)
            .collect()
    };

    let mut to_remove = Vec::new();
    let mut skipped = Vec::new();
    for t in candidates {
        // A version is held back while a clone leans on its base or on any
        // of its variants (§6.6).
        let held = disks(&store, &t).into_iter().any(|disk| {
            let canon = disk.canonicalize().unwrap_or(disk);
            in_use.contains(&canon)
        });
        if held {
            skipped.push(t);
        } else {
            to_remove.push(t);
        }
    }

    let plan = PrunePlan {
        freed: to_remove.iter().map(|t| disk_size(&store, t)).sum(),
        remove: to_remove.iter().map(StoredVersion::of).collect(),
        skipped: skipped.iter().map(StoredVersion::of).collect(),
        applied: apply,
    };
    if !apply {
        return answer(plan);
    }

    tokio::task::spawn_blocking(move || -> Result<()> {
        for t in &to_remove {
            store
                .remove(&t.arch, &t.name, &t.version, None, true, &|_| None)
                .with_context(|| format!("removing {}/{}@{}", t.arch, t.name, t.version))?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;
    answer(plan)
}

/// The builds a prune would drop: everything but the `keep` newest of each
/// matching `<arch>/<name>` family.
///
/// `templates` arrives in the store's own order — arch, then name, then
/// ascending version — so a family is a run of adjacent entries and "the
/// newest `keep`" is its tail.
fn superseded(
    templates: Vec<TemplateMeta>,
    filter: Option<&str>,
    keep: usize,
) -> Vec<TemplateMeta> {
    let mut families: Vec<((String, String), Vec<TemplateMeta>)> = Vec::new();
    for t in templates {
        if !family_matches(filter, &t.arch, &t.name) {
            continue;
        }
        match families.last_mut() {
            Some((k, v)) if k.0 == t.arch && k.1 == t.name => v.push(t),
            _ => families.push(((t.arch.clone(), t.name.clone()), vec![t])),
        }
    }
    families
        .into_iter()
        .flat_map(|(_, metas)| {
            let cut = metas.len().saturating_sub(keep);
            metas.into_iter().take(cut)
        })
        .collect()
}

/// Whether a `filter` selects `<arch>/<name>`: `None` matches all; `arch/name`
/// is exact; `arch/` matches any name in that arch; a bare `name` matches that
/// leaf name in any arch.
fn family_matches(filter: Option<&str>, arch: &str, name: &str) -> bool {
    let Some(f) = filter else { return true };
    match f.split_once('/') {
        Some((a, "")) => a == arch,
        Some((a, n)) => a == arch && n == name,
        None => f == name,
    }
}

/// Every image a clone leans on, as (canonical image, who): for each qcow2
/// in the working data of every registered lab and of the rest of its
/// worktree family (§21.5), every image anywhere in its backing chain —
/// frozen generations, a template variant and its base alike. The who is
/// `lab/vm`, a frozen file carrying its generation (see [`backing::owner`]).
/// Best-effort: unreadable labs and images end a chain early rather than
/// failing, so a scan hiccup never blocks a prune.
pub(super) async fn clones_by_backing() -> Vec<(PathBuf, String)> {
    let reg = super::registry::Registry::load();
    let registered: Vec<(String, PathBuf)> = reg
        .labs()
        .iter()
        .map(|l| (l.name.clone(), l.root.clone()))
        .collect();
    let roots = backing::scan_roots(
        &registered,
        crate::freeze::main_lab_root,
        crate::freeze::worktree_lab_roots,
    );
    let read = |image: PathBuf| async move {
        crate::template::qimg::image_info(&image)
            .await
            .ok()
            .and_then(|info| info.backing_file)
    };
    let mut headers = backing::Headers::new();
    let mut out = Vec::new();
    for scan in roots {
        for disk in backing::lab_disks(&crate::paths::lab_local_dir(&scan.root)) {
            let who = backing::owner(&scan.label, &disk);
            for image in backing::backing_chain(&disk.path, &mut headers, &read).await {
                out.push((image, who.clone()));
            }
        }
    }
    out
}

/// `store.export`: write one template to a portable archive.
pub async fn export(reference: String, out: PathBuf) -> Result<Value, CommandError> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let (arch, name, version) = parse_version_ref(&reference)?;
        store().export(&arch, &name, version.as_deref(), &out)?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(Value::Null)
}

/// `store.import`: read a template back out of an archive, with its
/// variants (§6.6).
pub async fn import(archive: PathBuf, overwrite: bool) -> Result<Value, CommandError> {
    let meta = tokio::task::spawn_blocking(move || store().import(&archive, overwrite))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    answer(InstalledVersion::of(&meta))
}

/// `store.pull`: download a published template into the store — the base,
/// its default variant, and the variant a `#<variant>` names or, with
/// `all_variants`, every one (§6.6).
pub async fn pull(
    target: String,
    arch: Option<String>,
    overwrite: bool,
    all_variants: bool,
) -> Result<Value, CommandError> {
    let store = store();
    let meta = crate::template::oci_bridge::pull(
        &target,
        arch.as_deref(),
        &store,
        overwrite,
        all_variants,
    )
    .await
    .context("pulling from registry")
    .map_err(|e| format!("{e:#}"))?;
    answer(InstalledVersion::of(&meta))
}

/// `store.push`: start uploading one store version, reporting progress the
/// way every other long template operation does — as `template.op.*` events
/// against `lab`, claimed in the same registry a lab-scoped build or push
/// claims. A console watching that lab sees a CLI push, and can stop it.
pub async fn push(
    sup: Arc<Supervisor>,
    reference: String,
    target: Option<String>,
    source: Option<String>,
    prerelease: bool,
    lab: String,
) -> Result<Value, CommandError> {
    let resolved = tokio::task::spawn_blocking(move || -> Result<_> {
        let (arch, name, version) = parse_version_ref(&reference)?;
        let resolved = store().resolve_version(&arch, &name, version.as_deref())?;
        Ok(resolved)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;

    // Target repo: the caller's, else the template's own `registry` field.
    let Some(repo) = target.or_else(|| resolved.meta.registry.clone()) else {
        return Err(CommandError::failed(
            "no push target — pass one (ghcr.io/owner/name) or set `registry` in the template",
        ));
    };
    let arch = resolved.meta.arch.clone();
    let name = resolved.meta.name.clone();
    let version = resolved.meta.version.clone();
    let target = crate::oci::with_version_tag(&repo, &version).map_err(|e| format!("{e:#}"))?;
    let moving_tag = if prerelease {
        "latest-prerelease"
    } else {
        "latest"
    };

    let guard = sup.template_ops.try_begin(&lab, &arch, &name, "push")?;
    let cancel = guard.cancel_token();
    let started = answer(PushStarted {
        pushing: StoredVersion::of(&resolved.meta),
        target: target.clone(),
        source: source.clone(),
        moving_tag: moving_tag.to_string(),
    })?;
    sup.emit(Event::new(
        "template.op.start",
        &*lab,
        json!({"template": name, "arch": arch, "kind": "push", "version": version}),
    ));

    let log = op_sink(sup.clone(), lab.clone(), arch.clone(), name.clone(), "push");
    tokio::spawn(async move {
        let _guard = guard;
        let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
        log(format!("pushing {arch}/{name}@{version} to {target}\n"));
        let upload = crate::template::oci_bridge::push(
            &resolved.dir,
            &target,
            host_cfg.oci_chunk_size,
            &arch,
            source.as_deref(),
            Some(moving_tag),
        );
        let done = json!({"template": name, "arch": arch, "kind": "push", "version": version});
        // Cancelling drops the upload future, which drops the connection
        // mid-body. There is no partial state to unwind: an OCI blob is not
        // committed until its final PUT, so the registry discards it.
        tokio::select! {
            _ = cancel.cancelled() => {
                sup.emit(Event::new("template.op.cancelled", &*lab, done));
            }
            result = upload => match result {
                Ok(report) => {
                    for line in report.lines() {
                        log(format!("{line}\n"));
                    }
                    sup.emit(Event::new("template.op.done", &*lab, done));
                }
                Err(e) => {
                    let mut error = format!("{e:#}");
                    if error.contains("401") || error.to_lowercase().contains("unauthorized") {
                        error.push_str(" — run `vmlab template login <registry>` first");
                    }
                    let mut data = done;
                    data["error"] = json!(error);
                    sup.emit(Event::new("template.op.error", &*lab, data));
                }
            },
        }
    });
    Ok(started)
}

/// `store.stop_push`: abort a running push, the counterpart of
/// `template.stop_build`.
pub fn stop_push(
    sup: Arc<Supervisor>,
    lab: String,
    arch: String,
    template: String,
) -> Result<Value, CommandError> {
    sup.template_ops.cancel(&lab, &arch, &template, "push")?;
    Ok(json!({"stopping": true}))
}

// ---------------------------------------------------------------------------
// Registries
// ---------------------------------------------------------------------------

/// `registry.search`: search one namespace, or every configured namespace of
/// the right kind, and merge the results.
pub async fn search(
    query: Option<String>,
    namespace: Option<String>,
    arch: Option<String>,
    containers: bool,
) -> Result<Value, CommandError> {
    use crate::template::registries;

    let namespaces = match namespace {
        Some(one) => vec![one],
        None => registries::list()
            .map_err(|e| format!("{e:#}"))?
            .into_iter()
            .filter(|entry| {
                if containers {
                    entry.use_for.containers()
                } else {
                    entry.use_for.vms()
                }
            })
            .map(|entry| entry.namespace)
            .collect(),
    };

    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    for namespace in &namespaces {
        match crate::template::catalog::search_catalog(
            query.clone(),
            namespace.clone(),
            arch.clone(),
            containers,
        )
        .await
        {
            Ok(found) => {
                rows.extend(found.rows);
                warnings.extend(found.warnings);
            }
            Err(error) => errors.push(format!("{namespace}: {error:#}")),
        }
    }
    // Every namespace failing is a failed search; some of them failing is a
    // warning beside the results that did come back.
    if rows.is_empty() && !errors.is_empty() {
        return Err(CommandError::failed(errors.join("\n")));
    }
    warnings.extend(errors);
    rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.repo.cmp(&b.repo)));
    rows.dedup_by(|a, b| a.reference == b.reference);
    answer(crate::template::catalog::CatalogSearch {
        rows,
        warnings,
        namespaces: namespaces.len(),
    })
}

/// `registry.login`: store credentials for a registry host.
///
/// The password never reaches an event, an operation log or a trace: this
/// handler is the only thing that sees it, and it hands it straight to the
/// credential store.
pub async fn login(
    registry: String,
    username: String,
    password: String,
) -> Result<Value, CommandError> {
    crate::template::oci_bridge::login(&registry, &username, &password)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(Value::Null)
}

/// `registry.namespaces`: the searchable namespaces this host is configured
/// with.
pub fn namespaces() -> Result<Value, CommandError> {
    answer(crate::template::registries::list().map_err(|e| format!("{e:#}"))?)
}

/// `registry.namespace_add`: add or update one searchable namespace.
pub fn namespace_add(
    namespace: String,
    use_for: crate::template::registries::RegistryUse,
) -> Result<Value, CommandError> {
    let entry =
        crate::template::registries::add(&namespace, use_for).map_err(|e| format!("{e:#}"))?;
    serde_json::to_value(entry).map_err(|e| CommandError::internal(e.to_string()))
}

/// `registry.namespace_remove`: drop one searchable namespace.
pub fn namespace_remove(namespace: String) -> Result<Value, CommandError> {
    crate::template::registries::remove(&namespace).map_err(|e| format!("{e:#}"))?;
    let normalised = crate::template::registries::normalise_namespace(&namespace)
        .map_err(|e| format!("{e:#}"))?;
    answer(normalised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_filter_matching() {
        // None matches everything.
        assert!(family_matches(None, "x86_64", "win11"));
        // Exact arch/name.
        assert!(family_matches(Some("x86_64/win11"), "x86_64", "win11"));
        assert!(!family_matches(Some("x86_64/win11"), "x86_64", "win10"));
        assert!(!family_matches(Some("x86_64/win11"), "aarch64", "win11"));
        // arch-only (trailing slash) matches any name in that arch.
        assert!(family_matches(Some("x86_64/"), "x86_64", "anything"));
        assert!(!family_matches(Some("x86_64/"), "aarch64", "anything"));
        // Bare name matches that leaf name in any arch.
        assert!(family_matches(
            Some("ubuntu-24.04"),
            "x86_64",
            "ubuntu-24.04"
        ));
        assert!(family_matches(
            Some("ubuntu-24.04"),
            "aarch64",
            "ubuntu-24.04"
        ));
        assert!(!family_matches(
            Some("ubuntu-24.04"),
            "x86_64",
            "ubuntu-26.04"
        ));
    }

    /// A store entry with only the three fields a prune looks at.
    fn meta(arch: &str, name: &str, version: &str) -> TemplateMeta {
        TemplateMeta {
            name: name.into(),
            arch: arch.into(),
            version: version.into(),
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
            accel: None,
            created: chrono::Utc::now(),
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

    fn refs(metas: &[TemplateMeta]) -> Vec<String> {
        metas
            .iter()
            .map(|t| format!("{}/{}@{}", t.arch, t.name, t.version))
            .collect()
    }

    /// The store lists ascending by version within a family, so the newest
    /// `keep` are its tail and everything before them is superseded.
    #[test]
    fn a_prune_keeps_the_newest_of_every_family() {
        let store = vec![
            meta("aarch64", "base", "1.0"),
            meta("x86_64", "base", "1.0"),
            meta("x86_64", "base", "1.1"),
            meta("x86_64", "base", "1.2"),
            meta("x86_64", "other", "3.0"),
        ];
        // Every family keeps its own newest — `aarch64/base` and
        // `x86_64/other` are each alone and survive.
        assert_eq!(
            refs(&superseded(store.clone(), None, 1)),
            ["x86_64/base@1.0", "x86_64/base@1.1"]
        );
        assert_eq!(
            refs(&superseded(store.clone(), None, 2)),
            ["x86_64/base@1.0"]
        );
        assert!(superseded(store.clone(), None, 3).is_empty());
        // A filter narrows which families are considered at all.
        assert!(superseded(store.clone(), Some("aarch64/"), 1).is_empty());
        assert_eq!(
            refs(&superseded(store, Some("x86_64/base"), 1)),
            ["x86_64/base@1.0", "x86_64/base@1.1"]
        );
    }

    /// The in-use check names the machines whose clone is backed by one of
    /// the doomed disks, and only those (§6.6: per variant).
    #[test]
    fn in_use_names_the_clones_backed_by_a_doomed_disk() {
        let dir = tempfile::tempdir().unwrap();
        let standard = dir.path().join("standard.qcow2");
        let extra = dir.path().join("extra.qcow2");
        std::fs::write(&standard, b"s").unwrap();
        std::fs::write(&extra, b"e").unwrap();
        let clones = vec![
            (extra.canonicalize().unwrap(), "lab/x".to_string()),
            (extra.canonicalize().unwrap(), "lab/y".to_string()),
        ];
        assert_eq!(
            backed_clones_reason(&clones, std::slice::from_ref(&standard)),
            None
        );
        let why = backed_clones_reason(&clones, &[standard, extra]).expect("in use");
        assert!(why.contains("lab/x, lab/y"), "{why}");
    }

    #[test]
    fn a_variant_reference_is_refused_where_a_version_is_meant() {
        let err = parse_version_ref("x86_64/win11@1#extra").unwrap_err();
        assert!(err.to_string().contains("drop the `#extra`"), "{err}");
        let r = parse_store_ref("x86_64/win11@1#extra").unwrap();
        assert_eq!(r.variant, Some(VariantRef::Named("extra".into())));
    }

    #[test]
    fn a_store_reference_needs_arch_and_name() {
        let (arch, name, version) = parse_version_ref("x86_64/win11@26100.1").unwrap();
        assert_eq!((arch.as_str(), name.as_str()), ("x86_64", "win11"));
        assert_eq!(version.as_deref(), Some("26100.1"));
        let (_, _, version) = parse_version_ref("x86_64/win11").unwrap();
        assert_eq!(version, None);
        // A registry reference is not a store reference.
        assert!(parse_version_ref("ghcr.io/owner/name:1.0").is_err());
    }
}
