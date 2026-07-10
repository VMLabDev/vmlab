//! Resolving and pulling container images into the [`ImageCache`].
//!
//! [`ensure_container_image`] mirrors the template path's
//! [`crate::oci::client::ensure_registry_template`]: a digest selector (or a
//! tag whose cached resolution is still installed) is satisfied fully
//! offline; a tag is otherwise re-resolved against the registry, falling
//! back to the cached resolution — with a warning — when the registry is
//! unreachable. A fresh pull resolves the platform manifest (multi-arch
//! index aware), verifies every blob digest as it streams, flattens the
//! layers to a squashfs (verifying `diff_ids`), and installs the result
//! under the manifest digest.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::cache::{CONFIG_FILE, ImageCache, MANIFEST_FILE, ROOTFS_FILE};
use super::flatten::flatten_to_squashfs;
use super::model::{ImageConfig, oci_arch};
use super::reference::{ImageReference, Selector, parse_image_reference};
use crate::oci::auth;
use crate::oci::client::{HttpTransport, Transport, digest_of};
use crate::oci::manifest::{Descriptor, ImageIndex, ManifestOrIndex, parse_manifest_or_index};

/// Progress of an image pull's layer-download phase (the shape mirrors the
/// template pull's [`crate::oci::client::PullProgress`]). Byte counts are
/// compressed (what is actually transferred); `layer` counts completed
/// layers (0 at the start, `layers` when done), with intra-layer byte
/// updates streamed in between.
#[derive(Debug, Clone, Copy)]
pub struct ImagePullProgress {
    pub layer: usize,
    pub layers: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// A resolved, cached container image.
#[derive(Debug, Clone)]
pub struct PulledImage {
    /// The `sha256:<hex>` digest of the platform manifest — the image's
    /// cache identity.
    pub manifest_digest: String,
    /// The parsed image config (runtime defaults, platform).
    pub config: ImageConfig,
    /// The flattened root filesystem (`rootfs.sqfs`) in the cache.
    pub rootfs_image: PathBuf,
}

/// Resolve `reference` for vmlab `arch` against `cache`, pulling from the
/// registry only when the image is not already installed. `progress` fires
/// during the layer download (never when the image is cached).
pub async fn ensure_container_image(
    reference: &str,
    arch: &str,
    cache: &ImageCache,
    progress: &mut (dyn FnMut(ImagePullProgress) + Send),
) -> Result<PulledImage> {
    let reference = parse_image_reference(reference)?;
    // A cached digest selector needs no transport (and no credentials).
    if let Selector::Digest(d) = &reference.selector
        && let Some(image) = load_cached(cache, d)?
    {
        return Ok(image);
    }
    let credential = auth::resolve(&reference.host)?;
    let transport = HttpTransport::new(reference.host.clone(), credential)?;
    pull_with_transport(&reference, arch, cache, &transport, progress).await
}

/// The transport-injectable core of [`ensure_container_image`] (tests drive
/// it with a fake registry, mirroring `Registry::with_transport`).
pub(crate) async fn pull_with_transport(
    reference: &ImageReference,
    arch: &str,
    cache: &ImageCache,
    transport: &dyn Transport,
    progress: &mut (dyn FnMut(ImagePullProgress) + Send),
) -> Result<PulledImage> {
    let repo = &reference.repository;
    let want_arch = oci_arch(arch)?;

    // 1. Resolve the selector to the top-level manifest/index bytes.
    let fetched = match &reference.selector {
        Selector::Digest(d) => {
            if let Some(image) = load_cached(cache, d)? {
                return Ok(image); // content-addressed: fully offline
            }
            let f = transport
                .get_manifest(repo, d)
                .await
                .with_context(|| format!("resolving {reference}"))?
                .ok_or_else(|| anyhow!("{reference} not found in the registry"))?;
            let got = digest_of(&f.body);
            if !got.eq_ignore_ascii_case(d) {
                bail!("{reference}: registry returned a document with digest {got}");
            }
            f
        }
        Selector::Tag(tag) => match transport.get_manifest(repo, tag).await {
            Ok(Some(f)) => f,
            Ok(None) => bail!("{reference} not found in the registry"),
            Err(e) => {
                // Offline fallback: the digest this tag last resolved to.
                if let Some(d) = cache.resolved_ref(&reference.host, repo, tag)
                    && let Some(image) = load_cached(cache, &d)?
                {
                    tracing::warn!(
                        "cannot reach {}: {e:#} — using cached {d} for {reference}",
                        reference.host
                    );
                    return Ok(image);
                }
                return Err(e).with_context(|| format!("resolving {reference}"));
            }
        },
    };
    let top_digest = digest_of(&fetched.body);

    // 2. Resolve to the single platform manifest (index-aware).
    let (manifest, manifest_digest, manifest_bytes) = match parse_manifest_or_index(&fetched.body)?
    {
        ManifestOrIndex::Manifest(m) => (m, top_digest, fetched.body),
        ManifestOrIndex::Index(index) => {
            let desc = resolve_platform(&index, want_arch).ok_or_else(|| {
                let platforms: Vec<String> = index
                    .manifests
                    .iter()
                    .filter_map(|d| d.platform.as_ref())
                    .map(|p| format!("{}/{}", p.os, p.architecture))
                    .collect();
                anyhow!(
                    "{reference} has no linux/{want_arch} image (available: {})",
                    platforms.join(", ")
                )
            })?;
            let digest = desc.digest.clone();
            if let Some(image) = load_cached(cache, &digest)? {
                // Already flattened for this platform; refresh the tag record.
                if let Selector::Tag(tag) = &reference.selector {
                    cache.record_ref(&reference.host, repo, tag, &digest)?;
                }
                return Ok(image);
            }
            let f = transport
                .get_manifest(repo, &digest)
                .await?
                .ok_or_else(|| anyhow!("manifest {digest} missing from registry"))?;
            let got = digest_of(&f.body);
            if !got.eq_ignore_ascii_case(&digest) {
                bail!("manifest {digest} digest mismatch: got {got}");
            }
            match parse_manifest_or_index(&f.body)? {
                ManifestOrIndex::Manifest(m) => (m, digest, f.body),
                ManifestOrIndex::Index(_) => bail!("index entry {digest} is itself an index"),
            }
        }
    };
    if let Some(image) = load_cached(cache, &manifest_digest)? {
        if let Selector::Tag(tag) = &reference.selector {
            cache.record_ref(&reference.host, repo, tag, &manifest_digest)?;
        }
        return Ok(image);
    }

    // 3. Config blob: verify, parse, and refuse anything but linux.
    let config_bytes = transport
        .get_blob(repo, &manifest.config.digest)
        .await
        .context("downloading image config blob")?;
    let got = digest_of(&config_bytes);
    if !got.eq_ignore_ascii_case(&manifest.config.digest) {
        bail!("image config blob digest mismatch: got {got}");
    }
    let config: ImageConfig = serde_json::from_slice(&config_bytes)
        .with_context(|| format!("malformed image config for {reference}"))?;
    if config.os != "linux" {
        bail!(
            "{reference} is a {} image — vmlab containers only run linux images",
            config.os
        );
    }
    // Defence in depth for the plain-manifest path, which has no index keying.
    if config.architecture != want_arch {
        bail!(
            "{reference} is linux/{} but linux/{want_arch} was requested",
            config.architecture
        );
    }
    ensure!(
        config.rootfs.diff_ids.len() == manifest.layers.len(),
        "{reference}: manifest has {} layers but the config lists {} diff_ids",
        manifest.layers.len(),
        config.rootfs.diff_ids.len()
    );

    // 4. Download the layer blobs (skipping any kept from a failed earlier
    // attempt), verifying each compressed digest as it streams.
    let staging = cache.staging_dir()?;
    let layers_total = manifest.layers.len();
    let bytes_total: u64 = manifest.layers.iter().map(|d| d.size).sum();
    let mut bytes_done: u64 = 0;
    progress(ImagePullProgress {
        layer: 0,
        layers: layers_total,
        bytes_done,
        bytes_total,
    });
    let mut layer_files: Vec<(PathBuf, String)> = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        let blob = cache.blob_path(&layer.digest)?;
        if !blob.is_file() {
            let tmp = staging.path().join(format!("layer-{i:04}"));
            let base = bytes_done;
            let mut on_bytes = |n: u64| {
                progress(ImagePullProgress {
                    layer: i,
                    layers: layers_total,
                    bytes_done: base + n,
                    bytes_total,
                });
            };
            transport
                .get_blob_to_file(repo, &layer.digest, &tmp, &mut on_bytes)
                .await
                .with_context(|| format!("downloading layer {i} of {reference}"))?;
            cache.install_blob(&tmp, &layer.digest)?;
        }
        layer_files.push((blob, layer.media_type.clone()));
        bytes_done += layer.size;
        progress(ImagePullProgress {
            layer: i + 1,
            layers: layers_total,
            bytes_done,
            bytes_total,
        });
    }

    // 5. Flatten to a squashfs, verifying diff_ids (blocking work). The
    // blobs stay in the cache on failure so a retry resumes.
    let rootfs_tmp = staging.path().join(ROOTFS_FILE);
    {
        let layer_files = layer_files.clone();
        let diff_ids = config.rootfs.diff_ids.clone();
        let dest = rootfs_tmp.clone();
        tokio::task::spawn_blocking(move || flatten_to_squashfs(&layer_files, &diff_ids, &dest))
            .await
            .context("flatten task panicked")?
            .with_context(|| format!("flattening {reference}"))?;
    }

    // 6. Install the image atomically, then drop the now-redundant blobs
    // and record the tag resolution.
    std::fs::write(staging.path().join(MANIFEST_FILE), &manifest_bytes)
        .context("cannot stage manifest.json")?;
    std::fs::write(staging.path().join(CONFIG_FILE), &config_bytes)
        .context("cannot stage config.json")?;
    let dir = cache.install_image(staging.path(), &manifest_digest)?;
    for layer in &manifest.layers {
        let _ = cache.remove_blob(&layer.digest);
    }
    if let Selector::Tag(tag) = &reference.selector {
        cache.record_ref(&reference.host, repo, tag, &manifest_digest)?;
    }
    tracing::info!(reference = %reference, digest = %manifest_digest, "pulled container image");
    Ok(PulledImage {
        manifest_digest,
        config,
        rootfs_image: dir.join(ROOTFS_FILE),
    })
}

/// Pick the index entry for `linux/<want_arch>`. For `arm64` only the `v8`
/// (or unstated) variant matches — a 32-bit `arm/v7` entry never does.
fn resolve_platform<'a>(index: &'a ImageIndex, want_arch: &str) -> Option<&'a Descriptor> {
    index.manifests.iter().find(|d| {
        d.platform.as_ref().is_some_and(|p| {
            p.os == "linux"
                && p.architecture == want_arch
                && (want_arch != "arm64" || matches!(p.variant.as_deref(), None | Some("v8")))
        })
    })
}

/// Load an installed image from the cache, if complete.
fn load_cached(cache: &ImageCache, manifest_digest: &str) -> Result<Option<PulledImage>> {
    let Some(dir) = cache.cached_image(manifest_digest) else {
        return Ok(None);
    };
    let config_path = dir.join(CONFIG_FILE);
    let bytes = std::fs::read(&config_path)
        .with_context(|| format!("cannot read {}", config_path.display()))?;
    let config: ImageConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("malformed cached config {}", config_path.display()))?;
    Ok(Some(PulledImage {
        manifest_digest: manifest_digest.to_string(),
        config,
        rootfs_image: dir.join(ROOTFS_FILE),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::client::Fetched;
    use crate::oci::media_types;
    use crate::qemu::process::binary_on_path;
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// True when `bin` is on PATH; tests skip (with a note) otherwise.
    fn have(bin: &str) -> bool {
        let found = binary_on_path(bin);
        if !found {
            eprintln!("skipping: {bin} not on PATH");
        }
        found
    }

    /// An in-memory fake image registry: read-only Transport over maps, with
    /// a switchable "network down" mode and a blob-fetch counter.
    #[derive(Default)]
    struct FakeHub {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        manifests: Mutex<HashMap<String, (String, Vec<u8>)>>,
        offline: AtomicBool,
        blob_gets: AtomicUsize,
    }

    fn key(repo: &str, id: &str) -> String {
        format!("{repo}@{id}")
    }

    impl FakeHub {
        fn put_blob(&self, repo: &str, bytes: Vec<u8>) -> String {
            let digest = digest_of(&bytes);
            self.blobs.lock().unwrap().insert(key(repo, &digest), bytes);
            digest
        }

        fn put_manifest(&self, repo: &str, reference: &str, media_type: &str, body: Vec<u8>) {
            self.manifests
                .lock()
                .unwrap()
                .insert(key(repo, reference), (media_type.to_string(), body));
        }

        fn check_online(&self) -> Result<()> {
            if self.offline.load(Ordering::SeqCst) {
                bail!("network down (fake)");
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Transport for FakeHub {
        async fn blob_exists(&self, repo: &str, digest: &str) -> Result<bool> {
            self.check_online()?;
            Ok(self.blobs.lock().unwrap().contains_key(&key(repo, digest)))
        }
        async fn get_blob(&self, repo: &str, digest: &str) -> Result<Vec<u8>> {
            self.check_online()?;
            self.blob_gets.fetch_add(1, Ordering::SeqCst);
            self.blobs
                .lock()
                .unwrap()
                .get(&key(repo, digest))
                .cloned()
                .ok_or_else(|| anyhow!("no blob {digest}"))
        }
        async fn put_blob(&self, _: &str, _: &str, _: Vec<u8>) -> Result<()> {
            bail!("not used")
        }
        async fn put_blob_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            bail!("not used")
        }
        async fn get_manifest(&self, repo: &str, reference: &str) -> Result<Option<Fetched>> {
            self.check_online()?;
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
        async fn put_manifest(&self, _: &str, _: &str, _: &str, _: Vec<u8>) -> Result<String> {
            bail!("not used")
        }
        async fn list_tags(&self, _: &str) -> Result<Vec<String>> {
            bail!("not used")
        }
        async fn list_catalog(&self) -> Result<Vec<String>> {
            bail!("not used")
        }
    }

    /// A single-file gzipped layer: returns (gz bytes, diff_id).
    fn gz_layer(path: &str, contents: &str) -> (Vec<u8>, String) {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(contents.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, path, contents.as_bytes()).unwrap();
        let tar_bytes = tar.into_inner().unwrap();
        let diff_id = digest_of(&tar_bytes);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        (enc.finish().unwrap(), diff_id)
    }

    /// Seed `hub` with a hand-built two-layer docker-style image. Returns
    /// the platform manifest's digest. When `tag` is set, a multi-arch
    /// index (amd64 + a dangling arm64/v8 entry) is stored under it.
    fn seed_image(
        hub: &FakeHub,
        repo: &str,
        tag: Option<&str>,
        os: &str,
        arch: &str,
        corrupt_diff_ids: bool,
    ) -> String {
        let (gz1, diff1) = gz_layer("etc/marker", "one");
        let (gz2, diff2) = gz_layer("etc/marker", "two");
        let layer_media = "application/vnd.docker.image.rootfs.diff.tar.gzip";
        let (size1, size2) = (gz1.len() as u64, gz2.len() as u64);
        let d1 = hub.put_blob(repo, gz1);
        let d2 = hub.put_blob(repo, gz2);

        let diff2_recorded = if corrupt_diff_ids {
            format!("sha256:{}", "0".repeat(64))
        } else {
            diff2
        };
        let config = serde_json::json!({
            "architecture": arch,
            "os": os,
            "config": { "Env": ["FOO=bar"], "Cmd": ["/bin/sh"] },
            "rootfs": { "type": "layers", "diff_ids": [diff1, diff2_recorded] }
        });
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let config_size = config_bytes.len() as u64;
        let config_digest = hub.put_blob(repo, config_bytes);

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::DOCKER_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_size
            },
            "layers": [
                { "mediaType": layer_media, "digest": d1, "size": size1 },
                { "mediaType": layer_media, "digest": d2, "size": size2 }
            ]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = digest_of(&manifest_bytes);
        hub.put_manifest(
            repo,
            &manifest_digest,
            media_types::DOCKER_MANIFEST,
            manifest_bytes.clone(),
        );

        if let Some(tag) = tag {
            let index = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": media_types::DOCKER_MANIFEST_LIST,
                "manifests": [
                    {
                        "mediaType": media_types::DOCKER_MANIFEST,
                        "digest": manifest_digest,
                        "size": manifest_bytes.len(),
                        "platform": { "architecture": arch, "os": os }
                    },
                    {
                        // Dangling entry for another platform: resolving the
                        // wrong one would fail loudly on the missing manifest.
                        "mediaType": media_types::DOCKER_MANIFEST,
                        "digest": format!("sha256:{}", "d".repeat(64)),
                        "size": 1,
                        "platform": { "architecture": "arm64", "os": "linux", "variant": "v8" }
                    }
                ]
            });
            hub.put_manifest(
                repo,
                tag,
                media_types::DOCKER_MANIFEST_LIST,
                serde_json::to_vec(&index).unwrap(),
            );
        }
        manifest_digest
    }

    fn new_cache() -> (tempfile::TempDir, ImageCache) {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(tmp.path().join("oci"));
        (tmp, cache)
    }

    #[tokio::test]
    async fn pull_resolves_platform_flattens_and_goes_offline() {
        if !have("sqfstar") {
            return;
        }
        let hub = FakeHub::default();
        let md = seed_image(
            &hub,
            "library/nginx",
            Some("latest"),
            "linux",
            "amd64",
            false,
        );
        let (_tmp, cache) = new_cache();
        let reference = parse_image_reference("nginx").unwrap();

        let mut updates: Vec<ImagePullProgress> = Vec::new();
        let image =
            pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |p| updates.push(p))
                .await
                .unwrap();

        // The amd64 platform manifest was chosen and its config parsed.
        assert_eq!(image.manifest_digest, md);
        assert_eq!(image.config.config.env, vec!["FOO=bar"]);
        assert_eq!(image.config.config.cmd, vec!["/bin/sh"]);
        assert!(image.rootfs_image.is_file());

        // Progress: initial 0-of-N, byte updates, complete final report.
        let first = updates.first().copied().expect("progress reported");
        let last = updates.last().copied().unwrap();
        assert_eq!((first.layer, first.bytes_done), (0, 0));
        assert_eq!(last.layer, last.layers);
        assert_eq!(last.layers, 2);
        assert_eq!(last.bytes_done, last.bytes_total);
        assert!(last.bytes_total > 0);
        for w in updates.windows(2) {
            assert!(w[1].bytes_done >= w[0].bytes_done, "monotonic bytes");
        }

        // Layer blobs were deleted after the successful flatten.
        assert!(
            !cache.root().join("blobs/sha256").is_dir() || {
                std::fs::read_dir(cache.root().join("blobs/sha256"))
                    .unwrap()
                    .next()
                    .is_none()
            }
        );

        // The flattened rootfs has the upper layer's content.
        if have("unsquashfs") {
            let out = std::process::Command::new("unsquashfs")
                .args(["-cat", image.rootfs_image.to_str().unwrap(), "etc/marker"])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(out.stdout, b"two", "last layer wins");
        }

        // Online second call: resolves the tag, finds the platform image
        // cached, downloads no blobs.
        let gets_before = hub.blob_gets.load(Ordering::SeqCst);
        let again = pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(again.manifest_digest, md);
        assert_eq!(hub.blob_gets.load(Ordering::SeqCst), gets_before);

        // Registry unreachable: the tag falls back to its cached resolution…
        hub.offline.store(true, Ordering::SeqCst);
        let offline = pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(offline.manifest_digest, md);

        // …and a digest selector is satisfied with no registry at all.
        let by_digest = parse_image_reference(&format!("nginx@{md}")).unwrap();
        let pinned = pull_with_transport(&by_digest, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(pinned.manifest_digest, md);
    }

    #[tokio::test]
    async fn uncached_tag_fails_when_offline() {
        let hub = FakeHub::default();
        hub.offline.store(true, Ordering::SeqCst);
        let (_tmp, cache) = new_cache();
        let reference = parse_image_reference("nginx").unwrap();
        let err = pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("resolving"), "{err}");
    }

    #[tokio::test]
    async fn rejects_non_linux_image() {
        let hub = FakeHub::default();
        // A plain (non-index) manifest under the tag, config os=windows.
        let md = seed_image(&hub, "owner/winapp", None, "windows", "amd64", false);
        // Bind before the call: the guard temporary must not overlap the
        // re-lock inside put_manifest.
        let body = hub.manifests.lock().unwrap()[&key("owner/winapp", &md)]
            .1
            .clone();
        hub.put_manifest("owner/winapp", "v1", media_types::DOCKER_MANIFEST, body);
        let (_tmp, cache) = new_cache();
        let reference = parse_image_reference("owner/winapp:v1").unwrap();
        let err = pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("windows"), "{text}");
        assert!(
            text.contains("owner/winapp"),
            "error names the image: {text}"
        );
    }

    #[tokio::test]
    async fn rejects_wrong_arch_plain_manifest() {
        let hub = FakeHub::default();
        let md = seed_image(&hub, "owner/armapp", None, "linux", "arm64", false);
        let body = hub.manifests.lock().unwrap()[&key("owner/armapp", &md)]
            .1
            .clone();
        hub.put_manifest("owner/armapp", "v1", media_types::DOCKER_MANIFEST, body);
        let (_tmp, cache) = new_cache();
        let reference = parse_image_reference("owner/armapp:v1").unwrap();
        let err = pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("amd64"), "{err}");
    }

    #[tokio::test]
    async fn rejects_missing_platform_in_index() {
        let hub = FakeHub::default();
        seed_image(
            &hub,
            "library/nginx",
            Some("latest"),
            "linux",
            "amd64",
            false,
        );
        let (_tmp, cache) = new_cache();
        let reference = parse_image_reference("nginx").unwrap();
        // The index has amd64 + arm64 only.
        let err = pull_with_transport(&reference, "riscv64", &cache, &hub, &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("riscv64"), "{err}");
    }

    #[tokio::test]
    async fn rejects_diff_id_mismatch() {
        if !have("sqfstar") {
            return;
        }
        let hub = FakeHub::default();
        seed_image(&hub, "library/bad", Some("latest"), "linux", "amd64", true);
        let (_tmp, cache) = new_cache();
        let reference = parse_image_reference("bad").unwrap();
        let err = pull_with_transport(&reference, "x86_64", &cache, &hub, &mut |_| {})
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("diff_id mismatch"), "{err:#}");
        // nothing installed, and the verified blobs are kept for a retry
        assert!(
            !cache.root().join("images/sha256").is_dir()
                || std::fs::read_dir(cache.root().join("images/sha256"))
                    .unwrap()
                    .next()
                    .is_none(),
            "no image installed on failure"
        );
        assert!(
            std::fs::read_dir(cache.root().join("blobs/sha256"))
                .map(|d| d.count() > 0)
                .unwrap_or(false),
            "layer blobs kept on failure"
        );
    }

    #[test]
    fn resolve_platform_honours_arm64_variant() {
        let index: ImageIndex = serde_json::from_value(serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::OCI_INDEX,
            "manifests": [
                { "mediaType": media_types::OCI_MANIFEST, "digest": "sha256:v7", "size": 1,
                  "platform": { "architecture": "arm", "os": "linux", "variant": "v7" } },
                { "mediaType": media_types::OCI_MANIFEST, "digest": "sha256:v8", "size": 1,
                  "platform": { "architecture": "arm64", "os": "linux", "variant": "v8" } },
                { "mediaType": media_types::OCI_MANIFEST, "digest": "sha256:win", "size": 1,
                  "platform": { "architecture": "amd64", "os": "windows" } },
                { "mediaType": media_types::OCI_MANIFEST, "digest": "sha256:amd", "size": 1,
                  "platform": { "architecture": "amd64", "os": "linux" } }
            ]
        }))
        .unwrap();
        assert_eq!(
            resolve_platform(&index, "amd64").unwrap().digest,
            "sha256:amd"
        );
        assert_eq!(
            resolve_platform(&index, "arm64").unwrap().digest,
            "sha256:v8"
        );
        assert!(resolve_platform(&index, "riscv64").is_none());

        // arm64 with an unstated variant also matches; an explicit non-v8
        // variant does not.
        let odd: ImageIndex = serde_json::from_value(serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::OCI_INDEX,
            "manifests": [
                { "mediaType": media_types::OCI_MANIFEST, "digest": "sha256:bare", "size": 1,
                  "platform": { "architecture": "arm64", "os": "linux" } }
            ]
        }))
        .unwrap();
        assert_eq!(
            resolve_platform(&odd, "arm64").unwrap().digest,
            "sha256:bare"
        );
    }
}
