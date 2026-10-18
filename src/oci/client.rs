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

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::auth::{self, BearerChallenge, Credential};
use super::chunking;
use super::config_blob::TemplateConfig;
use super::manifest::{
    Descriptor, ImageIndex, Manifest, ManifestOrIndex, VariantEntry, annotations, build_manifest,
    build_variant_manifest, parse_manifest_or_index,
};
use super::media_types;
use super::reference::Reference;
use crate::template::TemplateMeta;
use crate::template::store::{DISK_FILE, TemplateStore};

/// A blob or manifest body plus the media type the server reported.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// `Content-Type` as reported by the registry. Unread — manifest/index
    /// discrimination sniffs the JSON instead — but mirrors the wire.
    #[allow(dead_code)]
    pub media_type: String,
    pub body: Vec<u8>,
}

/// Progress of a pull's chunk-download phase, reported to the callback passed
/// to [`Registry::pull_with_progress`]. Byte counts are compressed (i.e. what
/// is actually transferred); `chunk` counts completed chunks (0 at the start,
/// `chunks` when done).
#[derive(Debug, Clone, Copy)]
pub struct PullProgress {
    pub chunk: usize,
    pub chunks: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// How many of a part's chunks a push uploaded, and how many the registry
/// already held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadCount {
    pub uploaded: usize,
    pub reused: usize,
}

/// What a push uploaded: the base's chunks, then each variant's (§6.6). A
/// variant's base chunks are never counted — the base's own push already
/// put them there.
#[derive(Debug, Clone, Default)]
pub struct PushReport {
    pub base: UploadCount,
    pub variants: Vec<(String, UploadCount)>,
}

impl PushReport {
    /// One line per part, for the push's log.
    pub fn lines(&self) -> Vec<String> {
        let line = |what: &str, c: &UploadCount| {
            format!(
                "{what}: uploaded {} chunk(s), {} already in the registry",
                c.uploaded, c.reused
            )
        };
        std::iter::once(line("base", &self.base))
            .chain(
                self.variants
                    .iter()
                    .map(|(name, c)| line(&format!("variant {name}"), c)),
            )
            .collect()
    }
}

/// The tag a variant's manifest is published under: `<tag>--<variant>`
/// (§6.6). A version never contains `--`, and a variant name never starts
/// with `-`, so the split is unambiguous; [`is_variant_tag`] recognises one.
pub fn variant_tag(tag: &str, variant: &str) -> String {
    format!("{tag}--{variant}")
}

/// Whether a published tag is a variant's ([`variant_tag`]) rather than a
/// version or a moving alias — what a listing of versions leaves out.
pub fn is_variant_tag(tag: &str) -> bool {
    tag.contains("--")
}

/// The distribution-API surface push/pull is written against. Digests are
/// `sha256:<hex>` strings; `repository` is the repo path under the host.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// HEAD a blob: does `digest` already exist in `repository`?
    async fn blob_exists(&self, repository: &str, digest: &str) -> Result<bool>;
    /// GET a blob's bytes.
    async fn get_blob(&self, repository: &str, digest: &str) -> Result<Vec<u8>>;
    /// GET a blob into a file on disk, verifying its digest and reporting
    /// cumulative bytes to `progress`. Returns the byte count. The default
    /// implementation buffers through [`get_blob`](Self::get_blob);
    /// [`HttpTransport`] overrides it to stream, so multi-hundred-MiB
    /// container layers never land in memory.
    async fn get_blob_to_file(
        &self,
        repository: &str,
        digest: &str,
        dest: &Path,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<u64> {
        let bytes = self.get_blob(repository, digest).await?;
        let got = digest_of(&bytes);
        if !got.eq_ignore_ascii_case(digest) {
            bail!("blob {digest} digest mismatch: got {got} — download corrupt");
        }
        tokio::fs::write(dest, &bytes)
            .await
            .with_context(|| format!("cannot write {}", dest.display()))?;
        progress(bytes.len() as u64);
        Ok(bytes.len() as u64)
    }
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
    #[allow(dead_code)]
    pub fn with_transport(reference: Reference, transport: Box<dyn Transport>) -> Self {
        Self {
            reference,
            transport,
        }
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

    /// The concrete version a template tag carries and the architectures it
    /// publishes, read from the tag's index (or plain manifest) and the config
    /// blob of its first entry, with no chunk download. Every entry of an index
    /// carries the same version — a push merges each arch into its version
    /// tag's index and points a moving alias at that whole index — so this is
    /// how `latest` is resolved to the version it names (PRD §6.4).
    pub async fn tag_version(&self, tag: &str) -> Result<(String, Vec<String>)> {
        let repo = &self.reference.repository;
        let top = self
            .transport
            .get_manifest(repo, tag)
            .await?
            .ok_or_else(|| anyhow!("{}/{repo}:{tag} not found", self.reference.host))?;
        let (manifest, arches) = match parse_manifest_or_index(&top.body)? {
            ManifestOrIndex::Manifest(m) => (m, Vec::new()),
            ManifestOrIndex::Index(index) => {
                let arches = index
                    .manifests
                    .iter()
                    .filter_map(|d| d.platform.as_ref().map(|p| p.architecture.clone()))
                    .collect::<Vec<_>>();
                let first = index
                    .manifests
                    .first()
                    .ok_or_else(|| anyhow!("{repo}:{tag} is an empty index"))?;
                let fetched = self
                    .transport
                    .get_manifest(repo, &first.digest)
                    .await?
                    .ok_or_else(|| anyhow!("manifest {} missing from registry", first.digest))?;
                match parse_manifest_or_index(&fetched.body)? {
                    ManifestOrIndex::Manifest(m) => (m, arches),
                    ManifestOrIndex::Index(_) => {
                        bail!("index entry {} is itself an index", first.digest)
                    }
                }
            }
        };
        if !manifest.is_vmlab_template() {
            bail!("{repo}:{tag} is not a vmlab template");
        }
        let config_bytes = self
            .transport
            .get_blob(repo, &manifest.config.digest)
            .await
            .context("downloading config blob")?;
        let config = TemplateConfig::from_json(&config_bytes)?;
        let arches = if arches.is_empty() {
            vec![config.arch]
        } else {
            arches
        };
        Ok((config.version, arches))
    }

    /// The tag a pull of this reference fetches: its own tag resolved against
    /// the repository's published tags as a version pin, with the version
    /// `latest` names as the authority on which version is current
    /// ([`resolve_registry_pin`](crate::template::store::resolve_registry_pin)).
    /// A moving alias is used as it stands, and so is the tag itself when the
    /// tags cannot be listed or nothing matches.
    pub async fn pull_tag(&self) -> String {
        let pin = self.tag();
        if matches!(pin, "latest" | "latest-prerelease") {
            return pin.to_string();
        }
        let Ok(tags) = self.list_tags().await else {
            return pin.to_string();
        };
        let latest = if tags.iter().any(|t| t == "latest") {
            self.tag_version("latest").await.ok().map(|(v, _)| v)
        } else {
            None
        };
        crate::template::store::resolve_registry_pin(&tags, pin, latest.as_deref())
            .unwrap_or_else(|| pin.to_string())
    }

    /// OCI platform architectures for either a container image or a vmlab
    /// template. Unlike `index_arches`, a plain manifest reads the standard
    /// container config's `architecture` field rather than template metadata.
    pub async fn index_platform_arches(&self, tag: &str) -> Result<Vec<String>> {
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
                .filter_map(|d| {
                    d.platform.as_ref().and_then(|p| {
                        (p.os == "linux" && p.architecture != "unknown")
                            .then(|| p.architecture.clone())
                    })
                })
                .collect()),
            ManifestOrIndex::Manifest(m) => {
                let bytes = self.transport.get_blob(repo, &m.config.digest).await?;
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).context("parsing OCI image config")?;
                let arch = value["architecture"]
                    .as_str()
                    .ok_or_else(|| anyhow!("OCI image config has no architecture"))?;
                Ok(vec![arch.to_string()])
            }
        }
    }

    /// Push a template directory (containing `disk.qcow2` + `template.wcl`,
    /// and `variants/<name>/disk.qcow2` per variant) to the reference for the
    /// given `arch`. Chunks the disk, uploads each chunk blob (skipping any
    /// already present), uploads the config blob, PUTs the per-arch manifest,
    /// then merges that manifest into the tag's index so repeated pushes for
    /// different arches build a multi-arch index (PRD §6.4).
    ///
    /// Each variant goes up first, as its own manifest — the base's chunks,
    /// then the overlay's — tagged `<tag>--<variant>`; the registry already
    /// holds the base's chunks by then, so only the overlay's are uploaded.
    /// The base manifest then names every variant's manifest by digest, which
    /// is how a pull finds them (§6.6).
    pub async fn push(
        &self,
        template_dir: &Path,
        chunk_size: u64,
        arch: &str,
        work_dir: &Path,
        source: Option<&str>,
        moving_tag: Option<&str>,
    ) -> Result<PushReport> {
        let repo = self.reference.repository.clone();
        // The variants the store serves: a block whose overlay is missing
        // does not count (§6.6).
        let meta = crate::template::store::read_meta(template_dir).with_context(|| {
            format!(
                "cannot read template metadata in {}",
                template_dir.display()
            )
        })?;
        if let Some(stale) = meta.variants.iter().find(|v| meta.is_stale(v)) {
            bail!(
                "variant `{}` of {}/{}@{} is stale — it was built on an earlier base; rebuild it \
                 or remove it before pushing",
                stale.name,
                meta.arch,
                meta.name,
                meta.version
            );
        }
        let disk = template_dir.join(DISK_FILE);
        let mut report = PushReport::default();

        // 1 + 2. Chunk, compress and upload the base.
        let set = chunking::chunk_and_compress(&disk, chunk_size, &work_dir.join("chunks"))?;
        report.base = self.upload_chunks(&repo, &set).await?;

        // 3. The variants, each a manifest of its own.
        let mut config_meta = meta.clone();
        config_meta.arch = arch.to_string();
        let mut entries = Vec::new();
        for v in &meta.variants {
            let overlay = template_dir.join(crate::template::meta::variant_disk_rel(&v.name));
            let ensured = crate::template::store::qcow2_backing_file(&overlay)?;
            ensure!(
                ensured.as_deref() == Some(crate::template::store::VARIANT_BACKING),
                "variant `{}` is backed by {} rather than {} — not a store overlay",
                v.name,
                ensured.as_deref().unwrap_or("nothing"),
                crate::template::store::VARIANT_BACKING
            );
            let vset = chunking::chunk_and_compress(
                &overlay,
                chunk_size,
                &work_dir.join("variants").join(&v.name),
            )?;
            let uploaded = self.upload_chunks(&repo, &vset).await?;
            let config = self
                .put_config(&repo, &TemplateConfig::from_variant(&config_meta, v))
                .await?;
            let mut manifest = build_variant_manifest(&v.name, &set, &vset, config);
            if let Some(src) = source {
                manifest
                    .annotations
                    .get_or_insert_with(BTreeMap::new)
                    .insert(annotations::IMAGE_SOURCE.to_string(), src.to_string());
            }
            let tag = variant_tag(&self.reference.tag, &v.name);
            let digest = self
                .put_arch_manifest(&repo, arch, &manifest, &tag, None, source)
                .await
                .with_context(|| format!("pushing variant `{}`", v.name))?;
            tracing::info!(
                variant = %v.name,
                uploaded = uploaded.uploaded,
                reused = uploaded.reused,
                "pushed variant"
            );
            report.variants.push((v.name.clone(), uploaded));
            entries.push(VariantEntry {
                name: v.name.clone(),
                manifest: digest,
            });
        }

        // 4. The base's config blob and manifest, naming its variants.
        // Stamp the source-repo annotation so registries connect the package
        // to its repo (GHCR).
        let mut base_config = config_meta.clone();
        base_config.variants.clear();
        let config_desc = self
            .put_config(&repo, &TemplateConfig::from_meta(&base_config))
            .await?;
        let mut manifest = build_manifest(&set, config_desc);
        manifest.set_variants(&entries, meta.default_variant().map(|d| d.name.as_str()));
        if let Some(src) = source {
            manifest
                .annotations
                .get_or_insert_with(BTreeMap::new)
                .insert(annotations::IMAGE_SOURCE.to_string(), src.to_string());
        }
        // 5. Merge into the tag's index, and re-point a moving alias
        // (`latest` / `latest-prerelease`) at the same merged index — no blob
        // re-upload (PRD §6.4). Each arch push leaves the alias on the latest
        // complete index.
        self.put_arch_manifest(
            &repo,
            arch,
            &manifest,
            &self.reference.tag,
            moving_tag,
            source,
        )
        .await?;

        tracing::info!(
            reference = %self.reference.canonical(),
            arch,
            chunks = set.chunk_count,
            variants = entries.len(),
            "pushed template"
        );
        Ok(report)
    }

    /// Upload a chunk set's blobs, skipping each the registry already holds.
    async fn upload_chunks(&self, repo: &str, set: &chunking::ChunkSet) -> Result<UploadCount> {
        let mut count = UploadCount::default();
        for c in &set.chunks {
            if self
                .transport
                .blob_exists(repo, &c.compressed_digest)
                .await?
            {
                tracing::debug!(digest = %c.compressed_digest, "chunk already present, skipping");
                count.reused += 1;
                continue;
            }
            self.transport
                .put_blob_file(repo, &c.compressed_digest, &c.compressed_path)
                .await
                .with_context(|| format!("uploading chunk {}", c.index))?;
            count.uploaded += 1;
        }
        Ok(count)
    }

    /// Upload a config blob unless present, returning its descriptor.
    async fn put_config(&self, repo: &str, config: &TemplateConfig) -> Result<Descriptor> {
        let bytes = config.to_json()?;
        let digest = digest_of(&bytes);
        if !self.transport.blob_exists(repo, &digest).await? {
            self.transport
                .put_blob(repo, &digest, bytes.clone())
                .await
                .context("uploading config blob")?;
        }
        Ok(Descriptor::new(
            media_types::CONFIG_TEMPLATE_JSON,
            &digest,
            bytes.len() as u64,
        ))
    }

    /// PUT a per-arch manifest by its digest (so an index can reference it
    /// stably), merge it into `tag`'s index — creating it if absent, promoting
    /// a plain manifest tag into one — and point `moving_tag` at the same
    /// index. Returns the manifest's digest.
    async fn put_arch_manifest(
        &self,
        repo: &str,
        arch: &str,
        manifest: &Manifest,
        tag: &str,
        moving_tag: Option<&str>,
        source: Option<&str>,
    ) -> Result<String> {
        let manifest_bytes = serde_json::to_vec(manifest)?;
        let manifest_digest = digest_of(&manifest_bytes);
        self.transport
            .put_manifest(
                repo,
                &manifest_digest,
                media_types::OCI_MANIFEST,
                manifest_bytes.clone(),
            )
            .await
            .context("uploading manifest")?;

        let mut index = match self.transport.get_manifest(repo, tag).await? {
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
            .put_manifest(repo, tag, media_types::OCI_INDEX, index_bytes.clone())
            .await
            .with_context(|| format!("uploading the index for tag {tag}"))?;
        if let Some(alias) = moving_tag {
            self.transport
                .put_manifest(repo, alias, media_types::OCI_INDEX, index_bytes)
                .await
                .with_context(|| format!("updating moving tag {alias}"))?;
        }
        Ok(manifest_digest)
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

    /// [`Self::pull_with_progress`] of the default variant alone, unwatched.
    #[cfg(test)]
    pub async fn pull(
        &self,
        arch: Option<&str>,
        dest_store: &TemplateStore,
        work_dir: &Path,
        overwrite: bool,
    ) -> Result<TemplateMeta> {
        self.pull_with_progress(
            arch,
            dest_store,
            work_dir,
            overwrite,
            &VariantWant::default(),
            &mut |_| {},
        )
        .await
    }

    /// Pull the reference for `arch` into `dest_store`. Fetches the tag
    /// (resolving the index when present — `arch` required unless single),
    /// verifies the manifest is a vmlab artifact, downloads chunks in
    /// order, assembles, verifies the whole-image digest, then installs
    /// into the store recording the originating reference as the origin
    /// (PRD §6.4).
    ///
    /// `progress` is called once before the first chunk (`chunk == 0`) and
    /// after each chunk completes, carrying the compressed bytes downloaded
    /// so far out of the total — enough for a UI progress bar (PRD §6.4,
    /// issue #1). Byte totals are the compressed (downloaded) sizes.
    ///
    /// Variants (§6.6): the base manifest names each variant's manifest. A
    /// pull takes the default variant whatever `want` says — so a bare
    /// reference resolves in this store to the variant it resolves to in the
    /// registry — plus every variant `want` names. A base the store already
    /// holds is not downloaded again, nor is a variant it holds with the same
    /// digest; what arrives is merged into the version, keeping the variants
    /// already there.
    pub async fn pull_with_progress(
        &self,
        arch: Option<&str>,
        dest_store: &TemplateStore,
        work_dir: &Path,
        overwrite: bool,
        want: &VariantWant,
        progress: &mut (dyn FnMut(PullProgress) + Send),
    ) -> Result<TemplateMeta> {
        let repo = &self.reference.repository;

        // Steps 1-3: resolve the tag to the per-arch manifest + metadata
        // (no chunk download yet).
        let (manifest, meta) = self.resolve_manifest_meta(arch).await?;
        let whole = manifest
            .whole_digest()
            .ok_or_else(|| anyhow!("manifest is missing the whole-image digest annotation"))?
            .to_string();
        let reference = format!("{}/{}@{}", meta.arch, meta.name, meta.version);
        let (listed, default) = manifest.variants()?;
        let chosen = want.choose(&listed, default.as_deref(), &reference)?;

        // The same version already in the store: the same artefact is used
        // as it stands, and only a different one is refused. The store name
        // comes from the template's metadata, not the repository path, so a
        // template pushed from this store and referenced back by registry
        // lands here rather than in an offline cache hit.
        let local = if overwrite {
            None
        } else {
            dest_store.installed(&meta.arch, &meta.name, &meta.version)?
        };
        if let Some(local) = &local {
            let held = local.clone();
            let whole = whole.clone();
            let same = tokio::task::spawn_blocking(move || same_disk(&held, &whole))
                .await
                .map_err(|e| anyhow!("template digest task: {e}"))??;
            ensure!(
                same,
                "template {reference} is already in the store with a different disk than {} \
                 — remove it first, or pull with overwrite to replace it",
                self.reference.canonical()
            );
        }

        // The variants to fetch: each one's manifest and config blob, which
        // say what its overlay assembles to and the block it stands for.
        let mut parts: Vec<(crate::template::meta::VariantMeta, Vec<Descriptor>, String)> =
            Vec::new();
        for entry in chosen {
            let vm = self.fetch_manifest(repo, &entry.manifest).await?;
            if !vm.is_vmlab_template() {
                bail!(
                    "variant `{}`'s manifest is not a vmlab template",
                    entry.name
                );
            }
            let layout = vm.variant_layout()?;
            ensure!(
                layout.name == entry.name,
                "the manifest named for variant `{}` is variant `{}`'s",
                entry.name,
                layout.name
            );
            ensure!(
                digest_eq(&layout.base_digest, &whole),
                "variant `{}` was pushed on base {} but the tag's base is {whole} — the tag \
                 changed mid-pull or was pushed inconsistently; pull again",
                entry.name,
                layout.base_digest
            );
            if let Some(held) = local.as_ref().and_then(|l| l.meta.variant(&entry.name))
                && digest_eq(&held.sha256, &layout.overlay_digest)
            {
                continue;
            }
            let config_bytes = self
                .transport
                .get_blob(repo, &vm.config.digest)
                .await
                .with_context(|| format!("downloading variant `{}`'s config blob", entry.name))?;
            let variant = TemplateConfig::from_json(&config_bytes)?.into_variant(&meta)?;
            ensure!(
                variant.name == entry.name && digest_eq(&variant.sha256, &layout.overlay_digest),
                "variant `{}`'s config blob disagrees with its manifest",
                entry.name
            );
            parts.push((variant, layout.overlay, layout.overlay_digest));
        }
        if let Some(local) = &local
            && parts.is_empty()
        {
            return Ok(local.meta.clone());
        }

        // 4. Download chunks in order — the base's only when the store does
        // not already hold it — verifying each chunk's digest as it lands.
        let chunks_dir = work_dir.join("chunks");
        let base_layers: Vec<Descriptor> = if local.is_none() {
            manifest.layers_in_order().into_iter().cloned().collect()
        } else {
            Vec::new()
        };
        let mut download = Download::new(
            std::iter::once(&base_layers)
                .chain(parts.iter().map(|(_, layers, _)| layers))
                .flatten(),
            progress,
        );
        let base_paths = self
            .download_layers(repo, &base_layers, &chunks_dir.join("base"), &mut download)
            .await?;
        let mut overlays = Vec::new();
        for (variant, layers, digest) in parts {
            let dir = chunks_dir.join("variants").join(&variant.name);
            let paths = self
                .download_layers(repo, layers.as_slice(), &dir, &mut download)
                .await?;
            overlays.push((variant, paths, digest));
        }

        // 5 + 6. Decompress the chunks into the disk images, verify each
        // whole-image digest, and install into the store (a work_dir under
        // the store root keeps the install a plain rename; one elsewhere
        // costs a copy). Record the base digest if absent.
        //
        // All of this is multi-GB CPU and disk work — a Windows template is
        // tens of GB — so it runs on a blocking thread. Inline, it stalled the
        // caller's whole runtime: for a lab daemon that means the network
        // fabric, QMP and agent channels frozen for the length of the install.
        let staging = work_dir.join("staging");
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("cannot create {}", staging.display()))?;
        let store = dest_store.clone();
        let meta = tokio::task::spawn_blocking(move || -> Result<TemplateMeta> {
            let mut variants = Vec::new();
            for (mut variant, paths, digest) in overlays {
                let disk = staging.join(crate::template::meta::variant_disk_rel(&variant.name));
                chunking::assemble(&paths, &disk)?;
                chunking::verify_whole(&disk, &digest)
                    .with_context(|| format!("variant `{}`", variant.name))?;
                if crate::template::store::ensure_variant_backing(&disk)? {
                    variant.sha256 = crate::template::store::sha256_file(&disk)?;
                }
                variants.push(variant);
            }
            match local {
                Some(held) => store.add_variants(
                    &held.meta.arch,
                    &held.meta.name,
                    &held.meta.version,
                    &staging,
                    &variants,
                ),
                None => {
                    let disk = staging.join(DISK_FILE);
                    chunking::assemble(&base_paths, &disk)?;
                    chunking::verify_whole(&disk, &whole)?;
                    let mut meta = meta;
                    if meta.sha256.is_none() {
                        meta.sha256 = Some(
                            whole
                                .strip_prefix("sha256:")
                                .unwrap_or(&whole)
                                .to_ascii_lowercase(),
                        );
                    }
                    meta.variants = variants;
                    store.install_with_variants(&staging, &meta, overwrite)?;
                    Ok(meta)
                }
            }
        })
        .await
        .map_err(|e| anyhow!("template install task: {e}"))??;
        tracing::info!(
            reference = %self.reference.canonical(),
            arch = %meta.arch,
            variants = meta.variants.len(),
            "pulled template into store"
        );
        Ok(meta)
    }

    /// GET a manifest by digest, checking the body is the one the digest
    /// names.
    async fn fetch_manifest(&self, repo: &str, digest: &str) -> Result<Manifest> {
        let fetched = self
            .transport
            .get_manifest(repo, digest)
            .await?
            .ok_or_else(|| anyhow!("manifest {digest} missing from registry"))?;
        let got = digest_of(&fetched.body);
        ensure!(
            digest_eq(&got, digest),
            "manifest {digest} digest mismatch: got {got}"
        );
        match parse_manifest_or_index(&fetched.body)? {
            ManifestOrIndex::Manifest(m) => Ok(m),
            ManifestOrIndex::Index(_) => bail!("{digest} is an index, not a manifest"),
        }
    }

    /// Download `layers` in order into `dir`, verifying each compressed
    /// chunk's digest, and return the chunk files in that order.
    async fn download_layers(
        &self,
        repo: &str,
        layers: &[Descriptor],
        dir: &Path,
        download: &mut Download<'_>,
    ) -> Result<Vec<std::path::PathBuf>> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let mut paths = Vec::with_capacity(layers.len());
        for (i, layer) in layers.iter().enumerate() {
            let chunk = download.chunk;
            let bytes = self
                .transport
                .get_blob(repo, &layer.digest)
                .await
                .with_context(|| format!("downloading chunk {chunk}"))?;
            let got = digest_of(&bytes);
            if !got.eq_ignore_ascii_case(&layer.digest) {
                bail!(
                    "chunk {chunk} digest mismatch: expected {}, got {got}",
                    layer.digest
                );
            }
            let path = dir.join(format!("chunk-{i:04}.zst"));
            std::fs::write(&path, &bytes)
                .with_context(|| format!("cannot write {}", path.display()))?;
            paths.push(path);
            download.done(layer.size);
        }
        Ok(paths)
    }
}

/// Which variants a pull fetches beyond the default, which it always does
/// (§6.6): the names a `#<variant>` asked for, or every variant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantWant {
    pub names: std::collections::BTreeSet<String>,
    pub all: bool,
}

impl VariantWant {
    /// Every variant (`template pull --all-variants`).
    pub fn all() -> Self {
        Self {
            names: Default::default(),
            all: true,
        }
    }

    /// What a reference's `#<variant>` asks for: a named variant, or nothing
    /// beyond the default for no `#` and for `#base`.
    pub fn of(variant: Option<&crate::config::model::VariantRef>) -> Self {
        let mut want = Self::default();
        want.add(variant);
        want
    }

    /// Add what another reference's `#<variant>` asks for.
    pub fn add(&mut self, variant: Option<&crate::config::model::VariantRef>) {
        if let Some(crate::config::model::VariantRef::Named(n)) = variant {
            self.names.insert(n.clone());
        }
    }

    /// Whether a store version already holds everything this asks for, as
    /// far as can be told offline: every named variant installed. Every
    /// variant cannot be known without asking the registry.
    pub fn held_by(&self, meta: &TemplateMeta) -> bool {
        !self.all && self.names.iter().all(|n| meta.variant(n).is_some())
    }

    /// The variants of a base manifest's `listed` this pull fetches, in
    /// listed order: the default, and every one asked for. A name the
    /// manifest does not list is an error naming those it does.
    fn choose(
        &self,
        listed: &[VariantEntry],
        default: Option<&str>,
        reference: &str,
    ) -> Result<Vec<VariantEntry>> {
        if listed.is_empty() {
            if let Some(name) = self.names.iter().next() {
                bail!("template {reference} has no variants, so `#{name}` names nothing");
            }
            return Ok(Vec::new());
        }
        for name in &self.names {
            if !listed.iter().any(|e| &e.name == name) {
                let names: Vec<String> = listed
                    .iter()
                    .map(|e| {
                        if Some(e.name.as_str()) == default {
                            format!("{} (default)", e.name)
                        } else {
                            e.name.clone()
                        }
                    })
                    .collect();
                bail!(
                    "template {reference} has no variant `{name}` (its variants: {})",
                    names.join(", ")
                );
            }
        }
        Ok(listed
            .iter()
            .filter(|e| {
                self.all || Some(e.name.as_str()) == default || self.names.contains(&e.name)
            })
            .cloned()
            .collect())
    }
}

/// The running count behind a pull's progress reports, across every part it
/// downloads.
struct Download<'a> {
    chunk: usize,
    chunks: usize,
    bytes_done: u64,
    bytes_total: u64,
    progress: &'a mut (dyn FnMut(PullProgress) + Send),
}

impl<'a> Download<'a> {
    /// Start counting `layers`, reporting the 0-of-N update.
    fn new<'l>(
        layers: impl Iterator<Item = &'l Descriptor>,
        progress: &'a mut (dyn FnMut(PullProgress) + Send),
    ) -> Self {
        let (chunks, bytes_total) = layers.fold((0, 0), |(n, b), d| (n + 1, b + d.size));
        let mut d = Self {
            chunk: 0,
            chunks,
            bytes_done: 0,
            bytes_total,
            progress,
        };
        d.report();
        d
    }

    fn done(&mut self, size: u64) {
        self.chunk += 1;
        self.bytes_done += size;
        self.report();
    }

    fn report(&mut self) {
        (self.progress)(PullProgress {
            chunk: self.chunk,
            chunks: self.chunks,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
        });
    }
}

/// Two `sha256` digests, either maybe `sha256:`-prefixed, compared without
/// case.
fn digest_eq(a: &str, b: &str) -> bool {
    let strip = |d: &str| d.strip_prefix("sha256:").unwrap_or(d).to_ascii_lowercase();
    strip(a) == strip(b)
}

/// Whether a stored template's disk is the image a manifest's whole-image
/// digest names. The digest recorded at install answers without reading the
/// disk; an entry that never recorded one is hashed.
fn same_disk(local: &crate::template::store::TemplateVersion, whole: &str) -> Result<bool> {
    let want = whole
        .strip_prefix("sha256:")
        .unwrap_or(whole)
        .to_ascii_lowercase();
    let have = match &local.meta.sha256 {
        Some(d) => d.strip_prefix("sha256:").unwrap_or(d).to_ascii_lowercase(),
        None => crate::template::store::sha256_file(&local.disk_path)?,
    };
    Ok(have == want)
}

/// Offline-only resolution of a `registry` template reference: `Some` when
/// the referenced tag is already a store entry, `None` when a pull would be
/// needed. Zero network — this is the cheap "is it cached?" probe behind the
/// deferred pulls (labd build placeholders, status `template_cached`). A
/// moving alias (`latest`) never resolves here until its first pull records
/// a concrete version.
///
/// A store *version*: the reference's `#<variant>` is the caller's to apply
/// ([`crate::template::store::TemplateVersion::select`]), since machines with
/// different variants of one reference share its pull.
pub fn cached_registry_template(
    reference: &str,
    arch: &str,
    store: &TemplateStore,
) -> Result<Option<crate::template::store::TemplateVersion>> {
    let registry = Registry::new(reference)?;
    // Templates live at `host/owner/[group/]name:VERSION`; the store name is
    // the last repository path component.
    let store_name = registry
        .repository()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    Ok(store
        .resolve_version(arch, &store_name, Some(registry.tag()))
        .ok())
}

/// Resolve a `registry` template reference to a cached store entry, pulling it
/// (with progress) only when it isn't already present (PRD §6.4). This mirrors
/// the resolution a first `up` performs, factored out so every pull site
/// (labd's deferred `ensure_pulled`, streaming progress to the UI) shares one
/// implementation.
///
/// A concrete tag already in the store is used offline. A moving alias
/// (`latest` / `latest-prerelease`) or a build-counter prefix is resolved
/// against the registry's published tags, then to its real version, and the
/// disk is pulled only when that version is absent. `progress` fires during the
/// chunk download (never when the template is already cached).
pub async fn ensure_registry_template(
    reference: &str,
    arch: &str,
    store: &TemplateStore,
    want: &VariantWant,
    progress: &mut (dyn FnMut(PullProgress) + Send),
) -> Result<crate::template::store::TemplateVersion> {
    // Tag is a concrete, already-cached version holding every variant asked
    // for: stay offline.
    if let Some(r) = cached_registry_template(reference, arch, store)?
        && want.held_by(&r.meta)
    {
        return Ok(r);
    }
    let registry = Registry::new(reference)?;
    let store_name = registry
        .repository()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    // Not cached. Resolve the requested tag against the registry's published
    // tags first: a version pin (e.g. 26100.1742, or 24.04) resolves to the
    // version `latest` names when it falls under the pin, else to the newest
    // matching tag in `latest`'s scheme; a moving alias or exact tag is used
    // as-is. Then resolve that to its real version without downloading the
    // disk, and only pull when absent.
    let pull_tag = registry.pull_tag().await;
    let registry = if pull_tag.as_str() == registry.tag() {
        registry
    } else {
        Registry::new(&super::reference::with_version_tag(reference, &pull_tag)?)?
    };
    let real = registry
        .resolve_version(Some(arch))
        .await
        .with_context(|| format!("resolving {reference}"))?;
    if let Ok(r) = store.resolve_version(arch, &store_name, Some(&real))
        && want.held_by(&r.meta)
    {
        return Ok(r);
    }
    // Its own staging directory, on the store's filesystem: two labs pulling
    // at once must not share (and then delete) one.
    let work = crate::template::oci_bridge::fresh_work_dir(
        &crate::paths::template_store_dir().join(".oci-pull"),
    )?;
    let meta = registry
        .pull_with_progress(Some(arch), store, work.path(), false, want, progress)
        .await
        .with_context(|| format!("pulling {reference}"))?;
    store.resolve_version(&meta.arch, &meta.name, Some(&meta.version))
}

/// `sha256:<hex>` of a byte slice.
pub(crate) fn digest_of(bytes: &[u8]) -> String {
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

    async fn get_blob_to_file(
        &self,
        repository: &str,
        digest: &str,
        dest: &Path,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<u64> {
        use futures::StreamExt as _;
        use tokio::io::AsyncWriteExt as _;

        let url = self.url(&format!("/v2/{repository}/blobs/{digest}"));
        let scope = self.scope(repository, false);
        let resp = self
            .send_with_auth(&scope, || self.client.get(&url))
            .await?;
        if !resp.status().is_success() {
            bail!("GET blob {digest} returned {}", resp.status());
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("cannot create {}", dest.display()))?;
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("reading blob {digest} stream"))?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .with_context(|| format!("cannot write {}", dest.display()))?;
            done += chunk.len() as u64;
            progress(done);
        }
        file.flush()
            .await
            .with_context(|| format!("cannot flush {}", dest.display()))?;
        drop(file);
        let got = format!("sha256:{}", hex::encode(hasher.finalize()));
        if !got.eq_ignore_ascii_case(digest) {
            let _ = tokio::fs::remove_file(dest).await;
            bail!("blob {digest} digest mismatch: got {got} — download corrupt");
        }
        Ok(done)
    }

    async fn put_blob(&self, repository: &str, digest: &str, data: Vec<u8>) -> Result<()> {
        // An upload session outlives its usefulness: a chunk blob is up to
        // `oci_chunk_size` (512 MiB by default), and GHCR drops the session
        // under a PUT that takes minutes, answering 404 to a location it
        // handed out itself. Every large template failed that way on
        // 2026-09-05 while the small ones went straight up. A dropped session
        // is not a refusal, so start a new one and send the blob again.
        let mut last = String::new();
        for attempt in 1..=BLOB_PUT_ATTEMPTS {
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
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            last = status.to_string();
            if !session_lost(status) {
                break;
            }
            tracing::debug!(%digest, %status, attempt, "upload session lost; restarting it");
            backoff(attempt).await;
        }
        bail!("PUT blob {digest} returned {last}");
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

/// How many times a blob PUT restarts its upload session before giving up.
const BLOB_PUT_ATTEMPTS: u32 = 3;

/// Whether a failed blob PUT means the upload session went away rather than
/// the registry refusing the blob: 404 on a location the registry itself
/// handed out. Anything else (401 after the auth retry, 413, 400) is a
/// refusal that a new session cannot change, and 5xx is already retried
/// against the same session by `send_with_auth`.
fn session_lost(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND
}

async fn backoff(attempt: u32) {
    let ms = 100u64 * 2u64.pow(attempt.saturating_sub(1));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::chunking;
    use crate::template::META_FILE;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory fake registry: blobs keyed by digest, manifests keyed
    /// by reference (tag or digest), all per-repository.
    #[derive(Default)]
    struct FakeRegistry {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        manifests: Mutex<HashMap<String, (String, Vec<u8>)>>,
        /// Every blob digest PUT, in order — what a push uploaded.
        puts: Mutex<Vec<String>>,
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
            self.puts.lock().unwrap().push(digest.to_string());
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
            cpu_model: None,
            accel: None,
            created: "2026-06-12T00:00:00Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            variants: Vec::new(),
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

    /// Push a tiny `arch` build of `name@version` to `repo`, moving `alias`.
    async fn push_version(
        fake: &std::sync::Arc<FakeRegistry>,
        repo: &str,
        arch: &str,
        version: &str,
        alias: Option<&str>,
    ) {
        let work = tempfile::tempdir().unwrap();
        let tdir = work.path().join("tmpl");
        let mut m = meta(arch);
        m.version = version.into();
        make_template(&tdir, &m, b"disk");
        registry_with_fake(&format!("{repo}:{version}"), fake.clone())
            .push(&tdir, 1024, arch, &work.path().join("push"), None, alias)
            .await
            .unwrap();
    }

    /// The ubuntu-24.04 shape: date-scheme tags from a retired per-arch
    /// scheme sorting above the multi-arch `<declared>.<N>` that `latest` names.
    async fn stale_scheme_repo() -> std::sync::Arc<FakeRegistry> {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let repo = "ghcr.io/owner/ubuntu";
        for v in ["24.04.20260518", "24.04.20260519", "24.04.20260520"] {
            push_version(&fake, repo, "aarch64", v, Some("latest")).await;
        }
        push_version(&fake, repo, "riscv64", "24.04.20260520", Some("latest")).await;
        for arch in ["x86_64", "aarch64", "riscv64"] {
            push_version(&fake, repo, arch, "24.04.4.2", Some("latest")).await;
        }
        push_version(
            &fake,
            repo,
            "x86_64",
            "24.04.4.3",
            Some("latest-prerelease"),
        )
        .await;
        fake
    }

    #[tokio::test]
    async fn tag_version_names_latests_version_and_arches() {
        let fake = stale_scheme_repo().await;
        let reg = registry_with_fake("ghcr.io/owner/ubuntu", fake);
        let (version, mut arches) = reg.tag_version("latest").await.unwrap();
        arches.sort();
        assert_eq!(version, "24.04.4.2");
        assert_eq!(arches, ["aarch64", "riscv64", "x86_64"]);
        // A concrete tag reads the same way.
        let (version, arches) = reg.tag_version("24.04.20260520").await.unwrap();
        assert_eq!(version, "24.04.20260520");
        assert_eq!(arches, ["aarch64", "riscv64"]);
    }

    #[tokio::test]
    async fn pull_tag_follows_latest_over_higher_stale_tags() {
        let fake = stale_scheme_repo().await;
        let pull_tag = |r: &str| {
            let reg = registry_with_fake(&format!("ghcr.io/owner/ubuntu:{r}"), fake.clone());
            async move { reg.pull_tag().await }
        };
        assert_eq!(pull_tag("24.04").await, "24.04.4.2");
        assert_eq!(pull_tag("24.04.4").await, "24.04.4.2");
        assert_eq!(pull_tag("24.04.4.3").await, "24.04.4.3");
        assert_eq!(pull_tag("24.04.20260519").await, "24.04.20260519");
        assert_eq!(pull_tag("latest").await, "latest");
        assert_eq!(pull_tag("latest-prerelease").await, "latest-prerelease");
        // Nothing matches: the tag as written, for the pull to report.
        assert_eq!(pull_tag("22.04").await, "22.04");
    }

    #[tokio::test]
    async fn pull_tag_without_latest_keeps_the_counter_rule() {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let repo = "ghcr.io/owner/win";
        for v in ["26100.1742.0", "26100.1742.3", "26100.1742.10"] {
            push_version(&fake, repo, "x86_64", v, None).await;
        }
        let reg = registry_with_fake(&format!("{repo}:26100.1742"), fake);
        assert_eq!(reg.pull_tag().await, "26100.1742.10");
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
        let resolved = store
            .resolve_version("x86_64", "alpine", Some("3.20"))
            .unwrap();
        assert_eq!(std::fs::read(&resolved.disk_path).unwrap(), disk);
    }

    /// Pulling a version the store already holds uses the stored copy when it
    /// is the same image, without downloading, and refuses only when the two
    /// disks differ.
    #[tokio::test]
    async fn pull_of_a_version_already_in_the_store() {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        let disk: Vec<u8> = (0..(3u32 * 1024 * 1024)).map(|i| (i % 241) as u8).collect();
        let tdir = work.path().join("tmpl");
        let mut m = meta("x86_64");
        m.sha256 = Some(hex::encode(Sha256::digest(&disk)));
        make_template(&tdir, &m, &disk);
        // Pushed under a repository whose last component is not the
        // template's name, as a lab's `template = "localhost:5000/x/y:3.20"`.
        let reference = "localhost:5000/e2e/other-name:3.20";
        registry_with_fake(reference, fake.clone())
            .push(
                &tdir,
                1024 * 1024,
                "x86_64",
                &work.path().join("push"),
                None,
                None,
            )
            .await
            .unwrap();

        let store_root = work.path().join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let store = TemplateStore::new(store_root.clone());
        let staged = store_root.join("stage");
        make_template(&staged, &m, &disk);
        store.install(&staged, &m, false).unwrap();

        let mut downloads = 0;
        let pulled = registry_with_fake(reference, fake.clone())
            .pull_with_progress(
                Some("x86_64"),
                &store,
                &store_root.join(".oci-pull"),
                false,
                &VariantWant::default(),
                &mut |_| downloads += 1,
            )
            .await
            .expect("the same image is used as it stands");
        assert_eq!(
            (pulled.name.as_str(), pulled.version.as_str()),
            ("alpine", "3.20")
        );
        assert_eq!(downloads, 0, "nothing downloaded for an image already held");

        // A different disk under the same name and version is refused.
        store
            .remove("x86_64", "alpine", "3.20", None, true, &|_| None)
            .unwrap();
        let mut other = m.clone();
        other.sha256 = Some("00".repeat(32));
        make_template(&staged, &other, b"not the pushed disk");
        store.install(&staged, &other, false).unwrap();
        let err = registry_with_fake(reference, fake.clone())
            .pull(Some("x86_64"), &store, &store_root.join(".oci-pull"), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("different disk"), "{err}");
    }

    #[tokio::test]
    async fn pull_reports_progress() {
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();

        // ~5 MiB disk pushed in 1 MiB chunks → several layers to report on.
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
            None,
            Some("latest"),
        )
        .await
        .unwrap();

        let store_root = work.path().join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let store = TemplateStore::new(store_root.clone());
        let reg2 = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone());
        let mut updates: Vec<PullProgress> = Vec::new();
        reg2.pull_with_progress(
            Some("x86_64"),
            &store,
            &store_root.join(".oci-pull"),
            false,
            &VariantWant::default(),
            &mut |p| updates.push(p),
        )
        .await
        .unwrap();

        // An initial 0-of-N report, then one per chunk.
        let first = updates.first().copied().expect("at least one update");
        let last = updates.last().copied().unwrap();
        assert_eq!(first.chunk, 0);
        assert_eq!(first.bytes_done, 0);
        assert!(last.chunks > 1, "5 MiB / 1 MiB should be several chunks");
        assert_eq!(
            updates.len(),
            last.chunks + 1,
            "0-report plus one per chunk"
        );
        assert_eq!(last.chunk, last.chunks, "final report is complete");
        assert_eq!(last.bytes_done, last.bytes_total);
        assert!(last.bytes_total > 0);
        // Monotonic non-decreasing byte counts, consistent chunk total.
        for w in updates.windows(2) {
            assert!(w[1].bytes_done >= w[0].bytes_done);
            assert_eq!(w[1].chunks, w[0].chunks);
        }
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
                    .resolve_version("x86_64", "alpine", Some("3.20"))
                    .unwrap()
                    .disk_path
            )
            .unwrap(),
            disk_x
        );
        assert_eq!(
            std::fs::read(
                &store
                    .resolve_version("aarch64", "alpine", Some("3.20"))
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

    #[test]
    fn cached_registry_template_hit_and_miss() {
        // Purely offline: no transport exists to reach, so a network attempt
        // would fail loudly rather than pass.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::template::TemplateStore::new(dir.path().join("templates"));

        // Miss: empty store.
        let miss =
            cached_registry_template("ghcr.io/acme/ubuntu-2404:1.0", "x86_64", &store).unwrap();
        assert!(miss.is_none());

        // Install a matching entry, then the same reference resolves offline.
        let meta = crate::template::meta::TemplateMeta {
            name: "ubuntu-2404".into(),
            arch: "x86_64".into(),
            version: "1.0".into(),
            profile: Some("linux".into()),
            cpus: Some(2),
            memory: Some(4 << 30),
            disk: Some(20 << 30),
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            cpu_model: None,
            accel: None,
            created: "2026-06-12T00:00:00Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            variants: Vec::new(),
        };
        std::fs::create_dir_all(store.root()).unwrap();
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        std::fs::write(
            staging.path().join(crate::template::store::DISK_FILE),
            b"qcow2",
        )
        .unwrap();
        store.install(&staging.keep(), &meta, false).unwrap();

        let hit = cached_registry_template("ghcr.io/acme/ubuntu-2404:1.0", "x86_64", &store)
            .unwrap()
            .expect("installed version should resolve offline");
        assert_eq!(hit.meta.version, "1.0");

        // A different tag (or arch) still misses — moving aliases stay
        // "not cached" until their first pull records a concrete version.
        assert!(
            cached_registry_template("ghcr.io/acme/ubuntu-2404:2.0", "x86_64", &store)
                .unwrap()
                .is_none()
        );
        assert!(
            cached_registry_template("ghcr.io/acme/ubuntu-2404:1.0", "aarch64", &store)
                .unwrap()
                .is_none()
        );
    }

    /// A registry that loses the first upload session it hands out, the way
    /// GHCR loses one under a slow PUT: 404 on a location it issued itself.
    /// The push has to start a new session rather than report the blob
    /// refused (every large template failed that way on 2026-09-05).
    #[tokio::test]
    async fn a_lost_upload_session_is_started_again() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sessions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = sessions.clone();

        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        let head = String::from_utf8_lossy(&buf[..n]).to_string();
                        let reply = if head.starts_with("POST") {
                            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            format!(
                                "HTTP/1.1 202 Accepted\r\nLocation: /upload/{n}\r\nContent-Length: 0\r\n\r\n"
                            )
                        } else if head.starts_with("PUT /upload/1") {
                            // The session the registry itself handed out is gone.
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                        } else {
                            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n".to_string()
                        };
                        if sock.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let transport = HttpTransport::new(addr.to_string(), Credential::Anonymous).unwrap();
        let data = b"blob bytes".to_vec();
        transport
            .put_blob("acme/ubuntu", &digest_of(&data), data)
            .await
            .expect("a lost session is not a refusal");
        assert_eq!(
            sessions.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the second attempt must open its own upload session"
        );
    }

    /// A refusal the registry means is reported, not retried into silence.
    #[tokio::test]
    async fn a_refused_blob_is_not_retried() {
        assert!(session_lost(reqwest::StatusCode::NOT_FOUND));
        assert!(!session_lost(reqwest::StatusCode::PAYLOAD_TOO_LARGE));
        assert!(!session_lost(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!session_lost(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
    }

    // ---- variants (§6.6) ----------------------------------------------------

    use crate::template::meta::{VariantMeta, variant_disk_rel};
    use crate::template::store::sha256_file;

    fn have_qemu_tools() -> bool {
        let found = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join("qemu-img").is_file() && dir.join("qemu-io").is_file())
        });
        if !found {
            eprintln!("skipping: qemu-img/qemu-io not found on PATH");
        }
        found
    }

    fn run(dir: &Path, tool: &str, args: &[&str]) {
        let out = std::process::Command::new(tool)
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{tool} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn variant(name: &str, default: bool, sha256: String, base_sha256: &str) -> VariantMeta {
        VariantMeta {
            name: name.into(),
            default,
            created: "2026-07-01T00:00:00Z".parse().unwrap(),
            sha256,
            base_sha256: base_sha256.into(),
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
            display: None,
            cpu_model: None,
            accel: None,
        }
    }

    /// A template directory laid out as the store lays a version out: a real
    /// qcow2 base, and the overlays `standard` (default) and `extra`
    /// (`memory` and `first_boot_script` overridden, described), each
    /// backed on `../../disk.qcow2` and each holding data of its own.
    fn variant_template(dir: &Path) -> TemplateMeta {
        std::fs::create_dir_all(dir).unwrap();
        run(
            dir,
            "qemu-img",
            &["create", "-q", "-f", "qcow2", DISK_FILE, "4M"],
        );
        run(
            dir,
            "qemu-io",
            &["-c", "write -q -P 0xaa 0 256k", DISK_FILE],
        );
        let base_sha = sha256_file(&dir.join(DISK_FILE)).unwrap();
        let mut m = meta("x86_64");
        m.sha256 = Some(base_sha.clone());
        m.first_boot_script = Some("base script".into());
        for (name, pattern) in [("standard", "0x11"), ("extra", "0x22")] {
            let vdir = dir.join("variants").join(name);
            std::fs::create_dir_all(&vdir).unwrap();
            run(
                &vdir,
                "qemu-img",
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
            run(
                &vdir,
                "qemu-io",
                &["-c", &format!("write -q -P {pattern} 1M 128k"), DISK_FILE],
            );
            let sha = sha256_file(&vdir.join(DISK_FILE)).unwrap();
            let mut v = variant(name, name == "standard", sha, &base_sha);
            if name == "extra" {
                v.memory = Some(8 << 30);
                v.first_boot_script = Some("extra script".into());
                v.description = Some("more memory".into());
            }
            m.variants.push(v);
        }
        m.write_to(&dir.join(META_FILE)).unwrap();
        m
    }

    const SMALL_CHUNKS: u64 = 64 * 1024;

    /// Push `variant_template` to `ghcr.io/owner/alpine:3.20`, moving
    /// `latest`.
    async fn push_variants(
        fake: &std::sync::Arc<FakeRegistry>,
        work: &Path,
    ) -> (TemplateMeta, PushReport) {
        let tdir = work.join("tmpl");
        let m = variant_template(&tdir);
        let report = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .push(
                &tdir,
                SMALL_CHUNKS,
                "x86_64",
                &work.join("push"),
                None,
                Some("latest"),
            )
            .await
            .unwrap();
        (m, report)
    }

    async fn arch_manifest(fake: &FakeRegistry, tag: &str) -> Manifest {
        let index = fake
            .get_manifest("owner/alpine", tag)
            .await
            .unwrap()
            .unwrap();
        let ManifestOrIndex::Index(index) = parse_manifest_or_index(&index.body).unwrap() else {
            panic!("{tag} is not an index");
        };
        let desc = index.resolve(Some("x86_64")).unwrap();
        let body = fake
            .get_manifest("owner/alpine", &desc.digest)
            .await
            .unwrap()
            .unwrap();
        let ManifestOrIndex::Manifest(m) = parse_manifest_or_index(&body.body).unwrap() else {
            panic!("index entry is not a manifest");
        };
        m
    }

    async fn config_json(fake: &FakeRegistry, m: &Manifest) -> serde_json::Value {
        serde_json::from_slice(
            &fake
                .get_blob("owner/alpine", &m.config.digest)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    /// The layout a push gives a variant: its own manifest under
    /// `<version>--<variant>`, the base's layers first and in order, then the
    /// overlay's; annotations saying where the base ends and what each part
    /// assembles to; a config blob of effective metadata naming the variant
    /// and its base. The base manifest names each variant's manifest. No blob
    /// goes up twice, so a variant uploads only its overlay's chunks.
    #[tokio::test]
    async fn a_variant_pushes_as_base_layers_then_its_overlay() {
        if !have_qemu_tools() {
            return;
        }
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        let (m, report) = push_variants(&fake, work.path()).await;

        let base = arch_manifest(&fake, "3.20").await;
        let (entries, default) = base.variants().unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["standard", "extra"]
        );
        assert_eq!(default.as_deref(), Some("standard"));
        assert_eq!(
            base.whole_digest().unwrap(),
            format!("sha256:{}", m.sha256.as_deref().unwrap())
        );
        // `latest` names the same base, so `latest#extra` finds the variant.
        assert_eq!(arch_manifest(&fake, "latest").await, base);
        // The base's config blob is the base's alone.
        let base_config = config_json(&fake, &base).await;
        assert!(base_config.get("variant").is_none(), "{base_config}");

        for entry in &entries {
            let tagged = arch_manifest(&fake, &variant_tag("3.20", &entry.name)).await;
            assert_eq!(
                digest_of(&serde_json::to_vec(&tagged).unwrap()),
                entry.manifest
            );
            assert!(tagged.is_vmlab_template());
            assert_eq!(
                tagged.whole_digest(),
                None,
                "no whole digest for an old client to trust"
            );

            let layout = tagged.variant_layout().unwrap();
            assert_eq!(layout.name, entry.name);
            let base_layers: Vec<&str> = base
                .layers_in_order()
                .iter()
                .map(|d| d.digest.as_str())
                .collect();
            assert_eq!(
                layout
                    .base
                    .iter()
                    .map(|d| d.digest.as_str())
                    .collect::<Vec<_>>(),
                base_layers,
                "the base's layers, first and in order"
            );
            assert_eq!(layout.base_digest, base.whole_digest().unwrap());
            let v = m.variant(&entry.name).unwrap();
            assert_eq!(layout.overlay_digest, format!("sha256:{}", v.sha256));
            let ann = tagged.annotations.as_ref().unwrap();
            assert_eq!(ann[annotations::BASE_LAYERS], base_layers.len().to_string());
            assert_eq!(
                ann[annotations::VARIANT_CHUNK_COUNT],
                layout.overlay.len().to_string()
            );
            assert_eq!(
                ann[annotations::CHUNK_COUNT],
                (base_layers.len() + layout.overlay.len()).to_string()
            );
            // Running chunk indexes, the part marked on each layer.
            for (i, d) in tagged.layers.iter().enumerate() {
                let a = d.annotations.as_ref().unwrap();
                assert_eq!(a[annotations::CHUNK_INDEX], i.to_string());
                let part = if i < base_layers.len() {
                    annotations::PART_BASE
                } else {
                    annotations::PART_VARIANT
                };
                assert_eq!(a[annotations::PART], part);
            }

            // The config blob: effective metadata, plus name and base.
            let config = config_json(&fake, &tagged).await;
            assert_eq!(config["variant"]["name"], entry.name.as_str());
            assert_eq!(
                config["variant"]["base_sha256"],
                m.sha256.as_deref().unwrap()
            );
            assert_eq!(config["variant"]["default"], entry.name == "standard");
            assert_eq!(config["sha256"], v.sha256.as_str());
            let effective = m.effective(Some(v));
            assert_eq!(config["memory"], serde_json::json!(effective.memory));
            assert_eq!(
                config["first_boot_script"],
                serde_json::json!(effective.first_boot_script)
            );
        }

        // Every blob went up once; a variant's uploads are its overlay's.
        let puts = fake.puts.lock().unwrap().clone();
        let mut unique = puts.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(puts.len(), unique.len(), "a blob was uploaded twice");
        assert_eq!(report.base.uploaded + report.base.reused, base.layers.len());
        for (name, count) in &report.variants {
            let layout = arch_manifest(&fake, &variant_tag("3.20", name))
                .await
                .variant_layout()
                .unwrap();
            assert_eq!(count.uploaded + count.reused, layout.overlay.len());
            assert!(count.uploaded > 0);
            for d in &layout.base {
                let at = puts.iter().position(|p| p == &d.digest).unwrap();
                assert!(
                    at < report.base.uploaded,
                    "base chunk {} uploaded outside the base's push",
                    d.digest
                );
            }
        }

        // A re-push uploads nothing.
        let again = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .push(
                &work.path().join("tmpl"),
                SMALL_CHUNKS,
                "x86_64",
                &work.path().join("push2"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(again.base.uploaded, 0);
        assert!(again.variants.iter().all(|(_, c)| c.uploaded == 0));
    }

    /// A stale variant is not published.
    #[tokio::test]
    async fn a_stale_variant_is_not_pushed() {
        if !have_qemu_tools() {
            return;
        }
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        let tdir = work.path().join("tmpl");
        let mut m = variant_template(&tdir);
        m.variants[1].base_sha256 = "00".repeat(32);
        m.write_to(&tdir.join(META_FILE)).unwrap();
        let err = registry_with_fake("ghcr.io/owner/alpine:3.20", fake.clone())
            .push(
                &tdir,
                SMALL_CHUNKS,
                "x86_64",
                &work.path().join("p"),
                None,
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`extra`") && err.contains("stale"), "{err}");
    }

    fn store_in(work: &Path, name: &str) -> TemplateStore {
        let root = work.join(name);
        std::fs::create_dir_all(&root).unwrap();
        TemplateStore::new(root)
    }

    async fn pull_want(
        fake: &std::sync::Arc<FakeRegistry>,
        reference: &str,
        store: &TemplateStore,
        want: &VariantWant,
    ) -> Result<(TemplateMeta, usize)> {
        let mut chunks = 0;
        let meta = registry_with_fake(reference, fake.clone())
            .pull_with_progress(
                Some("x86_64"),
                store,
                &store
                    .root()
                    .join(".oci-pull")
                    .join(rand::random::<u32>().to_string()),
                false,
                want,
                &mut |p| chunks = p.chunks,
            )
            .await?;
        Ok((meta, chunks))
    }

    /// What a pull put in the store: the version's variant names, and
    /// whether each disk is byte-for-byte the pushed one, its overlay backed
    /// on `../../disk.qcow2`.
    fn assert_pulled(store: &TemplateStore, pushed: &Path, want: &[&str]) -> TemplateMeta {
        let v = store
            .resolve_version("x86_64", "alpine", Some("3.20"))
            .unwrap();
        assert_eq!(
            v.meta
                .variants
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>(),
            want
        );
        assert_eq!(
            std::fs::read(&v.disk_path).unwrap(),
            std::fs::read(pushed.join(DISK_FILE)).unwrap()
        );
        for name in want {
            let disk = v.variant_disk(name);
            assert_eq!(
                std::fs::read(&disk).unwrap(),
                std::fs::read(pushed.join(variant_disk_rel(name))).unwrap(),
                "variant {name}"
            );
            assert_eq!(
                crate::template::store::qcow2_backing_file(&disk)
                    .unwrap()
                    .as_deref(),
                Some("../../disk.qcow2")
            );
        }
        v.meta
    }

    /// `#<variant>` brings the base, the default and the variant named; the
    /// pulled blocks resolve to the effective metadata the pushed ones did.
    /// A bare reference brings the default; `--all-variants` every one. A
    /// name the template does not have is refused, naming those it does.
    #[tokio::test]
    async fn pull_a_variant_into_an_empty_store() {
        if !have_qemu_tools() {
            return;
        }
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        let (m, _) = push_variants(&fake, work.path()).await;
        let pushed = work.path().join("tmpl");
        let reference = "ghcr.io/owner/alpine:3.20";

        let store = store_in(work.path(), "named");
        pull_want(
            &fake,
            reference,
            &store,
            &VariantWant::of(Some(&VariantRefNamed("extra"))),
        )
        .await
        .unwrap();
        let pulled = assert_pulled(&store, &pushed, &["standard", "extra"]);
        for v in &m.variants {
            let got = pulled.variant(&v.name).unwrap();
            assert_eq!(pulled.effective(Some(got)), {
                let mut e = m.effective(Some(v));
                e.origin = pulled.origin.clone();
                e
            });
            assert_eq!(got.description, v.description);
            assert_eq!(got.default, v.default);
        }
        let r = store
            .resolve(
                "x86_64",
                "alpine",
                Some("3.20"),
                Some(&VariantRefNamed("extra")),
                Purpose::Lab,
            )
            .unwrap();
        assert_eq!(r.meta.memory, Some(8 << 30));

        let bare = store_in(work.path(), "bare");
        pull_want(&fake, reference, &bare, &VariantWant::default())
            .await
            .unwrap();
        assert_pulled(&bare, &pushed, &["standard"]);
        let r = bare
            .resolve("x86_64", "alpine", Some("3.20"), None, Purpose::Lab)
            .unwrap();
        assert_eq!(r.variant.as_deref(), Some("standard"));

        // `latest` resolves the same way.
        let all = store_in(work.path(), "all");
        pull_want(
            &fake,
            "ghcr.io/owner/alpine:latest",
            &all,
            &VariantWant::all(),
        )
        .await
        .unwrap();
        assert_pulled(&all, &pushed, &["standard", "extra"]);

        let none = store_in(work.path(), "unknown");
        let err = pull_want(
            &fake,
            reference,
            &none,
            &VariantWant::of(Some(&VariantRefNamed("nope"))),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("no variant `nope`") && err.contains("standard (default), extra"),
            "{err}"
        );
        assert!(none.list().unwrap().is_empty());
    }

    #[allow(non_snake_case)]
    fn VariantRefNamed(n: &str) -> crate::config::model::VariantRef {
        crate::config::model::VariantRef::Named(n.into())
    }

    use crate::template::store::Purpose;

    /// A variant pulled onto a version the store holds merges into its
    /// `template.wcl`, keeping the variant already there, and downloads only
    /// its overlay — never the base again.
    #[tokio::test]
    async fn a_pulled_variant_merges_into_the_held_version() {
        if !have_qemu_tools() {
            return;
        }
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        push_variants(&fake, work.path()).await;
        let pushed = work.path().join("tmpl");
        let reference = "ghcr.io/owner/alpine:3.20";
        let store = store_in(work.path(), "store");

        pull_want(&fake, reference, &store, &VariantWant::default())
            .await
            .unwrap();
        assert_pulled(&store, &pushed, &["standard"]);
        let before = std::fs::metadata(
            store
                .resolve_version("x86_64", "alpine", None)
                .unwrap()
                .disk_path,
        )
        .unwrap()
        .modified()
        .unwrap();

        let (_, chunks) = pull_want(
            &fake,
            reference,
            &store,
            &VariantWant::of(Some(&VariantRefNamed("extra"))),
        )
        .await
        .unwrap();
        let extra = arch_manifest(&fake, &variant_tag("3.20", "extra"))
            .await
            .variant_layout()
            .unwrap();
        assert_eq!(
            chunks,
            extra.overlay.len(),
            "only the overlay's chunks downloaded"
        );
        let merged = assert_pulled(&store, &pushed, &["standard", "extra"]);
        assert_eq!(merged.default_variant().unwrap().name, "standard");
        assert_eq!(
            std::fs::metadata(
                store
                    .resolve_version("x86_64", "alpine", None)
                    .unwrap()
                    .disk_path
            )
            .unwrap()
            .modified()
            .unwrap(),
            before,
            "the base was left alone"
        );

        // Everything held: nothing downloads.
        let (_, chunks) = pull_want(&fake, reference, &store, &VariantWant::all())
            .await
            .unwrap();
        assert_eq!(chunks, 0);
    }

    /// A corrupt overlay chunk fails the pull on its digest, and nothing of
    /// the version is installed.
    #[tokio::test]
    async fn a_corrupt_variant_chunk_fails_the_pull() {
        if !have_qemu_tools() {
            return;
        }
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        push_variants(&fake, work.path()).await;
        let extra = arch_manifest(&fake, &variant_tag("3.20", "extra"))
            .await
            .variant_layout()
            .unwrap();
        let target = key("owner/alpine", &extra.overlay[0].digest);
        fake.blobs
            .lock()
            .unwrap()
            .get_mut(&target)
            .unwrap()
            .push(0xFF);

        let store = store_in(work.path(), "store");
        let err = pull_want(
            &fake,
            "ghcr.io/owner/alpine:3.20",
            &store,
            &VariantWant::of(Some(&VariantRefNamed("extra"))),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("mismatch"), "{err}");
        assert!(store.list().unwrap().is_empty(), "nothing installed");
    }

    /// What a client that predates variants sees at the version tag: the
    /// base manifest, whose layers are the base's chunks alone and assemble
    /// to the base's digest, and a config blob it reads as before. The
    /// variant annotations are simply more annotations.
    #[tokio::test]
    async fn an_old_client_pulls_the_base_alone() {
        if !have_qemu_tools() {
            return;
        }
        let fake = std::sync::Arc::new(FakeRegistry::default());
        let work = tempfile::tempdir().unwrap();
        let (m, _) = push_variants(&fake, work.path()).await;
        let base = arch_manifest(&fake, "3.20").await;
        // The old algorithm: every layer in order, assembled, checked whole.
        let dir = work.path().join("old");
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for (i, d) in base.layers_in_order().iter().enumerate() {
            let p = dir.join(format!("{i}.zst"));
            std::fs::write(&p, fake.get_blob("owner/alpine", &d.digest).await.unwrap()).unwrap();
            paths.push(p);
        }
        chunking::assemble(&paths, &dir.join(DISK_FILE)).unwrap();
        chunking::verify_whole(&dir.join(DISK_FILE), base.whole_digest().unwrap()).unwrap();
        assert_eq!(
            std::fs::read(dir.join(DISK_FILE)).unwrap(),
            std::fs::read(work.path().join("tmpl").join(DISK_FILE)).unwrap()
        );
        // Its config blob, read as an old client's would.
        let mut config = config_json(&fake, &base).await;
        config
            .as_object_mut()
            .unwrap()
            .retain(|k, _| k != "variant");
        let old: TemplateConfig = serde_json::from_value(config).unwrap();
        let old = old.into_meta(None).unwrap();
        assert!(old.variants.is_empty());
        assert_eq!(old.sha256, m.sha256);

        // And a variant's tag holds nothing such a client would install: no
        // whole-image digest, which it refuses.
        assert!(
            arch_manifest(&fake, &variant_tag("3.20", "extra"))
                .await
                .whole_digest()
                .is_none()
        );
    }

    #[test]
    fn variant_tags_are_told_from_versions() {
        assert_eq!(variant_tag("3.23.10", "extra"), "3.23.10--extra");
        assert!(is_variant_tag("3.23.10--extra"));
        assert!(!is_variant_tag("3.23.10"));
        assert!(!is_variant_tag("latest"));
        assert!(!is_variant_tag("1.2-rc1"));
    }
}
