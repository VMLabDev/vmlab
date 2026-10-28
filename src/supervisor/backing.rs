//! What the template store's in-use check (§7.1, ADR-0010) scans: every
//! qcow2 in a lab's working data, and the whole backing chain of each.
//!
//! A clone is not always one hop from the store. Under a freeze (§21.5) a
//! worktree machine's overlay backs onto a frozen generation, which backs onto
//! older generations, then a template variant, then its base. So the check
//! walks every chain to its end, and scans the frozen files and the linked
//! worktrees' labs as well as the registered labs: a worktree lab that is not
//! running still depends on the template.
//!
//! The walk is pure over an injected "read this image's backing file"
//! function, and the family expansion over injected lookups, so both are
//! tested without qemu-img or git (ADR-0003).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

/// The deepest chain followed. Real chains are a handful of layers (overlay,
/// a few generations, variant, base); the cap only stops a corrupt one.
const MAX_DEPTH: usize = 64;

/// How a scanned lab is named in a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabLabel {
    /// A lab in the supervisor's registry, by the name it runs under.
    Registered(String),
    /// A lab of a registered lab's worktree family that is not registered
    /// itself, by its root.
    Unregistered(PathBuf),
}

/// One lab whose working data the check scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRoot {
    pub root: PathBuf,
    pub label: LabLabel,
}

/// Every lab to scan: each registered lab, then the rest of its worktree
/// family (§21) — the main checkout's lab when the registered one is a
/// linked worktree, and every linked worktree of that main lab. A root is
/// scanned once; a registered lab keeps its registered name.
///
/// `main_of` is [`crate::freeze::main_lab_root`] and `worktrees_of`
/// [`crate::freeze::worktree_lab_roots`], injected for the tests.
pub fn scan_roots(
    registered: &[(String, PathBuf)],
    main_of: impl Fn(&Path) -> Option<PathBuf>,
    worktrees_of: impl Fn(&Path) -> Vec<PathBuf>,
) -> Vec<ScanRoot> {
    let key = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (name, root) in registered {
        if seen.insert(key(root)) {
            out.push(ScanRoot {
                root: root.clone(),
                label: LabLabel::Registered(name.clone()),
            });
        }
    }
    for (_, root) in registered {
        let main = main_of(root).unwrap_or_else(|| root.clone());
        let family = std::iter::once(main.clone()).chain(worktrees_of(&main));
        for member in family {
            if seen.insert(key(&member)) {
                out.push(ScanRoot {
                    label: LabLabel::Unregistered(member.clone()),
                    root: member,
                });
            }
        }
    }
    out
}

/// One qcow2 in a lab's working data, and the machine it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabDisk {
    pub path: PathBuf,
    pub machine: String,
    /// The freeze generation the file is frozen in, `None` for a machine's
    /// own disk.
    pub generation: Option<u32>,
}

/// Every qcow2 in the working data `lab_local`: a VM's disks
/// (`vms/<machine>/*.qcow2`: `disk0.qcow2` and each `disk-<name>.qcow2`), a
/// container's (`containers/<machine>/*.qcow2`), and every frozen disk
/// (`freeze/<n>/<machine>/*.qcow2`). A generation's copied volumes sit a
/// level deeper and are not disks.
pub fn lab_disks(lab_local: &Path) -> Vec<LabDisk> {
    let mut out = Vec::new();
    for kind in ["vms", "containers"] {
        machine_disks(&lab_local.join(kind), None, &mut out);
    }
    for generation in crate::freeze::generations(lab_local) {
        machine_disks(
            &crate::freeze::generation_dir(lab_local, generation),
            Some(generation),
            &mut out,
        );
    }
    out
}

/// `<dir>/<machine>/*.qcow2`, sorted so a refusal names machines in a
/// stable order.
fn machine_disks(dir: &Path, generation: Option<u32>, out: &mut Vec<LabDisk>) {
    let mut machines: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    machines.sort();
    for m in machines {
        let Some(machine) = m.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let mut disks: Vec<PathBuf> = std::fs::read_dir(&m)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "qcow2") && p.is_file())
            .collect();
        disks.sort();
        out.extend(disks.into_iter().map(|path| LabDisk {
            path,
            machine: machine.to_string(),
            generation,
        }));
    }
}

/// Who a disk's chain belongs to, as a refusal names it: `lab/machine` for a
/// registered lab, with the generation for a frozen file.
pub fn owner(label: &LabLabel, disk: &LabDisk) -> String {
    let machine = match disk.generation {
        Some(n) => format!("{} (freeze generation {n})", disk.machine),
        None => disk.machine.clone(),
    };
    match label {
        LabLabel::Registered(lab) => format!("{lab}/{machine}"),
        LabLabel::Unregistered(root) => format!("{machine} in the lab at {}", root.display()),
    }
}

/// A backing file as an image records it, resolved the way QEMU opens it: a
/// relative path is relative to the image's own directory (a store variant's
/// `../../disk.qcow2`), not to the working directory.
pub fn resolve_backing(image: &Path, backing: &Path) -> PathBuf {
    if backing.is_absolute() {
        return backing.to_path_buf();
    }
    image
        .parent()
        .map_or_else(|| backing.to_path_buf(), |dir| dir.join(backing))
}

/// Every image `disk` is backed by, nearest first, each canonical so it
/// compares with a store disk. `read` answers an image's backing file as the
/// image records it, `None` for a chain's end or an image it cannot read;
/// `headers` remembers its answers across walks, because frozen files and
/// store disks sit in many chains. The walk stops at a chain's end, at a
/// backing file that does not exist, at a cycle, and at [`MAX_DEPTH`].
pub async fn backing_chain<F, Fut>(disk: &Path, headers: &mut Headers, read: &F) -> Vec<PathBuf>
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = Option<PathBuf>>,
{
    let mut chain = Vec::new();
    let mut seen: HashSet<PathBuf> = disk.canonicalize().into_iter().collect();
    let mut image = disk.to_path_buf();
    while chain.len() < MAX_DEPTH {
        let backing = match headers.get(&image) {
            Some(hit) => hit.clone(),
            None => {
                let answer = read(image.clone()).await;
                headers.insert(image.clone(), answer.clone());
                answer
            }
        };
        let Some(backing) = backing else {
            break;
        };
        let Ok(next) = resolve_backing(&image, &backing).canonicalize() else {
            break;
        };
        if !seen.insert(next.clone()) {
            break;
        }
        chain.push(next.clone());
        image = next;
    }
    chain
}

/// The backing file each image read so far records, `None` for none.
pub type Headers = HashMap<PathBuf, Option<PathBuf>>;

impl fmt::Display for LabLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LabLabel::Registered(name) => f.write_str(name),
            LabLabel::Unregistered(root) => write!(f, "{}", root.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"q").unwrap();
        p.canonicalize().unwrap()
    }

    /// Walk `disk` over a fixed map of image → backing file as recorded.
    async fn chain(disk: &Path, map: &HashMap<PathBuf, PathBuf>) -> Vec<PathBuf> {
        let read = |p: PathBuf| {
            let hit = map.get(&p.canonicalize().unwrap_or(p)).cloned();
            async move { hit }
        };
        backing_chain(disk, &mut Headers::new(), &read).await
    }

    #[tokio::test]
    async fn a_three_layer_chain_is_walked_to_the_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = touch(&tmp.path().join("store/disk.qcow2"));
        let variant = touch(&tmp.path().join("store/variants/std/disk.qcow2"));
        let clone = touch(&tmp.path().join("lab/.vmlab/vms/dc01/disk0.qcow2"));
        let map = HashMap::from([
            (clone.clone(), variant.clone()),
            (variant.clone(), base.clone()),
        ]);
        assert_eq!(chain(&clone, &map).await, vec![variant, base]);
    }

    #[tokio::test]
    async fn a_frozen_generation_sits_between_the_overlay_and_the_template() {
        let tmp = tempfile::tempdir().unwrap();
        let base = touch(&tmp.path().join("store/disk.qcow2"));
        let variant = touch(&tmp.path().join("store/variants/std/disk.qcow2"));
        let local = tmp.path().join("main/.vmlab");
        let gen1 = touch(&crate::freeze::machine_dir(&local, 1, "dc01").join("disk0.qcow2"));
        let gen2 = touch(&crate::freeze::machine_dir(&local, 2, "dc01").join("disk0.qcow2"));
        let main = touch(&local.join("vms/dc01/disk0.qcow2"));
        let wt = touch(&tmp.path().join("wt/.vmlab/vms/dc01/disk0.qcow2"));
        let map = HashMap::from([
            (wt.clone(), gen1.clone()),
            (main.clone(), gen2.clone()),
            (gen2.clone(), gen1.clone()),
            (gen1.clone(), variant.clone()),
            (variant.clone(), base.clone()),
        ]);
        assert_eq!(
            chain(&wt, &map).await,
            vec![gen1.clone(), variant.clone(), base.clone()]
        );
        assert_eq!(chain(&main, &map).await, vec![gen2, gen1, variant, base]);
        // The frozen files are themselves scanned, with their generation.
        let disks = lab_disks(&local);
        let gens: Vec<_> = disks
            .iter()
            .map(|d| (d.machine.as_str(), d.generation))
            .collect();
        assert_eq!(gens, [("dc01", None), ("dc01", Some(1)), ("dc01", Some(2))]);
    }

    #[tokio::test]
    async fn a_relative_backing_resolves_against_the_image_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = touch(&tmp.path().join("store/disk.qcow2"));
        let variant = touch(&tmp.path().join("store/variants/std/disk.qcow2"));
        let clone = touch(&tmp.path().join("lab/disk0.qcow2"));
        let map = HashMap::from([
            (clone.clone(), variant.clone()),
            (variant.clone(), PathBuf::from("../../disk.qcow2")),
        ]);
        assert_eq!(chain(&clone, &map).await, vec![variant, base]);
    }

    #[tokio::test]
    async fn the_walk_stops_at_a_missing_file_and_a_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let a = touch(&tmp.path().join("a.qcow2"));
        let b = touch(&tmp.path().join("b.qcow2"));
        let map = HashMap::from([(a.clone(), b.clone()), (b.clone(), a.clone())]);
        assert_eq!(chain(&a, &map).await, vec![b]);
        let map = HashMap::from([(a.clone(), tmp.path().join("gone.qcow2"))]);
        assert!(chain(&a, &map).await.is_empty());
    }

    #[test]
    fn an_unregistered_worktree_of_a_registered_main_is_scanned() {
        let main = PathBuf::from("/repo/labs/demo");
        let wt = PathBuf::from("/wt/feat-x/labs/demo");
        let roots = scan_roots(
            &[("demo".into(), main.clone())],
            |_| None,
            |m| {
                if m == Path::new("/repo/labs/demo") {
                    vec![PathBuf::from("/wt/feat-x/labs/demo")]
                } else {
                    vec![]
                }
            },
        );
        assert_eq!(
            roots,
            [
                ScanRoot {
                    root: main,
                    label: LabLabel::Registered("demo".into())
                },
                ScanRoot {
                    root: wt.clone(),
                    label: LabLabel::Unregistered(wt)
                },
            ]
        );
    }

    #[test]
    fn a_registered_worktree_brings_its_main_and_siblings_once() {
        let main = PathBuf::from("/repo/labs/demo");
        let a = PathBuf::from("/wt/a/labs/demo");
        let b = PathBuf::from("/wt/b/labs/demo");
        let roots = scan_roots(
            &[("demo-a".into(), a.clone()), ("demo".into(), main.clone())],
            |r| (r != Path::new("/repo/labs/demo")).then(|| PathBuf::from("/repo/labs/demo")),
            |m| {
                if m == Path::new("/repo/labs/demo") {
                    vec![a.clone(), b.clone()]
                } else {
                    vec![]
                }
            },
        );
        let labels: Vec<String> = roots.iter().map(|r| r.label.to_string()).collect();
        assert_eq!(labels, ["demo-a", "demo", "/wt/b/labs/demo"]);
    }

    #[test]
    fn owners_name_the_lab_machine_and_generation() {
        let disk = |generation| LabDisk {
            path: PathBuf::from("/x/disk0.qcow2"),
            machine: "dc01".into(),
            generation,
        };
        let reg = LabLabel::Registered("demo".into());
        assert_eq!(owner(&reg, &disk(None)), "demo/dc01");
        assert_eq!(
            owner(&reg, &disk(Some(2))),
            "demo/dc01 (freeze generation 2)"
        );
        let wt = LabLabel::Unregistered(PathBuf::from("/wt/feat-x/labs/demo"));
        assert_eq!(
            owner(&wt, &disk(None)),
            "dc01 in the lab at /wt/feat-x/labs/demo"
        );
    }

    #[test]
    fn lab_disks_finds_extra_and_container_disks() {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path();
        touch(&local.join("vms/dc01/disk0.qcow2"));
        touch(&local.join("vms/dc01/disk-data.qcow2"));
        touch(&local.join("vms/dc01/OVMF_VARS.fd"));
        touch(&local.join("containers/web/scratch.qcow2"));
        // A generation's volumes are copied folders, not disks.
        touch(&crate::freeze::volumes_dir(local, 1).join("cache/inner.qcow2"));
        let found: Vec<String> = lab_disks(local)
            .iter()
            .map(|d| {
                d.path
                    .strip_prefix(local)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            found,
            [
                "vms/dc01/disk-data.qcow2",
                "vms/dc01/disk0.qcow2",
                "containers/web/scratch.qcow2"
            ]
        );
    }

    /// The same chain built for real: a store base, a variant overlay backed
    /// by `../../disk.qcow2`, a frozen generation, the main lab's overlay
    /// and a worktree's, each header read by `qemu-img info`.
    #[tokio::test]
    async fn qemu_img_chains_through_a_freeze_to_the_template() {
        let have = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qemu-img").is_file())
        });
        if !have {
            eprintln!("skipping: qemu-img not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let qemu_img = |dir: &Path, args: &[&str]| {
            std::fs::create_dir_all(dir).unwrap();
            let out = std::process::Command::new("qemu-img")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let overlay = |dir: &Path, backing: &str| {
            qemu_img(
                dir,
                &[
                    "create",
                    "-q",
                    "-f",
                    "qcow2",
                    "-b",
                    backing,
                    "-F",
                    "qcow2",
                    "disk0.qcow2",
                ],
            )
        };
        let store = t.join("store/x86_64/tiny/1.0");
        qemu_img(
            &store,
            &["create", "-q", "-f", "qcow2", "disk.qcow2", "16M"],
        );
        let vdir = store.join("variants/std");
        qemu_img(
            &vdir,
            &[
                "create",
                "-q",
                "-f",
                "qcow2",
                "-b",
                "../../disk.qcow2",
                "-F",
                "qcow2",
                "disk.qcow2",
            ],
        );
        let main = t.join("main/.vmlab");
        let gen1 = crate::freeze::machine_dir(&main, 1, "dc01");
        overlay(&gen1, vdir.join("disk.qcow2").to_str().unwrap());
        overlay(
            &main.join("vms/dc01"),
            gen1.join("disk0.qcow2").to_str().unwrap(),
        );
        let wt = t.join("wt/.vmlab");
        overlay(
            &wt.join("vms/dc01"),
            gen1.join("disk0.qcow2").to_str().unwrap(),
        );

        let read = |image: PathBuf| async move {
            crate::template::qimg::image_info(&image)
                .await
                .ok()
                .and_then(|info| info.backing_file)
        };
        let mut headers = Headers::new();
        let mut clones = Vec::new();
        for (label, local) in [
            (LabLabel::Registered("demo".into()), &main),
            (LabLabel::Unregistered(t.join("wt")), &wt),
        ] {
            for disk in lab_disks(local) {
                let who = owner(&label, &disk);
                for image in backing_chain(&disk.path, &mut headers, &read).await {
                    clones.push((image, who.clone()));
                }
            }
        }
        let base = store.join("disk.qcow2");
        let why = crate::supervisor::store::backed_clones_reason(&clones, &[base])
            .expect("the base is in use");
        assert!(why.contains("demo/dc01, "), "{why}");
        assert!(why.contains("demo/dc01 (freeze generation 1)"), "{why}");
        assert!(why.contains("dc01 in the lab at "), "{why}");
        // Only the main lab is gone: the worktree and the freeze still hold it.
        let held: Vec<_> = clones
            .iter()
            .filter(|(_, who)| who != "demo/dc01")
            .cloned()
            .collect();
        let variant = vdir.join("disk.qcow2");
        assert!(crate::supervisor::store::backed_clones_reason(&held, &[variant]).is_some());
    }
}
