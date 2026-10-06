//! `vmlab lab move <lab> [--from <root>]` — hand a stopped lab's working
//! data, clones and all, from one lab root to another declaring the same
//! name (issue #143).
//!
//! A lab name is host-global (ADR-0011), so several worktrees of one
//! repository share one lab, and a provisioned guest is expensive to build
//! again. The verb runs from the target root: it refuses while anything of the
//! lab runs or when the target already has machines of its own, releases the
//! name, moves the working data (`.vmlab/`, or the `VMLAB_WORK_DIR` directory
//! keyed by the root's hash), carries the paths stored in it over to the new
//! root, and registers the lab from there.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::daemon;
use super::lab::{lab_status, registry_labs, release, root_for, rt};
use crate::config::LabFile;
use crate::config::model::{MachineCfg, VolumeSource};
use crate::proto::CommandError;

/// The directories of a lab's working data that hold a machine's state —
/// disks, snapshots, firmware variables, TPM state, named volumes. Everything
/// else under it (built media, SMB state, the `@dev` ledgers, `state.json`)
/// is rebuilt or meaningless without them.
const MACHINE_DATA: [&str; 3] = ["vms", "containers", "volumes"];

pub fn cmd_lab_move(name: &str, from: Option<PathBuf>) -> Result<()> {
    rt()?.block_on(async {
        let target = crate::paths::find_lab_root(&std::env::current_dir()?)?;
        let target_file = load(&target)?;
        if target_file.lab.name != name {
            return Err(CommandError::invalid(format!(
                "{} declares lab \"{}\", not \"{name}\" — run `vmlab lab move` from a \
                 checkout whose lab file declares \"{name}\"",
                target.join(crate::paths::LAB_FILE).display(),
                target_file.lab.name,
            ))
            .into());
        }

        let labs = registry_labs().await?;
        let registered = root_for(&labs, name);
        let source = source_root(name, from.as_deref(), registered.as_deref(), &target)?;

        let source_data = crate::paths::lab_local_dir(&source);
        let target_data = crate::paths::lab_local_dir(&target);
        if !source_data.is_dir() {
            bail!(
                "{} has no working data for lab \"{name}\" ({} does not exist) — nothing to move",
                source.display(),
                source_data.display()
            );
        }
        // Read before anything changes, so a target holding machines of its
        // own refuses with the source still registered and untouched.
        let held = machine_data(&target_data)?;
        if !held.is_empty() {
            return Err(CommandError::conflict(format!(
                "{} already holds machine data ({}) — `vmlab destroy` here first, \
                 or move the lab somewhere else",
                target_data.display(),
                held.join(", ")
            ))
            .into());
        }

        // Anything still running refuses before the name is released. A
        // registered daemon answers for its machines; with no daemon, a
        // process left holding the disks answers for itself below.
        if registered.is_some()
            && let Some(client) = daemon::try_lab_daemon(name).await
            && let Ok(status) = lab_status(&client).await
        {
            let running: Vec<&str> = status
                .machines
                .iter()
                .filter(|m| m.state != crate::status::PowerState::Stopped)
                .map(|m| m.name.as_str())
                .collect();
            if !running.is_empty() {
                return Err(CommandError::conflict(format!(
                    "lab \"{name}\" still has machines running in {}: {} — \
                     `vmlab down` there first",
                    source.display(),
                    running.join(", ")
                ))
                .into());
            }
        }

        match load(&source) {
            Ok(source_file) => {
                if source_file.lab.name != name {
                    return Err(CommandError::invalid(format!(
                        "{} declares lab \"{}\", not \"{name}\" — its working data is not \
                         this lab's",
                        source.join(crate::paths::LAB_FILE).display(),
                        source_file.lab.name
                    ))
                    .into());
                }
                for line in differences(&source_file, &target_file) {
                    println!("warning: {line}");
                }
            }
            Err(_) => println!(
                "warning: {} does not load, so the two lab files were not compared",
                source.join(crate::paths::LAB_FILE).display()
            ),
        }

        if registered.is_some() {
            release(name).await?;
            println!("released lab \"{name}\" from {}", source.display());
        }
        let holding = crate::qemu::process::lab_processes(name, &source_data);
        if !holding.is_empty() {
            return Err(CommandError::conflict(format!(
                "processes still use lab \"{name}\"'s working data in {} (pids {}) — \
                 stop them, then run this again",
                source_data.display(),
                holding
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .into());
        }

        if target_data.exists() {
            std::fs::remove_dir_all(&target_data)
                .with_context(|| format!("removing {}", target_data.display()))?;
        }
        if let Some(parent) = target_data.parent() {
            crate::paths::ensure_dir(parent)?;
        }
        let how = move_tree(&source_data, &target_data)?;
        println!(
            "moved {} to {} ({how})",
            source_data.display(),
            target_data.display()
        );
        for line in rewrite_paths(&target_data, &source, &target)? {
            println!("{line}");
        }

        let missing = missing_host_paths(&target_file);
        if !missing.is_empty() {
            println!(
                "warning: these host paths do not exist under {} — anything git ignores \
                 (a media folder, say) did not move with the lab:",
                target.display()
            );
            for (machine, what, path) in &missing {
                println!("  {machine}: {what} {}", path.display());
            }
        }

        daemon::ensure_lab_daemon(name, &target)
            .await
            .map_err(|e| anyhow!("the lab moved, but registering it failed: {e:#}"))?;
        println!(
            "lab \"{name}\" now lives in {} — run `vmlab validate`, then `vmlab up`",
            target.display()
        );
        Ok(())
    })
}

fn load(root: &Path) -> Result<LabFile> {
    crate::config::load_lab_root(root).map_err(|e| anyhow!("{:?}", miette::Report::new(e)))
}

/// The root the lab moves out of: `--from` when given, else where the name
/// is registered. A name registered somewhere `--from` does not name is
/// another checkout's lab, and the target's own registration means there is
/// nothing to move.
fn source_root(
    name: &str,
    from: Option<&Path>,
    registered: Option<&Path>,
    target: &Path,
) -> Result<PathBuf> {
    // A root whose checkout is gone (with `VMLAB_WORK_DIR` its data outlives
    // it) cannot be canonicalized; it is used as given.
    let from = from
        .map(|p| {
            let p = super::daemon::abs_path(p)?;
            Ok::<_, anyhow::Error>(p.canonicalize().unwrap_or(p))
        })
        .transpose()?;
    let source = match (from, registered) {
        (Some(from), Some(registered)) if from != registered => {
            return Err(CommandError::conflict(format!(
                "lab \"{name}\" is registered from {}, not {} — move it from there, or \
                 `vmlab lab stop {name}` to release it first",
                registered.display(),
                from.display()
            ))
            .into());
        }
        (Some(from), _) => from,
        (None, Some(registered)) => registered.to_path_buf(),
        (None, None) => bail!(
            "lab \"{name}\" is not registered, so vmlab cannot tell where it lives — \
             name the old root with `--from <dir>`"
        ),
    };
    if source == target {
        bail!(
            "lab \"{name}\" already lives in {} — nothing to move",
            target.display()
        );
    }
    Ok(source)
}

/// What under a lab's working data belongs to a machine: each non-empty
/// [`MACHINE_DATA`] directory's entries, as `vms/<name>`.
fn machine_data(lab_local: &Path) -> Result<Vec<String>> {
    let mut held = Vec::new();
    for dir in MACHINE_DATA {
        let Ok(entries) = std::fs::read_dir(lab_local.join(dir)) else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", lab_local.display()))?;
            held.push(format!("{dir}/{}", entry.file_name().to_string_lossy()));
        }
    }
    held.sort();
    Ok(held)
}

/// How the two lab files differ, one line each, for a warning rather than a
/// refusal: the next `up` loads the target's file, and a clone whose template
/// changed is refused there by name.
fn differences(source: &LabFile, target: &LabFile) -> Vec<String> {
    let digests = |file: &LabFile| -> BTreeMap<String, String> {
        file.lab
            .machines()
            .map(|m| (m.name().to_string(), crate::config::declaration_digest(&m)))
            .collect()
    };
    let (old, new) = (digests(source), digests(target));
    let mut lines = Vec::new();
    for (machine, digest) in &old {
        match new.get(machine) {
            None => lines.push(format!(
                "{machine} is not declared here; its clone moves anyway, unused until \
                 `vmlab destroy`"
            )),
            Some(other) if other != digest => lines.push(format!(
                "{machine} is declared differently here; the next `up` runs this declaration"
            )),
            Some(_) => {}
        }
    }
    for machine in new.keys().filter(|m| !old.contains_key(*m)) {
        lines.push(format!(
            "{machine} is declared only here; the next `up` creates it"
        ));
    }
    if lines.is_empty()
        && crate::config::declaration_digest(&source.lab)
            != crate::config::declaration_digest(&target.lab)
    {
        lines.push(
            "the lab files declare the same machines but differ elsewhere (segments, DNS, \
             handlers); the next `up` runs this one"
                .to_string(),
        );
    }
    lines
}

/// Move a directory tree: a rename where both ends are on one filesystem,
/// else a copy beside the destination, verified file by file, renamed into
/// place, and only then the source deleted — so a failure part way leaves the
/// source whole and no destination. Returns which way it went.
fn move_tree(source: &Path, dest: &Path) -> Result<&'static str> {
    match std::fs::rename(source, dest) {
        Ok(()) => return Ok("renamed"),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("moving {} to {}", source.display(), dest.display()));
        }
    }
    let staging = staging_path(dest);
    if staging.exists() {
        // Only an earlier interrupted move of this verb leaves one.
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("removing {}", staging.display()))?;
    }
    let copied = copy_tree(source, &staging).and_then(|()| verify_tree(source, &staging));
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e.context(format!(
            "copying {} to {} — the source is untouched",
            source.display(),
            dest.display()
        )));
    }
    std::fs::rename(&staging, dest)
        .with_context(|| format!("renaming {} to {}", staging.display(), dest.display()))?;
    std::fs::remove_dir_all(source).with_context(|| {
        format!(
            "the copy in {} is complete and verified, but removing the source {} failed — \
             delete it by hand",
            dest.display(),
            source.display()
        )
    })?;
    Ok("copied across filesystems, verified, source removed")
}

/// Where a cross-filesystem copy is assembled before it takes `dest`'s name.
fn staging_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".moving");
    dest.with_file_name(name)
}

/// Copy `source` to `dest` (which must not exist): directories with their
/// permissions, files with their contents and permissions, symlinks as
/// symlinks.
fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)
                .with_context(|| format!("copying {}", from.display()))?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
        // Sockets and FIFOs are a running process's, and nothing runs.
    }
    let mode = std::fs::metadata(source)?.permissions();
    std::fs::set_permissions(dest, mode)?;
    Ok(())
}

/// Check every regular file and symlink under `source` arrived in `dest`
/// with the same content.
fn verify_tree(source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            verify_tree(&from, &to)?;
        } else if kind.is_symlink() {
            if std::fs::read_link(&from)? != std::fs::read_link(&to)? {
                bail!("{} did not copy intact", to.display());
            }
        } else if kind.is_file() && file_digest(&from)? != file_digest(&to)? {
            bail!("{} did not copy intact", to.display());
        }
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<(u64, Vec<u8>)> {
    use sha2::{Digest, Sha256};
    let mut file =
        std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        len += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((len, hasher.finalize().to_vec()))
}

/// Carry the absolute paths the working data stores over to the new root.
///
/// Found by grepping a lab's working data after an `up` with shares and a
/// `@dev` workspace: `smb/smb.conf` names the old directories, but smbd's
/// start renders it afresh, so it is removed rather than edited; each `@dev`
/// machine's sync ledger names its host workspace, which is rebased. Disk
/// clones name their template in the store, not the lab, and SMB's logs are
/// history. Returns a line per change.
fn rewrite_paths(lab_local: &Path, from: &Path, to: &Path) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let conf = lab_local.join("smb").join("smb.conf");
    if conf.exists() {
        std::fs::remove_file(&conf).with_context(|| format!("removing {}", conf.display()))?;
        lines.push("removed smb/smb.conf (the next `up` renders it for this root)".to_string());
    }
    let ledgers = lab_local.join("workspace");
    if let Ok(entries) = std::fs::read_dir(&ledgers) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && crate::labd::workspace::ledger::Ledger::rebase(&path, from, to)?
            {
                lines.push(format!(
                    "rebased the workspace ledger {} onto {}",
                    path.strip_prefix(lab_local).unwrap_or(&path).display(),
                    to.display()
                ));
            }
        }
    }
    Ok(lines)
}

/// Every host path the lab file points a machine at — share folders,
/// container bind mounts, `@dev` workspaces — that does not exist under its
/// root. They are the target checkout's to provide: a folder git ignores is
/// not in a fresh worktree, and the working data never held it.
fn missing_host_paths(file: &LabFile) -> Vec<(String, &'static str, PathBuf)> {
    let home = crate::paths::home();
    let resolve =
        |p: &Path| crate::labd::share_plan::resolve_share_host(&file.root, Some(&home), p);
    let mut missing = Vec::new();
    for machine in file.lab.machines() {
        let mut paths: Vec<(&'static str, PathBuf)> = Vec::new();
        match machine {
            MachineCfg::Vm(vm) => {
                paths.extend(vm.shares.iter().map(|s| ("share", resolve(&s.host))));
            }
            MachineCfg::Container(c) => {
                paths.extend(c.volumes.iter().filter_map(|v| match &v.source {
                    VolumeSource::Host(p) => Some(("volume", resolve(p))),
                    VolumeSource::Named(_) => None,
                }));
            }
        }
        if let Some(ws) = machine.dev().and_then(|d| d.workspace.as_ref()) {
            paths.push(("workspace", file.root.join(ws)));
        }
        for (what, path) in paths {
            // `./media` joined to the root reads `<root>/./media`.
            let path: PathBuf = path.components().collect();
            if !path.exists() {
                missing.push((machine.name().to_string(), what, path));
            }
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAB: &str = r#"import <vmlab.wcl>

lab "moving" {
  segment "lan" { subnet = "10.70.0.0/24" }
  vm "web" {
    template = "x86_64/linux-modern"
    nic { segment = "lan" }
    share { host = "./media" guest = "/mnt/media" }
  }
  vm "db" {
    template = "x86_64/linux-modern"
    nic { segment = "lan" }
  }
}
"#;

    fn lab_at(root: &Path, source: &str) -> LabFile {
        crate::config::load_lab_source(source, "<test>", root).unwrap()
    }

    /// Two checkouts of one lab file differ only in their roots, which is
    /// not a difference worth a warning.
    #[test]
    fn the_same_lab_file_in_two_roots_does_not_differ() {
        let a = lab_at(Path::new("/a"), LAB);
        let b = lab_at(Path::new("/b"), LAB);
        assert_eq!(differences(&a, &b), Vec::<String>::new());
    }

    #[test]
    fn differing_machines_are_named_each_way() {
        let a = lab_at(Path::new("/a"), LAB);
        let edited = LAB
            .replace(
                "vm \"db\" {\n    template = \"x86_64/linux-modern\"",
                "vm \"db\" {\n    template = \"x86_64/other\"",
            )
            .replace("vm \"web\"", "vm \"www\"");
        let b = lab_at(Path::new("/b"), &edited);
        let lines = differences(&a, &b);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].starts_with("db is declared differently here"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("web is not declared here"),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with("www is declared only here"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_lab_level_edit_is_said_when_no_machine_differs() {
        let a = lab_at(Path::new("/a"), LAB);
        let b = lab_at(
            Path::new("/b"),
            &LAB.replace("10.70.0.0/24", "10.71.0.0/24"),
        );
        let lines = differences(&a, &b);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("differ elsewhere"), "{lines:?}");
    }

    #[test]
    fn a_share_folder_the_target_lacks_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let file = lab_at(dir.path(), LAB);
        assert_eq!(
            missing_host_paths(&file),
            vec![("web".to_string(), "share", dir.path().join("media"))]
        );
        std::fs::create_dir(dir.path().join("media")).unwrap();
        assert!(missing_host_paths(&lab_at(dir.path(), LAB)).is_empty());
    }

    #[test]
    fn the_source_is_from_or_the_registration_and_never_the_target() {
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        assert_eq!(source_root("l", None, Some(a), b).unwrap(), a);
        assert_eq!(source_root("l", Some(a), Some(a), b).unwrap(), a);
        assert_eq!(source_root("l", Some(a), None, b).unwrap(), a);
        let elsewhere = source_root("l", Some(a), Some(Path::new("/c")), b).unwrap_err();
        assert!(
            elsewhere.to_string().contains("registered from /c"),
            "{elsewhere}"
        );
        let unknown = source_root("l", None, None, b).unwrap_err();
        assert!(unknown.to_string().contains("--from"), "{unknown}");
        let here = source_root("l", None, Some(b), b).unwrap_err();
        assert!(here.to_string().contains("nothing to move"), "{here}");
    }

    #[test]
    fn only_machine_state_counts_as_held() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("smb")).unwrap();
        std::fs::create_dir_all(dir.path().join("vms")).unwrap();
        std::fs::write(dir.path().join("state.json"), "{}").unwrap();
        assert!(machine_data(dir.path()).unwrap().is_empty());
        std::fs::create_dir_all(dir.path().join("vms/web")).unwrap();
        std::fs::create_dir_all(dir.path().join("volumes/data")).unwrap();
        assert_eq!(
            machine_data(dir.path()).unwrap(),
            ["vms/web", "volumes/data"]
        );
    }

    /// The cross-filesystem path: copied, verified, renamed into place, and
    /// the source gone — with symlinks and permissions carried.
    #[test]
    fn a_copied_tree_arrives_whole_and_the_source_goes() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::create_dir_all(src.join("vms/web")).unwrap();
        std::fs::write(src.join("vms/web/disk0.qcow2"), vec![7u8; 3 << 20]).unwrap();
        std::fs::write(src.join("smb-creds"), "u:p").unwrap();
        std::fs::set_permissions(
            src.join("smb-creds"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::os::unix::fs::symlink("vms/web", src.join("link")).unwrap();

        let staging = staging_path(&dst);
        copy_tree(&src, &staging).unwrap();
        verify_tree(&src, &staging).unwrap();

        assert_eq!(
            std::fs::read(staging.join("vms/web/disk0.qcow2")).unwrap(),
            vec![7u8; 3 << 20]
        );
        let mode = std::fs::metadata(staging.join("smb-creds"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_link(staging.join("link")).unwrap(),
            Path::new("vms/web")
        );
    }

    #[test]
    fn a_copy_that_differs_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("disk"), "abc").unwrap();
        copy_tree(&src, &dst).unwrap();
        std::fs::write(dst.join("disk"), "abd").unwrap();
        assert!(verify_tree(&src, &dst).is_err());
    }

    #[test]
    fn a_rename_on_one_filesystem_moves_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("a/.vmlab"), dir.path().join("b/.vmlab"));
        std::fs::create_dir_all(src.join("vms/web")).unwrap();
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        assert_eq!(move_tree(&src, &dst).unwrap(), "renamed");
        assert!(dst.join("vms/web").is_dir() && !src.exists());
    }

    #[test]
    fn the_smb_conf_is_dropped_and_ledgers_rebased() {
        use crate::labd::workspace::ledger::Ledger;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        std::fs::create_dir_all(data.join("smb")).unwrap();
        std::fs::write(data.join("smb/smb.conf"), "path = /a/media\n").unwrap();
        let ledger = Ledger::path(data, "dev01");
        Ledger::new(Path::new("/a/src"), "/src")
            .save(&ledger)
            .unwrap();

        let lines = rewrite_paths(data, Path::new("/a"), Path::new("/b")).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(!data.join("smb/smb.conf").exists());
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert!(
            text.contains("\"/b/src\"") && !text.contains("/a/src"),
            "{text}"
        );
    }
}
