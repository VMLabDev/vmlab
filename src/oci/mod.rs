//! OCI registry distribution for vmlab templates (PRD §6.4, §16 #8/#9).
//!
//! Templates distribute as **OCI artifacts** (not container images) through
//! any registry speaking the OCI distribution API. The pieces:
//!
//! - [`media_types`] — the frozen `application/vnd.vmlab.*.v1` media/artifact
//!   type strings (§16 #9).
//! - [`reference`] — `registry/owner/name[:tag]` parsing; an explicit
//!   registry host is always required.
//! - [`chunking`] — split a qcow2 into fixed-size zstd chunks (default
//!   512 MiB) and reassemble + verify on pull (§16 #8).
//! - [`config_blob`] — the template-metadata JSON config blob.
//! - [`manifest`] — OCI manifest / image-index serde types and the
//!   ChunkSet → manifest / multi-arch index construction.
//! - [`auth`] — Docker-style credential reuse and the Bearer token flow.
//! - [`client`] — the async registry client ([`Registry`]) with push/pull.
//! - [`image`] — pulling plain **container images** (Docker-shorthand
//!   references, multi-arch resolution, layer flattening into a squashfs
//!   rootfs) for labs that run containers.
//!
//! `vmlab template login` is [`login`]: it validates a credential against
//! the registry's `/v2/` endpoint, then stores it in the Docker config so a
//! later `push`/`pull` just works.

pub mod auth;
pub mod catalog;
pub mod chunking;
pub mod client;
pub mod config_blob;
pub mod image;
pub mod manifest;
pub mod media_types;
pub mod reference;

use std::path::PathBuf;

use anyhow::Result;

pub use catalog::list_repositories_filtered;
pub use client::{PullProgress, Registry, cached_registry_template, ensure_registry_template};
// No in-crate consumer until the container-runtime wiring lands (`image` has
// the matching module-level dead_code allow).
#[allow(unused_imports)]
pub use image::{ImageCache, ImagePullProgress, PulledImage, ensure_container_image};
pub use reference::with_version_tag;

/// Validate `username`/`password` against `registry`'s `/v2/` endpoint and,
/// on success, persist them into the Docker config (PRD §6.4 — `vmlab
/// template login`). `registry` is a bare host like `ghcr.io`. Returns the
/// path the credential was written to.
pub async fn login(registry: &str, username: &str, password: &str) -> Result<PathBuf> {
    client::HttpTransport::new(
        registry.into(),
        auth::Credential::Basic {
            username: username.into(),
            password: password.into(),
        },
    )?
    .validate_login()
    .await?;
    auth::store_login(registry, username, password)
}

/// Whether Docker-compatible config or a credential helper can resolve a
/// non-anonymous credential for this registry host. Never exposes the secret.
pub fn has_credentials(registry: &str) -> bool {
    matches!(auth::resolve(registry), Ok(auth::Credential::Basic { .. }))
}
