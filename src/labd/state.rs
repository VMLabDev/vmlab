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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    /// Captured while running (disk+RAM+device) vs powered off (disk only).
    pub online: bool,
    pub taken_at: chrono::DateTime<chrono::Utc>,
    /// Container snapshots only: the image digest pinned at capture. The
    /// scratch overlay (and any vmstate) is valid only against the same
    /// read-only rootfs, so restore refuses a changed pin. `None` for VMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
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
            },
        );
        s.machine_mut("web").snapshots.insert(
            "prepped".into(),
            SnapshotRecord {
                online: false,
                taken_at: chrono::Utc::now(),
                image_digest: Some("sha256:abc".into()),
            },
        );
        s.save(tmp.path()).unwrap();
        let loaded = LabState::load(tmp.path()).unwrap();
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
