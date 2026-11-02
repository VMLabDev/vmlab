//! The SeaBIOS splash embedded in the CLI and staged beside a VM's VARS.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const JPEG: &[u8] = include_bytes!("../../guest/logo/bootsplash.jpg");

/// Atomically stage this build's splash without rewriting an identical copy.
pub fn install(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("bootsplash.jpg");
    if std::fs::read(&path).is_ok_and(|bytes| bytes == JPEG) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, JPEG)?;
    tmp.persist(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_and_updates_the_embedded_splash() {
        let dir = tempfile::tempdir().unwrap();
        let path = install(dir.path()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), JPEG);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        install(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        std::fs::write(&path, b"old splash").unwrap();
        install(dir.path()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), JPEG);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
