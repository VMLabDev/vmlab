//! Crash cleanup for staging directories shared by multiple vmlab processes.

use std::fs::{self, File, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};

pub(crate) const STAGING_PREFIX: &str = ".staging-";

/// Dropping the temporary directory before the flock keeps it protected
/// through cleanup. Locking the directory itself leaves no lock file behind
/// when the directory is published by rename.
pub(crate) struct StagingDir {
    dir: tempfile::TempDir,
    _lock: Flock<File>,
}

impl StagingDir {
    pub(crate) fn create(root: &Path, prefix: &str, mode: u32) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        // The root lock closes the gap between creating the directory and
        // taking its lifetime lock. The sweep holds the same root lock.
        let _root_lock = lock_root(root)?;
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .rand_bytes(16)
            .permissions(Permissions::from_mode(mode))
            .tempdir_in(root)?;
        let file = open_directory(dir.path())?;
        let lock = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .with_context(|| format!("locking {}", dir.path().display()))?;
        Ok(Self { dir, _lock: lock })
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}

pub(crate) fn sweep(root: &Path, prefix: &str) -> Result<usize> {
    let _root_lock = match File::open(root) {
        Ok(file) => lock_root_file(file, root)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("opening {}", root.display())),
    };
    let entries = fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?;
    Ok(entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(prefix.as_bytes())
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && reap(&entry.path())
        })
        .count())
}

fn reap(path: &Path) -> bool {
    let Ok(file) = open_directory(path) else {
        return false;
    };
    let Ok(_lock) = Flock::lock(file, FlockArg::LockExclusiveNonblock) else {
        return false;
    };
    match fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not remove staging directory");
            false
        }
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)
}

fn lock_root(root: &Path) -> Result<Flock<File>> {
    let file = File::open(root).with_context(|| format!("opening {}", root.display()))?;
    lock_root_file(file, root)
}

fn lock_root_file(file: File, root: &Path) -> Result<Flock<File>> {
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .with_context(|| format!("locking {}", root.display()))
}
