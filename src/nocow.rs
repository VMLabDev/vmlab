//! NOCOW for machine disk directories on btrfs (PRD §7.1).
//!
//! A qcow2 on a copy-on-write btrfs fragments with every guest write and
//! every online snapshot, which writes the guest's RAM into the same file:
//! tens of thousands of extents, and snapshots that slow to megabytes a
//! second. Setting `FS_NOCOW_FL` (`chattr +C`) on the directory before its
//! disks exist makes every file created in it afterwards NOCOW — and that is
//! the only moment it can: btrfs ignores the flag on a file that already has
//! data, and vmlab never touches an existing file's attributes. NOCOW also
//! turns off btrfs's data checksums and compression for those files.
//!
//! The decision is a value ([`plan`], ADR-0003) over a filesystem probe, so
//! it is tested without a btrfs; [`apply`] carries it out. The host config's
//! `btrfs_nocow = false` opts out.

use std::io;
use std::path::{Path, PathBuf};

/// `statfs.f_type` of a btrfs filesystem (`linux/magic.h`).
const BTRFS_SUPER_MAGIC: u32 = 0x9123_683E;

/// What the decision needs to know about the host's filesystems.
pub trait FsProbe {
    /// Whether `path` — or, when it does not exist yet, its nearest existing
    /// ancestor — lives on btrfs.
    fn is_btrfs(&self, path: &Path) -> io::Result<bool>;
    /// Whether the directory carries NOCOW; `None` when it does not exist.
    fn nocow(&self, dir: &Path) -> io::Result<Option<bool>>;
    /// The disk images (`*.qcow2`) already in `dir`, sorted; none when the
    /// directory does not exist.
    fn disks(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
}

/// What a machine's disk directory gets before its disks are created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Not on btrfs: nothing to do.
    NotBtrfs,
    /// On btrfs, but the host config says `btrfs_nocow = false`.
    OptedOut,
    /// The directory already carries NOCOW.
    AlreadySet,
    /// Create the directory if need be and set NOCOW on it. `existing` are
    /// disks already in it: they keep copy-on-write, since only a file
    /// created after the flag inherits it.
    Set { existing: Vec<PathBuf> },
}

/// Decide what `dir`, a machine's disk directory, gets. `enabled` is the
/// host config's `btrfs_nocow`, consulted only on btrfs, where it matters.
pub fn plan(probe: &impl FsProbe, dir: &Path, enabled: bool) -> io::Result<Plan> {
    if !probe.is_btrfs(dir)? {
        return Ok(Plan::NotBtrfs);
    }
    if !enabled {
        return Ok(Plan::OptedOut);
    }
    if probe.nocow(dir)? == Some(true) {
        return Ok(Plan::AlreadySet);
    }
    Ok(Plan::Set {
        existing: probe.disks(dir)?,
    })
}

impl Plan {
    /// The line `up` prints once it has set the flag (`done`), or the line
    /// `validate` prints for what `up` will do. `None` when there is nothing
    /// to say: off btrfs, opted out, or already set.
    pub fn describe(&self, machine: &str, dir: &Path, done: bool) -> Option<String> {
        let Plan::Set { existing } = self else {
            return None;
        };
        let verb = if done { "set" } else { "`up` sets" };
        let mut line = format!(
            "{machine}: {} is on btrfs: {verb} NOCOW (chattr +C) on it before creating disks, \
             so they stay unfragmented under online snapshots; btrfs does not checksum or \
             compress NOCOW files (`btrfs_nocow = false` in the host config opts out)",
            dir.display()
        );
        if !existing.is_empty() {
            let names: Vec<String> = existing
                .iter()
                .map(|p| {
                    p.file_name()
                        .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into())
                })
                .collect();
            line.push_str(&format!(
                "; {} already there keep{} copy-on-write — only files created after the flag \
                 inherit it",
                names.join(", "),
                if existing.len() == 1 { "s" } else { "" }
            ));
        }
        Some(line)
    }
}

/// Carry out `plan` on `dir`: create it and set NOCOW on the directory
/// alone. A no-op for every plan but [`Plan::Set`].
pub fn apply(plan: &Plan, dir: &Path) -> io::Result<()> {
    if !matches!(plan, Plan::Set { .. }) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let fd = open_dir(dir)?;
    let flags = rustix::fs::ioctl_getflags(&fd)?;
    rustix::fs::ioctl_setflags(&fd, flags | rustix::fs::IFlags::NOCOW)?;
    Ok(())
}

fn open_dir(dir: &Path) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    Ok(rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// The probe over the host's real filesystems.
pub struct HostFs;

impl FsProbe for HostFs {
    fn is_btrfs(&self, path: &Path) -> io::Result<bool> {
        let existing = path
            .ancestors()
            .find(|p| p.exists())
            .unwrap_or_else(|| Path::new("/"));
        let st = rustix::fs::statfs(existing)?;
        // `f_type` is a signed word whose width varies by arch; the magic
        // is the low 32 bits.
        Ok(st.f_type as u32 == BTRFS_SUPER_MAGIC)
    }

    fn nocow(&self, dir: &Path) -> io::Result<Option<bool>> {
        let fd = match open_dir(dir) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let flags = rustix::fs::ioctl_getflags(&fd)?;
        Ok(Some(flags.contains(rustix::fs::IFlags::NOCOW)))
    }

    fn disks(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut disks = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|x| x == "qcow2") && path.is_file() {
                disks.push(path);
            }
        }
        disks.sort();
        Ok(disks)
    }
}

/// The plan for `dir` under this host's config and filesystems, or the
/// warning line to say instead when the probe fails: NOCOW is a performance
/// measure, and the disks work without it, so nothing is refused over it.
pub fn decide_on_host(machine: &str, dir: &Path) -> Result<Plan, String> {
    let enabled = crate::config::host::HostConfig::load_default()
        .map(|h| h.btrfs_nocow)
        .unwrap_or(true);
    plan(&HostFs, dir, enabled).map_err(|e| {
        format!(
            "warning: {machine}: cannot tell whether {} needs NOCOW: {e}",
            dir.display()
        )
    })
}

/// Decide and apply NOCOW for a machine's disk directory, returning what to
/// say about it: the [`Plan::describe`] line once the flag is set, or a
/// `warning:` line when it could not be.
pub fn prepare(machine: &str, dir: &Path) -> Option<String> {
    let plan = match decide_on_host(machine, dir) {
        Ok(plan) => plan,
        Err(warning) => return Some(warning),
    };
    match apply(&plan, dir) {
        Ok(()) => plan.describe(machine, dir, true),
        Err(e) => Some(format!(
            "warning: {machine}: could not set NOCOW on {}: {e}; its disks stay copy-on-write",
            dir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A filesystem described up front: whether it is btrfs, which
    /// directories exist with what flag, and the disks in each.
    #[derive(Default)]
    struct FakeFs {
        btrfs: bool,
        dirs: BTreeMap<PathBuf, bool>,
        disks: BTreeMap<PathBuf, Vec<PathBuf>>,
    }

    impl FsProbe for FakeFs {
        fn is_btrfs(&self, _: &Path) -> io::Result<bool> {
            Ok(self.btrfs)
        }
        fn nocow(&self, dir: &Path) -> io::Result<Option<bool>> {
            Ok(self.dirs.get(dir).copied())
        }
        fn disks(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            Ok(self.disks.get(dir).cloned().unwrap_or_default())
        }
    }

    fn dir() -> PathBuf {
        PathBuf::from("/lab/.vmlab/vms/alp")
    }

    #[test]
    fn a_fresh_directory_on_btrfs_is_set() {
        let fs = FakeFs {
            btrfs: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&fs, &dir(), true).unwrap(),
            Plan::Set { existing: vec![] }
        );
    }

    #[test]
    fn off_btrfs_nothing_is_done_or_said() {
        let fs = FakeFs::default();
        let p = plan(&fs, &dir(), true).unwrap();
        assert_eq!(p, Plan::NotBtrfs);
        assert_eq!(p.describe("alp", &dir(), true), None);
    }

    #[test]
    fn the_host_config_opts_out() {
        let fs = FakeFs {
            btrfs: true,
            ..Default::default()
        };
        let p = plan(&fs, &dir(), false).unwrap();
        assert_eq!(p, Plan::OptedOut);
        assert_eq!(p.describe("alp", &dir(), true), None);
        assert!(apply(&p, Path::new("/nonexistent/never/created")).is_ok());
    }

    #[test]
    fn a_directory_already_nocow_is_left_alone_and_silent() {
        let mut fs = FakeFs {
            btrfs: true,
            ..Default::default()
        };
        fs.dirs.insert(dir(), true);
        let p = plan(&fs, &dir(), true).unwrap();
        assert_eq!(p, Plan::AlreadySet);
        assert_eq!(p.describe("alp", &dir(), true), None);
    }

    #[test]
    fn existing_disks_are_named_and_keep_copy_on_write() {
        let mut fs = FakeFs {
            btrfs: true,
            ..Default::default()
        };
        fs.dirs.insert(dir(), false);
        fs.disks.insert(
            dir(),
            vec![dir().join("disk0.qcow2"), dir().join("disk-data.qcow2")],
        );
        let p = plan(&fs, &dir(), true).unwrap();
        assert_eq!(
            p,
            Plan::Set {
                existing: vec![dir().join("disk0.qcow2"), dir().join("disk-data.qcow2")]
            }
        );
        let line = p.describe("alp", &dir(), true).unwrap();
        assert!(
            line.contains("disk0.qcow2, disk-data.qcow2 already there keep copy-on-write"),
            "{line}"
        );
    }

    #[test]
    fn the_line_says_what_it_costs_and_how_to_opt_out() {
        let p = Plan::Set { existing: vec![] };
        let done = p.describe("alp", &dir(), true).unwrap();
        assert!(done.starts_with("alp: /lab/.vmlab/vms/alp is on btrfs: set NOCOW"));
        assert!(done.contains("does not checksum or compress"));
        assert!(done.contains("`btrfs_nocow = false`"));
        assert!(!done.contains("already there"));
        let will = p.describe("alp", &dir(), false).unwrap();
        assert!(will.contains("`up` sets NOCOW"), "{will}");
    }

    #[test]
    fn apply_creates_the_directory_for_a_set_plan_only() {
        let tmp = tempfile::tempdir().unwrap();
        let skipped = tmp.path().join("skipped");
        apply(&Plan::AlreadySet, &skipped).unwrap();
        assert!(!skipped.exists());
        // Off btrfs the ioctl may be refused; the directory is made first
        // either way, and an existing file in it is never opened.
        let target = tmp.path().join("vms/alp");
        let _ = apply(&Plan::Set { existing: vec![] }, &target);
        assert!(target.is_dir());
    }

    #[test]
    fn the_host_probe_reads_a_real_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("disk0.qcow2"), b"").unwrap();
        std::fs::write(tmp.path().join("OVMF_VARS.fd"), b"").unwrap();
        assert_eq!(
            HostFs.disks(tmp.path()).unwrap(),
            vec![tmp.path().join("disk0.qcow2")]
        );
        assert_eq!(
            HostFs.disks(&tmp.path().join("absent")).unwrap(),
            Vec::<PathBuf>::new()
        );
        assert_eq!(HostFs.nocow(&tmp.path().join("absent")).unwrap(), None);
        // The nearest existing ancestor answers for a path not made yet.
        let missing = tmp.path().join("a/b/c");
        assert_eq!(
            HostFs.is_btrfs(&missing).unwrap(),
            HostFs.is_btrfs(tmp.path()).unwrap()
        );
    }
}
