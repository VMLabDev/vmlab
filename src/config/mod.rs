//! Lab configuration: WCL schema, typed model, extraction, validation
//! (PRD §5).

// False positives raised inside the miette `Diagnostic` derive expansions.
#![allow(unused_assignments)]

mod extract;
pub mod host;
pub mod model;
pub mod validate;

use std::path::Path;

use miette::{Diagnostic, NamedSource};
use thiserror::Error;
use wcl_lang::{Document, Environment, Registry, disk_loader};

#[allow(unused_imports)]
pub use extract::derive_share_name;
pub use model::{LabFile, TemplateFile};
pub use validate::{ValidationContext, validate};

/// The embedded schema library, imported by user files as
/// `import <vmlab.wcl>`.
pub const SCHEMA_WCL: &str = include_str!("schema.wcl");

/// A single configuration problem with an optional source position.
// The `unused_assignments` allow silences a false positive raised inside the
// miette `Diagnostic` derive expansion.
#[allow(unused_assignments)]
#[derive(Debug, Clone, Error, Diagnostic)]
#[error("{message}")]
pub struct Issue {
    pub message: String,
    #[label]
    pub span: Option<miette::SourceSpan>,
}

impl Issue {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }

    pub fn at(span: model::Span, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: Some(miette::SourceSpan::new(
                span.0.into(),
                span.1.saturating_sub(span.0),
            )),
        }
    }
}

pub type IssueList = Vec<Issue>;

/// All problems found in one lab file, renderable as miette diagnostics
/// against the original source.
#[allow(unused_assignments)]
#[derive(Debug, Error, Diagnostic)]
#[error("{} error(s) in {name}", issues.len())]
pub struct ConfigErrors {
    pub name: String,
    #[source_code]
    pub src: NamedSource<String>,
    #[related]
    pub issues: Vec<Issue>,
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("vmlab.wcl", SCHEMA_WCL);
    r
}

fn open(source: &str, name: &str, base_dir: Option<&Path>) -> Result<Document, ConfigErrors> {
    if !source.contains("import <vmlab.wcl>") {
        return Err(ConfigErrors {
            name: name.to_string(),
            src: NamedSource::new(name, source.to_string()),
            issues: vec![Issue::new(
                "missing schema import — add `import <vmlab.wcl>` at the top of the file",
            )],
        });
    }
    let loader = registry().loader(disk_loader());
    Document::open_at_with_loader(
        source,
        name,
        base_dir.map(Path::to_path_buf),
        &Environment::new(),
        loader,
    )
    .map_err(|e| ConfigErrors {
        name: name.to_string(),
        src: NamedSource::new(name, source.to_string()),
        issues: vec![Issue::new(format!("parse error: {e}"))],
    })
}

fn schema_issues(doc: &Document) -> IssueList {
    doc.schema_errors()
        .into_iter()
        .map(|e| {
            let span = e.labels().and_then(|mut it| it.next()).map(|l| *l.inner());
            Issue {
                message: e.to_string(),
                span,
            }
        })
        .collect()
}

/// Parse + schema-check + extract a lab file. Semantic validation (§5.1) is
/// a separate pass — see [`validate`].
pub fn load_lab_source(source: &str, name: &str, root: &Path) -> Result<LabFile, ConfigErrors> {
    let doc = open(source, name, Some(root))?;
    let mut issues = schema_issues(&doc);
    let lab = extract::extract_lab_file(&doc, root, &mut issues);
    match lab {
        Some(lab) if issues.is_empty() => Ok(lab),
        _ => Err(ConfigErrors {
            name: name.to_string(),
            src: NamedSource::new(name, source.to_string()),
            issues,
        }),
    }
}

/// Load the lab file from a lab root directory.
pub fn load_lab_root(root: &Path) -> Result<LabFile, ConfigErrors> {
    let path = root.join(crate::paths::LAB_FILE);
    let source = std::fs::read_to_string(&path).map_err(|e| ConfigErrors {
        name: path.display().to_string(),
        src: NamedSource::new(path.display().to_string(), String::new()),
        issues: vec![Issue::new(format!("cannot read {}: {e}", path.display()))],
    })?;
    load_lab_source(&source, &path.display().to_string(), root)
}

/// Parse a dedicated template file (templates only, no lab required).
pub fn load_template_source(
    source: &str,
    name: &str,
    root: &Path,
) -> Result<TemplateFile, ConfigErrors> {
    let doc = open(source, name, Some(root))?;
    let mut issues = schema_issues(&doc);
    let tf = extract::extract_template_file(&doc, root, &mut issues);
    if issues.is_empty() {
        Ok(tf)
    } else {
        Err(ConfigErrors {
            name: name.to_string(),
            src: NamedSource::new(name, source.to_string()),
            issues,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::model::*;
    use super::*;

    const SAMPLE: &str = r#"import <vmlab.wcl>

lab "ad-lab" {

  segment "corp" {
    subnet = "10.50.0.0/24"
    dns { server = "10.50.0.10" }
    route { dest = "10.60.0.0/24" via = "10.50.0.254" }
  }

  segment "dmz" { mtu = 9000 }

  vm "dc01" {
    template = "x86_64/windows-server-2025"
    profile  = "windows-server"
    cpus     = 4
    memory   = 8GiB
    nic { segment = "corp"  ip = "10.50.0.10" }
  }

  vm "client01" {
    template   = "x86_64/windows-11@26100.1"
    depends_on = ["dc01"]
    nic { segment = "corp" }
  }

  vm "buildbox" {
    template = "x86_64/linux-modern"
    nic { nat = true }
  }

  vm "airgapped" { template = "x86_64/windows-11" }

  vm "installtest" {
    template = "scratch"
    arch     = "x86_64"
    profile  = "windows-11"
    disk     = 80GiB
    cdrom    = "./isos/win11-build.iso"
  }

  vm "router" {
    template = "aarch64/linux-router@1.2"
    nic { segment = "corp" ip = "10.50.0.254" }
    nic { segment = "dmz" }
  }

  provision "scripts/setup.ws" { }

  on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
  on "host.disk_low" { run = "scripts/alert.ws" }
}
"#;

    #[test]
    fn parses_the_prd_example() {
        let lf = load_lab_source(SAMPLE, "<test>", Path::new("/tmp")).unwrap();
        let lab = &lf.lab;
        assert_eq!(lab.name, "ad-lab");
        assert_eq!(lab.segments.len(), 2);
        assert_eq!(lab.vms.len(), 6);
        assert_eq!(lab.provisions.len(), 1);
        assert_eq!(lab.handlers.len(), 2);

        let corp = &lab.segments[0];
        assert_eq!(corp.name, "corp");
        assert_eq!(corp.subnet.unwrap().to_string(), "10.50.0.0/24");
        assert_eq!(corp.dns.server.unwrap().to_string(), "10.50.0.10");
        assert!(corp.dhcp);
        assert!(!corp.nat);
        assert_eq!(corp.routes.len(), 1);

        let dmz = &lab.segments[1];
        assert!(dmz.subnet.is_none());
        assert_eq!(dmz.mtu, Some(9000));
        assert_eq!(corp.mtu, None); // unset → default resolved at assembly time

        let dc = &lab.vms[0];
        assert_eq!(dc.name, "dc01");
        assert_eq!(dc.cpus, Some(4));
        assert_eq!(dc.memory, Some(8 << 30));
        assert_eq!(dc.nics.len(), 1);
        assert_eq!(dc.nics[0].ip.unwrap().to_string(), "10.50.0.10");
        assert!(
            matches!(&dc.template, TemplateRef::Store { arch, version: None, .. } if arch == "x86_64")
        );

        let client = &lab.vms[1];
        assert_eq!(client.depends_on, vec!["dc01"]);
        assert!(
            matches!(&client.template, TemplateRef::Store { version: Some(v), .. } if v == "26100.1")
        );

        let buildbox = &lab.vms[2];
        assert!(buildbox.nics[0].nat);
        assert!(buildbox.nics[0].segment.is_none());

        let airgapped = &lab.vms[3];
        assert!(airgapped.nics.is_empty());

        let scratch = &lab.vms[4];
        assert_eq!(scratch.template, TemplateRef::Scratch);
        assert_eq!(scratch.disk, Some(80 << 30));
        assert_eq!(scratch.arch.as_deref(), Some("x86_64"));

        let router = &lab.vms[5];
        assert_eq!(router.nics.len(), 2);

        assert_eq!(lab.handlers[0].event, "vm.crashed");
    }

    #[test]
    fn rejects_unknown_attributes() {
        let src = "import <vmlab.wcl>\nlab \"x\" {\n  vm \"a\" { template = \"x86_64/t\" bogus_attr = 1 }\n}\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues.iter().any(|i| i.message.contains("bogus_attr")),
            "expected unknown-attribute error, got: {:?}",
            err.issues
        );
    }

    #[test]
    fn rejects_out_of_range_mtu() {
        let src = "import <vmlab.wcl>\nlab \"x\" {\n  segment \"s\" { mtu = 100 }\n}\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues.iter().any(|i| i.message.contains("mtu")),
            "expected mtu range error, got: {:?}",
            err.issues
        );
    }

    #[test]
    fn requires_schema_import() {
        let err = load_lab_source("lab \"x\" {}\n", "<t>", Path::new("/tmp")).unwrap_err();
        assert!(err.issues[0].message.contains("import <vmlab.wcl>"));
    }

    #[test]
    fn template_blocks_extract() {
        let src = r#"import <vmlab.wcl>
lab "l" { vm "a" { template = "x86_64/base" } }
template "base" {
  arch    = "x86_64"
  version = "1.0"
  profile = "linux-modern"
  disk    = 20GiB
  source "iso" { url = "https://example.com/x.iso" sha256 = "abc123" }
  media { kind = "iso" from = "./unattend/" }
  provision "scripts/install.ws" { }
}
"#;
        let lf = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap();
        assert_eq!(lf.templates.len(), 1);
        let t = &lf.templates[0];
        assert_eq!(t.name, "base");
        assert_eq!(t.version, "1.0");
        assert!(matches!(
            &t.source,
            TemplateSource::Iso(ArtefactSource::Url { .. })
        ));
        assert_eq!(t.media.len(), 1);
        assert_eq!(t.provisions.len(), 1);
    }

    /// Every shipped example template's `vmlab.wcl` must parse (keeps the
    /// examples/templates/ definitions honest, like the wscript script test).
    #[test]
    fn shipped_example_templates_parse() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/templates");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(root).unwrap() {
            let dir = entry.unwrap().path();
            let wcl = dir.join("vmlab.wcl");
            if !wcl.is_file() {
                continue;
            }
            let src = std::fs::read_to_string(&wcl).unwrap();
            let tf = load_template_source(&src, "vmlab.wcl", &dir)
                .unwrap_or_else(|e| panic!("{}: {e:?}", wcl.display()));
            assert!(!tf.templates.is_empty(), "{}: no templates", wcl.display());
            checked += 1;
        }
        assert!(checked >= 4, "expected example templates, found {checked}");
    }
}
