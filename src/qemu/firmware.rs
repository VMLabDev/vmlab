//! Locate firmware blobs (OVMF/SeaBIOS/AAVMF) across distro layouts.
//! Exact paths vary per distribution; we search the well-known spots and
//! fail with an actionable error listing what was tried.
//!
//! Discovery probes the host filesystem, so it happens once per machine
//! start, where the lab daemon assembles the runtime paths — never inside
//! [`super::cmdline::build_args`], which takes the resolved image as an
//! injected path (see ADR-0008).

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// A resolved UEFI firmware pair: read-only CODE image plus a pristine VARS
/// template to copy per VM.
#[derive(Debug, Clone)]
pub struct UefiFirmware {
    pub code: PathBuf,
    pub vars_template: PathBuf,
}

/// The well-known CODE/VARS locations for one arch, most specific first.
/// Data, not lookup: [`lookup_under`] is what touches the filesystem.
fn candidates(arch: &str) -> Result<(&'static [&'static str], &'static [&'static str])> {
    Ok(match arch {
        // OVMF for x86_64. Secure boot has its own pairs: see
        // [`SECURE_BOOT_X86_64`].
        "x86_64" => (
            &[
                "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
                "/usr/share/edk2/ovmf/OVMF_CODE.fd",
                "/usr/share/OVMF/OVMF_CODE_4M.fd",
                "/usr/share/OVMF/OVMF_CODE.fd",
                "/usr/share/edk2-ovmf/OVMF_CODE.fd",
                "/usr/share/qemu/ovmf-x86_64-code.bin",
            ][..],
            &[
                "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
                "/usr/share/edk2/ovmf/OVMF_VARS.fd",
                "/usr/share/OVMF/OVMF_VARS_4M.fd",
                "/usr/share/OVMF/OVMF_VARS.fd",
                "/usr/share/edk2-ovmf/OVMF_VARS.fd",
                "/usr/share/qemu/ovmf-x86_64-vars.bin",
            ][..],
        ),
        // UEFI for aarch64 (QEMU_EFI / AAVMF).
        "aarch64" => (
            &[
                "/usr/share/edk2/aarch64/QEMU_CODE.fd",
                "/usr/share/edk2/aarch64/QEMU_EFI.fd",
                "/usr/share/AAVMF/AAVMF_CODE.fd",
                "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
            ][..],
            &[
                "/usr/share/edk2/aarch64/QEMU_VARS.fd",
                "/usr/share/AAVMF/AAVMF_VARS.fd",
            ][..],
        ),
        // UEFI for riscv64 (EDK2 RiscVVirt). The `virt` machine takes the
        // CODE image on pflash unit 0 and the writable VARS on unit 1,
        // exactly like aarch64; the packaged blobs are already padded to the
        // 32 MiB pflash size QEMU requires (copied verbatim per VM, so the
        // size is preserved).
        "riscv64" => (
            &[
                "/usr/share/qemu-efi-riscv64/RISCV_VIRT_CODE.fd",
                "/usr/share/edk2/riscv64/RISCV_VIRT_CODE.fd",
                "/usr/share/edk2/riscv/RISCV_VIRT_CODE.fd",
            ][..],
            &[
                "/usr/share/qemu-efi-riscv64/RISCV_VIRT_VARS.fd",
                "/usr/share/edk2/riscv64/RISCV_VIRT_VARS.fd",
                "/usr/share/edk2/riscv/RISCV_VIRT_VARS.fd",
            ][..],
        ),
        other => return Err(anyhow!("no UEFI firmware lookup for arch {other}")),
    })
}

/// The x86_64 secure-boot builds, each with the VARS template that has keys
/// enrolled for it — Microsoft's, which Windows and shim-signed Linux boot
/// under. A blank VARS leaves the firmware in setup mode, where it verifies
/// nothing, so the pair is searched as a pair: CODE and VARS sizes must
/// match, and a secboot CODE without its enrolled VARS is no answer.
const SECURE_BOOT_X86_64: &[(&str, &str)] = &[
    // Debian, Ubuntu (`ovmf`).
    (
        "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
        "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
    ),
    (
        "/usr/share/OVMF/OVMF_CODE.secboot.fd",
        "/usr/share/OVMF/OVMF_VARS.ms.fd",
    ),
    // Fedora, RHEL (`edk2-ovmf`).
    (
        "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
        "/usr/share/edk2/ovmf/OVMF_VARS.secboot.fd",
    ),
];

/// Secure-boot CODE builds shipped without an enrolled VARS beside them —
/// Arch's `edk2-ovmf` is one. Named in the error so a host carrying one is
/// told what is missing rather than that it has no firmware.
const SECURE_BOOT_X86_64_CODE_ONLY: &[&str] = &["/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd"];

/// The UEFI CODE/VARS pair for a QEMU arch (`x86_64`, `aarch64`, `riscv64`).
/// `secure_boot` selects an x86_64 secboot build with keys enrolled; other
/// arches have no secure-boot variant.
pub fn lookup(arch: &str, secure_boot: bool) -> Result<UefiFirmware> {
    lookup_under(Path::new("/"), arch, secure_boot)
}

/// [`lookup`] against an alternate filesystem root — the candidate paths are
/// absolute, so `root` is joined onto each. Tests lay out a fake distro
/// under a temp dir, which is what makes candidate ordering and the
/// secure-boot variant testable on a host with no edk2 at all.
fn lookup_under(root: &Path, arch: &str, secure_boot: bool) -> Result<UefiFirmware> {
    let at = |c: &str| root.join(c.trim_start_matches('/'));
    if secure_boot && arch == "x86_64" {
        return secure_boot_x86_64(&at);
    }
    let (code_candidates, vars_candidates) = candidates(arch)?;
    let first_existing = |candidates: &[&str]| -> Option<PathBuf> {
        candidates.iter().map(|c| at(c)).find(|p| p.is_file())
    };
    let code = first_existing(code_candidates).ok_or_else(|| {
        anyhow!(
            "{arch} UEFI firmware not found; tried: {}",
            code_candidates.join(", ")
        )
    })?;
    let vars_template = first_existing(vars_candidates).ok_or_else(|| {
        anyhow!(
            "{arch} UEFI VARS template not found; tried: {}",
            vars_candidates.join(", ")
        )
    })?;
    Ok(UefiFirmware {
        code,
        vars_template,
    })
}

/// The first [`SECURE_BOOT_X86_64`] pair present in full. A host with a
/// secboot CODE but no enrolled VARS to go with it is refused by name:
/// booting it would look like secure boot and enforce nothing.
fn secure_boot_x86_64(at: &dyn Fn(&str) -> PathBuf) -> Result<UefiFirmware> {
    if let Some((code, vars)) = SECURE_BOOT_X86_64
        .iter()
        .map(|(c, v)| (at(c), at(v)))
        .find(|(c, v)| c.is_file() && v.is_file())
    {
        return Ok(UefiFirmware {
            code,
            vars_template: vars,
        });
    }
    let enrolled: Vec<&str> = SECURE_BOOT_X86_64.iter().map(|(_, v)| *v).collect();
    let found_code = SECURE_BOOT_X86_64
        .iter()
        .map(|(c, _)| *c)
        .chain(SECURE_BOOT_X86_64_CODE_ONLY.iter().copied())
        .find(|c| at(c).is_file());
    match found_code {
        Some(code) => Err(anyhow!(
            "x86_64 secure boot needs a UEFI VARS template with keys enrolled, and this host has \
             only {code} without one — a blank VARS boots in setup mode and enforces nothing. \
             Install an OVMF build that ships enrolled VARS; tried: {}",
            enrolled.join(", ")
        )),
        None => {
            let codes: Vec<&str> = SECURE_BOOT_X86_64
                .iter()
                .map(|(c, _)| *c)
                .chain(SECURE_BOOT_X86_64_CODE_ONLY.iter().copied())
                .collect();
            Err(anyhow!(
                "x86_64 secure-boot UEFI firmware not found; tried: {}",
                codes.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out `paths` (absolute, as the candidate lists write them) as empty
    /// files under a temp root.
    fn distro(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let full = dir.path().join(p.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, b"").unwrap();
        }
        dir
    }

    /// Candidates are searched in order: a host carrying two of them boots
    /// off the earlier one.
    #[test]
    fn first_candidate_wins() {
        let root = distro(&[
            // Second in the x86_64 CODE list…
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
            // …and first, which must win.
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
            "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.fd",
        ]);
        let fw = lookup_under(root.path(), "x86_64", false).unwrap();
        assert!(fw.code.ends_with("edk2/x64/OVMF_CODE.4m.fd"), "{fw:?}");
        assert!(
            fw.vars_template.ends_with("edk2/x64/OVMF_VARS.4m.fd"),
            "{fw:?}"
        );
    }

    /// Secure boot picks the secboot CODE build and the VARS template with
    /// keys enrolled beside it — never the blank one plain boot uses.
    #[test]
    fn secure_boot_picks_the_secboot_build_with_enrolled_vars() {
        let root = distro(&[
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
        ]);
        let plain = lookup_under(root.path(), "x86_64", false).unwrap();
        assert!(
            !plain.code.to_string_lossy().contains("secboot"),
            "{plain:?}"
        );
        assert!(plain.vars_template.ends_with("OVMF/OVMF_VARS_4M.fd"));

        let sb = lookup_under(root.path(), "x86_64", true).unwrap();
        assert!(sb.code.ends_with("OVMF/OVMF_CODE_4M.secboot.fd"), "{sb:?}");
        assert!(
            sb.vars_template.ends_with("OVMF/OVMF_VARS_4M.ms.fd"),
            "{sb:?}"
        );
    }

    /// CODE and VARS come as a pair: Fedora's 2 MiB secboot build takes
    /// Fedora's enrolled VARS, not Debian's 4 MiB one found first.
    #[test]
    fn secure_boot_pairs_code_with_its_own_vars() {
        let root = distro(&[
            "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
            "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.secboot.fd",
        ]);
        let sb = lookup_under(root.path(), "x86_64", true).unwrap();
        assert!(
            sb.code.ends_with("edk2/ovmf/OVMF_CODE.secboot.fd"),
            "{sb:?}"
        );
        assert!(
            sb.vars_template.ends_with("edk2/ovmf/OVMF_VARS.secboot.fd"),
            "{sb:?}"
        );
    }

    /// A secboot build with only a blank VARS — Arch's layout — is refused
    /// by name rather than booted in setup mode, where it would enforce
    /// nothing.
    #[test]
    fn secure_boot_without_enrolled_vars_is_an_error() {
        let root = distro(&[
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
            "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd",
            "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
        ]);
        let msg = lookup_under(root.path(), "x86_64", true)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("keys enrolled"), "{msg}");
        assert!(msg.contains("OVMF_CODE.secboot.4m.fd"), "{msg}");
        assert!(msg.contains("/usr/share/OVMF/OVMF_VARS_4M.ms.fd"), "{msg}");
    }

    /// A host with the plain build but no secboot one fails rather than
    /// silently booting a firmware that cannot enforce secure boot.
    #[test]
    fn missing_secboot_build_is_an_error() {
        let root = distro(&[
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ]);
        let err = lookup_under(root.path(), "x86_64", true).unwrap_err();
        assert!(err.to_string().contains("firmware not found"), "{err}");
    }

    /// The not-found error names the arch and every path tried, so the user
    /// knows which package is missing.
    #[test]
    fn not_found_names_what_was_tried() {
        let root = distro(&[]);
        let err = lookup_under(root.path(), "aarch64", false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("aarch64 UEFI firmware not found"), "{msg}");
        assert!(msg.contains("/usr/share/AAVMF/AAVMF_CODE.fd"), "{msg}");

        // CODE present, VARS absent — a distinct, equally specific message.
        let root = distro(&["/usr/share/AAVMF/AAVMF_CODE.fd"]);
        let err = lookup_under(root.path(), "aarch64", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("aarch64 UEFI VARS template not found"),
            "{msg}"
        );
        assert!(msg.contains("/usr/share/AAVMF/AAVMF_VARS.fd"), "{msg}");
    }

    #[test]
    fn riscv64_has_its_own_candidates() {
        let root = distro(&[
            "/usr/share/edk2/riscv64/RISCV_VIRT_CODE.fd",
            "/usr/share/edk2/riscv64/RISCV_VIRT_VARS.fd",
        ]);
        let fw = lookup_under(root.path(), "riscv64", false).unwrap();
        assert!(fw.code.ends_with("RISCV_VIRT_CODE.fd"), "{fw:?}");
    }

    #[test]
    fn unknown_arch_is_an_error() {
        let err = lookup("s390x", false).unwrap_err();
        assert!(err.to_string().contains("no UEFI firmware lookup"), "{err}");
    }
}
