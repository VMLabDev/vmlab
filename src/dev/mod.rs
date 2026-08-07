//! Dev machines (PRD §19.1): who carries `@dev`, which one is the lab's
//! default, and what the decorator's unset arguments resolve to.
//!
//! Precedence is **`@dev` argument > profile > vmlab floor**, and this is the
//! only implementation of it. It deliberately sits apart from the hardware
//! resolver ADR-0008 owns: dev defaults share its shape but are not hardware,
//! and folding them in would muddy "nothing else may reimplement it" for the
//! chain that actually boots a machine.
//!
//! Two rules the resolution rests on, both from §19.1:
//!
//! - **A profile carrying no dev keys still hosts a dev machine.** A missing
//!   key means the floor applies, never that the profile cannot be a dev
//!   target — otherwise `@dev` on the `custom` profile would be impossible.
//! - **The default dev machine is the one declaring `default = true`, or the
//!   only machine carrying `@dev`.** "First in file order wins" was rejected:
//!   declaration order already means something in vmlab, and overloading it
//!   would let a block reorder silently move the default.
//!
//! [`select`] carries the other half of the question, the one `vmlab.wcl`
//! structurally cannot answer: which of those machines is *mine* (§19.7).

pub mod select;

use std::path::PathBuf;

use crate::config::model::{DevDecl, Lab, MachineCfg};
use crate::profiles::Profile;

/// Where an `@dev` workspace lands when neither the decorator nor the profile
/// says. Linux-shaped because that is what a guest vmlab knows nothing else
/// about most likely is — a container micro-VM, or a VM on the `custom`
/// profile — and a lab author who wants another path writes one.
pub const WORKSPACE_GUEST_FLOOR: &str = "/src";

/// One machine's `@dev` declaration, resolved — the dev counterpart of
/// [`ResolvedVm`](crate::qemu::resolve::ResolvedVm), and deliberately not the
/// same resolver (ADR-0008).
///
/// Note that `default` is a *lab-level* answer: a lone `@dev` machine is the
/// default without declaring it, so this is not simply the decorator's
/// argument read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDev {
    /// The machine carrying the decorator.
    pub name: String,
    /// This is the lab's default dev machine.
    pub default: bool,
    /// Host directory whose contents sync into the workspace, relative to the
    /// lab root (§19.6). `None` = this dev machine has no workspace; it is
    /// still attachable.
    pub workspace: Option<PathBuf>,
    /// Guest path the workspace lands at — `@dev` > profile > floor.
    pub workspace_guest: String,
}

/// Resolve one machine's dev declaration against its guest OS profile.
///
/// `profile` is the machine's **effective** profile: a VM's may come from its
/// template rather than its own block (§5.2), which only the caller can know.
/// `None` — no profile at all, or one this build cannot find — resolves to the
/// floor, which is the same answer a profile declaring no dev keys gives.
fn resolve(name: &str, dev: &DevDecl, profile: Option<&Profile>) -> ResolvedDev {
    ResolvedDev {
        name: name.to_string(),
        default: dev.default,
        workspace: dev.workspace.clone(),
        workspace_guest: dev
            .workspace_guest
            .clone()
            .or_else(|| profile.and_then(|p| p.workspace_guest.clone()))
            .unwrap_or_else(|| WORKSPACE_GUEST_FLOOR.to_string()),
    }
}

/// Every `@dev` machine in a lab, in declaration order, with `default`
/// resolved — so the caller reads the answer rather than the argument.
///
/// `profile` answers with a machine's effective guest OS profile; a caller
/// with no profile set at hand passes `|_| None` and gets the floor.
///
/// A lab whose file declares two `default = true` machines is a validation
/// error (§5.1); resolution reports both as declared rather than picking one,
/// because silently choosing is what the rule exists to prevent.
pub fn machines<'a>(
    lab: &'a Lab,
    profile: impl Fn(MachineCfg<'a>) -> Option<Profile>,
) -> Vec<ResolvedDev> {
    let mut out: Vec<ResolvedDev> = lab
        .machines()
        .filter_map(|m| {
            let dev = m.dev()?;
            Some(resolve(m.name(), dev, profile(m).as_ref()))
        })
        .collect();
    // The lone dev machine never has to meet the concept: it is the default
    // implicitly, whether or not it says so. That includes one that wrote
    // `default = false` — "where none carries `default = true`, the only
    // machine carrying `@dev`" is the whole rule, and the alternative is a
    // lab with exactly one dev machine and no default to attach to.
    if out.len() == 1 {
        out[0].default = true;
    }
    out
}

impl From<ResolvedDev> for crate::status::DevStatus {
    /// What `status` reports (ADR-0004). The host path crosses as text: the
    /// projection is a wire type, and every surface that shows a workspace
    /// shows the path the lab file wrote.
    fn from(dev: ResolvedDev) -> Self {
        Self {
            default: dev.default,
            workspace: dev.workspace.map(|p| p.display().to_string()),
            workspace_guest: dev.workspace_guest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_lab_source;
    use crate::profiles::ProfileSet;
    use std::path::Path;

    /// A lab body, parsed through the real loader — `@dev` is schema-checked
    /// on the way in, so a test that mis-writes it fails here.
    fn lab(src: &str) -> Lab {
        let full = format!("import <vmlab.wcl>\nlab \"t\" {{\n{src}\n}}\n");
        load_lab_source(&full, "<test>", Path::new("/tmp"))
            .expect("the lab parses")
            .lab
    }

    /// Resolve a lab's dev machines against the shipped profiles, keyed by
    /// each machine's own `profile` — the layer between `@dev` and the floor.
    fn resolved(src: &str) -> Vec<ResolvedDev> {
        let lab = lab(src);
        let profiles = ProfileSet::shipped().unwrap();
        machines(&lab, |m| {
            // The declared profile only: a test lab has no template store to
            // inherit one from, which is the layer `LabRuntime` supplies.
            let declared = match m {
                MachineCfg::Vm(v) => v.profile.as_deref(),
                MachineCfg::Container(c) => c.profile.as_deref(),
            };
            declared.and_then(|p| profiles.get(p).cloned())
        })
    }

    /// The lab's default dev machine, as every consumer reads it.
    fn default_of(devs: &[ResolvedDev]) -> Option<&ResolvedDev> {
        devs.iter().find(|d| d.default)
    }

    const DEV_VM: &str =
        r#"@dev vm "dev01" { template = "x86_64/win" profile = "windows-server" }"#;

    /// A bare `@dev` is a complete dev machine: no arguments, and every
    /// unset one resolves.
    #[test]
    fn a_bare_dev_resolves_from_the_profile() {
        let devs = resolved(DEV_VM);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "dev01");
        assert_eq!(devs[0].workspace_guest, "C:\\src");
        assert_eq!(devs[0].workspace, None);
    }

    /// The decorator beats the profile, which beats the floor.
    #[test]
    fn precedence_is_decorator_over_profile_over_floor() {
        // Decorator over profile.
        let devs = resolved(
            r#"@dev(workspace = "./src", workspace_guest = "D:\\work")
               vm "dev01" { template = "x86_64/win" profile = "windows-server" }"#,
        );
        assert_eq!(devs[0].workspace_guest, "D:\\work");
        assert_eq!(devs[0].workspace, Some(PathBuf::from("./src")));

        // Profile over floor: a Linux profile lands somewhere else than a
        // Windows one, which is the whole reason the key is profile-sourced.
        let devs =
            resolved(r#"@dev container "buildbox" { image = "sdk:9.0" profile = "container" }"#);
        assert_eq!(devs[0].workspace_guest, "/src");
    }

    /// A profile with no dev keys still hosts a dev machine — the floor
    /// applies. `custom` is the proving case: it assumes nothing, and `@dev`
    /// on it has to work.
    #[test]
    fn a_profile_with_no_dev_keys_still_hosts_a_dev_machine() {
        let devs = resolved(
            r#"@dev vm "dev01" { template = "scratch" arch = "x86_64" profile = "custom" disk = 10GiB }"#,
        );
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].workspace_guest, WORKSPACE_GUEST_FLOOR);

        // And so does a machine naming no profile at all.
        let devs =
            resolved(r#"@dev container "buildbox" { image = "sdk:9.0" cpus = 1 memory = 512MiB }"#);
        assert_eq!(devs[0].workspace_guest, WORKSPACE_GUEST_FLOOR);
    }

    /// The lone dev machine is the default without saying so, so a lab with
    /// one never meets the concept.
    #[test]
    fn the_only_dev_machine_is_the_default_implicitly() {
        let devs = resolved(&format!(
            "{DEV_VM}\nvm \"dc01\" {{ template = \"x86_64/win\" }}"
        ));
        assert_eq!(
            devs.len(),
            1,
            "the undecorated machine is not a dev machine"
        );
        assert!(devs[0].default);
        assert_eq!(default_of(&devs).map(|d| d.name.as_str()), Some("dev01"));
    }

    /// With more than one, the declaration decides — and nothing else does.
    #[test]
    fn the_default_is_declared_once_there_is_more_than_one() {
        let devs = resolved(&format!(
            "{DEV_VM}\n@dev(default = true) container \"buildbox\" {{ image = \"sdk:9.0\" profile = \"container\" }}"
        ));
        assert_eq!(default_of(&devs).map(|d| d.name.as_str()), Some("buildbox"));

        // None declared: there is no default, rather than a machine picked by
        // file order.
        let devs = resolved(&format!(
            "{DEV_VM}\n@dev container \"buildbox\" {{ image = \"sdk:9.0\" profile = \"container\" }}"
        ));
        assert!(default_of(&devs).is_none());
        assert!(devs.iter().all(|d| !d.default));
    }

    /// Zero is normal: most labs are not dev labs.
    #[test]
    fn a_lab_with_no_dev_machine_resolves_to_nothing() {
        let devs = resolved(r#"vm "dc01" { template = "x86_64/win" }"#);
        assert!(devs.is_empty());
        assert!(default_of(&devs).is_none());
    }
}
