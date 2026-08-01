//! `vmlab template ...` — store management, builds, OCI distribution (PRD
//! §6, §12), as a client of the supervisor.
//!
//! Nothing here opens the template store or dials a registry. PRD §3 gives
//! the supervisor "serialised writes to the template store (pulls, builds,
//! imports …)", so every verb below is a request over the supervisor socket
//! and this file is presentation: parse the flags, ask, render the answer.
//! That is what makes a build started from a terminal visible to the console
//! and stoppable from either, instead of the two surfaces each running their
//! own copy.
//!
//! Reading the caller's own surroundings stays here, because only the caller
//! has them: which lab file the shell is standing in, and the git `origin` a
//! pushed package should be linked to.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::cli::daemon::{abs_path, ensure_supervisor, remote};
use crate::proto::WireRequest;
use crate::proto::client::SupClient;
use crate::proto::{Event, SupRequest};
use crate::template::catalog::CatalogSearch;
use crate::template::registries::RegistryEntry;
use crate::template::store_view::{PrunePlan, PushStarted, StoreEntry, StoredVersion};

/// The two fields a build needs from `template.list`: which templates the
/// file declares, and how each is addressed.
#[derive(serde::Deserialize)]
struct DeclaredTemplate {
    arch: String,
    name: String,
}

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
    /// Stop a build or push running for a template — one this terminal
    /// started, or one the console did
    Stop {
        /// Template name, as the file declares it
        name: String,
        /// Architecture, when the name is declared for more than one
        #[arg(long, default_value = "x86_64")]
        arch: String,
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

/// Send one request and decode its answer into the view the supervisor
/// promised.
///
/// Decoding rather than indexing is what keeps the two halves of `store.*`
/// honest: a field renamed on one side stops the other compiling, where
/// `v["freed"].as_u64().unwrap_or(0)` would have printed a confident zero.
async fn ask<T: DeserializeOwned>(sup: &SupClient, req: SupRequest) -> Result<T> {
    let cmd = req.cmd();
    let answer = sup.send(req).await.map_err(remote)?;
    serde_json::from_value(answer).with_context(|| {
        format!("the supervisor answered `{cmd}` in a shape this build cannot read")
    })
}

pub fn cmd_template(cmd: TemplateCmd) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let sup = ensure_supervisor().await?;
        match cmd {
            TemplateCmd::Build {
                file,
                name,
                version,
            } => build(&sup, file, name, version).await,
            TemplateCmd::Stop { name, arch } => stop(&sup, name, arch).await,
            TemplateCmd::List { json, remote } => list(&sup, json, remote).await,
            TemplateCmd::Search {
                query,
                registry,
                arch,
                kind,
                json,
            } => search(&sup, query, registry, arch, kind, json).await,
            TemplateCmd::Rm { reference, force } => rm(&sup, reference, force).await,
            TemplateCmd::Clean {
                filter,
                keep,
                yes,
                force,
            } => clean(&sup, filter, keep, yes, force).await,
            TemplateCmd::Export { reference, out } => export(&sup, reference, out).await,
            TemplateCmd::Import { archive, overwrite } => import(&sup, archive, overwrite).await,
            TemplateCmd::Push {
                reference,
                target,
                source,
                prerelease,
            } => push(&sup, reference, target, source, prerelease).await,
            TemplateCmd::Pull {
                target,
                arch,
                overwrite,
            } => pull(&sup, target, arch, overwrite).await,
            TemplateCmd::Login {
                registry,
                username,
                password,
            } => login(&sup, registry, username, password).await,
            TemplateCmd::Registry { command } => registry_command(&sup, command).await,
        }
    })
}

// ---------------------------------------------------------------------------
// Long-running operations
// ---------------------------------------------------------------------------

/// How far a followed operation got.
#[derive(Debug)]
enum OpOutcome {
    Done,
    Failed(String),
    Cancelled,
}

impl OpOutcome {
    /// The verb's result: `what` names the operation for the error message
    /// ("building x86_64/base", "pushing to registry").
    fn into_result(self, what: &str) -> Result<()> {
        match self {
            OpOutcome::Done => Ok(()),
            OpOutcome::Cancelled => bail!("{what}: cancelled"),
            OpOutcome::Failed(e) => bail!("{what}: {e}"),
        }
    }
}

/// Follow one template operation to its end, printing its log as it arrives.
///
/// This is the same `template.op.*` stream the console renders, which is the
/// point: there is one progress mechanism, and a terminal reads it as text
/// where a browser draws it.
///
/// `stop` is what an interrupt sends. The operation is then followed to its
/// end anyway, so the supervisor has finished clearing up before this process
/// exits — and because tokio's handler has replaced SIGINT's default, a second
/// interrupt has to be honoured here or an upload that will not die leaves the
/// user with no way out.
async fn follow_op(
    sup: &SupClient,
    events: &mut mpsc::Receiver<Event>,
    lab: &str,
    arch: &str,
    template: &str,
    stop: SupRequest,
) -> Result<OpOutcome> {
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let mut stopping = false;
    loop {
        tokio::select! {
            _ = &mut interrupt => {
                if stopping {
                    bail!("interrupted a second time — {arch}/{template} may still be running");
                }
                stopping = true;
                eprintln!("interrupted — stopping {arch}/{template}");
                let _ = sup.send(stop.clone()).await;
                interrupt = Box::pin(tokio::signal::ctrl_c());
            }
            received = events.recv() => {
                let Some(ev) = received else {
                    bail!("lost the supervisor connection while it was running");
                };
                if ev.lab != lab
                    || ev.data["arch"] != *arch
                    || ev.data["template"] != *template
                {
                    continue;
                }
                match ev.event.as_str() {
                    "template.op.log" => {
                        if let Some(line) = ev.data["line"].as_str() {
                            println!("{line}");
                        }
                    }
                    "template.op.done" => return Ok(OpOutcome::Done),
                    "template.op.cancelled" => return Ok(OpOutcome::Cancelled),
                    "template.op.error" => {
                        return Ok(OpOutcome::Failed(
                            ev.data["error"].as_str().unwrap_or("failed").to_string(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The lab the shell is standing in, or the empty lab when it is not in one.
fn lab_here() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::paths::find_lab_root(&cwd).ok())
        .map(|root| lab_of(&root))
        .unwrap_or_default()
}

/// The lab a template file belongs to, so its build is filed where a console
/// watching that lab will see it. A file that declares no lab — a bare
/// template file — belongs to the store, spelled as the empty lab.
fn lab_of(root: &Path) -> String {
    crate::config::load_lab_root(root)
        .map(|f| f.lab.name)
        .unwrap_or_default()
}

async fn build(
    sup: &SupClient,
    file: Option<PathBuf>,
    only: Option<String>,
    version: Option<String>,
) -> Result<()> {
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
    let lab = lab_of(&root);

    let declared: Vec<DeclaredTemplate> = ask(
        sup,
        SupRequest::TemplateList {
            lab: lab.clone(),
            root: root.clone(),
            file: Some(path.clone()),
        },
    )
    .await?;
    if declared.is_empty() {
        bail!("no `template {{}}` blocks in {}", path.display());
    }
    let targets: Vec<DeclaredTemplate> = declared
        .into_iter()
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

    for DeclaredTemplate { arch, name } in targets {
        // Subscribe before asking, so no line of the build's output can be
        // emitted between the request and the first thing we listen to.
        let mut events = sup.subscribe().await.map_err(remote)?;
        let what = format!("building {arch}/{name}");
        ask::<Value>(
            sup,
            SupRequest::TemplateBuild {
                lab: lab.clone(),
                root: root.clone(),
                template: name.clone(),
                arch: Some(arch.clone()),
                version: version.clone(),
                file: Some(path.clone()),
            },
        )
        .await
        .context(what.clone())?;
        let stop = SupRequest::TemplateStopBuild {
            lab: lab.clone(),
            arch: arch.clone(),
            template: name.clone(),
        };
        follow_op(sup, &mut events, &lab, &arch, &name, stop)
            .await?
            .into_result(&what)?;
    }
    Ok(())
}

/// `vmlab template stop`: cancel an operation somebody else started — from
/// another terminal, or from the console.
///
/// Build and push are stopped by different commands because they are
/// different requests; a user with one running operation should not have to
/// know which. So this asks to stop the build, and takes the supervisor's
/// "that one is a push" as the answer to which command to send instead.
async fn stop(sup: &SupClient, name: String, arch: String) -> Result<()> {
    let lab = lab_here();
    let build = SupRequest::TemplateStopBuild {
        lab: lab.clone(),
        arch: arch.clone(),
        template: name.clone(),
    };
    let is_push = match sup.send(build).await {
        Ok(_) => false,
        Err(e) => {
            let e = crate::proto::CommandError::from(e);
            if e.code != crate::proto::ErrorCode::Conflict {
                return Err(anyhow::Error::new(e));
            }
            true
        }
    };
    if is_push {
        ask::<Value>(
            sup,
            SupRequest::StoreStopPush {
                lab,
                arch: arch.clone(),
                template: name.clone(),
            },
        )
        .await?;
    }
    println!("stopping {arch}/{name}");
    Ok(())
}

async fn push(
    sup: &SupClient,
    reference: String,
    target: Option<String>,
    source: Option<String>,
    prerelease: bool,
) -> Result<()> {
    // The git origin of the directory the user is standing in — a fact only
    // this process has, so it travels with the request.
    let source = source.or_else(detect_git_source);
    // File the push under the lab the shell is in, when it is in one, so a
    // console watching that lab sees it and can stop it.
    let lab = lab_here();

    let mut events = sup.subscribe().await.map_err(remote)?;
    let started: PushStarted = ask(
        sup,
        SupRequest::StorePush {
            reference,
            target,
            source,
            prerelease,
            lab: lab.clone(),
        },
    )
    .await
    .context("pushing to registry")?;

    let pushed = &started.pushing;
    let stop = SupRequest::StoreStopPush {
        lab: lab.clone(),
        arch: pushed.arch.clone(),
        template: pushed.name.clone(),
    };
    follow_op(sup, &mut events, &lab, &pushed.arch, &pushed.name, stop)
        .await?
        .into_result("pushing to registry")?;
    let src_note = started
        .source
        .map(|s| format!(", source {s}"))
        .unwrap_or_default();
    println!(
        "pushed {pushed} to {} (moved {}{src_note})",
        started.target, started.moving_tag,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

async fn list(sup: &SupClient, json: bool, check_remote: bool) -> Result<()> {
    let rows: Vec<StoreEntry> = ask(
        sup,
        SupRequest::StoreList {
            remote: check_remote,
        },
    )
    .await?;

    if json {
        // `--json` prints the metadata alone, with the registry check folded
        // in as one more field when it was asked for.
        let entries: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut meta = serde_json::to_value(&row.meta).unwrap_or(Value::Null);
                if let Some(status) = row.remote {
                    meta["remote"] = Value::String(status.as_str().into());
                }
                meta
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no templates in the store");
        return Ok(());
    }
    // Show the full registry path when known, else the bare store name.
    fn name_of(row: &StoreEntry) -> &str {
        row.meta.registry.as_deref().unwrap_or(&row.meta.name)
    }
    let name_w = rows
        .iter()
        .map(|row| name_of(row).len())
        .max()
        .unwrap_or(0)
        .max(8);
    if check_remote {
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
    for row in &rows {
        let meta = &row.meta;
        let size = human_size(row.size);
        // The wire carries RFC 3339; the table has always shown the date.
        let created: String = meta.created.chars().take(10).collect();
        match row.remote {
            Some(status) => println!(
                "{:<8} {:<name_w$} {:<16} {:<8} {:<7} {created}",
                meta.arch,
                name_of(row),
                meta.version,
                size,
                status.as_str(),
            ),
            None => println!(
                "{:<8} {:<name_w$} {:<16} {:<8} {created}",
                meta.arch,
                name_of(row),
                meta.version,
                size,
            ),
        }
    }
    Ok(())
}

async fn rm(sup: &SupClient, reference: String, force: bool) -> Result<()> {
    let removed: StoredVersion = ask(sup, SupRequest::StoreRemove { reference, force }).await?;
    println!("removed {removed}");
    Ok(())
}

/// `vmlab template clean`: per `<arch>/<name>` family, keep the `keep` newest
/// builds (by version order) and remove the rest. Dry-run unless `yes`; a build
/// still backing a clone is skipped unless `force`.
async fn clean(
    sup: &SupClient,
    filter: Option<String>,
    keep: usize,
    yes: bool,
    force: bool,
) -> Result<()> {
    if keep == 0 {
        bail!("--keep must be >= 1 (use `template rm` to remove specific versions)");
    }
    let plan: PrunePlan = ask(
        sup,
        SupRequest::StorePrune {
            filter,
            keep,
            apply: yes,
            force,
        },
    )
    .await?;
    if plan.remove.is_empty() && plan.skipped.is_empty() {
        println!("nothing to clean — every template is within --keep {keep}");
        return Ok(());
    }
    let verb = if plan.applied {
        "removing"
    } else {
        "would remove"
    };
    for t in &plan.remove {
        println!("{verb} {t}");
    }
    for t in &plan.skipped {
        println!("skipping {t} — backs a clone (use --force)");
    }

    let freed = human_size(plan.freed);
    let count = plan.remove.len();
    if plan.applied {
        println!("\nremoved {count} build(s), freed {freed}");
    } else {
        println!("\n{count} build(s), {freed} — dry run; re-run with --yes to remove");
    }
    Ok(())
}

async fn export(sup: &SupClient, reference: String, out: PathBuf) -> Result<()> {
    ask::<Value>(
        sup,
        SupRequest::StoreExport {
            reference,
            out: abs_path(&out)?,
        },
    )
    .await?;
    println!("exported to {}", out.display());
    Ok(())
}

async fn import(sup: &SupClient, archive: PathBuf, overwrite: bool) -> Result<()> {
    let archive = abs_path(&archive)?;
    let imported: StoredVersion = ask(sup, SupRequest::StoreImport { archive, overwrite }).await?;
    println!("imported {imported}");
    Ok(())
}

async fn pull(
    sup: &SupClient,
    target: String,
    arch: Option<String>,
    overwrite: bool,
) -> Result<()> {
    let pulled: StoredVersion = ask(
        sup,
        SupRequest::StorePull {
            target,
            arch,
            overwrite,
        },
    )
    .await?;
    println!("pulled {pulled} into the store");
    Ok(())
}

// ---------------------------------------------------------------------------
// Registries
// ---------------------------------------------------------------------------

async fn search(
    sup: &SupClient,
    query: Option<String>,
    registry: Option<String>,
    arch: Option<String>,
    kind: CatalogKind,
    json: bool,
) -> Result<()> {
    let found: CatalogSearch = ask(
        sup,
        SupRequest::RegistrySearch {
            query,
            namespace: registry,
            arch,
            containers: matches!(kind, CatalogKind::Container),
        },
    )
    .await?;
    for warning in &found.warnings {
        eprintln!("warning: {warning}");
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&found.rows)?);
        return Ok(());
    }
    if found.rows.is_empty() {
        println!(
            "no results found in {} configured registries",
            found.namespaces
        );
        return Ok(());
    }
    let name_w = found
        .rows
        .iter()
        .map(|r| r.repo.len())
        .max()
        .unwrap_or(0)
        .max(8);
    println!("{:<name_w$} {:<24} VERSION", "TEMPLATE", "ARCH");
    for r in &found.rows {
        println!(
            "{:<name_w$} {:<24} {}",
            r.repo,
            r.arches.join(","),
            r.version
        );
    }
    Ok(())
}

async fn login(
    sup: &SupClient,
    registry: String,
    username: String,
    password: String,
) -> Result<()> {
    ask::<Value>(
        sup,
        SupRequest::RegistryLogin {
            registry: registry.clone(),
            username,
            password,
        },
    )
    .await?;
    println!("logged in to {registry}");
    Ok(())
}

async fn registry_command(sup: &SupClient, command: RegistryCmd) -> Result<()> {
    match command {
        RegistryCmd::List { json } => {
            let entries: Vec<RegistryEntry> = ask(sup, SupRequest::RegistryNamespaces {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{:<52} USE", "NAMESPACE");
                for entry in entries {
                    println!("{:<52} {}", entry.namespace, entry.use_for.as_str());
                }
            }
        }
        RegistryCmd::Add { namespace, use_for } => {
            let entry: RegistryEntry =
                ask(sup, SupRequest::RegistryNamespaceAdd { namespace, use_for }).await?;
            println!("added {}", entry.namespace);
        }
        RegistryCmd::Remove { namespace } => {
            let removed: String =
                ask(sup, SupRequest::RegistryNamespaceRemove { namespace }).await?;
            println!("removed {removed}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering, and the caller's own surroundings
// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::proto::server::{Handler, Server, Streamer};
    use crate::proto::{CommandError, SupRequest};

    /// A supervisor that answers everything and says nothing, so a test can
    /// drive `follow_op` purely from the events it emits.
    struct Quiet;

    #[async_trait::async_trait]
    impl Handler<SupRequest> for Quiet {
        async fn handle(&self, _req: SupRequest, _s: &Streamer) -> Result<Value, CommandError> {
            Ok(Value::Bool(true))
        }
    }

    async fn supervisor() -> (tempfile::TempDir, Server<SupRequest>, SupClient) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vmlabd.sock");
        let server = Server::bind(&sock, Arc::new(Quiet)).await.unwrap();
        let client = SupClient::connect(&sock).await.unwrap();
        (dir, server, client)
    }

    fn op_event(event: &str, arch: &str, template: &str, extra: Value) -> Event {
        let mut data = json!({"arch": arch, "template": template, "kind": "build"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            data[k] = v;
        }
        Event::new(event, "demo", data)
    }

    fn stop_request() -> SupRequest {
        SupRequest::TemplateStopBuild {
            lab: "demo".into(),
            arch: "x86_64".into(),
            template: "base".into(),
        }
    }

    async fn follow(sup: &SupClient, events: &mut mpsc::Receiver<Event>) -> Result<OpOutcome> {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            follow_op(sup, events, "demo", "x86_64", "base", stop_request()),
        )
        .await
        .expect("the operation should have ended")
    }

    /// A build ends where its own `template.op.*` stream ends — and another
    /// template's failure, arriving on the same broadcast, is not its.
    #[tokio::test]
    async fn a_followed_build_ends_on_its_own_done_event() {
        let (_dir, server, sup) = supervisor().await;
        let mut events = sup.subscribe().await.unwrap();
        server.emit(op_event(
            "template.op.error",
            "x86_64",
            "other",
            json!({"error": "somebody else's problem"}),
        ));
        server.emit(op_event("template.op.log", "x86_64", "base", json!({})));
        server.emit(op_event("template.op.done", "x86_64", "base", json!({})));
        assert!(matches!(
            follow(&sup, &mut events).await.unwrap(),
            OpOutcome::Done
        ));
    }

    /// The supervisor's own wording reaches the terminal, so a failed build says
    /// what went wrong rather than that something did.
    #[tokio::test]
    async fn a_failed_build_carries_the_supervisors_reason() {
        let (_dir, server, sup) = supervisor().await;
        let mut events = sup.subscribe().await.unwrap();
        server.emit(op_event(
            "template.op.error",
            "x86_64",
            "base",
            json!({"error": "no such ISO"}),
        ));
        let OpOutcome::Failed(reason) = follow(&sup, &mut events).await.unwrap() else {
            panic!("the build failed");
        };
        assert_eq!(reason, "no such ISO");
    }

    /// A build cancelled from anywhere — this terminal, another one, or the
    /// console — is a cancellation here, not a success.
    #[tokio::test]
    async fn a_cancelled_build_is_not_a_finished_one() {
        let (_dir, server, sup) = supervisor().await;
        let mut events = sup.subscribe().await.unwrap();
        server.emit(op_event(
            "template.op.cancelled",
            "x86_64",
            "base",
            json!({}),
        ));
        assert!(matches!(
            follow(&sup, &mut events).await.unwrap(),
            OpOutcome::Cancelled
        ));
    }

    /// A supervisor that records what it was asked and answers the way a real
    /// one would when the running operation is a push.
    #[derive(Default)]
    struct Busy {
        asked: Mutex<Vec<&'static str>>,
    }

    #[async_trait::async_trait]
    impl Handler<SupRequest> for Busy {
        async fn handle(&self, req: SupRequest, _s: &Streamer) -> Result<Value, CommandError> {
            self.asked.lock().unwrap().push(req.cmd());
            match req {
                SupRequest::TemplateStopBuild { .. } => Err(CommandError::conflict(
                    "the operation running for `x86_64/base` is a push",
                )),
                _ => Ok(Value::Bool(true)),
            }
        }
    }

    /// `vmlab template stop` means "stop whatever is running for this
    /// template". Build and push are stopped by different commands, so it asks
    /// for the build and takes the conflict as the answer to which to send.
    #[tokio::test]
    async fn stopping_falls_through_to_the_push_command() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vmlabd.sock");
        let handler = Arc::new(Busy::default());
        let _server = Server::bind(&sock, handler.clone()).await.unwrap();
        let sup = SupClient::connect(&sock).await.unwrap();

        stop(&sup, "base".into(), "x86_64".into()).await.unwrap();
        assert_eq!(
            *handler.asked.lock().unwrap(),
            ["template.stop_build", "store.stop_push"]
        );
    }

    /// The reason the answers are typed: a supervisor of a different build
    /// answering a shape this one cannot read is an error, not a table of
    /// zeroes and blanks.
    #[tokio::test]
    async fn an_unreadable_answer_is_a_failure_not_an_empty_render() {
        struct Nonsense;
        #[async_trait::async_trait]
        impl Handler<SupRequest> for Nonsense {
            async fn handle(&self, _r: SupRequest, _s: &Streamer) -> Result<Value, CommandError> {
                Ok(json!({"remove": "all of them"}))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vmlabd.sock");
        let _server = Server::bind(&sock, Arc::new(Nonsense)).await.unwrap();
        let sup = SupClient::connect(&sock).await.unwrap();

        let err = ask::<PrunePlan>(
            &sup,
            SupRequest::StorePrune {
                filter: None,
                keep: 1,
                apply: false,
                force: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("store.prune"), "{err}");
    }

    /// The acceptance test of the whole change: nothing on the CLI's template
    /// surface opens the store or dials a registry. Every one of these used
    /// to appear in this file, and each is now the supervisor's.
    #[test]
    fn the_template_verbs_reach_the_store_only_through_the_supervisor() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/template/cli.rs");
        let source = std::fs::read_to_string(&path).unwrap();
        // Everything before the first `#[cfg(test)]` is the surface itself;
        // the needles below would otherwise match this very test.
        let surface = source.split("#[cfg(test)]").next().unwrap();
        for needle in [
            "TemplateStore",
            "oci_bridge",
            "crate::oci",
            "build_template",
            "registries::list",
            "registries::add",
            "registries::remove",
        ] {
            assert!(
                !surface.contains(needle),
                "`{needle}` is back in the template CLI — it belongs to the supervisor"
            );
        }
    }

    #[test]
    fn byte_counts_render_short() {
        assert_eq!(human_size(0), "-");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2 * 1024), "2.0K");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0G");
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
