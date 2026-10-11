//! ISO image building from folders (PRD §6.3).
//!
//! Prefers `xorriso -as mkisofs`, falling back to `genisoimage` and then
//! `mkisofs` when xorriso is absent. Images are built with Joliet and Rock
//! Ridge extensions so both Windows unattend media and Linux payloads read
//! correctly.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

/// Maximum ISO 9660 volume identifier length.
const ISO_LABEL_MAX: usize = 32;

/// Builds an ISO image at `out` from the contents of `src_folder`.
pub fn build_iso(src_folder: &Path, out: &Path, label: Option<&str>) -> Result<()> {
    ensure!(
        src_folder.is_dir(),
        "ISO source {} is not a directory",
        src_folder.display()
    );
    if let Some(label) = label {
        validate_iso_label(label)?;
    }
    // A symlinked source folder, and links inside it, are taken as what they
    // point at, as the media hash takes them: with Rock Ridge a link would go
    // into the image as a link, which a guest reading plain ISO 9660 (DOS,
    // Windows 9x) sees as an empty file.
    let src_folder = &src_folder
        .canonicalize()
        .with_context(|| format!("resolving {}", src_folder.display()))?;

    // (program, leading args) in preference order.
    let tools: &[(&str, &[&str])] = &[
        ("xorriso", &["-as", "mkisofs"]),
        ("genisoimage", &[]),
        ("mkisofs", &[]),
    ];

    for (program, lead) in tools {
        let mut cmd = Command::new(program);
        cmd.args(*lead);
        cmd.arg("-o").arg(out);
        cmd.args(["-J", "-R", "-f"]);
        if let Some(label) = label {
            cmd.arg("-V").arg(label);
        }
        cmd.arg(src_folder);

        let output = match cmd.output() {
            Ok(output) => output,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| format!("running {program}")),
        };

        if !output.status.success() {
            bail!(
                "{program} failed building ISO from {} ({}): {}",
                src_folder.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(());
    }

    bail!("no ISO building tool found: install xorriso (preferred), genisoimage, or mkisofs")
}

/// Validates an ISO volume label: non-empty, at most 32 printable ASCII
/// characters.
fn validate_iso_label(label: &str) -> Result<()> {
    ensure!(!label.is_empty(), "ISO label must not be empty");
    ensure!(
        label.len() <= ISO_LABEL_MAX,
        "ISO label {label:?} is {} characters; the maximum is {ISO_LABEL_MAX}",
        label.len()
    );
    ensure!(
        label.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
        "ISO label {label:?} contains characters outside printable ASCII"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    /// Lists every path inside an ISO via `xorriso -indev <iso> -find`.
    fn iso_listing(iso: &Path) -> String {
        let output = Command::new("xorriso")
            .arg("-indev")
            .arg(iso)
            .arg("-find")
            .output()
            .expect("xorriso should be runnable");
        assert!(output.status.success(), "xorriso -find failed: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn builds_iso_with_nested_files() {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("autounattend.xml"), b"<unattend/>").unwrap();
        fs::create_dir_all(src.path().join("drivers/net")).unwrap();
        fs::write(src.path().join("drivers/net/virtio.inf"), b"[Version]").unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let iso = out_dir.path().join("media.iso");
        build_iso(src.path(), &iso, Some("UNATTEND")).expect("ISO build should succeed");

        assert!(iso.is_file());
        let listing = iso_listing(&iso);
        assert!(listing.contains("autounattend.xml"), "listing: {listing}");
        assert!(listing.contains("drivers"), "listing: {listing}");
        assert!(listing.contains("virtio.inf"), "listing: {listing}");
    }

    /// A symlinked folder, and a symlinked file inside it, go into the image
    /// as their contents, not as links: DOS reads no Rock Ridge.
    #[test]
    fn follows_symlinks_to_their_contents() {
        let real = tempfile::tempdir().unwrap();
        fs::write(real.path().join("payload.txt"), b"payload").unwrap();
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join("linked.txt"), b"linked").unwrap();
        std::os::unix::fs::symlink(
            other.path().join("linked.txt"),
            real.path().join("link.txt"),
        )
        .unwrap();
        let links = tempfile::tempdir().unwrap();
        let src = links.path().join("src");
        std::os::unix::fs::symlink(real.path(), &src).unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let iso = out_dir.path().join("media.iso");
        build_iso(&src, &iso, Some("LINKED")).expect("ISO build should succeed");

        let listing = iso_listing(&iso);
        assert!(listing.contains("payload.txt"), "listing: {listing}");
        assert!(listing.contains("link.txt"), "listing: {listing}");
        let extracted = out_dir.path().join("link.txt");
        let status = Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(&iso)
            .args(["-extract", "/link.txt"])
            .arg(&extracted)
            .output()
            .expect("xorriso should be runnable");
        assert!(status.status.success(), "{status:?}");
        assert!(
            !fs::symlink_metadata(&extracted)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&extracted).unwrap(), b"linked");
    }

    #[test]
    fn rejects_missing_source_folder() {
        let out_dir = tempfile::tempdir().unwrap();
        let err = build_iso(
            Path::new("/nonexistent/vmlab-test-src"),
            &out_dir.path().join("x.iso"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn rejects_overlong_label() {
        let src = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();
        let long = "X".repeat(ISO_LABEL_MAX + 1);
        let err = build_iso(src.path(), &out_dir.path().join("x.iso"), Some(&long)).unwrap_err();
        assert!(err.to_string().contains("maximum is 32"));
    }
}
