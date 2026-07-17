//! `vmlab template ...` — store management, builds, OCI distribution (PRD
//! §6, §12). Runs in the CLI process (template store writes are serialised
//! by the store's own file lock).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use super::build::build_template;
use super::store::TemplateStore;
use crate::config::model::parse_template_ref;

#[derive(clap::Subcommand)]
pub enum TemplateCmd {
    /// Build templates defined in a lab/template file
    Build {
        /// File containing `template {}` blocks (default: ./vmlab.wcl)
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Build only the named template (default: all in the file)
        name: Option<String>,
        /// Pin an explicit version instead of auto-incrementing (requires a
        /// single target template)
        #[arg(long)]
        version: Option<String>,
    },
    /// List templates in the store
    List {
        /// Emit a JSON array instead of a table
        #[arg(long)]
        json: bool,
        /// Also check each template's registry to show whether it's uploaded
        /// (adds a REMOTE column: yes/no/local). Requires network access.
        #[arg(long)]
        remote: bool,
    },
    /// Search a registry for published templates (name substring + arch filter)
    Search {
        /// Case-insensitive substring to match the template name (default: all)
        query: Option<String>,
        /// Registry namespace to search (default: the vmlab registry)
        #[arg(long)]
        registry: Option<String>,
        /// Only show templates that have this arch
        #[arg(long)]
        arch: Option<String>,
        /// Search VM templates or container images
        #[arg(long, value_enum, default_value_t = CatalogKind::Vm)]
        kind: CatalogKind,
        /// Emit a JSON array instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Remove a template (`<arch>/<name>[@<version>]`)
    Rm {
        reference: String,
        /// Remove even if it backs existing clones
        #[arg(long)]
        force: bool,
    },
    /// Prune superseded builds, keeping the latest per template. Dry-run
    /// unless `--yes`; builds still backing a clone are skipped unless `--force`.
    Clean {
        /// Limit to a family: `<arch>/<name>`, `<arch>/` (all names in an arch),
        /// or `<name>` (that name in any arch). Default: every template.
        filter: Option<String>,
        /// Most-recent builds to keep per template (by version order)
        #[arg(long, default_value_t = 1)]
        keep: usize,
        /// Actually delete; without this, only prints what would be removed
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also remove builds that still back existing clones
        #[arg(long)]
        force: bool,
    },
    /// Export a template to a portable archive
    Export {
        reference: String,
        /// Output archive path (.tar.zst)
        out: PathBuf,
    },
    /// Import a template from an archive
    Import {
        archive: PathBuf,
        /// Overwrite an existing version
        #[arg(long)]
        overwrite: bool,
    },
    /// Push a template to an OCI registry
    Push {
        /// Local template `<arch>/<name>[@<version>]`
        reference: String,
        /// Registry repo, e.g. ghcr.io/owner/name. Defaults to the template's
        /// own `registry` field when omitted.
        target: Option<String>,
        /// Source repository URL to link the package to (e.g.
        /// https://github.com/owner/repo). Defaults to the git `origin`
        /// remote of the current directory when it resolves to a web URL.
        #[arg(long)]
        source: Option<String>,
        /// Publish as a pre-release: move `latest-prerelease` instead of
        /// `latest`.
        #[arg(long)]
        prerelease: bool,
    },
    /// Pull a template from an OCI registry
    Pull {
        /// Registry reference, e.g. ghcr.io/owner/name:version
        target: String,
        /// Architecture to pull (required for multi-arch indexes)
        #[arg(long)]
        arch: Option<String>,
        /// Overwrite an existing version in the store
        #[arg(long)]
        overwrite: bool,
    },
    /// Log in to an OCI registry
    Login {
        registry: String,
        #[arg(short, long)]
        username: String,
        #[arg(short, long)]
        password: String,
    },
    /// Manage OCI namespaces shared by CLI and web search
    Registry {
        #[command(subcommand)]
        command: RegistryCmd,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum CatalogKind {
    Vm,
    Container,
}

#[derive(clap::Subcommand)]
pub enum RegistryCmd {
    /// List configured registry namespaces
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add or update a searchable namespace
    Add {
        namespace: String,
        #[arg(long, value_enum, default_value_t = super::registries::RegistryUse::Both)]
        use_for: super::registries::RegistryUse,
    },
    /// Remove a searchable namespace
    Remove { namespace: String },
}

fn store() -> TemplateStore {
    TemplateStore::new(crate::paths::template_store_dir())
}

pub fn cmd_template(cmd: TemplateCmd) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        match cmd {
            TemplateCmd::Build {
                file,
                name,
                version,
            } => build(file, name, version).await,
            TemplateCmd::List { json, remote } => list(json, remote).await,
            TemplateCmd::Search {
                query,
                registry,
                arch,
                kind,
                json,
            } => search(query, registry, arch, kind, json).await,
            TemplateCmd::Rm { reference, force } => rm(&reference, force),
            TemplateCmd::Clean {
                filter,
                keep,
                yes,
                force,
            } => clean(filter, keep, yes, force).await,
            TemplateCmd::Export { reference, out } => export(&reference, &out),
            TemplateCmd::Import { archive, overwrite } => import(&archive, overwrite),
            TemplateCmd::Push {
                reference,
                target,
                source,
                prerelease,
            } => push(&reference, target, source, prerelease).await,
            TemplateCmd::Pull {
                target,
                arch,
                overwrite,
            } => pull(&target, arch.as_deref(), overwrite).await,
            TemplateCmd::Login {
                registry,
                username,
                password,
            } => login(&registry, &username, &password).await,
            TemplateCmd::Registry { command } => registry_command(command),
        }
    })
}

async fn build(file: Option<PathBuf>, only: Option<String>, version: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = match file {
        // Absolutize: a bare `-f vmlab.wcl` has an EMPTY parent(), which used
        // to silently break every root-relative resolution (media, scripts,
        // playbooks) once the build ran from its work dir.
        Some(p) if p.is_relative() => cwd.join(p),
        Some(p) => p,
        None => crate::paths::find_lab_root(&cwd)?.join(crate::paths::LAB_FILE),
    };
    let root = path.parent().unwrap_or(&cwd).to_path_buf();
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let tf = crate::config::load_template_source(&source, &path.display().to_string(), &root)
        .map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
    if tf.templates.is_empty() {
        bail!("no `template {{}}` blocks in {}", path.display());
    }
    let profiles = crate::profiles::ProfileSet::load_default()?;
    let store = store();
    let log: crate::scripting::OutputSink = Arc::new(|line: String| print!("{line}"));

    let targets: Vec<_> = tf
        .templates
        .iter()
        .filter(|t| only.as_deref().is_none_or(|n| n == t.name))
        .collect();
    if targets.is_empty() {
        bail!(
            "no template named \"{}\" in {}",
            only.unwrap_or_default(),
            path.display()
        );
    }
    if version.is_some() && targets.len() > 1 {
        bail!("--version needs a single target template; pass a template name too");
    }
    for def in targets {
        build_template(
            def,
            &root,
            &store,
            &profiles,
            log.clone(),
            version.as_deref(),
            crate::template::build::BuildControl::default(),
        )
        .await
        .with_context(|| format!("building {}/{}", def.arch, def.name))?;
    }
    Ok(())
}

async fn list(json: bool, remote: bool) -> Result<()> {
    let store = store();
    let templates = store.list()?;

    // With --remote, check each template's registry concurrently (preserving
    // order) for whether its exact version+arch is already uploaded.
    let statuses: Vec<String> = if remote {
        use futures::StreamExt as _;
        futures::stream::iter(
            templates
                .iter()
                .map(|t| registry_status(t.registry.clone(), t.version.clone(), t.arch.clone())),
        )
        .buffered(8)
        .collect()
        .await
    } else {
        Vec::new()
    };

    if json {
        let entries: Vec<_> = templates
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut v = meta_json(t);
                if remote {
                    v["remote"] = serde_json::json!(statuses[i]);
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if templates.is_empty() {
        println!("no templates in the store");
        return Ok(());
    }
    // Show the full registry path when known, else the bare store name.
    let name_of = |t: &crate::template::meta::TemplateMeta| {
        t.registry.clone().unwrap_or_else(|| t.name.clone())
    };
    let name_w = templates
        .iter()
        .map(|t| name_of(t).len())
        .max()
        .unwrap_or(0)
        .max(8);
    if remote {
        println!(
            "{:<8} {:<name_w$} {:<16} {:<8} {:<7} CREATED",
            "ARCH", "TEMPLATE", "VERSION", "SIZE", "REMOTE"
        );
    } else {
        println!(
            "{:<8} {:<name_w$} {:<16} {:<8} CREATED",
            "ARCH", "TEMPLATE", "VERSION", "SIZE"
        );
    }
    for (i, t) in templates.iter().enumerate() {
        let disk = store
            .root()
            .join(&t.arch)
            .join(&t.name)
            .join(&t.version)
            .join(crate::template::store::DISK_FILE);
        let size = human_size(std::fs::metadata(&disk).map(|m| m.len()).unwrap_or(0));
        let created = t.created.format("%Y-%m-%d");
        if remote {
            println!(
                "{:<8} {:<name_w$} {:<16} {:<8} {:<7} {}",
                t.arch,
                name_of(t),
                t.version,
                size,
                statuses[i],
                created
            );
        } else {
            println!(
                "{:<8} {:<name_w$} {:<16} {:<8} {}",
                t.arch,
                name_of(t),
                t.version,
                size,
                created
            );
        }
    }
    Ok(())
}

/// Whether `<registry>:<version>` already carries `arch` on the remote: `yes`,
/// `no` (missing — needs upload), `local` (no registry target), or `?` (the
/// registry ref is malformed).
async fn registry_status(registry: Option<String>, version: String, arch: String) -> String {
    let Some(reg) = registry else {
        return "local".to_string();
    };
    let Ok(r) = crate::oci::Registry::new(&reg) else {
        return "?".to_string();
    };
    match r.index_arches(&version).await {
        Ok(arches) if arches.contains(&arch) => "yes".to_string(),
        _ => "no".to_string(),
    }
}

/// Round a byte count to a short human string (`1.8G`, `456M`, `512B`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if bytes == 0 {
        return "-".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// Fixed-shape JSON for scripting: every key always present (null when
/// the template does not record it), sizes in bytes, created as RFC 3339.
fn meta_json(t: &crate::template::meta::TemplateMeta) -> serde_json::Value {
    serde_json::json!({
        "arch": t.arch,
        "name": t.name,
        "version": t.version,
        "ref": format!("{}/{}@{}", t.arch, t.name, t.version),
        "profile": t.profile,
        "cpus": t.cpus,
        "memory": t.memory,
        "disk": t.disk,
        "firmware": t.firmware,
        "tpm": t.tpm,
        "secure_boot": t.secure_boot,
        "display": t.display,
        "created": t.created.to_rfc3339(),
        "origin": t.origin,
        "registry": t.registry,
        "sha256": t.sha256,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogSearchRow {
    /// Leaf name (used for query matching).
    pub name: String,
    /// Full OCI repository path, e.g. ghcr.io/owner/group/name.
    pub repo: String,
    pub arches: Vec<String>,
    pub version: String,
    pub reference: String,
}

/// Search one OCI registry namespace and resolve each matching repository's
/// newest usable tag plus the architectures published by its manifest index.
/// Shared by the CLI and the web editor's VM/container chooser.
pub async fn search_catalog(
    query: Option<String>,
    registry: String,
    arch: Option<String>,
    containers: bool,
) -> Result<Vec<CatalogSearchRow>> {
    use futures::StreamExt as _;

    let namespace = registry;
    let repos = crate::oci::list_repositories_filtered(&namespace, query.as_deref())
        .await
        .with_context(|| format!("listing templates in {namespace}"))?;
    let ns_prefix = format!("{}/", namespace.trim_end_matches('/'));

    // Resolve each repo's latest version + arches concurrently.
    let mut rows: Vec<CatalogSearchRow> = futures::stream::iter(repos.into_iter().map(|repo| {
        let ns_prefix = ns_prefix.clone();
        async move { fetch_search_row(repo, &ns_prefix, containers).await }
    }))
    .buffer_unordered(8)
    .filter_map(|row| async move { row })
    .collect()
    .await;

    let q = query.map(|s| s.to_lowercase());
    let wanted_arch = arch;
    rows.retain(|r| {
        q.as_ref().is_none_or(|q| r.name.to_lowercase().contains(q))
            && wanted_arch
                .as_ref()
                .is_none_or(|a| r.arches.iter().any(|x| x == a))
    });
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

async fn search(
    query: Option<String>,
    registry: Option<String>,
    arch: Option<String>,
    kind: CatalogKind,
    json: bool,
) -> Result<()> {
    let containers = matches!(kind, CatalogKind::Container);
    let namespaces = match registry {
        Some(namespace) => vec![namespace],
        None => super::registries::list()?
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
    let mut errors = Vec::new();
    for namespace in &namespaces {
        match search_catalog(query.clone(), namespace.clone(), arch.clone(), containers).await {
            Ok(found) => rows.extend(found),
            Err(error) => errors.push(format!("{namespace}: {error:#}")),
        }
    }
    if rows.is_empty() && !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    for error in errors {
        eprintln!("warning: {error}");
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.repo.cmp(&b.repo)));
    rows.dedup_by(|a, b| a.reference == b.reference);

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "no results found in {} configured registries",
            namespaces.len()
        );
        return Ok(());
    }
    let name_w = rows.iter().map(|r| r.repo.len()).max().unwrap_or(0).max(8);
    println!("{:<name_w$} {:<24} VERSION", "TEMPLATE", "ARCH");
    for r in rows {
        println!(
            "{:<name_w$} {:<24} {}",
            r.repo,
            r.arches.join(","),
            r.version
        );
    }
    Ok(())
}

fn registry_command(command: RegistryCmd) -> Result<()> {
    match command {
        RegistryCmd::List { json } => {
            let entries = super::registries::list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{:<52} USE", "NAMESPACE");
                for entry in entries {
                    let use_for = match entry.use_for {
                        super::registries::RegistryUse::Vms => "vms",
                        super::registries::RegistryUse::Containers => "containers",
                        super::registries::RegistryUse::Both => "both",
                    };
                    println!("{:<52} {use_for}", entry.namespace);
                }
            }
        }
        RegistryCmd::Add { namespace, use_for } => {
            let entry = super::registries::add(&namespace, use_for)?;
            println!("added {}", entry.namespace);
        }
        RegistryCmd::Remove { namespace } => {
            super::registries::remove(&namespace)?;
            println!(
                "removed {}",
                super::registries::normalise_namespace(&namespace)?
            );
        }
    }
    Ok(())
}

/// Resolve one repository's display name, latest version and arches. Returns
/// `None` (skipping the repo) on any error or when it has no usable tag.
fn oci_to_vmlab_arch(arch: String) -> Option<String> {
    match arch.as_str() {
        "amd64" => Some("x86_64".into()),
        "arm64" => Some("aarch64".into()),
        "riscv64" => Some(arch),
        _ => None,
    }
}

async fn fetch_search_row(
    repo: String,
    ns_prefix: &str,
    containers: bool,
) -> Option<CatalogSearchRow> {
    let name = repo.strip_prefix(ns_prefix).unwrap_or(&repo).to_string();
    let registry = crate::oci::Registry::new(&repo).ok()?;
    let tags = match registry.list_tags().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("warning: {repo}: {e:#}");
            return None;
        }
    };
    // Prefer the highest concrete version tag; fall back to `latest`.
    let versions: Vec<String> = tags
        .iter()
        .filter(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .cloned()
        .collect();
    let newest_version = versions
        .into_iter()
        .max_by(|a, b| crate::template::store::compare_versions(a, b));
    let latest = tags.iter().find(|t| *t == "latest").cloned();
    let tag = if containers {
        latest.or(newest_version)
    } else {
        newest_version.or(latest)
    }
    .or_else(|| tags.iter().max().cloned())?;
    let mut arches = if containers {
        registry
            .index_platform_arches(&tag)
            .await
            .ok()?
            .into_iter()
            .filter_map(oci_to_vmlab_arch)
            .collect()
    } else {
        registry.index_arches(&tag).await.ok()?
    };
    arches.sort();
    arches.dedup();
    Some(CatalogSearchRow {
        name,
        arches,
        version: tag.clone(),
        reference: format!("{repo}:{tag}"),
        repo,
    })
}

fn rm(reference: &str, force: bool) -> Result<()> {
    let (arch, name, version) = parse_store_ref(reference)?;
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
    println!("removed {arch}/{name}@{version}");
    Ok(())
}

/// `vmlab template clean`: per `<arch>/<name>` family, keep the `keep` newest
/// builds (by version order) and remove the rest. Dry-run unless `yes`; a build
/// still backing a clone is skipped unless `force`.
async fn clean(filter: Option<String>, keep: usize, yes: bool, force: bool) -> Result<()> {
    if keep == 0 {
        bail!("--keep must be >= 1 (use `template rm` to remove specific versions)");
    }
    let store = store();
    let templates = store.list()?;

    // Group versions by (arch, name), preserving the list's arch/name/version
    // ordering (ascending version within each family).
    let mut families: Vec<((String, String), Vec<crate::template::meta::TemplateMeta>)> =
        Vec::new();
    for t in templates {
        if !family_matches(filter.as_deref(), &t.arch, &t.name) {
            continue;
        }
        match families.last_mut() {
            Some((k, v)) if k.0 == t.arch && k.1 == t.name => v.push(t),
            _ => families.push(((t.arch.clone(), t.name.clone()), vec![t])),
        }
    }

    // Decide removals: all but the `keep` highest versions per family.
    let mut removals: Vec<crate::template::meta::TemplateMeta> = Vec::new();
    for (_, metas) in &families {
        let cut = metas.len().saturating_sub(keep);
        removals.extend(metas.iter().take(cut).cloned());
    }
    if removals.is_empty() {
        println!("nothing to clean — every template is within --keep {keep}");
        return Ok(());
    }

    // In-use protection: store disks currently backing a clone are skipped
    // unless --force.
    let in_use = if force {
        std::collections::HashSet::new()
    } else {
        backing_disks_in_use().await
    };

    let mut to_remove = Vec::new();
    let mut skipped = Vec::new();
    for t in removals {
        let disk = store
            .root()
            .join(&t.arch)
            .join(&t.name)
            .join(&t.version)
            .join(crate::template::store::DISK_FILE);
        let canon = disk.canonicalize().unwrap_or(disk.clone());
        if in_use.contains(&canon) {
            skipped.push(t);
        } else {
            to_remove.push((t, disk));
        }
    }

    let mut freed = 0u64;
    for (t, disk) in &to_remove {
        freed += std::fs::metadata(disk).map(|m| m.len()).unwrap_or(0);
        let verb = if yes { "removing" } else { "would remove" };
        println!("{verb} {}/{}@{}", t.arch, t.name, t.version);
    }
    for t in &skipped {
        println!(
            "skipping {}/{}@{} — backs a clone (use --force)",
            t.arch, t.name, t.version
        );
    }

    if !yes {
        println!(
            "\n{} build(s), {} — dry run; re-run with --yes to remove",
            to_remove.len(),
            human_size(freed)
        );
        return Ok(());
    }

    let mut removed = 0usize;
    for (t, _) in &to_remove {
        store
            .remove(&t.arch, &t.name, &t.version, true, &|_| None)
            .with_context(|| format!("removing {}/{}@{}", t.arch, t.name, t.version))?;
        removed += 1;
    }
    println!("\nremoved {removed} build(s), freed {}", human_size(freed));
    Ok(())
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
/// skipped, so a scan hiccup never blocks a clean.
async fn backing_disks_in_use() -> std::collections::HashSet<PathBuf> {
    let mut in_use = std::collections::HashSet::new();
    let reg = crate::supervisor::registry::Registry::load();
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
            if let Ok(info) = super::qimg::image_info(&disk).await
                && let Some(backing) = info.backing_file
                && let Ok(canon) = backing.canonicalize()
            {
                in_use.insert(canon);
            }
        }
    }
    in_use
}

fn export(reference: &str, out: &std::path::Path) -> Result<()> {
    let (arch, name, version) = parse_store_ref(reference)?;
    store().export(&arch, &name, version.as_deref(), out)?;
    println!("exported to {}", out.display());
    Ok(())
}

fn import(archive: &std::path::Path, overwrite: bool) -> Result<()> {
    let meta = store().import(archive, overwrite)?;
    println!("imported {}/{}@{}", meta.arch, meta.name, meta.version);
    Ok(())
}

async fn push(
    reference: &str,
    target: Option<String>,
    source: Option<String>,
    prerelease: bool,
) -> Result<()> {
    let (arch, name, version) = parse_store_ref(reference)?;
    let resolved = store().resolve(&arch, &name, version.as_deref())?;
    // Target repo: explicit CLI arg, else the template's own `registry` field.
    let repo = match target.or_else(|| resolved.meta.registry.clone()) {
        Some(t) => t,
        None => bail!(
            "no push target — pass one (ghcr.io/owner/name) or set `registry` in the template"
        ),
    };
    let target = crate::oci::with_version_tag(&repo, &resolved.meta.version)?;
    let moving_tag = if prerelease {
        "latest-prerelease"
    } else {
        "latest"
    };
    let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
    let source = source.or_else(detect_git_source);
    super::oci_bridge::push(
        &resolved.dir,
        &target,
        host_cfg.oci_chunk_size,
        &arch,
        source.as_deref(),
        Some(moving_tag),
    )
    .await
    .context("pushing to registry")?;
    let src_note = source.map(|s| format!(", source {s}")).unwrap_or_default();
    println!(
        "pushed {arch}/{name}@{} to {target} (moved {moving_tag}{src_note})",
        resolved.meta.version
    );
    Ok(())
}

/// Best-effort source-repo URL for the package link: the git `origin` remote
/// of the current directory, normalised to a web URL. Returns `None` when
/// there is no git, no `origin`, or it isn't a URL we can normalise.
fn detect_git_source() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    normalize_git_url(&url)
}

/// Normalise a git remote URL to an `https://host/owner/repo` web URL. Handles
/// scp-like (`git@host:owner/repo.git`), `ssh://`, and `http(s)://` forms;
/// returns `None` for anything else (e.g. a local path).
fn normalize_git_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_suffix(".git").unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix("git@") {
        // scp-like: host:owner/repo
        return rest
            .split_once(':')
            .map(|(h, p)| format!("https://{h}/{p}"));
    }
    if let Some(rest) = s.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        return Some(format!("https://{rest}"));
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        return Some(s.to_string());
    }
    None
}

async fn pull(target: &str, arch: Option<&str>, overwrite: bool) -> Result<()> {
    let store = store();
    let meta = super::oci_bridge::pull(target, arch, &store, overwrite)
        .await
        .context("pulling from registry")?;
    println!(
        "pulled {}/{}@{} into the store",
        meta.arch, meta.name, meta.version
    );
    Ok(())
}

async fn login(registry: &str, username: &str, password: &str) -> Result<()> {
    super::oci_bridge::login(registry, username, password).await?;
    println!("logged in to {registry}");
    Ok(())
}

fn parse_store_ref(reference: &str) -> Result<(String, String, Option<String>)> {
    match parse_template_ref(reference).map_err(|e| anyhow!(e))? {
        crate::config::model::TemplateRef::Store {
            arch,
            name,
            version,
        } => Ok((arch, name, version)),
        _ => bail!("expected a local store reference `<arch>/<name>[@<version>]`"),
    }
}

#[cfg(test)]
mod tests {
    use super::{family_matches, normalize_git_url};

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

    #[test]
    fn normalizes_git_remote_forms() {
        assert_eq!(
            normalize_git_url("git@github.com:wiltaylor/vmlab-templates.git").as_deref(),
            Some("https://github.com/wiltaylor/vmlab-templates")
        );
        assert_eq!(
            normalize_git_url("https://github.com/wiltaylor/vmlab-templates.git\n").as_deref(),
            Some("https://github.com/wiltaylor/vmlab-templates")
        );
        assert_eq!(
            normalize_git_url("ssh://git@github.com/o/r.git").as_deref(),
            Some("https://github.com/o/r")
        );
        // a local path is not a web URL
        assert_eq!(normalize_git_url("/srv/git/repo.git"), None);
        assert_eq!(normalize_git_url(""), None);
    }
}
