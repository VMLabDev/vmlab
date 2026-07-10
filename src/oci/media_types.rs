//! Frozen OCI media / artifact type strings (PRD §6.4, §16 decision #9).
//!
//! Templates distribute as **OCI artifacts, not container images**: the
//! manifest carries a vmlab-specific `artifactType`, the config blob is the
//! template metadata, and the layer blobs are zstd-compressed qcow2 chunks.
//! These strings are part of the on-the-wire contract — once a public push
//! has happened they must not change (§16 #9: "freeze before first public
//! push"). Treat every constant here as immutable.

/// Manifest `artifactType` marking a manifest as a vmlab template. A
/// `docker pull` / `docker run` against such a reference must fail fast as
/// "not a container image"; `vmlab template pull` refuses anything whose
/// `artifactType` is not exactly this string.
pub const ARTIFACT_TYPE_TEMPLATE: &str = "application/vnd.vmlab.template.v1";

/// Config-blob media type. The config blob carries the template metadata
/// (the recorded hardware / profile / provenance) serialised as JSON — a
/// stable, registry-friendly encoding independent of the on-disk WCL
/// representation. We deliberately use `+json` (not `+wcl`) so generic
/// tooling can introspect the config without a WCL parser.
pub const CONFIG_TEMPLATE_JSON: &str = "application/vnd.vmlab.template.config.v1+json";

/// Layer media type for one zstd-compressed qcow2 chunk.
pub const CHUNK_ZSTD: &str = "application/vnd.vmlab.template.chunk.v1+zstd";

/// Standard OCI image manifest media type.
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Standard OCI image index (multi-arch) media type.
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// Docker schema-2 image manifest media type. Accepted on *pull* so plain
/// Docker Hub container images resolve; vmlab never emits it.
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Docker schema-2 manifest list (multi-arch index) media type. Accepted on
/// pull only, like [`DOCKER_MANIFEST`].
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire contract is frozen: these exact bytes ship in public
    /// manifests. A change here is a breaking change and this test exists
    /// to make that change loud and deliberate.
    #[test]
    fn media_types_are_frozen() {
        assert_eq!(ARTIFACT_TYPE_TEMPLATE, "application/vnd.vmlab.template.v1");
        assert_eq!(
            CONFIG_TEMPLATE_JSON,
            "application/vnd.vmlab.template.config.v1+json"
        );
        assert_eq!(CHUNK_ZSTD, "application/vnd.vmlab.template.chunk.v1+zstd");
        assert_eq!(OCI_MANIFEST, "application/vnd.oci.image.manifest.v1+json");
        assert_eq!(OCI_INDEX, "application/vnd.oci.image.index.v1+json");
        assert_eq!(
            DOCKER_MANIFEST,
            "application/vnd.docker.distribution.manifest.v2+json"
        );
        assert_eq!(
            DOCKER_MANIFEST_LIST,
            "application/vnd.docker.distribution.manifest.list.v2+json"
        );
    }
}
