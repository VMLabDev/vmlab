//! `vmlab validate` — full PRD §5.1 validation, no side effects.

use std::path::Path;

use anyhow::Result;
use miette::NamedSource;

use crate::config::{self, ConfigErrors, ValidationContext};

/// Real validation context: consults the on-disk template store and the
/// profile set. Script compile checking is wired to the wisp host module.
pub struct HostContext {
    profiles: crate::profiles::ProfileSet,
}

impl HostContext {
    pub fn new() -> Result<Self> {
        Ok(Self {
            profiles: crate::profiles::ProfileSet::load_default()?,
        })
    }
}

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
        self.profiles.exists(name)
    }

    fn check_script(&self, path: &Path) -> Result<(), String> {
        let source = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        crate::scripting::check_script_source(&source)
    }
}

pub fn cmd_validate() -> Result<()> {
    let file = validate_current()?;
    println!(
        "ok: lab \"{}\" — {} vm(s), {} segment(s)",
        file.lab.name,
        file.lab.vms.len(),
        file.lab.segments.len()
    );
    Ok(())
}

/// Full validation of the cwd's lab; every side-effecting verb runs this
/// first (PRD §5.1: implicitly every other verb).
pub fn validate_current() -> Result<crate::config::LabFile> {
    let cwd = std::env::current_dir()?;
    let root = crate::paths::find_lab_root(&cwd)?;
    let file = config::load_lab_root(&root).map_err(miette_to_anyhow)?;
    let issues = config::validate(&file, &HostContext::new()?);
    if issues.is_empty() {
        return Ok(file);
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
