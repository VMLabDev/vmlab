//! `vmlab validate` — full PRD §5.1 validation, no side effects.

use std::path::Path;

use anyhow::Result;
use miette::NamedSource;

use crate::config::{self, ConfigErrors, ValidationContext};

/// Real validation context: consults the on-disk template store and the
/// profile set. Script compile checking is wired to the wscript host module.
pub struct HostContext {
    profiles: crate::profiles::ProfileSet,
    store: crate::template::TemplateStore,
}

impl HostContext {
    pub fn new() -> Result<Self> {
        Ok(Self {
            profiles: crate::profiles::ProfileSet::load_default()?,
            store: crate::template::TemplateStore::new(crate::paths::template_store_dir()),
        })
    }
}

impl ValidationContext for HostContext {
    /// Resolved through the store, so a version pin resolves the same way
    /// it will at `up` (PRD §6.2) and a half-installed entry — metadata
    /// without its disk, or the reverse — reads as absent rather than
    /// present-but-unusable. A variant whose overlay is missing is likewise
    /// absent from the answer (§6.6).
    fn template_meta(
        &self,
        arch: &str,
        name: &str,
        version: Option<&str>,
    ) -> Option<crate::template::TemplateMeta> {
        self.store
            .resolve_version(arch, name, version)
            .ok()
            .map(|t| t.meta)
    }

    fn profile(&self, name: &str) -> Option<crate::profiles::Profile> {
        self.profiles.get(name).cloned()
    }

    fn check_container_hardware(
        &self,
        container: &crate::config::model::Container,
    ) -> Result<(), String> {
        // Containers run the host architecture (v1), which is also what the
        // lab runtime resolves them on.
        crate::qemu::resolve_container(container, std::env::consts::ARCH, &self.profiles)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    }

    fn check_script(&self, path: &Path) -> Result<(), String> {
        let source = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        crate::scripting::check_script_source(&source)
    }

    fn check_host_gpu(
        &self,
        gpu: &crate::config::model::Gpu,
        arch: Option<&str>,
    ) -> Result<(), String> {
        let host = crate::config::host::HostConfig::load_default().map_err(|e| format!("{e:#}"))?;
        crate::qemu::hostgpu::render_plan(
            Some(gpu),
            arch,
            &host,
            &crate::qemu::hostgpu::Sysfs::host(),
            &crate::paths::data_dir(),
        )
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
    }

    fn check_virtiofs_share(&self, share: &crate::config::model::Share) -> Result<(), String> {
        use crate::qemu::virtiofsd::{Probe, probe};
        match probe() {
            Probe::Missing => Ok(()),
            Probe::Found(v) if share.readonly => v
                .readonly_refusal(&format!("share \"{}\"", share.name))
                .map_or(Ok(()), Err),
            Probe::Found(_) => Ok(()),
            unusable => Err(format!(
                "share \"{}\" demands transport = \"virtiofs\", but {}",
                share.name,
                unusable.unusable_reason().unwrap_or_default()
            )),
        }
    }
}

/// A validation problem reduced for the web editor: a message and an optional
/// 1-based line number (derived from the issue's source span).
pub struct SourceIssue {
    pub message: String,
    pub line: Option<usize>,
}

impl SourceIssue {
    pub fn from_issue(source: &str, issue: &config::Issue) -> Self {
        let line = issue.span.map(|s| {
            let off = s.offset().min(source.len());
            source[..off].bytes().filter(|&b| b == b'\n').count() + 1
        });
        Self {
            message: issue.message.clone(),
            line,
        }
    }
}

/// Validate an in-memory lab source against the host (parse + schema + §5.1
/// semantic checks) without touching disk. `Ok(())` means it's safe to write;
/// `Err` carries the issues (with best-effort line numbers) for the editor.
pub fn validate_source(content: &str, root: &Path) -> std::result::Result<(), Vec<SourceIssue>> {
    let file = match config::load_lab_source(content, crate::paths::LAB_FILE, root) {
        Ok(f) => f,
        Err(errs) => {
            return Err(errs
                .issues
                .iter()
                .map(|i| SourceIssue::from_issue(content, i))
                .collect());
        }
    };
    let ctx = match HostContext::new() {
        Ok(c) => c,
        Err(e) => {
            return Err(vec![SourceIssue {
                message: format!("validation context: {e}"),
                line: None,
            }]);
        }
    };
    let issues = config::validate(&file, &ctx);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues
            .iter()
            .map(|i| SourceIssue::from_issue(content, i))
            .collect())
    }
}

pub fn cmd_validate() -> Result<()> {
    let file = validate_current()?;
    for warning in gpu_warnings(&file, |_| true) {
        eprintln!("WARNING: {warning}");
    }
    println!(
        "ok: lab \"{}\" — {} vm(s), {} container(s), {} segment(s)",
        file.lab.name,
        file.lab.vms.len(),
        file.lab.containers.len(),
        file.lab.segments.len()
    );
    Ok(())
}

/// What the chosen VMs' `gpu {}` blocks earn on this host short of an error
/// (PRD §5.2): a `host_gpu` this host lacks that a paravirtual mode falls
/// back from, and a render GPU nothing picked on a host with several. An
/// unreadable host config says nothing here; validation already said it.
pub fn gpu_warnings(
    file: &crate::config::LabFile,
    chosen: impl Fn(&crate::config::model::Vm) -> bool,
) -> Vec<String> {
    let Ok(host) = crate::config::host::HostConfig::load_default() else {
        return Vec::new();
    };
    let gpus = file
        .lab
        .vms
        .iter()
        .filter(|vm| chosen(vm))
        .filter_map(|vm| Some((vm.name.as_str(), vm.gpu.as_ref()?)));
    crate::qemu::hostgpu::warnings(gpus, &host, &crate::qemu::hostgpu::Sysfs::host())
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
