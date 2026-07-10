//! The digest-addressed container-image cache.
//!
//! Layout under [`crate::paths::oci_cache_dir()`]:
//!
//! ```text
//! .lock                                   exclusive flock for every mutation
//! blobs/sha256/<hex>                      compressed layer blobs (transient:
//!                                         deleted after a successful flatten,
//!                                         kept on failure so retries resume)
//! images/sha256/<manifest-hex>/           one flattened image per manifest
//!     manifest.json                       the raw platform manifest
//!     config.json                         the raw image config blob
//!     rootfs.sqfs                         the flattened root filesystem
//! refs/<host>/<urlencoded-repo>/<tag>     last-resolved manifest digest for
//!                                         a tag (the offline fallback)
//! ```
//!
//! The mutation discipline mirrors the template store
//! ([`crate::template::store`]): reads are lock-free, every mutation holds
//! the flock, and content only ever enters the cache by an atomic
//! `rename(2)` of a fully staged file or directory.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use nix::fcntl::{Flock, FlockArg};

const LOCK_FILE: &str = ".lock";
const STAGING_PREFIX: &str = ".staging-";

/// The raw platform manifest inside an image directory.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The raw image config blob inside an image directory.
pub const CONFIG_FILE: &str = "config.json";
/// The flattened root filesystem inside an image directory.
pub const ROOTFS_FILE: &str = "rootfs.sqfs";

/// Handle on a container-image cache root. Callers normally pass
/// [`crate::paths::oci_cache_dir()`]; tests pass temp dirs.
#[derive(Debug, Clone)]
pub struct ImageCache {
    root: PathBuf,
}

impl ImageCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The cache at its default location, [`crate::paths::oci_cache_dir()`].
    pub fn open_default() -> Self {
        Self::new(crate::paths::oci_cache_dir())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- reads (lock-free) -------------------------------------------------

    /// Where the compressed blob for `digest` lives (whether or not present).
    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.root.join("blobs/sha256").join(digest_hex(digest)?))
    }

    /// Where the image for `manifest_digest` lives (whether or not present).
    pub fn image_dir(&self, manifest_digest: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("images/sha256")
            .join(digest_hex(manifest_digest)?))
    }

    /// The image directory for `manifest_digest` if it is fully installed
    /// (all three files present), else `None`.
    pub fn cached_image(&self, manifest_digest: &str) -> Option<PathBuf> {
        let dir = self.image_dir(manifest_digest).ok()?;
        let complete = [MANIFEST_FILE, CONFIG_FILE, ROOTFS_FILE]
            .iter()
            .all(|f| dir.join(f).is_file());
        complete.then_some(dir)
    }

    /// The manifest digest a tag last resolved to, if recorded.
    pub fn resolved_ref(&self, host: &str, repository: &str, tag: &str) -> Option<String> {
        let text = fs::read_to_string(self.ref_path(host, repository, tag)).ok()?;
        let digest = text.trim();
        (!digest.is_empty()).then(|| digest.to_string())
    }

    // ---- mutations (exclusive flock) ----------------------------------------

    /// Move a fully downloaded, digest-verified blob file into the cache.
    /// `staged` must live on the same filesystem (use [`Self::staging_dir`]).
    pub fn install_blob(&self, staged: &Path, digest: &str) -> Result<PathBuf> {
        let dest = self.blob_path(digest)?;
        let _lock = self.lock()?;
        if dest.is_file() {
            // Someone else won the race; the staged copy is redundant.
            let _ = fs::remove_file(staged);
            return Ok(dest);
        }
        let parent = dest.parent().expect("blob path always has a parent");
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        fs::rename(staged, &dest)
            .with_context(|| format!("cannot move staged blob into {}", dest.display()))?;
        Ok(dest)
    }

    /// Remove the blob for `digest` (after a successful flatten). Missing
    /// blobs are fine — another pull may have consumed them already.
    pub fn remove_blob(&self, digest: &str) -> Result<()> {
        let path = self.blob_path(digest)?;
        let _lock = self.lock()?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
        }
    }

    /// Atomically install a staged image directory (containing
    /// `manifest.json` + `config.json` + `rootfs.sqfs`) as the image for
    /// `manifest_digest`. An already-installed image wins the race — the
    /// content is identical by construction (digest-addressed).
    pub fn install_image(&self, staging: &Path, manifest_digest: &str) -> Result<PathBuf> {
        for f in [MANIFEST_FILE, CONFIG_FILE, ROOTFS_FILE] {
            ensure!(
                staging.join(f).is_file(),
                "staged image directory {} is missing {f}",
                staging.display()
            );
        }
        let dest = self.image_dir(manifest_digest)?;
        let _lock = self.lock()?;
        if dest.exists() {
            let _ = fs::remove_dir_all(staging);
            return Ok(dest);
        }
        let parent = dest.parent().expect("image dir always has a parent");
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        fs::rename(staging, &dest).with_context(|| {
            format!(
                "cannot move staged image into {} (staging must be on the same \
                 filesystem as the cache)",
                dest.display()
            )
        })?;
        Ok(dest)
    }

    /// Record that `host/repository:tag` last resolved to `manifest_digest`
    /// (the offline fallback for tag pulls when the registry is unreachable).
    pub fn record_ref(
        &self,
        host: &str,
        repository: &str,
        tag: &str,
        manifest_digest: &str,
    ) -> Result<()> {
        let path = self.ref_path(host, repository, tag);
        let parent = path.parent().expect("ref path always has a parent");
        let _lock = self.lock()?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        // Stage-then-rename so a torn write can never half-record a digest.
        let tmp = parent.join(format!("{STAGING_PREFIX}{:08x}", rand::random::<u32>()));
        fs::write(&tmp, manifest_digest)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("cannot record tag ref {}", path.display()))?;
        Ok(())
    }

    /// Create a staging directory inside the cache root (same filesystem, so
    /// installs are atomic renames). Removed on drop unless consumed.
    pub fn staging_dir(&self) -> Result<StagingDir> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("cannot create cache root {}", self.root.display()))?;
        StagingDir::create(&self.root)
    }

    fn ref_path(&self, host: &str, repository: &str, tag: &str) -> PathBuf {
        self.root
            .join("refs")
            .join(host)
            .join(encode_repo(repository))
            .join(tag)
    }

    /// Exclusive advisory lock on the cache (same discipline as the
    /// template store). Held for the lifetime of the returned guard.
    fn lock(&self) -> Result<Flock<File>> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("cannot create cache root {}", self.root.display()))?;
        let path = self.root.join(LOCK_FILE);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open lock file {}", path.display()))?;
        Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, errno)| anyhow!("cannot lock image cache: {errno}"))
    }
}

/// The `<hex>` part of a `sha256:<hex>` digest, validated so a digest can
/// never traverse out of the cache directory.
fn digest_hex(digest: &str) -> Result<&str> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!("unsupported digest `{digest}` — only sha256 is supported");
    };
    ensure!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "malformed digest `{digest}`"
    );
    Ok(hex)
}

/// Percent-encode a repository path into one filesystem-safe path segment
/// (`library/nginx` → `library%2Fnginx`).
fn encode_repo(repo: &str) -> String {
    let mut out = String::with_capacity(repo.len());
    for b in repo.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            other => {
                out.push('%');
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    out
}

/// Staging directory inside the cache root, removed on drop unless the
/// install rename already consumed it (mirrors the template store).
pub struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn create(root: &Path) -> Result<Self> {
        let path = root.join(format!("{STAGING_PREFIX}{:08x}", rand::random::<u32>()));
        fs::create_dir_all(&path)
            .with_context(|| format!("cannot create staging dir {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    fn new_cache() -> (tempfile::TempDir, ImageCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(dir.path().join("oci"));
        (dir, cache)
    }

    #[test]
    fn blob_install_remove_round_trip() {
        let (_tmp, cache) = new_cache();
        let digest = digest_of(b"blob bytes");
        assert!(!cache.blob_path(&digest).unwrap().is_file());

        let staging = cache.staging_dir().unwrap();
        let staged = staging.path().join("layer");
        fs::write(&staged, b"blob bytes").unwrap();
        let installed = cache.install_blob(&staged, &digest).unwrap();
        assert!(installed.is_file());
        assert_eq!(fs::read(&installed).unwrap(), b"blob bytes");

        // idempotent second install (race loser) keeps the original
        let staged2 = staging.path().join("layer2");
        fs::write(&staged2, b"blob bytes").unwrap();
        cache.install_blob(&staged2, &digest).unwrap();

        cache.remove_blob(&digest).unwrap();
        assert!(!installed.is_file());
        // removing an already-gone blob is fine
        cache.remove_blob(&digest).unwrap();
    }

    #[test]
    fn image_install_is_atomic_and_complete_only() {
        let (_tmp, cache) = new_cache();
        let digest = digest_of(b"manifest");
        assert!(cache.cached_image(&digest).is_none());

        // an incomplete staging dir refuses to install
        let staging = cache.staging_dir().unwrap();
        fs::write(staging.path().join(MANIFEST_FILE), b"{}").unwrap();
        let err = cache.install_image(staging.path(), &digest).unwrap_err();
        assert!(err.to_string().contains(CONFIG_FILE), "{err}");
        assert!(cache.cached_image(&digest).is_none());

        // complete it and install
        fs::write(staging.path().join(CONFIG_FILE), b"{}").unwrap();
        fs::write(staging.path().join(ROOTFS_FILE), b"sqfs").unwrap();
        let dir = cache.install_image(staging.path(), &digest).unwrap();
        assert_eq!(cache.cached_image(&digest), Some(dir.clone()));
        assert!(!staging.path().exists(), "staging consumed by rename");
        assert_eq!(fs::read(dir.join(ROOTFS_FILE)).unwrap(), b"sqfs");

        // a second install (race loser) keeps the existing image
        let staging2 = cache.staging_dir().unwrap();
        for f in [MANIFEST_FILE, CONFIG_FILE, ROOTFS_FILE] {
            fs::write(staging2.path().join(f), b"other").unwrap();
        }
        cache.install_image(staging2.path(), &digest).unwrap();
        assert_eq!(fs::read(dir.join(ROOTFS_FILE)).unwrap(), b"sqfs");
    }

    #[test]
    fn tag_refs_record_and_resolve() {
        let (_tmp, cache) = new_cache();
        let digest = digest_of(b"m1");
        assert!(
            cache
                .resolved_ref("registry-1.docker.io", "library/nginx", "latest")
                .is_none()
        );
        cache
            .record_ref("registry-1.docker.io", "library/nginx", "latest", &digest)
            .unwrap();
        assert_eq!(
            cache
                .resolved_ref("registry-1.docker.io", "library/nginx", "latest")
                .as_deref(),
            Some(digest.as_str())
        );
        // re-recording (tag moved) replaces
        let digest2 = digest_of(b"m2");
        cache
            .record_ref("registry-1.docker.io", "library/nginx", "latest", &digest2)
            .unwrap();
        assert_eq!(
            cache
                .resolved_ref("registry-1.docker.io", "library/nginx", "latest")
                .as_deref(),
            Some(digest2.as_str())
        );
        // repository slashes are encoded, not treated as directories
        let refs_dir = cache.root().join("refs/registry-1.docker.io");
        assert!(refs_dir.join("library%2Fnginx/latest").is_file());
    }

    #[test]
    fn malformed_digests_rejected() {
        let (_tmp, cache) = new_cache();
        assert!(cache.blob_path("sha256:short").is_err());
        assert!(cache.blob_path("md5:aaaa").is_err());
        assert!(cache.image_dir("sha256:../../../etc/passwd").is_err());
        let traversal = format!("sha256:{}", "a".repeat(63) + "/");
        assert!(cache.blob_path(&traversal).is_err());
    }

    #[test]
    fn staging_dir_cleans_up_on_drop() {
        let (_tmp, cache) = new_cache();
        let path = {
            let staging = cache.staging_dir().unwrap();
            fs::write(staging.path().join("x"), b"y").unwrap();
            staging.path().to_path_buf()
        };
        assert!(!path.exists());
    }
}
