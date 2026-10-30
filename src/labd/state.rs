//! Persisted lab state (`<lab>/.vmlab/state.json`): generated MACs,
//! created clones, snapshot power-state records (PRD §7.3 — every snapshot
//! records the VM's power state at capture time).

use anyhow::Context;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::model::MacAddr;

/// One machine's persisted state. VMs and containers record the same things;
/// the image fields simply stay `None` on a VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MachineState {
    /// MAC per NIC index — generated deterministically, persisted so DHCP
    /// reservations stay stable (PRD §9.4).
    #[serde(default)]
    pub macs: Vec<MacAddr>,
    /// Containers only: image manifest digest resolved at first pull — pins
    /// the container across `up`s (never re-pulled implicitly, mirroring
    /// registry templates, PRD §6.4) until destroy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    /// The `image =` reference the digest was resolved from; editing the
    /// reference in vmlab.wcl invalidates the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    /// Snapshot name → record.
    #[serde(default)]
    pub snapshots: BTreeMap<String, SnapshotRecord>,
    /// The agent version `vmlab machine repair-agent` pushed into this
    /// machine, which is what makes it a **diverged machine** (PRD §19.4):
    /// its guest content no longer matches the artefact it was created from,
    /// because a verb deliberately changed it in place. `None` — the ordinary
    /// case — is a machine still running what it was built with.
    ///
    /// Persisted rather than remembered, because divergence outlives the lab
    /// daemon that caused it and outlives the machine's next boot; cleared by
    /// [`crate::labd::lab::LabRuntime::destroy_machine`] along with the disks
    /// the divergence lived on, since what comes back is a fresh clone of the
    /// sealed artefact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repaired_agent: Option<String>,
    /// The last config-weave run on this machine that hit its `timeout`,
    /// while what it started may still be running in the guest. The next
    /// run asks the agent about [`TimedOutRun::tree`] and is refused until
    /// that tree has drained, which clears this.
    ///
    /// Persisted because the guest outlives the lab daemon: a daemon
    /// restarted between the timeout and the next run must still refuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out_run: Option<TimedOutRun>,
}

/// A config-weave run that timed out, and the process tree it ran in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedOutRun {
    /// The agent's name for the run's process tree.
    pub tree: String,
    /// The playbook folder, as declared.
    pub playbook: String,
    pub play: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    /// Captured while running (disk+RAM+device) vs powered off (disk only).
    pub online: bool,
    pub taken_at: chrono::DateTime<chrono::Utc>,
    /// Container snapshots only: the image digest pinned at capture. The
    /// scratch overlay (and any vmstate) is valid only against the same
    /// read-only rootfs, so restore refuses a changed pin. `None` for VMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    /// The freeze point of generation `n` (PRD §21.3): restoring it replaces
    /// the machine's overlays with empty ones on their own backing files,
    /// rather than applying an internal snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze_point: Option<u32>,
    /// Taken before generation `n` was frozen, so stored inside a frozen
    /// file: listed as locked, and restore and delete refuse until `thaw`
    /// (PRD §21.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_in: Option<u32>,
    /// The hypervisor that took it (PRD §22). An online snapshot's RAM and
    /// device state are that hypervisor's own format, so it restores only
    /// under the same one; an offline one is plain qcow2 and restores under
    /// either. A record from before the field was QEMU's — nothing else
    /// could take one.
    #[serde(default)]
    pub hypervisor: crate::config::hypervisor::HypervisorKind,
}

impl SnapshotRecord {
    /// Why this snapshot cannot be restored on a machine that now runs on
    /// `now` — `None` when it can (PRD §22, D31). Only an online one is
    /// bound to its hypervisor, and switching the machine back makes it
    /// restorable again, so the refusal says how.
    pub fn hypervisor_refusal(
        &self,
        machine: &str,
        snap: &str,
        now: crate::config::hypervisor::HypervisorKind,
    ) -> Option<String> {
        (self.online && self.hypervisor != now).then(|| {
            format!(
                "\"{machine}\": snapshot \"{snap}\" was taken online under {took}, and the \
                 machine now runs on {now} — an online snapshot restores only under the \
                 hypervisor that took it (PRD §22). Set `hypervisor = \"{took}\"` on the \
                 machine to restore it, or restore an offline snapshot, which either can",
                took = self.hypervisor,
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabState {
    /// Machine name → state, one namespace for both kinds.
    #[serde(default)]
    pub machines: BTreeMap<String, MachineState>,
    /// Pre-unification state files kept VMs and containers in separate maps.
    /// Read on load and folded into `machines`, never written back — so an
    /// existing lab keeps its MACs, image pins and snapshot records.
    #[serde(default, skip_serializing)]
    vms: BTreeMap<String, MachineState>,
    #[serde(default, skip_serializing)]
    containers: BTreeMap<String, MachineState>,
}

impl LabState {
    pub fn path(lab_local: &Path) -> PathBuf {
        lab_local.join("state.json")
    }

    /// Read the lab's persisted state.
    ///
    /// A missing file is a fresh lab and starts empty. A file that exists but
    /// cannot be read or parsed is *not* treated as empty: doing so silently
    /// regenerates every MAC — moving every DHCP reservation — and orphans
    /// every snapshot record. The caller gets the error and the lab refuses to
    /// come up rather than quietly rebuilding itself.
    pub fn load(lab_local: &Path) -> anyhow::Result<LabState> {
        let path = Self::path(lab_local);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LabState::default()),
            Err(e) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("reading {}", path.display()));
            }
        };
        let mut state: LabState = serde_json::from_str(&raw).with_context(|| {
            format!(
                "parsing {} — move it aside to start over, but every MAC will \
                 be regenerated and every snapshot record lost",
                path.display()
            )
        })?;
        state.migrate_legacy_maps();
        Ok(state)
    }

    /// Fold any legacy `vms`/`containers` entries into `machines`. Entries
    /// already under `machines` win — a half-migrated file cannot lose the
    /// newer record.
    fn migrate_legacy_maps(&mut self) {
        for (name, m) in self
            .vms
            .split_off("")
            .into_iter()
            .chain(self.containers.split_off(""))
        {
            self.machines.entry(name).or_insert(m);
        }
    }

    pub fn save(&self, lab_local: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(lab_local)?;
        let tmp = Self::path(lab_local).with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, Self::path(lab_local))?;
        Ok(())
    }

    pub fn machine_mut(&mut self, name: &str) -> &mut MachineState {
        self.machines.entry(name.to_string()).or_default()
    }
}

/// Deterministic MAC for (lab, vm, nic index): 52:54:00 OUI prefix (QEMU's)
/// plus three bytes of SHA-256("lab:vm:i") (PRD: deterministic MAC via hash).
pub fn generate_mac(lab: &str, vm: &str, nic_index: usize) -> MacAddr {
    let mut h = Sha256::new();
    h.update(lab.as_bytes());
    h.update(b":");
    h.update(vm.as_bytes());
    h.update(b":");
    h.update(nic_index.to_string().as_bytes());
    let d = h.finalize();
    MacAddr([0x52, 0x54, 0x00, d[0], d[1], d[2]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_deterministic_and_distinct() {
        let a = generate_mac("lab1", "dc01", 0);
        let b = generate_mac("lab1", "dc01", 0);
        let c = generate_mac("lab1", "dc01", 1);
        let d = generate_mac("lab2", "dc01", 0);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a.0[0..3], [0x52, 0x54, 0x00]);
    }

    use crate::config::hypervisor::HypervisorKind;

    /// A record written before the field existed was QEMU's (PRD §22).
    #[test]
    fn a_record_without_a_hypervisor_was_qemus() {
        let record: SnapshotRecord =
            serde_json::from_str(r#"{"online": true, "taken_at": "2026-01-01T00:00:00Z"}"#)
                .unwrap();
        assert_eq!(record.hypervisor, HypervisorKind::Qemu);
        assert_eq!(
            record.hypervisor_refusal("m", "s", HypervisorKind::Qemu),
            None
        );
    }

    /// D31: an online snapshot is bound to the hypervisor that took it; an
    /// offline one is plain qcow2 and restores under either.
    #[test]
    fn only_an_online_snapshot_is_bound_to_its_hypervisor() {
        let record = |online, hypervisor| SnapshotRecord {
            online,
            taken_at: chrono::Utc::now(),
            image_digest: None,
            freeze_point: None,
            frozen_in: None,
            hypervisor,
        };
        let why = record(true, HypervisorKind::Vmlab)
            .hypervisor_refusal("web", "warm", HypervisorKind::Qemu)
            .expect("refused");
        assert!(why.contains("\"web\""), "{why}");
        assert!(why.contains("\"warm\""), "{why}");
        assert!(why.contains("taken online under vmlab"), "{why}");
        assert!(why.contains("now runs on qemu"), "{why}");
        assert!(why.contains("hypervisor = \"vmlab\""), "{why}");
        assert!(
            record(true, HypervisorKind::Qemu)
                .hypervisor_refusal("web", "warm", HypervisorKind::Vmlab)
                .is_some()
        );
        for (took, now) in [
            (HypervisorKind::Qemu, HypervisorKind::Vmlab),
            (HypervisorKind::Vmlab, HypervisorKind::Qemu),
        ] {
            assert_eq!(
                record(false, took).hypervisor_refusal("web", "cold", now),
                None
            );
        }
        assert_eq!(
            record(true, HypervisorKind::Vmlab).hypervisor_refusal(
                "web",
                "warm",
                HypervisorKind::Vmlab
            ),
            None
        );
    }

    #[test]
    fn state_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = LabState::default();
        s.machine_mut("a").macs.push(generate_mac("l", "a", 0));
        s.machine_mut("a").snapshots.insert(
            "clean".into(),
            SnapshotRecord {
                online: true,
                taken_at: chrono::Utc::now(),
                image_digest: None,
                freeze_point: None,
                frozen_in: None,
                hypervisor: Default::default(),
            },
        );
        s.machine_mut("web").snapshots.insert(
            "prepped".into(),
            SnapshotRecord {
                online: false,
                taken_at: chrono::Utc::now(),
                image_digest: Some("sha256:abc".into()),
                freeze_point: None,
                frozen_in: None,
                hypervisor: Default::default(),
            },
        );
        s.machine_mut("hv").snapshots.insert(
            "live".into(),
            SnapshotRecord {
                online: true,
                taken_at: chrono::Utc::now(),
                image_digest: None,
                freeze_point: None,
                frozen_in: None,
                hypervisor: HypervisorKind::Vmlab,
            },
        );
        s.save(tmp.path()).unwrap();
        let loaded = LabState::load(tmp.path()).unwrap();
        assert_eq!(
            loaded.machines["hv"].snapshots["live"].hypervisor,
            HypervisorKind::Vmlab
        );
        assert_eq!(loaded.machines["a"].macs.len(), 1);
        assert!(loaded.machines["a"].snapshots["clean"].online);
        assert_eq!(
            loaded.machines["web"].snapshots["prepped"]
                .image_digest
                .as_deref(),
            Some("sha256:abc")
        );
    }

    /// A lab written before VMs and containers shared one map must keep its
    /// MACs, image pins and snapshot records — losing a MAC would move a
    /// guest's DHCP reservation, and losing a pin would orphan its snapshots.
    #[test]
    fn legacy_split_maps_migrate_without_loss() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            LabState::path(tmp.path()),
            r#"{
              "vms": {
                "dc01": {
                  "macs": [[82, 84, 0, 170, 187, 204]],
                  "snapshots": {
                    "clean": {"online": true, "taken_at": "2026-07-01T00:00:00Z"}
                  }
                }
              },
              "containers": {
                "web": {
                  "macs": [[82, 84, 0, 221, 238, 255]],
                  "image_digest": "sha256:abc",
                  "image_ref": "nginx:1.27",
                  "snapshots": {
                    "prepped": {"online": false, "taken_at": "2026-07-02T00:00:00Z",
                                "image_digest": "sha256:abc"}
                  }
                }
              }
            }"#,
        )
        .unwrap();

        let loaded = LabState::load(tmp.path()).unwrap();
        assert_eq!(loaded.machines.len(), 2, "both kinds folded in");
        assert_eq!(
            loaded.machines["dc01"].macs[0].to_string(),
            "52:54:00:aa:bb:cc"
        );
        assert!(loaded.machines["dc01"].snapshots["clean"].online);
        assert_eq!(
            loaded.machines["web"].image_ref.as_deref(),
            Some("nginx:1.27"),
            "the container's image pin survives"
        );
        assert_eq!(
            loaded.machines["web"].snapshots["prepped"]
                .image_digest
                .as_deref(),
            Some("sha256:abc")
        );

        // Re-saving writes only the unified map; a second load is stable.
        loaded.save(tmp.path()).unwrap();
        let written = std::fs::read_to_string(LabState::path(tmp.path())).unwrap();
        assert!(
            !written.contains("\"vms\""),
            "legacy maps are not rewritten"
        );
        assert!(!written.contains("\"containers\""));
        let again = LabState::load(tmp.path()).unwrap();
        assert_eq!(again.machines.len(), 2);
        assert_eq!(
            again.machines["web"].image_ref.as_deref(),
            Some("nginx:1.27")
        );
    }

    /// Treating an unparseable state file as "no state" would regenerate every
    /// MAC and orphan every snapshot, quietly. It has to be an error.
    #[test]
    fn a_corrupt_state_file_is_an_error_not_an_empty_lab() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(LabState::path(tmp.path()), "{ this is not json").unwrap();
        let err = LabState::load(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("state.json"), "names the file: {msg}");
        assert!(msg.contains("MAC"), "warns what starting over costs: {msg}");
    }

    /// A lab that has never run has no state file, and that is not an error.
    #[test]
    fn a_missing_state_file_is_a_fresh_lab() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(LabState::load(tmp.path()).unwrap().machines.is_empty());
    }
}
