//! Reloading an edited lab file into a running lab daemon (PRD §7.1).
//!
//! The decision is [`reload_plan`](crate::labd::reload_plan)'s; this carries
//! it out. A machine is rebuilt only while it is stopped, and only under the
//! runtime's rebuild gate, which every start holds across looking the machine
//! up and starting it — so a start never boots an instance a reload has just
//! replaced, and a reload never replaces one mid-start.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::LabRuntime;
use crate::config::LabFile;
use crate::config::model::MachineCfg;
use crate::labd::machine::Machine;
use crate::labd::machine_build::{self, BuildContext, Built};
use crate::labd::pull_ledger::PullJob;
use crate::labd::reload_plan::{self, Action, Held};
use crate::labd::vm::PowerState;
use crate::proto::CommandError;
use crate::status::{MachineReload, PendingMachine, PendingReason, ReloadReport};
use crate::sync::LockRecover;
use crate::template::TemplateStore;

/// How a reload builds a machine in a runtime whose machines are doubles.
#[cfg(test)]
pub(in crate::labd) type MachineDoubles =
    Arc<dyn Fn(MachineCfg<'_>) -> Arc<dyn Machine> + Send + Sync>;

impl LabRuntime {
    /// `lab.reload` (PRD §7.1): read the lab file afresh, plan the reload
    /// against what this daemon holds, and carry it out — or, when the file
    /// changes the network, apply nothing and say what changed.
    pub async fn reload(self: &Arc<Self>) -> Result<ReloadReport, CommandError> {
        let mut file = crate::config::load_lab_root(&self.root)
            .map_err(|e| CommandError::invalid(format!("{:?}", miette::Report::new(e))))?;
        crate::instance::adopt(&mut file, &self.name, &self.root)
            .map_err(|e| CommandError::invalid(format!("{e:#}")))?;
        self.reload_with(file).await
    }

    /// [`Self::reload`] of a lab file already loaded.
    pub(in crate::labd) async fn reload_with(
        self: &Arc<Self>,
        file: LabFile,
    ) -> Result<ReloadReport, CommandError> {
        let new = Arc::new(file);
        let _gate = self.rebuild_gate.write().await;
        let old = self.config();
        let segments: BTreeSet<String> =
            self.network.lock().await.segments.keys().cloned().collect();
        let held = self.held().await;
        let plan = {
            let borrowed: BTreeMap<String, Held<'_>> = held
                .iter()
                .filter_map(|(name, h)| {
                    Some((
                        name.clone(),
                        Held {
                            built: h.from.lab.machine(name)?,
                            live: h.live,
                            pulling: h.pulling,
                        },
                    ))
                })
                .collect();
            reload_plan::plan(&old.lab, &new.lab, &borrowed, &segments)
                .map_err(|refused| CommandError::conflict(refused.to_string()))?
        };

        // Build everything the plan builds before swapping anything in, so a
        // machine that cannot be built (a template gone from the store)
        // fails the reload with nothing applied.
        let wanted: Vec<MachineCfg<'_>> = plan
            .machines
            .iter()
            .filter(|(_, action)| matches!(action, Action::Rebuild | Action::Add))
            .map(|(name, _)| {
                new.lab
                    .machine(name)
                    .expect("the plan builds only machines the file declares")
            })
            .collect();
        let built = self
            .build_machines(&wanted)
            .await
            .map_err(|e| CommandError::failed(format!("{e:#}")))?;

        let mut report = ReloadReport {
            handlers: plan.handlers,
            ..ReloadReport::default()
        };
        {
            let net = self.network.lock().await;
            let mut set = self.machines.write().expect("machine set lock");
            let mut pulls = self.pulls.lock_recover();
            for (machine, pull) in built {
                let name = machine.machine().name().to_string();
                net.name_leases(&name, &machine.macs());
                pulls.set_pending(&name, pull);
                set.insert(machine, new.clone());
            }
            for (name, action) in &plan.machines {
                report.machines.push(match action {
                    Action::Rebuild => MachineReload::Rebuilt {
                        machine: name.clone(),
                    },
                    Action::Add => MachineReload::Added {
                        machine: name.clone(),
                    },
                    Action::Drop => {
                        set.remove(name);
                        pulls.set_pending(name, None);
                        MachineReload::Dropped {
                            machine: name.clone(),
                        }
                    }
                    Action::Pending(reason) => MachineReload::Pending(PendingMachine {
                        machine: name.clone(),
                        reason: *reason,
                    }),
                });
            }
            // Every machine left alone runs what the new file declares, and
            // is held as built from it.
            for name in held.keys() {
                if !plan.machines.contains_key(name) {
                    set.rebase(name, new.clone());
                }
            }
            *self.config.write().expect("config lock") = new.clone();
        }

        // Forwards and container ports, live, for every running machine
        // whose forwards changed (§7.1). One that is not running installs
        // its forwards when it boots.
        let live: Vec<String> = held
            .iter()
            .filter(|(name, h)| h.live && plan.forwards.contains(*name))
            .map(|(name, _)| name.clone())
            .collect();
        if !live.is_empty() {
            report.forwards = self.install_forwards(&live).await;
        }
        report.forwards.extend(self.reshare(&held).await);

        self.events.emit(
            "lab.reloaded",
            json!({
                "machines": report.machines,
                "handlers": report.handlers,
            }),
        );
        Ok(report)
    }

    /// What a reload needs of every machine this daemon holds: the file it
    /// was built from, whether it runs, whether its download does.
    async fn held(&self) -> BTreeMap<String, HeldMachine> {
        let machines: Vec<(Arc<dyn Machine>, Arc<LabFile>)> = {
            let set = self.machines.read().expect("machine set lock");
            set.all()
                .into_iter()
                .filter_map(|m| {
                    let from = set.built_from(m.name())?;
                    Some((m, from))
                })
                .collect()
        };
        let mut held = BTreeMap::new();
        for (m, from) in machines {
            let live = m.state().await != PowerState::Stopped;
            let pulling = self.pulls.lock_recover().is_active(m.name());
            held.insert(
                m.name().to_string(),
                HeldMachine {
                    from,
                    live,
                    pulling,
                },
            );
        }
        held
    }

    /// Build `cfgs`, against the network and freeze this daemon assembled,
    /// persisting the MACs and pins they settle on. Fails naming the first
    /// machine that cannot be built.
    async fn build_machines(
        &self,
        cfgs: &[MachineCfg<'_>],
    ) -> Result<Vec<(Built, Option<PullJob>)>> {
        #[cfg(test)]
        {
            let double = self.doubles.lock_recover().clone();
            if let Some(double) = double {
                return Ok(cfgs
                    .iter()
                    .map(|cfg| (Built::Double(double(*cfg)), None))
                    .collect());
            }
        }
        let mtus = self.network.lock().await.mtus();
        let store = TemplateStore::new(crate::paths::template_store_dir());
        let home = std::env::var_os("HOME").map(PathBuf::from);
        // The freeze before the state, the order a clone from the freeze
        // takes them in.
        let freeze = self.freeze.lock().await;
        let mut state = self.state.lock().await;
        let cx = BuildContext {
            lab: &self.name,
            root: &self.root,
            lab_local: &self.lab_local,
            profiles: &self.profiles,
            store: &store,
            home: home.as_deref(),
            mtus: &mtus,
            freeze: freeze.as_ref(),
        };
        let mut built = Vec::new();
        for cfg in cfgs {
            built.push(
                machine_build::build(&cx, *cfg, &mut state)
                    .await
                    .with_context(|| format!("\"{}\"", cfg.name()))?,
            );
        }
        state.save(&self.lab_local)?;
        Ok(built)
    }

    /// Before `name` boots: rebuild it from the current declaration when an
    /// earlier reload left it pending and it has stopped since (PRD §7.1),
    /// or refuse when it has left the lab file. A no-op for every machine
    /// running what the file declares — which is every machine, in a daemon
    /// no reload has touched.
    pub(in crate::labd) async fn settle_pending(&self, name: &str) -> Result<()> {
        if !self.is_pending(name) {
            return Ok(());
        }
        let _gate = self.rebuild_gate.write().await;
        if !self.is_pending(name) {
            return Ok(());
        }
        let m = self.machine(name)?;
        if m.state().await != PowerState::Stopped || self.pulls.lock_recover().is_active(name) {
            return Ok(());
        }
        let config = self.config();
        let Some(cfg) = config.lab.machine(name) else {
            self.drop_machine(name);
            bail!(
                "\"{name}\" is no longer declared in vmlab.wcl, and has been dropped from the lab"
            );
        };
        let (machine, pull) = self
            .build_machines(&[cfg])
            .await?
            .pop()
            .expect("one machine built");
        self.network.lock().await.name_leases(name, &machine.macs());
        self.pulls.lock_recover().set_pending(name, pull);
        self.machines
            .write()
            .expect("machine set lock")
            .insert(machine, config.clone());
        tracing::info!("{name}: rebuilt from the new declaration before its boot");
        self.events
            .emit("machine.rebuilt", json!({"vm": name, "machine": name}));
        Ok(())
    }

    /// Whether `name` is held on a different declaration than the lab's.
    fn is_pending(&self, name: &str) -> bool {
        let config = self.config();
        let set = self.machines.read().expect("machine set lock");
        let Some(from) = set.built_from(name) else {
            return false;
        };
        if Arc::ptr_eq(&from, &config) {
            return false;
        }
        match from.lab.machine(name) {
            Some(held) => !reload_plan::runs_declared(held, config.lab.machine(name)),
            None => false,
        }
    }

    fn drop_machine(&self, name: &str) {
        self.machines
            .write()
            .expect("machine set lock")
            .remove(name);
        self.pulls.lock_recover().set_pending(name, None);
    }

    /// Every machine held on a different declaration than the lab's, and
    /// why — `status`'s `pending` (PRD §7.1). A machine the file dropped
    /// that has stopped since goes now, unless a start or a reload holds
    /// the gate, in which case the next `status` drops it.
    pub(super) async fn pending(&self) -> Vec<PendingMachine> {
        let config = self.config();
        let held: Vec<(Arc<dyn Machine>, Arc<LabFile>)> = {
            let set = self.machines.read().expect("machine set lock");
            set.all()
                .into_iter()
                .filter_map(|m| {
                    let from = set.built_from(m.name())?;
                    (!Arc::ptr_eq(&from, &config)).then_some((m, from))
                })
                .collect()
        };
        let mut pending = Vec::new();
        for (m, from) in held {
            let name = m.name();
            let Some(was) = from.lab.machine(name) else {
                continue;
            };
            let now = config.lab.machine(name);
            if reload_plan::runs_declared(was, now) {
                continue;
            }
            let live = m.state().await != PowerState::Stopped;
            let pulling = self.pulls.lock_recover().is_active(name);
            let reason = match (now, live, pulling) {
                (_, _, true) => PendingReason::Downloading,
                (None, true, _) => PendingReason::Removed,
                (None, false, _) => {
                    if let Ok(_gate) = self.rebuild_gate.try_write() {
                        self.drop_machine(name);
                        continue;
                    }
                    PendingReason::Removed
                }
                (Some(_), true, _) => PendingReason::Running,
                (Some(_), false, _) => PendingReason::Stopped,
            };
            pending.push(PendingMachine {
                machine: name.to_string(),
                reason,
            });
        }
        pending
    }

    /// The lab's SMB server serves the exports it was started with. When a
    /// reload changes them, a server nothing is running against is stopped,
    /// so the next boot starts it from the new file; one a running machine
    /// mounts from keeps serving, and the reload says so.
    async fn reshare(&self, held: &BTreeMap<String, HeldMachine>) -> Vec<String> {
        if self.smb.lock().await.is_none() {
            return Vec::new();
        }
        let now = match self.share_plan().await {
            Ok(plan) => plan.smb.map(|smb| smb_exports(&smb)).unwrap_or_default(),
            Err(e) => format!("{e}"),
        };
        if *self.smb_served.lock_recover() == now {
            return Vec::new();
        }
        if held.values().any(|h| h.live) {
            return vec![
                "shares: the lab's SMB server keeps serving the shares it was started with \
                 while a machine runs; `vmlab down`, then `vmlab up`, serves the new ones"
                    .to_string(),
            ];
        }
        if let Some(mut smb) = self.smb.lock().await.take() {
            smb.stop();
        }
        Vec::new()
    }
}

/// A machine as [`LabRuntime::held`] found it.
struct HeldMachine {
    from: Arc<LabFile>,
    live: bool,
    pulling: bool,
}

/// What an SMB server serves, as one comparable value.
pub(super) fn smb_exports(smb: &crate::labd::share_plan::SmbPlan) -> String {
    let exports: Vec<_> = smb
        .exports
        .iter()
        .map(|e| (&e.machine, e.gateway, &e.shares))
        .collect();
    crate::config::declaration_digest(&exports)
}
