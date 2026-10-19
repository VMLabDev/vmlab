//! OCI image manifest / index types and vmlab-specific construction
//! (PRD §6.4).
//!
//! A vmlab template pushes as an OCI **artifact**: the manifest's
//! `artifactType` is [`media_types::ARTIFACT_TYPE_TEMPLATE`], the config
//! descriptor points at the template-metadata JSON blob, and the layers are
//! the zstd-compressed qcow2 chunks in order. Manifest-level annotations
//! record chunk count / chunk size / total size / whole-image digest so a
//! puller can reassemble and verify without inspecting blob contents.
//!
//! Multi-arch tags resolve through an [`ImageIndex`] keyed by platform
//! arch, mapping the store's `arch` dimension onto OCI's native
//! multi-platform mechanism.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::chunking::ChunkSet;
use super::media_types;

/// vmlab annotation keys recorded on the manifest (the `vnd.vmlab.*`
/// family — distinct from the frozen *media* types, but equally part of
/// the wire contract).
pub mod annotations {
    pub const CHUNK_COUNT: &str = "vnd.vmlab.template.chunk.count";
    pub const CHUNK_SIZE: &str = "vnd.vmlab.template.chunk.size";
    pub const TOTAL_SIZE: &str = "vnd.vmlab.template.total.size";
    /// Digest of the assembled (uncompressed) image — what pull verifies.
    pub const WHOLE_DIGEST: &str = "vnd.vmlab.template.whole.digest";
    /// Per-layer: the zero-based chunk index.
    pub const CHUNK_INDEX: &str = "vnd.vmlab.template.chunk.index";
    /// Per-layer: the uncompressed size of this chunk.
    pub const CHUNK_UNCOMPRESSED_SIZE: &str = "vnd.vmlab.template.chunk.uncompressed.size";
    /// Standard OCI annotation naming the source repository. Registries such
    /// as GHCR read it to connect a pushed package to its repo; not
    /// vmlab-namespaced.
    pub const IMAGE_SOURCE: &str = "org.opencontainers.image.source";

    // ---- variants (§6.6) ----------------------------------------------------
    //
    // A base manifest names its variants; each variant is a manifest of its
    // own whose layers are the base's chunks, in order, then the overlay's.

    /// Base manifest: the variant names, comma-separated, in the order the
    /// version's `template.wcl` lists them.
    pub const VARIANTS: &str = "vnd.vmlab.template.variants";
    /// Base manifest: the default variant's name.
    pub const VARIANT_DEFAULT: &str = "vnd.vmlab.template.variant.default";
    /// Base manifest: prefix of `<prefix><name>` = the digest of that
    /// variant's manifest, which the puller fetches by digest.
    pub const VARIANT_MANIFEST_PREFIX: &str = "vnd.vmlab.template.variant.manifest.";
    /// Variant manifest: the variant's name.
    pub const VARIANT: &str = "vnd.vmlab.template.variant";
    /// Variant manifest: how many leading layers are the base's — where the
    /// base ends and the overlay begins.
    pub const BASE_LAYERS: &str = "vnd.vmlab.template.base.layers";
    /// Variant manifest: the assembled base image's digest, size and chunks.
    pub const BASE_DIGEST: &str = "vnd.vmlab.template.base.digest";
    pub const BASE_SIZE: &str = "vnd.vmlab.template.base.size";
    pub const BASE_CHUNK_COUNT: &str = "vnd.vmlab.template.base.chunk.count";
    /// Variant manifest: the assembled overlay's digest, size and chunks.
    pub const VARIANT_DIGEST: &str = "vnd.vmlab.template.variant.digest";
    pub const VARIANT_SIZE: &str = "vnd.vmlab.template.variant.size";
    pub const VARIANT_CHUNK_COUNT: &str = "vnd.vmlab.template.variant.chunk.count";
    /// Per-layer, variant manifest: which part the chunk belongs to,
    /// [`PART_BASE`] or [`PART_VARIANT`].
    pub const PART: &str = "vnd.vmlab.template.part";
    pub const PART_BASE: &str = "base";
    pub const PART_VARIANT: &str = "variant";
}

/// An OCI content descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    /// `sha256:<hex>` digest of the referenced blob/manifest.
    pub digest: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub annotations: Option<BTreeMap<String, String>>,
    /// Present on index entries: which platform this manifest targets.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub platform: Option<Platform>,
}

impl Descriptor {
    pub fn new(media_type: impl Into<String>, digest: impl Into<String>, size: u64) -> Self {
        Self {
            media_type: media_type.into(),
            digest: digest.into(),
            size,
            annotations: None,
            platform: None,
        }
    }
}

/// OCI platform descriptor. vmlab maps the store's `arch` onto
/// `architecture`; `os` is fixed to `vmlab` since these are not OS-keyed
/// container images.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    /// CPU variant, present on container-image platforms (e.g. `arm64`/`v8`);
    /// never set on vmlab template indexes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub variant: Option<String>,
}

/// The `os` value used for vmlab template platform descriptors.
pub const PLATFORM_OS: &str = "vmlab";

/// An OCI image manifest carrying a single arch's chunked template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    #[serde(
        rename = "artifactType",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub artifact_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl Manifest {
    /// Whether this manifest is a vmlab template artifact.
    pub fn is_vmlab_template(&self) -> bool {
        self.artifact_type.as_deref() == Some(media_types::ARTIFACT_TYPE_TEMPLATE)
    }

    /// The whole-image digest annotation, if present.
    pub fn whole_digest(&self) -> Option<&str> {
        self.annotations
            .as_ref()
            .and_then(|a| a.get(annotations::WHOLE_DIGEST))
            .map(String::as_str)
    }

    /// Layer descriptors ordered by their recorded chunk index (falls back
    /// to manifest order when the annotation is absent).
    pub fn layers_in_order(&self) -> Vec<&Descriptor> {
        let mut layers: Vec<&Descriptor> = self.layers.iter().collect();
        layers.sort_by_key(|d| {
            d.annotations
                .as_ref()
                .and_then(|a| a.get(annotations::CHUNK_INDEX))
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        });
        layers
    }
}

/// Build a template manifest from a [`ChunkSet`] and the already-pushed
/// config descriptor. Layers are emitted in chunk-index order, each
/// annotated with its index and uncompressed size; the manifest carries the
/// chunk-count / chunk-size / total-size / whole-image-digest annotations.
pub fn build_manifest(set: &ChunkSet, config: Descriptor) -> Manifest {
    let mut layers = Vec::with_capacity(set.chunks.len());
    let mut ordered = set.chunks.clone();
    ordered.sort_by_key(|c| c.index);
    for c in &ordered {
        let mut ann = BTreeMap::new();
        ann.insert(annotations::CHUNK_INDEX.to_string(), c.index.to_string());
        ann.insert(
            annotations::CHUNK_UNCOMPRESSED_SIZE.to_string(),
            c.uncompressed_size.to_string(),
        );
        layers.push(Descriptor {
            media_type: media_types::CHUNK_ZSTD.to_string(),
            digest: c.compressed_digest.clone(),
            size: c.compressed_size,
            annotations: Some(ann),
            platform: None,
        });
    }

    let mut ann = BTreeMap::new();
    ann.insert(
        annotations::CHUNK_COUNT.to_string(),
        set.chunk_count.to_string(),
    );
    ann.insert(
        annotations::CHUNK_SIZE.to_string(),
        set.chunk_size.to_string(),
    );
    ann.insert(
        annotations::TOTAL_SIZE.to_string(),
        set.total_size.to_string(),
    );
    ann.insert(
        annotations::WHOLE_DIGEST.to_string(),
        set.whole_digest.clone(),
    );

    Manifest {
        schema_version: 2,
        media_type: media_types::OCI_MANIFEST.to_string(),
        artifact_type: Some(media_types::ARTIFACT_TYPE_TEMPLATE.to_string()),
        config,
        layers,
        annotations: Some(ann),
    }
}

/// Build a variant's manifest (§6.6): the base's chunk layers in order, then
/// the overlay's, under one running chunk index so [`Manifest::layers_in_order`]
/// keeps them in sequence. Each layer is marked with the part it belongs to,
/// and the manifest records where the base ends and each part's assembled
/// digest, size and chunk count.
///
/// It carries no whole-image digest: the layers do not assemble into one
/// image, and a client that predates variants refuses a manifest without one
/// rather than concatenating base and overlay into nonsense.
pub fn build_variant_manifest(
    name: &str,
    base: &ChunkSet,
    overlay: &ChunkSet,
    config: Descriptor,
) -> Manifest {
    let mut layers = Vec::with_capacity(base.chunks.len() + overlay.chunks.len());
    let parts = [
        (annotations::PART_BASE, base),
        (annotations::PART_VARIANT, overlay),
    ];
    for (part, set) in parts {
        let mut ordered = set.chunks.clone();
        ordered.sort_by_key(|c| c.index);
        for c in &ordered {
            let mut ann = BTreeMap::new();
            ann.insert(
                annotations::CHUNK_INDEX.to_string(),
                layers.len().to_string(),
            );
            ann.insert(
                annotations::CHUNK_UNCOMPRESSED_SIZE.to_string(),
                c.uncompressed_size.to_string(),
            );
            ann.insert(annotations::PART.to_string(), part.to_string());
            layers.push(Descriptor {
                media_type: media_types::CHUNK_ZSTD.to_string(),
                digest: c.compressed_digest.clone(),
                size: c.compressed_size,
                annotations: Some(ann),
                platform: None,
            });
        }
    }

    let ann: BTreeMap<String, String> = [
        (annotations::VARIANT, name.to_string()),
        (annotations::CHUNK_COUNT, layers.len().to_string()),
        (annotations::CHUNK_SIZE, base.chunk_size.to_string()),
        (annotations::BASE_LAYERS, base.chunks.len().to_string()),
        (annotations::BASE_DIGEST, base.whole_digest.clone()),
        (annotations::BASE_SIZE, base.total_size.to_string()),
        (annotations::BASE_CHUNK_COUNT, base.chunk_count.to_string()),
        (annotations::VARIANT_DIGEST, overlay.whole_digest.clone()),
        (annotations::VARIANT_SIZE, overlay.total_size.to_string()),
        (
            annotations::VARIANT_CHUNK_COUNT,
            overlay.chunk_count.to_string(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    Manifest {
        schema_version: 2,
        media_type: media_types::OCI_MANIFEST.to_string(),
        artifact_type: Some(media_types::ARTIFACT_TYPE_TEMPLATE.to_string()),
        config,
        layers,
        annotations: Some(ann),
    }
}

/// One variant a base manifest names: its name and its manifest's digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantEntry {
    pub name: String,
    pub manifest: String,
}

/// A variant manifest read back: where its base ends, and what each part
/// assembles to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantLayout {
    pub name: String,
    /// The base's chunk layers, in order.
    pub base: Vec<Descriptor>,
    /// `sha256:<hex>` of the assembled base.
    pub base_digest: String,
    /// The overlay's chunk layers, in order.
    pub overlay: Vec<Descriptor>,
    /// `sha256:<hex>` of the assembled overlay.
    pub overlay_digest: String,
}

impl Manifest {
    fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations
            .as_ref()
            .and_then(|a| a.get(key))
            .map(String::as_str)
    }

    /// Record a base manifest's variants: their names in order, the default,
    /// and each one's manifest digest.
    pub fn set_variants(&mut self, entries: &[VariantEntry], default: Option<&str>) {
        if entries.is_empty() {
            return;
        }
        let ann = self.annotations.get_or_insert_with(BTreeMap::new);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        ann.insert(annotations::VARIANTS.to_string(), names.join(","));
        if let Some(d) = default {
            ann.insert(annotations::VARIANT_DEFAULT.to_string(), d.to_string());
        }
        for e in entries {
            ann.insert(
                format!("{}{}", annotations::VARIANT_MANIFEST_PREFIX, e.name),
                e.manifest.clone(),
            );
        }
    }

    /// The variants a base manifest names, in order, and its default. Empty
    /// for a base-only template, and for one pushed before variants existed.
    pub fn variants(&self) -> Result<(Vec<VariantEntry>, Option<String>)> {
        let Some(list) = self.annotation(annotations::VARIANTS) else {
            return Ok((Vec::new(), None));
        };
        let mut entries = Vec::new();
        for name in list.split(',').filter(|n| !n.is_empty()) {
            let key = format!("{}{name}", annotations::VARIANT_MANIFEST_PREFIX);
            let manifest = self.annotation(&key).ok_or_else(|| {
                anyhow!("the manifest names variant `{name}` but not its manifest")
            })?;
            entries.push(VariantEntry {
                name: name.to_string(),
                manifest: manifest.to_string(),
            });
        }
        let default = match self.annotation(annotations::VARIANT_DEFAULT) {
            Some(d) => Some(d.to_string()),
            None if entries.len() == 1 => Some(entries[0].name.clone()),
            None => None,
        };
        if let Some(d) = &default
            && !entries.iter().any(|e| &e.name == d)
        {
            bail!("the manifest's default variant `{d}` is not one of its variants");
        }
        Ok((entries, default))
    }

    /// Read a variant manifest's layout: its layers split where the base
    /// ends, and each part's assembled digest. Errors when the manifest is
    /// not a variant's, or its annotations disagree with its layers.
    pub fn variant_layout(&self) -> Result<VariantLayout> {
        let name = self
            .annotation(annotations::VARIANT)
            .ok_or_else(|| anyhow!("not a variant manifest: no `{}`", annotations::VARIANT))?
            .to_string();
        let need = |key: &str| {
            self.annotation(key)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("variant manifest `{name}` is missing `{key}`"))
        };
        let base_layers: usize = need(annotations::BASE_LAYERS)?
            .parse()
            .map_err(|_| anyhow!("variant manifest `{name}`: malformed base layer count"))?;
        let ordered: Vec<Descriptor> = self.layers_in_order().into_iter().cloned().collect();
        if base_layers == 0 || base_layers >= ordered.len() {
            bail!(
                "variant manifest `{name}` says {base_layers} of its {} layers are the base",
                ordered.len()
            );
        }
        let (base, overlay) = ordered.split_at(base_layers);
        let part_is = |layers: &[Descriptor], want: &str| {
            layers.iter().all(|d| {
                d.annotations
                    .as_ref()
                    .and_then(|a| a.get(annotations::PART))
                    .is_none_or(|p| p == want)
            })
        };
        if !part_is(base, annotations::PART_BASE) || !part_is(overlay, annotations::PART_VARIANT) {
            bail!(
                "variant manifest `{name}`: its layers' parts disagree with its base layer count"
            );
        }
        Ok(VariantLayout {
            base_digest: need(annotations::BASE_DIGEST)?,
            overlay_digest: need(annotations::VARIANT_DIGEST)?,
            base: base.to_vec(),
            overlay: overlay.to_vec(),
            name,
        })
    }
}

/// An OCI image index (multi-arch) — manifest descriptors keyed by
/// platform arch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub manifests: Vec<Descriptor>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl Default for ImageIndex {
    fn default() -> Self {
        Self {
            schema_version: 2,
            media_type: media_types::OCI_INDEX.to_string(),
            manifests: Vec::new(),
            annotations: None,
        }
    }
}

impl ImageIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) the manifest descriptor for `arch`. A descriptor's
    /// platform is set from `arch`; any existing entry for the same arch is
    /// replaced so repeated single-arch pushes converge on one index entry
    /// per arch (PRD §6.4 multi-arch assembly).
    pub fn upsert_arch(&mut self, arch: &str, mut manifest_desc: Descriptor) {
        manifest_desc.platform = Some(Platform {
            architecture: arch.to_string(),
            os: PLATFORM_OS.to_string(),
            variant: None,
        });
        if let Some(existing) = self
            .manifests
            .iter_mut()
            .find(|d| d.platform.as_ref().map(|p| p.architecture.as_str()) == Some(arch))
        {
            *existing = manifest_desc;
        } else {
            self.manifests.push(manifest_desc);
        }
    }

    /// The manifest descriptor for `arch`, if present.
    pub fn manifest_for_arch(&self, arch: &str) -> Option<&Descriptor> {
        self.manifests
            .iter()
            .find(|d| d.platform.as_ref().map(|p| p.architecture.as_str()) == Some(arch))
    }

    /// If the index has exactly one entry, return it (the "unambiguous
    /// single-arch manifest" pull path of §6.4).
    pub fn single(&self) -> Option<&Descriptor> {
        if self.manifests.len() == 1 {
            self.manifests.first()
        } else {
            None
        }
    }

    /// Resolve the manifest descriptor to pull. `arch` is required unless
    /// the index is unambiguously single-arch (PRD §6.4 — never assume the
    /// host arch).
    pub fn resolve(&self, arch: Option<&str>) -> Result<&Descriptor> {
        match arch {
            Some(a) => self
                .manifest_for_arch(a)
                .ok_or_else(|| anyhow!("index has no manifest for arch `{a}`")),
            None => self.single().ok_or_else(|| {
                let arches: Vec<&str> = self
                    .manifests
                    .iter()
                    .filter_map(|d| d.platform.as_ref().map(|p| p.architecture.as_str()))
                    .collect();
                anyhow!(
                    "this is a multi-arch index ({}); --arch is required",
                    arches.join(", ")
                )
            }),
        }
    }
}

/// Sniff a fetched manifest/index document: returns `Ok(Either)` or an
/// error if it parses as neither.
pub enum ManifestOrIndex {
    Manifest(Manifest),
    Index(ImageIndex),
}

/// Parse raw bytes as either an image index or an image manifest. The
/// `mediaType` field (when present) disambiguates — the OCI types and their
/// Docker schema-2 equivalents are both accepted, so container images pulled
/// from Docker Hub resolve; otherwise the presence of `manifests` (index) vs
/// `layers` (manifest) decides.
pub fn parse_manifest_or_index(bytes: &[u8]) -> Result<ManifestOrIndex> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| anyhow!("malformed manifest JSON: {e}"))?;
    let media = value
        .get("mediaType")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let is_index = media == media_types::OCI_INDEX
        || media == media_types::DOCKER_MANIFEST_LIST
        || (media.is_empty() && value.get("manifests").is_some());
    if is_index {
        let index: ImageIndex =
            serde_json::from_value(value).map_err(|e| anyhow!("malformed image index: {e}"))?;
        Ok(ManifestOrIndex::Index(index))
    } else if value.get("layers").is_some()
        || media == media_types::OCI_MANIFEST
        || media == media_types::DOCKER_MANIFEST
    {
        let manifest: Manifest =
            serde_json::from_value(value).map_err(|e| anyhow!("malformed image manifest: {e}"))?;
        Ok(ManifestOrIndex::Manifest(manifest))
    } else {
        bail!("document is neither an OCI image manifest nor an image index");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::chunking::ChunkInfo;
    use std::path::PathBuf;

    fn sample_set() -> ChunkSet {
        ChunkSet {
            chunks: vec![
                ChunkInfo {
                    index: 0,
                    compressed_path: PathBuf::from("/tmp/chunk-0000.zst"),
                    compressed_digest: "sha256:aa".to_string(),
                    compressed_size: 100,
                    uncompressed_size: 1024,
                },
                ChunkInfo {
                    index: 1,
                    compressed_path: PathBuf::from("/tmp/chunk-0001.zst"),
                    compressed_digest: "sha256:bb".to_string(),
                    compressed_size: 80,
                    uncompressed_size: 512,
                },
            ],
            whole_digest: "sha256:ff".to_string(),
            total_size: 1536,
            chunk_size: 1024,
            chunk_count: 2,
        }
    }

    #[test]
    fn manifest_serde_round_trip_and_annotations() {
        let config = Descriptor::new(media_types::CONFIG_TEMPLATE_JSON, "sha256:cc", 42);
        let manifest = build_manifest(&sample_set(), config);

        assert_eq!(manifest.schema_version, 2);
        assert!(manifest.is_vmlab_template());
        assert_eq!(manifest.layers.len(), 2);
        // whole digest annotation present and correct
        assert_eq!(manifest.whole_digest(), Some("sha256:ff"));
        let ann = manifest.annotations.as_ref().unwrap();
        assert_eq!(ann.get(annotations::CHUNK_COUNT).unwrap(), "2");
        assert_eq!(ann.get(annotations::CHUNK_SIZE).unwrap(), "1024");
        assert_eq!(ann.get(annotations::TOTAL_SIZE).unwrap(), "1536");
        // per-layer index annotations
        assert_eq!(
            manifest.layers[1]
                .annotations
                .as_ref()
                .unwrap()
                .get(annotations::CHUNK_INDEX)
                .unwrap(),
            "1"
        );

        let json = serde_json::to_vec(&manifest).unwrap();
        // the JSON uses OCI camelCase keys
        let text = String::from_utf8(json.clone()).unwrap();
        assert!(text.contains("\"schemaVersion\":2"));
        assert!(text.contains("\"artifactType\""));
        assert!(text.contains("\"mediaType\""));

        match parse_manifest_or_index(&json).unwrap() {
            ManifestOrIndex::Manifest(m) => assert_eq!(m, manifest),
            ManifestOrIndex::Index(_) => panic!("parsed as index"),
        }
    }

    #[test]
    fn layers_in_order_sorts_by_index() {
        let config = Descriptor::new(media_types::CONFIG_TEMPLATE_JSON, "sha256:cc", 42);
        let mut manifest = build_manifest(&sample_set(), config);
        manifest.layers.reverse(); // scramble on-wire order
        let ordered = manifest.layers_in_order();
        assert_eq!(ordered[0].digest, "sha256:aa");
        assert_eq!(ordered[1].digest, "sha256:bb");
    }

    #[test]
    fn index_merge_two_arches() {
        let mut index = ImageIndex::new();
        index.upsert_arch(
            "x86_64",
            Descriptor::new(media_types::OCI_MANIFEST, "sha256:1111", 10),
        );
        index.upsert_arch(
            "aarch64",
            Descriptor::new(media_types::OCI_MANIFEST, "sha256:2222", 20),
        );
        assert_eq!(index.manifests.len(), 2);
        assert_eq!(
            index.manifest_for_arch("x86_64").unwrap().digest,
            "sha256:1111"
        );
        assert_eq!(
            index.manifest_for_arch("aarch64").unwrap().digest,
            "sha256:2222"
        );
        // platform recorded
        assert_eq!(
            index.manifests[0].platform.as_ref().unwrap().os,
            PLATFORM_OS
        );

        // re-push x86_64 replaces, does not duplicate
        index.upsert_arch(
            "x86_64",
            Descriptor::new(media_types::OCI_MANIFEST, "sha256:3333", 30),
        );
        assert_eq!(index.manifests.len(), 2);
        assert_eq!(
            index.manifest_for_arch("x86_64").unwrap().digest,
            "sha256:3333"
        );

        // round-trip through JSON and back
        let bytes = serde_json::to_vec(&index).unwrap();
        match parse_manifest_or_index(&bytes).unwrap() {
            ManifestOrIndex::Index(i) => assert_eq!(i, index),
            ManifestOrIndex::Manifest(_) => panic!("parsed as manifest"),
        }
    }

    #[test]
    fn index_resolve_requires_arch_when_ambiguous() {
        let mut index = ImageIndex::new();
        index.upsert_arch(
            "x86_64",
            Descriptor::new(media_types::OCI_MANIFEST, "sha256:1", 1),
        );
        index.upsert_arch(
            "aarch64",
            Descriptor::new(media_types::OCI_MANIFEST, "sha256:2", 2),
        );
        let err = index.resolve(None).unwrap_err();
        assert!(err.to_string().contains("--arch is required"), "{err}");
        assert_eq!(index.resolve(Some("x86_64")).unwrap().digest, "sha256:1");
        assert!(index.resolve(Some("riscv64")).is_err());
    }

    #[test]
    fn index_resolve_single_arch_without_flag() {
        let mut index = ImageIndex::new();
        index.upsert_arch(
            "x86_64",
            Descriptor::new(media_types::OCI_MANIFEST, "sha256:1", 1),
        );
        assert_eq!(index.resolve(None).unwrap().digest, "sha256:1");
    }

    #[test]
    fn parses_docker_media_types() {
        // A docker schema-2 manifest list is an index …
        let list = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::DOCKER_MANIFEST_LIST,
            "manifests": [{
                "mediaType": media_types::DOCKER_MANIFEST,
                "digest": "sha256:1111",
                "size": 10,
                "platform": { "architecture": "amd64", "os": "linux" }
            }, {
                "mediaType": media_types::DOCKER_MANIFEST,
                "digest": "sha256:2222",
                "size": 11,
                "platform": { "architecture": "arm64", "os": "linux", "variant": "v8" }
            }]
        });
        match parse_manifest_or_index(&serde_json::to_vec(&list).unwrap()).unwrap() {
            ManifestOrIndex::Index(i) => {
                assert_eq!(i.manifests.len(), 2);
                // the variant survives the round trip
                assert_eq!(
                    i.manifests[1].platform.as_ref().unwrap().variant.as_deref(),
                    Some("v8")
                );
                assert_eq!(i.manifests[0].platform.as_ref().unwrap().variant, None);
            }
            ManifestOrIndex::Manifest(_) => panic!("docker manifest list parsed as manifest"),
        }

        // … and a docker schema-2 manifest is a manifest.
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media_types::DOCKER_MANIFEST,
            "config": {
                "mediaType": "application/vnd.docker.container.image.v1+json",
                "digest": "sha256:cc",
                "size": 42
            },
            "layers": [{
                "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                "digest": "sha256:aa",
                "size": 100
            }]
        });
        match parse_manifest_or_index(&serde_json::to_vec(&manifest).unwrap()).unwrap() {
            ManifestOrIndex::Manifest(m) => {
                assert_eq!(m.layers.len(), 1);
                assert!(!m.is_vmlab_template(), "container image is not a template");
            }
            ManifestOrIndex::Index(_) => panic!("docker manifest parsed as index"),
        }
    }

    #[test]
    fn rejects_non_vmlab_artifact_type() {
        let manifest = Manifest {
            schema_version: 2,
            media_type: media_types::OCI_MANIFEST.to_string(),
            artifact_type: Some("application/vnd.oci.image.config.v1+json".to_string()),
            config: Descriptor::new("application/vnd.oci.image.config.v1+json", "sha256:c", 1),
            layers: vec![],
            annotations: None,
        };
        assert!(!manifest.is_vmlab_template());
    }

    fn overlay_set() -> ChunkSet {
        ChunkSet {
            chunks: vec![ChunkInfo {
                index: 0,
                compressed_path: PathBuf::from("/tmp/v/chunk-0000.zst"),
                compressed_digest: "sha256:dd".to_string(),
                compressed_size: 30,
                uncompressed_size: 300,
            }],
            whole_digest: "sha256:ee".to_string(),
            total_size: 300,
            chunk_size: 1024,
            chunk_count: 1,
        }
    }

    /// A variant's manifest: the base's layers first, then the overlay's,
    /// on one running index; each part's digest, size and count recorded;
    /// no whole-image digest. Read back, the layout splits where the base
    /// ends — whatever order the layers arrive in.
    #[test]
    fn variant_manifest_layout() {
        let config = Descriptor::new(media_types::CONFIG_TEMPLATE_JSON, "sha256:cc", 42);
        let mut m = build_variant_manifest("extra", &sample_set(), &overlay_set(), config);
        assert!(m.is_vmlab_template());
        assert_eq!(m.whole_digest(), None);
        let digests: Vec<&str> = m.layers.iter().map(|d| d.digest.as_str()).collect();
        assert_eq!(digests, ["sha256:aa", "sha256:bb", "sha256:dd"]);
        let ann = m.annotations.clone().unwrap();
        assert_eq!(ann[annotations::VARIANT], "extra");
        assert_eq!(ann[annotations::BASE_LAYERS], "2");
        assert_eq!(ann[annotations::CHUNK_COUNT], "3");
        assert_eq!(ann[annotations::BASE_DIGEST], "sha256:ff");
        assert_eq!(ann[annotations::BASE_SIZE], "1536");
        assert_eq!(ann[annotations::BASE_CHUNK_COUNT], "2");
        assert_eq!(ann[annotations::VARIANT_DIGEST], "sha256:ee");
        assert_eq!(ann[annotations::VARIANT_SIZE], "300");
        assert_eq!(ann[annotations::VARIANT_CHUNK_COUNT], "1");
        let parts: Vec<&str> = m
            .layers
            .iter()
            .map(|d| d.annotations.as_ref().unwrap()[annotations::PART].as_str())
            .collect();
        assert_eq!(parts, ["base", "base", "variant"]);

        m.layers.reverse();
        let layout = m.variant_layout().unwrap();
        assert_eq!(layout.name, "extra");
        assert_eq!(layout.base_digest, "sha256:ff");
        assert_eq!(layout.overlay_digest, "sha256:ee");
        let base: Vec<&str> = layout.base.iter().map(|d| d.digest.as_str()).collect();
        assert_eq!(base, ["sha256:aa", "sha256:bb"]);
        assert_eq!(layout.overlay[0].digest, "sha256:dd");

        // A base layer count the layers contradict is refused.
        m.annotations
            .as_mut()
            .unwrap()
            .insert(annotations::BASE_LAYERS.into(), "1".into());
        assert!(m.variant_layout().is_err());
        m.annotations
            .as_mut()
            .unwrap()
            .insert(annotations::BASE_LAYERS.into(), "3".into());
        assert!(m.variant_layout().is_err());
        // A base manifest is not a variant's.
        let base = build_manifest(
            &sample_set(),
            Descriptor::new(media_types::CONFIG_TEMPLATE_JSON, "sha256:cc", 42),
        );
        assert!(base.variant_layout().is_err());
    }

    /// The base manifest names its variants, their order, the default and
    /// each one's manifest digest; one pushed before variants names none.
    #[test]
    fn base_manifest_names_its_variants() {
        let config = Descriptor::new(media_types::CONFIG_TEMPLATE_JSON, "sha256:cc", 42);
        let mut m = build_manifest(&sample_set(), config);
        assert_eq!(m.variants().unwrap(), (Vec::new(), None));
        let entries = vec![
            VariantEntry {
                name: "standard".into(),
                manifest: "sha256:01".into(),
            },
            VariantEntry {
                name: "win3d".into(),
                manifest: "sha256:02".into(),
            },
        ];
        m.set_variants(&entries, Some("standard"));
        assert_eq!(
            m.variants().unwrap(),
            (entries.clone(), Some("standard".to_string()))
        );
        // The base's own reassembly annotations are untouched.
        assert_eq!(m.whole_digest(), Some("sha256:ff"));
        assert_eq!(m.layers.len(), 2);

        let ann = m.annotations.as_mut().unwrap();
        ann.remove(&format!("{}win3d", annotations::VARIANT_MANIFEST_PREFIX));
        assert!(
            m.variants().is_err(),
            "a named variant without its manifest"
        );
    }
}
