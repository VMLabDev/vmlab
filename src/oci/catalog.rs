//! Enumerating the repositories published under a registry namespace
//! (PRD §6.4 — `vmlab template search`).
//!
//! GHCR does **not** implement the OCI `/v2/_catalog` endpoint, so for
//! `ghcr.io` we enumerate through the GitHub packages REST API; every other
//! registry uses the standard catalog endpoint ([`Registry::list_catalog`]).

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::Registry;
use super::auth::{self, Credential};

/// Full repository paths (`host/owner/.../name`) published under `namespace`
/// (`host/owner[/group...]`). For `ghcr.io` this uses the GitHub packages API;
/// other registries use OCI `/v2/_catalog`. Results are sorted.
pub async fn list_repositories(namespace: &str) -> Result<Vec<String>> {
    let (host, owner, subpath) = split_namespace(namespace)?;
    let mut repos = if host == "ghcr.io" || host.ends_with(".ghcr.io") {
        list_ghcr(&host, &owner, &subpath).await?
    } else {
        list_via_catalog(namespace, &host, &owner, &subpath).await?
    };
    repos.sort();
    repos.dedup();
    Ok(repos)
}

/// Split `host/owner[/group...]` into (host, owner, sub-path); sub-path may be
/// empty (just `host/owner`).
fn split_namespace(namespace: &str) -> Result<(String, String, String)> {
    let ns = namespace.trim().trim_end_matches('/');
    let mut parts = ns.splitn(3, '/');
    let host = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("namespace `{namespace}` has no host"))?;
    let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow!("namespace `{namespace}` needs an owner, e.g. ghcr.io/owner[/group]")
    })?;
    let subpath = parts.next().unwrap_or("").to_string();
    Ok((host.to_string(), owner.to_string(), subpath))
}

// ---- GHCR (GitHub packages API) --------------------------------------------

#[derive(Deserialize)]
struct GhPackage {
    name: String,
}

async fn list_ghcr(host: &str, owner: &str, subpath: &str) -> Result<Vec<String>> {
    let token = github_token(host)?;
    let client = reqwest::Client::builder()
        .user_agent("vmlab-oci/1")
        .build()
        .context("cannot build HTTP client")?;

    // Container packages are owned by an org or a user; try org first.
    let names = match fetch_packages(&client, &token, "orgs", owner).await? {
        Some(n) => n,
        None => fetch_packages(&client, &token, "users", owner)
            .await?
            .ok_or_else(|| anyhow!("no GitHub org or user named `{owner}`"))?,
    };

    let prefix = if subpath.is_empty() {
        String::new()
    } else {
        format!("{subpath}/")
    };
    Ok(names
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .map(|n| format!("{host}/{owner}/{n}"))
        .collect())
}

/// `Ok(Some(names))` on success, `Ok(None)` when the owner kind is a 404
/// (so the caller can try the other kind), `Err` otherwise.
async fn fetch_packages(
    client: &reqwest::Client,
    token: &str,
    kind: &str,
    owner: &str,
) -> Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    let mut url = Some(format!(
        "https://api.github.com/{kind}/{owner}/packages?package_type=container&per_page=100"
    ));
    while let Some(u) = url.take() {
        let resp = client
            .get(&u)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("GitHub packages request failed")?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => return Ok(None),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => bail!(
                "GitHub rejected the token listing {kind}/{owner} packages ({}). The token needs \
                 `read:packages`",
                resp.status()
            ),
            s if !s.is_success() => bail!("GitHub packages list returned {s}"),
            _ => {}
        }
        let next = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .and_then(next_link);
        let page: Vec<GhPackage> = resp.json().await.context("parsing GitHub packages list")?;
        names.extend(page.into_iter().map(|p| p.name));
        url = next;
    }
    Ok(Some(names))
}

/// The GitHub token to list packages: the registry login's password (a PAT)
/// if present, else `$GH_TOKEN` / `$GITHUB_TOKEN`.
fn github_token(host: &str) -> Result<String> {
    if let Ok(Credential::Basic { password, .. }) = auth::resolve(host)
        && !password.is_empty()
    {
        return Ok(password);
    }
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.is_empty()
        {
            return Ok(t);
        }
    }
    bail!(
        "searching {host} needs a GitHub token — run `vmlab template login {host}` or set \
         GH_TOKEN/GITHUB_TOKEN (needs `read:packages`)"
    )
}

// ---- Generic OCI `/v2/_catalog` --------------------------------------------

async fn list_via_catalog(
    namespace: &str,
    host: &str,
    owner: &str,
    subpath: &str,
) -> Result<Vec<String>> {
    let registry = Registry::new(namespace)?;
    let repos = registry.list_catalog().await?;
    let ns_path = if subpath.is_empty() {
        owner.to_string()
    } else {
        format!("{owner}/{subpath}")
    };
    let prefix = format!("{ns_path}/");
    Ok(repos
        .into_iter()
        .filter(|r| r.starts_with(&prefix))
        .map(|r| format!("{host}/{r}"))
        .collect())
}

/// Extract the `rel="next"` URL from an RFC 5988 `Link` header.
fn next_link(header: &str) -> Option<String> {
    for part in header.split(',') {
        if part.contains("rel=\"next\"") || part.contains("rel=next") {
            let start = part.find('<')?;
            let end = part[start..].find('>')? + start;
            return Some(part[start + 1..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_namespace_forms() {
        assert_eq!(
            split_namespace("ghcr.io/vmlabdev/vmlab-templates").unwrap(),
            (
                "ghcr.io".into(),
                "vmlabdev".into(),
                "vmlab-templates".into()
            )
        );
        assert_eq!(
            split_namespace("ghcr.io/vmlabdev").unwrap(),
            ("ghcr.io".into(), "vmlabdev".into(), String::new())
        );
        assert_eq!(
            split_namespace("harbor.example.com/team/project/").unwrap(),
            ("harbor.example.com".into(), "team".into(), "project".into())
        );
        assert!(split_namespace("ghcr.io").is_err());
    }

    #[test]
    fn parses_next_link() {
        let h = "<https://api.github.com/x?page=2>; rel=\"next\", <...>; rel=\"last\"";
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(next_link("<...>; rel=\"prev\"").as_deref(), None);
    }
}
