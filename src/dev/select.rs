//! **Which dev machine a `vmlab dev` verb is about** (PRD §19.7).
//!
//! The ladder is fixed and it **never guesses**:
//!
//! 1. an explicit argument,
//! 2. `VMLAB_DEV_MACHINE`,
//! 3. `@dev(default = true)`, and with it the lone `@dev` machine, which is
//!    the default implicitly ([`super::machines`]),
//! 4. otherwise an **error listing the candidates**.
//!
//! Every rung that names a machine is checked rather than trusted: a rung
//! naming something that is not a dev machine in this lab is an error at that
//! rung, never a silent fall through to the next one. Falling through is the
//! guess this ladder exists to prevent — an environment variable left over
//! from another lab would otherwise land the developer on a machine nothing
//! ever said out loud.

use anyhow::{Result, bail};

use super::ResolvedDev;

/// The environment rung of the ladder — one lab's dev machine for one shell,
/// without recording anything.
pub const ENV_VAR: &str = "VMLAB_DEV_MACHINE";

/// The environment rung, read from this process's environment.
pub fn env_selection() -> Option<String> {
    std::env::var(ENV_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Walk the ladder. `devs` is the lab's dev machines as [`super::machines`]
/// resolved them, so `default` already covers both "declared it" and "is the
/// only one".
///
/// Every input is passed in rather than read here: the resolution is the part
/// worth testing, and the environment is the caller's.
pub fn resolve<'a>(
    lab: &str,
    devs: &'a [ResolvedDev],
    argument: Option<&str>,
    env: Option<&str>,
) -> Result<&'a ResolvedDev> {
    for (named, source) in [(argument, Naming::Argument), (env, Naming::Environment)] {
        let Some(name) = named else { continue };
        match devs.iter().find(|d| d.name == name) {
            Some(dev) => return Ok(dev),
            None => bail!(rejected(lab, devs, name, source)),
        }
    }

    if let Some(dev) = devs.iter().find(|d| d.default) {
        return Ok(dev);
    }
    bail!(undecided(lab, devs))
}

/// The rungs that *name* a machine — the ones that can be wrong, and so the
/// only ones a rejection can come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    Argument,
    Environment,
}

/// A rung named something this lab does not offer. Which rung matters as much
/// as which name: a mistyped argument and a stale environment variable are
/// undone in different places, so each carries its own remedy.
fn rejected(lab: &str, devs: &[ResolvedDev], name: &str, source: Naming) -> String {
    let (what, remedy) = match source {
        Naming::Argument => (
            format!("\"{name}\" is not a dev machine in lab \"{lab}\""),
            "`@dev` is what makes a machine a dev machine (§19.1).",
        ),
        Naming::Environment => (
            format!("${ENV_VAR} names \"{name}\", which is not a dev machine in lab \"{lab}\""),
            "Unset it, or point it at one of those.",
        ),
    };
    format!("{what} — {}\n{remedy}", candidates(devs))
}

/// Nothing named one and nothing is the default — the rung that lists.
fn undecided(lab: &str, devs: &[ResolvedDev]) -> String {
    if devs.is_empty() {
        return format!(
            "lab \"{lab}\" declares no dev machine — put `@dev` on the machine you develop on \
             (§19.1)"
        );
    }
    format!(
        "lab \"{lab}\" has {} dev machines and none of them is the default — {}\nName one \
         (as an argument or in ${ENV_VAR}), or declare `@dev(default = true)` on one of them.",
        devs.len(),
        candidates(devs),
    )
}

/// The candidate list every error carries. Never a pick — the whole point is
/// that vmlab hands the choice back.
fn candidates(devs: &[ResolvedDev]) -> String {
    if devs.is_empty() {
        return "it declares no dev machine at all".to_string();
    }
    let names: Vec<String> = devs.iter().map(|d| format!("\"{}\"", d.name)).collect();
    format!("its dev machines are {}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, default: bool) -> ResolvedDev {
        ResolvedDev {
            name: name.to_string(),
            default,
            workspace: None,
            workspace_guest: "/src".to_string(),
        }
    }

    /// The ladder, rung by rung, on a lab where every rung could answer: the
    /// higher one always does.
    #[test]
    fn the_argument_beats_the_environment_beats_the_default() {
        let devs = [
            dev("dev01", true),
            dev("buildbox", false),
            dev("mac", false),
        ];

        let picked = resolve("lab", &devs, Some("mac"), Some("buildbox")).unwrap();
        assert_eq!(picked.name, "mac");

        let picked = resolve("lab", &devs, None, Some("buildbox")).unwrap();
        assert_eq!(picked.name, "buildbox");

        let picked = resolve("lab", &devs, None, None).unwrap();
        assert_eq!(picked.name, "dev01");
    }

    /// The lone `@dev` machine is already the default by the time resolution
    /// sees it (§19.1), so a lab with one dev machine needs nothing named.
    #[test]
    fn a_lone_dev_machine_answers_without_being_named() {
        let devs = [dev("dev01", true)];
        assert_eq!(resolve("lab", &devs, None, None).unwrap().name, "dev01");
    }

    /// A rung that names a machine this lab does not offer **fails at that
    /// rung**. Falling through to the next one is the guess the ladder exists
    /// to prevent: a variable left over from another lab would otherwise land
    /// the developer somewhere nothing ever named.
    #[test]
    fn a_rung_that_names_a_stranger_fails_there_and_never_falls_through() {
        let devs = [dev("dev01", true)];

        let err = resolve("lab", &devs, None, Some("ghost")).unwrap_err();
        let said = format!("{err}");
        assert!(said.contains("VMLAB_DEV_MACHINE names \"ghost\""), "{said}");
        assert!(said.contains("its dev machines are \"dev01\""), "{said}");
        assert!(said.contains("Unset it"), "the env rung's remedy: {said}");
    }

    /// A machine that exists but carries no `@dev` is not a dev machine, and
    /// the refusal says what would make it one.
    #[test]
    fn an_ordinary_machine_named_outright_is_refused() {
        let err = resolve("lab", &[dev("dev01", true)], Some("dc01"), None).unwrap_err();
        let said = format!("{err}");
        assert!(
            said.contains("\"dc01\" is not a dev machine in lab \"lab\""),
            "{said}"
        );
        assert!(said.contains("`@dev`"), "{said}");
    }

    /// Several dev machines and no default: the candidates are listed, and
    /// nothing is picked.
    #[test]
    fn several_with_no_default_errors_listing_the_candidates() {
        let devs = [dev("dev01", false), dev("buildbox", false)];
        let err = resolve("lab", &devs, None, None).unwrap_err();
        let said = format!("{err}");
        assert!(said.contains("none of them is the default"), "{said}");
        assert!(
            said.contains("\"dev01\", \"buildbox\""),
            "both candidates, in declaration order: {said}"
        );
        assert!(said.contains(ENV_VAR), "{said}");
        assert!(said.contains("@dev(default = true)"), "{said}");
    }

    /// A lab with no dev machine at all says so, rather than listing an empty
    /// set of candidates.
    #[test]
    fn a_lab_with_no_dev_machine_says_so() {
        let err = resolve("lab", &[], None, None).unwrap_err();
        let said = format!("{err}");
        assert!(said.contains("declares no dev machine"), "{said}");
        assert!(said.contains("`@dev`"), "{said}");
    }
}
