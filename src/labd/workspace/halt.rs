//! The conflict halt, as a value and as the two things it says (PRD §19.6).
//!
//! **A conflict is an anomaly.** The developer authors guest-side; *canonical*
//! is doing durability work rather than authorship work, and the host-side
//! writer set is small and enumerable — git operations, occasional host-side
//! tooling, and vmlab's own restore re-seed. That is what licenses an
//! expensive, loud, safe policy here instead of a winner rule that would have
//! to be right thousands of times a day.
//!
//! So the policy is **halt and surface**: the whole workspace, both directions,
//! on one machine, reporting every conflicting path in the batch. Four things
//! fall out of it, and each is somewhere else in this module's neighbours
//! rather than here:
//!
//! - **Scan then halt.** A host-side `git pull` collides in *batches*, so the
//!   halt is computed from a whole reconciliation ([`plan`](super::plan)) and
//!   names every path in it; halting on the first would turn one `pull` into
//!   thirty resolve-and-resume round trips.
//! - **Finish the file in flight, then stop.** Structural rather than
//!   defended: a pass scans, reconciles and only then applies, so the halt is
//!   decided before any transfer of that pass has begun, and the applies of the
//!   pass *before* it have long since completed. A torn half-written file is
//!   worse than one extra completed transfer.
//! - **The watch keeps running and nothing escalates.** Ten conflicts do not
//!   become a bigger hammer, and the host keeps draining the guest's dirty set
//!   into its own pending set while halted, so a long halt costs no rescan.
//! - **No conflict copies on disk.** The two copies already exist, one per
//!   side, and a halt writes neither and deletes neither. Inventing
//!   `foo.cs.conflict-host` would add a file the build sees, `git status`
//!   reports, and someone eventually commits.
//!
//! ### Why resolution is host-side, and why the guest gets a file
//!
//! **Resolution is host-side necessarily**, and it is worth saying why so
//! nobody later "fixes" it: ADR-0013's invariant is that the host opens
//! channels and the guest answers, so there is **no guest→host control path at
//! all** — a `vmlab` shim inside the guest could not call back even if one were
//! shipped. The seam-crossing worry is softer than it looks, because the host
//! copy is a plain directory on the developer's own workstation; only the
//! *guest* copy is behind the seam, which is what `dev sync diff` earns its
//! place by pulling.
//!
//! The other half of that invariant is why [`marker`] exists. From inside the
//! guest a halt is otherwise *nothing happening* — the file simply stops
//! updating — which is the silent-divergence failure §19.6 keeps ruling out, on
//! the one side no control path can reach. So the halt writes a file at the
//! workspace root, in the built-in ignore floor so it never syncs, and its
//! `git status` noise **is the feature**: it is the developer noticing.

use super::plan::{BulkDelete, Conflict, Plan};

/// The marker file's name, at the workspace root inside the guest.
///
/// It is covered by the ignore floor's `.vmlab-sync*` glob from the start
/// rather than from the moment it is first written — a signal file that syncs
/// itself into the other tree is worse than no signal at all.
pub const MARKER: &str = ".vmlab-sync-halt";

/// How many halted paths the marker lists.
///
/// Capped for the case §19.6 names as rare and self-inflicted: un-ignoring a
/// populated `node_modules` halts on tens of thousands of paths, and a marker
/// listing all of them would be a multi-megabyte untracked file dropped into
/// the developer's editor — the opposite of the small, noticeable thing this is
/// meant to be. What is dropped is always said to have been dropped.
const LISTED: usize = 200;

/// One machine's workspace, stopped.
///
/// It **names the machine** because two dev machines may share one host
/// workspace: the host is a hub rather than a peer, each machine has its own
/// ledger against it, and one machine halting must not stop another. A halt
/// that said only "the workspace has conflicts" would be unreadable in exactly
/// that lab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Halt {
    pub machine: String,
    /// Every path both sides moved, whole — the batch, not its first member.
    pub conflicts: Vec<Conflict>,
    /// Guest→host deletions the mass guard withheld, if it fired.
    pub bulk_delete: Option<BulkDelete>,
    /// The ignore rules changed since the ledger last recorded their digest.
    ///
    /// **Entering scope is a conflict**: no agreement point exists for a path
    /// that was guest-owned until a moment ago, and both sides may hold
    /// content. The rules' digest is in the ledger precisely so the halt can
    /// say *these conflict because you just changed the rules* — the files
    /// most likely to be un-ignored are `.env`, local certs and
    /// `appsettings.Development.json`, where the two sides differing is the
    /// **normal** situation.
    pub rules_changed: bool,
}

impl Halt {
    /// The halt this reconciliation is, or `None` where the workspace still
    /// agrees with itself.
    pub fn of(machine: &str, plan: &Plan, rules_changed: bool) -> Option<Halt> {
        plan.halts().then(|| Halt {
            machine: machine.to_string(),
            conflicts: plan.conflicts.clone(),
            bulk_delete: plan.bulk_delete.clone(),
            rules_changed,
        })
    }

    /// Every path this halt is about, conflicts first — what `--all` resolves,
    /// what the marker lists, and what the projection carries.
    pub fn paths(&self) -> Vec<String> {
        let conflicts = self.conflicts.iter().map(|c| c.path.clone());
        let deleted = self
            .bulk_delete
            .iter()
            .flat_map(|bulk| bulk.paths.iter().cloned());
        conflicts.chain(deleted).collect()
    }

    /// Each halted path with the sentence that explains it, in the same order
    /// as [`paths`](Halt::paths).
    pub fn reasons(&self) -> Vec<(String, String)> {
        let conflicts = self
            .conflicts
            .iter()
            .map(|c| (c.path.clone(), c.kind.to_string()));
        let deleted = self.bulk_delete.iter().flat_map(|bulk| {
            bulk.paths.iter().map(|path| {
                (
                    path.clone(),
                    "the guest deleted it, in a batch large enough to be a rewrite of the \
                     canonical copy rather than an edit"
                        .to_string(),
                )
            })
        });
        conflicts.chain(deleted).collect()
    }

    /// The one line that says what happened and to which machine.
    pub fn headline(&self) -> String {
        let mut said = match (self.conflicts.len(), &self.bulk_delete) {
            (0, Some(bulk)) => {
                format!("the workspace on \"{}\" has stopped: {bulk}", self.machine)
            }
            (n, None) => format!(
                "the workspace on \"{}\" has stopped, both directions, on {n} conflicting \
                 {}",
                self.machine,
                if n == 1 { "path" } else { "paths" },
            ),
            (n, Some(bulk)) => format!(
                "the workspace on \"{}\" has stopped, both directions, on {n} conflicting {} — \
                 and {bulk}",
                self.machine,
                if n == 1 { "path" } else { "paths" },
            ),
        };
        if self.rules_changed {
            said.push_str(
                ". The ignore rules changed since the last agreement, so these are most likely \
                 paths that just entered scope — a path with no agreement point and content on \
                 both sides is a conflict by construction",
            );
        }
        said
    }

    /// The routes out, in the order a developer would try them. Said wherever
    /// the halt is said, because a stopped workspace with no next step is the
    /// obstruction §19.6 is careful not to be.
    pub fn routes(&self) -> String {
        "`vmlab dev sync diff <path>` shows the guest copy host-side; `vmlab dev sync resolve \
         <path> --host` or `--guest` picks a side, and `--all` takes the batch. Making both sides \
         identical by hand needs no verb at all — the next pass adopts them as agreed. Both copies \
         are still where they were: a halt writes neither and deletes neither."
            .to_string()
    }
}

/// What the guest is handed at its workspace root while the halt stands.
///
/// Plain text, because the audience is a developer who has just noticed an
/// untracked file appear in an editor they are attached with — not a parser.
/// It lists the halted paths for the same reason `dev sync status` does: from
/// inside the guest, the alternative to this file is finding out that nothing
/// has been syncing for an hour.
pub fn marker(halt: &Halt) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "vmlab: this workspace has stopped syncing.");
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", halt.headline());
    let _ = writeln!(out);
    let reasons = halt.reasons();
    for (path, why) in reasons.iter().take(LISTED) {
        let _ = writeln!(out, "  {path}  —  {why}");
    }
    if reasons.len() > LISTED {
        let _ = writeln!(
            out,
            "  … and {} more, not listed here — `vmlab dev sync status` on the host has them all, \
             and resolving the batch needs no list at all",
            reasons.len() - LISTED,
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Resolve it from the host, in the lab directory. There is no guest-side route: vmlab \
         opens every channel from the host and the guest only ever answers (ADR-0013), so a \
         `vmlab` inside this machine could not call back even if one were installed."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", halt.routes());
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "This file is vmlab's; it never syncs, and it goes when the halt does."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::workspace::plan::ConflictKind;

    fn conflict(path: &str, kind: ConflictKind) -> Conflict {
        Conflict {
            path: path.to_string(),
            kind,
        }
    }

    fn halted() -> Halt {
        Halt {
            machine: "dev01".into(),
            conflicts: vec![
                conflict("src/main.rs", ConflictKind::BothModified),
                conflict(".env", ConflictKind::BothCreated),
            ],
            bulk_delete: None,
            rules_changed: false,
        }
    }

    /// The whole batch, and the machine — two dev machines may share one host
    /// workspace, so a halt that did not say which one had stopped would be
    /// unreadable in exactly the lab that has two.
    #[test]
    fn a_halt_names_the_machine_and_every_path_in_the_batch() {
        let halt = halted();
        assert!(halt.headline().contains("\"dev01\""), "{}", halt.headline());
        assert_eq!(halt.paths(), vec!["src/main.rs".to_string(), ".env".into()]);
        let said = marker(&halt);
        assert!(said.contains("src/main.rs"), "{said}");
        assert!(said.contains(".env"), "{said}");
    }

    /// **The reason is attached, so nobody later "fixes" it**: resolution is
    /// host-side because ADR-0013 leaves no guest→host control path at all.
    #[test]
    fn the_marker_says_why_there_is_no_guest_side_route() {
        let said = marker(&halted());
        assert!(said.contains("ADR-0013"), "{said}");
        assert!(said.contains("could not call back"), "{said}");
    }

    /// Every route out, including the one that needs no verb — a stopped
    /// workspace with no next step is obstruction.
    #[test]
    fn the_marker_offers_all_three_resolution_routes() {
        let said = marker(&halted());
        assert!(said.contains("dev sync resolve"), "{said}");
        assert!(said.contains("--host"), "{said}");
        assert!(said.contains("--guest"), "{said}");
        assert!(said.contains("--all"), "{said}");
        assert!(said.contains("identical by hand"), "{said}");
        assert!(said.contains("dev sync diff"), "{said}");
    }

    /// **No conflict copies**: the halt's own words say both copies are where
    /// they were, because the alternative design is the one a reader expects.
    #[test]
    fn the_marker_says_both_copies_survive() {
        assert!(
            marker(&halted()).contains("writes neither and deletes neither"),
            "{}",
            marker(&halted())
        );
    }

    /// The ignore rules' digest is in the ledger for exactly this sentence:
    /// un-ignoring a populated directory halts, and the halt says the rules
    /// just changed rather than leaving a developer to work it out.
    #[test]
    fn a_halt_after_a_rules_change_says_the_rules_changed() {
        let halt = Halt {
            rules_changed: true,
            ..halted()
        };
        assert!(
            halt.headline().contains("ignore rules changed"),
            "{}",
            halt.headline()
        );
        assert!(
            halt.headline().contains("entered scope"),
            "{}",
            halt.headline()
        );
    }

    /// A withheld mass deletion is the same halt with the same surface: one
    /// stopped workspace, one resolution route, no second vocabulary.
    #[test]
    fn a_withheld_mass_deletion_halts_like_a_conflict() {
        let plan = Plan {
            bulk_delete: Some(BulkDelete {
                paths: vec!["a.rs".into(), "b.rs".into()],
                agreed: 2,
            }),
            ..Plan::default()
        };
        let halt = Halt::of("dev01", &plan, false).expect("a withheld deletion is a halt");
        assert_eq!(halt.paths(), vec!["a.rs".to_string(), "b.rs".into()]);
        assert!(halt.headline().contains("\"dev01\""));
        assert!(marker(&halt).contains("a.rs"));
    }

    /// The 30 000-file case §19.6 calls rare and self-inflicted: the marker
    /// stays a file a developer notices rather than becoming a megabyte of
    /// untracked noise, and it says what it left out.
    #[test]
    fn a_very_large_halt_caps_the_marker_and_says_it_did() {
        let halt = Halt {
            machine: "dev01".into(),
            conflicts: (0..30_000)
                .map(|i| {
                    conflict(
                        &format!("node_modules/p{i}/index.js"),
                        ConflictKind::BothCreated,
                    )
                })
                .collect(),
            bulk_delete: None,
            rules_changed: true,
        };
        let said = marker(&halt);
        assert!(said.len() < 32_768, "the marker is {} bytes", said.len());
        assert!(said.contains("and 29800 more"), "{said}");
        assert!(said.contains("node_modules/p0/index.js"), "{said}");
    }

    /// A reconciliation that agrees with itself is not a halt, and there is
    /// nothing to write.
    #[test]
    fn an_agreeing_pass_is_no_halt_at_all() {
        assert_eq!(Halt::of("dev01", &Plan::default(), true), None);
    }
}
