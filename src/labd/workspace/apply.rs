//! Executing a reconciliation against both sides (PRD §19.6).
//!
//! Two invariants this module exists to hold, in this order:
//!
//! 1. **Every apply is temp-name-then-rename, with the temp in the same
//!    directory as its target** — in *both* directions, so the rename is
//!    atomic rather than a cross-volume copy, and on Windows it inherits the
//!    case-sensitivity flag set at `mkdir`. The temp name is in the ignore
//!    floor, so it never becomes a sync object itself. Guest→host it is
//!    load-bearing for a second reason: the target is the canonical copy, and
//!    a torn half-written file there is the one loss nothing re-derives.
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
//!
//! What the two directions do *not* share is where the receiving side's own
//! `(size, mtime)` comes from: each is read back from the side that took the
//! rename, because a side's record is only ever compared against itself.
//!
//! Both are equally **idempotent** — an already-absent removal and an
//! already-present directory are the state the action asked for, so both are
//! successes. Guest-side that rule lives one layer down, in the file session
//! itself; host-side it is spelled out here, because `std::fs` has no opinion.

use std::path::PathBuf;

use super::guest::GuestFs;
use super::ignore::TEMP_PREFIX;
use super::ledger::{Agreed, Kind, Ledger, Side};
use super::plan::{Action, Direction, Plan};
use super::scan::{join_guest, mtime_ns};
use crate::labd::vm_agent::{ErrorCode, FileOpsError, LinkKind};

/// Where a workspace's two copies live, and how the guest wants them made.
pub struct Target {
    /// The canonical host directory.
    pub host_root: PathBuf,
    /// Where the working copy lands in the guest.
    pub guest_root: String,
    /// Create directories case-sensitive (§19.6). NTFS accepts the flag only
    /// while a directory is empty, which the syncer's always is, so it rides
    /// every `mkdir` — **per directory, never by inheritance**, because
    /// Microsoft's own documentation contradicts itself on whether
    /// inheritance holds and setting it each time costs nothing.
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

/// What happened to one side's copy.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub placed: usize,
    pub removed: usize,
}

impl Counts {
    fn quiet(&self) -> bool {
        self.placed == 0 && self.removed == 0
    }
}

/// What one pass actually did. Counted per direction, because "the workspace
/// moved 400 files" says nothing about which copy just changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub to_guest: Counts,
    pub to_host: Counts,
    /// Paths adopted as agreed without a transfer, because both sides already
    /// held the same content.
    pub adopted: usize,
    /// Directories one side dropped that the other still holds its own
    /// content in — left standing, named, and out of the ledger.
    pub left_standing: Vec<String>,
    /// Symlinks that were attempted and did not take (§19.6, §19.4). Held
    /// apart from [`failures`](Applied::failures) because the remedy is not
    /// the same: a symlink-capable image is a documented precondition, and
    /// naming the link is how vmlab declines to work around it silently.
    pub symlinks_refused: Vec<Failure>,
    /// Directories that had to be created without the case-sensitivity flag
    /// because the guest would not take it. The declaration said the flag was
    /// available and the guest disagreed, so the workspace is degraded from
    /// here on and collisions become refusals.
    pub case_insensitive_dirs: Vec<Failure>,
    pub failures: Vec<Failure>,
}

impl Applied {
    /// Whether anything at all moved, in either direction.
    pub fn moved(&self) -> bool {
        !self.to_guest.quiet() || !self.to_host.quiet() || self.adopted > 0
    }

    /// The tally the actions going this way land in.
    fn counts(&mut self, direction: Direction) -> &mut Counts {
        match direction {
            Direction::ToGuest => &mut self.to_guest,
            Direction::ToHost => &mut self.to_host,
        }
    }
}

/// Carry out `plan` against both sides, updating `ledger` as each apply lands.
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

    // The workspace root is already there, made with the flag by
    // [`prepare_root`](super::windows::prepare_root) before the plan this is
    // carrying out was computed — because whether the flag takes is one of
    // that plan's inputs.

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
        let direction = action.direction();
        let landed = match direction {
            Direction::ToGuest => {
                into_guest(guest, target, action, &mut done.case_insensitive_dirs).await
            }
            Direction::ToHost => onto_host(guest, target, action).await,
        };
        match landed {
            Ok(Landed::Removed) => {
                ledger.entries.remove(path);
                done.counts(direction).removed += 1;
            }
            // The other side still holds content of its own in there — a
            // guest-owned `node_modules` under a directory the host just
            // dropped, or a host-side `target/` under one the guest did.
            // Neither direction touches guest-owned paths, so the directory
            // stays and the *agreement* about it goes: a removal that can
            // never succeed must not be retried on every pass for the life of
            // the machine.
            Ok(Landed::LeftStanding) => {
                ledger.entries.remove(path);
                done.left_standing.push(path.to_string());
            }
            Ok(Landed::Placed { kind, digest, side }) => {
                // Only now, with the rename behind us: what the *receiving*
                // side reports for its own copy is what the ledger records
                // for that side, because a side's record is only ever
                // compared against itself.
                match receiving_side(guest, target, path, direction, kind).await {
                    Ok(other) => {
                        let (host, guest_side) = if direction.source_is_host() {
                            (side, other)
                        } else {
                            (other, side)
                        };
                        ledger.entries.insert(
                            path.to_string(),
                            Agreed {
                                kind,
                                digest,
                                host,
                                guest: guest_side,
                            },
                        );
                        done.counts(direction).placed += 1;
                    }
                    // Placed but unverifiable: leaving the agreement out is
                    // the safe direction — the next pass hashes both sides
                    // and adopts them if they match.
                    Err(e) => done.failures.push(Failure {
                        path: path.to_string(),
                        why: format!("{e:#}"),
                    }),
                }
            }
            // A symlink the *guest* would not create is the one failure with
            // its own name: §19.4 makes a symlink-capable image a
            // precondition, and vmlab warns rather than working around it.
            // Only that way round — a link the host refuses is an ordinary
            // host-side failure, and no image precondition is at stake.
            // Everything else about it is ordinary too: nothing is agreed, so
            // the next pass tries again.
            Err(e) if refused_symlink(action) => done.symlinks_refused.push(Failure {
                path: path.to_string(),
                why: format!(
                    "the guest would not create this symlink, which a dev-capable image must be \
                     able to do (§19.4): {e:#}"
                ),
            }),
            Err(e) => done.failures.push(Failure {
                path: path.to_string(),
                why: format!("{e:#}"),
            }),
        }
    }
    done
}

/// Whether a failed action was a link this guest was asked to create.
fn refused_symlink(action: &Action) -> bool {
    matches!(action, Action::PutSymlink { direction, .. } if direction.source_is_host())
}

/// Create one directory, asking for the case-sensitivity flag where the
/// machine has been found to take it (§19.6).
///
/// The machine-wide answer is settled before the plan is even computed, so
/// this is the odd directory that disagrees with it. Where the flag was asked
/// for and refused, the directory is created **without** it and said out loud:
/// failing the whole workspace over the flag would be worse than the collision
/// it guards against, and a second attempt that finds the directory already
/// there is success, so the fallback lands whether the first attempt got as
/// far as creating it or not.
async fn make_dir(
    guest: &dyn GuestFs,
    target: &Target,
    guest_path: &str,
    degraded: &mut Vec<Failure>,
) -> anyhow::Result<()> {
    match guest.mkdir(guest_path, target.case_sensitive_dirs).await {
        Ok(()) => Ok(()),
        Err(refused) if !target.case_sensitive_dirs => Err(refused),
        Err(refused) => {
            guest.mkdir(guest_path, false).await.map_err(|_| refused)?;
            degraded.push(Failure {
                path: guest_path.to_string(),
                why: "this guest would not make the directory case-sensitive, so two host paths \
                      differing only in case cannot both land in it and are refused by name"
                    .into(),
            });
            Ok(())
        }
    }
}

/// What one action did to the side it was carried to.
enum Landed {
    Removed,
    /// The directory could not go: the other side owns content in it.
    LeftStanding,
    Placed {
        kind: Kind,
        digest: String,
        /// The **moving** side's own `(size, mtime)`.
        side: Side,
    },
}

/// Carry a host change into the guest's working copy.
///
/// `degraded` collects the directories this guest would not take §19.6's
/// case-sensitivity flag on, where the machine-wide probe said it would: the
/// tree still lands, and the workspace is degraded from there on.
async fn into_guest(
    guest: &dyn GuestFs,
    target: &Target,
    action: &Action,
    degraded: &mut Vec<Failure>,
) -> anyhow::Result<Landed> {
    let path = action.path();
    let guest_path = join_guest(&target.guest_root, path);
    match action {
        Action::Remove { kind, .. } => {
            let removed = match kind {
                Kind::Dir => guest.rmdir(&guest_path).await,
                _ => guest.remove(&guest_path).await,
            };
            match removed {
                Ok(()) => Ok(Landed::Removed),
                Err(e) if is_not_empty(&e) => Ok(Landed::LeftStanding),
                Err(e) => Err(e),
            }
        }
        // A directory needs no temp: creating one destroys nothing, and on
        // NTFS the case-sensitivity flag can only be set on the directory
        // that will actually hold the tree.
        Action::MakeDir { side, .. } => {
            make_dir(guest, target, &guest_path, degraded)
                .await
                .map(|()| Landed::Placed {
                    kind: Kind::Dir,
                    digest: String::new(),
                    side: *side,
                })
        }
        Action::PutFile { side, digest, .. } => {
            place_in_guest(guest, target, path, &guest_path, Placing::File)
                .await
                .map(|()| Landed::Placed {
                    kind: Kind::File,
                    digest: digest.clone(),
                    side: *side,
                })
        }
        Action::PutSymlink {
            target: link_target,
            side,
            digest,
            dir_link,
            ..
        } => place_in_guest(
            guest,
            target,
            path,
            &guest_path,
            Placing::Symlink(link_target, *dir_link),
        )
        .await
        .map(|()| Landed::Placed {
            kind: Kind::Symlink,
            digest: digest.clone(),
            side: *side,
        }),
    }
}

/// Carry a guest change onto the canonical copy. Plain host filesystem work,
/// under the same discipline: temp beside the target, then rename.
async fn onto_host(
    guest: &dyn GuestFs,
    target: &Target,
    action: &Action,
) -> anyhow::Result<Landed> {
    let path = action.path();
    let host_path = target.host_root.join(path);
    match action {
        Action::Remove { kind, .. } => {
            let removed = match kind {
                Kind::Dir => std::fs::remove_dir(&host_path),
                _ => std::fs::remove_file(&host_path),
            };
            match removed {
                Ok(()) => Ok(Landed::Removed),
                // Already gone is the state the action asked for.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Landed::Removed),
                Err(e) if host_not_empty(&e) => Ok(Landed::LeftStanding),
                Err(e) => Err(anyhow::Error::new(e).context(format!("removing {path}"))),
            }
        }
        // A directory needs no temp here either: creating one destroys
        // nothing, and one that is already there is the state the action
        // asked for.
        Action::MakeDir { side, .. } => {
            let placed = Landed::Placed {
                kind: Kind::Dir,
                digest: String::new(),
                side: *side,
            };
            match std::fs::create_dir(&host_path) {
                Ok(()) => Ok(placed),
                Err(_) if host_path.is_dir() => Ok(placed),
                Err(e) => Err(anyhow::Error::new(e).context(format!("creating {path}"))),
            }
        }
        Action::PutFile { side, digest, .. } => place_on_host(&host_path, |temp| {
            let temp = temp.to_path_buf();
            let remote = join_guest(&target.guest_root, path);
            async move { guest.pull(&remote, &temp).await }
        })
        .await
        .map(|()| Landed::Placed {
            kind: Kind::File,
            digest: digest.clone(),
            side: *side,
        }),
        Action::PutSymlink {
            target: link_target,
            side,
            digest,
            ..
        } => place_on_host(&host_path, |temp| {
            let temp = temp.to_path_buf();
            async move {
                // Verbatim, and never followed. A guest-side link to
                // `C:\Users\dev` lands host-side as that string and dangles,
                // which is correct: vmlab moves what it is told to move and
                // translates nothing.
                std::os::unix::fs::symlink(link_target, &temp).map_err(|e| {
                    anyhow::Error::new(e).context(format!("linking {}", temp.display()))
                })
            }
        })
        .await
        .map(|()| Landed::Placed {
            kind: Kind::Symlink,
            digest: digest.clone(),
            side: *side,
        }),
    }
}

/// The receiving side's own `(size, mtime)`, read back after the rename.
async fn receiving_side(
    guest: &dyn GuestFs,
    target: &Target,
    path: &str,
    direction: Direction,
    kind: Kind,
) -> anyhow::Result<Side> {
    match direction {
        Direction::ToGuest => {
            let guest_path = join_guest(&target.guest_root, path);
            match guest.lstat(&guest_path).await {
                Ok(Some(attrs)) => Ok(Side::new(attrs.size, attrs.mtime_ns)),
                Ok(None) => Err(anyhow::anyhow!(
                    "the guest reports nothing there after the rename"
                )),
                Err(e) => Err(e.context("placed, but the guest could not be re-read")),
            }
        }
        Direction::ToHost => {
            let host_path = target.host_root.join(path);
            let meta = std::fs::symlink_metadata(&host_path).map_err(|e| {
                anyhow::Error::new(e).context(
                    "placed, but the host copy could not \
                                                            be re-read",
                )
            })?;
            Ok(Side::new(
                host_size(&meta, &host_path, kind),
                mtime_ns(&meta),
            ))
        }
    }
}

/// What the host scan will report as this path's size next pass — the link
/// *target's* length for a symlink, because a link's target string is its
/// content and the two records have to be the same shape.
fn host_size(meta: &std::fs::Metadata, path: &std::path::Path, kind: Kind) -> u64 {
    match kind {
        Kind::Symlink => std::fs::read_link(path)
            .map(|t| t.to_string_lossy().len() as u64)
            .unwrap_or(0),
        _ => meta.len(),
    }
}

/// What is being written into place.
enum Placing<'a> {
    File,
    /// The target string, and whether Windows needs a directory link for it —
    /// decided by the plan from the host tree, never by following the link.
    Symlink(&'a str, bool),
}

/// Write it under a temp name in the guest's own directory, then rename it
/// over. The failure path removes the temp: a half-written scratch file left
/// in the tree is litter the developer would have to explain.
async fn place_in_guest(
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
        // link at creation and cannot infer which from a target that is not
        // there yet. The plan decided it from the host tree it already holds,
        // without ever following the link. On a Linux guest the kind is
        // ignored outright.
        Placing::Symlink(link, dir) => {
            let kind = if dir { LinkKind::Dir } else { LinkKind::File };
            guest.symlink(link, &temp, kind).await
        }
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

/// The same discipline against the canonical copy: `write` fills a temp beside
/// the target, and only a completed one is renamed over it.
///
/// This is the direction where it matters most. The target here is the copy
/// that survives `destroy`, so a reader catching it half-written — a host-side
/// `cargo build`, a `git status`, the developer's own editor — is reading the
/// one copy nothing re-derives.
async fn place_on_host<F, Fut>(host_path: &std::path::Path, write: F) -> anyhow::Result<()>
where
    F: FnOnce(&std::path::Path) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let temp = host_temp(host_path);
    let _ = std::fs::remove_file(&temp);
    if let Some(parent) = host_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::Error::new(e).context(format!("creating {}", parent.display())))?;
    }
    if let Err(e) = write(&temp).await {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&temp, host_path) {
        let _ = std::fs::remove_file(&temp);
        return Err(anyhow::Error::new(e).context(format!("writing {}", host_path.display())));
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

/// The same, for a host path: same directory, same derived name, same reason.
///
/// Built from the path's own bytes and joined onto its real parent rather than
/// re-parsed out of a string, because a host filename need not be UTF-8 and a
/// lossy one would name a temp in a directory that is not the target's.
fn host_temp(host_path: &std::path::Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let tag = hex::encode(&Sha256::digest(host_path.as_os_str().as_bytes())[..8]);
    let name = format!("{TEMP_PREFIX}{tag}");
    match host_path.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// The guest still holds something of its own in there.
fn is_not_empty(e: &anyhow::Error) -> bool {
    e.downcast_ref::<FileOpsError>()
        .is_some_and(|e| e.code == ErrorCode::NotEmpty)
}

/// The same, host-side: a directory the guest dropped that still holds a
/// guest-owned `target/` the syncer has never touched.
fn host_not_empty(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::DirectoryNotEmpty
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

    /// The same guest, told apart by the flag the syncer asks for.
    fn case_sensitive_target(root: &Path) -> Target {
        Target {
            case_sensitive_dirs: true,
            ..target(root)
        }
    }

    /// One whole pass: scan the host, probe the guest, reconcile, apply.
    async fn pass(root: &Path, guest: &FakeGuest, ledger: &mut Ledger) -> Applied {
        run_pass(root, guest, ledger, target(root), false, &[]).await
    }

    /// The same, with `dirty` standing in for what the guest's own watcher
    /// drained — the only way a path the host has never heard of enters a
    /// pass at all.
    async fn pass_with(
        root: &Path,
        guest: &FakeGuest,
        ledger: &mut Ledger,
        dirty: &[&str],
    ) -> Applied {
        run_pass(root, guest, ledger, target(root), false, dirty).await
    }

    /// The same, against a guest with §19.6's Windows semantics: the flag the
    /// syncer asks for at every `mkdir`, and whether this guest folds case.
    async fn pass_onto(
        root: &Path,
        guest: &FakeGuest,
        ledger: &mut Ledger,
        target: Target,
        case_folding: bool,
    ) -> Applied {
        run_pass(root, guest, ledger, target, case_folding, &[]).await
    }

    async fn run_pass(
        root: &Path,
        guest: &FakeGuest,
        ledger: &mut Ledger,
        mut target: Target,
        case_folding: bool,
        dirty: &[&str],
    ) -> Applied {
        // As the syncer does: the root, and what the guest will really take,
        // before the plan that depends on it.
        target.case_sensitive_dirs = super::super::windows::prepare_root(
            guest,
            &target.guest_root,
            target.case_sensitive_dirs,
        )
        .await
        .unwrap();
        let (scan, ignores) = host_scan(root, ledger, CAP).unwrap();
        let paths: BTreeSet<String> = scan
            .tree
            .keys()
            .chain(ledger.entries.keys())
            .cloned()
            .chain(dirty.iter().map(|p| (*p).to_string()))
            .collect();
        let probe = super::super::scan::probe_guest(guest, "/src", &paths, ledger, CAP).await;
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
            resolved: &BTreeMap::new(),
            max_file_bytes: CAP,
            case_folding,
        });
        apply(guest, &target, &plan, ledger).await
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
        assert_eq!(done.to_guest.placed, 3);
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
        assert_eq!(first.to_guest.placed, 0);
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
        assert_eq!(done.to_guest.placed, 1);
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
        assert_eq!(done.to_guest.removed, 1);
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
        let probe = super::super::scan::probe_guest(&guest, "/src", &paths, &ledger, CAP).await;
        let plan = reconcile(&Inputs {
            host: &scan.tree,
            guest: &probe.tree,
            ledger: &ledger,
            undecided: &BTreeSet::new(),
            guest_owned: &BTreeSet::new(),
            resolved: &BTreeMap::new(),
            max_file_bytes: CAP,
            case_folding: false,
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

        assert_eq!(done.to_guest.removed, 0, "the guest's copy was deleted");
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
            resolved: &BTreeMap::new(),
            max_file_bytes: 4,
            case_folding: false,
        });
        apply(&guest, &target(dir.path()), &plan, &mut ledger).await;

        assert!(guest.get("/src/big.vhdx").is_none());
        assert!(guest.get("/src/small.rs").is_some());
        assert_eq!(plan.oversize.len(), 1);
        assert!(plan.oversize[0].to_string().contains("big.vhdx"));
    }

    /// **Every** directory the syncer creates carries the flag, including the
    /// workspace root — and each one is set at its own creation, because
    /// inheritance must not be relied on (§19.6).
    #[tokio::test]
    async fn every_directory_the_syncer_creates_is_made_case_sensitive() {
        let dir = workspace(&[("a/b/c/deep.rs", "x"), ("top.rs", "y")]);
        let guest = FakeGuest::new();
        guest.folding();
        let mut ledger = ledger_for(dir.path());

        let done = pass_onto(
            dir.path(),
            &guest,
            &mut ledger,
            case_sensitive_target(dir.path()),
            false,
        )
        .await;
        assert_eq!(done.failures, vec![]);
        assert_eq!(done.case_insensitive_dirs, vec![]);

        for made in ["/src", "/src/a", "/src/a/b", "/src/a/b/c"] {
            assert!(
                guest.is_case_sensitive(made),
                "{made} was created without the flag: {:?}",
                guest.paths()
            );
        }
    }

    /// The acceptance case the flag exists for: two host paths differing only
    /// in case both land, and neither overwrites the other.
    #[tokio::test]
    async fn two_paths_differing_only_in_case_both_land_and_neither_wins() {
        let dir = workspace(&[("src/Foo.cs", "upper"), ("src/foo.cs", "lower")]);
        let guest = FakeGuest::new();
        // A default NTFS volume: one object for both names, unless the
        // directory was made case-sensitive.
        guest.folding();
        let mut ledger = ledger_for(dir.path());

        let done = pass_onto(
            dir.path(),
            &guest,
            &mut ledger,
            case_sensitive_target(dir.path()),
            false,
        )
        .await;

        assert_eq!(done.failures, vec![]);
        assert_eq!(guest.text("/src/src/Foo.cs").as_deref(), Some("upper"));
        assert_eq!(guest.text("/src/src/foo.cs").as_deref(), Some("lower"));
        assert!(ledger.entries.contains_key("src/Foo.cs"));
        assert!(ledger.entries.contains_key("src/foo.cs"));
    }

    /// Without the flag the same two names *are* one object — which is why
    /// the refusal exists, and why this pass must never be what ships.
    #[tokio::test]
    async fn without_the_flag_the_second_write_would_land_on_the_first() {
        let dir = workspace(&[("src/Foo.cs", "upper"), ("src/foo.cs", "lower")]);
        let guest = FakeGuest::new();
        guest.folding();
        let mut ledger = ledger_for(dir.path());

        // Which is exactly what `case_folding` refuses: nothing is written.
        let done = pass_onto(dir.path(), &guest, &mut ledger, target(dir.path()), true).await;
        assert_eq!(done.to_guest.placed, 1, "only the directory");
        assert!(
            guest.get("/src/src/Foo.cs").is_none(),
            "{:?}",
            guest.paths()
        );
        assert!(guest.get("/src/src/foo.cs").is_none());
        assert!(!ledger.entries.contains_key("src/Foo.cs"));
        assert!(!ledger.entries.contains_key("src/foo.cs"));
    }

    /// One directory the guest refuses the flag on, where the machine-wide
    /// probe said it would take it. The tree still lands — failing the whole
    /// workspace over the flag is worse than the collision it guards against
    /// — and the directory is named, so the refusal can take over from there.
    #[tokio::test]
    async fn a_directory_that_refuses_the_flag_still_lands_and_is_named() {
        let dir = workspace(&[("pkg/app.js", "x")]);
        let guest = FakeGuest::new();
        guest.folding().refuse_case_flag_at("/src/pkg");
        let mut ledger = ledger_for(dir.path());

        let done = pass_onto(
            dir.path(),
            &guest,
            &mut ledger,
            case_sensitive_target(dir.path()),
            false,
        )
        .await;

        assert_eq!(done.failures, vec![], "the tree still landed");
        assert_eq!(guest.text("/src/pkg/app.js").as_deref(), Some("x"));
        let named: Vec<&str> = done
            .case_insensitive_dirs
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(named, vec!["/src/pkg"]);
        assert!(
            done.case_insensitive_dirs[0].why.contains("case"),
            "{:?}",
            done.case_insensitive_dirs[0]
        );
        // …and the root, which did take it, is unaffected.
        assert!(guest.is_case_sensitive("/src"));
    }

    /// **Attempted, and warned about by name** (§19.6/§19.4). Never worked
    /// around silently, and never a halt: the rest of the tree still lands.
    #[tokio::test]
    async fn a_symlink_that_will_not_take_is_named_rather_than_worked_around() {
        let dir = workspace(&[("src/real.rs", "x")]);
        std::os::unix::fs::symlink("src/real.rs", dir.path().join("lib")).unwrap();
        let guest = FakeGuest::new();
        guest.refuse_symlinks();
        let mut ledger = ledger_for(dir.path());

        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.failures, vec![], "not an ordinary failure");
        assert_eq!(done.symlinks_refused.len(), 1);
        assert_eq!(done.symlinks_refused[0].path, "lib");
        assert!(
            done.symlinks_refused[0].why.contains("§19.4"),
            "{:?}",
            done.symlinks_refused[0]
        );
        // Nothing agreed, no leftover temp, and the rest of the tree landed.
        assert!(!ledger.entries.contains_key("lib"));
        assert!(
            !guest.paths().iter().any(|p| p.contains(".vmlab-sync.")),
            "{:?}",
            guest.paths()
        );
        assert_eq!(guest.text("/src/src/real.rs").as_deref(), Some("x"));
    }

    /// **The syncer translates no bytes.** Git does all normalisation on both
    /// sides, from settings that now agree; a syncer that rewrote line endings
    /// would make the two sides' digests disagree forever.
    #[tokio::test]
    async fn the_syncer_translates_no_bytes_in_either_direction() {
        let dir = workspace(&[
            ("crlf.txt", "one\r\ntwo\r\n"),
            ("lf.txt", "one\ntwo\n"),
            ("mixed.txt", "one\r\ntwo\nthree\r"),
        ]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        for name in ["crlf.txt", "lf.txt", "mixed.txt"] {
            let host = std::fs::read(dir.path().join(name)).unwrap();
            assert_eq!(
                guest.text(&format!("/src/{name}")).unwrap().as_bytes(),
                host.as_slice(),
                "{name} was rewritten across the seam"
            );
        }
    }

    /// The direction the developer authors in. A guest-side edit lands on the
    /// canonical copy, and the agreement records each side from that side —
    /// the guest's from the guest, the host's read back after the rename.
    #[tokio::test]
    async fn a_guest_side_edit_lands_on_the_canonical_copy() {
        let dir = workspace(&[("a.txt", "one")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        guest.file("/src/a.txt", "two", 4_242);
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.to_host.placed, 1);
        assert_eq!(done.to_guest.placed, 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "two"
        );
        let agreed = &ledger.entries["a.txt"];
        assert_eq!(agreed.guest, Side::new(3, 4_242), "the guest's own record");
        let meta = std::fs::symlink_metadata(dir.path().join("a.txt")).unwrap();
        assert_eq!(agreed.host, Side::new(meta.len(), mtime_ns(&meta)));
    }

    /// A path only the guest has — the file the developer just created in
    /// their editor — reaches the host through the drained set, with its
    /// directory made first.
    #[tokio::test]
    async fn a_guest_created_tree_reaches_the_host() {
        let dir = workspace(&[("keep.rs", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        guest.dir("/src/feature");
        guest.file("/src/feature/mod.rs", "pub fn f() {}", 9);
        let done = pass_with(
            dir.path(),
            &guest,
            &mut ledger,
            &["feature", "feature/mod.rs"],
        )
        .await;

        assert_eq!(done.failures, vec![]);
        assert_eq!(done.to_host.placed, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("feature/mod.rs")).unwrap(),
            "pub fn f() {}"
        );
        assert!(ledger.entries.contains_key("feature/mod.rs"));
    }

    /// The temp-then-rename discipline, in the direction where it matters
    /// most: the target is the copy that survives `destroy`, so nothing may
    /// ever observe it half-written — and the temp is cleared either way.
    #[tokio::test]
    async fn a_pull_that_fails_leaves_the_canonical_copy_untouched() {
        let dir = workspace(&[("a.txt", "host version")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        guest.file("/src/a.txt", "guest version", 4_242);
        guest.fail_pull("/src/a.txt");
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.to_host.placed, 0);
        assert_eq!(done.failures.len(), 1);
        assert_eq!(done.failures[0].path, "a.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "host version",
            "the canonical copy was written before the transfer finished"
        );
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(TEMP_PREFIX))
            .collect();
        assert!(left.is_empty(), "a temp was left behind: {left:?}");

        // Resume is re-transfer: the whole file again, from the start.
        let second = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(second.failures, vec![]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "guest version"
        );
    }

    /// A guest-side delete propagates immediately — the guard §19.6 puts on
    /// this direction is about *mass*, and lands with the halt.
    #[tokio::test]
    async fn a_guest_side_delete_removes_the_host_copy_and_the_agreement() {
        let dir = workspace(&[("a.txt", "one"), ("b.txt", "two")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        guest.unlink("/src/a.txt");
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.to_host.removed, 1);
        assert!(!dir.path().join("a.txt").exists());
        assert!(!ledger.entries.contains_key("a.txt"));
        assert!(dir.path().join("b.txt").exists());
    }

    /// A guest-side symlink crosses verbatim in this direction too, and its
    /// target is never translated — the same rule, the same way round.
    #[tokio::test]
    async fn a_guest_side_symlink_crosses_verbatim() {
        let dir = workspace(&[("keep.rs", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;

        guest.put(
            "/src/lib",
            super::super::guest::fake::Node::Symlink("C:\\Users\\dev\\lib".into()),
            5,
        );
        let done = pass_with(dir.path(), &guest, &mut ledger, &["lib"]).await;

        assert_eq!(done.failures, vec![]);
        let link = std::fs::read_link(dir.path().join("lib")).unwrap();
        assert_eq!(link.to_string_lossy(), "C:\\Users\\dev\\lib");
        assert_eq!(ledger.entries["lib"].kind, Kind::Symlink);
    }

    /// The guest dropping a directory the *host* still holds guest-owned
    /// content in leaves it standing, exactly as the mirror case does: a
    /// removal that can never succeed must not be retried forever.
    #[tokio::test]
    async fn a_host_directory_holding_guest_owned_content_is_left_standing() {
        let dir = workspace(&[(".gitignore", "target/\n"), ("pkg/app.js", "x")]);
        let guest = FakeGuest::new();
        let mut ledger = ledger_for(dir.path());
        pass(dir.path(), &guest, &mut ledger).await;
        // Guest-owned host-side: the syncer has never touched it and never
        // will, but it is still in the way of the directory's removal.
        std::fs::create_dir_all(dir.path().join("pkg/target")).unwrap();
        std::fs::write(dir.path().join("pkg/target/out"), "built").unwrap();

        guest.unlink("/src/pkg/app.js");
        guest.unlink("/src/pkg");
        let done = pass(dir.path(), &guest, &mut ledger).await;

        assert_eq!(done.failures, vec![]);
        assert_eq!(done.left_standing, vec!["pkg".to_string()]);
        assert!(!dir.path().join("pkg/app.js").exists());
        assert!(dir.path().join("pkg/target/out").exists());
        assert!(!ledger.entries.contains_key("pkg"));

        // The removal is not retried. The host does still hold the directory,
        // so the ordinary one-side-changed rule puts it back in the guest —
        // and then both sides agree and nothing moves again.
        let again = pass(dir.path(), &guest, &mut ledger).await;
        assert_eq!(
            again.to_host.removed, 0,
            "the impossible removal was retried"
        );
        assert_eq!(again.to_guest.placed, 1);
        assert_eq!(
            pass(dir.path(), &guest, &mut ledger).await,
            Applied::default()
        );
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
