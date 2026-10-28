//! Worktree labs (PRD §21, ADR-0019): the main checkout's lab frozen at an
//! offline snapshot, and the linked git worktrees whose labs clone from it.
//!
//! This module holds what every side of the feature shares: the on-disk
//! layout of a freeze, the `.vmlab/worktree.json` record, and how a lab root
//! finds its main checkout or its linked worktrees. Everything is read
//! straight from the filesystem, without the `git` binary, as
//! [`crate::instance::worktree_name`] does.
//!
//! Layout, under the main checkout's working data ([`lab_local_dir`]):
//!
//! ```text
//! .vmlab/worktree.json                       newest generation + snapshot
//! .vmlab/freeze/<n>/<machine>/disk0.qcow2    frozen disks, read-only
//! .vmlab/freeze/<n>/<machine>/disk-<x>.qcow2
//! .vmlab/freeze/<n>/<machine>/scratch.qcow2  a container's writable layer
//! .vmlab/freeze/<n>/<machine>/OVMF_VARS.fd   copied, not frozen
//! .vmlab/freeze/<n>/<machine>/tpm-state/
//! .vmlab/freeze/<n>/<machine>/state.json     the machine's state record
//! .vmlab/freeze/<n>/volumes/<name>/          the lab's named volumes
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::lab_local_dir;

/// The record a freeze writes, beside the lab's other working data.
pub const CONFIG_FILE: &str = "worktree.json";

/// The directory holding every generation.
pub const FREEZE_DIR: &str = "freeze";

/// A machine's state record, copied into its generation directory.
pub const MACHINE_STATE_FILE: &str = "state.json";

/// The named volumes, copied into a generation directory.
pub const VOLUMES_DIR: &str = "volumes";

/// `.vmlab/worktree.json`: the newest generation and the snapshot it was
/// frozen at. Its presence turns on worktree isolation (§21.4) for the main
/// lab and every worktree lab of the family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeConfig {
    /// The newest generation, `1` for the first freeze.
    pub generation: u32,
    /// The offline snapshot the newest generation was frozen at. It is the
    /// freeze point every lab of the family can restore (§21.3).
    pub snapshot: String,
}

impl WorktreeConfig {
    /// The record in the working data `lab_local`, or `None` when the lab is
    /// not frozen.
    pub fn load(lab_local: &Path) -> Result<Option<Self>> {
        let path = lab_local.join(CONFIG_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write the record into `lab_local`, through a temporary file and a
    /// rename so a reader never sees half of it.
    pub fn save(&self, lab_local: &Path) -> Result<()> {
        std::fs::create_dir_all(lab_local)
            .with_context(|| format!("creating {}", lab_local.display()))?;
        let path = lab_local.join(CONFIG_FILE);
        let tmp = lab_local.join(format!("{CONFIG_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// Remove the record from `lab_local`. Missing is not an error.
    pub fn remove(lab_local: &Path) -> Result<()> {
        let path = lab_local.join(CONFIG_FILE);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// `.vmlab/freeze/` in the working data `lab_local`.
pub fn freeze_root(lab_local: &Path) -> PathBuf {
    lab_local.join(FREEZE_DIR)
}

/// `.vmlab/freeze/<n>/`.
pub fn generation_dir(lab_local: &Path, generation: u32) -> PathBuf {
    freeze_root(lab_local).join(generation.to_string())
}

/// `.vmlab/freeze/<n>/<machine>/`.
pub fn machine_dir(lab_local: &Path, generation: u32, machine: &str) -> PathBuf {
    generation_dir(lab_local, generation).join(machine)
}

/// `.vmlab/freeze/<n>/volumes/`.
pub fn volumes_dir(lab_local: &Path, generation: u32) -> PathBuf {
    generation_dir(lab_local, generation).join(VOLUMES_DIR)
}

/// The generations present under `.vmlab/freeze/`, oldest first.
pub fn generations(lab_local: &Path) -> Vec<u32> {
    let mut out: Vec<u32> = std::fs::read_dir(freeze_root(lab_local))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    out.sort_unstable();
    out
}

/// The top of the git worktree `root` is in, and what its `.git` is.
enum Checkout {
    /// A main worktree: `.git` is a directory.
    Main { top: PathBuf, git_dir: PathBuf },
    /// A linked worktree: `.git` is a file naming `<common>/worktrees/<name>`.
    Linked { top: PathBuf, common_dir: PathBuf },
}

fn checkout(root: &Path) -> Option<Checkout> {
    for dir in root.ancestors() {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(Checkout::Main {
                top: dir.to_path_buf(),
                git_dir: dotgit,
            });
        }
        if dotgit.is_file() {
            let text = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir = PathBuf::from(text.lines().next()?.strip_prefix("gitdir:")?.trim());
            let gitdir = if gitdir.is_absolute() {
                gitdir
            } else {
                dir.join(gitdir)
            };
            if gitdir.parent()?.file_name()? != "worktrees" {
                return None;
            }
            // `<gitdir>/commondir` names the shared git directory, usually
            // `../..`; without it the layout is `<common>/worktrees/<name>`.
            let common_dir = match std::fs::read_to_string(gitdir.join("commondir")) {
                Ok(c) => {
                    let c = PathBuf::from(c.trim());
                    if c.is_absolute() { c } else { gitdir.join(c) }
                }
                Err(_) => gitdir.parent()?.parent()?.to_path_buf(),
            };
            return Some(Checkout::Linked {
                top: dir.to_path_buf(),
                common_dir: common_dir.canonicalize().unwrap_or(common_dir),
            });
        }
    }
    None
}

/// The lab root in the main checkout that corresponds to `root`, when `root`
/// is in a linked worktree. `None` in a main worktree, a submodule, a bare
/// repository's worktree, or outside git.
///
/// The lab sits at the same path relative to the top of each checkout, so a
/// lab in `<worktree>/labs/demo` maps to `<main>/labs/demo`.
pub fn main_lab_root(root: &Path) -> Option<PathBuf> {
    let Checkout::Linked { top, common_dir } = checkout(root)? else {
        return None;
    };
    if common_dir.file_name()? != ".git" {
        return None;
    }
    let main_top = common_dir.parent()?;
    let rel = root.strip_prefix(&top).ok()?;
    Some(main_top.join(rel))
}

/// The lab roots in every linked worktree of the repository whose main
/// checkout holds the lab at `main_root`. Empty when `main_root` is not in a
/// main worktree. A worktree whose directory is gone is skipped.
pub fn worktree_lab_roots(main_root: &Path) -> Vec<PathBuf> {
    let Some(Checkout::Main { top, git_dir }) = checkout(main_root) else {
        return Vec::new();
    };
    let Ok(rel) = main_root.strip_prefix(&top) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = std::fs::read_dir(git_dir.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            // `<common>/worktrees/<name>/gitdir` names `<worktree>/.git`.
            let text = std::fs::read_to_string(e.path().join("gitdir")).ok()?;
            let wt_top = PathBuf::from(text.trim()).parent()?.to_path_buf();
            wt_top.is_dir().then(|| wt_top.join(rel))
        })
        .collect();
    out.sort();
    out
}

/// Where a lab root stands in a worktree family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Family {
    /// The main checkout's lab, frozen.
    FrozenMain { config: WorktreeConfig },
    /// A linked worktree's lab whose main checkout is frozen.
    Worktree {
        main_root: PathBuf,
        main_lab_local: PathBuf,
        config: WorktreeConfig,
    },
}

impl Family {
    /// The family the lab at `root` belongs to, or `None` when no freeze
    /// applies to it: the main checkout is not frozen, or `root` is not in a
    /// git checkout at all.
    pub fn of(root: &Path) -> Result<Option<Self>> {
        if let Some(main_root) = main_lab_root(root) {
            let main_lab_local = lab_local_dir(&main_root);
            return Ok(
                WorktreeConfig::load(&main_lab_local)?.map(|config| Family::Worktree {
                    main_root,
                    main_lab_local,
                    config,
                }),
            );
        }
        Ok(WorktreeConfig::load(&lab_local_dir(root))?.map(|config| Family::FrozenMain { config }))
    }

    pub fn config(&self) -> &WorktreeConfig {
        match self {
            Family::FrozenMain { config } | Family::Worktree { config, .. } => config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A main checkout at `<tmp>/main` with a lab in `labs/demo`, and a
    /// linked worktree at `<tmp>/wt/feat-x`, laid out as `git worktree add`
    /// leaves them.
    fn repo(tmp: &Path) -> (PathBuf, PathBuf) {
        let main = tmp.join("main");
        let common = main.join(".git");
        let wt_git = common.join("worktrees").join("feat-x");
        let wt = tmp.join("wt").join("feat-x");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::create_dir_all(main.join("labs/demo")).unwrap();
        std::fs::create_dir_all(wt.join("labs/demo")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            wt_git.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        (
            main.canonicalize().unwrap().join("labs/demo"),
            wt.join("labs/demo"),
        )
    }

    #[test]
    fn worktree_finds_its_main_lab_root() {
        let tmp = tempdir();
        let (main, wt) = repo(tmp.path());
        assert_eq!(main_lab_root(&wt), Some(main.clone()));
        assert_eq!(main_lab_root(&main), None);
    }

    #[test]
    fn main_lists_its_worktrees() {
        let tmp = tempdir();
        let (main, wt) = repo(tmp.path());
        assert_eq!(worktree_lab_roots(&main), vec![wt.clone()]);
        assert!(worktree_lab_roots(&wt).is_empty());
    }

    #[test]
    fn gone_worktree_is_skipped() {
        let tmp = tempdir();
        let (main, wt) = repo(tmp.path());
        std::fs::remove_dir_all(wt.parent().unwrap().parent().unwrap()).unwrap();
        assert!(worktree_lab_roots(&main).is_empty());
    }

    #[test]
    fn outside_git_has_no_family() {
        let tmp = tempdir();
        assert_eq!(main_lab_root(tmp.path()), None);
        assert!(worktree_lab_roots(tmp.path()).is_empty());
    }

    #[test]
    fn config_round_trips_and_removes() {
        let tmp = tempdir();
        assert_eq!(WorktreeConfig::load(tmp.path()).unwrap(), None);
        let c = WorktreeConfig {
            generation: 2,
            snapshot: "base".into(),
        };
        c.save(tmp.path()).unwrap();
        assert_eq!(WorktreeConfig::load(tmp.path()).unwrap(), Some(c));
        WorktreeConfig::remove(tmp.path()).unwrap();
        WorktreeConfig::remove(tmp.path()).unwrap();
        assert_eq!(WorktreeConfig::load(tmp.path()).unwrap(), None);
    }

    #[test]
    fn generations_are_sorted_numbers() {
        let tmp = tempdir();
        for g in ["10", "2", "x"] {
            std::fs::create_dir_all(freeze_root(tmp.path()).join(g)).unwrap();
        }
        assert_eq!(generations(tmp.path()), vec![2, 10]);
        assert_eq!(
            machine_dir(tmp.path(), 2, "dc01"),
            tmp.path().join("freeze/2/dc01")
        );
    }
}
