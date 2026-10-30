//! The freeze, thaw and freeze-point restore plans run by the executor over
//! real qcow2 files (PRD §21). Skipped, with a note, where `qemu-img` or
//! `qemu-io` is not on PATH.
//!
//! Each disk is written with recognisable byte patterns at fixed offsets, so
//! every assertion is "this chain reads back exactly what that state wrote".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::inspect::{chain, disk_facts, generation_of};
use super::plan::{self, FreezeInput, FreezeMachine, ThawDisk, ThawInput, ThawMachine};
use super::steps::execute;
use crate::labd::state::SnapshotRecord;
use crate::template::qimg;

fn have(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(tool).is_file()));
    if !found {
        eprintln!("skipping: {tool} not found on PATH");
    }
    found
}

const MIB: u64 = 1 << 20;

fn io(path: &Path, cmd: &str, read_only: bool) -> String {
    let mut c = std::process::Command::new("qemu-io");
    if read_only {
        c.arg("-r");
    }
    let out = c
        .args(["-f", "qcow2", "-c", cmd])
        .arg(path)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "qemu-io {cmd}: {text}");
    text
}

/// Fill MiB `slot` of `path` with `byte`.
fn write(path: &Path, slot: u64, byte: u8) {
    io(
        path,
        &format!("write -P {byte} {} {MIB}", slot * MIB),
        false,
    );
}

/// Whether MiB `slot` of `path` reads back as `byte`.
fn reads(path: &Path, slot: u64, byte: u8) -> bool {
    !io(path, &format!("read -P {byte} {} {MIB}", slot * MIB), true).contains("failed")
}

fn check_clean(path: &Path) {
    let out = std::process::Command::new("qemu-img")
        .args(["check", "-U"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "qemu-img check {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Bytes of guest data `path`'s own layer holds, from `qemu-img map`: what a
/// freeze actually wrote into the main lab's overlay.
fn own_data(path: &Path) -> u64 {
    let out = std::process::Command::new("qemu-img")
        .args(["map", "-U", "--output=json"])
        .arg(path)
        .output()
        .unwrap();
    let map: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    map.iter()
        .filter(|e| e["depth"] == 0 && e["data"] == true)
        .map(|e| e["length"].as_u64().unwrap())
        .sum()
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
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

struct Lab {
    _tmp: tempfile::TempDir,
    lab_local: PathBuf,
}

impl Lab {
    fn disk(&self, machine: &str) -> PathBuf {
        self.lab_local.join("vms").join(machine).join("disk0.qcow2")
    }
}

/// A lab with machines `names`, each a clone of one template: slot 0 is the
/// template's, slot 1 provisioning's, then offline snapshot `base`, then
/// slot 2 written after it.
async fn lab(names: &[&str]) -> Lab {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tpl = root.join("template.qcow2");
    qimg::create_blank(&tpl, 16 * MIB).await.unwrap();
    write(&tpl, 0, 0x10);
    let lab_local = root.join(".vmlab");
    for name in names {
        let dir = lab_local.join("vms").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("OVMF_VARS.fd"), b"vars").unwrap();
        let disk = dir.join("disk0.qcow2");
        qimg::create_linked_clone(&tpl, &disk).await.unwrap();
        write(&disk, 1, 0x11);
        // More provisioning, which a freeze must not copy into the overlay.
        for slot in 9..13 {
            write(&disk, slot, 0x19);
        }
        qimg::snapshot_create(&disk, "base").await.unwrap();
        write(&disk, 2, 0x12);
    }
    Lab {
        _tmp: tmp,
        lab_local,
    }
}

async fn freeze(lab: &Lab, names: &[&str], generation: u32, snapshot: &str) -> anyhow::Result<()> {
    let mut machines = Vec::new();
    for name in names {
        let disk = lab.disk(name);
        machines.push(FreezeMachine {
            name: name.to_string(),
            stopped: true,
            local_dir: disk.parent().unwrap().to_path_buf(),
            disks: vec![(disk.clone(), disk_facts(&disk).await.unwrap())],
            record: Some(record(false)),
            state_json: b"{}".to_vec(),
        });
    }
    let carried = names
        .iter()
        .map(|n| (n.to_string(), vec!["OVMF_VARS.fd".to_string()]))
        .collect();
    let input = FreezeInput {
        lab_local: lab.lab_local.clone(),
        in_linked_worktree: false,
        generation,
        snapshot: snapshot.into(),
        machines,
        volumes: vec![],
    };
    let plan = plan::plan_freeze(&input, &carried)?;
    let applied = execute(plan).await?;
    assert!(applied.finish().await.is_empty());
    Ok(())
}

async fn point_restore(disk: &Path, machine: &str, snapshot: &str, generation: u32) {
    let facts = disk_facts(disk).await.unwrap().unwrap();
    let plan = plan::plan_point_restore(machine, snapshot, generation, &[facts]).unwrap();
    let applied = execute(plan).await.unwrap();
    assert!(applied.finish().await.is_empty());
}

async fn thaw(lab: &Lab, machine: &str, points: BTreeMap<u32, String>) -> plan::ThawPlan {
    let disk = lab.disk(machine);
    let root = super::freeze_root(&lab.lab_local);
    let mut layers = chain(&disk).await;
    let top = layers.remove(0);
    let layers = layers
        .into_iter()
        .filter_map(|f| generation_of(&f.path, &root).map(|g| (g, f)))
        .collect();
    let input = ThawInput {
        lab_local: lab.lab_local.clone(),
        in_linked_worktree: false,
        machines: vec![ThawMachine {
            name: machine.into(),
            stopped: true,
            disks: vec![ThawDisk { top, layers }],
            online_records: Default::default(),
        }],
        points,
        drop_online: false,
    };
    let out = plan::plan_thaw(&input).unwrap();
    let applied = execute(out.plan.clone()).await.unwrap();
    assert!(applied.finish().await.is_empty());
    out
}

fn snapshot_names(path: &Path) -> Vec<String> {
    let out = std::process::Command::new("qemu-img")
        .args(["info", "-U", "--output=json"])
        .arg(path)
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut names: Vec<String> = v["snapshots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn freeze_point_restore_refreeze_and_thaw_keep_every_state() {
    if !have("qemu-img") || !have("qemu-io") {
        return;
    }
    let lab = lab(&["dc"]).await;
    let disk = lab.disk("dc");
    let g1 = super::machine_dir(&lab.lab_local, 1, "dc").join("disk0.qcow2");

    // Freeze at `base`: the main lab keeps slot 2, the frozen file is `base`.
    freeze(&lab, &["dc"], 1, "base").await.unwrap();
    assert!(reads(&disk, 0, 0x10) && reads(&disk, 1, 0x11) && reads(&disk, 2, 0x12));
    assert!(reads(&g1, 2, 0) && reads(&g1, 1, 0x11));
    assert!(std::fs::metadata(&g1).unwrap().permissions().readonly());
    assert_eq!(snapshot_names(&g1), ["base"]);
    assert_eq!(
        qimg::image_info(&disk).await.unwrap().full_backing_file,
        Some(g1.clone())
    );
    assert!(
        super::machine_dir(&lab.lab_local, 1, "dc")
            .join("OVMF_VARS.fd")
            .is_file()
    );
    check_clean(&disk);
    check_clean(&g1);

    // A snapshot after the freeze lives in the overlay.
    write(&disk, 3, 0x13);
    qimg::snapshot_create(&disk, "after").await.unwrap();
    write(&disk, 4, 0x14);

    // Restoring the freeze point empties the overlay and keeps `after`.
    point_restore(&disk, "dc", "base", 1).await;
    assert!(reads(&disk, 1, 0x11) && reads(&disk, 2, 0) && reads(&disk, 3, 0));
    assert_eq!(snapshot_names(&disk), ["after", "base"]);
    qimg::snapshot_apply(&disk, "after").await.unwrap();
    assert!(reads(&disk, 2, 0x12) && reads(&disk, 3, 0x13) && reads(&disk, 4, 0));
    // The second time is a plain apply of the mark the first one left.
    point_restore(&disk, "dc", "base", 1).await;
    assert!(reads(&disk, 2, 0) && reads(&disk, 3, 0));
    qimg::snapshot_apply(&disk, "after").await.unwrap();
    assert_eq!(
        snapshot_names(&g1),
        ["base"],
        "the frozen file was never written"
    );
    check_clean(&disk);

    // Refreeze at `two`, a snapshot of the overlay: generation 2 backs onto 1.
    write(&disk, 5, 0x15);
    qimg::snapshot_create(&disk, "two").await.unwrap();
    write(&disk, 6, 0x16);
    freeze(&lab, &["dc"], 2, "two").await.unwrap();
    let g2 = super::machine_dir(&lab.lab_local, 2, "dc").join("disk0.qcow2");
    assert_eq!(
        qimg::image_info(&g2).await.unwrap().full_backing_file,
        Some(g1.clone())
    );
    assert!(reads(&g2, 5, 0x15) && reads(&g2, 6, 0));
    assert!(reads(&disk, 3, 0x13) && reads(&disk, 5, 0x15) && reads(&disk, 6, 0x16));
    // A refreeze writes only what changed since `two` (slot 6).
    assert_eq!(own_data(&disk), MIB);
    check_clean(&disk);
    check_clean(&g2);
    write(&disk, 7, 0x17);
    qimg::snapshot_create(&disk, "late").await.unwrap();
    write(&disk, 8, 0x18);

    // Thaw: everything back in one file, the current state and every
    // snapshot, pre-freeze ones included.
    let out = thaw(
        &lab,
        "dc",
        BTreeMap::from([(1, "base".to_string()), (2, "two".to_string())]),
    )
    .await;
    assert!(!super::freeze_root(&lab.lab_local).exists());
    let info = qimg::image_info(&disk).await.unwrap();
    assert_eq!(
        info.full_backing_file,
        Some(
            lab._tmp
                .path()
                .canonicalize()
                .unwrap()
                .join("template.qcow2")
        )
    );
    assert!(!std::fs::metadata(&disk).unwrap().permissions().readonly());
    assert_eq!(snapshot_names(&disk), ["after", "base", "late", "two"]);
    assert_eq!(
        out.survivors["dc"].iter().cloned().collect::<Vec<_>>(),
        ["after", "base", "late", "two"]
    );
    for slot in [3, 5, 7, 8] {
        assert!(
            reads(&disk, slot, 0x10 + slot as u8),
            "current state slot {slot}"
        );
    }
    check_clean(&disk);
    let states: [(&str, &[(u64, u8)]); 4] = [
        ("base", &[(1, 0x11), (2, 0), (3, 0)]),
        ("after", &[(2, 0x12), (3, 0x13), (4, 0)]),
        ("two", &[(3, 0x13), (5, 0x15), (6, 0)]),
        ("late", &[(5, 0x15), (6, 0x16), (7, 0x17), (8, 0)]),
    ];
    for (name, slots) in states {
        qimg::snapshot_apply(&disk, name).await.unwrap();
        for (slot, byte) in slots {
            assert!(reads(&disk, *slot, *byte), "{name}: slot {slot}");
        }
    }
}

#[tokio::test]
async fn a_failure_mid_freeze_puts_every_disk_back() {
    if !have("qemu-img") || !have("qemu-io") {
        return;
    }
    let lab = lab(&["a", "b"]).await;
    let before = |name: &str| snapshot_names(&lab.disk(name));
    let (a0, b0) = (before("a"), before("b"));
    let a_bytes = bytes(&lab.disk("a"));
    // `b` loses its snapshot after the facts were read: its apply fails,
    // after `a` was completely frozen.
    let mut machines = Vec::new();
    for name in ["a", "b"] {
        let disk = lab.disk(name);
        machines.push(FreezeMachine {
            name: name.to_string(),
            stopped: true,
            local_dir: disk.parent().unwrap().to_path_buf(),
            disks: vec![(disk.clone(), disk_facts(&disk).await.unwrap())],
            record: Some(record(false)),
            state_json: b"{}".to_vec(),
        });
    }
    qimg::snapshot_delete(&lab.disk("b"), "base").await.unwrap();
    let b_bytes = bytes(&lab.disk("b"));
    let input = FreezeInput {
        lab_local: lab.lab_local.clone(),
        in_linked_worktree: false,
        generation: 1,
        snapshot: "base".into(),
        machines,
        volumes: vec![],
    };
    let plan = plan::plan_freeze(&input, &BTreeMap::new()).unwrap();
    let err = execute(plan).await.err().expect("the freeze fails");
    assert!(format!("{err:#}").contains("undone"), "{err:#}");

    assert!(!super::freeze_root(&lab.lab_local).exists());
    for name in ["a", "b"] {
        let disk = lab.disk(name);
        assert!(
            qimg::image_info(&disk)
                .await
                .unwrap()
                .full_backing_file
                .is_some()
        );
        assert!(reads(&disk, 1, 0x11) && reads(&disk, 2, 0x12), "{name}");
        assert!(!std::fs::metadata(&disk).unwrap().permissions().readonly());
        check_clean(&disk);
    }
    assert_eq!(bytes(&lab.disk("a")), a_bytes, "a is byte-identical");
    assert_eq!(bytes(&lab.disk("b")), b_bytes, "b is byte-identical");
    assert_eq!(before("a"), a0);
    assert_eq!(
        before("b"),
        b0.into_iter().filter(|n| n != "base").collect::<Vec<_>>()
    );
    qimg::snapshot_apply(&lab.disk("a"), "base").await.unwrap();
    assert!(reads(&lab.disk("a"), 2, 0));
}

#[tokio::test]
async fn a_rollback_after_every_step_ran_unfreezes_too() {
    if !have("qemu-img") || !have("qemu-io") {
        return;
    }
    // What the lab daemon does when saving the records fails after the plan
    // succeeded: the work copy, kept until commit, is the disk again.
    let lab = lab(&["a"]).await;
    let disk = lab.disk("a");
    let original = bytes(&disk);
    let input = FreezeInput {
        lab_local: lab.lab_local.clone(),
        in_linked_worktree: false,
        generation: 1,
        snapshot: "base".into(),
        machines: vec![FreezeMachine {
            name: "a".into(),
            stopped: true,
            local_dir: disk.parent().unwrap().to_path_buf(),
            disks: vec![(disk.clone(), disk_facts(&disk).await.unwrap())],
            record: Some(record(false)),
            state_json: b"{}".to_vec(),
        }],
        volumes: vec![],
    };
    let applied = execute(plan::plan_freeze(&input, &BTreeMap::new()).unwrap())
        .await
        .unwrap();
    assert!(applied.rollback().await.is_empty());
    assert!(!super::freeze_root(&lab.lab_local).exists());
    assert_eq!(bytes(&disk), original, "byte-identical");
    assert!(!disk.with_file_name("disk0.qcow2.freeze-work").exists());
    assert!(reads(&disk, 1, 0x11) && reads(&disk, 2, 0x12));
    assert_eq!(snapshot_names(&disk), ["base"]);
    check_clean(&disk);
}
