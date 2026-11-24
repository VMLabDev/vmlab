//! Content-addressed cache for built media images (PRD §6.3).
//!
//! Images are keyed by a digest over the source folder's contents plus the
//! media kind and label, so unchanged folders never rebuild. The lab daemon
//! points this at `<lab>/.vmlab/media`.
//!
//! An entry is shared: every machine of every instance of the lab with that
//! folder gets the same file. An ISO is attached read-only. A floppy is not,
//! so no machine attaches a floppy entry itself — each takes its own copy
//! ([`private_floppy`]) — and a floppy entry carries a **seal**, the digest of
//! the image as built, that a hit is checked against. An entry that fails the
//! check, or has no seal, is built again.

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
        let sealed = matches!(kind, MediaKind::Floppy);
        if dest.is_file() && (!sealed || seal_holds(&dest)) {
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

        // The seal follows the image: a crash between the two leaves an entry
        // that fails the check and is built again.
        let seal = if sealed {
            Some(image_digest(&tmp)?)
        } else {
            None
        };
        fs::rename(&tmp, &dest)
            .with_context(|| format!("installing media cache entry {}", dest.display()))?;
        if let Some(seal) = seal {
            write_seal(&dest, &seal)?;
        }
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
            // A kept entry keeps its seal.
            let name = item.file_name();
            let entry = match name.to_str().and_then(|n| n.strip_suffix(SEAL_SUFFIX)) {
                Some(entry) => OsString::from(entry),
                None => name,
            };
            if path.is_file() && !keep_names.contains(&entry) {
                fs::remove_file(&path)
                    .with_context(|| format!("removing stale cache entry {}", path.display()))?;
            }
        }
        Ok(())
    }
}

/// What a seal's file name adds to its entry's.
const SEAL_SUFFIX: &str = ".sha256";

/// Where the seal of the entry at `entry` is kept.
fn seal_path(entry: &Path) -> PathBuf {
    let mut name = entry.as_os_str().to_owned();
    name.push(SEAL_SUFFIX);
    PathBuf::from(name)
}

/// The hex SHA-256 of the image at `path`.
fn image_digest(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Whether the entry at `entry` is the image its seal was written for. An
/// entry with no seal, or one that cannot be read, is not.
fn seal_holds(entry: &Path) -> bool {
    let Ok(seal) = fs::read_to_string(seal_path(entry)) else {
        return false;
    };
    image_digest(entry).is_ok_and(|digest| digest == seal.trim())
}

/// Writes the seal of `entry` through a temporary name, so a seal is whole
/// or absent.
fn write_seal(entry: &Path, digest: &str) -> Result<()> {
    let seal = seal_path(entry);
    let dir = entry.parent().expect("a cache entry has a directory");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("writing {}", seal.display()))?;
    std::io::Write::write_all(&mut tmp, digest.as_bytes())
        .with_context(|| format!("writing {}", seal.display()))?;
    tmp.persist(&seal)
        .with_context(|| format!("writing {}", seal.display()))?;
    Ok(())
}

/// The name prefix of a machine's own copy of a built floppy.
const PRIVATE_FLOPPY_PREFIX: &str = "floppy-";

/// The image a machine puts in drive A for the built floppy at `entry`: its
/// own copy under `dir`, the machine's directory, made on first use.
///
/// Drive A is writable and the entry is shared, so the entry itself is never
/// attached. The copy is named after the entry, which is named after the
/// folder's contents: it lasts as long as the machine's disks do, guest
/// writes included, and a changed folder gives a new copy. Copies of earlier
/// builds are removed. Call it only while the machine is stopped.
pub fn private_floppy(entry: &Path, dir: &Path) -> Result<PathBuf> {
    let name = entry
        .file_name()
        .with_context(|| format!("floppy image {} has no file name", entry.display()))?;
    let mut own = OsString::from(PRIVATE_FLOPPY_PREFIX);
    own.push(name);
    let dest = dir.join(&own);
    if !dest.is_file() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("copying floppy image to {}", dest.display()))?;
        fs::copy(entry, tmp.path()).with_context(|| {
            format!(
                "copying floppy image {} to {}",
                entry.display(),
                dest.display()
            )
        })?;
        tmp.persist(&dest)
            .with_context(|| format!("copying floppy image to {}", dest.display()))?;
    }
    for item in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let item = item.with_context(|| format!("reading {}", dir.display()))?;
        let stale = item.file_name() != own
            && item
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(PRIVATE_FLOPPY_PREFIX) && n.ends_with(".img"));
        if stale {
            fs::remove_file(item.path())
                .with_context(|| format!("removing {}", item.path().display()))?;
        }
    }
    Ok(dest)
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

    /// F5 (docs/reviews/appshare-media.md): chmod alone updates the mode
    /// a guest sees through Rock Ridge.
    #[test]
    fn a_mode_change_rebuilds_an_iso() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let src = tempfile::tempdir().unwrap();
        let script = src.path().join("run.sh");
        fs::write(&script, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));
        let first = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();

        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let second = cache.ensure(MediaKind::Iso, src.path(), None).unwrap();
        let extracted = cache_root.path().join("run.sh");
        let output = Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(&second)
            .args(["-extract", "/run.sh"])
            .arg(&extracted)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_ne!(fs::metadata(extracted).unwrap().permissions().mode() & 0o111, 0);
        assert_ne!(first, second);
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
    fn an_unsealed_floppy_entry_is_built_again() {
        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));
        let entry = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();

        // An entry from before seals existed: a guest may have written it.
        fs::remove_file(seal_path(&entry)).unwrap();
        fs::write(&entry, b"left by a guest").unwrap();

        let again = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();
        assert_eq!(entry, again);
        assert_eq!(fs::metadata(&again).unwrap().len(), 2880 * 512);
        assert!(seal_holds(&again));
    }

    #[test]
    fn a_machine_gets_its_own_copy_of_a_built_floppy() {
        let cache = tempfile::tempdir().unwrap();
        let entry = cache.path().join("0123456789abcdef.img");
        fs::write(&entry, b"as built").unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

        let in_a = private_floppy(&entry, a.path()).unwrap();
        let in_b = private_floppy(&entry, b.path()).unwrap();
        assert_ne!(in_a, in_b);
        assert_ne!(in_a, entry);
        assert!(in_a.starts_with(a.path()));

        // The copy outlives a restart, with what the guest wrote to it.
        fs::write(&in_a, b"written by a guest").unwrap();
        assert_eq!(private_floppy(&entry, a.path()).unwrap(), in_a);
        assert_eq!(fs::read(&in_a).unwrap(), b"written by a guest");
        assert_eq!(fs::read(&in_b).unwrap(), b"as built");
        assert_eq!(fs::read(&entry).unwrap(), b"as built");
    }

    #[test]
    fn a_changed_folder_replaces_the_machines_copy() {
        let cache = tempfile::tempdir().unwrap();
        let (old, new) = (cache.path().join("aaaa.img"), cache.path().join("bbbb.img"));
        fs::write(&old, b"old build").unwrap();
        fs::write(&new, b"new build").unwrap();
        let machine = tempfile::tempdir().unwrap();
        fs::write(machine.path().join("disk0.qcow2"), b"").unwrap();

        let first = private_floppy(&old, machine.path()).unwrap();
        let second = private_floppy(&new, machine.path()).unwrap();

        assert_eq!(fs::read(&second).unwrap(), b"new build");
        assert!(!first.exists(), "the copy of the earlier build is removed");
        assert!(machine.path().join("disk0.qcow2").exists());
    }

    #[test]
    fn gc_keeps_the_seal_of_a_kept_entry() {
        let src = unattend_folder();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(cache_root.path().join("media"));

        let kept = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();
        fs::write(src.path().join("extra.txt"), b"more").unwrap();
        let dropped = cache.ensure(MediaKind::Floppy, src.path(), None).unwrap();

        cache.gc(std::slice::from_ref(&kept)).unwrap();
        assert!(seal_holds(&kept));
        assert!(!dropped.exists());
        assert!(!seal_path(&dropped).exists());
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
