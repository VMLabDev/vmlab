//! The OCI distribution client (PRD §6.4).
//!
//! Push/pull orchestration is written against a [`Transport`] trait that
//! captures the handful of distribution-API operations vmlab needs (blob
//! existence/get/put, manifest get/put). The real implementation,
//! [`HttpTransport`], drives a `reqwest` client and handles the Bearer
//! token flow and per-chunk retry/backoff; tests drive a fake transport, so
//! the chunk/manifest/index logic is exercised without a network.
//!
//! ## Multi-arch
//!
//! A single-arch push creates a plain image manifest and points the tag at
//! it. A multi-arch tag is assembled incrementally: each arch's `push` to
//! the *same tag* fetches the existing tag, and if it is already an index
//! (or a manifest we promote into one), merges this arch's manifest
//! descriptor in and re-PUTs the index. So the multi-arch index is the
//! union of the arches pushed to a tag, with `pull --arch` selecting one.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::auth::{self, BearerChallenge, Credential};
use super::chunking;
use super::config_blob::TemplateConfig;
use super::manifest::{
    Descriptor, ImageIndex, Manifest, ManifestOrIndex, annotations, build_manifest,
    parse_manifest_or_index,
};
use super::media_types;
use super::reference::Reference;
use crate::template::store::{DISK_FILE, TemplateStore};
use crate::template::{META_FILE, TemplateMeta};

/// A blob or manifest body plus the media type the server reported.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub media_type: String,
    pub body: Vec<u8>,
}

/// The distribution-API surface push/pull is written against. Digests are
/// `sha256:<hex>` strings; `repository` is the repo path under the host.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// HEAD a blob: does `digest` already exist in `repository`?
    async fn blob_exists(&self, repository: &str, digest: &str) -> Result<bool>;
    /// GET a blob's bytes.
    async fn get_blob(&self, repository: &str, digest: &str) -> Result<Vec<u8>>;
    /// Upload `data` as a blob with the given `digest` (monolithic).
    async fn put_blob(&self, repository: &str, digest: &str, data: Vec<u8>) -> Result<()>;
    /// Upload a blob whose bytes are streamed from a file on disk.
    async fn put_blob_file(&self, repository: &str, digest: &str, path: &Path) -> Result<()>;
    /// GET a manifest/index by reference (tag or digest), returning the
    /// raw bytes and reported media type. `Ok(None)` if it does not exist.
    async fn get_manifest(&self, repository: &str, reference: &str) -> Result<Option<Fetched>>;
    /// PUT a manifest/index by reference with the given media type.
    async fn put_manifest(
        &self,
        repository: &str,
        reference: &str,
        media_type: &str,
        body: Vec<u8>,
    ) -> Result<String>;
    /// List the tags of `repository` (`GET /v2/<repo>/tags/list`). Returns an
    /// empty vec when the repository does not exist yet.
    async fn list_tags(&self, repository: &str) -> Result<Vec<String>>;
    /// List every repository on the registry (`GET /v2/_catalog`). Errors on
    /// registries that don't implement the catalog endpoint (e.g. GHCR).
    async fn list_catalog(&self) -> Result<Vec<String>>;
}

/// The vmlab registry client.
pub struct Registry {
    reference: Reference,
    transport: Box<dyn Transport>,
}

impl Registry {
    /// Build a client for `reference` using the real HTTP transport, with
    /// credentials resolved from Docker config (PRD §6.4 auth reuse).
    pub fn new(reference: &str) -> Result<Self> {
        let reference = Reference::parse(reference)?;
        let credential = auth::resolve(&reference.host)?;
        let transport = HttpTransport::new(reference.host.clone(), credential)?;
        Ok(Self {
            reference,
            transport: Box::new(transport),
        })
    }

    /// Build a client over an explicit transport (used by tests).
    pub fn with_transport(reference: Reference, transport: Box<dyn Transport>) -> Self {
        Self {
            reference,
            transport,
        }
    }

    pub fn host(&self) -> &str {
        &self.reference.host
    }
    pub fn repository(&self) -> &str {
        &self.reference.repository
    }
    pub fn tag(&self) -> &str {
        &self.reference.tag
    }

    /// List the tags published under this reference's repository (the version
    /// tags plus any moving aliases). Empty when the repo does not yet exist.
    pub async fn list_tags(&self) -> Result<Vec<String>> {
        self.transport.list_tags(&self.reference.repository).await
    }

    /// List every repository on this reference's registry (`/v2/_catalog`).
    pub async fn list_catalog(&self) -> Result<Vec<String>> {
        self.transport.list_catalog().await
    }

    /// The architectures available under this reference's tag: the platform
    /// entries of a multi-arch index, or the single arch of a plain manifest.
    pub async fn index_arches(&self, tag: &str) -> Result<Vec<String>> {
        let repo = &self.reference.repository;
        let top = self
            .transport
            .get_manifest(repo, tag)
            .await?
            .ok_or_else(|| anyhow!("{}/{repo}:{tag} not found", self.reference.host))?;
        match parse_manifest_or_index(&top.body)? {
            ManifestOrIndex::Index(index) => Ok(index
                .manifests
                .iter()
                .filter_map(|d| d.platform.as_ref().map(|p| p.architecture.clone()))
                .collect()),
            ManifestOrIndex::Manifest(m) => {
                // Plain single-arch manifest: read its config blob for the arch.
                let config_bytes = self
                    .transport
                    .get_blob(repo, &m.config.digest)
                    .await
                    .context("downloading config blob")?;
                Ok(vec![TemplateConfig::from_json(&config_bytes)?.arch])
            }
        }
    }

    /// Push a template directory (containing `disk.qcow2` + `template.wcl`)
    /// to the reference for the given `arch`. Chunks the disk, uploads each
    /// chunk blob (skipping any already present), uploads the config blob,
    /// PUTs the per-arch manifest, then merges that manifest into the tag's
    /// index so repeated pushes for different arches build a multi-arch
    /// index (PRD §6.4).
    pub async fn push(
        &self,
        template_dir: &Path,
        chunk_size: u64,
        arch: &str,
        work_dir: &Path,
        source: Option<&str>,
        moving_tag: Option<&str>,
    ) -> Result<()> {
        let repo = self.reference.repository.clone();
        let meta = TemplateMeta::read_from(&template_dir.join(META_FILE)).with_context(|| {
            format!(
                "cannot read template metadata in {}",
                template_dir.display()
            )
        })?;
        let disk = template_dir.join(DISK_FILE);

        // 1. Chunk + compress.
        let chunks_dir = work_dir.join("chunks");
        let set = chunking::chunk_and_compress(&disk, chunk_size, &chunks_dir)?;

        // 2. Upload chunk blobs (skip if present).
        for c in &set.chunks {
            if self
                .transport
                .blob_exists(&repo, &c.compressed_digest)
                .await?
            {
                tracing::debug!(digest = %c.compressed_digest, "chunk already present, skipping");
                continue;
            }
            self.transport
                .put_blob_file(&repo, &c.compressed_digest, &c.compressed_path)
                .await
                .with_context(|| format!("uploading chunk {}", c.index))?;
        }

        // 3. Upload the config blob.
        let mut config_meta = meta.clone();
        config_meta.arch = arch.to_string();
        let config_bytes = TemplateConfig::from_meta(&config_meta).to_json()?;
        let config_digest = digest_of(&config_bytes);
        if !self.transport.blob_exists(&repo, &config_digest).await? {
            self.transport
                .put_blob(&repo, &config_digest, config_bytes.clone())
                .await
                .context("uploading config blob")?;
        }
        let config_desc = Descriptor::new(
            media_types::CONFIG_TEMPLATE_JSON,
            &config_digest,
            config_bytes.len() as u64,
        );

        // 4. Build + PUT the per-arch manifest (addressed by its digest so
        // the index can reference it stably). Stamp the source-repo
        // annotation so registries connect the package to its repo (GHCR).
        let mut manifest = build_manifest(&set, config_desc);
        if let Some(src) = source {
            manifest
                .annotations
                .get_or_insert_with(BTreeMap::new)
                .insert(annotations::IMAGE_SOURCE.to_string(), src.to_string());
        }
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        let manifest_digest = digest_of(&manifest_bytes);
        self.transport
            .put_manifest(
                &repo,
                &manifest_digest,
                media_types::OCI_MANIFEST,
                manifest_bytes.clone(),
            )
            .await
            .context("uploading manifest")?;

        // 5. Merge into the tag's index (creating it if absent / promoting
        // a plain manifest tag into an index).
        let mut index = match self
            .transport
            .get_manifest(&repo, &self.reference.tag)
            .await?
        {
            Some(f) => match parse_manifest_or_index(&f.body)? {
                ManifestOrIndex::Index(i) => i,
                ManifestOrIndex::Manifest(_) => ImageIndex::new(),
            },
            None => ImageIndex::new(),
        };
        index.upsert_arch(
            arch,
            Descriptor::new(
                media_types::OCI_MANIFEST,
                &manifest_digest,
                manifest_bytes.len() as u64,
            ),
        );
        if let Some(src) = source {
            index
                .annotations
                .get_or_insert_with(BTreeMap::new)
                .insert(annotations::IMAGE_SOURCE.to_string(), src.to_string());
        }
        let index_bytes = serde_json::to_vec(&index)?;
        self.transport
            .put_manifest(
                &repo,
                &self.reference.tag,
                media_types::OCI_INDEX,
                index_bytes.clone(),
            )
            .await
            .context("uploading tag index")?;

        // Re-point a moving alias (`latest` / `latest-prerelease`) at the same
        // merged index — no blob re-upload (PRD §6.4). Each arch push leaves the
        // alias on the latest complete index.
        if let Some(alias) = moving_tag {
            self.transport
                .put_manifest(&repo, alias, media_types::OCI_INDEX, index_bytes)
                .await
                .with_context(|| format!("updating moving tag {alias}"))?;
        }

        tracing::info!(
            reference = %self.reference.canonical(),
            arch,
            chunks = set.chunk_count,
            "pushed template"
        );
        Ok(())
    }

    /// Resolve this reference's tag to the per-arch [`Manifest`] and its
    /// [`TemplateMeta`] (config blob) — the cheap part of a pull, with no chunk
    /// download. Used to learn the real version behind a moving tag.
    async fn resolve_manifest_meta(&self, arch: Option<&str>) -> Result<(Manifest, TemplateMeta)> {
        let repo = &self.reference.repository;

        // 1. Fetch the tag and resolve to a single manifest.
        let top = self
            .transport
            .get_manifest(repo, &self.reference.tag)
            .await?
            .ok_or_else(|| anyhow!("reference {} not found", self.reference.canonical()))?;
        let manifest = match parse_manifest_or_index(&top.body)? {
            ManifestOrIndex::Manifest(m) => m,
            ManifestOrIndex::Index(index) => {
                let desc = index.resolve(arch)?;
                let fetched = self
                    .transport
                    .get_manifest(repo, &desc.digest)
                    .await?
                    .ok_or_else(|| anyhow!("manifest {} missing from registry", desc.digest))?;
                match parse_manifest_or_index(&fetched.body)? {
                    ManifestOrIndex::Manifest(m) => m,
                    ManifestOrIndex::Index(_) => {
                        bail!("index entry {} is itself an index", desc.digest)
                    }
                }
            }
        };

        // 2. Refuse non-vmlab artifacts.
        if !manifest.is_vmlab_template() {
            bail!(
                "{} is not a vmlab template (artifactType {:?}) — refusing to pull",
                self.reference.canonical(),
                manifest.artifact_type
            );
        }

        // 3. Download config → TemplateMeta.
        let config_bytes = self
            .transport
            .get_blob(repo, &manifest.config.digest)
            .await
            .context("downloading config blob")?;
        let config = TemplateConfig::from_json(&config_bytes)?;
        let meta = config.into_meta(Some(self.reference.canonical()))?;

        // A requested arch must match what we resolved (defence in depth
        // for the plain-manifest path which has no index keying).
        if let Some(a) = arch
            && a != meta.arch
        {
            bail!(
                "requested arch `{a}` but {} is arch `{}`",
                self.reference.canonical(),
                meta.arch
            );
        }
        Ok((manifest, meta))
    }

    /// The concrete version behind this reference's tag for `arch` (resolves a
    /// moving alias like `latest` without downloading the disk).
    pub async fn resolve_version(&self, arch: Option<&str>) -> Result<String> {
        Ok(self.resolve_manifest_meta(arch).await?.1.version)
    }

    /// Pull the reference for `arch` into `dest_store`. Fetches the tag
    /// (resolving the index when present — `arch` required unless single),
    /// verifies the manifest is a vmlab artifact, downloads chunks in
    /// order, assembles, verifies the whole-image digest, then installs
    /// into the store recording the originating reference as the origin
    /// (PRD §6.4).
    pub async fn pull(
        &self,
        arch: Option<&str>,
        dest_store: &TemplateStore,
        work_dir: &Path,
        overwrite: bool,
    ) -> Result<TemplateMeta> {
        let repo = &self.reference.repository;

        // Steps 1-3: resolve the tag to the per-arch manifest + metadata
        // (no chunk download yet).
        let (manifest, meta) = self.resolve_manifest_meta(arch).await?;

        // 4. Download chunks in order to a temp dir.
        let chunks_dir = work_dir.join("chunks");
        std::fs::create_dir_all(&chunks_dir)
            .with_context(|| format!("cannot create {}", chunks_dir.display()))?;
        let mut chunk_paths = Vec::new();
        for (i, layer) in manifest.layers_in_order().into_iter().enumerate() {
            let bytes = self
                .transport
                .get_blob(repo, &layer.digest)
                .await
                .with_context(|| format!("downloading chunk {i}"))?;
            // Verify each chunk's compressed digest as we go.
            let got = digest_of(&bytes);
            if !got.eq_ignore_ascii_case(&layer.digest) {
                bail!(
                    "chunk {i} digest mismatch: expected {}, got {got}",
                    layer.digest
                );
            }
            let path = chunks_dir.join(format!("chunk-{i:04}.zst"));
            std::fs::write(&path, &bytes)
                .with_context(|| format!("cannot write {}", path.display()))?;
            chunk_paths.push(path);
        }

        // 5. Assemble + verify the whole-image digest.
        let staging = work_dir.join("staging");
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("cannot create {}", staging.display()))?;
        let disk = staging.join(DISK_FILE);
        chunking::assemble(&chunk_paths, &disk)?;
        let whole = manifest
            .whole_digest()
            .ok_or_else(|| anyhow!("manifest is missing the whole-image digest annotation"))?;
        chunking::verify_whole(&disk, whole)?;

        // 6. Install into the store (staging must be on the same FS as the
        // store; the caller is expected to pass a work_dir under the store
        // root). Record the disk digest if absent.
        let mut meta = meta;
        if meta.sha256.is_none() {
            meta.sha256 = Some(crate::template::store::sha256_file(&disk)?);
        }
        dest_store.install(&staging, &meta, overwrite)?;
        tracing::info!(
            reference = %self.reference.canonical(),
            arch = %meta.arch,
            "pulled template into store"
        );
        Ok(meta)
    }
}

/// `sha256:<hex>` of a byte slice.
fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

// ---- HTTP transport --------------------------------------------------------

/// reqwest-backed [`Transport`] with the Bearer token flow and per-call
/// retry/backoff.
pub struct HttpTransport {
    host: String,
    base: String,
    credential: Credential,
    client: reqwest::Client,
    /// Cached bearer token per scope string.
    tokens: tokio::sync::Mutex<BTreeMap<String, String>>,
}

const MAX_RETRIES: u32 = 3;
const ACCEPT_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default, rename = "access_token")]
    access_token: Option<String>,
}

impl HttpTransport {
    pub fn new(host: String, credential: Credential) -> Result<Self> {
        let scheme = if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
            "http"
        } else {
            "https"
        };
        let base = format!("{scheme}://{host}");
        let client = reqwest::Client::builder()
            .user_agent("vmlab-oci/1")
            .build()
            .context("cannot build HTTP client")?;
        Ok(Self {
            host,
            base,
            credential,
            client,
            tokens: tokio::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Send a request builder factory, performing the 401 Bearer token
    /// flow and a couple of retries on transient (5xx / network) errors.
    /// The factory is called afresh each attempt because request bodies are
    /// not cloneable.
    async fn send_with_auth<F>(&self, scope: &str, make: F) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = make();
            if let Some(token) = self.tokens.lock().await.get(scope).cloned() {
                req = req.bearer_auth(token);
            } else if let Some(basic) = self.credential.basic_header() {
                req = req.header(reqwest::header::AUTHORIZATION, basic);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < MAX_RETRIES && is_transient(&e) => {
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(e).context("HTTP request failed"),
            };

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                && let Some(challenge) = resp
                    .headers()
                    .get(reqwest::header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(auth::parse_bearer_challenge)
            {
                let token = self.fetch_token(&challenge).await?;
                self.tokens.lock().await.insert(scope.to_string(), token);
                // Retry now that we have a token.
                if attempt < MAX_RETRIES {
                    continue;
                }
            }

            if resp.status().is_server_error() && attempt < MAX_RETRIES {
                backoff(attempt).await;
                continue;
            }
            return Ok(resp);
        }
    }

    /// Exchange a Bearer challenge for a token, GETting the realm with basic
    /// auth when a credential is available (anonymous otherwise).
    async fn fetch_token(&self, challenge: &BearerChallenge) -> Result<String> {
        let mut req = self.client.get(&challenge.realm);
        let mut query = Vec::new();
        if let Some(service) = &challenge.service {
            query.push(("service", service.clone()));
        }
        if let Some(scope) = &challenge.scope {
            query.push(("scope", scope.clone()));
        }
        if !query.is_empty() {
            req = req.query(&query);
        }
        if let Some(basic) = self.credential.basic_header() {
            req = req.header(reqwest::header::AUTHORIZATION, basic);
        }
        let resp = req.send().await.context("token request failed")?;
        if !resp.status().is_success() {
            bail!(
                "token endpoint {} returned {}",
                challenge.realm,
                resp.status()
            );
        }
        let body: TokenResponse = resp.json().await.context("malformed token response")?;
        body.token
            .or(body.access_token)
            .ok_or_else(|| anyhow!("token response had no token"))
    }

    fn scope(&self, repository: &str, push: bool) -> String {
        let action = if push { "pull,push" } else { "pull" };
        format!("repository:{repository}:{action}")
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn blob_exists(&self, repository: &str, digest: &str) -> Result<bool> {
        let url = self.url(&format!("/v2/{repository}/blobs/{digest}"));
        let scope = self.scope(repository, false);
        let resp = self
            .send_with_auth(&scope, || self.client.head(&url))
            .await?;
        Ok(resp.status().is_success())
    }

    async fn get_blob(&self, repository: &str, digest: &str) -> Result<Vec<u8>> {
        let url = self.url(&format!("/v2/{repository}/blobs/{digest}"));
        let scope = self.scope(repository, false);
        let resp = self
            .send_with_auth(&scope, || self.client.get(&url))
            .await?;
        if !resp.status().is_success() {
            bail!("GET blob {digest} returned {}", resp.status());
        }
        Ok(resp.bytes().await.context("reading blob body")?.to_vec())
    }

    async fn put_blob(&self, repository: &str, digest: &str, data: Vec<u8>) -> Result<()> {
        let location = self.start_upload(repository).await?;
        let scope = self.scope(repository, true);
        let sep = if location.contains('?') { '&' } else { '?' };
        let url = format!("{location}{sep}digest={digest}");
        let resp = self
            .send_with_auth(&scope, || {
                self.client
                    .put(&url)
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(data.clone())
            })
            .await?;
        if !resp.status().is_success() {
            bail!("PUT blob {digest} returned {}", resp.status());
        }
        Ok(())
    }

    async fn put_blob_file(&self, repository: &str, digest: &str, path: &Path) -> Result<()> {
        let data = tokio::fs::read(path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        self.put_blob(repository, digest, data).await
    }

    async fn get_manifest(&self, repository: &str, reference: &str) -> Result<Option<Fetched>> {
        let url = self.url(&format!("/v2/{repository}/manifests/{reference}"));
        let scope = self.scope(repository, false);
        let resp = self
            .send_with_auth(&scope, || {
                self.client
                    .get(&url)
                    .header(reqwest::header::ACCEPT, ACCEPT_MANIFEST)
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("GET manifest {reference} returned {}", resp.status());
        }
        let media_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(media_types::OCI_MANIFEST)
            .to_string();
        let body = resp
            .bytes()
            .await
            .context("reading manifest body")?
            .to_vec();
        Ok(Some(Fetched { media_type, body }))
    }

    async fn put_manifest(
        &self,
        repository: &str,
        reference: &str,
        media_type: &str,
        body: Vec<u8>,
    ) -> Result<String> {
        let url = self.url(&format!("/v2/{repository}/manifests/{reference}"));
        let scope = self.scope(repository, true);
        let resp = self
            .send_with_auth(&scope, || {
                self.client
                    .put(&url)
                    .header(reqwest::header::CONTENT_TYPE, media_type)
                    .body(body.clone())
            })
            .await?;
        if !resp.status().is_success() {
            bail!("PUT manifest {reference} returned {}", resp.status());
        }
        let digest = resp
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .unwrap_or_else(|| digest_of(&body));
        Ok(digest)
    }

    async fn list_tags(&self, repository: &str) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct TagList {
            #[serde(default)]
            tags: Option<Vec<String>>,
        }
        let scope = self.scope(repository, false);
        let mut tags = Vec::new();
        let mut next = Some(self.url(&format!("/v2/{repository}/tags/list?n=1000")));
        while let Some(url) = next.take() {
            let resp = self
                .send_with_auth(&scope, || self.client.get(&url))
                .await?;
            // A repository with nothing published yet 404s — treat as empty.
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }
            if !resp.status().is_success() {
                bail!("GET tags/list returned {}", resp.status());
            }
            let link = resp
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = resp.bytes().await.context("reading tags/list body")?;
            let parsed: TagList =
                serde_json::from_slice(&body).context("parsing tags/list response")?;
            if let Some(mut t) = parsed.tags {
                tags.append(&mut t);
            }
            // Follow `Link: <...>; rel="next"` pagination.
            next = link.and_then(|l| next_link(&l)).map(|rel| {
                if rel.starts_with("http://") || rel.starts_with("https://") {
                    rel
                } else {
                    self.url(&rel)
                }
            });
        }
        Ok(tags)
    }

    async fn list_catalog(&self) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Catalog {
            #[serde(default)]
            repositories: Option<Vec<String>>,
        }
        let scope = "registry:catalog:*";
        let mut repos = Vec::new();
        let mut next = Some(self.url("/v2/_catalog?n=1000"));
        while let Some(url) = next.take() {
            let resp = self.send_with_auth(scope, || self.client.get(&url)).await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!(
                    "registry {} does not support the catalog endpoint (/v2/_catalog)",
                    self.host
                );
            }
            if !resp.status().is_success() {
                bail!("GET _catalog returned {}", resp.status());
            }
            let link = resp
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = resp.bytes().await.context("reading _catalog body")?;
            let parsed: Catalog =
                serde_json::from_slice(&body).context("parsing _catalog response")?;
            if let Some(mut r) = parsed.repositories {
                repos.append(&mut r);
            }
            next = link.and_then(|l| next_link(&l)).map(|rel| {
                if rel.starts_with("http://") || rel.starts_with("https://") {
                    rel
                } else {
                    self.url(&rel)
                }
            });
        }
        Ok(repos)
    }
}

/// Extract the `rel="next"` URL from an RFC 5988 `Link` header, if present.
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

impl HttpTransport {
    /// POST `/v2/<repo>/blobs/uploads/` and return the upload location URL.
    async fn start_upload(&self, repository: &str) -> Result<String> {
        let url = self.url(&format!("/v2/{repository}/blobs/uploads/"));
        let scope = self.scope(repository, true);
        let resp = self
            .send_with_auth(&scope, || self.client.post(&url))
            .await?;
        if !resp.status().is_success() {
            bail!("starting blob upload returned {}", resp.status());
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("blob upload response had no Location header"))?;
        // Location may be relative to the registry host.
        if location.starts_with("http://") || location.starts_with("https://") {
            Ok(location.to_string())
        } else {
            Ok(self.url(location))
        }
    }
}

fn is_transient(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

async fn backoff(attempt: u32) {
    let ms = 100u64 * 2u64.pow(attempt.saturating_sub(1));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::chunking;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory fake registry: blobs keyed by digest, manifests keyed
    /// by reference (tag or digest), all per-repository.
    #[derive(Default)]
    struct FakeRegistry {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        manifests: Mutex<HashMap<String, (String, Vec<u8>)>>,
    }

    fn key(repo: &str, id: &str) -> String {
        format!("{repo}@{id}")
    }

    #[async_trait::async_trait]
    impl Transport for FakeRegistry {
        async fn blob_exists(&self, repo: &str, digest: &str) -> Result<bool> {
            Ok(self.blobs.lock().unwrap().contains_key(&key(repo, digest)))
        }
        async fn get_blob(&self, repo: &str, digest: &str) -> Result<Vec<u8>> {
            self.blobs
                .lock()
                .unwrap()
                .get(&key(repo, digest))
                .cloned()
                .ok_or_else(|| anyhow!("no blob {digest}"))
        }
        async fn put_blob(&self, repo: &str, digest: &str, data: Vec<u8>) -> Result<()> {
            // verify the digest like a real registry does
            let got = digest_of(&data);
            assert_eq!(got, digest, "fake registry: digest mismatch on put");
            self.blobs.lock().unwrap().insert(key(repo, digest), data);
            Ok(())
        }
        async fn put_blob_file(&self, repo: &str, digest: &str, path: &Path) -> Result<()> {
            let data = std::fs::read(path)?;
            self.put_blob(repo, digest, data).await
        }
        async fn get_manifest(&self, repo: &str, reference: &str) -> Result<Option<Fetched>> {
            Ok(self
                .manifests
                .lock()
                .unwrap()
                .get(&key(repo, reference))
                .map(|(mt, b)| Fetched {
                    media_type: mt.clone(),
                    body: b.clone(),
                }))
        }
        async fn put_manifest(
            &self,
            repo: &str,
            reference: &str,
            media_type: &str,
            body: Vec<u8>,
        ) -> Result<String> {
            let digest = digest_of(&body);
            // a real registry stores by digest too
            self.manifests
                .lock()
                .unwrap()
                .insert(key(repo, &digest), (media_type.to_string(), body.clone()));
            self.manifests
                .lock()
                .unwrap()
                .insert(key(repo, reference), (media_type.to_string(), body));
            Ok(digest)
        }
        async fn list_tags(&self, repo: &str) -> Result<Vec<String>> {
            let prefix = format!("{repo}@");
            let mut tags: Vec<String> = self
                .manifests
                .lock()
                .unwrap()
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix))
                .filter(|id| !id.starts_with("sha256:"))
                .map(String::from)
                .collect();
            tags.sort();
            tags.dedup();
            Ok(tags)
        }
        async fn list_catalog(&self) -> Result<Vec<String>> {
            let mut repos: Vec<String> = self
                .manifests
                .lock()
                .unwrap()
                .keys()
                .filter_map(|k| k.rsplit_once('@').map(|(repo, _)| repo.to_string()))
                .collect();
            repos.sort();
            repos.dedup();
            Ok(repos)
        }
    }

    fn meta(arch: &str) -> TemplateMeta {
        TemplateMeta {
            name: "alpine".into(),
            arch: arch.into(),
            version: "3.20".into(),
            profile: Some("linux".into()),
            cpus: Some(2),
            memory: Some(4 << 30),
            disk: Some(20 << 30),
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            created: "2026-06-12T00:00:00Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
        }
    }

    /// Stage a fake template dir with a disk and metadata.
    fn make_template(dir: &Path, m: &TemplateMeta, disk_bytes: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(DISK_FILE), disk_bytes).unwrap();
        m.write_to(&dir.join(META_FILE)).unwrap();
    }

    fn registry_with_fake(reference: &str, fake: std::sync::Arc<FakeRegistry>) -> Registry {
        struct Shared(std::sync::Arc<FakeRegistry>);
        #[async_trait::async_trait]
        impl Transport for Shared {
            async fn blob_exists(&self, r: &str, d: &str) -> Result<bool> {
                self.0.blob_exists(r, d).await
            }
            async fn get_blob(&self, r: &str, d: &str) -> Result<Vec<u8>> {
                self.0.get_blob(r, d).await
            }
            async fn put_blob(&self, r: &str, d: &str, data: Vec<u8>) -> Result<()> {
                self.0.put_blob(r, d, data).await
            }
            async fn put_blob_file(&self, r: &str, d: &str, p: &Path) -> Result<()> {
                self.0.put_blob_file(r, d, p).await
            }
            async fn get_manifest(&self, r: &str, rf: &str) -> Result<Option<Fetched>> {
                self.0.get_manifest(r, rf).await
            }
            async fn put_manifest(
                &self,
                r: &str,
                rf: &str,
                mt: &str,
                b: Vec<u8>,
            ) -> Result<String> {
                self.0.put_manifest(r, rf, mt, b).await
            }
            async fn list_tags(&self, r: &str) -> Result<Vec<String>> {
                self.0.list_tags(r).await
            }
            async fn list_catalog(&self) -> Result<Vec<String>> {
                self.0.list_catalog().await
            }
        }
        Registry::with_transport(Reference::parse(reference).unwrap(), Box::new(Shared(fake)))
    }

    #[tokio::test]
    async fn push_pull_round_trip() {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();

        // create a ~5 MiB disk
        let disk: Vec<u8> = (0..(5u32 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
        let tdir = work.path().join("tmpl");
        let m = meta("x86_64");
        make_template(&tdir, &m, &disk);

        let reg = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone());
        reg.push(
            &tdir,
            1024 * 1024,
            "x86_64",
            &work.path().join("push"),
            Some("https://github.com/owner/repo"),
            Some("latest"),
        )
        .await
        .unwrap();

        // the moving `latest` tag points at the same index as the version tag.
        let ver_idx = fake.get_manifest("owner/alpine", "3.20").await.unwrap();
        let latest_idx = fake.get_manifest("owner/alpine", "latest").await.unwrap();
        assert_eq!(
            ver_idx.map(|f| f.body),
            latest_idx.map(|f| f.body),
            "latest should mirror the version index"
        );

        // the source-repo annotation is stamped on the tag's index (what a
        // registry reads to connect the package to its repo).
        let index_raw = fake
            .get_manifest("owner/alpine", "3.20")
            .await
            .unwrap()
            .unwrap();
        let index: ImageIndex = serde_json::from_slice(&index_raw.body).unwrap();
        assert_eq!(
            index
                .annotations
                .as_ref()
                .and_then(|a| a.get("org.opencontainers.image.source"))
                .map(String::as_str),
            Some("https://github.com/owner/repo")
        );

        // pull into a store; work_dir lives under the store root so the
        // install rename stays on one filesystem.
        let store_root = work.path().join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let store = TemplateStore::new(store_root.clone());
        let pull_work = store_root.join(".oci-pull");
        let reg2 = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone());
        let pulled = reg2
            .pull(Some("x86_64"), &store, &pull_work, false)
            .await
            .unwrap();

        assert_eq!(pulled.arch, "x86_64");
        assert_eq!(pulled.name, "alpine");
        assert_eq!(pulled.origin.as_deref(), Some("ghcr.io/owner/alpine:3.20"));
        // disk reassembled identically
        let resolved = store.resolve("x86_64", "alpine", Some("3.20")).unwrap();
        assert_eq!(std::fs::read(&resolved.disk_path).unwrap(), disk);
    }

    #[tokio::test]
    async fn multi_arch_index_push_then_pull_each() {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();

        let disk_x: Vec<u8> = (0..(2u32 * 1024 * 1024)).map(|i| (i % 7) as u8).collect();
        let disk_a: Vec<u8> = (0..(2u32 * 1024 * 1024)).map(|i| (i % 13) as u8).collect();

        let tx = work.path().join("x");
        let ta = work.path().join("a");
        make_template(&tx, &meta("x86_64"), &disk_x);
        make_template(&ta, &meta("aarch64"), &disk_a);

        // push both arches to the same tag
        registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .push(
                &tx,
                1024 * 1024,
                "x86_64",
                &work.path().join("px"),
                None,
                None,
            )
            .await
            .unwrap();
        registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .push(
                &ta,
                1024 * 1024,
                "aarch64",
                &work.path().join("pa"),
                None,
                None,
            )
            .await
            .unwrap();

        // index_arches reports every arch in the merged index; list_catalog
        // surfaces the repo.
        let mut arches = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .index_arches("3.20")
            .await
            .unwrap();
        arches.sort();
        assert_eq!(arches, vec!["aarch64".to_string(), "x86_64".to_string()]);
        let catalog = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .list_catalog()
            .await
            .unwrap();
        assert!(catalog.contains(&"owner/alpine".to_string()), "{catalog:?}");

        // the tag now resolves to an index — pulling without --arch fails
        let store_root = work.path().join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let store = TemplateStore::new(store_root.clone());
        let no_arch = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .pull(None, &store, &store_root.join(".p0"), false)
            .await;
        assert!(no_arch.is_err());
        assert!(
            no_arch
                .unwrap_err()
                .to_string()
                .contains("--arch is required"),
            "multi-arch pull without --arch must demand it"
        );

        // each arch pulls correctly
        let px = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .pull(Some("x86_64"), &store, &store_root.join(".px"), false)
            .await
            .unwrap();
        assert_eq!(px.arch, "x86_64");
        let pa = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .pull(Some("aarch64"), &store, &store_root.join(".pa"), false)
            .await
            .unwrap();
        assert_eq!(pa.arch, "aarch64");

        assert_eq!(
            std::fs::read(
                &store
                    .resolve("x86_64", "alpine", Some("3.20"))
                    .unwrap()
                    .disk_path
            )
            .unwrap(),
            disk_x
        );
        assert_eq!(
            std::fs::read(
                &store
                    .resolve("aarch64", "alpine", Some("3.20"))
                    .unwrap()
                    .disk_path
            )
            .unwrap(),
            disk_a
        );
    }

    #[tokio::test]
    async fn pull_rejects_non_vmlab_artifact() {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        // hand-craft a manifest with the wrong artifactType under the tag
        let bogus = Manifest {
            schema_version: 2,
            media_type: media_types::OCI_MANIFEST.to_string(),
            artifact_type: Some("application/vnd.oci.image.config.v1+json".to_string()),
            config: Descriptor::new("application/vnd.oci.image.config.v1+json", "sha256:00", 1),
            layers: vec![],
            annotations: None,
        };
        let body = serde_json::to_vec(&bogus).unwrap();
        fake.put_manifest("owner/alpine", "3.20", media_types::OCI_MANIFEST, body)
            .await
            .unwrap();

        let work = tempfile::tempdir().unwrap();
        let store = TemplateStore::new(work.path().join("store"));
        let err = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .pull(Some("x86_64"), &store, work.path(), false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not a vmlab template"),
            "expected vmlab-artifact rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn pull_detects_corrupt_chunk() {
        // Push, then corrupt a chunk blob in the fake registry; pull must
        // fail on the per-chunk digest check.
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        // incompressible disk so a chunk blob is unambiguously the largest.
        let disk: Vec<u8> = (0..(2u32 * 1024 * 1024))
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let tdir = work.path().join("t");
        make_template(&tdir, &meta("x86_64"), &disk);
        registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .push(
                &tdir,
                1024 * 1024,
                "x86_64",
                &work.path().join("p"),
                None,
                None,
            )
            .await
            .unwrap();

        // tamper with the largest blob — a chunk, not the small config blob.
        {
            let mut blobs = fake.blobs.lock().unwrap();
            let target = blobs
                .iter()
                .max_by_key(|(_, v)| v.len())
                .map(|(k, _)| k.clone())
                .unwrap();
            blobs.get_mut(&target).unwrap().push(0xFF);
        }

        let store = TemplateStore::new(work.path().join("store"));
        let err = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .pull(Some("x86_64"), &store, &work.path().join("pull"), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
    }

    #[test]
    fn assemble_matches_chunking_helpers() {
        // sanity: client uses chunking helpers; ensure they are reachable
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("d");
        std::fs::write(&img, b"abc").unwrap();
        let set = chunking::chunk_and_compress(&img, 1024 * 1024, &dir.path().join("c")).unwrap();
        assert_eq!(set.chunk_count, 1);
    }
}
