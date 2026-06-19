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
//!
//! `vmlab template login` is [`login`]: it validates a credential against
//! the registry's `/v2/` endpoint, then stores it in the Docker config so a
//! later `push`/`pull` just works.

// Buildout in progress: the CLI consumers of these re-exports land later
// (the crate root's dead_code allow does not cover unused imports).
#![allow(unused_imports)]

pub mod auth;
pub mod catalog;
pub mod chunking;
pub mod client;
pub mod config_blob;
pub mod manifest;
pub mod media_types;
pub mod reference;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub use catalog::list_repositories;
pub use chunking::{ChunkInfo, ChunkSet, DEFAULT_CHUNK_SIZE};
pub use client::{Registry, Transport};
pub use manifest::{Descriptor, ImageIndex, Manifest};
pub use media_types::ARTIFACT_TYPE_TEMPLATE;
pub use reference::{Reference, with_version_tag};

/// Validate `username`/`password` against `registry`'s `/v2/` endpoint and,
/// on success, persist them into the Docker config (PRD §6.4 — `vmlab
/// template login`). `registry` is a bare host like `ghcr.io`. Returns the
/// path the credential was written to.
pub async fn login(registry: &str, username: &str, password: &str) -> Result<PathBuf> {
    use base64::Engine as _;
    let scheme = if registry.starts_with("localhost") || registry.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    };
    let url = format!("{scheme}://{registry}/v2/");
    let client = reqwest::Client::builder()
        .user_agent("vmlab-oci/1")
        .build()
        .context("cannot build HTTP client")?;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));

    // First request may come back 401 with a Bearer challenge; satisfy it
    // to actually exercise the credential.
    let resp = client
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"))
        .send()
        .await
        .context("registry login request failed")?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(challenge) = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(auth::parse_bearer_challenge)
        {
            let mut req = client.get(&challenge.realm);
            let mut query = Vec::new();
            if let Some(service) = &challenge.service {
                query.push(("service", service.clone()));
            }
            if !query.is_empty() {
                req = req.query(&query);
            }
            let token_resp = req
                .header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"))
                .send()
                .await
                .context("token request failed")?;
            if !token_resp.status().is_success() {
                bail!("login failed: registry rejected the credential");
            }
        } else {
            bail!("login failed: registry rejected the credential");
        }
    } else if !resp.status().is_success() {
        bail!(
            "login failed: registry {registry} returned {}",
            resp.status()
        );
    }

    auth::store_login(registry, username, password)
}
