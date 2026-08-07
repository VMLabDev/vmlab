//! The **sync ledger**: what the two sides last agreed on (PRD §19.6).
//!
//! One record per relative path, carrying a content digest plus **each side's
//! own** `(size, mtime)` as a change-detector. Four properties, each of which
//! is load-bearing rather than a design preference:
//!
//! - **Host-side only.** A guest-held copy is exactly the surviving guest-side
//!   state ADR-0014 retired the workspace disk to eliminate, and it can
//!   disagree with the host's.
//! - **Never compare a host mtime to a guest mtime.** Each side's mtime is
//!   compared only against its own recorded value — which is why [`Side`]
//!   exists twice in an [`Agreed`] rather than once. A restored guest resumes
//!   with a clock *behind* the host, so every file it holds would look older,
//!   and that disqualifies `newest-wins` outright, before taste enters.
//! - **Digest is the truth; `(size, mtime)` is a pre-filter.** A same-size
//!   in-place write is exactly the case the share transports were caught
//!   missing, so [`Side::unchanged`] may only ever *skip* a digest, never
//!   decide a disagreement.
//! - **A missing ledger is not a decision.** Loading one that is absent gives
//!   an empty ledger, and [`plan`](super::plan) adopts matching digests as
//!   agreed and sends differing paths down the ordinary conflict path. "No
//!   ledger means blind host→guest seed" is the version that eats a
//!   developer's work the one time they deleted `.vmlab/` to fix something
//!   else.
//!
//! It lives in the lab's `.vmlab/`, per (machine, workspace), so `destroy`
//! wipes it: the guest tree is gone and there is nothing left to have agreed
//! with.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use vmlab_agent_proto::watch::EntryKind;

/// On-disk format version. A ledger written by a different one is discarded
/// rather than guessed at — an empty ledger is a case this design already
/// handles safely, where a misread one is not.
const VERSION: u32 = 1;

/// What the ledger can hold agreement about. Deliberately narrower than the
/// wire's [`EntryKind`]: **special files never enter the ledger** (§19.6), so
/// they cannot produce a phantom conflict, and the type says so rather than a
/// comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Dir,
    /// Synced verbatim and never followed. Its digest is of the **target
    /// string**, which is content like any other and is never translated
    /// across the seam.
    Symlink,
}

impl Kind {
    /// The ledger's reading of a wire kind. `None` for a socket, FIFO, device
    /// node or non-symlink reparse point — skipped loudly, never recorded.
    pub fn of(kind: EntryKind) -> Option<Kind> {
        match kind {
            EntryKind::File => Some(Kind::File),
            EntryKind::Dir => Some(Kind::Dir),
            EntryKind::Symlink => Some(Kind::Symlink),
            EntryKind::Other => None,
        }
    }
}

/// One side's own change-detector for one path. Compared **only** against the
/// same side's current state; the two sides' clocks are not comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Side {
    pub size: u64,
    pub mtime_ns: i64,
}

impl Side {
    pub fn new(size: u64, mtime_ns: i64) -> Side {
        Side { size, mtime_ns }
    }

    /// This side looks untouched since the agreement.
    ///
    /// A **pre-filter, not a verdict**: `true` licenses skipping the digest,
    /// and `false` only means "go and hash it". Nothing may conclude a
    /// disagreement from this, because a same-size in-place write compares
    /// equal on both fields.
    pub fn unchanged(self, size: u64, mtime_ns: i64) -> bool {
        self.size == size && self.mtime_ns == mtime_ns
    }
}

/// One path's agreement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agreed {
    pub kind: Kind,
    /// SHA-256 of the content both sides held — of the target string for a
    /// symlink, and empty for a directory, which has no content to agree on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    pub host: Side,
    pub guest: Side,
}

/// Every agreement for one (machine, workspace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    version: u32,
    /// The two roots this ledger is *about*. A `@dev(workspace = …)` edit
    /// points the machine at a different tree, whose paths have never been
    /// agreed with anything — so the ledger is discarded rather than
    /// reinterpreted against a tree it never saw.
    host_root: String,
    guest_root: String,
    /// The ignore rules' own digest (§19.6). They live *in* the tree and are
    /// developer-owned, so they change under the syncer; carrying their
    /// digest is what lets a later halt say *these conflict because you just
    /// changed the rules*.
    #[serde(default)]
    pub ignore_digest: String,
    /// The **prune list** last computed from those rules (§19.6) — the
    /// directory prefixes the guest registers no watcher under.
    ///
    /// Remembered rather than recomputed on demand because of an ordering
    /// fact: it has to be known *before* the watch opens, and the walk that
    /// computes it happens inside a pass. Without it a restart would open the
    /// watch on nothing and register a dependency tree it already knows to
    /// skip — on Linux, one watch descriptor per directory of it.
    #[serde(default)]
    pub prune: Vec<String>,
    /// Keyed by `/`-separated path relative to the workspace root.
    pub entries: BTreeMap<String, Agreed>,
}

impl Ledger {
    /// An empty ledger for one workspace — what a first run, and a wiped
    /// `.vmlab/`, both start from.
    pub fn new(host_root: &Path, guest_root: &str) -> Ledger {
        Ledger {
            version: VERSION,
            host_root: host_root.display().to_string(),
            guest_root: guest_root.to_string(),
            ignore_digest: String::new(),
            prune: Vec::new(),
            entries: BTreeMap::new(),
        }
    }

    /// Where one machine's ledger lives under the lab's `.vmlab/`. `destroy`
    /// removes that directory wholesale, which is the whole of the ledger's
    /// lifecycle.
    pub fn path(lab_local: &Path, machine: &str) -> PathBuf {
        lab_local.join("workspace").join(format!("{machine}.json"))
    }

    /// Read the ledger, or start empty.
    ///
    /// Absent, unreadable, from another format version, or about another
    /// workspace all give the same answer — an empty ledger — because that is
    /// a case the reconciliation already handles safely by inspecting the
    /// guest. Anything cleverer would be guessing at agreement.
    pub fn load(path: &Path, host_root: &Path, guest_root: &str) -> Ledger {
        let fresh = Ledger::new(host_root, guest_root);
        let Ok(bytes) = std::fs::read(path) else {
            return fresh;
        };
        match serde_json::from_slice::<Ledger>(&bytes) {
            Ok(ledger)
                if ledger.version == VERSION
                    && ledger.host_root == fresh.host_root
                    && ledger.guest_root == fresh.guest_root =>
            {
                ledger
            }
            Ok(_) => fresh,
            Err(e) => {
                tracing::warn!("workspace ledger {} is unreadable: {e}", path.display());
                fresh
            }
        }
    }

    /// Write the ledger where a crash cannot leave a torn one: same directory,
    /// temp then rename — the discipline every apply follows, for the same
    /// reason.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let temp = dir.join(format!(
            "{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agreed() -> Agreed {
        Agreed {
            kind: Kind::File,
            digest: "ab".repeat(32),
            host: Side::new(12, 1_700_000_000_000_000_000),
            guest: Side::new(12, 1_600_000_000_000_000_000),
        }
    }

    fn saved(dir: &Path) -> (PathBuf, Ledger) {
        let path = Ledger::path(dir, "dev01");
        let mut ledger = Ledger::new(Path::new("/lab/src"), "/src");
        ledger.entries.insert("src/main.rs".into(), agreed());
        ledger.save(&path).unwrap();
        (path, ledger)
    }

    #[test]
    fn a_ledger_round_trips_through_the_lab_local_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (path, want) = saved(dir.path());
        assert_eq!(Ledger::load(&path, Path::new("/lab/src"), "/src"), want);
    }

    /// A missing ledger is not a decision — it is an empty one, which the
    /// reconciliation resolves by inspecting the guest.
    #[test]
    fn a_missing_ledger_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::load(
            &Ledger::path(dir.path(), "never-synced"),
            Path::new("/lab/src"),
            "/src",
        );
        assert!(ledger.entries.is_empty());
    }

    /// The two sides' own records survive separately. Folding them into one
    /// would be the newest-wins bug in storage form.
    #[test]
    fn each_side_keeps_its_own_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = saved(dir.path());
        let got = Ledger::load(&path, Path::new("/lab/src"), "/src");
        let entry = &got.entries["src/main.rs"];
        assert_eq!(entry.host.mtime_ns, 1_700_000_000_000_000_000);
        assert_eq!(entry.guest.mtime_ns, 1_600_000_000_000_000_000);
    }

    /// Pointing `@dev(workspace)` at another tree means nothing here was ever
    /// agreed about it.
    #[test]
    fn a_ledger_about_another_workspace_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = saved(dir.path());
        assert!(
            Ledger::load(&path, Path::new("/lab/other"), "/src")
                .entries
                .is_empty()
        );
        assert!(
            Ledger::load(&path, Path::new("/lab/src"), "C:\\src")
                .entries
                .is_empty()
        );
    }

    #[test]
    fn a_corrupt_ledger_starts_over_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let path = Ledger::path(dir.path(), "dev01");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(
            Ledger::load(&path, Path::new("/lab/src"), "/src")
                .entries
                .is_empty()
        );
    }

    /// The pre-filter may skip a digest and nothing more. A same-size
    /// in-place write is what the share transports were caught missing, so a
    /// changed mtime is the only thing it can notice.
    #[test]
    fn the_pre_filter_only_ever_licenses_skipping_a_digest() {
        let side = Side::new(12, 100);
        assert!(side.unchanged(12, 100));
        assert!(!side.unchanged(12, 101));
        assert!(!side.unchanged(13, 100));
    }

    /// A socket, FIFO or device node has no agreement to record, so it cannot
    /// reach the ledger to produce a phantom conflict later.
    #[test]
    fn a_special_file_has_no_ledger_kind() {
        assert_eq!(Kind::of(EntryKind::File), Some(Kind::File));
        assert_eq!(Kind::of(EntryKind::Dir), Some(Kind::Dir));
        assert_eq!(Kind::of(EntryKind::Symlink), Some(Kind::Symlink));
        assert_eq!(Kind::of(EntryKind::Other), None);
    }
}
