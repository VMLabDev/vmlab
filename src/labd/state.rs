//! Persisted lab state (`<lab>/.vmlab/state.json`): generated MACs,
//! created clones, snapshot power-state records (PRD §7.3 — every snapshot
//! records the VM's power state at capture time).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::model::MacAddr;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VmState {
    /// MAC per NIC index — generated deterministically, persisted so DHCP
    /// reservations stay stable (PRD §9.4).
    #[serde(default)]
    pub macs: Vec<MacAddr>,
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
pub struct ContainerState {
    /// MAC per NIC index — same stability contract as VM MACs.
    #[serde(default)]
    pub macs: Vec<MacAddr>,
    /// Image manifest digest resolved at first pull — pins the container
    /// across `up`s (never re-pulled implicitly, mirroring registry
    /// templates, PRD §6.4) until `container destroy` / lab destroy.
    #[serde(default)]
    pub image_digest: Option<String>,
    /// The `image =` reference the digest was resolved from; editing the
    /// reference in vmlab.wcl invalidates the pin.
    #[serde(default)]
    pub image_ref: Option<String>,
    /// Snapshot name → record, same contract as VM snapshots (PRD §18).
    #[serde(default)]
    pub snapshots: BTreeMap<String, SnapshotRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabState {
    #[serde(default)]
    pub vms: BTreeMap<String, VmState>,
    #[serde(default)]
    pub containers: BTreeMap<String, ContainerState>,
}

impl LabState {
    pub fn path(lab_local: &Path) -> PathBuf {
        lab_local.join("state.json")
    }

    pub fn load(lab_local: &Path) -> LabState {
        std::fs::read_to_string(Self::path(lab_local))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, lab_local: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(lab_local)?;
        let tmp = Self::path(lab_local).with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, Self::path(lab_local))?;
        Ok(())
    }

    pub fn vm_mut(&mut self, name: &str) -> &mut VmState {
        self.vms.entry(name.to_string()).or_default()
    }

    pub fn container_mut(&mut self, name: &str) -> &mut ContainerState {
        self.containers.entry(name.to_string()).or_default()
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
        s.vm_mut("a").macs.push(generate_mac("l", "a", 0));
        s.vm_mut("a").snapshots.insert(
            "clean".into(),
            SnapshotRecord {
                online: true,
                taken_at: chrono::Utc::now(),
                image_digest: None,
            },
        );
        s.container_mut("web").snapshots.insert(
            "prepped".into(),
            SnapshotRecord {
                online: false,
                taken_at: chrono::Utc::now(),
                image_digest: Some("sha256:abc".into()),
            },
        );
        s.save(tmp.path()).unwrap();
        let loaded = LabState::load(tmp.path());
        assert_eq!(loaded.vms["a"].macs.len(), 1);
        assert!(loaded.vms["a"].snapshots["clean"].online);
        assert_eq!(
            loaded.containers["web"].snapshots["prepped"]
                .image_digest
                .as_deref(),
            Some("sha256:abc")
        );
    }
}
