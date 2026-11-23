//! Registry of lab daemons (name → root, pid, state), persisted to the
//! state dir so a restarted supervisor can re-adopt running labs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::proto::CommandError;

/// Resolve a lab root before it enters the registry's pure name decision.
pub fn canonical_root(root: &Path) -> Result<PathBuf, CommandError> {
    root.canonicalize().map_err(|error| {
        CommandError::failed(format!(
            "cannot resolve lab root {}: {error}",
            root.display()
        ))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabState {
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabEntry {
    pub name: String,
    pub root: PathBuf,
    pub pid: u32,
    /// When the daemon `pid` started (`supervisor::process_start_time`), so
    /// a later process given the same pid is not taken for it. Absent in
    /// entries written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    pub state: LabState,
}

impl LabEntry {
    /// Whether this entry leaves `name` free for a lab at `requested_root`
    /// (ADR-0011): the conflict when it is registered from anywhere else.
    ///
    /// The remedies it names each release the name: a full `vmlab down`
    /// there, or `vmlab lab stop` by name from anywhere — which is also the
    /// way out when that directory no longer exists — both stop the lab and
    /// have the supervisor reap its daemon while keeping its clones.
    /// Or this directory takes a name of its own as a lab instance
    /// (crate::instance).
    pub fn claims(&self, name: &str, requested_root: &Path) -> Result<(), CommandError> {
        if self.root == requested_root {
            return Ok(());
        }
        Err(CommandError::conflict(format!(
            "lab `{name}` is already registered from {root} — run `vmlab down` there \
             (or `vmlab lab stop {name}` from anywhere) to release it, or run this \
             directory as its own instance with `--instance <id>`",
            root = self.root.display()
        )))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    labs: Vec<LabEntry>,
}

impl Registry {
    fn path() -> PathBuf {
        crate::paths::state_dir().join("labs.json")
    }

    pub fn load() -> anyhow::Result<Self> {
        Ok(match std::fs::read_to_string(Self::path()) {
            Ok(s) => {
                let mut registry: Self = serde_json::from_str(&s).unwrap_or_default();
                for entry in &mut registry.labs {
                    if let Ok(root) = canonical_root(&entry.root) {
                        entry.root = root;
                    }
                }
                registry
            }
            Err(_) => Self::default(),
        })
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn labs(&self) -> &[LabEntry] {
        &self.labs
    }

    pub fn get(&self, name: &str) -> Option<&LabEntry> {
        self.labs.iter().find(|l| l.name == name)
    }

    /// Decide whether `name` may identify a lab at `requested_root`.
    ///
    /// Both roots must be canonical before this pure decision is made.
    pub fn check_name(&self, name: &str, requested_root: &Path) -> Result<(), CommandError> {
        match self.get(name) {
            Some(entry) => entry.claims(name, requested_root),
            None => Ok(()),
        }
    }

    pub fn upsert(&mut self, entry: LabEntry) -> Result<(), CommandError> {
        self.check_name(&entry.name, &entry.root)?;
        match self.labs.iter_mut().find(|l| l.name == entry.name) {
            Some(slot) => *slot = entry,
            None => self.labs.push(entry),
        }
        Ok(())
    }

    pub fn set_state(&mut self, name: &str, state: LabState) {
        if let Some(l) = self.labs.iter_mut().find(|l| l.name == name) {
            l.state = state;
        }
    }

    /// Record when the daemon `pid` of `name` started, for an entry that
    /// has no start time yet.
    pub fn record_start(&mut self, name: &str, pid: u32, started: u64) {
        if let Some(l) = self
            .labs
            .iter_mut()
            .find(|l| l.name == name && l.pid == pid)
        {
            l.started = Some(started);
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.labs.retain(|l| l.name != name);
    }

    /// Remove `name`'s entry only if it still describes the daemon `pid`.
    ///
    /// What a daemon's exit is allowed to clear: by the time an exit is
    /// noticed the name may already belong to that daemon's successor, and
    /// removing the successor's entry would orphan a running lab.
    pub fn remove_daemon(&mut self, name: &str, pid: u32) {
        self.labs.retain(|l| l.name != name || l.pid != pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ErrorCode;
    use std::os::unix::fs::symlink;

    #[test]
    fn a_registered_lab_name_conflicts_at_another_root_in_every_state() {
        for state in [LabState::Running, LabState::Stopping, LabState::Failed] {
            let registry = Registry {
                labs: vec![LabEntry {
                    name: "mylab".into(),
                    root: PathBuf::from("/labs/first"),
                    pid: 42,
                    started: None,
                    state,
                }],
            };

            let error = registry
                .check_name("mylab", std::path::Path::new("/labs/second"))
                .unwrap_err();

            assert_eq!(error.code, ErrorCode::Conflict);
            assert!(error.message.contains("/labs/first"));
            assert!(error.message.contains("run `vmlab down` there"));
            assert!(error.message.contains("vmlab lab stop mylab"));
            assert!(error.message.contains("--instance <id>"));
        }
    }

    const UNPARSED_REGISTRY_HELPER: &str = "VMLAB_TEST_UNPARSED_REGISTRY";

    /// Review F14 (card #574): a `labs.json` that does not parse — cut
    /// short, or written by a newer build — is not an empty registry. Read
    /// as one, every running lab daemon is forgotten and the next `save`
    /// replaces the file.
    #[test]
    fn a_registry_that_does_not_parse_is_not_empty() {
        let scratch = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "supervisor::registry::tests::unparsed_registry_helper",
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(UNPARSED_REGISTRY_HELPER, "1")
            .env("XDG_RUNTIME_DIR", scratch.path().join("run"))
            .env("XDG_STATE_HOME", scratch.path().join("state"))
            .env("XDG_DATA_HOME", scratch.path().join("data"))
            .env("XDG_CONFIG_HOME", scratch.path().join("config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Isolated paths: the state directory is this process's own.
    #[test]
    #[ignore = "subprocess helper for a_registry_that_does_not_parse_is_not_empty"]
    fn unparsed_registry_helper() {
        if std::env::var_os(UNPARSED_REGISTRY_HELPER).is_none() {
            return;
        }
        let path = Registry::path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // No file is a first start, not an error.
        assert!(Registry::load().unwrap().labs().is_empty());

        // What a supervisor wrote comes back.
        let root = canonical_root(path.parent().unwrap()).unwrap();
        let written = Registry {
            labs: vec![LabEntry {
                name: "mylab".into(),
                root: root.clone(),
                pid: 42,
                started: Some(7),
                state: LabState::Stopping,
            }],
        };
        written.save();
        let read = Registry::load().unwrap();
        let entry = read.get("mylab").expect("the entry is read back");
        assert_eq!(entry.root, root);
        assert_eq!(entry.pid, 42);
        assert_eq!(entry.started, Some(7));
        assert_eq!(entry.state, LabState::Stopping);

        for broken in [
            "{ this is not json",
            // Cut short in the middle of a write.
            r#"{ "labs": [ { "name": "mylab", "root": "/labs/my"#,
            // A state this build does not know.
            r#"{ "labs": [ { "name": "mylab", "root": "/labs/mylab", "pid": 42, "state": "paused" } ] }"#,
        ] {
            std::fs::write(&path, broken).unwrap();
            let error = match Registry::load() {
                Ok(registry) => panic!(
                    "{broken:?} was read as a registry of {} lab(s)",
                    registry.labs().len()
                ),
                Err(error) => format!("{error:#}"),
            };
            assert!(error.contains(&path.display().to_string()), "{error}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                broken,
                "the file is left as it was"
            );
        }
    }

    #[test]
    fn a_registered_lab_name_is_available_at_the_same_root() {
        let registry = Registry {
            labs: vec![LabEntry {
                name: "mylab".into(),
                root: PathBuf::from("/labs/mylab"),
                pid: 42,
                started: None,
                state: LabState::Running,
            }],
        };

        registry
            .check_name("mylab", std::path::Path::new("/labs/mylab"))
            .unwrap();
    }

    #[test]
    fn a_daemon_exit_clears_only_its_own_entry() {
        let entry = |pid| LabEntry {
            name: "mylab".into(),
            root: PathBuf::from("/labs/mylab"),
            pid,
            started: None,
            state: LabState::Running,
        };
        let mut registry = Registry {
            labs: vec![entry(84)],
        };
        // The old daemon (42) exiting after its successor (84) registered.
        registry.remove_daemon("mylab", 42);
        assert_eq!(registry.get("mylab").unwrap().pid, 84);
        registry.remove_daemon("mylab", 84);
        assert!(registry.get("mylab").is_none());
    }

    #[test]
    fn upsert_cannot_replace_a_lab_from_another_root() {
        let mut registry = Registry {
            labs: vec![LabEntry {
                name: "mylab".into(),
                root: PathBuf::from("/labs/first"),
                pid: 42,
                started: None,
                state: LabState::Failed,
            }],
        };

        registry
            .upsert(LabEntry {
                name: "mylab".into(),
                root: PathBuf::from("/labs/second"),
                pid: 84,
                started: None,
                state: LabState::Running,
            })
            .unwrap_err();

        let entry = registry.get("mylab").unwrap();
        assert_eq!(entry.root, PathBuf::from("/labs/first"));
        assert_eq!(entry.pid, 42);
        assert_eq!(entry.state, LabState::Failed);
    }

    #[test]
    fn canonical_root_resolves_relative_absolute_and_symlink_paths() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("lab");
        std::fs::create_dir(&root).unwrap();
        let link = temp.path().join("lab-link");
        symlink(&root, &link).unwrap();
        // Reach the system temp directory without writing into the checkout
        // or changing the process-wide cwd while other tests are running.
        let mut relative: PathBuf = cwd.ancestors().skip(1).map(|_| "..").collect();
        relative.push(root.strip_prefix("/").unwrap());
        assert!(relative.is_relative());

        let canonical = canonical_root(&root).unwrap();

        assert_eq!(canonical_root(&relative).unwrap(), canonical);
        assert_eq!(canonical_root(&link).unwrap(), canonical);
    }
}
