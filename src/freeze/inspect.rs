//! What the disks of a worktree family say about the freeze (PRD §21): the
//! facts every plan in [`super::plan`] is computed from, which worktrees
//! depend on which generation, and `vmlab worktree status`.
//!
//! Dependence is read off the qcow2 files themselves — each worktree disk's
//! backing chain, walked file by file — rather than off any record, because
//! the chain is what breaks if a generation goes away.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use super::plan::DiskFacts;
use crate::paths::lab_local_dir;
use crate::template::qimg;

/// What `qemu-img info` says about `path`, or `None` when it does not exist.
pub async fn disk_facts(path: &Path) -> Result<Option<DiskFacts>> {
    if !path.exists() {
        return Ok(None);
    }
    let info = qimg::image_info(path).await?;
    let backing = info.full_backing_file.clone().or_else(|| {
        info.backing_file
            .clone()
            .map(|b| path.parent().unwrap_or(Path::new("/")).join(b))
    });
    Ok(Some(DiskFacts {
        path: path.to_path_buf(),
        backing,
        backing_relative: info.backing_file.as_ref().is_some_and(|b| b.is_relative()),
        snapshots: info.snapshots,
    }))
}

/// The longest backing chain followed. Real chains are a handful of files;
/// this only stops a loop.
const MAX_CHAIN: usize = 64;

/// `path` and every file under it in its backing chain, top first. A file
/// that is missing or unreadable ends the walk where it is.
pub async fn chain(path: &Path) -> Vec<DiskFacts> {
    let mut out = Vec::new();
    let mut next = Some(path.to_path_buf());
    while let Some(p) = next.take() {
        if out.len() >= MAX_CHAIN {
            break;
        }
        let Ok(Some(f)) = disk_facts(&p).await else {
            break;
        };
        next = f.backing.clone();
        out.push(f);
    }
    out
}

/// The generation `path` is a frozen file of, when it lies under
/// `freeze_root` (`…/freeze/<n>/…`).
pub fn generation_of(path: &Path, freeze_root: &Path) -> Option<u32> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root = freeze_root
        .canonicalize()
        .unwrap_or_else(|_| freeze_root.to_path_buf());
    path.strip_prefix(&root)
        .ok()?
        .components()
        .next()?
        .as_os_str()
        .to_str()?
        .parse()
        .ok()
}

/// Every qcow2 a lab's machines own: the files directly in each
/// `.vmlab/vms/<machine>/` and `.vmlab/containers/<machine>/`.
pub fn lab_disks(lab_local: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ["vms", "containers"]
        .iter()
        .flat_map(|kind| {
            std::fs::read_dir(lab_local.join(kind))
                .into_iter()
                .flatten()
        })
        .flatten()
        .flat_map(|machine| std::fs::read_dir(machine.path()).into_iter().flatten())
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "qcow2"))
        .collect();
    out.sort();
    out
}

/// A linked worktree whose disks back onto the main lab's freeze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependant {
    /// The worktree's lab root.
    pub root: PathBuf,
    /// Each disk that depends, and the newest generation in its chain.
    pub disks: Vec<(PathBuf, u32)>,
}

impl Dependant {
    pub fn generations(&self) -> BTreeSet<u32> {
        self.disks.iter().map(|(_, g)| *g).collect()
    }
}

/// The worktrees of the main lab at `main_root` that have a disk backing onto
/// any of its generations.
pub async fn dependants(main_root: &Path) -> Vec<Dependant> {
    let root = super::freeze_root(&lab_local_dir(main_root));
    let mut out = Vec::new();
    for wt in super::worktree_lab_roots(main_root) {
        let mut disks = Vec::new();
        for disk in lab_disks(&lab_local_dir(&wt)) {
            let generation = chain(&disk)
                .await
                .iter()
                .skip(1)
                .find_map(|f| generation_of(&f.path, &root));
            if let Some(g) = generation {
                disks.push((disk, g));
            }
        }
        if !disks.is_empty() {
            out.push(Dependant { root: wt, disks });
        }
    }
    out
}

/// Why `what` cannot happen while `dependants` use the freeze, naming each
/// worktree and how to release it.
pub fn dependants_refusal(what: &str, dependants: &[Dependant]) -> String {
    let names: Vec<String> = dependants
        .iter()
        .map(|d| {
            let gens: Vec<String> = d.generations().iter().map(u32::to_string).collect();
            format!("{} (generation {})", d.root.display(), gens.join(", "))
        })
        .collect();
    format!(
        "{what} while worktree labs back onto the freeze: {} — run `vmlab destroy` in each \
         to release it",
        names.join("; ")
    )
}

/// Each generation's freeze point, read from the machine records frozen into
/// it: the record whose `freeze_point` names the generation.
pub fn generation_points(lab_local: &Path) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for g in super::generations(lab_local) {
        let dir = super::generation_dir(lab_local, g);
        let found = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read(e.path().join(super::MACHINE_STATE_FILE)).ok())
            .filter_map(|b| serde_json::from_slice::<crate::labd::state::MachineState>(&b).ok())
            .find_map(|m| {
                m.snapshots
                    .into_iter()
                    .find(|(_, r)| r.freeze_point == Some(g))
                    .map(|(n, _)| n)
            });
        if let Some(name) = found {
            out.insert(g, name);
        }
    }
    out
}

/// One generation, as `vmlab worktree status` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct GenerationStatus {
    pub generation: u32,
    /// Its freeze point.
    pub snapshot: Option<String>,
    /// The machines frozen into it.
    pub machines: Vec<String>,
    /// Whether `worktree.json` names it: what a worktree's next `up` clones.
    pub newest: bool,
    /// The worktree lab roots with a disk whose newest generation is this.
    pub worktrees: Vec<PathBuf>,
}

/// `vmlab worktree status`.
#[derive(Debug, Clone, Serialize)]
pub struct FreezeStatus {
    /// The main checkout's lab root.
    pub main_root: PathBuf,
    /// The lab asked about, when it is a linked worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    pub config: Option<super::WorktreeConfig>,
    pub generations: Vec<GenerationStatus>,
}

/// The freeze the lab at `root` belongs to, whether `root` is the main
/// checkout or one of its linked worktrees.
pub async fn status(root: &Path) -> Result<FreezeStatus> {
    let (main_root, worktree) = match super::main_lab_root(root) {
        Some(main) => (main, Some(root.to_path_buf())),
        None => (root.to_path_buf(), None),
    };
    let lab_local = lab_local_dir(&main_root);
    let config = super::WorktreeConfig::load(&lab_local)?;
    let points = generation_points(&lab_local);
    let deps = dependants(&main_root).await;
    let generations = super::generations(&lab_local)
        .into_iter()
        .map(|g| {
            let mut machines: Vec<String> = std::fs::read_dir(super::generation_dir(&lab_local, g))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().join(super::MACHINE_STATE_FILE).is_file())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect();
            machines.sort();
            GenerationStatus {
                generation: g,
                snapshot: points.get(&g).cloned().or_else(|| {
                    config
                        .as_ref()
                        .filter(|c| c.generation == g)
                        .map(|c| c.snapshot.clone())
                }),
                machines,
                newest: config.as_ref().is_some_and(|c| c.generation == g),
                worktrees: deps
                    .iter()
                    .filter(|d| d.generations().contains(&g))
                    .map(|d| d.root.clone())
                    .collect(),
            }
        })
        .collect();
    Ok(FreezeStatus {
        main_root,
        worktree,
        config,
        generations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_path_names_its_generation() {
        let root = Path::new("/m/.vmlab/freeze");
        assert_eq!(
            generation_of(Path::new("/m/.vmlab/freeze/3/dc/disk0.qcow2"), root),
            Some(3)
        );
        assert_eq!(
            generation_of(Path::new("/m/.vmlab/vms/dc/disk0.qcow2"), root),
            None
        );
        assert_eq!(
            generation_of(Path::new("/m/.vmlab/freeze/x/dc/disk0.qcow2"), root),
            None
        );
    }

    #[test]
    fn the_refusal_names_each_worktree_and_the_way_out() {
        let d = Dependant {
            root: "/wt/feat-x/labs/demo".into(),
            disks: vec![("/wt/a.qcow2".into(), 1), ("/wt/b.qcow2".into(), 2)],
        };
        let why = dependants_refusal("cannot thaw", &[d]);
        assert!(
            why.contains("/wt/feat-x/labs/demo (generation 1, 2)"),
            "{why}"
        );
        assert!(why.contains("vmlab destroy"), "{why}");
    }

    #[test]
    fn generation_points_come_from_the_frozen_records() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = crate::labd::state::MachineState::default();
        m.snapshots.insert(
            "base".into(),
            crate::labd::state::SnapshotRecord {
                online: false,
                taken_at: chrono::Utc::now(),
                image_digest: None,
                freeze_point: Some(1),
                frozen_in: None,
                hypervisor: Default::default(),
            },
        );
        let dir = super::super::machine_dir(tmp.path(), 1, "dc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(super::super::MACHINE_STATE_FILE),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(super::super::generation_dir(tmp.path(), 2)).unwrap();
        assert_eq!(
            generation_points(tmp.path()),
            BTreeMap::from([(1, "base".to_string())])
        );
    }
}
