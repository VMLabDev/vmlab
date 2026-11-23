//! What an offline snapshot holds beside the disks (PRD §7.3).
//!
//! A powered-off VM is more than its disks: an OVMF VM keeps its variable
//! store in a file of its own, and a VM with a TPM keeps the TPM's permanent
//! state in a directory of its own. An online snapshot carries both in its
//! device state. An offline one is a qcow2-internal snapshot per disk, which
//! has nowhere to put them, so they are kept here: one directory per snapshot
//! under the VM's `snapshot-state/`, holding a copy of each as it was at
//! capture.
//!
//! The directory's existence is the record. A snapshot with none was taken
//! before vmlab kept this state, and a restore of it leaves the variables and
//! the TPM alone, as it always did. A snapshot whose directory lacks one of
//! the two was taken when the VM had none, and a restore removes the VM's
//! own: a variable store written after the capture does not belong to the
//! disk the restore brings back, and the next start makes a fresh one.
//!
//! Every change is staged first and put in place by a rename, so a copy that
//! fails leaves the VM and its other snapshots as they were.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const VARS: &str = "vars";
const TPM: &str = "tpm";

/// One VM's firmware state and where its offline snapshots keep theirs.
#[derive(Debug, Clone)]
pub(super) struct FirmwareState {
    /// `snapshot-state/` in the VM's directory.
    root: PathBuf,
    /// The VM's UEFI variable store, a file.
    vars: PathBuf,
    /// The VM's TPM state, a directory.
    tpm: PathBuf,
}

/// A restore's copies, made but not yet in place.
#[derive(Debug)]
pub(super) struct StagedRestore {
    vars: Option<PathBuf>,
    tpm: Option<PathBuf>,
}

impl FirmwareState {
    pub(super) fn new(root: PathBuf, vars: PathBuf, tpm: PathBuf) -> Self {
        Self { root, vars, tpm }
    }

    /// A snapshot name is not a safe file name by itself (`..` is a valid
    /// one), so every entry carries a prefix.
    fn entry(&self, name: &str) -> PathBuf {
        self.root.join(format!("snap-{name}"))
    }

    /// Copy the VM's firmware state as it is now into a staging directory.
    /// Nothing is visible under a snapshot's name until
    /// [`commit_capture`](Self::commit_capture).
    pub(super) fn stage_capture(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        // The machine's snapshot gate admits one offline operation at a
        // time, so a staging directory found here is a dead one.
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("tmp-") {
                remove_tree(&entry.path())?;
            }
        }
        let staged = self
            .root
            .join(format!("tmp-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&staged)?;
        let copy = || -> Result<()> {
            if self.vars.is_file() {
                std::fs::copy(&self.vars, staged.join(VARS))
                    .with_context(|| format!("copying {}", self.vars.display()))?;
            }
            if self.tpm.is_dir() {
                copy_tree(&self.tpm, &staged.join(TPM))
                    .with_context(|| format!("copying {}", self.tpm.display()))?;
            }
            Ok(())
        };
        if let Err(error) = copy() {
            self.discard_capture(&staged);
            return Err(error.context("saving the firmware state for an offline snapshot"));
        }
        Ok(staged)
    }

    /// Give a staged capture its snapshot's name.
    pub(super) fn commit_capture(&self, staged: &Path, name: &str) -> Result<()> {
        let entry = self.entry(name);
        // The disks have just accepted this name, so no snapshot owns what
        // is here: it is left over from one whose delete did not finish.
        remove_tree(&entry)?;
        std::fs::rename(staged, &entry)
            .with_context(|| format!("keeping the firmware state of snapshot {name:?}"))
    }

    pub(super) fn discard_capture(&self, staged: &Path) {
        if let Err(error) = remove_tree(staged) {
            tracing::warn!(path = %staged.display(), %error, "could not remove staged firmware state");
        }
    }

    /// Copy what snapshot `name` holds to beside the VM's own state, ready to
    /// be renamed over it. `None` for a snapshot that holds none.
    pub(super) fn stage_restore(&self, name: &str) -> Result<Option<StagedRestore>> {
        let entry = self.entry(name);
        if !entry.is_dir() {
            return Ok(None);
        }
        let mut staged = StagedRestore {
            vars: None,
            tpm: None,
        };
        let copy = |staged: &mut StagedRestore| -> Result<()> {
            let saved = entry.join(VARS);
            if saved.is_file() {
                let to = sibling(&self.vars, "restore");
                remove_tree(&to)?;
                staged.vars = Some(to.clone());
                std::fs::copy(&saved, &to)
                    .with_context(|| format!("copying {}", saved.display()))?;
            }
            let saved = entry.join(TPM);
            if saved.is_dir() {
                let to = sibling(&self.tpm, "restore");
                remove_tree(&to)?;
                staged.tpm = Some(to.clone());
                copy_tree(&saved, &to).with_context(|| format!("copying {}", saved.display()))?;
            }
            Ok(())
        };
        if let Err(error) = copy(&mut staged) {
            self.discard_restore(staged);
            return Err(error.context(format!("reading the firmware state of snapshot {name:?}")));
        }
        Ok(Some(staged))
    }

    /// Put a staged restore in place of the VM's own state. The VM must be
    /// stopped: a running TPM emulator writes its state as it goes.
    pub(super) fn commit_restore(&self, staged: StagedRestore) -> Result<()> {
        match &staged.vars {
            Some(from) => std::fs::rename(from, &self.vars)
                .with_context(|| format!("replacing {}", self.vars.display()))?,
            None => remove_tree(&self.vars)?,
        }
        // A directory cannot be renamed over one that has files in it, so
        // the old one steps aside first.
        let old = sibling(&self.tpm, "old");
        remove_tree(&old)?;
        let had = match std::fs::rename(&self.tpm, &old) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error).with_context(|| format!("replacing {}", self.tpm.display()));
            }
        };
        if let Some(from) = &staged.tpm
            && let Err(error) = std::fs::rename(from, &self.tpm)
        {
            if had {
                let _ = std::fs::rename(&old, &self.tpm);
            }
            return Err(error).with_context(|| format!("replacing {}", self.tpm.display()));
        }
        if let Err(error) = remove_tree(&old) {
            tracing::warn!(path = %old.display(), %error, "could not remove the replaced TPM state");
        }
        Ok(())
    }

    pub(super) fn discard_restore(&self, staged: StagedRestore) {
        for path in [staged.vars, staged.tpm].into_iter().flatten() {
            if let Err(error) = remove_tree(&path) {
                tracing::warn!(path = %path.display(), %error, "could not remove staged firmware state");
            }
        }
    }

    /// Forget what snapshot `name` holds. A snapshot that holds nothing is
    /// not an error.
    pub(super) fn delete(&self, name: &str) -> Result<()> {
        let entry = self.entry(name);
        remove_tree(&entry).with_context(|| format!("removing {}", entry.display()))
    }
}

/// `path` with `.suffix` on its file name: the same directory, so the same
/// filesystem, so a rename between the two is atomic.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Remove a file or a whole directory. One that is not there is not an error.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Copy the directory `from` to the new directory `to`, keeping symlinks as
/// symlinks and each file's mode.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    std::fs::set_permissions(to, std::fs::metadata(from)?.permissions())?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Vm {
        _tmp: tempfile::TempDir,
        state: FirmwareState,
        vars: PathBuf,
        tpm: PathBuf,
    }

    fn vm() -> Vm {
        let tmp = tempfile::tempdir().unwrap();
        let vars = tmp.path().join("OVMF_VARS.fd");
        let tpm = tmp.path().join("tpm-state");
        Vm {
            state: FirmwareState::new(tmp.path().join("snapshot-state"), vars.clone(), tpm.clone()),
            _tmp: tmp,
            vars,
            tpm,
        }
    }

    fn capture(vm: &Vm, name: &str) {
        let staged = vm.state.stage_capture().unwrap();
        vm.state.commit_capture(&staged, name).unwrap();
    }

    fn restore(vm: &Vm, name: &str) -> bool {
        match vm.state.stage_restore(name).unwrap() {
            Some(staged) => {
                vm.state.commit_restore(staged).unwrap();
                true
            }
            None => false,
        }
    }

    #[test]
    fn a_restore_puts_back_both_and_keeps_symlinks() {
        let vm = vm();
        std::fs::write(&vm.vars, b"then").unwrap();
        std::fs::create_dir(&vm.tpm).unwrap();
        std::fs::write(vm.tpm.join("permall"), b"then").unwrap();
        std::os::unix::fs::symlink("permall", vm.tpm.join("link")).unwrap();
        capture(&vm, "s");

        std::fs::write(&vm.vars, b"now").unwrap();
        std::fs::write(vm.tpm.join("permall"), b"now").unwrap();
        std::fs::write(vm.tpm.join("later"), b"now").unwrap();
        assert!(restore(&vm, "s"));

        assert_eq!(std::fs::read(&vm.vars).unwrap(), b"then");
        assert_eq!(std::fs::read(vm.tpm.join("permall")).unwrap(), b"then");
        assert_eq!(
            std::fs::read_link(vm.tpm.join("link")).unwrap(),
            Path::new("permall")
        );
        assert!(!vm.tpm.join("later").exists());
        assert!(!sibling(&vm.tpm, "old").exists());
        assert!(!sibling(&vm.tpm, "restore").exists());
        assert!(!sibling(&vm.vars, "restore").exists());
    }

    /// A VM captured before its first start has neither. Restoring that
    /// snapshot gives back a VM that has neither, so the next start makes
    /// them fresh for the disk it is given.
    #[test]
    fn a_capture_of_a_vm_with_no_state_restores_to_none() {
        let vm = vm();
        capture(&vm, "pristine");
        std::fs::write(&vm.vars, b"now").unwrap();
        std::fs::create_dir(&vm.tpm).unwrap();
        std::fs::write(vm.tpm.join("permall"), b"now").unwrap();

        assert!(restore(&vm, "pristine"));
        assert!(!vm.vars.exists());
        assert!(!vm.tpm.exists());
    }

    /// A snapshot taken before vmlab kept this state has no entry, and a
    /// restore of it must not take the VM's variables away.
    #[test]
    fn a_snapshot_with_no_entry_leaves_the_state_alone() {
        let vm = vm();
        std::fs::write(&vm.vars, b"now").unwrap();
        assert!(!restore(&vm, "old"));
        assert_eq!(std::fs::read(&vm.vars).unwrap(), b"now");
    }

    #[test]
    fn dot_names_stay_inside_the_state_directory() {
        let vm = vm();
        std::fs::write(&vm.vars, b"then").unwrap();
        capture(&vm, "..");
        assert!(vm.state.root.join("snap-..").is_dir());
        vm.state.delete("..").unwrap();
        assert_eq!(std::fs::read(&vm.vars).unwrap(), b"then");
        assert_eq!(std::fs::read_dir(&vm.state.root).unwrap().count(), 0);
    }

    #[test]
    fn a_discarded_capture_and_a_dead_staging_directory_leave_nothing() {
        let vm = vm();
        std::fs::write(&vm.vars, b"then").unwrap();
        let dead = vm.state.stage_capture().unwrap();
        let staged = vm.state.stage_capture().unwrap();
        assert!(
            !dead.exists(),
            "the next capture sweeps a dead staging directory"
        );
        vm.state.discard_capture(&staged);
        assert_eq!(std::fs::read_dir(&vm.state.root).unwrap().count(), 0);
        vm.state.delete("never").unwrap();
    }
}
