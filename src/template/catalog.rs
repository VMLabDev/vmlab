//! Searching an OCI namespace for publishable things (PRD §6.4).
//!
//! One repository at a time this resolves the newest version — the one a
//! template repository's `latest` tag names — and the architectures its
//! manifest index publishes, which is what both the
//! supervisor's `registry.search` and the web editor's VM/container chooser
//! render.

use anyhow::{Context, Result};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct CatalogSearch {
    pub rows: Vec<CatalogSearchRow>,
    pub warnings: Vec<String>,
    /// How many namespaces were searched to produce this. One when a caller
    /// named the namespace; however many are configured when it did not.
    #[serde(default)]
    pub namespaces: usize,
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
    Ok(CatalogSearch {
        rows,
        warnings,
        namespaces: 1,
    })
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

/// Resolve one repository's display name, newest version and arches, plus the
/// warning to report when it could not be read. A repository with no usable
/// tag yields neither: there is nothing to show and nothing went wrong.
///
/// For a template the newest version is the one the moving `latest` tag names,
/// with the arches of the index it points at — never the highest tag, which a
/// tag left over from a retired versioning scheme can outrank (PRD §6.4).
async fn fetch_search_row(
    repo: String,
    ns_prefix: &str,
    containers: bool,
) -> (Option<CatalogSearchRow>, Option<String>) {
    let name = repo.strip_prefix(ns_prefix).unwrap_or(&repo).to_string();
    let Ok(registry) = crate::oci::Registry::new(&repo) else {
        return (None, None);
    };
    search_row_of(&registry, name, repo, containers).await
}

/// Whether a string a registry sent may be shown: printable ASCII with no
/// space, which every repository path, tag, version and architecture vmlab
/// can use is. Anything else — an escape sequence, a line break, a
/// bidirectional override — could redraw the table `template search` prints.
fn showable(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic())
}

/// `text` with every character a terminal would act on, or that would hide
/// in a line, written as its escape.
fn escaped(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c.to_string()
            } else {
                c.escape_default().to_string()
            }
        })
        .collect()
}

/// [`fetch_search_row`] over a client that is already built, with what the
/// registry sent checked before it can be printed: a row that holds a
/// string that is not [`showable`] is left out with a warning, and a warning
/// is [`escaped`].
async fn search_row_of(
    registry: &crate::oci::Registry,
    name: String,
    repo: String,
    containers: bool,
) -> (Option<CatalogSearchRow>, Option<String>) {
    let (row, warning) = resolve_search_row(registry, name, repo.clone(), containers).await;
    let unshowable = row.as_ref().is_some_and(|row| {
        ![&row.name, &row.repo, &row.version, &row.reference]
            .into_iter()
            .chain(&row.arches)
            .all(|text| showable(text))
    });
    if unshowable {
        let said =
            format!("{repo}: left out, the registry sent a name with unprintable characters");
        return (None, Some(escaped(&said)));
    }
    (row, warning.as_deref().map(escaped))
}

async fn resolve_search_row(
    registry: &crate::oci::Registry,
    name: String,
    repo: String,
    containers: bool,
) -> (Option<CatalogSearchRow>, Option<String>) {
    let mut tags = match registry.list_tags().await {
        Ok(t) => t,
        Err(e) => return (None, Some(format!("{repo}: {e:#}"))),
    };
    // A tag that cannot be shown is not a version of anything vmlab
    // published. The tags beside it are still listed.
    tags.retain(|tag| showable(tag));
    if !containers
        && tags.iter().any(|t| t == "latest")
        && let Ok((version, mut arches)) = registry.tag_version("latest").await
    {
        arches.sort();
        arches.dedup();
        return (Some(row(name, repo, version, arches)), None);
    }
    // No `latest` to name the newest version: the highest concrete version
    // tag, or `latest` for a container image.
    let versions: Vec<String> = tags
        .iter()
        .filter(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        // A variant's tag (`<version>--<variant>`, §6.6) is not a version.
        .filter(|t| !crate::oci::client::is_variant_tag(t))
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
    (Some(row(name, repo, tag, arches)), None)
}

fn row(name: String, repo: String, version: String, arches: Vec<String>) -> CatalogSearchRow {
    CatalogSearchRow {
        name,
        arches,
        reference: format!("{repo}:{version}"),
        version,
        repo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::client::{Fetched, Transport};
    use crate::oci::reference::Reference;
    use std::path::Path;

    /// A registry with one repository: its tag list is `tags`, and every tag
    /// resolves to an index of `arches`.
    struct FakeRegistry {
        tags: Vec<String>,
        arches: Vec<String>,
    }

    #[async_trait::async_trait]
    impl Transport for FakeRegistry {
        async fn blob_exists(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn get_blob(&self, _: &str, digest: &str) -> Result<Vec<u8>> {
            anyhow::bail!("no blob {digest}")
        }
        async fn put_blob(&self, _: &str, _: &str, _: Vec<u8>) -> Result<()> {
            anyhow::bail!("read-only")
        }
        async fn put_blob_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            anyhow::bail!("read-only")
        }
        async fn get_manifest(&self, _: &str, _: &str) -> Result<Option<Fetched>> {
            let manifests: Vec<serde_json::Value> = self
                .arches
                .iter()
                .map(|arch| {
                    serde_json::json!({
                        "mediaType": crate::oci::media_types::OCI_MANIFEST,
                        "digest": format!("sha256:{}", "0".repeat(64)),
                        "size": 2,
                        "platform": {"architecture": arch, "os": "vmlab"},
                    })
                })
                .collect();
            let index = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": crate::oci::media_types::OCI_INDEX,
                "manifests": manifests,
            });
            Ok(Some(Fetched {
                media_type: crate::oci::media_types::OCI_INDEX.to_string(),
                body: serde_json::to_vec(&index)?,
            }))
        }
        async fn put_manifest(&self, _: &str, _: &str, _: &str, _: Vec<u8>) -> Result<String> {
            anyhow::bail!("read-only")
        }
        async fn list_tags(&self, _: &str) -> Result<Vec<String>> {
            Ok(self.tags.clone())
        }
        async fn list_catalog(&self) -> Result<Vec<String>> {
            Ok(vec!["acme/ubuntu".into()])
        }
    }

    async fn search_row(
        tags: &[&str],
        arches: &[&str],
    ) -> (Option<CatalogSearchRow>, Option<String>) {
        let repo = "registry.example/acme/ubuntu";
        let registry = crate::oci::Registry::with_transport(
            Reference::parse(repo).unwrap(),
            Box::new(FakeRegistry {
                tags: tags.iter().map(|t| t.to_string()).collect(),
                arches: arches.iter().map(|a| a.to_string()).collect(),
            }),
        );
        search_row_of(&registry, "ubuntu".into(), repo.into(), false).await
    }

    fn unprintable(text: &str) -> bool {
        text.chars().any(char::is_control)
    }

    /// Review F19: a tag is any string JSON can carry. One with an escape
    /// sequence must not become the version of a row.
    #[tokio::test]
    async fn a_tag_with_a_control_character_is_not_listed() {
        let (row, warning) = search_row(&["1.0\u{1b}[2J"], &["x86_64"]).await;
        assert!(row.is_none(), "the tag reached a row: {row:?}");
        assert!(
            !warning.as_deref().is_some_and(unprintable),
            "the warning carries the character: {warning:?}"
        );

        // The tags beside it are still listed.
        let (row, _) = search_row(&["1.0\u{1b}[2J", "0.9"], &["x86_64"]).await;
        let row = row.expect("the printable tag is a row");
        assert_eq!(row.version, "0.9");
        assert_eq!(row.reference, "registry.example/acme/ubuntu:0.9");
    }

    /// The architectures of a row are the registry's strings too.
    #[tokio::test]
    async fn an_arch_with_a_control_character_is_not_listed() {
        let (row, warning) = search_row(&["1.0"], &["x86_64\u{1b}[1A"]).await;
        assert!(row.is_none(), "the arch reached a row: {row:?}");
        let warning = warning.expect("a row that is left out is said");
        assert!(!unprintable(&warning), "{warning:?}");
        assert!(warning.contains("registry.example/acme/ubuntu"), "{warning}");
    }
}
