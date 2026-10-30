//! Building a worktree lab's machines from the main checkout's freeze (PRD
//! §21.2, ADR-0019).
//!
//! In a linked git worktree whose main checkout is frozen, a machine with no
//! disk yet is a linked clone of the newest generation when it is in the
//! freeze and its template or image reference is the one it was frozen under.
//! Every other machine builds from its template or image as it always has, and
//! a machine that already has a disk keeps it.
//!
//! Which machine comes from where, and exactly what is copied and overlaid
//! for each, is a [`FreezePlan`] computed before any file is touched
//! (ADR-0003): [`observe`] reads the filesystem once, [`plan`] is a pure
//! function of what it read. The lab daemon plans when it builds, because a
//! cloned machine's MACs are the frozen ones and a machine's MACs are fixed
//! when it is built; it executes a machine's part of the plan just before
//! that machine's first start ([`MachineClone::stage`], then
//! [`MachineClone::finish`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::freeze::{self, Family, WorktreeConfig};
use crate::labd::state::MachineState;

/// The kind of disk a machine boots, which decides what a clone of it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Vm,
    Container,
}

/// One machine as this worktree's lab file declares it: what the planner
/// compares against the freeze.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    pub kind: Kind,
    /// What its disk is made from: a VM's [`crate::labd::vm::clone_source`],
    /// a container's `image =` reference.
    pub source: String,
    /// Where the machine keeps its disks (`.vmlab/vms/<name>` or
    /// `.vmlab/containers/<name>`).
    pub local: PathBuf,
    /// The disk files it boots, primary first: a VM's `disk0.qcow2` and each
    /// `disk-<name>.qcow2` it declares, a container's `scratch.qcow2`.
    pub disks: Vec<String>,
    /// The named volumes it mounts (containers only).
    pub volumes: Vec<String>,
}

impl Declared {
    fn primary(&self) -> &str {
        &self.disks[0]
    }
}

/// One machine's directory in a generation, as read off the disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Frozen {
    /// The names of the files and directories in it.
    pub entries: BTreeSet<String>,
    /// `clone-source`, trimmed, when present.
    pub clone_source: Option<String>,
    /// `state.json`, or why it could not be read.
    pub record: std::result::Result<MachineState, String>,
}

/// Everything the planner needs from the filesystem, read once.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    /// The main checkout's lab root and working data, and its
    /// `worktree.json`. `None` when the lab is not a worktree lab of a
    /// frozen family.
    pub family: Option<(PathBuf, PathBuf, WorktreeConfig)>,
    /// The newest generation's directory exists.
    pub generation_present: bool,
    /// Machine directories in the newest generation, by name.
    pub frozen: BTreeMap<String, Frozen>,
    /// Named volumes in the newest generation.
    pub frozen_volumes: BTreeSet<String>,
    /// Declared machines whose primary disk exists already.
    pub built: BTreeSet<String>,
    /// The worktree lab's own working data.
    pub lab_local: PathBuf,
    /// Named volumes the worktree lab already has.
    pub volumes_present: BTreeSet<String>,
}

/// Read what [`plan`] needs for the lab at `root` declaring `machines`.
pub fn observe(root: &Path, lab_local: &Path, machines: &[Declared]) -> Result<Observed> {
    let built = machines
        .iter()
        .filter(|m| m.local.join(m.primary()).exists())
        .map(|m| m.name.clone())
        .collect();
    let volumes_present = machines
        .iter()
        .flat_map(|m| &m.volumes)
        .filter(|v| crate::labd::container::named_volume_dir(lab_local, v).exists())
        .cloned()
        .collect();
    let mut observed = Observed {
        family: None,
        generation_present: false,
        frozen: BTreeMap::new(),
        frozen_volumes: BTreeSet::new(),
        built,
        lab_local: lab_local.to_path_buf(),
        volumes_present,
    };
    let Some(Family::Worktree {
        main_root,
        main_lab_local,
        config,
    }) = Family::of(root)?
    else {
        return Ok(observed);
    };
    let generation = freeze::generation_dir(&main_lab_local, config.generation);
    observed.generation_present = generation.is_dir();
    for m in machines {
        let dir = freeze::machine_dir(&main_lab_local, config.generation, &m.name);
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        let entries = read
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        let clone_source = std::fs::read_to_string(dir.join("clone-source"))
            .ok()
            .map(|s| s.trim().to_string());
        let state = dir.join(freeze::MACHINE_STATE_FILE);
        let record = std::fs::read(&state)
            .map_err(|e| format!("reading {}: {e}", state.display()))
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| format!("parsing {}: {e}", state.display()))
            });
        observed.frozen.insert(
            m.name.clone(),
            Frozen {
                entries,
                clone_source,
                record,
            },
        );
    }
    observed.frozen_volumes =
        std::fs::read_dir(freeze::volumes_dir(&main_lab_local, config.generation))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
    // `main_lab_root` of a lab at the top of its checkout ends in a
    // separator; said in a message, the path reads better without it.
    let main_root = main_root.components().collect();
    observed.family = Some((main_root, main_lab_local, config));
    Ok(observed)
}

/// Where one machine's disk comes from on its next start.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// It has a disk, and keeps it.
    Built,
    /// From its template or image, as without a freeze; the reason is why
    /// not from the freeze.
    Template(String),
    /// A linked clone of the freeze.
    Freeze(Box<MachineClone>),
    /// Nothing: the freeze it would come from cannot be read, and a
    /// template build in its place would silently be a different machine.
    Refused(String),
}

impl Source {
    /// Whether a daemon planned with `self` builds the machine the way a plan
    /// made now (`now`) would. Only a machine without a disk can differ: one
    /// already built keeps its disk whatever either plan says.
    fn agrees_with(&self, now: &Source) -> bool {
        match (self, now) {
            (_, Source::Built) => true,
            (Source::Template(_), Source::Template(_)) => true,
            (a, b) => a == b,
        }
    }
}

/// A freeze copy: a file or a directory, copied whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCopy {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// A qcow2 overlay at `dest` backed by the frozen file `backing`, named by
/// absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub backing: PathBuf,
    pub dest: PathBuf,
}

/// Everything building one machine from the freeze does.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineClone {
    pub generation: u32,
    /// The machine's own directory, created first.
    pub local: PathBuf,
    /// Its extra disks, then its primary disk last: the primary disk's
    /// presence is what marks the machine built, so it appears only once
    /// everything else is in place.
    pub overlays: Vec<Overlay>,
    /// `OVMF_VARS.fd`, `tpm-state/`, `clone-source`, `firstboot.done` and
    /// `tsc-hz`, when the frozen machine has them.
    pub copies: Vec<FileCopy>,
    /// `freeze-generation` beside the disks.
    pub generation_file: PathBuf,
    /// The machine's record as the worktree lab keeps it: the frozen one,
    /// MACs and image pin unchanged, the freeze point its only snapshot.
    pub record: MachineState,
}

/// A named volume copied from the freeze, before the first start of any
/// container that mounts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeCopy {
    pub name: String,
    pub copy: FileCopy,
    pub used_by: Vec<String>,
}

/// The newest generation, as this worktree lab builds from it.
#[derive(Debug, Clone, PartialEq)]
pub struct FreezePlan {
    pub main_root: PathBuf,
    pub generation: u32,
    pub snapshot: String,
    /// Every declared machine, by name.
    pub machines: BTreeMap<String, Source>,
    /// Named volumes this lab mounts, has not got yet, and the freeze holds.
    pub volumes: Vec<VolumeCopy>,
}

impl FreezePlan {
    /// Every machine built from the freeze.
    pub fn clones(&self) -> impl Iterator<Item = (&str, &MachineClone)> {
        self.machines.iter().filter_map(|(n, s)| match s {
            Source::Freeze(c) => Some((n.as_str(), c.as_ref())),
            _ => None,
        })
    }

    /// The frozen record `machine` is built with, when it is built from the
    /// freeze: where its MACs and image pin come from.
    pub fn record(&self, machine: &str) -> Option<&MachineState> {
        match self.machines.get(machine)? {
            Source::Freeze(c) => Some(&c.record),
            _ => None,
        }
    }
}

/// Whether a daemon that planned `built_with` would build some machine
/// differently from a plan made now: the freeze has appeared, moved to a new
/// generation or gone, or a machine lost its disk since. Such a daemon is
/// replaced before it boots anything, as one running an edited lab file is.
pub fn stale(built_with: Option<&FreezePlan>, now: Option<&FreezePlan>) -> bool {
    let sources = |p: Option<&FreezePlan>, m: &str| -> Source {
        p.and_then(|p| p.machines.get(m).cloned())
            .unwrap_or_else(|| Source::Template(String::new()))
    };
    let names: BTreeSet<&String> = built_with
        .iter()
        .chain(now.iter())
        .flat_map(|p| p.machines.keys())
        .collect();
    names
        .into_iter()
        .any(|m| !sources(built_with, m).agrees_with(&sources(now, m)))
}

/// Decide which machines of a worktree lab are built from the freeze, and
/// with what. `None` when no freeze applies to the lab.
pub fn plan(machines: &[Declared], observed: &Observed) -> Option<FreezePlan> {
    let (main_root, main_lab_local, config) = observed.family.as_ref()?;
    let generation = config.generation;
    let mut plan = FreezePlan {
        main_root: main_root.clone(),
        generation,
        snapshot: config.snapshot.clone(),
        machines: BTreeMap::new(),
        volumes: Vec::new(),
    };
    for m in machines {
        let source = if observed.built.contains(&m.name) {
            Source::Built
        } else if !observed.generation_present {
            Source::Refused(missing_generation(main_root, main_lab_local, generation))
        } else {
            machine_source(m, observed, main_root, main_lab_local, config)
        };
        plan.machines.insert(m.name.clone(), source);
    }
    if observed.generation_present {
        let mut used_by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for m in machines {
            for v in &m.volumes {
                let users = used_by.entry(v).or_default();
                if !users.contains(&m.name) {
                    users.push(m.name.clone());
                }
            }
        }
        for (name, used_by) in used_by {
            if observed.volumes_present.contains(name) || !observed.frozen_volumes.contains(name) {
                continue;
            }
            plan.volumes.push(VolumeCopy {
                name: name.to_string(),
                copy: FileCopy {
                    from: freeze::volumes_dir(main_lab_local, generation).join(name),
                    to: crate::labd::container::named_volume_dir(&observed.lab_local, name),
                },
                used_by,
            });
        }
    }
    Some(plan)
}

fn missing_generation(main_root: &Path, main_lab_local: &Path, generation: u32) -> String {
    format!(
        "the main checkout's lab at {} is frozen at generation {generation}, but {} is \
         missing, so this worktree's machines cannot be cloned from it — and building them \
         from their templates instead would silently skip the provisioning the freeze \
         carries. Run `vmlab worktree freeze <snapshot>` in {} to freeze it again",
        main_root.display(),
        freeze::generation_dir(main_lab_local, generation).display(),
        main_root.display(),
    )
}

/// One unbuilt machine of a generation that exists.
fn machine_source(
    m: &Declared,
    observed: &Observed,
    main_root: &Path,
    main_lab_local: &Path,
    config: &WorktreeConfig,
) -> Source {
    let generation = config.generation;
    let Some(frozen) = observed.frozen.get(&m.name) else {
        return Source::Template(format!("not in freeze generation {generation}"));
    };
    if !frozen.entries.contains(m.primary()) {
        return Source::Template(format!(
            "its frozen directory holds no {}, so it was frozen as another kind of machine",
            m.primary()
        ));
    }
    let record = match &frozen.record {
        Ok(record) => record,
        Err(why) => {
            return Source::Refused(format!(
                "\"{}\" is in freeze generation {generation}, but its frozen state record \
                 cannot be read ({why}). Run `vmlab worktree freeze <snapshot>` in {} to \
                 freeze again",
                m.name,
                main_root.display(),
            ));
        }
    };
    let frozen_source = match m.kind {
        Kind::Vm => frozen.clone_source.as_deref(),
        Kind::Container => record.image_ref.as_deref(),
    };
    if frozen_source != Some(m.source.as_str()) {
        return Source::Template(match frozen_source {
            Some(was) => format!("frozen from \"{was}\", declared now as \"{}\"", m.source),
            None => "its frozen copy does not record what it was made from".to_string(),
        });
    }

    let dir = freeze::machine_dir(main_lab_local, generation, &m.name);
    // The primary disk, always frozen (checked above), last.
    let overlays = m
        .disks
        .iter()
        .skip(1)
        .chain(std::iter::once(&m.disks[0]))
        .filter(|d| frozen.entries.contains(*d))
        .map(|d| Overlay {
            backing: dir.join(d),
            dest: m.local.join(d),
        })
        .collect::<Vec<_>>();
    let carried: &[&str] = match m.kind {
        Kind::Vm => &[
            "OVMF_VARS.fd",
            "tpm-state",
            "clone-source",
            "firstboot.done",
            "tsc-hz",
        ],
        Kind::Container => &[],
    };
    let copies = carried
        .iter()
        .filter(|f| frozen.entries.contains(**f))
        .map(|f| FileCopy {
            from: dir.join(f),
            to: m.local.join(f),
        })
        .collect();
    Source::Freeze(Box::new(MachineClone {
        generation,
        local: m.local.clone(),
        overlays,
        copies,
        generation_file: m.local.join(freeze::FREEZE_SOURCE_FILE),
        record: worktree_record(record, config),
    }))
}

/// The frozen record as a worktree machine keeps it.
///
/// Its MACs and a container's image pin are the frozen machine's, unchanged.
/// Of its snapshots only the freeze point is kept: every other one lives in a
/// frozen file this lab must never write, and one taken after the freeze is
/// in the main lab's overlay, which this lab has no copy of. The record of an
/// agent repair stays, because the frozen disk may well carry the repaired
/// agent. A timed-out run does not: it names a process tree in the main
/// lab's running guest, and the clone boots cold from an offline snapshot.
fn worktree_record(frozen: &MachineState, config: &WorktreeConfig) -> MachineState {
    let mut record = frozen.clone();
    record.timed_out_run = None;
    record.snapshots = frozen
        .snapshots
        .iter()
        .filter(|(name, s)| **name == config.snapshot || s.freeze_point == Some(config.generation))
        .map(|(name, s)| {
            let mut s = s.clone();
            s.freeze_point = Some(config.generation);
            s.frozen_in = None;
            (name.clone(), s)
        })
        .collect();
    record
}

impl MachineClone {
    /// Everything but the primary disk: the copies, the extra disks'
    /// overlays and `freeze-generation`. Safe to run again after a failure.
    pub async fn stage(&self) -> Result<()> {
        std::fs::create_dir_all(&self.local)
            .with_context(|| format!("creating {}", self.local.display()))?;
        for c in &self.copies {
            copy_tree(&c.from, &c.to).await?;
        }
        for o in &self.overlays[..self.overlays.len() - 1] {
            overlay(o).await?;
        }
        std::fs::write(&self.generation_file, format!("{}\n", self.generation))
            .with_context(|| format!("writing {}", self.generation_file.display()))
    }

    /// The primary disk's overlay, which makes the machine built.
    pub async fn finish(&self) -> Result<()> {
        overlay(self.overlays.last().expect("a clone has its primary disk")).await
    }
}

impl VolumeCopy {
    pub async fn run(&self) -> Result<()> {
        if let Some(parent) = self.copy.to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        copy_tree(&self.copy.from, &self.copy.to).await
    }
}

/// An overlay backed by the frozen file. The frozen file is only ever read:
/// `qemu-img create` opens a backing file read-only to learn its size.
async fn overlay(o: &Overlay) -> Result<()> {
    match std::fs::remove_file(&o.dest) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", o.dest.display()));
        }
        _ => {}
    }
    crate::template::qimg::create_linked_clone(&o.backing, &o.dest)
        .await
        .with_context(|| {
            format!(
                "creating {} over the frozen {}",
                o.dest.display(),
                o.backing.display()
            )
        })
}

/// Copy a file or a directory whole, preserving modes and symlinks, sharing
/// blocks where the filesystem can (`cp --reflink=auto -a`). Through a
/// temporary name and a rename, so a half copy is never taken for a whole
/// one. The copy is left writable by its owner: a VARS store or TPM state
/// copied off a read-only source is still the machine's own to write.
async fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let name = to.file_name().context("copy destination has no name")?;
    let tmp = to.with_file_name(format!(".{}.freeze-tmp", name.to_string_lossy()));
    remove_any(&tmp)?;
    let out = tokio::process::Command::new("cp")
        .arg("--reflink=auto")
        .arg("-a")
        .arg("--")
        .arg(from)
        .arg(&tmp)
        .output()
        .await
        .context("running cp")?;
    if !out.status.success() {
        let _ = remove_any(&tmp);
        anyhow::bail!(
            "copying {} to {}: {}",
            from.display(),
            to.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let meta = std::fs::symlink_metadata(&tmp)?;
    if !meta.file_type().is_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = meta.permissions();
        perm.set_mode(perm.mode() | 0o200);
        std::fs::set_permissions(&tmp, perm)?;
    }
    remove_any(to)?;
    std::fs::rename(&tmp, to).with_context(|| format!("writing {}", to.display()))
}

fn remove_any(path: &Path) -> Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(_) => return Ok(()),
    };
    result.with_context(|| format!("removing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::MacAddr;
    use crate::labd::state::{SnapshotRecord, TimedOutRun};

    const MAIN: &str = "/repo/main/lab";
    const MAIN_LOCAL: &str = "/repo/main/lab/.vmlab";
    const WT_LOCAL: &str = "/repo/wt/lab/.vmlab";

    fn vm(name: &str, source: &str, extra: &[&str]) -> Declared {
        Declared {
            name: name.into(),
            kind: Kind::Vm,
            source: source.into(),
            local: Path::new(WT_LOCAL).join("vms").join(name),
            disks: std::iter::once("disk0.qcow2".to_string())
                .chain(extra.iter().map(|d| format!("disk-{d}.qcow2")))
                .collect(),
            volumes: Vec::new(),
        }
    }

    fn container(name: &str, image: &str, volumes: &[&str]) -> Declared {
        Declared {
            name: name.into(),
            kind: Kind::Container,
            source: image.into(),
            local: Path::new(WT_LOCAL).join("containers").join(name),
            disks: vec!["scratch.qcow2".into()],
            volumes: volumes.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn snap(freeze_point: Option<u32>, frozen_in: Option<u32>) -> SnapshotRecord {
        SnapshotRecord {
            online: false,
            taken_at: "2026-10-01T00:00:00Z".parse().unwrap(),
            image_digest: None,
            freeze_point,
            frozen_in,
            hypervisor: Default::default(),
        }
    }

    fn record(mac: u8) -> MachineState {
        MachineState {
            macs: vec![MacAddr([0x52, 0x54, 0, 1, 2, mac])],
            snapshots: [
                ("base".to_string(), snap(Some(1), None)),
                ("early".to_string(), snap(None, Some(1))),
            ]
            .into(),
            repaired_agent: Some("0.9.1".into()),
            timed_out_run: Some(TimedOutRun {
                tree: "t1".into(),
                playbook: "pb".into(),
                play: "p".into(),
                at: "2026-10-01T00:00:00Z".parse().unwrap(),
            }),
            ..MachineState::default()
        }
    }

    fn frozen(files: &[&str], clone_source: Option<&str>, record: MachineState) -> Frozen {
        Frozen {
            entries: files
                .iter()
                .map(|f| f.to_string())
                .chain(["state.json".to_string()])
                .collect(),
            clone_source: clone_source.map(str::to_string),
            record: Ok(record),
        }
    }

    fn observed(frozen: Vec<(&str, Frozen)>) -> Observed {
        Observed {
            family: Some((
                MAIN.into(),
                MAIN_LOCAL.into(),
                WorktreeConfig {
                    generation: 1,
                    snapshot: "base".into(),
                },
            )),
            generation_present: true,
            frozen: frozen
                .into_iter()
                .map(|(n, f)| (n.to_string(), f))
                .collect(),
            frozen_volumes: BTreeSet::new(),
            built: BTreeSet::new(),
            lab_local: WT_LOCAL.into(),
            volumes_present: BTreeSet::new(),
        }
    }

    fn clone_of<'a>(plan: &'a FreezePlan, m: &str) -> &'a MachineClone {
        match &plan.machines[m] {
            Source::Freeze(c) => c,
            other => panic!("{m}: {other:?}"),
        }
    }

    fn gen1(m: &str) -> PathBuf {
        Path::new(MAIN_LOCAL).join("freeze/1").join(m)
    }

    #[test]
    fn no_family_no_plan() {
        let mut o = observed(Vec::new());
        o.family = None;
        assert_eq!(plan(&[vm("dc01", "win", &[])], &o), None);
    }

    /// A machine in the freeze under the reference it is declared with now
    /// is a clone: an overlay per frozen disk it uses, the primary last, and
    /// copies of what the frozen machine carries.
    #[test]
    fn a_matched_vm_is_cloned() {
        let m = vm("dc01", "x86_64/win2022", &["data", "new"]);
        let o = observed(vec![(
            "dc01",
            frozen(
                &[
                    "disk0.qcow2",
                    "disk-data.qcow2",
                    "disk-gone.qcow2",
                    "OVMF_VARS.fd",
                    "tpm-state",
                    "clone-source",
                    "firstboot.done",
                    "tsc-hz",
                ],
                Some("x86_64/win2022"),
                record(1),
            ),
        )]);
        let p = plan(std::slice::from_ref(&m), &o).unwrap();
        let c = clone_of(&p, "dc01");
        assert_eq!(
            c.overlays,
            vec![
                Overlay {
                    backing: gen1("dc01").join("disk-data.qcow2"),
                    dest: m.local.join("disk-data.qcow2"),
                },
                Overlay {
                    backing: gen1("dc01").join("disk0.qcow2"),
                    dest: m.local.join("disk0.qcow2"),
                },
            ],
            "the declared disks the freeze holds, primary last; a disk the branch \
             declares new is left to `ensure_disks`, one it dropped is not overlaid"
        );
        let copied: Vec<_> = c
            .copies
            .iter()
            .map(|c| c.to.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            copied,
            [
                "OVMF_VARS.fd",
                "tpm-state",
                "clone-source",
                "firstboot.done",
                "tsc-hz"
            ]
        );
        assert!(c.copies.iter().all(|c| c.from.starts_with(gen1("dc01"))));
        assert_eq!(c.generation_file, m.local.join("freeze-generation"));
        assert_eq!(c.generation, 1);
        assert_eq!(p.record("dc01").unwrap().macs, record(1).macs);
    }

    /// The record keeps the MACs and the agent repair, keeps the freeze point
    /// as the only snapshot, and drops the timed-out run of a guest that
    /// boots cold.
    #[test]
    fn the_record_keeps_macs_and_only_the_freeze_point() {
        let mut r = record(7);
        r.snapshots.insert("renamed".into(), snap(None, Some(1)));
        let o = observed(vec![(
            "dc01",
            frozen(&["disk0.qcow2"], Some("win"), r.clone()),
        )]);
        let p = plan(&[vm("dc01", "win", &[])], &o).unwrap();
        let kept = &clone_of(&p, "dc01").record;
        assert_eq!(kept.macs, r.macs);
        assert_eq!(kept.repaired_agent.as_deref(), Some("0.9.1"));
        assert_eq!(kept.timed_out_run, None);
        assert_eq!(kept.snapshots.keys().collect::<Vec<_>>(), ["base"]);
        assert_eq!(kept.snapshots["base"].freeze_point, Some(1));
    }

    /// The freeze point is the snapshot `worktree.json` names, marked as such
    /// even when the frozen record predates the marking.
    #[test]
    fn the_named_snapshot_is_the_freeze_point() {
        let mut r = record(7);
        r.snapshots = [("base".to_string(), snap(None, None))].into();
        let o = observed(vec![("dc01", frozen(&["disk0.qcow2"], Some("win"), r))]);
        let p = plan(&[vm("dc01", "win", &[])], &o).unwrap();
        assert_eq!(
            clone_of(&p, "dc01").record.snapshots["base"].freeze_point,
            Some(1)
        );
    }

    #[test]
    fn a_changed_template_reference_builds_from_the_template() {
        let o = observed(vec![(
            "dc01",
            frozen(&["disk0.qcow2"], Some("x86_64/win2022"), record(1)),
        )]);
        let p = plan(&[vm("dc01", "x86_64/win2025", &[])], &o).unwrap();
        let Source::Template(why) = &p.machines["dc01"] else {
            panic!("{:?}", p.machines["dc01"]);
        };
        assert!(why.contains("win2022") && why.contains("win2025"), "{why}");
        assert_eq!(p.record("dc01"), None);
    }

    #[test]
    fn a_frozen_vm_without_a_recorded_source_builds_from_the_template() {
        let o = observed(vec![("dc01", frozen(&["disk0.qcow2"], None, record(1)))]);
        let p = plan(&[vm("dc01", "win", &[])], &o).unwrap();
        assert!(matches!(p.machines["dc01"], Source::Template(_)));
    }

    #[test]
    fn a_machine_new_in_the_branch_builds_from_the_template() {
        let o = observed(vec![(
            "dc01",
            frozen(&["disk0.qcow2"], Some("win"), record(1)),
        )]);
        let p = plan(&[vm("dc01", "win", &[]), vm("web", "linux", &[])], &o).unwrap();
        assert!(matches!(p.machines["dc01"], Source::Freeze(_)));
        let Source::Template(why) = &p.machines["web"] else {
            panic!();
        };
        assert!(why.contains("not in freeze generation 1"), "{why}");
    }

    /// A name the freeze holds as the other kind of machine is not in the
    /// freeze as far as this declaration goes.
    #[test]
    fn a_machine_frozen_as_another_kind_builds_from_its_image() {
        let o = observed(vec![(
            "web",
            frozen(&["disk0.qcow2"], Some("nginx:1"), record(1)),
        )]);
        let p = plan(&[container("web", "nginx:1", &[])], &o).unwrap();
        let Source::Template(why) = &p.machines["web"] else {
            panic!();
        };
        assert!(why.contains("scratch.qcow2"), "{why}");
    }

    /// A container matches on the image reference its frozen record pinned,
    /// and keeps the pin.
    #[test]
    fn a_container_matches_on_its_image_reference() {
        let mut r = record(3);
        r.image_ref = Some("nginx:1.27".into());
        r.image_digest = Some("sha256:abc".into());
        let o = observed(vec![("web", frozen(&["scratch.qcow2"], None, r))]);
        let p = plan(&[container("web", "nginx:1.27", &[])], &o).unwrap();
        let c = clone_of(&p, "web");
        assert_eq!(c.overlays.len(), 1);
        assert!(c.copies.is_empty(), "a container carries no firmware state");
        assert_eq!(c.record.image_digest.as_deref(), Some("sha256:abc"));

        let p = plan(&[container("web", "nginx:1.28", &[])], &o).unwrap();
        assert!(matches!(p.machines["web"], Source::Template(_)));
    }

    #[test]
    fn an_existing_disk_is_kept() {
        let mut o = observed(vec![(
            "dc01",
            frozen(&["disk0.qcow2"], Some("win"), record(1)),
        )]);
        o.built.insert("dc01".into());
        let p = plan(&[vm("dc01", "win", &[])], &o).unwrap();
        assert_eq!(p.machines["dc01"], Source::Built);
        assert_eq!(p.record("dc01"), None, "a built machine keeps its own MACs");
    }

    /// A missing generation refuses every machine still to be built, naming
    /// the main checkout, the generation and the fix; one already built is
    /// left alone.
    #[test]
    fn a_missing_generation_refuses_and_never_falls_back() {
        let mut o = observed(Vec::new());
        o.generation_present = false;
        o.built.insert("old".into());
        let p = plan(&[vm("dc01", "win", &[]), vm("old", "win", &[])], &o).unwrap();
        let Source::Refused(why) = &p.machines["dc01"] else {
            panic!("{:?}", p.machines["dc01"]);
        };
        assert!(why.contains(MAIN), "{why}");
        assert!(why.contains("generation 1"), "{why}");
        assert!(why.contains("vmlab worktree freeze"), "{why}");
        assert_eq!(p.machines["old"], Source::Built);
    }

    #[test]
    fn an_unreadable_frozen_record_refuses() {
        let mut f = frozen(&["disk0.qcow2"], Some("win"), record(1));
        f.record = Err("parsing state.json: bad".into());
        let p = plan(&[vm("dc01", "win", &[])], &observed(vec![("dc01", f)])).unwrap();
        let Source::Refused(why) = &p.machines["dc01"] else {
            panic!();
        };
        assert!(why.contains("bad") && why.contains(MAIN), "{why}");
    }

    /// A frozen named volume is copied once, before the first of the
    /// containers mounting it starts, and only when this lab has none yet.
    #[test]
    fn named_volumes_copy_on_first_build_only() {
        let mut o = observed(Vec::new());
        o.frozen_volumes = ["pgdata", "cache", "unused"].map(String::from).into();
        o.volumes_present.insert("cache".into());
        let p = plan(
            &[
                container("db", "postgres:16", &["pgdata", "cache"]),
                container("tool", "alpine", &["pgdata", "fresh"]),
            ],
            &o,
        )
        .unwrap();
        assert_eq!(
            p.volumes,
            vec![VolumeCopy {
                name: "pgdata".into(),
                copy: FileCopy {
                    from: Path::new(MAIN_LOCAL).join("freeze/1/volumes/pgdata"),
                    to: Path::new(WT_LOCAL).join("volumes/pgdata"),
                },
                used_by: vec!["db".into(), "tool".into()],
            }]
        );
    }

    /// A daemon is stale when a machine without a disk would now be built
    /// another way; machines built since, by either route, are not a change.
    #[test]
    fn staleness_is_about_unbuilt_machines_only() {
        let m = [vm("dc01", "win", &[]), vm("web", "linux", &[])];
        let o = observed(vec![(
            "dc01",
            frozen(&["disk0.qcow2"], Some("win"), record(1)),
        )]);
        let planned = plan(&m, &o).unwrap();
        assert!(!stale(Some(&planned), Some(&planned)));
        assert!(!stale(None, None));

        // Both built since: the daemon built them as it planned.
        let mut built = o.clone();
        built.built = ["dc01", "web"].map(String::from).into();
        assert!(!stale(Some(&planned), plan(&m, &built).as_ref()));

        // Frozen since the daemon started.
        let mut unfrozen = o.clone();
        unfrozen.family = None;
        assert!(stale(plan(&m, &unfrozen).as_ref(), Some(&planned)));
        // Thawed since, with dc01 still unbuilt.
        assert!(stale(Some(&planned), None));

        // Refrozen as generation 2.
        let mut refrozen = o.clone();
        refrozen.family.as_mut().unwrap().2.generation = 2;
        assert!(stale(Some(&planned), plan(&m, &refrozen).as_ref()));

        // dc01 was built when the daemon started and destroyed since.
        let mut was_built = o.clone();
        was_built.built.insert("dc01".into());
        assert!(stale(plan(&m, &was_built).as_ref(), Some(&planned)));

        // The generation went missing.
        let mut missing = o.clone();
        missing.generation_present = false;
        assert!(stale(Some(&planned), plan(&m, &missing).as_ref()));
    }

    // ---- the executor, against real qcow2 files ----

    fn have_qemu_img() -> bool {
        let found = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qemu-img").is_file())
        });
        if !found {
            eprintln!("skipping: qemu-img not found on PATH");
        }
        found
    }

    async fn qemu_img(args: &[&str]) -> String {
        let out = tokio::process::Command::new("qemu-img")
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "qemu-img {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The bytes a qcow2 image reads as, through its whole backing chain.
    async fn contents(image: &Path) -> Vec<u8> {
        let raw = image.with_extension("raw");
        qemu_img(&[
            "convert",
            "-U",
            "-O",
            "raw",
            image.to_str().unwrap(),
            raw.to_str().unwrap(),
        ])
        .await;
        std::fs::read(raw).unwrap()
    }

    /// A qcow2 at `path` whose first bytes are `data`, optionally backed by
    /// `backing` (whose other bytes it then reads through).
    async fn image(path: &Path, data: &[u8], offset: usize, backing: Option<&Path>) {
        let raw = path.with_extension("seed");
        let mut bytes = match backing {
            Some(b) => contents(b).await,
            None => vec![0u8; 1 << 20],
        };
        bytes[offset..offset + data.len()].copy_from_slice(data);
        std::fs::write(&raw, &bytes).unwrap();
        let (raw, path) = (raw.to_str().unwrap(), path.to_str().unwrap());
        match backing {
            Some(b) => {
                let b = b.to_str().unwrap();
                qemu_img(&["convert", "-O", "qcow2", "-B", b, "-F", "qcow2", raw, path]).await
            }
            None => qemu_img(&["convert", "-O", "qcow2", raw, path]).await,
        };
    }

    /// A main checkout frozen at generation 2 (backed by generation 1, backed
    /// by a template) and a linked worktree: observe it, plan it, run the
    /// clone, and read the worktree's disk back through its chain.
    #[tokio::test]
    async fn a_clone_reads_the_frozen_state_and_writes_no_frozen_file() {
        if !have_qemu_img() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        // git's layout, as `git worktree add` leaves it.
        let main = tmp.join("main");
        let wt = tmp.join("wt");
        let wt_git = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let main_local = crate::paths::lab_local_dir(&main);
        let wt_local = crate::paths::lab_local_dir(&wt);

        let template = tmp.join("template.qcow2");
        image(&template, b"TEMPLATE", 0, None).await;
        let g1 = freeze::machine_dir(&main_local, 1, "dc01");
        let g2 = freeze::machine_dir(&main_local, 2, "dc01");
        std::fs::create_dir_all(&g1).unwrap();
        std::fs::create_dir_all(g2.join("tpm-state")).unwrap();
        image(&g1.join("disk0.qcow2"), b"GEN-ONE", 4096, Some(&template)).await;
        image(
            &g2.join("disk0.qcow2"),
            b"GEN-TWO",
            8192,
            Some(&g1.join("disk0.qcow2")),
        )
        .await;
        std::fs::write(g2.join("OVMF_VARS.fd"), b"vars").unwrap();
        std::fs::write(g2.join("tpm-state/tpm2-00.permall"), b"tpm").unwrap();
        std::os::unix::fs::symlink("tpm2-00.permall", g2.join("tpm-state/link")).unwrap();
        std::fs::write(g2.join("clone-source"), "x86_64/win2022\n").unwrap();
        std::fs::write(g2.join("firstboot.done"), b"").unwrap();
        let mut r = record(9);
        r.snapshots = [("second".to_string(), snap(Some(2), None))].into();
        std::fs::write(g2.join("state.json"), serde_json::to_vec(&r).unwrap()).unwrap();
        std::fs::create_dir_all(freeze::volumes_dir(&main_local, 2).join("pgdata/sub")).unwrap();
        std::fs::write(
            freeze::volumes_dir(&main_local, 2).join("pgdata/sub/PG_VERSION"),
            b"16",
        )
        .unwrap();
        WorktreeConfig {
            generation: 2,
            snapshot: "second".into(),
        }
        .save(&main_local)
        .unwrap();
        // Read-only, as a freeze leaves them.
        let frozen_files = [
            g1.join("disk0.qcow2"),
            g2.join("disk0.qcow2"),
            g2.join("OVMF_VARS.fd"),
        ];
        let before: Vec<Vec<u8>> = frozen_files
            .iter()
            .map(|f| std::fs::read(f).unwrap())
            .collect();
        for f in &frozen_files {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o444)).unwrap();
        }

        let declared = vec![
            Declared {
                local: wt_local.join("vms/dc01"),
                ..vm("dc01", "x86_64/win2022", &[])
            },
            Declared {
                local: wt_local.join("containers/db"),
                ..container("db", "postgres:16", &["pgdata"])
            },
        ];
        let observed = observe(&wt, &wt_local, &declared).unwrap();
        let p = plan(&declared, &observed).unwrap();
        assert_eq!(p.main_root, main);
        assert!(matches!(p.machines["db"], Source::Template(_)));
        let c = clone_of(&p, "dc01");
        c.stage().await.unwrap();
        assert!(
            !wt_local.join("vms/dc01/disk0.qcow2").exists(),
            "the primary disk comes last"
        );
        c.finish().await.unwrap();
        for v in &p.volumes {
            v.run().await.unwrap();
        }

        let disk = wt_local.join("vms/dc01/disk0.qcow2");
        let read = contents(&disk).await;
        assert_eq!(&read[0..8], b"TEMPLATE");
        assert_eq!(&read[4096..4103], b"GEN-ONE");
        assert_eq!(&read[8192..8199], b"GEN-TWO");
        let info = crate::template::qimg::image_info(&disk).await.unwrap();
        assert_eq!(info.backing_file, Some(g2.join("disk0.qcow2")));

        let local = wt_local.join("vms/dc01");
        assert_eq!(std::fs::read(local.join("OVMF_VARS.fd")).unwrap(), b"vars");
        let vars_mode = std::fs::metadata(local.join("OVMF_VARS.fd"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(
            vars_mode & 0o200,
            0,
            "the VARS copy is the machine's to write"
        );
        assert_eq!(
            std::fs::read_link(local.join("tpm-state/link")).unwrap(),
            Path::new("tpm2-00.permall")
        );
        assert_eq!(
            std::fs::read_to_string(local.join("clone-source")).unwrap(),
            "x86_64/win2022\n"
        );
        assert!(local.join("firstboot.done").exists());
        assert_eq!(
            std::fs::read_to_string(local.join("freeze-generation")).unwrap(),
            "2\n"
        );
        assert_eq!(
            std::fs::read(wt_local.join("volumes/pgdata/sub/PG_VERSION")).unwrap(),
            b"16"
        );
        assert_eq!(c.record.snapshots.keys().collect::<Vec<_>>(), ["second"]);

        // Writing the clone leaves every frozen file as it was.
        qemu_img(&["resize", disk.to_str().unwrap(), "2M"]).await;
        for (f, b) in frozen_files.iter().zip(&before) {
            assert_eq!(&std::fs::read(f).unwrap(), b, "{} changed", f.display());
        }

        // Built now: planned again, it is kept.
        let again = plan(&declared, &observe(&wt, &wt_local, &declared).unwrap()).unwrap();
        assert_eq!(again.machines["dc01"], Source::Built);
        assert!(again.volumes.is_empty());
        assert!(!stale(
            Some(&FreezePlan {
                machines: [
                    ("dc01".to_string(), Source::Built),
                    ("db".to_string(), p.machines["db"].clone()),
                ]
                .into(),
                ..p.clone()
            }),
            Some(&again)
        ));
    }
}
