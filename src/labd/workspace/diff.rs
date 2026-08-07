//! `vmlab dev sync diff` — the guest's copy, brought host-side (PRD §19.6).
//!
//! **Only one of the two copies is behind the seam.** The host copy is a plain
//! directory on the developer's own workstation, so inspecting it is `cd`
//! rather than a remote operation; the guest copy is the one that needs a
//! channel. That asymmetry is the whole justification for this verb — without
//! it, a developer resolving a halt has to attach a *second* time just to read
//! the version they are being asked about.
//!
//! Both copies come back together anyway, read by the daemon, because the
//! daemon is the one thing that knows a workspace's two roots without being
//! told. A client doing that arithmetic would be one `@dev(workspace = …)` edit
//! away from diffing the wrong tree.
//!
//! Nothing here changes either side. A halt writes neither copy and deletes
//! neither, and reading them must not either — so the guest's bytes land in a
//! scratch file the daemon owns and throws away.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::guest::GuestFs;
use super::scan::join_guest;
use super::syncer::Workspace;

/// The most either side may carry inline.
///
/// A diff is for reading, and nothing a person reads is larger than this. Past
/// it the sides are still *described* — size and digest, which is what answers
/// "are these the same file" — and the bytes are declined by name, because
/// moving a gigabyte onto a terminal helps nobody and `vmlab cp` already
/// exists for wanting the file itself.
const INLINE: u64 = 4 << 20;

/// Every path one `dev sync diff` asked about, and the two roots they are
/// relative to.
///
/// A **typed** reply rather than a hand-built JSON object, and the reason is
/// ADR-0004's: the CLI deserialises this very type, so a field renamed here
/// stops the renderer compiling instead of quietly becoming "no guest copy".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub machine: String,
    /// The canonical directory, as a path on this host — where the developer
    /// can simply go and look.
    pub host_root: String,
    /// The working copy's root inside the guest, which they cannot.
    pub guest_root: String,
    pub files: Vec<Sides>,
}

/// One workspace path, from both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sides {
    pub path: String,
    pub host: Option<SideCopy>,
    pub guest: Option<SideCopy>,
    /// The two copies hold the same bytes. Worth saying outright: it is the
    /// state a developer reaches by resolving a halt **by hand**, and the one
    /// the next pass adopts as agreed.
    pub identical: bool,
}

/// What one side holds — or why it could not be read, which is a different
/// thing from holding nothing and must never be reported as one.
///
/// `SideCopy` rather than the domain's own word, which is `copy`: a type named
/// `Copy` shadows `std::marker::Copy` for everything in this module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideCopy {
    pub size: u64,
    pub digest: String,
    /// The content, where it is text and within the cap.
    pub text: Option<String>,
    /// Why the content is not here: too large, not text, or unreadable.
    pub omitted: Option<String>,
}

impl SideCopy {
    fn of(bytes: &[u8]) -> SideCopy {
        use sha2::{Digest as _, Sha256};
        let digest = hex::encode(Sha256::digest(bytes));
        let size = bytes.len() as u64;
        // A NUL byte is the same test `git diff` uses, and for the same
        // reason: what makes a file undiffable is not its encoding but that
        // rendering it would scribble on the terminal.
        let text = match std::str::from_utf8(bytes) {
            Ok(text) if !bytes.contains(&0) => Some(text.to_string()),
            _ => None,
        };
        SideCopy {
            size,
            digest,
            omitted: text.is_none().then(|| "it is not text".to_string()),
            text,
        }
    }

    fn unreadable(why: String) -> SideCopy {
        SideCopy {
            size: 0,
            digest: String::new(),
            text: None,
            omitted: Some(why),
        }
    }

    fn too_large(size: u64) -> SideCopy {
        SideCopy {
            size,
            digest: String::new(),
            text: None,
            omitted: Some(format!(
                "it is {size} bytes, over the {INLINE}-byte inline cap — `vmlab cp` moves the file \
                 itself"
            )),
        }
    }
}

/// Read every path from both sides, as one reply.
///
/// The scratch directory the guest's copies are pulled through is this
/// function's own, and goes when it returns: a diff is a read, and reading must
/// leave nothing behind on either side any more than a halt does.
pub async fn all(guest: &dyn GuestFs, workspace: &Workspace, paths: &[String]) -> Result<Diff> {
    let scratch = tempfile::tempdir().context("a host scratch directory for the guest copy")?;
    let mut files = Vec::with_capacity(paths.len());
    for (n, path) in paths.iter().enumerate() {
        files.push(one(guest, workspace, path, &scratch.path().join(n.to_string())).await);
    }
    Ok(Diff {
        machine: workspace.machine.clone(),
        host_root: workspace.host_root.display().to_string(),
        guest_root: workspace.guest_root.clone(),
        files,
    })
}

/// Read one path from both sides. `scratch` is a host path the guest's copy is
/// pulled to and the caller throws away.
pub async fn one(guest: &dyn GuestFs, workspace: &Workspace, rel: &str, scratch: &Path) -> Sides {
    let host = host_side(&workspace.host_root.join(rel));
    let guest = guest_side(guest, &join_guest(&workspace.guest_root, rel), scratch).await;
    let identical = match (&host, &guest) {
        (Some(a), Some(b)) => !a.digest.is_empty() && a.digest == b.digest,
        // Absent on both sides is not "the same": there is nothing to be the
        // same about, and saying otherwise would read as a resolved conflict.
        _ => false,
    };
    Sides {
        path: rel.to_string(),
        host,
        guest,
        identical,
    }
}

fn host_side(path: &Path) -> Option<SideCopy> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(SideCopy::unreadable(format!("host: cannot read it ({e})"))),
    };
    if meta.is_dir() {
        return Some(SideCopy::unreadable("host: it is a directory".into()));
    }
    if meta.is_symlink() {
        return Some(match std::fs::read_link(path) {
            Ok(target) => SideCopy::of(target.to_string_lossy().as_bytes()),
            Err(e) => SideCopy::unreadable(format!("host: cannot read the link target ({e})")),
        });
    }
    if meta.len() > INLINE {
        return Some(SideCopy::too_large(meta.len()));
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(SideCopy::of(&bytes)),
        Err(e) => Some(SideCopy::unreadable(format!("host: cannot read it ({e})"))),
    }
}

async fn guest_side(guest: &dyn GuestFs, path: &str, scratch: &Path) -> Option<SideCopy> {
    let attrs = match guest.lstat(path).await {
        Ok(Some(attrs)) => attrs,
        Ok(None) => return None,
        Err(e) => {
            return Some(SideCopy::unreadable(format!(
                "guest: cannot read it ({e:#})"
            )));
        }
    };
    use crate::labd::vm_agent::EntryKind;
    match attrs.kind {
        EntryKind::Dir => return Some(SideCopy::unreadable("guest: it is a directory".into())),
        EntryKind::Other => {
            return Some(SideCopy::unreadable(
                "guest: not a file, directory or symlink".into(),
            ));
        }
        EntryKind::Symlink => {
            return Some(match guest.readlink(path).await {
                Ok(target) => SideCopy::of(target.as_bytes()),
                Err(e) => {
                    SideCopy::unreadable(format!("guest: cannot read the link target ({e:#})"))
                }
            });
        }
        EntryKind::File => {}
    }
    if attrs.size > INLINE {
        return Some(SideCopy::too_large(attrs.size));
    }
    if let Err(e) = guest.pull(path, scratch).await {
        return Some(SideCopy::unreadable(format!(
            "guest: cannot read it ({e:#})"
        )));
    }
    Some(match std::fs::read(scratch) {
        Ok(bytes) => SideCopy::of(&bytes),
        Err(e) => SideCopy::unreadable(format!("guest: the pulled copy could not be read ({e})")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::workspace::guest::fake::FakeGuest;
    use crate::labd::workspace::windows::Preconditions;
    use std::sync::Arc;

    fn workspace(root: &Path) -> Workspace {
        Workspace {
            machine: "dev01".into(),
            host_root: root.to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: root.join(".vmlab/workspace/dev01.json"),
            max_file_bytes: 1 << 30,
            preconditions: Preconditions::default(),
        }
    }

    /// The point of the verb: the copy behind the seam comes to the host, and
    /// the copy that was never behind it comes along so a developer sees both
    /// without doing any path arithmetic.
    #[tokio::test]
    async fn both_copies_come_back_and_the_guest_side_crosses_the_seam() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "the host's version").unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/main.rs", "the guest's version", 1);

        let sides = one(
            &guest,
            &workspace(dir.path()),
            "main.rs",
            &scratch.path().join("0"),
        )
        .await;
        assert_eq!(
            sides.host.unwrap().text.as_deref(),
            Some("the host's version")
        );
        assert_eq!(
            sides.guest.unwrap().text.as_deref(),
            Some("the guest's version")
        );
        assert!(!sides.identical);
    }

    /// Reading changes neither copy — a halt writes neither and deletes
    /// neither, and looking at one must not be the exception.
    #[tokio::test]
    async fn a_diff_writes_to_neither_side() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "host").unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/a.rs", "guest", 1);

        let before = guest.paths();
        one(
            &guest,
            &workspace(dir.path()),
            "a.rs",
            &scratch.path().join("0"),
        )
        .await;
        assert_eq!(guest.paths(), before);
        assert!(guest.writes().is_empty(), "{:?}", guest.writes());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "host"
        );
    }

    /// Matching content is said outright: it is the state resolving a halt by
    /// hand reaches, and the one the next pass adopts as agreed.
    #[tokio::test]
    async fn identical_copies_are_reported_as_identical() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "settled").unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/a.rs", "settled", 1);
        let sides = one(
            &guest,
            &workspace(dir.path()),
            "a.rs",
            &scratch.path().join("0"),
        )
        .await;
        assert!(sides.identical);
    }

    /// One side deleted it, which is half of the conflict this verb is most
    /// often called about — and absence must read as absence rather than as an
    /// empty file.
    #[tokio::test]
    async fn a_side_that_deleted_the_path_reports_nothing_rather_than_nothing_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "still here").unwrap();
        let guest = Arc::new(FakeGuest::new());
        let sides = one(
            &guest,
            &workspace(dir.path()),
            "a.rs",
            &scratch.path().join("0"),
        )
        .await;
        assert!(sides.guest.is_none());
        assert!(sides.host.is_some());
        assert!(!sides.identical, "nothing to be identical about");
    }

    /// A path that cannot be read is described rather than reported absent —
    /// "I could not look" and "nothing is there" are opposite answers.
    #[tokio::test]
    async fn an_unreadable_guest_copy_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/a.rs", "root-owned", 1);
        guest.unreadable("/src/a.rs");
        let sides = one(
            &guest,
            &workspace(dir.path()),
            "a.rs",
            &scratch.path().join("0"),
        )
        .await;
        let copy = sides.guest.expect("absence is not the answer");
        assert!(copy.text.is_none());
        assert!(copy.omitted.unwrap().contains("cannot read"));
    }

    /// Bytes nobody would read are declined by name, and the two sides are
    /// still compared by the thing that answers the question.
    #[tokio::test]
    async fn an_undiffable_copy_is_described_rather_than_dumped() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), [0u8, 1, 2, 3]).unwrap();
        let guest = Arc::new(FakeGuest::new());
        let sides = one(
            &guest,
            &workspace(dir.path()),
            "a.bin",
            &scratch.path().join("0"),
        )
        .await;
        let copy = sides.host.expect("it is there");
        assert!(copy.text.is_none());
        assert_eq!(copy.omitted.as_deref(), Some("it is not text"));
        assert_eq!(copy.size, 4);
        assert!(!copy.digest.is_empty(), "still comparable");
    }
}
