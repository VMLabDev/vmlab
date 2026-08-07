//! Executing a reconciliation against the guest (PRD §19.6).
//!
//! Two invariants this module exists to hold, in this order:
//!
//! 1. **Every apply is temp-name-then-rename, with the temp in the same
//!    directory as its target** — so the rename is atomic rather than a
//!    cross-volume copy, and on Windows it inherits the case-sensitivity flag
//!    set at `mkdir`. The temp name is in the ignore floor, so it never
//!    becomes a sync object itself.
//! 2. **The ledger records agreement only after the rename**, never after the
//!    last write. Otherwise a crash between the two leaves the ledger claiming
//!    agreement on a file that was never placed, the next pass concludes
//!    "unchanged", and the divergence is permanent and silent.
//!
//! The asymmetry between the two failure directions is the whole design. A
//! ledger that has *not yet* recorded an agreement costs a digest comparison
//! on the next pass, which then adopts the two matching sides for free. A
//! ledger that recorded one too early costs the developer their work. So the
//! entry is written after the rename lands and never before, and a pass that
//! dies part-way is re-derived rather than trusted.
//!
//! **Resume is re-transfer, not offset-resume**: a dropped channel leaves a
//! temp file nobody will finish, the target untouched, and the ledger silent —
//! and the next pass starts the whole file again, because the source may have
//! changed while the channel was down and there is no cheap way to know which
//! prefix is still valid.
//!
//! **Host→guest deletes are unguarded** (the guard on mass deletion is
//! guest→host's, where the copy being replicated over is the canonical one):
//! a `git checkout` removing 400 files just removes them.

use std::path::PathBuf;

use super::guest::GuestFs;
use super::ignore::TEMP_PREFIX;
use super::ledger::{Agreed, Kind, Ledger, Side};
use super::plan::{Action, Plan};
use super::scan::join_guest;
use crate::labd::vm_agent::{ErrorCode, FileOpsError, LinkKind};

/// Where a workspace's two copies live, and how the guest wants them made.
pub struct Target {
    /// The canonical host directory.
    pub host_root: PathBuf,
    /// Where the working copy lands in the guest.
    pub guest_root: String,
    /// Create directories case-sensitive. NTFS accepts the flag only while a
    /// directory is empty, which the syncer's always is — and the Windows
    /// preconditions that make it meaningful (the collision refusal, the
    /// symlink warning, the guest's line-ending setting) are their own ticket.
    pub case_sensitive_dirs: bool,
}

/// One thing that did not land, named. Never a halt on its own: the rest of
/// the pass still runs, and the path is simply not agreed, so the next pass
/// tries it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    pub why: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.why)
    }
}

/// What one pass actually did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub placed: usize,
    pub removed: usize,
    /// Paths adopted as agreed without a transfer, because both sides already
    /// held the same content.
    pub adopted: usize,
    /// Directories the host dropped that the guest still holds its own
    /// content in — left standing, named, and out of the ledger.
    pub left_standing: Vec<String>,
    pub failures: Vec<Failure>,
}

/// Carry out `plan` against the guest, updating `ledger` as each apply lands.
///
/// The ledger is mutated in place and left to the caller to persist; every
/// entry in it by the time this returns is one whose rename completed. A pass
/// that dies before the caller saves loses agreements rather than inventing
/// them, and the next pass re-derives them by digest — which costs a hash and
/// no transfer.
pub async fn apply(
    guest: &dyn GuestFs,
    target: &Target,
    plan: &Plan,
    ledger: &mut Ledger,
) -> Applied {
    let mut done = Applied::default();

    // Adoptions are not applies: both sides already hold the same content, so
    // the agreement is all there is to record.
    for (path, agreed) in &plan.adopt {
        ledger.entries.insert(path.clone(), agreed.clone());
        done.adopted += 1;
    }
    for path in &plan.forget {
        ledger.entries.remove(path);
    }

    for action in &plan.actions {
        let path = action.path();
        let guest_path = join_guest(&target.guest_root, path);
        let outcome = match action {
            Action::Remove { kind, .. } => {
                let removed = match kind {
                    Kind::Dir => guest.rmdir(&guest_path).await,
                    _ => guest.remove(&guest_path).await,
                };
                match removed {
                    Ok(()) => {
                        ledger.entries.remove(path);
                        done.removed += 1;
                        continue;
                    }
                    // The guest still holds guest-owned content in there —
                    // `node_modules` under a directory the host just dropped.
                    // Neither direction touches guest-owned paths, so the
                    // directory stays and the *agreement* about it goes: a
                    // removal that can never succeed must not be retried on
                    // every pass for the life of the machine.
                    Err(e) if is_not_empty(&e) => {
                        ledger.entries.remove(path);
                        done.left_standing.push(path.to_string());
                        continue;
                    }
                    Err(e) => Err(e),
                }
            }
            Action::MakeDir { host, .. } => {
                // A directory needs no temp: creating one destroys nothing,
                // and on NTFS the case-sensitivity flag can only be set on
                // the directory that will actually hold the tree.
                guest
                    .mkdir(&guest_path, target.case_sensitive_dirs)
                    .await
                    .map(|()| (Kind::Dir, String::new(), *host))
            }
            Action::PutFile { host, digest, .. } => {
                place(guest, target, path, &guest_path, Placing::File)
                    .await
                    .map(|()| (Kind::File, digest.clone(), *host))
            }
            Action::PutSymlink {
                target: link_target,
                host,
                digest,
                ..
            } => place(
                guest,
                target,
                path,
                &guest_path,
                Placing::Symlink(link_target),
            )
            .await
            .map(|()| (Kind::Symlink, digest.clone(), *host)),
        };

        match outcome {
            Ok((kind, digest, host)) => {
                // Only now, with the rename behind us: what the guest reports
                // for its own copy is what the ledger records for the guest
                // side, because a side's mtime is only ever compared against
                // its own.
                match guest.lstat(&guest_path).await {
                    Ok(Some(attrs)) => {
                        ledger.entries.insert(
                            path.to_string(),
                            Agreed {
                                kind,
                                digest,
                                host,
                                guest: Side::new(attrs.size, attrs.mtime_ns),
                            },
                        );
                        done.placed += 1;
                    }
                    // Placed but unverifiable: leaving the agreement out is
                    // the safe direction — the next pass hashes both sides
                    // and adopts them if they match.
                    Ok(None) => done.failures.push(Failure {
                        path: path.to_string(),
                        why: "the guest reports nothing there after the rename".into(),
                    }),
                    Err(e) => done.failures.push(Failure {
                        path: path.to_string(),
                        why: format!("placed, but the guest could not be re-read: {e:#}"),
                    }),
                }
            }
            Err(e) => done.failures.push(Failure {
                path: path.to_string(),
                why: format!("{e:#}"),
            }),
        }
    }
    done
}

/// What is being written into place.
enum Placing<'a> {
    File,
    Symlink(&'a str),
}

/// Write it under a temp name in the target's own directory, then rename it
/// over. The failure path removes the temp: a half-written scratch file left
/// in the tree is litter the developer would have to explain.
async fn place(
    guest: &dyn GuestFs,
    target: &Target,
    rel: &str,
    guest_path: &str,
    what: Placing<'_>,
) -> anyhow::Result<()> {
    let temp = temp_beside(guest_path);
    // A temp left by an earlier attempt is not an obstacle: the name is
    // derived from the path, so the previous pass's leftovers are this pass's
    // to clear.
    let _ = guest.remove(&temp).await;
    let written = match what {
        Placing::File => guest.push(&target.host_root.join(rel), &temp).await,
        // Windows picks a different object for a file link and a directory
        // link at creation, and vmlab never follows a link to find out which
        // — that is the Windows preconditions' business, alongside the
        // warning §19.4 says a failed symlink owes. On a Linux guest the kind
        // is ignored outright.
        Placing::Symlink(link) => guest.symlink(link, &temp, LinkKind::File).await,
    };
    if let Err(e) = written {
        let _ = guest.remove(&temp).await;
        return Err(e);
    }
    if let Err(e) = guest.rename(&temp, guest_path).await {
        let _ = guest.remove(&temp).await;
        return Err(e);
    }
    Ok(())
}

/// The temp name for one target, in the target's own directory so the rename
/// is atomic. Derived from the target rather than random, so a crashed pass
/// leaves at most one leftover per path and the next pass reclaims it.
fn temp_beside(guest_path: &str) -> String {
    use sha2::{Digest, Sha256};
    let tag = hex::encode(&Sha256::digest(guest_path.as_bytes())[..8]);
    match guest_path.rfind(['/', '\\']) {
        Some(cut) => format!("{}{TEMP_PREFIX}{tag}", &guest_path[..cut + 1]),
        None => format!("{TEMP_PREFIX}{tag}"),
    }
}

/// The guest still holds something of its own in there.
fn is_not_empty(e: &anyhow::Error) -> bool {
    e.downcast_ref::<FileOpsError>()
        .is_some_and(|e| e.code == ErrorCode::NotEmpty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::super::guest::fake::{FakeGuest, Node};
    use super::super::plan::{Inputs, State, reconcile};
    use super::super::scan::host_scan;

    const CAP: u64 = 1 << 30;

    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        dir
    }

    fn target(root: &Path) -> Target {
        Target {
            host_root: root.to_path_buf(),
            guest_root: "/src".into(),
            case_sensitive_dirs: false,
        }
    }

    /// One whole pass: scan the host, probe the guest, reconcile, apply.
    async fn pass(root: &Path, guest: &FakeGuest, ledger: &mut Ledger) -> Applied {
        let (scan, ignores) = host_scan(root, ledger, CAP).unwrap();
        let paths: BTreeSet<String> = scan
            .tree
            .keys()
            .chain(ledger.entries.keys())
            .cloned()
            .collect();
        let probe = super::super::scan::probe_guest(guest, "/src", &paths, ledger).await;
        let undecided: BTreeSet<String> = scan
            .skipped
            .iter()
            .chain(probe.skipped.iter())
            .map(|s| s.path.clone())
            .collect();
        let guest_owned: BTreeSet<String> = ledger
            .entries
            .keys()
            .filter(|path| ignores.verdict(path, false).is_guest_owned())
            .cloned()
            .collect();
        let plan = reconcile(&Inputs {
            host: &scan.tree,
            guest: &probe.tree,
            ledger,
            undecided: &undecided,
            guest_owned: &guest_owned,
            max_file_bytes: CAP,
        });
        apply(guest, &target(root), &plan, ledger).await
    }

    fn ledger_for(root: &Path) -> Ledger {
        Ledger::new(root, "/src")
    }

    /// The acceptance case: a declared workspace appears in the guest, whole.
    #[tokio::test]
    async fn the_first_pass_seeds_the_guest() {
        let dir = workspace(&[("src/main.rs", "fn main() {}"), ("README.md", "hi")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.failures, vec![]);
        assert_eq!(
            guest.text("/src/src/main.rs").as_deref(),
            Some("fn main() {}")
        );
        assert_eq!(guest.text("/src/README.md").as_deref(), Some("hi"));
        assert_eq!(guest.get("/src/src"), Some(Node::Dir));
        assert_eq!(done.placed, 3);
    }

    /// Every apply lands under a temp name in the target's own directory and
    /// is renamed over. Nothing is ever written to the target itself, so a
    /// reader never sees a half-written file.
    #[tokio::test]
    async fn a_file_is_written_beside_its_target_and_renamed_over() {
        let dir = workspace(&[("a.txt", "hello")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        let writes = guest.writes();
        assert!(
            writes.iter().any(|w| w.starts_with("/src/.vmlab-sync.")),
            "{writes:?}"
        );
        assert!(
            !writes.iter().any(|w| w == "/src/a.txt"),
            "the target was written to directly: {writes:?}"
        );
        assert_eq!(guest.text("/src/a.txt").as_deref(), Some("hello"));
        // And the temp is gone: the rename consumed it.
        assert!(
            !guest.paths().iter().any(|p| p.contains(".vmlab-sync.")),
            "{:?}",
            guest.paths()
        );
    }

    /// The ordering the ledger's whole value rests on. A transfer that fails
    /// leaves nothing agreed, so the next pass tries again rather than
    /// concluding "unchanged" about a file that was never placed.
    #[tokio::test]
    async fn a_failed_transfer_agrees_to_nothing_and_resumes_by_re_transfer() {
        let dir = workspace(&[("a.txt", "hello")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        guest.fail_push(&temp_beside("/src/a.txt"));

        let first = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(first.placed, 0);
        assert_eq!(first.failures.len(), 1);
        assert_eq!(first.failures[0].path, "a.txt");
        assert!(!ledger.entries.contains_key("a.txt"), "nothing was agreed");
        assert!(guest.get("/src/a.txt").is_none());
        assert!(
            !guest.paths().iter().any(|p| p.contains(".vmlab-sync.")),
            "the temp was left behind: {:?}",
            guest.paths()
        );

        // Resume is re-transfer: the whole file again, from the start.
        let second = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(second.failures, vec![]);
        assert_eq!(guest.text("/src/a.txt").as_deref(), Some("hello"));
        assert!(ledger.entries.contains_key("a.txt"));
    }

    /// Each side's own size and mtime, recorded from that side. The guest's
    /// come from the guest after the rename; the host's from the host scan.
    #[tokio::test]
    async fn the_ledger_records_each_side_from_that_side() {
        let dir = workspace(&[("a.txt", "hello")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        let agreed = &ledger.entries["a.txt"];
        let host_meta = std::fs::symlink_metadata(dir.path().join("a.txt")).unwrap();
        assert_eq!(agreed.host.size, host_meta.len());
        assert_ne!(agreed.host.mtime_ns, agreed.guest.mtime_ns);
        // The fake guest stamps everything it writes with 9.
        assert_eq!(agreed.guest.mtime_ns, 9);
    }

    /// A settled workspace does nothing at all on the next pass: the
    /// pre-filter vouches for both sides and no digest is even asked for.
    #[tokio::test]
    async fn a_settled_workspace_transfers_nothing_on_the_next_pass() {
        let dir = workspace(&[("a.txt", "hello"), ("src/b.rs", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        let again = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(again, Applied::default());
    }

    /// A host-side edit reaches the guest, and the ledger moves with it.
    #[tokio::test]
    async fn a_host_side_edit_propagates() {
        let dir = workspace(&[("a.txt", "one")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        let done = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(done.placed, 1);
        assert_eq!(guest.text("/src/a.txt").as_deref(), Some("two"));
    }

    /// A host-side delete just removes it: the guest copy is the
    /// reconstructible one, so this direction's deletes are unguarded.
    #[tokio::test]
    async fn a_host_side_delete_removes_the_guest_copy_and_the_agreement() {
        let dir = workspace(&[("a.txt", "one"), ("b.txt", "two")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let done = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(done.removed, 1);
        assert!(guest.get("/src/a.txt").is_none());
        assert!(!ledger.entries.contains_key("a.txt"));
        assert!(guest.get("/src/b.txt").is_some());
    }

    /// The rule that keeps a wiped `.vmlab/` from eating a developer's work:
    /// matching digests are adopted for free, and differing ones never get
    /// seeded over.
    #[tokio::test]
    async fn a_wiped_ledger_adopts_what_matches_and_refuses_to_seed_over_what_does_not() {
        let dir = workspace(&[("same.txt", "shared"), ("differs.txt", "host side")]);
        let guest = FakeGuest::new();
        guest.dir("/src");
        guest.file("/src/same.txt", "shared", 4);
        guest.file("/src/differs.txt", "guest side", 4);
        let mut ledger = ledger_for(dir.path());

        let (scan, _) = host_scan(dir.path(), &ledger, CAP).unwrap();
        let paths: BTreeSet<String> = scan.tree.keys().cloned().collect();
        let probe = super::super::scan::probe_guest(&guest, "/src", &paths, &ledger).await;
        let plan = reconcile(&Inputs {
            host: &scan.tree,
            guest: &probe.tree,
            ledger: &ledger,
            undecided: &BTreeSet::new(),
            guest_owned: &BTreeSet::new(),
            max_file_bytes: CAP,
        });
        let done = apply(&guest, &target(dir.path()), &plan, &mut ledger).await;

        assert_eq!(done.adopted, 1);
        assert!(ledger.entries.contains_key("same.txt"));
        assert_eq!(
            guest.text("/src/differs.txt").as_deref(),
            Some("guest side"),
            "the guest's copy was overwritten"
        );
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].path, "differs.txt");
    }

    /// A guest-side special file is skipped by name and never enters the
    /// ledger — and it is not mistaken for absence, so nothing is written
    /// over it.
    #[tokio::test]
    async fn a_guest_side_special_file_is_skipped_rather_than_overwritten() {
        let dir = workspace(&[("build.sock", "host thinks this is a file")]);
        let guest = FakeGuest::new();
        guest.put("/src/build.sock", Node::Special, 1);
        let mut ledger = ledger_for(dir.path());

        let done = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(done, Applied::default());
        assert_eq!(guest.get("/src/build.sock"), Some(Node::Special));
        assert!(!ledger.entries.contains_key("build.sock"));
    }

    /// A path the login cannot read is a named skip and nothing more — never
    /// a halt, and never a blind overwrite.
    #[tokio::test]
    async fn an_unreadable_guest_path_is_left_alone() {
        let dir = workspace(&[("a.txt", "host")]);
        let guest = FakeGuest::new();
        guest.file("/src/a.txt", "guest", 1);
        guest.unreadable("/src/a.txt");
        let mut ledger = ledger_for(dir.path());

        let done = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(done, Applied::default());
        assert_eq!(guest.text("/src/a.txt").as_deref(), Some("guest"));
    }

    /// Symlinks cross verbatim, are never followed, and their targets are
    /// never translated — a link to `/usr/lib/foo` lands as that string even
    /// where it will dangle.
    #[tokio::test]
    async fn a_symlink_crosses_verbatim() {
        let dir = workspace(&[("src/real.rs", "x")]);
        std::os::unix::fs::symlink("/usr/lib/foo", dir.path().join("lib")).unwrap();
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());

        pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(
            guest.get("/src/lib"),
            Some(Node::Symlink("/usr/lib/foo".into()))
        );
        assert_eq!(ledger.entries["lib"].kind, Kind::Symlink);
    }

    /// Guest-owned means untouched: the guest keeps its own diverging content
    /// and the host never sees it either.
    #[tokio::test]
    async fn a_guest_owned_path_is_never_touched() {
        let dir = workspace(&[(".gitignore", "node_modules/\n"), ("app.js", "x")]);
        let guest = FakeGuest::new();
        guest.file("/src/node_modules/pkg/index.js", "guest-native", 1);
        let mut ledger = ledger_for(dir.path());

        pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(
            guest.text("/src/node_modules/pkg/index.js").as_deref(),
            Some("guest-native")
        );
        assert!(!ledger.entries.keys().any(|p| p.starts_with("node_modules")));
    }

    /// **Leaving scope is free.** Ignoring a path that was already synced
    /// must not read as a host-side delete: the ledger entry goes, and both
    /// copies stay exactly where they are.
    #[tokio::test]
    async fn ignoring_a_synced_path_leaves_both_copies_alone() {
        let dir = workspace(&[("app.log", "logged"), ("app.js", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(guest.text("/src/app.log").as_deref(), Some("logged"));

        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.removed, 0, "the guest's copy was deleted");
        assert_eq!(guest.text("/src/app.log").as_deref(), Some("logged"));
        assert!(dir.path().join("app.log").exists());
        assert!(!ledger.entries.contains_key("app.log"));
    }

    /// The host dropping a directory the guest still holds its own content in
    /// leaves it standing — guest-owned paths are never touched — and clears
    /// the agreement, so the removal is not retried for the life of the
    /// machine.
    #[tokio::test]
    async fn a_directory_the_guest_still_owns_content_in_is_left_standing() {
        let dir = workspace(&[("pkg/app.js", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;
        // Guest-owned, so the syncer never saw it and never will.
        guest.file("/src/pkg/target/out", "guest-built", 1);

        std::fs::remove_dir_all(dir.path().join("pkg")).unwrap();
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.failures, vec![]);
        assert_eq!(done.left_standing, vec!["pkg".to_string()]);
        assert!(guest.get("/src/pkg/app.js").is_none());
        assert_eq!(
            guest.text("/src/pkg/target/out").as_deref(),
            Some("guest-built")
        );
        assert!(!ledger.entries.contains_key("pkg"));

        // And it is not retried, pass after pass, forever.
        let again = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(again, Applied::default());
    }

    /// The size guard refuses the one file, before transfer, and the rest of
    /// the tree still lands.
    #[tokio::test]
    async fn the_size_guard_refuses_before_the_transfer() {
        let dir = workspace(&[("big.vhdx", "0123456789"), ("small.rs", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());

        let (scan, _) = host_scan(dir.path(), &ledger, 4).unwrap();
        let plan = reconcile(&Inputs {
            host: &scan.tree,
            guest: &BTreeMap::<String, State>::new(),
            ledger: &ledger,
            undecided: &BTreeSet::new(),
            guest_owned: &BTreeSet::new(),
            max_file_bytes: 4,
        });
        apply(&guest, &target(dir.path()), &plan, &mut ledger).await;

        assert!(guest.get("/src/big.vhdx").is_none());
        assert!(guest.get("/src/small.rs").is_some());
        assert_eq!(plan.oversize.len(), 1);
        assert!(plan.oversize[0].to_string().contains("big.vhdx"));
    }

    /// The temp lives in the target's own directory, so the rename is atomic
    /// rather than a cross-volume copy.
    #[test]
    fn a_temp_name_sits_beside_its_target() {
        let temp = temp_beside("/src/a/b.txt");
        assert!(temp.starts_with("/src/a/.vmlab-sync."), "{temp}");
        assert_ne!(temp, temp_beside("/src/a/c.txt"));
        assert_eq!(temp, temp_beside("/src/a/b.txt"), "derived, not random");
        assert!(temp_beside("b.txt").starts_with(".vmlab-sync."));
        assert!(temp_beside("C:\\src\\b.txt").starts_with("C:\\src\\.vmlab-sync."));
    }
}
