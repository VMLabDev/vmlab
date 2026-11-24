//! Content digest of a media source folder.
//!
//! The digest covers each entry's relative path and, for files, its length
//! and bytes. ISO digests also cover modes because Rock Ridge records them.
//! Paths are sorted for a deterministic result. Timestamps and ownership
//! are excluded (PRD §6.3).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

/// One entry in the folder walk: relative path (always `/`-separated) and
/// whether it is a directory.
struct Entry {
    rel: String,
    is_dir: bool,
}

/// Computes the sha256 content digest of `src_folder`.
///
/// Pass `include_modes` for images that preserve Unix permissions.
pub fn folder_digest(src_folder: &Path, include_modes: bool) -> Result<[u8; 32]> {
    ensure!(
        src_folder.is_dir(),
        "media source {} is not a directory",
        src_folder.display()
    );

    let mut entries = Vec::new();
    collect(src_folder, "", &mut entries)
        .with_context(|| format!("walking media source {}", src_folder.display()))?;
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut hasher = Sha256::new();
    if include_modes {
        hasher.update(mode_bytes(src_folder)?);
    }
    for entry in &entries {
        let abs = src_folder.join(&entry.rel);
        if entry.is_dir {
            hasher.update(b"D");
            hasher.update(entry.rel.as_bytes());
            hasher.update([0u8]);
            if include_modes {
                hasher.update(mode_bytes(&abs)?);
            }
        } else {
            let mut file = fs::File::open(&abs)
                .with_context(|| format!("opening {} for hashing", abs.display()))?;
            let metadata = file
                .metadata()
                .with_context(|| format!("reading metadata of {}", abs.display()))?;
            hasher.update(b"F");
            hasher.update(entry.rel.as_bytes());
            hasher.update([0u8]);
            if include_modes {
                hasher.update(metadata.permissions().mode().to_le_bytes());
            }
            hasher.update(metadata.len().to_le_bytes());
            crate::hashing::feed(&mut file, &mut hasher)
                .with_context(|| format!("hashing contents of {}", abs.display()))?;
        }
    }
    Ok(hasher.finalize().into())
}

fn mode_bytes(path: &Path) -> Result<[u8; 4]> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading metadata of {}", path.display()))?;
    Ok(metadata.permissions().mode().to_le_bytes())
}

/// Recursively collects entries under `root`, recording paths relative to
/// the original source folder with `/` separators.
fn collect(root: &Path, prefix: &str, out: &mut Vec<Entry>) -> Result<()> {
    let dir = root.join(prefix);
    for item in
        fs::read_dir(&dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let item = item.with_context(|| format!("reading directory {}", dir.display()))?;
        let name = item.file_name();
        let name = name.to_string_lossy();
        let rel = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        // Follows symlinks: a link to a file hashes as that file's contents.
        let is_dir = item.path().is_dir();
        if is_dir {
            collect(root, &rel, out)?;
        }
        out.push(Entry { rel, is_dir });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use super::*;

    fn digest(dir: &Path) -> [u8; 32] {
        folder_digest(dir, false).expect("digest should succeed")
    }

    #[test]
    fn identical_content_same_digest() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [a.path(), b.path()] {
            fs::write(dir.join("hello.txt"), b"hi there").unwrap();
        }
        assert_eq!(digest(a.path()), digest(b.path()));
    }

    #[test]
    fn different_content_different_digest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), b"version one").unwrap();
        let before = digest(dir.path());
        fs::write(dir.path().join("hello.txt"), b"version two").unwrap();
        assert_ne!(before, digest(dir.path()));
    }

    #[test]
    fn renamed_file_different_digest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("old.txt"), b"same bytes").unwrap();
        let before = digest(dir.path());
        fs::rename(dir.path().join("old.txt"), dir.path().join("new.txt")).unwrap();
        assert_ne!(before, digest(dir.path()));
    }

    #[test]
    fn touched_mtime_same_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("stable.txt");
        fs::write(&file, b"unchanging").unwrap();
        let before = digest(dir.path());

        let handle = fs::File::options().write(true).open(&file).unwrap();
        handle
            .set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        drop(handle);

        assert_eq!(before, digest(dir.path()));
    }

    #[test]
    fn nested_dirs_affect_digest() {
        let flat = tempfile::tempdir().unwrap();
        fs::write(flat.path().join("payload.txt"), b"cargo").unwrap();

        let nested = tempfile::tempdir().unwrap();
        fs::create_dir_all(nested.path().join("a/b")).unwrap();
        fs::write(nested.path().join("a/b/payload.txt"), b"cargo").unwrap();

        assert_ne!(digest(flat.path()), digest(nested.path()));

        // And a second nested tree with the same shape matches.
        let nested2 = tempfile::tempdir().unwrap();
        fs::create_dir_all(nested2.path().join("a/b")).unwrap();
        fs::write(nested2.path().join("a/b/payload.txt"), b"cargo").unwrap();
        assert_eq!(digest(nested.path()), digest(nested2.path()));
    }

    #[test]
    fn non_directory_source_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        assert!(folder_digest(&file, false).is_err());
    }
}
