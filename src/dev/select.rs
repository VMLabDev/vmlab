//! **Which dev machine is mine** (PRD §19.7) — the host-side answer to a
//! question `vmlab.wcl` structurally cannot hold, because it is committed and
//! shared.
//!
//! The ladder is fixed and it **never guesses**:
//!
//! 1. an explicit argument,
//! 2. `VMLAB_DEV_MACHINE`,
//! 3. the `vmlab dev use` selection,
//! 4. `@dev(default = true)`, and with it the lone `@dev` machine, which is
//!    the default implicitly ([`super::machines`]),
//! 5. otherwise an **error listing the candidates**.
//!
//! Every rung that names a machine is checked rather than trusted: a rung
//! naming something that is not a dev machine in this lab is an error at that
//! rung, never a silent fall through to the next one. Falling through is the
//! guess this ladder exists to prevent — an environment variable left over
//! from another lab would otherwise land the developer on a machine nothing
//! ever said out loud.
//!
//! **The selection lives in the lab's own `.vmlab/`**, which §4 already says
//! should be gitignored. That makes it per-developer by construction, which is
//! exactly what a committed lab file cannot express, and it needs no key at
//! all since it lives inside the lab it describes. `destroy` removes that
//! directory, so it forgets the selection; re-setting it is one command.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::ResolvedDev;

/// The environment rung of the ladder — one lab's dev machine for one shell,
/// without recording anything.
pub const ENV_VAR: &str = "VMLAB_DEV_MACHINE";

/// The `vmlab dev use` selection, inside the lab's own working directory.
///
/// A file of its own rather than a key in `state.json`: that file is the lab
/// daemon's, written from a running lab, and this is a developer's answer that
/// has to be readable and writable with nothing running at all.
pub const SELECTION_FILE: &str = "dev-machine";

/// Which rung of the ladder answered. Reported rather than inferred, so a
/// developer surprised by which machine they landed on can see why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Argument,
    Environment,
    Selection,
    Default,
}

impl Source {
    /// How the answer is explained where it is printed.
    pub fn describe(self) -> String {
        match self {
            Source::Argument => "named on the command line".to_string(),
            Source::Environment => format!("from ${ENV_VAR}"),
            Source::Selection => "recorded by `vmlab dev use`".to_string(),
            Source::Default => "the lab's default dev machine".to_string(),
        }
    }
}

/// A dev machine, and the rung that named it.
///
/// It borrows the machine resolution already produced rather than carrying its
/// name: the caller wants the declaration — the workspace, the guest path —
/// and looking it up again by name would be re-deriving an answer this module
/// had in its hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected<'a> {
    pub dev: &'a ResolvedDev,
    pub source: Source,
}

impl Selected<'_> {
    /// The machine's name, which is what every message and every request uses.
    pub fn machine(&self) -> &str {
        &self.dev.name
    }
}

/// Where the selection is recorded for a lab whose working directory is
/// `lab_local` ([`crate::paths::lab_local_dir`]).
pub fn selection_path(lab_local: &Path) -> PathBuf {
    lab_local.join(SELECTION_FILE)
}

/// The recorded selection, or `None`.
///
/// Unreadable is the same as absent: this is one developer's convenience, and
/// a lab whose `.vmlab/` cannot be read has louder problems than which machine
/// was picked last week. An empty or blank file also reads as absent, so
/// truncating the file un-records the selection.
pub fn read_selection(lab_local: &Path) -> Option<String> {
    let text = std::fs::read_to_string(selection_path(lab_local)).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Record the selection, and answer with the file it went in.
///
/// Written whole through a fsynced temp file and a rename, like every other
/// file vmlab replaces: a half-written name is a machine that does not exist,
/// and the next `dev attach` would refuse rather than attach.
pub fn record_selection(lab_local: &Path, machine: &str) -> Result<PathBuf> {
    use std::io::Write as _;

    std::fs::create_dir_all(lab_local)
        .with_context(|| format!("creating {}", lab_local.display()))?;
    let path = selection_path(lab_local);
    let tmp = path.with_extension("tmp");
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(format!("{machine}\n").as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming onto {}", path.display()))?;
    Ok(path)
}

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
/// worth testing, and the environment and the filesystem are the caller's.
pub fn resolve<'a>(
    lab: &str,
    devs: &'a [ResolvedDev],
    argument: Option<&str>,
    env: Option<&str>,
    stored: Option<&str>,
) -> Result<Selected<'a>> {
    for (named, source) in [
        (argument, Naming::Argument),
        (env, Naming::Environment),
        (stored, Naming::Selection),
    ] {
        let Some(name) = named else { continue };
        match devs.iter().find(|d| d.name == name) {
            Some(dev) => {
                return Ok(Selected {
                    dev,
                    source: source.into(),
                });
            }
            None => bail!(rejected(lab, devs, name, source)),
        }
    }

    if let Some(dev) = devs.iter().find(|d| d.default) {
        return Ok(Selected {
            dev,
            source: Source::Default,
        });
    }
    bail!(undecided(lab, devs))
}

/// The three rungs that *name* a machine — the ones that can be wrong, and so
/// the only ones a rejection can come from. The default rung names nothing, so
/// it is deliberately not one of these rather than an arm nothing reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    Argument,
    Environment,
    Selection,
}

impl From<Naming> for Source {
    fn from(named: Naming) -> Self {
        match named {
            Naming::Argument => Source::Argument,
            Naming::Environment => Source::Environment,
            Naming::Selection => Source::Selection,
        }
    }
}

/// A rung named something this lab does not offer. Which rung matters as much
/// as which name: a stale `dev use` and a stale environment variable are
/// undone in different places, so each carries its own remedy.
fn rejected(lab: &str, devs: &[ResolvedDev], name: &str, source: Naming) -> String {
    let (what, remedy) = match source {
        Naming::Argument => (
            format!("\"{name}\" is not a dev machine in lab \"{lab}\""),
            "An ordinary machine is reached with `vmlab ssh` instead; `@dev` is what makes one a \
             dev machine (§19.1).",
        ),
        Naming::Environment => (
            format!("${ENV_VAR} names \"{name}\", which is not a dev machine in lab \"{lab}\""),
            "Unset it, or point it at one of those.",
        ),
        Naming::Selection => (
            format!(
                "`vmlab dev use` recorded \"{name}\", which is no longer a dev machine in lab \
                 \"{lab}\""
            ),
            "Record one of those with `vmlab dev use <machine>`.",
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
         (`vmlab dev attach <machine>`), record one (`vmlab dev use <machine>`), or declare \
         `@dev(default = true)` on one of them.",
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
    fn the_argument_beats_the_environment_beats_the_selection_beats_the_default() {
        let devs = [
            dev("dev01", true),
            dev("buildbox", false),
            dev("mac", false),
        ];

        let picked = resolve("lab", &devs, Some("mac"), Some("buildbox"), Some("dev01")).unwrap();
        assert_eq!(picked.machine(), "mac");
        assert_eq!(picked.source, Source::Argument);

        let picked = resolve("lab", &devs, None, Some("buildbox"), Some("mac")).unwrap();
        assert_eq!(picked.machine(), "buildbox");
        assert_eq!(picked.source, Source::Environment);

        let picked = resolve("lab", &devs, None, None, Some("mac")).unwrap();
        assert_eq!(picked.machine(), "mac");
        assert_eq!(picked.source, Source::Selection);

        let picked = resolve("lab", &devs, None, None, None).unwrap();
        assert_eq!(picked.machine(), "dev01");
        assert_eq!(picked.source, Source::Default);
    }

    /// The lone `@dev` machine is already the default by the time resolution
    /// sees it (§19.1), so the last two rungs are one code path — and a lab
    /// with one dev machine needs nothing recorded at all.
    #[test]
    fn a_lone_dev_machine_answers_without_anything_being_recorded() {
        let devs = [dev("dev01", true)];
        let picked = resolve("lab", &devs, None, None, None).unwrap();
        assert_eq!(picked.machine(), "dev01");
        assert_eq!(picked.source, Source::Default);
    }

    /// A rung that names a machine this lab does not offer **fails at that
    /// rung**. Falling through to the next one is the guess the ladder exists
    /// to prevent: a variable left over from another lab would otherwise land
    /// the developer somewhere nothing ever named.
    #[test]
    fn a_rung_that_names_a_stranger_fails_there_and_never_falls_through() {
        let devs = [dev("dev01", true)];

        let err = resolve("lab", &devs, None, Some("ghost"), None).unwrap_err();
        let said = format!("{err}");
        assert!(said.contains("VMLAB_DEV_MACHINE names \"ghost\""), "{said}");
        assert!(said.contains("its dev machines are \"dev01\""), "{said}");
        assert!(
            !said.contains("Record one"),
            "the env rung's remedy: {said}"
        );

        let err = resolve("lab", &devs, None, None, Some("ghost")).unwrap_err();
        let said = format!("{err}");
        assert!(
            said.contains("`vmlab dev use` recorded \"ghost\""),
            "{said}"
        );
        assert!(said.contains("`vmlab dev use <machine>`"), "{said}");
    }

    /// A machine that exists but carries no `@dev` is not a dev machine, and
    /// the refusal says where such a machine *is* reached from.
    #[test]
    fn an_ordinary_machine_named_outright_is_refused_and_pointed_at_ssh() {
        let err = resolve("lab", &[dev("dev01", true)], Some("dc01"), None, None).unwrap_err();
        let said = format!("{err}");
        assert!(
            said.contains("\"dc01\" is not a dev machine in lab \"lab\""),
            "{said}"
        );
        assert!(said.contains("`vmlab ssh`"), "{said}");
    }

    /// Several dev machines and no default: the candidates are listed, and
    /// nothing is picked.
    #[test]
    fn several_with_no_default_errors_listing_the_candidates() {
        let devs = [dev("dev01", false), dev("buildbox", false)];
        let err = resolve("lab", &devs, None, None, None).unwrap_err();
        let said = format!("{err}");
        assert!(said.contains("none of them is the default"), "{said}");
        assert!(
            said.contains("\"dev01\", \"buildbox\""),
            "both candidates, in declaration order: {said}"
        );
        assert!(said.contains("vmlab dev use <machine>"), "{said}");
        assert!(said.contains("@dev(default = true)"), "{said}");
    }

    /// A lab with no dev machine at all says so, rather than listing an empty
    /// set of candidates.
    #[test]
    fn a_lab_with_no_dev_machine_says_so() {
        let err = resolve("lab", &[], None, None, None).unwrap_err();
        let said = format!("{err}");
        assert!(said.contains("declares no dev machine"), "{said}");
        assert!(said.contains("`@dev`"), "{said}");
    }

    /// The selection round-trips through the lab's own working directory, and
    /// an absent one is simply absent — every lab starts that way.
    #[test]
    fn the_selection_round_trips_through_the_labs_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".vmlab");
        assert_eq!(read_selection(&local), None);

        let written = record_selection(&local, "dev01").unwrap();
        assert_eq!(written, local.join(SELECTION_FILE));
        assert_eq!(read_selection(&local).as_deref(), Some("dev01"));

        // Re-recording replaces rather than appends.
        record_selection(&local, "buildbox").unwrap();
        assert_eq!(read_selection(&local).as_deref(), Some("buildbox"));

        // It is one line a person can read, and re-read by hand.
        let raw = std::fs::read_to_string(&written).unwrap();
        assert_eq!(raw, "buildbox\n");
    }

    /// `destroy` removes the lab's working directory wholesale, so the
    /// selection is forgotten with it — there is no second place to clear.
    #[test]
    fn removing_the_labs_directory_forgets_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".vmlab");
        record_selection(&local, "dev01").unwrap();
        std::fs::remove_dir_all(&local).unwrap();
        assert_eq!(read_selection(&local), None);
    }

    /// A blank file is not a selection: truncating it un-records, and a
    /// trailing newline is not part of the name.
    #[test]
    fn a_blank_selection_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".vmlab");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(selection_path(&local), "  \n").unwrap();
        assert_eq!(read_selection(&local), None);
    }
}
