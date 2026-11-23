//! Content-addressed cache for built media images (PRD §6.3).
//!
//! Images are keyed by a digest over the source folder's contents plus the
//! media kind and label, so unchanged folders never rebuild. The lab daemon
//! points this at `<lab>/.vmlab/media`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::config::model::MediaKind;
use crate::media::floppy::build_floppy;
use crate::media::hash::folder_digest;
use crate::media::iso::build_iso;

/// Content-addressed store of built ISO/floppy images.
pub struct MediaCache {
    cache_dir: PathBuf,
}

impl MediaCache {
    /// Creates a cache rooted at `cache_dir`. The directory is created
    /// lazily on the first build.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Returns the path to a built image for `src_folder`, building it only
    /// if no cache entry exists for the folder's current contents.
    ///
    /// Builds go to a temporary name in the cache directory and are renamed
    /// into place, so a crashed build never leaves a half-written entry
    /// under a valid cache name.
    pub fn ensure(
        &self,
        kind: MediaKind,
        src_folder: &Path,
        label: Option<&str>,
    ) -> Result<PathBuf> {
        let digest = folder_digest(src_folder)?;
        let dest = self.cache_dir.join(entry_name(kind, label, &digest));
        if dest.is_file() {
            return Ok(dest);
        }

        fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("creating media cache {}", self.cache_dir.display()))?;
        let tmp = self.cache_dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            dest.file_name()
                .expect("cache entry names are never empty")
                .to_string_lossy()
        ));

        let built = match kind {
            MediaKind::Iso => build_iso(src_folder, &tmp, label),
            MediaKind::Floppy => build_floppy(src_folder, &tmp, label),
        };
        if let Err(err) = built {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }

        fs::rename(&tmp, &dest)
            .with_context(|| format!("installing media cache entry {}", dest.display()))?;
        Ok(dest)
    }

    /// Deletes cache entries whose paths are not in `keep`. Stale temporary
    /// files from interrupted builds are removed as well. (No caller prunes
    /// the cache yet — entries are tiny relative to disks — but the tested
    /// GC is kept for when one does.)
    #[allow(dead_code)]
    pub fn gc(&self, keep: &[PathBuf]) -> Result<()> {
        if !self.cache_dir.is_dir() {
            return Ok(());
        }
        let keep_names: HashSet<OsString> = keep
            .iter()
            .filter_map(|path| path.file_name().map(OsString::from))
            .collect();

        for item in fs::read_dir(&self.cache_dir)
            .with_context(|| format!("reading media cache {}", self.cache_dir.display()))?
        {
            let item =
                item.with_context(|| format!("reading media cache {}", self.cache_dir.display()))?;
            let path = item.path();
            if path.is_file() && !keep_names.contains(&item.file_name()) {
                fs::remove_file(&path)
                    .with_context(|| format!("removing stale cache entry {}", path.display()))?;
            }
        }
        Ok(())
    }
}

/// Cache entry file name: 16 hex characters of a digest over kind, label,
/// and folder contents, plus the kind's extension.
fn entry_name(kind: MediaKind, label: Option<&str>, folder: &[u8; 32]) -> String {
    let (tag, ext) = match kind {
        MediaKind::Iso => ("iso", "iso"),
        MediaKind::Floppy => ("floppy", "img"),
    };
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    hasher.update([0u8]);
    hasher.update(label.unwrap_or_default().as_bytes());
    hasher.update([0u8]);
    hasher.update(folder);
    let digest = hasher.finalize();
    format!("{}.{ext}", hex::encode(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn unattend_folder() -> tempfile::TempDir {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("autounattend.xml"), b"<unattend/>").unwrap();
        src
    }

    #[test]
    fn cache_hit_skips_rebuild() {
        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));

        let first = cache
            .ensure(MediaKind::Iso, src.path(), Some("PAYLOAD"))
            .expect("first build should succeed");
        let mtime = fs::metadata(&first).unwrap().modified().unwrap();

        // A rebuild would bump the mtime even at coarse timestamp
        // granularity after this pause.
        thread::sleep(Duration::from_millis(50));

        let second = cache
            .ensure(MediaKind::Iso, src.path(), Some("PAYLOAD"))
            .expect("cache hit should succeed");
        assert_eq!(first, second);
        assert_eq!(mtime, fs::metadata(&second).unwrap().modified().unwrap());
    }

    #[test]
    fn changed_folder_builds_new_entry() {
        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));

        let first = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();
        fs::write(src.path().join("extra.txt"), b"more").unwrap();
        let second = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();

        assert_ne!(first, second);
        assert!(first.is_file());
        assert!(second.is_file());
    }

    #[test]
    fn kind_and_label_are_part_of_the_key() {
        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));

        let iso = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();
        let floppy = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();
        let labelled = cache
            .ensure(MediaKind::Iso, src.path(), Some("OTHER"))
            .unwrap();

        assert_ne!(iso, floppy);
        assert_ne!(iso, labelled);
        assert_eq!(iso.extension().unwrap(), "iso");
        assert_eq!(floppy.extension().unwrap(), "img");
    }

    /// F1 (docs/reviews/appshare-media.md): drive A is writable, so whoever
    /// reads a built floppy can change the file it was handed. A changed
    /// entry must not be what the next `ensure` returns.
    ///
    /// The comparison is of the sector that was written, not of the whole
    /// image: mtools stamps a volume serial number and file times into each
    /// build, so two builds of one folder are not equal byte for byte.
    #[test]
    fn a_built_floppy_is_not_changed_by_its_reader() {
        use std::os::unix::fs::FileExt;

        const SECTOR: usize = 512;
        // The last sector of a 1.44 MB image: free data area in this build.
        const LAST: u64 = 2879 * SECTOR as u64;
        let last_sector = |img: &Path| {
            let mut sector = [0u8; SECTOR];
            fs::File::open(img)
                .unwrap()
                .read_exact_at(&mut sector, LAST)
                .unwrap();
            sector
        };

        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));
        let first = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();

        // What a guest `write_at` on drive A does to the image.
        fs::OpenOptions::new()
            .write(true)
            .open(&first)
            .unwrap()
            .write_all_at(&[0xEE; SECTOR], LAST)
            .unwrap();

        let second = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();
        let fresh_root = tempfile::tempdir().unwrap();
        let fresh = MediaCache::new(fresh_root.path().join("media"))
            .ensure(MediaKind::Floppy, src.path(), None)
            .unwrap();

        assert!(
            last_sector(&second) == last_sector(&fresh),
            "the cache handed back the sector its reader wrote"
        );
    }

    #[test]
    fn gc_removes_unkept_entries() {
        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));

        let kept = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();
        fs::write(src.path().join("extra.txt"), b"more").unwrap();
        let dropped = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();

        cache.gc(std::slice::from_ref(&kept)).unwrap();
        assert!(kept.is_file());
        assert!(!dropped.exists());
    }
}
