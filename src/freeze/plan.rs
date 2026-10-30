//! Freeze, thaw and freeze-point restore as values (ADR-0003, PRD §21).
//!
//! Each planner is a pure function of what `qemu-img info` and the lab state
//! said about the disks, and answers with either a refusal or the ordered
//! [`Plan`] the executor in [`super::steps`] runs. Nothing here touches a
//! file, which is what lets the tests below pin each verb's whole sequence.
//!
//! Two facts about qcow2 shape every plan. A disk's internal snapshot is a
//! set of that file's own clusters, so it means something only over the same
//! backing content it was taken against; and nothing but `qemu-img snapshot
//! -c` can put a snapshot into a file. Carrying a snapshot from one file to
//! another is therefore always the same four moves: set the receiving file's
//! active state to the content the snapshot was written against, apply the
//! snapshot in the file that holds it, commit that layer down, and take the
//! snapshot again in the receiving file. Online snapshots cannot make that
//! trip, because their machine state is not a cluster `commit` moves.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::steps::{Plan, Step};
use crate::labd::state::LabState;
use crate::proto::CommandError;
use crate::template::qimg::InternalSnapshot;

/// The internal snapshot a thaw or a freeze-point restore holds a disk's
/// current state in while it rearranges the chain. Never left behind by a
/// finished verb; a freeze refuses a disk that still has one.
pub const HOLD_SNAPSHOT: &str = "vmlab-freeze-hold";

/// What `qemu-img info` says about one disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFacts {
    pub path: PathBuf,
    /// The backing file, resolved to an absolute path. `None` for a disk
    /// with no backing (a blank or a scratch disk).
    pub backing: Option<PathBuf>,
    /// The image records its backing relative to itself, so moving it would
    /// break it.
    pub backing_relative: bool,
    pub snapshots: Vec<InternalSnapshot>,
}

impl DiskFacts {
    fn has(&self, name: &str) -> bool {
        self.snapshots.iter().any(|s| s.name == name)
    }

    fn online(&self) -> impl Iterator<Item = &str> {
        self.snapshots
            .iter()
            .filter(|s| s.vm_state_size > 0)
            .map(|s| s.name.as_str())
    }
}

fn refuse(message: impl Into<String>) -> anyhow::Error {
    CommandError::conflict(message).into()
}

fn file_name(path: &Path) -> &std::ffi::OsStr {
    path.file_name().unwrap_or_default()
}

/// `path` with `suffix` appended to its file name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = file_name(path).to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Which internal snapshot each file's active state equals, as far as the
/// plan has moved it. A step that changes a file's active state is undone by
/// applying the snapshot it equalled before, so every such step needs this.
#[derive(Default)]
struct Active(HashMap<PathBuf, Option<String>>);

impl Active {
    fn set(&mut self, disk: &Path, name: Option<&str>) {
        self.0.insert(disk.to_path_buf(), name.map(str::to_string));
    }

    fn get(&self, disk: &Path) -> Option<String> {
        self.0.get(disk).cloned().flatten()
    }

    /// Apply `name` on `disk`, undone by re-applying what it was.
    fn apply(&mut self, plan: &mut Plan, disk: &Path, name: &str) {
        let before = self.get(disk);
        if before.as_deref() == Some(name) {
            return;
        }
        let undo = before
            .map(|b| Step::SnapshotApply {
                disk: disk.to_path_buf(),
                name: b,
            })
            .into_iter()
            .collect();
        plan.push(
            Step::SnapshotApply {
                disk: disk.to_path_buf(),
                name: name.to_string(),
            },
            undo,
        );
        self.set(disk, Some(name));
    }

    /// Commit `top` into `base`, whose active state stops equalling any
    /// snapshot; undone by re-applying the one it equalled.
    fn commit(&mut self, plan: &mut Plan, top: &Path, base: &Path) {
        let undo = self
            .get(base)
            .map(|b| Step::SnapshotApply {
                disk: base.to_path_buf(),
                name: b,
            })
            .into_iter()
            .collect();
        plan.push(
            Step::Commit {
                top: top.to_path_buf(),
            },
            undo,
        );
        self.set(base, None);
    }

    fn create(&mut self, plan: &mut Plan, disk: &Path, name: &str) {
        plan.push(
            Step::SnapshotCreate {
                disk: disk.to_path_buf(),
                name: name.to_string(),
            },
            vec![Step::SnapshotDelete {
                disk: disk.to_path_buf(),
                name: name.to_string(),
            }],
        );
        self.set(disk, Some(name));
    }
}

// ---- freeze (§21.1) ----------------------------------------------------------

/// One machine of the lab, as a freeze needs it.
#[derive(Debug, Clone)]
pub struct FreezeMachine {
    pub name: String,
    pub stopped: bool,
    /// `.vmlab/vms/<name>` or `.vmlab/containers/<name>`.
    pub local_dir: PathBuf,
    /// Every disk the machine declares, in snapshot order, and what qemu-img
    /// said about it (`None`: the file does not exist).
    pub disks: Vec<(PathBuf, Option<DiskFacts>)>,
    /// The state record of `<snapshot>` on this machine: `None` when it has
    /// none.
    pub record: Option<crate::labd::state::SnapshotRecord>,
    /// The machine's state record as the freeze leaves it, serialized into
    /// the generation beside the frozen disks.
    pub state_json: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FreezeInput {
    pub lab_local: PathBuf,
    /// Set when the lab is in a linked git worktree, which never freezes.
    pub in_linked_worktree: bool,
    pub generation: u32,
    pub snapshot: String,
    pub machines: Vec<FreezeMachine>,
    /// Named volumes under `.vmlab/volumes/`.
    pub volumes: Vec<String>,
}

/// What a freeze copies beside a machine's frozen disks, when present: the
/// UEFI variables, the TPM, the template reference, the first-boot mark and
/// a `tsc = "host"` VM's pinned TSC frequency (§5.2).
pub const CARRIED: [&str; 5] = [
    "OVMF_VARS.fd",
    "tpm-state",
    "clone-source",
    "firstboot.done",
    "tsc-hz",
];

/// Plan `vmlab worktree freeze <snapshot>` (§21.1), or refuse it.
///
/// `carried` names which of [`CARRIED`] each machine's directory holds; it
/// is a fact of the filesystem the caller gathered, like the disks.
pub fn plan_freeze(input: &FreezeInput, carried: &BTreeMap<String, Vec<String>>) -> Result<Plan> {
    let snap = &input.snapshot;
    if input.in_linked_worktree {
        return Err(refuse(
            "this lab is in a linked git worktree; only the main checkout's lab freezes — run \
             `vmlab worktree freeze` there",
        ));
    }
    if input.machines.is_empty() {
        return Err(refuse(
            "the lab declares no machines, so there is nothing to freeze",
        ));
    }
    let running: Vec<&str> = input
        .machines
        .iter()
        .filter(|m| !m.stopped)
        .map(|m| m.name.as_str())
        .collect();
    if !running.is_empty() {
        return Err(refuse(format!(
            "a freeze needs the lab stopped, and {} {} running — `vmlab down` first",
            running.join(", "),
            if running.len() == 1 { "is" } else { "are" }
        )));
    }
    for m in &input.machines {
        let Some(record) = &m.record else {
            return Err(refuse(format!(
                "\"{}\" has no snapshot \"{snap}\" — take one on every machine with the lab \
                 stopped (`vmlab snapshot create {snap}`), then freeze",
                m.name
            )));
        };
        if record.online {
            return Err(refuse(format!(
                "\"{}\": snapshot \"{snap}\" was taken online. Its memory holds the main lab's \
                 DHCP leases and agent sessions, and booting its disk alone is a power cut — \
                 take an offline snapshot with the lab stopped and freeze that",
                m.name
            )));
        }
        if let Some(g) = record.frozen_in {
            return Err(refuse(format!(
                "\"{}\": snapshot \"{snap}\" is locked in generation {g}, inside a frozen file; \
                 take a new offline snapshot to freeze at",
                m.name
            )));
        }
        for (path, facts) in &m.disks {
            let Some(facts) = facts else {
                return Err(refuse(format!(
                    "\"{}\" has never been built ({} does not exist) — `vmlab up` it, take \
                     \"{snap}\", then freeze",
                    m.name,
                    path.display()
                )));
            };
            if !facts.has(snap) {
                return Err(refuse(format!(
                    "{} has no internal snapshot \"{snap}\", though the lab's record says \
                     \"{}\" has one",
                    path.display(),
                    m.name
                )));
            }
            if facts.has(HOLD_SNAPSHOT) {
                return Err(refuse(format!(
                    "{} still holds the snapshot \"{HOLD_SNAPSHOT}\" an interrupted freeze or \
                     thaw left behind; delete it with `qemu-img snapshot -d` once you are sure \
                     the disk is in the state you want",
                    path.display()
                )));
            }
            if facts.backing_relative {
                return Err(refuse(format!(
                    "{} names its backing file by a relative path, which moving it into the \
                     freeze would break",
                    path.display()
                )));
            }
        }
    }

    let mut plan = Plan::default();
    let gen_dir = super::generation_dir(&input.lab_local, input.generation);
    plan.push(
        Step::CreateDir {
            path: gen_dir.clone(),
        },
        vec![
            Step::Remove { path: gen_dir },
            Step::RemoveIfEmpty {
                path: super::freeze_root(&input.lab_local),
            },
        ],
    );
    for m in &input.machines {
        let dir = super::machine_dir(&input.lab_local, input.generation, &m.name);
        plan.push(
            Step::CreateDir { path: dir.clone() },
            vec![Step::Remove { path: dir.clone() }],
        );
        for (disk, _) in &m.disks {
            freeze_disk(&mut plan, disk, &dir.join(file_name(disk)), snap);
        }
        for name in carried.get(&m.name).into_iter().flatten() {
            let to = dir.join(name);
            plan.push(
                Step::Copy {
                    from: m.local_dir.join(name),
                    to: to.clone(),
                },
                vec![Step::Remove { path: to }],
            );
        }
        let state = dir.join(super::MACHINE_STATE_FILE);
        plan.push(
            Step::WriteFile {
                path: state.clone(),
                bytes: m.state_json.clone(),
            },
            vec![Step::Remove { path: state }],
        );
    }
    if !input.volumes.is_empty() {
        let vols = super::volumes_dir(&input.lab_local, input.generation);
        plan.push(
            Step::CreateDir { path: vols.clone() },
            vec![Step::Remove { path: vols.clone() }],
        );
        for v in &input.volumes {
            plan.push(
                Step::Copy {
                    from: input.lab_local.join(super::VOLUMES_DIR).join(v),
                    to: vols.join(v),
                },
                vec![Step::Remove { path: vols.join(v) }],
            );
        }
    }
    Ok(plan)
}

/// One disk's freeze, each step with what undoes it.
///
/// §21.1's sequence with step 4 done by a safe rebase rather than `convert
/// -B`, which would write every cluster the disk's own layer ever allocated:
/// a whole provisioning's worth on a first freeze. A copy of the disk taken
/// before anything moves keeps the current state readable, the main lab's
/// new disk starts as an empty overlay on that copy, and `qemu-img rebase`
/// (safe) moves it onto the frozen file, writing only the clusters where the
/// current state and the freeze point differ. No hold snapshot is needed.
///
/// The copy is also the rollback: renamed back over the disk it is the disk,
/// byte for byte. So it stays until the freeze is committed and is removed by
/// the plan's cleanup, never by a step.
fn freeze_disk(plan: &mut Plan, disk: &Path, frozen: &Path, snap: &str) {
    let (disk, frozen) = (disk.to_path_buf(), frozen.to_path_buf());
    let work = beside(&disk, ".freeze-work");
    // 1. The current state, kept readable: `cp --reflink=auto`, so free where
    //    the filesystem shares extents.
    plan.push(
        Step::Copy {
            from: disk.clone(),
            to: work.clone(),
        },
        vec![Step::Remove { path: work.clone() }],
    );
    // 2. Rewind the file to the freeze point. Undone by the copy, which is
    //    the file as it was.
    plan.push(
        Step::SnapshotApply {
            disk: disk.clone(),
            name: snap.to_string(),
        },
        vec![Step::Rename {
            from: work.clone(),
            to: disk.clone(),
        }],
    );
    // 3. Into the generation.
    plan.push(
        Step::Rename {
            from: disk.clone(),
            to: frozen.clone(),
        },
        vec![Step::Rename {
            from: frozen.clone(),
            to: disk.clone(),
        }],
    );
    // 4. Read-only before anything is layered on it, so the rebase below
    //    cannot open it read-write either.
    plan.push(
        Step::ReadOnly {
            path: frozen.clone(),
        },
        vec![Step::Writable {
            path: frozen.clone(),
        }],
    );
    // 5. The main lab's new disk: empty, over the current state.
    plan.push(
        Step::Overlay {
            backing: work.clone(),
            dest: disk.clone(),
        },
        vec![Step::Remove { path: disk.clone() }],
    );
    // 6. Onto the frozen file, writing only what differs from it.
    plan.push(
        Step::RebaseSafe {
            disk: disk.clone(),
            backing: frozen,
        },
        vec![],
    );
    plan.cleanup.push(Step::Remove { path: work });
}

/// Mark the main lab's records for generation `generation` frozen at
/// `snapshot` (§21.3): `snapshot` becomes the freeze point, and every other
/// record — every one that lives in a file being frozen — is locked in this
/// generation unless an earlier one already locked it.
pub fn mark_frozen(state: &mut LabState, snapshot: &str, generation: u32) {
    for m in state.machines.values_mut() {
        for (name, record) in m.snapshots.iter_mut() {
            if name == snapshot {
                record.freeze_point = Some(generation);
            } else if record.frozen_in.is_none() {
                record.frozen_in = Some(generation);
            }
        }
    }
}

// ---- freeze-point restore (§21.3) -------------------------------------------

/// Whether `backing` is `disk`'s frozen file in generation `generation` of
/// machine `machine`: `…/freeze/<n>/<machine>/<disk's file name>`. A main lab
/// disk's backing is in its own `.vmlab/`, a worktree's in the main
/// checkout's, so the shape is what is checked, not the prefix.
pub fn is_frozen_file(backing: &Path, disk: &Path, machine: &str, generation: u32) -> bool {
    let mut up = backing.ancestors().skip(1).map(file_name);
    file_name(backing) == file_name(disk)
        && up.next() == Some(std::ffi::OsStr::new(machine))
        && up.next() == Some(std::ffi::OsStr::new(&generation.to_string()))
        && up.next() == Some(std::ffi::OsStr::new(super::FREEZE_DIR))
}

/// Plan restoring the freeze point `snapshot` of generation `generation` on
/// machine `machine` (§21.3), in any lab of the family.
///
/// The freeze point is the backing file's own state, so the restore empties
/// the overlay. Where the overlay already holds `snapshot` itself — the
/// empty mark a previous freeze-point restore left — that is an ordinary
/// `snapshot -a`. Otherwise the overlay is replaced by a fresh one that
/// starts with that mark, and every snapshot taken in the old overlay since
/// the freeze is carried into it, so restoring the freeze point keeps them,
/// as restoring any snapshot does (§7.3). An online one cannot be carried,
/// and is refused by name rather than lost.
pub fn plan_point_restore(
    machine: &str,
    snapshot: &str,
    generation: u32,
    disks: &[DiskFacts],
) -> Result<Plan> {
    for d in disks {
        let Some(backing) = &d.backing else {
            return Err(refuse(format!(
                "\"{machine}\": {} was not part of generation {generation}'s freeze (it has no \
                 backing file), so it has no state at the freeze point \"{snapshot}\"",
                d.path.display()
            )));
        };
        if !is_frozen_file(backing, &d.path, machine, generation) {
            return Err(refuse(format!(
                "\"{machine}\": {} is not an overlay on generation {generation}'s frozen disk \
                 (it backs onto {}), so the freeze point \"{snapshot}\" cannot be restored on \
                 it — `vmlab vm destroy {machine}` and `vmlab up` rebuild it from the freeze",
                d.path.display(),
                backing.display()
            )));
        }
        if d.has(HOLD_SNAPSHOT) {
            return Err(refuse(format!(
                "{} still holds the snapshot \"{HOLD_SNAPSHOT}\" an interrupted freeze-point \
                 restore left behind",
                d.path.display()
            )));
        }
        if !d.has(snapshot) {
            let online: Vec<&str> = d.online().collect();
            if !online.is_empty() {
                return Err(refuse(format!(
                    "\"{machine}\": restoring the freeze point \"{snapshot}\" replaces {}, and \
                     the online snapshots {} it holds cannot be carried into the new one — \
                     `vmlab snapshot delete` them first",
                    d.path.display(),
                    online.join(", ")
                )));
            }
        }
    }

    let mut plan = Plan::default();
    for d in disks {
        let disk = &d.path;
        let backing = d.backing.clone().unwrap_or_default();
        // Held either way, so a later disk's failure puts this one back.
        let mut active = Active::default();
        active.create(&mut plan, disk, HOLD_SNAPSHOT);
        plan.cleanup.push(Step::SnapshotDelete {
            disk: disk.clone(),
            name: HOLD_SNAPSHOT.into(),
        });
        if d.has(snapshot) {
            active.apply(&mut plan, disk, snapshot);
            continue;
        }
        let fresh = beside(disk, ".restore");
        let carry = beside(disk, ".carry");
        plan.push(
            Step::Overlay {
                backing,
                dest: fresh.clone(),
            },
            vec![Step::Remove {
                path: fresh.clone(),
            }],
        );
        active.create(&mut plan, &fresh, snapshot);
        for s in d.snapshots.iter().filter(|s| s.name != HOLD_SNAPSHOT) {
            active.apply(&mut plan, &fresh, snapshot);
            plan.push(
                Step::Export {
                    src: disk.clone(),
                    snapshot: s.name.clone(),
                    backing: fresh.clone(),
                    dest: carry.clone(),
                },
                vec![Step::Remove {
                    path: carry.clone(),
                }],
            );
            active.commit(&mut plan, &carry, &fresh);
            plan.push(
                Step::Remove {
                    path: carry.clone(),
                },
                vec![],
            );
            active.create(&mut plan, &fresh, &s.name);
        }
        active.apply(&mut plan, &fresh, snapshot);
        let old = beside(disk, ".old");
        plan.push(
            Step::Rename {
                from: disk.clone(),
                to: old.clone(),
            },
            vec![Step::Rename {
                from: old.clone(),
                to: disk.clone(),
            }],
        );
        plan.push(
            Step::Rename {
                from: fresh.clone(),
                to: disk.clone(),
            },
            vec![Step::Rename {
                from: disk.clone(),
                to: fresh,
            }],
        );
        // The hold went with the old overlay; the cleanup is for it alone.
        plan.cleanup.pop();
        plan.cleanup.push(Step::Remove { path: old });
    }
    Ok(plan)
}

// ---- thaw (§21.3) -------------------------------------------------------------

/// One disk of the main lab and the frozen files under it.
#[derive(Debug, Clone)]
pub struct ThawDisk {
    /// The disk at its own path: the main lab's overlay.
    pub top: DiskFacts,
    /// The frozen files its chain runs through, nearest first, each with its
    /// generation. Empty for a disk that is not on the freeze at all (a
    /// machine built after the last freeze).
    pub layers: Vec<(u32, DiskFacts)>,
}

#[derive(Debug, Clone)]
pub struct ThawMachine {
    pub name: String,
    pub stopped: bool,
    pub disks: Vec<ThawDisk>,
    /// The machine's records that say the capture was online.
    pub online_records: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct ThawInput {
    pub lab_local: PathBuf,
    pub in_linked_worktree: bool,
    pub machines: Vec<ThawMachine>,
    /// Each generation's freeze point.
    pub points: BTreeMap<u32, String>,
    /// Drop the online snapshots no thaw can carry, rather than refuse.
    pub drop_online: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ThawPlan {
    pub plan: Plan,
    /// Per machine, the snapshots that exist once the thaw is done: the
    /// lab's records keep these and lose the rest.
    pub survivors: BTreeMap<String, BTreeSet<String>>,
    /// Per machine, the online snapshots `drop_online` gave up.
    pub dropped: BTreeMap<String, Vec<String>>,
}

/// Plan `vmlab worktree thaw` (§21.3), or refuse it. The caller has already
/// refused while any worktree depends on a generation.
///
/// Each disk's chain is collapsed into its oldest frozen file, which holds
/// every snapshot taken before the first freeze and becomes the disk again.
/// Every layer above it is committed down, but `commit` moves only a layer's
/// active state; its snapshots are carried one by one, so every offline
/// snapshot the main lab took after a freeze survives the thaw. The main
/// lab's current state is carried last, through [`HOLD_SNAPSHOT`].
pub fn plan_thaw(input: &ThawInput) -> Result<ThawPlan> {
    if input.in_linked_worktree {
        return Err(refuse(
            "this lab is in a linked git worktree; only the main checkout's lab thaws — run \
             `vmlab worktree thaw` there",
        ));
    }
    let running: Vec<&str> = input
        .machines
        .iter()
        .filter(|m| !m.stopped)
        .map(|m| m.name.as_str())
        .collect();
    if !running.is_empty() {
        return Err(refuse(format!(
            "a thaw needs the lab stopped, and {} {} running — `vmlab down` first",
            running.join(", "),
            if running.len() == 1 { "is" } else { "are" }
        )));
    }

    let mut out = ThawPlan::default();
    let mut active = Active::default();
    for m in &input.machines {
        // Online snapshots stored above a disk's base, which nothing carries.
        let mut stranded: BTreeSet<String> = BTreeSet::new();
        for d in &m.disks {
            let Some(((_, _), above)) = d.layers.split_last() else {
                continue;
            };
            for f in above.iter().map(|(_, f)| f).chain([&d.top]) {
                stranded.extend(f.online().map(str::to_string));
                stranded.extend(
                    f.snapshots
                        .iter()
                        .filter(|s| m.online_records.contains(&s.name))
                        .map(|s| s.name.clone()),
                );
            }
        }
        if !stranded.is_empty() && !input.drop_online {
            return Err(refuse(format!(
                "\"{}\": the online snapshots {} were taken after a freeze, and their machine \
                 state cannot be carried into the thawed disk — `vmlab snapshot delete` the \
                 ones taken since the last freeze, or pass --drop-online-snapshots to lose them",
                m.name,
                stranded.iter().cloned().collect::<Vec<_>>().join(", ")
            )));
        }

        let mut survivors: Option<BTreeSet<String>> = None;
        for d in &m.disks {
            let names = thaw_disk(&mut out.plan, &mut active, input, m, d, &stranded)?;
            survivors.get_or_insert(names);
        }
        // A machine with no disk (never built) keeps its records as they are.
        if let Some(survivors) = survivors {
            out.survivors.insert(m.name.clone(), survivors);
        }
        if !stranded.is_empty() {
            out.dropped
                .insert(m.name.clone(), stranded.into_iter().collect());
        }
    }
    out.plan.cleanup.push(Step::Remove {
        path: super::freeze_root(&input.lab_local),
    });
    Ok(out)
}

/// Collapse one disk's chain into its base and put the base back at the
/// disk's path. Answers with the snapshots the thawed disk holds.
fn thaw_disk(
    plan: &mut Plan,
    active: &mut Active,
    input: &ThawInput,
    m: &ThawMachine,
    d: &ThawDisk,
    stranded: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let top = &d.top.path;
    let names = |f: &DiskFacts| -> BTreeSet<String> {
        f.snapshots
            .iter()
            .map(|s| s.name.clone())
            .filter(|n| n != HOLD_SNAPSHOT)
            .collect()
    };
    let Some(((base_gen, base), above)) = d.layers.split_last() else {
        return Ok(names(&d.top));
    };
    let point = |g: u32| -> Result<String> {
        input.points.get(&g).cloned().ok_or_else(|| {
            refuse(format!(
                "generation {g} records no freeze point, so {} cannot be thawed",
                top.display()
            ))
        })
    };
    if d.top.has(HOLD_SNAPSHOT) || d.layers.iter().any(|(_, f)| f.has(HOLD_SNAPSHOT)) {
        return Err(refuse(format!(
            "{}'s chain still holds the snapshot \"{HOLD_SNAPSHOT}\" an interrupted freeze or \
             thaw left behind",
            top.display()
        )));
    }

    let b = &base.path;
    let mut held = names(base);
    plan.push(
        Step::Writable { path: b.clone() },
        vec![Step::ReadOnly { path: b.clone() }],
    );
    let mut base_point = point(*base_gen)?;
    active.set(b, Some(&base_point));
    // The layers above the base, bottom up, then the main lab's own overlay.
    let layers = above
        .iter()
        .rev()
        .map(|(g, f)| (Some(*g), f))
        .chain([(None, &d.top)]);
    for (generation, layer) in layers {
        let l = &layer.path;
        match generation {
            Some(g) => {
                plan.push(
                    Step::Writable { path: l.clone() },
                    vec![Step::ReadOnly { path: l.clone() }],
                );
                active.set(l, Some(&point(g)?));
            }
            None => active.create(plan, l, HOLD_SNAPSHOT),
        }
        let backing = layer.backing.clone().unwrap_or_default();
        if &backing != b {
            plan.push(
                Step::Rebase {
                    disk: l.clone(),
                    backing: b.clone(),
                },
                vec![Step::Rebase {
                    disk: l.clone(),
                    backing,
                }],
            );
        }
        for s in &layer.snapshots {
            let name = &s.name;
            if name == HOLD_SNAPSHOT || stranded.contains(name) {
                continue;
            }
            // The empty mark a freeze-point restore leaves is the base's own
            // state, which the base already holds under that name.
            if *name == base_point && held.contains(name) {
                continue;
            }
            if held.contains(name) {
                return Err(refuse(format!(
                    "\"{}\": snapshot \"{name}\" is stored twice in {}'s chain, so the thaw \
                     cannot tell which to keep",
                    m.name,
                    top.display()
                )));
            }
            active.apply(plan, b, &base_point);
            active.apply(plan, l, name);
            active.commit(plan, l, b);
            active.create(plan, b, name);
            held.insert(name.clone());
        }
        match generation {
            // A frozen layer's active state is its own freeze point, now in
            // the base: the next layer up was written against it.
            Some(g) => {
                let own = point(g)?;
                if !held.contains(&own) {
                    return Err(refuse(format!(
                        "{} does not hold generation {g}'s freeze point \"{own}\"",
                        l.display()
                    )));
                }
                active.apply(plan, b, &own);
                base_point = own;
            }
            None => {
                active.apply(plan, b, &base_point);
                active.apply(plan, l, HOLD_SNAPSHOT);
                active.commit(plan, l, b);
            }
        }
    }
    let old = beside(top, ".thaw-old");
    plan.push(
        Step::Rename {
            from: top.clone(),
            to: old.clone(),
        },
        vec![Step::Rename {
            from: old.clone(),
            to: top.clone(),
        }],
    );
    plan.push(
        Step::Rename {
            from: b.clone(),
            to: top.clone(),
        },
        vec![Step::Rename {
            from: top.clone(),
            to: b.clone(),
        }],
    );
    plan.cleanup.push(Step::Remove { path: old });
    Ok(held)
}

/// Clear every record's freeze marks after a thaw, and drop the records of
/// snapshots the thawed disks no longer hold. Answers with what was dropped,
/// as `(machine, snapshot)`.
pub fn thaw_records(
    state: &mut LabState,
    survivors: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<(String, String)> {
    let mut dropped = Vec::new();
    for (machine, m) in state.machines.iter_mut() {
        if let Some(keep) = survivors.get(machine) {
            m.snapshots.retain(|name, _| {
                let kept = keep.contains(name);
                if !kept {
                    dropped.push((machine.clone(), name.clone()));
                }
                kept
            });
        }
        for record in m.snapshots.values_mut() {
            record.freeze_point = None;
            record.frozen_in = None;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::state::SnapshotRecord;

    fn snap(name: &str) -> InternalSnapshot {
        InternalSnapshot {
            name: name.into(),
            vm_state_size: 0,
        }
    }

    fn facts(path: &str, backing: Option<&str>, snaps: &[&str]) -> DiskFacts {
        DiskFacts {
            path: PathBuf::from(path),
            backing: backing.map(PathBuf::from),
            backing_relative: false,
            snapshots: snaps.iter().map(|s| snap(s)).collect(),
        }
    }

    fn record(online: bool) -> SnapshotRecord {
        SnapshotRecord {
            online,
            taken_at: chrono::Utc::now(),
            image_digest: None,
            freeze_point: None,
            frozen_in: None,
            hypervisor: Default::default(),
        }
    }

    fn machine(name: &str, stopped: bool, rec: Option<SnapshotRecord>) -> FreezeMachine {
        let disk = format!("/l/.vmlab/vms/{name}/disk0.qcow2");
        FreezeMachine {
            name: name.into(),
            stopped,
            local_dir: PathBuf::from(format!("/l/.vmlab/vms/{name}")),
            disks: vec![(
                PathBuf::from(&disk),
                Some(facts(&disk, Some("/store/t.qcow2"), &["base"])),
            )],
            record: rec,
            state_json: b"{}".to_vec(),
        }
    }

    fn input(machines: Vec<FreezeMachine>) -> FreezeInput {
        FreezeInput {
            lab_local: PathBuf::from("/l/.vmlab"),
            in_linked_worktree: false,
            generation: 1,
            snapshot: "base".into(),
            machines,
            volumes: vec![],
        }
    }

    fn refusal(r: Result<impl std::fmt::Debug>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    #[test]
    fn freeze_rebases_each_disk_onto_its_frozen_file() {
        let carried = BTreeMap::from([("dc".to_string(), vec!["OVMF_VARS.fd".to_string()])]);
        let plan = plan_freeze(
            &input(vec![machine("dc", true, Some(record(false)))]),
            &carried,
        )
        .unwrap();
        let disk = PathBuf::from("/l/.vmlab/vms/dc/disk0.qcow2");
        let work = PathBuf::from("/l/.vmlab/vms/dc/disk0.qcow2.freeze-work");
        let frozen = PathBuf::from("/l/.vmlab/freeze/1/dc/disk0.qcow2");
        assert_eq!(
            plan.forward(),
            vec![
                &Step::CreateDir {
                    path: "/l/.vmlab/freeze/1".into()
                },
                &Step::CreateDir {
                    path: "/l/.vmlab/freeze/1/dc".into()
                },
                &Step::Copy {
                    from: disk.clone(),
                    to: work.clone()
                },
                &Step::SnapshotApply {
                    disk: disk.clone(),
                    name: "base".into()
                },
                &Step::Rename {
                    from: disk.clone(),
                    to: frozen.clone()
                },
                &Step::ReadOnly {
                    path: frozen.clone()
                },
                &Step::Overlay {
                    backing: work.clone(),
                    dest: disk.clone()
                },
                &Step::RebaseSafe {
                    disk: disk.clone(),
                    backing: frozen.clone()
                },
                &Step::Copy {
                    from: "/l/.vmlab/vms/dc/OVMF_VARS.fd".into(),
                    to: "/l/.vmlab/freeze/1/dc/OVMF_VARS.fd".into()
                },
                &Step::WriteFile {
                    path: "/l/.vmlab/freeze/1/dc/state.json".into(),
                    bytes: b"{}".to_vec()
                },
            ]
        );
        // The rewind is undone by the copy, which is the disk as it was; the
        // copy goes only once the freeze is committed.
        assert_eq!(
            plan.steps[3].undo,
            vec![Step::Rename {
                from: work.clone(),
                to: disk
            }]
        );
        assert_eq!(plan.cleanup, vec![Step::Remove { path: work }]);
        // The frozen file is read-only before anything layers onto it.
        assert_eq!(plan.steps[5].undo, vec![Step::Writable { path: frozen }]);
    }

    #[test]
    fn freeze_copies_named_volumes() {
        let mut i = input(vec![machine("dc", true, Some(record(false)))]);
        i.volumes = vec!["data".into()];
        let plan = plan_freeze(&i, &BTreeMap::new()).unwrap();
        assert!(plan.forward().contains(&&Step::Copy {
            from: "/l/.vmlab/volumes/data".into(),
            to: "/l/.vmlab/freeze/1/volumes/data".into()
        }));
    }

    #[test]
    fn freeze_refuses_what_it_must() {
        let none = BTreeMap::new();
        let mut wt = input(vec![machine("dc", true, Some(record(false)))]);
        wt.in_linked_worktree = true;
        assert!(refusal(plan_freeze(&wt, &none)).contains("main checkout"));
        let running = input(vec![
            machine("dc", false, Some(record(false))),
            machine("ws", true, Some(record(false))),
        ]);
        assert!(refusal(plan_freeze(&running, &none)).contains("dc is running"));
        let missing = input(vec![machine("dc", true, None)]);
        assert!(refusal(plan_freeze(&missing, &none)).contains("has no snapshot"));
        let online = input(vec![machine("dc", true, Some(record(true)))]);
        let why = refusal(plan_freeze(&online, &none));
        assert!(why.contains("online") && why.contains("power cut"), "{why}");
        let mut locked = record(false);
        locked.frozen_in = Some(1);
        let locked = input(vec![machine("dc", true, Some(locked))]);
        assert!(refusal(plan_freeze(&locked, &none)).contains("locked in generation 1"));
        let mut unbuilt = machine("dc", true, Some(record(false)));
        unbuilt.disks[0].1 = None;
        assert!(refusal(plan_freeze(&input(vec![unbuilt]), &none)).contains("never been built"));
        let mut held = machine("dc", true, Some(record(false)));
        held.disks[0]
            .1
            .as_mut()
            .unwrap()
            .snapshots
            .push(snap(HOLD_SNAPSHOT));
        assert!(refusal(plan_freeze(&input(vec![held]), &none)).contains("interrupted"));
    }

    #[test]
    fn marking_makes_a_freeze_point_and_locks_the_rest_once() {
        let mut state = LabState::default();
        let m = state.machine_mut("dc");
        m.snapshots.insert("base".into(), record(false));
        m.snapshots.insert("early".into(), record(true));
        mark_frozen(&mut state, "base", 1);
        let s = &state.machines["dc"].snapshots;
        assert_eq!(s["base"].freeze_point, Some(1));
        assert_eq!(s["base"].frozen_in, None);
        assert_eq!(s["early"].frozen_in, Some(1));
        // Refreezing locks the old freeze point and leaves earlier locks be.
        state
            .machine_mut("dc")
            .snapshots
            .insert("later".into(), record(false));
        mark_frozen(&mut state, "later", 2);
        let s = &state.machines["dc"].snapshots;
        assert_eq!(s["base"].frozen_in, Some(2));
        assert_eq!(s["early"].frozen_in, Some(1));
        assert_eq!(s["later"].freeze_point, Some(2));
    }

    #[test]
    fn frozen_file_shape_is_checked_not_its_prefix() {
        let disk = Path::new("/wt/.vmlab/vms/dc/disk0.qcow2");
        assert!(is_frozen_file(
            Path::new("/main/.vmlab/freeze/2/dc/disk0.qcow2"),
            disk,
            "dc",
            2
        ));
        for wrong in [
            "/main/.vmlab/freeze/1/dc/disk0.qcow2",
            "/main/.vmlab/freeze/2/ws/disk0.qcow2",
            "/main/.vmlab/freeze/2/dc/disk-data.qcow2",
            "/store/2/dc/disk0.qcow2",
        ] {
            assert!(!is_frozen_file(Path::new(wrong), disk, "dc", 2), "{wrong}");
        }
    }

    const FROZEN: &str = "/m/.vmlab/freeze/1/dc/disk0.qcow2";
    const DISK: &str = "/w/.vmlab/vms/dc/disk0.qcow2";

    #[test]
    fn point_restore_on_a_marked_overlay_is_an_apply() {
        let d = facts(DISK, Some(FROZEN), &["base", "after"]);
        let plan = plan_point_restore("dc", "base", 1, &[d]).unwrap();
        let disk = PathBuf::from(DISK);
        assert_eq!(
            plan.forward(),
            vec![
                &Step::SnapshotCreate {
                    disk: disk.clone(),
                    name: HOLD_SNAPSHOT.into()
                },
                &Step::SnapshotApply {
                    disk: disk.clone(),
                    name: "base".into()
                },
            ]
        );
        assert_eq!(
            plan.cleanup,
            vec![Step::SnapshotDelete {
                disk,
                name: HOLD_SNAPSHOT.into()
            }]
        );
    }

    #[test]
    fn point_restore_replaces_the_overlay_and_carries_its_snapshots() {
        let d = facts(DISK, Some(FROZEN), &["after"]);
        let plan = plan_point_restore("dc", "base", 1, &[d]).unwrap();
        let disk = PathBuf::from(DISK);
        let fresh = PathBuf::from(format!("{DISK}.restore"));
        let carry = PathBuf::from(format!("{DISK}.carry"));
        let mut want = vec![];
        for step in plan.forward() {
            want.push(match step {
                Step::SnapshotCreate { name, .. } => format!("c {name}"),
                Step::SnapshotApply { disk, name } => {
                    format!("a {name} {}", file_name(disk).to_string_lossy())
                }
                Step::Overlay { backing, dest } => {
                    assert_eq!((backing.as_path(), dest), (Path::new(FROZEN), &fresh));
                    "overlay".into()
                }
                Step::Export {
                    src,
                    snapshot,
                    backing,
                    dest,
                } => {
                    assert_eq!((src, backing, dest), (&disk, &fresh, &carry));
                    format!("export {snapshot}")
                }
                Step::Commit { top } => {
                    assert_eq!(top, &carry);
                    "commit".into()
                }
                Step::Remove { .. } => "rm".into(),
                Step::Rename { to, .. } => format!("mv {}", file_name(to).to_string_lossy()),
                other => panic!("unexpected {other}"),
            });
        }
        assert_eq!(
            want,
            [
                format!("c {HOLD_SNAPSHOT}"),
                "overlay".into(),
                "c base".into(),
                "export after".into(),
                "commit".into(),
                "rm".into(),
                "c after".into(),
                "a base disk0.qcow2.restore".into(),
                "mv disk0.qcow2.old".into(),
                "mv disk0.qcow2".into(),
            ]
        );
        assert_eq!(
            plan.cleanup,
            vec![Step::Remove {
                path: PathBuf::from(format!("{DISK}.old"))
            }]
        );
    }

    #[test]
    fn point_restore_refuses_a_foreign_backing_and_stranded_online_snapshots() {
        let d = facts(DISK, Some("/store/t.qcow2"), &[]);
        assert!(refusal(plan_point_restore("dc", "base", 1, &[d])).contains("not an overlay"));
        let d = facts(DISK, None, &[]);
        assert!(refusal(plan_point_restore("dc", "base", 1, &[d])).contains("no backing"));
        let mut d = facts(DISK, Some(FROZEN), &[]);
        d.snapshots.push(InternalSnapshot {
            name: "live".into(),
            vm_state_size: 4096,
        });
        assert!(refusal(plan_point_restore("dc", "base", 1, &[d])).contains("live"));
    }

    fn thaw_input(disks: Vec<ThawDisk>) -> ThawInput {
        ThawInput {
            lab_local: PathBuf::from("/m/.vmlab"),
            in_linked_worktree: false,
            machines: vec![ThawMachine {
                name: "dc".into(),
                stopped: true,
                disks,
                online_records: BTreeSet::new(),
            }],
            points: BTreeMap::from([(1, "base".to_string()), (2, "two".to_string())]),
            drop_online: false,
        }
    }

    const MAIN: &str = "/m/.vmlab/vms/dc/disk0.qcow2";
    const G1: &str = "/m/.vmlab/freeze/1/dc/disk0.qcow2";
    const G2: &str = "/m/.vmlab/freeze/2/dc/disk0.qcow2";

    /// The forward steps, abbreviated: which file each touches, by role.
    fn sketch(plan: &Plan) -> Vec<String> {
        let role = |p: &Path| match p.to_str().unwrap() {
            MAIN => "main",
            G1 => "g1",
            G2 => "g2",
            other if other.ends_with(".thaw-old") => "old",
            other => panic!("unexpected path {other}"),
        };
        plan.forward()
            .into_iter()
            .map(|s| match s {
                Step::Writable { path } => format!("rw {}", role(path)),
                Step::SnapshotCreate { disk, name } => format!("c {} {name}", role(disk)),
                Step::SnapshotApply { disk, name } => format!("a {} {name}", role(disk)),
                Step::Commit { top } => format!("commit {}", role(top)),
                Step::Rebase { disk, backing } => {
                    format!("rebase {} {}", role(disk), role(backing))
                }
                Step::Rename { from, to } => format!("mv {} {}", role(from), role(to)),
                other => panic!("unexpected {other}"),
            })
            .collect()
    }

    #[test]
    fn thaw_carries_post_freeze_snapshots_and_the_current_state_down() {
        let d = ThawDisk {
            top: facts(MAIN, Some(G1), &["after"]),
            layers: vec![(1, facts(G1, Some("/store/t.qcow2"), &["early", "base"]))],
        };
        let out = plan_thaw(&thaw_input(vec![d])).unwrap();
        let h = HOLD_SNAPSHOT;
        assert_eq!(
            sketch(&out.plan),
            [
                "rw g1".to_string(),
                format!("c main {h}"),
                "a main after".into(),
                "commit main".into(),
                "c g1 after".into(),
                "a g1 base".into(),
                format!("a main {h}"),
                "commit main".into(),
                "mv main old".into(),
                "mv g1 main".into(),
            ]
        );
        assert_eq!(
            out.survivors["dc"],
            BTreeSet::from(["early".into(), "base".into(), "after".into()])
        );
        assert_eq!(
            out.plan.cleanup.last(),
            Some(&Step::Remove {
                path: "/m/.vmlab/freeze".into()
            })
        );
    }

    #[test]
    fn thaw_collapses_a_refrozen_chain_bottom_up() {
        // Generation 2 froze the overlay holding "mid" and "two"; the main
        // lab's overlay then restored the freeze point once, so holds its
        // empty mark "two".
        let d = ThawDisk {
            top: facts(MAIN, Some(G2), &["two"]),
            layers: vec![
                (2, facts(G2, Some(G1), &["mid", "two"])),
                (1, facts(G1, Some("/store/t.qcow2"), &["base"])),
            ],
        };
        let out = plan_thaw(&thaw_input(vec![d])).unwrap();
        let h = HOLD_SNAPSHOT;
        assert_eq!(
            sketch(&out.plan),
            [
                "rw g1".to_string(),
                "rw g2".into(),
                "a g2 mid".into(),
                "commit g2".into(),
                "c g1 mid".into(),
                "a g1 base".into(),
                "a g2 two".into(),
                "commit g2".into(),
                "c g1 two".into(),
                format!("c main {h}"),
                "rebase main g1".into(),
                "commit main".into(),
                "mv main old".into(),
                "mv g1 main".into(),
            ]
        );
        assert_eq!(
            out.survivors["dc"],
            BTreeSet::from(["base".into(), "mid".into(), "two".into()])
        );
    }

    #[test]
    fn every_thaw_step_that_moves_an_active_state_is_undone_by_a_named_snapshot() {
        let d = ThawDisk {
            top: facts(MAIN, Some(G2), &["late"]),
            layers: vec![
                (2, facts(G2, Some(G1), &["mid", "two"])),
                (1, facts(G1, Some("/store/t.qcow2"), &["base"])),
            ],
        };
        let out = plan_thaw(&thaw_input(vec![d])).unwrap();
        for p in &out.plan.steps {
            if matches!(p.step, Step::Commit { .. } | Step::SnapshotApply { .. }) {
                assert!(
                    matches!(p.undo.as_slice(), [Step::SnapshotApply { .. }]),
                    "{} has undo {:?}",
                    p.step,
                    p.undo
                );
            }
        }
    }

    #[test]
    fn thaw_refuses_stranded_online_snapshots_unless_told_to_drop_them() {
        let mut top = facts(MAIN, Some(G1), &[]);
        top.snapshots.push(InternalSnapshot {
            name: "live".into(),
            vm_state_size: 1 << 20,
        });
        let d = ThawDisk {
            top,
            layers: vec![(1, facts(G1, Some("/store/t.qcow2"), &["base", "old-live"]))],
        };
        let mut i = thaw_input(vec![d]);
        // An online snapshot in the base survives: it never moves.
        i.machines[0].online_records.insert("old-live".into());
        let why = refusal(plan_thaw(&i));
        assert!(why.contains("live") && !why.contains("old-live"), "{why}");
        i.drop_online = true;
        let out = plan_thaw(&i).unwrap();
        assert_eq!(out.dropped["dc"], vec!["live".to_string()]);
        assert!(out.survivors["dc"].contains("old-live"));
        assert!(!out.survivors["dc"].contains("live"));
    }

    #[test]
    fn thaw_refuses_running_machines_and_worktrees() {
        let mut i = thaw_input(vec![]);
        i.machines[0].stopped = false;
        assert!(refusal(plan_thaw(&i)).contains("dc is running"));
        let mut i = thaw_input(vec![]);
        i.in_linked_worktree = true;
        assert!(refusal(plan_thaw(&i)).contains("main checkout"));
    }

    #[test]
    fn thawed_records_lose_their_marks_and_whatever_did_not_survive() {
        let mut state = LabState::default();
        let m = state.machine_mut("dc");
        let mut base = record(false);
        base.freeze_point = Some(1);
        let mut early = record(false);
        early.frozen_in = Some(1);
        m.snapshots.insert("base".into(), base);
        m.snapshots.insert("early".into(), early);
        m.snapshots.insert("live".into(), record(true));
        let survivors = BTreeMap::from([(
            "dc".to_string(),
            BTreeSet::from(["base".to_string(), "early".to_string()]),
        )]);
        let dropped = thaw_records(&mut state, &survivors);
        assert_eq!(dropped, vec![("dc".to_string(), "live".to_string())]);
        let s = &state.machines["dc"].snapshots;
        assert!(
            s.values()
                .all(|r| r.freeze_point.is_none() && r.frozen_in.is_none())
        );
        assert_eq!(s.len(), 2);
    }
}
