//! `vmlab-gpu-keepawake`, the helper that holds a host GPU out of runtime
//! suspend (PRD §5.2, ADR-0018).
//!
//! The helper is `src/qemu/gpu_keepawake.c`, compiled by `build.rs` and
//! embedded here. The supervisor writes it into a content-addressed
//! directory under the data directory ([`install`]) and runs one per held
//! GPU (`supervisor::keepawake`). The data directory rather than the runtime
//! one for the reason the Vulkan layer lives there: the runtime directory's
//! fallback under `/tmp` is mounted `noexec` on hardened hosts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// The compiled helper.
pub const BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vmlab-gpu-keepawake"));

const FILE: &str = "vmlab-gpu-keepawake";

/// The line prefix the helper prints once its context is current.
pub const READY: &str = "ready ";

/// The helper's path under `data`: in a directory named by its digest, so a
/// file once written never changes and two vmlab builds side by side never
/// write over each other's.
pub fn path(data: &Path) -> PathBuf {
    let digest = hex::encode(Sha256::digest(BINARY));
    data.join("gpu-keepawake").join(&digest[..16]).join(FILE)
}

/// Write the helper to `path` unless it is already there, through a
/// temporary file and a rename, so a helper starting beside this never runs
/// half a file.
pub fn install(path: &Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().expect("the helper lives in a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a file in {}", dir.display()))?;
    std::io::Write::write_all(&mut tmp, BINARY)
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The helper's arguments for the GPU at `address`, exiting once `parent`
/// is no longer its parent.
pub fn args(address: &str, parent: u32) -> Vec<String> {
    vec![
        "--pci".into(),
        address.into(),
        "--parent".into(),
        parent.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded helper is an executable that loads libEGL by name.
    #[test]
    fn the_embedded_helper_is_an_executable_that_dlopens_egl() {
        assert_eq!(&BINARY[..4], b"\x7fELF");
        let needle = b"libEGL.so.1";
        assert!(BINARY.windows(needle.len()).any(|w| w == needle));
    }

    /// The helper is content-addressed under `gpu-keepawake/`, and
    /// installing it twice leaves one executable file.
    #[test]
    fn the_helper_is_installed_once_under_a_content_addressed_directory() {
        let data = tempfile::tempdir().unwrap();
        let p = path(data.path());
        assert!(p.starts_with(data.path().join("gpu-keepawake")));
        assert_eq!(p.file_name().unwrap(), FILE);
        install(&p).unwrap();
        install(&p).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read(&p).unwrap(), BINARY);
    }
}
