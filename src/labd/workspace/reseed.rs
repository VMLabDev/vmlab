//! The bracket's re-seed: what a snapshot restore does to a workspace
//! (PRD §19.6, ADR-0014).
//!
//! **A restore rewinds the guest by hundreds of files at once, which a naive
//! bidirectional syncer cannot tell apart from the developer having edited
//! them** — and it would carry them onto the canonical copy, overwriting real
//! work with old versions, silently. The saving grace is that **vmlab performs
//! the restore**, so it can bracket it. This is the argument that the syncer is
//! vmlab-integrated rather than a generic tool wrapped: an off-the-shelf syncer
//! cannot know a rewind happened.
//!
//! So re-convergence is a **host-only, digest-based reconcile**, and each word
//! of that is load-bearing:
//!
//! - **Host-only.** The guarantee is *directional*: nothing flows guest→host,
//!   so old guest state cannot come back. [`reseed`] emits no
//!   [`Direction::ToHost`] action at all — not "few", none — which is what
//!   makes the guarantee a property of the type rather than of a reviewer's
//!   attention.
//! - **The guest is inspected, never believed.** Its tree is walked for the
//!   sole purpose of deciding what to overwrite and delete: overwrite anything
//!   differing from host truth, delete anything host truth does not hold,
//!   transfer nothing else. It contributes no *digest* and no agreement to the
//!   ledger — only its own `(size, mtime)`, which is by definition the guest's
//!   own change-detector and cannot come from anywhere else.
//! - **By digest, never by mtime.** Stated rather than left implicit because
//!   the cheap version *looks* correct and silently keeps exactly the state
//!   this exists to destroy: a restored guest's clock runs behind the host, so
//!   a same-size in-place write compares identical on `(size, mtime)`. Both
//!   walks therefore run against an **empty** ledger, so nothing is vouched for
//!   by a stat pair and [`same`] refuses to call two sides equal without a
//!   digest in hand.
//!
//! A literal wipe-and-re-transfer would satisfy the guarantee trivially and
//! stays legal; it is not required, and it is not what this does — a restored
//! machine usually holds most of the tree already, and re-pushing a repository
//! to rediscover that would make restore an operation nobody uses.
//!
//! The re-seed also **replaces the stat-walk** rather than following one. That
//! is not an optimisation: the walk's whole job is to answer *what did the
//! guest do while we were not looking*, and here vmlab already knows — it did
//! exactly this. Two routes re-establish agreement, and each has its own; see
//! [`syncer`](super::syncer) for the barrier that keeps them apart.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};

use super::apply::{Failure, Target, apply};
use super::guest::GuestFs;
use super::ledger::{Agreed, Kind, Ledger};
use super::plan::{
    Action, Collision, Direction, Inputs, Oversize, Plan, State, Winner, collisions, place,
};
use super::scan::{Skip, guest_walk, host_scan};
use super::syncer::Workspace;

/// The plan that carries the guest back to host truth.
///
/// Every action in it is [`Direction::ToGuest`]. Paths whose two copies
/// already hold the same content are adopted — agreement recorded, nothing
/// transferred — and paths the host does not hold are removed from the guest.
/// There are no conflicts, because a re-seed does not ask the guest's opinion:
/// the halt this restore may have just discarded was the developer's answer to
/// that question, and they gave it by asking for the restore.
pub fn reseed(inputs: &Inputs<'_>) -> Plan {
    // The same refusal the steady state makes, computed the same way: two host
    // paths a case-folding guest cannot hold apart must not be raced onto one
    // guest path, whichever pass is doing the placing.
    let mut plan = Plan {
        collisions: collisions(inputs),
        ..Plan::default()
    };
    let colliding: BTreeSet<&str> = plan
        .collisions
        .iter()
        .flat_map(|c| c.paths.iter().map(String::as_str))
        .collect();

    let mut removals: Vec<Action> = Vec::new();
    let mut creations: Vec<Action> = Vec::new();

    let paths: BTreeSet<&String> = inputs.host.keys().chain(inputs.guest.keys()).collect();
    for path in paths {
        // A path neither side could be read for is left strictly alone, here
        // as everywhere: *"nothing is there"* and *"I could not look"* produce
        // opposite actions, and only one of them is recoverable.
        if inputs.undecided.contains(path) || colliding.contains(path.as_str()) {
            continue;
        }
        let host = inputs.host.get(path);
        let guest = inputs.guest.get(path);
        match (host, guest) {
            // Host truth does not hold it, so the guest must not either: this
            // is the guest-side work the snapshot captured and the restore is
            // throwing away, plus anything the host has deleted since.
            (None, Some(guest)) => removals.push(Action::Remove {
                direction: Direction::ToGuest,
                path: path.clone(),
                kind: guest.kind,
            }),
            (Some(host), guest) => {
                if guest.is_some_and(|guest| same(host, guest)) {
                    // Transfer nothing: the two copies already agree, and the
                    // only thing missing is the record saying so.
                    plan.adopt.push((
                        path.clone(),
                        Agreed {
                            kind: host.kind,
                            digest: host.digest.clone().unwrap_or_default(),
                            host: host.side(),
                            guest: guest.expect("matched a guest copy").side(),
                        },
                    ));
                    continue;
                }
                // Neither side can rename a file over a directory, so what is
                // in the way goes first.
                if let Some(guest) = guest
                    && guest.kind != host.kind
                {
                    removals.push(Action::Remove {
                        direction: Direction::ToGuest,
                        path: path.clone(),
                        kind: guest.kind,
                    });
                }
                // `None` for the agreement: there is nothing agreed to fall
                // back on, which is the point — the digest carried here is the
                // one the host scan just computed.
                match place(Direction::ToGuest, path, host, inputs, None) {
                    Ok(action) => creations.push(action),
                    Err(refusal) => plan.oversize.push(refusal),
                }
            }
            (None, None) => unreachable!("a path came from one of the two trees"),
        }
    }

    // Children before their parents on the way out, parents before their
    // children on the way in — the same ordering the steady state applies, and
    // for the same reason.
    removals.sort_by(|a, b| b.path().cmp(a.path()));
    creations.sort_by(|a, b| a.path().cmp(b.path()));
    plan.actions = removals;
    plan.actions.extend(creations);
    // **No volume warning.** A re-seed carrying a whole repository is the
    // expected shape of this operation, not a burst worth suggesting an ignore
    // rule for, and a warning that fires every time says nothing.
    plan
}

/// Whether the two copies hold the same thing, **by digest**.
///
/// The one rule this whole module exists to state: a missing digest is never
/// agreement. In the steady state an absent digest means *the pre-filter
/// vouched for this side*, which is a legitimate answer; here both walks run
/// against an empty ledger, so an absent digest can only mean the file was
/// never hashed — over the size cap, or a kind with no content — and reading
/// that as "unchanged" is exactly the mtime-shaped mistake that keeps the
/// rolled-back copy.
fn same(host: &State, guest: &State) -> bool {
    if host.kind != guest.kind || host.oversize || guest.oversize {
        return false;
    }
    match host.kind {
        // A directory has no content to differ about.
        Kind::Dir => true,
        Kind::File | Kind::Symlink => host.digest.is_some() && host.digest == guest.digest,
    }
}

/// What one re-convergence did, for the event feed and the syncer's report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconverged {
    /// Paths written into the guest.
    pub placed: usize,
    /// Paths removed from the guest — the rolled-back copy's own state, and
    /// anything the host has dropped since the capture.
    pub removed: usize,
    /// Paths whose two copies already matched, so nothing crossed the seam.
    pub adopted: usize,
    /// Files the size guard refused, by name, before transfer.
    pub oversize: Vec<Oversize>,
    /// Host paths a case-folding guest cannot hold apart.
    pub collisions: Vec<Collision>,
    /// Paths neither side could be read for.
    pub skipped: Vec<Skip>,
    /// Paths that did not land. Not agreed, so the next ordinary pass carries
    /// them the usual way.
    pub failures: Vec<Failure>,
}

impl Reconverged {
    /// The one line the event feed and `dev sync status` both carry.
    pub fn headline(&self, machine: &str) -> String {
        format!(
            "\"{machine}\"'s workspace re-converged from the canonical copy after a snapshot \
             restore: {} placed, {} removed, {} already matched. Nothing flowed guest→host, so \
             the rolled-back copy could not reach the host tree.",
            self.placed, self.removed, self.adopted,
        )
    }
}

/// Carry one machine's workspace back to host truth after a restore.
///
/// Both walks run against a **fresh, empty ledger**, which is what makes the
/// comparison a digest comparison: nothing is vouched for by a `(size, mtime)`
/// pair that a rewound guest can satisfy with stale content. The ledger the
/// caller holds is then **replaced** rather than edited, so no agreement
/// predating the restore can survive it — which is the same sentence as
/// "delete anything the ledger does not hold", read from the other end.
pub async fn reconverge(
    guest: &dyn GuestFs,
    workspace: &Workspace,
    case_sensitive_dirs: bool,
    case_folding: bool,
    ledger: &mut Ledger,
) -> Result<Reconverged> {
    let fresh = Ledger::new(&workspace.host_root, &workspace.guest_root);

    let root = workspace.host_root.clone();
    let cap = workspace.max_file_bytes;
    let scan_ledger = fresh.clone();
    let (scan, ignores) = tokio::task::spawn_blocking(move || host_scan(&root, &scan_ledger, cap))
        .await
        .map_err(|e| anyhow::anyhow!("the workspace scan panicked: {e}"))?
        .with_context(|| format!("walking {}", workspace.host_root.display()))?;

    // The stat-walk's machinery, asked a different question. Against an empty
    // ledger every file is a suspect, so the guest reports a digest for
    // everything it holds — which is the whole difference between a reconcile
    // that undoes a rewind and one that preserves it.
    let mut probe = guest_walk(guest, &workspace.guest_root, &ignores, &fresh, cap)
        .await
        .with_context(|| format!("walking the guest tree at {}", workspace.guest_root))?;
    // Filtering stays host-side and happens on receipt, exactly as in an
    // ordinary pass: the guest was handed no rules and asked to decide
    // nothing.
    probe.tree.retain(|path, state| {
        !ignores
            .verdict(path, state.kind == Kind::Dir)
            .is_guest_owned()
    });

    let skipped: Vec<Skip> = scan
        .skipped
        .iter()
        .chain(probe.skipped.iter())
        .cloned()
        .collect();
    let undecided: BTreeSet<String> = skipped.iter().map(|skip| skip.path.clone()).collect();

    let empty_paths = BTreeSet::new();
    let empty_resolutions = BTreeMap::<String, Winner>::new();
    let plan = reseed(&Inputs {
        host: &scan.tree,
        guest: &probe.tree,
        ledger: &fresh,
        undecided: &undecided,
        guest_owned: &empty_paths,
        resolved: &empty_resolutions,
        max_file_bytes: cap,
        case_folding,
    });
    debug_assert!(
        !plan
            .actions
            .iter()
            .any(|action| action.direction() == Direction::ToHost),
        "a re-seed is host-only: nothing may flow guest→host",
    );

    // Replaced, not edited: every agreement this workspace held was made
    // against a guest tree that no longer exists.
    *ledger = fresh;
    let applied = apply(
        guest,
        &Target {
            host_root: workspace.host_root.clone(),
            guest_root: workspace.guest_root.clone(),
            case_sensitive_dirs,
        },
        &plan,
        ledger,
    )
    .await;
    ledger.ignore_digest = ignores.digest();
    ledger.prune = ignores.prune_list(&scan.pruned);

    Ok(Reconverged {
        placed: applied.to_guest.placed,
        removed: applied.to_guest.removed,
        adopted: applied.adopted,
        oversize: plan.oversize,
        collisions: plan.collisions,
        skipped,
        failures: applied
            .failures
            .into_iter()
            .chain(applied.symlinks_refused)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::workspace::guest::fake::FakeGuest;
    use crate::labd::workspace::ledger::Side;
    use crate::labd::workspace::windows::Preconditions;
    use std::path::Path;
    use std::sync::Arc;

    const NO_CAP: u64 = u64::MAX;

    fn digest(body: &str) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(body.as_bytes()))
    }

    fn file(body: Option<&str>, mtime_ns: i64) -> State {
        State {
            kind: Kind::File,
            size: body.map_or(0, |b| b.len() as u64),
            mtime_ns,
            digest: body.map(digest),
            target: None,
            oversize: false,
        }
    }

    fn dir() -> State {
        State {
            kind: Kind::Dir,
            size: 0,
            mtime_ns: 0,
            digest: None,
            target: None,
            oversize: false,
        }
    }

    struct Sides {
        host: BTreeMap<String, State>,
        guest: BTreeMap<String, State>,
        undecided: BTreeSet<String>,
        ledger: Ledger,
        empty_paths: BTreeSet<String>,
        resolved: BTreeMap<String, Winner>,
    }

    impl Sides {
        fn new(host: &[(&str, State)], guest: &[(&str, State)]) -> Sides {
            Sides {
                host: host
                    .iter()
                    .map(|(p, s)| ((*p).to_string(), s.clone()))
                    .collect(),
                guest: guest
                    .iter()
                    .map(|(p, s)| ((*p).to_string(), s.clone()))
                    .collect(),
                undecided: BTreeSet::new(),
                ledger: Ledger::new(Path::new("/lab/src"), "/src"),
                empty_paths: BTreeSet::new(),
                resolved: BTreeMap::new(),
            }
        }

        fn plan(&self) -> Plan {
            reseed(&Inputs {
                host: &self.host,
                guest: &self.guest,
                ledger: &self.ledger,
                undecided: &self.undecided,
                guest_owned: &self.empty_paths,
                resolved: &self.resolved,
                max_file_bytes: NO_CAP,
                case_folding: false,
            })
        }
    }

    /// **The guarantee is directional.** Every action a re-seed emits carries
    /// into the guest, whatever the guest is holding — that is what stops a
    /// rolled-back copy reaching the canonical one.
    #[test]
    fn a_re_seed_never_emits_a_guest_to_host_action() {
        let sides = Sides::new(
            &[("kept.rs", file(Some("new"), 200)), ("dir", dir())],
            &[
                ("kept.rs", file(Some("old"), 100)),
                ("dir", dir()),
                ("only-guest.rs", file(Some("unsynced"), 100)),
                ("gone/", file(Some("x"), 100)),
            ],
        );
        let plan = sides.plan();
        assert!(!plan.actions.is_empty());
        for action in &plan.actions {
            assert_eq!(
                action.direction(),
                Direction::ToGuest,
                "{action:?} flows the wrong way"
            );
        }
        assert!(plan.conflicts.is_empty(), "a re-seed asks no questions");
        assert!(
            plan.bulk_delete.is_none(),
            "host→guest deletes are unguarded"
        );
    }

    /// The hazard, exactly: the guest's copy differs, its clock runs behind,
    /// and host truth wins without anything being asked.
    #[test]
    fn a_rolled_back_file_is_overwritten_from_host_truth() {
        let sides = Sides::new(
            &[("src/main.rs", file(Some("current"), 2_000))],
            &[("src/main.rs", file(Some("rolled back"), 1_000))],
        );
        let plan = sides.plan();
        let placed: Vec<&str> = plan.actions.iter().map(Action::path).collect();
        assert_eq!(placed, vec!["src/main.rs"]);
        assert!(plan.adopt.is_empty());
    }

    /// **It must compare by digest, never by mtime.** A restored guest's clock
    /// runs behind the host, so a same-size in-place write compares identical
    /// on `(size, mtime)` — the cheap version looks correct and silently keeps
    /// exactly the state the reconcile exists to destroy.
    #[test]
    fn a_same_size_rewind_with_an_identical_stat_pair_is_still_overwritten() {
        let host = file(Some("aaaa"), 1_000);
        let guest = State {
            // Byte-for-byte the same stat pair, different content.
            digest: Some(digest("bbbb")),
            ..host.clone()
        };
        assert_eq!(host.size, guest.size);
        assert_eq!(host.mtime_ns, guest.mtime_ns);
        let sides = Sides::new(&[("a.txt", host)], &[("a.txt", guest)]);
        let plan = sides.plan();
        assert_eq!(
            plan.actions.iter().map(Action::path).collect::<Vec<_>>(),
            vec!["a.txt"],
        );
    }

    /// **Transfer nothing else.** A restored machine usually holds most of the
    /// tree already; re-pushing a repository to rediscover that would make
    /// restore an operation nobody uses.
    #[test]
    fn matching_content_is_adopted_rather_than_re_transferred() {
        let sides = Sides::new(
            &[("a.txt", file(Some("same"), 2_000)), ("d", dir())],
            &[("a.txt", file(Some("same"), 1_000)), ("d", dir())],
        );
        let plan = sides.plan();
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        let adopted: Vec<&str> = plan.adopt.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(adopted, vec!["a.txt", "d"]);
        // Host truth's digest, and **each side's own** stat pair: the guest
        // contributes its change-detector and nothing else.
        let agreed = &plan.adopt[0].1;
        assert_eq!(agreed.digest, digest("same"));
        assert_eq!(agreed.host, Side::new(4, 2_000));
        assert_eq!(agreed.guest, Side::new(4, 1_000));
    }

    /// Guest-side work the snapshot captured and the restore is throwing away.
    /// It is unrecoverable by design, which is why the verb needs a flag.
    #[test]
    fn a_path_host_truth_does_not_hold_is_deleted_from_the_guest() {
        let sides = Sides::new(&[], &[("scratch.rs", file(Some("unsynced"), 100))]);
        assert_eq!(
            sides.plan().actions,
            vec![Action::Remove {
                direction: Direction::ToGuest,
                path: "scratch.rs".into(),
                kind: Kind::File,
            }],
        );
    }

    /// A file the guard would refuse is never *equal* either: it was never
    /// hashed, so nothing can be concluded about it, and the refusal names it.
    #[test]
    fn an_unhashable_file_is_refused_rather_than_assumed_to_match() {
        let big = State {
            size: 4096,
            oversize: true,
            digest: None,
            ..file(None, 1_000)
        };
        let sides = Sides::new(&[("big.vhdx", big.clone())], &[("big.vhdx", big)]);
        let plan = reseed(&Inputs {
            host: &sides.host,
            guest: &sides.guest,
            ledger: &sides.ledger,
            undecided: &sides.undecided,
            guest_owned: &sides.empty_paths,
            resolved: &sides.resolved,
            max_file_bytes: 16,
            case_folding: false,
        });
        assert!(plan.adopt.is_empty(), "never hashed, so never agreed");
        assert!(plan.actions.is_empty(), "refused before transfer");
        assert_eq!(plan.oversize.len(), 1);
        assert_eq!(plan.oversize[0].path, "big.vhdx");
    }

    /// Kind replacement clears what is in the way first: neither side can
    /// rename a file over a directory.
    #[test]
    fn a_guest_directory_where_the_host_holds_a_file_is_cleared_first() {
        let sides = Sides::new(
            &[("thing", file(Some("a file now"), 2_000))],
            &[("thing", dir())],
        );
        let plan = sides.plan();
        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(
            plan.actions[0],
            Action::Remove {
                kind: Kind::Dir,
                ..
            }
        ));
        assert!(matches!(plan.actions[1], Action::PutFile { .. }));
    }

    /// A path neither side could be read for is left strictly alone — never
    /// mistaken for absence, which would delete whatever is really there.
    #[test]
    fn an_unreadable_path_is_left_where_it_is() {
        let mut sides = Sides::new(&[], &[("root-only/artefact", file(Some("x"), 1))]);
        sides.undecided.insert("root-only/artefact".into());
        assert!(sides.plan().actions.is_empty());
    }

    /// One workspace, end to end, over the file session the syncer uses: the
    /// rewound guest is carried back to host truth and the ledger that comes
    /// out describes host truth and nothing else.
    #[tokio::test]
    async fn reconverge_carries_a_rewound_guest_back_to_host_truth() {
        let host = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host.path().join("src")).unwrap();
        std::fs::write(host.path().join("src/main.rs"), "current").unwrap();
        std::fs::write(host.path().join("README.md"), "same on both").unwrap();

        let guest = Arc::new(FakeGuest::new());
        guest.dir("/src");
        guest.dir("/src/src");
        // Rolled back…
        guest.file("/src/src/main.rs", "rolled back", 10);
        // …already agreeing…
        guest.file("/src/README.md", "same on both", 10);
        // …and captured mid-experiment, never synced to the host.
        guest.file("/src/scratch.rs", "guest-only", 10);

        let workspace = Workspace {
            machine: "dev01".into(),
            host_root: host.path().to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: host.path().join("ignored.json"),
            max_file_bytes: NO_CAP,
            preconditions: Preconditions::default(),
        };
        // A ledger from before the capture, holding an agreement the restore
        // has just invalidated.
        let mut ledger = Ledger::new(&workspace.host_root, "/src");
        ledger.entries.insert(
            "vanished.rs".into(),
            Agreed {
                kind: Kind::File,
                digest: digest("gone"),
                host: Side::new(4, 1),
                guest: Side::new(4, 1),
            },
        );

        let done = reconverge(guest.as_ref(), &workspace, false, false, &mut ledger)
            .await
            .unwrap();

        assert_eq!(guest.text("/src/src/main.rs").as_deref(), Some("current"));
        assert_eq!(
            guest.get("/src/scratch.rs"),
            None,
            "discarded by the bracket"
        );
        assert!(done.placed >= 1);
        assert_eq!(done.removed, 1);
        assert!(done.failures.is_empty(), "{:?}", done.failures);

        // The ledger describes host truth: nothing from before the restore
        // survives it, and every entry names a path the host holds.
        assert!(!ledger.entries.contains_key("vanished.rs"));
        assert!(!ledger.entries.contains_key("scratch.rs"));
        assert_eq!(
            ledger.entries["src/main.rs"].digest,
            digest("current"),
            "the digest is host truth's",
        );
        assert!(ledger.entries.contains_key("README.md"));
        assert!(done.headline("dev01").contains("guest→host"));
    }

    /// The host copy is untouched, every time. The whole hazard is a restore
    /// reaching the canonical tree; a test that only checks the guest would
    /// not notice it doing so.
    #[tokio::test]
    async fn reconverge_writes_nothing_to_the_canonical_copy() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("a.txt"), "host truth").unwrap();

        let guest = Arc::new(FakeGuest::new());
        guest.dir("/src");
        guest.file("/src/a.txt", "rolled back", 10);
        guest.file("/src/b.txt", "guest invented this", 10);

        let workspace = Workspace {
            machine: "dev01".into(),
            host_root: host.path().to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: host.path().join("ignored.json"),
            max_file_bytes: NO_CAP,
            preconditions: Preconditions::default(),
        };
        let mut ledger = Ledger::new(&workspace.host_root, "/src");
        reconverge(guest.as_ref(), &workspace, false, false, &mut ledger)
            .await
            .unwrap();

        let mut left: Vec<String> = std::fs::read_dir(host.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["a.txt".to_string()], "the host tree moved");
        assert_eq!(
            std::fs::read_to_string(host.path().join("a.txt")).unwrap(),
            "host truth",
        );
    }

    /// A guest tree that is simply not there — a restore to a snapshot taken
    /// before the workspace existed — is a seed, not a failure.
    #[tokio::test]
    async fn reconverge_seeds_a_guest_that_holds_no_workspace_at_all() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("a.txt"), "host truth").unwrap();
        let guest = Arc::new(FakeGuest::new());

        let workspace = Workspace {
            machine: "dev01".into(),
            host_root: host.path().to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: host.path().join("ignored.json"),
            max_file_bytes: NO_CAP,
            preconditions: Preconditions::default(),
        };
        let mut ledger = Ledger::new(&workspace.host_root, "/src");
        let done = reconverge(guest.as_ref(), &workspace, false, false, &mut ledger)
            .await
            .unwrap();
        assert_eq!(done.placed, 1);
        assert_eq!(guest.text("/src/a.txt").as_deref(), Some("host truth"));
    }

    /// An ignored path is **guest-owned**, and a re-seed is no more entitled
    /// to it than an ordinary pass: the guest's own `node_modules` survives
    /// the restore, because it is reconstructible and nobody agreed about it.
    #[tokio::test]
    async fn reconverge_leaves_guest_owned_paths_alone() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join(".gitignore"), "node_modules/\n").unwrap();

        let guest = Arc::new(FakeGuest::new());
        guest.dir("/src");
        guest.dir("/src/node_modules");
        guest.file("/src/node_modules/pkg.js", "guest-native", 10);

        let workspace = Workspace {
            machine: "dev01".into(),
            host_root: host.path().to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: host.path().join("ignored.json"),
            max_file_bytes: NO_CAP,
            preconditions: Preconditions::default(),
        };
        let mut ledger = Ledger::new(&workspace.host_root, "/src");
        reconverge(guest.as_ref(), &workspace, false, false, &mut ledger)
            .await
            .unwrap();
        assert_eq!(
            guest.text("/src/node_modules/pkg.js").as_deref(),
            Some("guest-native"),
        );
        assert!(!ledger.entries.contains_key("node_modules"));
    }
}
