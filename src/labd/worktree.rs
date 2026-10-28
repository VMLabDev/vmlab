//! Worktree labs, the lab daemon's half (PRD §21, ADR-0019): `vmlab worktree
//! freeze` and `thaw`, restoring a freeze point, and the guards that keep a
//! frozen file from being written or a depended-on freeze from being removed.
//!
//! These run here rather than in the CLI for the reason every snapshot verb
//! does: the lab daemon owns `state.json` and knows each machine's power
//! state and disks, and a freeze rewrites both. What the plans are made of —
//! the facts, the refusals, the steps — lives in [`crate::freeze`]; this file
//! gathers the facts, holds the state lock across the run and commits the
//! records only once every disk is done.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::lab::LabRuntime;
use super::machine::Machine;
use super::state::LabState;
use super::vm::PowerState;
use crate::freeze::inspect::{self, disk_facts};
use crate::freeze::plan::{self, CARRIED, FreezeInput, FreezeMachine, ThawDisk, ThawInput};
use crate::freeze::steps::{execute, rolled_back};
use crate::freeze::{self as fz, WorktreeConfig};
use crate::proto::CommandError;

/// How long a freeze-point restore waits for a running machine to stop.
const STOP_WAIT: Duration = Duration::from_secs(60);

/// Why a locked snapshot (§21.3) refuses `verb`.
pub(crate) fn locked_refusal(machine: &str, snap: &str, generation: u32, verb: &str) -> String {
    format!(
        "\"{machine}\": snapshot \"{snap}\" was taken before generation {generation} was \
         frozen, so it lives in a frozen file that worktree labs read — it is locked, and \
         {verb} it would write that file. `vmlab worktree thaw` unlocks it once no worktree \
         uses the freeze"
    )
}

impl LabRuntime {
    /// Whether this lab is in a linked git worktree rather than the main
    /// checkout.
    fn in_linked_worktree(&self) -> bool {
        fz::main_lab_root(&self.root).is_some()
    }

    /// `vmlab worktree freeze <snapshot>` (§21.1).
    pub async fn worktree_freeze(&self, snapshot: &str) -> Result<Value> {
        super::vm::validate_snapshot_name(snapshot)?;
        let mut state = self.state.lock().await;
        let previous = WorktreeConfig::load(&self.lab_local)?;
        let generation = fz::generations(&self.lab_local)
            .last()
            .copied()
            .into_iter()
            .chain(previous.as_ref().map(|c| c.generation))
            .max()
            .unwrap_or(0)
            + 1;
        let mut marked = state.clone();
        plan::mark_frozen(&mut marked, snapshot, generation);

        let mut machines = Vec::new();
        let mut carried = BTreeMap::new();
        for m in self.machines() {
            let name = m.name().to_string();
            let mut disks = Vec::new();
            for path in m.disk_paths() {
                let facts = disk_facts(&path).await?;
                disks.push((path, facts));
            }
            let present: Vec<String> = CARRIED
                .iter()
                .filter(|f| m.local_dir().join(f).exists())
                .map(|f| f.to_string())
                .collect();
            carried.insert(name.clone(), present);
            machines.push(FreezeMachine {
                record: state
                    .machines
                    .get(&name)
                    .and_then(|s| s.snapshots.get(snapshot))
                    .cloned(),
                stopped: m.state().await == PowerState::Stopped,
                local_dir: m.local_dir().to_path_buf(),
                state_json: serde_json::to_vec_pretty(
                    &marked.machines.get(&name).cloned().unwrap_or_default(),
                )?,
                name,
                disks,
            });
        }
        let volumes: Vec<String> = std::fs::read_dir(self.lab_local.join(fz::VOLUMES_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        let input = FreezeInput {
            lab_local: self.lab_local.clone(),
            in_linked_worktree: self.in_linked_worktree(),
            generation,
            snapshot: snapshot.to_string(),
            machines,
            volumes,
        };
        let plan = plan::plan_freeze(&input, &carried)?;
        let applied = execute(plan).await?;

        // The records, then `worktree.json` last: its presence is what makes
        // the family frozen, so it appears only once everything under it is.
        let config = WorktreeConfig {
            generation,
            snapshot: snapshot.to_string(),
        };
        let committed = marked
            .save(&self.lab_local)
            .and_then(|()| config.save(&self.lab_local));
        if let Err(e) = committed {
            let failed = applied.rollback().await;
            let _ = state.save(&self.lab_local);
            let _ = match &previous {
                Some(p) => p.save(&self.lab_local),
                None => WorktreeConfig::remove(&self.lab_local),
            };
            return Err(rolled_back(e, &failed));
        }
        *state = marked;
        let warnings = applied.finish().await;
        let names: Vec<String> = input.machines.iter().map(|m| m.name.clone()).collect();
        self.events.emit(
            "worktree.frozen",
            json!({"generation": generation, "snapshot": snapshot, "machines": names}),
        );
        Ok(json!({
            "generation": generation,
            "snapshot": snapshot,
            "machines": names,
            "warnings": warnings,
        }))
    }

    /// `vmlab worktree thaw` (§21.3).
    pub async fn worktree_thaw(&self, drop_online: bool) -> Result<Value> {
        if self.in_linked_worktree() {
            return Err(CommandError::conflict(
                "this lab is in a linked git worktree; only the main checkout's lab thaws — \
                 run `vmlab worktree thaw` there",
            )
            .into());
        }
        let freeze_root = fz::freeze_root(&self.lab_local);
        if WorktreeConfig::load(&self.lab_local)?.is_none() && !freeze_root.exists() {
            return Err(CommandError::conflict(
                "this lab is not frozen, so there is nothing to thaw",
            )
            .into());
        }
        self.refuse_while_depended_on("cannot thaw").await?;

        let mut state = self.state.lock().await;
        let mut points = inspect::generation_points(&self.lab_local);
        for m in state.machines.values() {
            for (name, r) in &m.snapshots {
                if let Some(g) = r.freeze_point {
                    points.entry(g).or_insert_with(|| name.clone());
                }
            }
        }
        let mut machines = Vec::new();
        for m in self.machines() {
            let mut disks = Vec::new();
            for path in m.disk_paths() {
                let mut chain = inspect::chain(&path).await.into_iter();
                let Some(top) = chain.next() else { continue };
                let layers = chain
                    .map_while(|f| inspect::generation_of(&f.path, &freeze_root).map(|g| (g, f)))
                    .collect();
                disks.push(ThawDisk { top, layers });
            }
            let online_records = state
                .machines
                .get(m.name())
                .into_iter()
                .flat_map(|s| s.snapshots.iter())
                .filter(|(_, r)| r.online)
                .map(|(n, _)| n.clone())
                .collect();
            machines.push(plan::ThawMachine {
                name: m.name().to_string(),
                stopped: m.state().await == PowerState::Stopped,
                disks,
                online_records,
            });
        }
        let input = ThawInput {
            lab_local: self.lab_local.clone(),
            in_linked_worktree: false,
            machines,
            points,
            drop_online,
        };
        let thaw = plan::plan_thaw(&input)?;
        let applied = execute(thaw.plan).await?;

        let mut thawed = state.clone();
        let removed = plan::thaw_records(&mut thawed, &thaw.survivors);
        if let Err(e) = thawed.save(&self.lab_local) {
            let failed = applied.rollback().await;
            let _ = state.save(&self.lab_local);
            return Err(rolled_back(e, &failed));
        }
        *state = thawed;
        let mut warnings = Vec::new();
        if let Err(e) = WorktreeConfig::remove(&self.lab_local) {
            warnings.push(format!("{e:#}"));
        }
        warnings.extend(applied.finish().await);
        let removed: Vec<Value> = removed
            .into_iter()
            .map(|(machine, name)| json!({"machine": machine, "name": name}))
            .collect();
        self.events
            .emit("worktree.thawed", json!({"dropped": removed.clone()}));
        Ok(json!({"dropped": removed, "warnings": warnings}))
    }

    /// Refuse `what` while a linked worktree's disk backs onto any of this
    /// lab's generations (§21.5). Nothing to check in a linked worktree or a
    /// lab that was never frozen.
    pub(crate) async fn refuse_while_depended_on(&self, what: &str) -> Result<()> {
        if self.in_linked_worktree() || !fz::freeze_root(&self.lab_local).exists() {
            return Ok(());
        }
        let deps = inspect::dependants(&self.root).await;
        if deps.is_empty() {
            return Ok(());
        }
        Err(CommandError::conflict(inspect::dependants_refusal(what, &deps)).into())
    }

    /// Restore the freeze point of generation `generation` on `m` (§21.3):
    /// stop it like any offline restore, then empty its overlays down to the
    /// frozen files they back onto. The same in the main lab and a worktree.
    pub(crate) async fn restore_freeze_point(
        &self,
        m: &Arc<dyn Machine>,
        snap: &str,
        generation: u32,
    ) -> Result<()> {
        if m.state().await != PowerState::Stopped {
            m.stop(false).await?;
            m.wait_state(PowerState::Stopped, STOP_WAIT).await?;
        }
        let mut disks = Vec::new();
        for path in m.disk_paths() {
            let facts = disk_facts(&path).await?.ok_or_else(|| {
                CommandError::conflict(format!(
                    "\"{}\" has no disk {} to restore",
                    m.name(),
                    path.display()
                ))
            })?;
            disks.push(facts);
        }
        let plan = plan::plan_point_restore(m.name(), snap, generation, &disks)?;
        let applied = execute(plan).await?;
        for warning in applied.finish().await {
            tracing::warn!("{}: {warning}", m.name());
        }
        Ok(())
    }
}

/// The refusal for taking a snapshot under a name a freeze owns: a locked
/// record's, or a freeze point's. Taking it again would replace the record
/// that says where the snapshot really lives.
pub(crate) fn take_refusal(state: &LabState, machine: &str, snap: &str) -> Option<String> {
    let record = state.machines.get(machine)?.snapshots.get(snap)?;
    if let Some(g) = record.frozen_in {
        return Some(locked_refusal(machine, snap, g, "replacing"));
    }
    record.freeze_point.map(|g| {
        format!(
            "\"{machine}\": \"{snap}\" is generation {g}'s freeze point, which every lab of \
             the family restores — take the new snapshot under another name"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::state::SnapshotRecord;

    fn record() -> SnapshotRecord {
        SnapshotRecord {
            online: false,
            taken_at: chrono::Utc::now(),
            image_digest: None,
            freeze_point: None,
            frozen_in: None,
        }
    }

    #[test]
    fn a_freeze_owned_name_cannot_be_taken_again() {
        let mut state = LabState::default();
        let m = state.machine_mut("dc");
        let mut locked = record();
        locked.frozen_in = Some(1);
        let mut point = record();
        point.freeze_point = Some(2);
        m.snapshots.insert("early".into(), locked);
        m.snapshots.insert("base".into(), point);
        m.snapshots.insert("plain".into(), record());
        let why = take_refusal(&state, "dc", "early").unwrap();
        assert!(why.contains("vmlab worktree thaw"), "{why}");
        let why = take_refusal(&state, "dc", "base").unwrap();
        assert!(why.contains("freeze point"), "{why}");
        assert_eq!(take_refusal(&state, "dc", "plain"), None);
        assert_eq!(take_refusal(&state, "dc", "new"), None);
    }
}
