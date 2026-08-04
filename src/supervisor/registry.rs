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
    pub state: LabState,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    labs: Vec<LabEntry>,
}

impl Registry {
    fn path() -> PathBuf {
        crate::paths::state_dir().join("labs.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
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
        }
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
        if let Some(entry) = self.get(name)
            && entry.root != requested_root
        {
            return Err(CommandError::conflict(format!(
                "lab `{name}` is already registered from {} — stop the other lab there or rename this lab",
                entry.root.display()
            )));
        }
        Ok(())
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

    pub fn remove(&mut self, name: &str) {
        self.labs.retain(|l| l.name != name);
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
                    state,
                }],
            };

            let error = registry
                .check_name("mylab", std::path::Path::new("/labs/second"))
                .unwrap_err();

            assert_eq!(error.code, ErrorCode::Conflict);
            assert!(error.message.contains("/labs/first"));
            assert!(error.message.contains("stop the other lab"));
            assert!(error.message.contains("rename this lab"));
        }
    }

    #[test]
    fn a_registered_lab_name_is_available_at_the_same_root() {
        let registry = Registry {
            labs: vec![LabEntry {
                name: "mylab".into(),
                root: PathBuf::from("/labs/mylab"),
                pid: 42,
                state: LabState::Running,
            }],
        };

        registry
            .check_name("mylab", std::path::Path::new("/labs/mylab"))
            .unwrap();
    }

    #[test]
    fn upsert_cannot_replace_a_lab_from_another_root() {
        let mut registry = Registry {
            labs: vec![LabEntry {
                name: "mylab".into(),
                root: PathBuf::from("/labs/first"),
                pid: 42,
                state: LabState::Failed,
            }],
        };

        registry
            .upsert(LabEntry {
                name: "mylab".into(),
                root: PathBuf::from("/labs/second"),
                pid: 84,
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
        let temp = tempfile::tempdir_in(&cwd).unwrap();
        let root = temp.path().join("lab");
        std::fs::create_dir(&root).unwrap();
        let link = temp.path().join("lab-link");
        symlink(&root, &link).unwrap();
        let relative = root.strip_prefix(&cwd).unwrap();

        let canonical = canonical_root(&root).unwrap();

        assert_eq!(canonical_root(relative).unwrap(), canonical);
        assert_eq!(canonical_root(&link).unwrap(), canonical);
    }
}
