//! Scaffolding for brand-new labs (the web UI's "New Lab"). A lab is just a
//! directory containing `vmlab.wcl` (PRD §4); this writes the minimal valid
//! one.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// The initial `vmlab.wcl` for a lab named `name`. An empty lab parses and
/// validates — VMs and segments arrive through the editor.
pub fn initial_lab_wcl(name: &str) -> String {
    format!("import <vmlab.wcl>\n\nlab \"{name}\" {{\n}}\n")
}

/// Create `dir` (if needed) and write the initial `vmlab.wcl` for `name`.
/// Refuses to touch a directory that already holds a lab file or any other
/// content, so it can never clobber existing work. `name` must already be
/// validated as a DNS label by the caller (it is quoted into the lab file).
pub fn create_lab_dir(name: &str, dir: &Path) -> Result<()> {
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
    std::fs::write(&lab_file, initial_lab_wcl(name))
        .with_context(|| format!("cannot write {}", lab_file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_wcl_is_a_valid_lab() {
        let src = initial_lab_wcl("fresh");
        let file = crate::config::load_lab_source(&src, "<test>", Path::new("/tmp")).unwrap();
        assert_eq!(file.lab.name, "fresh");
        assert!(file.lab.vms.is_empty());
    }

    #[test]
    fn creates_into_a_fresh_or_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("newlab");
        create_lab_dir("newlab", &dir).unwrap();
        let written = std::fs::read_to_string(dir.join(crate::paths::LAB_FILE)).unwrap();
        assert!(written.contains("lab \"newlab\""));

        // An existing but empty directory is fine too.
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        create_lab_dir("empty", &empty).unwrap();
    }

    #[test]
    fn refuses_existing_lab_or_non_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lab");
        create_lab_dir("lab", &dir).unwrap();
        let err = create_lab_dir("lab", &dir).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        let busy = tmp.path().join("busy");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("keep.txt"), "x").unwrap();
        let err = create_lab_dir("busy", &busy).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }
}
