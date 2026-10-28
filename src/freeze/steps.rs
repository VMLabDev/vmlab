//! The steps a freeze, a thaw or a freeze-point restore is made of, and the
//! one executor that runs them (ADR-0003).
//!
//! Each verb is planned as an ordered list of [`Planned`] steps before
//! anything is touched, and every step carries its own undo: what puts the
//! files back exactly as they were before it ran. The executor runs the list
//! in order and, on the first failure, runs the undos of everything already
//! done in reverse. That is what makes a freeze all or nothing (§21.1) without
//! the executor knowing what a freeze is.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::template::qimg;

/// One filesystem or `qemu-img` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `qemu-img snapshot -c`.
    SnapshotCreate {
        disk: PathBuf,
        name: String,
    },
    /// `qemu-img snapshot -a`.
    SnapshotApply {
        disk: PathBuf,
        name: String,
    },
    /// `qemu-img snapshot -d`.
    SnapshotDelete {
        disk: PathBuf,
        name: String,
    },
    /// Write `dest` as an overlay on `backing` holding `src` at its internal
    /// snapshot `snapshot` ([`qimg::export_overlay`]).
    Export {
        src: PathBuf,
        snapshot: String,
        backing: PathBuf,
        dest: PathBuf,
    },
    /// An empty overlay on `backing`, by absolute path.
    Overlay {
        backing: PathBuf,
        dest: PathBuf,
    },
    /// `qemu-img commit -d`: `top`'s active layer into its backing file.
    Commit {
        top: PathBuf,
    },
    /// `qemu-img rebase -u`: point `disk` at `backing` without touching data.
    Rebase {
        disk: PathBuf,
        backing: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Mode 0444: nothing opens the file read-write by accident.
    ReadOnly {
        path: PathBuf,
    },
    /// Mode 0644: what [`Step::ReadOnly`] took away.
    Writable {
        path: PathBuf,
    },
    CreateDir {
        path: PathBuf,
    },
    /// `cp -a --reflink=auto`, a file or a tree.
    Copy {
        from: PathBuf,
        to: PathBuf,
    },
    WriteFile {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    /// A file or a tree; one already gone is not an error.
    Remove {
        path: PathBuf,
    },
    /// A directory, only when empty: what a step that may have created a
    /// parent leaves for its undo.
    RemoveIfEmpty {
        path: PathBuf,
    },
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = |p: &PathBuf| p.display().to_string();
        match self {
            Step::SnapshotCreate { disk, name } => write!(f, "snapshot -c {name} {}", d(disk)),
            Step::SnapshotApply { disk, name } => write!(f, "snapshot -a {name} {}", d(disk)),
            Step::SnapshotDelete { disk, name } => write!(f, "snapshot -d {name} {}", d(disk)),
            Step::Export {
                src,
                snapshot,
                backing,
                dest,
            } => write!(
                f,
                "convert -l snapshot.name={snapshot} -B {} {} {}",
                d(backing),
                d(src),
                d(dest)
            ),
            Step::Overlay { backing, dest } => write!(f, "create -b {} {}", d(backing), d(dest)),
            Step::Commit { top } => write!(f, "commit -d {}", d(top)),
            Step::Rebase { disk, backing } => write!(f, "rebase -u -b {} {}", d(backing), d(disk)),
            Step::Rename { from, to } => write!(f, "mv {} {}", d(from), d(to)),
            Step::ReadOnly { path } => write!(f, "chmod 0444 {}", d(path)),
            Step::Writable { path } => write!(f, "chmod 0644 {}", d(path)),
            Step::CreateDir { path } => write!(f, "mkdir -p {}", d(path)),
            Step::Copy { from, to } => write!(f, "cp -a {} {}", d(from), d(to)),
            Step::WriteFile { path, .. } => write!(f, "write {}", d(path)),
            Step::Remove { path } => write!(f, "rm -rf {}", d(path)),
            Step::RemoveIfEmpty { path } => write!(f, "rmdir {}", d(path)),
        }
    }
}

/// A step and what undoes it, in the order the undo runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub step: Step,
    pub undo: Vec<Step>,
}

/// A verb's whole plan: the steps that are rolled back on failure, then
/// `cleanup`, which runs only once the caller has committed to the result
/// (removing what the steps set aside) and cannot be undone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Planned>,
    pub cleanup: Vec<Step>,
}

impl Plan {
    pub fn push(&mut self, step: Step, undo: Vec<Step>) {
        self.steps.push(Planned { step, undo });
    }

    /// Every step, without its undo — what the plan does when nothing fails.
    pub fn forward(&self) -> Vec<&Step> {
        self.steps.iter().map(|p| &p.step).collect()
    }
}

/// Run one step.
pub async fn run_step(step: &Step) -> Result<()> {
    match step {
        Step::SnapshotCreate { disk, name } => qimg::snapshot_create(disk, name).await?,
        Step::SnapshotApply { disk, name } => qimg::snapshot_apply(disk, name).await?,
        Step::SnapshotDelete { disk, name } => qimg::snapshot_delete(disk, name).await?,
        Step::Export {
            src,
            snapshot,
            backing,
            dest,
        } => qimg::export_overlay(src, snapshot, backing, dest).await?,
        Step::Overlay { backing, dest } => qimg::create_linked_clone(backing, dest).await?,
        Step::Commit { top } => qimg::commit(top).await?,
        Step::Rebase { disk, backing } => {
            qimg::rebase_unsafe(disk, &backing.to_string_lossy()).await?
        }
        Step::Rename { from, to } => std::fs::rename(from, to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?,
        Step::ReadOnly { path } => set_mode(path, 0o444)?,
        Step::Writable { path } => set_mode(path, 0o644)?,
        Step::CreateDir { path } => {
            std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?
        }
        Step::Copy { from, to } => copy(from, to).await?,
        Step::WriteFile { path, bytes } => {
            std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?
        }
        Step::Remove { path } => remove(path)?,
        Step::RemoveIfEmpty { path } => {
            let _ = std::fs::remove_dir(path);
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

async fn copy(from: &Path, to: &Path) -> Result<()> {
    let out = tokio::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(from)
        .arg(to)
        .output()
        .await
        .context("running cp")?;
    if !out.status.success() {
        return Err(anyhow!(
            "copying {} to {}: {}",
            from.display(),
            to.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    let r = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match r {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// A plan whose steps all ran. The caller either commits to it
/// ([`Applied::finish`]) or, when what it does next fails, takes it back
/// ([`Applied::rollback`]).
#[must_use = "an applied plan is finished or rolled back"]
pub struct Applied {
    undo: Vec<Vec<Step>>,
    cleanup: Vec<Step>,
}

impl Applied {
    /// Run the plan's cleanup. A failure there is said, not raised: the
    /// result is already in place, and what is left is litter.
    pub async fn finish(self) -> Vec<String> {
        let mut warnings = Vec::new();
        for step in &self.cleanup {
            if let Err(e) = run_step(step).await {
                warnings.push(format!("{step}: {e:#}"));
            }
        }
        warnings
    }

    /// Undo every step, last first. Answers with the undos that failed.
    pub async fn rollback(self) -> Vec<String> {
        undo_all(self.undo).await
    }
}

async fn undo_all(done: Vec<Vec<Step>>) -> Vec<String> {
    let mut failed = Vec::new();
    for undo in done.into_iter().rev() {
        for step in undo {
            if let Err(e) = run_step(&step).await {
                tracing::warn!("undo {step}: {e:#}");
                failed.push(format!("{step}: {e:#}"));
            }
        }
    }
    failed
}

/// Run `plan`. On the first failing step everything already done is undone,
/// and the error says what failed and whether the undo was clean.
pub async fn execute(plan: Plan) -> Result<Applied> {
    let mut done: Vec<Vec<Step>> = Vec::with_capacity(plan.steps.len());
    for Planned { step, undo } in plan.steps {
        tracing::debug!("freeze step: {step}");
        if let Err(e) = run_step(&step).await {
            let failed = undo_all(done).await;
            return Err(rolled_back(e.context(format!("{step}")), &failed));
        }
        done.push(undo);
    }
    Ok(Applied {
        undo: done,
        cleanup: plan.cleanup,
    })
}

/// `e`, said together with how the rollback went.
pub fn rolled_back(e: anyhow::Error, undo_failures: &[String]) -> anyhow::Error {
    if undo_failures.is_empty() {
        e.context("every step already done was undone")
    } else {
        e.context(format!(
            "and undoing what was already done failed too, so the disks need a hand: {}",
            undo_failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failure_undoes_what_ran_in_reverse() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::write(&a, "x").unwrap();
        let mut plan = Plan::default();
        plan.push(
            Step::Rename {
                from: a.clone(),
                to: b.clone(),
            },
            vec![Step::Rename {
                from: b.clone(),
                to: a.clone(),
            }],
        );
        plan.push(
            Step::WriteFile {
                path: tmp.path().join("c"),
                bytes: b"y".to_vec(),
            },
            vec![Step::Remove {
                path: tmp.path().join("c"),
            }],
        );
        plan.push(
            Step::Rename {
                from: tmp.path().join("missing"),
                to: tmp.path().join("z"),
            },
            vec![],
        );
        let err = execute(plan).await.err().unwrap();
        assert!(format!("{err:#}").contains("undone"), "{err:#}");
        assert!(a.exists() && !b.exists() && !tmp.path().join("c").exists());
    }

    #[tokio::test]
    async fn rollback_after_success_restores_and_cleanup_runs_on_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        let mut plan = Plan::default();
        plan.push(
            Step::CreateDir { path: dir.clone() },
            vec![Step::Remove { path: dir.clone() }],
        );
        let applied = execute(plan.clone()).await.unwrap();
        assert!(dir.is_dir());
        assert!(applied.rollback().await.is_empty());
        assert!(!dir.exists());

        plan.cleanup.push(Step::Remove { path: dir.clone() });
        let applied = execute(plan).await.unwrap();
        assert!(applied.finish().await.is_empty());
        assert!(!dir.exists());
    }
}
