//! Thin async wrappers over the `qemu-img` binary: blank disks, linked
//! clones (PRD §7.1), image inspection and resize.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::process::Command;

/// Failures from a `qemu-img` invocation.
#[derive(Debug, thiserror::Error)]
pub enum QemuImgError {
    #[error("cannot run qemu-img (is QEMU installed?): {0}")]
    Spawn(#[source] std::io::Error),
    #[error("{command} failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("cannot parse `qemu-img info` output for {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot resolve backing file {path}: {source}")]
    Backing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, QemuImgError>;

/// Subset of `qemu-img info --output=json` vmlab cares about.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageInfo {
    /// Guest-visible disk size in bytes.
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
    /// Bytes actually allocated on the host (the sparseness test reads it).
    #[allow(dead_code)]
    #[serde(rename = "actual-size")]
    pub actual_size: u64,
    /// Backing image, present for linked clones.
    #[serde(rename = "backing-filename", default)]
    pub backing_file: Option<PathBuf>,
}

async fn run(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("qemu-img")
        .args(args)
        .output()
        .await
        .map_err(QemuImgError::Spawn)?;
    if !output.status.success() {
        return Err(QemuImgError::Failed {
            command: format!("qemu-img {}", args.join(" ")),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Create a blank qcow2 of `size_bytes` (PRD §6.5 scratch disks).
pub async fn create_blank(path: &Path, size_bytes: u64) -> Result<()> {
    let path = path_str(path);
    let size = size_bytes.to_string();
    run(&["create", "-f", "qcow2", &path, &size]).await?;
    Ok(())
}

/// Create a qcow2 linked clone at `dest` backed by `backing` (PRD §7.1).
/// The backing path is recorded absolute so the clone works from any cwd.
pub async fn create_linked_clone(backing: &Path, dest: &Path) -> Result<()> {
    let backing = backing.canonicalize().map_err(|e| QemuImgError::Backing {
        path: backing.to_path_buf(),
        source: e,
    })?;
    let backing = path_str(&backing);
    let dest = path_str(dest);
    run(&[
        "create", "-f", "qcow2", "-b", &backing, "-F", "qcow2", &dest,
    ])
    .await?;
    Ok(())
}

/// Inspect an image via `qemu-img info --output=json`.
pub async fn image_info(path: &Path) -> Result<ImageInfo> {
    let path_arg = path_str(path);
    let stdout = run(&["info", "--output=json", &path_arg]).await?;
    serde_json::from_slice(&stdout).map_err(|e| QemuImgError::Parse {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Resize an image to `new_size` bytes (growing only, qcow2 shrink needs
/// `--shrink` and is deliberately not exposed).
pub async fn resize(path: &Path, new_size: u64) -> Result<()> {
    let path = path_str(path);
    let size = new_size.to_string();
    run(&["resize", &path, &size]).await?;
    Ok(())
}

/// Convert any image (e.g. a raw FAT volume) to qcow2.
pub async fn convert_to_qcow2(src: &Path, dest: &Path) -> Result<()> {
    let src = path_str(src);
    let dest = path_str(dest);
    run(&["convert", "-O", "qcow2", &src, &dest]).await?;
    Ok(())
}

/// Create a qcow2-internal snapshot on a powered-off image (PRD §7.3
/// offline snapshots).
pub async fn snapshot_create(path: &Path, name: &str) -> Result<()> {
    let path = path_str(path);
    run(&["snapshot", "-c", name, &path]).await?;
    Ok(())
}

/// Apply (revert to) a qcow2-internal snapshot.
pub async fn snapshot_apply(path: &Path, name: &str) -> Result<()> {
    let path = path_str(path);
    run(&["snapshot", "-a", name, &path]).await?;
    Ok(())
}

/// Delete a qcow2-internal snapshot.
pub async fn snapshot_delete(path: &Path, name: &str) -> Result<()> {
    let path = path_str(path);
    run(&["snapshot", "-d", name, &path]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when `qemu-img` is on PATH; tests skip (with a note) otherwise.
    fn have_qemu_img() -> bool {
        let found = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qemu-img").is_file())
        });
        if !found {
            eprintln!("skipping: qemu-img not found on PATH");
        }
        found
    }

    #[tokio::test]
    async fn blank_create_and_info() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.qcow2");
        create_blank(&disk, 64 << 20).await.unwrap();
        let info = image_info(&disk).await.unwrap();
        assert_eq!(info.virtual_size, 64 << 20);
        assert!(info.backing_file.is_none());
        assert!(info.actual_size < 64 << 20, "blank qcow2 must be sparse");
    }

    #[tokio::test]
    async fn linked_clone_records_absolute_backing() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        let clone = dir.path().join("clone.qcow2");
        create_blank(&base, 32 << 20).await.unwrap();
        create_linked_clone(&base, &clone).await.unwrap();
        let info = image_info(&clone).await.unwrap();
        assert_eq!(info.virtual_size, 32 << 20, "clone inherits virtual size");
        let backing = info.backing_file.expect("clone must have a backing file");
        assert!(
            backing.is_absolute(),
            "backing path must be absolute: {backing:?}"
        );
        assert_eq!(backing, base.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn resize_grows() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.qcow2");
        create_blank(&disk, 16 << 20).await.unwrap();
        resize(&disk, 64 << 20).await.unwrap();
        assert_eq!(image_info(&disk).await.unwrap().virtual_size, 64 << 20);
    }

    #[tokio::test]
    async fn failure_carries_stderr() {
        if !have_qemu_img() {
            return;
        }
        let err = image_info(Path::new("/nonexistent/nope.qcow2"))
            .await
            .unwrap_err();
        match err {
            QemuImgError::Failed {
                stderr, command, ..
            } => {
                assert!(!stderr.is_empty(), "stderr should be captured");
                assert!(command.starts_with("qemu-img info"), "{command}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn linked_clone_missing_backing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = create_linked_clone(
            &dir.path().join("missing.qcow2"),
            &dir.path().join("clone.qcow2"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, QemuImgError::Backing { .. }), "{err:?}");
    }
}
