//! Bridge from the template CLI to the OCI distribution module (PRD §6.4).

use std::path::Path;

use anyhow::Result;

use super::meta::TemplateMeta;
use super::store::TemplateStore;
use crate::staging::StagingDir;

pub(crate) const PULL_WORK_SUBDIR: &str = ".oci-pull";
pub(crate) const PUSH_WORK_SUBDIR: &str = "cache/oci-push";
pub(crate) const WORK_PREFIX: &str = "op-";

pub async fn push(
    template_dir: &Path,
    target: &str,
    chunk_size: u64,
    arch: &str,
    source: Option<&str>,
    moving_tag: Option<&str>,
) -> Result<crate::oci::client::PushReport> {
    let registry = crate::oci::Registry::new(target)?;
    // Chunks stage anywhere, but each push needs its own directory: they are
    // named by index, so two pushes sharing one would upload each other's
    // bytes under their own digests and the registry would refuse them.
    let work = fresh_work_dir(&crate::paths::data_dir().join(PUSH_WORK_SUBDIR))?;
    registry
        .push(
            template_dir,
            chunk_size,
            arch,
            work.path(),
            source,
            moving_tag,
        )
        .await
}

/// A work directory of its own under `base`, removed when dropped. Concurrent
/// pushes and pulls each get one, so none can overwrite or delete another's
/// staged files.
pub(crate) fn fresh_work_dir(base: &Path) -> Result<StagingDir> {
    StagingDir::create(base, WORK_PREFIX, 0o700)
}

/// Pull `target` — a registry reference, maybe ending `#<variant>` — into
/// `store`: the base, its default variant, and the variant named or, with
/// `all_variants`, every one (§6.6). `in_use` is asked before a variant
/// overlay the store holds is replaced by a rebuilt one.
pub async fn pull(
    target: &str,
    arch: Option<&str>,
    store: &TemplateStore,
    overwrite: bool,
    all_variants: bool,
    in_use: super::build::InUse,
) -> Result<TemplateMeta> {
    let (reference, variant) =
        crate::config::model::split_variant(target).map_err(anyhow::Error::msg)?;
    let want = if all_variants {
        crate::oci::client::VariantWant::all()
    } else {
        crate::oci::client::VariantWant::of(variant.as_ref())
    };
    let registry = crate::oci::Registry::new(reference)?;
    // Pull staging must share the store's filesystem for the atomic install
    // rename (the OCI module documents this), and each pull gets its own
    // directory so a concurrent one cannot delete it.
    let work = fresh_work_dir(&store.root().join(PULL_WORK_SUBDIR))?;
    let work = std::sync::Arc::new(work);
    registry
        .guarding(in_use)
        .holding(work.clone())
        .pull_with_progress(arch, store, work.path(), overwrite, &want, &mut |_| {})
        .await
}

pub async fn login(registry: &str, user: &str, pass: &str) -> Result<()> {
    crate::oci::login(registry, user, pass).await.map(|_| ())
}

pub fn has_credentials(registry: &str) -> bool {
    crate::oci::has_credentials(registry)
}

#[cfg(test)]
mod tests {
    use super::fresh_work_dir;

    /// Every operation gets a directory of its own, and it is gone once the
    /// operation lets go of it, so concurrent pushes and pulls never share
    /// staged chunks.
    #[test]
    fn each_operation_gets_its_own_work_dir() {
        let base = tempfile::tempdir().unwrap();
        let a = fresh_work_dir(base.path()).unwrap();
        let b = fresh_work_dir(base.path()).unwrap();
        assert_ne!(a.path(), b.path());
        assert!(a.path().starts_with(base.path()) && b.path().starts_with(base.path()));
        let gone = a.path().to_path_buf();
        drop(a);
        assert!(!gone.exists());
        assert!(b.path().exists());
    }
}
