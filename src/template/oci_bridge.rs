//! Bridge from the template CLI to the OCI distribution module (PRD §6.4).

use std::path::Path;

use anyhow::Result;

use super::meta::TemplateMeta;
use super::store::TemplateStore;

pub async fn push(
    template_dir: &Path,
    target: &str,
    chunk_size: u64,
    arch: &str,
    source: Option<&str>,
    moving_tag: Option<&str>,
) -> Result<()> {
    let registry = crate::oci::Registry::new(target)?;
    // Work area on the same filesystem as the store is unnecessary for push
    // (chunks stage anywhere); use a tempdir under the data dir.
    let work = crate::paths::data_dir().join("cache").join("oci-push");
    std::fs::create_dir_all(&work)?;
    registry
        .push(template_dir, chunk_size, arch, &work, source, moving_tag)
        .await
}

pub async fn pull(
    target: &str,
    arch: Option<&str>,
    store: &TemplateStore,
    overwrite: bool,
) -> Result<TemplateMeta> {
    let registry = crate::oci::Registry::new(target)?;
    // Pull staging must share the store's filesystem for the atomic install
    // rename (the OCI module documents this).
    let work = crate::paths::template_store_dir().join(".oci-pull");
    std::fs::create_dir_all(&work)?;
    let meta = registry.pull(arch, store, &work, overwrite).await;
    let _ = std::fs::remove_dir_all(&work);
    meta
}

pub async fn login(registry: &str, user: &str, pass: &str) -> Result<()> {
    crate::oci::login(registry, user, pass).await.map(|_| ())
}

pub fn has_credentials(registry: &str) -> bool {
    crate::oci::has_credentials(registry)
}
