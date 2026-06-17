//! Guest OS profiles (PRD §5.3): named bundles of known-good hardware
//! defaults. Profiles are data — shipped as WCL, user-overridable and
//! user-extensible from `~/.config/vmlab/profiles/*.wcl`. Inheritance
//! precedence is VM block > template > profile; the profile is the floor.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use wcl_lang::{Document, Environment, Registry, Value, disk_loader};

use crate::config::model::parse_size;

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
    let schema_errors = doc.schema_errors();
    if !schema_errors.is_empty() {
        let msgs: Vec<String> = schema_errors.iter().map(|e| e.to_string()).collect();
        return Err(anyhow!("schema errors in {name}: {}", msgs.join("; ")));
    }

    let mut out = Vec::new();
    for block in doc.blocks() {
        if block.kind() != "profile" {
            continue;
        }
        let label = block
            .labels()
            .map_err(|e| anyhow!("cannot evaluate profile label: {e}"))?
            .into_iter()
            .next();
        let Some(Value::Utf8(pname)) = label else {
            return Err(anyhow!("profile block in {name} requires a name label"));
        };

        let get_str = |field: &str| -> Result<Option<String>> {
            match block.field(field) {
                None => Ok(None),
                Some(f) => match f.value() {
                    Ok(Value::Utf8(s)) => Ok(Some(s.clone())),
                    Ok(Value::None) => Ok(None),
                    Ok(other) => Err(anyhow!(
                        "profile {pname}: `{field}` must be a string, got {other:?}"
                    )),
                    Err(e) => Err(anyhow!("profile {pname}: cannot evaluate `{field}`: {e}")),
                },
            }
        };
        let get_bool = |field: &str| -> Result<Option<bool>> {
            match block.field(field) {
                None => Ok(None),
                Some(f) => match f.value() {
                    Ok(Value::Bool(b)) => Ok(Some(*b)),
                    Ok(Value::None) => Ok(None),
                    Ok(other) => Err(anyhow!(
                        "profile {pname}: `{field}` must be a bool, got {other:?}"
                    )),
                    Err(e) => Err(anyhow!("profile {pname}: cannot evaluate `{field}`: {e}")),
                },
            }
        };

        let machine = match get_str("machine")?.as_deref() {
            None => None,
            Some("q35") => Some(Machine::Q35),
            Some("pc") => Some(Machine::I440fx),
            Some(other) => {
                return Err(anyhow!(
                    "profile {pname}: unknown machine `{other}` (expected q35, pc)"
                ));
            }
        };
        let firmware = match get_str("firmware")?.as_deref() {
            None => None,
            Some("ovmf") => Some(FirmwareKind::Ovmf),
            Some("seabios") => Some(FirmwareKind::Seabios),
            Some(other) => {
                return Err(anyhow!(
                    "profile {pname}: unknown firmware `{other}` (expected ovmf, seabios)"
                ));
            }
        };
        let disk_bus = match get_str("disk_bus")?.as_deref() {
            None => None,
            Some("virtio") => Some(DiskBus::Virtio),
            Some("ide") => Some(DiskBus::Ide),
            Some("sata") => Some(DiskBus::Sata),
            Some(other) => {
                return Err(anyhow!(
                    "profile {pname}: unknown disk_bus `{other}` (expected virtio, ide, sata)"
                ));
            }
        };
        let cpus = match block.field("cpus") {
            None => None,
            Some(f) => match f.value() {
                Ok(Value::I64(n)) if *n > 0 => Some(*n as u32),
                Ok(Value::None) => None,
                Ok(other) => {
                    return Err(anyhow!(
                        "profile {pname}: cpus must be a positive integer, got {other:?}"
                    ));
                }
                Err(e) => return Err(anyhow!("profile {pname}: cannot evaluate cpus: {e}")),
            },
        };
        let memory = match get_str("memory")? {
            None => None,
            Some(s) => Some(parse_size(&s).map_err(|e| anyhow!("profile {pname}: {e}"))?),
        };
        let input_transport = match get_str("input_transport")?.as_deref() {
            None | Some("qmp") => InputTransport::Qmp,
            Some("vnc") => InputTransport::Vnc,
            Some(other) => {
                return Err(anyhow!(
                    "profile {pname}: unknown input_transport `{other}` (expected qmp, vnc)"
                ));
            }
        };

        out.push(Profile {
            name: pname.clone(),
            description: get_str("description")?,
            machine,
            firmware,
            secure_boot: get_bool("secure_boot")?,
            tpm: get_bool("tpm")?,
            disk_bus,
            nic_model: get_str("nic_model")?,
            display: get_str("display")?,
            cpus,
            memory,
            agent_channel: get_bool("agent_channel")?.unwrap_or(true),
            input_transport,
        });
    }
    Ok(out)
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

    #[test]
    fn bad_profile_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("bad.wcl"),
            "import <vmlab-profile.wcl>\nprofile \"x\" { machine = \"vax\" }\n",
        )
        .unwrap();
        let err = ProfileSet::load(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("unknown machine"));
    }

    #[test]
    fn missing_import_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("noimport.wcl"), "profile \"x\" { }\n").unwrap();
        let err = ProfileSet::load(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("vmlab-profile.wcl"));
    }
}
