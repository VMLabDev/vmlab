//! Enumerating the repositories published under a registry namespace
//! (PRD §6.4 — `vmlab template search`).
//!
//! GHCR does **not** implement the OCI `/v2/_catalog` endpoint, so for
//! `ghcr.io` we enumerate through GitHub Packages (the authenticated REST API
//! or the public packages page); every other registry uses the standard
//! catalog endpoint ([`Registry::list_catalog`]).

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Deserialize;

use super::Registry;
use super::auth::{self, Credential};
use super::client::{MAX_JSON_BODY, MAX_LIST_BYTES, MAX_LIST_PAGES, bounded_body};

// Provider reads share registry limits: 8 MiB/page, 32 MiB/list, 100 pages.
// Count raw items, before filtering/deduplication, with a 10,000-item ceiling.
const MAX_LIST_ITEMS: usize = 10_000;

fn provider_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("vmlab-oci/1")
        // No provider needs redirects. In particular, never replay a PAT on one.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

fn package_request(client: &reqwest::Client, url: &str, token: &str) -> Result<reqwest::Request> {
    let url = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "provider URL must not contain credentials"
    );
    let mut request = client.get(url.clone());
    // Intentional GHCR PAT reuse is restricted to the GitHub API HTTPS origin.
    if url.scheme() == "https"
        && url.host_str() == Some("api.github.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
    {
        request = request.bearer_auth(token);
    }
    Ok(request
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .build()?)
}

#[derive(Default)]
struct ProviderBudget {
    pages: usize,
    bytes: usize,
    items: usize,
}

impl ProviderBudget {
    fn page(&mut self) -> Result<()> {
        anyhow::ensure!(self.pages < MAX_LIST_PAGES, "provider page limit exceeded");
        self.pages += 1;
        Ok(())
    }

    async fn body(&mut self, response: reqwest::Response) -> Result<Vec<u8>> {
        let body = bounded_body(response, MAX_JSON_BODY.min(MAX_LIST_BYTES - self.bytes)).await?;
        self.bytes += body.len();
        Ok(body)
    }

    fn items(&mut self, count: usize) -> Result<()> {
        anyhow::ensure!(
            count <= MAX_LIST_ITEMS - self.items,
            "provider item limit exceeded"
        );
        self.items += count;
        Ok(())
    }
}

/// Full repository paths (`host/owner/.../name`) published under `namespace`,
/// optionally filtered by leaf name. GHCR uses GitHub Packages, Docker Hub
/// uses its supported Hub API, and other registries use OCI `/v2/_catalog`.
pub async fn list_repositories_filtered(
    namespace: &str,
    query: Option<&str>,
) -> Result<Vec<String>> {
    let (host, owner, subpath) = split_namespace(namespace)?;
    let mut repos = if matches!(
        host.as_str(),
        "registry-1.docker.io" | "docker.io" | "index.docker.io"
    ) {
        list_docker_hub(&owner, &subpath, query).await?
    } else if host == "ghcr.io" || host.ends_with(".ghcr.io") {
        list_ghcr(&host, &owner, &subpath).await?
    } else {
        list_via_catalog(namespace, &host, &owner, &subpath).await?
    };
    if let Some(q) = query.map(str::to_lowercase).filter(|q| !q.is_empty()) {
        repos.retain(|repo| {
            repo.rsplit('/')
                .next()
                .is_some_and(|name| name.to_lowercase().contains(&q))
        });
    }
    repos.sort();
    repos.dedup();
    Ok(repos)
}

#[derive(Deserialize)]
struct DockerHubRepositories {
    #[serde(default)]
    results: Vec<DockerHubRepository>,
}

#[derive(Deserialize)]
struct DockerHubRepository {
    name: String,
}

/// Docker Hub intentionally omits `_catalog`; its supported Hub API lists a
/// namespace and accepts a partial-name filter.
async fn list_docker_hub(owner: &str, subpath: &str, query: Option<&str>) -> Result<Vec<String>> {
    if !subpath.is_empty() {
        bail!("Docker Hub namespaces cannot contain a nested group (`{owner}/{subpath}`)");
    }
    let client = provider_client()?;
    let mut request = client
        .get(format!(
            "https://hub.docker.com/v2/namespaces/{owner}/repositories"
        ))
        .query(&[("page_size", "100"), ("ordering", "name")]);
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        request = request.query(&[("name", q)]);
    }
    let response = request.send().await.context("searching Docker Hub")?;
    if !response.status().is_success() {
        bail!("Docker Hub namespace search returned {}", response.status());
    }
    // Hub search intentionally reads only its first page (at most 100 results).
    let mut budget = ProviderBudget::default();
    budget.page()?;
    let page: DockerHubRepositories = serde_json::from_slice(&budget.body(response).await?)
        .context("parsing Docker Hub search")?;
    budget.items(page.results.len())?;
    Ok(page
        .results
        .into_iter()
        .map(|repo| format!("registry-1.docker.io/{owner}/{}", repo.name))
        .collect())
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
    let client = provider_client()?;

    // GitHub's package-list REST endpoint requires authentication even when
    // every result is public. When no credential is configured, enumerate the
    // public packages page instead; known public images can already be pulled
    // from GHCR anonymously, so search should have the same behavior.
    let names = if let Some(token) = github_token(host) {
        fetch_for_owner(owner, |kind| fetch_packages(&client, &token, kind, owner)).await?
    } else {
        fetch_for_owner(owner, |kind| fetch_public_packages(&client, kind, owner)).await?
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

async fn fetch_for_owner<F, Fut>(owner: &str, fetch: F) -> Result<Vec<String>>
where
    F: Fn(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Vec<String>>>>,
{
    match fetch("orgs").await? {
        Some(names) => Ok(names),
        None => fetch("users")
            .await?
            .ok_or_else(|| anyhow!("no GitHub org or user named `{owner}`")),
    }
}

/// `Ok(Some(names))` on success, `Ok(None)` when the owner kind is a 404
/// (so the caller can try the other kind), `Err` otherwise.
async fn fetch_packages(
    client: &reqwest::Client,
    token: &str,
    kind: &str,
    owner: &str,
) -> Result<Option<Vec<String>>> {
    let url = format!(
        "https://api.github.com/{kind}/{owner}/packages?package_type=container&per_page=100"
    );
    fetch_packages_from(client, token, kind, owner, url).await
}

async fn fetch_packages_from(
    client: &reqwest::Client,
    token: &str,
    kind: &str,
    owner: &str,
    start: String,
) -> Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    let mut budget = ProviderBudget::default();
    let mut url = Some(start);
    while let Some(u) = url.take() {
        budget.page()?;
        let resp = client
            .execute(package_request(client, &u, token)?)
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
        let page: Vec<GhPackage> = serde_json::from_slice(&budget.body(resp).await?)
            .context("parsing GitHub packages list")?;
        budget.items(page.len())?;
        names.extend(page.into_iter().map(|p| p.name));
        url = next;
    }
    Ok(Some(names))
}

/// Enumerate public packages from the same page GitHub serves to signed-out
/// users. Unlike the Packages REST list endpoint, this does not require a PAT.
async fn fetch_public_packages(
    client: &reqwest::Client,
    kind: &str,
    owner: &str,
) -> Result<Option<Vec<String>>> {
    let url = format!("https://github.com/{kind}/{owner}/packages?ecosystem=container");
    fetch_public_packages_from(client, url).await
}

async fn fetch_public_packages_from(
    client: &reqwest::Client,
    start: String,
) -> Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    let mut budget = ProviderBudget::default();
    let mut url = Some(start);

    while let Some(u) = url.take() {
        budget.page()?;
        let response = client
            .get(&u)
            .send()
            .await
            .context("GitHub public packages request failed")?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => return Ok(None),
            status if !status.is_success() => {
                bail!("GitHub public packages page returned {status}")
            }
            _ => {}
        }
        let body = String::from_utf8(budget.body(response).await?)
            .context("reading GitHub public packages page")?;
        let (page_names, next) = parse_public_packages_page(&body)?;
        budget.items(page_names.len())?;
        names.extend(page_names);
        url = next.map(|next| {
            if next.starts_with("https://") || next.starts_with("http://") {
                next
            } else {
                format!("https://github.com{next}")
            }
        });
    }
    Ok(Some(names))
}

fn parse_public_packages_page(body: &str) -> Result<(Vec<String>, Option<String>)> {
    let package = Regex::new(
        r#"title="([a-z0-9][a-z0-9._/-]*)"[^>]*href="/(?:orgs|users)/[^"/]+/packages/container/package/"#,
    )
    .expect("valid public package regex");
    let next_after_href =
        Regex::new(r#"href="([^"]+)"[^>]*rel="next""#).expect("valid pagination regex");
    let next_before_href =
        Regex::new(r#"rel="next"[^>]*href="([^"]+)""#).expect("valid pagination regex");
    let names = package
        .captures_iter(body)
        .map(|capture| capture[1].to_string())
        .collect();
    let next = next_after_href
        .captures(body)
        .or_else(|| next_before_href.captures(body))
        .map(|capture| capture[1].replace("&amp;", "&"));
    Ok((names, next))
}

/// The GitHub token to list packages: the registry login's password (a PAT)
/// if present, else `$GH_TOKEN` / `$GITHUB_TOKEN`.
fn github_token(host: &str) -> Option<String> {
    if let Ok(Credential::Basic { password, .. }) = auth::resolve(host)
        && !password.is_empty()
    {
        return Some(password);
    }
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.is_empty()
        {
            return Some(t);
        }
    }
    None
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

    async fn fake_provider(
        status: &str,
        headers: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n{body}")
            .replace("{SELF}", &url);
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            // Enough for the entire production page budget, without test-only limits.
            for _ in 0..MAX_LIST_PAGES {
                let accepted =
                    tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                        .await;
                let Ok(Ok((mut socket, _))) = accepted else {
                    break;
                };
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 1024];
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = socket.write_all(response.as_bytes()).await;
            }
            requests
        });
        (url, task)
    }

    #[test]
    fn pat_has_an_explicit_https_api_allow_list() {
        let client = provider_client().unwrap();
        for (url, allowed) in [
            ("https://api.github.com/x", true),
            ("https://api.github.com:443/x", true),
            ("http://api.github.com/x", false),
            ("https://api.github.com:444/x", false),
            ("https://api.github.com.evil.example/x", false),
            ("https://github.com/x", false),
        ] {
            let request = package_request(&client, url, "fake-secret").unwrap();
            assert_eq!(
                request.headers().contains_key("authorization"),
                allowed,
                "{url}"
            );
        }
        assert!(package_request(&client, "https://user@api.github.com/x", "fake-secret").is_err());
    }

    #[tokio::test]
    async fn hostile_link_and_redirect_cannot_receive_pat() {
        let client = provider_client().unwrap();
        let (hostile, received) = fake_provider("200 OK", "Content-Length: 2\r\n", "[]").await;
        let (source, first) = fake_provider(
            "200 OK",
            &format!("Content-Length: 2\r\nLink: <{hostile}>; rel=\"next\"\r\n"),
            "[]",
        )
        .await;
        fetch_packages_from(&client, "fake-secret", "orgs", "test", source)
            .await
            .unwrap();
        for request in first
            .await
            .unwrap()
            .into_iter()
            .chain(received.await.unwrap())
        {
            assert!(!request.to_lowercase().contains("authorization:"));
            assert!(!request.contains("fake-secret"));
        }

        let (hostile, received) = fake_provider("200 OK", "Content-Length: 2\r\n", "[]").await;
        let (source, first) = fake_provider(
            "302 Found",
            &format!("Content-Length: 0\r\nLocation: {hostile}\r\n"),
            "",
        )
        .await;
        assert!(
            fetch_packages_from(&client, "fake-secret", "orgs", "test", source)
                .await
                .is_err()
        );
        assert_eq!(first.await.unwrap().len(), 1);
        assert!(received.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_reads_reject_declared_and_chunked_oversize() {
        let client = provider_client().unwrap();
        let large = "x".repeat(MAX_JSON_BODY + 1);
        for (headers, body) in [
            (
                format!("Content-Length: {}\r\n", MAX_JSON_BODY + 1),
                String::new(),
            ),
            (
                "Transfer-Encoding: chunked\r\n".into(),
                format!("{:x}\r\n{large}\r\n0\r\n\r\n", large.len()),
            ),
        ] {
            let (url, task) = fake_provider("200 OK", &headers, &body).await;
            let response = client.get(url).send().await.unwrap();
            assert!(ProviderBudget::default().body(response).await.is_err());
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn endless_packages_stop_at_page_limit() {
        let (source, task) = fake_provider(
            "200 OK",
            "Content-Length: 2\r\nLink: <{SELF}>; rel=\"next\"\r\n",
            "[]",
        )
        .await;
        let error = fetch_packages_from(
            &provider_client().unwrap(),
            "fake-secret",
            "orgs",
            "test",
            source,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("page limit"));
        assert_eq!(task.await.unwrap().len(), MAX_LIST_PAGES);
    }

    #[tokio::test]
    async fn endless_public_pages_stop_at_page_limit() {
        let html = "<a href=\"{SELF}\" rel=\"next\">Next</a>";
        // Content-Length is computed after substituting the local server URL.
        let (source, task) = fake_provider("200 OK", "", html).await;
        let error = fetch_public_packages_from(&provider_client().unwrap(), source)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("page limit"));
        assert_eq!(task.await.unwrap().len(), MAX_LIST_PAGES);
    }

    #[tokio::test]
    async fn aggregate_byte_budget_is_enforced_on_the_stream() {
        let mut budget = ProviderBudget {
            bytes: MAX_LIST_BYTES - 2,
            ..ProviderBudget::default()
        };
        let (url, task) = fake_provider(
            "200 OK",
            "Transfer-Encoding: chunked\r\n",
            "3\r\nabc\r\n0\r\n\r\n",
        )
        .await;
        let response = provider_client().unwrap().get(url).send().await.unwrap();
        assert!(budget.body(response).await.is_err());
        task.await.unwrap();
    }

    #[test]
    fn aggregate_items_and_bytes_are_bounded() {
        let mut budget = ProviderBudget::default();
        budget.items(MAX_LIST_ITEMS).unwrap();
        assert!(budget.items(1).is_err());
        assert_eq!(MAX_LIST_BYTES, 4 * MAX_JSON_BODY);
    }

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

    #[test]
    fn parses_signed_out_github_packages_page() {
        let html = r#"
          <a title="vmlab-templates/alpine-3.23"
             href="/orgs/VMLabDev/packages/container/package/vmlab-templates%2Falpine-3.23">Alpine</a>
          <a href="/orgs/VMLabDev/packages?ecosystem=container&amp;page=2" rel="next">Next</a>
        "#;
        let (names, next) = parse_public_packages_page(html).unwrap();
        assert_eq!(names, ["vmlab-templates/alpine-3.23"]);
        assert_eq!(
            next.as_deref(),
            Some("/orgs/VMLabDev/packages?ecosystem=container&page=2")
        );
    }
}
