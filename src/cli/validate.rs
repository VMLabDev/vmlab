//! `vmlab validate` — full PRD §5.1 validation, no side effects.

use std::path::Path;

use anyhow::Result;
use miette::NamedSource;

use crate::config::{self, ConfigErrors, ValidationContext};

/// Real validation context: consults the on-disk template store and the
/// profile set. Script compile checking is wired to the wisp host module.
pub struct HostContext;

impl ValidationContext for HostContext {
    fn template_exists(&self, arch: &str, name: &str, version: Option<&str>) -> bool {
        let dir = crate::paths::template_store_dir().join(arch).join(name);
        match version {
            Some(v) => dir.join(v).join("disk.qcow2").is_file(),
            None => std::fs::read_dir(&dir)
                .map(|mut entries| entries.any(|e| e.is_ok()))
                .unwrap_or(false),
        }
    }

    fn profile_exists(&self, name: &str) -> bool {
        // Interim: shipped profile names; replaced by the profile loader.
        [
            "windows-11",
            "windows-server",
            "windows-legacy",
            "linux-modern",
            "linux-generic",
            "custom",
        ]
        .contains(&name)
    }

    fn check_script(&self, _path: &Path) -> Result<(), String> {
        // Wired to the wisp compiler once the host module exists (task: wisp
        // host module); existence is already checked by the validator.
        Ok(())
    }
}

pub fn cmd_validate() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let root = crate::paths::find_lab_root(&cwd)?;
    let file = config::load_lab_root(&root).map_err(miette_to_anyhow)?;
    let issues = config::validate(&file, &HostContext);
    if issues.is_empty() {
        println!(
            "ok: lab \"{}\" — {} vm(s), {} segment(s)",
            file.lab.name,
            file.lab.vms.len(),
            file.lab.segments.len()
        );
        return Ok(());
    }
    let path = root.join(crate::paths::LAB_FILE);
    let source = std::fs::read_to_string(&path).unwrap_or_default();
    let err = ConfigErrors {
        name: path.display().to_string(),
        src: NamedSource::new(path.display().to_string(), source),
        issues,
    };
    Err(miette_to_anyhow(err))
}

fn miette_to_anyhow(e: ConfigErrors) -> anyhow::Error {
    anyhow::anyhow!("{:?}", miette::Report::new(e))
}
