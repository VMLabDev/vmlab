//! Scaffolding for brand-new labs (the web UI's "New Lab"). A lab is just a
//! directory containing `vmlab.wcl` (PRD §4); this writes the minimal valid
//! one.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// What a brand-new lab starts out as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabPreset {
    /// Just `lab "<name>" {}` — segments and VMs arrive through the editor.
    #[default]
    Empty,
    /// A NAT'd segment plus one alpine VM on it: bootable as-is.
    Starter,
}

/// The alpine template the starter preset wires up. Multi-arch and public, so
/// it pulls on the first `up` even on a host with an empty template store.
const STARTER_TEMPLATE: &str = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23";

/// The initial `vmlab.wcl` for a lab named `name`. Both presets parse and
/// validate.
pub fn initial_lab_wcl(name: &str, preset: LabPreset) -> String {
    match preset {
        LabPreset::Empty => format!("import <vmlab.wcl>\n\nlab \"{name}\" {{\n}}\n"),
        LabPreset::Starter => starter_lab_wcl(name, std::env::consts::ARCH),
    }
}

/// The starter lab: one NAT-enabled segment and one alpine VM attached to it.
/// `arch` is the host's — an OCI registry ref carries every arch, so the VM has
/// to name the one it wants (§5.1), and the host's is the only one that runs
/// under KVM.
fn starter_lab_wcl(name: &str, arch: &str) -> String {
    format!(
        r#"import <vmlab.wcl>

lab "{name}" {{

  segment "lan" {{
    subnet = "10.80.0.0/24"
    nat    = true              // internet egress for the guests
  }}

  vm "alpine" {{
    template = "{STARTER_TEMPLATE}"
    arch     = "{arch}"
    memory   = 1GiB
    nic {{ segment = "lan" }}    // dynamic DHCP lease
  }}
}}
"#
    )
}

/// Create `dir` (if needed) and write the initial `vmlab.wcl` for `name`.
/// Refuses to touch a directory that already holds a lab file or any other
/// content, so it can never clobber existing work. `name` must already be
/// validated as a DNS label by the caller (it is quoted into the lab file).
pub fn create_lab_dir(name: &str, dir: &Path, preset: LabPreset) -> Result<()> {
    let lab_file = dir.join(crate::paths::LAB_FILE);
    if lab_file.exists() {
        bail!("{} already exists", lab_file.display());
    }
    if dir.exists() {
        let mut entries =
            std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
        if entries.next().is_some() {
            bail!("{} already exists and is not empty", dir.display());
        }
    } else {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(&lab_file, initial_lab_wcl(name, preset))
        .with_context(|| format!("cannot write {}", lab_file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_wcl_is_a_valid_lab() {
        let src = initial_lab_wcl("fresh", LabPreset::Empty);
        let file = crate::config::load_lab_source(&src, "<test>", Path::new("/tmp")).unwrap();
        assert_eq!(file.lab.name, "fresh");
        assert!(file.lab.vms.is_empty());
        assert!(file.lab.segments.is_empty());
    }

    /// The starter preset is the one thing here a schema change could rot
    /// silently, so parse it back and check every piece it promises.
    #[test]
    fn starter_wcl_is_a_valid_lab() {
        for arch in ["x86_64", "aarch64"] {
            let src = starter_lab_wcl("demo", arch);
            let file = crate::config::load_lab_source(&src, "<test>", Path::new("/tmp")).unwrap();
            assert_eq!(file.lab.name, "demo");

            let [seg] = &file.lab.segments[..] else {
                panic!("expected one segment, got {:?}", file.lab.segments);
            };
            assert_eq!(seg.name, "lan");
            assert!(seg.nat, "the starter segment needs internet egress");
            assert_eq!(seg.subnet.unwrap().to_string(), "10.80.0.0/24");

            let [vm] = &file.lab.vms[..] else {
                panic!("expected one vm, got {:?}", file.lab.vms);
            };
            assert_eq!(vm.name, "alpine");
            assert_eq!(vm.arch.as_deref(), Some(arch));
            let [nic] = &vm.nics[..] else {
                panic!("expected one nic, got {:?}", vm.nics);
            };
            assert_eq!(nic.segment.as_deref(), Some("lan"));
        }
    }

    #[test]
    fn creates_into_a_fresh_or_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("newlab");
        create_lab_dir("newlab", &dir, LabPreset::Empty).unwrap();
        let written = std::fs::read_to_string(dir.join(crate::paths::LAB_FILE)).unwrap();
        assert!(written.contains("lab \"newlab\""));

        // An existing but empty directory is fine too.
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        create_lab_dir("empty", &empty, LabPreset::Empty).unwrap();

        // The starter preset lands the same way, VMs and all.
        let starter = tmp.path().join("starter");
        create_lab_dir("starter", &starter, LabPreset::Starter).unwrap();
        let written = std::fs::read_to_string(starter.join(crate::paths::LAB_FILE)).unwrap();
        assert!(written.contains("vm \"alpine\""), "{written}");
    }

    #[test]
    fn refuses_existing_lab_or_non_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lab");
        create_lab_dir("lab", &dir, LabPreset::Empty).unwrap();
        let err = create_lab_dir("lab", &dir, LabPreset::Empty).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        let busy = tmp.path().join("busy");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("keep.txt"), "x").unwrap();
        let err = create_lab_dir("busy", &busy, LabPreset::Empty).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }
}
