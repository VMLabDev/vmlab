//! Lab configuration: WCL schema, typed model, extraction, validation
//! (PRD §5).

pub(crate) mod block;
mod extract;
pub mod host;
pub mod model;
pub mod projection;
pub mod validate;

use std::path::Path;

use miette::{Diagnostic, NamedSource};
use thiserror::Error;
use wcl_lang::{Document, Environment, Registry, disk_loader};

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

/// Schema violations as positioned issues, so a caller can accumulate them
/// alongside the ones [`block::Reader`] raises.
pub(crate) fn schema_issues(doc: &Document) -> IssueList {
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
    let tf = extract::extract_template_file(&doc, &mut issues);
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
    provision "scripts/setup.ws" { }
    playbook "playbooks/base" {
      play = "baseline"
      var "tz" { value = "UTC" }
    }
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
        assert_eq!(lab.handlers.len(), 2);

        // Configuration steps live inside the machine they configure.
        let buildbox = lab.vms.iter().find(|v| v.name == "buildbox").unwrap();
        assert_eq!(buildbox.provisions.len(), 1);
        assert_eq!(
            buildbox.provisions[0].script.display().to_string(),
            "scripts/setup.ws"
        );
        assert_eq!(buildbox.playbooks.len(), 1);
        assert_eq!(buildbox.playbooks[0].play, "baseline");
        assert_eq!(buildbox.playbooks[0].vars.len(), 1);
        assert_eq!(buildbox.playbooks[0].vars[0].name, "tz");
        assert_eq!(buildbox.playbooks[0].vars[0].value, "UTC");

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
    fn container_blocks_extract() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  segment "s" { subnet = "10.1.1.0/24" }
  container "web" {
    image      = "ghcr.io/owner/web:2.1"
    mode       = :workload
    entrypoint = ["/entry.sh"]
    command    = ["--serve"]
    workdir    = "/srv"
    user       = "33:33"
    cpus       = 2
    memory     = 512MiB
    depends_on = ["db"]
    nic { segment = "s" ip = "10.1.1.30" }
    env { name = "MODE" value = "prod" }
    volume { name = "data" target = "/var/lib/data" }
    volume { host = "./www" target = "/srv/www" read_only = true }
    port { host = 18080 container = 80 proto = "both" }
    healthcheck {
      command      = ["curl", "-fsS", "http://localhost/"]
      interval     = 5s
      timeout      = 2s
      retries      = 5
      start_period = 30s
    }
  }
  container "db" { image = "postgres:16" mode = :idle nic { segment = "s" } }
}
"#;
        let lf = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap();
        let lab = &lf.lab;
        assert_eq!(lab.containers.len(), 2);

        let web = &lab.containers[0];
        assert_eq!(web.name, "web");
        assert_eq!(web.image.reference, "ghcr.io/owner/web:2.1");
        assert_eq!(web.mode, ContainerMode::Workload);
        assert_eq!(
            web.entrypoint.as_deref(),
            Some(&["/entry.sh".to_string()][..])
        );
        assert_eq!(web.command.as_deref(), Some(&["--serve".to_string()][..]));
        assert_eq!(web.workdir.as_deref(), Some("/srv"));
        assert_eq!(web.user.as_deref(), Some("33:33"));
        assert_eq!(web.cpus, Some(2));
        assert_eq!(web.memory, Some(512 << 20));
        assert_eq!(web.depends_on, vec!["db"]);
        assert_eq!(web.nics.len(), 1);
        assert_eq!(web.nics[0].ip.unwrap().to_string(), "10.1.1.30");
        assert_eq!(web.env.len(), 1);
        assert_eq!(
            (web.env[0].name.as_str(), web.env[0].value.as_str()),
            ("MODE", "prod")
        );
        assert_eq!(web.volumes.len(), 2);
        assert!(matches!(&web.volumes[0].source, VolumeSource::Named(n) if n == "data"));
        assert!(!web.volumes[0].read_only);
        assert!(matches!(&web.volumes[1].source, VolumeSource::Host(_)));
        assert!(web.volumes[1].read_only);
        assert_eq!(web.ports.len(), 1);
        assert_eq!(web.ports[0].host_port, 18080);
        assert_eq!(web.ports[0].container_port, 80);
        assert_eq!(web.ports[0].proto, Proto::Both);
        let hc = web.healthcheck.as_ref().unwrap();
        assert_eq!(hc.command[0], "curl");
        assert_eq!(hc.interval, std::time::Duration::from_secs(5));
        assert_eq!(hc.timeout, std::time::Duration::from_secs(2));
        assert_eq!(hc.retries, 5);
        assert_eq!(hc.start_period, std::time::Duration::from_secs(30));

        let db = &lab.containers[1];
        assert_eq!(db.mode, ContainerMode::Idle);
        assert!(db.entrypoint.is_none());
        assert!(db.command.is_none());
        assert!(db.healthcheck.is_none());
    }

    /// §19.2: identity is machine-level, so `login {}` reads the same on a VM
    /// and on a container, and a machine may declare several.
    #[test]
    fn login_blocks_extract_on_both_machine_kinds() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  vm "dev01" {
    template = "x86_64/t"
    login "dev"   { user = "PROBE\\dev"   password = "vmlab123!" default = true }
    login "admin" { user = "PROBE\\admin" password = "vmlab123!" elevated = false }
  }
  container "buildbox" {
    image = "sdk:9.0"
    login "dev" { user = "dev" }
  }
}
"#;
        let lf = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap();
        let vm = &lf.lab.vms[0];
        assert_eq!(vm.logins.len(), 2);
        assert_eq!(vm.logins[0].label, "dev");
        assert_eq!(vm.logins[0].user, r"PROBE\dev");
        assert_eq!(vm.logins[0].password.as_deref(), Some("vmlab123!"));
        assert_eq!(vm.logins[0].default, Some(true));
        // Unwritten `elevated` stays unwritten in the model: §19.2's Linux
        // rule is about the field being declared, not about its value, and
        // the default it would take depends on the guest family.
        assert_eq!(vm.logins[0].elevated, None);
        assert_eq!(vm.logins[1].elevated, Some(false));
        assert!(vm.logins[1].password.is_some());

        let container = &lf.lab.containers[0];
        assert_eq!(container.logins.len(), 1);
        assert_eq!(container.logins[0].user, "dev");
        assert!(container.logins[0].password.is_none());
        assert_eq!(container.logins[0].default, None);
    }

    /// §19.2: "a lone `login {}` is the default implicitly, matching `@dev`'s
    /// shape" — so a single declaration never has to meet the concept.
    #[test]
    fn a_lone_login_is_the_default_without_saying_so() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  vm "one" { template = "x86_64/t"  login "dev" { user = "dev" } }
  vm "two" {
    template = "x86_64/t"
    login "dev"   { user = "dev" }
    login "admin" { user = "admin" default = true }
  }
  vm "none" { template = "x86_64/t" }
}
"#;
        let lf = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap();
        let vms = &lf.lab.vms;
        assert_eq!(
            default_login(&vms[0].logins).map(|l| l.label.as_str()),
            Some("dev")
        );
        assert_eq!(
            default_login(&vms[1].logins).map(|l| l.label.as_str()),
            Some("admin")
        );
        assert!(default_login(&vms[2].logins).is_none());
    }

    /// The label is the SSH username selector (§19.2), so it is required —
    /// an unlabelled `login {}` is not a nameless identity, it is a mistake.
    #[test]
    fn a_login_needs_a_label_and_a_user() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t"  login { user = "dev" } }
  vm "b" { template = "x86_64/t"  login "dev" { } }
}
"#;
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        let messages: Vec<&str> = err.issues.iter().map(|i| i.message.as_str()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("`login` requires a name label")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("missing required field `user`")),
            "{messages:?}"
        );
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
    fn rejects_the_removed_container_restart_field_at_its_source_line() {
        let src = "import <vmlab.wcl>\nlab \"x\" {\n  container \"web\" { image = \"nginx\" restart = \"always\" }\n}\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        let restart_offset = src.find("restart =").unwrap();

        assert!(
            err.issues.iter().any(|issue| {
                issue.message.contains("restart")
                    && issue.span.is_some_and(|span| {
                        let offset = span.offset();
                        offset >= restart_offset
                            && offset < src[restart_offset..].find('\n').unwrap() + restart_offset
                    })
            }),
            "expected an unknown restart-field error on its source line, got: {:?}",
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

    /// A decorator nothing declares is a typo, not an annotation WCL ignores
    /// — `@dve` for the `@dev` the schema does declare (PRD §19.1).
    #[test]
    fn rejects_an_undeclared_decorator_on_a_block() {
        let src =
            "import <vmlab.wcl>\nlab \"x\" {\n  @dve\n  vm \"a\" { template = \"x86_64/t\" }\n}\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues
                .iter()
                .any(|i| i.message.contains("'dve'") && i.message.contains("@decorator")),
            "expected the undeclared decorator to be named, got: {:?}",
            err.issues
        );
    }

    /// The schema's own metadata decorators say where they may be written, so
    /// one of them on a lab block is an error rather than a silent no-op.
    #[test]
    fn rejects_a_schema_metadata_decorator_on_a_block() {
        let src = "import <vmlab.wcl>\nlab \"x\" {\n  @options([\"a\"])\n  vm \"a\" { template = \"x86_64/t\" }\n}\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues
                .iter()
                .any(|i| i.message.contains("'@options'")
                    && i.message.contains("'block' position")),
            "expected a position-applicability error, got: {:?}",
            err.issues
        );
    }

    /// A declared decorator's arguments are typed, so a wrong-typed or
    /// unknown one is an error — `@dev`'s own, since the schema declares it
    /// (PRD §19.1).
    #[test]
    fn rejects_wrong_typed_and_unknown_decorator_arguments() {
        let wrong_type = "import <vmlab.wcl>\nlab \"x\" {\n  @dev(default = 42)\n  vm \"a\" { template = \"x86_64/t\" }\n}\n";
        let err = load_lab_source(wrong_type, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues
                .iter()
                .any(|i| i.message.contains("'default'") && i.message.contains("bool")),
            "expected a typed-argument error, got: {:?}",
            err.issues
        );

        let unknown_arg = "import <vmlab.wcl>\nlab \"x\" {\n  @dev(worksapce = \"./src\")\n  vm \"a\" { template = \"x86_64/t\" }\n}\n";
        let err = load_lab_source(unknown_arg, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues
                .iter()
                .any(|i| i.message.contains("'worksapce'") && i.message.contains("not declared")),
            "expected an unknown-argument error, got: {:?}",
            err.issues
        );
    }

    /// `@applies_to` narrows a decorator to the block kinds it is meant for,
    /// and the error points at the decorator rather than at the block. `@dev`
    /// names `vm` and `container`, so it is rejected on every other kind —
    /// `nic {}` is §19.1's own example.
    #[test]
    fn rejects_a_decorator_on_a_block_kind_it_does_not_apply_to() {
        let src = "import <vmlab.wcl>\n\
                   lab \"x\" {\n  \
                     vm \"a\" { template = \"x86_64/t\"\n    \
                       @dev\n    \
                       nic { nat = true }\n  \
                     }\n\
                   }\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        let use_site = src.rfind("@dev").expect("the decorator use site");
        assert!(
            err.issues.iter().any(|issue| {
                issue.message.contains("nic")
                    && issue.span.is_some_and(|span| span.offset() == use_site + 1)
            }),
            "expected a kind-applicability error spanning the decorator, got: {:?}",
            err.issues
        );
    }

    /// A decorator that is not declared `repeatable` may appear once per node,
    /// so `@dev @dev` on one machine is an error rather than a second reading
    /// of the same declaration.
    #[test]
    fn rejects_a_repeated_at_most_once_decorator() {
        let src = "import <vmlab.wcl>\n\
                   lab \"x\" {\n  \
                     @dev\n  \
                     @dev\n  \
                     vm \"a\" { template = \"x86_64/t\" }\n\
                   }\n";
        let err = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap_err();
        assert!(
            err.issues
                .iter()
                .any(|i| i.message.contains("at most once")),
            "expected a cardinality error, got: {:?}",
            err.issues
        );
    }

    /// The declaration itself: `@dev` on both machine kinds, every argument
    /// optional, and the arguments landing in the model (PRD §19.1).
    #[test]
    fn dev_decorators_extract_on_both_machine_kinds() {
        let src = r#"import <vmlab.wcl>
lab "x" {
  @dev(default = true, workspace = "./src", workspace_guest = "C:\\src")
  vm "dev01" { template = "x86_64/win" }
  @dev
  container "buildbox" { image = "sdk:9.0" }
  vm "dc01" { template = "x86_64/win" }
}
"#;
        let lf = load_lab_source(src, "<test>", Path::new("/tmp")).unwrap();
        let dev01 = lf.lab.vms[0].dev.as_ref().expect("@dev on the vm");
        assert!(dev01.default);
        assert_eq!(dev01.workspace.as_deref(), Some(Path::new("./src")));
        assert_eq!(dev01.workspace_guest.as_deref(), Some("C:\\src"));
        // The span is the decorator's, so a diagnostic points at `@dev(…)`
        // rather than at the machine block around it.
        assert_eq!(dev01.span.0, src.find("@dev(").unwrap());

        // A bare `@dev` carries nothing and is still a dev machine.
        let bare = lf.lab.containers[0]
            .dev
            .as_ref()
            .expect("@dev on the container");
        assert!(!bare.default);
        assert!(bare.workspace.is_none());
        assert!(bare.workspace_guest.is_none());

        // And an undecorated machine is not one.
        assert!(lf.lab.vms[1].dev.is_none());
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

    /// Every shipped example **lab** must parse into the typed model, for the
    /// same reason: an example nobody can run is documentation that lies.
    ///
    /// This is the parse half only — resolving a `template` needs a store,
    /// which a unit test has no business having. The wscript half of the same
    /// promise is `scripting::example_tests::shipped_examples_compile`.
    #[test]
    fn shipped_example_labs_parse() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(root).unwrap() {
            let dir = entry.unwrap().path();
            let wcl = dir.join(crate::paths::LAB_FILE);
            if !wcl.is_file() {
                continue;
            }
            let src = std::fs::read_to_string(&wcl).unwrap();
            let lf = load_lab_source(&src, "vmlab.wcl", &dir)
                .unwrap_or_else(|e| panic!("{}: {e:?}", wcl.display()));
            assert!(
                !lf.lab.name.is_empty(),
                "{}: the lab has no name",
                wcl.display()
            );
            checked += 1;
        }
        assert!(checked >= 6, "expected example labs, found {checked}");
    }

    /// The §19.8 worked examples carry the declarations they exist to
    /// demonstrate: a `@dev` machine with a workspace and a `login {}`, on
    /// **both** machine kinds — one contract, every machine kind, shown
    /// rather than asserted.
    #[test]
    fn the_worked_examples_declare_a_dev_machine_of_each_kind() {
        let load = |name: &str| {
            let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/examples"))
                .join(name);
            let src = std::fs::read_to_string(dir.join(crate::paths::LAB_FILE)).unwrap();
            load_lab_source(&src, "vmlab.wcl", &dir).unwrap().lab
        };

        for (example, machine, is_vm) in [
            ("dev-vscode-windows", "dev01", true),
            ("dev-neovim-container", "dev01", false),
        ] {
            let lab = load(example);
            let m = lab.machine(machine).unwrap_or_else(|| {
                panic!("{example} declares no machine `{machine}`");
            });
            let dev = m
                .dev()
                .unwrap_or_else(|| panic!("{example}/{machine} carries no @dev"));
            assert!(
                dev.workspace.is_some(),
                "{example}: a worked example without a workspace demonstrates half of §19"
            );
            // The identity the editor bits land as — without it the provision
            // runs as the machine and §19.8's guarantee is untested.
            assert!(
                m.logins().iter().any(|l| l.label == "dev"),
                "{example}/{machine} declares no `dev` login"
            );
            // And the two are different machine kinds, which is the whole
            // reason there are two examples.
            assert_eq!(
                matches!(m, crate::config::model::MachineCfg::Vm(_)),
                is_vm,
                "{example}: the pair must be split by machine kind"
            );
        }
    }
}
