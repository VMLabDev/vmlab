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
use super::templates::op_sink;
use crate::config::model::{TemplateRef, parse_template_ref};
use crate::proto::{CommandError, Event};
use crate::template::TemplateStore;
use crate::template::meta::TemplateMeta;
use crate::template::store_view::{
    PrunePlan, PushStarted, RemoteStatus, StoreEntry, StoredVersion, TemplateSummary,
};

/// Serialise a typed answer, which cannot realistically fail — every view in
/// `store_view` is plain data.
fn answer<T: serde::Serialize>(value: T) -> Result<Value, CommandError> {
    serde_json::to_value(value).map_err(|e| CommandError::internal(e.to_string()))
}

fn store() -> TemplateStore {
    TemplateStore::new(crate::paths::template_store_dir())
}

/// Split `<arch>/<name>[@<version>]`, refusing anything that is not a local
/// store reference.
fn parse_store_ref(reference: &str) -> Result<(String, String, Option<String>)> {
    match parse_template_ref(reference).map_err(|e| anyhow!(e))? {
        TemplateRef::Store {
            arch,
            name,
            version,
        } => Ok((arch, name, version)),
        _ => bail!("expected a local store reference `<arch>/<name>[@<version>]`"),
    }
}

fn disk_path(store: &TemplateStore, t: &TemplateMeta) -> PathBuf {
    store
        .root()
        .join(&t.arch)
        .join(&t.name)
        .join(&t.version)
        .join(crate::template::store::DISK_FILE)
}

fn disk_size(store: &TemplateStore, t: &TemplateMeta) -> u64 {
    std::fs::metadata(disk_path(store, t))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// `store.list`: every template in the store with its disk size, and — when
/// asked — whether that exact version and architecture is already published.
pub async fn list(remote: bool) -> Result<Value, CommandError> {
    let (templates, sizes) = tokio::task::spawn_blocking(move || -> Result<_> {
        let store = store();
        let templates = store.list()?;
        let sizes: Vec<u64> = templates.iter().map(|t| disk_size(&store, t)).collect();
        Ok((templates, sizes))
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

/// `store.remove`: drop one exact version.
pub async fn remove(reference: String, force: bool) -> Result<Value, CommandError> {
    let removed = tokio::task::spawn_blocking(move || -> Result<StoredVersion> {
        let (arch, name, version) = parse_store_ref(&reference)?;
        let version = version.ok_or_else(|| {
            anyhow!("specify the exact version to remove, e.g. {arch}/{name}@<version>")
        })?;
        store().remove(&arch, &name, &version, force, &|_| {
            if force {
                None
            } else {
                Some(
                    "deleting a template may break existing linked clones; re-run with --force"
                        .to_string(),
                )
            }
        })?;
        Ok(StoredVersion {
            arch,
            name,
            version,
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    answer(removed)
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
    let in_use = if force || candidates.is_empty() {
        HashSet::new()
    } else {
        backing_disks_in_use().await
    };

    let mut to_remove = Vec::new();
    let mut skipped = Vec::new();
    for t in candidates {
        let disk = disk_path(&store, &t);
        let canon = disk.canonicalize().unwrap_or(disk);
        if in_use.contains(&canon) {
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
                .remove(&t.arch, &t.name, &t.version, true, &|_| None)
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

/// Canonical store disk paths (`<version>/disk.qcow2`) currently backing a
/// linked clone in any registered lab. Best-effort: unreadable labs/clones are
/// skipped, so a scan hiccup never blocks a prune.
async fn backing_disks_in_use() -> HashSet<PathBuf> {
    let mut in_use = HashSet::new();
    let reg = super::registry::Registry::load();
    for lab in reg.labs() {
        let vms = crate::paths::lab_local_dir(&lab.root).join("vms");
        let Ok(entries) = std::fs::read_dir(&vms) else {
            continue;
        };
        for e in entries.flatten() {
            let disk = e.path().join("disk0.qcow2");
            if !disk.is_file() {
                continue;
            }
            if let Ok(info) = crate::template::qimg::image_info(&disk).await
                && let Some(backing) = info.backing_file
                && let Ok(canon) = backing.canonicalize()
            {
                in_use.insert(canon);
            }
        }
    }
    in_use
}

/// `store.export`: write one template to a portable archive.
pub async fn export(reference: String, out: PathBuf) -> Result<Value, CommandError> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let (arch, name, version) = parse_store_ref(&reference)?;
        store().export(&arch, &name, version.as_deref(), &out)?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(Value::Null)
}

/// `store.import`: read a template back out of an archive.
pub async fn import(archive: PathBuf, overwrite: bool) -> Result<Value, CommandError> {
    let meta = tokio::task::spawn_blocking(move || store().import(&archive, overwrite))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    answer(StoredVersion::of(&meta))
}

/// `store.pull`: download a published template into the store.
pub async fn pull(
    target: String,
    arch: Option<String>,
    overwrite: bool,
) -> Result<Value, CommandError> {
    let store = store();
    let meta = crate::template::oci_bridge::pull(&target, arch.as_deref(), &store, overwrite)
        .await
        .context("pulling from registry")
        .map_err(|e| format!("{e:#}"))?;
    answer(StoredVersion::of(&meta))
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
        let (arch, name, version) = parse_store_ref(&reference)?;
        let resolved = store().resolve(&arch, &name, version.as_deref())?;
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
                Ok(()) => sup.emit(Event::new("template.op.done", &*lab, done)),
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
            display: None,
            created: chrono::Utc::now(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
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

    #[test]
    fn a_store_reference_needs_arch_and_name() {
        let (arch, name, version) = parse_store_ref("x86_64/win11@26100.1").unwrap();
        assert_eq!((arch.as_str(), name.as_str()), ("x86_64", "win11"));
        assert_eq!(version.as_deref(), Some("26100.1"));
        let (_, _, version) = parse_store_ref("x86_64/win11").unwrap();
        assert_eq!(version, None);
        // A registry reference is not a store reference.
        assert!(parse_store_ref("ghcr.io/owner/name:1.0").is_err());
    }
}
