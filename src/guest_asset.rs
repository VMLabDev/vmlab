//! Locate the micro-VM guest boot asset — the kernel + initramfs pair that
//! boots an OCI-container micro-VM (built by `guest/build-asset.sh`, shipped
//! per architecture as `<arch>/{vmlinuz,initramfs.img,VERSION}`).
//!
//! Lookup order:
//!  1. `$VMLAB_GUEST_ASSET_DIR/<arch>/` — explicit override (dev builds point
//!     it at `guest/dist/`).
//!  2. `/usr/share/vmlab/guest/<arch>/` — system-wide install (packages, the
//!     container image).
//!  3. `~/.local/share/vmlab/guest/<arch>/` — the per-user data dir
//!     ([`crate::paths::data_dir`]).
//!
//! No OCI pull yet — that arrives with container orchestration.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Result, bail};

/// Filenames inside `<base>/<arch>/`.
const KERNEL_FILE: &str = "vmlinuz";
const INITRD_FILE: &str = "initramfs.img";
const VERSION_FILE: &str = "VERSION";

/// A resolved guest boot asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestAsset {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// Content of the optional VERSION file (`"unknown"` when absent).
    pub version: String,
}

/// Find the guest asset for `arch` (e.g. `x86_64`, `aarch64`), or fail with
/// every searched path listed.
pub fn ensure_guest_asset(arch: &str) -> Result<GuestAsset> {
    find_in(&candidate_dirs(), arch)
}

/// The base directories searched, in priority order.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os("VMLAB_GUEST_ASSET_DIR").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(PathBuf::from("/usr/share/vmlab/guest"));
    dirs.push(crate::paths::data_dir().join("guest"));
    dirs
}

/// The env-free core of [`ensure_guest_asset`], separated for testability
/// (mutating the environment is `unsafe` in edition 2024 and racy across
/// parallel tests, so tests inject directories instead).
fn find_in(dirs: &[PathBuf], arch: &str) -> Result<GuestAsset> {
    let mut searched = Vec::new();
    for base in dirs {
        let dir = base.join(arch);
        let kernel = dir.join(KERNEL_FILE);
        let initrd = dir.join(INITRD_FILE);
        if kernel.is_file() && initrd.is_file() {
            let version = fs::read_to_string(dir.join(VERSION_FILE))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            return Ok(GuestAsset {
                kernel,
                initrd,
                version,
            });
        }
        searched.push(dir);
    }
    let searched = searched
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no micro-VM guest asset for {arch} (need {KERNEL_FILE} + {INITRD_FILE}); \
         searched: {searched}. Build one with `guest/build-asset.sh {arch}` and \
         install it into one of those directories (or point VMLAB_GUEST_ASSET_DIR \
         at guest/dist)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_asset(dir: &std::path::Path, arch: &str, version: Option<&str>) {
        let d = dir.join(arch);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(KERNEL_FILE), b"kernel").unwrap();
        fs::write(d.join(INITRD_FILE), b"initrd").unwrap();
        if let Some(v) = version {
            fs::write(d.join(VERSION_FILE), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn finds_asset_in_first_matching_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_asset(&b, "x86_64", Some("alpine=3.22 kernel=k cinit=r"));
        // `a` exists but has no asset — search proceeds past it.
        fs::create_dir_all(a.join("x86_64")).unwrap();

        let got = find_in(&[a, b.clone()], "x86_64").unwrap();
        assert_eq!(got.kernel, b.join("x86_64").join(KERNEL_FILE));
        assert_eq!(got.initrd, b.join("x86_64").join(INITRD_FILE));
        assert_eq!(got.version, "alpine=3.22 kernel=k cinit=r");
    }

    #[test]
    fn priority_order_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let hi = tmp.path().join("hi");
        let lo = tmp.path().join("lo");
        write_asset(&hi, "aarch64", None);
        write_asset(&lo, "aarch64", Some("low"));
        let got = find_in(&[hi.clone(), lo], "aarch64").unwrap();
        assert_eq!(got.kernel, hi.join("aarch64").join(KERNEL_FILE));
        assert_eq!(got.version, "unknown"); // VERSION is optional
    }

    #[test]
    fn kernel_without_initrd_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("x86_64");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(KERNEL_FILE), b"kernel").unwrap();
        assert!(find_in(&[tmp.path().to_path_buf()], "x86_64").is_err());
    }

    #[test]
    fn missing_asset_error_lists_searched_paths_and_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("one");
        let b = tmp.path().join("two");
        let err = find_in(&[a.clone(), b.clone()], "riscv64").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains(&a.join("riscv64").display().to_string()),
            "{msg}"
        );
        assert!(
            msg.contains(&b.join("riscv64").display().to_string()),
            "{msg}"
        );
        assert!(msg.contains("build-asset.sh riscv64"), "{msg}");
    }
}
