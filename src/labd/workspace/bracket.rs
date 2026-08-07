//! What snapshot capture and restore say to a workspace, and what they refuse
//! (PRD §19.6).
//!
//! **Snapshots are not a workspace backup.** That sentence is the reason this
//! module is words rather than only mechanism: the durability story for a dev
//! machine's source is the canonical host tree, and a developer who believes a
//! snapshot holds their work has believed something this design deliberately
//! does not offer. So [`NOT_A_BACKUP`] is said on every surface that captures
//! or restores one, and both refusals below repeat it where a developer is
//! most likely to be assuming otherwise.
//!
//! The two brackets are deliberately asymmetric, and the asymmetry is the
//! whole of §19.6's argument about them:
//!
//! - **Capture refuses with no escape.** If the guest has unsynced work, a
//!   snapshot of it is a snapshot of a tree the canonical copy has never seen,
//!   and restoring it later lands somewhere meaningless — mid-transfer, or
//!   holding a version of the repository that exists nowhere else. There is no
//!   flag because there is no case for one: the fix is to let the flush finish,
//!   or to resolve the halt, and both are the thing the developer wanted
//!   anyway.
//! - **Restore refuses only while a halt stands, and an explicit flag answers
//!   it.** Refusing outright would be obstruction: wanting to throw the guest
//!   copy away is frequently *why* someone restores. But a restore discards the
//!   guest side of every halted path by design, and each of those paths is a
//!   file a developer was going to be asked about — so it must be asked for by
//!   name rather than performed in passing.

use super::syncer::Report;

/// How many outstanding paths a refusal names before it stops.
///
/// A capture refused because 30 000 paths are in flight is refused for one
/// reason, and printing all of them onto a terminal buries it. What is dropped
/// is always said to have been dropped — a truncation nobody is told about is
/// the silent-incompleteness class §19.6 keeps refusing.
const NAMED: usize = 20;

/// A path list as a refusal says it: the first [`NAMED`] of them, and then how
/// many it left out.
///
/// One function rather than the same block in both refusals below, because the
/// two say it for the same reason and would otherwise drift into two different
/// ways of admitting a truncation.
fn named(paths: &[String]) -> String {
    let mut said = paths
        .iter()
        .take(NAMED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > NAMED {
        said.push_str(&format!(" and {} more", paths.len() - NAMED));
    }
    said
}

/// The sentence every snapshot surface carries.
///
/// One string rather than a paraphrase per surface, so the CLI, the console and
/// the daemon's own refusals cannot drift into saying three different things
/// about the one fact a developer most needs to have right.
pub const NOT_A_BACKUP: &str = "Snapshots are not a workspace backup: a dev machine's source lives on the host, which is \
     what survives `destroy` and what a restore re-converges the guest from.";

/// Why a capture will not go ahead — one machine's workspace, not in step.
///
/// Assembled from the pre-flight flush's own [`Report`] rather than from a
/// second inspection, so what the refusal says and what `vmlab dev sync status`
/// says are the same answer read twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outstanding {
    pub machine: String,
    /// The workspace is halted. Its paths are the ones a capture would freeze
    /// mid-disagreement.
    pub halt: Option<String>,
    /// The pre-flight pass could not finish — a dropped channel, a guest that
    /// has stopped answering. **Not knowing is a refusal**: a capture taken
    /// while the guest is unreachable is a capture of an unknown tree.
    pub trouble: Option<String>,
    /// Both directions are waiting on a stat-walk, so the host does not yet
    /// know what the guest holds.
    pub rescan: Option<String>,
    /// Both directions are waiting on the bracket's own re-seed: an earlier
    /// restore has not finished re-converging this tree, so it is mid-rewrite
    /// and a capture of it would hold neither version whole.
    pub reseed: Option<String>,
    /// Paths the flush did not carry, by name.
    pub unsynced: Vec<String>,
    /// The syncer has never completed a pass, so nothing about this workspace
    /// has been agreed at all.
    pub never_synced: bool,
}

impl Outstanding {
    /// What the pre-flight flush found, or `None` where the workspace is in
    /// step and the capture may go ahead.
    ///
    /// Everything here is a *transient* state that clears itself or resolves:
    /// a named skip is not among them, because a socket in the tree or a
    /// root-owned build artefact is permanent and normal, and refusing on one
    /// would make a whole class of repository unsnapshottable.
    pub fn of(machine: &str, report: &Report) -> Option<Outstanding> {
        let found = Outstanding {
            machine: machine.to_string(),
            halt: report.halt.as_ref().map(super::halt::Halt::headline),
            trouble: report.trouble.clone(),
            rescan: report.rescan.clone(),
            reseed: report.reseed.clone(),
            unsynced: report.unsynced.clone(),
            never_synced: report.passes == 0,
        };
        (found.halt.is_some()
            || found.trouble.is_some()
            || found.rescan.is_some()
            || found.reseed.is_some()
            || !found.unsynced.is_empty()
            || found.never_synced)
            .then_some(found)
    }

    /// The same question for a machine that is **down**, answered from what
    /// the last pass left on its ledger.
    ///
    /// A stopped machine cannot be flushed at all, so this refuses on the two
    /// states its ledger still records rather than on everything a live report
    /// would carry: a halt, which a developer can answer, and an **owed
    /// re-seed**, which means an earlier restore rewound this tree and nothing
    /// has carried it back yet. Refusing on more than that would make every
    /// down dev machine unsnapshottable, which is a bigger obstruction than
    /// the incoherence it would be guarding against.
    pub fn when_stopped(
        machine: &str,
        halted: &[String],
        reseed_owed: bool,
    ) -> Option<Outstanding> {
        let found = Outstanding {
            machine: machine.to_string(),
            halt: (!halted.is_empty()).then(|| {
                format!(
                    "It was halted on {} {} when it last synced, and it is not running now, so \
                     nothing has re-checked them since.",
                    halted.len(),
                    if halted.len() == 1 { "path" } else { "paths" },
                )
            }),
            reseed: reseed_owed.then(|| {
                "A snapshot restore rewound this workspace and it has not been carried back to \
                 the canonical copy yet — the re-seed runs when the machine next starts, so what \
                 is in the guest tree now is neither version whole."
                    .to_string()
            }),
            trouble: None,
            rescan: None,
            unsynced: halted.to_vec(),
            never_synced: false,
        };
        (found.halt.is_some() || found.reseed.is_some()).then_some(found)
    }
}

impl std::fmt::Display for Outstanding {
    /// Names the machine, what is outstanding, and the way out — and says
    /// there is no flag, because a developer who has just been refused will
    /// look for one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "\"{}\"'s workspace is not in step with the canonical copy, so this snapshot would \
             capture a tree the host has never agreed with",
            self.machine,
        )?;
        if let Some(halt) = &self.halt {
            write!(f, ". {halt}")?;
        }
        if let Some(trouble) = &self.trouble {
            write!(
                f,
                ". The pre-flight sync pass could not finish, so what the guest is holding is \
                 unknown rather than merely behind: {trouble}"
            )?;
        }
        for waiting in [&self.rescan, &self.reseed].into_iter().flatten() {
            write!(f, ". {waiting}")?;
        }
        if self.never_synced {
            write!(
                f,
                ". This workspace has not completed a single sync pass yet, so nothing in it has \
                 been agreed with the host at all"
            )?;
        }
        if !self.unsynced.is_empty() {
            write!(f, ". Still owed: {}", named(&self.unsynced))?;
        }
        write!(
            f,
            ". There is no flag for this — a snapshot of a tree mid-transfer restores to \
             somewhere meaningless. `vmlab dev sync status {}` says what is outstanding and \
             `vmlab dev sync flush {}` waits for it; a halt has to be resolved first. {}",
            self.machine, self.machine, NOT_A_BACKUP,
        )
    }
}

/// Why a restore will not go ahead without being asked twice.
///
/// The one refusal in §19.6 that **has** an escape, because the escape is
/// frequently the point: a restore discards the guest side by design, and
/// wanting to throw the guest copy away is a normal reason to want one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Halted {
    pub machine: String,
    /// The halt as it stands, in its own words.
    pub halt: String,
    /// Every path whose guest copy the restore would destroy.
    pub paths: Vec<String>,
}

impl std::fmt::Display for Halted {
    /// Names what would be lost, then the flag — in that order, because a
    /// refusal that leads with its remedy is a refusal nobody reads.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "\"{}\"'s workspace is halted, and restoring would silently destroy the guest copy of \
             every conflicting path. {}",
            self.machine, self.halt,
        )?;
        if !self.paths.is_empty() {
            write!(f, " Affected: {}.", named(&self.paths))?;
        }
        write!(
            f,
            " `vmlab dev sync diff` shows the guest copy host-side and `vmlab dev sync resolve` \
             picks a side. Pass `{}` to restore anyway, discarding the guest copy of the whole \
             workspace and re-converging it from the host. {}",
            DISCARD_FLAG, NOT_A_BACKUP,
        )
    }
}

/// The flag a restore needs while a halt stands, spelled once.
pub const DISCARD_FLAG: &str = "--discard-guest-changes";

/// The halt standing in this machine's way, or `None` where a restore may go
/// ahead unasked.
pub fn halted(machine: &str, report: &Report) -> Option<Halted> {
    report.halt.as_ref().map(|halt| Halted {
        machine: machine.to_string(),
        halt: halt.headline(),
        paths: halt.paths(),
    })
}

/// The same question for a machine that is **down**, answered from the paths
/// the last completed pass left on its ledger.
///
/// It exists because a restore does not need a running machine, and the halt
/// is the state a developer must not lose by having stopped one: `vmlab down`
/// takes the syncer with it, and without this the very next `snapshot restore`
/// would destroy the guest copy of every conflicting path unasked. Its words
/// are shorter than a live halt's on purpose — the batch is a list of paths
/// rather than a reconciliation, so it says what it knows and no more.
pub fn halted_when_stopped(machine: &str, paths: &[String]) -> Option<Halted> {
    (!paths.is_empty()).then(|| Halted {
        machine: machine.to_string(),
        halt: format!(
            "It was halted on {} {} when it last synced, and it is not running now, so nothing \
             has re-checked them since.",
            paths.len(),
            if paths.len() == 1 { "path" } else { "paths" },
        ),
        paths: paths.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::workspace::halt::Halt;
    use crate::labd::workspace::plan::{Conflict, ConflictKind};

    fn halt_of(paths: &[&str]) -> Halt {
        Halt {
            machine: "dev01".into(),
            conflicts: paths
                .iter()
                .map(|path| Conflict {
                    path: (*path).to_string(),
                    kind: ConflictKind::BothModified,
                })
                .collect(),
            bulk_delete: None,
            rules_changed: false,
        }
    }

    /// A workspace in step is not a reason to refuse anything.
    #[test]
    fn a_workspace_in_step_lets_a_capture_through() {
        let report = Report {
            passes: 3,
            ..Report::default()
        };
        assert_eq!(Outstanding::of("dev01", &report), None);
        assert_eq!(halted("dev01", &report), None);
    }

    /// **Capture refuses with no escape**, and says so — otherwise the next
    /// thing a developer does is search for the flag.
    #[test]
    fn a_capture_refusal_says_there_is_no_flag_for_it() {
        let report = Report {
            passes: 2,
            unsynced: vec!["src/main.rs".into()],
            ..Report::default()
        };
        let said = Outstanding::of("dev01", &report).unwrap().to_string();
        assert!(said.contains("no flag for this"), "{said}");
        assert!(said.contains("src/main.rs"), "{said}");
        assert!(said.contains("dev sync flush dev01"), "{said}");
    }

    /// Every surface says it, and the refusal says it where a developer is
    /// most likely to be assuming the opposite.
    #[test]
    fn both_refusals_say_a_snapshot_is_not_a_workspace_backup() {
        let report = Report {
            passes: 1,
            halt: Some(halt_of(&["a.rs"])),
            ..Report::default()
        };
        assert!(
            Outstanding::of("dev01", &report)
                .unwrap()
                .to_string()
                .contains(NOT_A_BACKUP)
        );
        assert!(
            halted("dev01", &report)
                .unwrap()
                .to_string()
                .contains(NOT_A_BACKUP)
        );
    }

    /// **Not knowing is a refusal.** A pass that could not reach the guest
    /// leaves the tree unknown rather than merely behind, and capturing an
    /// unknown tree is the failure this bracket exists to prevent.
    #[test]
    fn a_capture_refuses_when_the_pre_flight_pass_could_not_finish() {
        let report = Report {
            passes: 4,
            trouble: Some("the agent channel dropped".into()),
            ..Report::default()
        };
        let said = Outstanding::of("dev01", &report).unwrap().to_string();
        assert!(said.contains("unknown rather than merely behind"), "{said}");
        assert!(said.contains("the agent channel dropped"), "{said}");
    }

    /// A syncer that has never completed a pass has agreed nothing, which a
    /// zero-length `unsynced` list would otherwise report as "in step".
    #[test]
    fn a_workspace_that_has_never_synced_is_not_in_step() {
        let said = Outstanding::of("dev01", &Report::default())
            .unwrap()
            .to_string();
        assert!(said.contains("not completed a single sync pass"), "{said}");
    }

    /// The one refusal with an escape names what is lost first and the flag
    /// second — a refusal that leads with its remedy is a refusal nobody
    /// reads.
    #[test]
    fn a_restore_refusal_names_the_paths_and_then_the_flag() {
        let report = Report {
            passes: 9,
            halt: Some(halt_of(&["src/main.rs", ".env"])),
            ..Report::default()
        };
        let said = halted("dev01", &report).unwrap().to_string();
        let lost = said.find("src/main.rs").expect("names the paths");
        let flag = said.find(DISCARD_FLAG).expect("names the flag");
        assert!(lost < flag, "{said}");
        assert!(said.contains(".env"), "{said}");
        assert!(said.contains("dev sync diff"), "{said}");
        assert!(said.contains("dev sync resolve"), "{said}");
    }

    /// The 30 000-path case is real — un-ignoring a populated `node_modules`
    /// is one edit away — and a refusal that prints all of them buries the one
    /// reason it was refused. What it drops, it says it dropped.
    #[test]
    fn a_very_large_refusal_names_a_readable_number_and_says_what_it_left_out() {
        let paths: Vec<String> = (0..5_000)
            .map(|i| format!("node_modules/p{i}.js"))
            .collect();
        let report = Report {
            passes: 1,
            unsynced: paths.clone(),
            ..Report::default()
        };
        let said = Outstanding::of("dev01", &report).unwrap().to_string();
        assert!(said.len() < 2_048, "the refusal is {} bytes", said.len());
        assert!(said.contains("and 4980 more"), "{said}");
    }
}
