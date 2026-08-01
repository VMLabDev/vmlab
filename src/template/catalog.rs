//! Searching an OCI namespace for publishable things (PRD §6.4).
//!
//! One repository at a time this resolves the newest usable tag and the
//! architectures its manifest index publishes, which is what both the
//! supervisor's `registry.search` and the web editor's VM/container chooser
//! render.

use anyhow::{Context, Result};

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

/// What one namespace search found: the rows, and the repositories that could
/// not be read. A repository nobody can list is a warning rather than a
/// failure — the rest of the namespace still answers — so the warnings travel
/// with the rows instead of going to whichever process happened to run the
/// search.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CatalogSearch {
    pub rows: Vec<CatalogSearchRow>,
    pub warnings: Vec<String>,
}

/// Search one OCI registry namespace and resolve each matching repository's
/// newest usable tag plus the architectures published by its manifest index.
pub async fn search_catalog(
    query: Option<String>,
    registry: String,
    arch: Option<String>,
    containers: bool,
) -> Result<CatalogSearch> {
    use futures::StreamExt as _;

    let namespace = registry;
    let repos = crate::oci::list_repositories_filtered(&namespace, query.as_deref())
        .await
        .with_context(|| format!("listing templates in {namespace}"))?;
    let ns_prefix = format!("{}/", namespace.trim_end_matches('/'));

    // Resolve each repo's latest version + arches concurrently.
    let found: Vec<(Option<CatalogSearchRow>, Option<String>)> =
        futures::stream::iter(repos.into_iter().map(|repo| {
            let ns_prefix = ns_prefix.clone();
            async move { fetch_search_row(repo, &ns_prefix, containers).await }
        }))
        .buffer_unordered(8)
        .collect()
        .await;
    let mut rows: Vec<CatalogSearchRow> = found.iter().filter_map(|(r, _)| r.clone()).collect();
    let mut warnings: Vec<String> = found.into_iter().filter_map(|(_, w)| w).collect();
    warnings.sort();

    let q = query.map(|s| s.to_lowercase());
    let wanted_arch = arch;
    rows.retain(|r| {
        q.as_ref().is_none_or(|q| r.name.to_lowercase().contains(q))
            && wanted_arch
                .as_ref()
                .is_none_or(|a| r.arches.iter().any(|x| x == a))
    });
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(CatalogSearch { rows, warnings })
}

/// The vmlab spelling of an OCI platform architecture, or `None` for one
/// vmlab has no name for.
fn oci_to_vmlab_arch(arch: String) -> Option<String> {
    match arch.as_str() {
        "amd64" => Some("x86_64".into()),
        "arm64" => Some("aarch64".into()),
        "riscv64" => Some(arch),
        _ => None,
    }
}

/// Resolve one repository's display name, latest version and arches, plus the
/// warning to report when it could not be read. A repository with no usable
/// tag yields neither: there is nothing to show and nothing went wrong.
async fn fetch_search_row(
    repo: String,
    ns_prefix: &str,
    containers: bool,
) -> (Option<CatalogSearchRow>, Option<String>) {
    let name = repo.strip_prefix(ns_prefix).unwrap_or(&repo).to_string();
    let Ok(registry) = crate::oci::Registry::new(&repo) else {
        return (None, None);
    };
    let tags = match registry.list_tags().await {
        Ok(t) => t,
        Err(e) => return (None, Some(format!("{repo}: {e:#}"))),
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
    let picked = if containers {
        latest.or(newest_version)
    } else {
        newest_version.or(latest)
    }
    .or_else(|| tags.iter().max().cloned());
    let Some(tag) = picked else {
        return (None, None);
    };
    let indexed = if containers {
        registry.index_platform_arches(&tag).await.map(|arches| {
            arches
                .into_iter()
                .filter_map(oci_to_vmlab_arch)
                .collect::<Vec<_>>()
        })
    } else {
        registry.index_arches(&tag).await
    };
    let Ok(mut arches) = indexed else {
        return (None, None);
    };
    arches.sort();
    arches.dedup();
    (
        Some(CatalogSearchRow {
            name,
            arches,
            version: tag.clone(),
            reference: format!("{repo}:{tag}"),
            repo,
        }),
        None,
    )
}
