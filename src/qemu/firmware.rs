//! Locate UEFI firmware (OVMF/AAVMF/RiscVVirt) for a VM.
//!
//! vmlab ships its own: the guest asset set carries Debian's edk2 builds
//! under `firmware/<arch>/` (`guest/build-asset.sh` fetches them pinned by
//! sha256), so a host needs no OVMF package and every host boots the same
//! bytes. The search, highest priority first (PRD §5.2):
//!
//!  1. the host config's `firmware_dir` — `<firmware_dir>/<arch>/`, laid out
//!     like the bundled set. A directory there is authoritative for that
//!     arch: a pair missing from it is an error, never a silent fall-through
//!     to something the user did not choose.
//!  2. vmlab's bundled firmware, `<guest>/firmware/<arch>/`, under each of
//!     the guest asset directories in [`crate::guest_asset`]'s order.
//!  3. the host's distro firmware at its well-known paths.
//!
//! Secure boot and plain UEFI walk the same order. A secure-boot build is
//! always taken with the VARS template that has keys enrolled for it: a
//! blank VARS leaves the firmware in setup mode, where it verifies nothing.
//!
//! Discovery probes the host filesystem, so it happens once per machine
//! start, where the lab daemon assembles the runtime paths — never inside
//! [`super::cmdline::build_args`], which takes the resolved image as an
//! injected path (see ADR-0008).

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// A resolved UEFI firmware pair: read-only CODE image plus a pristine VARS
/// template to copy per VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UefiFirmware {
    pub code: PathBuf,
    pub vars_template: PathBuf,
}

/// The file names inside a `firmware/<arch>/` directory — the bundled set's
/// layout, and the one a `firmware_dir` override follows. `None` where vmlab
/// has no such build: riscv64 has no secure-boot variant.
pub fn bundled_names(arch: &str, secure_boot: bool) -> Option<(&'static str, &'static str)> {
    Some(match (arch, secure_boot) {
        // Debian `ovmf`: the 4 MiB builds; `.ms` carries Microsoft's keys
        // (and the distro's) enrolled, which Windows and shim boot under.
        ("x86_64", false) => ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"),
        ("x86_64", true) => ("OVMF_CODE_4M.secboot.fd", "OVMF_VARS_4M.ms.fd"),
        // Debian `qemu-efi-aarch64`: 64 MiB pflash images.
        ("aarch64", false) => ("AAVMF_CODE.fd", "AAVMF_VARS.fd"),
        ("aarch64", true) => ("AAVMF_CODE.secboot.fd", "AAVMF_VARS.ms.fd"),
        // Not bundled, but an override may carry it.
        ("riscv64", false) => ("RISCV_VIRT_CODE.fd", "RISCV_VIRT_VARS.fd"),
        _ => return None,
    })
}

/// The host's plain-UEFI pairs for one arch, most specific first. Data, not
/// lookup: [`Sources`] is what touches the filesystem.
fn host_pairs(arch: &str) -> Result<&'static [(&'static str, &'static str)]> {
    Ok(match arch {
        "x86_64" => &[
            // Arch (`edk2-ovmf`).
            (
                "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
                "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
            ),
            // Fedora, RHEL (`edk2-ovmf`).
            (
                "/usr/share/edk2/ovmf/OVMF_CODE.fd",
                "/usr/share/edk2/ovmf/OVMF_VARS.fd",
            ),
            // Debian, Ubuntu (`ovmf`).
            (
                "/usr/share/OVMF/OVMF_CODE_4M.fd",
                "/usr/share/OVMF/OVMF_VARS_4M.fd",
            ),
            (
                "/usr/share/OVMF/OVMF_CODE.fd",
                "/usr/share/OVMF/OVMF_VARS.fd",
            ),
            // Gentoo.
            (
                "/usr/share/edk2-ovmf/OVMF_CODE.fd",
                "/usr/share/edk2-ovmf/OVMF_VARS.fd",
            ),
            // openSUSE.
            (
                "/usr/share/qemu/ovmf-x86_64-code.bin",
                "/usr/share/qemu/ovmf-x86_64-vars.bin",
            ),
        ],
        "aarch64" => &[
            (
                "/usr/share/edk2/aarch64/QEMU_CODE.fd",
                "/usr/share/edk2/aarch64/QEMU_VARS.fd",
            ),
            (
                "/usr/share/edk2/aarch64/QEMU_EFI.fd",
                "/usr/share/edk2/aarch64/QEMU_VARS.fd",
            ),
            (
                "/usr/share/AAVMF/AAVMF_CODE.fd",
                "/usr/share/AAVMF/AAVMF_VARS.fd",
            ),
            (
                "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
                "/usr/share/AAVMF/AAVMF_VARS.fd",
            ),
        ],
        // EDK2 RiscVVirt. The `virt` machine takes the CODE image on pflash
        // unit 0 and the writable VARS on unit 1, exactly like aarch64; the
        // packaged blobs are already padded to the 32 MiB pflash size QEMU
        // requires (copied verbatim per VM, so the size is preserved).
        "riscv64" => &[
            (
                "/usr/share/qemu-efi-riscv64/RISCV_VIRT_CODE.fd",
                "/usr/share/qemu-efi-riscv64/RISCV_VIRT_VARS.fd",
            ),
            (
                "/usr/share/edk2/riscv64/RISCV_VIRT_CODE.fd",
                "/usr/share/edk2/riscv64/RISCV_VIRT_VARS.fd",
            ),
            (
                "/usr/share/edk2/riscv/RISCV_VIRT_CODE.fd",
                "/usr/share/edk2/riscv/RISCV_VIRT_VARS.fd",
            ),
        ],
        other => return Err(anyhow!("no UEFI firmware lookup for arch {other}")),
    })
}

/// The host's secure-boot builds, each with the VARS template that has keys
/// enrolled for it — searched as a pair, so a secboot CODE without its
/// enrolled VARS is no answer. Empty for an arch with no secure-boot variant.
fn host_secure_boot_pairs(arch: &str) -> &'static [(&'static str, &'static str)] {
    match arch {
        "x86_64" => &[
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
        ],
        // Debian (`qemu-efi-aarch64`).
        "aarch64" => &[(
            "/usr/share/AAVMF/AAVMF_CODE.secboot.fd",
            "/usr/share/AAVMF/AAVMF_VARS.ms.fd",
        )],
        _ => &[],
    }
}

/// Secure-boot CODE builds shipped without an enrolled VARS beside them —
/// Arch's `edk2-ovmf` is one. Named in the error so a host carrying one is
/// told what is missing rather than that it has no firmware.
const SECURE_BOOT_X86_64_CODE_ONLY: &[&str] = &["/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd"];

/// Where firmware is searched, highest priority first. Every tier is a field,
/// which is both how production describes the installed layout
/// ([`Sources::installed`]) and the seam the tests lay out a fake one on.
#[derive(Debug, Clone)]
pub struct Sources {
    /// The host config's `firmware_dir`: `<dir>/<arch>/` in the bundled
    /// layout, authoritative for an arch whose directory exists.
    pub override_dir: Option<PathBuf>,
    /// Guest asset bases; vmlab's own firmware is `<base>/firmware/<arch>/`.
    pub bundled: Vec<PathBuf>,
    /// The filesystem root the host's distro paths are joined onto — `/` in
    /// production.
    pub host_root: PathBuf,
}

impl Sources {
    /// The installed layout: `override_dir` from the host config, the guest
    /// asset directories in their lookup order, and the real root.
    pub fn installed(override_dir: Option<PathBuf>) -> Self {
        Self {
            override_dir,
            bundled: crate::guest_asset::candidate_dirs(),
            host_root: PathBuf::from("/"),
        }
    }

    fn host(&self, p: &str) -> PathBuf {
        self.host_root.join(p.trim_start_matches('/'))
    }

    /// `<override>/<arch>/`, when the override names a directory for this
    /// arch — which makes it the only place this arch is looked up.
    fn override_arch_dir(&self, arch: &str) -> Option<PathBuf> {
        self.override_dir
            .as_ref()
            .map(|d| d.join(arch))
            .filter(|d| d.is_dir())
    }

    /// The UEFI CODE/VARS pair for a QEMU arch (`x86_64`, `aarch64`,
    /// `riscv64`). `secure_boot` selects a secboot build with keys enrolled
    /// (x86_64, aarch64); riscv64 has no secure-boot variant and gets its
    /// plain build.
    pub fn lookup(&self, arch: &str, secure_boot: bool) -> Result<UefiFirmware> {
        let names = bundled_names(arch, secure_boot);
        if let Some(dir) = self.override_arch_dir(arch) {
            let Some((code, vars)) = names else {
                return Err(anyhow!(
                    "firmware_dir {} holds no {} build vmlab can use for {arch}",
                    dir.display(),
                    if secure_boot { "secure-boot" } else { "UEFI" }
                ));
            };
            let (code, vars) = (dir.join(code), dir.join(vars));
            if code.is_file() && vars.is_file() {
                return Ok(UefiFirmware {
                    code,
                    vars_template: vars,
                });
            }
            return Err(anyhow!(
                "firmware_dir {} is the {arch} firmware this host config chose, and it lacks {}{} \
                 — add the pair, or remove the {arch} directory to fall back to vmlab's own",
                dir.display(),
                if code.is_file() {
                    ""
                } else {
                    "the CODE image "
                },
                if code.is_file() {
                    format!("the VARS template {}", vars.display())
                } else {
                    code.display().to_string()
                },
            ));
        }
        if let Some(fw) = self.bundled_pair(arch, secure_boot) {
            return Ok(fw);
        }
        self.host_lookup(arch, secure_boot).map_err(|e| {
            let tried = self.bundled_tried(arch, secure_boot);
            if tried.is_empty() {
                e
            } else {
                anyhow!(
                    "{e}; vmlab's own firmware was not found either (tried: {}) — reinstall the \
                     guest asset bundle, or point `firmware_dir` in the host config at a copy",
                    tried.join(", ")
                )
            }
        })
    }

    /// The pair to boot a VM whose VARS copy already exists with `vars_len`
    /// bytes. A VARS store only works under a CODE build of its own flash
    /// layout, so a VM created under some other firmware — the host's 2 MiB
    /// OVMF, before vmlab shipped its own — keeps booting the firmware it was
    /// created with. The usual answer is [`Sources::lookup`]'s; past that,
    /// every candidate of the right kind in search order, by VARS size.
    pub fn lookup_for_vars(
        &self,
        arch: &str,
        secure_boot: bool,
        vars_len: u64,
    ) -> Result<UefiFirmware> {
        let first = self.lookup(arch, secure_boot);
        if let Ok(fw) = &first
            && len_of(&fw.vars_template) == Some(vars_len)
        {
            return first;
        }
        let matching = self
            .every_pair(arch, secure_boot)
            .into_iter()
            .find(|fw| fw.code.is_file() && len_of(&fw.vars_template) == Some(vars_len));
        if let Some(fw) = matching {
            return Ok(fw);
        }
        let fw = first?;
        Err(anyhow!(
            "this VM's UEFI VARS store is {vars_len} bytes and the firmware vmlab would boot \
             ({}) takes {} — it was created under a different firmware build, and none of that \
             layout is installed. Delete the VM's OVMF_VARS.fd to start it with a fresh store \
             (losing its UEFI boot entries), or install the firmware it was created with",
            fw.code.display(),
            len_of(&fw.vars_template).unwrap_or(0),
        ))
    }

    fn bundled_pair(&self, arch: &str, secure_boot: bool) -> Option<UefiFirmware> {
        let (code, vars) = bundled_names(arch, secure_boot)?;
        self.bundled.iter().find_map(|base| {
            let dir = base.join("firmware").join(arch);
            let (code, vars) = (dir.join(code), dir.join(vars));
            (code.is_file() && vars.is_file()).then_some(UefiFirmware {
                code,
                vars_template: vars,
            })
        })
    }

    fn bundled_tried(&self, arch: &str, secure_boot: bool) -> Vec<String> {
        let Some((code, _)) = bundled_names(arch, secure_boot) else {
            return Vec::new();
        };
        self.bundled
            .iter()
            .map(|b| {
                b.join("firmware")
                    .join(arch)
                    .join(code)
                    .display()
                    .to_string()
            })
            .collect()
    }

    /// Every candidate pair of one kind, in search order, existing or not.
    fn every_pair(&self, arch: &str, secure_boot: bool) -> Vec<UefiFirmware> {
        let mut out = Vec::new();
        let pair = |dir: PathBuf, (c, v): (&str, &str)| UefiFirmware {
            code: dir.join(c),
            vars_template: dir.join(v),
        };
        let names = bundled_names(arch, secure_boot);
        if let Some(dir) = self.override_arch_dir(arch) {
            // Authoritative: nothing else is a candidate for this arch.
            out.extend(names.map(|n| pair(dir, n)));
            return out;
        }
        if let Some(n) = names {
            for base in &self.bundled {
                out.push(pair(base.join("firmware").join(arch), n));
            }
        }
        let host: &[(&str, &str)] = if secure_boot && arch != "riscv64" {
            host_secure_boot_pairs(arch)
        } else {
            host_pairs(arch).unwrap_or(&[])
        };
        out.extend(host.iter().map(|(c, v)| UefiFirmware {
            code: self.host(c),
            vars_template: self.host(v),
        }));
        out
    }

    /// The host's distro firmware — the last tier.
    fn host_lookup(&self, arch: &str, secure_boot: bool) -> Result<UefiFirmware> {
        let plain = host_pairs(arch)?;
        if secure_boot && !host_secure_boot_pairs(arch).is_empty() {
            return self.host_secure_boot(arch);
        }
        if let Some(fw) = plain
            .iter()
            .map(|(c, v)| (self.host(c), self.host(v)))
            .find(|(c, v)| c.is_file() && v.is_file())
            .map(|(code, vars_template)| UefiFirmware {
                code,
                vars_template,
            })
        {
            return Ok(fw);
        }
        let codes: Vec<&str> = plain.iter().map(|(c, _)| *c).collect();
        let vars: Vec<&str> = plain.iter().map(|(_, v)| *v).collect();
        if codes.iter().any(|c| self.host(c).is_file()) {
            Err(anyhow!(
                "{arch} UEFI VARS template not found; tried: {}",
                vars.join(", ")
            ))
        } else {
            Err(anyhow!(
                "{arch} UEFI firmware not found; tried: {}",
                codes.join(", ")
            ))
        }
    }

    /// The first host secure-boot pair present in full. A host with a
    /// secboot CODE but no enrolled VARS to go with it is refused by name:
    /// booting it would look like secure boot and enforce nothing.
    fn host_secure_boot(&self, arch: &str) -> Result<UefiFirmware> {
        let pairs = host_secure_boot_pairs(arch);
        if let Some((code, vars)) = pairs
            .iter()
            .map(|(c, v)| (self.host(c), self.host(v)))
            .find(|(c, v)| c.is_file() && v.is_file())
        {
            return Ok(UefiFirmware {
                code,
                vars_template: vars,
            });
        }
        let code_only: &[&str] = if arch == "x86_64" {
            SECURE_BOOT_X86_64_CODE_ONLY
        } else {
            &[]
        };
        let codes: Vec<&str> = pairs
            .iter()
            .map(|(c, _)| *c)
            .chain(code_only.iter().copied())
            .collect();
        let enrolled: Vec<&str> = pairs.iter().map(|(_, v)| *v).collect();
        match codes.iter().find(|c| self.host(c).is_file()) {
            Some(code) => Err(anyhow!(
                "{arch} secure boot needs a UEFI VARS template with keys enrolled, and this host \
                 has only {code} without one — a blank VARS boots in setup mode and enforces \
                 nothing; tried: {}",
                enrolled.join(", ")
            )),
            None => Err(anyhow!(
                "{arch} secure-boot UEFI firmware not found; tried: {}",
                codes.join(", ")
            )),
        }
    }
}

fn len_of(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok().map(|m| m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake host: `host` lays out distro paths (absolute, as the candidate
    /// lists write them) under `root/`, `bundled` writes `firmware/<arch>/…`
    /// files under `guest/`, and `overrides` under `override/`.
    struct Fake {
        dir: tempfile::TempDir,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn put(&self, rel: &str, len: usize) -> &Self {
            let full = self.dir.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, vec![0u8; len]).unwrap();
            self
        }
        fn host(&self, paths: &[&str]) -> &Self {
            for p in paths {
                self.put(&format!("root/{}", p.trim_start_matches('/')), 0);
            }
            self
        }
        fn bundled(&self, arch: &str, files: &[&str]) -> &Self {
            for f in files {
                self.put(&format!("guest/firmware/{arch}/{f}"), 0);
            }
            self
        }
        fn sources(&self, with_override: bool) -> Sources {
            Sources {
                override_dir: with_override.then(|| self.dir.path().join("override")),
                bundled: vec![
                    self.dir.path().join("missing-guest"),
                    self.dir.path().join("guest"),
                ],
                host_root: self.dir.path().join("root"),
            }
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }
    }

    const DEBIAN_X86: &[&str] = &[
        "/usr/share/OVMF/OVMF_CODE_4M.fd",
        "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
        "/usr/share/OVMF/OVMF_VARS_4M.fd",
        "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
    ];
    const BUNDLED_X86: &[&str] = &[
        "OVMF_CODE_4M.fd",
        "OVMF_VARS_4M.fd",
        "OVMF_CODE_4M.secboot.fd",
        "OVMF_VARS_4M.ms.fd",
    ];

    /// The bundled set wins over the host's distro firmware, for plain UEFI
    /// and for secure boot alike — and secure boot takes the enrolled VARS.
    #[test]
    fn bundled_beats_host() {
        let f = Fake::new();
        f.host(DEBIAN_X86).bundled("x86_64", BUNDLED_X86);
        let s = f.sources(false);
        let plain = s.lookup("x86_64", false).unwrap();
        assert_eq!(plain.code, f.path("guest/firmware/x86_64/OVMF_CODE_4M.fd"));
        assert_eq!(
            plain.vars_template,
            f.path("guest/firmware/x86_64/OVMF_VARS_4M.fd")
        );
        let sb = s.lookup("x86_64", true).unwrap();
        assert_eq!(
            sb.code,
            f.path("guest/firmware/x86_64/OVMF_CODE_4M.secboot.fd")
        );
        assert_eq!(
            sb.vars_template,
            f.path("guest/firmware/x86_64/OVMF_VARS_4M.ms.fd")
        );
    }

    /// An override beats the bundled set, and is authoritative for an arch
    /// it has a directory for: a missing pair there is an error, not a fall
    /// back to firmware the user did not choose.
    #[test]
    fn override_beats_bundled_and_is_authoritative() {
        let f = Fake::new();
        f.bundled("x86_64", BUNDLED_X86)
            .bundled("aarch64", &["AAVMF_CODE.fd", "AAVMF_VARS.fd"])
            .put("override/x86_64/OVMF_CODE_4M.fd", 0)
            .put("override/x86_64/OVMF_VARS_4M.fd", 0);
        let s = f.sources(true);
        let fw = s.lookup("x86_64", false).unwrap();
        assert_eq!(fw.code, f.path("override/x86_64/OVMF_CODE_4M.fd"));

        // The override's x86_64 directory has no secboot pair.
        let msg = s.lookup("x86_64", true).unwrap_err().to_string();
        assert!(msg.contains("firmware_dir"), "{msg}");
        assert!(msg.contains("OVMF_CODE_4M.secboot.fd"), "{msg}");

        // No aarch64 directory in the override: the bundled set answers.
        let fw = s.lookup("aarch64", false).unwrap();
        assert_eq!(fw.code, f.path("guest/firmware/aarch64/AAVMF_CODE.fd"));
    }

    /// Bundled secure boot is a pair: a bundled secboot CODE whose enrolled
    /// VARS is missing is passed over, never paired with the blank VARS.
    #[test]
    fn bundled_secure_boot_needs_its_enrolled_vars() {
        let f = Fake::new();
        f.bundled(
            "x86_64",
            &[
                "OVMF_CODE_4M.fd",
                "OVMF_VARS_4M.fd",
                "OVMF_CODE_4M.secboot.fd",
            ],
        )
        .host(DEBIAN_X86);
        let sb = f.sources(false).lookup("x86_64", true).unwrap();
        assert!(
            sb.code
                .ends_with("root/usr/share/OVMF/OVMF_CODE_4M.secboot.fd")
        );
        assert!(sb.vars_template.ends_with("OVMF/OVMF_VARS_4M.ms.fd"));
    }

    /// aarch64 has its own bundled pairs, secure boot included.
    #[test]
    fn aarch64_bundled_pairs() {
        let f = Fake::new();
        f.bundled(
            "aarch64",
            &[
                "AAVMF_CODE.fd",
                "AAVMF_VARS.fd",
                "AAVMF_CODE.secboot.fd",
                "AAVMF_VARS.ms.fd",
            ],
        );
        let s = f.sources(false);
        let plain = s.lookup("aarch64", false).unwrap();
        assert_eq!(plain.code, f.path("guest/firmware/aarch64/AAVMF_CODE.fd"));
        assert_eq!(
            plain.vars_template,
            f.path("guest/firmware/aarch64/AAVMF_VARS.fd")
        );
        let sb = s.lookup("aarch64", true).unwrap();
        assert_eq!(
            sb.code,
            f.path("guest/firmware/aarch64/AAVMF_CODE.secboot.fd")
        );
        assert_eq!(
            sb.vars_template,
            f.path("guest/firmware/aarch64/AAVMF_VARS.ms.fd")
        );
    }

    /// With nothing bundled, the host's distro firmware is the fallback, its
    /// candidates searched in order.
    #[test]
    fn host_is_the_fallback_and_first_candidate_wins() {
        let f = Fake::new();
        f.host(&[
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.fd",
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
            "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
        ]);
        let fw = f.sources(false).lookup("x86_64", false).unwrap();
        assert!(fw.code.ends_with("edk2/x64/OVMF_CODE.4m.fd"), "{fw:?}");
        assert!(fw.vars_template.ends_with("edk2/x64/OVMF_VARS.4m.fd"));
    }

    /// Host secure boot pairs CODE with its own VARS: Fedora's secboot build
    /// takes Fedora's enrolled VARS, not Debian's found first.
    #[test]
    fn host_secure_boot_pairs_code_with_its_own_vars() {
        let f = Fake::new();
        f.host(&[
            "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
            "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.secboot.fd",
        ]);
        let sb = f.sources(false).lookup("x86_64", true).unwrap();
        assert!(sb.code.ends_with("edk2/ovmf/OVMF_CODE.secboot.fd"));
        assert!(sb.vars_template.ends_with("edk2/ovmf/OVMF_VARS.secboot.fd"));
    }

    /// A secboot build with only a blank VARS — Arch's layout — is refused
    /// by name rather than booted in setup mode, and the error says vmlab's
    /// own firmware is missing too.
    #[test]
    fn secure_boot_without_enrolled_vars_is_an_error() {
        let f = Fake::new();
        f.host(&[
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
            "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd",
            "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
        ]);
        let msg = f
            .sources(false)
            .lookup("x86_64", true)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("keys enrolled"), "{msg}");
        assert!(msg.contains("OVMF_CODE.secboot.4m.fd"), "{msg}");
        assert!(msg.contains("/usr/share/OVMF/OVMF_VARS_4M.ms.fd"), "{msg}");
        assert!(
            msg.contains("guest/firmware/x86_64/OVMF_CODE_4M.secboot.fd"),
            "{msg}"
        );
        assert!(msg.contains("firmware_dir"), "{msg}");
    }

    /// Nothing anywhere: the error names the arch and every path tried, host
    /// and bundled, so the user knows what is missing.
    #[test]
    fn not_found_names_what_was_tried() {
        let f = Fake::new();
        let msg = f
            .sources(false)
            .lookup("aarch64", false)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("aarch64 UEFI firmware not found"), "{msg}");
        assert!(msg.contains("/usr/share/AAVMF/AAVMF_CODE.fd"), "{msg}");
        assert!(
            msg.contains("guest/firmware/aarch64/AAVMF_CODE.fd"),
            "{msg}"
        );

        // CODE present, VARS absent — a distinct, equally specific message.
        f.host(&["/usr/share/AAVMF/AAVMF_CODE.fd"]);
        let msg = f
            .sources(false)
            .lookup("aarch64", false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("aarch64 UEFI VARS template not found"),
            "{msg}"
        );
        assert!(msg.contains("/usr/share/AAVMF/AAVMF_VARS.fd"), "{msg}");

        let msg = f
            .sources(false)
            .lookup("x86_64", true)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("secure-boot UEFI firmware not found"), "{msg}");
    }

    /// An existing VM's VARS store keeps the firmware of its own layout: a
    /// 2 MiB store created under the host's old OVMF boots that OVMF, not
    /// the bundled 4 MiB build; a store of the bundled size boots the bundle.
    #[test]
    fn an_existing_vars_store_keeps_its_layout() {
        let f = Fake::new();
        f.put("guest/firmware/x86_64/OVMF_CODE_4M.fd", 8)
            .put("guest/firmware/x86_64/OVMF_VARS_4M.fd", 540)
            .put("root/usr/share/OVMF/OVMF_CODE.fd", 8)
            .put("root/usr/share/OVMF/OVMF_VARS.fd", 128);
        let s = f.sources(false);
        let fw = s.lookup_for_vars("x86_64", false, 540).unwrap();
        assert_eq!(fw.code, f.path("guest/firmware/x86_64/OVMF_CODE_4M.fd"));
        let fw = s.lookup_for_vars("x86_64", false, 128).unwrap();
        assert_eq!(fw.code, f.path("root/usr/share/OVMF/OVMF_CODE.fd"));
        let msg = s
            .lookup_for_vars("x86_64", false, 999)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("999 bytes") && msg.contains("OVMF_VARS.fd"),
            "{msg}"
        );
    }

    #[test]
    fn riscv64_has_its_own_candidates() {
        let f = Fake::new();
        f.host(&[
            "/usr/share/edk2/riscv64/RISCV_VIRT_CODE.fd",
            "/usr/share/edk2/riscv64/RISCV_VIRT_VARS.fd",
        ]);
        let fw = f.sources(false).lookup("riscv64", false).unwrap();
        assert!(fw.code.ends_with("RISCV_VIRT_CODE.fd"), "{fw:?}");
        // No secure-boot variant: the plain build answers, as it always has.
        let fw = f.sources(false).lookup("riscv64", true).unwrap();
        assert!(fw.code.ends_with("RISCV_VIRT_CODE.fd"), "{fw:?}");
    }

    #[test]
    fn unknown_arch_is_an_error() {
        let err = Fake::new()
            .sources(false)
            .lookup("s390x", false)
            .unwrap_err();
        assert!(err.to_string().contains("no UEFI firmware lookup"), "{err}");
    }
}
