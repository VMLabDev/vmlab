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
}
