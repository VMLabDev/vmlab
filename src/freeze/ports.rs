//! Ports in a frozen family (PRD §21.7): the **port slots** and every fixed
//! port each lab of the family has bound, kept in the main checkout's
//! `.vmlab/worktree-ports.json`.
//!
//! Every lab of a frozen family binds its forwards on `127.0.0.1` (§21.4), so
//! two labs declaring one fixed `host_port` would collide. The main lab is
//! slot 0 and binds exactly what it declares; a worktree lab takes the lowest
//! free slot from 1 and binds a fixed port `P` on `P + slot`, or the next port
//! above that no other lab of the family has recorded and nothing on the host
//! holds. Whatever a lab gets is recorded against its own declaration and
//! asked for first next time, so it survives `down` and `up`.
//!
//! The decisions are pure ([`FamilyPorts::slot`], [`FamilyPorts::choose`])
//! over the loaded record and an injected "is this port free" check;
//! [`lock`] is the one way to read or change the file, under an exclusive
//! `flock` on it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::net::Ipv4Addr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};

use crate::config::model::Proto;

/// The family's port record, beside `worktree.json` in the main checkout's
/// working data.
pub const PORTS_FILE: &str = "worktree-ports.json";

/// One fixed-port declaration of one lab of the family, as the lab finds its
/// own record again on a later install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortKey {
    /// The linked worktree's name; `None` for the main lab (slot 0).
    pub worktree: Option<String>,
    pub machine: String,
    /// `forward` (a segment `forward {}`), `port` (a container `port {}`) or
    /// `script` (a wscript `seg.forward`).
    pub source: String,
    pub guest_port: u16,
    pub proto: Proto,
    /// The `host_port` the declaration asked for.
    pub declared: u16,
}

impl PortKey {
    /// Whether the forward listens on UDP. `both` listens on TCP (§9.8), so
    /// it shares TCP's port space.
    fn udp(&self) -> bool {
        self.proto == Proto::Udp
    }
}

/// A fixed port a lab of the family bound, or (slot 0, from a freeze) will.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRecord {
    #[serde(flatten)]
    pub key: PortKey,
    pub bound: u16,
}

/// `worktree-ports.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyPorts {
    /// Each worktree lab's slot, by git worktree name. The main lab is slot 0
    /// and has no entry.
    #[serde(default)]
    pub slots: BTreeMap<String, u16>,
    #[serde(default)]
    pub ports: Vec<PortRecord>,
}

impl FamilyPorts {
    /// The slot of the lab `worktree` (`None` = the main lab, slot 0),
    /// allocating the lowest free one from 1 when it has none yet.
    pub fn slot(&mut self, worktree: Option<&str>) -> u16 {
        let Some(name) = worktree else { return 0 };
        if let Some(slot) = self.slots.get(name) {
            return *slot;
        }
        let slot = (1..=u16::MAX)
            .find(|s| !self.slots.values().any(|held| held == s))
            .unwrap_or(u16::MAX);
        self.slots.insert(name.to_string(), slot);
        slot
    }

    /// The port this lab recorded for `key` last time.
    pub fn previous(&self, key: &PortKey) -> Option<u16> {
        self.ports.iter().find(|r| r.key == *key).map(|r| r.bound)
    }

    /// Whether `port` is recorded for any declaration other than `key` on
    /// the same transport — another lab's, or another of this lab's own.
    fn taken_by_others(&self, key: &PortKey, port: u16) -> bool {
        self.ports
            .iter()
            .any(|r| r.key != *key && r.key.udp() == key.udp() && r.bound == port)
    }

    /// The port to bind for `key` in `slot` (§21.7). `is_free` says whether
    /// the host has the port free on the address the forward binds, on the
    /// key's transport.
    ///
    /// The main lab (slot 0) always gets exactly the declared port. A
    /// worktree lab asks for its own previous port first, then `P + slot`,
    /// then each port above it that is neither recorded for another
    /// declaration nor held on the host. Never above 65535: running out fails
    /// the forward, naming the declared port.
    pub fn choose(
        &self,
        key: &PortKey,
        slot: u16,
        mut is_free: impl FnMut(u16) -> bool,
    ) -> Result<u16, String> {
        let declared = key.declared;
        if slot == 0 {
            return Ok(declared);
        }
        let mut usable = |p: u16| !self.taken_by_others(key, p) && is_free(p);
        if let Some(p) = self.previous(key).filter(|p| *p != 0)
            && usable(p)
        {
            return Ok(p);
        }
        let Some(start) = declared.checked_add(slot) else {
            return Err(format!(
                "host port {declared} shifted by port slot {slot} passes 65535 (PRD §21.7)"
            ));
        };
        (start..=u16::MAX).find(|p| usable(*p)).ok_or_else(|| {
            format!(
                "host port {declared}: no free port from {start} to 65535 for port slot \
                 {slot} (PRD §21.7)"
            )
        })
    }

    /// Record that `key` is bound on `bound`, replacing its earlier record.
    pub fn record(&mut self, key: PortKey, bound: u16) {
        match self.ports.iter_mut().find(|r| r.key == key) {
            Some(r) => r.bound = bound,
            None => self.ports.push(PortRecord { key, bound }),
        }
    }

    /// Release the worktree lab `worktree`'s slot and every port it recorded
    /// (`vmlab destroy` in that worktree). Whether anything was there.
    pub fn release(&mut self, worktree: &str) -> bool {
        let before = self.ports.len();
        self.ports
            .retain(|r| r.key.worktree.as_deref() != Some(worktree));
        self.slots.remove(worktree).is_some() || self.ports.len() != before
    }

    /// The main lab's records (slot 0).
    pub fn main_records(&self) -> Vec<PortRecord> {
        self.ports
            .iter()
            .filter(|r| r.key.worktree.is_none())
            .cloned()
            .collect()
    }

    /// Replace the main lab's records with `records` (a freeze writes its
    /// declared fixed ports; a failed freeze puts the old ones back).
    pub fn replace_main(&mut self, records: Vec<PortRecord>) {
        self.ports.retain(|r| r.key.worktree.is_some());
        self.ports.extend(records);
    }
}

/// The main lab's declared fixed ports as slot-0 records, bound where they
/// are declared: what a freeze writes so no worktree takes one while the main
/// lab is down.
pub fn main_declared(keys: impl IntoIterator<Item = PortKey>) -> Vec<PortRecord> {
    keys.into_iter()
        .map(|key| PortRecord {
            bound: key.declared,
            key,
        })
        .collect()
}

/// Whether nothing on the host holds `port` on `addr`, tested by binding it
/// the way a forward would and letting go at once.
pub fn host_free(addr: Ipv4Addr, port: u16, proto: Proto) -> bool {
    match proto {
        Proto::Udp => std::net::UdpSocket::bind((addr, port)).is_ok(),
        _ => std::net::TcpListener::bind((addr, port)).is_ok(),
    }
}

/// `worktree-ports.json` in the main checkout's working data `lab_local`,
/// held under an exclusive `flock` until [`Locked::save`] or drop.
pub struct Locked {
    file: Flock<File>,
    path: PathBuf,
}

/// Lock the family's port record in `lab_local`, creating it empty when it
/// is not there yet.
///
/// A save replaces the file by rename, so the lock is taken on whatever the
/// path names once it is held: a waiter that woke on a file a save has since
/// replaced, or a thaw removed, opens the path again.
pub fn lock(lab_local: &Path) -> Result<Locked> {
    std::fs::create_dir_all(lab_local)
        .with_context(|| format!("creating {}", lab_local.display()))?;
    let path = lab_local.join(PORTS_FILE);
    loop {
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let file = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, errno)| anyhow!("cannot lock {}: {errno}", path.display()))?;
        let held = file.metadata()?;
        match std::fs::metadata(&path) {
            Ok(now) if now.ino() == held.ino() && now.dev() == held.dev() => {
                return Ok(Locked { file, path });
            }
            _ => continue,
        }
    }
}

impl Locked {
    /// The record as it stands. An empty file is an empty record.
    pub fn load(&mut self) -> Result<FamilyPorts> {
        let mut text = String::new();
        (&*self.file)
            .read_to_string(&mut text)
            .with_context(|| format!("reading {}", self.path.display()))?;
        if text.trim().is_empty() {
            return Ok(FamilyPorts::default());
        }
        serde_json::from_str(&text).with_context(|| format!("reading {}", self.path.display()))
    }

    /// Write `ports` through a temporary file and a rename, then let go of
    /// the lock. The rename is the last thing done under it, so nobody can
    /// lock the new file while this one still believes it holds the old.
    pub fn save(self, ports: &FamilyPorts) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(ports)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Delete the record (a thaw), under the lock.
    pub fn remove(self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// Lock, change and save the record in `lab_local`, returning what `f` did.
pub fn update<T>(lab_local: &Path, f: impl FnOnce(&mut FamilyPorts) -> T) -> Result<T> {
    let mut locked = lock(lab_local)?;
    let mut ports = locked.load()?;
    let out = f(&mut ports);
    locked.save(&ports)?;
    Ok(out)
}

/// Put the main lab's records in `lab_local` back to `records` (a failed
/// freeze), leaving the worktrees' as they are now. A record left with
/// nothing in it is removed, as it was before the freeze wrote it.
pub fn restore_main(lab_local: &Path, records: Vec<PortRecord>) -> Result<()> {
    let mut locked = lock(lab_local)?;
    let mut ports = locked.load()?;
    ports.replace_main(records);
    if ports == FamilyPorts::default() {
        locked.remove()
    } else {
        locked.save(&ports)
    }
}

/// Delete the record in `lab_local`, if there is one (a thaw).
pub fn remove(lab_local: &Path) -> Result<()> {
    if !lab_local.join(PORTS_FILE).exists() {
        return Ok(());
    }
    lock(lab_local)?.remove()
}

/// `vmlab destroy` of the lab at `root`: when it is a linked worktree's lab
/// and its main checkout keeps a port record, release the worktree's slot
/// and every port it recorded. Nothing to do anywhere else.
pub fn release_worktree(root: &Path) -> Result<()> {
    let (Some(main_root), Some(name)) = (
        super::main_lab_root(root),
        crate::instance::worktree_name(root),
    ) else {
        return Ok(());
    };
    let main_local = crate::paths::lab_local_dir(&main_root);
    if !main_local.join(PORTS_FILE).exists() {
        return Ok(());
    }
    update(&main_local, |ports| ports.release(&name)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(worktree: Option<&str>, declared: u16) -> PortKey {
        PortKey {
            worktree: worktree.map(str::to_string),
            machine: "web".into(),
            source: "port".into(),
            guest_port: 80,
            proto: Proto::Tcp,
            declared,
        }
    }

    fn all_free(_: u16) -> bool {
        true
    }

    #[test]
    fn slots_are_the_lowest_free_from_one_and_kept() {
        let mut f = FamilyPorts::default();
        assert_eq!(f.slot(None), 0, "the main lab");
        assert_eq!(f.slot(Some("a")), 1);
        assert_eq!(f.slot(Some("b")), 2);
        assert_eq!(f.slot(Some("a")), 1, "kept");
        assert!(f.release("a"));
        assert_eq!(f.slot(Some("c")), 1, "a's slot is free again");
        assert_eq!(f.slot(Some("d")), 3);
        assert!(f.slots.values().all(|s| *s != 0));
    }

    #[test]
    fn the_main_lab_binds_exactly_what_it_declares() {
        let mut f = FamilyPorts::default();
        f.record(key(Some("a"), 9000), 8080);
        assert_eq!(f.choose(&key(None, 8080), 0, |_| false), Ok(8080));
    }

    #[test]
    fn a_worktree_shifts_by_its_slot() {
        let f = FamilyPorts::default();
        assert_eq!(f.choose(&key(Some("a"), 8080), 1, all_free), Ok(8081));
        assert_eq!(f.choose(&key(Some("b"), 8080), 2, all_free), Ok(8082));
    }

    /// A port another lab recorded is skipped even while that lab is down,
    /// and so is one the host holds.
    #[test]
    fn recorded_and_held_ports_are_skipped() {
        let mut f = FamilyPorts::default();
        f.record(key(Some("b"), 8079), 8081);
        let held = |p: u16| p != 8082;
        assert_eq!(f.choose(&key(Some("a"), 8080), 1, held), Ok(8083));
    }

    /// The main lab's freeze-time records keep a worktree off its ports.
    #[test]
    fn a_frozen_main_lab_s_ports_are_never_taken() {
        let mut f = FamilyPorts::default();
        f.replace_main(main_declared([key(None, 8081)]));
        assert_eq!(f.choose(&key(Some("a"), 8080), 1, all_free), Ok(8082));
    }

    #[test]
    fn a_previous_port_is_asked_for_first() {
        let mut f = FamilyPorts::default();
        let k = key(Some("a"), 8080);
        f.record(k.clone(), 8090);
        assert_eq!(f.choose(&k, 1, all_free), Ok(8090));
        // Gone to the host meanwhile: the shift starts again.
        assert_eq!(f.choose(&k, 1, |p| p != 8090), Ok(8081));
        // Recorded since by another lab: likewise.
        f.record(key(Some("b"), 1), 8090);
        assert_eq!(f.choose(&k, 1, all_free), Ok(8081));
    }

    /// One lab's own other declarations count as taken too, so two fixed
    /// ports of one worktree never land on one host port.
    #[test]
    fn a_lab_s_own_other_ports_are_taken() {
        let mut f = FamilyPorts::default();
        f.record(key(Some("a"), 8081), 8082);
        assert_eq!(f.choose(&key(Some("a"), 8080), 1, all_free), Ok(8081));
        let mut other = key(Some("a"), 8081);
        other.guest_port = 81;
        assert_eq!(f.choose(&other, 1, all_free), Ok(8083));
    }

    /// TCP and UDP are separate port spaces; `both` listens on TCP.
    #[test]
    fn transports_do_not_contest() {
        let mut f = FamilyPorts::default();
        f.record(key(Some("b"), 1), 8081);
        let mut udp = key(Some("a"), 8080);
        udp.proto = Proto::Udp;
        assert_eq!(f.choose(&udp, 1, all_free), Ok(8081));
        let mut both = key(Some("a"), 8080);
        both.proto = Proto::Both;
        assert_eq!(f.choose(&both, 1, all_free), Ok(8082));
    }

    #[test]
    fn a_shift_past_65535_fails_naming_the_declared_port() {
        let f = FamilyPorts::default();
        let why = f.choose(&key(Some("a"), 65535), 1, all_free).unwrap_err();
        assert!(why.contains("65535"), "{why}");
        let why = f
            .choose(&key(Some("a"), 65530), 2, |p| p < 65532)
            .unwrap_err();
        assert!(why.contains("host port 65530"), "{why}");
        assert_eq!(f.choose(&key(Some("a"), 65534), 1, all_free), Ok(65535));
    }

    #[test]
    fn release_drops_a_worktree_s_slot_and_ports_only() {
        let mut f = FamilyPorts::default();
        f.slot(Some("a"));
        f.slot(Some("b"));
        f.record(key(None, 8080), 8080);
        f.record(key(Some("a"), 8080), 8081);
        f.record(key(Some("b"), 8080), 8082);
        assert!(f.release("a"));
        assert!(!f.release("a"));
        assert_eq!(f.slots.keys().collect::<Vec<_>>(), ["b"]);
        assert_eq!(f.ports.len(), 2);
        assert_eq!(f.main_records().len(), 1);
    }

    #[test]
    fn replace_main_leaves_the_worktrees_alone() {
        let mut f = FamilyPorts::default();
        f.record(key(None, 8080), 8080);
        f.record(key(Some("a"), 8080), 8081);
        f.replace_main(main_declared([key(None, 9090)]));
        assert_eq!(f.main_records(), main_declared([key(None, 9090)]));
        assert_eq!(f.previous(&key(Some("a"), 8080)), Some(8081));
    }

    #[test]
    fn the_file_round_trips_under_its_lock_and_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut locked = lock(tmp.path()).unwrap();
        assert_eq!(locked.load().unwrap(), FamilyPorts::default());
        drop(locked);
        let slot = update(tmp.path(), |f| {
            f.record(key(Some("a"), 8080), 8081);
            f.slot(Some("a"))
        })
        .unwrap();
        assert_eq!(slot, 1);
        let mut locked = lock(tmp.path()).unwrap();
        let f = locked.load().unwrap();
        assert_eq!(f.slots.get("a"), Some(&1));
        assert_eq!(f.previous(&key(Some("a"), 8080)), Some(8081));
        drop(locked);
        let text = std::fs::read_to_string(tmp.path().join(PORTS_FILE)).unwrap();
        assert!(text.contains("\"worktree\": \"a\""), "{text}");
        remove(tmp.path()).unwrap();
        remove(tmp.path()).unwrap();
        assert!(!tmp.path().join(PORTS_FILE).exists());
    }

    /// A failed freeze puts the main lab's records back, and removes a
    /// record it created.
    #[test]
    fn restore_main_undoes_a_freeze_s_write() {
        let tmp = tempfile::tempdir().unwrap();
        update(tmp.path(), |f| {
            f.replace_main(main_declared([key(None, 8080)]))
        })
        .unwrap();
        restore_main(tmp.path(), Vec::new()).unwrap();
        assert!(!tmp.path().join(PORTS_FILE).exists());

        update(tmp.path(), |f| f.record(key(Some("a"), 8080), 8081)).unwrap();
        let before = update(tmp.path(), |f| {
            let before = f.main_records();
            f.replace_main(main_declared([key(None, 9090)]));
            before
        })
        .unwrap();
        restore_main(tmp.path(), before).unwrap();
        let f = lock(tmp.path()).unwrap().load().unwrap();
        assert!(f.main_records().is_empty());
        assert_eq!(f.previous(&key(Some("a"), 8080)), Some(8081));
    }

    /// Two writers serialise on the lock, and neither loses the other's
    /// change across the rename a save does.
    #[test]
    fn concurrent_updates_both_land() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    update(&dir, |f| f.slot(Some(&format!("wt{i}")))).unwrap()
                })
            })
            .collect();
        let mut slots: Vec<u16> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        slots.sort_unstable();
        assert_eq!(slots, (1..=8).collect::<Vec<_>>());
    }

    #[test]
    fn host_free_sees_a_held_port() {
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!host_free(Ipv4Addr::LOCALHOST, port, Proto::Tcp));
        drop(held);
        assert!(host_free(Ipv4Addr::LOCALHOST, port, Proto::Tcp));
    }
}
