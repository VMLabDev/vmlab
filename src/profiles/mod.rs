//! Guest OS profiles (PRD §5.3): named bundles of known-good hardware
//! defaults. Profiles are data — shipped as WCL, user-overridable and
//! user-extensible from `~/.config/vmlab/profiles/*.wcl`. Inheritance
//! precedence is VM block > template > profile; the profile is the floor.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use wcl_lang::{Block, Document, Environment, Registry, disk_loader};

use crate::config::IssueList;
use crate::config::block::{Reader, Unspan, finish};

pub const PROFILE_SCHEMA_WCL: &str = include_str!("profile_schema.wcl");
pub const SHIPPED_PROFILES_WCL: &str = include_str!("shipped.wcl");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    Q35,
    I440fx,
}

impl Machine {
    pub fn qemu_name(self) -> &'static str {
        match self {
            Machine::Q35 => "q35",
            Machine::I440fx => "pc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskBus {
    Virtio,
    Ide,
    Sata,
}

/// How scripted input (`vm.send_keys`/`mouse_*`) reaches the guest. QMP
/// `send-key` only drives the PS/2 keyboard, which USB-HID-only guests
/// (macOS/OpenCore) ignore; `Vnc` routes input over the VM's VNC socket
/// instead — the path a real viewer uses (PRD §10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputTransport {
    #[default]
    Qmp,
    Vnc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareKind {
    Ovmf,
    Seabios,
}

/// One guest OS profile. Every field is optional: `custom` assumes nothing
/// (PRD §5.3) and the QEMU defaults apply for whatever stays unset.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub name: String,
    /// Human-readable summary from the profile's WCL `description`. Nothing
    /// surfaces it yet (there is no `profile list` verb); parsed for schema
    /// parity.
    pub description: Option<String>,
    pub machine: Option<Machine>,
    pub firmware: Option<FirmwareKind>,
    pub secure_boot: Option<bool>,
    pub tpm: Option<bool>,
    pub disk_bus: Option<DiskBus>,
    pub nic_model: Option<String>,
    pub display: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub agent_channel: bool,
    pub input_transport: InputTransport,
    /// The guest OS mounts virtiofs natively (`mount -t virtiofs` on Linux,
    /// the virtio-win driver + WinFsp on Windows) — makes it a candidate
    /// for `transport = "auto"` shares (§7.5).
    pub virtiofs: bool,
}

/// The full profile set: shipped profiles plus user overrides/extensions.
#[derive(Debug, Clone)]
pub struct ProfileSet {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileSet {
    /// Shipped profiles only.
    pub fn shipped() -> Result<Self> {
        let mut set = Self {
            profiles: BTreeMap::new(),
        };
        set.merge_source(SHIPPED_PROFILES_WCL, "<shipped profiles>")?;
        Ok(set)
    }

    /// Shipped profiles plus `*.wcl` files from the user profile directory
    /// (`~/.config/vmlab/profiles`). A user profile with a shipped name
    /// replaces it.
    pub fn load(user_dir: &Path) -> Result<Self> {
        let mut set = Self::shipped()?;
        if user_dir.is_dir() {
            let mut paths: Vec<_> = std::fs::read_dir(user_dir)
                .with_context(|| format!("reading {}", user_dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "wcl"))
                .collect();
            paths.sort();
            for path in paths {
                let source = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                set.merge_source(&source, &path.display().to_string())
                    .with_context(|| format!("loading profiles from {}", path.display()))?;
            }
        }
        Ok(set)
    }

    /// Standard load from the XDG config dir.
    pub fn load_default() -> Result<Self> {
        Self::load(&crate::paths::config_dir().join("profiles"))
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.profiles.contains_key(name)
    }

    /// All profile names (the web catalog endpoint and tests use this).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    fn merge_source(&mut self, source: &str, name: &str) -> Result<()> {
        for profile in parse_profiles(source, name)? {
            self.profiles.insert(profile.name.clone(), profile);
        }
        Ok(())
    }
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("vmlab-profile.wcl", PROFILE_SCHEMA_WCL);
    r
}

fn parse_profiles(source: &str, name: &str) -> Result<Vec<Profile>> {
    if !source.contains("import <vmlab-profile.wcl>") {
        return Err(anyhow!(
            "profile file {name} is missing `import <vmlab-profile.wcl>` at the top"
        ));
    }
    let doc = Document::open_at_with_loader(
        source,
        name,
        None,
        &Environment::new(),
        registry().loader(disk_loader()),
    )
    .map_err(|e| anyhow!("parse error in {name}: {e}"))?;
    let mut issues = crate::config::schema_issues(&doc);
    let mut out = Vec::new();
    for block in doc.blocks() {
        if block.kind() == "profile"
            && let Some(p) = extract_profile(&block, &mut issues)
        {
            out.push(p);
        }
    }
    Ok(finish(name, source, issues, Some(out))?)
}

/// The profile field mapping. Reading, coercion, spans and wording all come
/// from [`crate::config::block`] (ADR-0006).
fn extract_profile(b: &Block, issues: &mut IssueList) -> Option<Profile> {
    let mut r = Reader::new(b, issues);
    // Read the label, but keep reading: a nameless profile should not
    // swallow the diagnostics for everything else wrong with it.
    let name = r.label();
    let profile = Profile {
        name: String::new(),
        description: r.string("description").unspan(),
        machine: r
            .keyword("machine", &[("q35", Machine::Q35), ("pc", Machine::I440fx)])
            .unspan(),
        firmware: r
            .keyword(
                "firmware",
                &[
                    ("ovmf", FirmwareKind::Ovmf),
                    ("seabios", FirmwareKind::Seabios),
                ],
            )
            .unspan(),
        secure_boot: r.bool("secure_boot").unspan(),
        tpm: r.bool("tpm").unspan(),
        disk_bus: r
            .keyword(
                "disk_bus",
                &[
                    ("virtio", DiskBus::Virtio),
                    ("ide", DiskBus::Ide),
                    ("sata", DiskBus::Sata),
                ],
            )
            .unspan(),
        nic_model: r.string("nic_model").unspan(),
        display: r.string("display").unspan(),
        cpus: r.int_at_least("cpus", 1).unspan(),
        memory: r.size("memory").unspan(),
        agent_channel: r.bool("agent_channel").unspan().unwrap_or(true),
        input_transport: r
            .keyword(
                "input_transport",
                &[("qmp", InputTransport::Qmp), ("vnc", InputTransport::Vnc)],
            )
            .unspan()
            .unwrap_or_default(),
        virtiofs: r.bool("virtiofs").unspan().unwrap_or(false),
    };
    Some(Profile {
        name: name?,
        ..profile
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_profiles_load() {
        let set = ProfileSet::shipped().unwrap();
        let names: Vec<&str> = set.names().collect();
        for expected in [
            "windows-11",
            "windows-10",
            "windows-server",
            "windows-legacy",
            "linux-modern",
            "linux-generic",
            "custom",
        ] {
            assert!(
                names.contains(&expected),
                "missing shipped profile {expected}"
            );
        }

        let win11 = set.get("windows-11").unwrap();
        assert_eq!(win11.machine, Some(Machine::Q35));
        assert_eq!(win11.firmware, Some(FirmwareKind::Ovmf));
        assert_eq!(win11.secure_boot, Some(true));
        assert_eq!(win11.tpm, Some(true));
        assert_eq!(win11.memory, Some(8 << 30));

        let legacy = set.get("windows-legacy").unwrap();
        assert_eq!(legacy.machine, Some(Machine::I440fx));
        assert_eq!(legacy.firmware, Some(FirmwareKind::Seabios));
        assert_eq!(legacy.disk_bus, Some(DiskBus::Ide));
        assert_eq!(legacy.nic_model.as_deref(), Some("e1000"));

        let custom = set.get("custom").unwrap();
        assert!(custom.machine.is_none());
        assert!(custom.firmware.is_none());
        assert!(custom.agent_channel);
    }

    #[test]
    fn user_profiles_override_and_extend() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("mine.wcl"),
            r#"import <vmlab-profile.wcl>
profile "windows-11" { machine = "pc" }
profile "freebsd" { machine = "q35" firmware = "seabios" }
"#,
        )
        .unwrap();
        let set = ProfileSet::load(tmp.path()).unwrap();
        // Override replaces the shipped definition entirely.
        assert_eq!(
            set.get("windows-11").unwrap().machine,
            Some(Machine::I440fx)
        );
        assert!(set.get("windows-11").unwrap().firmware.is_none());
        // Extension adds a new name.
        assert!(set.exists("freebsd"));
        // Shipped names survive.
        assert!(set.exists("linux-modern"));
    }

    /// The diagnostics a profile author now gets — the same wording a lab
    /// author gets for the same mistake, anchored at the line (ADR-0006).
    fn profile_err(body: &str) -> String {
        let source = format!("import <vmlab-profile.wcl>\n{body}");
        let err = parse_profiles(&source, "p.wcl").expect_err("profile should be rejected");
        format!("{err:#}")
    }

    #[test]
    fn bad_profile_rejected_with_the_shared_wording() {
        assert_eq!(
            profile_err("profile \"x\" { machine = \"vax\" }\n"),
            "1 error(s) in p.wcl\n  p.wcl:2:15: `machine` must be one of q35, pc, got `vax`"
        );
        assert!(
            profile_err("profile \"x\" { firmware = \"bios\" }\n")
                .contains("`firmware` must be one of ovmf, seabios, got `bios`")
        );
        assert!(
            profile_err("profile \"x\" { disk_bus = \"scsi\" }\n")
                .contains("`disk_bus` must be one of virtio, ide, sata, got `scsi`")
        );
        assert!(
            profile_err("profile \"x\" { input_transport = \"usb\" }\n")
                .contains("`input_transport` must be one of qmp, vnc, got `usb`")
        );
    }

    #[test]
    fn profile_type_errors_read_like_lab_file_ones() {
        assert!(
            profile_err("profile \"x\" { nic_model = 3 }\n")
                .contains("`nic_model` must be a string, got an integer")
        );
        assert!(
            profile_err("profile \"x\" { tpm = \"yes\" }\n")
                .contains("`tpm` must be a bool, got a string")
        );
        assert!(
            profile_err("profile \"x\" { cpus = 0 }\n")
                .contains("`cpus` must be at least 1, got 0")
        );
        assert!(
            profile_err("profile \"x\" { memory = -1 }\n")
                .contains("`memory` must be at least 0, got -1")
        );
    }

    #[test]
    fn every_mistake_in_a_profile_file_is_reported_at_once() {
        let err =
            profile_err("profile \"a\" { machine = \"vax\" }\nprofile \"b\" { tpm = \"yes\" }\n");
        // Both mistakes in one pass, each at its own line — the second is
        // no longer hidden behind the first.
        assert!(
            err.contains("p.wcl:2:15: `machine` must be one of"),
            "{err}"
        );
        assert!(err.contains("p.wcl:3:15: `tpm` must be a bool"), "{err}");
    }

    /// A profile with no name label still reports what else is wrong with
    /// it — the label failure does not swallow the rest of the pass.
    #[test]
    fn a_nameless_profile_still_reports_its_other_mistakes() {
        let err = profile_err("profile { machine = \"vax\" }\n");
        assert!(err.contains("`profile` requires a name label"), "{err}");
        assert!(err.contains("`machine` must be one of q35, pc"), "{err}");
    }

    /// A field the profile schema does not name is the schema's to reject,
    /// not the extractor's — but it must still reach the user, positioned.
    #[test]
    fn an_unknown_field_is_rejected_by_the_schema() {
        let err = profile_err("profile \"x\" { bogus = 1 }\n");
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("p.wcl:2:"), "unpositioned: {err}");
    }

    /// Spans survive as spans, not just as rendered text, whichever config
    /// file they came from (ADR-0006, story 19).
    #[test]
    fn a_profile_issue_carries_a_span_a_surface_can_use() {
        let source = "import <vmlab-profile.wcl>\nprofile \"x\" { machine = \"vax\" }\n";
        let err = parse_profiles(source, "p.wcl").unwrap_err();
        let issues = err
            .downcast_ref::<crate::config::block::IssueError>()
            .expect("profile errors carry their issues")
            .diagnostic();
        let span = issues.issues[0].span.expect("positioned");
        assert_eq!(
            &source[span.offset()..span.offset() + span.len()],
            "machine = \"vax\""
        );
    }

    #[test]
    fn missing_import_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("noimport.wcl"), "profile \"x\" { }\n").unwrap();
        let err = ProfileSet::load(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("vmlab-profile.wcl"));
    }
}
