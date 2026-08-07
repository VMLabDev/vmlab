//! `.git`'s mutable set, and the deferral a held lock buys it (PRD §19.6).
//!
//! **`.git` syncs bidirectionally**, and the deciding fact inverts the usual
//! reasoning: the guest can stay offline and for a domain lab usually will, so
//! the *host* is the side with network access and a host-side `git fetch` or
//! `pull` is a first-class operation rather than an edge case. Meanwhile
//! guest-side git is a **target workflow** — a coding agent working in the dev
//! machine commits, branches and diffs constantly and has no host shell to do
//! it from. Both sides run git, on purpose.
//!
//! The contention decomposes rather than needing a policy. `.git` is mostly
//! immutable and additive: loose objects are content-addressed and packfiles
//! and indexes are write-once, so that majority syncs freely because no two
//! writers ever produce different content at one path. What is left is the
//! small mutable set below, and the rules for it are: **never sync `*.lock`**
//! (the ignore floor's job), and **defer the mutable set while a lock is held
//! on either side**.
//!
//! **That deferral is timing, not a conflict rule** — which is the distinction
//! this module exists to keep. A deferred path is left exactly as it is, on
//! both sides, and the next pass reconsiders it; nothing is reported, nothing
//! needs resolving, and it clears itself when git lets go. **`.git` needs no
//! special conflict rule** beyond that: a whole-workspace halt has no
//! granularity to argue about, so the carve-out shrinks back to what it always
//! was.
//!
//! Running git on both sides at once remains a documented way to reach an
//! ordinary halt, which behaves correctly there — both copies survive.

use std::collections::BTreeSet;

/// The repository directory. Matched at the workspace root only, exactly as
/// the ignore floor's `.git/**/*.lock` is: a submodule's own `.git` is a
/// nested repository with its own everything, and quietly reaching into one
/// from a rule written for the outer repository is the kind of half-applied
/// semantics §19.6 keeps refusing.
const GIT: &str = ".git";

/// The mutable files: two writers *can* produce different content at each of
/// these, which is the whole of `.git`'s conflict surface.
const MUTABLE_FILES: &[&str] = &[
    "index",
    "HEAD",
    "ORIG_HEAD",
    "FETCH_HEAD",
    "packed-refs",
    "config",
];

/// The mutable subtrees, and their own directory entries.
const MUTABLE_DIRS: &[&str] = &["refs", "logs"];

/// Whether this path is one side's transient claim on the mutable set.
///
/// A lock file means nothing on the other side of the seam — it is a *local*
/// promise that one process is about to rewrite something — so it never syncs,
/// and its presence is the only thing the other side reads off it.
pub fn is_lock(rel: &str) -> bool {
    rel.starts_with(&format!("{GIT}/")) && rel.ends_with(".lock")
}

/// Whether this path is in the mutable set — the part of `.git` a held lock
/// defers.
///
/// Everything else under `.git` keeps syncing while a lock is held, which is
/// the point of decomposing rather than deferring the directory: a `git fetch`
/// host-side writes packfiles and loose objects that no guest-side commit can
/// disagree with, and stalling those would stall the workflow the bidirectional
/// `.git` exists for.
pub fn is_mutable(rel: &str) -> bool {
    let Some(rest) = rel.strip_prefix(&format!("{GIT}/")) else {
        return false;
    };
    MUTABLE_FILES.contains(&rest)
        || MUTABLE_DIRS
            .iter()
            .any(|dir| rest == *dir || rest.starts_with(&format!("{dir}/")))
}

/// Which of `paths` this pass leaves strictly alone, given the locks either
/// side is holding.
///
/// Deliberately the *whole* mutable set rather than the locked path's own
/// target: `index.lock` is taken while `index`, `HEAD` and half of `refs/` are
/// rewritten together, and syncing any one of them mid-operation would carry a
/// tree that never existed on either side.
pub fn deferred<'a>(
    locks: &BTreeSet<String>,
    paths: impl Iterator<Item = &'a String>,
) -> BTreeSet<String> {
    if locks.is_empty() {
        return BTreeSet::new();
    }
    paths
        .filter(|path| is_mutable(path))
        .cloned()
        .collect::<BTreeSet<String>>()
}

/// What the event feed says about it — naming the lock, because "some paths
/// are waiting" is the kind of quiet this design keeps ruling out.
pub fn why(locks: &BTreeSet<String>, deferred: usize) -> String {
    let held: Vec<&str> = locks.iter().take(4).map(String::as_str).collect();
    format!(
        "git is holding {} ({deferred} path(s) of .git's mutable set deferred until it lets go). \
         This is timing rather than a conflict: both copies stay exactly as they are and the next \
         pass reconsiders them",
        held.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    /// A lock is one side's transient claim, wherever under `.git` it is
    /// taken — `index.lock` at the top, a ref lock several levels down.
    #[test]
    fn a_lock_is_recognised_anywhere_under_git() {
        assert!(is_lock(".git/index.lock"));
        assert!(is_lock(".git/refs/heads/main.lock"));
        assert!(is_lock(".git/config.lock"));
        assert!(!is_lock(".git/index"));
        assert!(!is_lock("src/build.lock"), "not git's");
        assert!(!is_lock("Cargo.lock"), "a lock file that is content");
    }

    /// The mutable set is small and enumerable, and everything else under
    /// `.git` is content-addressed or write-once — so it keeps syncing while a
    /// lock is held.
    #[test]
    fn only_the_mutable_set_is_deferrable() {
        for mutable in [
            ".git/index",
            ".git/HEAD",
            ".git/packed-refs",
            ".git/config",
            ".git/refs",
            ".git/refs/heads/main",
            ".git/logs/HEAD",
        ] {
            assert!(is_mutable(mutable), "{mutable}");
        }
        for additive in [
            ".git/objects/ab/cdef",
            ".git/objects/pack/pack-1.pack",
            ".git/hooks/pre-commit",
            ".git",
            "src/main.rs",
        ] {
            assert!(!is_mutable(additive), "{additive}");
        }
    }

    /// With a lock held, the whole mutable set waits — not just the locked
    /// path's own target, because git rewrites several of them together and a
    /// half-carried set is a tree that never existed on either side.
    #[test]
    fn a_held_lock_defers_the_whole_mutable_set() {
        let paths = set(&[
            ".git/index",
            ".git/HEAD",
            ".git/refs/heads/main",
            ".git/objects/ab/cdef",
            "src/main.rs",
        ]);
        let deferred = deferred(&set(&[".git/index.lock"]), paths.iter());
        assert_eq!(
            deferred,
            set(&[".git/index", ".git/HEAD", ".git/refs/heads/main"]),
        );
    }

    /// No lock, no deferral: the steady state pays nothing for this rule.
    #[test]
    fn nothing_defers_while_no_lock_is_held() {
        assert!(deferred(&BTreeSet::new(), set(&[".git/index"]).iter()).is_empty());
    }

    /// It is named out loud, like everything else the syncer declines to do.
    #[test]
    fn the_deferral_names_the_lock_and_says_it_clears_itself() {
        let said = why(&set(&[".git/index.lock"]), 3);
        assert!(said.contains(".git/index.lock"), "{said}");
        assert!(said.contains("rather than a conflict"), "{said}");
    }
}
